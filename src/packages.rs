//! Local packages of a repository (npm workspaces, Cargo crates), so imports of one
//! package from another resolve to its files instead of an external package.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Manifest files package discovery reads (and whose changes make an index stale).
pub const MANIFESTS: &[&str] = &["package.json", "Cargo.toml", "pnpm-workspace.yaml"];

/// Directories whose packages are samples or test data unless a workspace lists them.
const NON_WORKSPACE_DIRS: &[&str] = &[
    "fixtures",
    "fixture",
    "__fixtures__",
    "testdata",
    "examples",
    "example",
    "samples",
    "test",
    "tests",
    "__tests__",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PackageKind {
    Npm,
    Crate,
}

/// A package name and its ecosystem, as kept in the graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageRef {
    pub kind: PackageKind,
    pub name: String,
}

/// A package declared in the repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Package {
    pub kind: PackageKind,
    /// As imported: `@acme/utils`; a crate's name with `-` turned into `_`.
    pub name: String,
    /// Repo-relative directory of its manifest ("" at the root).
    pub dir: String,
    /// Candidate entry files, repo-relative, best first (they may not exist).
    pub entries: Vec<String>,
    /// npm `exports` subpaths (`str` for `./str`) and their candidate files.
    pub subpaths: Vec<(String, Vec<String>)>,
}

/// Packages from the manifest files found while scanning `root` (absolute paths). When
/// the root declares workspaces (`workspaces`, pnpm-workspace.yaml, `[workspace]
/// members`), only those count; otherwise samples and test data are left out. A name
/// declared twice is dropped: an import of it cannot be resolved for sure.
pub fn from_manifests(root: &Path, manifests: &[PathBuf]) -> Vec<Package> {
    let mut npm_globs: Option<Vec<String>> = None;
    let mut crate_globs: Option<Vec<String>> = None;
    let mut found = Vec::new();
    for path in manifests {
        let Ok(rel) = path.strip_prefix(root) else {
            continue;
        };
        let dir = rel
            .parent()
            .map(|d| d.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        let Ok(text) = fs::read_to_string(path) else {
            continue;
        };
        let file = path.file_name().and_then(|f| f.to_str()).unwrap_or("");
        match file {
            "package.json" => {
                if dir.is_empty() {
                    npm_globs = npm_globs.or(npm_workspaces(&text));
                }
                found.extend(npm(&text, &dir));
            }
            "pnpm-workspace.yaml" if dir.is_empty() => npm_globs = Some(pnpm_workspaces(&text)),
            "Cargo.toml" => {
                let (package, members) = cargo(&text, &dir);
                if dir.is_empty() {
                    crate_globs = crate_globs.or(members);
                }
                found.extend(package);
            }
            _ => {}
        }
    }
    found.retain(|p| {
        let globs = match p.kind {
            PackageKind::Npm => &npm_globs,
            PackageKind::Crate => &crate_globs,
        };
        match globs {
            Some(globs) => p.dir.is_empty() || globs.iter().any(|g| glob_match(g, &p.dir)),
            None => !p.dir.split('/').any(|d| NON_WORKSPACE_DIRS.contains(&d)),
        }
    });
    let mut count: HashMap<(PackageKind, &str), usize> = HashMap::new();
    for p in &found {
        *count.entry((p.kind, p.name.as_str())).or_default() += 1;
    }
    let unique: Vec<bool> = found
        .iter()
        .map(|p| count[&(p.kind, p.name.as_str())] == 1)
        .collect();
    let mut out: Vec<Package> = found
        .into_iter()
        .zip(unique)
        .filter_map(|(p, u)| u.then_some(p))
        .collect();
    out.sort_by(|a, b| a.dir.cmp(&b.dir).then(a.name.cmp(&b.name)));
    out
}

/// `packages/*`, `apps/**`, `crates/core`: `*` matches within one path segment, `**` any
/// number of segments. Negated patterns (`!…`) are ignored.
pub fn glob_match(glob: &str, path: &str) -> bool {
    fn seg(p: &str, s: &str) -> bool {
        match p.split_once('*') {
            None => p == s,
            Some((head, tail)) => {
                s.starts_with(head)
                    && (0..=s.len() - head.len()).any(|i| {
                        s.is_char_boundary(head.len() + i) && seg(tail, &s[head.len() + i..])
                    })
            }
        }
    }
    fn go(g: &[&str], p: &[&str]) -> bool {
        match (g.first(), p.first()) {
            (None, None) => true,
            (Some(&"**"), _) => go(&g[1..], p) || (!p.is_empty() && go(g, &p[1..])),
            (Some(gs), Some(ps)) => seg(gs, ps) && go(&g[1..], &p[1..]),
            _ => false,
        }
    }
    let glob = glob.trim().trim_start_matches("./").trim_end_matches('/');
    if glob.starts_with('!') {
        return false;
    }
    let g: Vec<&str> = glob.split('/').filter(|s| !s.is_empty()).collect();
    let p: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    go(&g, &p)
}

fn join(dir: &str, rel: &str) -> String {
    let rel = rel.trim_start_matches("./");
    if dir.is_empty() {
        rel.to_string()
    } else {
        format!("{dir}/{rel}")
    }
}

/// A target and, for built outputs (`dist/`, `build/`, `lib/`, `out/`), the same path
/// under `src/`, since only sources are indexed.
fn with_sources(dir: &str, target: &str) -> Vec<String> {
    let t = target.trim_start_matches("./");
    let mut out = vec![join(dir, t)];
    for built in ["dist/", "build/", "lib/", "out/"] {
        if let Some(rest) = t.strip_prefix(built) {
            out.push(join(dir, &format!("src/{rest}")));
        }
    }
    out
}

/// The files an `exports` value points to: a path, or conditions (types, import, module,
/// default, require), nested.
fn export_targets(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::String(s) => out.push(s.clone()),
        Value::Object(cond) => {
            for key in ["types", "import", "module", "default", "require"] {
                if let Some(x) = cond.get(key) {
                    export_targets(x, out);
                }
            }
        }
        _ => {}
    }
}

fn npm_workspaces(text: &str) -> Option<Vec<String>> {
    let v: Value = serde_json::from_str(text).ok()?;
    let ws = v.get("workspaces")?;
    let list = ws
        .as_array()
        .or_else(|| ws.get("packages").and_then(Value::as_array))?;
    Some(
        list.iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
    )
}

/// `packages:` entries of a pnpm-workspace.yaml (`- 'packages/*'`).
fn pnpm_workspaces(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut inside = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if !line.starts_with([' ', '\t', '-']) && !trimmed.is_empty() {
            inside = trimmed.starts_with("packages:");
            continue;
        }
        if inside && let Some(item) = trimmed.strip_prefix('-') {
            out.push(item.trim().trim_matches(['\'', '"']).to_string());
        }
    }
    out
}

/// `name`, entry candidates (`exports["."]`, `module`, `main`, `types`, then `src/index`,
/// `index`) and `exports` subpaths of a package.json.
fn npm(text: &str, dir: &str) -> Option<Package> {
    let v: Value = serde_json::from_str(text).ok()?;
    let name = v.get("name")?.as_str()?.to_string();
    let mut raw: Vec<String> = Vec::new();
    let mut subpaths = Vec::new();
    match v.get("exports") {
        Some(Value::Object(map)) if map.keys().any(|k| k.starts_with('.')) => {
            for (key, target) in map {
                let mut targets = Vec::new();
                export_targets(target, &mut targets);
                if key == "." {
                    raw.extend(targets);
                } else if let Some(sub) = key.strip_prefix("./") {
                    let files = targets.iter().flat_map(|t| with_sources(dir, t)).collect();
                    subpaths.push((sub.to_string(), files));
                }
            }
        }
        Some(exports) => export_targets(exports, &mut raw),
        None => {}
    }
    for key in ["module", "main", "types", "typings"] {
        if let Some(s) = v.get(key).and_then(Value::as_str) {
            raw.push(s.to_string());
        }
    }
    let mut entries: Vec<String> = raw.iter().flat_map(|r| with_sources(dir, r)).collect();
    for fallback in ["src/index", "index", "lib/index"] {
        entries.push(join(dir, fallback));
    }
    Some(Package {
        kind: PackageKind::Npm,
        name,
        dir: dir.to_string(),
        entries,
        subpaths,
    })
}

/// A TOML line without its comment (a `#` outside quotes), trimmed.
fn strip_comment(line: &str) -> &str {
    let mut quote = None;
    for (i, c) in line.char_indices() {
        match (quote, c) {
            (None, '"' | '\'') => quote = Some(c),
            (Some(q), c) if c == q => quote = None,
            (None, '#') => return line[..i].trim(),
            _ => {}
        }
    }
    line.trim()
}

/// The quoted strings of a TOML value (`"a"`, `['a', "b"]`).
fn quoted(value: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = value;
    while let Some(start) = rest.find(['"', '\'']) {
        let q = rest[start..].chars().next().unwrap();
        let after = &rest[start + 1..];
        let Some(end) = after.find(q) else { break };
        out.push(after[..end].to_string());
        rest = &after[end + 1..];
    }
    out
}

/// The crate of a Cargo.toml (`[lib] name` over `[package] name`, `-` as `_`; `[lib]
/// path` or `src/lib.rs`) and its `[workspace] members`.
fn cargo(text: &str, dir: &str) -> (Option<Package>, Option<Vec<String>>) {
    let mut section = String::new();
    let (mut package_name, mut lib_name, mut lib_path) = (None, None, None);
    let mut members: Option<Vec<String>> = None;
    let mut collecting_members = false;
    for line in text.lines() {
        let line = strip_comment(line);
        if collecting_members {
            members.get_or_insert_with(Vec::new).extend(quoted(line));
            collecting_members = !line.contains(']');
            continue;
        }
        if line.starts_with('[') {
            section = line[..line.find(']').map_or(line.len(), |i| i + 1)].to_string();
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        let first = quoted(value).into_iter().next();
        match (section.as_str(), key.trim()) {
            ("[package]", "name") => package_name = first,
            ("[lib]", "name") => lib_name = first,
            ("[lib]", "path") => lib_path = first,
            ("[workspace]", "members") => {
                members = Some(quoted(value));
                collecting_members = value.starts_with('[') && !value.contains(']');
            }
            _ => {}
        }
    }
    let package = lib_name.or(package_name).map(|name| Package {
        kind: PackageKind::Crate,
        name: name.replace('-', "_"),
        dir: dir.to_string(),
        entries: vec![join(dir, lib_path.as_deref().unwrap_or("src/lib.rs"))],
        subpaths: Vec::new(),
    });
    (package, members)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_npm_entries_and_subpaths() {
        let p = npm(
            r#"{"name":"@acme/utils","main":"dist/index.js","exports":{".":{"types":"./src/index.ts","import":"./dist/index.mjs"},"./str":"./dist/strings/index.js"}}"#,
            "packages/utils",
        )
        .unwrap();
        assert_eq!(p.name, "@acme/utils");
        assert_eq!(
            &p.entries[..3],
            [
                "packages/utils/src/index.ts",
                "packages/utils/dist/index.mjs",
                "packages/utils/src/index.mjs"
            ]
        );
        assert_eq!(
            p.subpaths,
            [(
                "str".to_string(),
                vec![
                    "packages/utils/dist/strings/index.js".to_string(),
                    "packages/utils/src/strings/index.js".to_string()
                ]
            )]
        );
        assert!(
            npm(r#"{"private":true}"#, "").is_none(),
            "no name, no package"
        );
    }

    #[test]
    fn reads_cargo_manifests() {
        let (p, _) = cargo(
            "[package]  # meta\nname = 'acme-core' # the core\n\n[dependencies]\nname = \"x\"\n",
            "crates/core",
        );
        let p = p.unwrap();
        assert_eq!(p.name, "acme_core");
        assert_eq!(p.entries, ["crates/core/src/lib.rs"]);
        let (p, _) = cargo(
            "[lib]\nname = \"core2\"\npath = \"lib/a.rs\"\n[package]\nname = \"a\"\n",
            "",
        );
        let p = p.unwrap();
        assert_eq!(
            (p.name.as_str(), p.entries[0].as_str()),
            ("core2", "lib/a.rs")
        );
        let (p, members) = cargo(
            "[workspace]\nmembers = [\n  \"crates/*\",\n  \"tools/x\", # tool\n]\n",
            "",
        );
        assert!(p.is_none());
        assert_eq!(members.unwrap(), ["crates/*", "tools/x"]);
    }

    #[test]
    fn globs() {
        assert!(glob_match("packages/*", "packages/utils"));
        assert!(!glob_match("packages/*", "packages/utils/inner"));
        assert!(glob_match("apps/**", "apps/web/inner"));
        assert!(glob_match("./crates/core", "crates/core"));
        assert!(glob_match("pkg-*", "pkg-a"));
        assert!(!glob_match("!packages/*", "packages/a"));
        assert_eq!(
            pnpm_workspaces("packages:\n  - 'packages/*'\n  - \"apps/*\"\nother: 1\n"),
            ["packages/*", "apps/*"]
        );
    }
}
