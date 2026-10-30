//! Impact of local changes or of a branch: reads `git diff`, maps the changed lines to the
//! innermost symbols containing them, and finds everything that depends on those symbols.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};

use crate::graph::{Confidence, Graph, SymKind};
use crate::query;

/// Which changes to analyse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffSource {
    /// Every uncommitted change (staged or not) against HEAD.
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

/// What one diff says about a file: changed line ranges in the new version.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct FileDiff {
    pub ranges: Vec<(u32, u32)>,
    pub added: bool,
    pub deleted: bool,
}

/// Parses `git diff -U0` output into new-side line ranges per path.
pub fn parse_diff(text: &str) -> BTreeMap<String, FileDiff> {
    let mut files: BTreeMap<String, FileDiff> = BTreeMap::new();
    let mut old: Option<String> = None;
    let mut current: Option<String> = None;
    for line in text.lines() {
        if line.starts_with("diff --git ") {
            old = None;
            current = None;
        } else if let Some(p) = line.strip_prefix("--- ") {
            old = path_of(p, "a/");
        } else if let Some(p) = line.strip_prefix("+++ ") {
            match path_of(p, "b/") {
                Some(new) => {
                    let entry = files.entry(new.clone()).or_default();
                    entry.added = old.is_none();
                    current = Some(new);
                }
                // Deleted: the old path has no new lines.
                None => {
                    if let Some(o) = old.clone() {
                        files.entry(o).or_default().deleted = true;
                    }
                    current = None;
                }
            }
        } else if let (Some(hunk), Some(path)) = (line.strip_prefix("@@ "), current.as_ref()) {
            // `@@ -a[,b] +c[,d] @@`: lines c..c+d-1 are new; d = 0 is a pure deletion just
            // after line c, which still changes the code around it.
            let Some(new) = hunk.split_whitespace().find(|p| p.starts_with('+')) else {
                continue;
            };
            let mut parts = new[1..].split(',');
            let start: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
            let count: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(1);
            let range = if count == 0 {
                (start.max(1), start.max(1))
            } else {
                (start, start + count - 1)
            };
            files.get_mut(path).unwrap().ranges.push(range);
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

/// `git diff` for `source`, with paths relative to `root` and limited to it.
fn git_diff(root: &Path, source: &DiffSource) -> Result<String> {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(root)
        .args(["diff", "--no-color", "--no-ext-diff", "-U0", "--relative"])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null());
    match source {
        DiffSource::WorkingTree => {
            cmd.arg("HEAD");
        }
        DiffSource::Staged => {
            cmd.arg("--cached");
        }
        DiffSource::Base(base) => {
            if base.starts_with('-') || base.is_empty() {
                bail!("`base` must be a branch, tag or commit, got `{base}`");
            }
            cmd.arg(format!("{base}...HEAD"));
        }
    }
    cmd.args(["--", "."]);
    let out = cmd
        .output()
        .map_err(|e| anyhow!("cannot run git ({e}): diff_impact needs git"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        bail!("git diff failed: {}", err.trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

pub fn diff_impact(
    g: &Graph,
    root: &Path,
    source: &DiffSource,
    depth: u32,
    limit: usize,
    min: Confidence,
) -> Result<Value> {
    let diff = parse_diff(&git_diff(root, source)?);
    Ok(impact_of(g, source, &diff, depth, limit, min))
}

/// The analysis on an already parsed diff.
pub fn impact_of(
    g: &Graph,
    source: &DiffSource,
    diff: &BTreeMap<String, FileDiff>,
    depth: u32,
    limit: usize,
    min: Confidence,
) -> Value {
    // Symbols of the changed files, to find the innermost one around each changed line.
    let mut symbols: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, n) in g.nodes.iter().enumerate() {
        if n.kind != SymKind::File && n.kind != SymKind::External && diff.contains_key(&n.file) {
            symbols.entry(n.file.as_str()).or_default().push(i);
        }
    }
    let mut changed: BTreeSet<usize> = BTreeSet::new();
    let mut not_indexed = Vec::new();
    let mut added = Vec::new();
    let mut deleted = Vec::new();
    for (path, fd) in diff {
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
        let syms = symbols.get(path.as_str()).map_or(&[][..], Vec::as_slice);
        for &(start, end) in &fd.ranges {
            for line in start..=end {
                let innermost = syms
                    .iter()
                    .copied()
                    .filter(|&s| g.nodes[s].line <= line && line <= g.nodes[s].end_line)
                    .max_by_key(|&s| (g.nodes[s].line, std::cmp::Reverse(g.nodes[s].end_line)));
                changed.insert(innermost.unwrap_or(file));
            }
        }
    }

    // Seeds: the changed nodes and the interface/base methods they override (callers
    // through an interface reach them at runtime).
    let mut seeds: Vec<(usize, Confidence)> =
        changed.iter().map(|&c| (c, Confidence::Exact)).collect();
    for &c in &changed {
        for (m, conf) in query::overridden_members(g, c, min) {
            if !seeds.iter().any(|&(s, _)| s == m) {
                seeds.push((m, conf));
            }
        }
    }
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
    for (key, list) in [
        ("new_files", &added),
        ("deleted_files", &deleted),
        ("not_indexed", &not_indexed),
    ] {
        if !list.is_empty() {
            out[key] = json!(list);
        }
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
+y\n\
@@ -10 +12 @@\n\
-a\n\
+b\n\
@@ -20,3 +21,0 @@\n\
diff --git a/new.py b/new.py\n\
new file mode 100644\n\
--- /dev/null\n\
+++ b/new.py\n\
@@ -0,0 +1,3 @@\n\
diff --git a/old.py b/old.py\n\
deleted file mode 100644\n\
--- a/old.py\n\
+++ /dev/null\n\
@@ -1,2 +0,0 @@\n\
diff --git a/x.py b/y.py\n\
rename from x.py\n\
rename to y.py\n\
--- a/x.py\n\
+++ b/y.py\n\
@@ -1 +1 @@\n";
        let d = parse_diff(text);
        assert_eq!(d["src/a.py"].ranges, vec![(4, 5), (12, 12), (21, 21)]);
        assert!(!d["src/a.py"].added && !d["src/a.py"].deleted);
        assert_eq!(
            d["new.py"],
            FileDiff {
                ranges: vec![(1, 3)],
                added: true,
                deleted: false
            }
        );
        assert_eq!(
            d["old.py"],
            FileDiff {
                ranges: vec![],
                added: false,
                deleted: true
            }
        );
        assert_eq!(d["y.py"].ranges, vec![(1, 1)]);
        assert!(!d.contains_key("x.py"));
    }
}
