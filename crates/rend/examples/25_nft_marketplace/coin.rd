// SLICE 11+: module `coin` — fungible balance ledger that backs payments.
//
// Independent of `nft` and `market`; the marketplace pays the seller by
// calling `coin::transfer` at settlement time. Two transactions trading
// disjoint NFTs touch only their own buyer/seller cells in `balances`,
// so OCC commits them in parallel.

module coin;

state balances: map<Address, u64>;

entry fn mint(to: Address, amount: u64) {
    balances[to] = balances[to] + amount;
}

entry fn balance_of(who: Address) -> u64 {
    return balances[who];
}

entry fn transfer(from: Address, to: Address, amount: u64) -> bool {
    let b = balances[from];
    if b < amount { return false; }
    balances[from] = b - amount;
    balances[to]   = balances[to] + amount;
    return true;
}
