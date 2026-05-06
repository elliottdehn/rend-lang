// SLICE 16 (working): mutable struct field paths.
//
// `obj.field = expr;` works directly. The lowering reads `obj`, FieldSets the
// target field, and writes the modified copy back to `obj`. Nested paths like
// `game.player.position.x = 5;` recurse: walk down to read each level, set
// the leaf, then walk back up writing the modified copy at each level.
//
// Same shape works for state struct slots — `c.n = c.n + 1;` lowers to a
// single KvGet → FieldSet → KvPut, leaving the read/write set fine-grained
// (the entire struct is read and written, but other states are untouched).

struct Counter { n: i64 }
state c: Counter;

entry fn incr() {
    c.n = c.n + 1;     // read c, FieldSet n, write c — done
}

fn main() -> i64 {
    incr();
    incr();
    incr();
    return c.n;        // 3
}
