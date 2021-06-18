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
            test_directory_module(out, "tests/misc_testsuite/module-linking", strategy)?;
            test_directory_module(out, "tests/misc_testsuite/threads", strategy)?;
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
        write_testsuite_tests(out, entry, testsuite, strategy, false)?;
        write_testsuite_tests(out, entry, testsuite, strategy, true)?;
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
    pooling: bool,
) -> anyhow::Result<()> {
    let path = path.as_ref();
    let testname = extract_name(path);

    writeln!(out, "#[test]")?;
    if x64_should_panic(testsuite, &testname, strategy) {
        writeln!(out, r#"#[should_panic]"#)?;
    } else if ignore(testsuite, &testname, strategy) {
        writeln!(out, "#[ignore]")?;
    } else if pooling {
        // Ignore on aarch64 due to using QEMU for running tests (limited memory)
        writeln!(out, r#"#[cfg_attr(target_arch = "aarch64", ignore)]"#)?;
    }

    writeln!(
        out,
        "fn r#{}{}() {{",
        &testname,
        if pooling { "_pooling" } else { "" }
    )?;
    writeln!(out, "    let _ = env_logger::try_init();")?;
    writeln!(
        out,
        "    crate::wast::run_wast(r#\"{}\"#, crate::wast::Strategy::{}, {}).unwrap();",
        path.display(),
        strategy,
        pooling
    )?;
    writeln!(out, "}}")?;
    writeln!(out)?;
    Ok(())
}

/// For x64 backend features that are not supported yet, mark tests as panicking, so
/// they stop "passing" once the features are properly implemented.
fn x64_should_panic(testsuite: &str, testname: &str, strategy: &str) -> bool {
    if !platform_is_x64() || strategy != "Cranelift" {
        return false;
    }

    match (testsuite, testname) {
        ("simd", "simd_conversions") => return true, // unknown operator or unexpected token: tests/spec_testsuite/proposals/simd/simd_conversions.wast:724:6
        ("simd", "simd_i16x8_extadd_pairwise_i8x16") => return true,
        ("simd", "simd_i16x8_extmul_i8x16") => return true,
        ("simd", "simd_i16x8_q15mulr_sat_s") => return true,
        ("simd", "simd_i32x4_extadd_pairwise_i16x8") => return true,
        ("simd", "simd_i32x4_extmul_i16x8") => return true,
        ("simd", "simd_i32x4_trunc_sat_f64x2") => return true,
        ("simd", "simd_i64x2_extmul_i32x4") => return true,
        ("simd", "simd_int_to_int_extend") => return true,
        ("simd", _) => return false,
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
            // No simd support yet for s390x.
            ("simd", _) if platform_is_s390x() => return true,

            // These are new instructions that are not really implemented in any backend.
            ("simd", "simd_i8x16_arith2")
            | ("simd", "simd_conversions")
            | ("simd", "simd_i16x8_extadd_pairwise_i8x16")
            | ("simd", "simd_i16x8_extmul_i8x16")
            | ("simd", "simd_i16x8_q15mulr_sat_s")
            | ("simd", "simd_i32x4_extadd_pairwise_i16x8")
            | ("simd", "simd_i32x4_extmul_i16x8")
            | ("simd", "simd_i32x4_trunc_sat_f64x2")
            | ("simd", "simd_i64x2_extmul_i32x4") => return true,

            _ => {}
        },
        _ => panic!("unrecognized strategy"),
    }

    false
}

fn platform_is_x64() -> bool {
    env::var("CARGO_CFG_TARGET_ARCH").unwrap() == "x86_64"
}

fn platform_is_s390x() -> bool {
    env::var("CARGO_CFG_TARGET_ARCH").unwrap() == "s390x"
}
