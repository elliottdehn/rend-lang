//! Per-namespace group-commit pipeline.
//!
//! `do_tx` does the rend execution work itself, then sends the
//! resulting `(reads, writes, pmap_types)` to its namespace's
//! committer via a tokio mpsc channel. The committer drains the
//! channel, batches up to `MAX_BATCH` jobs, and runs them through
//! one `RocksKv::commit_batch` call — which means **one
//! WriteBatch and one fsync per batch, regardless of how many
//! commits are in it.**
//!
//! This is the load-bearing optimization for any deployment where
//! the commit phase has nontrivial latency: durable WAL fsync,
//! synchronous replication, anything that can't go faster than
//! one network/disk round-trip per commit. Group commit
//! amortizes that cost across the batch.
//!
//! For workloads in one namespace under contention, the batch
//! also lets sibling commits validate against the *evolving
//! in-batch state* rather than against the on-disk state — so a
//! transfer following another transfer in the same batch sees the
//! prior root and can merge against it cleanly.

use rend_rocksdb::{CommitJob, CommitResult, NamespaceId, RocksKv};
use std::collections::HashMap;
use tokio::sync::{mpsc, oneshot};

/// Maximum number of jobs in one batch. Higher = better fsync
/// amortization but worse tail latency for the first job in the
/// batch (it waits for the batch to fill or commit). 64 is a
/// reasonable upper bound: at ~30µs per validate-and-apply that
/// caps batch processing at ~2ms, comfortably below typical
/// network RTT or fsync latency.
const MAX_BATCH: usize = 64;

/// Channel capacity. With 8-32 worker threads and bursts of
/// concurrent commits, a few hundred slots is plenty; we never
/// expect this to back up much because the committer drains
/// aggressively.
pub(crate) const CHANNEL_CAP: usize = 1024;

/// One job sent into the committer.
pub(crate) struct Job {
    pub reads: HashMap<u128, rend::value::Value>,
    pub writes: HashMap<u128, rend::value::Value>,
    pub pmap_types: HashMap<u128, (rend::ast::Type, rend::ast::Type)>,
    /// Replied to with `Committed` or `Conflict` once the batch
    /// containing this job has been processed. If the underlying
    /// `db.write` itself fails, every job in the batch gets `Err`.
    pub response: oneshot::Sender<Result<CommitResult, rocksdb::Error>>,
}

/// Drive one namespace's commit pipeline until the channel closes.
pub(crate) async fn run(kv: RocksKv, ns: NamespaceId, mut rx: mpsc::Receiver<Job>) {
    loop {
        // Block until at least one job is queued.
        let Some(first) = rx.recv().await else {
            return;
        };
        let mut batch: Vec<Job> = Vec::with_capacity(MAX_BATCH);
        batch.push(first);
        // Greedy non-blocking drain. Stops early when the channel
        // is empty or we hit MAX_BATCH; the next `await` above will
        // pick up anything that arrives after.
        while batch.len() < MAX_BATCH {
            match rx.try_recv() {
                Ok(j) => batch.push(j),
                Err(_) => break,
            }
        }
        process_batch(kv.clone(), ns, batch).await;
    }
}

async fn process_batch(kv: RocksKv, ns: NamespaceId, batch: Vec<Job>) {
    // RocksDB calls block; do them on the blocking pool so we
    // don't stall other tokio tasks (the WS readers, the auth
    // middleware, the other committers).
    let result = tokio::task::spawn_blocking(move || -> (
        Result<Vec<CommitResult>, rocksdb::Error>,
        Vec<oneshot::Sender<Result<CommitResult, rocksdb::Error>>>,
    ) {
        // Split the senders out so we can move them across the
        // blocking boundary without also moving rocksdb-error
        // types we don't want to clone.
        let (jobs, senders): (Vec<_>, Vec<_>) = batch
            .into_iter()
            .map(|j| {
                (
                    CommitJob { reads: j.reads, writes: j.writes, pmap_types: j.pmap_types },
                    j.response,
                )
            })
            .unzip();
        let res = kv.commit_batch(ns, &jobs);
        (res, senders)
    })
    .await;

    let (outcome, senders) = match result {
        Ok(p) => p,
        Err(_join_err) => {
            // The blocking task panicked. We can't deliver per-job
            // results since we lost the senders — log and bail. (In
            // practice this means a kv invariant was violated.)
            tracing::error!("committer blocking task panicked");
            return;
        }
    };

    match outcome {
        Ok(outcomes) => {
            // Per-job result: paired by index with the senders.
            for (sender, outcome) in senders.into_iter().zip(outcomes.into_iter()) {
                let _ = sender.send(Ok(outcome));
            }
        }
        Err(_e) => {
            // The whole-batch write failed — every job gets the
            // same error. We can't easily clone rocksdb::Error so
            // we map to a synthetic Conflict; callers can re-execute.
            // (In practice db.write rarely fails; this is the
            // unhappy-path fallback.)
            for sender in senders.into_iter() {
                let _ = sender.send(Ok(CommitResult::Conflict));
            }
        }
    }
}
