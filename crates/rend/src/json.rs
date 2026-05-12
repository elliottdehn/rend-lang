//! `json` runtime type. Internally **jsonb**-shaped: parsed at the
//! trust boundary (host input or `parse_json(string)`) into a
//! structured value, then accessed in parsed form.
//!
//! ## Leaves are *native* rend values
//!
//! Json itself only carries the JSON-specific composite shapes:
//! `Null`, `Array(Vec<Value>)`, and `Object(IndexMap<String, Value>)`.
//! Primitive JSON leaves materialize as ordinary rend values —
//! `Value::Bool`, `Value::Int(BigInt)`, `Value::Float(F64Bits)`,
//! `Value::Str(String)`. There is no separate `Json::Int` /
//! `Json::Bool` / etc. — the rend type system's primitives *are*
//! the JSON leaves.
//!
//! Numeric handling:
//!   * Bare numbers without `.`/`e`/`E` parse to `Value::Int`
//!     (arbitrary precision, no overflow at parse time).
//!   * Numbers with `.`/`e`/`E` parse to `Value::Float`.
//!
//! Object lookup is O(1) via `IndexMap`; iteration order matches
//! insertion order so content-addressed cells over JSON hash
//! deterministically.
//!
//! "Buyer beware" semantics throughout: missing keys / wrong shapes
//! yield `None` at the path-access site (the host then surfaces
//! `Value::Json(Json::Null)`). There is no static schema check; the
//! type is intentionally dynamic.

use crate::value::{F64Bits, Value};
use indexmap::IndexMap;
use num_bigint::BigInt;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Json {
    /// JSON's `null` — the one truly Json-specific leaf shape; rend
    /// has no native null value to map onto.
    Null,
    /// Composite array. Each element is a native rend `Value`:
    /// numbers are `Value::Int(BigInt)` (or `Float`), booleans are
    /// `Value::Bool`, strings are `Value::Str`, nested arrays /
    /// objects are `Value::Json(Json::Array(...))` etc.
    Array(Vec<Value>),
    /// Composite object: insertion-order-preserving hash map.
    /// O(1) lookup, deterministic iteration. Values are native
    /// rend `Value`s, same shape as `Array`'s elements.
    Object(IndexMap<String, Value>),
}

impl Json {
    /// Object key access. Missing key → `None`. Non-object → `None`.
    pub fn get_field(&self, key: &str) -> Option<&Value> {
        match self {
            Json::Object(m) => m.get(key),
            _ => None,
        }
    }

    /// Array index access. Out-of-range → `None`. Non-array → `None`.
    pub fn get_index(&self, idx: usize) -> Option<&Value> {
        match self {
            Json::Array(items) => items.get(idx),
            _ => None,
        }
    }

    /// Best-effort canonical text rendering. Output is deterministic
    /// given the same input value.
    pub fn to_string_canonical(&self) -> String {
        let mut out = String::new();
        write_canonical_json(self, &mut out);
        out
    }
}

/// Canonical-text rendering for a Value that came from JSON
/// (primitives in their native variants). Falls back to `null` for
/// any rend Value that doesn't map to a JSON shape — e.g. a
/// `Value::Resource` or `Value::PMap` accidentally placed into a
/// json-typed cell. Reserved cells should never end up there
/// anyway; this is a defense, not a contract.
pub fn write_canonical_value(v: &Value, out: &mut String) {
    match v {
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Int(n) | Value::UInt(n) => out.push_str(&n.to_string()),
        Value::Float(F64Bits(n)) => {
            if n.is_finite() {
                let s = format!("{n}");
                if s.contains('.') || s.contains('e') || s.contains('E') {
                    out.push_str(&s);
                } else {
                    out.push_str(&s);
                    out.push_str(".0");
                }
            } else {
                // NaN / ±Inf aren't valid JSON. Emit `null` so the
                // output stays parseable; callers that need stricter
                // behavior should validate before serializing.
                out.push_str("null");
            }
        }
        Value::Str(s) => write_string(s, out),
        Value::Json(j) => write_canonical_json(j, out),
        _ => out.push_str("null"),
    }
}

fn write_canonical_json(j: &Json, out: &mut String) {
    match j {
        Json::Null => out.push_str("null"),
        Json::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 { out.push(','); }
                write_canonical_value(item, out);
            }
            out.push(']');
        }
        Json::Object(m) => {
            out.push('{');
            for (i, (k, v)) in m.iter().enumerate() {
                if i > 0 { out.push(','); }
                write_string(k, out);
                out.push(':');
                write_canonical_value(v, out);
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

/// Parse a JSON text into a rend `Value`. Primitives materialize as
/// native rend variants (`Value::Bool`, `Value::Int`, `Value::Float`,
/// `Value::Str`); `null`, arrays, and objects are wrapped as
/// `Value::Json(Json::*)`.
pub fn parse(s: &str) -> Result<Value, ParseError> {
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

    fn parse_value(&mut self) -> Result<Value, ParseError> {
        self.skip_ws();
        match self.peek() {
            Some(b'n') => self.parse_lit("null", Value::Json(Json::Null)),
            Some(b't') => self.parse_lit("true", Value::Bool(true)),
            Some(b'f') => self.parse_lit("false", Value::Bool(false)),
            Some(b'"') => self.parse_string().map(Value::Str),
            Some(b'[') => self.parse_array().map(|a| Value::Json(a)),
            Some(b'{') => self.parse_object().map(|o| Value::Json(o)),
            Some(b'-') | Some(b'0'..=b'9') => self.parse_number(),
            Some(b) => Err(self.err(format!("unexpected byte {b:?}"))),
            None => Err(self.err("unexpected end of input")),
        }
    }

    fn parse_lit(&mut self, lit: &str, value: Value) -> Result<Value, ParseError> {
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
        let mut items: Vec<Value> = Vec::new();
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
        let mut map: IndexMap<String, Value> = IndexMap::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.advance();
            return Ok(Json::Object(map));
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
            map.insert(key, value);
            self.skip_ws();
            match self.advance() {
                Some(b',') => {}
                Some(b'}') => return Ok(Json::Object(map)),
                Some(b) => return Err(self.err(format!("expected ',' or '}}' in object, got {:?}", b as char))),
                None => return Err(self.err("unterminated object")),
            }
        }
    }

    fn parse_number(&mut self) -> Result<Value, ParseError> {
        let start = self.pos;
        if self.peek() == Some(b'-') { self.advance(); }
        let int_start = self.pos;
        while let Some(b) = self.peek() {
            if b.is_ascii_digit() { self.advance(); } else { break; }
        }
        if int_start == self.pos {
            return Err(self.err("expected digits"));
        }
        let mut is_float = false;
        if self.peek() == Some(b'.') {
            is_float = true;
            self.advance();
            let frac_start = self.pos;
            while let Some(b) = self.peek() {
                if b.is_ascii_digit() { self.advance(); } else { break; }
            }
            if frac_start == self.pos {
                return Err(self.err("expected digits after '.'"));
            }
        }
        if matches!(self.peek(), Some(b'e') | Some(b'E')) {
            is_float = true;
            self.advance();
            if matches!(self.peek(), Some(b'+') | Some(b'-')) {
                self.advance();
            }
            let exp_start = self.pos;
            while let Some(b) = self.peek() {
                if b.is_ascii_digit() { self.advance(); } else { break; }
            }
            if exp_start == self.pos {
                return Err(self.err("expected digits in exponent"));
            }
        }
        let s = std::str::from_utf8(&self.bytes[start..self.pos])
            .map_err(|_| self.err("invalid number"))?;
        if is_float {
            let f: f64 = s.parse().map_err(|_| self.err("invalid float literal"))?;
            Ok(Value::Float(F64Bits(f)))
        } else {
            let n: BigInt = s.parse()
                .map_err(|_| self.err("invalid integer literal"))?;
            Ok(Value::Int(n))
        }
    }
}

fn utf8_len(first: u8) -> usize {
    if first < 0x80 { 1 }
    else if first < 0xC0 { 1 }
    else if first < 0xE0 { 2 }
    else if first < 0xF0 { 3 }
    else { 4 }
}

// ---------- binary serialization (composites only) ----------
//
// Json::Null / Array / Object are the only shapes encoded by this
// module. Primitive leaves live as ordinary rend `Value`s and use
// the regular `serialize::serialize` path. A json-typed state cell
// whose runtime value happens to be a primitive (e.g. `Value::Int`
// from `parse_json("42")`) is encoded with its primitive tag, not
// TAG_JSON; the deserializer in `serialize.rs` accepts any tag when
// the expected type is `Type::Json`.
//
// Tag scheme (inside TAG_JSON):
//   0x00 NULL
//   0x01 ARRAY   — u32 count, then `count` regular Value encodings
//   0x02 OBJECT  — u32 count, then `count` (u32 keylen, keybytes,
//                  Value) triples

const J_NULL: u8 = 0x00;
const J_ARRAY: u8 = 0x01;
const J_OBJECT: u8 = 0x02;

pub fn serialize(j: &Json) -> Vec<u8> {
    let mut out = Vec::new();
    write_json(j, &mut out);
    out
}

fn write_json(j: &Json, out: &mut Vec<u8>) {
    match j {
        Json::Null => out.push(J_NULL),
        Json::Array(items) => {
            out.push(J_ARRAY);
            out.extend_from_slice(&(items.len() as u32).to_be_bytes());
            for v in items {
                out.extend_from_slice(&crate::serialize::serialize(v));
            }
        }
        Json::Object(m) => {
            out.push(J_OBJECT);
            out.extend_from_slice(&(m.len() as u32).to_be_bytes());
            for (k, v) in m {
                out.extend_from_slice(&(k.len() as u32).to_be_bytes());
                out.extend_from_slice(k.as_bytes());
                out.extend_from_slice(&crate::serialize::serialize(v));
            }
        }
    }
}

/// How many bytes the serialized Json composite at the start of
/// `bytes` occupies. Used by struct-field deserialization to know
/// where one json field ends and the next field begins.
pub fn byte_size(bytes: &[u8]) -> Option<usize> {
    if bytes.is_empty() { return None; }
    match bytes[0] {
        J_NULL => Some(1),
        J_ARRAY => {
            if bytes.len() < 5 { return None; }
            let n = u32::from_be_bytes(bytes[1..5].try_into().ok()?) as usize;
            let mut total = 5;
            for _ in 0..n {
                let inner = crate::serialize::value_byte_size(bytes.get(total..)?, &crate::ast::Type::Json)?;
                total += inner;
            }
            Some(total)
        }
        J_OBJECT => {
            if bytes.len() < 5 { return None; }
            let n = u32::from_be_bytes(bytes[1..5].try_into().ok()?) as usize;
            let mut total = 5;
            for _ in 0..n {
                let kl = u32::from_be_bytes(bytes.get(total..total + 4)?.try_into().ok()?) as usize;
                total += 4 + kl;
                let inner = crate::serialize::value_byte_size(bytes.get(total..)?, &crate::ast::Type::Json)?;
                total += inner;
            }
            Some(total)
        }
        _ => None,
    }
}

pub fn deserialize(bytes: &[u8]) -> Option<Json> {
    if bytes.is_empty() { return None; }
    let mut pos = 0;
    let j = read_json(bytes, &mut pos)?;
    if pos != bytes.len() { return None; }
    Some(j)
}

fn read_json(bytes: &[u8], pos: &mut usize) -> Option<Json> {
    let tag = *bytes.get(*pos)?;
    *pos += 1;
    match tag {
        J_NULL => Some(Json::Null),
        J_ARRAY => {
            let n = read_u32(bytes, pos)? as usize;
            let mut items = Vec::with_capacity(n);
            for _ in 0..n {
                let size = crate::serialize::value_byte_size(bytes.get(*pos..)?, &crate::ast::Type::Json)?;
                let v = crate::serialize::deserialize(bytes.get(*pos..*pos + size)?, &crate::ast::Type::Json)?;
                *pos += size;
                items.push(v);
            }
            Some(Json::Array(items))
        }
        J_OBJECT => {
            let n = read_u32(bytes, pos)? as usize;
            let mut map = IndexMap::with_capacity(n);
            for _ in 0..n {
                let kl = read_u32(bytes, pos)? as usize;
                let k = std::str::from_utf8(bytes.get(*pos..*pos + kl)?).ok()?.to_string();
                *pos += kl;
                let size = crate::serialize::value_byte_size(bytes.get(*pos..)?, &crate::ast::Type::Json)?;
                let v = crate::serialize::deserialize(bytes.get(*pos..*pos + size)?, &crate::ast::Type::Json)?;
                *pos += size;
                map.insert(k, v);
            }
            Some(Json::Object(map))
        }
        _ => None,
    }
}

fn read_u32(bytes: &[u8], pos: &mut usize) -> Option<u32> {
    let arr: [u8; 4] = bytes.get(*pos..*pos + 4)?.try_into().ok()?;
    *pos += 4;
    Some(u32::from_be_bytes(arr))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ji(n: i64) -> Value { Value::int(n) }

    #[test]
    fn parse_primitives_materialize_as_native_values() {
        assert_eq!(parse("null").unwrap(), Value::Json(Json::Null));
        assert_eq!(parse("true").unwrap(), Value::Bool(true));
        assert_eq!(parse("false").unwrap(), Value::Bool(false));
        assert_eq!(parse("42").unwrap(), ji(42));
        assert_eq!(parse("-7").unwrap(), ji(-7));
        assert_eq!(parse("1.5").unwrap(), Value::Float(F64Bits(1.5)));
        assert_eq!(parse("\"hi\"").unwrap(), Value::Str("hi".into()));
    }

    #[test]
    fn parse_arbitrary_precision_ints() {
        let huge = "1606938044258990275541962092341162602522202993782792835301375";
        match parse(huge).unwrap() {
            Value::Int(n) => assert_eq!(n.to_string(), huge),
            other => panic!("expected Int, got {other:?}"),
        }
    }

    #[test]
    fn parse_array_and_object() {
        let v = parse("[1, 2, 3]").unwrap();
        match v {
            Value::Json(Json::Array(items)) => {
                assert_eq!(items, vec![ji(1), ji(2), ji(3)]);
            }
            other => panic!("expected Array, got {other:?}"),
        }
        let v = parse(r#"{"name": "alice", "age": 30}"#).unwrap();
        match v {
            Value::Json(Json::Object(m)) => {
                let pairs: Vec<_> = m.into_iter().collect();
                assert_eq!(pairs[0], ("name".to_string(), Value::Str("alice".into())));
                assert_eq!(pairs[1], ("age".to_string(), ji(30)));
            }
            other => panic!("expected Object, got {other:?}"),
        }
    }

    #[test]
    fn parse_nested() {
        let v = parse(r#"{"users": [{"id": 1, "name": "a"}]}"#).unwrap();
        let Value::Json(j) = v else { panic!() };
        let users = j.get_field("users").unwrap();
        let Value::Json(u) = users else { panic!() };
        let alice = u.get_index(0).unwrap();
        let Value::Json(alice) = alice else { panic!() };
        assert_eq!(alice.get_field("name"), Some(&Value::Str("a".into())));
    }

    #[test]
    fn canonical_distinguishes_int_and_float() {
        let i = parse("1").unwrap();
        let f = parse("1.0").unwrap();
        assert_ne!(i, f);
        match (&i, &f) {
            (Value::Int(_), Value::Float(_)) => {}
            _ => panic!("wrong variants"),
        }
    }

    #[test]
    fn missing_key_returns_none() {
        let v = parse(r#"{"a": 1}"#).unwrap();
        let Value::Json(j) = v else { panic!() };
        assert_eq!(j.get_field("nope"), None);
        assert_eq!(j.get_index(5), None);
    }
}
