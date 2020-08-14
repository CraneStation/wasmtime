# Introduction

[Wasmtime][github], a [Bytecode Alliance][BA] project, is used to run
[Webassembly][Wasm] (Wasm) programs [without a web browser].  
Typically, an application calls embedded Wasmtime to load and run Wasm
[modules]. Or, the Wasmtime command line utility can run standalone Wasm
programs.

[Webassembly][Wasm] (Wasm) is an assembly language for a [virtual
stack machine][VSM] running inside an application.  High-level
languages must be compiled to Wasm prior to loading. Upon loading,
Wasmtime compiles Wasm to native machine code.

[WebAssembly System Interface][WASI] (WASI) provides a library of
essential functions, such as reading files, somewhat similar to libc.

Wasmtime strives to be a highly configurable embedded runtime for
any scale of application. Many features are still under development so if you
have a question don't hesitate to [file an issue][issue].

This guide is intended to serve a number of purposes and within you'll find:

* [How to create simple Wasm modules](tutorial-create-hello-world.md)
* [How to use Wasmtime from a number of languages](lang.md)
* [How to use install and use the `Wasmtime` CLI](cli.md)
* Information about [stability](stability.md) and [security](security.md) in
  Wasmtime.

... and more! The source for this guide [lives on
GitHub](https://github.com/bytecodealliance/wasmtime/tree/main/docs) and
contributions are welcome!

[github]: https://github.com/bytecodealliance/wasmtime
[BA]: https://bytecodealliance.org/
[VSM]: https://en.wikipedia.org/wiki/Stack_machine
[Wasm]: https://webassembly.org/
[WASI]: https://wasi.dev
[without a web browser]: https://webassembly.org/docs/non-web/
[Wasm-bindgen]: https://rustwasm.github.io/docs/wasm-bindgen/
[issue]: https://github.com/bytecodealliance/wasmtime/issues/new
[modules]: https://doc.rust-lang.org/book/ch07-02-defining-modules-to-control-scope-and-privacy.html
