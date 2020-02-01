use anyhow::Result;
use std::panic::{self, AssertUnwindSafe};
use std::rc::Rc;
use wasmtime::*;

#[test]
fn test_trap_return() -> Result<()> {
    struct HelloCallback;

    impl Callable for HelloCallback {
        fn call(&self, _params: &[Val], _results: &mut [Val]) -> Result<(), Trap> {
            Err(Trap::new("test 123"))
        }
    }

    let store = Store::default();
    let wat = r#"
        (module
        (func $hello (import "" "hello"))
        (func (export "run") (call $hello))
        )
    "#;

    let module = Module::new(&store, wat)?;
    let hello_type = FuncType::new(Box::new([]), Box::new([]));
    let hello_func = Func::new(&store, hello_type, Rc::new(HelloCallback));

    let instance = Instance::new(&module, &[hello_func.into()])?;
    let run_func = instance.exports()[0]
        .func()
        .expect("expected function export");

    let e = run_func.call(&[]).err().expect("error calling function");

    assert_eq!(e.message(), "test 123");

    Ok(())
}

#[test]
fn test_trap_trace() -> Result<()> {
    let store = Store::default();
    let wat = r#"
        (module $hello_mod
            (func (export "run") (call $hello))
            (func $hello (unreachable))
        )
    "#;

    let module = Module::new(&store, wat)?;
    let instance = Instance::new(&module, &[])?;
    let run_func = instance.exports()[0]
        .func()
        .expect("expected function export");

    let e = run_func.call(&[]).err().expect("error calling function");

    let trace = e.trace();
    assert_eq!(trace.len(), 2);
    assert_eq!(trace[0].module_name().unwrap(), "hello_mod");
    assert_eq!(trace[0].func_index(), 1);
    assert_eq!(trace[0].func_name(), Some("hello"));
    assert_eq!(trace[1].module_name().unwrap(), "hello_mod");
    assert_eq!(trace[1].func_index(), 0);
    assert_eq!(trace[1].func_name(), None);
    assert!(e.message().contains("unreachable"));

    Ok(())
}

#[test]
fn test_trap_trace_cb() -> Result<()> {
    struct ThrowCallback;

    impl Callable for ThrowCallback {
        fn call(&self, _params: &[Val], _results: &mut [Val]) -> Result<(), Trap> {
            Err(Trap::new("cb throw"))
        }
    }

    let store = Store::default();
    let wat = r#"
        (module $hello_mod
            (import "" "throw" (func $throw))
            (func (export "run") (call $hello))
            (func $hello (call $throw))
        )
    "#;

    let fn_type = FuncType::new(Box::new([]), Box::new([]));
    let fn_func = Func::new(&store, fn_type, Rc::new(ThrowCallback));

    let module = Module::new(&store, wat)?;
    let instance = Instance::new(&module, &[fn_func.into()])?;
    let run_func = instance.exports()[0]
        .func()
        .expect("expected function export");

    let e = run_func.call(&[]).err().expect("error calling function");

    let trace = e.trace();
    assert_eq!(trace.len(), 2);
    assert_eq!(trace[0].module_name().unwrap(), "hello_mod");
    assert_eq!(trace[0].func_index(), 2);
    assert_eq!(trace[1].module_name().unwrap(), "hello_mod");
    assert_eq!(trace[1].func_index(), 1);
    assert_eq!(e.message(), "cb throw");

    Ok(())
}

#[test]
fn test_trap_stack_overflow() -> Result<()> {
    let store = Store::default();
    let wat = r#"
        (module $rec_mod
            (func $run (export "run") (call $run))
        )
    "#;

    let module = Module::new(&store, wat)?;
    let instance = Instance::new(&module, &[])?;
    let run_func = instance.exports()[0]
        .func()
        .expect("expected function export");

    let e = run_func.call(&[]).err().expect("error calling function");

    let trace = e.trace();
    assert!(trace.len() >= 32);
    for i in 0..trace.len() {
        assert_eq!(trace[i].module_name().unwrap(), "rec_mod");
        assert_eq!(trace[i].func_index(), 0);
        assert_eq!(trace[i].func_name(), Some("run"));
    }
    assert!(e.message().contains("call stack exhausted"));

    Ok(())
}

#[test]
fn trap_display_pretty() -> Result<()> {
    let store = Store::default();
    let wat = r#"
        (module $m
            (func $die unreachable)
            (func call $die)
            (func $foo call 1)
            (func (export "bar") call $foo)
        )
    "#;

    let module = Module::new(&store, wat)?;
    let instance = Instance::new(&module, &[])?;
    let run_func = instance.exports()[0]
        .func()
        .expect("expected function export");

    let e = run_func.call(&[]).err().expect("error calling function");
    assert_eq!(
        e.to_string(),
        "\
wasm trap: unreachable, source location: @0023
wasm backtrace:
  0: m!die
  1: m!<wasm function 1>
  2: m!foo
  3: m!<wasm function 3>
"
    );
    Ok(())
}

#[test]
fn trap_display_multi_module() -> Result<()> {
    let store = Store::default();
    let wat = r#"
        (module $a
            (func $die unreachable)
            (func call $die)
            (func $foo call 1)
            (func (export "bar") call $foo)
        )
    "#;

    let module = Module::new(&store, wat)?;
    let instance = Instance::new(&module, &[])?;
    let bar = instance.exports()[0].clone();

    let wat = r#"
        (module $b
            (import "" "" (func $bar))
            (func $middle call $bar)
            (func (export "bar2") call $middle)
        )
    "#;
    let module = Module::new(&store, wat)?;
    let instance = Instance::new(&module, &[bar])?;
    let bar2 = instance.exports()[0]
        .func()
        .expect("expected function export");

    let e = bar2.call(&[]).err().expect("error calling function");
    assert_eq!(
        e.to_string(),
        "\
wasm trap: unreachable, source location: @0023
wasm backtrace:
  0: a!die
  1: a!<wasm function 1>
  2: a!foo
  3: a!<wasm function 3>
  4: b!middle
  5: b!<wasm function 2>
"
    );
    Ok(())
}

#[test]
fn trap_start_function_import() -> Result<()> {
    struct ReturnTrap;

    impl Callable for ReturnTrap {
        fn call(&self, _params: &[Val], _results: &mut [Val]) -> Result<(), Trap> {
            Err(Trap::new("user trap"))
        }
    }

    let store = Store::default();
    let binary = wat::parse_str(
        r#"
            (module $a
                (import "" "" (func $foo))
                (start $foo)
            )
        "#,
    )?;

    let module = Module::new(&store, &binary)?;
    let sig = FuncType::new(Box::new([]), Box::new([]));
    let func = Func::new(&store, sig, Rc::new(ReturnTrap));
    let err = Instance::new(&module, &[func.into()]).err().unwrap();
    assert_eq!(err.downcast_ref::<Trap>().unwrap().message(), "user trap");
    Ok(())
}

#[test]
fn rust_panic_import() -> Result<()> {
    struct Panic;

    impl Callable for Panic {
        fn call(&self, _params: &[Val], _results: &mut [Val]) -> Result<(), Trap> {
            panic!("this is a panic");
        }
    }

    let store = Store::default();
    let binary = wat::parse_str(
        r#"
            (module $a
                (import "" "" (func $foo))
                (func (export "foo") call $foo)
            )
        "#,
    )?;

    let module = Module::new(&store, &binary)?;
    let sig = FuncType::new(Box::new([]), Box::new([]));
    let func = Func::new(&store, sig, Rc::new(Panic));
    let instance = Instance::new(&module, &[func.into()])?;
    let func = instance.exports()[0].func().unwrap().clone();
    let err = panic::catch_unwind(AssertUnwindSafe(|| {
        drop(func.call(&[]));
    }))
    .unwrap_err();
    assert_eq!(err.downcast_ref::<&'static str>(), Some(&"this is a panic"));
    Ok(())
}

#[test]
fn rust_panic_start_function() -> Result<()> {
    struct Panic;

    impl Callable for Panic {
        fn call(&self, _params: &[Val], _results: &mut [Val]) -> Result<(), Trap> {
            panic!("this is a panic");
        }
    }

    let store = Store::default();
    let binary = wat::parse_str(
        r#"
            (module $a
                (import "" "" (func $foo))
                (start $foo)
            )
        "#,
    )?;

    let module = Module::new(&store, &binary)?;
    let sig = FuncType::new(Box::new([]), Box::new([]));
    let func = Func::new(&store, sig, Rc::new(Panic));
    let err = panic::catch_unwind(AssertUnwindSafe(|| {
        drop(Instance::new(&module, &[func.into()]));
    }))
    .unwrap_err();
    assert_eq!(err.downcast_ref::<&'static str>(), Some(&"this is a panic"));
    Ok(())
}

#[test]
fn mismatched_arguments() -> Result<()> {
    let store = Store::default();
    let binary = wat::parse_str(
        r#"
            (module $a
                (func (export "foo") (param i32))
            )
        "#,
    )?;

    let module = Module::new(&store, &binary)?;
    let instance = Instance::new(&module, &[])?;
    let func = instance.exports()[0].func().unwrap().clone();
    assert_eq!(
        func.call(&[]).unwrap_err().message(),
        "expected 1 arguments, got 0"
    );
    assert_eq!(
        func.call(&[Val::F32(0)]).unwrap_err().message(),
        "argument type mismatch",
    );
    assert_eq!(
        func.call(&[Val::I32(0), Val::I32(1)])
            .unwrap_err()
            .message(),
        "expected 1 arguments, got 2"
    );
    Ok(())
}
