//! L2 — verify command execution: direct argv spawn inside the sandbox,
//! in-process wall-clock deadline, whole-tree kill (own process group →
//! SIGTERM → grace → SIGKILL), capped tail-keeping output (no pipe deadlock).
//! Mirrors unirun's lifecycle semantics (process group kill, not GNU timeout).

use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::types::VerifyResult;

/// Tail cap per stream (default 64 KiB) — errors cluster at the end.
pub const TAIL_CAP: usize = 64 * 1024;
/// Grace between SIGTERM and SIGKILL when killing the tree.
#[cfg(unix)]
pub const KILL_GRACE: Duration = Duration::from_secs(1);

pub struct ExecSpec {
    pub argv: Vec<String>,
    pub workdir: PathBuf,
    pub env: Vec<(String, String)>,
    pub timeout: Duration,
}

/// Run the verify command. Err = environment-level failure (cannot spawn) →
/// caller exits 2. Everything else is a VerifyResult.
pub fn run(spec: &ExecSpec) -> Result<VerifyResult, String> {
    let start = Instant::now();
    let mut child = match spawn_verify(spec) {
        Ok(c) => c,
        Err(e) => {
            return Err(format!(
                "cannot spawn verify command `{}`: {e} (is it on PATH / synced into the sandbox?)",
                spec.argv.join(" ")
            ))
        }
    };

    let out_t = child
        .stdout
        .take()
        .map(|s| thread::spawn(move || read_tail(s, TAIL_CAP)));
    let err_t = child
        .stderr
        .take()
        .map(|s| thread::spawn(move || read_tail(s, TAIL_CAP)));

    let mut exit_code: Option<i32> = None;
    let mut killed = false;
    let deadline = start + spec.timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                exit_code = status.code();
                break;
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    killed = true;
                    kill_tree(&mut child);
                    let _ = child.wait();
                    break;
                }
                thread::sleep(Duration::from_millis(5));
            }
            Err(_) => {
                let _ = child.wait();
                break;
            }
        }
    }

    let (out, out_trunc) = join_capture(out_t);
    let (err, err_trunc) = join_capture(err_t);

    let stdout = String::from_utf8_lossy(&out).into_owned();
    let stderr = String::from_utf8_lossy(&err).into_owned();
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
        cmd: spec.argv.clone(),
        exit_code,
        duration_ms: start.elapsed().as_millis() as u64,
        output_tail,
        truncated: out_trunc || err_trunc,
        killed,
    })
}

/// Spawn the verify process, applying execvp-style ENOEXEC fallback: a
/// shebang-less script cannot be exec'd directly on Linux (execve → ENOEXEC),
/// while macOS posix_spawn falls back to /bin/sh implicitly. Without this,
/// `sandbox-run -- ./verify.sh` would work on macOS and fail on Linux.
#[cfg(unix)]
fn spawn_verify(spec: &ExecSpec) -> Result<Child, std::io::Error> {
    match spawn_argv(&spec.argv[0], &spec.argv[1..], spec) {
        Ok(c) => Ok(c),
        Err(e) if e.raw_os_error() == Some(libc::ENOEXEC) => {
            // run the script via sh: sh <script> <args...>
            let mut sh_argv = Vec::with_capacity(spec.argv.len() + 1);
            sh_argv.push("sh".to_string());
            sh_argv.extend(spec.argv.iter().cloned());
            spawn_argv(&sh_argv[0], &sh_argv[1..], spec)
        }
        Err(e) => Err(e),
    }
}

#[cfg(windows)]
fn spawn_verify(spec: &ExecSpec) -> Result<Child, std::io::Error> {
    spawn_argv(&spec.argv[0], &spec.argv[1..], spec)
}

/// Build and spawn the process: own process group (whole-tree kill),
/// inherited env + overrides, cwd = sandbox.
fn spawn_argv(program: &str, args: &[String], spec: &ExecSpec) -> Result<Child, std::io::Error> {
    let mut cmd = Command::new(program);
    cmd.args(args);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .current_dir(&spec.workdir);
    for (k, v) in &spec.env {
        cmd.env(k, v);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP);
    }
    cmd.spawn()
}

/// Read a stream to EOF keeping only the tail `max` bytes (drains the rest so
/// the child never blocks on a full pipe).
fn read_tail<R: Read>(mut reader: R, max: usize) -> (Vec<u8>, bool) {
    let mut tail: Vec<u8> = Vec::with_capacity(max.saturating_add(8192));
    let mut total = 0usize;
    let mut chunk = [0u8; 8192];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                total += n;
                tail.extend_from_slice(&chunk[..n]);
                if tail.len() > max {
                    let excess = tail.len() - max;
                    tail.drain(..excess);
                }
            }
            Err(_) => break,
        }
    }
    (tail, total > max)
}

fn join_capture(h: Option<thread::JoinHandle<(Vec<u8>, bool)>>) -> (Vec<u8>, bool) {
    match h {
        Some(j) => j.join().unwrap_or((Vec::new(), false)),
        None => (Vec::new(), false),
    }
}

/// Terminate the whole process tree: SIGTERM to the group, then after the
/// child dies (or grace expires) SIGKILL the group to catch stragglers — a
/// group signal does not hit members spawned after delivery, so a background
/// job raced in right after TERM would otherwise survive (leaked process).
#[cfg(unix)]
fn kill_tree(child: &mut Child) {
    let pid = child.id() as i32;
    unsafe { libc::kill(-pid, libc::SIGTERM) };
    let deadline = Instant::now() + KILL_GRACE;
    loop {
        if let Ok(Some(_)) = child.try_wait() {
            break; // child is gone; the group may still hold stragglers
        }
        if Instant::now() >= deadline {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    unsafe { libc::kill(-pid, libc::SIGKILL) };
    // wait (bounded) for the group to actually empty, so tests/agents never
    // observe a residual process
    let give_up = Instant::now() + Duration::from_millis(500);
    loop {
        // kill(-pid, 0) probes the group: 0 = the group still has members
        if unsafe { libc::kill(-pid, 0) } != 0 {
            break;
        }
        if Instant::now() >= give_up {
            break;
        }
        thread::sleep(Duration::from_millis(5));
    }
}

/// Windows tree termination via `taskkill /T /F` (best-effort; Windows local
/// execution is a P1 acceptance item).
#[cfg(windows)]
fn kill_tree(child: &mut Child) {
    let _ = Command::new("taskkill")
        .args(["/PID", &child.id().to_string(), "/T", "/F"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh_ok(command: &str) -> ExecSpec {
        ExecSpec {
            argv: vec!["sh".into(), "-c".into(), command.into()],
            workdir: std::env::temp_dir(),
            env: vec![],
            timeout: Duration::from_secs(10),
        }
    }

    #[test]
    fn exit_code_propagates() {
        if cfg!(windows) {
            return;
        }
        let ok = run(&sh_ok("exit 0")).unwrap();
        assert_eq!(ok.exit_code, Some(0));
        assert!(!ok.killed);
        let fail = run(&sh_ok("exit 7")).unwrap();
        assert_eq!(fail.exit_code, Some(7));
    }

    #[test]
    fn timeout_kills_whole_tree() {
        if cfg!(windows) {
            return;
        }
        // 91s is a distinctive duration unlikely to collide with unrelated
        // system processes (this machine's LOS wrappers run `sleep 30`).
        let mut spec = sh_ok("sleep 91 & wait");
        spec.timeout = Duration::from_millis(200);
        let start = Instant::now();
        let r = run(&spec).unwrap();
        assert!(r.killed, "expected timeout kill");
        assert_eq!(r.exit_code, None);
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "kill took too long"
        );
        // no residual sleep from this test's tree
        let out = Command::new("sh")
            .arg("-c")
            .arg("pgrep -f '^sleep 91$' || true")
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&out.stdout).trim().is_empty(),
            "residual sleep process found"
        );
    }

    #[test]
    fn output_tail_is_capped() {
        if cfg!(windows) {
            return;
        }
        let spec = sh_ok("head -c 100000 /dev/zero | tr '\\0' 'x'; echo END");
        let r = run(&spec).unwrap();
        assert!(r.truncated);
        assert!(r.output_tail.trim_end().ends_with("END"));
        assert!(r.output_tail.len() <= 2 * TAIL_CAP + 16);
    }

    #[test]
    fn enoexec_fallback_runs_shebangless_script() {
        if cfg!(windows) {
            return;
        }
        let tmp = std::env::temp_dir().join(format!("sr-enoexec-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let script = tmp.join("no-shebang.sh");
        std::fs::write(&script, "echo ran-via-sh; exit 0").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let spec = ExecSpec {
            argv: vec![script.to_string_lossy().into_owned()],
            workdir: tmp.clone(),
            env: vec![],
            timeout: Duration::from_secs(5),
        };
        let r = run(&spec).unwrap();
        assert_eq!(
            r.exit_code,
            Some(0),
            "shebang-less script must run via sh fallback"
        );
        assert!(
            r.output_tail.contains("ran-via-sh"),
            "output: {}",
            r.output_tail
        );
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn spawn_error_is_err() {
        let spec = ExecSpec {
            argv: vec!["definitely-not-a-binary-xyz".into()],
            workdir: std::env::temp_dir(),
            env: vec![],
            timeout: Duration::from_secs(1),
        };
        assert!(run(&spec).is_err());
    }
}
