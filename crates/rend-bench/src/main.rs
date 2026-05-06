//! Load generator for rend-server.
//!
//! Spins up N concurrent workers, each with its own EOA (so each
//! worker is its own org → cross-org commits proceed fully in
//! parallel), deploys a workload module, then drives requests for
//! `--duration` seconds. Reports throughput and latency percentiles.
//!
//! ## Usage
//!
//! ```text
//! rend-bench [--target URL] [--workers N] [--duration SECS]
//!            [--workload WORKLOAD] [--shared] [--warmup SECS]
//!            [--accounts N] [--ws] [--inflight N]
//! ```
//!
//! Workloads:
//!
//! - `tx`            counter `bump()`; 1-cell read, 1-cell write
//! - `query`         counter `current()`; 1-cell read, no write
//! - `transfer`      bank transfer between random accounts;
//!                   2-cell read, 2-cell write, with an `assert`
//!                   on sufficient balance
//! - `transfer-hot`  bank transfer from random sender → fixed
//!                   account 0 (escrow); maximises OCC contention
//!                   on account 0 under `--shared`
//!
//! Transports:
//!
//! - default (HTTP): every request carries an EOA signature and a
//!   nonce. Server verifies sig + CASes nonce per request.
//! - `--ws`: each worker opens one WebSocket whose handshake is
//!   the only authed step. Subsequent frames travel without
//!   per-message sig verify. Use `--inflight N` (default 1) to
//!   pipeline N concurrent in-flight frames per socket.

use futures_util::{SinkExt, StreamExt};
use rand::Rng;
use rend_server::auth::{Eoa, NONCE_HEADER, SIG_HEADER};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;

const COUNTER_MODULE: &str = r#"
    module ctr;
    state n: u64;
    entry view fn current() -> u64 { return n; }
    entry fn bump() -> u64 { n = n + 1u64; return n; }
    fn main() -> i64 { return 0; }
"#;

// Note the `transfer` pattern: BOTH reads happen first, BEFORE any
// assert or write. With pmap walk-level batching (Tx::pending_walks)
// the runtime sees both walks queued together at the first force
// point and batches their per-level cell reads in one
// `Kv::get_many`. The "assert + writes" tail then runs against
// already-resolved values. Without this read-first ordering the
// assert's force drains walk 1 alone, then walk 2 runs alone — no
// batching even with Slice 1's machinery.
const BANK_MODULE: &str = r#"
    module bank;
    state accounts: pmap<Address, u64>;
    entry view fn balance(who: Address) -> u64 { return accounts[who]; }
    entry fn mint(to: Address, amount: u64) -> u64 {
        accounts[to] = accounts[to] + amount;
        return accounts[to];
    }
    entry fn transfer(from: Address, to: Address, amount: u64) -> u64 {
        let b_from = accounts[from];
        let b_to   = accounts[to];
        assert(b_from >= amount, "insufficient");
        accounts[from] = b_from - amount;
        accounts[to]   = b_to + amount;
        return accounts[to];
    }
    fn main() -> i64 { return 0; }
"#;

#[derive(Clone, Debug)]
struct Args {
    target: String,
    workers: usize,
    duration: Duration,
    warmup: Duration,
    workload: Workload,
    shared: bool,
    accounts: usize,
    ws: bool,
    inflight: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Workload { Tx, Query, Transfer, TransferHot }

impl Workload {
    fn label(self) -> &'static str {
        match self {
            Workload::Tx => "tx",
            Workload::Query => "query",
            Workload::Transfer => "transfer",
            Workload::TransferHot => "transfer-hot",
        }
    }
    fn is_bank(self) -> bool {
        matches!(self, Workload::Transfer | Workload::TransferHot)
    }
    fn is_query(self) -> bool { matches!(self, Workload::Query) }
}

impl Args {
    fn parse() -> anyhow::Result<Self> {
        let mut a = Self {
            target: "http://127.0.0.1:8080".to_string(),
            workers: 8,
            duration: Duration::from_secs(10),
            warmup: Duration::from_secs(1),
            workload: Workload::Tx,
            shared: false,
            accounts: 100,
            ws: false,
            inflight: 1,
        };
        let mut it = std::env::args().skip(1);
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "--target" => a.target = it.next().ok_or_else(|| anyhow::anyhow!("--target needs value"))?,
                "--workers" => a.workers = it.next().unwrap_or_default().parse()?,
                "--duration" => a.duration = Duration::from_secs(it.next().unwrap_or_default().parse()?),
                "--warmup" => a.warmup = Duration::from_secs(it.next().unwrap_or_default().parse()?),
                "--workload" => {
                    a.workload = match it.next().unwrap_or_default().as_str() {
                        "tx" => Workload::Tx,
                        "query" => Workload::Query,
                        "transfer" => Workload::Transfer,
                        "transfer-hot" => Workload::TransferHot,
                        other => anyhow::bail!("unknown workload {other:?}"),
                    };
                }
                "--shared" => a.shared = true,
                "--ws" => a.ws = true,
                "--inflight" => a.inflight = it.next().unwrap_or_default().parse()?,
                "--accounts" => a.accounts = it.next().unwrap_or_default().parse()?,
                "-h" | "--help" => {
                    println!("Usage: rend-bench [--target URL] [--workers N] [--duration SECS]");
                    println!("                  [--warmup SECS] [--workload WORKLOAD] [--shared]");
                    println!("                  [--accounts N] [--ws] [--inflight N]");
                    println!("WORKLOAD: tx | query | transfer | transfer-hot");
                    std::process::exit(0);
                }
                other => anyhow::bail!("unexpected arg {other:?}"),
            }
        }
        Ok(a)
    }
}

// ============================================================
// transport: trait over HTTP and WebSocket
// ============================================================

#[async_trait::async_trait]
trait Transport: Send + Sync {
    async fn call(&self, kind: &str, body: serde_json::Value)
        -> anyhow::Result<serde_json::Value>;
}

// ---------- HTTP transport ----------

struct HttpTransport {
    target: String,
    http: reqwest::Client,
    wallet: Arc<Eoa>,
    next_nonce: Arc<AtomicU64>,
}

impl HttpTransport {
    fn new(target: String, wallet: Arc<Eoa>, next_nonce: Arc<AtomicU64>) -> Arc<Self> {
        Arc::new(Self {
            target,
            http: reqwest::Client::builder()
                .pool_max_idle_per_host(64)
                .build().unwrap(),
            wallet,
            next_nonce,
        })
    }
}

#[async_trait::async_trait]
impl Transport for HttpTransport {
    async fn call(&self, kind: &str, body: serde_json::Value)
        -> anyhow::Result<serde_json::Value>
    {
        let nonce = self.next_nonce.fetch_add(1, Ordering::SeqCst);
        let path = format!("/v1/orgs/{}/{}", self.wallet.address_hex(), kind);
        let body_bytes = serde_json::to_vec(&body)?;
        let sig = self.wallet.sign("POST", &path, nonce, &body_bytes);
        let resp = self.http
            .post(format!("{}{}", self.target, path))
            .header(SIG_HEADER, format!("0x{}", hex::encode(sig)))
            .header(NONCE_HEADER, nonce.to_string())
            .header("content-type", "application/json")
            .body(body_bytes)
            .send().await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            anyhow::bail!("{status}: {text}");
        }
        Ok(serde_json::from_str(&text)?)
    }
}

// ---------- WS transport ----------

struct WsTransport {
    out_tx: mpsc::UnboundedSender<Message>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<serde_json::Value>>>>,
    next_id: AtomicU64,
}

impl WsTransport {
    async fn connect(target: &str, wallet: &Eoa, nonce: u64) -> anyhow::Result<Arc<Self>> {
        let path = format!("/v1/orgs/{}/stream", wallet.address_hex());
        let sig = wallet.sign("GET", &path, nonce, &[]);
        let ws_url = target
            .replacen("http://", "ws://", 1)
            .replacen("https://", "wss://", 1);
        let url = format!("{ws_url}{path}");

        let req = http::Request::builder()
            .method("GET")
            .uri(&url)
            .header(SIG_HEADER, format!("0x{}", hex::encode(sig)))
            .header(NONCE_HEADER, nonce.to_string())
            .header("Host", target.trim_start_matches("http://").trim_start_matches("https://"))
            .header("Connection", "Upgrade")
            .header("Upgrade", "websocket")
            .header("Sec-WebSocket-Version", "13")
            .header("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ==")
            .body(())?;

        let (ws, _) = tokio_tungstenite::connect_async(req).await?;
        let (mut write, mut read) = ws.split();

        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Message>();
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<serde_json::Value>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        // Writer task
        tokio::spawn(async move {
            while let Some(m) = out_rx.recv().await {
                if write.send(m).await.is_err() { break; }
            }
        });

        // Reader task: route responses by id.
        let pending_r = pending.clone();
        tokio::spawn(async move {
            while let Some(Ok(msg)) = read.next().await {
                if let Message::Text(text) = msg {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                        if let Some(id) = v.get("id").and_then(|i| i.as_u64()) {
                            if let Some(tx) = pending_r.lock().unwrap().remove(&id) {
                                let _ = tx.send(v);
                            }
                        }
                    }
                }
            }
        });

        Ok(Arc::new(Self {
            out_tx,
            pending,
            next_id: AtomicU64::new(1),
        }))
    }
}

#[async_trait::async_trait]
impl Transport for WsTransport {
    async fn call(&self, kind: &str, body: serde_json::Value)
        -> anyhow::Result<serde_json::Value>
    {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let frame = serde_json::json!({ "id": id, "kind": kind, "body": body });
        self.out_tx.send(Message::Text(frame.to_string()))
            .map_err(|_| anyhow::anyhow!("ws writer closed"))?;
        let resp = rx.await.map_err(|_| anyhow::anyhow!("ws reader closed"))?;
        if !resp.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
            anyhow::bail!("{}", resp.get("error").and_then(|v| v.as_str()).unwrap_or("unknown"));
        }
        Ok(resp.get("result").cloned().unwrap_or(serde_json::Value::Null))
    }
}

// ============================================================
// setup + drive
// ============================================================

async fn setup(
    t: &Arc<dyn Transport>,
    args: &Args,
) -> anyhow::Result<String> {
    let source = if args.workload.is_bank() { BANK_MODULE } else { COUNTER_MODULE };
    let resp = t.call("compile", serde_json::json!({ "source": source })).await?;
    let hash = resp["hash"].as_str().ok_or_else(|| anyhow::anyhow!("no hash"))?.to_string();
    let _ = t.call("deploy", serde_json::json!({ "hash": hash })).await?;
    if args.workload.is_bank() {
        for i in 0..args.accounts {
            let tx = format!(
                r#"module setup; fn main() -> u64 {{ return bank::mint(address("acc-{i}"), 1000000u64); }}"#,
            );
            t.call("tx", serde_json::json!({ "tx_source": tx, "deps": [hash] })).await?;
        }
    }
    Ok(hash)
}

fn build_body(workload: Workload, hash: &str, accounts: usize) -> serde_json::Value {
    let mut rng = rand::thread_rng();
    match workload {
        Workload::Tx => serde_json::json!({
            "tx_source": "module t; fn main() -> u64 { return ctr::bump(); }",
            "deps": [hash],
        }),
        Workload::Query => serde_json::json!({
            "tx_source": "module q; view fn main() -> u64 { return ctr::current(); }",
            "deps": [hash],
        }),
        Workload::Transfer => {
            let from = rng.gen_range(0..accounts);
            let mut to = rng.gen_range(0..accounts);
            if to == from { to = (to + 1) % accounts; }
            let tx = format!(
                r#"module t; fn main() -> u64 {{ return bank::transfer(address("acc-{from}"), address("acc-{to}"), 1u64); }}"#,
            );
            serde_json::json!({ "tx_source": tx, "deps": [hash] })
        }
        Workload::TransferHot => {
            let from = rng.gen_range(1..accounts.max(2));
            let tx = format!(
                r#"module t; fn main() -> u64 {{ return bank::transfer(address("acc-{from}"), address("acc-0"), 1u64); }}"#,
            );
            serde_json::json!({ "tx_source": tx, "deps": [hash] })
        }
    }
}

async fn drive_one(
    t: Arc<dyn Transport>,
    hash: String,
    args: Args,
    deadline: Instant,
    latencies: Arc<Mutex<Vec<u64>>>,
    ok: Arc<AtomicU64>,
    err: Arc<AtomicU64>,
) {
    let kind = if args.workload.is_query() { "query" } else { "tx" };
    let mut local = Vec::with_capacity(1024);
    while Instant::now() < deadline {
        let body = build_body(args.workload, &hash, args.accounts);
        let t0 = Instant::now();
        match t.call(kind, body).await {
            Ok(_) => {
                local.push(t0.elapsed().as_micros() as u64);
                ok.fetch_add(1, Ordering::Relaxed);
            }
            Err(_) => { err.fetch_add(1, Ordering::Relaxed); }
        }
    }
    latencies.lock().unwrap().extend(local);
}

fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() { return 0; }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse()?;
    println!("rend-bench → {}", args.target);
    println!("  workers   {}{}", args.workers, if args.shared { " (shared org)" } else { "" });
    println!("  workload  {}", args.workload.label());
    println!("  transport {}{}",
        if args.ws { "ws" } else { "http" },
        if args.ws && args.inflight > 1 { format!(" (inflight={})", args.inflight) } else { String::new() });
    if args.workload.is_bank() { println!("  accounts  {}", args.accounts); }
    println!("  warmup    {:?}", args.warmup);
    println!("  duration  {:?}", args.duration);

    // Wallets + nonce counters (HTTP) — each worker has its own EOA
    // unless --shared. With --shared all workers also share the
    // nonce counter so they don't race at the auth layer.
    let (wallets, nonces): (Vec<_>, Vec<_>) = if args.shared {
        let w = Arc::new(Eoa::new());
        let n = Arc::new(AtomicU64::new(1));
        (0..args.workers).map(|_| (w.clone(), n.clone())).unzip()
    } else {
        (0..args.workers).map(|_| {
            (Arc::new(Eoa::new()), Arc::new(AtomicU64::new(1)))
        }).unzip()
    };

    // Build a Transport per worker. For HTTP this is just a struct;
    // for WS we open a socket and burn one nonce on the handshake.
    let mut transports: Vec<Arc<dyn Transport>> = Vec::with_capacity(args.workers);
    for (w, n) in wallets.iter().zip(nonces.iter()) {
        if args.ws {
            let nonce = n.fetch_add(1, Ordering::SeqCst);
            let t = WsTransport::connect(&args.target, w, nonce).await?;
            transports.push(t as Arc<dyn Transport>);
        } else {
            let t = HttpTransport::new(args.target.clone(), w.clone(), n.clone());
            transports.push(t as Arc<dyn Transport>);
        }
    }

    print!("setup ... ");
    let started_setup = Instant::now();
    let mut hashes = Vec::with_capacity(transports.len());
    if args.shared {
        let hash = setup(&transports[0], &args).await?;
        for _ in 0..transports.len() { hashes.push(hash.clone()); }
    } else {
        for t in &transports {
            hashes.push(setup(t, &args).await?);
        }
    }
    println!("ok ({:.1}s)", started_setup.elapsed().as_secs_f64());

    if !args.warmup.is_zero() {
        print!("warmup {:?} ... ", args.warmup);
        let deadline = Instant::now() + args.warmup;
        let mut handles = Vec::new();
        let lats = Arc::new(Mutex::new(Vec::new()));
        let ok = Arc::new(AtomicU64::new(0));
        let err = Arc::new(AtomicU64::new(0));
        for (t, h) in transports.iter().cloned().zip(hashes.iter().cloned()) {
            for _ in 0..args.inflight {
                let t = t.clone();
                let h = h.clone();
                let args = args.clone();
                let lats = lats.clone();
                let ok = ok.clone();
                let err = err.clone();
                handles.push(tokio::spawn(drive_one(t, h, args, deadline, lats, ok, err)));
            }
        }
        for h in handles { h.await.unwrap(); }
        println!("done");
    }

    let lats = Arc::new(Mutex::new(Vec::new()));
    let ok = Arc::new(AtomicU64::new(0));
    let err = Arc::new(AtomicU64::new(0));
    let deadline = Instant::now() + args.duration;
    let started = Instant::now();
    let mut handles = Vec::new();
    for (t, h) in transports.into_iter().zip(hashes.into_iter()) {
        for _ in 0..args.inflight {
            let t = t.clone();
            let h = h.clone();
            let args = args.clone();
            let lats = lats.clone();
            let ok = ok.clone();
            let err = err.clone();
            handles.push(tokio::spawn(drive_one(t, h, args, deadline, lats, ok, err)));
        }
    }
    for h in handles { h.await.unwrap(); }
    let elapsed = started.elapsed();

    let ok = ok.load(Ordering::Relaxed);
    let err = err.load(Ordering::Relaxed);
    let mut lats = Arc::try_unwrap(lats).unwrap().into_inner().unwrap();
    lats.sort_unstable();

    println!();
    println!("== results ==");
    println!("  elapsed   {:.3}s", elapsed.as_secs_f64());
    println!("  ok        {ok}");
    println!("  err       {err}");
    if elapsed.as_secs_f64() > 0.0 {
        println!("  ops/sec   {:.1}", ok as f64 / elapsed.as_secs_f64());
    }
    if !lats.is_empty() {
        let mean = lats.iter().sum::<u64>() as f64 / lats.len() as f64;
        println!("  latency µs:  mean {:.0}  p50 {}  p95 {}  p99 {}  max {}",
            mean,
            percentile(&lats, 0.50),
            percentile(&lats, 0.95),
            percentile(&lats, 0.99),
            *lats.last().unwrap_or(&0),
        );
    }
    Ok(())
}
