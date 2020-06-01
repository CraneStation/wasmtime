#![cfg(feature = "benches")]
#![feature(test)]

extern crate test;

use anyhow::{bail, Context as _, Result};
use std::path::Path;
use wasmtime::{Config, Engine, OptLevel, Store, Strategy};
use wasmtime_wast::WastContext;
use wast::{
    parser::{self, ParseBuffer},
    Wat,
};

macro_rules! mk_test {
    ($(#[$attrs:meta])* $name:ident, $path:expr, $strategy:expr) => {
        mod $name {
            use wasmtime::Strategy;

            #[bench]
            $(#[$attrs])*
            fn compile(b: &mut ::test::bench::Bencher) {
                crate::bench_compile(b, $path, $strategy).unwrap();
            }

            #[bench]
            $(#[$attrs])*
            fn run(b: &mut ::test::bench::Bencher) {
                crate::bench_run(b, $path, $strategy).unwrap();
            }
        }
    };
}

include!(concat!(env!("OUT_DIR"), "/wast_testsuite_tests.rs"));

fn bench_compile(b: &mut test::bench::Bencher, wast: &str, strategy: Strategy) -> Result<()> {
    let path = Path::new(wast);

    let simd = path.iter().any(|s| s == "simd");

    let bulk_mem = path.iter().any(|s| s == "bulk-memory-operations");

    // Some simd tests assume support for multiple tables, which are introduced
    // by reference types.
    let reftypes = simd || path.iter().any(|s| s == "reference-types");

    let multi_val = path.iter().any(|s| s == "multi-value");

    let mut cfg = Config::new();
    cfg.wasm_simd(simd)
        .wasm_bulk_memory(bulk_mem)
        .wasm_reference_types(reftypes)
        .wasm_multi_value(multi_val)
        .strategy(strategy)?
        .cranelift_debug_verifier(cfg!(debug_assertions));

    // FIXME: https://github.com/bytecodealliance/wasmtime/issues/1186
    if simd {
        cfg.cranelift_opt_level(OptLevel::None);
    }

    let store = Store::new(&Engine::new(&cfg));
    let mut wast_context = WastContext::new(store);
    wast_context.register_spectest()?;

    let adjust_wast = |mut err: wast::Error| {
        err.set_path(path);
        err.set_text(wast);
        err
    };

    let file_contents = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read `{}`", path.display()))?;
    let buf = ParseBuffer::new(&file_contents).map_err(adjust_wast)?;
    let ast = parser::parse::<wast::Wast>(&buf).map_err(adjust_wast)?;

    let mut modules = Vec::new();

    for directive in ast.directives {
        use wast::WastDirective::*;

        match directive {
            Module(mut module) => {
                let binary = module.encode()?;

                let (name, bin) = (module.id.map(|s| s.name().to_string()), binary);
                wast_context.module(name.as_ref().map(|s| &s[..]), &bin)?;

                b.bytes += bin.len() as u64;
                modules.push((name, bin));
            }
            QuoteModule { span: _, source } => {
                let mut module = String::new();
                for src in source {
                    module.push_str(std::str::from_utf8(src)?);
                    module.push_str(" ");
                }
                let buf = ParseBuffer::new(&module)?;
                let mut wat = parser::parse::<Wat>(&buf)?;
                let binary = wat.module.encode()?;
                let (name, bin) = (wat.module.id.map(|s| s.name().to_string()), binary);
                wast_context.module(name.as_ref().map(|s| &s[..]), &bin)?;

                b.bytes += bin.len() as u64;
                modules.push((name, bin));
            }
            Register {
                span: _,
                name,
                module,
            } => {
                wast_context.register(module.map(|s| s.name()), name)?;
            }
            _ => {}
        }
    }

    b.iter(|| {
        for (name, bin) in &modules {
            wast_context
                .module(name.as_ref().map(|s| &s[..]), bin)
                .unwrap();
        }
    });

    Ok(())
}

fn bench_run(b: &mut test::bench::Bencher, wast: &str, strategy: Strategy) -> Result<()> {
    let path = Path::new(wast);

    let simd = path.iter().any(|s| s == "simd");

    let bulk_mem = path.iter().any(|s| s == "bulk-memory-operations");

    // Some simd tests assume support for multiple tables, which are introduced
    // by reference types.
    let reftypes = simd || path.iter().any(|s| s == "reference-types");

    let multi_val = path.iter().any(|s| s == "multi-value");

    let mut cfg = Config::new();
    cfg.wasm_simd(simd)
        .wasm_bulk_memory(bulk_mem)
        .wasm_reference_types(reftypes)
        .wasm_multi_value(multi_val)
        .strategy(strategy)?
        .cranelift_debug_verifier(cfg!(debug_assertions));

    // FIXME: https://github.com/bytecodealliance/wasmtime/issues/1186
    if simd {
        cfg.cranelift_opt_level(OptLevel::None);
    }

    let store = Store::new(&Engine::new(&cfg));
    let mut wast_context = WastContext::new(store);
    wast_context.register_spectest()?;

    let adjust_wast = |mut err: wast::Error| {
        err.set_path(path);
        err.set_text(wast);
        err
    };

    let file_contents = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read `{}`", path.display()))?;
    let buf = ParseBuffer::new(&file_contents).map_err(adjust_wast)?;
    let ast = parser::parse::<wast::Wast>(&buf).map_err(adjust_wast)?;

    let mut execute_directives = Vec::new();
    let mut current = None;
    let mut dummy_name_id = 0;

    for directive in ast.directives {
        use wast::{WastDirective::*, WastExecute};

        match directive {
            Module(mut module) => {
                let binary = module.encode()?;

                let name = module.id.map(|s| s.name().to_string()).unwrap_or_else(|| {
                    let name = format!("---dummy-name-{}", dummy_name_id);
                    dummy_name_id += 1;
                    name
                });
                wast_context.module(Some(&name), &binary)?;
                current = Some(name);
            }
            QuoteModule { span: _, source } => {
                let mut module = String::new();
                for src in source {
                    module.push_str(std::str::from_utf8(src)?);
                    module.push_str(" ");
                }
                let buf = ParseBuffer::new(&module)?;
                let mut wat = parser::parse::<Wat>(&buf)?;
                let binary = wat.module.encode()?;
                let name = wat
                    .module
                    .id
                    .map(|s| s.name().to_string())
                    .unwrap_or_else(|| {
                        let name = format!("---dummy-name-{}", dummy_name_id);
                        dummy_name_id += 1;
                        name
                    });
                wast_context.module(Some(&name), &binary)?;
                current = Some(name);
            }
            Register {
                span: _,
                name,
                module,
            } => {
                wast_context.register(module.map(|s| s.name()), name)?;
            }
            Invoke(call)
            | AssertExhaustion { call, .. }
            | AssertReturn {
                exec: WastExecute::Invoke(call),
                ..
            }
            | AssertTrap {
                exec: WastExecute::Invoke(call),
                ..
            } => {
                use wasmtime::Val;

                // Copy/pasted from `wasmtime-wast`
                fn runtime_value(v: &wast::Expression<'_>) -> Result<Val> {
                    use wast::Instruction::*;

                    if v.instrs.len() != 1 {
                        bail!("too many instructions in {:?}", v);
                    }
                    Ok(match &v.instrs[0] {
                        I32Const(x) => Val::I32(*x),
                        I64Const(x) => Val::I64(*x),
                        F32Const(x) => Val::F32(x.bits),
                        F64Const(x) => Val::F64(x.bits),
                        V128Const(x) => Val::V128(u128::from_le_bytes(x.to_le_bytes())),
                        other => bail!("couldn't convert {:?} to a runtime value", other),
                    })
                }

                let values = call
                    .args
                    .iter()
                    .map(runtime_value)
                    .collect::<Result<Vec<_>>>()?;

                execute_directives.push((
                    call.module
                        .map(|m| m.name().to_string())
                        .unwrap_or_else(|| current.clone().unwrap()),
                    call.name.to_string(),
                    values,
                ));
            }
            _ => {}
        }
    }

    b.iter(|| {
        for (mod_name, fn_name, args) in &execute_directives {
            wast_context.invoke(Some(mod_name), fn_name, args).unwrap();
        }
    });

    Ok(())
}
