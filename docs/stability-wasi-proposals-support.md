# WASI Proposals Support

Wasmtime has initial support for some WASI proposals to assist in their
development as well as allowing users to more easily provide feedback by testing
them. This is somewhat of a work-in-progress because there aren't a lot of WASI
proposals at this time.

Currently supported WASI proposals are:

* [The base WASI specification](https://github.com/webassembly/wasi) - the
  `snapshot` directory is supported by the [`wasmtime-wasi`
  crate](https://docs.rs/wasmtime-wasi/0.21.0/wasmtime_wasi/)

* [The wasi-nn proposal](https://github.com/WebAssembly/wasi-nn) - the
  `ephemeral` directory with the wasi-nn APIs.

Wasmtime strives to support WASI proposals in-tree to assist with
maintainability and provide easier to support to users for easier feedback. In
doing so, however, we have a few requirements around adding support for a WASI
proposal to Wasmtime:

* Proposals must be accompanied with an
  [RFC](https://github.com/bytecodealliance/rfcs) which should help explain
  motivation, implementation details if necessary, etc.

* The proposal must be tested and exercised on CI. These tests don't need to be
  overly exhaustive but a "smoke test" should pass. Additionally CI should not
  be overly long compared to existing tests run, and it should also be reliable
  and not have flaky tests. Wasmtime contributors may disable CI for proposals
  at any time if needed, and maintainers for the proposal will be notified.

* The size of the code in the wasmtime repository should not be unduly large.
  This helps keep the size of the repo within reasonable limits and relatively
  easy for others not working on the proposal to still be productive.

* Compile-time and runtime requirements for Wasmtime depends on where the
  proposal is [in the WASI staging
  process](https://github.com/WebAssembly/WASI/blob/master/docs/Process.md):

  * Stage 0 - support must be disabled by default at compile time and usage at
    runtime must require a flag.

  * Stage 1 - support may be enabled by default at compile time and usage at
    runtime must require a flag.

  * Stage 2+ - support may be enabled by default at compile time and may not
    require a runtime flag.

  It's ok if the initial implementation doesn't support every platform and
  architecture out-of-the-box (stage 0), but as it moves through the stages it's
  expected to become more complete.

We may tweak these guidelines over time but this is our working set of
assumptions for now!
