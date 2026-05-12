// Frequent-batch auction (FBA). No time priority — orders sit
// in a pending tick; at tick close `commit_tick` picks the
// volume-maximizing uniform clearing price and fills every
// eligible order pro-rata at that single price.
//
// Two composite indexes give priority-order iteration without
// time as a tiebreaker — the `id` slot only resolves equal-price
// ties deterministically:
//
//   unique_index buy_book  on pending_buys.(price DESC, id ASC);
//   unique_index sell_book on pending_sells.(price ASC,  id ASC);
//
// The clearing-price search walks `buy_book` DESC. For each
// candidate price P:
//
//   buy_qty(P)  = Σ qty over buys  with price ≥ P
//   sell_qty(P) = Σ qty over sells with price ≤ P
//   cross(P)    = min(buy_qty, sell_qty)
//
// volume(P) is piecewise constant — it can only change at limit
// prices in either book. Walking buy-side candidates is enough
// to find the max under the standard "tie-break upward" rule
// (the highest-price tie is the natural choice — sellers can't
// complain about a higher price, buyers get a price they bid
// at or below). Pro-rata fills at the clearing price share the
// crossed volume among eligible orders by their order qty.

module fba;

const SIDE_BUY:  u32 = 0u32;
const SIDE_SELL: u32 = 1u32;

struct Order { id: u64, price: u64, qty: u64, owner: Address }
struct Fill  { order_id: u64, price: u64, qty: u64, side: u32 }

state pending_buys:  pmap<u64, Order>;
state pending_sells: pmap<u64, Order>;
state buy_book:      pbtree<bytes, u64>;
state sell_book:     pbtree<bytes, u64>;
state next_id:       u64;
state tick_no:       u64;
state fills:         pvec<Fill>;

unique_index buy_book  on pending_buys.(price DESC, id ASC);
unique_index sell_book on pending_sells.(price ASC,  id ASC);

entry fn submit_buy(owner: Address, price: u64, qty: u64) -> u64 {
    let id = next_id + 1u64;
    next_id = id;
    pending_buys[id] = Order { id: id, price: price, qty: qty, owner: owner };
    return id;
}

entry fn submit_sell(owner: Address, price: u64, qty: u64) -> u64 {
    let id = next_id + 1u64;
    next_id = id;
    pending_sells[id] = Order { id: id, price: price, qty: qty, owner: owner };
    return id;
}

// Σ qty over buys with price ≥ p. Iterates buy_book DESC,
// breaks when the cursor drops below p.
fn buy_qty_above(p: u64) -> u64 {
    let total = 0u64;
    for id in buy_book {
        let b = pending_buys[id];
        if b.price < p { break; }
        total = total + b.qty;
    }
    return total;
}

// Σ qty over sells with price ≤ p. Iterates sell_book ASC,
// breaks when the cursor exceeds p.
fn sell_qty_below(p: u64) -> u64 {
    let total = 0u64;
    for id in sell_book {
        let s = pending_sells[id];
        if s.price > p { break; }
        total = total + s.qty;
    }
    return total;
}

// Run the uncrossing auction for the current tick. Returns the
// crossed volume (0 if the books don't cross at any price).
entry fn commit_tick() -> u64 {
    // 1. Find the clearing price P* — the candidate price that
    //    maximizes min(buy_qty(P), sell_qty(P)). The DESC walk
    //    naturally picks the highest tie-breaking P* if several
    //    candidates share the max.
    let best_price  = 0u64;
    let best_volume = 0u64;
    for buy_id in buy_book {
        let candidate = pending_buys[buy_id].price;
        let bq = buy_qty_above(candidate);
        let sq = sell_qty_below(candidate);
        let cross = if bq <= sq { bq } else { sq };
        if cross > best_volume {
            best_volume = cross;
            best_price  = candidate;
        }
    }
    if best_volume == 0u64 {
        tick_no = tick_no + 1u64;
        return 0u64;
    }

    // 2. Eligibility totals at P*. `total_buy` is the demand we'll
    //    split across eligible buys; `total_sell` the supply
    //    across eligible sells. cleared = min(total_buy,
    //    total_sell) = best_volume.
    let total_buy  = buy_qty_above(best_price);
    let total_sell = sell_qty_below(best_price);

    // 3. Pro-rata fills. Each eligible order's fill is
    //        order.qty * cleared_volume / total_side_qty
    //    floored; the demonstration drops the integer remainder
    //    rather than reallocating it ("largest-remainder" is a
    //    drop-in upgrade later). The stable pbtree cursor lets
    //    us delete fully-filled orders mid-walk.
    for id in buy_book {
        let b = pending_buys[id];
        if b.price < best_price { break; }
        let fill_qty = b.qty * best_volume / total_buy;
        if fill_qty > 0u64 {
            pvec_push(fills, Fill {
                order_id: b.id, price: best_price,
                qty: fill_qty, side: SIDE_BUY,
            });
        }
        if fill_qty >= b.qty {
            delete pending_buys[b.id];
        } else {
            pending_buys[b.id].qty = b.qty - fill_qty;
        }
    }
    for id in sell_book {
        let s = pending_sells[id];
        if s.price > best_price { break; }
        let fill_qty = s.qty * best_volume / total_sell;
        if fill_qty > 0u64 {
            pvec_push(fills, Fill {
                order_id: s.id, price: best_price,
                qty: fill_qty, side: SIDE_SELL,
            });
        }
        if fill_qty >= s.qty {
            delete pending_sells[s.id];
        } else {
            pending_sells[s.id].qty = s.qty - fill_qty;
        }
    }

    tick_no = tick_no + 1u64;
    return best_volume;
}

entry view fn fill_count()   -> u64 { return pvec_len(fills); }
entry view fn current_tick() -> u64 { return tick_no; }

fn main() -> u64 {
    let alice = address("alice");
    let bob   = address("bob");
    let carol = address("carol");
    let dave  = address("dave");

    // Build a crossing book:
    //   buys:  alice 10 @ 100, bob   5 @ 99
    //   sells: carol  8 @ 98,  dave  7 @ 101
    //
    // Candidate clearing prices (from the buy side):
    //   P=100: buy_qty_above=10 (alice). sell_qty_below=8 (carol
    //          @98 qualifies, dave @101 doesn't). cross = min(10,8) = 8.
    //   P=99:  buy_qty_above=15 (alice+bob). sell_qty_below=8 (still
    //          just carol). cross = min(15,8) = 8.
    // Tie at 8. DESC walk visits 100 first, so best_volume sets at 100
    // and stays (the strict `>` keeps the highest-price tiebreak).
    //
    // At P*=100 the eligible sides are:
    //   buys  ≥ 100: alice only — total_buy  = 10
    //   sells ≤ 100: carol only — total_sell = 8
    // cleared = best_volume = 8.
    //
    // Pro-rata:
    //   alice fill = 10 * 8 / 10 = 8  (her order has 2 qty left)
    //   carol fill = 8  * 8 / 8  = 8  (fully filled, order deleted)
    //
    // Result: 2 fills, volume=8.

    // Submit four orders concurrently. The sequential `submit_buy
    // / submit_sell` entries each bump `next_id` — calling them
    // back-to-back inside `parallel { }` would conflict on that
    // one counter and re-run three of the four. Pre-allocating
    // the id range serially clears the conflict, after which the
    // four primary writes hit disjoint pmap keys (alice/bob in
    // pending_buys, carol/dave in pending_sells) and the shadow
    // Tx merge runs without re-execution. The compiler's index
    // maintenance still fires inside each shadow Tx — same back-
    // link writes as the entry-fn path.
    let id_a = next_id + 1u64;
    let id_b = next_id + 2u64;
    let id_c = next_id + 3u64;
    let id_d = next_id + 4u64;
    next_id = id_d;
    parallel {
        pending_buys[id_a]  = Order { id: id_a, price: 100u64, qty: 10u64, owner: alice };
        pending_buys[id_b]  = Order { id: id_b, price:  99u64, qty:  5u64, owner: bob   };
        pending_sells[id_c] = Order { id: id_c, price:  98u64, qty:  8u64, owner: carol };
        pending_sells[id_d] = Order { id: id_d, price: 101u64, qty:  7u64, owner: dave  };
    }

    let volume = commit_tick();

    // Encoded: volume*1_000_000 + clearing_price*100 + fills
    //        = 8*1_000_000 + 100*100 + 2 = 8_010_002.
    //
    // We pull clearing_price back as the price stamped on alice's
    // fill (slot 0 in the pvec). Doing it this way keeps `main`
    // self-checking — it can fail loudly if pro-rata or
    // uncrossing drift later.
    let first_fill = fills[0i64];
    return volume * 1000000u64
         + first_fill.price * 100u64
         + fill_count();
}
