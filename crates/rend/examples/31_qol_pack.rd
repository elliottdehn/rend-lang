// SLICE 28: the quality-of-life pack — range loops, bitwise ops,
// module-level `const`, and string builtins, all in one example.
//
// We model a permission/flags-based access manager:
//
//   - Each user has a u64 bitfield of role flags.
//   - Each role is named with a `const` and assigned a single bit.
//   - Granting / revoking a role is a single bitwise op.
//   - We check membership by AND-ing the flag against the bitfield.
//   - We iterate role slots with a range loop to render a textual
//     summary of who-has-what for an off-chain UI.
//
// The point isn't that any one feature is fancy. It's that all four
// of them line up to express something clear and short.

module access;

// ---- role flags -------------------------------------------------
// Using bitwise OR-able flags lets a single u64 carry up to 64
// independent role assignments. Each flag is one bit.

const ROLE_NONE:   u64 = 0u64;
const ROLE_READ:   u64 = 1u64;       // bit 0
const ROLE_WRITE:  u64 = 2u64;       // bit 1
const ROLE_ADMIN:  u64 = 4u64;       // bit 2
const ROLE_BILL:   u64 = 8u64;       // bit 3
const ROLE_AUDIT:  u64 = 16u64;      // bit 4

// How many role bits we know how to render. Used as the upper
// bound of a range loop below.
const ROLE_COUNT: u64 = 5u64;

// Sentinel mask: union of every defined role. Useful for
// "did the caller hand us a flag we don't know about?" checks.
const ROLE_ALL: u64 = 31u64;         // 1 | 2 | 4 | 8 | 16

// ---- state ------------------------------------------------------

state roles: map<Address, u64>;

// ---- modifiers --------------------------------------------------
// Constants flow naturally into modifier args.

modifier OnlyRole(needed: u64) {
    let mine = roles[msg_sender()];
    assert((mine & needed) == needed);
    _;
}

// ---- entry points -----------------------------------------------

entry fn grant(who: Address, role: u64) [OnlyRole(ROLE_ADMIN)] -> u64 {
    // Reject unknown bits up front: anything outside ROLE_ALL
    // would silently survive the OR otherwise. `(role | ROLE_ALL)
    // == ROLE_ALL` is true iff every bit of `role` is already in
    // ROLE_ALL — the no-`~` way to ask "is `role` a subset?".
    assert((role | ROLE_ALL) == ROLE_ALL);
    let cur = roles[who];
    roles[who] = cur | role;
    return roles[who];
}

entry fn revoke(who: Address, role: u64) [OnlyRole(ROLE_ADMIN)] -> u64 {
    let cur = roles[who];
    // To clear the bits in `role`, AND with "all known bits
    // except those in role". `ROLE_ALL ^ role` is exactly that
    // mask (assuming role ⊆ ROLE_ALL, enforced on grant).
    roles[who] = cur & (ROLE_ALL ^ role);
    return roles[who];
}

entry fn has_role(who: Address, role: u64) -> bool {
    let mine = roles[who];
    return (mine & role) == role;
}

// Render a human-readable summary like
//   "READ|WRITE"
// for the given user. Iterates over each known role bit using a
// range loop, masks against the bitfield, and string_concat's the
// names with a separator.
entry fn describe(who: Address) -> string {
    let mine = roles[who];
    let out = "";
    let first = true;
    for slot in 0u64..ROLE_COUNT {
        let bit = 1u64 << slot;
        if (mine & bit) == bit {
            let name = role_name(bit);
            if first {
                out = name;
                first = false;
            } else {
                out = string_concat(out, "|");
                out = string_concat(out, name);
            }
        }
    }
    if first {
        return "(none)";
    }
    return out;
}

// String op + bitwise compare — the kind of small lookup that
// wants the new builtins more than it wants a struct.
fn role_name(bit: u64) -> string {
    if bit == ROLE_READ  { return "READ"; }
    if bit == ROLE_WRITE { return "WRITE"; }
    if bit == ROLE_ADMIN { return "ADMIN"; }
    if bit == ROLE_BILL  { return "BILL"; }
    if bit == ROLE_AUDIT { return "AUDIT"; }
    return "?";
}

// Quick sanity check: does the rendered description mention a
// given role name? Combines string_contains with the iteration
// above. Useful for off-chain UIs that don't want to re-implement
// the bitwise check.
entry fn description_contains(who: Address, role_name: string) -> bool {
    let s = describe(who);
    return string_contains(s, role_name);
}

// ---- demo -------------------------------------------------------

fn main() -> string {
    // Set up: bootstrap msg_sender as ADMIN so they can grant.
    roles[msg_sender()] = ROLE_ADMIN;

    let alice = address("alice");
    let bob   = address("bob");

    grant(alice, ROLE_READ | ROLE_WRITE);
    grant(bob,   ROLE_READ | ROLE_AUDIT);

    // Should be "READ|WRITE"
    return describe(alice);
}
