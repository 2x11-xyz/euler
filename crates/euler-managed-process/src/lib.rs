//! Language-neutral managed-process extension runtime.
//!
//! The runtime adapts a versioned JSON-RPC stdio process to Euler's existing
//! `Extension` / `HostApi` boundary. It deliberately contains no language
//! names: Python is one SDK client, not a runtime mode.

mod protocol;
mod runtime;

pub use runtime::{
    ManagedProcessExtension, ManagedProcessLimits, ManagedProcessRuntimeError,
    MANAGED_PROCESS_PROTOCOL_VERSION,
};
