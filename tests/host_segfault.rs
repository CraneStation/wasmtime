// To handle out-of-bounds reads and writes we use segfaults right now. We only
// want to catch a subset of segfaults, however, rather than all segfaults
// happening everywhere. The purpose of this test is to ensure that we *don't*
// catch segfaults if it happens in a random place in the code, but we instead
// bail out of our segfault handler early.
//
// This is sort of hard to test for but the general idea here is that we confirm
// that execution made it to our `segfault` function by printing something, and
// then we also make sure that stderr is empty to confirm that no weird panics
// happened or anything like that.

use std::env;
use std::future::Future;
use std::io::{self, Write};
use std::pin::Pin;
use std::process::{Command, ExitStatus};
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
use wasmtime::*;

const VAR_NAME: &str = "__TEST_TO_RUN";
const CONFIRM: &str = "well at least we ran up to the crash";

fn segfault() -> ! {
    unsafe {
        println!("{}", CONFIRM);
        io::stdout().flush().unwrap();
        *(0x4 as *mut i32) = 3;
        unreachable!()
    }
}

fn allocate_stack_space() -> ! {
    let _a = [0u8; 1024];

    for _ in 0..100000 {
        allocate_stack_space();
    }

    unreachable!()
}

fn overrun_the_stack() -> ! {
    println!("{}", CONFIRM);
    io::stdout().flush().unwrap();
    allocate_stack_space();
}

fn run_future<F: Future>(future: F) -> F::Output {
    let mut f = Pin::from(Box::new(future));
    let waker = dummy_waker();
    let mut cx = Context::from_waker(&waker);
    loop {
        match f.as_mut().poll(&mut cx) {
            Poll::Ready(val) => break val,
            Poll::Pending => {}
        }
    }
}

fn dummy_waker() -> Waker {
    return unsafe { Waker::from_raw(clone(5 as *const _)) };

    unsafe fn clone(ptr: *const ()) -> RawWaker {
        assert_eq!(ptr as usize, 5);
        const VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, drop);
        RawWaker::new(ptr, &VTABLE)
    }

    unsafe fn wake(ptr: *const ()) {
        assert_eq!(ptr as usize, 5);
    }

    unsafe fn wake_by_ref(ptr: *const ()) {
        assert_eq!(ptr as usize, 5);
    }

    unsafe fn drop(ptr: *const ()) {
        assert_eq!(ptr as usize, 5);
    }
}

fn main() {
    // Skip this tests if it looks like we're in a cross-compiled situation and
    // we're emulating this test for a different platform. In that scenario
    // emulators (like QEMU) tend to not report signals the same way and such.
    if std::env::vars()
        .filter(|(k, _v)| k.starts_with("CARGO_TARGET") && k.ends_with("RUNNER"))
        .count()
        > 0
    {
        return;
    }

    let mut tests: Vec<(&str, fn(), bool)> = vec![
        ("normal segfault", || segfault(), false),
        (
            "make instance then segfault",
            || {
                let engine = Engine::default();
                let mut store = Store::new(&engine, ());
                let module = Module::new(&engine, "(module)").unwrap();
                let _instance = Instance::new(&mut store, &module, &[]).unwrap();
                segfault();
            },
            false,
        ),
        (
            "make instance then overrun the stack",
            || {
                let engine = Engine::default();
                let mut store = Store::new(&engine, ());
                let module = Module::new(&engine, "(module)").unwrap();
                let _instance = Instance::new(&mut store, &module, &[]).unwrap();
                overrun_the_stack();
            },
            true,
        ),
        (
            "segfault in a host function",
            || {
                let engine = Engine::default();
                let mut store = Store::new(&engine, ());
                let module = Module::new(&engine, r#"(import "" "" (func)) (start 0)"#).unwrap();
                let segfault = Func::wrap(&mut store, || segfault());
                Instance::new(&mut store, &module, &[segfault.into()]).unwrap();
                unreachable!();
            },
            false,
        ),
        (
            "hit async stack guard page",
            || {
                let mut config = Config::default();
                config.async_support(true);
                let engine = Engine::new(&config).unwrap();
                let mut store = Store::new(&engine, ());
                let f = Func::wrap0_async(&mut store, |_| {
                    Box::new(async {
                        overrun_the_stack();
                    })
                });
                run_future(f.call_async(&mut store, &[])).unwrap();
                unreachable!();
            },
            true,
        ),
    ];
    if !cfg!(target_os = "openbsd") {
        tests.push((
            "hit async stack guard page with pooling allocator",
            || {
                let mut config = Config::default();
                config.async_support(true);
                config.allocation_strategy(InstanceAllocationStrategy::pooling());
                let engine = Engine::new(&config).unwrap();
                let mut store = Store::new(&engine, ());
                let f = Func::wrap0_async(&mut store, |_| {
                    Box::new(async {
                        overrun_the_stack();
                    })
                });
                run_future(f.call_async(&mut store, &[])).unwrap();
                unreachable!();
            },
            true,
        ));
    }
    match env::var(VAR_NAME) {
        Ok(s) => {
            let test = tests
                .iter()
                .find(|p| p.0 == s)
                .expect("failed to find test")
                .1;
            test();
        }
        Err(_) => {
            for (name, _test, stack_overflow) in tests {
                run_test(name, stack_overflow);
            }
        }
    }
}

fn run_test(name: &str, stack_overflow: bool) {
    let me = env::current_exe().unwrap();
    let mut cmd = Command::new(me);
    cmd.env(VAR_NAME, name);
    let output = cmd.output().expect("failed to spawn subprocess");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let mut desc = format!("got status: {}", output.status);

    if !stdout.trim().is_empty() {
        desc.push_str("\nstdout: ----\n");
        desc.push_str("    ");
        desc.push_str(&stdout.replace("\n", "\n    "));
    }

    if !stderr.trim().is_empty() {
        desc.push_str("\nstderr: ----\n");
        desc.push_str("    ");
        desc.push_str(&stderr.replace("\n", "\n    "));
    }

    if stack_overflow {
        if is_stack_overflow(&output.status, &stderr) {
            assert!(
                stdout.trim().ends_with(CONFIRM),
                "failed to find confirmation in test `{}`\n{}",
                name,
                desc
            );
        } else {
            panic!("\n\nexpected a stack overflow on `{}`\n{}\n\n", name, desc);
        }
    } else {
        if is_segfault(&output.status) {
            assert!(
                stdout.trim().ends_with(CONFIRM) && stderr.is_empty(),
                "failed to find confirmation in test `{}`\n{}",
                name,
                desc
            );
        } else {
            panic!("\n\nexpected a segfault on `{}`\n{}\n\n", name, desc);
        }
    }
}

#[cfg(unix)]
fn is_segfault(status: &ExitStatus) -> bool {
    use std::os::unix::prelude::*;

    match status.signal() {
        Some(libc::SIGSEGV) => true,
        _ => false,
    }
}

#[cfg(unix)]
fn is_stack_overflow(status: &ExitStatus, stderr: &str) -> bool {
    use std::os::unix::prelude::*;

    // The main thread might overflow or it might be from a fiber stack (SIGSEGV/SIGBUS)
    stderr.contains("thread 'main' has overflowed its stack")
        || match status.signal() {
            Some(libc::SIGSEGV) | Some(libc::SIGBUS) => true,
            _ => false,
        }
}

#[cfg(windows)]
fn is_segfault(status: &ExitStatus) -> bool {
    match status.code().map(|s| s as u32) {
        Some(0xc0000005) => true,
        _ => false,
    }
}

#[cfg(windows)]
fn is_stack_overflow(status: &ExitStatus, _stderr: &str) -> bool {
    match status.code().map(|s| s as u32) {
        Some(0xc00000fd) => true,
        _ => false,
    }
}
