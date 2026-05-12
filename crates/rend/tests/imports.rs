//! Slice 5+9: host imports with typed `HostError`.

use std::sync::{Arc, Mutex};

use rend::host::HostError;
use rend::{Engine, Fuel, Value};

#[test]
fn imported_pure_function_returns_to_guest() {
    let mut engine = Engine::new();
    engine.bind("triple", |args| match args {
        [Value::Int(n)] => Ok(Value::Int(n * 3)),
        _ => Err(HostError::invalid_args("triple: expected single i64")),
    });
    let src = "
        import triple: fn(i64) -> i64;
        fn main() -> i64 { return triple(7); }
    ";
    let r = engine.run(src, Fuel::new(1000)).unwrap();
    assert_eq!(r, Value::int(21i64));
}

#[test]
fn missing_host_impl_is_link_time_error() {
    let engine = Engine::new();
    let src = "
        import host_log: fn(i64);
        fn main() -> i64 { host_log(1); return 0; }
    ";
    let err = engine.run(src, Fuel::new(1000)).unwrap_err();
    assert!(err.to_string().contains("no host impl"), "got {err}");
}

#[test]
fn host_side_effects_are_observable() {
    let captured: Arc<Mutex<Vec<i64>>> = Arc::new(Mutex::new(Vec::new()));
    let captured2 = Arc::clone(&captured);

    let mut engine = Engine::new();
    engine.bind("log_int", move |args| {
        if let [Value::Int(n)] = args {
            captured2.lock().unwrap().push(
                num_traits::ToPrimitive::to_i64(n).expect("fits"),
            );
            Ok(Value::Unit)
        } else {
            Err(HostError::invalid_args("log_int: expected single int"))
        }
    });

    let src = "
        import log_int: fn(i64);
        fn main() -> i64 {
            log_int(10);
            log_int(20);
            log_int(30);
            return 0;
        }
    ";
    engine.run(src, Fuel::new(1000)).unwrap();
    assert_eq!(*captured.lock().unwrap(), vec![10, 20, 30]);
}

#[test]
fn host_can_propagate_typed_errors() {
    let mut engine = Engine::new();
    engine.bind("explode", |_| Err::<Value, _>(HostError::aborted("kaboom")));
    let src = "
        import explode: fn() -> i64;
        fn main() -> i64 { return explode(); }
    ";
    let err = engine.run(src, Fuel::new(1000)).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("explode"));
    assert!(msg.contains("kaboom"));
    assert!(msg.contains("code 2"), "expected error-code propagation; got {msg}");
}

#[test]
fn import_arg_type_mismatch_is_compile_error() {
    let mut engine = Engine::new();
    engine.bind("logn", |_| Ok(Value::Unit));
    let src = "
        import logn: fn(i64);
        fn main() -> i64 { logn(true); return 0; }
    ";
    let err = engine.run(src, Fuel::new(1000)).unwrap_err();
    assert!(err.to_string().contains("expected int"), "got {err}");
}

#[test]
fn import_can_be_called_from_recursion() {
    let count: Arc<Mutex<u32>> = Arc::new(Mutex::new(0));
    let count2 = Arc::clone(&count);

    let mut engine = Engine::new();
    engine.bind("touch", move |_| {
        *count2.lock().unwrap() += 1;
        Ok(Value::Unit)
    });

    let src = "
        import touch: fn();
        entry fn rec(n: i64) -> i64 {
            touch();
            if n <= 0 { return 0; }
            return rec(n - 1);
        }
        fn main() -> i64 { return rec(5); }
    ";
    engine.run(src, Fuel::new(10_000)).unwrap();
    assert_eq!(*count.lock().unwrap(), 6);
}

#[test]
fn imports_consume_resource_args() {
    let mut engine = Engine::new();
    engine.bind("eat", |args| match args {
        [Value::Resource(_)] => Ok(Value::Unit),
        _ => Err(HostError::invalid_args("eat: expected Resource")),
    });

    let bad = "
        import eat: fn(Resource);
        fn main() -> i64 {
            let r = resource(1);
            eat(r);
            eat(r);
            return 0;
        }
    ";
    let err = engine.run(bad, Fuel::new(1000)).unwrap_err();
    assert!(err.to_string().contains("used after move"), "got {err}");
}
