// SLICE 10b (working): a sealed-bid auction in a single module.
//
// Real, end-to-end usable today. Demonstrates:
//   - struct state for the running highest bid
//   - bool state for the lifecycle flag
//   - Address values as participants
//   - conditional state updates that produce minimal RW sets
//   - all under fuel + OCC like everything else
//
// The RW set for a successful bid:
//   READS:  state_root("main", "ended"), state_root("main", "highest")
//   WRITES: state_root("main", "highest")
//
// Two simultaneous bids touch the same `highest` slot — under
// `commit_batch` the second one re-executes against the post-first KV
// (and may then be rejected if its amount is no longer winning).

struct Bid {
    bidder: Address,
    amount: i64,
}

state highest: Bid;
state ended: bool;

entry fn place_bid(who: Address, amount: i64) -> bool {
    if ended { return false; }
    if amount <= highest.amount { return false; }
    highest = Bid { bidder: who, amount: amount };
    return true;
}

entry fn close() {
    ended = true;
}

fn main() -> i64 {
    place_bid(address("0xa11ce"), 100);   // accepted
    place_bid(address("0xb0b"),   150);   // accepted (higher)
    place_bid(address("0xcaro1"), 120);   // rejected (lower than 150)
    close();
    return highest.amount;                 // 150
}
