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

use rend::kv::Kv;
use rend::value::Value;
use rocksdb::{
    ErrorKind, OptimisticTransactionDB, Options, SingleThreaded, WriteBatchWithTransaction,
};
use std::collections::HashMap;
use std::sync::Arc;

pub type NamespaceId = u64;

const NAMESPACE_PREFIX_LEN: usize = 8;
const CELL_KEY_LEN: usize = 16;
const FULL_KEY_LEN: usize = NAMESPACE_PREFIX_LEN + CELL_KEY_LEN;

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

impl RocksKv {
    /// Open or create a RocksDB at `path`. Uses an
    /// `OptimisticTransactionDB` so multi-tx commit conflict
    /// detection is built in.
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Self, rocksdb::Error> {
        let mut opts = Options::default();
        opts.create_if_missing(true);
        opts.set_compression_type(rocksdb::DBCompressionType::Snappy);
        // Bigger memtable cuts write amplification on indexed-write
        // workloads (every primary write also touches multiple
        // index cells).
        opts.set_write_buffer_size(64 * 1024 * 1024);
        let db = OptimisticTransactionDB::open(&opts, path)?;
        Ok(Self { db: Arc::new(db) })
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
    pub fn commit_with_occ(
        &self,
        ns: NamespaceId,
        reads: &HashMap<u128, Value>,
        writes: &HashMap<u128, Value>,
    ) -> Result<CommitResult, rocksdb::Error> {
        let txn = self.db.transaction();

        // 1+2: validate the read set.
        for (cell, expected) in reads {
            let on_disk = txn.get_for_update(encode_key(ns, *cell), true)?;
            if !read_matches(&on_disk, expected) {
                // Don't bother committing — we already know we'd
                // conflict at the rend logical level.
                return Ok(CommitResult::Conflict);
            }
        }

        // 3: stage writes.
        for (cell, value) in writes {
            txn.put(encode_key(ns, *cell), rend::serialize::serialize(value))?;
        }

        // 4: commit. Rocks surfaces "another tx wrote to a key we
        // get_for_update'd" as Busy/TryAgain.
        match txn.commit() {
            Ok(()) => Ok(CommitResult::Committed),
            Err(e) => match e.kind() {
                ErrorKind::Busy | ErrorKind::TryAgain => Ok(CommitResult::Conflict),
                _ => Err(e),
            },
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
        let r = kv.commit_with_occ(1, &reads, &writes).unwrap();
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
        let r = kv.commit_with_occ(1, &reads, &writes).unwrap();
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
        let r = kv.commit_with_occ(1, &reads, &writes).unwrap();
        assert_eq!(r, CommitResult::Committed);
    }
}
