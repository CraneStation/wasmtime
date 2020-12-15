use crate::frame_info::StoreFrameInfo;
use crate::sig_registry::SignatureRegistry;
use crate::trampoline::StoreInstanceHandle;
use crate::{Engine, Module, Trap};
use anyhow::{bail, Result};
use std::any::Any;
use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::convert::TryFrom;
use std::fmt;
use std::future::Future;
use std::hash::{Hash, Hasher};
use std::pin::Pin;
use std::ptr;
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::task::{Context, Poll};
use wasmtime_environ::wasm;
use wasmtime_jit::{CompiledModule, ModuleCode, TypeTables};
use wasmtime_runtime::{
    InstanceAllocator, InstanceHandle, SignalHandler, StackMapRegistry, TrapInfo, VMContext,
    VMExternRef, VMExternRefActivationsTable, VMInterrupts, VMSharedSignatureIndex,
};

/// Used to associate instances with the store.
///
/// This is needed to track if the instance was allocated expliclty with the default
/// instance allocator.
struct StoreInstance {
    handle: InstanceHandle,
    use_default_allocator: bool,
}

/// A `Store` is a collection of WebAssembly instances and host-defined items.
///
/// All WebAssembly instances and items will be attached to and refer to a
/// `Store`. For example instances, functions, globals, and tables are all
/// attached to a `Store`. Instances are created by instantiating a
/// [`Module`](crate::Module) within a `Store`.
///
/// `Store` is not thread-safe and cannot be sent to other threads. All items
/// which refer to a `Store` additionally are not threadsafe and can only be
/// used on the original thread that they were created on.
///
/// A `Store` is not intended to be a long-lived object in a program. No form of
/// GC is implemented at this time so once an instance is created within a
/// `Store` it will not be deallocated until all references to the `Store` have
/// gone away (this includes all references to items in the store). This makes
/// `Store` unsuitable for creating an unbounded number of instances in it
/// because `Store` will never release this memory. It's instead recommended to
/// have a long-lived [`Engine`] and instead create a `Store` for a more scoped
/// portion of your application.
///
/// # Stores and `Clone`
///
/// Using `clone` on a `Store` is a cheap operation. It will not create an
/// entirely new store, but rather just a new reference to the existing object.
/// In other words it's a shallow copy, not a deep copy.
///
/// ## Stores and `Default`
///
/// You can create a store with default configuration settings using
/// `Store::default()`. This will create a brand new [`Engine`] with default
/// ocnfiguration (see [`Config`](crate::Config) for more information).
#[derive(Clone)]
pub struct Store {
    inner: Rc<StoreInner>,
}

pub(crate) struct StoreInner {
    is_async: bool,
    engine: Engine,
    interrupts: Arc<VMInterrupts>,
    signatures: RefCell<SignatureRegistry>,
    instances: RefCell<Vec<StoreInstance>>,
    signal_handler: RefCell<Option<Box<SignalHandler<'static>>>>,
    externref_activations_table: VMExternRefActivationsTable,
    stack_map_registry: StackMapRegistry,
    /// Information about JIT code which allows us to test if a program counter
    /// is in JIT code, lookup trap information, etc.
    frame_info: RefCell<StoreFrameInfo>,
    /// Set of all compiled modules that we're holding a strong reference to
    /// the module's code for. This includes JIT functions, trampolines, etc.
    modules: RefCell<HashSet<ArcModuleCode>>,

    // Numbers of resources instantiated in this store.
    instance_count: Cell<usize>,
    memory_count: Cell<usize>,
    table_count: Cell<usize>,

    /// An adjustment to add to the fuel consumed value in `interrupts` above
    /// to get the true amount of fuel consumed.
    fuel_adj: Cell<i64>,

    #[cfg(feature = "async")]
    current_suspend: Cell<*const wasmtime_fiber::Suspend<Result<(), Trap>, (), Result<(), Trap>>>,
    #[cfg(feature = "async")]
    current_poll_cx: Cell<*mut Context<'static>>,
    out_of_gas_behavior: Cell<OutOfGas>,
}

#[derive(Copy, Clone)]
enum OutOfGas {
    Trap,
    InjectFuel {
        injection_count: u32,
        fuel_to_inject: u64,
    },
}

struct HostInfoKey(VMExternRef);

impl PartialEq for HostInfoKey {
    fn eq(&self, rhs: &Self) -> bool {
        VMExternRef::eq(&self.0, &rhs.0)
    }
}

impl Eq for HostInfoKey {}

impl Hash for HostInfoKey {
    fn hash<H>(&self, hasher: &mut H)
    where
        H: Hasher,
    {
        VMExternRef::hash(&self.0, hasher);
    }
}

impl Store {
    /// Creates a new store to be associated with the given [`Engine`].
    ///
    /// Note that this `Store` cannot be used with asynchronous host calls nor
    /// can it be used to call functions asynchronously. For that you'll want to
    /// use [`Store::new_async`].
    pub fn new(engine: &Engine) -> Store {
        Store::_new(engine, false)
    }

    /// Creates a new async store to be associated with the given [`Engine`].
    ///
    /// The returned store can optionally define host functions with `async`.
    /// Instances created and functions called within the returned `Store`
    /// *must* be called through their asynchronous APIs, however. For example
    /// using [`Func::call`](crate::Func::call) will panic in the returned
    /// store.
    ///
    /// # Asynchronous Wasm
    ///
    /// WebAssembly does not currently have a way to specify at the bytecode
    /// level what is and isn't async. Host-defined functions, however, may be
    /// defined as `async`. WebAssembly imports always appear synchronous, which
    /// gives rise to a bit of an impedence mismatch here. To solve this
    /// Wasmtime supports "asynchronous stores" which enables calling these
    /// asynchronous functions in a way that looks synchronous to the executing
    /// WebAssembly code.
    ///
    /// An asynchronous store must always invoke wasm code asynchronously,
    /// meaning we'll always represent its computation as a
    /// [`Future`](std::future::Future). The `poll` method of the futures
    /// returned by Wasmtime will perform the actual work of calling the
    /// WebAssembly. Wasmtime won't manage its own thread pools or similar,
    /// that's left up to the embedder.
    ///
    /// To implement futures in a way that WebAssembly sees asynchronous host
    /// functions as synchronous, all async Wasmtime futures will execute on a
    /// separately allocated native stack from the thread otherwise executing
    /// Wasmtime. This separate native stack can then be switched to and from.
    /// Using this whenever an `async` host function returns a future that
    /// resolves to `Pending` we switch away from the temporary stack back to
    /// the main stack and propagate the `Pending` status.
    ///
    /// In general it's encouraged that the integration with `async` and
    /// wasmtime is designed early on in your embedding of Wasmtime to ensure
    /// that it's planned that WebAssembly executes in the right context of your
    /// application.
    ///
    /// # Execution in `poll`
    ///
    /// The [`Future::poll`](std::future::Future::poll) method is the main
    /// driving force behind Rust's futures. That method's own documentation
    /// states "an implementation of `poll` should strive to return quickly, and
    /// should not block". This, however, can be at odds with executing
    /// WebAssembly code as part of the `poll` method itself. If your
    /// WebAssembly is untrusted then this could allow the `poll` method to take
    /// arbitrarily long in the worst case, likely blocking all other
    /// asynchronous tasks.
    ///
    /// To remedy this situation you have a two possible ways to solve this:
    ///
    /// * First you can spawn futures into a thread pool. Care must be taken for
    ///   this because Wasmtime futures are not `Send` or `Sync`. If you ensure
    ///   that the entire state of a `Store` is wrapped up in a single future,
    ///   though, you can send the whole future at once to a separate thread. By
    ///   doing this in a thread pool you are relaxing the requirement that
    ///   `Future::poll` must be fast because your future is executing on a
    ///   separate thread. This strategy, however, would likely still require
    ///   some form of cancellation via [`Store::interrupt_handle`] to ensure
    ///   wasm doesn't take *too* long to execute.
    ///
    /// * Alternatively you can enable the
    ///   [`Config::consume_fuel`](crate::Config::consume_fuel) method as well
    ///   as [`Store::out_of_fuel_async_yield`] When doing so this will
    ///   configure Wasmtime futures to yield periodically while they're
    ///   executing WebAssembly code. After consuming the specified amount of
    ///   fuel wasm futures will return `Poll::Pending` from their `poll`
    ///   method, and will get automatically re-polled later. This enables the
    ///   `Future::poll` method to take roughly a fixed amount of time since
    ///   fuel is guaranteed to get consumed while wasm is executing. Note that
    ///   to prevent infinite execution of wasm you'll still need to use
    ///   [`Store::interrupt_handle`].
    ///
    /// In either case special care needs to be taken when integrating
    /// asynchronous wasm into your application. You should carefully plan where
    /// WebAssembly will execute and what compute resources will be allotted to
    /// it. If Wasmtime doesn't support exactly what you'd like just yet, please
    /// feel free to open an issue!
    #[cfg(feature = "async")]
    #[cfg_attr(nightlydoc, doc(cfg(feature = "async")))]
    pub fn new_async(engine: &Engine) -> Store {
        Store::_new(engine, true)
    }

    fn _new(engine: &Engine, is_async: bool) -> Store {
        // Ensure that wasmtime_runtime's signal handlers are configured. Note
        // that at the `Store` level it means we should perform this
        // once-per-thread. Platforms like Unix, however, only require this
        // once-per-program. In any case this is safe to call many times and
        // each one that's not relevant just won't do anything.
        wasmtime_runtime::init_traps();

        Store {
            inner: Rc::new(StoreInner {
                is_async,
                engine: engine.clone(),
                interrupts: Arc::new(Default::default()),
                signatures: RefCell::new(Default::default()),
                instances: RefCell::new(Vec::new()),
                signal_handler: RefCell::new(None),
                externref_activations_table: VMExternRefActivationsTable::new(),
                stack_map_registry: StackMapRegistry::default(),
                frame_info: Default::default(),
                modules: Default::default(),
                instance_count: Default::default(),
                memory_count: Default::default(),
                table_count: Default::default(),
                fuel_adj: Cell::new(0),
                #[cfg(feature = "async")]
                current_suspend: Cell::new(ptr::null()),
                #[cfg(feature = "async")]
                current_poll_cx: Cell::new(ptr::null_mut()),
                out_of_gas_behavior: Cell::new(OutOfGas::Trap),
            }),
        }
    }

    pub(crate) fn from_inner(inner: Rc<StoreInner>) -> Store {
        Store { inner }
    }

    /// Returns the [`Engine`] that this store is associated with.
    pub fn engine(&self) -> &Engine {
        &self.inner.engine
    }

    pub(crate) fn signatures(&self) -> &RefCell<SignatureRegistry> {
        &self.inner.signatures
    }

    pub(crate) fn lookup_shared_signature<'a>(
        &'a self,
        types: &'a TypeTables,
    ) -> impl Fn(wasm::SignatureIndex) -> VMSharedSignatureIndex + 'a {
        move |index| {
            self.signatures()
                .borrow()
                .lookup(&types.wasm_signatures[index])
                .expect("signature not previously registered")
        }
    }

    pub(crate) fn register_module(&self, module: &Module) {
        // All modules register their JIT code in a store for two reasons
        // currently:
        //
        // * First we only catch signals/traps if the program counter falls
        //   within the jit code of an instantiated wasm module. This ensures
        //   we don't catch accidental Rust/host segfaults.
        //
        // * Second when generating a backtrace we'll use this mapping to
        //   only generate wasm frames for instruction pointers that fall
        //   within jit code.
        self.register_jit_code(module.compiled_module());

        // We need to know about all the stack maps of all instantiated modules
        // so when performing a GC we know about all wasm frames that we find
        // on the stack.
        self.register_stack_maps(module.compiled_module());

        // Signatures are loaded into our `SignatureRegistry` here
        // once-per-module (and once-per-signature). This allows us to create
        // a `Func` wrapper for any function in the module, which requires that
        // we know about the signature and trampoline for all instances.
        self.register_signatures(module);

        // And finally with a module being instantiated into this `Store` we
        // need to preserve its jit-code. References to this module's code and
        // trampolines are not owning-references so it's our responsibility to
        // keep it all alive within the `Store`.
        self.inner
            .modules
            .borrow_mut()
            .insert(ArcModuleCode(module.compiled_module().code().clone()));
    }

    fn register_jit_code(&self, module: &CompiledModule) {
        let functions = module.finished_functions();
        let first_pc = match functions.values().next() {
            Some(f) => unsafe { (**f).as_ptr() as usize },
            None => return,
        };
        // Only register this module if it hasn't already been registered.
        let mut info = self.inner.frame_info.borrow_mut();
        if !info.contains_pc(first_pc) {
            info.register(module);
        }
    }

    fn register_stack_maps(&self, module: &CompiledModule) {
        self.stack_map_registry()
            .register_stack_maps(module.stack_maps().map(|(func, stack_maps)| unsafe {
                let ptr = (*func).as_ptr();
                let len = (*func).len();
                let start = ptr as usize;
                let end = ptr as usize + len;
                let range = start..end;
                (range, stack_maps)
            }));
    }

    fn register_signatures(&self, module: &Module) {
        let trampolines = module.compiled_module().trampolines();
        let mut signatures = self.signatures().borrow_mut();
        for (index, wasm) in module.types().wasm_signatures.iter() {
            signatures.register(wasm, trampolines[index]);
        }
    }

    pub(crate) fn bump_resource_counts(&self, module: &Module) -> Result<()> {
        let config = self.engine().config();

        fn bump(slot: &Cell<usize>, max: usize, amt: usize, desc: &str) -> Result<()> {
            let new = slot.get().saturating_add(amt);
            if new > max {
                bail!(
                    "resource limit exceeded: {} count too high at {}",
                    desc,
                    new
                );
            }
            slot.set(new);
            Ok(())
        }

        let module = module.env_module();
        let memories = module.memory_plans.len() - module.num_imported_memories;
        let tables = module.table_plans.len() - module.num_imported_tables;

        bump(
            &self.inner.instance_count,
            config.max_instances,
            1,
            "instance",
        )?;
        bump(
            &self.inner.memory_count,
            config.max_memories,
            memories,
            "memory",
        )?;
        bump(&self.inner.table_count, config.max_tables, tables, "table")?;

        Ok(())
    }

    pub(crate) unsafe fn add_instance(
        &self,
        handle: InstanceHandle,
        use_default_allocator: bool,
    ) -> StoreInstanceHandle {
        self.inner.instances.borrow_mut().push(StoreInstance {
            handle: handle.clone(),
            use_default_allocator,
        });
        StoreInstanceHandle {
            store: self.clone(),
            handle,
        }
    }

    pub(crate) fn existing_instance_handle(&self, handle: InstanceHandle) -> StoreInstanceHandle {
        debug_assert!(self
            .inner
            .instances
            .borrow()
            .iter()
            .any(|i| i.handle.vmctx_ptr() == handle.vmctx_ptr()));
        StoreInstanceHandle {
            store: self.clone(),
            handle,
        }
    }

    pub(crate) unsafe fn existing_vmctx(&self, cx: *mut VMContext) -> StoreInstanceHandle {
        self.existing_instance_handle(InstanceHandle::from_vmctx(cx))
    }

    pub(crate) fn weak(&self) -> Weak<StoreInner> {
        Rc::downgrade(&self.inner)
    }

    pub(crate) fn upgrade(weak: &Weak<StoreInner>) -> Option<Self> {
        let inner = weak.upgrade()?;
        Some(Self { inner })
    }

    pub(crate) fn set_signal_handler(&self, handler: Option<Box<SignalHandler<'static>>>) {
        *self.inner.signal_handler.borrow_mut() = handler;
    }

    pub(crate) fn interrupts(&self) -> &VMInterrupts {
        &self.inner.interrupts
    }

    /// Returns whether the stores `a` and `b` refer to the same underlying
    /// `Store`.
    ///
    /// Because the `Store` type is reference counted multiple clones may point
    /// to the same underlying storage, and this method can be used to determine
    /// whether two stores are indeed the same.
    pub fn same(a: &Store, b: &Store) -> bool {
        Rc::ptr_eq(&a.inner, &b.inner)
    }

    /// Creates an [`InterruptHandle`] which can be used to interrupt the
    /// execution of instances within this `Store`.
    ///
    /// An [`InterruptHandle`] handle is a mechanism of ensuring that guest code
    /// doesn't execute for too long. For example it's used to prevent wasm
    /// programs for executing infinitely in infinite loops or recursive call
    /// chains.
    ///
    /// The [`InterruptHandle`] type is sendable to other threads so you can
    /// interact with it even while the thread with this `Store` is executing
    /// wasm code.
    ///
    /// There's one method on an interrupt handle:
    /// [`InterruptHandle::interrupt`]. This method is used to generate an
    /// interrupt and cause wasm code to exit "soon".
    ///
    /// ## When are interrupts delivered?
    ///
    /// The term "interrupt" here refers to one of two different behaviors that
    /// are interrupted in wasm:
    ///
    /// * The head of every loop in wasm has a check to see if it's interrupted.
    /// * The prologue of every function has a check to see if it's interrupted.
    ///
    /// This interrupt mechanism makes no attempt to signal interrupts to
    /// native code. For example if a host function is blocked, then sending
    /// an interrupt will not interrupt that operation.
    ///
    /// Interrupts are consumed as soon as possible when wasm itself starts
    /// executing. This means that if you interrupt wasm code then it basically
    /// guarantees that the next time wasm is executing on the target thread it
    /// will return quickly (either normally if it were already in the process
    /// of returning or with a trap from the interrupt). Once an interrupt
    /// trap is generated then an interrupt is consumed, and further execution
    /// will not be interrupted (unless another interrupt is set).
    ///
    /// When implementing interrupts you'll want to ensure that the delivery of
    /// interrupts into wasm code is also handled in your host imports and
    /// functionality. Host functions need to either execute for bounded amounts
    /// of time or you'll need to arrange for them to be interrupted as well.
    ///
    /// ## Return Value
    ///
    /// This function returns a `Result` since interrupts are not always
    /// enabled. Interrupts are enabled via the
    /// [`Config::interruptable`](crate::Config::interruptable) method, and if
    /// this store's [`Config`](crate::Config) hasn't been configured to enable
    /// interrupts then an error is returned.
    ///
    /// ## Examples
    ///
    /// ```
    /// # use anyhow::Result;
    /// # use wasmtime::*;
    /// # fn main() -> Result<()> {
    /// // Enable interruptable code via `Config` and then create an interrupt
    /// // handle which we'll use later to interrupt running code.
    /// let engine = Engine::new(Config::new().interruptable(true));
    /// let store = Store::new(&engine);
    /// let interrupt_handle = store.interrupt_handle()?;
    ///
    /// // Compile and instantiate a small example with an infinite loop.
    /// let module = Module::new(&engine, r#"
    ///     (func (export "run") (loop br 0))
    /// "#)?;
    /// let instance = Instance::new(&store, &module, &[])?;
    /// let run = instance
    ///     .get_func("run")
    ///     .ok_or(anyhow::format_err!("failed to find `run` function export"))?
    ///     .get0::<()>()?;
    ///
    /// // Spin up a thread to send us an interrupt in a second
    /// std::thread::spawn(move || {
    ///     std::thread::sleep(std::time::Duration::from_secs(1));
    ///     interrupt_handle.interrupt();
    /// });
    ///
    /// let trap = run().unwrap_err();
    /// assert!(trap.to_string().contains("wasm trap: interrupt"));
    /// # Ok(())
    /// # }
    /// ```
    pub fn interrupt_handle(&self) -> Result<InterruptHandle> {
        if self.engine().config().tunables.interruptable {
            Ok(InterruptHandle {
                interrupts: self.inner.interrupts.clone(),
            })
        } else {
            bail!("interrupts aren't enabled for this `Store`")
        }
    }

    pub(crate) fn externref_activations_table(&self) -> &VMExternRefActivationsTable {
        &self.inner.externref_activations_table
    }

    pub(crate) fn stack_map_registry(&self) -> &StackMapRegistry {
        &self.inner.stack_map_registry
    }

    pub(crate) fn frame_info(&self) -> &RefCell<StoreFrameInfo> {
        &self.inner.frame_info
    }

    /// Perform garbage collection of `ExternRef`s.
    pub fn gc(&self) {
        // For this crate's API, we ensure that `set_stack_canary` invariants
        // are upheld for all host-->Wasm calls, and we register every module
        // used with this store in `self.inner.stack_map_registry`.
        unsafe {
            wasmtime_runtime::gc(
                &self.inner.stack_map_registry,
                &self.inner.externref_activations_table,
            );
        }
    }

    /// Returns the amount of fuel consumed by this store's execution so far.
    ///
    /// If fuel consumption is not enabled via
    /// [`Config::consume_fuel`](crate::Config::consume_fuel) then this
    /// function will return `None`. Also note that fuel, if enabled, must be
    /// originally configured via [`Store::add_fuel`].
    pub fn fuel_consumed(&self) -> Option<u64> {
        if !self.engine().config().tunables.consume_fuel {
            return None;
        }
        let consumed = unsafe { *self.inner.interrupts.fuel_consumed.get() };
        Some(u64::try_from(self.inner.fuel_adj.get() + consumed).unwrap())
    }

    /// Adds fuel to this [`Store`] for wasm to consume while executing.
    ///
    /// For this method to work fuel consumption must be enabled via
    /// [`Config::consume_fuel`](crate::Config::consume_fuel). By default a
    /// [`Store`] starts with 0 fuel for wasm to execute with (meaning it will
    /// immediately trap). This function must be called for the store to have
    /// some fuel to allow WebAssembly to execute.
    ///
    /// Note that at this time when fuel is entirely consumed it will cause
    /// wasm to trap. More usages of fuel are planned for the future.
    ///
    /// # Panics
    ///
    /// This function will panic if the store's [`Config`](crate::Config) did
    /// not have fuel consumption enabled.
    pub fn add_fuel(&self, fuel: u64) {
        assert!(self.engine().config().tunables.consume_fuel);

        // Fuel is stored as an i64, so we need to cast it. If the provided fuel
        // value overflows that just assume that i64::max will suffice. Wasm
        // execution isn't fast enough to burn through i64::max fuel in any
        // reasonable amount of time anyway.
        let fuel = i64::try_from(fuel).unwrap_or(i64::max_value());
        let adj = self.inner.fuel_adj.get();
        let consumed_ptr = unsafe { &mut *self.inner.interrupts.fuel_consumed.get() };

        match (consumed_ptr.checked_sub(fuel), adj.checked_add(fuel)) {
            // If we succesfully did arithmetic without overflowing then we can
            // just update our fields.
            (Some(consumed), Some(adj)) => {
                self.inner.fuel_adj.set(adj);
                *consumed_ptr = consumed;
            }

            // Otherwise something overflowed. Make sure that we preserve the
            // amount of fuel that's already consumed, but otherwise assume that
            // we were given infinite fuel.
            _ => {
                self.inner.fuel_adj.set(i64::max_value());
                *consumed_ptr = (*consumed_ptr + adj) - i64::max_value();
            }
        }
    }

    /// Configures a [`Store`] to generate a [`Trap`] whenever it runs out of
    /// fuel.
    ///
    /// When a [`Store`] is configured to consume fuel with
    /// [`Config::consume_fuel`](crate::Config::consume_fuel) this method will
    /// configure what happens when fuel runs out. Specifically a WebAssembly
    /// trap will be raised and the current execution of WebAssembly will be
    /// aborted.
    ///
    /// This is the default behavior for running out of fuel.
    pub fn out_of_fuel_trap(&self) {
        self.inner.out_of_gas_behavior.set(OutOfGas::Trap);
    }

    /// Configures a [`Store`] to yield execution of async WebAssembly code
    /// periodically.
    ///
    /// When a [`Store`] is configured to consume fuel with
    /// [`Config::consume_fuel`](crate::Config::consume_fuel) this method will
    /// configure what happens when fuel runs out. Specifically executing
    /// WebAssembly will be suspended and control will be yielded back to the
    /// caller. This is only suitable with use of [async
    /// stores](Store::new_async) because only then are futures used and yields
    /// are possible.
    ///
    /// The purpose of this behavior is to ensure that futures which represent
    /// execution of WebAssembly do not execute too long inside their
    /// `Future::poll` method. This allows for some form of cooperative
    /// multitasking where WebAssembly will voluntarily yield control
    /// periodically (based on fuel consumption) back to the running thread.
    ///
    /// Note that futures returned by this crate will automatically flag
    /// themselves to get re-polled if a yield happens. This means that
    /// WebAssembly will continue to execute, just after giving the host an
    /// opportunity to do something else.
    ///
    /// The `fuel_to_inject` parameter indicates how much fuel should be
    /// automatically re-injected after fuel runs out. This is how much fuel
    /// will be consumed between yields of an async future.
    ///
    /// The `injection_count` parameter indicates how many times this fuel will
    /// be injected. Multiplying the two parameters is the total amount of fuel
    /// this store is allowed before wasm traps.
    ///
    /// # Panics
    ///
    /// This method will panic if it is not called on an [async
    /// store](Store::new_async).
    pub fn out_of_fuel_async_yield(&self, injection_count: u32, fuel_to_inject: u64) {
        assert!(self.is_async());
        self.inner.out_of_gas_behavior.set(OutOfGas::InjectFuel {
            injection_count,
            fuel_to_inject,
        });
    }

    pub(crate) fn is_async(&self) -> bool {
        self.inner.is_async
    }

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
    #[cfg(feature = "async")]
    pub(crate) fn block_on<T>(
        &self,
        mut future: Pin<&mut dyn Future<Output = T>>,
    ) -> Result<T, Trap> {
        debug_assert!(self.is_async());

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
        let suspend = self.inner.current_suspend.replace(ptr::null());
        let _reset = Reset(&self.inner.current_suspend, suspend);
        assert!(!suspend.is_null());

        loop {
            let future_result = unsafe {
                let current_poll_cx = self.inner.current_poll_cx.replace(ptr::null_mut());
                let _reset = Reset(&self.inner.current_poll_cx, current_poll_cx);
                assert!(!current_poll_cx.is_null());
                future.as_mut().poll(&mut *current_poll_cx)
            };
            match future_result {
                Poll::Ready(t) => break Ok(t),
                Poll::Pending => {}
            }
            unsafe {
                (*suspend).suspend(())?;
            }
        }
    }

    /// Executes a synchronous computation `func` asynchronously on a new fiber.
    ///
    /// This function will convert the synchronous `func` into an asynchronous
    /// future. This is done by running `func` in a fiber on a separate native
    /// stack which can be suspended and resumed from.
    ///
    /// Most of the nitty-gritty here is how we juggle the various contexts
    /// necessary to suspend the fiber later on and poll sub-futures. It's hoped
    /// that the various comments are illuminating as to what's going on here.
    #[cfg(feature = "async")]
    pub(crate) async fn on_fiber<R>(&self, func: impl FnOnce() -> R) -> Result<R, Trap> {
        debug_assert!(self.is_async());

        // TODO: allocation of a fiber should be much more abstract where we
        // shouldn't be allocating huge stacks on every async wasm function call.
        let mut slot = None;
        let fiber = wasmtime_fiber::Fiber::new(10 * 1024 * 1024, |keep_going, suspend| {
            // First check and see if we were interrupted/dropped, and only
            // continue if we haven't been.
            keep_going?;

            // Configure our store's suspension context for the rest of the
            // execution of this fiber. Note that a raw pointer is stored here
            // which is only valid for the duration of this closure.
            // Consequently we at least replace it with the previous value when
            // we're done. This reset is also required for correctness because
            // otherwise our value will overwrite another active fiber's value.
            // There should be a test that segfaults in `async_functions.rs` if
            // this `Replace` is removed.
            let prev = self.inner.current_suspend.replace(suspend);
            let _reset = Reset(&self.inner.current_suspend, prev);

            slot = Some(func());
            Ok(())
        })
        .map_err(|e| Trap::from(anyhow::Error::from(e)))?;

        // Once we have the fiber representing our synchronous computation, we
        // wrap that in a custom future implementation which does the
        // translation from the future protocol to our fiber API.
        FiberFuture { fiber, store: self }.await?;
        return Ok(slot.unwrap());

        struct FiberFuture<'a> {
            fiber: wasmtime_fiber::Fiber<'a, Result<(), Trap>, (), Result<(), Trap>>,
            store: &'a Store,
        }

        impl Future for FiberFuture<'_> {
            type Output = Result<(), Trap>;

            fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
                // We need to carry over this `cx` into our fiber's runtime
                // for when it trys to poll sub-futures that are created. Doing
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
                let cx =
                    unsafe { std::mem::transmute::<&mut Context<'_>, *mut Context<'static>>(cx) };
                let prev = self.store.inner.current_poll_cx.replace(cx);
                let _reste = Reset(&self.store.inner.current_poll_cx, prev);

                // After that's set up we resume execution of the fiber, which
                // may also start the fiber for the first time. This either
                // returns `Ok` saying the fiber finished (yay!) or it returns
                // `Err` with the payload passed to `suspend`, which in our case
                // is `()`. If `Err` is returned that means the fiber polled a
                // future but it said "Pending", so we propagate that here.
                match self.fiber.resume(Ok(())) {
                    Ok(result) => Poll::Ready(result),
                    Err(()) => Poll::Pending,
                }
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
        // We don't actually care about the fiber's return value here (one one's
        // around to look at it), we just assert the fiber finished to
        // completion.
        impl Drop for FiberFuture<'_> {
            fn drop(&mut self) {
                if self.fiber.done() {
                    return;
                }
                let result = self.fiber.resume(Err(Trap::new("future dropped")));
                // This resumption with an error should always complete the
                // fiber. While it's technically possible for host code to catch
                // the trap and re-resume, we'd ideally like to signal that to
                // callers that they shouldn't be doing that.
                debug_assert!(result.is_ok());
            }
        }
    }

    /// Immediately raise a trap on an out-of-gas condition.
    fn out_of_gas_trap(&self) -> ! {
        #[derive(Debug)]
        struct OutOfGasError;

        impl fmt::Display for OutOfGasError {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("all fuel consumed by WebAssembly")
            }
        }

        impl std::error::Error for OutOfGasError {}
        unsafe {
            wasmtime_runtime::raise_lib_trap(wasmtime_runtime::Trap::User(Box::new(OutOfGasError)))
        }
    }

    /// Yields execution to the caller on out-of-gas
    ///
    /// This only works on async futures and stores, and assumes that we're
    /// executing on a fiber. This will yield execution back to the caller once
    /// and when we come back we'll continue with `fuel_to_inject` more fuel.
    #[cfg(feature = "async")]
    fn out_of_gas_yield(&self, fuel_to_inject: u64) {
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
        match self.block_on(unsafe { Pin::new_unchecked(&mut future) }) {
            // If this finished successfully then we were resumed normally via a
            // `poll`, so inject some more fuel and keep going.
            Ok(()) => self.add_fuel(fuel_to_inject),
            // If the future was dropped while we were yielded, then we need to
            // clean up this fiber. Do so by raising a trap which will abort all
            // wasm and get caught on the other side to clean things up.
            Err(trap) => unsafe { wasmtime_runtime::raise_user_trap(trap.into()) },
        }
    }
}

unsafe impl TrapInfo for Store {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn is_wasm_trap(&self, addr: usize) -> bool {
        self.frame_info().borrow().lookup_trap_info(addr).is_some()
    }

    fn custom_signal_handler(&self, call: &dyn Fn(&SignalHandler) -> bool) -> bool {
        if let Some(handler) = &*self.inner.signal_handler.borrow() {
            return call(handler);
        }
        false
    }

    fn max_wasm_stack(&self) -> usize {
        self.engine().config().max_wasm_stack
    }

    fn out_of_gas(&self) {
        match self.inner.out_of_gas_behavior.get() {
            OutOfGas::Trap => self.out_of_gas_trap(),
            #[cfg(feature = "async")]
            OutOfGas::InjectFuel {
                injection_count,
                fuel_to_inject,
            } => {
                if injection_count == 0 {
                    self.out_of_gas_trap();
                }
                self.inner.out_of_gas_behavior.set(OutOfGas::InjectFuel {
                    injection_count: injection_count - 1,
                    fuel_to_inject,
                });
                self.out_of_gas_yield(fuel_to_inject);
            }
            #[cfg(not(feature = "async"))]
            OutOfGas::InjectFuel { .. } => unreachable!(),
        }
    }
}

impl Default for Store {
    fn default() -> Store {
        Store::new(&Engine::default())
    }
}

impl fmt::Debug for Store {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let inner = &*self.inner as *const StoreInner;
        f.debug_struct("Store").field("inner", &inner).finish()
    }
}

impl Drop for StoreInner {
    fn drop(&mut self) {
        let allocator = self.engine.config().instance_allocator();
        for instance in self.instances.borrow().iter() {
            unsafe {
                if instance.use_default_allocator {
                    self.engine
                        .config()
                        .default_instance_allocator
                        .deallocate(&instance.handle);
                } else {
                    allocator.deallocate(&instance.handle);
                }
            }
        }
    }
}

/// A threadsafe handle used to interrupt instances executing within a
/// particular `Store`.
///
/// This structure is created by the [`Store::interrupt_handle`] method.
pub struct InterruptHandle {
    interrupts: Arc<VMInterrupts>,
}

// The `VMInterrupts` type is a pod-type with no destructor, and we only access
// `interrupts` from other threads, so add in these trait impls which are
// otherwise not available due to the `fuel_consumed` variable in
// `VMInterrupts`.
unsafe impl Send for InterruptHandle {}
unsafe impl Sync for InterruptHandle {}

impl InterruptHandle {
    /// Flags that execution within this handle's original [`Store`] should be
    /// interrupted.
    ///
    /// This will not immediately interrupt execution of wasm modules, but
    /// rather it will interrupt wasm execution of loop headers and wasm
    /// execution of function entries. For more information see
    /// [`Store::interrupt_handle`].
    pub fn interrupt(&self) {
        self.interrupts.interrupt()
    }
}

// Wrapper struct to implement hash/equality based on the pointer value of the
// `Arc` in question.
struct ArcModuleCode(Arc<ModuleCode>);

impl PartialEq for ArcModuleCode {
    fn eq(&self, other: &ArcModuleCode) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for ArcModuleCode {}

impl Hash for ArcModuleCode {
    fn hash<H: Hasher>(&self, hasher: &mut H) {
        Arc::as_ptr(&self.0).hash(hasher)
    }
}

struct Reset<'a, T: Copy>(&'a Cell<T>, T);

impl<T: Copy> Drop for Reset<'_, T> {
    fn drop(&mut self) {
        self.0.set(self.1);
    }
}
