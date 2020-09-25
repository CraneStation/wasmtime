use crate::frame_info::GlobalFrameInfoRegistration;
use crate::runtime::{Config, Engine, EngineJitCodeRegistration, ModuleRegistration};
use crate::types::{EntityType, ExportType, ExternType, ImportType};
use anyhow::{bail, Context, Result};
use std::path::Path;
use std::sync::{Arc, Mutex};
#[cfg(feature = "cache")]
use wasmtime_cache::ModuleCacheEntry;
use wasmtime_jit::{CompilationArtifacts, CompiledModule};

/// A compiled WebAssembly module, ready to be instantiated.
///
/// A `Module` is a compiled in-memory representation of an input WebAssembly
/// binary. A `Module` is then used to create an [`Instance`](crate::Instance)
/// through an instantiation process. You cannot call functions or fetch
/// globals, for example, on a `Module` because it's purely a code
/// representation. Instead you'll need to create an
/// [`Instance`](crate::Instance) to interact with the wasm module.
///
/// Creating a `Module` currently involves compiling code, meaning that it can
/// be an expensive operation. All `Module` instances are compiled according to
/// the configuration in [`Config`], but typically they're JIT-compiled. If
/// you'd like to instantiate a module multiple times you can do so with
/// compiling the original wasm module only once with a single [`Module`]
/// instance.
///
/// The `Module` is threadsafe and safe to share accross threads.
///
/// ## Modules and `Clone`
///
/// Using `clone` on a `Module` is a cheap operation. It will not create an
/// entirely new module, but rather just a new reference to the existing module.
/// In other words it's a shallow copy, not a deep copy.
///
/// ## Examples
///
/// There are a number of ways you can create a `Module`, for example pulling
/// the bytes from a number of locations. One example is loading a module from
/// the filesystem:
///
/// ```no_run
/// # use wasmtime::*;
/// # fn main() -> anyhow::Result<()> {
/// let engine = Engine::default();
/// let module = Module::from_file(&engine, "path/to/foo.wasm")?;
/// # Ok(())
/// # }
/// ```
///
/// You can also load the wasm text format if more convenient too:
///
/// ```no_run
/// # use wasmtime::*;
/// # fn main() -> anyhow::Result<()> {
/// let engine = Engine::default();
/// // Now we're using the WebAssembly text extension: `.wat`!
/// let module = Module::from_file(&engine, "path/to/foo.wat")?;
/// # Ok(())
/// # }
/// ```
///
/// And if you've already got the bytes in-memory you can use the
/// [`Module::new`] constructor:
///
/// ```no_run
/// # use wasmtime::*;
/// # fn main() -> anyhow::Result<()> {
/// let engine = Engine::default();
/// # let wasm_bytes: Vec<u8> = Vec::new();
/// let module = Module::new(&engine, &wasm_bytes)?;
///
/// // It also works with the text format!
/// let module = Module::new(&engine, "(module (func))")?;
/// # Ok(())
/// # }
/// ```
///
/// [`Config`]: crate::Config
#[derive(Clone)]
pub struct Module {
    engine: Engine,
    compiled: Arc<CompiledModule>,
    frame_info_registration: Arc<Mutex<Option<Option<Arc<GlobalFrameInfoRegistration>>>>>,
    jit_code_registration: Arc<EngineJitCodeRegistration>,
    module_registration: Arc<Option<ModuleRegistration>>,
}

impl Module {
    /// Creates a new WebAssembly `Module` from the given in-memory `bytes`.
    ///
    /// The `bytes` provided must be in one of two formats:
    ///
    /// * It can be a [binary-encoded][binary] WebAssembly module. This
    ///   is always supported.
    /// * It may also be a [text-encoded][text] instance of the WebAssembly
    ///   text format. This is only supported when the `wat` feature of this
    ///   crate is enabled. If this is supplied then the text format will be
    ///   parsed before validation. Note that the `wat` feature is enabled by
    ///   default.
    ///
    /// The data for the wasm module must be loaded in-memory if it's present
    /// elsewhere, for example on disk. This requires that the entire binary is
    /// loaded into memory all at once, this API does not support streaming
    /// compilation of a module.
    ///
    /// The WebAssembly binary will be decoded and validated. It will also be
    /// compiled according to the configuration of the provided `engine`.
    ///
    /// # Errors
    ///
    /// This function may fail and return an error. Errors may include
    /// situations such as:
    ///
    /// * The binary provided could not be decoded because it's not a valid
    ///   WebAssembly binary
    /// * The WebAssembly binary may not validate (e.g. contains type errors)
    /// * Implementation-specific limits were exceeded with a valid binary (for
    ///   example too many locals)
    /// * The wasm binary may use features that are not enabled in the
    ///   configuration of `enging`
    /// * If the `wat` feature is enabled and the input is text, then it may be
    ///   rejected if it fails to parse.
    ///
    /// The error returned should contain full information about why module
    /// creation failed if one is returned.
    ///
    /// [binary]: https://webassembly.github.io/spec/core/binary/index.html
    /// [text]: https://webassembly.github.io/spec/core/text/index.html
    ///
    /// # Examples
    ///
    /// The `new` function can be invoked with a in-memory array of bytes:
    ///
    /// ```no_run
    /// # use wasmtime::*;
    /// # fn main() -> anyhow::Result<()> {
    /// # let engine = Engine::default();
    /// # let wasm_bytes: Vec<u8> = Vec::new();
    /// let module = Module::new(&engine, &wasm_bytes)?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// Or you can also pass in a string to be parsed as the wasm text
    /// format:
    ///
    /// ```
    /// # use wasmtime::*;
    /// # fn main() -> anyhow::Result<()> {
    /// # let engine = Engine::default();
    /// let module = Module::new(&engine, "(module (func))")?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn new(engine: &Engine, bytes: impl AsRef<[u8]>) -> Result<Module> {
        #[cfg(feature = "wat")]
        let bytes = wat::parse_bytes(bytes.as_ref())?;
        Module::from_binary(engine, bytes.as_ref())
    }

    /// Creates a new WebAssembly `Module` from the given in-memory `binary`
    /// data. The provided `name` will be used in traps/backtrace details.
    ///
    /// See [`Module::new`] for other details.
    pub fn new_with_name(engine: &Engine, bytes: impl AsRef<[u8]>, name: &str) -> Result<Module> {
        let mut module = Module::new(engine, bytes.as_ref())?;
        Arc::get_mut(&mut module.compiled)
            .unwrap()
            .module_mut()
            .expect("mutable module")
            .name = Some(name.to_string());
        Ok(module)
    }

    /// Creates a new WebAssembly `Module` from the contents of the given
    /// `file` on disk.
    ///
    /// This is a convenience function that will read the `file` provided and
    /// pass the bytes to the [`Module::new`] function. For more information
    /// see [`Module::new`]
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use wasmtime::*;
    /// # fn main() -> anyhow::Result<()> {
    /// let engine = Engine::default();
    /// let module = Module::from_file(&engine, "./path/to/foo.wasm")?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// The `.wat` text format is also supported:
    ///
    /// ```no_run
    /// # use wasmtime::*;
    /// # fn main() -> anyhow::Result<()> {
    /// # let engine = Engine::default();
    /// let module = Module::from_file(&engine, "./path/to/foo.wat")?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn from_file(engine: &Engine, file: impl AsRef<Path>) -> Result<Module> {
        #[cfg(feature = "wat")]
        let wasm = wat::parse_file(file)?;
        #[cfg(not(feature = "wat"))]
        let wasm = std::fs::read(file)?;
        Module::new(engine, &wasm)
    }

    /// Creates a new WebAssembly `Module` from the given in-memory `binary`
    /// data.
    ///
    /// This is similar to [`Module::new`] except that it requires that the
    /// `binary` input is a WebAssembly binary, the text format is not supported
    /// by this function. It's generally recommended to use [`Module::new`],
    /// but if it's required to not support the text format this function can be
    /// used instead.
    ///
    /// # Examples
    ///
    /// ```
    /// # use wasmtime::*;
    /// # fn main() -> anyhow::Result<()> {
    /// # let engine = Engine::default();
    /// let wasm = b"\0asm\x01\0\0\0";
    /// let module = Module::from_binary(&engine, wasm)?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// Note that the text format is **not** accepted by this function:
    ///
    /// ```
    /// # use wasmtime::*;
    /// # fn main() -> anyhow::Result<()> {
    /// # let engine = Engine::default();
    /// assert!(Module::from_binary(&engine, b"(module)").is_err());
    /// # Ok(())
    /// # }
    /// ```
    pub fn from_binary(engine: &Engine, binary: &[u8]) -> Result<Module> {
        Module::validate(engine, binary)?;
        // Note that the call to `from_binary_unchecked` here should be ok
        // because we previously validated the binary, meaning we're guaranteed
        // to pass a valid binary for `engine`.
        unsafe { Module::from_binary_unchecked(engine, binary) }
    }

    /// Creates a new WebAssembly `Module` from the given in-memory `binary`
    /// data, skipping validation and asserting that `binary` is a valid
    /// WebAssembly module.
    ///
    /// This function is the same as [`Module::new`] except that it skips the
    /// call to [`Module::validate`] and it does not support the text format of
    /// WebAssembly. The WebAssembly binary is not validated for
    /// correctness and it is simply assumed as valid.
    ///
    /// For more information about creation of a module and the `engine` argument
    /// see the documentation of [`Module::new`].
    ///
    /// # Unsafety
    ///
    /// This function is `unsafe` due to the unchecked assumption that the input
    /// `binary` is valid. If the `binary` is not actually a valid wasm binary it
    /// may cause invalid machine code to get generated, cause panics, etc.
    ///
    /// It is only safe to call this method if [`Module::validate`] succeeds on
    /// the same arguments passed to this function.
    ///
    /// # Errors
    ///
    /// This function may fail for many of the same reasons as [`Module::new`].
    /// While this assumes that the binary is valid it still needs to actually
    /// be somewhat valid for decoding purposes, and the basics of decoding can
    /// still fail.
    pub unsafe fn from_binary_unchecked(engine: &Engine, binary: &[u8]) -> Result<Module> {
        Module::compile(engine, binary)
    }

    /// Validates `binary` input data as a WebAssembly binary given the
    /// configuration in `engine`.
    ///
    /// This function will perform a speedy validation of the `binary` input
    /// WebAssembly module (which is in [binary form][binary], the text format
    /// is not accepted by this function) and return either `Ok` or `Err`
    /// depending on the results of validation. The `engine` argument indicates
    /// configuration for WebAssembly features, for example, which are used to
    /// indicate what should be valid and what shouldn't be.
    ///
    /// Validation automatically happens as part of [`Module::new`], but is a
    /// requirement for [`Module::from_binary_unchecked`] to be safe.
    ///
    /// # Errors
    ///
    /// If validation fails for any reason (type check error, usage of a feature
    /// that wasn't enabled, etc) then an error with a description of the
    /// validation issue will be returned.
    ///
    /// [binary]: https://webassembly.github.io/spec/core/binary/index.html
    pub fn validate(engine: &Engine, binary: &[u8]) -> Result<()> {
        engine.config().validator().validate_all(binary)?;
        Ok(())
    }

    unsafe fn compile(engine: &Engine, binary: &[u8]) -> Result<Self> {
        #[cfg(feature = "cache")]
        let artifacts = ModuleCacheEntry::new("wasmtime", engine.cache_config())
            .get_data((engine.compiler(), binary), |(compiler, binary)| {
                CompilationArtifacts::build(compiler, binary)
            })?;
        #[cfg(not(feature = "cache"))]
        let artifacts = CompilationArtifacts::build(engine.compiler(), binary)?;

        let compiled = CompiledModule::from_artifacts(
            artifacts,
            engine.compiler().isa(),
            &*engine.config().profiler,
        )?;

        let compiled = Arc::new(compiled);
        let jit_code_registration = engine.jit_code().register_jit_code(&compiled);
        let module_registration = engine.register_module(&compiled, binary);

        Ok(Module {
            engine: engine.clone(),
            compiled,
            frame_info_registration: Arc::new(Mutex::new(None)),
            jit_code_registration: Arc::new(jit_code_registration),
            module_registration: Arc::new(Some(module_registration)),
        })
    }

    /// Serialize compilation artifacts to the buffer. See also `deseriaize`.
    pub fn serialize(&self) -> Result<Vec<u8>> {
        let artifacts = (
            compiler_fingerprint(self.engine.config()),
            self.compiled.to_compilation_artifacts(),
        );

        let mut buffer = Vec::new();
        bincode::serialize_into(&mut buffer, &artifacts)?;

        Ok(buffer)
    }

    /// Deserializes and creates a module from the compilatio nartifacts.
    /// The `serialize` saves the compilation artifacts along with the host
    /// fingerprint, which consists of target, compiler flags, and wasmtime
    /// package version.
    ///
    /// The method will fail if fingerprints of current host and serialized
    /// one are different. The method does not verify the serialized artifacts
    /// for modifications or curruptions. All responsibily of signing and its
    /// verification falls on the embedder.
    pub fn deserialize(engine: &Engine, serialized: &[u8]) -> Result<Module> {
        let expected_fingerprint = compiler_fingerprint(engine.config());

        let (fingerprint, artifacts) =
            bincode::deserialize_from::<_, (u64, CompilationArtifacts)>(serialized)
                .context("Deserialize compilation artifacts")?;
        if fingerprint != expected_fingerprint {
            bail!("Incompatible compilation artifact");
        }

        let compiled = CompiledModule::from_artifacts(
            artifacts,
            engine.compiler().isa(),
            &*engine.config().profiler,
        )?;

        let compiled = Arc::new(compiled);
        let jit_code_registration = engine.jit_code().register_jit_code(&compiled);

        Ok(Module {
            engine: engine.clone(),
            compiled,
            frame_info_registration: Arc::new(Mutex::new(None)),
            jit_code_registration: Arc::new(jit_code_registration),
            module_registration: Arc::new(None),
        })
    }

    pub(crate) fn compiled_module(&self) -> &Arc<CompiledModule> {
        &self.compiled
    }

    /// Returns identifier/name that this [`Module`] has. This name
    /// is used in traps/backtrace details.
    ///
    /// Note that most LLVM/clang/Rust-produced modules do not have a name
    /// associated with them, but other wasm tooling can be used to inject or
    /// add a name.
    ///
    /// # Examples
    ///
    /// ```
    /// # use wasmtime::*;
    /// # fn main() -> anyhow::Result<()> {
    /// # let engine = Engine::default();
    /// let module = Module::new(&engine, "(module $foo)")?;
    /// assert_eq!(module.name(), Some("foo"));
    ///
    /// let module = Module::new(&engine, "(module)")?;
    /// assert_eq!(module.name(), None);
    ///
    /// let module = Module::new_with_name(&engine, "(module)", "bar")?;
    /// assert_eq!(module.name(), Some("bar"));
    /// # Ok(())
    /// # }
    /// ```
    pub fn name(&self) -> Option<&str> {
        self.compiled.module().name.as_deref()
    }

    /// Returns the list of imports that this [`Module`] has and must be
    /// satisfied.
    ///
    /// This function returns the list of imports that the wasm module has, but
    /// only the types of each import. The type of each import is used to
    /// typecheck the [`Instance::new`](crate::Instance::new) method's `imports`
    /// argument. The arguments to that function must match up 1-to-1 with the
    /// entries in the array returned here.
    ///
    /// The imports returned reflect the order of the imports in the wasm module
    /// itself, and note that no form of deduplication happens.
    ///
    /// # Examples
    ///
    /// Modules with no imports return an empty list here:
    ///
    /// ```
    /// # use wasmtime::*;
    /// # fn main() -> anyhow::Result<()> {
    /// # let engine = Engine::default();
    /// let module = Module::new(&engine, "(module)")?;
    /// assert_eq!(module.imports().len(), 0);
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// and modules with imports will have a non-empty list:
    ///
    /// ```
    /// # use wasmtime::*;
    /// # fn main() -> anyhow::Result<()> {
    /// # let engine = Engine::default();
    /// let wat = r#"
    ///     (module
    ///         (import "host" "foo" (func))
    ///     )
    /// "#;
    /// let module = Module::new(&engine, wat)?;
    /// assert_eq!(module.imports().len(), 1);
    /// let import = module.imports().next().unwrap();
    /// assert_eq!(import.module(), "host");
    /// assert_eq!(import.name(), "foo");
    /// match import.ty() {
    ///     ExternType::Func(_) => { /* ... */ }
    ///     _ => panic!("unexpected import type!"),
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub fn imports<'module>(
        &'module self,
    ) -> impl ExactSizeIterator<Item = ImportType<'module>> + 'module {
        let module = self.compiled.module();
        module
            .imports
            .iter()
            .map(move |(module_name, name, entity_index)| {
                let r#type = EntityType::new(entity_index, module);
                ImportType::new(module_name, name, r#type)
            })
    }

    /// Returns the list of exports that this [`Module`] has and will be
    /// available after instantiation.
    ///
    /// This function will return the type of each item that will be returned
    /// from [`Instance::exports`](crate::Instance::exports). Each entry in this
    /// list corresponds 1-to-1 with that list, and the entries here will
    /// indicate the name of the export along with the type of the export.
    ///
    /// # Examples
    ///
    /// Modules might not have any exports:
    ///
    /// ```
    /// # use wasmtime::*;
    /// # fn main() -> anyhow::Result<()> {
    /// # let engine = Engine::default();
    /// let module = Module::new(&engine, "(module)")?;
    /// assert!(module.exports().next().is_none());
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// When the exports are not empty, you can inspect each export:
    ///
    /// ```
    /// # use wasmtime::*;
    /// # fn main() -> anyhow::Result<()> {
    /// # let engine = Engine::default();
    /// let wat = r#"
    ///     (module
    ///         (func (export "foo"))
    ///         (memory (export "memory") 1)
    ///     )
    /// "#;
    /// let module = Module::new(&engine, wat)?;
    /// assert_eq!(module.exports().len(), 2);
    ///
    /// let mut exports = module.exports();
    /// let foo = exports.next().unwrap();
    /// assert_eq!(foo.name(), "foo");
    /// match foo.ty() {
    ///     ExternType::Func(_) => { /* ... */ }
    ///     _ => panic!("unexpected export type!"),
    /// }
    ///
    /// let memory = exports.next().unwrap();
    /// assert_eq!(memory.name(), "memory");
    /// match memory.ty() {
    ///     ExternType::Memory(_) => { /* ... */ }
    ///     _ => panic!("unexpected export type!"),
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub fn exports<'module>(
        &'module self,
    ) -> impl ExactSizeIterator<Item = ExportType<'module>> + 'module {
        let module = self.compiled.module();
        module.exports.iter().map(move |(name, entity_index)| {
            let r#type = EntityType::new(entity_index, module);
            ExportType::new(name, r#type)
        })
    }

    /// Looks up an export in this [`Module`] by name.
    ///
    /// This function will return the type of an export with the given name.
    ///
    /// # Examples
    ///
    /// There may be no export with that name:
    ///
    /// ```
    /// # use wasmtime::*;
    /// # fn main() -> anyhow::Result<()> {
    /// # let engine = Engine::default();
    /// let module = Module::new(&engine, "(module)")?;
    /// assert!(module.get_export("foo").is_none());
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// When there is an export with that name, it is returned:
    ///
    /// ```
    /// # use wasmtime::*;
    /// # fn main() -> anyhow::Result<()> {
    /// # let engine = Engine::default();
    /// let wat = r#"
    ///     (module
    ///         (func (export "foo"))
    ///         (memory (export "memory") 1)
    ///     )
    /// "#;
    /// let module = Module::new(&engine, wat)?;
    /// let foo = module.get_export("foo");
    /// assert!(foo.is_some());
    ///
    /// let foo = foo.unwrap();
    /// match foo {
    ///     ExternType::Func(_) => { /* ... */ }
    ///     _ => panic!("unexpected export type!"),
    /// }
    ///
    /// # Ok(())
    /// # }
    /// ```
    pub fn get_export<'module>(&'module self, name: &'module str) -> Option<ExternType> {
        let module = self.compiled.module();
        let entity_index = module.exports.get(name)?;
        Some(EntityType::new(entity_index, module).extern_type())
    }

    /// Returns the [`Engine`] that this [`Module`] was compiled by.
    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    /// Register this module's stack frame information into the global scope.
    ///
    /// This is required to ensure that any traps can be properly symbolicated.
    pub(crate) fn register_frame_info(&self) -> Option<Arc<GlobalFrameInfoRegistration>> {
        let mut info = self.frame_info_registration.lock().unwrap();
        if let Some(info) = &*info {
            return info.clone();
        }
        let ret = super::frame_info::register(&self.compiled).map(Arc::new);
        *info = Some(ret.clone());
        return ret;
    }

    /// Sets a breakpoint in the module. The `offset` is wasm module bytecode
    /// offset for the target instruction.
    pub fn set_breakpoint(&self, offset: usize) {
        let reg = (*self.module_registration).as_ref().unwrap();
        self.engine.add_breakpoints(reg.id(), offset as u64);
    }
}

fn compiler_fingerprint(config: &Config) -> u64 {
    use std::hash::Hasher;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    config.compiler_fingerprint(&mut hasher);
    hasher.finish()
}

fn _assert_send_sync() {
    fn _assert<T: Send + Sync>() {}
    _assert::<Module>();
}
