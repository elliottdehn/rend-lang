//! Loop-level read prefetching.
//!
//! For a `for x in xs` loop or a `[mapper for x in xs]` comprehension,
//! identify state-map reads inside the body whose key is exactly the
//! iter variable `x`. The compiler emits an `Instr::PrefetchMap`
//! before the loop runs; the VM uses one `Kv::get_many` to warm the
//! tx read-cache for every cell the loop will visit. Per-iteration
//! `MapGet`s then serve from the cache.
//!
//! Two recognition patterns:
//!
//! 1. **Direct**: `state_map[x]` — an `Index` expression whose
//!    `target` is a state map and whose `key` is the iter var.
//!
//! 2. **Indirect**: `f(x)` where `f` is summarized as "returns
//!    `state_map[arg_n]`" — single-statement functions of the form
//!    `fn f(p: T) -> V { return state_map[p]; }` qualify.
//!
//! Safety: prefetching is always semantically transparent because
//! `Tx::read` checks pending writes before the read cache. If an
//! earlier iteration's body writes a cell that a later iteration
//! reads, the write shadows the prefetched value automatically.

use std::collections::HashMap;

use crate::ast::*;

/// Map: function name → "this fn reads `state_name` keyed by parameter
/// at index `param_idx`". Populated by `summarize_fns` for every
/// function in the module that matches the trivial single-map-read
/// shape.
pub type FnSummaries = HashMap<String, MapReadSummary>;

#[derive(Debug, Clone)]
pub struct MapReadSummary {
    pub state_name: String,
    pub param_idx: usize,
}

/// Walk every function in `module`. Functions whose body is exactly
/// `return state_map[p];` (single statement, return of an Index over a
/// state map keyed by a parameter) get a summary recorded so callers
/// can treat the function call as a transparent read for prefetch
/// purposes.
pub fn summarize_fns(
    module: &Module,
    states: &HashMap<String, Type>,
) -> FnSummaries {
    let mut out = HashMap::new();
    for f in &module.functions {
        if let Some(summary) = single_map_read(f, states) {
            out.insert(f.name.clone(), summary);
        }
    }
    out
}

fn single_map_read(f: &FnDef, states: &HashMap<String, Type>) -> Option<MapReadSummary> {
    if f.body.stmts.len() != 1 { return None; }
    let Stmt::Return { value: Some(e), .. } = &f.body.stmts[0] else { return None; };
    let ExprKind::Index { target, key } = &e.kind else { return None; };
    let ExprKind::Ident(state_name) = &target.kind else { return None; };
    if !matches!(states.get(state_name), Some(Type::Map { .. })) { return None; }
    let ExprKind::Ident(key_name) = &key.kind else { return None; };
    let param_idx = f.params.iter().position(|p| p.name == *key_name)?;
    Some(MapReadSummary { state_name: state_name.clone(), param_idx })
}

/// Scan a loop body for prefetchable state maps. Returns the deduplicated
/// set of state names whose `[iter_var]` cells will be read on every
/// iteration. The caller emits one `PrefetchMap` instruction per state.
pub fn scan_loop_body(
    body: &Block,
    iter_var: &str,
    states: &HashMap<String, Type>,
    summaries: &FnSummaries,
) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for stmt in &body.stmts {
        scan_stmt(stmt, iter_var, states, summaries, &mut found, &mut seen);
    }
    found
}

/// Same, but for a comprehension's mapper expression and any `If`
/// clauses that follow the outermost `For`. Used by the comprehension
/// lowering path in compile.rs.
pub fn scan_comp_inner(
    inner_clauses: &[CompClause],
    leaves: &[&Expr],
    iter_var: &str,
    states: &HashMap<String, Type>,
    summaries: &FnSummaries,
) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for clause in inner_clauses {
        match clause {
            CompClause::For { iter, .. } => {
                scan_expr(iter, iter_var, states, summaries, &mut found, &mut seen);
                // We don't descend into a nested For's body — only
                // outermost-loop prefetching for now.
                break;
            }
            CompClause::If(cond) => {
                scan_expr(cond, iter_var, states, summaries, &mut found, &mut seen);
            }
        }
    }
    for leaf in leaves {
        scan_expr(leaf, iter_var, states, summaries, &mut found, &mut seen);
    }
    found
}

fn scan_stmt(
    stmt: &Stmt,
    iter_var: &str,
    states: &HashMap<String, Type>,
    summaries: &FnSummaries,
    found: &mut Vec<String>,
    seen: &mut std::collections::HashSet<String>,
) {
    match stmt {
        Stmt::Let { value, .. } => scan_expr(value, iter_var, states, summaries, found, seen),
        Stmt::Assign { target, value, .. } => {
            scan_expr(target, iter_var, states, summaries, found, seen);
            scan_expr(value, iter_var, states, summaries, found, seen);
        }
        Stmt::Return { value: Some(e), .. } => scan_expr(e, iter_var, states, summaries, found, seen),
        Stmt::Return { value: None, .. } => {}
        Stmt::If(if_stmt) => scan_if(if_stmt, iter_var, states, summaries, found, seen),
        Stmt::While { cond, body, .. } => {
            scan_expr(cond, iter_var, states, summaries, found, seen);
            for s in &body.stmts {
                scan_stmt(s, iter_var, states, summaries, found, seen);
            }
        }
        Stmt::For { iter, body, var: inner_var, .. } => {
            scan_expr(iter, iter_var, states, summaries, found, seen);
            // A nested for-loop introduces its own iter var; reads
            // keyed by *that* var aren't prefetchable from the outer
            // loop's perspective. Only descend into reads that
            // reference the OUTER iter_var.
            if inner_var != iter_var {
                for s in &body.stmts {
                    scan_stmt(s, iter_var, states, summaries, found, seen);
                }
            }
        }
        Stmt::ForRange { start, end, body, var: inner_var, .. } => {
            scan_expr(start, iter_var, states, summaries, found, seen);
            scan_expr(end, iter_var, states, summaries, found, seen);
            if inner_var != iter_var {
                for s in &body.stmts {
                    scan_stmt(s, iter_var, states, summaries, found, seen);
                }
            }
        }
        Stmt::Break(_) | Stmt::Continue(_) | Stmt::Placeholder(_) => {}
        Stmt::Expr(e) => scan_expr(e, iter_var, states, summaries, found, seen),
        Stmt::Emit { value, .. } => {
            // The emitted struct value may carry iter-var-keyed reads;
            // scan it so those reads still feed loop prefetching.
            scan_expr(value, iter_var, states, summaries, found, seen);
        }
        Stmt::Delete { target, .. } => {
            scan_expr(target, iter_var, states, summaries, found, seen);
        }
        Stmt::LetTuple { value, .. } => {
            scan_expr(value, iter_var, states, summaries, found, seen);
        }
        Stmt::Parallel { stmts, .. } => {
            for s in stmts {
                scan_stmt(s, iter_var, states, summaries, found, seen);
            }
        }
        Stmt::ParallelForTo { source, output, body, .. } => {
            scan_expr(source, iter_var, states, summaries, found, seen);
            scan_expr(output, iter_var, states, summaries, found, seen);
            for s in &body.stmts {
                scan_stmt(s, iter_var, states, summaries, found, seen);
            }
        }
    }
}

fn scan_if(
    ifs: &IfStmt,
    iter_var: &str,
    states: &HashMap<String, Type>,
    summaries: &FnSummaries,
    found: &mut Vec<String>,
    seen: &mut std::collections::HashSet<String>,
) {
    scan_expr(&ifs.cond, iter_var, states, summaries, found, seen);
    for s in &ifs.then.stmts { scan_stmt(s, iter_var, states, summaries, found, seen); }
    match &ifs.else_branch {
        ElseBranch::None => {}
        ElseBranch::Block(b) => for s in &b.stmts { scan_stmt(s, iter_var, states, summaries, found, seen); }
        ElseBranch::If(inner) => scan_if(inner, iter_var, states, summaries, found, seen),
    }
}

fn scan_expr(
    expr: &Expr,
    iter_var: &str,
    states: &HashMap<String, Type>,
    summaries: &FnSummaries,
    found: &mut Vec<String>,
    seen: &mut std::collections::HashSet<String>,
) {
    match &expr.kind {
        // Pattern 1: state_map[iter_var]
        ExprKind::Index { target, key } => {
            if let (ExprKind::Ident(state_name), ExprKind::Ident(key_name)) = (&target.kind, &key.kind) {
                if key_name == iter_var
                    && matches!(states.get(state_name), Some(Type::Map { .. }))
                    && seen.insert(state_name.clone())
                {
                    found.push(state_name.clone());
                }
            }
            scan_expr(target, iter_var, states, summaries, found, seen);
            scan_expr(key, iter_var, states, summaries, found, seen);
        }
        // Pattern 2: f(iter_var) where f is a single-map-read function
        ExprKind::Call { module: None, name, args } => {
            if let Some(s) = summaries.get(name) {
                if let Some(arg) = args.get(s.param_idx) {
                    if let ExprKind::Ident(arg_name) = &arg.kind {
                        if arg_name == iter_var && seen.insert(s.state_name.clone()) {
                            found.push(s.state_name.clone());
                        }
                    }
                }
            }
            for a in args { scan_expr(a, iter_var, states, summaries, found, seen); }
        }
        // Recurse through other compound shapes — a prefetchable read
        // can hide inside any of these.
        ExprKind::Binary { lhs, rhs, .. } => {
            scan_expr(lhs, iter_var, states, summaries, found, seen);
            scan_expr(rhs, iter_var, states, summaries, found, seen);
        }
        ExprKind::Unary { operand, .. } => scan_expr(operand, iter_var, states, summaries, found, seen),
        ExprKind::Field { target, .. } => scan_expr(target, iter_var, states, summaries, found, seen),
        ExprKind::Array(elems) | ExprKind::SetLit(elems) => {
            for e in elems { scan_expr(e, iter_var, states, summaries, found, seen); }
        }
        ExprKind::ArrayAlloc { len, .. } => {
            scan_expr(len, iter_var, states, summaries, found, seen);
        }
        ExprKind::Reserve { count, .. } => {
            scan_expr(count, iter_var, states, summaries, found, seen);
        }
        ExprKind::DictLit(pairs) => {
            for (k, v) in pairs {
                scan_expr(k, iter_var, states, summaries, found, seen);
                scan_expr(v, iter_var, states, summaries, found, seen);
            }
        }
        ExprKind::StructLit { fields, .. } => {
            for (_, e) in fields { scan_expr(e, iter_var, states, summaries, found, seen); }
        }
        ExprKind::Pipe { head, step } => {
            scan_expr(head, iter_var, states, summaries, found, seen);
            scan_expr(step, iter_var, states, summaries, found, seen);
        }
        ExprKind::Call { module: Some(_), args, .. } => {
            // Cross-module: we don't have summaries, so we can't see
            // through it. Recurse into args in case the iter var is
            // used elsewhere.
            for a in args { scan_expr(a, iter_var, states, summaries, found, seen); }
        }
        ExprKind::DynCall { args, .. } => {
            // Same shape as cross-module: opaque target.
            for a in args { scan_expr(a, iter_var, states, summaries, found, seen); }
        }
        ExprKind::ListComp { mapper, clauses } | ExprKind::SetComp { mapper, clauses } => {
            scan_expr(mapper, iter_var, states, summaries, found, seen);
            for c in clauses {
                match c {
                    CompClause::For { iter, var } => {
                        scan_expr(iter, iter_var, states, summaries, found, seen);
                        // Inner clauses' var shadows our iter — stop
                        // descending once shadowed (any further reads
                        // are keyed by the inner var, not ours).
                        if var == iter_var { return; }
                    }
                    CompClause::If(cond) => scan_expr(cond, iter_var, states, summaries, found, seen),
                }
            }
        }
        ExprKind::DictComp { key, value, clauses } => {
            scan_expr(key, iter_var, states, summaries, found, seen);
            scan_expr(value, iter_var, states, summaries, found, seen);
            for c in clauses {
                match c {
                    CompClause::For { iter, var } => {
                        scan_expr(iter, iter_var, states, summaries, found, seen);
                        if var == iter_var { return; }
                    }
                    CompClause::If(cond) => scan_expr(cond, iter_var, states, summaries, found, seen),
                }
            }
        }
        ExprKind::TupleLit(elems) => {
            for e in elems { scan_expr(e, iter_var, states, summaries, found, seen); }
        }
        ExprKind::TupleIndex { target, .. } => {
            scan_expr(target, iter_var, states, summaries, found, seen);
        }
        ExprKind::EnumCtor { args, .. } => {
            for a in args { scan_expr(a, iter_var, states, summaries, found, seen); }
        }
        ExprKind::Match { scrut, arms } => {
            scan_expr(scrut, iter_var, states, summaries, found, seen);
            for arm in arms {
                scan_expr(&arm.body, iter_var, states, summaries, found, seen);
            }
        }
        ExprKind::Block(block) => {
            for s in &block.stmts { scan_stmt(s, iter_var, states, summaries, found, seen); }
            if let Some(t) = &block.tail { scan_expr(t, iter_var, states, summaries, found, seen); }
        }
        ExprKind::If { cond, then, else_branch } => {
            scan_expr(cond, iter_var, states, summaries, found, seen);
            for s in &then.stmts { scan_stmt(s, iter_var, states, summaries, found, seen); }
            if let Some(t) = &then.tail { scan_expr(t, iter_var, states, summaries, found, seen); }
            match else_branch {
                ElseBranch::None => {}
                ElseBranch::Block(b) => {
                    for s in &b.stmts { scan_stmt(s, iter_var, states, summaries, found, seen); }
                    if let Some(t) = &b.tail { scan_expr(t, iter_var, states, summaries, found, seen); }
                }
                ElseBranch::If(inner) => scan_if(inner, iter_var, states, summaries, found, seen),
            }
        }
        ExprKind::Int(_) | ExprKind::UInt(_) | ExprKind::Float(_) | ExprKind::JsonNull | ExprKind::I32(_) | ExprKind::U32(_) | ExprKind::U64(_)
        | ExprKind::U128(_) | ExprKind::Bool(_) | ExprKind::Str(_) | ExprKind::Ident(_)
        | ExprKind::Prev => {}
        ExprKind::JsonObject(pairs) => {
            for (_, v) in pairs { scan_expr(v, iter_var, states, summaries, found, seen); }
        }
        ExprKind::JsonArray(items) => {
            for v in items { scan_expr(v, iter_var, states, summaries, found, seen); }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::tokenize;
    use crate::parser::parse;

    fn parse_and_resolve(src: &str) -> (Module, HashMap<String, Type>) {
        let mut m = parse(tokenize(src).unwrap()).unwrap();
        crate::typeck::resolve_types(&mut m).unwrap();
        let states: HashMap<String, Type> = m.states.iter()
            .map(|s| (s.name.clone(), s.ty.clone()))
            .collect();
        (m, states)
    }

    #[test]
    fn summarizes_single_map_read_function() {
        let (m, states) = parse_and_resolve("
            state balances: map<i64, i64>;
            entry fn read_balance(a: i64) -> i64 { return balances[a]; }
        ");
        let s = summarize_fns(&m, &states);
        let summary = s.get("read_balance").unwrap();
        assert_eq!(summary.state_name, "balances");
        assert_eq!(summary.param_idx, 0);
    }

    #[test]
    fn does_not_summarize_multi_statement_function() {
        let (m, states) = parse_and_resolve("
            state balances: map<i64, i64>;
            entry fn complex(a: i64) -> i64 {
                let x = balances[a];
                return x + 1;
            }
        ");
        let s = summarize_fns(&m, &states);
        assert!(!s.contains_key("complex"));
    }

    #[test]
    fn finds_direct_state_read_in_for_body() {
        let (m, states) = parse_and_resolve("
            state balances: map<i64, i64>;
            fn main() -> i64 {
                let total = 0;
                for a in [1, 2, 3] {
                    total = total + balances[a];
                }
                return total;
            }
        ");
        let summaries = summarize_fns(&m, &states);
        let main = m.functions.iter().find(|f| f.name == "main").unwrap();
        let Stmt::For { var, body, .. } = &main.body.stmts[1] else { panic!() };
        let targets = scan_loop_body(body, var, &states, &summaries);
        assert_eq!(targets, vec!["balances"]);
    }

    #[test]
    fn finds_indirect_read_via_summarized_fn() {
        let (m, states) = parse_and_resolve("
            state balances: map<i64, i64>;
            entry fn read_balance(a: i64) -> i64 { return balances[a]; }
            fn main() -> i64 {
                let total = 0;
                for a in [1, 2, 3] {
                    total = total + read_balance(a);
                }
                return total;
            }
        ");
        let summaries = summarize_fns(&m, &states);
        let main = m.functions.iter().find(|f| f.name == "main").unwrap();
        let Stmt::For { var, body, .. } = &main.body.stmts[1] else { panic!() };
        let targets = scan_loop_body(body, var, &states, &summaries);
        assert_eq!(targets, vec!["balances"]);
    }

    #[test]
    fn ignores_reads_not_keyed_by_iter_var() {
        let (m, states) = parse_and_resolve("
            state balances: map<i64, i64>;
            fn main() -> i64 {
                let k = 99;
                let total = 0;
                for a in [1, 2, 3] {
                    total = total + balances[k];
                }
                return total;
            }
        ");
        let summaries = summarize_fns(&m, &states);
        let main = m.functions.iter().find(|f| f.name == "main").unwrap();
        let Stmt::For { var, body, .. } = &main.body.stmts[2] else { panic!() };
        let targets = scan_loop_body(body, var, &states, &summaries);
        assert!(targets.is_empty());
    }

    #[test]
    fn dedupes_repeated_reads_of_same_state() {
        let (m, states) = parse_and_resolve("
            state balances: map<i64, i64>;
            fn main() -> i64 {
                let total = 0;
                for a in [1, 2, 3] {
                    total = total + balances[a];
                    total = total + balances[a];
                }
                return total;
            }
        ");
        let summaries = summarize_fns(&m, &states);
        let main = m.functions.iter().find(|f| f.name == "main").unwrap();
        let Stmt::For { var, body, .. } = &main.body.stmts[1] else { panic!() };
        let targets = scan_loop_body(body, var, &states, &summaries);
        assert_eq!(targets, vec!["balances"]);
    }
}
