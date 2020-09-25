//! Wasmtime's embedding API
//!
//! This crate contains an API used to interact with WebAssembly modules. For
//! example you can compile modules, instantiate them, call them, etc. As an
//! embedder of WebAssembly you can also provide WebAssembly modules
//! functionality from the host by creating host-defined functions, memories,
//! globals, etc, which can do things that WebAssembly cannot (such as print to
//! the screen).
//!
//! The `wasmtime` crate draws inspiration from a number of sources, including
//! the [JS WebAssembly
//! API](https://developer.mozilla.org/en-US/docs/WebAssembly) as well as the
//! [proposed C API](https://github.com/webassembly/wasm-c-api). As with all
//! other Rust code you're guaranteed that programs will be safe (not have
//! undefined behavior or segfault) so long as you don't use `unsafe` in your
//! own program. With `wasmtime` you can easily and conveniently embed a
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
//!     // All wasm objects operate within the context of a "store"
//!     let store = Store::default();
//!
//!     // Modules can be compiled through either the text or binary format
//!     let wat = r#"
//!         (module
//!             (import "" "" (func $host_hello (param i32)))
//!
//!             (func (export "hello")
//!                 i32.const 3
//!                 call $host_hello)
//!         )
//!     "#;
//!     let module = Module::new(store.engine(), wat)?;
//!
//!     // Host functions can be defined which take/return wasm values and
//!     // execute arbitrary code on the host.
//!     let host_hello = Func::wrap(&store, |param: i32| {
//!         println!("Got {} from WebAssembly", param);
//!     });
//!
//!     // Instantiation of a module requires specifying its imports and then
//!     // afterwards we can fetch exports by name, as well as asserting the
//!     // type signature of the function with `get0`.
//!     let instance = Instance::new(&store, &module, &[host_hello.into()])?;
//!     let hello = instance
//!         .get_func("hello")
//!         .ok_or(anyhow::format_err!("failed to find `hello` function export"))?
//!         .get0::<()>()?;
//!
//!     // And finally we can call the wasm as if it were a Rust function!
//!     hello()?;
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
//! * Reference counting - almost all objects in this API are reference counted.
//!   Most of the time when and object is `clone`d you're just bumping a
//!   reference count. For example when you clone an [`Instance`] that is a
//!   cheap operation, it doesn't create an entirely new instance.
//!
//! * [`Store`] - all WebAssembly object and host values will be "connected" to
//!   a store. A [`Store`] is not threadsafe which means that itself and all
//!   objects connected to it are pinned to a single thread (this happens
//!   automatically through a lack of the `Send` and `Sync` traits). Similarly
//!   `wasmtime` does not have a garbage collector so anything created within a
//!   [`Store`] will not be deallocated until all references have gone away. See
//!   the [`Store`] documentation for more information.
//!
//! * [`Module`] - a compiled WebAssembly module. This structure represents
//!   in-memory JIT code which is ready to execute after being instantiated.
//!   It's often important to cache instances of a [`Module`] because creation
//!   (compilation) can be expensive. Note that [`Module`] is safe to share
//!   across threads.
//!
//! * [`Instance`] - an instantiated WebAssembly module. An instance is where
//!   you can actually acquire a [`Func`] from, for example, to call. Each
//!   [`Instance`], like all other [`Store`]-connected objects, cannot be sent
//!   across threads.
//!
//! There are other important types within the `wasmtime` crate but it's crucial
//! to be familiar with the above types! Be sure to browse the API documentation
//! to get a feeling for what other functionality is offered by this crate.
//!
//! ## Example Architecture
//!
//! To better understand how Wasmtime types interact with each other let's walk
//! through, at a high-level, an example of how you might use WebAssembly. In
//! our use case let's say we have a web server where we'd like to run some
//! custom WebAssembly on each request. To ensure requests are isolated from
//! each other, though, we'll be creating a new [`Instance`] for each request.
//!
//! When the server starts, we'll start off by creating an [`Engine`] (and maybe
//! tweaking [`Config`] settings if necessary). This [`Engine`] will be the only
//! engine for the lifetime of the server itself.
//!
//! Next, we can compile our WebAssembly. You'd create a [`Module`] through the
//! [`Module::new`] API. This will generate JIT code and perform expensive
//! compilation tasks up-front.
//!
//! After that setup, the server starts up as usual and is ready to receive
//! requests. Upon receiving a request you'd then create a [`Store`] with
//! [`Store::new`] referring to the original [`Engine`]. Using your [`Module`]
//! from before you'd then call [`Instance::new`] to instantiate our module for
//! the request. Both of these operations are designed to be as cheap as
//! possible.
//!
//! With an [`Instance`] you can then invoke various exports and interact with
//! the WebAssembly module. Once the request is finished the [`Store`],
//! [`Instance`], and all other items loaded are dropped and everything will be
//! deallocated. Note that it's crucial to create a [`Store`]-per-request to
//! ensure that memory usage doesn't balloon accidentally by keeping a [`Store`]
//! alive indefinitely.
//!
//! ## Advanced Linking
//!
//! Often WebAssembly modules are not entirely self-isolated. They might refer
//! to quite a few pieces of host functionality, WASI, or maybe even a number of
//! other wasm modules. To help juggling all this together this crate provides a
//! [`Linker`] type which serves as an abstraction to assist in instantiating a
//! module. The [`Linker`] type also transparently handles Commands and Reactors
//! as defined by WASI.
//!
//! ## WASI
//!
//! The `wasmtime` crate does not natively provide support for WASI, but you can
//! use the `wasmtime-wasi` crate for that purpose. With `wasmtime-wasi` you can
//! create a "wasi instance" and then add all of its items into a [`Linker`],
//! which can then be used to instantiate a [`Module`] that uses WASI.
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
//! use wasmtime_wasi::{Wasi, WasiCtx};
//!
//! # fn main() -> Result<()> {
//! let store = Store::default();
//! let mut linker = Linker::new(&store);
//!
//! // Create an instance of `Wasi` which contains a `WasiCtx`. Note that
//! // `WasiCtx` provides a number of ways to configure what the target program
//! // will have access to.
//! let wasi = Wasi::new(&store, WasiCtx::new(std::env::args())?);
//! wasi.add_to_linker(&mut linker)?;
//!
//! // Instantiate our module with the imports we've created, and run it.
//! let module = Module::from_file(store.engine(), "foo.wasm")?;
//! let instance = linker.instantiate(&module)?;
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
//! let store = Store::default();
//! let log_str = Func::wrap(&store, |caller: Caller<'_>, ptr: i32, len: i32| {
//!     let mem = match caller.get_export("memory") {
//!         Some(Extern::Memory(mem)) => mem,
//!         _ => return Err(Trap::new("failed to find host memory")),
//!     };
//!
//!     // We're reading raw wasm memory here so we need `unsafe`. Note
//!     // though that this should be safe because we don't reenter wasm
//!     // while we're reading wasm memory, nor should we clash with
//!     // any other memory accessors (assuming they're well-behaved
//!     // too).
//!     unsafe {
//!         let data = mem.data_unchecked()
//!             .get(ptr as u32 as usize..)
//!             .and_then(|arr| arr.get(..len as u32 as usize));
//!         let string = match data {
//!             Some(data) => match str::from_utf8(data) {
//!                 Ok(s) => s,
//!                 Err(_) => return Err(Trap::new("invalid utf-8")),
//!             },
//!             None => return Err(Trap::new("pointer/length out of bounds")),
//!         };
//!         assert_eq!(string, "Hello, world!");
//!         println!("{}", string);
//!     }
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
//! let instance = Instance::new(&store, &module, &[log_str.into()])?;
//! let foo = instance.get_func("foo").unwrap().get0::<()>()?;
//! foo()?;
//! # Ok(())
//! # }
//! ```

#![allow(unknown_lints)]
#![deny(missing_docs, broken_intra_doc_links)]
#![doc(test(attr(deny(warnings))))]
#![doc(test(attr(allow(dead_code, unused_variables, unused_mut))))]

pub mod debugger;
mod externals;
mod frame_info;
mod func;
mod instance;
mod linker;
mod module;
mod r#ref;
mod runtime;
mod trampoline;
mod trap;
mod types;
mod values;

pub use crate::externals::*;
pub use crate::frame_info::FrameInfo;
pub use crate::func::*;
pub use crate::instance::Instance;
pub use crate::linker::*;
pub use crate::module::Module;
pub use crate::r#ref::ExternRef;
pub use crate::runtime::*;
pub use crate::trap::Trap;
pub use crate::types::*;
pub use crate::values::*;

cfg_if::cfg_if! {
    if #[cfg(unix)] {
        pub mod unix;
    } else if #[cfg(windows)] {
        pub mod windows;
    } else {
        // ... unknown os!
    }
}
