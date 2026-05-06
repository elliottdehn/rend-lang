//! WebSocket streaming endpoint.
//!
//! Auth happens once at upgrade time via the existing
//! `require_auth` middleware (signed handshake, nonce CAS). After
//! upgrade the socket is bound to the recovered address and every
//! frame is processed without re-verifying.
//!
//! ## Frame format
//!
//! Request (text JSON):
//!
//! ```json
//! {"id": <any>, "kind": "compile"|"deploy"|"tx"|"query", "body": {...}}
//! ```
//!
//! Response (text JSON):
//!
//! ```json
//! {"id": <same>, "ok": true,  "result": {...}}
//! {"id": <same>, "ok": false, "error": "..."}
//! ```
//!
//! `body` matches the HTTP endpoint of the same name. `result`
//! matches the corresponding HTTP response shape.
//!
//! Frames are processed concurrently per socket: a reader task
//! spawns one handler task per inbound frame; an mpsc-fed writer
//! task drains responses out. The wire is single-stream, but
//! in-flight requests can overlap, so a slow tx doesn't block
//! cheap queries on the same connection.

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::response::Response;
use futures_util::{SinkExt, StreamExt};
use rend_rocksdb::NamespaceId;
use serde::Deserialize;
use tokio::sync::mpsc;

use crate::{
    do_compile, do_deploy, do_query, do_tx, CompileBody, DeployBody, ServerError, ServerState,
    TxBody,
};

pub async fn stream_handler(
    ws: WebSocketUpgrade,
    State(state): State<ServerState>,
    Path(org): Path<String>,
) -> Response {
    let ns = state.ns_for(&org);
    ws.on_upgrade(move |socket| handle_socket(socket, state, ns))
}

async fn handle_socket(socket: WebSocket, state: ServerState, ns: NamespaceId) {
    let (mut write, mut read) = socket.split();
    let (out_tx, mut out_rx) = mpsc::channel::<Message>(256);

    // Writer task: serialize all outbound frames through one
    // sink. Handler tasks just push into `out_tx` without caring
    // about ordering or backpressure beyond the channel bound.
    let writer = tokio::spawn(async move {
        while let Some(m) = out_rx.recv().await {
            if write.send(m).await.is_err() {
                break;
            }
        }
        let _ = write.close().await;
    });

    while let Some(Ok(msg)) = read.next().await {
        match msg {
            Message::Text(text) => {
                let state = state.clone();
                let out_tx = out_tx.clone();
                tokio::spawn(async move {
                    let response = process_frame(&state, ns, &text).await;
                    let _ = out_tx.send(Message::Text(response)).await;
                });
            }
            Message::Binary(_) | Message::Ping(_) | Message::Pong(_) => {}
            Message::Close(_) => break,
        }
    }

    drop(out_tx);
    let _ = writer.await;
}

#[derive(Deserialize)]
struct Frame {
    /// Opaque correlation id; echoed back on the response. Allows
    /// the client to interleave many in-flight requests on one
    /// socket.
    id: serde_json::Value,
    kind: String,
    body: serde_json::Value,
}

async fn process_frame(state: &ServerState, ns: NamespaceId, text: &str) -> String {
    let frame: Frame = match serde_json::from_str(text) {
        Ok(f) => f,
        Err(e) => return error_frame(serde_json::Value::Null, &format!("frame parse: {e}")),
    };
    let id = frame.id.clone();

    let result: Result<serde_json::Value, ServerError> = match frame.kind.as_str() {
        "compile" => parse_body::<CompileBody>(frame.body)
            .and_then(|b| do_compile(state, ns, b))
            .map(|r| serde_json::to_value(r).unwrap()),
        "deploy" => parse_body::<DeployBody>(frame.body)
            .and_then(|b| do_deploy(state, ns, b))
            .map(|r| serde_json::to_value(r).unwrap()),
        "tx" => match parse_body::<TxBody>(frame.body) {
            Ok(b) => do_tx(state, ns, b).await.map(|r| serde_json::to_value(r).unwrap()),
            Err(e) => Err(e),
        },
        "query" => parse_body::<TxBody>(frame.body)
            .and_then(|b| do_query(state, ns, b))
            .map(|r| serde_json::to_value(r).unwrap()),
        other => return error_frame(id, &format!("unknown kind: {other}")),
    };

    match result {
        Ok(v) => serde_json::json!({"id": id, "ok": true, "result": v}).to_string(),
        Err(e) => error_frame(id, &e.to_string()),
    }
}

fn parse_body<B: serde::de::DeserializeOwned>(
    body: serde_json::Value,
) -> Result<B, ServerError> {
    serde_json::from_value(body).map_err(|e| ServerError::BadRequest(format!("body: {e}")))
}

fn error_frame(id: serde_json::Value, msg: &str) -> String {
    serde_json::json!({"id": id, "ok": false, "error": msg}).to_string()
}
