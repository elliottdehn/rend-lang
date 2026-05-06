// SLICES 36–40+ combined: native JSON, delete, sorted/unique/multi
// indexes, streaming iteration, range queries, aggregations.
//
// Example: an event log fed by opaque JSON payloads from a host.
// The contract receives raw event JSON, parses it once at the trust
// boundary, extracts the typed fields it wants to index, and stores
// the payload alongside as `json` for later inspection.
//
// Three distinct access patterns covered by three indexes:
//
//   * `unique_index by_seq` over a `pbtree<u64, u64>` — every event
//     has a strictly-increasing sequence number. Sorted index gives
//     range queries (paginate over recent events) and "smallest /
//     largest" without scanning the primary.
//
//   * `index by_user` over `pmap<u64, [u64]>` — many events per
//     user; the multi-index collects ids per user. Hash-ordered is
//     fine here since lookups are point queries by user id.
//
//   * `index by_kind` over `pmap<string, [u64]>` — same shape,
//     keyed by the event kind ("login", "purchase", ...).
//
// `delete_event(id)` removes an event from the primary AND
// auto-cleans all three back-links — no risk of stale index entries.
//
// JSON paths (`payload -> user -> id`) are how we extract fields at
// insert time and at query time. The runtime never re-parses the
// JSON text — it's stored in jsonb-shaped binary form, navigated
// directly.
//
// Every `view fn` below is callable on the read-only `Engine::query`
// path: no OCC bookkeeping, no write log, just walk-and-read.

module event_log;

struct Event {
    id:        u64,
    seq:       u64,
    user_id:   u64,
    kind:      string,
    timestamp: u64,
    payload:   json,    // the opaque blob from the host
}

state events:    pmap<u64, Event>;
state next_seq:  u64;

state by_seq:    pbtree<u64, u64>;       // sorted unique: seq → id
state by_user:   pmap<u64, [u64]>;       // multi: user_id → [id]
state by_kind:   pmap<string, [u64]>;    // multi: kind → [id]

unique_index by_seq  on events.seq;
index        by_user on events.user_id;
index        by_kind on events.kind;

// ---------- ingest ----------

// Receive a raw JSON event from the host. Parse once, project the
// indexed fields, store the parsed payload alongside. The compiler
// auto-emits maintenance for all three indexes — there's no second
// statement to forget.
//
// Expected JSON shape (buyer beware: missing fields abort):
//   { "user": { "id": <u64> },
//     "kind": "<string>",
//     "timestamp": <u64>,
//     ...other arbitrary fields, kept opaque... }
entry fn record(id: u64, raw: string) -> u64 {
    let payload = parse_json(raw);
    let seq = next_seq + 1u64;
    next_seq = seq;

    events[id] = Event {
        id:        id,
        seq:       seq,
        user_id:   json_to_u64(payload -> user -> id),
        kind:      json_to_string(payload -> kind),
        timestamp: json_to_u64(payload -> timestamp),
        payload:   payload,
    };
    return seq;
}

// Remove an event. The compiler-emitted cleanup walks all three
// indexes and removes the back-links pointing at this id.
entry fn delete_event(id: u64) -> u64 {
    delete events[id];
    return 1u64;
}

// ---------- queries: indexed lookup ----------

// O(log32 N) walk through the sorted index, then one read in
// `events`. SQL: `SELECT * FROM events WHERE seq = ?`.
entry view fn find_by_seq(seq: u64) -> Event {
    let id = by_seq[seq];
    return events[id];
}

// SQL: `SELECT * FROM events WHERE user_id = ?`. Multi-index gives
// the list of ids; we then fetch each record. M reads instead of N.
entry view fn events_for_user(user_id: u64) -> [Event] {
    return [events[id] for id in by_user[user_id]];
}

entry view fn events_of_kind(kind: string) -> [Event] {
    return [events[id] for id in by_kind[kind]];
}

// ---------- queries: range over the sorted index ----------

// Pagination via the sorted index. SQL: `SELECT * FROM events
// WHERE seq BETWEEN ? AND ? ORDER BY seq`. The pbtree walks only
// the leaves covering the range; out-of-range subtrees stay
// unfetched.
entry view fn page(after_seq: u64, n: u64) -> [Event] {
    let ids = pbtree_range(by_seq, after_seq, after_seq + n);
    return [events[id] for id in ids];
}

// SQL: `SELECT * FROM events ORDER BY seq DESC LIMIT 1`. Streams
// the sorted index and stops at the first matching id. `break`
// after the first iteration — the cursor never visits the rest.
//
// Note: pbtree iterates ascending. For "most recent" we'd need
// descending iteration, which we don't have yet; this returns the
// *earliest* event instead. Real apps would query a known-recent
// seq via `find_by_seq(next_seq)`.
entry view fn earliest() -> Event {
    for id in by_seq {
        return events[id];
    }
    return events[0u64];
}

// ---------- queries: aggregation ----------

// SQL: `SELECT COUNT(*) FROM events WHERE user_id = ?`.
// Counting via the index is constant — list length, no scan.
entry view fn count_for_user(user_id: u64) -> i64 {
    return len(by_user[user_id]);
}

// SQL: `SELECT SUM(amount) FROM events WHERE user_id = ?
//        AND kind = 'purchase'`. The amount field lives in the
// opaque payload; we navigate via `->` per event.
entry view fn total_purchase_amount(user_id: u64) -> i64 {
    let sum_amount = 0;
    for id in by_user[user_id] {
        let e = events[id];
        if e.kind == "purchase" {
            sum_amount = sum_amount + json_to_i64(e.payload -> amount);
        }
    }
    return sum_amount;
}

// ---------- queries: full-table scan with predicate ----------

// SQL: `SELECT * FROM events WHERE timestamp > ?`. No index covers
// timestamp — so we stream the primary and filter. Comprehension
// streams the source (no materialization of the full pmap up front);
// only the result array materializes incrementally.
entry view fn since(threshold: u64) -> [Event] {
    return [e for e in events if e.timestamp > threshold];
}

// ---------- queries: extract opaque payload field ----------

// Late-bind a JSON path that wasn't pre-extracted. Path components
// must be literal idents, quoted strings, or `[expr]` integer
// indices — computed string keys aren't supported in `->` syntax;
// `json_get_field(j, computed_key)` is the workaround.
//
// SQL: `SELECT payload->>'session_id' FROM events WHERE id = ?`.
entry view fn session_id_for(id: u64) -> string {
    return json_to_string(events[id].payload -> session_id);
}

// ---------- constructor: seed sample data ----------

// Deploy-time setup. The constructor records a few events so a
// fresh KV is non-empty, lets the host smoke-test queries, and
// demonstrates the round-trip from raw JSON → parsed Event.
fn main() -> u64 {
    record(
        1u64,
        "{\"user\": {\"id\": 100}, \"kind\": \"login\", \"timestamp\": 1700000000, \"session_id\": \"s1\"}",
    );
    record(
        2u64,
        "{\"user\": {\"id\": 100}, \"kind\": \"purchase\", \"timestamp\": 1700000060, \"amount\": 25, \"session_id\": \"s1\"}",
    );
    record(
        3u64,
        "{\"user\": {\"id\": 200}, \"kind\": \"login\", \"timestamp\": 1700000120, \"session_id\": \"s2\"}",
    );
    record(
        4u64,
        "{\"user\": {\"id\": 100}, \"kind\": \"purchase\", \"timestamp\": 1700000180, \"amount\": 50, \"session_id\": \"s1\"}",
    );
    record(
        5u64,
        "{\"user\": {\"id\": 200}, \"kind\": \"logout\", \"timestamp\": 1700000240, \"session_id\": \"s2\"}",
    );
    return next_seq;
}
