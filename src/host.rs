//! Host imports — typed Rust closures the guest can call.
//!
//! For OCC soundness, host functions should be deterministic with respect to
//! their inputs (i.e. given the same args + same state snapshot, return the
//! same value). The runtime cannot enforce this — it's the host's
//! responsibility. Pure logging is fine. A host clock that returns wall-time
//! is NOT safe; instead, pass the current block-height/timestamp in as
//! state and read it via state slots.
//!
//! Closures are required to be `Send + Sync` so that batch execution
//! (`crate::occ::commit_batch`) can run them in parallel across rayon
//! workers.

use std::collections::HashMap;

use crate::value::Value;

/// Typed error returned by a host import. `code` lets host-defined error
/// categories be propagated; `msg` is human-readable. Callers pattern-match
/// on `code` to recover; the engine wraps this into a runtime [`crate::Error`]
/// when surfacing to the guest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostError {
    pub code: u32,
    pub msg: String,
}

impl HostError {
    pub fn new(code: u32, msg: impl Into<String>) -> Self {
        Self { code, msg: msg.into() }
    }

    pub fn invalid_args(msg: impl Into<String>) -> Self {
        Self { code: 1, msg: msg.into() }
    }

    pub fn aborted(msg: impl Into<String>) -> Self {
        Self { code: 2, msg: msg.into() }
    }
}

impl std::fmt::Display for HostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "host error {}: {}", self.code, self.msg)
    }
}

impl std::error::Error for HostError {}

pub type HostFn = Box<dyn Fn(&[Value]) -> Result<Value, HostError> + Send + Sync>;

#[derive(Default)]
pub struct Host {
    funcs: HashMap<String, HostFn>,
}

impl Host {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn bind<F>(&mut self, name: impl Into<String>, f: F) -> &mut Self
    where
        F: Fn(&[Value]) -> Result<Value, HostError> + Send + Sync + 'static,
    {
        self.funcs.insert(name.into(), Box::new(f));
        self
    }

    pub fn has(&self, name: &str) -> bool {
        self.funcs.contains_key(name)
    }

    pub fn call(&self, name: &str, args: &[Value]) -> Result<Value, HostError> {
        let f = self
            .funcs
            .get(name)
            .ok_or_else(|| HostError::new(0, format!("no host impl bound for '{name}'")))?;
        f(args)
    }
}
