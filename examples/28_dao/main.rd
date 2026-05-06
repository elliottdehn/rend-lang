// Main entry — drives a full DAO lifecycle in one transaction.
//
// Storyline:
//   1. Three members register with weighted voting power
//      (alice=10, bob=5, carol=3 — total 18).
//   2. Alice proposes a treasury payout to "0xt" for 100 tokens.
//   3. Members vote: alice yes (10), bob yes (5), carol no (3).
//      Tally: 15 yes vs 3 no → passes.
//   4. Anyone calls `execute` — the proposal is finalized and emitted.
//
// The host receives a single (read_set, write_set, event_log). The
// event log threads three modules' emits in execution order:
//
//   members::Added × 3
//   proposals::Proposed
//   proposals::Voted × 3
//   proposals::Executed
//
// main returns 1 if the proposal passed, 0 otherwise.

module main;

fn main() -> i64 {
    let alice = address("0xa11ce");
    let bob   = address("0xb0b");
    let carol = address("0xca7a");

    members::add(alice, 10u64);
    members::add(bob,    5u64);
    members::add(carol,  3u64);

    let id = proposals::propose(alice, address("0xtreasury"), 100u64);
    proposals::vote(id, alice, true);
    proposals::vote(id, bob,   true);
    proposals::vote(id, carol, false);

    let passed = proposals::execute(id);
    if passed { return 1; }
    return 0;
}
