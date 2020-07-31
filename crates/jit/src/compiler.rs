//! JIT compilation.

use crate::instantiate::SetupError;
use crate::object::{build_object, ObjectUnwindInfo};
use cranelift_codegen::ir;
use std::hash::{Hash, Hasher};
use wasmtime_debug::{emit_dwarf, DebugInfoData, DwarfSection};
use wasmtime_environ::entity::{EntityRef, PrimaryMap};
use wasmtime_environ::isa::{unwind::UnwindInfo, TargetFrontendConfig, TargetIsa};
use wasmtime_environ::wasm::{DefinedFuncIndex, DefinedMemoryIndex, MemoryIndex};
use wasmtime_environ::{
    Compiler as _C, Module, ModuleAddressMap, ModuleMemoryOffset, ModuleTranslation,
    ModuleVmctxInfo, StackMaps, Traps, Tunables, VMOffsets, ValueLabelsRanges,
};

/// Select which kind of compilation to use.
#[derive(Copy, Clone, Debug, Hash)]
pub enum CompilationStrategy {
    /// Let Wasmtime pick the strategy.
    Auto,

    /// Compile all functions with Cranelift.
    Cranelift,

    /// Compile all functions with Lightbeam.
    #[cfg(feature = "lightbeam")]
    Lightbeam,
}

/// A WebAssembly code JIT compiler.
///
/// A `Compiler` instance owns the executable memory that it allocates.
///
/// TODO: Evolve this to support streaming rather than requiring a `&[u8]`
/// containing a whole wasm module at once.
///
/// TODO: Consider using cranelift-module.
pub struct Compiler {
    isa: Box<dyn TargetIsa>,
    strategy: CompilationStrategy,
    tunables: Tunables,
}

impl Compiler {
    /// Construct a new `Compiler`.
    pub fn new(isa: Box<dyn TargetIsa>, strategy: CompilationStrategy, tunables: Tunables) -> Self {
        Self {
            isa,
            strategy,
            tunables,
        }
    }
}

fn _assert_compiler_send_sync() {
    fn _assert<T: Send + Sync>() {}
    _assert::<Compiler>();
}

fn transform_dwarf_data(
    isa: &dyn TargetIsa,
    module: &Module,
    debug_data: DebugInfoData,
    address_transform: &ModuleAddressMap,
    value_ranges: &ValueLabelsRanges,
    stack_slots: PrimaryMap<DefinedFuncIndex, ir::StackSlots>,
    unwind_info: PrimaryMap<DefinedFuncIndex, &Option<UnwindInfo>>,
) -> Result<Vec<DwarfSection>, SetupError> {
    let target_config = isa.frontend_config();
    let ofs = VMOffsets::new(target_config.pointer_bytes(), &module.local);

    let module_vmctx_info = {
        ModuleVmctxInfo {
            memory_offset: if ofs.num_imported_memories > 0 {
                ModuleMemoryOffset::Imported(ofs.vmctx_vmmemory_import(MemoryIndex::new(0)))
            } else if ofs.num_defined_memories > 0 {
                ModuleMemoryOffset::Defined(
                    ofs.vmctx_vmmemory_definition_base(DefinedMemoryIndex::new(0)),
                )
            } else {
                ModuleMemoryOffset::None
            },
            stack_slots,
        }
    };
    emit_dwarf(
        isa,
        &debug_data,
        &address_transform,
        &module_vmctx_info,
        &value_ranges,
        &unwind_info,
    )
    .map_err(SetupError::DebugInfo)
}

#[allow(missing_docs)]
pub struct Compilation {
    pub obj: Vec<u8>,
    pub unwind_info: Vec<ObjectUnwindInfo>,
    pub traps: Traps,
    pub stack_maps: StackMaps,
    pub address_transform: ModuleAddressMap,
}

impl Compiler {
    /// Return the isa.
    pub fn isa(&self) -> &dyn TargetIsa {
        self.isa.as_ref()
    }

    /// Return the target's frontend configuration settings.
    pub fn frontend_config(&self) -> TargetFrontendConfig {
        self.isa.frontend_config()
    }

    /// Return the tunables in use by this engine.
    pub fn tunables(&self) -> &Tunables {
        &self.tunables
    }

    /// Return the compilation strategy.
    pub fn strategy(&self) -> CompilationStrategy {
        self.strategy
    }

    /// Compile the given function bodies.
    pub(crate) fn compile<'data>(
        &self,
        translation: &ModuleTranslation,
        debug_data: Option<DebugInfoData>,
    ) -> Result<Compilation, SetupError> {
        let (
            compilation,
            relocations,
            address_transform,
            value_ranges,
            stack_slots,
            traps,
            stack_maps,
        ) = match self.strategy {
            // For now, interpret `Auto` as `Cranelift` since that's the most stable
            // implementation.
            CompilationStrategy::Auto | CompilationStrategy::Cranelift => {
                wasmtime_environ::cranelift::Cranelift::compile_module(translation, &*self.isa)
            }
            #[cfg(feature = "lightbeam")]
            CompilationStrategy::Lightbeam => {
                wasmtime_environ::lightbeam::Lightbeam::compile_module(translation, &*self.isa)
            }
        }
        .map_err(SetupError::Compile)?;

        let dwarf_sections = if debug_data.is_some() && !compilation.is_empty() {
            let unwind_info = compilation.unwind_info();
            transform_dwarf_data(
                &*self.isa,
                &translation.module,
                debug_data.unwrap(),
                &address_transform,
                &value_ranges,
                stack_slots,
                unwind_info,
            )?
        } else {
            vec![]
        };

        let (obj, unwind_info) = build_object(
            &*self.isa,
            &translation.module,
            compilation,
            relocations,
            dwarf_sections,
        )?;

        Ok(Compilation {
            obj,
            unwind_info,
            traps,
            stack_maps,
            address_transform,
        })
    }
}

impl Hash for Compiler {
    fn hash<H: Hasher>(&self, hasher: &mut H) {
        let Compiler {
            strategy,
            isa,
            tunables,
        } = self;

        // Hash compiler's flags: compilation strategy, isa, frontend config,
        // misc tunables.
        strategy.hash(hasher);
        isa.triple().hash(hasher);
        // TODO: if this `to_string()` is too expensive then we should upstream
        // a native hashing ability of flags into cranelift itself, but
        // compilation and/or cache loading is relatively expensive so seems
        // unlikely.
        isa.flags().to_string().hash(hasher);
        isa.frontend_config().hash(hasher);
        tunables.hash(hasher);

        // TODO: ... and should we hash anything else? There's a lot of stuff in
        // `TargetIsa`, like registers/encodings/etc. Should we be hashing that
        // too? It seems like wasmtime doesn't configure it too too much, but
        // this may become an issue at some point.
    }
}
