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
//
// Each submit also moves the user's stake into the order book
// at submit time: buys lock cash, sells lock asset. When a tick
// commits, sellers' filled qty pays out as cash and buyers'
// filled qty pays out as asset; any per-unit refund (bid price
// minus clearing price) flows back to the buyer's cash. This
// gives the parallel-submit path real disjoint R→W work per
// stmt — each shadow Tx reads/writes one user's cash or asset
// cell plus one fresh order key.

module fba;

const SIDE_BUY:  u32 = 0u32;
const SIDE_SELL: u32 = 1u32;

struct Order { id: u64, price: u64, qty: u64, owner: Address }
struct Fill  { order_id: u64, price: u64, qty: u64, side: u32 }

state cash:  pmap<Address, u64>;
state asset: pmap<Address, u64>;

state pending_buys:  pmap<u64, Order>;
state pending_sells: pmap<u64, Order>;
state buy_book:      pbtree<bytes, u64>;
state sell_book:     pbtree<bytes, u64>;
state next_id:       u64;
state tick_no:       u64;
state fills:         pvec<Fill>;

unique_index buy_book  on pending_buys.(price DESC, id ASC);
unique_index sell_book on pending_sells.(price ASC,  id ASC);

// Escrow + register a buy. R→W on `cash[owner]` (deduct the
// max cost = qty × price), then a W on `pending_buys[id]`. The
// caller pre-allocates `id` so the four parallel submits in
// main don't fence on `next_id`.
entry fn escrow_buy(owner: Address, id: u64, price: u64, qty: u64) {
    cash[owner] = cash[owner] - price * qty;
    pending_buys[id] = Order { id: id, price: price, qty: qty, owner: owner };
}

// Same shape for sells — R→W on `asset[owner]`, then write
// the order.
entry fn escrow_sell(owner: Address, id: u64, price: u64, qty: u64) {
    asset[owner] = asset[owner] - qty;
    pending_sells[id] = Order { id: id, price: price, qty: qty, owner: owner };
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

    // 3. Pro-rata fills + escrow settlement. Each eligible
    //    order's fill is
    //        order.qty * cleared_volume / total_side_qty
    //    floored; the demonstration drops the integer remainder
    //    rather than reallocating it ("largest-remainder" is a
    //    drop-in upgrade later). The stable pbtree cursor lets
    //    us delete fully-filled orders mid-walk.
    //
    //    Settlement: buyers get asset for filled qty + a refund
    //    on cash for any (bid_price − P*) gap per filled unit.
    //    Sellers get cash for filled qty × P*. The cash and asset
    //    still locked against unfilled remainders stay inside the
    //    Order — no escrow-tally cell needed.
    for id in buy_book {
        let b = pending_buys[id];
        if b.price < best_price { break; }
        let fill_qty = b.qty * best_volume / total_buy;
        if fill_qty > 0u64 {
            pvec_push(fills, Fill {
                order_id: b.id, price: best_price,
                qty: fill_qty, side: SIDE_BUY,
            });
            asset[b.owner] = asset[b.owner] + fill_qty;
            let refund = (b.price - best_price) * fill_qty;
            if refund > 0u64 {
                cash[b.owner] = cash[b.owner] + refund;
            }
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
            cash[s.owner] = cash[s.owner] + best_price * fill_qty;
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
entry view fn cash_of(who: Address)  -> u64 { return cash[who]; }
entry view fn asset_of(who: Address) -> u64 { return asset[who]; }

fn main() -> u64 {
    let alice = address("alice");
    let bob   = address("bob");
    let carol = address("carol");
    let dave  = address("dave");

    // Seed funded balances. Buyers get cash, sellers get asset.
    // (Sequential — these are the only writes to these cells in
    // setup; no concurrency to exploit.)
    cash[alice]  = 1000u64;
    cash[bob]    =  600u64;
    asset[carol] =   10u64;
    asset[dave]  =   10u64;

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
    // Pro-rata + settlement:
    //   alice fills 8 → asset[alice] += 8; refund 0 (bid = clearing).
    //                   2 qty stays in pending_buys (200 cash still locked).
    //   carol fills 8 → cash[carol]  += 800. Order deleted.

    // Submit four orders concurrently. `submit_buy / submit_sell`
    // entries each bump `next_id`; calling them sequentially
    // inside `parallel { }` would conflict on that one counter
    // and re-run three of the four shadow Txs. Pre-allocating
    // the id range serially clears the conflict, after which
    // each parallel stmt is a real disjoint R→W cycle:
    //
    //   buys:  R cash[u]  → W cash[u]  → W pending_buys[id]
    //   sells: R asset[u] → W asset[u] → W pending_sells[id]
    //
    // Different users touch different `cash` / `asset` keys
    // (HAMT-disjoint at this scale), and the order writes hit
    // fresh pmap keys. The compiler-emitted index back-link
    // writes share buy_book / sell_book roots; those merge via
    // shadow-Tx conflict re-run, but the disjoint cash / asset
    // / order writes commute cleanly.
    let id_a = next_id + 1u64;
    let id_b = next_id + 2u64;
    let id_c = next_id + 3u64;
    let id_d = next_id + 4u64;
    next_id = id_d;
    parallel {
        escrow_buy(alice,  id_a, 100u64, 10u64);
        escrow_buy(bob,    id_b,  99u64,  5u64);
        escrow_sell(carol, id_c,  98u64,  8u64);
        escrow_sell(dave,  id_d, 101u64,  7u64);
    }

    let volume = commit_tick();

    // Encoded:  volume * 1_000_000
    //         + cash_of(carol) * 100
    //         + asset_of(alice)
    //
    //         = 8 * 1_000_000          (cleared volume)
    //         + 800 * 100              (seller received cash)
    //         +   8                    (buyer received asset)
    //         = 8_080_008
    //
    // The mix proves both the FBA math (volume) and the escrow
    // round-trip (asset to buyer, cash to seller) in one number —
    // drift in either path breaks the assertion.
    return volume * 1000000u64
         + cash_of(carol) * 100u64
         + asset_of(alice);
}
