use std::any::Any;
use std::cell::Cell;
use std::io;
use std::marker::PhantomData;
use std::panic::{self, AssertUnwindSafe};

#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as imp;

#[cfg(unix)]
mod unix;
#[cfg(unix)]
use unix as imp;

/// Represents an execution stack to use for a fiber.
#[derive(Debug)]
pub struct FiberStack(imp::FiberStack);

impl FiberStack {
    /// Creates a new fiber stack of the given size.
    pub fn new(size: usize) -> io::Result<Self> {
        Ok(Self(imp::FiberStack::new(size)?))
    }

    /// Creates a new fiber stack with the given pointer to the top of the stack.
    ///
    /// # Safety
    ///
    /// This is unsafe because there is no validation of the given stack pointer.
    ///
    /// The caller must properly allocate the stack space with a guard page and
    /// make the pages accessible for correct behavior.
    pub unsafe fn from_top_ptr(top: *mut u8) -> io::Result<Self> {
        Ok(Self(imp::FiberStack::from_top_ptr(top)?))
    }

    /// Gets the top of the stack.
    ///
    /// Returns `None` if the platform does not support getting the top of the stack.
    pub fn top(&self) -> Option<*mut u8> {
        self.0.top()
    }
}

pub struct Fiber<'a, Resume, Yield, Return> {
    stack: FiberStack,
    inner: imp::Fiber,
    done: Cell<bool>,
    _phantom: PhantomData<&'a (Resume, Yield, Return)>,
}

pub struct Suspend<Resume, Yield, Return> {
    inner: imp::Suspend,
    _phantom: PhantomData<(Resume, Yield, Return)>,
}

enum RunResult<Resume, Yield, Return> {
    Executing,
    Resuming(Resume),
    Yield(Yield),
    Returned(Return),
    Panicked(Box<dyn Any + Send>),
}

impl<'a, Resume, Yield, Return> Fiber<'a, Resume, Yield, Return> {
    /// Creates a new fiber which will execute `func` on the given stack.
    ///
    /// This function returns a `Fiber` which, when resumed, will execute `func`
    /// to completion. When desired the `func` can suspend itself via
    /// `Fiber::suspend`.
    pub fn new(
        stack: FiberStack,
        func: impl FnOnce(Resume, &Suspend<Resume, Yield, Return>) -> Return + 'a,
    ) -> Result<Self, (io::Error, FiberStack)> {
        let inner = match imp::Fiber::new(&stack.0, func) {
            Ok(inner) => inner,
            Err(e) => return Err((e, stack)),
        };

        Ok(Self {
            stack,
            inner,
            done: Cell::new(false),
            _phantom: PhantomData,
        })
    }

    /// Resumes execution of this fiber.
    ///
    /// This function will transfer execution to the fiber and resume from where
    /// it last left off.
    ///
    /// Returns `true` if the fiber finished or `false` if the fiber was
    /// suspended in the middle of execution.
    ///
    /// # Panics
    ///
    /// Panics if the current thread is already executing a fiber or if this
    /// fiber has already finished.
    ///
    /// Note that if the fiber itself panics during execution then the panic
    /// will be propagated to this caller.
    pub fn resume(&self, val: Resume) -> Result<Return, Yield> {
        assert!(!self.done.replace(true), "cannot resume a finished fiber");
        let result = Cell::new(RunResult::Resuming(val));
        self.inner.resume(&self.stack.0, &result);
        match result.into_inner() {
            RunResult::Resuming(_) | RunResult::Executing => unreachable!(),
            RunResult::Yield(y) => {
                self.done.set(false);
                Err(y)
            }
            RunResult::Returned(r) => Ok(r),
            RunResult::Panicked(payload) => std::panic::resume_unwind(payload),
        }
    }

    /// Returns whether this fiber has finished executing.
    pub fn done(&self) -> bool {
        self.done.get()
    }

    /// Gets the stack associated with this fiber.
    pub fn stack(&self) -> &FiberStack {
        &self.stack
    }
}

impl<Resume, Yield, Return> Suspend<Resume, Yield, Return> {
    /// Suspend execution of a currently running fiber.
    ///
    /// This function will switch control back to the original caller of
    /// `Fiber::resume`. This function will then return once the `Fiber::resume`
    /// function is called again.
    ///
    /// # Panics
    ///
    /// Panics if the current thread is not executing a fiber from this library.
    pub fn suspend(&self, value: Yield) -> Resume {
        self.inner
            .switch::<Resume, Yield, Return>(RunResult::Yield(value))
    }

    fn execute(
        inner: imp::Suspend,
        initial: Resume,
        func: impl FnOnce(Resume, &Suspend<Resume, Yield, Return>) -> Return,
    ) {
        let suspend = Suspend {
            inner,
            _phantom: PhantomData,
        };
        let result = panic::catch_unwind(AssertUnwindSafe(|| (func)(initial, &suspend)));
        suspend.inner.switch::<Resume, Yield, Return>(match result {
            Ok(result) => RunResult::Returned(result),
            Err(panic) => RunResult::Panicked(panic),
        });
    }
}

impl<A, B, C> Drop for Fiber<'_, A, B, C> {
    fn drop(&mut self) {
        debug_assert!(self.done.get(), "fiber dropped without finishing");
    }
}

#[cfg(test)]
mod tests {
    use super::{Fiber, FiberStack};
    use std::cell::Cell;
    use std::panic::{self, AssertUnwindSafe};
    use std::rc::Rc;

    #[test]
    fn small_stacks() {
        Fiber::<(), (), ()>::new(FiberStack::new(0).unwrap(), |_, _| {})
            .unwrap()
            .resume(())
            .unwrap();
        Fiber::<(), (), ()>::new(FiberStack::new(1).unwrap(), |_, _| {})
            .unwrap()
            .resume(())
            .unwrap();
    }

    #[test]
    fn smoke() {
        let hit = Rc::new(Cell::new(false));
        let hit2 = hit.clone();
        let fiber = Fiber::<(), (), ()>::new(FiberStack::new(1024 * 1024).unwrap(), move |_, _| {
            hit2.set(true);
        })
        .unwrap();
        assert!(!hit.get());
        fiber.resume(()).unwrap();
        assert!(hit.get());
    }

    #[test]
    fn suspend_and_resume() {
        let hit = Rc::new(Cell::new(false));
        let hit2 = hit.clone();
        let fiber = Fiber::<(), (), ()>::new(FiberStack::new(1024 * 1024).unwrap(), move |_, s| {
            s.suspend(());
            hit2.set(true);
            s.suspend(());
        })
        .unwrap();
        assert!(!hit.get());
        assert!(fiber.resume(()).is_err());
        assert!(!hit.get());
        assert!(fiber.resume(()).is_err());
        assert!(hit.get());
        assert!(fiber.resume(()).is_ok());
        assert!(hit.get());
    }

    #[test]
    fn backtrace_traces_to_host() {
        #[inline(never)] // try to get this to show up in backtraces
        fn look_for_me() {
            run_test();
        }
        fn assert_contains_host() {
            let trace = backtrace::Backtrace::new();
            println!("{:?}", trace);
            assert!(
                trace
                .frames()
                .iter()
                .flat_map(|f| f.symbols())
                .filter_map(|s| Some(s.name()?.to_string()))
                .any(|s| s.contains("look_for_me"))
                // TODO: apparently windows unwind routines don't unwind through fibers, so this will always fail. Is there a way we can fix that?
                || cfg!(windows)
            );
        }

        fn run_test() {
            let fiber =
                Fiber::<(), (), ()>::new(FiberStack::new(1024 * 1024).unwrap(), move |(), s| {
                    assert_contains_host();
                    s.suspend(());
                    assert_contains_host();
                    s.suspend(());
                    assert_contains_host();
                })
                .unwrap();
            assert!(fiber.resume(()).is_err());
            assert!(fiber.resume(()).is_err());
            assert!(fiber.resume(()).is_ok());
        }

        look_for_me();
    }

    #[test]
    fn panics_propagated() {
        let a = Rc::new(Cell::new(false));
        let b = SetOnDrop(a.clone());
        let fiber =
            Fiber::<(), (), ()>::new(FiberStack::new(1024 * 1024).unwrap(), move |(), _s| {
                drop(&b);
                panic!();
            })
            .unwrap();
        assert!(panic::catch_unwind(AssertUnwindSafe(|| fiber.resume(()))).is_err());
        assert!(a.get());

        struct SetOnDrop(Rc<Cell<bool>>);

        impl Drop for SetOnDrop {
            fn drop(&mut self) {
                self.0.set(true);
            }
        }
    }

    #[test]
    fn suspend_and_resume_values() {
        let fiber = Fiber::new(FiberStack::new(1024 * 1024).unwrap(), move |first, s| {
            assert_eq!(first, 2.0);
            assert_eq!(s.suspend(4), 3.0);
            "hello".to_string()
        })
        .unwrap();
        assert_eq!(fiber.resume(2.0), Err(4));
        assert_eq!(fiber.resume(3.0), Ok("hello".to_string()));
    }
}
