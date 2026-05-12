//! Tree-walking interpreter. Slice 1: pure-functional, no side effects, no host imports.

use std::cell::RefCell;
use std::collections::HashMap;

use crate::ast::*;
use crate::error::{Error, ErrorKind};
use crate::host::Host;
use crate::kv::EmptyKv;
use crate::token::Span;
use crate::tx::Tx;
use crate::value::Value;

pub struct Interp<'a> {
    module: &'a Module,
    host: &'a Host,
    tx: RefCell<Tx<'a>>,
    state_types: HashMap<String, Type>,
    state_defaults: HashMap<String, Value>,
    state_roots: HashMap<String, u128>,
    struct_decls: HashMap<String, Vec<String>>,
    pipe_stack: RefCell<Vec<Value>>,
}

#[derive(Default)]
struct Scope {
    vars: HashMap<String, Value>,
}

enum Flow {
    Normal(Value),
    Return(Value),
    Break,
    Continue,
}

impl<'a> Interp<'a> {
    pub fn new(module: &'a Module, host: &'a Host, tx: Tx<'a>) -> Self {
        let mut state_defaults = HashMap::new();
        let mut state_types = HashMap::new();
        let mut state_roots = HashMap::new();
        for s in &module.states {
            // Strip `` `T` `` tags from the storage type — the
            // explicit-literal property is compile-time only;
            // on-disk encoding must match between read and write
            // regardless of any backtick wrapping on the slot's
            // declared type.
            let stripped = strip_explicit_literal_ty(&s.ty);
            state_defaults.insert(s.name.clone(), default_for_type(&stripped));
            state_types.insert(s.name.clone(), stripped);
            state_roots.insert(s.name.clone(), crate::hashing::state_root("main", &s.name));
        }
        let mut struct_decls = HashMap::new();
        for d in &module.structs {
            struct_decls.insert(
                d.name.clone(),
                d.fields.iter().map(|f| f.name.clone()).collect(),
            );
        }
        // Caps share runtime representation with structs — register
        // their field order in the same map so the StructLit eval
        // path handles cap construction without a special case.
        for d in &module.caps {
            struct_decls.insert(
                d.name.clone(),
                d.fields.iter().map(|f| f.name.clone()).collect(),
            );
        }
        Self {
            module,
            host,
            tx: RefCell::new(tx),
            state_types,
            state_defaults,
            state_roots,
            struct_decls,
            pipe_stack: RefCell::new(Vec::new()),
        }
    }

    pub fn into_tx(self) -> Tx<'a> {
        self.tx.into_inner()
    }

    fn read_state(&self, name: &str) -> Value {
        let ty = self.state_types.get(name).cloned().unwrap_or(Type::Int);
        let root = *self.state_roots.get(name).expect("known state");
        self.tx.borrow_mut().read_typed(root, &ty)
    }

    fn write_state(&self, name: &str, v: Value) {
        let root = *self.state_roots.get(name).expect("known state");
        let ty = self.state_types.get(name).cloned().unwrap_or(Type::Int);
        self.tx.borrow_mut().write_typed(root, &ty, v);
    }

    /// If `expr` is a chain of `Field` ending at an unshadowed state Ident,
    /// returns the state name and the ordered field path. Used to short-cut
    /// reads/writes of a known sub-tree directly to its leaf cell, instead
    /// of materializing the parent struct.
    fn try_state_field_path(&self, expr: &Expr, scopes: &[Scope]) -> Option<(String, Vec<String>)> {
        let mut names = Vec::new();
        let mut cur = expr;
        loop {
            match &cur.kind {
                ExprKind::Field { target, name } => {
                    names.push(name.clone());
                    cur = target;
                }
                ExprKind::Ident(name) => {
                    for scope in scopes.iter().rev() {
                        if scope.vars.contains_key(name) { return None; }
                    }
                    if !self.is_state(name) { return None; }
                    names.reverse();
                    return Some((name.clone(), names));
                }
                _ => return None,
            }
        }
    }

    /// Walk a state field path to the leaf KV key and type, then call f.
    fn with_state_path<R>(&self, state_name: &str, path: &[String], f: impl FnOnce(u128, &Type) -> R) -> R {
        let mut key = *self.state_roots.get(state_name).expect("known state");
        let mut ty = self.state_types.get(state_name).cloned().expect("known state");
        for name in path {
            key = crate::hashing::child(key, name.as_bytes());
            let next_ty = match ty {
                Type::Struct { fields, .. } => fields
                    .into_iter()
                    .find(|(n, _)| n == name)
                    .map(|(_, t)| t)
                    .expect("path field exists by typeck"),
                _ => panic!("state field path on non-struct type"),
            };
            ty = next_ty;
        }
        f(key, &ty)
    }

    fn is_state(&self, name: &str) -> bool {
        self.state_defaults.contains_key(name)
    }

    /// Drain the tx's pending-emit queue, dispatching every matching
    /// handler in stable declaration order. Each handler runs
    /// serially against the live tx — the interp's reference path
    /// doesn't try to parallelize, but the observable behavior
    /// matches the bytecode VM's parallel scheduler.
    pub fn drain_pending_emits(&self) -> Result<(), Error> {
        loop {
            let batch = self.tx.borrow_mut().take_pending_emits();
            if batch.is_empty() {
                return Ok(());
            }
            let emit_module = self
                .module
                .name
                .clone()
                .unwrap_or_else(|| "main".to_string());
            for emit in batch {
                let handler_names: Vec<String> = self
                    .module
                    .handlers
                    .iter()
                    .filter(|h| {
                        h.event_type == emit.struct_name
                            && h.event_module
                                .as_deref()
                                .map(|m| m == emit.module)
                                .unwrap_or(emit.module == emit_module)
                    })
                    .map(|h| h.fn_def.name.clone())
                    .collect();
                for hname in handler_names {
                    let _ = self.call(&hname, vec![emit.value.clone()])?;
                }
            }
        }
    }

    pub fn call(&self, name: &str, args: Vec<Value>) -> Result<Value, Error> {
        // User-defined sum/max/min shadow the builtin (matches typeck +
        // compile resolution). Other builtins still win unconditionally.
        let user_overrides_builtin =
            matches!(name, "sum" | "max" | "min")
            && self.module.functions.iter().any(|f| f.name == name);
        if !user_overrides_builtin {
            if let Some(v) = self.try_builtin(name, &args)? {
                return Ok(v);
            }
        }
        if self.module.imports.iter().any(|i| i.name == name) {
            return self.host.call(name, &args).map_err(|msg| {
                Error::new(
                    ErrorKind::Runtime,
                    format!("host '{name}': {msg}"),
                    Span::default(),
                )
            });
        }
        let (fn_idx, f) = self
            .module
            .functions
            .iter()
            .enumerate()
            .find(|(_, f)| f.name == name)
            .ok_or_else(|| {
                Error::new(
                    ErrorKind::Runtime,
                    format!("unknown function '{name}'"),
                    Span::default(),
                )
            })?;
        if f.params.len() != args.len() {
            return Err(Error::new(
                ErrorKind::Runtime,
                format!(
                    "function '{name}' expects {} arg(s), got {}",
                    f.params.len(),
                    args.len()
                ),
                f.span,
            ));
        }
        // Non-reentrant guard. Module idx is hard-coded 0 in the
        // tree-walk interp; that's fine since interp doesn't support
        // cross-module dispatch.
        let nore_key = (0u32, fn_idx as u32);
        if f.is_nore && !self.tx.borrow_mut().nore_enter(nore_key.0, nore_key.1) {
            return Err(Error::new(
                ErrorKind::Runtime,
                format!("re-entry into non-reentrant function '{name}'"),
                f.span,
            ));
        }
        let mut frame = Scope::default();
        for (p, a) in f.params.iter().zip(args.into_iter()) {
            frame.vars.insert(p.name.clone(), a);
        }
        let mut scopes = vec![frame];
        let result = self.exec_block_with_tail(&f.body, &mut scopes, true);
        if f.is_nore { self.tx.borrow_mut().nore_exit(nore_key.0, nore_key.1); }
        match result? {
            Flow::Normal(v) | Flow::Return(v) => Ok(v),
            Flow::Break | Flow::Continue => Err(Error::new(
                ErrorKind::Runtime,
                "break/continue outside of a loop",
                f.span,
            )),
        }
    }

    fn try_builtin(&self, name: &str, args: &[Value]) -> Result<Option<Value>, Error> {
        match name {
            "resource" => match args {
                [Value::Int(n)] => match num_traits::ToPrimitive::to_i64(n) {
                    Some(v) => Ok(Some(Value::Resource(v))),
                    None => Err(Error::new(
                        ErrorKind::Runtime,
                        "resource(): int out of i64 range",
                        Span::default(),
                    )),
                },
                _ => Err(Error::new(
                    ErrorKind::Runtime,
                    "resource() expects a single int",
                    Span::default(),
                )),
            },
            "unwrap" => match args {
                [Value::Resource(n)] => Ok(Some(Value::int(n.clone()))),
                _ => Err(Error::new(
                    ErrorKind::Runtime,
                    "unwrap() expects a single Resource",
                    Span::default(),
                )),
            },
            "address" => match args {
                [Value::Str(s)] => Ok(Some(Value::Address(s.clone()))),
                _ => Err(Error::new(
                    ErrorKind::Runtime,
                    "address() expects a single string",
                    Span::default(),
                )),
            },
            "len" => match args {
                [Value::Array(elems)] => Ok(Some(Value::int(elems.len()))),
                [Value::Str(s)] => Ok(Some(Value::int(s.as_bytes().len()))),
                [Value::Set(elems)] => Ok(Some(Value::int(elems.len()))),
                [Value::Dict(pairs)] => Ok(Some(Value::int(pairs.len()))),
                _ => Err(Error::new(
                    ErrorKind::Runtime,
                    "len() expects array/string/set/dict",
                    Span::default(),
                )),
            },
            "msg_sender" => {
                if !args.is_empty() {
                    return Err(Error::new(
                        ErrorKind::Runtime,
                        "msg_sender() takes no arguments",
                        Span::default(),
                    ));
                }
                Ok(Some(self.tx.borrow().context().sender.clone()))
            }
            "block_timestamp" => {
                if !args.is_empty() {
                    return Err(Error::new(
                        ErrorKind::Runtime,
                        "block_timestamp() takes no arguments",
                        Span::default(),
                    ));
                }
                Ok(Some(Value::U64(self.tx.borrow().context().block_timestamp)))
            }
            "block_number" => {
                if !args.is_empty() {
                    return Err(Error::new(
                        ErrorKind::Runtime,
                        "block_number() takes no arguments",
                        Span::default(),
                    ));
                }
                Ok(Some(Value::U64(self.tx.borrow().context().block_number)))
            }
            "to_bytes" | "bytes_len" | "bytes_concat" | "bytes_eq" | "bytes_slice"
            | "to_be_bytes" | "bit_not_bytes"
            | "string_concat" | "string_slice" | "string_contains" => {
                Ok(Some(crate::ops::call_builtin(name, args)?))
            }
            "assert" => match args {
                [Value::Bool(true)] | [Value::Bool(true), Value::Str(_)] => {
                    Ok(Some(Value::Unit))
                }
                [Value::Bool(false)] => Err(Error::new(
                    ErrorKind::Runtime,
                    "assertion failed",
                    Span::default(),
                )),
                [Value::Bool(false), Value::Str(msg)] => Err(Error::new(
                    ErrorKind::Runtime,
                    format!("assertion failed: {msg}"),
                    Span::default(),
                )),
                _ => Err(Error::new(
                    ErrorKind::Runtime,
                    "assert(cond: bool [, msg: string])",
                    Span::default(),
                )),
            },
            "set_insert" => match args {
                [Value::Set(s), v] => {
                    let mut new_s = s.clone();
                    if !new_s.iter().any(|e| e == v) {
                        new_s.push(v.clone());
                    }
                    Ok(Some(Value::Set(new_s)))
                }
                _ => Err(Error::new(ErrorKind::Runtime, "set_insert() expects (set, elem)", Span::default())),
            },
            "set_remove" => match args {
                [Value::Set(s), v] => {
                    let new_s: Vec<Value> = s.iter().filter(|e| *e != v).cloned().collect();
                    Ok(Some(Value::Set(new_s)))
                }
                _ => Err(Error::new(ErrorKind::Runtime, "set_remove() expects (set, elem)", Span::default())),
            },
            "set_contains" => match args {
                [Value::Set(s), v] => Ok(Some(Value::Bool(s.iter().any(|e| e == v)))),
                _ => Err(Error::new(ErrorKind::Runtime, "set_contains() expects (set, elem)", Span::default())),
            },
            "set_len" => match args {
                [Value::Set(s)] => Ok(Some(Value::int(s.len()))),
                _ => Err(Error::new(ErrorKind::Runtime, "set_len() expects (set)", Span::default())),
            },
            "dict_set" => match args {
                [Value::Dict(d), k, v] => {
                    let mut new_d = d.clone();
                    if let Some(pair) = new_d.iter_mut().find(|(kk, _)| kk == k) {
                        pair.1 = v.clone();
                    } else {
                        new_d.push((k.clone(), v.clone()));
                    }
                    Ok(Some(Value::Dict(new_d)))
                }
                _ => Err(Error::new(ErrorKind::Runtime, "dict_set() expects (dict, key, value)", Span::default())),
            },
            "dict_remove" => match args {
                [Value::Dict(d), k] => {
                    let new_d: Vec<(Value, Value)> = d.iter().filter(|(kk, _)| kk != k).cloned().collect();
                    Ok(Some(Value::Dict(new_d)))
                }
                _ => Err(Error::new(ErrorKind::Runtime, "dict_remove() expects (dict, key)", Span::default())),
            },
            "dict_get" => match args {
                [Value::Dict(d), k, default] => Ok(Some(
                    d.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone()).unwrap_or_else(|| default.clone())
                )),
                _ => Err(Error::new(ErrorKind::Runtime, "dict_get() expects (dict, key, default)", Span::default())),
            },
            "dict_has" => match args {
                [Value::Dict(d), k] => Ok(Some(Value::Bool(d.iter().any(|(kk, _)| kk == k)))),
                _ => Err(Error::new(ErrorKind::Runtime, "dict_has() expects (dict, key)", Span::default())),
            },
            "dict_len" => match args {
                [Value::Dict(d)] => Ok(Some(Value::int(d.len()))),
                _ => Err(Error::new(ErrorKind::Runtime, "dict_len() expects (dict)", Span::default())),
            },
            "i64" | "i32" | "u32" | "u64" | "u128" => {
                if args.len() != 1 {
                    return Err(Error::new(
                        ErrorKind::Runtime,
                        format!("{name}() takes one argument"),
                        Span::default(),
                    ));
                }
                let target: u8 = match name {
                    "i64" => 0, "i32" => 1, "u32" => 2, "u64" => 3, "u128" => 4,
                    _ => unreachable!(),
                };
                Ok(Some(crate::ops::convert(&args[0], target, Span::default())?))
            }
            // Aggregations — delegate to the shared ops::call_builtin
            // so we don't duplicate the fold logic across paths.
            "sum" | "max" | "min" => Ok(Some(crate::ops::call_builtin(name, args)?)),
            // JSON builtins. Same delegation — interp and bytecode VM
            // share one source of truth for parse / navigate / convert.
            "parse_json" | "json_stringify"
            | "json_get_field" | "json_get_index"
            | "json_to_string" | "json_to_i64" | "json_to_u64"
            | "json_to_bool" | "json_is_null" => {
                Ok(Some(crate::ops::call_builtin(name, args)?))
            }
            _ => Ok(None),
        }
    }

    fn exec_block(&self, block: &Block, scopes: &mut Vec<Scope>) -> Result<Flow, Error> {
        self.exec_block_with_tail(block, scopes, false)
    }

    /// Run a block. When `tail_is_implicit_return` is true (fn-body
    /// path), the trailing tail expression turns into a Return flow;
    /// otherwise it's evaluated for side-effects and discarded.
    fn exec_block_with_tail(
        &self,
        block: &Block,
        scopes: &mut Vec<Scope>,
        tail_is_implicit_return: bool,
    ) -> Result<Flow, Error> {
        scopes.push(Scope::default());
        for stmt in &block.stmts {
            match self.exec_stmt(stmt, scopes)? {
                Flow::Normal(_) => {}
                Flow::Return(v) => {
                    scopes.pop();
                    return Ok(Flow::Return(v));
                }
                Flow::Break => {
                    scopes.pop();
                    return Ok(Flow::Break);
                }
                Flow::Continue => {
                    scopes.pop();
                    return Ok(Flow::Continue);
                }
            }
        }
        if let Some(t) = &block.tail {
            let v = self.eval(t, scopes)?;
            scopes.pop();
            if tail_is_implicit_return {
                return Ok(Flow::Return(v));
            }
            return Ok(Flow::Normal(v));
        }
        scopes.pop();
        Ok(Flow::Normal(Value::Unit))
    }

    fn exec_stmt(&self, stmt: &Stmt, scopes: &mut Vec<Scope>) -> Result<Flow, Error> {
        match stmt {
            Stmt::Let { name, ty: _, value, .. } => {
                let v = self.eval(value, scopes)?;
                scopes.last_mut().expect("scope stack").vars.insert(name.clone(), v);
                Ok(Flow::Normal(Value::Unit))
            }
            Stmt::Assign { target, value, .. } => {
                let v = self.eval(value, scopes)?;
                self.assign_path(target, v, scopes)?;
                Ok(Flow::Normal(Value::Unit))
            }
            Stmt::Return { value, .. } => {
                let v = match value {
                    Some(e) => self.eval(e, scopes)?,
                    None => Value::Unit,
                };
                Ok(Flow::Return(v))
            }
            Stmt::If(if_stmt) => self.exec_if(if_stmt, scopes),
            Stmt::While { cond, body, span } => {
                loop {
                    let c = self.eval(cond, scopes)?;
                    let Value::Bool(true) = c else {
                        if let Value::Bool(false) = c { break; }
                        return Err(Error::new(
                            ErrorKind::Runtime,
                            format!("while condition must be bool, got {c}"),
                            *span,
                        ));
                    };
                    match self.exec_block(body, scopes)? {
                        Flow::Normal(_) | Flow::Continue => continue,
                        Flow::Break => break,
                        Flow::Return(v) => return Ok(Flow::Return(v)),
                    }
                }
                Ok(Flow::Normal(Value::Unit))
            }
            Stmt::For { var, iter, body, span } => {
                let arr_v = self.eval_iter_to_array(iter, scopes, *span)?;
                let Value::Array(elems) = arr_v else {
                    return Err(Error::new(
                        ErrorKind::Runtime,
                        "for-in iterator must be an array",
                        *span,
                    ));
                };
                for elem in elems {
                    scopes.push(Scope::default());
                    scopes.last_mut().unwrap().vars.insert(var.clone(), elem);
                    let flow = self.exec_block(body, scopes)?;
                    scopes.pop();
                    match flow {
                        Flow::Normal(_) | Flow::Continue => {}
                        Flow::Break => break,
                        Flow::Return(v) => return Ok(Flow::Return(v)),
                    }
                }
                Ok(Flow::Normal(Value::Unit))
            }
            Stmt::ForRange { var, start, end, inclusive, body, span } => {
                let s = self.eval(start, scopes)?;
                let e = self.eval(end, scopes)?;
                // Two iterator shapes:
                //   * BigInt loop for `int` (unbounded counter, must
                //     not be cast to a fixed-width type).
                //   * i128 loop for the sized integer types — wide
                //     enough to hold any of i32/u32/u64/u128's range,
                //     with a per-variant constructor to re-wrap.
                if let (Value::Int(a), Value::Int(b)) = (&s, &e) {
                    let mut i = a.clone();
                    let lim = b.clone();
                    while if *inclusive { i <= lim } else { i < lim } {
                        scopes.push(Scope::default());
                        scopes.last_mut().unwrap().vars
                            .insert(var.clone(), Value::Int(i.clone()));
                        let flow = self.exec_block(body, scopes)?;
                        scopes.pop();
                        match flow {
                            Flow::Normal(_) | Flow::Continue => {}
                            Flow::Break => break,
                            Flow::Return(v) => return Ok(Flow::Return(v)),
                        }
                        i += 1;
                    }
                    return Ok(Flow::Normal(Value::Unit));
                }
                let (mut i, lim, ctor): (i128, i128, fn(i128) -> Value) = match (&s, &e) {
                    (Value::U64(a), Value::U64(b))   => (*a as i128, *b as i128, |n| Value::U64(n as u64)),
                    (Value::I32(a), Value::I32(b))   => (*a as i128, *b as i128, |n| Value::I32(n as i32)),
                    (Value::U32(a), Value::U32(b))   => (*a as i128, *b as i128, |n| Value::U32(n as u32)),
                    (Value::U128(a), Value::U128(b)) => (*a as i128, *b as i128, |n| Value::U128(n as u128)),
                    _ => return Err(Error::new(
                        ErrorKind::Runtime,
                        "range bounds must be the same integer type",
                        *span,
                    )),
                };
                while if *inclusive { i <= lim } else { i < lim } {
                    scopes.push(Scope::default());
                    scopes.last_mut().unwrap().vars.insert(var.clone(), ctor(i));
                    let flow = self.exec_block(body, scopes)?;
                    scopes.pop();
                    match flow {
                        Flow::Normal(_) | Flow::Continue => {}
                        Flow::Break => break,
                        Flow::Return(v) => return Ok(Flow::Return(v)),
                    }
                    i += 1;
                }
                Ok(Flow::Normal(Value::Unit))
            }
            Stmt::Break(_) => Ok(Flow::Break),
            Stmt::Continue(_) => Ok(Flow::Continue),
            Stmt::Placeholder(_) => unreachable!(
                "modifier expansion should have removed all `_;`",
            ),
            Stmt::Expr(expr) => {
                let _ = self.eval(expr, scopes)?;
                Ok(Flow::Normal(Value::Unit))
            }
            Stmt::Delete { target, span } => {
                self.exec_delete(target, *span, scopes)?;
                Ok(Flow::Normal(Value::Unit))
            }
            Stmt::Emit { value, span } => {
                // Interp is serial reference. Log the event then
                // dispatch handlers inline in declaration order —
                // observably identical to the bytecode VM's
                // parallel scheduler when handlers don't conflict.
                let v = self.eval(value, scopes)?;
                let struct_val = v.clone();
                let struct_name = match &v {
                    Value::Struct { name, .. } => name.clone(),
                    other => return Err(Error::new(
                        ErrorKind::Runtime,
                        format!("emit expected a struct value, got {other}"),
                        *span,
                    )),
                };
                let emit_module = self
                    .module
                    .name
                    .clone()
                    .unwrap_or_else(|| "main".to_string());
                if let Value::Struct { fields, .. } = v {
                    let args: Vec<Value> = fields.into_iter().map(|(_, v)| v).collect();
                    self.tx.borrow_mut().emit(crate::tx::EmittedEvent {
                        module: emit_module.clone(),
                        name: struct_name.clone(),
                        args,
                    });
                }
                let handler_names: Vec<String> = self
                    .module
                    .handlers
                    .iter()
                    .filter(|h| {
                        h.event_type == struct_name
                            && h.event_module
                                .as_deref()
                                .map(|m| m == emit_module)
                                .unwrap_or(true)
                    })
                    .map(|h| h.fn_def.name.clone())
                    .collect();
                for hname in handler_names {
                    let _ = self.call(&hname, vec![struct_val.clone()])?;
                }
                Ok(Flow::Normal(Value::Unit))
            }
            Stmt::LetTuple { names, value, span } => {
                let v = self.eval(value, scopes)?;
                let elems = match v {
                    Value::Tuple(elems) => elems,
                    other => return Err(Error::new(
                        ErrorKind::Runtime,
                        format!("destructure: expected tuple, got {other}"),
                        *span,
                    )),
                };
                if elems.len() != names.len() {
                    return Err(Error::new(
                        ErrorKind::Runtime,
                        "tuple destructure arity mismatch",
                        *span,
                    ));
                }
                let scope = scopes.last_mut().expect("scope stack");
                for (n, v) in names.iter().zip(elems.into_iter()) {
                    scope.vars.insert(n.clone(), v);
                }
                Ok(Flow::Normal(Value::Unit))
            }
            Stmt::Parallel { stmts, .. } => {
                // Tree-walk reference interp runs the stmts serially.
                // Bindings go into the current scope so they outlive
                // the block (the bytecode VM is what parallelizes;
                // the interp is just the semantic baseline).
                for s in stmts {
                    let flow = self.exec_stmt(s, scopes)?;
                    if !matches!(flow, Flow::Normal(_)) {
                        return Ok(flow);
                    }
                }
                Ok(Flow::Normal(Value::Unit))
            }
            Stmt::ParallelForTo { id_var, idx_var, source, output, body, span } => {
                // Reference semantics — serial loop. Bytecode VM will
                // dispatch in parallel; observable result is the same
                // because slot writes are disjoint by construction.
                let src_v = self.eval(source, scopes)?;
                let Value::Array(src_elems) = src_v else {
                    return Err(Error::new(
                        ErrorKind::Runtime,
                        "parallel-for source must evaluate to an array",
                        source.span,
                    ));
                };
                // Output must be a local Ident — slice 1 doesn't
                // support arbitrary lvalues here (state cells, nested
                // indexing). The bound value's current array contents
                // are used as the default for slots not overwritten
                // by the loop body (continue / break skip a slot).
                let out_name = match &output.kind {
                    ExprKind::Ident(s) => s.clone(),
                    _ => return Err(Error::new(
                        ErrorKind::Runtime,
                        "parallel-for output must be a local array binding",
                        output.span,
                    )),
                };
                let mut out_arr: Vec<Value> = {
                    let mut found: Option<Vec<Value>> = None;
                    for s in scopes.iter() {
                        if let Some(v) = s.vars.get(&out_name) {
                            match v {
                                Value::Array(a) => { found = Some(a.clone()); }
                                _ => return Err(Error::new(
                                    ErrorKind::Runtime,
                                    format!("parallel-for output `{out_name}` must be an array"),
                                    output.span,
                                )),
                            }
                            break;
                        }
                    }
                    match found {
                        Some(v) => v,
                        None => return Err(Error::new(
                            ErrorKind::Runtime,
                            format!("parallel-for output `{out_name}` is not bound"),
                            output.span,
                        )),
                    }
                };
                if out_arr.len() != src_elems.len() {
                    return Err(Error::new(
                        ErrorKind::Runtime,
                        format!(
                            "parallel-for source length {} != output length {}",
                            src_elems.len(), out_arr.len(),
                        ),
                        *span,
                    ));
                }
                for (i, id_val) in src_elems.into_iter().enumerate() {
                    scopes.push(Scope::default());
                    scopes.last_mut().unwrap().vars.insert(id_var.clone(), id_val);
                    if let Some(idx) = idx_var {
                        scopes.last_mut().unwrap().vars.insert(
                            idx.clone(), Value::int(i as i64),
                        );
                    }
                    let flow = self.exec_block(body, scopes)?;
                    scopes.pop();
                    match flow {
                        Flow::Normal(v) => { out_arr[i] = v; }
                        Flow::Continue => {} // slot keeps existing value
                        Flow::Break => break,
                        Flow::Return(v) => return Ok(Flow::Return(v)),
                    }
                }
                // Write the mutated array back to its binding.
                for s in scopes.iter_mut() {
                    if s.vars.contains_key(&out_name) {
                        s.vars.insert(out_name.clone(), Value::Array(out_arr));
                        break;
                    }
                }
                Ok(Flow::Normal(Value::Unit))
            }
        }
    }

    fn exec_if(&self, ifs: &IfStmt, scopes: &mut Vec<Scope>) -> Result<Flow, Error> {
        let cond = self.eval(&ifs.cond, scopes)?;
        match cond {
            Value::Bool(true) => self.exec_block(&ifs.then, scopes),
            Value::Bool(false) => match &ifs.else_branch {
                ElseBranch::None => Ok(Flow::Normal(Value::Unit)),
                ElseBranch::Block(b) => self.exec_block(b, scopes),
                ElseBranch::If(inner) => self.exec_if(inner, scopes),
            },
            other => Err(Error::new(
                ErrorKind::Runtime,
                format!("if condition must be bool, got {other}"),
                ifs.span,
            )),
        }
    }

    /// For every index covering `primary_state`, project the indexed
    /// field out of the just-written value and write
    /// `index[projected] = key`. Mirrors the auto-maintenance the
    /// bytecode compile pass emits at every primary `pmap[k] = v`
    /// write site.
    /// Execute `delete state[k]` in the interp. Reads the current
    /// value (so index back-links can be cleaned), removes it from
    /// the primary, then walks declared indexes and removes the
    /// matching back-links.
    fn exec_delete(
        &self,
        target: &Expr,
        span: Span,
        scopes: &mut Vec<Scope>,
    ) -> Result<(), Error> {
        let ExprKind::Index { target: t, key } = &target.kind else {
            return Err(Error::new(
                ErrorKind::Runtime,
                "delete target must be `state[key]`".to_string(),
                span,
            ));
        };
        let ExprKind::Ident(state_name) = &t.kind else {
            return Err(Error::new(
                ErrorKind::Runtime,
                "delete target must be a state slot".to_string(),
                span,
            ));
        };
        let key_v = self.eval(key, scopes)?;
        let state_ty = self.state_types.get(state_name).cloned().ok_or_else(|| {
            Error::new(ErrorKind::Runtime, format!("'{state_name}' is not a state"), span)
        })?;
        let root = *self.state_roots.get(state_name).expect("known state");

        match state_ty {
            Type::PMap { key: kt, value: vt } => {
                let pmap_ty = Type::PMap { key: kt.clone(), value: vt.clone() };
                let root_hash = match self.tx.borrow_mut().read_cell(root, &pmap_ty) {
                    Value::PMap(h) => h,
                    _ => crate::pmap::EMPTY,
                };
                // Read prior value first so index cleanup can project
                // the indexed fields. None → nothing to clean.
                let prior = {
                    let mut tx = self.tx.borrow_mut();
                    crate::pmap::get(root_hash, &key_v, &mut tx, &kt, &vt)
                };
                // Remove from primary.
                let new_root = {
                    let mut tx = self.tx.borrow_mut();
                    let (h, _) = crate::pmap::remove(root_hash, &key_v, &mut tx, &kt, &vt)?;
                    h
                };
                self.tx.borrow_mut().write(root, Value::PMap(new_root));
                if let Some(old_value) = prior {
                    self.unmaintain_indexes(state_name, &key_v, &old_value)?;
                }
            }
            Type::PBTree { key: kt, value: vt } => {
                let pbtree_ty = Type::PBTree { key: kt.clone(), value: vt.clone() };
                let root_hash = match self.tx.borrow_mut().read_cell(root, &pbtree_ty) {
                    Value::PBTree(h) => h,
                    _ => crate::pbtree::EMPTY,
                };
                let prior = {
                    let mut tx = self.tx.borrow_mut();
                    crate::pbtree::get(root_hash, &key_v, &mut tx, &kt, &vt)
                };
                let new_root = {
                    let mut tx = self.tx.borrow_mut();
                    crate::pbtree::remove(root_hash, &key_v, &mut tx, &kt, &vt)?
                };
                self.tx.borrow_mut().write(root, Value::PBTree(new_root));
                if let Some(old_value) = prior {
                    self.unmaintain_indexes(state_name, &key_v, &old_value)?;
                }
            }
            _ => return Err(Error::new(
                ErrorKind::Runtime,
                format!("delete: '{state_name}' is not a pmap or pbtree state"),
                span,
            )),
        }
        Ok(())
    }

    /// Mirror of `maintain_indexes` but for delete: project the
    /// **prior** value, find the index entries pointing at the
    /// just-deleted primary key, and remove them. For unique
    /// indexes only remove the entry if it still maps to `key`
    /// (an earlier field-changing update may have left a stale
    /// pointer at someone else). For multi indexes filter the
    /// list.
    fn unmaintain_indexes(
        &self,
        primary_state: &str,
        key: &Value,
        prior_value: &Value,
    ) -> Result<(), Error> {
        for idx in &self.module.indexes {
            if idx.on_state != primary_state { continue; }
            let cur = project_index_key(idx, prior_value)?;
            let (idx_kt, idx_vt, is_pbtree) = match self.state_types.get(&idx.name) {
                Some(Type::PMap { key, value }) => ((**key).clone(), (**value).clone(), false),
                Some(Type::PBTree { key, value }) => ((**key).clone(), (**value).clone(), true),
                _ => return Err(Error::new(
                    ErrorKind::Runtime,
                    format!("index '{}' is not a pmap/pbtree state at runtime", idx.name),
                    idx.span,
                )),
            };
            let idx_root_cell = *self.state_roots.get(&idx.name).expect("known state");
            // Read root from the right backend, then dispatch by
            // (kind, backend).
            if is_pbtree {
                let idx_pbtree_ty = Type::PBTree { key: Box::new(idx_kt.clone()), value: Box::new(idx_vt.clone()) };
                let idx_root = match self.tx.borrow_mut().read_cell(idx_root_cell, &idx_pbtree_ty) {
                    Value::PBTree(h) => h,
                    _ => crate::pbtree::EMPTY,
                };
                match idx.kind {
                    crate::ast::IndexKind::Unique => {
                        let mut tx = self.tx.borrow_mut();
                        let existing = crate::pbtree::get(idx_root, &cur, &mut tx, &idx_kt, &idx_vt);
                        if matches!(existing, Some(ref v) if v == key) {
                            let new_root = crate::pbtree::remove(idx_root, &cur, &mut tx, &idx_kt, &idx_vt)?;
                            drop(tx);
                            self.tx.borrow_mut().write(idx_root_cell, Value::PBTree(new_root));
                        }
                    }
                    crate::ast::IndexKind::Multi => {
                        let mut tx = self.tx.borrow_mut();
                        let existing = crate::pbtree::get(idx_root, &cur, &mut tx, &idx_kt, &idx_vt);
                        if let Some(Value::Array(mut arr)) = existing {
                            arr.retain(|e| e != key);
                            let new_root = if arr.is_empty() {
                                crate::pbtree::remove(idx_root, &cur, &mut tx, &idx_kt, &idx_vt)?
                            } else {
                                crate::pbtree::set(idx_root, cur, Value::Array(arr), &mut tx, &idx_kt, &idx_vt)?
                            };
                            drop(tx);
                            self.tx.borrow_mut().write(idx_root_cell, Value::PBTree(new_root));
                        }
                    }
                }
            } else {
                let idx_pmap_ty = Type::PMap { key: Box::new(idx_kt.clone()), value: Box::new(idx_vt.clone()) };
                let idx_root = match self.tx.borrow_mut().read_cell(idx_root_cell, &idx_pmap_ty) {
                    Value::PMap(h) => h,
                    _ => crate::pmap::EMPTY,
                };
                match idx.kind {
                    crate::ast::IndexKind::Unique => {
                        let mut tx = self.tx.borrow_mut();
                        let existing = crate::pmap::get(idx_root, &cur, &mut tx, &idx_kt, &idx_vt);
                        if matches!(existing, Some(ref v) if v == key) {
                            let (new_root, _) = crate::pmap::remove(idx_root, &cur, &mut tx, &idx_kt, &idx_vt)?;
                            drop(tx);
                            self.tx.borrow_mut().write(idx_root_cell, Value::PMap(new_root));
                        }
                    }
                    crate::ast::IndexKind::Multi => {
                        let mut tx = self.tx.borrow_mut();
                        let existing = crate::pmap::get(idx_root, &cur, &mut tx, &idx_kt, &idx_vt);
                        if let Some(Value::Array(mut arr)) = existing {
                            arr.retain(|e| e != key);
                            let new_root = if arr.is_empty() {
                                crate::pmap::remove(idx_root, &cur, &mut tx, &idx_kt, &idx_vt)?.0
                            } else {
                                crate::pmap::set(idx_root, cur, Value::Array(arr), &mut tx, &idx_kt, &idx_vt)?
                            };
                            drop(tx);
                            self.tx.borrow_mut().write(idx_root_cell, Value::PMap(new_root));
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn maintain_indexes(
        &self,
        primary_state: &str,
        key: &Value,
        value: &Value,
    ) -> Result<(), Error> {
        for idx in &self.module.indexes {
            if idx.on_state != primary_state { continue; }
            let cur = project_index_key(idx, value)?;
            // Determine the index slot's backend (pmap or pbtree)
            // and dispatch to the right module. K/V types are the
            // same shape; only the trie module changes.
            let (idx_kt, idx_vt, is_pbtree) = match self.state_types.get(&idx.name) {
                Some(Type::PMap { key, value }) => ((**key).clone(), (**value).clone(), false),
                Some(Type::PBTree { key, value }) => ((**key).clone(), (**value).clone(), true),
                _ => return Err(Error::new(
                    ErrorKind::Runtime,
                    format!("index '{}' is not a pmap/pbtree state at runtime", idx.name),
                    idx.span,
                )),
            };
            let idx_root_cell = *self.state_roots.get(&idx.name).expect(
                "index slot validated by typeck",
            );
            // Read root + dispatch. The two backends keep parallel
            // value-shapes (PMap(h) vs PBTree(h)), so the rest of
            // the maintenance code differs only in which module
            // does the trie ops.
            if is_pbtree {
                let idx_pbtree_ty = Type::PBTree { key: Box::new(idx_kt.clone()), value: Box::new(idx_vt.clone()) };
                let idx_root_hash = match self.tx.borrow_mut().read_cell(idx_root_cell, &idx_pbtree_ty) {
                    Value::PBTree(h) => h,
                    _ => crate::pbtree::EMPTY,
                };
                match idx.kind {
                    crate::ast::IndexKind::Unique => {
                        let mut tx = self.tx.borrow_mut();
                        let existing = crate::pbtree::get(
                            idx_root_hash, &cur, &mut tx, &idx_kt, &idx_vt,
                        );
                        if let Some(prior) = existing {
                            if &prior != key {
                                return Err(Error::new(
                                    ErrorKind::Runtime,
                                    format!(
                                        "unique constraint violation on '{}': key {cur} already maps to {prior}, refused to remap to {key}",
                                        idx.name,
                                    ),
                                    idx.span,
                                ));
                            }
                        } else {
                            let new_root = crate::pbtree::set(idx_root_hash, cur, key.clone(), &mut tx, &idx_kt, &idx_vt)?;
                            drop(tx);
                            self.tx.borrow_mut().write(idx_root_cell, Value::PBTree(new_root));
                        }
                    }
                    crate::ast::IndexKind::Multi => {
                        let mut tx = self.tx.borrow_mut();
                        let existing = crate::pbtree::get(
                            idx_root_hash, &cur, &mut tx, &idx_kt, &idx_vt,
                        );
                        let mut arr = match existing {
                            Some(Value::Array(a)) => a,
                            _ => Vec::new(),
                        };
                        if !arr.iter().any(|e| e == key) {
                            arr.push(key.clone());
                            let new_root = crate::pbtree::set(
                                idx_root_hash, cur, Value::Array(arr),
                                &mut tx, &idx_kt, &idx_vt,
                            )?;
                            drop(tx);
                            self.tx.borrow_mut().write(idx_root_cell, Value::PBTree(new_root));
                        }
                    }
                }
                continue;
            }
            let idx_pmap_ty = Type::PMap { key: Box::new(idx_kt.clone()), value: Box::new(idx_vt.clone()) };
            let idx_root_hash = match self.tx.borrow_mut().read_cell(idx_root_cell, &idx_pmap_ty) {
                Value::PMap(h) => h,
                _ => crate::pmap::EMPTY,
            };
            match idx.kind {
                crate::ast::IndexKind::Unique => {
                    // Read first; reject if a different primary
                    // already claims this key. Idempotent re-writes
                    // of the same (key, value) pair pass.
                    let mut tx = self.tx.borrow_mut();
                    let existing = crate::pmap::get(
                        idx_root_hash, &cur, &mut tx, &idx_kt, &idx_vt,
                    );
                    if let Some(prior) = existing {
                        if &prior != key {
                            return Err(Error::new(
                                ErrorKind::Runtime,
                                format!(
                                    "unique constraint violation on '{}': key {cur} already maps to {prior}, refused to remap to {key}",
                                    idx.name,
                                ),
                                idx.span,
                            ));
                        }
                    } else {
                        let new_root = crate::pmap::set(idx_root_hash, cur, key.clone(), &mut tx, &idx_kt, &idx_vt)?;
                        drop(tx);
                        self.tx.borrow_mut().write(idx_root_cell, Value::PMap(new_root));
                    }
                }
                crate::ast::IndexKind::Multi => {
                    // Read existing list, dedup-append, write back.
                    let mut tx = self.tx.borrow_mut();
                    let existing = crate::pmap::get(
                        idx_root_hash, &cur, &mut tx, &idx_kt, &idx_vt,
                    );
                    let mut arr = match existing {
                        Some(Value::Array(a)) => a,
                        _ => Vec::new(),
                    };
                    if !arr.iter().any(|e| e == key) {
                        arr.push(key.clone());
                        let new_root = crate::pmap::set(
                            idx_root_hash, cur, Value::Array(arr),
                            &mut tx, &idx_kt, &idx_vt,
                        )?;
                        drop(tx);
                        self.tx.borrow_mut().write(idx_root_cell, Value::PMap(new_root));
                    }
                }
            }
        }
        Ok(())
    }

    /// Evaluate a for-in iterator. If `iter` is a state ident referring
    /// to a pmap, pbtree, or pvec, walk the persistent collection and
    /// return the materialized array. Otherwise fall back to ordinary
    /// eval (which handles array literals, function returns, etc.).
    ///
    /// **Materialize-vs-stream asymmetry, intentional.** The bytecode
    /// VM lowers `for x in pmap_state` (and pbtree, pvec) to a cursor
    /// pair (`PMapWalkInit` / `PMapWalkNext`, etc.) that fetches cells
    /// lazily — `break` after a match avoids touching the rest of the
    /// tree. The interp is simpler: it walks the whole collection up
    /// front and the for-loop iterates the materialized array. Same
    /// observable behavior (sorted order on pbtree, key-derived order
    /// on pmap, push order on pvec; `break` exits at the same logical
    /// point), but the interp doesn't realize the streaming
    /// performance win — every for-in pays the full O(N) cell read
    /// cost regardless of how many entries the body actually visits.
    ///
    /// This is fine because the interp is the development/testing
    /// reference, not the production hot path. Anything that needs
    /// streaming behavior must run through `Engine::execute*` (which
    /// uses the bytecode VM). Tests that specifically verify
    /// streaming use that path.
    fn eval_iter_to_array(
        &self,
        iter: &Expr,
        scopes: &mut Vec<Scope>,
        span: Span,
    ) -> Result<Value, Error> {
        if let ExprKind::Ident(name) = &iter.kind {
            if let Some(state_ty) = self.state_types.get(name) {
                match state_ty.clone() {
                    Type::PMap { key, value } => {
                        let root_cell = *self.state_roots.get(name).expect("known state");
                        let root_v = self.tx.borrow_mut().read_cell(root_cell, &Type::PMap {
                            key: key.clone(),
                            value: value.clone(),
                        });
                        let root_v = self.tx.borrow_mut().force(root_v);
                        let root_hash = match root_v {
                            Value::PMap(h) => h,
                            _ => crate::pmap::EMPTY,
                        };
                        let mut tx = self.tx.borrow_mut();
                        let entries = crate::pmap::entries(root_hash, &mut tx, &key, &value);
                        return Ok(Value::Array(
                            entries.into_iter().map(|(_, v)| v).collect(),
                        ));
                    }
                    Type::PVec { elem } => {
                        let root_cell = *self.state_roots.get(name).expect("known state");
                        let cell_v = self.tx.borrow_mut().read_cell(root_cell, &Type::PVec {
                            elem: elem.clone(),
                        });
                        let cell_v = self.tx.borrow_mut().force(cell_v);
                        let (len, root) = match cell_v {
                            Value::PVec { len, root } => (len, root),
                            _ => (0, crate::pvec::EMPTY),
                        };
                        let mut tx = self.tx.borrow_mut();
                        let elements = crate::pvec::to_vec(root, len, &mut tx, &elem)?;
                        return Ok(Value::Array(elements));
                    }
                    Type::PBTree { key, value } => {
                        let root_cell = *self.state_roots.get(name).expect("known state");
                        let pbtree_ty = Type::PBTree { key: key.clone(), value: value.clone() };
                        let root_v = self.tx.borrow_mut().read_cell(root_cell, &pbtree_ty);
                        let root_v = self.tx.borrow_mut().force(root_v);
                        let root_hash = match root_v {
                            Value::PBTree(h) => h,
                            _ => crate::pbtree::EMPTY,
                        };
                        let mut tx = self.tx.borrow_mut();
                        let entries = crate::pbtree::entries(root_hash, &mut tx, &key, &value);
                        return Ok(Value::Array(
                            entries.into_iter().map(|(_, v)| v).collect(),
                        ));
                    }
                    _ => {}
                }
            }
        }
        // Suppress the now-unused `span` if we fall through.
        let _ = span;
        self.eval(iter, scopes)
    }

    fn eval(&self, expr: &Expr, scopes: &mut Vec<Scope>) -> Result<Value, Error> {
        match &expr.kind {
            ExprKind::Int(n) => Ok(Value::int(n.clone())),
            ExprKind::UInt(n) => Ok(Value::uint(n.clone())),
            ExprKind::Float(n) => Ok(Value::Float(*n)),
            ExprKind::JsonNull => Ok(Value::Json(crate::json::Json::Null)),
            ExprKind::JsonObject(pairs) => {
                let mut map = indexmap::IndexMap::with_capacity(pairs.len());
                for (k, v_expr) in pairs {
                    let v = self.eval(v_expr, scopes)?;
                    map.insert(k.clone(), v);
                }
                Ok(Value::Json(crate::json::Json::Object(map)))
            }
            ExprKind::JsonArray(items) => {
                let mut out = Vec::with_capacity(items.len());
                for item in items {
                    out.push(self.eval(item, scopes)?);
                }
                Ok(Value::Json(crate::json::Json::Array(out)))
            }
            ExprKind::I32(n) => Ok(Value::I32(*n)),
            ExprKind::U32(n) => Ok(Value::U32(*n)),
            ExprKind::U64(n) => Ok(Value::U64(*n)),
            ExprKind::U128(n) => Ok(Value::U128(*n)),
            ExprKind::Bool(b) => Ok(Value::Bool(*b)),
            ExprKind::Str(s) => Ok(Value::Str(s.clone())),
            ExprKind::Ident(name) => self.lookup(name, scopes, expr.span),
            ExprKind::Binary { op, lhs, rhs } => {
                let l = self.eval(lhs, scopes)?;
                let r = self.eval(rhs, scopes)?;
                eval_binary(*op, l, r, expr.span)
            }
            ExprKind::Unary { op, operand } => {
                let v = self.eval(operand, scopes)?;
                eval_unary(*op, v, expr.span)
            }
            ExprKind::Index { target, key } => {
                // Map state read?
                if let ExprKind::Ident(state_name) = &target.kind {
                    if let Some(Type::Map { value, .. }) = self.state_types.get(state_name) {
                        let key_v = self.eval(key, scopes)?;
                        let root = *self.state_roots.get(state_name).expect("known state");
                        let cell = compose_map_key(root, &key_v, expr.span)?;
                        let default = self.state_defaults.get(state_name).cloned().unwrap_or(Value::Unit);
                        return Ok(self.tx.borrow_mut().read(cell, value, default));
                    }
                    // pvec state read — same shape as pmap but
                    // indexed by i64.
                    if let Some(Type::PVec { elem }) = self.state_types.get(state_name).cloned() {
                        let key_v = self.eval(key, scopes)?;
                        let i = match key_v {
                            Value::Int(ref n) => match num_traits::ToPrimitive::to_u64(n) {
                                Some(v) => v,
                                None => return Err(Error::new(
                                    ErrorKind::Runtime,
                                    format!("pvec index out of u64 range: {n}"),
                                    expr.span,
                                )),
                            },
                            other => return Err(Error::new(
                                ErrorKind::Runtime,
                                format!("pvec index: expected non-negative int, got {other}"),
                                expr.span,
                            )),
                        };
                        let root_cell = *self.state_roots.get(state_name).expect("known state");
                        let pvec_ty = Type::PVec { elem: elem.clone() };
                        let cell_v = self.tx.borrow_mut().read_cell(root_cell, &pvec_ty);
                        let cell_v = self.tx.borrow_mut().force(cell_v);
                        let (len, root) = match cell_v {
                            Value::PVec { len, root } => (len, root),
                            _ => (0, crate::pvec::EMPTY),
                        };
                        let mut tx = self.tx.borrow_mut();
                        return crate::pvec::get(root, len, i, &mut tx, &elem, expr.span);
                    }
                    // pmap state read. The state cell stores the
                    // root hash (a u128); the HAMT itself lives in
                    // content-addressed node cells. `pmap::get`
                    // walks O(log32 N) nodes on demand via Tx, so
                    // a multi-gigabyte pmap never sits in memory.
                    if let Some(Type::PMap { key: kt, value: vt }) = self.state_types.get(state_name).cloned() {
                        let key_v = self.eval(key, scopes)?;
                        let root_cell = *self.state_roots.get(state_name).expect("known state");
                        let pmap_ty = Type::PMap { key: kt.clone(), value: vt.clone() };
                        let root_hash = match self.tx.borrow_mut().read_cell(root_cell, &pmap_ty) {
                            Value::PMap(h) => h,
                            _ => crate::pmap::EMPTY,
                        };
                        let mut tx = self.tx.borrow_mut();
                        return Ok(crate::pmap::get(root_hash, &key_v, &mut tx, &kt, &vt)
                            .unwrap_or_else(|| Value::default_for(&vt)));
                    }
                    // pbtree state read — same shape as pmap.
                    if let Some(Type::PBTree { key: kt, value: vt }) = self.state_types.get(state_name).cloned() {
                        let key_v = self.eval(key, scopes)?;
                        let root_cell = *self.state_roots.get(state_name).expect("known state");
                        let pbtree_ty = Type::PBTree { key: kt.clone(), value: vt.clone() };
                        let root_hash = match self.tx.borrow_mut().read_cell(root_cell, &pbtree_ty) {
                            Value::PBTree(h) => h,
                            _ => crate::pbtree::EMPTY,
                        };
                        let mut tx = self.tx.borrow_mut();
                        return Ok(crate::pbtree::get(root_hash, &key_v, &mut tx, &kt, &vt)
                            .unwrap_or_else(|| Value::default_for(&vt)));
                    }
                }
                // Array indexing
                let arr_v = self.eval(target, scopes)?;
                let idx_v = self.eval(key, scopes)?;
                match (arr_v, idx_v) {
                    (Value::Array(elems), Value::Int(i)) => {
                        let idx = match num_traits::ToPrimitive::to_usize(&i) {
                            Some(v) if v < elems.len() => v,
                            _ => return Err(Error::new(
                                ErrorKind::Runtime,
                                format!("array index out of bounds: {i} of {}", elems.len()),
                                expr.span,
                            )),
                        };
                        Ok(elems[idx].clone())
                    }
                    (a, k) => Err(Error::new(
                        ErrorKind::Runtime,
                        format!("can't index {a} with {k}"),
                        expr.span,
                    )),
                }
            }
            ExprKind::Array(elems) => {
                let mut out = Vec::with_capacity(elems.len());
                for e in elems {
                    out.push(self.eval(e, scopes)?);
                }
                Ok(Value::Array(out))
            }
            ExprKind::ArrayAlloc { elem_ty, len } => {
                let len_v = self.eval(len, scopes)?;
                let n = value_to_usize(&len_v).ok_or_else(|| Error::new(
                    ErrorKind::Runtime,
                    format!("`arr<T>[N]` length must be a non-negative integer, got {len_v}"),
                    len.span,
                ))?;
                let default = Value::default_for(elem_ty);
                let buf = vec![default; n];
                Ok(Value::Array(buf))
            }
            ExprKind::ExplicitLiteral(inner) => self.eval(inner, scopes),
            ExprKind::Reserve { count, state } => {
                let count_v = self.eval(count, scopes)?;
                let n = value_to_u64(&count_v).ok_or_else(|| Error::new(
                    ErrorKind::Runtime,
                    format!("`reserve N from ...` count must be a non-negative integer, got {count_v}"),
                    count.span,
                ))?;
                let prev = match self.read_state(state) {
                    Value::U64(p) => p,
                    other => return Err(Error::new(
                        ErrorKind::Runtime,
                        format!("`reserve ... from {state}`: state must be u64, got {other}"),
                        expr.span,
                    )),
                };
                self.write_state(state, Value::U64(prev + n));
                let mut out = Vec::with_capacity(n as usize);
                for i in 0..n {
                    out.push(Value::U64(prev + i + 1));
                }
                Ok(Value::Array(out))
            }
            ExprKind::StructLit { name, fields } => {
                let decl = self.struct_decls.get(name).ok_or_else(|| {
                    Error::new(
                        ErrorKind::Runtime,
                        format!("unknown struct '{name}'"),
                        expr.span,
                    )
                })?;
                let mut out = Vec::with_capacity(decl.len());
                for declared_name in decl.iter() {
                    let provided = fields
                        .iter()
                        .find(|(n, _)| n == declared_name)
                        .ok_or_else(|| {
                            Error::new(
                                ErrorKind::Runtime,
                                format!("missing field '{declared_name}' in struct '{name}'"),
                                expr.span,
                            )
                        })?;
                    let v = self.eval(&provided.1, scopes)?;
                    out.push((declared_name.clone(), v));
                }
                Ok(Value::Struct { name: name.clone(), fields: out })
            }
            ExprKind::Field { target, name } => {
                if let Some((state_name, path)) = self.try_state_field_path(expr, scopes) {
                    return Ok(self.with_state_path(&state_name, &path, |key, ty| {
                        self.tx.borrow_mut().read_typed(key, ty)
                    }));
                }
                let v = self.eval(target, scopes)?;
                let Value::Struct { fields, .. } = v else {
                    return Err(Error::new(
                        ErrorKind::Runtime,
                        "field access on non-struct",
                        target.span,
                    ));
                };
                fields
                    .into_iter()
                    .find(|(n, _)| n == name)
                    .map(|(_, v)| v)
                    .ok_or_else(|| Error::new(
                        ErrorKind::Runtime,
                        format!("no field '{name}'"),
                        expr.span,
                    ))
            }
            ExprKind::Prev => self
                .pipe_stack
                .borrow()
                .last()
                .cloned()
                .ok_or_else(|| Error::new(
                    ErrorKind::Runtime,
                    "$$ used outside of a pipe stage",
                    expr.span,
                )),
            ExprKind::Pipe { head, step } => {
                let v = self.eval(head, scopes)?;
                self.pipe_stack.borrow_mut().push(v);
                let result = self.eval(step, scopes);
                self.pipe_stack.borrow_mut().pop();
                result
            }
            ExprKind::SetLit(elems) => {
                let mut out: Vec<Value> = Vec::with_capacity(elems.len());
                for e in elems {
                    let v = self.eval(e, scopes)?;
                    if !out.iter().any(|x| x == &v) { out.push(v); }
                }
                Ok(Value::Set(out))
            }
            ExprKind::DictLit(pairs) => {
                let mut out: Vec<(Value, Value)> = Vec::with_capacity(pairs.len());
                for (kx, vx) in pairs {
                    let k = self.eval(kx, scopes)?;
                    let v = self.eval(vx, scopes)?;
                    if let Some(pair) = out.iter_mut().find(|(kk, _)| kk == &k) {
                        pair.1 = v;
                    } else {
                        out.push((k, v));
                    }
                }
                Ok(Value::Dict(out))
            }
            ExprKind::SetComp { mapper, clauses } => {
                let mut out: Vec<Value> = Vec::new();
                self.walk_comp_clauses(clauses, 0, scopes, &mut |this, scopes| {
                    let v = this.eval(mapper, scopes)?;
                    if !out.iter().any(|x| x == &v) { out.push(v); }
                    Ok(())
                })?;
                Ok(Value::Set(out))
            }
            ExprKind::DictComp { key, value, clauses } => {
                let mut out: Vec<(Value, Value)> = Vec::new();
                self.walk_comp_clauses(clauses, 0, scopes, &mut |this, scopes| {
                    let k = this.eval(key, scopes)?;
                    let v = this.eval(value, scopes)?;
                    if let Some(pair) = out.iter_mut().find(|(kk, _)| kk == &k) {
                        pair.1 = v;
                    } else {
                        out.push((k, v));
                    }
                    Ok(())
                })?;
                Ok(Value::Dict(out))
            }
            ExprKind::ListComp { mapper, clauses } => {
                let mut out: Vec<Value> = Vec::new();
                self.walk_comp_clauses(clauses, 0, scopes, &mut |this, scopes| {
                    let v = this.eval(mapper, scopes)?;
                    out.push(v);
                    Ok(())
                })?;
                Ok(Value::Array(out))
            }
            ExprKind::DynCall { target_ident, method, args, .. } => {
                // Resolve the target to its bound module name.
                // Targets are Copy (interface = string handle), so
                // we don't worry about moves here.
                let target_v = self.lookup(target_ident, scopes, expr.span)?;
                let target_module = match target_v {
                    Value::Interface { target_module, .. } => target_module,
                    other => return Err(Error::new(
                        ErrorKind::Runtime,
                        format!(
                            "dynamic dispatch on non-interface value: {other}",
                        ),
                        expr.span,
                    )),
                };
                if target_module.is_empty() {
                    return Err(Error::new(
                        ErrorKind::Runtime,
                        format!(
                            "interface '{target_ident}' is unbound — call \
                             IFace::bind(\"module_name\") before dispatch",
                        ),
                        expr.span,
                    ));
                }
                // The tree-walk interp doesn't support cross-module
                // calls today (only the bytecode VM does); same
                // restriction applies to dynamic dispatch.
                let _ = method;
                let _ = args;
                return Err(Error::new(
                    ErrorKind::Runtime,
                    format!(
                        "tree-walk interp does not support dynamic dispatch \
                         (target '{target_module}'); use Engine::execute_*",
                    ),
                    expr.span,
                ));
            }
            ExprKind::Call { module, name, args } => {
                // pvec_push / pvec_len need direct Tx access to walk
                // / mutate the trie. Bypass the value-only
                // `try_builtin` path.
                if matches!(name.as_str(), "pvec_push" | "pvec_len") && module.is_none() {
                    let state_name = match args.first().map(|a| &a.kind) {
                        Some(ExprKind::Ident(n)) => n.clone(),
                        _ => return Err(Error::new(
                            ErrorKind::Runtime,
                            format!("{name}() arg 0 must be a pvec state"),
                            expr.span,
                        )),
                    };
                    let pvec_ty = self.state_types.get(&state_name).cloned().ok_or_else(|| Error::new(
                        ErrorKind::Runtime,
                        format!("'{state_name}' is not a state"),
                        expr.span,
                    ))?;
                    let elem = match &pvec_ty {
                        Type::PVec { elem } => (**elem).clone(),
                        _ => return Err(Error::new(
                            ErrorKind::Runtime,
                            format!("'{state_name}' is not a pvec"),
                            expr.span,
                        )),
                    };
                    let root_cell = *self.state_roots.get(&state_name).expect("known state");
                    let cell_v = self.tx.borrow_mut().read_cell(root_cell, &pvec_ty);
                    let cell_v = self.tx.borrow_mut().force(cell_v);
                    let (len, root) = match cell_v {
                        Value::PVec { len, root } => (len, root),
                        _ => (0, crate::pvec::EMPTY),
                    };
                    if name == "pvec_len" {
                        return Ok(Value::U64(len));
                    }
                    // pvec_push
                    let value = self.eval(&args[1], scopes)?;
                    let mut tx = self.tx.borrow_mut();
                    let (new_root, new_len) = crate::pvec::push(root, len, value, &mut tx, &elem)?;
                    drop(tx);
                    self.tx.borrow_mut().write(root_cell, Value::PVec { len: new_len, root: new_root });
                    return Ok(Value::U64(len));
                }
                // pmap_contains is tree-walking — needs direct Tx
                // access to read node cells lazily. Bypass the
                // generic call path so we don't have to pass a tx
                // through the value-only `try_builtin`.
                if name == "pbtree_contains" && module.is_none() {
                    let state_name = match args.first().map(|a| &a.kind) {
                        Some(ExprKind::Ident(n)) => n.clone(),
                        _ => return Err(Error::new(
                            ErrorKind::Runtime,
                            "pbtree_contains() arg 0 must be a pbtree state".to_string(),
                            expr.span,
                        )),
                    };
                    let pbtree_ty = self.state_types.get(&state_name).cloned().ok_or_else(|| Error::new(
                        ErrorKind::Runtime, format!("'{state_name}' is not a state"), expr.span,
                    ))?;
                    let (kt, vt) = match &pbtree_ty {
                        Type::PBTree { key, value } => ((**key).clone(), (**value).clone()),
                        _ => return Err(Error::new(
                            ErrorKind::Runtime,
                            format!("'{state_name}' is not a pbtree"),
                            expr.span,
                        )),
                    };
                    let root_cell = *self.state_roots.get(&state_name).expect("known state");
                    let root_v = self.tx.borrow_mut().read_cell(root_cell, &pbtree_ty);
                    let root_v = self.tx.borrow_mut().force(root_v);
                    let root_hash = match root_v {
                        Value::PBTree(h) => h,
                        _ => crate::pbtree::EMPTY,
                    };
                    let key_v = self.eval(&args[1], scopes)?;
                    let mut tx = self.tx.borrow_mut();
                    return Ok(Value::Bool(
                        crate::pbtree::contains(root_hash, &key_v, &mut tx, &kt, &vt),
                    ));
                }
                if name == "pbtree_range" && module.is_none() {
                    let state_name = match args.first().map(|a| &a.kind) {
                        Some(ExprKind::Ident(n)) => n.clone(),
                        _ => return Err(Error::new(
                            ErrorKind::Runtime,
                            "pbtree_range() arg 0 must be a pbtree state".to_string(),
                            expr.span,
                        )),
                    };
                    let pbtree_ty = self.state_types.get(&state_name).cloned().ok_or_else(|| Error::new(
                        ErrorKind::Runtime, format!("'{state_name}' is not a state"), expr.span,
                    ))?;
                    let (kt, vt) = match &pbtree_ty {
                        Type::PBTree { key, value } => ((**key).clone(), (**value).clone()),
                        _ => return Err(Error::new(
                            ErrorKind::Runtime,
                            format!("'{state_name}' is not a pbtree"),
                            expr.span,
                        )),
                    };
                    let root_cell = *self.state_roots.get(&state_name).expect("known state");
                    let root_v = self.tx.borrow_mut().read_cell(root_cell, &pbtree_ty);
                    let root_v = self.tx.borrow_mut().force(root_v);
                    let root_hash = match root_v {
                        Value::PBTree(h) => h,
                        _ => crate::pbtree::EMPTY,
                    };
                    let lo_v = self.eval(&args[1], scopes)?;
                    let hi_v = self.eval(&args[2], scopes)?;
                    let mut tx = self.tx.borrow_mut();
                    let result = crate::pbtree::range(root_hash, &lo_v, &hi_v, &mut tx, &kt, &vt);
                    return Ok(Value::Array(result));
                }
                if name == "pmap_contains" && module.is_none() {
                    let state_name = match args.first().map(|a| &a.kind) {
                        Some(ExprKind::Ident(n)) => n.clone(),
                        _ => return Err(Error::new(
                            ErrorKind::Runtime,
                            format!("{name}() arg 0 must be a pmap state"),
                            expr.span,
                        )),
                    };
                    let pmap_ty = self.state_types.get(&state_name).cloned().ok_or_else(|| {
                        Error::new(
                            ErrorKind::Runtime,
                            format!("'{state_name}' is not a state"),
                            expr.span,
                        )
                    })?;
                    let (key_ty, value_ty) = match &pmap_ty {
                        Type::PMap { key, value } => ((**key).clone(), (**value).clone()),
                        _ => return Err(Error::new(
                            ErrorKind::Runtime,
                            format!("'{state_name}' is not a pmap"),
                            expr.span,
                        )),
                    };
                    let root_cell = *self.state_roots.get(&state_name).expect("known state");
                    let root_v = self.tx.borrow_mut().read_cell(root_cell, &pmap_ty);
                    let root_v = self.tx.borrow_mut().force(root_v);
                    let root_hash = match root_v {
                        Value::PMap(h) => h,
                        _ => crate::pmap::EMPTY,
                    };
                    let key_v = self.eval(&args[1], scopes)?;
                    let mut tx = self.tx.borrow_mut();
                    return Ok(Value::Bool(
                        crate::pmap::contains(root_hash, &key_v, &mut tx, &key_ty, &value_ty)
                    ));
                }
                // Whole-tree walks. Same shape as pmap_contains but
                // produce arrays rather than booleans. The interp
                // handles them so single-source `run(...)` tests work.
                if matches!(name.as_str(),
                    "pmap_entries" | "pmap_keys" | "pmap_values"
                ) && module.is_none() {
                    let state_name = match args.first().map(|a| &a.kind) {
                        Some(ExprKind::Ident(n)) => n.clone(),
                        _ => return Err(Error::new(
                            ErrorKind::Runtime,
                            format!("{name}() arg 0 must be a pmap state"),
                            expr.span,
                        )),
                    };
                    let pmap_ty = self.state_types.get(&state_name).cloned().ok_or_else(|| {
                        Error::new(
                            ErrorKind::Runtime,
                            format!("'{state_name}' is not a state"),
                            expr.span,
                        )
                    })?;
                    let (key_ty, value_ty) = match &pmap_ty {
                        Type::PMap { key, value } => ((**key).clone(), (**value).clone()),
                        _ => return Err(Error::new(
                            ErrorKind::Runtime,
                            format!("'{state_name}' is not a pmap"),
                            expr.span,
                        )),
                    };
                    let root_cell = *self.state_roots.get(&state_name).expect("known state");
                    let root_v = self.tx.borrow_mut().read_cell(root_cell, &pmap_ty);
                    let root_v = self.tx.borrow_mut().force(root_v);
                    let root_hash = match root_v {
                        Value::PMap(h) => h,
                        _ => crate::pmap::EMPTY,
                    };
                    let mut tx = self.tx.borrow_mut();
                    let entries = crate::pmap::entries(root_hash, &mut tx, &key_ty, &value_ty);
                    return Ok(Value::Array(match name.as_str() {
                        "pmap_entries" => entries.into_iter()
                            .map(|(k, v)| Value::Tuple(vec![k, v]))
                            .collect(),
                        "pmap_keys" => entries.into_iter().map(|(k, _)| k).collect(),
                        "pmap_values" => entries.into_iter().map(|(_, v)| v).collect(),
                        _ => unreachable!(),
                    }));
                }
                if name == "pvec_to_array" && module.is_none() {
                    let state_name = match args.first().map(|a| &a.kind) {
                        Some(ExprKind::Ident(n)) => n.clone(),
                        _ => return Err(Error::new(
                            ErrorKind::Runtime,
                            "pvec_to_array() arg 0 must be a pvec state".to_string(),
                            expr.span,
                        )),
                    };
                    let pvec_ty = self.state_types.get(&state_name).cloned().ok_or_else(|| {
                        Error::new(
                            ErrorKind::Runtime,
                            format!("'{state_name}' is not a state"),
                            expr.span,
                        )
                    })?;
                    let elem_ty = match &pvec_ty {
                        Type::PVec { elem } => (**elem).clone(),
                        _ => return Err(Error::new(
                            ErrorKind::Runtime,
                            format!("'{state_name}' is not a pvec"),
                            expr.span,
                        )),
                    };
                    let root_cell = *self.state_roots.get(&state_name).expect("known state");
                    let cell_v = self.tx.borrow_mut().read_cell(root_cell, &pvec_ty);
                    let cell_v = self.tx.borrow_mut().force(cell_v);
                    let (len, root) = match cell_v {
                        Value::PVec { len, root } => (len, root),
                        _ => (0, crate::pvec::EMPTY),
                    };
                    let mut tx = self.tx.borrow_mut();
                    let elements = crate::pvec::to_vec(root, len, &mut tx, &elem_ty)?;
                    return Ok(Value::Array(elements));
                }
                let mut vals = Vec::with_capacity(args.len());
                for a in args {
                    vals.push(self.eval(a, scopes)?);
                }
                if module.is_some() {
                    return Err(Error::new(
                        ErrorKind::Runtime,
                        "tree-walk interp does not support cross-module calls; use Engine::execute_main",
                        expr.span,
                    ));
                }
                self.call(name, vals)
            }
            ExprKind::TupleLit(elems) => {
                let mut out = Vec::with_capacity(elems.len());
                for e in elems {
                    out.push(self.eval(e, scopes)?);
                }
                Ok(Value::Tuple(out))
            }
            ExprKind::TupleIndex { target, index } => {
                let v = self.eval(target, scopes)?;
                let elems = match v {
                    Value::Tuple(elems) => elems,
                    other => return Err(Error::new(
                        ErrorKind::Runtime,
                        format!("tuple index on non-tuple: {other}"),
                        target.span,
                    )),
                };
                if *index >= elems.len() {
                    return Err(Error::new(
                        ErrorKind::Runtime,
                        format!("tuple index {index} out of bounds (len {})", elems.len()),
                        expr.span,
                    ));
                }
                Ok(elems[*index].clone())
            }
            ExprKind::EnumCtor { enum_name, variant, args } => {
                let mut payload = Vec::with_capacity(args.len());
                for a in args {
                    payload.push(self.eval(a, scopes)?);
                }
                Ok(Value::Enum {
                    enum_name: enum_name.clone(),
                    variant: variant.clone(),
                    payload,
                })
            }
            ExprKind::Block(block) => {
                scopes.push(Scope::default());
                for s in &block.stmts {
                    match self.exec_stmt(s, scopes)? {
                        Flow::Normal(_) => {}
                        other => {
                            scopes.pop();
                            return match other {
                                Flow::Return(v) => Ok(v),
                                _ => Err(Error::new(
                                    ErrorKind::Runtime,
                                    "break/continue inside block expression",
                                    expr.span,
                                )),
                            };
                        }
                    }
                }
                let v = match &block.tail {
                    Some(t) => self.eval(t, scopes)?,
                    None => Value::Unit,
                };
                scopes.pop();
                Ok(v)
            }
            ExprKind::If { cond, then, else_branch } => {
                let c = self.eval(cond, scopes)?;
                let take_then = matches!(c, Value::Bool(true));
                let body_block: &Block = if take_then {
                    then
                } else {
                    match else_branch {
                        ElseBranch::None => return Err(Error::new(
                            ErrorKind::Runtime,
                            "if-expression missing else branch",
                            expr.span,
                        )),
                        ElseBranch::Block(b) => b,
                        ElseBranch::If(inner) => {
                            let synthetic = Expr {
                                kind: ExprKind::If {
                                    cond: Box::new(inner.cond.clone()),
                                    then: inner.then.clone(),
                                    else_branch: inner.else_branch.clone(),
                                },
                                span: inner.span,
                            };
                            return self.eval(&synthetic, scopes);
                        }
                    }
                };
                let synthetic = Expr {
                    kind: ExprKind::Block(body_block.clone()),
                    span: body_block.span,
                };
                self.eval(&synthetic, scopes)
            }
            ExprKind::Match { scrut, arms } => {
                let v = self.eval(scrut, scopes)?;
                let (got_variant, got_payload) = match v {
                    Value::Enum { variant, payload, .. } => (variant, payload),
                    other => return Err(Error::new(
                        ErrorKind::Runtime,
                        format!("match on non-enum: {other}"),
                        scrut.span,
                    )),
                };
                for arm in arms {
                    let matches_arm = match &arm.pattern {
                        MatchPattern::Wildcard => true,
                        MatchPattern::EnumVariant { variant, .. } => variant == &got_variant,
                    };
                    if !matches_arm { continue; }
                    scopes.push(Scope::default());
                    if let MatchPattern::EnumVariant { bindings, .. } = &arm.pattern {
                        for (n, v) in bindings.iter().zip(got_payload.iter()) {
                            scopes.last_mut().unwrap().vars.insert(n.clone(), v.clone());
                        }
                    }
                    let r = self.eval(&arm.body, scopes);
                    scopes.pop();
                    return r;
                }
                Err(Error::new(
                    ErrorKind::Runtime,
                    format!("no arm matched variant '{got_variant}'"),
                    expr.span,
                ))
            }
        }
    }

    /// Recursively assign `value` into the lvalue at `target`. For an Ident,
    /// writes to the local or state slot. For an Index of a map state, writes
    /// to the map cell. For a Field path (`obj.q.x = v`), reads the parent
    /// struct, replaces the named field, and recursively assigns the
    /// modified copy back to the parent.
    fn assign_path(&self, target: &Expr, value: Value, scopes: &mut Vec<Scope>) -> Result<(), Error> {
        match &target.kind {
            ExprKind::Ident(name) => {
                for scope in scopes.iter_mut().rev() {
                    if scope.vars.contains_key(name) {
                        scope.vars.insert(name.clone(), value);
                        return Ok(());
                    }
                }
                self.write_state(name, value);
                Ok(())
            }
            ExprKind::Index { target: t, key } => {
                let ExprKind::Ident(state_name) = &t.kind else {
                    return Err(Error::new(
                        ErrorKind::Runtime,
                        "indexed assignment target must be a state",
                        target.span,
                    ));
                };
                let key_v = self.eval(key, scopes)?;
                let root = *self.state_roots.get(state_name).expect("known state");
                // pvec indexed assignment.
                if let Some(Type::PVec { elem }) = self.state_types.get(state_name).cloned() {
                    let i = match key_v {
                        Value::Int(ref n) => match num_traits::ToPrimitive::to_u64(n) {
                            Some(v) => v,
                            None => return Err(Error::new(
                                ErrorKind::Runtime,
                                format!("pvec index out of u64 range: {n}"),
                                target.span,
                            )),
                        },
                        other => return Err(Error::new(
                            ErrorKind::Runtime,
                            format!("pvec index: expected non-negative int, got {other}"),
                            target.span,
                        )),
                    };
                    let pvec_ty = Type::PVec { elem: elem.clone() };
                    let cell_v = self.tx.borrow_mut().read_cell(root, &pvec_ty);
                    let cell_v = self.tx.borrow_mut().force(cell_v);
                    let (len, old_root) = match cell_v {
                        Value::PVec { len, root } => (len, root),
                        _ => (0, crate::pvec::EMPTY),
                    };
                    let mut tx = self.tx.borrow_mut();
                    let new_root = crate::pvec::set(old_root, len, i, value, &mut tx, &elem, target.span)?;
                    drop(tx);
                    self.tx.borrow_mut().write(root, Value::PVec { len, root: new_root });
                    return Ok(());
                }
                // pmap assignment — fetch the current root hash,
                // apply set (which writes O(log32 N) new node cells
                // and returns the new root hash), then store the
                // new root in the state cell. HAMT structural
                // sharing keeps the rewrite cost logarithmic even
                // for gigabyte-scale trees.
                if let Some(Type::PMap { key: kt, value: vt }) = self.state_types.get(state_name).cloned() {
                    let pmap_ty = Type::PMap { key: kt.clone(), value: vt.clone() };
                    let root_hash = match self.tx.borrow_mut().read_cell(root, &pmap_ty) {
                        Value::PMap(h) => h,
                        _ => crate::pmap::EMPTY,
                    };
                    // Auto-maintain indexes covering this pmap: for
                    // each index, project the value's indexed field
                    // and write `index[v.field] = key`. Done before
                    // the primary write so the primary's `value` is
                    // still available for projection (structs are
                    // Copy by convention so cloning is cheap).
                    let key_for_idx = key_v.clone();
                    let value_for_idx = value.clone();
                    let mut tx = self.tx.borrow_mut();
                    let new_root = crate::pmap::set(root_hash, key_v, value, &mut tx, &kt, &vt)?;
                    drop(tx);
                    self.tx.borrow_mut().write(root, Value::PMap(new_root));
                    self.maintain_indexes(state_name, &key_for_idx, &value_for_idx)?;
                    return Ok(());
                }
                // pbtree state assignment — same shape as pmap, but
                // through the sorted-trie module. Indexes are not
                // currently supported on pbtree (slice-1 limit).
                if let Some(Type::PBTree { key: kt, value: vt }) = self.state_types.get(state_name).cloned() {
                    let pbtree_ty = Type::PBTree { key: kt.clone(), value: vt.clone() };
                    let root_hash = match self.tx.borrow_mut().read_cell(root, &pbtree_ty) {
                        Value::PBTree(h) => h,
                        _ => crate::pbtree::EMPTY,
                    };
                    let mut tx = self.tx.borrow_mut();
                    let new_root = crate::pbtree::set(root_hash, key_v, value, &mut tx, &kt, &vt)?;
                    drop(tx);
                    self.tx.borrow_mut().write(root, Value::PBTree(new_root));
                    return Ok(());
                }
                let cell = compose_map_key(root, &key_v, target.span)?;
                self.tx.borrow_mut().write(cell, value);
                Ok(())
            }
            ExprKind::Field { target: inner, name } => {
                // Fast path: if the entire chain bottoms at a state slot, do
                // a single leaf write — never read+rebuild the parent struct.
                if let Some((state_name, path)) = self.try_state_field_path(target, scopes) {
                    self.with_state_path(&state_name, &path, |key, ty| {
                        self.tx.borrow_mut().write_typed(key, ty, value);
                    });
                    return Ok(());
                }
                // Slow path: parent is a local Copy struct — read, modify,
                // recursively write back up the chain.
                let parent = self.eval(inner, scopes)?;
                let Value::Struct { name: sname, mut fields } = parent else {
                    return Err(Error::new(
                        ErrorKind::Runtime,
                        "field assignment on non-struct",
                        inner.span,
                    ));
                };
                let pair = fields
                    .iter_mut()
                    .find(|(n, _)| n == name)
                    .ok_or_else(|| Error::new(
                        ErrorKind::Runtime,
                        format!("no field '{name}'"),
                        target.span,
                    ))?;
                pair.1 = value;
                self.assign_path(inner, Value::Struct { name: sname, fields }, scopes)
            }
            _ => Err(Error::new(
                ErrorKind::Runtime,
                "invalid assignment target",
                target.span,
            )),
        }
    }

    /// Walk a comprehension clause sequence, calling `emit` with the bindings
    /// in scope at the innermost point of each successful iteration. `For`
    /// pushes a scope per iteration; `If` short-circuits.
    fn walk_comp_clauses(
        &self,
        clauses: &[CompClause],
        i: usize,
        scopes: &mut Vec<Scope>,
        emit: &mut dyn FnMut(&Self, &mut Vec<Scope>) -> Result<(), Error>,
    ) -> Result<(), Error> {
        if i == clauses.len() {
            return emit(self, scopes);
        }
        match &clauses[i] {
            CompClause::For { var, iter } => {
                let arr = self.eval_iter_to_array(iter, scopes, iter.span)?;
                let Value::Array(elems) = arr else {
                    return Err(Error::new(
                        ErrorKind::Runtime,
                        "comprehension iterator must be an array",
                        iter.span,
                    ));
                };
                for elem in elems {
                    scopes.push(Scope::default());
                    scopes.last_mut().unwrap().vars.insert(var.clone(), elem);
                    let r = self.walk_comp_clauses(clauses, i + 1, scopes, emit);
                    scopes.pop();
                    r?;
                }
                Ok(())
            }
            CompClause::If(cond) => {
                let c = self.eval(cond, scopes)?;
                let Value::Bool(b) = c else {
                    return Err(Error::new(
                        ErrorKind::Runtime,
                        format!("comprehension filter must be bool, got {c}"),
                        cond.span,
                    ));
                };
                if b {
                    self.walk_comp_clauses(clauses, i + 1, scopes, emit)
                } else {
                    Ok(())
                }
            }
        }
    }

    fn lookup(&self, name: &str, scopes: &mut Vec<Scope>, span: Span) -> Result<Value, Error> {
        for scope in scopes.iter().rev() {
            if let Some(v) = scope.vars.get(name) {
                return Ok(v.clone());
            }
        }
        if self.is_state(name) {
            return Ok(self.read_state(name));
        }
        // Module-level constants — resolve by re-evaluating the RHS
        // expression in an empty local scope. (Const RHSs only see
        // module-level bindings, which the rest of `eval` already
        // handles.)
        if let Some(c) = self.module.consts.iter().find(|c| c.name == name) {
            let rhs = c.value.clone();
            return self.eval(&rhs, scopes);
        }
        Err(Error::new(
            ErrorKind::Runtime,
            format!("undefined variable '{name}'"),
            span,
        ))
    }
}

// Convenience for callers that don't care about Tx (no state declarations).
impl<'a> Interp<'a> {
    pub fn without_storage(module: &'a Module, host: &'a Host, kv: &'a EmptyKv) -> Self {
        Self::new(module, host, Tx::new(kv))
    }
}

/// Project an index's composite/single key out of a stored
/// primary value. Single-field indexes return the projected value
/// directly (legacy shape: index slot is `pmap<K, ...>` or
/// `pbtree<K, ...>` keyed by the field type). Composite indexes
/// pack each projected field via big-endian byte encoding,
/// bit-inverting DESC components, and concatenate — the result
/// is a `Value::Bytes`. The index slot for composite must be
/// `pbtree<bytes, ...>`.
fn project_index_key(
    idx: &crate::ast::IndexDecl,
    primary_value: &Value,
) -> Result<Value, Error> {
    if idx.fields.len() == 1
        && idx.fields[0].direction == crate::ast::SortDirection::Asc
    {
        return walk_field_path(&idx.fields[0].path, primary_value, idx);
    }
    let mut packed: Vec<u8> = Vec::new();
    for f in &idx.fields {
        let v = walk_field_path(&f.path, primary_value, idx)?;
        let be = crate::ops::call_builtin("to_be_bytes", &[v])?;
        let part = if f.direction == crate::ast::SortDirection::Desc {
            crate::ops::call_builtin("bit_not_bytes", &[be])?
        } else {
            be
        };
        match part {
            Value::Bytes(b) => packed.extend(b),
            _ => unreachable!("to_be_bytes always returns bytes"),
        }
    }
    Ok(Value::Bytes(packed))
}

fn walk_field_path(
    path: &[String],
    primary_value: &Value,
    idx: &crate::ast::IndexDecl,
) -> Result<Value, Error> {
    let mut cur = primary_value.clone();
    for field in path {
        cur = match cur {
            Value::Struct { fields, .. } => fields
                .iter()
                .find(|(n, _)| n == field)
                .map(|(_, v)| v.clone())
                .ok_or_else(|| Error::new(
                    ErrorKind::Runtime,
                    format!(
                        "index '{}': field '{field}' missing on stored struct",
                        idx.name,
                    ),
                    idx.span,
                ))?,
            other => return Err(Error::new(
                ErrorKind::Runtime,
                format!(
                    "index '{}': cannot project '{field}' from {other}",
                    idx.name,
                ),
                idx.span,
            )),
        };
    }
    Ok(cur)
}

fn compose_map_key(root: u128, key: &Value, _span: Span) -> Result<u128, Error> {
    if !is_keyable_value(key) {
        return Err(Error::new(
            ErrorKind::Runtime,
            format!("map key value not keyable: {key}"),
            Span::default(),
        ));
    }
    let payload = crate::serialize::serialize(key);
    Ok(crate::hashing::child(root, &payload))
}

fn is_keyable_value(v: &Value) -> bool {
    match v {
        Value::Int(_) | Value::I32(_) | Value::U32(_) | Value::U64(_) | Value::U128(_)
        | Value::Bool(_) | Value::Str(_) | Value::Address(_) | Value::Bytes(_) => true,
        Value::Array(elems) => elems.iter().all(is_keyable_value),
        Value::Struct { fields, .. } => fields.iter().all(|(_, v)| is_keyable_value(v)),
        Value::Enum { payload, .. } => payload.iter().all(is_keyable_value),
        _ => false,
    }
}

/// Coerce a Value carrying a non-negative integer to u64. Returns
/// None for non-integer values or negative ints. Used to read
/// runtime length expressions for `arr<T>[N]` and `reserve N from`.
fn value_to_u64(v: &Value) -> Option<u64> {
    use num_traits::ToPrimitive;
    match v {
        Value::U64(n) => Some(*n),
        Value::U32(n) => Some(*n as u64),
        Value::I32(n) if *n >= 0 => Some(*n as u64),
        Value::U128(n) => (*n).try_into().ok(),
        Value::Int(n) => n.to_u64(),
        Value::UInt(n) => n.to_u64(),
        _ => None,
    }
}

fn value_to_usize(v: &Value) -> Option<usize> {
    value_to_u64(v).and_then(|n| n.try_into().ok())
}

/// Drop `` `T` `` wrappers from a type tree, recursively. Mirrors
/// the compile-time helper of the same name in compile.rs — used
/// to normalize state types before passing them to the storage
/// layer, so encoding stays symmetric across reads and writes.
fn strip_explicit_literal_ty(ty: &Type) -> Type {
    match ty {
        Type::ExplicitLiteral(inner) => strip_explicit_literal_ty(inner),
        Type::Array(elem) => Type::Array(Box::new(strip_explicit_literal_ty(elem))),
        Type::Set(elem) => Type::Set(Box::new(strip_explicit_literal_ty(elem))),
        Type::Dict { key, value } => Type::Dict {
            key: Box::new(strip_explicit_literal_ty(key)),
            value: Box::new(strip_explicit_literal_ty(value)),
        },
        Type::Map { key, value } => Type::Map {
            key: Box::new(strip_explicit_literal_ty(key)),
            value: Box::new(strip_explicit_literal_ty(value)),
        },
        Type::PMap { key, value } => Type::PMap {
            key: Box::new(strip_explicit_literal_ty(key)),
            value: Box::new(strip_explicit_literal_ty(value)),
        },
        Type::PBTree { key, value } => Type::PBTree {
            key: Box::new(strip_explicit_literal_ty(key)),
            value: Box::new(strip_explicit_literal_ty(value)),
        },
        Type::PVec { elem } => Type::PVec { elem: Box::new(strip_explicit_literal_ty(elem)) },
        Type::Struct { name, fields, field_groups } => Type::Struct {
            name: name.clone(),
            fields: fields.iter()
                .map(|(n, t)| (n.clone(), strip_explicit_literal_ty(t)))
                .collect(),
            field_groups: field_groups.clone(),
        },
        Type::Tuple(elems) => Type::Tuple(
            elems.iter().map(strip_explicit_literal_ty).collect()
        ),
        other => other.clone(),
    }
}

fn default_for_type(ty: &Type) -> Value {
    Value::default_for(ty)
}

fn eval_binary(op: BinOp, l: Value, r: Value, span: Span) -> Result<Value, Error> {
    crate::ops::eval_binary(op, l, r, span)
}

fn eval_unary(op: UnOp, v: Value, span: Span) -> Result<Value, Error> {
    crate::ops::eval_unary(op, v, span)
}
