// Portfolio module — values a user's holdings by asking the
// currently-bound price feed. The interface declaration here
// is the *only* place that lists the price-feed surface; the
// concrete oracle modules above don't import or extend it,
// they just happen to satisfy the signatures.
//
// `portfolio_value` is `entry view`: it reads state and routes
// through a `view` interface method. The compiler tracks that
// effect bound through the dynamic dispatch — `Engine::query`
// can serve this entry on the read-only path without OCC.

module portfolio;

interface IPriceFeed {
    entry view fn price_of(symbol: string) -> u64;
}

state holdings:    pmap<Address, u64>;
state asset_unit:  string;
state feed:        IPriceFeed;

// Constructor would normally seed `feed` here, but
// execute_modules only runs `main`'s constructor; we expose a
// switch_feed entry instead so the demo can re-bind from
// the driver tx.
entry fn switch_feed(name: string) -> u64 {
    feed = IPriceFeed::bind(name);
    return 1u64;
}

entry fn set_unit(symbol: string) -> u64 {
    asset_unit = symbol;
    return 1u64;
}

entry fn record_holdings(who: Address, units: u64) -> u64 {
    holdings[who] = units;
    return units;
}

// The view path: dispatches to whichever oracle is bound.
// Compiler verifies `price_of` is `view`, so this whole fn
// stays read-only — eligible for `Engine::query`.
entry view fn portfolio_value(who: Address) -> u64 {
    let units = holdings[who];
    let unit_price = $feed::price_of(asset_unit);
    return units * unit_price;
}

// A pure helper showing the `pure` annotation works alongside
// view dispatch — the compiler verifies this fn touches no
// state and can constant-fold it in the bytecode optimizer.
pure fn apply_haircut(value: u64, bps: u64) -> u64 {
    return value * (10000u64 - bps) / 10000u64;
}

entry view fn portfolio_value_after_haircut(who: Address, bps: u64) -> u64 {
    return apply_haircut(portfolio_value(who), bps);
}
