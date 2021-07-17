//! Standalone environment for WebAssembly using Cranelift. Provides functions to translate
//! `get_global`, `set_global`, `memory.size`, `memory.grow`, `call_indirect` that hardcode in
//! the translation the base addresses of regions of memory that will hold the globals, tables and
//! linear memories.

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

mod address_map;
mod builtin;
mod compilation;
mod data_structures;
mod module;
mod module_environ;
mod tunables;
mod vmoffsets;

pub use crate::address_map::*;
pub use crate::builtin::*;
pub use crate::compilation::*;
pub use crate::data_structures::*;
pub use crate::module::*;
pub use crate::module_environ::*;
pub use crate::tunables::Tunables;
pub use crate::vmoffsets::*;

/// WebAssembly page sizes are defined to be 64KiB.
pub const WASM_PAGE_SIZE: u32 = 0x10000;

/// The number of pages we can have before we run out of byte index space.
pub const WASM_MAX_PAGES: u32 = 0x10000;

/// Version number of this crate.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Returns the reference type to use for the provided wasm type.
pub fn reference_type(wasm_ty: cranelift_wasm::WasmType, pointer_type: ir::Type) -> ir::Type {
    match wasm_ty {
        cranelift_wasm::WasmType::FuncRef => pointer_type,
        cranelift_wasm::WasmType::ExternRef => match pointer_type {
            ir::types::I32 => ir::types::R32,
            ir::types::I64 => ir::types::R64,
            _ => panic!("unsupported pointer type"),
        },
        _ => panic!("unsupported Wasm reference type"),
    }
}

/// Iterates through all `LibCall` members and all runtime exported functions.
#[macro_export]
macro_rules! for_each_libcall {
    ($op:ident) => {
        $op![
            (UdivI64, wasmtime_i64_udiv),
            (SdivI64, wasmtime_i64_sdiv),
            (UremI64, wasmtime_i64_urem),
            (SremI64, wasmtime_i64_srem),
            (IshlI64, wasmtime_i64_ishl),
            (UshrI64, wasmtime_i64_ushr),
            (SshrI64, wasmtime_i64_sshr),
            (CeilF32, wasmtime_f32_ceil),
            (FloorF32, wasmtime_f32_floor),
            (TruncF32, wasmtime_f32_trunc),
            (NearestF32, wasmtime_f32_nearest),
            (CeilF64, wasmtime_f64_ceil),
            (FloorF64, wasmtime_f64_floor),
            (TruncF64, wasmtime_f64_trunc),
            (NearestF64, wasmtime_f64_nearest),
            (Probestack, wasmtime_probestack),
            (Memcpy, wasmtime_memcpy),
            (Memset, wasmtime_memset),
            (Memmove, wasmtime_memmove),
            (ElfTlsGetAddr, wasmtime_tls_get_addr)
        ];
    };
}

fn __assert_all_for_each_libcalls(libcall: crate::ir::LibCall) {
    use crate::ir::LibCall;
    macro_rules! assert_libcalls {
        [$(($libcall:ident, $export:ident)),*] => {
            match libcall {
                $(
                   | LibCall::$libcall
                )+ => (),
            }
        };
    }
    for_each_libcall!(assert_libcalls)
}
