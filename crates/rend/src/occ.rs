//! Optimistic-concurrency commit driver.
//!
//! Pattern:
//!   1. Take a stable snapshot of the KV.
//!   2. Run every tx against that *same* snapshot in parallel (rayon),
//!      collecting its `(read_set, write_set, result)`.
//!   3. Validate + commit each tx in order against the live KV:
//!        * if the tx's read set still matches the live KV → apply writes;
//!        * otherwise re-execute the tx against the live KV (its
//!          re-execution is deterministic since the language is) and apply
//!          the new write set.
//!
//! Disjoint RW sets compose; a conflict only forces *the conflicting tx* to
//! re-execute, not the others. The driver preserves a host-chosen serial
//! order so the final state is identical to running the txs sequentially in
//! that order.

use std::collections::HashMap;

use rayon::prelude::*;

use crate::engine::{Engine, ExecOutcome};
use crate::error::Error;
use crate::kv::{InMemoryKv, Kv};
use crate::serialize;
use crate::value::Value;
use crate::vm::Fuel;

/// Read-only KV view that layers `overlay` (a tx's pending writes,
/// stored as `Value`s) on top of `base`. Used by the merge path so
/// it can walk HAMT nodes from both the in-flight tx and the live
/// store without cloning either. `overlay` wins on lookup.
struct StackedKv<'a> {
    overlay: &'a HashMap<u128, Value>,
    base: &'a dyn Kv,
}

impl<'a> Kv for StackedKv<'a> {
    fn get(&self, key: u128) -> Option<Vec<u8>> {
        if let Some(v) = self.overlay.get(&key) {
            return Some(serialize::serialize(v));
        }
        self.base.get(key)
    }
    // get_many / put / put_many use default impls. The stacked view
    // is read-only — writes through it would have nowhere coherent
    // to land, so leaving the default no-ops is correct.
}

#[derive(Clone, Copy)]
pub struct TxRequest<'a> {
    pub src: &'a str,
    pub fuel_first: u64,
    pub fuel_retry: u64,
}

impl<'a> TxRequest<'a> {
    pub fn new(src: &'a str, fuel: u64) -> Self {
        Self { src, fuel_first: fuel, fuel_retry: fuel }
    }
}

#[derive(Debug)]
pub struct TxReport {
    pub result: Value,
    pub re_executed: bool,
}

#[derive(Debug, Default)]
pub struct BatchReport {
    pub txs: Vec<TxReport>,
    /// Count of txs that had to re-execute because their read set
    /// no longer matched the live KV at commit time.
    pub conflicts: usize,
    /// Count of txs that *would* have re-executed under classic OCC
    /// but were salvaged by a 3-way HAMT merge on a pmap state cell.
    /// The original speculative result still applies; the runtime
    /// just substitutes the merged root + new node cells in place
    /// of the tx's own root write.
    pub merged: usize,
}

/// Run a batch of txs with OCC semantics. The KV is mutated in place to
/// reflect the committed final state. Returns per-tx results plus a count
/// of re-executions caused by validation failures.
pub fn commit_batch(
    engine: &Engine,
    kv: &mut InMemoryKv,
    txs: &[TxRequest<'_>],
) -> Result<BatchReport, Error> {
    // Phase 1: parallel speculative execution against an immutable snapshot.
    let snapshot = kv.clone();
    let speculative: Result<Vec<ExecOutcome>, Error> = txs
        .par_iter()
        .map(|tx| engine.execute(tx.src, Fuel::new(tx.fuel_first), &snapshot))
        .collect();
    let speculative = speculative?;

    // Phase 2: validate + commit serially.
    let mut report = BatchReport::default();
    for (i, outcome) in speculative.into_iter().enumerate() {
        if validate(&outcome.reads, kv) {
            kv.apply(&outcome.writes);
            report.txs.push(TxReport { result: outcome.result, re_executed: false });
        } else if let Some(adjusted) = try_persistent_merge(&outcome, kv, &snapshot) {
            // Salvage: the only mismatched reads were on pmap state
            // cells, and the changes 3-way-merged cleanly. Apply the
            // adjusted writes (merged roots + freshly synthesized
            // node cells) instead of re-executing.
            kv.apply(&adjusted);
            report.merged += 1;
            report.txs.push(TxReport { result: outcome.result, re_executed: false });
        } else {
            report.conflicts += 1;
            let re = engine.execute(txs[i].src, Fuel::new(txs[i].fuel_retry), kv as &dyn Kv)?;
            kv.apply(&re.writes);
            report.txs.push(TxReport { result: re.result, re_executed: true });
        }
    }
    Ok(report)
}

/// On classic-OCC validation failure, see if the only mismatches are
/// persistent-collection state-cell writes (pmap or pvec) whose
/// divergent roots can be 3-way-merged against the snapshot. Returns
/// an adjusted write set on success — the tx's root writes are
/// replaced with the merged roots and any newly synthesized node
/// cells are added to the set. On any mismatch the merge can't
/// resolve, returns `None` and the caller re-executes.
fn try_persistent_merge(
    outcome: &ExecOutcome,
    live: &InMemoryKv,
    snapshot: &InMemoryKv,
) -> Option<HashMap<u128, Value>> {
    // Walk reads. For each read whose live value differs, the cell
    // must be a persistent-collection state cell (pmap/pvec) that
    // this tx also wrote — else the mismatch isn't mergeable.
    let mut mergeable_cells: Vec<u128> = Vec::new();
    for (key, observed) in &outcome.reads {
        let live_match = match live.get(*key) {
            Some(bytes) => bytes == serialize::serialize(observed),
            None => serialize::is_default(observed),
        };
        if live_match { continue; }
        let is_persistent = matches!(observed, Value::PMap(_) | Value::PVec { .. });
        let wrote = outcome.writes.get(key).is_some();
        if !is_persistent || !wrote {
            return None;
        }
        mergeable_cells.push(*key);
    }
    if mergeable_cells.is_empty() {
        return Some(outcome.writes.clone());
    }
    let stacked = StackedKv { overlay: &outcome.writes, base: live };
    let mut adjusted = outcome.writes.clone();
    for cell in mergeable_cells {
        // Dispatch on what kind of persistent collection lives at
        // this cell. The tx recorded the type when it touched the
        // cell (via PMapGet/PMapPut or PVecGet/PVecSet/etc.).
        if let Some((key_ty, val_ty)) = outcome.pmap_types.get(&cell).cloned() {
            let pmap_ty = crate::ast::Type::PMap {
                key: Box::new(key_ty.clone()),
                value: Box::new(val_ty.clone()),
            };
            let new_a = match outcome.writes.get(&cell) {
                Some(Value::PMap(h)) => *h,
                _ => return None,
            };
            let live_b = read_pmap_root(&pmap_ty, live, cell).unwrap_or(crate::pmap::EMPTY);
            let ancestor = read_pmap_root(&pmap_ty, snapshot, cell).unwrap_or(crate::pmap::EMPTY);
            let merged = crate::pmap::merge_three_way(new_a, live_b, ancestor, &stacked, &key_ty, &val_ty)?;
            adjusted.insert(cell, Value::PMap(merged.root));
            for (cell_key, bytes) in merged.new_cells {
                adjusted.insert(cell_key, Value::Bytes(bytes));
            }
        } else if let Some(elem_ty) = outcome.pvec_types.get(&cell).cloned() {
            let pvec_ty = crate::ast::Type::PVec { elem: Box::new(elem_ty.clone()) };
            let (new_root, new_len) = match outcome.writes.get(&cell) {
                Some(Value::PVec { len, root }) => (*root, *len),
                _ => return None,
            };
            let (live_root, live_len) = read_pvec_root(&pvec_ty, live, cell)
                .unwrap_or((crate::pvec::EMPTY, 0));
            let (anc_root, anc_len) = read_pvec_root(&pvec_ty, snapshot, cell)
                .unwrap_or((crate::pvec::EMPTY, 0));
            let merged = crate::pvec::merge_three_way(
                new_root, new_len,
                live_root, live_len,
                anc_root, anc_len,
                &stacked,
                &elem_ty,
            )?;
            adjusted.insert(cell, Value::PVec { len: new_len, root: merged.root });
            for (cell_key, bytes) in merged.new_cells {
                adjusted.insert(cell_key, Value::Bytes(bytes));
            }
        } else {
            // Mismatched persistent-collection cell whose K/V types
            // we don't have. Without them we can't decode nodes.
            return None;
        }
    }
    Some(adjusted)
}

fn read_pmap_root(pmap_ty: &crate::ast::Type, kv: &InMemoryKv, cell: u128) -> Option<u128> {
    let bytes = kv.get(cell)?;
    match crate::serialize::deserialize(&bytes, pmap_ty) {
        Some(Value::PMap(h)) => Some(h),
        _ => None,
    }
}

fn read_pvec_root(pvec_ty: &crate::ast::Type, kv: &InMemoryKv, cell: u128) -> Option<(u128, u64)> {
    let bytes = kv.get(cell)?;
    match crate::serialize::deserialize(&bytes, pvec_ty) {
        Some(Value::PVec { len, root }) => Some((root, len)),
        _ => None,
    }
}

fn validate(reads: &HashMap<u128, Value>, kv: &dyn Kv) -> bool {
    reads.iter().all(|(key, observed)| {
        match kv.get(*key) {
            Some(bytes) => bytes == serialize::serialize(observed),
            // None ≡ key still unset ≡ effective value is the default; the
            // observed value is consistent iff it *was* the default at exec
            // time (which it must have been, since KV had no entry).
            None => serialize::is_default(observed),
        }
    })
}
