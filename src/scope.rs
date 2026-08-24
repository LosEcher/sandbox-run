//! L1 — Change detection (scope detector). The harness decides the scope,
//! never the verify command: explicit ChangeSet or git/jj diff detection.
//! Detection failure or an unrecognizable repository is fail-closed (exit 2),
//! never a silent fallback to running in the main tree.

use std::path::Path;
use std::process::Command;

use crate::types::{ChangeSet, Scope, Vcs};

/// Default excluded cache/build directories (clean-verify: the sandbox
/// rebuilds them, so a pass proves a clean tree passes — the tool's first
/// value). These are excluded from sync AND from G1 pollution counting.
pub const DEFAULT_EXCLUDES: &[&str] = &[
    "target/**",
    "**/target/**",
    "node_modules/**",
    "**/node_modules/**",
    "dist/**",
    "**/dist/**",
    ".venv/**",
    "**/.venv/**",
    "__pycache__/**",
    "**/__pycache__/**",
    ".pytest_cache/**",
    "**/.pytest_cache/**",
];

/// VCS metadata that must never be synced.
const VCS_META: &[&str] = &[".git", ".jj"];

#[derive(Debug)]
pub enum ScopeError {
    NotARepository,
    CommandFailed { cmd: String, status: String },
}

impl std::fmt::Display for ScopeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ScopeError::NotARepository => write!(
                f,
                "not a git or jj repository (no .git/.jj found in cwd or parents); \
                 refusing to run in the main tree (fail-closed)"
            ),
            ScopeError::CommandFailed { cmd, status } => {
                write!(f, "command `{cmd}` failed: {status}")
            }
        }
    }
}

impl std::error::Error for ScopeError {}

/// Detect the VCS of `cwd` by walking up the directory tree.
pub fn detect_vcs(cwd: &Path) -> Option<Vcs> {
    let mut dir = Some(cwd);
    while let Some(d) = dir {
        if d.join(".git").exists() {
            return Some(Vcs::Git);
        }
        if d.join(".jj").exists() {
            return Some(Vcs::Jj);
        }
        dir = d.parent();
    }
    None
}

fn run(cmd: &mut Command) -> Result<String, ScopeError> {
    let out = cmd.output().map_err(|e| ScopeError::CommandFailed {
        cmd: format!("{cmd:?}"),
        status: e.to_string(),
    })?;
    if !out.status.success() {
        return Err(ScopeError::CommandFailed {
            cmd: format!("{cmd:?}"),
            status: format!(
                "exit {:?}: {}",
                out.status.code(),
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// NUL-separated git output (`-z`) → list of paths (drop empty trailing).
pub(crate) fn nul_paths(out: &str) -> Vec<String> {
    out.split('\0')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Split a `git status --porcelain=v1 -z` stream into (status, path) pairs.
/// Rename entries emit two records; we only consume their path parts.
pub fn status_entries(out: &[u8]) -> Vec<(String, String)> {
    let mut entries = Vec::new();
    for seg in out.split(|&b| b == 0) {
        if seg.len() < 3 {
            continue;
        }
        let st = String::from_utf8_lossy(&seg[..2]).into_owned();
        let mut path = String::from_utf8_lossy(&seg[3..]).into_owned();
        // untracked dirs carry a trailing slash in some git versions
        while path.ends_with('/') {
            path.pop();
        }
        entries.push((st, path));
    }
    entries
}

fn git_scope(
    cwd: &Path,
    base: &str,
    excludes: &[String],
    ledger_rel: &Option<String>,
) -> Result<Scope, ScopeError> {
    // tracked changes vs base (all statuses: modified/added/deleted/renamed)
    let tracked = run(Command::new("git")
        .arg("diff")
        .arg("--name-only")
        .arg("--no-renames")
        .arg("-z")
        .arg(base)
        .current_dir(cwd))?;
    // untracked non-ignored files (new agent files)
    let untracked = run(Command::new("git")
        .arg("ls-files")
        .arg("--others")
        .arg("--exclude-standard")
        .arg("-z")
        .current_dir(cwd))?;

    let mut files: Vec<String> = Vec::new();
    for p in nul_paths(&tracked).into_iter().chain(nul_paths(&untracked)) {
        if !is_excluded(&p, excludes) && !is_ledger(&p, ledger_rel) && !is_vcs_meta(&p) {
            files.push(p);
        }
    }
    files.sort();
    files.dedup();

    Ok(Scope {
        vcs: Some(Vcs::Git),
        base: base.to_string(),
        source: "git-diff".to_string(),
        files,
        excluded: Vec::new(),
    })
}

fn jj_scope(
    cwd: &Path,
    excludes: &[String],
    ledger_rel: &Option<String>,
) -> Result<Scope, ScopeError> {
    // jj diff --name-only includes modified/added (incl. untracked)/deleted.
    let out = run(Command::new("jj")
        .arg("diff")
        .arg("--name-only")
        .current_dir(cwd))?;
    let mut files: Vec<String> = Vec::new();
    for p in out
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
    {
        if !is_excluded(&p, excludes) && !is_ledger(&p, ledger_rel) && !is_vcs_meta(&p) {
            files.push(p);
        }
    }
    files.sort();
    files.dedup();

    Ok(Scope {
        vcs: Some(Vcs::Jj),
        base: "@".to_string(),
        source: "jj-diff".to_string(),
        files,
        excluded: Vec::new(),
    })
}

/// Detect the scope. `force` picks the VCS; None auto-detects.
/// `ledger_rel` (the event-log path relative to cwd, when inside the repo) is
/// excluded from the change set.
pub fn detect_scope(
    cwd: &Path,
    vcs: Option<Vcs>,
    base: &str,
    excludes: &[String],
    ledger_rel: &Option<String>,
) -> Result<Scope, ScopeError> {
    let vcs = match vcs {
        Some(v) => v,
        None => detect_vcs(cwd).ok_or(ScopeError::NotARepository)?,
    };
    match vcs {
        Vcs::Git => git_scope(cwd, base, excludes, ledger_rel),
        Vcs::Jj => jj_scope(cwd, excludes, ledger_rel),
    }
}

/// Build the scope from an explicit ChangeSet (caller decides; detection
/// skipped). Files are copied from the main tree when present; the VCS is
/// still detected to choose the sandbox backend.
pub fn scope_from_changeset(
    cwd: &Path,
    cs: &ChangeSet,
    excludes: &[String],
    ledger_rel: &Option<String>,
) -> Result<Scope, ScopeError> {
    let vcs = detect_vcs(cwd).ok_or(ScopeError::NotARepository)?;
    let mut files: Vec<String> = Vec::new();
    for p in &cs.files {
        if !is_excluded(p, excludes) && !is_ledger(p, ledger_rel) && !is_vcs_meta(p) {
            files.push(p.clone());
        }
    }
    files.sort();
    files.dedup();
    Ok(Scope {
        vcs: Some(vcs),
        base: cs.base_ref.clone(),
        source: "changeset".to_string(),
        files,
        excluded: Vec::new(),
    })
}

/// True when `path` is inside the event-log directory (the only file the tool
/// is allowed to write in the main tree). The whole parent dir is excluded
/// (e.g. `.sandbox-run/`), so `git status` never reports the ledger itself as
/// pollution of the main tree.
pub(crate) fn is_ledger(path: &str, ledger_rel: &Option<String>) -> bool {
    let Some(rel) = ledger_rel else {
        return false;
    };
    if rel.is_empty() {
        return false;
    }
    if path == rel {
        return true;
    }
    let parent = std::path::Path::new(rel)
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .filter(|p| !p.is_empty());
    match parent {
        Some(p) => path == p || path.starts_with(&format!("{p}/")),
        None => false,
    }
}

fn is_vcs_meta(path: &str) -> bool {
    let first = path.split('/').next().unwrap_or("");
    VCS_META.contains(&first)
}

/// glob-ish matching: '*' matches within a path segment, '**' crosses segments.
pub fn is_excluded(path: &str, patterns: &[String]) -> bool {
    patterns.iter().any(|p| glob_match(p, path))
}

fn glob_match(pattern: &str, text: &str) -> bool {
    fn match_here(p: &[char], t: &[char]) -> bool {
        if p.is_empty() {
            return t.is_empty();
        }
        match p[0] {
            '*' => {
                if p.len() >= 2 && p[1] == '*' {
                    let rest = &p[2..];
                    for i in 0..=t.len() {
                        if match_here(rest, &t[i..]) {
                            return true;
                        }
                    }
                    false
                } else {
                    let rest = &p[1..];
                    let max = t.iter().position(|&c| c == '/').unwrap_or(t.len());
                    for i in 0..=max {
                        if match_here(rest, &t[i..]) {
                            return true;
                        }
                    }
                    false
                }
            }
            c => !t.is_empty() && t[0] == c && match_here(&p[1..], &t[1..]),
        }
    }
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    match_here(&p, &t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_rules() {
        let pats: Vec<String> = DEFAULT_EXCLUDES.iter().map(|s| s.to_string()).collect();
        assert!(is_excluded("target/", &pats));
        assert!(is_excluded("target/debug/x", &pats));
        assert!(is_excluded("src/target/x.rs", &pats));
        assert!(is_excluded("node_modules/pkg/index.js", &pats));
        assert!(is_excluded("dist/bundle.js", &pats));
        assert!(!is_excluded("src/main.rs", &pats));
        assert!(!is_excluded("targetx/keep.rs", &pats));
        assert!(!is_excluded("mytarget/keep.rs", &pats));
    }

    #[test]
    fn vcs_meta_filter() {
        assert!(is_vcs_meta(".git/config"));
        assert!(is_vcs_meta(".jj/repo"));
        assert!(is_vcs_meta(".git"));
        assert!(!is_vcs_meta("src/.gitkeep"));
    }

    #[test]
    fn ledger_filter() {
        let rel = Some(".sandbox-run/runs.jsonl".to_string());
        assert!(is_ledger(".sandbox-run/runs.jsonl", &rel));
        assert!(is_ledger(".sandbox-run/other.jsonl", &rel));
        assert!(!is_ledger(".sandbox-runx/runs.jsonl", &rel));
        assert!(!is_ledger("src/lib.rs", &rel));
        assert!(!is_ledger("src/lib.rs", &None));
    }

    #[test]
    fn status_entries_parse() {
        let out = b" M src/a.rs\0?? new.txt\0 D old.rs\0";
        let entries = status_entries(out);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0], (" M".to_string(), "src/a.rs".to_string()));
        assert_eq!(entries[1], ("??".to_string(), "new.txt".to_string()));
        assert_eq!(entries[2], (" D".to_string(), "old.rs".to_string()));
    }

    #[test]
    fn nul_paths_basic() {
        assert_eq!(
            nul_paths("a.rs\0b.rs\0"),
            vec!["a.rs".to_string(), "b.rs".to_string()]
        );
        assert!(nul_paths("").is_empty());
    }
}
