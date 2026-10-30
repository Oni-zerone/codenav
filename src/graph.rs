//! The code graph: nodes (files and symbols), typed edges, persistence and lookup indexes.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::communities::Community;
use crate::lang::Lang;

pub const INDEX_DIR: &str = ".codenav";
const GRAPH_FILE: &str = "graph.cnav";
/// Name used before the switch to `.cnav`; removed on save so stale copies don't linger.
const LEGACY_GRAPH_FILE: &str = "graph.json";
/// v2: communities (`Node::community`, `Graph::communities`).
/// v3: `Edge::confidence` replaces the `ambiguous` flag.
const FORMAT_VERSION: u32 = 3;
/// Version of what the indexer extracts and how it resolves names. Bump it when the
/// same sources would give a different graph: older indexes are then stale and rebuilt,
/// while readers of the format (like the VS Code extension) are unaffected.
/// 1: JS/TS module scoping, JSX components only, top-level constants.
/// 2: `Edge::lines` (every line of a relation, e.g. call sites).
pub const ANALYSIS_VERSION: u32 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum SymKind {
    File,
    External,
    Module,
    Class,
    Struct,
    Enum,
    Interface,
    Type,
    Function,
    Method,
    Constant,
    Macro,
}

impl SymKind {
    pub fn parse(s: &str) -> Option<SymKind> {
        Some(match s {
            "file" => SymKind::File,
            "external" => SymKind::External,
            "module" => SymKind::Module,
            "class" => SymKind::Class,
            "struct" => SymKind::Struct,
            "enum" => SymKind::Enum,
            "interface" | "trait" | "protocol" => SymKind::Interface,
            "type" => SymKind::Type,
            "function" => SymKind::Function,
            "method" => SymKind::Method,
            "constant" => SymKind::Constant,
            "macro" => SymKind::Macro,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            SymKind::File => "file",
            SymKind::External => "external",
            SymKind::Module => "module",
            SymKind::Class => "class",
            SymKind::Struct => "struct",
            SymKind::Enum => "enum",
            SymKind::Interface => "interface",
            SymKind::Type => "type",
            SymKind::Function => "function",
            SymKind::Method => "method",
            SymKind::Constant => "constant",
            SymKind::Macro => "macro",
        }
    }

    /// Used to pick a winner when one syntax node matches several patterns.
    /// Which kind wins when one node matches several patterns: a JS `const f = () => {}`
    /// is a function, not a constant.
    pub fn specificity(self) -> u8 {
        match self {
            SymKind::Type => 0,
            SymKind::Constant => 1,
            SymKind::Function => 2,
            _ => 3,
        }
    }

    pub fn is_type(self) -> bool {
        matches!(
            self,
            SymKind::Class | SymKind::Struct | SymKind::Enum | SymKind::Interface | SymKind::Type
        )
    }

    pub fn is_callable(self) -> bool {
        matches!(
            self,
            SymKind::Function
                | SymKind::Method
                | SymKind::Macro
                | SymKind::Class
                | SymKind::Struct
                | SymKind::Enum
        )
    }
}

/// How sure the resolver is about an edge's target (resolution is name-based).
/// Ordered from least to most certain, so `a >= min` means "at least as sure as `min`".
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord,
)]
#[serde(rename_all = "lowercase")]
pub enum Confidence {
    /// One of several same-named candidates, or a common method on a value of unknown type.
    Ambiguous,
    /// A unique candidate found by proximity (same directory or crate, or the only one in
    /// the repository), or a method called on a value of unknown type.
    Inferred,
    /// Structural (containment, imports), or a unique candidate in the same file, an
    /// imported file, or named by a `self`/type/module qualifier.
    #[default]
    Exact,
}

impl Confidence {
    pub const ALL: [Confidence; 3] = [
        Confidence::Exact,
        Confidence::Inferred,
        Confidence::Ambiguous,
    ];

    pub fn parse(s: &str) -> Option<Confidence> {
        Some(match s {
            "exact" => Confidence::Exact,
            "inferred" => Confidence::Inferred,
            "ambiguous" => Confidence::Ambiguous,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Confidence::Exact => "exact",
            Confidence::Inferred => "inferred",
            Confidence::Ambiguous => "ambiguous",
        }
    }

    pub fn is_exact(&self) -> bool {
        *self == Confidence::Exact
    }

    /// The name to show next to an edge or a result, or `None` when exact (the default).
    pub fn label(self) -> Option<&'static str> {
        (!self.is_exact()).then(|| self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum EdgeKind {
    /// file -> symbol, type -> member
    Contains,
    /// file -> file / external package
    Imports,
    /// symbol -> symbol
    Calls,
    /// type -> supertype / trait / protocol
    Inherits,
    /// symbol -> type used in its signature or body
    References,
}

impl EdgeKind {
    pub fn parse(s: &str) -> Option<EdgeKind> {
        Some(match s {
            "contains" => EdgeKind::Contains,
            "imports" => EdgeKind::Imports,
            "calls" => EdgeKind::Calls,
            "inherits" | "implements" => EdgeKind::Inherits,
            "references" | "uses" => EdgeKind::References,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            EdgeKind::Contains => "contains",
            EdgeKind::Imports => "imports",
            EdgeKind::Calls => "calls",
            EdgeKind::Inherits => "inherits",
            EdgeKind::References => "references",
        }
    }

    /// Phrase for the reverse direction, used when describing incoming edges.
    pub fn inverse_str(self) -> &'static str {
        match self {
            EdgeKind::Contains => "contained_by",
            EdgeKind::Imports => "imported_by",
            EdgeKind::Calls => "called_by",
            EdgeKind::Inherits => "inherited_by",
            EdgeKind::References => "referenced_by",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Node {
    /// Stable identifier, e.g. `src/graph.rs::Graph::load`.
    pub id: String,
    pub kind: SymKind,
    pub name: String,
    /// Repo-relative path ("" for external packages).
    pub file: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lang: Option<Lang>,
    pub line: u32,
    pub end_line: u32,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub signature: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub doc: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<usize>,
    /// Index in `Graph::communities`; none for external packages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub community: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Edge {
    pub from: usize,
    pub to: usize,
    pub kind: EdgeKind,
    /// Number of occurrences (e.g. call sites) merged into this edge.
    pub count: u32,
    /// How sure the resolver is about the target (omitted when exact).
    #[serde(default, skip_serializing_if = "Confidence::is_exact")]
    pub confidence: Confidence,
    /// First line in the source file where the relation occurs.
    pub line: u32,
    /// Every line where it occurs, ascending (dependency edges; empty in older indexes
    /// and for containment).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lines: Vec<u32>,
}

/// Sites listed per edge in tool output.
pub const MAX_SITES_SHOWN: usize = 10;

impl Edge {
    pub fn is_ambiguous(&self) -> bool {
        self.confidence == Confidence::Ambiguous
    }

    /// Adds the sites to a JSON object: `call_sites` for calls, `lines` otherwise, at
    /// most `MAX_SITES_SHOWN` of them plus `more_sites`.
    pub fn add_sites(&self, o: &mut serde_json::Value) {
        if self.lines.is_empty() {
            return;
        }
        let key = if self.kind == EdgeKind::Calls {
            "call_sites"
        } else {
            "lines"
        };
        let shown = &self.lines[..self.lines.len().min(MAX_SITES_SHOWN)];
        o[key] = serde_json::json!(shown);
        if self.lines.len() > shown.len() {
            o["more_sites"] = serde_json::json!(self.lines.len() - shown.len());
        }
    }

    /// `L12,40` (`L1,2,…(+5)` when long) for the text views; None when unknown.
    pub fn sites_text(&self) -> Option<String> {
        use std::fmt::Write;
        let (first, rest) = self.lines.split_first()?;
        let mut text = format!("L{first}");
        for l in rest.iter().take(MAX_SITES_SHOWN - 1) {
            let _ = write!(text, ",{l}");
        }
        if self.lines.len() > MAX_SITES_SHOWN {
            let _ = write!(text, ",…(+{})", self.lines.len() - MAX_SITES_SHOWN);
        }
        Some(text)
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Graph {
    pub version: u32,
    pub root: String,
    /// Unix timestamp (seconds) of the indexing run.
    pub indexed_at: u64,
    /// The same in milliseconds (0 in older indexes), for precise staleness checks.
    #[serde(default)]
    pub indexed_at_ms: u64,
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    /// Structural communities, largest first (see `communities`).
    #[serde(default)]
    pub communities: Vec<Community>,
    /// Git HEAD commit and branch (None: detached) when the graph was built; absent
    /// outside a Git repository.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_branch: Option<String>,
    /// Tracked files had uncommitted changes when the graph was built (absent when
    /// unknown: no Git repository or no `git` command).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_dirty: Option<bool>,
    /// `ANALYSIS_VERSION` of the indexer that built the graph (0: before it existed).
    #[serde(default)]
    pub analysis: u32,
    /// Hub percentile the communities were computed with.
    #[serde(default = "default_hub_percentile")]
    pub hub_percentile: f64,

    #[serde(skip)]
    pub out_edges: Vec<Vec<usize>>,
    #[serde(skip)]
    pub in_edges: Vec<Vec<usize>>,
    #[serde(skip)]
    pub by_id: HashMap<String, usize>,
    #[serde(skip)]
    pub by_name: HashMap<String, Vec<usize>>,
    #[serde(skip)]
    pub by_file: HashMap<String, usize>,
}

/// Directory of a repo-relative path ("" for top-level files).
pub fn dir_of(file: &str) -> &str {
    file.rsplit_once('/').map_or("", |(d, _)| d)
}

fn default_hub_percentile() -> f64 {
    crate::communities::DEFAULT_HUB_PERCENTILE
}

impl Graph {
    /// Hub percentile of the saved index, without loading the whole graph.
    pub fn saved_hub_percentile(root: &Path) -> Option<f64> {
        #[derive(Deserialize)]
        struct Header {
            version: u32,
            #[serde(default = "default_hub_percentile")]
            hub_percentile: f64,
        }
        let data = fs::read(Self::index_path(root)).ok()?;
        let h: Header = serde_json::from_slice(&data).ok()?;
        (h.version == FORMAT_VERSION).then_some(h.hub_percentile)
    }

    pub fn new(root: &Path) -> Graph {
        Graph {
            version: FORMAT_VERSION,
            root: root.display().to_string(),
            indexed_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            hub_percentile: default_hub_percentile(),
            analysis: ANALYSIS_VERSION,
            ..Default::default()
        }
    }

    pub fn index_path(root: &Path) -> PathBuf {
        root.join(INDEX_DIR).join(GRAPH_FILE)
    }

    pub fn save(&self, root: &Path) -> Result<PathBuf> {
        let dir = root.join(INDEX_DIR);
        fs::create_dir_all(&dir)?;
        // Keep the index out of version control without touching the repo's .gitignore.
        let ignore = dir.join(".gitignore");
        if !ignore.exists() {
            fs::write(&ignore, "*\n")?;
        }
        let path = Self::index_path(root);
        let tmp = path.with_extension("cnav.tmp");
        fs::write(&tmp, serde_json::to_vec(self)?)?;
        fs::rename(&tmp, &path)?;
        let _ = fs::remove_file(dir.join(LEGACY_GRAPH_FILE));
        Ok(path)
    }

    pub fn load(root: &Path) -> Result<Graph> {
        let path = Self::index_path(root);
        let data = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        let mut g: Graph = serde_json::from_slice(&data)?;
        anyhow::ensure!(
            g.version == FORMAT_VERSION,
            "index format v{} is outdated, re-run `codenav index`",
            g.version
        );
        g.build_indexes();
        Ok(g)
    }

    pub fn build_indexes(&mut self) {
        let n = self.nodes.len();
        self.out_edges = vec![Vec::new(); n];
        self.in_edges = vec![Vec::new(); n];
        for (i, e) in self.edges.iter().enumerate() {
            self.out_edges[e.from].push(i);
            self.in_edges[e.to].push(i);
        }
        self.by_id.clear();
        self.by_name.clear();
        self.by_file.clear();
        for (i, node) in self.nodes.iter().enumerate() {
            self.by_id.insert(node.id.clone(), i);
            self.by_name
                .entry(node.name.to_lowercase())
                .or_default()
                .push(i);
            if node.kind == SymKind::File {
                self.by_file.insert(node.file.clone(), i);
            }
        }
    }

    pub fn degree(&self, i: usize) -> usize {
        self.out_edges[i].len() + self.in_edges[i].len()
    }

    /// Display-friendly qualified name without the file prefix.
    pub fn qualified_name(&self, i: usize) -> String {
        let node = &self.nodes[i];
        node.id
            .strip_prefix(&format!("{}::", node.file))
            .unwrap_or(&node.id)
            .to_string()
    }

    /// The file node that (transitively) contains a symbol.
    pub fn file_of(&self, i: usize) -> Option<usize> {
        self.by_file.get(&self.nodes[i].file).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_writes_cnav_and_removes_the_legacy_json() {
        let root = std::env::temp_dir().join(format!("codenav-unit-save-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join(INDEX_DIR)).unwrap();
        fs::write(root.join(INDEX_DIR).join(LEGACY_GRAPH_FILE), "{}").unwrap();

        let path = Graph::new(&root).save(&root).unwrap();

        assert_eq!(path, root.join(".codenav/graph.cnav"));
        assert!(path.exists());
        assert!(!root.join(".codenav/graph.json").exists());
        assert!(!root.join(".codenav/graph.cnav.tmp").exists());
        assert_eq!(Graph::load(&root).unwrap().version, FORMAT_VERSION);
        fs::remove_dir_all(&root).unwrap();
    }
}
