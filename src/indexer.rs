//! Repository indexing: walks the tree, extracts facts from every supported file in
//! parallel, then resolves names across files into a `Graph`.
//!
//! Resolution is heuristic (no type checking): a reference is linked to definitions
//! with the same name, preferring the same file, then imported files, then the same
//! directory, then a small set of global candidates. Each edge records how sure the
//! match is (`Confidence`): exact, inferred by proximity, or ambiguous.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use ignore::{Match as IgnoreMatch, WalkBuilder};
use rayon::prelude::*;

use crate::communities;
use crate::extract::{self, FileFacts, RefKind};
use crate::graph::{Confidence, Edge, EdgeKind, Graph, INDEX_DIR, Node, SymKind};
use crate::lang::Lang;
use crate::packages::{self, PackageKind};
use crate::vcs;

/// Project-specific exclusions, same syntax as `.gitignore`.
pub const IGNORE_FILE: &str = ".codenavignore";
const MAX_FILE_BYTES: u64 = 1024 * 1024;
const MAX_AMBIGUOUS: usize = 3;
/// No node (a node without a file).
const NONE: usize = usize::MAX;
const SKIP_DIRS: &[&str] = &[
    INDEX_DIR,
    "node_modules",
    "target",
    "vendor",
    "Pods",
    "Carthage",
    "DerivedData",
    "dist",
    "build",
    "__pycache__",
    "venv",
    "site-packages",
];
/// Method names so common that an unqualified global match is almost always wrong.
const COMMON_METHODS: &[&str] = &[
    "new",
    "get",
    "set",
    "len",
    "push",
    "pop",
    "insert",
    "remove",
    "contains",
    "clone",
    "map",
    "filter",
    "iter",
    "into",
    "from",
    "default",
    "to_string",
    "unwrap",
    "append",
    "add",
    "update",
    "keys",
    "values",
    "items",
    "init",
    "run",
    "start",
    "stop",
    "close",
    "open",
    "read",
    "write",
    "send",
    "format",
    "join",
    "split",
    "next",
    "build",
    "apply",
    "call",
    "execute",
    "handle",
    "parse",
    "load",
    "save",
    "then",
    "catch",
    "log",
    "error",
    "forEach",
    "reduce",
    "toString",
    "is_empty",
    "as_ref",
    "description",
];

pub struct IndexStats {
    pub files: usize,
    pub symbols: usize,
    pub edges: usize,
    pub communities: usize,
    pub errors: Vec<String>,
    pub elapsed: Duration,
    pub by_lang: HashMap<Lang, usize>,
}

pub struct SourceFile {
    pub rel: String,
    pub abs: PathBuf,
    pub lang: Lang,
    /// Modification time when scanned.
    pub modified: Option<SystemTime>,
}

/// Files under `root` that indexing considers: .gitignore and .codenavignore rules, no
/// dependency or build directories. Shared with package discovery.
pub fn walk_files(root: &Path) -> impl Iterator<Item = ignore::DirEntry> {
    let excludes = CodenavIgnore::new(root);
    WalkBuilder::new(root)
        .require_git(false)
        .filter_entry(move |e| {
            let is_dir = e.file_type().is_some_and(|t| t.is_dir());
            let skipped_dir = is_dir
                && e.file_name()
                    .to_str()
                    .is_some_and(|n| SKIP_DIRS.contains(&n));
            !skipped_dir && !excludes.is_excluded(e.path(), is_dir)
        })
        .build()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
}

/// What one walk of the repository finds: source files to index and package manifests.
pub struct Tree {
    pub sources: Vec<SourceFile>,
    /// package.json, Cargo.toml, pnpm-workspace.yaml (absolute paths, sorted).
    pub manifests: Vec<PathBuf>,
}

#[cfg(test)]
fn discover(root: &Path) -> Vec<SourceFile> {
    scan(root).sources
}

pub fn scan(root: &Path) -> Tree {
    let mut sources = Vec::new();
    let mut manifests = Vec::new();
    for e in walk_files(root) {
        let name = e.file_name().to_string_lossy();
        if packages::MANIFESTS.contains(&name.as_ref()) {
            manifests.push(e.path().to_path_buf());
            continue;
        }
        let Some(lang) = Lang::from_path(e.path()) else {
            continue;
        };
        let Some(meta) = e.metadata().ok().filter(|m| m.len() <= MAX_FILE_BYTES) else {
            continue;
        };
        let Ok(rel) = e.path().strip_prefix(root) else {
            continue;
        };
        sources.push(SourceFile {
            rel: to_unix(rel),
            abs: e.path().to_path_buf(),
            lang,
            modified: meta.modified().ok(),
        });
    }
    sources.sort_by(|a, b| a.rel.cmp(&b.rel));
    manifests.sort();
    Tree { sources, manifests }
}

/// `.codenavignore` rules, applied on top of the walker's own filters (`.gitignore`,
/// hidden files, `SKIP_DIRS`). They can only narrow the index: a `!pattern` only makes
/// an exception to an earlier `.codenavignore` rule and never re-includes a path the
/// walker already excludes. Only files at the root and below are read, so indexing
/// an excluded directory directly works whatever the pattern in its parent.
struct CodenavIgnore {
    root: PathBuf,
    /// directory -> its parsed `.codenavignore`, if any
    cache: Mutex<HashMap<PathBuf, Option<Arc<Gitignore>>>>,
}

impl CodenavIgnore {
    fn new(root: &Path) -> CodenavIgnore {
        CodenavIgnore {
            root: root.to_path_buf(),
            cache: Mutex::new(HashMap::new()),
        }
    }

    fn matcher(&self, dir: &Path) -> Option<Arc<Gitignore>> {
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        cache
            .entry(dir.to_path_buf())
            .or_insert_with(|| {
                let file = dir.join(IGNORE_FILE);
                if !file.is_file() {
                    return None;
                }
                let mut builder = GitignoreBuilder::new(dir);
                // Like git's core.ignorecase default on case-insensitive filesystems.
                let _ = builder.case_insensitive(cfg!(any(target_os = "macos", windows)));
                if let Some(err) = builder.add(&file) {
                    eprintln!("codenav: {}: {err}", file.display());
                }
                builder.build().ok().map(Arc::new)
            })
            .clone()
    }

    /// Deeper `.codenavignore` files take precedence, as with `.gitignore`.
    fn is_excluded(&self, path: &Path, is_dir: bool) -> bool {
        let Ok(rel) = path.strip_prefix(&self.root) else {
            return false;
        };
        let parents: Vec<_> = rel
            .parent()
            .map(|p| p.components().collect())
            .unwrap_or_default();
        let mut dir = self.root.clone();
        let mut excluded = false;
        for i in 0..=parents.len() {
            if i > 0 {
                dir.push(parents[i - 1]);
            }
            if let Some(m) = self.matcher(&dir) {
                match m.matched(path, is_dir) {
                    IgnoreMatch::Ignore(_) => excluded = true,
                    IgnoreMatch::Whitelist(_) => excluded = false,
                    IgnoreMatch::None => {}
                }
            }
        }
        excluded
    }
}

/// True when some source file changed after the graph was built (or files were added/removed),
/// or the graph was built by an indexer with a different analysis.
pub fn is_stale(root: &Path, graph: &Graph) -> bool {
    is_stale_tree(graph, &scan(root))
}

/// `is_stale` on a scan the caller already made (to reuse it for re-indexing).
pub fn is_stale_tree(graph: &Graph, tree: &Tree) -> bool {
    if graph.analysis != crate::graph::ANALYSIS_VERSION {
        return true;
    }
    let files = &tree.sources;
    if files.len() != graph.by_file.len() || tree.manifests.len() != graph.manifest_files {
        return true;
    }
    // Milliseconds when known: whole seconds would flag files saved just before indexing.
    let since = if graph.indexed_at_ms > 0 {
        UNIX_EPOCH + Duration::from_millis(graph.indexed_at_ms)
    } else {
        UNIX_EPOCH + Duration::from_secs(graph.indexed_at)
    };
    let changed = |p: &Path| {
        fs::metadata(p)
            .and_then(|m| m.modified())
            .is_ok_and(|t| t > since)
    };
    // Package manifests decide how imports resolve: their changes count too.
    files
        .iter()
        .any(|f| !graph.by_file.contains_key(&f.rel) || f.modified.is_some_and(|t| t > since))
        || tree.manifests.iter().any(|m| changed(m))
}

/// Explicit settings win; otherwise keep those of the saved index, so a later
/// re-index (by `serve`, `tool` or a plain `index`) doesn't change the communities.
pub fn index_options(root: &Path, requested: Option<f64>) -> IndexOptions {
    IndexOptions {
        hub_percentile: requested
            .or_else(|| Graph::saved_hub_percentile(root))
            .unwrap_or(communities::DEFAULT_HUB_PERCENTILE),
    }
}

/// Reuses the saved graph when it is up to date and was built with the requested
/// settings, otherwise indexes and saves. Returns the settings in effect.
/// `check_sources`: look for source files changed since the graph was built (a walk of
/// the repository); without it, a saved graph is reused unless the analysis changed
/// (for callers that refresh on their own, or know it is current).
pub fn load_or_index(
    root: &Path,
    force: bool,
    requested: Option<f64>,
    check_sources: bool,
) -> Result<(Graph, IndexOptions)> {
    let index_path = Graph::index_path(root);
    let (opts, tree) = if force || !index_path.exists() {
        if !force {
            eprintln!("codenav: no index found, indexing {}…", root.display());
        }
        (index_options(root, requested), None)
    } else {
        // The scan for changed sources runs while the graph loads.
        let (loaded, tree) = std::thread::scope(|s| {
            let tree = check_sources.then(|| s.spawn(|| scan(root)));
            let loaded = Graph::load(root);
            let tree = tree.map(|t| t.join().unwrap_or_else(|e| std::panic::resume_unwind(e)));
            (loaded, tree)
        });
        match loaded {
            Ok(mut g) => {
                let opts = IndexOptions {
                    hub_percentile: requested.unwrap_or(g.hub_percentile),
                };
                if opts.hub_percentile != g.hub_percentile {
                    eprintln!(
                        "codenav: hub percentile changed ({} → {}), re-indexing…",
                        g.hub_percentile, opts.hub_percentile
                    );
                } else if g.analysis != crate::graph::ANALYSIS_VERSION
                    || tree.as_ref().is_some_and(|t| is_stale_tree(&g, t))
                {
                    eprintln!("codenav: index is stale, re-indexing…");
                } else {
                    // Same code, maybe a new commit: keep the graph, update its Git state.
                    if vcs::refresh_state(&mut g, root)
                        && let Err(e) = g.save(root)
                    {
                        eprintln!("codenav: could not save index: {e:#}");
                    }
                    return Ok((g, opts));
                }
                (opts, tree)
            }
            Err(e) => {
                eprintln!("codenav: cannot use the saved index ({e:#}), re-indexing…");
                (index_options(root, requested), tree)
            }
        }
    };
    let (graph, stats) = index_cached(root, &opts, &mut FactsCache::default(), tree, None)?;
    eprintln!(
        "codenav: {} files, {} symbols, {} edges, {} communities ({:.2?})",
        stats.files, stats.symbols, stats.edges, stats.communities, stats.elapsed
    );
    if let Err(e) = graph.save(root) {
        eprintln!("codenav: could not save index: {e:#}");
    }
    Ok((graph, opts))
}

/// Indexing settings that are not read from the repository itself.
#[derive(Debug, Clone, Copy)]
pub struct IndexOptions {
    /// Dependency-degree percentile above which a node is a hub for community detection.
    pub hub_percentile: f64,
}

impl Default for IndexOptions {
    fn default() -> Self {
        IndexOptions {
            hub_percentile: communities::DEFAULT_HUB_PERCENTILE,
        }
    }
}

#[cfg(test)]
pub fn index(root: &Path) -> Result<(Graph, IndexStats)> {
    index_with(root, &IndexOptions::default())
}

/// FxHash (as in rustc): much faster than the default SipHash for the short keys of the
/// resolver's hot maps; their keys come from the repository, not from an adversary.
#[derive(Default, Clone, Copy)]
struct FxHasher(u64);

impl std::hash::Hasher for FxHasher {
    fn write(&mut self, bytes: &[u8]) {
        let (words, rest) = bytes.as_chunks::<8>();
        for word in words {
            self.add(u64::from_le_bytes(*word));
        }
        for &b in rest {
            self.add(u64::from(b));
        }
    }

    fn write_u8(&mut self, n: u8) {
        self.add(u64::from(n));
    }

    fn write_u32(&mut self, n: u32) {
        self.add(u64::from(n));
    }

    fn write_u64(&mut self, n: u64) {
        self.add(n);
    }

    fn write_usize(&mut self, n: usize) {
        self.add(n as u64);
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

impl FxHasher {
    fn add(&mut self, word: u64) {
        self.0 = (self.0.rotate_left(5) ^ word).wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
    }
}

type FastMap<K, V> = HashMap<K, V, std::hash::BuildHasherDefault<FxHasher>>;
type FastSet<K> = HashSet<K, std::hash::BuildHasherDefault<FxHasher>>;
/// A reference's name, language family and kind (see `Builder::candidates`).
type CandidateKey<'a> = (&'a str, u8, u8);
/// The types whose names a scope uses, sorted (see `Builder::named_types`).
type NamedTypes = Rc<Vec<usize>>;

/// JS/TS runtime objects whose uses become `references` edges to a node of their own,
/// so "who uses localStorage" has an answer. Not `history`, `location` (often names of
/// other things) nor `console` (used everywhere).
const JS_GLOBAL_OBJECTS: &[&str] = &[
    "localStorage",
    "sessionStorage",
    "indexedDB",
    "document",
    "window",
    "navigator",
    "globalThis",
    "process",
];
/// Objects holding the globals: `window.localStorage` is `localStorage`.
const JS_GLOBAL_HOSTS: &[&str] = &["window", "globalThis", "self"];
/// JS/TS runtime functions whose calls become `calls` edges.
const JS_GLOBAL_FUNCTIONS: &[&str] = &["fetch"];

pub fn index_with(root: &Path, opts: &IndexOptions) -> Result<(Graph, IndexStats)> {
    index_cached(root, opts, &mut FactsCache::default(), None, None)
}

/// Facts extracted from each file, kept between indexing runs by the server so that only
/// changed files are parsed again. Resolution and communities always run on every file's
/// facts, so the graph is the same as a full index.
#[derive(Default)]
pub struct FactsCache {
    files: HashMap<String, CachedFile>,
}

struct CachedFile {
    modified: SystemTime,
    len: u64,
    /// when the file was read (taken before reading)
    read_at: SystemTime,
    facts: Result<FileFacts, String>,
}

impl CachedFile {
    /// Still matches a file with this stamp. A file modified around the time it was read
    /// may have changed again since with the same size and mtime (same clock tick): read
    /// it again. Without sub-second timestamps the tick can be up to 2 s (FAT).
    fn matches(&self, (modified, len): (SystemTime, u64)) -> bool {
        let coarse = modified
            .duration_since(UNIX_EPOCH)
            .is_ok_and(|d| d.subsec_nanos() == 0);
        let tick = std::time::Duration::from_secs(if coarse { 2 } else { 0 });
        self.modified == modified && self.len == len && modified + tick < self.read_at
    }
}

/// A file's (mtime, size).
fn stamp(path: &Path) -> Option<(SystemTime, u64)> {
    let m = fs::metadata(path).ok()?;
    Some((m.modified().ok()?, m.len()))
}

fn read_facts(f: &SourceFile) -> Result<FileFacts, String> {
    fs::read(&f.abs)
        .map_err(|e| format!("{}: {e}", f.rel))
        .and_then(|src| {
            extract::extract(&f.rel, f.lang, &src).map_err(|e| format!("{}: {e:#}", f.rel))
        })
}

impl FactsCache {
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Parses every source file of `root` into the cache (used to warm it in the
    /// background, so that the first incremental update only parses changed files).
    pub fn fill(&mut self, root: &Path) {
        let tree = scan(root);
        let files = &tree.sources;
        let parsed: Vec<(String, CachedFile)> = files
            .par_iter()
            .filter(|f| !self.files.contains_key(&f.rel))
            .filter_map(|f| {
                let read_at = SystemTime::now();
                let (modified, len) = stamp(&f.abs)?;
                let facts = read_facts(f);
                Some((
                    f.rel.clone(),
                    CachedFile {
                        modified,
                        len,
                        read_at,
                        facts,
                    },
                ))
            })
            .collect();
        self.files.extend(parsed);
    }
}

/// Indexes `root`, parsing only files that are new or changed since `cache` saw them (by
/// modification time and size); `tree` is a scan of `root` if the caller already has one.
/// The Git state is read (`git status`: tens of ms) unless `git_from` gives a graph to copy
/// it from, for callers that read it afterwards, off the critical path.
pub fn index_cached(
    root: &Path,
    opts: &IndexOptions,
    cache: &mut FactsCache,
    tree: Option<Tree>,
    git_from: Option<&Graph>,
) -> Result<(Graph, IndexStats)> {
    let started = Instant::now();
    // Timestamp taken before reading, so edits made during indexing count as stale.
    let started_at = SystemTime::now().duration_since(UNIX_EPOCH)?;
    let tree = tree.unwrap_or_else(|| scan(root));
    let files = &tree.sources;

    // Files new or changed since cached, read again (with their stamp, taken before).
    let parsed: Vec<(usize, CachedFile)> = (0..files.len())
        .into_par_iter()
        .filter_map(|k| {
            let f = &files[k];
            let read_at = SystemTime::now();
            let stamp = stamp(&f.abs);
            let cached = cache.files.get(&f.rel);
            if cached.is_some_and(|c| stamp.is_some_and(|s| c.matches(s))) {
                return None;
            }
            let (modified, len) = stamp.unwrap_or((UNIX_EPOCH, 0));
            let facts = read_facts(f);
            Some((
                k,
                CachedFile {
                    modified,
                    len,
                    read_at,
                    facts,
                },
            ))
        })
        .collect();
    for (k, entry) in parsed {
        cache.files.insert(files[k].rel.clone(), entry);
    }
    // Forget removed files.
    let present: HashSet<&str> = files.iter().map(|f| f.rel.as_str()).collect();
    cache
        .files
        .retain(|path, _| present.contains(path.as_str()));

    let mut facts: Vec<&FileFacts> = Vec::with_capacity(files.len());
    let mut errors = Vec::new();
    for f in files {
        match &cache.files[&f.rel].facts {
            Ok(x) => facts.push(x),
            Err(e) => errors.push(e.clone()),
        }
    }

    let mut graph = Builder::new(root, &facts, &tree.manifests).build();
    graph.manifest_files = tree.manifests.len();
    graph.indexed_at = started_at.as_secs();
    graph.indexed_at_ms = started_at.as_millis() as u64;
    graph.build_indexes();
    match git_from {
        Some(previous) => {
            graph.git_commit = previous.git_commit.clone();
            graph.git_branch = previous.git_branch.clone();
            graph.git_dirty = previous.git_dirty;
        }
        None => vcs::record_state(&mut graph, root),
    }
    communities::detect(&mut graph, opts.hub_percentile);

    let mut by_lang = HashMap::new();
    for f in &facts {
        *by_lang.entry(f.lang).or_insert(0) += 1;
    }
    let stats = IndexStats {
        files: facts.len(),
        symbols: graph
            .nodes
            .iter()
            .filter(|n| !matches!(n.kind, SymKind::File | SymKind::External))
            .count(),
        edges: graph.edges.len(),
        communities: graph.communities.len(),
        errors,
        elapsed: started.elapsed(),
        by_lang,
    };
    Ok((graph, stats))
}

struct Builder<'a> {
    facts: &'a [&'a FileFacts],
    graph: Graph,
    /// file index -> file node
    file_nodes: Vec<usize>,
    file_by_path: HashMap<String, usize>,
    /// file index -> def index -> node
    def_nodes: Vec<Vec<usize>>,
    /// file index -> def index -> index of the def it is nested in (syntactically)
    def_parents: Vec<Vec<Option<usize>>>,
    /// file index -> impl index -> resolved owner type node and how sure that is
    impl_owner: Vec<Vec<Option<(usize, Confidence)>>>,
    /// member node -> confidence of its owner type, when the owner was not certain
    owner_confidence: HashMap<usize, Confidence>,
    /// exact name -> symbol nodes
    names: FastMap<String, Vec<usize>>,
    /// definition node -> its byte range in its file
    def_ranges: FastMap<usize, (usize, usize)>,
    /// node -> its file node, directory id and Rust crate id (`NONE` without a file),
    /// so that resolution compares integers instead of paths
    node_file: Vec<usize>,
    node_dir: Vec<u32>,
    node_crate: Vec<u32>,
    /// node -> ids of its file's stem (`utils` for `src/utils.py`) and directory name,
    /// with the maps from those names to ids (module qualifiers are compared by id)
    node_stem: Vec<u32>,
    /// node -> interned name of its parent (`NO_ID` for none), and the interning
    node_parent_name: Vec<u32>,
    parent_name_ids: HashMap<String, u32>,
    /// node -> a member (of a class or namespace, nested only in those) reachable from
    /// another JS/TS file through an instance or a qualifier
    member_reachable: Vec<bool>,
    /// (name, language family, reference kind) -> definitions of that name it may target,
    /// before any per-reference filter, each with whether a qualified JS/TS reference
    /// from another file can see it (`js_visible` without the per-reference part)
    candidates: RefCell<FastMap<CandidateKey<'a>, Vec<(usize, bool)>>>,
    /// (file, scope) -> the types whose names the scope uses (`scope_names`), sorted
    named_types: RefCell<FastMap<(usize, usize), NamedTypes>>,
    /// (file, specifier) -> what a JS/TS import resolves to
    js_imports: RefCell<FastMap<(&'a str, &'a str), Vec<Target>>>,
    node_dirname: Vec<u32>,
    stem_ids: HashMap<String, u32>,
    dirname_ids: HashMap<String, u32>,
    /// node (definition or file) -> byte ranges of the definitions nested directly in it
    child_ranges: FastMap<usize, Vec<(usize, usize)>>,
    /// file index -> name or qualifier -> byte offsets of the refs using it
    /// file index -> (byte, name or qualifier) of its refs, sorted by byte; built on
    /// first use (only files with common-method calls need it)
    mentions: Vec<std::cell::OnceCell<Vec<(usize, &'a str)>>>,
    /// local npm packages: name -> index in `packages`, entry file
    npm_packages: HashMap<String, (usize, Option<usize>)>,
    /// local crates: name -> (module root directory, lib file)
    crates: HashMap<String, (String, usize)>,
    packages: Vec<packages::Package>,
    /// path without extension -> file node (also `dir` for mod.rs / __init__.py / index.ts)
    stems: HashMap<String, usize>,
    /// every path suffix of a stem (by component) -> file nodes
    stem_suffixes: HashMap<String, Vec<usize>>,
    /// directory -> file nodes
    dirs: HashMap<String, Vec<usize>>,
    /// every path suffix of a directory -> directories
    dir_suffixes: HashMap<String, Vec<String>>,
    /// file node -> files it imports
    imported: HashMap<usize, HashSet<usize>>,
    /// While resolving the references of one file: node -> is a file it imports
    imports_file: Vec<bool>,
    externals: HashMap<String, usize>,
    edge_index: FastMap<(usize, usize, EdgeKind), usize>,
    lang_of_node: Vec<Lang>,
    /// JS/TS top-level definitions not exported from their module.
    hidden: HashSet<usize>,
    /// JS files with neither imports nor exports: plain scripts, whose top-level
    /// definitions are globals.
    script_files: HashSet<usize>,
    /// JS/TS file -> files it re-exports (`export … from`).
    reexports: HashMap<usize, HashSet<usize>>,
    /// JS/TS file index -> names imported from sources we cannot resolve to a repo file
    /// (workspace packages, tsconfig paths, npm packages).
    unresolved_imports: Vec<HashSet<String>>,
}

impl<'a> Builder<'a> {
    fn new(root: &Path, facts: &'a [&'a FileFacts], manifests: &[PathBuf]) -> Self {
        Builder {
            facts,
            graph: Graph::new(root),
            file_nodes: Vec::new(),
            file_by_path: HashMap::new(),
            def_nodes: Vec::new(),
            impl_owner: Vec::new(),
            owner_confidence: HashMap::new(),
            names: FastMap::default(),
            stems: HashMap::new(),
            stem_suffixes: HashMap::new(),
            dirs: HashMap::new(),
            dir_suffixes: HashMap::new(),
            imported: HashMap::new(),
            imports_file: Vec::new(),
            externals: HashMap::new(),
            edge_index: FastMap::default(),
            lang_of_node: Vec::new(),
            hidden: HashSet::new(),
            def_ranges: FastMap::default(),
            node_file: Vec::new(),
            node_dir: Vec::new(),
            node_crate: Vec::new(),
            node_stem: Vec::new(),
            node_parent_name: Vec::new(),
            parent_name_ids: HashMap::new(),
            member_reachable: Vec::new(),
            candidates: RefCell::new(FastMap::default()),
            named_types: RefCell::new(FastMap::default()),
            js_imports: RefCell::new(FastMap::default()),
            node_dirname: Vec::new(),
            stem_ids: HashMap::new(),
            dirname_ids: HashMap::new(),
            def_parents: Vec::new(),
            child_ranges: FastMap::default(),
            mentions: Vec::new(),
            npm_packages: HashMap::new(),
            crates: HashMap::new(),
            packages: packages::from_manifests(root, manifests),
            script_files: HashSet::new(),
            reexports: HashMap::new(),
            unresolved_imports: Vec::new(),
        }
    }

    fn build(mut self) -> Graph {
        self.add_files_and_defs();
        self.map_packages();
        self.collect_imported_files();
        self.resolve_owners();
        self.assign_ids();
        self.add_contains_edges();
        self.resolve_imports();
        self.resolve_impls();
        self.resolve_refs();
        self.graph.packages = self
            .packages
            .iter()
            .map(|p| packages::PackageRef {
                kind: p.kind,
                name: p.name.clone(),
            })
            .collect();
        self.graph
    }

    fn push_node(&mut self, node: Node, lang: Lang) -> usize {
        self.graph.nodes.push(node);
        self.lang_of_node.push(lang);
        self.graph.nodes.len() - 1
    }

    fn add_files_and_defs(&mut self) {
        for f in self.facts {
            let file_node = self.push_node(
                Node {
                    id: f.path.clone(),
                    kind: SymKind::File,
                    name: f.path.rsplit('/').next().unwrap_or(&f.path).to_string(),
                    file: f.path.clone(),
                    lang: Some(f.lang),
                    line: 1,
                    end_line: f.lines,
                    signature: String::new(),
                    doc: None,
                    parent: None,
                    community: None,
                },
                f.lang,
            );
            self.file_nodes.push(file_node);
            self.file_by_path.insert(f.path.clone(), file_node);
            let is_js = f.lang.family() == Lang::JavaScript.family();
            // No `import`, `export` or CommonJS exports: a plain script (`require()` and
            // dynamic `import()` don't make a file a module).
            if is_js && !f.is_module && !f.has_exports {
                self.script_files.insert(file_node);
            }
            self.index_file_path(&f.path, file_node);

            // Defs are sorted by (start, -end): a stack yields the innermost container.
            let mut nodes = Vec::with_capacity(f.defs.len());
            let mut stack: Vec<(usize, usize, usize)> = Vec::new(); // (end_byte, node, def index)
            let mut parents = Vec::with_capacity(f.defs.len());
            for (di, d) in f.defs.iter().enumerate() {
                while stack.last().is_some_and(|&(end, _, _)| end <= d.start_byte) {
                    stack.pop();
                }
                let parent = stack.last().map(|&(_, n, _)| n).unwrap_or(file_node);
                parents.push(stack.last().map(|&(_, _, i)| i));
                let mut kind = d.kind;
                if kind == SymKind::Function
                    && (self.graph.nodes[parent].kind.is_type() || d.owner.is_some())
                {
                    kind = SymKind::Method;
                }
                let node = self.push_node(
                    Node {
                        id: String::new(),
                        kind,
                        name: d.name.clone(),
                        file: f.path.clone(),
                        lang: Some(f.lang),
                        line: d.start_line,
                        end_line: d.end_line,
                        signature: d.signature.clone(),
                        doc: d.doc.clone(),
                        parent: Some(parent),
                        community: None,
                    },
                    f.lang,
                );
                self.names.entry(d.name.clone()).or_default().push(node);
                self.def_ranges.insert(node, (d.start_byte, d.end_byte));
                self.child_ranges
                    .entry(parent)
                    .or_default()
                    .push((d.start_byte, d.end_byte));
                // Without any `export`, a JS file is CommonJS or a script: nothing is hidden.
                if is_js && f.has_exports && !d.exported {
                    self.hidden.insert(node);
                }
                stack.push((d.end_byte, node, di));
                nodes.push(node);
            }
            self.def_nodes.push(nodes);
            self.def_parents.push(parents);
            self.impl_owner.push(vec![None; f.impls.len()]);
        }
    }

    fn index_file_path(&mut self, path: &str, node: usize) {
        let stem = match path.rsplit_once('.') {
            Some((s, _)) => s.to_string(),
            None => path.to_string(),
        };
        let dir = parent_dir(path);
        let base = stem.rsplit('/').next().unwrap_or(&stem);
        let mut stems = vec![stem.clone()];
        if matches!(base, "mod" | "__init__" | "index" | "lib") {
            stems.push(dir.clone());
        }
        for s in stems {
            self.stems.entry(s.clone()).or_insert(node);
            for suffix in path_suffixes(&s) {
                self.stem_suffixes.entry(suffix).or_default().push(node);
            }
        }
        let files = self.dirs.entry(dir.clone()).or_default();
        if files.is_empty() {
            for suffix in path_suffixes(&dir) {
                self.dir_suffixes
                    .entry(suffix)
                    .or_default()
                    .push(dir.clone());
            }
        }
        files.push(node);
    }

    /// Members declared in `impl`/`extension` blocks or with Go receivers belong to a
    /// type that may live in another file.
    fn resolve_owners(&mut self) {
        for (fi, f) in self.facts.iter().enumerate() {
            for (ii, imp) in f.impls.iter().enumerate() {
                self.impl_owner[fi][ii] = self.find_type(&imp.type_name, fi);
            }
            for (di, d) in f.defs.iter().enumerate() {
                let node = self.def_nodes[fi][di];
                let owner_name = d.owner.clone().or_else(|| {
                    // innermost impl containing this def, unless a def nested deeper does
                    let parent = self.graph.nodes[node].parent?;
                    let parent_start = if self.graph.nodes[parent].kind == SymKind::File {
                        0
                    } else {
                        f.defs[self.def_nodes[fi].iter().position(|&n| n == parent)?].start_byte
                    };
                    f.impls
                        .iter()
                        .filter(|i| i.start_byte <= d.start_byte && d.end_byte <= i.end_byte)
                        .filter(|i| i.start_byte >= parent_start)
                        .max_by_key(|i| i.start_byte)
                        .map(|i| i.type_name.clone())
                });
                let Some(owner_name) = owner_name else {
                    continue;
                };
                if self.graph.nodes[node].kind == SymKind::Function {
                    self.graph.nodes[node].kind = SymKind::Method;
                }
                match self.find_type(&owner_name, fi) {
                    Some((owner, conf)) if owner != node => {
                        self.graph.nodes[node].parent = Some(owner);
                        if !conf.is_exact() {
                            self.owner_confidence.insert(node, conf);
                        }
                    }
                    // Type not in the repo: keep the owner in the qualified name (see `qualified`).
                    _ => self.graph.nodes[node].id = format!("{owner_name}::{}", d.name),
                }
            }
        }
    }

    /// The type named `name` as seen from file `fi`: same file, else an imported file, else
    /// same directory, else the only one in the repository.
    fn find_type(&self, name: &str, fi: usize) -> Option<(usize, Confidence)> {
        let family = self.facts[fi].lang.family();
        let cands = || {
            self.names
                .get(name)
                .into_iter()
                .flatten()
                .copied()
                .filter(move |&n| {
                    self.graph.nodes[n].kind.is_type() && self.lang_of_node[n].family() == family
                })
        };
        let file = &self.facts[fi].path;
        let dir = parent_dir(file);
        let imported = self.imported.get(&self.file_nodes[fi]);
        pick_one(
            cands().filter(|&n| &self.graph.nodes[n].file == file),
            Confidence::Exact,
        )
        .or_else(|| {
            let imported = imported?;
            pick_one(
                cands().filter(|&n| {
                    self.file_by_path
                        .get(&self.graph.nodes[n].file)
                        .is_some_and(|f| imported.contains(f))
                }),
                Confidence::Exact,
            )
        })
        .or_else(|| {
            pick_one(
                cands().filter(|&n| parent_dir(&self.graph.nodes[n].file) == dir),
                Confidence::Inferred,
            )
        })
        .or_else(|| {
            let mut all = cands();
            match (all.next(), all.next()) {
                (Some(t), None) => Some((t, Confidence::Inferred)),
                _ => None,
            }
        })
    }

    fn assign_ids(&mut self) {
        let mut memo: HashMap<usize, String> = HashMap::new();
        let mut seen: HashMap<String, u32> = HashMap::new();
        for i in 0..self.graph.nodes.len() {
            if self.graph.nodes[i].kind == SymKind::File {
                continue;
            }
            let q = self.qualified(i, &mut memo, 0);
            let base = format!("{}::{}", self.graph.nodes[i].file, q);
            let n = seen.entry(base.clone()).or_insert(0);
            *n += 1;
            self.graph.nodes[i].id = if *n == 1 { base } else { format!("{base}#{n}") };
        }
    }

    fn qualified(&self, i: usize, memo: &mut HashMap<usize, String>, depth: u32) -> String {
        if let Some(q) = memo.get(&i) {
            return q.clone();
        }
        let node = &self.graph.nodes[i];
        // A pre-set id carries an unresolved owner prefix (`Owner::name`).
        let own = if node.id.is_empty() {
            node.name.clone()
        } else {
            node.id.clone()
        };
        let q = match node.parent {
            Some(p) if self.graph.nodes[p].kind != SymKind::File && depth < 16 => {
                format!("{}::{own}", self.qualified(p, memo, depth + 1))
            }
            _ => own,
        };
        memo.insert(i, q.clone());
        q
    }

    fn add_contains_edges(&mut self) {
        for i in 0..self.graph.nodes.len() {
            if let Some(p) = self.graph.nodes[i].parent {
                let line = self.graph.nodes[i].line;
                // A member attached to a guessed owner type is only as sure as that guess.
                let conf = self
                    .owner_confidence
                    .get(&i)
                    .copied()
                    .unwrap_or(Confidence::Exact);
                self.add_edge(p, i, EdgeKind::Contains, line, conf, None);
            }
        }
    }

    /// Adds (or merges into) an edge. `in_file` is the file the occurrence was found in:
    /// its line is a site of the edge only when that is `from`'s own file (a reference
    /// attributed to an owner type declared elsewhere has no line there).
    fn add_edge(
        &mut self,
        from: usize,
        to: usize,
        kind: EdgeKind,
        line: u32,
        conf: Confidence,
        in_file: Option<usize>,
    ) {
        let site = (kind != EdgeKind::Contains
            && in_file.is_some_and(|fi| self.facts[fi].path == self.graph.nodes[from].file))
        .then_some(line);
        if let Some(&e) = self.edge_index.get(&(from, to, kind)) {
            let edge = &mut self.graph.edges[e];
            edge.count += 1;
            edge.line = edge.line.min(line);
            if let Some(site) = site
                && let Err(at) = edge.lines.binary_search(&site)
            {
                edge.lines.insert(at, site);
            }
            // One sure occurrence is enough to trust the relation.
            edge.confidence = edge.confidence.max(conf);
            return;
        }
        self.edge_index
            .insert((from, to, kind), self.graph.edges.len());
        self.graph.edges.push(Edge {
            from,
            to,
            kind,
            count: 1,
            confidence: conf,
            line,
            lines: site.into_iter().collect(),
        });
    }

    /// Node of a runtime global (`localStorage`, `fetch`), created on first use.
    fn runtime_global(&mut self, name: &str) -> usize {
        let id = format!("global:{name}");
        if let Some(&n) = self.externals.get(&id) {
            return n;
        }
        let n = self.external_node(id.clone(), name, "runtime global");
        self.externals.insert(id, n);
        n
    }

    /// The JS/TS runtime global the call `r` in file `fi` uses, if any, and the edge to
    /// it: `localStorage.getItem()`, `window.sessionStorage.setItem()` (references to the
    /// object), `fetch()` or `window.fetch()` (a call). Only from the root of the chain
    /// (`vscode.window.show()` is no use of `window`), and not when that root is a local
    /// binding, an import or a top-level definition of the file.
    fn global_used<'r>(&self, fi: usize, r: &'r extract::Ref) -> Option<(&'r str, EdgeKind)> {
        if r.local_root {
            return None;
        }
        let path: Vec<&str> = r
            .qualifier_path
            .as_deref()
            .map(|p| p.split('.').map(|s| s.trim_end_matches('?')).collect())
            .unwrap_or_default();
        let host = |s: &str| JS_GLOBAL_HOSTS.contains(&s);
        let object = |s: &str| JS_GLOBAL_OBJECTS.contains(&s);
        let function = |s: &str| JS_GLOBAL_FUNCTIONS.contains(&s);
        let (global, kind) = match path.as_slice() {
            [] if function(&r.name) => (r.name.as_str(), EdgeKind::Calls),
            [h] if host(h) && function(&r.name) => (r.name.as_str(), EdgeKind::Calls),
            [h, o, ..] if host(h) && object(o) => (*o, EdgeKind::References),
            [o, ..] if object(o) => (*o, EdgeKind::References),
            _ => return None,
        };
        let root = path.first().copied().unwrap_or(global);
        let f = self.facts[fi];
        let file_node = self.file_nodes[fi];
        let shadowed = f.import_bindings.iter().any(|(name, _)| name == root)
            || f.imports.iter().any(|i| i.names.iter().any(|n| n == root))
            || self.names.get(root).is_some_and(|defs| {
                defs.iter()
                    .any(|&d| self.graph.nodes[d].parent == Some(file_node))
            });
        (!shadowed).then_some((global, kind))
    }

    /// A node outside the repository (a package, a runtime global).
    fn external_node(&mut self, id: String, name: &str, signature: &str) -> usize {
        self.push_node(
            Node {
                id,
                kind: SymKind::External,
                name: name.to_string(),
                file: String::new(),
                lang: None,
                line: 0,
                end_line: 0,
                signature: signature.to_string(),
                doc: None,
                parent: None,
                community: None,
            },
            Lang::Rust, // unused for externals
        )
    }

    fn external(&mut self, name: &str) -> usize {
        if let Some(&n) = self.externals.get(name) {
            return n;
        }
        let n = self.external_node(format!("ext:{name}"), name, "");
        self.externals.insert(name.to_string(), n);
        n
    }

    /// Maps local package names to their indexed entry files.
    fn map_packages(&mut self) {
        let mut npm = HashMap::new();
        let mut crates = HashMap::new();
        for (k, p) in self.packages.iter().enumerate() {
            let entry = p.entries.iter().find_map(|e| self.lookup_stem(e));
            match p.kind {
                PackageKind::Npm => {
                    npm.insert(p.name.clone(), (k, entry));
                }
                PackageKind::Crate => {
                    if let Some(lib) = entry {
                        crates.insert(
                            p.name.clone(),
                            (parent_dir(&self.graph.nodes[lib].file), lib),
                        );
                    }
                }
            }
        }
        self.npm_packages = npm;
        self.crates = crates;
    }

    /// The file node for a module path, with or without its extension (`a/b.ts`, `a/b`,
    /// or a directory with an index file).
    fn lookup_stem(&self, p: &str) -> Option<usize> {
        let p = p.trim_end_matches('/');
        let without_ext = match p.rsplit_once('.') {
            Some((s, ext)) if !s.is_empty() && !ext.contains('/') => s,
            _ => p,
        };
        self.stems.get(p).or(self.stems.get(without_ext)).copied()
    }

    /// Import targets of file `fi` (repo files and external packages).
    fn import_targets(&self, fi: usize, imp: &'a extract::Import) -> Vec<Target> {
        let f: &'a FileFacts = self.facts[fi];
        match f.lang {
            Lang::Rust => self.resolve_rust_import(&f.path, imp),
            Lang::Python => self.resolve_python_import(&f.path, imp),
            Lang::JavaScript | Lang::TypeScript | Lang::Tsx => {
                self.resolve_js_import(&f.path, &imp.path)
            }
            Lang::Go => self.resolve_go_import(&imp.path),
            Lang::Swift => vec![Target::External(imp.path.clone())],
        }
    }

    /// Fills `imported` (file -> repo files it imports) without adding edges or nodes.
    /// Importing a JS/TS barrel (`export … from`) also counts as importing what it
    /// re-exports, transitively.
    fn collect_imported_files(&mut self) {
        for f in self.facts {
            let unresolved = f
                .import_bindings
                .iter()
                .filter(|(_, path)| {
                    !self
                        .resolve_js_import(&f.path, path)
                        .iter()
                        .any(|t| matches!(t, Target::File(_)))
                })
                .map(|(name, _)| name.clone())
                .collect();
            self.unresolved_imports.push(unresolved);
        }
        for (fi, f) in self.facts.iter().enumerate() {
            let from = self.file_nodes[fi];
            for imp in &f.imports {
                for t in self.import_targets(fi, imp) {
                    if let Target::File(n) = t
                        && n != from
                    {
                        self.imported.entry(from).or_default().insert(n);
                        if imp.reexport {
                            self.reexports.entry(from).or_default().insert(n);
                        }
                    }
                }
            }
        }
        if self.reexports.is_empty() {
            return;
        }
        for set in self.imported.values_mut() {
            let mut queue: Vec<usize> = set.iter().copied().collect();
            while let Some(f) = queue.pop() {
                for &n in self.reexports.get(&f).into_iter().flatten() {
                    if set.insert(n) {
                        queue.push(n);
                    }
                }
            }
        }
    }

    fn resolve_imports(&mut self) {
        for (fi, f) in self.facts.iter().enumerate() {
            let from = self.file_nodes[fi];
            for imp in &f.imports {
                for t in self.import_targets(fi, imp) {
                    let to = match t {
                        Target::File(n) if n == from => continue,
                        Target::File(n) => {
                            self.imported.entry(from).or_default().insert(n);
                            n
                        }
                        Target::External(name) => self.external(&name),
                    };
                    self.add_edge(
                        from,
                        to,
                        EdgeKind::Imports,
                        imp.line,
                        Confidence::Exact,
                        Some(fi),
                    );
                }
            }
        }
    }

    fn resolve_rust_import(&self, file: &str, imp: &extract::Import) -> Vec<Target> {
        let dir = parent_dir(file);
        let stem = file.trim_end_matches(".rs");
        let file_name = stem.rsplit('/').next().unwrap_or(stem);
        // Directory holding this module's children.
        let module_dir = if matches!(file_name, "mod" | "lib" | "main") {
            dir.clone()
        } else {
            stem.to_string()
        };
        if imp.is_mod {
            let cand = join(&module_dir, &imp.path);
            return self
                .stems
                .get(&cand)
                .map(|&n| Target::File(n))
                .into_iter()
                .collect();
        }
        let crate_root = crate_root(file);
        let mut out = Vec::new();
        for path in expand_rust_use(&imp.path) {
            let segs: Vec<&str> = path.split("::").filter(|s| !s.is_empty()).collect();
            let Some(&first) = segs.first() else { continue };
            let (base, rest): (String, &[&str]) = match first {
                "crate" => (crate_root.clone(), &segs[1..]),
                "self" => (module_dir.clone(), &segs[1..]),
                "super" => {
                    let mut base = parent_dir(&module_dir);
                    let mut rest = &segs[1..];
                    while rest.first() == Some(&"super") {
                        base = parent_dir(&base);
                        rest = &rest[1..];
                    }
                    (base, rest)
                }
                // Another crate of this repository: resolve inside it.
                other if self.crates.contains_key(other) => {
                    let (src, lib) = &self.crates[other];
                    if segs.len() == 1 {
                        out.push(Target::File(*lib));
                        continue;
                    }
                    (src.clone(), &segs[1..])
                }
                _ => (module_dir.clone(), &segs[..]),
            };
            let mut found = None;
            for k in (1..=rest.len()).rev() {
                let cand = join(&base, &rest[..k].join("/"));
                if let Some(&n) = self.stems.get(&cand) {
                    found = Some(n);
                    break;
                }
                // 2018 paths relative to the crate root (`use graph::Graph` in main.rs);
                // not for another local crate, resolved in its own tree only.
                if self.crates.contains_key(first) {
                    continue;
                }
                let cand = join(&crate_root, &rest[..k].join("/"));
                if let Some(&n) = self.stems.get(&cand) {
                    found = Some(n);
                    break;
                }
            }
            match found {
                Some(n) => out.push(Target::File(n)),
                // An item of a local crate not found as a module: its lib re-exports it.
                None if self.crates.contains_key(first) => {
                    out.push(Target::File(self.crates[first].1))
                }
                None if !matches!(first, "crate" | "self" | "super") => {
                    out.push(Target::External(first.to_string()))
                }
                None => {}
            }
        }
        out
    }

    fn resolve_python_import(&self, file: &str, imp: &extract::Import) -> Vec<Target> {
        let dots = imp.path.chars().take_while(|&c| c == '.').count();
        let module = &imp.path[dots..];
        let mut bases = Vec::new();
        if dots > 0 {
            let mut base = parent_dir(file);
            for _ in 1..dots {
                base = parent_dir(&base);
            }
            bases.push(base);
        }
        let mut paths: Vec<String> = imp.names.iter().map(|n| join_dotted(module, n)).collect();
        paths.push(module.to_string());

        let mut out = Vec::new();
        for p in paths.iter().filter(|p| !p.is_empty()) {
            let segs: Vec<&str> = p.split('.').collect();
            let found = (1..=segs.len()).rev().find_map(|k| {
                let rel = segs[..k].join("/");
                if dots > 0 {
                    bases
                        .iter()
                        .find_map(|b| self.stems.get(&join(b, &rel)).copied())
                } else {
                    self.stems
                        .get(&rel)
                        .copied()
                        .or_else(|| self.best_suffix_match(&rel, file))
                }
            });
            if let Some(n) = found {
                out.push(Target::File(n));
                break;
            }
        }
        if out.is_empty() && dots == 0 && !module.is_empty() {
            out.push(Target::External(
                module.split('.').next().unwrap_or(module).to_string(),
            ));
        }
        out
    }

    /// `resolve_js_import_uncached`, once per (file, specifier): imports are resolved
    /// in several passes.
    fn resolve_js_import(&self, file: &'a str, spec: &'a str) -> Vec<Target> {
        if let Some(found) = self.js_imports.borrow().get(&(file, spec)) {
            return found.clone();
        }
        let found = self.resolve_js_import_uncached(file, spec);
        self.js_imports
            .borrow_mut()
            .insert((file, spec), found.clone());
        found
    }

    fn resolve_js_import_uncached(&self, file: &str, spec: &str) -> Vec<Target> {
        let lookup = |p: &str| self.lookup_stem(p);
        if spec.starts_with('.') {
            let p = normalize(&join(&parent_dir(file), spec));
            return lookup(&p).map(Target::File).into_iter().collect();
        }
        // Path aliases such as `@/components/Button` or `~/lib/api`.
        if let Some(rest) = spec.strip_prefix("@/").or(spec.strip_prefix("~/")) {
            let rest = rest.trim_end_matches('/');
            if let Some(n) = lookup(rest).or_else(|| self.best_suffix_match(rest, file)) {
                return vec![Target::File(n)];
            }
        }
        let pkg = if spec.starts_with('@') {
            spec.splitn(3, '/').take(2).collect::<Vec<_>>().join("/")
        } else {
            spec.split('/').next().unwrap_or(spec).to_string()
        };
        // A package of this repository (a workspace): its entry file, an `exports`
        // subpath, or a file under it (built outputs map to `src/`).
        if let Some(&(k, entry)) = self.npm_packages.get(&pkg) {
            let package = &self.packages[k];
            let sub = spec[pkg.len()..].trim_start_matches('/');
            let found = if sub.is_empty() {
                entry
            } else {
                let mut cands: Vec<String> = package
                    .subpaths
                    .iter()
                    .filter(|(key, _)| key == sub)
                    .flat_map(|(_, files)| files.iter().cloned())
                    .collect();
                cands.push(join(&package.dir, sub));
                cands.push(join(&package.dir, &format!("src/{sub}")));
                for built in ["dist/", "build/", "lib/", "out/"] {
                    if let Some(rest) = sub.strip_prefix(built) {
                        cands.push(join(&package.dir, &format!("src/{rest}")));
                    }
                }
                cands.iter().find_map(|c| self.lookup_stem(c))
            };
            if let Some(n) = found {
                return vec![Target::File(n)];
            }
        }
        vec![Target::External(pkg)]
    }

    fn resolve_go_import(&self, spec: &str) -> Vec<Target> {
        let segs: Vec<&str> = spec.split('/').collect();
        for k in 0..segs.len() {
            let suffix = segs[k..].join("/");
            if let Some(dirs) = self.dir_suffixes.get(&suffix)
                && dirs.len() == 1
            {
                return self.dirs[&dirs[0]]
                    .iter()
                    .map(|&n| Target::File(n))
                    .collect();
            }
        }
        vec![Target::External(spec.to_string())]
    }

    /// Among files whose path ends with `rel`, picks the one closest to `from_file`.
    fn best_suffix_match(&self, rel: &str, from_file: &str) -> Option<usize> {
        let cands = self.stem_suffixes.get(rel)?;
        cands
            .iter()
            .max_by_key(|&&n| common_prefix_len(&self.graph.nodes[n].file, from_file))
            .copied()
    }

    /// `impl Trait for Type` / `extension Type: Protocol` become inheritance edges.
    fn resolve_impls(&mut self) {
        for (fi, f) in self.facts.iter().enumerate() {
            for (ii, imp) in f.impls.iter().enumerate() {
                let (Some((owner, owner_conf)), Some(tr)) =
                    (self.impl_owner[fi][ii], imp.trait_name.as_ref())
                else {
                    continue;
                };
                if let Some((t, conf)) = self.find_type(tr, fi) {
                    let conf = conf.min(owner_conf);
                    self.add_edge(owner, t, EdgeKind::Inherits, imp.line, conf, Some(fi));
                }
            }
        }
    }

    /// Resets the per-file mention lists (built on first use by `names_in`).
    fn index_mentions(&mut self) {
        self.mentions = (0..self.facts.len())
            .map(|_| std::cell::OnceCell::new())
            .collect();
    }

    /// Names and qualifiers used in file `fi` within `range`, outside `excluded`.
    fn names_in<'s>(
        &'s self,
        fi: usize,
        range: (usize, usize),
        excluded: &'s [(usize, usize)],
    ) -> impl Iterator<Item = &'a str> + 's {
        let all = self.mentions[fi].get_or_init(|| {
            let f: &'a FileFacts = self.facts[fi];
            let mut v: Vec<(usize, &'a str)> = Vec::with_capacity(f.refs.len());
            for r in &f.refs {
                v.push((r.byte, r.name.as_str()));
                if let Some(q) = &r.qualifier {
                    v.push((r.byte, q.as_str()));
                }
            }
            v.sort_unstable();
            v
        });
        let from = all.partition_point(|&(b, _)| b < range.0);
        all[from..]
            .iter()
            .take_while(move |&&(b, _)| b < range.1)
            .filter(move |&&(b, _)| !excluded.iter().any(|&(s, e)| s <= b && b < e))
            .map(|&(_, name)| name)
    }

    /// Fills `node_file`, `node_dir` and `node_crate` for every node.
    fn locate_nodes(&mut self) {
        let mut dirs: HashMap<String, u32> = HashMap::new();
        let mut crates: HashMap<String, u32> = HashMap::new();
        let intern = |map: &mut HashMap<String, u32>, key: String| -> u32 {
            let next = map.len() as u32;
            *map.entry(key).or_insert(next)
        };
        let mut stems: HashMap<String, u32> = HashMap::new();
        let mut dirnames: HashMap<String, u32> = HashMap::new();
        let n = self.graph.nodes.len();
        let (mut file, mut dir, mut krate) = (vec![NONE; n], vec![u32::MAX; n], vec![u32::MAX; n]);
        let (mut stem, mut dirname) = (vec![u32::MAX; n], vec![u32::MAX; n]);
        for i in 0..n {
            let path = &self.graph.nodes[i].file;
            let Some(&f) = self.file_by_path.get(path) else {
                continue;
            };
            file[i] = f;
            dir[i] = intern(&mut dirs, parent_dir(path));
            krate[i] = intern(&mut crates, crate_root(path));
            let mut parts = path.rsplit('/');
            let file_stem = parts.next().and_then(|f| f.split('.').next()).unwrap_or("");
            stem[i] = intern(&mut stems, file_stem.to_string());
            dirname[i] = intern(&mut dirnames, parts.next().unwrap_or("").to_string());
        }
        self.node_file = file;
        self.node_dir = dir;
        self.node_crate = krate;
        self.node_stem = stem;
        self.node_dirname = dirname;
        self.stem_ids = stems;
        self.dirname_ids = dirnames;

        let nodes = &self.graph.nodes;
        let mut parent_names: HashMap<String, u32> = HashMap::new();
        self.node_parent_name = nodes
            .iter()
            .map(|node| match node.parent {
                Some(p) => intern(&mut parent_names, nodes[p].name.clone()),
                None => u32::MAX,
            })
            .collect();
        self.parent_name_ids = parent_names;
        let container = |k: SymKind| k.is_type() || k == SymKind::Module;
        self.member_reachable = (0..n)
            .map(|i| {
                let file = self.node_file[i];
                let mut owner = nodes[i].parent;
                if !owner.is_some_and(|p| container(nodes[p].kind)) {
                    return false;
                }
                while let Some(o) = owner.filter(|&o| o != file) {
                    if !container(nodes[o].kind) {
                        return false;
                    }
                    owner = nodes[o].parent;
                }
                true
            })
            .collect();
    }

    fn resolve_refs(&mut self) {
        self.locate_nodes();
        self.index_mentions();
        self.imports_file = vec![false; self.graph.nodes.len()];
        for fi in 0..self.facts.len() {
            let f = self.facts[fi];
            let is_js = f.lang.family() == Lang::JavaScript.family();
            let imported: Vec<usize> = self
                .imported
                .get(&self.file_nodes[fi])
                .map_or_else(Vec::new, |set| set.iter().copied().collect());
            for &file in &imported {
                self.imports_file[file] = true;
            }
            for r in &f.refs {
                if is_js
                    && r.kind == RefKind::Call
                    && let Some((global, kind)) = self.global_used(fi, r)
                    && let Some(scope) = self.scope_of(fi, r.byte, r.kind)
                {
                    let target = self.runtime_global(global);
                    self.add_edge(scope, target, kind, r.line, Confidence::Exact, Some(fi));
                }
                // Most identifiers name nothing in the repo: skip them before the scope scan.
                let Some(named) = self.names.get(&r.name) else {
                    continue;
                };
                if r.kind == RefKind::Value
                    && !named
                        .iter()
                        .any(|&n| self.graph.nodes[n].kind == SymKind::Constant)
                {
                    continue;
                }
                let Some(scope) = self.scope_of(fi, r.byte, r.kind) else {
                    continue;
                };
                let (kind, accept): (EdgeKind, fn(SymKind) -> bool) = match r.kind {
                    RefKind::Call => (EdgeKind::Calls, SymKind::is_callable),
                    RefKind::Base => (EdgeKind::Inherits, SymKind::is_type),
                    RefKind::TypeRef => (EdgeKind::References, SymKind::is_type),
                    RefKind::Value => (EdgeKind::References, |k| k == SymKind::Constant),
                };
                let (targets, conf) = self.pick_targets(fi, scope, r, accept);
                for t in targets {
                    if t == scope && kind != EdgeKind::Calls {
                        continue;
                    }
                    // A type referencing one of its own members is just containment.
                    if kind == EdgeKind::References && self.graph.nodes[scope].parent == Some(t) {
                        continue;
                    }
                    self.add_edge(scope, t, kind, r.line, conf, Some(fi));
                }
            }
            for &file in &imported {
                self.imports_file[file] = false;
            }
        }
    }

    /// The innermost definition containing `byte`, else the owner of the innermost impl
    /// block, else the file itself.
    fn scope_of(&self, fi: usize, byte: usize, kind: RefKind) -> Option<usize> {
        let f = &self.facts[fi];
        // Innermost def containing `byte`: the last one starting at or before it, or one
        // of that def's ancestors (defs are sorted by start and properly nested).
        let mut cand = f
            .defs
            .partition_point(|d| d.start_byte <= byte)
            .checked_sub(1);
        while let Some(i) = cand {
            if byte < f.defs[i].end_byte {
                break;
            }
            cand = self.def_parents[fi][i];
        }
        let def = cand.map(|i| (self.def_nodes[fi][i], f.defs[i].start_byte));
        let imp = f
            .impls
            .iter()
            .enumerate()
            .filter(|(_, i)| i.start_byte <= byte && byte < i.end_byte)
            .max_by_key(|(_, i)| i.start_byte);
        match (def, imp) {
            (Some((n, start)), Some((_, i))) if start >= i.start_byte => Some(n),
            (Some((n, _)), None) => Some(n),
            // Inside an impl/extension header but not in a member.
            (_, Some((ii, _))) => {
                if kind == RefKind::TypeRef {
                    return None;
                }
                Some(self.impl_owner[fi][ii].map_or(self.file_nodes[fi], |(t, _)| t))
            }
            (None, None) => Some(self.file_nodes[fi]),
        }
    }

    /// Definitions a reference may point to, and how sure the match is.
    fn pick_targets(
        &self,
        fi: usize,
        scope: usize,
        r: &'a extract::Ref,
        accept: fn(SymKind) -> bool,
    ) -> (Vec<usize>, Confidence) {
        let none = (vec![], Confidence::Ambiguous);
        let family = self.facts[fi].lang.family();
        let family_id = self.facts[fi].lang.family_id();
        let Some(all) = self.names.get(&r.name) else {
            return none;
        };
        // The same for every reference with that name, kind and language: computed once.
        let ref_kind = match r.kind {
            RefKind::Call => 0,
            RefKind::Base => 1,
            RefKind::TypeRef => 2,
            RefKind::Value => 3,
        };
        let is_js = family == Lang::JavaScript.family();
        let mut cache = self.candidates.borrow_mut();
        let all = cache
            .entry((r.name.as_str(), family_id, ref_kind))
            .or_insert_with(|| {
                all.iter()
                    .copied()
                    .filter(|&n| accept(self.graph.nodes[n].kind))
                    .filter(|&n| self.lang_of_node[n].family_id() == family_id)
                    .map(|n| (n, is_js && self.js_visible_qualified_elsewhere(n)))
                    .collect()
            });
        let file_node = self.file_nodes[fi];
        let mut cands: Vec<usize> = if !is_js {
            all.iter().map(|&(n, _)| n).collect()
        } else if r.qualifier.is_some() {
            // Only same-file candidates depend on where the reference is.
            all.iter()
                .filter(|&&(n, elsewhere)| {
                    if self.node_file[n] == file_node {
                        self.js_visible(fi, scope, n, r)
                    } else {
                        elsewhere
                    }
                })
                .map(|&(n, _)| n)
                .collect()
        } else {
            all.iter()
                .map(|&(n, _)| n)
                .filter(|&n| self.js_visible(fi, scope, n, r))
                .collect()
        };
        drop(cache);

        // Receiver/path qualifiers narrow the candidates down. `risky` marks method calls
        // on values whose type we cannot know; `qualified` a match on the qualifier's name.
        let mut risky = false;
        let mut qualified = false;
        let mut unknown_receiver = false;
        if let Some(q) = r.qualifier.as_deref() {
            if matches!(q, "self" | "this" | "Self") {
                let owner = self.enclosing_type(scope);
                let own: Vec<usize> = cands
                    .iter()
                    .copied()
                    .filter(|&n| owner.is_some() && self.graph.nodes[n].parent == owner)
                    .collect();
                if !own.is_empty() {
                    // The owner is certain; several hits are overloads of one member.
                    return (own, Confidence::Exact);
                }
                // Inherited member: any method with that name.
                cands.retain(|&n| self.graph.nodes[n].kind == SymKind::Method);
                risky = true;
            } else {
                let path = r.qualifier_path.as_deref().unwrap_or(q);
                let module_like = (
                    // Rust: `crate::store::load()` is a path; `s.communities.len()` a field.
                    r.is_static,
                    // Others: `utils.f()`, or a dotted path the file imports (`pkg.utils.f()`).
                    !path.contains(['.', ':', '(', '[', '-'])
                        || self.facts[fi].imports.iter().any(|i| i.path == path),
                );
                let q_ids = (
                    self.parent_name_ids.get(q).copied(),
                    self.stem_ids.get(q).copied(),
                    self.dirname_ids.get(q).copied(),
                );
                // No parent, file or directory has that name: nothing to compare.
                let matched: Vec<usize> = if q_ids == (None, None, None) {
                    Vec::new()
                } else {
                    cands
                        .iter()
                        .copied()
                        .filter(|&n| self.qualifier_matches(n, module_like, q_ids))
                        .collect()
                };
                if !matched.is_empty() {
                    cands = matched;
                    qualified = true;
                } else if r.is_static || q.starts_with(char::is_uppercase) {
                    // `Vec::new()`, `Math.max()`: a type/module outside the repo.
                    return none;
                } else {
                    // `value.method()` can only target methods.
                    cands.retain(|&n| self.graph.nodes[n].kind == SymKind::Method);
                    risky = true;
                    unknown_receiver = true;
                }
            }
        }
        if cands.is_empty() {
            return none;
        }
        let common = COMMON_METHODS.contains(&r.name.as_str());
        // `x.len()`, `it.filter(…)` on an unknown receiver are almost always library
        // methods: link a same-named repo method only when the calling code names its
        // type (an annotation, `WGraph::new()`, a reference).
        // The evidence: the types (by node) whose names the scope uses.
        let evidence: Option<Rc<Vec<usize>>> = (unknown_receiver && common).then(|| {
            // The same for every reference in the scope: computed once.
            let mut cache = self.named_types.borrow_mut();
            let types = cache.entry((fi, scope)).or_insert_with(|| {
                let mut types: Vec<usize> = self
                    .scope_names(fi, scope)
                    .into_iter()
                    .filter_map(|name| self.names.get(name))
                    .flatten()
                    .copied()
                    .filter(|&t| self.graph.nodes[t].kind.is_type())
                    .collect();
                types.sort_unstable();
                Rc::new(types)
            });
            Rc::clone(types)
        });
        let evidenced = evidence.is_some();
        if let Some(types) = &evidence {
            cands.retain(|&n| {
                self.graph.nodes[n]
                    .parent
                    .is_some_and(|p| types.binary_search(&p).is_ok())
            });
            if cands.is_empty() {
                return none;
            }
        }
        // A unique match: `sure` when found where it should be (same or imported file) or
        // named by a qualifier; an unknown receiver lowers it, a common method name on an
        // unknown receiver is a guess.
        // With the type named nearby, a common method is no longer a blind guess.
        let common = common && !evidenced;
        let unique = |sure: bool, risky: bool| -> Confidence {
            if risky && common {
                Confidence::Ambiguous
            } else if (sure || qualified) && !risky {
                Confidence::Exact
            } else {
                Confidence::Inferred
            }
        };

        let file_node = self.file_nodes[fi];
        let (dir, krate) = (self.node_dir[file_node], self.node_crate[file_node]);
        let is_rust = self.facts[fi].lang == Lang::Rust;
        // The nearest of: same file, an imported file (a unique hit there is exact), same
        // directory, same Rust crate.
        let tier_of = |n: usize| -> u8 {
            let file = self.node_file[n];
            if file == file_node {
                0
            } else if file != NONE && self.imports_file[file] {
                1
            } else if self.node_dir[n] == dir {
                2
            } else if is_rust && self.node_crate[n] == krate {
                3
            } else {
                4
            }
        };
        let tiers: Vec<u8> = cands.iter().map(|&n| tier_of(n)).collect();
        let nearest = tiers.iter().copied().min().unwrap_or(4);
        if nearest < 4 {
            let hits: Vec<usize> = (0..cands.len())
                .filter(|&k| tiers[k] == nearest)
                .map(|k| cands[k])
                .collect();
            return match hits.len() {
                1 => (hits, unique(nearest <= 1, risky)),
                k if k <= MAX_AMBIGUOUS => (hits, Confidence::Ambiguous),
                _ => none,
            };
        }

        // Global fallback. Rust crates only see each other through imports, which we
        // resolve to externals, so a match in another crate is only a guess.
        if is_rust {
            risky = true;
        }
        if risky && (common || cands.len() > 1) {
            return none;
        }
        match cands.len() {
            // Only a cross-crate Rust match gets here while risky: a guess.
            1 if risky => (cands, Confidence::Ambiguous),
            1 => (cands, unique(false, false)),
            k if k <= MAX_AMBIGUOUS => (cands, Confidence::Ambiguous),
            _ => none,
        }
    }

    /// Names used in the code that belongs to `node` itself: its range without the
    /// definitions nested in it (the whole file without its functions, a type without its
    /// methods: its fields).
    fn own_code_names(&self, fi: usize, node: usize, out: &mut FastSet<&'a str>) {
        let range = if node == self.file_nodes[fi] {
            (0, usize::MAX)
        } else if let Some(&r) = self.def_ranges.get(&node) {
            r
        } else {
            return;
        };
        let excluded = self.child_ranges.get(&node).map_or(&[][..], Vec::as_slice);
        out.extend(self.names_in(fi, range, excluded));
    }

    /// Names the code around a reference in `scope` uses (types, values, calls,
    /// qualifiers like `Name::new()`): the scope's whole body when it is a definition of
    /// this file (else its file's top-level code), and the fields of the type it belongs
    /// to. Not the type's other methods.
    fn scope_names(&self, fi: usize, scope: usize) -> FastSet<&'a str> {
        let mut names = FastSet::default();
        match self.def_ranges.get(&scope) {
            Some(&r) if self.node_file[scope] == self.file_nodes[fi] => {
                names.extend(self.names_in(fi, r, &[]));
            }
            _ => self.own_code_names(fi, self.file_nodes[fi], &mut names),
        }
        if let Some(t) = self.graph.nodes[scope].parent.filter(|&p| {
            self.graph.nodes[p].kind.is_type() && self.node_file[p] == self.file_nodes[fi]
        }) {
            self.own_code_names(fi, t, &mut names);
        }
        names
    }

    /// JS/TS module scoping: can code in file `fi`, inside `scope`, see definition `n`
    /// through reference `r`? In its own file a definition nested in a function is visible
    /// only inside that function. From another file a top-level definition must be
    /// exported and, for a bare name (`f()`, `new C()`, `x: T`), imported: from its file
    /// (or a barrel re-exporting it), or from a source we cannot resolve; a plain script's
    /// top-level definitions are globals. Members of classes and namespaces are reachable
    /// through an instance or a qualifier, exported or not (instances escape).
    fn js_visible(&self, fi: usize, scope: usize, n: usize, r: &extract::Ref) -> bool {
        let bare = r.qualifier.is_none();
        let nodes = &self.graph.nodes;
        let Some(parent) = nodes[n].parent else {
            return true;
        };
        let parent_kind = nodes[parent].kind;
        if self.node_file[n] == self.file_nodes[fi] {
            return parent_kind == SymKind::File
                || parent_kind.is_type()
                || self.is_within(scope, parent);
        }
        let file = self.node_file[n];
        if file == NONE {
            return false;
        }
        match parent_kind {
            SymKind::File => {
                let imported =
                    || self.imports_file[file] || self.unresolved_imports[fi].contains(&r.name);
                self.script_files.contains(&file)
                    || (!self.hidden.contains(&n) && (!bare || imported()))
            }
            // A member: its owners up to the file must be types or namespaces, not
            // functions. A bare name never reaches into another file's class or namespace.
            k if k.is_type() || k == SymKind::Module => self.member_reachable[n] && !bare,
            _ => false,
        }
    }

    /// `js_visible` for a qualified reference (`x.f()`) from another file than `n`'s.
    fn js_visible_qualified_elsewhere(&self, n: usize) -> bool {
        let nodes = &self.graph.nodes;
        let Some(parent) = nodes[n].parent else {
            return true;
        };
        let file = self.node_file[n];
        if file == NONE {
            return false;
        }
        match nodes[parent].kind {
            SymKind::File => self.script_files.contains(&file) || !self.hidden.contains(&n),
            k if k.is_type() || k == SymKind::Module => self.member_reachable[n],
            _ => false,
        }
    }

    /// `node` is `ancestor` or nested in it.
    fn is_within(&self, mut node: usize, ancestor: usize) -> bool {
        loop {
            if node == ancestor {
                return true;
            }
            match self.graph.nodes[node].parent {
                Some(p) => node = p,
                None => return false,
            }
        }
    }

    /// `q.name()` / `q::name()` may target a member of type `q`, a function in module
    /// (file) `q`, or in Go package (directory) `q`.
    /// A module or package qualifier is a single name: `utils.f()` (Python/JS), `store.F()`
    /// (Go), or a `::` path in Rust; `self.graph.len()` or a Rust field `x.items.len()`
    /// names a value, whatever the file it happens to share its name with. `module_like`
    /// is that judgement for the reference, computed once: (for Rust, for other languages).
    /// `q_ids`: the ids of `q` as a parent's name, a file stem and a directory name, if any
    /// node or file has one.
    fn qualifier_matches(
        &self,
        n: usize,
        module_like: (bool, bool),
        q_ids: (Option<u32>, Option<u32>, Option<u32>),
    ) -> bool {
        if q_ids.0 == Some(self.node_parent_name[n]) {
            return true;
        }
        let module_like = if self.lang_of_node[n] == Lang::Rust {
            module_like.0
        } else {
            module_like.1
        };
        if !module_like {
            return false;
        }
        q_ids.1 == Some(self.node_stem[n])
            || (self.facts_lang(n) == Some(Lang::Go) && q_ids.2 == Some(self.node_dirname[n]))
    }

    fn facts_lang(&self, n: usize) -> Option<Lang> {
        self.graph.nodes[n].lang
    }

    /// The type a scope belongs to: itself if it is a type, else its parent type.
    fn enclosing_type(&self, scope: usize) -> Option<usize> {
        let node = &self.graph.nodes[scope];
        if node.kind.is_type() {
            return Some(scope);
        }
        node.parent.filter(|&p| self.graph.nodes[p].kind.is_type())
    }
}

#[derive(Clone)]
enum Target {
    File(usize),
    External(String),
}

/// `a::{b, c::{D, E}}` -> `a::b`, `a::c::D`, `a::c::E` (aliases and globs dropped).
fn expand_rust_use(s: &str) -> Vec<String> {
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out = Vec::new();
    expand_into("", &s, &mut out);
    out
}

fn expand_into(prefix: &str, s: &str, out: &mut Vec<String>) {
    let s = s.trim();
    if let Some(open) = s.find('{') {
        let head = s[..open].trim().trim_end_matches("::");
        let inner = s[open + 1..].strip_suffix('}').unwrap_or(&s[open + 1..]);
        let new_prefix = join_rust(prefix, head);
        let mut depth = 0;
        let mut start = 0;
        for (i, c) in inner.char_indices() {
            match c {
                '{' => depth += 1,
                '}' => depth -= 1,
                ',' if depth == 0 => {
                    expand_into(&new_prefix, &inner[start..i], out);
                    start = i + 1;
                }
                _ => {}
            }
        }
        expand_into(&new_prefix, &inner[start..], out);
    } else {
        let s = s
            .split(" as ")
            .next()
            .unwrap_or(s)
            .trim()
            .trim_end_matches("::*");
        let s = if s == "*" { "" } else { s };
        if s.is_empty() || s == "self" {
            if !prefix.is_empty() {
                out.push(prefix.to_string());
            }
        } else {
            out.push(join_rust(prefix, s));
        }
    }
}

fn join_rust(a: &str, b: &str) -> String {
    match (a.is_empty(), b.is_empty()) {
        (true, _) => b.to_string(),
        (_, true) => a.to_string(),
        _ => format!("{a}::{b}"),
    }
}

fn join_dotted(a: &str, b: &str) -> String {
    if a.is_empty() {
        b.to_string()
    } else {
        format!("{a}.{b}")
    }
}

/// The directory treated as `crate::` for a Rust file: the nearest enclosing `src`.
fn crate_root(file: &str) -> String {
    let mut dir = parent_dir(file);
    loop {
        if dir.rsplit('/').next() == Some("src") {
            return dir;
        }
        if dir.is_empty() {
            return parent_dir(file).split('/').next().unwrap_or("").to_string();
        }
        dir = parent_dir(&dir);
    }
}

/// The first of `hits` with confidence `unique` if it is the only one, else ambiguous.
fn pick_one(
    mut hits: impl Iterator<Item = usize>,
    unique: Confidence,
) -> Option<(usize, Confidence)> {
    let first = hits.next()?;
    let conf = if hits.next().is_none() {
        unique
    } else {
        Confidence::Ambiguous
    };
    Some((first, conf))
}

fn parent_dir(path: &str) -> String {
    path.rsplit_once('/')
        .map(|(d, _)| d.to_string())
        .unwrap_or_default()
}

fn join(dir: &str, rel: &str) -> String {
    if dir.is_empty() {
        rel.to_string()
    } else {
        format!("{dir}/{rel}")
    }
}

/// Resolves `.` and `..` components in a slash-separated relative path.
fn normalize(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for p in path.split('/') {
        match p {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            _ => parts.push(p),
        }
    }
    parts.join("/")
}

fn path_suffixes(path: &str) -> Vec<String> {
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    (0..parts.len()).map(|i| parts[i..].join("/")).collect()
}

fn common_prefix_len(a: &str, b: &str) -> usize {
    a.split('/')
        .zip(b.split('/'))
        .take_while(|(x, y)| x == y)
        .count()
}

fn to_unix(p: &Path) -> String {
    p.components()
        .filter_map(|c| match c {
            Component::Normal(s) => s.to_str(),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A throwaway repository with the given files, removed on drop.
    pub(crate) struct TempRepo(pub(crate) PathBuf);

    impl TempRepo {
        pub(crate) fn new(name: &str, files: &[(&str, &str)]) -> TempRepo {
            let dir =
                std::env::temp_dir().join(format!("codenav-unit-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            for (path, content) in files {
                let p = dir.join(path);
                fs::create_dir_all(p.parent().unwrap()).unwrap();
                fs::write(p, content).unwrap();
            }
            TempRepo(dir)
        }
    }

    impl Drop for TempRepo {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// The graph as JSON, without what depends on when it was built.
    fn comparable(g: &Graph) -> serde_json::Value {
        let mut v = serde_json::to_value(g).unwrap();
        for key in [
            "indexed_at",
            "indexed_at_ms",
            "git_commit",
            "git_branch",
            "git_dirty",
        ] {
            v.as_object_mut().unwrap().remove(key);
        }
        v
    }

    #[test]
    fn incremental_index_equals_a_full_one() {
        let repo = TempRepo::new(
            "incremental",
            &[
                (
                    "a.py",
                    "def helper():\n    pass\n\ndef moved():\n    pass\n",
                ),
                (
                    "b.py",
                    "from a import helper, moved\n\ndef user():\n    helper()\n    moved()\n",
                ),
                ("c.py", "def gone():\n    pass\n"),
                (
                    "d.py",
                    "from c import gone\n\ndef uses_gone():\n    gone()\n",
                ),
                ("old_name.py", "def renamed():\n    pass\n"),
            ],
        );
        let opts = IndexOptions::default();
        let mut cache = FactsCache::default();
        index_cached(&repo.0, &opts, &mut cache, None, None).unwrap();

        // Modify, add, remove, rename a file; move a symbol from a.py to e.py, whose
        // callers in other files must follow.
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(repo.0.join("a.py"), "def helper():\n    return 1\n").unwrap();
        fs::write(repo.0.join("e.py"), "def moved():\n    pass\n").unwrap();
        fs::write(
            repo.0.join("b.py"),
            "from a import helper\nfrom e import moved\n\ndef user():\n    helper()\n    moved()\n",
        )
        .unwrap();
        fs::remove_file(repo.0.join("c.py")).unwrap();
        fs::rename(repo.0.join("old_name.py"), repo.0.join("new_name.py")).unwrap();

        let (incremental, _) = index_cached(&repo.0, &opts, &mut cache, None, None).unwrap();
        let (full, _) = index_with(&repo.0, &opts).unwrap();
        assert_eq!(comparable(&incremental), comparable(&full));
        assert!(full.by_id.contains_key("e.py::moved") && !full.by_id.contains_key("c.py::gone"));
        assert_eq!(cache.files.len(), 5, "removed files leave the cache");
    }

    #[test]
    fn incremental_update_of_one_file_is_fast() {
        // About the size of the repository in the issue: 360 files, thousands of edges.
        let mut files: Vec<(String, String)> = Vec::new();
        for i in 0..360 {
            let mut src = format!(
                "import {{ f{} }} from './m{}';\n",
                (i + 1) % 360,
                (i + 1) % 360
            );
            for j in 0..6 {
                src.push_str(&format!(
                    "export function f{i}_{j}(x: number) {{\n  return f{}(x) + {j};\n}}\n",
                    (i + 1) % 360
                ));
            }
            src.push_str(&format!(
                "export function f{i}(x: number) {{ return x; }}\n"
            ));
            files.push((format!("src/m{i}.ts"), src));
        }
        let refs: Vec<(&str, &str)> = files
            .iter()
            .map(|(p, s)| (p.as_str(), s.as_str()))
            .collect();
        let repo = TempRepo::new("incremental-speed", &refs);
        let opts = IndexOptions::default();
        let mut cache = FactsCache::default();
        let (mut previous, _) = index_cached(&repo.0, &opts, &mut cache, None, None).unwrap();

        // The best of a few runs, so that a busy machine does not fail the test.
        let mut elapsed = std::time::Duration::MAX;
        for run in 0..5 {
            std::thread::sleep(std::time::Duration::from_millis(20));
            let edit = format!("+ {}", 100 + run);
            fs::write(repo.0.join("src/m7.ts"), files[7].1.replace("+ 0", &edit)).unwrap();
            let started = Instant::now();
            // As the server does: the Git state is read after the graph is published.
            let (graph, stats) =
                index_cached(&repo.0, &opts, &mut cache, None, Some(&previous)).unwrap();
            previous = graph;
            elapsed = elapsed.min(started.elapsed());
            assert_eq!(stats.files, 360);
        }
        eprintln!("incremental update of one file in 360: {elapsed:?}");
        // The target is for release builds; debug builds are several times slower.
        let limit = if cfg!(debug_assertions) { 400 } else { 20 };
        assert!(elapsed.as_millis() < limit, "{elapsed:?}");
    }

    #[test]
    fn codenavignore_excludes_paths_at_any_level() {
        let repo = TempRepo::new(
            "ignore",
            &[
                (".codenavignore", "gen/\n"),
                ("a.py", "def a(): pass\n"),
                ("gen/b.py", "def b(): pass\n"),
                ("sub/.codenavignore", "local.py\n"),
                ("sub/local.py", "def c(): pass\n"),
                ("sub/kept.py", "def d(): pass\n"),
                ("other/local.py", "def e(): pass\n"),
            ],
        );
        let files: Vec<String> = discover(&repo.0).into_iter().map(|f| f.rel).collect();
        assert_eq!(files, vec!["a.py", "other/local.py", "sub/kept.py"]);

        // Indexing an excluded directory directly still works.
        let gen_files: Vec<String> = discover(&repo.0.join("gen"))
            .into_iter()
            .map(|f| f.rel)
            .collect();
        assert_eq!(gen_files, vec!["b.py"]);
    }

    #[test]
    fn codenavignore_only_narrows_and_stays_inside_the_root() {
        let repo = TempRepo::new(
            "narrow",
            &[
                // The "only index Python" idiom must not re-include ignored paths.
                ("repo/.codenavignore", "*\n!*/\n!*.py\n"),
                ("repo/.gitignore", "secret.py\nenv/\n"),
                ("repo/a.py", ""),
                ("repo/notes.txt", ""),
                ("repo/secret.py", ""),
                ("repo/env/e.py", ""),
                ("repo/.venv/v.py", ""),
                ("repo/gen/b.py", ""),
                // Rules above the root are not read.
                (".codenavignore", "repo/\n*.py\n"),
                // `dir/**` in a parent file must not hide the directory indexed directly.
                ("repo/sub/.codenavignore", "inner/**\n"),
                ("repo/sub/inner/c.py", ""),
            ],
        );
        let root = repo.0.join("repo");
        let files: Vec<String> = discover(&root).into_iter().map(|f| f.rel).collect();
        assert_eq!(files, vec!["a.py", "gen/b.py"]);
        let inner: Vec<String> = discover(&root.join("sub/inner"))
            .into_iter()
            .map(|f| f.rel)
            .collect();
        assert_eq!(inner, vec!["c.py"]);
    }

    #[cfg(any(target_os = "macos", windows))]
    #[test]
    fn codenavignore_is_case_insensitive_like_git_on_this_platform() {
        let repo = TempRepo::new(
            "case",
            &[(".codenavignore", "gen/\n"), ("Gen/g.py", ""), ("a.py", "")],
        );
        let files: Vec<String> = discover(&repo.0).into_iter().map(|f| f.rel).collect();
        assert_eq!(files, vec!["a.py"]);
    }

    #[test]
    fn changes_to_ignored_files_do_not_make_the_index_stale() {
        let repo = TempRepo::new(
            "stale",
            &[
                (".codenavignore", "gen/\n"),
                ("a.py", "def a(): pass\n"),
                ("gen/b.py", "x = 1\n"),
            ],
        );
        // The index timestamp has second precision: files written in the same second
        // as the indexing count as newer (a harmless extra re-index), so wait first.
        std::thread::sleep(Duration::from_millis(1100));
        let (graph, _) = index(&repo.0).unwrap();
        assert!(!is_stale(&repo.0, &graph));
        std::thread::sleep(Duration::from_millis(1100));
        fs::write(repo.0.join("gen/b.py"), "x = 2\n").unwrap();
        fs::write(repo.0.join("gen/new.py"), "y = 1\n").unwrap();
        assert!(!is_stale(&repo.0, &graph));
        fs::write(repo.0.join("a.py"), "def a(): return 1\n").unwrap();
        assert!(is_stale(&repo.0, &graph));
    }

    #[test]
    fn rust_use_expansion() {
        assert_eq!(
            expand_rust_use("crate::graph::{Graph, Node as N, sub::{A, B}}"),
            vec![
                "crate::graph::Graph",
                "crate::graph::Node",
                "crate::graph::sub::A",
                "crate::graph::sub::B"
            ]
        );
        assert_eq!(expand_rust_use("std::fs"), vec!["std::fs"]);
        assert_eq!(expand_rust_use("super::*"), vec!["super"]);
        assert_eq!(expand_rust_use("a::{self, b}"), vec!["a", "a::b"]);
    }

    #[test]
    fn paths() {
        assert_eq!(normalize("src/a/../b/./c"), "src/b/c");
        assert_eq!(crate_root("crates/x/src/a/b.rs"), "crates/x/src");
        assert_eq!(path_suffixes("a/b/c"), vec!["a/b/c", "b/c", "c"]);
    }

    #[test]
    fn a_file_read_in_its_mtime_tick_is_read_again() {
        let entry = |modified: SystemTime, read_at: SystemTime| CachedFile {
            modified,
            len: 10,
            read_at,
            facts: Err(String::new()),
        };
        let t = UNIX_EPOCH + std::time::Duration::from_millis(1_700_000_000_123);
        let ms = std::time::Duration::from_millis;
        assert!(entry(t, t + ms(1)).matches((t, 10)));
        assert!(!entry(t, t + ms(1)).matches((t, 11)));
        // Read in the same instant it was written: a same-size edit may follow unseen.
        assert!(!entry(t, t).matches((t, 10)));
        // Whole-second timestamps: the tick may be 2 s long.
        let coarse = UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        assert!(!entry(coarse, coarse + ms(1500)).matches((coarse, 10)));
        assert!(entry(coarse, coarse + ms(2500)).matches((coarse, 10)));
    }
}
