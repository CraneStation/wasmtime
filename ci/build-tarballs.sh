#!/bin/bash

# A small script used for assembling release tarballs for both the `wasmtime`
# binary and the C API. This is executed with two arguments, mostly coming from
# the CI matrix.
#
# * The first argument is the name of the platform, used to name the release
# * The second argument is the "target", if present, currently only for
#   cross-compiles
#
# This expects the build to already be done and will assemble release artifacts
# in `dist/`

set -ex

platform=$1
target=$2

rm -rf tmp
mkdir tmp
mkdir -p dist

tag=dev
if [[ $GITHUB_REF == refs/tags/v* ]]; then
  tag=${GITHUB_REF:10}
fi

bin_pkgname=wasmtime-$tag-$platform
api_pkgname=wasmtime-$tag-$platform-c-api

mkdir tmp/$api_pkgname
mkdir tmp/$api_pkgname/lib
mkdir tmp/$api_pkgname/include
mkdir tmp/$bin_pkgname
cp LICENSE README.md tmp/$api_pkgname
cp LICENSE README.md tmp/$bin_pkgname
cp -r crates/c-api/include tmp/$api_pkgname
cp crates/c-api/wasm-c-api/include/wasm.h tmp/$api_pkgname/include

fmt=tar
if [ "$platform" = "x86_64-windows" ]; then
  cp target/release/wasmtime.exe tmp/$bin_pkgname
  cp target/release/{wasmtime.dll,wasmtime.lib,wasmtime.dll.lib} tmp/$api_pkgname/lib
  fmt=zip

  # Generate a `*.msi` installer for Windows as well
  export WT_VERSION=`cat Cargo.toml | sed -n 's/^version = "\([^"]*\)".*/\1/p'`
  "$WIX/bin/candle" -arch x64 -out target/wasmtime.wixobj ci/wasmtime.wxs
  "$WIX/bin/light" -out dist/$bin_pkgname.msi target/wasmtime.wixobj -ext WixUtilExtension
  rm dist/$bin_pkgname.wixpdb
elif [ "$platform" = "x86_64-mingw" ]; then
  cp target/x86_64-pc-windows-gnu/release/wasmtime.exe tmp/$bin_pkgname
  cp target/x86_64-pc-windows-gnu/release/{wasmtime.dll,libwasmtime.a} tmp/$api_pkgname/lib
  fmt=zip
elif [ "$platform" = "x86_64-macos" ]; then
  # Postprocess the macOS dylib a bit to have a more reasonable `LC_ID_DYLIB`
  # directive than the default one that comes out of the linker when typically
  # doing `cargo build`. For more info see #984
  install_name_tool -id "@rpath/libwasmtime.dylib" target/release/libwasmtime.dylib
  cp target/release/wasmtime tmp/$bin_pkgname
  cp target/release/libwasmtime.{a,dylib} tmp/$api_pkgname/lib
elif [ "$platform" = "x86_64-linux-musl" ]; then
  cp target/$target/release/wasmtime tmp/$bin_pkgname
  cp target/$target/release/libwasmtime.a tmp/$api_pkgname/lib # no libwasmtime.so for musl
elif [ "$target" = "" ]; then
  cp target/release/wasmtime tmp/$bin_pkgname
  cp target/release/libwasmtime.{a,so} tmp/$api_pkgname/lib
else
  cp target/$target/release/wasmtime tmp/$bin_pkgname
  cp target/$target/release/libwasmtime.{a,so} tmp/$api_pkgname/lib
fi


mktarball() {
  dir=$1
  if [ "$fmt" = "tar" ]; then
    # this is a bit wonky, but the goal is to use `xz` with threaded compression
    # to ideally get better performance with the `-T0` flag.
    tar -cvf - -C tmp $dir | xz -9 -T0 > dist/$dir.tar.xz
  else
    # Note that this runs on Windows, and it looks like GitHub Actions doesn't
    # have a `zip` tool there, so we use something else
    (cd tmp && 7z a ../dist/$dir.zip $dir/)
  fi
}

mktarball $api_pkgname
mktarball $bin_pkgname
