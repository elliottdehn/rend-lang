//! Bytecode-level optimizations.
//!
//! Currently implements one pass: read clustering. A *cluster* is a
//! maximal run of state-read instructions that can be issued together
//! to the KV via a single `Kv::get_many` round-trip. The pass walks
//! each function's bytecode, identifies clusters, and rewrites them
//! into a single `Instr::ReadBatch` followed by no-op padding.
//!
//! ## What can join a cluster
//!
//! - `KvGet` — reads a whole state slot; key index is compile-time known.
//! - `KvGetPath` — reads a single granular leaf beneath a struct state;
//!   leaf key + type are compile-time known via `PathSpec`.
//!
//! Map-keyed reads (`MapGet`) are not batched yet because their key
//! depends on a runtime register; a future pass could batch a MapGet
//! whose key_reg was finalized before the cluster started.
//!
//! ## What breaks a cluster
//!
//! Any instruction that could:
//!   1. write state (KvPut/KvPutPath/MapPut),
//!   2. call a function that writes or has unknown effects,
//!   3. depend on an in-flight read (we don't yet do dependency
//!      tracking inside a cluster — primitive correctness is to fence
//!      whenever a `Move`/`Bin`/etc. consumes a register that another
//!      instruction in the same cluster wrote into).
//!
//! Pure register operations (`Move`, `Bin`, `Un`, `LoadConst`,
//! `MakeArray`, …) and calls to functions classified as Pure or
//! ReadOnly do *not* break a cluster, provided they don't read from a
//! pending-cluster destination register. We approximate this with a
//! "pending writes" register set; if a non-read instruction's source
//! reads from a pending destination, we flush.
//!
//! ## Why this works
//!
//! State reads commute with each other and with pure computation.
//! Reordering a sequence of reads around any non-writing computation
//! preserves semantics. A `KvGet` returning the same value before vs.
//! after a `Move r1, r2` is unaffected by whether the read or the
//! Move ran first. The fence at writes / impure calls preserves
//! happens-before for any read that depends on prior side effects.

use std::collections::{HashMap, HashSet};

use crate::bc::{BcModule, Instr, ReadOp};
use crate::effects::EffectClass;

/// Apply read-clustering to every function in the module. Modifies the
/// module's `read_groups` and rewrites `code` in place. Idempotent: a
/// second run finds nothing to do because clusters are already
/// `ReadBatch` instructions.
pub fn optimize(module: &mut BcModule, fn_effects: &HashMap<String, EffectClass>) {
    for f in &mut module.functions {
        optimize_fn(f, fn_effects, &module.fn_index);
    }
}

fn optimize_fn(
    f: &mut crate::bc::BcFn,
    fn_effects: &HashMap<String, EffectClass>,
    local_fn_index: &HashMap<String, usize>,
) {
    let mut new_code: Vec<Instr> = Vec::with_capacity(f.code.len());
    let mut cluster: Vec<ReadOp> = Vec::new();
    let mut cluster_old: Vec<usize> = Vec::new();
    let mut cluster_dsts: HashSet<u16> = HashSet::new();
    // Non-read instructions that appeared between cluster reads but
    // didn't depend on any pending dst. Held until we flush so the
    // cluster's reads emit at the position of the *first* original
    // read — preserving any jump target that pointed there.
    let mut pending_nonreads: Vec<(usize, Instr)> = Vec::new();
    let mut idx_map: Vec<Option<usize>> = vec![None; f.code.len()];
    // For each new PC that holds a Jump/JumpIfFalse, the old PC of the
    // original instruction so we can rewrite its relative offset.
    let mut new_jump_old: HashMap<usize, usize> = HashMap::new();

    fn flush(
        cluster: &mut Vec<ReadOp>,
        cluster_old: &mut Vec<usize>,
        cluster_dsts: &mut HashSet<u16>,
        pending_nonreads: &mut Vec<(usize, Instr)>,
        new_code: &mut Vec<Instr>,
        idx_map: &mut [Option<usize>],
        new_jump_old: &mut HashMap<usize, usize>,
        read_groups: &mut Vec<Vec<ReadOp>>,
    ) {
        if !cluster.is_empty() {
            if cluster.len() == 1 {
                let op = cluster.drain(..).next().unwrap();
                let old = cluster_old.drain(..).next().unwrap();
                idx_map[old] = Some(new_code.len());
                match op {
                    ReadOp::State { dst, key_idx } => {
                        new_code.push(Instr::KvGet { dst, key_idx });
                    }
                    ReadOp::Path { dst, path_idx } => {
                        new_code.push(Instr::KvGetPath { dst, path_idx });
                    }
                }
            } else {
                let group_idx = read_groups.len() as u16;
                let batch_pos = new_code.len();
                read_groups.push(std::mem::take(cluster));
                new_code.push(Instr::ReadBatch { group_idx });
                for old in cluster_old.drain(..) {
                    idx_map[old] = Some(batch_pos);
                }
            }
        }
        for (old, instr) in pending_nonreads.drain(..) {
            let new_pc = new_code.len();
            idx_map[old] = Some(new_pc);
            if matches!(instr, Instr::Jump { .. } | Instr::JumpIfFalse { .. }) {
                new_jump_old.insert(new_pc, old);
            }
            new_code.push(instr);
        }
        cluster_dsts.clear();
    }

    for (old_idx, instr) in f.code.iter().enumerate() {
        match instr {
            Instr::KvGet { dst, key_idx } => {
                if reads_pending_dst(instr, &cluster_dsts) {
                    flush(&mut cluster, &mut cluster_old, &mut cluster_dsts,
                          &mut pending_nonreads, &mut new_code, &mut idx_map,
                          &mut new_jump_old, &mut f.read_groups);
                }
                cluster.push(ReadOp::State { dst: *dst, key_idx: *key_idx });
                cluster_old.push(old_idx);
                cluster_dsts.insert(*dst);
            }
            Instr::KvGetPath { dst, path_idx } => {
                if reads_pending_dst(instr, &cluster_dsts) {
                    flush(&mut cluster, &mut cluster_old, &mut cluster_dsts,
                          &mut pending_nonreads, &mut new_code, &mut idx_map,
                          &mut new_jump_old, &mut f.read_groups);
                }
                cluster.push(ReadOp::Path { dst: *dst, path_idx: *path_idx });
                cluster_old.push(old_idx);
                cluster_dsts.insert(*dst);
            }
            other => {
                let must_flush = cluster_breaker(other, fn_effects, local_fn_index)
                    || reads_pending_dst(other, &cluster_dsts);
                if must_flush {
                    flush(&mut cluster, &mut cluster_old, &mut cluster_dsts,
                          &mut pending_nonreads, &mut new_code, &mut idx_map,
                          &mut new_jump_old, &mut f.read_groups);
                    let new_pc = new_code.len();
                    idx_map[old_idx] = Some(new_pc);
                    if matches!(other, Instr::Jump { .. } | Instr::JumpIfFalse { .. }) {
                        new_jump_old.insert(new_pc, old_idx);
                    }
                    new_code.push(other.clone());
                } else if !cluster.is_empty() {
                    // Cluster is in flight; defer emission so the cluster's
                    // ReadBatch sits at the position of the first original
                    // read.
                    pending_nonreads.push((old_idx, other.clone()));
                } else {
                    let new_pc = new_code.len();
                    idx_map[old_idx] = Some(new_pc);
                    if matches!(other, Instr::Jump { .. } | Instr::JumpIfFalse { .. }) {
                        new_jump_old.insert(new_pc, old_idx);
                    }
                    new_code.push(other.clone());
                }
            }
        }
    }
    flush(&mut cluster, &mut cluster_old, &mut cluster_dsts,
          &mut pending_nonreads, &mut new_code, &mut idx_map,
          &mut new_jump_old, &mut f.read_groups);

    let new_len = new_code.len() as i32;
    let map_or_end = |old_target: i32| -> i32 {
        if old_target < 0 { return 0; }
        let ot = old_target as usize;
        if ot >= idx_map.len() { return new_len; }
        for i in ot..idx_map.len() {
            if let Some(v) = idx_map[i] { return v as i32; }
        }
        new_len
    };
    for (new_pc, instr) in new_code.iter_mut().enumerate() {
        if let Instr::Jump { offset } | Instr::JumpIfFalse { offset, .. } = instr {
            let old_pc = *new_jump_old.get(&new_pc).expect("jump origin recorded");
            let old_target = (old_pc as i32) + 1 + *offset;
            let new_target = map_or_end(old_target);
            *offset = new_target - (new_pc as i32) - 1;
        }
    }

    f.code = new_code;
}

/// Whether `instr` would read from any register currently in `pending`.
/// If so, we must flush — the cluster's reads haven't run yet, so the
/// register's value isn't available.
fn reads_pending_dst(instr: &Instr, pending: &HashSet<u16>) -> bool {
    if pending.is_empty() { return false; }
    let mut srcs: Vec<u16> = Vec::new();
    match instr {
        Instr::KvGet { .. } | Instr::KvGetPath { .. } | Instr::LoadConst { .. }
        | Instr::ReturnUnit => {}
        Instr::Move { src, .. } => srcs.push(*src),
        Instr::Bin { lhs, rhs, .. } => { srcs.push(*lhs); srcs.push(*rhs); }
        Instr::Un { src, .. } => srcs.push(*src),
        Instr::Jump { .. } => {}
        Instr::JumpIfFalse { cond, .. } => srcs.push(*cond),
        Instr::Call { args_start, n_args, .. }
        | Instr::CallHost { args_start, n_args, .. }
        | Instr::CallExternal { args_start, n_args, .. }
        | Instr::BuiltinCall { args_start, n_args, .. } => {
            for i in 0..*n_args { srcs.push(args_start + i as u16); }
        }
        Instr::CallExternalDyn { target_reg, args_start, n_args, .. } => {
            srcs.push(*target_reg);
            for i in 0..*n_args { srcs.push(args_start + i as u16); }
        }
        Instr::MakeInterface { target_reg, .. } => srcs.push(*target_reg),
        Instr::Return { src } => srcs.push(*src),
        Instr::ArrayGet { arr, idx, .. } => { srcs.push(*arr); srcs.push(*idx); }
        Instr::ArrayAppend { arr, elem, .. } => { srcs.push(*arr); srcs.push(*elem); }
        Instr::MakeArray { args_start, n, .. } => {
            for i in 0..*n { srcs.push(args_start + i); }
        }
        Instr::MakeStruct { args_start, n, .. } => {
            for i in 0..*n { srcs.push(args_start + i); }
        }
        Instr::FieldGet { src, .. } => srcs.push(*src),
        Instr::FieldSet { dst, val, .. } => { srcs.push(*dst); srcs.push(*val); }
        Instr::MakeSet { args_start, n, .. } => {
            for i in 0..*n { srcs.push(args_start + i); }
        }
        Instr::MakeDict { args_start, n_pairs, .. } => {
            for i in 0..(n_pairs * 2) { srcs.push(args_start + i); }
        }
        Instr::KvPut { src, .. } => srcs.push(*src),
        Instr::KvPutPath { src, .. } => srcs.push(*src),
        Instr::MapGet { key_reg, .. } => srcs.push(*key_reg),
        Instr::MapPut { key_reg, src, .. } => { srcs.push(*key_reg); srcs.push(*src); }
        Instr::Convert { src, .. } => srcs.push(*src),
        Instr::BuiltinResource { src, .. } => srcs.push(*src),
        Instr::BuiltinUnwrap { src, .. } => srcs.push(*src),
        Instr::BuiltinAddress { src, .. } => srcs.push(*src),
        Instr::BuiltinLen { src, .. } => srcs.push(*src),
        Instr::ReadBatch { .. } => {}
        Instr::PrefetchMap { arr_reg, .. } => srcs.push(*arr_reg),
        Instr::Emit { value } => srcs.push(*value),
        // Parallel block markers — treated as cluster fences in the
        // optimizer (the bytecode inside each range is its own
        // optimization unit; the outer cluster ends here).
        Instr::ParallelBegin { .. } => {}
        Instr::ParallelYield { value } => {
            if let Some(v) = value { srcs.push(*v); }
        }
        Instr::ParallelForBegin { source_reg, output_reg, .. } => {
            srcs.push(*source_reg);
            srcs.push(*output_reg);
        }
        Instr::ArrayAlloc { len_reg, default_reg, .. } => {
            srcs.push(*len_reg);
            srcs.push(*default_reg);
        }
        Instr::Reserve { count_reg, .. } => {
            srcs.push(*count_reg);
        }
        Instr::ParallelForSkip => {}
        Instr::Context { .. } => {}
        Instr::MakeTuple { args_start, n, .. } => {
            for i in 0..*n { srcs.push(args_start + i); }
        }
        Instr::TupleGet { src, .. } => srcs.push(*src),
        Instr::MakeEnum { args_start, n, .. } => {
            for i in 0..*n { srcs.push(args_start + i); }
        }
        Instr::EnumTag { src, .. } => srcs.push(*src),
        Instr::EnumPayload { src, .. } => srcs.push(*src),
        Instr::IncReg { reg } => srcs.push(*reg),
        Instr::PMapGet { key_reg, .. } => srcs.push(*key_reg),
        Instr::PMapPut { key_reg, src, .. } => { srcs.push(*key_reg); srcs.push(*src); }
        Instr::PMapContains { key_reg, .. } => srcs.push(*key_reg),
        Instr::PVecGet { idx_reg, .. } => srcs.push(*idx_reg),
        Instr::PVecSet { idx_reg, src, .. } => { srcs.push(*idx_reg); srcs.push(*src); }
        Instr::PVecPush { src, .. } => srcs.push(*src),
        Instr::PVecLen { .. } => {}
        Instr::PMapEntries { .. } | Instr::PMapKeys { .. }
        | Instr::PMapValues { .. } | Instr::PVecToArray { .. } => {}
        Instr::PMapAppendUnique { key_reg, elem_reg, .. } => {
            srcs.push(*key_reg);
            srcs.push(*elem_reg);
        }
        Instr::PMapPutUnique { key_reg, src, .. } => {
            srcs.push(*key_reg);
            srcs.push(*src);
        }
        Instr::PMapWalkInit { .. } => {}
        Instr::PMapWalkNext { cursor_reg, .. } => srcs.push(*cursor_reg),
        Instr::PBTreeGet { key_reg, .. } => srcs.push(*key_reg),
        Instr::PBTreePut { key_reg, src, .. } => { srcs.push(*key_reg); srcs.push(*src); }
        Instr::PBTreeContains { key_reg, .. } => srcs.push(*key_reg),
        Instr::PBTreeRange { lo_reg, hi_reg, .. } => {
            srcs.push(*lo_reg); srcs.push(*hi_reg);
        }
        Instr::PBTreeWalkInit { .. } => {}
        Instr::PBTreeWalkNext { cursor_reg, .. } => srcs.push(*cursor_reg),
        Instr::PBTreePutUnique { key_reg, src, .. } => { srcs.push(*key_reg); srcs.push(*src); }
        Instr::PBTreeAppendUnique { key_reg, elem_reg, .. } => {
            srcs.push(*key_reg); srcs.push(*elem_reg);
        }
        Instr::PMapDelete { key_reg, .. } | Instr::PBTreeDelete { key_reg, .. } => {
            srcs.push(*key_reg);
        }
        Instr::PMapRemoveUnique { key_reg, expected_reg, .. }
        | Instr::PBTreeRemoveUnique { key_reg, expected_reg, .. } => {
            srcs.push(*key_reg); srcs.push(*expected_reg);
        }
        Instr::PMapRemoveFromList { key_reg, elem_reg, .. }
        | Instr::PBTreeRemoveFromList { key_reg, elem_reg, .. } => {
            srcs.push(*key_reg); srcs.push(*elem_reg);
        }
    }
    srcs.iter().any(|r| pending.contains(r))
}

/// Whether `instr` forces a read cluster to flush (i.e., we cannot
/// continue accumulating reads past it).
///
/// Pure register ops and calls to Pure/ReadOnly functions don't break.
/// Anything that writes state, calls into the host, or has unknown
/// effects does break.
fn cluster_breaker(
    instr: &Instr,
    fn_effects: &HashMap<String, EffectClass>,
    local_fn_index: &HashMap<String, usize>,
) -> bool {
    match instr {
        // Direct state writes — definite barrier.
        Instr::KvPut { .. } | Instr::KvPutPath { .. } | Instr::MapPut { .. }
        | Instr::PMapPut { .. } | Instr::PVecSet { .. } | Instr::PVecPush { .. }
        | Instr::PMapAppendUnique { .. } | Instr::PMapPutUnique { .. }
        | Instr::PBTreePut { .. }
        | Instr::PBTreePutUnique { .. } | Instr::PBTreeAppendUnique { .. }
        | Instr::PMapDelete { .. } | Instr::PBTreeDelete { .. }
        | Instr::PMapRemoveUnique { .. } | Instr::PMapRemoveFromList { .. }
        | Instr::PBTreeRemoveUnique { .. } | Instr::PBTreeRemoveFromList { .. } => true,
        // Map / pmap reads have a runtime-computed cell key, so they
        // can't sit *inside* a ReadBatch (whose keys are precomputed
        // at compile time). But they're pure side-effect-free reads —
        // hoisting a batch of compile-time-keyed reads past them is
        // safe, since the batch can only see cells the runtime-keyed
        // read would also see by going through the same Tx cache.
        // So they don't terminate the cluster. Same for
        // PMapContains and PVecGet/PVecLen, which only read.
        Instr::MapGet { .. } | Instr::PMapGet { .. }
        | Instr::PMapContains { .. }
        | Instr::PVecGet { .. } | Instr::PVecLen { .. }
        // Whole-tree walks read N cells but never write — they don't
        // fence the cluster. Their reads happen sequentially since
        // the walk's next-cell-key depends on the previous cell's
        // payload (HAMT bitmap or trie child slot), so they can't
        // *join* the cluster either; they just don't break it.
        | Instr::PMapEntries { .. } | Instr::PMapKeys { .. }
        | Instr::PMapValues { .. } | Instr::PVecToArray { .. }
        // Streaming cursor: init reads the root cell, next pulls
        // one node at a time. Both read-only.
        | Instr::PMapWalkInit { .. } | Instr::PMapWalkNext { .. }
        // pbtree reads — same shape as pmap reads.
        | Instr::PBTreeGet { .. } | Instr::PBTreeContains { .. }
        | Instr::PBTreeRange { .. }
        | Instr::PBTreeWalkInit { .. } | Instr::PBTreeWalkNext { .. } => false,
        // Host calls — unknown effects, full fence.
        Instr::CallHost { .. } => true,
        // Cross-module call — the compiler stamps the callee's
        // declared effect bound onto the instruction at emission
        // time. A `view` or `pure` callee can't write state, so it
        // doesn't fence reads. When the flags can't be resolved at
        // compile time (no manifest) they default to `(false,
        // false)`, matching the old pessimistic behavior.
        Instr::CallExternal { is_view, is_pure, .. } => {
            !(*is_view || *is_pure)
        }
        // Dynamic dispatch — the bound module is unknown until
        // runtime, but the interface method's declared effect
        // bound is baked in at compile time. A `view` or `pure`
        // method can't write state, so it doesn't fence reads.
        Instr::CallExternalDyn { is_view, is_pure, .. } => {
            !(*is_view || *is_pure)
        }
        // Pure register op — wraps a string as Value::Interface.
        Instr::MakeInterface { .. } => false,
        // Same-module call — refine via the local effects map.
        Instr::Call { fn_idx, .. } => {
            let name = local_fn_index
                .iter()
                .find(|(_, idx)| **idx as u16 == *fn_idx)
                .map(|(n, _)| n.clone());
            match name.and_then(|n| fn_effects.get(&n).copied()) {
                Some(EffectClass::Pure) | Some(EffectClass::ReadOnly) => false,
                _ => true,
            }
        }
        // Control flow — clusters can't span blocks. Conservative.
        Instr::Jump { .. } | Instr::JumpIfFalse { .. }
        | Instr::Return { .. } | Instr::ReturnUnit => true,
        // Pure register operations — never a barrier.
        Instr::LoadConst { .. } | Instr::Move { .. } | Instr::Bin { .. }
        | Instr::Un { .. } | Instr::Convert { .. }
        | Instr::ArrayGet { .. } | Instr::ArrayAppend { .. } | Instr::MakeArray { .. }
        | Instr::MakeStruct { .. } | Instr::FieldGet { .. } | Instr::FieldSet { .. }
        | Instr::MakeSet { .. } | Instr::MakeDict { .. } | Instr::BuiltinCall { .. }
        | Instr::BuiltinResource { .. } | Instr::BuiltinUnwrap { .. }
        | Instr::BuiltinAddress { .. } | Instr::BuiltinLen { .. } => false,
        // Reads — handled in the caller, not here.
        Instr::KvGet { .. } | Instr::KvGetPath { .. } | Instr::ReadBatch { .. } => false,
        // Prefetch is a pure cache-warming hint; never breaks a cluster.
        Instr::PrefetchMap { .. } => false,
        // emit appends to the event log but doesn't read or write
        // state — reads on either side commute around it.
        Instr::Emit { .. } => false,
        // Parallel block boundaries — definite barriers. The sub-tasks
        // dispatch inside their own shadow Txs; the outer read cluster
        // can't reach into them.
        Instr::ParallelBegin { .. } | Instr::ParallelYield { .. } => true,
        Instr::ParallelForBegin { .. } => true,
        // Reserve writes a state cell — a definite barrier. ArrayAlloc is a
        // pure register op but appears next to Reserve in workflows where
        // ordering matters; treat it as a barrier conservatively for now.
        Instr::Reserve { .. } => true,
        Instr::ArrayAlloc { .. } => false,
        // Parallel-for-skip terminates a leg without writing back —
        // a control-flow barrier.
        Instr::ParallelForSkip => true,
        // Context reads are pure constants per-tx; never break.
        Instr::Context { .. } => false,
        // Tuple construct/extract are pure register ops.
        Instr::MakeTuple { .. } | Instr::TupleGet { .. } => false,
        // Enum construct/extract are pure register ops.
        Instr::MakeEnum { .. } | Instr::EnumTag { .. } | Instr::EnumPayload { .. } => false,
        // Pure register increment.
        Instr::IncReg { .. } => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bc::Instr;
    use crate::compile;
    use crate::frontend;

    fn compile_optimized(src: &str) -> BcModule {
        let module = frontend(src).unwrap();
        let mut bc = compile::compile(&module).unwrap();
        let effects = crate::effects::classify(&module);
        optimize(&mut bc, &effects);
        bc
    }

    fn count_instrs(bc: &BcModule, fn_name: &str, p: impl Fn(&Instr) -> bool) -> usize {
        let f = bc.functions.iter().find(|f| f.name == fn_name).unwrap();
        f.code.iter().filter(|i| p(i)).count()
    }

    #[test]
    fn two_independent_state_reads_become_one_batch() {
        let bc = compile_optimized("
            state a: i64;
            state b: i64;
            fn main() -> i64 {
                let x = a;
                let y = b;
                return x + y;
            }
        ");
        let kv_gets = count_instrs(&bc, "main", |i| matches!(i, Instr::KvGet { .. }));
        let batches = count_instrs(&bc, "main", |i| matches!(i, Instr::ReadBatch { .. }));
        assert_eq!(kv_gets, 0, "individual reads should be folded into a batch");
        assert_eq!(batches, 1);
        let f = bc.functions.iter().find(|f| f.name == "main").unwrap();
        assert_eq!(f.read_groups.len(), 1);
        assert_eq!(f.read_groups[0].len(), 2);
    }

    #[test]
    fn write_between_reads_breaks_the_cluster() {
        let bc = compile_optimized("
            state a: i64;
            state b: i64;
            fn main() -> i64 {
                let x = a;
                a = 99;             // barrier
                let y = b;
                return x + y;
            }
        ");
        let kv_gets = count_instrs(&bc, "main", |i| matches!(i, Instr::KvGet { .. }));
        let batches = count_instrs(&bc, "main", |i| matches!(i, Instr::ReadBatch { .. }));
        // Two singleton reads (one before, one after) → no batch (clusters
        // of size 1 stay as KvGet).
        assert_eq!(kv_gets, 2);
        assert_eq!(batches, 0);
    }

    #[test]
    fn pure_call_between_reads_doesnt_break_cluster() {
        let bc = compile_optimized("
            state a: i64;
            state b: i64;
            entry fn double(n: i64) -> i64 { return n * 2; }
            fn main() -> i64 {
                let x = a;
                let _z = double(7);   // pure — reads can flow past
                let y = b;
                return x + y;
            }
        ");
        let kv_gets = count_instrs(&bc, "main", |i| matches!(i, Instr::KvGet { .. }));
        let batches = count_instrs(&bc, "main", |i| matches!(i, Instr::ReadBatch { .. }));
        assert_eq!(kv_gets, 0);
        assert_eq!(batches, 1);
    }

    #[test]
    fn dependent_read_breaks_the_cluster() {
        // `let y = a + 1; let z = a;` — the second read of a doesn't
        // depend on x in any way, but `a + 1` reads x's destination
        // register, so the optimizer flushes before the Bin.
        let bc = compile_optimized("
            state a: i64;
            fn main() -> i64 {
                let x = a;
                let y = x + 1;     // depends on x → flushes
                return x + y;
            }
        ");
        // Only one read → no batch (single reads stay as KvGet).
        let kv_gets = count_instrs(&bc, "main", |i| matches!(i, Instr::KvGet { .. }));
        let batches = count_instrs(&bc, "main", |i| matches!(i, Instr::ReadBatch { .. }));
        assert_eq!(kv_gets, 1);
        assert_eq!(batches, 0);
    }

    #[test]
    fn jumps_after_optimization_still_target_correctly() {
        // The optimizer rewrites code positions; jump offsets must be
        // patched. End-to-end correctness check across both branches.
        let module = frontend("
            state a: i64;
            state b: i64;
            fn main() -> i64 {
                let x = a;
                let y = b;
                if x + y >= 0 { return x + y + 30; }
                return -1;
            }
        ").unwrap();
        let mut bc = compile::compile(&module).unwrap();
        let effects = crate::effects::classify(&module);
        optimize(&mut bc, &effects);
        let v = crate::vm::run(&bc, "main", &[], crate::vm::Fuel::new(10_000)).unwrap();
        // Defaults are 0; 0 + 0 >= 0 → take the then-branch → 30.
        assert_eq!(v, crate::value::Value::int(30i64));
    }

    #[test]
    fn five_reads_become_one_batch_end_to_end() {
        let module = frontend("
            state a: i64;
            state b: i64;
            state c: i64;
            state d: i64;
            state e: i64;
            fn main() -> i64 {
                let v1 = a;
                let v2 = b;
                let v3 = c;
                let v4 = d;
                let v5 = e;
                return v1 + v2 + v3 + v4 + v5;
            }
        ").unwrap();
        let mut bc = compile::compile(&module).unwrap();
        let effects = crate::effects::classify(&module);
        optimize(&mut bc, &effects);
        let f = bc.functions.iter().find(|f| f.name == "main").unwrap();
        assert_eq!(f.read_groups.len(), 1);
        assert_eq!(f.read_groups[0].len(), 5);
        let v = crate::vm::run(&bc, "main", &[], crate::vm::Fuel::new(10_000)).unwrap();
        assert_eq!(v, crate::value::Value::int(0i64)); // all defaults
    }
}
