//! Object-file writing library using the wasmtime environment.

#![deny(
    missing_docs,
    trivial_numeric_casts,
    unused_extern_crates,
    unstable_features
)]
#![warn(unused_import_braces)]
#![cfg_attr(feature = "clippy", plugin(clippy(conf_file = "../../clippy.toml")))]
#![cfg_attr(feature = "cargo-clippy", allow(clippy::new_without_default))]
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

mod builder;
mod context;
mod data_segment;
mod module;
mod patchable;
mod table;

pub use crate::builder::{utils, ObjectBuilder, ObjectBuilderTarget};
pub use crate::module::emit_module;
pub use crate::patchable::{
    convert_object_elf_to_loadable_file, ensure_supported_elf_format, patch_loadable_file,
    sanitize_loadable_file,
};

/// Version number of this crate.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
