//! Top-level lib.rs for `cranelift_module`.

#![deny(missing_docs, trivial_numeric_casts, unused_extern_crates)]
#![warn(unused_import_braces)]
#![cfg_attr(feature = "std", deny(unstable_features))]
#![cfg_attr(feature = "clippy", plugin(clippy(conf_file = "../../clippy.toml")))]
#![cfg_attr(feature = "cargo-clippy", allow(clippy::new_without_default))]
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
#![no_std]

#[cfg(not(feature = "std"))]
#[macro_use]
extern crate alloc as std;
#[cfg(feature = "std")]
#[macro_use]
extern crate std;

#[cfg(not(feature = "std"))]
use hashbrown::{hash_map, HashMap};
use std::borrow::ToOwned;
use std::boxed::Box;
#[cfg(feature = "std")]
use std::collections::{hash_map, HashMap};
use std::string::String;

use cranelift_codegen::ir;

mod data_context;
mod module;
mod traps;

pub use crate::data_context::{DataContext, DataDescription, Init};
pub use crate::module::{
    DataId, FuncId, FuncOrDataId, Linkage, Module, ModuleCompiledFunction, ModuleDeclarations,
    ModuleError, ModuleResult, RelocRecord,
};
pub use crate::traps::TrapSite;

/// Version number of this crate.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Default names for `ir::LibCall`s. A function by this name is imported into the object as
/// part of the translation of a `ir::ExternalName::LibCall` variant.
pub fn default_libcall_names() -> Box<dyn Fn(ir::LibCall) -> String + Send + Sync> {
    Box::new(move |libcall| match libcall {
        ir::LibCall::Probestack => "__cranelift_probestack".to_owned(),
        ir::LibCall::UdivI64 => "__udivdi3".to_owned(),
        ir::LibCall::SdivI64 => "__divdi3".to_owned(),
        ir::LibCall::UremI64 => "__umoddi3".to_owned(),
        ir::LibCall::SremI64 => "__moddi3".to_owned(),
        ir::LibCall::IshlI64 => "__ashldi3".to_owned(),
        ir::LibCall::UshrI64 => "__lshrdi3".to_owned(),
        ir::LibCall::SshrI64 => "__ashrdi3".to_owned(),
        ir::LibCall::CeilF32 => "ceilf".to_owned(),
        ir::LibCall::CeilF64 => "ceil".to_owned(),
        ir::LibCall::FloorF32 => "floorf".to_owned(),
        ir::LibCall::FloorF64 => "floor".to_owned(),
        ir::LibCall::TruncF32 => "truncf".to_owned(),
        ir::LibCall::TruncF64 => "trunc".to_owned(),
        ir::LibCall::NearestF32 => "nearbyintf".to_owned(),
        ir::LibCall::NearestF64 => "nearbyint".to_owned(),
        ir::LibCall::Memcpy => "memcpy".to_owned(),
        ir::LibCall::Memset => "memset".to_owned(),
        ir::LibCall::Memmove => "memmove".to_owned(),
        ir::LibCall::Memcmp => "memcmp".to_owned(),

        ir::LibCall::ElfTlsGetAddr => "__tls_get_addr".to_owned(),
    })
}
