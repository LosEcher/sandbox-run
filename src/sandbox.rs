//! L2 — Sandbox executor. git worktree / jj workspace lifecycle bound to one
//! run: created in `std::env::temp_dir()`, cleaned up on every exit path (a
//! leftover is a bug). The main tree is never used as execution cwd.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::types::Vcs;

pub struct Sandbox {
    pub backend: &'static str,
    pub vcs: Vcs,
    pub dir: PathBuf,
    /// jj only: the sandbox workspace's working-copy change id at add time.
    /// Stable across auto-snapshots, so `jj abandon <this>` removes the whole
    /// orphaned chain after `jj workspace forget`.
    pub jj_wc_change: Option<String>,
}

pub fn sandbox_dir(run_id: &str) -> PathBuf {
    std::env::temp_dir().join(format!("sandbox-run-{run_id}"))
}

/// Create the isolated sandbox for the run. `cwd` is the repo root (main).
pub fn setup(vcs: Vcs, base: &str, run_id: &str, cwd: &Path) -> Result<Sandbox, String> {
    let dir = sandbox_dir(run_id);
    let _ = std::fs::remove_dir_all(&dir);

    match vcs {
        Vcs::Git => {
            let add = Command::new("git")
                .arg("worktree")
                .arg("add")
                .arg("--detach")
                .arg(&dir)
                .arg(base)
                .current_dir(cwd)
                .output()
                .map_err(|e| format!("git worktree add failed to spawn: {e}"))?;
            if !add.status.success() {
                let _ = std::fs::remove_dir_all(&dir);
                return Err(format!(
                    "git worktree add --detach {} {} failed: {}",
                    dir.display(),
                    base,
                    String::from_utf8_lossy(&add.stderr).trim()
                ));
            }
            Ok(Sandbox {
                backend: "worktree",
                vcs,
                dir,
                jj_wc_change: None,
            })
        }
        Vcs::Jj => {
            let add = Command::new("jj")
                .arg("workspace")
                .arg("add")
                .arg(&dir)
                .current_dir(cwd)
                .output()
                .map_err(|e| format!("jj workspace add failed to spawn: {e}"))?;
            if !add.status.success() {
                // a half-registered workspace must not linger
                let name = dir
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let _ = Command::new("jj")
                    .arg("workspace")
                    .arg("forget")
                    .arg(&name)
                    .current_dir(cwd)
                    .output();
                let _ = std::fs::remove_dir_all(&dir);
                return Err(format!(
                    "jj workspace add {} failed: {}",
                    dir.display(),
                    String::from_utf8_lossy(&add.stderr).trim()
                ));
            }
            // record the wc change id (snapshots update the commit, not the
            // change); --no-graph keeps the template output free of graph
            // artifacts ("@  <id>\n│\n~")
            let wc = Command::new("jj")
                .arg("log")
                .arg("-r")
                .arg("@")
                .arg("--no-graph")
                .arg("-T")
                .arg("change_id.short()")
                .current_dir(&dir)
                .output()
                .map_err(|e| format!("jj log (sandbox wc) failed: {e}"))?;
            let wc_change = if wc.status.success() {
                let s = String::from_utf8_lossy(&wc.stdout);
                let id = s.trim().to_string();
                if id.is_empty() {
                    None
                } else {
                    Some(id)
                }
            } else {
                None
            };
            Ok(Sandbox {
                backend: "workspace",
                vcs,
                dir,
                jj_wc_change: wc_change,
            })
        }
    }
}

/// Establish the G1 pollution baseline. git: `git add -A` makes the index the
/// materialized reference so `git status` afterwards shows only verify-induced
/// changes. jj: no-op — pollution is detected by a file-hash snapshot taken by
/// the caller (gates::snapshot_tree), which avoids running any jj command
/// between baseline and verify.
pub fn establish_baseline(s: &Sandbox) -> Result<(), String> {
    match s.vcs {
        Vcs::Git => {
            let add = Command::new("git")
                .arg("add")
                .arg("-A")
                .current_dir(&s.dir)
                .output()
                .map_err(|e| format!("git add (baseline) failed to spawn: {e}"))?;
            if !add.status.success() {
                return Err(format!(
                    "git add -A in sandbox failed: {}",
                    String::from_utf8_lossy(&add.stderr).trim()
                ));
            }
        }
        Vcs::Jj => {}
    }
    Ok(())
}

/// Clean up the sandbox on every exit path. Returns (cleaned, detail).
pub fn cleanup(s: &Sandbox, cwd: &Path) -> (bool, String) {
    let mut issues: Vec<String> = Vec::new();
    match s.vcs {
        Vcs::Git => {
            let rm = Command::new("git")
                .arg("worktree")
                .arg("remove")
                .arg("--force")
                .arg(&s.dir)
                .current_dir(cwd)
                .output();
            if let Ok(out) = rm {
                if !out.status.success() {
                    // stale registration; prune it
                    let _ = Command::new("git")
                        .arg("worktree")
                        .arg("prune")
                        .current_dir(cwd)
                        .output();
                }
            }
            let _ = std::fs::remove_dir_all(&s.dir);
            if s.dir.exists() {
                issues.push(format!("sandbox dir still exists: {}", s.dir.display()));
            }
            let list = Command::new("git")
                .arg("worktree")
                .arg("list")
                .current_dir(cwd)
                .output();
            if let Ok(out) = list {
                let text = String::from_utf8_lossy(&out.stdout);
                let path_str = s.dir.to_string_lossy();
                if text
                    .lines()
                    .any(|l| l.trim().starts_with(path_str.as_ref()))
                {
                    issues.push("worktree still registered in `git worktree list`".to_string());
                }
            }
        }
        Vcs::Jj => {
            let name = s
                .dir
                .file_name()
                .map(|x| x.to_string_lossy().into_owned())
                .unwrap_or_default();
            let forget = Command::new("jj")
                .arg("workspace")
                .arg("forget")
                .arg(&name)
                .current_dir(cwd)
                .output();
            if let Ok(out) = forget {
                if !out.status.success() {
                    let msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
                    if !msg.contains("doesn't exist") && !msg.contains("not found") {
                        issues.push(format!("jj workspace forget failed: {msg}"));
                    }
                }
            }
            // abandon the orphaned wc change (and its chain); missing = already gone
            if let Some(wc) = &s.jj_wc_change {
                let ab = Command::new("jj")
                    .arg("abandon")
                    .arg(wc)
                    .current_dir(cwd)
                    .output();
                if let Ok(out) = ab {
                    if !out.status.success() {
                        let msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
                        if !msg.contains("doesn't exist") && !msg.contains("not found") {
                            issues.push(format!("jj abandon {wc} failed: {msg}"));
                        }
                    }
                }
            }
            let _ = std::fs::remove_dir_all(&s.dir);
            if s.dir.exists() {
                issues.push(format!("sandbox dir still exists: {}", s.dir.display()));
            }
        }
    }
    let cleaned = issues.is_empty();
    let detail = if cleaned {
        format!(
            "sandbox {} cleaned (dir removed, no VCS registration)",
            s.backend
        )
    } else {
        format!("sandbox cleanup incomplete: {}", issues.join("; "))
    };
    (cleaned, detail)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sandbox_dir_naming() {
        let d = sandbox_dir("run-123");
        let name = d.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with("sandbox-run-"));
        assert!(d.is_absolute());
    }
}
