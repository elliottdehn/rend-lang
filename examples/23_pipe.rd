// SLICE 13d (working): pipe notation `|>` with `$$` placeholder.
//
// Each `|>` evaluates its left side, binds the result to `$$`, and then
// evaluates its right side. The whole expression's value is the result of
// the last stage. `$$` may appear anywhere in a stage — first arg, last
// arg, multiple times, inside arithmetic, inside indexing — so the pattern
// is much more flexible than Elixir-style "first arg" piping.
//
//   (head)
//   |> stage1($$)
//   |> stage2($$, ...) - stage3($$)
//   |> arr[$$]
//   |> stage4(..., $$, ...) + stage5(..., $$, ...)
//
// Each `|>` is left-associative; chains parse as
//   ((head |> stage1) |> stage2) |> stage3 ...
// so each stage sees only the immediately-preceding stage's result.

entry fn double(n: i64) -> i64 { return n * 2; }
entry fn square(n: i64) -> i64 { return n * n; }
entry fn add(a: i64, b: i64) -> i64 { return a + b; }

fn main() -> i64 {
    return (10)
        |> double($$)             // 20
        |> square($$)             // 400
        |> add($$, 1)             // 401
        |> ($$ - 1) * 2;          // (401-1)*2 = 800
}
