// A second implementation of the same interface. The router
// can dispatch to either at runtime depending on which one is
// bound.

module eur;

state balances: pmap<Address, u64>;

entry fn transfer(from: Address, to: Address, amount: u64) -> u64 {
    // Demonstration twist: this implementation charges a 1u64
    // protocol fee on every transfer. The router doesn't know;
    // dispatch is dynamic.
    assert(balances[from] >= amount + 1u64);
    balances[from] = balances[from] - amount - 1u64;
    balances[to]   = balances[to]   + amount;
    return balances[from];
}

entry view fn balance_of(who: Address) -> u64 {
    return balances[who];
}

entry fn mint(to: Address, amount: u64) -> u64 {
    balances[to] = balances[to] + amount;
    return balances[to];
}
