pub(crate) fn brace_delta(line: &str) -> i32 {
    let mut n = 0i32;
    for ch in line.chars() {
        match ch {
            '{' => n += 1,
            '}' => n -= 1,
            _ => {}
        }
    }
    n
}

/// Inlang identifiers use kebab-case: letters, digits, and internal `-`.
/// Underscores are not part of the surface language.
pub(crate) fn is_ident_start(ch: char) -> bool {
    ch.is_ascii_alphabetic()
}

pub(crate) fn is_ident_continue(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '-'
}

pub(crate) fn is_ident_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(ch) if is_ident_start(ch) => chars.all(is_ident_continue),
        _ => false,
    }
}

pub(crate) fn trim(s: &str) -> &str {
    s.trim()
}

pub(crate) fn line_indent(raw: &str) -> usize {
    raw.chars().take_while(|ch| ch.is_whitespace()).count()
}

pub(crate) fn strip_trailing_colon(line: &str) -> &str {
    line.strip_suffix(':').unwrap_or(line).trim()
}

/// Split on first `:` not inside parentheses (for single-line fn: `fn foo(x: Int) -> Ret: body`).
pub(crate) fn split_first_colon(line: &str) -> Option<(&str, &str)> {
    let mut paren_depth = 0u32;
    for (i, c) in line.char_indices() {
        match c {
            '(' => paren_depth += 1,
            ')' => paren_depth = paren_depth.saturating_sub(1),
            ':' if paren_depth == 0 => return Some((&line[..i], &line[i + 1..])),
            _ => {}
        }
    }
    None
}

pub(crate) fn split_and_trim(sep: char, s: &str) -> Vec<String> {
    s.split(sep)
        .map(trim)
        .filter(|x| !x.is_empty())
        .map(String::from)
        .collect()
}

pub(crate) fn brace_content_after_open(s: &str, open_idx: usize) -> Option<&str> {
    if open_idx >= s.len() || !s[open_idx..].starts_with('{') {
        return None;
    }
    let mut d = 0i32;
    let mut in_string = false;
    let mut escape = false;
    for (i, c) in s[open_idx..].char_indices() {
        let abs = open_idx + i;
        if escape {
            escape = false;
            continue;
        }
        if in_string {
            if c == '\\' {
                escape = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '{' => d += 1,
            '}' => {
                d -= 1;
                if d == 0 {
                    return Some(&s[open_idx + 1..abs]);
                }
            }
            _ => {}
        }
    }
    None
}

pub(crate) fn find_fn_body_open_brace(rest: &str) -> Option<usize> {
    let mut paren = 0i32;
    let mut saw_open_paren = false;
    let mut in_string = false;
    let mut escape = false;
    for (i, c) in rest.char_indices() {
        if escape {
            escape = false;
            continue;
        }
        if in_string {
            if c == '\\' {
                escape = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '(' => {
                paren += 1;
                saw_open_paren = true;
            }
            ')' => paren -= 1,
            '{' if paren == 0 && saw_open_paren => return Some(i),
            _ => {}
        }
    }
    None
}

pub(crate) fn strip_line_comment_outside_strings(seg: &str) -> &str {
    let mut in_string = false;
    let mut escape = false;
    for (i, c) in seg.char_indices() {
        if escape {
            escape = false;
            continue;
        }
        if in_string {
            if c == '\\' {
                escape = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '/' if seg.get(i + 1..).is_some_and(|t| t.starts_with('/')) => {
                return trim(&seg[..i]);
            }
            _ => {}
        }
    }
    seg
}
pub(crate) fn split_struct_field_segments(inner: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut in_string = false;
    let mut escape = false;
    for (i, c) in inner.char_indices() {
        if escape {
            escape = false;
            continue;
        }
        if in_string {
            if c == '\\' {
                escape = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            ';' | '\n' => {
                let piece = trim(&inner[start..i]);
                if !piece.is_empty() {
                    out.push(piece);
                }
                start = i + 1;
            }
            _ => {}
        }
    }
    let tail = trim(&inner[start..]);
    if !tail.is_empty() {
        out.push(tail);
    }
    out
}

pub(crate) fn strip_enclosing_parens(s: &str) -> Option<&str> {
    let s = trim(s);
    if !(s.starts_with('(') && s.ends_with(')')) {
        return None;
    }
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escape = false;
    for (i, c) in s.char_indices() {
        if escape {
            escape = false;
            continue;
        }
        if in_string {
            if c == '\\' {
                escape = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 && i + c.len_utf8() < s.len() {
                    return None;
                }
            }
            '[' => depth += 1,
            ']' => depth -= 1,
            _ => {}
        }
    }
    if depth == 0 {
        Some(&s[1..s.len() - 1])
    } else {
        None
    }
}

pub(crate) fn find_top_level_binary_op<'a>(s: &str, ops: &[&'a str]) -> Option<(&'a str, usize)> {
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escape = false;
    let mut matches = Vec::new();
    // Track the end index of the last matched operator so we don't match
    // characters that are part of a longer operator (e.g. matching `<` inside `<<`).
    let mut skip_until: Option<usize> = None;
    for (i, c) in s.char_indices() {
        if let Some(end) = skip_until {
            if i < end {
                continue;
            }
            skip_until = None;
        }
        if escape {
            escape = false;
            continue;
        }
        if in_string {
            if c == '\\' {
                escape = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '(' => depth += 1,
            ')' => depth -= 1,
            '{' => depth += 1,
            '}' => depth -= 1,
            '[' => depth += 1,
            ']' => depth -= 1,
            _ if depth == 0 => {
                let mut best: Option<(&'a str, usize)> = None;
                for op in ops {
                    if s[i..].starts_with(op) {
                        if *op == "-" && s[i + 1..].starts_with('>') {
                            continue;
                        }
                        // Kebab-case identifiers: `foo-bar` is one name.
                        // Keep numeric subtraction (`1-2`) and spaced minus (`a - b`).
                        if *op == "-" {
                            let prev = s[..i].chars().next_back();
                            let next = s[i + 1..].chars().next();
                            if let (Some(p), Some(n)) = (prev, next) {
                                let mid_ident = is_ident_continue(p)
                                    && is_ident_continue(n)
                                    && !(p.is_ascii_digit() && n.is_ascii_digit());
                                if mid_ident {
                                    continue;
                                }
                            }
                        }
                        match best {
                            Some((prev, _)) if op.len() <= prev.len() => {}
                            _ => best = Some((*op, i)),
                        }
                    }
                }
                if let Some((op, pos)) = best {
                    matches.push((op, pos));
                    // Skip characters consumed by this operator so sub-sequences
                    // (e.g. '<' inside '<<') don't produce a second match.
                    if op.len() > 1 {
                        skip_until = Some(pos + op.len());
                    }
                }
            }
            _ => {}
        }
    }
    matches.into_iter().last()
}

pub(crate) fn find_top_level_field_dot(s: &str) -> Option<usize> {
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escape = false;
    let mut found = None;
    for (i, c) in s.char_indices() {
        if escape {
            escape = false;
            continue;
        }
        if in_string {
            if c == '\\' {
                escape = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '(' | '{' | '[' => depth += 1,
            ')' | '}' | ']' => depth -= 1,
            '.' if depth == 0 => found = Some(i),
            _ => {}
        }
    }
    found
}

pub(crate) fn find_top_level_index_open(s: &str) -> Option<usize> {
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escape = false;
    let mut found = None;
    for (i, c) in s.char_indices() {
        if escape {
            escape = false;
            continue;
        }
        if in_string {
            if c == '\\' {
                escape = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '(' | '{' => depth += 1,
            ')' | '}' => depth -= 1,
            '[' => {
                if depth == 0 && found.is_none() {
                    found = Some(i);
                }
                depth += 1;
            }
            ']' => depth -= 1,
            _ => {}
        }
    }
    found
}

pub(crate) fn find_struct_init_open_brace(s: &str) -> Option<usize> {
    let mut paren = 0i32;
    let mut in_string = false;
    let mut escape = false;
    for (i, c) in s.char_indices() {
        if escape {
            escape = false;
            continue;
        }
        if in_string {
            if c == '\\' {
                escape = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '(' => paren += 1,
            ')' => paren -= 1,
            '[' => paren += 1,
            ']' => paren -= 1,
            '{' if paren == 0 => return Some(i),
            _ => {}
        }
    }
    None
}

pub(crate) fn find_call_open_paren(s: &str) -> Option<usize> {
    let mut in_string = false;
    let mut escape = false;
    for (i, c) in s.char_indices() {
        if escape {
            escape = false;
            continue;
        }
        if in_string {
            if c == '\\' {
                escape = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '(' => return Some(i),
            _ => {}
        }
    }
    None
}

pub(crate) fn split_call_args(inner: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escape = false;
    for (i, c) in inner.char_indices() {
        if escape {
            escape = false;
            continue;
        }
        if in_string {
            if c == '\\' {
                escape = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '(' => depth += 1,
            ')' => depth -= 1,
            '{' => depth += 1,
            '}' => depth -= 1,
            '[' => depth += 1,
            ']' => depth -= 1,
            ',' if depth == 0 => {
                let arg = trim(&inner[start..i]);
                if !arg.is_empty() {
                    out.push(arg.to_string());
                }
                start = i + 1;
            }
            _ => {}
        }
    }
    let tail = trim(&inner[start..]);
    if !tail.is_empty() {
        out.push(tail.to_string());
    }
    out
}

pub(crate) fn split_struct_init_fields(inner: &str) -> Vec<String> {
    split_call_args(inner)
}

pub(crate) fn split_function_statements(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escape = false;
    for (i, c) in body.char_indices() {
        if escape {
            escape = false;
            continue;
        }
        if in_string {
            if c == '\\' {
                escape = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    if body[i + 1..].trim_start().starts_with("else") {
                        continue;
                    }
                    if body[i + 1..].trim_start().starts_with("catch") {
                        continue;
                    }
                    if body[i + 1..].trim_start().starts_with('.') {
                        continue;
                    }
                    let stmt = trim(&body[start..=i]);
                    if !stmt.is_empty() {
                        out.push(stmt.to_string());
                    }
                    start = i + 1;
                }
            }
            ';' | '\n' if depth == 0 => {
                if c == '\n' && body[start..i].trim_end().ends_with('}') {
                    let tail = body[i + 1..].trim_start();
                    if tail.starts_with("else")
                        || tail.starts_with("catch")
                        || tail.starts_with('.')
                    {
                        continue;
                    }
                }
                let stmt = trim(&body[start..i]);
                if !stmt.is_empty() {
                    let stmt = strip_line_comment_outside_strings(stmt);
                    let stmt = trim(stmt);
                    if !stmt.is_empty() && !stmt.starts_with("//") {
                        out.push(stmt.to_string());
                    }
                }
                start = i + 1;
            }
            _ => {}
        }
    }
    let tail = trim(&body[start..]);
    if !tail.is_empty() {
        let tail = strip_line_comment_outside_strings(tail);
        let tail = trim(tail);
        if !tail.is_empty() && !tail.starts_with("//") {
            out.push(tail.to_string());
        }
    }
    out
}

pub(crate) fn brace_content_bounds_after_open(s: &str, open_idx: usize) -> Option<(&str, usize)> {
    if open_idx >= s.len() || !s[open_idx..].starts_with('{') {
        return None;
    }
    let mut d = 0i32;
    let mut in_string = false;
    let mut escape = false;
    for (i, c) in s[open_idx..].char_indices() {
        let abs = open_idx + i;
        if escape {
            escape = false;
            continue;
        }
        if in_string {
            if c == '\\' {
                escape = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '{' => d += 1,
            '}' => {
                d -= 1;
                if d == 0 {
                    return Some((&s[open_idx + 1..abs], abs));
                }
            }
            _ => {}
        }
    }
    None
}
