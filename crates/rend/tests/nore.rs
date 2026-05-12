//! `nore` (non-reentrant) — the canonical guard against call-back
//! attacks. The runtime tracks `(module, fn)` pairs currently on the
//! call stack under `nore`; re-entry aborts the tx.
//!
//! Mirrors Solidity's `nonReentrant` modifier semantics: re-entry
//! within the same call-chain is rejected, but two non-overlapping
//! invocations in the same tx are fine.

use rend::value::Value;
use rend::{run, Engine, Fuel};

#[test]
fn direct_recursion_into_nore_fn_is_rejected() {
    let err = run("
        nore entry fn loops(n: i64) -> i64 {
            if n == 0 { return 0; }
            return loops(n - 1);
        }
        fn main() -> i64 { return loops(3); }
    ").unwrap_err();
    assert!(err.to_string().contains("re-entry"), "got: {err}");
}

#[test]
fn non_recursive_call_to_nore_fn_works() {
    // Calling a nore fn straight-line (no re-entry) succeeds.
    let v = run("
        nore entry fn pure_op() -> i64 { return 42; }
        fn main() -> i64 { return pure_op(); }
    ").unwrap();
    assert_eq!(v, Value::int(42i64));
}

#[test]
fn sequential_invocations_of_nore_fn_each_succeed() {
    // Two non-overlapping invocations in the same tx — each enters,
    // exits, and the slot is freed for the next.
    let v = run("
        state n: i64;
        nore entry fn bump() -> i64 {
            n = n + 1;
            return n;
        }
        fn main() -> i64 {
            bump();
            bump();
            bump();
            return n;
        }
    ").unwrap();
    assert_eq!(v, Value::int(3i64));
}

#[test]
fn nore_releases_slot_after_normal_return() {
    // A nore fn that calls *another* nore fn (different name) and
    // returns successfully — both slots free up afterwards, so a
    // later call to either should work.
    let v = run("
        nore entry fn inner() -> i64 { return 1; }
        nore entry fn outer() -> i64 { return inner() + 10; }
        fn main() -> i64 {
            let a = outer();
            let b = inner();
            let c = outer();
            return a + b + c;     // 11 + 1 + 11 = 23
        }
    ").unwrap();
    assert_eq!(v, Value::int(23i64));
}

#[test]
fn cross_module_callback_into_nore_fn_is_rejected() {
    // The classic Solidity reentrancy shape: module A.transfer is
    // nore. transfer calls module B.notify, which calls back A.transfer.
    // Without nore, the inner transfer would observe stale state.
    // With nore, the re-entry aborts the whole tx.
    let kv = rend::kv::InMemoryKv::new();
    let bank_src = "
        module bank;
        state balance: u64;
        entry fn init(b: u64) { balance = b; }
        nore entry fn transfer(amount: u64) -> bool {
            assert(balance >= amount, \"insufficient\");
            attacker::callback(amount);     // nasty external call
            balance = balance - amount;
            return true;
        }
    ";
    let attacker_src = "
        module attacker;
        entry fn callback(amount: u64) {
            // Try to drain the bank by calling transfer again before
            // the outer transfer's `balance = balance - amount`.
            bank::transfer(amount);
        }
    ";
    let main_src = "
        module main;
        fn main() -> bool {
            bank::init(100u64);
            return bank::transfer(50u64);
        }
    ";
    let err = Engine::new()
        .execute_modules(
            &[main_src.to_string(), bank_src.to_string(), attacker_src.to_string()],
            "main",
            Fuel::new(50_000),
            &kv,
        )
        .unwrap_err();
    assert!(err.to_string().contains("re-entry"), "got: {err}");
}

#[test]
fn nore_only_guards_the_specific_function_not_others_in_module() {
    // Two nore fns in the same module are independent — re-entering
    // `a` while inside `a` is illegal, but calling `b` from inside
    // `a` is fine.
    let v = run("
        nore entry fn a() -> i64 { return b() + 1; }
        nore entry fn b() -> i64 { return 10; }
        fn main() -> i64 { return a(); }
    ").unwrap();
    assert_eq!(v, Value::int(11i64));
}

#[test]
fn non_nore_recursion_still_works() {
    // Recursion into a non-nore function is unaffected.
    let v = run("
        entry fn fib(n: i64) -> i64 {
            if n < 2 { return n; }
            return fib(n - 1) + fib(n - 2);
        }
        fn main() -> i64 { return fib(7); }
    ").unwrap();
    assert_eq!(v, Value::int(13i64));
}

#[test]
fn nore_can_combine_with_entry_in_either_order() {
    // `nore entry fn` and `entry nore fn` both parse + behave the same.
    let v1 = run("
        nore entry fn op() -> i64 { return 1; }
        fn main() -> i64 { return op(); }
    ").unwrap();
    let v2 = run("
        entry nore fn op() -> i64 { return 1; }
        fn main() -> i64 { return op(); }
    ").unwrap();
    assert_eq!(v1, Value::int(1i64));
    assert_eq!(v2, Value::int(1i64));
}
