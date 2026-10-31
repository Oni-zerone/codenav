//! Which repository state an index was built from: the Git HEAD commit and branch, read
//! straight from `.git` (no library), and whether there were uncommitted changes.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::graph::Graph;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Head {
    pub commit: String,
    /// None when HEAD is detached.
    pub branch: Option<String>,
}

/// A `git` command in `root` that never takes optional locks (`.git/index.lock`), so it
/// cannot make the user's own git commands fail; None when git is missing or fails.
fn git(root: &Path, args: &[&str]) -> Option<Vec<u8>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status.success().then_some(out.stdout)
}

/// The `.git` directory of the repository `root` belongs to (a worktree's or submodule's
/// `.git` file points to it), and its common directory (shared refs). A repository found
/// in a parent directory counts only if it tracks files under `root`: an unversioned
/// folder inside, say, a dotfiles repository is not part of it.
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
            if d != root && git(root, &["ls-files", "--error-unmatch", "--", "."]).is_none() {
                return None;
            }
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
    head_from_files(&git_dir, &common).or_else(|| head_from_git(root))
}

/// HEAD from `.git` files: loose and packed refs, detached HEAD.
fn head_from_files(git_dir: &Path, common: &Path) -> Option<Head> {
    let head = fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let head = head.trim();
    let Some(reference) = head.strip_prefix("ref:").map(str::trim) else {
        return is_sha(head).then(|| Head {
            commit: head.to_string(),
            branch: None,
        });
    };
    let commit = [git_dir, common]
        .iter()
        .find_map(|d| fs::read_to_string(d.join(reference)).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| is_sha(s))
        .or_else(|| packed_ref(common, reference))?;
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

/// HEAD through the git command, for layouts the files don't cover (e.g. reftable).
fn head_from_git(root: &Path) -> Option<Head> {
    let text = |b: Vec<u8>| String::from_utf8_lossy(&b).trim().to_string();
    let commit = text(git(root, &["rev-parse", "--verify", "-q", "HEAD"])?);
    let branch = git(root, &["symbolic-ref", "-q", "--short", "HEAD"])
        .map(text)
        .filter(|b| !b.is_empty());
    is_sha(&commit).then_some(Head { commit, branch })
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

/// Whether the indexed code under `root` differs from HEAD: changes to tracked files
/// (staged or not), or untracked files the index includes. None without git.
fn uncommitted_changes(root: &Path, indexed: &dyn Fn(&str) -> bool) -> Option<bool> {
    let tracked = git(
        root,
        &["status", "--porcelain", "--untracked-files=no", "--", "."],
    )?;
    if !tracked.is_empty() {
        return Some(true);
    }
    // Paths relative to `root`, like the graph's.
    let untracked = git(
        root,
        &[
            "ls-files",
            "--others",
            "--exclude-standard",
            "-z",
            "--",
            ".",
        ],
    )?;
    Some(
        untracked
            .split(|&b| b == 0)
            .filter(|p| !p.is_empty())
            .any(|p| indexed(&String::from_utf8_lossy(p))),
    )
}

/// Records in `g` the repository state for the code it was built from: HEAD and whether
/// that code had uncommitted changes. Outside a Git repository the fields are cleared.
pub fn record_state(g: &mut Graph, root: &Path) {
    let head = head(root);
    let dirty = head
        .is_some()
        .then(|| uncommitted_changes(root, &|p| g.by_file.contains_key(p)))
        .flatten();
    g.git_commit = head.as_ref().map(|h| h.commit.clone());
    g.git_branch = head.and_then(|h| h.branch);
    g.git_dirty = dirty;
}

/// Whether the files `indexed` (paths relative to `root`) differ from HEAD, as recorded by
/// `record_state` (git status: tens of ms, so the server runs it in the background after
/// an incremental update). None outside a Git repository.
pub fn dirty(root: &Path, indexed: &HashSet<String>) -> Option<bool> {
    head(root)?;
    uncommitted_changes(root, &|p| indexed.contains(p))
}

/// When only the repository state moved (a commit, checkout of the same files, reset
/// --soft) the graph still matches the code: update its Git fields instead of re-indexing.
/// Returns whether they changed (so the caller can save the graph).
pub fn refresh_state(g: &mut Graph, root: &Path) -> bool {
    if !head_changed(g, root) {
        return false;
    }
    record_state(g, root);
    true
}

/// Whether HEAD (commit or branch) differs from the one recorded in `g` (reads `.git`).
pub fn head_changed(g: &Graph, root: &Path) -> bool {
    let now = head(root);
    now.as_ref().map(|h| &h.commit) != g.git_commit.as_ref()
        || now.as_ref().and_then(|h| h.branch.as_ref()) != g.git_branch.as_ref()
}

/// When and from which repository state the graph was built. `check_sources` also
/// looks for source files changed since (a full walk: callers that re-index on their own
/// skip it), warning that a re-index is needed.
pub fn index_state(g: &Graph, root: &Path, check_sources: bool) -> serde_json::Value {
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
    if check_sources && crate::indexer::is_stale(root, g) {
        warnings.push("source files changed since indexing: call reindex".to_string());
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
        assert_eq!(head(&d.0), Some(main));
        // A subdirectory only belongs to it with tracked files (none in this fake repo).
        d.write("src/x.rs", "");
        assert_eq!(head(&d.0.join("src")), None);

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
    fn warns_when_head_moved_since_indexing() {
        let d = Dir::new("warn");
        d.write(".git/HEAD", "ref: refs/heads/main\n");
        d.write(".git/refs/heads/main", &format!("{B}\n"));
        let mut g = Graph::new(&d.0);
        g.git_commit = Some(A.into());
        g.git_branch = Some("main".into());
        let v = index_state(&g, &d.0, false);
        assert_eq!(
            v["warnings"],
            serde_json::json!([
                "HEAD is now 222222222222 (main) but the graph was built at 111111111111: it may not match the code"
            ])
        );
        g.git_commit = Some(B.into());
        assert!(index_state(&g, &d.0, false).get("warnings").is_none());
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
