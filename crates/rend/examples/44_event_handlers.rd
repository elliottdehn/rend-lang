// Event handlers — `on T fn h(e: T) { ... }`.
//
// `emit StructExpr;` logs an event AND fires every handler bound
// to that struct type. Handlers are ordinary fns with one
// parameter (the event struct), declared at module top level.
// The scheduler runs all matching handlers in parallel against
// the pre-emit state, merges deltas in stable declaration order,
// and re-runs any handler whose read set overlaps a prior merged
// write (intra-tx OCC).
//
// Pattern: a single write site emits one event; multiple
// independent "observers" maintain derived state (indexes,
// counters, history) without the write site knowing about them.
// Adding a new aggregator is a new `on Foo fn ...` decl — no
// touch to the existing code path. The compiler-generated
// `index NAME on X.field` declaration is the same shape; this
// example shows the pattern as user-visible code.

module game;

struct ScoreSubmitted { player: Address, score: u64 }

// Primary record: each player's latest score.
state scores:        pmap<Address, u64>;
// Derived: sorted by score for "top N" queries. The pbtree
// gives O(log32 N) range queries; we maintain it via a handler.
state by_score:      pbtree<u64, Address>;
// Derived: total submissions across all players.
state total_games:   u64;
// Derived: which players submitted at least once.
state seen_players:  pmap<Address, bool>;

// Three independent handlers, three disjoint state cells. The
// runtime runs them in parallel (no read/write overlap → no
// re-run). Order across handlers within one event is stable
// (declaration order), but the scheduler's *speedup* comes from
// not having to serialize disjoint work.

on ScoreSubmitted fn record_score(e: ScoreSubmitted) {
    scores[e.player] = e.score;
    by_score[e.score] = e.player;
}

on ScoreSubmitted fn bump_total(e: ScoreSubmitted) {
    let _ = e.player;
    let _ = e.score;
    total_games = total_games + 1u64;
}

on ScoreSubmitted fn track_seen(e: ScoreSubmitted) {
    let _ = e.score;
    seen_players[e.player] = true;
}

entry fn submit(player: Address, score: u64) -> u64 {
    emit ScoreSubmitted { player: player, score: score };
    return total_games;
}

entry view fn games_played() -> u64 { return total_games; }
entry view fn score_of(player: Address) -> u64 { return scores[player]; }

fn main() -> u64 {
    submit(address("alice"),  100u64);
    submit(address("bob"),    250u64);
    submit(address("carol"),  175u64);
    submit(address("alice"),  300u64);     // alice resubmits, higher
    // Three submissions × three handlers = 9 handler invocations.
    // With disjoint state, every batch merges without re-run.
    return total_games;
}
