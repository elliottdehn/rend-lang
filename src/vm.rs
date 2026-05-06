//! Register-based VM with fuel metering.
//!
//! Each instruction costs 1 fuel by default. Out-of-fuel returns a runtime
//! error instead of running unbounded — this is the actual sandbox enforcement
//! that makes embedding under adversarial constraints sound.

use crate::ast::{BinOp, UnOp};
use crate::bc::*;
use crate::error::{Error, ErrorKind};
use crate::host::Host;
use crate::kv::EmptyKv;
use crate::token::Span;
use crate::tx::Tx;
use crate::value::Value;

pub const OUT_OF_FUEL_MSG: &str = "out of fuel";

pub struct Fuel {
    pub remaining: u64,
}

impl Fuel {
    pub fn new(amount: u64) -> Self {
        Self { remaining: amount }
    }
    pub fn unlimited() -> Self {
        Self { remaining: u64::MAX }
    }
}

pub fn run(module: &BcModule, name: &str, args: &[Value], fuel: Fuel) -> Result<Value, Error> {
    let host = Host::new();
    let kv = EmptyKv;
    let mut tx = Tx::new(&kv);
    run_full(module, name, args, fuel, &host, &mut tx)
}

pub fn run_with_host(
    module: &BcModule,
    name: &str,
    args: &[Value],
    fuel: Fuel,
    host: &Host,
) -> Result<Value, Error> {
    let kv = EmptyKv;
    let mut tx = Tx::new(&kv);
    run_full(module, name, args, fuel, host, &mut tx)
}

pub fn run_full(
    module: &BcModule,
    name: &str,
    args: &[Value],
    fuel: Fuel,
    host: &Host,
    tx: &mut Tx,
) -> Result<Value, Error> {
    let single = std::slice::from_ref(module);
    run_world(single, 0, name, args, fuel, host, tx)
}

/// Multi-module entry point. `modules[main_idx]` is the dispatch start;
/// `CallExternal` instructions resolve names against `modules` directly.
pub fn run_world(
    modules: &[BcModule],
    main_idx: usize,
    name: &str,
    args: &[Value],
    fuel: Fuel,
    host: &Host,
    tx: &mut Tx,
) -> Result<Value, Error> {
    let module = &modules[main_idx];
    let fn_idx = *module.fn_index.get(name).ok_or_else(|| {
        Error::new(
            ErrorKind::Runtime,
            format!("unknown function '{name}'"),
            Span::default(),
        )
    })?;
    // Enable lazy reads for the duration of this VM run. Primitive
    // reads queue up and batch via `force`; struct reads batch their
    // leaves eagerly. Restored on exit so the Tx the host borrowed
    // ends up with an empty pending queue and the return value is
    // fully resolved.
    tx.set_lazy(true);
    let mut state = VmState { fuel: fuel.remaining, host, tx, modules };
    let result = state.call(main_idx, fn_idx, args);
    let result = result.map(|v| state.tx.force(v));
    // Drain any reads the program issued but didn't consume — the OCC
    // read set must reflect every observation the tx made, even
    // unconsumed ones, to match eager-mode validation.
    state.tx.flush_pending();
    state.tx.set_lazy(false);
    result
}

struct VmState<'a, 'tx> {
    fuel: u64,
    host: &'a Host,
    tx: &'a mut Tx<'tx>,
    modules: &'a [BcModule],
}

impl<'a, 'tx> VmState<'a, 'tx> {
    /// Resolve a register's value, flushing any queued reads if it (or
    /// anything reachable through it) is a `Value::Pending`. Caches
    /// the result back into the register so subsequent forces of the
    /// same register are O(1).
    fn force_reg(&mut self, regs: &mut [Value], r: u16) -> Value {
        let v = self.tx.force(regs[r as usize].clone());
        regs[r as usize] = v.clone();
        v
    }

    /// Shallow variant — resolves only if the register itself is a
    /// `Value::Pending`. Nested Pending elements/fields stay deferred.
    /// Used by ops that need the value's shape (Array/Struct/etc.) but
    /// don't actually consume the leaf data.
    fn force_reg_spine(&mut self, regs: &mut [Value], r: u16) -> Value {
        let v = self.tx.force_spine(regs[r as usize].clone());
        regs[r as usize] = v.clone();
        v
    }

    fn tick(&mut self) -> Result<(), Error> {
        if self.fuel == 0 {
            return Err(Error::new(
                ErrorKind::Runtime,
                OUT_OF_FUEL_MSG,
                Span::default(),
            ));
        }
        self.fuel -= 1;
        Ok(())
    }

    fn call(&mut self, module_idx: usize, fn_idx: usize, args: &[Value]) -> Result<Value, Error> {
        let module = &self.modules[module_idx];
        let f = &module.functions[fn_idx];
        if args.len() != f.n_params as usize {
            return Err(Error::new(
                ErrorKind::Runtime,
                format!(
                    "function '{}' expects {} arg(s), got {}",
                    f.name,
                    f.n_params,
                    args.len()
                ),
                Span::default(),
            ));
        }
        // Non-reentrant guard: if this fn is already on the call
        // stack under `nore`, abort. Otherwise mark and run; we
        // unmark on every successful or error return so the slot
        // doesn't leak.
        let nore_key = (module_idx as u32, fn_idx as u32);
        if f.is_nore && !self.tx.nore_enter(nore_key.0, nore_key.1) {
            return Err(Error::new(
                ErrorKind::Runtime,
                format!("re-entry into non-reentrant function '{}'", f.name),
                Span::default(),
            ));
        }
        // Wrap the rest of the body so nore_exit always fires.
        let result = self.call_body(module_idx, fn_idx, args);
        if f.is_nore { self.tx.nore_exit(nore_key.0, nore_key.1); }
        result
    }

    fn call_body(&mut self, module_idx: usize, fn_idx: usize, args: &[Value]) -> Result<Value, Error> {
        let module = &self.modules[module_idx];
        let f = &module.functions[fn_idx];
        let mut regs: Vec<Value> = (0..f.n_regs.max(args.len() as u16))
            .map(|_| Value::Unit)
            .collect();
        for (i, a) in args.iter().enumerate() {
            regs[i] = a.clone();
        }
        let mut pc: usize = 0;
        loop {
            self.tick()?;
            let instr = f.code.get(pc).cloned().ok_or_else(|| {
                Error::new(
                    ErrorKind::Runtime,
                    "instruction pointer out of bounds",
                    Span::default(),
                )
            })?;
            pc += 1;
            match instr {
                Instr::LoadConst { dst, idx } => {
                    regs[dst as usize] = match &f.consts[idx as usize] {
                        Const::Int(n) => Value::Int(*n),
                        Const::I32(n) => Value::I32(*n),
                        Const::U32(n) => Value::U32(*n),
                        Const::U64(n) => Value::U64(*n),
                        Const::U128(n) => Value::U128(*n),
                        Const::Bool(b) => Value::Bool(*b),
                        Const::Str(s) => Value::Str(s.clone()),
                    };
                }
                Instr::Convert { dst, src, target } => {
                    let v = self.force_reg(&mut regs, src);
                    regs[dst as usize] = crate::ops::convert(&v, target, Span::default())?;
                }
                Instr::Move { dst, src } => {
                    regs[dst as usize] = regs[src as usize].clone();
                }
                Instr::Bin { op, dst, lhs, rhs } => {
                    let l = self.force_reg(&mut regs, lhs);
                    let r = self.force_reg(&mut regs, rhs);
                    regs[dst as usize] = eval_bin(op, l, r)?;
                }
                Instr::Un { op, dst, src } => {
                    let v = self.force_reg(&mut regs, src);
                    regs[dst as usize] = eval_un(op, v)?;
                }
                Instr::Jump { offset } => {
                    pc = ((pc as i64) + offset as i64) as usize;
                }
                Instr::JumpIfFalse { cond, offset } => {
                    let v = self.force_reg(&mut regs, cond);
                    if let Value::Bool(false) = v {
                        pc = ((pc as i64) + offset as i64) as usize;
                    }
                }
                Instr::Call { dst, fn_idx: callee, args_start, n_args } => {
                    let mut call_args = Vec::with_capacity(n_args as usize);
                    for i in 0..n_args {
                        call_args.push(regs[(args_start + i as u16) as usize].clone());
                    }
                    let result = self.call(module_idx, callee as usize, &call_args)?;
                    regs[dst as usize] = result;
                }
                Instr::CallExternal { dst, module_name_idx, fn_name_idx, args_start, n_args, .. } => {
                    let mod_name = match &f.consts[module_name_idx as usize] {
                        Const::Str(s) => s.clone(),
                        _ => return Err(Error::new(
                            ErrorKind::Runtime,
                            "CallExternal: module name const must be string",
                            Span::default(),
                        )),
                    };
                    let fn_name = match &f.consts[fn_name_idx as usize] {
                        Const::Str(s) => s.clone(),
                        _ => return Err(Error::new(
                            ErrorKind::Runtime,
                            "CallExternal: fn name const must be string",
                            Span::default(),
                        )),
                    };
                    let target_idx = self.modules.iter().position(|m| m.name == mod_name).ok_or_else(|| {
                        Error::new(
                            ErrorKind::Runtime,
                            format!("unknown module '{mod_name}'"),
                            Span::default(),
                        )
                    })?;
                    let target = &self.modules[target_idx];
                    let target_fn_idx = *target.fn_index.get(&fn_name).ok_or_else(|| {
                        Error::new(
                            ErrorKind::Runtime,
                            format!("unknown function '{fn_name}' in module '{mod_name}'"),
                            Span::default(),
                        )
                    })?;
                    if !target.functions[target_fn_idx].is_entry {
                        return Err(Error::new(
                            ErrorKind::Runtime,
                            format!("function '{fn_name}' in module '{mod_name}' is not an entry function"),
                            Span::default(),
                        ));
                    }
                    let mut call_args = Vec::with_capacity(n_args as usize);
                    for i in 0..n_args {
                        call_args.push(regs[(args_start + i as u16) as usize].clone());
                    }
                    let result = self.call(target_idx, target_fn_idx, &call_args)?;
                    regs[dst as usize] = result;
                }
                Instr::MakeInterface { dst, iface_name_idx, target_reg } => {
                    let iface = match &f.consts[iface_name_idx as usize] {
                        Const::Str(s) => s.clone(),
                        _ => return Err(Error::new(
                            ErrorKind::Runtime,
                            "MakeInterface: iface name const must be string",
                            Span::default(),
                        )),
                    };
                    let target_v = self.force_reg(&mut regs, target_reg);
                    let target_module = match target_v {
                        Value::Str(s) => s,
                        other => return Err(Error::new(
                            ErrorKind::Runtime,
                            format!("MakeInterface: target must be string, got {other}"),
                            Span::default(),
                        )),
                    };
                    // Bind-time conformance check: the bound module
                    // must implement every method the interface
                    // declares, with matching signatures and an
                    // effect bound at-least-as-strict. Failing fast
                    // here (rather than at first dispatch) makes
                    // the error point at the bind site.
                    let iface_decl = self
                        .modules
                        .iter()
                        .find_map(|m| m.interface_index.get(&iface).map(|i| &m.interfaces[*i]))
                        .ok_or_else(|| Error::new(
                            ErrorKind::Runtime,
                            format!("MakeInterface: interface '{iface}' not declared in any loaded module"),
                            Span::default(),
                        ))?;
                    let target_mod = self
                        .modules
                        .iter()
                        .find(|m| m.name == target_module)
                        .ok_or_else(|| Error::new(
                            ErrorKind::Runtime,
                            format!("IFace::bind: target module '{target_module}' not loaded"),
                            Span::default(),
                        ))?;
                    if let Err(msg) = check_iface_conformance(iface_decl, target_mod) {
                        return Err(Error::new(
                            ErrorKind::Runtime,
                            msg,
                            Span::default(),
                        ));
                    }
                    regs[dst as usize] = Value::Interface { iface, target_module };
                }
                Instr::CallExternalDyn { dst, target_reg, fn_name_idx, args_start, n_args, .. } => {
                    // Pull the bound module name from the target
                    // register (a Value::Interface) and dispatch
                    // through the world's module index. Same entry-
                    // only / known-fn rules as CallExternal.
                    let target_v = self.force_reg(&mut regs, target_reg);
                    let mod_name = match target_v {
                        Value::Interface { target_module, .. } => target_module,
                        other => return Err(Error::new(
                            ErrorKind::Runtime,
                            format!("dynamic dispatch on non-interface value: {other}"),
                            Span::default(),
                        )),
                    };
                    if mod_name.is_empty() {
                        return Err(Error::new(
                            ErrorKind::Runtime,
                            "dynamic dispatch on unbound interface (call IFace::bind first)",
                            Span::default(),
                        ));
                    }
                    let fn_name = match &f.consts[fn_name_idx as usize] {
                        Const::Str(s) => s.clone(),
                        _ => return Err(Error::new(
                            ErrorKind::Runtime,
                            "CallExternalDyn: fn name const must be string",
                            Span::default(),
                        )),
                    };
                    let target_idx = self.modules.iter().position(|m| m.name == mod_name).ok_or_else(|| {
                        Error::new(
                            ErrorKind::Runtime,
                            format!("dynamic dispatch: bound module '{mod_name}' not loaded"),
                            Span::default(),
                        )
                    })?;
                    let target = &self.modules[target_idx];
                    let target_fn_idx = *target.fn_index.get(&fn_name).ok_or_else(|| {
                        Error::new(
                            ErrorKind::Runtime,
                            format!("dynamic dispatch: '{mod_name}::{fn_name}' not defined"),
                            Span::default(),
                        )
                    })?;
                    if !target.functions[target_fn_idx].is_entry {
                        return Err(Error::new(
                            ErrorKind::Runtime,
                            format!("dynamic dispatch: '{mod_name}::{fn_name}' is not an entry fn"),
                            Span::default(),
                        ));
                    }
                    let mut call_args = Vec::with_capacity(n_args as usize);
                    for i in 0..n_args {
                        call_args.push(regs[(args_start + i as u16) as usize].clone());
                    }
                    let result = self.call(target_idx, target_fn_idx, &call_args)?;
                    regs[dst as usize] = result;
                }
                Instr::CallHost { dst, import_idx, args_start, n_args } => {
                    // Host-bound functions get only concrete values.
                    let mut call_args = Vec::with_capacity(n_args as usize);
                    for i in 0..n_args {
                        call_args.push(self.force_reg(&mut regs, args_start + i as u16));
                    }
                    let name = &module.imports[import_idx as usize];
                    let result = self.host.call(name, &call_args).map_err(|err| {
                        Error::new(
                            ErrorKind::Runtime,
                            format!("host '{name}' [code {}]: {}", err.code, err.msg),
                            Span::default(),
                        )
                    })?;
                    regs[dst as usize] = result;
                }
                Instr::Return { src } => {
                    return Ok(regs[src as usize].clone());
                }
                Instr::ReturnUnit => {
                    return Ok(Value::Unit);
                }
                Instr::BuiltinResource { dst, src } => {
                    let v = self.force_reg(&mut regs, src);
                    regs[dst as usize] = match v {
                        Value::Int(n) => Value::Resource(n),
                        other => return Err(Error::new(
                            ErrorKind::Runtime,
                            format!("resource() expects i64, got {other}"),
                            Span::default(),
                        )),
                    };
                }
                Instr::BuiltinUnwrap { dst, src } => {
                    let v = self.force_reg(&mut regs, src);
                    regs[dst as usize] = match v {
                        Value::Resource(n) => Value::Int(n),
                        other => return Err(Error::new(
                            ErrorKind::Runtime,
                            format!("unwrap() expects Resource, got {other}"),
                            Span::default(),
                        )),
                    };
                }
                Instr::BuiltinAddress { dst, src } => {
                    let v = self.force_reg(&mut regs, src);
                    regs[dst as usize] = match v {
                        Value::Str(s) => Value::Address(s),
                        other => return Err(Error::new(
                            ErrorKind::Runtime,
                            format!("address() expects string, got {other}"),
                            Span::default(),
                        )),
                    };
                }
                Instr::BuiltinLen { dst, src } => {
                    // len() needs the actual collection — force at top
                    // level. Pending elements *inside* an array stay
                    // Pending; we just need to know the length, which
                    // requires the array's spine to be concrete.
                    let v = self.force_reg_spine(&mut regs, src);
                    regs[dst as usize] = match v {
                        Value::Array(elems) => Value::Int(elems.len() as i64),
                        Value::Str(s) => Value::Int(s.as_bytes().len() as i64),
                        Value::Set(elems) => Value::Int(elems.len() as i64),
                        Value::Dict(pairs) => Value::Int(pairs.len() as i64),
                        other => return Err(Error::new(
                            ErrorKind::Runtime,
                            format!("len() expects array or string, got {other}"),
                            Span::default(),
                        )),
                    };
                }
                Instr::MakeArray { dst, args_start, n } => {
                    let mut elems = Vec::with_capacity(n as usize);
                    for i in 0..n {
                        elems.push(regs[(args_start + i) as usize].clone());
                    }
                    regs[dst as usize] = Value::Array(elems);
                }
                Instr::MakeStruct { dst, shape_idx, args_start, n } => {
                    let shape = &module.struct_shapes[shape_idx as usize];
                    let mut fields = Vec::with_capacity(n as usize);
                    for i in 0..n {
                        let name = shape.field_names[i as usize].clone();
                        let v = regs[(args_start + i) as usize].clone();
                        fields.push((name, v));
                    }
                    regs[dst as usize] = Value::Struct {
                        name: shape.name.clone(),
                        fields,
                    };
                }
                Instr::FieldGet { dst, src, name_idx } => {
                    let field_name = match &f.consts[name_idx as usize] {
                        Const::Str(s) => s.clone(),
                        other => return Err(Error::new(
                            ErrorKind::Runtime,
                            format!("field name const must be string, got {other:?}"),
                            Span::default(),
                        )),
                    };
                    // Force at the struct's spine — we need to know it
                    // *is* a struct to extract a field. The extracted
                    // value can itself stay Pending and propagate.
                    let v = self.force_reg_spine(&mut regs, src);
                    let v = match v {
                        Value::Struct { fields, .. } => fields
                            .into_iter()
                            .find(|(n, _)| n == &field_name)
                            .map(|(_, v)| v)
                            .ok_or_else(|| Error::new(
                                ErrorKind::Runtime,
                                format!("no field '{field_name}'"),
                                Span::default(),
                            ))?,
                        other => return Err(Error::new(
                            ErrorKind::Runtime,
                            format!("field access on non-struct: {other}"),
                            Span::default(),
                        )),
                    };
                    regs[dst as usize] = v;
                }
                Instr::FieldSet { dst, name_idx, val } => {
                    let field_name = match &f.consts[name_idx as usize] {
                        Const::Str(s) => s.clone(),
                        other => return Err(Error::new(
                            ErrorKind::Runtime,
                            format!("field name const must be string, got {other:?}"),
                            Span::default(),
                        )),
                    };
                    let new_val = regs[val as usize].clone();
                    let _ = self.force_reg_spine(&mut regs, dst);
                    match &mut regs[dst as usize] {
                        Value::Struct { fields, .. } => {
                            let pair = fields
                                .iter_mut()
                                .find(|(n, _)| n == &field_name)
                                .ok_or_else(|| Error::new(
                                    ErrorKind::Runtime,
                                    format!("no field '{field_name}'"),
                                    Span::default(),
                                ))?;
                            pair.1 = new_val;
                        }
                        other => return Err(Error::new(
                            ErrorKind::Runtime,
                            format!("field assignment on non-struct: {other}"),
                            Span::default(),
                        )),
                    }
                }
                Instr::MakeSet { dst, args_start, n } => {
                    // Force every element so set-dedup compares concrete
                    // values, not Pending handles (two reads of the same
                    // key produce different handles but identical values
                    // — without forcing, the set would falsely keep both).
                    let mut elems: Vec<Value> = Vec::with_capacity(n as usize);
                    for i in 0..n {
                        let v = self.force_reg(&mut regs, args_start + i);
                        if !elems.iter().any(|x| x == &v) { elems.push(v); }
                    }
                    regs[dst as usize] = Value::Set(elems);
                }
                Instr::MakeDict { dst, args_start, n_pairs } => {
                    // Force keys for the same reason as MakeSet; values
                    // can stay Pending and propagate.
                    let mut pairs: Vec<(Value, Value)> = Vec::with_capacity(n_pairs as usize);
                    for i in 0..n_pairs {
                        let k = self.force_reg(&mut regs, args_start + i * 2);
                        let v = regs[(args_start + i * 2 + 1) as usize].clone();
                        if let Some(p) = pairs.iter_mut().find(|(kk, _)| kk == &k) {
                            p.1 = v;
                        } else {
                            pairs.push((k, v));
                        }
                    }
                    regs[dst as usize] = Value::Dict(pairs);
                }
                Instr::BuiltinCall { dst, name_idx, args_start, n_args } => {
                    let name = match &f.consts[name_idx as usize] {
                        Const::Str(s) => s.clone(),
                        _ => return Err(Error::new(
                            ErrorKind::Runtime,
                            "BuiltinCall: name const must be string",
                            Span::default(),
                        )),
                    };
                    // Builtins (set_/dict_/etc.) inspect their args.
                    let mut call_args = Vec::with_capacity(n_args as usize);
                    for i in 0..n_args {
                        call_args.push(self.force_reg(&mut regs, args_start + i as u16));
                    }
                    regs[dst as usize] = crate::ops::call_builtin(&name, &call_args)?;
                }
                Instr::ArrayAppend { dst, arr, elem } => {
                    // Force the array's spine; elem can stay Pending.
                    let arr_v = self.force_reg_spine(&mut regs, arr);
                    let new_arr = match arr_v {
                        Value::Array(mut v) => {
                            v.push(regs[elem as usize].clone());
                            Value::Array(v)
                        }
                        other => return Err(Error::new(
                            ErrorKind::Runtime,
                            format!("array_append on non-array: {other}"),
                            Span::default(),
                        )),
                    };
                    regs[dst as usize] = new_arr;
                }
                Instr::ArrayGet { dst, arr, idx } => {
                    // Spine-force the array (need to know it's a
                    // Value::Array and read its length); full-force
                    // the index (must be a concrete Int to use).
                    let arr_val = self.force_reg_spine(&mut regs, arr);
                    let idx_val = self.force_reg(&mut regs, idx);
                    let (Value::Array(elems), Value::Int(i)) = (arr_val, idx_val) else {
                        return Err(Error::new(
                            ErrorKind::Runtime,
                            "array index requires [T] and i64".to_string(),
                            Span::default(),
                        ));
                    };
                    if i < 0 || (i as usize) >= elems.len() {
                        return Err(Error::new(
                            ErrorKind::Runtime,
                            format!("array index out of bounds: {i} of {}", elems.len()),
                            Span::default(),
                        ));
                    }
                    regs[dst as usize] = elems[i as usize].clone();
                }
                Instr::KvGet { dst, key_idx } => {
                    let key = module.state_roots[key_idx as usize];
                    let ty = &module.state_types[key_idx as usize];
                    regs[dst as usize] = self.tx.read_typed(key, ty);
                }
                Instr::KvPut { src, key_idx } => {
                    let key = module.state_roots[key_idx as usize];
                    let ty = &module.state_types[key_idx as usize];
                    self.tx.write_typed(key, ty, regs[src as usize].clone());
                }
                Instr::KvGetPath { dst, path_idx } => {
                    let spec = &module.path_specs[path_idx as usize];
                    regs[dst as usize] = self.tx.read_typed(spec.leaf_key, &spec.leaf_type);
                }
                Instr::PrefetchMap { arr_reg, state_idx } => {
                    // Need concrete keys to compute cell addresses, so
                    // force the array (and its elements — if elements
                    // are Pending, the iter values aren't yet known).
                    let arr_v = self.force_reg(&mut regs, arr_reg);
                    let arr = match arr_v {
                        Value::Array(elems) => elems,
                        _ => continue, // shape changed; silently no-op
                    };
                    let value_ty = match &module.state_types[state_idx as usize] {
                        crate::ast::Type::Map { value, .. } => (**value).clone(),
                        _ => continue,
                    };
                    let root = module.state_roots[state_idx as usize];
                    let mut ops: Vec<(u128, crate::ast::Type)> = Vec::with_capacity(arr.len());
                    for elem in &arr {
                        if !is_keyable_value(elem) { continue; }
                        let cell = compose_map_cell(root, elem)?;
                        ops.push((cell, value_ty.clone()));
                    }
                    // Warm the cache; results are discarded, but Tx
                    // records each leaf in `reads` so the in-loop
                    // MapGets serve from cache.
                    let _ = self.tx.read_typed_many(&ops);
                }
                Instr::MakeEnum { dst, shape_idx, variant_idx, args_start, n } => {
                    let shape = &module.enum_shapes[shape_idx as usize];
                    let variant = shape.variant_names[variant_idx as usize].clone();
                    let mut payload = Vec::with_capacity(n as usize);
                    for i in 0..n {
                        payload.push(regs[(args_start + i) as usize].clone());
                    }
                    regs[dst as usize] = Value::Enum {
                        enum_name: shape.name.clone(),
                        variant,
                        payload,
                    };
                }
                Instr::EnumTag { dst, src } => {
                    let v = self.force_reg_spine(&mut regs, src);
                    let (enum_name, variant_name) = match v {
                        Value::Enum { enum_name, variant, .. } => (enum_name, variant),
                        other => return Err(Error::new(
                            ErrorKind::Runtime,
                            format!("EnumTag on non-enum: {other}"),
                            Span::default(),
                        )),
                    };
                    let shape_idx = module
                        .enum_index
                        .get(&enum_name)
                        .copied()
                        .ok_or_else(|| Error::new(
                            ErrorKind::Runtime,
                            format!("unknown enum '{enum_name}'"),
                            Span::default(),
                        ))?;
                    let idx = module.enum_shapes[shape_idx]
                        .variant_names
                        .iter()
                        .position(|n| n == &variant_name)
                        .ok_or_else(|| Error::new(
                            ErrorKind::Runtime,
                            format!("variant '{variant_name}' not found in '{enum_name}'"),
                            Span::default(),
                        ))?;
                    regs[dst as usize] = Value::Int(idx as i64);
                }
                Instr::EnumPayload { dst, src, index } => {
                    let v = self.force_reg_spine(&mut regs, src);
                    let payload = match v {
                        Value::Enum { payload, .. } => payload,
                        other => return Err(Error::new(
                            ErrorKind::Runtime,
                            format!("EnumPayload on non-enum: {other}"),
                            Span::default(),
                        )),
                    };
                    let i = index as usize;
                    if i >= payload.len() {
                        return Err(Error::new(
                            ErrorKind::Runtime,
                            format!("EnumPayload index {i} out of bounds (len {})", payload.len()),
                            Span::default(),
                        ));
                    }
                    regs[dst as usize] = payload[i].clone();
                }
                Instr::MakeTuple { dst, args_start, n } => {
                    // Tuples don't force their elements — Pending
                    // values flow through, just like MakeArray.
                    let mut elems = Vec::with_capacity(n as usize);
                    for i in 0..n {
                        elems.push(regs[(args_start + i) as usize].clone());
                    }
                    regs[dst as usize] = Value::Tuple(elems);
                }
                Instr::TupleGet { dst, src, index } => {
                    // Spine-force only — the extracted element keeps
                    // any nested Pending it had.
                    let v = self.force_reg_spine(&mut regs, src);
                    let elems = match v {
                        Value::Tuple(elems) => elems,
                        other => return Err(Error::new(
                            ErrorKind::Runtime,
                            format!("tuple index on non-tuple: {other}"),
                            Span::default(),
                        )),
                    };
                    if (index as usize) >= elems.len() {
                        return Err(Error::new(
                            ErrorKind::Runtime,
                            format!("tuple index {index} out of bounds (len {})", elems.len()),
                            Span::default(),
                        ));
                    }
                    regs[dst as usize] = elems[index as usize].clone();
                }
                Instr::Context { dst, kind } => {
                    let ctx = self.tx.context();
                    regs[dst as usize] = match kind {
                        0 => ctx.sender.clone(),
                        1 => Value::U64(ctx.block_timestamp),
                        2 => Value::U64(ctx.block_number),
                        other => return Err(Error::new(
                            ErrorKind::Runtime,
                            format!("unknown context kind {other}"),
                            Span::default(),
                        )),
                    };
                }
                Instr::Emit { event_idx, args_start, n_args } => {
                    // Force every arg fully — the host receives
                    // concrete values, never Pending handles. Stamp
                    // the entry with the *executing* module's name so
                    // cross-module callers see who emitted what.
                    let shape = &module.events[event_idx as usize];
                    let mut args: Vec<Value> = Vec::with_capacity(n_args as usize);
                    for i in 0..n_args {
                        args.push(self.force_reg(&mut regs, args_start + i as u16));
                    }
                    self.tx.emit(crate::tx::EmittedEvent {
                        module: module.name.clone(),
                        name: shape.name.clone(),
                        args,
                    });
                }
                Instr::ReadBatch { group_idx } => {
                    let group = &f.read_groups[group_idx as usize];
                    let ops: Vec<(u128, crate::ast::Type)> = group
                        .iter()
                        .map(|op| match op {
                            crate::bc::ReadOp::State { key_idx, .. } => (
                                module.state_roots[*key_idx as usize],
                                module.state_types[*key_idx as usize].clone(),
                            ),
                            crate::bc::ReadOp::Path { path_idx, .. } => {
                                let spec = &module.path_specs[*path_idx as usize];
                                (spec.leaf_key, spec.leaf_type.clone())
                            }
                        })
                        .collect();
                    let values = self.tx.read_typed_many(&ops);
                    for (op, v) in group.iter().zip(values.into_iter()) {
                        let dst = match op {
                            crate::bc::ReadOp::State { dst, .. } => *dst,
                            crate::bc::ReadOp::Path { dst, .. } => *dst,
                        };
                        regs[dst as usize] = v;
                    }
                }
                Instr::KvPutPath { src, path_idx } => {
                    let spec = &module.path_specs[path_idx as usize];
                    self.tx.write_typed(spec.leaf_key, &spec.leaf_type, regs[src as usize].clone());
                }
                Instr::MapGet { dst, state_idx, key_reg } => {
                    let root = module.state_roots[state_idx as usize];
                    let key_value = self.force_reg(&mut regs, key_reg);
                    let cell = compose_map_cell(root, &key_value)?;
                    let value_ty = match &module.state_types[state_idx as usize] {
                        crate::ast::Type::Map { value, .. } => (**value).clone(),
                        other => other.clone(),
                    };
                    // Map cells store their value as one blob (matching
                    // MapPut), so use read_cell — never split structs
                    // into leaf reads. Still lazy when enabled.
                    regs[dst as usize] = self.tx.read_cell(cell, &value_ty);
                }
                Instr::MapPut { state_idx, key_reg, src } => {
                    let root = module.state_roots[state_idx as usize];
                    let key_value = self.force_reg(&mut regs, key_reg);
                    let cell = compose_map_cell(root, &key_value)?;
                    // Tx::write forces the value internally so writes
                    // never carry Pending leaves.
                    self.tx.write(cell, regs[src as usize].clone());
                }
                Instr::PMapGet { dst, state_idx, key_reg } => {
                    {
                        let pmap_ty = &module.state_types[state_idx as usize];
                        if let crate::ast::Type::PMap { key, value } = pmap_ty {
                            self.tx.record_pmap_types(
                                module.state_roots[state_idx as usize],
                                (**key).clone(),
                                (**value).clone(),
                            );
                        }
                    }
                    // Walk O(log32 N) node cells via Tx — the tree
                    // is spread across content-addressed cells, so
                    // a multi-gigabyte pmap never sits in memory.
                    let root_cell = module.state_roots[state_idx as usize];
                    let key_value = self.force_reg(&mut regs, key_reg);
                    let pmap_ty = module.state_types[state_idx as usize].clone();
                    let (key_ty, value_ty) = match &pmap_ty {
                        crate::ast::Type::PMap { key, value } => ((**key).clone(), (**value).clone()),
                        _ => unreachable!("PMapGet on non-pmap state"),
                    };
                    let root_v = self.tx.read_cell(root_cell, &pmap_ty);
                    let root_v = self.tx.force(root_v);
                    let root_hash = match root_v {
                        Value::PMap(h) => h,
                        _ => crate::pmap::EMPTY,
                    };
                    regs[dst as usize] = crate::pmap::get(root_hash, &key_value, &mut self.tx, &key_ty, &value_ty)
                        .unwrap_or_else(|| Value::default_for(&value_ty));
                }
                Instr::PMapPut { state_idx, key_reg, src } => {
                    {
                        let pmap_ty = &module.state_types[state_idx as usize];
                        if let crate::ast::Type::PMap { key, value } = pmap_ty {
                            self.tx.record_pmap_types(
                                module.state_roots[state_idx as usize],
                                (**key).clone(),
                                (**value).clone(),
                            );
                        }
                    }
                    let root_cell = module.state_roots[state_idx as usize];
                    let key_value = self.force_reg(&mut regs, key_reg);
                    let value = self.force_reg(&mut regs, src);
                    let pmap_ty = module.state_types[state_idx as usize].clone();
                    let (key_ty, value_ty) = match &pmap_ty {
                        crate::ast::Type::PMap { key, value } => ((**key).clone(), (**value).clone()),
                        _ => unreachable!("PMapPut on non-pmap state"),
                    };
                    let root_v = self.tx.read_cell(root_cell, &pmap_ty);
                    let root_v = self.tx.force(root_v);
                    let root_hash = match root_v {
                        Value::PMap(h) => h,
                        _ => crate::pmap::EMPTY,
                    };
                    let new_root = crate::pmap::set(root_hash, key_value, value, &mut self.tx, &key_ty, &value_ty)?;
                    self.tx.write(root_cell, Value::PMap(new_root));
                }
                Instr::PMapContains { dst, state_idx, key_reg } => {
                    let root_cell = module.state_roots[state_idx as usize];
                    let key_value = self.force_reg(&mut regs, key_reg);
                    let pmap_ty = module.state_types[state_idx as usize].clone();
                    let (key_ty, value_ty) = match &pmap_ty {
                        crate::ast::Type::PMap { key, value } => ((**key).clone(), (**value).clone()),
                        _ => unreachable!("PMapContains on non-pmap state"),
                    };
                    let root_v = self.tx.read_cell(root_cell, &pmap_ty);
                    let root_v = self.tx.force(root_v);
                    let root_hash = match root_v {
                        Value::PMap(h) => h,
                        _ => crate::pmap::EMPTY,
                    };
                    regs[dst as usize] = Value::Bool(
                        crate::pmap::contains(root_hash, &key_value, &mut self.tx, &key_ty, &value_ty)
                    );
                }
                Instr::PVecGet { dst, state_idx, idx_reg } => {
                    let root_cell = module.state_roots[state_idx as usize];
                    let idx_v = self.force_reg(&mut regs, idx_reg);
                    let pvec_ty = module.state_types[state_idx as usize].clone();
                    let elem_ty = match &pvec_ty {
                        crate::ast::Type::PVec { elem } => (**elem).clone(),
                        _ => unreachable!("PVecGet on non-pvec state"),
                    };
                    self.tx.record_pvec_type(root_cell, elem_ty.clone());
                    let cell_v = self.tx.read_cell(root_cell, &pvec_ty);
                    let cell_v = self.tx.force(cell_v);
                    let (len, root) = match cell_v {
                        Value::PVec { len, root } => (len, root),
                        _ => (0, crate::pvec::EMPTY),
                    };
                    let i = match idx_v {
                        Value::Int(n) if n >= 0 => n as u64,
                        Value::Int(n) => return Err(Error::new(
                            ErrorKind::Runtime,
                            format!("pvec index must be non-negative, got {n}"),
                            Span::default(),
                        )),
                        Value::U64(n) => n,
                        other => return Err(Error::new(
                            ErrorKind::Runtime,
                            format!("pvec index must be i64 or u64, got {other}"),
                            Span::default(),
                        )),
                    };
                    regs[dst as usize] = crate::pvec::get(root, len, i, &mut self.tx, &elem_ty, Span::default())?;
                }
                Instr::PVecSet { state_idx, idx_reg, src } => {
                    let root_cell = module.state_roots[state_idx as usize];
                    let idx_v = self.force_reg(&mut regs, idx_reg);
                    let value = self.force_reg(&mut regs, src);
                    let pvec_ty = module.state_types[state_idx as usize].clone();
                    let elem_ty = match &pvec_ty {
                        crate::ast::Type::PVec { elem } => (**elem).clone(),
                        _ => unreachable!("PVecSet on non-pvec state"),
                    };
                    self.tx.record_pvec_type(root_cell, elem_ty.clone());
                    let cell_v = self.tx.read_cell(root_cell, &pvec_ty);
                    let cell_v = self.tx.force(cell_v);
                    let (len, root) = match cell_v {
                        Value::PVec { len, root } => (len, root),
                        _ => (0, crate::pvec::EMPTY),
                    };
                    let i = match idx_v {
                        Value::Int(n) if n >= 0 => n as u64,
                        other => return Err(Error::new(
                            ErrorKind::Runtime,
                            format!("pvec index: expected non-negative i64, got {other}"),
                            Span::default(),
                        )),
                    };
                    let new_root = crate::pvec::set(root, len, i, value, &mut self.tx, &elem_ty, Span::default())?;
                    self.tx.write(root_cell, Value::PVec { len, root: new_root });
                }
                Instr::PVecPush { dst, state_idx, src } => {
                    let root_cell = module.state_roots[state_idx as usize];
                    let value = self.force_reg(&mut regs, src);
                    let pvec_ty = module.state_types[state_idx as usize].clone();
                    let elem_ty = match &pvec_ty {
                        crate::ast::Type::PVec { elem } => (**elem).clone(),
                        _ => unreachable!("PVecPush on non-pvec state"),
                    };
                    self.tx.record_pvec_type(root_cell, elem_ty.clone());
                    let cell_v = self.tx.read_cell(root_cell, &pvec_ty);
                    let cell_v = self.tx.force(cell_v);
                    let (len, root) = match cell_v {
                        Value::PVec { len, root } => (len, root),
                        _ => (0, crate::pvec::EMPTY),
                    };
                    let (new_root, new_len) = crate::pvec::push(root, len, value, &mut self.tx, &elem_ty)?;
                    self.tx.write(root_cell, Value::PVec { len: new_len, root: new_root });
                    // Returns the index assigned to the new element.
                    regs[dst as usize] = Value::U64(len);
                }
                Instr::PVecLen { dst, state_idx } => {
                    let root_cell = module.state_roots[state_idx as usize];
                    let pvec_ty = module.state_types[state_idx as usize].clone();
                    let cell_v = self.tx.read_cell(root_cell, &pvec_ty);
                    let cell_v = self.tx.force(cell_v);
                    let len = match cell_v {
                        Value::PVec { len, .. } => len,
                        _ => 0,
                    };
                    regs[dst as usize] = Value::U64(len);
                }
                Instr::PMapEntries { dst, state_idx } => {
                    let entries = pmap_walk_entries(self, module, state_idx);
                    regs[dst as usize] = Value::Array(
                        entries.into_iter()
                            .map(|(k, v)| Value::Tuple(vec![k, v]))
                            .collect(),
                    );
                }
                Instr::PMapKeys { dst, state_idx } => {
                    let entries = pmap_walk_entries(self, module, state_idx);
                    regs[dst as usize] = Value::Array(
                        entries.into_iter().map(|(k, _)| k).collect(),
                    );
                }
                Instr::PMapValues { dst, state_idx } => {
                    let entries = pmap_walk_entries(self, module, state_idx);
                    regs[dst as usize] = Value::Array(
                        entries.into_iter().map(|(_, v)| v).collect(),
                    );
                }
                Instr::PMapPutUnique { state_idx, key_reg, src } => {
                    // Unique-index maintenance: read first, abort
                    // if a different value is already there for
                    // this key. Idempotent for the same (k, v).
                    let root_cell = module.state_roots[state_idx as usize];
                    let pmap_ty = module.state_types[state_idx as usize].clone();
                    let (key_ty, value_ty) = match &pmap_ty {
                        crate::ast::Type::PMap { key, value } => ((**key).clone(), (**value).clone()),
                        _ => unreachable!("PMapPutUnique on non-pmap state"),
                    };
                    let key_value = self.force_reg(&mut regs, key_reg);
                    let new_value = self.force_reg(&mut regs, src);
                    let root_v = self.tx.read_cell(root_cell, &pmap_ty);
                    let root_v = self.tx.force(root_v);
                    let root_hash = match root_v {
                        Value::PMap(h) => h,
                        _ => crate::pmap::EMPTY,
                    };
                    let existing = crate::pmap::get(
                        root_hash, &key_value, &mut self.tx, &key_ty, &value_ty,
                    );
                    if let Some(prior) = existing {
                        if prior != new_value {
                            return Err(Error::new(
                                ErrorKind::Runtime,
                                format!(
                                    "unique constraint violation: key {key_value} already maps to {prior}, refused to remap to {new_value}",
                                ),
                                Span::default(),
                            ));
                        }
                        // Same value as before — idempotent, skip the write.
                    } else {
                        let new_root = crate::pmap::set(
                            root_hash, key_value, new_value,
                            &mut self.tx, &key_ty, &value_ty,
                        )?;
                        self.tx.write(root_cell, Value::PMap(new_root));
                    }
                }
                Instr::PMapAppendUnique { state_idx, key_reg, elem_reg } => {
                    // Multi-index maintenance: read pmap[key] (an
                    // array), append elem if not already present,
                    // write back. Same shape as PMapPut but with
                    // dedup semantics for the array value.
                    let root_cell = module.state_roots[state_idx as usize];
                    let pmap_ty = module.state_types[state_idx as usize].clone();
                    let (key_ty, value_ty) = match &pmap_ty {
                        crate::ast::Type::PMap { key, value } => ((**key).clone(), (**value).clone()),
                        _ => unreachable!("PMapAppendUnique on non-pmap state"),
                    };
                    let key_value = self.force_reg(&mut regs, key_reg);
                    let elem_value = self.force_reg(&mut regs, elem_reg);
                    let root_v = self.tx.read_cell(root_cell, &pmap_ty);
                    let root_v = self.tx.force(root_v);
                    let root_hash = match root_v {
                        Value::PMap(h) => h,
                        _ => crate::pmap::EMPTY,
                    };
                    let existing = crate::pmap::get(
                        root_hash, &key_value, &mut self.tx, &key_ty, &value_ty,
                    );
                    let mut arr = match existing {
                        Some(Value::Array(a)) => a,
                        _ => Vec::new(),
                    };
                    if !arr.iter().any(|e| e == &elem_value) {
                        arr.push(elem_value);
                        let new_root = crate::pmap::set(
                            root_hash, key_value, Value::Array(arr),
                            &mut self.tx, &key_ty, &value_ty,
                        )?;
                        self.tx.write(root_cell, Value::PMap(new_root));
                    }
                }
                Instr::PBTreeGet { dst, state_idx, key_reg } => {
                    let root_cell = module.state_roots[state_idx as usize];
                    let key_value = self.force_reg(&mut regs, key_reg);
                    let pbtree_ty = module.state_types[state_idx as usize].clone();
                    let (key_ty, value_ty) = match &pbtree_ty {
                        crate::ast::Type::PBTree { key, value } => ((**key).clone(), (**value).clone()),
                        _ => unreachable!("PBTreeGet on non-pbtree state"),
                    };
                    let root_v = self.tx.read_cell(root_cell, &pbtree_ty);
                    let root_v = self.tx.force(root_v);
                    let root_hash = match root_v {
                        Value::PBTree(h) => h,
                        _ => crate::pbtree::EMPTY,
                    };
                    let v = crate::pbtree::get(root_hash, &key_value, &mut self.tx, &key_ty, &value_ty)
                        .unwrap_or_else(|| Value::default_for(&value_ty));
                    regs[dst as usize] = v;
                }
                Instr::PBTreePut { state_idx, key_reg, src } => {
                    let root_cell = module.state_roots[state_idx as usize];
                    let key_value = self.force_reg(&mut regs, key_reg);
                    let value     = self.force_reg(&mut regs, src);
                    let pbtree_ty = module.state_types[state_idx as usize].clone();
                    let (key_ty, value_ty) = match &pbtree_ty {
                        crate::ast::Type::PBTree { key, value } => ((**key).clone(), (**value).clone()),
                        _ => unreachable!("PBTreePut on non-pbtree state"),
                    };
                    let root_v = self.tx.read_cell(root_cell, &pbtree_ty);
                    let root_v = self.tx.force(root_v);
                    let root_hash = match root_v {
                        Value::PBTree(h) => h,
                        _ => crate::pbtree::EMPTY,
                    };
                    let new_root = crate::pbtree::set(root_hash, key_value, value, &mut self.tx, &key_ty, &value_ty)?;
                    self.tx.write(root_cell, Value::PBTree(new_root));
                }
                Instr::PBTreeContains { dst, state_idx, key_reg } => {
                    let root_cell = module.state_roots[state_idx as usize];
                    let key_value = self.force_reg(&mut regs, key_reg);
                    let pbtree_ty = module.state_types[state_idx as usize].clone();
                    let (key_ty, value_ty) = match &pbtree_ty {
                        crate::ast::Type::PBTree { key, value } => ((**key).clone(), (**value).clone()),
                        _ => unreachable!("PBTreeContains on non-pbtree state"),
                    };
                    let root_v = self.tx.read_cell(root_cell, &pbtree_ty);
                    let root_v = self.tx.force(root_v);
                    let root_hash = match root_v {
                        Value::PBTree(h) => h,
                        _ => crate::pbtree::EMPTY,
                    };
                    regs[dst as usize] = Value::Bool(
                        crate::pbtree::contains(root_hash, &key_value, &mut self.tx, &key_ty, &value_ty),
                    );
                }
                Instr::PBTreeRange { dst, state_idx, lo_reg, hi_reg } => {
                    let root_cell = module.state_roots[state_idx as usize];
                    let lo_v = self.force_reg(&mut regs, lo_reg);
                    let hi_v = self.force_reg(&mut regs, hi_reg);
                    let pbtree_ty = module.state_types[state_idx as usize].clone();
                    let (key_ty, value_ty) = match &pbtree_ty {
                        crate::ast::Type::PBTree { key, value } => ((**key).clone(), (**value).clone()),
                        _ => unreachable!("PBTreeRange on non-pbtree state"),
                    };
                    let root_v = self.tx.read_cell(root_cell, &pbtree_ty);
                    let root_v = self.tx.force(root_v);
                    let root_hash = match root_v {
                        Value::PBTree(h) => h,
                        _ => crate::pbtree::EMPTY,
                    };
                    let result = crate::pbtree::range(root_hash, &lo_v, &hi_v, &mut self.tx, &key_ty, &value_ty);
                    regs[dst as usize] = Value::Array(result);
                }
                Instr::PBTreeWalkInit { dst, state_idx } => {
                    let root_cell = module.state_roots[state_idx as usize];
                    let pbtree_ty = module.state_types[state_idx as usize].clone();
                    let (key_ty, value_ty) = match &pbtree_ty {
                        crate::ast::Type::PBTree { key, value } => ((**key).clone(), (**value).clone()),
                        _ => unreachable!("PBTreeWalkInit on non-pbtree state"),
                    };
                    let root_v = self.tx.read_cell(root_cell, &pbtree_ty);
                    let root_v = self.tx.force(root_v);
                    let root_hash = match root_v {
                        Value::PBTree(h) => h,
                        _ => crate::pbtree::EMPTY,
                    };
                    let mut cursor = crate::value::PMapCursor {
                        stack: Vec::new(),
                        pending: Vec::new(),
                        key_ty,
                        value_ty,
                    };
                    if root_hash != crate::pbtree::EMPTY {
                        cursor.stack.push((root_hash, 0));
                    }
                    regs[dst as usize] = Value::PBTreeCursor(Box::new(cursor));
                }
                Instr::PBTreeWalkNext { cursor_reg, value_reg, end_offset } => {
                    let mut owned = std::mem::replace(
                        &mut regs[cursor_reg as usize], Value::Unit,
                    );
                    let cursor = match &mut owned {
                        Value::PBTreeCursor(c) => c.as_mut(),
                        _ => return Err(Error::new(
                            ErrorKind::Runtime,
                            "PBTreeWalkNext: cursor register doesn't hold a pbtree cursor",
                            Span::default(),
                        )),
                    };
                    let next = pbtree_walk_advance(cursor, &mut self.tx)?;
                    regs[cursor_reg as usize] = owned;
                    match next {
                        Some((_, v)) => {
                            regs[value_reg as usize] = v;
                        }
                        None => {
                            pc = (pc as i32 + end_offset) as usize;
                        }
                    }
                }
                Instr::PBTreePutUnique { state_idx, key_reg, src } => {
                    let root_cell = module.state_roots[state_idx as usize];
                    let pbtree_ty = module.state_types[state_idx as usize].clone();
                    let (key_ty, value_ty) = match &pbtree_ty {
                        crate::ast::Type::PBTree { key, value } => ((**key).clone(), (**value).clone()),
                        _ => unreachable!("PBTreePutUnique on non-pbtree state"),
                    };
                    let key_value = self.force_reg(&mut regs, key_reg);
                    let new_value = self.force_reg(&mut regs, src);
                    let root_v = self.tx.read_cell(root_cell, &pbtree_ty);
                    let root_v = self.tx.force(root_v);
                    let root_hash = match root_v {
                        Value::PBTree(h) => h,
                        _ => crate::pbtree::EMPTY,
                    };
                    let existing = crate::pbtree::get(
                        root_hash, &key_value, &mut self.tx, &key_ty, &value_ty,
                    );
                    if let Some(prior) = existing {
                        if prior != new_value {
                            return Err(Error::new(
                                ErrorKind::Runtime,
                                format!(
                                    "unique constraint violation: key {key_value} already maps to {prior}, refused to remap to {new_value}",
                                ),
                                Span::default(),
                            ));
                        }
                    } else {
                        let new_root = crate::pbtree::set(
                            root_hash, key_value, new_value,
                            &mut self.tx, &key_ty, &value_ty,
                        )?;
                        self.tx.write(root_cell, Value::PBTree(new_root));
                    }
                }
                Instr::PBTreeAppendUnique { state_idx, key_reg, elem_reg } => {
                    let root_cell = module.state_roots[state_idx as usize];
                    let pbtree_ty = module.state_types[state_idx as usize].clone();
                    let (key_ty, value_ty) = match &pbtree_ty {
                        crate::ast::Type::PBTree { key, value } => ((**key).clone(), (**value).clone()),
                        _ => unreachable!("PBTreeAppendUnique on non-pbtree state"),
                    };
                    let key_value = self.force_reg(&mut regs, key_reg);
                    let elem_value = self.force_reg(&mut regs, elem_reg);
                    let root_v = self.tx.read_cell(root_cell, &pbtree_ty);
                    let root_v = self.tx.force(root_v);
                    let root_hash = match root_v {
                        Value::PBTree(h) => h,
                        _ => crate::pbtree::EMPTY,
                    };
                    let existing = crate::pbtree::get(
                        root_hash, &key_value, &mut self.tx, &key_ty, &value_ty,
                    );
                    let mut arr = match existing {
                        Some(Value::Array(a)) => a,
                        _ => Vec::new(),
                    };
                    if !arr.iter().any(|e| e == &elem_value) {
                        arr.push(elem_value);
                        let new_root = crate::pbtree::set(
                            root_hash, key_value, Value::Array(arr),
                            &mut self.tx, &key_ty, &value_ty,
                        )?;
                        self.tx.write(root_cell, Value::PBTree(new_root));
                    }
                }
                Instr::PMapDelete { state_idx, key_reg } => {
                    let root_cell = module.state_roots[state_idx as usize];
                    let pmap_ty = module.state_types[state_idx as usize].clone();
                    let (key_ty, value_ty) = match &pmap_ty {
                        crate::ast::Type::PMap { key, value } => ((**key).clone(), (**value).clone()),
                        _ => unreachable!("PMapDelete on non-pmap state"),
                    };
                    let key_value = self.force_reg(&mut regs, key_reg);
                    let root_v = self.tx.read_cell(root_cell, &pmap_ty);
                    let root_v = self.tx.force(root_v);
                    let root_hash = match root_v {
                        Value::PMap(h) => h,
                        _ => crate::pmap::EMPTY,
                    };
                    let (new_root, _removed) = crate::pmap::remove(root_hash, &key_value, &mut self.tx, &key_ty, &value_ty)?;
                    self.tx.write(root_cell, Value::PMap(new_root));
                }
                Instr::PBTreeDelete { state_idx, key_reg } => {
                    let root_cell = module.state_roots[state_idx as usize];
                    let pbtree_ty = module.state_types[state_idx as usize].clone();
                    let (key_ty, value_ty) = match &pbtree_ty {
                        crate::ast::Type::PBTree { key, value } => ((**key).clone(), (**value).clone()),
                        _ => unreachable!("PBTreeDelete on non-pbtree state"),
                    };
                    let key_value = self.force_reg(&mut regs, key_reg);
                    let root_v = self.tx.read_cell(root_cell, &pbtree_ty);
                    let root_v = self.tx.force(root_v);
                    let root_hash = match root_v {
                        Value::PBTree(h) => h,
                        _ => crate::pbtree::EMPTY,
                    };
                    let new_root = crate::pbtree::remove(root_hash, &key_value, &mut self.tx, &key_ty, &value_ty)?;
                    self.tx.write(root_cell, Value::PBTree(new_root));
                }
                Instr::PMapRemoveUnique { state_idx, key_reg, expected_reg } => {
                    // Remove pmap[key] only if it currently maps to
                    // expected_reg's value. Used for unique-index
                    // back-link cleanup on delete: the entry might
                    // have been overwritten by a stale-index update,
                    // in which case it points at someone else and
                    // we shouldn't touch it.
                    let root_cell = module.state_roots[state_idx as usize];
                    let pmap_ty = module.state_types[state_idx as usize].clone();
                    let (key_ty, value_ty) = match &pmap_ty {
                        crate::ast::Type::PMap { key, value } => ((**key).clone(), (**value).clone()),
                        _ => unreachable!("PMapRemoveUnique on non-pmap state"),
                    };
                    let key_value = self.force_reg(&mut regs, key_reg);
                    let expected = self.force_reg(&mut regs, expected_reg);
                    let root_v = self.tx.read_cell(root_cell, &pmap_ty);
                    let root_v = self.tx.force(root_v);
                    let root_hash = match root_v {
                        Value::PMap(h) => h,
                        _ => crate::pmap::EMPTY,
                    };
                    let existing = crate::pmap::get(root_hash, &key_value, &mut self.tx, &key_ty, &value_ty);
                    if matches!(existing, Some(ref v) if v == &expected) {
                        let (new_root, _) = crate::pmap::remove(root_hash, &key_value, &mut self.tx, &key_ty, &value_ty)?;
                        self.tx.write(root_cell, Value::PMap(new_root));
                    }
                }
                Instr::PMapRemoveFromList { state_idx, key_reg, elem_reg } => {
                    // Read pmap[key]'s array, filter out elem_reg,
                    // write back. If the list becomes empty, remove
                    // the entry. Used for multi-index back-link cleanup.
                    let root_cell = module.state_roots[state_idx as usize];
                    let pmap_ty = module.state_types[state_idx as usize].clone();
                    let (key_ty, value_ty) = match &pmap_ty {
                        crate::ast::Type::PMap { key, value } => ((**key).clone(), (**value).clone()),
                        _ => unreachable!("PMapRemoveFromList on non-pmap state"),
                    };
                    let key_value = self.force_reg(&mut regs, key_reg);
                    let elem = self.force_reg(&mut regs, elem_reg);
                    let root_v = self.tx.read_cell(root_cell, &pmap_ty);
                    let root_v = self.tx.force(root_v);
                    let root_hash = match root_v {
                        Value::PMap(h) => h,
                        _ => crate::pmap::EMPTY,
                    };
                    let existing = crate::pmap::get(root_hash, &key_value, &mut self.tx, &key_ty, &value_ty);
                    if let Some(Value::Array(mut arr)) = existing {
                        let before = arr.len();
                        arr.retain(|e| e != &elem);
                        if arr.len() == before {
                            // No-op: elem wasn't in the list.
                        } else if arr.is_empty() {
                            let (new_root, _) = crate::pmap::remove(root_hash, &key_value, &mut self.tx, &key_ty, &value_ty)?;
                            self.tx.write(root_cell, Value::PMap(new_root));
                        } else {
                            let new_root = crate::pmap::set(root_hash, key_value, Value::Array(arr), &mut self.tx, &key_ty, &value_ty)?;
                            self.tx.write(root_cell, Value::PMap(new_root));
                        }
                    }
                }
                Instr::PBTreeRemoveUnique { state_idx, key_reg, expected_reg } => {
                    let root_cell = module.state_roots[state_idx as usize];
                    let pbtree_ty = module.state_types[state_idx as usize].clone();
                    let (key_ty, value_ty) = match &pbtree_ty {
                        crate::ast::Type::PBTree { key, value } => ((**key).clone(), (**value).clone()),
                        _ => unreachable!("PBTreeRemoveUnique on non-pbtree state"),
                    };
                    let key_value = self.force_reg(&mut regs, key_reg);
                    let expected = self.force_reg(&mut regs, expected_reg);
                    let root_v = self.tx.read_cell(root_cell, &pbtree_ty);
                    let root_v = self.tx.force(root_v);
                    let root_hash = match root_v {
                        Value::PBTree(h) => h,
                        _ => crate::pbtree::EMPTY,
                    };
                    let existing = crate::pbtree::get(root_hash, &key_value, &mut self.tx, &key_ty, &value_ty);
                    if matches!(existing, Some(ref v) if v == &expected) {
                        let new_root = crate::pbtree::remove(root_hash, &key_value, &mut self.tx, &key_ty, &value_ty)?;
                        self.tx.write(root_cell, Value::PBTree(new_root));
                    }
                }
                Instr::PBTreeRemoveFromList { state_idx, key_reg, elem_reg } => {
                    let root_cell = module.state_roots[state_idx as usize];
                    let pbtree_ty = module.state_types[state_idx as usize].clone();
                    let (key_ty, value_ty) = match &pbtree_ty {
                        crate::ast::Type::PBTree { key, value } => ((**key).clone(), (**value).clone()),
                        _ => unreachable!("PBTreeRemoveFromList on non-pbtree state"),
                    };
                    let key_value = self.force_reg(&mut regs, key_reg);
                    let elem = self.force_reg(&mut regs, elem_reg);
                    let root_v = self.tx.read_cell(root_cell, &pbtree_ty);
                    let root_v = self.tx.force(root_v);
                    let root_hash = match root_v {
                        Value::PBTree(h) => h,
                        _ => crate::pbtree::EMPTY,
                    };
                    let existing = crate::pbtree::get(root_hash, &key_value, &mut self.tx, &key_ty, &value_ty);
                    if let Some(Value::Array(mut arr)) = existing {
                        let before = arr.len();
                        arr.retain(|e| e != &elem);
                        if arr.len() == before {
                            // no-op
                        } else if arr.is_empty() {
                            let new_root = crate::pbtree::remove(root_hash, &key_value, &mut self.tx, &key_ty, &value_ty)?;
                            self.tx.write(root_cell, Value::PBTree(new_root));
                        } else {
                            let new_root = crate::pbtree::set(root_hash, key_value, Value::Array(arr), &mut self.tx, &key_ty, &value_ty)?;
                            self.tx.write(root_cell, Value::PBTree(new_root));
                        }
                    }
                }
                Instr::PMapWalkInit { dst, state_idx } => {
                    // Initialize a streaming HAMT cursor: read the
                    // root-cell, push the root onto the walk stack,
                    // and stash the cursor in `dst`. Cells are not
                    // fetched until PMapWalkNext descends into them.
                    let root_cell = module.state_roots[state_idx as usize];
                    let pmap_ty = module.state_types[state_idx as usize].clone();
                    let (key_ty, value_ty) = match &pmap_ty {
                        crate::ast::Type::PMap { key, value } => ((**key).clone(), (**value).clone()),
                        _ => unreachable!("PMapWalkInit on non-pmap state"),
                    };
                    let root_v = self.tx.read_cell(root_cell, &pmap_ty);
                    let root_v = self.tx.force(root_v);
                    let root_hash = match root_v {
                        Value::PMap(h) => h,
                        _ => crate::pmap::EMPTY,
                    };
                    let mut cursor = crate::value::PMapCursor {
                        stack: Vec::new(),
                        pending: Vec::new(),
                        key_ty,
                        value_ty,
                    };
                    if root_hash != crate::pmap::EMPTY {
                        cursor.stack.push((root_hash, 0));
                    }
                    regs[dst as usize] = Value::PMapCursor(Box::new(cursor));
                }
                Instr::PMapWalkNext { cursor_reg, value_reg, end_offset } => {
                    // Pop a cursor, advance to the next leaf entry,
                    // emit (or jump if exhausted), then store the
                    // mutated cursor back. The mutation happens
                    // through the boxed state so `regs[cursor_reg]`
                    // doesn't need rewriting.
                    let mut owned = std::mem::replace(
                        &mut regs[cursor_reg as usize], Value::Unit,
                    );
                    let cursor = match &mut owned {
                        Value::PMapCursor(c) => c.as_mut(),
                        _ => return Err(Error::new(
                            ErrorKind::Runtime,
                            "PMapWalkNext: cursor register doesn't hold a cursor",
                            Span::default(),
                        )),
                    };
                    let next = pmap_walk_advance(cursor, &mut self.tx)?;
                    regs[cursor_reg as usize] = owned;
                    match next {
                        Some((_, v)) => {
                            regs[value_reg as usize] = v;
                        }
                        None => {
                            pc = (pc as i32 + end_offset) as usize;
                        }
                    }
                }
                Instr::PVecToArray { dst, state_idx } => {
                    let root_cell = module.state_roots[state_idx as usize];
                    let pvec_ty = module.state_types[state_idx as usize].clone();
                    let elem_ty = match &pvec_ty {
                        crate::ast::Type::PVec { elem } => (**elem).clone(),
                        _ => unreachable!("PVecToArray on non-pvec state"),
                    };
                    self.tx.record_pvec_type(root_cell, elem_ty.clone());
                    let cell_v = self.tx.read_cell(root_cell, &pvec_ty);
                    let cell_v = self.tx.force(cell_v);
                    let (len, root) = match cell_v {
                        Value::PVec { len, root } => (len, root),
                        _ => (0, crate::pvec::EMPTY),
                    };
                    let elements = crate::pvec::to_vec(root, len, &mut self.tx, &elem_ty)?;
                    regs[dst as usize] = Value::Array(elements);
                }
                Instr::IncReg { reg } => {
                    let v = self.force_reg(&mut regs, reg);
                    regs[reg as usize] = match v {
                        Value::Int(n)  => Value::Int(n.checked_add(1).ok_or_else(|| Error::new(
                            ErrorKind::Runtime, "integer overflow", Span::default()))?),
                        Value::I32(n)  => Value::I32(n.checked_add(1).ok_or_else(|| Error::new(
                            ErrorKind::Runtime, "integer overflow", Span::default()))?),
                        Value::U32(n)  => Value::U32(n.checked_add(1).ok_or_else(|| Error::new(
                            ErrorKind::Runtime, "integer overflow", Span::default()))?),
                        Value::U64(n)  => Value::U64(n.checked_add(1).ok_or_else(|| Error::new(
                            ErrorKind::Runtime, "integer overflow", Span::default()))?),
                        Value::U128(n) => Value::U128(n.checked_add(1).ok_or_else(|| Error::new(
                            ErrorKind::Runtime, "integer overflow", Span::default()))?),
                        other => return Err(Error::new(
                            ErrorKind::Runtime,
                            format!("IncReg on non-integer value: {other}"),
                            Span::default(),
                        )),
                    };
                }
            }
        }
    }
}

/// Advance a streaming HAMT cursor by one leaf entry. Returns
/// `Some((key, value))` if there's more to yield, or `None` when
/// the walk is exhausted. Cells are fetched lazily — the cursor's
/// `stack` only descends when the runtime actually needs the next
/// entry, so `break` after a match avoids touching the rest of
/// the tree.
fn pmap_walk_advance(
    cursor: &mut crate::value::PMapCursor,
    tx: &mut Tx<'_>,
) -> Result<Option<(Value, Value)>, Error> {
    loop {
        // Drain pending leaf entries first — every leaf may carry
        // multiple (k, v) pairs (collision chains, multi-entry leaves).
        if let Some(pair) = cursor.pending.pop() {
            return Ok(Some(pair));
        }
        // Otherwise pop the top stack frame and visit it.
        let Some((node_hash, mut next_child)) = cursor.stack.pop() else {
            return Ok(None);
        };
        let node = crate::pmap::read_node(tx, node_hash, &cursor.key_ty, &cursor.value_ty);
        match node {
            None => continue,
            Some(crate::pmap::Node::Leaf { entries }) => {
                // Push entries onto pending in reverse so we yield
                // them in declaration order.
                cursor.pending = entries.into_iter().rev().collect();
            }
            Some(crate::pmap::Node::Inner { children, .. }) => {
                if next_child < children.len() {
                    let child = children[next_child];
                    next_child += 1;
                    // Replace this frame with the advanced index,
                    // then push the child to descend into next.
                    cursor.stack.push((node_hash, next_child));
                    cursor.stack.push((child, 0));
                }
            }
        }
    }
}

/// Advance a streaming sorted-trie cursor by one leaf entry. Same
/// shape as `pmap_walk_advance` but routes through the `pbtree`
/// module so node cells come from the pbtree namespace.
fn pbtree_walk_advance(
    cursor: &mut crate::value::PMapCursor,
    tx: &mut Tx<'_>,
) -> Result<Option<(Value, Value)>, Error> {
    loop {
        if let Some(pair) = cursor.pending.pop() {
            return Ok(Some(pair));
        }
        let Some((node_hash, mut next_child)) = cursor.stack.pop() else {
            return Ok(None);
        };
        let node = crate::pbtree::read_node(tx, node_hash, &cursor.key_ty, &cursor.value_ty);
        match node {
            None => continue,
            Some(crate::pbtree::Node::Leaf { entries }) => {
                // Yield in declaration (sorted) order — push reversed
                // so .pop() returns the first.
                cursor.pending = entries.into_iter().rev().collect();
            }
            Some(crate::pbtree::Node::Inner { children, .. }) => {
                if next_child < children.len() {
                    let child = children[next_child];
                    next_child += 1;
                    cursor.stack.push((node_hash, next_child));
                    cursor.stack.push((child, 0));
                }
            }
        }
    }
}

/// Walk a `pmap` state slot end-to-end, returning every (key, value)
/// pair the HAMT contains. Hash-of-key order — deterministic but not
/// user-meaningful. Shared across `PMapEntries` / `PMapKeys` /
/// `PMapValues` instructions; the only difference between them is
/// what they project from the result.
fn pmap_walk_entries(
    state: &mut VmState<'_, '_>,
    module: &BcModule,
    state_idx: u16,
) -> Vec<(Value, Value)> {
    let root_cell = module.state_roots[state_idx as usize];
    let pmap_ty = module.state_types[state_idx as usize].clone();
    let (key_ty, value_ty) = match &pmap_ty {
        crate::ast::Type::PMap { key, value } => ((**key).clone(), (**value).clone()),
        _ => unreachable!("pmap walk on non-pmap state"),
    };
    let root_v = state.tx.read_cell(root_cell, &pmap_ty);
    let root_v = state.tx.force(root_v);
    let root_hash = match root_v {
        Value::PMap(h) => h,
        _ => crate::pmap::EMPTY,
    };
    crate::pmap::entries(root_hash, &mut state.tx, &key_ty, &value_ty)
}

/// Check that `target` (a loaded module) implements every method
/// declared by `iface`. Returns the first conformance failure as
/// a printable string. Pure logic — no VM state, easy to unit-test.
///
/// Effect bound: the interface declaration is the *upper bound* on
/// what the implementer is allowed to do, so the implementer's
/// classification must be at-least-as-strict:
///   * `iface pure`    ⇒  impl must be pure.
///   * `iface view`    ⇒  impl must be view OR pure.
///   * `iface unbound` ⇒  no constraint.
pub fn check_iface_conformance(
    iface: &crate::ast::InterfaceDecl,
    target: &BcModule,
) -> Result<(), String> {
    for method in &iface.methods {
        let fn_idx = target.fn_index.get(&method.name).ok_or_else(|| {
            format!(
                "interface '{}' requires method '{}' but module '{}' does not define it",
                iface.name, method.name, target.name,
            )
        })?;
        let f = &target.functions[*fn_idx];
        if !f.is_entry {
            return Err(format!(
                "interface '{}' requires method '{}' to be entry-callable, but '{}::{}' is not declared `entry`",
                iface.name, method.name, target.name, method.name,
            ));
        }
        if f.n_params as usize != method.params.len() {
            return Err(format!(
                "interface '{}' method '{}' takes {} param(s), but '{}::{}' takes {}",
                iface.name, method.name, method.params.len(),
                target.name, method.name, f.n_params,
            ));
        }
        for (i, want) in method.params.iter().enumerate() {
            let got = &f.param_types[i];
            if got != &want.ty {
                return Err(format!(
                    "interface '{}' method '{}' param {} expects {:?}, but '{}::{}' has {:?}",
                    iface.name, method.name, i, want.ty,
                    target.name, method.name, got,
                ));
            }
        }
        if f.return_type != method.return_type {
            return Err(format!(
                "interface '{}' method '{}' returns {:?}, but '{}::{}' returns {:?}",
                iface.name, method.name, method.return_type,
                target.name, method.name, f.return_type,
            ));
        }
        if method.is_pure && !f.is_pure {
            return Err(format!(
                "interface '{}' declares method '{}' pure, but '{}::{}' is not pure",
                iface.name, method.name, target.name, method.name,
            ));
        }
        if method.is_view && !(f.is_view || f.is_pure) {
            return Err(format!(
                "interface '{}' declares method '{}' view, but '{}::{}' is neither view nor pure",
                iface.name, method.name, target.name, method.name,
            ));
        }
    }
    Ok(())
}

fn compose_map_cell(root: u128, key: &Value) -> Result<u128, Error> {
    if !is_keyable_value(key) {
        return Err(Error::new(
            ErrorKind::Runtime,
            format!("map key value not keyable: {key}"),
            Span::default(),
        ));
    }
    let bytes = crate::serialize::serialize(key);
    Ok(crate::hashing::child(root, &bytes))
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

pub fn eval_bin(op: BinOp, l: Value, r: Value) -> Result<Value, Error> {
    crate::ops::eval_binary(op, l, r, Span::default())
}

#[allow(dead_code)]
pub fn eval_un(op: UnOp, v: Value) -> Result<Value, Error> {
    crate::ops::eval_unary(op, v, Span::default())
}
