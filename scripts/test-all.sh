#!/bin/bash
set -euo pipefail

# This is the top-level test script:
#
# - Check code formatting.
# - Make a debug build.
# - Make a release build.
# - Run unit tests for all Rust crates
# - Build API documentation.
# - Optionally, run fuzzing.
#
# All tests run by this script should be passing at all times.

# Repository top-level directory.
topdir=$(dirname "$0")/..
cd "$topdir"

function banner {
    echo "======  $*  ======"
}

# Run rustfmt if we have it.
banner "Rust formatting"
if cargo +stable fmt -- --version > /dev/null ; then
    if ! "$topdir/scripts/format-all.sh" --check ; then
        echo "Formatting diffs detected! Run \"cargo fmt --all\" to correct."
        exit 1
    fi
else
    echo "cargo-fmt not available; formatting not checked!"
    echo
    echo "If you are using rustup, rustfmt can be installed via"
    echo "\"rustup component add --toolchain=stable rustfmt-preview\", or see"
    echo "https://github.com/rust-lang-nursery/rustfmt for more information."
fi

# Make sure the code builds in release mode.
banner "Rust release build"
cargo build --release

# Make sure the code builds in debug mode.
banner "Rust debug build"
cargo build

# Run the tests. We run these in debug mode so that assertions are enabled.
banner "Rust unit tests"

# TODO: lightbeam currently requires rust nightly, so don't try to run the
# tests here. Name all the other packages, rather than using --all. We'll
# run the lightbeam tests below if nightly is available.
#RUST_BACKTRACE=1 cargo test --all
RUST_BACKTRACE=1 cargo test \
  --package wasmtime-cli \
  --package wasmtime \
  --package wasmtime-wasi \
  --package wasmtime-wast \
  --package wasmtime-debug \
  --package wasmtime-environ \
  --package wasmtime-runtime \
  --package wasmtime-jit \
  --package wasmtime-obj \
  --package wiggle \
  --package wiggle-generate \
  --package wiggle-test \
  --package wiggle-macro \
  --package wasi-common

# Make sure the documentation builds.
banner "Rust documentation: $topdir/target/doc/wasmtime/index.html"
cargo doc

# Ensure fuzzer works by running it with a single input
# Note LSAN is disabled due to https://github.com/google/sanitizers/issues/764
banner "cargo fuzz check"
if rustup toolchain list | grep -q nightly; then
    if cargo install --list | grep -q cargo-fuzz; then
        echo "cargo-fuzz found"
    else
        echo "installing cargo-fuzz"
        cargo +nightly install cargo-fuzz
    fi

    fuzz_module="1340712d77d3db3c79b4b0c1494df18615485480"
    ASAN_OPTIONS=detect_leaks=0 \
    cargo +nightly fuzz run compile \
        "$topdir/fuzz/corpus/compile/$fuzz_module"

    # Nightly is available, so also run lightbeam's tests, which we
    # skipped earlier.
    cargo +nightly test --features lightbeam --package lightbeam
    cargo +nightly test --features lightbeam

    # Also run wasmtime-py and wasmtime-rust's tests.
    RUST_BACKTRACE=1 cargo +nightly test \
      --package wasmtime-py \
      --package wasmtime-rust
else
    echo "nightly toolchain not found, skipping fuzz target integration test"
fi

banner "OK"
