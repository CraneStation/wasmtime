use anyhow::{bail, Context as _, Result};
use object::write::Object;
use target_lexicon::Triple;
use wasmparser::WasmFeatures;
use wasmtime::Strategy;
use wasmtime_environ::{settings, settings::Configurable, ModuleEnvironment, Tunables};
use wasmtime_jit::{native, Compiler, ObjectMetadataFormat};

/// Creates object file from binary wasm data.
pub fn compile_to_obj(
    wasm: &[u8],
    target: Option<&Triple>,
    strategy: Strategy,
    enable_simd: bool,
    opt_level: wasmtime::OptLevel,
    debug_info: bool,
    symbol_prefix: &str,
) -> Result<Object> {
    let isa_builder = match target {
        Some(target) => native::lookup(target.clone())?,
        None => native::builder(),
    };
    let mut flag_builder = settings::builder();
    let mut features = WasmFeatures::default();

    // There are two possible traps for division, and this way
    // we get the proper one if code traps.
    flag_builder.enable("avoid_div_traps").unwrap();

    if enable_simd {
        flag_builder.enable("enable_simd").unwrap();
        features.simd = true;
    }

    match opt_level {
        wasmtime::OptLevel::None => {}
        wasmtime::OptLevel::Speed => {
            flag_builder.set("opt_level", "speed").unwrap();
        }
        wasmtime::OptLevel::SpeedAndSize => {
            flag_builder.set("opt_level", "speed_and_size").unwrap();
        }
        other => bail!("unknown optimization level {:?}", other),
    }

    let isa = isa_builder.finish(settings::Flags::new(flag_builder));

    // TODO: Expose the tunables as command-line flags.
    let mut tunables = Tunables::default();
    tunables.generate_native_debuginfo = debug_info;
    tunables.parse_wasm_debuginfo = debug_info;

    let compiler = Compiler::new(
        isa,
        match strategy {
            Strategy::Auto => wasmtime_jit::CompilationStrategy::Auto,
            Strategy::Cranelift => wasmtime_jit::CompilationStrategy::Cranelift,
            #[cfg(feature = "lightbeam")]
            Strategy::Lightbeam => wasmtime_jit::CompilationStrategy::Lightbeam,
            #[cfg(not(feature = "lightbeam"))]
            Strategy::Lightbeam => bail!("lightbeam support not enabled"),
            s => bail!("unknown compilation strategy {:?}", s),
        },
        tunables.clone(),
        features.clone(),
    );

    let environ = ModuleEnvironment::new(compiler.isa().frontend_config(), &tunables, &features);
    let (_main_module, mut translation, types) = environ
        .translate(wasm)
        .context("failed to translate module")?;
    assert_eq!(translation.len(), 1);
    let compilation = compiler.compile(
        &mut translation[0],
        &types,
        ObjectMetadataFormat::AOT {
            prefix: symbol_prefix,
            use_debug_names: true,
        },
    )?;
    Ok(compilation.obj)
}
