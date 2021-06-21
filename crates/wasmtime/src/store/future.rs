use crate::store::{StoreInner, StoreOpaque};
use crate::{AsContext, AsContextMut, StoreContext, StoreContextMut, Trap};
use std::cell::UnsafeCell;
use std::future::Future;
use std::pin::Pin;
use std::ptr;
use std::task::{Context, Poll};

type FiberResume = Result<(*mut u8, StoreOpaque<'static>), Trap>;
type FiberResult = ();

pub struct AsyncState {
    current_suspend: UnsafeCell<*const wasmtime_fiber::Suspend<FiberResume, (), FiberResult>>,
    current_poll_cx: UnsafeCell<*mut Context<'static>>,
}

// Lots of pesky unsafe cells and pointers in this structure. This means we need
// to declare explicitly that we use this in a threadsafe fashion.
unsafe impl Send for AsyncState {}
unsafe impl Sync for AsyncState {}

impl AsyncState {
    pub fn new() -> AsyncState {
        AsyncState {
            current_suspend: UnsafeCell::new(ptr::null()),
            current_poll_cx: UnsafeCell::new(ptr::null_mut()),
        }
    }
}

impl<T> StoreInner<T> {
    #[inline]
    pub fn async_cx(&self) -> AsyncCx {
        debug_assert!(self.async_support());
        AsyncCx {
            current_suspend: self.async_state.current_suspend.get(),
            current_poll_cx: self.async_state.current_poll_cx.get(),
        }
    }

    /// Yields execution to the caller on out-of-gas
    ///
    /// This only works on async futures and stores, and assumes that we're
    /// executing on a fiber. This will yield execution back to the caller once
    /// and when we come back we'll continue with `fuel_to_inject` more fuel.
    pub fn out_of_gas_yield(&mut self, fuel_to_inject: u64) -> Result<(), Trap> {
        // Small future that yields once and then returns ()
        #[derive(Default)]
        struct Yield {
            yielded: bool,
        }

        impl Future for Yield {
            type Output = ();

            fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
                if self.yielded {
                    Poll::Ready(())
                } else {
                    // Flag ourselves as yielded to return next time, and also
                    // flag the waker that we're already ready to get
                    // re-enqueued for another poll.
                    self.yielded = true;
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
            }
        }

        let mut future = Yield::default();
        let result = unsafe { self.async_cx().block_on(Pin::new_unchecked(&mut future)) };
        match result {
            // If this finished successfully then we were resumed normally via a
            // `poll`, so inject some more fuel and keep going.
            Ok(()) => {
                self.add_fuel(fuel_to_inject).unwrap();
                Ok(())
            }
            // If the future was dropped while we were yielded, then we need to
            // clean up this fiber. Do so by raising a trap which will abort all
            // wasm and get caught on the other side to clean things up.
            Err(trap) => Err(trap),
        }
    }
}

/// A "trait alias" for futures returned by [`Func::call_async`] and
/// [`TypedFunc::call_async`].
///
/// This trait represents the return value of those two asynchronous functions,
/// and indicates that the return value implements both the standard [`Future`]
/// trait as well as the [`AsContextMut`] trait. This enables callers to get
/// access to the underlying store context while the future is not running.
///
/// [`Func::call_async`]: crate::Func::call_async
/// [`TypedFunc::call_async`]: crate::TypedFunc::call_async
pub trait WasmtimeFuture: AsContextMut + Future {}

impl<T> WasmtimeFuture for T where T: AsContextMut + Future {}

/// Executes a synchronous computation `func` asynchronously on a new fiber.
///
/// This function will convert the synchronous `func` into an asynchronous
/// future. This is done by running `func` in a fiber on a separate native
/// stack which can be suspended and resumed from.
///
/// Most of the nitty-gritty here is how we juggle the various contexts
/// necessary to suspend the fiber later on and poll sub-futures. It's hoped
/// that the various comments are illuminating as to what's going on here.
pub fn on_fiber<'func, T, R>(
    mut store: T,
    // note that this `Send` bound is important for the `unsafe impl Send` below.
    func: impl FnOnce(&mut StoreOpaque<'_>) -> R + Send + 'func,
) -> impl WasmtimeFuture<Data = T::Data, Output = R> + 'func
where
    T: AsContextMut + 'func,
    R: FromError + 'func,
    // While this send bound isn't strictly required for correctness it's a
    // "better safe than sorry" kind of situation. Wasmtime host functions are
    // required to return `Send` futures, so basically nothing works with
    // non-`Send` state anyway.
    T::Data: Send,
{
    let cx = store.as_context_mut();
    // Sanity checks
    debug_assert!(cx.0.async_support());
    debug_assert!(cx.engine().config().async_stack_size > 0);

    // Allocate a fiber stack from the engine's allocator, which may do
    // something pooling if configured or otherwise allocates on-demand for now.
    // Note that this probably wants optimization in the future for the
    // on-demand allocation strategy.
    let stack = match cx.engine().allocator().allocate_fiber_stack() {
        Ok(stack) => stack,
        Err(e) => {
            return FiberFuture {
                state: State::Failed(Some(e.into())),
                store,
            }
        }
    };

    // Create the fiber that will execute on a separate stack. The closure
    // provided here is what runs on the separate stack, and it closes over the
    // `func` input above. Note that its resumption type, `FiberResume`,
    // contains most of the information it needs to keep running (like the store
    // context).
    //
    // Note that it's important that the type of the fiber's closure is erased
    // as we store the fiber. This allows communication with the suspension
    // points which don't know the type of return value or closure here, they
    // only have a general store.
    let fiber = wasmtime_fiber::Fiber::new(stack, move |init: FiberResume, suspend| {
        let (slot, mut store) = match init {
            Ok(pair) => pair,
            // we were dropped before we started, just bail out.
            Err(_) => return,
        };

        // Configure our store's suspension context for the rest of the
        // execution of this fiber. Note that a raw pointer is stored here
        // which is only valid for the duration of this closure.
        // Consequently we at least replace it with the previous value when
        // we're done. This reset is also required for correctness because
        // otherwise our value will overwrite another active fiber's value.
        // There should be a test that segfaults in `async_functions.rs` if
        // this `Replace` is removed.
        //
        // Note that `slot`, our return pointer, is actually a pointer onto the
        // stack of the original runtime (hurray pinned futures). We need to
        // erase the type of `func` from the fiber's type, so this is
        // transmitted as a bland `*mut u8` which requires a cast here.
        unsafe {
            let current_suspend = (*store).async_state.current_suspend.get();
            let _reset = Reset(current_suspend, *current_suspend);
            *current_suspend = suspend;

            *slot.cast() = Some(func(&mut store));
        }
    });
    let fiber = match fiber {
        Ok(fiber) => fiber,
        Err((e, stack)) => {
            unsafe {
                cx.as_context()
                    .engine()
                    .allocator()
                    .deallocate_fiber_stack(&stack);
            }
            return FiberFuture {
                state: State::Failed(Some(e.into())),
                store,
            };
        }
    };

    FiberFuture {
        store,
        state: State::Fiber {
            ret: None,
            fiber: RunningFiber(fiber),
            first: true,
        },
    }
}

pub struct FiberFuture<'fiber, T: AsContextMut, R> {
    state: State<'fiber, R>,
    store: T,
}

enum State<'fiber, R> {
    Failed(Option<anyhow::Error>),
    Fiber {
        first: bool,
        ret: Option<R>,
        fiber: RunningFiber<'fiber>,
    },
}

struct RunningFiber<'fiber>(wasmtime_fiber::Fiber<'fiber, FiberResume, (), FiberResult>);

// This is surely the most dangerous `unsafe impl Send` in the entire
// crate. The fiber here is actual honest-to-god Rust code which we're moving
// around. What we're doing is the equivalent of moving our thread's stack to
// another OS thread. Turns out we, in general, have no idea what's on the stack
// and would generally have no way to verify that this is actually safe to do!
//
// Thankfully, though, Wasmtime has the power. Without being glib it's
// actually worth examining what's on the stack. It's unfortunately not
// super-local to this function itself. Our closure to `Fiber::new` runs
// `func`, which is given to us from the outside. Thankfully, though, we
// have tight control over this. Usage of `on_fiber` is typically done
// *just* before entering WebAssembly itself, so we'll have a few stack
// frames of Rust code (all in Wasmtime itself) before we enter wasm.
//
// Once we've entered wasm, well then we have a whole bunch of wasm
// frames on the stack. We've got this nifty thing called Cranelift,
// though, which allows us to also have complete control over everything
// on the stack!
//
// Finally, when wasm switches back to the fiber's starting pointer
// (this future we're returning) then it means wasm has reentered Rust.
// Suspension can only happen via the `block_on` function of an
// `AsyncCx`. This, conveniently, also happens entirely in Wasmtime
// controlled code!
//
// There's an extremely important point that should be called out here.
// User-provided stack frames **are not on the stack** during suspension
// points. This is extremely crucial because we in general cannot reason about
// Send/Sync for stack-local variables since rustc doesn't analyze them at all.
// With our construction, though, we are guaranteed that Wasmtime owns all
// stack frames between the stack of a fiber and when the fiber suspends (and
// it could move across threads). At this time the only user-provided piece of
// data on the stack is the future itself given to us for an async host call
// which yields. Lo-and-behold as you might notice the future is required to be
// `Send`!
//
// What this all boils down to is that we, as the authors of Wasmtime,
// need to be extremely careful that on the async fiber stack we only
// store Send things. For example we can't start using `Rc` willy nilly
// by accident and leave a copy in TLS somewhere. (similarly we have to
// be ready for TLS to change while we're executing wasm code between
// suspension points).
//
// While somewhat onerous it shouldn't be too too hard (the TLS bit is
// the hardest bit so far). This does mean, though, that no user should
// ever have to worry about the `Send`-ness of Wasmtime. If rustc says
// it's ok, then it's ok.
//
// With all that in mind we unsafely assert here that wasmtime is
// correct. We declare the fiber as only containing Send data on its
// stack, despite not knowing for sure at compile time that this is
// correct. That's what `unsafe` in Rust is all about, though, right?
unsafe impl Send for RunningFiber<'_> {}

impl<T, R> Future for FiberFuture<'_, T, R>
where
    T: AsContextMut,
    R: FromError,
{
    type Output = R;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // Note the unsafety here, which is here for, uh, a number of reasons:
        //
        // * First we're using `unchecked` methods on `Pin`. The purpose of
        //   `Pin` is to ensure that `Self` does not move in memory. We actually
        //   indeed rely on this because the return pointer, `*mut Option<R>`,
        //   is passed to the fiber as `*mut u8` and isn't allowed to move. We
        //   use the unsafe methods here because we can't otherwise safely work
        //   with the `Pin`. Internally this means we need to be careful to not
        //   actually move out of the memory, which we do indeed do (except for
        //   fetching the return values which are safe to move out at that
        //   time).
        //
        // * Second we're doing some bits and pieces with unsafe pointers. We
        //   need to transmit the `cx` value, as a temporary pointer, to the
        //   `Store`. This value is then read from an `AsyncCx` variable during
        //   a hostcall whenever a yield actually happens. The transmutation of
        //   `Context` to promote it to a `'static` context is not safe, but our
        //   usage of this should be safe since we only ever actually use the
        //   `Context` with a shorter temporary lifetime, and the `'static` is
        //   just for storage and transmission through the store.
        //
        // * Finally we do a similar thing with the store pointer as we do with
        //   the context pointer. The store is transmitted to the fiber as a
        //   resumption parameter to effectively grant it access to the
        //   internals of the store while it's executing. Note that it's an
        //   unsafe pointer, but we internally within the fiber ensure that the
        //   store is only ever used within an execution, not across a
        //   suspension point.
        unsafe {
            let me = self.get_unchecked_mut();
            let store = me.store.as_context_mut();
            let (fiber, ret) = match &mut me.state {
                State::Failed(err) => return Poll::Ready(R::from_error(err.take().unwrap())),
                State::Fiber { fiber, ret, first } => {
                    // On the first entry into the fiber we run the hook which
                    // indicates that we're about to leave native code.
                    if *first {
                        *first = false;
                        if let Err(e) = store.0.exiting_native_hook() {
                            return Poll::Ready(R::from_trap(e));
                        }
                    }
                    (fiber, ret)
                }
            };

            // We need to carry over this `cx` into our fiber's runtime
            // for when it tries to poll sub-futures that are created. Doing
            // this must be done unsafely, however, since `cx` is only alive
            // for this one singular function call. Here we do a `transmute`
            // to extend the lifetime of `Context` so it can be stored in
            // our `Store`, and then we replace the current polling context
            // with this one.
            //
            // Note that the replace is done for weird situations where
            // futures might be switching contexts and there's multiple
            // wasmtime futures in a chain of futures.
            //
            // On exit from this function, though, we reset the polling
            // context back to what it was to signify that `Store` no longer
            // has access to this pointer.
            let current_poll_cx = store.0.async_state.current_poll_cx.get();
            let _reset = Reset(current_poll_cx, *current_poll_cx);
            *current_poll_cx = std::mem::transmute::<&mut Context<'_>, *mut Context<'static>>(cx);

            // Futz with the `Store` pointer a bit and then give both the out
            // pointer an the store pointer to the fiber to resume. Note that
            // these two pointers are primarily used on the initial run of the
            // fiber. Also note that the `result` pointer must not move in
            // memory after the first resumption since we currently don't read
            // the resumption value after each time.
            let store = me.store.as_context_mut().opaque();
            let result = ret as *mut Option<R>;
            let store = std::mem::transmute::<StoreOpaque<'_>, StoreOpaque<'static>>(store);
            match fiber.0.resume(Ok((result.cast(), store))) {
                // This means the fiber finished. Upon finishing we run the hook
                // to come back into native code. If that succeeds then we can
                // read the `result` pointer.
                //
                // Note that the `result` pointer should always be `Some` here
                // so we `unwrap` it. The only case that the pointer is `None`
                // is if the future was dropped, but in that case we can't be
                // executing this method anyway, so that part doesn't matter.
                Ok(()) => match me.store.as_context_mut().0.entering_native_hook() {
                    Ok(()) => Poll::Ready((*result).take().unwrap()),
                    Err(e) => Poll::Ready(R::from_trap(e)),
                },

                // The fiber hit a suspension point and gave us the suspension
                // type (`()` in our case). This only happens when a
                // host-provided future suspended, so we propagate the
                // suspension here.
                Err(()) => Poll::Pending,
            }
        }
    }
}

impl<T, R> AsContext for FiberFuture<'_, T, R>
where
    T: AsContextMut,
{
    type Data = T::Data;
    fn as_context(&self) -> StoreContext<'_, T::Data> {
        self.store.as_context()
    }
}

impl<T, R> AsContextMut for FiberFuture<'_, T, R>
where
    T: AsContextMut,
{
    fn as_context_mut(&mut self) -> StoreContextMut<'_, T::Data> {
        self.store.as_context_mut()
    }
}

// Dropping futures is pretty special in that it means the future has
// been requested to be cancelled. Here we run the risk of dropping an
// in-progress fiber, and if we were to do nothing then the fiber would
// leak all its owned stack resources.
//
// To handle this we implement `Drop` here and, if the fiber isn't done,
// resume execution of the fiber saying "hey please stop you're
// interrupted". Our `Trap` created here (which has the stack trace
// of whomever dropped us) will then get propagated in whatever called
// `block_on`, and the idea is that the trap propagates all the way back
// up to the original fiber start, finishing execution.
//
// We don't actually care about the fiber's return value here (no one's
// around to look at it), we just assert the fiber finished to
// completion.
impl<T, R> Drop for FiberFuture<'_, T, R>
where
    T: AsContextMut,
{
    fn drop(&mut self) {
        let fiber = match &mut self.state {
            State::Fiber { fiber, .. } => fiber,
            State::Failed(_) => return,
        };
        if !fiber.0.done() {
            let result = fiber.0.resume(Err(Trap::new("future dropped")));
            // This resumption with an error should always complete the
            // fiber. While it's technically possible for host code to catch
            // the trap and re-resume, we'd ideally like to signal that to
            // callers that they shouldn't be doing that.
            debug_assert!(result.is_ok());
        }

        unsafe {
            self.store
                .as_context()
                .engine()
                .allocator()
                .deallocate_fiber_stack(fiber.0.stack());
        }
    }
}

pub struct AsyncCx {
    current_suspend: *mut *const wasmtime_fiber::Suspend<FiberResume, (), FiberResult>,
    current_poll_cx: *mut *mut Context<'static>,
}

impl AsyncCx {
    /// Blocks on the asynchronous computation represented by `future` and
    /// produces the result here, in-line.
    ///
    /// This function is designed to only work when it's currently executing on
    /// a native fiber. This fiber provides the ability for us to handle the
    /// future's `Pending` state as "jump back to whomever called the fiber in
    /// an asynchronous fashion and propagate `Pending`". This tight coupling
    /// with `on_fiber` below is what powers the asynchronicity of calling wasm.
    /// Note that the asynchronous part only applies to host functions, wasm
    /// itself never really does anything asynchronous at this time.
    ///
    /// This function takes a `future` and will (appear to) synchronously wait
    /// on the result. While this function is executing it will fiber switch
    /// to-and-from the original frame calling `on_fiber` which should be a
    /// guarantee due to how async stores are configured.
    ///
    /// The return value here is either the output of the future `T`, or a trap
    /// which represents that the asynchronous computation was cancelled. It is
    /// not recommended to catch the trap and try to keep executing wasm, so
    /// we've tried to liberally document this.
    pub unsafe fn block_on<U>(
        &self,
        mut future: Pin<&mut (dyn Future<Output = U> + Send)>,
    ) -> Result<U, Trap> {
        // Take our current `Suspend` context which was configured as soon as
        // our fiber started. Note that we must load it at the front here and
        // save it on our stack frame. While we're polling the future other
        // fibers may be started for recursive computations, and the current
        // suspend context is only preserved at the edges of the fiber, not
        // during the fiber itself.
        //
        // For a little bit of extra safety we also replace the current value
        // with null to try to catch any accidental bugs on our part early.
        // This is all pretty unsafe so we're trying to be careful...
        //
        // Note that there should be a segfaulting test  in `async_functions.rs`
        // if this `Reset` is removed.
        let suspend = *self.current_suspend;
        let _reset = Reset(self.current_suspend, suspend);
        *self.current_suspend = ptr::null();
        assert!(!suspend.is_null());

        loop {
            let future_result = {
                let poll_cx = *self.current_poll_cx;
                let _reset = Reset(self.current_poll_cx, poll_cx);
                *self.current_poll_cx = ptr::null_mut();
                assert!(!poll_cx.is_null());
                future.as_mut().poll(&mut *poll_cx)
            };

            match future_result {
                Poll::Ready(t) => break Ok(t),
                Poll::Pending => {}
            }

            let before = wasmtime_runtime::TlsRestore::take().map_err(Trap::from_runtime)?;
            let res = (*suspend).suspend(());
            before.replace().map_err(Trap::from_runtime)?;
            res?;
        }
    }
}

pub trait FromError {
    fn from_error(err: anyhow::Error) -> Self;
    fn from_trap(err: Trap) -> Self;
}

impl<T, E> FromError for Result<T, E>
where
    E: From<anyhow::Error> + From<Trap>,
{
    fn from_error(err: anyhow::Error) -> Self {
        Err(err.into())
    }
    fn from_trap(err: Trap) -> Self {
        Err(err.into())
    }
}

struct Reset<T: Copy>(*mut T, T);

impl<T: Copy> Drop for Reset<T> {
    fn drop(&mut self) {
        unsafe {
            *self.0 = self.1;
        }
    }
}
