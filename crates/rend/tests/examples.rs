//! Smoke tests for the example bed.
//!
//! Examples in `examples/` come in three flavors:
//!   01–04, 06–08         — slices 1–8, no host imports, runnable today
//!   05                   — needs an Engine with `bind`s
//!   09, 10, 11, 16       — slices 9, 10a, 10b, runnable today (state KV)
//!   12_multi_module/     — slice 11 sketch (multi-module), parse-only stub
//!   13–15                — slice 12+ sketches (placeholders today)

use std::sync::{Arc, Mutex};
use rend::host::HostError;
use rend::kv::InMemoryKv;
use rend::{run, Engine, Fuel, Value};

const EXAMPLES_DIR: &str = "examples";

fn read(name: &str) -> String {
    let path = std::path::Path::new(EXAMPLES_DIR).join(name);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

#[test]
fn example_01_arithmetic_runs_to_fib_20() {
    assert_eq!(run(&read("01_arithmetic.rd")).unwrap(), Value::int(6765i64));
}

#[test]
fn example_02_typed_runs() {
    assert_eq!(run(&read("02_typed.rd")).unwrap(), Value::int(42i64));
}

#[test]
fn examples_without_imports_run_today() {
    for name in [
        "03_affine.rd",
        "04_fuel.rd",
        "06_persistent_counter.rd",
        "07_token.rd",
        "08_concurrent_voting.rd",
        "13_native_jit.rd",
        "14_integer_types.rd",
        "15_mutable_fields.rd",
    ] {
        let src = read(name);
        run(&src).unwrap_or_else(|e| panic!("{name}: {e}"));
    }
}

#[test]
fn example_05_runs_via_engine_with_host_impls() {
    let log: Arc<Mutex<Vec<i64>>> = Arc::new(Mutex::new(Vec::new()));
    let log_for_closure = Arc::clone(&log);

    let mut engine = Engine::new();
    engine.bind("host_log", move |args| {
        if let [Value::Int(n)] = args {
            log_for_closure.lock().unwrap().push(
                num_traits::ToPrimitive::to_i64(n).expect("fits"),
            );
            Ok(Value::Unit)
        } else {
            Err(HostError::invalid_args("host_log: expected single int"))
        }
    });
    engine.bind("host_double", |args| match args {
        [Value::Int(n)] => Ok(Value::int(n * 2)),
        _ => Err(HostError::invalid_args("host_double: expected single i64")),
    });

    let result = engine.run(&read("05_host_imports.rd"), Fuel::new(10_000)).unwrap();
    assert_eq!(result, Value::int(42i64));
    assert_eq!(*log.lock().unwrap(), vec![42]);
}

#[test]
fn example_09_strings_runs_with_state() {
    let kv = InMemoryKv::new();
    let out = Engine::new()
        .execute(&read("09_strings.rd"), Fuel::new(10_000), &kv)
        .unwrap();
    assert_eq!(out.result, Value::Str("Alice in Wonderland".into()));
}

#[test]
fn example_10_arrays_runs_with_state() {
    let kv = InMemoryKv::new();
    let out = Engine::new()
        .execute(&read("10_arrays.rd"), Fuel::new(50_000), &kv)
        .unwrap();
    assert_eq!(out.result, Value::int(150i64)); // 10+20+30+40+50
}

#[test]
fn example_11_structs_runs_with_state() {
    let kv = InMemoryKv::new();
    let out = Engine::new()
        .execute(&read("11_structs.rd"), Fuel::new(10_000), &kv)
        .unwrap();
    assert_eq!(out.result, Value::int(120i64));
}

#[test]
fn example_16_auction_runs_with_state() {
    let kv = InMemoryKv::new();
    let out = Engine::new()
        .execute(&read("16_auction.rd"), Fuel::new(10_000), &kv)
        .unwrap();
    assert_eq!(out.result, Value::int(150i64));
}
