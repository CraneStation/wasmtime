//! WebAssembly trap handling, which is built on top of the lower-level
//! signalhandling mechanisms.

use crate::VMContext;
use backtrace::Backtrace;
use std::any::Any;
use std::cell::{Cell, RefCell};
use std::error::Error;
use std::io;
use std::ptr;
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use std::sync::Once;
use wasmtime_environ::ir;

extern "C" {
    fn RegisterSetjmp(
        jmp_buf: *mut *const u8,
        callback: extern "C" fn(*mut u8),
        payload: *mut u8,
    ) -> i32;
    fn Unwind(jmp_buf: *const u8) -> !;
}

cfg_if::cfg_if! {
    if #[cfg(unix)] {
        use std::mem::{self, MaybeUninit};

        /// Function which may handle custom signals while processing traps.
        pub type SignalHandler<'a> = dyn Fn(libc::c_int, *const libc::siginfo_t, *const libc::c_void) -> bool + 'a;

        static mut PREV_SIGSEGV: MaybeUninit<libc::sigaction> = MaybeUninit::uninit();
        static mut PREV_SIGBUS: MaybeUninit<libc::sigaction> = MaybeUninit::uninit();
        static mut PREV_SIGILL: MaybeUninit<libc::sigaction> = MaybeUninit::uninit();
        static mut PREV_SIGFPE: MaybeUninit<libc::sigaction> = MaybeUninit::uninit();
        static mut PREV_SIGTRAP: MaybeUninit<libc::sigaction> = MaybeUninit::uninit();

        unsafe fn platform_init() {
            let register = |slot: &mut MaybeUninit<libc::sigaction>, signal: i32| {
                let mut handler: libc::sigaction = mem::zeroed();
                // The flags here are relatively careful, and they are...
                //
                // SA_SIGINFO gives us access to information like the program
                // counter from where the fault happened.
                //
                // SA_ONSTACK allows us to handle signals on an alternate stack,
                // so that the handler can run in response to running out of
                // stack space on the main stack. Rust installs an alternate
                // stack with sigaltstack, so we rely on that.
                //
                // SA_NODEFER allows us to reenter the signal handler if we
                // crash while handling the signal, and fall through to the
                // Breakpad handler by testing handlingSegFault.
                handler.sa_flags = libc::SA_SIGINFO | libc::SA_NODEFER | libc::SA_ONSTACK;
                handler.sa_sigaction = trap_handler as usize;
                libc::sigemptyset(&mut handler.sa_mask);
                if libc::sigaction(signal, &handler, slot.as_mut_ptr()) != 0 {
                    panic!(
                        "unable to install signal handler: {}",
                        io::Error::last_os_error(),
                    );
                }
            };

            // Allow handling OOB with signals on all architectures
            register(&mut PREV_SIGSEGV, libc::SIGSEGV);

            // Handle `unreachable` instructions which execute `ud2` right now
            register(&mut PREV_SIGILL, libc::SIGILL);

            register(&mut PREV_SIGTRAP, libc::SIGTRAP);

            // x86 uses SIGFPE to report division by zero
            if cfg!(target_arch = "x86") || cfg!(target_arch = "x86_64") {
                register(&mut PREV_SIGFPE, libc::SIGFPE);
            }

            // On ARM, handle Unaligned Accesses.
            // On Darwin, guard page accesses are raised as SIGBUS.
            if cfg!(target_arch = "arm") || cfg!(target_os = "macos") {
                register(&mut PREV_SIGBUS, libc::SIGBUS);
            }
        }

        unsafe extern "C" fn trap_handler(
            signum: libc::c_int,
            siginfo: *mut libc::siginfo_t,
            context: *mut libc::c_void,
        ) {
            let previous = match signum {
                libc::SIGSEGV => &PREV_SIGSEGV,
                libc::SIGBUS => &PREV_SIGBUS,
                libc::SIGFPE => &PREV_SIGFPE,
                libc::SIGILL => &PREV_SIGILL,
                libc::SIGTRAP => &PREV_SIGTRAP,
                _ => panic!("unknown signal: {}", signum),
            };
            let handled = tls::with(|info| {
                // If no wasm code is executing, we don't handle this as a wasm
                // trap.
                let info = match info {
                    Some(info) => info,
                    None => return false,
                };

                if signum == libc::SIGTRAP {
                    // Try handle breakpoints/stepping.
                    let dbg = info.debugger_context;
                    return match dbg {
                        Some(dbg) =>
                            crate::debugger::debugger_handler(dbg, siginfo, context),
                        None => false,
                    };
                }

                // If we hit an exception while handling a previous trap, that's
                // quite bad, so bail out and let the system handle this
                // recursive segfault.
                //
                // Otherwise flag ourselves as handling a trap, do the trap
                // handling, and reset our trap handling flag. Then we figure
                // out what to do based on the result of the trap handling.
                let jmp_buf = info.handle_trap(
                    get_pc(context),
                    |handler| handler(signum, siginfo, context),
                );

                // Figure out what to do based on the result of this handling of
                // the trap. Note that our sentinel value of 1 means that the
                // exception was handled by a custom exception handler, so we
                // keep executing.
                if jmp_buf.is_null() {
                    return false;
                } else if jmp_buf as usize == 1 {
                    return true;
                } else {
                    Unwind(jmp_buf)
                }
            });

            if handled {
                return;
            }

            // This signal is not for any compiled wasm code we expect, so we
            // need to forward the signal to the next handler. If there is no
            // next handler (SIG_IGN or SIG_DFL), then it's time to crash. To do
            // this, we set the signal back to its original disposition and
            // return. This will cause the faulting op to be re-executed which
            // will crash in the normal way. If there is a next handler, call
            // it. It will either crash synchronously, fix up the instruction
            // so that execution can continue and return, or trigger a crash by
            // returning the signal to it's original disposition and returning.
            let previous = &*previous.as_ptr();
            if previous.sa_flags & libc::SA_SIGINFO != 0 {
                mem::transmute::<
                    usize,
                    extern "C" fn(libc::c_int, *mut libc::siginfo_t, *mut libc::c_void),
                >(previous.sa_sigaction)(signum, siginfo, context)
            } else if previous.sa_sigaction == libc::SIG_DFL ||
                previous.sa_sigaction == libc::SIG_IGN
            {
                libc::sigaction(signum, previous, ptr::null_mut());
            } else {
                mem::transmute::<usize, extern "C" fn(libc::c_int)>(
                    previous.sa_sigaction
                )(signum)
            }
        }

        unsafe fn get_pc(cx: *mut libc::c_void) -> *const u8 {
            cfg_if::cfg_if! {
                if #[cfg(all(target_os = "linux", target_arch = "x86_64"))] {
                    let cx = &*(cx as *const libc::ucontext_t);
                    cx.uc_mcontext.gregs[libc::REG_RIP as usize] as *const u8
                } else if #[cfg(all(target_os = "linux", target_arch = "x86"))] {
                    let cx = &*(cx as *const libc::ucontext_t);
                    cx.uc_mcontext.gregs[libc::REG_EIP as usize] as *const u8
                } else if #[cfg(all(any(target_os = "linux", target_os = "android"), target_arch = "aarch64"))] {
                    let cx = &*(cx as *const libc::ucontext_t);
                    cx.uc_mcontext.pc as *const u8
                } else if #[cfg(target_os = "macos")] {
                    let cx = &*(cx as *const libc::ucontext_t);
                    (*cx.uc_mcontext).__ss.__rip as *const u8
                } else {
                    compile_error!("unsupported platform");
                }
            }
        }
    } else if #[cfg(target_os = "windows")] {
        use winapi::um::errhandlingapi::*;
        use winapi::um::winnt::*;
        use winapi::um::minwinbase::*;
        use winapi::vc::excpt::*;

        /// Function which may handle custom signals while processing traps.
        pub type SignalHandler<'a> = dyn Fn(winapi::um::winnt::PEXCEPTION_POINTERS) -> bool + 'a;

        unsafe fn platform_init() {
            // our trap handler needs to go first, so that we can recover from
            // wasm faults and continue execution, so pass `1` as a true value
            // here.
            if AddVectoredExceptionHandler(1, Some(exception_handler)).is_null() {
                panic!("failed to add exception handler: {}", io::Error::last_os_error());
            }
        }

        unsafe extern "system" fn exception_handler(
            exception_info: PEXCEPTION_POINTERS
        ) -> LONG {
            // Check the kind of exception, since we only handle a subset within
            // wasm code. If anything else happens we want to defer to whatever
            // the rest of the system wants to do for this exception.
            let record = &*(*exception_info).ExceptionRecord;
            if record.ExceptionCode != EXCEPTION_ACCESS_VIOLATION &&
                record.ExceptionCode != EXCEPTION_ILLEGAL_INSTRUCTION &&
                record.ExceptionCode != EXCEPTION_INT_DIVIDE_BY_ZERO &&
                record.ExceptionCode != EXCEPTION_INT_OVERFLOW
            {
                return EXCEPTION_CONTINUE_SEARCH;
            }

            // FIXME: this is what the previous C++ did to make sure that TLS
            // works by the time we execute this trap handling code. This isn't
            // exactly super easy to call from Rust though and it's not clear we
            // necessarily need to do so. Leaving this here in case we need this
            // in the future, but for now we can probably wait until we see a
            // strange fault before figuring out how to reimplement this in
            // Rust.
            //
            // if (!NtCurrentTeb()->Reserved1[sThreadLocalArrayPointerIndex]) {
            //     return EXCEPTION_CONTINUE_SEARCH;
            // }

            // This is basically the same as the unix version above, only with a
            // few parameters tweaked here and there.
            tls::with(|info| {
                let info = match info {
                    Some(info) => info,
                    None => return EXCEPTION_CONTINUE_SEARCH,
                };
                cfg_if::cfg_if! {
                    if #[cfg(target_arch = "x86_64")] {
                        let ip = (*(*exception_info).ContextRecord).Rip as *const u8;
                    } else if #[cfg(target_arch = "x86")] {
                        let ip = (*(*exception_info).ContextRecord).Eip as *const u8;
                    } else {
                        compile_error!("unsupported platform");
                    }
                }
                let jmp_buf = info.handle_trap(ip, |handler| handler(exception_info));
                if jmp_buf.is_null() {
                    EXCEPTION_CONTINUE_SEARCH
                } else if jmp_buf as usize == 1 {
                    EXCEPTION_CONTINUE_EXECUTION
                } else {
                    Unwind(jmp_buf)
                }
            })
        }
    }
}

/// This function performs the low-overhead signal handler initialization that
/// we want to do eagerly to ensure a more-deterministic global process state.
///
/// This is especially relevant for signal handlers since handler ordering
/// depends on installation order: the wasm signal handler must run *before*
/// the other crash handlers and since POSIX signal handlers work LIFO, this
/// function needs to be called at the end of the startup process, after other
/// handlers have been installed. This function can thus be called multiple
/// times, having no effect after the first call.
pub fn init_traps() {
    static INIT: Once = Once::new();
    INIT.call_once(real_init);
}

fn real_init() {
    unsafe {
        platform_init();
    }
}

/// Raises a user-defined trap immediately.
///
/// This function performs as-if a wasm trap was just executed, only the trap
/// has a dynamic payload associated with it which is user-provided. This trap
/// payload is then returned from `catch_traps` below.
///
/// # Safety
///
/// Only safe to call when wasm code is on the stack, aka `catch_traps` must
/// have been previously called. Additionally no Rust destructors can be on the
/// stack. They will be skipped and not executed.
pub unsafe fn raise_user_trap(data: Box<dyn Error + Send + Sync>) -> ! {
    tls::with(|info| info.unwrap().unwind_with(UnwindReason::UserTrap(data)))
}

/// Raises a trap from inside library code immediately.
///
/// This function performs as-if a wasm trap was just executed. This trap
/// payload is then returned from `catch_traps` below.
///
/// # Safety
///
/// Only safe to call when wasm code is on the stack, aka `catch_traps` must
/// have been previously called. Additionally no Rust destructors can be on the
/// stack. They will be skipped and not executed.
pub unsafe fn raise_lib_trap(trap: Trap) -> ! {
    tls::with(|info| info.unwrap().unwind_with(UnwindReason::LibTrap(trap)))
}

/// Carries a Rust panic across wasm code and resumes the panic on the other
/// side.
///
/// # Safety
///
/// Only safe to call when wasm code is on the stack, aka `catch_traps` must
/// have been previously called. Additionally no Rust destructors can be on the
/// stack. They will be skipped and not executed.
pub unsafe fn resume_panic(payload: Box<dyn Any + Send>) -> ! {
    tls::with(|info| info.unwrap().unwind_with(UnwindReason::Panic(payload)))
}

/// Stores trace message with backtrace.
#[derive(Debug)]
pub enum Trap {
    /// A user-raised trap through `raise_user_trap`.
    User(Box<dyn Error + Send + Sync>),

    /// A trap raised from jit code
    Jit {
        /// The program counter in JIT code where this trap happened.
        pc: usize,
        /// Native stack backtrace at the time the trap occurred
        backtrace: Backtrace,
        /// An indicator for whether this may have been a trap generated from an
        /// interrupt, used for switching what would otherwise be a stack
        /// overflow trap to be an interrupt trap.
        maybe_interrupted: bool,
    },

    /// A trap raised from a wasm libcall
    Wasm {
        /// Code of the trap.
        trap_code: ir::TrapCode,
        /// Native stack backtrace at the time the trap occurred
        backtrace: Backtrace,
    },

    /// A trap indicating that the runtime was unable to allocate sufficient memory.
    OOM {
        /// Native stack backtrace at the time the OOM occurred
        backtrace: Backtrace,
    },
}

impl Trap {
    /// Construct a new Wasm trap with the given source location and trap code.
    ///
    /// Internally saves a backtrace when constructed.
    pub fn wasm(trap_code: ir::TrapCode) -> Self {
        let backtrace = Backtrace::new_unresolved();
        Trap::Wasm {
            trap_code,
            backtrace,
        }
    }

    /// Construct a new OOM trap with the given source location and trap code.
    ///
    /// Internally saves a backtrace when constructed.
    pub fn oom() -> Self {
        let backtrace = Backtrace::new_unresolved();
        Trap::OOM { backtrace }
    }
}

type DebuggerContext = dyn crate::debugger::DebuggerContext + Send + Sync + 'static;

/// Catches any wasm traps that happen within the execution of `closure`,
/// returning them as a `Result`.
///
/// Highly unsafe since `closure` won't have any dtors run.
pub unsafe fn catch_traps<'a, F>(
    vmctx: *mut VMContext,
    max_wasm_stack: usize,
    is_wasm_code: impl Fn(usize) -> bool + 'a,
    signal_handler: Option<&'a SignalHandler>,
    debugger_context: Option<&'a DebuggerContext>,
    mut closure: F,
) -> Result<(), Trap>
where
    F: FnMut() + 'a,
{
    // Ensure that we have our sigaltstack installed.
    #[cfg(unix)]
    setup_unix_sigaltstack()?;

    return CallThreadState::new(vmctx, &is_wasm_code, signal_handler, debugger_context).with(
        max_wasm_stack,
        |cx| {
            RegisterSetjmp(
                cx.jmp_buf.as_ptr(),
                call_closure::<F>,
                &mut closure as *mut F as *mut u8,
            )
        },
    );

    extern "C" fn call_closure<F>(payload: *mut u8)
    where
        F: FnMut(),
    {
        unsafe { (*(payload as *mut F))() }
    }
}

/// Temporary state stored on the stack which is registered in the `tls` module
/// below for calls into wasm.
pub struct CallThreadState<'a, 'f> {
    unwind: Cell<UnwindReason>,
    jmp_buf: Cell<*const u8>,
    vmctx: *mut VMContext,
    handling_trap: Cell<bool>,
    is_wasm_code: &'f (dyn Fn(usize) -> bool + 'a),
    signal_handler: Option<&'a SignalHandler<'a>>,
    debugger_context: Option<&'a DebuggerContext>,
}

enum UnwindReason {
    None,
    Panic(Box<dyn Any + Send>),
    UserTrap(Box<dyn Error + Send + Sync>),
    LibTrap(Trap),
    JitTrap { backtrace: Backtrace, pc: usize },
}

impl<'a, 'f> CallThreadState<'a, 'f> {
    fn new(
        vmctx: *mut VMContext,
        is_wasm_code: &'f (dyn Fn(usize) -> bool + 'a),
        signal_handler: Option<&'a SignalHandler<'a>>,
        debugger_context: Option<&'a DebuggerContext>,
    ) -> CallThreadState<'a, 'f> {
        CallThreadState {
            unwind: Cell::new(UnwindReason::None),
            vmctx,
            jmp_buf: Cell::new(ptr::null()),
            handling_trap: Cell::new(false),
            is_wasm_code,
            signal_handler,
            debugger_context,
        }
    }

    fn with(
        self,
        max_wasm_stack: usize,
        closure: impl FnOnce(&CallThreadState) -> i32,
    ) -> Result<(), Trap> {
        let _reset = self.update_stack_limit(max_wasm_stack)?;
        let ret = tls::set(&self, || closure(&self));
        match self.unwind.replace(UnwindReason::None) {
            UnwindReason::None => {
                debug_assert_eq!(ret, 1);
                Ok(())
            }
            UnwindReason::UserTrap(data) => {
                debug_assert_eq!(ret, 0);
                Err(Trap::User(data))
            }
            UnwindReason::LibTrap(trap) => Err(trap),
            UnwindReason::JitTrap { backtrace, pc } => {
                debug_assert_eq!(ret, 0);
                let maybe_interrupted = unsafe {
                    (*self.vmctx).instance().interrupts.stack_limit.load(SeqCst)
                        == wasmtime_environ::INTERRUPTED
                };
                Err(Trap::Jit {
                    pc,
                    backtrace,
                    maybe_interrupted,
                })
            }
            UnwindReason::Panic(panic) => {
                debug_assert_eq!(ret, 0);
                std::panic::resume_unwind(panic)
            }
        }
    }

    /// Checks and/or initializes the wasm native call stack limit.
    ///
    /// This function will inspect the current state of the stack and calling
    /// context to determine which of three buckets we're in:
    ///
    /// 1. We are the first wasm call on the stack. This means that we need to
    ///    set up a stack limit where beyond which if the native wasm stack
    ///    pointer goes beyond forces a trap. For now we simply reserve an
    ///    arbitrary chunk of bytes (1 MB from roughly the current native stack
    ///    pointer). This logic will likely get tweaked over time.
    ///
    /// 2. We aren't the first wasm call on the stack. In this scenario the wasm
    ///    stack limit is already configured. This case of wasm -> host -> wasm
    ///    we assume that the native stack consumed by the host is accounted for
    ///    in the initial stack limit calculation. That means that in this
    ///    scenario we do nothing.
    ///
    /// 3. We were previously interrupted. In this case we consume the interrupt
    ///    here and return a trap, clearing the interrupt and allowing the next
    ///    wasm call to proceed.
    ///
    /// The return value here is a trap for case 3, a noop destructor in case 2,
    /// and a meaningful destructor in case 1
    ///
    /// For more information about interrupts and stack limits see
    /// `crates/environ/src/cranelift.rs`.
    ///
    /// Note that this function must be called with `self` on the stack, not the
    /// heap/etc.
    fn update_stack_limit(&self, max_wasm_stack: usize) -> Result<impl Drop + '_, Trap> {
        // Make an "educated guess" to figure out where the wasm sp value should
        // start trapping if it drops below.
        let wasm_stack_limit = self as *const _ as usize - max_wasm_stack;

        let interrupts = unsafe { &**(&*self.vmctx).instance().interrupts() };
        let reset_stack_limit = match interrupts.stack_limit.compare_exchange(
            usize::max_value(),
            wasm_stack_limit,
            SeqCst,
            SeqCst,
        ) {
            Ok(_) => {
                // We're the first wasm on the stack so we've now reserved the
                // `max_wasm_stack` bytes of native stack space for wasm.
                // Nothing left to do here now except reset back when we're
                // done.
                true
            }
            Err(n) if n == wasmtime_environ::INTERRUPTED => {
                // This means that an interrupt happened before we actually
                // called this function, which means that we're now
                // considered interrupted. Be sure to consume this interrupt
                // as part of this process too.
                interrupts.stack_limit.store(usize::max_value(), SeqCst);
                return Err(Trap::Wasm {
                    trap_code: ir::TrapCode::Interrupt,
                    backtrace: Backtrace::new_unresolved(),
                });
            }
            Err(_) => {
                // The stack limit was previously set by a previous wasm
                // call on the stack. We leave the original stack limit for
                // wasm in place in that case, and don't reset the stack
                // limit when we're done.
                false
            }
        };

        struct Reset<'a>(bool, &'a AtomicUsize);

        impl Drop for Reset<'_> {
            fn drop(&mut self) {
                if self.0 {
                    self.1.store(usize::max_value(), SeqCst);
                }
            }
        }

        Ok(Reset(reset_stack_limit, &interrupts.stack_limit))
    }

    fn unwind_with(&self, reason: UnwindReason) -> ! {
        self.unwind.replace(reason);
        unsafe {
            Unwind(self.jmp_buf.get());
        }
    }

    /// Trap handler using our thread-local state.
    ///
    /// * `pc` - the program counter the trap happened at
    /// * `call_handler` - a closure used to invoke the platform-specific
    ///   signal handler for each instance, if available.
    ///
    /// Attempts to handle the trap if it's a wasm trap. Returns a few
    /// different things:
    ///
    /// * null - the trap didn't look like a wasm trap and should continue as a
    ///   trap
    /// * 1 as a pointer - the trap was handled by a custom trap handler on an
    ///   instance, and the trap handler should quickly return.
    /// * a different pointer - a jmp_buf buffer to longjmp to, meaning that
    ///   the wasm trap was succesfully handled.
    fn handle_trap(
        &self,
        pc: *const u8,
        call_handler: impl Fn(&SignalHandler) -> bool,
    ) -> *const u8 {
        // If we hit a fault while handling a previous trap, that's quite bad,
        // so bail out and let the system handle this recursive segfault.
        //
        // Otherwise flag ourselves as handling a trap, do the trap handling,
        // and reset our trap handling flag.
        if self.handling_trap.replace(true) {
            return ptr::null();
        }
        let _reset = ResetCell(&self.handling_trap, false);

        // If we haven't even started to handle traps yet, bail out.
        if self.jmp_buf.get().is_null() {
            return ptr::null();
        }

        // First up see if any instance registered has a custom trap handler,
        // in which case run them all. If anything handles the trap then we
        // return that the trap was handled.
        if let Some(handler) = self.signal_handler {
            if call_handler(handler) {
                return 1 as *const _;
            }
        }

        // If this fault wasn't in wasm code, then it's not our problem
        if !(self.is_wasm_code)(pc as usize) {
            return ptr::null();
        }

        // TODO: stack overflow can happen at any random time (i.e. in malloc()
        // in memory.grow) and it's really hard to determine if the cause was
        // stack overflow and if it happened in WebAssembly module.
        //
        // So, let's assume that any untrusted code called from WebAssembly
        // doesn't trap. Then, if we have called some WebAssembly code, it
        // means the trap is stack overflow.
        if self.jmp_buf.get().is_null() {
            return ptr::null();
        }
        let backtrace = Backtrace::new_unresolved();
        self.unwind.replace(UnwindReason::JitTrap {
            backtrace,
            pc: pc as usize,
        });
        self.jmp_buf.get()
    }
}

struct ResetCell<'a, T: Copy>(&'a Cell<T>, T);

impl<T: Copy> Drop for ResetCell<'_, T> {
    fn drop(&mut self) {
        self.0.set(self.1);
    }
}

// A private inner module for managing the TLS state that we require across
// calls in wasm. The WebAssembly code is called from C++ and then a trap may
// happen which requires us to read some contextual state to figure out what to
// do with the trap. This `tls` module is used to persist that information from
// the caller to the trap site.
mod tls {
    use super::CallThreadState;
    use std::cell::Cell;
    use std::mem;
    use std::ptr;

    thread_local!(static PTR: Cell<*const CallThreadState<'static, 'static>> = Cell::new(ptr::null()));

    /// Configures thread local state such that for the duration of the
    /// execution of `closure` any call to `with` will yield `ptr`, unless this
    /// is recursively called again.
    pub fn set<R>(ptr: &CallThreadState<'_, '_>, closure: impl FnOnce() -> R) -> R {
        struct Reset<'a, T: Copy>(&'a Cell<T>, T);

        impl<T: Copy> Drop for Reset<'_, T> {
            fn drop(&mut self) {
                self.0.set(self.1);
            }
        }

        PTR.with(|p| {
            // Note that this extension of the lifetime to `'static` should be
            // safe because we only ever access it below with an anonymous
            // lifetime, meaning `'static` never leaks out of this module.
            let ptr = unsafe {
                mem::transmute::<
                    *const CallThreadState<'_, '_>,
                    *const CallThreadState<'static, 'static>,
                >(ptr)
            };
            let _r = Reset(p, p.replace(ptr));
            closure()
        })
    }

    /// Returns the last pointer configured with `set` above. Panics if `set`
    /// has not been previously called.
    pub fn with<R>(closure: impl FnOnce(Option<&CallThreadState<'_, '_>>) -> R) -> R {
        PTR.with(|ptr| {
            let p = ptr.get();
            unsafe { closure(if p.is_null() { None } else { Some(&*p) }) }
        })
    }
}

/// A module for registering a custom alternate signal stack (sigaltstack).
///
/// Rust's libstd installs an alternate stack with size `SIGSTKSZ`, which is not
/// always large enough for our signal handling code. Override it by creating
/// and registering our own alternate stack that is large enough and has a guard
/// page.
#[cfg(unix)]
fn setup_unix_sigaltstack() -> Result<(), Trap> {
    use std::convert::TryInto;
    use std::ptr::null_mut;

    thread_local! {
        /// Thread-local state is lazy-initialized on the first time it's used,
        /// and dropped when the thread exits.
        static TLS: RefCell<Tls> = RefCell::new(Tls::None);
    }

    /// The size of the sigaltstack (not including the guard, which will be
    /// added). Make this large enough to run our signal handlers.
    const MIN_STACK_SIZE: usize = 16 * 4096;

    enum Tls {
        None,
        Allocated {
            mmap_ptr: *mut libc::c_void,
            mmap_size: usize,
        },
        BigEnough,
    }

    return TLS.with(|slot| unsafe {
        let mut slot = slot.borrow_mut();
        match *slot {
            Tls::None => {}
            // already checked
            _ => return Ok(()),
        }

        // Check to see if the existing sigaltstack, if it exists, is big
        // enough. If so we don't need to allocate our own.
        let mut old_stack = mem::zeroed();
        let r = libc::sigaltstack(ptr::null(), &mut old_stack);
        assert_eq!(r, 0, "learning about sigaltstack failed");
        if old_stack.ss_flags & libc::SS_DISABLE == 0 && old_stack.ss_size >= MIN_STACK_SIZE {
            *slot = Tls::BigEnough;
            return Ok(());
        }

        // ... but failing that we need to allocate our own, so do all that
        // here.
        let page_size: usize = libc::sysconf(libc::_SC_PAGESIZE).try_into().unwrap();
        let guard_size = page_size;
        let alloc_size = guard_size + MIN_STACK_SIZE;

        let ptr = libc::mmap(
            null_mut(),
            alloc_size,
            libc::PROT_NONE,
            libc::MAP_PRIVATE | libc::MAP_ANON,
            -1,
            0,
        );
        if ptr == libc::MAP_FAILED {
            return Err(Trap::oom());
        }

        // Prepare the stack with readable/writable memory and then register it
        // with `sigaltstack`.
        let stack_ptr = (ptr as usize + guard_size) as *mut libc::c_void;
        let r = libc::mprotect(
            stack_ptr,
            MIN_STACK_SIZE,
            libc::PROT_READ | libc::PROT_WRITE,
        );
        assert_eq!(r, 0, "mprotect to configure memory for sigaltstack failed");
        let new_stack = libc::stack_t {
            ss_sp: stack_ptr,
            ss_flags: 0,
            ss_size: MIN_STACK_SIZE,
        };
        let r = libc::sigaltstack(&new_stack, ptr::null_mut());
        assert_eq!(r, 0, "registering new sigaltstack failed");

        *slot = Tls::Allocated {
            mmap_ptr: ptr,
            mmap_size: alloc_size,
        };
        Ok(())
    });

    impl Drop for Tls {
        fn drop(&mut self) {
            let (ptr, size) = match self {
                Tls::Allocated {
                    mmap_ptr,
                    mmap_size,
                } => (*mmap_ptr, *mmap_size),
                _ => return,
            };
            unsafe {
                // Deallocate the stack memory.
                let r = libc::munmap(ptr, size);
                debug_assert_eq!(r, 0, "munmap failed during thread shutdown");
            }
        }
    }
}
