//! Tool output for LLMs: a compact YAML-like rendering of JSON results and a cut to an
//! approximate token budget, with the truncation notice at the top so a reader cannot
//! mistake a cut for an absence.

use serde_json::{Map, Value};

pub const CHARS_PER_TOKEN: usize = 3;
pub const DEFAULT_TOKEN_BUDGET: usize = 2000;
pub const MIN_TOKEN_BUDGET: usize = 200;
pub const MAX_TOKEN_BUDGET: usize = 100_000;
/// Room kept for the truncation notice when the output has to be cut.
const NOTICE_RESERVE: usize = 300;
/// Room kept for the `… N more lines cut` marker of a partially shown block.
const MARKER_RESERVE: usize = 40;
/// Headers longer than this are clipped, so they cannot eat the budget.
const HEADER_MAX: usize = 400;
/// Collections of scalars shorter than this are written on one line.
const INLINE_MAX: usize = 200;

/// Keeps whole blocks within the budget, cutting the first block that does not fit
/// line by line, and announces any cut at the top. `unit` names what a block is
/// ("nodes", "lines"); `hint` tells the reader how to narrow the request.
///
/// Lengths are measured on the exact text written, so the suggested budget in the
/// notice is always enough to show everything.
pub fn fit_blocks(
    header: &str,
    blocks: &[Vec<String>],
    token_budget: usize,
    unit: &str,
    hint: &str,
) -> String {
    // Node blocks are separated by a blank line, plain lines are not.
    let sep = if unit == "nodes" { "\n\n" } else { "\n" };
    let limit = token_budget.saturating_mul(CHARS_PER_TOKEN);
    let header = clip(header, HEADER_MAX);
    let rendered: Vec<String> = blocks.iter().map(|b| b.join("\n")).collect();
    let full = join(&header, None, sep, &rendered).len();
    if full <= limit {
        return join(&header, None, sep, &rendered);
    }

    // Cut: header + body must leave room for the notice.
    let budget = limit.saturating_sub(NOTICE_RESERVE);
    let prefix = if header.is_empty() {
        0
    } else {
        header.len() + sep.len()
    };
    let mut body = String::new();
    let mut whole = 0;
    let mut partial = false;
    for (b, text) in blocks.iter().zip(&rendered) {
        let gap = if body.is_empty() { 0 } else { sep.len() };
        if prefix + body.len() + gap + text.len() <= budget {
            body.push_str(&sep[..gap]);
            body.push_str(text);
            whole += 1;
            continue;
        }
        // The first block that does not fit is cut line by line, so a single huge
        // one cannot blow the budget; nothing after it is shown.
        let mut lines = 0;
        for line in b {
            let cost = if lines == 0 { gap } else { 1 } + line.len();
            if prefix + body.len() + cost + MARKER_RESERVE > budget {
                break;
            }
            body.push_str(if lines == 0 { &sep[..gap] } else { "\n" });
            body.push_str(line);
            lines += 1;
        }
        if lines > 0 {
            body.push_str(&format!("\n    … {} more lines cut", b.len() - lines));
            partial = true;
        }
        break;
    }

    let shown = if partial {
        format!("{whole} of {} {unit} in full, 1 partially", blocks.len())
    } else {
        format!("{whole} of {} {unit}", blocks.len())
    };
    let needed = full.div_ceil(CHARS_PER_TOKEN);
    let advice = if needed <= MAX_TOKEN_BUDGET {
        format!("token_budget={needed} would show all; or {hint}")
    } else {
        format!("the full output exceeds the max token_budget={MAX_TOKEN_BUDGET}; {hint}")
    };
    let notice =
        format!("[!] TRUNCATED: showing {shown} (~{token_budget}-token budget). {advice}.");
    join(&header, Some(&notice), sep, &[body])
}

/// `header`, optional notice and blocks, skipping empty parts.
fn join(header: &str, notice: Option<&str>, sep: &str, blocks: &[String]) -> String {
    let mut out = String::from(header);
    if let Some(n) = notice {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(n);
    }
    for b in blocks.iter().filter(|b| !b.is_empty()) {
        if !out.is_empty() {
            out.push_str(sep);
        }
        out.push_str(b);
    }
    out
}

/// Cuts `s` to at most `max` bytes on a char boundary, marking the cut.
pub fn clip(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max.saturating_sub('…'.len_utf8());
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// Renders a JSON result as compact YAML-like text cut to the budget.
/// Whether `fit_value` shows `v` whole within `token_budget`.
pub fn fits(v: &Value, token_budget: usize) -> bool {
    let lines = to_lines(v);
    let len = lines.iter().map(String::len).sum::<usize>() + lines.len().saturating_sub(1);
    len <= token_budget.saturating_mul(CHARS_PER_TOKEN)
}

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
        Value::String(s) if s.is_empty() => "\"\"".to_string(),
        Value::String(s) => s.clone(),
        Value::Null => "null".to_string(),
        other => other.to_string(),
    }
}

fn is_scalar(v: &Value) -> bool {
    !matches!(v, Value::Array(_) | Value::Object(_))
}

fn is_multiline(v: &Value) -> bool {
    matches!(v, Value::String(s) if s.contains(['\n', '\r']))
}

/// `prefix` is `"key: "` or `"- "`.
fn push_scalar(prefix: &str, v: &Value, indent: usize, out: &mut Vec<String>) {
    if is_multiline(v) {
        out.push(format!("{}{prefix}|", pad(indent)));
        // Keep blank and trailing lines: they are part of the value.
        let text = scalar_str(v).replace("\r\n", "\n").replace('\r', "\n");
        for line in text.split('\n') {
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
        // Values containing the separator or braces would make the line ambiguous.
        Value::Object(m)
            if m.values().all(|x| {
                is_scalar(x) && !is_multiline(x) && !scalar_str(x).contains(['{', '}', ';'])
            }) =>
        {
            m.iter()
                .map(|(k, x)| format!("{k}: {}", scalar_str(x)))
                .collect::<Vec<_>>()
                .join("; ")
        }
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
        Value::Object(m) if m.is_empty() => out.push(format!("{}- {{}}", pad(indent))),
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
        let out = fit_value(&v, 200, "narrow it");
        assert!(out.len() <= 200 * CHARS_PER_TOKEN, "{}", out.len());
        assert!(out.starts_with("[!] TRUNCATED: showing"), "{out}");
        assert!(out.contains("narrow it"));
        assert!(!fit_value(&json!({ "a": 1 }), 200, "").contains("TRUNCATED"));
    }

    #[test]
    fn rendering_is_unambiguous() {
        let v = json!({
            "sig": { "id": "a", "signature": "fn f(x: [u8; 4])" },
            "items": [{}, ""],
            "text": "a\n\nb\n",
        });
        assert_eq!(
            to_lines(&v),
            vec![
                "sig:",
                "  id: a",
                "  signature: fn f(x: [u8; 4])",
                "items:",
                "  - {}",
                "  - \"\"",
                "text: |",
                "  a",
                "  ",
                "  b",
                "  ",
            ]
        );
    }

    /// The exact-length accounting: whatever the shape, the output fits the budget and
    /// the suggested budget shows everything.
    #[test]
    fn suggested_budget_always_shows_all() {
        let suggested = |out: &str| -> Option<usize> {
            out.split("token_budget=")
                .nth(1)?
                .split_whitespace()
                .next()?
                .parse()
                .ok()
        };
        let shapes: Vec<Vec<Vec<String>>> = vec![
            vec![vec!["x".repeat(700)]],
            vec![vec!["a".repeat(33), "b".into()]],
            (0..3000).map(|i| vec![format!("f{i}")]).collect(),
            (0..50)
                .map(|i| vec![format!("node {i}"), "    calls → y".into()])
                .collect(),
        ];
        for unit in ["nodes", "lines"] {
            for blocks in &shapes {
                for budget in [200, 333, 1000, 5000] {
                    let out = fit_blocks("Question: q", blocks, budget, unit, "narrow");
                    assert!(
                        out.len() <= budget * CHARS_PER_TOKEN,
                        "{unit} {budget}: {}",
                        out.len()
                    );
                    if let Some(s) = suggested(&out) {
                        let all = fit_blocks("Question: q", blocks, s, unit, "narrow");
                        assert!(!all.contains("TRUNCATED"), "{unit} {budget} -> {s}");
                    }
                    assert!(!out.contains("… 0 more lines"), "{out}");
                }
            }
        }
    }

    #[test]
    fn long_headers_are_clipped_and_huge_outputs_say_so() {
        let header = "漢".repeat(500);
        let out = fit_blocks(
            &header,
            &vec![vec!["x".to_string()]; 10],
            200,
            "nodes",
            "narrow",
        );
        assert!(out.len() <= 600, "{}", out.len());
        let huge: Vec<Vec<String>> = (0..40_000).map(|i| vec![format!("line {i:08}")]).collect();
        let out = fit_blocks("", &huge, 1000, "lines", "narrow");
        assert!(
            out.contains("exceeds the max token_budget=100000"),
            "{}",
            &out[..200]
        );
    }

    #[test]
    fn budget_holds_for_one_huge_block() {
        let mut block = vec!["hub".to_string()];
        block.extend((0..1500).map(|i| format!("    called_by ← caller_{i}")));
        let out = fit_blocks("H", &[block], 200, "nodes", "narrow");
        assert!(out.len() <= 200 * CHARS_PER_TOKEN, "{}", out.len());
        assert!(
            out.contains("1 partially") && out.contains("more lines cut"),
            "{out}"
        );
        let small = fit_blocks("H", &[vec!["a".into()]], usize::MAX, "nodes", "");
        assert!(!small.contains("TRUNCATED"));
    }
}
