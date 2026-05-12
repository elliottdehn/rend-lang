// `parallel { ... }` — statement-granularity parallel execution.
// Each stmt inside runs in its own shadow `Tx` under rayon; deltas
// merge in stable declaration order with intra-tx OCC re-run on
// conflict. `let` bindings inside escape into the enclosing scope.
//
// When does `parallel` actually help?
//
//   1. **CPU-bound** independent statements (multiple reductions,
//      cross-module view-fn calls that each do meaningful work,
//      hashing/validation).
//
//   2. **Sequences of read→write on disjoint cells.** Serially,
//      ```
//      balances[alice] = balances[alice] + 100;
//      balances[bob]   = balances[bob]   + 50;
//      ```
//      runs as R(alice) → W(alice) → R(bob) → W(bob). The write
//      to `alice` fences the read cluster, so R(bob) can't share
//      its KV round-trip with R(alice). Inside `parallel { }`,
//      each stmt is its own shadow Tx — the two reads issue
//      concurrently, and the disjoint writes merge without
//      conflict re-run.
//
// `parallel` does NOT help for naked sequential reads (no
// intervening writes); the VM's lazy pmap walks already share
// `get_many` round-trips per HAMT level via `advance_walks`, so
// wrapping `let x = pmap[k];` reads in `parallel` *defeats* the
// natural batcher.
//
// This example shows the second pattern: a batch credit operation
// that adds different amounts to several user accounts. Without
// the parallel block, each credit's read-then-write fences the
// next read; with it, all the reads launch concurrently and the
// writes merge in stable declaration order.

module ledger;

state balances: pmap<Address, u64>;

// Batch credit — adds three different amounts to three different
// accounts. The interesting structural property: each line is a
// read-modify-write of a *different* cell, so the reads commute
// and the writes are disjoint. Serial execution would force the
// reads to round-trip the KV one at a time (the intervening
// writes fence the read cluster); parallel lets them launch
// together.
entry fn credit_batch(
    alice: Address, alice_delta: u64,
    bob:   Address, bob_delta:   u64,
    carol: Address, carol_delta: u64,
) {
    parallel {
        balances[alice] = balances[alice] + alice_delta;
        balances[bob]   = balances[bob]   + bob_delta;
        balances[carol] = balances[carol] + carol_delta;
    }
}

entry view fn balance_of(who: Address) -> u64 { return balances[who]; }

// Constructor / driver. Seeds opening balances (one user at a
// time — the seeding is a setup detail, not the demo), then
// runs two batch credits and returns the sum of final balances.
fn main() -> u64 {
    let alice = address("alice");
    let bob   = address("bob");
    let carol = address("carol");

    // Seed opening balances. (Serial — these are the only
    // operations on these cells, no read-write fencing to avoid.)
    balances[alice] = 1000u64;
    balances[bob]   = 500u64;
    balances[carol] = 250u64;

    // Two batch credits via the parallel-friendly entry. Inside
    // each call, the three read→writes happen concurrently.
    credit_batch(alice, 100u64, bob, 50u64, carol, 25u64);
    credit_batch(alice, 200u64, bob, 75u64, carol, 30u64);

    // Final: alice=1300, bob=625, carol=305 → 2230.
    return balance_of(alice) + balance_of(bob) + balance_of(carol);
}
