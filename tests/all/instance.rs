use anyhow::Result;
use wasmtime::*;

#[test]
fn wrong_import_numbers() -> Result<()> {
    let mut store = Store::<()>::default();
    let module = Module::new(store.engine(), r#"(module (import "" "" (func)))"#)?;

    assert!(Instance::new(&mut store, &module, &[]).is_err());
    let func = Func::wrap(&mut store, || {});
    assert!(Instance::new(&mut store, &module, &[func.clone().into(), func.into()]).is_err());
    Ok(())
}

#[test]
fn initializes_linear_memory() -> Result<()> {
    // Test for https://github.com/bytecodealliance/wasmtime/issues/2784
    let wat = r#"
        (module
            (memory (export "memory") 2)
            (data (i32.const 0) "Hello World!")
        )"#;
    let module = Module::new(&Engine::default(), wat)?;

    let mut store = Store::new(module.engine(), ());
    let instance = Instance::new(&mut store, &module, &[])?;
    let memory = instance.get_memory(&mut store, "memory").unwrap();

    let mut bytes = [0; 12];
    memory.read(&store, 0, &mut bytes)?;
    assert_eq!(bytes, "Hello World!".as_bytes());
    Ok(())
}

#[test]
#[cfg(not(target_os = "openbsd"))]
fn linear_memory_limits() -> Result<()> {
    // this test will allocate 4GB of virtual memory space, and may not work in
    // situations like CI QEMU emulation where it triggers SIGKILL.
    if std::env::var("WASMTIME_TEST_NO_HOG_MEMORY").is_ok() {
        return Ok(());
    }
    test(&Engine::default())?;
    test(&Engine::new(Config::new().allocation_strategy(
        InstanceAllocationStrategy::Pooling {
            strategy: PoolingAllocationStrategy::NextAvailable,
            module_limits: ModuleLimits {
                memory_pages: 65536,
                ..ModuleLimits::default()
            },
            instance_limits: InstanceLimits::default(),
        },
    ))?)?;
    return Ok(());

    fn test(engine: &Engine) -> Result<()> {
        let wat = r#"
        (module
            (memory 65534)

            (func (export "foo")  (result i32)
                i32.const 1
                memory.grow)
        )
    "#;
        let module = Module::new(engine, wat)?;

        let mut store = Store::new(engine, ());
        let instance = Instance::new(&mut store, &module, &[])?;
        let foo = instance.get_typed_func::<(), i32, _>(&mut store, "foo")?;

        assert_eq!(foo.call(&mut store, ())?, 65534);
        assert_eq!(foo.call(&mut store, ())?, -1);
        Ok(())
    }
}
