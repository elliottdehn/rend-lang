//! Affine ownership analysis.
//!
//! Tracks each binding's "live / moved" state through control flow. A non-Copy
//! value is moved by:
//!  - using its name as an expression (it transitions to Moved at that span);
//!  - passing it as a function argument (same — the arg expression is a use);
//!  - rebinding via `let new = old;` (the rhs use moves it).
//!
//! Plain `i64` / `bool` / `()` are Copy and never move.
//!
//! At branch joins, a binding is considered Moved if any branch moved it (the
//! conservative side, ensuring soundness).

use std::collections::HashMap;

use crate::ast::*;
use crate::error::{Error, ErrorKind};
use crate::token::Span;

pub fn check(module: &Module) -> Result<(), Error> {
    let sigs = build_sigs(module);
    // Consts are looked up like ambient bindings — each use re-evaluates the
    // RHS, so a read never moves anything.
    let mut states: HashMap<String, Type> = module
        .states
        .iter()
        .map(|s| (s.name.clone(), s.ty.clone()))
        .collect();
    for c in &module.consts {
        states.insert(c.name.clone(), c.ty.clone());
    }
    // Caps need to be recognizable from a `Name { ... }` literal
    // (returned by check_expr) so a `let c = CapName { ... }` binding
    // sees a non-Copy type. Without this map the affine pass treats
    // every struct-literal as `Type::Int` and misses cap moves.
    let cap_decls: HashMap<String, Vec<(String, Type)>> = module
        .caps
        .iter()
        .map(|c| (
            c.name.clone(),
            c.fields.iter().map(|f| (f.name.clone(), f.ty.clone())).collect(),
        ))
        .collect();
    for f in &module.functions {
        check_fn(f, &sigs, &states, &cap_decls)?;
    }
    Ok(())
}

#[derive(Clone)]
struct Binding {
    ty: Type,
    moved_at: Option<Span>,
}

struct FnSig {
    ret: Type,
}

type Env = Vec<HashMap<String, Binding>>;

fn build_sigs(module: &Module) -> HashMap<String, FnSig> {
    let mut sigs = HashMap::new();
    sigs.insert("resource".to_string(), FnSig { ret: Type::Resource });
    sigs.insert("unwrap".to_string(),   FnSig { ret: Type::Int });
    sigs.insert("address".to_string(),  FnSig { ret: Type::Address });
    sigs.insert("len".to_string(),      FnSig { ret: Type::Int });
    sigs.insert("assert".to_string(),   FnSig { ret: Type::Unit });
    sigs.insert("msg_sender".to_string(),      FnSig { ret: Type::Address });
    sigs.insert("block_timestamp".to_string(), FnSig { ret: Type::U64 });
    sigs.insert("block_number".to_string(),    FnSig { ret: Type::U64 });
    sigs.insert("to_bytes".to_string(),     FnSig { ret: Type::Bytes });
    sigs.insert("bytes_len".to_string(),    FnSig { ret: Type::Int });
    sigs.insert("bytes_concat".to_string(), FnSig { ret: Type::Bytes });
    sigs.insert("bytes_eq".to_string(),     FnSig { ret: Type::Bool });
    sigs.insert("bytes_slice".to_string(),  FnSig { ret: Type::Bytes });
    sigs.insert("to_be_bytes".to_string(),   FnSig { ret: Type::Bytes });
    sigs.insert("bit_not_bytes".to_string(), FnSig { ret: Type::Bytes });
    sigs.insert("string_concat".to_string(),   FnSig { ret: Type::String });
    sigs.insert("pmap_contains".to_string(),    FnSig { ret: Type::Bool });
    sigs.insert("pvec_push".to_string(),        FnSig { ret: Type::U64 });
    sigs.insert("pvec_len".to_string(),         FnSig { ret: Type::U64 });
    // Whole-tree walks. The actual return type is `[T]` / `[(K, V)]`,
    // but affine doesn't track array element types — Type::Int is the
    // pass's "I don't care" sentinel.
    sigs.insert("pmap_entries".to_string(),     FnSig { ret: Type::Int });
    sigs.insert("pmap_keys".to_string(),        FnSig { ret: Type::Int });
    sigs.insert("pmap_values".to_string(),      FnSig { ret: Type::Int });
    sigs.insert("pvec_to_array".to_string(),    FnSig { ret: Type::Int });
    // Aggregations. Return type is the element type — affine doesn't
    // care which int kind it is (sentinel Type::Int).
    sigs.insert("sum".to_string(),              FnSig { ret: Type::Int });
    sigs.insert("max".to_string(),              FnSig { ret: Type::Int });
    sigs.insert("min".to_string(),              FnSig { ret: Type::Int });
    // Sorted-trie ops. Same affine sentinel — no non-Copy values
    // pass through them.
    sigs.insert("pbtree_contains".to_string(),  FnSig { ret: Type::Bool });
    sigs.insert("pbtree_range".to_string(),     FnSig { ret: Type::Int });
    // JSON builtins. Affine doesn't track json shape; sentinel ret
    // is fine since none of these produce non-Copy values.
    sigs.insert("parse_json".to_string(),       FnSig { ret: Type::Json });
    sigs.insert("json_stringify".to_string(),   FnSig { ret: Type::String });
    sigs.insert("json_get_field".to_string(),   FnSig { ret: Type::Json });
    sigs.insert("json_get_index".to_string(),   FnSig { ret: Type::Json });
    sigs.insert("json_to_string".to_string(),   FnSig { ret: Type::String });
    sigs.insert("json_to_i64".to_string(),      FnSig { ret: Type::Int });
    sigs.insert("json_to_u64".to_string(),      FnSig { ret: Type::U64 });
    sigs.insert("json_to_bool".to_string(),     FnSig { ret: Type::Bool });
    sigs.insert("json_is_null".to_string(),     FnSig { ret: Type::Bool });
    sigs.insert("string_slice".to_string(),    FnSig { ret: Type::String });
    sigs.insert("string_contains".to_string(), FnSig { ret: Type::Bool });
    sigs.insert("i64".to_string(),      FnSig { ret: Type::Int });
    sigs.insert("i32".to_string(),      FnSig { ret: Type::I32 });
    sigs.insert("u32".to_string(),      FnSig { ret: Type::U32 });
    sigs.insert("u64".to_string(),      FnSig { ret: Type::U64 });
    sigs.insert("u128".to_string(),     FnSig { ret: Type::U128 });
    sigs.insert("set_insert".to_string(),   FnSig { ret: Type::Int });
    sigs.insert("set_remove".to_string(),   FnSig { ret: Type::Int });
    sigs.insert("set_contains".to_string(), FnSig { ret: Type::Bool });
    sigs.insert("set_len".to_string(),      FnSig { ret: Type::Int });
    sigs.insert("dict_set".to_string(),     FnSig { ret: Type::Int });
    sigs.insert("dict_remove".to_string(),  FnSig { ret: Type::Int });
    sigs.insert("dict_get".to_string(),     FnSig { ret: Type::Int });
    sigs.insert("dict_has".to_string(),     FnSig { ret: Type::Bool });
    sigs.insert("dict_len".to_string(),     FnSig { ret: Type::Int });
    for imp in &module.imports {
        sigs.insert(imp.name.clone(), FnSig { ret: imp.return_type.clone() });
    }
    for f in &module.functions {
        sigs.insert(f.name.clone(), FnSig { ret: f.return_type.clone() });
    }
    sigs
}

fn check_fn(
    f: &FnDef,
    sigs: &HashMap<String, FnSig>,
    states: &HashMap<String, Type>,
    cap_decls: &HashMap<String, Vec<(String, Type)>>,
) -> Result<(), Error> {
    let mut env: Env = vec![HashMap::new()];
    for p in &f.params {
        env.last_mut().unwrap().insert(
            p.name.clone(),
            Binding { ty: p.ty.clone(), moved_at: None },
        );
    }
    check_block(&f.body, &mut env, sigs, states, cap_decls)?;
    Ok(())
}

fn check_block(
    block: &Block,
    env: &mut Env,
    sigs: &HashMap<String, FnSig>,
    states: &HashMap<String, Type>,
    cap_decls: &HashMap<String, Vec<(String, Type)>>,
) -> Result<(), Error> {
    env.push(HashMap::new());
    for stmt in &block.stmts {
        check_stmt(stmt, env, sigs, states, cap_decls)?;
    }
    env.pop();
    Ok(())
}

fn check_stmt(
    stmt: &Stmt,
    env: &mut Env,
    sigs: &HashMap<String, FnSig>,
    states: &HashMap<String, Type>,
    cap_decls: &HashMap<String, Vec<(String, Type)>>,
) -> Result<(), Error> {
    match stmt {
        Stmt::Let { name, ty, value, .. } => {
            let v_ty = check_expr(value, env, sigs, states, cap_decls)?;
            let bind_ty = ty.clone().unwrap_or(v_ty);
            env.last_mut().unwrap().insert(
                name.clone(),
                Binding { ty: bind_ty, moved_at: None },
            );
            Ok(())
        }
        Stmt::Assign { target, value, .. } => {
            // analyze target's key sub-expression too (it can move bindings)
            if let ExprKind::Index { key, .. } = &target.kind {
                check_expr(key, env, sigs, states, cap_decls)?;
            }
            check_expr(value, env, sigs, states, cap_decls)?;
            Ok(())
        }
        Stmt::Return { value, .. } => {
            if let Some(e) = value {
                check_expr(e, env, sigs, states, cap_decls)?;
            }
            Ok(())
        }
        Stmt::If(if_stmt) => check_if(if_stmt, env, sigs, states, cap_decls),
        Stmt::While { cond, body, .. } => {
            check_expr(cond, env, sigs, states, cap_decls)?;
            // For MVP we conservatively run the body's analysis once;
            // since loops only allow Copy locals, no moves can compound.
            let snapshot = env.clone();
            check_block(body, env, sigs, states, cap_decls)?;
            *env = snapshot;
            Ok(())
        }
        Stmt::For { var, iter, body, .. } => {
            check_expr(iter, env, sigs, states, cap_decls)?;
            let snapshot = env.clone();
            env.push(HashMap::new());
            env.last_mut().unwrap().insert(
                var.clone(),
                Binding { ty: Type::Int, moved_at: None },
            );
            check_block(body, env, sigs, states, cap_decls)?;
            env.pop();
            *env = snapshot;
            Ok(())
        }
        Stmt::ForRange { var, start, end, body, .. } => {
            check_expr(start, env, sigs, states, cap_decls)?;
            check_expr(end, env, sigs, states, cap_decls)?;
            let snapshot = env.clone();
            env.push(HashMap::new());
            env.last_mut().unwrap().insert(
                var.clone(),
                Binding { ty: Type::Int, moved_at: None },
            );
            check_block(body, env, sigs, states, cap_decls)?;
            env.pop();
            *env = snapshot;
            Ok(())
        }
        Stmt::Break(_) | Stmt::Continue(_) | Stmt::Placeholder(_) => Ok(()),
        Stmt::Expr(e) => {
            check_expr(e, env, sigs, states, cap_decls)?;
            Ok(())
        }
        Stmt::Emit { value, .. } => {
            // The emitted struct value is consumed (serialized into
            // the event log + handed to handlers). Walk the
            // expression to track moves of non-Copy contents.
            check_expr(value, env, sigs, states, cap_decls)?;
            Ok(())
        }
        Stmt::Delete { target, .. } => {
            // The key sub-expression is consumed; the state ident is
            // ambient. Walk both for move tracking on the key.
            check_expr(target, env, sigs, states, cap_decls)?;
            Ok(())
        }
        Stmt::Parallel { stmts, .. } => {
            // Parallel block: walk each inner stmt; affine semantics
            // are unchanged (a `let` inside still binds into the
            // outer scope, since intra-block refs are forbidden).
            for s in stmts {
                check_stmt(s, env, sigs, states, cap_decls)?;
            }
            Ok(())
        }
        Stmt::LetTuple { names, value, .. } => {
            check_expr(value, env, sigs, states, cap_decls)?;
            // Bind each name as Int (placeholder — the affine pass
            // doesn't need precise types for tuple-component locals,
            // since destructured tuples are Copy by construction).
            for n in names {
                env.last_mut().unwrap().insert(
                    n.clone(),
                    Binding { ty: Type::Int, moved_at: None },
                );
            }
            Ok(())
        }
    }
}

fn check_if(
    ifs: &IfStmt,
    env: &mut Env,
    sigs: &HashMap<String, FnSig>,
    states: &HashMap<String, Type>,
    cap_decls: &HashMap<String, Vec<(String, Type)>>,
) -> Result<(), Error> {
    check_expr(&ifs.cond, env, sigs, states, cap_decls)?;
    let snapshot = env.clone();
    check_block(&ifs.then, env, sigs, states, cap_decls)?;
    let after_then = std::mem::replace(env, snapshot);
    match &ifs.else_branch {
        ElseBranch::None => {
            *env = merge(env.clone(), after_then);
        }
        ElseBranch::Block(b) => {
            check_block(b, env, sigs, states, cap_decls)?;
            let after_else = env.clone();
            *env = merge(after_then, after_else);
        }
        ElseBranch::If(inner) => {
            check_if(inner, env, sigs, states, cap_decls)?;
            let after_else = env.clone();
            *env = merge(after_then, after_else);
        }
    }
    Ok(())
}

fn merge(a: Env, b: Env) -> Env {
    a.iter()
        .zip(b.iter())
        .map(|(sa, sb)| {
            let mut out = HashMap::new();
            for (k, va) in sa {
                let vb = sb.get(k).unwrap_or(va);
                let moved_at = va.moved_at.or(vb.moved_at);
                out.insert(
                    k.clone(),
                    Binding { ty: va.ty.clone(), moved_at },
                );
            }
            out
        })
        .collect()
}

fn check_comp_clauses(
    clauses: &[CompClause],
    env: &mut Env,
    sigs: &HashMap<String, FnSig>,
    states: &HashMap<String, Type>,
    cap_decls: &HashMap<String, Vec<(String, Type)>>,
) -> Result<usize, Error> {
    let mut pushed = 0;
    for clause in clauses {
        match clause {
            CompClause::For { var, iter } => {
                check_expr(iter, env, sigs, states, cap_decls)?;
                env.push(HashMap::new());
                env.last_mut().unwrap().insert(
                    var.clone(),
                    Binding { ty: Type::Int, moved_at: None },
                );
                pushed += 1;
            }
            CompClause::If(cond) => {
                check_expr(cond, env, sigs, states, cap_decls)?;
            }
        }
    }
    Ok(pushed)
}

/// Walk a field-access target without consuming bindings. Used by
/// the `Field` arm so that `c.f` doesn't move `c`. Recurses through
/// chained field access; for any non-Ident base (e.g., a function
/// call returning a struct) it falls back to full evaluation, since
/// the value at that point is rvalue-temporary anyway.
fn field_peek(
    expr: &Expr,
    env: &mut Env,
    sigs: &HashMap<String, FnSig>,
    states: &HashMap<String, Type>,
    cap_decls: &HashMap<String, Vec<(String, Type)>>,
) -> Result<(), Error> {
    match &expr.kind {
        ExprKind::Ident(name) => {
            for scope in env.iter() {
                if let Some(b) = scope.get(name) {
                    if let Some(prev) = b.moved_at {
                        return Err(Error::new(
                            ErrorKind::Type,
                            format!(
                                "value '{name}' used after move (moved at {}..{})",
                                prev.start, prev.end
                            ),
                            expr.span,
                        ));
                    }
                    return Ok(());
                }
            }
            // Not a local — could be state. Either way, no move.
            Ok(())
        }
        ExprKind::Field { target, .. } => field_peek(target, env, sigs, states, cap_decls),
        // Anything else evaluates to a temporary; no binding to preserve.
        _ => {
            check_expr(expr, env, sigs, states, cap_decls)?;
            Ok(())
        }
    }
}

fn check_expr(
    expr: &Expr,
    env: &mut Env,
    sigs: &HashMap<String, FnSig>,
    states: &HashMap<String, Type>,
    cap_decls: &HashMap<String, Vec<(String, Type)>>,
) -> Result<Type, Error> {
    match &expr.kind {
        ExprKind::Int(_) => Ok(Type::Int),
        ExprKind::UInt(_) => Ok(Type::UInt),
        ExprKind::Float(_) => Ok(Type::Float),
        ExprKind::JsonObject(_) | ExprKind::JsonArray(_) | ExprKind::JsonNull => {
            Ok(Type::Json)
        }
        ExprKind::I32(_) => Ok(Type::I32),
        ExprKind::U32(_) => Ok(Type::U32),
        ExprKind::U64(_) => Ok(Type::U64),
        ExprKind::U128(_) => Ok(Type::U128),
        ExprKind::Bool(_) => Ok(Type::Bool),
        ExprKind::Str(_) => Ok(Type::String),
        ExprKind::Ident(name) => {
            for scope in env.iter_mut().rev() {
                if let Some(b) = scope.get_mut(name) {
                    if let Some(prev) = b.moved_at {
                        return Err(Error::new(
                            ErrorKind::Type,
                            format!(
                                "value '{name}' used after move (moved at {}..{})",
                                prev.start, prev.end
                            ),
                            expr.span,
                        ));
                    }
                    let ty = b.ty.clone();
                    if !ty.is_copy() {
                        b.moved_at = Some(expr.span);
                    }
                    return Ok(ty);
                }
            }
            // states are ambient and Copy-typed; reading does not move
            if let Some(t) = states.get(name) {
                return Ok(t.clone());
            }
            Err(Error::new(
                ErrorKind::Type,
                format!("undefined variable '{name}'"),
                expr.span,
            ))
        }
        ExprKind::Binary { lhs, rhs, .. } => {
            check_expr(lhs, env, sigs, states, cap_decls)?;
            check_expr(rhs, env, sigs, states, cap_decls)?;
            Ok(Type::Int)
        }
        ExprKind::Unary { operand, .. } => {
            check_expr(operand, env, sigs, states, cap_decls)?;
            Ok(Type::Int)
        }
        ExprKind::DynCall { args, .. } => {
            // Dynamic dispatch: target is an interface ident
            // (Copy), so no move; args are walked as usual.
            for a in args { check_expr(a, env, sigs, states, cap_decls)?; }
            Ok(Type::Int)
        }
        ExprKind::Call { module, name, args } => {
            for a in args {
                check_expr(a, env, sigs, states, cap_decls)?;
            }
            // Cross-module calls have no signature info available in this
            // module's analysis; trust the link step to validate.
            if module.is_some() {
                return Ok(Type::Int);
            }
            sigs.get(name)
                .map(|s| s.ret.clone())
                .ok_or_else(|| Error::new(
                    ErrorKind::Type,
                    format!("unknown function '{name}'"),
                    expr.span,
                ))
        }
        ExprKind::Index { target, key } => {
            check_expr(key, env, sigs, states, cap_decls)?;
            check_expr(target, env, sigs, states, cap_decls)?;
            if let ExprKind::Ident(name) = &target.kind {
                if let Some(Type::Map { value, .. }) = states.get(name) {
                    return Ok((**value).clone());
                }
            }
            // Arrays produce their element type (Copy); we don't need to be
            // precise here since affine only cares whether values move.
            Ok(Type::Int)
        }
        ExprKind::Array(elems) => {
            for e in elems { check_expr(e, env, sigs, states, cap_decls)?; }
            Ok(Type::Int)
        }
        ExprKind::StructLit { name, fields } => {
            for (_, e) in fields { check_expr(e, env, sigs, states, cap_decls)?; }
            // If the literal names a cap, return the matching cap type
            // so `let c = CapName { ... }` produces a non-Copy binding.
            // Plain structs stay as the affine pass's "I don't care"
            // Type::Int sentinel — they're already Copy when their
            // fields are Copy, and we don't track non-Copy fields.
            if let Some(cap_fields) = cap_decls.get(name) {
                Ok(Type::Cap {
                    name: name.clone(),
                    fields: cap_fields.clone(),
                    owner_module: String::new(),
                })
            } else {
                Ok(Type::Int)
            }
        }
        ExprKind::Field { target, .. } => {
            // Reading a field is a *peek*, not a move. A non-Copy
            // struct or cap whose field we read remains usable: the
            // field value is copied out, the parent stays in place.
            // (If we ever support non-Copy fields, the field-itself
            // move would still need handling — but our current types
            // never have non-Copy fields nested.)
            field_peek(target, env, sigs, states, cap_decls)?;
            Ok(Type::Int)
        }
        ExprKind::Prev => Ok(Type::Int),
        ExprKind::Pipe { head, step } => {
            check_expr(head, env, sigs, states, cap_decls)?;
            check_expr(step, env, sigs, states, cap_decls)?;
            Ok(Type::Int)
        }
        ExprKind::ListComp { mapper, clauses }
        | ExprKind::SetComp { mapper, clauses } => {
            let pushed = check_comp_clauses(clauses, env, sigs, states, cap_decls)?;
            check_expr(mapper, env, sigs, states, cap_decls)?;
            for _ in 0..pushed { env.pop(); }
            Ok(Type::Int)
        }
        ExprKind::DictComp { key, value, clauses } => {
            let pushed = check_comp_clauses(clauses, env, sigs, states, cap_decls)?;
            check_expr(key, env, sigs, states, cap_decls)?;
            check_expr(value, env, sigs, states, cap_decls)?;
            for _ in 0..pushed { env.pop(); }
            Ok(Type::Int)
        }
        ExprKind::SetLit(elems) => {
            for e in elems { check_expr(e, env, sigs, states, cap_decls)?; }
            Ok(Type::Int)
        }
        ExprKind::DictLit(pairs) => {
            for (k, v) in pairs {
                check_expr(k, env, sigs, states, cap_decls)?;
                check_expr(v, env, sigs, states, cap_decls)?;
            }
            Ok(Type::Int)
        }
        ExprKind::TupleLit(elems) => {
            for e in elems { check_expr(e, env, sigs, states, cap_decls)?; }
            Ok(Type::Int)
        }
        ExprKind::TupleIndex { target, .. } => {
            check_expr(target, env, sigs, states, cap_decls)?;
            Ok(Type::Int)
        }
        ExprKind::EnumCtor { args, .. } => {
            for a in args { check_expr(a, env, sigs, states, cap_decls)?; }
            Ok(Type::Int)
        }
        ExprKind::Match { scrut, arms } => {
            check_expr(scrut, env, sigs, states, cap_decls)?;
            for arm in arms {
                env.push(HashMap::new());
                if let MatchPattern::EnumVariant { bindings, .. } = &arm.pattern {
                    for n in bindings {
                        env.last_mut().unwrap().insert(
                            n.clone(),
                            Binding { ty: Type::Int, moved_at: None },
                        );
                    }
                }
                check_expr(&arm.body, env, sigs, states, cap_decls)?;
                env.pop();
            }
            Ok(Type::Int)
        }
        ExprKind::Block(block) => {
            env.push(HashMap::new());
            for s in &block.stmts { check_stmt(s, env, sigs, states, cap_decls)?; }
            if let Some(t) = &block.tail { check_expr(t, env, sigs, states, cap_decls)?; }
            env.pop();
            Ok(Type::Int)
        }
        ExprKind::If { cond, then, else_branch } => {
            check_expr(cond, env, sigs, states, cap_decls)?;
            env.push(HashMap::new());
            for s in &then.stmts { check_stmt(s, env, sigs, states, cap_decls)?; }
            if let Some(t) = &then.tail { check_expr(t, env, sigs, states, cap_decls)?; }
            env.pop();
            match else_branch {
                ElseBranch::None => {}
                ElseBranch::Block(b) => {
                    env.push(HashMap::new());
                    for s in &b.stmts { check_stmt(s, env, sigs, states, cap_decls)?; }
                    if let Some(t) = &b.tail { check_expr(t, env, sigs, states, cap_decls)?; }
                    env.pop();
                }
                ElseBranch::If(inner) => {
                    let synthetic = Expr {
                        kind: ExprKind::If {
                            cond: Box::new(inner.cond.clone()),
                            then: inner.then.clone(),
                            else_branch: inner.else_branch.clone(),
                        },
                        span: inner.span,
                    };
                    check_expr(&synthetic, env, sigs, states, cap_decls)?;
                }
            }
            Ok(Type::Int)
        }
    }
}
