//! Garbage collection for tree-spread persistent collections.
//!
//! Each pmap/pvec write produces fresh content-addressed node cells
//! along the path it modified. Old path cells become unreachable
//! once the state cell's root pointer is updated, but they linger
//! in KV until something sweeps them. This module is that sweeper.
//!
//! ## Why a candidate set?
//!
//! Content-addressed node cells share a key space with whatever
//! else is in the KV (regular state cells, `map<K,V>` per-key
//! cells, etc.). The KV doesn't know which is which. Naively
//! sweeping "every cell not in the live set" would also delete
//! `map<K,V>` cells whose state root happens to be in the live
//! set but whose per-key cells aren't.
//!
//! So GC takes an explicit `candidates` set: cells eligible for
//! sweep. The runtime tracks this on every pmap/pvec write
//! (`Tx::record_node_cell_write`), the engine returns it via
//! `ExecOutcome::node_cells_written`, and the host accumulates
//! candidates across whatever commits it wants to GC over.
//!
//! ## Live set
//!
//! `live_states` lists the (state_cell, declared_type) pairs the
//! caller wants preserved. For each pmap/pvec entry we walk the
//! current root and add every reachable node cell to the live
//! set. State cells themselves are also added (they're keep-by-
//! definition). Anything in `candidates` not in the live set is
//! removed.
//!
//! ## Cross-state sharing
//!
//! Two persistent collections may share node cells via content-
//! addressing — e.g., two pmaps both end up with an interior
//! subtree that has identical contents. The reachability walk
//! handles this correctly: the cell is reachable from either
//! state, so it stays in the live set.

use std::collections::HashSet;

use crate::ast::Type;
use crate::kv::{InMemoryKv, Kv};
use crate::value::Value;

#[derive(Debug)]
pub struct GcReport {
    /// Number of cells in the final live set.
    pub kept: usize,
    /// Number of node-cell candidates removed from the KV.
    pub swept: usize,
}

/// Sweep node-cell candidates that aren't reachable from any live
/// state. The caller passes:
///
///   * `live_states` — `(cell_key, type)` for every state slot
///     the host wants preserved. Pmap/pvec entries drive a tree
///     walk; other types are kept on the basis of their cell key
///     alone.
///   * `candidates` — cell keys eligible for sweep, gathered from
///     `ExecOutcome::node_cells_written` across whatever commits
///     this GC pass should reclaim.
///   * `kv` — the live store; mutated in place.
pub fn sweep(
    live_states: &[(u128, Type)],
    candidates: &HashSet<u128>,
    kv: &mut InMemoryKv,
) -> GcReport {
    let mut live: HashSet<u128> = HashSet::new();

    for (cell, ty) in live_states {
        // Every named state cell is keep-by-definition.
        live.insert(*cell);
        match ty {
            Type::PMap { key, value } => {
                if let Some(bytes) = kv.get(*cell) {
                    if let Some(Value::PMap(root)) = crate::serialize::deserialize(&bytes, ty) {
                        live.extend(crate::pmap::reachable_cells(root, kv as &dyn Kv, key, value));
                    }
                }
            }
            Type::PVec { elem } => {
                if let Some(bytes) = kv.get(*cell) {
                    if let Some(Value::PVec { root, .. }) = crate::serialize::deserialize(&bytes, ty) {
                        live.extend(crate::pvec::reachable_cells(root, kv as &dyn Kv, elem));
                    }
                }
            }
            _ => {}
        }
    }

    let mut swept = 0;
    for candidate in candidates {
        if !live.contains(candidate) && kv.data.remove(candidate).is_some() {
            swept += 1;
        }
    }
    GcReport { kept: live.len(), swept }
}
