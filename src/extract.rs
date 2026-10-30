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
    };
    let mut export_names: std::collections::HashSet<String> = std::collections::HashSet::new();

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
        } else if let Some(n) = caps.get("export.name") {
            export_names.insert(text(*n));
        } else if let Some(node) = caps.get("valueref") {
            facts.refs.push(Ref {
                kind: RefKind::Value,
                name: text(*node),
                qualifier: None,
                is_static: false,
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
                byte: node.start_byte(),
                line: node.start_position().row as u32 + 1,
            });
        }
    }

    dedupe_defs(&mut facts.defs);
    for d in &mut facts.defs {
        d.exported |= export_names.contains(&d.name);
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
    let mut lines = Vec::new();
    let mut prev = node.prev_sibling();
    while let Some(p) = prev {
        let kind = p.kind();
        if kind.contains("comment") {
            if p.end_position().row + 1 < node.start_position().row && lines.is_empty() {
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
    (!doc.is_empty()).then(|| truncate(doc, 300))
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
