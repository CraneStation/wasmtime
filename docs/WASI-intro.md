# Welcome to WASI!

WASI stands for WebAssembly System Interface. It's an API designed by
the [Wasmtime] project that provides access to several operating-system-like
features, including files and filesystems, Berkeley sockets, clocks, and
random numbers, that we'll be proposing for standardization.

It's designed to be independent of browsers, so it doesn't depend on
Web APIs or JS, and isn't limited by the need to be compatible with JS.
And it has integrated capability-based security, so it extends
WebAssembly's characteristic sandboxing to include I/O.

See the [WASI Overview](WASI-overview.md) for more detailed background
information, and the [WASI Tutorial](WASI-tutorial.md) for a walkthrough
showing how various pieces fit together.

Note that everything here is a prototype, and while a lot of stuff works,
there are numerous missing features and some rough edges. For example,
networking support is incomplete.

## How can I write programs that use WASI?

The two toolchains that currently work well are the Rust toolchain and
a specially packaged C and C++ toolchain. Of course, we hope other
toolchains will be able to implement WASI as well!

### Rust

To install a WASI-enabled Rust toolchain, see the [online section of the
guide](https://bytecodealliance.github.io/wasmtime/wasm-rust.html)

### C/C++

To install a WASI-enabled C/C++ toolchain, see the [online section of the
guide](https://bytecodealliance.github.io/wasmtime/wasm-c.html)

## How can I run programs that use WASI?

Currently the options are [Wasmtime] and the [browser polyfill], though we
intend WASI to be implementable in many wasm VMs.

[Wasmtime]: https://github.com/bytecodealliance/wasmtime
[browser polyfill]: https://wasi.dev/polyfill/

### Wasmtime

[Wasmtime] is a non-Web WebAssembly engine which is part of the
[Bytecode Alliance project](https://bytecodealliance.org). To build
it, download the code and build with `cargo build --release`. It can
run WASI-using wasm programs by simply running `wasmtime foo.wasm`,
or `cargo run --bin wasmtime foo.wasm`.

### The browser polyfill

The polyfill is online [here](https://wasi.dev/polyfill/).

The source is [here](https://github.com/bytecodealliance/wasmtime/tree/177af5357880c33863cbe3363754000dbd23bddb/crates/wasi-c).

## Where can I learn more?

Beyond the [WASI Overview](WASI-overview.md), take a look at the
various [WASI documents](WASI-documents.md).
