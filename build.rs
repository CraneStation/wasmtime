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
        writeln!(out, "#[cfg(test)]")?;
        writeln!(out, "#[allow(non_snake_case)]")?;
        writeln!(out, "mod {} {{", strategy)?;

        with_test_module(&mut out, "misc", |out| {
            test_directory(out, "tests/misc_testsuite", strategy)?;
            test_directory_module(out, "tests/misc_testsuite/bulk-memory-operations", strategy)?;
            test_directory_module(out, "tests/misc_testsuite/reference-types", strategy)?;
            test_directory_module(out, "tests/misc_testsuite/multi-memory", strategy)?;
            Ok(())
        })?;

        with_test_module(&mut out, "spec", |out| {
            let spec_tests = test_directory(out, "tests/spec_testsuite", strategy)?;
            // Skip running spec_testsuite tests if the submodule isn't checked
            // out.
            if spec_tests > 0 {
                test_directory_module(out, "tests/spec_testsuite/proposals/simd", strategy)?;
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

    writeln!(out, "#[test]")?;
    if experimental_x64_should_panic(testsuite, &testname, strategy) {
        writeln!(
            out,
            r#"#[cfg_attr(feature = "experimental_x64", should_panic)]"#
        )?;
    } else if ignore(testsuite, &testname, strategy) {
        writeln!(out, "#[ignore]")?;
    }
    writeln!(out, "fn r#{}() {{", &testname)?;
    writeln!(out, "    let _ = env_logger::try_init();")?;
    writeln!(
        out,
        "    crate::wast::run_wast(r#\"{}\"#, crate::wast::Strategy::{}).unwrap();",
        path.display(),
        strategy
    )?;
    writeln!(out, "}}")?;
    writeln!(out)?;
    Ok(())
}

/// For experimental_x64 backend features that are not supported yet, mark tests as panicking, so
/// they stop "passing" once the features are properly implemented.
fn experimental_x64_should_panic(testsuite: &str, testname: &str, strategy: &str) -> bool {
    if !cfg!(feature = "experimental_x64") || strategy != "Cranelift" {
        return false;
    }

    match (testsuite, testname) {
        ("simd", "simd_address") => return false,
        ("simd", "simd_bitwise") => return false,
        ("simd", "simd_bit_shift") => return false,
        ("simd", "simd_boolean") => return false,
        ("simd", "simd_const") => return false,
        ("simd", "simd_i8x16_arith") => return false,
        ("simd", "simd_i8x16_arith2") => return false,
        ("simd", "simd_i8x16_cmp") => return false,
        ("simd", "simd_i8x16_sat_arith") => return false,
        ("simd", "simd_i16x8_arith") => return false,
        ("simd", "simd_i16x8_arith2") => return false,
        ("simd", "simd_i16x8_cmp") => return false,
        ("simd", "simd_i16x8_sat_arith") => return false,
        ("simd", "simd_i32x4_arith") => return false,
        ("simd", "simd_i32x4_arith2") => return false,
        ("simd", "simd_i32x4_cmp") => return false,
        ("simd", "simd_i64x2_arith") => return false,
        ("simd", "simd_f32x4") => return false,
        ("simd", "simd_f32x4_arith") => return false,
        ("simd", "simd_f32x4_cmp") => return false,
        ("simd", "simd_f32x4_pmin_pmax") => return false,
        ("simd", "simd_f64x2") => return false,
        ("simd", "simd_f64x2_arith") => return false,
        ("simd", "simd_f64x2_cmp") => return false,
        ("simd", "simd_f64x2_pmin_pmax") => return false,
        ("simd", "simd_lane") => return false,
        ("simd", "simd_load") => return false,
        ("simd", "simd_load_splat") => return false,
        ("simd", "simd_splat") => return false,
        ("simd", "simd_store") => return false,
        ("simd", _) => return true,
        _ => {}
    }

    false
}

/// Ignore tests that aren't supported yet.
fn ignore(testsuite: &str, testname: &str, strategy: &str) -> bool {
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
            // TODO(#1886): Ignore reference types tests if this isn't x64,
            // because Cranelift only supports reference types on x64.
            ("reference_types", _) => {
                return env::var("CARGO_CFG_TARGET_ARCH").unwrap() != "x86_64";
            }

            // These are only implemented on aarch64 and x64.
            ("simd", "simd_boolean")
            | ("simd", "simd_f32x4_pmin_pmax")
            | ("simd", "simd_f64x2_pmin_pmax") => {
                return !(cfg!(feature = "experimental_x64")
                    || env::var("CARGO_CFG_TARGET_ARCH").unwrap() == "aarch64")
            }

            // These are only implemented on aarch64.
            ("simd", "simd_f32x4_rounding") | ("simd", "simd_f64x2_rounding") => {
                return env::var("CARGO_CFG_TARGET_ARCH").unwrap() != "aarch64";
            }

            // These tests have simd operators which aren't implemented yet.
            // (currently none)
            _ => {}
        },
        _ => panic!("unrecognized strategy"),
    }

    false
}
