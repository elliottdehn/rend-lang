//! RocksDB-backed `rend::kv::Kv` implementation.
//!
//! Three layers:
//!
//!   * [`RocksKv`] — thin wrapper around an
//!     [`OptimisticTransactionDB`]. One per server process. Used
//!     for namespaced read access AND for OCC commits driven by
//!     [`commit_with_occ`].
//!
//!   * [`NamespacedKv`] — per-tx read view that prefixes every
//!     cell key with an opaque namespace tag (typically an org id).
//!     Implements `rend::kv::Kv` so the runtime reads through it.
//!
//!   * [`commit_with_occ`] — applies a rend `ExecOutcome`
//!     atomically: validates the read set against current state via
//!     `get_for_update`, then writes the write set, then commits.
//!     RocksDB's optimistic-transaction conflict detection catches
//!     concurrent committers; we surface it as
//!     [`CommitResult::Conflict`] so the caller can re-execute the
//!     tx against fresh state.
//!
//! ### Key layout
//!
//! Each rend cell key is a `u128`. Under namespacing we prepend a
//! big-endian `u64` namespace id, so the on-disk key is 24 bytes:
//!
//! ```text
//! [u64 namespace][u128 cell key]
//! ```
//!
//! Two orgs touching the same content-addressed cell key end up at
//! different on-disk keys, so cross-org commits don't conflict on
//! the storage layer. Within an org, conflicts on overlapping cells
//! are detected by RocksDB; conflicts on disjoint cells aren't, so
//! disjoint-key txs commit in parallel.

use rend::ast::Type;
use rend::kv::Kv;
use rend::value::Value;
use rocksdb::{
    ColumnFamilyDescriptor, ErrorKind, OptimisticTransactionDB, Options, SingleThreaded,
    WriteBatchWithTransaction, WriteOptions,
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

pub type NamespaceId = u64;

const NAMESPACE_PREFIX_LEN: usize = 8;
const CELL_KEY_LEN: usize = 16;
const FULL_KEY_LEN: usize = NAMESPACE_PREFIX_LEN + CELL_KEY_LEN;

/// Column family for per-EOA monotonic auth nonces. Keyed by the
/// 20-byte recovered address; value is a big-endian u64.
const NONCES_CF: &str = "nonces";

/// Encode a `(namespace, cell_key)` pair into the on-disk key.
fn encode_key(ns: NamespaceId, cell: u128) -> [u8; FULL_KEY_LEN] {
    let mut out = [0u8; FULL_KEY_LEN];
    out[..NAMESPACE_PREFIX_LEN].copy_from_slice(&ns.to_be_bytes());
    out[NAMESPACE_PREFIX_LEN..].copy_from_slice(&cell.to_be_bytes());
    out
}

/// The shared RocksDB instance. Cheap to clone (`Arc` inside).
#[derive(Clone)]
pub struct RocksKv {
    db: Arc<OptimisticTransactionDB<SingleThreaded>>,
    /// Optional fine-grained timing breakdown of `commit_with_occ`'s
    /// internal phases. Off by default (zero overhead). Toggled on
    /// for profiling runs.
    pub timings: Arc<CommitTimings>,
    /// Whether to skip the WAL on tx commit. False = standard
    /// durable behavior (each commit fsyncs the WAL). True = trade
    /// last-few-ms-of-writes durability for ~10× faster commits;
    /// state still survives clean shutdown via the memtable+SST
    /// flush path. Set via `RocksKv::set_durable`.
    durable: Arc<std::sync::atomic::AtomicBool>,
    /// Per-namespace commit mutexes. The commit path acquires the
    /// namespace's mutex, validates and merges against current
    /// state via plain reads, then applies a `WriteBatch`. This
    /// replaces the `OptimisticTransactionDB`'s commit-time
    /// conflict-tracking machinery (which we already duplicate in
    /// `commit_with_occ`) with a much cheaper synchronization
    /// primitive — at the cost of serializing commits within an
    /// org. Cross-org commits remain fully parallel because
    /// different namespaces hold different mutexes.
    commit_locks: Arc<Mutex<HashMap<NamespaceId, Arc<Mutex<()>>>>>,
}

/// Atomic ns counters for the inner commit pipeline.
#[derive(Default)]
pub struct CommitTimings {
    pub validate_ns: std::sync::atomic::AtomicU64,
    pub stage_writes_ns: std::sync::atomic::AtomicU64,
    pub txn_commit_ns: std::sync::atomic::AtomicU64,
    pub commits: std::sync::atomic::AtomicU64,
}

/// Outcome of an OCC-validated commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitResult {
    /// Reads validated, writes applied, transaction committed.
    Committed,
    /// Either a read had been overwritten since the rend tx
    /// recorded it, or RocksDB's optimistic commit detected a
    /// concurrent committer touching one of our keys. Caller
    /// should re-execute the rend tx against fresh state.
    Conflict,
}

/// Outcome of a single nonce CAS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NonceResult {
    /// `submitted` was strictly greater than the stored value;
    /// the nonce row has been advanced and the request is fresh.
    Bumped,
    /// `submitted <= stored`. Replay or out-of-order submission;
    /// the request must be rejected.
    TooLow { stored: u64 },
}

impl RocksKv {
    /// Toggle durable commits at runtime. `true` (default) → each
    /// commit syncs the WAL. `false` → commits skip the WAL,
    /// landing only in the memtable until the next SST flush. The
    /// fast mode is fine for benchmarking and for deployments that
    /// can replay the last few seconds of work from an external
    /// log on crash; do not enable on a node where on-disk
    /// durability of every commit is a hard requirement.
    pub fn set_durable(&self, durable: bool) {
        self.durable.store(durable, std::sync::atomic::Ordering::Relaxed);
    }

    /// Open or create a RocksDB at `path`. Uses an
    /// `OptimisticTransactionDB` so multi-tx commit conflict
    /// detection is built in.
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Self, rocksdb::Error> {
        let mut opts = Options::default();
        opts.create_if_missing(true);
        opts.create_missing_column_families(true);
        opts.set_compression_type(rocksdb::DBCompressionType::Snappy);
        // Bigger memtable cuts write amplification on indexed-write
        // workloads (every primary write also touches multiple
        // index cells).
        opts.set_write_buffer_size(64 * 1024 * 1024);
        let cfs = vec![ColumnFamilyDescriptor::new(NONCES_CF, Options::default())];
        let db = OptimisticTransactionDB::open_cf_descriptors(&opts, path, cfs)?;
        Ok(Self {
            db: Arc::new(db),
            timings: Arc::new(CommitTimings::default()),
            durable: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            commit_locks: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    /// Get-or-insert the commit mutex for `ns`. The outer lock is
    /// only held briefly during the lookup; the returned mutex is
    /// what callers actually serialize on. Per-NS entries are kept
    /// for the lifetime of the process — bounded by the number of
    /// distinct orgs the server has ever served.
    fn ns_lock(&self, ns: NamespaceId) -> Arc<Mutex<()>> {
        let mut locks = self.commit_locks.lock().unwrap();
        locks
            .entry(ns)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    /// Build a per-tx Kv view scoped to one namespace. The view is
    /// cheap: it just clones the `Arc`.
    pub fn namespace(&self, ns: NamespaceId) -> NamespacedKv {
        NamespacedKv { db: self.db.clone(), ns }
    }

    /// Plain non-OCC batch write — used for content-addressed
    /// artifact bytes, where last-writer-wins is correct because
    /// all writers produce the same bytes for the same hash.
    /// Not for state cells; state writes go through
    /// [`commit_with_occ`].
    pub fn apply(
        &self,
        ns: NamespaceId,
        writes: &HashMap<u128, Value>,
    ) -> Result<(), rocksdb::Error> {
        let mut batch = WriteBatchWithTransaction::<true>::default();
        for (cell, value) in writes {
            let key = encode_key(ns, *cell);
            let bytes = rend::serialize::serialize(value);
            batch.put(key, bytes);
        }
        self.db.write(batch)
    }

    /// Commit a rend [`ExecOutcome`] atomically with OCC validation.
    ///
    /// Inside one optimistic-transaction:
    ///
    ///   1. For each `(cell, expected_value)` in `reads`, call
    ///      `get_for_update` against the on-disk key. RocksDB
    ///      tracks the key for conflict detection.
    ///   2. Compare the on-disk bytes against
    ///      `serialize(expected_value)`. Treat "no value" and the
    ///      serialized default as equivalent — rend reads a missing
    ///      cell as the value-type's default, and we want the
    ///      validation to agree.
    ///   3. If any read disagrees, return `Conflict` early; the
    ///      caller re-executes the rend tx against fresh state.
    ///   4. Otherwise stage all writes via `txn.put`.
    ///   5. `txn.commit()`. If a concurrent committer touched any
    ///      of our `get_for_update` keys, RocksDB returns
    ///      `Busy`/`TryAgain` and we surface `Conflict`.
    ///
    /// Two txs that touch disjoint sets of cells commit in
    /// parallel — neither's `get_for_update` keys overlap, so
    /// neither blocks the other.
    ///
    /// When a read mismatch lands on a pmap root cell whose K/V
    /// types are recorded in `pmap_types`, this method attempts a
    /// 3-way HAMT merge against the live and ancestor roots before
    /// surfacing `Conflict`. Two transfers on disjoint accounts in
    /// one bank merge byte-identically to the serialized outcome.
    ///
    /// Synchronization: a per-namespace mutex serializes commits
    /// within an org. We do all of OCC validation ourselves; the
    /// `OptimisticTransactionDB` machinery would be redundant
    /// overhead. Inside the mutex, plain `db.get` reads the latest
    /// committed state (no concurrent writer can race because no
    /// other committer can hold the mutex), so validation is
    /// trivially correct. Cross-org commits hold disjoint mutexes
    /// and run fully in parallel.
    pub fn commit_with_occ(
        &self,
        ns: NamespaceId,
        reads: &HashMap<u128, Value>,
        writes: &HashMap<u128, Value>,
        pmap_types: &HashMap<u128, (Type, Type)>,
    ) -> Result<CommitResult, rocksdb::Error> {
        use std::sync::atomic::Ordering;

        let lock = self.ns_lock(ns);
        let _guard = lock.lock().unwrap();

        let validate_t0 = std::time::Instant::now();
        let mut effective_writes: HashMap<u128, Value> = writes.clone();
        let mut merge_node_cells: Vec<(u128, Vec<u8>)> = Vec::new();

        for (cell, expected) in reads {
            let on_disk = self.db.get(encode_key(ns, *cell))?;
            if read_matches(&on_disk, expected) {
                continue;
            }
            // Read mismatch. Try a 3-way merge if this is a pmap root.
            let Some((key_ty, val_ty)) = pmap_types.get(cell) else {
                return Ok(CommitResult::Conflict);
            };
            let pmap_ty = Type::PMap {
                key: Box::new(key_ty.clone()),
                value: Box::new(val_ty.clone()),
            };
            let ancestor_root = match expected {
                Value::PMap(h) => *h,
                _ => return Ok(CommitResult::Conflict),
            };
            let live_root = match on_disk.as_ref()
                .and_then(|b| rend::serialize::deserialize(b, &pmap_ty))
            {
                Some(Value::PMap(h)) => h,
                _ => 0, // EMPTY
            };
            let our_root = match effective_writes.get(cell) {
                Some(Value::PMap(h)) => *h,
                _ => return Ok(CommitResult::Conflict),
            };
            // Build a Kv view that overlays the tx's in-flight writes
            // on top of the underlying namespace; the merge may need
            // to read HAMT nodes our tx wrote but didn't commit yet.
            let view = self.namespace(ns);
            let merge_kv = MergeKv { base: &view, overlay: writes };
            let Some(merged) = rend::pmap::merge_three_way(
                our_root, live_root, ancestor_root,
                &merge_kv, key_ty, val_ty,
            ) else {
                // Real conflict — both sides changed the same key.
                return Ok(CommitResult::Conflict);
            };
            effective_writes.insert(*cell, Value::PMap(merged.root));
            merge_node_cells.extend(merged.new_cells);
        }
        self.timings.validate_ns.fetch_add(
            validate_t0.elapsed().as_nanos() as u64, Ordering::Relaxed);

        // Build a single WriteBatch with effective writes + any
        // merge-synthesized HAMT node cells, and apply it
        // atomically. WriteBatch's own atomicity is sufficient —
        // we already serialized concurrent writers via the
        // namespace mutex.
        let stage_t0 = std::time::Instant::now();
        let mut batch = WriteBatchWithTransaction::<true>::default();
        for (cell, value) in &effective_writes {
            batch.put(encode_key(ns, *cell), rend::serialize::serialize(value));
        }
        for (cell, bytes) in merge_node_cells {
            batch.put(
                encode_key(ns, cell),
                rend::serialize::serialize(&Value::Bytes(bytes)),
            );
        }
        self.timings.stage_writes_ns.fetch_add(
            stage_t0.elapsed().as_nanos() as u64, Ordering::Relaxed);

        let commit_t0 = std::time::Instant::now();
        let mut wo = WriteOptions::default();
        if !self.durable.load(Ordering::Relaxed) {
            wo.disable_wal(true);
        } else {
            // Real durability — fsync the WAL on every commit. On
            // macOS this is fdatasync (page-cache flush, not device
            // flush), but it's the closest analogue to what a
            // production deployment would do in any environment
            // where commit latency is dominated by sync, and is
            // what makes group commit a meaningful win (per-batch
            // sync amortizes across N commits).
            wo.set_sync(true);
        }
        self.db.write_opt(batch, &wo)?;
        self.timings.txn_commit_ns.fetch_add(
            commit_t0.elapsed().as_nanos() as u64, Ordering::Relaxed);
        self.timings.commits.fetch_add(1, Ordering::Relaxed);
        Ok(CommitResult::Committed)
    }

    /// Atomically advance the auth nonce for `address` to
    /// `submitted`, requiring `submitted > stored`. Loops on RocksDB
    /// optimistic-commit conflict (another concurrent CAS for the
    /// same address); converges after at most a few attempts because
    /// each retry sees the new state. Caller should treat
    /// `TooLow { stored }` as a 401 (replay or out-of-order).
    pub fn bump_nonce(
        &self,
        address: &[u8; 20],
        submitted: u64,
    ) -> Result<NonceResult, rocksdb::Error> {
        let cf = self
            .db
            .cf_handle(NONCES_CF)
            .expect("nonces column family was created at open()");
        loop {
            let txn = self.db.transaction();
            let stored = txn
                .get_for_update_cf(&cf, address.as_slice(), true)?
                .map(|b| {
                    let mut buf = [0u8; 8];
                    let n = b.len().min(8);
                    buf[..n].copy_from_slice(&b[..n]);
                    u64::from_be_bytes(buf)
                })
                .unwrap_or(0);
            if submitted <= stored {
                return Ok(NonceResult::TooLow { stored });
            }
            txn.put_cf(&cf, address.as_slice(), submitted.to_be_bytes())?;
            match txn.commit() {
                Ok(()) => return Ok(NonceResult::Bumped),
                Err(e) => match e.kind() {
                    ErrorKind::Busy | ErrorKind::TryAgain => continue,
                    _ => return Err(e),
                },
            }
        }
    }
}

/// True iff the on-disk bytes match the rend-recorded value at
/// read time. rend reads a missing cell as the value-type's
/// default, so "no value on disk" and "serialized default" are
/// both valid matches for a recorded default — but only the
/// default. A non-default recorded value MUST appear on disk
/// byte-for-byte.
fn read_matches(on_disk: &Option<Vec<u8>>, expected: &Value) -> bool {
    match on_disk {
        None => rend::serialize::is_default(expected),
        Some(bytes) => {
            let want = rend::serialize::serialize(expected);
            *bytes == want
        }
    }
}

/// `Kv` view used during a 3-way pmap merge. Reads are sourced
/// from `overlay` first (the in-flight tx's writes — HAMT nodes
/// our tx produced but hasn't committed yet) and fall back to
/// `base` (the underlying namespaced storage). This lets
/// `pmap::merge_three_way` decode our tree's nodes without us
/// having to flush them to disk first.
struct MergeKv<'a> {
    base: &'a NamespacedKv,
    overlay: &'a HashMap<u128, Value>,
}

impl<'a> Kv for MergeKv<'a> {
    fn get(&self, key: u128) -> Option<Vec<u8>> {
        if let Some(v) = self.overlay.get(&key) {
            return Some(rend::serialize::serialize(v));
        }
        self.base.get(key)
    }
    fn get_many(&self, keys: &[u128]) -> Vec<Option<Vec<u8>>> {
        keys.iter().map(|k| self.get(*k)).collect()
    }
}

/// Per-tx, per-namespace read view. Implements the rend `Kv` trait
/// so the runtime can read through it.
pub struct NamespacedKv {
    db: Arc<OptimisticTransactionDB<SingleThreaded>>,
    ns: NamespaceId,
}

impl Kv for NamespacedKv {
    fn get(&self, key: u128) -> Option<Vec<u8>> {
        let full = encode_key(self.ns, key);
        match self.db.get(full) {
            Ok(Some(bytes)) => Some(bytes),
            Ok(None) => None,
            Err(_) => None,
        }
    }

    fn get_many(&self, keys: &[u128]) -> Vec<Option<Vec<u8>>> {
        let full_keys: Vec<[u8; FULL_KEY_LEN]> = keys.iter()
            .map(|k| encode_key(self.ns, *k))
            .collect();
        self.db
            .multi_get(&full_keys)
            .into_iter()
            .map(|r| r.ok().flatten())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn round_trip_through_namespace() {
        let dir = tempfile::tempdir().unwrap();
        let kv = RocksKv::open(dir.path()).unwrap();
        let mut writes = HashMap::new();
        writes.insert(0xCAFE_BABE_u128, Value::U64(42));
        kv.apply(7, &writes).unwrap();

        let view = kv.namespace(7);
        let bytes = view.get(0xCAFE_BABE_u128).unwrap();
        assert_eq!(
            rend::serialize::deserialize(&bytes, &rend::ast::Type::U64),
            Some(Value::U64(42)),
        );

        // Different namespace sees nothing.
        let other = kv.namespace(8);
        assert!(other.get(0xCAFE_BABE_u128).is_none());
    }

    #[test]
    fn get_many_preserves_order_and_namespace() {
        let dir = tempfile::tempdir().unwrap();
        let kv = RocksKv::open(dir.path()).unwrap();
        let mut writes = HashMap::new();
        writes.insert(1, Value::U64(10));
        writes.insert(3, Value::U64(30));
        kv.apply(1, &writes).unwrap();

        let view = kv.namespace(1);
        let results = view.get_many(&[1, 2, 3]);
        assert_eq!(results.len(), 3);
        assert!(results[0].is_some());
        assert!(results[1].is_none());
        assert!(results[2].is_some());
    }

    #[test]
    fn commit_with_occ_succeeds_when_reads_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let kv = RocksKv::open(dir.path()).unwrap();
        // Seed cell 1 = 100.
        let mut seed = HashMap::new();
        seed.insert(1u128, Value::U64(100));
        kv.apply(1, &seed).unwrap();

        // Pretend a rend tx read cell 1 = 100 and now wants to
        // write cell 1 = 200. No concurrent change → succeeds.
        let mut reads = HashMap::new();
        reads.insert(1u128, Value::U64(100));
        let mut writes = HashMap::new();
        writes.insert(1u128, Value::U64(200));
        let r = kv.commit_with_occ(1, &reads, &writes, &HashMap::new()).unwrap();
        assert_eq!(r, CommitResult::Committed);

        // Verify post-commit state.
        let view = kv.namespace(1);
        let bytes = view.get(1).unwrap();
        assert_eq!(
            rend::serialize::deserialize(&bytes, &rend::ast::Type::U64),
            Some(Value::U64(200)),
        );
    }

    #[test]
    fn commit_with_occ_detects_stale_read() {
        let dir = tempfile::tempdir().unwrap();
        let kv = RocksKv::open(dir.path()).unwrap();
        // Seed cell 1 = 100.
        let mut seed = HashMap::new();
        seed.insert(1u128, Value::U64(100));
        kv.apply(1, &seed).unwrap();

        // Some other tx already moved 1 → 999.
        let mut other = HashMap::new();
        other.insert(1u128, Value::U64(999));
        kv.apply(1, &other).unwrap();

        // Our tx's recorded read still says 100. Validation rejects.
        let mut reads = HashMap::new();
        reads.insert(1u128, Value::U64(100));
        let mut writes = HashMap::new();
        writes.insert(1u128, Value::U64(200));
        let r = kv.commit_with_occ(1, &reads, &writes, &HashMap::new()).unwrap();
        assert_eq!(r, CommitResult::Conflict);

        // State unchanged from the other tx's value.
        let view = kv.namespace(1);
        let bytes = view.get(1).unwrap();
        assert_eq!(
            rend::serialize::deserialize(&bytes, &rend::ast::Type::U64),
            Some(Value::U64(999)),
        );
    }

    #[test]
    fn bump_nonce_accepts_strictly_increasing() {
        let dir = tempfile::tempdir().unwrap();
        let kv = RocksKv::open(dir.path()).unwrap();
        let addr = [0xAA; 20];
        assert_eq!(kv.bump_nonce(&addr, 1).unwrap(), NonceResult::Bumped);
        assert_eq!(kv.bump_nonce(&addr, 2).unwrap(), NonceResult::Bumped);
        // Same nonce twice → reject (replay).
        assert_eq!(
            kv.bump_nonce(&addr, 2).unwrap(),
            NonceResult::TooLow { stored: 2 },
        );
        // Older nonce → reject.
        assert_eq!(
            kv.bump_nonce(&addr, 1).unwrap(),
            NonceResult::TooLow { stored: 2 },
        );
        // Skip ahead is fine.
        assert_eq!(kv.bump_nonce(&addr, 100).unwrap(), NonceResult::Bumped);
    }

    #[test]
    fn bump_nonce_isolates_addresses() {
        let dir = tempfile::tempdir().unwrap();
        let kv = RocksKv::open(dir.path()).unwrap();
        let a = [1u8; 20];
        let b = [2u8; 20];
        assert_eq!(kv.bump_nonce(&a, 5).unwrap(), NonceResult::Bumped);
        // b's nonce starts at 0 regardless of a's history.
        assert_eq!(kv.bump_nonce(&b, 1).unwrap(), NonceResult::Bumped);
        assert_eq!(
            kv.bump_nonce(&b, 1).unwrap(),
            NonceResult::TooLow { stored: 1 },
        );
    }

    /// Build a pmap on `kv` (namespace 1) by inserting `entries`.
    /// Returns the resulting root hash and the (read, write) sets
    /// the runtime would have produced — wired so the commit path
    /// can validate them.
    #[cfg(test)]
    fn build_pmap_in_ns(
        kv: &RocksKv,
        starting_root: u128,
        entries: &[(i64, i64)],
    ) -> (u128, HashMap<u128, Value>, HashMap<u128, Value>) {
        let view = kv.namespace(1);
        let mut tx = rend::tx::Tx::new(&view);
        let mut h = starting_root;
        for (k, v) in entries {
            h = rend::pmap::set(
                h, Value::Int(*k), Value::Int(*v),
                &mut tx,
                &Type::Int, &Type::Int,
            ).unwrap();
        }
        let (reads, writes, _, _, _, _) = tx.into_full();
        (h, reads, writes)
    }

    #[test]
    fn commit_merges_disjoint_pmap_writes() {
        // Two transactions both built on the same starting pmap
        // root, each inserting a different key. The first commits;
        // the second's commit sees the root cell has changed but
        // the 3-way merge succeeds (disjoint subtrees) — both writes
        // land. Without the merge wiring this would 409.
        let dir = tempfile::tempdir().unwrap();
        let kv = RocksKv::open(dir.path()).unwrap();
        const ROOT_CELL: u128 = 0xACC0u128;

        // Seed: build a base pmap with one entry, write it as the
        // state cell's PMap pointer.
        let (base_root, _seed_reads, seed_writes) = build_pmap_in_ns(&kv, 0, &[(1, 10)]);
        let mut seed = seed_writes;
        seed.insert(ROOT_CELL, Value::PMap(base_root));
        kv.apply(1, &seed).unwrap();

        // Tx A: read base_root, set key 99. (Inserts under one HAMT slot.)
        let (root_a, _, writes_a_nodes) = build_pmap_in_ns(&kv, base_root, &[(99, 9900)]);
        let mut reads_a = HashMap::new();
        reads_a.insert(ROOT_CELL, Value::PMap(base_root));
        let mut writes_a = writes_a_nodes;
        writes_a.insert(ROOT_CELL, Value::PMap(root_a));

        // Tx B: also reads base_root, sets a DIFFERENT key (2).
        let (root_b, _, writes_b_nodes) = build_pmap_in_ns(&kv, base_root, &[(2, 20)]);
        let mut reads_b = HashMap::new();
        reads_b.insert(ROOT_CELL, Value::PMap(base_root));
        let mut writes_b = writes_b_nodes;
        writes_b.insert(ROOT_CELL, Value::PMap(root_b));

        let mut pmap_types = HashMap::new();
        pmap_types.insert(ROOT_CELL, (Type::Int, Type::Int));

        // Commit A — no contention, succeeds.
        let r = kv.commit_with_occ(1, &reads_a, &writes_a, &pmap_types).unwrap();
        assert_eq!(r, CommitResult::Committed);

        // Commit B — root cell has changed under us. Without merge,
        // this would Conflict. With merge, A's insert (key 99) and
        // B's insert (key 2) hit disjoint subtrees → merge succeeds.
        let r = kv.commit_with_occ(1, &reads_b, &writes_b, &pmap_types).unwrap();
        assert_eq!(r, CommitResult::Committed,
            "disjoint-key writes on a pmap must merge instead of 409");

        // Both keys must be present in the final tree.
        let view = kv.namespace(1);
        let final_root_bytes = view.get(ROOT_CELL).unwrap();
        let final_root_v = rend::serialize::deserialize(
            &final_root_bytes,
            &Type::PMap { key: Box::new(Type::Int), value: Box::new(Type::Int) },
        ).unwrap();
        let final_root = match final_root_v { Value::PMap(h) => h, _ => panic!() };
        let mut tx = rend::tx::Tx::new(&view);
        for (k, expected) in &[(1, 10), (2, 20), (99, 9900)] {
            assert_eq!(
                rend::pmap::get(final_root, &Value::Int(*k), &mut tx, &Type::Int, &Type::Int),
                Some(Value::Int(*expected)),
                "key {k} missing from merged tree",
            );
        }
    }

    #[test]
    fn commit_conflict_when_same_pmap_key_changes() {
        // Both txs modify the SAME key — merge returns None and we
        // surface Conflict so OCC retry kicks in.
        let dir = tempfile::tempdir().unwrap();
        let kv = RocksKv::open(dir.path()).unwrap();
        const ROOT_CELL: u128 = 0xACC0u128;

        let (base_root, _, seed_writes) = build_pmap_in_ns(&kv, 0, &[(1, 10)]);
        let mut seed = seed_writes;
        seed.insert(ROOT_CELL, Value::PMap(base_root));
        kv.apply(1, &seed).unwrap();

        // Both txs overwrite key=1 to different values.
        let (root_a, _, writes_a_nodes) = build_pmap_in_ns(&kv, base_root, &[(1, 100)]);
        let mut reads_a = HashMap::new();
        reads_a.insert(ROOT_CELL, Value::PMap(base_root));
        let mut writes_a = writes_a_nodes;
        writes_a.insert(ROOT_CELL, Value::PMap(root_a));

        let (root_b, _, writes_b_nodes) = build_pmap_in_ns(&kv, base_root, &[(1, 200)]);
        let mut reads_b = HashMap::new();
        reads_b.insert(ROOT_CELL, Value::PMap(base_root));
        let mut writes_b = writes_b_nodes;
        writes_b.insert(ROOT_CELL, Value::PMap(root_b));

        let mut pmap_types = HashMap::new();
        pmap_types.insert(ROOT_CELL, (Type::Int, Type::Int));

        assert_eq!(
            kv.commit_with_occ(1, &reads_a, &writes_a, &pmap_types).unwrap(),
            CommitResult::Committed,
        );
        assert_eq!(
            kv.commit_with_occ(1, &reads_b, &writes_b, &pmap_types).unwrap(),
            CommitResult::Conflict,
            "same-key writes must surface as conflict",
        );
    }

    #[test]
    fn commit_with_occ_treats_default_as_missing() {
        // rend reads a missing cell as the value-type default. The
        // OCC validator must accept "no value on disk" as a match
        // for a recorded default-valued read.
        let dir = tempfile::tempdir().unwrap();
        let kv = RocksKv::open(dir.path()).unwrap();
        let mut reads = HashMap::new();
        reads.insert(99u128, Value::U64(0));   // default for u64
        let mut writes = HashMap::new();
        writes.insert(99u128, Value::U64(42));
        let r = kv.commit_with_occ(1, &reads, &writes, &HashMap::new()).unwrap();
        assert_eq!(r, CommitResult::Committed);
    }
}
