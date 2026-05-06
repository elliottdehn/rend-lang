// SLICE 11+: module `oracle` — append-only price feed.
//
// Read-mostly: `loans` reads prices on every collateralization check, but
// only an admin (off-chain) calls `set_price`. Two transactions reading
// the same price under OCC don't conflict — only writes do.

module oracle;

state prices: map<Address, u64>;   // asset → price (cents)

entry fn set_price(asset: Address, price: u64) {
    prices[asset] = price;
}

entry fn price_of(asset: Address) -> u64 {
    return prices[asset];
}
