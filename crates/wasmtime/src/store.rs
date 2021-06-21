use crate::{module::ModuleRegistry, Engine, Module, Trap};
use anyhow::{bail, Result};
use std::collections::HashMap;
use std::convert::TryFrom;
use std::error::Error;
use std::fmt;
use std::marker;
use std::mem::ManuallyDrop;
use std::ops::{Deref, DerefMut};
use std::sync::Arc;
use wasmtime_runtime::{
    InstanceAllocationRequest, InstanceAllocator, InstanceHandle, ModuleInfo,
    OnDemandInstanceAllocator, SignalHandler, VMCallerCheckedAnyfunc, VMContext, VMExternRef,
    VMExternRefActivationsTable, VMInterrupts, VMSharedSignatureIndex, VMTrampoline,
};

mod context;
pub use self::context::*;
mod data;
pub use self::data::*;
#[cfg(feature = "async")]
mod future;
#[cfg(feature = "async")]
pub use self::future::{on_fiber, WasmtimeFuture};

/// A [`Store`] is a collection of WebAssembly instances and host-defined state.
///
/// All WebAssembly instances and items will be attached to and refer to a
/// [`Store`]. For example instances, functions, globals, and tables are all
/// attached to a [`Store`]. Instances are created by instantiating a
/// [`Module`](crate::Module) within a [`Store`].
///
/// A [`Store`] is intended to be a short-lived object in a program. No form
/// of GC is implemented at this time so once an instance is created within a
/// [`Store`] it will not be deallocated until the [`Store`] itself is dropped.
/// This makes [`Store`] unsuitable for creating an unbounded number of
/// instances in it because [`Store`] will never release this memory. It's
/// recommended to have a [`Store`] correspond roughly to the liftime of a "main
/// instance" that an embedding is interested in executing.
///
/// ## Type parameter `T`
///
/// Each [`Store`] has a type parameter `T` associated with it. This `T`
/// represents state defined by the host. This state will be accessible through
/// the [`Caller`](crate::Caller) type that host-defined functions get access
/// to. This `T` is suitable for storing `Store`-specific information which
/// imported functions may want access to.
///
/// The data `T` can be accessed through methods like [`Store::data`] and
/// [`Store::data_mut`].
///
/// ## Stores, contexts, oh my
///
/// Most methods in Wasmtime take something of the form
/// [`AsContext`](crate::AsContext) or [`AsContextMut`](crate::AsContextMut) as
/// the first argument. These two traits allow ergonomically passing in the
/// context you currently have to any method. The primary two sources of
/// contexts are:
///
/// * `Store<T>`
/// * `Caller<'_, T>`
///
/// corresponding to what you create and what you have access to in a host
/// function. You can also explicitly acquire a [`StoreContext`] or
/// [`StoreContextMut`] and pass that around as well.
///
/// Note that all methods on [`Store`] are mirrored onto [`StoreContext`],
/// [`StoreContextMut`], and [`Caller`](crate::Caller). This way no matter what
/// form of context you have you can call various methods, create objects, etc.
///
/// ## Stores and `Default`
///
/// You can create a store with default configuration settings using
/// `Store::default()`. This will create a brand new [`Engine`] with default
/// ocnfiguration (see [`Config`](crate::Config) for more information).
pub struct Store<T> {
    // for comments about `ManuallyDrop`, see `Store::into_data`
    inner: ManuallyDrop<Box<StoreInner<T>>>,
}

pub struct StoreInner<T> {
    // This `StoreInner<T>` structure has references to itself. These aren't
    // immediately evident, however, so we need to tell the compiler that it
    // contains self-references. This notably suppresses `noalias` annotations
    // when this shows up in compiled code because types of this structure do
    // indeed alias itself. The best example of this is `StoreOpaque` which
    // contains a `&mut StoreInner` and a `*mut dyn Store` which are actually
    // the same pointer, indeed aliasing!
    //
    // It's somewhat unclear to me at this time if this is 100% sufficient to
    // get all the right codegen in all the right places. For example does
    // `Store` need to internally contain a `Pin<Box<StoreInner<T>>>`? Do the
    // contexts need to contain `Pin<&mut StoreInner<T>>`? I'm not familiar
    // enough with `Pin` to understand if it's appropriate here (we do, for
    // example want to allow movement in and out of `data: T`, just not movement
    // of most of the other members). It's also not clear if using `Pin` in a
    // few places buys us much other than a bunch of `unsafe` that we already
    // sort of hand-wave away.
    //
    // In any case this seems like a good mid-ground for now where we're at
    // least telling the compiler something about all the aliasing happening
    // within a `Store`.
    _marker: marker::PhantomPinned,
    inner: StoreInnermost,
    limiter: Option<Box<dyn FnMut(&mut T) -> &mut (dyn crate::ResourceLimiter) + Send + Sync>>,
    entering_native_hook: Option<Box<dyn FnMut(&mut T) -> Result<(), crate::Trap> + Send + Sync>>,
    exiting_native_hook: Option<Box<dyn FnMut(&mut T) -> Result<(), crate::Trap> + Send + Sync>>,
    // for comments about `ManuallyDrop`, see `Store::into_data`
    data: ManuallyDrop<T>,
}

impl<T> Deref for StoreInner<T> {
    type Target = StoreInnermost;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}
impl<T> DerefMut for StoreInner<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

// I apologize for the convoluted structure and the terrible naming of this struct.
// This exists so that most of wasmtime can be monomorphic on StoreInnermost, without
// having to care about the generic in StoreInner<T>.
pub struct StoreInnermost {
    engine: Engine,
    interrupts: Arc<VMInterrupts>,
    instances: Vec<StoreInstance>,
    signal_handler: Option<Box<SignalHandler<'static>>>,
    externref_activations_table: VMExternRefActivationsTable,
    modules: ModuleRegistry,
    host_trampolines: HashMap<VMSharedSignatureIndex, VMTrampoline>,
    // Numbers of resources instantiated in this store, and their limits
    instance_count: usize,
    instance_limit: usize,
    memory_count: usize,
    memory_limit: usize,
    table_count: usize,
    table_limit: usize,
    /// An adjustment to add to the fuel consumed value in `interrupts` above
    /// to get the true amount of fuel consumed.
    fuel_adj: i64,
    #[cfg(feature = "async")]
    async_state: future::AsyncState,
    out_of_gas_behavior: OutOfGas,
    store_data: StoreData,
    default_callee: InstanceHandle,
}

/// Used to associate instances with the store.
///
/// This is needed to track if the instance was allocated explicitly with the on-demand
/// instance allocator.
struct StoreInstance {
    handle: InstanceHandle,
    // Stores whether or not to use the on-demand allocator to deallocate the instance
    ondemand: bool,
}

#[derive(Copy, Clone)]
enum OutOfGas {
    Trap,
    InjectFuel {
        injection_count: u32,
        fuel_to_inject: u64,
    },
}

impl<T> Store<T> {
    /// Creates a new [`Store`] to be associated with the given [`Engine`] and
    /// `data` provided.
    ///
    /// The created [`Store`] will place no additional limits on the size of
    /// linear memories or tables at runtime. Linear memories and tables will
    /// be allowed to grow to any upper limit specified in their definitions.
    /// The store will limit the number of instances, linear memories, and
    /// tables created to 10,000. This can be overridden with the
    /// [`Store::limiter`] configuration method.
    pub fn new(engine: &Engine, data: T) -> Self {
        let finished_functions = &Default::default();
        // Wasmtime uses the callee argument to host functions to learn about
        // the original pointer to the `Store` itself, allowing it to
        // reconstruct a `StoreContextMut<T>`. When we initially call a `Func`,
        // however, there's no "callee" to provide. To fix this we allocate a
        // single "default callee" for the entire `Store`. This is then used as
        // part of `Func::call` to guarantee that the `callee: *mut VMContext`
        // is never null.
        let default_callee = unsafe {
            OnDemandInstanceAllocator::default()
                .allocate(InstanceAllocationRequest {
                    host_state: Box::new(()),
                    finished_functions,
                    shared_signatures: None.into(),
                    imports: Default::default(),
                    module: Arc::new(wasmtime_environ::Module::default()),
                    store: None,
                })
                .expect("failed to allocate default callee")
        };
        let mut inner = Box::new(StoreInner {
            _marker: marker::PhantomPinned,
            inner: StoreInnermost {
                engine: engine.clone(),
                interrupts: Default::default(),
                instances: Vec::new(),
                signal_handler: None,
                externref_activations_table: VMExternRefActivationsTable::new(),
                modules: ModuleRegistry::default(),
                host_trampolines: HashMap::default(),
                instance_count: 0,
                instance_limit: wasmtime_runtime::DEFAULT_INSTANCE_LIMIT,
                memory_count: 0,
                memory_limit: wasmtime_runtime::DEFAULT_MEMORY_LIMIT,
                table_count: 0,
                table_limit: wasmtime_runtime::DEFAULT_TABLE_LIMIT,
                fuel_adj: 0,
                #[cfg(feature = "async")]
                async_state: future::AsyncState::new(),
                out_of_gas_behavior: OutOfGas::Trap,
                store_data: StoreData::new(),
                default_callee,
            },
            limiter: None,
            entering_native_hook: None,
            exiting_native_hook: None,
            data: ManuallyDrop::new(data),
        });

        // Once we've actually allocated the store itself we can configure the
        // trait object pointer of the default callee.
        let store = StoreContextMut(&mut *inner).opaque().traitobj;
        unsafe {
            inner.default_callee.set_store(store);
        }
        Self {
            inner: ManuallyDrop::new(inner),
        }
    }

    /// Access the underlying data owned by this `Store`.
    #[inline]
    pub fn data(&self) -> &T {
        self.inner.data()
    }

    /// Access the underlying data owned by this `Store`.
    #[inline]
    pub fn data_mut(&mut self) -> &mut T {
        self.inner.data_mut()
    }

    /// Consumes this [`Store`], destroying it, and returns the underlying data.
    pub fn into_data(mut self) -> T {
        // This is an unsafe operation because we want to avoid having a runtime
        // check or boolean for whether the data is actually contained within a
        // `Store`. The data itself is stored as `ManuallyDrop` since we're
        // manually managing the memory here, and there's also a `ManuallyDrop`
        // around the `Box<StoreInner<T>>`. The way this works though is a bit
        // tricky, so here's how things get dropped appropriately:
        //
        // * When a `Store<T>` is normally dropped, the custom destructor for
        //   `Store<T>` will drop `T`, then the `self.inner` field. The
        //   rustc-glue destructor runs for `Box<StoreInner<T>>` which drops
        //   `StoreInner<T>`. This cleans up all internal fields and doesn't
        //   touch `T` because it's wrapped in `ManuallyDrop`.
        //
        // * When calling this method we skip the top-level destructor for
        //   `Store<T>` with `mem::forget`. This skips both the destructor for
        //   `T` and the destructor for `StoreInner<T>`. We do, however, run the
        //   destructor for `Box<StoreInner<T>>` which, like above, will skip
        //   the destructor for `T` since it's `ManuallyDrop`.
        //
        // In both cases all the other fields of `StoreInner<T>` should all get
        // dropped, and the manual management of destructors is basically
        // between this method and `Drop for Store<T>`. Note that this also
        // means that `Drop for StoreInner<T>` cannot access `self.data`, so
        // there is a comment indicating this as well.
        unsafe {
            let mut inner = ManuallyDrop::take(&mut self.inner);
            std::mem::forget(self);
            ManuallyDrop::take(&mut inner.data)
        }
    }

    /// Configures the [`ResourceLimiter`](crate::ResourceLimiter) used to limit
    /// resource creation within this [`Store`].
    ///
    /// Note that this limiter is only used to limit the creation/growth of
    /// resources in the future, this does not retroactively attempt to apply
    /// limits to the [`Store`].
    pub fn limiter(
        &mut self,
        mut limiter: impl FnMut(&mut T) -> &mut (dyn crate::ResourceLimiter) + Send + Sync + 'static,
    ) {
        // Apply the limits on instances, tables, and memory given by the limiter:
        let inner = &mut self.inner;
        let (instance_limit, table_limit, memory_limit) = {
            let l = limiter(&mut inner.data);
            (l.instances(), l.tables(), l.memories())
        };
        let innermost = &mut inner.inner;
        innermost.instance_limit = instance_limit;
        innermost.table_limit = table_limit;
        innermost.memory_limit = memory_limit;

        // Save the limiter accessor function:
        inner.limiter = Some(Box::new(limiter));
    }

    /// Configure a function that runs each time WebAssembly code running on this [`Store`] calls
    /// into native code.
    ///
    /// This function may return a [`Trap`], which terminates execution.
    pub fn entering_native_code_hook(
        &mut self,
        hook: impl FnMut(&mut T) -> Result<(), Trap> + Send + Sync + 'static,
    ) {
        self.inner.entering_native_hook = Some(Box::new(hook));
    }

    /// Configure a function that runs before native code running on this [`Store`] returns to
    /// WebAssembly code.
    ///
    /// This function may return a [`Trap`], which terminates execution.
    pub fn exiting_native_code_hook(
        &mut self,
        hook: impl FnMut(&mut T) -> Result<(), Trap> + Send + Sync + 'static,
    ) {
        self.inner.exiting_native_hook = Some(Box::new(hook));
    }

    /// Returns the [`Engine`] that this store is associated with.
    pub fn engine(&self) -> &Engine {
        self.inner.engine()
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
    /// let engine = Engine::new(Config::new().interruptable(true))?;
    /// let mut store = Store::new(&engine, ());
    /// let interrupt_handle = store.interrupt_handle()?;
    ///
    /// // Compile and instantiate a small example with an infinite loop.
    /// let module = Module::new(&engine, r#"
    ///     (func (export "run") (loop br 0))
    /// "#)?;
    /// let instance = Instance::new(&mut store, &module, &[])?;
    /// let run = instance.get_typed_func::<(), (), _>(&mut store, "run")?;
    ///
    /// // Spin up a thread to send us an interrupt in a second
    /// std::thread::spawn(move || {
    ///     std::thread::sleep(std::time::Duration::from_secs(1));
    ///     interrupt_handle.interrupt();
    /// });
    ///
    /// let trap = run.call(&mut store, ()).unwrap_err();
    /// assert!(trap.to_string().contains("wasm trap: interrupt"));
    /// # Ok(())
    /// # }
    /// ```
    pub fn interrupt_handle(&self) -> Result<InterruptHandle> {
        self.inner.interrupt_handle()
    }

    /// Perform garbage collection of `ExternRef`s.
    ///
    /// Note that it is not required to actively call this function. GC will
    /// automatically happen when internal buffers fill up. This is provided if
    /// fine-grained control over the GC is desired.
    pub fn gc(&mut self) {
        self.inner.gc()
    }

    /// Returns the amount of fuel consumed by this store's execution so far.
    ///
    /// If fuel consumption is not enabled via
    /// [`Config::consume_fuel`](crate::Config::consume_fuel) then this
    /// function will return `None`. Also note that fuel, if enabled, must be
    /// originally configured via [`Store::add_fuel`].
    pub fn fuel_consumed(&self) -> Option<u64> {
        self.inner.fuel_consumed()
    }

    /// Adds fuel to this [`Store`] for wasm to consume while executing.
    ///
    /// For this method to work fuel consumption must be enabled via
    /// [`Config::consume_fuel`](crate::Config::consume_fuel). By default a
    /// [`Store`] starts with 0 fuel for wasm to execute with (meaning it will
    /// immediately trap). This function must be called for the store to have
    /// some fuel to allow WebAssembly to execute.
    ///
    /// Most WebAssembly instructions consume 1 unit of fuel. Some
    /// instructions, such as `nop`, `drop`, `block`, and `loop`, consume 0
    /// units, as any execution cost associated with them involves other
    /// instructions which do consume fuel.
    ///
    /// Note that at this time when fuel is entirely consumed it will cause
    /// wasm to trap. More usages of fuel are planned for the future.
    ///
    /// # Panics
    ///
    /// This function will panic if the store's [`Config`](crate::Config) did
    /// not have fuel consumption enabled.
    pub fn add_fuel(&mut self, fuel: u64) -> Result<()> {
        self.inner.add_fuel(fuel)
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
    pub fn out_of_fuel_trap(&mut self) {
        self.inner.out_of_fuel_trap()
    }

    /// Configures a [`Store`] to yield execution of async WebAssembly code
    /// periodically.
    ///
    /// When a [`Store`] is configured to consume fuel with
    /// [`Config::consume_fuel`](crate::Config::consume_fuel) this method will
    /// configure what happens when fuel runs out. Specifically executing
    /// WebAssembly will be suspended and control will be yielded back to the
    /// caller. This is only suitable with use of a store associated with an [async
    /// config](crate::Config::async_support) because only then are futures used and yields
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
    /// This method will panic if it is not called on a store associated with an [async
    /// config](crate::Config::async_support).
    pub fn out_of_fuel_async_yield(&mut self, injection_count: u32, fuel_to_inject: u64) {
        self.inner
            .out_of_fuel_async_yield(injection_count, fuel_to_inject)
    }
}

impl<'a, T> StoreContext<'a, T> {
    pub(crate) fn async_support(&self) -> bool {
        self.0.async_support()
    }

    /// Returns the underlying [`Engine`] this store is connected to.
    pub fn engine(&self) -> &Engine {
        self.0.engine()
    }

    /// Returns an [`InterruptHandle`] to interrupt wasm execution.
    ///
    /// See [`Store::interrupt_handle`] for more information.
    pub fn interrupt_handle(&self) -> Result<InterruptHandle> {
        self.0.interrupt_handle()
    }

    /// Access the underlying data owned by this `Store`.
    ///
    /// Same as [`Store::data`].
    pub fn data(&self) -> &T {
        self.0.data()
    }

    /// Returns the fuel consumed by this store.
    ///
    /// For more information see [`Store::fuel_consumed`].
    pub fn fuel_consumed(&self) -> Option<u64> {
        self.0.fuel_consumed()
    }
}

impl<'a, T> StoreContextMut<'a, T> {
    /// Access the underlying data owned by this `Store`.
    ///
    /// Same as [`Store::data`].
    pub fn data(&self) -> &T {
        self.0.data()
    }

    /// Access the underlying data owned by this `Store`.
    ///
    /// Same as [`Store::data_mut`].
    pub fn data_mut(&mut self) -> &mut T {
        self.0.data_mut()
    }

    /// Returns the underlying [`Engine`] this store is connected to.
    pub fn engine(&self) -> &Engine {
        self.0.engine()
    }

    /// Returns an [`InterruptHandle`] to interrupt wasm execution.
    ///
    /// See [`Store::interrupt_handle`] for more information.
    pub fn interrupt_handle(&self) -> Result<InterruptHandle> {
        self.0.interrupt_handle()
    }

    /// Perform garbage collection of `ExternRef`s.
    ///
    /// Same as [`Store::gc`].
    pub fn gc(&mut self) {
        self.0.gc()
    }

    /// Returns the fuel consumed by this store.
    ///
    /// For more information see [`Store::fuel_consumed`].
    pub fn fuel_consumed(&self) -> Option<u64> {
        self.0.fuel_consumed()
    }

    /// Inject more fuel into this store to be consumed when executing wasm code.
    ///
    /// For more information see [`Store::add_fuel`]
    pub fn add_fuel(&mut self, fuel: u64) -> Result<()> {
        self.0.add_fuel(fuel)
    }

    /// Configures this `Store` to trap whenever fuel runs out.
    ///
    /// For more information see [`Store::out_of_fuel_trap`]
    pub fn out_of_fuel_trap(&mut self) {
        self.0.out_of_fuel_trap()
    }

    /// Configures this `Store` to yield while executing futures whenever fuel
    /// runs out.
    ///
    /// For more information see [`Store::out_of_fuel_async_yield`]
    pub fn out_of_fuel_async_yield(&mut self, injection_count: u32, fuel_to_inject: u64) {
        self.0
            .out_of_fuel_async_yield(injection_count, fuel_to_inject)
    }
}

impl<T> StoreInner<T> {
    #[inline]
    fn data(&self) -> &T {
        &self.data
    }

    #[inline]
    fn data_mut(&mut self) -> &mut T {
        &mut self.data
    }

    pub fn limiter(&mut self) -> Option<&mut dyn crate::limits::ResourceLimiter> {
        let accessor = self.limiter.as_mut()?;
        Some(accessor(&mut self.data))
    }

    pub fn entering_native_hook(&mut self) -> Result<(), Trap> {
        if let Some(hook) = &mut self.entering_native_hook {
            hook(&mut self.data)
        } else {
            Ok(())
        }
    }

    pub fn exiting_native_hook(&mut self) -> Result<(), Trap> {
        if let Some(hook) = &mut self.exiting_native_hook {
            hook(&mut self.data)
        } else {
            Ok(())
        }
    }
}

impl StoreInnermost {
    pub fn bump_resource_counts(&mut self, module: &Module) -> Result<()> {
        fn bump(slot: &mut usize, max: usize, amt: usize, desc: &str) -> Result<()> {
            let new = slot.saturating_add(amt);
            if new > max {
                bail!(
                    "resource limit exceeded: {} count too high at {}",
                    desc,
                    new
                );
            }
            *slot = new;
            Ok(())
        }

        let module = module.env_module();
        let memories = module.memory_plans.len() - module.num_imported_memories;
        let tables = module.table_plans.len() - module.num_imported_tables;

        bump(&mut self.instance_count, self.instance_limit, 1, "instance")?;
        bump(
            &mut self.memory_count,
            self.memory_limit,
            memories,
            "memory",
        )?;
        bump(&mut self.table_count, self.table_limit, tables, "table")?;

        Ok(())
    }
    #[inline]
    pub fn async_support(&self) -> bool {
        cfg!(feature = "async") && self.engine().config().async_support
    }

    #[inline]
    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    pub fn store_data(&self) -> &StoreData {
        &self.store_data
    }

    pub fn store_data_mut(&mut self) -> &mut StoreData {
        &mut self.store_data
    }

    pub fn register_host_trampoline(
        &mut self,
        idx: VMSharedSignatureIndex,
        trampoline: VMTrampoline,
    ) {
        self.host_trampolines.insert(idx, trampoline);
    }

    pub fn interrupt_handle(&self) -> Result<InterruptHandle> {
        if self.engine.config().tunables.interruptable {
            Ok(InterruptHandle {
                interrupts: self.interrupts.clone(),
            })
        } else {
            bail!("interrupts aren't enabled for this `Store`")
        }
    }

    #[inline]
    pub(crate) fn modules_mut(&mut self) -> &mut ModuleRegistry {
        &mut self.modules
    }

    pub unsafe fn add_instance(&mut self, handle: InstanceHandle, ondemand: bool) -> InstanceId {
        self.instances.push(StoreInstance {
            handle: handle.clone(),
            ondemand,
        });
        InstanceId(self.instances.len() - 1)
    }

    pub fn instance(&self, id: InstanceId) -> &InstanceHandle {
        &self.instances[id.0].handle
    }

    pub fn instance_mut(&mut self, id: InstanceId) -> &mut InstanceHandle {
        &mut self.instances[id.0].handle
    }

    #[cfg_attr(not(target_os = "linux"), allow(dead_code))] // not used on all platforms
    pub fn set_signal_handler(&mut self, handler: Option<Box<SignalHandler<'static>>>) {
        self.signal_handler = handler;
    }

    #[inline]
    pub fn interrupts(&self) -> &VMInterrupts {
        &self.interrupts
    }

    #[inline]
    pub fn externref_activations_table(&mut self) -> &mut VMExternRefActivationsTable {
        &mut self.externref_activations_table
    }

    pub fn gc(&mut self) {
        // For this crate's API, we ensure that `set_stack_canary` invariants
        // are upheld for all host-->Wasm calls.
        unsafe { wasmtime_runtime::gc(&self.modules, &mut self.externref_activations_table) }
    }

    pub fn lookup_trampoline(&self, anyfunc: &VMCallerCheckedAnyfunc) -> VMTrampoline {
        // Look up the trampoline with the store's trampolines (from `Func`).
        if let Some(trampoline) = self.host_trampolines.get(&anyfunc.type_index) {
            return *trampoline;
        }

        // Look up the trampoline with the registered modules
        if let Some(trampoline) = self.modules.lookup_trampoline(anyfunc) {
            return trampoline;
        }

        panic!("trampoline missing")
    }

    pub fn fuel_consumed(&self) -> Option<u64> {
        if !self.engine.config().tunables.consume_fuel {
            return None;
        }
        let consumed = unsafe { *self.interrupts.fuel_consumed.get() };
        Some(u64::try_from(self.fuel_adj + consumed).unwrap())
    }

    fn out_of_fuel_trap(&mut self) {
        self.out_of_gas_behavior = OutOfGas::Trap;
    }

    fn out_of_fuel_async_yield(&mut self, injection_count: u32, fuel_to_inject: u64) {
        assert!(
            self.async_support(),
            "cannot use `out_of_fuel_async_yield` without enabling async support in the config"
        );
        self.out_of_gas_behavior = OutOfGas::InjectFuel {
            injection_count,
            fuel_to_inject,
        };
    }

    fn add_fuel(&mut self, fuel: u64) -> Result<()> {
        anyhow::ensure!(
            self.engine().config().tunables.consume_fuel,
            "fuel is not configured in this store"
        );

        // Fuel is stored as an i64, so we need to cast it. If the provided fuel
        // value overflows that just assume that i64::max will suffice. Wasm
        // execution isn't fast enough to burn through i64::max fuel in any
        // reasonable amount of time anyway.
        let fuel = i64::try_from(fuel).unwrap_or(i64::max_value());
        let adj = self.fuel_adj;
        let consumed_ptr = unsafe { &mut *self.interrupts.fuel_consumed.get() };

        match (consumed_ptr.checked_sub(fuel), adj.checked_add(fuel)) {
            // If we succesfully did arithmetic without overflowing then we can
            // just update our fields.
            (Some(consumed), Some(adj)) => {
                self.fuel_adj = adj;
                *consumed_ptr = consumed;
            }

            // Otherwise something overflowed. Make sure that we preserve the
            // amount of fuel that's already consumed, but otherwise assume that
            // we were given infinite fuel.
            _ => {
                self.fuel_adj = i64::max_value();
                *consumed_ptr = (*consumed_ptr + adj) - i64::max_value();
            }
        }

        Ok(())
    }

    #[inline]
    pub fn signal_handler(&self) -> Option<*const SignalHandler<'static>> {
        let handler = self.signal_handler.as_ref()?;
        Some(&**handler as *const _)
    }

    #[inline]
    pub fn vminterrupts(&self) -> *mut VMInterrupts {
        &*self.interrupts as *const VMInterrupts as *mut VMInterrupts
    }

    pub unsafe fn insert_vmexternref(&mut self, r: VMExternRef) {
        self.externref_activations_table
            .insert_with_gc(r, &self.modules)
    }

    #[inline]
    pub fn default_callee(&self) -> *mut VMContext {
        self.default_callee.vmctx_ptr()
    }
}

unsafe impl<T> wasmtime_runtime::Store for StoreInner<T> {
    fn vminterrupts(&self) -> *mut VMInterrupts {
        <StoreInnermost>::vminterrupts(self)
    }

    fn externref_activations_table(
        &mut self,
    ) -> (
        &mut VMExternRefActivationsTable,
        &dyn wasmtime_runtime::ModuleInfoLookup,
    ) {
        let inner = &mut self.inner;
        (&mut inner.externref_activations_table, &inner.modules)
    }

    fn limiter(&mut self) -> Option<&mut dyn wasmtime_runtime::ResourceLimiter> {
        <Self>::limiter(self)
    }

    fn out_of_gas(&mut self) -> Result<(), Box<dyn Error + Send + Sync>> {
        return match &mut self.out_of_gas_behavior {
            OutOfGas::Trap => Err(Box::new(OutOfGasError)),
            #[cfg(feature = "async")]
            OutOfGas::InjectFuel {
                injection_count,
                fuel_to_inject,
            } => {
                if *injection_count == 0 {
                    return Err(Box::new(OutOfGasError));
                }
                *injection_count -= 1;
                let fuel = *fuel_to_inject;
                self.out_of_gas_yield(fuel)?;
                Ok(())
            }
            #[cfg(not(feature = "async"))]
            OutOfGas::InjectFuel { .. } => unreachable!(),
        };

        #[derive(Debug)]
        struct OutOfGasError;

        impl fmt::Display for OutOfGasError {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("all fuel consumed by WebAssembly")
            }
        }

        impl std::error::Error for OutOfGasError {}
    }
}

impl<T: Default> Default for Store<T> {
    fn default() -> Store<T> {
        Store::new(&Engine::default(), T::default())
    }
}

impl<T: fmt::Debug> fmt::Debug for Store<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let inner = &**self.inner as *const StoreInner<T>;
        f.debug_struct("Store")
            .field("inner", &inner)
            .field("data", &self.inner.data)
            .finish()
    }
}

impl<T> Drop for Store<T> {
    fn drop(&mut self) {
        // for documentation on this `unsafe`, see `into_data`.
        unsafe {
            ManuallyDrop::drop(&mut self.inner.data);
            ManuallyDrop::drop(&mut self.inner);
        }
    }
}

impl Drop for StoreInnermost {
    fn drop(&mut self) {
        // NB it's important that this destructor does not access `self.data`.
        // That is deallocated by `Drop for Store<T>` above.

        let allocator = self.engine.allocator();
        unsafe {
            let ondemand = OnDemandInstanceAllocator::default();
            for instance in self.instances.iter() {
                if instance.ondemand {
                    ondemand.deallocate(&instance.handle);
                } else {
                    allocator.deallocate(&instance.handle);
                }
            }
            ondemand.deallocate(&self.default_callee);
        }
    }
}

impl wasmtime_runtime::ModuleInfoLookup for ModuleRegistry {
    fn lookup(&self, pc: usize) -> Option<Arc<dyn ModuleInfo>> {
        self.lookup_module(pc)
    }
}

/// A threadsafe handle used to interrupt instances executing within a
/// particular `Store`.
///
/// This structure is created by the [`Store::interrupt_handle`] method.
#[derive(Debug)]
pub struct InterruptHandle {
    interrupts: Arc<VMInterrupts>,
}

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
