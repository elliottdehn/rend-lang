//! Solidity-style event emission. Modules declare events with
//! `event Name(typed_params);` at top level and emit them inside
//! function bodies with `emit Name(args);`. Emissions accumulate in
//! an in-memory append-only log returned in `ExecOutcome::events`,
//! tagged with the module name that issued the emit.

use rend::tx::EmittedEvent;
use rend::value::Value;
use rend::{Engine, Fuel};

#[test]
fn emit_appears_in_outcome_log() {
    let kv = rend::kv::InMemoryKv::new();
    let src = "
        event Hello(n: i64);
        fn main() -> i64 {
            emit Hello(42);
            return 0;
        }
    ";
    let out = Engine::new().execute(src, Fuel::new(1000), &kv).unwrap();
    assert_eq!(out.events.len(), 1);
    assert_eq!(out.events[0].name, "Hello");
    assert_eq!(out.events[0].module, "main");
    assert_eq!(out.events[0].args, vec![Value::int(42i64)]);
}

#[test]
fn multi_arg_event_carries_all_values() {
    let kv = rend::kv::InMemoryKv::new();
    let src = r#"
        event Transfer(from: Address, to: Address, amount: u64);
        fn main() -> i64 {
            emit Transfer(address("0xa"), address("0xb"), 100u64);
            return 0;
        }
    "#;
    let out = Engine::new().execute(src, Fuel::new(1000), &kv).unwrap();
    assert_eq!(out.events.len(), 1);
    assert_eq!(out.events[0].name, "Transfer");
    assert_eq!(
        out.events[0].args,
        vec![
            Value::Address("0xa".into()),
            Value::Address("0xb".into()),
            Value::U64(100),
        ],
    );
}

#[test]
fn emits_appear_in_emission_order() {
    let kv = rend::kv::InMemoryKv::new();
    let src = "
        event Tick(n: i64);
        fn main() -> i64 {
            for n in [1, 2, 3, 4, 5] {
                emit Tick(n);
            }
            return 0;
        }
    ";
    let out = Engine::new().execute(src, Fuel::new(10_000), &kv).unwrap();
    let ns: Vec<i64> = out
        .events
        .iter()
        .map(|e| match &e.args[0] {
            Value::Int(n) => num_traits::ToPrimitive::to_i64(n).expect("fits"),
            _ => panic!(),
        })
        .collect();
    assert_eq!(ns, vec![1, 2, 3, 4, 5]);
}

#[test]
fn undeclared_event_is_compile_error() {
    let kv = rend::kv::InMemoryKv::new();
    let src = "
        fn main() -> i64 {
            emit Ghost(7);
            return 0;
        }
    ";
    let err = Engine::new().execute(src, Fuel::new(1000), &kv).unwrap_err();
    assert!(
        err.to_string().contains("Ghost"),
        "expected error mentioning Ghost; got: {err}",
    );
}

#[test]
fn arg_type_mismatch_is_compile_error() {
    let kv = rend::kv::InMemoryKv::new();
    let src = "
        event Pinged(n: i64);
        fn main() -> i64 {
            emit Pinged(true);
            return 0;
        }
    ";
    let err = Engine::new().execute(src, Fuel::new(1000), &kv).unwrap_err();
    assert!(
        err.to_string().contains("expected int"),
        "expected type-mismatch error; got: {err}",
    );
}

#[test]
fn arg_count_mismatch_is_compile_error() {
    let kv = rend::kv::InMemoryKv::new();
    let src = "
        event Two(a: i64, b: i64);
        fn main() -> i64 {
            emit Two(1);
            return 0;
        }
    ";
    let err = Engine::new().execute(src, Fuel::new(1000), &kv).unwrap_err();
    assert!(err.to_string().contains("declares") || err.to_string().contains("provides"));
}

#[test]
fn duplicate_event_decl_is_compile_error() {
    let kv = rend::kv::InMemoryKv::new();
    let src = "
        event Foo(n: i64);
        event Foo(n: u64);
        fn main() -> i64 { return 0; }
    ";
    let err = Engine::new().execute(src, Fuel::new(1000), &kv).unwrap_err();
    assert!(err.to_string().contains("duplicate"), "got: {err}");
}

#[test]
fn emits_dont_touch_read_or_write_set() {
    // Events are pure observable output — they don't show up in the
    // OCC reads/writes, so two transactions emitting the same event
    // never conflict at commit time.
    let kv = rend::kv::InMemoryKv::new();
    let src = "
        event Ping(n: i64);
        fn main() -> i64 {
            emit Ping(1);
            emit Ping(2);
            return 0;
        }
    ";
    let out = Engine::new().execute(src, Fuel::new(1000), &kv).unwrap();
    assert_eq!(out.events.len(), 2);
    assert!(out.reads.is_empty(), "no reads expected: {:?}", out.reads);
    assert!(out.writes.is_empty(), "no writes expected: {:?}", out.writes);
}

#[test]
fn assert_failure_drops_emitted_log_along_with_writes() {
    // A runtime error returns Err to the host; the partially-built
    // event log goes nowhere because the host never receives an
    // ExecOutcome to consume.
    let kv = rend::kv::InMemoryKv::new();
    let src = "
        event Step(n: i64);
        fn main() -> i64 {
            emit Step(1);
            assert(false);
            emit Step(2);
            return 0;
        }
    ";
    let err = Engine::new().execute(src, Fuel::new(1000), &kv).unwrap_err();
    assert!(err.to_string().contains("assertion failed"));
}

#[test]
fn cross_module_emit_is_tagged_with_emitting_module() {
    let kv = rend::kv::InMemoryKv::new();
    let main_src = "
        module main;
        fn main() -> i64 {
            ledger::transfer(1, 2, 100);
            return 0;
        }
    ";
    let ledger_src = "
        module ledger;
        event Transfer(from: i64, to: i64, amount: i64);
        state balances: map<i64, i64>;
        entry fn transfer(from: i64, to: i64, amount: i64) -> bool {
            balances[from] = balances[from] - amount;
            balances[to]   = balances[to] + amount;
            emit Transfer(from, to, amount);
            return true;
        }
    ";
    let out = Engine::new()
        .execute_modules(
            &[main_src.to_string(), ledger_src.to_string()],
            "main",
            Fuel::new(10_000),
            &kv,
        )
        .unwrap();
    assert_eq!(out.events.len(), 1);
    assert_eq!(out.events[0].module, "ledger", "emit should be tagged with the *emitting* module");
    assert_eq!(out.events[0].name, "Transfer");
    assert_eq!(
        out.events[0].args,
        vec![Value::int(1i64), Value::int(2i64), Value::int(100i64)],
    );
}

#[test]
fn cross_module_emit_only_visible_in_owning_module() {
    // module main can't `emit Transfer(...);` because Transfer is
    // declared in module ledger, not main.
    let kv = rend::kv::InMemoryKv::new();
    let main_src = "
        module main;
        fn main() -> i64 {
            emit Transfer(1, 2, 100);
            return 0;
        }
    ";
    let ledger_src = "
        module ledger;
        event Transfer(from: i64, to: i64, amount: i64);
        entry fn ping() {}
    ";
    let err = Engine::new()
        .execute_modules(
            &[main_src.to_string(), ledger_src.to_string()],
            "main",
            Fuel::new(1000),
            &kv,
        )
        .unwrap_err();
    assert!(err.to_string().contains("Transfer"), "got: {err}");
}

#[test]
fn struct_typed_event_arg_works() {
    let kv = rend::kv::InMemoryKv::new();
    let src = "
        struct Point { x: i64, y: i64 }
        event Placed(at: Point);
        fn main() -> i64 {
            emit Placed(Point { x: 3, y: 4 });
            return 0;
        }
    ";
    let out = Engine::new().execute(src, Fuel::new(1000), &kv).unwrap();
    assert_eq!(out.events.len(), 1);
    let Value::Struct { name, fields } = &out.events[0].args[0] else {
        panic!("expected struct, got {:?}", out.events[0].args[0]);
    };
    assert_eq!(name, "Point");
    assert_eq!(fields, &vec![
        ("x".into(), Value::int(3i64)),
        ("y".into(), Value::int(4i64)),
    ]);
}

#[test]
fn event_arg_can_carry_lazy_state_value() {
    // The VM's Emit instruction forces every arg, so even if the
    // value entered as Pending, the host always sees a concrete
    // value in the log.
    let mut kv = rend::kv::InMemoryKv::new();
    let setup = "
        state n: i64;
        fn main() -> i64 {
            n = 99;
            return 0;
        }
    ";
    let setup_out = Engine::new().execute(setup, Fuel::new(1000), &kv).unwrap();
    kv.apply(&setup_out.writes);

    let src = "
        event Snapshot(n: i64);
        state n: i64;
        fn main() -> i64 {
            emit Snapshot(n);
            return 0;
        }
    ";
    let out = Engine::new().execute(src, Fuel::new(1000), &kv).unwrap();
    assert_eq!(out.events.len(), 1);
    assert_eq!(out.events[0].args, vec![Value::int(99i64)]);
}

#[test]
fn emitted_event_record_shape_is_what_host_expects() {
    let kv = rend::kv::InMemoryKv::new();
    let src = r#"
        event Mint(to: Address, amount: u64);
        fn main() -> i64 {
            emit Mint(address("alice"), 1000u64);
            return 0;
        }
    "#;
    let out = Engine::new().execute(src, Fuel::new(1000), &kv).unwrap();
    let expected = EmittedEvent {
        module: "main".into(),
        name: "Mint".into(),
        args: vec![Value::Address("alice".into()), Value::U64(1000)],
    };
    assert_eq!(out.events, vec![expected]);
}
