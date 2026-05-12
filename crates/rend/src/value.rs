//! Runtime values produced by the interpreter.

use crate::ast::Type;
use num_bigint::BigInt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    /// Arbitrary-precision signed integer. The default integer type
    /// in rend; no overflow at the language level, no fixed width.
    /// Sized variants (`I32`, `U32`, `U64`, `U128`) carry hardware
    /// integers for places where range and bit-width matter.
    Int(BigInt),
    I32(i32),
    U32(u32),
    U64(u64),
    U128(u128),
    Bool(bool),
    Unit,
    Resource(i64),
    Str(String),
    Address(String),
    Bytes(Vec<u8>),
    Array(Vec<Value>),
    /// In-memory set. Insertion order; uniqueness enforced on construction
    /// and on `set_insert`. Non-storage.
    Set(Vec<Value>),
    /// In-memory dictionary. Insertion order; key uniqueness enforced.
    /// Non-storage.
    Dict(Vec<(Value, Value)>),
    /// Struct value. Fields are stored in declaration order, paired with
    /// their declared names so field access works at runtime without an
    /// out-of-band type table.
    Struct { name: String, fields: Vec<(String, Value)> },
    /// Anonymous fixed-arity tuple. Local-only; never serialized or
    /// stored in state. Used for multi-value returns and `let (a, b)`
    /// destructuring.
    Tuple(Vec<Value>),
    /// Sum-type value. `enum_name` is the type's declared name;
    /// `variant` names which case is active; `payload` carries the
    /// case's positional payload (empty for unit variants).
    Enum { enum_name: String, variant: String, payload: Vec<Value> },
    /// A read result that was queued but not yet fetched from the KV.
    /// Carries a handle id; resolved by `Tx::force` when the value is
    /// consumed (Bin/Un/comparisons/host calls/Return-to-host/etc.).
    /// Pending values propagate through Move, MakeArray, MakeStruct,
    /// FieldGet, ArrayGet, function calls, and Return — only consumer
    /// instructions trigger resolution. Lets multiple pending reads
    /// pile up across loop iterations and call boundaries before being
    /// flushed in one `Kv::get_many` round-trip.
    Pending(u64),
    /// Persistent map. The runtime carries only the *root hash* of
    /// the HAMT — the tree itself lives spread across content-
    /// addressed KV cells, one cell per node. Operations walk the
    /// tree on demand via the Tx layer, so a multi-gigabyte pmap
    /// never sits in memory all at once. Zero is the empty tree.
    PMap(u128),
    /// Persistent sorted map (radix trie keyed by big-endian key
    /// bytes). Same root-hash representation as PMap; the difference
    /// is purely in how the bits drive the trie path (preserving
    /// key order instead of randomizing via hash). Distinct cell
    /// namespace from PMap so the two never share node cells.
    PBTree(u128),
    /// Dynamic JSON value. Carries the parsed structure inline —
    /// not the original text. Constructed via `parse_json(string)`
    /// or received from a host function; navigated with `->`;
    /// converted to typed primitives via `json_to_*` builtins.
    Json(crate::json::Json),
    /// Persistent vector. The state cell carries the current
    /// length plus the root hash; tree nodes (one per cell) hold
    /// up to 32 elements (leaves) or 32 children (inner). Like
    /// PMap, walks are O(log32 N) and never materialize the whole
    /// vector in memory.
    PVec { len: u64, root: u128 },
    /// Interface value — a runtime-bound module reference. The
    /// `iface` is the static interface name (for diagnostics);
    /// the `target_module` is the bound module's name, used by
    /// the VM to look up the dispatch target via `module_index`.
    Interface { iface: String, target_module: String },
    /// Transient streaming-iterator state for a HAMT (`pmap`) walk.
    /// Lives in a register between `PMapWalkInit` and the loop's
    /// terminating `PMapWalkNext`. Never serialized or stored in
    /// state. Holds the depth-first walk frontier plus pending
    /// leaf entries to yield one at a time.
    PMapCursor(Box<PMapCursor>),
    /// Transient streaming-iterator state for a sorted-trie
    /// (`pbtree`) walk. Same shape as `PMapCursor`, distinct so
    /// the runtime can't mix walks routed through different cell
    /// namespaces.
    PBTreeCursor(Box<PMapCursor>),
}

/// Walk state for a streaming HAMT iteration. Each entry on
/// `stack` is a `(node_hash, next_child_index)` pair: when the
/// runtime pops the top entry and the node is an interior node,
/// it pushes one child at a time so cells are fetched lazily.
/// `pending` holds entries from the current leaf, drained one
/// per `PMapWalkNext` call before the walk descends further.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PMapCursor {
    pub stack: Vec<(u128, usize)>,
    pub pending: Vec<(Value, Value)>,
    pub key_ty: Type,
    pub value_ty: Type,
}

impl Value {
    /// Construct `Value::Int` from any integer kind that converts
    /// into a `BigInt`. Lets calls like `Value::int(42)` work
    /// without writing `BigInt::from(...)` at every site, and keeps
    /// the variant's storage type a private detail callers don't
    /// need to spell out.
    pub fn int(n: impl Into<BigInt>) -> Self {
        Value::Int(n.into())
    }

    /// Extract a borrowed `BigInt` if this is an `Int`. Returns
    /// `None` for any other variant.
    pub fn as_int(&self) -> Option<&BigInt> {
        match self { Value::Int(n) => Some(n), _ => None }
    }

    /// Default value for a given type — used when reading a state cell that
    /// was never written, and as the seed when constructing fresh structs.
    /// For `Type::Map`, returns the value-type's default (the map itself
    /// has no aggregate Value form; reads operate per-cell).
    pub fn default_for(ty: &Type) -> Value {
        match ty {
            Type::Int => Value::int(0),
            Type::I32 => Value::I32(0),
            Type::U32 => Value::U32(0),
            Type::U64 => Value::U64(0),
            Type::U128 => Value::U128(0),
            Type::Bool => Value::Bool(false),
            Type::Unit => Value::Unit,
            Type::Resource => Value::Resource(0),
            Type::String => Value::Str(String::new()),
            Type::Address => Value::Address(String::new()),
            Type::Bytes => Value::Bytes(Vec::new()),
            Type::Array(_) => Value::Array(Vec::new()),
            Type::Struct { name, fields } => Value::Struct {
                name: name.clone(),
                fields: fields
                    .iter()
                    .map(|(n, t)| (n.clone(), Value::default_for(t)))
                    .collect(),
            },
            Type::Map { value, .. } => Value::default_for(value),
            // The default *whole-pmap* is the empty HAMT — used when
            // reading the state cell for a never-written pmap. Per-key
            // defaults (when `state[key]` finds no entry) are computed
            // from V at the eval site, not here.
            Type::PMap { .. } => Value::PMap(crate::pmap::EMPTY),
            Type::PBTree { .. } => Value::PBTree(crate::pbtree::EMPTY),
            Type::Json => Value::Json(crate::json::Json::Null),
            Type::PVec { .. } => Value::PVec { len: 0, root: crate::pvec::EMPTY },
            // Default interface value — empty target_module,
            // dispatching against it is a runtime error. State
            // cells holding interfaces start unbound; the program
            // is expected to bind them via `IFace::bind(...)`
            // before dispatch.
            Type::Interface { name, .. } => Value::Interface {
                iface: name.clone(),
                target_module: String::new(),
            },
            Type::Set(_) => Value::Set(Vec::new()),
            Type::Dict { .. } => Value::Dict(Vec::new()),
            Type::Tuple(elems) => Value::Tuple(elems.iter().map(Value::default_for).collect()),
            Type::Enum { name, variants } => {
                // Default to the first variant with all-default payloads.
                // Mirrors Rust's `#[derive(Default)]` requiring a variant
                // to be tagged default — for rend, the convention is
                // "first variant is default".
                let (vname, payload_tys) = variants
                    .first()
                    .cloned()
                    .unwrap_or_else(|| ("".to_string(), Vec::new()));
                Value::Enum {
                    enum_name: name.clone(),
                    variant: vname,
                    payload: payload_tys.iter().map(Value::default_for).collect(),
                }
            }
            // A "default" cap is the all-zero scope. Reading an
            // uninitialized state cell yields this — a cap whose
            // values represent the most-restrictive interpretation
            // of every scope field (zero charges, zero ceiling, etc).
            Type::Cap { name, fields, .. } => Value::Struct {
                name: name.clone(),
                fields: fields
                    .iter()
                    .map(|(n, t)| (n.clone(), Value::default_for(t)))
                    .collect(),
            },
        }
    }
}

impl std::fmt::Display for Value {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Value::Int(n) => write!(f, "{n}"),
            Value::I32(n) => write!(f, "{n}i32"),
            Value::U32(n) => write!(f, "{n}u32"),
            Value::U64(n) => write!(f, "{n}u64"),
            Value::U128(n) => write!(f, "{n}u128"),
            Value::Bool(b) => write!(f, "{b}"),
            Value::Unit => write!(f, "()"),
            Value::Resource(n) => write!(f, "Resource({n})"),
            Value::Str(s) => write!(f, "\"{s}\""),
            Value::Address(s) => write!(f, "@{s}"),
            Value::Bytes(b) => {
                write!(f, "0x")?;
                for byte in b { write!(f, "{byte:02x}")?; }
                Ok(())
            }
            Value::Array(elems) => {
                write!(f, "[")?;
                for (i, v) in elems.iter().enumerate() {
                    if i > 0 { write!(f, ", ")?; }
                    write!(f, "{v}")?;
                }
                write!(f, "]")
            }
            Value::Set(elems) => {
                write!(f, "set{{")?;
                for (i, v) in elems.iter().enumerate() {
                    if i > 0 { write!(f, ", ")?; }
                    write!(f, "{v}")?;
                }
                write!(f, "}}")
            }
            Value::Dict(pairs) => {
                write!(f, "dict{{")?;
                for (i, (k, v)) in pairs.iter().enumerate() {
                    if i > 0 { write!(f, ", ")?; }
                    write!(f, "{k}: {v}")?;
                }
                write!(f, "}}")
            }
            Value::Struct { name, fields } => {
                write!(f, "{name}{{")?;
                for (i, (n, v)) in fields.iter().enumerate() {
                    if i > 0 { write!(f, ", ")?; }
                    write!(f, "{n}: {v}")?;
                }
                write!(f, "}}")
            }
            Value::Pending(id) => write!(f, "<pending #{id}>"),
            Value::Tuple(elems) => {
                write!(f, "(")?;
                for (i, v) in elems.iter().enumerate() {
                    if i > 0 { write!(f, ", ")?; }
                    write!(f, "{v}")?;
                }
                write!(f, ")")
            }
            Value::Enum { enum_name, variant, payload } => {
                write!(f, "{enum_name}::{variant}")?;
                if !payload.is_empty() {
                    write!(f, "(")?;
                    for (i, v) in payload.iter().enumerate() {
                        if i > 0 { write!(f, ", ")?; }
                        write!(f, "{v}")?;
                    }
                    write!(f, ")")?;
                }
                Ok(())
            }
            Value::PMap(hash) => write!(f, "pmap@{hash:x}"),
            Value::PBTree(hash) => write!(f, "pbtree@{hash:x}"),
            Value::Json(j) => write!(f, "{j}"),
            Value::PVec { len, root } => write!(f, "pvec[{len}]@{root:x}"),
            Value::Interface { iface, target_module } => {
                write!(f, "{iface}::bind(\"{target_module}\")")
            }
            Value::PMapCursor(_) => write!(f, "<pmap-cursor>"),
            Value::PBTreeCursor(_) => write!(f, "<pbtree-cursor>"),
        }
    }
}
