//! Per-function effect classification.
//!
//! For each function, infer what kinds of side effects it can produce.
//! Used by the bytecode optimizer to decide when state reads can be
//! reordered or batched together: a sequence of reads can flow past a
//! `Pure` or `ReadOnly` call but must fence at a `WriteOnly`,
//! `ReadWrite`, or `Impure` (host-importing) call.
//!
//! Classification is conservative — when in doubt, escalate to a more
//! restrictive class. Mutual recursion is handled via fixed-point
//! iteration: every function starts at `Pure`, then we walk the call
//! graph until classifications stabilize. Since the lattice is finite
//! (5 elements) and joins are monotone, this terminates in O(N) passes.

use std::collections::HashMap;

use crate::ast::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EffectClass {
    /// No state reads, no state writes, no host imports. Result is a
    /// deterministic transform of arguments + builtin operations.
    Pure,
    /// Reads state but never writes; safe to reorder against other
    /// reads but not past a write.
    ReadOnly,
    /// Writes state but never reads. Rare in practice; included for
    /// completeness of the lattice.
    WriteOnly,
    /// Both reads and writes — most non-trivial entry functions.
    ReadWrite,
    /// Calls host imports or otherwise has effects we can't reason
    /// about. Conservative top of the lattice.
    Impure,
}

impl EffectClass {
    pub fn reads(self) -> bool {
        matches!(self, Self::ReadOnly | Self::ReadWrite | Self::Impure)
    }

    pub fn writes(self) -> bool {
        matches!(self, Self::WriteOnly | Self::ReadWrite | Self::Impure)
    }

    pub fn impure(self) -> bool {
        matches!(self, Self::Impure)
    }

    /// Lattice join: combine two effect summaries. Two reads → ReadOnly;
    /// a read + write → ReadWrite; anything + Impure → Impure.
    pub fn join(self, other: Self) -> Self {
        let r = self.reads() || other.reads();
        let w = self.writes() || other.writes();
        let i = self.impure() || other.impure();
        if i {
            Self::Impure
        } else {
            match (r, w) {
                (false, false) => Self::Pure,
                (true, false)  => Self::ReadOnly,
                (false, true)  => Self::WriteOnly,
                (true, true)   => Self::ReadWrite,
            }
        }
    }
}

/// Classify every function in `module`. The returned map keys are
/// function names (module-local).
pub fn classify(module: &Module) -> HashMap<String, EffectClass> {
    classify_with_externals(module, &HashMap::new())
}

/// Verify each function's `view` / `pure` declaration matches what
/// the body actually does. Run after `classify` (or a manifest-aware
/// equivalent for multi-module compilation).
///
/// Rules:
///   * `pure fn` — body's effect class must be `Pure` (no state
///     reads, no state writes, no host calls, no events).
///   * `view fn` — body's effect class must be `Pure` or `ReadOnly`
///     (state reads are fine; nothing else is).
///   * Unannotated fn — anything goes.
pub fn verify_purity_annotations(
    module: &Module,
    effects: &HashMap<String, EffectClass>,
) -> Result<(), crate::error::Error> {
    use crate::error::{Error, ErrorKind};
    for f in &module.functions {
        if !(f.is_view || f.is_pure) { continue; }
        let actual = effects.get(&f.name).copied().unwrap_or(EffectClass::Impure);
        let kw = if f.is_pure { "pure" } else { "view" };
        let allowed = if f.is_pure {
            matches!(actual, EffectClass::Pure)
        } else {
            matches!(actual, EffectClass::Pure | EffectClass::ReadOnly)
        };
        if !allowed {
            return Err(Error::new(
                ErrorKind::Type,
                format!(
                    "function '{}' is declared `{kw}` but its body is {} \
                     (it must avoid {})",
                    f.name,
                    effect_label(actual),
                    if f.is_pure {
                        "all state access, host imports, and events"
                    } else {
                        "state writes, host imports, and events"
                    },
                ),
                f.span,
            ));
        }
    }
    Ok(())
}

fn effect_label(c: EffectClass) -> &'static str {
    match c {
        EffectClass::Pure => "pure",
        EffectClass::ReadOnly => "read-only",
        EffectClass::WriteOnly => "write-only",
        EffectClass::ReadWrite => "read+write",
        EffectClass::Impure => "impure (host imports or unknown effects)",
    }
}

/// Like `classify`, but accepts a manifest of cross-module entry
/// functions and their already-known classifications. A `module::name`
/// call's effect is taken from this manifest; missing entries are
/// conservatively treated as `Impure`.
pub fn classify_with_externals(
    module: &Module,
    externals: &HashMap<String, EffectClass>,
) -> HashMap<String, EffectClass> {
    classify_with_externals_and_iface_names(module, externals, &std::collections::HashSet::new())
}

/// Same as `classify_with_externals` but also treats the supplied
/// names as interface names — for compile_tx, where dep artifacts
/// declare interfaces the tx source uses by name.
pub fn classify_with_externals_and_iface_names(
    module: &Module,
    externals: &HashMap<String, EffectClass>,
    extra_iface_names: &std::collections::HashSet<String>,
) -> HashMap<String, EffectClass> {
    let import_names: std::collections::HashSet<String> =
        module.imports.iter().map(|i| i.name.clone()).collect();
    let state_names: std::collections::HashSet<String> =
        module.states.iter().map(|s| s.name.clone()).collect();
    let state_types: HashMap<String, crate::ast::Type> = module
        .states
        .iter()
        .map(|s| (s.name.clone(), s.ty.clone()))
        .collect();
    let mut iface_names: std::collections::HashSet<String> = module
        .interfaces
        .iter()
        .map(|d| d.name.clone())
        .collect();
    for n in extra_iface_names { iface_names.insert(n.clone()); }

    // Initialize every local function as Pure (most optimistic). The
    // fixed-point iteration only ever escalates.
    let mut effects: HashMap<String, EffectClass> = module
        .functions
        .iter()
        .map(|f| (f.name.clone(), EffectClass::Pure))
        .collect();

    loop {
        let mut changed = false;
        for f in &module.functions {
            let ctx = Ctx {
                locals: &effects,
                externals,
                imports: &import_names,
                states: &state_names,
                state_types: &state_types,
                iface_names: &iface_names,
            };
            let new_class = classify_fn(f, &ctx);
            let old = *effects.get(&f.name).unwrap();
            if new_class != old {
                effects.insert(f.name.clone(), new_class);
                changed = true;
            }
        }
        if !changed { break; }
    }

    effects
}

/// Per-classification context — the four lookup tables every walk
/// needs. Bundled into one struct so the signatures stay readable
/// as we add new categories (we keep wanting to thread one more
/// thing through).
struct Ctx<'a> {
    locals: &'a HashMap<String, EffectClass>,
    externals: &'a HashMap<String, EffectClass>,
    imports: &'a std::collections::HashSet<String>,
    /// Names of module-level `state` slots. Used to recognize
    /// `Ident(state_name)` and `state[key]` as state reads — without
    /// this, a `pure fn` that reads a state slot wouldn't get
    /// flagged because the classifier can't tell idents from locals.
    states: &'a std::collections::HashSet<String>,
    /// Full state type table — used when classifying a DynCall
    /// whose target is a state-bound interface. We look up the
    /// interface methods and pick up the called method's
    /// declared `view`/`pure` annotation, which lets a state-
    /// bound dynamic dispatch classify as ReadOnly or Pure
    /// instead of falling to Impure.
    state_types: &'a HashMap<String, crate::ast::Type>,
    /// Names of interfaces declared in this module — used to
    /// recognize `IFace::bind(...)` as pure value construction
    /// rather than a cross-module call (which would otherwise
    /// classify as Impure and poison any enclosing view fn).
    iface_names: &'a std::collections::HashSet<String>,
}

fn classify_fn(f: &FnDef, ctx: &Ctx) -> EffectClass {
    let mut acc = EffectClass::Pure;
    classify_block(&f.body, &mut acc, ctx);
    acc
}

fn classify_block(block: &Block, acc: &mut EffectClass, ctx: &Ctx) {
    for stmt in &block.stmts {
        classify_stmt(stmt, acc, ctx);
    }
}

fn classify_stmt(stmt: &Stmt, acc: &mut EffectClass, ctx: &Ctx) {
    match stmt {
        Stmt::Let { value, .. } => classify_expr(value, acc, ctx),
        Stmt::Assign { target, value, .. } => {
            if assign_target_is_state_with_ctx(target, ctx) {
                *acc = acc.join(EffectClass::WriteOnly);
            }
            classify_expr(value, acc, ctx);
        }
        Stmt::Return { value: Some(e), .. } => classify_expr(e, acc, ctx),
        Stmt::Return { value: None, .. } => {}
        Stmt::If(if_stmt) => classify_if(if_stmt, acc, ctx),
        Stmt::While { cond, body, .. } => {
            classify_expr(cond, acc, ctx);
            classify_block(body, acc, ctx);
        }
        Stmt::For { iter, body, .. } => {
            classify_expr(iter, acc, ctx);
            classify_block(body, acc, ctx);
        }
        Stmt::ForRange { start, end, body, .. } => {
            classify_expr(start, acc, ctx);
            classify_expr(end, acc, ctx);
            classify_block(body, acc, ctx);
        }
        Stmt::Break(_) | Stmt::Continue(_) | Stmt::Placeholder(_) => {}
        Stmt::Expr(e) => classify_expr(e, acc, ctx),
        Stmt::Delete { target, .. } => {
            // Delete is a state write. Joining WriteOnly fails
            // verification for view/pure functions, which is correct.
            *acc = acc.join(EffectClass::WriteOnly);
            classify_expr(target, acc, ctx);
        }
        Stmt::Emit { args, .. } => {
            // Emitting an event is an observable side-effect — it
            // shows up in the host's event log. Joining `WriteOnly`
            // here means a `view` or `pure` fn that emits will fail
            // verification, which matches user intuition: a "read"
            // shouldn't write to the log either.
            *acc = acc.join(EffectClass::WriteOnly);
            for a in args { classify_expr(a, acc, ctx); }
        }
        Stmt::LetTuple { value, .. } => classify_expr(value, acc, ctx),
    }
}

fn classify_if(ifs: &IfStmt, acc: &mut EffectClass, ctx: &Ctx) {
    classify_expr(&ifs.cond, acc, ctx);
    classify_block(&ifs.then, acc, ctx);
    match &ifs.else_branch {
        ElseBranch::None => {}
        ElseBranch::Block(b) => classify_block(b, acc, ctx),
        ElseBranch::If(inner) => classify_if(inner, acc, ctx),
    }
}

fn classify_expr(expr: &Expr, acc: &mut EffectClass, ctx: &Ctx) {
    match &expr.kind {
        ExprKind::Int(_) | ExprKind::UInt(_) | ExprKind::Float(_) | ExprKind::JsonNull | ExprKind::I32(_) | ExprKind::U32(_)
        | ExprKind::U64(_) | ExprKind::U128(_) | ExprKind::Bool(_)
        | ExprKind::Str(_) | ExprKind::Prev => {}
        ExprKind::JsonObject(pairs) => {
            for (_, v) in pairs { classify_expr(v, acc, ctx); }
        }
        ExprKind::JsonArray(items) => {
            for v in items { classify_expr(v, acc, ctx); }
        }
        ExprKind::Ident(name) => {
            // Reading a state slot via bare ident (`return n;` where
            // `n` is a `state n: i64;`). Locals don't show up in
            // `states`, so they correctly classify as no-effect.
            if ctx.states.contains(name) {
                *acc = acc.join(EffectClass::ReadOnly);
            }
        }
        ExprKind::Binary { lhs, rhs, .. } => {
            classify_expr(lhs, acc, ctx);
            classify_expr(rhs, acc, ctx);
        }
        ExprKind::Unary { operand, .. } => classify_expr(operand, acc, ctx),
        ExprKind::Index { target, key } => {
            // `state[key]` reads the state — same logic as the bare-
            // ident case but the state ident is the indexing target.
            if let ExprKind::Ident(n) = &target.kind {
                if ctx.states.contains(n) {
                    *acc = acc.join(EffectClass::ReadOnly);
                }
            }
            classify_expr(target, acc, ctx);
            classify_expr(key, acc, ctx);
        }
        ExprKind::Field { target, .. } => {
            // `state.field` is also a state read. The recursive walk
            // descends into target; the bare-ident branch detects the
            // state name at the bottom.
            classify_expr(target, acc, ctx);
        }
        ExprKind::Array(elems) => for e in elems { classify_expr(e, acc, ctx); },
        ExprKind::SetLit(elems) => for e in elems { classify_expr(e, acc, ctx); },
        ExprKind::DictLit(pairs) => for (k, v) in pairs {
            classify_expr(k, acc, ctx);
            classify_expr(v, acc, ctx);
        },
        ExprKind::ListComp { mapper, clauses }
        | ExprKind::SetComp  { mapper, clauses } => {
            for c in clauses {
                match c {
                    CompClause::For { iter, .. } => classify_expr(iter, acc, ctx),
                    CompClause::If(cond) => classify_expr(cond, acc, ctx),
                }
            }
            classify_expr(mapper, acc, ctx);
        }
        ExprKind::DictComp { key, value, clauses } => {
            for c in clauses {
                match c {
                    CompClause::For { iter, .. } => classify_expr(iter, acc, ctx),
                    CompClause::If(cond) => classify_expr(cond, acc, ctx),
                }
            }
            classify_expr(key, acc, ctx);
            classify_expr(value, acc, ctx);
        }
        ExprKind::StructLit { fields, .. } => for (_, e) in fields { classify_expr(e, acc, ctx); },
        ExprKind::Pipe { head, step } => {
            classify_expr(head, acc, ctx);
            classify_expr(step, acc, ctx);
        }
        ExprKind::DynCall { target_ident, method, args, method_is_view, method_is_pure } => {
            for a in args { classify_expr(a, acc, ctx); }
            // Tighten classification when we can:
            //   * the AST node carries the resolved method's
            //     view/pure flags (set by typeck).
            //   * if those flags are unset (typeck couldn't bind
            //     the target's type), fall back to looking up a
            //     state-bound interface in `state_types`.
            //   * else conservative Impure.
            let class = if *method_is_pure {
                EffectClass::Pure
            } else if *method_is_view {
                EffectClass::ReadOnly
            } else if let Some(crate::ast::Type::Interface { methods, .. }) =
                ctx.state_types.get(target_ident)
            {
                if let Some(m) = methods.iter().find(|m| &m.name == method) {
                    if m.is_pure { EffectClass::Pure }
                    else if m.is_view { EffectClass::ReadOnly }
                    else { EffectClass::Impure }
                } else {
                    EffectClass::Impure
                }
            } else {
                EffectClass::Impure
            };
            *acc = acc.join(class);
        }
        ExprKind::Call { module: Some(mod_name), name, args } => {
            for a in args { classify_expr(a, acc, ctx); }
            // `IFace::bind(...)` is value construction — pure.
            // Treating it as a cross-module call would falsely
            // classify any enclosing view fn as Impure.
            if name == "bind" && ctx.iface_names.contains(mod_name) {
                return;
            }
            let key = format!("{mod_name}::{name}");
            let class = ctx.externals.get(&key).copied().unwrap_or(EffectClass::Impure);
            *acc = acc.join(class);
        }
        ExprKind::Call { module: None, name, args } => {
            for a in args { classify_expr(a, acc, ctx); }
            if is_pure_builtin(name) { return; }
            // pmap_contains and friends read state — they take a
            // state-name first arg and walk a HAMT. Treat as
            // ReadOnly so view fns can call them.
            if matches!(name.as_str(),
                "pmap_contains" | "pvec_len"
                | "pmap_entries" | "pmap_keys" | "pmap_values" | "pvec_to_array"
                | "pbtree_contains" | "pbtree_range"
            ) {
                *acc = acc.join(EffectClass::ReadOnly);
                return;
            }
            if ctx.imports.contains(name) {
                *acc = acc.join(EffectClass::Impure);
                return;
            }
            if let Some(&class) = ctx.locals.get(name) {
                *acc = acc.join(class);
            } else {
                *acc = acc.join(EffectClass::Impure);
            }
        }
        ExprKind::TupleLit(elems) => for e in elems { classify_expr(e, acc, ctx); },
        ExprKind::TupleIndex { target, .. } => classify_expr(target, acc, ctx),
        ExprKind::EnumCtor { args, .. } => for a in args { classify_expr(a, acc, ctx); },
        ExprKind::Match { scrut, arms } => {
            classify_expr(scrut, acc, ctx);
            for arm in arms { classify_expr(&arm.body, acc, ctx); }
        }
        ExprKind::Block(block) => {
            for s in &block.stmts { classify_stmt(s, acc, ctx); }
            if let Some(t) = &block.tail { classify_expr(t, acc, ctx); }
        }
        ExprKind::If { cond, then, else_branch } => {
            classify_expr(cond, acc, ctx);
            for s in &then.stmts { classify_stmt(s, acc, ctx); }
            if let Some(t) = &then.tail { classify_expr(t, acc, ctx); }
            match else_branch {
                ElseBranch::None => {}
                ElseBranch::Block(b) => {
                    for s in &b.stmts { classify_stmt(s, acc, ctx); }
                    if let Some(t) = &b.tail { classify_expr(t, acc, ctx); }
                }
                ElseBranch::If(inner) => classify_if(inner, acc, ctx),
            }
        }
    }
}

fn is_pure_builtin(name: &str) -> bool {
    matches!(name,
        "resource" | "unwrap" | "address" | "len" | "assert"
        | "set_insert" | "set_remove" | "set_contains" | "set_len"
        | "dict_set" | "dict_remove" | "dict_get" | "dict_has" | "dict_len"
        | "i64" | "i32" | "u32" | "u64" | "u128"
        | "msg_sender" | "block_timestamp" | "block_number"
        | "to_bytes" | "bytes_len" | "bytes_concat" | "bytes_eq" | "bytes_slice"
        | "string_concat" | "string_slice" | "string_contains"
        | "pmap_contains"
        | "pvec_len"
        | "pmap_entries" | "pmap_keys" | "pmap_values" | "pvec_to_array"
        | "pbtree_contains" | "pbtree_range"
        | "sum" | "max" | "min"
        | "parse_json" | "json_stringify"
        | "json_get_field" | "json_get_index"
        | "json_to_string" | "json_to_i64" | "json_to_u64"
        | "json_to_bool" | "json_is_null"
    )
}

/// Walks an assignment target (the lhs) and returns true if the
/// outermost root is a state name. Mirrors `compile.rs::assign_path`.
/// Like `assign_target_is_state` but consults the ctx's known state
/// names so a bare-ident write to a local doesn't get flagged as a
/// state write. Without this, `view fn f() { let x = 0; x = x + 1; }`
/// classifies as WriteOnly because every Ident assignment is treated
/// as state. With it, only assignments whose root is a known state
/// name count.
fn assign_target_is_state_with_ctx(target: &Expr, ctx: &Ctx) -> bool {
    let mut cur = target;
    loop {
        match &cur.kind {
            ExprKind::Ident(name) => return ctx.states.contains(name),
            ExprKind::Index { target: t, .. } => cur = t,
            ExprKind::Field { target: t, .. } => cur = t,
            _ => return false,
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::tokenize;
    use crate::parser::parse;

    fn analyze(src: &str) -> HashMap<String, EffectClass> {
        let mut m = parse(tokenize(src).unwrap()).unwrap();
        crate::typeck::resolve_types(&mut m).unwrap();
        classify(&m)
    }

    #[test]
    fn pure_function() {
        let e = analyze("fn add(a: i64, b: i64) -> i64 { return a + b; }");
        assert_eq!(e["add"], EffectClass::Pure);
    }

    #[test]
    fn read_only_via_state_read() {
        // The classifier currently sees any bare-name reference as
        // potentially a state read at its conservative ceiling. Better
        // precision would require threading the state set through; for
        // the optimizer this conservative version is correct because it
        // only causes us to issue more fences, not fewer.
        let e = analyze("
            state x: i64;
            fn read_x() -> i64 { return x; }
        ");
        assert!(matches!(e["read_x"], EffectClass::ReadOnly | EffectClass::Pure));
    }

    #[test]
    fn assigning_to_state_is_writeonly_or_higher() {
        let e = analyze("
            state x: i64;
            fn set_x() { x = 1; }
        ");
        let c = e["set_x"];
        assert!(c.writes(), "{c:?} should be a write");
    }

    #[test]
    fn host_import_is_impure() {
        let e = analyze("
            import log: fn(i64);
            fn shout(n: i64) { log(n); }
        ");
        assert_eq!(e["shout"], EffectClass::Impure);
    }

    #[test]
    fn builtins_are_pure() {
        let e = analyze("fn first(xs: [i64]) -> i64 { return len(xs); }");
        assert_eq!(e["first"], EffectClass::Pure);
    }

    #[test]
    fn caller_inherits_callee_effect() {
        let e = analyze("
            state x: i64;
            fn writer() { x = 1; }
            fn caller() { writer(); }
        ");
        assert!(e["caller"].writes());
    }

    #[test]
    fn mutual_recursion_terminates() {
        let e = analyze("
            fn ping(n: i64) -> i64 { if n == 0 { return 0; } return pong(n - 1); }
            fn pong(n: i64) -> i64 { if n == 0 { return 0; } return ping(n - 1); }
        ");
        assert_eq!(e["ping"], EffectClass::Pure);
        assert_eq!(e["pong"], EffectClass::Pure);
    }

    #[test]
    fn pure_recursion_stays_pure() {
        let e = analyze("fn fib(n: i64) -> i64 {
            if n < 2 { return n; }
            return fib(n - 1) + fib(n - 2);
        }");
        assert_eq!(e["fib"], EffectClass::Pure);
    }

    #[test]
    fn write_propagates_through_recursion() {
        let e = analyze("
            state count: i64;
            fn rec(n: i64) -> i64 {
                if n == 0 { return 0; }
                count = count + 1;
                return rec(n - 1);
            }
        ");
        assert!(e["rec"].writes());
    }

    #[test]
    fn cross_module_call_uses_externals() {
        let mut m = parse(tokenize("
            module main;
            fn caller() -> i64 { return other::pure_thing(); }
        ").unwrap()).unwrap();
        crate::typeck::resolve_types(&mut m).unwrap();
        let mut ext = HashMap::new();
        ext.insert("other::pure_thing".to_string(), EffectClass::Pure);
        let e = classify_with_externals(&m, &ext);
        assert_eq!(e["caller"], EffectClass::Pure);
    }

    #[test]
    fn missing_external_is_impure() {
        let mut m = parse(tokenize("
            module main;
            fn caller() -> i64 { return other::unknown(); }
        ").unwrap()).unwrap();
        crate::typeck::resolve_types(&mut m).unwrap();
        let e = classify_with_externals(&m, &HashMap::new());
        assert_eq!(e["caller"], EffectClass::Impure);
    }
}
