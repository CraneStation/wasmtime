#![allow(dead_code)]

use anyhow::{bail, format_err, Result};
use filecheck::{CheckerBuilder, NO_VARIABLES};
use std::env;
use std::io::Write;
use std::process::Command;
use tempfile::NamedTempFile;

fn lldb_with_script(args: &[&str], script: &str) -> Result<String> {
    let lldb_path = env::var("LLDB").unwrap_or("lldb".to_string());
    let mut cmd = Command::new(&lldb_path);

    cmd.arg("--batch");
    if cfg!(target_os = "macos") {
        cmd.args(&["-o", "settings set plugin.jit-loader.gdb.enable on"]);
    }
    let mut script_file = NamedTempFile::new()?;
    script_file.write(script.as_bytes())?;
    let script_path = script_file.path().to_str().unwrap();
    cmd.args(&["-s", &script_path]);

    let mut me = std::env::current_exe().expect("current_exe specified");
    me.pop(); // chop off the file name
    me.pop(); // chop off `deps`
    me.push("wasmtime");
    cmd.arg(me);

    cmd.arg("--");
    cmd.args(args);

    let output = cmd.output().expect("success");
    if !output.status.success() {
        bail!(
            "failed to execute {:?}: {}",
            cmd,
            String::from_utf8_lossy(&output.stderr),
        );
    }
    Ok(String::from_utf8(output.stdout)?)
}

fn check_lldb_output(output: &str, directives: &str) -> Result<()> {
    let mut builder = CheckerBuilder::new();
    builder
        .text(directives)
        .map_err(|e| format_err!("unable to build checker: {:?}", e))?;
    let checker = builder.finish();
    let check = checker
        .explain(output, NO_VARIABLES)
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
pub fn test_debug_dwarf_lldb() -> Result<()> {
    let output = lldb_with_script(
        &[
            "-g",
            "tests/all/debug/testsuite/fib-wasm.wasm",
            "--invoke",
            "fib",
            "3",
        ],
        r#"b fib
r
fr v
c"#,
    )?;

    check_lldb_output(
        &output,
        r#"
check: Breakpoint 1: no locations (pending)
check: Unable to resolve breakpoint to any actual locations.
check: 1 location added to breakpoint 1
check: stop reason = breakpoint 1.1
check: frame #0
sameln: JIT
sameln: fib(n=3)
check: n = 3
check: a = 0
check: resuming
check: exited with status
"#,
    )?;
    Ok(())
}

#[test]
#[ignore]
#[cfg(all(
    any(target_os = "linux", target_os = "macos"),
    target_pointer_width = "64"
))]
pub fn test_debug_dwarf5_lldb() -> Result<()> {
    let output = lldb_with_script(
        &[
            "-g",
            "tests/all/debug/testsuite/fib-wasm-dwarf5.wasm",
            "--invoke",
            "fib",
            "3",
        ],
        r#"b fib
r
fr v
c"#,
    )?;

    check_lldb_output(
        &output,
        r#"
check: Breakpoint 1: no locations (pending)
check: Unable to resolve breakpoint to any actual locations.
check: 1 location added to breakpoint 1
check: stop reason = breakpoint 1.1
check: frame #0
sameln: JIT
sameln: fib(n=3)
check: n = 3
check: a = 0
check: resuming
check: exited with status
"#,
    )?;
    Ok(())
}

#[test]
#[ignore]
#[cfg(all(
    any(target_os = "linux", target_os = "macos"),
    target_pointer_width = "64",
    // Ignore test on new backend. The value this is looking for is
    // not available at the point that the breakpoint is set when
    // compiled by the new backend.
    feature = "old-x86-backend", // TODO
))]
pub fn test_debug_dwarf_ptr() -> Result<()> {
    let output = lldb_with_script(
        &[
            "-g",
            "--opt-level",
            "0",
            "tests/all/debug/testsuite/reverse-str.wasm",
        ],
        r#"b reverse-str.c:9
r
p __vmctx->set(),&*s
c"#,
    )?;

    check_lldb_output(
        &output,
        r#"
check: Breakpoint 1: no locations (pending)
check: stop reason = breakpoint 1.1
check: frame #0
sameln: reverse(s=(__ptr =
check: "Hello, world."
check: resuming
"#,
    )?;
    Ok(())
}

#[test]
#[ignore]
#[cfg(all(
    any(target_os = "linux", target_os = "macos"),
    target_pointer_width = "64"
))]
pub fn test_debug_dwarf_ref() -> Result<()> {
    let output = lldb_with_script(
        &[
            "-g",
            "--opt-level",
            "0",
            "tests/all/debug/testsuite/fraction-norm.wasm",
        ],
        r#"b fraction-norm.cc:26
r
p __vmctx->set(),n->denominator
c"#,
    )?;

    check_lldb_output(
        &output,
        r#"
check: Breakpoint 1: no locations (pending)
check: stop reason = breakpoint 1.1
check: frame #0
sameln: norm(n=(__ptr =
check: = 27
check: resuming
"#,
    )?;
    Ok(())
}
