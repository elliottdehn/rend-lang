//! Block-as-expression and if-as-expression. The trailing item in a
//! `{ ... }` block, if it doesn't end with `;`, is the block's value.
//! Function bodies use that as an implicit return; let-RHS / match-arm
//! / if-arm contexts use it to compute a value from a multi-statement
//! body. `if cond { ... } else { ... }` is now an expression too.

use rend::value::Value;
use rend::run;

#[test]
fn fn_body_implicit_return_via_tail_expression() {
    let v = run("
        fn main() -> i64 {
            let x = 7;
            x * 6
        }
    ").unwrap();
    assert_eq!(v, Value::int(42i64));
}

#[test]
fn block_as_let_rhs() {
    let v = run("
        fn main() -> i64 {
            let n = {
                let a = 10;
                let b = 20;
                a + b
            };
            return n;
        }
    ").unwrap();
    assert_eq!(v, Value::int(30i64));
}

#[test]
fn if_as_expression_picks_then_arm() {
    let v = run("
        fn main() -> i64 {
            let x = if true { 11 } else { 22 };
            return x;
        }
    ").unwrap();
    assert_eq!(v, Value::int(11i64));
}

#[test]
fn if_as_expression_picks_else_arm() {
    let v = run("
        fn main() -> i64 {
            let x = if false { 11 } else { 22 };
            return x;
        }
    ").unwrap();
    assert_eq!(v, Value::int(22i64));
}

#[test]
fn if_as_expression_with_multi_stmt_arms() {
    let v = run("
        fn main() -> i64 {
            let n = 5;
            let result = if n > 0 {
                let doubled = n * 2;
                doubled + 1
            } else {
                let neg = -n;
                neg + 1
            };
            return result;
        }
    ").unwrap();
    assert_eq!(v, Value::int(11i64));
}

#[test]
fn else_if_chain_as_expression() {
    let v = run("
        fn main() -> i64 {
            let n = 3;
            let label = if n == 0 {
                100
            } else if n == 1 {
                101
            } else if n == 3 {
                103
            } else {
                999
            };
            return label;
        }
    ").unwrap();
    assert_eq!(v, Value::int(103i64));
}

#[test]
fn match_arm_with_multi_stmt_block_body() {
    let v = run("
        enum Op { Add(i64, i64), Mul(i64, i64) }
        fn main() -> i64 {
            let o = Op::Mul(6, 7);
            return match o {
                Op::Add(a, b) => a + b,
                Op::Mul(a, b) => {
                    let prod = a * b;
                    prod + 0
                },
            };
        }
    ").unwrap();
    assert_eq!(v, Value::int(42i64));
}

#[test]
fn if_branches_must_agree_in_type() {
    let err = run("
        fn main() -> i64 {
            let x = if true { 1 } else { false };
            return x;
        }
    ").unwrap_err();
    assert!(err.to_string().contains("agree"), "got: {err}");
}

#[test]
fn if_as_expression_requires_else() {
    let err = run("
        fn main() -> i64 {
            let x = if true { 1 };
            return x;
        }
    ").unwrap_err();
    assert!(err.to_string().contains("else"), "got: {err}");
}

#[test]
fn fn_body_implicit_return_type_must_match_decl() {
    let err = run("
        fn main() -> i64 {
            true
        }
    ").unwrap_err();
    assert!(err.to_string().contains("implicit return") || err.to_string().contains("expected i64"));
}

#[test]
fn nested_block_expressions() {
    let v = run("
        fn main() -> i64 {
            let n = {
                let x = {
                    let y = 5;
                    y + 1
                };
                x * 2
            };
            return n;
        }
    ").unwrap();
    assert_eq!(v, Value::int(12i64));
}

#[test]
fn block_with_no_tail_is_unit() {
    // A block with all statements terminated by `;` evaluates to
    // Unit, which can be the value of a `()` -typed binding (or the
    // `let` form of side-effecting work).
    let v = run("
        state n: i64;
        fn main() -> i64 {
            // Side-effecting block; its tail-less value is Unit.
            { n = 1; n = n + 5; };
            return n;
        }
    ").unwrap();
    assert_eq!(v, Value::int(6i64));
}

#[test]
fn fn_with_explicit_return_still_works() {
    // Explicit `return expr;` continues to work; tail expressions
    // don't override existing return semantics.
    let v = run("
        fn main() -> i64 {
            return 99;
        }
    ").unwrap();
    assert_eq!(v, Value::int(99i64));
}

#[test]
fn match_arm_block_body_can_compute_via_local() {
    let v = run("
        enum Outcome { Win(u64), Loss }
        fn main() -> u64 {
            let o = Outcome::Win(50u64);
            return match o {
                Outcome::Win(prize) => {
                    let bonus = prize + 10u64;
                    bonus + 5u64
                },
                Outcome::Loss => 0u64,
            };
        }
    ").unwrap();
    assert_eq!(v, Value::U64(65));
}

#[test]
fn if_expr_used_as_argument() {
    let v = run("
        entry fn double(n: i64) -> i64 { return n * 2; }
        fn main() -> i64 {
            let cond = true;
            return double(if cond { 5 } else { -5 });
        }
    ").unwrap();
    assert_eq!(v, Value::int(10i64));
}

#[test]
fn block_expr_inside_tuple_literal() {
    let v = run("
        entry fn pair() -> (i64, i64) {
            return (
                { let a = 1; a + 1 },
                { let b = 10; b + 1 },
            );
        }
        fn main() -> i64 {
            let (a, b) = pair();
            return a + b;
        }
    ").unwrap();
    assert_eq!(v, Value::int(13i64));
}
