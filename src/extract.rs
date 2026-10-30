//! Per-file extraction: parses a source file with tree-sitter and turns query matches
//! into raw facts (definitions, impl blocks, references, imports). No cross-file
//! resolution happens here.

use std::collections::HashMap;
use std::sync::OnceLock;

use anyhow::{Context, Result, anyhow};
use tree_sitter::{Node, Parser, Query, QueryCursor, StreamingIterator};

use crate::graph::SymKind;
use crate::lang::Lang;

#[derive(Debug, Clone)]
pub struct Def {
    pub kind: SymKind,
    pub name: String,
    pub start_byte: usize,
    pub end_byte: usize,
    pub start_line: u32,
    pub end_line: u32,
    pub signature: String,
    pub doc: Option<String>,
    /// Explicit owner type (Go receivers).
    pub owner: Option<String>,
    /// JS/TS: declared with `export`, or named in an `export { … }` / `export default`.
    pub exported: bool,
}

#[derive(Debug, Clone)]
pub struct ImplBlock {
    pub type_name: String,
    pub trait_name: Option<String>,
    pub start_byte: usize,
    pub end_byte: usize,
    pub line: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RefKind {
    Call,
    Base,
    TypeRef,
    /// JS/TS identifier used as a value (may name a top-level constant).
    Value,
}

#[derive(Debug, Clone)]
pub struct Ref {
    pub kind: RefKind,
    pub name: String,
    pub qualifier: Option<String>,
    /// The qualifier is a static path (type or module), not a value.
    pub is_static: bool,
    /// The whole qualifier as written (`self.graph`, `pkg.utils`, `crate::store`).
    pub qualifier_path: Option<String>,
    pub byte: usize,
    pub line: u32,
}

#[derive(Debug, Clone)]
pub struct Import {
    pub path: String,
    pub names: Vec<String>,
    /// Rust `mod foo;`
    pub is_mod: bool,
    /// JS/TS `export … from '…'`: the file passes the target's exports on.
    pub reexport: bool,
    pub line: u32,
}

#[derive(Debug, Clone)]
pub struct FileFacts {
    pub path: String,
    pub lang: Lang,
    pub lines: u32,
    pub defs: Vec<Def>,
    pub impls: Vec<ImplBlock>,
    pub refs: Vec<Ref>,
    pub imports: Vec<Import>,
    /// JS/TS: the file has an `export` statement (an ES module).
    pub has_exports: bool,
    /// JS/TS: the file has an `import` statement or CommonJS exports: a module, not a
    /// plain script (whose top-level definitions are globals).
    pub is_module: bool,
    /// JS/TS: names bound by `import { a } from 'p'` / `import a from 'p'`, with `p`.
    pub import_bindings: Vec<(String, String)>,
}

fn query_for(lang: Lang) -> Result<&'static Query> {
    static QUERIES: OnceLock<HashMap<Lang, Result<Query, String>>> = OnceLock::new();
    let map = QUERIES.get_or_init(|| {
        Lang::ALL
            .iter()
            .map(|&l| {
                let q = Query::new(&l.grammar(), &l.query_source()).map_err(|e| format!("{e:?}"));
                (l, q)
            })
            .collect()
    });
    match map.get(&lang) {
        Some(Ok(q)) => Ok(q),
        Some(Err(e)) => Err(anyhow!("invalid {lang:?} query: {e}")),
        None => Err(anyhow!("no query for {lang:?}")),
    }
}

/// Validates all built-in queries.
#[cfg(test)]
pub fn check_queries() -> Vec<(Lang, Result<()>)> {
    Lang::ALL
        .iter()
        .map(|&l| (l, query_for(l).map(|_| ())))
        .collect()
}

pub fn extract(path: &str, lang: Lang, source: &[u8]) -> Result<FileFacts> {
    let mut parser = Parser::new();
    parser.set_language(&lang.grammar())?;
    let tree = parser
        .parse(source, None)
        .with_context(|| format!("failed to parse {path}"))?;
    let query = query_for(lang)?;
    let names = query.capture_names();

    let mut facts = FileFacts {
        path: path.to_string(),
        lang,
        lines: source.iter().filter(|&&b| b == b'\n').count() as u32 + 1,
        defs: Vec::new(),
        impls: Vec::new(),
        refs: Vec::new(),
        imports: Vec::new(),
        has_exports: false,
        is_module: false,
        import_bindings: Vec::new(),
    };
    let mut export_names: std::collections::HashSet<String> = std::collections::HashSet::new();
    // name -> byte ranges of the functions where a local binding shadows it
    let mut locals: HashMap<String, Vec<(usize, usize)>> = HashMap::new();
    let mut value_refs: Vec<Ref> = Vec::new();

    let text = |n: Node| n.utf8_text(source).unwrap_or("").to_string();

    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(query, tree.root_node(), source);
    while let Some(m) = matches.next() {
        let mut caps: HashMap<&str, Node> = HashMap::new();
        let mut import_names = Vec::new();
        for c in m.captures() {
            let cname = names[c.index as usize];
            if cname == "import.name" {
                import_names.push(text(c.node));
            } else {
                caps.insert(cname, c.node);
            }
        }

        if let Some((kind, node)) = caps
            .iter()
            .find_map(|(k, n)| k.strip_prefix("def.").map(|k| (k, *n)))
        {
            let Some(kind) = SymKind::parse(kind) else {
                continue;
            };
            let Some(name_node) = caps.get("name") else {
                continue;
            };
            facts.defs.push(Def {
                kind,
                name: text(*name_node),
                start_byte: node.start_byte(),
                end_byte: node.end_byte(),
                start_line: node.start_position().row as u32 + 1,
                end_line: node.end_position().row as u32 + 1,
                signature: signature_of(node, source),
                doc: doc_of(node, source),
                owner: caps.get("owner").map(|n| simple_type_name(&text(*n))),
                exported: lang.family() == Lang::JavaScript.family() && has_export_ancestor(node),
            });
        } else if let Some(node) = caps.get("impl") {
            let Some(ty) = caps.get("impl.type") else {
                continue;
            };
            facts.impls.push(ImplBlock {
                type_name: simple_type_name(&text(*ty)),
                trait_name: caps.get("impl.trait").map(|n| simple_type_name(&text(*n))),
                start_byte: node.start_byte(),
                end_byte: node.end_byte(),
                line: node.start_position().row as u32 + 1,
            });
        } else if let Some(node) = caps
            .get("import")
            .or(caps.get("import.mod"))
            .or(caps.get("import.reexport"))
        {
            let Some(path_node) = caps.get("path") else {
                continue;
            };
            facts.imports.push(Import {
                path: text(*path_node)
                    .trim_matches(|c| c == '"' || c == '\'')
                    .to_string(),
                names: import_names,
                is_mod: caps.contains_key("import.mod"),
                reexport: caps.contains_key("import.reexport"),
                line: node.start_position().row as u32 + 1,
            });
        } else if caps.contains_key("export") {
            facts.has_exports = true;
        } else if caps.contains_key("esm") || caps.contains_key("commonjs") {
            facts.is_module = true;
        } else if let Some(n) = caps.get("export.name") {
            export_names.insert(text(*n));
        } else if let (Some(n), Some(from)) = (caps.get("import.binding"), caps.get("import.from"))
        {
            facts.import_bindings.push((text(*n), text(*from)));
        } else if let Some(node) = caps.get("binding.pattern") {
            if let Some(scope) = enclosing_function(*node) {
                let mut names = Vec::new();
                pattern_names(*node, source, &mut names);
                for name in names {
                    locals
                        .entry(name)
                        .or_default()
                        .push((scope.start_byte(), scope.end_byte()));
                }
            }
        } else if let Some(node) = caps.get("valueref") {
            if in_import_or_export(*node) {
                continue;
            }
            value_refs.push(Ref {
                kind: RefKind::Value,
                name: text(*node),
                qualifier: None,
                is_static: false,
                qualifier_path: None,
                byte: node.start_byte(),
                line: node.start_position().row as u32 + 1,
            });
        } else if let Some(node) = caps.get("call") {
            let Some(name_node) = caps.get("name") else {
                continue;
            };
            facts.refs.push(Ref {
                kind: RefKind::Call,
                name: text(*name_node),
                qualifier: caps
                    .get("qualifier")
                    .or(caps.get("scope"))
                    .map(|n| last_segment(&text(*n))),
                is_static: caps.contains_key("scope"),
                qualifier_path: caps
                    .get("qualifier")
                    .or(caps.get("scope"))
                    .map(|n| text(*n)),
                byte: node.start_byte(),
                line: node.start_position().row as u32 + 1,
            });
        } else if let Some((kind, node)) = caps
            .get("base")
            .map(|n| (RefKind::Base, *n))
            .or(caps.get("typeref").map(|n| (RefKind::TypeRef, *n)))
        {
            facts.refs.push(Ref {
                kind,
                name: simple_type_name(&text(node)),
                qualifier: None,
                is_static: false,
                qualifier_path: None,
                byte: node.start_byte(),
                line: node.start_position().row as u32 + 1,
            });
        }
    }

    // A value ref shadowed by a local binding of an enclosing function is not the constant.
    facts.refs.extend(value_refs.into_iter().filter(|r| {
        !locals
            .get(&r.name)
            .is_some_and(|scopes| scopes.iter().any(|&(s, e)| s <= r.byte && r.byte < e))
    }));

    dedupe_defs(&mut facts.defs);
    // `export { a }` names a top-level definition (defs are sorted by start, -end).
    let mut end_of_open = 0;
    for d in &mut facts.defs {
        if d.start_byte >= end_of_open {
            d.exported |= export_names.contains(&d.name);
            end_of_open = d.end_byte;
        }
    }
    // A supertype is also matched as a plain type reference: keep only the inheritance.
    let bases: std::collections::HashSet<usize> = facts
        .refs
        .iter()
        .filter(|r| r.kind == RefKind::Base)
        .map(|r| r.byte)
        .collect();
    facts
        .refs
        .retain(|r| r.kind != RefKind::TypeRef || !bases.contains(&r.byte));
    Ok(facts)
}

const FUNCTION_KINDS: &[&str] = &[
    "function_declaration",
    "function_expression",
    "function",
    "arrow_function",
    "method_definition",
    "generator_function_declaration",
    "generator_function",
];

/// The innermost function (of any form) containing `node`; `None` at module level.
fn enclosing_function(node: Node) -> Option<Node> {
    let mut cur = node.parent();
    while let Some(n) = cur {
        if FUNCTION_KINDS.contains(&n.kind()) {
            return Some(n);
        }
        cur = n.parent();
    }
    None
}

/// Names bound by a parameter list or destructuring pattern (not default values or types).
fn pattern_names(node: Node, source: &[u8], out: &mut Vec<String>) {
    let text = |n: Node| n.utf8_text(source).unwrap_or("").to_string();
    match node.kind() {
        "identifier" | "shorthand_property_identifier_pattern" => out.push(text(node)),
        "pair_pattern" => {
            if let Some(v) = node.child_by_field_name("value") {
                pattern_names(v, source, out);
            }
        }
        "assignment_pattern" | "object_assignment_pattern" => {
            if let Some(l) = node.child_by_field_name("left") {
                pattern_names(l, source, out);
            }
        }
        "required_parameter" | "optional_parameter" => {
            if let Some(p) = node.child_by_field_name("pattern") {
                pattern_names(p, source, out);
            }
        }
        "formal_parameters" | "object_pattern" | "array_pattern" | "rest_pattern" => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                pattern_names(child, source, out);
            }
        }
        _ => {}
    }
}

/// Identifiers in import/export clauses name bindings, not uses of a value.
fn in_import_or_export(node: Node) -> bool {
    // `export default name`
    if node
        .parent()
        .is_some_and(|p| p.kind() == "export_statement")
    {
        return true;
    }
    let mut cur = node.parent();
    for _ in 0..3 {
        match cur {
            Some(p)
                if matches!(
                    p.kind(),
                    "import_specifier"
                        | "import_clause"
                        | "namespace_import"
                        | "export_specifier"
                        | "import_statement"
                ) =>
            {
                return true;
            }
            Some(p) => cur = p.parent(),
            None => return false,
        }
    }
    false
}

/// `export function f`, `export class C`, `export const x = …`, `export default …`.
fn has_export_ancestor(node: Node) -> bool {
    let mut cur = node.parent();
    // A declarator sits in a declaration statement, which sits in the export.
    for _ in 0..2 {
        match cur {
            Some(p) if p.kind() == "export_statement" => return true,
            Some(p) if p.kind().ends_with("declaration") => cur = p.parent(),
            _ => return false,
        }
    }
    false
}

/// The same node can match several definition patterns (e.g. a Go `type_spec`
/// matches both the struct and the generic type pattern): keep the most specific.
fn dedupe_defs(defs: &mut Vec<Def>) {
    let mut best: HashMap<(usize, usize), usize> = HashMap::new();
    let mut keep = vec![true; defs.len()];
    for (i, d) in defs.iter().enumerate() {
        match best.get(&(d.start_byte, d.end_byte)) {
            Some(&j) if defs[j].kind.specificity() >= d.kind.specificity() => keep[i] = false,
            Some(&j) => {
                keep[j] = false;
                best.insert((d.start_byte, d.end_byte), i);
            }
            None => {
                best.insert((d.start_byte, d.end_byte), i);
            }
        }
    }
    let mut i = 0;
    defs.retain(|_| {
        i += 1;
        keep[i - 1]
    });
    defs.sort_by_key(|d| (d.start_byte, std::cmp::Reverse(d.end_byte)));
}

fn signature_of(node: Node, source: &[u8]) -> String {
    let body = &source[node.start_byte()..node.end_byte()];
    let first = String::from_utf8_lossy(body.split(|&b| b == b'\n').next().unwrap_or(b""))
        .trim()
        .trim_end_matches('{')
        .trim()
        .to_string();
    truncate(first, 200)
}

/// Collects comments directly above a definition (skipping attributes/decorators),
/// or the docstring for Python definitions.
fn doc_of(node: Node, source: &[u8]) -> Option<String> {
    // Comments sit before the whole statement: `/** doc */ export const f = () => {}`.
    // A declaration with several declarators keeps its comment to itself.
    let mut anchor = node;
    while let Some(p) = anchor.parent().filter(|p| match p.kind() {
        "export_statement" => true,
        "lexical_declaration" | "variable_declaration" => p.named_child_count() == 1,
        _ => false,
    }) {
        anchor = p;
    }
    let mut lines = Vec::new();
    let mut prev = anchor.prev_sibling();
    while let Some(p) = prev {
        let kind = p.kind();
        if kind.contains("comment") {
            if p.end_position().row + 1 < anchor.start_position().row && lines.is_empty() {
                break;
            }
            let t = p.utf8_text(source).unwrap_or("");
            lines.push(clean_comment(t));
        } else if !(kind.contains("attribute") || kind == "decorator") {
            break;
        }
        prev = p.prev_sibling();
        if lines.len() >= 6 {
            break;
        }
    }
    lines.reverse();
    let mut doc = lines.join(" ").trim().to_string();

    if doc.is_empty() {
        // Python docstring: first statement of the body is a string.
        if let Some(body) = node.child_by_field_name("body")
            && let Some(first) = body.named_child(0)
            && first.kind() == "expression_statement"
            && let Some(s) = first.named_child(0).filter(|s| s.kind() == "string")
        {
            doc = s
                .utf8_text(source)
                .unwrap_or("")
                .trim_matches(|c| c == '"' || c == '\'')
                .trim()
                .to_string();
        }
    }
    // Comments opening a function's body (`fn f() { // does X`), for code documented
    // inside; for a class or trait they would be its first member's doc.
    let function_like = {
        let k = node.kind();
        k.contains("function") || k.contains("method") || k == "variable_declarator"
    };
    if doc.is_empty() && function_like {
        let body = node.child_by_field_name("body").or_else(|| {
            node.child_by_field_name("value")
                .and_then(|v| v.child_by_field_name("body"))
        });
        if let Some(body) = body {
            let mut cursor = body.walk();
            let leading: Vec<String> = body
                .named_children(&mut cursor)
                .take_while(|c| c.kind().contains("comment"))
                .take(6)
                .map(|c| clean_comment(c.utf8_text(source).unwrap_or("")))
                .filter(|c| !is_directive(c))
                .collect();
            doc = leading.join(" ").trim().to_string();
        }
    }
    (!doc.is_empty()).then(|| truncate(doc, 300))
}

/// Tool directives and notes (`TODO`, `eslint-disable`, `noqa`…), not descriptions.
fn is_directive(comment: &str) -> bool {
    let c = comment.trim_start().to_lowercase();
    [
        "todo", "fixme", "hack", "xxx", "eslint", "prettier", "@ts-", "noqa", "pylint", "nolint",
        "type:", "clippy", "istanbul", "c8 ", "biome-",
    ]
    .iter()
    .any(|p| c.starts_with(p))
}

fn clean_comment(t: &str) -> String {
    t.lines()
        .map(|l| {
            l.trim()
                .trim_start_matches("///")
                .trim_start_matches("//!")
                .trim_start_matches("//")
                .trim_start_matches("/**")
                .trim_start_matches("/*")
                .trim_end_matches("*/")
                .trim_start_matches('*')
                .trim_start_matches('#')
                .trim()
        })
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn truncate(mut s: String, max: usize) -> String {
    if s.len() > max {
        let mut cut = max;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
        s.push('…');
    }
    s
}

/// `&mut foo::Bar<T>` -> `Bar`, `*Server` -> `Server`.
pub fn simple_type_name(t: &str) -> String {
    let t = t
        .split('<')
        .next()
        .unwrap_or(t)
        .split('[')
        .next()
        .unwrap_or(t);
    let t = t
        .trim()
        .trim_start_matches('&')
        .trim_start_matches('*')
        .trim_start_matches("mut ")
        .trim_start_matches("dyn ")
        .trim();
    last_segment(t)
}

/// `a::b::C` / `a.b.C` -> `C`
pub fn last_segment(t: &str) -> String {
    t.rsplit(['.', ':']).next().unwrap_or(t).trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_queries_compile() {
        for (lang, res) in check_queries() {
            assert!(res.is_ok(), "{lang:?}: {:?}", res.err());
        }
    }

    #[test]
    fn simple_names() {
        assert_eq!(simple_type_name("&mut foo::Bar<T>"), "Bar");
        assert_eq!(simple_type_name("*Server"), "Server");
        assert_eq!(simple_type_name("models.User"), "User");
    }
}
