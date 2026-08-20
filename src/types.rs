//! Shared types: scope model, verify result, sandbox info, run report.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Vcs {
    Git,
    Jj,
}

impl Vcs {
    pub fn as_str(&self) -> &'static str {
        match self {
            Vcs::Git => "git",
            Vcs::Jj => "jj",
        }
    }
}

/// G2 result classification. `Interrupted` is only produced by the ledger
/// repair path (a dangling run.start with no report.emit), never by a live run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    Pass,
    Fail,
    Timeout,
    Polluted,
    Rejected,
    Interrupted,
}

impl Verdict {
    pub fn as_str(&self) -> &'static str {
        match self {
            Verdict::Pass => "pass",
            Verdict::Fail => "fail",
            Verdict::Timeout => "timeout",
            Verdict::Polluted => "polluted",
            Verdict::Rejected => "rejected",
            Verdict::Interrupted => "interrupted",
        }
    }
}

/// The verification scope: which VCS, which base, which changed files.
/// `files` is the sync manifest — the exact set of paths overlaid into the
/// sandbox (copied when present in the main tree, deleted otherwise).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scope {
    pub vcs: Option<Vcs>,
    pub base: String,
    /// "git-diff" | "jj-diff" | "changeset"
    pub source: String,
    pub files: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub excluded: Vec<String>,
}

/// Explicit ChangeSet input (`--changeset file.json`): the caller decides the
/// scope instead of VCS detection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangeSet {
    #[serde(default = "default_base")]
    pub base_ref: String,
    #[serde(default)]
    pub files: Vec<String>,
}

fn default_base() -> String {
    "HEAD".to_string()
}

/// Outcome of the verify command execution (L2).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VerifyResult {
    /// The verify argv (filled in by the orchestrator for the report).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cmd: Vec<String>,
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
    /// Merged stdout+stderr, tail-kept (default 64 KiB).
    pub output_tail: String,
    pub truncated: bool,
    /// True when the whole process tree was killed by our timeout.
    pub killed: bool,
}

impl VerifyResult {
    pub fn empty() -> Self {
        VerifyResult {
            cmd: Vec::new(),
            exit_code: None,
            duration_ms: 0,
            output_tail: String::new(),
            truncated: false,
            killed: false,
        }
    }
}

/// Sandbox description for the report (L2).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxInfo {
    /// "worktree" | "workspace" | "none"
    pub backend: String,
    pub base: String,
    pub overlaid_files: usize,
    pub cleaned: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct GateResult {
    pub gate: String,
    pub pass: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metric: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actual: Option<f64>,
    pub detail: String,
}

/// The machine-readable RunReport emitted on stdout (`--emit json`, default).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunReport {
    pub run_id: String,
    pub verdict: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vcs: Option<String>,
    pub base_ref: String,
    pub scope: Scope,
    pub sandbox: SandboxInfo,
    pub verify: VerifyResult,
    pub gates: Vec<GateResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log_path: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verdict_roundtrip() {
        assert_eq!(Verdict::Pass.as_str(), "pass");
        assert_eq!(Verdict::Interrupted.as_str(), "interrupted");
        let v: Verdict = serde_json::from_str("\"polluted\"").unwrap();
        assert_eq!(v, Verdict::Polluted);
    }

    #[test]
    fn changeset_default_base() {
        let cs: ChangeSet = serde_json::from_str("{\"files\":[\"a.rs\"]}").unwrap();
        assert_eq!(cs.base_ref, "HEAD");
        assert_eq!(cs.files, vec!["a.rs".to_string()]);
    }

    #[test]
    fn report_serializes_camelcase() {
        let report = RunReport {
            run_id: "run-1".into(),
            verdict: "pass".into(),
            vcs: Some("git".into()),
            base_ref: "HEAD".into(),
            scope: Scope {
                vcs: Some(Vcs::Git),
                base: "HEAD".into(),
                source: "git-diff".into(),
                files: vec!["a.rs".into()],
                excluded: vec![],
            },
            sandbox: SandboxInfo {
                backend: "worktree".into(),
                base: "HEAD".into(),
                overlaid_files: 1,
                cleaned: true,
                detail: None,
            },
            verify: VerifyResult {
                cmd: vec!["cargo".into()],
                exit_code: Some(0),
                duration_ms: 5,
                output_tail: String::new(),
                truncated: false,
                killed: false,
            },
            gates: vec![],
            log_path: Some(".sandbox-run/runs.jsonl".into()),
        };
        let json = serde_json::to_string(&report).unwrap();
        assert!(json.contains("\"runId\""));
        assert!(json.contains("\"baseRef\""));
        assert!(json.contains("\"overlaidFiles\""));
        assert!(json.contains("\"logPath\""));
        assert!(!json.contains("\"run_id\""));
    }
}
