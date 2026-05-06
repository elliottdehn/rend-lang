// A second oracle implementation that quotes a 5% premium —
// imagine a feed that includes spread/fee/risk-adjustment.
// The consumer (`portfolio`) doesn't change; only the runtime
// binding does.

module oracle_premium;

state base_prices: pmap<string, u64>;

entry view fn price_of(symbol: string) -> u64 {
    return base_prices[symbol] * 105u64 / 100u64;
}

entry fn set_base(symbol: string, price: u64) -> u64 {
    base_prices[symbol] = price;
    return price;
}
