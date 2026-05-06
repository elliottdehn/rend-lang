// SLICE 27+: comprehensive showcase — a time-locked vault.
//
// Pulls in every recent language feature into one program:
//   - `module <name>;`                          self-naming module
//   - block context                             msg_sender(), block_timestamp()
//   - bytes type + ops                          to_bytes, bytes_concat, bytes_len
//   - tuples + destructuring                    multi-value getters
//   - `nore`                                    non-reentrant deposit/withdraw
//   - modifiers with args                       OnlyAdmin, WhenNotPaused, MinAmount
//   - events                                    Deposited / Withdrawn / etc.
//   - asserts with messages                     invariant checks at boundaries
//   - granular struct state                     `meta: Meta` → one cell per field
//   - field-path writes through struct state    `meta.paused = true;`
//
// The vault holds time-locked deposits. Each deposit gets an integer
// id and a maturity timestamp; only the depositor can redeem, and
// only after `block_timestamp() >= unlocks_at`. The contract has an
// admin who can pause new deposits in an emergency. Every state
// change emits an event; every invariant is asserted with a message
// that the host's error log will surface verbatim.

module vault;

// Per-deposit record. Lives in a `map<i64, Position>`; the runtime
// stores the whole struct as one cell per id.
struct Position {
    owner:      Address,
    amount:     u64,
    unlocks_at: u64,
    redeemed:   bool,
}

// Granular struct state — each field becomes its own KV cell at
// child(state_root("vault", "meta"), field_name). Two unrelated
// admin fields can be updated by concurrent transactions without
// conflicting under OCC.
struct Meta {
    admin:    Address,
    paused:   bool,
    total:    u64,
    next_id:  i64,
}

state meta:      Meta;
state positions: map<i64, Position>;

event Deposited(id: i64, owner: Address, amount: u64, unlocks_at: u64);
event Withdrawn(id: i64, owner: Address, amount: u64);
event PauseToggled(by: Address, paused: bool);
event ProofIssued(id: i64, proof: bytes);

// ---------- modifiers ----------

modifier OnlyAdmin() {
    assert(msg_sender() == meta.admin, "not admin");
    _;
}

modifier WhenNotPaused() {
    assert(!meta.paused, "vault is paused");
    _;
}

// Modifier with an argument — at the call site, the arg is inlined
// into every reference to `min` inside this body.
modifier MinAmount(min: u64) {
    assert(min > 0u64, "amount must be positive");
    _;
}

// ---------- admin surface ----------

entry fn init(admin: Address) {
    assert(meta.admin == address(""), "already initialized");
    meta.admin = admin;
}

entry fn pause() [OnlyAdmin] {
    meta.paused = true;
    emit PauseToggled(msg_sender(), true);
}

entry fn unpause() [OnlyAdmin] {
    meta.paused = false;
    emit PauseToggled(msg_sender(), false);
}

// ---------- depositor surface ----------

// `nore` blocks any cross-module call-back from re-entering deposit
// while it's mid-flight. `[WhenNotPaused, MinAmount(amount)]` runs
// the pause check first, then the min-amount check; both inline at
// the function's entry.
nore entry fn deposit(amount: u64, lock_duration: u64)
    [WhenNotPaused, MinAmount(amount)] -> i64
{
    let id = meta.next_id + 1;
    meta.next_id = id;
    let unlocks_at = block_timestamp() + lock_duration;
    positions[id] = Position {
        owner:      msg_sender(),
        amount:     amount,
        unlocks_at: unlocks_at,
        redeemed:   false,
    };
    meta.total = meta.total + amount;
    emit Deposited(id, msg_sender(), amount, unlocks_at);
    return id;
}

nore entry fn withdraw(id: i64) -> u64 {
    let p = positions[id];
    assert(!p.redeemed, "already redeemed");
    assert(p.owner == msg_sender(), "not your deposit");
    assert(block_timestamp() >= p.unlocks_at, "still locked");

    // Field-path write through a map cell: the runtime reads the
    // whole Position, FieldSets `redeemed`, writes back.
    positions[id].redeemed = true;
    meta.total = meta.total - p.amount;
    emit Withdrawn(id, msg_sender(), p.amount);
    return p.amount;
}

// ---------- read-only views ----------

// Multi-value return via tuple — caller destructures with
// `let (owner, amount, unlocks_at, redeemed) = position(id);`.
entry fn position(id: i64) -> (Address, u64, u64, bool) {
    let p = positions[id];
    return (p.owner, p.amount, p.unlocks_at, p.redeemed);
}

// Three-tuple snapshot of the admin block.
entry fn vault_stats() -> (u64, bool, i64) {
    return (meta.total, meta.paused, meta.next_id);
}

// Issues an opaque proof tag the depositor can hold off-chain. In a
// production vault this would be a cryptographic commitment; here
// it's just a labeled byte blob to demo the bytes ops.
entry fn issue_proof(id: i64) -> bytes {
    let p = positions[id];
    assert(!p.redeemed, "redeemed deposits have no live proof");
    let header = to_bytes("vault-proof-v1:");
    let label  = to_bytes("position");
    let proof  = bytes_concat(header, label);
    emit ProofIssued(id, proof);
    return proof;
}

// ---------- driver ----------

fn main() -> u64 {
    let admin = address("0xadmin");

    // Deploy: admin slot starts empty, init populates it.
    init(admin);

    // Make a deposit for 1000 units, locked 100s. (msg_sender() here
    // defaults to the zero-address since the example is run without
    // a host TxContext; a real host would populate it.)
    let id = deposit(1000u64, 100u64);

    // Read back via tuple destructure.
    let (_owner, amount, _unlocks_at, redeemed) = position(id);
    assert(amount == 1000u64, "amount round-trip failed");
    assert(!redeemed, "should not yet be redeemed");

    // Admin block snapshot.
    let (total, paused, last_id) = vault_stats();
    assert(total == 1000u64, "total mismatch");
    assert(!paused, "vault should be open");
    assert(last_id == 1, "first deposit should be id 1");

    // Issue an off-chain proof and check it's well-formed.
    let proof = issue_proof(id);
    assert(bytes_len(proof) > 0, "proof must be non-empty");

    return total;
}
