//! Bytecode types. A `BcModule` is the compile output consumed by the VM.

use std::collections::HashMap;

use crate::ast::{BinOp, Type, UnOp};

#[derive(Debug, Clone)]
pub struct BcModule {
    pub name: String,
    pub imports: Vec<String>,
    pub import_index: HashMap<String, usize>,
    /// Per-state metadata. Index → (compile-time u128 root, declared type).
    /// For map states, `ty` is `Type::Map { key, value }` and individual cell
    /// keys are derived at runtime from the root + the encoded key.
    pub state_roots: Vec<u128>,
    pub state_types: Vec<Type>,
    pub state_defaults: Vec<crate::value::Value>,
    pub state_names: Vec<String>,
    pub state_index: HashMap<String, usize>,
    pub struct_shapes: Vec<StructShape>,
    pub struct_index: HashMap<String, usize>,
    pub functions: Vec<BcFn>,
    pub fn_index: HashMap<String, usize>,
    /// Pre-computed leaf cell descriptors for struct field paths into states.
    /// Each path is `(state_idx, [field_names])`; the resolved leaf key and
    /// type are baked in so the VM can issue a single granular cell access.
    pub path_specs: Vec<PathSpec>,
    /// Declared events. The VM's `Emit` instruction references one of
    /// these by index; the runtime stamps each emission with the
    /// module name so the host log records `module::EventName`.
    pub events: Vec<EventShape>,
    pub event_index: HashMap<String, usize>,
    pub enum_shapes: Vec<EnumShape>,
    pub enum_index: HashMap<String, usize>,
    /// Interface declarations carried in the artifact so a tx
    /// compiled against this module as a dep can reference its
    /// interfaces by name (e.g., `IERC20::bind("usd")` when
    /// `IERC20` was declared in this module).
    pub interfaces: Vec<crate::ast::InterfaceDecl>,
    pub interface_index: HashMap<String, usize>,
}

/// Bytecode-side description of an event: name + ordered param names.
/// Param types live in the AST EventDecl, not here, since at runtime
/// we don't need to type-check (typeck already validated).
#[derive(Debug, Clone)]
pub struct EventShape {
    pub name: String,
    pub param_names: Vec<String>,
}

/// Pre-computed descriptor for a state field path. The compiler walks the
/// state's struct type, derives the leaf KV key by chaining
/// `child(parent_key, field_name)` for each step, and stores the leaf type
/// for granular reads/writes.
#[derive(Debug, Clone)]
pub struct PathSpec {
    pub leaf_key: u128,
    pub leaf_type: Type,
}

/// Bytecode-side description of a struct: the name and the ordered field names.
/// Used by `MakeStruct` to attach names to values at construction.
#[derive(Debug, Clone)]
pub struct StructShape {
    pub name: String,
    pub field_names: Vec<String>,
}

/// Bytecode-side description of an enum: the type name and ordered
/// variant names. The variant *index* is assigned by position;
/// `MakeEnum` and `EnumTag` operate on these indices.
#[derive(Debug, Clone)]
pub struct EnumShape {
    pub name: String,
    pub variant_names: Vec<String>,
}

impl BcModule {
    pub fn root_of(&self, name: &str) -> Option<u128> {
        self.state_index.get(name).map(|i| self.state_roots[*i])
    }
}

#[derive(Debug, Clone)]
pub struct BcFn {
    pub name: String,
    pub n_params: u16,
    pub n_regs: u16,
    pub code: Vec<Instr>,
    pub consts: Vec<Const>,
    pub is_entry: bool,
    /// Non-reentrant guard. The VM checks `(module_idx, fn_idx)`
    /// against `Tx::nore_active` on entry; re-entry aborts the tx.
    pub is_nore: bool,
    /// Pre-baked clusters of independent state reads, populated by the
    /// `optimize` pass. The runtime uses `Kv::get_many` to issue one
    /// batched round-trip per cluster instead of N sequential reads.
    /// Empty until optimization runs.
    pub read_groups: Vec<Vec<ReadOp>>,
    /// Declared parameter types. Persisted in the artifact so a
    /// downstream module compiling *against* this artifact can
    /// typecheck `module::entry(args)` calls. Populated for every
    /// function (the runtime doesn't need them, but cross-artifact
    /// compile does).
    pub param_types: Vec<Type>,
    /// Declared return type. Same role as `param_types`.
    pub return_type: Type,
    /// `view` declaration: function may read state but not write
    /// or emit. Persisted so `Engine::query` can recognize a tx as
    /// safe to run on the read-only path.
    pub is_view: bool,
    /// `pure` declaration: stricter `view` — no state reads either.
    pub is_pure: bool,
}

/// A single state read inside a batched group.
#[derive(Debug, Clone, Copy)]
pub enum ReadOp {
    /// Reads `module.state_types[key_idx]` from `module.state_roots[key_idx]`
    /// into register `dst`. Mirrors `Instr::KvGet`'s semantics.
    State { dst: u16, key_idx: u16 },
    /// Reads the leaf described by `module.path_specs[path_idx]` into
    /// register `dst`. Mirrors `Instr::KvGetPath`.
    Path { dst: u16, path_idx: u16 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Const {
    Int(num_bigint::BigInt),
    UInt(num_bigint::BigInt),
    I32(i32),
    U32(u32),
    U64(u64),
    U128(u128),
    Bool(bool),
    Str(String),
}

/// Register-based bytecode. All instruction operands are register indices into
/// the current frame; `consts[idx]` for `LoadConst`; jumps are relative to the
/// instruction *after* the jump (PC has already been incremented).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Instr {
    LoadConst { dst: u16, idx: u16 },
    Move { dst: u16, src: u16 },
    Bin { op: BinOp, dst: u16, lhs: u16, rhs: u16 },
    Un { op: UnOp, dst: u16, src: u16 },
    Jump { offset: i32 },
    JumpIfFalse { cond: u16, offset: i32 },
    Call { dst: u16, fn_idx: u16, args_start: u16, n_args: u8 },
    CallHost { dst: u16, import_idx: u16, args_start: u16, n_args: u8 },
    /// Cross-module call. Module + function names are looked up in the
    /// per-function constant pool. Validated by the engine at link time
    /// (entry-only) and resolved at runtime.
    ///
    /// `is_view` / `is_pure` are baked in by the compiler from the
    /// callee's declared effect bound — the optimizer uses them to
    /// decide whether this call breaks a read cluster. They're advisory
    /// at runtime (the VM ignores them). When the callee's flags can't
    /// be resolved at compile time (cross-artifact link with no manifest),
    /// they default to `false` so the optimizer pessimistically fences.
    CallExternal {
        dst: u16,
        module_name_idx: u16,
        fn_name_idx: u16,
        args_start: u16,
        n_args: u8,
        is_view: bool,
        is_pure: bool,
    },
    /// `IFace::bind(target_module)` constructor. Wraps a string in
    /// `target_reg` (the bound module name) into a `Value::Interface`
    /// with `iface` set to the constant-pooled interface name.
    MakeInterface { dst: u16, iface_name_idx: u16, target_reg: u16 },
    /// Dynamic-dispatch cross-module call. The module name comes from
    /// `target_reg` at runtime (read off a `Value::Interface`); the
    /// function name is in the constant pool. Used to lower
    /// `$ident::method(args)`. The runtime looks the bound module up
    /// in the world's module index and falls back to a runtime error
    /// if it isn't loaded.
    ///
    /// `is_view` / `is_pure` are baked in by the compiler from the
    /// interface method's declared effect bound — the optimizer
    /// uses them to decide whether this call breaks a read cluster.
    /// They're advisory at runtime (the VM ignores them).
    CallExternalDyn {
        dst: u16,
        target_reg: u16,
        fn_name_idx: u16,
        args_start: u16,
        n_args: u8,
        is_view: bool,
        is_pure: bool,
    },
    Return { src: u16 },
    ReturnUnit,
    BuiltinResource { dst: u16, src: u16 },
    BuiltinUnwrap { dst: u16, src: u16 },
    BuiltinAddress { dst: u16, src: u16 },
    BuiltinLen { dst: u16, src: u16 },
    /// Numeric conversion. `target` encodes the destination integer type:
    /// 0=i64, 1=i32, 2=u32, 3=u64, 4=u128. Source register holds any int
    /// type; the runtime bounds-checks.
    Convert { dst: u16, src: u16, target: u8 },
    MakeArray { dst: u16, args_start: u16, n: u16 },
    ArrayGet { dst: u16, arr: u16, idx: u16 },
    ArrayAppend { dst: u16, arr: u16, elem: u16 },
    MakeSet { dst: u16, args_start: u16, n: u16 },
    MakeDict { dst: u16, args_start: u16, n_pairs: u16 },
    /// Generic name-dispatched builtin call. Used for collection ops that
    /// would otherwise require a dozen specific instructions.
    BuiltinCall { dst: u16, name_idx: u16, args_start: u16, n_args: u8 },
    /// Build a struct from the registers `args_start..args_start+n` paired
    /// with field names taken from the struct shape at `shape_idx`.
    MakeStruct { dst: u16, shape_idx: u16, args_start: u16, n: u16 },
    /// Read a field by name. `name_idx` is into the function's constant
    /// pool. Runtime does a small name-lookup on the struct value.
    FieldGet { dst: u16, src: u16, name_idx: u16 },
    /// In-place replace a field of the struct in `dst` with the value in
    /// `val`. Used by `assign_path` lowering for `obj.field = ...` to
    /// rebuild the struct one level at a time.
    FieldSet { dst: u16, name_idx: u16, val: u16 },
    KvGet { dst: u16, key_idx: u16 },
    KvPut { src: u16, key_idx: u16 },
    /// Granular read of a single leaf cell beneath a struct state. The path
    /// is pre-computed at compile time; see `PathSpec`.
    KvGetPath { dst: u16, path_idx: u16 },
    KvPutPath { src: u16, path_idx: u16 },
    /// Batched state read. Issues all reads in the indexed group via a
    /// single `Kv::get_many`, scattering results into the per-op
    /// destination registers. Inserted by the `optimize` pass.
    ReadBatch { group_idx: u16 },
    /// Loop-prefetch hint. Emitted before a `for x in arr` loop (or
    /// comprehension over `arr`) when the body's only state reads are
    /// keyed by `x`. The runtime walks `arr_reg`'s elements, derives
    /// each cell key for `state_idx`, and warms the tx read-cache via
    /// one `Kv::get_many` so the per-iteration `MapGet`s hit cache
    /// instead of issuing N round-trips. Inserted by the `prefetch`
    /// pass during compilation.
    PrefetchMap { arr_reg: u16, state_idx: u16 },
    /// `emit Foo(arg1, arg2);` — append an entry to the tx event log.
    /// Forces every arg (the host receives concrete values) and
    /// stamps the entry with the current module's name.
    Emit { event_idx: u16, args_start: u16, n_args: u8 },
    /// Read a tx-context field. `kind`: 0 = msg_sender (Address),
    /// 1 = block_timestamp (u64), 2 = block_number (u64).
    Context { dst: u16, kind: u8 },
    /// Build a tuple from `n` consecutive registers starting at
    /// `args_start`. Tuples are anonymous fixed-arity products.
    MakeTuple { dst: u16, args_start: u16, n: u16 },
    /// Read element `index` from the tuple in `src`.
    TupleGet { dst: u16, src: u16, index: u16 },
    /// Construct an enum value: variant index `variant_idx` of enum
    /// shape `shape_idx`, with `n` payload registers from `args_start`.
    MakeEnum { dst: u16, shape_idx: u16, variant_idx: u16, args_start: u16, n: u16 },
    /// Extract the variant index of `src` (an enum) into `dst` as i64.
    /// Used by `match` lowering to dispatch on the active variant.
    EnumTag { dst: u16, src: u16 },
    /// Extract `payload[index]` from the enum in `src`.
    EnumPayload { dst: u16, src: u16, index: u16 },
    MapGet { dst: u16, state_idx: u16, key_reg: u16 },
    MapPut { state_idx: u16, key_reg: u16, src: u16 },
    /// Persistent-map read. The state slot stores a serialized HAMT;
    /// the runtime fetches it, walks for `key_reg`, and writes the
    /// matching value into `dst` (or the V-default if absent).
    PMapGet { dst: u16, state_idx: u16, key_reg: u16 },
    /// Persistent-map write. Read the HAMT, apply the insert, write
    /// the new HAMT back. The HAMT's structural sharing keeps the
    /// rewrite cost O(log32 N) — only the path from root to the new
    /// leaf is rebuilt.
    PMapPut { state_idx: u16, key_reg: u16, src: u16 },
    /// Persistent-map membership test. Walks the HAMT to find `key`;
    /// returns a Bool. O(log32 N) cell reads, just like `PMapGet`.
    PMapContains { dst: u16, state_idx: u16, key_reg: u16 },
    /// Persistent-vector indexed read. Walks the trie and writes
    /// the element at index `idx_reg` into `dst`. Out-of-bounds
    /// is a runtime error.
    PVecGet { dst: u16, state_idx: u16, idx_reg: u16 },
    /// Persistent-vector indexed write. Replaces the element at
    /// `idx_reg`. Out-of-bounds is a runtime error.
    PVecSet { state_idx: u16, idx_reg: u16, src: u16 },
    /// Persistent-vector append. Stores the new index (the prior
    /// length) into `dst`; updates the state cell's (length, root).
    PVecPush { dst: u16, state_idx: u16, src: u16 },
    /// Persistent-vector length, read off the state cell. O(1).
    PVecLen { dst: u16, state_idx: u16 },
    /// Walk the entire HAMT and materialize an array of `(K, V)`
    /// tuples. The order is hash-of-key — deterministic but not
    /// user-meaningful; callers that need sorted iteration sort the
    /// resulting array themselves. Cost: O(N) cell reads.
    PMapEntries { dst: u16, state_idx: u16 },
    /// Same walk as `PMapEntries`, but only the keys.
    PMapKeys { dst: u16, state_idx: u16 },
    /// Same walk as `PMapEntries`, but only the values.
    PMapValues { dst: u16, state_idx: u16 },
    /// Materialize the entire `pvec` into a fresh array, in index
    /// order. Cost: O(N) cell reads.
    PVecToArray { dst: u16, state_idx: u16 },
    /// Multi-index maintenance: read the array currently stored at
    /// `pmap[key]`, append `elem` if not already present (linear
    /// scan; dedup), and write back. Used by the compile pass to
    /// emit auto-maintenance for `index NAME on PRIMARY.field`
    /// declarations whose kind is Multi.
    PMapAppendUnique { state_idx: u16, key_reg: u16, elem_reg: u16 },
    /// Unique-index maintenance: read pmap[key]; if a value already
    /// exists there AND it differs from `src`, abort the tx with a
    /// unique-constraint violation. Otherwise write `pmap[key] = src`.
    /// Idempotent for re-writes of the same `(key, src)` pair.
    /// Used by the compile pass for `unique_index NAME on PRIMARY.field`.
    PMapPutUnique { state_idx: u16, key_reg: u16, src: u16 },
    /// Initialize a streaming HAMT cursor over a `pmap` state. The
    /// cursor lives in `dst` as a transient `Value::PMapCursor`
    /// holding the walk stack. Subsequent `PMapWalkNext` instructions
    /// advance it. Used by the compile pass to lower
    /// `for x in pmap_state { ... }` without materializing the
    /// whole map up front.
    PMapWalkInit { dst: u16, state_idx: u16 },
    /// Advance a streaming HAMT cursor by one leaf entry. If the
    /// walk is exhausted, jump by `end_offset`. Otherwise write the
    /// next value into `value_reg` and fall through. Cells are
    /// fetched lazily as the walk descends — `break` after the
    /// first match never visits the rest of the tree.
    PMapWalkNext { cursor_reg: u16, value_reg: u16, end_offset: i32 },
    /// Sorted-trie point read. Same shape as `PMapGet` but routes
    /// through the `pbtree` module so the trie path uses
    /// order-preserving key bytes.
    PBTreeGet { dst: u16, state_idx: u16, key_reg: u16 },
    /// Sorted-trie write. Same shape as `PMapPut`.
    PBTreePut { state_idx: u16, key_reg: u16, src: u16 },
    /// Sorted-trie membership check. Returns Bool.
    PBTreeContains { dst: u16, state_idx: u16, key_reg: u16 },
    /// Range query: materialize values whose keys fall in
    /// `[lo_reg, hi_reg]` (inclusive) into a fresh array. Walks
    /// only the subtrees that overlap the range; out-of-range
    /// subtrees stay unfetched.
    PBTreeRange { dst: u16, state_idx: u16, lo_reg: u16, hi_reg: u16 },
    /// Initialize a streaming sorted-trie cursor. Same role as
    /// `PMapWalkInit` but yields entries in key-sorted order.
    PBTreeWalkInit { dst: u16, state_idx: u16 },
    /// Advance a streaming sorted-trie cursor by one leaf entry.
    /// Same shape as `PMapWalkNext`.
    PBTreeWalkNext { cursor_reg: u16, value_reg: u16, end_offset: i32 },
    /// Sorted-index unique-constraint write. Same semantics as
    /// `PMapPutUnique` but routes through the pbtree module.
    /// Used when an `unique_index` slot is declared as
    /// `pbtree<F, K>` instead of `pmap<F, K>`.
    PBTreePutUnique { state_idx: u16, key_reg: u16, src: u16 },
    /// Sorted multi-index maintenance. Same as `PMapAppendUnique`
    /// but routed through pbtree. Used when an `index` slot is
    /// declared as `pbtree<F, [K]>` instead of `pmap<F, [K]>`.
    PBTreeAppendUnique { state_idx: u16, key_reg: u16, elem_reg: u16 },
    /// `delete state[k]` for a pmap state. Reads the existing
    /// value first (so the compiler can emit index back-link
    /// cleanup before the primary write), then calls
    /// `pmap::remove` and writes the new root.
    PMapDelete { state_idx: u16, key_reg: u16 },
    /// `delete state[k]` for a pbtree state. Same shape; routes
    /// through `pbtree::remove`.
    PBTreeDelete { state_idx: u16, key_reg: u16 },
    /// Conditional unique-index back-link removal. Read
    /// `state[key_reg]`; if it equals `expected_reg`, remove the
    /// entry. Otherwise no-op (the entry was overwritten by a
    /// field-change update and points at someone else now).
    PMapRemoveUnique { state_idx: u16, key_reg: u16, expected_reg: u16 },
    /// Multi-index back-link removal. Read the array at
    /// `state[key_reg]`, filter out `elem_reg`, write back. If the
    /// list becomes empty, remove the entry entirely.
    PMapRemoveFromList { state_idx: u16, key_reg: u16, elem_reg: u16 },
    /// Pbtree-backed equivalents of the two ops above.
    PBTreeRemoveUnique { state_idx: u16, key_reg: u16, expected_reg: u16 },
    PBTreeRemoveFromList { state_idx: u16, key_reg: u16, elem_reg: u16 },
    /// Increment an integer register in place by 1, preserving its int
    /// type. Emitted by the range-loop lowering so the increment matches
    /// `u32` / `u64` / etc. counters without a separate typed-`1` const.
    IncReg { reg: u16 },
}
