// Multi-symbol order book — composite-index arity stretched to
// three fields.
//
//   unique_index ask_book on orders.(symbol ASC, price ASC,  placed_at ASC);
//   unique_index bid_book on orders.(symbol ASC, price DESC, placed_at ASC);
//
// The leading `symbol` component groups the same instrument's
// orders adjacent in the index. Inside a symbol the usual
// price-time priority applies. Iterating the index walks
// instruments in id order, and within each instrument best-price
// first — a single pbtree<bytes, u64> serves every symbol.
//
// Matching against a specific symbol uses ordered traversal:
// skip until we reach the symbol, match within, break when the
// symbol changes. No range-from-prefix primitive yet, so the
// pre-skip is linear in the number of orders for lower-numbered
// symbols. Real production rend would want a `range_from(prefix)`
// primitive; for this demo the cost is acceptable.

module book_v2;

// Symbols are u32 ids — pick a fixed mapping at the host layer.
const BTCUSD: u32 = 1u32;
const ETHUSD: u32 = 2u32;
const SOLUSD: u32 = 3u32;

struct Order {
    id:        u64,
    symbol:    u32,
    price:     u64,
    qty:       u64,
    placed_at: u64,
    owner:     Address,
}

struct Fill {
    bid_id:    u64,
    ask_id:    u64,
    symbol:    u32,
    price:     u64,
    qty:       u64,
    ts:        u64,
}

state asks:     pmap<u64, Order>;
state bids:     pmap<u64, Order>;
state ask_book: pbtree<bytes, u64>;
state bid_book: pbtree<bytes, u64>;
state next_id:  u64;
state next_seq: u64;
state fills:    pvec<Fill>;

// Composite indexes — three fields, mixed direction on `price`
// for the two sides. Maintenance auto-packs into the bytes key
// (`bit_not_bytes` is emitted under the hood for the DESC slot).
unique_index ask_book on asks.(symbol ASC, price ASC,  placed_at ASC);
unique_index bid_book on bids.(symbol ASC, price DESC, placed_at ASC);

// ---------- submit ----------

entry fn submit_buy(owner: Address, symbol: u32, price: u64, qty: u64) -> u64 {
    let buyer_id  = next_id  + 1u64;
    let buyer_seq = next_seq + 1u64;
    next_id  = buyer_id;
    next_seq = buyer_seq;

    let remaining = qty;
    // Walk the ask book in priority order. Composite leads with
    // `symbol`, so once we *pass* our symbol every remaining
    // entry is for a higher-numbered instrument — break out.
    // Stable cursor means we can `delete asks[id]` mid-loop.
    for id in ask_book {
        if remaining == 0u64 { break; }
        let ask = asks[id];
        if ask.symbol < symbol { continue; }       // not our instrument yet
        if ask.symbol > symbol { break; }          // past it; nothing more for us
        if ask.price > price  { break; }           // crossed past our limit
        let take = if ask.qty <= remaining { ask.qty } else { remaining };
        pvec_push(fills, Fill {
            bid_id: buyer_id, ask_id: ask.id,
            symbol: symbol,   price: ask.price,
            qty: take,        ts: buyer_seq,
        });
        if ask.qty <= remaining {
            delete asks[ask.id];
            remaining = remaining - take;
        } else {
            asks[ask.id].qty = ask.qty - take;
            remaining = 0u64;
        }
    }
    if remaining > 0u64 {
        bids[buyer_id] = Order {
            id: buyer_id, symbol: symbol, price: price,
            qty: remaining, placed_at: buyer_seq, owner: owner,
        };
    }
    return buyer_id;
}

entry fn submit_sell(owner: Address, symbol: u32, price: u64, qty: u64) -> u64 {
    let seller_id  = next_id  + 1u64;
    let seller_seq = next_seq + 1u64;
    next_id  = seller_id;
    next_seq = seller_seq;

    let remaining = qty;
    for id in bid_book {
        if remaining == 0u64 { break; }
        let bid = bids[id];
        if bid.symbol < symbol { continue; }
        if bid.symbol > symbol { break; }
        if bid.price  < price  { break; }
        let take = if bid.qty <= remaining { bid.qty } else { remaining };
        pvec_push(fills, Fill {
            bid_id: bid.id,    ask_id: seller_id,
            symbol: symbol,    price:  bid.price,
            qty:    take,      ts:     seller_seq,
        });
        if bid.qty <= remaining {
            delete bids[bid.id];
            remaining = remaining - take;
        } else {
            bids[bid.id].qty = bid.qty - take;
            remaining = 0u64;
        }
    }
    if remaining > 0u64 {
        asks[seller_id] = Order {
            id: seller_id, symbol: symbol, price: price,
            qty: remaining, placed_at: seller_seq, owner: owner,
        };
    }
    return seller_id;
}

// ---------- views ----------

// First bid/ask for a given symbol. The composite leads with
// `symbol`, so the first match is the symbol's top of book.
entry view fn best_bid_price(symbol: u32) -> u64 {
    for id in bid_book {
        let b = bids[id];
        if b.symbol < symbol { continue; }
        if b.symbol > symbol { return 0u64; }
        return b.price;
    }
    return 0u64;
}
entry view fn best_ask_price(symbol: u32) -> u64 {
    for id in ask_book {
        let a = asks[id];
        if a.symbol < symbol { continue; }
        if a.symbol > symbol { return 0u64; }
        return a.price;
    }
    return 0u64;
}
entry view fn fill_count() -> u64 { return pvec_len(fills); }

// ---------- driver ----------

fn main() -> u64 {
    let alice = address("alice");
    let bob   = address("bob");
    let carol = address("carol");

    // Seed three asks on three different instruments. The
    // composite groups them by symbol; within a symbol, by price.
    submit_sell(alice, BTCUSD, 50000u64, 1u64);    // ask: BTC @ 50k
    submit_sell(alice, ETHUSD,  3000u64, 5u64);    // ask: ETH @ 3k
    submit_sell(carol, BTCUSD, 50100u64, 2u64);    // ask: BTC @ 50.1k
    submit_sell(carol, SOLUSD,   200u64, 10u64);   // ask: SOL @ 200

    // Bid 1: takes 1 BTC @ 50000 (alice's full ask) — leaves the
    // remaining 50100 BTC ask untouched.
    submit_buy(bob, BTCUSD, 50050u64, 1u64);
    // Bid 2: 8 ETH @ 3000 — fully fills alice's 5-ETH ask, then
    // rests with 3 ETH remaining.
    submit_buy(bob, ETHUSD, 3000u64, 8u64);
    // Bid 3: 5 SOL @ 250 — full fill at 200 (carol's ask).
    submit_buy(bob, SOLUSD, 250u64, 5u64);

    // Expected fills: 3 (1 BTC @ 50k, 5 ETH @ 3k, 5 SOL @ 200).
    //   BTC ask top: 50100 — carol's untouched 50.1k ask.
    //   ETH bid top: 3000  — bob's 3-ETH leftover.
    //   SOL ask top: 200   — carol's 10-SOL ask has 5 left after
    //                        bob's 5-SOL bid drained part of it.
    // Encoded fills*1e9 + best_btc_ask*1e4 + best_eth_bid*10 + best_sol_ask
    //   = 3e9 + 501_000_000 + 30_000 + 200 = 3_501_030_200.
    return fill_count() * 1000000000u64
         + best_ask_price(BTCUSD) * 10000u64
         + best_bid_price(ETHUSD) * 10u64
         + best_ask_price(SOLUSD);
}
