//! L4 — Event-sourced run ledger. Append-only JSONL is the single source of
//! truth; reports and stats are derived projections. Replay/audit/recovery is
//! structurally free (a dangling run.start without report.emit is repaired as
//! `interrupted`, mirroring DSH repair semantics).

use serde::Serialize;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

static SEQ: AtomicU64 = AtomicU64::new(0);

pub fn new_run_id() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    format!("run-{now:013}-{seq}")
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Event types (v1). Event names use the design's dotted form
/// (`run.start`, `sandbox.setup`, ...); payload keys are snake_case.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Event<'a> {
    #[serde(rename = "run.start")]
    RunStart {
        run_id: String,
        version: &'a str,
        ts: u64,
        cwd: &'a str,
        vcs: Option<&'a str>,
        base_ref: &'a str,
        cmd: Vec<&'a str>,
        env_keys: Vec<&'a str>,
        scope_files: Vec<String>,
        scope_source: &'a str,
        config_hash: &'a str,
    },
    #[serde(rename = "scope.detect")]
    ScopeDetect {
        source: &'a str,
        files: Vec<String>,
        excluded: Vec<String>,
    },
    #[serde(rename = "sandbox.setup")]
    SandboxSetup {
        backend: &'a str,
        base: &'a str,
        overlaid_files: usize,
        started_at: u64,
    },
    #[serde(rename = "verify.start")]
    VerifyStart {
        cmd: Vec<&'a str>,
        cwd_in_sandbox: &'a str,
        timeout_ms: u64,
    },
    #[serde(rename = "verify.finish")]
    VerifyFinish {
        exit_code: Option<i32>,
        duration_ms: u64,
        output_tail: &'a str,
        truncated: bool,
        killed: bool,
    },
    #[serde(rename = "gate.check")]
    GateCheck {
        gate: &'a str,
        pass: bool,
        metric: Option<f64>,
        limit: Option<f64>,
        actual: Option<f64>,
        detail: &'a str,
    },
    #[serde(rename = "report.emit")]
    ReportEmit {
        verdict: &'a str,
        duration_ms: u64,
        output_path: Option<&'a str>,
    },
}

/// Append one event to the JSONL log (no-op if path is None).
pub fn append(log_path: Option<&Path>, event: &Event<'_>) -> std::io::Result<()> {
    let Some(path) = log_path else {
        return Ok(());
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut f = OpenOptions::new().create(true).append(true).open(path)?;
    let mut line =
        serde_json::to_string(event).map_err(|e| std::io::Error::other(e.to_string()))?;
    line.push('\n');
    f.write_all(line.as_bytes())?;
    f.flush()
}
