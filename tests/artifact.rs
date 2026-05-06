//! Compiled binary artifact — persistable bytecode that can ship
//! over the wire and be re-executed without recompiling from
//! source. Tests pin three things:
//!   * Round-trip: encode → decode → re-encode produces identical
//!     bytes (the artifact is a stable content-addressed thing).
//!   * Equivalence: running from artifact gives the same result
//!     as running from source.
//!   * Validation: decode catches corruption / wrong magic /
//!     truncation.

use rend::artifact::Artifact;
use rend::kv::InMemoryKv;
use rend::value::Value;
use rend::{Engine, Fuel};

// ---------- determinism ----------

#[test]
fn compile_is_deterministic() {
    let src = "
        struct Point { x: i64, y: i64 }
        state origin: Point;
        state n: i64;
        entry fn move_x(d: i64) -> i64 {
            origin.x = origin.x + d;
            n = n + 1;
            return origin.x;
        }
        fn main() -> i64 { return move_x(7); }
    ";
    let a = Engine::new().compile(src).unwrap();
    let b = Engine::new().compile(src).unwrap();
    assert_eq!(
        a.bytes, b.bytes,
        "two compilations of the same source must yield byte-identical artifacts",
    );
    assert_eq!(a.content_hash, b.content_hash);
}

#[test]
fn whitespace_changes_dont_change_bytecode() {
    // Lex/parse normalize away whitespace, so two formattings of
    // the same program compile to the same artifact.
    let a = Engine::new().compile("fn main() -> i64 { return 1 + 2; }").unwrap();
    let b = Engine::new().compile(
        "fn main() -> i64 {
            return 1 + 2;
        }",
    ).unwrap();
    assert_eq!(a.bytes, b.bytes);
}

// ---------- round-trip via bytes ----------

#[test]
fn artifact_round_trips_through_bytes() {
    let src = "
        state n: i64;
        entry fn bump() -> i64 { n = n + 1; return n; }
        fn main() -> i64 { bump(); bump(); return bump(); }
    ";
    let a = Engine::new().compile(src).unwrap();
    let bytes = a.bytes.clone();
    let b = Artifact::from_bytes(bytes).unwrap();
    assert_eq!(a.bytes, b.bytes);
    assert_eq!(a.content_hash, b.content_hash);
    assert_eq!(a.modules.len(), b.modules.len());

    // Running the round-tripped artifact produces the same answer
    // as running the original.
    let kv = InMemoryKv::new();
    let out = Engine::new().execute_artifact(&b, "main", Fuel::new(10_000), &kv).unwrap();
    assert_eq!(out.result, Value::Int(3));
}

// ---------- source vs artifact equivalence ----------

#[test]
fn execute_artifact_matches_execute_source_simple() {
    let src = "
        entry fn fib(n: i64) -> i64 {
            if n < 2 { return n; }
            return fib(n - 1) + fib(n - 2);
        }
        fn main() -> i64 { return fib(15); }
    ";
    let from_source = Engine::new().execute(src, Fuel::new(1_000_000), &InMemoryKv::new()).unwrap();
    let artifact = Engine::new().compile(src).unwrap();
    let from_artifact = Engine::new()
        .execute_artifact(&artifact, "main", Fuel::new(1_000_000), &InMemoryKv::new())
        .unwrap();
    assert_eq!(from_source.result, from_artifact.result);
    assert_eq!(from_source.result, Value::Int(610));
}

#[test]
fn execute_artifact_matches_execute_source_with_state() {
    let src = "
        state ledger: map<Address, u64>;
        entry fn deposit(who: Address, amount: u64) -> u64 {
            ledger[who] = ledger[who] + amount;
            return ledger[who];
        }
        fn main() -> u64 {
            let alice = address(\"alice\");
            deposit(alice, 100u64);
            deposit(alice, 50u64);
            return ledger[alice];
        }
    ";
    let kv1 = InMemoryKv::new();
    let from_source = Engine::new().execute(src, Fuel::new(50_000), &kv1).unwrap();

    let kv2 = InMemoryKv::new();
    let artifact = Engine::new().compile(src).unwrap();
    let from_artifact = Engine::new()
        .execute_artifact(&artifact, "main", Fuel::new(50_000), &kv2)
        .unwrap();

    assert_eq!(from_source.result, from_artifact.result);
    assert_eq!(from_source.result, Value::U64(150));
    // The two paths should produce the same write set.
    assert_eq!(from_source.writes, from_artifact.writes);
}

#[test]
fn execute_artifact_with_pmap_pvec() {
    let src = "
        state m: pmap<i64, u64>;
        state v: pvec<i64>;
        fn main() -> u64 {
            m[1] = 100u64;
            m[2] = 200u64;
            pvec_push(v, 7);
            pvec_push(v, 11);
            return m[1] + m[2] + u64(v[0]) + u64(v[1]);
        }
    ";
    let kv1 = InMemoryKv::new();
    let from_source = Engine::new().execute(src, Fuel::new(50_000), &kv1).unwrap();

    let kv2 = InMemoryKv::new();
    let artifact = Engine::new().compile(src).unwrap();
    let from_artifact = Engine::new()
        .execute_artifact(&artifact, "main", Fuel::new(50_000), &kv2)
        .unwrap();

    assert_eq!(from_source.result, from_artifact.result);
    assert_eq!(from_source.result, Value::U64(318));
}

// ---------- multi-module artifacts ----------

#[test]
fn multi_module_artifact_runs() {
    let sources = vec![
        "module ledger;
         state n: i64;
         entry fn add(d: i64) { n = n + d; }
         entry fn read_n() -> i64 { return n; }
        ".to_string(),
        "module main;
         fn main() -> i64 {
             ledger::add(7);
             ledger::add(8);
             return ledger::read_n();
         }
        ".to_string(),
    ];
    let artifact = Engine::new().compile_modules(&sources).unwrap();
    assert_eq!(artifact.modules.len(), 2);
    let kv = InMemoryKv::new();
    let out = Engine::new()
        .execute_artifact(&artifact, "main", Fuel::new(10_000), &kv)
        .unwrap();
    assert_eq!(out.result, Value::Int(15));
}

#[test]
fn multi_module_artifact_round_trips() {
    let sources = vec![
        "module a;
         entry fn ping() -> i64 { return 42; }
        ".to_string(),
        "module main;
         fn main() -> i64 { return a::ping(); }
        ".to_string(),
    ];
    let a = Engine::new().compile_modules(&sources).unwrap();
    let b = Artifact::from_bytes(a.bytes.clone()).unwrap();
    assert_eq!(a.bytes, b.bytes);

    let kv = InMemoryKv::new();
    let out = Engine::new()
        .execute_artifact(&b, "main", Fuel::new(10_000), &kv)
        .unwrap();
    assert_eq!(out.result, Value::Int(42));
}

// ---------- validation ----------

#[test]
fn decode_rejects_invalid_bytes() {
    let bad = vec![0u8; 32];
    let err = Artifact::from_bytes(bad).unwrap_err();
    assert!(err.to_string().contains("magic"));
}

#[test]
fn execute_artifact_rejects_unknown_main_module() {
    let artifact = Engine::new().compile("fn main() -> i64 { return 0; }").unwrap();
    let kv = InMemoryKv::new();
    let err = Engine::new()
        .execute_artifact(&artifact, "not_a_module", Fuel::new(1_000), &kv)
        .unwrap_err();
    assert!(err.to_string().contains("not present"));
}

#[test]
fn host_imports_revalidated_at_execute_time() {
    // The artifact compiles cleanly because we provide a binding.
    // If we then try to run it on a fresh Engine without that
    // binding, execute should refuse — the source isn't available
    // anymore, so we can't blame the user, but we can still fail
    // safely instead of crashing in the VM.
    use rend::host::HostError;
    let mut binding_engine = Engine::new();
    binding_engine.bind("host_log", |args| match args {
        [Value::Int(_)] => Ok(Value::Unit),
        _ => Err(HostError::invalid_args("host_log: expected i64")),
    });
    let src = "
        import host_log: fn(i64);
        fn main() -> i64 {
            host_log(7);
            return 0;
        }
    ";
    let artifact = binding_engine.compile(src).unwrap();

    let kv = InMemoryKv::new();
    let err = Engine::new() // no bindings
        .execute_artifact(&artifact, "main", Fuel::new(10_000), &kv)
        .unwrap_err();
    assert!(err.to_string().contains("host_log"));
}
