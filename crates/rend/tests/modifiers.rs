//! Solidity-style modifiers: declared at module level with
//! `modifier Name(params) { ... _; ... }`, applied to functions via
//! `fn foo(args) [Mod1(margs), Mod2] -> R { ... }`. The body is
//! desugared at AST-level (before typeck) — outermost-leftmost
//! wraps innermost, with `_;` substituted by the next layer.

use rend::tx::TxContext;
use rend::value::Value;
use rend::{run, Engine, Fuel};

#[test]
fn simple_modifier_runs_before_and_after_body() {
    // `_;` is desugared into the wrapped body inline. Pre-stmts run,
    // then body, then post-stmts. Note: an explicit `return` in the
    // body short-circuits past the post-stmts (matches Solidity); the
    // body here drops through implicitly so all three layers fire.
    let v = run("
        state count: i64;
        modifier Tally() {
            count = count + 100;
            _;
            count = count + 1;
        }
        entry fn op() [Tally] {
            count = count + 1000;
        }
        fn main() -> i64 {
            op();
            return count;
        }
    ").unwrap();
    assert_eq!(v, Value::int(1101i64));
}

#[test]
fn modifier_with_arg_binds_param_textually() {
    let kv = rend::kv::InMemoryKv::new();
    let src = r#"
        state owner: Address;
        modifier OnlyAt(target: Address) {
            assert(msg_sender() == target, "wrong sender");
            _;
        }
        entry fn init(o: Address) { owner = o; }
        entry fn admin_op() [OnlyAt(owner)] -> i64 { return 99; }
        fn main() -> i64 {
            init(address("0xa11ce"));
            return admin_op();
        }
    "#;
    let ctx = TxContext {
        sender: Value::Address("0xa11ce".into()),
        ..Default::default()
    };
    let out = Engine::new()
        .execute_with_context(src, ctx, Fuel::new(10_000), &kv)
        .unwrap();
    assert_eq!(out.result, Value::int(99i64));
}

#[test]
fn modifier_arg_check_can_fail_with_assert() {
    let kv = rend::kv::InMemoryKv::new();
    let src = r#"
        state owner: Address;
        modifier OnlyAt(target: Address) {
            assert(msg_sender() == target, "wrong sender");
            _;
        }
        entry fn init(o: Address) { owner = o; }
        entry fn admin_op() [OnlyAt(owner)] -> i64 { return 99; }
        fn main() -> i64 {
            init(address("0xowner"));
            return admin_op();
        }
    "#;
    let ctx = TxContext {
        sender: Value::Address("0xattacker".into()),
        ..Default::default()
    };
    let err = Engine::new()
        .execute_with_context(src, ctx, Fuel::new(10_000), &kv)
        .unwrap_err();
    assert!(err.to_string().contains("wrong sender"), "got: {err}");
}

#[test]
fn multiple_modifiers_compose_left_to_right() {
    // [A, B] means A wraps B wraps body. So order of side effects is
    //   A-pre  →  B-pre  →  body  →  B-post  →  A-post
    let v = run("
        state log: [i64];
        modifier A() {
            log = [1];
            _;
            log = [log[0], log[1], log[2], log[3], 5];
        }
        modifier B() {
            log = [log[0], 2];
            _;
            log = [log[0], log[1], log[2], 4];
        }
        entry fn op() [A, B] {
            log = [log[0], log[1], 3];
        }
        fn main() -> i64 {
            op();
            return len(log);
        }
    ").unwrap();
    // Final log: [1, 2, 3, 4, 5] → length 5.
    assert_eq!(v, Value::int(5i64));
}

#[test]
fn modifier_without_placeholder_is_compile_error() {
    let err = run("
        modifier Bad() {
            let _x = 1;
        }
        fn main() [Bad] -> i64 { return 0; }
    ").unwrap_err();
    assert!(err.to_string().contains("placeholder"), "got: {err}");
}

#[test]
fn modifier_arg_count_mismatch_is_compile_error() {
    let err = run("
        modifier Need(n: i64) {
            assert(n > 0);
            _;
        }
        fn main() [Need] -> i64 { return 0; }
    ").unwrap_err();
    assert!(err.to_string().contains("expects"), "got: {err}");
}

#[test]
fn unknown_modifier_is_compile_error() {
    let err = run("
        fn main() [Ghost] -> i64 { return 0; }
    ").unwrap_err();
    assert!(err.to_string().contains("Ghost"), "got: {err}");
}

#[test]
fn placeholder_outside_modifier_is_compile_error() {
    let err = run("
        fn main() -> i64 {
            _;
            return 0;
        }
    ").unwrap_err();
    assert!(err.to_string().contains("modifier"), "got: {err}");
}

#[test]
fn modifier_can_short_circuit_via_assert() {
    let err = run("
        modifier Reject() {
            assert(false, \"always reject\");
            _;
        }
        entry fn op() [Reject] -> i64 { return 1; }
        fn main() -> i64 { return op(); }
    ").unwrap_err();
    assert!(err.to_string().contains("always reject"));
}

#[test]
fn modifier_emits_event_around_body() {
    let kv = rend::kv::InMemoryKv::new();
    let src = "
        state result: i64;
        struct Entered { name: string }
        struct Exited  { name: string }
        modifier Trace() {
            emit Entered { name: \"op\" };
            _;
            emit Exited { name: \"op\" };
        }
        entry fn op() [Trace] { result = 7; }
        fn main() -> i64 {
            op();
            return result;
        }
    ";
    let out = Engine::new().execute(src, Fuel::new(10_000), &kv).unwrap();
    assert_eq!(out.result, Value::int(7i64));
    let names: Vec<&str> = out.events.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, vec!["Entered", "Exited"]);
}

#[test]
fn nore_and_modifier_compose() {
    // `nore` is the lightweight built-in attribute; modifiers are
    // user-defined sugar. They stack.
    let v = run("
        state n: i64;
        modifier Bump() {
            n = n + 10;
            _;
            n = n + 100;
        }
        nore entry fn op() [Bump] {
            n = n + 1000;
        }
        fn main() -> i64 {
            op();
            return n;
        }
    ").unwrap();
    assert_eq!(v, Value::int(1110i64));
}
