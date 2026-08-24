//! L1 sync — materialize the main working state into the sandbox by copying
//! per-manifest (never patch-application, never hardlink/symlink: writes to
//! the sandbox must never penetrate back into the main tree, G1).

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::scope;
use crate::types::Vcs;

pub enum SyncKind {
    /// File exists in the main tree → copy main → sandbox.
    Copy,
    /// File was deleted in the main tree → remove from the sandbox.
    Delete,
}

pub struct SyncEntry {
    pub path: String,
    pub kind: SyncKind,
}

/// Plan the sync: every scope file is a copy (when present in the main tree)
/// or a delete (when the agent deleted it).
pub fn plan(cwd: &Path, files: &[String]) -> Vec<SyncEntry> {
    files
        .iter()
        .map(|p| SyncEntry {
            path: p.clone(),
            kind: if cwd.join(p).exists() {
                SyncKind::Copy
            } else {
                SyncKind::Delete
            },
        })
        .collect()
}

/// Apply the sync manifest into the sandbox. Returns the number of files
/// overlaid (copied + deleted) — the design's `overlaidFiles`.
pub fn apply(entries: &[SyncEntry], main: &Path, sandbox: &Path) -> Result<usize, String> {
    let mut count = 0usize;
    for e in entries {
        let dst = sandbox.join(&e.path);
        match e.kind {
            SyncKind::Copy => {
                let src = main.join(&e.path);
                if let Some(parent) = dst.parent() {
                    std::fs::create_dir_all(parent).map_err(|err| {
                        format!("sync: cannot create {}: {err}", parent.display())
                    })?;
                }
                std::fs::copy(&src, &dst)
                    .map_err(|err| format!("sync: cannot copy {}: {err}", src.display()))?;
            }
            SyncKind::Delete => {
                let _ = std::fs::remove_file(&dst);
            }
        }
        count += 1;
    }
    let _ = PathBuf::new();
    Ok(count)
}

/// Docker backend: the container has no VCS checkout, so the sandbox cannot be
/// reconstructed from a base commit — the full working state (all non-ignored
/// files: tracked + untracked, minus VCS metadata, cache dirs and the ledger)
/// is materialized instead. Mirrors what the worktree/workspace sandbox
/// contains (base commit + changes) without dragging gitignored files in.
/// Returns the list of relative paths to copy.
pub fn plan_full(vcs: Vcs, cwd: &Path) -> Result<Vec<String>, String> {
    match vcs {
        Vcs::Git => {
            let out = Command::new("git")
                .args(["ls-files", "-z", "-c", "-o", "--exclude-standard"])
                .current_dir(cwd)
                .output()
                .map_err(|e| format!("git ls-files failed to spawn: {e}"))?;
            if !out.status.success() {
                return Err(format!(
                    "git ls-files failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ));
            }
            Ok(scope::nul_paths(&String::from_utf8_lossy(&out.stdout)))
        }
        Vcs::Jj => {
            // jj snapshots untracked non-ignored files into the wc commit, so
            // `jj file list -r @` is exactly the non-ignored working state.
            let out = Command::new("jj")
                .args(["file", "list", "-r", "@"])
                .current_dir(cwd)
                .output()
                .map_err(|e| format!("jj file list failed to spawn: {e}"))?;
            if !out.status.success() {
                return Err(format!(
                    "jj file list failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ));
            }
            Ok(String::from_utf8_lossy(&out.stdout)
                .lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect())
        }
    }
}

/// Apply the full working-state plan into the sandbox. Paths that no longer
/// exist on disk (tracked-but-deleted) are skipped; symlinks are never copied
/// (no symlink penetration into or out of the sandbox). Returns the number of
/// files materialized.
pub fn apply_full(
    paths: &[String],
    main: &Path,
    sandbox: &Path,
    excludes: &[String],
    ledger_rel: &Option<String>,
) -> Result<usize, String> {
    let mut count = 0usize;
    for p in paths {
        if scope::is_excluded(p, excludes) || scope::is_ledger(p, ledger_rel) {
            continue;
        }
        let src = main.join(p);
        let meta = match std::fs::symlink_metadata(&src) {
            Ok(m) => m,
            Err(_) => continue, // deleted tracked file
        };
        if meta.file_type().is_symlink() {
            continue; // no symlink penetration
        }
        if !meta.is_file() {
            continue;
        }
        let dst = sandbox.join(p);
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|err| format!("sync: cannot create {}: {err}", parent.display()))?;
        }
        std::fs::copy(&src, &dst)
            .map_err(|err| format!("sync: cannot copy {}: {err}", src.display()))?;
        count += 1;
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn apply_full_copies_filters() {
        let tmp = std::env::temp_dir().join(format!("sr-sync-full-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(tmp.join("src")).unwrap();
        fs::create_dir_all(tmp.join("target")).unwrap();
        fs::create_dir_all(tmp.join(".sandbox-run")).unwrap();
        fs::write(tmp.join("src/a.rs"), "x").unwrap();
        fs::write(tmp.join("tracked.txt"), "v2").unwrap();
        fs::write(tmp.join("target/out"), "y").unwrap();
        fs::write(tmp.join(".sandbox-run/runs.jsonl"), "{}").unwrap();
        fs::write(tmp.join("gone.txt"), "x").unwrap(); // tracked but deleted
        fs::remove_file(tmp.join("gone.txt")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(tmp.join("tracked.txt"), tmp.join("link.txt")).unwrap();

        let excludes: Vec<String> = vec!["target/**".to_string()];
        let ledger = Some(".sandbox-run/runs.jsonl".to_string());
        let sb = tmp.join("sandbox");
        fs::create_dir_all(&sb).unwrap();
        let n = apply_full(
            &[
                "src/a.rs".to_string(),
                "tracked.txt".to_string(),
                "gone.txt".to_string(),
                "link.txt".to_string(),
                ".sandbox-run/runs.jsonl".to_string(),
            ],
            &tmp,
            &sb,
            &excludes,
            &ledger,
        )
        .unwrap();
        // target/**, ledger and the symlink are skipped; gone.txt is gone.
        assert_eq!(n, 2);
        assert_eq!(fs::read_to_string(sb.join("src/a.rs")).unwrap(), "x");
        assert_eq!(fs::read_to_string(sb.join("tracked.txt")).unwrap(), "v2");
        assert!(!sb.join("target/out").exists());
        assert!(!sb.join(".sandbox-run/runs.jsonl").exists());
        assert!(!sb.join("link.txt").exists());
        fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn plan_copy_and_delete() {
        let tmp = std::env::temp_dir().join(format!("sr-sync-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(tmp.join("sub")).unwrap();
        fs::write(tmp.join("kept.txt"), "new").unwrap();
        fs::write(tmp.join("gone.txt"), "x").unwrap();
        // gone.txt exists in the plan but not on disk anymore → Delete
        fs::remove_file(tmp.join("gone.txt")).unwrap();

        let files = vec!["kept.txt".to_string(), "gone.txt".to_string()];
        let plan = plan(&tmp, &files);
        assert_eq!(plan.len(), 2);
        assert!(matches!(plan[0].kind, SyncKind::Copy));
        assert!(matches!(plan[1].kind, SyncKind::Delete));

        let sb = tmp.join("sandbox");
        fs::create_dir_all(&sb).unwrap();
        fs::write(sb.join("gone.txt"), "old").unwrap();
        let n = apply(&plan, &tmp, &sb).unwrap();
        assert_eq!(n, 2);
        assert_eq!(fs::read_to_string(sb.join("kept.txt")).unwrap(), "new");
        assert!(!sb.join("gone.txt").exists());

        fs::remove_dir_all(&tmp).unwrap();
    }
}
