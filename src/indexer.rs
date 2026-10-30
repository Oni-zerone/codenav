//! Repository indexing: walks the tree, extracts facts from every supported file in
//! parallel, then resolves names across files into a `Graph`.
//!
//! Resolution is heuristic (no type checking): a reference is linked to definitions
//! with the same name, preferring the same file, then imported files, then the same
//! directory, then a small set of global candidates. Each edge records how sure the
//! match is (`Confidence`): exact, inferred by proximity, or ambiguous.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
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

pub fn discover(root: &Path) -> Vec<SourceFile> {
    let mut files: Vec<SourceFile> = walk_files(root)
        .filter(|e| e.metadata().is_ok_and(|m| m.len() <= MAX_FILE_BYTES))
        .filter_map(|e| {
            let lang = Lang::from_path(e.path())?;
            let rel = e.path().strip_prefix(root).ok()?;
            Some(SourceFile {
                rel: to_unix(rel),
                abs: e.path().to_path_buf(),
                lang,
            })
        })
        .collect();
    files.sort_by(|a, b| a.rel.cmp(&b.rel));
    files
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
    if graph.analysis != crate::graph::ANALYSIS_VERSION {
        return true;
    }
    let files = discover(root);
    if files.len() != graph.by_file.len() {
        return true;
    }
    // Milliseconds when known: whole seconds would flag files saved just before indexing.
    let since = if graph.indexed_at_ms > 0 {
        UNIX_EPOCH + Duration::from_millis(graph.indexed_at_ms)
    } else {
        UNIX_EPOCH + Duration::from_secs(graph.indexed_at)
    };
    files.iter().any(|f| {
        !graph.by_file.contains_key(&f.rel)
            || fs::metadata(&f.abs)
                .and_then(|m| m.modified())
                .is_ok_and(|t| t > since)
    })
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
pub fn load_or_index(
    root: &Path,
    force: bool,
    requested: Option<f64>,
) -> Result<(Graph, IndexOptions)> {
    let index_path = Graph::index_path(root);
    let opts = if force || !index_path.exists() {
        if !force {
            eprintln!("codenav: no index found, indexing {}…", root.display());
        }
        index_options(root, requested)
    } else {
        match Graph::load(root) {
            Ok(mut g) => {
                let opts = IndexOptions {
                    hub_percentile: requested.unwrap_or(g.hub_percentile),
                };
                if opts.hub_percentile != g.hub_percentile {
                    eprintln!(
                        "codenav: hub percentile changed ({} → {}), re-indexing…",
                        g.hub_percentile, opts.hub_percentile
                    );
                } else if is_stale(root, &g) {
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
                opts
            }
            Err(e) => {
                eprintln!("codenav: cannot use the saved index ({e:#}), re-indexing…");
                index_options(root, requested)
            }
        }
    };
    let (graph, stats) = index_with(root, &opts)?;
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

pub fn index_with(root: &Path, opts: &IndexOptions) -> Result<(Graph, IndexStats)> {
    let started = Instant::now();
    // Timestamp taken before reading, so edits made during indexing count as stale.
    let started_at = SystemTime::now().duration_since(UNIX_EPOCH)?;
    let files = discover(root);

    let results: Vec<Result<FileFacts, String>> = files
        .par_iter()
        .map(|f| {
            let src = fs::read(&f.abs).map_err(|e| format!("{}: {e}", f.rel))?;
            extract::extract(&f.rel, f.lang, &src).map_err(|e| format!("{}: {e:#}", f.rel))
        })
        .collect();

    let mut facts = Vec::new();
    let mut errors = Vec::new();
    for r in results {
        match r {
            Ok(f) => facts.push(f),
            Err(e) => errors.push(e),
        }
    }

    let mut graph = Builder::new(root, &facts).build();
    graph.indexed_at = started_at.as_secs();
    graph.indexed_at_ms = started_at.as_millis() as u64;
    graph.build_indexes();
    vcs::record_state(&mut graph, root);
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
    facts: &'a [FileFacts],
    graph: Graph,
    /// file index -> file node
    file_nodes: Vec<usize>,
    file_by_path: HashMap<String, usize>,
    /// file index -> def index -> node
    def_nodes: Vec<Vec<usize>>,
    /// file index -> impl index -> resolved owner type node and how sure that is
    impl_owner: Vec<Vec<Option<(usize, Confidence)>>>,
    /// member node -> confidence of its owner type, when the owner was not certain
    owner_confidence: HashMap<usize, Confidence>,
    /// exact name -> symbol nodes
    names: HashMap<String, Vec<usize>>,
    /// definition node -> its byte range in its file
    def_ranges: HashMap<usize, (usize, usize)>,
    /// node (definition or file) -> byte ranges of the definitions nested directly in it
    child_ranges: HashMap<usize, Vec<(usize, usize)>>,
    /// file index -> name or qualifier -> byte offsets of the refs using it
    mentions: Vec<HashMap<String, Vec<usize>>>,
    /// local npm packages: name -> (directory, entry file)
    npm_packages: HashMap<String, (String, Option<usize>)>,
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
    externals: HashMap<String, usize>,
    edge_index: HashMap<(usize, usize, EdgeKind), usize>,
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
    fn new(root: &Path, facts: &'a [FileFacts]) -> Self {
        Builder {
            facts,
            graph: Graph::new(root),
            file_nodes: Vec::new(),
            file_by_path: HashMap::new(),
            def_nodes: Vec::new(),
            impl_owner: Vec::new(),
            owner_confidence: HashMap::new(),
            names: HashMap::new(),
            stems: HashMap::new(),
            stem_suffixes: HashMap::new(),
            dirs: HashMap::new(),
            dir_suffixes: HashMap::new(),
            imported: HashMap::new(),
            externals: HashMap::new(),
            edge_index: HashMap::new(),
            lang_of_node: Vec::new(),
            hidden: HashSet::new(),
            def_ranges: HashMap::new(),
            child_ranges: HashMap::new(),
            mentions: Vec::new(),
            npm_packages: HashMap::new(),
            crates: HashMap::new(),
            packages: packages::discover(root),
            script_files: HashSet::new(),
            reexports: HashMap::new(),
            unresolved_imports: Vec::new(),
        }
    }

    fn build(mut self) -> Graph {
        self.add_files_and_defs();
        self.map_packages();
        // Owners and base types are looked up in imported files too.
        self.collect_imported_files();
        self.resolve_owners();
        self.assign_ids();
        self.add_contains_edges();
        self.resolve_imports();
        self.resolve_impls();
        self.resolve_refs();
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
            let mut stack: Vec<(usize, usize)> = Vec::new(); // (end_byte, node)
            for d in &f.defs {
                while stack.last().is_some_and(|&(end, _)| end <= d.start_byte) {
                    stack.pop();
                }
                let parent = stack.last().map(|&(_, n)| n).unwrap_or(file_node);
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
                stack.push((d.end_byte, node));
                nodes.push(node);
            }
            self.def_nodes.push(nodes);
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

    fn external(&mut self, name: &str) -> usize {
        if let Some(&n) = self.externals.get(name) {
            return n;
        }
        let n = self.graph.nodes.len();
        self.graph.nodes.push(Node {
            id: format!("ext:{name}"),
            kind: SymKind::External,
            name: name.to_string(),
            file: String::new(),
            lang: None,
            line: 0,
            end_line: 0,
            signature: String::new(),
            doc: None,
            parent: None,
            community: None,
        });
        self.lang_of_node.push(Lang::Rust); // unused for externals
        self.externals.insert(name.to_string(), n);
        n
    }

    /// Maps local package names to their indexed entry files.
    fn map_packages(&mut self) {
        let lookup = |p: &str| -> Option<usize> {
            let without_ext = match p.rsplit_once('.') {
                Some((s, ext)) if !s.is_empty() && !ext.contains('/') => s,
                _ => p,
            };
            self.stems.get(p).or(self.stems.get(without_ext)).copied()
        };
        let mut npm = HashMap::new();
        let mut crates = HashMap::new();
        for p in &self.packages {
            let entry = p.entries.iter().find_map(|e| lookup(e));
            match p.kind {
                PackageKind::Npm => {
                    npm.insert(p.name.clone(), (p.dir.clone(), entry));
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

    /// Import targets of file `fi` (repo files and external packages).
    fn import_targets(&self, fi: usize, imp: &extract::Import) -> Vec<Target> {
        let f = &self.facts[fi];
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
                // 2018 paths relative to the crate root (`use graph::Graph` in main.rs)
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

    fn resolve_js_import(&self, file: &str, spec: &str) -> Vec<Target> {
        let lookup = |p: &str| -> Option<usize> {
            let p = p.trim_end_matches('/');
            let without_ext = match p.rsplit_once('.') {
                Some((s, ext)) if !s.is_empty() && !ext.contains('/') => s,
                _ => p,
            };
            self.stems.get(p).or(self.stems.get(without_ext)).copied()
        };
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
        // A package of this repository (a workspace): its entry file, or a file under it.
        if let Some((dir, entry)) = self.npm_packages.get(&pkg) {
            let sub = spec[pkg.len()..].trim_start_matches('/');
            let found = if sub.is_empty() {
                *entry
            } else {
                lookup(&join(dir, sub)).or_else(|| lookup(&join(dir, &format!("src/{sub}"))))
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

    /// Where each name and qualifier is used, per file, for `scope_mentions`.
    fn index_mentions(&mut self) {
        self.mentions = self
            .facts
            .iter()
            .map(|f| {
                let mut m: HashMap<String, Vec<usize>> = HashMap::new();
                for r in &f.refs {
                    m.entry(r.name.clone()).or_default().push(r.byte);
                    if let Some(q) = &r.qualifier {
                        m.entry(q.clone()).or_default().push(r.byte);
                    }
                }
                for v in m.values_mut() {
                    v.sort_unstable();
                }
                m
            })
            .collect();
    }

    fn resolve_refs(&mut self) {
        self.index_mentions();
        for fi in 0..self.facts.len() {
            let f = &self.facts[fi];
            for r in &f.refs {
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
        }
    }

    /// The innermost definition containing `byte`, else the owner of the innermost impl
    /// block, else the file itself.
    fn scope_of(&self, fi: usize, byte: usize, kind: RefKind) -> Option<usize> {
        let f = &self.facts[fi];
        let def = f
            .defs
            .iter()
            .enumerate()
            .filter(|(_, d)| d.start_byte <= byte && byte < d.end_byte)
            .max_by_key(|(_, d)| d.start_byte)
            .map(|(i, d)| (self.def_nodes[fi][i], d.start_byte));
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
        r: &extract::Ref,
        accept: fn(SymKind) -> bool,
    ) -> (Vec<usize>, Confidence) {
        let none = (vec![], Confidence::Ambiguous);
        let family = self.facts[fi].lang.family();
        let Some(all) = self.names.get(&r.name) else {
            return none;
        };
        let mut cands: Vec<usize> = all
            .iter()
            .copied()
            .filter(|&n| accept(self.graph.nodes[n].kind))
            .filter(|&n| self.lang_of_node[n].family() == family)
            .collect();
        if family == Lang::JavaScript.family() {
            cands.retain(|&n| self.js_visible(fi, scope, n, r));
        }

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
                let matched: Vec<usize> = cands
                    .iter()
                    .copied()
                    .filter(|&n| self.qualifier_matches(fi, n, q, r))
                    .collect();
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
        let mut evidenced = false;
        if unknown_receiver && common {
            evidenced = true;
            cands.retain(|&n| {
                self.graph.nodes[n]
                    .parent
                    .filter(|&p| self.graph.nodes[p].kind.is_type())
                    .is_some_and(|p| self.scope_mentions(fi, scope, &self.graph.nodes[p].name))
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

        let file = &self.facts[fi].path;
        let file_node = self.file_nodes[fi];
        let dir = parent_dir(file);
        let imported = self.imported.get(&file_node);
        let is_rust = self.facts[fi].lang == Lang::Rust;
        let krate = crate_root(file);
        // (filter, a unique hit here is exact)
        type Tier<'t> = (Box<dyn Fn(usize) -> bool + 't>, bool);
        let tiers: [Tier; 4] = [
            (Box::new(|n| &self.graph.nodes[n].file == file), true),
            (
                Box::new(|n| {
                    imported.is_some_and(|set| {
                        self.file_by_path
                            .get(&self.graph.nodes[n].file)
                            .is_some_and(|f| set.contains(f))
                    })
                }),
                true,
            ),
            (
                Box::new(|n| parent_dir(&self.graph.nodes[n].file) == dir),
                false,
            ),
            (
                Box::new(|n| is_rust && crate_root(&self.graph.nodes[n].file) == krate),
                false,
            ),
        ];
        for (tier, sure) in &tiers {
            let hits: Vec<usize> = cands.iter().copied().filter(|&n| tier(n)).collect();
            match hits.len() {
                0 => continue,
                1 => return (hits, unique(*sure, risky)),
                k if k <= MAX_AMBIGUOUS => return (hits, Confidence::Ambiguous),
                _ => return none,
            }
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

    /// Whether `name` is used in file `fi` within `range`, outside the `excluded` ranges.
    fn mentioned_in(
        &self,
        fi: usize,
        name: &str,
        range: (usize, usize),
        excluded: &[(usize, usize)],
    ) -> bool {
        let Some(bytes) = self.mentions[fi].get(name) else {
            return false;
        };
        let from = bytes.partition_point(|&b| b < range.0);
        bytes[from..]
            .iter()
            .take_while(|&&b| b < range.1)
            .any(|&b| !excluded.iter().any(|&(s, e)| s <= b && b < e))
    }

    /// Code that belongs to `node` itself: its range without the definitions nested in it
    /// (the whole file without its functions, a type without its methods: its fields).
    fn own_code_mentions(&self, fi: usize, node: usize, name: &str) -> bool {
        let range = if node == self.file_nodes[fi] {
            (0, usize::MAX)
        } else if let Some(&r) = self.def_ranges.get(&node) {
            r
        } else {
            return false;
        };
        let excluded = self.child_ranges.get(&node).map_or(&[][..], Vec::as_slice);
        self.mentioned_in(fi, name, range, excluded)
    }

    /// Whether the code around a reference in `scope` names `name` (as a type, a value, a
    /// call or a qualifier like `Name::new()`): the scope's whole body when it is a
    /// definition of this file (else its file's top-level code), and the fields of the
    /// type it belongs to. Not the type's other methods.
    fn scope_mentions(&self, fi: usize, scope: usize, name: &str) -> bool {
        let in_scope = match self.def_ranges.get(&scope) {
            Some(&r) if self.graph.nodes[scope].file == self.facts[fi].path => {
                self.mentioned_in(fi, name, r, &[])
            }
            _ => self.own_code_mentions(fi, self.file_nodes[fi], name),
        };
        in_scope
            || self.graph.nodes[scope]
                .parent
                .filter(|&p| {
                    self.graph.nodes[p].kind.is_type()
                        && self.graph.nodes[p].file == self.facts[fi].path
                })
                .is_some_and(|t| self.own_code_mentions(fi, t, name))
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
        if nodes[n].file == self.facts[fi].path {
            return parent_kind == SymKind::File
                || parent_kind.is_type()
                || self.is_within(scope, parent);
        }
        let Some(&file) = self.file_by_path.get(&nodes[n].file) else {
            return false;
        };
        let script = self.script_files.contains(&file);
        match parent_kind {
            SymKind::File => {
                let imported = || {
                    self.imported
                        .get(&self.file_nodes[fi])
                        .is_some_and(|set| set.contains(&file))
                        || self.unresolved_imports[fi].contains(&r.name)
                };
                script || (!self.hidden.contains(&n) && (!bare || imported()))
            }
            // A member: its owners up to the file must be types or namespaces, not functions.
            k if k.is_type() || k == SymKind::Module => {
                let mut owner = Some(parent);
                while let Some(o) = owner.filter(|&o| o != file) {
                    let kind = nodes[o].kind;
                    if !(kind.is_type() || kind == SymKind::Module) {
                        return false;
                    }
                    owner = nodes[o].parent;
                }
                // A bare name never reaches into another file's class or namespace.
                !bare
            }
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
    /// names a value, whatever the file it happens to share its name with.
    fn qualifier_matches(&self, fi: usize, n: usize, q: &str, r: &extract::Ref) -> bool {
        let node = &self.graph.nodes[n];
        if node.parent.is_some_and(|p| self.graph.nodes[p].name == q) {
            return true;
        }
        let path = r.qualifier_path.as_deref().unwrap_or(q);
        let module_like = if self.lang_of_node[n] == Lang::Rust {
            // `crate::store::load()` is a path; `s.communities.len()` is a field.
            r.is_static
        } else {
            // `utils.f()`, or a dotted path the file imports (`pkg.utils.f()`).
            !path.contains(['.', ':', '(', '[', '-'])
                || self.facts[fi].imports.iter().any(|i| i.path == path)
        };
        if !module_like {
            return false;
        }
        let mut parts = node.file.rsplit('/');
        let file_stem = parts.next().and_then(|f| f.split('.').next());
        let dir_name = parts.next();
        file_stem == Some(q) || (self.facts_lang(n) == Some(Lang::Go) && dir_name == Some(q))
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
mod tests {
    use super::*;

    /// A throwaway repository with the given files, removed on drop.
    struct TempRepo(PathBuf);

    impl TempRepo {
        fn new(name: &str, files: &[(&str, &str)]) -> TempRepo {
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
}
