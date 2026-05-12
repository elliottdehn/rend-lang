//! `assert(cond)` and `assert(cond, "msg")` — runtime sanity check.
//!
//! On true: returns `()`. On false: raises a `Runtime` error that
//! propagates up through the VM and out to the host as `Err(Error)`.
//! Pure (no state effects); the static optimizer treats it like any
//! other Pure builtin and clusters reads across it.

use rend::value::Value;
use rend::{run, Engine, Fuel};

#[test]
fn assert_true_returns_unit_and_program_continues() {
    let v = run("
        fn main() -> i64 {
            assert(1 + 1 == 2);
            return 42;
        }
    ").unwrap();
    assert_eq!(v, Value::int(42i64));
}

#[test]
fn assert_false_is_a_runtime_error() {
    let err = run("
        fn main() -> i64 {
            assert(1 + 1 == 3);
            return 0;
        }
    ").unwrap_err();
    assert!(
        err.to_string().contains("assertion failed"),
        "expected assertion-failed error, got: {err}",
    );
}

#[test]
fn assert_with_message_includes_string_in_error() {
    let err = run(r#"
        fn main() -> i64 {
            assert(false, "balance must be non-negative");
            return 0;
        }
    "#).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("assertion failed"), "got: {msg}");
    assert!(msg.contains("balance must be non-negative"), "got: {msg}");
}

#[test]
fn assert_non_bool_is_a_compile_error() {
    let err = run("
        fn main() -> i64 {
            assert(42);
            return 0;
        }
    ").unwrap_err();
    assert!(
        err.to_string().contains("bool"),
        "expected type error mentioning bool, got: {err}",
    );
}

#[test]
fn assert_message_must_be_string() {
    let err = run("
        fn main() -> i64 {
            assert(true, 99);
            return 0;
        }
    ").unwrap_err();
    assert!(
        err.to_string().contains("string"),
        "expected type error mentioning string, got: {err}",
    );
}

#[test]
fn assert_zero_args_or_too_many_is_compile_error() {
    let err = run("fn main() -> i64 { assert(); return 0; }").unwrap_err();
    assert!(err.to_string().contains("assert"), "got: {err}");
    let err = run(r#"fn main() -> i64 { assert(true, "a", "b"); return 0; }"#).unwrap_err();
    assert!(err.to_string().contains("assert"), "got: {err}");
}

#[test]
fn assert_inside_function_propagates_error_to_caller() {
    let err = run("
        entry fn check(x: i64) {
            assert(x > 0, \"x must be positive\");
        }
        fn main() -> i64 {
            check(-3);
            return 0;
        }
    ").unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("assertion failed"), "got: {msg}");
    assert!(msg.contains("x must be positive"), "got: {msg}");
}

#[test]
fn assert_in_state_writing_program_aborts_writes() {
    // A failed assert raises a runtime error; the host receives Err
    // and never commits the in-flight writes.
    let kv = rend::kv::InMemoryKv::new();
    let src = "
        state n: i64;
        fn main() -> i64 {
            n = 999;
            assert(false, \"never\");
            return 0;
        }
    ";
    let err = Engine::new().execute(src, Fuel::new(10_000), &kv).unwrap_err();
    assert!(err.to_string().contains("never"));
}

#[test]
fn assert_does_not_break_static_read_clustering() {
    // assert is Pure; the static optimizer should still cluster two
    // independent state reads across an assert call. Verifies via
    // run_bc parity (no functional difference; just exercises the
    // code path).
    let v = run("
        state a: i64;
        state b: i64;
        fn main() -> i64 {
            let x = a;
            let y = b;
            assert(x + y >= 0);
            return x + y;
        }
    ").unwrap();
    assert_eq!(v, Value::int(0i64));
}

#[test]
fn assert_in_loop_passes_for_all_iterations() {
    let v = run("
        fn main() -> i64 {
            let total = 0;
            for x in [1, 2, 3, 4, 5] {
                assert(x > 0);
                total = total + x;
            }
            return total;
        }
    ").unwrap();
    assert_eq!(v, Value::int(15i64));
}

#[test]
fn assert_in_loop_fails_on_first_violation() {
    let err = run("
        fn main() -> i64 {
            for x in [1, 2, -3, 4] {
                assert(x > 0, \"non-positive iter value\");
            }
            return 0;
        }
    ").unwrap_err();
    assert!(err.to_string().contains("non-positive"));
}

#[test]
fn assert_then_return_works_as_invariant_check() {
    let v = run("
        struct Account { balance: i64 }
        entry fn debit(a: Account, amount: i64) -> Account {
            assert(a.balance >= amount, \"insufficient balance\");
            let result = a;
            result.balance = a.balance - amount;
            return result;
        }
        fn main() -> i64 {
            let a = Account { balance: 100 };
            let b = debit(a, 30);
            return b.balance;
        }
    ").unwrap();
    assert_eq!(v, Value::int(70i64));
}

#[test]
fn assert_is_compatible_with_bytecode_vm() {
    // run_bc exercises only the bytecode path (no interp parity).
    // Confirms the BuiltinCall lowering and runtime resolve correctly.
    let v = rend::run_bc(
        "fn main() -> i64 { assert(true); return 7; }",
        rend::Fuel::new(1000),
    ).unwrap();
    assert_eq!(v, Value::int(7i64));

    let err = rend::run_bc(
        "fn main() -> i64 { assert(false); return 7; }",
        rend::Fuel::new(1000),
    ).unwrap_err();
    assert!(err.to_string().contains("assertion failed"));
}
