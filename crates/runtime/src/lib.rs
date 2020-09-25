//! Runtime library support for Wasmtime.

#![deny(missing_docs, trivial_numeric_casts, unused_extern_crates)]
#![warn(unused_import_braces)]
#![cfg_attr(feature = "clippy", plugin(clippy(conf_file = "../../clippy.toml")))]
#![cfg_attr(
    feature = "cargo-clippy",
    allow(clippy::new_without_default, clippy::new_without_default)
)]
#![cfg_attr(
    feature = "cargo-clippy",
    warn(
        clippy::float_arithmetic,
        clippy::mut_mut,
        clippy::nonminimal_bool,
        clippy::option_map_unwrap_or,
        clippy::option_map_unwrap_or_else,
        clippy::print_stdout,
        clippy::unicode_not_nfc,
        clippy::use_self
    )
)]

mod export;
mod externref;
mod imports;
mod instance;
mod jit_int;
mod memory;
mod mmap;
mod sig_registry;
mod table;
mod traphandlers;
mod vmcontext;

pub mod debug_builtins;
pub mod debugger;
pub mod libcalls;

pub use crate::export::*;
pub use crate::externref::*;
pub use crate::imports::Imports;
pub use crate::instance::{InstanceHandle, InstantiationError, LinkError};
pub use crate::jit_int::GdbJitImageRegistration;
pub use crate::memory::{RuntimeLinearMemory, RuntimeMemoryCreator};
pub use crate::mmap::Mmap;
pub use crate::sig_registry::SignatureRegistry;
pub use crate::table::{Table, TableElement};
pub use crate::traphandlers::{
    catch_traps, init_traps, raise_lib_trap, raise_user_trap, resume_panic, SignalHandler, Trap,
};
pub use crate::vmcontext::{
    VMCallerCheckedAnyfunc, VMContext, VMFunctionBody, VMFunctionImport, VMGlobalDefinition,
    VMGlobalImport, VMInterrupts, VMInvokeArgument, VMMemoryDefinition, VMMemoryImport,
    VMSharedSignatureIndex, VMTableDefinition, VMTableImport, VMTrampoline,
};

/// Version number of this crate.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The Cranelift IR type used for reference types for this target architecture.
pub fn ref_type() -> wasmtime_environ::ir::Type {
    if cfg!(target_pointer_width = "32") {
        wasmtime_environ::ir::types::R32
    } else if cfg!(target_pointer_width = "64") {
        wasmtime_environ::ir::types::R64
    } else {
        unreachable!()
    }
}

/// The Cranelift IR type used for pointer types for this target architecture.
pub fn pointer_type() -> wasmtime_environ::ir::Type {
    if cfg!(target_pointer_width = "32") {
        wasmtime_environ::ir::types::I32
    } else if cfg!(target_pointer_width = "64") {
        wasmtime_environ::ir::types::I64
    } else {
        unreachable!()
    }
}
