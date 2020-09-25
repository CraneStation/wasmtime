use crate::runtime::StoreInner;
use crate::trampoline::StoreInstanceHandle;
use crate::{Extern, ExternRef, FuncType, Memory, Store, Trap, Val, ValType};
use anyhow::{bail, ensure, Context as _, Result};
use smallvec::{smallvec, SmallVec};
use std::cmp::max;
use std::fmt;
use std::mem;
use std::panic::{self, AssertUnwindSafe};
use std::ptr::{self, NonNull};
use std::rc::Weak;
use wasmtime_runtime::{
    raise_user_trap, Export, InstanceHandle, VMContext, VMFunctionBody, VMSharedSignatureIndex,
    VMTrampoline,
};

/// A WebAssembly function which can be called.
///
/// This type can represent a number of callable items, such as:
///
/// * An exported function from a WebAssembly module.
/// * A user-defined function used to satisfy an import.
///
/// These types of callable items are all wrapped up in this `Func` and can be
/// used to both instantiate an [`Instance`] as well as be extracted from an
/// [`Instance`].
///
/// [`Instance`]: crate::Instance
///
/// # `Func` and `Clone`
///
/// Functions are internally reference counted so you can `clone` a `Func`. The
/// cloning process only performs a shallow clone, so two cloned `Func`
/// instances are equivalent in their functionality.
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
/// let store = Store::new(&engine);
/// let module = Module::new(&engine, r#"(module (func (export "foo")))"#)?;
/// let instance = Instance::new(&store, &module, &[])?;
/// let foo = instance.get_func("foo").expect("export wasn't a function");
///
/// // Work with `foo` as a `Func` at this point, such as calling it
/// // dynamically...
/// match foo.call(&[]) {
///     Ok(result) => { /* ... */ }
///     Err(trap) => {
///         panic!("execution of `foo` resulted in a wasm trap: {}", trap);
///     }
/// }
/// foo.call(&[])?;
///
/// // ... or we can make a static assertion about its signature and call it.
/// // Our first call here can fail if the signatures don't match, and then the
/// // second call can fail if the function traps (like the `match` above).
/// let foo = foo.get0::<()>()?;
/// foo()?;
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
/// let store = Store::default();
///
/// // Create a custom `Func` which can execute arbitrary code inside of the
/// // closure.
/// let add = Func::wrap(&store, |a: i32, b: i32| -> i32 { a + b });
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
/// let instance = Instance::new(&store, &module, &[add.into()])?;
/// let call_add_twice = instance.get_func("call_add_twice").expect("export wasn't a function");
/// let call_add_twice = call_add_twice.get0::<i32>()?;
///
/// assert_eq!(call_add_twice()?, 10);
/// # Ok(())
/// # }
/// ```
///
/// Or you could also create an entirely dynamic `Func`!
///
/// ```
/// # use wasmtime::*;
/// # fn main() -> anyhow::Result<()> {
/// let store = Store::default();
///
/// // Here we need to define the type signature of our `Double` function and
/// // then wrap it up in a `Func`
/// let double_type = wasmtime::FuncType::new(
///     Box::new([wasmtime::ValType::I32]),
///     Box::new([wasmtime::ValType::I32])
/// );
/// let double = Func::new(&store, double_type, |_, params, results| {
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
/// let instance = Instance::new(&store, &module, &[double.into()])?;
/// // .. work with `instance` if necessary
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct Func {
    instance: StoreInstanceHandle,
    trampoline: VMTrampoline,
    export: wasmtime_runtime::ExportFunction,
}

macro_rules! getters {
    ($(
        $(#[$doc:meta])*
        ($name:ident $(,$args:ident)*)
    )*) => ($(
        $(#[$doc])*
        #[allow(non_snake_case)]
        pub fn $name<$($args,)* R>(&self)
            -> anyhow::Result<impl Fn($($args,)*) -> Result<R, Trap>>
        where
            $($args: WasmTy,)*
            R: WasmTy,
        {
            // Verify all the paramers match the expected parameters, and that
            // there are no extra parameters...
            let ty = self.ty();
            let mut params = ty.params().iter().cloned();
            let n = 0;
            $(
                let n = n + 1;
                $args::matches(&mut params)
                    .with_context(|| format!("Type mismatch in argument {}", n))?;
            )*
            ensure!(params.next().is_none(), "Type mismatch: too many arguments (expected {})", n);

            // ... then do the same for the results...
            let mut results = ty.results().iter().cloned();
            R::matches(&mut results)
                .context("Type mismatch in return type")?;
            ensure!(results.next().is_none(), "Type mismatch: too many return values (expected 1)");

            // Pass the instance into the closure so that we keep it live for
            // the lifetime of the closure. Pass the `anyfunc` in so that we can
            // call it.
            let instance = self.instance.clone();
            let anyfunc = self.export.anyfunc;

            // ... and then once we've passed the typechecks we can hand out our
            // object since our `transmute` below should be safe!
            Ok(move |$($args: $args),*| -> Result<R, Trap> {
                unsafe {
                    let fnptr = mem::transmute::<
                        *const VMFunctionBody,
                        unsafe extern "C" fn(
                            *mut VMContext,
                            *mut VMContext,
                            $( $args::Abi, )*
                        ) -> R::Abi,
                    >(anyfunc.as_ref().func_ptr.as_ptr());

                    let mut ret = None;

                    let weak_store = instance.store.weak();
                    let weak_store = WeakStore(&weak_store);

                    $(
                        // Because this returned closure is not marked `unsafe`,
                        // we have to check that incoming values are compatible
                        // with our store.
                        if !$args.compatible_with_store(weak_store) {
                            return Err(Trap::new(
                                "attempt to pass cross-`Store` value to Wasm as function argument"
                            ));
                        }

                        let $args = $args.into_abi_for_arg(weak_store);
                    )*

                    invoke_wasm_and_catch_traps(anyfunc.as_ref().vmctx, &instance.store, || {
                        ret = Some(fnptr(
                            anyfunc.as_ref().vmctx,
                            ptr::null_mut(),
                            $( $args, )*
                        ));
                    })?;

                    Ok(R::from_abi(ret.unwrap(), weak_store))
                }
            })
        }
    )*)
}

impl Func {
    /// Creates a new `Func` with the given arguments, typically to create a
    /// user-defined function to pass as an import to a module.
    ///
    /// * `store` - a cache of data where information is stored, typically
    ///   shared with a [`Module`](crate::Module).
    ///
    /// * `ty` - the signature of this function, used to indicate what the
    ///   inputs and outputs are, which must be WebAssembly types.
    ///
    /// * `func` - the native code invoked whenever this `Func` will be called.
    ///   This closure is provided a [`Caller`] as its first argument to learn
    ///   information about the caller, and then it's passed a list of
    ///   parameters as a slice along with a mutable slice of where to write
    ///   results.
    ///
    /// Note that the implementation of `func` must adhere to the `ty`
    /// signature given, error or traps may occur if it does not respect the
    /// `ty` signature.
    ///
    /// Additionally note that this is quite a dynamic function since signatures
    /// are not statically known. For a more performant `Func` it's recommended
    /// to use [`Func::wrap`] if you can because with statically known
    /// signatures the engine can optimize the implementation much more.
    pub fn new(
        store: &Store,
        ty: FuncType,
        func: impl Fn(Caller<'_>, &[Val], &mut [Val]) -> Result<(), Trap> + 'static,
    ) -> Self {
        let store_weak = store.weak();
        let ty_clone = ty.clone();

        // Create our actual trampoline function which translates from a bunch
        // of bit patterns on the stack to actual instances of `Val` being
        // passed to the given function.
        let func = Box::new(move |caller_vmctx, values_vec: *mut u128| {
            // We have a dynamic guarantee that `values_vec` has the right
            // number of arguments and the right types of arguments. As a result
            // we should be able to safely run through them all and read them.
            const STACK_ARGS: usize = 4;
            const STACK_RETURNS: usize = 2;
            let mut args: SmallVec<[Val; STACK_ARGS]> =
                SmallVec::with_capacity(ty_clone.params().len());
            let store = Store::upgrade(&store_weak).unwrap();
            for (i, ty) in ty_clone.params().iter().enumerate() {
                unsafe {
                    let val = Val::read_value_from(&store, values_vec.add(i), ty);
                    args.push(val);
                }
            }

            let mut returns: SmallVec<[Val; STACK_RETURNS]> =
                smallvec![Val::null(); ty_clone.results().len()];

            func(
                Caller {
                    store: &store_weak,
                    caller_vmctx,
                },
                &args,
                &mut returns,
            )?;

            // Unlike our arguments we need to dynamically check that the return
            // values produced are correct. There could be a bug in `func` that
            // produces the wrong number or wrong types of values, and we need
            // to catch that here.
            for (i, (ret, ty)) in returns.into_iter().zip(ty_clone.results()).enumerate() {
                if ret.ty() != *ty {
                    return Err(Trap::new(
                        "function attempted to return an incompatible value",
                    ));
                }
                unsafe {
                    ret.write_value_to(&store, values_vec.add(i));
                }
            }
            Ok(())
        });
        let (instance, export, trampoline) =
            crate::trampoline::generate_func_export(&ty, func, store).expect("generated func");
        Func {
            instance,
            trampoline,
            export,
        }
    }

    pub(crate) unsafe fn from_caller_checked_anyfunc(
        store: &Store,
        anyfunc: *mut wasmtime_runtime::VMCallerCheckedAnyfunc,
    ) -> Option<Self> {
        let anyfunc = NonNull::new(anyfunc)?;
        debug_assert!(
            anyfunc.as_ref().type_index != wasmtime_runtime::VMSharedSignatureIndex::default()
        );

        let instance_handle = wasmtime_runtime::InstanceHandle::from_vmctx(anyfunc.as_ref().vmctx);
        let export = wasmtime_runtime::ExportFunction { anyfunc };
        let instance = store.existing_instance_handle(instance_handle);
        let f = Func::from_wasmtime_function(export, instance);
        Some(f)
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
    /// | Rust Return Type  | WebAssembly Return Type | Meaning           |
    /// |-------------------|-------------------------|-------------------|
    /// | `()`              | nothing                 | no return value   |
    /// | `Result<T, Trap>` | `T`                     | function may trap |
    ///
    /// At this time multi-value returns are not supported, and supporting this
    /// is the subject of [#1178].
    ///
    /// [#1178]: https://github.com/bytecodealliance/wasmtime/issues/1178
    ///
    /// Finally you can also optionally take [`Caller`] as the first argument of
    /// your closure. If inserted then you're able to inspect the caller's
    /// state, for example the [`Memory`] it has exported so you can read what
    /// pointers point to.
    ///
    /// Note that when using this API, the intention is to create as thin of a
    /// layer as possible for when WebAssembly calls the function provided. With
    /// sufficient inlining and optimization the WebAssembly will call straight
    /// into `func` provided, with no extra fluff entailed.
    ///
    /// # Examples
    ///
    /// First up we can see how simple wasm imports can be implemented, such
    /// as a function that adds its two arguments and returns the result.
    ///
    /// ```
    /// # use wasmtime::*;
    /// # fn main() -> anyhow::Result<()> {
    /// # let store = Store::default();
    /// let add = Func::wrap(&store, |a: i32, b: i32| a + b);
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
    /// let instance = Instance::new(&store, &module, &[add.into()])?;
    /// let foo = instance.get_func("foo").unwrap().get2::<i32, i32, i32>()?;
    /// assert_eq!(foo(1, 2)?, 3);
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
    /// # let store = Store::default();
    /// let add = Func::wrap(&store, |a: i32, b: i32| {
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
    /// let instance = Instance::new(&store, &module, &[add.into()])?;
    /// let foo = instance.get_func("foo").unwrap().get2::<i32, i32, i32>()?;
    /// assert_eq!(foo(1, 2)?, 3);
    /// assert!(foo(i32::max_value(), 1).is_err());
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// And don't forget all the wasm types are supported!
    ///
    /// ```
    /// # use wasmtime::*;
    /// # fn main() -> anyhow::Result<()> {
    /// # let store = Store::default();
    /// let debug = Func::wrap(&store, |a: i32, b: u32, c: f32, d: i64, e: u64, f: f64| {
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
    /// let instance = Instance::new(&store, &module, &[debug.into()])?;
    /// let foo = instance.get_func("foo").unwrap().get0::<()>()?;
    /// foo()?;
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
    /// # let store = Store::default();
    /// let log_str = Func::wrap(&store, |caller: Caller<'_>, ptr: i32, len: i32| {
    ///     let mem = match caller.get_export("memory") {
    ///         Some(Extern::Memory(mem)) => mem,
    ///         _ => return Err(Trap::new("failed to find host memory")),
    ///     };
    ///
    ///     // We're reading raw wasm memory here so we need `unsafe`. Note
    ///     // though that this should be safe because we don't reenter wasm
    ///     // while we're reading wasm memory, nor should we clash with
    ///     // any other memory accessors (assuming they're well-behaved
    ///     // too).
    ///     unsafe {
    ///         let data = mem.data_unchecked()
    ///             .get(ptr as u32 as usize..)
    ///             .and_then(|arr| arr.get(..len as u32 as usize));
    ///         let string = match data {
    ///             Some(data) => match str::from_utf8(data) {
    ///                 Ok(s) => s,
    ///                 Err(_) => return Err(Trap::new("invalid utf-8")),
    ///             },
    ///             None => return Err(Trap::new("pointer/length out of bounds")),
    ///         };
    ///         assert_eq!(string, "Hello, world!");
    ///         println!("{}", string);
    ///     }
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
    /// let instance = Instance::new(&store, &module, &[log_str.into()])?;
    /// let foo = instance.get_func("foo").unwrap().get0::<()>()?;
    /// foo()?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn wrap<Params, Results>(store: &Store, func: impl IntoFunc<Params, Results>) -> Func {
        func.into_func(store)
    }

    pub(crate) fn sig_index(&self) -> VMSharedSignatureIndex {
        unsafe { self.export.anyfunc.as_ref().type_index }
    }

    /// Returns the underlying wasm type that this `Func` has.
    pub fn ty(&self) -> FuncType {
        // Signatures should always be registered in the store's registry of
        // shared signatures, so we should be able to unwrap safely here.
        let wft = self.instance.store.lookup_signature(self.sig_index());

        // This is only called with `Export::Function`, and since it's coming
        // from wasmtime_runtime itself we should support all the types coming
        // out of it, so assert such here.
        FuncType::from_wasm_func_type(&wft)
    }

    /// Returns the number of parameters that this function takes.
    pub fn param_arity(&self) -> usize {
        let sig = self
            .instance
            .store
            .lookup_signature(unsafe { self.export.anyfunc.as_ref().type_index });
        sig.params.len()
    }

    /// Returns the number of results this function produces.
    pub fn result_arity(&self) -> usize {
        let sig = self
            .instance
            .store
            .lookup_signature(unsafe { self.export.anyfunc.as_ref().type_index });
        sig.returns.len()
    }

    /// Invokes this function with the `params` given, returning the results and
    /// any trap, if one occurs.
    ///
    /// The `params` here must match the type signature of this `Func`, or a
    /// trap will occur. If a trap occurs while executing this function, then a
    /// trap will also be returned.
    ///
    /// This function should not panic unless the underlying function itself
    /// initiates a panic.
    pub fn call(&self, params: &[Val]) -> Result<Box<[Val]>> {
        // We need to perform a dynamic check that the arguments given to us
        // match the signature of this function and are appropriate to pass to
        // this function. This involves checking to make sure we have the right
        // number and types of arguments as well as making sure everything is
        // from the same `Store`.
        let my_ty = self.ty();
        if my_ty.params().len() != params.len() {
            bail!(
                "expected {} arguments, got {}",
                my_ty.params().len(),
                params.len()
            );
        }

        let mut values_vec = vec![0; max(params.len(), my_ty.results().len())];

        // Store the argument values into `values_vec`.
        let param_tys = my_ty.params().iter();
        for ((arg, slot), ty) in params.iter().cloned().zip(&mut values_vec).zip(param_tys) {
            if arg.ty() != *ty {
                bail!(
                    "argument type mismatch: found {} but expected {}",
                    arg.ty(),
                    ty
                );
            }
            if !arg.comes_from_same_store(&self.instance.store) {
                bail!("cross-`Store` values are not currently supported");
            }
            unsafe {
                arg.write_value_to(&self.instance.store, slot);
            }
        }

        // Call the trampoline.
        unsafe {
            let anyfunc = self.export.anyfunc.as_ref();
            invoke_wasm_and_catch_traps(anyfunc.vmctx, &self.instance.store, || {
                (self.trampoline)(
                    anyfunc.vmctx,
                    ptr::null_mut(),
                    anyfunc.func_ptr.as_ptr(),
                    values_vec.as_mut_ptr(),
                )
            })?;
        }

        // Load the return values out of `values_vec`.
        let mut results = Vec::with_capacity(my_ty.results().len());
        for (index, ty) in my_ty.results().iter().enumerate() {
            unsafe {
                let ptr = values_vec.as_ptr().add(index);
                results.push(Val::read_value_from(&self.instance.store, ptr, ty));
            }
        }

        Ok(results.into())
    }

    pub(crate) fn caller_checked_anyfunc(
        &self,
    ) -> NonNull<wasmtime_runtime::VMCallerCheckedAnyfunc> {
        self.export.anyfunc
    }

    pub(crate) fn from_wasmtime_function(
        export: wasmtime_runtime::ExportFunction,
        instance: StoreInstanceHandle,
    ) -> Self {
        // Each function signature in a module should have a trampoline stored
        // on that module as well, so unwrap the result here since otherwise
        // it's a bug in wasmtime.
        let trampoline = instance
            .trampoline(unsafe { export.anyfunc.as_ref().type_index })
            .expect("failed to retrieve trampoline from module");

        Func {
            instance,
            export,
            trampoline,
        }
    }

    getters! {
        /// Extracts a natively-callable object from this `Func`, if the
        /// signature matches.
        ///
        /// See the [`Func::get1`] method for more documentation.
        (get0)

        /// Extracts a natively-callable object from this `Func`, if the
        /// signature matches.
        ///
        /// This function serves as an optimized version of the [`Func::call`]
        /// method if the type signature of a function is statically known to
        /// the program. This method is faster than `call` on a few metrics:
        ///
        /// * Runtime type-checking only happens once, when this method is
        ///   called.
        /// * The result values, if any, aren't boxed into a vector.
        /// * Arguments and return values don't go through boxing and unboxing.
        /// * No trampolines are used to transfer control flow to/from JIT code,
        ///   instead this function jumps directly into JIT code.
        ///
        /// For more information about which Rust types match up to which wasm
        /// types, see the documentation on [`Func::wrap`].
        ///
        /// # Return
        ///
        /// This function will return `None` if the type signature asserted
        /// statically does not match the runtime type signature. `Some`,
        /// however, will be returned if the underlying function takes one
        /// parameter of type `A` and returns the parameter `R`. Currently `R`
        /// can either be `()` (no return values) or one wasm type. At this time
        /// a multi-value return isn't supported.
        ///
        /// The returned closure will always return a `Result<R, Trap>` and an
        /// `Err` is returned if a trap happens while the wasm is executing.
        (get1, A1)

        /// Extracts a natively-callable object from this `Func`, if the
        /// signature matches.
        ///
        /// See the [`Func::get1`] method for more documentation.
        (get2, A1, A2)

        /// Extracts a natively-callable object from this `Func`, if the
        /// signature matches.
        ///
        /// See the [`Func::get1`] method for more documentation.
        (get3, A1, A2, A3)

        /// Extracts a natively-callable object from this `Func`, if the
        /// signature matches.
        ///
        /// See the [`Func::get1`] method for more documentation.
        (get4, A1, A2, A3, A4)

        /// Extracts a natively-callable object from this `Func`, if the
        /// signature matches.
        ///
        /// See the [`Func::get1`] method for more documentation.
        (get5, A1, A2, A3, A4, A5)

        /// Extracts a natively-callable object from this `Func`, if the
        /// signature matches.
        ///
        /// See the [`Func::get1`] method for more documentation.
        (get6, A1, A2, A3, A4, A5, A6)

        /// Extracts a natively-callable object from this `Func`, if the
        /// signature matches.
        ///
        /// See the [`Func::get1`] method for more documentation.
        (get7, A1, A2, A3, A4, A5, A6, A7)

        /// Extracts a natively-callable object from this `Func`, if the
        /// signature matches.
        ///
        /// See the [`Func::get1`] method for more documentation.
        (get8, A1, A2, A3, A4, A5, A6, A7, A8)

        /// Extracts a natively-callable object from this `Func`, if the
        /// signature matches.
        ///
        /// See the [`Func::get1`] method for more documentation.
        (get9, A1, A2, A3, A4, A5, A6, A7, A8, A9)

        /// Extracts a natively-callable object from this `Func`, if the
        /// signature matches.
        ///
        /// See the [`Func::get1`] method for more documentation.
        (get10, A1, A2, A3, A4, A5, A6, A7, A8, A9, A10)

        /// Extracts a natively-callable object from this `Func`, if the
        /// signature matches.
        ///
        /// See the [`Func::get1`] method for more documentation.
        (get11, A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11)

        /// Extracts a natively-callable object from this `Func`, if the
        /// signature matches.
        ///
        /// See the [`Func::get1`] method for more documentation.
        (get12, A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12)

        /// Extracts a natively-callable object from this `Func`, if the
        /// signature matches.
        ///
        /// See the [`Func::get1`] method for more documentation.
        (get13, A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13)

        /// Extracts a natively-callable object from this `Func`, if the
        /// signature matches.
        ///
        /// See the [`Func::get1`] method for more documentation.
        (get14, A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14)

        /// Extracts a natively-callable object from this `Func`, if the
        /// signature matches.
        ///
        /// See the [`Func::get1`] method for more documentation.
        (get15, A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15)
    }

    /// Get a reference to this function's store.
    pub fn store(&self) -> &Store {
        &self.instance.store
    }

    pub(crate) fn matches_expected(&self, expected: VMSharedSignatureIndex) -> bool {
        self.sig_index() == expected
    }

    pub(crate) fn vmimport(&self) -> wasmtime_runtime::VMFunctionImport {
        unsafe {
            let f = self.caller_checked_anyfunc();
            wasmtime_runtime::VMFunctionImport {
                body: f.as_ref().func_ptr,
                vmctx: f.as_ref().vmctx,
            }
        }
    }
}

impl fmt::Debug for Func {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Func")
    }
}

pub(crate) fn invoke_wasm_and_catch_traps(
    vmctx: *mut VMContext,
    store: &Store,
    closure: impl FnMut(),
) -> Result<(), Trap> {
    let signalhandler = store.signal_handler();
    unsafe {
        let canary = 0;
        let _auto_reset_canary = store
            .externref_activations_table()
            .set_stack_canary(&canary);

        wasmtime_runtime::catch_traps(
            vmctx,
            store.engine().config().max_wasm_stack,
            |addr| store.is_in_jit_code(addr),
            signalhandler.as_deref(),
            Some(store.engine()),
            closure,
        )
        .map_err(Trap::from_runtime)
    }
}

// Public (but hidden) wrapper around a `Weak<StoreInner>` so that we can use it
// in public (but hidden) trait methods.
#[doc(hidden)]
#[derive(Clone, Copy)]
pub struct WeakStore<'a>(&'a Weak<StoreInner>);

/// A trait implemented for types which can be arguments to closures passed to
/// [`Func::wrap`] and friends.
///
/// This trait should not be implemented by user types. This trait may change at
/// any time internally. The types which implement this trait, however, are
/// stable over time.
///
/// For more information see [`Func::wrap`]
pub unsafe trait WasmTy {
    // The raw ABI representation of this type inside Wasm.
    #[doc(hidden)]
    type Abi: Copy;

    // Is this value compatible with the given store?
    #[doc(hidden)]
    fn compatible_with_store<'a>(&self, store: WeakStore<'a>) -> bool;

    // Convert this value into its ABI representation, when passing a value into
    // Wasm as an argument.
    #[doc(hidden)]
    fn into_abi_for_arg<'a>(self, store: WeakStore<'a>) -> Self::Abi;

    // Convert from the raw ABI representation back into `Self`, when receiving
    // a value from Wasm.
    //
    // Safety: The abi value *must* have be valid for this type (e.g. for
    // `externref`, it must be a valid raw `VMExternRef` pointer, not some
    // random, dangling pointer).
    #[doc(hidden)]
    unsafe fn from_abi<'a>(abi: Self::Abi, store: WeakStore<'a>) -> Self;

    // Add this type to the given vec of expected valtypes.
    #[doc(hidden)]
    fn push(dst: &mut Vec<ValType>);

    // Does the next valtype(s) match this type?
    #[doc(hidden)]
    fn matches(tys: impl Iterator<Item = ValType>) -> anyhow::Result<()>;

    // Load this type's raw ABI representation from an args array.
    #[doc(hidden)]
    unsafe fn load_from_args(ptr: &mut *const u128) -> Self::Abi;

    // Store this type's raw ABI representation into an args array.
    #[doc(hidden)]
    unsafe fn store_to_args(abi: Self::Abi, ptr: *mut u128);
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

    // Same as `WasmTy::compatible_with_store`.
    #[doc(hidden)]
    fn compatible_with_store<'a>(&self, store: WeakStore<'a>) -> bool;

    // Similar to `WasmTy::into_abi_for_arg` but used when host code is
    // returning a value into Wasm, rather than host code passing an argument to
    // a Wasm call. Unlike `into_abi_for_arg`, implementors of this method can
    // raise traps, which means that callers must ensure that
    // `invoke_wasm_and_catch_traps` is on the stack, and therefore this method
    // is unsafe.
    #[doc(hidden)]
    unsafe fn into_abi_for_ret<'a>(self, store: WeakStore<'a>) -> Self::Abi;

    // Same as `WasmTy::from_abi`.
    #[doc(hidden)]
    unsafe fn from_abi<'a>(abi: Self::Abi, store: WeakStore<'a>) -> Self;

    // Same as `WasmTy::push`.
    #[doc(hidden)]
    fn push(dst: &mut Vec<ValType>);

    // Same as `WasmTy::matches`.
    #[doc(hidden)]
    fn matches(tys: impl Iterator<Item = ValType>) -> anyhow::Result<()>;

    // Same as `WasmTy::load_from_args`.
    #[doc(hidden)]
    unsafe fn load_from_args(ptr: &mut *const u128) -> Self::Abi;

    // Same as `WasmTy::store_to_args`.
    #[doc(hidden)]
    unsafe fn store_to_args(abi: Self::Abi, ptr: *mut u128);
}

unsafe impl WasmTy for () {
    type Abi = Self;

    #[inline]
    fn compatible_with_store<'a>(&self, _store: WeakStore<'a>) -> bool {
        true
    }

    #[inline]
    fn into_abi_for_arg<'a>(self, _store: WeakStore<'a>) -> Self::Abi {}

    #[inline]
    unsafe fn from_abi<'a>(_abi: Self::Abi, _store: WeakStore<'a>) -> Self {}

    fn push(_dst: &mut Vec<ValType>) {}

    fn matches(_tys: impl Iterator<Item = ValType>) -> anyhow::Result<()> {
        Ok(())
    }

    #[inline]
    unsafe fn load_from_args(_ptr: &mut *const u128) -> Self::Abi {}

    #[inline]
    unsafe fn store_to_args(_abi: Self::Abi, _ptr: *mut u128) {}
}

unsafe impl WasmTy for i32 {
    type Abi = Self;

    #[inline]
    fn compatible_with_store<'a>(&self, _store: WeakStore<'a>) -> bool {
        true
    }

    #[inline]
    fn into_abi_for_arg<'a>(self, _store: WeakStore<'a>) -> Self::Abi {
        self
    }

    #[inline]
    unsafe fn from_abi<'a>(abi: Self::Abi, _store: WeakStore<'a>) -> Self {
        abi
    }

    fn push(dst: &mut Vec<ValType>) {
        dst.push(ValType::I32);
    }

    fn matches(mut tys: impl Iterator<Item = ValType>) -> anyhow::Result<()> {
        let next = tys.next();
        ensure!(
            next == Some(ValType::I32),
            "Type mismatch, expected i32, got {:?}",
            next
        );
        Ok(())
    }

    #[inline]
    unsafe fn load_from_args(ptr: &mut *const u128) -> Self::Abi {
        let ret = **ptr as Self;
        *ptr = (*ptr).add(1);
        return ret;
    }

    #[inline]
    unsafe fn store_to_args(abi: Self::Abi, ptr: *mut u128) {
        *ptr = abi as u128;
    }
}

unsafe impl WasmTy for u32 {
    type Abi = <i32 as WasmTy>::Abi;

    #[inline]
    fn compatible_with_store<'a>(&self, _store: WeakStore<'a>) -> bool {
        true
    }

    #[inline]
    fn into_abi_for_arg<'a>(self, _store: WeakStore<'a>) -> Self::Abi {
        self as i32
    }

    #[inline]
    unsafe fn from_abi<'a>(abi: Self::Abi, _store: WeakStore<'a>) -> Self {
        abi as Self
    }

    fn push(dst: &mut Vec<ValType>) {
        <i32 as WasmTy>::push(dst)
    }

    fn matches(tys: impl Iterator<Item = ValType>) -> anyhow::Result<()> {
        <i32 as WasmTy>::matches(tys)
    }

    #[inline]
    unsafe fn load_from_args(ptr: &mut *const u128) -> Self::Abi {
        <i32 as WasmTy>::load_from_args(ptr)
    }

    #[inline]
    unsafe fn store_to_args(abi: Self::Abi, ptr: *mut u128) {
        <i32 as WasmTy>::store_to_args(abi, ptr)
    }
}

unsafe impl WasmTy for i64 {
    type Abi = Self;

    #[inline]
    fn compatible_with_store<'a>(&self, _store: WeakStore<'a>) -> bool {
        true
    }

    #[inline]
    fn into_abi_for_arg<'a>(self, _store: WeakStore<'a>) -> Self::Abi {
        self
    }

    #[inline]
    unsafe fn from_abi<'a>(abi: Self::Abi, _store: WeakStore<'a>) -> Self {
        abi
    }

    fn push(dst: &mut Vec<ValType>) {
        dst.push(ValType::I64);
    }

    fn matches(mut tys: impl Iterator<Item = ValType>) -> anyhow::Result<()> {
        let next = tys.next();
        ensure!(
            next == Some(ValType::I64),
            "Type mismatch, expected i64, got {:?}",
            next
        );
        Ok(())
    }

    #[inline]
    unsafe fn load_from_args(ptr: &mut *const u128) -> Self::Abi {
        let ret = **ptr as Self;
        *ptr = (*ptr).add(1);
        return ret;
    }

    #[inline]
    unsafe fn store_to_args(abi: Self::Abi, ptr: *mut u128) {
        *ptr = abi as u128;
    }
}

unsafe impl WasmTy for u64 {
    type Abi = <i64 as WasmTy>::Abi;

    #[inline]
    fn compatible_with_store<'a>(&self, _store: WeakStore<'a>) -> bool {
        true
    }

    #[inline]
    fn into_abi_for_arg<'a>(self, _store: WeakStore<'a>) -> Self::Abi {
        self as i64
    }

    #[inline]
    unsafe fn from_abi<'a>(abi: Self::Abi, _store: WeakStore<'a>) -> Self {
        abi as Self
    }

    fn push(dst: &mut Vec<ValType>) {
        <i64 as WasmTy>::push(dst)
    }

    fn matches(tys: impl Iterator<Item = ValType>) -> anyhow::Result<()> {
        <i64 as WasmTy>::matches(tys)
    }

    #[inline]
    unsafe fn load_from_args(ptr: &mut *const u128) -> Self::Abi {
        <i64 as WasmTy>::load_from_args(ptr)
    }

    #[inline]
    unsafe fn store_to_args(abi: Self::Abi, ptr: *mut u128) {
        <i64 as WasmTy>::store_to_args(abi, ptr)
    }
}

unsafe impl WasmTy for f32 {
    type Abi = Self;

    #[inline]
    fn compatible_with_store<'a>(&self, _store: WeakStore<'a>) -> bool {
        true
    }

    #[inline]
    fn into_abi_for_arg<'a>(self, _store: WeakStore<'a>) -> Self::Abi {
        self
    }

    #[inline]
    unsafe fn from_abi<'a>(abi: Self::Abi, _store: WeakStore<'a>) -> Self {
        abi
    }

    fn push(dst: &mut Vec<ValType>) {
        dst.push(ValType::F32);
    }

    fn matches(mut tys: impl Iterator<Item = ValType>) -> anyhow::Result<()> {
        let next = tys.next();
        ensure!(
            next == Some(ValType::F32),
            "Type mismatch, expected f32, got {:?}",
            next
        );
        Ok(())
    }

    #[inline]
    unsafe fn load_from_args(ptr: &mut *const u128) -> Self::Abi {
        let ret = f32::from_bits(**ptr as u32);
        *ptr = (*ptr).add(1);
        return ret;
    }

    #[inline]
    unsafe fn store_to_args(abi: Self::Abi, ptr: *mut u128) {
        *ptr = abi.to_bits() as u128;
    }
}

unsafe impl WasmTy for f64 {
    type Abi = Self;

    #[inline]
    fn compatible_with_store<'a>(&self, _store: WeakStore<'a>) -> bool {
        true
    }

    #[inline]
    fn into_abi_for_arg<'a>(self, _store: WeakStore<'a>) -> Self::Abi {
        self
    }

    #[inline]
    unsafe fn from_abi<'a>(abi: Self::Abi, _store: WeakStore<'a>) -> Self {
        abi
    }

    fn push(dst: &mut Vec<ValType>) {
        dst.push(ValType::F64);
    }

    fn matches(mut tys: impl Iterator<Item = ValType>) -> anyhow::Result<()> {
        let next = tys.next();
        ensure!(
            next == Some(ValType::F64),
            "Type mismatch, expected f64, got {:?}",
            next
        );
        Ok(())
    }

    #[inline]
    unsafe fn load_from_args(ptr: &mut *const u128) -> Self::Abi {
        let ret = f64::from_bits(**ptr as u64);
        *ptr = (*ptr).add(1);
        return ret;
    }

    #[inline]
    unsafe fn store_to_args(abi: Self::Abi, ptr: *mut u128) {
        *ptr = abi.to_bits() as u128;
    }
}

unsafe impl WasmTy for Option<ExternRef> {
    type Abi = *mut u8;

    #[inline]
    fn compatible_with_store<'a>(&self, _store: WeakStore<'a>) -> bool {
        true
    }

    #[inline]
    fn into_abi_for_arg<'a>(self, store: WeakStore<'a>) -> Self::Abi {
        if let Some(x) = self {
            let store = Store::upgrade(store.0).unwrap();
            let abi = x.inner.as_raw();
            unsafe {
                store
                    .externref_activations_table()
                    .insert_with_gc(x.inner, store.stack_map_registry());
            }
            abi
        } else {
            ptr::null_mut()
        }
    }

    #[inline]
    unsafe fn from_abi<'a>(abi: Self::Abi, _store: WeakStore<'a>) -> Self {
        if abi.is_null() {
            None
        } else {
            Some(ExternRef {
                inner: wasmtime_runtime::VMExternRef::clone_from_raw(abi),
            })
        }
    }

    fn push(dst: &mut Vec<ValType>) {
        dst.push(ValType::ExternRef);
    }

    fn matches(mut tys: impl Iterator<Item = ValType>) -> anyhow::Result<()> {
        let next = tys.next();
        ensure!(
            next == Some(ValType::ExternRef),
            "Type mismatch, expected externref, got {:?}",
            next
        );
        Ok(())
    }

    unsafe fn load_from_args(ptr: &mut *const u128) -> Self::Abi {
        let ret = **ptr as usize as *mut u8;
        *ptr = (*ptr).add(1);
        ret
    }

    unsafe fn store_to_args(abi: Self::Abi, ptr: *mut u128) {
        ptr::write(ptr, abi as usize as u128);
    }
}

unsafe impl WasmTy for Option<Func> {
    type Abi = *mut wasmtime_runtime::VMCallerCheckedAnyfunc;

    #[inline]
    fn compatible_with_store<'a>(&self, store: WeakStore<'a>) -> bool {
        if let Some(f) = self {
            let store = Store::upgrade(store.0).unwrap();
            Store::same(&store, f.store())
        } else {
            true
        }
    }

    #[inline]
    fn into_abi_for_arg<'a>(self, _store: WeakStore<'a>) -> Self::Abi {
        if let Some(f) = self {
            f.caller_checked_anyfunc().as_ptr()
        } else {
            ptr::null_mut()
        }
    }

    #[inline]
    unsafe fn from_abi<'a>(abi: Self::Abi, store: WeakStore<'a>) -> Self {
        let store = Store::upgrade(store.0).unwrap();
        Func::from_caller_checked_anyfunc(&store, abi)
    }

    fn push(dst: &mut Vec<ValType>) {
        dst.push(ValType::FuncRef);
    }

    fn matches(mut tys: impl Iterator<Item = ValType>) -> anyhow::Result<()> {
        let next = tys.next();
        ensure!(
            next == Some(ValType::FuncRef),
            "Type mismatch, expected funcref, got {:?}",
            next
        );
        Ok(())
    }

    unsafe fn load_from_args(ptr: &mut *const u128) -> Self::Abi {
        let ret = **ptr as usize as *mut wasmtime_runtime::VMCallerCheckedAnyfunc;
        *ptr = (*ptr).add(1);
        ret
    }

    unsafe fn store_to_args(abi: Self::Abi, ptr: *mut u128) {
        ptr::write(ptr, abi as usize as u128);
    }
}

unsafe impl<T> WasmRet for T
where
    T: WasmTy,
{
    type Abi = <T as WasmTy>::Abi;

    #[inline]
    fn compatible_with_store<'a>(&self, store: WeakStore<'a>) -> bool {
        <Self as WasmTy>::compatible_with_store(self, store)
    }

    #[inline]
    unsafe fn into_abi_for_ret<'a>(self, store: WeakStore<'a>) -> Self::Abi {
        <Self as WasmTy>::into_abi_for_arg(self, store)
    }

    #[inline]
    unsafe fn from_abi<'a>(abi: Self::Abi, store: WeakStore<'a>) -> Self {
        <Self as WasmTy>::from_abi(abi, store)
    }

    #[inline]
    fn push(dst: &mut Vec<ValType>) {
        <Self as WasmTy>::push(dst)
    }

    #[inline]
    fn matches(tys: impl Iterator<Item = ValType>) -> anyhow::Result<()> {
        <Self as WasmTy>::matches(tys)
    }

    #[inline]
    unsafe fn load_from_args(ptr: &mut *const u128) -> Self::Abi {
        <Self as WasmTy>::load_from_args(ptr)
    }

    #[inline]
    unsafe fn store_to_args(abi: Self::Abi, ptr: *mut u128) {
        <Self as WasmTy>::store_to_args(abi, ptr)
    }
}

unsafe impl<T> WasmRet for Result<T, Trap>
where
    T: WasmTy,
{
    type Abi = <T as WasmTy>::Abi;

    #[inline]
    fn compatible_with_store<'a>(&self, store: WeakStore<'a>) -> bool {
        match self {
            Ok(x) => <T as WasmTy>::compatible_with_store(x, store),
            Err(_) => true,
        }
    }

    #[inline]
    unsafe fn into_abi_for_ret<'a>(self, store: WeakStore<'a>) -> Self::Abi {
        match self {
            Ok(val) => return <T as WasmTy>::into_abi_for_arg(val, store),
            Err(trap) => handle_trap(trap),
        }

        unsafe fn handle_trap(trap: Trap) -> ! {
            raise_user_trap(trap.into())
        }
    }

    #[inline]
    unsafe fn from_abi<'a>(abi: Self::Abi, store: WeakStore<'a>) -> Self {
        Ok(<T as WasmTy>::from_abi(abi, store))
    }

    fn push(dst: &mut Vec<ValType>) {
        <T as WasmTy>::push(dst)
    }

    fn matches(tys: impl Iterator<Item = ValType>) -> anyhow::Result<()> {
        <T as WasmTy>::matches(tys)
    }

    #[inline]
    unsafe fn load_from_args(ptr: &mut *const u128) -> Self::Abi {
        <T as WasmTy>::load_from_args(ptr)
    }

    #[inline]
    unsafe fn store_to_args(abi: Self::Abi, ptr: *mut u128) {
        <T as WasmTy>::store_to_args(abi, ptr);
    }
}

/// Internal trait implemented for all arguments that can be passed to
/// [`Func::wrap`].
///
/// This trait should not be implemented by external users, it's only intended
/// as an implementation detail of this crate.
pub trait IntoFunc<Params, Results> {
    #[doc(hidden)]
    fn into_func(self, store: &Store) -> Func;
}

/// A structure representing the *caller's* context when creating a function
/// via [`Func::wrap`].
///
/// This structure can be taken as the first parameter of a closure passed to
/// [Func::wrap], and it can be used to learn information about the caller of
/// the function, such as the calling module's memory, exports, etc.
///
/// The primary purpose of this structure is to provide access to the
/// caller's information, namely it's exported memory and exported functions. This
/// allows functions which take pointers as arguments to easily read the memory the
/// pointers point into, or if a function is expected to call malloc in the wasm
/// module to reserve space for the output you can do that.
///
/// Note that this Caller type a pretty temporary mechanism for accessing the
/// caller's information until interface types has been fully standardized and
/// implemented. The interface types proposal will obsolete this type and this will
/// be removed in the future at some point after interface types is implemented. If
/// you're relying on this Caller type it's recommended to become familiar with
/// interface types to ensure that your use case is covered by the proposal.
pub struct Caller<'a> {
    // Note that this is a `Weak` pointer instead of a `&'a Store`,
    // intentionally so. This allows us to break an `Rc` cycle which would
    // otherwise look like this:
    //
    // * A `Store` object ...
    // * ... owns all `InstanceHandle` objects ...
    // * ... which are created in `Func::wrap` with custom host data ...
    // * ... where the custom host data needs to point to `Store` to be stored
    //   here
    //
    // This `Rc` cycle means that we would never actually reclaim any memory or
    // deallocate any instances. To break this cycle we use a weak pointer here
    // which points back to `Store`. A `Caller` should only ever be usable
    // when the original `Store` is alive, however, so this should always be an
    // upgrade-able pointer. Alternative solutions or other ideas to break this
    // cycle would be most welcome!
    store: &'a Weak<StoreInner>,
    caller_vmctx: *mut VMContext,
}

impl Caller<'_> {
    /// Looks up an export from the caller's module by the `name` given.
    ///
    /// Note that this function is only implemented for the `Extern::Memory`
    /// and the `Extern::Func` types currently. No other exported structures
    /// can be acquired through this just yet, but this may be implemented
    /// in the future!
    ///
    /// Note that when accessing and calling exported functions, one should adhere
    /// to the guidlines of the interface types proposal.
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
    pub fn get_export(&self, name: &str) -> Option<Extern> {
        unsafe {
            if self.caller_vmctx.is_null() {
                return None;
            }
            let instance = InstanceHandle::from_vmctx(self.caller_vmctx);
            let export = instance.lookup(name)?;
            // Our `Weak` pointer is used only to break a cycle where `Store`
            // stores instance handles which have this weak pointer as their
            // custom host data. This function should only be invoke-able while
            // the `Store` is active, so this upgrade should always succeed.
            debug_assert!(self.store.upgrade().is_some());
            let handle =
                Store::from_inner(self.store.upgrade()?).existing_instance_handle(instance);
            match export {
                Export::Memory(m) => Some(Extern::Memory(Memory::from_wasmtime_memory(m, handle))),
                Export::Function(f) => Some(Extern::Func(Func::from_wasmtime_function(f, handle))),
                _ => None,
            }
        }
    }

    /// Get a handle to this caller's store.
    pub fn store(&self) -> Store {
        // See comment above the `store` member for why this unwrap is OK.
        Store::upgrade(&self.store).unwrap()
    }
}

#[inline(never)]
#[cold]
unsafe fn raise_cross_store_trap() -> ! {
    #[derive(Debug)]
    struct CrossStoreError;

    impl std::error::Error for CrossStoreError {}

    impl fmt::Display for CrossStoreError {
        fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
            write!(
                f,
                "host function attempted to return cross-`Store` \
                 value to Wasm",
            )
        }
    }

    raise_user_trap(Box::new(CrossStoreError));
}

macro_rules! impl_into_func {
    ($(
        ($($args:ident)*)
    )*) => ($(
        // Implement for functions without a leading `&Caller` parameter,
        // delegating to the implementation below which does have the leading
        // `Caller` parameter.
        impl<F, $($args,)* R> IntoFunc<($($args,)*), R> for F
        where
            F: Fn($($args),*) -> R + 'static,
            $($args: WasmTy,)*
            R: WasmRet,
        {
            #[allow(non_snake_case)]
            fn into_func(self, store: &Store) -> Func {
                Func::wrap(store, move |_: Caller<'_>, $($args:$args),*| {
                    self($($args),*)
                })
            }
        }

        #[allow(non_snake_case)]
        impl<F, $($args,)* R> IntoFunc<(Caller<'_>, $($args,)*), R> for F
        where
            F: Fn(Caller<'_>, $($args),*) -> R + 'static,
            $($args: WasmTy,)*
            R: WasmRet,
        {
            fn into_func(self, store: &Store) -> Func {
                /// This shim is called by Wasm code, constructs a `Caller`,
                /// calls the wrapped host function, and returns the translated
                /// result back to Wasm.
                ///
                /// Note that this shim's ABI must *exactly* match that expected
                /// by Cranelift, since Cranelift is generating raw function
                /// calls directly to this function.
                unsafe extern "C" fn wasm_to_host_shim<F, $($args,)* R>(
                    vmctx: *mut VMContext,
                    caller_vmctx: *mut VMContext,
                    $( $args: $args::Abi, )*
                ) -> R::Abi
                where
                    F: Fn(Caller<'_>, $( $args ),*) -> R + 'static,
                    $( $args: WasmTy, )*
                    R: WasmRet,
                {
                    let state = (*vmctx).host_state();
                    // Double-check ourselves in debug mode, but we control
                    // the `Any` here so an unsafe downcast should also
                    // work.
                    debug_assert!(state.is::<(F, Weak<StoreInner>)>());
                    let (func, store) = &*(state as *const _ as *const (F, Weak<StoreInner>));
                    let weak_store = WeakStore(store);

                    let ret = {
                        panic::catch_unwind(AssertUnwindSafe(|| {
                            func(
                                Caller { store, caller_vmctx },
                                $( $args::from_abi($args, weak_store), )*
                            )
                        }))
                    };
                    match ret {
                        Err(panic) => wasmtime_runtime::resume_panic(panic),
                        Ok(ret) => {
                            // Because the wrapped function is not `unsafe`, we
                            // can't assume it returned a value that is
                            // compatible with this store.
                            if !ret.compatible_with_store(weak_store) {
                                raise_cross_store_trap();
                            }

                            ret.into_abi_for_ret(weak_store)
                        }
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
                        ) -> R::Abi,
                    >(ptr);

                    let mut _next = args as *const u128;
                    $( let $args = $args::load_from_args(&mut _next); )*
                    let ret = ptr(callee_vmctx, caller_vmctx, $( $args ),*);
                    R::store_to_args(ret, args);
                }

                let mut _args = Vec::new();
                $($args::push(&mut _args);)*
                let mut ret = Vec::new();
                R::push(&mut ret);
                let ty = FuncType::new(_args.into(), ret.into());

                let store_weak = store.weak();
                let trampoline = host_trampoline::<$($args,)* R>;
                let (instance, export) = unsafe {
                    crate::trampoline::generate_raw_func_export(
                        &ty,
                        std::slice::from_raw_parts_mut(
                            wasm_to_host_shim::<F, $($args,)* R> as *mut _,
                            0,
                        ),
                        trampoline,
                        store,
                        Box::new((self, store_weak)),
                    )
                    .expect("failed to generate export")
                };

                Func {
                    instance,
                    export,
                    trampoline,
                }
            }
        }
    )*)
}

impl_into_func! {
    ()
    (A1)
    (A1 A2)
    (A1 A2 A3)
    (A1 A2 A3 A4)
    (A1 A2 A3 A4 A5)
    (A1 A2 A3 A4 A5 A6)
    (A1 A2 A3 A4 A5 A6 A7)
    (A1 A2 A3 A4 A5 A6 A7 A8)
    (A1 A2 A3 A4 A5 A6 A7 A8 A9)
    (A1 A2 A3 A4 A5 A6 A7 A8 A9 A10)
    (A1 A2 A3 A4 A5 A6 A7 A8 A9 A10 A11)
    (A1 A2 A3 A4 A5 A6 A7 A8 A9 A10 A11 A12)
    (A1 A2 A3 A4 A5 A6 A7 A8 A9 A10 A11 A12 A13)
    (A1 A2 A3 A4 A5 A6 A7 A8 A9 A10 A11 A12 A13 A14)
    (A1 A2 A3 A4 A5 A6 A7 A8 A9 A10 A11 A12 A13 A14 A15)
    (A1 A2 A3 A4 A5 A6 A7 A8 A9 A10 A11 A12 A13 A14 A15 A16)
}

#[test]
fn wasm_ty_roundtrip() -> Result<(), anyhow::Error> {
    use crate::*;
    let store = Store::default();
    let debug = Func::wrap(&store, |a: i32, b: u32, c: f32, d: i64, e: u64, f: f64| {
        assert_eq!(a, -1);
        assert_eq!(b, 1);
        assert_eq!(c, 2.0);
        assert_eq!(d, -3);
        assert_eq!(e, 3);
        assert_eq!(f, 4.0);
    });
    let module = Module::new(
        store.engine(),
        r#"
             (module
                 (import "" "" (func $debug (param i32 i32 f32 i64 i64 f64)))
                 (func (export "foo") (param i32 i32 f32 i64 i64 f64)
                    (if (i32.ne (local.get 0) (i32.const -1))
                        (then unreachable)
                    )
                    (if (i32.ne (local.get 1) (i32.const 1))
                        (then unreachable)
                    )
                    (if (f32.ne (local.get 2) (f32.const 2))
                        (then unreachable)
                    )
                    (if (i64.ne (local.get 3) (i64.const -3))
                        (then unreachable)
                    )
                    (if (i64.ne (local.get 4) (i64.const 3))
                        (then unreachable)
                    )
                    (if (f64.ne (local.get 5) (f64.const 4))
                        (then unreachable)
                    )
                    local.get 0
                    local.get 1
                    local.get 2
                    local.get 3
                    local.get 4
                    local.get 5
                    call $debug
                )
            )
         "#,
    )?;
    let instance = Instance::new(&store, &module, &[debug.into()])?;
    let foo = instance
        .get_func("foo")
        .unwrap()
        .get6::<i32, u32, f32, i64, u64, f64, ()>()?;
    foo(-1, 1, 2.0, -3, 3, 4.0)?;
    Ok(())
}
