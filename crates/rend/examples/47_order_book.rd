// Price-time priority order book — under the composite-index
// syntax sugar.
//
//   index ask_book on asks.(price ASC, placed_at ASC);
//   index bid_book on bids.(price DESC, placed_at ASC);
//
// The compiler maintains each index on every write to its primary:
// for every projected field it emits a `to_be_bytes` call,
// `bit_not_bytes` on DESC components, and folds the parts via
// `bytes_concat`. The packed `bytes` go straight into a
// `pbtree<bytes, u64>` slot in priority order; iterating that
// slot walks orders best-first, and primary deletes auto-clean
// the index back-links.
//
// What you write here is just the *intent* — sort by these
// fields in these directions. The encoding is the compiler's
// problem.

module order_book;

struct Order { id: u64, price: u64, qty: u64, placed_at: u64, owner: Address }
struct Fill  { bid_id: u64, ask_id: u64, price: u64, qty: u64, ts: u64 }

state asks:     pmap<u64, Order>;
state bids:     pmap<u64, Order>;
state ask_book: pbtree<bytes, u64>;
state bid_book: pbtree<bytes, u64>;
state next_id:  u64;
state next_seq: u64;
state fills:    pvec<Fill>;

// Composite indexes. Compiler-generated maintenance writes a
// (packed_key, primary_id) entry on every `asks[id] = Order{...}`
// (resp. bids); `delete asks[id]` cascades the back-link removal.
// `unique_index` because `placed_at` is the per-order sequence —
// it makes the (price, placed_at) pair unique, so the index slot
// is `pbtree<bytes, u64>` rather than `pbtree<bytes, [u64]>`.
unique_index ask_book on asks.(price ASC,  placed_at ASC);
unique_index bid_book on bids.(price DESC, placed_at ASC);

// Best-priority resting order. Iteration is restarted on each call
// so back-link deletes inside the match loop don't poison a
// streaming cursor (stable-cursor follow-up will lift that).
fn best_ask_id() -> u64 {
    for id in ask_book { return id; }
    return 0u64;
}
fn best_bid_id() -> u64 {
    for id in bid_book { return id; }
    return 0u64;
}

entry fn submit_buy(owner: Address, price: u64, qty: u64) -> u64 {
    let buyer_id  = next_id  + 1u64;
    let buyer_seq = next_seq + 1u64;
    next_id  = buyer_id;
    next_seq = buyer_seq;

    let remaining = qty;
    while remaining > 0u64 {
        let best = best_ask_id();
        if best == 0u64 { break; }
        let ask = asks[best];
        if ask.price > price { break; }
        let take = if ask.qty <= remaining { ask.qty } else { remaining };
        pvec_push(fills, Fill {
            bid_id: buyer_id,
            ask_id: ask.id,
            price:  ask.price,
            qty:    take,
            ts:     buyer_seq,
        });
        if ask.qty <= remaining {
            // `delete asks[ask.id]` cascades through the index
            // maintenance pass — no manual `delete ask_book[...]`.
            delete asks[ask.id];
            remaining = remaining - take;
        } else {
            asks[ask.id].qty = ask.qty - take;
            remaining = 0u64;
        }
    }

    if remaining > 0u64 {
        // The index maintenance picks up the (price, time) packing
        // automatically and inserts a back-link into `bid_book`.
        bids[buyer_id] = Order {
            id:        buyer_id,
            price:     price,
            qty:       remaining,
            placed_at: buyer_seq,
            owner:     owner,
        };
    }
    return buyer_id;
}

entry fn submit_sell(owner: Address, price: u64, qty: u64) -> u64 {
    let seller_id  = next_id  + 1u64;
    let seller_seq = next_seq + 1u64;
    next_id  = seller_id;
    next_seq = seller_seq;

    let remaining = qty;
    while remaining > 0u64 {
        let best = best_bid_id();
        if best == 0u64 { break; }
        let bid = bids[best];
        if bid.price < price { break; }
        let take = if bid.qty <= remaining { bid.qty } else { remaining };
        pvec_push(fills, Fill {
            bid_id: bid.id,
            ask_id: seller_id,
            price:  bid.price,
            qty:    take,
            ts:     seller_seq,
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
            id:        seller_id,
            price:     price,
            qty:       remaining,
            placed_at: seller_seq,
            owner:     owner,
        };
    }
    return seller_id;
}

entry view fn best_bid_price() -> u64 {
    let id = best_bid_id();
    if id == 0u64 { return 0u64; }
    return bids[id].price;
}
entry view fn best_ask_price() -> u64 {
    let id = best_ask_id();
    if id == 0u64 { return 0u64; }
    return asks[id].price;
}
entry view fn fill_count() -> u64 { return pvec_len(fills); }

fn main() -> u64 {
    let alice = address("alice");
    let bob   = address("bob");
    let carol = address("carol");

    submit_sell(alice, 100u64, 10u64);    // ask: 100 × 10
    submit_sell(carol, 102u64,  5u64);    // ask: 102 × 5
    submit_buy(bob,  99u64,  3u64);       // rests at 99 (no cross)
    submit_buy(bob, 105u64, 20u64);       // matches 10 @ 100 then 5 @ 102

    // Final state:
    //   fills = 2 (10 @ 100, 5 @ 102)
    //   best_bid = 105 (bob's leftover after matching)
    //   best_ask = 0   (both asks fully consumed)
    // Encoded: fills*1_000_000 + best_bid*1000 + best_ask = 2_105_000.
    return fill_count() * 1000000u64
         + best_bid_price() * 1000u64
         + best_ask_price();
}
