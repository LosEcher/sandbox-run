//! L1 sync — materialize the main working state into the sandbox by copying
//! per-manifest (never patch-application, never hardlink/symlink: writes to
//! the sandbox must never penetrate back into the main tree, G1).

use std::path::{Path, PathBuf};

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

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
