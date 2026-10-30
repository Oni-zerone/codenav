//! Which repository state an index was built from: the Git HEAD commit and branch, read
//! straight from `.git` (no library), and whether there were uncommitted changes.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Head {
    pub commit: String,
    /// None when HEAD is detached.
    pub branch: Option<String>,
}

/// The `.git` directory of the repository containing `root` (a worktree's or
/// submodule's `.git` file points to it), and its common directory (shared refs).
fn git_dirs(root: &Path) -> Option<(PathBuf, PathBuf)> {
    let mut dir = Some(root);
    while let Some(d) = dir {
        let dot = d.join(".git");
        let git_dir = if dot.is_dir() {
            Some(dot)
        } else if dot.is_file() {
            let text = fs::read_to_string(&dot).ok()?;
            let target = text.trim().strip_prefix("gitdir:")?.trim();
            Some(d.join(target))
        } else {
            None
        };
        if let Some(git_dir) = git_dir {
            let common = fs::read_to_string(git_dir.join("commondir"))
                .map(|c| git_dir.join(c.trim()))
                .unwrap_or_else(|_| git_dir.clone());
            return Some((git_dir, common));
        }
        dir = d.parent();
    }
    None
}

/// HEAD of the repository containing `root`, or None outside a Git repository (or for a
/// branch with no commits yet).
pub fn head(root: &Path) -> Option<Head> {
    let (git_dir, common) = git_dirs(root)?;
    let head = fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head = head.trim();
    let Some(reference) = head.strip_prefix("ref:").map(str::trim) else {
        return is_sha(head).then(|| Head {
            commit: head.to_string(),
            branch: None,
        });
    };
    let commit = [&git_dir, &common]
        .iter()
        .find_map(|d| fs::read_to_string(d.join(reference)).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| is_sha(s))
        .or_else(|| packed_ref(&common, reference))?;
    Some(Head {
        commit,
        branch: Some(
            reference
                .strip_prefix("refs/heads/")
                .unwrap_or(reference)
                .to_string(),
        ),
    })
}

fn packed_ref(common: &Path, reference: &str) -> Option<String> {
    let packed = fs::read_to_string(common.join("packed-refs")).ok()?;
    packed.lines().find_map(|line| {
        let (sha, name) = line.split_once(' ')?;
        (name.trim() == reference && is_sha(sha)).then(|| sha.to_string())
    })
}

fn is_sha(s: &str) -> bool {
    (s.len() == 40 || s.len() == 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Whether tracked files under `root` have uncommitted (staged or unstaged) changes, via
/// `git status`; None when Git is not installed or `root` is not in a repository.
pub fn has_uncommitted_changes(root: &Path) -> Option<bool> {
    git_dirs(root)?;
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["status", "--porcelain", "--untracked-files=no", "--", "."])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status.success().then_some(!out.stdout.is_empty())
}

/// When and from which repository state the graph was built, with warnings when it no
/// longer matches: HEAD moved, or source files changed since.
pub fn index_state(g: &crate::graph::Graph, root: &Path) -> serde_json::Value {
    use serde_json::json;
    let mut v = json!({ "built_at_unix": g.indexed_at });
    if let Some(c) = &g.git_commit {
        v["commit"] = json!(c);
    }
    if let Some(b) = &g.git_branch {
        v["branch"] = json!(b);
    }
    if let Some(d) = g.git_dirty {
        v["uncommitted_changes"] = json!(d);
    }
    let mut warnings = Vec::new();
    if let (Some(built), Some(now)) = (&g.git_commit, head(root))
        && *built != now.commit
    {
        let at = match &now.branch {
            Some(b) => format!("{} ({b})", short(&now.commit)),
            None => short(&now.commit).to_string(),
        };
        warnings.push(format!(
            "HEAD is now {at} but the graph was built at {}: it may not match the code",
            short(built)
        ));
    }
    if crate::indexer::is_stale(root, g) {
        warnings.push(
            "source files changed since indexing: `serve` re-indexes automatically within \
             seconds, or call reindex"
                .to_string(),
        );
    }
    if !warnings.is_empty() {
        v["warnings"] = json!(warnings);
    }
    v
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(12)]
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "1111111111111111111111111111111111111111";
    const B: &str = "2222222222222222222222222222222222222222";

    struct Dir(PathBuf);
    impl Dir {
        fn new(name: &str) -> Dir {
            let d = std::env::temp_dir().join(format!("codenav-vcs-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&d);
            fs::create_dir_all(&d).unwrap();
            Dir(d)
        }
        fn write(&self, path: &str, content: &str) {
            let p = self.0.join(path);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, content).unwrap();
        }
    }
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn reads_branch_packed_and_detached_heads() {
        let d = Dir::new("heads");
        assert_eq!(head(&d.0.join("none")), None, "no repository");
        d.write(".git/HEAD", "ref: refs/heads/main\n");
        assert_eq!(head(&d.0), None, "no commit yet");
        d.write(".git/refs/heads/main", &format!("{A}\n"));
        let main = Head {
            commit: A.into(),
            branch: Some("main".into()),
        };
        assert_eq!(head(&d.0), Some(main.clone()));
        // From a subdirectory of the repository too.
        d.write("src/x.rs", "");
        assert_eq!(head(&d.0.join("src")), Some(main));

        d.write(".git/HEAD", "ref: refs/heads/feature/x\n");
        d.write(
            ".git/packed-refs",
            &format!("# pack-refs\n{B} refs/heads/feature/x\n"),
        );
        assert_eq!(
            head(&d.0),
            Some(Head {
                commit: B.into(),
                branch: Some("feature/x".into())
            })
        );

        d.write(".git/HEAD", &format!("{A}\n"));
        assert_eq!(
            head(&d.0),
            Some(Head {
                commit: A.into(),
                branch: None
            })
        );
    }

    #[test]
    fn follows_worktree_git_files() {
        let d = Dir::new("worktree");
        d.write("main/.git/refs/heads/wt", &format!("{B}\n"));
        d.write("main/.git/worktrees/wt/HEAD", "ref: refs/heads/wt\n");
        d.write("main/.git/worktrees/wt/commondir", "../..\n");
        d.write(
            "wt/.git",
            &format!("gitdir: {}\n", d.0.join("main/.git/worktrees/wt").display()),
        );
        assert_eq!(
            head(&d.0.join("wt")),
            Some(Head {
                commit: B.into(),
                branch: Some("wt".into())
            })
        );
    }
}
