//! Define the `instantiate` function, which takes a byte array containing an
//! encoded wasm module and returns a live wasm instance. Also, define
//! `CompiledModule` to allow compiling and instantiating to be done as separate
//! steps.

use crate::code_memory::CodeMemory;
use crate::compiler::Compiler;
use crate::imports::resolve_imports;
use crate::link::link_module;
use crate::resolver::Resolver;
use std::any::Any;
use std::collections::HashMap;
use std::io::Write;
use std::rc::Rc;
use std::sync::Arc;
use thiserror::Error;
use wasmtime_debug::read_debuginfo;
use wasmtime_environ::entity::{BoxedSlice, PrimaryMap};
use wasmtime_environ::wasm::{DefinedFuncIndex, SignatureIndex};
use wasmtime_environ::{
    CompileError, DataInitializer, DataInitializerLocation, Module, ModuleAddressMap,
    ModuleEnvironment, Traps,
};
use wasmtime_profiling::ProfilingAgent;
use wasmtime_runtime::VMInterrupts;
use wasmtime_runtime::{
    GdbJitImageRegistration, InstanceHandle, InstantiationError, RuntimeMemoryCreator,
    SignatureRegistry, VMFunctionBody, VMTrampoline,
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

/// This is similar to `CompiledModule`, but references the data initializers
/// from the wasm buffer rather than holding its own copy.
struct RawCompiledModule<'data> {
    code_memory: CodeMemory,
    module: Module,
    finished_functions: BoxedSlice<DefinedFuncIndex, *mut [VMFunctionBody]>,
    trampolines: PrimaryMap<SignatureIndex, VMTrampoline>,
    data_initializers: Box<[DataInitializer<'data>]>,
    dbg_jit_registration: Option<GdbJitImageRegistration>,
    traps: Traps,
    address_transform: ModuleAddressMap,
}

impl<'data> RawCompiledModule<'data> {
    /// Create a new `RawCompiledModule` by compiling the wasm module in `data` and instatiating it.
    fn new(
        compiler: &mut Compiler,
        data: &'data [u8],
        profiler: &dyn ProfilingAgent,
    ) -> Result<Self, SetupError> {
        let environ = ModuleEnvironment::new(compiler.frontend_config(), compiler.tunables());

        let translation = environ
            .translate(data)
            .map_err(|error| SetupError::Compile(CompileError::Wasm(error)))?;

        let mut debug_data = None;
        if compiler.tunables().debug_info {
            // TODO Do we want to ignore invalid DWARF data?
            debug_data = Some(read_debuginfo(&data)?);
        }

        let compilation = compiler.compile(&translation, debug_data)?;

        link_module(&translation.module, &compilation);

        // Make all code compiled thus far executable.
        let mut code_memory = compilation.code_memory;
        code_memory.publish(compiler.isa());

        // Initialize profiler and load the wasm module
        profiler.module_load(
            &translation.module,
            &compilation.finished_functions,
            compilation.dbg_image.as_deref(),
        );

        let dbg_jit_registration = if let Some(img) = compilation.dbg_image {
            let mut bytes = Vec::new();
            bytes.write_all(&img).expect("all written");
            let reg = GdbJitImageRegistration::register(bytes);
            Some(reg)
        } else {
            None
        };

        Ok(Self {
            code_memory,
            module: translation.module,
            finished_functions: compilation.finished_functions.into_boxed_slice(),
            trampolines: compilation.trampolines,
            data_initializers: translation.data_initializers.into_boxed_slice(),
            dbg_jit_registration,
            traps: compilation.traps,
            address_transform: compilation.address_transform,
        })
    }
}

/// A compiled wasm module, ready to be instantiated.
pub struct CompiledModule {
    code_memory: CodeMemory,
    module: Arc<Module>,
    finished_functions: BoxedSlice<DefinedFuncIndex, *mut [VMFunctionBody]>,
    trampolines: PrimaryMap<SignatureIndex, VMTrampoline>,
    data_initializers: Box<[OwnedDataInitializer]>,
    dbg_jit_registration: Option<Rc<GdbJitImageRegistration>>,
    traps: Traps,
    address_transform: ModuleAddressMap,
    interrupts: Arc<VMInterrupts>,
}

impl CompiledModule {
    /// Compile a data buffer into a `CompiledModule`, which may then be instantiated.
    pub fn new<'data>(
        compiler: &mut Compiler,
        data: &'data [u8],
        profiler: &dyn ProfilingAgent,
    ) -> Result<Self, SetupError> {
        let raw = RawCompiledModule::<'data>::new(compiler, data, profiler)?;

        Ok(Self::from_parts(
            raw.code_memory,
            raw.module,
            raw.finished_functions,
            raw.trampolines,
            raw.data_initializers
                .iter()
                .map(OwnedDataInitializer::new)
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            raw.dbg_jit_registration,
            raw.traps,
            raw.address_transform,
            compiler.interrupts().clone(),
        ))
    }

    /// Construct a `CompiledModule` from component parts.
    pub fn from_parts(
        code_memory: CodeMemory,
        module: Module,
        finished_functions: BoxedSlice<DefinedFuncIndex, *mut [VMFunctionBody]>,
        trampolines: PrimaryMap<SignatureIndex, VMTrampoline>,
        data_initializers: Box<[OwnedDataInitializer]>,
        dbg_jit_registration: Option<GdbJitImageRegistration>,
        traps: Traps,
        address_transform: ModuleAddressMap,
        interrupts: Arc<VMInterrupts>,
    ) -> Self {
        Self {
            code_memory,
            module: Arc::new(module),
            finished_functions,
            trampolines,
            data_initializers,
            dbg_jit_registration: dbg_jit_registration.map(Rc::new),
            traps,
            address_transform,
            interrupts,
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
        signature_registry: &SignatureRegistry,
        mem_creator: Option<&dyn RuntimeMemoryCreator>,
        host_state: Box<dyn Any>,
    ) -> Result<InstanceHandle, InstantiationError> {
        // Compute indices into the shared signature table.
        let signatures = {
            self.module
                .local
                .signatures
                .values()
                .map(|sig| signature_registry.register(sig))
                .collect::<PrimaryMap<_, _>>()
        };

        let mut trampolines = HashMap::new();
        for (i, trampoline) in self.trampolines.iter() {
            trampolines.insert(signatures[i], trampoline.clone());
        }

        let imports = resolve_imports(&self.module, signature_registry, resolver)?;
        InstanceHandle::new(
            Arc::clone(&self.module),
            self.finished_functions.clone(),
            trampolines,
            imports,
            mem_creator,
            signatures.into_boxed_slice(),
            self.dbg_jit_registration.as_ref().map(|r| Rc::clone(&r)),
            host_state,
            self.interrupts.clone(),
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

    /// Return a reference-counting pointer to a module.
    pub fn module_mut(&mut self) -> &mut Arc<Module> {
        &mut self.module
    }

    /// Return a reference to a module.
    pub fn module_ref(&self) -> &Module {
        &self.module
    }

    /// Returns the map of all finished JIT functions compiled for this module
    pub fn finished_functions(&self) -> &BoxedSlice<DefinedFuncIndex, *mut [VMFunctionBody]> {
        &self.finished_functions
    }

    /// Returns the a map for all traps in this module.
    pub fn traps(&self) -> &Traps {
        &self.traps
    }

    /// Returns a map of compiled addresses back to original bytecode offsets.
    pub fn address_transform(&self) -> &ModuleAddressMap {
        &self.address_transform
    }

    /// Returns all ranges convered by JIT code.
    pub fn jit_code_ranges<'a>(&'a self) -> impl Iterator<Item = (usize, usize)> + 'a {
        self.code_memory.published_ranges()
    }
}

/// Similar to `DataInitializer`, but owns its own copy of the data rather
/// than holding a slice of the original module.
pub struct OwnedDataInitializer {
    /// The location where the initialization is to be performed.
    location: DataInitializerLocation,

    /// The initialization data.
    data: Box<[u8]>,
}

impl OwnedDataInitializer {
    fn new(borrowed: &DataInitializer<'_>) -> Self {
        Self {
            location: borrowed.location.clone(),
            data: borrowed.data.to_vec().into_boxed_slice(),
        }
    }
}
