# Persistent structures

`pmap` and `pvec` are the two persistent collections. Both materialize their internal
tree structure as KV cells, one per node. That's what gives concurrent edits to
disjoint subtrees a clean 3-way merge.

## `pmap` — HAMT

A Hash Array Mapped Trie. Each interior node has 32 child slots; the slot at depth
`d` is selected by 5 bits of the key's hash starting at bit `5*d`. The node holds
those slot pointers plus a small bitmap recording which slots are populated.

Per-node KV layout:

| Cell key | Content |
|---|---|
| root cell (anchored by state slot key) | hash of the root node |
| interior node hash | bitmap + 32 child slots (each is a hash or empty) |
| leaf node hash | the (key, value) pair |

A `pmap[k] = v` write:

1. Hash `k`, walk down 5 bits at a time to find the leaf path.
2. Make a new leaf cell with `(k, v)`.
3. Bubble up: at each interior node, replace the relevant child pointer, write a new
   interior node cell, repeat to the root.
4. Write a new root pointer.

The number of new cells written is `O(log_32 N)` for `N` entries — typically 1–6 for
realistic sizes. Most of the tree stays shared; this is what makes 3-way merge
effective.

`pmap_contains(p, k)` walks the tree without materializing the value, returning true
iff a leaf for that key exists. This is how `pmap` distinguishes "set to default"
from "never set."

## `pvec` — indexed trie

A 32-way indexed trie. The path to index `i` is determined by the base-32 digits of
`i`. No bitmap: every populated slot is dense by construction (you can't have
gap-skipping indices), so the bitmap-bookkeeping `pmap` needs is unnecessary here.

Per-node KV layout:

| Cell key | Content |
|---|---|
| root cell | length + hash of the root node |
| interior | dense slot array of child hashes |
| leaf | the value at that index |

`pvec_push(v, x)`:

1. Allocate index `len(v)`.
2. Walk to the leaf, creating nodes along the way.
3. Increment length, bubble up new interior nodes, write new root.

Two concurrent `pvec_push` calls conflict on the length cell — that's the serial
point. Concurrent indexed writes to disjoint pre-existing slots don't conflict.

## Why granular nodes (instead of one big serialized blob)?

A pmap with 10,000 entries that gets one write would, in a one-cell-per-collection
model, produce a write set containing the entire 10,000-entry serialization. With
the tree-of-cells model, the write set is the path from root to leaf — about 6
cells.

This matters for OCC: the read set the host validates and the write set it commits
shrink from O(N) to O(log N). For a high-fanout workload, that's the difference
between practical and unworkable.

## 3-way merge

When two transactions concurrently produce new roots `A` and `B` from a common
ancestor `C`, the merge walks all three:

- For each subtree, if `A == C` (this side untouched), take `B`.
- If `B == C` (other side untouched), take `A`.
- If both differ, recursively merge.
- At a leaf where both edited the same key, conflict.

For workloads where transactions touch disjoint key ranges (typical: balance
ledgers, holder lists, multi-user state), most merges succeed without re-running.

## See also

- [`src/pmap.rs`](../../src/pmap.rs) — HAMT implementation
- [`src/pvec.rs`](../../src/pvec.rs) — indexed trie implementation
- [`src/occ.rs`](../../src/occ.rs) — 3-way merge driver
- [`tests/pmap.rs`](../../tests/pmap.rs) — semantics, granularity, conflict shape
