// Audit module — observes the `bank` module's transfers without
// the bank knowing it exists. The handler runs in the same tx
// as the emitting transfer, so the audit log is atomically
// consistent with the balance writes.

module audit;

// Local struct kept private (no `pub`) — only `audit` mints log
// entries; consumers read them through `entry_at`.
struct LogEntry { from: Address, to: Address, amount: u64, seq: u64 }

state log:      pmap<u64, LogEntry>;
state next_seq: u64;

// Cross-module handler: bound to `bank::Transferred`. Fires
// whenever `bank` emits a Transferred value, anywhere in the tx.
// The handler's parameter type must match the qualified event
// type — `e: bank::Transferred` is checked at typeck.
on bank::Transferred fn record(e: bank::Transferred) {
    let seq = next_seq + 1u64;
    next_seq = seq;
    log[seq] = LogEntry {
        from:   e.from,
        to:     e.to,
        amount: e.amount,
        seq:    seq,
    };
}

entry view fn entry_count() -> u64 { return next_seq; }
entry view fn entry_at(seq: u64) -> LogEntry { return log[seq]; }
