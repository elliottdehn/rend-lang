//! Persistent sorted map (B+-tree). Same KV-cell-per-node story as
//! `pmap`, but the structure is key-ordered: leaves carry sorted
//! `(k, v)` entries, inner nodes carry routing keys plus child
//! pointers. Range queries descend to the lower bound and walk
//! forward through siblings until the upper bound — visiting only
//! cells covering the requested span. Unlike a radix trie, this
//! works regardless of how the keys are distributed in the bit
//! space; dense monotonic IDs (`1, 2, 3, ...`) are first-class
//! and not pathological.
//!
//! Slice-1 limit: only `u64` keys are wired through. Other widths
//! and string keys extend the same machinery in follow-ups.
//!
//! ### Wire format (per cell)
//!
//! Inner: `0x01 | n_keys[u32_be] |
//!         (key_len[u32_be] | key_bytes)*n_keys |
//!         child_hash[u128_be]*(n_keys+1)`
//!
//! Leaf:  `0x02 | n_entries[u32_be] |
//!         (key_len[u32_be] | key_bytes |
//!          val_len[u32_be] | val_bytes)*n_entries`
//!
//! Cells are content-addressed in a namespace distinct from `pmap`'s,
//! so a stray pmap-encoded cell can't be misread as pbtree.

use crate::ast::Type;
use crate::error::{Error, ErrorKind};
use crate::token::Span;
use crate::tx::Tx;
use crate::value::Value;

/// Branching factor: max keys per inner, max entries per leaf.
/// On split, halves are roughly B/2 each. With B=32 and ~32-byte
/// entries, leaf cells are ~1KB — typical for content-addressed
/// stores.
const B: usize = 32;

const TAG_INNER: u8 = 0x01;
const TAG_LEAF:  u8 = 0x02;

/// Sentinel root hash for an empty tree.
pub const EMPTY: u128 = 0;

/// In-memory representation of a fetched B+-tree node.
pub enum Node {
    Inner {
        /// `keys[i]` is the smallest key of `children[i+1]`'s subtree.
        /// Equivalently: every key in `children[i]` is `< keys[i]`,
        /// every key in `children[i+1]` is `>= keys[i]`.
        keys: Vec<Value>,
        /// `keys.len() + 1` entries.
        children: Vec<u128>,
    },
    Leaf {
        /// Sorted by key.
        entries: Vec<(Value, Value)>,
    },
}

fn node_cell_key(bytes: &[u8]) -> u128 {
    crate::hashing::child(NODE_NS, bytes)
}

const NODE_NS: u128 = 0x5042_5452_4545_4E4F_4445_5042_5452_4545;

pub fn read_node(tx: &mut Tx, hash: u128, key_ty: &Type, val_ty: &Type) -> Option<Node> {
    if hash == EMPTY { return None; }
    let v = tx.read_cell(hash, &Type::Bytes);
    let v = tx.force(v);
    let bytes = match v {
        Value::Bytes(b) => b,
        _ => return None,
    };
    deserialize_node(&bytes, key_ty, val_ty)
}

fn write_node(tx: &mut Tx, bytes: Vec<u8>) -> u128 {
    let hash = node_cell_key(&bytes);
    tx.write(hash, Value::Bytes(bytes));
    tx.record_node_cell_write(hash);
    hash
}

// ---------- comparison driver ----------

pub fn compare_keys(a: &Value, b: &Value) -> std::cmp::Ordering {
    match (a, b) {
        (Value::U64(x), Value::U64(y)) => x.cmp(y),
        // Other key types extend here as the encoding work expands.
        _ => std::cmp::Ordering::Equal,
    }
}

/// Index of the child to descend into when looking for `key`.
/// Returns the position in `children` (i.e. `0..=keys.len()`).
fn descend_pos(keys: &[Value], key: &Value) -> usize {
    // First index where keys[i] > key. That child holds `key`'s
    // potential location: children[i] has keys < keys[i], so key
    // belongs there if key < keys[i]. For an exact match
    // keys[i] == key, descend to children[i+1] (which holds keys
    // >= keys[i] = key).
    keys.partition_point(|k| compare_keys(k, key).is_le())
}

// ---------- ops ----------

pub fn get(
    root: u128,
    key: &Value,
    tx: &mut Tx,
    key_ty: &Type,
    val_ty: &Type,
) -> Option<Value> {
    if root == EMPTY { return None; }
    get_at(root, key, tx, key_ty, val_ty)
}

fn get_at(
    node_hash: u128,
    key: &Value,
    tx: &mut Tx,
    key_ty: &Type,
    val_ty: &Type,
) -> Option<Value> {
    let node = read_node(tx, node_hash, key_ty, val_ty)?;
    match node {
        Node::Leaf { entries } => entries
            .into_iter()
            .find(|(k, _)| compare_keys(k, key).is_eq())
            .map(|(_, v)| v),
        Node::Inner { keys, children } => {
            let pos = descend_pos(&keys, key);
            get_at(children[pos], key, tx, key_ty, val_ty)
        }
    }
}

pub fn contains(
    root: u128,
    key: &Value,
    tx: &mut Tx,
    key_ty: &Type,
    val_ty: &Type,
) -> bool {
    get(root, key, tx, key_ty, val_ty).is_some()
}

/// Result of a recursive `set` call: either no split happened
/// (we have a single new root for this subtree), or the node split
/// (we have two new subtrees plus the separator key the parent
/// needs to insert between them).
enum SetResult {
    Plain(u128),
    Split { left: u128, sep: Value, right: u128 },
}

pub fn set(
    root: u128,
    key: Value,
    value: Value,
    tx: &mut Tx,
    key_ty: &Type,
    val_ty: &Type,
) -> Result<u128, Error> {
    if root == EMPTY {
        // First-ever insert: write a single-leaf root.
        let bytes = serialize_leaf(&[(key, value)]);
        return Ok(write_node(tx, bytes));
    }
    let result = set_inner(root, key, value, tx, key_ty, val_ty)?;
    Ok(match result {
        SetResult::Plain(h) => h,
        SetResult::Split { left, sep, right } => {
            // Root split — wrap the two halves under a fresh root.
            let bytes = serialize_inner(&[sep], &[left, right]);
            write_node(tx, bytes)
        }
    })
}

fn set_inner(
    node_hash: u128,
    key: Value,
    value: Value,
    tx: &mut Tx,
    key_ty: &Type,
    val_ty: &Type,
) -> Result<SetResult, Error> {
    let node = read_node(tx, node_hash, key_ty, val_ty).ok_or_else(|| Error::new(
        ErrorKind::Runtime,
        format!("pbtree: cell {node_hash:x} missing during set"),
        Span::default(),
    ))?;
    match node {
        Node::Leaf { mut entries } => {
            // Insert sorted, replacing existing key if present.
            match entries.binary_search_by(|(k, _)| compare_keys(k, &key)) {
                Ok(pos) => entries[pos] = (key, value),
                Err(pos) => entries.insert(pos, (key, value)),
            }
            if entries.len() <= B {
                let bytes = serialize_leaf(&entries);
                return Ok(SetResult::Plain(write_node(tx, bytes)));
            }
            // Split: leftmost ceil(B/2)+1 stay; remainder go right.
            // The smallest key of the right half is the separator.
            let mid = entries.len() / 2;
            let right_entries: Vec<_> = entries.split_off(mid);
            let sep = right_entries[0].0.clone();
            let left = write_node(tx, serialize_leaf(&entries));
            let right = write_node(tx, serialize_leaf(&right_entries));
            Ok(SetResult::Split { left, sep, right })
        }
        Node::Inner { keys, children } => {
            let pos = descend_pos(&keys, &key);
            let result = set_inner(children[pos], key, value, tx, key_ty, val_ty)?;
            match result {
                SetResult::Plain(new_child) => {
                    let mut new_children = children;
                    new_children[pos] = new_child;
                    let bytes = serialize_inner(&keys, &new_children);
                    Ok(SetResult::Plain(write_node(tx, bytes)))
                }
                SetResult::Split { left, sep, right } => {
                    // Replace children[pos] with `left`, insert
                    // `sep` at keys[pos], insert `right` at
                    // children[pos+1].
                    let mut new_keys = keys;
                    let mut new_children = children;
                    new_children[pos] = left;
                    new_keys.insert(pos, sep);
                    new_children.insert(pos + 1, right);
                    if new_keys.len() <= B {
                        let bytes = serialize_inner(&new_keys, &new_children);
                        return Ok(SetResult::Plain(write_node(tx, bytes)));
                    }
                    // Inner split: middle key gets pushed up.
                    let mid = new_keys.len() / 2;
                    let split_key = new_keys[mid].clone();
                    let right_keys: Vec<_> = new_keys.split_off(mid + 1);
                    new_keys.pop(); // remove split_key from left
                    let right_children: Vec<_> = new_children.split_off(mid + 1);
                    let left_hash = write_node(tx, serialize_inner(&new_keys, &new_children));
                    let right_hash = write_node(tx, serialize_inner(&right_keys, &right_children));
                    Ok(SetResult::Split { left: left_hash, sep: split_key, right: right_hash })
                }
            }
        }
    }
}

/// Result of a recursive `remove` call.
enum RemoveResult {
    /// Key wasn't present; no structural change. Original hash returned.
    Unchanged,
    /// Subtree updated; here's the new node hash.
    Plain(u128),
    /// The subtree disappeared entirely (last entry removed). Parent
    /// must drop its child pointer + adjacent routing key.
    Gone,
}

/// Remove `key` from the tree. Returns the new root hash. If the
/// key wasn't present the root is unchanged. If the last entry is
/// removed, returns `EMPTY`.
///
/// **Slice-1 limit:** no rebalance (borrow / merge). Leaves can
/// become under-full after long delete sequences; the structure
/// stays correct but height may grow lopsided. Production-grade
/// rebalance is a follow-up — for now, subsequent inserts will
/// rebalance opportunistically via the existing split logic.
pub fn remove(
    root: u128,
    key: &Value,
    tx: &mut Tx,
    key_ty: &Type,
    val_ty: &Type,
) -> Result<u128, Error> {
    if root == EMPTY { return Ok(EMPTY); }
    match remove_inner(root, key, tx, key_ty, val_ty)? {
        RemoveResult::Unchanged => Ok(root),
        RemoveResult::Plain(h) => Ok(h),
        RemoveResult::Gone => Ok(EMPTY),
    }
}

fn remove_inner(
    node_hash: u128,
    key: &Value,
    tx: &mut Tx,
    key_ty: &Type,
    val_ty: &Type,
) -> Result<RemoveResult, Error> {
    let node = read_node(tx, node_hash, key_ty, val_ty).ok_or_else(|| Error::new(
        ErrorKind::Runtime,
        format!("pbtree: cell {node_hash:x} missing during remove"),
        Span::default(),
    ))?;
    match node {
        Node::Leaf { mut entries } => {
            match entries.binary_search_by(|(k, _)| compare_keys(k, key)) {
                Ok(pos) => { entries.remove(pos); }
                Err(_) => return Ok(RemoveResult::Unchanged),
            }
            if entries.is_empty() {
                Ok(RemoveResult::Gone)
            } else {
                let bytes = serialize_leaf(&entries);
                Ok(RemoveResult::Plain(write_node(tx, bytes)))
            }
        }
        Node::Inner { keys, children } => {
            let pos = descend_pos(&keys, key);
            let result = remove_inner(children[pos], key, tx, key_ty, val_ty)?;
            match result {
                RemoveResult::Unchanged => Ok(RemoveResult::Unchanged),
                RemoveResult::Plain(new_child) => {
                    let mut new_children = children;
                    new_children[pos] = new_child;
                    let bytes = serialize_inner(&keys, &new_children);
                    Ok(RemoveResult::Plain(write_node(tx, bytes)))
                }
                RemoveResult::Gone => {
                    // Drop children[pos] and one adjacent routing key.
                    // children[pos] sits between keys[pos-1] and keys[pos];
                    // remove keys[pos] when it exists, otherwise keys[pos-1].
                    let mut new_keys = keys;
                    let mut new_children = children;
                    new_children.remove(pos);
                    if pos < new_keys.len() {
                        new_keys.remove(pos);
                    } else if pos > 0 {
                        new_keys.remove(pos - 1);
                    }
                    if new_children.is_empty() {
                        return Ok(RemoveResult::Gone);
                    }
                    if new_keys.is_empty() && new_children.len() == 1 {
                        // Inner shrank to a single child — collapse.
                        // The child becomes the new subtree root.
                        return Ok(RemoveResult::Plain(new_children[0]));
                    }
                    let bytes = serialize_inner(&new_keys, &new_children);
                    Ok(RemoveResult::Plain(write_node(tx, bytes)))
                }
            }
        }
    }
}

/// Range query: returns values whose keys fall in `[lo, hi]`
/// (inclusive on both ends), in sorted order. Walks only the
/// subtrees overlapping the range.
pub fn range(
    root: u128,
    lo: &Value,
    hi: &Value,
    tx: &mut Tx,
    key_ty: &Type,
    val_ty: &Type,
) -> Vec<Value> {
    let mut out = Vec::new();
    if root == EMPTY { return out; }
    range_at(root, lo, hi, tx, key_ty, val_ty, &mut out);
    out
}

fn range_at(
    node_hash: u128,
    lo: &Value,
    hi: &Value,
    tx: &mut Tx,
    key_ty: &Type,
    val_ty: &Type,
    out: &mut Vec<Value>,
) {
    let Some(node) = read_node(tx, node_hash, key_ty, val_ty) else { return };
    match node {
        Node::Leaf { entries } => {
            for (k, v) in entries {
                if compare_keys(&k, lo).is_lt() { continue; }
                if compare_keys(&k, hi).is_gt() { break; }
                out.push(v);
            }
        }
        Node::Inner { keys, children } => {
            // Only descend into children whose key range intersects
            // [lo, hi]. children[i] holds keys in [keys[i-1], keys[i]),
            // with keys[-1] = -inf and keys[len] = +inf.
            let lo_pos = descend_pos(&keys, lo);
            let hi_pos = descend_pos(&keys, hi);
            for pos in lo_pos..=hi_pos {
                if pos >= children.len() { break; }
                range_at(children[pos], lo, hi, tx, key_ty, val_ty, out);
            }
        }
    }
}

/// Walk every entry in sorted order. Used by full-walk builtins.
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
    let Some(node) = read_node(tx, hash, key_ty, val_ty) else { return };
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
    if !out.insert(hash) { return; }
    let Some(bytes) = kv.get(hash) else { return };
    let Some(node) = deserialize_node(&bytes, key_ty, val_ty) else { return };
    if let Node::Inner { children, .. } = node {
        for c in children {
            walk_reachable(c, kv, key_ty, val_ty, out);
        }
    }
}

// ---------- serialization ----------

fn serialize_inner(keys: &[Value], children: &[u128]) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + 4 + keys.len() * 16 + children.len() * 16);
    out.push(TAG_INNER);
    out.extend_from_slice(&(keys.len() as u32).to_be_bytes());
    for k in keys {
        let kb = crate::serialize::serialize(k);
        out.extend_from_slice(&(kb.len() as u32).to_be_bytes());
        out.extend_from_slice(&kb);
    }
    for c in children {
        out.extend_from_slice(&c.to_be_bytes());
    }
    out
}

fn serialize_leaf(entries: &[(Value, Value)]) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + 4);
    out.push(TAG_LEAF);
    out.extend_from_slice(&(entries.len() as u32).to_be_bytes());
    for (k, v) in entries {
        let kb = crate::serialize::serialize(k);
        out.extend_from_slice(&(kb.len() as u32).to_be_bytes());
        out.extend_from_slice(&kb);
        let vb = crate::serialize::serialize(v);
        out.extend_from_slice(&(vb.len() as u32).to_be_bytes());
        out.extend_from_slice(&vb);
    }
    out
}

fn deserialize_node(bytes: &[u8], key_ty: &Type, val_ty: &Type) -> Option<Node> {
    let (&tag, rest) = bytes.split_first()?;
    match tag {
        TAG_INNER => {
            if rest.len() < 4 { return None; }
            let n = u32::from_be_bytes(rest[0..4].try_into().ok()?) as usize;
            let mut p = 4;
            let mut keys = Vec::with_capacity(n);
            for _ in 0..n {
                if rest.len() < p + 4 { return None; }
                let kl = u32::from_be_bytes(rest[p..p+4].try_into().ok()?) as usize;
                p += 4;
                if rest.len() < p + kl { return None; }
                let key = crate::serialize::deserialize(&rest[p..p+kl], key_ty)?;
                p += kl;
                keys.push(key);
            }
            let mut children = Vec::with_capacity(n + 1);
            for _ in 0..(n + 1) {
                if rest.len() < p + 16 { return None; }
                let h = u128::from_be_bytes(rest[p..p+16].try_into().ok()?);
                children.push(h);
                p += 16;
            }
            Some(Node::Inner { keys, children })
        }
        TAG_LEAF => {
            if rest.len() < 4 { return None; }
            let n = u32::from_be_bytes(rest[0..4].try_into().ok()?) as usize;
            let mut p = 4;
            let mut entries = Vec::with_capacity(n);
            for _ in 0..n {
                if rest.len() < p + 4 { return None; }
                let kl = u32::from_be_bytes(rest[p..p+4].try_into().ok()?) as usize;
                p += 4;
                if rest.len() < p + kl { return None; }
                let key = crate::serialize::deserialize(&rest[p..p+kl], key_ty)?;
                p += kl;
                if rest.len() < p + 4 { return None; }
                let vl = u32::from_be_bytes(rest[p..p+4].try_into().ok()?) as usize;
                p += 4;
                if rest.len() < p + vl { return None; }
                let val = crate::serialize::deserialize(&rest[p..p+vl], val_ty)?;
                p += vl;
                entries.push((key, val));
            }
            Some(Node::Leaf { entries })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kv::EmptyKv;
    use crate::tx::Tx;

    #[test]
    fn get_returns_none_on_empty() {
        let kv = EmptyKv;
        let mut tx = Tx::new(&kv);
        assert!(get(EMPTY, &Value::U64(1), &mut tx, &Type::U64, &Type::U64).is_none());
    }

    #[test]
    fn set_then_get_roundtrip() {
        let kv = EmptyKv;
        let mut tx = Tx::new(&kv);
        let mut root = EMPTY;
        for k in &[5u64, 10, 1, 99, 50] {
            root = set(root, Value::U64(*k), Value::U64(*k * 10), &mut tx, &Type::U64, &Type::U64).unwrap();
        }
        for k in &[1u64, 5, 10, 50, 99] {
            assert_eq!(
                get(root, &Value::U64(*k), &mut tx, &Type::U64, &Type::U64),
                Some(Value::U64(k * 10)),
            );
        }
        assert_eq!(get(root, &Value::U64(7), &mut tx, &Type::U64, &Type::U64), None);
    }

    #[test]
    fn entries_yields_sorted() {
        let kv = EmptyKv;
        let mut tx = Tx::new(&kv);
        let mut root = EMPTY;
        for k in &[7u64, 3, 1, 9, 5, 2, 8] {
            root = set(root, Value::U64(*k), Value::U64(*k * 10), &mut tx, &Type::U64, &Type::U64).unwrap();
        }
        let es = entries(root, &mut tx, &Type::U64, &Type::U64);
        let keys: Vec<u64> = es.iter().map(|(k, _)| match k {
            Value::U64(n) => *n,
            _ => panic!(),
        }).collect();
        assert_eq!(keys, vec![1, 2, 3, 5, 7, 8, 9]);
    }

    #[test]
    fn range_returns_inclusive_bounds() {
        let kv = EmptyKv;
        let mut tx = Tx::new(&kv);
        let mut root = EMPTY;
        for k in 1u64..=20 {
            root = set(root, Value::U64(k), Value::U64(k * 100), &mut tx, &Type::U64, &Type::U64).unwrap();
        }
        let r = range(root, &Value::U64(5), &Value::U64(8), &mut tx, &Type::U64, &Type::U64);
        let vals: Vec<u64> = r.iter().map(|v| match v {
            Value::U64(n) => *n,
            _ => panic!(),
        }).collect();
        assert_eq!(vals, vec![500, 600, 700, 800]);
    }

    #[test]
    fn remove_clears_entry() {
        let kv = EmptyKv;
        let mut tx = Tx::new(&kv);
        let mut root = EMPTY;
        for k in 1u64..=5 {
            root = set(root, Value::U64(k), Value::U64(k * 10), &mut tx, &Type::U64, &Type::U64).unwrap();
        }
        root = remove(root, &Value::U64(3), &mut tx, &Type::U64, &Type::U64).unwrap();
        assert_eq!(get(root, &Value::U64(3), &mut tx, &Type::U64, &Type::U64), None);
        assert_eq!(get(root, &Value::U64(2), &mut tx, &Type::U64, &Type::U64), Some(Value::U64(20)));
    }

    #[test]
    fn remove_last_entry_returns_empty_root() {
        let kv = EmptyKv;
        let mut tx = Tx::new(&kv);
        let root = set(EMPTY, Value::U64(7), Value::U64(70), &mut tx, &Type::U64, &Type::U64).unwrap();
        let after = remove(root, &Value::U64(7), &mut tx, &Type::U64, &Type::U64).unwrap();
        assert_eq!(after, EMPTY);
    }

    #[test]
    fn remove_propagates_through_inner() {
        // Force the tree to split into multiple leaves, then remove
        // entire leaf-worth of entries — parent's child pointer
        // should be dropped and inner structure should shrink.
        let kv = EmptyKv;
        let mut tx = Tx::new(&kv);
        let mut root = EMPTY;
        for k in 1u64..=100 {
            root = set(root, Value::U64(k), Value::U64(k), &mut tx, &Type::U64, &Type::U64).unwrap();
        }
        // Remove every key. Tree should end up as EMPTY.
        for k in 1u64..=100 {
            root = remove(root, &Value::U64(k), &mut tx, &Type::U64, &Type::U64).unwrap();
        }
        assert_eq!(root, EMPTY);
    }

    #[test]
    fn remove_missing_key_is_no_op() {
        let kv = EmptyKv;
        let mut tx = Tx::new(&kv);
        let root = set(EMPTY, Value::U64(1), Value::U64(10), &mut tx, &Type::U64, &Type::U64).unwrap();
        let after = remove(root, &Value::U64(99), &mut tx, &Type::U64, &Type::U64).unwrap();
        assert_eq!(root, after);
    }

    #[test]
    fn many_entries_split_into_multiple_leaves() {
        // Insert > B entries to force at least one leaf split. After
        // the split the trie is one inner + 2 leaves, and lookups
        // still work.
        let kv = EmptyKv;
        let mut tx = Tx::new(&kv);
        let mut root = EMPTY;
        for k in 1u64..=100 {
            root = set(root, Value::U64(k), Value::U64(k), &mut tx, &Type::U64, &Type::U64).unwrap();
        }
        // Spot-check a few keys.
        for k in [1, 33, 67, 100] {
            assert_eq!(
                get(root, &Value::U64(k), &mut tx, &Type::U64, &Type::U64),
                Some(Value::U64(k)),
            );
        }
        // Sorted iteration still produces every key in order.
        let es = entries(root, &mut tx, &Type::U64, &Type::U64);
        assert_eq!(es.len(), 100);
        for (i, (k, _)) in es.iter().enumerate() {
            assert_eq!(*k, Value::U64((i as u64) + 1));
        }
    }
}
