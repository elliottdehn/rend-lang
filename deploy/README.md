# Deploying rend-server

A multi-tenant HTTP server exposing the rend runtime, backed by RocksDB.
Each org gets a disjoint key namespace; commits across orgs proceed in
parallel.

## Run locally

```sh
docker compose -f deploy/docker-compose.yml up --build
```

Server listens on `localhost:8080` with a persistent `rend-data` volume
for RocksDB.

## Sanity check

```sh
curl http://localhost:8080/healthz
# → ok

curl -X POST http://localhost:8080/v1/orgs/acme/compile \
  -H 'content-type: application/json' \
  -d '{"source": "module hello; fn main() -> i64 { return 42; }"}'
# → { "hash": "...", "bytes": "...", "modules": ["hello"] }
```

## Endpoints

| Method | Path | Body | Returns |
|---|---|---|---|
| `GET` | `/healthz` | — | `ok` |
| `POST` | `/v1/orgs/:org/compile` | `{ source }` | compiled artifact (hash + hex bytes + module names) |
| `POST` | `/v1/orgs/:org/deploy` | `{ hash }` | runs the constructor; returns its result + writes applied |
| `POST` | `/v1/orgs/:org/tx` | `{ tx_source, deps: [hash...] }` | executes a tx; returns result + writes_applied + events |
| `POST` | `/v1/orgs/:org/query` | `{ tx_source, deps: [hash...] }` | runs a `view`/`pure` tx on the read-only path; returns result |
| `GET` | `/v1/orgs/:org/artifacts/:hash` | — | raw artifact bytes |

## Configuration

Environment variables (with defaults):

| Var | Default | Meaning |
|---|---|---|
| `REND_LISTEN` | `0.0.0.0:8080` | bind address |
| `REND_DATA_DIR` | `/data` | RocksDB directory |
| `REND_FUEL` | `2000000` | per-tx fuel cap |
| `RUST_LOG` | `rend_server=info,tower_http=info` | tracing filter |

## Concurrency model

- Reads (`/query`) take **no locks**. Many concurrent queries against the
  same org commit no state, so they don't conflict with each other or with
  concurrent writers (modulo snapshot consistency, which the underlying
  RocksDB read provides).
- Writes (`/deploy`, `/tx`) take a **per-org commit mutex**. Commits within
  one org serialize; cross-org commits run in parallel.
- The rend runtime's OCC + 3-way merge handles disjoint-subtree concurrent
  edits inside a single commit (e.g., two indexed writes touching different
  HAMT subtrees coalesce without conflict).

## What's not in v1

- **Auth.** Anyone with network access can hit any org's endpoints. Bring
  your own reverse proxy (caddy / nginx / Cloudflare) with mTLS or bearer
  tokens until the first-party auth slice lands.
- **Replication.** Single-node only. RocksDB's WAL gives durability;
  failure recovery means restoring the volume.
- **Backups.** RocksDB checkpoints / snapshots are the right answer; no
  ergonomic API yet.
- **Metrics.** Tracing logs only; no Prometheus scrape endpoint yet.
- **Retry policy on commit conflicts.** Currently the per-org commit
  mutex serializes writes within an org, so OCC conflicts within the
  server don't happen. Cross-process / cross-server scenarios would
  need optimistic-retry — out of scope for single-node v1.
