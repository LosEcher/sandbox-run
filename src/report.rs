//! L4 — Report emission + ledger derived views. The JSONL event stream is the
//! single source of truth; `status` and `log` are derived projections.
//! A dangling run.start without a report.emit is repaired as `interrupted`
//! (mirroring DSH repair: the run was cut off by a crash/kill).

use std::io::BufRead;
use std::path::Path;

use serde_json::Value;

use crate::types::{RunReport, SandboxInfo, Scope, VerifyResult};

#[derive(Debug)]
pub enum LedgerError {
    LogMissing(String),
    CorruptLine(usize, String),
}

impl std::fmt::Display for LedgerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LedgerError::LogMissing(p) => write!(f, "event log not found: {p}"),
            LedgerError::CorruptLine(n, msg) => {
                write!(f, "corrupt event log at line {n}: {msg}")
            }
        }
    }
}

impl std::error::Error for LedgerError {}

/// One derived run summary from the ledger.
#[derive(Debug, Clone)]
pub struct RunSummary {
    pub run_id: String,
    pub verdict: String,
    pub duration_ms: Option<u64>,
    pub cmd: Vec<String>,
    pub events: Vec<Value>,
}

impl RunSummary {
    /// report.emit present → the run completed; otherwise interrupted.
    pub fn completed(&self) -> bool {
        self.events
            .iter()
            .any(|e| e["t"].as_str() == Some("report.emit"))
    }
}

/// Read the ledger, group events by run, derive summaries. Runs are returned
/// in log order (oldest first).
pub fn read_runs(log_path: &Path) -> Result<Vec<RunSummary>, LedgerError> {
    let file = std::fs::File::open(log_path)
        .map_err(|_| LedgerError::LogMissing(log_path.display().to_string()))?;
    let reader = std::io::BufReader::new(file);

    let mut runs: Vec<RunSummary> = Vec::new();
    let mut current: Option<RunSummary> = None;

    for (idx, line) in reader.lines().enumerate() {
        let line = line.map_err(|e| LedgerError::CorruptLine(idx + 1, e.to_string()))?;
        if line.trim().is_empty() {
            continue;
        }
        let v: Value = serde_json::from_str(&line)
            .map_err(|e| LedgerError::CorruptLine(idx + 1, e.to_string()))?;
        let t = v["t"].as_str().unwrap_or("");
        if t == "run.start" {
            if let Some(prev) = current.take() {
                runs.push(prev);
            }
            let run_id = v["run_id"].as_str().unwrap_or("").to_string();
            let cmd = v["cmd"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|s| s.as_str().map(|x| x.to_string()))
                        .collect()
                })
                .unwrap_or_default();
            current = Some(RunSummary {
                run_id,
                verdict: "interrupted".to_string(),
                duration_ms: None,
                cmd,
                events: Vec::new(),
            });
        }
        if let Some(run) = &mut current {
            run.events.push(v);
        }
    }
    if let Some(prev) = current.take() {
        runs.push(prev);
    }

    for run in &mut runs {
        if let Some(emit) = run
            .events
            .iter()
            .find(|e| e["t"].as_str() == Some("report.emit"))
        {
            run.verdict = emit["verdict"].as_str().unwrap_or("unknown").to_string();
        }
        if let Some(fin) = run
            .events
            .iter()
            .find(|e| e["t"].as_str() == Some("verify.finish"))
        {
            run.duration_ms = fin["duration_ms"].as_u64();
        }
    }
    Ok(runs)
}

pub fn find_run<'a>(runs: &'a [RunSummary], run_id: &str) -> Option<&'a RunSummary> {
    runs.iter().find(|r| r.run_id == run_id)
}

fn format_duration(ms: Option<u64>) -> String {
    match ms {
        Some(m) if m >= 1000 => format!("{:.1}s", m as f64 / 1000.0),
        Some(m) => format!("{m}ms"),
        None => "-".to_string(),
    }
}

/// Human-readable status table (all runs; dangling runs show interrupted).
pub fn render_status(runs: &[RunSummary]) -> String {
    if runs.is_empty() {
        return "no runs recorded".to_string();
    }
    let mut out = String::new();
    out.push_str(&format!(
        "{:<24} {:<12} {:<10} {}\n",
        "RUN_ID", "VERDICT", "DURATION", "COMMAND"
    ));
    for r in runs {
        let cmd = if r.cmd.is_empty() {
            "(no command)".to_string()
        } else {
            r.cmd.join(" ")
        };
        out.push_str(&format!(
            "{:<24} {:<12} {:<10} {}\n",
            r.run_id,
            r.verdict,
            format_duration(r.duration_ms),
            cmd
        ));
    }
    out
}

/// Full event chain for one run + a rebuilt summary line.
pub fn render_log(run: &RunSummary) -> String {
    let mut out = String::new();
    for e in &run.events {
        out.push_str(&serde_json::to_string(e).unwrap_or_else(|_| "{}".to_string()));
        out.push('\n');
    }
    let exit = run
        .events
        .iter()
        .find(|e| e["t"].as_str() == Some("verify.finish"))
        .and_then(|e| e["exit_code"].as_i64())
        .map(|c| c.to_string())
        .unwrap_or_else(|| "-".to_string());
    let truncated = run
        .events
        .iter()
        .find(|e| e["t"].as_str() == Some("verify.finish"))
        .and_then(|e| e["truncated"].as_bool())
        .unwrap_or(false);
    out.push_str(&format!(
        "run {}: verdict {} · duration {} · exit {} · truncated {} · {}",
        run.run_id,
        run.verdict,
        format_duration(run.duration_ms),
        exit,
        truncated,
        if run.completed() {
            "completed"
        } else {
            "dangling start (repaired: interrupted)"
        }
    ));
    out
}

/// Build the RunReport for a live run. `verdict` is the G2 classification
/// computed by the caller (classify() + the empty-scope rejection case).
pub fn build_report(
    run_id: String,
    verdict: &str,
    scope: &Scope,
    sandbox: SandboxInfo,
    verify: &VerifyResult,
    gates: &[crate::types::GateResult],
    log_path: Option<String>,
) -> RunReport {
    RunReport {
        run_id,
        verdict: verdict.to_string(),
        vcs: scope.vcs.map(|v| v.as_str().to_string()),
        base_ref: scope.base.clone(),
        scope: scope.clone(),
        sandbox,
        verify: verify.clone(),
        gates: gates.to_vec(),
        log_path,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(v: Value) -> String {
        let mut s = serde_json::to_string(&v).unwrap();
        s.push('\n');
        s
    }

    fn obj(t: &str, run_id: &str) -> Value {
        serde_json::json!({ "t": t, "run_id": run_id })
    }

    #[test]
    fn dangling_run_is_interrupted() {
        let tmp = std::env::temp_dir().join(format!("sr-ledger-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let log = tmp.join("runs.jsonl");
        // a completed run then a dangling run.start
        let mut content = String::new();
        content.push_str(&line(obj("run.start", "run-a")));
        content.push_str(&line(obj("report.emit", "run-a")));
        content.push_str(&line(obj("run.start", "run-b")));
        std::fs::write(&log, content).unwrap();

        let runs = read_runs(&log).unwrap();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].verdict, "unknown"); // report.emit without verdict field
        assert!(runs[0].completed());
        assert_eq!(runs[1].verdict, "interrupted");
        assert!(!runs[1].completed());
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn report_emit_verdict_wins() {
        let tmp = std::env::temp_dir().join(format!("sr-ledger2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let log = tmp.join("runs.jsonl");
        let mut content = String::new();
        content.push_str(&line(obj("run.start", "run-x")));
        content.push_str(&line(serde_json::json!({
            "t": "report.emit", "run_id": "run-x", "verdict": "timeout"
        })));
        std::fs::write(&log, content).unwrap();
        let runs = read_runs(&log).unwrap();
        assert_eq!(runs[0].verdict, "timeout");
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn corrupt_line_reported() {
        let tmp = std::env::temp_dir().join(format!("sr-ledger3-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let log = tmp.join("runs.jsonl");
        std::fs::write(&log, "not json\n").unwrap();
        assert!(read_runs(&log).is_err());
        std::fs::remove_dir_all(&tmp).unwrap();
    }
}
