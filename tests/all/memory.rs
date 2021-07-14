use anyhow::Result;
use rayon::prelude::*;
use wasmtime::*;

fn module(engine: &Engine) -> Result<Module> {
    let mut wat = format!("(module\n");
    wat.push_str("(import \"\" \"\" (memory 0))\n");
    for i in 0..=33 {
        let offset = if i == 0 {
            0
        } else if i == 33 {
            !0
        } else {
            1u32 << (i - 1)
        };

        for (width, instr) in [
            (1, &["i32.load8_s"][..]),
            (2, &["i32.load16_s"]),
            (4, &["i32.load" /*, "f32.load"*/]),
            (8, &["i64.load" /*, "f64.load"*/]),
            #[cfg(not(target_arch = "s390x"))]
            (16, &["v128.load"]),
        ]
        .iter()
        {
            for (j, instr) in instr.iter().enumerate() {
                wat.push_str(&format!(
                    "(func (export \"{} {} v{}\") (param i32)\n",
                    width, offset, j
                ));
                wat.push_str("local.get 0\n");
                wat.push_str(instr);
                wat.push_str(&format!(" offset={}\n", offset));
                wat.push_str("drop\n)");
            }
        }
    }
    wat.push_str(")");
    Module::new(engine, &wat)
}

struct TestFunc {
    width: u32,
    offset: u32,
    func: TypedFunc<u32, ()>,
}

fn find_funcs(store: &mut Store<()>, instance: &Instance) -> Vec<TestFunc> {
    let list = instance
        .exports(&mut *store)
        .map(|export| {
            let name = export.name();
            let mut parts = name.split_whitespace();
            (
                parts.next().unwrap().parse().unwrap(),
                parts.next().unwrap().parse().unwrap(),
                export.into_func().unwrap(),
            )
        })
        .collect::<Vec<_>>();
    list.into_iter()
        .map(|(width, offset, func)| TestFunc {
            width,
            offset,
            func: func.typed(&store).unwrap(),
        })
        .collect()
}

fn test_traps(store: &mut Store<()>, funcs: &[TestFunc], addr: u32, mem: &Memory) {
    let mem_size = mem.data_size(&store) as u64;
    for func in funcs {
        let result = func.func.call(&mut *store, addr);
        let base = u64::from(func.offset) + u64::from(addr);
        let range = base..base + u64::from(func.width);
        if range.start >= mem_size || range.end >= mem_size {
            assert!(result.is_err());
        } else {
            assert!(result.is_ok());
        }
    }
}

#[test]
fn offsets_static_dynamic_oh_my() -> Result<()> {
    const GB: u64 = 1 << 30;

    let mut engines = Vec::new();
    let sizes = [0, 1 * GB, 4 * GB];
    for &static_memory_maximum_size in sizes.iter() {
        for &guard_size in sizes.iter() {
            for &guard_before_linear_memory in [true, false].iter() {
                let mut config = Config::new();
                config.wasm_simd(true);
                config.static_memory_maximum_size(static_memory_maximum_size);
                config.dynamic_memory_guard_size(guard_size);
                config.static_memory_guard_size(guard_size);
                config.guard_before_linear_memory(guard_before_linear_memory);
                engines.push(Engine::new(&config)?);
            }
        }
    }

    engines.par_iter().for_each(|engine| {
        let module = module(&engine).unwrap();

        for limits in [Limits::new(1, Some(2)), Limits::new(1, None)].iter() {
            let mut store = Store::new(&engine, ());
            let mem = Memory::new(&mut store, MemoryType::new(limits.clone())).unwrap();
            let instance = Instance::new(&mut store, &module, &[mem.into()]).unwrap();
            let funcs = find_funcs(&mut store, &instance);

            test_traps(&mut store, &funcs, 0, &mem);
            test_traps(&mut store, &funcs, 65536, &mem);
            test_traps(&mut store, &funcs, u32::MAX, &mem);

            mem.grow(&mut store, 1).unwrap();

            test_traps(&mut store, &funcs, 0, &mem);
            test_traps(&mut store, &funcs, 65536, &mem);
            test_traps(&mut store, &funcs, u32::MAX, &mem);
        }
    });

    Ok(())
}

#[test]
fn guards_present() -> Result<()> {
    const GUARD_SIZE: u64 = 65536;

    let mut config = Config::new();
    config.static_memory_maximum_size(1 << 20);
    config.dynamic_memory_guard_size(GUARD_SIZE);
    config.static_memory_guard_size(GUARD_SIZE);
    config.guard_before_linear_memory(true);
    let engine = Engine::new(&config)?;
    let mut store = Store::new(&engine, ());
    let static_mem = Memory::new(&mut store, MemoryType::new(Limits::new(1, Some(2))))?;
    let dynamic_mem = Memory::new(&mut store, MemoryType::new(Limits::new(1, None)))?;

    let assert_guards = |store: &Store<()>| unsafe {
        // guards before
        println!("check pre-static-mem");
        assert_faults(static_mem.data_ptr(&store).offset(-(GUARD_SIZE as isize)));
        println!("check pre-dynamic-mem");
        assert_faults(dynamic_mem.data_ptr(&store).offset(-(GUARD_SIZE as isize)));

        // guards after
        println!("check post-static-mem");
        assert_faults(
            static_mem
                .data_ptr(&store)
                .add(static_mem.data_size(&store)),
        );
        println!("check post-dynamic-mem");
        assert_faults(
            dynamic_mem
                .data_ptr(&store)
                .add(dynamic_mem.data_size(&store)),
        );
    };
    assert_guards(&store);
    // static memory should start with the second page unmapped
    unsafe {
        assert_faults(static_mem.data_ptr(&store).add(65536));
    }
    println!("growing");
    static_mem.grow(&mut store, 1).unwrap();
    dynamic_mem.grow(&mut store, 1).unwrap();
    assert_guards(&store);

    Ok(())
}

#[test]
#[cfg(not(target_os = "openbsd"))]
fn guards_present_pooling() -> Result<()> {
    const GUARD_SIZE: u64 = 65536;

    let mut config = Config::new();
    config.static_memory_maximum_size(1 << 20);
    config.dynamic_memory_guard_size(GUARD_SIZE);
    config.static_memory_guard_size(GUARD_SIZE);
    config.guard_before_linear_memory(true);
    config.allocation_strategy(InstanceAllocationStrategy::Pooling {
        strategy: PoolingAllocationStrategy::default(),
        module_limits: ModuleLimits {
            memory_pages: 10,
            ..ModuleLimits::default()
        },
        instance_limits: InstanceLimits { count: 2 },
    });
    let engine = Engine::new(&config)?;

    let mut store = Store::new(&engine, ());

    let mem1 = {
        let m = Module::new(&engine, "(module (memory (export \"\") 1 2))")?;
        Instance::new(&mut store, &m, &[])?
            .get_memory(&mut store, "")
            .unwrap()
    };
    let mem2 = {
        let m = Module::new(&engine, "(module (memory (export \"\") 1))")?;
        Instance::new(&mut store, &m, &[])?
            .get_memory(&mut store, "")
            .unwrap()
    };

    unsafe fn assert_guards(store: &Store<()>, mem: &Memory) {
        // guards before
        println!("check pre-mem");
        assert_faults(mem.data_ptr(&store).offset(-(GUARD_SIZE as isize)));

        // unmapped just after memory
        println!("check mem");
        assert_faults(mem.data_ptr(&store).add(mem.data_size(&store)));

        // guards after memory
        println!("check post-mem");
        assert_faults(mem.data_ptr(&store).add(1 << 20));
    }
    unsafe {
        assert_guards(&store, &mem1);
        assert_guards(&store, &mem2);
        println!("growing");
        mem1.grow(&mut store, 1).unwrap();
        mem2.grow(&mut store, 1).unwrap();
        assert_guards(&store, &mem1);
        assert_guards(&store, &mem2);
    }

    Ok(())
}

unsafe fn assert_faults(ptr: *mut u8) {
    use std::io::Error;
    #[cfg(unix)]
    {
        // I think things get real weird with uffd since there's a helper thread
        // that's not cloned with `fork` below. Just skip this test for uffd
        // since it's covered by tests elsewhere.
        if cfg!(target_os = "linux") && cfg!(feature = "uffd") {
            return;
        }
        // There's probably a faster way to do this here, but, uh, when in rome?
        match libc::fork() {
            0 => {
                *ptr = 4;
                std::process::exit(0);
            }
            -1 => panic!("failed to fork: {}", Error::last_os_error()),
            n => {
                let mut status = 0;
                assert!(
                    libc::waitpid(n, &mut status, 0) == n,
                    "failed to wait: {}",
                    Error::last_os_error()
                );
                assert!(libc::WIFSIGNALED(status));
            }
        }
    }
    #[cfg(windows)]
    {
        use winapi::um::memoryapi::*;
        use winapi::um::winnt::*;

        let mut info = std::mem::MaybeUninit::uninit();
        let r = VirtualQuery(
            ptr as *const _,
            info.as_mut_ptr(),
            std::mem::size_of_val(&info),
        );
        if r == 0 {
            panic!("failed to VirtualAlloc: {}", Error::last_os_error());
        }
        let info = info.assume_init();
        assert_eq!(info.AllocationProtect, PAGE_NOACCESS);
    }
}
