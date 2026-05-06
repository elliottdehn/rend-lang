# State and persistence

State declarations live at the top of a module and define the persistent slots the
runtime materializes against the host's KV store:

```rd
state ctr:       i64;
state owner:     Address;
state balances:  map<Address, u64>;
state members:   pmap<Address, u64>;
state log:       pvec<Transfer>;
```

Reads look like ordinary variable reads. Writes look like assignments. The compiler
emits `LoadState` / `StoreState` instructions, and the runtime turns those into
specific KV cell fetches and writes.

## Granular state

State doesn't live in one big serialized blob. The compiler walks each state's type
and assigns each leaf field its own KV cell key:

```rd
struct Meta {
    name:   string,
    symbol: string,
    decimals: u32,
}

state meta: Meta;
state balances: map<Address, u64>;
```

`meta.name`, `meta.symbol`, and `meta.decimals` are three separate cells. `balances[who]`
is one cell per `who`. Two transactions that touch disjoint cells don't conflict
under [OCC](../runtime/transactions.md).

For nested struct fields, the compiler chains `child(parent_key, field_name)` to derive
the leaf key. The chain is materialized as a `PathSpec` in the artifact.

## `map` vs `pmap`

`map<K, V>` is the simple per-cell map: each `key` becomes one KV cell. A missing
cell reads as the default value of `V`. Writing the default is indistinguishable from
"never written."

`pmap<K, V>` is a persistent map backed by a HAMT (hash array mapped trie). Each
tree node is its own content-addressed cell. `pmap` distinguishes "set to default"
from "never set" via `pmap_contains(p, k)`. The trie shape is what gives concurrent
transactions touching different subtrees a clean 3-way merge.

| | `map<K,V>` | `pmap<K,V>` |
|---|---|---|
| Storage | one cell per key | tree-of-cells (HAMT) |
| Missing key | reads as default | reads as default; `pmap_contains` distinguishes |
| Conflict granularity | per-key | per-tree-node |
| Iteration | not supported | not supported |
| Best for | direct-keyed indices | when "ever set?" matters or for high parallel write rate |

## `pbtree<K, V>` — sorted persistent map

A persistent **B+-tree**. Each node lives in its own KV cell; leaves carry
sorted `(k, v)` entries; inner nodes carry routing keys and child pointers.
That makes iteration **key-sorted** and unlocks two query shapes that
`pmap` can't do efficiently:

- `pbtree_range(p, lo, hi)` — values whose keys fall in `[lo, hi]`, in sorted
  order. Walks only the subtrees covering the range; out-of-range subtrees
  stay unfetched.
- `for x in pbtree_state` — values in sorted order. With `break`, this is
  `LIMIT N ORDER BY k ASC` for free.

```rd
state holders: pbtree<u64, Holder>;

entry view fn page(after_id: u64, n: u64) -> [Holder] {
    return pbtree_range(holders, after_id, after_id + n);
}

entry view fn first_active() -> u64 {
    for h in holders {
        if h.active { return h.id; }
    }
    return 0u64;
}
```

B+-tree leaves split by *key value*, not bit position, so pbtree narrows
range queries correctly regardless of how keys are distributed in the bit
space — dense monotonic IDs (`1, 2, 3, ...`) work just as well as
widely-spread timestamps.

**Slice-1 limit:** only `u64` keys are wired through. Other widths (`i64`
with sign-bit flip, `u32`, `u128`) and string keys extend the same machinery
and are queued for follow-up slices.

Choose between `pmap` and `pbtree` by access pattern:

| Need | Use |
|---|---|
| Point lookup by key | either; `pmap` is slightly cheaper per fetch |
| Range query (`WHERE k BETWEEN lo AND hi`) | `pbtree` |
| Sorted iteration (`ORDER BY k ASC`) | `pbtree` |
| `LIMIT N` after sort | `pbtree` + `break` in for-loop |
| "Was this key ever set?" | either; `pmap_contains` / `pbtree_contains` |

## `pvec`

`pvec<T>` is an indexed-trie persistent vector: random-access reads/writes by `u64`
index, plus `pvec_push` to append. The internal trie shape gives the same disjoint-edit
parallelism as `pmap`.

```rd
state events: pvec<Transfer>;

entry fn record(t: Transfer) -> u64 {
    let i = pvec_push(events, t);
    events[i] = t;        // overwrite a previously pushed slot
    return pvec_len(events);
}
```

`pvec_push` is a serialization point under OCC: two concurrent pushes will conflict
on the length cell. Indexed writes to disjoint slots don't.

## Constants

Module-level immutables:

```rd
const MAX_SUPPLY: u64 = 1_000_000u64;
const FEE_BPS:    u64 = 30u64;
```

Constants are evaluated at every use site (functionally a compile-time substitution).
The typeck checks that the declared type matches the expression's type.

## Default values

Every state slot has a default, derived from its type:

| Type | Default |
|---|---|
| `i*`, `u*` | `0` |
| `bool` | `false` |
| `string` | `""` |
| `Address` | the empty address |
| `bytes` | empty `[]` |
| struct | recursively defaulted |
| enum | first variant (with defaulted payload, if any) |
| `map`, `pmap`, `pvec` | empty |
| interface | unbound (target module = `""`) |

A fresh storage with no writes will read every state slot as its default. This
matters for `Engine::deploy` flows: the constructor (`fn main()`) often runs against
an empty store and writes initial values.

## Deleting from a state collection

```rd
state ledger: pmap<u64, Balance>;
state holders: pbtree<u64, Holder>;

entry fn close_account(id: u64) -> u64 {
    delete ledger[id];
    delete holders[id];
    return 1u64;
}
```

`delete state[k];` works on `pmap` and `pbtree` slots. Indexes covering the
state are auto-cleaned at the same write site:

- A `unique_index` entry is removed if and only if it still maps to the
  primary key being deleted (a stale entry pointing at someone else stays).
- A multi `index` entry has the primary key filtered from its list. If the
  list becomes empty, the index entry itself is removed.

`pvec` slots can't be deleted by index (would require shifting subsequent
entries) — that's a future feature. Plain `map<K, V>` doesn't support
`delete` either; assigning the default value gives the same observable
behavior on a per-cell map.

## Iterating over a state collection

Both `pmap` and `pvec` can be walked end-to-end, which is the foundation for
queries over persistent state. The walks classify as `ReadOnly`, so they're
callable from `view` functions and `Engine::query`:

```rd
state amounts: pmap<Address, u64>;

entry view fn high_balance_holders() -> [Address] {
    return [e.0 for e in pmap_entries(amounts) if e.1 > 1000u64];
}

entry view fn total_supply() -> u64 {
    let total = 0u64;
    for v in amounts { total = total + v; }    // sugar for pmap_values
    return total;
}
```

`for x in pmap_state` and `for x in pvec_state` are **streaming** — the loop
fetches one element at a time, never materializing the whole collection. `break`
exits without touching the rest of the trie / vector. Comprehensions over the
same sources also stream — only the comprehension's *output* array materializes,
not the source.

```rd
// Streaming. break-early avoids fetching the rest.
for u in users {
    if u.email == target { return u; }
    break;
}

// Also streaming on the source. Output array grows as we go.
let active = [u for u in users if u.active];
```

Use the explicit walks (`pmap_values`, `pmap_entries`, `pmap_keys`,
`pvec_to_array`) only when you actually need the materialized array — e.g.
storing it, returning it, or iterating it more than once. They're O(N) cell
reads up front and O(N) memory.

For non-default iteration shapes (entries with keys, just keys, etc.), the
explicit walk is currently the only path:

```rd
// Yields (key, value) tuples — no streaming form yet.
for kv in pmap_entries(p) { ... use kv.0 and kv.1 ... }
```

[indexes](indexes.md) cover the case where you want to narrow before walking.

## See also

- [persistent structures](../internals/persistent-structures.md) — HAMT and trie internals
- [`examples/27_token.rd`](../../examples/27_token.rd) — granular struct + map state
- [`examples/33_pmap.rd`](../../examples/33_pmap.rd) — `pmap` semantics
- [`examples/34_pvec.rd`](../../examples/34_pvec.rd) — `pvec` semantics
