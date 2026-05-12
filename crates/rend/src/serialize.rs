//! Canonical Value ↔ bytes serialization for KV storage and key composition.
//!
//! Format (all integers big-endian):
//!   tag:u8 | payload
//! Tags:
//!   0x01 Int      — 8 bytes i64
//!   0x02 Bool     — 1 byte (0 / 1)
//!   0x03 Unit     — empty
//!   0x04 Resource — 8 bytes i64
//!   0x05 String   — u32 length + UTF-8 bytes
//!   0x06 Address  — u32 length + UTF-8 bytes (distinct tag from String for type safety)
//!
//! The format is stable and self-describing; deserialization knows the
//! expected type from the call site (state declarations) and rejects
//! mismatched tags.

use crate::ast::Type;
use crate::value::Value;

const TAG_INT: u8 = 0x01;
const TAG_BOOL: u8 = 0x02;
const TAG_UNIT: u8 = 0x03;
const TAG_RESOURCE: u8 = 0x04;
const TAG_STRING: u8 = 0x05;
const TAG_ADDRESS: u8 = 0x06;
const TAG_ARRAY: u8 = 0x07;
const TAG_STRUCT: u8 = 0x08;
const TAG_I32: u8 = 0x10;
const TAG_U32: u8 = 0x11;
const TAG_U64: u8 = 0x12;
const TAG_U128: u8 = 0x13;
const TAG_BYTES: u8 = 0x14;
const TAG_ENUM: u8 = 0x15;
const TAG_PMAP: u8 = 0x16;
const TAG_PVEC: u8 = 0x17;
const TAG_INTERFACE: u8 = 0x18;
const TAG_PBTREE: u8 = 0x19;
const TAG_JSON: u8 = 0x1a;
const TAG_UINT: u8 = 0x1b;
const TAG_FLOAT: u8 = 0x1c;

pub fn serialize(value: &Value) -> Vec<u8> {
    match value {
        Value::Int(n) => {
            // BigInt has variable byte length. Format:
            //   TAG_INT | u32 byte_count | <signed BE bytes>
            // Empty payload is *not* zero — `BigInt::to_signed_bytes_be`
            // returns at least one byte for any value including zero,
            // so deserialization is unambiguous.
            let bytes = n.to_signed_bytes_be();
            let mut out = Vec::with_capacity(5 + bytes.len());
            out.push(TAG_INT);
            out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
            out.extend_from_slice(&bytes);
            out
        }
        Value::UInt(n) => {
            // Same shape as Int. The variant tag (TAG_UINT) lets the
            // deserializer route to `Type::UInt` so the value's
            // non-negative invariant is preserved across reads.
            let bytes = n.to_signed_bytes_be();
            let mut out = Vec::with_capacity(5 + bytes.len());
            out.push(TAG_UINT);
            out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
            out.extend_from_slice(&bytes);
            out
        }
        Value::Float(n) => {
            // 8 BE bytes IEEE-754 double. NaN bit-patterns are
            // preserved (`F64Bits` is bit-pattern equal so a stored
            // NaN reads back as the same NaN).
            let mut out = Vec::with_capacity(9);
            out.push(TAG_FLOAT);
            out.extend_from_slice(&n.to_f64().to_be_bytes());
            out
        }
        Value::I32(n) => {
            let mut out = Vec::with_capacity(5);
            out.push(TAG_I32);
            out.extend_from_slice(&n.to_be_bytes());
            out
        }
        Value::U32(n) => {
            let mut out = Vec::with_capacity(5);
            out.push(TAG_U32);
            out.extend_from_slice(&n.to_be_bytes());
            out
        }
        Value::U64(n) => {
            let mut out = Vec::with_capacity(9);
            out.push(TAG_U64);
            out.extend_from_slice(&n.to_be_bytes());
            out
        }
        Value::U128(n) => {
            let mut out = Vec::with_capacity(17);
            out.push(TAG_U128);
            out.extend_from_slice(&n.to_be_bytes());
            out
        }
        Value::Bool(b) => vec![TAG_BOOL, if *b { 1 } else { 0 }],
        Value::Unit => vec![TAG_UNIT],
        Value::Resource(n) => {
            let mut out = Vec::with_capacity(9);
            out.push(TAG_RESOURCE);
            out.extend_from_slice(&n.to_be_bytes());
            out
        }
        Value::Str(s) => {
            let bytes = s.as_bytes();
            let mut out = Vec::with_capacity(5 + bytes.len());
            out.push(TAG_STRING);
            out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
            out.extend_from_slice(bytes);
            out
        }
        Value::Address(s) => {
            let bytes = s.as_bytes();
            let mut out = Vec::with_capacity(5 + bytes.len());
            out.push(TAG_ADDRESS);
            out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
            out.extend_from_slice(bytes);
            out
        }
        Value::Bytes(b) => {
            let mut out = Vec::with_capacity(5 + b.len());
            out.push(TAG_BYTES);
            out.extend_from_slice(&(b.len() as u32).to_be_bytes());
            out.extend_from_slice(b);
            out
        }
        Value::Array(elems) => {
            let mut out = Vec::with_capacity(5 + elems.len() * 9);
            out.push(TAG_ARRAY);
            out.extend_from_slice(&(elems.len() as u32).to_be_bytes());
            for e in elems {
                out.extend_from_slice(&serialize(e));
            }
            out
        }
        Value::Set(_) | Value::Dict(_) => Vec::new(), // non-storage; never serialized
        Value::Pending(_) => panic!(
            "serialize() called on Value::Pending — caller must Tx::force first",
        ),
        Value::Tuple(_) => panic!(
            "serialize() called on Value::Tuple — tuples are local-only and never stored",
        ),
        Value::Enum { variant, payload, .. } => {
            // Wire format: TAG_ENUM, len(variant_name) u32, variant
            // bytes, payload values back-to-back. Variant name (not
            // index) keeps the wire format stable across reorderings
            // of the source-level enum decl — same property as
            // structs being keyed by field name.
            let name_bytes = variant.as_bytes();
            let mut out = Vec::with_capacity(5 + name_bytes.len() + 16 * payload.len());
            out.push(TAG_ENUM);
            out.extend_from_slice(&(name_bytes.len() as u32).to_be_bytes());
            out.extend_from_slice(name_bytes);
            for p in payload {
                out.extend_from_slice(&serialize(p));
            }
            out
        }
        Value::Struct { name: _, fields } => {
            // Struct identity is established by the consumer's expected type;
            // the wire format is just the fields in declaration order.
            let mut out = vec![TAG_STRUCT];
            for (_, v) in fields {
                out.extend_from_slice(&serialize(v));
            }
            out
        }
        Value::PMap(root_hash) => {
            // A pmap state cell stores just the root-node content
            // hash (16 bytes). The HAMT itself lives in separate
            // content-addressed cells managed by the `pmap` module.
            let mut out = Vec::with_capacity(17);
            out.push(TAG_PMAP);
            out.extend_from_slice(&root_hash.to_be_bytes());
            out
        }
        Value::PBTree(root_hash) => {
            // Same shape as PMap — the trie's root hash. Distinct
            // tag so the deserializer routes to the right type.
            let mut out = Vec::with_capacity(17);
            out.push(TAG_PBTREE);
            out.extend_from_slice(&root_hash.to_be_bytes());
            out
        }
        Value::Json(j) => {
            // Inline the parsed structure. Storage cost is the
            // jsonb-shaped binary form, not the original text.
            let inner = crate::json::serialize(j);
            let mut out = Vec::with_capacity(1 + inner.len());
            out.push(TAG_JSON);
            out.extend_from_slice(&inner);
            out
        }
        Value::PVec { len, root } => {
            // A pvec state cell carries the current length and the
            // root-node content hash. Tree nodes themselves live
            // in content-addressed cells managed by `pvec`.
            let mut out = Vec::with_capacity(1 + 8 + 16);
            out.push(TAG_PVEC);
            out.extend_from_slice(&len.to_be_bytes());
            out.extend_from_slice(&root.to_be_bytes());
            out
        }
        Value::Interface { iface, target_module } => {
            // Interface values are runtime module-name handles —
            // serialize as two length-prefixed strings (interface
            // name + bound module name) so a state cell holding an
            // interface round-trips cleanly.
            let ib = iface.as_bytes();
            let mb = target_module.as_bytes();
            let mut out = Vec::with_capacity(1 + 4 + ib.len() + 4 + mb.len());
            out.push(TAG_INTERFACE);
            out.extend_from_slice(&(ib.len() as u32).to_be_bytes());
            out.extend_from_slice(ib);
            out.extend_from_slice(&(mb.len() as u32).to_be_bytes());
            out.extend_from_slice(mb);
            out
        }
        Value::PMapCursor(_) => panic!(
            "internal: PMapCursor is a transient runtime-only value and \
             cannot be serialized — it must never reach a state cell",
        ),
        Value::PBTreeCursor(_) => panic!(
            "internal: PBTreeCursor is a transient runtime-only value and \
             cannot be serialized — it must never reach a state cell",
        ),
    }
}

pub fn deserialize(bytes: &[u8], expected: &Type) -> Option<Value> {
    // Strip the compile-time explicit-literal tag — the on-disk
    // encoding is independent of any `` `T` `` wrapping on the
    // expected type.
    if let Type::ExplicitLiteral(inner) = expected {
        return deserialize(bytes, inner);
    }
    let (tag, rest) = bytes.split_first()?;
    match (*tag, expected) {
        (TAG_INT, Type::Int) => {
            // Variable-length signed BigInt: u32 length prefix
            // then `length` signed BE bytes.
            if rest.len() < 4 { return None; }
            let n_bytes = u32::from_be_bytes(rest[..4].try_into().ok()?) as usize;
            if rest.len() != 4 + n_bytes { return None; }
            Some(Value::Int(num_bigint::BigInt::from_signed_bytes_be(&rest[4..])))
        }
        (TAG_UINT, Type::UInt) => {
            if rest.len() < 4 { return None; }
            let n_bytes = u32::from_be_bytes(rest[..4].try_into().ok()?) as usize;
            if rest.len() != 4 + n_bytes { return None; }
            Some(Value::UInt(num_bigint::BigInt::from_signed_bytes_be(&rest[4..])))
        }
        (TAG_FLOAT, Type::Float) => {
            let arr: [u8; 8] = rest.try_into().ok()?;
            Some(Value::Float(crate::value::F64Bits(f64::from_be_bytes(arr))))
        }
        (TAG_I32, Type::I32) => {
            let arr: [u8; 4] = rest.try_into().ok()?;
            Some(Value::I32(i32::from_be_bytes(arr)))
        }
        (TAG_U32, Type::U32) => {
            let arr: [u8; 4] = rest.try_into().ok()?;
            Some(Value::U32(u32::from_be_bytes(arr)))
        }
        (TAG_U64, Type::U64) => {
            let arr: [u8; 8] = rest.try_into().ok()?;
            Some(Value::U64(u64::from_be_bytes(arr)))
        }
        (TAG_U128, Type::U128) => {
            let arr: [u8; 16] = rest.try_into().ok()?;
            Some(Value::U128(u128::from_be_bytes(arr)))
        }
        (TAG_BOOL, Type::Bool) => match rest {
            [0] => Some(Value::Bool(false)),
            [1] => Some(Value::Bool(true)),
            _ => None,
        },
        (TAG_UNIT, Type::Unit) => Some(Value::Unit),
        (TAG_RESOURCE, Type::Resource) => {
            let arr: [u8; 8] = rest.try_into().ok()?;
            Some(Value::Resource(i64::from_be_bytes(arr)))
        }
        (TAG_STRING, Type::String) => {
            let len_bytes: [u8; 4] = rest.get(..4)?.try_into().ok()?;
            let len = u32::from_be_bytes(len_bytes) as usize;
            let body = rest.get(4..4 + len)?;
            std::str::from_utf8(body).ok().map(|s| Value::Str(s.to_string()))
        }
        (TAG_ADDRESS, Type::Address) => {
            let len_bytes: [u8; 4] = rest.get(..4)?.try_into().ok()?;
            let len = u32::from_be_bytes(len_bytes) as usize;
            let body = rest.get(4..4 + len)?;
            std::str::from_utf8(body).ok().map(|s| Value::Address(s.to_string()))
        }
        (TAG_BYTES, Type::Bytes) => {
            let len_bytes: [u8; 4] = rest.get(..4)?.try_into().ok()?;
            let len = u32::from_be_bytes(len_bytes) as usize;
            let body = rest.get(4..4 + len)?;
            Some(Value::Bytes(body.to_vec()))
        }
        (TAG_ENUM, Type::Enum { name, variants }) => {
            let name_len_bytes: [u8; 4] = rest.get(..4)?.try_into().ok()?;
            let name_len = u32::from_be_bytes(name_len_bytes) as usize;
            let name_bytes = rest.get(4..4 + name_len)?;
            let variant_name = std::str::from_utf8(name_bytes).ok()?.to_string();
            let payload_tys = variants
                .iter()
                .find(|(n, _)| n == &variant_name)
                .map(|(_, ts)| ts.clone())?;
            let mut cursor = 4 + name_len;
            let mut payload = Vec::with_capacity(payload_tys.len());
            for ty in &payload_tys {
                let remaining = rest.get(cursor..)?;
                let consumed = sized_value(remaining, ty)?;
                let v = deserialize(&remaining[..consumed], ty)?;
                payload.push(v);
                cursor += consumed;
            }
            Some(Value::Enum {
                enum_name: name.clone(),
                variant: variant_name,
                payload,
            })
        }
        (TAG_ARRAY, Type::Array(elem_ty)) => {
            let len_bytes: [u8; 4] = rest.get(..4)?.try_into().ok()?;
            let len = u32::from_be_bytes(len_bytes) as usize;
            let mut cursor = 4usize;
            let mut out = Vec::with_capacity(len);
            for _ in 0..len {
                let remaining = rest.get(cursor..)?;
                let consumed = sized_value(remaining, elem_ty)?;
                let v = deserialize(&remaining[..consumed], elem_ty)?;
                out.push(v);
                cursor += consumed;
            }
            Some(Value::Array(out))
        }
        (TAG_STRUCT, Type::Struct { name, fields, field_groups: _ })
        | (TAG_STRUCT, Type::Cap { name, fields, .. }) => {
            // Caps share the struct on-disk format. The Type tells the
            // deserializer which field layout to use; the Value coming
            // out is always Value::Struct (caps don't have their own
            // runtime variant — affine + construction rules live in
            // typeck, not in the serialized form).
            let mut cursor = 0usize;
            let mut out = Vec::with_capacity(fields.len());
            for (fname, fty) in fields {
                let remaining = rest.get(cursor..)?;
                let consumed = sized_value(remaining, fty)?;
                let v = deserialize(&remaining[..consumed], fty)?;
                out.push((fname.clone(), v));
                cursor += consumed;
            }
            Some(Value::Struct { name: name.clone(), fields: out })
        }
        (TAG_PMAP, Type::PMap { .. }) => {
            // Cell payload is just the root hash. Operations walk the
            // tree on demand via the pmap module + Tx.
            let arr: [u8; 16] = rest.try_into().ok()?;
            Some(Value::PMap(u128::from_be_bytes(arr)))
        }
        (TAG_PBTREE, Type::PBTree { .. }) => {
            let arr: [u8; 16] = rest.try_into().ok()?;
            Some(Value::PBTree(u128::from_be_bytes(arr)))
        }
        (TAG_JSON, Type::Json) => {
            crate::json::deserialize(rest).map(Value::Json)
        }
        // Json-typed cells are *polymorphic* — a `state x: json;` may
        // hold any rend primitive (parsed from a leaf JSON value)
        // OR a `Value::Json` composite. When the expected type is
        // `Type::Json` and the tag is a primitive's, dispatch the
        // primitive deserializer with its native type instead.
        (TAG_INT, Type::Json) => {
            deserialize(bytes, &Type::Int)
        }
        (TAG_UINT, Type::Json) => {
            deserialize(bytes, &Type::UInt)
        }
        (TAG_FLOAT, Type::Json) => {
            deserialize(bytes, &Type::Float)
        }
        (TAG_BOOL, Type::Json) => {
            deserialize(bytes, &Type::Bool)
        }
        (TAG_STRING, Type::Json) => {
            deserialize(bytes, &Type::String)
        }
        (TAG_PVEC, Type::PVec { .. }) => {
            let len_bytes: [u8; 8] = rest.get(..8)?.try_into().ok()?;
            let root_bytes: [u8; 16] = rest.get(8..24)?.try_into().ok()?;
            Some(Value::PVec {
                len: u64::from_be_bytes(len_bytes),
                root: u128::from_be_bytes(root_bytes),
            })
        }
        (TAG_INTERFACE, Type::Interface { .. }) => {
            let ilen = u32::from_be_bytes(rest.get(..4)?.try_into().ok()?) as usize;
            let iface = std::str::from_utf8(rest.get(4..4 + ilen)?).ok()?.to_string();
            let off = 4 + ilen;
            let mlen = u32::from_be_bytes(rest.get(off..off + 4)?.try_into().ok()?) as usize;
            let target_module = std::str::from_utf8(rest.get(off + 4..off + 4 + mlen)?).ok()?.to_string();
            Some(Value::Interface { iface, target_module })
        }
        _ => None,
    }
}

/// Public alias for `sized_value`. Used by `json::byte_size` to
/// step through a Json composite's polymorphic elements.
pub fn value_byte_size(bytes: &[u8], ty: &Type) -> Option<usize> {
    sized_value(bytes, ty)
}

/// Returns the byte length of the next encoded value of type `ty` at the
/// start of `bytes`, without fully parsing it. Used to walk array elements.
fn sized_value(bytes: &[u8], ty: &Type) -> Option<usize> {
    let tag = *bytes.first()?;
    match (tag, ty) {
        (TAG_INT, Type::Int) | (TAG_UINT, Type::UInt) => {
            // Variable-length BigInt: 1 tag byte + 4 length bytes +
            // N data bytes.
            let len_bytes: [u8; 4] = bytes.get(1..5)?.try_into().ok()?;
            let n = u32::from_be_bytes(len_bytes) as usize;
            Some(1 + 4 + n)
        }
        (TAG_FLOAT, Type::Float) => Some(1 + 8),
        (TAG_RESOURCE, Type::Resource) => Some(1 + 8),
        (TAG_I32, Type::I32) | (TAG_U32, Type::U32) => Some(1 + 4),
        (TAG_U64, Type::U64) => Some(1 + 8),
        (TAG_U128, Type::U128) => Some(1 + 16),
        (TAG_BOOL, Type::Bool) => Some(1 + 1),
        (TAG_UNIT, Type::Unit) => Some(1),
        (TAG_STRING, Type::String) | (TAG_ADDRESS, Type::Address)
        | (TAG_BYTES, Type::Bytes) => {
            let len_bytes: [u8; 4] = bytes.get(1..5)?.try_into().ok()?;
            let len = u32::from_be_bytes(len_bytes) as usize;
            Some(1 + 4 + len)
        }
        (TAG_ARRAY, Type::Array(elem_ty)) => {
            let len_bytes: [u8; 4] = bytes.get(1..5)?.try_into().ok()?;
            let n = u32::from_be_bytes(len_bytes) as usize;
            let mut total = 1 + 4;
            for _ in 0..n {
                let inner = sized_value(bytes.get(total..)?, elem_ty)?;
                total += inner;
            }
            Some(total)
        }
        (TAG_STRUCT, Type::Struct { fields, .. })
        | (TAG_STRUCT, Type::Cap { fields, .. }) => {
            let mut total = 1usize;
            for (_, fty) in fields {
                let inner = sized_value(bytes.get(total..)?, fty)?;
                total += inner;
            }
            Some(total)
        }
        (TAG_PMAP, Type::PMap { .. }) => Some(1 + 16),
        (TAG_PBTREE, Type::PBTree { .. }) => Some(1 + 16),
        (TAG_PVEC, Type::PVec { .. }) => Some(1 + 8 + 16),
        (TAG_JSON, Type::Json) => {
            // The inner json binary form has a tag-byte + variable
            // body. `crate::json::byte_size` walks it without
            // allocating to compute the total.
            let inner = crate::json::byte_size(bytes.get(1..)?)?;
            Some(1 + inner)
        }
        // Polymorphic json field shapes — when the field type is
        // `Type::Json` but the stored value is a primitive (parsed
        // from a leaf JSON), the tag is the primitive's. Recurse
        // with the primitive's native type to size correctly.
        (TAG_INT, Type::Json) => sized_value(bytes, &Type::Int),
        (TAG_UINT, Type::Json) => sized_value(bytes, &Type::UInt),
        (TAG_FLOAT, Type::Json) => sized_value(bytes, &Type::Float),
        (TAG_BOOL, Type::Json) => sized_value(bytes, &Type::Bool),
        (TAG_STRING, Type::Json) => sized_value(bytes, &Type::String),
        (TAG_INTERFACE, Type::Interface { .. }) => {
            let ilen = u32::from_be_bytes(bytes.get(1..5)?.try_into().ok()?) as usize;
            let off = 1 + 4 + ilen;
            let mlen = u32::from_be_bytes(bytes.get(off..off + 4)?.try_into().ok()?) as usize;
            Some(off + 4 + mlen)
        }
        (TAG_ENUM, Type::Enum { variants, .. }) => {
            let name_len_bytes: [u8; 4] = bytes.get(1..5)?.try_into().ok()?;
            let name_len = u32::from_be_bytes(name_len_bytes) as usize;
            let name_bytes = bytes.get(5..5 + name_len)?;
            let variant_name = std::str::from_utf8(name_bytes).ok()?;
            let payload_tys = variants
                .iter()
                .find(|(n, _)| n == variant_name)
                .map(|(_, ts)| ts)?;
            let mut total = 1 + 4 + name_len;
            for fty in payload_tys {
                let inner = sized_value(bytes.get(total..)?, fty)?;
                total += inner;
            }
            Some(total)
        }
        _ => None,
    }
}

/// Returns `true` if `value` is the canonical default for its type.
/// Used to make OCC validation tolerate unset cells.
pub fn is_default(value: &Value) -> bool {
    use num_traits::Zero;
    match value {
        Value::Int(n) if n.is_zero() => true,
        Value::UInt(n) if n.is_zero() => true,
        // Default-Float is +0.0. NaN, ±Inf, and -0.0 are NOT defaults.
        Value::Float(crate::value::F64Bits(n)) if *n == 0.0 && n.is_sign_positive() => true,
        Value::I32(0) => true,
        Value::U32(0) => true,
        Value::U64(0) => true,
        Value::U128(0) => true,
        Value::Bool(false) => true,
        Value::Unit => true,
        Value::Resource(0) => true,
        Value::Str(s) if s.is_empty() => true,
        Value::Address(s) if s.is_empty() => true,
        Value::Array(elems) if elems.is_empty() => true,
        Value::Bytes(b) if b.is_empty() => true,
        // An "empty" pmap is a zero root hash — no tree, no node
        // cells. Same for an empty pvec (length 0, root 0).
        Value::PMap(0) => true,
        Value::PVec { len: 0, root: 0 } => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(v: Value, ty: Type) {
        let bytes = serialize(&v);
        let back = deserialize(&bytes, &ty).expect("deserialize");
        assert_eq!(back, v);
    }

    #[test]
    fn round_trip_primitives() {
        round_trip(Value::int(42), Type::Int);
        round_trip(Value::int(-99), Type::Int);
        round_trip(Value::Bool(true), Type::Bool);
        round_trip(Value::Bool(false), Type::Bool);
        round_trip(Value::Unit, Type::Unit);
        round_trip(Value::Resource(7), Type::Resource);
    }

    #[test]
    fn round_trip_strings() {
        round_trip(Value::Str("hello".into()), Type::String);
        round_trip(Value::Str("".into()), Type::String);
        round_trip(Value::Str("héllo 🌍".into()), Type::String);
    }

    #[test]
    fn round_trip_addresses() {
        round_trip(Value::Address("0xfeed".into()), Type::Address);
    }

    #[test]
    fn type_mismatch_returns_none() {
        let bytes = serialize(&Value::int(7));
        assert!(deserialize(&bytes, &Type::Bool).is_none());
    }

    #[test]
    fn defaults_are_recognized() {
        assert!(is_default(&Value::int(0)));
        assert!(is_default(&Value::Bool(false)));
        assert!(is_default(&Value::Str("".into())));
        assert!(!is_default(&Value::int(1)));
        assert!(!is_default(&Value::Str("x".into())));
    }
}
