//! Modifier expansion. Runs as a frontend pass between parse and
//! typeck. Each `FnDef` whose `modifiers: Vec<(name, args)>` is
//! non-empty has its body rewritten so the wrapped function ends up
//! nested inside the modifier(s), with `_;` placeholders replaced by
//! the previous level's statements.
//!
//! Outermost-leftmost is the convention: `[A, B] fn foo() { S; }`
//! expands to A's body with `_;` → B's body with `_;` → `{ S; }`.
//! Modifier params are bound by inlining the call-site argument
//! expressions in place of the param names — i.e., textual
//! substitution at the AST level. Modifiers can't shadow their own
//! params (the substitution doesn't track scope; users shouldn't
//! reuse a param name as a let-bound local in a modifier body).
//!
//! After this pass, every fn's `modifiers` list is cleared and every
//! `Stmt::Placeholder` has been removed. typeck operates on the
//! plain expanded fn.

use std::collections::HashMap;

use crate::ast::*;
use crate::error::{Error, ErrorKind};

pub fn expand(module: &mut Module) -> Result<(), Error> {
    // Index modifiers by name; reject duplicates.
    let mut by_name: HashMap<String, ModifierDecl> = HashMap::new();
    for m in &module.modifiers {
        if by_name.insert(m.name.clone(), m.clone()).is_some() {
            return Err(Error::new(
                ErrorKind::Type,
                format!("duplicate modifier '{}'", m.name),
                m.span,
            ));
        }
    }

    for f in module.functions.iter_mut() {
        if f.modifiers.is_empty() {
            // Still need to reject any stray `_;` outside a modifier
            // body (caught here as "unknown placeholder").
            ensure_no_placeholders(&f.body, f.span)?;
            continue;
        }
        // Take ownership of the applied list so we can clear `f.modifiers`.
        let applied = std::mem::take(&mut f.modifiers);

        // Build the wrapped body by folding modifiers right-to-left:
        // start from the original fn body, then for each modifier
        // (innermost first), wrap with that modifier's body.
        let mut current = f.body.stmts.clone();
        for (mod_name, args) in applied.iter().rev() {
            let m = by_name.get(mod_name).ok_or_else(|| Error::new(
                ErrorKind::Type,
                format!("unknown modifier '{mod_name}'"),
                f.span,
            ))?;
            if args.len() != m.params.len() {
                return Err(Error::new(
                    ErrorKind::Type,
                    format!(
                        "modifier '{mod_name}' expects {} arg(s), got {}",
                        m.params.len(),
                        args.len(),
                    ),
                    f.span,
                ));
            }
            // Bind params → arg expressions.
            let mut subst: HashMap<String, Expr> = HashMap::new();
            for (p, a) in m.params.iter().zip(args.iter()) {
                subst.insert(p.name.clone(), a.clone());
            }
            // Substitute params and replace `_;` with `current`.
            let mut new_stmts = Vec::with_capacity(m.body.stmts.len());
            let mut saw_placeholder = false;
            for stmt in &m.body.stmts {
                match stmt {
                    Stmt::Placeholder(_) => {
                        saw_placeholder = true;
                        new_stmts.extend(current.iter().cloned());
                    }
                    other => {
                        let mut out = other.clone();
                        substitute_params_in_stmt(&mut out, &subst);
                        new_stmts.push(out);
                    }
                }
            }
            if !saw_placeholder {
                return Err(Error::new(
                    ErrorKind::Type,
                    format!("modifier '{mod_name}' has no `_;` placeholder"),
                    m.span,
                ));
            }
            current = new_stmts;
        }
        f.body = Block { stmts: current, tail: f.body.tail.take(), span: f.body.span };
        // Final sanity: no leftover placeholders.
        ensure_no_placeholders(&f.body, f.span)?;
    }
    Ok(())
}

fn ensure_no_placeholders(block: &Block, fn_span: crate::token::Span) -> Result<(), Error> {
    for stmt in &block.stmts {
        match stmt {
            Stmt::Placeholder(span) => {
                return Err(Error::new(
                    ErrorKind::Type,
                    "`_;` is only valid inside a modifier body".to_string(),
                    *span,
                ));
            }
            Stmt::If(if_stmt) => check_if_for_placeholder(if_stmt, fn_span)?,
            Stmt::While { body, .. } | Stmt::For { body, .. } => {
                ensure_no_placeholders(body, fn_span)?;
            }
            _ => {}
        }
    }
    Ok(())
}

fn check_if_for_placeholder(ifs: &IfStmt, fn_span: crate::token::Span) -> Result<(), Error> {
    ensure_no_placeholders(&ifs.then, fn_span)?;
    match &ifs.else_branch {
        ElseBranch::None => {}
        ElseBranch::Block(b) => ensure_no_placeholders(b, fn_span)?,
        ElseBranch::If(inner) => check_if_for_placeholder(inner, fn_span)?,
    }
    Ok(())
}

// ---- AST-level substitution ----

fn substitute_params_in_stmt(stmt: &mut Stmt, subst: &HashMap<String, Expr>) {
    match stmt {
        Stmt::Let { value, .. } => substitute_in_expr(value, subst),
        Stmt::Assign { target, value, .. } => {
            substitute_in_expr(target, subst);
            substitute_in_expr(value, subst);
        }
        Stmt::Return { value: Some(e), .. } => substitute_in_expr(e, subst),
        Stmt::Return { value: None, .. } => {}
        Stmt::If(if_stmt) => substitute_in_if(if_stmt, subst),
        Stmt::While { cond, body, .. } => {
            substitute_in_expr(cond, subst);
            for s in &mut body.stmts { substitute_params_in_stmt(s, subst); }
        }
        Stmt::For { iter, body, .. } => {
            substitute_in_expr(iter, subst);
            for s in &mut body.stmts { substitute_params_in_stmt(s, subst); }
        }
        Stmt::ForRange { start, end, body, .. } => {
            substitute_in_expr(start, subst);
            substitute_in_expr(end, subst);
            for s in &mut body.stmts { substitute_params_in_stmt(s, subst); }
        }
        Stmt::Break(_) | Stmt::Continue(_) | Stmt::Placeholder(_) => {}
        Stmt::Expr(e) => substitute_in_expr(e, subst),
        Stmt::Emit { value, .. } => substitute_in_expr(value, subst),
        Stmt::Delete { target, .. } => substitute_in_expr(target, subst),
        Stmt::LetTuple { value, .. } => substitute_in_expr(value, subst),
        Stmt::Parallel { stmts, .. } => {
            for s in stmts { substitute_params_in_stmt(s, subst); }
        }
        Stmt::ParallelForTo { source, output, body, .. } => {
            substitute_in_expr(source, subst);
            substitute_in_expr(output, subst);
            for s in &mut body.stmts { substitute_params_in_stmt(s, subst); }
        }
    }
}

fn substitute_in_if(ifs: &mut IfStmt, subst: &HashMap<String, Expr>) {
    substitute_in_expr(&mut ifs.cond, subst);
    for s in &mut ifs.then.stmts { substitute_params_in_stmt(s, subst); }
    match &mut ifs.else_branch {
        ElseBranch::None => {}
        ElseBranch::Block(b) => for s in &mut b.stmts { substitute_params_in_stmt(s, subst); }
        ElseBranch::If(inner) => substitute_in_if(inner, subst),
    }
}

fn substitute_in_expr(expr: &mut Expr, subst: &HashMap<String, Expr>) {
    // If this entire expr is an Ident matching a substitution key,
    // replace the whole node with the bound argument.
    if let ExprKind::Ident(name) = &expr.kind {
        if let Some(replacement) = subst.get(name) {
            *expr = replacement.clone();
            return;
        }
    }
    match &mut expr.kind {
        ExprKind::Binary { lhs, rhs, .. } => {
            substitute_in_expr(lhs, subst);
            substitute_in_expr(rhs, subst);
        }
        ExprKind::Unary { operand, .. } => substitute_in_expr(operand, subst),
        ExprKind::Index { target, key } => {
            substitute_in_expr(target, subst);
            substitute_in_expr(key, subst);
        }
        ExprKind::Field { target, .. } => substitute_in_expr(target, subst),
        ExprKind::Array(elems) | ExprKind::SetLit(elems) | ExprKind::TupleLit(elems) => {
            for e in elems { substitute_in_expr(e, subst); }
        }
        ExprKind::ArrayAlloc { len, .. } => substitute_in_expr(len, subst),
        ExprKind::Reserve { count, .. } => substitute_in_expr(count, subst),
        ExprKind::ExplicitLiteral(inner) => substitute_in_expr(inner, subst),
        ExprKind::DictLit(pairs) => {
            for (k, v) in pairs {
                substitute_in_expr(k, subst);
                substitute_in_expr(v, subst);
            }
        }
        ExprKind::Call { args, .. } => for a in args { substitute_in_expr(a, subst); },
        ExprKind::DynCall { args, .. } => for a in args { substitute_in_expr(a, subst); },
        ExprKind::StructLit { fields, .. } => for (_, e) in fields { substitute_in_expr(e, subst); },
        ExprKind::Pipe { head, step } => {
            substitute_in_expr(head, subst);
            substitute_in_expr(step, subst);
        }
        ExprKind::ListComp { mapper, clauses } | ExprKind::SetComp { mapper, clauses } => {
            substitute_in_expr(mapper, subst);
            for c in clauses {
                match c {
                    CompClause::For { iter, .. } => substitute_in_expr(iter, subst),
                    CompClause::If(cond) => substitute_in_expr(cond, subst),
                }
            }
        }
        ExprKind::DictComp { key, value, clauses } => {
            substitute_in_expr(key, subst);
            substitute_in_expr(value, subst);
            for c in clauses {
                match c {
                    CompClause::For { iter, .. } => substitute_in_expr(iter, subst),
                    CompClause::If(cond) => substitute_in_expr(cond, subst),
                }
            }
        }
        ExprKind::TupleIndex { target, .. } => substitute_in_expr(target, subst),
        ExprKind::EnumCtor { args, .. } => for a in args { substitute_in_expr(a, subst); },
        ExprKind::Match { scrut, arms } => {
            substitute_in_expr(scrut, subst);
            for arm in arms { substitute_in_expr(&mut arm.body, subst); }
        }
        ExprKind::Block(block) => {
            for s in &mut block.stmts { substitute_params_in_stmt(s, subst); }
            if let Some(t) = &mut block.tail { substitute_in_expr(t, subst); }
        }
        ExprKind::If { cond, then, else_branch } => {
            substitute_in_expr(cond, subst);
            for s in &mut then.stmts { substitute_params_in_stmt(s, subst); }
            if let Some(t) = &mut then.tail { substitute_in_expr(t, subst); }
            match else_branch {
                ElseBranch::None => {}
                ElseBranch::Block(b) => {
                    for s in &mut b.stmts { substitute_params_in_stmt(s, subst); }
                    if let Some(t) = &mut b.tail { substitute_in_expr(t, subst); }
                }
                ElseBranch::If(inner) => substitute_in_if(inner, subst),
            }
        }
        ExprKind::Int(_) | ExprKind::UInt(_) | ExprKind::Float(_) | ExprKind::JsonNull | ExprKind::I32(_) | ExprKind::U32(_) | ExprKind::U64(_)
        | ExprKind::U128(_) | ExprKind::Bool(_) | ExprKind::Str(_) | ExprKind::Ident(_)
        | ExprKind::Prev => {}
        ExprKind::JsonObject(pairs) => {
            for (_, v) in pairs.iter_mut() { substitute_in_expr(v, subst); }
        }
        ExprKind::JsonArray(items) => {
            for v in items.iter_mut() { substitute_in_expr(v, subst); }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::tokenize;
    use crate::parser::parse;

    fn expand_src(src: &str) -> Result<Module, Error> {
        let mut m = parse(tokenize(src).unwrap()).unwrap();
        expand(&mut m)?;
        Ok(m)
    }

    #[test]
    fn no_modifiers_is_a_no_op() {
        let m = expand_src("fn main() -> i64 { return 1; }").unwrap();
        let f = &m.functions[0];
        assert!(f.modifiers.is_empty());
        assert_eq!(f.body.stmts.len(), 1);
    }

    #[test]
    fn placeholder_outside_modifier_is_rejected() {
        let err = expand_src("
            fn main() -> i64 {
                _;
                return 0;
            }
        ").unwrap_err();
        assert!(err.to_string().contains("modifier"));
    }

    #[test]
    fn single_modifier_wraps_body() {
        let m = expand_src("
            modifier Guard() {
                let _g = 1;
                _;
                let _h = 2;
            }
            fn main() [Guard] -> i64 {
                return 7;
            }
        ").unwrap();
        let f = &m.functions[0];
        assert!(f.modifiers.is_empty());
        // 3 stmts: let _g, return 7, let _h
        assert_eq!(f.body.stmts.len(), 3);
    }

    #[test]
    fn modifier_with_no_placeholder_is_rejected() {
        let err = expand_src("
            modifier Bad() {
                let _x = 1;
            }
            fn main() [Bad] -> i64 { return 0; }
        ").unwrap_err();
        assert!(err.to_string().contains("placeholder"));
    }
}
