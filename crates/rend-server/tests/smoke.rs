//! End-to-end smoke test for the HTTP service.
//!
//! Boots the server on a random port against a fresh tempdir, then
//! drives compile → deploy → tx → query through the HTTP API. Any
//! regression in the wire shape, RocksDB integration, or
//! per-org-namespace routing breaks this test.

use rend_rocksdb::RocksKv;
use rend_server::ServerState;
use std::time::Duration;
use tokio::net::TcpListener;

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
    // Give the listener a moment to settle on slow CI.
    tokio::time::sleep(Duration::from_millis(50)).await;
    (format!("http://{addr}"), dir)
}

#[tokio::test]
async fn deploy_then_query_round_trip() {
    let (base, _dir) = spawn_server().await;
    let client = reqwest::Client::new();

    // 1. Compile a tiny module.
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
    let resp: serde_json::Value = client
        .post(format!("{base}/v1/orgs/acme/compile"))
        .json(&serde_json::json!({ "source": src }))
        .send().await.unwrap()
        .json().await.unwrap();
    let hash = resp["hash"].as_str().unwrap().to_string();
    assert_eq!(resp["modules"], serde_json::json!(["counter"]));

    // 2. Deploy — runs the (empty) constructor.
    let resp: serde_json::Value = client
        .post(format!("{base}/v1/orgs/acme/deploy"))
        .json(&serde_json::json!({ "hash": hash }))
        .send().await.unwrap()
        .json().await.unwrap();
    assert_eq!(resp["result"], serde_json::json!(0));

    // 3. Tx that bumps the counter twice.
    let bump_src = format!(r#"
        module bump;
        fn main() -> i64 {{
            counter::bump();
            return counter::bump();
        }}
    "#);
    let resp: serde_json::Value = client
        .post(format!("{base}/v1/orgs/acme/tx"))
        .json(&serde_json::json!({
            "tx_source": bump_src,
            "deps": [hash],
        }))
        .send().await.unwrap()
        .json().await.unwrap();
    assert_eq!(resp["result"], serde_json::json!(2));
    assert!(resp["writes_applied"].as_u64().unwrap() > 0);

    // 4. Query — read-only path returns the current count.
    let q = r#"
        module q;
        view fn main() -> i64 { return counter::current(); }
    "#;
    let resp: serde_json::Value = client
        .post(format!("{base}/v1/orgs/acme/query"))
        .json(&serde_json::json!({
            "tx_source": q,
            "deps": [hash],
        }))
        .send().await.unwrap()
        .json().await.unwrap();
    assert_eq!(resp["result"], serde_json::json!(2));
}

#[tokio::test]
async fn orgs_are_namespaced() {
    let (base, _dir) = spawn_server().await;
    let client = reqwest::Client::new();
    let src = r#"
        module store;
        state v: i64;
        entry view fn read() -> i64 { return v; }
        entry fn write(n: i64) -> i64 { v = n; return n; }
        fn main() -> i64 { return 0; }
    "#;

    // Deploy under "acme" and "globex" with the same source — same
    // hash, but each org's state cells are disjoint.
    let resp: serde_json::Value = client
        .post(format!("{base}/v1/orgs/acme/compile"))
        .json(&serde_json::json!({ "source": src }))
        .send().await.unwrap()
        .json().await.unwrap();
    let hash = resp["hash"].as_str().unwrap().to_string();

    let _ = client
        .post(format!("{base}/v1/orgs/globex/compile"))
        .json(&serde_json::json!({ "source": src }))
        .send().await.unwrap();

    for org in ["acme", "globex"] {
        let _ = client
            .post(format!("{base}/v1/orgs/{org}/deploy"))
            .json(&serde_json::json!({ "hash": hash }))
            .send().await.unwrap();
    }

    // acme writes 11; globex writes 22. Both reads see only their
    // own org's value.
    let _ = client
        .post(format!("{base}/v1/orgs/acme/tx"))
        .json(&serde_json::json!({
            "tx_source": "module t; fn main() -> i64 { return store::write(11); }",
            "deps": [hash],
        }))
        .send().await.unwrap();
    let _ = client
        .post(format!("{base}/v1/orgs/globex/tx"))
        .json(&serde_json::json!({
            "tx_source": "module t; fn main() -> i64 { return store::write(22); }",
            "deps": [hash],
        }))
        .send().await.unwrap();

    for (org, expected) in [("acme", 11), ("globex", 22)] {
        let resp: serde_json::Value = client
            .post(format!("{base}/v1/orgs/{org}/query"))
            .json(&serde_json::json!({
                "tx_source": "module q; view fn main() -> i64 { return store::read(); }",
                "deps": [hash],
            }))
            .send().await.unwrap()
            .json().await.unwrap();
        assert_eq!(resp["result"], serde_json::json!(expected),
            "org {org} should see its own value");
    }
}

#[tokio::test]
async fn query_rejects_writing_main() {
    let (base, _dir) = spawn_server().await;
    let client = reqwest::Client::new();

    let src = r#"
        module store;
        state v: i64;
        entry fn touch() -> i64 { v = v + 1; return v; }
        fn main() -> i64 { return 0; }
    "#;
    let resp: serde_json::Value = client
        .post(format!("{base}/v1/orgs/acme/compile"))
        .json(&serde_json::json!({ "source": src }))
        .send().await.unwrap()
        .json().await.unwrap();
    let hash = resp["hash"].as_str().unwrap().to_string();
    let _ = client
        .post(format!("{base}/v1/orgs/acme/deploy"))
        .json(&serde_json::json!({ "hash": hash }))
        .send().await.unwrap();

    // A non-`view` main on the query path should be rejected by the
    // engine before any writes happen.
    let resp = client
        .post(format!("{base}/v1/orgs/acme/query"))
        .json(&serde_json::json!({
            "tx_source": "module q; fn main() -> i64 { return store::touch(); }",
            "deps": [hash],
        }))
        .send().await.unwrap();
    assert!(!resp.status().is_success(),
        "non-view main on /query must be rejected");
}
