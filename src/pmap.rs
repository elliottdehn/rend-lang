//! Persistent map (HAMT), tree-spread storage. Each node lives in
//! its own content-addressed KV cell; the state slot stores just the
//! root hash. Two transactions inserting into disjoint subtrees
//! touch disjoint cell sets and so commit in parallel under OCC.
//!
//! The API surface this module exposes:
//!
//!   * `Value::PMap(u128)` — runtime value carrying the root hash.
//!     Zero is the canonical empty tree.
//!   * `pmap::get(root_hash, key, tx, key_ty, val_ty) -> Option<Value>`
//!   * `pmap::set(root_hash, key, value, tx, key_ty, val_ty) -> u128`
//!   * `pmap::remove(...)`, `pmap::contains(...)`
//!
//! Note: there is no `len`. Maintaining a count would force every
//! insert/remove to write a new root node carrying the updated total,
//! which is bookkeeping the caller can do trivially with their own
//! state cell if they need it.
//!
//! The HAMT is 32-way (5 bits per level). A 64-bit key hash gives us
//! ~12 levels before bits run out; we fall back to a collision list
//! beyond `MAX_LEVELS` to keep adversarial inputs from blowing the
//! recursion stack.
//!
//! ### Node wire format (per cell)
//!
//! Inner: `0x01 | bitmap[u32_be] | n_children[u8] |
//!         child_hash_1[u128_be] | ... | child_hash_n[u128_be]`
//!
//! Leaf:  `0x02 | n_entries[u32_be] |
//!         (for each entry: key_len[u32_be] | key_bytes |
//!                          val_len[u32_be] | val_bytes)`
//!
//! Leaf entries are sorted by their serialized key bytes before
//! hashing, so two leaves carrying the same set of (k, v) pairs
//! hash identically — the property that gives us cross-pmap
//! structural sharing. Inner nodes carry no size metadata: it would
//! force every insert/remove to write a new root and so make `len`
//! impossible to compute without rewriting the path on every op.
//! Callers that need the size should track it themselves in a
//! separate state cell.

use crate::ast::Type;
use crate::error::Error;
use crate::tx::Tx;
use crate::value::Value;

/// 5 bits per level → 32 children per inner node.
const BRANCH: u32 = 32;
const BITS_PER_LEVEL: u32 = 5;
/// Hash exhaustion fallback: at this level, we stop splitting and
/// pile colliding keys into a single leaf's collision list. With 5
/// bits/level and a 64-bit hash, we have headroom up to 12 levels;
/// 10 leaves plenty of slack and bounds recursion.
const MAX_LEVELS: u32 = 10;

/// Tag byte for an Inner-node cell. The first byte of a node-cell's
/// payload selects which deserializer to run.
const TAG_INNER: u8 = 0x01;
/// Tag byte for a Leaf-node cell.
const TAG_LEAF:  u8 = 0x02;

/// Sentinel root hash for an empty tree. We never write a cell at
/// this hash; the state slot's "empty pmap" reads back as 0.
pub const EMPTY: u128 = 0;

/// Internal: "transient" view of a fetched node. Reconstructed from
/// bytes for the duration of a single op step, then dropped. We never
/// build whole trees in memory — only O(log32 N) nodes are live at a
/// time.
/// Internal representation of a HAMT node, exposed to the VM's
/// streaming-cursor code (`pmap_walk_advance` in `vm.rs`) so the
/// walk can descend lazily without re-implementing node decoding.
pub enum Node {
    Inner {
        bitmap: u32,
        children: Vec<u128>,   // dense; len == bitmap.count_ones()
    },
    Leaf {
        entries: Vec<(Value, Value)>,  // sorted by serialized-key bytes
    },
}

/// Cell key used to store a node whose serialized form is `bytes`.
/// Wrapping with a fixed namespace bytes-prefix keeps node cells
/// disjoint from state-root cells (which derive from
/// `hashing::state_root`, a different input shape).
fn node_cell_key(bytes: &[u8]) -> u128 {
    crate::hashing::child(NODE_NS, bytes)
}

/// Magic constant that namespaces node-cell keys away from state
/// cells. The actual value doesn't matter so long as it's distinct
/// from anything `state_root` could produce.
const NODE_NS: u128 = 0x504D_4150_4E4F_4445_504D_4150_4E4F_4445;

/// Read an existing node from KV. Returns `None` only if the cell is
/// missing, which (for a non-empty tree) shouldn't happen during a
/// well-formed walk.
pub fn read_node(tx: &mut Tx, hash: u128, key_ty: &Type, val_ty: &Type) -> Option<Node> {
    if hash == EMPTY {
        return None;
    }
    // The cell's value type is `bytes` — we manage the node-format
    // decoding ourselves in this module rather than asking the
    // generic serializer to understand HAMT internals.
    let v = tx.read_cell(hash, &Type::Bytes);
    let v = tx.force(v);
    let bytes = match v {
        Value::Bytes(b) => b,
        _ => return None,
    };
    deserialize_node(&bytes, key_ty, val_ty)
}

/// Write a node and return the cell key it now lives at. Cells are
/// content-addressed, so duplicate writes (same bytes) collapse.
fn write_node(tx: &mut Tx, bytes: Vec<u8>) -> u128 {
    let hash = node_cell_key(&bytes);
    tx.write(hash, Value::Bytes(bytes));
    // Record so GC can later distinguish node cells (content-
    // addressed, subject to sweep) from state cells.
    tx.record_node_cell_write(hash);
    hash
}

// ---------- ops ----------

pub fn get(
    root_hash: u128,
    key: &Value,
    tx: &mut Tx,
    key_ty: &Type,
    val_ty: &Type,
) -> Option<Value> {
    if root_hash == EMPTY {
        return None;
    }
    let h = hash_key(key);
    get_at(root_hash, key, h, 0, tx, key_ty, val_ty)
}

fn get_at(
    node_hash: u128,
    key: &Value,
    h: u64,
    level: u32,
    tx: &mut Tx,
    key_ty: &Type,
    val_ty: &Type,
) -> Option<Value> {
    let node = read_node(tx, node_hash, key_ty, val_ty)?;
    match node {
        Node::Leaf { entries } => entries
            .into_iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v),
        Node::Inner { bitmap, children, .. } => {
            let slot = bits_at(h, level);
            if bitmap & (1u32 << slot) == 0 {
                return None;
            }
            let idx = (bitmap & ((1u32 << slot) - 1)).count_ones() as usize;
            get_at(children[idx], key, h, level + 1, tx, key_ty, val_ty)
        }
    }
}

pub fn contains(
    root_hash: u128,
    key: &Value,
    tx: &mut Tx,
    key_ty: &Type,
    val_ty: &Type,
) -> bool {
    get(root_hash, key, tx, key_ty, val_ty).is_some()
}

pub fn set(
    root_hash: u128,
    key: Value,
    value: Value,
    tx: &mut Tx,
    key_ty: &Type,
    val_ty: &Type,
) -> Result<u128, Error> {
    let h = hash_key(&key);
    set_at(root_hash, key, value, h, 0, tx, key_ty, val_ty)
}

fn set_at(
    node_hash: u128,
    key: Value,
    value: Value,
    h: u64,
    level: u32,
    tx: &mut Tx,
    key_ty: &Type,
    val_ty: &Type,
) -> Result<u128, Error> {
    if node_hash == EMPTY {
        // Fresh leaf for this single entry.
        let bytes = serialize_leaf(&[(key, value)]);
        return Ok(write_node(tx, bytes));
    }
    let node = read_node(tx, node_hash, key_ty, val_ty)
        .unwrap_or(Node::Leaf { entries: Vec::new() });
    match node {
        Node::Leaf { entries } => {
            // Existing-key replacement is in-place.
            if let Some(idx) = entries.iter().position(|(k, _)| k == &key) {
                let mut new_entries = entries;
                new_entries[idx] = (key, value);
                sort_entries(&mut new_entries);
                let bytes = serialize_leaf(&new_entries);
                return Ok(write_node(tx, bytes));
            }
            if entries.is_empty() {
                let bytes = serialize_leaf(&[(key, value)]);
                return Ok(write_node(tx, bytes));
            }
            if level >= MAX_LEVELS {
                // Hash bits exhausted — collision-list fallback.
                let mut new_entries = entries;
                new_entries.push((key, value));
                sort_entries(&mut new_entries);
                let bytes = serialize_leaf(&new_entries);
                return Ok(write_node(tx, bytes));
            }
            // Split. Hand all existing entries plus the new one to
            // `build_from_entries`, which constructs an Inner at this
            // level by grouping by level-L slot and recursing into
            // any slot that still has multiple entries (each
            // recursion bumps the level — terminates).
            let mut all = entries;
            all.push((key, value));
            build_from_entries(&all, level, tx, key_ty, val_ty)
        }
        Node::Inner { bitmap, mut children } => {
            let slot = bits_at(h, level);
            let bit = 1u32 << slot;
            let idx = (bitmap & (bit - 1)).count_ones() as usize;
            if bitmap & bit == 0 {
                let leaf_bytes = serialize_leaf(&[(key, value)]);
                let leaf_hash = write_node(tx, leaf_bytes);
                children.insert(idx, leaf_hash);
                let new_inner = serialize_inner(bitmap | bit, &children);
                return Ok(write_node(tx, new_inner));
            }
            let old_child = children[idx];
            let new_child = set_at(old_child, key, value, h, level + 1, tx, key_ty, val_ty)?;
            children[idx] = new_child;
            let new_inner = serialize_inner(bitmap, &children);
            Ok(write_node(tx, new_inner))
        }
    }
}

/// Construct a sub-tree from a flat list of entries at a given
/// level. Drives leaf-splits during a `set` and is the only place
/// the trie's "build wider when slots collide" rule is encoded.
/// Each level-L recursion advances to L+1, so the call tree is
/// bounded by MAX_LEVELS.
fn build_from_entries(
    entries: &[(Value, Value)],
    level: u32,
    tx: &mut Tx,
    key_ty: &Type,
    val_ty: &Type,
) -> Result<u128, Error> {
    if entries.is_empty() {
        return Ok(EMPTY);
    }
    if entries.len() == 1 || level >= MAX_LEVELS {
        let bytes = serialize_leaf(entries);
        return Ok(write_node(tx, bytes));
    }
    // Bucket by slot at this level.
    let mut by_slot: std::collections::BTreeMap<u32, Vec<(Value, Value)>> = std::collections::BTreeMap::new();
    for (k, v) in entries {
        let kh = hash_key(k);
        let slot = bits_at(kh, level);
        by_slot.entry(slot).or_default().push((k.clone(), v.clone()));
    }
    // All entries collided into the same slot — push the whole batch
    // down a level rather than emitting a useless one-child Inner.
    if by_slot.len() == 1 {
        let (_, slot_entries) = by_slot.into_iter().next().unwrap();
        return build_from_entries(&slot_entries, level + 1, tx, key_ty, val_ty);
    }
    let mut bitmap = 0u32;
    let mut children: Vec<u128> = Vec::new();
    for (slot, slot_entries) in by_slot {
        bitmap |= 1 << slot;
        children.push(build_from_entries(&slot_entries, level + 1, tx, key_ty, val_ty)?);
    }
    let bytes = serialize_inner(bitmap, &children);
    Ok(write_node(tx, bytes))
}

pub fn remove(
    root_hash: u128,
    key: &Value,
    tx: &mut Tx,
    key_ty: &Type,
    val_ty: &Type,
) -> Result<(u128, bool), Error> {
    if root_hash == EMPTY {
        return Ok((EMPTY, false));
    }
    let h = hash_key(key);
    remove_at(root_hash, key, h, 0, tx, key_ty, val_ty)
}

fn remove_at(
    node_hash: u128,
    key: &Value,
    h: u64,
    level: u32,
    tx: &mut Tx,
    key_ty: &Type,
    val_ty: &Type,
) -> Result<(u128, bool), Error> {
    let node = match read_node(tx, node_hash, key_ty, val_ty) {
        Some(n) => n,
        None => return Ok((node_hash, false)),
    };
    match node {
        Node::Leaf { entries } => {
            if let Some(idx) = entries.iter().position(|(k, _)| k == key) {
                let mut new_entries = entries;
                new_entries.remove(idx);
                if new_entries.is_empty() {
                    return Ok((EMPTY, true));
                }
                let bytes = serialize_leaf(&new_entries);
                let h = write_node(tx, bytes);
                return Ok((h, true));
            }
            Ok((node_hash, false))
        }
        Node::Inner { bitmap, mut children } => {
            let slot = bits_at(h, level);
            let bit = 1u32 << slot;
            if bitmap & bit == 0 {
                return Ok((node_hash, false));
            }
            let idx = (bitmap & (bit - 1)).count_ones() as usize;
            let (new_child, found) = remove_at(children[idx], key, h, level + 1, tx, key_ty, val_ty)?;
            if !found {
                return Ok((node_hash, false));
            }
            if new_child == EMPTY {
                children.remove(idx);
                let new_bitmap = bitmap & !bit;
                if new_bitmap == 0 {
                    return Ok((EMPTY, true));
                }
                let new_inner = serialize_inner(new_bitmap, &children);
                let h = write_node(tx, new_inner);
                return Ok((h, true));
            }
            children[idx] = new_child;
            let new_inner = serialize_inner(bitmap, &children);
            let h = write_node(tx, new_inner);
            Ok((h, true))
        }
    }
}

// ---------- reachability ----------------------------------------
//
// Walks the tree from `root` and returns every node cell key that's
// reachable. The GC driver unions these sets across every live
// pmap/pvec state to compute the keep set.

/// Walk the entire HAMT and collect every (key, value) pair. Used by
/// the `pmap_entries` / `pmap_keys` / `pmap_values` builtins. The walk
/// is depth-first; the resulting order is hash-of-key, which is
/// deterministic but not user-meaningful — callers that need a sort
/// should sort the returned array themselves.
///
/// Cost: one cell read per node visited, O(N) where N is the number
/// of entries (since each leaf is visited once and the tree is
/// balanced log-32 wide). This is the price of replacing SQL — see
/// `docs/language/persistent-collections.md` for the trade.
pub fn entries(
    root: u128,
    tx: &mut Tx,
    key_ty: &Type,
    val_ty: &Type,
) -> Vec<(Value, Value)> {
    let mut out = Vec::new();
    if root != EMPTY {
        collect_entries(root, tx, key_ty, val_ty, &mut out);
    }
    out
}

fn collect_entries(
    hash: u128,
    tx: &mut Tx,
    key_ty: &Type,
    val_ty: &Type,
    out: &mut Vec<(Value, Value)>,
) {
    let Some(node) = read_node(tx, hash, key_ty, val_ty) else {
        return;
    };
    match node {
        Node::Leaf { entries } => out.extend(entries),
        Node::Inner { children, .. } => {
            for c in children {
                collect_entries(c, tx, key_ty, val_ty, out);
            }
        }
    }
}

pub fn reachable_cells(
    root: u128,
    kv: &dyn crate::kv::Kv,
    key_ty: &Type,
    val_ty: &Type,
) -> std::collections::HashSet<u128> {
    let mut out = std::collections::HashSet::new();
    walk_reachable(root, kv, key_ty, val_ty, &mut out);
    out
}

fn walk_reachable(
    hash: u128,
    kv: &dyn crate::kv::Kv,
    key_ty: &Type,
    val_ty: &Type,
    out: &mut std::collections::HashSet<u128>,
) {
    if hash == EMPTY { return; }
    if !out.insert(hash) {
        // Already visited — content-addressed dedup means a subtree
        // can be referenced from multiple paths, but we only need to
        // walk it once.
        return;
    }
    match read_node_kv(kv, hash, key_ty, val_ty) {
        Some(Node::Inner { children, .. }) => {
            for c in children { walk_reachable(c, kv, key_ty, val_ty, out); }
        }
        // Leaves are terminal — no further references.
        Some(Node::Leaf { .. }) | None => {}
    }
}

// ---------- 3-way merge -----------------------------------------
//
// When two transactions concurrently modify a pmap, OCC's first
// instinct is to abort the loser and re-execute it against the live
// state. For a HAMT where each node is content-addressed, we can do
// better: walk both new roots against their common ancestor and
// produce a merged root whenever the two changes touch disjoint
// keys. The result is byte-identical to running the txs serially.
//
// Where this kicks in: `occ::commit_batch` calls `merge_three_way`
// when a tx's read of the pmap state cell mismatches the live root
// at validate time. If merge succeeds, we substitute the merged
// root (and any new interior node cells the merge synthesized) into
// the tx's write set instead of re-executing.

/// Output of a successful merge — the new root hash plus any
/// freshly synthesized interior node cells that need to be
/// committed to KV alongside.
pub struct MergeResult {
    pub root: u128,
    /// New node cells produced by the merge. Each `(cell_key, bytes)`
    /// pair is what the caller writes into KV. Cells that already
    /// exist in KV (content-addressed deduplication) are not
    /// re-emitted here.
    pub new_cells: Vec<(u128, Vec<u8>)>,
}

/// Three-way merge of two divergent pmap roots against their common
/// ancestor. Returns `None` if the merge would require resolving
/// conflicting writes to the same key — in that case, OCC falls
/// back to re-executing the losing transaction.
pub fn merge_three_way(
    new_a: u128,
    live_b: u128,
    ancestor: u128,
    kv: &dyn crate::kv::Kv,
    key_ty: &Type,
    val_ty: &Type,
) -> Option<MergeResult> {
    let mut new_cells: Vec<(u128, Vec<u8>)> = Vec::new();
    let root = merge_at(new_a, live_b, ancestor, kv, key_ty, val_ty, &mut new_cells)?;
    Some(MergeResult { root, new_cells })
}

fn read_node_kv(
    kv: &dyn crate::kv::Kv,
    hash: u128,
    key_ty: &Type,
    val_ty: &Type,
) -> Option<Node> {
    if hash == EMPTY { return None; }
    // Cells store node bytes wrapped as Value::Bytes; unwrap, then
    // run our node deserializer.
    let bytes = kv.get(hash)?;
    let v = crate::serialize::deserialize(&bytes, &Type::Bytes)?;
    let raw = match v {
        Value::Bytes(b) => b,
        _ => return None,
    };
    deserialize_node(&raw, key_ty, val_ty)
}

fn merge_at(
    a: u128,
    b: u128,
    base: u128,
    kv: &dyn crate::kv::Kv,
    key_ty: &Type,
    val_ty: &Type,
    out: &mut Vec<(u128, Vec<u8>)>,
) -> Option<u128> {
    if a == b { return Some(a); }
    if a == base { return Some(b); }
    if b == base { return Some(a); }

    let na = read_node_kv(kv, a, key_ty, val_ty)?;
    let nb = read_node_kv(kv, b, key_ty, val_ty)?;
    let nbase = read_node_kv(kv, base, key_ty, val_ty);

    match (na, nb) {
        (Node::Leaf { entries: ea }, Node::Leaf { entries: eb }) => {
            // 3-way merge of leaf entries. Treat absent base as an
            // empty leaf so newly-introduced keys land cleanly.
            let base_entries: Vec<(Value, Value)> = match nbase {
                Some(Node::Leaf { entries }) => entries,
                _ => Vec::new(),
            };
            // Index every entry by its serialized-key bytes so the
            // ordering is canonical and stable across versions.
            let to_map = |entries: &[(Value, Value)]| {
                let mut m: std::collections::BTreeMap<Vec<u8>, Value> =
                    std::collections::BTreeMap::new();
                for (k, v) in entries {
                    m.insert(crate::serialize::serialize(k), v.clone());
                }
                m
            };
            let map_a = to_map(&ea);
            let map_b = to_map(&eb);
            let map_base = to_map(&base_entries);
            // Iterate the union of keys.
            let all_keys: std::collections::BTreeSet<&Vec<u8>> = map_a.keys()
                .chain(map_b.keys())
                .chain(map_base.keys())
                .collect();
            let mut merged_entries: Vec<(Value, Value)> = Vec::new();
            for kb in all_keys {
                let va = map_a.get(kb);
                let vb = map_b.get(kb);
                let vbase = map_base.get(kb);
                let merged_value = match (va, vb, vbase) {
                    // Both arrived at the same view (including both
                    // absent — i.e., both deleted). No-op.
                    (a, b, _) if a == b => a.cloned(),
                    // a didn't change vs. base → take b's view.
                    (a, b, base) if a == base => b.cloned(),
                    // mirror.
                    (a, b, base) if b == base => a.cloned(),
                    // Both changed to different values → conflict.
                    _ => return None,
                };
                if let Some(v) = merged_value {
                    // Recover the deserialized key from any side.
                    let recovered_key = ea.iter().chain(eb.iter()).chain(base_entries.iter())
                        .find(|(k, _)| crate::serialize::serialize(k) == *kb)
                        .map(|(k, _)| k.clone());
                    let key = recovered_key?;
                    merged_entries.push((key, v));
                }
            }
            if merged_entries.is_empty() {
                return Some(EMPTY);
            }
            sort_entries(&mut merged_entries);
            let bytes = serialize_leaf(&merged_entries);
            let h = node_cell_key(&bytes);
            out.push((h, bytes));
            Some(h)
        }
        (Node::Inner { bitmap: ba, children: ca },
         Node::Inner { bitmap: bb, children: cb }) => {
            let (base_bitmap, base_children) = match &nbase {
                Some(Node::Inner { bitmap, children }) => (*bitmap, children.clone()),
                _ => (0u32, Vec::new()),
            };
            let mut merged_bitmap = 0u32;
            let mut merged_children: Vec<u128> = Vec::new();
            // Walk every slot that's set in any of a/b/base. We walk
            // base-included slots too because removals there matter
            // for the (kept, removed) merge case.
            let union_bitmap = ba | bb | base_bitmap;
            for slot in 0..32u32 {
                let bit = 1u32 << slot;
                if union_bitmap & bit == 0 { continue; }
                let in_a = ba & bit != 0;
                let in_b = bb & bit != 0;
                let in_base = base_bitmap & bit != 0;
                let child_a = if in_a {
                    ca[(ba & (bit - 1)).count_ones() as usize]
                } else { EMPTY };
                let child_b = if in_b {
                    cb[(bb & (bit - 1)).count_ones() as usize]
                } else { EMPTY };
                let child_base = if in_base {
                    base_children[(base_bitmap & (bit - 1)).count_ones() as usize]
                } else { EMPTY };
                let merged = merge_at(child_a, child_b, child_base, kv, key_ty, val_ty, out)?;
                if merged != EMPTY {
                    merged_children.push(merged);
                    merged_bitmap |= bit;
                }
            }
            if merged_bitmap == 0 {
                return Some(EMPTY);
            }
            let bytes = serialize_inner(merged_bitmap, &merged_children);
            let h = node_cell_key(&bytes);
            out.push((h, bytes));
            Some(h)
        }
        // Leaf vs. Inner: shape mismatch. This only happens if a
        // split or compaction at the same level was driven by both
        // sides — rare, and the safe call is to bail out and let
        // OCC re-execute one side.
        _ => None,
    }
}

// ---------- node format -----------------------------------------

fn serialize_inner(bitmap: u32, children: &[u128]) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + 4 + 1 + children.len() * 16);
    out.push(TAG_INNER);
    out.extend_from_slice(&bitmap.to_be_bytes());
    out.push(children.len() as u8);
    for h in children {
        out.extend_from_slice(&h.to_be_bytes());
    }
    out
}

fn serialize_leaf(entries: &[(Value, Value)]) -> Vec<u8> {
    // Canonicalize entry order by serialized-key bytes so two leaves
    // with the same (k, v) set produce identical bytes — the hash
    // collapse is what gives content-addressing its dedup property.
    let mut ordered: Vec<(Value, Value)> = entries.to_vec();
    sort_entries(&mut ordered);
    let mut out = Vec::with_capacity(1 + 4 + ordered.len() * 32);
    out.push(TAG_LEAF);
    out.extend_from_slice(&(ordered.len() as u32).to_be_bytes());
    for (k, v) in &ordered {
        let kb = crate::serialize::serialize(k);
        out.extend_from_slice(&(kb.len() as u32).to_be_bytes());
        out.extend_from_slice(&kb);
        let vb = crate::serialize::serialize(v);
        out.extend_from_slice(&(vb.len() as u32).to_be_bytes());
        out.extend_from_slice(&vb);
    }
    out
}

fn sort_entries(entries: &mut Vec<(Value, Value)>) {
    entries.sort_by(|a, b| {
        let ab = crate::serialize::serialize(&a.0);
        let bb = crate::serialize::serialize(&b.0);
        ab.cmp(&bb)
    });
}

fn deserialize_node(bytes: &[u8], key_ty: &Type, val_ty: &Type) -> Option<Node> {
    let tag = *bytes.first()?;
    let mut cursor = 1usize;
    match tag {
        TAG_INNER => {
            let bitmap = u32::from_be_bytes(bytes.get(cursor..cursor + 4)?.try_into().ok()?);
            cursor += 4;
            let n = *bytes.get(cursor)? as usize;
            cursor += 1;
            let mut children = Vec::with_capacity(n);
            for _ in 0..n {
                let h = u128::from_be_bytes(bytes.get(cursor..cursor + 16)?.try_into().ok()?);
                cursor += 16;
                children.push(h);
            }
            // Defensive cross-check: bitmap.count_ones() must match the
            // number of children we just decoded. Mismatch => corrupt
            // bytes; surface as None so the caller falls back cleanly.
            if bitmap.count_ones() as usize != n {
                return None;
            }
            Some(Node::Inner { bitmap, children })
        }
        TAG_LEAF => {
            let n = u32::from_be_bytes(bytes.get(cursor..cursor + 4)?.try_into().ok()?) as usize;
            cursor += 4;
            let mut entries = Vec::with_capacity(n);
            for _ in 0..n {
                let kl = u32::from_be_bytes(bytes.get(cursor..cursor + 4)?.try_into().ok()?) as usize;
                cursor += 4;
                let kb = bytes.get(cursor..cursor + kl)?;
                cursor += kl;
                let k = crate::serialize::deserialize(kb, key_ty)?;
                let vl = u32::from_be_bytes(bytes.get(cursor..cursor + 4)?.try_into().ok()?) as usize;
                cursor += 4;
                let vb = bytes.get(cursor..cursor + vl)?;
                cursor += vl;
                let v = crate::serialize::deserialize(vb, val_ty)?;
                entries.push((k, v));
            }
            Some(Node::Leaf { entries })
        }
        _ => None,
    }
}

// ---------- helpers ----------------------------------------------

fn bits_at(h: u64, level: u32) -> u32 {
    ((h >> (level * BITS_PER_LEVEL)) as u32) & (BRANCH - 1)
}

fn hash_key(key: &Value) -> u64 {
    let bytes = crate::serialize::serialize(key);
    let h128 = crate::hashing::child(0, &bytes);
    h128 as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::Type;
    use crate::host::Host;
    use crate::kv::EmptyKv;
    use crate::tx::Tx;
    use crate::value::Value;

    fn fresh_tx<'a>(host: &'a Host, kv: &'a EmptyKv) -> Tx<'a> {
        let _ = host;
        Tx::new(kv)
    }

    #[test]
    fn empty_tree_has_no_entries() {
        let host = Host::new();
        let kv = EmptyKv;
        let mut tx = fresh_tx(&host, &kv);
        assert!(get(EMPTY, &Value::Int(1), &mut tx, &Type::Int, &Type::Int).is_none());
        assert!(!contains(EMPTY, &Value::Int(1), &mut tx, &Type::Int, &Type::Int));
    }

    #[test]
    fn set_and_get_single() {
        let host = Host::new();
        let kv = EmptyKv;
        let mut tx = fresh_tx(&host, &kv);
        let h = set(EMPTY, Value::Int(1), Value::Int(100), &mut tx, &Type::Int, &Type::Int).unwrap();
        assert_ne!(h, EMPTY);
        assert!(contains(h, &Value::Int(1), &mut tx, &Type::Int, &Type::Int));
        assert_eq!(get(h, &Value::Int(1), &mut tx, &Type::Int, &Type::Int), Some(Value::Int(100)));
    }

    #[test]
    fn set_overwrites() {
        let host = Host::new();
        let kv = EmptyKv;
        let mut tx = fresh_tx(&host, &kv);
        let h = set(EMPTY, Value::Int(1), Value::Int(10), &mut tx, &Type::Int, &Type::Int).unwrap();
        let h = set(h, Value::Int(1), Value::Int(20), &mut tx, &Type::Int, &Type::Int).unwrap();
        assert_eq!(get(h, &Value::Int(1), &mut tx, &Type::Int, &Type::Int), Some(Value::Int(20)));
    }

    #[test]
    fn many_keys_and_get() {
        let host = Host::new();
        let kv = EmptyKv;
        let mut tx = fresh_tx(&host, &kv);
        let mut h = EMPTY;
        for i in 0..100i64 {
            h = set(h, Value::Int(i), Value::Int(i * 10), &mut tx, &Type::Int, &Type::Int).unwrap();
        }
        for i in 0..100i64 {
            assert_eq!(
                get(h, &Value::Int(i), &mut tx, &Type::Int, &Type::Int),
                Some(Value::Int(i * 10)),
            );
        }
        assert_eq!(get(h, &Value::Int(999), &mut tx, &Type::Int, &Type::Int), None);
    }

    #[test]
    fn old_root_unchanged_after_set_persistent_property() {
        let host = Host::new();
        let kv = EmptyKv;
        let mut tx = fresh_tx(&host, &kv);
        let h1 = set(EMPTY, Value::Int(1), Value::Int(10), &mut tx, &Type::Int, &Type::Int).unwrap();
        let h2 = set(h1, Value::Int(2), Value::Int(20), &mut tx, &Type::Int, &Type::Int).unwrap();
        // h1 is still readable — its nodes weren't rewritten because
        // each new operation writes new nodes at new content hashes.
        assert!(get(h1, &Value::Int(2), &mut tx, &Type::Int, &Type::Int).is_none());
        assert_eq!(get(h2, &Value::Int(2), &mut tx, &Type::Int, &Type::Int), Some(Value::Int(20)));
        assert_eq!(get(h1, &Value::Int(1), &mut tx, &Type::Int, &Type::Int), Some(Value::Int(10)));
    }

    #[test]
    fn remove_drops_key() {
        let host = Host::new();
        let kv = EmptyKv;
        let mut tx = fresh_tx(&host, &kv);
        let mut h = EMPTY;
        for i in 0..10i64 {
            h = set(h, Value::Int(i), Value::Int(i), &mut tx, &Type::Int, &Type::Int).unwrap();
        }
        let (h, found) = remove(h, &Value::Int(3), &mut tx, &Type::Int, &Type::Int).unwrap();
        assert!(found);
        assert!(!contains(h, &Value::Int(3), &mut tx, &Type::Int, &Type::Int));
        assert_eq!(get(h, &Value::Int(3), &mut tx, &Type::Int, &Type::Int), None);
        assert_eq!(get(h, &Value::Int(4), &mut tx, &Type::Int, &Type::Int), Some(Value::Int(4)));
    }

    #[test]
    fn remove_missing_returns_false() {
        let host = Host::new();
        let kv = EmptyKv;
        let mut tx = fresh_tx(&host, &kv);
        let h = set(EMPTY, Value::Int(1), Value::Int(1), &mut tx, &Type::Int, &Type::Int).unwrap();
        let (h2, found) = remove(h, &Value::Int(999), &mut tx, &Type::Int, &Type::Int).unwrap();
        assert!(!found);
        assert_eq!(h2, h);
    }

    /// Test scaffolding: apply N inserts on top of `starting` (which
    /// must be a root hash whose nodes already exist in `seed_kv`),
    /// returning the resulting root + a KV that's `seed_kv` plus the
    /// new path nodes the inserts wrote.
    fn build_on(
        seed_kv: &crate::kv::InMemoryKv,
        starting: u128,
        entries: &[(i64, i64)],
    ) -> (u128, crate::kv::InMemoryKv) {
        let host = Host::new();
        let mut tx = Tx::new(seed_kv);
        let mut h = starting;
        for (k, v) in entries {
            h = set(h, Value::Int(*k), Value::Int(*v), &mut tx, &Type::Int, &Type::Int).unwrap();
        }
        let _ = host;
        let (_reads, writes) = tx.into_sets();
        let mut out = seed_kv.clone();
        out.apply(&writes);
        (h, out)
    }

    /// Take the union of two KVs. Used in merge tests where two
    /// divergent worlds each have their own cell sets and we want
    /// the merge to see all of them.
    fn union_kv(a: &crate::kv::InMemoryKv, b: &crate::kv::InMemoryKv) -> crate::kv::InMemoryKv {
        let mut out = crate::kv::InMemoryKv::new();
        for (k, v) in &a.data { out.put_raw(*k, v.clone()); }
        for (k, v) in &b.data { out.put_raw(*k, v.clone()); }
        out
    }

    /// Apply a merge's `new_cells` into a KV using the same wrapping
    /// (`Value::Bytes`) the runtime uses, so subsequent reads can
    /// deserialize them as nodes.
    fn apply_merge_cells(kv: &mut crate::kv::InMemoryKv, merged: &MergeResult) {
        for (k, b) in &merged.new_cells {
            kv.put_raw(*k, crate::serialize::serialize(&Value::Bytes(b.clone())));
        }
    }

    #[test]
    fn merge_disjoint_keys_succeeds() {
        let empty_kv = crate::kv::InMemoryKv::new();
        let (root_base, kv_base) = build_on(&empty_kv, EMPTY, &[(1, 10), (2, 20), (3, 30)]);
        let (root_a, kv_a) = build_on(&kv_base, root_base, &[(99, 9900)]);
        let (root_b, kv_b) = build_on(&kv_base, root_base, &[(101, 10100)]);
        let mut kv = union_kv(&kv_a, &kv_b);

        let merged = merge_three_way(root_a, root_b, root_base, &kv, &Type::Int, &Type::Int)
            .expect("disjoint inserts should merge");
        apply_merge_cells(&mut kv, &merged);

        let kv_for_tx: &dyn crate::kv::Kv = &kv;
        let mut tx = Tx::new(kv_for_tx);
        for (k, expected) in &[(1, 10), (2, 20), (3, 30), (99, 9900), (101, 10100)] {
            assert_eq!(
                get(merged.root, &Value::Int(*k), &mut tx, &Type::Int, &Type::Int),
                Some(Value::Int(*expected)),
                "key {} missing from merged tree", k,
            );
        }
    }

    #[test]
    fn merge_overwrite_on_one_side_only_succeeds() {
        let empty_kv = crate::kv::InMemoryKv::new();
        let (root_base, kv_base) = build_on(&empty_kv, EMPTY, &[(1, 10), (2, 20)]);
        let (root_a, kv_a) = build_on(&kv_base, root_base, &[(1, 999)]);
        let (root_b, kv_b) = build_on(&kv_base, root_base, &[(3, 30)]);
        let mut kv = union_kv(&kv_a, &kv_b);

        let merged = merge_three_way(root_a, root_b, root_base, &kv, &Type::Int, &Type::Int)
            .expect("disjoint changes should merge");
        apply_merge_cells(&mut kv, &merged);

        let kv_for_tx: &dyn crate::kv::Kv = &kv;
        let mut tx = Tx::new(kv_for_tx);
        assert_eq!(get(merged.root, &Value::Int(1), &mut tx, &Type::Int, &Type::Int), Some(Value::Int(999)));
        assert_eq!(get(merged.root, &Value::Int(2), &mut tx, &Type::Int, &Type::Int), Some(Value::Int(20)));
        assert_eq!(get(merged.root, &Value::Int(3), &mut tx, &Type::Int, &Type::Int), Some(Value::Int(30)));
    }

    #[test]
    fn merge_conflicting_overwrites_returns_none() {
        let empty_kv = crate::kv::InMemoryKv::new();
        let (root_base, kv_base) = build_on(&empty_kv, EMPTY, &[(1, 10)]);
        let (root_a, _) = build_on(&kv_base, root_base, &[(1, 100)]);
        let (root_b, _) = build_on(&kv_base, root_base, &[(1, 200)]);
        let merged = merge_three_way(root_a, root_b, root_base, &kv_base, &Type::Int, &Type::Int);
        assert!(merged.is_none(), "conflicting overwrites must surface as None");
    }

    #[test]
    fn merge_against_empty_ancestor() {
        let empty_kv = crate::kv::InMemoryKv::new();
        let (root_a, kv_a) = build_on(&empty_kv, EMPTY, &[(1, 10), (2, 20)]);
        let (root_b, kv_b) = build_on(&empty_kv, EMPTY, &[(3, 30), (4, 40)]);
        let mut kv = union_kv(&kv_a, &kv_b);
        let merged = merge_three_way(root_a, root_b, EMPTY, &kv, &Type::Int, &Type::Int)
            .expect("inserts into empty ancestor with disjoint keys should merge");
        apply_merge_cells(&mut kv, &merged);
        let kv_for_tx: &dyn crate::kv::Kv = &kv;
        let mut tx = Tx::new(kv_for_tx);
        for (k, expected) in &[(1, 10), (2, 20), (3, 30), (4, 40)] {
            assert_eq!(
                get(merged.root, &Value::Int(*k), &mut tx, &Type::Int, &Type::Int),
                Some(Value::Int(*expected)),
            );
        }
    }

    #[test]
    fn structurally_equal_subtrees_share_cells() {
        // Two pmaps built independently with identical content
        // produce identical root hashes (and identical interior
        // cells along the way). That's the dedup property.
        let host = Host::new();
        let kv = EmptyKv;
        let mut tx = fresh_tx(&host, &kv);
        let mut a = EMPTY;
        let mut b = EMPTY;
        for i in 0..20i64 {
            a = set(a, Value::Int(i), Value::Int(i + 1000), &mut tx, &Type::Int, &Type::Int).unwrap();
            b = set(b, Value::Int(i), Value::Int(i + 1000), &mut tx, &Type::Int, &Type::Int).unwrap();
        }
        assert_eq!(a, b, "equal-content pmaps should hash to the same root");
    }
}
