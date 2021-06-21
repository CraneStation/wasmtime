//! Wasmtime's embedding API
//!
//! This crate contains an API used to interact with WebAssembly modules. For
//! example you can compile modules, instantiate them, call them, etc. As an
//! embedder of WebAssembly you can also provide WebAssembly modules
//! functionality from the host by creating host-defined functions, memories,
//! globals, etc, which can do things that WebAssembly cannot (such as print to
//! the screen).
//!
//! The `wasmtime` crate has similar concepts to the
//! the [JS WebAssembly
//! API](https://developer.mozilla.org/en-US/docs/WebAssembly) as well as the
//! [proposed C API](https://github.com/webassembly/wasm-c-api), but the Rust
//! API is designed for efficiency, ergonomics, and expressivity in Rust. As
//! with all other Rust code you're guaranteed that programs will be safe (not
//! have undefined behavior or segfault) so long as you don't use `unsafe` in
//! your own program. With `wasmtime` you can easily and conveniently embed a
//! WebAssembly runtime with confidence that the WebAssembly is safely
//! sandboxed.
//!
//! An example of using Wasmtime looks like:
//!
//! ```
//! use anyhow::Result;
//! use wasmtime::*;
//!
//! fn main() -> Result<()> {
//!     // Modules can be compiled through either the text or binary format
//!     let engine = Engine::default();
//!     let wat = r#"
//!         (module
//!             (import "host" "hello" (func $host_hello (param i32)))
//!
//!             (func (export "hello")
//!                 i32.const 3
//!                 call $host_hello)
//!         )
//!     "#;
//!     let module = Module::new(&engine, wat)?;
//!
//!     // All wasm objects operate within the context of a "store". Each
//!     // `Store` has a type parameter to store host-specific data, which in
//!     // this case we're using `4` for.
//!     let mut store = Store::new(&engine, 4);
//!     let host_hello = Func::wrap(&mut store, |caller: Caller<'_, u32>, param: i32| {
//!         println!("Got {} from WebAssembly", param);
//!         println!("my host state is: {}", caller.data());
//!     });
//!
//!     // Instantiation of a module requires specifying its imports and then
//!     // afterwards we can fetch exports by name, as well as asserting the
//!     // type signature of the function with `get_typed_func`.
//!     let instance = Instance::new(&mut store, &module, &[host_hello.into()])?;
//!     let hello = instance.get_typed_func::<(), (), _>(&mut store, "hello")?;
//!
//!     // And finally we can call the wasm!
//!     hello.call(&mut store, ())?;
//!
//!     Ok(())
//! }
//! ```
//!
//! ## Core Concepts
//!
//! There are a number of core types and concepts that are important to be aware
//! of when using the `wasmtime` crate:
//!
//! * [`Engine`] - a global compilation environment for WebAssembly. An
//!   [`Engine`] is an object that can be shared concurrently across threads and
//!   is created with a [`Config`] to tweak various settings. Compilation of any
//!   WebAssembly requires first configuring and creating an [`Engine`].
//!
//! * [`Module`] - a compiled WebAssembly module. This structure represents
//!   in-memory JIT code which is ready to execute after being instantiated.
//!   It's often important to cache instances of a [`Module`] because creation
//!   (compilation) can be expensive. Note that [`Module`] is safe to share
//!   across threads, and can be created from a WebAssembly binary and an
//!   [`Engine`] with [`Module::new`]. Caching can either happen with
//!   [`Engine::precompile_module`] or [`Module::serialize`], feeding those
//!   bytes back into [`Module::deserialize`].
//!
//! * [`Store`] - container for all information related to WebAssembly objects
//!   such as functions, instances, memories, etc. A [`Store<T>`][`Store`]
//!   allows customization of the `T` to store arbitrary host data within a
//!   [`Store`]. This host data can be accessed through host functions via the
//!   [`Caller`] function parameter in host-defined functions. A [`Store`] is
//!   required for all WebAssembly operations, such as calling a wasm function.
//!   The [`Store`] is passed in as a "context" to methods like [`Func::call`].
//!   Dropping a [`Store`] will deallocate all memory associated with
//!   WebAssembly objects within the [`Store`].
//!
//! * [`Instance`] - an instantiated WebAssembly module. An instance is where
//!   you can actually acquire a [`Func`] from, for example, to call.
//!
//! * [`Func`] - a WebAssembly (or host) function. This can be acquired as the
//!   export of an [`Instance`] to call WebAssembly functions, or it can be
//!   created via functions like [`Func::wrap`] to wrap host-defined
//!   functionality and give it to WebAssembly.
//!
//! * [`Table`], [`Global`], [`Memory`] - other WebAssembly objects which can
//!   either be defined on the host or in wasm itself (via instances). These all
//!   have various ways of being interacted with like [`Func`].
//!
//! All "store-connected" types such as [`Func`], [`Memory`], etc, require the
//! store to be passed in as a context to each method. Methods in wasmtime
//! frequently have their first parameter as either [`impl
//! AsContext`][`AsContext`] or [`impl AsContextMut`][`AsContextMut`]. These
//! traits are implemented for a variety of types, allowing you to, for example,
//! pass the following types into methods:
//!
//! * `&Store<T>`
//! * `&mut Store<T>`
//! * `&Caller<'_, T>`
//! * `&mut Caller<'_, T>`
//! * `StoreContext<'_, T>`
//! * `StoreContextMut<'_, T>`
//!
//! A [`Store`] is the sole owner of all WebAssembly internals. Types like
//! [`Func`] point within the [`Store`] and require the [`Store`] to be provided
//! to actually access the internals of the WebAssembly function, for instance.
//!
//! ## Linking
//!
//! WebAssembly modules almost always require functionality from the host to
//! perform I/O-like tasks. They might refer to quite a few pieces of host
//! functionality, WASI, or maybe even a number of other wasm modules. To assist
//! with managing this a [`Linker`] type is provided to instantiate modules.
//!
//! A [`Linker`] performs name-based resolution of the imports of a WebAssembly
//! module so the [`Linker::instantiate`] method does not take an `imports`
//! argument like [`Instance::new`] does. Methods like [`Linker::define`] or
//! [`Linker::func_wrap`] can be used to define names within a [`Linker`] to
//! later be used for instantiation.
//!
//! For example we can reimplement the above example with a `Linker`:
//!
//! ```
//! use anyhow::Result;
//! use wasmtime::*;
//!
//! fn main() -> Result<()> {
//!     let engine = Engine::default();
//!     let wat = r#"
//!         (module
//!             (import "host" "hello" (func $host_hello (param i32)))
//!
//!             (func (export "hello")
//!                 i32.const 3
//!                 call $host_hello)
//!         )
//!     "#;
//!     let module = Module::new(&engine, wat)?;
//!
//!     // Create a `Linker` and define our host function in it:
//!     let mut linker = Linker::new(&engine);
//!     linker.func_wrap("host", "hello", |caller: Caller<'_, u32>, param: i32| {
//!         println!("Got {} from WebAssembly", param);
//!         println!("my host state is: {}", caller.data());
//!     })?;
//!
//!     // Use the `linker` to instantiate the module, which will automatically
//!     // resolve the imports of the module using name-based resolution.
//!     let mut store = Store::new(&engine, 0);
//!     let instance = linker.instantiate(&mut store, &module)?;
//!     let hello = instance.get_typed_func::<(), (), _>(&mut store, "hello")?;
//!     hello.call(&mut store, ())?;
//!
//!     Ok(())
//! }
//! ```
//!
//! The [`Linker`] type also transparently handles Commands and Reactors
//! as defined by WASI.
//!
//! ## Example Architecture
//!
//! To better understand how Wasmtime types interact with each other let's walk
//! through, at a high-level, an example of how you might use WebAssembly. In
//! our use case let's say we have a web server where we'd like to run some
//! custom WebAssembly on each request. To ensure requests are entirely isolated
//! from each other, though, we'll be creating a new [`Store`] for each
//! request.
//!
//! When the server starts, we'll start off by creating an [`Engine`] (and maybe
//! tweaking [`Config`] settings if necessary). This [`Engine`] will be the only
//! engine for the lifetime of the server itself. Next, we can compile our
//! WebAssembly. You'd create a [`Module`] through the [`Module::new`] API.
//! This will generate JIT code and perform expensive compilation tasks
//! up-front. Finally the last step of initialization would be to create a
//! [`Linker`] which will later be used to instantiate modules, adding
//! functionality like WASI to the linker too.
//!
//! After that setup, the server starts up as usual and is ready to receive
//! requests. Upon receiving a request you'd then create a [`Store`] with
//! [`Store::new`] referring to the original [`Engine`]. Using your [`Module`]
//! and [`Linker`] from before you'd then call [`Linker::instantiate`] to
//! instantiate our module for the request. Both of these operations are
//! designed to be as cheap as possible.
//!
//! With an [`Instance`] you can then invoke various exports and interact with
//! the WebAssembly module. Once the request is finished the [`Store`],
//! is dropped and everything will be deallocated. Note that if the same
//! [`Store`] were used for every request then that would have all requests
//! sharing resources and nothing would ever get deallocated, causing memory
//! usage to baloon and would achive less isolation between requests.
//!
//! ## WASI
//!
//! The `wasmtime` crate does not natively provide support for WASI, but you can
//! use the [`wasmtime-wasi`] crate for that purpose. With [`wasmtime-wasi`] all
//! WASI functions can be added to a [`Linker`] and then used to instantiate
//! WASI-using modules. For more information see the [WASI example in the
//! documentation](https://docs.wasmtime.dev/examples-rust-wasi.html).
//!
//! [`wasmtime-wasi`]: https://crates.io/crates/wasmtime-wasi
//!
//! ## Cross-store usage of items
//!
//! In `wasmtime` wasm items such as [`Global`] and [`Memory`] "belong" to a
//! [`Store`]. The store they belong to is the one they were created with
//! (passed in as a parameter) or instantiated with. This store is the only
//! store that can be used to interact with wasm items after they're created.
//!
//! The `wasmtime` crate will panic if the [`Store`] argument passed in to these
//! operations is incorrect. In other words it's considered a programmer error
//! rather than a recoverable error for the wrong [`Store`] to be used when
//! calling APIs.
//!
//! ## Crate Features
//!
//! The `wasmtime` crate comes with a number of compile-time features that can
//! be used to customize what features it supports. Some of these features are
//! just internal details, but some affect the public API of the `wasmtime`
//! crate. Be sure to check the API you're using to see if any crate features
//! are enabled.
//!
//! * `cache` - Enabled by default, this feature adds support for wasmtime to
//!   perform internal caching of modules in a global location. This must still
//!   be enabled explicitly through [`Config::cache_config_load`] or
//!   [`Config::cache_config_load_default`].
//!
//! * `wat` - Enabled by default, this feature adds support for accepting the
//!   text format of WebAssembly in [`Module::new`]. The text format will be
//!   automatically recognized and translated to binary when compiling a
//!   module.
//!
//! * `parallel-compilation` - Enabled by default, this feature enables support
//!   for compiling functions of a module in parallel with `rayon`.
//!
//! * `async` - Enabled by default, this feature enables APIs and runtime
//!   support for defining asynchronous host functions and calling WebAssembly
//!   asynchronously.
//!
//! * `jitdump` - Enabled by default, this feature compiles in support for the
//!   jitdump runtime profilng format. The profiler can be selected with
//!   [`Config::profiler`].
//!
//! * `vtune` - Not enabled by default, this feature compiles in support for
//!   supporting VTune profiling of JIT code.
//!
//! * `uffd` - Not enabled by default. This feature enables `userfaultfd` support
//!   when using the pooling instance allocator. As handling page faults in user space
//!   comes with a performance penalty, this feature should only be enabled when kernel
//!   lock contention is hampering multithreading throughput. This feature is only
//!   supported on Linux and requires a Linux kernel version 4.11 or higher.
//!
//! * `all-arch` - Not enabled by default. This feature compiles in support for
//!   all architectures for both the JIT compiler and the `wasmtime compile` CLI
//!   command.
//!
//! ## Examples
//!
//! In addition to the examples below be sure to check out the [online embedding
//! documentation][rustdocs] as well as the [online list of examples][examples]
//!
//! [rustdocs]: https://bytecodealliance.github.io/wasmtime/lang-rust.html
//! [examples]: https://bytecodealliance.github.io/wasmtime/examples-rust-embed.html
//!
//! An example of using WASI looks like:
//!
//! ```no_run
//! # use anyhow::Result;
//! # use wasmtime::*;
//! use wasmtime_wasi::sync::WasiCtxBuilder;
//!
//! # fn main() -> Result<()> {
//! // Compile our module and create a `Linker` which has WASI functions defined
//! // within it.
//! let engine = Engine::default();
//! let module = Module::from_file(&engine, "foo.wasm")?;
//! let mut linker = Linker::new(&engine);
//! wasmtime_wasi::add_to_linker(&mut linker, |cx| cx)?;
//!
//! // Configure and create a `WasiCtx`, which WASI functions need access to
//! // through the host state of the store (which in this case is the host state
//! // of the store)
//! let wasi_ctx = WasiCtxBuilder::new().inherit_stdio().build();
//! let mut store = Store::new(&engine, wasi_ctx);
//!
//! // Instantiate our module with the imports we've created, and run it.
//! let instance = linker.instantiate(&mut store, &module)?;
//! // ...
//!
//! # Ok(())
//! # }
//! ```
//!
//! An example of reading a string from a wasm module:
//!
//! ```
//! use std::str;
//!
//! # use wasmtime::*;
//! # fn main() -> anyhow::Result<()> {
//! let mut store = Store::default();
//! let log_str = Func::wrap(&mut store, |mut caller: Caller<'_, ()>, ptr: i32, len: i32| {
//!     // Use our `caller` context to learn about the memory export of the
//!     // module which called this host function.
//!     let mem = match caller.get_export("memory") {
//!         Some(Extern::Memory(mem)) => mem,
//!         _ => return Err(Trap::new("failed to find host memory")),
//!     };
//!
//!     // Use the `ptr` and `len` values to get a subslice of the wasm-memory
//!     // which we'll attempt to interpret as utf-8.
//!     let data = mem.data(&caller)
//!         .get(ptr as u32 as usize..)
//!         .and_then(|arr| arr.get(..len as u32 as usize));
//!     let string = match data {
//!         Some(data) => match str::from_utf8(data) {
//!             Ok(s) => s,
//!             Err(_) => return Err(Trap::new("invalid utf-8")),
//!         },
//!         None => return Err(Trap::new("pointer/length out of bounds")),
//!     };
//!     assert_eq!(string, "Hello, world!");
//!     println!("{}", string);
//!     Ok(())
//! });
//! let module = Module::new(
//!     store.engine(),
//!     r#"
//!         (module
//!             (import "" "" (func $log_str (param i32 i32)))
//!             (func (export "foo")
//!                 i32.const 4   ;; ptr
//!                 i32.const 13  ;; len
//!                 call $log_str)
//!             (memory (export "memory") 1)
//!             (data (i32.const 4) "Hello, world!"))
//!     "#,
//! )?;
//! let instance = Instance::new(&mut store, &module, &[log_str.into()])?;
//! let foo = instance.get_typed_func::<(), (), _>(&mut store, "foo")?;
//! foo.call(&mut store, ())?;
//! # Ok(())
//! # }
//! ```

#![allow(unknown_lints)]
#![deny(missing_docs, broken_intra_doc_links)]
#![doc(test(attr(deny(warnings))))]
#![doc(test(attr(allow(dead_code, unused_variables, unused_mut))))]
#![cfg_attr(nightlydoc, feature(doc_cfg))]
#![cfg_attr(not(feature = "default"), allow(dead_code, unused_imports))]

#[macro_use]
mod func;

mod config;
mod engine;
mod externals;
mod instance;
mod limits;
mod linker;
mod memory;
mod module;
mod r#ref;
mod signatures;
mod store;
mod trampoline;
mod trap;
mod types;
mod values;

pub use crate::config::*;
pub use crate::engine::*;
pub use crate::externals::*;
pub use crate::func::*;
pub use crate::instance::{Instance, InstancePre};
pub use crate::limits::*;
pub use crate::linker::*;
pub use crate::memory::*;
pub use crate::module::{FrameInfo, FrameSymbol, Module};
pub use crate::r#ref::ExternRef;
#[cfg(feature = "async")]
pub use crate::store::WasmtimeFuture;
pub use crate::store::{
    AsContext, AsContextMut, InterruptHandle, Store, StoreContext, StoreContextMut,
};
pub use crate::trap::*;
pub use crate::types::*;
pub use crate::values::*;

cfg_if::cfg_if! {
    if #[cfg(target_os = "macos")] {
        // no extensions for macOS at this time
    } else if #[cfg(unix)] {
        pub mod unix;
    } else if #[cfg(windows)] {
        pub mod windows;
    } else {
        // ... unknown os!
    }
}

fn _assert_send_sync() {
    fn _assert<T: Send + Sync>() {}
    fn _assert_send<T: Send>(_t: T) {}
    _assert::<Engine>();
    _assert::<Config>();
    _assert::<InterruptHandle>();
    _assert::<(Func, TypedFunc<(), ()>, Global, Table, Memory)>();
    _assert::<Instance>();
    _assert::<Module>();
    _assert::<Store<()>>();
    _assert::<StoreContext<'_, ()>>();
    _assert::<StoreContextMut<'_, ()>>();
    _assert::<Caller<'_, ()>>();
    _assert::<Linker<()>>();
    _assert::<Linker<*mut u8>>();
    _assert::<ExternRef>();
    _assert::<InstancePre<()>>();
    _assert::<InstancePre<*mut u8>>();

    #[cfg(feature = "async")]
    fn _call_async(s: &mut Store<()>, f: Func) {
        _assert_send(f.call_async(&mut *s, &[]))
    }
    #[cfg(feature = "async")]
    fn _typed_call_async(s: &mut Store<()>, f: TypedFunc<(), ()>) {
        _assert_send(f.call_async(&mut *s, ()))
    }
    #[cfg(feature = "async")]
    fn _instantiate_async(s: &mut Store<()>, m: &Module) {
        _assert_send(Instance::new_async(s, m, &[]))
    }
}
