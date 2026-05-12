// Module `proposals` — proposal lifecycle: propose → vote → execute.
//
// Cross-module reads of `members::power_of` gate every privileged op:
// non-members can't propose or vote. The `voted` map uses a composite
// struct key `(proposal_id, voter)` so each member can vote at most
// once per proposal — but is free to vote on a different proposal
// without state collision.
//
// Vote-mutation walks `book[id].yes_votes = ...` (field-path write
// through a map cell). The cell is one Loan-shaped blob in the KV;
// the runtime reads it, FieldSets, writes back.
//
// Every transition emits an event so an off-chain indexer can rebuild
// the timeline without re-querying state.

module proposals;

struct Proposal {
    proposer:  Address,
    target:    Address,
    amount:    u64,
    yes_votes: u64,
    no_votes:  u64,
    executed:  bool,
    passed:    bool,
}

struct VoteKey { proposal: i64, voter: Address }

state book:    map<i64, Proposal>;
state next_id: i64;
state voted:   map<VoteKey, bool>;

// Events are struct values; `emit Foo { ... }` logs them.
struct Proposed { id: i64, proposer: Address, target: Address, amount: u64 }
struct Voted    { id: i64, voter: Address, support: bool, weight: u64 }
struct Executed { id: i64, passed: bool, yes: u64, no: u64 }

entry fn propose(proposer: Address, target: Address, amount: u64) -> i64 {
    assert(members::is_member(proposer), "only members can propose");
    let id = next_id + 1;
    next_id = id;
    book[id] = Proposal {
        proposer:  proposer,
        target:    target,
        amount:    amount,
        yes_votes: 0u64,
        no_votes:  0u64,
        executed:  false,
        passed:    false,
    };
    emit Proposed {
        id: id, proposer: proposer, target: target, amount: amount,
    };
    return id;
}

entry fn vote(id: i64, voter: Address, support: bool) -> bool {
    let p = book[id];
    assert(!p.executed, "proposal already executed");
    let power = members::power_of(voter);
    assert(power > 0u64, "not a member");

    let k = VoteKey { proposal: id, voter: voter };
    assert(!voted[k], "already voted");
    voted[k] = true;

    if support {
        book[id].yes_votes = p.yes_votes + power;
    } else {
        book[id].no_votes = p.no_votes + power;
    }
    emit Voted { id: id, voter: voter, support: support, weight: power };
    return true;
}

entry fn execute(id: i64) -> bool {
    let p = book[id];
    assert(!p.executed, "already executed");
    let passed = p.yes_votes > p.no_votes;
    book[id].executed = true;
    book[id].passed   = passed;
    emit Executed { id: id, passed: passed, yes: p.yes_votes, no: p.no_votes };
    return passed;
}

entry fn proposal(id: i64) -> Proposal { return book[id]; }
entry fn yes_votes(id: i64) -> u64    { return book[id].yes_votes; }
entry fn no_votes(id: i64)  -> u64    { return book[id].no_votes; }
entry fn is_executed(id: i64) -> bool { return book[id].executed; }
entry fn has_passed(id: i64)  -> bool { return book[id].passed; }
