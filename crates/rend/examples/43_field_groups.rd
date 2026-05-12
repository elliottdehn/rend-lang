// Struct field grouping: control storage granularity per-field.
//
// By default every struct field of a top-level state struct lives
// in its own KV cell — that's what makes two transactions touching
// disjoint fields conflict-free under OCC. But sometimes the
// "always granular" default is wasteful: a profile blob that's
// always read together as a unit pays N round-trips for what
// could be one. Use `group <name> { fields }` to opt that subset
// into shared storage.
//
// Layout consequences:
//
//   struct U {
//       balance: u64,               // hot — granular cell `balance`
//       group profile {             // cold — single blob cell `profile`
//           name: string,
//           email: string,
//           created_at: u64,
//       },
//   }
//
//   state u: U;
//
//   u.balance = u.balance + 1;      // RMW touches only `balance`
//   u.name = "...";                 // RMW touches only `profile`
//
// Reads: granular fields are read by their own cell key;
// grouped-field reads load the group blob and project the field.
// Writes through a grouped field are read-modify-write of the
// group blob — granular siblings outside the group are left
// alone.
//
// Caveat: a group is the unit of storage. If a struct inside a
// group itself declares groups, those inner annotations are
// inert — the outer cell already swallowed the subtree.

module accounts;

struct U {
    // Hot field: bumped on every transfer. Stays granular so
    // two transfers to different users don't pile up on a
    // shared cell.
    balance: u64,
    // Cold fields: only touched on registration / edits. One
    // cell for all three keeps the profile read down to a
    // single round-trip when product code wants to render the
    // whole thing.
    group profile {
        name: string,
        email: string,
        created_at: u64,
    },
}

state alice: U;
state bob: U;

entry fn register_alice(name: string, email: string, at: u64) {
    alice = U {
        balance: 0u64,
        name: name,
        email: email,
        created_at: at,
    };
}

entry fn credit_alice(amount: u64) {
    alice.balance = alice.balance + amount;
}

entry fn update_email(new_email: string) {
    // RMW: loads the `profile` group cell, sets one field,
    // writes it back. `balance` is not touched.
    alice.email = new_email;
}

entry view fn alice_email() -> string {
    return alice.email;
}

fn main() -> u64 {
    register_alice("alice", "a@example.com", 1u64);
    credit_alice(100u64);
    credit_alice(50u64);
    update_email("alice@example.com");

    // Cross-account write doesn't conflict with alice's writes
    // because bob lives at a disjoint state root.
    bob = U {
        balance: 9u64,
        name: "bob",
        email: "b@example.com",
        created_at: 2u64,
    };

    // Sanity: alice's balance (150) is reachable via the granular
    // cell; the updated email is reachable via the group projection.
    let balance = alice.balance;
    let email_ok = alice_email() == "alice@example.com";

    if email_ok {
        return balance + bob.balance;   // 150 + 9
    }
    return 0u64;
}
