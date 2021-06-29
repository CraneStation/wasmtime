use anyhow::Result;
use std::panic::{self, AssertUnwindSafe};
use std::process::Command;
use wasmtime::*;

#[test]
fn test_trap_return() -> Result<()> {
    let mut store = Store::<()>::default();
    let wat = r#"
        (module
        (func $hello (import "" "hello"))
        (func (export "run") (call $hello))
        )
    "#;

    let module = Module::new(store.engine(), wat)?;
    let hello_type = FuncType::new(None, None);
    let hello_func = Func::new(&mut store, hello_type, |_, _, _| Err(Trap::new("test 123")));

    let instance = Instance::new(&mut store, &module, &[hello_func.into()])?;
    let run_func = instance.get_typed_func::<(), (), _>(&mut store, "run")?;

    let e = run_func
        .call(&mut store, ())
        .err()
        .expect("error calling function");
    assert!(e.to_string().contains("test 123"));

    Ok(())
}

#[test]
#[cfg_attr(all(target_os = "macos", target_arch = "aarch64"), ignore)] // TODO #2808 system libunwind is broken on aarch64
#[cfg_attr(target_os = "openbsd", ignore)] // backtraces are broken on OpenBSD
fn test_trap_trace() -> Result<()> {
    let mut store = Store::<()>::default();
    let wat = r#"
        (module $hello_mod
            (func (export "run") (call $hello))
            (func $hello (unreachable))
        )
    "#;

    let module = Module::new(store.engine(), wat)?;
    let instance = Instance::new(&mut store, &module, &[])?;
    let run_func = instance.get_typed_func::<(), (), _>(&mut store, "run")?;

    let e = run_func
        .call(&mut store, ())
        .err()
        .expect("error calling function");

    let trace = e.trace();
    assert_eq!(trace.len(), 2);
    assert_eq!(trace[0].module_name().unwrap(), "hello_mod");
    assert_eq!(trace[0].func_index(), 1);
    assert_eq!(trace[0].func_name(), Some("hello"));
    assert_eq!(trace[0].func_offset(), 1);
    assert_eq!(trace[0].module_offset(), 0x26);
    assert_eq!(trace[1].module_name().unwrap(), "hello_mod");
    assert_eq!(trace[1].func_index(), 0);
    assert_eq!(trace[1].func_name(), None);
    assert_eq!(trace[1].func_offset(), 1);
    assert_eq!(trace[1].module_offset(), 0x21);
    assert!(
        e.to_string().contains("unreachable"),
        "wrong message: {}",
        e.to_string()
    );

    Ok(())
}

#[test]
#[cfg_attr(all(target_os = "macos", target_arch = "aarch64"), ignore)] // TODO #2808 system libunwind is broken on aarch64
#[cfg_attr(target_os = "openbsd", ignore)] // backtraces are broken on OpenBSD
fn test_trap_trace_cb() -> Result<()> {
    let mut store = Store::<()>::default();
    let wat = r#"
        (module $hello_mod
            (import "" "throw" (func $throw))
            (func (export "run") (call $hello))
            (func $hello (call $throw))
        )
    "#;

    let fn_type = FuncType::new(None, None);
    let fn_func = Func::new(&mut store, fn_type, |_, _, _| Err(Trap::new("cb throw")));

    let module = Module::new(store.engine(), wat)?;
    let instance = Instance::new(&mut store, &module, &[fn_func.into()])?;
    let run_func = instance.get_typed_func::<(), (), _>(&mut store, "run")?;

    let e = run_func
        .call(&mut store, ())
        .err()
        .expect("error calling function");

    let trace = e.trace();
    assert_eq!(trace.len(), 2);
    assert_eq!(trace[0].module_name().unwrap(), "hello_mod");
    assert_eq!(trace[0].func_index(), 2);
    assert_eq!(trace[1].module_name().unwrap(), "hello_mod");
    assert_eq!(trace[1].func_index(), 1);
    assert!(e.to_string().contains("cb throw"));

    Ok(())
}

#[test]
#[cfg_attr(all(target_os = "macos", target_arch = "aarch64"), ignore)] // TODO #2808 system libunwind is broken on aarch64
#[cfg_attr(target_os = "openbsd", ignore)] // backtraces are broken on OpenBSD
fn test_trap_stack_overflow() -> Result<()> {
    let mut store = Store::<()>::default();
    let wat = r#"
        (module $rec_mod
            (func $run (export "run") (call $run))
        )
    "#;

    let module = Module::new(store.engine(), wat)?;
    let instance = Instance::new(&mut store, &module, &[])?;
    let run_func = instance.get_typed_func::<(), (), _>(&mut store, "run")?;

    let e = run_func
        .call(&mut store, ())
        .err()
        .expect("error calling function");

    let trace = e.trace();
    assert!(trace.len() >= 32);
    for i in 0..trace.len() {
        assert_eq!(trace[i].module_name().unwrap(), "rec_mod");
        assert_eq!(trace[i].func_index(), 0);
        assert_eq!(trace[i].func_name(), Some("run"));
    }
    assert!(e.to_string().contains("call stack exhausted"));

    Ok(())
}

#[test]
#[cfg_attr(all(target_os = "macos", target_arch = "aarch64"), ignore)] // TODO #2808 system libunwind is broken on aarch64
#[cfg_attr(target_os = "openbsd", ignore)] // backtraces are broken on OpenBSD
fn trap_display_pretty() -> Result<()> {
    let mut store = Store::<()>::default();
    let wat = r#"
        (module $m
            (func $die unreachable)
            (func call $die)
            (func $foo call 1)
            (func (export "bar") call $foo)
        )
    "#;

    let module = Module::new(store.engine(), wat)?;
    let instance = Instance::new(&mut store, &module, &[])?;
    let run_func = instance.get_typed_func::<(), (), _>(&mut store, "bar")?;

    let e = run_func
        .call(&mut store, ())
        .err()
        .expect("error calling function");
    assert_eq!(
        e.to_string(),
        "\
wasm trap: unreachable
wasm backtrace:
    0:   0x23 - m!die
    1:   0x27 - m!<wasm function 1>
    2:   0x2c - m!foo
    3:   0x31 - m!<wasm function 3>
"
    );
    Ok(())
}

#[test]
#[cfg_attr(all(target_os = "macos", target_arch = "aarch64"), ignore)] // TODO #2808 system libunwind is broken on aarch64
#[cfg_attr(target_os = "openbsd", ignore)] // backtraces are broken on OpenBSD
fn trap_display_multi_module() -> Result<()> {
    let mut store = Store::<()>::default();
    let wat = r#"
        (module $a
            (func $die unreachable)
            (func call $die)
            (func $foo call 1)
            (func (export "bar") call $foo)
        )
    "#;

    let module = Module::new(store.engine(), wat)?;
    let instance = Instance::new(&mut store, &module, &[])?;
    let bar = instance.get_export(&mut store, "bar").unwrap();

    let wat = r#"
        (module $b
            (import "" "" (func $bar))
            (func $middle call $bar)
            (func (export "bar2") call $middle)
        )
    "#;
    let module = Module::new(store.engine(), wat)?;
    let instance = Instance::new(&mut store, &module, &[bar])?;
    let bar2 = instance.get_typed_func::<(), (), _>(&mut store, "bar2")?;

    let e = bar2
        .call(&mut store, ())
        .err()
        .expect("error calling function");
    assert_eq!(
        e.to_string(),
        "\
wasm trap: unreachable
wasm backtrace:
    0:   0x23 - a!die
    1:   0x27 - a!<wasm function 1>
    2:   0x2c - a!foo
    3:   0x31 - a!<wasm function 3>
    4:   0x29 - b!middle
    5:   0x2e - b!<wasm function 2>
"
    );
    Ok(())
}

#[test]
fn trap_start_function_import() -> Result<()> {
    let mut store = Store::<()>::default();
    let binary = wat::parse_str(
        r#"
            (module $a
                (import "" "" (func $foo))
                (start $foo)
            )
        "#,
    )?;

    let module = Module::new(store.engine(), &binary)?;
    let sig = FuncType::new(None, None);
    let func = Func::new(&mut store, sig, |_, _, _| Err(Trap::new("user trap")));
    let err = Instance::new(&mut store, &module, &[func.into()])
        .err()
        .unwrap();
    assert!(err
        .downcast_ref::<Trap>()
        .unwrap()
        .to_string()
        .contains("user trap"));
    Ok(())
}

#[test]
fn rust_panic_import() -> Result<()> {
    let mut store = Store::<()>::default();
    let binary = wat::parse_str(
        r#"
            (module $a
                (import "" "" (func $foo))
                (import "" "" (func $bar))
                (func (export "foo") call $foo)
                (func (export "bar") call $bar)
            )
        "#,
    )?;

    let module = Module::new(store.engine(), &binary)?;
    let sig = FuncType::new(None, None);
    let func = Func::new(&mut store, sig, |_, _, _| panic!("this is a panic"));
    let func2 = Func::wrap(&mut store, || panic!("this is another panic"));
    let instance = Instance::new(&mut store, &module, &[func.into(), func2.into()])?;
    let func = instance.get_typed_func::<(), (), _>(&mut store, "foo")?;
    let err =
        panic::catch_unwind(AssertUnwindSafe(|| drop(func.call(&mut store, ())))).unwrap_err();
    assert_eq!(err.downcast_ref::<&'static str>(), Some(&"this is a panic"));

    let func = instance.get_typed_func::<(), (), _>(&mut store, "bar")?;
    let err = panic::catch_unwind(AssertUnwindSafe(|| {
        drop(func.call(&mut store, ()));
    }))
    .unwrap_err();
    assert_eq!(
        err.downcast_ref::<&'static str>(),
        Some(&"this is another panic")
    );
    Ok(())
}

#[test]
fn rust_panic_start_function() -> Result<()> {
    let mut store = Store::<()>::default();
    let binary = wat::parse_str(
        r#"
            (module $a
                (import "" "" (func $foo))
                (start $foo)
            )
        "#,
    )?;

    let module = Module::new(store.engine(), &binary)?;
    let sig = FuncType::new(None, None);
    let func = Func::new(&mut store, sig, |_, _, _| panic!("this is a panic"));
    let err = panic::catch_unwind(AssertUnwindSafe(|| {
        drop(Instance::new(&mut store, &module, &[func.into()]));
    }))
    .unwrap_err();
    assert_eq!(err.downcast_ref::<&'static str>(), Some(&"this is a panic"));

    let func = Func::wrap(&mut store, || panic!("this is another panic"));
    let err = panic::catch_unwind(AssertUnwindSafe(|| {
        drop(Instance::new(&mut store, &module, &[func.into()]));
    }))
    .unwrap_err();
    assert_eq!(
        err.downcast_ref::<&'static str>(),
        Some(&"this is another panic")
    );
    Ok(())
}

#[test]
fn mismatched_arguments() -> Result<()> {
    let mut store = Store::<()>::default();
    let binary = wat::parse_str(
        r#"
            (module $a
                (func (export "foo") (param i32))
            )
        "#,
    )?;

    let module = Module::new(store.engine(), &binary)?;
    let instance = Instance::new(&mut store, &module, &[])?;
    let func = instance.get_func(&mut store, "foo").unwrap();
    assert_eq!(
        func.call(&mut store, &[]).unwrap_err().to_string(),
        "expected 1 arguments, got 0"
    );
    assert_eq!(
        func.call(&mut store, &[Val::F32(0)])
            .unwrap_err()
            .to_string(),
        "argument type mismatch: found f32 but expected i32",
    );
    assert_eq!(
        func.call(&mut store, &[Val::I32(0), Val::I32(1)])
            .unwrap_err()
            .to_string(),
        "expected 1 arguments, got 2"
    );
    Ok(())
}

#[test]
fn call_signature_mismatch() -> Result<()> {
    let mut store = Store::<()>::default();
    let binary = wat::parse_str(
        r#"
            (module $a
                (func $foo
                    i32.const 0
                    call_indirect)
                (func $bar (param i32))
                (start $foo)

                (table 1 anyfunc)
                (elem (i32.const 0) 1)
            )
        "#,
    )?;

    let module = Module::new(store.engine(), &binary)?;
    let err = Instance::new(&mut store, &module, &[])
        .err()
        .unwrap()
        .downcast::<Trap>()
        .unwrap();
    assert!(err
        .to_string()
        .contains("wasm trap: indirect call type mismatch"));
    Ok(())
}

#[test]
#[cfg_attr(all(target_os = "macos", target_arch = "aarch64"), ignore)] // TODO #2808 system libunwind is broken on aarch64
#[cfg_attr(target_os = "openbsd", ignore)] // backtraces are broken on OpenBSD
fn start_trap_pretty() -> Result<()> {
    let mut store = Store::<()>::default();
    let wat = r#"
        (module $m
            (func $die unreachable)
            (func call $die)
            (func $foo call 1)
            (func $start call $foo)
            (start $start)
        )
    "#;

    let module = Module::new(store.engine(), wat)?;
    let e = match Instance::new(&mut store, &module, &[]) {
        Ok(_) => panic!("expected failure"),
        Err(e) => e.downcast::<Trap>()?,
    };

    assert_eq!(
        e.to_string(),
        "\
wasm trap: unreachable
wasm backtrace:
    0:   0x1d - m!die
    1:   0x21 - m!<wasm function 1>
    2:   0x26 - m!foo
    3:   0x2b - m!start
"
    );
    Ok(())
}

#[test]
#[cfg_attr(all(target_os = "macos", target_arch = "aarch64"), ignore)] // TODO #2808 system libunwind is broken on aarch64
#[cfg_attr(target_os = "openbsd", ignore)] // backtraces are broken on OpenBSD
fn present_after_module_drop() -> Result<()> {
    let mut store = Store::<()>::default();
    let module = Module::new(store.engine(), r#"(func (export "foo") unreachable)"#)?;
    let instance = Instance::new(&mut store, &module, &[])?;
    let func = instance.get_typed_func::<(), (), _>(&mut store, "foo")?;

    println!("asserting before we drop modules");
    assert_trap(func.call(&mut store, ()).unwrap_err());
    drop((instance, module));

    println!("asserting after drop");
    assert_trap(func.call(&mut store, ()).unwrap_err());
    return Ok(());

    fn assert_trap(t: Trap) {
        println!("{}", t);
        assert_eq!(t.trace().len(), 1);
        assert_eq!(t.trace()[0].func_index(), 0);
    }
}

fn assert_trap_code(wat: &str, code: wasmtime::TrapCode) {
    let mut store = Store::<()>::default();
    let module = Module::new(store.engine(), wat).unwrap();

    let err = match Instance::new(&mut store, &module, &[]) {
        Ok(_) => unreachable!(),
        Err(e) => e,
    };
    let trap = err.downcast_ref::<Trap>().unwrap();
    assert_eq!(trap.trap_code(), Some(code));
}

#[test]
fn heap_out_of_bounds_trap() {
    assert_trap_code(
        r#"
            (module
              (memory 0)
              (func $start (drop (i32.load (i32.const 1000000))))
              (start $start)
            )
         "#,
        TrapCode::MemoryOutOfBounds,
    );

    assert_trap_code(
        r#"
            (module
              (memory 0)
              (func $start (drop (i32.load memory.size)))
              (start $start)
            )
         "#,
        TrapCode::MemoryOutOfBounds,
    );
}

fn rustc(src: &str) -> Vec<u8> {
    let td = tempfile::TempDir::new().unwrap();
    let output = td.path().join("foo.wasm");
    let input = td.path().join("input.rs");
    std::fs::write(&input, src).unwrap();
    let result = Command::new("rustc")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .arg("--target")
        .arg("wasm32-wasi")
        .arg("-g")
        .output()
        .unwrap();
    if result.status.success() {
        return std::fs::read(&output).unwrap();
    }
    panic!(
        "rustc failed: {}\n{}",
        result.status,
        String::from_utf8_lossy(&result.stderr)
    );
}

#[test]
#[cfg_attr(all(target_os = "macos", target_arch = "aarch64"), ignore)] // TODO #2808 system libunwind is broken on aarch64
#[cfg_attr(target_os = "openbsd", ignore)] // backtraces are broken on OpenBSD
fn parse_dwarf_info() -> Result<()> {
    let wasm = rustc(
        "
            fn main() {
                panic!();
            }
        ",
    );
    let mut config = Config::new();
    config.wasm_backtrace_details(WasmBacktraceDetails::Enable);
    let engine = Engine::new(&config)?;
    let module = Module::new(&engine, &wasm)?;
    let mut linker = Linker::new(&engine);
    wasmtime_wasi::add_to_linker(&mut linker, |s| s)?;
    let mut store = Store::new(
        &engine,
        wasmtime_wasi::sync::WasiCtxBuilder::new()
            .inherit_stdio()
            .build(),
    );
    linker.module(&mut store, "", &module)?;
    let run = linker.get_default(&mut store, "")?;
    let trap = run.call(&mut store, &[]).unwrap_err().downcast::<Trap>()?;

    let mut found = false;
    for frame in trap.trace() {
        for symbol in frame.symbols() {
            if let Some(file) = symbol.file() {
                if file.ends_with("input.rs") {
                    found = true;
                    assert!(symbol.name().unwrap().contains("main"));
                    assert_eq!(symbol.line(), Some(3));
                }
            }
        }
    }
    assert!(found);
    Ok(())
}

#[test]
#[cfg_attr(all(target_os = "macos", target_arch = "aarch64"), ignore)] // TODO #2808 system libunwind is broken on aarch64
#[cfg_attr(target_os = "openbsd", ignore)] // backtraces are broken on OpenBSD
fn no_hint_even_with_dwarf_info() -> Result<()> {
    let mut config = Config::new();
    config.wasm_backtrace_details(WasmBacktraceDetails::Disable);
    let engine = Engine::new(&config)?;
    let mut store = Store::new(&engine, ());
    let module = Module::new(
        &engine,
        r#"
            (module
                (@custom ".debug_info" (after last) "")
                (func $start
                    unreachable)
                (start $start)
            )
        "#,
    )?;
    let trap = Instance::new(&mut store, &module, &[])
        .err()
        .unwrap()
        .downcast::<Trap>()?;
    assert_eq!(
        trap.to_string(),
        "\
wasm trap: unreachable
wasm backtrace:
    0:   0x1a - <unknown>!start
"
    );
    Ok(())
}

#[test]
#[cfg_attr(all(target_os = "macos", target_arch = "aarch64"), ignore)] // TODO #2808 system libunwind is broken on aarch64
#[cfg_attr(target_os = "openbsd", ignore)] // backtraces are broken on OpenBSD
fn hint_with_dwarf_info() -> Result<()> {
    // Skip this test if the env var is already configure, but in CI we're sure
    // to run tests without this env var configured.
    if std::env::var("WASMTIME_BACKTRACE_DETAILS").is_ok() {
        return Ok(());
    }
    let mut store = Store::<()>::default();
    let module = Module::new(
        store.engine(),
        r#"
            (module
                (@custom ".debug_info" (after last) "")
                (func $start
                    unreachable)
                (start $start)
            )
        "#,
    )?;
    let trap = Instance::new(&mut store, &module, &[])
        .err()
        .unwrap()
        .downcast::<Trap>()?;
    assert_eq!(
        trap.to_string(),
        "\
wasm trap: unreachable
wasm backtrace:
    0:   0x1a - <unknown>!start
note: run with `WASMTIME_BACKTRACE_DETAILS=1` environment variable to display more information
"
    );
    Ok(())
}

#[test]
fn multithreaded_traps() -> Result<()> {
    // Compile and run unreachable on a thread, then moves over the whole store to another thread,
    // and make sure traps are still correctly caught after notifying the store of the move.
    let mut store = Store::<()>::default();
    let module = Module::new(
        store.engine(),
        r#"(module (func (export "run") unreachable))"#,
    )?;
    let instance = Instance::new(&mut store, &module, &[])?;

    assert!(instance
        .get_typed_func::<(), (), _>(&mut store, "run")?
        .call(&mut store, ())
        .is_err());

    let handle = std::thread::spawn(move || {
        assert!(instance
            .get_typed_func::<(), (), _>(&mut store, "run")
            .unwrap()
            .call(&mut store, ())
            .is_err());
    });

    handle.join().expect("couldn't join thread");

    Ok(())
}
