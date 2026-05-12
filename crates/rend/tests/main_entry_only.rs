//! `main` is the host's view of the module — a transaction submitting a
//! `main` body is conceptually a foreign caller, so the typeck enforces
//! that `main` may only call `entry` functions of any module. Internal
//! helpers must be marked `entry` to be reachable from `main`. Cross-module
//! entry-only restriction is enforced separately at the link step.

use rend::value::Value;
use rend::run;

#[test]
fn main_calling_non_entry_helper_is_rejected() {
    let err = run("
        fn helper() -> i64 { return 42; }
        fn main() -> i64 { return helper(); }
    ").unwrap_err();
    assert!(
        err.to_string().contains("entry"),
        "expected entry-rule error, got: {err}",
    );
}

#[test]
fn main_calling_entry_helper_is_allowed() {
    let v = run("
        entry fn helper() -> i64 { return 42; }
        fn main() -> i64 { return helper(); }
    ").unwrap();
    assert_eq!(v, Value::int(42i64));
}

#[test]
fn entry_can_call_non_entry_helpers() {
    // Inside an entry function (or any non-main function), normal call
    // rules apply: a private helper is freely callable.
    let v = run("
        fn private_add(a: i64, b: i64) -> i64 { return a + b; }
        entry fn public_doubler(n: i64) -> i64 { return private_add(n, n); }
        fn main() -> i64 { return public_doubler(7); }
    ").unwrap();
    assert_eq!(v, Value::int(14i64));
}

#[test]
fn deep_internal_call_chain_is_fine_under_entry_root() {
    let v = run("
        fn level3(n: i64) -> i64 { return n + 1; }
        fn level2(n: i64) -> i64 { return level3(n) + 1; }
        fn level1(n: i64) -> i64 { return level2(n) + 1; }
        entry fn surface(n: i64) -> i64 { return level1(n); }
        fn main() -> i64 { return surface(0); }
    ").unwrap();
    assert_eq!(v, Value::int(3i64));
}

#[test]
fn main_calling_builtin_is_always_fine() {
    // Builtins (resource/unwrap/address/len/set_*/dict_*/i32/u32/...) are
    // not subject to the entry rule.
    let v = run("
        fn main() -> i64 { return len([1, 2, 3]); }
    ").unwrap();
    assert_eq!(v, Value::int(3i64));
}

#[test]
fn main_calling_host_import_is_always_fine() {
    // Host imports cross the host boundary and are always callable from
    // main, regardless of the entry rule.
    let mut engine = rend::Engine::new();
    engine.bind("triple", |args| match args {
        [Value::Int(n)] => Ok(Value::Int(n * 3)),
        _ => Err(rend::host::HostError::new(0, "triple expects i64")),
    });
    let kv = rend::kv::InMemoryKv::new();
    let src = "
        import triple: fn(i64) -> i64;
        fn main() -> i64 { return triple(7); }
    ";
    let out = engine.execute(src, rend::Fuel::new(10_000), &kv).unwrap();
    assert_eq!(out.result, Value::int(21i64));
}

#[test]
fn error_message_names_the_offender() {
    let err = run("
        fn private_thing() -> i64 { return 1; }
        fn main() -> i64 { return private_thing(); }
    ").unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("private_thing"), "msg should name the function: {msg}");
    assert!(msg.contains("entry") || msg.contains("private"));
}
