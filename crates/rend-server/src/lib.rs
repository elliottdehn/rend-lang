//! HTTP service exposing the rend runtime.
//!
//! ## Surface
//!
//! All endpoints are scoped to an org namespace via the URL path
//! `/v1/orgs/:org/...`. The server keeps each org's state cells in
//! a disjoint key range in RocksDB (see `rend-rocksdb`); two orgs
//! cannot read or write each other's cells.
//!
//! ```text
//! POST /v1/orgs/:org/compile             { source: "..." }
//!   → 200 { hash, bytes, modules: [...] }      // compiled artifact
//!
//! POST /v1/orgs/:org/deploy              { hash }
//!   → 200 { result, writes_applied }            // runs the constructor
//!
//! POST /v1/orgs/:org/tx                  { tx_source, deps: [hash, ...] }
//!   → 200 { result, writes_applied, events }    // compile + execute_tx
//!
//! POST /v1/orgs/:org/query               { tx_source, deps: [hash, ...] }
//!   → 200 { result }                            // read-only path
//!
//! GET  /v1/orgs/:org/artifacts/:hash
//!   → 200 (raw bytes, application/octet-stream)
//!
//! GET  /healthz
//!   → 200 "ok"
//! ```
//!
//! ## Concurrency
//!
//! - Reads (`/query`) take no locks. Many concurrent queries against
//!   one org commit zero state, so they never conflict.
//! - Writes (`/deploy`, `/tx`) take a per-org commit mutex. Inside
//!   the mutex: execute the rend program, validate OCC reads against
//!   live state, apply writes. Cross-org commits are independent.
//! - There is no global write lock — orgs commit in parallel.
//!
//! ## Artifacts
//!
//! Artifacts are stored as raw bytes in RocksDB under reserved cell
//! keys derived from the artifact's content hash, in a per-org
//! namespace adjacent to the state cells. Deploying re-uses the
//! same content-addressed identity rend's frontend produces — so
//! "deploy" is effectively "register the bytes + run the
//! constructor."

use anyhow::Context;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use rend::artifact::Artifact;
use rend::value::Value;
use rend::Engine;
use rend_rocksdb::{CommitResult, NamespaceId, RocksKv};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

/// How many times we re-execute a tx after an OCC conflict before
/// surfacing a 409 to the caller. In practice contention >5 means
/// either pathological hot keys or an attack — the caller should
/// back off, not the server.
const COMMIT_MAX_ATTEMPTS: usize = 5;

#[derive(Debug, Clone)]
pub struct Config {
    pub listen: String,
    pub data_dir: PathBuf,
    pub fuel: u64,
}

/// Long-lived server state. Cheap to clone (`Arc` inside).
#[derive(Clone)]
pub struct ServerState {
    inner: Arc<Inner>,
}

struct Inner {
    kv: RocksKv,
    fuel: u64,
}

impl ServerState {
    pub fn new(kv: RocksKv, fuel: u64) -> Self {
        Self { inner: Arc::new(Inner { kv, fuel }) }
    }

    fn ns_for(&self, org: &str) -> NamespaceId {
        // Stable u64 hash of the org name. We don't need
        // cryptographic strength — just disjointness across orgs.
        // FxHash or fnv would do; the rend hashing crate is
        // already in scope so we reuse it for a single deterministic
        // implementation.
        let bytes = org.as_bytes();
        let h128 = rend::hashing::child(0, bytes);
        // Fold to u64. Truncation is fine for namespace separation —
        // collisions across distinct org names would be extraordinary,
        // and the cell layer's content-addressed cells are separate
        // from the namespace prefix.
        h128 as u64
    }
}

pub async fn serve(config: Config) -> anyhow::Result<()> {
    std::fs::create_dir_all(&config.data_dir)
        .with_context(|| format!("creating data dir {:?}", config.data_dir))?;
    let kv = RocksKv::open(&config.data_dir)
        .with_context(|| format!("opening RocksDB at {:?}", config.data_dir))?;
    let state = ServerState::new(kv, config.fuel);

    let listener = tokio::net::TcpListener::bind(&config.listen).await?;
    tracing::info!(listen = %config.listen, "rend-server up");
    axum::serve(listener, router(state)).await?;
    Ok(())
}

/// Build the axum `Router` over a prepared `ServerState`. Exposed
/// for embedding in tests / alternative entry points where the
/// caller binds its own `TcpListener`.
pub fn router(state: ServerState) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/v1/orgs/:org/compile", post(compile))
        .route("/v1/orgs/:org/deploy", post(deploy))
        .route("/v1/orgs/:org/tx", post(submit_tx))
        .route("/v1/orgs/:org/query", post(submit_query))
        .route("/v1/orgs/:org/artifacts/:hash", get(get_artifact))
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .with_state(state)
}

// ---------- error type ----------

#[derive(Debug, thiserror::Error)]
enum ServerError {
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error("not found")]
    NotFound,
    #[error("commit failed after {COMMIT_MAX_ATTEMPTS} retries due to OCC conflicts")]
    CommitConflict,
    #[error("rend error: {0}")]
    Rend(#[from] rend::Error),
    #[error("rocksdb error: {0}")]
    RocksDb(#[from] rocksdb::Error),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl IntoResponse for ServerError {
    fn into_response(self) -> Response {
        let (status, msg) = match &self {
            ServerError::BadRequest(s) => (StatusCode::BAD_REQUEST, s.clone()),
            ServerError::NotFound => (StatusCode::NOT_FOUND, "not found".into()),
            ServerError::CommitConflict => (StatusCode::CONFLICT, self.to_string()),
            ServerError::Rend(e) => (StatusCode::UNPROCESSABLE_ENTITY, e.to_string()),
            ServerError::RocksDb(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
            ServerError::Other(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        };
        (status, Json(serde_json::json!({ "error": msg }))).into_response()
    }
}

type ServerResult<T> = Result<T, ServerError>;

// ---------- compile ----------

#[derive(Deserialize)]
pub struct CompileBody {
    pub source: String,
}

#[derive(Serialize)]
pub struct CompileResponse {
    pub hash: String,
    /// Hex-encoded artifact bytes. The client can store these
    /// locally and avoid re-uploading the source on every deploy.
    pub bytes: String,
    pub modules: Vec<String>,
}

async fn compile(
    State(state): State<ServerState>,
    Path(org): Path<String>,
    Json(body): Json<CompileBody>,
) -> ServerResult<Json<CompileResponse>> {
    let artifact = Engine::new()
        .compile(&body.source)
        .map_err(ServerError::Rend)?;
    let hash = format!("{:032x}", artifact.content_hash);
    let bytes = hex::encode(&artifact.bytes);
    let modules = artifact.modules.iter().map(|m| m.name.clone()).collect();
    // Store the artifact bytes under a namespaced key so deploy/tx
    // can fetch them later. Same org → same artifact storage scope.
    let ns = state.ns_for(&org);
    write_artifact(&state.inner.kv, ns, &artifact)?;
    Ok(Json(CompileResponse { hash, bytes, modules }))
}

// ---------- deploy ----------

#[derive(Deserialize)]
pub struct DeployBody {
    pub hash: String,
}

#[derive(Serialize)]
pub struct DeployResponse {
    pub result: serde_json::Value,
    pub writes_applied: usize,
}

async fn deploy(
    State(state): State<ServerState>,
    Path(org): Path<String>,
    Json(body): Json<DeployBody>,
) -> ServerResult<Json<DeployResponse>> {
    let ns = state.ns_for(&org);
    let artifact = read_artifact(&state.inner.kv, ns, &body.hash)?
        .ok_or(ServerError::NotFound)?;

    // Constructor (`fn main`) reads + writes state — full OCC
    // commit path. Retry on conflict like a regular tx.
    for _ in 0..COMMIT_MAX_ATTEMPTS {
        let view = state.inner.kv.namespace(ns);
        let outcome = Engine::new()
            .deploy(&artifact, rend::Fuel::new(state.inner.fuel), &view)
            .map_err(ServerError::Rend)?;
        let Some(outcome) = outcome else {
            // Module without a constructor — nothing to commit.
            return Ok(Json(DeployResponse {
                result: serde_json::Value::Null,
                writes_applied: 0,
            }));
        };
        let writes_n = outcome.writes.len();
        match state.inner.kv.commit_with_occ(ns, &outcome.reads, &outcome.writes)? {
            CommitResult::Committed => {
                return Ok(Json(DeployResponse {
                    result: value_to_json(&outcome.result),
                    writes_applied: writes_n,
                }));
            }
            CommitResult::Conflict => continue,
        }
    }
    Err(ServerError::CommitConflict)
}

// ---------- submit tx ----------

#[derive(Deserialize)]
pub struct TxBody {
    pub tx_source: String,
    #[serde(default)]
    pub deps: Vec<String>,
}

#[derive(Serialize)]
pub struct TxResponse {
    pub result: serde_json::Value,
    pub writes_applied: usize,
    pub events: Vec<EventRecord>,
}

#[derive(Serialize)]
pub struct EventRecord {
    pub module: String,
    pub name: String,
    pub args: Vec<serde_json::Value>,
}

async fn submit_tx(
    State(state): State<ServerState>,
    Path(org): Path<String>,
    Json(body): Json<TxBody>,
) -> ServerResult<Json<TxResponse>> {
    let ns = state.ns_for(&org);
    let deps = resolve_deps(&state.inner.kv, ns, &body.deps)?;
    let tx = Engine::new()
        .compile_tx(&body.tx_source, &deps)
        .map_err(ServerError::Rend)?;

    // OCC retry loop: re-execute the rend tx if a concurrent
    // committer wrote to any cell we read. Disjoint-cell txs
    // commit in parallel — neither's read set overlaps the
    // other's write set, so neither fires this branch.
    for _ in 0..COMMIT_MAX_ATTEMPTS {
        let view = state.inner.kv.namespace(ns);
        let outcome = Engine::new()
            .execute_tx(&tx, &deps, rend::Fuel::new(state.inner.fuel), &view)
            .map_err(ServerError::Rend)?;
        let writes_n = outcome.writes.len();
        match state.inner.kv.commit_with_occ(ns, &outcome.reads, &outcome.writes)? {
            CommitResult::Committed => {
                let events = outcome.events.iter().map(|e| EventRecord {
                    module: e.module.clone(),
                    name: e.name.clone(),
                    args: e.args.iter().map(value_to_json).collect(),
                }).collect();
                return Ok(Json(TxResponse {
                    result: value_to_json(&outcome.result),
                    writes_applied: writes_n,
                    events,
                }));
            }
            CommitResult::Conflict => continue,
        }
    }
    Err(ServerError::CommitConflict)
}

// ---------- submit query (read-only) ----------

#[derive(Serialize)]
pub struct QueryResponse {
    pub result: serde_json::Value,
}

async fn submit_query(
    State(state): State<ServerState>,
    Path(org): Path<String>,
    Json(body): Json<TxBody>,
) -> ServerResult<Json<QueryResponse>> {
    let ns = state.ns_for(&org);
    let deps = resolve_deps(&state.inner.kv, ns, &body.deps)?;
    let tx = Engine::new()
        .compile_tx(&body.tx_source, &deps)
        .map_err(ServerError::Rend)?;
    // No org lock: queries don't write. The view of the database
    // is "whatever's committed at this instant"; concurrent writes
    // race against the read but that's expected behavior — same
    // semantics SQLite gives you on a read-only handle.
    let view = state.inner.kv.namespace(ns);
    let outcome = Engine::new()
        .query(&tx, &deps, rend::Fuel::new(state.inner.fuel), &view)
        .map_err(ServerError::Rend)?;
    Ok(Json(QueryResponse { result: value_to_json(&outcome.result) }))
}

// ---------- get artifact ----------

async fn get_artifact(
    State(state): State<ServerState>,
    Path((org, hash)): Path<(String, String)>,
) -> ServerResult<Response> {
    let ns = state.ns_for(&org);
    let artifact = read_artifact(&state.inner.kv, ns, &hash)?
        .ok_or(ServerError::NotFound)?;
    Ok((
        [(axum::http::header::CONTENT_TYPE, "application/octet-stream")],
        artifact.bytes,
    ).into_response())
}

// ---------- artifact storage ----------
//
// Artifacts live in the same namespaced cell space as state, but
// under a tag-derived key so the runtime never collides with them.
// We read them out by hash on demand.

const ARTIFACT_NS_TAG: u128 = 0x4152_5449_4641_4354_4152_5449_4641_4354;

fn artifact_cell(hash: u128) -> u128 {
    rend::hashing::child(ARTIFACT_NS_TAG, &hash.to_be_bytes())
}

fn write_artifact(kv: &RocksKv, ns: NamespaceId, artifact: &Artifact) -> ServerResult<()> {
    let cell = artifact_cell(artifact.content_hash);
    let mut writes = HashMap::new();
    writes.insert(cell, Value::Bytes(artifact.bytes.clone()));
    kv.apply(ns, &writes)?;
    Ok(())
}

fn read_artifact(kv: &RocksKv, ns: NamespaceId, hex_hash: &str) -> ServerResult<Option<Artifact>> {
    let hash = parse_hash(hex_hash)?;
    let cell = artifact_cell(hash);
    let view = kv.namespace(ns);
    let Some(bytes) = <rend_rocksdb::NamespacedKv as rend::kv::Kv>::get(&view, cell) else {
        return Ok(None);
    };
    let value = rend::serialize::deserialize(&bytes, &rend::ast::Type::Bytes)
        .ok_or_else(|| ServerError::BadRequest("corrupt artifact in storage".into()))?;
    let Value::Bytes(art_bytes) = value else {
        return Err(ServerError::BadRequest("artifact cell has wrong tag".into()));
    };
    let modules = rend::artifact::decode(&art_bytes)
        .map_err(|e| ServerError::BadRequest(format!("artifact decode failed: {e}")))?;
    Ok(Some(Artifact {
        content_hash: hash,
        bytes: art_bytes,
        modules,
    }))
}

fn resolve_deps(kv: &RocksKv, ns: NamespaceId, hashes: &[String]) -> ServerResult<Vec<Artifact>> {
    let mut out = Vec::with_capacity(hashes.len());
    for h in hashes {
        let art = read_artifact(kv, ns, h)?
            .ok_or_else(|| ServerError::BadRequest(format!("dep artifact {h} not found")))?;
        out.push(art);
    }
    Ok(out)
}

fn parse_hash(s: &str) -> Result<u128, ServerError> {
    if s.len() != 32 {
        return Err(ServerError::BadRequest(format!(
            "artifact hash must be 32 hex chars, got {}", s.len(),
        )));
    }
    u128::from_str_radix(s, 16)
        .map_err(|_| ServerError::BadRequest(format!("invalid hex hash: {s}")))
}

// ---------- value → JSON ----------
//
// rend `Value` is richer than JSON. We project to JSON-compatible
// shapes for the wire; clients that need full fidelity can submit
// queries that already produce JSON-friendly results (use
// `json_stringify` on the contract side).

fn value_to_json(v: &Value) -> serde_json::Value {
    use serde_json::Value as J;
    match v {
        Value::Int(n) => J::Number((*n).into()),
        Value::I32(n) => J::Number((*n).into()),
        Value::U32(n) => J::Number((*n).into()),
        Value::U64(n) => J::Number((*n).into()),
        Value::U128(n) => J::String(n.to_string()),
        Value::Bool(b) => J::Bool(*b),
        Value::Unit => J::Null,
        Value::Resource(n) => J::Number((*n).into()),
        Value::Str(s) => J::String(s.clone()),
        Value::Address(s) => J::String(s.clone()),
        Value::Bytes(b) => J::String(hex::encode(b)),
        Value::Array(items) => J::Array(items.iter().map(value_to_json).collect()),
        Value::Set(items) => J::Array(items.iter().map(value_to_json).collect()),
        Value::Dict(pairs) => J::Array(pairs.iter().map(|(k, v)| {
            J::Array(vec![value_to_json(k), value_to_json(v)])
        }).collect()),
        Value::Tuple(items) => J::Array(items.iter().map(value_to_json).collect()),
        Value::Struct { name, fields } => {
            let mut obj = serde_json::Map::new();
            obj.insert("@struct".into(), J::String(name.clone()));
            for (k, v) in fields { obj.insert(k.clone(), value_to_json(v)); }
            J::Object(obj)
        }
        Value::Enum { enum_name, variant, payload } => {
            serde_json::json!({
                "@enum": enum_name,
                "variant": variant,
                "payload": payload.iter().map(value_to_json).collect::<Vec<_>>(),
            })
        }
        Value::Pending(_) => J::Null,
        Value::PMap(_) | Value::PBTree(_) | Value::PVec { .. } => J::Null,
        Value::Interface { iface, target_module } => {
            serde_json::json!({"@iface": iface, "target": target_module})
        }
        Value::PMapCursor(_) | Value::PBTreeCursor(_) => J::Null,
        Value::Json(j) => {
            // Re-render json values directly. Round-trip via canonical
            // text, then parse with serde_json so we hand back a
            // proper JSON tree.
            let text = j.to_string_canonical();
            serde_json::from_str(&text).unwrap_or(J::Null)
        }
    }
}
