//! Slice 13b: while + for-in + mutable Copy locals + break/continue.

use rend::value::Value;
use rend::{run, Engine, Fuel};

#[test]
fn while_loop_factorial() {
    let v = run("
        fn main() -> i64 {
            let acc = 1;
            let i = 1;
            while i <= 6 {
                acc = acc * i;
                i = i + 1;
            }
            return acc;
        }
    ").unwrap();
    assert_eq!(v, Value::Int(720));
}

#[test]
fn while_loop_with_zero_iterations() {
    let v = run("
        fn main() -> i64 {
            let n = 0;
            while false { n = 99; }
            return n;
        }
    ").unwrap();
    assert_eq!(v, Value::Int(0));
}

#[test]
fn for_in_sums_array() {
    let v = run("
        fn main() -> i64 {
            let total = 0;
            for x in [3, 1, 4, 1, 5, 9, 2, 6] {
                total = total + x;
            }
            return total;
        }
    ").unwrap();
    assert_eq!(v, Value::Int(31));
}

#[test]
fn for_in_with_break() {
    let v = run("
        fn main() -> i64 {
            let first = -1;
            for x in [-1, -2, 7, 3] {
                if x > 0 { first = x; break; }
            }
            return first;
        }
    ").unwrap();
    assert_eq!(v, Value::Int(7));
}

#[test]
fn while_with_continue_skips_iteration() {
    let v = run("
        fn main() -> i64 {
            let total = 0;
            let i = 0;
            while i < 10 {
                i = i + 1;
                if i == 5 { continue; }
                total = total + i;
            }
            return total;
        }
    ").unwrap();
    // 1+2+3+4+ (skip 5) +6+7+8+9+10 = 50
    assert_eq!(v, Value::Int(50));
}

#[test]
fn nested_loops_inner_break_doesnt_escape_outer() {
    let v = run("
        fn main() -> i64 {
            let outer = 0;
            for x in [1, 2, 3] {
                for y in [10, 20, 30] {
                    if y == 20 { break; }
                    outer = outer + y;   // 10 each iteration of x
                }
            }
            return outer;
        }
    ").unwrap();
    // outer iters: x=1,2,3 → each adds 10 once before inner break
    assert_eq!(v, Value::Int(30));
}

#[test]
fn break_outside_loop_is_compile_error() {
    let err = run("fn main() -> i64 { break; return 0; }").unwrap_err();
    assert!(err.to_string().contains("break"), "got {err}");
}

#[test]
fn continue_outside_loop_is_compile_error() {
    let err = run("fn main() -> i64 { continue; return 0; }").unwrap_err();
    assert!(err.to_string().contains("continue"), "got {err}");
}

#[test]
fn while_cond_must_be_bool() {
    let err = run("fn main() -> i64 { while 1 { return 0; } return 1; }").unwrap_err();
    assert!(err.to_string().contains("bool"), "got {err}");
}

#[test]
fn for_iter_must_be_array() {
    let err = run("fn main() -> i64 { for x in 5 { return x; } return 0; }").unwrap_err();
    assert!(err.to_string().contains("array"), "got {err}");
}

#[test]
fn local_mutable_counter_is_observable_outside_loop() {
    let v = run("
        fn main() -> i64 {
            let count = 0;
            for x in [10, 20, 30] {
                count = count + 1;
            }
            return count;
        }
    ").unwrap();
    assert_eq!(v, Value::Int(3));
}

#[test]
fn while_with_early_return() {
    let v = run("
        entry fn first_pos(xs: [i64]) -> i64 {
            let i = 0;
            while i < len(xs) {
                if xs[i] > 0 { return xs[i]; }
                i = i + 1;
            }
            return -1;
        }
        fn main() -> i64 { return first_pos([-3, -1, 4, 9]); }
    ").unwrap();
    assert_eq!(v, Value::Int(4));
}

#[test]
fn example_18_loops_runs() {
    let kv = rend::kv::InMemoryKv::new();
    let src = std::fs::read_to_string("examples/18_loops.rd").unwrap();
    let out = Engine::new().execute(&src, Fuel::new(50_000), &kv).unwrap();
    assert_eq!(out.result, Value::Int(749));
}
