# rend

A programmable transactional key-value store with a typed contract
language on top. Single-node, multi-tenant via EOA-style auth,
durable through RocksDB.

The point is to be the substrate underneath things you'd otherwise
build on Postgres or SQLite — application backends with strict
ACID transactions over richly-typed state — but with a smaller
surface area and much higher throughput on the path that actually
matters (read a few cells, run some code, write a few cells,
commit).

## What's in it

Three layers, each useful by itself:

1. **Namespaced transactional KV.** RocksDB underneath, per-org
   keyspaces, group-commit pipeline that batches concurrent
   commits into one fsync. Address-as-org EOA auth means no
   separate registration step — your private key is your tenant.
2. **Persistent collections.** `pmap<K, V>` (32-way HAMT),
   `pvec<T>`, `pbtree<K, V>`. Content-addressed, structurally
   shared across versions. The HAMT exposes a 3-way merge that
   reconciles concurrent disjoint-subtree updates byte-identically
   to a serialized run — two transfers on different accounts
   commit in parallel without conflict.
3. **The rend language.** Strongly-typed, multi-statement entry
   functions, persistent-collection state declarations,
   cross-module calls, runtime asserts that abort and roll back.
   Compiles to a register-based bytecode VM. See
   `crates/rend/examples/` for source samples.

The transport story:

- **HTTP:** every request is signed (`X-Rend-Sig` over a canonical
  `keccak256("REND/v1\n" || method || "\n" || path || "\n" ||
  nonce_be_u64 || "\n" || body)`), per-EOA monotonic nonces, the
  recovered 20-byte address must equal the URL's `:org` segment.
- **WebSocket:** auth happens once at upgrade. After that, frames
  travel without per-message signatures — amortizing
  secp256k1-recover across the lifetime of the connection. Frames
  carry an opaque correlation `id` so one socket can have many
  in-flight requests.

## Quick start

```sh
# Build and run with docker compose (durable WAL fsync by default).
docker compose -f deploy/docker-compose.yml up --build

# Or directly with cargo.
cargo run --release -p rend-server

# Health check.
curl http://localhost:8080/healthz
# → ok

# Org endpoints require a signed request — see deploy/README.md for
# the wire format. The bench tool (below) is the easiest way to
# drive it.
```

## Workspace layout

```
crates/
  rend/             # language: parser, typer, bytecode VM, runtime,
                    # persistent collections, 3-way merge
  rend-rocksdb/     # RocksDB-backed Kv impl, group-commit pipeline,
                    # per-namespace mutex, EOA nonce CAS
  rend-server/      # HTTP + WS service, EOA auth middleware,
                    # /admin/stats, durability config
  rend-bench/       # load generator: HTTP / WS, transfer + counter
                    # workloads, cross-org / shared / hot-account
                    # variants, durable / non-durable
deploy/             # Dockerfile, compose, deploy notes
```

## Benchmarking

`rend-bench` runs N concurrent EOA-authed clients against a server.
Each worker has its own EOA (= its own org) by default; pass
`--shared` to put them all on one EOA so contention lands on a
single namespace.

```sh
# Cross-org transfer, 16 workers, 10 seconds, WebSocket transport.
cargo run --release -p rend-bench -- \
  --target http://localhost:8080 \
  --workers 16 --duration 10 \
  --workload transfer --ws

# Same EOA → all transfers contend on one bank's pmap root.
cargo run --release -p rend-bench -- \
  --target http://localhost:8080 \
  --workers 16 --duration 10 \
  --workload transfer --ws --shared
```

Workloads: `tx` (counter bump), `query` (counter read), `transfer`
(2-account bank transfer), `transfer-hot` (all transfers funnel
into account 0 — maximises OCC contention).

### Reference numbers (M2 Pro, single laptop, release build)

The bank workload reads two accounts, asserts the sender has
enough, and writes both. Same shape as a SQL transfer.

| transport | mode | workload | workers | ops/sec | p99 |
|---|---|---|---:|---:|---:|
| ws | durable=on (WAL fsync) | shared transfer | 16 | **31,000** | 1,350µs |
| ws | durable=on (WAL fsync) | shared transfer | 32 | **32,000** | 2,773µs |
| ws | durable=on (WAL fsync) | cross-org transfer | 32 | 50,000 | 2,635µs |
| ws | durable=off (in-memory) | shared transfer | 32 | 38,000 | 2,207µs |
| ws | durable=off (in-memory) | cross-org transfer | 32 | 64,000 | 1,343µs |
| http | durable=on | cross-org transfer | 32 | 22,000 | — |

The shared-org number is the load-bearing one — it's what you'd
hit if your whole application uses a single bank/ledger contract.
Cross-org is the upper bound when contracts are independent (each
tenant's own state).

The path to the shared-30K-with-durability number was a sequence:

- **EOA auth + WS streaming** (one signature verify per
  connection, not per message) → amortized sig verify out of the
  hot path.
- **Pmap 3-way merge wired into the commit path** → two transfers
  on disjoint accounts merge to a valid combined root instead of
  one of them 409ing.
- **Per-namespace mutex commit path** → drop OCC machinery we were
  duplicating ourselves; replace with a `Mutex<()>` held for ~10µs.
  Cross-org went up, shared went up much more.
- **Group commit** → per-namespace tokio task drains a channel,
  batches up to 64 jobs, one fsync per batch. The shared-durable
  collapse from 30K (in-memory) to 2.5K (fsync per commit) became
  30K → 30K — fsync amortizes across the batch.

## Status

What works:

- All of the above. ~850 unit and integration tests pass.
- Stress test that fires 1000 concurrent transfers across 4
  accounts and verifies total balance is conserved.
- The bank, counter, and query workloads under load.

What's not in it:

- **Replication.** Single-node only. RocksDB's WAL gives durability
  on disk; failure recovery means restoring the volume.
- **Backups.** RocksDB checkpoints are the right answer; no
  ergonomic API yet.
- **Metrics.** Per-stage timings via `GET /admin/stats`; no
  Prometheus scrape endpoint yet.
- **Compile cache.** Each tx submission re-parses + re-lowers its
  source. ~25µs/req we don't need to pay; haven't fixed yet.
- **Public KV / collections endpoints.** All client access today
  goes through a rend tx. Exposing the substrate directly (raw KV,
  pmap operations) is the natural next slice.
