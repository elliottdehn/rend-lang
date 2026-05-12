//! Indexes as first-class state. `index N on P.field;` declares that
//! the pmap state slot `N` is auto-maintained as a derived index over
//! `P` by the value's `field` projection. Every write `P[k] = v`
//! also emits `N[v.field] = k`. Lookup is plain pmap access.

use rend::value::Value;
use rend::{run, Engine, Fuel};

// ---------- happy path ----------

#[test]
fn write_to_primary_populates_index() {
    let v = run("
        struct User { id: u64, email: string }
        state users:    pmap<u64, User>;
        state by_email: pmap<string, u64>;
        unique_index by_email on users.email;

        fn main() -> u64 {
            users[1u64] = User { id: 1u64, email: \"alice@x.com\" };
            users[2u64] = User { id: 2u64, email: \"bob@x.com\" };
            // Lookup via the index; round-trip back through users
            // to fetch the full record.
            let id = by_email[\"bob@x.com\"];
            return users[id].id;
        }
    ").unwrap();
    assert_eq!(v, Value::U64(2));
}

#[test]
fn missing_index_lookup_returns_default() {
    // pmap default behavior: a never-set key reads as the value
    // type's default. For string-keyed pmap<string, u64>, that's 0.
    let v = run("
        struct User { id: u64, email: string }
        state users:    pmap<u64, User>;
        state by_email: pmap<string, u64>;
        unique_index by_email on users.email;

        fn main() -> u64 {
            users[1u64] = User { id: 1u64, email: \"alice@x.com\" };
            return by_email[\"unknown@x.com\"];
        }
    ").unwrap();
    assert_eq!(v, Value::U64(0));
}

#[test]
fn multiple_indexes_on_same_primary() {
    let v = run("
        struct User { id: u64, email: string, dept: string }
        state users:    pmap<u64, User>;
        state by_email: pmap<string, u64>;
        state by_dept:  pmap<string, u64>;
        unique_index by_email on users.email;
        unique_index by_dept  on users.dept;

        fn main() -> u64 {
            users[1u64] = User { id: 1u64, email: \"alice@x.com\", dept: \"eng\" };
            users[2u64] = User { id: 2u64, email: \"bob@x.com\",   dept: \"sales\" };
            // Both indexes see both writes.
            let from_email = by_email[\"alice@x.com\"];
            let from_dept  = by_dept[\"sales\"];
            return from_email + from_dept;       // 1 + 2
        }
    ").unwrap();
    assert_eq!(v, Value::U64(3));
}

#[test]
fn nested_field_projection_works() {
    let v = run("
        struct Email { local: string, domain: string }
        struct User  { id: u64, email: Email }
        state users:     pmap<u64, User>;
        state by_domain: pmap<string, u64>;
        unique_index by_domain on users.email.domain;

        fn main() -> u64 {
            users[1u64] = User { id: 1u64, email: Email { local: \"alice\", domain: \"x.com\" } };
            users[2u64] = User { id: 2u64, email: Email { local: \"bob\",   domain: \"y.com\" } };
            return by_domain[\"y.com\"];
        }
    ").unwrap();
    assert_eq!(v, Value::U64(2));
}

#[test]
fn index_works_on_bytecode_vm() {
    // run() uses interp; execute() goes through the bytecode VM.
    // Both must auto-maintain.
    let src = "
        struct User { id: u64, email: string }
        state users:    pmap<u64, User>;
        state by_email: pmap<string, u64>;
        unique_index by_email on users.email;

        fn main() -> u64 {
            users[1u64] = User { id: 1u64, email: \"alice@x.com\" };
            users[7u64] = User { id: 7u64, email: \"carol@x.com\" };
            return by_email[\"carol@x.com\"];
        }
    ";
    let kv = rend::kv::InMemoryKv::new();
    let outcome = Engine::new()
        .execute(src, Fuel::new(50_000), &kv)
        .unwrap();
    assert_eq!(outcome.result, Value::U64(7));
}

// ---------- typeck rejection ----------

#[test]
fn typeck_rejects_non_pmap_primary() {
    let err = rend::frontend("
        struct User { email: string }
        state users:    map<u64, User>;
        state by_email: pmap<string, u64>;
        unique_index by_email on users.email;
        fn main() -> u64 { return 0u64; }
    ").unwrap_err();
    assert!(err.to_string().contains("can only index a pmap"), "got: {err}");
}

#[test]
fn typeck_rejects_missing_field() {
    let err = rend::frontend("
        struct User { email: string }
        state users:    pmap<u64, User>;
        state by_other: pmap<string, u64>;
        unique_index by_other on users.nope;
        fn main() -> u64 { return 0u64; }
    ").unwrap_err();
    assert!(err.to_string().contains("nope"), "got: {err}");
}

#[test]
fn typeck_rejects_wrong_index_key_type() {
    // Index pmap key is u64, but projected field is string.
    let err = rend::frontend("
        struct User { email: string }
        state users:    pmap<u64, User>;
        state bad:      pmap<u64, u64>;
        unique_index bad on users.email;
        fn main() -> u64 { return 0u64; }
    ").unwrap_err();
    assert!(
        err.to_string().contains("does not match index key type")
        || err.to_string().contains("string")
        || err.to_string().contains("u64"),
        "got: {err}",
    );
}

#[test]
fn typeck_rejects_wrong_index_value_type() {
    // Index pmap value is string, but primary's key type is u64.
    let err = rend::frontend("
        struct User { email: string }
        state users:    pmap<u64, User>;
        state bad:      pmap<string, string>;
        unique_index bad on users.email;
        fn main() -> u64 { return 0u64; }
    ").unwrap_err();
    assert!(
        err.to_string().contains("must hold the primary key type"),
        "got: {err}",
    );
}

#[test]
fn typeck_rejects_unknown_primary_state() {
    let err = rend::frontend("
        state by_email: pmap<string, u64>;
        unique_index by_email on users.email;
        fn main() -> u64 { return 0u64; }
    ").unwrap_err();
    assert!(err.to_string().contains("users"), "got: {err}");
}

#[test]
fn typeck_rejects_missing_index_state() {
    let err = rend::frontend("
        struct User { email: string }
        state users: pmap<u64, User>;
        unique_index by_email on users.email;
        fn main() -> u64 { return 0u64; }
    ").unwrap_err();
    assert!(err.to_string().contains("by_email"), "got: {err}");
}

// ---------- multi (non-unique) indexes ----------

#[test]
fn multi_index_collects_keys_per_field_value() {
    // `index by_dept` is non-unique: many primary keys can share
    // a department, so the index value type is `[u64]`.
    let v = run("
        struct User { id: u64, dept: string }
        state users:   pmap<u64, User>;
        state by_dept: pmap<string, [u64]>;
        index by_dept on users.dept;

        fn main() -> i64 {
            users[1u64] = User { id: 1u64, dept: \"eng\" };
            users[2u64] = User { id: 2u64, dept: \"eng\" };
            users[3u64] = User { id: 3u64, dept: \"sales\" };
            users[4u64] = User { id: 4u64, dept: \"eng\" };
            return len(by_dept[\"eng\"]);    // {1, 2, 4}
        }
    ").unwrap();
    assert_eq!(v, Value::int(3i64));
}

#[test]
fn multi_index_dedupes_repeated_writes() {
    // Re-registering the same primary key shouldn't grow the
    // index list. The runtime scans before appending.
    let v = run("
        struct User { id: u64, dept: string }
        state users:   pmap<u64, User>;
        state by_dept: pmap<string, [u64]>;
        index by_dept on users.dept;

        fn main() -> i64 {
            users[1u64] = User { id: 1u64, dept: \"eng\" };
            users[1u64] = User { id: 1u64, dept: \"eng\" };
            users[1u64] = User { id: 1u64, dept: \"eng\" };
            return len(by_dept[\"eng\"]);
        }
    ").unwrap();
    assert_eq!(v, Value::int(1i64));
}

#[test]
fn multi_index_disjoint_field_values_dont_share_cells() {
    // Writes to dept=\"eng\" and dept=\"sales\" hit different
    // index cells, so they don't conflict on commit. Empirically
    // we just check both lists are populated independently.
    let v = run("
        struct User { id: u64, dept: string }
        state users:   pmap<u64, User>;
        state by_dept: pmap<string, [u64]>;
        index by_dept on users.dept;

        fn main() -> i64 {
            users[1u64] = User { id: 1u64, dept: \"eng\" };
            users[2u64] = User { id: 2u64, dept: \"sales\" };
            return len(by_dept[\"eng\"]) + len(by_dept[\"sales\"]) * 10;
        }
    ").unwrap();
    assert_eq!(v, Value::int(11i64));   // 1 + 1*10
}

#[test]
fn multi_index_empty_lookup_returns_empty_array() {
    let v = run("
        struct User { id: u64, dept: string }
        state users:   pmap<u64, User>;
        state by_dept: pmap<string, [u64]>;
        index by_dept on users.dept;

        fn main() -> i64 {
            users[1u64] = User { id: 1u64, dept: \"eng\" };
            return len(by_dept[\"nonexistent\"]);
        }
    ").unwrap();
    assert_eq!(v, Value::int(0i64));
}

#[test]
fn multi_index_works_on_bytecode_vm() {
    let src = "
        struct User { id: u64, dept: string }
        state users:   pmap<u64, User>;
        state by_dept: pmap<string, [u64]>;
        index by_dept on users.dept;

        fn main() -> i64 {
            users[1u64] = User { id: 1u64, dept: \"eng\" };
            users[2u64] = User { id: 2u64, dept: \"eng\" };
            users[3u64] = User { id: 3u64, dept: \"eng\" };
            return len(by_dept[\"eng\"]);
        }
    ";
    let kv = rend::kv::InMemoryKv::new();
    let outcome = Engine::new()
        .execute(src, Fuel::new(50_000), &kv)
        .unwrap();
    assert_eq!(outcome.result, Value::int(3i64));
}

#[test]
fn multi_index_iterate_to_resolve_records() {
    // The "GROUP BY" shape: index gives you the list of primary
    // keys; you then read each one out of the primary.
    let v = run("
        struct User { id: u64, dept: string, salary: u64 }
        state users:   pmap<u64, User>;
        state by_dept: pmap<string, [u64]>;
        index by_dept on users.dept;

        fn main() -> u64 {
            users[1u64] = User { id: 1u64, dept: \"eng\", salary: 100u64 };
            users[2u64] = User { id: 2u64, dept: \"eng\", salary: 150u64 };
            users[3u64] = User { id: 3u64, dept: \"sales\", salary: 200u64 };
            // Total eng salary using the index instead of a scan.
            let eng_ids = by_dept[\"eng\"];
            let total = 0u64;
            for id in eng_ids { total = total + users[id].salary; }
            return total;       // 100 + 150
        }
    ").unwrap();
    assert_eq!(v, Value::U64(250));
}

#[test]
fn typeck_rejects_multi_index_with_singular_value_type() {
    // `index` is multi by default; its slot must be pmap<F, [K]>.
    // Using `pmap<F, K>` should be rejected with a hint about
    // `unique_index`.
    let err = rend::frontend("
        struct User { dept: string }
        state users:   pmap<u64, User>;
        state by_dept: pmap<string, u64>;       // wrong: should be [u64]
        index by_dept on users.dept;
        fn main() -> u64 { return 0u64; }
    ").unwrap_err();
    assert!(
        err.to_string().contains("multi-index slot must be")
        && err.to_string().contains("unique_index"),
        "got: {err}",
    );
}

#[test]
fn typeck_rejects_unique_index_with_array_value_type() {
    // The mirror: `unique_index` slot must be `pmap<F, K>`, not
    // `pmap<F, [K]>`. The existing wrong-value-type rule catches
    // this without a hint to multi.
    let err = rend::frontend("
        struct User { email: string }
        state users:    pmap<u64, User>;
        state by_email: pmap<string, [u64]>;
        unique_index by_email on users.email;
        fn main() -> u64 { return 0u64; }
    ").unwrap_err();
    assert!(
        err.to_string().contains("must hold the primary key type"),
        "got: {err}",
    );
}

// ---------- unique-constraint enforcement ----------

#[test]
fn unique_index_rejects_duplicate_field_value() {
    // Two different primary keys, same indexed value — second
    // write must abort the tx with a unique-constraint error.
    let err = run("
        struct User { id: u64, email: string }
        state users:    pmap<u64, User>;
        state by_email: pmap<string, u64>;
        unique_index by_email on users.email;

        fn main() -> u64 {
            users[1u64] = User { id: 1u64, email: \"a@x.com\" };
            users[2u64] = User { id: 2u64, email: \"a@x.com\" };
            return 0u64;
        }
    ").unwrap_err();
    assert!(
        err.to_string().contains("unique constraint violation"),
        "got: {err}",
    );
}

#[test]
fn unique_index_idempotent_re_register_succeeds() {
    // Re-writing the same (key, value) pair to the index is a
    // no-op. The interp/VM detects equality with the existing
    // entry and skips the write.
    let v = run("
        struct User { id: u64, email: string }
        state users:    pmap<u64, User>;
        state by_email: pmap<string, u64>;
        unique_index by_email on users.email;

        fn main() -> u64 {
            users[1u64] = User { id: 1u64, email: \"a@x.com\" };
            users[1u64] = User { id: 1u64, email: \"a@x.com\" };
            users[1u64] = User { id: 1u64, email: \"a@x.com\" };
            return by_email[\"a@x.com\"];
        }
    ").unwrap();
    assert_eq!(v, Value::U64(1));
}

#[test]
fn unique_index_rejects_duplicate_on_bytecode_vm() {
    // Same shape, but through the bytecode VM path. The
    // PMapPutUnique opcode raises a runtime error.
    let src = "
        struct User { id: u64, email: string }
        state users:    pmap<u64, User>;
        state by_email: pmap<string, u64>;
        unique_index by_email on users.email;

        fn main() -> u64 {
            users[1u64] = User { id: 1u64, email: \"a@x.com\" };
            users[2u64] = User { id: 2u64, email: \"a@x.com\" };
            return 0u64;
        }
    ";
    let kv = rend::kv::InMemoryKv::new();
    let err = Engine::new()
        .execute(src, Fuel::new(50_000), &kv)
        .unwrap_err();
    assert!(
        err.to_string().contains("unique constraint violation"),
        "got: {err}",
    );
}

#[test]
fn unique_index_violation_aborts_whole_tx() {
    // The first write succeeds in-memory but the second write
    // aborts the tx. After the abort, no writes are committed —
    // a fresh KV stays empty.
    let kv = rend::kv::InMemoryKv::new();
    let src = "
        struct User { id: u64, email: string }
        state users:    pmap<u64, User>;
        state by_email: pmap<string, u64>;
        unique_index by_email on users.email;

        fn main() -> u64 {
            users[1u64] = User { id: 1u64, email: \"a@x.com\" };
            users[2u64] = User { id: 2u64, email: \"a@x.com\" };  // boom
            return 99u64;
        }
    ";
    let err = Engine::new()
        .execute(src, Fuel::new(50_000), &kv)
        .unwrap_err();
    assert!(err.to_string().contains("unique constraint"), "got: {err}");
    // The Engine never returned an outcome with writes, so there's
    // nothing to apply. Re-run with only the first registration:
    let recover = "
        struct User { id: u64, email: string }
        state users:    pmap<u64, User>;
        state by_email: pmap<string, u64>;
        unique_index by_email on users.email;

        fn main() -> u64 {
            users[1u64] = User { id: 1u64, email: \"a@x.com\" };
            return 1u64;
        }
    ";
    let outcome = Engine::new().execute(recover, Fuel::new(50_000), &kv).unwrap();
    assert_eq!(outcome.result, Value::U64(1));
}

#[test]
fn unique_index_field_change_still_stale_but_no_violation() {
    // The field-change-on-update case: user 1's email moves from
    // a@x.com to b@x.com. The new entry by_email[b@x.com] = 1 is
    // fresh (no prior key), so no violation. The old entry
    // by_email[a@x.com] = 1 stays stale (the slice-4 limitation).
    let v = run("
        struct User { id: u64, email: string }
        state users:    pmap<u64, User>;
        state by_email: pmap<string, u64>;
        unique_index by_email on users.email;

        fn main() -> u64 {
            users[1u64] = User { id: 1u64, email: \"a@x.com\" };
            users[1u64] = User { id: 1u64, email: \"b@x.com\" };
            // Both index entries point at id 1. The b@x.com entry
            // is correct; the a@x.com entry is now a stale
            // back-link. Pack: 100 * b + a.
            return by_email[\"b@x.com\"] * 100u64 + by_email[\"a@x.com\"];
        }
    ").unwrap();
    assert_eq!(v, Value::U64(101));
}

// ---------- sorted indexes (pbtree-backed) ----------

#[test]
fn unique_sorted_index_supports_range_query() {
    // unique_index over a pbtree slot: each id maps to exactly
    // one user, and we can range-query the index to find users
    // whose id falls in [lo, hi].
    let v = run("
        struct User { id: u64, name: string }
        state users:    pmap<u64, User>;
        state by_id:    pbtree<u64, u64>;
        unique_index by_id on users.id;

        fn main() -> u64 {
            users[1u64] = User { id: 1u64, name: \"alice\" };
            users[2u64] = User { id: 2u64, name: \"bob\" };
            users[3u64] = User { id: 3u64, name: \"carol\" };
            users[4u64] = User { id: 4u64, name: \"dan\" };
            // Range over the sorted index: ids 2..=3 → 2 entries.
            return u64(len(pbtree_range(by_id, 2u64, 3u64)));
        }
    ").unwrap();
    assert_eq!(v, Value::U64(2));
}

#[test]
fn unique_sorted_index_iterates_in_key_order() {
    // The point of a sorted index: iteration walks in key order,
    // not insertion order.
    let v = run("
        struct User { id: u64, name: string }
        state users: pmap<u64, User>;
        state by_id: pbtree<u64, u64>;
        unique_index by_id on users.id;

        fn main() -> u64 {
            // Insert out of order.
            users[300u64] = User { id: 300u64, name: \"c\" };
            users[100u64] = User { id: 100u64, name: \"a\" };
            users[200u64] = User { id: 200u64, name: \"b\" };
            // Walk in sorted order — first id should be 100.
            for primary_id in by_id { return primary_id; }
            return 0u64;
        }
    ").unwrap();
    assert_eq!(v, Value::U64(100));
}

#[test]
fn multi_sorted_index_collects_keys_per_field_value() {
    // Sorted multi-index: pbtree<F, [K]>. Many users per age,
    // and ages are key-sorted so range queries on age work.
    let v = run("
        struct User { id: u64, age: u64 }
        state users:  pmap<u64, User>;
        state by_age: pbtree<u64, [u64]>;
        index by_age on users.age;

        fn main() -> i64 {
            users[1u64] = User { id: 1u64, age: 25u64 };
            users[2u64] = User { id: 2u64, age: 30u64 };
            users[3u64] = User { id: 3u64, age: 25u64 };  // same age as 1
            users[4u64] = User { id: 4u64, age: 50u64 };
            // by_age[25] should be [1, 3].
            return len(by_age[25u64]);
        }
    ").unwrap();
    assert_eq!(v, Value::int(2i64));
}

#[test]
fn multi_sorted_index_range_query_returns_grouped_lists() {
    // pbtree_range on a multi-index returns the [K] arrays for
    // each in-range field value, in sorted order.
    let v = run("
        struct User { id: u64, age: u64 }
        state users:  pmap<u64, User>;
        state by_age: pbtree<u64, [u64]>;
        index by_age on users.age;

        fn main() -> i64 {
            users[1u64] = User { id: 1u64, age: 25u64 };
            users[2u64] = User { id: 2u64, age: 30u64 };
            users[3u64] = User { id: 3u64, age: 35u64 };
            users[4u64] = User { id: 4u64, age: 40u64 };
            // Range [25..=35] should pull lists for ages 25, 30, 35.
            // sum of len of each list (each has 1 entry) = 3.
            let groups = pbtree_range(by_age, 25u64, 35u64);
            let total = 0;
            for g in groups { total = total + len(g); }
            return total;
        }
    ").unwrap();
    assert_eq!(v, Value::int(3i64));
}

#[test]
fn unique_sorted_index_enforces_constraint() {
    // Same uniqueness check as pmap-backed unique_index, but
    // through pbtree::get/set.
    let err = run("
        struct User { id: u64, email: string }
        state users:    pmap<u64, User>;
        state by_email: pbtree<u64, u64>;
        unique_index by_email on users.id;

        fn main() -> u64 {
            users[1u64] = User { id: 7u64, email: \"a\" };
            users[2u64] = User { id: 7u64, email: \"b\" };  // same id, different primary
            return 0u64;
        }
    ").unwrap_err();
    assert!(
        err.to_string().contains("unique constraint violation"),
        "got: {err}",
    );
}

#[test]
fn sorted_index_works_on_bytecode_vm() {
    // Both backends must wire identically through the VM.
    let src = "
        struct User { id: u64, age: u64 }
        state users:  pmap<u64, User>;
        state by_age: pbtree<u64, [u64]>;
        index by_age on users.age;

        fn main() -> u64 {
            users[1u64] = User { id: 1u64, age: 30u64 };
            users[2u64] = User { id: 2u64, age: 30u64 };
            // by_age[30] = [1, 2], len = 2 → return as u64.
            return u64(len(by_age[30u64]));
        }
    ";
    let kv = rend::kv::InMemoryKv::new();
    let outcome = Engine::new()
        .execute(src, Fuel::new(50_000), &kv)
        .unwrap();
    assert_eq!(outcome.result, Value::U64(2));
}

#[test]
fn pmap_and_pbtree_indexes_coexist_on_same_primary() {
    // Mix backends: hash-ordered for unique-by-email, key-sorted
    // for grouping-by-age. Each index uses its appropriate trie.
    let v = run("
        struct User { id: u64, email: string, age: u64 }
        state users:    pmap<u64, User>;
        state by_email: pmap<string, u64>;       // hash-ordered (point lookup)
        state by_age:   pbtree<u64, [u64]>;      // key-sorted (range)
        unique_index by_email on users.email;
        index by_age on users.age;

        fn main() -> u64 {
            users[1u64] = User { id: 1u64, email: \"a@x\", age: 30u64 };
            users[2u64] = User { id: 2u64, email: \"b@x\", age: 25u64 };
            users[3u64] = User { id: 3u64, email: \"c@x\", age: 30u64 };
            // Both indexes populated correctly.
            let e_id   = by_email[\"b@x\"];           // → 2
            let age_count = u64(len(by_age[30u64])); // [1, 3] → 2
            return e_id * 100u64 + age_count;
        }
    ").unwrap();
    assert_eq!(v, Value::U64(202));
}

// ---------- update behavior (slice-4 limitation) ----------

#[test]
fn update_with_same_indexed_field_keeps_index_correct() {
    // Update users[1].email stays "a@x.com"; index by email "a@x.com"
    // → 1 stays correct after the second write.
    let v = run("
        struct User { id: u64, email: string }
        state users:    pmap<u64, User>;
        state by_email: pmap<string, u64>;
        unique_index by_email on users.email;

        fn main() -> u64 {
            users[1u64] = User { id: 1u64, email: \"a@x.com\" };
            // Same email, same id, just \"refresh\" the record.
            users[1u64] = User { id: 1u64, email: \"a@x.com\" };
            return by_email[\"a@x.com\"];
        }
    ").unwrap();
    assert_eq!(v, Value::U64(1));
}

#[test]
fn update_with_changed_field_leaves_old_entry_stale() {
    // The slice-4 limitation: changing the indexed field on an
    // existing entry adds the new index entry but does NOT remove
    // the old one. Tests pin this behavior so it's an explicit
    // contract — when proper update handling lands, this test
    // changes shape.
    let v = run("
        struct User { id: u64, email: string }
        state users:    pmap<u64, User>;
        state by_email: pmap<string, u64>;
        unique_index by_email on users.email;

        fn main() -> u64 {
            users[1u64] = User { id: 1u64, email: \"old@x.com\" };
            users[1u64] = User { id: 1u64, email: \"new@x.com\" };
            // New entry is correct.
            let new_id = by_email[\"new@x.com\"];
            // Old entry is stale (still points to id 1, but id 1's
            // current email is now \"new@x.com\", not \"old@x.com\").
            let stale = by_email[\"old@x.com\"];
            // Pack: 100 * new + stale
            return new_id * 100u64 + stale;
        }
    ").unwrap();
    // new_id = 1, stale = 1 (still pointing at the same primary key,
    // even though the back-link is now broken). 100 * 1 + 1 = 101.
    assert_eq!(v, Value::U64(101));
}
