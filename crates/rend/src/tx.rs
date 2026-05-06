//! Per-execution transaction. Tracks observed reads and pending writes
//! against an immutable snapshot of the KV. The accumulated
//! `(read_set, write_set)` enables OCC-style concurrent execution: at commit
//! time the host re-fetches each read key, validates the observed value still
//! holds, and applies the writes.

use std::collections::HashMap;

use crate::ast::Type;
use crate::kv::Kv;
use crate::serialize::deserialize;
use crate::value::Value;

/// Per-transaction context the host populates before invoking the VM.
/// Holds the values exposed via `msg_sender()`, `block_timestamp()`,
/// and `block_number()` builtins. Defaults to a zero-address sender +
/// timestamp 0 + block 0; production hosts should supply the real
/// authenticated sender and chain-context numbers.
#[derive(Debug, Clone)]
pub struct TxContext {
    pub sender: Value,
    pub block_timestamp: u64,
    pub block_number: u64,
}

impl Default for TxContext {
    fn default() -> Self {
        Self {
            sender: Value::Address(String::new()),
            block_timestamp: 0,
            block_number: 0,
        }
    }
}

/// One log entry produced by a `Stmt::Emit`. The `module` field is
/// populated by the VM at emission time so the host can tell where
/// each event came from in a multi-module run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmittedEvent {
    pub module: String,
    pub name: String,
    pub args: Vec<Value>,
}

pub struct Tx<'a> {
    kv: &'a dyn Kv,
    /// First-observed value at each read key. Used by the host for OCC
    /// validation; the value's type tag is preserved so re-reads can be
    /// canonically re-serialized for byte comparison if desired.
    reads: HashMap<u128, Value>,
    /// Pending writes; later writes for the same key overwrite earlier ones.
    /// Reads of a written key see the in-flight value (read-your-writes).
    writes: HashMap<u128, Value>,
    /// When true, primitive `read_typed` calls queue the read and
    /// return a `Value::Pending(handle)` instead of going to the KV.
    /// `force` (called at consume points) flushes the queue with one
    /// `Kv::get_many` and substitutes the resolved values.
    lazy: bool,
    /// Queued reads awaiting resolution. Only used when `lazy` is on.
    pending: Vec<PendingRead>,
    /// Maps `Value::Pending(id)` → resolved value, populated at flush.
    resolved: HashMap<u64, Value>,
    /// Monotonic id allocator for pending handles.
    next_handle: u64,
    /// Append-only log of `emit` records produced during this tx.
    /// Returned to the host alongside reads/writes; never written
    /// back to the KV.
    events: Vec<EmittedEvent>,
    /// Host-provided context (sender, timestamp, block).
    context: TxContext,
    /// Set of `(module_idx, fn_idx)` currently executing under `nore`.
    /// On entry to a `nore` fn, we check this set; if already present,
    /// abort with a re-entry error. Cleared on normal return.
    nore_active: std::collections::HashSet<(u32, u32)>,
    /// For each pmap state cell touched, the K and V types of the
    /// declared `pmap<K,V>`. Recorded by PMapGet/PMapPut so the OCC
    /// validator can run a 3-way HAMT merge on root-pointer
    /// conflicts without re-reading the module schema. Only present
    /// for cells the tx actually touched.
    pmap_types: HashMap<u128, (Type, Type)>,
    /// Same idea as `pmap_types`, but for `pvec<T>` state cells —
    /// records the element type so the OCC merge can deserialize
    /// leaf bytes during a 3-way pvec merge.
    pvec_types: HashMap<u128, Type>,
    /// Cell keys written as pmap/pvec tree-node bytes. The GC
    /// driver uses this as its candidate set: anything in here that
    /// isn't reachable from a live root after a commit can be
    /// reclaimed.
    node_cells_written: std::collections::HashSet<u128>,
}

#[derive(Debug, Clone)]
struct PendingRead {
    id: u64,
    key: u128,
    ty: Type,
}

impl<'a> Tx<'a> {
    pub fn new(kv: &'a dyn Kv) -> Self {
        Self::new_with_context(kv, TxContext::default())
    }

    pub fn new_with_context(kv: &'a dyn Kv, context: TxContext) -> Self {
        Self {
            kv,
            reads: HashMap::new(),
            writes: HashMap::new(),
            lazy: false,
            pending: Vec::new(),
            resolved: HashMap::new(),
            next_handle: 0,
            events: Vec::new(),
            context,
            nore_active: std::collections::HashSet::new(),
            pmap_types: HashMap::new(),
            pvec_types: HashMap::new(),
            node_cells_written: std::collections::HashSet::new(),
        }
    }

    /// Record that `cell` was written as a persistent-collection
    /// node (pmap or pvec). Tracked in a separate set so GC has a
    /// sweep candidate list that's distinct from the regular write
    /// set — node cells are content-addressed and may be orphaned
    /// when a state's root changes.
    pub fn record_node_cell_write(&mut self, cell: u128) {
        self.node_cells_written.insert(cell);
    }

    pub fn node_cells_written(&self) -> &std::collections::HashSet<u128> {
        &self.node_cells_written
    }

    /// Record the declared K/V types of a pmap state cell. Looked up
    /// later by the OCC merge path; idempotent if called more than
    /// once for the same cell.
    pub fn record_pmap_types(&mut self, cell: u128, key_ty: Type, val_ty: Type) {
        self.pmap_types.entry(cell).or_insert((key_ty, val_ty));
    }

    /// Snapshot of the pmap-type table. Returned alongside
    /// reads/writes so the OCC layer can run a 3-way merge.
    pub fn pmap_types(&self) -> &HashMap<u128, (Type, Type)> {
        &self.pmap_types
    }

    /// Record the declared element type of a pvec state cell.
    pub fn record_pvec_type(&mut self, cell: u128, elem_ty: Type) {
        self.pvec_types.entry(cell).or_insert(elem_ty);
    }

    pub fn pvec_types(&self) -> &HashMap<u128, Type> {
        &self.pvec_types
    }

    pub fn context(&self) -> &TxContext { &self.context }

    /// Record entry into a `nore` (non-reentrant) function. Returns
    /// true if the fn was not already on the call stack; false if it
    /// was — in which case the caller should abort with a re-entry
    /// error. The exit path calls `nore_exit`.
    pub fn nore_enter(&mut self, module_idx: u32, fn_idx: u32) -> bool {
        self.nore_active.insert((module_idx, fn_idx))
    }

    pub fn nore_exit(&mut self, module_idx: u32, fn_idx: u32) {
        self.nore_active.remove(&(module_idx, fn_idx));
    }

    /// Append an emitted event. Caller is responsible for forcing any
    /// Pending values in `args` first; the log carries fully-resolved
    /// values so the host can hand them straight to subscribers.
    pub fn emit(&mut self, event: EmittedEvent) {
        self.events.push(event);
    }

    /// Switch on lazy reads. The bytecode VM enables this so primitive
    /// reads queue up and batch via `force`. The tree-walk interpreter
    /// leaves it off — it's a reference implementation and benefits less
    /// from the extra bookkeeping.
    pub fn set_lazy(&mut self, lazy: bool) {
        self.lazy = lazy;
    }

    /// Resolve every `Value::Pending` (reachable through arrays/sets/
    /// dicts/struct fields) by flushing any outstanding queued reads
    /// in one `Kv::get_many` and substituting handles with their
    /// concrete values. Idempotent on values without Pending leaves.
    pub fn force(&mut self, v: Value) -> Value {
        if !contains_pending(&v) { return v; }
        if !self.pending.is_empty() {
            self.flush_pending();
        }
        substitute_resolved(v, &self.resolved)
    }

    /// Shallow force: resolve only if the value is a top-level
    /// `Value::Pending` (so the caller can inspect its shape — Array,
    /// Struct, etc.). Nested Pending leaves stay deferred so they can
    /// continue propagating through copy-only operations and only
    /// flush at a real consume site.
    pub fn force_spine(&mut self, v: Value) -> Value {
        if let Value::Pending(_) = v {
            return self.force(v);
        }
        v
    }

    /// Flush all outstanding queued reads as a single `Kv::get_many`.
    /// Populates `self.reads` (so the OCC read set sees them) and
    /// `self.resolved` (so `force` can substitute). Safe to call when
    /// the queue is empty; cheap no-op in that case.
    pub fn flush_pending(&mut self) {
        if self.pending.is_empty() { return; }
        let pending = std::mem::take(&mut self.pending);
        let keys: Vec<u128> = pending.iter().map(|p| p.key).collect();
        let bytes = self.kv.get_many(&keys);
        for (p, bz) in pending.into_iter().zip(bytes.into_iter()) {
            let v = match bz {
                Some(b) => crate::serialize::deserialize(&b, &p.ty)
                    .unwrap_or_else(|| Value::default_for(&p.ty)),
                None => Value::default_for(&p.ty),
            };
            self.reads.insert(p.key, v.clone());
            self.resolved.insert(p.id, v);
        }
    }

    /// Read the value at `key` interpreted as `ty`. If neither the write set
    /// nor the read cache has it, the underlying KV is consulted; on a miss
    /// (or a deserialization mismatch) `default` is returned and recorded
    /// as the observed value.
    pub fn read(&mut self, key: u128, ty: &Type, default: Value) -> Value {
        if let Some(v) = self.writes.get(&key) {
            return v.clone();
        }
        if let Some(v) = self.reads.get(&key) {
            return v.clone();
        }
        let observed = match self.kv.get(key) {
            Some(bytes) => deserialize(&bytes, ty).unwrap_or_else(|| default.clone()),
            None => default.clone(),
        };
        self.reads.insert(key, observed.clone());
        observed
    }

    pub fn write(&mut self, key: u128, value: Value) {
        let value = self.force(value);
        self.writes.insert(key, value);
    }

    /// Single-cell typed read — never splits structs. Used by map-cell
    /// reads, where the whole value is stored as one blob (matching
    /// `MapPut`/`Tx::write`'s shape). Defers to the lazy queue if
    /// enabled and the cell isn't already cached.
    pub fn read_cell(&mut self, key: u128, ty: &Type) -> Value {
        if let Some(v) = self.writes.get(&key) { return v.clone(); }
        if let Some(v) = self.reads.get(&key) { return v.clone(); }
        if self.lazy {
            let id = self.next_handle;
            self.next_handle += 1;
            self.pending.push(PendingRead { id, key, ty: ty.clone() });
            Value::Pending(id)
        } else {
            self.read(key, ty, Value::default_for(ty))
        }
    }

    /// Read a value typed by `ty`, splitting `Type::Struct` into per-field
    /// leaf reads. Each leaf cell is keyed by `child(parent_key, field_name)`,
    /// so two transactions touching disjoint fields don't conflict under OCC.
    /// Non-struct types (incl. arrays/sets/dicts/maps) read as a single cell.
    ///
    /// In lazy mode, primitive (non-struct) reads that miss the read
    /// cache and pending writes are queued and return a
    /// `Value::Pending(handle)`; the actual KV access is deferred until
    /// the value is consumed. Struct reads stay eager but go through
    /// `read_typed_many` so all leaves batch into one round-trip.
    pub fn read_typed(&mut self, key: u128, ty: &Type) -> Value {
        if matches!(ty, Type::Struct { .. }) {
            return self
                .read_typed_many(&[(key, ty.clone())])
                .into_iter()
                .next()
                .unwrap();
        }
        if let Some(v) = self.writes.get(&key) { return v.clone(); }
        if let Some(v) = self.reads.get(&key) { return v.clone(); }
        if self.lazy {
            let id = self.next_handle;
            self.next_handle += 1;
            self.pending.push(PendingRead { id, key, ty: ty.clone() });
            Value::Pending(id)
        } else {
            self.read(key, ty, Value::default_for(ty))
        }
    }

    /// Batched typed read. Walks each `(key, ty)` pair into its leaf
    /// cells (struct types split per-field, primitives stay as one),
    /// then issues a single `Kv::get_many` call for every uncached
    /// leaf. Cached values (from prior reads or pending writes) are
    /// served locally without going to the KV. Returns one Value per
    /// input op, in order.
    ///
    /// Used by the bytecode optimizer: a cluster of N independent state
    /// reads collapses into one `get_many` round-trip, even when some
    /// of those reads are struct-shaped and themselves expand to
    /// multiple leaves.
    pub fn read_typed_many(&mut self, ops: &[(u128, Type)]) -> Vec<Value> {
        // Step 1: expand each op into a tree of leaf reads, recording
        // every leaf's key + type in flat parallel vecs.
        let mut leaf_keys: Vec<u128> = Vec::new();
        let mut leaf_types: Vec<Type> = Vec::new();
        let mut tree_specs: Vec<TreeSpec> = Vec::with_capacity(ops.len());
        for (key, ty) in ops {
            tree_specs.push(collect_leaves(*key, ty, &mut leaf_keys, &mut leaf_types));
        }

        // Step 2: split leaves into "already known" vs "must fetch".
        // Pending writes shadow KV; prior reads are cached.
        let mut leaf_values: Vec<Option<Value>> = Vec::with_capacity(leaf_keys.len());
        let mut to_fetch_idx: Vec<usize> = Vec::new();
        let mut to_fetch_keys: Vec<u128> = Vec::new();
        for (i, k) in leaf_keys.iter().enumerate() {
            if let Some(v) = self.writes.get(k) {
                leaf_values.push(Some(v.clone()));
            } else if let Some(v) = self.reads.get(k) {
                leaf_values.push(Some(v.clone()));
            } else {
                leaf_values.push(None);
                to_fetch_idx.push(i);
                to_fetch_keys.push(*k);
            }
        }

        // Step 3: one batch round-trip for everything we still need.
        let bytes = self.kv.get_many(&to_fetch_keys);
        for (slot, bz) in to_fetch_idx.iter().zip(bytes.into_iter()) {
            let ty = &leaf_types[*slot];
            let v = match bz {
                Some(b) => crate::serialize::deserialize(&b, ty)
                    .unwrap_or_else(|| Value::default_for(ty)),
                None => Value::default_for(ty),
            };
            self.reads.insert(leaf_keys[*slot], v.clone());
            leaf_values[*slot] = Some(v);
        }

        // Step 4: reassemble each op's value from its tree spec.
        tree_specs
            .iter()
            .map(|spec| reassemble(spec, &leaf_values))
            .collect()
    }

    /// Write a value typed by `ty`, splitting structs into per-field leaf
    /// writes. Mismatched values fall back to a single-cell write.
    pub fn write_typed(&mut self, key: u128, ty: &Type, value: Value) {
        let value = self.force(value);
        match (ty, value) {
            (Type::Struct { fields: declared, .. }, Value::Struct { fields: actual, .. }) => {
                for (fname, fty) in declared {
                    let leaf = crate::hashing::child(key, fname.as_bytes());
                    let v = actual
                        .iter()
                        .find(|(n, _)| n == fname)
                        .map(|(_, v)| v.clone())
                        .unwrap_or_else(|| Value::default_for(fty));
                    self.write_typed(leaf, fty, v);
                }
            }
            (_, v) => self.write(key, v),
        }
    }

    pub fn into_sets(self) -> (HashMap<u128, Value>, HashMap<u128, Value>) {
        (self.reads, self.writes)
    }

    /// Same as `into_sets` plus the emitted event log. Used by the
    /// engine when assembling `ExecOutcome`.
    pub fn into_parts(self) -> (HashMap<u128, Value>, HashMap<u128, Value>, Vec<EmittedEvent>) {
        (self.reads, self.writes, self.events)
    }

    /// Same as `into_parts` plus per-pmap and per-pvec type tables
    /// and the set of node cells the tx wrote.
    pub fn into_full(self) -> (
        HashMap<u128, Value>,
        HashMap<u128, Value>,
        Vec<EmittedEvent>,
        HashMap<u128, (Type, Type)>,
        HashMap<u128, Type>,
        std::collections::HashSet<u128>,
    ) {
        (
            self.reads, self.writes, self.events,
            self.pmap_types, self.pvec_types,
            self.node_cells_written,
        )
    }
}

/// Whether `v` reaches any `Value::Pending` through arrays / sets /
/// dicts / struct fields. Cheap recursive walk; used to skip the
/// flush+substitute work entirely when the value is already concrete.
fn contains_pending(v: &Value) -> bool {
    match v {
        Value::Pending(_) => true,
        Value::Array(elems) | Value::Set(elems) | Value::Tuple(elems) => {
            elems.iter().any(contains_pending)
        }
        Value::Struct { fields, .. } => fields.iter().any(|(_, v)| contains_pending(v)),
        Value::Dict(pairs) => pairs
            .iter()
            .any(|(k, v)| contains_pending(k) || contains_pending(v)),
        _ => false,
    }
}

/// Replace every `Value::Pending(id)` reachable in `v` with the
/// already-resolved value at `id`. Caller flushes the queue first so
/// every handle has an entry.
fn substitute_resolved(v: Value, resolved: &HashMap<u64, Value>) -> Value {
    match v {
        Value::Pending(id) => resolved.get(&id).cloned().expect("flushed"),
        Value::Array(elems) => Value::Array(
            elems.into_iter().map(|v| substitute_resolved(v, resolved)).collect(),
        ),
        Value::Set(elems) => Value::Set(
            elems.into_iter().map(|v| substitute_resolved(v, resolved)).collect(),
        ),
        Value::Tuple(elems) => Value::Tuple(
            elems.into_iter().map(|v| substitute_resolved(v, resolved)).collect(),
        ),
        Value::Dict(pairs) => Value::Dict(
            pairs
                .into_iter()
                .map(|(k, v)| (substitute_resolved(k, resolved), substitute_resolved(v, resolved)))
                .collect(),
        ),
        Value::Struct { name, fields } => Value::Struct {
            name,
            fields: fields
                .into_iter()
                .map(|(n, v)| (n, substitute_resolved(v, resolved)))
                .collect(),
        },
        v => v,
    }
}

/// Tree-shape of a typed read, recording how leaves project back into a
/// composite Value. Used by `read_typed_many` to flatten reads into a
/// single `Kv::get_many` call and then reassemble structs from the
/// scattered leaves.
enum TreeSpec {
    /// Index into the parallel `leaf_values` vec.
    Leaf(usize),
    Struct { name: String, fields: Vec<(String, TreeSpec)> },
}

fn collect_leaves(
    key: u128,
    ty: &Type,
    keys: &mut Vec<u128>,
    types: &mut Vec<Type>,
) -> TreeSpec {
    match ty {
        Type::Struct { name, fields } => {
            let mut field_specs = Vec::with_capacity(fields.len());
            for (fname, fty) in fields {
                let leaf_key = crate::hashing::child(key, fname.as_bytes());
                field_specs.push((fname.clone(), collect_leaves(leaf_key, fty, keys, types)));
            }
            TreeSpec::Struct { name: name.clone(), fields: field_specs }
        }
        _ => {
            let idx = keys.len();
            keys.push(key);
            types.push(ty.clone());
            TreeSpec::Leaf(idx)
        }
    }
}

fn reassemble(spec: &TreeSpec, leaves: &[Option<Value>]) -> Value {
    match spec {
        TreeSpec::Leaf(i) => leaves[*i].clone().expect("leaf value resolved"),
        TreeSpec::Struct { name, fields } => Value::Struct {
            name: name.clone(),
            fields: fields
                .iter()
                .map(|(n, sub)| (n.clone(), reassemble(sub, leaves)))
                .collect(),
        },
    }
}
