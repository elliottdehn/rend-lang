//! `json` runtime type. Internally **jsonb**-shaped: parsed at the
//! trust boundary (host input or `parse_json(string)`) into a
//! structured value, then accessed in parsed form. State storage
//! uses a tagged binary serialization, not the original text.
//!
//! Trade-offs we inherit from the parsed-once model:
//!   * Path access (`j -> "key"`, `j -> [n]`) is fast — no re-parse
//!     per step. Object key lookup is O(N) over the keys; array
//!     index is O(1).
//!   * Storage cost is roughly the size of the parsed form,
//!     typically smaller than the original JSON text.
//!   * Whitespace and original key formatting are not preserved.
//!     Object keys keep insertion order (deterministic for content-
//!     addressed cells) — Postgres jsonb canonicalizes by sorting
//!     keys; we don't, which keeps round-trips structurally equal.
//!
//! Numeric handling: JSON has one numeric kind. We split into
//! `Int(i64)` for negative or sign-bearing integers and `U64` for
//! non-negative integers wider than i64. Floats are not modeled —
//! rend has no float type. A JSON value carrying a fractional
//! number fails to parse.
//!
//! "Buyer beware" semantics throughout: missing keys / wrong shapes
//! yield `Json::Null` or runtime errors at conversion sites. There
//! is no static schema check; the type is intentionally dynamic.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Json {
    Null,
    Bool(bool),
    Int(i64),
    U64(u64),
    Str(String),
    Array(Vec<Json>),
    /// Object: insertion-order-preserving key/value list. Linear key
    /// lookup is fine for typical JSON document sizes; switch to a
    /// map representation later if profiling demands it.
    Object(Vec<(String, Json)>),
}

impl Json {
    /// Object key access. Missing key → `None`. Non-object → `None`.
    pub fn get_field(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Object(pairs) => pairs.iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v),
            _ => None,
        }
    }

    /// Array index access. Out-of-range → `None`. Non-array → `None`.
    pub fn get_index(&self, idx: usize) -> Option<&Json> {
        match self {
            Json::Array(items) => items.get(idx),
            _ => None,
        }
    }

    /// Best-effort canonical text rendering. Used by `json_stringify`
    /// and for `Display` (mostly diagnostic output). Output is
    /// deterministic given the same input value.
    pub fn to_string_canonical(&self) -> String {
        let mut out = String::new();
        write_canonical(self, &mut out);
        out
    }
}

fn write_canonical(j: &Json, out: &mut String) {
    match j {
        Json::Null => out.push_str("null"),
        Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Json::Int(n) => out.push_str(&n.to_string()),
        Json::U64(n) => out.push_str(&n.to_string()),
        Json::Str(s) => write_string(s, out),
        Json::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 { out.push(','); }
                write_canonical(item, out);
            }
            out.push(']');
        }
        Json::Object(pairs) => {
            out.push('{');
            for (i, (k, v)) in pairs.iter().enumerate() {
                if i > 0 { out.push(','); }
                write_string(k, out);
                out.push(':');
                write_canonical(v, out);
            }
            out.push('}');
        }
    }
}

fn write_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

impl fmt::Display for Json {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_string_canonical())
    }
}

// ---------- parser ----------

#[derive(Debug)]
pub struct ParseError {
    pub msg: String,
    pub pos: usize,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "JSON parse error at {}: {}", self.pos, self.msg)
    }
}

pub fn parse(s: &str) -> Result<Json, ParseError> {
    let bytes = s.as_bytes();
    let mut p = Parser { bytes, pos: 0 };
    p.skip_ws();
    let v = p.parse_value()?;
    p.skip_ws();
    if p.pos != bytes.len() {
        return Err(ParseError {
            msg: format!("unexpected trailing data at {}", p.pos),
            pos: p.pos,
        });
    }
    Ok(v)
}

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn advance(&mut self) -> Option<u8> {
        let b = self.peek();
        if b.is_some() { self.pos += 1; }
        b
    }

    fn skip_ws(&mut self) {
        while let Some(b) = self.peek() {
            if matches!(b, b' ' | b'\t' | b'\n' | b'\r') {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn err(&self, msg: impl Into<String>) -> ParseError {
        ParseError { msg: msg.into(), pos: self.pos }
    }

    fn parse_value(&mut self) -> Result<Json, ParseError> {
        self.skip_ws();
        match self.peek() {
            Some(b'n') => self.parse_lit("null", Json::Null),
            Some(b't') => self.parse_lit("true", Json::Bool(true)),
            Some(b'f') => self.parse_lit("false", Json::Bool(false)),
            Some(b'"') => self.parse_string().map(Json::Str),
            Some(b'[') => self.parse_array(),
            Some(b'{') => self.parse_object(),
            Some(b'-') | Some(b'0'..=b'9') => self.parse_number(),
            Some(b) => Err(self.err(format!("unexpected byte {b:?}"))),
            None => Err(self.err("unexpected end of input")),
        }
    }

    fn parse_lit(&mut self, lit: &str, value: Json) -> Result<Json, ParseError> {
        for &c in lit.as_bytes() {
            if self.advance() != Some(c) {
                return Err(self.err(format!("expected literal '{lit}'")));
            }
        }
        Ok(value)
    }

    fn parse_string(&mut self) -> Result<String, ParseError> {
        if self.advance() != Some(b'"') {
            return Err(self.err("expected '\"'"));
        }
        let mut out = String::new();
        loop {
            match self.advance() {
                Some(b'"') => return Ok(out),
                Some(b'\\') => match self.advance() {
                    Some(b'"') => out.push('"'),
                    Some(b'\\') => out.push('\\'),
                    Some(b'/') => out.push('/'),
                    Some(b'n') => out.push('\n'),
                    Some(b'r') => out.push('\r'),
                    Some(b't') => out.push('\t'),
                    Some(b'b') => out.push('\u{08}'),
                    Some(b'f') => out.push('\u{0c}'),
                    Some(b'u') => {
                        let mut hex = 0u32;
                        for _ in 0..4 {
                            let b = self.advance().ok_or_else(|| {
                                self.err("incomplete \\u escape")
                            })?;
                            let v = match b {
                                b'0'..=b'9' => (b - b'0') as u32,
                                b'a'..=b'f' => (b - b'a' + 10) as u32,
                                b'A'..=b'F' => (b - b'A' + 10) as u32,
                                _ => return Err(self.err("invalid \\u hex digit")),
                            };
                            hex = (hex << 4) | v;
                        }
                        // Slice-1: no surrogate-pair handling. Single
                        // BMP code points only.
                        let c = char::from_u32(hex).ok_or_else(|| {
                            self.err("invalid \\u code point (surrogate pairs unsupported)")
                        })?;
                        out.push(c);
                    }
                    Some(b) => return Err(self.err(format!("invalid escape \\{:?}", b as char))),
                    None => return Err(self.err("incomplete escape")),
                },
                Some(b) if b < 0x20 => {
                    return Err(self.err("control character in string"));
                }
                Some(b) => {
                    // UTF-8: collect any continuation bytes.
                    let start = self.pos - 1;
                    let n = utf8_len(b);
                    if n > 1 {
                        for _ in 1..n {
                            self.advance().ok_or_else(|| {
                                self.err("incomplete UTF-8")
                            })?;
                        }
                    }
                    let s = std::str::from_utf8(&self.bytes[start..self.pos])
                        .map_err(|_| self.err("invalid UTF-8"))?;
                    out.push_str(s);
                }
                None => return Err(self.err("unterminated string")),
            }
        }
    }

    fn parse_array(&mut self) -> Result<Json, ParseError> {
        self.advance(); // '['
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.advance();
            return Ok(Json::Array(items));
        }
        loop {
            items.push(self.parse_value()?);
            self.skip_ws();
            match self.advance() {
                Some(b',') => self.skip_ws(),
                Some(b']') => return Ok(Json::Array(items)),
                Some(b) => return Err(self.err(format!("expected ',' or ']' in array, got {:?}", b as char))),
                None => return Err(self.err("unterminated array")),
            }
        }
    }

    fn parse_object(&mut self) -> Result<Json, ParseError> {
        self.advance(); // '{'
        let mut pairs = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.advance();
            return Ok(Json::Object(pairs));
        }
        loop {
            self.skip_ws();
            let key = self.parse_string()?;
            self.skip_ws();
            if self.advance() != Some(b':') {
                return Err(self.err("expected ':' after object key"));
            }
            self.skip_ws();
            let value = self.parse_value()?;
            pairs.push((key, value));
            self.skip_ws();
            match self.advance() {
                Some(b',') => {}
                Some(b'}') => return Ok(Json::Object(pairs)),
                Some(b) => return Err(self.err(format!("expected ',' or '}}' in object, got {:?}", b as char))),
                None => return Err(self.err("unterminated object")),
            }
        }
    }

    fn parse_number(&mut self) -> Result<Json, ParseError> {
        let start = self.pos;
        if self.peek() == Some(b'-') { self.advance(); }
        let int_start = self.pos;
        while let Some(b) = self.peek() {
            if b.is_ascii_digit() { self.advance(); } else { break; }
        }
        if int_start == self.pos {
            return Err(self.err("expected digits"));
        }
        // Reject fractional numbers — rend has no float type.
        if matches!(self.peek(), Some(b'.') | Some(b'e') | Some(b'E')) {
            return Err(self.err(
                "JSON numbers with fractional or exponent parts are not supported (rend has no float type)"
            ));
        }
        let s = std::str::from_utf8(&self.bytes[start..self.pos])
            .map_err(|_| self.err("invalid digits"))?;
        if let Ok(n) = s.parse::<i64>() {
            Ok(Json::Int(n))
        } else if !s.starts_with('-') {
            // Too big for i64 but might fit u64.
            s.parse::<u64>()
                .map(Json::U64)
                .map_err(|_| self.err("integer literal out of u64 range"))
        } else {
            Err(self.err("integer literal out of i64 range"))
        }
    }
}

fn utf8_len(first: u8) -> usize {
    if first < 0x80 { 1 }
    else if first < 0xC0 { 1 } // continuation byte (shouldn't be a leader)
    else if first < 0xE0 { 2 }
    else if first < 0xF0 { 3 }
    else { 4 }
}

// ---------- binary serialization (state cells) ----------

const TAG_NULL:   u8 = 0x01;
const TAG_TRUE:   u8 = 0x02;
const TAG_FALSE:  u8 = 0x03;
const TAG_INT:    u8 = 0x04;
const TAG_U64:    u8 = 0x05;
const TAG_STR:    u8 = 0x06;
const TAG_ARRAY:  u8 = 0x07;
const TAG_OBJECT: u8 = 0x08;

pub fn serialize(j: &Json) -> Vec<u8> {
    let mut out = Vec::new();
    write_node(j, &mut out);
    out
}

fn write_node(j: &Json, out: &mut Vec<u8>) {
    match j {
        Json::Null     => out.push(TAG_NULL),
        Json::Bool(true)  => out.push(TAG_TRUE),
        Json::Bool(false) => out.push(TAG_FALSE),
        Json::Int(n)   => { out.push(TAG_INT); out.extend_from_slice(&n.to_be_bytes()); }
        Json::U64(n)   => { out.push(TAG_U64); out.extend_from_slice(&n.to_be_bytes()); }
        Json::Str(s)   => {
            out.push(TAG_STR);
            out.extend_from_slice(&(s.len() as u32).to_be_bytes());
            out.extend_from_slice(s.as_bytes());
        }
        Json::Array(items) => {
            out.push(TAG_ARRAY);
            out.extend_from_slice(&(items.len() as u32).to_be_bytes());
            for v in items { write_node(v, out); }
        }
        Json::Object(pairs) => {
            out.push(TAG_OBJECT);
            out.extend_from_slice(&(pairs.len() as u32).to_be_bytes());
            for (k, v) in pairs {
                out.extend_from_slice(&(k.len() as u32).to_be_bytes());
                out.extend_from_slice(k.as_bytes());
                write_node(v, out);
            }
        }
    }
}

/// How many bytes the serialized JSON value at the start of `bytes`
/// occupies. Used by struct-field deserialization to know where one
/// json field ends and the next field begins.
pub fn byte_size(bytes: &[u8]) -> Option<usize> {
    let mut r = Reader { bytes, pos: 0 };
    r.skip_node()?;
    Some(r.pos)
}

impl<'a> Reader<'a> {
    /// Walk a node without materializing it. Returns `None` if the
    /// bytes are malformed or truncated.
    fn skip_node(&mut self) -> Option<()> {
        let tag = self.read_byte()?;
        match tag {
            TAG_NULL | TAG_TRUE | TAG_FALSE => {}
            TAG_INT | TAG_U64 => { self.pos += 8; }
            TAG_STR => {
                let n = self.read_u32()? as usize;
                self.pos += n;
            }
            TAG_ARRAY => {
                let n = self.read_u32()? as usize;
                for _ in 0..n { self.skip_node()?; }
            }
            TAG_OBJECT => {
                let n = self.read_u32()? as usize;
                for _ in 0..n {
                    let kl = self.read_u32()? as usize;
                    self.pos += kl;
                    self.skip_node()?;
                }
            }
            _ => return None,
        }
        if self.pos > self.bytes.len() { None } else { Some(()) }
    }
}

pub fn deserialize(bytes: &[u8]) -> Option<Json> {
    let mut r = Reader { bytes, pos: 0 };
    let v = r.read_node()?;
    if r.pos != bytes.len() { return None; }
    Some(v)
}

struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn read_byte(&mut self) -> Option<u8> {
        let b = *self.bytes.get(self.pos)?;
        self.pos += 1;
        Some(b)
    }
    fn read_u32(&mut self) -> Option<u32> {
        let arr: [u8; 4] = self.bytes.get(self.pos..self.pos + 4)?.try_into().ok()?;
        self.pos += 4;
        Some(u32::from_be_bytes(arr))
    }
    fn read_str(&mut self, n: usize) -> Option<String> {
        let s = self.bytes.get(self.pos..self.pos + n)?;
        self.pos += n;
        std::str::from_utf8(s).ok().map(|s| s.to_string())
    }
    fn read_node(&mut self) -> Option<Json> {
        let tag = self.read_byte()?;
        Some(match tag {
            TAG_NULL  => Json::Null,
            TAG_TRUE  => Json::Bool(true),
            TAG_FALSE => Json::Bool(false),
            TAG_INT   => {
                let arr: [u8; 8] = self.bytes.get(self.pos..self.pos + 8)?.try_into().ok()?;
                self.pos += 8;
                Json::Int(i64::from_be_bytes(arr))
            }
            TAG_U64 => {
                let arr: [u8; 8] = self.bytes.get(self.pos..self.pos + 8)?.try_into().ok()?;
                self.pos += 8;
                Json::U64(u64::from_be_bytes(arr))
            }
            TAG_STR => {
                let n = self.read_u32()? as usize;
                Json::Str(self.read_str(n)?)
            }
            TAG_ARRAY => {
                let n = self.read_u32()? as usize;
                let mut items = Vec::with_capacity(n);
                for _ in 0..n { items.push(self.read_node()?); }
                Json::Array(items)
            }
            TAG_OBJECT => {
                let n = self.read_u32()? as usize;
                let mut pairs = Vec::with_capacity(n);
                for _ in 0..n {
                    let kl = self.read_u32()? as usize;
                    let k = self.read_str(kl)?;
                    let v = self.read_node()?;
                    pairs.push((k, v));
                }
                Json::Object(pairs)
            }
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_primitives() {
        assert_eq!(parse("null").unwrap(), Json::Null);
        assert_eq!(parse("true").unwrap(), Json::Bool(true));
        assert_eq!(parse("false").unwrap(), Json::Bool(false));
        assert_eq!(parse("42").unwrap(), Json::Int(42));
        assert_eq!(parse("-7").unwrap(), Json::Int(-7));
        assert_eq!(parse("18446744073709551615").unwrap(), Json::U64(u64::MAX));
        assert_eq!(parse("\"hello\"").unwrap(), Json::Str("hello".into()));
    }

    #[test]
    fn parse_array_and_object() {
        let v = parse("[1, 2, 3]").unwrap();
        assert_eq!(v, Json::Array(vec![Json::Int(1), Json::Int(2), Json::Int(3)]));

        let v = parse(r#"{"name": "alice", "age": 30}"#).unwrap();
        assert_eq!(v, Json::Object(vec![
            ("name".into(), Json::Str("alice".into())),
            ("age".into(), Json::Int(30)),
        ]));
    }

    #[test]
    fn parse_nested() {
        let v = parse(r#"{"users": [{"id": 1, "name": "a"}, {"id": 2, "name": "b"}]}"#).unwrap();
        let users = v.get_field("users").unwrap();
        let alice = users.get_index(0).unwrap();
        assert_eq!(alice.get_field("name"), Some(&Json::Str("a".into())));
    }

    #[test]
    fn rejects_floats() {
        assert!(parse("1.5").is_err());
        assert!(parse("1e10").is_err());
    }

    #[test]
    fn missing_key_returns_none() {
        let v = parse(r#"{"a": 1}"#).unwrap();
        assert_eq!(v.get_field("nope"), None);
        assert_eq!(v.get_index(5), None);
    }

    #[test]
    fn serialize_round_trip() {
        let v = parse(r#"{"name": "alice", "tags": ["a", "b"], "n": 42}"#).unwrap();
        let bytes = serialize(&v);
        let v2 = deserialize(&bytes).unwrap();
        assert_eq!(v, v2);
    }

    #[test]
    fn canonical_output_is_deterministic() {
        let v = parse(r#"{ "a"  : 1 ,  "b": [ 2,3 ] }"#).unwrap();
        assert_eq!(v.to_string_canonical(), r#"{"a":1,"b":[2,3]}"#);
    }
}
