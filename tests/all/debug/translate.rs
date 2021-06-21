use super::dump::{get_dwarfdump, DwarfDumpSection};
use super::obj::compile_cranelift;
use anyhow::{format_err, Result};
use filecheck::{CheckerBuilder, NO_VARIABLES};
use std::fs::read;
use tempfile::NamedTempFile;

#[allow(dead_code)]
fn check_wasm(wasm_path: &str, directives: &str) -> Result<()> {
    let wasm = read(wasm_path)?;
    let obj_file = NamedTempFile::new()?;
    let obj_path = obj_file.path().to_str().unwrap();
    compile_cranelift(&wasm, None, obj_path)?;
    let dump = get_dwarfdump(obj_path, DwarfDumpSection::DebugInfo)?;
    let mut builder = CheckerBuilder::new();
    builder
        .text(directives)
        .map_err(|e| format_err!("unable to build checker: {:?}", e))?;
    let checker = builder.finish();
    let check = checker
        .explain(&dump, NO_VARIABLES)
        .map_err(|e| format_err!("{:?}", e))?;
    assert!(check.0, "didn't pass check {}", check.1);
    Ok(())
}

#[allow(dead_code)]
fn check_line_program(wasm_path: &str, directives: &str) -> Result<()> {
    let wasm = read(wasm_path)?;
    let obj_file = NamedTempFile::new()?;
    let obj_path = obj_file.path().to_str().unwrap();
    compile_cranelift(&wasm, None, obj_path)?;
    let dump = get_dwarfdump(obj_path, DwarfDumpSection::DebugLine)?;
    let mut builder = CheckerBuilder::new();
    builder
        .text(directives)
        .map_err(|e| format_err!("unable to build checker: {:?}", e))?;
    let checker = builder.finish();
    let check = checker
        .explain(&dump, NO_VARIABLES)
        .map_err(|e| format_err!("{:?}", e))?;
    assert!(check.0, "didn't pass check {}", check.1);
    Ok(())
}

#[test]
#[ignore]
#[cfg(all(
    any(target_os = "linux", target_os = "macos"),
    target_pointer_width = "64"
))]
fn test_debug_dwarf_translate() -> Result<()> {
    check_wasm(
        "tests/all/debug/testsuite/fib-wasm.wasm",
        r##"
check: DW_TAG_compile_unit
# We have "fib" function
check: DW_TAG_subprogram
check:      DW_AT_name	("fib")
# Accepts one parameter
check:      DW_TAG_formal_parameter
check:        DW_AT_name	("n")
check:        DW_AT_decl_line	(8)
# Has four locals: t, a, b, i
check:      DW_TAG_variable
check:        DW_AT_name	("t")
check:        DW_AT_decl_line	(9)
check:      DW_TAG_variable
check:        DW_AT_name	("a")
check:      DW_TAG_variable
check:        DW_AT_name	("b")
check:      DW_TAG_variable
check:        DW_AT_name	("i")
check:        DW_AT_decl_line	(10)
    "##,
    )
}

#[test]
#[ignore]
#[cfg(all(
    any(target_os = "linux", target_os = "macos"),
    target_pointer_width = "64"
))]
fn test_debug_dwarf5_translate() -> Result<()> {
    check_wasm(
        "tests/all/debug/testsuite/fib-wasm-dwarf5.wasm",
        r##"
check: DW_TAG_compile_unit
# We have "fib" function
check: DW_TAG_subprogram
check:      DW_AT_name	("fib")
# Accepts one parameter
check:      DW_TAG_formal_parameter
check:        DW_AT_name	("n")
check:        DW_AT_decl_line	(8)
# Has four locals: t, a, b, i
check:      DW_TAG_variable
check:        DW_AT_name	("t")
check:        DW_AT_decl_line	(9)
check:      DW_TAG_variable
check:        DW_AT_name	("a")
check:      DW_TAG_variable
check:        DW_AT_name	("b")
check:      DW_TAG_variable
check:        DW_AT_name	("i")
check:        DW_AT_decl_line	(10)
    "##,
    )
}

#[test]
#[ignore]
#[cfg(all(
    any(target_os = "linux", target_os = "macos"),
    target_arch = "x86_64",
    target_pointer_width = "64",
    // Ignore test on new backend. This is a specific test with hardcoded
    // offsets and the new backend compiles the return basic-block at a different
    // offset, causing mismatches.
    feature = "old-x86-backend", // TODO
))]
fn test_debug_dwarf5_translate_lines() -> Result<()> {
    check_line_program(
        "tests/all/debug/testsuite/fib-wasm-dwarf5.wasm",
        r##"
check:   Address            Line   Column File   ISA Discriminator Flags
check: 0x000000000000013c     15      3      1   0             0
# The important point is that the following offset must be _after_ the `ret` instruction.
# FIXME: this +1 increment might vary on other archs.
nextln: 0x000000000000013d     15      3      1   0             0  end_sequence
    "##,
    )
}
