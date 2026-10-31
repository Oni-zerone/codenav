//! The code graph: nodes (files and symbols), typed edges, persistence and lookup indexes.

use std::cell::Cell;
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
/// `graph.cnav` starts with these bytes, then `FORMAT_VERSION` (u32, little endian), then
/// the graph in bincode. Older indexes are JSON: still read (and rewritten on load).
const MAGIC: &[u8; 4] = b"CNAV";

thread_local! {
    /// Set while writing `graph.cnav`: bincode has no field names, so every field is
    /// written (`skip_serializing_if` would shift the following ones). JSON (`codenav
    /// export`, the extension) still omits empty fields.
    static WRITING_BINARY: Cell<bool> = const { Cell::new(false) };
}

fn writing_binary() -> bool {
    WRITING_BINARY.with(Cell::get)
}

fn omit_none<T>(v: &Option<T>) -> bool {
    v.is_none() && !writing_binary()
}

fn omit_empty_string(v: &str) -> bool {
    v.is_empty() && !writing_binary()
}

fn omit_empty_vec<T>(v: &[T]) -> bool {
    v.is_empty() && !writing_binary()
}

fn omit_exact(c: &Confidence) -> bool {
    c.is_exact() && !writing_binary()
}
/// v2: communities (`Node::community`, `Graph::communities`).
/// v3: `Edge::confidence` replaces the `ambiguous` flag.
/// v4: binary (see `MAGIC`); the fields are v3's. Changing a serialized type changes the
///     binary layout: bump it then (a test fails until `LAYOUT_FINGERPRINT` is updated).
pub const FORMAT_VERSION: u32 = 4;
/// The last JSON format, still read.
const JSON_FORMAT_VERSION: u32 = 3;
/// Version of what the indexer extracts and how it resolves names. Bump it when the
/// same sources would give a different graph: older indexes are then stale and rebuilt,
/// while readers of the format (like the VS Code extension) are unaffected.
/// 1: JS/TS module scoping, JSX components only, top-level constants.
/// 2: `Edge::lines` (every line of a relation, e.g. call sites).
/// 3: common methods on unknown receivers need the owner type named nearby; module
///    qualifiers must be single names.
/// 4: imports of local packages (npm workspaces, Cargo crates) resolve to their files.
/// 5: JS/TS runtime globals (`localStorage`, `fetch`…) are nodes with edges to them.
pub const ANALYSIS_VERSION: u32 = 5;

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
    #[serde(default, skip_serializing_if = "omit_none")]
    pub lang: Option<Lang>,
    pub line: u32,
    pub end_line: u32,
    #[serde(default, skip_serializing_if = "omit_empty_string")]
    pub signature: String,
    #[serde(default, skip_serializing_if = "omit_none")]
    pub doc: Option<String>,
    #[serde(default, skip_serializing_if = "omit_none")]
    pub parent: Option<usize>,
    /// Index in `Graph::communities`; none for external packages.
    #[serde(default, skip_serializing_if = "omit_none")]
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
    #[serde(default, skip_serializing_if = "omit_exact")]
    pub confidence: Confidence,
    /// First line in the source file where the relation occurs.
    pub line: u32,
    /// Every line where it occurs, ascending (dependency edges; empty in older indexes
    /// and for containment).
    #[serde(default, skip_serializing_if = "omit_empty_vec")]
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

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
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
    #[serde(default, skip_serializing_if = "omit_none")]
    pub git_commit: Option<String>,
    #[serde(default, skip_serializing_if = "omit_none")]
    pub git_branch: Option<String>,
    /// Tracked files had uncommitted changes when the graph was built (absent when
    /// unknown: no Git repository or no `git` command).
    #[serde(default, skip_serializing_if = "omit_none")]
    pub git_dirty: Option<bool>,
    /// `ANALYSIS_VERSION` of the indexer that built the graph (0: before it existed).
    #[serde(default)]
    pub analysis: u32,
    /// Packages the repository declares (npm workspaces, crates).
    #[serde(default, skip_serializing_if = "omit_empty_vec")]
    pub packages: Vec<crate::packages::PackageRef>,
    /// Package manifests found when indexing (a new or removed one makes it stale).
    #[serde(default)]
    pub manifest_files: usize,
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

/// Writes through a temporary file, so readers never see a partial file.
fn write_atomically(path: &Path, data: &[u8]) -> Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    fs::write(&tmp, data)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

fn default_hub_percentile() -> f64 {
    crate::communities::DEFAULT_HUB_PERCENTILE
}

impl Graph {
    /// The saved graph without its lookup indexes, if there is a readable one.
    fn read_saved(root: &Path) -> Option<Graph> {
        Graph::from_bytes(&fs::read(Self::index_path(root)).ok()?).ok()
    }

    /// Hub percentile of the saved index.
    pub fn saved_hub_percentile(root: &Path) -> Option<f64> {
        Self::read_saved(root).map(|g| g.hub_percentile)
    }

    /// Packages recorded in the saved index.
    pub fn saved_packages(root: &Path) -> Vec<crate::packages::PackageRef> {
        Self::read_saved(root)
            .map(|g| g.packages)
            .unwrap_or_default()
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

    /// The graph as stored in `graph.cnav`.
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        struct Writing;
        impl Drop for Writing {
            fn drop(&mut self) {
                WRITING_BINARY.with(|w| w.set(false));
            }
        }
        WRITING_BINARY.with(|w| w.set(true));
        let _writing = Writing;
        let mut data = MAGIC.to_vec();
        data.extend(FORMAT_VERSION.to_le_bytes());
        bincode::serialize_into(&mut data, self)?;
        Ok(data)
    }

    /// A graph from the contents of `graph.cnav` (binary, or JSON from older versions),
    /// without its lookup indexes.
    pub fn from_bytes(data: &[u8]) -> Result<Graph> {
        if data.first() == Some(&b'{') {
            let mut g: Graph = serde_json::from_slice(data)?;
            anyhow::ensure!(
                g.version == JSON_FORMAT_VERSION,
                "index format v{} is outdated, re-run `codenav index`",
                g.version
            );
            g.version = FORMAT_VERSION;
            return Ok(g);
        }
        let body = data
            .strip_prefix(MAGIC.as_slice())
            .context("not a codenav graph (.cnav) file")?;
        let (version, body) = body
            .split_first_chunk::<4>()
            .context("truncated codenav graph")?;
        let version = u32::from_le_bytes(*version);
        anyhow::ensure!(
            version == FORMAT_VERSION,
            "index format v{version} is {}, re-run `codenav index`",
            if version < FORMAT_VERSION {
                "outdated"
            } else {
                "newer than this codenav"
            }
        );
        let mut g: Graph = bincode::deserialize(body).context("corrupt codenav graph")?;
        g.version = FORMAT_VERSION;
        Ok(g)
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
        write_atomically(&path, &self.to_bytes()?)?;
        // Leftovers of older versions.
        let _ = fs::remove_file(dir.join(LEGACY_GRAPH_FILE));
        let _ = fs::remove_file(dir.join("graph.bin"));
        Ok(path)
    }

    pub fn load(root: &Path) -> Result<Graph> {
        let path = Self::index_path(root);
        let data = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        let mut g = Graph::from_bytes(&data)?;
        // A JSON index from an older version: rewrite it in the current format.
        if data.first() == Some(&b'{')
            && let Err(e) = g.save(root)
        {
            eprintln!("codenav: could not convert the index: {e:#}");
        }
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
        self.by_id.reserve(n);
        self.by_name.reserve(n);
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

    /// A graph with every optional field both set and unset.
    fn sample(root: &Path) -> Graph {
        let mut g = Graph::new(root);
        let node = |name: &str, parent: Option<usize>| Node {
            id: format!("a.py::{name}"),
            kind: SymKind::Function,
            name: name.into(),
            file: "a.py".into(),
            lang: parent.map(|_| Lang::Python),
            line: 1,
            end_line: 2,
            signature: if parent.is_some() {
                "def f()".into()
            } else {
                String::new()
            },
            doc: parent.map(|_| "Docs.".into()),
            parent,
            community: parent.map(|_| 0),
        };
        g.nodes = vec![node("f", None), node("g", Some(0))];
        let edge = |confidence, lines: Vec<u32>| Edge {
            from: 1,
            to: 0,
            kind: EdgeKind::Calls,
            count: 1,
            confidence,
            line: 1,
            lines,
        };
        g.edges = vec![
            edge(Confidence::Exact, vec![]),
            edge(Confidence::Ambiguous, vec![1, 3]),
        ];
        g.git_commit = Some("abc".into());
        g.git_dirty = Some(false);
        g.indexed_at = 1;
        g.communities = vec![Community {
            label: "a".into(),
            size: 2,
        }];
        g.packages = vec![crate::packages::PackageRef {
            kind: crate::packages::PackageKind::Npm,
            name: "pkg".into(),
        }];
        g
    }

    fn json(g: &Graph) -> serde_json::Value {
        serde_json::to_value(g).unwrap()
    }

    #[test]
    fn graphs_round_trip_through_the_binary_file() {
        let root = std::env::temp_dir().join(format!("codenav-unit-bin-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let g = sample(&root);
        g.save(&root).unwrap();
        let data = fs::read(Graph::index_path(&root)).unwrap();
        assert!(data.starts_with(MAGIC));
        assert_eq!(json(&Graph::load(&root).unwrap()), json(&g));
        assert_eq!(Graph::saved_packages(&root), g.packages);
        // JSON (export, the extension) still leaves empty fields out.
        let text = serde_json::to_string(&g).unwrap();
        assert!(!text.contains("\"doc\":null") && !text.contains("\"lines\":[]"));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_json_index_loads_and_is_rewritten_in_binary() {
        let root = std::env::temp_dir().join(format!("codenav-unit-mig-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join(INDEX_DIR)).unwrap();
        let mut g = sample(&root);
        g.version = JSON_FORMAT_VERSION;
        fs::write(Graph::index_path(&root), serde_json::to_vec(&g).unwrap()).unwrap();
        fs::write(root.join(INDEX_DIR).join("graph.bin"), "old copy").unwrap();
        let loaded = Graph::load(&root).unwrap();
        g.version = FORMAT_VERSION;
        assert_eq!(json(&loaded), json(&g));
        assert_eq!(loaded.by_id.len(), 2);
        assert!(
            fs::read(Graph::index_path(&root))
                .unwrap()
                .starts_with(MAGIC)
        );
        assert!(!root.join(INDEX_DIR).join("graph.bin").exists());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn unreadable_files_say_why() {
        let err = |data: &[u8]| format!("{:#}", Graph::from_bytes(data).unwrap_err());
        assert!(
            err(br#"{"version":1,"root":"","indexed_at":0,"nodes":[],"edges":[]}"#)
                .contains("outdated")
        );
        let mut newer = MAGIC.to_vec();
        newer.extend((FORMAT_VERSION + 1).to_le_bytes());
        assert!(err(&newer).contains("newer"));
        assert!(err(b"hello").contains("not a codenav graph"));
        let mut cut = sample(Path::new("/r")).to_bytes().unwrap();
        cut.truncate(cut.len() / 2);
        assert!(err(&cut).contains("corrupt"));
    }

    /// FNV-1a of the binary file of `sample("/repo")`. It changes when a serialized type
    /// changes its layout: then bump `FORMAT_VERSION` (older files must not be misread),
    /// and update this value.
    const LAYOUT_FINGERPRINT: u64 = 0xdea8_043d_700e_c209;

    #[test]
    fn the_binary_layout_matches_the_format_version() {
        let bytes = sample(Path::new("/repo")).to_bytes().unwrap();
        let hash = bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| {
            (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
        });
        assert_eq!(
            hash, LAYOUT_FINGERPRINT,
            "the binary layout of the graph changed: bump FORMAT_VERSION (now {FORMAT_VERSION}) \
             and set LAYOUT_FINGERPRINT to {hash:#x}"
        );
    }

    /// The binary file has no field names: a field left out would shift the next ones.
    /// Every `skip_serializing_if` of a serialized type must use an `omit_` helper.
    #[test]
    fn serialized_types_skip_fields_only_through_omit_helpers() {
        for source in [
            include_str!("graph.rs"),
            include_str!("communities.rs"),
            include_str!("lang.rs"),
            include_str!("packages.rs"),
        ] {
            let pattern = concat!("skip_serializing_if", " = \"");
            for (i, _) in source.match_indices(pattern) {
                let helper = &source[i + pattern.len()..];
                assert!(
                    helper.starts_with("omit_"),
                    "{}",
                    &helper[..30.min(helper.len())]
                );
            }
        }
    }
}
