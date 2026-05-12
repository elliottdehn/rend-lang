//! Per-execution transaction. Tracks observed reads and pending writes
//! against an immutable snapshot of the KV. The accumulated
//! `(read_set, write_set)` enables OCC-style concurrent execution: at commit
//! time the host re-fetches each read key, validates the observed value still
//! holds, and applies the writes.

use std::collections::{HashMap, VecDeque};

use crate::ast::Type;
use crate::kv::Kv;
use crate::serialize::deserialize;
use crate::value::Value;

/// One queued event awaiting handler dispatch. Pushed by `Instr::Emit`
/// / `Stmt::Emit`; drained by the engine's handler scheduler after
/// the executing fn returns. Both the emitting module and the
/// struct's full value are recorded so the dispatch step can
/// rebuild handler args and form the `"<module>::<struct>"` lookup
/// key.
#[derive(Debug, Clone)]
pub struct PendingEmit {
    pub module: String,
    pub struct_name: String,
    pub value: Value,
}

/// Per-handler delta extracted from a shadow `Tx`. The scheduler
/// collects one of these per parallel-worker run, then folds them
/// back into the parent `Tx` in stable declaration order.
#[derive(Debug, Default)]
pub struct HandlerDelta {
    pub reads: HashMap<u128, Value>,
    pub writes: HashMap<u128, Value>,
    pub events: Vec<EmittedEvent>,
    pub pending: VecDeque<PendingEmit>,
    pub node_cells_written: std::collections::HashSet<u128>,
    pub pmap_types: HashMap<u128, (Type, Type)>,
    pub pvec_types: HashMap<u128, Type>,
}

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
    /// `Some` for *shadow* transactions spun up by the parallel
    /// handler scheduler. Reads fall through to the parent's writes
    /// and then reads if the local maps don't have the key. Writes
    /// always go to the local maps so the parent stays immutable
    /// while parallel handlers run. Root transactions have `None`.
    parent: Option<&'a Tx<'a>>,
    /// First-observed value at each read key. Used by the host for OCC
    /// validation; the value's type tag is preserved so re-reads can be
    /// canonically re-serialized for byte comparison if desired.
    reads: HashMap<u128, Value>,
    /// Pending writes; later writes for the same key overwrite earlier ones.
    /// Reads of a written key see the in-flight value (read-your-writes).
    writes: HashMap<u128, Value>,
    /// Events queued for handler dispatch. `Instr::Emit` /
    /// `Stmt::Emit` push here; the engine's scheduler drains the
    /// queue after the current fn returns, running matching handlers
    /// in parallel batches with stable-order conflict re-run.
    pending_emits: VecDeque<PendingEmit>,
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
    /// In-flight lazy pmap walks. Drained by `advance_walks` at the
    /// next force. Independent walks across different pmaps land
    /// here together when the optimizer's read-hoist pass clusters
    /// `PMapGet`s, so `advance_walks` can batch their per-level
    /// cell reads into one `Kv::get_many` round-trip.
    pending_walks: Vec<PWalk>,
}

#[derive(Debug, Clone)]
struct PendingRead {
    id: u64,
    key: u128,
    ty: Type,
}

/// In-flight pmap lookup. Driven step-by-step by `advance_walks` so
/// multiple walks across pmaps can have their per-level cell reads
/// batched into one `Kv::get_many` round-trip.
#[derive(Debug, Clone)]
struct PWalk {
    /// `Value::Pending(handle)` returned to the VM register; resolved
    /// to the lookup result once the walk is done.
    handle: u64,
    key: Value,
    key_hash: u64,
    key_ty: Type,
    val_ty: Type,
    /// Current HAMT level (0 at the root).
    level: u32,
    state: WalkState,
}

#[derive(Debug, Clone)]
enum WalkState {
    /// Need this cell's bytes to advance. The walk is dropped from
    /// `pending_walks` and its result installed in `resolved` once
    /// `advance_walks` finishes the descent — there is no "Done"
    /// variant; finishedness is encoded by absence from the queue.
    Need { cell: u128 },
}

impl<'a> Tx<'a> {
    pub fn new(kv: &'a dyn Kv) -> Self {
        Self::new_with_context(kv, TxContext::default())
    }

    pub fn new_with_context(kv: &'a dyn Kv, context: TxContext) -> Self {
        Self {
            kv,
            parent: None,
            reads: HashMap::new(),
            writes: HashMap::new(),
            pending_emits: VecDeque::new(),
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
            pending_walks: Vec::new(),
        }
    }

    /// Open a *shadow* transaction over `parent`. Reads fall through
    /// to `parent`'s writes-then-reads-then-KV chain; writes go to
    /// the shadow's local maps. The scheduler runs each handler in
    /// its own shadow so parallel workers don't see each other's
    /// pending writes — no cloning of the parent's read/write maps.
    pub fn shadow_of(parent: &'a Tx<'a>) -> Self {
        Self {
            kv: parent.kv,
            parent: Some(parent),
            reads: HashMap::new(),
            writes: HashMap::new(),
            pending_emits: VecDeque::new(),
            lazy: parent.lazy,
            pending: Vec::new(),
            resolved: HashMap::new(),
            next_handle: parent.next_handle,
            events: Vec::new(),
            context: parent.context.clone(),
            nore_active: parent.nore_active.clone(),
            pmap_types: HashMap::new(),
            pvec_types: HashMap::new(),
            node_cells_written: std::collections::HashSet::new(),
            pending_walks: Vec::new(),
        }
    }

    /// Consume the shadow and return its accumulated delta. The
    /// parent merges this in declaration order with conflict re-run.
    pub fn into_delta(self) -> HandlerDelta {
        HandlerDelta {
            reads: self.reads,
            writes: self.writes,
            events: self.events,
            pending: self.pending_emits,
            node_cells_written: self.node_cells_written,
            pmap_types: self.pmap_types,
            pvec_types: self.pvec_types,
        }
    }

    /// Fold a shadow's accumulated delta into this Tx. Reads use
    /// first-observed semantics (don't overwrite a value the parent
    /// already saw); writes always overwrite. Events + pending emits
    /// append.
    pub fn merge_delta(&mut self, delta: HandlerDelta) {
        for (k, v) in delta.reads {
            self.reads.entry(k).or_insert(v);
        }
        for (k, v) in delta.writes {
            self.writes.insert(k, v);
        }
        self.events.extend(delta.events);
        self.pending_emits.extend(delta.pending);
        self.node_cells_written.extend(delta.node_cells_written);
        self.pmap_types.extend(delta.pmap_types);
        self.pvec_types.extend(delta.pvec_types);
    }

    /// Push a pending event onto the scheduler queue.
    pub fn enqueue_emit(&mut self, emit: PendingEmit) {
        self.pending_emits.push_back(emit);
    }

    /// Drain every queued emit. Used by the engine's handler
    /// scheduler between batches.
    pub fn take_pending_emits(&mut self) -> VecDeque<PendingEmit> {
        std::mem::take(&mut self.pending_emits)
    }

    pub fn has_pending_emits(&self) -> bool {
        !self.pending_emits.is_empty()
    }

    /// Look up a key in the parent overlay (writes then reads,
    /// chained upward). Returns `None` if no ancestor has it.
    fn parent_lookup(&self, key: u128) -> Option<Value> {
        let mut p = self.parent;
        while let Some(t) = p {
            if let Some(v) = t.writes.get(&key) { return Some(v.clone()); }
            if let Some(v) = t.reads.get(&key) { return Some(v.clone()); }
            p = t.parent;
        }
        None
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
    ///
    /// Drives both single-cell pending reads (`flush_pending`) and
    /// in-flight pmap walks (`advance_walks`) until both queues are
    /// empty. Walks issue per-level cell reads, which themselves go
    /// through the pending queue — so we loop until no progress is
    /// possible. The order doesn't matter: each iteration either
    /// drains pending single-cell reads or advances every walk by
    /// one HAMT level.
    pub fn force(&mut self, v: Value) -> Value {
        if !contains_pending(&v) { return v; }
        loop {
            let had_reads = !self.pending.is_empty();
            let had_walks = !self.pending_walks.is_empty();
            if !had_reads && !had_walks { break; }
            if had_reads { self.flush_pending(); }
            if had_walks { self.advance_walks(); }
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
        // Shadow Tx: try the parent overlay before issuing a KV
        // round-trip. Parent writes shadow KV, so a lazy read of a
        // freshly-emitted value resolves locally.
        let mut to_fetch: Vec<(usize, PendingRead)> = Vec::new();
        let mut resolved_inline: Vec<(PendingRead, Value)> = Vec::new();
        for (i, p) in pending.into_iter().enumerate() {
            if let Some(v) = self.parent_lookup(p.key) {
                resolved_inline.push((p, v));
            } else {
                to_fetch.push((i, p));
            }
        }
        let keys: Vec<u128> = to_fetch.iter().map(|(_, p)| p.key).collect();
        let bytes = self.kv.get_many(&keys);
        for ((_, p), bz) in to_fetch.into_iter().zip(bytes.into_iter()) {
            let v = match bz {
                Some(b) => crate::serialize::deserialize(&b, &p.ty)
                    .unwrap_or_else(|| Value::default_for(&p.ty)),
                None => Value::default_for(&p.ty),
            };
            self.reads.insert(p.key, v.clone());
            self.resolved.insert(p.id, v);
        }
        for (p, v) in resolved_inline {
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
        // Shadow Tx: fall through to ancestor writes/reads before
        // going to KV. The looked-up value is cached locally so
        // repeated reads stay O(1) within the shadow.
        if let Some(v) = self.parent_lookup(key) {
            self.reads.insert(key, v.clone());
            return v;
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
        if let Some(v) = self.parent_lookup(key) {
            self.reads.insert(key, v.clone());
            return v;
        }
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
        // Pending writes shadow KV; prior reads are cached. Shadow
        // Txs also peek at the parent overlay chain before falling
        // through to KV.
        let mut leaf_values: Vec<Option<Value>> = Vec::with_capacity(leaf_keys.len());
        let mut to_fetch_idx: Vec<usize> = Vec::new();
        let mut to_fetch_keys: Vec<u128> = Vec::new();
        for (i, k) in leaf_keys.iter().enumerate() {
            if let Some(v) = self.writes.get(k) {
                leaf_values.push(Some(v.clone()));
            } else if let Some(v) = self.reads.get(k) {
                leaf_values.push(Some(v.clone()));
            } else if let Some(v) = self.parent_lookup(*k) {
                self.reads.insert(*k, v.clone());
                leaf_values.push(Some(v));
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
            (
                Type::Struct { name: parent_name, fields: declared, field_groups },
                Value::Struct { fields: actual, .. },
            ) => {
                // Track which groups we've already emitted so each
                // group cell is written exactly once even though
                // multiple `declared` entries point at it.
                let mut emitted_groups: std::collections::HashSet<String> =
                    std::collections::HashSet::new();
                for (i, (fname, fty)) in declared.iter().enumerate() {
                    let group = field_groups.get(i).and_then(|g| g.as_ref());
                    match group {
                        None => {
                            let leaf = crate::hashing::child(key, fname.as_bytes());
                            let v = actual
                                .iter()
                                .find(|(n, _)| n == fname)
                                .map(|(_, v)| v.clone())
                                .unwrap_or_else(|| Value::default_for(fty));
                            self.write_typed(leaf, fty, v);
                        }
                        Some(g) => {
                            if !emitted_groups.insert(g.clone()) {
                                continue;
                            }
                            let (_synth_ty, synth_val) = build_group_blob(
                                parent_name,
                                declared,
                                field_groups,
                                &actual,
                                g,
                            );
                            // Group cell: write the synthetic struct
                            // as a single blob. Going through `write`
                            // (rather than `write_typed`) intentionally
                            // skips the per-field split — the on-disk
                            // shape is one cell with the group's
                            // fields packed in declaration order.
                            let leaf_key = crate::hashing::child(key, g.as_bytes());
                            self.write(leaf_key, synth_val);
                        }
                    }
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

    // ---------- lazy pmap walks ----------
    //
    // A `PMapGet` instruction registers a walk and returns
    // `Value::Pending(handle)` rather than synchronously chasing the
    // HAMT. The walk's per-level cell reads are driven by
    // `advance_walks`, which is invoked from `force()` whenever
    // anything tries to consume a Pending value. Multiple in-flight
    // walks share one `Kv::get_many` round-trip per level, so when
    // the optimizer hoists adjacent `PMapGet`s into the same cluster
    // the cost collapses from N×depth sequential reads to depth
    // batched reads.

    /// Begin a lazy pmap lookup. Returns the handle that will resolve
    /// to either the found value or the value type's default once the
    /// walk completes. If the root is empty (no tree), resolves
    /// immediately to the default — no walk needed.
    pub fn begin_pmap_get(
        &mut self,
        root_hash: u128,
        key: Value,
        key_ty: Type,
        val_ty: Type,
    ) -> Value {
        if root_hash == crate::pmap::EMPTY {
            return Value::default_for(&val_ty);
        }
        let key_hash = crate::pmap::hash_for_walk(&key);
        let handle = self.next_handle;
        self.next_handle += 1;
        self.pending_walks.push(PWalk {
            handle,
            key,
            key_hash,
            key_ty,
            val_ty,
            level: 0,
            state: WalkState::Need { cell: root_hash },
        });
        Value::Pending(handle)
    }

    /// Drive every in-flight walk by one HAMT level. All walks at
    /// `Need { cell }` contribute their cell key to a single
    /// `Kv::get_many`; results are decoded; each walk either descends
    /// to the next level or finishes. Walks that finish are moved out
    /// of `pending_walks` and their handles installed in `resolved`.
    fn advance_walks(&mut self) {
        if self.pending_walks.is_empty() { return; }
        let walks = std::mem::take(&mut self.pending_walks);

        // Collect (and dedup) the cells we need to fetch from KV.
        // Cells already in the read/write cache are served inline.
        // Dedup matters at HAMT level 0 — every walk on the same
        // pmap shares the root node cell — and at any deeper level
        // where two keys share a prefix.
        let mut to_fetch: Vec<u128> = Vec::new();
        let mut seen: HashMap<u128, usize> = HashMap::new();
        for w in walks.iter() {
            let WalkState::Need { cell } = &w.state;
            if self.writes.contains_key(cell) || self.reads.contains_key(cell) {
                continue;
            }
            // Shadow Tx: parent overlay can supply node bytes
            // without a KV round-trip.
            if self.parent_lookup(*cell).is_some() {
                continue;
            }
            seen.entry(*cell).or_insert_with(|| {
                let idx = to_fetch.len();
                to_fetch.push(*cell);
                idx
            });
        }
        let fetched = if to_fetch.is_empty() {
            Vec::new()
        } else {
            self.kv.get_many(&to_fetch)
        };
        let mut bytes_by_cell: HashMap<u128, Option<Vec<u8>>> = HashMap::with_capacity(to_fetch.len());
        for (cell, bz) in to_fetch.into_iter().zip(fetched.into_iter()) {
            bytes_by_cell.insert(cell, bz);
        }

        let mut next_round = Vec::new();
        for mut w in walks.into_iter() {
            let WalkState::Need { cell } = w.state.clone();

            // Source node bytes for this cell. Three paths:
            //   1. write set hit  — already-decoded `Value::Bytes(node)`;
            //                       same shape as `read_node` returns.
            //   2. read cache hit — same.
            //   3. fresh fetch    — raw on-disk bytes that still need
            //                       one `serialize::deserialize` to unwrap
            //                       the `Value::Bytes` framing the cell
            //                       stores them in. Records OCC read.
            let node_bytes: Option<Vec<u8>> =
                if let Some(v) = self.writes.get(&cell) {
                    match v { Value::Bytes(b) => Some(b.clone()), _ => None }
                } else if let Some(v) = self.reads.get(&cell) {
                    match v { Value::Bytes(b) => Some(b.clone()), _ => None }
                } else if let Some(v) = self.parent_lookup(cell) {
                    // Shadow Tx: parent overlay hit — cache locally
                    // so subsequent walks at the same cell skip the
                    // lookup, and treat as if KV had returned the
                    // bytes (an OCC read on the cell).
                    self.reads.insert(cell, v.clone());
                    match v { Value::Bytes(b) => Some(b), _ => None }
                } else {
                    let raw = bytes_by_cell.get(&cell).cloned().unwrap_or(None);
                    let observed = match &raw {
                        Some(b) => crate::serialize::deserialize(b, &Type::Bytes)
                            .unwrap_or_else(|| Value::default_for(&Type::Bytes)),
                        None => Value::default_for(&Type::Bytes),
                    };
                    // Record OCC read in the same shape `read_cell`
                    // would have. Subsequent walks at the same cell
                    // (level-0 root sharing) will see the cache hit
                    // above on this loop iteration too.
                    self.reads.insert(cell, observed.clone());
                    match observed { Value::Bytes(b) => Some(b), _ => None }
                };

            match advance_pmap_walk(&mut w, node_bytes) {
                AdvanceResult::Done(value) => {
                    self.resolved.insert(w.handle, value);
                }
                AdvanceResult::Continue => next_round.push(w),
            }
        }
        self.pending_walks = next_round;
    }
}

/// Result of advancing a single walk by one HAMT level.
enum AdvanceResult {
    /// Walk finished; `Value` is the lookup result already substituted
    /// to the value type's default if the key was absent.
    Done(Value),
    /// Walk descended to the next level. `walk.state` is updated to
    /// `Need` for the new cell.
    Continue,
}

fn advance_pmap_walk(walk: &mut PWalk, bytes: Option<Vec<u8>>) -> AdvanceResult {
    let Some(bytes) = bytes else {
        // Missing cell — well-formed walks shouldn't hit this, but
        // treat as "not present" for safety.
        return AdvanceResult::Done(Value::default_for(&walk.val_ty));
    };
    let node = match crate::pmap::decode_node(&bytes, &walk.key_ty, &walk.val_ty) {
        Some(n) => n,
        None => return AdvanceResult::Done(Value::default_for(&walk.val_ty)),
    };
    match node {
        crate::pmap::Node::Leaf { entries } => {
            let value = entries.into_iter()
                .find(|(k, _)| k == &walk.key)
                .map(|(_, v)| v)
                .unwrap_or_else(|| Value::default_for(&walk.val_ty));
            AdvanceResult::Done(value)
        }
        crate::pmap::Node::Inner { bitmap, children } => {
            // Hash bits exhausted — the node should be a Leaf, not an
            // Inner. Defensive: treat as miss.
            if walk.level + 1 > crate::pmap::MAX_WALK_LEVELS {
                return AdvanceResult::Done(Value::default_for(&walk.val_ty));
            }
            let slot = crate::pmap::slot_at(walk.key_hash, walk.level);
            let bit = 1u32 << slot;
            if bitmap & bit == 0 {
                return AdvanceResult::Done(Value::default_for(&walk.val_ty));
            }
            let idx = (bitmap & (bit - 1)).count_ones() as usize;
            walk.level += 1;
            walk.state = WalkState::Need { cell: children[idx] };
            AdvanceResult::Continue
        }
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
    /// Field that lives inside a grouped cell. `leaf_idx` points at
    /// the group blob's slot in `leaf_values`; `field_name` is the
    /// field to project out during reassembly.
    Projection { leaf_idx: usize, field_name: String },
}

fn collect_leaves(
    key: u128,
    ty: &Type,
    keys: &mut Vec<u128>,
    types: &mut Vec<Type>,
) -> TreeSpec {
    match ty {
        Type::Struct { name, fields, field_groups } => {
            let mut field_specs = Vec::with_capacity(fields.len());
            // Each unique group name → leaf_idx of its synthetic blob.
            // Populated lazily on first sight; subsequent siblings in
            // the same group reuse the registered leaf.
            let mut group_leaf: std::collections::HashMap<String, usize> =
                std::collections::HashMap::new();
            for (i, (fname, fty)) in fields.iter().enumerate() {
                let group = field_groups.get(i).and_then(|g| g.as_ref());
                match group {
                    None => {
                        let leaf_key = crate::hashing::child(key, fname.as_bytes());
                        field_specs.push((
                            fname.clone(),
                            collect_leaves(leaf_key, fty, keys, types),
                        ));
                    }
                    Some(g) => {
                        let leaf_idx = if let Some(&idx) = group_leaf.get(g) {
                            idx
                        } else {
                            // Synthetic struct type for the group cell:
                            // a flat struct holding the group's fields
                            // in declaration order. `field_groups` is
                            // empty so the serializer treats it as one
                            // blob (no nested splitting).
                            let group_fields: Vec<(String, Type)> = fields
                                .iter()
                                .enumerate()
                                .filter(|(j, _)| {
                                    field_groups.get(*j).and_then(|g2| g2.as_ref())
                                        == Some(g)
                                })
                                .map(|(_, (fn_, ft))| (fn_.clone(), ft.clone()))
                                .collect();
                            let synthetic_ty = Type::Struct {
                                name: synthetic_group_name(name, g),
                                fields: group_fields,
                                field_groups: Vec::new(),
                            };
                            let leaf_key = crate::hashing::child(key, g.as_bytes());
                            let idx = keys.len();
                            keys.push(leaf_key);
                            types.push(synthetic_ty);
                            group_leaf.insert(g.clone(), idx);
                            idx
                        };
                        field_specs.push((
                            fname.clone(),
                            TreeSpec::Projection {
                                leaf_idx,
                                field_name: fname.clone(),
                            },
                        ));
                    }
                }
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
        TreeSpec::Projection { leaf_idx, field_name } => {
            let leaf = leaves[*leaf_idx].as_ref().expect("group leaf resolved");
            match leaf {
                Value::Struct { fields, .. } => fields
                    .iter()
                    .find(|(n, _)| n == field_name)
                    .map(|(_, v)| v.clone())
                    .expect("group struct missing projected field"),
                other => panic!("group leaf is not a struct value: {other}"),
            }
        }
    }
}

/// Synthetic name for a group's blob cell. Used as the
/// `Type::Struct.name` of the on-disk synthetic struct so the
/// runtime can tell a group blob apart from a real struct in
/// debug output. Format is `{parent}::group::{group}`.
pub(crate) fn synthetic_group_name(parent: &str, group: &str) -> String {
    format!("{parent}::group::{group}")
}

/// Build the (synthetic_struct_type, synthetic_struct_value) pair
/// for a single group inside a parent struct. Used by both the
/// runtime write path and the compile path to keep the on-disk
/// shape in lockstep. Returns `None` if `actual` has no values for
/// the group's fields (caller should treat as "no write needed",
/// though today every code path that calls this has all values).
pub(crate) fn build_group_blob(
    parent_name: &str,
    declared: &[(String, Type)],
    field_groups: &[Option<String>],
    actual: &[(String, Value)],
    group: &str,
) -> (Type, Value) {
    let group_fields: Vec<(String, Type)> = declared
        .iter()
        .enumerate()
        .filter(|(j, _)| field_groups.get(*j).and_then(|g| g.as_ref()) == Some(&group.to_string()))
        .map(|(_, (fn_, ft))| (fn_.clone(), ft.clone()))
        .collect();
    let synthetic_ty = Type::Struct {
        name: synthetic_group_name(parent_name, group),
        fields: group_fields.clone(),
        field_groups: Vec::new(),
    };
    let value_fields: Vec<(String, Value)> = group_fields
        .iter()
        .map(|(fn_, ft)| {
            let v = actual
                .iter()
                .find(|(n, _)| n == fn_)
                .map(|(_, v)| v.clone())
                .unwrap_or_else(|| Value::default_for(ft));
            (fn_.clone(), v)
        })
        .collect();
    let synth_val = Value::Struct {
        name: synthetic_group_name(parent_name, group),
        fields: value_fields,
    };
    (synthetic_ty, synth_val)
}
