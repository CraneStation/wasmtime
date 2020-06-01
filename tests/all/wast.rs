use std::path::Path;
use wasmtime::{Config, Engine, Store, Strategy};
use wasmtime_wast::WastContext;

macro_rules! mk_test {
    ($(#[$attrs:meta])* $name:ident, $path:expr, $strategy:expr) => {
        #[test]
        $(#[$attrs])*
        fn $name() {
            use wasmtime::Strategy;

            crate::wast::run_wast($path, $strategy).unwrap();
        }
    };
}

include!(concat!(env!("OUT_DIR"), "/wast_testsuite_tests.rs"));

// Each of the tests included from `wast_testsuite_tests` will call this
// function which actually executes the `wast` test suite given the `strategy`
// to compile it.
fn run_wast(wast: &str, strategy: Strategy) -> anyhow::Result<()> {
    let wast = Path::new(wast);

    let simd = wast.iter().any(|s| s == "simd");

    let bulk_mem = wast.iter().any(|s| s == "bulk-memory-operations");

    // Some simd tests assume support for multiple tables, which are introduced
    // by reference types.
    let reftypes = simd || wast.iter().any(|s| s == "reference-types");

    let mut cfg = Config::new();
    cfg.wasm_simd(simd)
        .wasm_bulk_memory(bulk_mem)
        .wasm_reference_types(reftypes)
        .strategy(strategy)?
        .cranelift_debug_verifier(true);

    let store = Store::new(&Engine::new(&cfg));
    let mut wast_context = WastContext::new(store);
    wast_context.register_spectest()?;
    wast_context.run_file(wast)?;

    Ok(())
}
