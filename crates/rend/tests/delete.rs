//! `delete state[k];` — remove an entry from a pmap or pbtree
//! state. Index back-links are auto-cleaned: unique-index entries
//! get removed (only if they still point at the deleted primary
//! key), multi-index entries get filtered out of the list.

use rend::value::Value;
use rend::{run, Engine, Fuel};

// ---------- pmap delete ----------

#[test]
fn delete_pmap_entry_returns_default_on_lookup() {
    let v = run("
        state ledger: pmap<u64, u64>;
        fn main() -> u64 {
            ledger[1u64] = 100u64;
            ledger[2u64] = 200u64;
            delete ledger[1u64];
            return ledger[1u64];
        }
    ").unwrap();
    assert_eq!(v, Value::U64(0));
}

#[test]
fn delete_pmap_makes_contains_false() {
    let v = run("
        state ledger: pmap<u64, u64>;
        fn main() -> bool {
            ledger[1u64] = 0u64;
            let before = pmap_contains(ledger, 1u64);
            delete ledger[1u64];
            let after = pmap_contains(ledger, 1u64);
            return before && !after;
        }
    ").unwrap();
    assert_eq!(v, Value::Bool(true));
}

#[test]
fn delete_pmap_missing_key_is_no_op() {
    let v = run("
        state ledger: pmap<u64, u64>;
        fn main() -> u64 {
            ledger[1u64] = 100u64;
            delete ledger[99u64];
            return ledger[1u64];
        }
    ").unwrap();
    assert_eq!(v, Value::U64(100));
}

// ---------- pbtree delete ----------

#[test]
fn delete_pbtree_entry_returns_default() {
    let v = run("
        state h: pbtree<u64, u64>;
        fn main() -> u64 {
            h[10u64] = 100u64;
            h[20u64] = 200u64;
            h[30u64] = 300u64;
            delete h[20u64];
            return h[20u64];
        }
    ").unwrap();
    assert_eq!(v, Value::U64(0));
}

#[test]
fn delete_pbtree_preserves_sorted_iteration() {
    // After deleting from the middle, iteration still yields the
    // remaining entries in sorted order.
    let v = run("
        state h: pbtree<u64, u64>;
        fn main() -> u64 {
            h[10u64] = 1u64;
            h[20u64] = 2u64;
            h[30u64] = 3u64;
            h[40u64] = 4u64;
            delete h[20u64];
            // Sum positionally: 1*1000 + 3*100 + 4*10 + 0 = 1340
            // (only 3 entries remain so the 4th iter doesn't fire).
            let acc = 0u64;
            let pos = 1000u64;
            for v in h {
                acc = acc + v * pos;
                pos = pos / 10u64;
            }
            return acc;
        }
    ").unwrap();
    assert_eq!(v, Value::U64(1340));
}

#[test]
fn delete_pbtree_after_many_inserts_still_correct() {
    // 100 inserts force at least one split; deleting half of them
    // exercises the propagation/collapse logic. Lookups for
    // remaining keys still work; deleted keys read as default.
    let v = run("
        state h: pbtree<u64, u64>;
        fn main() -> u64 {
            let i = 1u64;
            while i <= 100u64 {
                h[i] = i;
                i = i + 1u64;
            }
            // Delete every even key.
            i = 2u64;
            while i <= 100u64 {
                delete h[i];
                i = i + 2u64;
            }
            // Sum the remaining (odd) keys via streaming for-in.
            let acc = 0u64;
            for v in h { acc = acc + v; }
            return acc;
            // 1 + 3 + 5 + ... + 99 = 2500
        }
    ").unwrap();
    assert_eq!(v, Value::U64(2500));
}

// ---------- index back-link cleanup ----------

#[test]
fn delete_clears_unique_index_back_link() {
    let v = run("
        struct User { id: u64, email: string }
        state users: pmap<u64, User>;
        state by_email: pmap<string, u64>;
        unique_index by_email on users.email;

        fn main() -> u64 {
            users[1u64] = User { id: 1u64, email: \"a@x.com\" };
            users[2u64] = User { id: 2u64, email: \"b@x.com\" };
            delete users[1u64];
            // by_email[\"a@x.com\"] should be cleared (not pointing at 1).
            // Default for u64 is 0 — read returns 0.
            let stale_lookup = by_email[\"a@x.com\"];
            let live_lookup  = by_email[\"b@x.com\"];
            return stale_lookup * 100u64 + live_lookup;
            // 0 * 100 + 2 = 2
        }
    ").unwrap();
    assert_eq!(v, Value::U64(2));
}

#[test]
fn delete_filters_multi_index_list() {
    let v = run("
        struct User { id: u64, dept: string }
        state users: pmap<u64, User>;
        state by_dept: pmap<string, [u64]>;
        index by_dept on users.dept;

        fn main() -> i64 {
            users[1u64] = User { id: 1u64, dept: \"eng\" };
            users[2u64] = User { id: 2u64, dept: \"eng\" };
            users[3u64] = User { id: 3u64, dept: \"eng\" };
            // Before: by_dept[\"eng\"] = [1, 2, 3]
            delete users[2u64];
            // After: by_dept[\"eng\"] = [1, 3]
            return len(by_dept[\"eng\"]);
        }
    ").unwrap();
    assert_eq!(v, Value::int(2i64));
}

#[test]
fn delete_last_in_multi_index_list_removes_entry() {
    // When the last user in a dept is deleted, by_dept[dept] should
    // be removed entirely (so contains returns false).
    let v = run("
        struct User { id: u64, dept: string }
        state users: pmap<u64, User>;
        state by_dept: pmap<string, [u64]>;
        index by_dept on users.dept;

        fn main() -> bool {
            users[1u64] = User { id: 1u64, dept: \"eng\" };
            let before = pmap_contains(by_dept, \"eng\");
            delete users[1u64];
            let after = pmap_contains(by_dept, \"eng\");
            return before && !after;
        }
    ").unwrap();
    assert_eq!(v, Value::Bool(true));
}

#[test]
fn delete_clears_pbtree_unique_index() {
    let v = run("
        struct User { id: u64, email: string }
        state users: pmap<u64, User>;
        state by_id:  pbtree<u64, u64>;
        unique_index by_id on users.id;

        fn main() -> u64 {
            users[1u64] = User { id: 100u64, email: \"a\" };
            users[2u64] = User { id: 200u64, email: \"b\" };
            delete users[1u64];
            // by_id range should now contain only 200.
            return u64(len(pbtree_range(by_id, 0u64, 1000u64)));
        }
    ").unwrap();
    assert_eq!(v, Value::U64(1));
}

#[test]
fn delete_filters_pbtree_multi_index() {
    let v = run("
        struct User { id: u64, age: u64 }
        state users: pmap<u64, User>;
        state by_age: pbtree<u64, [u64]>;
        index by_age on users.age;

        fn main() -> i64 {
            users[1u64] = User { id: 1u64, age: 30u64 };
            users[2u64] = User { id: 2u64, age: 30u64 };
            delete users[1u64];
            return len(by_age[30u64]);
        }
    ").unwrap();
    assert_eq!(v, Value::int(1i64));
}

// ---------- bytecode VM parity ----------

#[test]
fn delete_works_on_bytecode_vm() {
    let src = "
        struct User { id: u64, email: string }
        state users: pmap<u64, User>;
        state by_email: pmap<string, u64>;
        unique_index by_email on users.email;

        fn main() -> u64 {
            users[1u64] = User { id: 1u64, email: \"a@x.com\" };
            delete users[1u64];
            // by_email back-link should be cleared.
            return by_email[\"a@x.com\"];
        }
    ";
    let kv = rend::kv::InMemoryKv::new();
    let outcome = Engine::new()
        .execute(src, Fuel::new(50_000), &kv)
        .unwrap();
    assert_eq!(outcome.result, Value::U64(0));
}

// ---------- typeck rejection ----------

#[test]
fn delete_on_non_pmap_state_is_compile_error() {
    let err = rend::frontend("
        state xs: [i64];
        fn main() -> u64 {
            delete xs[0];
            return 0u64;
        }
    ").unwrap_err();
    assert!(
        err.to_string().contains("delete"),
        "got: {err}",
    );
}

#[test]
fn delete_with_wrong_key_type_is_compile_error() {
    let err = rend::frontend("
        state ledger: pmap<u64, u64>;
        fn main() -> u64 {
            delete ledger[\"oops\"];
            return 0u64;
        }
    ").unwrap_err();
    assert!(err.to_string().contains("u64"), "got: {err}");
}

#[test]
fn delete_in_view_fn_is_compile_error() {
    // delete is a write — view fns can't do it.
    let err = rend::frontend("
        state ledger: pmap<u64, u64>;
        view fn main() -> u64 {
            delete ledger[1u64];
            return 0u64;
        }
    ").unwrap_err();
    assert!(
        err.to_string().contains("view"),
        "got: {err}",
    );
}
