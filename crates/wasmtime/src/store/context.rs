use crate::store::{Store, StoreInner, StoreInnermost};
use std::ops::{Deref, DerefMut};

/// A temporary handle to a [`&Store<T>`][`Store`].
///
/// This type is sutable for [`AsContext`] trait bounds on methods if desired.
/// For more information, see [`Store`].
// NB the repr(transparent) here is for the C API and it's important that the
// representation of this `struct` is a pointer for now. If the representation
// changes then the C API will need to be updated
#[repr(transparent)]
pub struct StoreContext<'a, T>(pub(crate) &'a StoreInner<T>);

/// A temporary handle to a [`&mut Store<T>`][`Store`].
///
/// This type is sutable for [`AsContextMut`] or [`AsContext`] trait bounds on
/// methods if desired.  For more information, see [`Store`].
// NB the repr(transparent) here is for the same reason as above.
#[repr(transparent)]
pub struct StoreContextMut<'a, T>(pub(crate) &'a mut StoreInner<T>);

impl<'a, T> StoreContextMut<'a, T> {
    /// One of the unsafe lynchpins of Wasmtime.
    ///
    /// This method is called from one location, `Caller::with`, and is where we
    /// load the raw unsafe trait object pointer from a `*mut VMContext` and
    /// then cast it back to a `StoreContextMut`. This is naturally unsafe due
    /// to the raw pointer usage, but it's also unsafe because `T` here needs to
    /// line up with the `T` used to define the trait object itself.
    ///
    /// This should generally be achieved with various trait bounds throughout
    /// Wasmtime that might give access to the `Caller<'_, T>` type.
    /// Unfortunately there's not a ton of debug asserts we can add here, so we
    /// rely on testing to largely help show that this is correctly used.
    pub(crate) unsafe fn from_raw(
        store: *mut dyn wasmtime_runtime::Store,
    ) -> StoreContextMut<'a, T> {
        StoreContextMut(&mut *(store as *mut StoreInner<T>))
    }

    /// A helper method to erase the `T` on `Self` so the returned type has no
    /// generics. For some more information see [`StoreOpaque`] itself.
    ///
    /// The primary purpose of this is to help improve compile times where
    /// non-generic code can be compiled into libwasmtime.rlib.
    pub(crate) fn opaque(mut self) -> StoreOpaque<'a> {
        StoreOpaque {
            traitobj: self.traitobj(),
            inner: self.0,
        }
    }

    fn traitobj(&mut self) -> *mut dyn wasmtime_runtime::Store {
        unsafe {
            std::mem::transmute::<
                *mut (dyn wasmtime_runtime::Store + '_),
                *mut (dyn wasmtime_runtime::Store + 'static),
            >(self.0)
        }
    }
}

/// A trait used to get shared access to a [`Store`] in Wasmtime.
///
/// This trait is used as a bound on the first argument of many methods within
/// Wasmtime. This trait is implemented for types like [`Store`],
/// [`Caller`](crate::Caller), and [`StoreContext`] itself. Implementors of this
/// trait provide access to a [`StoreContext`] via some means, allowing the
/// method in question to get access to the store's internal information.
///
/// Note that this is only used in contexts where the store's information is
/// read, but not written. For example methods that return type information will
/// use this trait as a bound. More commonly, though, mutation is required and
/// [`AsContextMut`] is needed.
pub trait AsContext {
    /// The host information associated with the [`Store`], aka the `T` in
    /// [`Store<T>`].
    type Data;

    /// Returns the store context that this type provides access to.
    fn as_context(&self) -> StoreContext<'_, Self::Data>;
}

/// A trait used to get exclusive mutable access to a [`Store`] in Wasmtime.
///
/// This trait is used as a bound on the first argument of many methods within
/// Wasmtime. This trait is implemented for types like [`Store`],
/// [`Caller`](crate::Caller), and [`StoreContextMut`] itself. Implementors of
/// this trait provide access to a [`StoreContextMut`] via some means, allowing
/// the method in question to get access to the store's internal information.
///
/// This is notably used for methods that may require some mutation of the
/// [`Store`] itself. For example calling a wasm function can mutate linear
/// memory or globals. Creation of a [`Func`](crate::Func) will update internal
/// data structures. This ends up being quite a common bound in Wasmtime, but
/// typically you can simply pass `&mut store` or `&mut caller` to satisfy it.
pub trait AsContextMut: AsContext {
    /// Returns the store context that this type provides access to.
    fn as_context_mut(&mut self) -> StoreContextMut<'_, Self::Data>;
}

impl<T> AsContext for Store<T> {
    type Data = T;

    #[inline]
    fn as_context(&self) -> StoreContext<'_, T> {
        StoreContext(&self.inner)
    }
}

impl<T> AsContextMut for Store<T> {
    #[inline]
    fn as_context_mut(&mut self) -> StoreContextMut<'_, T> {
        StoreContextMut(&mut self.inner)
    }
}

impl<T> AsContext for StoreContext<'_, T> {
    type Data = T;

    #[inline]
    fn as_context(&self) -> StoreContext<'_, T> {
        StoreContext(&*self.0)
    }
}

impl<T> AsContext for StoreContextMut<'_, T> {
    type Data = T;

    #[inline]
    fn as_context(&self) -> StoreContext<'_, T> {
        StoreContext(&*self.0)
    }
}

impl<T> AsContextMut for StoreContextMut<'_, T> {
    #[inline]
    fn as_context_mut(&mut self) -> StoreContextMut<'_, T> {
        StoreContextMut(&mut *self.0)
    }
}

// forward AsContext for &T
impl<T: AsContext> AsContext for &'_ T {
    type Data = T::Data;

    #[inline]
    fn as_context(&self) -> StoreContext<'_, T::Data> {
        T::as_context(*self)
    }
}

// forward AsContext for &mut T
impl<T: AsContext> AsContext for &'_ mut T {
    type Data = T::Data;

    #[inline]
    fn as_context(&self) -> StoreContext<'_, T::Data> {
        T::as_context(*self)
    }
}

// forward AsContextMut for &mut T
impl<T: AsContextMut> AsContextMut for &'_ mut T {
    #[inline]
    fn as_context_mut(&mut self) -> StoreContextMut<'_, T::Data> {
        T::as_context_mut(*self)
    }
}

//
impl<'a, T: AsContext> From<&'a T> for StoreContext<'a, T::Data> {
    fn from(t: &'a T) -> StoreContext<'a, T::Data> {
        t.as_context()
    }
}

impl<'a, T: AsContext> From<&'a mut T> for StoreContext<'a, T::Data> {
    fn from(t: &'a mut T) -> StoreContext<'a, T::Data> {
        T::as_context(t)
    }
}

impl<'a, T: AsContextMut> From<&'a mut T> for StoreContextMut<'a, T::Data> {
    fn from(t: &'a mut T) -> StoreContextMut<'a, T::Data> {
        t.as_context_mut()
    }
}

/// This structure is akin to a `StoreContextMut` except that the `T` is
/// "erased" to an opaque type.
///
/// This structure is used pervasively through wasmtime whenever the `T` isn't
/// needed (quite common!). This allows the compiler to erase generics and
/// compile more code in the wasmtime crate itself instead of monomorphizing
/// everything into consumer crates. The primary purpose of this is to help
/// compile times.
#[doc(hidden)] // this is part of `WasmTy`, but a hidden part, so hide this
pub struct StoreOpaque<'a> {
    /// The actual pointer to the `StoreInner` internals.
    inner: &'a mut StoreInnermost,

    /// A raw trait object that can be used to invoke functions with. Note that
    /// this is a pointer which aliases with `inner` above, so extreme care
    /// needs to be used when using this (the above `inner` cannot be actively
    /// borrowed).
    pub traitobj: *mut dyn wasmtime_runtime::Store,
}

impl<'a> StoreOpaque<'a> {
    pub fn into_inner(self) -> &'a StoreInnermost {
        self.inner
    }
}

// Deref impls to forward all methods on `StoreOpaque` to `StoreInner`.
impl<'a> Deref for StoreOpaque<'a> {
    type Target = StoreInnermost;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &*self.inner
    }
}

impl<'a> DerefMut for StoreOpaque<'a> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut *self.inner
    }
}
