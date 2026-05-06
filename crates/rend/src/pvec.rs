//! Persistent vector — indexed-trie storage. Each tree node lives
//! in its own content-addressed KV cell. The state cell carries
//! (length, root_hash); operations on the underlying trie are
//! O(log32 N) cell accesses.
//!
//! ### Tree shape
//!
//! 32-way branching, 5 bits per level. Indices fill left-to-right:
//! a leaf may hold fewer than 32 elements only if it's the rightmost
//! leaf at its depth.
//!
//! Depth is determined by the current length:
//!   * `len <= 32` → depth 0 (single leaf, possibly partial).
//!   * `len <= 32^2 = 1024` → depth 1.
//!   * `len <= 32^3 = 32_768` → depth 2.
//!   * etc. Bumping past a 32^(d+1) boundary grows the tree by a
//!     level (wrap the old root in a new single-child inner).
//!
//! ### Wire format (per cell)
//!
//! Inner: `0x01 | n_children[u8] | child_hash_1[u128_be] | ... | child_hash_n[u128_be]`
//!
//! Leaf:  `0x02 | n_elements[u8] |
//!         (for each element: len[u32_be] | element_bytes)`
//!
//! No bitmap on inner nodes because vec slots fill densely; the
//! count alone tells you how many children are present and which
//! slots they occupy (always 0..n).

use crate::ast::Type;
use crate::error::{Error, ErrorKind};
use crate::token::Span;
use crate::tx::Tx;
use crate::value::Value;

const BRANCH: u32 = 32;
const BITS_PER_LEVEL: u32 = 5;

const TAG_INNER: u8 = 0x01;
const TAG_LEAF:  u8 = 0x02;

/// Sentinel root hash for an empty vector.
pub const EMPTY: u128 = 0;

/// Magic constant namespacing pvec node-cell keys away from pmap's
/// and from state-root cells. Values just need to be distinct from
/// every other key-derivation site in the runtime.
const NODE_NS: u128 = 0x5056_4543_4E4F_4445_5056_4543_4E4F_4445;

/// Internal: one node, decoded from KV bytes for the duration of
/// a single op step.
enum Node {
    Inner { children: Vec<u128> },
    Leaf { elements: Vec<Value> },
}

/// Number of trie levels above the leaf for a given length.
/// `len 0..=32` → 0 (leaves only). Each additional 5 bits of index
/// width adds a level.
fn depth_for(len: u64) -> u32 {
    if len <= BRANCH as u64 { return 0; }
    let high = len - 1;
    let bits = 64 - high.leading_zeros();
    bits.div_ceil(BITS_PER_LEVEL) - 1
}

/// Slot at the given level for an index. Level 0 picks the bottom
/// 5 bits (a leaf slot); higher levels pick higher 5-bit chunks.
fn slot_at(i: u64, level: u32) -> usize {
    ((i >> (level * BITS_PER_LEVEL)) & (BRANCH as u64 - 1)) as usize
}

fn node_cell_key(bytes: &[u8]) -> u128 {
    crate::hashing::child(NODE_NS, bytes)
}

fn read_node(tx: &mut Tx, hash: u128, elem_ty: &Type) -> Option<Node> {
    if hash == EMPTY { return None; }
    let v = tx.read_cell(hash, &Type::Bytes);
    let v = tx.force(v);
    let bytes = match v {
        Value::Bytes(b) => b,
        _ => return None,
    };
    deserialize_node(&bytes, elem_ty)
}

fn write_node(tx: &mut Tx, bytes: Vec<u8>) -> u128 {
    let hash = node_cell_key(&bytes);
    tx.write(hash, Value::Bytes(bytes));
    tx.record_node_cell_write(hash);
    hash
}

// ---------- public API -------------------------------------------

/// Materialize the entire pvec into a `Vec<Value>` in index order.
/// Used by the `pvec_to_array` builtin. O(N) reads — one per element
/// (modulo trie node sharing). Callers that only need a subset should
/// keep using indexed access.
pub fn to_vec(
    root: u128,
    len: u64,
    tx: &mut Tx,
    elem_ty: &Type,
) -> Result<Vec<Value>, Error> {
    if len == 0 {
        return Ok(Vec::new());
    }
    let level = depth_for(len);
    let mut out = Vec::with_capacity(len as usize);
    collect_elements(root, level, len, &mut out, tx, elem_ty)?;
    Ok(out)
}

fn collect_elements(
    node_hash: u128,
    level: u32,
    remaining: u64,
    out: &mut Vec<Value>,
    tx: &mut Tx,
    elem_ty: &Type,
) -> Result<(), Error> {
    let Some(node) = read_node(tx, node_hash, elem_ty) else {
        return Err(Error::new(
            ErrorKind::Runtime,
            "pvec internal: missing node during full-walk".to_string(),
            Span::default(),
        ));
    };
    match node {
        Node::Leaf { elements } => {
            // The last leaf may be partially full; clamp to `remaining`
            // so we don't emit padding zeros from the final block.
            let take = std::cmp::min(elements.len(), remaining as usize);
            out.extend(elements.into_iter().take(take));
        }
        Node::Inner { children } => {
            // Each child subtree holds 32^level elements (full) — except
            // the last child, which may be partial. Walk left-to-right,
            // tracking how much of `remaining` has been emitted.
            let subtree_capacity: u64 = 1u64 << (level * BITS_PER_LEVEL);
            let mut emitted: u64 = 0;
            for c in children {
                if emitted >= remaining { break; }
                let already_done = out.len() as u64;
                let want = std::cmp::min(subtree_capacity, remaining - emitted);
                collect_elements(c, level - 1, want, out, tx, elem_ty)?;
                emitted += out.len() as u64 - already_done;
            }
        }
    }
    Ok(())
}

pub fn get(
    root: u128,
    len: u64,
    i: u64,
    tx: &mut Tx,
    elem_ty: &Type,
    span: Span,
) -> Result<Value, Error> {
    if i >= len {
        return Err(Error::new(
            ErrorKind::Runtime,
            format!("pvec index out of bounds: {i} of length {len}"),
            span,
        ));
    }
    let level = depth_for(len);
    get_at(root, level, i, tx, elem_ty)
        .ok_or_else(|| Error::new(
            ErrorKind::Runtime,
            "pvec internal: missing node along read path".to_string(),
            span,
        ))
}

fn get_at(
    node_hash: u128,
    level: u32,
    i: u64,
    tx: &mut Tx,
    elem_ty: &Type,
) -> Option<Value> {
    let node = read_node(tx, node_hash, elem_ty)?;
    match node {
        Node::Leaf { elements } => {
            elements.get(slot_at(i, 0)).cloned()
        }
        Node::Inner { children } => {
            let slot = slot_at(i, level);
            let child = *children.get(slot)?;
            get_at(child, level - 1, i, tx, elem_ty)
        }
    }
}

pub fn set(
    root: u128,
    len: u64,
    i: u64,
    value: Value,
    tx: &mut Tx,
    elem_ty: &Type,
    span: Span,
) -> Result<u128, Error> {
    if i >= len {
        return Err(Error::new(
            ErrorKind::Runtime,
            format!("pvec index out of bounds: {i} of length {len}"),
            span,
        ));
    }
    let level = depth_for(len);
    set_at(root, level, i, value, tx, elem_ty)
        .ok_or_else(|| Error::new(
            ErrorKind::Runtime,
            "pvec internal: missing node along write path".to_string(),
            span,
        ))
}

fn set_at(
    node_hash: u128,
    level: u32,
    i: u64,
    value: Value,
    tx: &mut Tx,
    elem_ty: &Type,
) -> Option<u128> {
    let node = read_node(tx, node_hash, elem_ty)?;
    match node {
        Node::Leaf { mut elements } => {
            let slot = slot_at(i, 0);
            *elements.get_mut(slot)? = value;
            let bytes = serialize_leaf(&elements);
            Some(write_node(tx, bytes))
        }
        Node::Inner { mut children } => {
            let slot = slot_at(i, level);
            let child = *children.get(slot)?;
            let new_child = set_at(child, level - 1, i, value, tx, elem_ty)?;
            children[slot] = new_child;
            let bytes = serialize_inner(&children);
            Some(write_node(tx, bytes))
        }
    }
}

/// Append. Returns `(new_root, new_len)`. The new index assigned
/// to `value` is `len` (the old length).
pub fn push(
    root: u128,
    len: u64,
    value: Value,
    tx: &mut Tx,
    elem_ty: &Type,
) -> Result<(u128, u64), Error> {
    let new_len = len.checked_add(1).ok_or_else(|| Error::new(
        ErrorKind::Runtime,
        "pvec length overflow",
        Span::default(),
    ))?;
    let new_index = len;
    let old_depth = depth_for(len);
    let new_depth = depth_for(new_len);

    // If the tree's depth grew, wrap the old root in successive
    // single-child inners until we're at the new depth.
    let mut working_root = root;
    let mut working_level = old_depth;
    while working_level < new_depth {
        let bytes = serialize_inner(&[working_root]);
        working_root = write_node(tx, bytes);
        working_level += 1;
    }

    let new_root = push_at(working_root, new_depth, new_index, value, tx, elem_ty)?;
    Ok((new_root, new_len))
}

fn push_at(
    node_hash: u128,
    level: u32,
    i: u64,
    value: Value,
    tx: &mut Tx,
    elem_ty: &Type,
) -> Result<u128, Error> {
    if level == 0 {
        // Leaf — append.
        let mut elements = match read_node(tx, node_hash, elem_ty) {
            Some(Node::Leaf { elements }) => elements,
            _ => Vec::new(),
        };
        elements.push(value);
        let bytes = serialize_leaf(&elements);
        return Ok(write_node(tx, bytes));
    }
    let mut children = match read_node(tx, node_hash, elem_ty) {
        Some(Node::Inner { children }) => children,
        _ => Vec::new(),
    };
    let slot = slot_at(i, level);
    if slot >= children.len() {
        // First entry into this slot. Recursive build down to a leaf.
        // (`slot` always equals `children.len()` for a push: the
        // pre-existing children fill 0..=slot-1, and we're starting
        // a fresh subtree at slot.)
        let new_child = push_at(EMPTY, level - 1, i, value, tx, elem_ty)?;
        children.push(new_child);
    } else {
        let child = children[slot];
        let new_child = push_at(child, level - 1, i, value, tx, elem_ty)?;
        children[slot] = new_child;
    }
    let bytes = serialize_inner(&children);
    Ok(write_node(tx, bytes))
}

// ---------- reachability ----------------------------------------

pub fn reachable_cells(
    root: u128,
    kv: &dyn crate::kv::Kv,
    elem_ty: &Type,
) -> std::collections::HashSet<u128> {
    let mut out = std::collections::HashSet::new();
    walk_reachable(root, kv, elem_ty, &mut out);
    out
}

fn walk_reachable(
    hash: u128,
    kv: &dyn crate::kv::Kv,
    elem_ty: &Type,
    out: &mut std::collections::HashSet<u128>,
) {
    if hash == EMPTY { return; }
    if !out.insert(hash) { return; }
    match read_node_kv(kv, hash, elem_ty) {
        Some(Node::Inner { children, .. }) => {
            for c in children { walk_reachable(c, kv, elem_ty, out); }
        }
        Some(Node::Leaf { .. }) | None => {}
    }
}

// ---------- 3-way merge -----------------------------------------
//
// Like pmap, two transactions writing to disjoint indices in the
// same pvec produce content-divergent roots that share most of
// their interior nodes with the ancestor. The OCC validator runs
// a 3-way merge on the root hash to salvage the conflict.
//
// Limitation: this slice only merges when all three lengths are
// equal — i.e. only indexed-set changes, no push on either side.
// Push-vs-push is a genuine conflict (both want the next slot);
// push-vs-set could merge by replaying the set into the pushed
// tree, but the bookkeeping isn't worth it for a first slice and
// is filed as future work.

pub struct MergeResult {
    pub root: u128,
    pub new_cells: Vec<(u128, Vec<u8>)>,
}

pub fn merge_three_way(
    new_a: u128,
    len_a: u64,
    live_b: u128,
    len_b: u64,
    ancestor: u128,
    len_ancestor: u64,
    kv: &dyn crate::kv::Kv,
    elem_ty: &Type,
) -> Option<MergeResult> {
    if len_a != len_ancestor || len_b != len_ancestor {
        // Length-changing op (push) on at least one side. Out of
        // scope for the 3-way merge — caller falls back to re-exec.
        return None;
    }
    let mut new_cells: Vec<(u128, Vec<u8>)> = Vec::new();
    let depth = depth_for(len_a);
    let root = merge_at(new_a, live_b, ancestor, depth, kv, elem_ty, &mut new_cells)?;
    Some(MergeResult { root, new_cells })
}

fn read_node_kv(kv: &dyn crate::kv::Kv, hash: u128, elem_ty: &Type) -> Option<Node> {
    if hash == EMPTY { return None; }
    let bytes = kv.get(hash)?;
    let v = crate::serialize::deserialize(&bytes, &Type::Bytes)?;
    let raw = match v {
        Value::Bytes(b) => b,
        _ => return None,
    };
    deserialize_node(&raw, elem_ty)
}

fn merge_at(
    a: u128,
    b: u128,
    base: u128,
    level: u32,
    kv: &dyn crate::kv::Kv,
    elem_ty: &Type,
    out: &mut Vec<(u128, Vec<u8>)>,
) -> Option<u128> {
    if a == b { return Some(a); }
    if a == base { return Some(b); }
    if b == base { return Some(a); }

    let na = read_node_kv(kv, a, elem_ty)?;
    let nb = read_node_kv(kv, b, elem_ty)?;
    let nbase = read_node_kv(kv, base, elem_ty);

    match (na, nb) {
        (Node::Leaf { elements: ea }, Node::Leaf { elements: eb }) => {
            // Same length → same slot count (leaves are dense and
            // determined by length). Per-element 3-way reconcile.
            let base_elems: Vec<Value> = match nbase {
                Some(Node::Leaf { elements }) => elements,
                _ => Vec::new(),
            };
            if ea.len() != eb.len() {
                // Shape mismatch within a same-length tree shouldn't
                // happen, but be defensive.
                return None;
            }
            let mut merged = Vec::with_capacity(ea.len());
            for i in 0..ea.len() {
                let va = &ea[i];
                let vb = &eb[i];
                let vbase = base_elems.get(i);
                let v = if va == vb {
                    va.clone()
                } else if Some(va) == vbase {
                    vb.clone()
                } else if Some(vb) == vbase {
                    va.clone()
                } else {
                    // Both sides changed the same index to different
                    // values → genuine conflict.
                    return None;
                };
                merged.push(v);
            }
            let bytes = serialize_leaf(&merged);
            let h = node_cell_key(&bytes);
            out.push((h, bytes));
            Some(h)
        }
        (Node::Inner { children: ca }, Node::Inner { children: cb }) => {
            // Same length → same child counts at this level.
            let base_children: Vec<u128> = match nbase {
                Some(Node::Inner { children }) => children,
                _ => Vec::new(),
            };
            if ca.len() != cb.len() {
                return None;
            }
            let mut merged = Vec::with_capacity(ca.len());
            for i in 0..ca.len() {
                let child_a = ca[i];
                let child_b = cb[i];
                let child_base = base_children.get(i).copied().unwrap_or(EMPTY);
                let m = merge_at(child_a, child_b, child_base, level - 1, kv, elem_ty, out)?;
                merged.push(m);
            }
            let bytes = serialize_inner(&merged);
            let h = node_cell_key(&bytes);
            out.push((h, bytes));
            Some(h)
        }
        // Inner vs Leaf — shouldn't happen for same-length trees.
        _ => None,
    }
}

// ---------- node format -----------------------------------------

fn serialize_inner(children: &[u128]) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + children.len() * 16);
    out.push(TAG_INNER);
    out.push(children.len() as u8);
    for h in children {
        out.extend_from_slice(&h.to_be_bytes());
    }
    out
}

fn serialize_leaf(elements: &[Value]) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + elements.len() * 32);
    out.push(TAG_LEAF);
    out.push(elements.len() as u8);
    for v in elements {
        let vb = crate::serialize::serialize(v);
        out.extend_from_slice(&(vb.len() as u32).to_be_bytes());
        out.extend_from_slice(&vb);
    }
    out
}

fn deserialize_node(bytes: &[u8], elem_ty: &Type) -> Option<Node> {
    let tag = *bytes.first()?;
    let mut cursor = 1usize;
    match tag {
        TAG_INNER => {
            let n = *bytes.get(cursor)? as usize;
            cursor += 1;
            let mut children = Vec::with_capacity(n);
            for _ in 0..n {
                let h = u128::from_be_bytes(bytes.get(cursor..cursor + 16)?.try_into().ok()?);
                cursor += 16;
                children.push(h);
            }
            Some(Node::Inner { children })
        }
        TAG_LEAF => {
            let n = *bytes.get(cursor)? as usize;
            cursor += 1;
            let mut elements = Vec::with_capacity(n);
            for _ in 0..n {
                let vl = u32::from_be_bytes(bytes.get(cursor..cursor + 4)?.try_into().ok()?) as usize;
                cursor += 4;
                let vb = bytes.get(cursor..cursor + vl)?;
                cursor += vl;
                let v = crate::serialize::deserialize(vb, elem_ty)?;
                elements.push(v);
            }
            Some(Node::Leaf { elements })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::Type;
    use crate::host::Host;
    use crate::kv::EmptyKv;
    use crate::value::Value;

    fn fresh_tx<'a>(host: &'a Host, kv: &'a EmptyKv) -> Tx<'a> {
        let _ = host;
        Tx::new(kv)
    }

    #[test]
    fn push_and_get_within_first_leaf() {
        let host = Host::new();
        let kv = EmptyKv;
        let mut tx = fresh_tx(&host, &kv);
        let mut root = EMPTY;
        let mut len = 0u64;
        for i in 0..10i64 {
            let (r, l) = push(root, len, Value::Int(i * 10), &mut tx, &Type::Int).unwrap();
            root = r;
            len = l;
        }
        assert_eq!(len, 10);
        for i in 0..10u64 {
            let v = get(root, len, i, &mut tx, &Type::Int, Span::default()).unwrap();
            assert_eq!(v, Value::Int(i as i64 * 10));
        }
    }

    #[test]
    fn push_grows_the_tree_past_one_leaf() {
        let host = Host::new();
        let kv = EmptyKv;
        let mut tx = fresh_tx(&host, &kv);
        let mut root = EMPTY;
        let mut len = 0u64;
        for i in 0..100i64 {
            let (r, l) = push(root, len, Value::Int(i), &mut tx, &Type::Int).unwrap();
            root = r;
            len = l;
        }
        assert_eq!(len, 100);
        for i in 0..100u64 {
            let v = get(root, len, i, &mut tx, &Type::Int, Span::default()).unwrap();
            assert_eq!(v, Value::Int(i as i64));
        }
    }

    #[test]
    fn push_grows_to_depth_two() {
        let host = Host::new();
        let kv = EmptyKv;
        let mut tx = fresh_tx(&host, &kv);
        let mut root = EMPTY;
        let mut len = 0u64;
        // 1500 entries forces depth 2 (32^2 = 1024 < 1500 <= 32^3).
        for i in 0..1500i64 {
            let (r, l) = push(root, len, Value::Int(i), &mut tx, &Type::Int).unwrap();
            root = r;
            len = l;
        }
        assert_eq!(len, 1500);
        // Spot-check: first, mid, last.
        for i in &[0u64, 500, 1023, 1024, 1499] {
            let v = get(root, len, *i, &mut tx, &Type::Int, Span::default()).unwrap();
            assert_eq!(v, Value::Int(*i as i64));
        }
    }

    #[test]
    fn set_overwrites_existing_index() {
        let host = Host::new();
        let kv = EmptyKv;
        let mut tx = fresh_tx(&host, &kv);
        let mut root = EMPTY;
        let mut len = 0u64;
        for i in 0..50i64 {
            let (r, l) = push(root, len, Value::Int(i), &mut tx, &Type::Int).unwrap();
            root = r;
            len = l;
        }
        let new_root = set(root, len, 25, Value::Int(9999), &mut tx, &Type::Int, Span::default()).unwrap();
        assert_eq!(get(new_root, len, 25, &mut tx, &Type::Int, Span::default()).unwrap(), Value::Int(9999));
        assert_eq!(get(new_root, len, 24, &mut tx, &Type::Int, Span::default()).unwrap(), Value::Int(24));
        assert_eq!(get(new_root, len, 26, &mut tx, &Type::Int, Span::default()).unwrap(), Value::Int(26));
    }

    #[test]
    fn get_out_of_bounds_returns_runtime_error() {
        let host = Host::new();
        let kv = EmptyKv;
        let mut tx = fresh_tx(&host, &kv);
        let (root, len) = push(EMPTY, 0, Value::Int(7), &mut tx, &Type::Int).unwrap();
        let err = get(root, len, 5, &mut tx, &Type::Int, Span::default()).unwrap_err();
        assert!(err.to_string().contains("out of bounds"));
    }

    #[test]
    fn set_out_of_bounds_is_runtime_error() {
        let host = Host::new();
        let kv = EmptyKv;
        let mut tx = fresh_tx(&host, &kv);
        let (root, len) = push(EMPTY, 0, Value::Int(7), &mut tx, &Type::Int).unwrap();
        let err = set(root, len, 5, Value::Int(0), &mut tx, &Type::Int, Span::default()).unwrap_err();
        assert!(err.to_string().contains("out of bounds"));
    }

    #[test]
    fn old_root_unchanged_after_push_persistent_property() {
        let host = Host::new();
        let kv = EmptyKv;
        let mut tx = fresh_tx(&host, &kv);
        let (r1, l1) = push(EMPTY, 0, Value::Int(10), &mut tx, &Type::Int).unwrap();
        let (r2, l2) = push(r1, l1, Value::Int(20), &mut tx, &Type::Int).unwrap();
        // r1 still observes only its own state.
        assert_eq!(get(r1, l1, 0, &mut tx, &Type::Int, Span::default()).unwrap(), Value::Int(10));
        let oob = get(r1, l1, 1, &mut tx, &Type::Int, Span::default());
        assert!(oob.is_err(), "r1 should not see r2's appended element");
        assert_eq!(get(r2, l2, 0, &mut tx, &Type::Int, Span::default()).unwrap(), Value::Int(10));
        assert_eq!(get(r2, l2, 1, &mut tx, &Type::Int, Span::default()).unwrap(), Value::Int(20));
    }
}
