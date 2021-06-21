use crate::store::{StoreData, StoreOpaque, Stored};
use crate::{
    AsContext, AsContextMut, Engine, Extern, FuncType, Instance, InterruptHandle, StoreContext,
    StoreContextMut, Trap, Val, ValType,
};
use anyhow::{bail, Context as _, Result};
use smallvec::{smallvec, SmallVec};
use std::cmp::max;
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::mem;
use std::panic::{self, AssertUnwindSafe};
use std::pin::Pin;
use std::ptr::NonNull;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use wasmtime_environ::wasm::{EntityIndex, FuncIndex};
use wasmtime_runtime::{
    raise_user_trap, ExportFunction, InstanceAllocator, InstanceHandle, OnDemandInstanceAllocator,
    VMCallerCheckedAnyfunc, VMContext, VMFunctionBody, VMFunctionImport, VMSharedSignatureIndex,
    VMTrampoline,
};

/// A WebAssembly function which can be called.
///
/// This type can represent either an exported function from a WebAssembly
/// module or a host-defined function which can be used to satisfy an import of
/// a module. [`Func`] and can be used to both instantiate an [`Instance`] as
/// well as be extracted from an [`Instance`].
///
/// [`Instance`]: crate::Instance
///
/// A [`Func`] "belongs" to the store that it was originally created within.
/// Operations on a [`Func`] only work with the store it belongs to, and if
/// another store is passed in by accident then methods will panic.
///
/// # `Func` and `async`
///
/// Functions from the perspective of WebAssembly are always synchronous. You
/// might have an `async` function in Rust, however, which you'd like to make
/// available from WebAssembly. Wasmtime supports asynchronously calling
/// WebAssembly through native stack switching. You can get some more
/// information about [asynchronous configs](crate::Config::async_support), but
/// from the perspective of `Func` it's important to know that whether or not
/// your [`Store`](crate::Store) is asynchronous will dictate whether you call
/// functions through [`Func::call`] or [`Func::call_async`] (or the typed
/// wrappers such as [`TypedFunc::call`] vs [`TypedFunc::call_async`]).
///
/// # To `Func::call` or to `Func::typed().call()`
///
/// There's a 2x2 matrix of methods to call [`Func`]. Invocations can either be
/// asynchronous or synchronous. They can also be statically typed or not.
/// Whether or not an invocation is asynchronous is indicated via the method
/// being `async` and [`call_async`](Func::call_async) being the entry point.
/// Otherwise for statically typed or not your options are:
///
/// * Dynamically typed - if you don't statically know the signature of the
///   function that you're calling you'll be using [`Func::call`] or
///   [`Func::call_async`]. These functions take a variable-length slice of
///   "boxed" arguments in their [`Val`] representation. Additionally the
///   results are returned as an owned slice of [`Val`]. These methods are not
///   optimized due to the dynamic type checks that must occur, in addition to
///   some dynamic allocations for where to put all the arguments. While this
///   allows you to call all possible wasm function signatures, if you're
///   looking for a speedier alternative you can also use...
///
/// * Statically typed - if you statically know the type signature of the wasm
///   function you're calling, then you'll want to use the [`Func::typed`]
///   method to acquire an instance of [`TypedFunc`]. This structure is static proof
///   that the underlying wasm function has the ascripted type, and type
///   validation is only done once up-front. The [`TypedFunc::call`] and
///   [`TypedFunc::call_async`] methods are much more efficient than [`Func::call`]
///   and [`Func::call_async`] because the type signature is statically known.
///   This eschews runtime checks as much as possible to get into wasm as fast
///   as possible.
///
/// # Examples
///
/// One way to get a `Func` is from an [`Instance`] after you've instantiated
/// it:
///
/// ```
/// # use wasmtime::*;
/// # fn main() -> anyhow::Result<()> {
/// let engine = Engine::default();
/// let module = Module::new(&engine, r#"(module (func (export "foo")))"#)?;
/// let mut store = Store::new(&engine, ());
/// let instance = Instance::new(&mut store, &module, &[])?;
/// let foo = instance.get_func(&mut store, "foo").expect("export wasn't a function");
///
/// // Work with `foo` as a `Func` at this point, such as calling it
/// // dynamically...
/// match foo.call(&mut store, &[]) {
///     Ok(result) => { /* ... */ }
///     Err(trap) => {
///         panic!("execution of `foo` resulted in a wasm trap: {}", trap);
///     }
/// }
/// foo.call(&mut store, &[])?;
///
/// // ... or we can make a static assertion about its signature and call it.
/// // Our first call here can fail if the signatures don't match, and then the
/// // second call can fail if the function traps (like the `match` above).
/// let foo = foo.typed::<(), (), _>(&store)?;
/// foo.call(&mut store, ())?;
/// # Ok(())
/// # }
/// ```
///
/// You can also use the [`wrap` function](Func::wrap) to create a
/// `Func`
///
/// ```
/// # use wasmtime::*;
/// # fn main() -> anyhow::Result<()> {
/// let mut store = Store::<()>::default();
///
/// // Create a custom `Func` which can execute arbitrary code inside of the
/// // closure.
/// let add = Func::wrap(&mut store, |a: i32, b: i32| -> i32 { a + b });
///
/// // Next we can hook that up to a wasm module which uses it.
/// let module = Module::new(
///     store.engine(),
///     r#"
///         (module
///             (import "" "" (func $add (param i32 i32) (result i32)))
///             (func (export "call_add_twice") (result i32)
///                 i32.const 1
///                 i32.const 2
///                 call $add
///                 i32.const 3
///                 i32.const 4
///                 call $add
///                 i32.add))
///     "#,
/// )?;
/// let instance = Instance::new(&mut store, &module, &[add.into()])?;
/// let call_add_twice = instance.get_typed_func::<(), i32, _>(&mut store, "call_add_twice")?;
///
/// assert_eq!(call_add_twice.call(&mut store, ())?, 10);
/// # Ok(())
/// # }
/// ```
///
/// Or you could also create an entirely dynamic `Func`!
///
/// ```
/// # use wasmtime::*;
/// # fn main() -> anyhow::Result<()> {
/// let mut store = Store::<()>::default();
///
/// // Here we need to define the type signature of our `Double` function and
/// // then wrap it up in a `Func`
/// let double_type = wasmtime::FuncType::new(
///     [wasmtime::ValType::I32].iter().cloned(),
///     [wasmtime::ValType::I32].iter().cloned(),
/// );
/// let double = Func::new(&mut store, double_type, |_, params, results| {
///     let mut value = params[0].unwrap_i32();
///     value *= 2;
///     results[0] = value.into();
///     Ok(())
/// });
///
/// let module = Module::new(
///     store.engine(),
///     r#"
///         (module
///             (import "" "" (func $double (param i32) (result i32)))
///             (func $start
///                 i32.const 1
///                 call $double
///                 drop)
///             (start $start))
///     "#,
/// )?;
/// let instance = Instance::new(&mut store, &module, &[double.into()])?;
/// // .. work with `instance` if necessary
/// # Ok(())
/// # }
/// ```
#[derive(Copy, Clone, Debug)]
#[repr(transparent)] // here for the C API
pub struct Func(Stored<FuncData>);

/// The three ways that a function can be created and referenced from within a
/// store.
pub(crate) enum FuncData {
    /// A function already owned by the store via some other means. This is
    /// used, for example, when creating a `Func` from an instance's exported
    /// function. The instance's `InstanceHandle` is already owned by the store
    /// and we just have some pointers into that which represent how to call the
    /// function.
    StoreOwned {
        trampoline: VMTrampoline,
        export: ExportFunction,
    },

    /// A function is shared across possibly other stores, hence the `Arc`. This
    /// variant happens when a `Linker`-defined function is instantiated within
    /// a `Store` (e.g. via `Linker::get` or similar APIs). The `Arc` here
    /// indicates that there's some number of other stores holding this function
    /// too, so dropping this may not deallocate the underlying
    /// `InstanceHandle`.
    SharedHost(Arc<HostFunc>),

    /// A uniquely-owned host function within a `Store`. This comes about with
    /// `Func::new` or similar APIs. The `HostFunc` internally owns the
    /// `InstanceHandle` and that will get dropped when this `HostFunc` itself
    /// is dropped.
    Host(HostFunc),
}

macro_rules! for_each_function_signature {
    ($mac:ident) => {
        $mac!(0);
        $mac!(1 A1);
        $mac!(2 A1 A2);
        $mac!(3 A1 A2 A3);
        $mac!(4 A1 A2 A3 A4);
        $mac!(5 A1 A2 A3 A4 A5);
        $mac!(6 A1 A2 A3 A4 A5 A6);
        $mac!(7 A1 A2 A3 A4 A5 A6 A7);
        $mac!(8 A1 A2 A3 A4 A5 A6 A7 A8);
        $mac!(9 A1 A2 A3 A4 A5 A6 A7 A8 A9);
        $mac!(10 A1 A2 A3 A4 A5 A6 A7 A8 A9 A10);
        $mac!(11 A1 A2 A3 A4 A5 A6 A7 A8 A9 A10 A11);
        $mac!(12 A1 A2 A3 A4 A5 A6 A7 A8 A9 A10 A11 A12);
        $mac!(13 A1 A2 A3 A4 A5 A6 A7 A8 A9 A10 A11 A12 A13);
        $mac!(14 A1 A2 A3 A4 A5 A6 A7 A8 A9 A10 A11 A12 A13 A14);
        $mac!(15 A1 A2 A3 A4 A5 A6 A7 A8 A9 A10 A11 A12 A13 A14 A15);
        $mac!(16 A1 A2 A3 A4 A5 A6 A7 A8 A9 A10 A11 A12 A13 A14 A15 A16);
    };
}

mod typed;
pub use typed::*;

macro_rules! generate_wrap_async_func {
    ($num:tt $($args:ident)*) => (paste::paste!{
        /// Same as [`Func::wrap`], except the closure asynchronously produces
        /// its result. For more information see the [`Func`] documentation.
        ///
        /// # Panics
        ///
        /// This function will panic if called with a non-asynchronous store.
        #[allow(non_snake_case)]
        #[cfg(feature = "async")]
        #[cfg_attr(nightlydoc, doc(cfg(feature = "async")))]
        pub fn [<wrap $num _async>]<T, $($args,)* R>(
            store: impl AsContextMut<Data = T>,
            func: impl for<'a> Fn(Caller<'a, T>, $($args),*) -> Box<dyn Future<Output = R> + Send + 'a> + Send + Sync + 'static,
        ) -> Func
        where
            $($args: WasmTy,)*
            R: WasmRet,
        {
            assert!(store.as_context().async_support(), concat!("cannot use `wrap", $num, "_async` without enabling async support on the config"));
            Func::wrap(store, move |caller: Caller<'_, T>, $($args: $args),*| {
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

impl Func {
    /// Creates a new `Func` with the given arguments, typically to create a
    /// host-defined function to pass as an import to a module.
    ///
    /// * `store` - the store in which to create this [`Func`], which will own
    ///   the return value.
    ///
    /// * `ty` - the signature of this function, used to indicate what the
    ///   inputs and outputs are.
    ///
    /// * `func` - the native code invoked whenever this `Func` will be called.
    ///   This closure is provided a [`Caller`] as its first argument to learn
    ///   information about the caller, and then it's passed a list of
    ///   parameters as a slice along with a mutable slice of where to write
    ///   results.
    ///
    /// Note that the implementation of `func` must adhere to the `ty` signature
    /// given, error or traps may occur if it does not respect the `ty`
    /// signature. For example if the function type declares that it returns one
    /// i32 but the `func` closures does not write anything into the results
    /// slice then a trap may be generated.
    ///
    /// Additionally note that this is quite a dynamic function since signatures
    /// are not statically known. For a more performant and ergonomic `Func`
    /// it's recommended to use [`Func::wrap`] if you can because with
    /// statically known signatures Wasmtime can optimize the implementation
    /// much more.
    ///
    /// For more information about `Send + Sync + 'static` requirements on the
    /// `func`, see [`Func::wrap`](#why-send--sync--static).
    pub fn new<T>(
        mut store: impl AsContextMut<Data = T>,
        ty: FuncType,
        func: impl Fn(Caller<'_, T>, &[Val], &mut [Val]) -> Result<(), Trap> + Send + Sync + 'static,
    ) -> Self {
        let mut store = store.as_context_mut().opaque();

        // part of this unsafety is about matching the `T` to a `Store<T>`,
        // which is done through the `AsContextMut` bound above.
        unsafe {
            let host = HostFunc::new(store.engine(), ty, func);
            host.into_func(&mut store)
        }
    }

    /// Creates a new host-defined WebAssembly function which, when called,
    /// will run the asynchronous computation defined by `func` to completion
    /// and then return the result to WebAssembly.
    ///
    /// This function is the asynchronous analogue of [`Func::new`] and much of
    /// that documentation applies to this as well. The key difference is that
    /// `func` returns a future instead of simply a `Result`. Note that the
    /// returned future can close over any of the arguments, but it cannot close
    /// over the state of the closure itself. It's recommended to store any
    /// necessary async state in the `T` of the [`Store<T>`](crate::Store) which
    /// can be accessed through [`Caller::data`] or [`Caller::data_mut`].
    ///
    /// For more information on `Send + Sync + 'static`, see
    /// [`Func::wrap`](#why-send--sync--static).
    ///
    /// # Panics
    ///
    /// This function will panic if `store` is not associated with an [async
    /// config](crate::Config::async_support).
    ///
    /// # Examples
    ///
    /// ```
    /// # use wasmtime::*;
    /// # fn main() -> anyhow::Result<()> {
    /// // Simulate some application-specific state as well as asynchronous
    /// // functions to query that state.
    /// struct MyDatabase {
    ///     // ...
    /// }
    ///
    /// impl MyDatabase {
    ///     async fn get_row_count(&self) -> u32 {
    ///         // ...
    /// #       100
    ///     }
    /// }
    ///
    /// let my_database = MyDatabase {
    ///     // ...
    /// };
    ///
    /// // Using `new_async` we can hook up into calling our async
    /// // `get_row_count` function.
    /// let engine = Engine::new(Config::new().async_support(true))?;
    /// let mut store = Store::new(&engine, MyDatabase {
    ///     // ...
    /// });
    /// let get_row_count_type = wasmtime::FuncType::new(
    ///     None,
    ///     Some(wasmtime::ValType::I32),
    /// );
    /// let get = Func::new_async(&mut store, get_row_count_type, |caller, _params, results| {
    ///     Box::new(async move {
    ///         let count = caller.data().get_row_count().await;
    ///         results[0] = Val::I32(count as i32);
    ///         Ok(())
    ///     })
    /// });
    /// // ...
    /// # Ok(())
    /// # }
    /// ```
    #[cfg(feature = "async")]
    #[cfg_attr(nightlydoc, doc(cfg(feature = "async")))]
    pub fn new_async<T, F>(store: impl AsContextMut<Data = T>, ty: FuncType, func: F) -> Func
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
            store.as_context().async_support(),
            "cannot use `new_async` without enabling async support in the config"
        );
        Func::new(store, ty, move |caller, params, results| {
            let async_cx = caller.store.0.async_cx();
            let mut future = Pin::from(func(caller, params, results));
            match unsafe { async_cx.block_on(future.as_mut()) } {
                Ok(Ok(())) => Ok(()),
                Ok(Err(trap)) | Err(trap) => Err(trap),
            }
        })
    }

    pub(crate) unsafe fn from_caller_checked_anyfunc(
        store: &mut StoreOpaque,
        anyfunc: *mut VMCallerCheckedAnyfunc,
    ) -> Option<Self> {
        let anyfunc = NonNull::new(anyfunc)?;
        debug_assert!(anyfunc.as_ref().type_index != VMSharedSignatureIndex::default());
        let export = ExportFunction { anyfunc };
        Some(Func::from_wasmtime_function(export, store))
    }

    /// Creates a new `Func` from the given Rust closure.
    ///
    /// This function will create a new `Func` which, when called, will
    /// execute the given Rust closure. Unlike [`Func::new`] the target
    /// function being called is known statically so the type signature can
    /// be inferred. Rust types will map to WebAssembly types as follows:
    ///
    /// | Rust Argument Type  | WebAssembly Type |
    /// |---------------------|------------------|
    /// | `i32`               | `i32`            |
    /// | `u32`               | `i32`            |
    /// | `i64`               | `i64`            |
    /// | `u64`               | `i64`            |
    /// | `f32`               | `f32`            |
    /// | `f64`               | `f64`            |
    /// | (not supported)     | `v128`           |
    /// | `Option<Func>`      | `funcref`        |
    /// | `Option<ExternRef>` | `externref`      |
    ///
    /// Any of the Rust types can be returned from the closure as well, in
    /// addition to some extra types
    ///
    /// | Rust Return Type  | WebAssembly Return Type | Meaning               |
    /// |-------------------|-------------------------|-----------------------|
    /// | `()`              | nothing                 | no return value       |
    /// | `T`               | `T`                     | a single return value |
    /// | `(T1, T2, ...)`   | `T1 T2 ...`             | multiple returns      |
    ///
    /// Note that all return types can also be wrapped in `Result<_, Trap>` to
    /// indicate that the host function can generate a trap as well as possibly
    /// returning a value.
    ///
    /// Finally you can also optionally take [`Caller`] as the first argument of
    /// your closure. If inserted then you're able to inspect the caller's
    /// state, for example the [`Memory`](crate::Memory) it has exported so you
    /// can read what pointers point to.
    ///
    /// Note that when using this API, the intention is to create as thin of a
    /// layer as possible for when WebAssembly calls the function provided. With
    /// sufficient inlining and optimization the WebAssembly will call straight
    /// into `func` provided, with no extra fluff entailed.
    ///
    /// # Why `Send + Sync + 'static`?
    ///
    /// All host functions defined in a [`Store`](crate::Store) (including
    /// those from [`Func::new`] and other constructors) require that the
    /// `func` provided is `Send + Sync + 'static`. Additionally host functions
    /// always are `Fn` as opposed to `FnMut` or `FnOnce`. This can at-a-glance
    /// feel restrictive since the closure cannot close over as many types as
    /// before. The reason for this, though, is to ensure that
    /// [`Store<T>`](crate::Store) can implement both the `Send` and `Sync`
    /// traits.
    ///
    /// Fear not, however, because this isn't as restrictive as it seems! Host
    /// functions are provided a [`Caller<'_, T>`](crate::Caller) argument which
    /// allows access to the host-defined data within the
    /// [`Store`](crate::Store). The `T` type is not required to be any of
    /// `Send`, `Sync`, or `'static`! This means that you can store whatever
    /// you'd like in `T` and have it accessible by all host functions.
    /// Additionally mutable access to `T` is allowed through
    /// [`Caller::data_mut`].
    ///
    /// Most host-defined [`Func`] values provide closures that end up not
    /// actually closing over any values. These zero-sized types will use the
    /// context from [`Caller`] for host-defined information.
    ///
    /// # Examples
    ///
    /// First up we can see how simple wasm imports can be implemented, such
    /// as a function that adds its two arguments and returns the result.
    ///
    /// ```
    /// # use wasmtime::*;
    /// # fn main() -> anyhow::Result<()> {
    /// # let mut store = Store::<()>::default();
    /// let add = Func::wrap(&mut store, |a: i32, b: i32| a + b);
    /// let module = Module::new(
    ///     store.engine(),
    ///     r#"
    ///         (module
    ///             (import "" "" (func $add (param i32 i32) (result i32)))
    ///             (func (export "foo") (param i32 i32) (result i32)
    ///                 local.get 0
    ///                 local.get 1
    ///                 call $add))
    ///     "#,
    /// )?;
    /// let instance = Instance::new(&mut store, &module, &[add.into()])?;
    /// let foo = instance.get_typed_func::<(i32, i32), i32, _>(&mut store, "foo")?;
    /// assert_eq!(foo.call(&mut store, (1, 2))?, 3);
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// We can also do the same thing, but generate a trap if the addition
    /// overflows:
    ///
    /// ```
    /// # use wasmtime::*;
    /// # fn main() -> anyhow::Result<()> {
    /// # let mut store = Store::<()>::default();
    /// let add = Func::wrap(&mut store, |a: i32, b: i32| {
    ///     match a.checked_add(b) {
    ///         Some(i) => Ok(i),
    ///         None => Err(Trap::new("overflow")),
    ///     }
    /// });
    /// let module = Module::new(
    ///     store.engine(),
    ///     r#"
    ///         (module
    ///             (import "" "" (func $add (param i32 i32) (result i32)))
    ///             (func (export "foo") (param i32 i32) (result i32)
    ///                 local.get 0
    ///                 local.get 1
    ///                 call $add))
    ///     "#,
    /// )?;
    /// let instance = Instance::new(&mut store, &module, &[add.into()])?;
    /// let foo = instance.get_typed_func::<(i32, i32), i32, _>(&mut store, "foo")?;
    /// assert_eq!(foo.call(&mut store, (1, 2))?, 3);
    /// assert!(foo.call(&mut store, (i32::max_value(), 1)).is_err());
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// And don't forget all the wasm types are supported!
    ///
    /// ```
    /// # use wasmtime::*;
    /// # fn main() -> anyhow::Result<()> {
    /// # let mut store = Store::<()>::default();
    /// let debug = Func::wrap(&mut store, |a: i32, b: u32, c: f32, d: i64, e: u64, f: f64| {
    ///
    ///     println!("a={}", a);
    ///     println!("b={}", b);
    ///     println!("c={}", c);
    ///     println!("d={}", d);
    ///     println!("e={}", e);
    ///     println!("f={}", f);
    /// });
    /// let module = Module::new(
    ///     store.engine(),
    ///     r#"
    ///         (module
    ///             (import "" "" (func $debug (param i32 i32 f32 i64 i64 f64)))
    ///             (func (export "foo")
    ///                 i32.const -1
    ///                 i32.const 1
    ///                 f32.const 2
    ///                 i64.const -3
    ///                 i64.const 3
    ///                 f64.const 4
    ///                 call $debug))
    ///     "#,
    /// )?;
    /// let instance = Instance::new(&mut store, &module, &[debug.into()])?;
    /// let foo = instance.get_typed_func::<(), (), _>(&mut store, "foo")?;
    /// foo.call(&mut store, ())?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// Finally if you want to get really fancy you can also implement
    /// imports that read/write wasm module's memory
    ///
    /// ```
    /// use std::str;
    ///
    /// # use wasmtime::*;
    /// # fn main() -> anyhow::Result<()> {
    /// # let mut store = Store::default();
    /// let log_str = Func::wrap(&mut store, |mut caller: Caller<'_, ()>, ptr: i32, len: i32| {
    ///     let mem = match caller.get_export("memory") {
    ///         Some(Extern::Memory(mem)) => mem,
    ///         _ => return Err(Trap::new("failed to find host memory")),
    ///     };
    ///     let data = mem.data(&caller)
    ///         .get(ptr as u32 as usize..)
    ///         .and_then(|arr| arr.get(..len as u32 as usize));
    ///     let string = match data {
    ///         Some(data) => match str::from_utf8(data) {
    ///             Ok(s) => s,
    ///             Err(_) => return Err(Trap::new("invalid utf-8")),
    ///         },
    ///         None => return Err(Trap::new("pointer/length out of bounds")),
    ///     };
    ///     assert_eq!(string, "Hello, world!");
    ///     println!("{}", string);
    ///     Ok(())
    /// });
    /// let module = Module::new(
    ///     store.engine(),
    ///     r#"
    ///         (module
    ///             (import "" "" (func $log_str (param i32 i32)))
    ///             (func (export "foo")
    ///                 i32.const 4   ;; ptr
    ///                 i32.const 13  ;; len
    ///                 call $log_str)
    ///             (memory (export "memory") 1)
    ///             (data (i32.const 4) "Hello, world!"))
    ///     "#,
    /// )?;
    /// let instance = Instance::new(&mut store, &module, &[log_str.into()])?;
    /// let foo = instance.get_typed_func::<(), (), _>(&mut store, "foo")?;
    /// foo.call(&mut store, ())?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn wrap<T, Params, Results>(
        mut store: impl AsContextMut<Data = T>,
        func: impl IntoFunc<T, Params, Results>,
    ) -> Func {
        let mut store = store.as_context_mut().opaque();
        // part of this unsafety is about matching the `T` to a `Store<T>`,
        // which is done through the `AsContextMut` bound above.
        unsafe {
            let host = HostFunc::wrap(store.engine(), func);
            host.into_func(&mut store)
        }
    }

    for_each_function_signature!(generate_wrap_async_func);

    /// Returns the underlying wasm type that this `Func` has.
    ///
    /// # Panics
    ///
    /// Panics if `store` does not own this function.
    pub fn ty(&self, store: impl AsContext) -> FuncType {
        // Signatures should always be registered in the engine's registry of
        // shared signatures, so we should be able to unwrap safely here.
        let store = store.as_context();
        let sig_index = unsafe { store[self.0].export().anyfunc.as_ref().type_index };
        FuncType::from_wasm_func_type(
            store
                .engine()
                .signatures()
                .lookup_type(sig_index)
                .expect("signature should be registered"),
        )
    }

    pub(crate) fn sig_index(&self, data: &StoreData) -> VMSharedSignatureIndex {
        unsafe { data[self.0].export().anyfunc.as_ref().type_index }
    }

    /// Invokes this function with the `params` given, returning the results and
    /// any trap, if one occurs.
    ///
    /// The `params` here must match the type signature of this `Func`, or a
    /// trap will occur. If a trap occurs while executing this function, then a
    /// trap will also be returned.
    ///
    /// # Panics
    ///
    /// This function will panic if called on a function belonging to an async
    /// store. Asynchronous stores must always use `call_async`.
    /// initiates a panic. Also panics if `store` does not own this function.
    pub fn call(&self, mut store: impl AsContextMut, params: &[Val]) -> Result<Box<[Val]>> {
        assert!(
            !store.as_context().async_support(),
            "must use `call_async` when async support is enabled on the config",
        );
        let my_ty = self.ty(&store);
        store.as_context_mut().0.exiting_native_hook()?;
        let r = self.call_impl(&mut store.as_context_mut().opaque(), my_ty, params);
        store.as_context_mut().0.entering_native_hook()?;
        r
    }

    /// Invokes this function with the `params` given, returning the results
    /// asynchronously.
    ///
    /// This function is the same as [`Func::call`] except that it is
    /// asynchronous. This is only compatible with stores associated with an
    /// [asynchronous config](crate::Config::async_support).
    ///
    /// It's important to note that the execution of WebAssembly will happen
    /// synchronously in the `poll` method of the future returned from this
    /// function. Wasmtime does not manage its own thread pool or similar to
    /// execute WebAssembly in. Future `poll` methods are generally expected to
    /// resolve quickly, so it's recommended that you run or poll this future
    /// in a "blocking context".
    ///
    /// For more information see the documentation on [asynchronous
    /// configs](crate::Config::async_support).
    ///
    /// # Panics
    ///
    /// Panics if this is called on a function in a synchronous store. This
    /// only works with functions defined within an asynchronous store. Also
    /// panics if `store` does not own this function.
    #[cfg(feature = "async")]
    #[cfg_attr(nightlydoc, doc(cfg(feature = "async")))]
    pub fn call_async<'a, T>(
        &'a self,
        store: impl AsContextMut<Data = T> + 'a,
        params: &'a [Val],
    ) -> impl crate::WasmtimeFuture<Data = T, Output = Result<Box<[Val]>>> + 'a
    where
        T: Send,
    {
        assert!(
            store.as_context().async_support(),
            "must use `call` with non-async stores"
        );
        let my_ty = self.ty(&store);
        crate::store::on_fiber(store, move |store| self.call_impl(store, my_ty, params))
    }

    fn call_impl(
        &self,
        store: &mut StoreOpaque<'_>,
        my_ty: FuncType,
        params: &[Val],
    ) -> Result<Box<[Val]>> {
        let data = &store[self.0];
        let trampoline = data.trampoline();
        let anyfunc = data.export().anyfunc;
        // We need to perform a dynamic check that the arguments given to us
        // match the signature of this function and are appropriate to pass to
        // this function. This involves checking to make sure we have the right
        // number and types of arguments as well as making sure everything is
        // from the same `Store`.
        if my_ty.params().len() != params.len() {
            bail!(
                "expected {} arguments, got {}",
                my_ty.params().len(),
                params.len()
            );
        }

        let mut values_vec = vec![0; max(params.len(), my_ty.results().len())];

        // Store the argument values into `values_vec`.
        let param_tys = my_ty.params();
        for ((arg, slot), ty) in params.iter().cloned().zip(&mut values_vec).zip(param_tys) {
            if arg.ty() != ty {
                bail!(
                    "argument type mismatch: found {} but expected {}",
                    arg.ty(),
                    ty
                );
            }
            if !arg.comes_from_same_store(store) {
                bail!("cross-`Store` values are not currently supported");
            }
            unsafe {
                arg.write_value_to(store, slot);
            }
        }

        // Call the trampoline.
        unsafe {
            let anyfunc = anyfunc.as_ref();
            invoke_wasm_and_catch_traps(store, |callee| {
                trampoline(
                    anyfunc.vmctx,
                    callee,
                    anyfunc.func_ptr.as_ptr(),
                    values_vec.as_mut_ptr(),
                )
            })?;
        }

        // Load the return values out of `values_vec`.
        let mut results = Vec::with_capacity(my_ty.results().len());
        for (index, ty) in my_ty.results().enumerate() {
            unsafe {
                let ptr = values_vec.as_ptr().add(index);
                results.push(Val::read_value_from(store, ptr, ty));
            }
        }

        Ok(results.into())
    }

    #[inline]
    pub(crate) fn caller_checked_anyfunc(
        &self,
        store: &StoreOpaque,
    ) -> NonNull<VMCallerCheckedAnyfunc> {
        store[self.0].export().anyfunc
    }

    pub(crate) unsafe fn from_wasmtime_function(
        export: ExportFunction,
        store: &mut StoreOpaque,
    ) -> Self {
        let anyfunc = export.anyfunc.as_ref();
        let trampoline = store.lookup_trampoline(&*anyfunc);
        let data = FuncData::StoreOwned { trampoline, export };
        Func(store.store_data_mut().insert(data))
    }

    pub(crate) fn vmimport(&self, store: &mut StoreOpaque<'_>) -> VMFunctionImport {
        unsafe {
            let f = self.caller_checked_anyfunc(store);
            VMFunctionImport {
                body: f.as_ref().func_ptr,
                vmctx: f.as_ref().vmctx,
            }
        }
    }

    pub(crate) fn comes_from_same_store(&self, store: &StoreOpaque) -> bool {
        store.store_data().contains(self.0)
    }

    fn invoke<T>(
        mut caller: Caller<'_, T>,
        ty: &FuncType,
        values_vec: *mut u128,
        func: &dyn Fn(Caller<'_, T>, &[Val], &mut [Val]) -> Result<(), Trap>,
    ) -> Result<(), Trap> {
        caller.store.0.entering_native_hook()?;
        // We have a dynamic guarantee that `values_vec` has the right
        // number of arguments and the right types of arguments. As a result
        // we should be able to safely run through them all and read them.
        const STACK_ARGS: usize = 4;
        const STACK_RETURNS: usize = 2;
        let mut args: SmallVec<[Val; STACK_ARGS]> = SmallVec::with_capacity(ty.params().len());
        let mut store = caller.store.as_context_mut().opaque();
        for (i, ty) in ty.params().enumerate() {
            unsafe {
                let val = Val::read_value_from(&mut store, values_vec.add(i), ty);
                args.push(val);
            }
        }

        let mut returns: SmallVec<[Val; STACK_RETURNS]> =
            smallvec![Val::null(); ty.results().len()];

        func(caller.sub_caller(), &args, &mut returns)?;

        // Unlike our arguments we need to dynamically check that the return
        // values produced are correct. There could be a bug in `func` that
        // produces the wrong number, wrong types, or wrong stores of
        // values, and we need to catch that here.
        let mut store = caller.store.as_context_mut().opaque();
        for (i, (ret, ty)) in returns.into_iter().zip(ty.results()).enumerate() {
            if ret.ty() != ty {
                return Err(Trap::new(
                    "function attempted to return an incompatible value",
                ));
            }
            if !ret.comes_from_same_store(&store) {
                return Err(Trap::new(
                    "cross-`Store` values are not currently supported",
                ));
            }
            unsafe {
                ret.write_value_to(&mut store, values_vec.add(i));
            }
        }

        caller.store.0.exiting_native_hook()?;
        Ok(())
    }

    /// Attempts to extract a typed object from this `Func` through which the
    /// function can be called.
    ///
    /// This function serves as an alternative to [`Func::call`] and
    /// [`Func::call_async`]. This method performs a static type check (using
    /// the `Params` and `Results` type parameters on the underlying wasm
    /// function. If the type check passes then a `TypedFunc` object is returned,
    /// otherwise an error is returned describing the typecheck failure.
    ///
    /// The purpose of this relative to [`Func::call`] is that it's much more
    /// efficient when used to invoke WebAssembly functions. With the types
    /// statically known far less setup/teardown is required when invoking
    /// WebAssembly. If speed is desired then this function is recommended to be
    /// used instead of [`Func::call`] (which is more general, hence its
    /// slowdown).
    ///
    /// The `Params` type parameter is used to describe the parameters of the
    /// WebAssembly function. This can either be a single type (like `i32`), or
    /// a tuple of types representing the list of parameters (like `(i32, f32,
    /// f64)`). Additionally you can use `()` to represent that the function has
    /// no parameters.
    ///
    /// The `Results` type parameter is used to describe the results of the
    /// function. This behaves the same way as `Params`, but just for the
    /// results of the function.
    ///
    /// The `S` type parameter represents the method of passing in the store
    /// context, and can typically be specified as simply `_` when calling this
    /// function.
    ///
    /// Translation between Rust types and WebAssembly types looks like:
    ///
    /// | WebAssembly | Rust                |
    /// |-------------|---------------------|
    /// | `i32`       | `i32` or `u32`      |
    /// | `i64`       | `i64` or `u64`      |
    /// | `f32`       | `f32`               |
    /// | `f64`       | `f64`               |
    /// | `externref` | `Option<ExternRef>` |
    /// | `funcref`   | `Option<Func>`      |
    /// | `v128`      | not supported       |
    ///
    /// (note that this mapping is the same as that of [`Func::wrap`]).
    ///
    /// Note that once the [`TypedFunc`] return value is acquired you'll use either
    /// [`TypedFunc::call`] or [`TypedFunc::call_async`] as necessary to actually invoke
    /// the function. This method does not invoke any WebAssembly code, it
    /// simply performs a typecheck before returning the [`TypedFunc`] value.
    ///
    /// This method also has a convenience wrapper as
    /// [`Instance::get_typed_func`](crate::Instance::get_typed_func) to
    /// directly get a typed function value from an
    /// [`Instance`](crate::Instance).
    ///
    /// # Errors
    ///
    /// This function will return an error if `Params` or `Results` does not
    /// match the native type of this WebAssembly function.
    ///
    /// # Panics
    ///
    /// This method will panic if `store` does not own this function.
    ///
    /// # Examples
    ///
    /// An end-to-end example of calling a function which takes no parameters
    /// and has no results:
    ///
    /// ```
    /// # use wasmtime::*;
    /// # fn main() -> anyhow::Result<()> {
    /// let engine = Engine::default();
    /// let mut store = Store::new(&engine, ());
    /// let module = Module::new(&engine, r#"(module (func (export "foo")))"#)?;
    /// let instance = Instance::new(&mut store, &module, &[])?;
    /// let foo = instance.get_func(&mut store, "foo").expect("export wasn't a function");
    ///
    /// // Note that this call can fail due to the typecheck not passing, but
    /// // in our case we statically know the module so we know this should
    /// // pass.
    /// let typed = foo.typed::<(), (), _>(&store)?;
    ///
    /// // Note that this can fail if the wasm traps at runtime.
    /// typed.call(&mut store, ())?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// You can also pass in multiple parameters and get a result back
    ///
    /// ```
    /// # use wasmtime::*;
    /// # fn foo(add: &Func, mut store: Store<()>) -> anyhow::Result<()> {
    /// let typed = add.typed::<(i32, i64), f32, _>(&store)?;
    /// assert_eq!(typed.call(&mut store, (1, 2))?, 3.0);
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// and similarly if a function has multiple results you can bind that too
    ///
    /// ```
    /// # #[cfg(not(feature = "old-x86-backend"))]
    /// # use wasmtime::*;
    /// # #[cfg(not(feature = "old-x86-backend"))]
    /// # fn foo(add_with_overflow: &Func, mut store: Store<()>) -> anyhow::Result<()> {
    /// let typed = add_with_overflow.typed::<(u32, u32), (u32, i32), _>(&store)?;
    /// let (result, overflow) = typed.call(&mut store, (u32::max_value(), 2))?;
    /// assert_eq!(result, 1);
    /// assert_eq!(overflow, 1);
    /// # Ok(())
    /// # }
    /// ```
    pub fn typed<Params, Results, S>(&self, store: S) -> Result<TypedFunc<Params, Results>>
    where
        Params: WasmParams,
        Results: WasmResults,
        S: AsContext,
    {
        // Type-check that the params/results are all valid
        let ty = self.ty(store);
        Params::typecheck(ty.params()).context("type mismatch with parameters")?;
        Results::typecheck(ty.results()).context("type mismatch with results")?;

        // and then we can construct the typed version of this function
        // (unsafely), which should be safe since we just did the type check above.
        unsafe { Ok(TypedFunc::new_unchecked(*self)) }
    }
}

/// Prepares for entrance into WebAssembly.
///
/// This function will set up context such that `closure` is allowed to call a
/// raw trampoline or a raw WebAssembly function. This *must* be called to do
/// things like catch traps and set up GC properly.
///
/// The `closure` provided receives a default "callee" `VMContext` parameter it
/// can pass to the called wasm function, if desired.
#[inline]
pub(crate) fn invoke_wasm_and_catch_traps(
    store: &mut StoreOpaque<'_>,
    closure: impl FnMut(*mut VMContext),
) -> Result<(), Trap> {
    unsafe {
        let exit = if store.externref_activations_table().stack_canary().is_some() {
            false
        } else {
            enter_wasm(store)?;
            true
        };

        let result = wasmtime_runtime::catch_traps(
            store.vminterrupts(),
            store.signal_handler(),
            store.default_callee(),
            closure,
        );
        if exit {
            exit_wasm(store);
        }
        result.map_err(Trap::from_runtime)
    }
}

/// This function is called to register state within `Store` whenever
/// WebAssembly is entered for the first time within the `Store`. This isn't
/// called when wasm is called recursively within the `Store`.
///
/// This function sets up various limits such as:
///
/// * The stack limit. This is what ensures that we limit the stack space
///   allocated by WebAssembly code and it's relative to the initial stack
///   pointer that called into wasm.
///
/// * Stack canaries for externref gc tracing. Currently the implementation
///   relies on walking frames but the stack walker isn't always 100% reliable,
///   so a canary is used to ensure that if the canary is seen then it's
///   guaranteed all wasm frames have been walked.
///
/// This function may fail if the the stack limit can't be set because an
/// interrupt already happened.
#[inline]
fn enter_wasm(store: &mut StoreOpaque<'_>) -> Result<(), Trap> {
    let stack_pointer = psm::stack_pointer() as usize;

    // Determine the stack pointer where, after which, any wasm code will
    // immediately trap. This is checked on the entry to all wasm functions.
    //
    // Note that this isn't 100% precise. We are requested to give wasm
    // `max_wasm_stack` bytes, but what we're actually doing is giving wasm
    // probably a little less than `max_wasm_stack` because we're
    // calculating the limit relative to this function's approximate stack
    // pointer. Wasm will be executed on a frame beneath this one (or next
    // to it). In any case it's expected to be at most a few hundred bytes
    // of slop one way or another. When wasm is typically given a MB or so
    // (a million bytes) the slop shouldn't matter too much.
    //
    // After we've got the stack limit then we store it into the `stack_limit`
    // variable. Note that the store is an atomic swap to ensure that we can
    // consume any previously-sent interrupt requests. If we found that wasm was
    // previously interrupted then we immediately return a trap (after resetting
    // the stack limit). Otherwise we're good to keep on going.
    //
    // Note the usage of `Relaxed` memory orderings here. This is specifically
    // an optimization in the `Drop` below where a `Relaxed` store is speedier
    // than a `SeqCst` store. The rationale for `Relaxed` here is that the
    // atomic orderings here aren't actually protecting any memory, we're just
    // trying to be atomic with respect to this one location in memory (for when
    // `InterruptHandle` sends us a signal). Due to the lack of needing to
    // synchronize with any other memory it's hoped that the choice of `Relaxed`
    // here should be correct for our use case.
    let wasm_stack_limit = stack_pointer - store.engine().config().max_wasm_stack;
    let interrupts = store.interrupts();
    match interrupts.stack_limit.swap(wasm_stack_limit, Relaxed) {
        wasmtime_environ::INTERRUPTED => {
            // This means that an interrupt happened before we actually
            // called this function, which means that we're now
            // considered interrupted.
            interrupts.stack_limit.store(usize::max_value(), Relaxed);
            return Err(Trap::new_wasm(
                None,
                wasmtime_environ::ir::TrapCode::Interrupt,
                backtrace::Backtrace::new_unresolved(),
            ));
        }
        n => debug_assert_eq!(usize::max_value(), n),
    }
    store
        .externref_activations_table()
        .set_stack_canary(Some(stack_pointer));

    Ok(())
}

#[inline]
fn exit_wasm(store: &mut StoreOpaque<'_>) {
    store.externref_activations_table().set_stack_canary(None);

    // see docs above for why this uses `Relaxed`
    store
        .interrupts()
        .stack_limit
        .store(usize::max_value(), Relaxed);
}

/// A trait implemented for types which can be returned from closures passed to
/// [`Func::wrap`] and friends.
///
/// This trait should not be implemented by user types. This trait may change at
/// any time internally. The types which implement this trait, however, are
/// stable over time.
///
/// For more information see [`Func::wrap`]
pub unsafe trait WasmRet {
    // Same as `WasmTy::Abi`.
    #[doc(hidden)]
    type Abi: Copy;
    #[doc(hidden)]
    type Retptr: Copy;

    // Same as `WasmTy::compatible_with_store`.
    #[doc(hidden)]
    fn compatible_with_store(&self, store: &StoreOpaque) -> bool;

    // Similar to `WasmTy::into_abi_for_arg` but used when host code is
    // returning a value into Wasm, rather than host code passing an argument to
    // a Wasm call. Unlike `into_abi_for_arg`, implementors of this method can
    // raise traps, which means that callers must ensure that
    // `invoke_wasm_and_catch_traps` is on the stack, and therefore this method
    // is unsafe.
    #[doc(hidden)]
    unsafe fn into_abi_for_ret(
        self,
        store: &mut StoreOpaque,
        ptr: Self::Retptr,
    ) -> Result<Self::Abi, Trap>;

    #[doc(hidden)]
    fn func_type(params: impl Iterator<Item = ValType>) -> FuncType;

    #[doc(hidden)]
    unsafe fn wrap_trampoline(ptr: *mut u128, f: impl FnOnce(Self::Retptr) -> Self::Abi);

    // Utilities used to convert an instance of this type to a `Result`
    // explicitly, used when wrapping async functions which always bottom-out
    // in a function that returns a trap because futures can be cancelled.
    #[doc(hidden)]
    type Fallible: WasmRet<Abi = Self::Abi, Retptr = Self::Retptr>;
    #[doc(hidden)]
    fn into_fallible(self) -> Self::Fallible;
    #[doc(hidden)]
    fn fallible_from_trap(trap: Trap) -> Self::Fallible;
}

unsafe impl<T> WasmRet for T
where
    T: WasmTy,
{
    type Abi = <T as WasmTy>::Abi;
    type Retptr = ();
    type Fallible = Result<T, Trap>;

    fn compatible_with_store(&self, store: &StoreOpaque) -> bool {
        <Self as WasmTy>::compatible_with_store(self, store)
    }

    unsafe fn into_abi_for_ret(
        self,
        store: &mut StoreOpaque,
        _retptr: (),
    ) -> Result<Self::Abi, Trap> {
        Ok(<Self as WasmTy>::into_abi(self, store))
    }

    fn func_type(params: impl Iterator<Item = ValType>) -> FuncType {
        FuncType::new(params, Some(<Self as WasmTy>::valtype()))
    }

    unsafe fn wrap_trampoline(ptr: *mut u128, f: impl FnOnce(Self::Retptr) -> Self::Abi) {
        *ptr.cast::<Self::Abi>() = f(());
    }

    fn into_fallible(self) -> Result<T, Trap> {
        Ok(self)
    }

    fn fallible_from_trap(trap: Trap) -> Result<T, Trap> {
        Err(trap)
    }
}

unsafe impl<T> WasmRet for Result<T, Trap>
where
    T: WasmRet,
{
    type Abi = <T as WasmRet>::Abi;
    type Retptr = <T as WasmRet>::Retptr;
    type Fallible = Self;

    fn compatible_with_store(&self, store: &StoreOpaque) -> bool {
        match self {
            Ok(x) => <T as WasmRet>::compatible_with_store(x, store),
            Err(_) => true,
        }
    }

    unsafe fn into_abi_for_ret(
        self,
        store: &mut StoreOpaque,
        retptr: Self::Retptr,
    ) -> Result<Self::Abi, Trap> {
        self.and_then(|val| val.into_abi_for_ret(store, retptr))
    }

    fn func_type(params: impl Iterator<Item = ValType>) -> FuncType {
        T::func_type(params)
    }

    unsafe fn wrap_trampoline(ptr: *mut u128, f: impl FnOnce(Self::Retptr) -> Self::Abi) {
        T::wrap_trampoline(ptr, f)
    }

    fn into_fallible(self) -> Result<T, Trap> {
        self
    }

    fn fallible_from_trap(trap: Trap) -> Result<T, Trap> {
        Err(trap)
    }
}

macro_rules! impl_wasm_host_results {
    ($n:tt $($t:ident)*) => (
        #[allow(non_snake_case)]
        unsafe impl<$($t),*> WasmRet for ($($t,)*)
        where
            $($t: WasmTy,)*
            ($($t::Abi,)*): HostAbi,
        {
            type Abi = <($($t::Abi,)*) as HostAbi>::Abi;
            type Retptr = <($($t::Abi,)*) as HostAbi>::Retptr;
            type Fallible = Result<Self, Trap>;

            #[inline]
            fn compatible_with_store(&self, _store: &StoreOpaque) -> bool {
                let ($($t,)*) = self;
                $( $t.compatible_with_store(_store) && )* true
            }

            #[inline]
            unsafe fn into_abi_for_ret(self, _store: &mut StoreOpaque, ptr: Self::Retptr) -> Result<Self::Abi, Trap> {
                let ($($t,)*) = self;
                let abi = ($($t.into_abi(_store),)*);
                Ok(<($($t::Abi,)*) as HostAbi>::into_abi(abi, ptr))
            }

            fn func_type(params: impl Iterator<Item = ValType>) -> FuncType {
                FuncType::new(
                    params,
                    std::array::IntoIter::new([$($t::valtype(),)*]),
                )
            }

            #[allow(unused_assignments)]
            unsafe fn wrap_trampoline(mut _ptr: *mut u128, f: impl FnOnce(Self::Retptr) -> Self::Abi) {
                let ($($t,)*) = <($($t::Abi,)*) as HostAbi>::call(f);
                $(
                    *_ptr.cast() = $t;
                    _ptr = _ptr.add(1);
                )*
            }

            #[inline]
            fn into_fallible(self) -> Result<Self, Trap> {
                Ok(self)
            }

            #[inline]
            fn fallible_from_trap(trap: Trap) -> Result<Self, Trap> {
                Err(trap)
            }
        }
    )
}

for_each_function_signature!(impl_wasm_host_results);

// Internal trait representing how to communicate tuples of return values across
// an ABI boundary. This internally corresponds to the "wasmtime" ABI inside of
// cranelift itself. Notably the first element of each tuple is returned via the
// typical system ABI (e.g. systemv or fastcall depending on platform) and all
// other values are returned packed via the stack.
//
// This trait helps to encapsulate all the details of that.
#[doc(hidden)]
pub trait HostAbi {
    // A value returned from native functions which return `Self`
    type Abi: Copy;
    // A return pointer, added to the end of the argument list, for native
    // functions that return `Self`. Note that a 0-sized type here should get
    // elided at the ABI level.
    type Retptr: Copy;

    // Converts a value of `self` into its components. Stores necessary values
    // into `ptr` and then returns whatever needs to be returned from the
    // function.
    unsafe fn into_abi(self, ptr: Self::Retptr) -> Self::Abi;

    // Calls `f` with a suitably sized return area and requires `f` to return
    // the raw abi value of the first element of our tuple. This will then
    // unpack the `Retptr` and assemble it with `Self::Abi` to return an
    // instance of the whole tuple.
    unsafe fn call(f: impl FnOnce(Self::Retptr) -> Self::Abi) -> Self;
}

macro_rules! impl_host_abi {
    // Base case, everything is `()`
    (0) => {
        impl HostAbi for () {
            type Abi = ();
            type Retptr = ();

            #[inline]
            unsafe fn into_abi(self, _ptr: Self::Retptr) -> Self::Abi {}

            #[inline]
            unsafe fn call(f: impl FnOnce(Self::Retptr) -> Self::Abi) -> Self {
                f(())
            }
        }
    };

    // In the 1-case the retptr is not present, so it's a 0-sized value.
    (1 $a:ident) => {
        impl<$a: Copy> HostAbi for ($a,) {
            type Abi = $a;
            type Retptr = ();

            unsafe fn into_abi(self, _ptr: Self::Retptr) -> Self::Abi {
                self.0
            }

            unsafe fn call(f: impl FnOnce(Self::Retptr) -> Self::Abi) -> Self {
                (f(()),)
            }
        }
    };

    // This is where the more interesting case happens. The first element of the
    // tuple is returned via `Abi` and all other elements are returned via
    // `Retptr`. We create a `TupleRetNN` structure to represent all of the
    // return values here.
    //
    // Also note that this isn't implemented for the old backend right now due
    // to the original author not really being sure how to implement this in the
    // old backend.
    ($n:tt $t:ident $($u:ident)*) => {paste::paste!{
        #[doc(hidden)]
        #[allow(non_snake_case)]
        #[repr(C)]
        #[cfg(not(feature = "old-x86-backend"))]
        pub struct [<TupleRet $n>]<$($u,)*> {
            $($u: $u,)*
        }

        #[cfg(not(feature = "old-x86-backend"))]
        #[allow(non_snake_case, unused_assignments)]
        impl<$t: Copy, $($u: Copy,)*> HostAbi for ($t, $($u,)*) {
            type Abi = $t;
            type Retptr = *mut [<TupleRet $n>]<$($u,)*>;

            unsafe fn into_abi(self, ptr: Self::Retptr) -> Self::Abi {
                let ($t, $($u,)*) = self;
                // Store the tail of our tuple into the return pointer...
                $((*ptr).$u = $u;)*
                // ... and return the head raw.
                $t
            }

            unsafe fn call(f: impl FnOnce(Self::Retptr) -> Self::Abi) -> Self {
                // Create space to store all the return values and then invoke
                // the function.
                let mut space = std::mem::MaybeUninit::uninit();
                let t = f(space.as_mut_ptr());
                let space = space.assume_init();

                // Use the return value as the head of the tuple and unpack our
                // return area to get the rest of the tuple.
                (t, $(space.$u,)*)
            }
        }
    }};
}

for_each_function_signature!(impl_host_abi);

/// Internal trait implemented for all arguments that can be passed to
/// [`Func::wrap`] and [`Linker::func_wrap`](crate::Linker::func_wrap).
///
/// This trait should not be implemented by external users, it's only intended
/// as an implementation detail of this crate.
pub trait IntoFunc<T, Params, Results>: Send + Sync + 'static {
    #[doc(hidden)]
    fn into_func(self, engine: &Engine) -> (InstanceHandle, VMTrampoline);
}

/// A structure representing the caller's context when creating a function
/// via [`Func::wrap`].
///
/// This structure can be taken as the first parameter of a closure passed to
/// [`Func::wrap`] or other constructors, and serves two purposes:
///
/// * First consumers can use [`Caller<'_, T>`](crate::Caller) to get access to
///   [`StoreContextMut<'_, T>`](crate::StoreContextMut) and/or get access to
///   `T` itself. This means that the [`Caller`] type can serve as a proxy to
///   the original [`Store`](crate::Store) itself and is used to satisfy
///   [`AsContext`] and [`AsContextMut`] bounds.
///
/// * Second a [`Caller`] can be used as the name implies, learning about the
///   caller's context, namely it's exported memory and exported functions. This
///   allows functions which take pointers as arguments to easily read the
///   memory the pointers point into, or if a function is expected to call
///   malloc in the wasm module to reserve space for the output you can do that.
///
/// Host functions which want access to [`Store`](crate::Store)-level state are
/// recommended to use this type.
pub struct Caller<'a, T> {
    pub(crate) store: StoreContextMut<'a, T>,
    caller: &'a InstanceHandle,
}

impl<T> Caller<'_, T> {
    unsafe fn with<R>(caller: *mut VMContext, f: impl FnOnce(Caller<'_, T>) -> R) -> R {
        assert!(!caller.is_null());
        let instance = InstanceHandle::from_vmctx(caller);
        let store = StoreContextMut::from_raw(instance.store());
        f(Caller {
            store,
            caller: &instance,
        })
    }

    fn sub_caller(&mut self) -> Caller<'_, T> {
        Caller {
            store: self.store.as_context_mut(),
            caller: self.caller,
        }
    }

    /// Looks up an export from the caller's module by the `name` given.
    ///
    /// Note that this function is only implemented for the `Extern::Memory`
    /// and the `Extern::Func` types currently. No other exported structures
    /// can be acquired through this method.
    ///
    /// Note that when accessing and calling exported functions, one should
    /// adhere to the guidelines of the interface types proposal.  This method
    /// is a temporary mechanism for accessing the caller's information until
    /// interface types has been fully standardized and implemented. The
    /// interface types proposal will obsolete this type and this will be
    /// removed in the future at some point after interface types is
    /// implemented. If you're relying on this method type it's recommended to
    /// become familiar with interface types to ensure that your use case is
    /// covered by the proposal.
    ///
    /// # Return
    ///
    /// If a memory or function export with the `name` provided was found, then it is
    /// returned as a `Memory`. There are a number of situations, however, where
    /// the memory or function may not be available:
    ///
    /// * The caller instance may not have an export named `name`
    /// * The export named `name` may not be an exported memory
    /// * There may not be a caller available, for example if `Func` was called
    ///   directly from host code.
    ///
    /// It's recommended to take care when calling this API and gracefully
    /// handling a `None` return value.
    pub fn get_export(&mut self, name: &str) -> Option<Extern> {
        // All instances created have a `host_state` with a pointer pointing
        // back to themselves. If this caller doesn't have that `host_state`
        // then it probably means it was a host-created object like `Func::new`
        // which doesn't have any exports we want to return anyway.
        match self
            .caller
            .host_state()
            .downcast_ref::<Instance>()?
            .get_export(&mut self.store, name)?
        {
            Extern::Func(f) => Some(Extern::Func(f)),
            Extern::Memory(f) => Some(Extern::Memory(f)),
            // Intentionally ignore other Extern items here since this API is
            // supposed to be a temporary stop-gap until interface types.
            _ => None,
        }
    }

    /// Access the underlying data owned by this `Store`.
    ///
    /// Same as [`Store::data`](crate::Store::data)
    pub fn data(&self) -> &T {
        self.store.data()
    }

    /// Access the underlying data owned by this `Store`.
    ///
    /// Same as [`Store::data_mut`](crate::Store::data_mut)
    pub fn data_mut(&mut self) -> &mut T {
        self.store.data_mut()
    }

    /// Returns the underlying [`Engine`] this store is connected to.
    pub fn engine(&self) -> &Engine {
        self.store.engine()
    }

    /// Returns an [`InterruptHandle`] to interrupt wasm execution.
    ///
    /// See [`Store::interrupt_handle`](crate::Store::interrupt_handle) for more
    /// information.
    pub fn interrupt_handle(&self) -> Result<InterruptHandle> {
        self.store.interrupt_handle()
    }

    /// Perform garbage collection of `ExternRef`s.
    ///
    /// Same as [`Store::gc`](crate::Store::gc).
    pub fn gc(&mut self) {
        self.store.gc()
    }

    /// Returns the fuel consumed by this store.
    ///
    /// For more information see [`Store::fuel_consumed`](crate::Store::fuel_consumed)
    pub fn fuel_consumed(&self) -> Option<u64> {
        self.store.fuel_consumed()
    }

    /// Inject more fuel into this store to be consumed when executing wasm code.
    ///
    /// For more information see [`Store::add_fuel`](crate::Store::add_fuel)
    pub fn add_fuel(&mut self, fuel: u64) -> Result<()> {
        self.store.add_fuel(fuel)
    }

    /// Configures this `Store` to trap whenever fuel runs out.
    ///
    /// For more information see
    /// [`Store::out_of_fuel_trap`](crate::Store::out_of_fuel_trap)
    pub fn out_of_fuel_trap(&mut self) {
        self.store.out_of_fuel_trap()
    }

    /// Configures this `Store` to yield while executing futures whenever fuel
    /// runs out.
    ///
    /// For more information see
    /// [`Store::out_of_fuel_async_yield`](crate::Store::out_of_fuel_async_yield)
    pub fn out_of_fuel_async_yield(&mut self, injection_count: u32, fuel_to_inject: u64) {
        self.store
            .out_of_fuel_async_yield(injection_count, fuel_to_inject)
    }
}

impl<T> AsContext for Caller<'_, T> {
    type Data = T;
    fn as_context(&self) -> StoreContext<'_, T> {
        self.store.as_context()
    }
}

impl<T> AsContextMut for Caller<'_, T> {
    fn as_context_mut(&mut self) -> StoreContextMut<'_, T> {
        self.store.as_context_mut()
    }
}

fn cross_store_trap() -> Box<dyn Error + Send + Sync> {
    #[derive(Debug)]
    struct CrossStoreError;

    impl Error for CrossStoreError {}

    impl fmt::Display for CrossStoreError {
        fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
            write!(
                f,
                "host function attempted to return cross-`Store` \
                 value to Wasm",
            )
        }
    }

    Box::new(CrossStoreError)
}

macro_rules! impl_into_func {
    ($num:tt $($args:ident)*) => {
        // Implement for functions without a leading `&Caller` parameter,
        // delegating to the implementation below which does have the leading
        // `Caller` parameter.
        #[allow(non_snake_case)]
        impl<T, F, $($args,)* R> IntoFunc<T, ($($args,)*), R> for F
        where
            F: Fn($($args),*) -> R + Send + Sync + 'static,
            $($args: WasmTy,)*
            R: WasmRet,
        {
            fn into_func(self, engine: &Engine) -> (InstanceHandle, VMTrampoline) {
                let f = move |_: Caller<'_, T>, $($args:$args),*| {
                    self($($args),*)
                };

                f.into_func(engine)
            }
        }

        #[allow(non_snake_case)]
        impl<T, F, $($args,)* R> IntoFunc<T, (Caller<'_, T>, $($args,)*), R> for F
        where
            F: Fn(Caller<'_, T>, $($args),*) -> R + Send + Sync + 'static,
            $($args: WasmTy,)*
            R: WasmRet,
        {
            fn into_func(self, engine: &Engine) -> (InstanceHandle, VMTrampoline) {
                /// This shim is called by Wasm code, constructs a `Caller`,
                /// calls the wrapped host function, and returns the translated
                /// result back to Wasm.
                ///
                /// Note that this shim's ABI must *exactly* match that expected
                /// by Cranelift, since Cranelift is generating raw function
                /// calls directly to this function.
                unsafe extern "C" fn wasm_to_host_shim<T, F, $($args,)* R>(
                    vmctx: *mut VMContext,
                    caller_vmctx: *mut VMContext,
                    $( $args: $args::Abi, )*
                    retptr: R::Retptr,
                ) -> R::Abi
                where
                    F: Fn(Caller<'_, T>, $( $args ),*) -> R + 'static,
                    $( $args: WasmTy, )*
                    R: WasmRet,
                {
                    enum CallResult<U> {
                        Ok(U),
                        Trap(Box<dyn Error + Send + Sync>),
                        Panic(Box<dyn std::any::Any + Send>),
                    }

                    // Note that this `result` is intentionally scoped into a
                    // separate block. Handling traps and panics will involve
                    // longjmp-ing from this function which means we won't run
                    // destructors. As a result anything requiring a destructor
                    // should be part of this block, and the long-jmp-ing
                    // happens after the block in handling `CallResult`.
                    let result = Caller::with(caller_vmctx, |mut caller| {
                        let state = (*vmctx).host_state();
                        // Double-check ourselves in debug mode, but we control
                        // the `Any` here so an unsafe downcast should also
                        // work.
                        debug_assert!(state.is::<F>());
                        let func = &*(state as *const _ as *const F);

                        let ret = {
                            panic::catch_unwind(AssertUnwindSafe(|| {
                                if let Err(trap) = caller.store.0.entering_native_hook() {
                                    return R::fallible_from_trap(trap);
                                }
                                let mut _store = caller.sub_caller().store.opaque();
                                $(let $args = $args::from_abi($args, &mut _store);)*
                                let r = func(
                                    caller.sub_caller(),
                                    $( $args, )*
                                );
                                if let Err(trap) = caller.store.0.exiting_native_hook() {
                                    return R::fallible_from_trap(trap);
                                }
                                r.into_fallible()
                            }))
                        };

                        // Note that we need to be careful when dealing with traps
                        // here. Traps are implemented with longjmp/setjmp meaning
                        // that it's not unwinding and consequently no Rust
                        // destructors are run. We need to be careful to ensure that
                        // nothing on the stack needs a destructor when we exit
                        // abnormally from this `match`, e.g. on `Err`, on
                        // cross-store-issues, or if `Ok(Err)` is raised.
                        match ret {
                            Err(panic) => CallResult::Panic(panic),
                            Ok(ret) => {
                                // Because the wrapped function is not `unsafe`, we
                                // can't assume it returned a value that is
                                // compatible with this store.
                                let mut store = caller.store.opaque();
                                if !ret.compatible_with_store(&store) {
                                    CallResult::Trap(cross_store_trap())
                                } else {
                                    match ret.into_abi_for_ret(&mut store, retptr) {
                                        Ok(val) => CallResult::Ok(val),
                                        Err(trap) => CallResult::Trap(trap.into()),
                                    }
                                }

                            }
                        }
                    });

                    match result {
                        CallResult::Ok(val) => val,
                        CallResult::Trap(trap) => raise_user_trap(trap),
                        CallResult::Panic(panic) => wasmtime_runtime::resume_panic(panic),
                    }
                }

                /// This trampoline allows host code to indirectly call the
                /// wrapped function (e.g. via `Func::call` on a `funcref` that
                /// happens to reference our wrapped function).
                ///
                /// It reads the arguments out of the incoming `args` array,
                /// calls the given function pointer, and then stores the result
                /// back into the `args` array.
                unsafe extern "C" fn host_trampoline<$($args,)* R>(
                    callee_vmctx: *mut VMContext,
                    caller_vmctx: *mut VMContext,
                    ptr: *const VMFunctionBody,
                    args: *mut u128,
                )
                where
                    $($args: WasmTy,)*
                    R: WasmRet,
                {
                    let ptr = mem::transmute::<
                        *const VMFunctionBody,
                        unsafe extern "C" fn(
                            *mut VMContext,
                            *mut VMContext,
                            $( $args::Abi, )*
                            R::Retptr,
                        ) -> R::Abi,
                    >(ptr);

                    let mut _n = 0;
                    $(
                        let $args = *args.add(_n).cast::<$args::Abi>();
                        _n += 1;
                    )*
                    R::wrap_trampoline(args, |retptr| {
                        ptr(callee_vmctx, caller_vmctx, $( $args, )* retptr)
                    });
                }

                let ty = R::func_type(
                    None::<ValType>.into_iter()
                        $(.chain(Some($args::valtype())))*
                );

                let shared_signature_id = engine.signatures().register(ty.as_wasm_func_type());

                let trampoline = host_trampoline::<$($args,)* R>;


                let instance = unsafe {
                    crate::trampoline::create_raw_function(
                        std::slice::from_raw_parts_mut(
                            wasm_to_host_shim::<T, F, $($args,)* R> as *mut _,
                            0,
                        ),
                        shared_signature_id,
                        Box::new(self),
                    )
                    .expect("failed to create raw function")
                };

                (instance, trampoline)
            }
        }
    }
}

for_each_function_signature!(impl_into_func);

/// Representation of a host-defined function.
///
/// This is used for `Func::new` but also for `Linker`-defined functions. For
/// `Func::new` this is stored within a `Store`, and for `Linker`-defined
/// functions they wrap this up in `Arc` to enable shared ownership of this
/// across many stores.
///
/// Technically this structure needs a `<T>` type parameter to connect to the
/// `Store<T>` itself, but that's an unsafe contract of using this for now
/// rather than part of the struct type (to avoid `Func<T>` in the API).
pub(crate) struct HostFunc {
    // Owned `*mut VMContext` allocation. Deallocated when this `HostFunc` is
    // dropped.
    instance: InstanceHandle,
    // Trampoline to enter this function from Rust.
    trampoline: VMTrampoline,
    // The loaded `ExportFunction` from the above `InstanceHandle` which has raw
    // pointers and information about how to actually call this function (e.g.
    // the actual address in JIT code and the vm shared function index).
    export: ExportFunction,
    // Stored to unregister this function's signature with the engine when this
    // is dropped.
    engine: Engine,
}

impl HostFunc {
    /// Analog of [`Func::new`]
    pub fn new<T>(
        engine: &Engine,
        ty: FuncType,
        func: impl Fn(Caller<'_, T>, &[Val], &mut [Val]) -> Result<(), Trap> + Send + Sync + 'static,
    ) -> Self {
        let ty_clone = ty.clone();

        // Create a trampoline that converts raw u128 values to `Val`
        let func = Box::new(move |caller_vmctx, values_vec: *mut u128| unsafe {
            Caller::with(caller_vmctx, |caller| {
                Func::invoke(caller, &ty_clone, values_vec, &func)
            })
        });

        let (instance, trampoline) = crate::trampoline::create_function(&ty, func, engine)
            .expect("failed to create function");
        HostFunc::_new(engine, instance, trampoline)
    }

    /// Analog of [`Func::wrap`]
    pub fn wrap<T, Params, Results>(
        engine: &Engine,
        func: impl IntoFunc<T, Params, Results>,
    ) -> Self {
        let (instance, trampoline) = func.into_func(engine);
        HostFunc::_new(engine, instance, trampoline)
    }

    /// Requires that this function's signature is already registered within
    /// `Engine`. This happens automatically during the above two constructors.
    fn _new(engine: &Engine, instance: InstanceHandle, trampoline: VMTrampoline) -> Self {
        let idx = EntityIndex::Function(FuncIndex::from_u32(0));
        let export = match instance.lookup_by_declaration(&idx) {
            wasmtime_runtime::Export::Function(f) => f,
            _ => unreachable!(),
        };

        HostFunc {
            instance,
            trampoline,
            export,
            engine: engine.clone(),
        }
    }

    /// Inserts this `HostFunc` into a `Store`, returning the `Func` pointing to
    /// it.
    ///
    /// # Unsafety
    ///
    /// Can only be inserted into stores with a matching `T` relative to when
    /// this `HostFunc` was first created.
    pub unsafe fn to_func(self: &Arc<Self>, store: &mut StoreOpaque<'_>) -> Func {
        self.register_trampoline(store);
        let me = self.clone();
        Func(store.store_data_mut().insert(FuncData::SharedHost(me)))
    }

    /// Same as [`HostFunc::to_func`], different ownership.
    unsafe fn into_func(self, store: &mut StoreOpaque<'_>) -> Func {
        self.register_trampoline(store);
        Func(store.store_data_mut().insert(FuncData::Host(self)))
    }

    unsafe fn register_trampoline(&self, store: &mut StoreOpaque<'_>) {
        let idx = self.export.anyfunc.as_ref().type_index;
        store.register_host_trampoline(idx, self.trampoline);
    }

    pub(crate) fn sig_index(&self) -> VMSharedSignatureIndex {
        unsafe { self.export.anyfunc.as_ref().type_index }
    }
}

impl Drop for HostFunc {
    fn drop(&mut self) {
        unsafe {
            self.engine
                .signatures()
                .unregister(self.export.anyfunc.as_ref().type_index);

            // Host functions are always allocated with the default (on-demand)
            // allocator
            OnDemandInstanceAllocator::default().deallocate(&self.instance);
        }
    }
}

impl FuncData {
    fn trampoline(&self) -> VMTrampoline {
        match self {
            FuncData::StoreOwned { trampoline, .. } => *trampoline,
            FuncData::SharedHost(host) => host.trampoline,
            FuncData::Host(host) => host.trampoline,
        }
    }

    #[inline]
    fn export(&self) -> &ExportFunction {
        match self {
            FuncData::StoreOwned { export, .. } => export,
            FuncData::SharedHost(host) => &host.export,
            FuncData::Host(host) => &host.export,
        }
    }
}
