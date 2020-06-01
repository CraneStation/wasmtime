//! Build program to generate a program which runs all the testsuites.
//!
//! By generating a separate `#[test]` test for each file, we allow cargo test
//! to automatically run the files in parallel.

use anyhow::Context;
use std::env;
use std::fmt::Write;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() -> anyhow::Result<()> {
    println!("cargo:rerun-if-changed=build.rs");
    let out_dir = PathBuf::from(
        env::var_os("OUT_DIR").expect("The OUT_DIR environment variable must be set"),
    );
    let mut out = String::new();

    for strategy in &[
        "Cranelift",
        #[cfg(feature = "lightbeam")]
        "Lightbeam",
    ] {
        writeln!(out, "#[allow(non_snake_case)]")?;
        writeln!(out, "mod {} {{", strategy)?;

        with_test_module(&mut out, "misc", |out| {
            test_directory(out, "tests/misc_testsuite", strategy)?;
            test_directory_module(out, "tests/misc_testsuite/bulk-memory-operations", strategy)?;
            test_directory_module(out, "tests/misc_testsuite/reference-types", strategy)?;
            Ok(())
        })?;

        with_test_module(&mut out, "spec", |out| {
            let spec_tests = test_directory(out, "tests/spec_testsuite", strategy)?;
            // Skip running spec_testsuite tests if the submodule isn't checked
            // out.
            if spec_tests > 0 {
                test_directory_module(out, "tests/spec_testsuite/proposals/simd", strategy)?;
                test_directory_module(out, "tests/spec_testsuite/proposals/multi-value", strategy)?;
                test_directory_module(
                    out,
                    "tests/spec_testsuite/proposals/reference-types",
                    strategy,
                )?;
                test_directory_module(
                    out,
                    "tests/spec_testsuite/proposals/bulk-memory-operations",
                    strategy,
                )?;
            } else {
                println!(
                    "cargo:warning=The spec testsuite is disabled. To enable, run `git submodule \
                 update --remote`."
                );
            }
            Ok(())
        })?;

        writeln!(out, "}}")?;
    }

    // Write out our auto-generated tests and opportunistically format them with
    // `rustfmt` if it's installed.
    let output = out_dir.join("wast_testsuite_tests.rs");
    fs::write(&output, out)?;
    drop(Command::new("rustfmt").arg(&output).status());
    Ok(())
}

fn test_directory_module(
    out: &mut String,
    path: impl AsRef<Path>,
    strategy: &str,
) -> anyhow::Result<usize> {
    let path = path.as_ref();
    let testsuite = &extract_name(path);
    with_test_module(out, testsuite, |out| test_directory(out, path, strategy))
}

fn test_directory(
    out: &mut String,
    path: impl AsRef<Path>,
    strategy: &str,
) -> anyhow::Result<usize> {
    let path = path.as_ref();
    let mut dir_entries: Vec<_> = path
        .read_dir()
        .context(format!("failed to read {:?}", path))?
        .map(|r| r.expect("reading testsuite directory entry"))
        .filter_map(|dir_entry| {
            let p = dir_entry.path();
            let ext = p.extension()?;
            // Only look at wast files.
            if ext != "wast" {
                return None;
            }
            // Ignore files starting with `.`, which could be editor temporary files
            if p.file_stem()?.to_str()?.starts_with(".") {
                return None;
            }
            Some(p)
        })
        .collect();

    dir_entries.sort();

    let testsuite = &extract_name(path);
    for entry in dir_entries.iter() {
        write_testsuite_tests(out, entry, testsuite, strategy)?;
    }

    Ok(dir_entries.len())
}

/// Extract a valid Rust identifier from the stem of a path.
fn extract_name(path: impl AsRef<Path>) -> String {
    path.as_ref()
        .file_stem()
        .expect("filename should have a stem")
        .to_str()
        .expect("filename should be representable as a string")
        .replace("-", "_")
        .replace("/", "_")
}

fn with_test_module<T>(
    out: &mut String,
    testsuite: &str,
    f: impl FnOnce(&mut String) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    out.push_str("mod ");
    out.push_str(testsuite);
    out.push_str(" {\n");

    let result = f(out)?;

    out.push_str("}\n");
    Ok(result)
}

fn write_testsuite_tests(
    out: &mut String,
    path: impl AsRef<Path>,
    testsuite: &str,
    strategy: &str,
) -> anyhow::Result<()> {
    let path = path.as_ref();
    let testname = extract_name(path);

    writeln!(out, "mk_test! {{")?;
    if ignore(testsuite, &testname, strategy) {
        writeln!(out, "#[ignore]")?;
    }
    writeln!(
        out,
        "r#{}, r#\"{}\"#, Strategy::{}",
        &testname,
        path.display(),
        strategy
    )?;
    writeln!(out, "}}")?;
    writeln!(out)?;
    Ok(())
}

/// Ignore tests that aren't supported yet.
fn ignore(testsuite: &str, testname: &str, strategy: &str) -> bool {
    let target = env::var("TARGET").unwrap();
    match strategy {
        #[cfg(feature = "lightbeam")]
        "Lightbeam" => match (testsuite, testname) {
            ("simd", _) => return true,
            ("multi_value", _) => return true,
            ("reference_types", _) => return true,
            ("bulk_memory_operations", _) => return true,
            _ => (),
        },
        "Cranelift" => match (testsuite, testname) {
            ("simd", "simd_store") => return false,
            // Most simd tests are known to fail on aarch64 for now, it's going
            // to be a big chunk of work to implement them all there!
            ("simd", _) if target.contains("aarch64") => return true,

            ("simd", "simd_conversions") => return true, // FIXME Unsupported feature: proposed SIMD operator I16x8NarrowI32x4S
            ("simd", "simd_f32x4") => return true, // FIXME expected V128(F32x4([CanonicalNan, CanonicalNan, Value(Float32 { bits: 0 }), Value(Float32 { bits: 0 })])), got V128(18428729675200069632)
            ("simd", "simd_f64x2") => return true, // FIXME expected V128(F64x2([Value(Float64 { bits: 9221120237041090560 }), Value(Float64 { bits: 0 })])), got V128(0)
            ("simd", "simd_f64x2_arith") => return true, // FIXME expected V128(F64x2([Value(Float64 { bits: 9221120237041090560 }), Value(Float64 { bits: 13835058055282163712 })])), got V128(255211775190703847615975447847722024960)
            ("simd", "simd_i64x2_arith") => return true, // FIXME Unsupported feature: proposed SIMD operator I64x2Mul
            ("simd", "simd_load") => return true, // FIXME Unsupported feature: proposed SIMD operator I8x16Shl
            ("simd", "simd_splat") => return true, // FIXME Unsupported feature: proposed SIMD operator I8x16ShrS

            // not parsed in wasmparser yet
            ("simd", "simd_i32x4_arith2") => return true,
            ("simd", "simd_i16x8_arith2") => return true,
            ("simd", "simd_i8x16_arith2") => return true,

            // waiting for the upstream spec to get updated with new binary
            // encodings of operations and for that to propagate to the
            // testsuite repo.
            ("simd", "simd_const") => return true,

            // Still working on implementing these. See #929.
            ("reference_types", "table_copy_on_imported_tables") => return false,
            ("reference_types", _) => return true,

            _ => {}
        },
        _ => panic!("unrecognized strategy"),
    }

    false
}
