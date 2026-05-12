//! Shared arithmetic and comparison kernels used by both the tree-walk
//! interpreter and the bytecode VM. Handles every integer type and produces
//! checked-overflow errors uniformly.

use crate::ast::{BinOp, UnOp};
use crate::error::{Error, ErrorKind};
use crate::token::Span;
use crate::value::Value;
use num_bigint::BigInt;
use num_traits::{Signed, ToPrimitive, Zero};

macro_rules! int_arith {
    ($op:expr, $a:expr, $b:expr, $variant:ident, $span:expr) => {{
        let overflow = || Error::new(ErrorKind::Runtime, "integer overflow", $span);
        let div_zero = || Error::new(ErrorKind::Runtime, "division by zero", $span);
        match $op {
            BinOp::Add => $a.checked_add(*$b).map(Value::$variant).ok_or_else(overflow),
            BinOp::Sub => $a.checked_sub(*$b).map(Value::$variant).ok_or_else(overflow),
            BinOp::Mul => $a.checked_mul(*$b).map(Value::$variant).ok_or_else(overflow),
            BinOp::Div => {
                if *$b == 0 { Err(div_zero()) }
                else { $a.checked_div(*$b).map(Value::$variant).ok_or_else(overflow) }
            }
            BinOp::Mod => {
                if *$b == 0 { Err(div_zero()) }
                else { $a.checked_rem(*$b).map(Value::$variant).ok_or_else(overflow) }
            }
            BinOp::Lt   => Ok(Value::Bool($a < $b)),
            BinOp::Gt   => Ok(Value::Bool($a > $b)),
            BinOp::LtEq => Ok(Value::Bool($a <= $b)),
            BinOp::GtEq => Ok(Value::Bool($a >= $b)),
            BinOp::Eq   => Ok(Value::Bool($a == $b)),
            BinOp::NotEq=> Ok(Value::Bool($a != $b)),
            BinOp::BitAnd => Ok(Value::$variant($a & $b)),
            BinOp::BitOr  => Ok(Value::$variant($a | $b)),
            BinOp::BitXor => Ok(Value::$variant($a ^ $b)),
            BinOp::Shl => {
                // Reject negative or wider-than-type shifts. Use
                // overflow-checked shift to fail loudly instead of
                // silently wrapping.
                let n: u32 = (*$b).try_into().map_err(|_| overflow())?;
                $a.checked_shl(n).map(Value::$variant).ok_or_else(overflow)
            }
            BinOp::Shr => {
                let n: u32 = (*$b).try_into().map_err(|_| overflow())?;
                $a.checked_shr(n).map(Value::$variant).ok_or_else(overflow)
            }
            other => Err(Error::new(
                ErrorKind::Runtime,
                format!("operator {other:?} not defined for {} values", stringify!($variant)),
                $span,
            )),
        }
    }};
}

/// Arbitrary-precision integer arithmetic for `Type::Int`. No
/// overflow is possible at this layer (the values are unbounded);
/// the only failure modes are division/modulo by zero and shifts by
/// values that don't fit in `u32`. Comparisons and bitwise ops use
/// `BigInt`'s built-in `Ord` / `BitAnd` / `BitOr` / `BitXor` impls.
fn bigint_arith(op: BinOp, a: &BigInt, b: &BigInt, span: Span) -> Result<Value, Error> {
    let div_zero = || Error::new(ErrorKind::Runtime, "division by zero", span);
    let bad_shift = || Error::new(ErrorKind::Runtime, "shift amount out of range", span);
    match op {
        BinOp::Add => Ok(Value::Int(a + b)),
        BinOp::Sub => Ok(Value::Int(a - b)),
        BinOp::Mul => Ok(Value::Int(a * b)),
        BinOp::Div => {
            if b.is_zero() { Err(div_zero()) } else { Ok(Value::Int(a / b)) }
        }
        BinOp::Mod => {
            if b.is_zero() { Err(div_zero()) } else { Ok(Value::Int(a % b)) }
        }
        BinOp::Lt    => Ok(Value::Bool(a < b)),
        BinOp::Gt    => Ok(Value::Bool(a > b)),
        BinOp::LtEq  => Ok(Value::Bool(a <= b)),
        BinOp::GtEq  => Ok(Value::Bool(a >= b)),
        BinOp::Eq    => Ok(Value::Bool(a == b)),
        BinOp::NotEq => Ok(Value::Bool(a != b)),
        BinOp::BitAnd => Ok(Value::Int(a & b)),
        BinOp::BitOr  => Ok(Value::Int(a | b)),
        BinOp::BitXor => Ok(Value::Int(a ^ b)),
        BinOp::Shl => {
            // BigInt's `<<` takes any usize-castable amount but
            // negative shifts are meaningless. Reject them loudly
            // so silent truncation never happens.
            if b.is_negative() { return Err(bad_shift()); }
            let n: u32 = b.to_u32().ok_or_else(bad_shift)?;
            Ok(Value::Int(a << n))
        }
        BinOp::Shr => {
            if b.is_negative() { return Err(bad_shift()); }
            let n: u32 = b.to_u32().ok_or_else(bad_shift)?;
            Ok(Value::Int(a >> n))
        }
        other => Err(Error::new(
            ErrorKind::Runtime,
            format!("operator {other:?} not defined for Int values"),
            span,
        )),
    }
}

pub fn eval_binary(op: BinOp, l: Value, r: Value, span: Span) -> Result<Value, Error> {
    use Value::*;
    let mismatch = || {
        Error::new(
            ErrorKind::Runtime,
            format!("type error: {l} {op:?} {r}"),
            span,
        )
    };

    match (&l, &r) {
        (Int(a),  Int(b))  => bigint_arith(op, a, b, span),
        (I32(a),  I32(b))  => int_arith!(op, a, b, I32,  span),
        (U32(a),  U32(b))  => int_arith!(op, a, b, U32,  span),
        (U64(a),  U64(b))  => int_arith!(op, a, b, U64,  span),
        (U128(a), U128(b)) => int_arith!(op, a, b, U128, span),
        (Bool(a), Bool(b)) => match op {
            BinOp::And   => Ok(Bool(*a && *b)),
            BinOp::Or    => Ok(Bool(*a || *b)),
            BinOp::Eq    => Ok(Bool(a == b)),
            BinOp::NotEq => Ok(Bool(a != b)),
            _ => Err(mismatch()),
        },
        // Equality on any same-typed pair (catches Str/Address/Array/Struct):
        _ if std::mem::discriminant(&l) == std::mem::discriminant(&r) => match op {
            BinOp::Eq    => Ok(Bool(l == r)),
            BinOp::NotEq => Ok(Bool(l != r)),
            _ => Err(mismatch()),
        },
        _ => Err(mismatch()),
    }
}

pub fn eval_unary(op: UnOp, v: Value, span: Span) -> Result<Value, Error> {
    let overflow = || Error::new(ErrorKind::Runtime, "integer overflow", span);
    match (op, v) {
        (UnOp::Neg, Value::Int(n)) => Ok(Value::Int(-n)),
        (UnOp::Neg, Value::I32(n)) => n.checked_neg().map(Value::I32).ok_or_else(overflow),
        (UnOp::Neg, Value::U32(0)) => Ok(Value::U32(0)),
        (UnOp::Neg, Value::U32(_)) => Err(overflow()),
        (UnOp::Neg, Value::U64(0)) => Ok(Value::U64(0)),
        (UnOp::Neg, Value::U64(_)) => Err(overflow()),
        (UnOp::Neg, Value::U128(0)) => Ok(Value::U128(0)),
        (UnOp::Neg, Value::U128(_)) => Err(overflow()),
        (UnOp::Not, Value::Bool(b)) => Ok(Value::Bool(!b)),
        (op, v) => Err(Error::new(
            ErrorKind::Runtime,
            format!("type error: {op:?} {v}"),
            span,
        )),
    }
}

/// Dispatch a polymorphic collection builtin by name. Used by the VM's
/// `BuiltinCall` instruction. The interpreter has its own copy of this
/// dispatch in `Interp::try_builtin`.
pub fn call_builtin(name: &str, args: &[Value]) -> Result<Value, Error> {
    let span = Span::default();
    let bad = |msg: String| Error::new(ErrorKind::Runtime, msg, span);
    match name {
        "set_insert" => match args {
            [Value::Set(s), v] => {
                let mut new_s = s.clone();
                if !new_s.iter().any(|e| e == v) { new_s.push(v.clone()); }
                Ok(Value::Set(new_s))
            }
            _ => Err(bad("set_insert(set, elem)".into())),
        },
        "set_remove" => match args {
            [Value::Set(s), v] => Ok(Value::Set(
                s.iter().filter(|e| *e != v).cloned().collect()
            )),
            _ => Err(bad("set_remove(set, elem)".into())),
        },
        "set_contains" => match args {
            [Value::Set(s), v] => Ok(Value::Bool(s.iter().any(|e| e == v))),
            _ => Err(bad("set_contains(set, elem)".into())),
        },
        "set_len" => match args {
            [Value::Set(s)] => Ok(Value::int(s.len())),
            _ => Err(bad("set_len(set)".into())),
        },
        "dict_set" => match args {
            [Value::Dict(d), k, v] => {
                let mut new_d = d.clone();
                if let Some(pair) = new_d.iter_mut().find(|(kk, _)| kk == k) {
                    pair.1 = v.clone();
                } else {
                    new_d.push((k.clone(), v.clone()));
                }
                Ok(Value::Dict(new_d))
            }
            _ => Err(bad("dict_set(dict, key, value)".into())),
        },
        "dict_remove" => match args {
            [Value::Dict(d), k] => Ok(Value::Dict(
                d.iter().filter(|(kk, _)| kk != k).cloned().collect()
            )),
            _ => Err(bad("dict_remove(dict, key)".into())),
        },
        "dict_get" => match args {
            [Value::Dict(d), k, default] => Ok(d
                .iter()
                .find(|(kk, _)| kk == k)
                .map(|(_, v)| v.clone())
                .unwrap_or_else(|| default.clone())),
            _ => Err(bad("dict_get(dict, key, default)".into())),
        },
        "dict_has" => match args {
            [Value::Dict(d), k] => Ok(Value::Bool(d.iter().any(|(kk, _)| kk == k))),
            _ => Err(bad("dict_has(dict, key)".into())),
        },
        "dict_len" => match args {
            [Value::Dict(d)] => Ok(Value::int(d.len())),
            _ => Err(bad("dict_len(dict)".into())),
        },
        "to_bytes" => match args {
            [Value::Str(s)] => Ok(Value::Bytes(s.as_bytes().to_vec())),
            _ => Err(bad("to_bytes(string)".into())),
        },
        "string_concat" => match args {
            [Value::Str(a), Value::Str(b)] => {
                let mut s = a.clone();
                s.push_str(b);
                Ok(Value::Str(s))
            }
            _ => Err(bad("string_concat(string, string)".into())),
        },
        "string_slice" => match args {
            // Byte indices, not char indices. Caller is responsible
            // for picking offsets that fall on char boundaries — we
            // reject UTF-8 splits with a runtime error.
            [Value::Str(s), Value::Int(start), Value::Int(end)] => {
                let s_bytes = s.as_bytes();
                let st = start.to_usize().ok_or_else(|| {
                    bad(format!("string_slice: start out of range: {start}"))
                })?;
                let en = end.to_usize().ok_or_else(|| {
                    bad(format!("string_slice: end out of range: {end}"))
                })?;
                if st > en || en > s_bytes.len() {
                    return Err(bad(format!(
                        "string_slice: out-of-range [{start}..{end}] over len {}",
                        s_bytes.len(),
                    )));
                }
                if !s.is_char_boundary(st) || !s.is_char_boundary(en) {
                    return Err(bad("string_slice: indices not on UTF-8 char boundary".into()));
                }
                Ok(Value::Str(s[st..en].to_string()))
            }
            _ => Err(bad("string_slice(string, start: i64, end: i64)".into())),
        },
        "string_contains" => match args {
            [Value::Str(haystack), Value::Str(needle)] => {
                Ok(Value::Bool(haystack.contains(needle.as_str())))
            }
            _ => Err(bad("string_contains(haystack, needle)".into())),
        },
        "bytes_len" => match args {
            [Value::Bytes(b)] => Ok(Value::int(b.len())),
            _ => Err(bad("bytes_len(bytes)".into())),
        },
        "bytes_concat" => match args {
            [Value::Bytes(a), Value::Bytes(b)] => {
                let mut out = a.clone();
                out.extend_from_slice(b);
                Ok(Value::Bytes(out))
            }
            _ => Err(bad("bytes_concat(bytes, bytes)".into())),
        },
        "bytes_eq" => match args {
            [Value::Bytes(a), Value::Bytes(b)] => Ok(Value::Bool(a == b)),
            _ => Err(bad("bytes_eq(bytes, bytes)".into())),
        },
        "bytes_slice" => match args {
            [Value::Bytes(b), Value::Int(start), Value::Int(end)] => {
                let s = start.to_usize().ok_or_else(|| {
                    bad(format!("bytes_slice: start out of range: {start}"))
                })?;
                let e = end.to_usize().ok_or_else(|| {
                    bad(format!("bytes_slice: end out of range: {end}"))
                })?;
                if s > e || e > b.len() {
                    return Err(bad(format!(
                        "bytes_slice: out-of-range [{start}..{end}] over len {}",
                        b.len(),
                    )));
                }
                Ok(Value::Bytes(b[s..e].to_vec()))
            }
            _ => Err(bad("bytes_slice(bytes, start: int, end: int)".into())),
        },
        "assert" => match args {
            [Value::Bool(true)] => Ok(Value::Unit),
            [Value::Bool(true), Value::Str(_)] => Ok(Value::Unit),
            [Value::Bool(false)] => Err(bad("assertion failed".into())),
            [Value::Bool(false), Value::Str(msg)] => {
                Err(bad(format!("assertion failed: {msg}")))
            }
            _ => Err(bad(
                "assert(cond: bool [, msg: string])".into(),
            )),
        },
        // Aggregations over arrays. The init/default's type fixes the
        // result type, so the typeck enforces array elements share it.
        // For sum, init is the starting accumulator (typically zero).
        // For max/min, default is returned only when the array is empty
        // — non-empty arrays always start their fold from element[0].
        "sum" => match args {
            [Value::Array(elems), init] => {
                let mut acc = init.clone();
                for e in elems {
                    acc = eval_binary(BinOp::Add, acc, e.clone(), span)?;
                }
                Ok(acc)
            }
            _ => Err(bad("sum(array, init)".into())),
        },
        "max" => match args {
            [Value::Array(elems), default] => {
                if elems.is_empty() {
                    return Ok(default.clone());
                }
                let mut acc = elems[0].clone();
                for e in &elems[1..] {
                    let cmp = eval_binary(BinOp::Gt, e.clone(), acc.clone(), span)?;
                    if matches!(cmp, Value::Bool(true)) {
                        acc = e.clone();
                    }
                }
                Ok(acc)
            }
            _ => Err(bad("max(array, default_if_empty)".into())),
        },
        // JSON builtins. `parse_json` is the parse entry point;
        // `json_get_field` / `json_get_index` navigate the parsed
        // structure (missing → Json::Null, no error); the
        // `json_to_*` / `json_is_null` family converts to typed
        // primitives or asserts shape (errors on mismatch — buyer
        // beware). `json_stringify` is the canonical text dump.
        "parse_json" => match args {
            [Value::Str(s)] => crate::json::parse(s)
                .map(Value::Json)
                .map_err(|e| bad(e.to_string())),
            _ => Err(bad("parse_json(string)".into())),
        },
        "json_stringify" => match args {
            [Value::Json(j)] => Ok(Value::Str(j.to_string_canonical())),
            _ => Err(bad("json_stringify(json)".into())),
        },
        "json_get_field" => match args {
            [Value::Json(j), Value::Str(k)] => {
                Ok(Value::Json(j.get_field(k).cloned().unwrap_or(crate::json::Json::Null)))
            }
            _ => Err(bad("json_get_field(json, string)".into())),
        },
        "json_get_index" => match args {
            [Value::Json(j), Value::Int(i)] => match i.to_usize() {
                Some(idx) => Ok(Value::Json(
                    j.get_index(idx).cloned().unwrap_or(crate::json::Json::Null))),
                None => Err(bad(format!("json_get_index: out of range: {i}"))),
            },
            [Value::Json(j), Value::U64(i)] => {
                Ok(Value::Json(j.get_index(*i as usize).cloned().unwrap_or(crate::json::Json::Null)))
            }
            _ => Err(bad("json_get_index(json, int|u64)".into())),
        },
        "json_to_string" => match args {
            [Value::Json(crate::json::Json::Str(s))] => Ok(Value::Str(s.clone())),
            [Value::Json(other)] => Err(bad(format!(
                "json_to_string: value is not a string: {other}",
            ))),
            _ => Err(bad("json_to_string(json)".into())),
        },
        "json_to_i64" => match args {
            // `json_to_i64` now returns a regular `int` (BigInt) — the
            // i64 name is historical, kept for source-compat. The
            // returned Value::Int carries the full BigInt; there is no
            // truncation. Sized-i64 conversion happens at the typed
            // builtin boundary if needed.
            [Value::Json(crate::json::Json::Int(n))] => Ok(Value::int(n.clone())),
            [Value::Json(other)] => Err(bad(format!(
                "json_to_i64: value is not an integer: {other}",
            ))),
            _ => Err(bad("json_to_i64(json)".into())),
        },
        "json_to_u64" => match args {
            [Value::Json(crate::json::Json::Int(n))] => {
                use num_traits::ToPrimitive;
                match n.to_u64() {
                    Some(v) => Ok(Value::U64(v)),
                    None => Err(bad(format!(
                        "json_to_u64: integer out of u64 range or negative: {n}",
                    ))),
                }
            }
            [Value::Json(other)] => Err(bad(format!(
                "json_to_u64: value is not a non-negative integer: {other}",
            ))),
            _ => Err(bad("json_to_u64(json)".into())),
        },
        "json_to_bool" => match args {
            [Value::Json(crate::json::Json::Bool(b))] => Ok(Value::Bool(*b)),
            [Value::Json(other)] => Err(bad(format!(
                "json_to_bool: value is not a bool: {other}",
            ))),
            _ => Err(bad("json_to_bool(json)".into())),
        },
        "json_is_null" => match args {
            [Value::Json(crate::json::Json::Null)] => Ok(Value::Bool(true)),
            [Value::Json(_)] => Ok(Value::Bool(false)),
            _ => Err(bad("json_is_null(json)".into())),
        },
        "min" => match args {
            [Value::Array(elems), default] => {
                if elems.is_empty() {
                    return Ok(default.clone());
                }
                let mut acc = elems[0].clone();
                for e in &elems[1..] {
                    let cmp = eval_binary(BinOp::Lt, e.clone(), acc.clone(), span)?;
                    if matches!(cmp, Value::Bool(true)) {
                        acc = e.clone();
                    }
                }
                Ok(acc)
            }
            _ => Err(bad("min(array, default_if_empty)".into())),
        },
        other => Err(bad(format!("unknown builtin '{other}'"))),
    }
}

/// Runtime numeric conversion used by `i32`/`u32`/`u64`/`u128`/`i64` builtins.
/// Bounds-checked; returns a runtime error on out-of-range input.
pub fn convert(value: &Value, target: u8, span: Span) -> Result<Value, Error> {
    let bounds = || Error::new(ErrorKind::Runtime, "integer out of range", span);
    let mismatch = || {
        Error::new(
            ErrorKind::Runtime,
            format!("can't convert {value} to int kind {target}"),
            span,
        )
    };

    fn from_i128(target: u8, n: i128, bounds: impl Fn() -> Error) -> Result<Value, Error> {
        match target {
            // target 0 → `int` (BigInt): widen from i128 — always succeeds.
            0 => Ok(Value::int(n)),
            1 => i32::try_from(n).map(Value::I32).map_err(|_| bounds()),
            2 => u32::try_from(n).map(Value::U32).map_err(|_| bounds()),
            3 => u64::try_from(n).map(Value::U64).map_err(|_| bounds()),
            4 => {
                if n < 0 { return Err(bounds()); }
                Ok(Value::U128(n as u128))
            }
            _ => Err(bounds()),
        }
    }

    // Convert through BigInt for the source value to keep `int` first-class.
    // For sized variants we widen to i128 (safe except for u128 > i128::MAX,
    // handled separately).
    if let Value::Int(n) = value {
        // BigInt → target. For target 0 (int), return the BigInt directly.
        if target == 0 { return Ok(Value::Int(n.clone())); }
        return match target {
            1 => n.to_i32().map(Value::I32).ok_or_else(bounds),
            2 => n.to_u32().map(Value::U32).ok_or_else(bounds),
            3 => n.to_u64().map(Value::U64).ok_or_else(bounds),
            4 => n.to_u128().map(Value::U128).ok_or_else(bounds),
            _ => Err(bounds()),
        };
    }
    let n: i128 = match value {
        Value::I32(n) => *n as i128,
        Value::U32(n) => *n as i128,
        Value::U64(n) => *n as i128,
        Value::U128(n) => {
            if target == 4 { return Ok(Value::U128(*n)); }
            if *n > i128::MAX as u128 { return Err(bounds()); }
            *n as i128
        }
        _ => return Err(mismatch()),
    };
    from_i128(target, n, bounds)
}
