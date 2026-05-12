// `parallel { ... }` block — statement-granularity parallel
// execution. Each statement inside runs in its own shadow `Tx`
// under rayon; deltas merge in stable declaration order with
// intra-tx OCC re-run on conflict. `let` bindings inside escape
// into the enclosing scope.
//
// Pattern shown here: a portfolio-snapshot view. The work is
// three *independent* state reads (one per currency map) plus
// some downstream arithmetic. Run serially, that's three
// successive `Kv::get_many` round-trips. Inside a `parallel`
// block, each read fires under its own shadow `Tx`; the three
// shadow walks issue cells concurrently and the merged deltas
// give a single answer.
//
// Constraint reminders:
//   * Statements inside the block can only reference *outer-scope*
//     names — typeck rejects intra-block references so the
//     parallelism is honest. (`let usd = ...; let eur = usd;` is
//     a compile error.)
//   * Slice-1 body is flat (`let`, state assigns, call/emit/
//     delete). No `if`/`for`/nested `parallel` inside.

module accounts;

struct Portfolio {
    usd: u64,
    eur: u64,
    jpy: u64,
    // USD-equivalent total of all three balances, computed from
    // the fixed FX table below.
    total_in_usd: u64,
}

state usd_balances: pmap<Address, u64>;
state eur_balances: pmap<Address, u64>;
state jpy_balances: pmap<Address, u64>;

// Basis-point FX conversion factors (×100 for fixed-point math).
// Frozen here for the example; production code would source these
// from an oracle module via `entry view fn`.
const FX_USD_BP: u64 = 100u64;  // 1.00 ×
const FX_EUR_BP: u64 = 108u64;  // 1.08 ×
const FX_JPY_BP: u64 = 1u64;    //   ~0.01 × (illustrative)

// View entry — runs cheaply on the read-only path. The
// parallel block fans out the three pmap walks; without it,
// the walks would serialize across three cluster fences.
entry view fn snapshot(user: Address) -> Portfolio {
    parallel {
        let usd = usd_balances[user];
        let eur = eur_balances[user];
        let jpy = jpy_balances[user];
    }
    // Arithmetic happens after the merge — handlers/sub-tasks
    // never see partial state.
    let total = usd * FX_USD_BP / 100u64
              + eur * FX_EUR_BP / 100u64
              + jpy * FX_JPY_BP / 100u64;
    return Portfolio {
        usd:          usd,
        eur:          eur,
        jpy:          jpy,
        total_in_usd: total,
    };
}

// Parallel writes: each branch deposits to a *different* map cell,
// so the three writes commute under intra-tx OCC and merge with
// no re-run.
entry fn deposit_basket(user: Address, usd: u64, eur: u64, jpy: u64) {
    let prev_usd = usd_balances[user];
    let prev_eur = eur_balances[user];
    let prev_jpy = jpy_balances[user];
    parallel {
        usd_balances[user] = prev_usd + usd;
        eur_balances[user] = prev_eur + eur;
        jpy_balances[user] = prev_jpy + jpy;
    }
}

// Constructor / driver. Seeds balances, then asks for the
// snapshot. Expected: 1000 USD + 500 × 1.08 EUR + 100000 × 0.01
// JPY = 1000 + 540 + 1000 = 2540.
fn main() -> u64 {
    let alice = address("alice");
    deposit_basket(alice, 1000u64, 500u64, 100000u64);
    let p = snapshot(alice);
    return p.total_in_usd;
}
