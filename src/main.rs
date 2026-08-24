//! sandbox-run — isolated verification execution for AI agents.
//!
//! Runs a verify command (build/test/lint/typecheck) inside a git worktree or
//! jj workspace whose content mirrors the main working state (changed files
//! overlaid by manifest; cache dirs excluded for clean-verify), so the main
//! tree is never polluted and a pass proves a clean tree passes.
//!
//! Exit codes: 0 pass · 1 fail/timeout/polluted/rejected · 2 error (usage,
//! VCS, sandbox setup, G0 isolation violation).

mod docker;
mod events;
mod exec;
mod gates;
mod report;
mod sandbox;
mod scope;
mod sync;
mod types;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use crate::events::Event;
use crate::types::{ChangeSet, SandboxInfo, Scope, Vcs, Verdict, VerifyResult};

const USAGE: &str = "\
sandbox-run — isolated verification execution for AI agents.

Runs the verify command in a sandbox that mirrors the current working state
(changed files overlaid onto a git worktree / jj workspace, cache dirs
excluded), so the main working tree is never polluted and a pass proves a
clean tree passes. Event-sourced ledger: .sandbox-run/runs.jsonl.

USAGE:
  sandbox-run [OPTIONS] -- <verify-command...>

SCOPE (choose one; default: auto-detect git or jj):
  --scope-from-git            force git scope detection
  --scope-from-jj             force jj scope detection
  --changeset <file.json>     explicit ChangeSet {\"base_ref\": \"HEAD\", \"files\": [...]}
  --base <ref>                git base for detection + worktree (default HEAD; jj: not supported in P0)

OPTIONS:
  --timeout <secs>            verify timeout, then whole-tree kill (default 120)
  --env <K=V>                 extra env for the verify command (repeatable)
  --pollution <deny|warn>     G1 policy when the verify command modifies the
                              sandbox (default deny)
  --exclude <glob,...>        extra exclusion globs (defaults: target/**,
                              node_modules/**, dist/**, .venv/**, __pycache__/**,
                              .pytest_cache/**)
  --backend <auto|worktree|docker>
                              sandbox backend (default auto → worktree).
                              docker runs the verify inside a container and
                              requires a working docker daemon
  --docker-image <name>       image for --backend docker (default ubuntu:24.04;
                              pick one with your toolchain, e.g. node:22)
  --docker-mount-auth         read-only mount ~/.codex and ~/.claude into the
                              container (default: not mounted)
  --emit json                 machine output (default json)
  --log <path>                event log (default .sandbox-run/runs.jsonl)
  --no-log                    disable the event log
  --version                   print version
  --help                      print this help

SUBCOMMANDS:
  status [--log <path>] [--limit N]
                        list runs from the ledger; dangling runs (start without
                        a report) are shown as interrupted
  log <runId> [--log <path>]
                        full event chain of one run + rebuilt summary

EXIT CODES: 0 pass · 1 fail/timeout/polluted/rejected · 2 error.

CHANGESET (explicit scope; caller decides, detection skipped):
  { \"base_ref\": \"HEAD\", \"files\": [\"src/lib.rs\", \"test/\"] }

DOCKER BACKEND (--backend docker):
  The verify command runs inside a container; the host orchestrates and
  retrieves the result. The sandbox-run binary is staged content-addressed
  (~/.cache/sandbox-run/runtime/<sha256>/, read-only mounted) and the host
  working state is copied into the container workspace. The container is a
  named, labeled resource (com.losecher.sandbox-run=1): an unowned container
  with the same name is refused, and a fingerprint mismatch (binary/image/
  schema/mount-auth change) recreates it via `docker rm --force`. The host
  copy is mounted read-only at /host inside the container — container writes
  to host paths are rejected (G0). Timeout kills the whole container.
";

/// Sandbox backend selection. `auto` resolves to the VCS-native backend
/// (git worktree / jj workspace); `docker` requires a working docker daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Backend {
    Auto,
    Worktree,
    Docker,
}

impl Backend {
    fn as_str(&self) -> &'static str {
        match self {
            Backend::Auto => "auto",
            Backend::Worktree => "worktree",
            Backend::Docker => "docker",
        }
    }

    /// Resolve `auto` → the default (worktree). Docker stays explicit.
    fn effective(self) -> Backend {
        match self {
            Backend::Auto => Backend::Worktree,
            b => b,
        }
    }
}

#[derive(Debug)]
struct Config {
    vcs: Option<Vcs>,
    base: String,
    base_explicit: bool,
    changeset: Option<PathBuf>,
    timeout_secs: u64,
    env: Vec<(String, String)>,
    pollution: String,
    excludes: Vec<String>,
    log: Option<PathBuf>,
    verify_cmd: Vec<String>,
    backend: Backend,
    docker_image: String,
    docker_mount_auth: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            vcs: None,
            base: "HEAD".to_string(),
            base_explicit: false,
            changeset: None,
            timeout_secs: 120,
            env: Vec::new(),
            pollution: "deny".to_string(),
            excludes: scope::DEFAULT_EXCLUDES
                .iter()
                .map(|s| s.to_string())
                .collect(),
            log: Some(PathBuf::from(".sandbox-run/runs.jsonl")),
            verify_cmd: Vec::new(),
            backend: Backend::Auto,
            docker_image: docker::DEFAULT_IMAGE.to_string(),
            docker_mount_auth: false,
        }
    }
}

fn take_value<'a>(
    it: &mut std::iter::Peekable<std::slice::Iter<'a, String>>,
    flag: &str,
) -> Result<String, String> {
    it.next()
        .cloned()
        .ok_or_else(|| format!("missing value for {flag}"))
}

fn parse_u64(v: &str, flag: &str) -> Result<u64, String> {
    v.parse::<u64>()
        .map_err(|_| format!("invalid number for {flag}: {v}"))
}

fn parse_args(args: &[String]) -> Result<Config, String> {
    let mut cfg = Config::default();
    let mut it = args.iter().peekable();
    let mut in_cmd = false;
    while let Some(arg) = it.next() {
        if in_cmd {
            cfg.verify_cmd.push(arg.clone());
            continue;
        }
        match arg.as_str() {
            "--" => in_cmd = true,
            "--help" | "-h" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            "--version" | "-V" => {
                println!("sandbox-run {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "--scope-from-git" => cfg.vcs = Some(Vcs::Git),
            "--scope-from-jj" => cfg.vcs = Some(Vcs::Jj),
            "--base" => {
                cfg.base = take_value(&mut it, "--base")?;
                cfg.base_explicit = true;
            }
            "--changeset" => {
                cfg.changeset = Some(PathBuf::from(take_value(&mut it, "--changeset")?))
            }
            "--timeout" => {
                cfg.timeout_secs = parse_u64(&take_value(&mut it, "--timeout")?, "--timeout")?
            }
            "--env" => {
                let kv = take_value(&mut it, "--env")?;
                let (k, v) = kv
                    .split_once('=')
                    .ok_or_else(|| format!("--env expects K=V, got: {kv}"))?;
                if k.is_empty() {
                    return Err(format!("--env key is empty: {kv}"));
                }
                cfg.env.push((k.to_string(), v.to_string()));
            }
            "--pollution" => {
                let v = take_value(&mut it, "--pollution")?;
                if v != "deny" && v != "warn" {
                    return Err(format!("--pollution expects deny|warn, got: {v}"));
                }
                cfg.pollution = v;
            }
            "--backend" => {
                let v = take_value(&mut it, "--backend")?;
                cfg.backend = match v.as_str() {
                    "auto" => Backend::Auto,
                    "worktree" => Backend::Worktree,
                    "docker" => Backend::Docker,
                    other => {
                        return Err(format!(
                            "--backend expects auto|worktree|docker, got: {other}"
                        ))
                    }
                };
            }
            "--docker-image" => {
                cfg.docker_image = take_value(&mut it, "--docker-image")?;
            }
            "--docker-mount-auth" => cfg.docker_mount_auth = true,
            "--exclude" => {
                let v = take_value(&mut it, "--exclude")?;
                cfg.excludes.extend(
                    v.split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty()),
                );
            }
            "--emit" => {
                let v = take_value(&mut it, "--emit")?;
                if v != "json" {
                    return Err(format!("unknown --emit value: {v} (json)"));
                }
            }
            "--log" => cfg.log = Some(PathBuf::from(take_value(&mut it, "--log")?)),
            "--no-log" => cfg.log = None,
            other if other.starts_with('-') => {
                return Err(format!("unknown argument: {other} (see --help)"));
            }
            other => {
                in_cmd = true;
                cfg.verify_cmd.push(other.to_string());
            }
        }
    }
    if cfg.verify_cmd.is_empty() {
        return Err(
            "missing verify command (use: sandbox-run [OPTIONS] -- <verify-command...>)"
                .to_string(),
        );
    }
    if cfg.backend != Backend::Docker
        && (cfg.docker_mount_auth || cfg.docker_image != docker::DEFAULT_IMAGE)
    {
        return Err("--docker-image / --docker-mount-auth require --backend docker".to_string());
    }
    Ok(cfg)
}

/// The event-log path relative to cwd (when inside the repo), for the G0
/// ledger exclusion and the sync manifest filter. A relative --log path is
/// already relative to cwd (no canonicalization, which would break under
/// symlinked cwd like /var → /private/var on macOS); an absolute path is
/// stripped against the canonicalized cwd when possible.
fn ledger_relative(cwd: &Path, log: Option<&Path>) -> Option<String> {
    let log = log?;
    if !log.is_absolute() {
        return Some(normalize_rel(&log.to_string_lossy()));
    }
    let cwd_abs = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let log_abs = log.canonicalize().unwrap_or_else(|_| log.to_path_buf());
    log_abs
        .strip_prefix(&cwd_abs)
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
}

/// Lexically clean a relative path ("a/../b", "./x" → "x").
fn normalize_rel(p: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for seg in p.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    parts.join("/")
}

fn config_hash(cfg: &Config, scope: &Scope) -> String {
    let j = serde_json::json!({
        "vcs": scope.vcs.map(|v| v.as_str()),
        "base": scope.base,
        "timeout_secs": cfg.timeout_secs,
        "pollution": cfg.pollution,
        "excludes": cfg.excludes,
        "log": cfg.log.as_ref().map(|p| p.display().to_string()),
        "cmd": cfg.verify_cmd,
        "env_keys": cfg.env.iter().map(|(k, _)| k).collect::<Vec<_>>(),
        "backend": cfg.backend.effective().as_str(),
        "docker_image": cfg.docker_image,
        "docker_mount_auth": cfg.docker_mount_auth,
    });
    gates::sha256_hex(&j.to_string())
}

fn main_state(vcs: Vcs, cwd: &Path, ledger_rel: &Option<String>) -> Result<String, String> {
    match vcs {
        Vcs::Git => gates::git_main_state(cwd, ledger_rel),
        Vcs::Jj => gates::jj_main_state(cwd, ledger_rel),
    }
}

fn run(cfg: &Config, cwd: &Path) -> Result<i32, String> {
    let run_id = events::new_run_id();
    let log = cfg.log.as_deref();
    let ledger_rel = ledger_relative(cwd, cfg.log.as_deref());
    let log_path_str = cfg.log.as_ref().map(|p| p.display().to_string());
    let ts = events::now_ms();

    // ---- L1: scope -----------------------------------------------------
    let scope: Scope = if let Some(cs_path) = &cfg.changeset {
        let text = std::fs::read_to_string(cs_path)
            .map_err(|e| format!("cannot read changeset {}: {e}", cs_path.display()))?;
        let cs: ChangeSet =
            serde_json::from_str(&text).map_err(|e| format!("invalid changeset: {e}"))?;
        scope::scope_from_changeset(cwd, &cs, &cfg.excludes, &ledger_rel)
            .map_err(|e| e.to_string())?
    } else {
        let vcs = cfg.vcs.or_else(|| scope::detect_vcs(cwd));
        let v = vcs.ok_or_else(|| scope::ScopeError::NotARepository.to_string())?;
        if cfg.base_explicit && v == Vcs::Jj {
            return Err(
                "--base is not supported for jj in P0 (jj scope is the working copy vs its parent)"
                    .to_string(),
            );
        }
        scope::detect_scope(cwd, Some(v), &cfg.base, &cfg.excludes, &ledger_rel)
            .map_err(|e| e.to_string())?
    };
    let vcs = scope.vcs.ok_or_else(|| "scope has no VCS".to_string())?;

    events::append(
        log,
        &Event::RunStart {
            run_id: run_id.clone(),
            version: env!("CARGO_PKG_VERSION"),
            ts,
            cwd: &cwd.display().to_string(),
            vcs: Some(vcs.as_str()),
            base_ref: &scope.base,
            cmd: cfg.verify_cmd.iter().map(|s| s.as_str()).collect(),
            env_keys: cfg.env.iter().map(|(k, _)| k.as_str()).collect(),
            scope_files: scope.files.clone(),
            scope_source: &scope.source,
            config_hash: &config_hash(cfg, &scope),
        },
    )
    .map_err(|e| format!("cannot write event log: {e}"))?;
    events::append(
        log,
        &Event::ScopeDetect {
            source: &scope.source,
            files: scope.files.clone(),
            excluded: scope.excluded.clone(),
        },
    )
    .map_err(|e| format!("cannot write event log: {e}"))?;

    // Empty scope: nothing to verify → rejected (fail-closed; the change set
    // the agent asked to verify is empty).
    if scope.files.is_empty() {
        let report = report::build_report(
            run_id,
            "rejected",
            &scope,
            SandboxInfo {
                backend: "none".to_string(),
                base: scope.base.clone(),
                overlaid_files: 0,
                cleaned: true,
                detail: Some("no changes detected; nothing to verify".to_string()),
            },
            &VerifyResult::empty(),
            &[],
            log_path_str.clone(),
        );
        events::append(
            log,
            &Event::ReportEmit {
                verdict: "rejected",
                duration_ms: 0,
                output_path: log_path_str.as_deref(),
            },
        )
        .map_err(|e| format!("cannot write event log: {e}"))?;
        emit_report(&report);
        eprintln!("sandbox-run: rejected — no changes detected to verify (scope is empty)");
        return Ok(1);
    }

    // ---- G0 before (isolation baseline) --------------------------------
    let g0_before = main_state(vcs, cwd, &ledger_rel)?;

    // ---- L2: sandbox + verify + pollution --------------------------------
    let body: docker::BodyResult = match cfg.backend.effective() {
        Backend::Docker => {
            let run = docker::DockerRun {
                opts: &docker::DockerOptions {
                    image: cfg.docker_image.clone(),
                    mount_auth: cfg.docker_mount_auth,
                    env: cfg.env.clone(),
                    timeout: Duration::from_secs(cfg.timeout_secs),
                    excludes: cfg.excludes.clone(),
                    verify_cmd: cfg.verify_cmd.clone(),
                },
                scope: &scope,
                vcs,
                cwd,
                log,
                ledger_rel: &ledger_rel,
            };
            docker::run_body(&run)
        }
        Backend::Worktree => run_vcs_body(cfg, &scope, vcs, cwd, log, &run_id),
        Backend::Auto => unreachable!("auto resolved before dispatch"),
    };

    let (verify, pollution, overlaid, cleaned, clean_detail, backend) = match body {
        Ok(b) => b,
        Err(e) => {
            eprintln!("sandbox-run: error: {e}");
            return Ok(2);
        }
    };

    // ---- L3: gates ------------------------------------------------------
    let g1 = gates::g1_check(&pollution, &cfg.pollution);
    let verdict = gates::classify(&verify, pollution.len(), &cfg.pollution);
    let g0_after = main_state(vcs, cwd, &ledger_rel)?;
    let g0 = gates::g0_check(&g0_before, &g0_after, cleaned, &clean_detail);

    for g in [&g0, &g1] {
        events::append(
            log,
            &Event::GateCheck {
                gate: &g.gate,
                pass: g.pass,
                metric: g.metric,
                limit: g.limit,
                actual: g.actual,
                detail: &g.detail,
            },
        )
        .map_err(|e| format!("cannot write event log: {e}"))?;
    }

    let mut verify_with_cmd = verify.clone();
    verify_with_cmd.cmd = cfg.verify_cmd.clone();

    let sandbox_info = SandboxInfo {
        backend: backend.to_string(),
        base: scope.base.clone(),
        overlaid_files: overlaid,
        cleaned,
        detail: Some(clean_detail.clone()),
    };
    let report = report::build_report(
        run_id.clone(),
        verdict.as_str(),
        &scope,
        sandbox_info,
        &verify_with_cmd,
        &[g0.clone(), g1.clone()],
        log_path_str.clone(),
    );
    events::append(
        log,
        &Event::ReportEmit {
            verdict: verdict.as_str(),
            duration_ms: verify.duration_ms,
            output_path: log_path_str.as_deref(),
        },
    )
    .map_err(|e| format!("cannot write event log: {e}"))?;

    emit_report(&report);
    emit_human_summary(&report, &g0, &g1);

    // G0 is a hard gate: a violation is a bug → exit 2 regardless of verdict.
    if !g0.pass {
        return Ok(2);
    }
    Ok(match verdict {
        Verdict::Pass => 0,
        _ => 1,
    })
}

/// VCS-native body (git worktree / jj workspace): setup → overlay → baseline
/// → verify → pollution, with cleanup on every exit path.
fn run_vcs_body(
    cfg: &Config,
    scope: &Scope,
    vcs: Vcs,
    cwd: &Path,
    log: Option<&Path>,
    run_id: &str,
) -> docker::BodyResult {
    let sb = sandbox::setup(vcs, &scope.base, run_id, cwd)?;
    let backend = sb.backend;

    let outcome = (|| -> Result<(VerifyResult, Vec<String>, usize), String> {
        // overlay the working state into the sandbox
        let entries = sync::plan(cwd, &scope.files);
        let overlaid = sync::apply(&entries, cwd, &sb.dir)?;
        events::append(
            log,
            &Event::SandboxSetup {
                backend: sb.backend,
                base: &scope.base,
                overlaid_files: overlaid,
                started_at: events::now_ms(),
            },
        )
        .map_err(|e| format!("cannot write event log: {e}"))?;
        sandbox::establish_baseline(&sb)?;

        // jj: hash-snapshot the materialized tree BEFORE verify (no jj command
        // between baseline and verify, so the wc change stays clean)
        let jj_baseline = if vcs == Vcs::Jj {
            Some(gates::snapshot_tree(&sb.dir, &cfg.excludes)?)
        } else {
            None
        };

        // verify command
        events::append(
            log,
            &Event::VerifyStart {
                cmd: cfg.verify_cmd.iter().map(|s| s.as_str()).collect(),
                cwd_in_sandbox: ".",
                timeout_ms: cfg.timeout_secs * 1000,
            },
        )
        .map_err(|e| format!("cannot write event log: {e}"))?;
        let spec = exec::ExecSpec {
            argv: cfg.verify_cmd.clone(),
            workdir: sb.dir.clone(),
            env: cfg.env.clone(),
            timeout: Duration::from_secs(cfg.timeout_secs),
        };
        let verify = exec::run(&spec)?;
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

        // G1 pollution
        let pollution = match vcs {
            Vcs::Git => gates::git_pollution(&sb.dir, &cfg.excludes)?,
            Vcs::Jj => gates::jj_pollution(
                &sb.dir,
                &cfg.excludes,
                jj_baseline.as_ref().expect("jj baseline present"),
            )?,
        };

        Ok((verify, pollution, overlaid))
    })();

    // cleanup on every exit path (residual sandbox is a bug, G0)
    let (cleaned, clean_detail) = sandbox::cleanup(&sb, cwd);

    match outcome {
        Ok((verify, pollution, overlaid)) => {
            Ok((verify, pollution, overlaid, cleaned, clean_detail, backend))
        }
        Err(e) => Err(e),
    }
}

fn emit_report(report: &types::RunReport) {
    let json = serde_json::to_string_pretty(report).expect("report serializes");
    println!("{json}");
}

fn emit_human_summary(report: &types::RunReport, g0: &types::GateResult, g1: &types::GateResult) {
    eprintln!(
        "sandbox-run {}: {} — exit {:?}, {}ms, {} file(s) overlaid, sandbox {}",
        env!("CARGO_PKG_VERSION"),
        report.verdict,
        report.verify.exit_code,
        report.verify.duration_ms,
        report.sandbox.overlaid_files,
        if g0.pass && g1.pass { "clean" } else { "DIRTY" }
    );
    if !g1.pass {
        eprintln!("sandbox-run: G1 sandbox.clean: {}", g1.detail);
    }
    if !g0.pass {
        eprintln!("sandbox-run: G0 isolation.integrity: {}", g0.detail);
    }
}

// ---- subcommands ---------------------------------------------------------

fn run_status(args: &[String]) -> Result<i32, String> {
    let mut log = PathBuf::from(".sandbox-run/runs.jsonl");
    let mut limit: Option<usize> = None;
    let mut it = args.iter().peekable();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--log" => log = PathBuf::from(take_value(&mut it, "--log")?),
            "--limit" => {
                limit = Some(parse_u64(&take_value(&mut it, "--limit")?, "--limit")? as usize)
            }
            s if s.starts_with("--") => return Err(format!("unknown status option: {s}")),
            s => return Err(format!("unexpected status argument: {s}")),
        }
    }
    let runs = match report::read_runs(&log) {
        Ok(r) => r,
        Err(report::LedgerError::LogMissing(_)) => {
            println!("no runs recorded (log: {})", log.display());
            return Ok(0);
        }
        Err(e) => return Err(e.to_string()),
    };
    let view: &[report::RunSummary] = match limit {
        Some(n) if n < runs.len() => &runs[runs.len() - n..],
        _ => &runs[..],
    };
    print!("{}", report::render_status(view));
    Ok(0)
}

fn run_log(args: &[String]) -> Result<i32, String> {
    let mut log = PathBuf::from(".sandbox-run/runs.jsonl");
    let mut run_id: Option<String> = None;
    let mut it = args.iter().peekable();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--log" => log = PathBuf::from(take_value(&mut it, "--log")?),
            s if s.starts_with("--") => return Err(format!("unknown log option: {s}")),
            s => {
                if run_id.is_some() {
                    return Err(format!("unexpected extra argument: {s}"));
                }
                run_id = Some(s.to_string());
            }
        }
    }
    let run_id =
        run_id.ok_or_else(|| "missing <runId> (find ids via `sandbox-run status`)".to_string())?;
    let runs = report::read_runs(&log).map_err(|e| e.to_string())?;
    let run = report::find_run(&runs, &run_id)
        .ok_or_else(|| format!("no run with id {run_id} in the log"))?;
    print!("{}", report::render_log(run));
    Ok(0)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.first().map(String::as_str) == Some("status") {
        return match run_status(&args[1..]) {
            Ok(code) => ExitCode::from(code as u8),
            Err(e) => {
                eprintln!("sandbox-run: status: {e}");
                ExitCode::from(2)
            }
        };
    }
    if args.first().map(String::as_str) == Some("log") {
        return match run_log(&args[1..]) {
            Ok(code) => ExitCode::from(code as u8),
            Err(e) => {
                eprintln!("sandbox-run: log: {e}");
                ExitCode::from(2)
            }
        };
    }

    let cfg = match parse_args(&args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("sandbox-run: {e}");
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    let cwd = std::env::current_dir().expect("current dir");
    match run(&cfg, &cwd) {
        Ok(code) => ExitCode::from(code as u8),
        Err(e) => {
            eprintln!("sandbox-run: error: {e}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_args_verify_cmd_after_dashdash() {
        let cfg = parse_args(&[
            "--scope-from-git".to_string(),
            "--timeout".to_string(),
            "30".to_string(),
            "--".to_string(),
            "cargo".to_string(),
            "test".to_string(),
            "--no-run".to_string(),
        ])
        .unwrap();
        assert_eq!(cfg.vcs, Some(Vcs::Git));
        assert_eq!(cfg.timeout_secs, 30);
        assert_eq!(cfg.verify_cmd, vec!["cargo", "test", "--no-run"]);
    }

    #[test]
    fn parse_args_positional_cmd() {
        let cfg = parse_args(&[
            "cargo".to_string(),
            "test".to_string(),
            "--no-run".to_string(),
        ])
        .unwrap();
        assert_eq!(cfg.verify_cmd, vec!["cargo", "test", "--no-run"]);
    }

    #[test]
    fn parse_args_env_and_pollution() {
        let cfg = parse_args(&[
            "--env".to_string(),
            "RUST_BACKTRACE=1".to_string(),
            "--pollution".to_string(),
            "warn".to_string(),
            "--".to_string(),
            "sh".to_string(),
            "-c".to_string(),
            "true".to_string(),
        ])
        .unwrap();
        assert_eq!(
            cfg.env,
            vec![("RUST_BACKTRACE".to_string(), "1".to_string())]
        );
        assert_eq!(cfg.pollution, "warn");
    }

    #[test]
    fn parse_args_rejects() {
        assert!(parse_args(&[]).is_err());
        assert!(parse_args(&["--nope".to_string(), "x".to_string()]).is_err());
        assert!(parse_args(&[
            "--pollution".to_string(),
            "maybe".to_string(),
            "--".to_string(),
            "x".to_string()
        ])
        .is_err());
        assert!(parse_args(&[
            "--env".to_string(),
            "NOEQ".to_string(),
            "--".to_string(),
            "x".to_string()
        ])
        .is_err());
        assert!(parse_args(&[
            "--emit".to_string(),
            "yaml".to_string(),
            "--".to_string(),
            "x".to_string()
        ])
        .is_err());
    }

    #[test]
    fn parse_args_backend_variants() {
        let cfg = parse_args(&[
            "--backend".to_string(),
            "docker".to_string(),
            "--docker-image".to_string(),
            "node:22".to_string(),
            "--docker-mount-auth".to_string(),
            "--".to_string(),
            "true".to_string(),
        ])
        .unwrap();
        assert_eq!(cfg.backend, Backend::Docker);
        assert_eq!(cfg.docker_image, "node:22");
        assert!(cfg.docker_mount_auth);

        let cfg = parse_args(&[
            "--backend".to_string(),
            "worktree".to_string(),
            "--".to_string(),
            "true".to_string(),
        ])
        .unwrap();
        assert_eq!(cfg.backend, Backend::Worktree);

        let cfg = parse_args(&["--".to_string(), "true".to_string()]).unwrap();
        assert_eq!(cfg.backend, Backend::Auto);
        assert_eq!(cfg.docker_image, docker::DEFAULT_IMAGE);
        assert!(!cfg.docker_mount_auth);

        assert!(parse_args(&[
            "--backend".to_string(),
            "podman".to_string(),
            "--".to_string(),
            "true".to_string()
        ])
        .is_err());
    }

    #[test]
    fn parse_args_docker_flags_require_docker_backend() {
        // docker flags without --backend docker → fail-closed
        assert!(parse_args(&[
            "--docker-image".to_string(),
            "node:22".to_string(),
            "--".to_string(),
            "true".to_string()
        ])
        .is_err());
        assert!(parse_args(&[
            "--docker-mount-auth".to_string(),
            "--".to_string(),
            "true".to_string()
        ])
        .is_err());
        // default image with --backend docker → ok
        assert!(parse_args(&[
            "--backend".to_string(),
            "docker".to_string(),
            "--".to_string(),
            "true".to_string()
        ])
        .is_ok());
    }

    #[test]
    fn backend_effective_resolution() {
        assert_eq!(Backend::Auto.effective(), Backend::Worktree);
        assert_eq!(Backend::Worktree.effective(), Backend::Worktree);
        assert_eq!(Backend::Docker.effective(), Backend::Docker);
        assert_eq!(Backend::Docker.as_str(), "docker");
    }

    #[test]
    fn ledger_relative_basic() {
        let cwd = std::env::temp_dir();
        let rel = ledger_relative(&cwd, Some(Path::new(".sandbox-run/runs.jsonl")));
        // temp dir is not a git repo; canonicalization may fail → still computes
        assert!(rel.is_some());
    }
}
