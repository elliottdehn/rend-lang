//! Load generator for rend-server.
//!
//! Spins up N concurrent workers, each with its own EOA (so each
//! worker is its own org → cross-org commits proceed fully in
//! parallel), deploys a counter module, then drives bumps for
//! `--duration` seconds. Reports throughput and latency percentiles.
//!
//! ## Usage
//!
//! ```text
//! rend-bench [--target URL] [--workers N] [--duration SECS]
//!            [--workload tx|query] [--shared] [--warmup SECS]
//! ```
//!
//! Defaults: `--target http://127.0.0.1:8080 --workers 8 --duration 10
//!            --workload tx --warmup 1`.
//!
//! With `--shared`, all workers share a single EOA → all writes hit
//! one org → measures intra-org OCC contention rather than the
//! parallel-org happy path.

use rend_server::auth::{Eoa, NONCE_HEADER, SIG_HEADER};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const COUNTER_MODULE: &str = r#"
    module ctr;
    state n: u64;
    entry view fn current() -> u64 { return n; }
    entry fn bump() -> u64 { n = n + 1u64; return n; }
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
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Workload { Tx, Query }

impl Args {
    fn parse() -> anyhow::Result<Self> {
        let mut target = "http://127.0.0.1:8080".to_string();
        let mut workers = 8usize;
        let mut duration = Duration::from_secs(10);
        let mut warmup = Duration::from_secs(1);
        let mut workload = Workload::Tx;
        let mut shared = false;

        let mut it = std::env::args().skip(1);
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "--target" => target = it.next().ok_or_else(|| anyhow::anyhow!("--target needs value"))?,
                "--workers" => workers = it.next().unwrap_or_default().parse()?,
                "--duration" => duration = Duration::from_secs(it.next().unwrap_or_default().parse()?),
                "--warmup" => warmup = Duration::from_secs(it.next().unwrap_or_default().parse()?),
                "--workload" => {
                    workload = match it.next().unwrap_or_default().as_str() {
                        "tx" => Workload::Tx,
                        "query" => Workload::Query,
                        other => anyhow::bail!("unknown workload {other:?}"),
                    };
                }
                "--shared" => shared = true,
                "-h" | "--help" => {
                    println!("Usage: rend-bench [--target URL] [--workers N] [--duration SECS] [--warmup SECS] [--workload tx|query] [--shared]");
                    std::process::exit(0);
                }
                other => anyhow::bail!("unexpected arg {other:?}"),
            }
        }
        Ok(Self { target, workers, duration, warmup, workload, shared })
    }
}

#[derive(Clone)]
struct Worker {
    target: String,
    http: reqwest::Client,
    wallet: Arc<Eoa>,
    /// Shared with siblings that bind to the same EOA. The server
    /// requires strictly-monotonic nonces per address, so any two
    /// workers under one wallet MUST allocate from the same counter
    /// — otherwise they race and most requests 401.
    next_nonce: Arc<AtomicU64>,
}

impl Worker {
    fn new(target: String, wallet: Arc<Eoa>, next_nonce: Arc<AtomicU64>) -> Self {
        Self {
            target,
            http: reqwest::Client::builder()
                .pool_max_idle_per_host(64)
                .build().unwrap(),
            wallet,
            next_nonce,
        }
    }

    async fn post(
        &self,
        path_after_org: &str,
        body: serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        let nonce = self.next_nonce.fetch_add(1, Ordering::SeqCst);
        let path = format!("/v1/orgs/{}/{}", self.wallet.address_hex(), path_after_org);
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

async fn setup(worker: &Worker) -> anyhow::Result<String> {
    let resp = worker.post("compile", serde_json::json!({ "source": COUNTER_MODULE })).await?;
    let hash = resp["hash"].as_str().ok_or_else(|| anyhow::anyhow!("no hash"))?.to_string();
    let _ = worker.post("deploy", serde_json::json!({ "hash": hash })).await?;
    Ok(hash)
}

async fn drive(
    worker: Worker,
    hash: String,
    workload: Workload,
    deadline: Instant,
    latencies: Arc<std::sync::Mutex<Vec<u64>>>,
    ok: Arc<AtomicU64>,
    err: Arc<AtomicU64>,
) {
    let body = match workload {
        Workload::Tx => serde_json::json!({
            "tx_source": "module t; fn main() -> u64 { return ctr::bump(); }",
            "deps": [hash],
        }),
        Workload::Query => serde_json::json!({
            "tx_source": "module q; view fn main() -> u64 { return ctr::current(); }",
            "deps": [hash],
        }),
    };
    let path = match workload {
        Workload::Tx => "tx",
        Workload::Query => "query",
    };
    let mut local_lats = Vec::with_capacity(1024);
    while Instant::now() < deadline {
        let t0 = Instant::now();
        match worker.post(path, body.clone()).await {
            Ok(_) => {
                local_lats.push(t0.elapsed().as_micros() as u64);
                ok.fetch_add(1, Ordering::Relaxed);
            }
            Err(_) => {
                err.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    latencies.lock().unwrap().extend(local_lats);
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
    println!("  workers   {}", args.workers);
    println!("  workload  {:?}{}", args.workload, if args.shared { " (shared org)" } else { "" });
    println!("  warmup    {:?}", args.warmup);
    println!("  duration  {:?}", args.duration);

    // Wallets + nonce counters. With --shared, every worker binds to
    // the same EOA AND shares its nonce counter, so they don't fight
    // each other at the auth layer — all contention happens at the
    // OCC commit layer where we want it.
    let workers: Vec<Worker> = if args.shared {
        let wallet = Arc::new(Eoa::new());
        let nonce = Arc::new(AtomicU64::new(1));
        (0..args.workers)
            .map(|_| Worker::new(args.target.clone(), wallet.clone(), nonce.clone()))
            .collect()
    } else {
        (0..args.workers).map(|_| Worker::new(
            args.target.clone(),
            Arc::new(Eoa::new()),
            Arc::new(AtomicU64::new(1)),
        )).collect()
    };

    print!("setup ... ");
    let mut hashes = Vec::with_capacity(workers.len());
    if args.shared {
        // One worker drives the (single) compile+deploy. Others
        // re-use the hash but skip deploy (deploy on an already-
        // deployed namespace just re-runs the constructor, which is
        // a no-op for our counter module).
        let hash = setup(&workers[0]).await?;
        for _ in 0..workers.len() { hashes.push(hash.clone()); }
    } else {
        for w in &workers {
            hashes.push(setup(w).await?);
        }
    }
    println!("ok");

    if !args.warmup.is_zero() {
        print!("warmup {:?} ... ", args.warmup);
        let deadline = Instant::now() + args.warmup;
        let lats = Arc::new(std::sync::Mutex::new(Vec::new()));
        let ok = Arc::new(AtomicU64::new(0));
        let err = Arc::new(AtomicU64::new(0));
        let mut handles = Vec::new();
        for (w, h) in workers.iter().cloned().zip(hashes.iter().cloned()) {
            let lats = lats.clone();
            let ok = ok.clone();
            let err = err.clone();
            handles.push(tokio::spawn(drive(w, h, args.workload, deadline, lats, ok, err)));
        }
        for h in handles { h.await.unwrap(); }
        println!("done");
    }

    let lats = Arc::new(std::sync::Mutex::new(Vec::new()));
    let ok = Arc::new(AtomicU64::new(0));
    let err = Arc::new(AtomicU64::new(0));
    let deadline = Instant::now() + args.duration;
    let started = Instant::now();
    let mut handles = Vec::new();
    for (w, h) in workers.into_iter().zip(hashes.into_iter()) {
        let lats = lats.clone();
        let ok = ok.clone();
        let err = err.clone();
        handles.push(tokio::spawn(drive(w, h, args.workload, deadline, lats, ok, err)));
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
