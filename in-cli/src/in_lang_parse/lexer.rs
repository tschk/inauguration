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
    if [
        "return", "let", "var", "if", "while", "match", "throw", "try",
    ]
    .iter()
    .any(|keyword| {
        trimmed == *keyword
            || trimmed
                .strip_prefix(keyword)
                .is_some_and(|rest| rest.starts_with(char::is_whitespace))
    }) {
        return trimmed.to_string();
    }
    if let Some(call) = human_call_stmt(trimmed) {
        return call;
    }
    if trimmed.contains('(') || trimmed.contains('=') {
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
    let mut detection = SourceLineState::default();
    let has_human_fn = source.lines().any(|line| {
        let scanned = detection.scan_line(line);
        !scanned.starts_in_string
            && !scanned.ends_in_string
            && scanned.text.trim_end().ends_with(':')
    });
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

    let mut lexical = SourceLineState::default();
    for (idx, raw_line) in lines.iter().enumerate() {
        let scanned = lexical.scan_line(raw_line);
        let raw_line = scanned.text;
        if scanned.starts_in_string {
            out.push(raw_line.to_string());
            continue;
        }
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

        if scanned.ends_in_string {
            out.push(raw_line.to_string());
            continue;
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

pub(crate) fn is_core_decl_item(text: &str) -> bool {
    [
        "fn ",
        "interrupt fn ",
        "struct ",
        "class ",
        "interface ",
        "component ",
        "extern ",
        "var ",
        "const ",
    ]
    .iter()
    .any(|prefix| text.trim_start().starts_with(prefix))
}

/// Split complete top-level declarations at closing braces, semicolons, or newlines.
pub fn split_top_level_decl_blocks(source: &str) -> Vec<(usize, String)> {
    split_top_level_items(source)
        .unwrap_or_default()
        .into_iter()
        .filter(|(_, text)| is_core_decl_item(text))
        .collect()
}

pub(crate) fn split_top_level_items(source: &str) -> Result<Vec<(usize, String)>, String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut start_line = 1;
    let mut delimiters = Vec::new();
    let mut ends_in_string = false;
    let mut brace_decl = false;
    let mut lexical = SourceLineState::default();
    for (line_no, raw_line) in source.lines().enumerate() {
        let mut outside = Vec::new();
        let scanned = lexical.scan_line_with(raw_line, |index, ch| {
            if matches!(ch, '{' | '}' | '(' | ')' | '[' | ']' | ';') {
                outside.push((index, ch));
            }
        });
        ends_in_string = scanned.ends_in_string;
        let literal = scanned.starts_in_string || scanned.ends_in_string;
        let text = scanned.text;
        let mut events = outside.into_iter();
        let mut begin = 0;
        while begin < text.len() {
            if current.is_empty() {
                let rest =
                    text[begin..].trim_start_matches(|ch: char| ch.is_whitespace() || ch == ';');
                begin = text.len() - rest.len();
                if rest.is_empty() {
                    break;
                }
                // Initializer braces are expressions; other top-level blocks
                // (including unknown/product-owned syntax) remain opaque items.
                brace_decl = ![
                    "var ",
                    "const ",
                    "extern ",
                    "package ",
                    "module ",
                    "import ",
                    "use ",
                    "bind ",
                    "capability ",
                    "enable ",
                ]
                .iter()
                .any(|prefix| rest.starts_with(prefix))
                    && !rest.starts_with('@');
                start_line = line_no + 1;
            }
            let mut end = text.len();
            let mut complete = false;
            for (index, ch) in events.by_ref() {
                if index < begin {
                    continue;
                }
                match ch {
                    '{' | '(' | '[' => delimiters.push(ch),
                    '}' | ')' | ']' => {
                        let expected = match ch {
                            '}' => '{',
                            ')' => '(',
                            _ => '[',
                        };
                        if delimiters.pop() != Some(expected) {
                            return Err(format!(
                                ".in: mismatched delimiter at line {}",
                                line_no + 1
                            ));
                        }
                    }
                    _ => {}
                }
                if delimiters.is_empty() && (ch == ';' || (brace_decl && ch == '}')) {
                    end = index + ch.len_utf8();
                    complete = true;
                    break;
                }
            }
            let part = &text[begin..end];
            let part = if literal { part } else { part.trim() };
            if !part.is_empty() || literal {
                if !current.is_empty() {
                    current.push('\n');
                }
                current.push_str(part);
            }
            begin = end;
            if complete {
                out.push((start_line, std::mem::take(&mut current)));
            } else {
                break;
            }
        }
        // Preserve blank physical lines inside multiline literals.
        if text.is_empty() && literal && !current.is_empty() {
            current.push('\n');
        }
        if !current.is_empty() && delimiters.is_empty() && !scanned.ends_in_string {
            out.push((start_line, std::mem::take(&mut current)));
        }
    }
    if !delimiters.is_empty() || ends_in_string {
        return Err(format!(".in: incomplete declaration at line {start_line}"));
    }
    Ok(out)
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
