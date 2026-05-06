// SLICE 10b (working): structs — decl, literal, field access, storage, keys.
//
// A leaderboard recording the highest score seen. Demonstrates:
//   - struct decl at module level
//   - struct literal with named fields (in any order)
//   - field access via `.name`
//   - struct values stored directly in state (`state high_score: Score;`)
//   - reading a struct's fields as part of an expression

struct Score {
    player: Address,
    points: i64,
}

state high_score: Score;

entry fn record(p: Address, pts: i64) {
    if pts > high_score.points {
        high_score = Score { player: p, points: pts };
    }
}

fn main() -> i64 {
    record(address("0xa11ce"),  100);
    record(address("0xb0b"),     95);
    record(address("0xcaro1"),  120);
    return high_score.points;     // 120
}
