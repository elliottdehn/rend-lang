//! Pluggable key-value storage backend.
//!
//! Keys are `u128` namespaces — derived deterministically by the runtime from
//! state declarations and (for map cells) hashed key payloads — and values
//! are opaque byte vectors. Hosts implementing real backends (RocksDB, an
//! HTTP API, a blob in S3, …) only need to handle bytes.
//!
//! The runtime never mutates the KV in place during execution. Writes
//! accumulate in a per-tx buffer and are returned to the host alongside the
//! observed read set as part of [`crate::engine::ExecOutcome`]. The host
//! decides how/when to apply.

use std::collections::HashMap;

use crate::serialize::serialize;
use crate::value::Value;

pub trait Kv: Sync {
    fn get(&self, key: u128) -> Option<Vec<u8>>;

    /// Batch read. Backends with native pipelining (RocksDB MultiGet,
    /// Redis pipeline, network RPC) should override this to issue a
    /// single round-trip; the default implementation falls back to N
    /// independent calls. The optimizer in `crate::optimize` clusters
    /// independent state reads so the runtime can call `get_many` with
    /// the whole cluster at once.
    fn get_many(&self, keys: &[u128]) -> Vec<Option<Vec<u8>>> {
        keys.iter().map(|k| self.get(*k)).collect()
    }

    /// Single-cell write. Default is a no-op so read-only sentinels
    /// like `EmptyKv` don't have to override it; backends with
    /// persistent storage do override.
    fn put(&mut self, _key: u128, _value: Vec<u8>) {}

    /// Batch write. Mirror of `get_many` for the commit path. Default
    /// loops over `put`; backends with native batch APIs (RocksDB
    /// WriteBatch, transaction commit, etc.) override to apply the
    /// entire write set in one round-trip.
    fn put_many(&mut self, writes: &[(u128, Vec<u8>)]) {
        for (k, v) in writes {
            self.put(*k, v.clone());
        }
    }

    /// Apply a typed write set: serialize each value once and commit
    /// via `put_many`. Backends never see Pending values — the runtime
    /// has already forced them — so serialization is straightforward.
    fn apply_writes(&mut self, writes: &HashMap<u128, Value>) {
        if writes.is_empty() { return; }
        let bytes: Vec<(u128, Vec<u8>)> = writes
            .iter()
            .map(|(k, v)| (*k, serialize(v)))
            .collect();
        self.put_many(&bytes);
    }
}

/// In-memory KV. Useful for tests and local-only embeddings.
#[derive(Default, Clone, Debug)]
pub struct InMemoryKv {
    pub data: HashMap<u128, Vec<u8>>,
}

impl InMemoryKv {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn put_raw(&mut self, key: u128, bytes: Vec<u8>) {
        self.data.insert(key, bytes);
    }

    pub fn put_value(&mut self, key: u128, value: &Value) {
        self.data.insert(key, serialize(value));
    }

    /// Retrieve a typed value at `key`, deserializing under the expected
    /// type. Returns `None` on miss or tag mismatch. Convenient for tests.
    pub fn get_typed(&self, key: u128, ty: &crate::ast::Type) -> Option<Value> {
        self.data
            .get(&key)
            .and_then(|bytes| crate::serialize::deserialize(bytes, ty))
    }

    /// Apply a write set into the store. Used by the host after a
    /// successful commit. Routes through `Kv::apply_writes` so any
    /// trait-level `put_many` overrides are honored.
    pub fn apply(&mut self, writes: &HashMap<u128, Value>) {
        self.apply_writes(writes);
    }
}

impl Kv for InMemoryKv {
    fn get(&self, key: u128) -> Option<Vec<u8>> {
        self.data.get(&key).cloned()
    }

    fn put(&mut self, key: u128, value: Vec<u8>) {
        self.data.insert(key, value);
    }

    fn put_many(&mut self, writes: &[(u128, Vec<u8>)]) {
        // Reserve once, then drain each pair — avoids per-write rehash
        // when the batch is large.
        self.data.reserve(writes.len());
        for (k, v) in writes {
            self.data.insert(*k, v.clone());
        }
    }
}

/// Empty KV — returns None for every key. Used when running modules with
/// no state declarations, or as a sentinel.
pub struct EmptyKv;

impl Kv for EmptyKv {
    fn get(&self, _: u128) -> Option<Vec<u8>> {
        None
    }
}
