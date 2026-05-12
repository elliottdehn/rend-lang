//! Static type checker.
//!
//! Slice 2: monomorphic types only — `i64`, `bool`, `()`. Function signatures
//! are required (params + return). Locals are inferred from their initializer.
//! Functions whose return type is non-`()` must `return` on every path.

use std::collections::HashMap;

use crate::ast::*;
use crate::error::{Error, ErrorKind};
use crate::token::Span;

/// Resolved struct info threaded through type resolution: parallel
/// arrays of `(field_name, field_type)` and per-field group names
/// (`None` = ungrouped granular cell, `Some(g)` = shares cell with
/// siblings under group `g`). Used to materialize `Type::Struct`
/// values when a struct name is encountered in any type position.
type StructInfo = (Vec<(String, Type)>, Vec<Option<String>>);

/// Canonical form: when every field is ungrouped, store the empty
/// vec rather than `[None, None, ...]`. Keeps `PartialEq` on
/// `Type::Struct` stable between the parse-time default and the
/// typeck-resolved form.
fn normalize_groups(groups: Vec<Option<String>>) -> Vec<Option<String>> {
    if groups.iter().all(Option::is_none) {
        Vec::new()
    } else {
        groups
    }
}

/// Cross-module struct catalog: keyed by `"module::name"`, value is
/// `(fields, field_groups, is_pub)`. Built in the multi-module
/// pipeline (`Engine::execute_main`) from every parsed module and
/// handed to typeck so `m::T` references can resolve their fields
/// and visibility from outside the local module.
pub type CrossModuleStructs = HashMap<String, (Vec<(String, Type)>, Vec<Option<String>>, bool)>;

/// Walk the module and replace every unresolved `Type::Struct { name, fields: [] }`
/// with a fully-populated `Type::Struct { name, fields: ... }` taken from the
/// matching declaration. Struct decls are processed in declaration order,
/// so each may reference structs declared before it. Cycles are detected.
pub fn resolve_types(module: &mut Module) -> Result<(), Error> {
    resolve_types_with_externals(module, &HashMap::new(), &HashMap::new())
}

/// Same as `resolve_types`, but also populates the iface map
/// with declarations from dependency artifacts. compile_tx
/// passes its deps' interfaces so the tx source can use them
/// as type names.
pub fn resolve_types_with_iface_externals(
    module: &mut Module,
    iface_externals: &HashMap<String, Vec<crate::ast::InterfaceMethodSig>>,
) -> Result<(), Error> {
    resolve_types_with_externals(module, &HashMap::new(), iface_externals)
}

/// Same as `resolve_types_with_iface_externals`, but also takes a
/// cross-module struct table so qualified `m::T` references resolve.
pub fn resolve_types_with_externals(
    module: &mut Module,
    cross_structs: &CrossModuleStructs,
    iface_externals: &HashMap<String, Vec<crate::ast::InterfaceMethodSig>>,
) -> Result<(), Error> {
    use std::collections::HashMap;
    let owner_module = module.name.clone().unwrap_or_else(|| "main".to_string());
    let mut struct_map: HashMap<String, StructInfo> = HashMap::new();
    // Pre-load cross-module `pub` structs under their fully
    // qualified `module::name` key. The parser lowers
    // `m::T` references as `Type::Struct.name = "m::T"`, so the
    // existing struct-name lookup picks them up without further
    // plumbing. Private structs are filtered out here — that's
    // the `pub` enforcement boundary.
    for (qname, (fields, groups, is_pub)) in cross_structs {
        if *is_pub {
            struct_map.insert(qname.clone(), (fields.clone(), groups.clone()));
        }
    }
    for decl in module.structs.iter_mut() {
        let mut resolved = Vec::with_capacity(decl.fields.len());
        let mut groups = Vec::with_capacity(decl.fields.len());
        for f in &mut decl.fields {
            f.ty = resolve_one(&f.ty, &struct_map, decl.span)?;
            resolved.push((f.name.clone(), f.ty.clone()));
            groups.push(f.group.clone());
        }
        if struct_map.contains_key(&decl.name) {
            return Err(Error::new(
                ErrorKind::Type,
                format!("duplicate struct '{}'", decl.name),
                decl.span,
            ));
        }
        struct_map.insert(decl.name.clone(), (resolved, normalize_groups(groups)));
    }
    // Resolve enum payload types — they may reference structs that
    // were resolved above.
    let mut enum_map: HashMap<String, Vec<(String, Vec<Type>)>> = HashMap::new();
    for decl in module.enums.iter_mut() {
        let mut resolved = Vec::with_capacity(decl.variants.len());
        for v in &mut decl.variants {
            for t in &mut v.payload {
                *t = resolve_one_with_enums(t, &struct_map, &enum_map, &HashMap::new(), &HashMap::new(), v.span)?;
            }
            resolved.push((v.name.clone(), v.payload.clone()));
        }
        if enum_map.contains_key(&decl.name) {
            return Err(Error::new(
                ErrorKind::Type,
                format!("duplicate enum '{}'", decl.name),
                decl.span,
            ));
        }
        enum_map.insert(decl.name.clone(), resolved);
    }
    // Cap field types may reference structs/enums (resolved above) but
    // not other caps. Build the cap map after structs+enums; cap fields
    // resolve once here.
    let empty_iface_map: HashMap<String, Vec<crate::ast::InterfaceMethodSig>> = HashMap::new();
    let mut cap_map: HashMap<String, (Vec<(String, Type)>, String)> = HashMap::new();
    for decl in module.caps.iter_mut() {
        let mut resolved = Vec::with_capacity(decl.fields.len());
        for f in &mut decl.fields {
            f.ty = resolve_one_with_enums(&f.ty, &struct_map, &enum_map, &HashMap::new(), &empty_iface_map, decl.span)?;
            resolved.push((f.name.clone(), f.ty.clone()));
        }
        if cap_map.contains_key(&decl.name) || struct_map.contains_key(&decl.name) || enum_map.contains_key(&decl.name) {
            return Err(Error::new(
                ErrorKind::Type,
                format!("duplicate type '{}'", decl.name),
                decl.span,
            ));
        }
        cap_map.insert(decl.name.clone(), (resolved, owner_module.clone()));
    }
    // Interface methods may reference structs/enums/caps already
    // resolved above. Like caps, they can't reference other
    // interfaces transitively (no recursive interface use).
    // Seed with cross-artifact interfaces. Local decls below
    // override these on collision (a tx that re-declares a dep
    // interface uses its own version), but otherwise dep
    // interfaces are visible as type names.
    let mut iface_map: HashMap<String, Vec<crate::ast::InterfaceMethodSig>> =
        iface_externals.clone();
    for decl in module.interfaces.iter_mut() {
        let mut sigs = Vec::with_capacity(decl.methods.len());
        for m in &mut decl.methods {
            for p in &mut m.params {
                p.ty = resolve_one_with_enums(&p.ty, &struct_map, &enum_map, &cap_map, &empty_iface_map, p.span)?;
            }
            m.return_type = resolve_one_with_enums(&m.return_type, &struct_map, &enum_map, &cap_map, &empty_iface_map, m.span)?;
            sigs.push(crate::ast::InterfaceMethodSig {
                name: m.name.clone(),
                params: m.params.iter().map(|p| p.ty.clone()).collect(),
                return_type: m.return_type.clone(),
                is_view: m.is_view,
                is_pure: m.is_pure,
            });
        }
        if iface_map.contains_key(&decl.name)
            || struct_map.contains_key(&decl.name)
            || enum_map.contains_key(&decl.name)
            || cap_map.contains_key(&decl.name)
        {
            return Err(Error::new(
                ErrorKind::Type,
                format!("duplicate type '{}'", decl.name),
                decl.span,
            ));
        }
        iface_map.insert(decl.name.clone(), sigs);
    }
    // Second pass over structs: now that enum_map and cap_map are
    // complete, re-resolve any struct field whose type referenced an
    // enum or a cap.
    for decl in module.structs.iter_mut() {
        let mut updated = Vec::with_capacity(decl.fields.len());
        let mut groups = Vec::with_capacity(decl.fields.len());
        for f in &mut decl.fields {
            f.ty = resolve_one_with_enums(&f.ty, &struct_map, &enum_map, &cap_map, &iface_map, decl.span)?;
            updated.push((f.name.clone(), f.ty.clone()));
            groups.push(f.group.clone());
        }
        struct_map.insert(decl.name.clone(), (updated, normalize_groups(groups)));
    }
    for s in module.states.iter_mut() {
        s.ty = resolve_one_with_enums(&s.ty, &struct_map, &enum_map, &cap_map, &iface_map, s.span)?;
    }
    for f in module.functions.iter_mut() {
        for p in f.params.iter_mut() {
            p.ty = resolve_one_with_enums(&p.ty, &struct_map, &enum_map, &cap_map, &iface_map, p.span)?;
        }
        f.return_type = resolve_one_with_enums(&f.return_type, &struct_map, &enum_map, &cap_map, &iface_map, f.span)?;
        resolve_block_with_enums(&mut f.body, &struct_map, &enum_map, &cap_map, &iface_map)?;
    }
    // Handler fn_defs live in `module.functions` (see parser) so the
    // loop above already resolved their bodies. The standalone
    // entries in `module.handlers` are only the dispatch table.
    for imp in module.imports.iter_mut() {
        for p in imp.params.iter_mut() {
            *p = resolve_one_with_enums(p, &struct_map, &enum_map, &cap_map, &iface_map, imp.span)?;
        }
        imp.return_type = resolve_one_with_enums(&imp.return_type, &struct_map, &enum_map, &cap_map, &iface_map, imp.span)?;
    }
    for c in module.consts.iter_mut() {
        c.ty = resolve_one_with_enums(&c.ty, &struct_map, &enum_map, &cap_map, &iface_map, c.span)?;
    }
    Ok(())
}

fn resolve_one_with_enums(
    ty: &Type,
    structs: &HashMap<String, StructInfo>,
    enums: &HashMap<String, Vec<(String, Vec<Type>)>>,
    caps: &HashMap<String, (Vec<(String, Type)>, String)>,
    ifaces: &HashMap<String, Vec<crate::ast::InterfaceMethodSig>>,
    span: crate::token::Span,
) -> Result<Type, Error> {
    match ty {
        Type::Enum { name, variants } if variants.is_empty() => {
            let v = enums.get(name).ok_or_else(|| {
                Error::new(ErrorKind::Type, format!("unknown enum '{name}'"), span)
            })?;
            Ok(Type::Enum { name: name.clone(), variants: v.clone() })
        }
        Type::Enum { .. } => Ok(ty.clone()),
        // Interface names arrive from the parser as
        // `Type::Interface { name, methods: [] }`. Inline the
        // method sigs here so downstream typeck has everything.
        Type::Interface { name, methods } if methods.is_empty() => {
            let m = ifaces.get(name).ok_or_else(|| {
                Error::new(ErrorKind::Type, format!("unknown interface '{name}'"), span)
            })?;
            Ok(Type::Interface { name: name.clone(), methods: m.clone() })
        }
        Type::Interface { .. } => Ok(ty.clone()),
        // Cap names arrive from the parser as `Type::Struct { name, fields: [] }`
        // because cap names share the struct first-pass set. Resolve them
        // to `Type::Cap` *before* the struct fallback.
        Type::Struct { name, fields, field_groups: _ } if fields.is_empty() && caps.contains_key(name) => {
            let (cap_fields, owner) = &caps[name];
            Ok(Type::Cap {
                name: name.clone(),
                fields: cap_fields.clone(),
                owner_module: owner.clone(),
            })
        }
        Type::Array(elem) => Ok(Type::Array(Box::new(resolve_one_with_enums(elem, structs, enums, caps, ifaces, span)?))),
        Type::Map { key, value } => Ok(Type::Map {
            key: Box::new(resolve_one_with_enums(key, structs, enums, caps, ifaces, span)?),
            value: Box::new(resolve_one_with_enums(value, structs, enums, caps, ifaces, span)?),
        }),
        Type::PMap { key, value } => Ok(Type::PMap {
            key: Box::new(resolve_one_with_enums(key, structs, enums, caps, ifaces, span)?),
            value: Box::new(resolve_one_with_enums(value, structs, enums, caps, ifaces, span)?),
        }),
        Type::PBTree { key, value } => Ok(Type::PBTree {
            key: Box::new(resolve_one_with_enums(key, structs, enums, caps, ifaces, span)?),
            value: Box::new(resolve_one_with_enums(value, structs, enums, caps, ifaces, span)?),
        }),
        Type::PVec { elem } => Ok(Type::PVec {
            elem: Box::new(resolve_one_with_enums(elem, structs, enums, caps, ifaces, span)?),
        }),
        Type::Tuple(elems) => {
            let resolved: Result<Vec<_>, _> = elems
                .iter()
                .map(|t| resolve_one_with_enums(t, structs, enums, caps, ifaces, span))
                .collect();
            Ok(Type::Tuple(resolved?))
        }
        _ => resolve_one(ty, structs, span),
    }
}

fn resolve_block_with_enums(
    block: &mut Block,
    structs: &HashMap<String, StructInfo>,
    enums: &HashMap<String, Vec<(String, Vec<Type>)>>,
    caps: &HashMap<String, (Vec<(String, Type)>, String)>,
    ifaces: &HashMap<String, Vec<crate::ast::InterfaceMethodSig>>,
) -> Result<(), Error> {
    for stmt in block.stmts.iter_mut() {
        resolve_stmt_with_enums(stmt, structs, enums, caps, ifaces)?;
    }
    Ok(())
}

fn resolve_stmt_with_enums(
    stmt: &mut Stmt,
    structs: &HashMap<String, StructInfo>,
    enums: &HashMap<String, Vec<(String, Vec<Type>)>>,
    caps: &HashMap<String, (Vec<(String, Type)>, String)>,
    ifaces: &HashMap<String, Vec<crate::ast::InterfaceMethodSig>>,
) -> Result<(), Error> {
    match stmt {
        Stmt::Let { ty: Some(t), span, .. } => {
            *t = resolve_one_with_enums(t, structs, enums, caps, ifaces, *span)?;
        }
        Stmt::If(if_stmt) => resolve_if_with_enums(if_stmt, structs, enums, caps, ifaces)?,
        Stmt::While { body, .. } | Stmt::For { body, .. } | Stmt::ForRange { body, .. } => {
            resolve_block_with_enums(body, structs, enums, caps, ifaces)?;
        }
        _ => {}
    }
    Ok(())
}

fn resolve_if_with_enums(
    ifs: &mut IfStmt,
    structs: &HashMap<String, StructInfo>,
    enums: &HashMap<String, Vec<(String, Vec<Type>)>>,
    caps: &HashMap<String, (Vec<(String, Type)>, String)>,
    ifaces: &HashMap<String, Vec<crate::ast::InterfaceMethodSig>>,
) -> Result<(), Error> {
    resolve_block_with_enums(&mut ifs.then, structs, enums, caps, ifaces)?;
    match &mut ifs.else_branch {
        ElseBranch::None => {}
        ElseBranch::Block(b) => resolve_block_with_enums(b, structs, enums, caps, ifaces)?,
        ElseBranch::If(inner) => resolve_if_with_enums(inner, structs, enums, caps, ifaces)?,
    }
    Ok(())
}

// (resolve_block / resolve_stmt / resolve_if were superseded by their
// `_with_enums` variants when sum types landed; the enum-aware
// helpers handle struct types correctly via the inner resolve_one.)

fn resolve_one(
    ty: &Type,
    structs: &std::collections::HashMap<String, StructInfo>,
    span: crate::token::Span,
) -> Result<Type, Error> {
    match ty {
        Type::Struct { name, fields, field_groups: _ } if fields.is_empty() => {
            let (fields, groups) = structs.get(name).ok_or_else(|| {
                Error::new(
                    ErrorKind::Type,
                    format!("unknown struct '{name}'"),
                    span,
                )
            })?;
            Ok(Type::Struct {
                name: name.clone(),
                fields: fields.clone(),
                field_groups: groups.clone(),
            })
        }
        Type::Struct { .. } => Ok(ty.clone()),
        Type::Array(elem) => Ok(Type::Array(Box::new(resolve_one(elem, structs, span)?))),
        Type::Map { key, value } => Ok(Type::Map {
            key: Box::new(resolve_one(key, structs, span)?),
            value: Box::new(resolve_one(value, structs, span)?),
        }),
        Type::PMap { key, value } => Ok(Type::PMap {
            key: Box::new(resolve_one(key, structs, span)?),
            value: Box::new(resolve_one(value, structs, span)?),
        }),
        Type::PBTree { key, value } => Ok(Type::PBTree {
            key: Box::new(resolve_one(key, structs, span)?),
            value: Box::new(resolve_one(value, structs, span)?),
        }),
        Type::PVec { elem } => Ok(Type::PVec {
            elem: Box::new(resolve_one(elem, structs, span)?),
        }),
        Type::Tuple(elems) => {
            let resolved: Result<Vec<_>, _> = elems
                .iter()
                .map(|t| resolve_one(t, structs, span))
                .collect();
            Ok(Type::Tuple(resolved?))
        }
        // Interfaces require an iface map for resolution.
        // `resolve_one` is the simpler helper that doesn't carry
        // it; an unresolved interface here is a programmer error.
        Type::Interface { name, methods } if methods.is_empty() => Err(Error::new(
            ErrorKind::Type,
            format!("interface '{name}' referenced where iface map isn't available"),
            span,
        )),
        other => Ok(other.clone()),
    }
}

pub fn check(module: &Module) -> Result<(), Error> {
    check_with_externals(module, HashMap::new())
}

/// Walk every function body and stamp each `DynCall` node with
/// the called interface method's `view`/`pure` flags. The
/// optimizer (and the effects classifier in the future) reads
/// these to decide whether a dynamic call breaks a read cluster
/// or counts as a side effect.
///
/// We track local types via let-bindings using a small
/// scope stack that recognizes:
///   * `let name: T = ...` (explicit annotation)
///   * `let name = IFace::bind(...)` (constructor pattern)
///   * `let name = state_ident` (copy from a state slot)
///   * `let name = some_local_ident` (alias of a local already
///     in scope)
///
/// Anything more elaborate (e.g., `let t = factory_fn();`) leaves
/// the local untyped, so its DynCall flags stay conservative —
/// the optimizer treats those as fences. State-bound and the
/// patterns above all succeed.
pub fn annotate_dyn_calls(module: &mut Module) {
    annotate_dyn_calls_with_extras(module, &HashMap::new());
}

/// Same as `annotate_dyn_calls` but also recognizes the
/// `IFace::bind(...)` pattern for `IFace`s declared in dep
/// artifacts. compile_tx passes its deps' interfaces.
pub fn annotate_dyn_calls_with_extras(
    module: &mut Module,
    iface_externals: &HashMap<String, Vec<crate::ast::InterfaceMethodSig>>,
) {
    let state_types: HashMap<String, Type> = module
        .states
        .iter()
        .map(|s| (s.name.clone(), s.ty.clone()))
        .collect();
    // Iface name -> declared methods. Used to recognize the
    // `IFace::bind(...)` pattern and synthesize an Interface
    // type for the bound local. Local decls override extras
    // on collision.
    let mut interfaces: HashMap<String, Vec<crate::ast::InterfaceMethodSig>> =
        iface_externals.clone();
    for d in &module.interfaces {
        interfaces.insert(
            d.name.clone(),
            d.methods.iter().map(|m| crate::ast::InterfaceMethodSig {
                name: m.name.clone(),
                params: m.params.iter().map(|p| p.ty.clone()).collect(),
                return_type: m.return_type.clone(),
                is_view: m.is_view,
                is_pure: m.is_pure,
            }).collect(),
        );
    }
    for f in module.functions.iter_mut() {
        let mut scopes: Vec<HashMap<String, Type>> = vec![HashMap::new()];
        // Seed the initial scope with parameter types.
        for p in &f.params {
            scopes[0].insert(p.name.clone(), p.ty.clone());
        }
        annotate_block(&mut f.body, &mut scopes, &state_types, &interfaces);
    }
}

fn annotate_block(
    block: &mut Block,
    scopes: &mut Vec<HashMap<String, Type>>,
    state_types: &HashMap<String, Type>,
    ifaces: &HashMap<String, Vec<crate::ast::InterfaceMethodSig>>,
) {
    scopes.push(HashMap::new());
    for stmt in block.stmts.iter_mut() {
        annotate_stmt(stmt, scopes, state_types, ifaces);
    }
    if let Some(t) = block.tail.as_mut() {
        annotate_expr(t, scopes, state_types, ifaces);
    }
    scopes.pop();
}

fn annotate_stmt(
    stmt: &mut Stmt,
    scopes: &mut Vec<HashMap<String, Type>>,
    state_types: &HashMap<String, Type>,
    ifaces: &HashMap<String, Vec<crate::ast::InterfaceMethodSig>>,
) {
    match stmt {
        Stmt::Let { name, ty, value, .. } => {
            // Annotate the RHS first so any inner DynCall gets
            // its flags set before we look at the binding.
            annotate_expr(value, scopes, state_types, ifaces);
            // Decide the binding's tracked type.
            let tracked = if let Some(t) = ty {
                Some(t.clone())
            } else {
                infer_simple_type(value, scopes, state_types, ifaces)
            };
            if let Some(t) = tracked {
                scopes.last_mut().unwrap().insert(name.clone(), t);
            }
        }
        Stmt::LetTuple { value, .. } => annotate_expr(value, scopes, state_types, ifaces),
        Stmt::Assign { target, value, .. } => {
            annotate_expr(target, scopes, state_types, ifaces);
            annotate_expr(value, scopes, state_types, ifaces);
        }
        Stmt::Return { value: Some(e), .. } => annotate_expr(e, scopes, state_types, ifaces),
        Stmt::Return { value: None, .. } => {}
        Stmt::If(if_stmt) => annotate_if(if_stmt, scopes, state_types, ifaces),
        Stmt::While { cond, body, .. } => {
            annotate_expr(cond, scopes, state_types, ifaces);
            annotate_block(body, scopes, state_types, ifaces);
        }
        Stmt::For { var, iter, body, .. } => {
            annotate_expr(iter, scopes, state_types, ifaces);
            scopes.push(HashMap::new());
            // Don't bother tracking iter-var type; the loop body
            // could do dyncalls but iter vars are rarely interfaces.
            let _ = var;
            for stmt in body.stmts.iter_mut() {
                annotate_stmt(stmt, scopes, state_types, ifaces);
            }
            scopes.pop();
        }
        Stmt::ForRange { var, start, end, body, .. } => {
            annotate_expr(start, scopes, state_types, ifaces);
            annotate_expr(end, scopes, state_types, ifaces);
            scopes.push(HashMap::new());
            let _ = var;
            for stmt in body.stmts.iter_mut() {
                annotate_stmt(stmt, scopes, state_types, ifaces);
            }
            scopes.pop();
        }
        Stmt::Expr(e) => annotate_expr(e, scopes, state_types, ifaces),
        Stmt::Emit { value, .. } => {
            annotate_expr(value, scopes, state_types, ifaces);
        }
        Stmt::Delete { target, .. } => {
            annotate_expr(target, scopes, state_types, ifaces);
        }
        Stmt::Parallel { stmts, .. } => {
            // Each inner stmt annotates against the same outer
            // scope. Intra-block refs are forbidden at check time;
            // annotation just visits the trees.
            for s in stmts.iter_mut() {
                annotate_stmt(s, scopes, state_types, ifaces);
            }
        }
        Stmt::ParallelForTo { id_var, source, output, body, .. } => {
            annotate_expr(source, scopes, state_types, ifaces);
            annotate_expr(output, scopes, state_types, ifaces);
            scopes.push(HashMap::new());
            scopes.last_mut().unwrap().insert(id_var.clone(), Type::U64);
            annotate_block(body, scopes, state_types, ifaces);
            scopes.pop();
        }
        Stmt::Break(_) | Stmt::Continue(_) | Stmt::Placeholder(_) => {}
    }
}

fn annotate_if(
    ifs: &mut IfStmt,
    scopes: &mut Vec<HashMap<String, Type>>,
    state_types: &HashMap<String, Type>,
    ifaces: &HashMap<String, Vec<crate::ast::InterfaceMethodSig>>,
) {
    annotate_expr(&mut ifs.cond, scopes, state_types, ifaces);
    annotate_block(&mut ifs.then, scopes, state_types, ifaces);
    match &mut ifs.else_branch {
        ElseBranch::None => {}
        ElseBranch::Block(b) => annotate_block(b, scopes, state_types, ifaces),
        ElseBranch::If(inner) => annotate_if(inner, scopes, state_types, ifaces),
    }
}

fn annotate_expr(
    expr: &mut Expr,
    scopes: &mut Vec<HashMap<String, Type>>,
    state_types: &HashMap<String, Type>,
    ifaces: &HashMap<String, Vec<crate::ast::InterfaceMethodSig>>,
) {
    match &mut expr.kind {
        ExprKind::DynCall { target_ident, method, args, method_is_view, method_is_pure } => {
            // Pull the target's tracked type. Locals first
            // (innermost scope wins), then states.
            let target_ty = scopes.iter().rev().find_map(|s| s.get(target_ident).cloned())
                .or_else(|| state_types.get(target_ident).cloned());
            if let Some(Type::Interface { methods, .. }) = target_ty {
                if let Some(m) = methods.iter().find(|m| &m.name == method) {
                    *method_is_view = m.is_view;
                    *method_is_pure = m.is_pure;
                }
            }
            for a in args.iter_mut() { annotate_expr(a, scopes, state_types, ifaces); }
        }
        ExprKind::Binary { lhs, rhs, .. } => {
            annotate_expr(lhs, scopes, state_types, ifaces);
            annotate_expr(rhs, scopes, state_types, ifaces);
        }
        ExprKind::Unary { operand, .. } => annotate_expr(operand, scopes, state_types, ifaces),
        ExprKind::Index { target, key } => {
            annotate_expr(target, scopes, state_types, ifaces);
            annotate_expr(key, scopes, state_types, ifaces);
        }
        ExprKind::Field { target, .. } => annotate_expr(target, scopes, state_types, ifaces),
        ExprKind::Array(elems) | ExprKind::SetLit(elems) | ExprKind::TupleLit(elems) => {
            for e in elems.iter_mut() { annotate_expr(e, scopes, state_types, ifaces); }
        }
        ExprKind::DictLit(pairs) => {
            for (k, v) in pairs.iter_mut() {
                annotate_expr(k, scopes, state_types, ifaces);
                annotate_expr(v, scopes, state_types, ifaces);
            }
        }
        ExprKind::ListComp { mapper, clauses } | ExprKind::SetComp { mapper, clauses } => {
            for c in clauses.iter_mut() {
                match c {
                    CompClause::For { iter, .. } => annotate_expr(iter, scopes, state_types, ifaces),
                    CompClause::If(cond) => annotate_expr(cond, scopes, state_types, ifaces),
                }
            }
            annotate_expr(mapper, scopes, state_types, ifaces);
        }
        ExprKind::DictComp { key, value, clauses } => {
            for c in clauses.iter_mut() {
                match c {
                    CompClause::For { iter, .. } => annotate_expr(iter, scopes, state_types, ifaces),
                    CompClause::If(cond) => annotate_expr(cond, scopes, state_types, ifaces),
                }
            }
            annotate_expr(key, scopes, state_types, ifaces);
            annotate_expr(value, scopes, state_types, ifaces);
        }
        ExprKind::StructLit { fields, .. } => for (_, e) in fields.iter_mut() {
            annotate_expr(e, scopes, state_types, ifaces);
        },
        ExprKind::Pipe { head, step } => {
            annotate_expr(head, scopes, state_types, ifaces);
            annotate_expr(step, scopes, state_types, ifaces);
        }
        ExprKind::Call { args, .. } => for a in args.iter_mut() {
            annotate_expr(a, scopes, state_types, ifaces);
        },
        ExprKind::TupleIndex { target, .. } => annotate_expr(target, scopes, state_types, ifaces),
        ExprKind::EnumCtor { args, .. } => for a in args.iter_mut() {
            annotate_expr(a, scopes, state_types, ifaces);
        },
        ExprKind::Match { scrut, arms } => {
            annotate_expr(scrut, scopes, state_types, ifaces);
            for arm in arms.iter_mut() {
                annotate_expr(&mut arm.body, scopes, state_types, ifaces);
            }
        }
        ExprKind::Block(block) => annotate_block(block, scopes, state_types, ifaces),
        ExprKind::If { cond, then, else_branch } => {
            annotate_expr(cond, scopes, state_types, ifaces);
            for s in then.stmts.iter_mut() { annotate_stmt(s, scopes, state_types, ifaces); }
            if let Some(t) = then.tail.as_mut() { annotate_expr(t, scopes, state_types, ifaces); }
            match else_branch {
                ElseBranch::None => {}
                ElseBranch::Block(b) => {
                    for s in b.stmts.iter_mut() { annotate_stmt(s, scopes, state_types, ifaces); }
                    if let Some(t) = b.tail.as_mut() { annotate_expr(t, scopes, state_types, ifaces); }
                }
                ElseBranch::If(inner) => annotate_if(inner, scopes, state_types, ifaces),
            }
        }
        // JSON object/array literals: recurse into each value
        // expression so their idents resolve and types annotate.
        ExprKind::JsonObject(pairs) => {
            for (_, v) in pairs {
                annotate_expr(v, scopes, state_types, ifaces);
            }
        }
        ExprKind::JsonArray(items) => {
            for item in items {
                annotate_expr(item, scopes, state_types, ifaces);
            }
        }
        // Leaves: nothing to descend into.
        ExprKind::Int(_) | ExprKind::UInt(_) | ExprKind::Float(_) | ExprKind::JsonNull | ExprKind::I32(_) | ExprKind::U32(_)
        | ExprKind::U64(_) | ExprKind::U128(_) | ExprKind::Bool(_)
        | ExprKind::Str(_) | ExprKind::Ident(_) | ExprKind::Prev => {}
    }
}

/// Best-effort type inference for the RHS of an unannotated
/// `let`. Returns `Some(Type)` only for the patterns the
/// annotator cares about — anything else falls back to None,
/// which means DynCalls through that local stay conservative.
fn infer_simple_type(
    expr: &Expr,
    scopes: &[HashMap<String, Type>],
    state_types: &HashMap<String, Type>,
    ifaces: &HashMap<String, Vec<crate::ast::InterfaceMethodSig>>,
) -> Option<Type> {
    match &expr.kind {
        // `let t = IFace::bind(...)` — recognize the constructor.
        ExprKind::Call { module: Some(mod_name), name, .. }
            if name == "bind" && ifaces.contains_key(mod_name) =>
        {
            let methods = ifaces.get(mod_name).cloned().unwrap_or_default();
            Some(Type::Interface { name: mod_name.clone(), methods })
        }
        // `let t = some_ident` — copy the existing type.
        ExprKind::Ident(name) => {
            for s in scopes.iter().rev() {
                if let Some(t) = s.get(name) { return Some(t.clone()); }
            }
            state_types.get(name).cloned()
        }
        _ => None,
    }
}


fn builtin_sigs() -> HashMap<String, FnSig> {
    let mut m = HashMap::new();
    m.insert("resource".to_string(), FnSig { params: vec![Type::Int], ret: Type::Resource });
    m.insert("unwrap".to_string(),   FnSig { params: vec![Type::Resource], ret: Type::Int });
    m.insert("address".to_string(),  FnSig { params: vec![Type::String], ret: Type::Address });
    m
}

fn is_storable(ty: &Type) -> bool {
    match ty {
        Type::Int | Type::UInt | Type::Float | Type::I32 | Type::U32 | Type::U64 | Type::U128
        | Type::Bool | Type::String | Type::Address | Type::Bytes => true,
        Type::Array(elem) => is_storable(elem),
        Type::Struct { fields, .. } => fields.iter().all(|(_, t)| is_storable(t)),
        Type::Map { .. } | Type::PMap { .. } | Type::PBTree { .. } | Type::PVec { .. } => true,
        Type::Json => true,
        // Interface values are storable: their runtime form is a
        // string (module name).
        Type::Interface { .. } => true,
        Type::Set(_) | Type::Dict { .. } | Type::Tuple(_) => false,
        Type::Enum { variants, .. } => variants
            .iter()
            .all(|(_, payload)| payload.iter().all(is_storable)),
        // Caps are storable iff every field is storable. Their non-Copy
        // semantics still apply to *handing* them around — putting a
        // cap in state is the typed way for a contract to "hold" its
        // own root authority.
        Type::Cap { fields, .. } => fields.iter().all(|(_, t)| is_storable(t)),
        Type::Unit | Type::Resource => false,
    }
}

struct FnSig {
    params: Vec<Type>,
    ret: Type,
}

struct TypeChecker {
    sigs: HashMap<String, FnSig>,
    /// Module-defined functions only (no imports/builtins), name → is_entry.
    /// Used to enforce the "main only calls entry" rule: a transaction's
    /// `main` is host-facing, so it must touch other functions only through
    /// their public (entry) surface.
    module_fn_entry: HashMap<String, bool>,
    states: HashMap<String, Type>,
    structs: HashMap<String, Vec<(String, Type)>>,
    /// Parallel to `structs`: per-field group annotations
    /// (`None` = own cell, `Some(g)` = shared cell). Populated from
    /// `StructDecl.fields[i].group`. Used when materializing a
    /// `Type::Struct` value from a literal so the type matches the
    /// declaration's storage shape and `==` succeeds on assignment.
    struct_groups: HashMap<String, Vec<Option<String>>>,
    /// Enum name → ordered (variant_name, payload_types). Used by
    /// constructors and match-arm validation.
    enums: HashMap<String, Vec<(String, Vec<Type>)>>,
    /// `const NAME: T = expr;` map. Looked up after locals/states
    /// when resolving an Ident.
    consts: HashMap<String, Type>,
    /// Cap declarations: name → (fields, declaring_module). Cap
    /// literals are only legal where `current_module == declaring_module`
    /// — that's what makes a cap unforgeable from outside.
    caps: HashMap<String, (Vec<(String, Type)>, String)>,
    /// Interface declarations: name → method signatures. Looked
    /// up when an `IFace::bind(name)` constructor or a method
    /// dispatch is type-checked.
    interfaces: HashMap<String, Vec<crate::ast::InterfaceMethodSig>>,
    /// Cross-artifact interfaces — declared in some dep, made
    /// available so a tx source can write `Foo::bind(...)` for
    /// a `Foo` declared in a deployed program. Looked up after
    /// the local `interfaces` map.
    iface_externals: HashMap<String, Vec<crate::ast::InterfaceMethodSig>>,
    /// Name of the module under check. Compared against a cap's
    /// `owner_module` to decide whether a literal is privileged.
    current_module: String,
    /// Cross-module entry signatures, keyed as "module::fn".
    externals: HashMap<String, FnSig>,
    /// Stack of `$$` types within enclosing pipe stages. The top is the
    /// type of the most-recently-bound pipe head.
    pipe_stack: std::cell::RefCell<Vec<Type>>,
    /// True while checking the body of a function named `main`. Such
    /// functions are subject to the host-boundary rule above.
    in_main: std::cell::Cell<bool>,
}

#[derive(Default)]
pub struct ModuleManifest {
    /// Entry signatures keyed by function name. The host of the typeck pass
    /// is responsible for combining manifests across modules.
    pub entry_sigs: HashMap<String, ModuleSig>,
}

#[derive(Clone)]
pub struct ModuleSig {
    pub params: Vec<Type>,
    pub ret: Type,
}

pub fn collect_manifest(module: &Module) -> ModuleManifest {
    let mut entry_sigs = HashMap::new();
    for f in &module.functions {
        if f.is_entry {
            entry_sigs.insert(
                f.name.clone(),
                ModuleSig {
                    params: f.params.iter().map(|p| p.ty.clone()).collect(),
                    ret: f.return_type.clone(),
                },
            );
        }
    }
    ModuleManifest { entry_sigs }
}

pub fn check_with_externals(
    module: &Module,
    externals: HashMap<String, ModuleSig>,
) -> Result<(), Error> {
    check_with_externals_and_ifaces(module, externals, HashMap::new())
}

/// Like `check_with_externals` but also accepts a manifest of
/// interfaces declared in dependency artifacts. The tx source can
/// write `Foo::bind(...)` for a `Foo` declared in a deployed
/// program; typeck looks it up here.
pub fn check_with_externals_and_ifaces(
    module: &Module,
    externals: HashMap<String, ModuleSig>,
    iface_externals: HashMap<String, Vec<crate::ast::InterfaceMethodSig>>,
) -> Result<(), Error> {
    let externals: HashMap<String, FnSig> = externals
        .into_iter()
        .map(|(k, v)| (k, FnSig { params: v.params, ret: v.ret }))
        .collect();
    check_inner(module, externals, iface_externals)
}

fn check_inner(
    module: &Module,
    externals: HashMap<String, FnSig>,
    iface_externals: HashMap<String, Vec<crate::ast::InterfaceMethodSig>>,
) -> Result<(), Error> {
    let mut sigs = run_sigs(module)?;
    // builtins come from the existing helper; merge:
    for (k, v) in builtin_sigs() { sigs.entry(k).or_insert(v); }

    let mut structs: HashMap<String, Vec<(String, Type)>> = HashMap::new();
    let mut struct_groups: HashMap<String, Vec<Option<String>>> = HashMap::new();
    for s in &module.structs {
        structs.insert(
            s.name.clone(),
            s.fields.iter().map(|f| (f.name.clone(), f.ty.clone())).collect(),
        );
        let groups: Vec<Option<String>> =
            s.fields.iter().map(|f| f.group.clone()).collect();
        struct_groups.insert(s.name.clone(), normalize_groups(groups));
    }

    let mut states: HashMap<String, Type> = HashMap::new();
    for s in &module.states {
        if !is_storable(&s.ty) {
            return Err(Error::new(
                ErrorKind::Type,
                format!("state '{}' has unsupported type {}", s.name, s.ty),
                s.span,
            ));
        }
        if let Type::Map { key, value } = &s.ty {
            if !key.is_keyable() {
                return Err(Error::new(
                    ErrorKind::Type,
                    format!("state '{}' map key {key} is not keyable", s.name),
                    s.span,
                ));
            }
            if !is_storable(value) {
                return Err(Error::new(
                    ErrorKind::Type,
                    format!("state '{}' map value type {value} is not storable", s.name),
                    s.span,
                ));
            }
        }
        // Slice-1 pbtree only supports u64 keys. Reject anything
        // else at state-decl time so users see the limit before
        // they wire up reads/writes.
        if let Type::PBTree { key, .. } = &s.ty {
            if !matches!(key.as_ref(), Type::U64 | Type::U128 | Type::Bytes) {
                return Err(Error::new(
                    ErrorKind::Type,
                    format!(
                        "state '{}': pbtree supports u64, u128, or bytes keys (got pbtree<{key}, _>)",
                        s.name,
                    ),
                    s.span,
                ));
            }
        }
        if states.contains_key(&s.name) {
            return Err(Error::new(
                ErrorKind::Type,
                format!("duplicate state '{}'", s.name),
                s.span,
            ));
        }
        states.insert(s.name.clone(), s.ty.clone());
    }

    // Validate index declarations: each must connect two pmap state
    // slots, and the projection path must resolve through the
    // primary's value-struct to a leaf type matching the index key.
    for idx in &module.indexes {
        let primary_ty = states.get(&idx.on_state).ok_or_else(|| Error::new(
            ErrorKind::Type,
            format!("index '{}': on-state '{}' is not a state slot", idx.name, idx.on_state),
            idx.span,
        ))?;
        let (primary_key, primary_val) = match primary_ty {
            Type::PMap { key, value } => ((**key).clone(), (**value).clone()),
            other => return Err(Error::new(
                ErrorKind::Type,
                format!(
                    "index '{}' on '{}': can only index a pmap state, got {other}",
                    idx.name, idx.on_state,
                ),
                idx.span,
            )),
        };
        let index_ty = states.get(&idx.name).ok_or_else(|| Error::new(
            ErrorKind::Type,
            format!("index '{}': must declare a matching state slot first", idx.name),
            idx.span,
        ))?;
        // Index slot can be either pmap (hash-ordered) or pbtree
        // (key-sorted). The compiler picks the right maintenance
        // ops based on which backend the user declared. Same
        // validation rules from there: K matches projection, V
        // matches primary key (or [K] for multi-indexes).
        let (index_key, index_val) = match index_ty {
            Type::PMap { key, value } => ((**key).clone(), (**value).clone()),
            Type::PBTree { key, value } => ((**key).clone(), (**value).clone()),
            other => return Err(Error::new(
                ErrorKind::Type,
                format!(
                    "index '{}': index slot must be a pmap or pbtree, got {other}",
                    idx.name,
                ),
                idx.span,
            )),
        };
        // Multi-indexes store `[K]`. Unwrap the array to get back to
        // K so the comparison logic below is uniform across kinds.
        let expected_index_val: Type = match idx.kind {
            crate::ast::IndexKind::Unique => index_val.clone(),
            crate::ast::IndexKind::Multi => match &index_val {
                Type::Array(inner) => (**inner).clone(),
                other => return Err(Error::new(
                    ErrorKind::Type,
                    format!(
                        "index '{}': multi-index slot must be pmap<F, [K]> or pbtree<F, [K]>, \
                         got value type {other} — did you mean `unique_index`?",
                        idx.name,
                    ),
                    idx.span,
                )),
            },
        };
        // Resolve each projected field's static type.
        let mut projected_types: Vec<Type> = Vec::with_capacity(idx.fields.len());
        for f in &idx.fields {
            let mut cur_ty = primary_val.clone();
            for (depth, field) in f.path.iter().enumerate() {
                let struct_name = match &cur_ty {
                    Type::Struct { name, .. } => name.clone(),
                    other => return Err(Error::new(
                        ErrorKind::Type,
                        format!(
                            "index '{}': projection step {depth} ('{field}') needs a struct type, got {other}",
                            idx.name,
                        ),
                        idx.span,
                    )),
                };
                let s_fields = structs.get(&struct_name).ok_or_else(|| Error::new(
                    ErrorKind::Type,
                    format!(
                        "index '{}': struct '{struct_name}' has no fields registered",
                        idx.name,
                    ),
                    idx.span,
                ))?;
                let next = s_fields.iter().find(|(n, _)| n == field).ok_or_else(|| Error::new(
                    ErrorKind::Type,
                    format!(
                        "index '{}': struct '{struct_name}' has no field '{field}'",
                        idx.name,
                    ),
                    idx.span,
                ))?;
                cur_ty = next.1.clone();
            }
            projected_types.push(cur_ty);
        }
        if idx.fields.len() == 1
            && idx.fields[0].direction == crate::ast::SortDirection::Asc
        {
            // Legacy single-field: the projected field's type IS
            // the index key type. The user declares the index slot
            // with that same key type (e.g. `pmap<u64, u64>`).
            let proj_ty = &projected_types[0];
            if proj_ty != &index_key {
                return Err(Error::new(
                    ErrorKind::Type,
                    format!(
                        "index '{}': projected field type {proj_ty} does not match index key type {index_key}",
                        idx.name,
                    ),
                    idx.span,
                ));
            }
        } else {
            // Composite: index slot must be `pbtree<bytes, _>` (the
            // packed key is variable-length bytes), and every
            // projected field must be `to_be_bytes`-able (fixed
            // width int).
            if index_key != Type::Bytes {
                return Err(Error::new(
                    ErrorKind::Type,
                    format!(
                        "index '{}' is composite ({} fields) — its slot must be `pbtree<bytes, _>` (got key type {index_key})",
                        idx.name, idx.fields.len(),
                    ),
                    idx.span,
                ));
            }
            for (f, t) in idx.fields.iter().zip(&projected_types) {
                if !matches!(t, Type::U32 | Type::U64 | Type::U128 | Type::I32) {
                    return Err(Error::new(
                        ErrorKind::Type,
                        format!(
                            "index '{}': composite field {:?} has type {t} — composite fields must be fixed-width ints (u32/u64/u128/i32)",
                            idx.name, f.path,
                        ),
                        idx.span,
                    ));
                }
            }
        }
        if expected_index_val != primary_key {
            let pretty = match idx.kind {
                crate::ast::IndexKind::Unique => format!("{expected_index_val}"),
                crate::ast::IndexKind::Multi  => format!("[{expected_index_val}]"),
            };
            return Err(Error::new(
                ErrorKind::Type,
                format!(
                    "index '{}': index value type {pretty} must hold the primary key type {primary_key}",
                    idx.name,
                ),
                idx.span,
            ));
        }
    }

    let mut module_fn_entry = HashMap::new();
    for f in &module.functions {
        module_fn_entry.insert(f.name.clone(), f.is_entry);
    }

    let mut enums: HashMap<String, Vec<(String, Vec<Type>)>> = HashMap::new();
    for en in &module.enums {
        if enums.contains_key(&en.name) {
            return Err(Error::new(
                ErrorKind::Type,
                format!("duplicate enum '{}'", en.name),
                en.span,
            ));
        }
        enums.insert(
            en.name.clone(),
            en.variants
                .iter()
                .map(|v| (v.name.clone(), v.payload.clone()))
                .collect(),
        );
    }

    // First, type-check each const's RHS against its declared type.
    // Build the const map here so subsequent type-checks can resolve
    // const references in expressions.
    let mut consts: HashMap<String, Type> = HashMap::new();
    for c in &module.consts {
        if states.contains_key(&c.name) || consts.contains_key(&c.name) {
            return Err(Error::new(
                ErrorKind::Type,
                format!("const '{}' shadows another module-level binding", c.name),
                c.span,
            ));
        }
        consts.insert(c.name.clone(), c.ty.clone());
    }

    let current_module = module.name.clone().unwrap_or_else(|| "main".to_string());
    let mut caps: HashMap<String, (Vec<(String, Type)>, String)> = HashMap::new();
    for decl in &module.caps {
        if structs.contains_key(&decl.name) || enums.contains_key(&decl.name)
            || states.contains_key(&decl.name) || consts.contains_key(&decl.name)
            || caps.contains_key(&decl.name)
        {
            return Err(Error::new(
                ErrorKind::Type,
                format!("cap '{}' shadows another module-level binding", decl.name),
                decl.span,
            ));
        }
        let resolved: Vec<(String, Type)> = decl.fields
            .iter()
            .map(|f| (f.name.clone(), f.ty.clone()))
            .collect();
        caps.insert(decl.name.clone(), (resolved, current_module.clone()));
    }

    let mut interfaces: HashMap<String, Vec<crate::ast::InterfaceMethodSig>> = HashMap::new();
    for d in &module.interfaces {
        let sigs = d.methods.iter().map(|m| crate::ast::InterfaceMethodSig {
            name: m.name.clone(),
            params: m.params.iter().map(|p| p.ty.clone()).collect(),
            return_type: m.return_type.clone(),
            is_view: m.is_view,
            is_pure: m.is_pure,
        }).collect();
        interfaces.insert(d.name.clone(), sigs);
    }
    let tc = TypeChecker {
        sigs,
        module_fn_entry,
        states,
        structs,
        struct_groups,
        enums,
        consts,
        caps,
        interfaces,
        iface_externals,
        current_module,
        externals,
        pipe_stack: std::cell::RefCell::new(Vec::new()),
        in_main: std::cell::Cell::new(false),
    };
    // Now validate each const's RHS produces the declared type.
    let empty_env: Env = vec![HashMap::new()];
    for c in &module.consts {
        let actual = tc.check_expr(&c.value, &empty_env)?;
        if actual != c.ty {
            return Err(Error::new(
                ErrorKind::Type,
                format!(
                    "const '{}': declared {}, value is {}",
                    c.name, c.ty, actual,
                ),
                c.span,
            ));
        }
    }
    for f in &module.functions {
        tc.check_fn(f)?;
    }
    // Handler fn bodies were already checked above (they live in
    // `module.functions`). Here we additionally validate the
    // handler-specific signature shape: exactly one parameter, whose
    // type is the struct named by `on <Type>`.
    for h in &module.handlers {
        if h.fn_def.params.len() != 1 {
            return Err(Error::new(
                ErrorKind::Type,
                format!(
                    "handler '{}' on '{}' must take exactly one parameter (the event struct)",
                    h.fn_def.name, h.event_type,
                ),
                h.span,
            ));
        }
        // The parser parsed the handler before lowering it into
        // `functions`, so its types haven't been resolved yet
        // through the type-resolution pass. Pull the resolved
        // version from `module.functions` and validate against
        // *that* — the unresolved form still has `fields: []`.
        let resolved = module
            .functions
            .iter()
            .find(|f| f.name == h.fn_def.name)
            .expect("handler fn was lowered into functions");
        let param_ty = &resolved.params[0].ty;
        let event_struct_name = match param_ty {
            Type::Struct { name, .. } => name.clone(),
            other => {
                return Err(Error::new(
                    ErrorKind::Type,
                    format!(
                        "handler '{}' on '{}': parameter type must be a struct, got {other}",
                        h.fn_def.name, h.event_type,
                    ),
                    resolved.params[0].span,
                ));
            }
        };
        // Compare the qualified `on` clause against the qualified
        // parameter type. Local handlers see a bare name on both
        // sides; cross-module handlers see `m::T` on both sides.
        let expected_qualified = match &h.event_module {
            Some(m) => format!("{m}::{}", h.event_type),
            None => h.event_type.clone(),
        };
        if event_struct_name != expected_qualified {
            return Err(Error::new(
                ErrorKind::Type,
                format!(
                    "handler '{}' on '{}': parameter type '{}' must match the event type",
                    h.fn_def.name, expected_qualified, event_struct_name,
                ),
                resolved.params[0].span,
            ));
        }
    }
    Ok(())
}

/// Collect names bound by `let` / `let (..)` inside a single
/// statement. Used by `parallel { ... }` to forbid intra-block
/// references: a stmt may not mention names other stmts in the
/// same block introduce.
fn collect_let_bindings(stmt: &Stmt) -> Vec<String> {
    match stmt {
        Stmt::Let { name, .. } => vec![name.clone()],
        Stmt::LetTuple { names, .. } => names.clone(),
        _ => Vec::new(),
    }
}

/// Walk a statement / expression tree and error if any identifier
/// reference hits a name in `forbidden`. The `parallel` block's
/// no-intra-block-ref rule lives here.
fn check_no_forbidden_refs(
    stmt: &Stmt,
    forbidden: &std::collections::HashSet<&str>,
    block_span: Span,
) -> Result<(), Error> {
    use std::collections::HashSet;
    fn walk_expr(
        e: &Expr,
        forbidden: &HashSet<&str>,
        block_span: Span,
    ) -> Result<(), Error> {
        match &e.kind {
            ExprKind::Ident(name) => {
                if forbidden.contains(name.as_str()) {
                    return Err(Error::new(
                        ErrorKind::Type,
                        format!(
                            "`parallel` block: statement references `{name}`, \
                             which is `let`-bound by another statement in the \
                             same block — intra-block references aren't \
                             allowed (would defeat the parallelism)",
                        ),
                        block_span,
                    ));
                }
                Ok(())
            }
            ExprKind::Field { target, .. } => walk_expr(target, forbidden, block_span),
            ExprKind::Index { target, key } => {
                walk_expr(target, forbidden, block_span)?;
                walk_expr(key, forbidden, block_span)
            }
            ExprKind::Call { args, .. } => {
                for a in args { walk_expr(a, forbidden, block_span)?; }
                Ok(())
            }
            ExprKind::Binary { lhs, rhs, .. } => {
                walk_expr(lhs, forbidden, block_span)?;
                walk_expr(rhs, forbidden, block_span)
            }
            ExprKind::Unary { operand, .. } => walk_expr(operand, forbidden, block_span),
            ExprKind::Array(es) => {
                for e in es { walk_expr(e, forbidden, block_span)?; }
                Ok(())
            }
            ExprKind::StructLit { fields, .. } => {
                for (_, e) in fields { walk_expr(e, forbidden, block_span)?; }
                Ok(())
            }
            ExprKind::If { cond, then: _, else_branch: _ } => {
                // `parallel` body is restricted to flat statements
                // (no `if`/`for`), but `if` can still appear inside
                // an RHS expression. Walk the condition; the
                // then/else branches are block expressions whose
                // statement walking happens via the outer pass.
                walk_expr(cond, forbidden, block_span)
            }
            // Anything else (literals, enum/match details, comprehensions,
            // etc.) — we underapproximate by skipping. The block_span
            // error message still catches the common case (direct refs).
            _ => Ok(()),
        }
    }
    match stmt {
        Stmt::Let { value, .. } | Stmt::LetTuple { value, .. } => {
            walk_expr(value, forbidden, block_span)
        }
        Stmt::Expr(e) => walk_expr(e, forbidden, block_span),
        Stmt::Emit { value, .. } => walk_expr(value, forbidden, block_span),
        Stmt::Assign { target, value, .. } => {
            walk_expr(target, forbidden, block_span)?;
            walk_expr(value, forbidden, block_span)
        }
        Stmt::Delete { target, .. } => walk_expr(target, forbidden, block_span),
        _ => Ok(()),
    }
}

fn run_sigs(module: &Module) -> Result<HashMap<String, FnSig>, Error> {
    let mut sigs = HashMap::new();
    for imp in &module.imports {
        if sigs.contains_key(&imp.name) {
            return Err(Error::new(
                ErrorKind::Type,
                format!("import '{}' shadows builtin or duplicate", imp.name),
                imp.span,
            ));
        }
        sigs.insert(
            imp.name.clone(),
            FnSig { params: imp.params.clone(), ret: imp.return_type.clone() },
        );
    }
    for f in &module.functions {
        if sigs.contains_key(&f.name) {
            return Err(Error::new(
                ErrorKind::Type,
                format!("duplicate function '{}'", f.name),
                f.span,
            ));
        }
        sigs.insert(
            f.name.clone(),
            FnSig {
                params: f.params.iter().map(|p| p.ty.clone()).collect(),
                ret: f.return_type.clone(),
            },
        );
    }
    // Handler fns are already in `module.functions` (parser lowered
    // them there), so they get a sig entry via the loop above.
    Ok(sigs)
}

type Env = Vec<HashMap<String, Type>>;

impl TypeChecker {
    fn check_fn(&self, f: &FnDef) -> Result<(), Error> {
        let mut env: Env = vec![HashMap::new()];
        for p in &f.params {
            env.last_mut().unwrap().insert(p.name.clone(), p.ty.clone());
        }
        let prev = self.in_main.replace(f.name == "main");
        let returns = self.check_block(&f.body, &mut env, &f.return_type);
        self.in_main.set(prev);
        let mut returns = returns?;
        // The fn body's trailing tail expression acts as an implicit
        // return — its type must match the declared return.
        if let Some(t) = &f.body.tail {
            let mut tail_env = env.clone();
            tail_env.push(HashMap::new());
            // Re-walk the body's statements so their let-bindings are
            // in scope when we check the tail.
            for s in &f.body.stmts {
                self.check_stmt(s, &mut tail_env, &f.return_type)?;
            }
            let actual = self.check_expr(t, &tail_env)?;
            if &actual != &f.return_type {
                return Err(Error::new(
                    ErrorKind::Type,
                    format!(
                        "implicit return type mismatch: expected {}, got {actual}",
                        f.return_type,
                    ),
                    t.span,
                ));
            }
            returns = true;
        }
        if f.return_type != Type::Unit && !returns {
            return Err(Error::new(
                ErrorKind::Type,
                format!("function '{}' must return on all paths", f.name),
                f.span,
            ));
        }
        Ok(())
    }

    /// Returns true if the block returns on every path (i.e. ends in a `return`
    /// or an `if` whose every branch returns).
    fn check_block(&self, block: &Block, env: &mut Env, expected_ret: &Type) -> Result<bool, Error> {
        env.push(HashMap::new());
        let mut returns = false;
        for stmt in &block.stmts {
            if self.check_stmt(stmt, env, expected_ret)? {
                returns = true;
            }
        }
        // Tail expressions on inner blocks (if/else branches, while
        // bodies) are values discarded in statement context; we
        // still type-check them but don't compare to expected_ret.
        // The fn-body tail check happens in `check_fn`.
        if let Some(t) = &block.tail {
            let _ = self.check_expr(t, env)?;
        }
        env.pop();
        Ok(returns)
    }

    fn check_stmt(&self, stmt: &Stmt, env: &mut Env, expected_ret: &Type) -> Result<bool, Error> {
        match stmt {
            Stmt::Let { name, ty, value, span } => {
                let t = match ty {
                    Some(annot) => {
                        let actual = self.check_expr_with_hint(value, env, Some(annot))?;
                        if &actual != annot {
                            return Err(Error::new(
                                ErrorKind::Type,
                                format!("let '{name}': annotation says {annot}, value has type {actual}"),
                                *span,
                            ));
                        }
                        annot.clone()
                    }
                    None => self.check_expr(value, env)?,
                };
                env.last_mut().unwrap().insert(name.clone(), t);
                Ok(false)
            }
            Stmt::Assign { target, value, span } => {
                let actual = self.check_expr(value, env)?;
                let expected = self.check_lvalue(target, env)?;
                if !types_compatible(&actual, &expected) {
                    return Err(Error::new(
                        ErrorKind::Type,
                        format!("assignment: expected {expected}, got {actual}"),
                        *span,
                    ));
                }
                Ok(false)
            }
            Stmt::Return { value, span } => {
                let actual = match value {
                    Some(e) => self.check_expr(e, env)?,
                    None => Type::Unit,
                };
                if !types_compatible(&actual, expected_ret) {
                    return Err(Error::new(
                        ErrorKind::Type,
                        format!(
                            "return type mismatch: expected {expected_ret}, got {actual}",
                        ),
                        *span,
                    ));
                }
                Ok(true)
            }
            Stmt::If(if_stmt) => self.check_if(if_stmt, env, expected_ret),
            Stmt::While { cond, body, span } => {
                let cond_ty = self.check_expr(cond, env)?;
                if cond_ty != Type::Bool {
                    return Err(Error::new(
                        ErrorKind::Type,
                        format!("while condition must be bool, got {cond_ty}"),
                        *span,
                    ));
                }
                self.check_block(body, env, expected_ret)?;
                Ok(false)
            }
            Stmt::For { var, iter, body, span } => {
                let iter_ty = self.check_expr(iter, env)?;
                // Sugar: iterating over a pmap state yields values;
                // iterating over a pvec state yields elements. For
                // entries or keys, call `pmap_entries` / `pmap_keys`
                // explicitly. This is the SQL `FOR row IN table`
                // equivalent — we pick "row" = value as the most
                // common case.
                let elem = match iter_ty {
                    Type::Array(elem)         => *elem,
                    Type::PMap { value, .. }  => *value,
                    Type::PBTree { value, .. } => *value,
                    Type::PVec { elem }       => *elem,
                    other => return Err(Error::new(
                        ErrorKind::Type,
                        format!("for-in iterator must be an array, pmap, pbtree, or pvec; got {other}"),
                        *span,
                    )),
                };
                env.push(HashMap::new());
                env.last_mut().unwrap().insert(var.clone(), elem);
                let r = self.check_block(body, env, expected_ret);
                env.pop();
                r?;
                Ok(false)
            }
            Stmt::ForRange { var, start, end, body, span, .. } => {
                let s_ty = self.check_expr(start, env)?;
                let e_ty = self.check_expr(end, env)?;
                if s_ty != e_ty {
                    return Err(Error::new(
                        ErrorKind::Type,
                        format!("range bounds must be the same type: {s_ty}..{e_ty}"),
                        *span,
                    ));
                }
                if !is_int(&s_ty) {
                    return Err(Error::new(
                        ErrorKind::Type,
                        format!("range bounds must be integer, got {s_ty}"),
                        *span,
                    ));
                }
                env.push(HashMap::new());
                env.last_mut().unwrap().insert(var.clone(), s_ty);
                let r = self.check_block(body, env, expected_ret);
                env.pop();
                r?;
                Ok(false)
            }
            Stmt::Break(_) | Stmt::Continue(_) => Ok(false),
            Stmt::Expr(e) => {
                let _ = self.check_expr(e, env)?;
                Ok(false)
            }
            Stmt::Placeholder(span) => Err(Error::new(
                ErrorKind::Type,
                "`_;` is only valid inside a modifier body".to_string(),
                *span,
            )),
            Stmt::LetTuple { names, value, span } => {
                let v_ty = self.check_expr(value, env)?;
                let elems = match v_ty {
                    Type::Tuple(elems) => elems,
                    other => return Err(Error::new(
                        ErrorKind::Type,
                        format!("destructure: expected tuple, got {other}"),
                        *span,
                    )),
                };
                if elems.len() != names.len() {
                    return Err(Error::new(
                        ErrorKind::Type,
                        format!(
                            "destructure: pattern has {} name(s), value has {} component(s)",
                            names.len(),
                            elems.len(),
                        ),
                        *span,
                    ));
                }
                for (n, t) in names.iter().zip(elems.into_iter()) {
                    env.last_mut().unwrap().insert(n.clone(), t);
                }
                Ok(false)
            }
            Stmt::Delete { target, span } => {
                // Target must be `state[key]` where state is a pmap
                // or pbtree. Other shapes (delete a struct field,
                // delete a pvec slot, delete a local) aren't
                // semantically meaningful here.
                let ExprKind::Index { target: t, key } = &target.kind else {
                    return Err(Error::new(
                        ErrorKind::Type,
                        "delete target must be `state[key]`".to_string(),
                        *span,
                    ));
                };
                let ExprKind::Ident(state_name) = &t.kind else {
                    return Err(Error::new(
                        ErrorKind::Type,
                        "delete target must be a state slot".to_string(),
                        *span,
                    ));
                };
                let state_ty = self.states.get(state_name).cloned().ok_or_else(|| Error::new(
                    ErrorKind::Type,
                    format!("'{state_name}' is not a state slot"),
                    *span,
                ))?;
                let kt = match &state_ty {
                    Type::PMap { key: kt, .. } => (**kt).clone(),
                    Type::PBTree { key: kt, .. } => (**kt).clone(),
                    other => return Err(Error::new(
                        ErrorKind::Type,
                        format!(
                            "delete: '{state_name}' must be a pmap or pbtree state, got {other}",
                        ),
                        *span,
                    )),
                };
                let actual_key = self.check_expr(key, env)?;
                if actual_key != kt {
                    return Err(Error::new(
                        ErrorKind::Type,
                        format!(
                            "delete: key for '{state_name}' must be {kt}, got {actual_key}",
                        ),
                        key.span,
                    ));
                }
                Ok(false)
            }
            Stmt::Emit { value, span: _ } => {
                // The expression must evaluate to a struct value; the
                // struct's name + the emitting module form the event
                // identity used by the handler dispatch table.
                let ty = self.check_expr(value, env)?;
                if !matches!(ty, Type::Struct { .. }) {
                    return Err(Error::new(
                        ErrorKind::Type,
                        format!("emit requires a struct value, got {ty}"),
                        value.span,
                    ));
                }
                Ok(false)
            }
            Stmt::Parallel { stmts, span } => {
                // Step 1: enforce "no intra-block references". For
                // each stmt, collect names bound by *other* stmts in
                // the same block; the stmt's expressions may not
                // reference any of them. The block's `let`s lift
                // into the enclosing scope (so the names outlive
                // the block).
                let bindings: Vec<Vec<String>> = stmts
                    .iter()
                    .map(|s| collect_let_bindings(s))
                    .collect();
                for (i, s) in stmts.iter().enumerate() {
                    let mut forbidden: std::collections::HashSet<&str> =
                        std::collections::HashSet::new();
                    for (j, names) in bindings.iter().enumerate() {
                        if j == i { continue; }
                        for n in names { forbidden.insert(n.as_str()); }
                    }
                    if !forbidden.is_empty() {
                        check_no_forbidden_refs(s, &forbidden, *span)?;
                    }
                }
                // Step 2: check each stmt as usual. Bindings flow
                // into `env` so post-block code sees them — that's
                // the "let bindings outlive the block" contract.
                for s in stmts {
                    self.check_stmt(s, env, expected_ret)?;
                }
                Ok(false)
            }
            Stmt::ParallelForTo { id_var, source, output, body, span } => {
                // Source must be [u64] — slice 1 fixes the id type at
                // u64 (matches the `reserve N from state` story, where
                // a state counter yields u64 ids). Future slices may
                // relax this to any [T] once typeck and the parallel
                // dispatcher learn to thread arbitrary element types.
                let src_ty = self.check_expr(source, env)?;
                match &src_ty {
                    Type::Array(elem) if matches!(**elem, Type::U64) => {}
                    _ => return Err(Error::new(
                        ErrorKind::Type,
                        format!(
                            "`parallel for ... in` source must be `[u64]`, got {src_ty}",
                        ),
                        *span,
                    )),
                }
                // Output must be [T] for some T; body tail must
                // produce a value compatible with T.
                let out_ty = self.check_expr(output, env)?;
                let elem_ty = match &out_ty {
                    Type::Array(e) => (**e).clone(),
                    _ => return Err(Error::new(
                        ErrorKind::Type,
                        format!(
                            "`parallel for ... to` output must be an array, got {out_ty}",
                        ),
                        *span,
                    )),
                };
                // Body type-checks against the outer scope plus the
                // per-iteration id binding.
                env.push(HashMap::new());
                env.last_mut().unwrap().insert(id_var.clone(), Type::U64);
                let body_tail = self.check_block_as_expr(body, env)?;
                env.pop();
                if !types_compatible(&body_tail, &elem_ty) {
                    return Err(Error::new(
                        ErrorKind::Type,
                        format!(
                            "`parallel for ... to` body must yield `{elem_ty}` for the output slot, got `{body_tail}`",
                        ),
                        *span,
                    ));
                }
                Ok(false)
            }
        }
    }

    /// Type-check a comprehension clause sequence into `comp_env`. Each
    /// `For` extends the env with a new binding visible to subsequent clauses
    /// and the mapper. `If` clauses must produce `bool`.
    fn check_comp_clauses(&self, clauses: &[CompClause], comp_env: &mut Env) -> Result<(), Error> {
        for clause in clauses {
            match clause {
                CompClause::For { var, iter } => {
                    let iter_ty = self.check_expr(iter, comp_env)?;
                    // Same sugar as Stmt::For — pmap yields values,
                    // pvec yields elements.
                    let elem_ty = match iter_ty {
                        Type::Array(elem)         => *elem,
                        Type::PMap { value, .. }  => *value,
                        Type::PBTree { value, .. } => *value,
                        Type::PVec { elem }       => *elem,
                        other => return Err(Error::new(
                            ErrorKind::Type,
                            format!("comprehension iterator must be an array, pmap, pbtree, or pvec; got {other}"),
                            iter.span,
                        )),
                    };
                    comp_env.push(HashMap::new());
                    comp_env.last_mut().unwrap().insert(var.clone(), elem_ty);
                }
                CompClause::If(cond) => {
                    let f_ty = self.check_expr(cond, comp_env)?;
                    if f_ty != Type::Bool {
                        return Err(Error::new(
                            ErrorKind::Type,
                            format!("comprehension filter must be bool, got {f_ty}"),
                            cond.span,
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    fn check_string_builtin(
        &self,
        name: &str,
        args: &[Expr],
        env: &Env,
        span: Span,
    ) -> Result<Option<Type>, Error> {
        let arity_err = |needed: usize| {
            Err(Error::new(
                ErrorKind::Type,
                format!("{name}() takes exactly {needed} argument(s)"),
                span,
            ))
        };
        match name {
            "string_concat" => {
                if args.len() != 2 { return arity_err(2); }
                let a = self.check_expr(&args[0], env)?;
                let b = self.check_expr(&args[1], env)?;
                if a != Type::String || b != Type::String {
                    return Err(Error::new(
                        ErrorKind::Type,
                        format!("string_concat() expects (string, string), got ({a}, {b})"),
                        span,
                    ));
                }
                Ok(Some(Type::String))
            }
            "string_slice" => {
                if args.len() != 3 { return arity_err(3); }
                let s = self.check_expr(&args[0], env)?;
                let st = self.check_expr(&args[1], env)?;
                let en = self.check_expr(&args[2], env)?;
                if s != Type::String {
                    return Err(Error::new(
                        ErrorKind::Type,
                        format!("string_slice() arg 0: expected string, got {s}"),
                        args[0].span,
                    ));
                }
                if st != Type::Int || en != Type::Int {
                    return Err(Error::new(
                        ErrorKind::Type,
                        format!("string_slice() arg 1/2: expected i64, got ({st}, {en})"),
                        span,
                    ));
                }
                Ok(Some(Type::String))
            }
            "string_contains" => {
                if args.len() != 2 { return arity_err(2); }
                let h = self.check_expr(&args[0], env)?;
                let n = self.check_expr(&args[1], env)?;
                if h != Type::String || n != Type::String {
                    return Err(Error::new(
                        ErrorKind::Type,
                        format!("string_contains() expects (string, string), got ({h}, {n})"),
                        span,
                    ));
                }
                Ok(Some(Type::Bool))
            }
            _ => Ok(None),
        }
    }

    fn check_bytes_builtin(
        &self,
        name: &str,
        args: &[Expr],
        env: &Env,
        span: Span,
    ) -> Result<Option<Type>, Error> {
        let arity_err = |needed: usize| {
            Err(Error::new(
                ErrorKind::Type,
                format!("{name}() takes exactly {needed} argument(s)"),
                span,
            ))
        };
        match name {
            "to_bytes" => {
                if args.len() != 1 { return arity_err(1); }
                let t = self.check_expr(&args[0], env)?;
                if t != Type::String {
                    return Err(Error::new(
                        ErrorKind::Type,
                        format!("to_bytes() expects string, got {t}"),
                        args[0].span,
                    ));
                }
                Ok(Some(Type::Bytes))
            }
            "bytes_len" => {
                if args.len() != 1 { return arity_err(1); }
                let t = self.check_expr(&args[0], env)?;
                if t != Type::Bytes {
                    return Err(Error::new(
                        ErrorKind::Type,
                        format!("bytes_len() expects bytes, got {t}"),
                        args[0].span,
                    ));
                }
                Ok(Some(Type::Int))
            }
            "bytes_concat" => {
                if args.len() != 2 { return arity_err(2); }
                let a = self.check_expr(&args[0], env)?;
                let b = self.check_expr(&args[1], env)?;
                if a != Type::Bytes || b != Type::Bytes {
                    return Err(Error::new(
                        ErrorKind::Type,
                        format!("bytes_concat() expects (bytes, bytes), got ({a}, {b})"),
                        span,
                    ));
                }
                Ok(Some(Type::Bytes))
            }
            "to_be_bytes" => {
                if args.len() != 1 { return arity_err(1); }
                let t = self.check_expr(&args[0], env)?;
                if !matches!(t, Type::U32 | Type::U64 | Type::U128 | Type::I32) {
                    return Err(Error::new(
                        ErrorKind::Type,
                        format!("to_be_bytes() expects a fixed-width int (u32/u64/u128/i32), got {t}"),
                        args[0].span,
                    ));
                }
                Ok(Some(Type::Bytes))
            }
            "bit_not_bytes" => {
                if args.len() != 1 { return arity_err(1); }
                let t = self.check_expr(&args[0], env)?;
                if t != Type::Bytes {
                    return Err(Error::new(
                        ErrorKind::Type,
                        format!("bit_not_bytes() expects bytes, got {t}"),
                        args[0].span,
                    ));
                }
                Ok(Some(Type::Bytes))
            }
            "bytes_eq" => {
                if args.len() != 2 { return arity_err(2); }
                let a = self.check_expr(&args[0], env)?;
                let b = self.check_expr(&args[1], env)?;
                if a != Type::Bytes || b != Type::Bytes {
                    return Err(Error::new(
                        ErrorKind::Type,
                        format!("bytes_eq() expects (bytes, bytes), got ({a}, {b})"),
                        span,
                    ));
                }
                Ok(Some(Type::Bool))
            }
            "bytes_slice" => {
                if args.len() != 3 { return arity_err(3); }
                let b_ty = self.check_expr(&args[0], env)?;
                let s_ty = self.check_expr(&args[1], env)?;
                let e_ty = self.check_expr(&args[2], env)?;
                if b_ty != Type::Bytes {
                    return Err(Error::new(
                        ErrorKind::Type,
                        format!("bytes_slice() arg 0: expected bytes, got {b_ty}"),
                        args[0].span,
                    ));
                }
                if s_ty != Type::Int || e_ty != Type::Int {
                    return Err(Error::new(
                        ErrorKind::Type,
                        format!("bytes_slice() arg 1/2: expected i64, got ({s_ty}, {e_ty})"),
                        span,
                    ));
                }
                Ok(Some(Type::Bytes))
            }
            _ => Ok(None),
        }
    }

    fn check_collection_builtin(
        &self,
        name: &str,
        args: &[Expr],
        env: &Env,
        span: Span,
    ) -> Result<Option<Type>, Error> {
        let arity_err = |needed: usize| {
            Err(Error::new(
                ErrorKind::Type,
                format!("{name}() takes exactly {needed} argument(s)"),
                span,
            ))
        };
        match name {
            "set_insert" | "set_remove" => {
                if args.len() != 2 { return arity_err(2); }
                let s_ty = self.check_expr(&args[0], env)?;
                let Type::Set(elem) = s_ty.clone() else {
                    return Err(Error::new(
                        ErrorKind::Type,
                        format!("{name}() expects set<T>, got {s_ty}"),
                        args[0].span,
                    ));
                };
                let v_ty = self.check_expr(&args[1], env)?;
                if v_ty != *elem {
                    return Err(Error::new(
                        ErrorKind::Type,
                        format!("{name}() element type mismatch: expected {elem}, got {v_ty}"),
                        args[1].span,
                    ));
                }
                Ok(Some(s_ty))
            }
            "set_contains" => {
                if args.len() != 2 { return arity_err(2); }
                let s_ty = self.check_expr(&args[0], env)?;
                let Type::Set(elem) = s_ty.clone() else {
                    return Err(Error::new(
                        ErrorKind::Type,
                        format!("set_contains() expects set<T>, got {s_ty}"),
                        args[0].span,
                    ));
                };
                let v_ty = self.check_expr(&args[1], env)?;
                if v_ty != *elem {
                    return Err(Error::new(
                        ErrorKind::Type,
                        format!("set_contains() element type mismatch: expected {elem}, got {v_ty}"),
                        args[1].span,
                    ));
                }
                Ok(Some(Type::Bool))
            }
            "set_len" => {
                if args.len() != 1 { return arity_err(1); }
                let s_ty = self.check_expr(&args[0], env)?;
                if !matches!(s_ty, Type::Set(_)) {
                    return Err(Error::new(
                        ErrorKind::Type,
                        format!("set_len() expects set<T>, got {s_ty}"),
                        args[0].span,
                    ));
                }
                Ok(Some(Type::Int))
            }
            "dict_set" => {
                if args.len() != 3 { return arity_err(3); }
                let d_ty = self.check_expr(&args[0], env)?;
                let Type::Dict { key, value } = d_ty.clone() else {
                    return Err(Error::new(
                        ErrorKind::Type,
                        format!("dict_set() expects dict<K, V>, got {d_ty}"),
                        args[0].span,
                    ));
                };
                let k_ty = self.check_expr(&args[1], env)?;
                if k_ty != *key {
                    return Err(Error::new(
                        ErrorKind::Type,
                        format!("dict_set() key type mismatch: expected {key}, got {k_ty}"),
                        args[1].span,
                    ));
                }
                let v_ty = self.check_expr(&args[2], env)?;
                if v_ty != *value {
                    return Err(Error::new(
                        ErrorKind::Type,
                        format!("dict_set() value type mismatch: expected {value}, got {v_ty}"),
                        args[2].span,
                    ));
                }
                Ok(Some(d_ty))
            }
            "dict_remove" => {
                if args.len() != 2 { return arity_err(2); }
                let d_ty = self.check_expr(&args[0], env)?;
                let Type::Dict { key, .. } = d_ty.clone() else {
                    return Err(Error::new(
                        ErrorKind::Type,
                        format!("dict_remove() expects dict<K, V>, got {d_ty}"),
                        args[0].span,
                    ));
                };
                let k_ty = self.check_expr(&args[1], env)?;
                if k_ty != *key {
                    return Err(Error::new(
                        ErrorKind::Type,
                        format!("dict_remove() key type mismatch: expected {key}, got {k_ty}"),
                        args[1].span,
                    ));
                }
                Ok(Some(d_ty))
            }
            "dict_get" => {
                if args.len() != 3 { return arity_err(3); }
                let d_ty = self.check_expr(&args[0], env)?;
                let Type::Dict { key, value } = d_ty.clone() else {
                    return Err(Error::new(
                        ErrorKind::Type,
                        format!("dict_get() expects dict<K, V>, got {d_ty}"),
                        args[0].span,
                    ));
                };
                let k_ty = self.check_expr(&args[1], env)?;
                if k_ty != *key {
                    return Err(Error::new(
                        ErrorKind::Type,
                        format!("dict_get() key type mismatch: expected {key}, got {k_ty}"),
                        args[1].span,
                    ));
                }
                let default_ty = self.check_expr(&args[2], env)?;
                if default_ty != *value {
                    return Err(Error::new(
                        ErrorKind::Type,
                        format!("dict_get() default type mismatch: expected {value}, got {default_ty}"),
                        args[2].span,
                    ));
                }
                Ok(Some(*value))
            }
            "dict_has" => {
                if args.len() != 2 { return arity_err(2); }
                let d_ty = self.check_expr(&args[0], env)?;
                let Type::Dict { key, .. } = d_ty.clone() else {
                    return Err(Error::new(
                        ErrorKind::Type,
                        format!("dict_has() expects dict<K, V>, got {d_ty}"),
                        args[0].span,
                    ));
                };
                let k_ty = self.check_expr(&args[1], env)?;
                if k_ty != *key {
                    return Err(Error::new(
                        ErrorKind::Type,
                        format!("dict_has() key type mismatch: expected {key}, got {k_ty}"),
                        args[1].span,
                    ));
                }
                Ok(Some(Type::Bool))
            }
            "dict_len" => {
                if args.len() != 1 { return arity_err(1); }
                let d_ty = self.check_expr(&args[0], env)?;
                if !matches!(d_ty, Type::Dict { .. }) {
                    return Err(Error::new(
                        ErrorKind::Type,
                        format!("dict_len() expects dict<K, V>, got {d_ty}"),
                        args[0].span,
                    ));
                }
                Ok(Some(Type::Int))
            }
            _ => Ok(None),
        }
    }

    fn check_lvalue(&self, target: &Expr, env: &Env) -> Result<Type, Error> {
        match &target.kind {
            ExprKind::Ident(name) => {
                // A Copy-typed local binding is assignable.
                for scope in env.iter().rev() {
                    if let Some(t) = scope.get(name) {
                        if !t.is_copy() {
                            return Err(Error::new(
                                ErrorKind::Type,
                                format!("local '{name}' has non-Copy type {t}; cannot reassign"),
                                target.span,
                            ));
                        }
                        return Ok(t.clone());
                    }
                }
                // Otherwise it must be a state slot.
                self.states.get(name).cloned().ok_or_else(|| {
                    Error::new(
                        ErrorKind::Type,
                        format!("'{name}' is not assignable; only `state` slots and Copy locals can be assigned"),
                        target.span,
                    )
                })
            }
            ExprKind::Field { target: inner, name } => {
                let inner_ty = self.check_lvalue(inner, env)?;
                let Type::Struct { fields, .. } = inner_ty else {
                    return Err(Error::new(
                        ErrorKind::Type,
                        format!("field assignment requires a struct path, got {inner_ty}"),
                        target.span,
                    ));
                };
                fields
                    .iter()
                    .find(|(n, _)| n == name)
                    .map(|(_, t)| t.clone())
                    .ok_or_else(|| Error::new(
                        ErrorKind::Type,
                        format!("no field '{name}' on struct"),
                        target.span,
                    ))
            }
            ExprKind::Index { target: t, key } => {
                let ExprKind::Ident(name) = &t.kind else {
                    return Err(Error::new(
                        ErrorKind::Type,
                        "indexed assignment target must be a state name",
                        target.span,
                    ));
                };
                let map_ty = self.states.get(name).ok_or_else(|| {
                    Error::new(
                        ErrorKind::Type,
                        format!("'{name}' is not a state slot"),
                        t.span,
                    )
                })?;
                // pvec uses an i64 index; map and pmap use a typed
                // key. Dispatch on the state's declared type.
                if let Type::PVec { elem } = map_ty {
                    let actual_key = self.check_expr(key, env)?;
                    if actual_key != Type::Int {
                        return Err(Error::new(
                            ErrorKind::Type,
                            format!("pvec index must be i64, got {actual_key}"),
                            key.span,
                        ));
                    }
                    return Ok((**elem).clone());
                }
                let (kt, vt) = match map_ty {
                    Type::Map { key, value } => (key, value),
                    Type::PMap { key, value } => (key, value),
                    Type::PBTree { key, value } => (key, value),
                    _ => return Err(Error::new(
                        ErrorKind::Type,
                        format!("'{name}' is not a map / pmap / pvec state"),
                        t.span,
                    )),
                };
                let actual_key = self.check_expr(key, env)?;
                if &actual_key != kt.as_ref() {
                    return Err(Error::new(
                        ErrorKind::Type,
                        format!("map key: expected {kt}, got {actual_key}"),
                        key.span,
                    ));
                }
                Ok((**vt).clone())
            }
            _ => Err(Error::new(
                ErrorKind::Type,
                "invalid assignment target",
                target.span,
            )),
        }
    }

    fn check_if(&self, ifs: &IfStmt, env: &mut Env, expected_ret: &Type) -> Result<bool, Error> {
        let cond_ty = self.check_expr(&ifs.cond, env)?;
        if cond_ty != Type::Bool {
            return Err(Error::new(
                ErrorKind::Type,
                format!("if condition must be bool, got {cond_ty}"),
                ifs.cond.span,
            ));
        }
        let then_ret = self.check_block(&ifs.then, env, expected_ret)?;
        let else_ret = match &ifs.else_branch {
            ElseBranch::None => false,
            ElseBranch::Block(b) => self.check_block(b, env, expected_ret)?,
            ElseBranch::If(inner) => self.check_if(inner, env, expected_ret)?,
        };
        Ok(then_ret && else_ret)
    }

    fn check_expr(&self, expr: &Expr, env: &Env) -> Result<Type, Error> {
        self.check_expr_with_hint(expr, env, None)
    }

    /// Type-check `expr`, optionally biased by an outer type hint. The hint is
    /// only consulted for empty literals (`[]`, `set{}`, `dict{}`) — every
    /// other shape is checked the same way regardless. The caller is
    /// responsible for verifying that the returned type matches the hint.
    fn check_expr_with_hint(
        &self,
        expr: &Expr,
        env: &Env,
        hint: Option<&Type>,
    ) -> Result<Type, Error> {
        match &expr.kind {
            ExprKind::Array(elems) if elems.is_empty() => {
                let Some(Type::Array(elem)) = hint else {
                    return Err(Error::new(
                        ErrorKind::Type,
                        "empty array literal needs a `let x: [T] = ...` annotation".to_string(),
                        expr.span,
                    ));
                };
                return Ok(Type::Array(elem.clone()));
            }
            ExprKind::SetLit(elems) if elems.is_empty() => {
                let Some(Type::Set(elem)) = hint else {
                    return Err(Error::new(
                        ErrorKind::Type,
                        "empty set literal needs a `let x: set<T> = ...` annotation".to_string(),
                        expr.span,
                    ));
                };
                if !elem.is_keyable() {
                    return Err(Error::new(
                        ErrorKind::Type,
                        format!("set element type {elem} is not keyable"),
                        expr.span,
                    ));
                }
                return Ok(Type::Set(elem.clone()));
            }
            ExprKind::DictLit(pairs) if pairs.is_empty() => {
                let Some(Type::Dict { key, value }) = hint else {
                    return Err(Error::new(
                        ErrorKind::Type,
                        "empty dict literal needs a `let x: dict<K, V> = ...` annotation".to_string(),
                        expr.span,
                    ));
                };
                if !key.is_keyable() {
                    return Err(Error::new(
                        ErrorKind::Type,
                        format!("dict key type {key} is not keyable"),
                        expr.span,
                    ));
                }
                return Ok(Type::Dict { key: key.clone(), value: value.clone() });
            }
            _ => {}
        }
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
            ExprKind::Prev => self
                .pipe_stack
                .borrow()
                .last()
                .cloned()
                .ok_or_else(|| Error::new(
                    ErrorKind::Type,
                    "$$ used outside of a pipe stage".to_string(),
                    expr.span,
                )),
            ExprKind::Pipe { head, step } => {
                let head_ty = self.check_expr(head, env)?;
                self.pipe_stack.borrow_mut().push(head_ty);
                let result = self.check_expr(step, env);
                self.pipe_stack.borrow_mut().pop();
                result
            }
            ExprKind::Bool(_) => Ok(Type::Bool),
            ExprKind::Str(_) => Ok(Type::String),
            ExprKind::Ident(name) => {
                for s in env.iter().rev() {
                    if let Some(t) = s.get(name) {
                        return Ok(t.clone());
                    }
                }
                if let Some(t) = self.states.get(name) {
                    return Ok(t.clone());
                }
                if let Some(t) = self.consts.get(name) {
                    return Ok(t.clone());
                }
                Err(Error::new(
                    ErrorKind::Type,
                    format!("undefined variable '{name}'"),
                    expr.span,
                ))
            }
            ExprKind::Binary { op, lhs, rhs } => {
                let l = self.check_expr(lhs, env)?;
                let r = self.check_expr(rhs, env)?;
                check_binop(*op, &l, &r, expr.span)
            }
            ExprKind::Unary { op, operand } => {
                let v = self.check_expr(operand, env)?;
                check_unop(*op, &v, expr.span)
            }
            ExprKind::Index { target, key } => {
                // Two cases: indexing a map state, or indexing an array value.
                if let ExprKind::Ident(name) = &target.kind {
                    if let Some(Type::Map { key: kt, value: vt }) = self.states.get(name) {
                        let actual_key = self.check_expr(key, env)?;
                        if &actual_key != kt.as_ref() {
                            return Err(Error::new(
                                ErrorKind::Type,
                                format!("map key: expected {kt}, got {actual_key}"),
                                key.span,
                            ));
                        }
                        return Ok((**vt).clone());
                    }
                    // pmap state: same shape on the user side as map.
                    if let Some(Type::PMap { key: kt, value: vt }) = self.states.get(name) {
                        let actual_key = self.check_expr(key, env)?;
                        if &actual_key != kt.as_ref() {
                            return Err(Error::new(
                                ErrorKind::Type,
                                format!("pmap key: expected {kt}, got {actual_key}"),
                                key.span,
                            ));
                        }
                        return Ok((**vt).clone());
                    }
                    // pbtree state: K → V indexing. Slice-1 supported
                    // key types are `u64` (point queries / monotonic
                    // sequences) and `u128` (composite-index packed
                    // keys). The pbtree::set runtime re-validates if
                    // anything sneaks past.
                    if let Some(Type::PBTree { key: kt, value: vt }) = self.states.get(name) {
                        if !matches!(kt.as_ref(), Type::U64 | Type::U128 | Type::Bytes) {
                            return Err(Error::new(
                                ErrorKind::Type,
                                format!(
                                    "pbtree supports u64, u128, or bytes keys; '{name}' uses pbtree<{kt}, _>",
                                ),
                                target.span,
                            ));
                        }
                        let actual_key = self.check_expr(key, env)?;
                        if &actual_key != kt.as_ref() {
                            return Err(Error::new(
                                ErrorKind::Type,
                                format!("pbtree key: expected {kt}, got {actual_key}"),
                                key.span,
                            ));
                        }
                        return Ok((**vt).clone());
                    }
                    // pvec state: indexed by i64.
                    if let Some(Type::PVec { elem }) = self.states.get(name) {
                        let actual_key = self.check_expr(key, env)?;
                        if actual_key != Type::Int {
                            return Err(Error::new(
                                ErrorKind::Type,
                                format!("pvec index must be i64, got {actual_key}"),
                                key.span,
                            ));
                        }
                        return Ok((**elem).clone());
                    }
                }
                let target_ty = self.check_expr(target, env)?;
                if let Type::Array(elem) = target_ty {
                    let key_ty = self.check_expr(key, env)?;
                    if key_ty != Type::Int {
                        return Err(Error::new(
                            ErrorKind::Type,
                            format!("array index must be i64, got {key_ty}"),
                            key.span,
                        ));
                    }
                    return Ok(*elem);
                }
                Err(Error::new(
                    ErrorKind::Type,
                    "indexing requires a map state or array".to_string(),
                    target.span,
                ))
            }
            ExprKind::StructLit { name, fields } => {
                // Caps share struct-literal syntax. Dispatch to the cap
                // path first; cap construction is restricted to the
                // declaring module. A struct literal is allowed
                // anywhere (caps included) only if the name resolves
                // to one of the two.
                let (decl, is_cap) = if let Some((cap_fields, owner)) = self.caps.get(name) {
                    if owner != &self.current_module {
                        return Err(Error::new(
                            ErrorKind::Type,
                            format!(
                                "cap '{name}' can only be constructed inside its \
                                 declaring module '{owner}', not '{}'",
                                self.current_module,
                            ),
                            expr.span,
                        ));
                    }
                    (cap_fields.clone(), true)
                } else if let Some(s) = self.structs.get(name) {
                    (s.clone(), false)
                } else {
                    return Err(Error::new(
                        ErrorKind::Type,
                        format!("unknown struct '{name}'"),
                        expr.span,
                    ));
                };
                if decl.len() != fields.len() {
                    return Err(Error::new(
                        ErrorKind::Type,
                        format!(
                            "{} '{name}' has {} field(s); literal provides {}",
                            if is_cap { "cap" } else { "struct" },
                            decl.len(),
                            fields.len()
                        ),
                        expr.span,
                    ));
                }
                for (declared_name, declared_ty) in decl.iter() {
                    let provided = fields
                        .iter()
                        .find(|(n, _)| n == declared_name)
                        .ok_or_else(|| {
                            Error::new(
                                ErrorKind::Type,
                                format!(
                                    "missing field '{declared_name}' in {} '{name}'",
                                    if is_cap { "cap" } else { "struct" },
                                ),
                                expr.span,
                            )
                        })?;
                    let actual = self.check_expr(&provided.1, env)?;
                    if &actual != declared_ty {
                        return Err(Error::new(
                            ErrorKind::Type,
                            format!(
                                "field '{declared_name}': expected {declared_ty}, got {actual}",
                            ),
                            provided.1.span,
                        ));
                    }
                }
                if is_cap {
                    Ok(Type::Cap {
                        name: name.clone(),
                        fields: decl,
                        owner_module: self.current_module.clone(),
                    })
                } else {
                    // Carry the declaration's group annotations so a
                    // freshly-constructed literal's type equals the
                    // declared `Type::Struct` (assignment to state
                    // checks `==`, including `field_groups`).
                    let field_groups = self
                        .struct_groups
                        .get(name)
                        .cloned()
                        .unwrap_or_default();
                    Ok(Type::Struct { name: name.clone(), fields: decl, field_groups })
                }
            }
            ExprKind::Field { target, name } => {
                let t = self.check_expr(target, env)?;
                // Both structs and caps support field access. Caps are
                // read-only via their fields — no Field-as-lvalue path
                // exists in the lvalue handler for caps.
                let fields = match t {
                    Type::Struct { fields, .. } => fields,
                    Type::Cap { fields, .. } => fields,
                    other => return Err(Error::new(
                        ErrorKind::Type,
                        format!("field access requires a struct or cap; got {other}"),
                        target.span,
                    )),
                };
                fields
                    .iter()
                    .find(|(n, _)| n == name)
                    .map(|(_, ty)| ty.clone())
                    .ok_or_else(|| Error::new(
                        ErrorKind::Type,
                        format!("no field '{name}'"),
                        expr.span,
                    ))
            }
            ExprKind::SetLit(elems) => {
                if elems.is_empty() {
                    return Err(Error::new(
                        ErrorKind::Type,
                        "empty set literal — annotation required (use `set{first} - set{first}` or call set_remove)".to_string(),
                        expr.span,
                    ));
                }
                let first = self.check_expr(&elems[0], env)?;
                if !first.is_keyable() {
                    return Err(Error::new(
                        ErrorKind::Type,
                        format!("set element type {first} is not keyable"),
                        elems[0].span,
                    ));
                }
                for (i, e) in elems.iter().enumerate().skip(1) {
                    let t = self.check_expr(e, env)?;
                    if t != first {
                        return Err(Error::new(
                            ErrorKind::Type,
                            format!("set element {i} has type {t}, expected {first}"),
                            e.span,
                        ));
                    }
                }
                Ok(Type::Set(Box::new(first)))
            }
            ExprKind::SetComp { mapper, clauses } => {
                let mut comp_env = env.clone();
                self.check_comp_clauses(clauses, &mut comp_env)?;
                let m_ty = self.check_expr(mapper, &comp_env)?;
                if !m_ty.is_keyable() {
                    return Err(Error::new(
                        ErrorKind::Type,
                        format!("set element type {m_ty} is not keyable"),
                        mapper.span,
                    ));
                }
                Ok(Type::Set(Box::new(m_ty)))
            }
            ExprKind::DictLit(pairs) => {
                if pairs.is_empty() {
                    return Err(Error::new(
                        ErrorKind::Type,
                        "empty dict literal — annotation required (use a non-empty literal or comprehension)".to_string(),
                        expr.span,
                    ));
                }
                let (k0, v0) = &pairs[0];
                let kt = self.check_expr(k0, env)?;
                let vt = self.check_expr(v0, env)?;
                if !kt.is_keyable() {
                    return Err(Error::new(
                        ErrorKind::Type,
                        format!("dict key type {kt} is not keyable"),
                        k0.span,
                    ));
                }
                for (i, (k, v)) in pairs.iter().enumerate().skip(1) {
                    let k_ty = self.check_expr(k, env)?;
                    if k_ty != kt {
                        return Err(Error::new(
                            ErrorKind::Type,
                            format!("dict key {i} has type {k_ty}, expected {kt}"),
                            k.span,
                        ));
                    }
                    let v_ty = self.check_expr(v, env)?;
                    if v_ty != vt {
                        return Err(Error::new(
                            ErrorKind::Type,
                            format!("dict value {i} has type {v_ty}, expected {vt}"),
                            v.span,
                        ));
                    }
                }
                Ok(Type::Dict { key: Box::new(kt), value: Box::new(vt) })
            }
            ExprKind::DictComp { key, value, clauses } => {
                let mut comp_env = env.clone();
                self.check_comp_clauses(clauses, &mut comp_env)?;
                let kt = self.check_expr(key, &comp_env)?;
                if !kt.is_keyable() {
                    return Err(Error::new(
                        ErrorKind::Type,
                        format!("dict key type {kt} is not keyable"),
                        key.span,
                    ));
                }
                let vt = self.check_expr(value, &comp_env)?;
                Ok(Type::Dict { key: Box::new(kt), value: Box::new(vt) })
            }
            ExprKind::ListComp { mapper, clauses } => {
                let mut comp_env = env.clone();
                self.check_comp_clauses(clauses, &mut comp_env)?;
                let m_ty = self.check_expr(mapper, &comp_env)?;
                Ok(Type::Array(Box::new(m_ty)))
            }
            ExprKind::Array(elems) => {
                if elems.is_empty() {
                    return Err(Error::new(
                        ErrorKind::Type,
                        "empty array literals not yet supported (need a typed let binding)".to_string(),
                        expr.span,
                    ));
                }
                let first = self.check_expr(&elems[0], env)?;
                for (i, e) in elems.iter().enumerate().skip(1) {
                    let t = self.check_expr(e, env)?;
                    if t != first {
                        return Err(Error::new(
                            ErrorKind::Type,
                            format!("array element {i} has type {t}, expected {first}"),
                            e.span,
                        ));
                    }
                }
                Ok(Type::Array(Box::new(first)))
            }
            ExprKind::DynCall { target_ident, method, args, .. } => {
                // Resolve target_ident to a Type::Interface. Locals
                // win over states (shadowing); fall back to states.
                let mut target_ty: Option<Type> = None;
                for s in env.iter().rev() {
                    if let Some(t) = s.get(target_ident) {
                        target_ty = Some(t.clone());
                        break;
                    }
                }
                if target_ty.is_none() {
                    target_ty = self.states.get(target_ident).cloned();
                }
                let target_ty = target_ty.ok_or_else(|| Error::new(
                    ErrorKind::Type,
                    format!("undefined identifier '{target_ident}' in dynamic dispatch"),
                    expr.span,
                ))?;
                let methods = match &target_ty {
                    Type::Interface { methods, .. } => methods,
                    other => return Err(Error::new(
                        ErrorKind::Type,
                        format!(
                            "dynamic dispatch needs an interface value; \
                             '{target_ident}' has type {other}",
                        ),
                        expr.span,
                    )),
                };
                let sig = methods.iter().find(|m| &m.name == method)
                    .ok_or_else(|| Error::new(
                        ErrorKind::Type,
                        format!(
                            "interface {target_ty} has no method '{method}'",
                        ),
                        expr.span,
                    ))?;
                if sig.params.len() != args.len() {
                    return Err(Error::new(
                        ErrorKind::Type,
                        format!(
                            "{target_ty}::{method} expects {} arg(s), got {}",
                            sig.params.len(), args.len(),
                        ),
                        expr.span,
                    ));
                }
                for (i, (a, expected)) in args.iter().zip(sig.params.iter()).enumerate() {
                    let actual = self.check_expr(a, env)?;
                    if !types_compatible(&actual, expected) {
                        return Err(Error::new(
                            ErrorKind::Type,
                            format!(
                                "arg {i} of '{target_ident}::{method}': expected {expected}, got {actual}",
                            ),
                            a.span,
                        ));
                    }
                }
                return Ok(sig.return_type.clone());
            }
            ExprKind::Call { module: Some(mod_name), name, args }
                if name == "bind" && (
                    self.interfaces.contains_key(mod_name)
                    || self.iface_externals.contains_key(mod_name)
                ) =>
            {
                // `IFace::bind(target_module: string)` constructor.
                // Looks up `IFace` in the local interfaces first,
                // then in dep-artifact interfaces — txs can bind
                // to interfaces declared in deployed programs.
                if args.len() != 1 {
                    return Err(Error::new(
                        ErrorKind::Type,
                        format!("{mod_name}::bind expects exactly 1 string argument"),
                        expr.span,
                    ));
                }
                let arg_ty = self.check_expr(&args[0], env)?;
                if arg_ty != Type::String {
                    return Err(Error::new(
                        ErrorKind::Type,
                        format!("{mod_name}::bind: target module must be a string, got {arg_ty}"),
                        args[0].span,
                    ));
                }
                let methods = self.interfaces.get(mod_name)
                    .cloned()
                    .or_else(|| self.iface_externals.get(mod_name).cloned())
                    .unwrap();
                return Ok(Type::Interface { name: mod_name.clone(), methods });
            }
            ExprKind::Call { module: Some(mod_name), name, args } => {
                // Cross-module call. Look up signature in the externals
                // manifest. If absent, defer validation to the link step.
                let sig = self.externals.get(&format!("{mod_name}::{name}"));
                if let Some(sig) = sig {
                    if sig.params.len() != args.len() {
                        return Err(Error::new(
                            ErrorKind::Type,
                            format!(
                                "{mod_name}::{name} expects {} arg(s), got {}",
                                sig.params.len(),
                                args.len()
                            ),
                            expr.span,
                        ));
                    }
                    for (i, (a, expected)) in args.iter().zip(sig.params.iter()).enumerate() {
                        let actual = self.check_expr(a, env)?;
                        if !types_compatible(&actual, expected) {
                            return Err(Error::new(
                                ErrorKind::Type,
                                format!("arg {i} of '{mod_name}::{name}': expected {expected}, got {actual}"),
                                a.span,
                            ));
                        }
                    }
                    Ok(sig.ret.clone())
                } else {
                    // No manifest provided — single-module path. Validate
                    // arg count only and return Int as a placeholder.
                    for a in args { let _ = self.check_expr(a, env)?; }
                    Ok(Type::Int)
                }
            }
            ExprKind::Call { module: None, name, args } => {
                // Type-conversion builtins.
                let conv_target: Option<Type> = match name.as_str() {
                    "i64" => Some(Type::Int),
                    "i32" => Some(Type::I32),
                    "u32" => Some(Type::U32),
                    "u64" => Some(Type::U64),
                    "u128" => Some(Type::U128),
                    _ => None,
                };
                if let Some(target_ty) = conv_target {
                    if args.len() != 1 {
                        return Err(Error::new(
                            ErrorKind::Type,
                            format!("{name}() takes exactly one argument"),
                            expr.span,
                        ));
                    }
                    let arg_ty = self.check_expr(&args[0], env)?;
                    if !is_int(&arg_ty) {
                        return Err(Error::new(
                            ErrorKind::Type,
                            format!("{name}() expects an integer argument, got {arg_ty}"),
                            args[0].span,
                        ));
                    }
                    return Ok(target_ty);
                }
                // JSON builtins. All take dynamic json values; only
                // `parse_json` takes a string.
                if matches!(name.as_str(),
                    "json_get_field" | "json_get_index"
                    | "json_to_string" | "json_to_i64" | "json_to_u64"
                    | "json_to_bool" | "json_is_null" | "json_stringify"
                ) {
                    let n = args.len();
                    let needed: usize = match name.as_str() {
                        "json_get_field" | "json_get_index" => 2,
                        _ => 1,
                    };
                    if n != needed {
                        return Err(Error::new(
                            ErrorKind::Type,
                            format!("{name}() takes exactly {needed} argument(s)"),
                            expr.span,
                        ));
                    }
                    let target_ty = self.check_expr(&args[0], env)?;
                    if target_ty != Type::Json {
                        return Err(Error::new(
                            ErrorKind::Type,
                            format!("{name}(): first arg must be json, got {target_ty}"),
                            args[0].span,
                        ));
                    }
                    if name == "json_get_field" {
                        let key_ty = self.check_expr(&args[1], env)?;
                        if key_ty != Type::String {
                            return Err(Error::new(
                                ErrorKind::Type,
                                format!("json_get_field(): key must be string, got {key_ty}"),
                                args[1].span,
                            ));
                        }
                        return Ok(Type::Json);
                    }
                    if name == "json_get_index" {
                        let idx_ty = self.check_expr(&args[1], env)?;
                        if idx_ty != Type::Int && idx_ty != Type::U64 {
                            return Err(Error::new(
                                ErrorKind::Type,
                                format!("json_get_index(): index must be i64 or u64, got {idx_ty}"),
                                args[1].span,
                            ));
                        }
                        return Ok(Type::Json);
                    }
                    return Ok(match name.as_str() {
                        "json_to_string" | "json_stringify" => Type::String,
                        "json_to_i64" => Type::Int,
                        "json_to_u64" => Type::U64,
                        "json_to_bool" | "json_is_null" => Type::Bool,
                        _ => unreachable!(),
                    });
                }
                if name == "parse_json" {
                    if args.len() != 1 {
                        return Err(Error::new(
                            ErrorKind::Type,
                            "parse_json(string) takes exactly 1 argument".to_string(),
                            expr.span,
                        ));
                    }
                    let arg_ty = self.check_expr(&args[0], env)?;
                    if arg_ty != Type::String {
                        return Err(Error::new(
                            ErrorKind::Type,
                            format!("parse_json(): arg must be string, got {arg_ty}"),
                            args[0].span,
                        ));
                    }
                    return Ok(Type::Json);
                }
                if name == "len" {
                    if args.len() != 1 {
                        return Err(Error::new(
                            ErrorKind::Type,
                            "len() takes exactly one argument".to_string(),
                            expr.span,
                        ));
                    }
                    let t = self.check_expr(&args[0], env)?;
                    return match t {
                        Type::Array(_) | Type::String | Type::Set(_) | Type::Dict { .. } => Ok(Type::Int),
                        other => Err(Error::new(
                            ErrorKind::Type,
                            format!("len() requires array/string/set/dict, got {other}"),
                            args[0].span,
                        )),
                    };
                }
                // Aggregations: sum/max/min over arrays.
                //   sum(xs: [T], init: T) -> T            (init seeds the fold)
                //   max(xs: [T], default: T) -> T         (default returned if xs is empty)
                //   min(xs: [T], default: T) -> T
                // User-defined functions with these names win — the
                // builtin only fires when there's no shadowing decl.
                if matches!(name.as_str(), "sum" | "max" | "min")
                    && !self.sigs.contains_key(name)
                {
                    if args.len() != 2 {
                        return Err(Error::new(
                            ErrorKind::Type,
                            format!("{name}(array, init) takes exactly 2 arguments"),
                            expr.span,
                        ));
                    }
                    let xs_ty = self.check_expr(&args[0], env)?;
                    let elem = match xs_ty {
                        Type::Array(elem) => *elem,
                        other => return Err(Error::new(
                            ErrorKind::Type,
                            format!("{name}() expects an array, got {other}"),
                            args[0].span,
                        )),
                    };
                    let init_ty = self.check_expr(&args[1], env)?;
                    if init_ty != elem {
                        return Err(Error::new(
                            ErrorKind::Type,
                            format!(
                                "{name}() init/default must match element type: array of {elem}, init of {init_ty}",
                            ),
                            args[1].span,
                        ));
                    }
                    if name == "sum" && !is_int(&elem) {
                        return Err(Error::new(
                            ErrorKind::Type,
                            format!("sum() requires an integer element type, got {elem}"),
                            args[0].span,
                        ));
                    }
                    return Ok(elem);
                }
                if matches!(name.as_str(), "msg_sender" | "block_timestamp" | "block_number") {
                    if !args.is_empty() {
                        return Err(Error::new(
                            ErrorKind::Type,
                            format!("{name}() takes no arguments"),
                            expr.span,
                        ));
                    }
                    return Ok(match name.as_str() {
                        "msg_sender" => Type::Address,
                        "block_timestamp" | "block_number" => Type::U64,
                        _ => unreachable!(),
                    });
                }
                // bytes builtins
                if let Some(ret_ty) = self.check_bytes_builtin(name, args, env, expr.span)? {
                    return Ok(ret_ty);
                }
                // string builtins
                if let Some(ret_ty) = self.check_string_builtin(name, args, env, expr.span)? {
                    return Ok(ret_ty);
                }
                if name == "assert" {
                    if args.is_empty() || args.len() > 2 {
                        return Err(Error::new(
                            ErrorKind::Type,
                            "assert(cond: bool [, msg: string])".to_string(),
                            expr.span,
                        ));
                    }
                    let cond_ty = self.check_expr(&args[0], env)?;
                    if cond_ty != Type::Bool {
                        return Err(Error::new(
                            ErrorKind::Type,
                            format!("assert() condition must be bool, got {cond_ty}"),
                            args[0].span,
                        ));
                    }
                    if let Some(msg) = args.get(1) {
                        let msg_ty = self.check_expr(msg, env)?;
                        if msg_ty != Type::String {
                            return Err(Error::new(
                                ErrorKind::Type,
                                format!("assert() message must be string, got {msg_ty}"),
                                msg.span,
                            ));
                        }
                    }
                    return Ok(Type::Unit);
                }
                // set_* / dict_* polymorphic builtins
                if let Some(ret_ty) = self.check_collection_builtin(name, args, env, expr.span)? {
                    return Ok(ret_ty);
                }
                // pvec builtins. Both take a pvec state name first.
                if matches!(name.as_str(), "pvec_push" | "pvec_len") {
                    let state_name = match args.first().map(|a| &a.kind) {
                        Some(ExprKind::Ident(n)) => n.clone(),
                        _ => return Err(Error::new(
                            ErrorKind::Type,
                            format!("{name}() arg 0 must name a pvec state slot"),
                            expr.span,
                        )),
                    };
                    let st = self.states.get(&state_name).cloned().ok_or_else(|| Error::new(
                        ErrorKind::Type,
                        format!("'{state_name}' is not a state slot"),
                        expr.span,
                    ))?;
                    let Type::PVec { elem } = st else {
                        return Err(Error::new(
                            ErrorKind::Type,
                            format!("{name}() expects a pvec state, got {st}"),
                            expr.span,
                        ));
                    };
                    if name == "pvec_len" {
                        if args.len() != 1 {
                            return Err(Error::new(
                                ErrorKind::Type,
                                "pvec_len(state) takes exactly 1 argument".to_string(),
                                expr.span,
                            ));
                        }
                        return Ok(Type::U64);
                    }
                    // pvec_push
                    if args.len() != 2 {
                        return Err(Error::new(
                            ErrorKind::Type,
                            "pvec_push(state, value) takes exactly 2 arguments".to_string(),
                            expr.span,
                        ));
                    }
                    let v_ty = self.check_expr(&args[1], env)?;
                    if &v_ty != elem.as_ref() {
                        return Err(Error::new(
                            ErrorKind::Type,
                            format!("pvec_push() value: expected {elem}, got {v_ty}"),
                            args[1].span,
                        ));
                    }
                    return Ok(Type::U64);
                }
                // pmap_contains takes a state-name as its first arg
                // so it can target a specific pmap slot — a non-state
                // argument is rejected here.
                if name == "pmap_contains" {
                    let state_name = match args.first().map(|a| &a.kind) {
                        Some(ExprKind::Ident(n)) => n.clone(),
                        _ => return Err(Error::new(
                            ErrorKind::Type,
                            format!("{name}() arg 0 must name a pmap state slot"),
                            expr.span,
                        )),
                    };
                    let st = self.states.get(&state_name).cloned().ok_or_else(|| Error::new(
                        ErrorKind::Type,
                        format!("'{state_name}' is not a state slot"),
                        expr.span,
                    ))?;
                    let Type::PMap { key: kt, .. } = st else {
                        return Err(Error::new(
                            ErrorKind::Type,
                            format!("{name}() expects a pmap state, got {st}"),
                            expr.span,
                        ));
                    };
                    if args.len() != 2 {
                        return Err(Error::new(
                            ErrorKind::Type,
                            "pmap_contains(state, key) takes exactly 2 arguments".to_string(),
                            expr.span,
                        ));
                    }
                    let k_ty = self.check_expr(&args[1], env)?;
                    if &k_ty != kt.as_ref() {
                        return Err(Error::new(
                            ErrorKind::Type,
                            format!("pmap_contains() key: expected {kt}, got {k_ty}"),
                            args[1].span,
                        ));
                    }
                    return Ok(Type::Bool);
                }
                // pbtree_contains(state, key) -> bool. Same shape
                // as pmap_contains but routes through the sorted-trie.
                if name == "pbtree_contains" {
                    let state_name = match args.first().map(|a| &a.kind) {
                        Some(ExprKind::Ident(n)) => n.clone(),
                        _ => return Err(Error::new(
                            ErrorKind::Type,
                            "pbtree_contains() arg 0 must name a pbtree state slot".to_string(),
                            expr.span,
                        )),
                    };
                    let st = self.states.get(&state_name).cloned().ok_or_else(|| Error::new(
                        ErrorKind::Type,
                        format!("'{state_name}' is not a state slot"),
                        expr.span,
                    ))?;
                    let Type::PBTree { key: kt, .. } = st else {
                        return Err(Error::new(
                            ErrorKind::Type,
                            format!("pbtree_contains() expects a pbtree state, got {st}"),
                            expr.span,
                        ));
                    };
                    if args.len() != 2 {
                        return Err(Error::new(
                            ErrorKind::Type,
                            "pbtree_contains(state, key) takes exactly 2 arguments".to_string(),
                            expr.span,
                        ));
                    }
                    let k_ty = self.check_expr(&args[1], env)?;
                    if &k_ty != kt.as_ref() {
                        return Err(Error::new(
                            ErrorKind::Type,
                            format!("pbtree_contains() key: expected {kt}, got {k_ty}"),
                            args[1].span,
                        ));
                    }
                    return Ok(Type::Bool);
                }
                // pbtree_range(state, lo, hi) -> [V]. Returns values
                // whose keys fall in [lo, hi] inclusive, in sorted order.
                if name == "pbtree_range" {
                    let state_name = match args.first().map(|a| &a.kind) {
                        Some(ExprKind::Ident(n)) => n.clone(),
                        _ => return Err(Error::new(
                            ErrorKind::Type,
                            "pbtree_range() arg 0 must name a pbtree state slot".to_string(),
                            expr.span,
                        )),
                    };
                    let st = self.states.get(&state_name).cloned().ok_or_else(|| Error::new(
                        ErrorKind::Type,
                        format!("'{state_name}' is not a state slot"),
                        expr.span,
                    ))?;
                    let Type::PBTree { key: kt, value: vt } = st else {
                        return Err(Error::new(
                            ErrorKind::Type,
                            format!("pbtree_range() expects a pbtree state, got {st}"),
                            expr.span,
                        ));
                    };
                    if args.len() != 3 {
                        return Err(Error::new(
                            ErrorKind::Type,
                            "pbtree_range(state, lo, hi) takes exactly 3 arguments".to_string(),
                            expr.span,
                        ));
                    }
                    let lo_ty = self.check_expr(&args[1], env)?;
                    let hi_ty = self.check_expr(&args[2], env)?;
                    if &lo_ty != kt.as_ref() || &hi_ty != kt.as_ref() {
                        return Err(Error::new(
                            ErrorKind::Type,
                            format!(
                                "pbtree_range() bounds must match key type {kt}; got lo={lo_ty}, hi={hi_ty}",
                            ),
                            expr.span,
                        ));
                    }
                    return Ok(Type::Array(vt));
                }
                // pmap_entries / pmap_keys / pmap_values: walk the
                // whole tree, return a fresh array. Each takes a
                // state-name first arg — a non-state argument is
                // rejected here so the compile pass has somewhere
                // to look up the state index.
                if matches!(name.as_str(), "pmap_entries" | "pmap_keys" | "pmap_values") {
                    let state_name = match args.first().map(|a| &a.kind) {
                        Some(ExprKind::Ident(n)) => n.clone(),
                        _ => return Err(Error::new(
                            ErrorKind::Type,
                            format!("{name}() arg 0 must name a pmap state slot"),
                            expr.span,
                        )),
                    };
                    let st = self.states.get(&state_name).cloned().ok_or_else(|| Error::new(
                        ErrorKind::Type,
                        format!("'{state_name}' is not a state slot"),
                        expr.span,
                    ))?;
                    let Type::PMap { key: kt, value: vt } = st else {
                        return Err(Error::new(
                            ErrorKind::Type,
                            format!("{name}() expects a pmap state, got {st}"),
                            expr.span,
                        ));
                    };
                    if args.len() != 1 {
                        return Err(Error::new(
                            ErrorKind::Type,
                            format!("{name}(state) takes exactly 1 argument"),
                            expr.span,
                        ));
                    }
                    let elem = match name.as_str() {
                        "pmap_entries" => Type::Tuple(vec![*kt, *vt]),
                        "pmap_keys"    => *kt,
                        "pmap_values"  => *vt,
                        _ => unreachable!(),
                    };
                    return Ok(Type::Array(Box::new(elem)));
                }
                // pvec_to_array(state) -> [T]. Same shape as the pmap
                // walks but for indexed-trie persistent vectors.
                if name == "pvec_to_array" {
                    let state_name = match args.first().map(|a| &a.kind) {
                        Some(ExprKind::Ident(n)) => n.clone(),
                        _ => return Err(Error::new(
                            ErrorKind::Type,
                            "pvec_to_array() arg 0 must name a pvec state slot".to_string(),
                            expr.span,
                        )),
                    };
                    let st = self.states.get(&state_name).cloned().ok_or_else(|| Error::new(
                        ErrorKind::Type,
                        format!("'{state_name}' is not a state slot"),
                        expr.span,
                    ))?;
                    let Type::PVec { elem } = st else {
                        return Err(Error::new(
                            ErrorKind::Type,
                            format!("pvec_to_array() expects a pvec state, got {st}"),
                            expr.span,
                        ));
                    };
                    if args.len() != 1 {
                        return Err(Error::new(
                            ErrorKind::Type,
                            "pvec_to_array(state) takes exactly 1 argument".to_string(),
                            expr.span,
                        ));
                    }
                    return Ok(Type::Array(elem));
                }
                let sig = self.sigs.get(name).ok_or_else(|| {
                    Error::new(
                        ErrorKind::Type,
                        format!("unknown function '{name}'"),
                        expr.span,
                    )
                })?;
                if self.in_main.get() {
                    if let Some(&is_entry) = self.module_fn_entry.get(name) {
                        if !is_entry {
                            return Err(Error::new(
                                ErrorKind::Type,
                                format!(
                                    "'main' may only call `entry` functions; '{name}' is module-private",
                                ),
                                expr.span,
                            ));
                        }
                    }
                }
                if sig.params.len() != args.len() {
                    return Err(Error::new(
                        ErrorKind::Type,
                        format!(
                            "function '{name}' expects {} arg(s), got {}",
                            sig.params.len(),
                            args.len()
                        ),
                        expr.span,
                    ));
                }
                for (i, (a, expected)) in args.iter().zip(sig.params.iter()).enumerate() {
                    let actual = self.check_expr(a, env)?;
                    if !types_compatible(&actual, expected) {
                        return Err(Error::new(
                            ErrorKind::Type,
                            format!(
                                "arg {i} of '{name}': expected {expected}, got {actual}",
                            ),
                            a.span,
                        ));
                    }
                }
                Ok(sig.ret.clone())
            }
            ExprKind::TupleLit(elems) => {
                let mut tys = Vec::with_capacity(elems.len());
                for e in elems {
                    tys.push(self.check_expr(e, env)?);
                }
                Ok(Type::Tuple(tys))
            }
            ExprKind::EnumCtor { enum_name, variant, args } => {
                let variants = self.enums.get(enum_name).cloned().ok_or_else(|| {
                    Error::new(
                        ErrorKind::Type,
                        format!("unknown enum '{enum_name}'"),
                        expr.span,
                    )
                })?;
                let payload = variants
                    .iter()
                    .find(|(n, _)| n == variant)
                    .map(|(_, p)| p.clone())
                    .ok_or_else(|| Error::new(
                        ErrorKind::Type,
                        format!("enum '{enum_name}' has no variant '{variant}'"),
                        expr.span,
                    ))?;
                if payload.len() != args.len() {
                    return Err(Error::new(
                        ErrorKind::Type,
                        format!(
                            "variant '{enum_name}::{variant}' takes {} payload(s), got {}",
                            payload.len(),
                            args.len(),
                        ),
                        expr.span,
                    ));
                }
                for (i, (a, expected)) in args.iter().zip(payload.iter()).enumerate() {
                    let actual = self.check_expr(a, env)?;
                    if !types_compatible(&actual, expected) {
                        return Err(Error::new(
                            ErrorKind::Type,
                            format!(
                                "variant '{enum_name}::{variant}' payload {i}: expected {expected}, got {actual}",
                            ),
                            a.span,
                        ));
                    }
                }
                Ok(Type::Enum { name: enum_name.clone(), variants })
            }
            ExprKind::Match { scrut, arms } => {
                let scrut_ty = self.check_expr(scrut, env)?;
                let (enum_name, variants) = match scrut_ty {
                    Type::Enum { ref name, ref variants } => (name.clone(), variants.clone()),
                    other => return Err(Error::new(
                        ErrorKind::Type,
                        format!("match scrutinee must be an enum, got {other}"),
                        scrut.span,
                    )),
                };
                if arms.is_empty() {
                    return Err(Error::new(
                        ErrorKind::Type,
                        "match needs at least one arm".to_string(),
                        expr.span,
                    ));
                }
                let mut covered: std::collections::HashSet<String> = std::collections::HashSet::new();
                let mut has_wildcard = false;
                let mut result_ty: Option<Type> = None;
                for arm in arms {
                    let mut arm_env = env.clone();
                    arm_env.push(HashMap::new());
                    match &arm.pattern {
                        MatchPattern::Wildcard => {
                            if has_wildcard {
                                return Err(Error::new(
                                    ErrorKind::Type,
                                    "duplicate wildcard arm".to_string(),
                                    arm.span,
                                ));
                            }
                            has_wildcard = true;
                        }
                        MatchPattern::EnumVariant { enum_name: en, variant, bindings } => {
                            if en != &enum_name {
                                return Err(Error::new(
                                    ErrorKind::Type,
                                    format!(
                                        "pattern's enum '{en}' doesn't match scrutinee enum '{enum_name}'",
                                    ),
                                    arm.span,
                                ));
                            }
                            let payload = variants
                                .iter()
                                .find(|(n, _)| n == variant)
                                .map(|(_, p)| p.clone())
                                .ok_or_else(|| Error::new(
                                    ErrorKind::Type,
                                    format!("enum '{enum_name}' has no variant '{variant}'"),
                                    arm.span,
                                ))?;
                            if bindings.len() != payload.len() {
                                return Err(Error::new(
                                    ErrorKind::Type,
                                    format!(
                                        "pattern '{enum_name}::{variant}' takes {} binding(s), got {}",
                                        payload.len(),
                                        bindings.len(),
                                    ),
                                    arm.span,
                                ));
                            }
                            if !covered.insert(variant.clone()) {
                                return Err(Error::new(
                                    ErrorKind::Type,
                                    format!("duplicate match arm for '{enum_name}::{variant}'"),
                                    arm.span,
                                ));
                            }
                            for (n, t) in bindings.iter().zip(payload.iter()) {
                                arm_env.last_mut().unwrap().insert(n.clone(), t.clone());
                            }
                        }
                    }
                    let body_ty = self.check_expr(&arm.body, &arm_env)?;
                    match &result_ty {
                        None => result_ty = Some(body_ty),
                        Some(expected) => {
                            if !types_compatible(&body_ty, expected) {
                                return Err(Error::new(
                                    ErrorKind::Type,
                                    format!(
                                        "match arms must agree: expected {expected}, got {body_ty}",
                                    ),
                                    arm.span,
                                ));
                            }
                        }
                    }
                }
                if !has_wildcard {
                    let missing: Vec<&String> = variants
                        .iter()
                        .map(|(n, _)| n)
                        .filter(|n| !covered.contains(*n))
                        .collect();
                    if !missing.is_empty() {
                        return Err(Error::new(
                            ErrorKind::Type,
                            format!(
                                "non-exhaustive match on '{enum_name}': missing {missing:?}",
                            ),
                            expr.span,
                        ));
                    }
                }
                Ok(result_ty.unwrap())
            }
            ExprKind::TupleIndex { target, index } => {
                let t = self.check_expr(target, env)?;
                let elems = match t {
                    Type::Tuple(elems) => elems,
                    other => return Err(Error::new(
                        ErrorKind::Type,
                        format!("tuple index on non-tuple type {other}"),
                        target.span,
                    )),
                };
                if *index >= elems.len() {
                    return Err(Error::new(
                        ErrorKind::Type,
                        format!("tuple index {index} out of bounds (len {})", elems.len()),
                        expr.span,
                    ));
                }
                Ok(elems[*index].clone())
            }
            ExprKind::Block(block) => {
                // Block as expression: type-check the body in a fresh
                // scope cloned from the caller's env (since check_expr
                // takes &Env, not &mut Env).
                self.check_block_as_expr(block, env)
            }
            ExprKind::If { cond, then, else_branch } => {
                let cond_ty = self.check_expr(cond, env)?;
                if cond_ty != Type::Bool {
                    return Err(Error::new(
                        ErrorKind::Type,
                        format!("if condition must be bool, got {cond_ty}"),
                        cond.span,
                    ));
                }
                let then_ty = self.check_block_as_expr(then, env)?;
                let else_ty = match else_branch {
                    ElseBranch::None => return Err(Error::new(
                        ErrorKind::Type,
                        "if used as expression must have an else branch".to_string(),
                        expr.span,
                    )),
                    ElseBranch::Block(b) => self.check_block_as_expr(b, env)?,
                    ElseBranch::If(inner) => {
                        let synthetic = Expr {
                            kind: ExprKind::If {
                                cond: Box::new(inner.cond.clone()),
                                then: inner.then.clone(),
                                else_branch: inner.else_branch.clone(),
                            },
                            span: inner.span,
                        };
                        self.check_expr(&synthetic, env)?
                    }
                };
                if then_ty != else_ty {
                    return Err(Error::new(
                        ErrorKind::Type,
                        format!(
                            "if-expression branches must agree: then={then_ty}, else={else_ty}",
                        ),
                        expr.span,
                    ));
                }
                Ok(then_ty)
            }
        }
    }

    /// Type a block as an expression, returning the tail's type
    /// (or Unit if none). Body statements are checked in a clone of
    /// the env so the caller's view is preserved.
    fn check_block_as_expr(&self, block: &Block, env: &Env) -> Result<Type, Error> {
        let mut local = env.clone();
        local.push(HashMap::new());
        for s in &block.stmts {
            self.check_stmt(s, &mut local, &Type::Unit)?;
        }
        match &block.tail {
            Some(t) => self.check_expr(t, &local),
            None => Ok(Type::Unit),
        }
    }
}

fn is_int(t: &Type) -> bool {
    matches!(t, Type::Int | Type::UInt | Type::Float | Type::I32 | Type::U32 | Type::U64 | Type::U128)
}

/// Returns true if `actual` can flow into a slot typed `expected`.
/// Mostly type equality, except Type::Json is polymorphic — any
/// primitive whose runtime form is a valid JSON leaf
/// (Bool/Int/UInt/Float/String) is assignable to a `json` slot, as
/// are JSON composites and `Type::Json` itself.
pub fn types_compatible(actual: &Type, expected: &Type) -> bool {
    if actual == expected { return true; }
    if expected == &Type::Json {
        return matches!(
            actual,
            Type::Bool | Type::Int | Type::UInt | Type::Float | Type::String | Type::Json,
        );
    }
    false
}

fn check_binop(op: BinOp, l: &Type, r: &Type, span: Span) -> Result<Type, Error> {
    let mismatch = || {
        Error::new(
            ErrorKind::Type,
            format!("cannot apply {op:?} to {l}, {r}"),
            span,
        )
    };
    match op {
        BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod => {
            if l == r && is_int(l) {
                Ok(l.clone())
            } else {
                Err(mismatch())
            }
        }
        BinOp::Lt | BinOp::Gt | BinOp::LtEq | BinOp::GtEq => {
            if l == r && is_int(l) {
                Ok(Type::Bool)
            } else {
                Err(mismatch())
            }
        }
        BinOp::Eq | BinOp::NotEq => {
            if l == r {
                Ok(Type::Bool)
            } else {
                Err(mismatch())
            }
        }
        BinOp::And | BinOp::Or => {
            if l == &Type::Bool && r == &Type::Bool {
                Ok(Type::Bool)
            } else {
                Err(mismatch())
            }
        }
        BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor
        | BinOp::Shl | BinOp::Shr => {
            // Bitwise ops are integer-only; both sides must agree
            // on type. (No automatic widening — explicit conversion
            // builtins exist for that.)
            if l == r && is_int(l) {
                Ok(l.clone())
            } else {
                Err(mismatch())
            }
        }
    }
}

fn check_unop(op: UnOp, v: &Type, span: Span) -> Result<Type, Error> {
    match op {
        UnOp::Neg if is_int(v) => Ok(v.clone()),
        UnOp::Not if v == &Type::Bool => Ok(Type::Bool),
        _ => Err(Error::new(
            ErrorKind::Type,
            format!("cannot apply {op:?} to {v}"),
            span,
        )),
    }
}
