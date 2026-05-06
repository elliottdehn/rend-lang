// SLICE 27+: sum types + match — encoded states the type system
// can't betray. The classic patterns:
//
//   - `Result`-shaped fallible operations
//   - Lifecycle stages where some fields only exist in some states
//   - Branching dispatch where the compiler enforces exhaustiveness
//
// This file walks through all three.

module sumtypes;

// 1. Result-style sum: success carries the value, failure carries
//    a reason string. Compare with returning -1 sentinels in Solidity.
enum Lookup {
    Found(u64),
    Missing(string),
}

state ledger: map<Address, u64>;

entry fn balance_of(who: Address) -> Lookup {
    let b = ledger[who];
    if b == 0u64 {
        return Lookup::Missing("no balance");
    }
    return Lookup::Found(b);
}

// 2. Lifecycle stages — some fields only exist in some states.
//    A `Closed` proposal carries a winner and a payout; an open or
//    voting proposal doesn't. Encoding the asymmetry as variants
//    rules out illegal states (`closed=true` AND `winner=0x` etc).
enum Stage {
    Open,
    Voting(u64),                  // payload: voting_deadline
    Closed(Address, u64),         // payload: (winner, payout)
}

state stage: Stage;

entry fn open_voting(deadline: u64) -> Stage {
    let next = Stage::Voting(deadline);
    stage = next;
    return next;
}

entry fn close_voting(winner: Address, payout: u64) -> Stage {
    let next = Stage::Closed(winner, payout);
    stage = next;
    return next;
}

// 3. Match dispatch — compiler enforces every variant covered.
//    Returns a status code per stage:
//      Open    → 0
//      Voting  → seconds until the deadline (0 if expired)
//      Closed  → the payout amount
//
// Block-as-expression lets us write the multi-line voting branch
// inline as `=> { stmt; tail }`. The if-expression in the inner
// block also yields a value directly — no helper needed.

entry fn stage_status(now: u64) -> u64 {
    return match stage {
        Stage::Open => 0u64,
        Stage::Voting(deadline) => {
            if now >= deadline { 0u64 } else { deadline - now }
        },
        Stage::Closed(_winner, payout) => payout,
    };
}

// Wildcards skip enumerating every case. Use sparingly — non-
// wildcard match is usually the safer default because new variants
// added later force you to revisit each call site.
entry fn is_terminal() -> bool {
    return match stage {
        Stage::Closed(_, _) => true,
        _ => false,
    };
}

// 4. Driver demonstrating all three patterns in one tx.
fn main() -> u64 {
    let alice = address("0xa11ce");

    // Seed a balance and look it up via Lookup-typed return.
    ledger[alice] = 100u64;
    let found_amount = match balance_of(alice) {
        Lookup::Found(n)    => n,
        Lookup::Missing(_)  => 0u64,
    };
    assert(found_amount == 100u64, "lookup should see the balance");

    // Lookup of a missing key — second variant fires.
    let zero_amount = match balance_of(address("0xnobody")) {
        Lookup::Found(n)    => n,
        Lookup::Missing(_)  => 0u64,
    };
    assert(zero_amount == 0u64, "missing should fall through to 0");

    // Walk the stage lifecycle.
    open_voting(1000u64);
    let mid_voting = stage_status(400u64);
    assert(mid_voting == 600u64, "should report time-to-deadline");

    close_voting(alice, 999u64);
    let after = stage_status(1500u64);
    assert(after == 999u64, "closed stage exposes payout");

    assert(is_terminal(), "Closed counts as terminal");

    return found_amount + after;     // 100 + 999 = 1099
}

// Block expressions inside match arms aren't a separate language
// feature — the `if/else` inside `Stage::Voting` is just a regular
// nested expression. We don't have block-as-expression yet, so
// multi-statement arms still need a helper fn.
