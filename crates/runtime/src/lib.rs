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
        clippy::map_unwrap_or,
        clippy::clippy::print_stdout,
        clippy::unicode_not_nfc,
        clippy::use_self
    )
)]

use std::error::Error;

mod export;
mod externref;
mod imports;
mod instance;
mod jit_int;
mod memory;
mod mmap;
mod table;
mod traphandlers;
mod vmcontext;

pub mod debug_builtins;
pub mod libcalls;

pub use crate::export::*;
pub use crate::externref::*;
pub use crate::imports::Imports;
pub use crate::instance::{
    InstanceAllocationRequest, InstanceAllocator, InstanceHandle, InstanceLimits,
    InstantiationError, LinkError, ModuleLimits, OnDemandInstanceAllocator,
    PoolingAllocationStrategy, PoolingInstanceAllocator, ResourceLimiter, DEFAULT_INSTANCE_LIMIT,
    DEFAULT_MEMORY_LIMIT, DEFAULT_TABLE_LIMIT,
};
pub use crate::jit_int::GdbJitImageRegistration;
pub use crate::memory::{Memory, RuntimeLinearMemory, RuntimeMemoryCreator};
pub use crate::mmap::Mmap;
pub use crate::table::{Table, TableElement};
pub use crate::traphandlers::{
    catch_traps, init_traps, raise_lib_trap, raise_user_trap, resume_panic, tls_eager_initialize,
    SignalHandler, TlsRestore, Trap,
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

/// Dynamic runtime functionality needed by this crate throughout the execution
/// of a wasm instance.
///
/// This trait is used to store a raw pointer trait object within each
/// `VMContext`. This raw pointer trait object points back to the
/// `wasmtime::Store` internally but is type-erased so this `wasmtime_runtime`
/// crate doesn't need the entire `wasmtime` crate to build.
///
/// Note that this is an extra-unsafe trait because no heed is paid to the
/// lifetime of this store or the Send/Sync-ness of this store. All of that must
/// be respected by embedders (e.g. the `wasmtime::Store` structure). The theory
/// is that `wasmtime::Store` handles all this correctly.
pub unsafe trait Store {
    /// Returns the raw pointer in memory where this store's shared
    /// `VMInterrupts` structure is located.
    ///
    /// Used to configure `VMContext` initialization and store the right pointer
    /// in the `VMContext`.
    fn vminterrupts(&self) -> *mut VMInterrupts;

    /// Returns the externref management structures necessary for this store.
    ///
    /// The first element returned is the table in which externrefs are stored
    /// throughout wasm execution, and the second element is how to look up
    /// module information for gc requests.
    fn externref_activations_table(
        &mut self,
    ) -> (&mut VMExternRefActivationsTable, &dyn ModuleInfoLookup);

    /// Returns a reference to the store's limiter for limiting resources, if any.
    fn limiter(&mut self) -> Option<&mut dyn ResourceLimiter>;

    /// Callback invoked whenever fuel runs out by a wasm instance. If an error
    /// is returned that's raised as a trap. Otherwise wasm execution will
    /// continue as normal.
    fn out_of_gas(&mut self) -> Result<(), Box<dyn Error + Send + Sync>>;
}

/// On OpenBSD, we need to map any memory that is used as a stack with the
/// `MAP_STACK` flag, due to a security mitigation on that OS. This is not (yet)
/// defined in the `libc` crate so we define it here. The value comes from
/// <sys/mman.h> on OpenBSD.
#[cfg(target_os = "openbsd")]
const STACK_MMAP_FLAGS: libc::c_int = 0x4000;
#[cfg(not(target_os = "openbsd"))]
const STACK_MMAP_FLAGS: libc::c_int = 0;
