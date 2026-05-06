// Module: token. Exposes the typical fungible-token surface
// (transfer, balance) but with all the new annotations:
//   * `view fn` for read-only queries (balance_of, supply).
//   * `pmap<Address, u64>` so disjoint transfers commit in
//     parallel under the OCC root-pointer merge.
//   * Constructor seeds initial supply to the deployer.
//
// Other modules in this artifact (market, main) call into the
// `entry` surface here via `token::transfer(...)` / etc.

module token;

state balances:     pmap<Address, u64>;
state total_supply: u64;

entry view fn balance_of(who: Address) -> u64 {
    return balances[who];
}

entry view fn supply() -> u64 {
    return total_supply;
}

entry fn transfer(from: Address, to: Address, amount: u64) -> u64 {
    // We accept `from` as a parameter rather than reading
    // `msg_sender()` because the calling module is the actual
    // sender; we want to charge the *original* user. A real
    // contract would gate this with a capability — kept simple
    // here to focus on the cross-module call shape.
    assert(balances[from] >= amount);
    balances[from] = balances[from] - amount;
    balances[to]   = balances[to]   + amount;
    return balances[from];
}

entry fn mint(to: Address, amount: u64) -> u64 {
    balances[to] = balances[to] + amount;
    total_supply = total_supply + amount;
    return total_supply;
}

fn main() -> u64 {
    // Constructor: seed the deployer with a large initial pile.
    let deployer = msg_sender();
    balances[deployer] = 1000000u64;
    total_supply       = 1000000u64;
    return total_supply;
}
