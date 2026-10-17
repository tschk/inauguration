mod core_ir {pub struct FloatVal(pub f64);pub enum Expr{IntLit(i64),FloatLit(FloatVal)}}
fn lua_string_literal(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let first = *bytes.first()?;
    if first == b'[' {
        let mut opening = 1;
        while bytes.get(opening) == Some(&b'=') {
            opening += 1;
        }
        if bytes.get(opening) != Some(&b'[') {
            return None;
        }
        let delimiter = opening + 1;
        let closing = bytes
            .get(..delimiter)?
            .iter()
            .map(|byte| match byte {
                b'[' => b']',
                byte => *byte,
            })
            .collect::<Vec<_>>();
        // Reject trailing data after the first matching delimiter.
        let content_end = delimiter
            + bytes
                .get(delimiter..)?
                .windows(delimiter)
                .position(|window| window == closing.as_slice())?;
        if content_end.checked_add(delimiter)? != bytes.len() {
            return None;
        }
        let mut at = delimiter;
        if matches!(bytes.get(at), Some(b'\r' | b'\n')) {
            at = lua_newline_end(bytes, at);
        }
        let mut output = Vec::new();
        while at < content_end {
            if matches!(bytes[at], b'\r' | b'\n') {
                output.push(b'\n');
                at = lua_newline_end(bytes, at);
            } else {
                output.push(bytes[at]);
                at += 1;
            }
        }
        return String::from_utf8(output).ok();
    }
    None
}
// Lua treats CR, LF, CRLF and LFCR as one line ending.
fn lua_newline_end(bytes: &[u8], at: usize) -> usize {
    let next = at + 1;
    if next < bytes.len() && matches!(bytes[next], b'\r' | b'\n') && bytes[next] != bytes[at] {
        next + 1
    } else {
        next
    }
}

const KEEP:Option<fn(&str)->Option<String>>=Some(lua_string_literal);
fn main(args:Vec<String>)->i64{42}
