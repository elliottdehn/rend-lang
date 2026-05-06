//! RocksDB-backed `rend::kv::Kv` implementation.
//!
//! Two layers:
//!
//!   * [`RocksKv`] — thin wrapper around a `rocksdb::DB`. One per
//!     server process; opened with the durability and write-batch
//!     defaults the rend runtime expects.
//!
//!   * [`NamespacedKv`] — read-only view that prefixes every cell
//!     key with an opaque namespace tag (typically an org id). One
//!     per tx. The rend runtime reads through the wrapper; commits
//!     are applied to the underlying DB by [`RocksKv::apply`].
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
//! The namespace prefix gives us per-org isolation: two orgs touching
//! the same content-addressed cell key (e.g., a hash that happens to
//! collide) end up at different on-disk keys, so commits are
//! independent.

use rend::kv::Kv;
use rend::value::Value;
use rocksdb::{DB, Options, WriteBatch};
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
    db: Arc<DB>,
}

impl RocksKv {
    /// Open or create a RocksDB at `path`. Tuned for the rend
    /// workload: small-value-heavy, frequent writes, occasional
    /// large reads via prefix scan.
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Self, rocksdb::Error> {
        let mut opts = Options::default();
        opts.create_if_missing(true);
        opts.set_compression_type(rocksdb::DBCompressionType::Snappy);
        // Bigger memtable cuts write amplification on indexed-write
        // workloads (every primary write also touches multiple
        // index cells).
        opts.set_write_buffer_size(64 * 1024 * 1024);
        let db = DB::open(&opts, path)?;
        Ok(Self { db: Arc::new(db) })
    }

    /// Apply a commit's write set in one batch. The rend
    /// `ExecOutcome::writes` is `HashMap<u128, Value>`; we
    /// serialize each value and write under the namespaced key.
    pub fn apply(
        &self,
        ns: NamespaceId,
        writes: &std::collections::HashMap<u128, Value>,
    ) -> Result<(), rocksdb::Error> {
        let mut batch = WriteBatch::default();
        for (cell, value) in writes {
            let key = encode_key(ns, *cell);
            let bytes = rend::serialize::serialize(value);
            batch.put(key, bytes);
        }
        self.db.write(batch)
    }

    /// Build a per-tx Kv view over this database scoped to one
    /// namespace. The view is cheap: it just clones the `Arc`.
    pub fn namespace(&self, ns: NamespaceId) -> NamespacedKv {
        NamespacedKv { db: self.db.clone(), ns }
    }
}

/// Per-tx, per-namespace read view. Implements the rend `Kv` trait
/// so the runtime can read through it. Writes go via
/// [`RocksKv::apply`] after the runtime returns its outcome.
pub struct NamespacedKv {
    db: Arc<DB>,
    ns: NamespaceId,
}

impl Kv for NamespacedKv {
    fn get(&self, key: u128) -> Option<Vec<u8>> {
        let full = encode_key(self.ns, key);
        match self.db.get(full) {
            Ok(Some(bytes)) => Some(bytes),
            Ok(None) => None,
            Err(_) => None,   // treat IO errors as "missing"; commit-time read-set check will surface mismatches
        }
    }

    fn get_many(&self, keys: &[u128]) -> Vec<Option<Vec<u8>>> {
        // RocksDB's multi_get is one call to the engine but issues
        // one disk read per key behind the scenes. Still cuts the
        // RPC overhead vs N independent gets.
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
}
