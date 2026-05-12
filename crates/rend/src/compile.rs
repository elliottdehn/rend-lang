//! AST → bytecode lowering.
//!
//! Register allocation is dead simple: each new sub-expression claims the next
//! free register; LIFO frees on the way back up. Locals live in stable
//! registers for the duration of their scope. Function args go into a
//! contiguous range of fresh registers so the `Call` instruction can pass them
//! as a slice.

use std::collections::HashMap;

use crate::ast::*;
use crate::bc::{BcFn, BcModule, Const, Instr, PathSpec, StructShape};
use crate::error::{Error, ErrorKind};
use crate::prefetch::FnSummaries;
use crate::value::Value;

pub fn compile(module: &Module) -> Result<BcModule, Error> {
    compile_named(module, "main")
}

pub fn compile_named(module: &Module, module_name: &str) -> Result<BcModule, Error> {
    compile_named_with_iface_externals(module, module_name, &std::collections::HashSet::new())
}

/// Same as `compile_named`, but treats names in `iface_externals`
/// as interface names too — so a tx source can write
/// `IFoo::bind(...)` for an `IFoo` declared in a dep artifact.
/// Only the names are needed at compile time; method signatures
/// were already verified by typeck.
pub fn compile_named_with_iface_externals(
    module: &Module,
    module_name: &str,
    iface_externals: &std::collections::HashSet<String>,
) -> Result<BcModule, Error> {
    compile_named_with_extras(
        module,
        module_name,
        iface_externals,
        &std::collections::HashMap::new(),
    )
}

/// Same as `compile_named_with_iface_externals`, but also takes a
/// `(module, fn) → (is_view, is_pure)` map so the compiler can stamp
/// each emitted `Instr::CallExternal` with the callee's effect bound.
/// The optimizer reads those flags to keep a `view` cross-module call
/// from fencing a read cluster — same idea we already apply to
/// `CallExternalDyn`. When a `(module, fn)` pair isn't in the map we
/// default to `(false, false)` so the optimizer stays pessimistic.
pub fn compile_named_with_extras(
    module: &Module,
    module_name: &str,
    iface_externals: &std::collections::HashSet<String>,
    extern_fn_flags: &std::collections::HashMap<(String, String), (bool, bool)>,
) -> Result<BcModule, Error> {
    let mut imports = Vec::with_capacity(module.imports.len());
    let mut import_index = HashMap::new();
    for (i, imp) in module.imports.iter().enumerate() {
        imports.push(imp.name.clone());
        import_index.insert(imp.name.clone(), i);
    }
    let mut state_roots = Vec::with_capacity(module.states.len());
    let mut state_types = Vec::with_capacity(module.states.len());
    let mut state_defaults = Vec::with_capacity(module.states.len());
    let mut state_names = Vec::with_capacity(module.states.len());
    let mut state_index = HashMap::new();
    for (i, s) in module.states.iter().enumerate() {
        state_roots.push(crate::hashing::state_root(module_name, &s.name));
        state_types.push(s.ty.clone());
        state_defaults.push(default_for(&s.ty));
        state_names.push(s.name.clone());
        state_index.insert(s.name.clone(), i);
    }
    let mut struct_shapes = Vec::with_capacity(module.structs.len() + module.caps.len());
    let mut struct_index = HashMap::new();
    for (i, s) in module.structs.iter().enumerate() {
        struct_shapes.push(StructShape {
            name: s.name.clone(),
            field_names: s.fields.iter().map(|f| f.name.clone()).collect(),
        });
        struct_index.insert(s.name.clone(), i);
    }
    // Caps share runtime + bytecode representation with structs.
    // Register them in the same shape table so MakeStruct / FieldGet
    // / FieldSet handle caps without a separate instruction.
    for c in &module.caps {
        let i = struct_shapes.len();
        struct_shapes.push(StructShape {
            name: c.name.clone(),
            field_names: c.fields.iter().map(|f| f.name.clone()).collect(),
        });
        struct_index.insert(c.name.clone(), i);
    }
    let mut fn_index = HashMap::new();
    for (i, f) in module.functions.iter().enumerate() {
        fn_index.insert(f.name.clone(), i);
    }
    let mut path_specs: Vec<PathSpec> = Vec::new();
    let mut path_index: HashMap<(u16, Vec<String>), u16> = HashMap::new();
    let states_by_name: HashMap<String, Type> = module
        .states
        .iter()
        .map(|s| (s.name.clone(), s.ty.clone()))
        .collect();
    let summaries = crate::prefetch::summarize_fns(module, &states_by_name);
    let mut events = Vec::with_capacity(module.events.len());
    let mut event_index = HashMap::new();
    for (i, ev) in module.events.iter().enumerate() {
        events.push(crate::bc::EventShape {
            name: ev.name.clone(),
            param_names: ev.params.iter().map(|p| p.name.clone()).collect(),
        });
        event_index.insert(ev.name.clone(), i);
    }
    let mut enum_shapes = Vec::with_capacity(module.enums.len());
    let mut enum_index = HashMap::new();
    for (i, en) in module.enums.iter().enumerate() {
        enum_shapes.push(crate::bc::EnumShape {
            name: en.name.clone(),
            variant_names: en.variants.iter().map(|v| v.name.clone()).collect(),
        });
        enum_index.insert(en.name.clone(), i);
    }
    let consts_map: HashMap<String, Expr> = module
        .consts
        .iter()
        .map(|c| (c.name.clone(), c.value.clone()))
        .collect();
    // Local interface names plus any from dep artifacts. A tx
    // compiled against a deployed program needs the dep's iface
    // names here so `Foo::bind(...)` lowers to MakeInterface
    // rather than a CallExternal that would later fail validation.
    let mut iface_index: std::collections::HashSet<String> = module
        .interfaces
        .iter()
        .map(|i| i.name.clone())
        .collect();
    for n in iface_externals { iface_index.insert(n.clone()); }
    // Indexes keyed by the primary state they cover. At each
    // `primary[k] = v` write, the compiler emits maintenance writes
    // for every index in the matching list.
    let mut indexes_by_primary: HashMap<String, Vec<crate::ast::IndexDecl>> = HashMap::new();
    for idx in &module.indexes {
        indexes_by_primary
            .entry(idx.on_state.clone())
            .or_default()
            .push(idx.clone());
    }
    let mut bc_fns = Vec::with_capacity(module.functions.len());
    for f in &module.functions {
        bc_fns.push(compile_fn(
            f,
            &fn_index,
            &import_index,
            &state_index,
            &state_types,
            &state_roots,
            &struct_index,
            &struct_shapes,
            &mut path_specs,
            &mut path_index,
            &states_by_name,
            &summaries,
            &event_index,
            &enum_index,
            &enum_shapes,
            &iface_index,
            &consts_map,
            extern_fn_flags,
            &indexes_by_primary,
        )?);
    }
    let interfaces = module.interfaces.clone();
    let interface_index = interfaces
        .iter()
        .enumerate()
        .map(|(i, d)| (d.name.clone(), i))
        .collect();
    Ok(BcModule {
        name: module_name.to_string(),
        imports,
        import_index,
        state_roots,
        state_types,
        state_defaults,
        state_names,
        state_index,
        struct_shapes,
        struct_index,
        functions: bc_fns,
        fn_index,
        path_specs,
        events,
        event_index,
        enum_shapes,
        enum_index,
        interfaces,
        interface_index,
    })
}

fn default_for(ty: &Type) -> Value {
    Value::default_for(ty)
}

fn compile_fn<'a>(
    f: &FnDef,
    fn_index: &'a HashMap<String, usize>,
    import_index: &'a HashMap<String, usize>,
    state_index: &'a HashMap<String, usize>,
    state_types: &'a [Type],
    state_roots: &'a [u128],
    struct_index: &'a HashMap<String, usize>,
    struct_shapes: &'a [StructShape],
    path_specs: &'a mut Vec<PathSpec>,
    path_index: &'a mut HashMap<(u16, Vec<String>), u16>,
    states_by_name: &'a HashMap<String, Type>,
    summaries: &'a FnSummaries,
    event_index: &'a HashMap<String, usize>,
    enum_index: &'a HashMap<String, usize>,
    enum_shapes: &'a [crate::bc::EnumShape],
    iface_index: &'a std::collections::HashSet<String>,
    module_consts: &'a HashMap<String, Expr>,
    extern_fn_flags: &'a HashMap<(String, String), (bool, bool)>,
    indexes_by_primary: &'a HashMap<String, Vec<crate::ast::IndexDecl>>,
) -> Result<BcFn, Error> {
    let n_params = f.params.len() as u16;
    let mut comp = FnCompiler {
        fn_index,
        import_index,
        state_index,
        state_types,
        state_roots,
        struct_index,
        struct_shapes,
        path_specs,
        path_index,
        states_by_name,
        summaries,
        event_index,
        enum_index,
        enum_shapes,
        iface_index,
        module_consts,
        extern_fn_flags,
        indexes_by_primary,
        code: Vec::new(),
        consts: Vec::new(),
        next_reg: n_params,
        max_reg: n_params,
        scopes: vec![HashMap::new()],
        pipe_stack: Vec::new(),
        loop_stack: Vec::new(),
    };
    for (i, p) in f.params.iter().enumerate() {
        comp.scopes.last_mut().unwrap().insert(p.name.clone(), i as u16);
    }
    comp.compile_fn_body(&f.body)?;
    if !matches!(comp.code.last(), Some(Instr::Return { .. } | Instr::ReturnUnit)) {
        comp.code.push(Instr::ReturnUnit);
    }
    Ok(BcFn {
        name: f.name.clone(),
        n_params,
        n_regs: comp.max_reg,
        code: comp.code,
        consts: comp.consts,
        is_entry: f.is_entry,
        is_nore: f.is_nore,
        read_groups: Vec::new(),
        param_types: f.params.iter().map(|p| p.ty.clone()).collect(),
        return_type: f.return_type.clone(),
        is_view: f.is_view,
        is_pure: f.is_pure,
    })
}

struct FnCompiler<'a> {
    fn_index: &'a HashMap<String, usize>,
    import_index: &'a HashMap<String, usize>,
    state_index: &'a HashMap<String, usize>,
    state_types: &'a [Type],
    state_roots: &'a [u128],
    struct_index: &'a HashMap<String, usize>,
    struct_shapes: &'a [StructShape],
    path_specs: &'a mut Vec<PathSpec>,
    path_index: &'a mut HashMap<(u16, Vec<String>), u16>,
    states_by_name: &'a HashMap<String, Type>,
    summaries: &'a FnSummaries,
    event_index: &'a HashMap<String, usize>,
    enum_index: &'a HashMap<String, usize>,
    enum_shapes: &'a [crate::bc::EnumShape],
    /// Set of interface names declared in the module — used to
    /// recognize `IFace::bind(...)` and emit `MakeInterface`
    /// instead of a generic cross-module call.
    iface_index: &'a std::collections::HashSet<String>,
    /// `const NAME = expr;` map. When an Ident reference doesn't
    /// resolve to a local or a state, we look up the const and
    /// inline its RHS expression at the use site.
    module_consts: &'a HashMap<String, Expr>,
    /// Cross-module callee effect flags, keyed by `(module, fn)`.
    /// Stamped onto every emitted `Instr::CallExternal` so the
    /// optimizer can decide whether the call breaks a read cluster.
    /// Missing entries default to `(false, false)` — the optimizer
    /// then fences the cluster, matching the pre-flag behavior.
    extern_fn_flags: &'a HashMap<(String, String), (bool, bool)>,
    /// Indexes covering each primary state, keyed by primary name.
    /// At every `primary[k] = v` write, we walk this list and emit
    /// auto-maintenance writes against each index pmap.
    indexes_by_primary: &'a HashMap<String, Vec<crate::ast::IndexDecl>>,
    code: Vec<Instr>,
    consts: Vec<Const>,
    next_reg: u16,
    max_reg: u16,
    scopes: Vec<HashMap<String, u16>>,
    /// Stack of registers holding `$$` for each enclosing pipe stage.
    pipe_stack: Vec<u16>,
    /// Stack of enclosing loops; pushed on while/for entry, popped on exit.
    /// Each frame collects placeholder Jump positions emitted by `break` and
    /// `continue` so the loop's compiler can backpatch them at the end.
    loop_stack: Vec<LoopFrame>,
}

struct LoopFrame {
    break_jumps: Vec<usize>,
    continue_jumps: Vec<usize>,
}

enum CompLeaf<'a> {
    ListAppend { mapper: &'a Expr, dst: u16 },
    SetInsert { mapper: &'a Expr, dst: u16 },
    DictSet { key: &'a Expr, value: &'a Expr, dst: u16 },
}

fn leaf_expr_refs<'a>(leaf: &CompLeaf<'a>) -> Vec<&'a Expr> {
    match leaf {
        CompLeaf::ListAppend { mapper, .. } | CompLeaf::SetInsert { mapper, .. } => {
            vec![*mapper]
        }
        CompLeaf::DictSet { key, value, .. } => vec![*key, *value],
    }
}

impl<'a> FnCompiler<'a> {
    fn alloc(&mut self) -> u16 {
        let r = self.next_reg;
        self.next_reg += 1;
        if self.next_reg > self.max_reg {
            self.max_reg = self.next_reg;
        }
        r
    }

    fn free(&mut self, r: u16) {
        if r + 1 == self.next_reg {
            self.next_reg -= 1;
        }
    }

    fn const_idx(&mut self, c: Const) -> u16 {
        for (i, existing) in self.consts.iter().enumerate() {
            if existing == &c {
                return i as u16;
            }
        }
        let i = self.consts.len() as u16;
        self.consts.push(c);
        i
    }

    /// If `expr` is a chain of `Field` ending at an unshadowed state Ident,
    /// returns the state index and the ordered path of field names.
    fn try_state_field_path(&self, expr: &Expr) -> Option<(u16, Vec<String>)> {
        let mut names = Vec::new();
        let mut cur = expr;
        loop {
            match &cur.kind {
                ExprKind::Field { target, name } => {
                    names.push(name.clone());
                    cur = target;
                }
                ExprKind::Ident(name) => {
                    if self.lookup_local(name).is_some() {
                        return None;
                    }
                    let idx = *self.state_index.get(name)?;
                    names.reverse();
                    return Some((idx as u16, names));
                }
                _ => return None,
            }
        }
    }

    /// Intern a `PathSpec` for `(state_idx, path)`, computing the leaf KV
    /// key by chaining `child(parent_key, field_name)` and the leaf type by
    /// walking the state's struct shape. Caller must ensure the path is
    /// valid (typeck guarantees this for paths produced from typed code).
    fn intern_path(&mut self, state_idx: u16, path: Vec<String>) -> u16 {
        if let Some(&idx) = self.path_index.get(&(state_idx, path.clone())) {
            return idx;
        }
        let mut key = self.state_roots[state_idx as usize];
        let mut ty = self.state_types[state_idx as usize].clone();
        for name in &path {
            key = crate::hashing::child(key, name.as_bytes());
            ty = match ty {
                Type::Struct { fields, .. } => fields
                    .into_iter()
                    .find(|(n, _)| n == name)
                    .map(|(_, t)| t)
                    .expect("path field exists by typeck"),
                _ => panic!("state field path on non-struct type"),
            };
        }
        let idx = self.path_specs.len() as u16;
        self.path_specs.push(PathSpec { leaf_key: key, leaf_type: ty });
        self.path_index.insert((state_idx, path), idx);
        idx
    }

    fn lookup_local(&self, name: &str) -> Option<u16> {
        for scope in self.scopes.iter().rev() {
            if let Some(r) = scope.get(name) {
                return Some(*r);
            }
        }
        None
    }

    fn compile_block(&mut self, block: &Block) -> Result<(), Error> {
        self.scopes.push(HashMap::new());
        let saved = self.next_reg;
        for stmt in &block.stmts {
            self.compile_stmt(stmt)?;
        }
        // Tail expression on a *non-fn-body* block: evaluate for
        // side-effects only — the value is discarded since this
        // block sits inside a statement context (if/else arm,
        // while/for body). The fn-body path is handled by
        // `compile_fn_body` below.
        if let Some(t) = &block.tail {
            let r = self.alloc();
            self.compile_expr_into(t, r)?;
            self.free(r);
        }
        self.scopes.pop();
        self.next_reg = saved;
        Ok(())
    }

    /// Compile the fn body — same as `compile_block` except a tail
    /// expression is treated as the implicit return value.
    fn compile_fn_body(&mut self, block: &Block) -> Result<(), Error> {
        self.scopes.push(HashMap::new());
        let saved = self.next_reg;
        for stmt in &block.stmts {
            self.compile_stmt(stmt)?;
        }
        if let Some(t) = &block.tail {
            let r = self.alloc();
            self.compile_expr_into(t, r)?;
            self.code.push(Instr::Return { src: r });
            self.free(r);
        }
        self.scopes.pop();
        self.next_reg = saved;
        Ok(())
    }

    /// Compile a block as an expression that yields its tail value
    /// (or Unit) into `dst`. Used by `ExprKind::If` arms.
    fn compile_block_into(&mut self, block: &Block, dst: u16) -> Result<(), Error> {
        self.scopes.push(HashMap::new());
        let saved = self.next_reg;
        for stmt in &block.stmts {
            self.compile_stmt(stmt)?;
        }
        if let Some(t) = &block.tail {
            self.compile_expr_into(t, dst)?;
        } else {
            let idx = self.const_idx(Const::Bool(false));
            self.code.push(Instr::LoadConst { dst, idx });
        }
        self.scopes.pop();
        self.next_reg = saved;
        Ok(())
    }

    fn compile_stmt(&mut self, stmt: &Stmt) -> Result<(), Error> {
        match stmt {
            Stmt::Let { name, ty: _, value, .. } => {
                let r = self.alloc();
                self.compile_expr_into(value, r)?;
                self.scopes.last_mut().unwrap().insert(name.clone(), r);
                Ok(())
            }
            Stmt::Assign { target, value, .. } => {
                let val_reg = self.alloc();
                self.compile_expr_into(value, val_reg)?;
                self.assign_path(target, val_reg)?;
                self.free(val_reg);
                Ok(())
            }
            Stmt::Return { value, .. } => {
                if let Some(e) = value {
                    let r = self.alloc();
                    self.compile_expr_into(e, r)?;
                    self.code.push(Instr::Return { src: r });
                    self.free(r);
                } else {
                    self.code.push(Instr::ReturnUnit);
                }
                Ok(())
            }
            Stmt::If(if_stmt) => self.compile_if(if_stmt),
            Stmt::While { cond, body, .. } => self.compile_while(cond, body),
            Stmt::For { var, iter, body, .. } => self.compile_for(var, iter, body),
            Stmt::ForRange { var, start, end, inclusive, body, .. } => {
                self.compile_for_range(var, start, end, *inclusive, body)
            }
            Stmt::Placeholder(_) => {
                // Modifier expansion erases all placeholders before
                // compile sees the AST. Reaching this branch means
                // expand() didn't run or a stray `_;` slipped past
                // the typeck check.
                unreachable!("modifier expansion should have removed all `_;`")
            }
            Stmt::Break(span) => {
                if self.loop_stack.is_empty() {
                    return Err(Error::new(
                        ErrorKind::Type,
                        "'break' outside of a loop",
                        *span,
                    ));
                }
                let pos = self.code.len();
                self.code.push(Instr::Jump { offset: 0 });
                self.loop_stack.last_mut().unwrap().break_jumps.push(pos);
                Ok(())
            }
            Stmt::Continue(span) => {
                if self.loop_stack.is_empty() {
                    return Err(Error::new(
                        ErrorKind::Type,
                        "'continue' outside of a loop",
                        *span,
                    ));
                }
                let pos = self.code.len();
                self.code.push(Instr::Jump { offset: 0 });
                self.loop_stack.last_mut().unwrap().continue_jumps.push(pos);
                Ok(())
            }
            Stmt::Expr(e) => {
                let r = self.alloc();
                self.compile_expr_into(e, r)?;
                self.free(r);
                Ok(())
            }
            Stmt::Delete { target, span } => {
                self.compile_delete(target, *span)
            }
            Stmt::Emit { name, args, span } => {
                let event_idx = *self.event_index.get(name).ok_or_else(|| {
                    Error::new(
                        ErrorKind::Type,
                        format!("unknown event '{name}'"),
                        *span,
                    )
                })?;
                let args_start = self.next_reg;
                for _ in 0..args.len() { self.alloc(); }
                for (i, a) in args.iter().enumerate() {
                    self.compile_expr_into(a, args_start + i as u16)?;
                }
                self.code.push(Instr::Emit {
                    event_idx: event_idx as u16,
                    args_start,
                    n_args: args.len() as u8,
                });
                for _ in 0..args.len() { self.next_reg -= 1; }
                Ok(())
            }
            Stmt::LetTuple { names, value, .. } => {
                // Evaluate the tuple value into a stable register,
                // then bind each `names[i]` to a fresh register
                // initialized via `TupleGet`.
                let tuple_reg = self.alloc();
                self.compile_expr_into(value, tuple_reg)?;
                for (i, name) in names.iter().enumerate() {
                    let r = self.alloc();
                    self.code.push(Instr::TupleGet {
                        dst: r,
                        src: tuple_reg,
                        index: i as u16,
                    });
                    self.scopes.last_mut().unwrap().insert(name.clone(), r);
                }
                Ok(())
            }
        }
    }

    fn compile_while(&mut self, cond: &Expr, body: &Block) -> Result<(), Error> {
        let loop_start = self.code.len();
        let cond_reg = self.alloc();
        self.compile_expr_into(cond, cond_reg)?;
        let jif_pos = self.code.len();
        self.code.push(Instr::JumpIfFalse { cond: cond_reg, offset: 0 });
        self.free(cond_reg);

        self.loop_stack.push(LoopFrame {
            break_jumps: Vec::new(),
            continue_jumps: Vec::new(),
        });
        self.compile_block(body)?;
        let frame = self.loop_stack.pop().unwrap();

        // continue → loop_start
        let jmp_pos = self.code.len();
        let back_offset = (loop_start as i32) - (jmp_pos as i32) - 1;
        self.code.push(Instr::Jump { offset: back_offset });
        for pos in &frame.continue_jumps {
            let off = (loop_start as i32) - (*pos as i32) - 1;
            if let Instr::Jump { offset } = &mut self.code[*pos] { *offset = off; }
        }

        // patch JumpIfFalse and break jumps
        let end_pos = self.code.len();
        if let Instr::JumpIfFalse { offset, .. } = &mut self.code[jif_pos] {
            *offset = (end_pos as i32) - (jif_pos as i32) - 1;
        }
        for pos in &frame.break_jumps {
            let off = (end_pos as i32) - (*pos as i32) - 1;
            if let Instr::Jump { offset } = &mut self.code[*pos] { *offset = off; }
        }
        Ok(())
    }

    /// Recursively lower an assignment target. For an Ident, writes
    /// `value_reg` into the local register or via KvPut. For an Index of a
    /// map state, emits a MapPut. For a Field path (`obj.q.x = ...`), reads
    /// the parent struct into a temp, FieldSets the field, then recursively
    /// assigns the temp back to the parent — repeating up the chain.
    fn assign_path(&mut self, target: &Expr, value_reg: u16) -> Result<(), Error> {
        match &target.kind {
            ExprKind::Ident(name) => {
                if let Some(local_reg) = self.lookup_local(name) {
                    if value_reg != local_reg {
                        self.code.push(Instr::Move { dst: local_reg, src: value_reg });
                    }
                    return Ok(());
                }
                let idx = *self.state_index.get(name).ok_or_else(|| {
                    Error::new(
                        ErrorKind::Type,
                        format!("'{name}' is not assignable"),
                        target.span,
                    )
                })?;
                self.code.push(Instr::KvPut { src: value_reg, key_idx: idx as u16 });
                Ok(())
            }
            ExprKind::Index { target: t, key } => {
                let ExprKind::Ident(state_name) = &t.kind else {
                    return Err(Error::new(
                        ErrorKind::Type,
                        "indexed assignment target must be a state name",
                        target.span,
                    ));
                };
                let state_idx = *self.state_index.get(state_name).ok_or_else(|| {
                    Error::new(
                        ErrorKind::Type,
                        format!("'{state_name}' is not a state"),
                        t.span,
                    )
                })?;
                let is_pmap = matches!(self.state_types[state_idx], Type::PMap { .. });
                let is_pbtree = matches!(self.state_types[state_idx], Type::PBTree { .. });
                let is_pvec = matches!(self.state_types[state_idx], Type::PVec { .. });
                let is_map  = matches!(self.state_types[state_idx], Type::Map { .. });
                if !is_map && !is_pmap && !is_pbtree && !is_pvec {
                    return Err(Error::new(
                        ErrorKind::Type,
                        format!(
                            "'{state_name}' is not indexable; only map/pmap/pvec cells can be assigned by index",
                        ),
                        t.span,
                    ));
                }
                let key_reg = self.alloc();
                self.compile_expr_into(key, key_reg)?;
                self.code.push(if is_pmap {
                    Instr::PMapPut { state_idx: state_idx as u16, key_reg, src: value_reg }
                } else if is_pbtree {
                    Instr::PBTreePut { state_idx: state_idx as u16, key_reg, src: value_reg }
                } else if is_pvec {
                    Instr::PVecSet { state_idx: state_idx as u16, idx_reg: key_reg, src: value_reg }
                } else {
                    Instr::MapPut { state_idx: state_idx as u16, key_reg, src: value_reg }
                });
                // Index auto-maintenance: for every index covering
                // this primary state, project the value's indexed
                // field and write `index[v.field] = key`.
                if is_pmap {
                    if let Some(idxs) = self.indexes_by_primary.get(state_name).cloned() {
                        for idx in idxs {
                            // Walk the projection path; `cur` always
                            // holds the current accessor's register.
                            let mut cur = value_reg;
                            // Track which register we own so we don't
                            // accidentally free the caller's value_reg.
                            let mut owned: Option<u16> = None;
                            for field in &idx.projection {
                                let name_idx = self.const_idx(Const::Str(field.clone()));
                                let next_reg = self.alloc();
                                self.code.push(Instr::FieldGet {
                                    dst: next_reg, src: cur, name_idx,
                                });
                                if let Some(prev) = owned.take() {
                                    self.free(prev);
                                }
                                cur = next_reg;
                                owned = Some(next_reg);
                            }
                            let index_state_idx = *self.state_index.get(&idx.name).expect(
                                "index slot validated by typeck",
                            );
                            // Dispatch on slot backend: pmap-backed
                            // (hash-ordered) vs pbtree-backed
                            // (key-sorted). Both validated by typeck.
                            let is_pbtree = matches!(
                                self.state_types[index_state_idx],
                                Type::PBTree { .. },
                            );
                            match (idx.kind, is_pbtree) {
                                (crate::ast::IndexKind::Unique, false) => {
                                    self.code.push(Instr::PMapPutUnique {
                                        state_idx: index_state_idx as u16,
                                        key_reg: cur,
                                        src: key_reg,
                                    });
                                }
                                (crate::ast::IndexKind::Unique, true) => {
                                    self.code.push(Instr::PBTreePutUnique {
                                        state_idx: index_state_idx as u16,
                                        key_reg: cur,
                                        src: key_reg,
                                    });
                                }
                                (crate::ast::IndexKind::Multi, false) => {
                                    self.code.push(Instr::PMapAppendUnique {
                                        state_idx: index_state_idx as u16,
                                        key_reg: cur,
                                        elem_reg: key_reg,
                                    });
                                }
                                (crate::ast::IndexKind::Multi, true) => {
                                    self.code.push(Instr::PBTreeAppendUnique {
                                        state_idx: index_state_idx as u16,
                                        key_reg: cur,
                                        elem_reg: key_reg,
                                    });
                                }
                            }
                            if let Some(reg) = owned {
                                self.free(reg);
                            }
                        }
                    }
                }
                self.free(key_reg);
                Ok(())
            }
            ExprKind::Field { target: inner, name } => {
                // Fast path: chain bottoms at a state ident — emit a single
                // granular cell write at the leaf, never read+rebuild the
                // parent struct.
                if let Some((state_idx, path)) = self.try_state_field_path(target) {
                    let path_idx = self.intern_path(state_idx, path);
                    self.code.push(Instr::KvPutPath { src: value_reg, path_idx });
                    return Ok(());
                }
                let tmp = self.alloc();
                self.compile_expr_into(inner, tmp)?;
                let name_idx = self.const_idx(Const::Str(name.clone()));
                self.code.push(Instr::FieldSet { dst: tmp, name_idx, val: value_reg });
                self.assign_path(inner, tmp)?;
                self.free(tmp);
                Ok(())
            }
            _ => Err(Error::new(
                ErrorKind::Type,
                "invalid assignment target",
                target.span,
            )),
        }
    }

    /// Recursively lower a comprehension's clause sequence into nested loops
    /// and conditional skips, calling the leaf emitter at the deepest point.
    fn lower_comp(
        &mut self,
        clauses: &[CompClause],
        i: usize,
        leaf: &CompLeaf,
    ) -> Result<(), Error> {
        if i == clauses.len() {
            return self.emit_comp_leaf(leaf);
        }
        match &clauses[i] {
            CompClause::For { var, iter } => {
                // Streaming sources: iterating directly over a
                // pmap/pvec state never materializes the source
                // array. Only the comprehension's *output* array
                // (built by the leaf emitter) materializes — and
                // even that grows incrementally with append.
                if let ExprKind::Ident(name) = &iter.kind {
                    if let Some(&state_idx) = self.state_index.get(name) {
                        match &self.state_types[state_idx] {
                            Type::PVec { .. } => {
                                return self.lower_comp_for_pvec(
                                    state_idx as u16, var, clauses, i, leaf,
                                );
                            }
                            Type::PMap { .. } => {
                                return self.lower_comp_for_pmap(
                                    state_idx as u16, var, clauses, i, leaf,
                                );
                            }
                            Type::PBTree { .. } => {
                                return self.lower_comp_for_pbtree(
                                    state_idx as u16, var, clauses, i, leaf,
                                );
                            }
                            _ => {}
                        }
                    }
                }
                // Fallback: materialize the iter into an array
                // (array literal, function call, local that holds
                // an array, etc.) then index-loop over it.
                let arr_reg = self.alloc();
                self.compile_expr_into(iter, arr_reg)?;
                // Prefetch state-map cells keyed by `var` whenever the
                // remaining clauses + leaf only touch them via this var.
                if i == 0 {
                    let leaf_exprs = leaf_expr_refs(leaf);
                    let targets = crate::prefetch::scan_comp_inner(
                        &clauses[i + 1..],
                        &leaf_exprs,
                        var,
                        self.states_by_name,
                        self.summaries,
                    );
                    for state_name in targets {
                        if let Some(&idx) = self.state_index.get(&state_name) {
                            self.code.push(Instr::PrefetchMap {
                                arr_reg,
                                state_idx: idx as u16,
                            });
                        }
                    }
                }
                let len_reg = self.alloc();
                self.code.push(Instr::BuiltinLen { dst: len_reg, src: arr_reg });
                let counter_reg = self.alloc();
                let zero_idx = self.const_idx(Const::Int(num_bigint::BigInt::from(0)));
                self.code.push(Instr::LoadConst { dst: counter_reg, idx: zero_idx });
                let one_reg = self.alloc();
                let one_idx = self.const_idx(Const::Int(num_bigint::BigInt::from(1)));
                self.code.push(Instr::LoadConst { dst: one_reg, idx: one_idx });
                let var_reg = self.alloc();
                self.scopes.push(HashMap::new());
                self.scopes.last_mut().unwrap().insert(var.clone(), var_reg);

                let loop_start = self.code.len();
                let check_reg = self.alloc();
                self.code.push(Instr::Bin {
                    op: BinOp::Lt, dst: check_reg, lhs: counter_reg, rhs: len_reg,
                });
                let jif_pos = self.code.len();
                self.code.push(Instr::JumpIfFalse { cond: check_reg, offset: 0 });
                self.free(check_reg);
                self.code.push(Instr::ArrayGet { dst: var_reg, arr: arr_reg, idx: counter_reg });

                self.lower_comp(clauses, i + 1, leaf)?;

                self.code.push(Instr::Bin {
                    op: BinOp::Add, dst: counter_reg, lhs: counter_reg, rhs: one_reg,
                });
                let jmp_pos = self.code.len();
                let back = (loop_start as i32) - (jmp_pos as i32) - 1;
                self.code.push(Instr::Jump { offset: back });

                let end_pos = self.code.len();
                if let Instr::JumpIfFalse { offset, .. } = &mut self.code[jif_pos] {
                    *offset = (end_pos as i32) - (jif_pos as i32) - 1;
                }
                self.scopes.pop();
                self.free(var_reg);
                self.free(one_reg);
                self.free(counter_reg);
                self.free(len_reg);
                self.free(arr_reg);
                Ok(())
            }
            CompClause::If(cond) => {
                let f_reg = self.alloc();
                self.compile_expr_into(cond, f_reg)?;
                let pos = self.code.len();
                self.code.push(Instr::JumpIfFalse { cond: f_reg, offset: 0 });
                self.free(f_reg);

                self.lower_comp(clauses, i + 1, leaf)?;

                let here = self.code.len();
                if let Instr::JumpIfFalse { offset, .. } = &mut self.code[pos] {
                    *offset = (here as i32) - (pos as i32) - 1;
                }
                Ok(())
            }
        }
    }

    /// Streaming comprehension clause for a `pvec` source. Same
    /// shape as the materialized fallback in `lower_comp`, but the
    /// element fetch is `PVecGet` against the state slot — no
    /// intermediate array.
    fn lower_comp_for_pvec(
        &mut self,
        state_idx: u16,
        var: &str,
        clauses: &[CompClause],
        i: usize,
        leaf: &CompLeaf,
    ) -> Result<(), Error> {
        let len_reg = self.alloc();
        self.code.push(Instr::PVecLen { dst: len_reg, state_idx });
        let counter_reg = self.alloc();
        let zero_idx = self.const_idx(Const::U64(0));
        self.code.push(Instr::LoadConst { dst: counter_reg, idx: zero_idx });
        let var_reg = self.alloc();
        self.scopes.push(HashMap::new());
        self.scopes.last_mut().unwrap().insert(var.to_string(), var_reg);

        let loop_start = self.code.len();
        let check_reg = self.alloc();
        self.code.push(Instr::Bin {
            op: BinOp::Lt, dst: check_reg, lhs: counter_reg, rhs: len_reg,
        });
        let jif_pos = self.code.len();
        self.code.push(Instr::JumpIfFalse { cond: check_reg, offset: 0 });
        self.free(check_reg);
        self.code.push(Instr::PVecGet {
            dst: var_reg, state_idx, idx_reg: counter_reg,
        });

        self.lower_comp(clauses, i + 1, leaf)?;

        self.code.push(Instr::IncReg { reg: counter_reg });
        let jmp_pos = self.code.len();
        let back = (loop_start as i32) - (jmp_pos as i32) - 1;
        self.code.push(Instr::Jump { offset: back });

        let end_pos = self.code.len();
        if let Instr::JumpIfFalse { offset, .. } = &mut self.code[jif_pos] {
            *offset = (end_pos as i32) - (jif_pos as i32) - 1;
        }
        self.scopes.pop();
        self.free(var_reg);
        self.free(counter_reg);
        self.free(len_reg);
        Ok(())
    }

    /// Streaming comprehension clause for a `pmap` source. Drives
    /// a `PMapWalkInit` / `PMapWalkNext` pair so cells are pulled
    /// lazily as inner clauses run. Same nesting / filter scoping
    /// as the array fallback.
    fn lower_comp_for_pmap(
        &mut self,
        state_idx: u16,
        var: &str,
        clauses: &[CompClause],
        i: usize,
        leaf: &CompLeaf,
    ) -> Result<(), Error> {
        let cursor_reg = self.alloc();
        self.code.push(Instr::PMapWalkInit { dst: cursor_reg, state_idx });
        let var_reg = self.alloc();
        self.scopes.push(HashMap::new());
        self.scopes.last_mut().unwrap().insert(var.to_string(), var_reg);

        let loop_start = self.code.len();
        let next_pos = self.code.len();
        self.code.push(Instr::PMapWalkNext {
            cursor_reg, value_reg: var_reg, end_offset: 0,
        });

        self.lower_comp(clauses, i + 1, leaf)?;

        let jmp_pos = self.code.len();
        let back = (loop_start as i32) - (jmp_pos as i32) - 1;
        self.code.push(Instr::Jump { offset: back });

        let end_pos = self.code.len();
        if let Instr::PMapWalkNext { end_offset, .. } = &mut self.code[next_pos] {
            *end_offset = (end_pos as i32) - (next_pos as i32) - 1;
        }
        self.scopes.pop();
        self.free(var_reg);
        self.free(cursor_reg);
        Ok(())
    }

    /// Streaming comprehension clause for a `pbtree` source. Yields
    /// values in sorted order; the comprehension's output array
    /// materializes incrementally via append.
    fn lower_comp_for_pbtree(
        &mut self,
        state_idx: u16,
        var: &str,
        clauses: &[CompClause],
        i: usize,
        leaf: &CompLeaf,
    ) -> Result<(), Error> {
        let cursor_reg = self.alloc();
        self.code.push(Instr::PBTreeWalkInit { dst: cursor_reg, state_idx });
        let var_reg = self.alloc();
        self.scopes.push(HashMap::new());
        self.scopes.last_mut().unwrap().insert(var.to_string(), var_reg);

        let loop_start = self.code.len();
        let next_pos = self.code.len();
        self.code.push(Instr::PBTreeWalkNext {
            cursor_reg, value_reg: var_reg, end_offset: 0,
        });

        self.lower_comp(clauses, i + 1, leaf)?;

        let jmp_pos = self.code.len();
        let back = (loop_start as i32) - (jmp_pos as i32) - 1;
        self.code.push(Instr::Jump { offset: back });

        let end_pos = self.code.len();
        if let Instr::PBTreeWalkNext { end_offset, .. } = &mut self.code[next_pos] {
            *end_offset = (end_pos as i32) - (next_pos as i32) - 1;
        }
        self.scopes.pop();
        self.free(var_reg);
        self.free(cursor_reg);
        Ok(())
    }

    fn emit_comp_leaf(&mut self, leaf: &CompLeaf) -> Result<(), Error> {
        match leaf {
            CompLeaf::ListAppend { mapper, dst } => {
                let elem_reg = self.alloc();
                self.compile_expr_into(mapper, elem_reg)?;
                self.code.push(Instr::ArrayAppend { dst: *dst, arr: *dst, elem: elem_reg });
                self.free(elem_reg);
                Ok(())
            }
            CompLeaf::SetInsert { mapper, dst } => {
                let elem_reg = self.alloc();
                self.compile_expr_into(mapper, elem_reg)?;
                let args_base = self.alloc();
                self.code.push(Instr::Move { dst: args_base, src: *dst });
                let args_elem = self.alloc();
                self.code.push(Instr::Move { dst: args_elem, src: elem_reg });
                let name_idx = self.const_idx(Const::Str("set_insert".to_string()));
                self.code.push(Instr::BuiltinCall {
                    dst: *dst, name_idx, args_start: args_base, n_args: 2,
                });
                self.free(args_elem);
                self.free(args_base);
                self.free(elem_reg);
                Ok(())
            }
            CompLeaf::DictSet { key, value, dst } => {
                let args_dict = self.alloc();
                self.code.push(Instr::Move { dst: args_dict, src: *dst });
                let args_key = self.alloc();
                self.compile_expr_into(key, args_key)?;
                let args_val = self.alloc();
                self.compile_expr_into(value, args_val)?;
                let name_idx = self.const_idx(Const::Str("dict_set".to_string()));
                self.code.push(Instr::BuiltinCall {
                    dst: *dst, name_idx, args_start: args_dict, n_args: 3,
                });
                self.free(args_val);
                self.free(args_key);
                self.free(args_dict);
                Ok(())
            }
        }
    }

    /// `for i in start..end { body }` — lowered to a counter loop
    /// without materializing an array. Inclusive ranges (`..=`) use
    /// LtEq instead of Lt for the loop guard.
    fn compile_for_range(
        &mut self,
        var: &str,
        start: &Expr,
        end: &Expr,
        inclusive: bool,
        body: &Block,
    ) -> Result<(), Error> {
        let counter = self.alloc();
        self.compile_expr_into(start, counter)?;
        let limit = self.alloc();
        self.compile_expr_into(end, limit)?;
        // The iter var is the counter — no separate ArrayGet needed.
        self.scopes.push(HashMap::new());
        self.scopes.last_mut().unwrap().insert(var.to_string(), counter);

        let loop_start = self.code.len();
        let check_reg = self.alloc();
        let cmp = if inclusive { BinOp::LtEq } else { BinOp::Lt };
        self.code.push(Instr::Bin {
            op: cmp, dst: check_reg, lhs: counter, rhs: limit,
        });
        let jif_pos = self.code.len();
        self.code.push(Instr::JumpIfFalse { cond: check_reg, offset: 0 });
        self.free(check_reg);

        self.loop_stack.push(LoopFrame {
            break_jumps: Vec::new(),
            continue_jumps: Vec::new(),
        });
        self.compile_block(body)?;
        let frame = self.loop_stack.pop().unwrap();

        // continue → increment
        let continue_target = self.code.len();
        for pos in &frame.continue_jumps {
            let off = (continue_target as i32) - (*pos as i32) - 1;
            if let Instr::Jump { offset } = &mut self.code[*pos] { *offset = off; }
        }
        // IncReg preserves the counter's int type, so `for i in 0u64..n`
        // increments by a u64 1 without a typed-`1` constant.
        self.code.push(Instr::IncReg { reg: counter });
        let jmp_pos = self.code.len();
        let back = (loop_start as i32) - (jmp_pos as i32) - 1;
        self.code.push(Instr::Jump { offset: back });

        let end_pos = self.code.len();
        if let Instr::JumpIfFalse { offset, .. } = &mut self.code[jif_pos] {
            *offset = (end_pos as i32) - (jif_pos as i32) - 1;
        }
        for pos in &frame.break_jumps {
            let off = (end_pos as i32) - (*pos as i32) - 1;
            if let Instr::Jump { offset } = &mut self.code[*pos] { *offset = off; }
        }
        self.scopes.pop();
        self.free(limit);
        self.free(counter);
        Ok(())
    }

    /// Materialize `iter` into a register holding a `Value::Array`.
    /// Kept around for callers that genuinely need the array form
    /// (none today — both for-loops and comprehensions stream
    /// pmap/pvec sources). Left in place so future array-shaped
    /// callers don't have to re-derive the dispatch.
    #[allow(dead_code)]
    fn compile_iter_into_array(&mut self, iter: &Expr, dst: u16) -> Result<(), Error> {
        if let ExprKind::Ident(name) = &iter.kind {
            if let Some(&state_idx) = self.state_index.get(name) {
                let state_idx16 = state_idx as u16;
                match &self.state_types[state_idx] {
                    Type::PMap { .. } => {
                        self.code.push(Instr::PMapValues {
                            dst, state_idx: state_idx16,
                        });
                        return Ok(());
                    }
                    Type::PVec { .. } => {
                        self.code.push(Instr::PVecToArray {
                            dst, state_idx: state_idx16,
                        });
                        return Ok(());
                    }
                    _ => {}
                }
            }
        }
        self.compile_expr_into(iter, dst)
    }

    /// Lower `delete state[key];` into a sequence:
    ///   1. Read prior value at `state[key]` into a register.
    ///   2. For each index covering this state, project the indexed
    ///      field out of the prior value and emit a back-link
    ///      removal op (`Remove*Unique` / `Remove*FromList`).
    ///   3. Emit the primary delete op.
    /// Done in this order so the prior value is available for
    /// projection. If the primary entry didn't exist, `prior_reg`
    /// holds the value-type's default and the cleanup ops no-op
    /// safely (RemoveUnique requires equality, RemoveFromList
    /// requires membership).
    fn compile_delete(&mut self, target: &Expr, span: crate::token::Span) -> Result<(), Error> {
        let ExprKind::Index { target: t, key } = &target.kind else {
            return Err(Error::new(
                ErrorKind::Type,
                "delete target must be `state[key]`".to_string(),
                span,
            ));
        };
        let ExprKind::Ident(state_name) = &t.kind else {
            return Err(Error::new(
                ErrorKind::Type,
                "delete target must be a state slot".to_string(),
                span,
            ));
        };
        let state_idx = *self.state_index.get(state_name).ok_or_else(|| {
            Error::new(
                ErrorKind::Type,
                format!("'{state_name}' is not a state"),
                span,
            )
        })?;
        let is_pbtree = matches!(self.state_types[state_idx], Type::PBTree { .. });
        let is_pmap = matches!(self.state_types[state_idx], Type::PMap { .. });
        if !is_pmap && !is_pbtree {
            return Err(Error::new(
                ErrorKind::Type,
                format!(
                    "delete: '{state_name}' must be a pmap or pbtree state",
                ),
                span,
            ));
        }
        let key_reg = self.alloc();
        self.compile_expr_into(key, key_reg)?;

        // Read prior value (default if missing) so index cleanup
        // can project the right field values.
        let prior_reg = self.alloc();
        if is_pbtree {
            self.code.push(Instr::PBTreeGet {
                dst: prior_reg, state_idx: state_idx as u16, key_reg,
            });
        } else {
            self.code.push(Instr::PMapGet {
                dst: prior_reg, state_idx: state_idx as u16, key_reg,
            });
        }

        // Walk indexes covering this state and emit cleanup ops.
        if let Some(idxs) = self.indexes_by_primary.get(state_name).cloned() {
            for idx in idxs {
                // Project the indexed field out of the prior value.
                let mut cur = prior_reg;
                let mut owned: Option<u16> = None;
                for field in &idx.projection {
                    let name_idx = self.const_idx(Const::Str(field.clone()));
                    let next_reg = self.alloc();
                    self.code.push(Instr::FieldGet {
                        dst: next_reg, src: cur, name_idx,
                    });
                    if let Some(prev) = owned.take() {
                        self.free(prev);
                    }
                    cur = next_reg;
                    owned = Some(next_reg);
                }
                let index_state_idx = *self.state_index.get(&idx.name).expect(
                    "index slot validated by typeck",
                );
                let index_is_pbtree = matches!(
                    self.state_types[index_state_idx],
                    Type::PBTree { .. },
                );
                match (idx.kind, index_is_pbtree) {
                    (crate::ast::IndexKind::Unique, false) => {
                        self.code.push(Instr::PMapRemoveUnique {
                            state_idx: index_state_idx as u16,
                            key_reg: cur,
                            expected_reg: key_reg,
                        });
                    }
                    (crate::ast::IndexKind::Unique, true) => {
                        self.code.push(Instr::PBTreeRemoveUnique {
                            state_idx: index_state_idx as u16,
                            key_reg: cur,
                            expected_reg: key_reg,
                        });
                    }
                    (crate::ast::IndexKind::Multi, false) => {
                        self.code.push(Instr::PMapRemoveFromList {
                            state_idx: index_state_idx as u16,
                            key_reg: cur,
                            elem_reg: key_reg,
                        });
                    }
                    (crate::ast::IndexKind::Multi, true) => {
                        self.code.push(Instr::PBTreeRemoveFromList {
                            state_idx: index_state_idx as u16,
                            key_reg: cur,
                            elem_reg: key_reg,
                        });
                    }
                }
                if let Some(reg) = owned {
                    self.free(reg);
                }
            }
        }

        // Finally, remove the entry from the primary state.
        if is_pbtree {
            self.code.push(Instr::PBTreeDelete {
                state_idx: state_idx as u16, key_reg,
            });
        } else {
            self.code.push(Instr::PMapDelete {
                state_idx: state_idx as u16, key_reg,
            });
        }
        self.free(prior_reg);
        self.free(key_reg);
        Ok(())
    }

    fn compile_for(&mut self, var: &str, iter: &Expr, body: &Block) -> Result<(), Error> {
        // Streaming fast paths: iterating directly over a pmap or
        // pvec state never materializes the full collection.
        // The compiler emits a tight loop driven by a cursor /
        // index, fetching one cell at a time. `break` exits without
        // touching unread cells; matters at any scale where the
        // collection is bigger than the result set.
        if let ExprKind::Ident(name) = &iter.kind {
            if let Some(&state_idx) = self.state_index.get(name) {
                match &self.state_types[state_idx] {
                    Type::PVec { .. } => {
                        return self.compile_for_streaming_pvec(state_idx as u16, var, body);
                    }
                    Type::PMap { .. } => {
                        return self.compile_for_streaming_pmap(state_idx as u16, var, body);
                    }
                    Type::PBTree { .. } => {
                        return self.compile_for_streaming_pbtree(state_idx as u16, var, body);
                    }
                    _ => {}
                }
            }
        }
        // Fallback: materialize iter into an array, then index-loop.
        let arr_reg = self.alloc();
        self.compile_expr_into(iter, arr_reg)?;
        // Loop-prefetch hint: if the body's only state-map reads are
        // keyed by `var`, warm the tx read-cache with one round-trip
        // covering every cell the loop will visit.
        let targets = crate::prefetch::scan_loop_body(
            body, var, self.states_by_name, self.summaries,
        );
        for state_name in targets {
            if let Some(&idx) = self.state_index.get(&state_name) {
                self.code.push(Instr::PrefetchMap {
                    arr_reg,
                    state_idx: idx as u16,
                });
            }
        }
        let len_reg = self.alloc();
        self.code.push(Instr::BuiltinLen { dst: len_reg, src: arr_reg });
        let counter_reg = self.alloc();
        let zero_idx = self.const_idx(Const::Int(num_bigint::BigInt::from(0)));
        self.code.push(Instr::LoadConst { dst: counter_reg, idx: zero_idx });
        let one_reg = self.alloc();
        let one_idx = self.const_idx(Const::Int(num_bigint::BigInt::from(1)));
        self.code.push(Instr::LoadConst { dst: one_reg, idx: one_idx });
        let var_reg = self.alloc();
        self.scopes.push(HashMap::new());
        self.scopes.last_mut().unwrap().insert(var.to_string(), var_reg);

        let loop_start = self.code.len();
        let check_reg = self.alloc();
        self.code.push(Instr::Bin {
            op: BinOp::Lt, dst: check_reg, lhs: counter_reg, rhs: len_reg,
        });
        let jif_pos = self.code.len();
        self.code.push(Instr::JumpIfFalse { cond: check_reg, offset: 0 });
        self.free(check_reg);
        self.code.push(Instr::ArrayGet { dst: var_reg, arr: arr_reg, idx: counter_reg });

        self.loop_stack.push(LoopFrame {
            break_jumps: Vec::new(),
            continue_jumps: Vec::new(),
        });
        self.compile_block(body)?;
        let frame = self.loop_stack.pop().unwrap();

        // continue target = increment step
        let continue_target = self.code.len();
        for pos in &frame.continue_jumps {
            let off = (continue_target as i32) - (*pos as i32) - 1;
            if let Instr::Jump { offset } = &mut self.code[*pos] { *offset = off; }
        }
        self.code.push(Instr::Bin {
            op: BinOp::Add, dst: counter_reg, lhs: counter_reg, rhs: one_reg,
        });
        let jmp_pos = self.code.len();
        let back_offset = (loop_start as i32) - (jmp_pos as i32) - 1;
        self.code.push(Instr::Jump { offset: back_offset });

        let end_pos = self.code.len();
        if let Instr::JumpIfFalse { offset, .. } = &mut self.code[jif_pos] {
            *offset = (end_pos as i32) - (jif_pos as i32) - 1;
        }
        for pos in &frame.break_jumps {
            let off = (end_pos as i32) - (*pos as i32) - 1;
            if let Instr::Jump { offset } = &mut self.code[*pos] { *offset = off; }
        }
        self.scopes.pop();
        self.free(var_reg);
        self.free(one_reg);
        self.free(counter_reg);
        self.free(len_reg);
        self.free(arr_reg);
        Ok(())
    }

    /// Streaming for-loop over a `pvec` state. Uses `pvec_len` plus
    /// indexed reads — never materializes the full vector. `break`
    /// exits without touching unread cells.
    fn compile_for_streaming_pvec(
        &mut self,
        state_idx: u16,
        var: &str,
        body: &Block,
    ) -> Result<(), Error> {
        let len_reg = self.alloc();
        self.code.push(Instr::PVecLen { dst: len_reg, state_idx });
        let counter_reg = self.alloc();
        // pvec_len returns u64 — match counter type so `<` and IncReg
        // operate on uniform integers.
        let zero_idx = self.const_idx(Const::U64(0));
        self.code.push(Instr::LoadConst { dst: counter_reg, idx: zero_idx });
        let var_reg = self.alloc();
        self.scopes.push(HashMap::new());
        self.scopes.last_mut().unwrap().insert(var.to_string(), var_reg);

        let loop_start = self.code.len();
        let check_reg = self.alloc();
        self.code.push(Instr::Bin {
            op: BinOp::Lt, dst: check_reg, lhs: counter_reg, rhs: len_reg,
        });
        let jif_pos = self.code.len();
        self.code.push(Instr::JumpIfFalse { cond: check_reg, offset: 0 });
        self.free(check_reg);
        self.code.push(Instr::PVecGet {
            dst: var_reg, state_idx, idx_reg: counter_reg,
        });

        self.loop_stack.push(LoopFrame {
            break_jumps: Vec::new(),
            continue_jumps: Vec::new(),
        });
        self.compile_block(body)?;
        let frame = self.loop_stack.pop().unwrap();

        let continue_target = self.code.len();
        for pos in &frame.continue_jumps {
            let off = (continue_target as i32) - (*pos as i32) - 1;
            if let Instr::Jump { offset } = &mut self.code[*pos] { *offset = off; }
        }
        self.code.push(Instr::IncReg { reg: counter_reg });
        let jmp_pos = self.code.len();
        let back_offset = (loop_start as i32) - (jmp_pos as i32) - 1;
        self.code.push(Instr::Jump { offset: back_offset });

        let end_pos = self.code.len();
        if let Instr::JumpIfFalse { offset, .. } = &mut self.code[jif_pos] {
            *offset = (end_pos as i32) - (jif_pos as i32) - 1;
        }
        for pos in &frame.break_jumps {
            let off = (end_pos as i32) - (*pos as i32) - 1;
            if let Instr::Jump { offset } = &mut self.code[*pos] { *offset = off; }
        }
        self.scopes.pop();
        self.free(var_reg);
        self.free(counter_reg);
        self.free(len_reg);
        Ok(())
    }

    /// Streaming for-loop over a `pmap` state. Uses a HAMT cursor in
    /// a register; each `PMapWalkNext` advances by one leaf entry,
    /// pulling cells lazily as the walk descends. `break` exits
    /// without fetching unvisited subtrees.
    fn compile_for_streaming_pmap(
        &mut self,
        state_idx: u16,
        var: &str,
        body: &Block,
    ) -> Result<(), Error> {
        let cursor_reg = self.alloc();
        self.code.push(Instr::PMapWalkInit { dst: cursor_reg, state_idx });
        let var_reg = self.alloc();
        self.scopes.push(HashMap::new());
        self.scopes.last_mut().unwrap().insert(var.to_string(), var_reg);

        let loop_start = self.code.len();
        let next_pos = self.code.len();
        // PMapWalkNext jumps to end_offset when the walk is exhausted
        // (no more entries). Patched once we know end_pos.
        self.code.push(Instr::PMapWalkNext {
            cursor_reg, value_reg: var_reg, end_offset: 0,
        });

        self.loop_stack.push(LoopFrame {
            break_jumps: Vec::new(),
            continue_jumps: Vec::new(),
        });
        self.compile_block(body)?;
        let frame = self.loop_stack.pop().unwrap();

        let continue_target = self.code.len();
        for pos in &frame.continue_jumps {
            let off = (continue_target as i32) - (*pos as i32) - 1;
            if let Instr::Jump { offset } = &mut self.code[*pos] { *offset = off; }
        }
        let jmp_pos = self.code.len();
        let back_offset = (loop_start as i32) - (jmp_pos as i32) - 1;
        self.code.push(Instr::Jump { offset: back_offset });

        let end_pos = self.code.len();
        if let Instr::PMapWalkNext { end_offset, .. } = &mut self.code[next_pos] {
            *end_offset = (end_pos as i32) - (next_pos as i32) - 1;
        }
        for pos in &frame.break_jumps {
            let off = (end_pos as i32) - (*pos as i32) - 1;
            if let Instr::Jump { offset } = &mut self.code[*pos] { *offset = off; }
        }
        self.scopes.pop();
        self.free(var_reg);
        self.free(cursor_reg);
        Ok(())
    }

    /// Streaming for-loop over a `pbtree` state. Yields values in
    /// key-sorted order. Same shape as `compile_for_streaming_pmap`
    /// but routes through the pbtree cursor instructions.
    fn compile_for_streaming_pbtree(
        &mut self,
        state_idx: u16,
        var: &str,
        body: &Block,
    ) -> Result<(), Error> {
        let cursor_reg = self.alloc();
        self.code.push(Instr::PBTreeWalkInit { dst: cursor_reg, state_idx });
        let var_reg = self.alloc();
        self.scopes.push(HashMap::new());
        self.scopes.last_mut().unwrap().insert(var.to_string(), var_reg);

        let loop_start = self.code.len();
        let next_pos = self.code.len();
        self.code.push(Instr::PBTreeWalkNext {
            cursor_reg, value_reg: var_reg, end_offset: 0,
        });

        self.loop_stack.push(LoopFrame {
            break_jumps: Vec::new(),
            continue_jumps: Vec::new(),
        });
        self.compile_block(body)?;
        let frame = self.loop_stack.pop().unwrap();

        let continue_target = self.code.len();
        for pos in &frame.continue_jumps {
            let off = (continue_target as i32) - (*pos as i32) - 1;
            if let Instr::Jump { offset } = &mut self.code[*pos] { *offset = off; }
        }
        let jmp_pos = self.code.len();
        let back_offset = (loop_start as i32) - (jmp_pos as i32) - 1;
        self.code.push(Instr::Jump { offset: back_offset });

        let end_pos = self.code.len();
        if let Instr::PBTreeWalkNext { end_offset, .. } = &mut self.code[next_pos] {
            *end_offset = (end_pos as i32) - (next_pos as i32) - 1;
        }
        for pos in &frame.break_jumps {
            let off = (end_pos as i32) - (*pos as i32) - 1;
            if let Instr::Jump { offset } = &mut self.code[*pos] { *offset = off; }
        }
        self.scopes.pop();
        self.free(var_reg);
        self.free(cursor_reg);
        Ok(())
    }

    fn compile_if(&mut self, ifs: &IfStmt) -> Result<(), Error> {
        let cond_r = self.alloc();
        self.compile_expr_into(&ifs.cond, cond_r)?;
        let jif_pos = self.code.len();
        self.code.push(Instr::JumpIfFalse { cond: cond_r, offset: 0 });
        self.free(cond_r);

        self.compile_block(&ifs.then)?;
        let jmp_end_pos = self.code.len();
        self.code.push(Instr::Jump { offset: 0 });

        let else_pos = self.code.len();
        let off = (else_pos as i32) - (jif_pos as i32) - 1;
        if let Instr::JumpIfFalse { offset, .. } = &mut self.code[jif_pos] {
            *offset = off;
        }

        match &ifs.else_branch {
            ElseBranch::None => {}
            ElseBranch::Block(b) => self.compile_block(b)?,
            ElseBranch::If(inner) => self.compile_if(inner)?,
        }

        let end_pos = self.code.len();
        let off = (end_pos as i32) - (jmp_end_pos as i32) - 1;
        if let Instr::Jump { offset } = &mut self.code[jmp_end_pos] {
            *offset = off;
        }

        Ok(())
    }

    fn compile_expr_into(&mut self, expr: &Expr, dst: u16) -> Result<(), Error> {
        match &expr.kind {
            ExprKind::Int(n) => {
                let idx = self.const_idx(Const::Int(n.clone()));
                self.code.push(Instr::LoadConst { dst, idx });
                Ok(())
            }
            ExprKind::UInt(n) => {
                let idx = self.const_idx(Const::UInt(n.clone()));
                self.code.push(Instr::LoadConst { dst, idx });
                Ok(())
            }
            ExprKind::Float(n) => {
                let idx = self.const_idx(Const::Float(*n));
                self.code.push(Instr::LoadConst { dst, idx });
                Ok(())
            }
            ExprKind::I32(n) => {
                let idx = self.const_idx(Const::I32(*n));
                self.code.push(Instr::LoadConst { dst, idx });
                Ok(())
            }
            ExprKind::U32(n) => {
                let idx = self.const_idx(Const::U32(*n));
                self.code.push(Instr::LoadConst { dst, idx });
                Ok(())
            }
            ExprKind::U64(n) => {
                let idx = self.const_idx(Const::U64(*n));
                self.code.push(Instr::LoadConst { dst, idx });
                Ok(())
            }
            ExprKind::U128(n) => {
                let idx = self.const_idx(Const::U128(*n));
                self.code.push(Instr::LoadConst { dst, idx });
                Ok(())
            }
            ExprKind::Bool(b) => {
                let idx = self.const_idx(Const::Bool(*b));
                self.code.push(Instr::LoadConst { dst, idx });
                Ok(())
            }
            ExprKind::Str(s) => {
                let idx = self.const_idx(Const::Str(s.clone()));
                self.code.push(Instr::LoadConst { dst, idx });
                Ok(())
            }
            ExprKind::Ident(name) => {
                if let Some(reg) = self.lookup_local(name) {
                    if reg != dst {
                        self.code.push(Instr::Move { dst, src: reg });
                    }
                    return Ok(());
                }
                if let Some(&idx) = self.state_index.get(name) {
                    self.code.push(Instr::KvGet { dst, key_idx: idx as u16 });
                    return Ok(());
                }
                // Constants are inlined: compile the const's RHS at
                // each use site. Cloned to avoid borrowing self while
                // we walk into the expression.
                if let Some(value) = self.module_consts.get(name).cloned() {
                    return self.compile_expr_into(&value, dst);
                }
                Err(Error::new(
                    ErrorKind::Type,
                    format!("undefined variable '{name}'"),
                    expr.span,
                ))
            }
            ExprKind::Binary { op, lhs, rhs } => {
                self.compile_expr_into(lhs, dst)?;
                let rhs_r = self.alloc();
                self.compile_expr_into(rhs, rhs_r)?;
                self.code.push(Instr::Bin { op: *op, dst, lhs: dst, rhs: rhs_r });
                self.free(rhs_r);
                Ok(())
            }
            ExprKind::Unary { op, operand } => {
                self.compile_expr_into(operand, dst)?;
                self.code.push(Instr::Un { op: *op, dst, src: dst });
                Ok(())
            }
            ExprKind::Index { target, key } => {
                // Map / pmap / pvec state indexing — emit the right
                // instruction when the named state is one of them.
                // Array values fall through to ArrayGet.
                if let ExprKind::Ident(state_name) = &target.kind {
                    if let Some(&idx) = self.state_index.get(state_name) {
                        let is_map    = matches!(self.state_types[idx], Type::Map { .. });
                        let is_pmap   = matches!(self.state_types[idx], Type::PMap { .. });
                        let is_pbtree = matches!(self.state_types[idx], Type::PBTree { .. });
                        let is_pvec   = matches!(self.state_types[idx], Type::PVec { .. });
                        if is_map || is_pmap || is_pbtree || is_pvec {
                            let key_reg = self.alloc();
                            self.compile_expr_into(key, key_reg)?;
                            self.code.push(if is_pmap {
                                Instr::PMapGet { dst, state_idx: idx as u16, key_reg }
                            } else if is_pbtree {
                                Instr::PBTreeGet { dst, state_idx: idx as u16, key_reg }
                            } else if is_pvec {
                                Instr::PVecGet { dst, state_idx: idx as u16, idx_reg: key_reg }
                            } else {
                                Instr::MapGet { dst, state_idx: idx as u16, key_reg }
                            });
                            self.free(key_reg);
                            return Ok(());
                        }
                    }
                }
                // Array value indexing — compile target into a register, then ArrayGet.
                let arr_reg = self.alloc();
                self.compile_expr_into(target, arr_reg)?;
                let idx_reg = self.alloc();
                self.compile_expr_into(key, idx_reg)?;
                self.code.push(Instr::ArrayGet { dst, arr: arr_reg, idx: idx_reg });
                self.free(idx_reg);
                self.free(arr_reg);
                Ok(())
            }
            ExprKind::Array(elems) => {
                let n = elems.len() as u16;
                let args_start = self.next_reg;
                for _ in 0..elems.len() {
                    self.alloc();
                }
                for (i, e) in elems.iter().enumerate() {
                    self.compile_expr_into(e, args_start + i as u16)?;
                }
                self.code.push(Instr::MakeArray { dst, args_start, n });
                for _ in 0..elems.len() {
                    self.next_reg -= 1;
                }
                Ok(())
            }
            ExprKind::StructLit { name, fields } => {
                let shape_idx = *self.struct_index.get(name).ok_or_else(|| {
                    Error::new(
                        ErrorKind::Type,
                        format!("unknown struct '{name}'"),
                        expr.span,
                    )
                })?;
                let shape = &self.struct_shapes[shape_idx];
                let n = shape.field_names.len() as u16;
                let args_start = self.next_reg;
                for _ in 0..shape.field_names.len() {
                    self.alloc();
                }
                for (i, declared_name) in shape.field_names.iter().enumerate() {
                    let provided = fields
                        .iter()
                        .find(|(fn_, _)| fn_ == declared_name)
                        .ok_or_else(|| {
                            Error::new(
                                ErrorKind::Type,
                                format!("missing field '{declared_name}' in struct '{name}'"),
                                expr.span,
                            )
                        })?;
                    self.compile_expr_into(&provided.1, args_start + i as u16)?;
                }
                self.code.push(Instr::MakeStruct {
                    dst,
                    shape_idx: shape_idx as u16,
                    args_start,
                    n,
                });
                for _ in 0..shape.field_names.len() {
                    self.next_reg -= 1;
                }
                Ok(())
            }
            ExprKind::Field { target, name } => {
                if let Some((state_idx, path)) = self.try_state_field_path(expr) {
                    let path_idx = self.intern_path(state_idx, path);
                    self.code.push(Instr::KvGetPath { dst, path_idx });
                    return Ok(());
                }
                let src = self.alloc();
                self.compile_expr_into(target, src)?;
                let name_idx = self.const_idx(Const::Str(name.clone()));
                self.code.push(Instr::FieldGet { dst, src, name_idx });
                self.free(src);
                Ok(())
            }
            ExprKind::Prev => {
                let r = *self.pipe_stack.last().ok_or_else(|| Error::new(
                    ErrorKind::Type,
                    "$$ used outside of a pipe stage".to_string(),
                    expr.span,
                ))?;
                if r != dst {
                    self.code.push(Instr::Move { dst, src: r });
                }
                Ok(())
            }
            ExprKind::Pipe { head, step } => {
                // Compile head into a stable register, push it as the
                // current `$$`, compile step into `dst`, then pop.
                let stash = self.alloc();
                self.compile_expr_into(head, stash)?;
                self.pipe_stack.push(stash);
                self.compile_expr_into(step, dst)?;
                self.pipe_stack.pop();
                self.free(stash);
                Ok(())
            }
            ExprKind::SetLit(elems) => {
                let n = elems.len() as u16;
                let args_start = self.next_reg;
                for _ in 0..elems.len() { self.alloc(); }
                for (i, e) in elems.iter().enumerate() {
                    self.compile_expr_into(e, args_start + i as u16)?;
                }
                self.code.push(Instr::MakeSet { dst, args_start, n });
                for _ in 0..elems.len() { self.next_reg -= 1; }
                Ok(())
            }
            ExprKind::DictLit(pairs) => {
                let n = pairs.len() as u16;
                let args_start = self.next_reg;
                for _ in 0..(pairs.len() * 2) { self.alloc(); }
                for (i, (k, v)) in pairs.iter().enumerate() {
                    self.compile_expr_into(k, args_start + (i * 2) as u16)?;
                    self.compile_expr_into(v, args_start + (i * 2 + 1) as u16)?;
                }
                self.code.push(Instr::MakeDict { dst, args_start, n_pairs: n });
                for _ in 0..(pairs.len() * 2) { self.next_reg -= 1; }
                Ok(())
            }
            ExprKind::SetComp { mapper, clauses } => {
                self.code.push(Instr::MakeSet { dst, args_start: 0, n: 0 });
                self.lower_comp(clauses, 0, &CompLeaf::SetInsert { mapper, dst })?;
                Ok(())
            }
            ExprKind::DictComp { key, value, clauses } => {
                self.code.push(Instr::MakeDict { dst, args_start: 0, n_pairs: 0 });
                self.lower_comp(clauses, 0, &CompLeaf::DictSet { key, value, dst })?;
                Ok(())
            }
            ExprKind::ListComp { mapper, clauses } => {
                self.code.push(Instr::MakeArray { dst, args_start: 0, n: 0 });
                self.lower_comp(clauses, 0, &CompLeaf::ListAppend { mapper, dst })?;
                Ok(())
            }
            ExprKind::DynCall { target_ident, method, args, method_is_view, method_is_pure } => {
                // Load the target (an interface value, Copy) into
                // a register; the VM reads its `target_module`
                // field and dispatches via the world's module
                // index. The fn name is constant-pooled like a
                // regular cross-module call. The view/pure flags
                // were stamped onto this node by the
                // `typeck::annotate_dyn_calls` pass.
                let target_reg = self.alloc();
                let target_expr = Expr {
                    kind: ExprKind::Ident(target_ident.clone()),
                    span: expr.span,
                };
                self.compile_expr_into(&target_expr, target_reg)?;
                let args_start = self.next_reg;
                for _ in 0..args.len() { self.alloc(); }
                for (i, a) in args.iter().enumerate() {
                    self.compile_expr_into(a, args_start + i as u16)?;
                }
                let fn_name_idx = self.const_idx(Const::Str(method.clone()));
                self.code.push(Instr::CallExternalDyn {
                    dst,
                    target_reg,
                    fn_name_idx,
                    args_start,
                    n_args: args.len() as u8,
                    is_view: *method_is_view,
                    is_pure: *method_is_pure,
                });
                for _ in 0..args.len() { self.next_reg -= 1; }
                self.free(target_reg);
                return Ok(());
            }
            ExprKind::Call { module, name, args } => {
                if name == "resource" && args.len() == 1 {
                    let r = self.alloc();
                    self.compile_expr_into(&args[0], r)?;
                    self.code.push(Instr::BuiltinResource { dst, src: r });
                    self.free(r);
                    return Ok(());
                }
                if name == "unwrap" && args.len() == 1 {
                    let r = self.alloc();
                    self.compile_expr_into(&args[0], r)?;
                    self.code.push(Instr::BuiltinUnwrap { dst, src: r });
                    self.free(r);
                    return Ok(());
                }
                if name == "address" && args.len() == 1 {
                    let r = self.alloc();
                    self.compile_expr_into(&args[0], r)?;
                    self.code.push(Instr::BuiltinAddress { dst, src: r });
                    self.free(r);
                    return Ok(());
                }
                if name == "len" && args.len() == 1 {
                    let r = self.alloc();
                    self.compile_expr_into(&args[0], r)?;
                    self.code.push(Instr::BuiltinLen { dst, src: r });
                    self.free(r);
                    return Ok(());
                }
                // Set/dict polymorphic builtins + assert → generic
                // BuiltinCall. assert raises a runtime error on false;
                // success returns Unit.
                // pvec_push / pvec_len take a pvec state name first.
                // Like the pmap builtins they need direct Tx access.
                if matches!(name.as_str(), "pvec_push" | "pvec_len") {
                    let state_name = match args.first().map(|a| &a.kind) {
                        Some(ExprKind::Ident(n)) => n.clone(),
                        _ => return Err(Error::new(
                            ErrorKind::Type,
                            format!("{name}() arg 0 must name a pvec state"),
                            expr.span,
                        )),
                    };
                    let state_idx = *self.state_index.get(&state_name).ok_or_else(|| {
                        Error::new(
                            ErrorKind::Type,
                            format!("'{state_name}' is not a state"),
                            expr.span,
                        )
                    })?;
                    if name == "pvec_len" {
                        self.code.push(Instr::PVecLen { dst, state_idx: state_idx as u16 });
                    } else {
                        let val_reg = self.alloc();
                        self.compile_expr_into(&args[1], val_reg)?;
                        self.code.push(Instr::PVecPush {
                            dst,
                            state_idx: state_idx as u16,
                            src: val_reg,
                        });
                        self.free(val_reg);
                    }
                    return Ok(());
                }
                // pmap_contains takes a state-name first arg and
                // needs direct Tx access. Emit a dedicated
                // instruction rather than the generic BuiltinCall
                // (which goes through `ops::call_builtin` and so
                // can't see the Tx).
                if name == "pbtree_contains" {
                    let state_name = match args.first().map(|a| &a.kind) {
                        Some(ExprKind::Ident(n)) => n.clone(),
                        _ => return Err(Error::new(
                            ErrorKind::Type,
                            "pbtree_contains() arg 0 must name a pbtree state".to_string(),
                            expr.span,
                        )),
                    };
                    let state_idx = *self.state_index.get(&state_name).ok_or_else(|| {
                        Error::new(
                            ErrorKind::Type,
                            format!("'{state_name}' is not a state"),
                            expr.span,
                        )
                    })?;
                    let key_reg = self.alloc();
                    self.compile_expr_into(&args[1], key_reg)?;
                    self.code.push(Instr::PBTreeContains {
                        dst, state_idx: state_idx as u16, key_reg,
                    });
                    self.free(key_reg);
                    return Ok(());
                }
                if name == "pbtree_range" {
                    let state_name = match args.first().map(|a| &a.kind) {
                        Some(ExprKind::Ident(n)) => n.clone(),
                        _ => return Err(Error::new(
                            ErrorKind::Type,
                            "pbtree_range() arg 0 must name a pbtree state".to_string(),
                            expr.span,
                        )),
                    };
                    let state_idx = *self.state_index.get(&state_name).ok_or_else(|| {
                        Error::new(
                            ErrorKind::Type,
                            format!("'{state_name}' is not a state"),
                            expr.span,
                        )
                    })?;
                    let lo_reg = self.alloc();
                    self.compile_expr_into(&args[1], lo_reg)?;
                    let hi_reg = self.alloc();
                    self.compile_expr_into(&args[2], hi_reg)?;
                    self.code.push(Instr::PBTreeRange {
                        dst, state_idx: state_idx as u16, lo_reg, hi_reg,
                    });
                    self.free(hi_reg);
                    self.free(lo_reg);
                    return Ok(());
                }
                if name == "pmap_contains" {
                    let state_name = match args.first().map(|a| &a.kind) {
                        Some(ExprKind::Ident(n)) => n.clone(),
                        _ => return Err(Error::new(
                            ErrorKind::Type,
                            format!("{name}() arg 0 must name a pmap state"),
                            expr.span,
                        )),
                    };
                    let state_idx = *self.state_index.get(&state_name).ok_or_else(|| {
                        Error::new(
                            ErrorKind::Type,
                            format!("'{state_name}' is not a state"),
                            expr.span,
                        )
                    })?;
                    let key_reg = self.alloc();
                    self.compile_expr_into(&args[1], key_reg)?;
                    self.code.push(Instr::PMapContains {
                        dst,
                        state_idx: state_idx as u16,
                        key_reg,
                    });
                    self.free(key_reg);
                    return Ok(());
                }
                // pmap walk-the-whole-tree builtins. Each takes a single
                // state-name arg and returns a fresh `[T]`. They're
                // O(N) cell reads — see `pmap::entries` for the trade.
                if matches!(name.as_str(),
                    "pmap_entries" | "pmap_keys" | "pmap_values" | "pvec_to_array"
                ) {
                    let state_name = match args.first().map(|a| &a.kind) {
                        Some(ExprKind::Ident(n)) => n.clone(),
                        _ => return Err(Error::new(
                            ErrorKind::Type,
                            format!("{name}() arg 0 must name a state slot"),
                            expr.span,
                        )),
                    };
                    let state_idx = *self.state_index.get(&state_name).ok_or_else(|| {
                        Error::new(
                            ErrorKind::Type,
                            format!("'{state_name}' is not a state"),
                            expr.span,
                        )
                    })?;
                    let instr = match name.as_str() {
                        "pmap_entries"  => Instr::PMapEntries  { dst, state_idx: state_idx as u16 },
                        "pmap_keys"     => Instr::PMapKeys     { dst, state_idx: state_idx as u16 },
                        "pmap_values"   => Instr::PMapValues   { dst, state_idx: state_idx as u16 },
                        "pvec_to_array" => Instr::PVecToArray  { dst, state_idx: state_idx as u16 },
                        _ => unreachable!(),
                    };
                    self.code.push(instr);
                    return Ok(());
                }
                // User-defined functions named `sum`/`max`/`min` win
                // over the builtin (matches typeck's resolution).
                let aggregator_shadowed = matches!(name.as_str(), "sum" | "max" | "min")
                    && self.fn_index.contains_key(name);
                if !aggregator_shadowed && matches!(name.as_str(),
                    "set_insert" | "set_remove" | "set_contains" | "set_len"
                    | "dict_set" | "dict_remove" | "dict_get" | "dict_has" | "dict_len"
                    | "assert"
                    | "to_bytes" | "bytes_len" | "bytes_concat" | "bytes_eq" | "bytes_slice"
                    | "string_concat" | "string_slice" | "string_contains"
                    | "sum" | "max" | "min"
                    | "parse_json" | "json_stringify"
                    | "json_get_field" | "json_get_index"
                    | "json_to_string" | "json_to_i64" | "json_to_u64"
                    | "json_to_bool" | "json_is_null"
                ) {
                    let args_start = self.next_reg;
                    for _ in 0..args.len() { self.alloc(); }
                    for (i, a) in args.iter().enumerate() {
                        self.compile_expr_into(a, args_start + i as u16)?;
                    }
                    let name_idx = self.const_idx(Const::Str(name.clone()));
                    self.code.push(Instr::BuiltinCall {
                        dst,
                        name_idx,
                        args_start,
                        n_args: args.len() as u8,
                    });
                    for _ in 0..args.len() { self.next_reg -= 1; }
                    return Ok(());
                }
                // Block context builtins — zero-arg, return Address/u64.
                if matches!(name.as_str(), "msg_sender" | "block_timestamp" | "block_number")
                    && args.is_empty()
                {
                    let kind: u8 = match name.as_str() {
                        "msg_sender" => 0,
                        "block_timestamp" => 1,
                        "block_number" => 2,
                        _ => unreachable!(),
                    };
                    self.code.push(Instr::Context { dst, kind });
                    return Ok(());
                }
                if matches!(name.as_str(), "i64" | "i32" | "u32" | "u64" | "u128")
                    && args.len() == 1
                {
                    let target: u8 = match name.as_str() {
                        "i64" => 0,
                        "i32" => 1,
                        "u32" => 2,
                        "u64" => 3,
                        "u128" => 4,
                        _ => unreachable!(),
                    };
                    let r = self.alloc();
                    self.compile_expr_into(&args[0], r)?;
                    self.code.push(Instr::Convert { dst, src: r, target });
                    self.free(r);
                    return Ok(());
                }

                let args_start = self.next_reg;
                for _ in 0..args.len() {
                    self.alloc();
                }
                for (i, a) in args.iter().enumerate() {
                    self.compile_expr_into(a, args_start + i as u16)?;
                }
                if let Some(mod_name) = module {
                    // `IFace::bind(target_module)` constructor —
                    // we recognize it by the iface_index lookup
                    // (populated from module.interfaces) and emit
                    // MakeInterface instead of a cross-module call.
                    if name == "bind" && self.iface_index.contains(mod_name) && args.len() == 1 {
                        let iface_name_idx = self.const_idx(Const::Str(mod_name.clone()));
                        self.code.push(Instr::MakeInterface {
                            dst,
                            iface_name_idx,
                            target_reg: args_start,
                        });
                        for _ in 0..args.len() { self.next_reg -= 1; }
                        return Ok(());
                    }
                    let module_name_idx = self.const_idx(Const::Str(mod_name.clone()));
                    let fn_name_idx = self.const_idx(Const::Str(name.clone()));
                    let (is_view, is_pure) = self
                        .extern_fn_flags
                        .get(&(mod_name.clone(), name.clone()))
                        .copied()
                        .unwrap_or((false, false));
                    self.code.push(Instr::CallExternal {
                        dst,
                        module_name_idx,
                        fn_name_idx,
                        args_start,
                        n_args: args.len() as u8,
                        is_view,
                        is_pure,
                    });
                } else if let Some(&import_idx) = self.import_index.get(name) {
                    self.code.push(Instr::CallHost {
                        dst,
                        import_idx: import_idx as u16,
                        args_start,
                        n_args: args.len() as u8,
                    });
                } else {
                    let fn_idx = *self.fn_index.get(name).ok_or_else(|| {
                        Error::new(
                            ErrorKind::Type,
                            format!("unknown function '{name}'"),
                            expr.span,
                        )
                    })?;
                    self.code.push(Instr::Call {
                        dst,
                        fn_idx: fn_idx as u16,
                        args_start,
                        n_args: args.len() as u8,
                    });
                }
                for _ in 0..args.len() {
                    self.next_reg -= 1;
                }
                Ok(())
            }
            ExprKind::TupleLit(elems) => {
                let n = elems.len() as u16;
                let args_start = self.next_reg;
                for _ in 0..elems.len() { self.alloc(); }
                for (i, e) in elems.iter().enumerate() {
                    self.compile_expr_into(e, args_start + i as u16)?;
                }
                self.code.push(Instr::MakeTuple { dst, args_start, n });
                for _ in 0..elems.len() { self.next_reg -= 1; }
                Ok(())
            }
            ExprKind::TupleIndex { target, index } => {
                let src = self.alloc();
                self.compile_expr_into(target, src)?;
                self.code.push(Instr::TupleGet {
                    dst,
                    src,
                    index: *index as u16,
                });
                self.free(src);
                Ok(())
            }
            ExprKind::Block(block) => {
                self.scopes.push(HashMap::new());
                let saved = self.next_reg;
                for stmt in &block.stmts {
                    self.compile_stmt(stmt)?;
                }
                if let Some(t) = &block.tail {
                    self.compile_expr_into(t, dst)?;
                } else {
                    // No tail → produces Unit; load a sentinel.
                    let idx = self.const_idx(Const::Bool(false));
                    self.code.push(Instr::LoadConst { dst, idx });
                }
                self.scopes.pop();
                self.next_reg = saved;
                Ok(())
            }
            ExprKind::If { cond, then, else_branch } => {
                // Lower like Stmt::If but with arm tails feeding `dst`.
                let cond_r = self.alloc();
                self.compile_expr_into(cond, cond_r)?;
                let jif_pos = self.code.len();
                self.code.push(Instr::JumpIfFalse { cond: cond_r, offset: 0 });
                self.free(cond_r);

                self.compile_block_into(then, dst)?;
                let jmp_end_pos = self.code.len();
                self.code.push(Instr::Jump { offset: 0 });

                let else_pos = self.code.len();
                let off = (else_pos as i32) - (jif_pos as i32) - 1;
                if let Instr::JumpIfFalse { offset, .. } = &mut self.code[jif_pos] {
                    *offset = off;
                }

                match else_branch {
                    ElseBranch::None => {
                        // typeck rejects this for if-as-expr, but be
                        // defensive — load a default into dst.
                        let idx = self.const_idx(Const::Bool(false));
                        self.code.push(Instr::LoadConst { dst, idx });
                    }
                    ElseBranch::Block(b) => self.compile_block_into(b, dst)?,
                    ElseBranch::If(inner) => {
                        let synthetic = Expr {
                            kind: ExprKind::If {
                                cond: Box::new(inner.cond.clone()),
                                then: inner.then.clone(),
                                else_branch: inner.else_branch.clone(),
                            },
                            span: inner.span,
                        };
                        self.compile_expr_into(&synthetic, dst)?;
                    }
                }

                let end_pos = self.code.len();
                let off = (end_pos as i32) - (jmp_end_pos as i32) - 1;
                if let Instr::Jump { offset } = &mut self.code[jmp_end_pos] {
                    *offset = off;
                }
                Ok(())
            }
            ExprKind::EnumCtor { enum_name, variant, args } => {
                let shape_idx = *self.enum_index.get(enum_name).ok_or_else(|| {
                    Error::new(
                        ErrorKind::Type,
                        format!("unknown enum '{enum_name}'"),
                        expr.span,
                    )
                })?;
                let shape = &self.enum_shapes[shape_idx];
                let variant_idx = shape
                    .variant_names
                    .iter()
                    .position(|n| n == variant)
                    .ok_or_else(|| Error::new(
                        ErrorKind::Type,
                        format!("enum '{enum_name}' has no variant '{variant}'"),
                        expr.span,
                    ))? as u16;
                let n = args.len() as u16;
                let args_start = self.next_reg;
                for _ in 0..args.len() { self.alloc(); }
                for (i, a) in args.iter().enumerate() {
                    self.compile_expr_into(a, args_start + i as u16)?;
                }
                self.code.push(Instr::MakeEnum {
                    dst,
                    shape_idx: shape_idx as u16,
                    variant_idx,
                    args_start,
                    n,
                });
                for _ in 0..args.len() { self.next_reg -= 1; }
                Ok(())
            }
            ExprKind::Match { scrut, arms } => {
                // Lower to: scrut → reg, tag → reg, then a cascade of
                // `Bin Eq` against each variant index, each followed
                // by JumpIfFalse to the next arm's check. The matched
                // arm extracts payloads, runs the body into `dst`,
                // and jumps to END.
                let scrut_reg = self.alloc();
                self.compile_expr_into(scrut, scrut_reg)?;
                let tag_reg = self.alloc();
                self.code.push(Instr::EnumTag { dst: tag_reg, src: scrut_reg });

                // Find the enum shape via the scrut's type. We don't
                // track typed bytecode, but the parser guarantees
                // every arm pattern names an enum we can look up.
                // Use the first non-wildcard arm's enum_name.
                let enum_name_opt = arms.iter().find_map(|a| match &a.pattern {
                    crate::ast::MatchPattern::EnumVariant { enum_name, .. } => Some(enum_name.clone()),
                    _ => None,
                });
                let shape = enum_name_opt
                    .as_ref()
                    .and_then(|n| self.enum_index.get(n))
                    .map(|i| &self.enum_shapes[*i]);

                let mut end_jumps: Vec<usize> = Vec::new();
                for arm in arms {
                    match &arm.pattern {
                        crate::ast::MatchPattern::Wildcard => {
                            // Always-fires: emit body then jump to END.
                            self.compile_match_body(&arm.body, dst, scrut_reg, &[])?;
                            let pos = self.code.len();
                            self.code.push(Instr::Jump { offset: 0 });
                            end_jumps.push(pos);
                            // Subsequent arms become unreachable; stop.
                            break;
                        }
                        crate::ast::MatchPattern::EnumVariant { variant, bindings, .. } => {
                            let variant_idx = shape
                                .and_then(|s| s.variant_names.iter().position(|n| n == variant))
                                .ok_or_else(|| Error::new(
                                    ErrorKind::Type,
                                    format!("unknown variant '{variant}'"),
                                    arm.span,
                                ))? as i64;
                            // tag == variant_idx?
                            let const_reg = self.alloc();
                            let cidx = self.const_idx(Const::Int(num_bigint::BigInt::from(variant_idx)));
                            self.code.push(Instr::LoadConst { dst: const_reg, idx: cidx });
                            let cmp = self.alloc();
                            self.code.push(Instr::Bin {
                                op: BinOp::Eq, dst: cmp, lhs: tag_reg, rhs: const_reg,
                            });
                            self.free(const_reg);
                            let skip_pos = self.code.len();
                            self.code.push(Instr::JumpIfFalse { cond: cmp, offset: 0 });
                            self.free(cmp);

                            // Bind payload registers, then compile body.
                            self.compile_match_body(&arm.body, dst, scrut_reg, bindings)?;
                            let jmp_pos = self.code.len();
                            self.code.push(Instr::Jump { offset: 0 });
                            end_jumps.push(jmp_pos);
                            // Patch the JumpIfFalse to land here.
                            let here = self.code.len();
                            if let Instr::JumpIfFalse { offset, .. } = &mut self.code[skip_pos] {
                                *offset = (here as i32) - (skip_pos as i32) - 1;
                            }
                        }
                    }
                }

                // If exhaustive without wildcard, the last arm's
                // JumpIfFalse points to a runtime trap — emit one.
                let trap_pos = self.code.len();
                self.code.push(Instr::ReturnUnit); // unreachable in well-typed code

                let end_pos = self.code.len();
                for pos in end_jumps {
                    if let Instr::Jump { offset } = &mut self.code[pos] {
                        *offset = (end_pos as i32) - (pos as i32) - 1;
                    }
                }
                let _ = trap_pos;
                self.free(tag_reg);
                self.free(scrut_reg);
                Ok(())
            }
        }
    }

    fn compile_match_body(
        &mut self,
        body: &Expr,
        dst: u16,
        scrut_reg: u16,
        bindings: &[String],
    ) -> Result<(), Error> {
        // Each binding gets a fresh local register populated via
        // EnumPayload, scoped to the arm's body.
        self.scopes.push(HashMap::new());
        let mut alloced = Vec::with_capacity(bindings.len());
        for (i, name) in bindings.iter().enumerate() {
            let r = self.alloc();
            self.code.push(Instr::EnumPayload {
                dst: r,
                src: scrut_reg,
                index: i as u16,
            });
            self.scopes.last_mut().unwrap().insert(name.clone(), r);
            alloced.push(r);
        }
        self.compile_expr_into(body, dst)?;
        self.scopes.pop();
        for r in alloced.into_iter().rev() { self.free(r); }
        Ok(())
    }
}
