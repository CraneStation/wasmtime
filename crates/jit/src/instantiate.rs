//! Define the `instantiate` function, which takes a byte array containing an
//! encoded wasm module and returns a live wasm instance. Also, define
//! `CompiledModule` to allow compiling and instantiating to be done as separate
//! steps.

use crate::code_memory::CodeMemory;
use crate::compiler::{Compilation, Compiler};
use crate::imports::resolve_imports;
use crate::link::{link_module, unlink_module};
use crate::object::ObjectUnwindInfo;
use crate::resolver::Resolver;
use serde::{Deserialize, Serialize};
use std::any::Any;
use std::collections::HashMap;
use std::mem::ManuallyDrop;
use std::sync::Arc;
use thiserror::Error;
use wasmtime_debug::read_debuginfo;
use wasmtime_environ::entity::{BoxedSlice, PrimaryMap};
use wasmtime_environ::isa::TargetIsa;
use wasmtime_environ::wasm::{DefinedFuncIndex, SignatureIndex};
use wasmtime_environ::{
    CompileError, DataInitializer, DataInitializerLocation, Module, ModuleAddressMap,
    ModuleEnvironment, ModuleTranslation, StackMaps, Traps,
};
use wasmtime_obj::{ensure_supported_elf_format, patch_loadable_file, sanitize_loadable_file};
use wasmtime_profiling::ProfilingAgent;
use wasmtime_runtime::VMInterrupts;
use wasmtime_runtime::{
    GdbJitImageRegistration, InstanceHandle, InstantiationError, RuntimeMemoryCreator,
    SignatureRegistry, StackMapRegistry, VMExternRefActivationsTable, VMFunctionBody, VMTrampoline,
};

/// An error condition while setting up a wasm instance, be it validation,
/// compilation, or instantiation.
#[derive(Error, Debug)]
pub enum SetupError {
    /// The module did not pass validation.
    #[error("Validation error: {0}")]
    Validate(String),

    /// A wasm translation error occured.
    #[error("WebAssembly failed to compile")]
    Compile(#[from] CompileError),

    /// Some runtime resource was unavailable or insufficient, or the start function
    /// trapped.
    #[error("Instantiation failed during setup")]
    Instantiate(#[from] InstantiationError),

    /// Debug information generation error occured.
    #[error("Debug information error")]
    DebugInfo(#[from] anyhow::Error),
}

/// Contains all compilation artifacts.
#[derive(Serialize, Deserialize)]
pub struct CompilationArtifacts {
    /// Module metadata.
    module: Module,

    /// ELF image with functions code.
    obj: Vec<u8>,

    /// Unwind information for function code.
    unwind_info: Box<[ObjectUnwindInfo]>,

    /// Data initiailizers.
    data_initializers: Box<[OwnedDataInitializer]>,

    /// Traps descriptors.
    traps: Traps,

    /// Stack map descriptors.
    stack_maps: StackMaps,

    /// Wasm to function code address map.
    address_transform: ModuleAddressMap,

    /// Debug info presence flags.
    debug_info: bool,
}

impl CompilationArtifacts {
    /// Builds compilation artifacts.
    pub fn build(compiler: &Compiler, data: &[u8]) -> Result<Self, SetupError> {
        let environ = ModuleEnvironment::new(compiler.frontend_config(), compiler.tunables());

        let translation = environ
            .translate(data)
            .map_err(|error| SetupError::Compile(CompileError::Wasm(error)))?;

        let debug_info = compiler.tunables().debug_info;

        let mut debug_data = None;
        if debug_info {
            // TODO Do we want to ignore invalid DWARF data?
            debug_data = Some(read_debuginfo(&data)?);
        }

        let Compilation {
            obj,
            unwind_info,
            traps,
            stack_maps,
            address_transform,
        } = compiler.compile(&translation, debug_data)?;

        let ModuleTranslation {
            module,
            data_initializers,
            ..
        } = translation;

        let data_initializers = data_initializers
            .into_iter()
            .map(OwnedDataInitializer::new)
            .collect::<Vec<_>>()
            .into_boxed_slice();

        Ok(Self {
            module,
            obj,
            unwind_info: unwind_info.into_boxed_slice(),
            data_initializers,
            traps,
            stack_maps,
            address_transform,
            debug_info,
        })
    }
}

struct FinishedFunctions(BoxedSlice<DefinedFuncIndex, *mut [VMFunctionBody]>);

unsafe impl Send for FinishedFunctions {}
unsafe impl Sync for FinishedFunctions {}

/// Container for data needed for an Instance function to exist.
pub struct ModuleCode {
    code_memory: ManuallyDrop<CodeMemory>,
    dbg_jit_registration: ManuallyDrop<Option<GdbJitImageRegistration>>,
    // Note that this is stored as a `usize` instead of a `*const` or `*mut`
    // pointer to allow this structure to be natively `Send` and `Sync` without
    // `unsafe impl`. This type is sendable across threads and shareable since
    // the coordination all happens at the OS layer.
    image_range: std::ops::Range<usize>,
}

impl Drop for ModuleCode {
    fn drop(&mut self) {
        unsafe {
            ManuallyDrop::drop(&mut self.dbg_jit_registration);
            ManuallyDrop::drop(&mut self.code_memory);
        }
    }
}

/// A compiled wasm module, ready to be instantiated.
pub struct CompiledModule {
    module: Arc<Module>,
    code: Arc<ModuleCode>,
    finished_functions: FinishedFunctions,
    trampolines: PrimaryMap<SignatureIndex, VMTrampoline>,
    data_initializers: Box<[OwnedDataInitializer]>,
    traps: Traps,
    stack_maps: StackMaps,
    address_transform: ModuleAddressMap,
    unwind_info: Box<[ObjectUnwindInfo]>,
}

impl CompiledModule {
    /// Compile a data buffer into a `CompiledModule`, which may then be instantiated.
    pub fn new<'data>(
        compiler: &Compiler,
        data: &'data [u8],
        profiler: &dyn ProfilingAgent,
    ) -> Result<Self, SetupError> {
        let artifacts = CompilationArtifacts::build(compiler, data)?;
        Self::from_artifacts(artifacts, compiler.isa(), profiler)
    }

    /// Creates `CompiledModule` directly from `CompilationArtifacts`.
    pub fn from_artifacts(
        artifacts: CompilationArtifacts,
        isa: &dyn TargetIsa,
        profiler: &dyn ProfilingAgent,
    ) -> Result<Self, SetupError> {
        let CompilationArtifacts {
            module,
            obj,
            unwind_info,
            data_initializers,
            traps,
            stack_maps,
            address_transform,
            debug_info,
        } = artifacts;

        // Allocate all of the compiled functions into executable memory,
        // copying over their contents.
        let (code_memory, image_range, finished_functions, trampolines) =
            build_code_memory(isa, obj, &module, &unwind_info).map_err(|message| {
                SetupError::Instantiate(InstantiationError::Resource(format!(
                    "failed to build code memory for functions: {}",
                    message
                )))
            })?;

        // Register GDB JIT images; initialize profiler and load the wasm module.
        let dbg_jit_registration = if debug_info {
            let bytes = unsafe { std::slice::from_raw_parts(image_range.0, image_range.1) };
            ensure_supported_elf_format(bytes).map_err(SetupError::DebugInfo)?;

            profiler.module_load(&module, &finished_functions, Some(bytes));

            let reg = GdbJitImageRegistration::register(image_range.0, image_range.1);
            Some(reg)
        } else {
            profiler.module_load(&module, &finished_functions, None);
            None
        };

        let finished_functions = FinishedFunctions(finished_functions.into_boxed_slice());

        Ok(Self {
            module: Arc::new(module),
            code: Arc::new(ModuleCode {
                code_memory: ManuallyDrop::new(code_memory),
                dbg_jit_registration: ManuallyDrop::new(dbg_jit_registration),
                image_range: image_range.0 as usize..image_range.0 as usize + image_range.1,
            }),
            finished_functions,
            trampolines,
            data_initializers,
            traps,
            stack_maps,
            address_transform,
            unwind_info,
        })
    }

    /// Extracts `CompilationArtifacts` from the compiled module.
    pub fn to_compilation_artifacts(&self) -> CompilationArtifacts {
        // Get ELF image bytes and sanitize that.
        let mut obj = Vec::from(unsafe {
            let range = &self.code.image_range;
            std::slice::from_raw_parts(range.start as *const u8, range.len())
        });
        drop(unlink_module(&mut obj));
        drop(sanitize_loadable_file(&mut obj));

        CompilationArtifacts {
            module: (*self.module).clone(),
            obj,
            unwind_info: self.unwind_info.clone(),
            data_initializers: self.data_initializers.clone(),
            traps: self.traps.clone(),
            stack_maps: self.stack_maps.clone(),
            address_transform: self.address_transform.clone(),
            debug_info: self.code.dbg_jit_registration.is_some(),
        }
    }

    /// Crate an `Instance` from this `CompiledModule`.
    ///
    /// Note that if only one instance of this module is needed, it may be more
    /// efficient to call the top-level `instantiate`, since that avoids copying
    /// the data initializers.
    ///
    /// # Unsafety
    ///
    /// See `InstanceHandle::new`
    pub unsafe fn instantiate(
        &self,
        resolver: &mut dyn Resolver,
        signature_registry: &mut SignatureRegistry,
        mem_creator: Option<&dyn RuntimeMemoryCreator>,
        interrupts: Arc<VMInterrupts>,
        host_state: Box<dyn Any>,
        externref_activations_table: *mut VMExternRefActivationsTable,
        stack_map_registry: *mut StackMapRegistry,
    ) -> Result<InstanceHandle, InstantiationError> {
        // Compute indices into the shared signature table.
        let signatures = {
            self.module
                .local
                .signatures
                .values()
                .map(|(wasm_sig, native)| {
                    signature_registry.register(wasm_sig.clone(), native.clone())
                })
                .collect::<PrimaryMap<_, _>>()
        };

        let mut trampolines = HashMap::new();
        for (i, trampoline) in self.trampolines.iter() {
            trampolines.insert(signatures[i], trampoline.clone());
        }

        let finished_functions = self.finished_functions.0.clone();

        let imports = resolve_imports(&self.module, signature_registry, resolver)?;
        InstanceHandle::new(
            self.module.clone(),
            self.code.clone(),
            finished_functions,
            trampolines,
            imports,
            mem_creator,
            signatures.into_boxed_slice(),
            host_state,
            interrupts,
            externref_activations_table,
            stack_map_registry,
        )
    }

    /// Returns data initializers to pass to `InstanceHandle::initialize`
    pub fn data_initializers(&self) -> Vec<DataInitializer<'_>> {
        self.data_initializers
            .iter()
            .map(|init| DataInitializer {
                location: init.location.clone(),
                data: &*init.data,
            })
            .collect()
    }

    /// Return a reference-counting pointer to a module.
    pub fn module(&self) -> &Arc<Module> {
        &self.module
    }

    /// Return a reference to a mutable module (if possible).
    pub fn module_mut(&mut self) -> Option<&mut Module> {
        Arc::get_mut(&mut self.module)
    }

    /// Returns the map of all finished JIT functions compiled for this module
    pub fn finished_functions(&self) -> &BoxedSlice<DefinedFuncIndex, *mut [VMFunctionBody]> {
        &self.finished_functions.0
    }

    /// Returns the map for all traps in this module.
    pub fn traps(&self) -> &Traps {
        &self.traps
    }

    /// Returns the map for each of this module's stack maps.
    pub fn stack_maps(&self) -> &StackMaps {
        &self.stack_maps
    }

    /// Returns a map of compiled addresses back to original bytecode offsets.
    pub fn address_transform(&self) -> &ModuleAddressMap {
        &self.address_transform
    }

    /// Returns all ranges convered by JIT code.
    pub fn jit_code_ranges<'a>(&'a self) -> impl Iterator<Item = (usize, usize)> + 'a {
        self.code.code_memory.published_ranges()
    }

    /// Returns module's JIT code.
    pub fn code(&self) -> &Arc<ModuleCode> {
        &self.code
    }
}

/// Similar to `DataInitializer`, but owns its own copy of the data rather
/// than holding a slice of the original module.
#[derive(Clone, Serialize, Deserialize)]
pub struct OwnedDataInitializer {
    /// The location where the initialization is to be performed.
    location: DataInitializerLocation,

    /// The initialization data.
    data: Box<[u8]>,
}

impl OwnedDataInitializer {
    fn new(borrowed: DataInitializer<'_>) -> Self {
        Self {
            location: borrowed.location.clone(),
            data: borrowed.data.to_vec().into_boxed_slice(),
        }
    }
}

fn build_code_memory(
    isa: &dyn TargetIsa,
    obj: Vec<u8>,
    module: &Module,
    unwind_info: &Box<[ObjectUnwindInfo]>,
) -> Result<
    (
        CodeMemory,
        (*const u8, usize),
        PrimaryMap<DefinedFuncIndex, *mut [VMFunctionBody]>,
        PrimaryMap<SignatureIndex, VMTrampoline>,
    ),
    String,
> {
    let mut code_memory = CodeMemory::new();

    let mut allocation = code_memory.allocate_for_object(&obj, &unwind_info)?;

    // Second, create a PrimaryMap from result vector of pointers.
    let mut finished_functions = PrimaryMap::new();
    for (i, fat_ptr) in allocation.funcs() {
        let fat_ptr: *mut [VMFunctionBody] = fat_ptr;
        assert_eq!(
            Some(finished_functions.push(fat_ptr)),
            module.local.defined_func_index(i)
        );
    }

    let mut trampolines = PrimaryMap::new();
    for (i, fat_ptr) in allocation.trampolines() {
        let fat_ptr =
            unsafe { std::mem::transmute::<*const VMFunctionBody, VMTrampoline>(fat_ptr.as_ptr()) };
        assert_eq!(trampolines.push(fat_ptr), i);
    }

    let code_range = allocation.code_range();
    let code_range = (code_range.as_ptr(), code_range.len());

    let obj = allocation.as_mut_slice();
    link_module(obj, &module, &finished_functions)?;

    patch_loadable_file(obj, code_range).map_err(|e| e.to_string())?;

    let obj = (obj.as_ptr(), obj.len());

    // Make all code compiled thus far executable.
    // TODO publish only .text section of ELF object.
    code_memory.publish(isa);

    Ok((code_memory, obj, finished_functions, trampolines))
}
