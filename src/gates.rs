//! L3 — Mechanical gates. Every invariant that can be checked mechanically is
//! a gate; G0 is always-on and its failure is an exit-2 bug signal, G1 is the
//! sandbox-pollution deny (default) / warn policy, G2 is the classification.

use std::path::Path;
use std::process::Command;

use crate::scope::{is_excluded, status_entries};
use crate::types::{GateResult, Verdict, VerifyResult};
use sha2::{Digest, Sha256};

/// G0 — isolation integrity. The main tree's VCS-visible state must be
/// byte-identical before and after the run (the event-log ledger is excluded:
/// it is the one designed write path in the main tree), and the sandbox must
/// be cleaned up. A failure here is a bug, not a verdict (exit 2).
pub fn g0_check(before: &str, after: &str, cleaned: bool, clean_detail: &str) -> GateResult {
    let pass = before == after && cleaned;
    let detail = if before != after {
        "main tree VCS state changed during the run (isolation violated)".to_string()
    } else if !cleaned {
        clean_detail.to_string()
    } else {
        "main tree state unchanged, sandbox cleaned".to_string()
    };
    GateResult {
        gate: "isolation.integrity".to_string(),
        pass,
        metric: None,
        limit: None,
        actual: None,
        detail,
    }
}

/// G1 — sandbox pollution. N = verify-induced changes to the materialized
/// snapshot (outside excluded dirs). deny (default): N>0 → polluted.
pub fn g1_check(polluted_paths: &[String], policy: &str) -> GateResult {
    let n = polluted_paths.len();
    let pass = n == 0;
    let detail = if n == 0 {
        "0 files modified by verify (sandbox clean)".to_string()
    } else {
        format!(
            "{} file(s) modified by verify (pollution{}): {}",
            n,
            if policy == "warn" {
                ", warn policy"
            } else {
                ", denied"
            },
            polluted_paths.join(", ")
        )
    };
    GateResult {
        gate: "sandbox.clean".to_string(),
        pass,
        metric: Some(n as f64),
        limit: Some(0.0),
        actual: Some(n as f64),
        detail,
    }
}

/// G2 — result classification. Precedence: timeout > polluted(deny) > pass/fail.
pub fn classify(verify: &VerifyResult, pollution_count: usize, policy: &str) -> Verdict {
    if verify.killed {
        return Verdict::Timeout;
    }
    if pollution_count > 0 && policy == "deny" {
        return Verdict::Polluted;
    }
    match verify.exit_code {
        Some(0) => Verdict::Pass,
        _ => Verdict::Fail,
    }
}

/// Snapshot the git main-tree status with the ledger path filtered out.
pub fn git_main_state(cwd: &Path, ledger_rel: &Option<String>) -> Result<String, String> {
    let out = Command::new("git")
        .arg("status")
        .arg("--porcelain=v1")
        .arg("-z")
        .current_dir(cwd)
        .output()
        .map_err(|e| format!("git status failed: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git status failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let mut keep: Vec<String> = Vec::new();
    for (_st, path) in status_entries(&out.stdout) {
        let excluded = ledger_rel
            .as_ref()
            .map(|rel| path == *rel || path.starts_with(&format!("{rel}/")))
            .unwrap_or(false);
        if !excluded {
            keep.push(path);
        }
    }
    Ok(keep.join("\n"))
}

/// Snapshot the jj main-tree state (status + workspace list), filtering the
/// ledger path out of the status lines.
pub fn jj_main_state(cwd: &Path, ledger_rel: &Option<String>) -> Result<String, String> {
    let status = Command::new("jj")
        .arg("status")
        .current_dir(cwd)
        .output()
        .map_err(|e| format!("jj status failed: {e}"))?;
    if !status.status.success() {
        return Err(format!(
            "jj status failed: {}",
            String::from_utf8_lossy(&status.stderr).trim()
        ));
    }
    let ws = Command::new("jj")
        .arg("workspace")
        .arg("list")
        .current_dir(cwd)
        .output()
        .map_err(|e| format!("jj workspace list failed: {e}"))?;
    if !ws.status.success() {
        return Err(format!(
            "jj workspace list failed: {}",
            String::from_utf8_lossy(&ws.stderr).trim()
        ));
    }
    let mut lines: Vec<String> = Vec::new();
    let status_text = String::from_utf8_lossy(&status.stdout);
    for line in status_text.lines() {
        let hit = ledger_rel
            .as_ref()
            .map(|rel| line.contains(rel.as_str()))
            .unwrap_or(false);
        if !hit {
            lines.push(line.to_string());
        }
    }
    lines.push(String::from_utf8_lossy(&ws.stdout).into_owned());
    Ok(lines.join("\n"))
}

/// G1 git backend: pollute = status entries whose second column differs from
/// the index (worktree changed after `git add -A` baseline) or untracked,
/// outside excluded dirs.
pub fn git_pollution(sandbox_dir: &Path, excludes: &[String]) -> Result<Vec<String>, String> {
    let out = Command::new("git")
        .arg("status")
        .arg("--porcelain=v1")
        .arg("-z")
        .current_dir(sandbox_dir)
        .output()
        .map_err(|e| format!("git status (sandbox) failed: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git status (sandbox) failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let mut polluted = Vec::new();
    for (st, path) in status_entries(&out.stdout) {
        let second = st.chars().nth(1).unwrap_or(' ');
        let untracked = st.starts_with('?');
        if (second != ' ' || untracked) && !is_excluded(&path, excludes) {
            polluted.push(path);
        }
    }
    polluted.sort();
    Ok(polluted)
}

/// G1 jj backend: hash-snapshot the sandbox tree (skip .jj + excluded dirs)
/// before and after verify; changed/missing/new files outside excluded dirs
/// are pollution.
pub fn jj_pollution(
    sandbox_dir: &Path,
    excludes: &[String],
    baseline: &std::collections::BTreeMap<String, [u8; 32]>,
) -> Result<Vec<String>, String> {
    let after = snapshot_tree(sandbox_dir, excludes)?;
    let mut polluted: Vec<String> = Vec::new();
    for (path, hash) in &after {
        match baseline.get(path) {
            Some(prev) => {
                if prev != hash {
                    polluted.push(path.clone());
                }
            }
            None => polluted.push(path.clone()), // new file
        }
    }
    for path in baseline.keys() {
        if !after.contains_key(path) {
            polluted.push(path.clone()); // deleted by verify
        }
    }
    polluted.sort();
    Ok(polluted)
}

/// Hash every file under `root` (excluding VCS metadata and excluded dirs).
pub fn snapshot_tree(
    root: &Path,
    excludes: &[String],
) -> Result<std::collections::BTreeMap<String, [u8; 32]>, String> {
    let mut map = std::collections::BTreeMap::new();
    walk(root, root, excludes, &mut map)?;
    Ok(map)
}

fn walk(
    root: &Path,
    dir: &Path,
    excludes: &[String],
    map: &mut std::collections::BTreeMap<String, [u8; 32]>,
) -> Result<(), String> {
    let rd = std::fs::read_dir(dir).map_err(|e| format!("read_dir {}: {e}", dir.display()))?;
    for ent in rd {
        let ent = ent.map_err(|e| format!("read_dir entry: {e}"))?;
        let name = ent.file_name();
        let name_str = name.to_string_lossy();
        if name_str == ".git" || name_str == ".jj" {
            continue;
        }
        let rel = ent
            .path()
            .strip_prefix(root)
            .map_err(|e| e.to_string())?
            .to_string_lossy()
            .into_owned();
        // normalize to forward slashes so glob patterns + pollution paths are
        // platform-independent (Windows produces backslashes)
        let rel = rel.replace('\\', "/");
        let ft = ent
            .file_type()
            .map_err(|e| format!("file_type {}: {e}", ent.path().display()))?;
        if ft.is_dir() {
            if is_excluded(&format!("{rel}/"), excludes) {
                continue;
            }
            walk(root, &ent.path(), excludes, map)?;
        } else if ft.is_file() {
            if is_excluded(&rel, excludes) {
                continue;
            }
            let bytes = std::fs::read(ent.path())
                .map_err(|e| format!("read {}: {e}", ent.path().display()))?;
            let mut hasher = Sha256::new();
            hasher.update(&bytes);
            let digest: [u8; 32] = hasher.finalize().into();
            map.insert(rel, digest);
        }
    }
    Ok(())
}

/// sha256 hex of a string (used for config hash + G0 display).
pub fn sha256_hex(s: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    let d = h.finalize();
    let mut out = String::with_capacity(64);
    for b in d {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_precedence() {
        let clean = VerifyResult {
            cmd: vec!["verify".into()],
            exit_code: Some(0),
            duration_ms: 1,
            output_tail: String::new(),
            truncated: false,
            killed: false,
        };
        assert_eq!(classify(&clean, 0, "deny"), Verdict::Pass);

        let fail = VerifyResult {
            exit_code: Some(1),
            ..clean.clone()
        };
        assert_eq!(classify(&fail, 0, "deny"), Verdict::Fail);

        let timed = VerifyResult {
            killed: true,
            ..clean.clone()
        };
        assert_eq!(classify(&timed, 0, "deny"), Verdict::Timeout);
        // timeout wins over pollution
        assert_eq!(classify(&timed, 3, "deny"), Verdict::Timeout);

        let poll = VerifyResult {
            exit_code: Some(0),
            ..clean.clone()
        };
        assert_eq!(classify(&poll, 2, "deny"), Verdict::Polluted);
        // warn policy: pollution does not flip the verdict
        assert_eq!(classify(&poll, 2, "warn"), Verdict::Pass);
    }

    #[test]
    fn g0_detects_main_change_and_cleanup() {
        assert!(g0_check("a", "a", true, "").pass);
        assert!(!g0_check("a", "b", true, "").pass);
        assert!(!g0_check("a", "a", false, "dir exists").pass);
    }

    #[test]
    fn sha256_hex_stable() {
        let a = sha256_hex("hello");
        let b = sha256_hex("hello");
        assert_eq!(a, b);
        assert_eq!(a.len(), 64);
        assert_ne!(a, sha256_hex("hellp"));
    }

    #[test]
    fn snapshot_tree_basics() {
        let tmp = std::env::temp_dir().join(format!("sr-snap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("src")).unwrap();
        std::fs::create_dir_all(tmp.join("target")).unwrap();
        std::fs::write(tmp.join("src/a.rs"), "x").unwrap();
        std::fs::write(tmp.join("target/out"), "y").unwrap();
        let excludes: Vec<String> = vec!["target/**".to_string()];
        let snap = snapshot_tree(&tmp, &excludes).unwrap();
        assert!(snap.contains_key("src/a.rs"));
        assert!(!snap.contains_key("target/out"));
        std::fs::remove_dir_all(&tmp).unwrap();
    }
}
