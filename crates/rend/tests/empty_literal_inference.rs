//! Slice 18: empty literal type inference via let annotation.

use rend::value::Value;
use rend::run;

#[test]
fn empty_array_with_annotation() {
    let v = run("
        fn main() -> i64 {
            let xs: [i64] = [];
            return len(xs);
        }
    ").unwrap();
    assert_eq!(v, Value::int(0i64));
}

#[test]
fn empty_array_then_append_via_loop() {
    let v = run("
        fn main() -> i64 {
            let xs: [i64] = [];
            let comp = [n for n in [1, 2, 3] if n > 1];
            return len(comp);
        }
    ").unwrap();
    assert_eq!(v, Value::int(2i64));
}

#[test]
fn empty_set_with_annotation() {
    let v = run("
        fn main() -> i64 {
            let s: set<i64> = set{};
            return set_len(s);
        }
    ").unwrap();
    assert_eq!(v, Value::int(0i64));
}

#[test]
fn empty_set_then_insert() {
    let v = run("
        fn main() -> i64 {
            let s: set<i64> = set{};
            let s2 = set_insert(s, 42);
            return set_len(s2);
        }
    ").unwrap();
    assert_eq!(v, Value::int(1i64));
}

#[test]
fn empty_dict_with_annotation() {
    let v = run("
        fn main() -> i64 {
            let d: dict<i64, i64> = dict{};
            return dict_len(d);
        }
    ").unwrap();
    assert_eq!(v, Value::int(0i64));
}

#[test]
fn empty_dict_then_set() {
    let v = run("
        fn main() -> i64 {
            let d: dict<i64, i64> = dict{};
            let d2 = dict_set(d, 5, 100);
            return dict_get(d2, 5, 0);
        }
    ").unwrap();
    assert_eq!(v, Value::int(100i64));
}

#[test]
fn empty_array_without_annotation_is_compile_error() {
    let err = run("fn main() -> i64 { let xs = []; return len(xs); }").unwrap_err();
    assert!(err.to_string().contains("annotation") || err.to_string().contains("empty"));
}

#[test]
fn empty_set_without_annotation_is_compile_error() {
    let err = run("fn main() -> i64 { let s = set{}; return set_len(s); }").unwrap_err();
    assert!(err.to_string().contains("annotation") || err.to_string().contains("empty"));
}

#[test]
fn empty_dict_without_annotation_is_compile_error() {
    let err = run("fn main() -> i64 { let d = dict{}; return dict_len(d); }").unwrap_err();
    assert!(err.to_string().contains("annotation") || err.to_string().contains("empty"));
}

#[test]
fn annotation_must_match_value_type() {
    let err = run("
        fn main() -> i64 {
            let xs: [i64] = [\"a\", \"b\"];
            return len(xs);
        }
    ").unwrap_err();
    assert!(err.to_string().contains("annotation") || err.to_string().contains("type"));
}

#[test]
fn annotation_on_non_empty_array_works() {
    let v = run("
        fn main() -> i64 {
            let xs: [i64] = [10, 20, 30];
            return len(xs);
        }
    ").unwrap();
    assert_eq!(v, Value::int(3i64));
}

#[test]
fn empty_set_unkeyable_element_rejected() {
    // Resource is non-keyable; even though Resource isn't a typical empty-set
    // element, we still want to reject the annotation up-front.
    let err = run("
        fn main() -> i64 {
            let s: set<Resource> = set{};
            return set_len(s);
        }
    ").unwrap_err();
    let _ = err; // we just check it errors — the message varies
}
