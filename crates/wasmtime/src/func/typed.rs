use super::{invoke_wasm_and_catch_traps, HostAbi};
use crate::store::StoreOpaque;
use crate::{AsContextMut, ExternRef, Func, Trap, ValType};
use anyhow::{bail, Result};
use std::marker;
use std::mem::{self, MaybeUninit};
use std::ptr;
use wasmtime_runtime::{VMContext, VMFunctionBody};

/// A statically typed WebAssembly function.
///
/// Values of this type represent statically type-checked WebAssembly functions.
/// The function within a [`TypedFunc`] is statically known to have `Params` as its
/// parameters and `Results` as its results.
///
/// This structure is created via [`Func::typed`] or [`TypedFunc::new_unchecked`].
/// For more documentation about this see those methods.
#[repr(transparent)] // here for the C API
pub struct TypedFunc<Params, Results> {
    _a: marker::PhantomData<fn(Params) -> Results>,
    func: Func,
}

impl<Params, Results> Copy for TypedFunc<Params, Results> {}

impl<Params, Results> Clone for TypedFunc<Params, Results> {
    fn clone(&self) -> TypedFunc<Params, Results> {
        *self
    }
}

impl<Params, Results> TypedFunc<Params, Results>
where
    Params: WasmParams,
    Results: WasmResults,
{
    /// An unchecked version of [`Func::typed`] which does not perform a
    /// typecheck and simply assumes that the type declared here matches the
    /// type of this function.
    ///
    /// The semantics of this function are the same as [`Func::typed`] except
    /// that no error is returned because no typechecking is done.
    ///
    /// # Unsafety
    ///
    /// This function only safe to call if `typed` would otherwise return `Ok`
    /// for the same `Params` and `Results` specified. If `typed` would return
    /// an error then the returned `TypedFunc` is memory unsafe to invoke.
    pub unsafe fn new_unchecked(func: Func) -> TypedFunc<Params, Results> {
        TypedFunc {
            _a: marker::PhantomData,
            func,
        }
    }

    /// Returns the underlying [`Func`] that this is wrapping, losing the static
    /// type information in the process.
    pub fn func(&self) -> &Func {
        &self.func
    }

    /// Invokes this WebAssembly function with the specified parameters.
    ///
    /// Returns either the results of the call, or a [`Trap`] if one happened.
    ///
    /// For more information, see the [`Func::typed`] and [`Func::call`]
    /// documentation.
    ///
    /// # Panics
    ///
    /// This function will panic if it is called when the underlying [`Func`] is
    /// connected to an asynchronous store.
    pub fn call(&self, mut store: impl AsContextMut, params: Params) -> Result<Results, Trap> {
        store.as_context_mut().0.exiting_native_hook()?;
        let mut store_opaque = store.as_context_mut().opaque();
        assert!(
            !store_opaque.async_support(),
            "must use `call_async` with async stores"
        );
        let r = unsafe { self._call(&mut store_opaque, params) };
        store.as_context_mut().0.entering_native_hook()?;
        r
    }

    /// Invokes this WebAssembly function with the specified parameters.
    ///
    /// Returns either the results of the call, or a [`Trap`] if one happened.
    ///
    /// For more information, see the [`Func::typed`] and [`Func::call_async`]
    /// documentation.
    ///
    /// # Panics
    ///
    /// This function will panic if it is called when the underlying [`Func`] is
    /// connected to a synchronous store.
    #[cfg(feature = "async")]
    #[cfg_attr(nightlydoc, doc(cfg(feature = "async")))]
    pub fn call_async<'a, T>(
        &'a self,
        store: impl AsContextMut<Data = T> + 'a,
        params: Params,
    ) -> impl crate::WasmtimeFuture<Data = T, Output = Result<Results, Trap>> + 'a
    where
        T: Send,
    {
        assert!(
            store.as_context().async_support(),
            "must use `call` with non-async stores"
        );
        crate::store::on_fiber(store, move |store| unsafe { self._call(store, params) })
    }

    unsafe fn _call(&self, store: &mut StoreOpaque<'_>, params: Params) -> Result<Results, Trap> {
        // Validate that all runtime values flowing into this store indeed
        // belong within this store, otherwise it would be unsafe for store
        // values to cross each other.
        let params = match params.into_abi(store) {
            Some(abi) => abi,
            None => {
                return Err(Trap::new(
                    "attempt to pass cross-`Store` value to Wasm as function argument",
                ))
            }
        };

        // Try to capture only a single variable (a tuple) in the closure below.
        // This means the size of the closure is one pointer and is much more
        // efficient to move in memory. This closure is actually invoked on the
        // other side of a C++ shim, so it can never be inlined enough to make
        // the memory go away, so the size matters here for performance.
        let mut captures = (
            self.func.caller_checked_anyfunc(store),
            MaybeUninit::uninit(),
            params,
            false,
        );

        let result = invoke_wasm_and_catch_traps(store, |callee| {
            let (anyfunc, ret, params, returned) = &mut captures;
            let anyfunc = anyfunc.as_ref();
            let result = Params::invoke::<Results>(
                anyfunc.func_ptr.as_ptr(),
                anyfunc.vmctx,
                callee,
                *params,
            );
            ptr::write(ret.as_mut_ptr(), result);
            *returned = true
        });
        let (_, ret, _, returned) = captures;
        debug_assert_eq!(result.is_ok(), returned);
        result?;
        Ok(Results::from_abi(store, ret.assume_init()))
    }
}

/// A trait implemented for types which can be arguments and results for
/// closures passed to [`Func::wrap`] as well as parameters to [`Func::typed`].
///
/// This trait should not be implemented by user types. This trait may change at
/// any time internally. The types which implement this trait, however, are
/// stable over time.
///
/// For more information see [`Func::wrap`] and [`Func::typed`]
pub unsafe trait WasmTy: Send {
    #[doc(hidden)]
    type Abi: Copy;
    #[doc(hidden)]
    #[inline]
    fn typecheck(ty: crate::ValType) -> Result<()> {
        if ty == Self::valtype() {
            Ok(())
        } else {
            bail!("expected {} found {}", Self::valtype(), ty)
        }
    }
    #[doc(hidden)]
    fn valtype() -> ValType;
    #[doc(hidden)]
    fn compatible_with_store(&self, store: &StoreOpaque) -> bool;
    #[doc(hidden)]
    fn into_abi(self, store: &mut StoreOpaque) -> Self::Abi;
    #[doc(hidden)]
    unsafe fn from_abi(abi: Self::Abi, store: &mut StoreOpaque) -> Self;
}

macro_rules! primitives {
    ($($primitive:ident => $ty:ident)*) => ($(
        unsafe impl WasmTy for $primitive {
            type Abi = $primitive;
            #[inline]
            fn valtype() -> ValType {
                ValType::$ty
            }
            #[inline]
            fn compatible_with_store(&self, _: &StoreOpaque) -> bool {
                true
            }
            #[inline]
            fn into_abi(self, _store: &mut StoreOpaque) -> Self::Abi {
                self
            }
            #[inline]
            unsafe fn from_abi(abi: Self::Abi, _store: &mut StoreOpaque) -> Self {
                abi
            }
        }
    )*)
}

primitives! {
    i32 => I32
    u32 => I32
    i64 => I64
    u64 => I64
    f32 => F32
    f64 => F64
}

unsafe impl WasmTy for Option<ExternRef> {
    type Abi = *mut u8;

    #[inline]
    fn valtype() -> ValType {
        ValType::ExternRef
    }

    #[inline]
    fn compatible_with_store(&self, _store: &StoreOpaque) -> bool {
        true
    }

    #[inline]
    fn into_abi(self, store: &mut StoreOpaque) -> Self::Abi {
        if let Some(x) = self {
            let abi = x.inner.as_raw();
            unsafe {
                store.insert_vmexternref(x.inner);
            }
            abi
        } else {
            ptr::null_mut()
        }
    }

    #[inline]
    unsafe fn from_abi(abi: Self::Abi, _store: &mut StoreOpaque) -> Self {
        if abi.is_null() {
            None
        } else {
            Some(ExternRef {
                inner: wasmtime_runtime::VMExternRef::clone_from_raw(abi),
            })
        }
    }
}

unsafe impl WasmTy for Option<Func> {
    type Abi = *mut wasmtime_runtime::VMCallerCheckedAnyfunc;

    #[inline]
    fn valtype() -> ValType {
        ValType::FuncRef
    }

    #[inline]
    fn compatible_with_store<'a>(&self, store: &StoreOpaque) -> bool {
        if let Some(f) = self {
            store.store_data().contains(f.0)
        } else {
            true
        }
    }

    #[inline]
    fn into_abi(self, store: &mut StoreOpaque) -> Self::Abi {
        if let Some(f) = self {
            f.caller_checked_anyfunc(store).as_ptr()
        } else {
            ptr::null_mut()
        }
    }

    #[inline]
    unsafe fn from_abi(abi: Self::Abi, store: &mut StoreOpaque) -> Self {
        Func::from_caller_checked_anyfunc(store, abi)
    }
}

/// A trait used for [`Func::typed`] and with [`TypedFunc`] to represent the set of
/// parameters for wasm functions.
///
/// This is implemented for bare types that can be passed to wasm as well as
/// tuples of those types.
pub unsafe trait WasmParams: Send {
    #[doc(hidden)]
    type Abi: Copy;
    #[doc(hidden)]
    fn typecheck(params: impl ExactSizeIterator<Item = crate::ValType>) -> Result<()>;
    #[doc(hidden)]
    fn into_abi(self, store: &mut StoreOpaque) -> Option<Self::Abi>;
    #[doc(hidden)]
    unsafe fn invoke<R: WasmResults>(
        func: *const VMFunctionBody,
        vmctx1: *mut VMContext,
        vmctx2: *mut VMContext,
        abi: Self::Abi,
    ) -> R::ResultAbi;
}

// Forward an impl from `T` to `(T,)` for convenience if there's only one
// parameter.
unsafe impl<T> WasmParams for T
where
    T: WasmTy,
{
    type Abi = <(T,) as WasmParams>::Abi;

    fn typecheck(params: impl ExactSizeIterator<Item = crate::ValType>) -> Result<()> {
        <(T,) as WasmParams>::typecheck(params)
    }
    #[inline]
    fn into_abi(self, store: &mut StoreOpaque) -> Option<Self::Abi> {
        <(T,) as WasmParams>::into_abi((self,), store)
    }
    unsafe fn invoke<R: WasmResults>(
        func: *const VMFunctionBody,
        vmctx1: *mut VMContext,
        vmctx2: *mut VMContext,
        abi: Self::Abi,
    ) -> R::ResultAbi {
        <(T,) as WasmParams>::invoke::<R>(func, vmctx1, vmctx2, abi)
    }
}

macro_rules! impl_wasm_params {
    ($n:tt $($t:ident)*) => {
        #[allow(non_snake_case)]
        unsafe impl<$($t: WasmTy,)*> WasmParams for ($($t,)*) {
            type Abi = ($($t::Abi,)*);

            fn typecheck(mut params: impl ExactSizeIterator<Item = crate::ValType>) -> Result<()> {
                let mut _n = 0;
                $(
                    match params.next() {
                        Some(t) => $t::typecheck(t)?,
                        None => bail!("expected {} types, found {}", $n, _n),
                    }
                    _n += 1;
                )*

                match params.next() {
                    None => Ok(()),
                    Some(_) => bail!("expected {} types, found {}", $n, params.len() + _n),
                }
            }

            fn into_abi(self, _store: &mut StoreOpaque) -> Option<Self::Abi> {
                let ($($t,)*) = self;
                $(
                    let $t = if $t.compatible_with_store(_store) {
                        $t.into_abi(_store)
                    } else {
                        return None;
                    };
                )*
                Some(($($t,)*))
            }

            unsafe fn invoke<R: WasmResults>(
                func: *const VMFunctionBody,
                vmctx1: *mut VMContext,
                vmctx2: *mut VMContext,
                abi: Self::Abi,
            ) -> R::ResultAbi {
                let fnptr = mem::transmute::<
                    *const VMFunctionBody,
                    unsafe extern "C" fn(
                        *mut VMContext,
                        *mut VMContext,
                        $($t::Abi,)*
                        <R::ResultAbi as HostAbi>::Retptr,
                    ) -> <R::ResultAbi as HostAbi>::Abi,
                >(func);
                let ($($t,)*) = abi;
                // Use the `call` function to acquire a `retptr` which we'll
                // forward to the native function. Once we have it we also
                // convert all our arguments to abi arguments to go to the raw
                // function.
                //
                // Upon returning `R::call` will convert all the returns back
                // into `R`.
                <R::ResultAbi as HostAbi>::call(|retptr| {
                    fnptr(vmctx1, vmctx2, $($t,)* retptr)
                })
            }
        }
    };
}

for_each_function_signature!(impl_wasm_params);

/// A trait used for [`Func::typed`] and with [`TypedFunc`] to represent the set of
/// results for wasm functions.
///
/// This is currently only implemented for `()` and for bare types that can be
/// returned. This is not yet implemented for tuples because a multi-value
/// `TypedFunc` is not currently supported.
pub unsafe trait WasmResults: WasmParams {
    #[doc(hidden)]
    type ResultAbi: HostAbi;
    #[doc(hidden)]
    unsafe fn from_abi(store: &mut StoreOpaque, abi: Self::ResultAbi) -> Self;
}

// Forwards from a bare type `T` to the 1-tuple type `(T,)`
unsafe impl<T: WasmTy> WasmResults for T
where
    (T::Abi,): HostAbi,
{
    type ResultAbi = <(T,) as WasmResults>::ResultAbi;

    unsafe fn from_abi(store: &mut StoreOpaque, abi: Self::ResultAbi) -> Self {
        <(T,) as WasmResults>::from_abi(store, abi).0
    }
}

macro_rules! impl_wasm_results {
    ($n:tt $($t:ident)*) => {
        #[allow(non_snake_case, unused_variables)]
        unsafe impl<$($t: WasmTy,)*> WasmResults for ($($t,)*)
            where ($($t::Abi,)*): HostAbi
        {
            type ResultAbi = ($($t::Abi,)*);

            unsafe fn from_abi(store: &mut StoreOpaque, abi: Self::ResultAbi) -> Self {
                let ($($t,)*) = abi;
                ($($t::from_abi($t, store),)*)
            }
        }
    };
}

for_each_function_signature!(impl_wasm_results);
