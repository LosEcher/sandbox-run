//! L2 — Docker backend. The verify command runs inside a container instead of
//! a VCS worktree; the host orchestrates and retrieves the result.
//!
//! Semantics borrowed from grok-bot's local-docker-host-connector (design
//! reference; the code here is a fresh Rust implementation):
//!
//! - **Content-addressed runtime staging**: the sandbox-run binary is cached
//!   at `~/.cache/sandbox-run/runtime/<sha256>/sandbox-run`; existing bytes
//!   are verified against the live binary (tamper / torn-write → rewritten
//!   atomically) and the file is mounted into the container **read-only**.
//! - **Fingerprint upgrade**: a named persistent container carries ownership,
//!   schema-version and host-sha256 labels; when the fingerprint no longer
//!   matches (binary changed / image changed / schema bumped / auth-mount
//!   toggled) the container is `docker rm --force`d and recreated. An
//!   unowned container with the same name is refused.
//! - **Run-in-container + result return**: the host copies the working state
//!   into the container workspace (`/workspace`, writable), the host's copy is
//!   mounted read-only at `/host` (writes are rejected — G0), the verify
//!   command runs via `docker exec`, the container is killed as a whole on
//!   timeout, and the workspace changes are retrieved with `docker cp`.
//!
//! The named container is a design-level persistent resource (like the
//! ledger): it is kept between runs and reused while the fingerprint matches,
//! so docker backend runs are serialized (a second concurrent run fails
//! loudly instead of corrupting the shared workspace).

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::events::{self, Event};
use crate::exec;
use crate::gates;
use crate::sync;
use crate::types::{Scope, Vcs, VerifyResult};

pub const SCHEMA_VERSION: &str = "1";
pub const CONTAINER_NAME: &str = "sandbox-run";
pub const OWNER_LABEL: &str = "com.losecher.sandbox-run=1";
pub const DEFAULT_IMAGE: &str = "ubuntu:24.04";
/// Labels in the fingerprint (mismatch → recreate).
const LABEL_SCHEMA: &str = "com.losecher.sandbox-run.schema-version";
const LABEL_HOST_SHA: &str = "com.losecher.sandbox-run.host-sha256";
const LABEL_MOUNT_AUTH: &str = "com.losecher.sandbox-run.mount-auth";

/// Per-run docker backend configuration (built from the CLI Config).
pub struct DockerOptions {
    pub image: String,
    pub mount_auth: bool,
    pub env: Vec<(String, String)>,
    pub timeout: Duration,
    pub excludes: Vec<String>,
    pub verify_cmd: Vec<String>,
}

/// The sandbox body outcome shared with the orchestrator:
/// (verify, pollution, overlaid, cleaned, clean_detail, backend).
pub type BodyResult =
    Result<(VerifyResult, Vec<String>, usize, bool, String, &'static str), String>;

/// Everything the run body needs; bundled to keep the call short.
pub struct DockerRun<'a> {
    pub opts: &'a DockerOptions,
    pub scope: &'a Scope,
    pub vcs: Vcs,
    pub cwd: &'a Path,
    pub log: Option<&'a Path>,
    pub ledger_rel: &'a Option<String>,
}

/// The live docker sandbox for one run.
pub struct DockerSandbox {
    /// Host workspace dir — the stable bind-mount source for the container's
    /// `/host` (read-only). Its inode must never be replaced (bind mounts bind
    /// the inode, not the path): contents are cleared, never the dir.
    pub workspace: PathBuf,
    pub container: String,
}

#[derive(Debug, Default, Clone)]
struct Inspected {
    exists: bool,
    owned: bool,
    running: bool,
    image: String,
    host_sha: String,
    schema: String,
    mount_auth: String,
}

/// Output of one docker CLI invocation (spawn failure is Err).
struct CmdOut {
    ok: bool,
    stdout: String,
    stderr: String,
}

impl CmdOut {
    fn combined(&self) -> String {
        let mut s = String::new();
        if !self.stdout.is_empty() {
            s.push_str(self.stdout.trim());
        }
        if !self.stderr.is_empty() {
            if !s.is_empty() {
                s.push('\n');
            }
            s.push_str(self.stderr.trim());
        }
        s
    }
}

/// The `~/.cache/sandbox-run` root (runtime staging + workspace).
fn cache_root() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    home.join(".cache").join("sandbox-run")
}

/// Serialize docker runs: the named container + shared workspace are
/// single-instance resources. A stale lock (dead owner pid) is reclaimed; a
/// live lock means another sandbox-run docker run is in progress → fail loudly
/// (the worktree backend remains the concurrent default).
fn acquire_lock() -> Result<(), String> {
    acquire_lock_at(&cache_root())
}

fn acquire_lock_at(root: &Path) -> Result<(), String> {
    std::fs::create_dir_all(root).map_err(|e| format!("cannot create {}: {e}", root.display()))?;
    let path = root.join("docker.lock");
    loop {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut f) => {
                let _ =
                    std::io::Write::write_all(&mut f, std::process::id().to_string().as_bytes());
                return Ok(());
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if lock_owner_alive(&path) {
                    return Err("another sandbox-run --backend docker run is in progress \
                         (docker backend is serialized; use --backend worktree for \
                         concurrent runs)"
                        .to_string());
                }
                // stale lock from a dead run → reclaim
                let _ = std::fs::remove_file(&path);
            }
            Err(e) => return Err(format!("cannot acquire docker run lock: {e}")),
        }
    }
}

/// Probe whether the pid recorded in the lock file is still alive.
#[cfg(unix)]
fn lock_owner_alive(path: &Path) -> bool {
    let pid: i32 = std::fs::read_to_string(path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(-1);
    if pid <= 0 {
        return true; // unreadable/unparseable → assume live (fail-closed)
    }
    unsafe { libc::kill(pid, 0) == 0 }
}

#[cfg(windows)]
fn lock_owner_alive(_path: &Path) -> bool {
    true // no process-alive probe on Windows; a stale lock needs manual removal
}

fn release_lock() {
    let _ = std::fs::remove_file(cache_root().join("docker.lock"));
}

fn runtime_root() -> PathBuf {
    cache_root().join("runtime")
}

/// The shared workspace dir — stable inode (container /host bind source).
pub fn workspace_dir() -> PathBuf {
    cache_root().join("workspace")
}

/// Content-addressed runtime staging. `root` is injected for tests; the
/// production path is `~/.cache/sandbox-run/runtime`.
fn stage_runtime_into(root: &Path) -> Result<(PathBuf, String), String> {
    let self_exe =
        std::env::current_exe().map_err(|e| format!("cannot resolve own binary path: {e}"))?;
    let bytes = std::fs::read(&self_exe)
        .map_err(|e| format!("cannot read own binary {}: {e}", self_exe.display()))?;
    let sha = gates::sha256_hex_bytes(&bytes);
    let dir = root.join(&sha);
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("cannot create runtime cache {}: {e}", dir.display()))?;
    let target = dir.join("sandbox-run");
    match std::fs::read(&target) {
        Ok(existing) if existing == bytes => {}
        _ => {
            // absent, tampered or torn → rewrite atomically
            let tmp = target.with_extension(format!("tmp.{}", std::process::id()));
            std::fs::write(&tmp, &bytes)
                .map_err(|e| format!("cannot write staged runtime {}: {e}", tmp.display()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))
                    .map_err(|e| format!("cannot chmod staged runtime: {e}"))?;
            }
            std::fs::rename(&tmp, &target)
                .map_err(|e| format!("cannot finalize staged runtime {}: {e}", target.display()))?;
        }
    }
    Ok((target, sha))
}

/// Stage the runtime into the user cache dir.
pub fn stage_runtime() -> Result<(PathBuf, String), String> {
    stage_runtime_into(&runtime_root())
}

/// Run a docker CLI command, capturing stdout/stderr concurrently (tail-capped
/// so a chatty `docker logs` never deadlocks the pipe).
fn docker(args: &[&str]) -> Result<CmdOut, String> {
    let mut child = Command::new("docker")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot spawn docker: {e} (is docker on PATH?)"))?;
    let out_t = child
        .stdout
        .take()
        .map(|s| thread::spawn(move || exec::read_tail(s, exec::TAIL_CAP)));
    let err_t = child
        .stderr
        .take()
        .map(|s| thread::spawn(move || exec::read_tail(s, exec::TAIL_CAP)));
    let status = child
        .wait()
        .map_err(|e| format!("docker wait failed: {e}"))?;
    let (out, _) = exec::join_capture(out_t);
    let (err, _) = exec::join_capture(err_t);
    Ok(CmdOut {
        ok: status.success(),
        stdout: String::from_utf8_lossy(&out).into_owned(),
        stderr: String::from_utf8_lossy(&err).into_owned(),
    })
}

fn container_logs() -> String {
    docker(&["logs", "--tail", "80", CONTAINER_NAME])
        .map(|o| o.combined())
        .unwrap_or_default()
}

/// Fail fast when the docker daemon is unreachable (docker backend is explicit
/// opt-in, so an unavailable daemon is an error, not a silent fallback).
fn check_daemon() -> Result<(), String> {
    let out = docker(&["info", "--format", "{{.ServerVersion}}"])?;
    if !out.ok {
        return Err(format!(
            "docker backend selected but docker is unavailable: {} \
             (start Docker, or use --backend worktree)",
            out.combined()
        ));
    }
    Ok(())
}

fn inspect_container() -> Result<Inspected, String> {
    let out = docker(&["inspect", "--format", "{{json .}}", CONTAINER_NAME])?;
    if !out.ok {
        let combined = out.combined().to_lowercase();
        if combined.contains("no such container") || combined.contains("no such object") {
            return Ok(Inspected::default());
        }
        return Err(format!(
            "docker inspect {CONTAINER_NAME} failed: {}",
            out.combined()
        ));
    }
    let v: serde_json::Value = serde_json::from_str(&out.stdout)
        .map_err(|e| format!("docker inspect returned malformed JSON: {e}"))?;
    let labels = &v["Config"]["Labels"];
    let label = |k: &str| labels.get(k).and_then(|x| x.as_str()).unwrap_or("");
    Ok(Inspected {
        exists: true,
        owned: label("com.losecher.sandbox-run") == "1",
        running: v["State"]["Running"].as_bool().unwrap_or(false),
        image: v["Config"]["Image"].as_str().unwrap_or("").to_string(),
        host_sha: label(LABEL_HOST_SHA).to_string(),
        schema: label(LABEL_SCHEMA).to_string(),
        mount_auth: label(LABEL_MOUNT_AUTH).to_string(),
    })
}

/// Fingerprint mismatch → the container must be recreated. Pure so tests can
/// exercise every combination without a daemon.
fn fingerprint_mismatch(i: &Inspected, opts: &DockerOptions, sha: &str) -> bool {
    i.schema != SCHEMA_VERSION
        || i.host_sha != sha
        || i.image != opts.image
        || i.mount_auth != if opts.mount_auth { "1" } else { "0" }
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Create the named container. The host workspace dir (already existing) is
/// mounted read-only at `/host` (G0: container writes to host paths are
/// rejected) and the staged runtime binary is mounted read-only at
/// `/usr/local/bin/sandbox-run`. `--docker-mount-auth` additionally mounts
/// `~/.codex` and `~/.claude` read-only (default: not mounted).
fn create_container(
    opts: &DockerOptions,
    ws: &Path,
    runtime: &Path,
    sha: &str,
) -> Result<(), String> {
    let mut args: Vec<String> = vec![
        "run".into(),
        "-d".into(),
        "--name".into(),
        CONTAINER_NAME.into(),
        "--label".into(),
        OWNER_LABEL.into(),
        "--label".into(),
        format!("{LABEL_SCHEMA}={SCHEMA_VERSION}"),
        "--label".into(),
        format!("{LABEL_HOST_SHA}={sha}"),
        "--label".into(),
        format!(
            "{LABEL_MOUNT_AUTH}={}",
            if opts.mount_auth { "1" } else { "0" }
        ),
        "--mount".into(),
        format!("type=bind,src={},dst=/host,readonly", ws.display()),
        "--mount".into(),
        format!(
            "type=bind,src={},dst=/usr/local/bin/sandbox-run,readonly",
            runtime.display()
        ),
    ];
    if opts.mount_auth {
        if let Some(home) = home_dir() {
            for (src, dst) in [
                (home.join(".codex"), "/root/.codex"),
                (home.join(".claude"), "/root/.claude"),
            ] {
                if src.is_dir() {
                    args.push("--mount".into());
                    args.push(format!(
                        "type=bind,src={},dst={dst},readonly",
                        src.display()
                    ));
                }
            }
        }
    }
    args.push(opts.image.clone());
    args.push("sh".into());
    args.push("-c".into());
    args.push("sleep infinity".into());
    let argv: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let out = docker(&argv)?;
    if !out.ok {
        return Err(format!(
            "docker run {CONTAINER_NAME} failed: {}\n{}",
            out.combined(),
            container_logs()
        ));
    }
    Ok(())
}

/// Ensure the named container exists, is ours and matches the fingerprint.
/// Recreates on mismatch, starts it when stopped, and refuses unowned
/// containers with the same name.
pub fn ensure(opts: &DockerOptions) -> Result<DockerSandbox, String> {
    check_daemon()?;
    let (runtime, sha) = stage_runtime()?;
    let ws = workspace_dir();
    std::fs::create_dir_all(&ws)
        .map_err(|e| format!("cannot create docker workspace {}: {e}", ws.display()))?;

    let inspected = inspect_container()?;
    if inspected.exists {
        if !inspected.owned {
            return Err(format!(
                "refusing to use container {CONTAINER_NAME}: it exists but is not owned by \
                 sandbox-run (missing label `com.losecher.sandbox-run=1`); remove it manually \
                 or rename it"
            ));
        }
        if fingerprint_mismatch(&inspected, opts, &sha) {
            let removed = docker(&["rm", "--force", CONTAINER_NAME])?;
            if !removed.ok {
                return Err(format!(
                    "cannot replace {CONTAINER_NAME} (fingerprint mismatch): {}",
                    removed.combined()
                ));
            }
        } else if !inspected.running {
            let started = docker(&["start", CONTAINER_NAME])?;
            if !started.ok {
                return Err(format!(
                    "docker start {CONTAINER_NAME} failed: {}\n{}",
                    started.combined(),
                    container_logs()
                ));
            }
        }
    }
    if !inspected.exists || fingerprint_mismatch(&inspected, opts, &sha) {
        create_container(opts, &ws, &runtime, &sha)?;
    }
    Ok(DockerSandbox {
        workspace: ws,
        container: CONTAINER_NAME.to_string(),
    })
}

/// Clear the contents of a directory, keeping the dir itself (the workspace
/// dir is a bind-mount source: its inode must stay stable).
pub fn clear_dir_contents(dir: &Path) -> Result<(), String> {
    let rd = std::fs::read_dir(dir).map_err(|e| format!("read_dir {}: {e}", dir.display()))?;
    for ent in rd {
        let ent = ent.map_err(|e| format!("read_dir entry: {e}"))?;
        let p = ent.path();
        let ft = ent
            .file_type()
            .map_err(|e| format!("file_type {}: {e}", p.display()))?;
        if ft.is_dir() {
            std::fs::remove_dir_all(&p)
                .map_err(|e| format!("remove_dir_all {}: {e}", p.display()))?;
        } else {
            std::fs::remove_file(&p).map_err(|e| format!("remove_file {}: {e}", p.display()))?;
        }
    }
    Ok(())
}

/// Reset the container process state: kill it as a whole (also the timeout
/// path). Tolerates "already stopped".
fn reset_container() {
    let _ = docker(&["kill", CONTAINER_NAME]);
}

/// Whole run for the docker backend, mirroring the worktree flow's event
/// sequence (sandbox.setup → verify.start → verify.finish). Returns the
/// verify result, pollution list, overlaid count, cleanup outcome and the
/// backend name.
pub fn run_body(r: &DockerRun<'_>) -> BodyResult {
    acquire_lock()?;
    let sb = ensure(r.opts)?;
    let log = r.log;
    let scope = r.scope;

    // host-side workspace reset + full working-state materialization
    clear_dir_contents(&sb.workspace)?;
    let paths = sync::plan_full(r.vcs, r.cwd)?;
    let overlaid = sync::apply_full(&paths, r.cwd, &sb.workspace, &r.opts.excludes, r.ledger_rel)?;
    events::append(
        log,
        &Event::SandboxSetup {
            backend: "docker",
            base: &scope.base,
            overlaid_files: overlaid,
            started_at: events::now_ms(),
        },
    )
    .map_err(|e| format!("cannot write event log: {e}"))?;
    let baseline = gates::snapshot_tree(&sb.workspace, &r.opts.excludes)?;

    let outcome = (|| -> Result<(VerifyResult, Vec<String>), String> {
        // materialize the host copy into the writable container workspace
        let prep = docker(&[
            "exec",
            CONTAINER_NAME,
            "sh",
            "-c",
            "rm -rf /workspace && mkdir -p /workspace && cp -a /host/. /workspace/",
        ])?;
        if !prep.ok {
            return Err(format!(
                "container workspace prep failed: {}\n{}",
                prep.combined(),
                container_logs()
            ));
        }

        events::append(
            log,
            &Event::VerifyStart {
                cmd: r.opts.verify_cmd.iter().map(|s| s.as_str()).collect(),
                cwd_in_sandbox: "/workspace",
                timeout_ms: r.opts.timeout.as_millis() as u64,
            },
        )
        .map_err(|e| format!("cannot write event log: {e}"))?;
        let verify = run_verify(&sb, r.opts)?;
        events::append(
            log,
            &Event::VerifyFinish {
                exit_code: verify.exit_code,
                duration_ms: verify.duration_ms,
                output_tail: &verify.output_tail,
                truncated: verify.truncated,
                killed: verify.killed,
            },
        )
        .map_err(|e| format!("cannot write event log: {e}"))?;

        // retrieve the workspace changes (clear first so deletions propagate)
        clear_dir_contents(&sb.workspace)?;
        let cp = docker(&[
            "cp",
            &format!("{}:/workspace/.", sb.container),
            &sb.workspace.to_string_lossy(),
        ])?;
        if !cp.ok {
            return Err(format!(
                "cannot retrieve workspace changes (docker cp): {}",
                cp.combined()
            ));
        }
        let pollution = gates::tree_pollution(&sb.workspace, &r.opts.excludes, &baseline)?;
        Ok((verify, pollution))
    })();

    // whole-container process reset on every path (idempotent with the timeout
    // kill) — a fresh PID space per run, no background stragglers leak through
    reset_container();
    let (cleaned, clean_detail) = cleanup(&sb);

    match outcome {
        Ok((verify, pollution)) => {
            Ok((verify, pollution, overlaid, cleaned, clean_detail, "docker"))
        }
        Err(e) => Err(e),
    }
}

/// Run the verify command inside the container via `docker exec` with an
/// in-process deadline. On timeout the whole container is killed (the tree is
/// the container's PID namespace — killing it kills everything in it). Exit
/// codes 125/126/127 (docker CLI daemon errors / sh exec failures, e.g. the
/// toolchain missing from the image) are environment errors → Err (exit 2).
fn run_verify(sb: &DockerSandbox, opts: &DockerOptions) -> Result<VerifyResult, String> {
    let start = Instant::now();
    let mut args: Vec<String> = vec!["exec".into(), "-w".into(), "/workspace".into()];
    for (k, v) in &opts.env {
        args.push("-e".into());
        args.push(format!("{k}={v}"));
    }
    args.push(sb.container.clone());
    // sh wrapper: `exec "$@"` gives execvp-style ENOEXEC fallback for
    // shebang-less scripts and propagates the command's exit code
    args.push("sh".into());
    args.push("-c".into());
    args.push("exec \"$@\"".into());
    args.push("sandbox-run-sh".into());
    args.extend(opts.verify_cmd.iter().cloned());
    let argv: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

    let mut child: Child = Command::new("docker")
        .args(&argv)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot spawn docker exec: {e} (is docker on PATH?)"))?;

    let out_t = child
        .stdout
        .take()
        .map(|s| thread::spawn(move || exec::read_tail(s, exec::TAIL_CAP)));
    let err_t = child
        .stderr
        .take()
        .map(|s| thread::spawn(move || exec::read_tail(s, exec::TAIL_CAP)));

    let mut exit_code: Option<i32> = None;
    let mut killed = false;
    let deadline = start + opts.timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                exit_code = status.code();
                break;
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    killed = true;
                    reset_container(); // whole-tree kill
                    let _ = child.wait();
                    break;
                }
                thread::sleep(Duration::from_millis(10));
            }
            Err(_) => {
                let _ = child.wait();
                break;
            }
        }
    }

    let (out, out_trunc) = exec::join_capture(out_t);
    let (err, err_trunc) = exec::join_capture(err_t);
    let stdout = String::from_utf8_lossy(&out).into_owned();
    let stderr = String::from_utf8_lossy(&err).into_owned();

    if !killed {
        if let Some(code) = exit_code {
            if code == 125 || code == 126 || code == 127 {
                return Err(format!(
                    "docker exec failed (exit {code}): {}\n\
                     (hint: is the verify toolchain installed in the image `{}`? use --docker-image)",
                    stderr.trim(),
                    opts.image
                ));
            }
        }
    }

    let mut output_tail = String::new();
    if !stdout.is_empty() {
        output_tail.push_str(&stdout);
    }
    if !stderr.is_empty() {
        if !output_tail.is_empty() {
            output_tail.push('\n');
        }
        output_tail.push_str(&stderr);
    }

    Ok(VerifyResult {
        cmd: opts.verify_cmd.clone(),
        exit_code,
        duration_ms: start.elapsed().as_millis() as u64,
        output_tail,
        truncated: out_trunc || err_trunc,
        killed,
    })
}

/// Host-side cleanup: clear the shared workspace contents (the container is a
/// design-level persistent resource and stays, stopped, for the next run) and
/// release the run lock.
pub fn cleanup(sb: &DockerSandbox) -> (bool, String) {
    release_lock();
    match clear_dir_contents(&sb.workspace) {
        Ok(()) => (
            true,
            "docker workspace cleared (container kept for reuse)".to_string(),
        ),
        Err(e) => (false, format!("docker workspace cleanup failed: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> DockerOptions {
        DockerOptions {
            image: DEFAULT_IMAGE.to_string(),
            mount_auth: false,
            env: vec![],
            timeout: Duration::from_secs(10),
            excludes: vec![],
            verify_cmd: vec!["true".to_string()],
        }
    }

    #[test]
    fn stage_runtime_content_addressed_and_tamper_resistant() {
        let tmp = std::env::temp_dir().join(format!("sr-dkr-stage-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let root = tmp.join("runtime");
        let (path, sha) = stage_runtime_into(&root).unwrap();
        assert_eq!(
            path.parent().unwrap().file_name().unwrap().to_str(),
            Some(sha.as_str())
        );
        assert_eq!(gates::sha256_hex_bytes(&std::fs::read(&path).unwrap()), sha);
        // staging again → same path, bytes untouched
        let (path2, sha2) = stage_runtime_into(&root).unwrap();
        assert_eq!(path, path2);
        assert_eq!(sha, sha2);
        // tamper → re-stage restores the original bytes
        std::fs::write(&path, b"evil").unwrap();
        let (path3, _) = stage_runtime_into(&root).unwrap();
        assert_eq!(
            gates::sha256_hex_bytes(&std::fs::read(&path3).unwrap()),
            sha
        );
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn clear_dir_contents_keeps_dir() {
        let tmp = std::env::temp_dir().join(format!("sr-dkr-clear-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("sub/deep")).unwrap();
        std::fs::write(tmp.join("a.txt"), "a").unwrap();
        std::fs::write(tmp.join(".hidden"), "h").unwrap();
        std::fs::write(tmp.join("sub/deep/b.txt"), "b").unwrap();
        clear_dir_contents(&tmp).unwrap();
        assert!(tmp.exists());
        assert_eq!(std::fs::read_dir(&tmp).unwrap().count(), 0);
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn lock_serializes_and_reclaims_stale() {
        let tmp = std::env::temp_dir().join(format!("sr-dkr-lock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        acquire_lock_at(&tmp).unwrap();
        // own pid is alive → a second acquire fails loudly
        let err = acquire_lock_at(&tmp).unwrap_err();
        assert!(err.contains("in progress"), "err: {err}");
        // A stale lock (dead pid) is reclaimed only where a liveness probe exists.
        std::fs::write(tmp.join("docker.lock"), "99999999").unwrap();
        #[cfg(not(windows))]
        acquire_lock_at(&tmp).unwrap();
        // Windows has no process-alive probe (`lock_owner_alive` is fail-closed there), so
        // a stale lock reads as live and needs manual removal. Assert *that* contract on
        // Windows instead: asserting the unix one makes this test impossible to pass there.
        #[cfg(windows)]
        {
            let err = acquire_lock_at(&tmp).unwrap_err();
            assert!(err.contains("in progress"), "err: {err}");
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn fingerprint_mismatch_matrix() {
        let o = opts();
        let sha = "abcd1234";
        let good = Inspected {
            exists: true,
            owned: true,
            running: true,
            image: DEFAULT_IMAGE.to_string(),
            host_sha: sha.to_string(),
            schema: SCHEMA_VERSION.to_string(),
            mount_auth: "0".to_string(),
        };
        assert!(!fingerprint_mismatch(&good, &o, sha));
        assert!(fingerprint_mismatch(
            &Inspected {
                schema: "0".into(),
                ..good.clone()
            },
            &o,
            sha
        ));
        assert!(fingerprint_mismatch(
            &Inspected {
                host_sha: "deadbeef".into(),
                ..good.clone()
            },
            &o,
            sha
        ));
        assert!(fingerprint_mismatch(
            &Inspected {
                image: "node:22".into(),
                ..good.clone()
            },
            &o,
            sha
        ));
        let auth = DockerOptions {
            mount_auth: true,
            ..o
        };
        assert!(fingerprint_mismatch(&good, &auth, sha));
        assert!(!fingerprint_mismatch(
            &Inspected {
                mount_auth: "1".into(),
                ..good.clone()
            },
            &auth,
            sha
        ));
    }
}
