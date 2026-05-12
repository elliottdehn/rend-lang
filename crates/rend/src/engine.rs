//! Embedding API.

use std::collections::HashMap;

use crate::bc::BcModule;
use crate::compile;
use crate::error::{Error, ErrorKind};
use crate::frontend;
use crate::host::{Host, HostError};
use crate::kv::{EmptyKv, Kv};
use crate::token::Span;
use crate::tx::Tx;
use crate::typeck;
use crate::value::Value;
use crate::vm;

#[derive(Default)]
pub struct Engine {
    host: Host,
}

/// What an [`Engine::execute`] (or [`Engine::execute_main`]) call hands back
/// to the host. Keys are u128 — derived from the runtime's hashing layer —
/// and values are typed Value variants. The host validates `reads` against
/// its current snapshot and applies `writes`.
#[derive(Debug)]
pub struct ExecOutcome {
    pub result: Value,
    pub reads: HashMap<u128, Value>,
    pub writes: HashMap<u128, Value>,
    /// Append-only log of `emit` records produced during the tx, in
    /// emission order. Each carries the module name that issued it,
    /// the event's declared name, and the resolved arg values.
    pub events: Vec<crate::tx::EmittedEvent>,
    /// For each pmap state cell the tx touched, the declared K/V
    /// types. The OCC merge path uses this to walk HAMT nodes when
    /// resolving root-pointer conflicts; without it, merge would
    /// have to consult the module schema separately.
    pub pmap_types: HashMap<u128, (crate::ast::Type, crate::ast::Type)>,
    /// Same shape, but for `pvec<T>` cells — element type only.
    pub pvec_types: HashMap<u128, crate::ast::Type>,
    /// Cell keys the tx wrote as pmap/pvec node bytes — content-
    /// addressed. GC uses this as its sweep candidate set: any cell
    /// here that's no longer reachable from a live state root after
    /// commit can be reclaimed.
    pub node_cells_written: std::collections::HashSet<u128>,
}

/// Result of a read-only `Engine::query` call. Mirrors `ExecOutcome`
/// minus the write set — queries never produce writes, so omitting
/// the field makes that contract explicit at the API level.
#[derive(Debug)]
pub struct QueryOutcome {
    pub result: Value,
    pub reads: HashMap<u128, Value>,
    pub events: Vec<crate::tx::EmittedEvent>,
}

impl Engine {
    pub fn new() -> Self {
        Self { host: Host::new() }
    }

    pub fn bind<F>(&mut self, name: impl Into<String>, f: F) -> &mut Self
    where
        F: Fn(&[Value]) -> Result<Value, HostError> + Send + Sync + 'static,
    {
        self.host.bind(name, f);
        self
    }

    pub fn host(&self) -> &Host {
        &self.host
    }

    /// Run `main()` in a single-module program against an empty KV.
    pub fn run(&self, src: &str, fuel: vm::Fuel) -> Result<Value, Error> {
        let kv = EmptyKv;
        Ok(self.execute(src, fuel, &kv)?.result)
    }

    /// Compile, link, and run `main()` for a single-module program.
    /// Defaults to a zeroed `TxContext`; for real txs the host should
    /// call [`Engine::execute_with_context`] with the authenticated
    /// sender plus chain-context numbers.
    pub fn execute(
        &self,
        src: &str,
        fuel: vm::Fuel,
        kv: &dyn Kv,
    ) -> Result<ExecOutcome, Error> {
        self.execute_with_context(src, crate::tx::TxContext::default(), fuel, kv)
    }

    /// Like `execute` but with a host-provided `TxContext` (sender,
    /// block timestamp, block number). The values are accessible
    /// inside the program via `msg_sender()` / `block_timestamp()` /
    /// `block_number()`.
    pub fn execute_with_context(
        &self,
        src: &str,
        ctx: crate::tx::TxContext,
        fuel: vm::Fuel,
        kv: &dyn Kv,
    ) -> Result<ExecOutcome, Error> {
        let module = frontend(src)?;
        for imp in &module.imports {
            if !self.host.has(&imp.name) {
                return Err(Error::new(
                    ErrorKind::Type,
                    format!("import '{}' has no host impl bound", imp.name),
                    imp.span,
                ));
            }
        }
        // Honor `module <name>;` if declared; default to "main" for
        // ad-hoc single-module sources that didn't bother to name
        // themselves. The chosen name namespaces state cells and
        // event-log entries.
        let module_name = module.name.clone().unwrap_or_else(|| "main".to_string());
        let mut bc_module = compile::compile_named(&module, &module_name)?;
        // Cluster independent state reads into batched `get_many` calls.
        // The classifier tells us which same-module callees are
        // read-safe to span (Pure / ReadOnly).
        let effects = crate::effects::classify(&module);
        crate::optimize::optimize(&mut bc_module, &effects);
        let mut tx = Tx::new_with_context(kv, ctx);
        let modules = [bc_module];
        let result = vm::run_world(&modules, 0, "main", &[], fuel, &self.host, &mut tx)?;
        let (reads, writes, events, pmap_types, pvec_types, node_cells_written) = tx.into_full();
        Ok(ExecOutcome { result, reads, writes, events, pmap_types, pvec_types, node_cells_written })
    }

    /// Multi-module run from self-describing sources. Each source must
    /// declare `module <name>;` at the top; the engine groups them by
    /// declared name. The host picks `main_module` to invoke. Duplicate
    /// declared names are an error.
    pub fn execute_modules(
        &self,
        sources: &[String],
        main_module: &str,
        fuel: vm::Fuel,
        kv: &dyn Kv,
    ) -> Result<ExecOutcome, Error> {
        let mut by_name: HashMap<String, String> = HashMap::with_capacity(sources.len());
        for src in sources {
            let tokens = crate::lexer::tokenize(src)?;
            let m = crate::parser::parse(tokens)?;
            let name = m.name.ok_or_else(|| {
                Error::new(
                    ErrorKind::Parse,
                    "execute_modules: source is missing a `module <name>;` declaration",
                    Span::default(),
                )
            })?;
            if by_name.insert(name.clone(), src.clone()).is_some() {
                return Err(Error::new(
                    ErrorKind::Type,
                    format!("duplicate module name '{name}' across sources"),
                    Span::default(),
                ));
            }
        }
        self.execute_main(&by_name, main_module, fuel, kv)
    }

    /// Multi-module run, sources keyed by module name. If a source contains
    /// `module <name>;` at the top, the declared name must match the map
    /// key. Prefer [`Engine::execute_modules`] when sources self-declare.
    pub fn execute_main(
        &self,
        sources: &HashMap<String, String>,
        main_module: &str,
        fuel: vm::Fuel,
        kv: &dyn Kv,
    ) -> Result<ExecOutcome, Error> {
        self.execute_main_with_context(
            sources,
            main_module,
            crate::tx::TxContext::default(),
            fuel,
            kv,
        )
    }

    pub fn execute_main_with_context(
        &self,
        sources: &HashMap<String, String>,
        main_module: &str,
        ctx: crate::tx::TxContext,
        fuel: vm::Fuel,
        kv: &dyn Kv,
    ) -> Result<ExecOutcome, Error> {
        // Phase 1a: parse + expand modifiers per module. Resolution
        // is deferred so we can build a cross-module struct catalog
        // first — that lets `m::T` references in any module find
        // their `pub` foreign struct decl.
        let mut parsed: Vec<(String, crate::ast::Module)> =
            Vec::with_capacity(sources.len());
        for (name, src) in sources {
            let tokens = crate::lexer::tokenize(src)?;
            let mut m = crate::parser::parse(tokens)?;
            if let Some(declared) = &m.name {
                if declared != name {
                    return Err(Error::new(
                        ErrorKind::Type,
                        format!(
                            "module declared as '{declared}' but loaded as '{name}'",
                        ),
                        Span::default(),
                    ));
                }
            }
            crate::modifier::expand(&mut m)?;
            parsed.push((name.clone(), m));
        }
        // Phase 1b: catalog every module's struct decls (fields
        // intentionally taken pre-resolution — they may still hold
        // unresolved local-struct references, which is fine because
        // each module re-runs the full resolve pass below).
        let mut cross_structs: crate::typeck::CrossModuleStructs =
            std::collections::HashMap::new();
        for (mod_name, m) in &parsed {
            for s in &m.structs {
                let fields: Vec<(String, crate::ast::Type)> = s
                    .fields
                    .iter()
                    .map(|f| (f.name.clone(), f.ty.clone()))
                    .collect();
                let groups: Vec<Option<String>> =
                    s.fields.iter().map(|f| f.group.clone()).collect();
                cross_structs.insert(
                    format!("{mod_name}::{}", s.name),
                    (fields, groups, s.is_pub),
                );
            }
        }
        // Phase 1c: per-module type resolution with cross-module
        // visibility. `m::T` lookups go through `cross_structs`
        // (which has already filtered out non-`pub` entries via the
        // resolver itself).
        for (_, m) in parsed.iter_mut() {
            crate::typeck::resolve_types_with_externals(
                m,
                &cross_structs,
                &std::collections::HashMap::new(),
            )?;
        }

        // Phase 2: collect cross-module entry signatures into one manifest.
        let mut externals: HashMap<String, crate::typeck::ModuleSig> = HashMap::new();
        for (mod_name, m) in &parsed {
            let manifest = typeck::collect_manifest(m);
            for (fn_name, sig) in manifest.entry_sigs {
                externals.insert(format!("{mod_name}::{fn_name}"), sig);
            }
        }
        // Cross-module effect manifest from declarations (mirrors
        // `compile_module_set`'s logic). Lets a `view fn` in one
        // module call a `view` entry in another without falsely
        // computing as Impure.
        let external_effects = collect_decl_effects(&parsed);

        // Phase 3: typeck each module against the manifest, affine-check,
        // compile, and run the read-clustering optimizer using each
        // module's own effect classification.
        let mut compiled: Vec<BcModule> = Vec::with_capacity(parsed.len());
        let mut module_index: HashMap<String, usize> = HashMap::new();
        // Annotate each module's DynCall nodes with view/pure
        // flags before typeck/compile. Locally-bound interface
        // dispatches need this to participate in read-cluster
        // optimization; state-bound calls work either way but
        // we keep one code path.
        for (_, m) in parsed.iter_mut() {
            typeck::annotate_dyn_calls(m);
        }
        // Cross-module callee flags map (module, fn) → (is_view,
        // is_pure). Stamped onto every emitted CallExternal so the
        // optimizer doesn't fence read clusters around `view`/`pure`
        // cross-module calls.
        let extern_fn_flags = collect_decl_fn_flags(&parsed);
        for (idx, (mod_name, m)) in parsed.iter().enumerate() {
            typeck::check_with_externals(m, externals.clone())?;
            crate::affine::check(m)?;
            for imp in &m.imports {
                if !self.host.has(&imp.name) {
                    return Err(Error::new(
                        ErrorKind::Type,
                        format!("module '{mod_name}': import '{}' has no host impl bound", imp.name),
                        imp.span,
                    ));
                }
            }
            let mut bc = compile::compile_named_with_extras(
                m, mod_name,
                &std::collections::HashSet::new(),
                &extern_fn_flags,
            )?;
            let effects = crate::effects::classify_with_externals(m, &external_effects);
            crate::effects::verify_purity_annotations(m, &effects)?;
            crate::optimize::optimize(&mut bc, &effects);
            compiled.push(bc);
            module_index.insert(mod_name.clone(), idx);
        }

        // Phase 4: validate cross-module references resolve to entry fns.
        for m in &compiled {
            for f in &m.functions {
                for instr in &f.code {
                    if let crate::bc::Instr::CallExternal {
                        module_name_idx,
                        fn_name_idx,
                        ..
                    } = instr
                    {
                        let mn = const_str(&f.consts[*module_name_idx as usize])?;
                        let fnn = const_str(&f.consts[*fn_name_idx as usize])?;
                        let target_idx = module_index.get(&mn).ok_or_else(|| {
                            Error::new(
                                ErrorKind::Type,
                                format!("module '{mn}' is not loaded"),
                                Span::default(),
                            )
                        })?;
                        let target = &compiled[*target_idx];
                        let target_fn = target.fn_index.get(&fnn).ok_or_else(|| {
                            Error::new(
                                ErrorKind::Type,
                                format!("'{mn}::{fnn}' is not defined"),
                                Span::default(),
                            )
                        })?;
                        if !target.functions[*target_fn].is_entry {
                            return Err(Error::new(
                                ErrorKind::Type,
                                format!("'{mn}::{fnn}' is not an entry function"),
                                Span::default(),
                            ));
                        }
                    }
                }
            }
        }

        // Phase 5: run.
        let main_idx = *module_index.get(main_module).ok_or_else(|| {
            Error::new(
                ErrorKind::Type,
                format!("main module '{main_module}' is not loaded"),
                Span::default(),
            )
        })?;
        let mut tx = Tx::new_with_context(kv, ctx);
        let result = vm::run_world(&compiled, main_idx, "main", &[], fuel, &self.host, &mut tx)?;
        let (reads, writes, events, pmap_types, pvec_types, node_cells_written) = tx.into_full();
        Ok(ExecOutcome { result, reads, writes, events, pmap_types, pvec_types, node_cells_written })
    }

    // ---------- artifact API ------------------------------------
    //
    // The host's path for "compile once, run many times". A
    // `crate::artifact::Artifact` is a serialized bundle of
    // `BcModule`s with a stable content hash; `compile` produces
    // one, `execute_artifact` runs it without re-doing the
    // lex/parse/typeck/affine/compile pipeline.

    /// Compile a single source into an artifact. The source may
    /// declare `module <name>;`; if it doesn't, the module is
    /// named `"main"`.
    pub fn compile(&self, src: &str) -> Result<crate::artifact::Artifact, Error> {
        let bc = self.compile_single(src)?;
        Ok(crate::artifact::Artifact::from_modules(vec![bc]))
    }

    /// Compile multiple self-describing sources into one artifact.
    /// Each source must declare `module <name>;`. Cross-module
    /// imports between modules in the same artifact are baked in
    /// at compile time.
    pub fn compile_modules(
        &self,
        sources: &[String],
    ) -> Result<crate::artifact::Artifact, Error> {
        let mut by_name: HashMap<String, String> = HashMap::with_capacity(sources.len());
        for src in sources {
            let tokens = crate::lexer::tokenize(src)?;
            let m = crate::parser::parse(tokens)?;
            let name = m.name.ok_or_else(|| {
                Error::new(
                    ErrorKind::Parse,
                    "compile_modules: source is missing a `module <name>;` declaration",
                    Span::default(),
                )
            })?;
            if by_name.insert(name.clone(), src.clone()).is_some() {
                return Err(Error::new(
                    ErrorKind::Type,
                    format!("duplicate module name '{name}' across sources"),
                    Span::default(),
                ));
            }
        }
        let modules = self.compile_module_set(&by_name)?;
        Ok(crate::artifact::Artifact::from_modules(modules))
    }

    /// Run a precompiled artifact. Re-validates host imports and
    /// cross-module calls before dispatch; either failure surfaces
    /// as a typed error rather than running the artifact.
    pub fn execute_artifact(
        &self,
        artifact: &crate::artifact::Artifact,
        main_module: &str,
        fuel: vm::Fuel,
        kv: &dyn Kv,
    ) -> Result<ExecOutcome, Error> {
        self.execute_artifact_with_context(
            artifact,
            main_module,
            crate::tx::TxContext::default(),
            fuel,
            kv,
        )
    }

    pub fn execute_artifact_with_context(
        &self,
        artifact: &crate::artifact::Artifact,
        main_module: &str,
        ctx: crate::tx::TxContext,
        fuel: vm::Fuel,
        kv: &dyn Kv,
    ) -> Result<ExecOutcome, Error> {
        let modules: &[BcModule] = &artifact.modules;
        // Re-validate host imports — the artifact may have been
        // compiled against a host with a different bind set.
        for m in modules {
            for imp in &m.imports {
                if !self.host.has(imp) {
                    return Err(Error::new(
                        ErrorKind::Type,
                        format!("module '{}': host import '{imp}' has no impl bound", m.name),
                        Span::default(),
                    ));
                }
            }
        }
        // Re-validate cross-module references — they're string-
        // keyed in the bytecode, and might point at modules the
        // current artifact doesn't ship.
        let mut module_index: HashMap<String, usize> = HashMap::with_capacity(modules.len());
        for (i, m) in modules.iter().enumerate() {
            module_index.insert(m.name.clone(), i);
        }
        validate_cross_module_calls(modules, &module_index)?;

        let main_idx = *module_index.get(main_module).ok_or_else(|| {
            Error::new(
                ErrorKind::Type,
                format!("main module '{main_module}' is not present in the artifact"),
                Span::default(),
            )
        })?;
        let mut tx = Tx::new_with_context(kv, ctx);
        let result = vm::run_world(modules, main_idx, "main", &[], fuel, &self.host, &mut tx)?;
        let (reads, writes, events, pmap_types, pvec_types, node_cells_written) = tx.into_full();
        Ok(ExecOutcome { result, reads, writes, events, pmap_types, pvec_types, node_cells_written })
    }

    /// Run a deployed program's `main` once, if it has one. This
    /// is the constructor pattern: the program's `main` (if
    /// present) sets up initial state — populates owners, seeds
    /// supply, validates parameters — and any writes flow into
    /// `kv` as the program's starting state.
    ///
    /// Programs without a `main` are still valid; deploy is then
    /// a no-op and returns `Ok(None)`. Hosts that distinguish
    /// deployment from runtime can rely on the runtime to never
    /// auto-invoke `main` later: subsequent calls go through
    /// `execute_tx`, which dispatches into entry fns by name and
    /// never visits a deployed module's `main`.
    pub fn deploy(
        &self,
        artifact: &crate::artifact::Artifact,
        fuel: vm::Fuel,
        kv: &dyn Kv,
    ) -> Result<Option<ExecOutcome>, Error> {
        self.deploy_with_context(artifact, crate::tx::TxContext::default(), fuel, kv)
    }

    pub fn deploy_with_context(
        &self,
        artifact: &crate::artifact::Artifact,
        ctx: crate::tx::TxContext,
        fuel: vm::Fuel,
        kv: &dyn Kv,
    ) -> Result<Option<ExecOutcome>, Error> {
        let Some(main_module) = artifact.main_module()? else {
            return Ok(None);
        };
        let main_module = main_module.to_string();
        let outcome = self.execute_artifact_with_context(
            artifact, &main_module, ctx, fuel, kv,
        )?;
        Ok(Some(outcome))
    }

    // ---------- multi-artifact: tx + deployed deps --------------
    //
    // The "deployed program" / "tx" split. A deployed program is
    // an artifact persisted by the host (carries state shapes +
    // entry fns). A tx is itself an artifact, freshly compiled
    // per request, that calls into deployed-program entries via
    // `module::fn(...)` syntax.

    /// Compile `src` as a tx, typechecking `module::fn(...)` calls
    /// against the entry signatures of `deps`. The resulting
    /// artifact carries only the tx's own bytecode; deployed
    /// modules stay in their own artifacts.
    pub fn compile_tx(
        &self,
        src: &str,
        deps: &[crate::artifact::Artifact],
    ) -> Result<crate::artifact::Artifact, Error> {
        let externals = collect_externals(deps);
        // Surface deps' interface declarations: the parser needs
        // their names so `IFace` parses as a type, and typeck
        // needs the methods so `IFace::bind(...)` resolves.
        let dep_iface_names: std::collections::HashSet<String> = deps
            .iter()
            .flat_map(|a| a.modules.iter())
            .flat_map(|m| m.interfaces.iter().map(|d| d.name.clone()))
            .collect();
        let dep_iface_methods: HashMap<String, Vec<crate::ast::InterfaceMethodSig>> = deps
            .iter()
            .flat_map(|a| a.modules.iter())
            .flat_map(|m| m.interfaces.iter())
            .map(|d| (
                d.name.clone(),
                d.methods.iter().map(|m| crate::ast::InterfaceMethodSig {
                    name: m.name.clone(),
                    params: m.params.iter().map(|p| p.ty.clone()).collect(),
                    return_type: m.return_type.clone(),
                    is_view: m.is_view,
                    is_pure: m.is_pure,
                }).collect(),
            ))
            .collect();
        // Surface deps' `pub` struct decls so the tx source can
        // reference them via `m::T`. Private structs in the dep
        // aren't published — the resolver's visibility check is
        // simply "absence from the cross-module catalog."
        let dep_cross_structs: crate::typeck::CrossModuleStructs = deps
            .iter()
            .flat_map(|a| a.modules.iter())
            .flat_map(|m| m.struct_shapes.iter().map(move |s| (m.name.clone(), s)))
            .filter(|(_, s)| s.is_pub)
            .map(|(mod_name, s)| {
                let fields: Vec<(String, crate::ast::Type)> = s
                    .field_names
                    .iter()
                    .zip(s.field_types.iter())
                    .map(|(n, t)| (n.clone(), t.clone()))
                    .collect();
                (
                    format!("{mod_name}::{}", s.name),
                    (fields, s.field_groups.clone(), s.is_pub),
                )
            })
            .collect();
        // Parse + resolve types.
        let mut module = {
            let tokens = crate::lexer::tokenize(src)?;
            let mut m = crate::parser::Parser::with_extra_iface_names(tokens, dep_iface_names.clone())
                .parse_module()?;
            crate::modifier::expand(&mut m)?;
            typeck::resolve_types_with_externals(
                &mut m,
                &dep_cross_structs,
                &dep_iface_methods,
            )?;
            m
        };
        // A tx is meaningless without a `main` — that's the
        // orchestration entry point. Surface this at compile time
        // so callers don't ship un-runnable tx artifacts.
        if !module.functions.iter().any(|f| f.name == "main") {
            return Err(Error::new(
                ErrorKind::Type,
                "compile_tx: tx source must define a `main` fn",
                Span::default(),
            ));
        }
        // Reject any module-name collision with deps.
        let module_name = module.name.clone().unwrap_or_else(|| "main".to_string());
        if let Some(_dep) = deps.iter().flat_map(|a| &a.modules).find(|m| m.name == module_name) {
            return Err(Error::new(
                ErrorKind::Type,
                format!(
                    "tx module name '{module_name}' collides with a deployed dep — \
                     pick a different module name in the tx source",
                ),
                Span::default(),
            ));
        }
        module.name = Some(module_name.clone());
        // Typecheck against the deps' entry signatures.
        typeck::check_with_externals_and_ifaces(&module, externals, dep_iface_methods.clone())?;
        crate::affine::check(&module)?;
        typeck::annotate_dyn_calls_with_extras(&mut module, &dep_iface_methods);
        for imp in &module.imports {
            if !self.host.has(&imp.name) {
                return Err(Error::new(
                    ErrorKind::Type,
                    format!("import '{}' has no host impl bound", imp.name),
                    imp.span,
                ));
            }
        }
        // Cross-module callee flags from dep artifacts so any
        // `dep_module::fn(...)` call in this tx gets stamped with
        // the callee's view/pure bound at emission time.
        let extern_fn_flags = collect_dep_fn_flags(deps);
        let mut bc = compile::compile_named_with_extras(
            &module, &module_name, &dep_iface_names, &extern_fn_flags,
        )?;
        // Cross-module effect manifest from deps' view/pure
        // annotations — lets `view fn main() { ... bank::view_fn() }`
        // verify cleanly when bank::view_fn is itself annotated.
        let external_effects = collect_external_effects(deps);
        let effects = crate::effects::classify_with_externals_and_iface_names(
            &module, &external_effects, &dep_iface_names,
        );
        crate::effects::verify_purity_annotations(&module, &effects)?;
        crate::optimize::optimize(&mut bc, &effects);

        // Validate that every `module::fn(...)` reference in the
        // tx resolves to an entry in either the tx itself or one
        // of the deps. The typeck pass above is permissive when
        // `externals` is missing a target — that's the desired
        // behavior in the single-module case, but here we want
        // hard failure.
        let mut all = vec![bc];
        let mut module_index: HashMap<String, usize> = HashMap::new();
        module_index.insert(all[0].name.clone(), 0);
        for dep in deps {
            for m in &dep.modules {
                if !module_index.contains_key(&m.name) {
                    module_index.insert(m.name.clone(), all.len());
                    all.push(m.clone());
                }
            }
        }
        validate_cross_module_calls(&all, &module_index)?;
        // Pull the tx's compiled module back out — we don't ship
        // the deps inside the tx artifact.
        let tx_bc = all.remove(0);
        Ok(crate::artifact::Artifact::from_modules(vec![tx_bc]))
    }

    /// Run a tx artifact alongside its deployed deps. The tx's
    /// `main` fn is the dispatch entry — `compile_tx` ensures
    /// every tx artifact has exactly one. The tx's
    /// `module::fn(...)` calls into the deps thread state through
    /// each dep's own state cells.
    pub fn execute_tx(
        &self,
        tx: &crate::artifact::Artifact,
        deps: &[crate::artifact::Artifact],
        fuel: vm::Fuel,
        kv: &dyn Kv,
    ) -> Result<ExecOutcome, Error> {
        self.execute_tx_with_context(
            tx, deps, crate::tx::TxContext::default(), fuel, kv,
        )
    }

    pub fn execute_tx_with_context(
        &self,
        tx: &crate::artifact::Artifact,
        deps: &[crate::artifact::Artifact],
        ctx: crate::tx::TxContext,
        fuel: vm::Fuel,
        kv: &dyn Kv,
    ) -> Result<ExecOutcome, Error> {
        // The tx's `main` is the entry point. compile_tx already
        // enforced that exactly one module has one; surface any
        // mismatch here as a runtime error so misuse from outside
        // the compile path is caught.
        let main_module = tx.main_module()?.ok_or_else(|| Error::new(
            ErrorKind::Type,
            "execute_tx: tx artifact has no `main` fn",
            Span::default(),
        ))?.to_string();
        // Build the unified module set: tx modules first, then
        // each dep's modules. Reject any name collision — state
        // cells namespace by module name, so dupes would silently
        // alias storage.
        let mut all: Vec<BcModule> = Vec::new();
        let mut module_index: HashMap<String, usize> = HashMap::new();
        for m in tx.modules.iter().chain(deps.iter().flat_map(|a| a.modules.iter())) {
            if module_index.contains_key(&m.name) {
                return Err(Error::new(
                    ErrorKind::Type,
                    format!("duplicate module '{}' across tx and deps", m.name),
                    Span::default(),
                ));
            }
            module_index.insert(m.name.clone(), all.len());
            all.push(m.clone());
        }
        // Re-validate host imports + cross-module calls.
        for m in &all {
            for imp in &m.imports {
                if !self.host.has(imp) {
                    return Err(Error::new(
                        ErrorKind::Type,
                        format!("module '{}': host import '{imp}' has no impl bound", m.name),
                        Span::default(),
                    ));
                }
            }
        }
        validate_cross_module_calls(&all, &module_index)?;

        let main_idx = *module_index.get(main_module.as_str()).ok_or_else(|| {
            Error::new(
                ErrorKind::Type,
                format!("main module '{main_module}' is not present in tx or deps"),
                Span::default(),
            )
        })?;
        let mut tx_state = Tx::new_with_context(kv, ctx);
        let result = vm::run_world(&all, main_idx, "main", &[], fuel, &self.host, &mut tx_state)?;
        let (reads, writes, events, pmap_types, pvec_types, node_cells_written) = tx_state.into_full();
        Ok(ExecOutcome { result, reads, writes, events, pmap_types, pvec_types, node_cells_written })
    }

    /// Read-only execution path. Verifies the tx artifact's `main`
    /// is declared `view` or `pure`, runs it against the dep
    /// artifacts, and asserts at the end that the write set is
    /// empty. Suitable for "query" workloads where the host wants
    /// to compute a value from current state without altering it
    /// (e.g., balance lookups, derived dashboards). Many concurrent
    /// queries can run against the same KV without OCC fencing —
    /// they don't write, so they don't conflict with each other or
    /// with concurrent writers (modulo snapshot consistency, which
    /// is the host's concern).
    pub fn query(
        &self,
        tx: &crate::artifact::Artifact,
        deps: &[crate::artifact::Artifact],
        fuel: vm::Fuel,
        kv: &dyn Kv,
    ) -> Result<QueryOutcome, Error> {
        self.query_with_context(
            tx, deps, crate::tx::TxContext::default(), fuel, kv,
        )
    }

    pub fn query_with_context(
        &self,
        tx: &crate::artifact::Artifact,
        deps: &[crate::artifact::Artifact],
        ctx: crate::tx::TxContext,
        fuel: vm::Fuel,
        kv: &dyn Kv,
    ) -> Result<QueryOutcome, Error> {
        // Static check: the tx's main must be declared view or pure.
        // Without this, a tx that happens to do no writes "by luck"
        // would slip through. The annotation makes the contract
        // explicit at the source level.
        let main_module = tx.main_module()?.ok_or_else(|| Error::new(
            ErrorKind::Type,
            "query: tx artifact has no `main` fn",
            Span::default(),
        ))?;
        let tx_module = tx.modules.iter()
            .find(|m| m.name == main_module)
            .expect("main_module came from tx.modules");
        let main_fn = tx_module.functions.iter()
            .find(|f| f.name == "main")
            .expect("main_module advertises a main fn");
        if !(main_fn.is_view || main_fn.is_pure) {
            return Err(Error::new(
                ErrorKind::Type,
                "query: tx `main` must be declared `view` or `pure` to run on the read-only path",
                Span::default(),
            ));
        }
        // Run the tx like a regular execute_tx, then defense-in-depth
        // assert no writes leaked. Compile-time view/pure verification
        // already guarantees this for the tx's own code; the runtime
        // check covers cross-artifact misuse (e.g., the host swapped
        // a dep for one whose `view` annotations don't match its
        // body — possible if dep artifacts came from different
        // compiler versions).
        let outcome = self.execute_tx_with_context(tx, deps, ctx, fuel, kv)?;
        if !outcome.writes.is_empty() {
            return Err(Error::new(
                ErrorKind::Runtime,
                format!(
                    "query: tx wrote {} cell(s) — `view`/`pure` annotation was violated by a callee",
                    outcome.writes.len(),
                ),
                Span::default(),
            ));
        }
        Ok(QueryOutcome {
            result: outcome.result,
            reads: outcome.reads,
            events: outcome.events,
        })
    }

    // ---------- internal helpers --------------------------------

    fn compile_single(&self, src: &str) -> Result<BcModule, Error> {
        let module = frontend(src)?;
        for imp in &module.imports {
            if !self.host.has(&imp.name) {
                return Err(Error::new(
                    ErrorKind::Type,
                    format!("import '{}' has no host impl bound", imp.name),
                    imp.span,
                ));
            }
        }
        let module_name = module.name.clone().unwrap_or_else(|| "main".to_string());
        let mut bc = compile::compile_named(&module, &module_name)?;
        let effects = crate::effects::classify(&module);
        crate::optimize::optimize(&mut bc, &effects);
        Ok(bc)
    }

    /// Compile a name → source map into a vector of `BcModule`s.
    /// Mirrors `execute_main_with_context` phases 1–4 without the
    /// Tx setup or `vm::run_world` call.
    fn compile_module_set(
        &self,
        sources: &HashMap<String, String>,
    ) -> Result<Vec<BcModule>, Error> {
        let mut parsed: Vec<(String, crate::ast::Module)> =
            Vec::with_capacity(sources.len());
        for (name, src) in sources {
            let tokens = crate::lexer::tokenize(src)?;
            let mut m = crate::parser::parse(tokens)?;
            if let Some(declared) = &m.name {
                if declared != name {
                    return Err(Error::new(
                        ErrorKind::Type,
                        format!("module declared as '{declared}' but loaded as '{name}'"),
                        Span::default(),
                    ));
                }
            }
            crate::modifier::expand(&mut m)?;
            typeck::resolve_types(&mut m)?;
            parsed.push((name.clone(), m));
        }
        let mut externals: HashMap<String, crate::typeck::ModuleSig> = HashMap::new();
        for (mod_name, m) in &parsed {
            let manifest = typeck::collect_manifest(m);
            for (fn_name, sig) in manifest.entry_sigs {
                externals.insert(format!("{mod_name}::{fn_name}"), sig);
            }
        }
        // Build a cross-module effect manifest from declarations:
        // a fn marked `pure` advertises Pure, `view` advertises
        // ReadOnly, anything else stays Impure (conservative
        // default). Lets a `view fn` in module B that calls
        // `A::view_fn` verify cleanly. We re-verify each module's
        // own declarations against a *local* classification below,
        // so the manifest can't lie its way past verification —
        // a fn declared `view` that actually mutates state still
        // fails its own verify pass.
        let external_effects = collect_decl_effects(&parsed);
        let extern_fn_flags = collect_decl_fn_flags(&parsed);
        let mut compiled: Vec<BcModule> = Vec::with_capacity(parsed.len());
        let mut module_index: HashMap<String, usize> = HashMap::new();
        for (_, m) in parsed.iter_mut() {
            typeck::annotate_dyn_calls(m);
        }
        for (idx, (mod_name, m)) in parsed.iter().enumerate() {
            typeck::check_with_externals(m, externals.clone())?;
            crate::affine::check(m)?;
            for imp in &m.imports {
                if !self.host.has(&imp.name) {
                    return Err(Error::new(
                        ErrorKind::Type,
                        format!("module '{mod_name}': import '{}' has no host impl bound", imp.name),
                        imp.span,
                    ));
                }
            }
            let mut bc = compile::compile_named_with_extras(
                m, mod_name,
                &std::collections::HashSet::new(),
                &extern_fn_flags,
            )?;
            let effects = crate::effects::classify_with_externals(m, &external_effects);
            crate::effects::verify_purity_annotations(m, &effects)?;
            crate::optimize::optimize(&mut bc, &effects);
            compiled.push(bc);
            module_index.insert(mod_name.clone(), idx);
        }
        validate_cross_module_calls(&compiled, &module_index)?;
        Ok(compiled)
    }
}

/// Build a cross-module effects manifest from each parsed
/// module's `view` / `pure` declarations. Used by the multi-
/// module compile so a `view fn` in module B that calls
/// `A::view_fn` doesn't fall back to the default-Impure
/// treatment.
fn collect_decl_effects(
    parsed: &[(String, crate::ast::Module)],
) -> HashMap<String, crate::effects::EffectClass> {
    let mut out: HashMap<String, crate::effects::EffectClass> = HashMap::new();
    for (mod_name, m) in parsed {
        for f in &m.functions {
            if !f.is_entry { continue; }
            let class = if f.is_pure {
                crate::effects::EffectClass::Pure
            } else if f.is_view {
                crate::effects::EffectClass::ReadOnly
            } else {
                crate::effects::EffectClass::Impure
            };
            out.insert(format!("{mod_name}::{}", f.name), class);
        }
    }
    out
}

/// Build the externals manifest needed by typeck of a tx that
/// imports deployed deps. We surface every dep module's `entry`
/// fns as `"module::fn"` keys whose signatures come straight from
/// the bytecode (the artifact persists `param_types` + `return_type`
/// for exactly this purpose).
fn collect_externals(
    deps: &[crate::artifact::Artifact],
) -> HashMap<String, crate::typeck::ModuleSig> {
    let mut out: HashMap<String, crate::typeck::ModuleSig> = HashMap::new();
    for dep in deps {
        for module in &dep.modules {
            for f in &module.functions {
                if !f.is_entry { continue; }
                out.insert(
                    format!("{}::{}", module.name, f.name),
                    crate::typeck::ModuleSig {
                        params: f.param_types.clone(),
                        ret: f.return_type.clone(),
                    },
                );
            }
        }
    }
    out
}

/// Build an effect-class manifest from `view`/`pure` annotations
/// on dep entries. A `pure` entry maps to `Pure`, `view` to
/// `ReadOnly`; un-annotated entries default to `Impure` so the
/// classifier stays conservative. Used by `compile_tx` so a
/// `view fn main() { return bank::balance_of(...); }` typeck
/// against an annotated dep without falsely concluding the call
/// is impure.
fn collect_external_effects(
    deps: &[crate::artifact::Artifact],
) -> HashMap<String, crate::effects::EffectClass> {
    let mut out: HashMap<String, crate::effects::EffectClass> = HashMap::new();
    for dep in deps {
        for module in &dep.modules {
            for f in &module.functions {
                if !f.is_entry { continue; }
                let class = if f.is_pure {
                    crate::effects::EffectClass::Pure
                } else if f.is_view {
                    crate::effects::EffectClass::ReadOnly
                } else {
                    crate::effects::EffectClass::Impure
                };
                out.insert(format!("{}::{}", module.name, f.name), class);
            }
        }
    }
    out
}

/// Build a `(module, fn) → (is_view, is_pure)` map from parsed
/// modules' AST. Used by `compile_named_with_extras` to stamp
/// each emitted `Instr::CallExternal` with the callee's effect
/// bound, so the read-cluster optimizer doesn't fence on
/// `view`/`pure` cross-module calls.
fn collect_decl_fn_flags(
    parsed: &[(String, crate::ast::Module)],
) -> HashMap<(String, String), (bool, bool)> {
    let mut out: HashMap<(String, String), (bool, bool)> = HashMap::new();
    for (mod_name, m) in parsed {
        for f in &m.functions {
            if !f.is_entry { continue; }
            out.insert((mod_name.clone(), f.name.clone()), (f.is_view, f.is_pure));
        }
    }
    out
}

/// Same shape as `collect_decl_fn_flags`, but reads the flags from
/// already-compiled dep artifacts (where each `BcFn` already carries
/// `is_view`/`is_pure`).
fn collect_dep_fn_flags(
    deps: &[crate::artifact::Artifact],
) -> HashMap<(String, String), (bool, bool)> {
    let mut out: HashMap<(String, String), (bool, bool)> = HashMap::new();
    for dep in deps {
        for module in &dep.modules {
            for f in &module.functions {
                if !f.is_entry { continue; }
                out.insert((module.name.clone(), f.name.clone()), (f.is_view, f.is_pure));
            }
        }
    }
    out
}

/// Walk every CallExternal in every function and confirm the
/// referenced `module::fn` resolves to an entry function in the
/// supplied module set. Used both at fresh-compile time and again
/// when an artifact comes back across the wire.
fn validate_cross_module_calls(
    modules: &[BcModule],
    module_index: &HashMap<String, usize>,
) -> Result<(), Error> {
    for m in modules {
        for f in &m.functions {
            for instr in &f.code {
                if let crate::bc::Instr::CallExternal {
                    module_name_idx,
                    fn_name_idx,
                    ..
                } = instr
                {
                    let mn = const_str(&f.consts[*module_name_idx as usize])?;
                    let fnn = const_str(&f.consts[*fn_name_idx as usize])?;
                    let target_idx = module_index.get(&mn).ok_or_else(|| {
                        Error::new(
                            ErrorKind::Type,
                            format!("module '{mn}' is not loaded"),
                            Span::default(),
                        )
                    })?;
                    let target = &modules[*target_idx];
                    let target_fn = target.fn_index.get(&fnn).ok_or_else(|| {
                        Error::new(
                            ErrorKind::Type,
                            format!("'{mn}::{fnn}' is not defined"),
                            Span::default(),
                        )
                    })?;
                    if !target.functions[*target_fn].is_entry {
                        return Err(Error::new(
                            ErrorKind::Type,
                            format!("'{mn}::{fnn}' is not an entry function"),
                            Span::default(),
                        ));
                    }
                }
            }
        }
    }
    Ok(())
}

fn const_str(c: &crate::bc::Const) -> Result<String, Error> {
    match c {
        crate::bc::Const::Str(s) => Ok(s.clone()),
        _ => Err(Error::new(
            ErrorKind::Type,
            "expected string const for module/function name",
            Span::default(),
        )),
    }
}
