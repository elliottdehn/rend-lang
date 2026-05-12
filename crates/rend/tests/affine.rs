//! Slice 3: affine ownership tests.

use rend::{run, Value};

fn assert_runs_to(src: &str, expected: Value) {
    match run(src) {
        Ok(v) => assert_eq!(v, expected, "src:\n{src}"),
        Err(e) => panic!("unexpected error in:\n{src}\n→ {e}"),
    }
}

fn assert_errors(src: &str, contains: &str) {
    match run(src) {
        Ok(v) => panic!("expected error, got {v}; src:\n{src}"),
        Err(e) => assert!(
            e.to_string().contains(contains),
            "error '{e}' did not contain '{contains}'",
        ),
    }
}

#[test]
fn resource_round_trip_unwraps_to_inner_int() {
    let src = "fn main() -> i64 { return unwrap(resource(42)); }";
    assert_runs_to(src, Value::int(42i64));
}

#[test]
fn moved_resource_consumed_once_works() {
    let src = "entry fn use_it(r: Resource) -> i64 { return unwrap(r); }
               fn main() -> i64 {
                   let r = resource(7);
                   return use_it(r);
               }";
    assert_runs_to(src, Value::int(7i64));
}

#[test]
fn double_unwrap_is_use_after_move() {
    let src = "fn main() -> i64 {
        let r = resource(5);
        let a = unwrap(r);
        let b = unwrap(r);
        return a + b;
    }";
    assert_errors(src, "used after move");
}

#[test]
fn passing_then_using_is_use_after_move() {
    let src = "entry fn use_it(r: Resource) -> i64 { return unwrap(r); }
               fn main() -> i64 {
                   let r = resource(5);
                   let _ = use_it(r);
                   return unwrap(r);
               }";
    assert_errors(src, "used after move");
}

#[test]
fn move_through_let_alias_still_consumes() {
    let src = "fn main() -> i64 {
        let r = resource(3);
        let r2 = r;
        return unwrap(r2);
    }";
    assert_runs_to(src, Value::int(3i64));
}

#[test]
fn copy_types_can_be_used_freely() {
    let src = "fn main() -> i64 {
        let x = 5;
        let y = x + x;       // x used twice — Copy, no move
        return y;
    }";
    assert_runs_to(src, Value::int(10i64));
}

#[test]
fn move_in_one_branch_poisons_after_join() {
    // r is moved in the then-branch but not the else-branch.
    // After the if, the merge says "potentially moved" → using r is an error.
    let src = "fn main() -> i64 {
        let r = resource(1);
        if true { let _ = unwrap(r); } else {}
        return unwrap(r);
    }";
    assert_errors(src, "used after move");
}

#[test]
fn no_use_after_move_when_neither_branch_moves() {
    let src = "fn main() -> i64 {
        let r = resource(9);
        if true {} else {}
        return unwrap(r);
    }";
    assert_runs_to(src, Value::int(9i64));
}
