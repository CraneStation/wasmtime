mod async_functions;
mod cli_tests;
mod custom_signal_handler;
mod debug;
mod externals;
mod fuel;
mod func;
mod funcref;
mod fuzzing;
mod gc;
mod globals;
mod host_funcs;
mod iloop;
mod import_calling_export;
mod import_indexes;
mod instance;
mod invoke_func_via_table;
mod limits;
mod linker;
mod memory;
mod memory_creator;
mod module;
mod module_linking;
mod module_serialize;
mod name;
mod native_hooks;
#[cfg(not(target_os = "openbsd"))]
mod pooling_allocator;
mod stack_overflow;
mod store;
mod table;
mod traps;
mod wast;

/// A helper to compile a module in a new store with reference types enabled.
pub(crate) fn ref_types_module(
    source: &str,
) -> anyhow::Result<(wasmtime::Store<()>, wasmtime::Module)> {
    use wasmtime::*;

    let _ = env_logger::try_init();

    let mut config = Config::new();
    config.wasm_reference_types(true);

    let engine = Engine::new(&config)?;
    let store = Store::new(&engine, ());

    let module = Module::new(&engine, source)?;

    Ok((store, module))
}

/// A helper determining whether the pooling allocator tests should be skipped.
pub(crate) fn skip_pooling_allocator_tests() -> bool {
    // There are a couple of issues when running the pooling allocator tests under QEMU:
    // - high memory usage that may exceed the limits imposed by the environment (e.g. CI)
    // - https://github.com/bytecodealliance/wasmtime/pull/2518#issuecomment-747280133
    std::env::var("WASMTIME_TEST_NO_HOG_MEMORY").is_ok() || cfg!(target_os = "openbsd")
}
