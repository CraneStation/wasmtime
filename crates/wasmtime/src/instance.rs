use crate::linker::Definition;
use crate::signatures::SignatureCollection;
use crate::store::{InstanceId, StoreData, StoreOpaque, Stored};
use crate::types::matching;
use crate::{
    AsContext, AsContextMut, Engine, Export, Extern, ExternType, Func, Global, InstanceType,
    Memory, Module, StoreContextMut, Table, Trap, TypedFunc,
};
use anyhow::{anyhow, bail, Context, Error, Result};
use std::mem;
use std::sync::Arc;
use wasmtime_environ::entity::PrimaryMap;
use wasmtime_environ::wasm::{
    EntityIndex, EntityType, FuncIndex, GlobalIndex, InstanceIndex, MemoryIndex, ModuleIndex,
    TableIndex,
};
use wasmtime_environ::Initializer;
use wasmtime_jit::TypeTables;
use wasmtime_runtime::{
    Imports, InstanceAllocationRequest, InstantiationError, VMContext, VMFunctionBody,
    VMFunctionImport, VMGlobalImport, VMMemoryImport, VMTableImport,
};

/// An instantiated WebAssembly module.
///
/// This type represents the instantiation of a [`Module`]. Once instantiated
/// you can access the [`exports`](Instance::exports) which are of type
/// [`Extern`] and provide the ability to call functions, set globals, read
/// memory, etc. When interacting with any wasm code you'll want to make an
/// [`Instance`] to call any code or execute anything.
///
/// Instances are owned by a [`Store`](crate::Store) which is passed in at
/// creation time. It's recommended to create instances with
/// [`Linker::instantiate`](crate::Linker::instantiate) or similar
/// [`Linker`](crate::Linker) methods, but a more low-level constructor is also
/// available as [`Instance::new`].
#[derive(Copy, Clone, Debug)]
#[repr(transparent)]
pub struct Instance(Stored<InstanceData>);

pub(crate) enum InstanceData {
    /// This variant is used for instances created through instantiation of a
    /// module, e.g. `Instance::new` or various linker methods.
    Instantiated {
        /// The id of the instance within the store, used to find the original
        /// `InstanceHandle`.
        id: InstanceId,
        /// A lazily-populated list of exports of this instance. The order of
        /// exports here matches the order of the exports in the the original
        /// module.
        exports: Vec<Option<Extern>>,
        /// The type information of the module that this was instantiated with.
        types: Arc<TypeTables>,
        signatures: Arc<SignatureCollection>,
    },

    /// This variant is used for synthetically created instances via `Linker`
    /// APIs. This is only used for the module linking proposal at this time.
    Synthetic(Arc<indexmap::IndexMap<String, Extern>>),
}

impl Instance {
    /// Creates a new [`Instance`] from the previously compiled [`Module`] and
    /// list of `imports` specified.
    ///
    /// This method instantiates the `module` provided with the `imports`,
    /// following the procedure in the [core specification][inst] to
    /// instantiate. Instantiation can fail for a number of reasons (many
    /// specified below), but if successful the `start` function will be
    /// automatically run (if specified in the `module`) and then the
    /// [`Instance`] will be returned.
    ///
    /// Per the WebAssembly spec, instantiation includes running the module's
    /// start function, if it has one (not to be confused with the `_start`
    /// function, which is not run).
    ///
    /// Note that this is a low-level function that just performs an
    /// instantiation. See the [`Linker`](crate::Linker) struct for an API which
    /// provides a convenient way to link imports and provides automatic Command
    /// and Reactor behavior.
    ///
    /// ## Providing Imports
    ///
    /// The entries in the list of `imports` are intended to correspond 1:1
    /// with the list of imports returned by [`Module::imports`]. Before
    /// calling [`Instance::new`] you'll want to inspect the return value of
    /// [`Module::imports`] and, for each import type, create an [`Extern`]
    /// which corresponds to that type.  These [`Extern`] values are all then
    /// collected into a list and passed to this function.
    ///
    /// Note that this function is intentionally relatively low level. For an
    /// easier time passing imports by doing name-based resolution it's
    /// recommended to instead use the [`Linker`](crate::Linker) type.
    ///
    /// ## Errors
    ///
    /// This function can fail for a number of reasons, including, but not
    /// limited to:
    ///
    /// * The number of `imports` provided doesn't match the number of imports
    ///   returned by the `module`'s [`Module::imports`] method.
    /// * The type of any [`Extern`] doesn't match the corresponding
    ///   [`ExternType`] entry that it maps to.
    /// * The `start` function in the instance, if present, traps.
    /// * Module/instance resource limits are exceeded.
    ///
    /// When instantiation fails it's recommended to inspect the return value to
    /// see why it failed, or bubble it upwards. If you'd like to specifically
    /// check for trap errors, you can use `error.downcast::<Trap>()`.
    ///
    /// # Panics
    ///
    /// This function will panic if called with a store associated with a
    /// [`asynchronous config`](crate::Config::async_support). This function
    /// will also panic if any [`Extern`] supplied is not owned by `store`.
    ///
    /// [inst]: https://webassembly.github.io/spec/core/exec/modules.html#exec-instantiation
    /// [`ExternType`]: crate::ExternType
    pub fn new(
        mut store: impl AsContextMut,
        module: &Module,
        imports: &[Extern],
    ) -> Result<Instance, Error> {
        // This unsafety comes from `Instantiator::new` where we must typecheck
        // first, which we are sure to do here.
        let mut cx = store.as_context_mut().opaque();
        let mut i = unsafe {
            typecheck_externs(&mut cx, module, imports)?;
            Instantiator::new(&mut cx, module, ImportSource::Externs(imports))?
        };
        i.run(&mut cx)
    }

    /// Same as [`Instance::new`], except for usage in [asynchronous stores].
    ///
    /// For more details about this function see the documentation on
    /// [`Instance::new`]. The only difference between these two methods is that
    /// this one will asynchronously invoke the wasm start function in case it
    /// calls any imported function which is an asynchronous host function (e.g.
    /// created with [`Func::new_async`](crate::Func::new_async).
    ///
    /// # Panics
    ///
    /// This function will panic if called with a store associated with a
    /// [`synchronous config`](crate::Config::new). This is only compatible with
    /// stores associated with an [`asynchronous
    /// config`](crate::Config::async_support).
    ///
    /// This function will also panic, like [`Instance::new`], if any [`Extern`]
    /// specified does not belong to `store`.
    #[cfg(feature = "async")]
    #[cfg_attr(nightlydoc, doc(cfg(feature = "async")))]
    pub async fn new_async<T>(
        mut store: impl AsContextMut<Data = T>,
        module: &Module,
        imports: &[Extern],
    ) -> Result<Instance>
    where
        T: Send,
    {
        // See `new` for unsafety comments
        let mut instantiator = unsafe {
            let mut cx = store.as_context_mut().opaque();
            typecheck_externs(&mut cx, module, imports)?;
            Instantiator::new(&mut cx, module, ImportSource::Externs(imports))?
        };
        instantiator.run_async(store).await
    }

    pub(crate) fn from_wasmtime(handle: InstanceData, store: &mut StoreOpaque) -> Instance {
        Instance(store.store_data_mut().insert(handle))
    }

    /// Returns the type signature of this instance.
    ///
    /// # Panics
    ///
    /// Panics if `store` does not own this instance.
    pub fn ty(&self, store: impl AsContext) -> InstanceType {
        let store = store.as_context();
        let mut ty = InstanceType::new();
        match &store[self.0] {
            InstanceData::Synthetic(items) => {
                for (name, item) in items.iter() {
                    ty.add_named_export(name, item.ty(&store));
                }
            }
            InstanceData::Instantiated { id, types, .. } => {
                let module = store.0.instance(*id).module();
                for (name, idx) in module.exports.iter() {
                    let export_ty = module.type_of(*idx);
                    ty.add_named_export(name, ExternType::from_wasmtime(types, &export_ty));
                }
            }
        }
        ty
    }

    pub(crate) fn data<'a>(&self, store: &'a StoreData) -> &'a InstanceData {
        &store[self.0]
    }

    pub(crate) fn comes_from_same_store(&self, store: &StoreOpaque) -> bool {
        store.store_data().contains(self.0)
    }

    /// Returns the list of exported items from this [`Instance`].
    ///
    /// # Panics
    ///
    /// Panics if `store` does not own this instance.
    pub fn exports<'a, T: 'a>(
        &'a self,
        store: impl Into<StoreContextMut<'a, T>>,
    ) -> impl ExactSizeIterator<Item = Export<'a>> + 'a {
        self._exports(store.into().opaque())
    }

    fn _exports<'a>(
        &'a self,
        mut store: StoreOpaque<'a>,
    ) -> impl ExactSizeIterator<Item = Export<'a>> + 'a {
        // If this is an `Instantiated` instance then all the `exports` may not
        // be filled in. Fill them all in now if that's the case.
        if let InstanceData::Instantiated { exports, id, .. } = &store[self.0] {
            if exports.iter().any(|e| e.is_none()) {
                let module = Arc::clone(store.instance(*id).module());
                for name in module.exports.keys() {
                    self._get_export(&mut store, name);
                }
            }
        }

        let inner = store.into_inner();
        return match &inner.store_data()[self.0] {
            InstanceData::Synthetic(names) => {
                Either::A(names.iter().map(|(k, v)| Export::new(k, v.clone())))
            }
            InstanceData::Instantiated { exports, id, .. } => {
                let module = inner.instance(*id).module();
                Either::B(
                    module
                        .exports
                        .iter()
                        .zip(exports)
                        .map(|((name, _), export)| Export::new(name, export.clone().unwrap())),
                )
            }
        };

        enum Either<A, B> {
            A(A),
            B(B),
        }

        impl<A, B> Iterator for Either<A, B>
        where
            A: Iterator,
            B: Iterator<Item = A::Item>,
        {
            type Item = A::Item;

            fn next(&mut self) -> Option<A::Item> {
                match self {
                    Either::A(a) => a.next(),
                    Either::B(b) => b.next(),
                }
            }

            fn size_hint(&self) -> (usize, Option<usize>) {
                match self {
                    Either::A(a) => a.size_hint(),
                    Either::B(b) => b.size_hint(),
                }
            }
        }

        impl<A, B> ExactSizeIterator for Either<A, B>
        where
            A: ExactSizeIterator,
            B: ExactSizeIterator<Item = A::Item>,
        {
            fn len(&self) -> usize {
                match self {
                    Either::A(a) => a.len(),
                    Either::B(b) => b.len(),
                }
            }
        }
    }

    /// Looks up an exported [`Extern`] value by name.
    ///
    /// This method will search the module for an export named `name` and return
    /// the value, if found.
    ///
    /// Returns `None` if there was no export named `name`.
    ///
    /// # Panics
    ///
    /// Panics if `store` does not own this instance.
    pub fn get_export(&self, mut store: impl AsContextMut, name: &str) -> Option<Extern> {
        self._get_export(&mut store.as_context_mut().opaque(), name)
    }

    fn _get_export(&self, store: &mut StoreOpaque<'_>, name: &str) -> Option<Extern> {
        match &store[self.0] {
            // Synthetic instances always have their entire list of exports
            // already specified.
            InstanceData::Synthetic(names) => names.get(name).cloned(),

            // Instantiated instances will lazily fill in exports, so we process
            // all that lazy logic here.
            InstanceData::Instantiated { id, exports, .. } => {
                let instance = store.instance(*id);
                let (i, _, index) = instance.module().exports.get_full(name)?;
                if let Some(export) = &exports[i] {
                    return Some(export.clone());
                }
                let item = unsafe {
                    Extern::from_wasmtime_export(instance.lookup_by_declaration(index), store)
                };
                let exports = match &mut store[self.0] {
                    InstanceData::Instantiated { exports, .. } => exports,
                    _ => unreachable!(),
                };
                exports[i] = Some(item.clone());
                Some(item)
            }
        }
    }

    /// Looks up an exported [`Func`] value by name.
    ///
    /// Returns `None` if there was no export named `name`, or if there was but
    /// it wasn't a function.
    ///
    /// # Panics
    ///
    /// Panics if `store` does not own this instance.
    pub fn get_func(&self, store: impl AsContextMut, name: &str) -> Option<Func> {
        self.get_export(store, name)?.into_func()
    }

    /// Looks up an exported [`Func`] value by name and with its type.
    ///
    /// This function is a convenience wrapper over [`Instance::get_func`] and
    /// [`Func::typed`]. For more information see the linked documentation.
    ///
    /// Returns an error if `name` isn't a function export or if the export's
    /// type did not match `Params` or `Results`
    ///
    /// # Panics
    ///
    /// Panics if `store` does not own this instance.
    pub fn get_typed_func<Params, Results, S>(
        &self,
        mut store: S,
        name: &str,
    ) -> Result<TypedFunc<Params, Results>>
    where
        Params: crate::WasmParams,
        Results: crate::WasmResults,
        S: AsContextMut,
    {
        let f = self
            .get_export(store.as_context_mut(), name)
            .and_then(|f| f.into_func())
            .ok_or_else(|| anyhow!("failed to find function export `{}`", name))?;
        Ok(f.typed::<Params, Results, _>(store)?)
    }

    /// Looks up an exported [`Table`] value by name.
    ///
    /// Returns `None` if there was no export named `name`, or if there was but
    /// it wasn't a table.
    ///
    /// # Panics
    ///
    /// Panics if `store` does not own this instance.
    pub fn get_table(&self, store: impl AsContextMut, name: &str) -> Option<Table> {
        self.get_export(store, name)?.into_table()
    }

    /// Looks up an exported [`Memory`] value by name.
    ///
    /// Returns `None` if there was no export named `name`, or if there was but
    /// it wasn't a memory.
    ///
    /// # Panics
    ///
    /// Panics if `store` does not own this instance.
    pub fn get_memory(&self, store: impl AsContextMut, name: &str) -> Option<Memory> {
        self.get_export(store, name)?.into_memory()
    }

    /// Looks up an exported [`Global`] value by name.
    ///
    /// Returns `None` if there was no export named `name`, or if there was but
    /// it wasn't a global.
    ///
    /// # Panics
    ///
    /// Panics if `store` does not own this instance.
    pub fn get_global(&self, store: impl AsContextMut, name: &str) -> Option<Global> {
        self.get_export(store, name)?.into_global()
    }
}

struct Instantiator<'a> {
    in_progress: Vec<ImportsBuilder<'a>>,
    cur: ImportsBuilder<'a>,
}

struct ImportsBuilder<'a> {
    src: ImportSource<'a>,
    functions: PrimaryMap<FuncIndex, VMFunctionImport>,
    tables: PrimaryMap<TableIndex, VMTableImport>,
    memories: PrimaryMap<MemoryIndex, VMMemoryImport>,
    globals: PrimaryMap<GlobalIndex, VMGlobalImport>,
    instances: PrimaryMap<InstanceIndex, Instance>,
    modules: PrimaryMap<ModuleIndex, Module>,
    initializer: usize,
    module: Module,
}

enum ImportSource<'a> {
    Externs(&'a [Extern]),
    Definitions(&'a [Definition]),
    Outer { initializer: usize },
}

impl<'a> Instantiator<'a> {
    /// Creates a new instantiation context used to process all the initializer
    /// directives of a module.
    ///
    /// This doesn't do much work itself beyond setting things up.
    ///
    /// # Unsafety
    ///
    /// This function is unsafe for a few reasons:
    ///
    /// * This assumes that `imports` has already been typechecked and is of the
    ///   appropriate length. It is memory unsafe if the types of `imports` are
    ///   not what `module` expects.
    ///
    /// * The `imports` must be safely able to get inserted into `store`. This
    ///   only applies if `ImportSource::Definitions` is used because this will
    ///   internally call `Definition::to_extern` which requires that any
    ///   host functions in the list were created with an original `T` as the
    ///   store that's being inserted into.
    ///
    /// * The `imports` must all come from the `store` specified.
    unsafe fn new(
        store: &mut StoreOpaque<'_>,
        module: &Module,
        imports: ImportSource<'a>,
    ) -> Result<Instantiator<'a>> {
        if !Engine::same(store.engine(), module.engine()) {
            bail!("cross-`Engine` instantiation is not currently supported");
        }

        Ok(Instantiator {
            in_progress: Vec::new(),
            cur: ImportsBuilder::new(module, imports),
        })
    }

    fn run(&mut self, store: &mut StoreOpaque<'_>) -> Result<Instance, Error> {
        assert!(
            !store.async_support(),
            "cannot use `new` when async support is enabled on the config"
        );

        // NB: this is the same code as `run_async`. It's intentionally
        // small but should be kept in sync (modulo the async bits).
        loop {
            if let Some((instance, start, toplevel)) = self.step(store)? {
                if let Some(start) = start {
                    Instantiator::start_raw(store, instance, start)?;
                }
                if toplevel {
                    break Ok(instance);
                }
            }
        }
    }

    #[cfg(feature = "async")]
    async fn run_async<T>(
        &mut self,
        mut store: impl AsContextMut<Data = T>,
    ) -> Result<Instance, Error>
    where
        T: Send,
    {
        let mut cx = store.as_context_mut();
        assert!(
            cx.0.async_support(),
            "cannot use `new_async` without enabling async support on the config"
        );

        // NB: this is similar code to `run`. It's intentionally
        // small but should be kept in sync (modulo the async bits).
        loop {
            let step = self.step(&mut cx.as_context_mut().opaque())?;
            if let Some((instance, start, toplevel)) = step {
                if let Some(start) = start {
                    crate::store::on_fiber(cx.as_context_mut(), |store| {
                        Instantiator::start_raw(store, instance, start)
                    })
                    .await?;
                }
                if toplevel {
                    break Ok(instance);
                }
            }
        }
    }

    /// Processes the next initializer for the next instance being created
    /// without running any wasm code.
    ///
    /// This function will process module initializers, handling recursive
    /// instantiations of modules for module linking if necessary as well. This
    /// does not actually execute any WebAssembly code, which means that it
    /// will return whenever an instance is created (because its `start`
    /// function may need to be executed).
    ///
    /// If this function returns `None`, then it simply needs to be called
    /// again to execute the next initializer. Otherwise this function has two
    /// return values:
    ///
    /// * The first is the raw handle to the instance that was just created.
    ///   This instance must have its start function executed by the caller.
    /// * The second is an optional list of items to get wrapped up in an
    ///   `Instance`. This is only `Some` for the outermost instance that was
    ///   created. If this is `None` callers need to keep calling this function
    ///   since the instance created was simply for a recursive instance
    ///   defined here.
    fn step(
        &mut self,
        store: &mut StoreOpaque<'_>,
    ) -> Result<Option<(Instance, Option<FuncIndex>, bool)>> {
        if self.cur.initializer == 0 {
            store.bump_resource_counts(&self.cur.module)?;
        }

        // Read the current module's initializer and move forward the
        // initializer pointer as well.
        self.cur.initializer += 1;
        match self
            .cur
            .module
            .env_module()
            .initializers
            .get(self.cur.initializer - 1)
        {
            Some(Initializer::Import { name, field, .. }) => {
                match &mut self.cur.src {
                    // If imports are coming from the runtime-provided list
                    // (e.g. the root module being instantiated) then we
                    // need to typecheck each item here before recording it.
                    //
                    // Note the `unwrap` here should be ok given the validation
                    // above in `Instantiation::new`.
                    ImportSource::Externs(list) => {
                        let (head, remaining) = list.split_first().unwrap();
                        *list = remaining;
                        self.cur.push(head.clone(), store);
                    }
                    ImportSource::Definitions(list) => {
                        let (head, remaining) = list.split_first().unwrap();
                        *list = remaining;
                        // This unsafety is encapsulated with
                        // `Instantiator::new`, documented above.
                        self.cur.push(unsafe { head.to_extern(store) }, store);
                    }

                    // Otherwise if arguments are coming from our outer
                    // instance due to a recursive instantiation then we
                    // look in the previous initializer's mapping of
                    // arguments to figure out where to load the item from.
                    // Note that no typechecking is necessary here due to
                    // validation.
                    ImportSource::Outer { initializer } => {
                        debug_assert!(field.is_none());
                        let outer = self.in_progress.last().unwrap();
                        let args = match &outer.module.env_module().initializers[*initializer] {
                            Initializer::Instantiate { args, .. } => args,
                            _ => unreachable!(),
                        };
                        let index = args.get(name).expect("should be present after validation");
                        match *index {
                            EntityIndex::Global(i) => {
                                self.cur.globals.push(outer.globals[i]);
                            }
                            EntityIndex::Function(i) => {
                                self.cur.functions.push(outer.functions[i]);
                            }
                            EntityIndex::Table(i) => {
                                self.cur.tables.push(outer.tables[i]);
                            }
                            EntityIndex::Memory(i) => {
                                self.cur.memories.push(outer.memories[i]);
                            }
                            EntityIndex::Module(i) => {
                                self.cur.modules.push(outer.modules[i].clone());
                            }
                            EntityIndex::Instance(i) => {
                                self.cur.instances.push(outer.instances[i].clone());
                            }
                        }
                    }
                }
            }

            // Here we lookup our instance handle, find the right export,
            // and then push that item into our own index space. We eschew
            // type-checking since only valid modules should reach this point.
            Some(Initializer::AliasInstanceExport { instance, export }) => {
                let instance = self.cur.instances[*instance];
                let export = instance._get_export(store, export).unwrap();
                self.cur.push(export, store);
            }

            // A recursive instantiation of an instance.
            //
            // The `module` argument is used to create an import builder
            // object, and we specify that the source of imports for the builder is
            // this initializer's position so we can look at the `args` payload
            // later.
            //
            // Once that's set up we save off `self.cur` into
            // `self.in_progress` and start the instantiation of the child
            // instance on the next execution of this function.
            Some(Initializer::Instantiate { module, args: _ }) => {
                let module = &self.cur.modules[*module];
                let imports = ImportsBuilder::new(
                    module,
                    ImportSource::Outer {
                        initializer: self.cur.initializer - 1,
                    },
                );
                let prev = mem::replace(&mut self.cur, imports);
                self.in_progress.push(prev);
            }

            // A new module is being defined, and the source of this module is
            // our module's list of closed-over-modules.
            //
            // This is used for outer aliases.
            Some(Initializer::DefineModule(upvar_index)) => {
                self.cur
                    .modules
                    .push(self.cur.module.module_upvar(*upvar_index).clone());
            }

            // A new module is defined, created from a set of compiled
            // artifacts. The new module value will be created with the
            // specified artifacts being closed over as well as the specified
            // set of module values in our index/upvar index spaces being closed
            // over.
            //
            // This is used for defining submodules.
            Some(Initializer::CreateModule {
                artifact_index,
                artifacts,
                modules,
            }) => {
                let submodule = self.cur.module.create_submodule(
                    *artifact_index,
                    artifacts,
                    modules,
                    &self.cur.modules,
                );
                self.cur.modules.push(submodule);
            }

            // All initializers have been processed, which means we're ready to
            // perform the actual raw instantiation with the raw import values.
            // Once that's done if there's an in-progress module we record the
            // instance in the index space. Otherwise this is the final module
            // and we return the items out.
            //
            // Note that in all cases we return the raw instance handle to get
            // the start function executed by the outer context.
            None => {
                let (instance, start) = self.instantiate_raw(store)?;
                let toplevel = match self.in_progress.pop() {
                    Some(imports) => {
                        self.cur = imports;
                        self.cur.instances.push(instance);
                        false
                    }
                    None => true,
                };
                return Ok(Some((instance, start, toplevel)));
            }
        }

        Ok(None)
    }

    fn instantiate_raw(
        &mut self,
        store: &mut StoreOpaque<'_>,
    ) -> Result<(Instance, Option<FuncIndex>)> {
        let compiled_module = self.cur.module.compiled_module();

        // Register the module just before instantiation to ensure we keep the module
        // properly referenced while in use by the store.
        store.modules_mut().register(&self.cur.module);

        unsafe {
            // The first thing we do is issue an instance allocation request
            // to the instance allocator. This, on success, will give us an
            // instance handle.
            //
            // Note that the `host_state` here is a pointer back to the
            // `Instance` we'll be returning from this function. This is a
            // circular reference so we can't construct it before we construct
            // this instance, so we determine what the ID is and then assert
            // it's the same later when we do actually insert it.
            let instance_to_be = store.store_data().next_id::<InstanceData>();
            let mut instance_handle =
                store
                    .engine()
                    .allocator()
                    .allocate(InstanceAllocationRequest {
                        module: compiled_module.module().clone(),
                        finished_functions: compiled_module.finished_functions(),
                        imports: self.cur.build(),
                        shared_signatures: self.cur.module.signatures().as_module_map().into(),
                        host_state: Box::new(Instance(instance_to_be)),
                        store: Some(store.traitobj),
                    })?;

            // The instance still has lots of setup, for example
            // data/elements/start/etc. This can all fail, but even on failure
            // the instance may persist some state via previous successful
            // initialization. For this reason once we have an instance handle
            // we immediately insert it into the store to keep it alive.
            //
            // Note that we `clone` the instance handle just to make easier
            // working the the borrow checker here easier. Technically the `&mut
            // instance` has somewhat of a borrow on `store` (which
            // conflicts with the borrow on `store.engine`) but this doesn't
            // matter in practice since initialization isn't even running any
            // code here anyway.
            let id = store.add_instance(instance_handle.clone(), false);

            // Additionally, before we start doing fallible instantiation, we
            // do one more step which is to insert an `InstanceData`
            // corresponding to this instance. This `InstanceData` can be used
            // via `Caller::get_export` if our instance's state "leaks" into
            // other instances, even if we don't return successfully from this
            // function.
            //
            // We don't actually load all exports from the instance at this
            // time, instead preferring to lazily load them as they're demanded.
            // For module/instance exports, though, those aren't actually
            // stored in the instance handle so we need to immediately handle
            // those here.
            let instance = {
                let exports = compiled_module
                    .module()
                    .exports
                    .values()
                    .map(|index| {
                        // Note that instances and modules are not handled by
                        // `wasmtime_runtime`, they're handled by us in this crate. That
                        // means we need to handle that here, otherwise we defer to the
                        // instance to load the values.
                        match *index {
                            EntityIndex::Instance(i) => {
                                Some(Extern::Instance(self.cur.instances[i].clone()))
                            }
                            EntityIndex::Module(i) => {
                                Some(Extern::Module(self.cur.modules[i].clone()))
                            }
                            _ => None,
                        }
                    })
                    .collect();
                let data = InstanceData::Instantiated {
                    id,
                    exports,
                    types: Arc::clone(self.cur.module.types()),
                    signatures: Arc::clone(self.cur.module.signatures()),
                };
                Instance::from_wasmtime(data, store)
            };

            // double-check our guess of what the new instance's ID would be
            // was actually correct.
            assert_eq!(instance.0, instance_to_be);

            // Now that we've recorded all information we need to about this
            // instance within a `Store` we can start performing fallible
            // initialization. Note that we still defer the `start` function to
            // later since that may need to run asynchronously.
            //
            // If this returns an error (or if the start function traps) then
            // any other initialization which may have succeeded which placed
            // items from this instance into other instances should be ok when
            // those items are loaded and run we'll have all the metadata to
            // look at them.
            store
                .engine()
                .allocator()
                .initialize(
                    &mut instance_handle,
                    compiled_module.module(),
                    store.engine().config().features.bulk_memory,
                )
                .map_err(|e| -> Error {
                    match e {
                        InstantiationError::Trap(trap) => Trap::from_runtime(trap).into(),
                        other => other.into(),
                    }
                })?;

            Ok((instance, compiled_module.module().start_func))
        }
    }

    fn start_raw(store: &mut StoreOpaque<'_>, instance: Instance, start: FuncIndex) -> Result<()> {
        let id = match &store[instance.0] {
            InstanceData::Instantiated { id, .. } => *id,
            InstanceData::Synthetic(_) => return Ok(()),
        };
        // If a start function is present, invoke it. Make sure we use all the
        // trap-handling configuration in `store` as well.
        let instance = store.instance(id);
        let f = match instance.lookup_by_declaration(&EntityIndex::Function(start)) {
            wasmtime_runtime::Export::Function(f) => f,
            _ => unreachable!(), // valid modules shouldn't hit this
        };
        let vmctx = instance.vmctx_ptr();
        unsafe {
            super::func::invoke_wasm_and_catch_traps(store, |_default_callee| {
                mem::transmute::<
                    *const VMFunctionBody,
                    unsafe extern "C" fn(*mut VMContext, *mut VMContext),
                >(f.anyfunc.as_ref().func_ptr.as_ptr())(
                    f.anyfunc.as_ref().vmctx, vmctx
                )
            })?;
        }
        Ok(())
    }
}

impl<'a> ImportsBuilder<'a> {
    fn new(module: &Module, src: ImportSource<'a>) -> ImportsBuilder<'a> {
        let raw = module.compiled_module().module();
        ImportsBuilder {
            src,
            functions: PrimaryMap::with_capacity(raw.num_imported_funcs),
            tables: PrimaryMap::with_capacity(raw.num_imported_tables),
            memories: PrimaryMap::with_capacity(raw.num_imported_memories),
            globals: PrimaryMap::with_capacity(raw.num_imported_globals),
            instances: PrimaryMap::with_capacity(raw.instances.len()),
            modules: PrimaryMap::with_capacity(raw.modules.len()),
            module: module.clone(),
            initializer: 0,
        }
    }

    fn push(&mut self, item: Extern, store: &mut StoreOpaque<'_>) {
        match item {
            Extern::Func(i) => {
                self.functions.push(i.vmimport(store));
            }
            Extern::Global(i) => {
                self.globals.push(i.vmimport(store));
            }
            Extern::Table(i) => {
                self.tables.push(i.vmimport(store));
            }
            Extern::Memory(i) => {
                self.memories.push(i.vmimport(store));
            }
            Extern::Instance(i) => {
                self.instances.push(i);
            }
            Extern::Module(m) => {
                self.modules.push(m);
            }
        }
    }

    fn build(&self) -> Imports<'_> {
        Imports {
            tables: self.tables.values().as_slice(),
            globals: self.globals.values().as_slice(),
            memories: self.memories.values().as_slice(),
            functions: self.functions.values().as_slice(),
        }
    }
}

/// An instance, pre-instantiation, that is ready to be instantiated.
///
/// This structure represents an instance *just before* it was instantiated,
/// after all type-checking and imports have been resolved. The only thing left
/// to do for this instance is to actually run the process of instantiation.
///
/// Note that an `InstancePre` may not be tied to any particular [`Store`] if
/// none of the imports it closed over are tied to any particular [`Store`].
///
/// This structure is created through the [`Linker::instantiate_pre`] method,
/// which also has some more information and examples.
///
/// [`Store`]: crate::Store
/// [`Linker::instantiate_pre`]: crate::Linker::instantiate_pre
pub struct InstancePre<T> {
    module: Module,
    items: Vec<Definition>,
    _marker: std::marker::PhantomData<fn() -> T>,
}

impl<T> InstancePre<T> {
    pub(crate) unsafe fn new(
        store: &mut StoreOpaque,
        module: &Module,
        items: Vec<Definition>,
    ) -> Result<InstancePre<T>> {
        typecheck_defs(store, module, &items)?;
        Ok(InstancePre {
            module: module.clone(),
            items,
            _marker: std::marker::PhantomData,
        })
    }

    /// Instantiates this instance, creating a new instance within the provided
    /// `store`.
    ///
    /// This function will run the actual process of instantiation to
    /// completion. This will use all of the previously-closed-over items as
    /// imports to instantiate the module that this was originally created with.
    ///
    /// For more information about instantiation see [`Instance::new`].
    ///
    /// # Panics
    ///
    /// Panics if any import closed over by this [`InstancePre`] isn't owned by
    /// `store`, or if `store` has async support enabled.
    pub fn instantiate(&self, mut store: impl AsContextMut<Data = T>) -> Result<Instance> {
        let mut store = store.as_context_mut().opaque();
        // For the unsafety here the typecheck happened at creation time of this
        // structure and then othrewise the `T` of `InstancePre<T>` connects any
        // host functions we have in our definition list to the `store` that was
        // passed in.
        unsafe {
            self.ensure_comes_from_same_store(&store)?;
            Instantiator::new(
                &mut store,
                &self.module,
                ImportSource::Definitions(&self.items),
            )?
            .run(&mut store)
        }
    }

    /// Creates a new instance, running the start function asynchronously
    /// instead of inline.
    ///
    /// For more information about asynchronous instantiation see the
    /// documentation on [`Instance::new_async`].
    ///
    /// # Panics
    ///
    /// Panics if any import closed over by this [`InstancePre`] isn't owned by
    /// `store`, or if `store` does not have async support enabled.
    #[cfg(feature = "async")]
    #[cfg_attr(nightlydoc, doc(cfg(feature = "async")))]
    pub async fn instantiate_async(
        &self,
        mut store: impl AsContextMut<Data = T>,
    ) -> Result<Instance>
    where
        T: Send,
    {
        // For the unsafety here see above
        let mut i = unsafe {
            let mut store = store.as_context_mut().opaque();
            self.ensure_comes_from_same_store(&store)?;
            Instantiator::new(
                &mut store,
                &self.module,
                ImportSource::Definitions(&self.items),
            )?
        };
        i.run_async(store).await
    }

    fn ensure_comes_from_same_store(&self, store: &StoreOpaque<'_>) -> Result<()> {
        for import in self.items.iter() {
            if !import.comes_from_same_store(store) {
                bail!("cross-`Store` instantiation is not currently supported");
            }
        }
        Ok(())
    }
}

fn typecheck_externs(store: &mut StoreOpaque, module: &Module, imports: &[Extern]) -> Result<()> {
    for import in imports {
        if !import.comes_from_same_store(store) {
            bail!("cross-`Store` instantiation is not currently supported");
        }
    }
    typecheck(store, module, imports, |cx, ty, item| cx.extern_(ty, item))
}

fn typecheck_defs(store: &mut StoreOpaque, module: &Module, imports: &[Definition]) -> Result<()> {
    for import in imports {
        if !import.comes_from_same_store(store) {
            bail!("cross-`Store` instantiation is not currently supported");
        }
    }
    typecheck(store, module, imports, |cx, ty, item| {
        cx.definition(ty, item)
    })
}

fn typecheck<I>(
    store: &mut StoreOpaque,
    module: &Module,
    imports: &[I],
    check: impl Fn(&matching::MatchCx<'_>, &EntityType, &I) -> Result<()>,
) -> Result<()> {
    let env_module = module.compiled_module().module();
    let expected = env_module.imports().count();
    if expected != imports.len() {
        bail!("expected {} imports, found {}", expected, imports.len());
    }
    let cx = matching::MatchCx {
        signatures: module.signatures(),
        types: module.types(),
        store: store,
        engine: store.engine(),
    };
    for ((name, field, expected_ty), actual) in env_module.imports().zip(imports) {
        check(&cx, &expected_ty, actual).with_context(|| {
            let extra = match field {
                Some(name) => format!("::{}", name),
                None => String::new(),
            };
            format!("incompatible import type for `{}{}`", name, extra)
        })?;
    }
    Ok(())
}
