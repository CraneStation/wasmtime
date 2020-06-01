//! JIT compilation.

use crate::code_memory::CodeMemory;
use crate::instantiate::SetupError;
use cranelift_codegen::ir::ExternalName;
use cranelift_codegen::ir::InstBuilder;
use cranelift_codegen::print_errors::pretty_error;
use cranelift_codegen::Context;
use cranelift_codegen::{binemit, ir};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use std::collections::HashMap;
use wasmtime_debug::{emit_debugsections_image, DebugInfoData};
use wasmtime_environ::entity::{EntityRef, PrimaryMap};
use wasmtime_environ::isa::{TargetFrontendConfig, TargetIsa};
use wasmtime_environ::wasm::{DefinedFuncIndex, DefinedMemoryIndex, MemoryIndex, SignatureIndex};
use wasmtime_environ::{
    CacheConfig, CompileError, CompiledFunction, Compiler as _C, ModuleAddressMap,
    ModuleMemoryOffset, ModuleTranslation, ModuleVmctxInfo, Relocation, RelocationTarget,
    Relocations, Traps, Tunables, VMOffsets,
};
use wasmtime_runtime::{InstantiationError, VMFunctionBody, VMTrampoline};

/// Select which kind of compilation to use.
#[derive(Copy, Clone, Debug)]
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
    cache_config: CacheConfig,
    tunables: Tunables,
}

impl Compiler {
    /// Construct a new `Compiler`.
    pub fn new(
        isa: Box<dyn TargetIsa>,
        strategy: CompilationStrategy,
        cache_config: CacheConfig,
        tunables: Tunables,
    ) -> Self {
        Self {
            isa,
            strategy,
            cache_config,
            tunables,
        }
    }
}

fn _assert_compiler_send_sync() {
    fn _assert<T: Send + Sync>() {}
    _assert::<Compiler>();
}

#[allow(missing_docs)]
pub struct Compilation {
    pub code_memory: CodeMemory,
    pub finished_functions: PrimaryMap<DefinedFuncIndex, *mut [VMFunctionBody]>,
    pub relocations: Relocations,
    pub trampolines: PrimaryMap<SignatureIndex, VMTrampoline>,
    pub trampoline_relocations: HashMap<SignatureIndex, Vec<Relocation>>,
    pub jt_offsets: PrimaryMap<DefinedFuncIndex, ir::JumpTableOffsets>,
    pub dbg_image: Option<Vec<u8>>,
    pub traps: Traps,
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

    /// Compile the given function bodies.
    pub(crate) fn compile<'data>(
        &self,
        translation: &ModuleTranslation,
        debug_data: Option<DebugInfoData>,
    ) -> Result<Compilation, SetupError> {
        let mut code_memory = CodeMemory::new();

        let (compilation, relocations, address_transform, value_ranges, stack_slots, traps) =
            match self.strategy {
                // For now, interpret `Auto` as `Cranelift` since that's the most stable
                // implementation.
                CompilationStrategy::Auto | CompilationStrategy::Cranelift => {
                    wasmtime_environ::cranelift::Cranelift::compile_module(
                        translation,
                        &*self.isa,
                        &self.cache_config,
                    )
                }
                #[cfg(feature = "lightbeam")]
                CompilationStrategy::Lightbeam => {
                    wasmtime_environ::lightbeam::Lightbeam::compile_module(
                        translation,
                        &*self.isa,
                        &self.cache_config,
                    )
                }
            }
            .map_err(SetupError::Compile)?;

        // Allocate all of the compiled functions into executable memory,
        // copying over their contents.
        let finished_functions =
            allocate_functions(&mut code_memory, &compilation).map_err(|message| {
                SetupError::Instantiate(InstantiationError::Resource(format!(
                    "failed to allocate memory for functions: {}",
                    message
                )))
            })?;

        // Eagerly generate a entry trampoline for every type signature in the
        // module. This should be "relatively lightweight" for most modules and
        // guarantees that all functions (including indirect ones through
        // tables) have a trampoline when invoked through the wasmtime API.
        let mut cx = FunctionBuilderContext::new();
        let mut trampolines = PrimaryMap::new();
        let mut trampoline_relocations = HashMap::new();
        for (index, sig) in translation.module.local.signatures.iter() {
            let (trampoline, relocations) = make_trampoline(
                &*self.isa,
                &mut code_memory,
                &mut cx,
                sig,
                std::mem::size_of::<u128>(),
            )?;
            trampolines.push(trampoline);

            // Typically trampolines do not have relocations, so if one does
            // show up be sure to log it in case anyone's listening and there's
            // an accidental bug.
            if relocations.len() > 0 {
                log::info!("relocations found in trampoline for {:?}", sig);
                trampoline_relocations.insert(index, relocations);
            }
        }

        // Translate debug info (DWARF) only if at least one function is present.
        let dbg_image = if debug_data.is_some() && !finished_functions.is_empty() {
            let target_config = self.isa.frontend_config();
            let ofs = VMOffsets::new(target_config.pointer_bytes(), &translation.module.local);

            let mut funcs = Vec::new();
            for (i, allocated) in finished_functions.into_iter() {
                let ptr = (*allocated) as *const u8;
                let body_len = compilation.get(i).body.len();
                funcs.push((ptr, body_len));
            }
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
            let bytes = emit_debugsections_image(
                &*self.isa,
                debug_data.as_ref().unwrap(),
                &module_vmctx_info,
                &address_transform,
                &value_ranges,
                &funcs,
                &compilation,
            )
            .map_err(SetupError::DebugInfo)?;
            Some(bytes)
        } else {
            None
        };

        let jt_offsets = compilation.get_jt_offsets();

        Ok(Compilation {
            code_memory,
            finished_functions,
            relocations,
            trampolines,
            trampoline_relocations,
            jt_offsets,
            dbg_image,
            traps,
            address_transform,
        })
    }
}

/// Create a trampoline for invoking a function.
pub fn make_trampoline(
    isa: &dyn TargetIsa,
    code_memory: &mut CodeMemory,
    fn_builder_ctx: &mut FunctionBuilderContext,
    signature: &ir::Signature,
    value_size: usize,
) -> Result<(VMTrampoline, Vec<Relocation>), SetupError> {
    let pointer_type = isa.pointer_type();
    let mut wrapper_sig = ir::Signature::new(isa.frontend_config().default_call_conv);

    // Add the callee `vmctx` parameter.
    wrapper_sig.params.push(ir::AbiParam::special(
        pointer_type,
        ir::ArgumentPurpose::VMContext,
    ));

    // Add the caller `vmctx` parameter.
    wrapper_sig.params.push(ir::AbiParam::new(pointer_type));

    // Add the `callee_address` parameter.
    wrapper_sig.params.push(ir::AbiParam::new(pointer_type));

    // Add the `values_vec` parameter.
    wrapper_sig.params.push(ir::AbiParam::new(pointer_type));

    let mut context = Context::new();
    context.func = ir::Function::with_name_signature(ir::ExternalName::user(0, 0), wrapper_sig);

    {
        let mut builder = FunctionBuilder::new(&mut context.func, fn_builder_ctx);
        let block0 = builder.create_block();

        builder.append_block_params_for_function_params(block0);
        builder.switch_to_block(block0);
        builder.seal_block(block0);

        let (vmctx_ptr_val, caller_vmctx_ptr_val, callee_value, values_vec_ptr_val) = {
            let params = builder.func.dfg.block_params(block0);
            (params[0], params[1], params[2], params[3])
        };

        // Load the argument values out of `values_vec`.
        let mflags = ir::MemFlags::trusted();
        let callee_args = signature
            .params
            .iter()
            .enumerate()
            .map(|(i, r)| {
                match i {
                    0 => vmctx_ptr_val,
                    1 => caller_vmctx_ptr_val,
                    _ =>
                    // i - 2 because vmctx and caller vmctx aren't passed through `values_vec`.
                    {
                        builder.ins().load(
                            r.value_type,
                            mflags,
                            values_vec_ptr_val,
                            ((i - 2) * value_size) as i32,
                        )
                    }
                }
            })
            .collect::<Vec<_>>();

        let new_sig = builder.import_signature(signature.clone());

        let call = builder
            .ins()
            .call_indirect(new_sig, callee_value, &callee_args);

        let results = builder.func.dfg.inst_results(call).to_vec();

        // Store the return values into `values_vec`.
        let mflags = ir::MemFlags::trusted();
        for (i, r) in results.iter().enumerate() {
            builder
                .ins()
                .store(mflags, *r, values_vec_ptr_val, (i * value_size) as i32);
        }

        builder.ins().return_(&[]);
        builder.finalize()
    }

    let mut code_buf = Vec::new();
    let mut reloc_sink = RelocSink::default();
    let mut trap_sink = binemit::NullTrapSink {};
    let mut stackmap_sink = binemit::NullStackmapSink {};
    context
        .compile_and_emit(
            isa,
            &mut code_buf,
            &mut reloc_sink,
            &mut trap_sink,
            &mut stackmap_sink,
        )
        .map_err(|error| {
            SetupError::Compile(CompileError::Codegen(pretty_error(
                &context.func,
                Some(isa),
                error,
            )))
        })?;

    let unwind_info = context.create_unwind_info(isa).map_err(|error| {
        SetupError::Compile(CompileError::Codegen(pretty_error(
            &context.func,
            Some(isa),
            error,
        )))
    })?;

    let ptr = code_memory
        .allocate_for_function(&CompiledFunction {
            body: code_buf,
            jt_offsets: context.func.jt_offsets,
            unwind_info,
        })
        .map_err(|message| SetupError::Instantiate(InstantiationError::Resource(message)))?
        .as_ptr();
    Ok((
        unsafe { std::mem::transmute::<*const VMFunctionBody, VMTrampoline>(ptr) },
        reloc_sink.relocs,
    ))
}

fn allocate_functions(
    code_memory: &mut CodeMemory,
    compilation: &wasmtime_environ::Compilation,
) -> Result<PrimaryMap<DefinedFuncIndex, *mut [VMFunctionBody]>, String> {
    if compilation.is_empty() {
        return Ok(PrimaryMap::new());
    }

    let fat_ptrs = code_memory.allocate_for_compilation(compilation)?;

    // Second, create a PrimaryMap from result vector of pointers.
    let mut result = PrimaryMap::with_capacity(compilation.len());
    for i in 0..fat_ptrs.len() {
        let fat_ptr: *mut [VMFunctionBody] = fat_ptrs[i];
        result.push(fat_ptr);
    }

    Ok(result)
}

/// We don't expect trampoline compilation to produce many relocations, so
/// this `RelocSink` just asserts that it doesn't recieve most of them, but
/// handles libcall ones.
#[derive(Default)]
struct RelocSink {
    relocs: Vec<Relocation>,
}

impl binemit::RelocSink for RelocSink {
    fn reloc_block(
        &mut self,
        _offset: binemit::CodeOffset,
        _reloc: binemit::Reloc,
        _block_offset: binemit::CodeOffset,
    ) {
        panic!("trampoline compilation should not produce block relocs");
    }
    fn reloc_external(
        &mut self,
        offset: binemit::CodeOffset,
        _srcloc: ir::SourceLoc,
        reloc: binemit::Reloc,
        name: &ir::ExternalName,
        addend: binemit::Addend,
    ) {
        let reloc_target = if let ExternalName::LibCall(libcall) = *name {
            RelocationTarget::LibCall(libcall)
        } else {
            panic!("unrecognized external name")
        };
        self.relocs.push(Relocation {
            reloc,
            reloc_target,
            offset,
            addend,
        });
    }
    fn reloc_constant(
        &mut self,
        _code_offset: binemit::CodeOffset,
        _reloc: binemit::Reloc,
        _constant_offset: ir::ConstantOffset,
    ) {
        panic!("trampoline compilation should not produce constant relocs");
    }
    fn reloc_jt(
        &mut self,
        _offset: binemit::CodeOffset,
        _reloc: binemit::Reloc,
        _jt: ir::JumpTable,
    ) {
        panic!("trampoline compilation should not produce jump table relocs");
    }
}
