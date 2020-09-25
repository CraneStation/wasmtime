use crate::debugger::{
    DebuggerAgent, DebuggerJitCodeRegistration, DebuggerModule, NullDebuggerAgent,
};
use crate::externals::MemoryCreator;
use crate::trampoline::{MemoryCreatorProxy, StoreInstanceHandle};
use crate::Module;
use anyhow::{bail, Result};
use std::cell::RefCell;
use std::cmp;
use std::collections::BTreeSet;
use std::convert::TryFrom;
use std::fmt;
use std::hash::{Hash, Hasher};
#[cfg(feature = "cache")]
use std::path::Path;
use std::rc::{Rc, Weak};
use std::sync::{Arc, Mutex, Weak as SyncWeak};
use target_lexicon::Triple;
use wasmparser::Validator;
#[cfg(feature = "cache")]
use wasmtime_cache::CacheConfig;
use wasmtime_environ::settings::{self, Configurable, SetError};
use wasmtime_environ::{ir, isa, isa::TargetIsa, wasm, Tunables};
use wasmtime_jit::{native, CompilationStrategy, CompiledModule, Compiler};
use wasmtime_profiling::{JitDumpAgent, NullProfilerAgent, ProfilingAgent, VTuneAgent};
use wasmtime_runtime::debugger::{
    BreakpointData, DebuggerContext, DebuggerContextData, DebuggerPauseKind, DebuggerResumeAction,
    PatchableCode,
};
use wasmtime_runtime::{
    debug_builtins, InstanceHandle, RuntimeMemoryCreator, SignalHandler, SignatureRegistry,
    StackMapRegistry, VMExternRef, VMExternRefActivationsTable, VMInterrupts,
    VMSharedSignatureIndex,
};

// Runtime Environment

// Configuration

/// Global configuration options used to create an [`Engine`] and customize its
/// behavior.
///
/// This structure exposed a builder-like interface and is primarily consumed by
/// [`Engine::new()`]
#[derive(Clone)]
pub struct Config {
    pub(crate) flags: settings::Builder,
    pub(crate) isa_flags: isa::Builder,
    pub(crate) tunables: Tunables,
    pub(crate) strategy: CompilationStrategy,
    #[cfg(feature = "cache")]
    pub(crate) cache_config: CacheConfig,
    pub(crate) debugger: Arc<Mutex<dyn DebuggerAgent>>,
    pub(crate) profiler: Arc<dyn ProfilingAgent>,
    pub(crate) memory_creator: Option<MemoryCreatorProxy>,
    pub(crate) max_wasm_stack: usize,
    wasm_threads: bool,
    wasm_reference_types: bool,
    pub(crate) wasm_bulk_memory: bool,
    wasm_simd: bool,
    wasm_multi_value: bool,
}

impl Config {
    /// Creates a new configuration object with the default configuration
    /// specified.
    pub fn new() -> Config {
        let mut tunables = Tunables::default();
        if cfg!(windows) {
            // For now, use a smaller footprint on Windows so that we don't
            // don't outstrip the paging file.
            tunables.static_memory_bound = cmp::min(tunables.static_memory_bound, 0x100);
            tunables.static_memory_offset_guard_size =
                cmp::min(tunables.static_memory_offset_guard_size, 0x10000);
        }

        let mut flags = settings::builder();

        // There are two possible traps for division, and this way
        // we get the proper one if code traps.
        flags
            .enable("avoid_div_traps")
            .expect("should be valid flag");

        // Invert cranelift's default-on verification to instead default off.
        flags
            .set("enable_verifier", "false")
            .expect("should be valid flag");

        // Turn on cranelift speed optimizations by default
        flags
            .set("opt_level", "speed")
            .expect("should be valid flag");

        // We don't use probestack as a stack limit mechanism
        flags
            .set("enable_probestack", "false")
            .expect("should be valid flag");

        Config {
            tunables,
            flags,
            isa_flags: native::builder(),
            strategy: CompilationStrategy::Auto,
            #[cfg(feature = "cache")]
            cache_config: CacheConfig::new_cache_disabled(),
            debugger: Arc::new(Mutex::new(NullDebuggerAgent)),
            profiler: Arc::new(NullProfilerAgent),
            memory_creator: None,
            max_wasm_stack: 1 << 20,
            wasm_threads: false,
            wasm_reference_types: cfg!(target_arch = "x86_64"),
            wasm_bulk_memory: true,
            wasm_simd: false,
            wasm_multi_value: true,
        }
    }

    /// Configures whether DWARF debug information will be emitted during
    /// compilation.
    ///
    /// By default this option is `false`.
    pub fn debug_info(&mut self, enable: bool) -> &mut Self {
        self.tunables.debug_info = enable;
        self
    }

    /// Configures whether functions and loops will be interruptable via the
    /// [`Store::interrupt_handle`] method.
    ///
    /// For more information see the documentation on
    /// [`Store::interrupt_handle`].
    ///
    /// By default this option is `false`.
    pub fn interruptable(&mut self, enable: bool) -> &mut Self {
        self.tunables.interruptable = enable;
        self
    }

    /// Configures the maximum amount of native stack space available to
    /// executing WebAssembly code.
    ///
    /// WebAssembly code currently executes on the native call stack for its own
    /// call frames. WebAssembly, however, also has well-defined semantics on
    /// stack overflow. This is intended to be a knob which can help configure
    /// how much native stack space a wasm module is allowed to consume. Note
    /// that the number here is not super-precise, but rather wasm will take at
    /// most "pretty close to this much" stack space.
    ///
    /// If a wasm call (or series of nested wasm calls) take more stack space
    /// than the `size` specified then a stack overflow trap will be raised.
    ///
    /// By default this option is 1 MB.
    pub fn max_wasm_stack(&mut self, size: usize) -> &mut Self {
        self.max_wasm_stack = size;
        self
    }

    /// Configures whether the WebAssembly threads proposal will be enabled for
    /// compilation.
    ///
    /// The [WebAssembly threads proposal][threads] is not currently fully
    /// standardized and is undergoing development. Additionally the support in
    /// wasmtime itself is still being worked on. Support for this feature can
    /// be enabled through this method for appropriate wasm modules.
    ///
    /// This feature gates items such as shared memories and atomic
    /// instructions. Note that enabling the threads feature will
    /// also enable the bulk memory feature.
    ///
    /// This is `false` by default.
    ///
    /// > **Note**: Wasmtime does not implement everything for the wasm threads
    /// > spec at this time, so bugs, panics, and possibly segfaults should be
    /// > expected. This should not be enabled in a production setting right
    /// > now.
    ///
    /// [threads]: https://github.com/webassembly/threads
    pub fn wasm_threads(&mut self, enable: bool) -> &mut Self {
        self.wasm_threads = enable;
        // The threads proposal depends on the bulk memory proposal
        if enable {
            self.wasm_bulk_memory(true);
        }
        self
    }

    /// Configures whether the [WebAssembly reference types proposal][proposal]
    /// will be enabled for compilation.
    ///
    /// This feature gates items such as the `externref` and `funcref` types as
    /// well as allowing a module to define multiple tables.
    ///
    /// Note that enabling the reference types feature will also enable the bulk
    /// memory feature.
    ///
    /// This is `true` by default on x86-64, and `false` by default on other
    /// architectures.
    ///
    /// [proposal]: https://github.com/webassembly/reference-types
    pub fn wasm_reference_types(&mut self, enable: bool) -> &mut Self {
        self.wasm_reference_types = enable;

        self.flags
            .set("enable_safepoints", if enable { "true" } else { "false" })
            .unwrap();

        // The reference types proposal depends on the bulk memory proposal.
        if enable {
            self.wasm_bulk_memory(true);
        }

        self
    }

    /// Configures whether the WebAssembly SIMD proposal will be
    /// enabled for compilation.
    ///
    /// The [WebAssembly SIMD proposal][proposal] is not currently
    /// fully standardized and is undergoing development. Additionally the
    /// support in wasmtime itself is still being worked on. Support for this
    /// feature can be enabled through this method for appropriate wasm
    /// modules.
    ///
    /// This feature gates items such as the `v128` type and all of its
    /// operators being in a module.
    ///
    /// This is `false` by default.
    ///
    /// > **Note**: Wasmtime does not implement everything for the wasm simd
    /// > spec at this time, so bugs, panics, and possibly segfaults should be
    /// > expected. This should not be enabled in a production setting right
    /// > now.
    ///
    /// [proposal]: https://github.com/webassembly/simd
    pub fn wasm_simd(&mut self, enable: bool) -> &mut Self {
        self.wasm_simd = enable;
        let val = if enable { "true" } else { "false" };
        self.flags
            .set("enable_simd", val)
            .expect("should be valid flag");
        self
    }

    /// Configures whether the [WebAssembly bulk memory operations
    /// proposal][proposal] will be enabled for compilation.
    ///
    /// This feature gates items such as the `memory.copy` instruction, passive
    /// data/table segments, etc, being in a module.
    ///
    /// This is `true` by default.
    ///
    /// [proposal]: https://github.com/webassembly/bulk-memory-operations
    pub fn wasm_bulk_memory(&mut self, enable: bool) -> &mut Self {
        self.wasm_bulk_memory = enable;
        self
    }

    /// Configures whether the WebAssembly multi-value proposal will
    /// be enabled for compilation.
    ///
    /// This feature gates functions and blocks returning multiple values in a
    /// module, for example.
    ///
    /// This is `true` by default.
    ///
    /// [proposal]: https://github.com/webassembly/multi-value
    pub fn wasm_multi_value(&mut self, enable: bool) -> &mut Self {
        self.wasm_multi_value = enable;
        self
    }

    /// Configures which compilation strategy will be used for wasm modules.
    ///
    /// This method can be used to configure which compiler is used for wasm
    /// modules, and for more documentation consult the [`Strategy`] enumeration
    /// and its documentation.
    ///
    /// The default value for this is `Strategy::Auto`.
    ///
    /// # Errors
    ///
    /// Some compilation strategies require compile-time options of `wasmtime`
    /// itself to be set, but if they're not set and the strategy is specified
    /// here then an error will be returned.
    pub fn strategy(&mut self, strategy: Strategy) -> Result<&mut Self> {
        self.strategy = match strategy {
            Strategy::Auto => CompilationStrategy::Auto,
            Strategy::Cranelift => CompilationStrategy::Cranelift,
            #[cfg(feature = "lightbeam")]
            Strategy::Lightbeam => CompilationStrategy::Lightbeam,
            #[cfg(not(feature = "lightbeam"))]
            Strategy::Lightbeam => {
                anyhow::bail!("lightbeam compilation strategy wasn't enabled at compile time");
            }
        };
        Ok(self)
    }

    /// Creates a default profiler based on the profiling strategy choosen
    ///
    /// Profiler creation calls the type's default initializer where the purpose is
    /// really just to put in place the type used for profiling.
    pub fn profiler(&mut self, profile: ProfilingStrategy) -> Result<&mut Self> {
        self.profiler = match profile {
            ProfilingStrategy::JitDump => Arc::new(JitDumpAgent::new()?) as Arc<dyn ProfilingAgent>,
            ProfilingStrategy::VTune => Arc::new(VTuneAgent::new()?) as Arc<dyn ProfilingAgent>,
            ProfilingStrategy::None => Arc::new(NullProfilerAgent),
        };
        Ok(self)
    }

    /// Sets debugger.
    pub fn debugger(&mut self, debugger: impl DebuggerAgent + 'static) -> &mut Self {
        self.debugger = Arc::new(Mutex::new(debugger));
        self
    }

    /// Configures whether the debug verifier of Cranelift is enabled or not.
    ///
    /// When Cranelift is used as a code generation backend this will configure
    /// it to have the `enable_verifier` flag which will enable a number of debug
    /// checks inside of Cranelift. This is largely only useful for the
    /// developers of wasmtime itself.
    ///
    /// The default value for this is `false`
    pub fn cranelift_debug_verifier(&mut self, enable: bool) -> &mut Self {
        let val = if enable { "true" } else { "false" };
        self.flags
            .set("enable_verifier", val)
            .expect("should be valid flag");
        self
    }

    /// Configures the Cranelift code generator optimization level.
    ///
    /// When the Cranelift code generator is used you can configure the
    /// optimization level used for generated code in a few various ways. For
    /// more information see the documentation of [`OptLevel`].
    ///
    /// The default value for this is `OptLevel::None`.
    pub fn cranelift_opt_level(&mut self, level: OptLevel) -> &mut Self {
        let val = match level {
            OptLevel::None => "none",
            OptLevel::Speed => "speed",
            OptLevel::SpeedAndSize => "speed_and_size",
        };
        self.flags
            .set("opt_level", val)
            .expect("should be valid flag");
        self
    }

    /// Configures whether Cranelift should perform a NaN-canonicalization pass.
    ///
    /// When Cranelift is used as a code generation backend this will configure
    /// it to replace NaNs with a single canonical value. This is useful for users
    /// requiring entirely deterministic WebAssembly computation.
    /// This is not required by the WebAssembly spec, so it is not enabled by default.
    ///
    /// The default value for this is `false`
    pub fn cranelift_nan_canonicalization(&mut self, enable: bool) -> &mut Self {
        let val = if enable { "true" } else { "false" };
        self.flags
            .set("enable_nan_canonicalization", val)
            .expect("should be valid flag");
        self
    }

    /// Allows settings another Cranelift flag defined by a flag name and value. This allows
    /// fine-tuning of Cranelift settings.
    ///
    /// Since Cranelift flags may be unstable, this method should not be considered to be stable
    /// either; other `Config` functions should be preferred for stability.
    ///
    /// Note that this is marked as unsafe, because setting the wrong flag might break invariants,
    /// resulting in execution hazards.
    ///
    /// # Errors
    ///
    /// This method can fail if the flag's name does not exist, or the value is not appropriate for
    /// the flag type.
    pub unsafe fn cranelift_other_flag(&mut self, name: &str, value: &str) -> Result<&mut Self> {
        if let Err(err) = self.flags.set(name, value) {
            match err {
                SetError::BadName(_) => {
                    // Try the target-specific flags.
                    self.isa_flags.set(name, value)?;
                }
                _ => bail!(err),
            }
        }
        Ok(self)
    }

    /// Loads cache configuration specified at `path`.
    ///
    /// This method will read the file specified by `path` on the filesystem and
    /// attempt to load cache configuration from it. This method can also fail
    /// due to I/O errors, misconfiguration, syntax errors, etc. For expected
    /// syntax in the configuration file see the [documentation online][docs].
    ///
    /// By default cache configuration is not enabled or loaded.
    ///
    /// This method is only available when the `cache` feature of this crate is
    /// enabled.
    ///
    /// # Errors
    ///
    /// This method can fail due to any error that happens when loading the file
    /// pointed to by `path` and attempting to load the cache configuration.
    ///
    /// [docs]: https://bytecodealliance.github.io/wasmtime/cli-cache.html
    #[cfg(feature = "cache")]
    pub fn cache_config_load(&mut self, path: impl AsRef<Path>) -> Result<&mut Self> {
        self.cache_config = CacheConfig::from_file(Some(path.as_ref()))?;
        Ok(self)
    }

    /// Loads cache configuration from the system default path.
    ///
    /// This commit is the same as [`Config::cache_config_load`] except that it
    /// does not take a path argument and instead loads the default
    /// configuration present on the system. This is located, for example, on
    /// Unix at `$HOME/.config/wasmtime/config.toml` and is typically created
    /// with the `wasmtime config new` command.
    ///
    /// By default cache configuration is not enabled or loaded.
    ///
    /// This method is only available when the `cache` feature of this crate is
    /// enabled.
    ///
    /// # Errors
    ///
    /// This method can fail due to any error that happens when loading the
    /// default system configuration. Note that it is not an error if the
    /// default config file does not exist, in which case the default settings
    /// for an enabled cache are applied.
    ///
    /// [docs]: https://bytecodealliance.github.io/wasmtime/cli-cache.html
    #[cfg(feature = "cache")]
    pub fn cache_config_load_default(&mut self) -> Result<&mut Self> {
        self.cache_config = CacheConfig::from_file(None)?;
        Ok(self)
    }

    /// Sets a custom memory creator
    pub fn with_host_memory(&mut self, mem_creator: Arc<dyn MemoryCreator>) -> &mut Self {
        self.memory_creator = Some(MemoryCreatorProxy { mem_creator });
        self
    }

    /// Configures the maximum size, in bytes, where a linear memory is
    /// considered static, above which it'll be considered dynamic.
    ///
    /// This function configures the threshold for wasm memories whether they're
    /// implemented as a dynamically relocatable chunk of memory or a statically
    /// located chunk of memory. The `max_size` parameter here is the size, in
    /// bytes, where if the maximum size of a linear memory is below `max_size`
    /// then it will be statically allocated with enough space to never have to
    /// move. If the maximum size of a linear memory is larger than `max_size`
    /// then wasm memory will be dynamically located and may move in memory
    /// through growth operations.
    ///
    /// Specifying a `max_size` of 0 means that all memories will be dynamic and
    /// may be relocated through `memory.grow`. Also note that if any wasm
    /// memory's maximum size is below `max_size` then it will still reserve
    /// `max_size` bytes in the virtual memory space.
    ///
    /// ## Static vs Dynamic Memory
    ///
    /// Linear memories represent contiguous arrays of bytes, but they can also
    /// be grown through the API and wasm instructions. When memory is grown if
    /// space hasn't been preallocated then growth may involve relocating the
    /// base pointer in memory. Memories in Wasmtime are classified in two
    /// different ways:
    ///
    /// * **static** - these memories preallocate all space necessary they'll
    ///   ever need, meaning that the base pointer of these memories is never
    ///   moved. Static memories may take more virtual memory space because of
    ///   pre-reserving space for memories.
    ///
    /// * **dynamic** - these memories are not preallocated and may move during
    ///   growth operations. Dynamic memories consume less virtual memory space
    ///   because they don't need to preallocate space for future growth.
    ///
    /// Static memories can be optimized better in JIT code because once the
    /// base address is loaded in a function it's known that we never need to
    /// reload it because it never changes, `memory.grow` is generally a pretty
    /// fast operation because the wasm memory is never relocated, and under
    /// some conditions bounds checks can be elided on memory accesses.
    ///
    /// Dynamic memories can't be quite as heavily optimized because the base
    /// address may need to be reloaded more often, they may require relocating
    /// lots of data on `memory.grow`, and dynamic memories require
    /// unconditional bounds checks on all memory accesses.
    ///
    /// ## Should you use static or dynamic memory?
    ///
    /// In general you probably don't need to change the value of this property.
    /// The defaults here are optimized for each target platform to consume a
    /// reasonable amount of physical memory while also generating speedy
    /// machine code.
    ///
    /// One of the main reasons you may want to configure this today is if your
    /// environment can't reserve virtual memory space for each wasm linear
    /// memory. On 64-bit platforms wasm memories require a 6GB reservation by
    /// default, and system limits may prevent this in some scenarios. In this
    /// case you may wish to force memories to be allocated dynamically meaning
    /// that the virtual memory footprint of creating a wasm memory should be
    /// exactly what's used by the wasm itself.
    ///
    /// For 32-bit memories a static memory must contain at least 4GB of
    /// reserved address space plus a guard page to elide any bounds checks at
    /// all. Smaller static memories will use similar bounds checks as dynamic
    /// memories.
    ///
    /// ## Default
    ///
    /// The default value for this property depends on the host platform. For
    /// 64-bit platforms there's lots of address space available, so the default
    /// configured here is 4GB. WebAssembly linear memories currently max out at
    /// 4GB which means that on 64-bit platforms Wasmtime by default always uses
    /// a static memory. This, coupled with a sufficiently sized guard region,
    /// should produce the fastest JIT code on 64-bit platforms, but does
    /// require a large address space reservation for each wasm memory.
    ///
    /// For 32-bit platforms this value defaults to 1GB. This means that wasm
    /// memories whose maximum size is less than 1GB will be allocated
    /// statically, otherwise they'll be considered dynamic.
    pub fn static_memory_maximum_size(&mut self, max_size: u64) -> &mut Self {
        let max_pages = max_size / u64::from(wasmtime_environ::WASM_PAGE_SIZE);
        self.tunables.static_memory_bound = u32::try_from(max_pages).unwrap_or(u32::max_value());
        self
    }

    /// Configures the size, in bytes, of the guard region used at the end of a
    /// static memory's address space reservation.
    ///
    /// All WebAssembly loads/stores are bounds-checked and generate a trap if
    /// they're out-of-bounds. Loads and stores are often very performance
    /// critical, so we want the bounds check to be as fast as possible!
    /// Accelerating these memory accesses is the motivation for a guard after a
    /// memory allocation.
    ///
    /// Memories (both static and dynamic) can be configured with a guard at the
    /// end of them which consists of unmapped virtual memory. This unmapped
    /// memory will trigger a memory access violation (e.g. segfault) if
    /// accessed. This allows JIT code to elide bounds checks if it can prove
    /// that an access, if out of bounds, would hit the guard region. This means
    /// that having such a guard of unmapped memory can remove the need for
    /// bounds checks in JIT code.
    ///
    /// For the difference between static and dynamic memories, see the
    /// [`Config::static_memory_maximum_size`].
    ///
    /// ## How big should the guard be?
    ///
    /// In general, like with configuring `static_memory_maximum_size`, you
    /// probably don't want to change this value from the defaults. Otherwise,
    /// though, the size of the guard region affects the number of bounds checks
    /// needed for generated wasm code. More specifically, loads/stores with
    /// immediate offsets will generate bounds checks based on how big the guard
    /// page is.
    ///
    /// For 32-bit memories a 4GB static memory is required to even start
    /// removing bounds checks. A 4GB guard size will guarantee that the module
    /// has zero bounds checks for memory accesses. A 2GB guard size will
    /// eliminate all bounds checks with an immediate offset less than 2GB. A
    /// guard size of zero means that all memory accesses will still have bounds
    /// checks.
    ///
    /// ## Default
    ///
    /// The default value for this property is 2GB on 64-bit platforms. This
    /// allows eliminating almost all bounds checks on loads/stores with an
    /// immediate offset of less than 2GB. On 32-bit platforms this defaults to
    /// 64KB.
    ///
    /// ## Static vs Dynamic Guard Size
    ///
    /// Note that for now the static memory guard size must be at least as large
    /// as the dynamic memory guard size, so configuring this property to be
    /// smaller than the dynamic memory guard size will have no effect.
    pub fn static_memory_guard_size(&mut self, guard_size: u64) -> &mut Self {
        let guard_size = round_up_to_pages(guard_size);
        let guard_size = cmp::max(guard_size, self.tunables.dynamic_memory_offset_guard_size);
        self.tunables.static_memory_offset_guard_size = guard_size;
        self
    }

    /// Configures the size, in bytes, of the guard region used at the end of a
    /// dynamic memory's address space reservation.
    ///
    /// For the difference between static and dynamic memories, see the
    /// [`Config::static_memory_maximum_size`]
    ///
    /// For more information about what a guard is, see the documentation on
    /// [`Config::static_memory_guard_size`].
    ///
    /// Note that the size of the guard region for dynamic memories is not super
    /// critical for performance. Making it reasonably-sized can improve
    /// generated code slightly, but for maximum performance you'll want to lean
    /// towards static memories rather than dynamic anyway.
    ///
    /// Also note that the dynamic memory guard size must be smaller than the
    /// static memory guard size, so if a large dynamic memory guard is
    /// specified then the static memory guard size will also be automatically
    /// increased.
    ///
    /// ## Default
    ///
    /// This value defaults to 64KB.
    pub fn dynamic_memory_guard_size(&mut self, guard_size: u64) -> &mut Self {
        let guard_size = round_up_to_pages(guard_size);
        self.tunables.dynamic_memory_offset_guard_size = guard_size;
        self.tunables.static_memory_offset_guard_size =
            cmp::max(guard_size, self.tunables.static_memory_offset_guard_size);
        self
    }

    pub(crate) fn target_isa(&self) -> Box<dyn TargetIsa> {
        self.isa_flags
            .clone()
            .finish(settings::Flags::new(self.flags.clone()))
    }

    pub(crate) fn target_isa_with_reference_types(&self) -> Box<dyn TargetIsa> {
        let mut flags = self.flags.clone();
        flags.set("enable_safepoints", "true").unwrap();
        self.isa_flags.clone().finish(settings::Flags::new(flags))
    }

    pub(crate) fn validator(&self) -> Validator {
        let mut ret = Validator::new();
        ret.wasm_threads(self.wasm_threads)
            .wasm_bulk_memory(self.wasm_bulk_memory)
            .wasm_multi_value(self.wasm_multi_value)
            .wasm_reference_types(self.wasm_reference_types)
            .wasm_simd(self.wasm_simd);
        return ret;
    }

    fn build_compiler(&self) -> Compiler {
        let isa = self.target_isa();
        Compiler::new(isa, self.strategy, self.tunables.clone())
    }

    /// Hashes/fingerprints compiler setting to ensure that compatible
    /// compilation artifacts are used.
    pub(crate) fn compiler_fingerprint<H>(&self, state: &mut H)
    where
        H: Hasher,
    {
        self.flags.hash(state);
        self.tunables.hash(state);

        let triple = Triple::host();
        triple.hash(state);

        // Catch accidental bugs of reusing across wasmtime versions.
        env!("CARGO_PKG_VERSION").hash(state);
    }
}

fn round_up_to_pages(val: u64) -> u64 {
    let page_size = region::page::size() as u64;
    debug_assert!(page_size.is_power_of_two());
    val.checked_add(page_size - 1)
        .map(|val| val & !(page_size - 1))
        .unwrap_or(u64::max_value() / page_size + 1)
}

impl Default for Config {
    fn default() -> Config {
        Config::new()
    }
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Config")
            .field("debug_info", &self.tunables.debug_info)
            .field("strategy", &self.strategy)
            .field("wasm_threads", &self.wasm_threads)
            .field("wasm_reference_types", &self.wasm_reference_types)
            .field("wasm_bulk_memory", &self.wasm_bulk_memory)
            .field("wasm_simd", &self.wasm_simd)
            .field("wasm_multi_value", &self.wasm_multi_value)
            .field(
                "flags",
                &settings::Flags::new(self.flags.clone()).to_string(),
            )
            .finish()
    }
}

/// Possible Compilation strategies for a wasm module.
///
/// This is used as an argument to the [`Config::strategy`] method.
#[non_exhaustive]
#[derive(Clone, Debug)]
pub enum Strategy {
    /// An indicator that the compilation strategy should be automatically
    /// selected.
    ///
    /// This is generally what you want for most projects and indicates that the
    /// `wasmtime` crate itself should make the decision about what the best
    /// code generator for a wasm module is.
    ///
    /// Currently this always defaults to Cranelift, but the default value will
    /// change over time.
    Auto,

    /// Currently the default backend, Cranelift aims to be a reasonably fast
    /// code generator which generates high quality machine code.
    Cranelift,

    /// A single-pass code generator that is faster than Cranelift but doesn't
    /// produce as high-quality code.
    ///
    /// To successfully pass this argument to [`Config::strategy`] the
    /// `lightbeam` feature of this crate must be enabled.
    Lightbeam,
}

/// Possible optimization levels for the Cranelift codegen backend.
#[non_exhaustive]
#[derive(Clone, Debug)]
pub enum OptLevel {
    /// No optimizations performed, minimizes compilation time by disabling most
    /// optimizations.
    None,
    /// Generates the fastest possible code, but may take longer.
    Speed,
    /// Similar to `speed`, but also performs transformations aimed at reducing
    /// code size.
    SpeedAndSize,
}

/// Select which profiling technique to support.
#[derive(Debug, Clone, Copy)]
pub enum ProfilingStrategy {
    /// No profiler support.
    None,

    /// Collect profiling info for "jitdump" file format, used with `perf` on
    /// Linux.
    JitDump,

    /// Collect profiling info using the "ittapi", used with `VTune` on Linux.
    VTune,
}

// Engine

/// An `Engine` which is a global context for compilation and management of wasm
/// modules.
///
/// An engine can be safely shared across threads and is a cheap cloneable
/// handle to the actual engine. The engine itself will be deallocate once all
/// references to it have gone away.
///
/// Engines store global configuration preferences such as compilation settings,
/// enabled features, etc. You'll likely only need at most one of these for a
/// program.
///
/// ## Engines and `Clone`
///
/// Using `clone` on an `Engine` is a cheap operation. It will not create an
/// entirely new engine, but rather just a new reference to the existing engine.
/// In other words it's a shallow copy, not a deep copy.
///
/// ## Engines and `Default`
///
/// You can create an engine with default configuration settings using
/// `Engine::default()`. Be sure to consult the documentation of [`Config`] for
/// default settings.
#[derive(Clone)]
pub struct Engine {
    inner: Arc<EngineInner>,
}

pub(crate) struct EngineInner {
    config: Config,
    compiler: Compiler,
    debugger_data: Mutex<Option<Box<dyn std::any::Any + Send + Sync>>>,
    jit_code: EngineJitCode,
}

impl EngineInner {
    pub(crate) fn config(&self) -> &Config {
        &self.config
    }
    pub(crate) fn jit_code(&self) -> &EngineJitCode {
        &self.jit_code
    }
    pub(crate) fn find_breakpoint(&self, addr: usize) -> Option<*const BreakpointData> {
        let lock = self.config.debugger.lock().unwrap();
        lock.find_breakpoint(addr)
    }
}

impl From<Arc<EngineInner>> for Engine {
    fn from(inner: Arc<EngineInner>) -> Self {
        Self { inner }
    }
}

pub(crate) struct EngineJitCode {
    jit_code_ranges: Arc<Mutex<Vec<(usize, usize, SyncWeak<CompiledModule>)>>>,
}

impl EngineJitCode {
    fn new() -> Self {
        Self {
            jit_code_ranges: Arc::new(Mutex::new(vec![])),
        }
    }
    pub(crate) fn lookup_jit_code_range<'a>(
        &'a self,
        addr: usize,
    ) -> Option<(usize, usize, SyncWeak<CompiledModule>)> {
        let ranges = self.jit_code_ranges.lock().unwrap();
        ranges.iter().find(|r| r.0 <= addr && addr < r.1).cloned()
    }
    pub(crate) fn register_jit_code(
        &self,
        module: &Arc<CompiledModule>,
    ) -> EngineJitCodeRegistration {
        use std::iter::FromIterator;
        let weak = Arc::downgrade(module);
        let ranges = module
            .jit_code_ranges()
            .map(|(start, end)| (start, end, weak.clone()));
        let mut jit_code_ranges = self.jit_code_ranges.lock().unwrap();
        jit_code_ranges.extend(ranges);
        EngineJitCodeRegistration {
            jit_code_ranges: self.jit_code_ranges.clone(),
            ranges: BTreeSet::from_iter(module.jit_code_ranges()),
        }
    }
}

pub(crate) struct EngineJitCodeRegistration {
    jit_code_ranges: Arc<Mutex<Vec<(usize, usize, SyncWeak<CompiledModule>)>>>,
    ranges: BTreeSet<(usize, usize)>,
}

impl Drop for EngineJitCodeRegistration {
    fn drop(&mut self) {
        let mut jit_code_ranges = self.jit_code_ranges.lock().unwrap();
        jit_code_ranges.retain(|r| !self.ranges.contains(&(r.0, r.1)));
    }
}

impl Engine {
    /// Creates a new [`Engine`] with the specified compilation and
    /// configuration settings.
    pub fn new(config: &Config) -> Engine {
        debug_builtins::ensure_exported();
        Engine {
            inner: Arc::new(EngineInner {
                config: config.clone(),
                compiler: config.build_compiler(),
                debugger_data: Mutex::new(None),
                jit_code: EngineJitCode::new(),
            }),
        }
    }

    /// Returns the configuration settings that this engine is using.
    pub fn config(&self) -> &Config {
        &self.inner.config
    }

    pub(crate) fn compiler(&self) -> &Compiler {
        &self.inner.compiler
    }

    #[cfg(feature = "cache")]
    pub(crate) fn cache_config(&self) -> &CacheConfig {
        &self.config().cache_config
    }

    pub(crate) fn weak(&self) -> SyncWeak<EngineInner> {
        Arc::downgrade(&self.inner)
    }

    pub(crate) fn jit_code(&self) -> &EngineJitCode {
        &self.inner.jit_code
    }

    pub(crate) fn register_module(
        &self,
        compiled_module: &Arc<CompiledModule>,
        bytes: &[u8],
    ) -> ModuleRegistration {
        let mut lock = self.inner.config().debugger.lock().unwrap();
        let reg = lock.register_module(DebuggerModule::new(compiled_module, self.weak(), bytes));
        ModuleRegistration(reg)
    }

    pub(crate) fn add_breakpoints(&self, module_id: u32, addr: u64) {
        let lock = self.inner.config().debugger.lock().unwrap();
        lock.add_breakpoints(module_id, addr);
    }

    /// Returns whether the engine `a` and `b` refer to the same configuration.
    pub fn same(a: &Engine, b: &Engine) -> bool {
        Arc::ptr_eq(&a.inner, &b.inner)
    }
}

impl Default for Engine {
    fn default() -> Engine {
        Engine::new(&Config::default())
    }
}

impl DebuggerContext for Engine {
    fn patchable(&self) -> &dyn PatchableCode {
        self
    }
    fn find_breakpoint(&self, addr: *const u8) -> Option<*const BreakpointData> {
        let addr = addr as usize;
        self.inner.find_breakpoint(addr)
    }
    fn pause(&self, kind: DebuggerPauseKind) -> DebuggerResumeAction {
        self.config().debugger.lock().unwrap().pause(kind)
    }
    fn data<'c, 'a>(&'c self) -> DebuggerContextData<'c, 'a> {
        self.inner.debugger_data.lock().unwrap()
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

impl PatchableCode for Engine {
    fn patch_jit_code(&self, addr: usize, len: usize, f: &mut dyn FnMut()) {
        let compiled = self
            .inner
            .jit_code()
            .lookup_jit_code_range(addr)
            .and_then(|(_, _, module)| module.upgrade())
            .expect("jit_code_range module ref exist");
        compiled.patch_jit_code(addr, len, f);
    }
}

pub(crate) struct ModuleRegistration(Box<dyn DebuggerJitCodeRegistration>);

impl ModuleRegistration {
    pub(crate) fn id(&self) -> u32 {
        self.0.id()
    }
}

// Store

/// A `Store` is a collection of WebAssembly instances and host-defined items.
///
/// All WebAssembly instances and items will be attached to and refer to a
/// `Store`. For example instances, functions, globals, and tables are all
/// attached to a `Store`. Instances are created by instantiating a [`Module`]
/// within a `Store`.
///
/// `Store` is not thread-safe and cannot be sent to other threads. All items
/// which refer to a `Store` additionally are not threadsafe and can only be
/// used on the original thread that they were created on.
///
/// A `Store` is not intended to be a long-lived object in a program. No form of
/// GC is implemented at this time so once an instance is created within a
/// `Store` it will not be deallocated until all references to the `Store` have
/// gone away (this includes all references to items in the store). This makes
/// `Store` unsuitable for creating an unbounded number of instances in it
/// because `Store` will never release this memory. It's instead recommended to
/// have a long-lived [`Engine`] and instead create a `Store` for a more scoped
/// portion of your application.
///
/// # Stores and `Clone`
///
/// Using `clone` on a `Store` is a cheap operation. It will not create an
/// entirely new store, but rather just a new reference to the existing object.
/// In other words it's a shallow copy, not a deep copy.
///
/// ## Stores and `Default`
///
/// You can create a store with default configuration settings using
/// `Store::default()`. This will create a brand new [`Engine`] with default
/// ocnfiguration (see [`Config`] for more information).
#[derive(Clone)]
pub struct Store {
    inner: Rc<StoreInner>,
}

pub(crate) struct StoreInner {
    engine: Engine,
    interrupts: Arc<VMInterrupts>,
    signatures: RefCell<SignatureRegistry>,
    instances: RefCell<Vec<InstanceHandle>>,
    signal_handler: RefCell<Option<Box<SignalHandler<'static>>>>,
    jit_code_ranges: RefCell<Vec<(usize, usize)>>,
    externref_activations_table: VMExternRefActivationsTable,
    stack_map_registry: StackMapRegistry,
}

struct HostInfoKey(VMExternRef);

impl PartialEq for HostInfoKey {
    fn eq(&self, rhs: &Self) -> bool {
        VMExternRef::eq(&self.0, &rhs.0)
    }
}

impl Eq for HostInfoKey {}

impl Hash for HostInfoKey {
    fn hash<H>(&self, hasher: &mut H)
    where
        H: Hasher,
    {
        VMExternRef::hash(&self.0, hasher);
    }
}

impl Store {
    /// Creates a new store to be associated with the given [`Engine`].
    pub fn new(engine: &Engine) -> Store {
        // Ensure that wasmtime_runtime's signal handlers are configured. Note
        // that at the `Store` level it means we should perform this
        // once-per-thread. Platforms like Unix, however, only require this
        // once-per-program. In any case this is safe to call many times and
        // each one that's not relevant just won't do anything.
        wasmtime_runtime::init_traps();

        Store {
            inner: Rc::new(StoreInner {
                engine: engine.clone(),
                interrupts: Arc::new(Default::default()),
                signatures: RefCell::new(Default::default()),
                instances: RefCell::new(Vec::new()),
                signal_handler: RefCell::new(None),
                jit_code_ranges: RefCell::new(Vec::new()),
                externref_activations_table: VMExternRefActivationsTable::new(),
                stack_map_registry: StackMapRegistry::default(),
            }),
        }
    }

    pub(crate) fn from_inner(inner: Rc<StoreInner>) -> Store {
        Store { inner }
    }

    /// Returns the [`Engine`] that this store is associated with.
    pub fn engine(&self) -> &Engine {
        &self.inner.engine
    }

    /// Returns an optional reference to a ['RuntimeMemoryCreator']
    pub(crate) fn memory_creator(&self) -> Option<&dyn RuntimeMemoryCreator> {
        self.engine()
            .config()
            .memory_creator
            .as_ref()
            .map(|x| x as _)
    }

    pub(crate) fn lookup_signature(&self, sig_index: VMSharedSignatureIndex) -> wasm::WasmFuncType {
        self.inner
            .signatures
            .borrow()
            .lookup_wasm(sig_index)
            .expect("failed to lookup signature")
    }

    pub(crate) fn lookup_wasm_and_native_signatures(
        &self,
        sig_index: VMSharedSignatureIndex,
    ) -> (wasm::WasmFuncType, ir::Signature) {
        self.inner
            .signatures
            .borrow()
            .lookup_wasm_and_native_signatures(sig_index)
            .expect("failed to lookup signature")
    }

    pub(crate) fn register_signature(
        &self,
        wasm_sig: wasm::WasmFuncType,
        native: ir::Signature,
    ) -> VMSharedSignatureIndex {
        self.inner
            .signatures
            .borrow_mut()
            .register(wasm_sig, native)
    }

    pub(crate) fn signatures_mut(&self) -> std::cell::RefMut<'_, SignatureRegistry> {
        self.inner.signatures.borrow_mut()
    }

    /// Returns whether or not the given address falls within the JIT code
    /// managed by the compiler
    pub(crate) fn is_in_jit_code(&self, addr: usize) -> bool {
        self.inner
            .jit_code_ranges
            .borrow()
            .iter()
            .any(|(start, end)| *start <= addr && addr < *end)
    }

    pub(crate) fn register_jit_code(&self, mut ranges: impl Iterator<Item = (usize, usize)>) {
        // Checking of we already registered JIT code ranges by searching
        // first range start.
        match ranges.next() {
            None => (),
            Some(first) => {
                if !self.is_in_jit_code(first.0) {
                    // The range is not registered -- add all ranges (including
                    // first one) to the jit_code_ranges.
                    let mut jit_code_ranges = self.inner.jit_code_ranges.borrow_mut();
                    jit_code_ranges.push(first);
                    jit_code_ranges.extend(ranges);
                }
            }
        }
    }

    pub(crate) fn register_stack_maps(&self, module: &Module) {
        let module = &module.compiled_module();
        self.stack_map_registry()
            .register_stack_maps(module.stack_maps().map(|(func, stack_maps)| unsafe {
                let ptr = (*func).as_ptr();
                let len = (*func).len();
                let start = ptr as usize;
                let end = ptr as usize + len;
                let range = start..end;
                (range, stack_maps)
            }));
    }

    pub(crate) unsafe fn add_instance(&self, handle: InstanceHandle) -> StoreInstanceHandle {
        self.inner.instances.borrow_mut().push(handle.clone());
        StoreInstanceHandle {
            store: self.clone(),
            handle,
        }
    }

    pub(crate) fn existing_instance_handle(&self, handle: InstanceHandle) -> StoreInstanceHandle {
        debug_assert!(self
            .inner
            .instances
            .borrow()
            .iter()
            .any(|i| i.vmctx_ptr() == handle.vmctx_ptr()));
        StoreInstanceHandle {
            store: self.clone(),
            handle,
        }
    }

    pub(crate) fn weak(&self) -> Weak<StoreInner> {
        Rc::downgrade(&self.inner)
    }

    pub(crate) fn upgrade(weak: &Weak<StoreInner>) -> Option<Self> {
        let inner = weak.upgrade()?;
        Some(Self { inner })
    }

    pub(crate) fn signal_handler(&self) -> std::cell::Ref<'_, Option<Box<SignalHandler<'static>>>> {
        self.inner.signal_handler.borrow()
    }

    pub(crate) fn signal_handler_mut(
        &self,
    ) -> std::cell::RefMut<'_, Option<Box<SignalHandler<'static>>>> {
        self.inner.signal_handler.borrow_mut()
    }

    pub(crate) fn interrupts(&self) -> &Arc<VMInterrupts> {
        &self.inner.interrupts
    }

    /// Returns whether the stores `a` and `b` refer to the same underlying
    /// `Store`.
    ///
    /// Because the `Store` type is reference counted multiple clones may point
    /// to the same underlying storage, and this method can be used to determine
    /// whether two stores are indeed the same.
    pub fn same(a: &Store, b: &Store) -> bool {
        Rc::ptr_eq(&a.inner, &b.inner)
    }

    /// Creates an [`InterruptHandle`] which can be used to interrupt the
    /// execution of instances within this `Store`.
    ///
    /// An [`InterruptHandle`] handle is a mechanism of ensuring that guest code
    /// doesn't execute for too long. For example it's used to prevent wasm
    /// programs for executing infinitely in infinite loops or recursive call
    /// chains.
    ///
    /// The [`InterruptHandle`] type is sendable to other threads so you can
    /// interact with it even while the thread with this `Store` is executing
    /// wasm code.
    ///
    /// There's one method on an interrupt handle:
    /// [`InterruptHandle::interrupt`]. This method is used to generate an
    /// interrupt and cause wasm code to exit "soon".
    ///
    /// ## When are interrupts delivered?
    ///
    /// The term "interrupt" here refers to one of two different behaviors that
    /// are interrupted in wasm:
    ///
    /// * The head of every loop in wasm has a check to see if it's interrupted.
    /// * The prologue of every function has a check to see if it's interrupted.
    ///
    /// This interrupt mechanism makes no attempt to signal interrupts to
    /// native code. For example if a host function is blocked, then sending
    /// an interrupt will not interrupt that operation.
    ///
    /// Interrupts are consumed as soon as possible when wasm itself starts
    /// executing. This means that if you interrupt wasm code then it basically
    /// guarantees that the next time wasm is executing on the target thread it
    /// will return quickly (either normally if it were already in the process
    /// of returning or with a trap from the interrupt). Once an interrupt
    /// trap is generated then an interrupt is consumed, and further execution
    /// will not be interrupted (unless another interrupt is set).
    ///
    /// When implementing interrupts you'll want to ensure that the delivery of
    /// interrupts into wasm code is also handled in your host imports and
    /// functionality. Host functions need to either execute for bounded amounts
    /// of time or you'll need to arrange for them to be interrupted as well.
    ///
    /// ## Return Value
    ///
    /// This function returns a `Result` since interrupts are not always
    /// enabled. Interrupts are enabled via the [`Config::interruptable`]
    /// method, and if this store's [`Config`] hasn't been configured to enable
    /// interrupts then an error is returned.
    ///
    /// ## Examples
    ///
    /// ```
    /// # use anyhow::Result;
    /// # use wasmtime::*;
    /// # fn main() -> Result<()> {
    /// // Enable interruptable code via `Config` and then create an interrupt
    /// // handle which we'll use later to interrupt running code.
    /// let engine = Engine::new(Config::new().interruptable(true));
    /// let store = Store::new(&engine);
    /// let interrupt_handle = store.interrupt_handle()?;
    ///
    /// // Compile and instantiate a small example with an infinite loop.
    /// let module = Module::new(&engine, r#"
    ///     (func (export "run") (loop br 0))
    /// "#)?;
    /// let instance = Instance::new(&store, &module, &[])?;
    /// let run = instance
    ///     .get_func("run")
    ///     .ok_or(anyhow::format_err!("failed to find `run` function export"))?
    ///     .get0::<()>()?;
    ///
    /// // Spin up a thread to send us an interrupt in a second
    /// std::thread::spawn(move || {
    ///     std::thread::sleep(std::time::Duration::from_secs(1));
    ///     interrupt_handle.interrupt();
    /// });
    ///
    /// let trap = run().unwrap_err();
    /// assert!(trap.to_string().contains("wasm trap: interrupt"));
    /// # Ok(())
    /// # }
    /// ```
    pub fn interrupt_handle(&self) -> Result<InterruptHandle> {
        if self.engine().config().tunables.interruptable {
            Ok(InterruptHandle {
                interrupts: self.interrupts().clone(),
            })
        } else {
            bail!("interrupts aren't enabled for this `Store`")
        }
    }

    pub(crate) fn externref_activations_table(&self) -> &VMExternRefActivationsTable {
        &self.inner.externref_activations_table
    }

    pub(crate) fn stack_map_registry(&self) -> &StackMapRegistry {
        &self.inner.stack_map_registry
    }

    /// Perform garbage collection of `ExternRef`s.
    pub fn gc(&self) {
        // For this crate's API, we ensure that `set_stack_canary` invariants
        // are upheld for all host-->Wasm calls, and we register every module
        // used with this store in `self.inner.stack_map_registry`.
        unsafe {
            wasmtime_runtime::gc(
                &self.inner.stack_map_registry,
                &self.inner.externref_activations_table,
            );
        }
    }
}

impl Default for Store {
    fn default() -> Store {
        Store::new(&Engine::default())
    }
}

impl fmt::Debug for Store {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let inner = &*self.inner as *const StoreInner;
        f.debug_struct("Store").field("inner", &inner).finish()
    }
}

impl Drop for StoreInner {
    fn drop(&mut self) {
        for instance in self.instances.get_mut().iter() {
            unsafe {
                instance.dealloc();
            }
        }
    }
}

/// A threadsafe handle used to interrupt instances executing within a
/// particular `Store`.
///
/// This structure is created by the [`Store::interrupt_handle`] method.
pub struct InterruptHandle {
    interrupts: Arc<VMInterrupts>,
}

impl InterruptHandle {
    /// Flags that execution within this handle's original [`Store`] should be
    /// interrupted.
    ///
    /// This will not immediately interrupt execution of wasm modules, but
    /// rather it will interrupt wasm execution of loop headers and wasm
    /// execution of function entries. For more information see
    /// [`Store::interrupt_handle`].
    pub fn interrupt(&self) {
        self.interrupts.interrupt()
    }
}

fn _assert_send_sync() {
    fn _assert<T: Send + Sync>() {}
    _assert::<Engine>();
    _assert::<Config>();
    _assert::<InterruptHandle>();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Module;
    use tempfile::TempDir;

    #[test]
    fn cache_accounts_for_opt_level() -> Result<()> {
        let td = TempDir::new()?;
        let config_path = td.path().join("config.toml");
        std::fs::write(
            &config_path,
            &format!(
                "
                    [cache]
                    enabled = true
                    directory = '{}'
                ",
                td.path().join("cache").display()
            ),
        )?;
        let mut cfg = Config::new();
        cfg.cranelift_opt_level(OptLevel::None)
            .cache_config_load(&config_path)?;
        let engine = Engine::new(&cfg);
        Module::new(&engine, "(module (func))")?;
        assert_eq!(engine.config().cache_config.cache_hits(), 0);
        assert_eq!(engine.config().cache_config.cache_misses(), 1);
        Module::new(&engine, "(module (func))")?;
        assert_eq!(engine.config().cache_config.cache_hits(), 1);
        assert_eq!(engine.config().cache_config.cache_misses(), 1);

        let mut cfg = Config::new();
        cfg.cranelift_opt_level(OptLevel::Speed)
            .cache_config_load(&config_path)?;
        let engine = Engine::new(&cfg);
        Module::new(&engine, "(module (func))")?;
        assert_eq!(engine.config().cache_config.cache_hits(), 0);
        assert_eq!(engine.config().cache_config.cache_misses(), 1);
        Module::new(&engine, "(module (func))")?;
        assert_eq!(engine.config().cache_config.cache_hits(), 1);
        assert_eq!(engine.config().cache_config.cache_misses(), 1);

        let mut cfg = Config::new();
        cfg.cranelift_opt_level(OptLevel::SpeedAndSize)
            .cache_config_load(&config_path)?;
        let engine = Engine::new(&cfg);
        Module::new(&engine, "(module (func))")?;
        assert_eq!(engine.config().cache_config.cache_hits(), 0);
        assert_eq!(engine.config().cache_config.cache_misses(), 1);
        Module::new(&engine, "(module (func))")?;
        assert_eq!(engine.config().cache_config.cache_hits(), 1);
        assert_eq!(engine.config().cache_config.cache_misses(), 1);

        // FIXME(#1523) need debuginfo on aarch64 before we run this test there
        if !cfg!(target_arch = "aarch64") {
            let mut cfg = Config::new();
            cfg.debug_info(true).cache_config_load(&config_path)?;
            let engine = Engine::new(&cfg);
            Module::new(&engine, "(module (func))")?;
            assert_eq!(engine.config().cache_config.cache_hits(), 0);
            assert_eq!(engine.config().cache_config.cache_misses(), 1);
            Module::new(&engine, "(module (func))")?;
            assert_eq!(engine.config().cache_config.cache_hits(), 1);
            assert_eq!(engine.config().cache_config.cache_misses(), 1);
        }

        Ok(())
    }
}
