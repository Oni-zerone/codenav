//! Impact of local changes or of a branch: reads `git diff`, maps the changed lines to the
//! symbols they belong to, finds everything that depends on them, and lists the code that
//! was removed (which the updated graph no longer has).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};

use crate::extract;
use crate::graph::{Confidence, Graph, SymKind};
use crate::lang::Lang;
use crate::query;

/// Which changes to analyse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffSource {
    /// Every uncommitted change (staged or not, and untracked files) against HEAD.
    WorkingTree,
    /// Staged changes against HEAD.
    Staged,
    /// Commits on the current branch since it left `base` (`base...HEAD`).
    Base(String),
}

impl DiffSource {
    fn describe(&self) -> String {
        match self {
            DiffSource::WorkingTree => "uncommitted changes vs HEAD".into(),
            DiffSource::Staged => "staged changes vs HEAD".into(),
            DiffSource::Base(b) => format!("{b}...HEAD"),
        }
    }
}

/// Changed lines in the new version of a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Range {
    /// Lines `a..=b` are new or changed.
    Lines(u32, u32),
    /// Lines were removed right after line `c` (nothing added).
    After(u32),
}

/// What one diff says about a file.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct FileDiff {
    pub ranges: Vec<Range>,
    /// Removed or changed lines in the old version.
    pub old_ranges: Vec<(u32, u32)>,
    pub added: bool,
    pub deleted: bool,
    pub renamed_from: Option<String>,
}

/// Parses `git diff -U0` output (with `a/` and `b/` prefixes) per path: new paths, or the
/// old path for deleted files. Hunk bodies are skipped by their line counts, so code that
/// looks like a header (`+++ x`, `--- x`) is not taken for one.
pub fn parse_diff(text: &str) -> BTreeMap<String, FileDiff> {
    let mut files: BTreeMap<String, FileDiff> = BTreeMap::new();
    let mut old: Option<String> = None;
    let mut current: Option<String> = None;
    let mut rename_from: Option<String> = None;
    let (mut old_left, mut new_left) = (0u32, 0u32);
    for line in text.lines() {
        if old_left > 0 || new_left > 0 {
            match line.as_bytes().first() {
                Some(b'-') => old_left = old_left.saturating_sub(1),
                Some(b'+') => new_left = new_left.saturating_sub(1),
                Some(b' ') => {
                    old_left = old_left.saturating_sub(1);
                    new_left = new_left.saturating_sub(1);
                }
                Some(b'\\') => {}
                _ => {
                    old_left = 0;
                    new_left = 0;
                }
            }
            if old_left > 0 || new_left > 0 || !line.starts_with("diff --git ") {
                continue;
            }
        }
        if line.starts_with("diff --git ") {
            old = None;
            current = None;
            rename_from = None;
        } else if let Some(p) = line.strip_prefix("rename from ") {
            rename_from = Some(p.trim_matches('"').to_string());
        } else if let Some(p) = line.strip_prefix("rename to ") {
            let new = p.trim_matches('"').to_string();
            files.entry(new.clone()).or_default().renamed_from = rename_from.clone();
            current = Some(new);
        } else if let Some(p) = line.strip_prefix("--- ") {
            old = path_of(p, "a/");
        } else if let Some(p) = line.strip_prefix("+++ ") {
            current = match path_of(p, "b/") {
                Some(new) => {
                    let entry = files.entry(new.clone()).or_default();
                    entry.added = old.is_none();
                    if entry.renamed_from.is_none() {
                        entry.renamed_from = rename_from.clone();
                    }
                    Some(new)
                }
                None => old
                    .clone()
                    .inspect(|o| files.entry(o.clone()).or_default().deleted = true),
            };
        } else if let (Some(hunk), Some(path)) = (line.strip_prefix("@@ "), current.as_ref()) {
            // `@@ -a[,b] +c[,d] @@`
            let side = |sign: char| -> (u32, u32) {
                let part = hunk
                    .split_whitespace()
                    .find(|p| p.starts_with(sign))
                    .unwrap_or("");
                let mut it = part.get(1..).unwrap_or("").split(',');
                let start = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
                let count = it.next().and_then(|s| s.parse().ok()).unwrap_or(1);
                (start, count)
            };
            let (a, b) = side('-');
            let (c, d) = side('+');
            let entry = files.get_mut(path).expect("hunk after its file header");
            if b > 0 {
                entry.old_ranges.push((a, a + b - 1));
            }
            if !entry.deleted {
                entry.ranges.push(if d == 0 {
                    Range::After(c)
                } else {
                    Range::Lines(c, c + d - 1)
                });
            }
            old_left = b;
            new_left = d;
        }
    }
    files
}

fn path_of(raw: &str, prefix: &str) -> Option<String> {
    let raw = raw.split('\t').next().unwrap_or(raw).trim();
    if raw == "/dev/null" {
        return None;
    }
    let raw = raw.trim_matches('"');
    Some(raw.strip_prefix(prefix).unwrap_or(raw).to_string())
}

/// Runs git in `root` (no optional locks, unquoted paths) and returns stdout.
fn git(root: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["-c", "core.quotePath=false"])
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .output()
        .map_err(|e| anyhow!("cannot run git ({e}): diff_impact needs git"))?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args[0],
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `git diff` for `source`, paths relative to `root` and limited to it, fixed prefixes
/// whatever the user's diff settings.
fn git_diff(root: &Path, source: &DiffSource) -> Result<String> {
    let mut args = vec![
        "diff",
        "--no-color",
        "--no-ext-diff",
        "-U0",
        "-M",
        "--relative",
        "--src-prefix=a/",
        "--dst-prefix=b/",
    ];
    let range;
    match source {
        DiffSource::WorkingTree => args.push("HEAD"),
        DiffSource::Staged => args.push("--cached"),
        DiffSource::Base(base) => {
            if base.is_empty() || base.starts_with('-') {
                bail!("`base` must be a branch, tag or commit, got `{base}`");
            }
            range = format!("{base}...HEAD");
            args.push(&range);
        }
    }
    args.extend(["--", "."]);
    git(root, &args)
}

pub fn diff_impact(
    g: &Graph,
    root: &Path,
    source: &DiffSource,
    depth: u32,
    limit: usize,
    min: Confidence,
) -> Result<Value> {
    let mut diff = parse_diff(&git_diff(root, source)?);
    if *source == DiffSource::WorkingTree {
        // Untracked files are uncommitted too (and the index includes them).
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
        for path in untracked.split('\0').filter(|p| !p.is_empty()) {
            let entry = diff.entry(path.to_string()).or_default();
            entry.added = true;
            entry.ranges = vec![Range::Lines(1, u32::MAX)];
        }
    }
    // The old version, to tell which symbols were removed.
    let old_rev = match source {
        DiffSource::Base(base) => git(root, &["merge-base", base, "HEAD"])?.trim().to_string(),
        _ => "HEAD".to_string(),
    };
    let old_source = |path: &str| git(root, &["show", &format!("{old_rev}:./{path}")]).ok();
    let mut v = impact_of(g, source, &diff, &old_source, depth, limit, min);

    if let DiffSource::Base(_) = source {
        // The graph reflects the working tree: uncommitted edits shift its lines.
        let dirty = git(
            root,
            &["diff", "--name-only", "--relative", "HEAD", "--", "."],
        )?;
        let overlap: Vec<&str> = dirty.lines().filter(|p| diff.contains_key(*p)).collect();
        if !overlap.is_empty() {
            v["warning"] = json!(format!(
                "uncommitted edits in {}: changed lines are matched against the working tree, \
                 so symbols there may be off",
                overlap.join(", ")
            ));
        }
    }
    Ok(v)
}

/// The analysis on a parsed diff. `old_source` returns the old content of a path.
pub fn impact_of(
    g: &Graph,
    source: &DiffSource,
    diff: &BTreeMap<String, FileDiff>,
    old_source: &dyn Fn(&str) -> Option<String>,
    depth: u32,
    limit: usize,
    min: Confidence,
) -> Value {
    let mut changed: BTreeSet<usize> = BTreeSet::new();
    let mut not_indexed = Vec::new();
    let mut added = Vec::new();
    let mut deleted = Vec::new();
    let mut renamed = Vec::new();
    let mut removed: Vec<String> = Vec::new();
    for (path, fd) in diff {
        if let Some(from) = &fd.renamed_from {
            renamed.push(format!("{from} → {path}"));
        }
        removed.extend(removed_symbols(g, path, fd, old_source));
        if fd.deleted {
            deleted.push(path.as_str());
            continue;
        }
        if fd.added {
            added.push(path.as_str());
        }
        let Some(&file) = g.by_file.get(path) else {
            not_indexed.push(path.as_str());
            continue;
        };
        let syms: Vec<usize> = (0..g.nodes.len())
            .filter(|&i| g.nodes[i].file == *path && g.nodes[i].kind != SymKind::File)
            .collect();
        for range in &fd.ranges {
            let mut nodes = changed_in(g, file, &syms, *range);
            // In a new file everything is new: its symbols, not its top-level gaps, matter.
            if fd.added {
                nodes.retain(|&n| n != file);
            }
            changed.extend(nodes);
        }
    }

    let changed_list: Vec<usize> = changed.iter().copied().collect();
    let seeds = query::seeds_with_overrides(g, &changed_list, min);
    let hits = query::dependents(g, &seeds, depth, min);
    let by_file = query::dependents_by_file(g, &hits, limit);

    // Communities and modules touched by the change or its impact, most nodes first.
    let touched: Vec<usize> = changed
        .iter()
        .copied()
        .chain(hits.iter().map(|h| h.0))
        .collect();
    let rank = |keys: Vec<String>| -> Vec<String> {
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        for k in keys {
            *counts.entry(k).or_default() += 1;
        }
        let mut v: Vec<(String, usize)> = counts.into_iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        v.into_iter().map(|(k, _)| k).collect()
    };
    let communities = rank(
        touched
            .iter()
            .filter_map(|&n| g.nodes[n].community)
            .filter_map(|c| g.communities.get(c as usize))
            .map(|c| c.label.clone())
            .collect(),
    );
    let modules = rank(
        touched
            .iter()
            .map(|&n| query::component(&g.nodes[n].file, 2))
            .collect(),
    );

    let mut changed_by_file: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for &c in &changed {
        let n = &g.nodes[c];
        let label = if n.kind == SymKind::File {
            "(top-level code)".to_string()
        } else {
            format!(
                "{} ({}) L{}-{}",
                g.qualified_name(c),
                n.kind.as_str(),
                n.line,
                n.end_line
            )
        };
        changed_by_file
            .entry(n.file.as_str())
            .or_default()
            .push(label);
    }

    let mut out = json!({
        "diff": source.describe(),
        "changed_files": diff.len(),
        "changed_symbols": changed.iter().filter(|&&c| g.nodes[c].kind != SymKind::File).count(),
        "changed": changed_by_file,
    });
    let lists: [(&str, Vec<String>); 5] = [
        ("new_files", added.iter().map(|s| s.to_string()).collect()),
        (
            "deleted_files",
            deleted.iter().map(|s| s.to_string()).collect(),
        ),
        ("renamed_files", renamed),
        ("removed_symbols", removed),
        (
            "not_indexed",
            not_indexed.iter().map(|s| s.to_string()).collect(),
        ),
    ];
    let mut gone = false;
    for (key, list) in lists {
        if !list.is_empty() {
            gone |= matches!(key, "deleted_files" | "renamed_files" | "removed_symbols");
            out[key] = json!(list);
        }
    }
    if gone {
        out["note"] = json!(
            "removed or moved code has no node in the updated graph, so its callers are not \
             listed: search its names (search_symbols, grep) for code that may break"
        );
    }
    out["communities"] = json!(communities);
    out["modules"] = json!(modules);
    out["affected_symbols"] = json!(
        hits.iter()
            .filter(|h| g.nodes[h.0].kind != SymKind::File)
            .count()
    );
    out["affected_files"] = json!(by_file.len());
    out["truncated"] = json!(hits.len() > limit);
    out["by_file"] = json!(by_file);
    out
}

/// Nodes a changed range belongs to: each symbol (or the file, for top-level code) with
/// changed lines not covered by one of its nested symbols. A pure deletion belongs to the
/// innermost symbol around both of its sides (none between symbols).
fn changed_in(g: &Graph, file: usize, syms: &[usize], range: Range) -> Vec<usize> {
    let span = |n: usize| (g.nodes[n].line, g.nodes[n].end_line);
    match range {
        Range::After(c) => syms
            .iter()
            .copied()
            .filter(|&s| span(s).0 <= c && c < span(s).1)
            .max_by_key(|&s| (span(s).0, std::cmp::Reverse(span(s).1)))
            .into_iter()
            .collect(),
        Range::Lines(a, b) => {
            let overlapping: Vec<usize> = syms
                .iter()
                .copied()
                .filter(|&s| span(s).0 <= b && a <= span(s).1)
                .collect();
            let mut out = Vec::new();
            for owner in std::iter::once(file).chain(overlapping.iter().copied()) {
                let (lo, hi) = if owner == file {
                    (a, b.min(span(file).1))
                } else {
                    (a.max(span(owner).0), b.min(span(owner).1))
                };
                // Direct children (symbols whose parent is `owner`) inside [lo, hi].
                let mut kids: Vec<(u32, u32)> = overlapping
                    .iter()
                    .filter(|&&s| g.nodes[s].parent == Some(owner))
                    .map(|&s| (span(s).0.max(lo), span(s).1.min(hi)))
                    .collect();
                kids.sort_unstable();
                let mut next = lo;
                for (s, e) in kids {
                    if s > next {
                        break;
                    }
                    next = next.max(e.saturating_add(1));
                }
                if next <= hi {
                    out.push(owner);
                }
            }
            out
        }
    }
}

/// Symbols of the old version of a file touched by removed lines that no longer exist in
/// the file (or in the renamed file): `path::Name (kind)`.
fn removed_symbols(
    g: &Graph,
    path: &str,
    fd: &FileDiff,
    old_source: &dyn Fn(&str) -> Option<String>,
) -> Vec<String> {
    if fd.old_ranges.is_empty() {
        return Vec::new();
    }
    let old_path = fd.renamed_from.as_deref().unwrap_or(path);
    let Some(lang) = Lang::from_path(Path::new(old_path)) else {
        return Vec::new();
    };
    let Some(text) = old_source(old_path) else {
        return Vec::new();
    };
    let Ok(facts) = extract::extract(old_path, lang, text.as_bytes()) else {
        return Vec::new();
    };
    let still_there: BTreeSet<&str> = if fd.deleted {
        BTreeSet::new()
    } else {
        g.nodes
            .iter()
            .filter(|n| n.file == path)
            .map(|n| n.name.as_str())
            .collect()
    };
    let mut out: Vec<String> = facts
        .defs
        .iter()
        .filter(|d| {
            fd.old_ranges
                .iter()
                .any(|&(a, b)| d.start_line <= b && a <= d.end_line)
                && !still_there.contains(d.name.as_str())
        })
        .map(|d| {
            // Qualify by the innermost definition around it (a method's class).
            let owner = facts
                .defs
                .iter()
                .filter(|o| {
                    o.start_byte <= d.start_byte
                        && d.end_byte <= o.end_byte
                        && (o.start_byte, o.end_byte) != (d.start_byte, d.end_byte)
                })
                .max_by_key(|o| o.start_byte);
            let (name, kind) = match owner {
                Some(o) if o.kind.is_type() && d.kind == SymKind::Function => {
                    (format!("{}::{}", o.name, d.name), SymKind::Method)
                }
                Some(o) => (format!("{}::{}", o.name, d.name), d.kind),
                None => (d.name.clone(), d.kind),
            };
            format!("{old_path}::{name} ({})", kind.as_str())
        })
        .collect();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hunks_new_deleted_and_renamed_files() {
        let text = "diff --git a/src/a.py b/src/a.py\n\
index 1..2 100644\n\
--- a/src/a.py\n\
+++ b/src/a.py\n\
@@ -3,0 +4,2 @@ def f():\n\
+x\n\
+++ looks like a header\n\
@@ -10 +12 @@\n\
-a\n\
+b\n\
@@ -20,3 +21,0 @@\n\
-p\n\
--- also not a header\n\
-r\n\
diff --git a/new.py b/new.py\n\
new file mode 100644\n\
--- /dev/null\n\
+++ b/new.py\n\
@@ -0,0 +1,3 @@\n\
+1\n\
+2\n\
+3\n\
diff --git a/old.py b/old.py\n\
deleted file mode 100644\n\
--- a/old.py\n\
+++ /dev/null\n\
@@ -1,2 +0,0 @@\n\
-a\n\
-b\n\
diff --git a/x.py b/y.py\n\
similarity index 90%\n\
rename from x.py\n\
rename to y.py\n\
--- a/x.py\n\
+++ b/y.py\n\
@@ -1 +1 @@\n\
-a\n\
+b\n\
diff --git a/m.py b/n.py\n\
similarity index 100%\n\
rename from m.py\n\
rename to n.py\n";
        let d = parse_diff(text);
        let a = &d["src/a.py"];
        assert_eq!(
            a.ranges,
            vec![Range::Lines(4, 5), Range::Lines(12, 12), Range::After(21)]
        );
        assert_eq!(a.old_ranges, vec![(10, 10), (20, 22)]);
        assert!(!a.added && !a.deleted);
        assert!(!d.contains_key("looks like a header") && !d.contains_key("also not a header"));
        assert_eq!(d["new.py"].ranges, vec![Range::Lines(1, 3)]);
        assert!(d["new.py"].added);
        assert!(d["old.py"].deleted && d["old.py"].ranges.is_empty());
        assert_eq!(d["old.py"].old_ranges, vec![(1, 2)]);
        assert_eq!(d["y.py"].renamed_from.as_deref(), Some("x.py"));
        assert_eq!(d["n.py"].renamed_from.as_deref(), Some("m.py"));
        assert!(!d.contains_key("x.py"));
    }
}
