# Indexes

An index is a derived state slot, auto-maintained by the compiler so it stays
consistent with a primary collection. The pattern replaces SQL's `CREATE INDEX`:
instead of metadata attached to a table, the index is a real state slot you declare
and read like any other map.

Two orthogonal choices when declaring an index:

**Uniqueness** — the keyword:

| Keyword | Slot value shape | When to use |
|---|---|---|
| `unique_index N on P.f` | `<F, K>` — one primary key per field value | uniqueness invariants (email, slug, owner) |
| `index N on P.f` | `<F, [K]>` — list of primary keys per field value | grouping (department, status, age) |

`index` is the default (non-unique) — same as SQL. Opt into uniqueness with
`unique_index`.

**Backend** — the slot's declared type:

| Slot declaration | Backend | Iteration order | Best for |
|---|---|---|---|
| `state x: pmap<F, K>` | hash-ordered HAMT | random | point lookup, no range |
| `state x: pbtree<F, K>` | key-sorted radix trie | sorted | range queries, ORDER BY, sorted scan |

Same keyword, different slot type — the compiler picks the right maintenance
ops based on which backend you declared.

## Unique index

```rd
struct User {
    id: u64,
    email: string,
}

state users:    pmap<u64, User>;
state by_email: pmap<string, u64>;
unique_index by_email on users.email;
```

Every write `users[id] = u` first checks `by_email[u.email]`. If a *different*
primary key is already there, the tx aborts with a runtime error:

```
unique constraint violation: key "alice@x.com" already maps to 1, refused to remap to 2
```

Re-writing the same `(email, id)` pair is idempotent — same-value replays don't
trip the check. The check costs one extra `O(log32 N)` HAMT walk per indexed
write; you only pay it on `unique_index`, not `index`.

Two transactions writing different emails don't conflict (disjoint cells in
`by_email`'s trie). Two transactions writing the same email under different ids
are mutually exclusive — whoever commits first wins, and the other re-runs and
fails the constraint check.

## Multi (non-unique) index

```rd
state users:   pmap<u64, User>;
state by_dept: pmap<string, [u64]>;
index by_dept on users.dept;
```

Every write `users[id] = u` reads the current list at `by_dept[u.dept]`,
appends-if-not-present, and writes it back. Re-registering the same `id` is
idempotent. The index value is `[u64]`, not `u64` — the typeck rejects a singular
slot with a hint to switch to `unique_index`.

Lookup gives the full list:

```rd
let eng_ids = by_dept["eng"];
for id in eng_ids { ... }
```

## Concurrency under both forms

Two transactions writing to *different* field values touch *different* index cells
— they don't conflict. Two transactions writing to the same field value do
conflict on that one cell, but that's true for any append-to-list scheme (and
SQL's btree leaf serialization works the same way).

## Lookup

Indexes are pmaps, so reads use indexing syntax:

```rd
// Unique-index lookup — one record back.
entry view fn user_by_email(e: string) -> User {
    let id = by_email[e];
    return users[id];
}

// Multi-index lookup — list of records back.
entry view fn users_in_dept(d: string) -> [User] {
    return [users[id] for id in by_dept[d]];
}
```

A missing key reads as the value type's default — `0u64` for unique, empty `[]` for
multi. Use `pmap_contains` if you need to distinguish "never indexed" from
"indexed to default".

## Type rules

The compiler validates at typeck time:

- The primary must be a `pmap<K, V>`.
- For `unique_index`, the index slot must be `pmap<F, K>` where `F` is the projected
  field's type.
- For `index`, the index slot must be `pmap<F, [K]>` — array-valued.
- The projection path (`primary.field`, or `primary.f1.f2`, ...) must resolve through
  the value type's struct fields to a leaf type matching `F`.

Any mismatch is a compile error. The multi-index slot-type rule includes a hint:
"did you mean `unique_index`?" if you wrote `pmap<F, K>` instead of `pmap<F, [K]>`.

## Nested projections

Multi-level paths work:

```rd
struct Email { local: string, domain: string }
struct User  { id: u64, email: Email }

state users:     pmap<u64, User>;
state by_domain: pmap<string, u64>;
index by_domain on users.email.domain;
```

`users[id] = u` now also emits `by_domain[u.email.domain] = id`.

## Multiple indexes on one primary

You can mix kinds and backends:

```rd
state users:    pmap<u64, User>;
state by_email: pmap<string, u64>;          // unique, hash-ordered
state by_dept:  pmap<string, [u64]>;        // multi, hash-ordered
state by_age:   pbtree<u64, [u64]>;         // multi, key-sorted (range queries)

unique_index by_email on users.email;
index        by_dept  on users.dept;
index        by_age   on users.age;
```

Each write emits maintenance for all three — the compiler chains them in
declaration order, picking pmap or pbtree maintenance ops based on each slot's
type.

## Sorted indexes

When the slot is `pbtree<F, _>`, range queries on the indexed field run through
the sorted-trie path, narrowing to the slice covered by `[lo, hi]`:

```rd
state holders: pmap<u64, Holder>;
state by_score: pbtree<u64, [u64]>;
index by_score on holders.score;

entry view fn top_scorers(min_score: u64, max_score: u64) -> [u64] {
    // Returns lists-of-ids for each score in [min..max], in sorted order.
    let groups = pbtree_range(by_score, min_score, max_score);
    let out: [u64] = [];
    for ids in groups {
        for id in ids { out = out ++ [id]; }
    }
    return out;
}

entry view fn lowest_scoring_holder() -> u64 {
    for ids in by_score {           // walks in score-sorted order
        return ids[0];               // smallest score's first id
    }
    return 0u64;
}
```

`unique_index` over a `pbtree<F, K>` slot gives you a sorted unique index —
`pbtree_range(by_id, lo, hi)` returns the primary keys in `[lo, hi]` directly,
no list-of-ids unwrapping needed.

## Iterating an index

Indexes are pmaps, so all the persistent-collection walks work
([builtins](builtins.md#persistent-maps-pmapk-v)):

```rd
entry view fn all_active_users() -> [u64] {
    return [v for v in by_active_flag if v != 0u64];
}
```

## Limitation: stale entries on field-changing updates

When a primary entry is overwritten with a new value whose projected field *differs*
from the prior value, the new index entry is added correctly — but the **old index
entry is not removed**. The previous projected key still points at the same primary
key, so it's a stale back-link.

```rd
users[1u64] = User { id: 1u64, email: "old@x.com" };
users[1u64] = User { id: 1u64, email: "new@x.com" };

by_email["new@x.com"]    // → 1, correct
by_email["old@x.com"]    // → 1, STALE; the actual primary entry no longer has this email
```

This is fine for insert-mostly workloads (audit logs, registries, append-only
ledgers) but bites if you mutate the indexed field on existing entries. The
proper fix needs a `pmap_remove` builtin and a pre-write read of the prior value
to detect field changes — a future slice.

Until then: design schemas so the indexed field is set at insert and never changes
(IDs, immutable timestamps, owner addresses), or rebuild the index periodically
from the primary.

## What this replaces

In SQL:

```sql
CREATE TABLE users (id BIGINT, email VARCHAR, dept VARCHAR);
CREATE UNIQUE INDEX users_by_email ON users(email);
CREATE INDEX users_by_dept ON users(dept);
```

In rend:

```rd
struct User { id: u64, email: string, dept: string }
state users:    pmap<u64, User>;
state by_email: pmap<string, u64>;
state by_dept:  pmap<string, [u64]>;
unique_index by_email on users.email;
index        by_dept  on users.dept;
```

The indexes are structural state, not metadata. Two transactions writing to
disjoint keys touch disjoint cells of *both* `users` and the index pmaps, and both
commits proceed in parallel under OCC — no serialization point.

## See also

- [`tests/indexes.rs`](../../tests/indexes.rs) — full test surface including type errors
- [state and persistence](state-and-persistence.md) — how `pmap` itself works
- [comprehensions](comprehensions.md) — query shapes over indexes
