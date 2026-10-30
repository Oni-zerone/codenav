//! Tool output for LLMs: a compact YAML-like rendering of JSON results and a cut to an
//! approximate token budget, with the truncation notice at the top so a reader cannot
//! mistake a cut for an absence.

use serde_json::{Map, Value};

pub const CHARS_PER_TOKEN: usize = 3;
pub const DEFAULT_TOKEN_BUDGET: usize = 2000;
pub const MIN_TOKEN_BUDGET: usize = 100;
pub const MAX_TOKEN_BUDGET: usize = 100_000;
/// Room kept for the truncation notice.
const NOTICE_RESERVE: usize = 260;
/// Collections of scalars shorter than this are written on one line.
const INLINE_MAX: usize = 200;

/// Keeps whole blocks within the budget, cutting the first block that does not fit
/// line by line, and announces any cut at the top. `unit` names what a block is
/// ("nodes", "lines"); `hint` tells the reader how to narrow the request.
pub fn fit_blocks(
    header: &str,
    blocks: &[Vec<String>],
    token_budget: usize,
    unit: &str,
    hint: &str,
) -> String {
    // Node blocks are separated by a blank line, plain lines are not.
    let block_sep = if unit == "nodes" { "\n\n" } else { "\n" };
    let budget = token_budget
        .saturating_mul(CHARS_PER_TOKEN)
        .saturating_sub(NOTICE_RESERVE)
        .max(header.len());
    let block_len = |b: &[String]| b.iter().map(|l| l.len() + 1).sum::<usize>() + block_sep.len();
    let full = header.len() + blocks.iter().map(|b| block_len(b)).sum::<usize>();

    let mut body = String::new();
    let mut used = header.len() + block_sep.len();
    let mut whole = 0;
    let mut partial = false;
    for b in blocks {
        let len = block_len(b);
        let fits_whole = used + len <= budget;
        let mut lines = 0;
        for line in b {
            if !fits_whole && used + line.len() + block_sep.len() > budget {
                break;
            }
            if lines == 0 && !body.is_empty() {
                body.push_str(block_sep);
            } else if lines > 0 {
                body.push('\n');
            }
            body.push_str(line);
            used += line.len() + 1;
            lines += 1;
        }
        if fits_whole {
            whole += 1;
            continue;
        }
        // The first block that does not fit is cut line by line, so a single huge
        // one cannot blow the budget; nothing after it is shown.
        if lines > 0 {
            body.push_str(&format!("\n    … {} more lines cut", b.len() - lines));
            partial = true;
        }
        break;
    }

    let mut out = String::with_capacity(used + NOTICE_RESERVE);
    out.push_str(header);
    if whole < blocks.len() {
        let shown = if partial {
            format!("{whole} of {} {unit} in full, 1 partially", blocks.len())
        } else {
            format!("{whole} of {} {unit}", blocks.len())
        };
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&format!(
            "[!] TRUNCATED: showing {shown} (~{token_budget}-token budget). \
             token_budget={} would show all; or {hint}.",
            (full + NOTICE_RESERVE).div_ceil(CHARS_PER_TOKEN)
        ));
    }
    if !body.is_empty() {
        if !out.is_empty() {
            out.push_str(block_sep);
        }
        out.push_str(&body);
    }
    out
}

/// Renders a JSON result as compact YAML-like text cut to the budget.
pub fn fit_value(v: &Value, token_budget: usize, hint: &str) -> String {
    let blocks: Vec<Vec<String>> = to_lines(v).into_iter().map(|l| vec![l]).collect();
    fit_blocks("", &blocks, token_budget, "lines", hint)
}

/// YAML-like lines: no quotes or braces, short scalar collections on one line,
/// multi-line strings as indented blocks. Object field order is preserved.
pub fn to_lines(v: &Value) -> Vec<String> {
    let mut out = Vec::new();
    match v {
        Value::Object(m) => write_fields(m, 0, &mut out),
        Value::Array(a) => a.iter().for_each(|item| write_item(item, 0, &mut out)),
        scalar => push_scalar("", scalar, 0, &mut out),
    }
    out
}

fn pad(indent: usize) -> String {
    " ".repeat(indent)
}

fn scalar_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "null".to_string(),
        other => other.to_string(),
    }
}

fn is_scalar(v: &Value) -> bool {
    !matches!(v, Value::Array(_) | Value::Object(_))
}

fn is_multiline(v: &Value) -> bool {
    matches!(v, Value::String(s) if s.contains('\n'))
}

/// `prefix` is `"key: "` or `"- "`.
fn push_scalar(prefix: &str, v: &Value, indent: usize, out: &mut Vec<String>) {
    if is_multiline(v) {
        out.push(format!("{}{prefix}|", pad(indent)));
        for line in scalar_str(v).lines() {
            out.push(format!("{}{line}", pad(indent + 2)));
        }
    } else {
        out.push(format!("{}{prefix}{}", pad(indent), scalar_str(v)));
    }
}

/// `a: 1; b: x` for objects of short scalars (`;` because values such as signatures
/// contain commas), `a, b` for arrays of single-word scalars.
fn inline(v: &Value) -> Option<String> {
    let s = match v {
        Value::Object(m) if m.values().all(|x| is_scalar(x) && !is_multiline(x)) => m
            .iter()
            .map(|(k, x)| format!("{k}: {}", scalar_str(x)))
            .collect::<Vec<_>>()
            .join("; "),
        Value::Array(a)
            if a.iter().all(|x| {
                is_scalar(x) && !scalar_str(x).contains(|c: char| c == ',' || c.is_whitespace())
            }) =>
        {
            a.iter().map(scalar_str).collect::<Vec<_>>().join(", ")
        }
        _ => return None,
    };
    (s.len() <= INLINE_MAX).then_some(s)
}

fn write_fields(m: &Map<String, Value>, indent: usize, out: &mut Vec<String>) {
    for (k, v) in m {
        match v {
            Value::Array(a) if a.is_empty() => out.push(format!("{}{k}: []", pad(indent))),
            Value::Object(o) if o.is_empty() => out.push(format!("{}{k}: {{}}", pad(indent))),
            Value::Array(_) => match inline(v) {
                Some(s) => out.push(format!("{}{k}: [{s}]", pad(indent))),
                None => {
                    out.push(format!("{}{k}:", pad(indent)));
                    if let Value::Array(a) = v {
                        a.iter().for_each(|item| write_item(item, indent + 2, out));
                    }
                }
            },
            Value::Object(o) => match inline(v) {
                Some(s) => out.push(format!("{}{k}: {{{s}}}", pad(indent))),
                None => {
                    out.push(format!("{}{k}:", pad(indent)));
                    write_fields(o, indent + 2, out);
                }
            },
            scalar => push_scalar(&format!("{k}: "), scalar, indent, out),
        }
    }
}

fn write_item(item: &Value, indent: usize, out: &mut Vec<String>) {
    match item {
        Value::Object(m) => {
            if let Some(s) = inline(item) {
                out.push(format!("{}- {s}", pad(indent)));
                return;
            }
            // First field on the dash line, the others aligned under it.
            let start = out.len();
            write_fields(m, indent + 2, out);
            if let Some(first) = out.get_mut(start) {
                first.replace_range(indent..indent + 2, "- ");
            }
        }
        Value::Array(a) => match inline(item) {
            Some(s) => out.push(format!("{}- [{s}]", pad(indent))),
            None => {
                out.push(format!("{}-", pad(indent)));
                a.iter().for_each(|x| write_item(x, indent + 2, out));
            }
        },
        scalar => push_scalar("- ", scalar, indent, out),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn renders_compact_yaml_like_text() {
        let v = json!({
            "id": "a.rs::f",
            "lines": "1-3",
            "outgoing": { "calls": ["a.rs::g", { "id": "b.rs::h", "count": 2 }] },
            "empty": [],
            "code": "fn f() {\n    g();\n}",
        });
        assert_eq!(
            to_lines(&v),
            vec![
                "id: a.rs::f",
                "lines: 1-3",
                "outgoing:",
                "  calls:",
                "    - a.rs::g",
                "    - id: b.rs::h; count: 2",
                "empty: []",
                "code: |",
                "  fn f() {",
                "      g();",
                "  }",
            ]
        );
    }

    #[test]
    fn nested_list_items_keep_alignment() {
        let v = json!([{ "file": "a.rs", "deps": [{ "id": "x", "long": "y".repeat(200) }] }]);
        let lines = to_lines(&v);
        assert_eq!(lines[0], "- file: a.rs");
        assert_eq!(lines[1], "  deps:");
        assert!(lines[2].starts_with("    - id: x"), "{lines:?}");
        assert!(lines[3].starts_with("      long: yyy"), "{lines:?}");
    }

    #[test]
    fn value_cut_respects_budget_and_announces_it() {
        let v =
            json!({ "items": (0..500).map(|i| format!("item number {i}")).collect::<Vec<_>>() });
        let out = fit_value(&v, 100, "narrow it");
        assert!(out.len() <= 100 * CHARS_PER_TOKEN, "{}", out.len());
        assert!(out.starts_with("[!] TRUNCATED: showing"), "{out}");
        assert!(out.contains("narrow it"));
        assert!(!fit_value(&json!({ "a": 1 }), 100, "").contains("TRUNCATED"));
    }

    #[test]
    fn budget_holds_for_one_huge_block() {
        let mut block = vec!["hub".to_string()];
        block.extend((0..1500).map(|i| format!("    called_by ← caller_{i}")));
        let out = fit_blocks("H", &[block], 100, "nodes", "narrow");
        assert!(out.len() <= 100 * CHARS_PER_TOKEN, "{}", out.len());
        assert!(
            out.contains("1 partially") && out.contains("more lines cut"),
            "{out}"
        );
        let small = fit_blocks("H", &[vec!["a".into()]], usize::MAX, "nodes", "");
        assert!(!small.contains("TRUNCATED"));
    }
}
