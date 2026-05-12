// Price-time priority order book.
//
// Composite indexing without syntax sugar yet — we hand-pack the
// composite key into `bytes` and store it in a `pbtree<bytes, u64>`.
// Lex order on the packed bytes is the priority order:
//
//   bid_book key = bit_not(price_be) || time_be
//                  └── DESC ──┘  └── ASC ──┘
//   ask_book key = price_be     || time_be
//                  └── ASC ──┘  └── ASC ──┘
//
// `bit_not` on the price bytes flips its sort direction (highest
// price → lowest packed key → first in the pbtree's natural ASC
// walk). Time gets appended in big-endian so earlier-placed orders
// at the same price win the tie.
//
// `bytes`-keyed pbtree handles arbitrary composite arity — the
// pattern generalizes to (price, exchange_id, time, …) just by
// concatenating more `to_be_bytes` calls. The upcoming
// `index NAME on STATE.(f1 ASC, f2 DESC, ...)` sugar will emit
// this same packing automatically.

module order_book;

struct Order { id: u64, price: u64, qty: u64, placed_at: u64, owner: Address }
struct Fill  { bid_id: u64, ask_id: u64, price: u64, qty: u64, ts: u64 }

state asks:    pmap<u64, Order>;
state bids:    pmap<u64, Order>;
state ask_book: pbtree<bytes, u64>;     // sorted: price ASC, time ASC
state bid_book: pbtree<bytes, u64>;     // sorted: price DESC, time ASC
state next_id:  u64;
state next_seq: u64;
state fills:    pvec<Fill>;

// ---------- composite key packers ----------

fn pack_ask_key(price: u64, time: u64) -> bytes {
    return bytes_concat(to_be_bytes(price), to_be_bytes(time));
}

fn pack_bid_key(price: u64, time: u64) -> bytes {
    // DESC price → bit-invert the price bytes so the highest
    // price sorts smallest under the pbtree's natural ASC walk.
    return bytes_concat(
        bit_not_bytes(to_be_bytes(price)),
        to_be_bytes(time),
    );
}

// ---------- matching helpers ----------

// Return the id of the current best-priority resting ask, or 0
// if the book is empty. Iteration restarts fresh each call so
// concurrent deletes (when this is called in a match loop) don't
// invalidate a stale cursor.
fn best_ask_id() -> u64 {
    for id in ask_book { return id; }
    return 0u64;
}
fn best_bid_id() -> u64 {
    for id in bid_book { return id; }
    return 0u64;
}

// ---------- submit ----------

entry fn submit_buy(owner: Address, price: u64, qty: u64) -> u64 {
    let buyer_id  = next_id  + 1u64;
    let buyer_seq = next_seq + 1u64;
    next_id  = buyer_id;
    next_seq = buyer_seq;

    let remaining = qty;
    // Match against resting asks while the next one is cheap
    // enough. Each iteration re-fetches the best ask so deletes
    // don't poison a streaming cursor (slice-1 limitation).
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
            delete asks[ask.id];
            delete ask_book[pack_ask_key(ask.price, ask.placed_at)];
            remaining = remaining - take;
        } else {
            asks[ask.id].qty = ask.qty - take;
            remaining = 0u64;
        }
    }

    if remaining > 0u64 {
        bids[buyer_id] = Order {
            id:        buyer_id,
            price:     price,
            qty:       remaining,
            placed_at: buyer_seq,
            owner:     owner,
        };
        bid_book[pack_bid_key(price, buyer_seq)] = buyer_id;
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
            delete bid_book[pack_bid_key(bid.price, bid.placed_at)];
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
        ask_book[pack_ask_key(price, seller_seq)] = seller_id;
    }
    return seller_id;
}

// ---------- views ----------

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

// ---------- driver ----------

fn main() -> u64 {
    let alice = address("alice");
    let bob   = address("bob");
    let carol = address("carol");

    // Asks: 100 × 10 (alice, earliest), 102 × 5 (carol).
    submit_sell(alice, 100u64, 10u64);
    submit_sell(carol, 102u64, 5u64);

    // Bid: 99 × 3 by bob — no cross, rests at 99.
    submit_buy(bob, 99u64, 3u64);

    // Aggressive bid: 105 × 20.  Matches 10 @ 100 (alice's full ask),
    // then 5 @ 102 (carol's full ask). Remaining 5 rests at 105.
    submit_buy(bob, 105u64, 20u64);

    // Final state:
    //   fills:  2  (10 @ 100, 5 @ 102)
    //   asks:   empty → best_ask_price = 0
    //   bids:   bob's 105 (best) + bob's 99
    //   best_bid_price = 105
    // Encoded: fills*1000000 + best_bid*1000 + best_ask = 2_105_000.
    return fill_count() * 1000000u64
         + best_bid_price() * 1000u64
         + best_ask_price();
}
