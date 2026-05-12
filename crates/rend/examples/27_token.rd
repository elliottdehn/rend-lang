// SLICE 11+: ERC20-style fungible token with events and assertions.
//
// This example flexes the recently-landed language pieces:
//   - `module <name>;`              top-of-file module declaration
//   - struct-shaped events          `emit Foo { ... };`
//   - `assert(cond, "msg")`         invariant checks at boundaries
//   - composite struct map keys     allowances keyed by (owner, spender)
//   - granular struct state         meta: Meta gets one cell per field
//   - per-cell map state            balances/allowances → disjoint OCC
//
// `main()` plays the part of the host's transaction-submission code: it
// initializes the contract once and drives a transfer/approve/transfer_from
// /burn workflow. Each `entry fn` is what an off-chain caller would invoke.

module token;

struct Meta { name: string, symbol: string, decimals: u32 }
struct AllowanceKey { owner: Address, spender: Address }

state meta:         Meta;
state total_supply: u64;
state balances:     map<Address, u64>;
state allowances:   map<AllowanceKey, u64>;

// Events are just structs — emit a value, the runtime logs (module,
// struct_name, fields) so the host can recognize the shape.
struct Transfer { from: Address, to: Address, amount: u64 }
struct Approval { owner: Address, spender: Address, amount: u64 }
struct Mint     { to: Address, amount: u64 }
struct Burn     { from: Address, amount: u64 }

entry fn init(name: string, symbol: string, decimals: u32) {
    // Granular state: each field becomes its own KV cell at
    // child(state_root("token", "meta"), field_name).
    meta.name     = name;
    meta.symbol   = symbol;
    meta.decimals = decimals;
}

entry fn balance_of(who: Address) -> u64 {
    return balances[who];
}

entry fn allowance(owner: Address, spender: Address) -> u64 {
    return allowances[AllowanceKey { owner: owner, spender: spender }];
}

entry fn approve(owner: Address, spender: Address, amount: u64) -> bool {
    allowances[AllowanceKey { owner: owner, spender: spender }] = amount;
    emit Approval { owner: owner, spender: spender, amount: amount };
    return true;
}

entry fn transfer(from: Address, to: Address, amount: u64) -> bool {
    let b = balances[from];
    let a = balances[to];
    assert(b >= amount, "insufficient balance");
    balances[from] = b - amount;
    balances[to]   = a + amount;
    emit Transfer { from: from, to: to, amount: amount };
    return true;
}

entry fn transfer_from(spender: Address, from: Address, to: Address, amount: u64) -> bool {
    let k = AllowanceKey { owner: from, spender: spender };
    let allowed = allowances[k];
    let b = balances[from];
    let a = balances[to];
    assert(allowed >= amount, "insufficient allowance");
    assert(b >= amount, "insufficient balance");
    allowances[k]  = allowed - amount;
    balances[from] = b - amount;
    balances[to]   = a + amount;
    emit Transfer { from: from, to: to, amount: amount };
    return true;
}

entry fn mint(to: Address, amount: u64) -> bool {
    let a = balances[to];
    let s = total_supply;
    balances[to] = a + amount;
    total_supply = s + amount;
    emit Mint { to: to, amount: amount };
    // ERC20 convention: mint emits Transfer from the zero address.
    emit Transfer { from: address(""), to: to, amount: amount };
    return true;
}

entry fn burn(from: Address, amount: u64) -> bool {
    let b = balances[from];
    let s = total_supply;
    assert(b >= amount, "insufficient balance");
    balances[from] = b - amount;
    total_supply   = s - amount;
    emit Burn { from: from, amount: amount };
    emit Transfer { from: from, to: address(""), amount: amount };
    return true;
}

fn main() -> u64 {
    let alice = address("0xa11ce");
    let bob   = address("0xb0b");
    let carol = address("0xca7a");

    init("MyToken", "MYT", 18u32);
    mint(alice, 1000u64);

    // alice → bob: 300
    transfer(alice, bob, 300u64);

    // bob delegates 100 to carol; carol pulls 60 to herself.
    approve(bob, carol, 100u64);
    transfer_from(carol, bob, carol, 60u64);

    // alice burns 50.
    burn(alice, 50u64);

    // alice: 1000 - 300 - 50 = 650
    return balance_of(alice);
}
