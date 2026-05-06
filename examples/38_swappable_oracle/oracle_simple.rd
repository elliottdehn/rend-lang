// A trivial oracle implementation: a fixed price table baked
// into module state. Real oracles fetch from off-chain feeds
// or aggregate trading data — the point of the interface
// boundary is that the consumer doesn't have to know.

module oracle_simple;

state prices: pmap<string, u64>;

entry view fn price_of(symbol: string) -> u64 {
    return prices[symbol];
}

entry fn set_price(symbol: string, price: u64) -> u64 {
    prices[symbol] = price;
    return price;
}
