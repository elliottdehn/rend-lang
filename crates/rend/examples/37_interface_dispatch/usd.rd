// One implementation of the IERC20-like interface defined in
// router.rd. Modules don't *declare* implementing an interface
// — the binding is a runtime act (`IERC20::bind("usd")`). We
// just need this module's `entry fn`s to match the interface's
// signatures or the host will fail at the bind site.

module usd;

state balances: pmap<Address, u64>;

entry fn transfer(from: Address, to: Address, amount: u64) -> u64 {
    let b = balances[from];
    let a = balances[to];
    assert(b >= amount);
    balances[from] = b - amount;
    balances[to]   = a + amount;
    return balances[from];
}

entry view fn balance_of(who: Address) -> u64 {
    return balances[who];
}

entry fn mint(to: Address, amount: u64) -> u64 {
    balances[to] = balances[to] + amount;
    return balances[to];
}
