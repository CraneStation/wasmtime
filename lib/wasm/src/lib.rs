//! Performs translation from a wasm module in binary format to the in-memory form
//! of Cranelift IR. More particularly, it translates the code of all the functions bodies and
//! interacts with an environment implementing the
//! [`ModuleEnvironment`](trait.ModuleEnvironment.html)
//! trait to deal with tables, globals and linear memory.
//!
//! The crate provides a `DummyEnvironment` struct that will allow to translate the code of the
//! functions but will fail at execution.
//!
//! The main function of this module is [`translate_module`](fn.translate_module.html).

#![deny(missing_docs, trivial_numeric_casts, unused_extern_crates)]
#![warn(unused_import_braces)]
#![cfg_attr(feature = "std", deny(unstable_features))]
#![cfg_attr(feature = "clippy", plugin(clippy(conf_file = "../../clippy.toml")))]
#![cfg_attr(
    feature = "cargo-clippy",
    allow(new_without_default, new_without_default_derive)
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
#![cfg_attr(not(feature = "std"), no_std)]
#![cfg_attr(not(feature = "std"), feature(alloc))]

mod code_translator;
mod environ;
mod func_translator;
mod module_translator;
mod sections_translator;
mod state;
mod translation_utils;

pub use crate::environ::{
    DummyEnvironment, FuncEnvironment, GlobalVariable, ModuleEnvironment, ReturnMode, WasmError,
    WasmResult,
};
pub use crate::func_translator::FuncTranslator;
pub use crate::module_translator::translate_module;
pub use crate::translation_utils::{
    DefinedFuncIndex, DefinedGlobalIndex, DefinedMemoryIndex, DefinedTableIndex, FuncIndex, Global,
    GlobalIndex, GlobalInit, Memory, MemoryIndex, SignatureIndex, Table, TableElementType,
    TableIndex,
};

#[cfg(not(feature = "std"))]
mod std {
    extern crate alloc;

    pub use self::alloc::string;
    pub use self::alloc::vec;
    pub use core::convert;
    pub use core::fmt;
    pub use core::option;
    pub use core::{cmp, i32, str, u32};
    pub mod collections {
        #[allow(unused_extern_crates)]
        extern crate hashmap_core;

        pub use self::hashmap_core::{map as hash_map, HashMap};
    }
}
