use super::ref_types_module;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst};
use std::sync::Arc;
use wasmtime::*;

struct SetFlagOnDrop(Arc<AtomicBool>);

impl Drop for SetFlagOnDrop {
    fn drop(&mut self) {
        self.0.store(true, SeqCst);
    }
}

#[test]
fn smoke_test_gc() -> anyhow::Result<()> {
    let (mut store, module) = ref_types_module(
        r#"
            (module
                (import "" "" (func $do_gc))
                (func $recursive (export "func") (param i32 externref) (result externref)
                    local.get 0
                    i32.eqz
                    if (result externref)
                        call $do_gc
                        local.get 1
                    else
                        local.get 0
                        i32.const 1
                        i32.sub
                        local.get 1
                        call $recursive
                    end
                )
            )
        "#,
    )?;

    let do_gc = Func::wrap(&mut store, |mut caller: Caller<'_, _>| {
        // Do a GC with `externref`s on the stack in Wasm frames.
        caller.gc();
    });
    let instance = Instance::new(&mut store, &module, &[do_gc.into()])?;
    let func = instance.get_func(&mut store, "func").unwrap();

    let inner_dropped = Arc::new(AtomicBool::new(false));
    let r = ExternRef::new(SetFlagOnDrop(inner_dropped.clone()));
    {
        let args = [Val::I32(5), Val::ExternRef(Some(r.clone()))];
        func.call(&mut store, &args)?;
    }

    // Still held alive by the `VMExternRefActivationsTable` (potentially in
    // multiple slots within the table) and by this `r` local.
    assert!(r.strong_count() >= 2);

    // Doing a GC should see that there aren't any `externref`s on the stack in
    // Wasm frames anymore.
    store.gc();
    assert_eq!(r.strong_count(), 1);

    // Dropping `r` should drop the inner `SetFlagOnDrop` value.
    drop(r);
    assert!(inner_dropped.load(SeqCst));

    Ok(())
}

#[test]
fn wasm_dropping_refs() -> anyhow::Result<()> {
    let (mut store, module) = ref_types_module(
        r#"
            (module
                (func (export "drop_ref") (param externref)
                    nop
                )
            )
        "#,
    )?;

    let instance = Instance::new(&mut store, &module, &[])?;
    let drop_ref = instance.get_func(&mut store, "drop_ref").unwrap();

    let num_refs_dropped = Arc::new(AtomicUsize::new(0));

    // NB: 4096 is greater than the initial `VMExternRefActivationsTable`
    // capacity, so this will trigger at least one GC.
    for _ in 0..4096 {
        let r = ExternRef::new(CountDrops(num_refs_dropped.clone()));
        let args = [Val::ExternRef(Some(r))];
        drop_ref.call(&mut store, &args)?;
    }

    assert!(num_refs_dropped.load(SeqCst) > 0);

    // And after doing a final GC, all the refs should have been dropped.
    store.gc();
    assert_eq!(num_refs_dropped.load(SeqCst), 4096);

    return Ok(());

    struct CountDrops(Arc<AtomicUsize>);

    impl Drop for CountDrops {
        fn drop(&mut self) {
            self.0.fetch_add(1, SeqCst);
        }
    }
}

#[test]
fn many_live_refs() -> anyhow::Result<()> {
    let mut wat = r#"
        (module
            ;; Make new `externref`s.
            (import "" "make_ref" (func $make_ref (result externref)))

            ;; Observe an `externref` so it is kept live.
            (import "" "observe_ref" (func $observe_ref (param externref)))

            (func (export "many_live_refs")
    "#
    .to_string();

    // This is more than the initial `VMExternRefActivationsTable` capacity, so
    // it will need to allocate additional bump chunks.
    const NUM_LIVE_REFS: usize = 1024;

    // Push `externref`s onto the stack.
    for _ in 0..NUM_LIVE_REFS {
        wat.push_str("(call $make_ref)\n");
    }

    // Pop `externref`s from the stack. Because we pass each of them to a
    // function call here, they are all live references for the duration of
    // their lifetimes.
    for _ in 0..NUM_LIVE_REFS {
        wat.push_str("(call $observe_ref)\n");
    }

    wat.push_str(
        "
            ) ;; func
        ) ;; module
        ",
    );

    let (mut store, module) = ref_types_module(&wat)?;

    let live_refs = Arc::new(AtomicUsize::new(0));

    let make_ref = Func::wrap(&mut store, {
        let live_refs = live_refs.clone();
        move || Some(ExternRef::new(CountLiveRefs::new(live_refs.clone())))
    });

    let observe_ref = Func::wrap(&mut store, |r: Option<ExternRef>| {
        let r = r.unwrap();
        let r = r.data().downcast_ref::<CountLiveRefs>().unwrap();
        assert!(r.live_refs.load(SeqCst) > 0);
    });

    let instance = Instance::new(&mut store, &module, &[make_ref.into(), observe_ref.into()])?;
    let many_live_refs = instance.get_func(&mut store, "many_live_refs").unwrap();

    many_live_refs.call(&mut store, &[])?;

    store.gc();
    assert_eq!(live_refs.load(SeqCst), 0);

    return Ok(());

    struct CountLiveRefs {
        live_refs: Arc<AtomicUsize>,
    }

    impl CountLiveRefs {
        fn new(live_refs: Arc<AtomicUsize>) -> Self {
            live_refs.fetch_add(1, SeqCst);
            Self { live_refs }
        }
    }

    impl Drop for CountLiveRefs {
        fn drop(&mut self) {
            self.live_refs.fetch_sub(1, SeqCst);
        }
    }
}

#[test]
fn drop_externref_via_table_set() -> anyhow::Result<()> {
    let (mut store, module) = ref_types_module(
        r#"
            (module
                (table $t 1 externref)

                (func (export "table-set") (param externref)
                  (table.set $t (i32.const 0) (local.get 0))
                )
            )
        "#,
    )?;

    let instance = Instance::new(&mut store, &module, &[])?;
    let table_set = instance.get_func(&mut store, "table-set").unwrap();

    let foo_is_dropped = Arc::new(AtomicBool::new(false));
    let bar_is_dropped = Arc::new(AtomicBool::new(false));

    let foo = ExternRef::new(SetFlagOnDrop(foo_is_dropped.clone()));
    let bar = ExternRef::new(SetFlagOnDrop(bar_is_dropped.clone()));

    {
        let args = vec![Val::ExternRef(Some(foo))];
        table_set.call(&mut store, &args)?;
    }
    store.gc();
    assert!(!foo_is_dropped.load(SeqCst));
    assert!(!bar_is_dropped.load(SeqCst));

    {
        let args = vec![Val::ExternRef(Some(bar))];
        table_set.call(&mut store, &args)?;
    }
    store.gc();
    assert!(foo_is_dropped.load(SeqCst));
    assert!(!bar_is_dropped.load(SeqCst));

    table_set.call(&mut store, &[Val::ExternRef(None)])?;
    assert!(foo_is_dropped.load(SeqCst));
    assert!(bar_is_dropped.load(SeqCst));

    Ok(())
}

#[test]
fn global_drops_externref() -> anyhow::Result<()> {
    test_engine(&Engine::default())?;
    test_engine(&Engine::new(
        Config::new().allocation_strategy(InstanceAllocationStrategy::pooling()),
    )?)?;

    return Ok(());

    fn test_engine(engine: &Engine) -> anyhow::Result<()> {
        let mut store = Store::new(&engine, ());
        let flag = Arc::new(AtomicBool::new(false));
        let externref = ExternRef::new(SetFlagOnDrop(flag.clone()));
        Global::new(
            &mut store,
            GlobalType::new(ValType::ExternRef, Mutability::Const),
            externref.into(),
        )?;
        drop(store);
        assert!(flag.load(SeqCst));

        let mut store = Store::new(&engine, ());
        let module = Module::new(
            &engine,
            r#"
                (module
                    (global (mut externref) (ref.null extern))

                    (func (export "run") (param externref)
                        local.get 0
                        global.set 0
                    )
                )
            "#,
        )?;
        let instance = Instance::new(&mut store, &module, &[])?;
        let run = instance.get_typed_func::<Option<ExternRef>, (), _>(&mut store, "run")?;
        let flag = Arc::new(AtomicBool::new(false));
        let externref = ExternRef::new(SetFlagOnDrop(flag.clone()));
        run.call(&mut store, Some(externref))?;
        drop(store);
        assert!(flag.load(SeqCst));
        Ok(())
    }
}

#[test]
fn table_drops_externref() -> anyhow::Result<()> {
    test_engine(&Engine::default())?;
    test_engine(&Engine::new(
        Config::new().allocation_strategy(InstanceAllocationStrategy::pooling()),
    )?)?;

    return Ok(());

    fn test_engine(engine: &Engine) -> anyhow::Result<()> {
        let mut store = Store::new(&engine, ());
        let flag = Arc::new(AtomicBool::new(false));
        let externref = ExternRef::new(SetFlagOnDrop(flag.clone()));
        Table::new(
            &mut store,
            TableType::new(ValType::ExternRef, Limits::new(1, None)),
            externref.into(),
        )?;
        drop(store);
        assert!(flag.load(SeqCst));

        let mut store = Store::new(&engine, ());
        let module = Module::new(
            &engine,
            r#"
            (module
                (table 1 externref)

                (func (export "run") (param externref)
                    i32.const 0
                    local.get 0
                    table.set 0
                )
            )
        "#,
        )?;
        let instance = Instance::new(&mut store, &module, &[])?;
        let run = instance.get_typed_func::<Option<ExternRef>, (), _>(&mut store, "run")?;
        let flag = Arc::new(AtomicBool::new(false));
        let externref = ExternRef::new(SetFlagOnDrop(flag.clone()));
        run.call(&mut store, Some(externref))?;
        drop(store);
        assert!(flag.load(SeqCst));
        Ok(())
    }
}

#[test]
fn gee_i_sure_hope_refcounting_is_atomic() -> anyhow::Result<()> {
    let mut config = Config::new();
    config.wasm_reference_types(true);
    config.interruptable(true);
    let engine = Engine::new(&config)?;
    let mut store = Store::new(&engine, ());
    let module = Module::new(
        &engine,
        r#"
            (module
                (global (mut externref) (ref.null extern))
                (table 1 externref)

                (func (export "run") (param externref)
                    local.get 0
                    global.set 0
                    i32.const 0
                    local.get 0
                    table.set 0
                    loop
                        global.get 0
                        global.set 0

                        i32.const 0
                        i32.const 0
                        table.get
                        table.set

                        local.get 0
                        call $f

                        br 0
                    end
                )

                (func $f (param externref))
            )
        "#,
    )?;

    let instance = Instance::new(&mut store, &module, &[])?;
    let run = instance.get_typed_func::<Option<ExternRef>, (), _>(&mut store, "run")?;

    let flag = Arc::new(AtomicBool::new(false));
    let externref = ExternRef::new(SetFlagOnDrop(flag.clone()));
    let externref2 = externref.clone();
    let handle = store.interrupt_handle()?;

    let child = std::thread::spawn(move || run.call(&mut store, Some(externref2)));

    for _ in 0..10000 {
        drop(externref.clone());
    }
    handle.interrupt();

    assert!(child.join().unwrap().is_err());
    assert!(!flag.load(SeqCst));
    assert_eq!(externref.strong_count(), 1);
    drop(externref);
    assert!(flag.load(SeqCst));

    Ok(())
}

#[test]
fn global_init_no_leak() -> anyhow::Result<()> {
    let (mut store, module) = ref_types_module(
        r#"
            (module
                (import "" "" (global externref))
                (global externref (global.get 0))
            )
        "#,
    )?;

    let externref = ExternRef::new(());
    let global = Global::new(
        &mut store,
        GlobalType::new(ValType::ExternRef, Mutability::Const),
        externref.clone().into(),
    )?;
    Instance::new(&mut store, &module, &[global.into()])?;
    drop(store);
    assert_eq!(externref.strong_count(), 1);

    Ok(())
}
