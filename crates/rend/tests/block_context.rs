//! `msg_sender()`, `block_timestamp()`, `block_number()` — host-
//! supplied per-tx context. Read-only, zero-arg builtins. Default
//! values are zero-address / 0 / 0; the host should populate the
//! authenticated sender + chain numbers via `execute_with_context`.

use rend::tx::TxContext;
use rend::value::Value;
use rend::{Engine, Fuel};

#[test]
fn msg_sender_returns_default_zero_address_when_no_context() {
    let kv = rend::kv::InMemoryKv::new();
    let src = r#"
        fn main() -> Address { return msg_sender(); }
    "#;
    let out = Engine::new().execute(src, Fuel::new(1000), &kv).unwrap();
    assert_eq!(out.result, Value::Address(String::new()));
}

#[test]
fn msg_sender_threads_through_host_context() {
    let kv = rend::kv::InMemoryKv::new();
    let src = "fn main() -> Address { return msg_sender(); }";
    let ctx = TxContext {
        sender: Value::Address("0xa11ce".into()),
        block_timestamp: 0,
        block_number: 0,
    };
    let out = Engine::new()
        .execute_with_context(src, ctx, Fuel::new(1000), &kv)
        .unwrap();
    assert_eq!(out.result, Value::Address("0xa11ce".into()));
}

#[test]
fn block_timestamp_and_number_threaded_through_context() {
    let kv = rend::kv::InMemoryKv::new();
    let src = "
        fn main() -> u64 {
            return block_timestamp() + block_number();
        }
    ";
    let ctx = TxContext {
        sender: Value::Address(String::new()),
        block_timestamp: 1_700_000_000,
        block_number: 42,
    };
    let out = Engine::new()
        .execute_with_context(src, ctx, Fuel::new(1000), &kv)
        .unwrap();
    assert_eq!(out.result, Value::U64(1_700_000_042));
}

#[test]
fn msg_sender_powers_only_owner_pattern() {
    // Canonical use case: an authorization gate using the sender.
    let kv = rend::kv::InMemoryKv::new();
    let src = r#"
        state owner: Address;
        entry fn init(o: Address) { owner = o; }
        entry fn admin_op() -> i64 {
            assert(msg_sender() == owner, "not owner");
            return 1;
        }
        fn main() -> i64 {
            init(address("0xowner"));
            return admin_op();
        }
    "#;
    let ctx = TxContext {
        sender: Value::Address("0xowner".into()),
        ..Default::default()
    };
    let out = Engine::new()
        .execute_with_context(src, ctx, Fuel::new(10_000), &kv)
        .unwrap();
    assert_eq!(out.result, Value::int(1i64));
}

#[test]
fn only_owner_rejects_wrong_sender() {
    let kv = rend::kv::InMemoryKv::new();
    let src = r#"
        state owner: Address;
        entry fn init(o: Address) { owner = o; }
        entry fn admin_op() -> i64 {
            assert(msg_sender() == owner, "not owner");
            return 1;
        }
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
    assert!(err.to_string().contains("not owner"), "got: {err}");
}

#[test]
fn context_reads_dont_break_static_read_clusters() {
    // msg_sender() / block_timestamp() / block_number() are pure;
    // independent state reads on either side should still cluster.
    let kv = rend::kv::InMemoryKv::new();
    let src = "
        state a: i64;
        state b: i64;
        fn main() -> i64 {
            let x = a;
            let _s = msg_sender();
            let _t = block_timestamp();
            let y = b;
            return x + y;
        }
    ";
    let out = Engine::new().execute(src, Fuel::new(10_000), &kv).unwrap();
    assert_eq!(out.result, Value::int(0i64));
}

#[test]
fn context_args_are_compile_error() {
    let kv = rend::kv::InMemoryKv::new();
    let err = Engine::new()
        .execute(
            "fn main() -> Address { return msg_sender(0); }",
            Fuel::new(1000),
            &kv,
        )
        .unwrap_err();
    assert!(err.to_string().contains("no arguments"));
}

#[test]
fn context_visible_across_module_boundary() {
    let kv = rend::kv::InMemoryKv::new();
    let main_src = r#"
        module main;
        fn main() -> bool {
            return auth::is_admin();
        }
    "#;
    let auth_src = r#"
        module auth;
        state admin: Address;
        entry fn init(a: Address) { admin = a; }
        entry fn is_admin() -> bool {
            return msg_sender() == admin;
        }
    "#;
    // Setup the admin slot in a separate, "deploy-time" tx so the
    // production tx really only checks identity.
    let mut kv_setup = rend::kv::InMemoryKv::new();
    let setup_main = r#"
        module main;
        fn main() -> i64 {
            auth::init(address("0xadmin"));
            return 0;
        }
    "#;
    let setup_out = Engine::new()
        .execute_modules(
            &[setup_main.to_string(), auth_src.to_string()],
            "main",
            Fuel::new(10_000),
            &kv_setup,
        )
        .unwrap();
    kv_setup.apply(&setup_out.writes);

    let ctx = TxContext {
        sender: Value::Address("0xadmin".into()),
        ..Default::default()
    };
    let out = Engine::new()
        .execute_main_with_context(
            &[("main".to_string(), main_src.to_string()),
              ("auth".to_string(), auth_src.to_string())]
                .into_iter()
                .collect(),
            "main",
            ctx,
            Fuel::new(10_000),
            &kv_setup,
        )
        .unwrap();
    assert_eq!(out.result, Value::Bool(true));
    let _ = kv;
}
