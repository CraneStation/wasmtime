use anyhow::{Context as _, Result};
use std::fs::File;
use std::io::Write;
use std::path::Path;
use target_lexicon::Triple;
use wasmtime::Strategy;
use wasmtime_cli::compile_to_obj;

pub fn compile_cranelift(
    wasm: &[u8],
    target: Option<Triple>,
    output: impl AsRef<Path>,
) -> Result<()> {
    let obj = compile_to_obj(
        wasm,
        target.as_ref(),
        Strategy::Cranelift,
        false,
        wasmtime::OptLevel::None,
        true,
        "",
    )?;

    let mut file = File::create(output).context("failed to create object file")?;
    file.write_all(&obj.write()?)
        .context("failed to write object file")?;

    Ok(())
}
