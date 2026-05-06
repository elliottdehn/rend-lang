// SLICE 34: dynamic dispatch through `view` interfaces.
//
// Three modules: portfolio (holds the interface decl + state),
// oracle_simple, oracle_premium. The driver below shows how
// the same `portfolio.portfolio_value` entry routes to two
// different oracles depending on which one is bound at the
// time of the call.
//
// What's interesting:
//
//   * `IPriceFeed::price_of` is annotated `view`. The compiler
//     stamps that effect bound onto every `$feed::price_of(...)`
//     dispatch. `portfolio_value` is itself `view`, and would
//     fail to verify if the dispatch counted as Impure.
//   * Both oracles have completely different internals (one
//     stores a price table, the other adds a 5% premium on
//     top of a base table). The portfolio module never knows.
//   * The host could serve `portfolio_value` queries on the
//     read-only path (`Engine::query`) — they're statically
//     proven to never write.

module main;

fn main() -> u64 {
    // Seed both oracles' tables.
    oracle_simple::set_price("ETH",  3000u64);
    oracle_simple::set_price("USDC",    1u64);
    oracle_premium::set_base("ETH",  3000u64);
    oracle_premium::set_base("USDC",    1u64);

    // Stand up alice's portfolio: 5 units of ETH.
    portfolio::set_unit("ETH");
    portfolio::record_holdings(address("alice"), 5u64);

    // First valuation: route through oracle_simple.
    portfolio::switch_feed("oracle_simple");
    let v_simple = portfolio::portfolio_value(address("alice"));   // 15000

    // Re-bind to oracle_premium and re-query — same alice,
    // same holdings, same call site, different number.
    portfolio::switch_feed("oracle_premium");
    let v_premium = portfolio::portfolio_value(address("alice"));  // 15750

    // Apply a haircut via the pure helper through the view
    // entry — exercises the compose path.
    let v_premium_haircut = portfolio::portfolio_value_after_haircut(
        address("alice"),
        100u64,    // 1% haircut
    );  // 15750 * 9900 / 10000 = 15592 (integer division)

    // Pack three numbers into one return:
    //   v_simple in millions: 15000 → 15
    //   v_premium in tens:    15750 → 1575
    //   haircut in units:                15592
    return v_simple * 1000000u64
         + v_premium * 100u64
         + v_premium_haircut;
}
