# KV backend

`rend` doesn't ship its own storage. The host provides a key-value store that
implements the `Kv` trait; the runtime reads from it and produces a write set the
host can apply.

## The `Kv` trait

```rust
pub trait Kv {
    fn get(&self, key: u128) -> Option<Value>;
    fn get_many(&self, keys: &[u128]) -> Vec<Option<Value>>;
}
```

Keys are u128 content hashes. The runtime treats them as opaque cell identifiers —
the host never has to interpret them.

`get_many` is the batched fetch. The optimizer pre-computes read clusters at compile
time, so the runtime can issue one `get_many` for a whole cluster of independent
reads instead of N sequential `get`s. Implement it for real performance; the default
implementation could fall back to per-key `get` calls.

## Applying writes

```rust
pub trait MutableKv: Kv {
    fn apply(&mut self, writes: &[(u128, Value)]);
}
```

The host calls `apply` after deciding to commit a transaction. There's no rollback
path: if the host doesn't `apply`, the writes are simply discarded.

For OCC validation before commit, the host compares the tx's `reads` against the
live KV state. If anything has changed since the tx read it, the tx is rejected and
must re-run.

## In-memory backend

For testing and embedding-as-library use cases:

```rust
use rend::kv::InMemoryKv;

let mut kv = InMemoryKv::new();
let outcome = engine.execute(src, fuel, &kv)?;
kv.apply(&outcome.writes);
```

`InMemoryKv` is a `HashMap<u128, Value>` wrapped to implement `Kv` and `MutableKv`.
Production hosts plug in their own backend (typically a persistent store with
content-addressed cells).

## What the runtime stores

| Cell | What it holds |
|---|---|
| state slot root | the current value at that state slot |
| `pmap` root cell | a content hash pointing at the HAMT's root node |
| `pmap` interior node | child slot pointers + a small bitmap |
| `pmap` leaf | the K/V pair |
| `pvec` root cell | a hash pointing at the trie's root |
| `pvec` interior / leaf | trie structure |
| struct field path | a single field's value (granular state) |

Most reads/writes hit specific leaf cells, not the root. That's why disjoint
pmap/pvec edits don't conflict — the root pointer changes but most of the tree's
hashes don't.

## See also

- [persistent structures](../internals/persistent-structures.md) — what's actually in those cells
- [`src/kv.rs`](../../src/kv.rs) — trait and `InMemoryKv`
- [`tests/granular_state.rs`](../../tests/granular_state.rs) — granular-cell reads/writes
