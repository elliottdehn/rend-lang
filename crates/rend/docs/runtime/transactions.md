# Transactions, OCC, and 3-way merge

A transaction in `rend` is a unit of atomic state change with explicit read and write
sets. The runtime never silently mutates the host's KV — it produces a write set the
host commits (or doesn't).

## `TxContext`

```rust
pub struct TxContext {
    pub sender: Value,           // typically Value::Address
    pub block_timestamp: u64,
    pub block_number: u64,
}
```

The `*_with_context` engine methods take a `TxContext`. The bytecode reads these
fields through the `msg_sender()`, `block_timestamp()`, `block_number()` builtins.
Without a context, defaults are zero-address / zero / zero.

## Reads, writes, events

```rust
pub struct ExecOutcome {
    pub result: Value,
    pub reads:  Vec<(u128, Value)>,    // OCC read set
    pub writes: Vec<(u128, Value)>,    // OCC write set
    pub events: Vec<EventRecord>,
    pub pmap_types: Vec<(u128, Type)>,  // root keys for pmap states touched
    pub pvec_types: Vec<(u128, Type)>,
    pub node_cells_written: usize,     // for accounting / metering
}
```

Keys are u128 content hashes. The host doesn't have to interpret them — they're
opaque cell identifiers from the KV's perspective.

## Optimistic concurrency

Two transactions running against the same prior state:

1. Both produce read sets and write sets.
2. The host orders them — first one wins. Apply writes, advance to "committed at".
3. The second tx is checked: are any of its read keys' values different from what it
   read? If yes, it conflicts and must re-run. If no, apply its writes too.

This is straightforward OCC, with one twist: the conflict unit isn't a state slot
or a struct, it's the KV cell. Granular state means two transactions modifying
different fields of the same struct don't conflict, and two transactions modifying
different keys of the same map don't conflict.

## 3-way merge for `pmap` / `pvec`

`pmap` and `pvec` are persistent — each modification produces a new root pointer that
points to a tree where most nodes are shared with the prior root. When two
transactions both edit a `pmap`, their write sets conflict on the *root pointer*
cell, even if their actual edits are to disjoint subtrees.

The runtime does a 3-way merge:

- Common ancestor: the root the txs both read.
- "Theirs": the root tx A wrote.
- "Ours": the root tx B wrote.

The merge walks all three, takes A's subtrees where B didn't change anything, takes
B's subtrees where A didn't change anything, and conflicts only where both edited
the same node. For most disjoint-subtree workloads, the merge succeeds.

This turns root-pointer conflicts (which would force one tx to re-run) into
non-conflicts whenever the actual data layouts allow it.

## Re-entry guard

A function annotated `nore` is added to a per-tx active-set. If the same `(module,
fn)` pair is entered while already active, the runtime aborts with a re-entry error.
This catches the classic "callback into me from a host hook before my first call
finishes" pattern.

## See also

- [`src/occ.rs`](../../src/occ.rs) — OCC + merge implementation
- [`tests/occ.rs`](../../tests/occ.rs) — conflict, retry, merge tests
- [`tests/concurrent_reads.rs`](../../tests/concurrent_reads.rs) — disjoint reads under load
- [`tests/pmap.rs`](../../tests/pmap.rs) — `pmap` granularity under conflict
