//! `rend` — an embeddable, sandboxed, affine-typed programming language.
//!
//! Slice 1: lexer + parser + tree-walk interpreter for a tiny core
//! (int, bool, fn, let, if/else, return, arithmetic, comparisons, calls).
//!
//! Future slices add: type checker, affine ownership analysis, register-bytecode VM
//! with fuel metering, and a host embedding API.

pub mod affine;
pub mod artifact;
pub mod ast;
pub mod bc;
pub mod compile;
pub mod effects;
pub mod engine;
pub mod modifier;
pub mod optimize;
pub mod prefetch;
pub mod error;
pub mod gc;
pub mod host;
pub mod interp;
pub mod hashing;
pub mod json;
pub mod kv;
pub mod lexer;
pub mod occ;
pub mod ops;
pub mod parser;
pub mod pmap;
pub mod pbtree;
pub mod pvec;
pub mod serialize;
pub mod token;
pub mod tx;
pub mod typeck;
pub mod value;
pub mod vm;

pub use engine::Engine;
pub use error::{Error, ErrorKind};
pub use host::Host;
pub use value::Value;
pub use vm::Fuel;

/// Lower `src` through the front end, returning a typed + affine-checked AST.
pub fn frontend(src: &str) -> Result<ast::Module, Error> {
    let tokens = lexer::tokenize(src)?;
    let mut module = parser::parse(tokens)?;
    modifier::expand(&mut module)?;
    typeck::resolve_types(&mut module)?;
    typeck::check(&module)?;
    affine::check(&module)?;
    // Stamp DynCall nodes with the called interface method's
    // view/pure flags so the optimizer can avoid fencing read
    // clusters around `view`/`pure` dynamic dispatches even when
    // the target is a local-bound interface.
    typeck::annotate_dyn_calls(&mut module);
    let effects = effects::classify(&module);
    effects::verify_purity_annotations(&module, &effects)?;
    Ok(module)
}

/// Compile `src` and call its `main()` function via the tree-walk interpreter.
/// Convenience helper — modules with `import`s should use [`Engine`].
pub fn run(src: &str) -> Result<Value, Error> {
    let module = frontend(src)?;
    let host = Host::new();
    let kv = kv::EmptyKv;
    interp::Interp::without_storage(&module, &host, &kv).call("main", Vec::new())
}

/// Compile `src` to bytecode and run `main()` on the register VM with the given
/// fuel budget. Modules with `import`s should use [`Engine`].
pub fn run_bc(src: &str, fuel: Fuel) -> Result<Value, Error> {
    let module = frontend(src)?;
    let bc_module = compile::compile(&module)?;
    vm::run(&bc_module, "main", &[], fuel)
}
