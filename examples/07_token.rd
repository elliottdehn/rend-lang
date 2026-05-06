// SLICE 7: map state. ERC20-ish balance ledger.
//
// `state balances: map<i64, i64>;` declares a per-cell namespace; each
// `balances[k]` becomes its own KV row at runtime (key composed as
// "balances/<k>"). Missing cells read as the value-type default (0 here).
//
// RW set produced by transfer(alice, bob, 10):
//   READS:   balances/<alice>, balances/<bob>
//   WRITES:  balances/<alice>, balances/<bob>
//
// Two transfers between disjoint address pairs touch disjoint keys → they
// commit in parallel under the OCC driver in slice 8.
//
// `Address` is i64 in this MVP. A future slice can introduce a typed Address.

state balances: map<i64, i64>;

entry fn balance_of(who: i64) -> i64 {
    return balances[who];
}

entry fn transfer(from: i64, to: i64, amount: i64) -> bool {
    let b = balances[from];
    if b < amount { return false; }
    balances[from] = b - amount;
    balances[to] = balances[to] + amount;
    return true;
}

fn main() -> bool {
    // For the test harness: seed alice=100, bob=0; transfer 30; result is true.
    balances[1] = 100;
    return transfer(1, 2, 30);
}
