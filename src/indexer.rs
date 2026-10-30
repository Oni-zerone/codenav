//! Repository indexing: walks the tree, extracts facts from every supported file in
//! parallel, then resolves names across files into a `Graph`.
//!
//! Resolution is heuristic (no type checking): a reference is linked to definitions
//! with the same name, preferring the same file, then imported files, then the same
//! directory, then a small set of global candidates (marked `ambiguous`).

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use ignore::WalkBuilder;
use rayon::prelude::*;

use crate::extract::{self, FileFacts, RefKind};
use crate::graph::{Edge, EdgeKind, Graph, INDEX_DIR, Node, SymKind};
use crate::lang::Lang;

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
    pub errors: Vec<String>,
    pub elapsed: Duration,
    pub by_lang: HashMap<Lang, usize>,
}

pub struct SourceFile {
    pub rel: String,
    pub abs: PathBuf,
    pub lang: Lang,
}

pub fn discover(root: &Path) -> Vec<SourceFile> {
    let mut files: Vec<SourceFile> = WalkBuilder::new(root)
        .require_git(false)
        .filter_entry(|e| {
            !(e.file_type().is_some_and(|t| t.is_dir())
                && e.file_name()
                    .to_str()
                    .is_some_and(|n| SKIP_DIRS.contains(&n)))
        })
        .build()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
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

/// True when some source file changed after the graph was built (or files were added/removed).
pub fn is_stale(root: &Path, graph: &Graph) -> bool {
    let files = discover(root);
    if files.len() != graph.by_file.len() {
        return true;
    }
    let since = UNIX_EPOCH + Duration::from_secs(graph.indexed_at);
    files.iter().any(|f| {
        !graph.by_file.contains_key(&f.rel)
            || fs::metadata(&f.abs)
                .and_then(|m| m.modified())
                .is_ok_and(|t| t > since)
    })
}

pub fn index(root: &Path) -> Result<(Graph, IndexStats)> {
    let started = Instant::now();
    // Timestamp taken before reading so edits made during indexing count as stale.
    let indexed_at = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
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
    graph.indexed_at = indexed_at;
    graph.build_indexes();

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
    /// file index -> impl index -> resolved owner type node
    impl_owner: Vec<Vec<Option<usize>>>,
    /// exact name -> symbol nodes
    names: HashMap<String, Vec<usize>>,
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
            names: HashMap::new(),
            stems: HashMap::new(),
            stem_suffixes: HashMap::new(),
            dirs: HashMap::new(),
            dir_suffixes: HashMap::new(),
            imported: HashMap::new(),
            externals: HashMap::new(),
            edge_index: HashMap::new(),
            lang_of_node: Vec::new(),
        }
    }

    fn build(mut self) -> Graph {
        self.add_files_and_defs();
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
                },
                f.lang,
            );
            self.file_nodes.push(file_node);
            self.file_by_path.insert(f.path.clone(), file_node);
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
                    },
                    f.lang,
                );
                self.names.entry(d.name.clone()).or_default().push(node);
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
                    Some(owner) if owner != node => self.graph.nodes[node].parent = Some(owner),
                    // Type not in the repo: keep the owner in the qualified name (see `qualified`).
                    _ => self.graph.nodes[node].id = format!("{owner_name}::{}", d.name),
                }
            }
        }
    }

    fn find_type(&self, name: &str, fi: usize) -> Option<usize> {
        let cands: Vec<usize> = self
            .names
            .get(name)?
            .iter()
            .copied()
            .filter(|&n| self.graph.nodes[n].kind.is_type())
            .filter(|&n| self.lang_of_node[n].family() == self.facts[fi].lang.family())
            .collect();
        let file = &self.facts[fi].path;
        let dir = parent_dir(file);
        cands
            .iter()
            .find(|&&n| &self.graph.nodes[n].file == file)
            .or_else(|| {
                cands
                    .iter()
                    .find(|&&n| parent_dir(&self.graph.nodes[n].file) == dir)
            })
            .copied()
            .or_else(|| (cands.len() == 1).then(|| cands[0]))
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
                self.add_edge(p, i, EdgeKind::Contains, line, false);
            }
        }
    }

    fn add_edge(&mut self, from: usize, to: usize, kind: EdgeKind, line: u32, ambiguous: bool) {
        if let Some(&e) = self.edge_index.get(&(from, to, kind)) {
            let edge = &mut self.graph.edges[e];
            edge.count += 1;
            edge.line = edge.line.min(line);
            edge.ambiguous &= ambiguous;
            return;
        }
        self.edge_index
            .insert((from, to, kind), self.graph.edges.len());
        self.graph.edges.push(Edge {
            from,
            to,
            kind,
            count: 1,
            ambiguous,
            line,
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
        });
        self.lang_of_node.push(Lang::Rust); // unused for externals
        self.externals.insert(name.to_string(), n);
        n
    }

    fn resolve_imports(&mut self) {
        for (fi, f) in self.facts.iter().enumerate() {
            let from = self.file_nodes[fi];
            for imp in &f.imports {
                let targets = match f.lang {
                    Lang::Rust => self.resolve_rust_import(&f.path, imp),
                    Lang::Python => self.resolve_python_import(&f.path, imp),
                    Lang::JavaScript | Lang::TypeScript | Lang::Tsx => {
                        self.resolve_js_import(&f.path, &imp.path)
                    }
                    Lang::Go => self.resolve_go_import(&imp.path),
                    Lang::Swift => vec![Target::External(imp.path.clone())],
                };
                for t in targets {
                    let to = match t {
                        Target::File(n) if n == from => continue,
                        Target::File(n) => {
                            self.imported.entry(from).or_default().insert(n);
                            n
                        }
                        Target::External(name) => self.external(&name),
                    };
                    self.add_edge(from, to, EdgeKind::Imports, imp.line, false);
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
                let (Some(owner), Some(tr)) = (self.impl_owner[fi][ii], imp.trait_name.as_ref())
                else {
                    continue;
                };
                if let Some(t) = self.find_type(tr, fi) {
                    self.add_edge(owner, t, EdgeKind::Inherits, imp.line, false);
                }
            }
        }
    }

    fn resolve_refs(&mut self) {
        for fi in 0..self.facts.len() {
            let f = &self.facts[fi];
            for r in &f.refs {
                let Some(scope) = self.scope_of(fi, r.byte, r.kind) else {
                    continue;
                };
                let (kind, accept): (EdgeKind, fn(SymKind) -> bool) = match r.kind {
                    RefKind::Call => (EdgeKind::Calls, SymKind::is_callable),
                    RefKind::Base => (EdgeKind::Inherits, SymKind::is_type),
                    RefKind::TypeRef => (EdgeKind::References, SymKind::is_type),
                };
                let (targets, ambiguous) = self.pick_targets(fi, scope, r, accept);
                for t in targets {
                    if t == scope && kind != EdgeKind::Calls {
                        continue;
                    }
                    // A type referencing one of its own members is just containment.
                    if kind == EdgeKind::References && self.graph.nodes[scope].parent == Some(t) {
                        continue;
                    }
                    self.add_edge(scope, t, kind, r.line, ambiguous);
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
                Some(self.impl_owner[fi][ii].unwrap_or(self.file_nodes[fi]))
            }
            (None, None) => Some(self.file_nodes[fi]),
        }
    }

    fn pick_targets(
        &self,
        fi: usize,
        scope: usize,
        r: &extract::Ref,
        accept: fn(SymKind) -> bool,
    ) -> (Vec<usize>, bool) {
        let family = self.facts[fi].lang.family();
        let Some(all) = self.names.get(&r.name) else {
            return (vec![], false);
        };
        let mut cands: Vec<usize> = all
            .iter()
            .copied()
            .filter(|&n| accept(self.graph.nodes[n].kind))
            .filter(|&n| self.lang_of_node[n].family() == family)
            .collect();

        // Receiver/path qualifiers narrow the candidates down; `risky` marks method
        // calls on values whose type we cannot know.
        let mut risky = false;
        if let Some(q) = r.qualifier.as_deref() {
            if matches!(q, "self" | "this" | "Self") {
                let owner = self.enclosing_type(scope);
                let own: Vec<usize> = cands
                    .iter()
                    .copied()
                    .filter(|&n| owner.is_some() && self.graph.nodes[n].parent == owner)
                    .collect();
                if !own.is_empty() {
                    return (own, false);
                }
                // Inherited member: any method with that name.
                cands.retain(|&n| self.graph.nodes[n].kind == SymKind::Method);
                risky = true;
            } else {
                let matched: Vec<usize> = cands
                    .iter()
                    .copied()
                    .filter(|&n| self.qualifier_matches(n, q))
                    .collect();
                if !matched.is_empty() {
                    cands = matched;
                } else if r.is_static || q.starts_with(char::is_uppercase) {
                    // `Vec::new()`, `Math.max()`: a type/module outside the repo.
                    return (vec![], false);
                } else {
                    // `value.method()` can only target methods.
                    cands.retain(|&n| self.graph.nodes[n].kind == SymKind::Method);
                    risky = true;
                }
            }
        }
        if cands.is_empty() {
            return (vec![], false);
        }

        let file = &self.facts[fi].path;
        let file_node = self.file_nodes[fi];
        let dir = parent_dir(file);
        let imported = self.imported.get(&file_node);
        let is_rust = self.facts[fi].lang == Lang::Rust;
        let krate = crate_root(file);
        let tiers: [Box<dyn Fn(usize) -> bool>; 4] = [
            Box::new(|n| &self.graph.nodes[n].file == file),
            Box::new(|n| {
                imported.is_some_and(|set| {
                    self.file_by_path
                        .get(&self.graph.nodes[n].file)
                        .is_some_and(|f| set.contains(f))
                })
            }),
            Box::new(|n| parent_dir(&self.graph.nodes[n].file) == dir),
            Box::new(|n| is_rust && crate_root(&self.graph.nodes[n].file) == krate),
        ];
        for tier in &tiers {
            let hits: Vec<usize> = cands.iter().copied().filter(|&n| tier(n)).collect();
            match hits.len() {
                0 => continue,
                1 => return (hits, risky && COMMON_METHODS.contains(&r.name.as_str())),
                k if k <= MAX_AMBIGUOUS => return (hits, true),
                _ => return (vec![], false),
            }
        }

        // Global fallback. Rust crates only see each other through imports, which we
        // resolve to externals, so a match in another crate is only a guess.
        if is_rust {
            risky = true;
        }
        if risky && (COMMON_METHODS.contains(&r.name.as_str()) || cands.len() > 1) {
            return (vec![], false);
        }
        match cands.len() {
            1 => (cands, risky),
            k if k <= MAX_AMBIGUOUS => (cands, true),
            _ => (vec![], false),
        }
    }

    /// `q.name()` / `q::name()` may target a member of type `q`, a function in module
    /// (file) `q`, or in Go package (directory) `q`.
    fn qualifier_matches(&self, n: usize, q: &str) -> bool {
        let node = &self.graph.nodes[n];
        if node.parent.is_some_and(|p| self.graph.nodes[p].name == q) {
            return true;
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
