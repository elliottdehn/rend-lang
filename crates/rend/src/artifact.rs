//! Compiled binary artifact.
//!
//! An `Artifact` bundles one or more `BcModule`s — typically the
//! whole multi-source compilation of a project — into a single
//! self-describing byte blob suitable for persistence, transport,
//! and content-addressing.
//!
//! ## Shape
//!
//! ```text
//!   MAGIC[8] | VERSION[2] | n_modules[u32_be] | MODULE+
//! ```
//!
//! Each `MODULE` carries everything `BcModule` holds: name,
//! imports, state metadata, struct/enum/event/cap shapes, function
//! bytecode, path specs, etc. Encoding is deterministic — the same
//! source set produces identical bytes — so the content hash
//! doubles as a stable module ID.
//!
//! ## What lives inside vs. outside
//!
//! Inside the artifact:
//!   * Resolved cross-module imports (within this artifact). The
//!     compiler has already validated entry signatures and baked
//!     in indices.
//!   * State-cell keys (precomputed `state_root` hashes).
//!   * Default values for state slots.
//!   * Optimization output (read-batch groups) so consumers don't
//!     re-run the optimizer.
//!
//! Outside (resolved at execution time):
//!   * Host imports (the host `bind`s).
//!   * Imports from *other* artifacts — these are name strings
//!     today; cross-artifact linking is its own follow-up.

use std::collections::HashMap;

use crate::ast::{BinOp, Type, UnOp};
use crate::bc::{
    BcFn, BcModule, Const, EnumShape, Instr, PathSpec, ReadOp, StructShape,
};
use crate::error::{Error, ErrorKind};
use crate::token::Span;

const MAGIC: &[u8; 8] = b"REND-BC0";
const VERSION_MAJOR: u8 = 0;
/// Minor version bumps:
///   0.1 → initial format.
///   0.2 → BcFn carries `param_types` + `return_type` so cross-
///         artifact compile can typecheck `module::entry(args)`.
///   0.3 → BcFn carries `is_view` + `is_pure` so `Engine::query`
///         can recognize a tx as safe to run on the read-only
///         path without inspecting source.
///   0.4 → New Type::Interface variant + Instr::CallExternalDyn
///         for dynamic dispatch through interface values.
///   0.5 → BcModule carries `interfaces` so a tx compiled
///         against this artifact as a dep can reference its
///         interface decls by name.
///   0.6 → Instr::CallExternal carries `is_view` + `is_pure` so
///         the read-cluster optimizer doesn't fence on `view`/`pure`
///         cross-module calls. Mirrors the existing handling for
///         CallExternalDyn.
///   0.7 → Whole-collection walk instructions: PMapEntries,
///         PMapKeys, PMapValues, PVecToArray. Unlocks comprehensions
///         and aggregations over persistent state.
///   0.8 → PMapAppendUnique for multi-index auto-maintenance
///         (`index NAME on P.field` for non-unique fields).
///   0.9 → PMapPutUnique enforces the unique constraint at write
///         time: writing a different primary key under an existing
///         indexed value aborts the tx.
///   0.10 → PMapWalkInit / PMapWalkNext for streaming HAMT
///         iteration. `for x in pmap_state` no longer materializes
///         the whole map up front; cells are fetched lazily and
///         `break` exits without paying for unread subtrees.
///   0.11 → Type::PBTree + matching ops (PBTreeGet/Put/Contains/
///         Range/WalkInit/WalkNext). Sorted persistent collection
///         for ORDER BY / WHERE-BETWEEN query shapes.
///   0.12 → PBTreePutUnique + PBTreeAppendUnique. Lets `index` and
///         `unique_index` declarations point at a `pbtree<F, _>`
///         slot to get a sorted-index automatically.
///   0.13 → DELETE. PMapDelete / PBTreeDelete plus index back-link
///         cleanup ops (RemoveUnique / RemoveFromList for both
///         backends). pbtree gained a B+-tree-style remove with
///         tombstone propagation.
///   0.14 → Type::Struct carries `field_groups` — a parallel array
///         of `Option<String>` group names that controls storage
///         granularity. Ungrouped fields each get their own cell;
///         grouped fields share a cell named after the group.
/// Older readers can't decode newer formats.
const VERSION_MINOR: u8 = 14;

/// A compiled artifact: bytes + content hash + the materialized
/// `BcModule`s. Either `bytes` or `modules` is canonical depending
/// on the use case — bytes for persistence/transport, modules for
/// execution. They round-trip via `encode` / `decode`.
#[derive(Debug, Clone)]
pub struct Artifact {
    pub bytes: Vec<u8>,
    /// Content hash of `bytes`. Same compile input → same hash;
    /// suitable as a stable module-set identifier.
    pub content_hash: u128,
    pub modules: Vec<BcModule>,
}

impl Artifact {
    pub fn from_modules(modules: Vec<BcModule>) -> Self {
        let bytes = encode(&modules);
        let content_hash = crate::hashing::child(0, &bytes);
        Self { bytes, content_hash, modules }
    }

    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self, Error> {
        let modules = decode(&bytes)?;
        let content_hash = crate::hashing::child(0, &bytes);
        Ok(Self { bytes, content_hash, modules })
    }

    /// The name of the (unique) module carrying a `main` fn, if any.
    /// Deployed programs may omit `main`; txs must define one.
    /// Returns `Err` if multiple modules each define a `main` —
    /// that's ambiguous: `deploy` and `execute_tx` wouldn't know
    /// which to invoke.
    pub fn main_module(&self) -> Result<Option<&str>, Error> {
        let mut found: Option<&str> = None;
        for m in &self.modules {
            if !m.fn_index.contains_key("main") { continue; }
            if found.is_some() {
                return Err(Error::new(
                    ErrorKind::Type,
                    "artifact has more than one module with a `main` fn — ambiguous",
                    Span::default(),
                ));
            }
            found = Some(m.name.as_str());
        }
        Ok(found)
    }
}

// ---------- top-level encode / decode ----------------------------

pub fn encode(modules: &[BcModule]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + 2 + 4 + modules.len() * 1024);
    out.extend_from_slice(MAGIC);
    out.push(VERSION_MAJOR);
    out.push(VERSION_MINOR);
    write_u32(&mut out, modules.len() as u32);
    for m in modules {
        write_module(&mut out, m);
    }
    out
}

pub fn decode(bytes: &[u8]) -> Result<Vec<BcModule>, Error> {
    let mut r = Reader::new(bytes);
    let magic = r.take_bytes(8)?;
    if magic != MAGIC {
        return Err(decode_err("artifact magic mismatch — not a REND artifact"));
    }
    let major = r.read_u8()?;
    let minor = r.read_u8()?;
    if major != VERSION_MAJOR {
        return Err(decode_err(format!(
            "unsupported artifact major version {major}.{minor} (this build expects {VERSION_MAJOR}.x)",
        )));
    }
    let n = r.read_u32()? as usize;
    let mut modules = Vec::with_capacity(n);
    for _ in 0..n {
        modules.push(read_module(&mut r)?);
    }
    if r.pos != r.bytes.len() {
        return Err(decode_err(format!(
            "artifact has {} trailing bytes after the last module",
            r.bytes.len() - r.pos,
        )));
    }
    Ok(modules)
}

fn decode_err(msg: impl Into<String>) -> Error {
    Error::new(ErrorKind::Runtime, msg.into(), Span::default())
}

// ---------- module ----------------------------------------------

fn write_module(out: &mut Vec<u8>, m: &BcModule) {
    write_str(out, &m.name);
    write_u32(out, m.imports.len() as u32);
    for s in &m.imports { write_str(out, s); }
    write_u32(out, m.state_roots.len() as u32);
    for h in &m.state_roots { write_u128(out, *h); }
    write_u32(out, m.state_types.len() as u32);
    for t in &m.state_types { write_type(out, t); }
    write_u32(out, m.state_defaults.len() as u32);
    for v in &m.state_defaults {
        let bytes = crate::serialize::serialize(v);
        write_u32(out, bytes.len() as u32);
        out.extend_from_slice(&bytes);
    }
    write_u32(out, m.state_names.len() as u32);
    for s in &m.state_names { write_str(out, s); }
    write_u32(out, m.struct_shapes.len() as u32);
    for s in &m.struct_shapes {
        write_str(out, &s.name);
        write_u32(out, s.field_names.len() as u32);
        for n in &s.field_names { write_str(out, n); }
    }
    write_u32(out, m.functions.len() as u32);
    for f in &m.functions { write_bcfn(out, f); }
    write_u32(out, m.path_specs.len() as u32);
    for p in &m.path_specs {
        write_u128(out, p.leaf_key);
        write_type(out, &p.leaf_type);
        out.push(if p.is_blob { 1 } else { 0 });
    }
    write_u32(out, m.enum_shapes.len() as u32);
    for e in &m.enum_shapes {
        write_str(out, &e.name);
        write_u32(out, e.variant_names.len() as u32);
        for v in &e.variant_names { write_str(out, v); }
    }
    write_u32(out, m.interfaces.len() as u32);
    for iface in &m.interfaces {
        write_str(out, &iface.name);
        write_u32(out, iface.methods.len() as u32);
        for meth in &iface.methods {
            write_str(out, &meth.name);
            out.push(if meth.is_view { 1 } else { 0 });
            out.push(if meth.is_pure { 1 } else { 0 });
            write_u32(out, meth.params.len() as u32);
            for p in &meth.params {
                write_str(out, &p.name);
                write_type(out, &p.ty);
            }
            write_type(out, &meth.return_type);
        }
    }
}

fn read_module(r: &mut Reader) -> Result<BcModule, Error> {
    let name = read_str(r)?;
    let n_imports = r.read_u32()? as usize;
    let mut imports = Vec::with_capacity(n_imports);
    for _ in 0..n_imports { imports.push(read_str(r)?); }
    let mut import_index = HashMap::with_capacity(n_imports);
    for (i, s) in imports.iter().enumerate() {
        import_index.insert(s.clone(), i);
    }
    let n_states = r.read_u32()? as usize;
    let mut state_roots = Vec::with_capacity(n_states);
    for _ in 0..n_states { state_roots.push(r.read_u128()?); }
    let n_types = r.read_u32()? as usize;
    let mut state_types = Vec::with_capacity(n_types);
    for _ in 0..n_types { state_types.push(read_type(r)?); }
    let n_defs = r.read_u32()? as usize;
    let mut state_defaults = Vec::with_capacity(n_defs);
    for _ in 0..n_defs {
        let len = r.read_u32()? as usize;
        let bytes = r.take_bytes(len)?;
        let ty = state_types.get(state_defaults.len()).cloned().unwrap_or(Type::Unit);
        let v = crate::serialize::deserialize(bytes, &ty).ok_or_else(|| {
            decode_err("could not deserialize state default value")
        })?;
        state_defaults.push(v);
    }
    let n_names = r.read_u32()? as usize;
    let mut state_names = Vec::with_capacity(n_names);
    for _ in 0..n_names { state_names.push(read_str(r)?); }
    let mut state_index = HashMap::with_capacity(n_names);
    for (i, s) in state_names.iter().enumerate() {
        state_index.insert(s.clone(), i);
    }
    let n_structs = r.read_u32()? as usize;
    let mut struct_shapes = Vec::with_capacity(n_structs);
    let mut struct_index = HashMap::with_capacity(n_structs);
    for i in 0..n_structs {
        let name = read_str(r)?;
        let nf = r.read_u32()? as usize;
        let mut fields = Vec::with_capacity(nf);
        for _ in 0..nf { fields.push(read_str(r)?); }
        struct_index.insert(name.clone(), i);
        struct_shapes.push(StructShape { name, field_names: fields });
    }
    let n_fns = r.read_u32()? as usize;
    let mut functions = Vec::with_capacity(n_fns);
    let mut fn_index = HashMap::with_capacity(n_fns);
    for i in 0..n_fns {
        let f = read_bcfn(r)?;
        fn_index.insert(f.name.clone(), i);
        functions.push(f);
    }
    let n_paths = r.read_u32()? as usize;
    let mut path_specs = Vec::with_capacity(n_paths);
    for _ in 0..n_paths {
        path_specs.push(PathSpec {
            leaf_key: r.read_u128()?,
            leaf_type: read_type(r)?,
            is_blob: r.read_u8()? != 0,
        });
    }
    let n_enums = r.read_u32()? as usize;
    let mut enum_shapes = Vec::with_capacity(n_enums);
    let mut enum_index = HashMap::with_capacity(n_enums);
    for i in 0..n_enums {
        let name = read_str(r)?;
        let nv = r.read_u32()? as usize;
        let mut variants = Vec::with_capacity(nv);
        for _ in 0..nv { variants.push(read_str(r)?); }
        enum_index.insert(name.clone(), i);
        enum_shapes.push(EnumShape { name, variant_names: variants });
    }
    let n_ifaces = r.read_u32()? as usize;
    let mut interfaces = Vec::with_capacity(n_ifaces);
    let mut interface_index = HashMap::with_capacity(n_ifaces);
    for i in 0..n_ifaces {
        let name = read_str(r)?;
        let nm = r.read_u32()? as usize;
        let mut methods = Vec::with_capacity(nm);
        for _ in 0..nm {
            let mname = read_str(r)?;
            let is_view = r.read_u8()? != 0;
            let is_pure = r.read_u8()? != 0;
            let np = r.read_u32()? as usize;
            let mut params = Vec::with_capacity(np);
            for _ in 0..np {
                let pname = read_str(r)?;
                let pty = read_type(r)?;
                params.push(crate::ast::Param {
                    name: pname,
                    ty: pty,
                    span: crate::token::Span::default(),
                });
            }
            let return_type = read_type(r)?;
            methods.push(crate::ast::InterfaceMethod {
                name: mname, params, return_type, is_view, is_pure,
                span: crate::token::Span::default(),
            });
        }
        interface_index.insert(name.clone(), i);
        interfaces.push(crate::ast::InterfaceDecl {
            name, methods, span: crate::token::Span::default(),
        });
    }
    Ok(BcModule {
        name, imports, import_index,
        state_roots, state_types, state_defaults, state_names, state_index,
        struct_shapes, struct_index,
        functions, fn_index,
        path_specs,
        enum_shapes, enum_index,
        interfaces, interface_index,
    })
}

// ---------- BcFn ------------------------------------------------

fn write_bcfn(out: &mut Vec<u8>, f: &BcFn) {
    write_str(out, &f.name);
    write_u16(out, f.n_params);
    write_u16(out, f.n_regs);
    out.push(if f.is_entry { 1 } else { 0 });
    out.push(if f.is_nore { 1 } else { 0 });
    out.push(if f.is_view { 1 } else { 0 });
    out.push(if f.is_pure { 1 } else { 0 });
    // Param types + return type — used by cross-artifact compile
    // for typechecking `module::entry(args)` against a deployed
    // program's signature. Runtime ignores them.
    write_u32(out, f.param_types.len() as u32);
    for t in &f.param_types { write_type(out, t); }
    write_type(out, &f.return_type);
    write_u32(out, f.consts.len() as u32);
    for c in &f.consts { write_const(out, c); }
    write_u32(out, f.code.len() as u32);
    for i in &f.code { write_instr(out, i); }
    write_u32(out, f.read_groups.len() as u32);
    for g in &f.read_groups {
        write_u32(out, g.len() as u32);
        for op in g { write_read_op(out, op); }
    }
}

fn read_bcfn(r: &mut Reader) -> Result<BcFn, Error> {
    let name = read_str(r)?;
    let n_params = r.read_u16()?;
    let n_regs = r.read_u16()?;
    let is_entry = r.read_u8()? != 0;
    let is_nore = r.read_u8()? != 0;
    let is_view = r.read_u8()? != 0;
    let is_pure = r.read_u8()? != 0;
    let n_param_types = r.read_u32()? as usize;
    let mut param_types = Vec::with_capacity(n_param_types);
    for _ in 0..n_param_types { param_types.push(read_type(r)?); }
    let return_type = read_type(r)?;
    let n_consts = r.read_u32()? as usize;
    let mut consts = Vec::with_capacity(n_consts);
    for _ in 0..n_consts { consts.push(read_const(r)?); }
    let n_code = r.read_u32()? as usize;
    let mut code = Vec::with_capacity(n_code);
    for _ in 0..n_code { code.push(read_instr(r)?); }
    let n_groups = r.read_u32()? as usize;
    let mut read_groups = Vec::with_capacity(n_groups);
    for _ in 0..n_groups {
        let g_len = r.read_u32()? as usize;
        let mut g = Vec::with_capacity(g_len);
        for _ in 0..g_len { g.push(read_read_op(r)?); }
        read_groups.push(g);
    }
    Ok(BcFn {
        name, n_params, n_regs, code, consts,
        is_entry, is_nore, read_groups,
        param_types, return_type,
        is_view, is_pure,
    })
}

// ---------- Const -----------------------------------------------

mod const_tag {
    pub const INT: u8 = 0x01;
    pub const I32: u8 = 0x02;
    pub const U32: u8 = 0x03;
    pub const U64: u8 = 0x04;
    pub const U128: u8 = 0x05;
    pub const BOOL: u8 = 0x06;
    pub const STR: u8 = 0x07;
    pub const UINT: u8 = 0x08;
    pub const FLOAT: u8 = 0x09;
}

fn write_const(out: &mut Vec<u8>, c: &Const) {
    match c {
        Const::Int(n)  => {
            out.push(const_tag::INT);
            let bytes = n.to_signed_bytes_be();
            write_u32(out, bytes.len() as u32);
            out.extend_from_slice(&bytes);
        }
        Const::UInt(n) => {
            out.push(const_tag::UINT);
            // Always non-negative — `to_signed_bytes_be` will give us
            // a leading zero byte for the high-bit case, which is fine
            // and round-trips correctly.
            let bytes = n.to_signed_bytes_be();
            write_u32(out, bytes.len() as u32);
            out.extend_from_slice(&bytes);
        }
        Const::Float(n) => {
            out.push(const_tag::FLOAT);
            out.extend_from_slice(&n.to_f64().to_be_bytes());
        }
        Const::I32(n)  => { out.push(const_tag::I32);  write_i32(out, *n); }
        Const::U32(n)  => { out.push(const_tag::U32);  write_u32(out, *n); }
        Const::U64(n)  => { out.push(const_tag::U64);  write_u64(out, *n); }
        Const::U128(n) => { out.push(const_tag::U128); write_u128(out, *n); }
        Const::Bool(b) => { out.push(const_tag::BOOL); out.push(if *b {1} else {0}); }
        Const::Str(s)  => { out.push(const_tag::STR);  write_str(out, s); }
    }
}

fn read_const(r: &mut Reader) -> Result<Const, Error> {
    let tag = r.read_u8()?;
    Ok(match tag {
        const_tag::INT  => {
            let n = r.read_u32()? as usize;
            let bytes = r.take_bytes(n)?.to_vec();
            Const::Int(num_bigint::BigInt::from_signed_bytes_be(&bytes))
        }
        const_tag::UINT => {
            let n = r.read_u32()? as usize;
            let bytes = r.take_bytes(n)?.to_vec();
            Const::UInt(num_bigint::BigInt::from_signed_bytes_be(&bytes))
        }
        const_tag::FLOAT => {
            let arr: [u8; 8] = r.take_bytes(8)?.try_into().unwrap();
            Const::Float(crate::value::F64Bits(f64::from_be_bytes(arr)))
        }
        const_tag::I32  => Const::I32(r.read_i32()?),
        const_tag::U32  => Const::U32(r.read_u32()?),
        const_tag::U64  => Const::U64(r.read_u64()?),
        const_tag::U128 => Const::U128(r.read_u128()?),
        const_tag::BOOL => Const::Bool(r.read_u8()? != 0),
        const_tag::STR  => Const::Str(read_str(r)?),
        other => return Err(decode_err(format!("unknown const tag 0x{other:02x}"))),
    })
}

// ---------- ReadOp ----------------------------------------------

fn write_read_op(out: &mut Vec<u8>, op: &ReadOp) {
    match op {
        ReadOp::State { dst, key_idx } => {
            out.push(0x01);
            write_u16(out, *dst);
            write_u16(out, *key_idx);
        }
        ReadOp::Path { dst, path_idx } => {
            out.push(0x02);
            write_u16(out, *dst);
            write_u16(out, *path_idx);
        }
    }
}

fn read_read_op(r: &mut Reader) -> Result<ReadOp, Error> {
    let tag = r.read_u8()?;
    Ok(match tag {
        0x01 => ReadOp::State { dst: r.read_u16()?, key_idx: r.read_u16()? },
        0x02 => ReadOp::Path  { dst: r.read_u16()?, path_idx: r.read_u16()? },
        other => return Err(decode_err(format!("unknown read-op tag 0x{other:02x}"))),
    })
}

// ---------- Type (recursive) ------------------------------------

mod ty_tag {
    pub const INT: u8 = 0x01;
    pub const I32: u8 = 0x02;
    pub const U32: u8 = 0x03;
    pub const U64: u8 = 0x04;
    pub const U128: u8 = 0x05;
    pub const BOOL: u8 = 0x06;
    pub const UNIT: u8 = 0x07;
    pub const RESOURCE: u8 = 0x08;
    pub const STRING: u8 = 0x09;
    pub const ADDRESS: u8 = 0x0a;
    pub const BYTES: u8 = 0x0b;
    pub const ARRAY: u8 = 0x10;
    pub const SET: u8 = 0x11;
    pub const DICT: u8 = 0x12;
    pub const MAP: u8 = 0x13;
    pub const STRUCT: u8 = 0x14;
    pub const TUPLE: u8 = 0x15;
    pub const ENUM: u8 = 0x16;
    pub const PMAP: u8 = 0x17;
    pub const PVEC: u8 = 0x18;
    pub const CAP: u8 = 0x19;
    pub const INTERFACE: u8 = 0x1a;
    pub const PBTREE: u8 = 0x1b;
    pub const JSON: u8 = 0x1c;
    pub const UINT: u8 = 0x1d;
    pub const FLOAT: u8 = 0x1e;
}

fn write_type(out: &mut Vec<u8>, ty: &Type) {
    match ty {
        Type::Int => out.push(ty_tag::INT),
        Type::UInt => out.push(ty_tag::UINT),
        Type::Float => out.push(ty_tag::FLOAT),
        Type::I32 => out.push(ty_tag::I32),
        Type::U32 => out.push(ty_tag::U32),
        Type::U64 => out.push(ty_tag::U64),
        Type::U128 => out.push(ty_tag::U128),
        Type::Bool => out.push(ty_tag::BOOL),
        Type::Unit => out.push(ty_tag::UNIT),
        Type::Resource => out.push(ty_tag::RESOURCE),
        Type::String => out.push(ty_tag::STRING),
        Type::Address => out.push(ty_tag::ADDRESS),
        Type::Bytes => out.push(ty_tag::BYTES),
        Type::Array(elem) => { out.push(ty_tag::ARRAY); write_type(out, elem); }
        Type::Set(elem)   => { out.push(ty_tag::SET);   write_type(out, elem); }
        Type::Dict { key, value } => {
            out.push(ty_tag::DICT); write_type(out, key); write_type(out, value);
        }
        Type::Map { key, value } => {
            out.push(ty_tag::MAP); write_type(out, key); write_type(out, value);
        }
        Type::PMap { key, value } => {
            out.push(ty_tag::PMAP); write_type(out, key); write_type(out, value);
        }
        Type::PBTree { key, value } => {
            out.push(ty_tag::PBTREE); write_type(out, key); write_type(out, value);
        }
        Type::Json => out.push(ty_tag::JSON),
        Type::PVec { elem } => {
            out.push(ty_tag::PVEC); write_type(out, elem);
        }
        Type::Struct { name, fields, field_groups } => {
            out.push(ty_tag::STRUCT);
            write_str(out, name);
            write_u32(out, fields.len() as u32);
            // Per-field: (name, type, group?) where group is encoded
            // as a 1-byte presence flag followed by the group name
            // when present. `field_groups` may be empty (uniform
            // ungrouped); treat that as all-`None`.
            for (i, (n, t)) in fields.iter().enumerate() {
                write_str(out, n);
                write_type(out, t);
                match field_groups.get(i).and_then(|g| g.as_ref()) {
                    Some(g) => { out.push(1); write_str(out, g); }
                    None => out.push(0),
                }
            }
        }
        Type::Tuple(elems) => {
            out.push(ty_tag::TUPLE);
            write_u32(out, elems.len() as u32);
            for e in elems { write_type(out, e); }
        }
        Type::Enum { name, variants } => {
            out.push(ty_tag::ENUM);
            write_str(out, name);
            write_u32(out, variants.len() as u32);
            for (vn, payload) in variants {
                write_str(out, vn);
                write_u32(out, payload.len() as u32);
                for p in payload { write_type(out, p); }
            }
        }
        Type::Cap { name, fields, owner_module } => {
            out.push(ty_tag::CAP);
            write_str(out, name);
            write_str(out, owner_module);
            write_u32(out, fields.len() as u32);
            for (n, t) in fields {
                write_str(out, n);
                write_type(out, t);
            }
        }
        Type::Interface { name, methods } => {
            out.push(ty_tag::INTERFACE);
            write_str(out, name);
            write_u32(out, methods.len() as u32);
            for m in methods {
                write_str(out, &m.name);
                out.push(if m.is_view { 1 } else { 0 });
                out.push(if m.is_pure { 1 } else { 0 });
                write_u32(out, m.params.len() as u32);
                for p in &m.params { write_type(out, p); }
                write_type(out, &m.return_type);
            }
        }
    }
}

fn read_type(r: &mut Reader) -> Result<Type, Error> {
    let tag = r.read_u8()?;
    Ok(match tag {
        ty_tag::INT => Type::Int,
        ty_tag::UINT => Type::UInt,
        ty_tag::FLOAT => Type::Float,
        ty_tag::I32 => Type::I32,
        ty_tag::U32 => Type::U32,
        ty_tag::U64 => Type::U64,
        ty_tag::U128 => Type::U128,
        ty_tag::BOOL => Type::Bool,
        ty_tag::UNIT => Type::Unit,
        ty_tag::RESOURCE => Type::Resource,
        ty_tag::STRING => Type::String,
        ty_tag::ADDRESS => Type::Address,
        ty_tag::BYTES => Type::Bytes,
        ty_tag::ARRAY => Type::Array(Box::new(read_type(r)?)),
        ty_tag::SET => Type::Set(Box::new(read_type(r)?)),
        ty_tag::DICT => Type::Dict { key: Box::new(read_type(r)?), value: Box::new(read_type(r)?) },
        ty_tag::MAP  => Type::Map  { key: Box::new(read_type(r)?), value: Box::new(read_type(r)?) },
        ty_tag::PMAP => Type::PMap { key: Box::new(read_type(r)?), value: Box::new(read_type(r)?) },
        ty_tag::PBTREE => Type::PBTree { key: Box::new(read_type(r)?), value: Box::new(read_type(r)?) },
        ty_tag::JSON => Type::Json,
        ty_tag::PVEC => Type::PVec { elem: Box::new(read_type(r)?) },
        ty_tag::STRUCT => {
            let name = read_str(r)?;
            let n = r.read_u32()? as usize;
            let mut fields = Vec::with_capacity(n);
            let mut groups = Vec::with_capacity(n);
            for _ in 0..n {
                let fname = read_str(r)?;
                let fty = read_type(r)?;
                let has_group = r.read_u8()?;
                let group = if has_group != 0 { Some(read_str(r)?) } else { None };
                fields.push((fname, fty));
                groups.push(group);
            }
            // Normalize all-`None` to empty to match the parser's
            // default for ungrouped structs (keeps Eq stable across
            // round-trips).
            let field_groups = if groups.iter().all(Option::is_none) {
                Vec::new()
            } else {
                groups
            };
            Type::Struct { name, fields, field_groups }
        }
        ty_tag::TUPLE => {
            let n = r.read_u32()? as usize;
            let mut elems = Vec::with_capacity(n);
            for _ in 0..n { elems.push(read_type(r)?); }
            Type::Tuple(elems)
        }
        ty_tag::ENUM => {
            let name = read_str(r)?;
            let n = r.read_u32()? as usize;
            let mut variants = Vec::with_capacity(n);
            for _ in 0..n {
                let vn = read_str(r)?;
                let np = r.read_u32()? as usize;
                let mut payload = Vec::with_capacity(np);
                for _ in 0..np { payload.push(read_type(r)?); }
                variants.push((vn, payload));
            }
            Type::Enum { name, variants }
        }
        ty_tag::CAP => {
            let name = read_str(r)?;
            let owner_module = read_str(r)?;
            let n = r.read_u32()? as usize;
            let mut fields = Vec::with_capacity(n);
            for _ in 0..n {
                let fname = read_str(r)?;
                let fty = read_type(r)?;
                fields.push((fname, fty));
            }
            Type::Cap { name, fields, owner_module }
        }
        ty_tag::INTERFACE => {
            let name = read_str(r)?;
            let n = r.read_u32()? as usize;
            let mut methods = Vec::with_capacity(n);
            for _ in 0..n {
                let mname = read_str(r)?;
                let is_view = r.read_u8()? != 0;
                let is_pure = r.read_u8()? != 0;
                let np = r.read_u32()? as usize;
                let mut params = Vec::with_capacity(np);
                for _ in 0..np { params.push(read_type(r)?); }
                let return_type = read_type(r)?;
                methods.push(crate::ast::InterfaceMethodSig {
                    name: mname, params, return_type, is_view, is_pure,
                });
            }
            Type::Interface { name, methods }
        }
        other => return Err(decode_err(format!("unknown type tag 0x{other:02x}"))),
    })
}

// ---------- BinOp / UnOp ----------------------------------------

fn binop_to_byte(op: BinOp) -> u8 {
    match op {
        BinOp::Add => 0x01, BinOp::Sub => 0x02, BinOp::Mul => 0x03, BinOp::Div => 0x04, BinOp::Mod => 0x05,
        BinOp::Eq  => 0x06, BinOp::NotEq => 0x07, BinOp::Lt => 0x08, BinOp::Gt => 0x09,
        BinOp::LtEq => 0x0a, BinOp::GtEq => 0x0b,
        BinOp::And => 0x0c, BinOp::Or => 0x0d,
        BinOp::BitAnd => 0x0e, BinOp::BitOr => 0x0f, BinOp::BitXor => 0x10,
        BinOp::Shl => 0x11, BinOp::Shr => 0x12,
    }
}

fn binop_from_byte(b: u8) -> Result<BinOp, Error> {
    Ok(match b {
        0x01 => BinOp::Add, 0x02 => BinOp::Sub, 0x03 => BinOp::Mul, 0x04 => BinOp::Div, 0x05 => BinOp::Mod,
        0x06 => BinOp::Eq,  0x07 => BinOp::NotEq, 0x08 => BinOp::Lt, 0x09 => BinOp::Gt,
        0x0a => BinOp::LtEq, 0x0b => BinOp::GtEq,
        0x0c => BinOp::And, 0x0d => BinOp::Or,
        0x0e => BinOp::BitAnd, 0x0f => BinOp::BitOr, 0x10 => BinOp::BitXor,
        0x11 => BinOp::Shl, 0x12 => BinOp::Shr,
        other => return Err(decode_err(format!("unknown binop byte 0x{other:02x}"))),
    })
}

fn unop_to_byte(op: UnOp) -> u8 {
    match op { UnOp::Neg => 0x01, UnOp::Not => 0x02 }
}

fn unop_from_byte(b: u8) -> Result<UnOp, Error> {
    Ok(match b {
        0x01 => UnOp::Neg,
        0x02 => UnOp::Not,
        other => return Err(decode_err(format!("unknown unop byte 0x{other:02x}"))),
    })
}

// ---------- Instr (the big one) ---------------------------------

mod op {
    pub const LOAD_CONST: u8       = 0x01;
    pub const MOVE: u8             = 0x02;
    pub const BIN: u8              = 0x03;
    pub const UN: u8               = 0x04;
    pub const JUMP: u8             = 0x05;
    pub const JUMP_IF_FALSE: u8    = 0x06;
    pub const CALL: u8             = 0x07;
    pub const CALL_HOST: u8        = 0x08;
    pub const CALL_EXTERNAL: u8    = 0x09;
    pub const RETURN: u8           = 0x0a;
    pub const RETURN_UNIT: u8      = 0x0b;
    pub const BUILTIN_RESOURCE: u8 = 0x0c;
    pub const BUILTIN_UNWRAP: u8   = 0x0d;
    pub const BUILTIN_ADDRESS: u8  = 0x0e;
    pub const BUILTIN_LEN: u8      = 0x0f;
    pub const CONVERT: u8          = 0x10;
    pub const MAKE_ARRAY: u8       = 0x11;
    pub const ARRAY_GET: u8        = 0x12;
    pub const ARRAY_APPEND: u8     = 0x13;
    pub const MAKE_SET: u8         = 0x14;
    pub const MAKE_DICT: u8        = 0x15;
    pub const BUILTIN_CALL: u8     = 0x16;
    pub const MAKE_STRUCT: u8      = 0x17;
    pub const FIELD_GET: u8        = 0x18;
    pub const FIELD_SET: u8        = 0x19;
    pub const KV_GET: u8           = 0x1a;
    pub const KV_PUT: u8           = 0x1b;
    pub const KV_GET_PATH: u8      = 0x1c;
    pub const KV_PUT_PATH: u8      = 0x1d;
    pub const READ_BATCH: u8       = 0x1e;
    pub const PREFETCH_MAP: u8     = 0x1f;
    pub const EMIT: u8              = 0x20;
    pub const CONTEXT: u8           = 0x21;
    pub const MAKE_TUPLE: u8        = 0x22;
    pub const TUPLE_GET: u8         = 0x23;
    pub const MAKE_ENUM: u8         = 0x24;
    pub const ENUM_TAG: u8          = 0x25;
    pub const ENUM_PAYLOAD: u8      = 0x26;
    pub const MAP_GET: u8           = 0x27;
    pub const MAP_PUT: u8           = 0x28;
    pub const PMAP_GET: u8          = 0x29;
    pub const PMAP_PUT: u8          = 0x2a;
    pub const PMAP_CONTAINS: u8     = 0x2b;
    pub const PVEC_GET: u8          = 0x2c;
    pub const PVEC_SET: u8          = 0x2d;
    pub const PVEC_PUSH: u8         = 0x2e;
    pub const PVEC_LEN: u8          = 0x2f;
    pub const INC_REG: u8           = 0x30;
    pub const CALL_EXTERNAL_DYN: u8 = 0x31;
    pub const MAKE_INTERFACE: u8    = 0x32;
    pub const PMAP_ENTRIES: u8      = 0x33;
    pub const PMAP_KEYS: u8         = 0x34;
    pub const PMAP_VALUES: u8       = 0x35;
    pub const PVEC_TO_ARRAY: u8     = 0x36;
    pub const PMAP_APPEND_UNIQUE: u8 = 0x37;
    pub const PMAP_PUT_UNIQUE: u8    = 0x38;
    pub const PMAP_WALK_INIT: u8     = 0x39;
    pub const PMAP_WALK_NEXT: u8     = 0x3a;
    pub const PBTREE_GET: u8         = 0x3b;
    pub const PBTREE_PUT: u8         = 0x3c;
    pub const PBTREE_CONTAINS: u8    = 0x3d;
    pub const PBTREE_RANGE: u8       = 0x3e;
    pub const PBTREE_WALK_INIT: u8   = 0x3f;
    pub const PBTREE_WALK_NEXT: u8   = 0x40;
    pub const PBTREE_PUT_UNIQUE: u8  = 0x41;
    pub const PBTREE_APPEND_UNIQUE: u8 = 0x42;
    pub const PMAP_DELETE: u8          = 0x43;
    pub const PBTREE_DELETE: u8        = 0x44;
    pub const PMAP_REMOVE_UNIQUE: u8   = 0x45;
    pub const PMAP_REMOVE_FROM_LIST: u8 = 0x46;
    pub const PBTREE_REMOVE_UNIQUE: u8 = 0x47;
    pub const PBTREE_REMOVE_FROM_LIST: u8 = 0x48;
}

fn write_instr(out: &mut Vec<u8>, instr: &Instr) {
    match instr {
        Instr::LoadConst { dst, idx } => { out.push(op::LOAD_CONST); write_u16(out, *dst); write_u16(out, *idx); }
        Instr::Move { dst, src }       => { out.push(op::MOVE); write_u16(out, *dst); write_u16(out, *src); }
        Instr::Bin { op: bop, dst, lhs, rhs } => {
            out.push(op::BIN);
            out.push(binop_to_byte(*bop));
            write_u16(out, *dst); write_u16(out, *lhs); write_u16(out, *rhs);
        }
        Instr::Un { op: uop, dst, src } => {
            out.push(op::UN);
            out.push(unop_to_byte(*uop));
            write_u16(out, *dst); write_u16(out, *src);
        }
        Instr::Jump { offset } => { out.push(op::JUMP); write_i32(out, *offset); }
        Instr::JumpIfFalse { cond, offset } => {
            out.push(op::JUMP_IF_FALSE); write_u16(out, *cond); write_i32(out, *offset);
        }
        Instr::Call { dst, fn_idx, args_start, n_args } => {
            out.push(op::CALL); write_u16(out, *dst); write_u16(out, *fn_idx);
            write_u16(out, *args_start); out.push(*n_args);
        }
        Instr::CallHost { dst, import_idx, args_start, n_args } => {
            out.push(op::CALL_HOST); write_u16(out, *dst); write_u16(out, *import_idx);
            write_u16(out, *args_start); out.push(*n_args);
        }
        Instr::CallExternal { dst, module_name_idx, fn_name_idx, args_start, n_args, is_view, is_pure } => {
            out.push(op::CALL_EXTERNAL); write_u16(out, *dst);
            write_u16(out, *module_name_idx); write_u16(out, *fn_name_idx);
            write_u16(out, *args_start); out.push(*n_args);
            out.push(if *is_view { 1 } else { 0 });
            out.push(if *is_pure { 1 } else { 0 });
        }
        Instr::Return { src } => { out.push(op::RETURN); write_u16(out, *src); }
        Instr::ReturnUnit     => out.push(op::RETURN_UNIT),
        Instr::BuiltinResource { dst, src } => { out.push(op::BUILTIN_RESOURCE); write_u16(out, *dst); write_u16(out, *src); }
        Instr::BuiltinUnwrap   { dst, src } => { out.push(op::BUILTIN_UNWRAP);   write_u16(out, *dst); write_u16(out, *src); }
        Instr::BuiltinAddress  { dst, src } => { out.push(op::BUILTIN_ADDRESS);  write_u16(out, *dst); write_u16(out, *src); }
        Instr::BuiltinLen      { dst, src } => { out.push(op::BUILTIN_LEN);      write_u16(out, *dst); write_u16(out, *src); }
        Instr::Convert { dst, src, target } => {
            out.push(op::CONVERT); write_u16(out, *dst); write_u16(out, *src); out.push(*target);
        }
        Instr::MakeArray { dst, args_start, n } => {
            out.push(op::MAKE_ARRAY); write_u16(out, *dst); write_u16(out, *args_start); write_u16(out, *n);
        }
        Instr::ArrayGet { dst, arr, idx } => {
            out.push(op::ARRAY_GET); write_u16(out, *dst); write_u16(out, *arr); write_u16(out, *idx);
        }
        Instr::ArrayAppend { dst, arr, elem } => {
            out.push(op::ARRAY_APPEND); write_u16(out, *dst); write_u16(out, *arr); write_u16(out, *elem);
        }
        Instr::MakeSet { dst, args_start, n } => {
            out.push(op::MAKE_SET); write_u16(out, *dst); write_u16(out, *args_start); write_u16(out, *n);
        }
        Instr::MakeDict { dst, args_start, n_pairs } => {
            out.push(op::MAKE_DICT); write_u16(out, *dst); write_u16(out, *args_start); write_u16(out, *n_pairs);
        }
        Instr::BuiltinCall { dst, name_idx, args_start, n_args } => {
            out.push(op::BUILTIN_CALL); write_u16(out, *dst); write_u16(out, *name_idx);
            write_u16(out, *args_start); out.push(*n_args);
        }
        Instr::MakeStruct { dst, shape_idx, args_start, n } => {
            out.push(op::MAKE_STRUCT); write_u16(out, *dst); write_u16(out, *shape_idx);
            write_u16(out, *args_start); write_u16(out, *n);
        }
        Instr::FieldGet { dst, src, name_idx } => {
            out.push(op::FIELD_GET); write_u16(out, *dst); write_u16(out, *src); write_u16(out, *name_idx);
        }
        Instr::FieldSet { dst, name_idx, val } => {
            out.push(op::FIELD_SET); write_u16(out, *dst); write_u16(out, *name_idx); write_u16(out, *val);
        }
        Instr::KvGet { dst, key_idx } => { out.push(op::KV_GET); write_u16(out, *dst); write_u16(out, *key_idx); }
        Instr::KvPut { src, key_idx } => { out.push(op::KV_PUT); write_u16(out, *src); write_u16(out, *key_idx); }
        Instr::KvGetPath { dst, path_idx } => { out.push(op::KV_GET_PATH); write_u16(out, *dst); write_u16(out, *path_idx); }
        Instr::KvPutPath { src, path_idx } => { out.push(op::KV_PUT_PATH); write_u16(out, *src); write_u16(out, *path_idx); }
        Instr::ReadBatch { group_idx } => { out.push(op::READ_BATCH); write_u16(out, *group_idx); }
        Instr::PrefetchMap { arr_reg, state_idx } => {
            out.push(op::PREFETCH_MAP); write_u16(out, *arr_reg); write_u16(out, *state_idx);
        }
        Instr::Emit { value } => {
            out.push(op::EMIT); write_u16(out, *value);
        }
        Instr::Context { dst, kind } => { out.push(op::CONTEXT); write_u16(out, *dst); out.push(*kind); }
        Instr::MakeTuple { dst, args_start, n } => {
            out.push(op::MAKE_TUPLE); write_u16(out, *dst); write_u16(out, *args_start); write_u16(out, *n);
        }
        Instr::TupleGet { dst, src, index } => {
            out.push(op::TUPLE_GET); write_u16(out, *dst); write_u16(out, *src); write_u16(out, *index);
        }
        Instr::MakeEnum { dst, shape_idx, variant_idx, args_start, n } => {
            out.push(op::MAKE_ENUM); write_u16(out, *dst); write_u16(out, *shape_idx);
            write_u16(out, *variant_idx); write_u16(out, *args_start); write_u16(out, *n);
        }
        Instr::EnumTag { dst, src } => { out.push(op::ENUM_TAG); write_u16(out, *dst); write_u16(out, *src); }
        Instr::EnumPayload { dst, src, index } => {
            out.push(op::ENUM_PAYLOAD); write_u16(out, *dst); write_u16(out, *src); write_u16(out, *index);
        }
        Instr::MapGet { dst, state_idx, key_reg } => {
            out.push(op::MAP_GET); write_u16(out, *dst); write_u16(out, *state_idx); write_u16(out, *key_reg);
        }
        Instr::MapPut { state_idx, key_reg, src } => {
            out.push(op::MAP_PUT); write_u16(out, *state_idx); write_u16(out, *key_reg); write_u16(out, *src);
        }
        Instr::PMapGet { dst, state_idx, key_reg } => {
            out.push(op::PMAP_GET); write_u16(out, *dst); write_u16(out, *state_idx); write_u16(out, *key_reg);
        }
        Instr::PMapPut { state_idx, key_reg, src } => {
            out.push(op::PMAP_PUT); write_u16(out, *state_idx); write_u16(out, *key_reg); write_u16(out, *src);
        }
        Instr::PMapContains { dst, state_idx, key_reg } => {
            out.push(op::PMAP_CONTAINS); write_u16(out, *dst); write_u16(out, *state_idx); write_u16(out, *key_reg);
        }
        Instr::PVecGet { dst, state_idx, idx_reg } => {
            out.push(op::PVEC_GET); write_u16(out, *dst); write_u16(out, *state_idx); write_u16(out, *idx_reg);
        }
        Instr::PVecSet { state_idx, idx_reg, src } => {
            out.push(op::PVEC_SET); write_u16(out, *state_idx); write_u16(out, *idx_reg); write_u16(out, *src);
        }
        Instr::PVecPush { dst, state_idx, src } => {
            out.push(op::PVEC_PUSH); write_u16(out, *dst); write_u16(out, *state_idx); write_u16(out, *src);
        }
        Instr::PVecLen { dst, state_idx } => {
            out.push(op::PVEC_LEN); write_u16(out, *dst); write_u16(out, *state_idx);
        }
        Instr::IncReg { reg } => { out.push(op::INC_REG); write_u16(out, *reg); }
        Instr::CallExternalDyn { dst, target_reg, fn_name_idx, args_start, n_args, is_view, is_pure } => {
            out.push(op::CALL_EXTERNAL_DYN);
            write_u16(out, *dst);
            write_u16(out, *target_reg);
            write_u16(out, *fn_name_idx);
            write_u16(out, *args_start);
            out.push(*n_args);
            out.push(if *is_view { 1 } else { 0 });
            out.push(if *is_pure { 1 } else { 0 });
        }
        Instr::MakeInterface { dst, iface_name_idx, target_reg } => {
            out.push(op::MAKE_INTERFACE);
            write_u16(out, *dst);
            write_u16(out, *iface_name_idx);
            write_u16(out, *target_reg);
        }
        Instr::PMapEntries { dst, state_idx } => {
            out.push(op::PMAP_ENTRIES); write_u16(out, *dst); write_u16(out, *state_idx);
        }
        Instr::PMapKeys { dst, state_idx } => {
            out.push(op::PMAP_KEYS); write_u16(out, *dst); write_u16(out, *state_idx);
        }
        Instr::PMapValues { dst, state_idx } => {
            out.push(op::PMAP_VALUES); write_u16(out, *dst); write_u16(out, *state_idx);
        }
        Instr::PVecToArray { dst, state_idx } => {
            out.push(op::PVEC_TO_ARRAY); write_u16(out, *dst); write_u16(out, *state_idx);
        }
        Instr::PMapAppendUnique { state_idx, key_reg, elem_reg } => {
            out.push(op::PMAP_APPEND_UNIQUE);
            write_u16(out, *state_idx);
            write_u16(out, *key_reg);
            write_u16(out, *elem_reg);
        }
        Instr::PMapPutUnique { state_idx, key_reg, src } => {
            out.push(op::PMAP_PUT_UNIQUE);
            write_u16(out, *state_idx);
            write_u16(out, *key_reg);
            write_u16(out, *src);
        }
        Instr::PMapWalkInit { dst, state_idx } => {
            out.push(op::PMAP_WALK_INIT);
            write_u16(out, *dst);
            write_u16(out, *state_idx);
        }
        Instr::PMapWalkNext { cursor_reg, value_reg, end_offset } => {
            out.push(op::PMAP_WALK_NEXT);
            write_u16(out, *cursor_reg);
            write_u16(out, *value_reg);
            write_i32(out, *end_offset);
        }
        Instr::PBTreeGet { dst, state_idx, key_reg } => {
            out.push(op::PBTREE_GET); write_u16(out, *dst); write_u16(out, *state_idx); write_u16(out, *key_reg);
        }
        Instr::PBTreePut { state_idx, key_reg, src } => {
            out.push(op::PBTREE_PUT); write_u16(out, *state_idx); write_u16(out, *key_reg); write_u16(out, *src);
        }
        Instr::PBTreeContains { dst, state_idx, key_reg } => {
            out.push(op::PBTREE_CONTAINS); write_u16(out, *dst); write_u16(out, *state_idx); write_u16(out, *key_reg);
        }
        Instr::PBTreeRange { dst, state_idx, lo_reg, hi_reg } => {
            out.push(op::PBTREE_RANGE);
            write_u16(out, *dst); write_u16(out, *state_idx);
            write_u16(out, *lo_reg); write_u16(out, *hi_reg);
        }
        Instr::PBTreeWalkInit { dst, state_idx } => {
            out.push(op::PBTREE_WALK_INIT); write_u16(out, *dst); write_u16(out, *state_idx);
        }
        Instr::PBTreeWalkNext { cursor_reg, value_reg, end_offset } => {
            out.push(op::PBTREE_WALK_NEXT);
            write_u16(out, *cursor_reg);
            write_u16(out, *value_reg);
            write_i32(out, *end_offset);
        }
        Instr::PBTreePutUnique { state_idx, key_reg, src } => {
            out.push(op::PBTREE_PUT_UNIQUE);
            write_u16(out, *state_idx);
            write_u16(out, *key_reg);
            write_u16(out, *src);
        }
        Instr::PBTreeAppendUnique { state_idx, key_reg, elem_reg } => {
            out.push(op::PBTREE_APPEND_UNIQUE);
            write_u16(out, *state_idx);
            write_u16(out, *key_reg);
            write_u16(out, *elem_reg);
        }
        Instr::PMapDelete { state_idx, key_reg } => {
            out.push(op::PMAP_DELETE); write_u16(out, *state_idx); write_u16(out, *key_reg);
        }
        Instr::PBTreeDelete { state_idx, key_reg } => {
            out.push(op::PBTREE_DELETE); write_u16(out, *state_idx); write_u16(out, *key_reg);
        }
        Instr::PMapRemoveUnique { state_idx, key_reg, expected_reg } => {
            out.push(op::PMAP_REMOVE_UNIQUE);
            write_u16(out, *state_idx); write_u16(out, *key_reg); write_u16(out, *expected_reg);
        }
        Instr::PMapRemoveFromList { state_idx, key_reg, elem_reg } => {
            out.push(op::PMAP_REMOVE_FROM_LIST);
            write_u16(out, *state_idx); write_u16(out, *key_reg); write_u16(out, *elem_reg);
        }
        Instr::PBTreeRemoveUnique { state_idx, key_reg, expected_reg } => {
            out.push(op::PBTREE_REMOVE_UNIQUE);
            write_u16(out, *state_idx); write_u16(out, *key_reg); write_u16(out, *expected_reg);
        }
        Instr::PBTreeRemoveFromList { state_idx, key_reg, elem_reg } => {
            out.push(op::PBTREE_REMOVE_FROM_LIST);
            write_u16(out, *state_idx); write_u16(out, *key_reg); write_u16(out, *elem_reg);
        }
    }
}

fn read_instr(r: &mut Reader) -> Result<Instr, Error> {
    let tag = r.read_u8()?;
    Ok(match tag {
        op::LOAD_CONST => Instr::LoadConst { dst: r.read_u16()?, idx: r.read_u16()? },
        op::MOVE       => Instr::Move      { dst: r.read_u16()?, src: r.read_u16()? },
        op::BIN => Instr::Bin {
            op: binop_from_byte(r.read_u8()?)?,
            dst: r.read_u16()?, lhs: r.read_u16()?, rhs: r.read_u16()?,
        },
        op::UN => Instr::Un {
            op: unop_from_byte(r.read_u8()?)?,
            dst: r.read_u16()?, src: r.read_u16()?,
        },
        op::JUMP => Instr::Jump { offset: r.read_i32()? },
        op::JUMP_IF_FALSE => Instr::JumpIfFalse { cond: r.read_u16()?, offset: r.read_i32()? },
        op::CALL => Instr::Call {
            dst: r.read_u16()?, fn_idx: r.read_u16()?,
            args_start: r.read_u16()?, n_args: r.read_u8()?,
        },
        op::CALL_HOST => Instr::CallHost {
            dst: r.read_u16()?, import_idx: r.read_u16()?,
            args_start: r.read_u16()?, n_args: r.read_u8()?,
        },
        op::CALL_EXTERNAL => Instr::CallExternal {
            dst: r.read_u16()?,
            module_name_idx: r.read_u16()?, fn_name_idx: r.read_u16()?,
            args_start: r.read_u16()?, n_args: r.read_u8()?,
            is_view: r.read_u8()? != 0,
            is_pure: r.read_u8()? != 0,
        },
        op::RETURN      => Instr::Return { src: r.read_u16()? },
        op::RETURN_UNIT => Instr::ReturnUnit,
        op::BUILTIN_RESOURCE => Instr::BuiltinResource { dst: r.read_u16()?, src: r.read_u16()? },
        op::BUILTIN_UNWRAP   => Instr::BuiltinUnwrap   { dst: r.read_u16()?, src: r.read_u16()? },
        op::BUILTIN_ADDRESS  => Instr::BuiltinAddress  { dst: r.read_u16()?, src: r.read_u16()? },
        op::BUILTIN_LEN      => Instr::BuiltinLen      { dst: r.read_u16()?, src: r.read_u16()? },
        op::CONVERT => Instr::Convert {
            dst: r.read_u16()?, src: r.read_u16()?, target: r.read_u8()?,
        },
        op::MAKE_ARRAY  => Instr::MakeArray  { dst: r.read_u16()?, args_start: r.read_u16()?, n: r.read_u16()? },
        op::ARRAY_GET   => Instr::ArrayGet   { dst: r.read_u16()?, arr: r.read_u16()?, idx: r.read_u16()? },
        op::ARRAY_APPEND => Instr::ArrayAppend { dst: r.read_u16()?, arr: r.read_u16()?, elem: r.read_u16()? },
        op::MAKE_SET    => Instr::MakeSet    { dst: r.read_u16()?, args_start: r.read_u16()?, n: r.read_u16()? },
        op::MAKE_DICT   => Instr::MakeDict   { dst: r.read_u16()?, args_start: r.read_u16()?, n_pairs: r.read_u16()? },
        op::BUILTIN_CALL => Instr::BuiltinCall {
            dst: r.read_u16()?, name_idx: r.read_u16()?,
            args_start: r.read_u16()?, n_args: r.read_u8()?,
        },
        op::MAKE_STRUCT => Instr::MakeStruct {
            dst: r.read_u16()?, shape_idx: r.read_u16()?,
            args_start: r.read_u16()?, n: r.read_u16()?,
        },
        op::FIELD_GET => Instr::FieldGet { dst: r.read_u16()?, src: r.read_u16()?, name_idx: r.read_u16()? },
        op::FIELD_SET => Instr::FieldSet { dst: r.read_u16()?, name_idx: r.read_u16()?, val: r.read_u16()? },
        op::KV_GET      => Instr::KvGet      { dst: r.read_u16()?, key_idx: r.read_u16()? },
        op::KV_PUT      => Instr::KvPut      { src: r.read_u16()?, key_idx: r.read_u16()? },
        op::KV_GET_PATH => Instr::KvGetPath  { dst: r.read_u16()?, path_idx: r.read_u16()? },
        op::KV_PUT_PATH => Instr::KvPutPath  { src: r.read_u16()?, path_idx: r.read_u16()? },
        op::READ_BATCH  => Instr::ReadBatch  { group_idx: r.read_u16()? },
        op::PREFETCH_MAP => Instr::PrefetchMap { arr_reg: r.read_u16()?, state_idx: r.read_u16()? },
        op::EMIT => Instr::Emit { value: r.read_u16()? },
        op::CONTEXT => Instr::Context { dst: r.read_u16()?, kind: r.read_u8()? },
        op::MAKE_TUPLE => Instr::MakeTuple { dst: r.read_u16()?, args_start: r.read_u16()?, n: r.read_u16()? },
        op::TUPLE_GET  => Instr::TupleGet  { dst: r.read_u16()?, src: r.read_u16()?, index: r.read_u16()? },
        op::MAKE_ENUM => Instr::MakeEnum {
            dst: r.read_u16()?, shape_idx: r.read_u16()?, variant_idx: r.read_u16()?,
            args_start: r.read_u16()?, n: r.read_u16()?,
        },
        op::ENUM_TAG     => Instr::EnumTag     { dst: r.read_u16()?, src: r.read_u16()? },
        op::ENUM_PAYLOAD => Instr::EnumPayload { dst: r.read_u16()?, src: r.read_u16()?, index: r.read_u16()? },
        op::MAP_GET => Instr::MapGet { dst: r.read_u16()?, state_idx: r.read_u16()?, key_reg: r.read_u16()? },
        op::MAP_PUT => Instr::MapPut { state_idx: r.read_u16()?, key_reg: r.read_u16()?, src: r.read_u16()? },
        op::PMAP_GET => Instr::PMapGet { dst: r.read_u16()?, state_idx: r.read_u16()?, key_reg: r.read_u16()? },
        op::PMAP_PUT => Instr::PMapPut { state_idx: r.read_u16()?, key_reg: r.read_u16()?, src: r.read_u16()? },
        op::PMAP_CONTAINS => Instr::PMapContains { dst: r.read_u16()?, state_idx: r.read_u16()?, key_reg: r.read_u16()? },
        op::PVEC_GET => Instr::PVecGet { dst: r.read_u16()?, state_idx: r.read_u16()?, idx_reg: r.read_u16()? },
        op::PVEC_SET => Instr::PVecSet { state_idx: r.read_u16()?, idx_reg: r.read_u16()?, src: r.read_u16()? },
        op::PVEC_PUSH => Instr::PVecPush { dst: r.read_u16()?, state_idx: r.read_u16()?, src: r.read_u16()? },
        op::PVEC_LEN => Instr::PVecLen { dst: r.read_u16()?, state_idx: r.read_u16()? },
        op::INC_REG => Instr::IncReg { reg: r.read_u16()? },
        op::CALL_EXTERNAL_DYN => Instr::CallExternalDyn {
            dst: r.read_u16()?, target_reg: r.read_u16()?,
            fn_name_idx: r.read_u16()?, args_start: r.read_u16()?,
            n_args: r.read_u8()?,
            is_view: r.read_u8()? != 0,
            is_pure: r.read_u8()? != 0,
        },
        op::MAKE_INTERFACE => Instr::MakeInterface {
            dst: r.read_u16()?,
            iface_name_idx: r.read_u16()?,
            target_reg: r.read_u16()?,
        },
        op::PMAP_ENTRIES   => Instr::PMapEntries  { dst: r.read_u16()?, state_idx: r.read_u16()? },
        op::PMAP_KEYS      => Instr::PMapKeys     { dst: r.read_u16()?, state_idx: r.read_u16()? },
        op::PMAP_VALUES    => Instr::PMapValues   { dst: r.read_u16()?, state_idx: r.read_u16()? },
        op::PVEC_TO_ARRAY  => Instr::PVecToArray  { dst: r.read_u16()?, state_idx: r.read_u16()? },
        op::PMAP_APPEND_UNIQUE => Instr::PMapAppendUnique {
            state_idx: r.read_u16()?, key_reg: r.read_u16()?, elem_reg: r.read_u16()?,
        },
        op::PMAP_PUT_UNIQUE => Instr::PMapPutUnique {
            state_idx: r.read_u16()?, key_reg: r.read_u16()?, src: r.read_u16()?,
        },
        op::PMAP_WALK_INIT => Instr::PMapWalkInit {
            dst: r.read_u16()?, state_idx: r.read_u16()?,
        },
        op::PMAP_WALK_NEXT => Instr::PMapWalkNext {
            cursor_reg: r.read_u16()?, value_reg: r.read_u16()?, end_offset: r.read_i32()?,
        },
        op::PBTREE_GET => Instr::PBTreeGet { dst: r.read_u16()?, state_idx: r.read_u16()?, key_reg: r.read_u16()? },
        op::PBTREE_PUT => Instr::PBTreePut { state_idx: r.read_u16()?, key_reg: r.read_u16()?, src: r.read_u16()? },
        op::PBTREE_CONTAINS => Instr::PBTreeContains { dst: r.read_u16()?, state_idx: r.read_u16()?, key_reg: r.read_u16()? },
        op::PBTREE_RANGE => Instr::PBTreeRange {
            dst: r.read_u16()?, state_idx: r.read_u16()?,
            lo_reg: r.read_u16()?, hi_reg: r.read_u16()?,
        },
        op::PBTREE_WALK_INIT => Instr::PBTreeWalkInit { dst: r.read_u16()?, state_idx: r.read_u16()? },
        op::PBTREE_WALK_NEXT => Instr::PBTreeWalkNext {
            cursor_reg: r.read_u16()?, value_reg: r.read_u16()?, end_offset: r.read_i32()?,
        },
        op::PBTREE_PUT_UNIQUE => Instr::PBTreePutUnique {
            state_idx: r.read_u16()?, key_reg: r.read_u16()?, src: r.read_u16()?,
        },
        op::PBTREE_APPEND_UNIQUE => Instr::PBTreeAppendUnique {
            state_idx: r.read_u16()?, key_reg: r.read_u16()?, elem_reg: r.read_u16()?,
        },
        op::PMAP_DELETE => Instr::PMapDelete { state_idx: r.read_u16()?, key_reg: r.read_u16()? },
        op::PBTREE_DELETE => Instr::PBTreeDelete { state_idx: r.read_u16()?, key_reg: r.read_u16()? },
        op::PMAP_REMOVE_UNIQUE => Instr::PMapRemoveUnique {
            state_idx: r.read_u16()?, key_reg: r.read_u16()?, expected_reg: r.read_u16()?,
        },
        op::PMAP_REMOVE_FROM_LIST => Instr::PMapRemoveFromList {
            state_idx: r.read_u16()?, key_reg: r.read_u16()?, elem_reg: r.read_u16()?,
        },
        op::PBTREE_REMOVE_UNIQUE => Instr::PBTreeRemoveUnique {
            state_idx: r.read_u16()?, key_reg: r.read_u16()?, expected_reg: r.read_u16()?,
        },
        op::PBTREE_REMOVE_FROM_LIST => Instr::PBTreeRemoveFromList {
            state_idx: r.read_u16()?, key_reg: r.read_u16()?, elem_reg: r.read_u16()?,
        },
        other => return Err(decode_err(format!("unknown instruction tag 0x{other:02x}"))),
    })
}

// ---------- low-level primitives --------------------------------

fn write_u16(out: &mut Vec<u8>, v: u16) { out.extend_from_slice(&v.to_be_bytes()); }
fn write_u32(out: &mut Vec<u8>, v: u32) { out.extend_from_slice(&v.to_be_bytes()); }
fn write_u64(out: &mut Vec<u8>, v: u64) { out.extend_from_slice(&v.to_be_bytes()); }
fn write_u128(out: &mut Vec<u8>, v: u128) { out.extend_from_slice(&v.to_be_bytes()); }
fn write_i32(out: &mut Vec<u8>, v: i32) { out.extend_from_slice(&v.to_be_bytes()); }
// `write_i64` / `read_i64` retired with the BigInt rewrite — Int /
// Resource use BigInt-based encoding now and other 8-byte values
// have their own helpers.

fn write_str(out: &mut Vec<u8>, s: &str) {
    let bytes = s.as_bytes();
    write_u32(out, bytes.len() as u32);
    out.extend_from_slice(bytes);
}

fn read_str(r: &mut Reader) -> Result<String, Error> {
    let len = r.read_u32()? as usize;
    let bytes = r.take_bytes(len)?;
    std::str::from_utf8(bytes)
        .map(str::to_string)
        .map_err(|_| decode_err("invalid utf-8 in string"))
}

struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self { Self { bytes, pos: 0 } }

    fn take_bytes(&mut self, n: usize) -> Result<&'a [u8], Error> {
        if self.pos + n > self.bytes.len() {
            return Err(decode_err(format!(
                "truncated artifact: needed {n} bytes at offset {}, had {}",
                self.pos,
                self.bytes.len() - self.pos,
            )));
        }
        let slice = &self.bytes[self.pos..self.pos + n];
        self.pos += n;
        Ok(slice)
    }

    fn read_u8(&mut self) -> Result<u8, Error> {
        Ok(self.take_bytes(1)?[0])
    }
    fn read_u16(&mut self) -> Result<u16, Error> {
        let arr: [u8; 2] = self.take_bytes(2)?.try_into().unwrap();
        Ok(u16::from_be_bytes(arr))
    }
    fn read_u32(&mut self) -> Result<u32, Error> {
        let arr: [u8; 4] = self.take_bytes(4)?.try_into().unwrap();
        Ok(u32::from_be_bytes(arr))
    }
    fn read_u64(&mut self) -> Result<u64, Error> {
        let arr: [u8; 8] = self.take_bytes(8)?.try_into().unwrap();
        Ok(u64::from_be_bytes(arr))
    }
    fn read_u128(&mut self) -> Result<u128, Error> {
        let arr: [u8; 16] = self.take_bytes(16)?.try_into().unwrap();
        Ok(u128::from_be_bytes(arr))
    }
    fn read_i32(&mut self) -> Result<i32, Error> {
        let arr: [u8; 4] = self.take_bytes(4)?.try_into().unwrap();
        Ok(i32::from_be_bytes(arr))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(modules: Vec<BcModule>) -> Vec<BcModule> {
        let bytes = encode(&modules);
        decode(&bytes).expect("decode")
    }

    fn assert_module_eq(a: &BcModule, b: &BcModule) {
        // We don't derive PartialEq on BcModule (HashMap doesn't
        // care about insertion order), so spot-check the salient
        // fields that should match exactly.
        assert_eq!(a.name, b.name);
        assert_eq!(a.imports, b.imports);
        assert_eq!(a.state_roots, b.state_roots);
        assert_eq!(a.state_types, b.state_types);
        assert_eq!(a.state_names, b.state_names);
        assert_eq!(a.state_defaults, b.state_defaults);
        assert_eq!(a.functions.len(), b.functions.len());
        for (af, bf) in a.functions.iter().zip(b.functions.iter()) {
            assert_eq!(af.name, bf.name);
            assert_eq!(af.n_params, bf.n_params);
            assert_eq!(af.n_regs, bf.n_regs);
            assert_eq!(af.is_entry, bf.is_entry);
            assert_eq!(af.is_nore, bf.is_nore);
            assert_eq!(af.consts, bf.consts);
            assert_eq!(af.code.len(), bf.code.len());
            for (ai, bi) in af.code.iter().zip(bf.code.iter()) {
                // Instr derives PartialEq.
                assert_eq!(ai, bi);
            }
        }
        assert_eq!(a.struct_shapes.len(), b.struct_shapes.len());
        assert_eq!(a.enum_shapes.len(), b.enum_shapes.len());
    }

    #[test]
    fn round_trip_minimal_module() {
        let src = "
            fn main() -> i64 {
                return 1 + 2 * 3;
            }
        ";
        let module = crate::frontend(src).unwrap();
        let bc = crate::compile::compile(&module).unwrap();
        let back = roundtrip(vec![bc.clone()]);
        assert_eq!(back.len(), 1);
        assert_module_eq(&bc, &back[0]);
    }

    #[test]
    fn round_trip_with_states_and_struct() {
        let src = "
            struct Point { x: i64, y: i64 }
            state origin: Point;
            state n: i64;
            entry fn move_x(d: i64) -> i64 {
                origin.x = origin.x + d;
                n = n + 1;
                return origin.x;
            }
            fn main() -> i64 { return move_x(7); }
        ";
        let module = crate::frontend(src).unwrap();
        let bc = crate::compile::compile(&module).unwrap();
        let back = roundtrip(vec![bc.clone()]);
        assert_module_eq(&bc, &back[0]);
    }

    #[test]
    fn round_trip_pmap_pvec() {
        let src = "
            state m: pmap<i64, u64>;
            state v: pvec<i64>;
            fn main() -> u64 {
                m[1] = 100u64;
                pvec_push(v, 7);
                return m[1];
            }
        ";
        let module = crate::frontend(src).unwrap();
        let bc = crate::compile::compile(&module).unwrap();
        let back = roundtrip(vec![bc.clone()]);
        assert_module_eq(&bc, &back[0]);
    }

    #[test]
    fn deterministic_encoding() {
        // Compiling the same source twice must produce byte-
        // identical artifacts — that's the property that makes
        // the content hash a stable module ID.
        let src = "
            state n: i64;
            entry fn bump() -> i64 { n = n + 1; return n; }
            fn main() -> i64 { return bump(); }
        ";
        let m1 = crate::compile::compile(&crate::frontend(src).unwrap()).unwrap();
        let m2 = crate::compile::compile(&crate::frontend(src).unwrap()).unwrap();
        let b1 = encode(std::slice::from_ref(&m1));
        let b2 = encode(std::slice::from_ref(&m2));
        assert_eq!(b1, b2, "same source must produce byte-identical artifact");
    }

    #[test]
    fn rejects_bad_magic() {
        let mut bytes = vec![0u8; 16];
        bytes[..8].copy_from_slice(b"NOTAREND");
        let err = decode(&bytes).unwrap_err();
        assert!(err.to_string().contains("magic"));
    }

    #[test]
    fn rejects_truncated() {
        let src = "fn main() -> i64 { return 1; }";
        let bc = crate::compile::compile(&crate::frontend(src).unwrap()).unwrap();
        let bytes = encode(std::slice::from_ref(&bc));
        let truncated = &bytes[..bytes.len() / 2];
        let err = decode(truncated).unwrap_err();
        assert!(err.to_string().contains("truncated") || err.to_string().contains("invalid"));
    }
}
