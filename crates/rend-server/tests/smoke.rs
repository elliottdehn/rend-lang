//! End-to-end smoke test for the HTTP service.
//!
//! Boots the server on a random port against a fresh tempdir, then
//! drives compile → deploy → tx → query through the HTTP API. Every
//! `/v1/orgs/:org/*` request is signed by an `Eoa` whose address
//! matches `:org`. Any regression in the wire shape, RocksDB
//! integration, per-org-namespace routing, or auth middleware
//! breaks this test.

use futures_util::{SinkExt, StreamExt};
use rend_rocksdb::RocksKv;
use rend_server::auth::{Eoa, NONCE_HEADER, SIG_HEADER};
use rend_server::ServerState;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

async fn spawn_server() -> (String, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let kv = RocksKv::open(dir.path()).unwrap();
    let state = ServerState::new(kv, 2_000_000);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = rend_server::router(state);
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    (format!("http://{addr}"), dir)
}

/// A signing HTTP client bound to one EOA. Tracks its own nonce.
#[derive(Clone)]
struct Client {
    base: String,
    http: reqwest::Client,
    wallet: Arc<Eoa>,
    next_nonce: Arc<AtomicU64>,
}

impl Client {
    fn new(base: String) -> Self {
        Self {
            base,
            http: reqwest::Client::new(),
            wallet: Arc::new(Eoa::new()),
            next_nonce: Arc::new(AtomicU64::new(1)),
        }
    }

    fn org(&self) -> String { self.wallet.address_hex() }

    /// Sign and send a JSON POST. The path is appended to the org
    /// segment, e.g. `path_after_org = "tx"` → `/v1/orgs/<addr>/tx`.
    async fn post_json(
        &self,
        path_after_org: &str,
        body: &serde_json::Value,
    ) -> reqwest::Response {
        let nonce = self.next_nonce.fetch_add(1, Ordering::SeqCst);
        let path = format!("/v1/orgs/{}/{}", self.org(), path_after_org);
        let body_bytes = serde_json::to_vec(body).unwrap();
        let sig = self.wallet.sign("POST", &path, nonce, &body_bytes);
        self.http
            .post(format!("{}{}", self.base, path))
            .header(SIG_HEADER, format!("0x{}", hex::encode(sig)))
            .header(NONCE_HEADER, nonce.to_string())
            .header("content-type", "application/json")
            .body(body_bytes)
            .send().await.unwrap()
    }

    async fn post_value(
        &self,
        path_after_org: &str,
        body: &serde_json::Value,
    ) -> serde_json::Value {
        let resp = self.post_json(path_after_org, body).await;
        let status = resp.status();
        let text = resp.text().await.unwrap();
        assert!(status.is_success(), "request failed [{status}]: {text}");
        serde_json::from_str(&text).unwrap()
    }
}

#[tokio::test]
async fn deploy_then_query_round_trip() {
    let (base, _dir) = spawn_server().await;
    let cli = Client::new(base);

    let src = r#"
        module counter;
        state ctr: i64;
        entry view fn current() -> i64 { return ctr; }
        entry fn bump() -> i64 {
            ctr = ctr + 1;
            return ctr;
        }
        fn main() -> i64 { return 0; }
    "#;
    let resp = cli.post_value("compile", &serde_json::json!({ "source": src })).await;
    let hash = resp["hash"].as_str().unwrap().to_string();
    assert_eq!(resp["modules"], serde_json::json!(["counter"]));

    let resp = cli.post_value("deploy", &serde_json::json!({ "hash": hash })).await;
    assert_eq!(resp["result"], serde_json::json!(0));

    let bump_src = r#"
        module bump;
        fn main() -> i64 {
            counter::bump();
            return counter::bump();
        }
    "#;
    let resp = cli.post_value("tx", &serde_json::json!({
        "tx_source": bump_src,
        "deps": [hash],
    })).await;
    assert_eq!(resp["result"], serde_json::json!(2));
    assert!(resp["writes_applied"].as_u64().unwrap() > 0);

    let q = "module q; view fn main() -> i64 { return counter::current(); }";
    let resp = cli.post_value("query", &serde_json::json!({
        "tx_source": q,
        "deps": [hash],
    })).await;
    assert_eq!(resp["result"], serde_json::json!(2));
}

#[tokio::test]
async fn orgs_are_namespaced() {
    let (base, _dir) = spawn_server().await;
    let acme = Client::new(base.clone());
    let globex = Client::new(base);

    let src = r#"
        module store;
        state v: i64;
        entry view fn read() -> i64 { return v; }
        entry fn write(n: i64) -> i64 { v = n; return n; }
        fn main() -> i64 { return 0; }
    "#;

    // Each org compiles + deploys the same source. Even though the
    // hash is identical, state cells are disjoint per namespace.
    let resp = acme.post_value("compile", &serde_json::json!({ "source": src })).await;
    let hash = resp["hash"].as_str().unwrap().to_string();
    let _ = globex.post_value("compile", &serde_json::json!({ "source": src })).await;
    let _ = acme.post_value("deploy", &serde_json::json!({ "hash": hash })).await;
    let _ = globex.post_value("deploy", &serde_json::json!({ "hash": hash })).await;

    let _ = acme.post_value("tx", &serde_json::json!({
        "tx_source": "module t; fn main() -> i64 { return store::write(11); }",
        "deps": [hash],
    })).await;
    let _ = globex.post_value("tx", &serde_json::json!({
        "tx_source": "module t; fn main() -> i64 { return store::write(22); }",
        "deps": [hash],
    })).await;

    for (cli, expected) in [(&acme, 11), (&globex, 22)] {
        let resp = cli.post_value("query", &serde_json::json!({
            "tx_source": "module q; view fn main() -> i64 { return store::read(); }",
            "deps": [hash],
        })).await;
        assert_eq!(resp["result"], serde_json::json!(expected),
            "{} should see {expected}", cli.org());
    }
}

#[tokio::test]
async fn parallel_writes_to_disjoint_keys_all_succeed() {
    // The OCC-without-mutex story: 16 concurrent writes to 16
    // distinct pmap keys all commit; none retry, none get
    // serialized.
    let (base, _dir) = spawn_server().await;
    let cli = Client::new(base);

    let src = r#"
        module bag;
        state items: pmap<u64, u64>;
        entry view fn read(k: u64) -> u64 { return items[k]; }
        entry fn write(k: u64, v: u64) -> u64 {
            items[k] = v;
            return v;
        }
        fn main() -> i64 { return 0; }
    "#;
    let resp = cli.post_value("compile", &serde_json::json!({ "source": src })).await;
    let hash = resp["hash"].as_str().unwrap().to_string();
    let _ = cli.post_value("deploy", &serde_json::json!({ "hash": hash })).await;

    let mut handles = Vec::new();
    for i in 0..16u64 {
        let cli = cli.clone();
        let hash = hash.clone();
        handles.push(tokio::spawn(async move {
            let tx = format!(
                "module t; fn main() -> u64 {{ return bag::write({i}u64, {}u64); }}",
                i * 100,
            );
            let resp = cli.post_json("tx", &serde_json::json!({
                "tx_source": tx,
                "deps": [hash],
            })).await;
            assert!(resp.status().is_success(), "writer {i} failed");
        }));
    }
    for h in handles { h.await.unwrap(); }

    for i in 0..16u64 {
        let q = format!(
            "module q; view fn main() -> u64 {{ return bag::read({i}u64); }}",
        );
        let resp = cli.post_value("query", &serde_json::json!({
            "tx_source": q, "deps": [hash],
        })).await;
        assert_eq!(resp["result"], serde_json::json!(i * 100));
    }
}

#[tokio::test]
async fn parallel_writes_to_same_key_serialize() {
    // Lost-update protection: many bumps of one counter add up.
    let (base, _dir) = spawn_server().await;
    let cli = Client::new(base);

    let src = r#"
        module ctr;
        state n: u64;
        entry view fn current() -> u64 { return n; }
        entry fn bump() -> u64 { n = n + 1u64; return n; }
        fn main() -> i64 { return 0; }
    "#;
    let resp = cli.post_value("compile", &serde_json::json!({ "source": src })).await;
    let hash = resp["hash"].as_str().unwrap().to_string();
    let _ = cli.post_value("deploy", &serde_json::json!({ "hash": hash })).await;

    // Spawning N concurrent tasks against the SAME EOA means N
    // pre-allocated nonces; some will retry-loop on OCC commit
    // conflict for the counter cell. The auth middleware bumps
    // nonces serially regardless, so each task burns one nonce.
    let mut handles = Vec::new();
    for _ in 0..8 {
        let cli = cli.clone();
        let hash = hash.clone();
        handles.push(tokio::spawn(async move {
            let tx = "module t; fn main() -> u64 { return ctr::bump(); }";
            let resp = cli.post_json("tx", &serde_json::json!({
                "tx_source": tx, "deps": [hash],
            })).await;
            resp.status().is_success()
        }));
    }
    let mut succeeded = 0;
    for h in handles {
        if h.await.unwrap() { succeeded += 1; }
    }
    assert!(succeeded >= 4, "at least half the bumps should succeed, got {succeeded}");

    let q = "module q; view fn main() -> u64 { return ctr::current(); }";
    let resp = cli.post_value("query", &serde_json::json!({
        "tx_source": q, "deps": [hash],
    })).await;
    assert_eq!(resp["result"], serde_json::json!(succeeded));
}

#[tokio::test]
async fn query_rejects_writing_main() {
    let (base, _dir) = spawn_server().await;
    let cli = Client::new(base);

    let src = r#"
        module store;
        state v: i64;
        entry fn touch() -> i64 { v = v + 1; return v; }
        fn main() -> i64 { return 0; }
    "#;
    let resp = cli.post_value("compile", &serde_json::json!({ "source": src })).await;
    let hash = resp["hash"].as_str().unwrap().to_string();
    let _ = cli.post_value("deploy", &serde_json::json!({ "hash": hash })).await;

    let resp = cli.post_json("query", &serde_json::json!({
        "tx_source": "module q; fn main() -> i64 { return store::touch(); }",
        "deps": [hash],
    })).await;
    assert!(!resp.status().is_success(),
        "non-view main on /query must be rejected");
}

#[tokio::test]
async fn unauthed_request_is_rejected() {
    let (base, _dir) = spawn_server().await;
    let resp = reqwest::Client::new()
        .post(format!("{base}/v1/orgs/0xabc/compile"))
        .json(&serde_json::json!({ "source": "module x; fn main() -> i64 { return 0; }" }))
        .send().await.unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn wrong_org_in_path_is_rejected() {
    // Sign with one EOA but route the request at a different :org.
    // Recovery succeeds but the address won't match the path → 401.
    let (base, _dir) = spawn_server().await;
    let wallet = Eoa::new();
    let other_org = "0x0000000000000000000000000000000000000001";
    let path = format!("/v1/orgs/{other_org}/compile");
    let body = serde_json::json!({ "source": "module x; fn main() -> i64 { return 0; }" });
    let body_bytes = serde_json::to_vec(&body).unwrap();
    let sig = wallet.sign("POST", &path, 1, &body_bytes);

    let resp = reqwest::Client::new()
        .post(format!("{base}{path}"))
        .header(SIG_HEADER, format!("0x{}", hex::encode(sig)))
        .header(NONCE_HEADER, "1")
        .header("content-type", "application/json")
        .body(body_bytes)
        .send().await.unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn replayed_nonce_is_rejected() {
    // Sign the SAME request twice — first succeeds, second 401s.
    let (base, _dir) = spawn_server().await;
    let wallet = Eoa::new();
    let path = format!("/v1/orgs/{}/compile", wallet.address_hex());
    let body = serde_json::json!({ "source": "module x; fn main() -> i64 { return 0; }" });
    let body_bytes = serde_json::to_vec(&body).unwrap();
    let sig = wallet.sign("POST", &path, 42, &body_bytes);

    let send = || {
        let body_bytes = body_bytes.clone();
        let sig = sig;
        let path = path.clone();
        let base = base.clone();
        async move {
            reqwest::Client::new()
                .post(format!("{base}{path}"))
                .header(SIG_HEADER, format!("0x{}", hex::encode(sig)))
                .header(NONCE_HEADER, "42")
                .header("content-type", "application/json")
                .body(body_bytes)
                .send().await.unwrap()
        }
    };

    let first = send().await;
    assert!(first.status().is_success(), "first request should succeed");
    let second = send().await;
    assert_eq!(second.status(), reqwest::StatusCode::UNAUTHORIZED,
        "replay must be rejected");
}

#[tokio::test]
async fn ws_round_trip() {
    // Open a /stream socket, run compile → deploy → tx → query
    // through frames. The handshake is the only signed step;
    // subsequent frames carry no signature.
    let (base, _dir) = spawn_server().await;
    let wallet = Eoa::new();
    let path = format!("/v1/orgs/{}/stream", wallet.address_hex());
    let sig = wallet.sign("GET", &path, 1, &[]);
    let host = base.trim_start_matches("http://").to_string();
    let url = format!("ws://{host}{path}");
    let req = http::Request::builder()
        .method("GET")
        .uri(&url)
        .header(SIG_HEADER, format!("0x{}", hex::encode(sig)))
        .header(NONCE_HEADER, "1")
        .header("Host", &host)
        .header("Connection", "Upgrade")
        .header("Upgrade", "websocket")
        .header("Sec-WebSocket-Version", "13")
        .header("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ==")
        .body(()).unwrap();
    let (mut ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();

    async fn call(
        ws: &mut tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
        id: u64, kind: &str, body: serde_json::Value,
    ) -> serde_json::Value {
        let frame = serde_json::json!({"id": id, "kind": kind, "body": body});
        ws.send(Message::Text(frame.to_string())).await.unwrap();
        let msg = ws.next().await.unwrap().unwrap();
        let text = match msg { Message::Text(t) => t, _ => panic!("not text") };
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["id"].as_u64(), Some(id), "response id mismatch");
        assert!(v["ok"].as_bool().unwrap_or(false), "frame error: {v}");
        v["result"].clone()
    }

    let src = "module ctr; state n: u64;
        entry view fn current() -> u64 { return n; }
        entry fn bump() -> u64 { n = n + 1u64; return n; }
        fn main() -> i64 { return 0; }";
    let r = call(&mut ws, 1, "compile", serde_json::json!({ "source": src })).await;
    let hash = r["hash"].as_str().unwrap().to_string();
    let _ = call(&mut ws, 2, "deploy", serde_json::json!({ "hash": hash })).await;
    let r = call(&mut ws, 3, "tx", serde_json::json!({
        "tx_source": "module t; fn main() -> u64 { return ctr::bump(); }",
        "deps": [hash],
    })).await;
    assert_eq!(r["result"], serde_json::json!(1));
    let r = call(&mut ws, 4, "query", serde_json::json!({
        "tx_source": "module q; view fn main() -> u64 { return ctr::current(); }",
        "deps": [hash],
    })).await;
    assert_eq!(r["result"], serde_json::json!(1));
}

#[tokio::test]
async fn ws_upgrade_without_auth_rejected() {
    let (base, _dir) = spawn_server().await;
    let host = base.trim_start_matches("http://").to_string();
    let url = format!("ws://{host}/v1/orgs/0xabc/stream");
    let req = http::Request::builder()
        .method("GET")
        .uri(&url)
        .header("Host", &host)
        .header("Connection", "Upgrade")
        .header("Upgrade", "websocket")
        .header("Sec-WebSocket-Version", "13")
        .header("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ==")
        .body(()).unwrap();
    let res = tokio_tungstenite::connect_async(req).await;
    assert!(res.is_err(), "unauthed WS upgrade must fail");
}

#[tokio::test]
async fn nonce_must_be_strictly_increasing() {
    let (base, _dir) = spawn_server().await;
    let wallet = Eoa::new();
    let org = wallet.address_hex();
    let post = |nonce: u64| {
        let body = serde_json::json!({ "source": "module x; fn main() -> i64 { return 0; }" });
        let body_bytes = serde_json::to_vec(&body).unwrap();
        let path = format!("/v1/orgs/{org}/compile");
        let sig = wallet.sign("POST", &path, nonce, &body_bytes);
        let base = base.clone();
        async move {
            reqwest::Client::new()
                .post(format!("{base}{path}"))
                .header(SIG_HEADER, format!("0x{}", hex::encode(sig)))
                .header(NONCE_HEADER, nonce.to_string())
                .header("content-type", "application/json")
                .body(body_bytes)
                .send().await.unwrap()
        }
    };

    assert!(post(10).await.status().is_success());
    assert_eq!(post(9).await.status(), reqwest::StatusCode::UNAUTHORIZED);
    assert_eq!(post(10).await.status(), reqwest::StatusCode::UNAUTHORIZED);
    assert!(post(11).await.status().is_success());
    assert!(post(1000).await.status().is_success());
}
