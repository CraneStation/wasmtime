//! CLI tool to run wast tests using the wasmtime libraries.

#![deny(
    missing_docs,
    trivial_numeric_casts,
    unused_extern_crates,
    unstable_features
)]
#![warn(unused_import_braces)]
#![cfg_attr(feature = "clippy", plugin(clippy(conf_file = "../../clippy.toml")))]
#![cfg_attr(
    feature = "cargo-clippy",
    allow(clippy::new_without_default, clippy::new_without_default_derive)
)]
#![cfg_attr(
    feature = "cargo-clippy",
    warn(
        clippy::float_arithmetic,
        clippy::mut_mut,
        clippy::nonminimal_bool,
        clippy::option_map_unwrap_or,
        clippy::option_map_unwrap_or_else,
        clippy::unicode_not_nfc,
        clippy::use_self
    )
)]

use clap::{App, Arg};
use cranelift_codegen::settings;
use cranelift_codegen::settings::Configurable;
use cranelift_native;
use file_per_thread_logger;
use pretty_env_logger;
use std::path::Path;
use std::process;
use wasmtime_jit::Compiler;
use wasmtime_wast::WastContext;

static LOG_FILENAME_PREFIX: &str = "cranelift.dbg.";

#[derive(Debug, Clone)]
struct Args {
    arg_file: Vec<String>,
    flag_debug: bool,
    flag_function: Option<String>,
    flag_optimize: bool,
}

fn main() {
    let cli = App::new("Wast test runner")
        .version("0.0.0")
        .arg(
            Arg::with_name("input")
                .help("Sets the input file to use")
                .required(true)
                .index(1),
        )
        .arg(
            Arg::with_name("debug")
                .short("d")
                .long("debug")
                .help("enable debug output on stderr/stdout"),
        )
        .arg(
            Arg::with_name("optimize")
                .short("o")
                .long("optimize")
                .help("runs optimization passes on the translated functions"),
        )
        .arg(
            Arg::with_name("invoke")
                .short("v")
                .long("invoke")
                .value_name("FN")
                .help("name of function to run")
                .takes_value(true),
        )
        .get_matches();

    let args: Args = Args {
        arg_file: clap::values_t!(cli.values_of("input"), String).unwrap(),
        flag_debug: cli.is_present("debug"),
        flag_optimize: cli.is_present("optimize"),
        flag_function: match cli.value_of("invoke") {
            Some(x) => Some(x.to_string()),
            None => None,
        },
    };

    if args.flag_debug {
        pretty_env_logger::init();
    } else {
        file_per_thread_logger::initialize(LOG_FILENAME_PREFIX);
    }

    let isa_builder = cranelift_native::builder().unwrap_or_else(|_| {
        panic!("host machine is not a supported target");
    });
    let mut flag_builder = settings::builder();

    // Enable verifier passes in debug mode.
    if cfg!(debug_assertions) {
        flag_builder.enable("enable_verifier").unwrap();
    }

    // Enable optimization if requested.
    if args.flag_optimize {
        flag_builder.set("opt_level", "best").unwrap();
    }

    let isa = isa_builder.finish(settings::Flags::new(flag_builder));
    let engine = Compiler::new(isa);
    let mut wast_context = WastContext::new(Box::new(engine));

    wast_context
        .register_spectest()
        .expect("error instantiating \"spectest\"");

    for filename in &args.arg_file {
        wast_context
            .run_file(Path::new(&filename))
            .unwrap_or_else(|e| {
                eprintln!("{}", e);
                process::exit(1)
            });
    }
}
