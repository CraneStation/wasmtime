//! The module that implements the `wasmtime wasm2obj` command.

use crate::obj::compile_to_obj;
use crate::{parse_target, pick_compilation_strategy, CommonOptions};
use anyhow::{Context as _, Result};
use std::{
    fs::File,
    io::Write,
    path::{Path, PathBuf},
};
use structopt::{clap::AppSettings, StructOpt};
use target_lexicon::Triple;

lazy_static::lazy_static! {
    static ref AFTER_HELP: String = {
        format!(
            "The translation is dependent on the environment chosen.\n\
            The default is a dummy environment that produces placeholder values.\n\
            \n\
            {}",
            crate::WASM_FEATURES.as_str()
        )
    };
}

/// Translates a WebAssembly module to native object file
#[derive(StructOpt)]
#[structopt(
    name = "wasm2obj",
    version = env!("CARGO_PKG_VERSION"),
    setting = AppSettings::ColoredHelp,
    after_help = AFTER_HELP.as_str(),
)]
pub struct WasmToObjCommand {
    #[structopt(flatten)]
    common: CommonOptions,

    /// The path of the WebAssembly module to translate
    #[structopt(index = 1, value_name = "MODULE_PATH", parse(from_os_str))]
    module: PathBuf,

    /// The path of the output object file
    #[structopt(index = 2, value_name = "OUTPUT_PATH")]
    output: String,

    /// The target triple; default is the host triple
    #[structopt(long, value_name = "TARGET", parse(try_from_str = parse_target))]
    target: Option<Triple>,

    /// The symbol prefix.
    #[structopt(long, value_name = "SYMBOL_PREFIX")]
    symbol_prefix: Option<String>,
}

impl WasmToObjCommand {
    /// Executes the command.
    pub fn execute(self) -> Result<()> {
        self.common.init_logging();

        let strategy = pick_compilation_strategy(self.common.cranelift, self.common.lightbeam)?;

        let data = wat::parse_file(&self.module).context("failed to parse module")?;

        let obj = compile_to_obj(
            &data,
            self.target.as_ref(),
            strategy,
            self.common.enable_simd,
            self.common.opt_level(),
            self.common.debug_info,
            self.symbol_prefix.as_ref().unwrap_or(&"".to_string()),
        )?;

        let mut file =
            File::create(Path::new(&self.output)).context("failed to create object file")?;
        file.write_all(&obj.write()?)
            .context("failed to write object file")?;

        Ok(())
    }
}
