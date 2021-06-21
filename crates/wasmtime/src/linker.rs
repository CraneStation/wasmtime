use crate::func::HostFunc;
use crate::instance::{InstanceData, InstancePre};
use crate::store::StoreOpaque;
use crate::{
    AsContextMut, Caller, Engine, Extern, ExternType, Func, FuncType, ImportType, Instance,
    IntoFunc, Module, Trap, Val,
};
use anyhow::{anyhow, bail, Context, Error, Result};
use log::warn;
use std::collections::hash_map::{Entry, HashMap};
#[cfg(feature = "async")]
use std::future::Future;
use std::marker;
#[cfg(feature = "async")]
use std::pin::Pin;
use std::sync::Arc;

/// Structure used to link wasm modules/instances together.
///
/// This structure is used to assist in instantiating a [`Module`]. A [`Linker`]
/// is a way of performing name resolution to make instantiating a module easier
/// than specifying positional imports to [`Instance::new`]. [`Linker`] is a
/// name-based resolver where names are dynamically defined and then used to
/// instantiate a [`Module`].
///
/// An important method is [`Linker::instantiate`] which takes a module to
/// instantiate into the provided store. This method will automatically select
/// all the right imports for the [`Module`] to be instantiated, and will
/// otherwise return an error if an import isn't satisfied.
///
/// ## Name Resolution
///
/// As mentioned previously, `Linker` is a form of name resolver. It will be
/// using the string-based names of imports on a module to attempt to select a
/// matching item to hook up to it. This name resolution has two-levels of
/// namespaces, a module level and a name level. Each item is defined within a
/// module and then has its own name. This basically follows the wasm standard
/// for modularization.
///
/// Names in a `Linker` cannot be defined twice, but allowing duplicates by
/// shadowing the previous definition can be controlled with the
/// [`Linker::allow_shadowing`] method.
///
/// ## Commands and Reactors
///
/// The [`Linker`] type provides conveniences for working with WASI Commands and
/// Reactors through the [`Linker::module`] method. This will automatically
/// handle instantiation and calling `_start` and such as appropriate
/// depending on the inferred type of module.
///
/// ## Type parameter `T`
///
/// It's worth pointing out that the type parameter `T` on [`Linker<T>`] does
/// not represent that `T` is stored within a [`Linker`]. Rather the `T` is used
/// to ensure that linker-defined functions and stores instantiated into all use
/// the same matching `T` as host state.
///
/// ## Multiple `Store`s
///
/// The [`Linker`] type is designed to be compatible, in some scenarios, with
/// instantiation in multiple [`Store`]s. Specifically host-defined functions
/// created in [`Linker`] with [`Linker::func_new`], [`Linker::func_wrap`], and
/// their async versions are compatible to instantiate into any [`Store`]. This
/// enables programs which want to instantiate lots of modules to create one
/// [`Linker`] value at program start up and use that continuously for each
/// [`Store`] created over the lifetime of the program.
///
/// Note that once [`Store`]-owned items, such as [`Global`], are defined witin
/// a [`Linker`] then it is no longer compatible with any [`Store`]. At that
/// point only the [`Store`] that owns the [`Global`] can be used to instantiate
/// modules.
///
/// [`Store`]: crate::Store
/// [`Global`]: crate::Global
pub struct Linker<T> {
    engine: Engine,
    string2idx: HashMap<Arc<str>, usize>,
    strings: Vec<Arc<str>>,
    map: HashMap<ImportKey, Definition>,
    allow_shadowing: bool,
    allow_unknown_exports: bool,
    _marker: marker::PhantomData<fn() -> T>,
}

impl<T> Clone for Linker<T> {
    fn clone(&self) -> Linker<T> {
        Linker {
            engine: self.engine.clone(),
            string2idx: self.string2idx.clone(),
            strings: self.strings.clone(),
            map: self.map.clone(),
            allow_shadowing: self.allow_shadowing,
            allow_unknown_exports: self.allow_unknown_exports,
            _marker: self._marker,
        }
    }
}

#[derive(Copy, Clone, Hash, PartialEq, Eq)]
struct ImportKey {
    name: usize,
    module: usize,
}

#[derive(Clone)]
pub(crate) enum Definition {
    Extern(Extern),
    HostFunc(Arc<HostFunc>),
    Instance(Arc<indexmap::IndexMap<String, Definition>>),
}

macro_rules! generate_wrap_async_func {
    ($num:tt $($args:ident)*) => (paste::paste!{
        /// Asynchronous analog of [`Linker::func_wrap`].
        ///
        /// For more information also see
        /// [`Func::wrapN_async`](crate::Func::wrap1_async).
        #[allow(non_snake_case)]
        #[cfg(feature = "async")]
        #[cfg_attr(nightlydoc, doc(cfg(feature = "async")))]
        pub fn [<func_wrap $num _async>]<$($args,)* R>(
            &mut self,
            module: &str,
            name: &str,
            func: impl for<'a> Fn(Caller<'a, T>, $($args),*) -> Box<dyn Future<Output = R> + Send + 'a> + Send + Sync + 'static,
        ) -> Result<&mut Self>
        where
            $($args: crate::WasmTy,)*
            R: crate::WasmRet,
        {
            assert!(
                self.engine.config().async_support,
                concat!(
                    "cannot use `func_wrap",
                    $num,
                    "_async` without enabling async support on the config",
                ),
            );
            self.func_wrap(module, name, move |caller: Caller<'_, T>, $($args: $args),*| {
                let async_cx = caller.store.0.async_cx();
                let mut future = Pin::from(func(caller, $($args),*));
                match unsafe { async_cx.block_on(future.as_mut()) } {
                    Ok(ret) => ret.into_fallible(),
                    Err(e) => R::fallible_from_trap(e),
                }
            })
        }
    })
}

impl<T> Linker<T> {
    /// Creates a new [`Linker`].
    pub fn new(engine: &Engine) -> Linker<T> {
        Linker {
            engine: engine.clone(),
            map: HashMap::new(),
            string2idx: HashMap::new(),
            strings: Vec::new(),
            allow_shadowing: false,
            allow_unknown_exports: false,
            _marker: marker::PhantomData,
        }
    }

    /// Returns the [`Engine`] this is connected to.
    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    /// Configures whether this [`Linker`] will shadow previous duplicate
    /// definitions of the same signature.
    ///
    /// By default a [`Linker`] will disallow duplicate definitions of the same
    /// signature. This method, however, can be used to instead allow duplicates
    /// and have the latest definition take precedence when linking modules.
    ///
    /// # Examples
    ///
    /// ```
    /// # use wasmtime::*;
    /// # fn main() -> anyhow::Result<()> {
    /// # let engine = Engine::default();
    /// let mut linker = Linker::<()>::new(&engine);
    /// linker.func_wrap("", "", || {})?;
    ///
    /// // by default, duplicates are disallowed
    /// assert!(linker.func_wrap("", "", || {}).is_err());
    ///
    /// // but shadowing can be configured to be allowed as well
    /// linker.allow_shadowing(true);
    /// linker.func_wrap("", "", || {})?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn allow_shadowing(&mut self, allow: bool) -> &mut Self {
        self.allow_shadowing = allow;
        self
    }

    /// Configures whether this [`Linker`] will allow unknown exports from
    /// command modules.
    ///
    /// By default a [`Linker`] will error when unknown exports are encountered
    /// in a command module while using [`Linker::module`].
    ///
    /// This method can be used to allow unknown exports from command modules.
    ///
    /// # Examples
    ///
    /// ```
    /// # use wasmtime::*;
    /// # fn main() -> anyhow::Result<()> {
    /// # let engine = Engine::default();
    /// # let module = Module::new(&engine, "(module)")?;
    /// # let mut store = Store::new(&engine, ());
    /// let mut linker = Linker::new(&engine);
    /// linker.allow_unknown_exports(true);
    /// linker.module(&mut store, "mod", &module)?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn allow_unknown_exports(&mut self, allow: bool) -> &mut Self {
        self.allow_unknown_exports = allow;
        self
    }

    /// Defines a new item in this [`Linker`].
    ///
    /// This method will add a new definition, by name, to this instance of
    /// [`Linker`]. The `module` and `name` provided are what to name the
    /// `item`.
    ///
    /// # Errors
    ///
    /// Returns an error if the `module` and `name` already identify an item
    /// of the same type as the `item` provided and if shadowing is disallowed.
    /// For more information see the documentation on [`Linker`].
    ///
    /// Also returns an error if `item` comes from a different store than this
    /// [`Linker`] was created with.
    ///
    /// # Examples
    ///
    /// ```
    /// # use wasmtime::*;
    /// # fn main() -> anyhow::Result<()> {
    /// # let engine = Engine::default();
    /// # let mut store = Store::new(&engine, ());
    /// let mut linker = Linker::new(&engine);
    /// let ty = GlobalType::new(ValType::I32, Mutability::Const);
    /// let global = Global::new(&mut store, ty, Val::I32(0x1234))?;
    /// linker.define("host", "offset", global)?;
    ///
    /// let wat = r#"
    ///     (module
    ///         (import "host" "offset" (global i32))
    ///         (memory 1)
    ///         (data (global.get 0) "foo")
    ///     )
    /// "#;
    /// let module = Module::new(&engine, wat)?;
    /// linker.instantiate(&mut store, &module)?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn define(
        &mut self,
        module: &str,
        name: &str,
        item: impl Into<Extern>,
    ) -> Result<&mut Self> {
        let key = self.import_key(module, Some(name));
        self.insert(key, Definition::Extern(item.into()))?;
        Ok(self)
    }

    /// Same as [`Linker::define`], except only the name of the import is
    /// provided, not a module name as well.
    ///
    /// This is only relevant when working with the module linking proposal
    /// where one-level names are allowed (in addition to two-level names).
    /// Otherwise this method need not be used.
    pub fn define_name(&mut self, name: &str, item: impl Into<Extern>) -> Result<&mut Self> {
        let key = self.import_key(name, None);
        self.insert(key, Definition::Extern(item.into()))?;
        Ok(self)
    }

    /// Creates a [`Func::new`]-style function named in this linker.
    ///
    /// For more information see [`Linker::func_wrap`].
    pub fn func_new(
        &mut self,
        module: &str,
        name: &str,
        ty: FuncType,
        func: impl Fn(Caller<'_, T>, &[Val], &mut [Val]) -> Result<(), Trap> + Send + Sync + 'static,
    ) -> Result<&mut Self> {
        let func = HostFunc::new(&self.engine, ty, func);
        let key = self.import_key(module, Some(name));
        self.insert(key, Definition::HostFunc(Arc::new(func)))?;
        Ok(self)
    }

    /// Creates a [`Func::new_async`]-style function named in this linker.
    ///
    /// For more information see [`Linker::func_wrap`].
    #[cfg(feature = "async")]
    #[cfg_attr(nightlydoc, doc(cfg(feature = "async")))]
    pub fn func_new_async<F>(
        &mut self,
        module: &str,
        name: &str,
        ty: FuncType,
        func: F,
    ) -> Result<&mut Self>
    where
        F: for<'a> Fn(
                Caller<'a, T>,
                &'a [Val],
                &'a mut [Val],
            ) -> Box<dyn Future<Output = Result<(), Trap>> + Send + 'a>
            + Send
            + Sync
            + 'static,
    {
        assert!(
            self.engine.config().async_support,
            "cannot use `func_new_async` without enabling async support in the config"
        );
        self.func_new(module, name, ty, move |caller, params, results| {
            let async_cx = caller.store.0.async_cx();
            let mut future = Pin::from(func(caller, params, results));
            match unsafe { async_cx.block_on(future.as_mut()) } {
                Ok(Ok(())) => Ok(()),
                Ok(Err(trap)) | Err(trap) => Err(trap),
            }
        })
    }

    /// Define a host function within this linker.
    ///
    /// For information about how the host function operates, see
    /// [`Func::wrap`]. That includes information about translating Rust types
    /// to WebAssembly native types.
    ///
    /// This method creates a host-provided function in this linker under the
    /// provided name. This method is distinct in its capability to create a
    /// [`Store`](crate::Store)-independent function. This means that the
    /// function defined here can be used to instantiate instances in multiple
    /// different stores, or in other words the function can be loaded into
    /// different stores.
    ///
    /// Note that the capability mentioned here applies to all other
    /// host-function-defining-methods on [`Linker`] as well. All of them can be
    /// used to create instances of [`Func`] within multiple stores. In a
    /// multithreaded program, for example, this means that the host functions
    /// could be called concurrently if different stores are executing on
    /// different threads.
    ///
    /// # Errors
    ///
    /// Returns an error if the `module` and `name` already identify an item
    /// of the same type as the `item` provided and if shadowing is disallowed.
    /// For more information see the documentation on [`Linker`].
    ///
    /// # Examples
    ///
    /// ```
    /// # use wasmtime::*;
    /// # fn main() -> anyhow::Result<()> {
    /// # let engine = Engine::default();
    /// let mut linker = Linker::new(&engine);
    /// linker.func_wrap("host", "double", |x: i32| x * 2)?;
    /// linker.func_wrap("host", "log_i32", |x: i32| println!("{}", x))?;
    /// linker.func_wrap("host", "log_str", |caller: Caller<'_, ()>, ptr: i32, len: i32| {
    ///     // ...
    /// })?;
    ///
    /// let wat = r#"
    ///     (module
    ///         (import "host" "double" (func (param i32) (result i32)))
    ///         (import "host" "log_i32" (func (param i32)))
    ///         (import "host" "log_str" (func (param i32 i32)))
    ///     )
    /// "#;
    /// let module = Module::new(&engine, wat)?;
    ///
    /// // instantiate in multiple different stores
    /// for _ in 0..10 {
    ///     let mut store = Store::new(&engine, ());
    ///     linker.instantiate(&mut store, &module)?;
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub fn func_wrap<Params, Args>(
        &mut self,
        module: &str,
        name: &str,
        func: impl IntoFunc<T, Params, Args>,
    ) -> Result<&mut Self> {
        let func = HostFunc::wrap(&self.engine, func);
        let key = self.import_key(module, Some(name));
        self.insert(key, Definition::HostFunc(Arc::new(func)))?;
        Ok(self)
    }

    for_each_function_signature!(generate_wrap_async_func);

    /// Convenience wrapper to define an entire [`Instance`] in this linker.
    ///
    /// This function is a convenience wrapper around [`Linker::define`] which
    /// will define all exports on `instance` into this linker. The module name
    /// for each export is `module_name`, and the name for each export is the
    /// name in the instance itself.
    ///
    /// # Errors
    ///
    /// Returns an error if the any item is redefined twice in this linker (for
    /// example the same `module_name` was already defined) and shadowing is
    /// disallowed, or if `instance` comes from a different
    /// [`Store`](crate::Store) than this [`Linker`] originally was created
    /// with.
    ///
    /// # Panics
    ///
    /// Panics if `instance` does not belong to `store`.
    ///
    /// # Examples
    ///
    /// ```
    /// # use wasmtime::*;
    /// # fn main() -> anyhow::Result<()> {
    /// # let engine = Engine::default();
    /// # let mut store = Store::new(&engine, ());
    /// let mut linker = Linker::new(&engine);
    ///
    /// // Instantiate a small instance...
    /// let wat = r#"(module (func (export "run") ))"#;
    /// let module = Module::new(&engine, wat)?;
    /// let instance = linker.instantiate(&mut store, &module)?;
    ///
    /// // ... and inform the linker that the name of this instance is
    /// // `instance1`. This defines the `instance1::run` name for our next
    /// // module to use.
    /// linker.instance(&mut store, "instance1", instance)?;
    ///
    /// let wat = r#"
    ///     (module
    ///         (import "instance1" "run" (func $instance1_run))
    ///         (func (export "run")
    ///             call $instance1_run
    ///         )
    ///     )
    /// "#;
    /// let module = Module::new(&engine, wat)?;
    /// let instance = linker.instantiate(&mut store, &module)?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn instance(
        &mut self,
        mut store: impl AsContextMut<Data = T>,
        module_name: &str,
        instance: Instance,
    ) -> Result<&mut Self> {
        for export in instance.exports(store.as_context_mut()) {
            let key = self.import_key(module_name, Some(export.name()));
            self.insert(key, Definition::Extern(export.into_extern()))?;
        }
        Ok(self)
    }

    /// Define automatic instantiations of a [`Module`] in this linker.
    ///
    /// This automatically handles [Commands and Reactors] instantiation and
    /// initialization.
    ///
    /// Exported functions of a Command module may be called directly, however
    /// instead of having a single instance which is reused for each call,
    /// each call creates a new instance, which lives for the duration of the
    /// call. The imports of the Command are resolved once, and reused for
    /// each instantiation, so all dependencies need to be present at the time
    /// when `Linker::module` is called.
    ///
    /// For Reactors, a single instance is created, and an initialization
    /// function is called, and then its exports may be called.
    ///
    /// Ordinary modules which don't declare themselves to be either Commands
    /// or Reactors are treated as Reactors without any initialization calls.
    ///
    /// [Commands and Reactors]: https://github.com/WebAssembly/WASI/blob/master/design/application-abi.md#current-unstable-abi
    ///
    /// # Errors
    ///
    /// Returns an error if the any item is redefined twice in this linker (for
    /// example the same `module_name` was already defined) and shadowing is
    /// disallowed, if `instance` comes from a different
    /// [`Store`](crate::Store) than this [`Linker`] originally was created
    /// with, or if a Reactor initialization function traps.
    ///
    /// # Panics
    ///
    /// Panics if any item used to instantiate the provided [`Module`] is not
    /// owned by `store`.
    ///
    /// # Examples
    ///
    /// ```
    /// # use wasmtime::*;
    /// # fn main() -> anyhow::Result<()> {
    /// # let engine = Engine::default();
    /// # let mut store = Store::new(&engine, ());
    /// let mut linker = Linker::new(&engine);
    ///
    /// // Instantiate a small instance and inform the linker that the name of
    /// // this instance is `instance1`. This defines the `instance1::run` name
    /// // for our next module to use.
    /// let wat = r#"(module (func (export "run") ))"#;
    /// let module = Module::new(&engine, wat)?;
    /// linker.module(&mut store, "instance1", &module)?;
    ///
    /// let wat = r#"
    ///     (module
    ///         (import "instance1" "run" (func $instance1_run))
    ///         (func (export "run")
    ///             call $instance1_run
    ///         )
    ///     )
    /// "#;
    /// let module = Module::new(&engine, wat)?;
    /// let instance = linker.instantiate(&mut store, &module)?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// For a Command, a new instance is created for each call.
    ///
    /// ```
    /// # use wasmtime::*;
    /// # fn main() -> anyhow::Result<()> {
    /// # let engine = Engine::default();
    /// # let mut store = Store::new(&engine, ());
    /// let mut linker = Linker::new(&engine);
    ///
    /// // Create a Command that attempts to count the number of times it is run, but is
    /// // foiled by each call getting a new instance.
    /// let wat = r#"
    ///     (module
    ///         (global $counter (mut i32) (i32.const 0))
    ///         (func (export "_start")
    ///             (global.set $counter (i32.add (global.get $counter) (i32.const 1)))
    ///         )
    ///         (func (export "read_counter") (result i32)
    ///             (global.get $counter)
    ///         )
    ///     )
    /// "#;
    /// let module = Module::new(&engine, wat)?;
    /// linker.module(&mut store, "commander", &module)?;
    /// let run = linker.get_default(&mut store, "")?
    ///     .typed::<(), (), _>(&store)?
    ///     .clone();
    /// run.call(&mut store, ())?;
    /// run.call(&mut store, ())?;
    /// run.call(&mut store, ())?;
    ///
    /// let wat = r#"
    ///     (module
    ///         (import "commander" "_start" (func $commander_start))
    ///         (import "commander" "read_counter" (func $commander_read_counter (result i32)))
    ///         (func (export "run") (result i32)
    ///             call $commander_start
    ///             call $commander_start
    ///             call $commander_start
    ///             call $commander_read_counter
    ///         )
    ///     )
    /// "#;
    /// let module = Module::new(&engine, wat)?;
    /// linker.module(&mut store, "", &module)?;
    /// let run = linker.get(&mut store, "", Some("run")).unwrap().into_func().unwrap();
    /// let count = run.typed::<(), i32, _>(&store)?.call(&mut store, ())?;
    /// assert_eq!(count, 0, "a Command should get a fresh instance on each invocation");
    ///
    /// # Ok(())
    /// # }
    /// ```
    pub fn module(
        &mut self,
        mut store: impl AsContextMut<Data = T>,
        module_name: &str,
        module: &Module,
    ) -> Result<&mut Self>
    where
        T: 'static,
    {
        match ModuleKind::categorize(module)? {
            ModuleKind::Command => self.command(store, module_name, module),
            ModuleKind::Reactor => {
                let instance = self.instantiate(&mut store, &module)?;

                if let Some(export) = instance.get_export(&mut store, "_initialize") {
                    if let Extern::Func(func) = export {
                        func.typed::<(), (), _>(&store)
                            .and_then(|f| f.call(&mut store, ()).map_err(Into::into))
                            .context("calling the Reactor initialization function")?;
                    }
                }

                self.instance(store, module_name, instance)
            }
        }
    }

    fn command(
        &mut self,
        mut store: impl AsContextMut<Data = T>,
        module_name: &str,
        module: &Module,
    ) -> Result<&mut Self>
    where
        T: 'static,
    {
        for export in module.exports() {
            if let Some(func_ty) = export.ty().func() {
                let instance_pre = self.instantiate_pre(&mut store, module)?;
                let export_name = export.name().to_owned();
                let func = Func::new(
                    &mut store,
                    func_ty.clone(),
                    move |mut caller, params, results| {
                        // Create a new instance for this command execution.
                        let instance = instance_pre.instantiate(&mut caller)?;

                        // `unwrap()` everything here because we know the instance contains a
                        // function export with the given name and signature because we're
                        // iterating over the module it was instantiated from.
                        let command_results = instance
                            .get_export(&mut caller, &export_name)
                            .unwrap()
                            .into_func()
                            .unwrap()
                            .call(&mut caller, params)
                            .map_err(|error| error.downcast::<Trap>().unwrap())?;

                        // Copy the return values into the output slice.
                        for (result, command_result) in
                            results.iter_mut().zip(command_results.into_vec())
                        {
                            *result = command_result;
                        }

                        Ok(())
                    },
                );
                let key = self.import_key(module_name, Some(export.name()));
                self.insert(key, Definition::Extern(func.into()))?;
            } else if export.name() == "memory" && export.ty().memory().is_some() {
                // Allow an exported "memory" memory for now.
            } else if export.name() == "__indirect_function_table" && export.ty().table().is_some()
            {
                // Allow an exported "__indirect_function_table" table for now.
            } else if export.name() == "table" && export.ty().table().is_some() {
                // Allow an exported "table" table for now.
            } else if export.name() == "__data_end" && export.ty().global().is_some() {
                // Allow an exported "__data_end" memory for compatibility with toolchains
                // which use --export-dynamic, which unfortunately doesn't work the way
                // we want it to.
                warn!("command module exporting '__data_end' is deprecated");
            } else if export.name() == "__heap_base" && export.ty().global().is_some() {
                // Allow an exported "__data_end" memory for compatibility with toolchains
                // which use --export-dynamic, which unfortunately doesn't work the way
                // we want it to.
                warn!("command module exporting '__heap_base' is deprecated");
            } else if export.name() == "__dso_handle" && export.ty().global().is_some() {
                // Allow an exported "__dso_handle" memory for compatibility with toolchains
                // which use --export-dynamic, which unfortunately doesn't work the way
                // we want it to.
                warn!("command module exporting '__dso_handle' is deprecated")
            } else if export.name() == "__rtti_base" && export.ty().global().is_some() {
                // Allow an exported "__rtti_base" memory for compatibility with
                // AssemblyScript.
                warn!("command module exporting '__rtti_base' is deprecated; pass `--runtime half` to the AssemblyScript compiler");
            } else if !self.allow_unknown_exports {
                bail!("command export '{}' is not a function", export.name());
            }
        }

        Ok(self)
    }

    /// Aliases one item's name as another.
    ///
    /// This method will alias an item with the specified `module` and `name`
    /// under a new name of `as_module` and `as_name`.
    ///
    /// # Errors
    ///
    /// Returns an error if any shadowing violations happen while defining new
    /// items, or if the original item wasn't defined.
    pub fn alias(
        &mut self,
        module: &str,
        name: &str,
        as_module: &str,
        as_name: &str,
    ) -> Result<&mut Self> {
        let src = self.import_key(module, Some(name));
        let dst = self.import_key(as_module, Some(as_name));
        match self.map.get(&src).cloned() {
            Some(item) => self.insert(dst, item)?,
            None => bail!("no item named `{}::{}` defined", module, name),
        }
        Ok(self)
    }

    /// Aliases one module's name as another.
    ///
    /// This method will alias all currently defined under `module` to also be
    /// defined under the name `as_module` too.
    ///
    /// # Errors
    ///
    /// Returns an error if any shadowing violations happen while defining new
    /// items.
    pub fn alias_module(&mut self, module: &str, as_module: &str) -> Result<()> {
        let module = self.intern_str(module);
        let as_module = self.intern_str(as_module);
        let items = self
            .map
            .iter()
            .filter(|(key, _def)| key.module == module)
            .map(|(key, def)| (key.name, def.clone()))
            .collect::<Vec<_>>();
        for (name, item) in items {
            self.insert(
                ImportKey {
                    module: as_module,
                    name,
                },
                item,
            )?;
        }
        Ok(())
    }

    fn insert(&mut self, key: ImportKey, item: Definition) -> Result<()> {
        match self.map.entry(key) {
            Entry::Occupied(_) if !self.allow_shadowing => {
                let module = &self.strings[key.module];
                let desc = match self.strings.get(key.name) {
                    Some(name) => format!("{}::{}", module, name),
                    None => module.to_string(),
                };
                bail!("import of `{}` defined twice", desc)
            }
            Entry::Occupied(mut o) => {
                o.insert(item);
            }
            Entry::Vacant(v) => {
                v.insert(item);
            }
        }
        Ok(())
    }

    fn import_key(&mut self, module: &str, name: Option<&str>) -> ImportKey {
        ImportKey {
            module: self.intern_str(module),
            name: name
                .map(|name| self.intern_str(name))
                .unwrap_or(usize::max_value()),
        }
    }

    fn intern_str(&mut self, string: &str) -> usize {
        if let Some(idx) = self.string2idx.get(string) {
            return *idx;
        }
        let string: Arc<str> = string.into();
        let idx = self.strings.len();
        self.strings.push(string.clone());
        self.string2idx.insert(string, idx);
        idx
    }

    /// Attempts to instantiate the `module` provided.
    ///
    /// This method will attempt to assemble a list of imports that correspond
    /// to the imports required by the [`Module`] provided. This list
    /// of imports is then passed to [`Instance::new`] to continue the
    /// instantiation process.
    ///
    /// Each import of `module` will be looked up in this [`Linker`] and must
    /// have previously been defined. If it was previously defined with an
    /// incorrect signature or if it was not previously defined then an error
    /// will be returned because the import can not be satisfied.
    ///
    /// Per the WebAssembly spec, instantiation includes running the module's
    /// start function, if it has one (not to be confused with the `_start`
    /// function, which is not run).
    ///
    /// # Errors
    ///
    /// This method can fail because an import may not be found, or because
    /// instantiation itself may fail. For information on instantiation
    /// failures see [`Instance::new`].
    ///
    /// # Panics
    ///
    /// Panics if any item used to instantiate `module` is not owned by
    /// `store`.
    ///
    /// # Examples
    ///
    /// ```
    /// # use wasmtime::*;
    /// # fn main() -> anyhow::Result<()> {
    /// # let engine = Engine::default();
    /// # let mut store = Store::new(&engine, ());
    /// let mut linker = Linker::new(&engine);
    /// linker.func_wrap("host", "double", |x: i32| x * 2)?;
    ///
    /// let wat = r#"
    ///     (module
    ///         (import "host" "double" (func (param i32) (result i32)))
    ///     )
    /// "#;
    /// let module = Module::new(&engine, wat)?;
    /// linker.instantiate(&mut store, &module)?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn instantiate(
        &self,
        mut store: impl AsContextMut<Data = T>,
        module: &Module,
    ) -> Result<Instance> {
        self.instantiate_pre(&mut store, module)?.instantiate(store)
    }

    /// Attempts to instantiate the `module` provided. This is the same as
    /// [`Linker::instantiate`], except for async `Store`s.
    #[cfg(feature = "async")]
    #[cfg_attr(nightlydoc, doc(cfg(feature = "async")))]
    pub async fn instantiate_async(
        &self,
        mut store: impl AsContextMut<Data = T>,
        module: &Module,
    ) -> Result<Instance>
    where
        T: Send,
    {
        self.instantiate_pre(&mut store, module)?
            .instantiate_async(store)
            .await
    }

    /// Performs all checks necessary for instantiating `module` with this
    /// linker within `store`, except that instantiation doesn't actually
    /// finish.
    ///
    /// This method is used for front-loading type-checking information as well
    /// as collecting the imports to use to instantiate a module with. The
    /// returned [`InstancePre`] represents a ready-to-be-instantiated module,
    /// which can also be instantiated multiple times if desired.
    ///
    /// # Panics
    ///
    /// This method will panic if any item defined in this linker used by
    /// `module` is not owned by `store`.
    ///
    /// # Examples
    ///
    /// ```
    /// # use wasmtime::*;
    /// # fn main() -> anyhow::Result<()> {
    /// # let engine = Engine::default();
    /// # let mut store = Store::new(&engine, ());
    /// let mut linker = Linker::new(&engine);
    /// linker.func_wrap("host", "double", |x: i32| x * 2)?;
    ///
    /// let wat = r#"
    ///     (module
    ///         (import "host" "double" (func (param i32) (result i32)))
    ///     )
    /// "#;
    /// let module = Module::new(&engine, wat)?;
    /// let instance_pre = linker.instantiate_pre(&mut store, &module)?;
    ///
    /// // Finish instantiation after the type-checking has all completed...
    /// let instance = instance_pre.instantiate(&mut store)?;
    ///
    /// // ... and we can even continue to keep instantiating if desired!
    /// instance_pre.instantiate(&mut store)?;
    /// instance_pre.instantiate(&mut store)?;
    ///
    /// // Note that functions defined in a linker with `func_wrap` and similar
    /// // constructors are not owned by any particular `Store`, so we can also
    /// // instantiate our `instance_pre` in other stores because no imports
    /// // belong to the original store.
    /// let mut new_store = Store::new(&engine, ());
    /// instance_pre.instantiate(&mut new_store)?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn instantiate_pre(
        &self,
        mut store: impl AsContextMut<Data = T>,
        module: &Module,
    ) -> Result<InstancePre<T>> {
        let imports = module
            .imports()
            .map(|import| {
                self._get_by_import(&import)
                    .ok_or_else(|| self.link_error(&import))
            })
            .collect::<Result<_>>()?;
        unsafe { InstancePre::new(&mut store.as_context_mut().opaque(), module, imports) }
    }

    fn link_error(&self, import: &ImportType) -> Error {
        let desc = match import.name() {
            Some(name) => format!("{}::{}", import.module(), name),
            None => import.module().to_string(),
        };
        anyhow!("unknown import: `{}` has not been defined", desc)
    }

    /// Returns an iterator over all items defined in this `Linker`, in
    /// arbitrary order.
    ///
    /// The iterator returned will yield 3-tuples where the first two elements
    /// are the module name and item name for the external item, and the third
    /// item is the item itself that is defined.
    ///
    /// Note that multiple `Extern` items may be defined for the same
    /// module/name pair.
    pub fn iter<'a: 'p, 'p>(
        &'a self,
        mut store: impl AsContextMut<Data = T> + 'p,
    ) -> impl Iterator<Item = (&str, &str, Extern)> + 'p {
        self.map.iter().map(move |(key, item)| {
            let mut store = store.as_context_mut().opaque();
            (
                &*self.strings[key.module],
                &*self.strings[key.name],
                // Should be safe since `T` is connecting the linker and store
                unsafe { item.to_extern(&mut store) },
            )
        })
    }

    /// Looks up a previously defined value in this [`Linker`], identified by
    /// the names provided.
    ///
    /// Returns `None` if this name was not previously defined in this
    /// [`Linker`].
    pub fn get(
        &self,
        mut store: impl AsContextMut<Data = T>,
        module: &str,
        name: Option<&str>,
    ) -> Option<Extern> {
        let mut store = store.as_context_mut().opaque();
        // Should be safe since `T` is connecting the linker and store
        Some(unsafe { self._get(module, name)?.to_extern(&mut store) })
    }

    fn _get(&self, module: &str, name: Option<&str>) -> Option<&Definition> {
        let key = ImportKey {
            module: *self.string2idx.get(module)?,
            name: match name {
                Some(name) => *self.string2idx.get(name)?,
                None => usize::max_value(),
            },
        };
        self.map.get(&key)
    }

    /// Looks up a value in this `Linker` which matches the `import` type
    /// provided.
    ///
    /// Returns `None` if no match was found.
    pub fn get_by_import(
        &self,
        mut store: impl AsContextMut<Data = T>,
        import: &ImportType,
    ) -> Option<Extern> {
        let mut store = store.as_context_mut().opaque();
        // Should be safe since `T` is connecting the linker and store
        Some(unsafe { self._get_by_import(import)?.to_extern(&mut store) })
    }

    fn _get_by_import(&self, import: &ImportType) -> Option<Definition> {
        if let Some(item) = self._get(import.module(), import.name()) {
            return Some(item.clone());
        }

        if import.name().is_some() {
            return None;
        }

        if let ExternType::Instance(t) = import.ty() {
            // This is a key location where the module linking proposal is
            // implemented. This logic allows single-level imports of an instance to
            // get satisfied by multiple definitions of items within this `Linker`.
            //
            // The instance being import is iterated over to load the names from
            // this `Linker` (recursively calling `get`). If anything isn't defined
            // we return `None` since the entire value isn't defined. Otherwise when
            // all values are loaded it's assembled into an `Instance` and
            // returned`.
            //
            // Note that this isn't exactly the speediest implementation in the
            // world. Ideally we would pre-create the `Instance` instead of creating
            // it each time a module is instantiated. For now though while the
            // module linking proposal is under development this should hopefully
            // suffice.
            let mut map = indexmap::IndexMap::new();
            for export in t.exports() {
                let item = self._get(import.module(), Some(export.name()))?;
                map.insert(export.name().to_string(), item.clone());
            }
            return Some(Definition::Instance(Arc::new(map)));
        }

        None
    }

    /// Returns the "default export" of a module.
    ///
    /// An export with an empty string is considered to be a "default export".
    /// "_start" is also recognized for compatibility.
    ///
    /// # Panics
    ///
    /// Panics if the default function found is not owned by `store`.
    pub fn get_default(
        &self,
        mut store: impl AsContextMut<Data = T>,
        module: &str,
    ) -> Result<Func> {
        if let Some(external) = self.get(&mut store, module, Some("")) {
            if let Extern::Func(func) = external {
                return Ok(func.clone());
            }
            bail!("default export in '{}' is not a function", module);
        }

        // For compatibility, also recognize "_start".
        if let Some(external) = self.get(&mut store, module, Some("_start")) {
            if let Extern::Func(func) = external {
                return Ok(func.clone());
            }
            bail!("`_start` in '{}' is not a function", module);
        }

        // Otherwise return a no-op function.
        Ok(Func::wrap(store, || {}))
    }
}

impl<T> Default for Linker<T> {
    fn default() -> Linker<T> {
        Linker::new(&Engine::default())
    }
}

impl Definition {
    /// Note the unsafety here is due to calling `HostFunc::to_func`. The
    /// requirement here is that the `T` that was originally used to create the
    /// `HostFunc` matches the `T` on the store.
    pub(crate) unsafe fn to_extern(&self, store: &mut StoreOpaque) -> Extern {
        match self {
            Definition::Extern(e) => e.clone(),
            Definition::HostFunc(func) => func.to_func(store).into(),
            Definition::Instance(i) => {
                let items = Arc::new(
                    i.iter()
                        .map(|(name, item)| (name.clone(), item.to_extern(store)))
                        .collect(),
                );
                Instance::from_wasmtime(InstanceData::Synthetic(items), store).into()
            }
        }
    }

    pub(crate) fn comes_from_same_store(&self, store: &StoreOpaque) -> bool {
        match self {
            Definition::Extern(e) => e.comes_from_same_store(store),
            Definition::HostFunc(_func) => true,
            Definition::Instance(i) => i.values().all(|e| e.comes_from_same_store(store)),
        }
    }
}

/// Modules can be interpreted either as Commands or Reactors.
enum ModuleKind {
    /// The instance is a Command, meaning an instance is created for each
    /// exported function and lives for the duration of the function call.
    Command,

    /// The instance is a Reactor, meaning one instance is created which
    /// may live across multiple calls.
    Reactor,
}

impl ModuleKind {
    /// Determine whether the given module is a Command or a Reactor.
    fn categorize(module: &Module) -> Result<ModuleKind> {
        let command_start = module.get_export("_start");
        let reactor_start = module.get_export("_initialize");
        match (command_start, reactor_start) {
            (Some(command_start), None) => {
                if let Some(_) = command_start.func() {
                    Ok(ModuleKind::Command)
                } else {
                    bail!("`_start` must be a function")
                }
            }
            (None, Some(reactor_start)) => {
                if let Some(_) = reactor_start.func() {
                    Ok(ModuleKind::Reactor)
                } else {
                    bail!("`_initialize` must be a function")
                }
            }
            (None, None) => {
                // Module declares neither of the recognized functions, so treat
                // it as a reactor with no initialization function.
                Ok(ModuleKind::Reactor)
            }
            (Some(_), Some(_)) => {
                // Module declares itself to be both a Command and a Reactor.
                bail!("Program cannot be both a Command and a Reactor")
            }
        }
    }
}
