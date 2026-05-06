// SLICE 31: persistent vectors. `pvec<T>` is the indexed-trie
// counterpart to `pmap`: each tree node lives in its own content-
// addressed KV cell, with the state cell carrying just the
// (length, root) pair.
//
// Operations:
//
//   * `s[i]` — bounds-checked read. O(log32 N) cell touches.
//   * `s[i] = v` — bounds-checked write. O(log32 N) cell writes;
//     two transactions setting *different* indices commit in
//     parallel via the OCC root-pointer merge.
//   * `pvec_push(s, v)` — append, returns the new index. Two
//     concurrent pushes both want the next index, so push is the
//     vec's serial point: the loser re-executes against the
//     winner's commit and lands at the *next* index.
//   * `pvec_len(s)` — O(1), reads the length off the state cell.

module pvec_demo;

// A simple "event log" — each entry records a (sender, amount)
// transfer, addressable by sequential index. The `flagged` vec
// runs in parallel for moderation: any entry can be marked
// suspicious without touching the underlying log.

struct Transfer {
    from: Address,
    to: Address,
    amount: u64,
}

state log:     pvec<Transfer>;
state flagged: pvec<bool>;

entry fn record(to: Address, amount: u64) -> u64 {
    let t = Transfer {
        from: msg_sender(),
        to: to,
        amount: amount,
    };
    let i = pvec_push(log, t);
    // Track a parallel `flagged[i] = false` slot so disjoint
    // moderators can later flip it without contending with each
    // other or with reads of the underlying transfer.
    pvec_push(flagged, false);
    return i;
}

entry fn flag(i: i64) -> bool {
    flagged[i] = true;
    return true;
}

entry fn entry_count() -> u64 {
    return pvec_len(log);
}

entry fn amount_at(i: i64) -> u64 {
    let t = log[i];
    return t.amount;
}

entry fn is_flagged(i: i64) -> bool {
    return flagged[i];
}

// ---- demo --------------------------------------------------------
//
// Three transfers; flag the middle one; sum amounts excluding
// flagged entries to show indexed read + write working together.

fn main() -> u64 {
    let alice = address("alice");
    let bob   = address("bob");
    let carol = address("carol");

    record(alice, 100u64);
    record(bob,   200u64);
    record(carol, 300u64);

    flag(1);     // bob's transfer is flagged

    let n = entry_count();
    let total = 0u64;
    let i = 0;
    while i < 3 {
        if !is_flagged(i) {
            total = total + amount_at(i);
        }
        i = i + 1;
    }
    return total;       // 100 + 300 = 400 (bob's 200 was flagged)
}
