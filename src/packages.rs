//! Local packages of a repository (npm workspaces, Cargo crates), so imports of one
//! package from another resolve to its files instead of an external package.

use std::fs;
use std::path::Path;

use serde_json::Value;

use crate::indexer;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageKind {
    Npm,
    Crate,
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
}

/// Every package.json and Cargo.toml with a name, outside ignored and dependency folders.
pub fn discover(root: &Path) -> Vec<Package> {
    let mut out = Vec::new();
    for entry in indexer::walk_files(root) {
        let name = entry.file_name().to_string_lossy();
        let parse = match name.as_ref() {
            "package.json" => npm,
            "Cargo.toml" => cargo,
            _ => continue,
        };
        let Ok(rel) = entry.path().strip_prefix(root) else {
            continue;
        };
        let dir = rel
            .parent()
            .map(|d| d.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        if let Ok(text) = fs::read_to_string(entry.path())
            && let Some(p) = parse(&text, &dir)
        {
            out.push(p);
        }
    }
    out.sort_by(|a, b| a.dir.cmp(&b.dir).then(a.name.cmp(&b.name)));
    out
}

/// Names of the packages of a repository (for list_projects).
pub fn names(root: &Path) -> Vec<String> {
    discover(root).into_iter().map(|p| p.name).collect()
}

fn join(dir: &str, rel: &str) -> String {
    let rel = rel.trim_start_matches("./");
    if dir.is_empty() {
        rel.to_string()
    } else {
        format!("{dir}/{rel}")
    }
}

/// `name` and the entry candidates of a package.json: `exports["."]` (types, import,
/// module, default, require), `module`, `main`, `types`, then `src/index`, `index`. Built
/// outputs (`dist/`, `build/`, `lib/`) also map to `src/`, since only sources are indexed.
fn npm(text: &str, dir: &str) -> Option<Package> {
    let v: Value = serde_json::from_str(text).ok()?;
    let name = v.get("name")?.as_str()?.to_string();
    let text = |x: Option<&Value>| x.and_then(Value::as_str).map(str::to_string);
    let mut raw: Vec<String> = Vec::new();
    match v.get("exports") {
        Some(Value::String(s)) => raw.push(s.clone()),
        Some(Value::Object(map)) => match map.get(".").unwrap_or(&v["exports"]) {
            Value::String(s) => raw.push(s.clone()),
            Value::Object(cond) => {
                for key in ["types", "import", "module", "default", "require"] {
                    raw.extend(text(cond.get(key)));
                }
            }
            _ => {}
        },
        _ => {}
    }
    for key in ["module", "main", "types", "typings"] {
        raw.extend(text(v.get(key)));
    }
    let mut entries = Vec::new();
    for r in raw {
        let r = r.trim_start_matches("./").to_string();
        entries.push(join(dir, &r));
        for built in ["dist/", "build/", "lib/", "out/"] {
            if let Some(rest) = r.strip_prefix(built) {
                entries.push(join(dir, &format!("src/{rest}")));
            }
        }
    }
    for fallback in ["src/index", "index", "lib/index"] {
        entries.push(join(dir, fallback));
    }
    Some(Package {
        kind: PackageKind::Npm,
        name,
        dir: dir.to_string(),
        entries,
    })
}

/// The `[package]` name and `[lib] path` (default `src/lib.rs`) of a Cargo.toml.
fn cargo(text: &str, dir: &str) -> Option<Package> {
    let mut section = "";
    let mut name = None;
    let mut lib = None;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            section = line;
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim().trim_matches('"').to_string();
        match (section, key.trim()) {
            ("[package]", "name") => name = Some(value),
            ("[lib]", "path") => lib = Some(value),
            ("[lib]", "name") => name = Some(value),
            _ => {}
        }
    }
    let name = name?.replace('-', "_");
    Some(Package {
        kind: PackageKind::Crate,
        name,
        dir: dir.to_string(),
        entries: vec![join(dir, lib.as_deref().unwrap_or("src/lib.rs"))],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_npm_entries() {
        let p = npm(
            r#"{"name":"@acme/utils","main":"dist/index.js","exports":{".":{"types":"./src/index.ts","import":"./dist/index.mjs"}}}"#,
            "packages/utils",
        )
        .unwrap();
        assert_eq!(p.name, "@acme/utils");
        assert_eq!(
            &p.entries[..4],
            [
                "packages/utils/src/index.ts",
                "packages/utils/dist/index.mjs",
                "packages/utils/src/index.mjs",
                "packages/utils/dist/index.js"
            ]
        );
        assert!(p.entries.contains(&"packages/utils/src/index".to_string()));
        assert!(
            npm(r#"{"private":true}"#, "").is_none(),
            "no name, no package"
        );
    }

    #[test]
    fn reads_cargo_manifests() {
        let p = cargo(
            "[package]\nname = \"acme-core\"\nversion = \"0.1.0\"\n\n[dependencies]\nname = \"x\"\n",
            "crates/core",
        )
        .unwrap();
        assert_eq!(p.name, "acme_core");
        assert_eq!(p.entries, ["crates/core/src/lib.rs"]);
        let p = cargo("[package]\nname = \"a\"\n[lib]\npath = \"lib/a.rs\"\n", "").unwrap();
        assert_eq!(p.entries, ["lib/a.rs"]);
        assert!(cargo("[workspace]\nmembers = [\"crates/*\"]\n", "").is_none());
    }
}
