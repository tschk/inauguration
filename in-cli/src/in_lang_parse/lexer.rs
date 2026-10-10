use super::util::*;
use std::sync::atomic::{AtomicBool, Ordering};

static HUMAN_IN_DEBUG: AtomicBool = AtomicBool::new(false);

pub fn set_human_in_debug(debug: bool) {
    HUMAN_IN_DEBUG.store(debug, Ordering::Relaxed);
}

pub fn human_in_debug() -> bool {
    HUMAN_IN_DEBUG.load(Ordering::Relaxed)
}

pub struct HumanInDebugGuard(bool);

impl HumanInDebugGuard {
    pub fn new(debug: bool) -> Self {
        let prev = human_in_debug();
        set_human_in_debug(debug);
        Self(prev)
    }
}

impl Drop for HumanInDebugGuard {
    fn drop(&mut self) {
        set_human_in_debug(self.0);
    }
}

pub(crate) fn human_call_stmt(line: &str) -> Option<String> {
    let trimmed = line.trim();
    let (name, rest) = trimmed.split_once(' ')?;
    if name.is_empty() || rest.is_empty() || name.contains('(') || !is_ident_name(name) {
        return None;
    }
    Some(format!("{name}({rest})"))
}

pub(crate) fn normalize_human_stmt(line: &str) -> String {
    let trimmed = line.trim();
    if trimmed == "done" {
        return "return".to_string();
    }
    if let Some(call) = human_call_stmt(trimmed) {
        return call;
    }
    if trimmed.contains('(')
        || trimmed.contains('=')
        || trimmed.starts_with("return")
        || trimmed.starts_with("let ")
        || trimmed.starts_with("var ")
        || trimmed.starts_with("if ")
        || trimmed.starts_with("while ")
        || trimmed.starts_with("match ")
        || trimmed.starts_with("throw ")
        || trimmed.starts_with("try ")
    {
        return trimmed.to_string();
    }
    if is_ident_name(trimmed) {
        return format!("{trimmed}()");
    }
    trimmed.to_string()
}

pub(crate) fn next_nonempty_line<'a>(lines: &'a [&'a str], start: usize) -> Option<&'a str> {
    lines
        .iter()
        .skip(start)
        .map(|line| line.trim())
        .find(|line| !line.is_empty())
}

/// Transform the "human-friendly" `.in` syntax into normalised brace form.
pub fn normalize_human_in_source(source: &str) -> String {
    // If no line ends with `:`, this is brace-form source — return unchanged.
    let has_human_fn = source
        .lines()
        .any(|l| l.trim().ends_with(':') && !l.trim().starts_with("//"));
    if !has_human_fn {
        return source.to_string();
    }
    let lines: Vec<&str> = source.lines().collect();
    let mut out = Vec::new();
    let mut stack: Vec<(&str, usize)> = Vec::new();

    if human_in_debug() {
        eprintln!("=== NORMALIZE INPUT ({}B) ===", source.len());
        eprintln!("{source}");
        eprintln!("=== END INPUT ===");
    }

    for (idx, raw_line) in lines.iter().enumerate() {
        let raw_line = strip_line_comment_outside_strings(raw_line);
        if raw_line.trim().is_empty() {
            continue;
        }
        let indent = line_indent(raw_line);
        while let Some((_, block_indent)) = stack.last() {
            if indent <= *block_indent {
                out.push("}".to_string());
                stack.pop();
            } else {
                break;
            }
        }

        let line = raw_line.trim();
        let next_line = next_nonempty_line(&lines, idx + 1).unwrap_or("");
        let next_trimmed = next_line.trim();
        let next_is_field = next_line.contains(':')
            && !next_line.ends_with(':')
            && !next_line.contains('(')
            && !next_line.starts_with('@')
            && !next_trimmed.starts_with("return")
            && !next_trimmed.starts_with("if ")
            && !next_trimmed.starts_with("while ")
            && !next_trimmed.starts_with("for ")
            && !next_trimmed.starts_with("let ")
            && !next_trimmed.starts_with("var ")
            && !next_trimmed.starts_with("print")
            && !next_trimmed.starts_with("fn ");

        if stack.last().map(|(kind, _)| *kind) == Some("struct") {
            if let Some((field, ty)) = line.split_once(':') {
                out.push(format!("{} {};", trim(ty), trim(field)));
                continue;
            }
        }

        if stack.last().map(|(kind, _)| *kind) == Some("stmt") {
            if line.ends_with(':') {
                if line == "parallel:" {
                    out.push("parallel {".to_string());
                    stack.push(("stmt", indent));
                    continue;
                }
                let header = strip_trailing_colon(line);
                let fn_header = if header.starts_with("distributed ") {
                    format!(
                        "distributed fn {} -> void {{",
                        trim(&header["distributed ".len()..])
                    )
                } else if header.contains('(') {
                    format!("fn {header} -> void {{")
                } else {
                    format!("fn {header}() -> void {{")
                };
                out.push(fn_header);
                stack.push(("stmt", indent));
                continue;
            }
            out.push(normalize_human_stmt(line));
            continue;
        }

        if let Some(rest) = line.strip_prefix("needs ") {
            out.push(format!("capability {};", trim(rest)));
            continue;
        }
        if let Some(rest) = line.strip_prefix("import ") {
            let rest = trim(rest);
            if rest.ends_with(';') {
                out.push(line.to_string());
            } else {
                out.push(format!("import {rest};"));
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("enable ") {
            let rest = trim(rest);
            if rest.ends_with(';') {
                out.push(line.to_string());
            } else {
                out.push(format!("enable {rest};"));
            }
            continue;
        }
        if let Some((sig, caps)) = line.split_once(" uses ") {
            if sig.contains('(') && !line.starts_with("extern ") {
                out.push(format!(
                    "extern human fn {} -> void requires {};",
                    trim(sig),
                    trim(caps)
                ));
                continue;
            }
        }
        if line.ends_with(':') {
            if line == "parallel:" {
                out.push("parallel {".to_string());
                stack.push(("stmt", indent));
                continue;
            }
            if next_is_field {
                let header = strip_trailing_colon(line);
                if header.starts_with("struct ") {
                    out.push(format!("{header} {{"));
                } else {
                    out.push(format!("struct {header} {{"));
                }
                stack.push(("struct", indent));
                continue;
            }
            let header = strip_trailing_colon(line);
            let fn_header = if header.starts_with("distributed ") {
                format!(
                    "distributed fn {} -> void {{",
                    trim(&header["distributed ".len()..])
                )
            } else if header.contains('(') {
                if header.contains("->") {
                    if header.starts_with("fn ") {
                        format!("{header} {{")
                    } else {
                        format!("fn {header} {{")
                    }
                } else {
                    if header.starts_with("fn ") {
                        format!("{header} -> void {{")
                    } else {
                        format!("fn {header} -> void {{")
                    }
                }
            } else {
                format!("fn {header}() -> void {{")
            };
            out.push(fn_header);
            stack.push(("stmt", indent));
            continue;
        }
        // Single-line fn with inline body: `fn name() -> Type: body;`
        if !line.ends_with(':') && line.contains(':') && !line.starts_with("@") {
            if let Some((header, body)) = split_first_colon(line) {
                let header = trim(header);
                let body = trim(body);
                if header.starts_with("fn ") || header.contains('(') {
                    let fn_header = if header.contains("->") {
                        format!("{header} {{")
                    } else {
                        format!("{header} -> void {{")
                    };
                    out.push(fn_header);
                    out.push(body.to_string());
                    out.push("}".to_string());
                    continue;
                }
            }
        }
        out.push(line.to_string());
    }

    while !stack.is_empty() {
        out.push("}".to_string());
        stack.pop();
    }

    out.join("\n")
}

/// Split source into complete top-level `struct` / `fn` declaration blocks (brace-balanced at depth 0).
pub fn split_top_level_decl_blocks(source: &str) -> Vec<(usize, String)> {
    let mut depth = 0i32;
    let mut current: Option<Vec<(usize, String)>> = None;
    let mut out = Vec::new();
    for (line_no, raw_line) in source.lines().enumerate() {
        let raw_line = strip_line_comment_outside_strings(raw_line);
        let t = raw_line.trim();
        let delta = brace_delta(raw_line);

        if current.is_none() {
            if t.is_empty() || t.starts_with("//") {
                continue;
            }
            if depth == 0
                && (t.starts_with("fn ")
                    || t.starts_with("interrupt fn ")
                    || t.starts_with("struct ")
                    || t.starts_with("extern ")
                    || t.starts_with("class ")
                    || t.starts_with("interface ")
                    || t.starts_with("component ")
                    || t.starts_with("var ")
                    || t.starts_with("const "))
            {
                current = Some(vec![(line_no + 1, t.to_string())]);
                depth += delta;
                if depth == 0 {
                    let buf = current.take().expect("just set");
                    let text = buf
                        .into_iter()
                        .map(|(_, s)| s)
                        .collect::<Vec<_>>()
                        .join("\n");
                    out.push((line_no + 1, text));
                }
                continue;
            }
            continue;
        }

        if !(t.is_empty() || t.starts_with("//")) {
            current
                .as_mut()
                .expect("inside decl")
                .push((line_no + 1, t.to_string()));
        }
        depth += delta;
        if depth < 0 {
            depth = 0;
        }
        if depth == 0 {
            let buf = current.take().expect("inside decl");
            let start_line = buf.first().map(|(l, _)| *l).unwrap_or(1);
            let text = buf
                .into_iter()
                .map(|(_, s)| s)
                .collect::<Vec<_>>()
                .join("\n");
            out.push((start_line, text));
        }
    }
    out
}

/// Legacy: line-oriented filter (single-line decls only). Prefer [`split_top_level_decl_blocks`].
pub fn filter_top_level_in_decl_lines(source: &str) -> String {
    split_top_level_decl_blocks(source)
        .into_iter()
        .map(|(_, text)| text)
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_human_in_debug_toggle() {
        let initial = human_in_debug();

        set_human_in_debug(true);
        assert!(human_in_debug());

        set_human_in_debug(false);
        assert!(!human_in_debug());

        set_human_in_debug(initial);
    }
}
