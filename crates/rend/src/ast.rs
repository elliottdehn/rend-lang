//! Abstract syntax tree.

use crate::token::Span;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Module {
    /// Optional `module <name>;` declaration at the top of the source. Used
    /// by `Engine::execute_modules` to group sources without out-of-band
    /// metadata. Single-module entry points may omit it (defaults to
    /// `"main"` when consulted by the engine).
    pub name: Option<String>,
    pub imports: Vec<Import>,
    pub states: Vec<StateDecl>,
    pub functions: Vec<FnDef>,
    pub structs: Vec<StructDecl>,
    /// `on Type fn name(e: Type) { ... }` event handlers declared at
    /// module top level. Each handler runs whenever the matching
    /// struct is emitted (anywhere in the loaded program); the
    /// scheduler drains them in declaration order at commit time.
    pub handlers: Vec<HandlerDecl>,
    pub modifiers: Vec<ModifierDecl>,
    pub enums: Vec<EnumDecl>,
    pub consts: Vec<ConstDecl>,
    pub caps: Vec<CapDecl>,
    pub interfaces: Vec<InterfaceDecl>,
    pub indexes: Vec<IndexDecl>,
}

/// `index` / `unique_index NAME on STATE.field1.field2;` — a derived
/// index relating two pmap state slots. The compiler auto-emits
/// maintenance code at every write to `STATE` so that the index
/// stays consistent.
///
/// **Unique** (`unique_index`): index slot is `pmap<F, K>`. Each
/// indexed field value maps to exactly one primary key. Re-writing
/// the same field value with a new primary key clobbers the prior.
/// Use for fields with a uniqueness invariant (email, owner, slug).
///
/// **Multi** (`index`): index slot is `pmap<F, [K]>`. Each indexed
/// field value maps to a list of primary keys. Maintenance dedups
/// on append, so re-writing the same record doesn't grow the list.
/// Use for grouping fields (department, status, owner-of-many).
///
/// Both have the slice-4 limitation: when an existing primary is
/// overwritten with a new value whose projected field differs from
/// the old one, the stale index entry is *not* removed. The new
/// entry is added correctly, but the old back-link lingers.
/// Insert-mostly workloads are unaffected; mutation-heavy workloads
/// will need a `pmap_remove`-aware update path (future slice).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexDecl {
    /// Name of the index state slot.
    pub name: String,
    /// Name of the primary pmap state slot the index covers.
    pub on_state: String,
    /// Projected fields. Single-entry for the legacy single-field
    /// shape (`STATE.email`); multi-entry for composite indexes
    /// (`STATE.(price ASC, time DESC, ...)`). For composite, the
    /// compiler packs the fields into a `bytes` key via
    /// `to_be_bytes` + `bit_not_bytes` (DESC) + `bytes_concat`,
    /// and the index slot must be `pbtree<bytes, _>`.
    pub fields: Vec<IndexField>,
    pub kind: IndexKind,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexField {
    /// Nested field path projected from the primary's value type.
    /// `["profile", "email"]` corresponds to `STATE.profile.email`.
    pub path: Vec<String>,
    pub direction: SortDirection,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortDirection { Asc, Desc }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexKind {
    Unique,
    Multi,
}

/// Module-level immutable binding: `const NAME: T = expr;`. The
/// expression is evaluated at every use site (functionally
/// equivalent to compile-time substitution); typeck verifies the
/// type matches the declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConstDecl {
    pub name: String,
    pub ty: Type,
    pub value: Expr,
    pub span: Span,
}

/// `enum Name { Unit, Tup(T1, T2), ... }` — sum-type declaration.
/// Variants are positional-tuple-style only for now (skip
/// named-field variants — wrap a struct in a single payload slot).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnumDecl {
    pub name: String,
    pub variants: Vec<EnumVariant>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnumVariant {
    pub name: String,
    pub payload: Vec<Type>,   // empty Vec = unit variant
    pub span: Span,
}

/// `modifier Name(params) { ... _; ... }` — Solidity-style wrappers
/// around fn bodies. Applied via `[Mod1(args), Mod2]` after a fn's
/// param list. Desugared into the fn body by `crate::modifier::expand`
/// before typeck runs, so typeck/affine/effects/etc. only ever see
/// already-expanded plain functions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModifierDecl {
    pub name: String,
    pub params: Vec<Param>,
    pub body: Block,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructDecl {
    pub name: String,
    pub fields: Vec<StructField>,
    /// `pub struct Foo { ... }` — marks the struct as referenceable
    /// from other modules via `m::Foo`. Bare `struct` is private:
    /// any cross-module reference fails typeck. Defaults to `false`.
    pub is_pub: bool,
    pub span: Span,
}

/// `on Foo fn handler(e: Foo) { ... }` — declares an event handler.
/// At commit time, every emitted `Foo` value is fed to this fn (and
/// every other handler bound to `Foo`) in stable declaration order.
/// The fn itself is an ordinary `FnDef` with a single parameter
/// whose type is the event struct; typeck enforces the shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandlerDecl {
    /// `None` for local handlers (`on Foo fn ...`), `Some(m)` for
    /// cross-module handlers (`on m::Foo fn ...`). Combined with
    /// `event_type` to form the dispatch key.
    pub event_module: Option<String>,
    /// Bare struct name (no module prefix). Identifies the event
    /// type within `event_module`.
    pub event_type: String,
    pub fn_def: FnDef,
    pub span: Span,
}

/// `interface Name { method-sig; ... }` — abstract API surface
/// usable as a value type. A binding (`Name::bind("module")`)
/// wraps a runtime module name with a static guarantee that the
/// module satisfies every method signature here. Calls are
/// written `$value::method(args)` and dispatch dynamically via
/// the bound module name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterfaceDecl {
    pub name: String,
    pub methods: Vec<InterfaceMethod>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterfaceMethod {
    pub name: String,
    pub params: Vec<Param>,
    pub return_type: Type,
    /// `view` or `pure` annotations on the interface method —
    /// the bound implementation must satisfy at least the same
    /// effect bound. (Today only `view` is meaningful here; we
    /// store both flags for symmetry with FnDef.)
    pub is_view: bool,
    pub is_pure: bool,
    pub span: Span,
}

/// `cap Name { f: T, ... }` — an unforgeable capability. Same shape
/// as a struct on the inside, but with two extra rules:
///   1. **Non-Copy** — a cap value is moved (affine), never duplicated.
///      Holding one is the proof of permission, so duplicating it
///      would defeat the model.
///   2. **Privileged construction** — a cap literal `Name { ... }` is
///      only legal inside the module that declares the cap. Outside
///      the declaring module, you can only obtain a cap by being
///      handed one (return value, struct field, state read).
///
/// At runtime caps are represented exactly like structs; the two
/// rules above are enforced statically by typeck + the affine pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapDecl {
    pub name: String,
    pub fields: Vec<StructField>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructField {
    pub name: String,
    pub ty: Type,
    /// `Some(group_name)` if this field was declared inside a
    /// `group <name> { ... }` block — sibling fields with the same
    /// group share one storage cell. `None` means the field has its
    /// own cell (default granular layout).
    pub group: Option<String>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Import {
    pub name: String,
    pub params: Vec<Type>,
    pub return_type: Type,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateDecl {
    pub name: String,
    pub ty: Type,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FnDef {
    pub name: String,
    pub params: Vec<Param>,
    pub return_type: Type,
    pub body: Block,
    /// `nore` (non-reentrant) marks a function that can't be called
    /// recursively from inside its own currently-executing invocation.
    /// The runtime maintains a per-tx HashSet of `(module, fn)` ids;
    /// re-entry fails with a runtime error. Useful for guarding
    /// transfer/withdraw flows from cross-module call-back attacks.
    pub is_nore: bool,
    /// `entry` marks a function as part of the module's public surface.
    /// Two host-boundary checks consult this flag:
    ///   1. The transaction's `main` may only call `entry` functions of any
    ///      module (local or cross). Internal helpers stay private to the
    ///      module's own non-main code.
    ///   2. Cross-module references must target `entry` functions; the link
    ///      step rejects calls to module-private helpers.
    /// Within a single module's non-main code, any function may call any
    /// other — `entry` is purely about the boundary, not the call graph.
    pub is_entry: bool,
    /// `view` declares the function may read state but never writes,
    /// emits, or calls anything that does. Verified by typeck against
    /// the effects classifier; mismatch is a compile error. The host
    /// can call view entries via `Engine::query` without OCC overhead.
    pub is_view: bool,
    /// `pure` is `view` plus "no state reads either". A pure fn is a
    /// deterministic transform of its arguments. Verified the same way.
    pub is_pure: bool,
    /// Applied modifiers, in declaration order (leftmost wraps
    /// outermost). Each entry is `(modifier_name, args)`. Cleared
    /// after `crate::modifier::expand` inlines them into `body`.
    pub modifiers: Vec<(String, Vec<Expr>)>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Param {
    pub name: String,
    pub ty: Type,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Type {
    /// `int` / `i64`. Default integer type; bare numeric literals
    /// like `42` have this type. Arbitrary precision — backed by
    /// `BigInt`. Use sized types (`i32`, `u32`, `u64`, `u128`) when
    /// fixed width matters.
    Int,
    /// `uint`. Arbitrary-precision non-negative integer. Literals
    /// with the `u` suffix (e.g. `42u`) have this type. Subtraction
    /// that would go negative is a runtime error.
    UInt,
    /// `float`. IEEE-754 double. Source literals with `.`, `e`, or
    /// `E` parse here. Arithmetic follows IEEE-754 — division by
    /// zero produces inf/nan rather than erroring.
    Float,
    I32,
    U32,
    U64,
    U128,
    Bool,
    Unit,
    Resource,
    String,
    Address,
    Bytes,
    Array(Box<Type>),
    /// In-memory set of unique elements — non-storage. For storage, build a
    /// `state map<T, bool>` instead.
    Set(Box<Type>),
    /// In-memory map — non-storage. For per-cell KV-backed storage, use
    /// the (state-only) `map<K, V>` type.
    Dict { key: Box<Type>, value: Box<Type> },
    Map { key: Box<Type>, value: Box<Type> },
    /// Persistent map. Same value semantics as `Map<K, V>` (state-only,
    /// `state[key]` indexed access), but the underlying KV layout is a
    /// HAMT: each tree node is its own content-addressed cell. The win
    /// is two-fold:
    ///  1. **Cheap metadata.** Size, contains-check, etc. live on the
    ///     root node and update atomically with each insert/remove.
    ///     `Map` can't support these without a full scan.
    ///  2. **OCC granularity.** Two transactions inserting into
    ///     disjoint subtrees touch disjoint cell sets, so they don't
    ///     conflict at commit time even though both modify "the same
    ///     pmap".
    PMap { key: Box<Type>, value: Box<Type> },
    /// Persistent **sorted** map. Same tree-spread storage as `PMap`,
    /// but the path bits come from the key's order-preserving byte
    /// encoding instead of a hash, so iteration is key-sorted and
    /// range queries (`pbtree_range`) only fetch cells covering the
    /// requested key range. Use for "ORDER BY" / "WHERE k BETWEEN"
    /// query shapes. Slice-1 supports `u64` keys only.
    PBTree { key: Box<Type>, value: Box<Type> },
    /// Persistent vector. Indexed by i64 (0..len). Same tree-spread
    /// storage strategy as pmap: each tree node is a content-
    /// addressed KV cell; the state cell carries (length, root_hash).
    /// Disjoint-index writes commit in parallel via the OCC merge
    /// path. `pvec_push` is a serial point — two concurrent pushes
    /// both want the same next index and so genuinely conflict.
    PVec { elem: Box<Type> },
    /// Dynamic JSON value (jsonb-shaped: parsed structure, not raw
    /// text). No declared schema; path access (`j -> "key"`,
    /// `j -> [n]`) returns another `json` value. Conversion to
    /// typed primitives goes through dedicated builtins
    /// (`json_to_string`, `json_to_i64`, etc.). "Buyer beware":
    /// missing keys yield `null`, type mismatches at conversion
    /// abort with a runtime error.
    Json,
    /// Named struct. Field types are inlined for self-contained Type values
    /// — typeck normalizes named references to fully-resolved types.
    Struct {
        name: String,
        fields: Vec<(String, Type)>,
        /// Parallel to `fields`: `field_groups[i]` is `Some(group_name)` if
        /// `fields[i]` belongs to a `group <name> { ... }` declaration and
        /// shares a cell with sibling fields under the same name. `None`
        /// means the field is granular (own cell). Empty `field_groups` is
        /// treated as all-`None` (uniform granular layout) so existing
        /// constructors that ignore groups continue to work.
        field_groups: Vec<Option<String>>,
    },
    /// Anonymous fixed-arity tuple. Local-only (not storable, not
    /// usable as a map key today). Used mostly for multi-value
    /// returns and destructuring `let`.
    Tuple(Vec<Type>),
    /// Named sum type. Variant payload types are inlined here so the
    /// Type value is self-contained (parallel with `Type::Struct`).
    Enum { name: String, variants: Vec<(String, Vec<Type>)> },
    /// Capability — struct-shaped, but always non-Copy and constructable
    /// only inside the module that declared it. The `owner_module` field
    /// records the declaring module so cross-module type references can
    /// be checked against the current module name during typeck.
    Cap { name: String, fields: Vec<(String, Type)>, owner_module: String },
    /// Interface — a runtime-bound module reference with a static
    /// guarantee that the bound module satisfies every listed
    /// method signature. The runtime value is a string (module
    /// name); dispatch happens via the existing module index.
    Interface { name: String, methods: Vec<InterfaceMethodSig> },
}

/// Method signature stored inside `Type::Interface`. Mirrors
/// `InterfaceMethod` from the AST but carries only what typeck +
/// runtime need (param types in order, return type, effect-ish
/// flags). Names are kept for error messages and dispatch.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct InterfaceMethodSig {
    pub name: String,
    pub params: Vec<Type>,
    pub return_type: Type,
    pub is_view: bool,
    pub is_pure: bool,
}

impl Type {
    /// Copy semantics: the value is duplicated (not moved) on each use.
    /// Non-Copy types (currently just Resource) participate in affine analysis.
    pub fn is_copy(&self) -> bool {
        match self {
            Type::Int | Type::UInt | Type::Float
            | Type::I32 | Type::U32 | Type::U64 | Type::U128
            | Type::Bool | Type::Unit | Type::String | Type::Address | Type::Bytes => true,
            Type::Array(elem) => elem.is_copy(),
            Type::Set(elem) => elem.is_copy(),
            Type::Dict { key, value } => key.is_copy() && value.is_copy(),
            Type::Struct { fields, .. } => fields.iter().all(|(_, t)| t.is_copy()),
            Type::Tuple(elems) => elems.iter().all(|t| t.is_copy()),
            Type::Enum { variants, .. } => variants
                .iter()
                .all(|(_, payload)| payload.iter().all(|t| t.is_copy())),
            // Caps are unconditionally non-Copy — duplicating one would
            // forge permission. Even a cap whose fields are all Copy
            // is moved.
            Type::Cap { .. } => false,
            // An interface value is just a module-name handle —
            // freely duplicable. Copy.
            Type::Interface { .. } => true,
            // JSON values are dynamic data with no ownership story —
            // freely duplicable like strings and bytes.
            Type::Json => true,
            _ => false,
        }
    }

    /// Whether values of this type may serve as a `map<K, _>` key.
    /// Composite types are keyable iff every component is keyable.
    pub fn is_keyable(&self) -> bool {
        match self {
            Type::Int | Type::I32 | Type::U32 | Type::U64 | Type::U128
            | Type::Bool | Type::String | Type::Address | Type::Bytes => true,
            Type::Array(elem) => elem.is_keyable(),
            Type::Struct { fields, .. } => fields.iter().all(|(_, t)| t.is_keyable()),
            Type::Enum { variants, .. } => variants
                .iter()
                .all(|(_, payload)| payload.iter().all(|t| t.is_keyable())),
            _ => false,
        }
    }
}

impl std::fmt::Display for Type {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Type::Int => write!(f, "int"),
            Type::UInt => write!(f, "uint"),
            Type::Float => write!(f, "float"),
            Type::I32 => write!(f, "i32"),
            Type::U32 => write!(f, "u32"),
            Type::U64 => write!(f, "u64"),
            Type::U128 => write!(f, "u128"),
            Type::Bool => write!(f, "bool"),
            Type::Unit => write!(f, "()"),
            Type::Resource => write!(f, "Resource"),
            Type::String => write!(f, "string"),
            Type::Address => write!(f, "Address"),
            Type::Bytes => write!(f, "bytes"),
            Type::Array(t) => write!(f, "[{t}]"),
            Type::Set(t) => write!(f, "set<{t}>"),
            Type::Dict { key, value } => write!(f, "dict<{key}, {value}>"),
            Type::Map { key, value } => write!(f, "map<{key}, {value}>"),
            Type::PMap { key, value } => write!(f, "pmap<{key}, {value}>"),
            Type::PBTree { key, value } => write!(f, "pbtree<{key}, {value}>"),
            Type::PVec { elem } => write!(f, "pvec<{elem}>"),
            Type::Json => write!(f, "json"),
            Type::Struct { name, .. } => write!(f, "{name}"),
            Type::Tuple(elems) => {
                write!(f, "(")?;
                for (i, t) in elems.iter().enumerate() {
                    if i > 0 { write!(f, ", ")?; }
                    write!(f, "{t}")?;
                }
                write!(f, ")")
            }
            Type::Enum { name, .. } => write!(f, "{name}"),
            Type::Cap { name, .. } => write!(f, "{name}"),
            Type::Interface { name, .. } => write!(f, "{name}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    pub stmts: Vec<Stmt>,
    /// Optional trailing expression (no terminating `;`). If present,
    /// the block's value is this expression's value; otherwise the
    /// block evaluates to `Unit`. Function bodies use this for
    /// implicit return; let-RHS / match-arm / if-arm contexts use it
    /// to compute a value from a multi-statement block.
    pub tail: Option<Box<Expr>>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stmt {
    Let { name: String, ty: Option<Type>, value: Expr, span: Span },
    /// Assignment to a state slot, map cell, or mutable local binding.
    /// Target must be one of:
    ///   - `Ident` of a `state` slot         → emits a `KvPut`
    ///   - `Ident` of a Copy-typed local      → mutates the local
    ///   - `Index { target: Ident, ... }` over a map state → emits `MapPut`
    Assign { target: Expr, value: Expr, span: Span },
    Return { value: Option<Expr>, span: Span },
    If(IfStmt),
    While { cond: Expr, body: Block, span: Span },
    For { var: String, iter: Expr, body: Block, span: Span },
    /// `for i in start..end { ... }` (or `..=` for inclusive). Lowered
    /// to a counter loop in compile/interp without materializing an
    /// actual array. start/end must be the same integer type.
    ForRange {
        var: String,
        start: Expr,
        end: Expr,
        inclusive: bool,
        body: Block,
        span: Span,
    },
    Break(Span),
    Continue(Span),
    Expr(Expr),
    /// `parallel { stmt; stmt; ... }` — every contained statement
    /// runs in its own shadow `Tx` under rayon; deltas merge back
    /// in stable declaration order with conflict re-run. `let`
    /// bindings inside escape the block into the enclosing scope.
    /// Intra-block references are rejected at typeck: each
    /// statement may read only names from the enclosing scope.
    Parallel { stmts: Vec<Stmt>, span: Span },
    /// `parallel for <id_var> in <source: [u64]> to <output: [T]> { body }`.
    /// Runs `body` once per element of `source` in parallel — each iteration
    /// in its own shadow `Tx`, with `id_var` bound to `source[idx]`. The
    /// body's tail expression evaluates to a value of `T` that the dispatcher
    /// writes to `output[idx]`; reaching `continue` skips the write (the
    /// slot stays at `T::default()`).
    ///
    /// Lengths of `source` and `output` are checked equal at runtime
    /// (typeck can't statically prove equality of two int_exprs). The
    /// output buffer's slot disjointness is structural: no two iterations
    /// ever touch the same `output[idx]`, so the merge has nothing to
    /// reconcile on the buffer side. State writes inside `body` still
    /// go through the shadow-Tx delta merge with conflict re-run.
    ParallelForTo {
        id_var: String,
        source: Expr,
        output: Expr,
        body: Block,
        span: Span,
    },
    /// `emit StructExpr;` — append the struct value to the tx event
    /// log and queue it for `on <Type>` handlers. Any expression
    /// resolving to a `Type::Struct` value is legal; a struct
    /// literal `Foo { ... }` is the common spelling.
    Emit { value: Box<Expr>, span: Span },
    /// `delete state[k];` — remove an entry from a `pmap` or
    /// `pbtree` state. The target must be an indexed expression on
    /// a state slot. Index back-links are auto-cleaned by the
    /// compiler at the same write site, mirroring how index
    /// maintenance happens on assignment.
    Delete { target: Expr, span: Span },
    /// `let (a, b, ...) = expr;` — destructure a tuple-typed value
    /// into multiple bindings in one statement. Each name is bound at
    /// the corresponding tuple index. Lowered to a temp + per-name
    /// `TupleIndex` reads in the compiler.
    LetTuple { names: Vec<String>, value: Expr, span: Span },
    /// `_;` — placeholder inside a `modifier` body that the wrapped
    /// fn body fills in at expansion time. Outside a modifier the
    /// expander rejects this; typeck never sees one.
    Placeholder(Span),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IfStmt {
    pub cond: Expr,
    pub then: Block,
    pub else_branch: ElseBranch,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ElseBranch {
    None,
    Block(Block),
    If(Box<IfStmt>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expr {
    pub kind: ExprKind,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExprKind {
    Int(num_bigint::BigInt),
    UInt(num_bigint::BigInt),
    Float(crate::value::F64Bits),
    I32(i32),
    U32(u32),
    U64(u64),
    U128(u128),
    /// JSON object literal: `{"a": 2, "b": expr, ...}`. Keys are
    /// static strings; values are general expressions evaluated at
    /// runtime. Produces a `Value::Json(Json::Object(...))` whose
    /// values are the evaluated rend Values.
    JsonObject(Vec<(String, Box<Expr>)>),
    /// JSON array literal — heterogeneous, distinct from rend's
    /// typed `[T]` arrays. Only appears as a value inside a JSON
    /// literal context. Produces `Value::Json(Json::Array(...))`.
    JsonArray(Vec<Box<Expr>>),
    /// JSON `null` literal. Only meaningful inside a JSON literal
    /// context; produces `Value::Json(Json::Null)`.
    JsonNull,
    Bool(bool),
    Str(String),
    Ident(String),
    Binary { op: BinOp, lhs: Box<Expr>, rhs: Box<Expr> },
    Unary { op: UnOp, operand: Box<Expr> },
    Call { module: Option<String>, name: String, args: Vec<Expr> },
    /// `$target::method(args)` — dynamic dispatch through an
    /// interface value. `target_ident` names a binding (local
    /// or state) of `Type::Interface`; the method is looked up
    /// in the interface declaration at typeck time, and at
    /// runtime the call dispatches into the bound module.
    ///
    /// `method_is_view` / `method_is_pure` are populated by
    /// typeck from the interface method's declared effect bound.
    /// The effects classifier reads them to compute a tighter
    /// classification than the conservative Impure default.
    DynCall {
        target_ident: String,
        method: String,
        args: Vec<Expr>,
        method_is_view: bool,
        method_is_pure: bool,
    },
    Index { target: Box<Expr>, key: Box<Expr> },
    Array(Vec<Expr>),
    StructLit { name: String, fields: Vec<(String, Expr)> },
    Field { target: Box<Expr>, name: String },
    /// Pipe stage: `head |> step` evaluates `head`, binds its value to `$$`
    /// for the duration of `step`, and yields whatever `step` produces.
    /// Chains are left-associative — `a |> b |> c` parses as `(a |> b) |> c`.
    Pipe { head: Box<Expr>, step: Box<Expr> },
    /// `$$` — refers to the result of the most-recently-bound pipe stage.
    /// Lexically valid only inside the right-hand side of a `|>`.
    Prev,
    /// List comprehension. Clauses run outermost-first; each `For` introduces
    /// a new binding visible to all later clauses and the mapper. `If` clauses
    /// short-circuit the current iteration. The clause list is non-empty and
    /// always begins with a `For`.
    ListComp { mapper: Box<Expr>, clauses: Vec<CompClause> },
    SetLit(Vec<Expr>),
    SetComp { mapper: Box<Expr>, clauses: Vec<CompClause> },
    DictLit(Vec<(Expr, Expr)>),
    DictComp { key: Box<Expr>, value: Box<Expr>, clauses: Vec<CompClause> },
    /// Tuple literal: `(a, b, c)`. Distinguished from a parenthesized
    /// expression by the presence of at least one comma.
    TupleLit(Vec<Expr>),
    /// Indexed access on a tuple: `t.0`, `t.1`.
    TupleIndex { target: Box<Expr>, index: usize },
    /// `EnumName::Variant` (no payload) or `EnumName::Variant(args)`.
    EnumCtor { enum_name: String, variant: String, args: Vec<Expr> },
    /// `match scrut { pat => arm, ... }` — sum-type dispatch.
    /// Each arm produces the match expression's value when its
    /// pattern fires; all arms must have the same type. Typeck
    /// enforces exhaustiveness over the scrutinee's enum.
    Match { scrut: Box<Expr>, arms: Vec<MatchArm> },
    /// Block-as-expression: `{ stmt; stmt; tail_expr }`. The tail is
    /// optional — without it, the block evaluates to `Unit`. Used as
    /// a let-RHS, match-arm body, if-arm body, or anywhere an
    /// expression is expected.
    Block(Block),
    /// `if cond { ... } else { ... }` as an expression. Both arms are
    /// required and must produce the same type. Statement-position
    /// `if` (with or without else) parses as `Stmt::If`; the
    /// expression form parses here when `if` appears in expression
    /// position (let-RHS, match arm, fn-body tail, etc.).
    If {
        cond: Box<Expr>,
        then: Block,
        else_branch: ElseBranch,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchArm {
    pub pattern: MatchPattern,
    pub body: Expr,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MatchPattern {
    /// `_` — matches any value.
    Wildcard,
    /// `EnumName::Variant` or `EnumName::Variant(b1, b2, ...)`.
    /// `bindings` carries names for each payload position; an empty
    /// vec is the unit-variant case.
    EnumVariant {
        enum_name: String,
        variant: String,
        bindings: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompClause {
    For { var: String, iter: Expr },
    If(Expr),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add, Sub, Mul, Div, Mod,
    Eq, NotEq, Lt, Gt, LtEq, GtEq,
    And, Or,
    BitAnd, BitOr, BitXor, Shl, Shr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnOp {
    Neg,
    Not,
}
