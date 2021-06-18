//! Provides functionality for compiling and running CLIF IR for `run` tests.
use core::{mem, ptr};
use cranelift_codegen::binemit::{NullRelocSink, NullStackMapSink, NullTrapSink};
use cranelift_codegen::data_value::DataValue;
use cranelift_codegen::ir::immediates::{Ieee32, Ieee64};
use cranelift_codegen::ir::{condcodes::IntCC, Function, InstBuilder, Signature, Type};
use cranelift_codegen::isa::TargetIsa;
use cranelift_codegen::{ir, settings, CodegenError, Context};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_native::builder_with_options;
use log::trace;
use memmap2::{Mmap, MmapMut};
use std::cmp::max;
use std::collections::HashMap;
use thiserror::Error;

/// Compile a single function.
///
/// Several Cranelift functions need the ability to run Cranelift IR (e.g. `test_run`); this
/// [SingleFunctionCompiler] provides a way for compiling Cranelift [Function]s to
/// `CompiledFunction`s and subsequently calling them through the use of a `Trampoline`. As its
/// name indicates, this compiler is limited: any functionality that requires knowledge of things
/// outside the [Function] will likely not work (e.g. global values, calls). For an example of this
/// "outside-of-function" functionality, see `cranelift_jit::backend::JITBackend`.
///
/// ```
/// use cranelift_filetests::SingleFunctionCompiler;
/// use cranelift_reader::parse_functions;
///
/// let code = "test run \n function %add(i32, i32) -> i32 {  block0(v0:i32, v1:i32):  v2 = iadd v0, v1  return v2 }".into();
/// let func = parse_functions(code).unwrap().into_iter().nth(0).unwrap();
/// let mut compiler = SingleFunctionCompiler::with_default_host_isa();
/// let compiled_func = compiler.compile(func).unwrap();
/// println!("Address of compiled function: {:p}", compiled_func.as_ptr());
/// ```
pub struct SingleFunctionCompiler {
    isa: Box<dyn TargetIsa>,
    trampolines: HashMap<Signature, Trampoline>,
}

impl SingleFunctionCompiler {
    /// Build a [SingleFunctionCompiler] from a [TargetIsa]. For functions to be runnable on the
    /// host machine, this [TargetIsa] must match the host machine's ISA (see
    /// [SingleFunctionCompiler::with_host_isa]).
    pub fn new(isa: Box<dyn TargetIsa>) -> Self {
        let trampolines = HashMap::new();
        Self { isa, trampolines }
    }

    /// Build a [SingleFunctionCompiler] using the host machine's ISA and the passed flags.
    pub fn with_host_isa(flags: settings::Flags) -> Self {
        let builder = builder_with_options(true)
            .expect("Unable to build a TargetIsa for the current host");
        let isa = builder.finish(flags);
        Self::new(isa)
    }

    /// Build a [SingleFunctionCompiler] using the host machine's ISA and the default flags for this
    /// ISA.
    pub fn with_default_host_isa() -> Self {
        let flags = settings::Flags::new(settings::builder());
        Self::with_host_isa(flags)
    }

    /// Compile the passed [Function] to a `CompiledFunction`. This function will:
    ///  - check that the default ISA calling convention is used (to ensure it can be called)
    ///  - compile the [Function]
    ///  - compile a `Trampoline` for the [Function]'s signature (or used a cached `Trampoline`;
    ///    this makes it possible to call functions when the signature is not known until runtime.
    pub fn compile(&mut self, function: Function) -> Result<CompiledFunction, CompilationError> {
        let signature = function.signature.clone();
        if signature.call_conv != self.isa.default_call_conv() {
            return Err(CompilationError::InvalidTargetIsa);
        }

        // Compile the function itself.
        let code_page = compile(function, self.isa.as_ref())?;

        // Compile the trampoline to call it, if necessary (it may be cached).
        let isa = self.isa.as_ref();
        let trampoline = self
            .trampolines
            .entry(signature.clone())
            .or_insert_with(|| {
                let ir = make_trampoline(&signature, isa);
                let code = compile(ir, isa).expect("failed to compile trampoline");
                Trampoline::new(code)
            });

        Ok(CompiledFunction::new(code_page, signature, trampoline))
    }
}

#[derive(Error, Debug)]
pub enum CompilationError {
    #[error("Cross-compilation not currently supported; use the host's default calling convention \
    or remove the specified calling convention in the function signature to use the host's default.")]
    InvalidTargetIsa,
    #[error("Cranelift codegen error")]
    CodegenError(#[from] CodegenError),
    #[error("Memory mapping error")]
    IoError(#[from] std::io::Error),
}

/// Contains the compiled code to move memory-allocated [DataValue]s to the correct location (e.g.
/// register, stack) dictated by the calling convention before calling a [CompiledFunction]. Without
/// this, it would be quite difficult to correctly place [DataValue]s since both the calling
/// convention and function signature are not known until runtime. See [make_trampoline] for the
/// Cranelift IR used to build this.
pub struct Trampoline {
    page: Mmap,
}

impl Trampoline {
    /// Build a new [Trampoline].
    pub fn new(page: Mmap) -> Self {
        Self { page }
    }

    /// Return a pointer to the compiled code.
    fn as_ptr(&self) -> *const u8 {
        self.page.as_ptr()
    }
}

/// Container for the compiled code of a [Function]. This wrapper allows users to call the compiled
/// function through the use of a [Trampoline].
///
/// ```
/// use cranelift_filetests::SingleFunctionCompiler;
/// use cranelift_reader::parse_functions;
/// use cranelift_codegen::data_value::DataValue;
///
/// let code = "test run \n function %add(i32, i32) -> i32 {  block0(v0:i32, v1:i32):  v2 = iadd v0, v1  return v2 }".into();
/// let func = parse_functions(code).unwrap().into_iter().nth(0).unwrap();
/// let mut compiler = SingleFunctionCompiler::with_default_host_isa();
/// let compiled_func = compiler.compile(func).unwrap();
///
/// let returned = compiled_func.call(&vec![DataValue::I32(2), DataValue::I32(40)]);
/// assert_eq!(vec![DataValue::I32(42)], returned);
/// ```
pub struct CompiledFunction<'a> {
    page: Mmap,
    signature: Signature,
    trampoline: &'a Trampoline,
}

impl<'a> CompiledFunction<'a> {
    /// Build a new [CompiledFunction].
    pub fn new(page: Mmap, signature: Signature, trampoline: &'a Trampoline) -> Self {
        Self {
            page,
            signature,
            trampoline,
        }
    }

    /// Return a pointer to the compiled code.
    pub fn as_ptr(&self) -> *const u8 {
        self.page.as_ptr()
    }

    /// Call the [CompiledFunction], passing in [DataValue]s using a compiled [Trampoline].
    pub fn call(&self, arguments: &[DataValue]) -> Vec<DataValue> {
        let mut values = UnboxedValues::make_arguments(arguments, &self.signature);
        let arguments_address = values.as_mut_ptr();
        let function_address = self.as_ptr();

        let callable_trampoline: fn(*const u8, *mut u128) -> () =
            unsafe { mem::transmute(self.trampoline.as_ptr()) };
        callable_trampoline(function_address, arguments_address);

        values.collect_returns(&self.signature)
    }
}

/// A container for laying out the [ValueData]s in memory in a way that the [Trampoline] can
/// understand.
struct UnboxedValues(Vec<u128>);

impl UnboxedValues {
    /// The size in bytes of each slot location in the allocated [DataValue]s. Though [DataValue]s
    /// could be smaller than 16 bytes (e.g. `I16`), this simplifies the creation of the [DataValue]
    /// array and could be used to align the slots to the largest used [DataValue] (i.e. 128-bit
    /// vectors).
    const SLOT_SIZE: usize = 16;

    /// Build the arguments vector for passing the [DataValue]s into the [Trampoline]. The size of
    /// `u128` used here must match [Trampoline::SLOT_SIZE].
    pub fn make_arguments(arguments: &[DataValue], signature: &ir::Signature) -> Self {
        assert_eq!(arguments.len(), signature.params.len());
        let mut values_vec = vec![0; max(signature.params.len(), signature.returns.len())];

        // Store the argument values into `values_vec`.
        for ((arg, slot), param) in arguments.iter().zip(&mut values_vec).zip(&signature.params) {
            assert!(
                arg.ty() == param.value_type || arg.is_vector(),
                "argument type mismatch: {} != {}",
                arg.ty(),
                param.value_type
            );
            unsafe {
                Self::write_value_to(arg, slot);
            }
        }

        Self(values_vec)
    }

    /// Return a pointer to the underlying memory for passing to the trampoline.
    pub fn as_mut_ptr(&mut self) -> *mut u128 {
        self.0.as_mut_ptr()
    }

    /// Collect the returned [DataValue]s into a [Vec]. The size of `u128` used here must match
    /// [Trampoline::SLOT_SIZE].
    pub fn collect_returns(&self, signature: &ir::Signature) -> Vec<DataValue> {
        assert!(self.0.len() >= signature.returns.len());
        let mut returns = Vec::with_capacity(signature.returns.len());

        // Extract the returned values from this vector.
        for (slot, param) in self.0.iter().zip(&signature.returns) {
            let value = unsafe { Self::read_value_from(slot, param.value_type) };
            returns.push(value);
        }

        returns
    }

    /// Write a [DataValue] to a memory location.
    unsafe fn write_value_to(v: &DataValue, p: *mut u128) {
        match v {
            DataValue::B(b) => ptr::write(p as *mut bool, *b),
            DataValue::I8(i) => ptr::write(p as *mut i8, *i),
            DataValue::I16(i) => ptr::write(p as *mut i16, *i),
            DataValue::I32(i) => ptr::write(p as *mut i32, *i),
            DataValue::I64(i) => ptr::write(p as *mut i64, *i),
            DataValue::F32(f) => ptr::write(p as *mut Ieee32, *f),
            DataValue::F64(f) => ptr::write(p as *mut Ieee64, *f),
            DataValue::V128(b) => ptr::write(p as *mut [u8; 16], *b),
            _ => unimplemented!(),
        }
    }

    /// Read a [DataValue] from a memory location using a given [Type].
    unsafe fn read_value_from(p: *const u128, ty: Type) -> DataValue {
        match ty {
            ir::types::I8 => DataValue::I8(ptr::read(p as *const i8)),
            ir::types::I16 => DataValue::I16(ptr::read(p as *const i16)),
            ir::types::I32 => DataValue::I32(ptr::read(p as *const i32)),
            ir::types::I64 => DataValue::I64(ptr::read(p as *const i64)),
            ir::types::F32 => DataValue::F32(ptr::read(p as *const Ieee32)),
            ir::types::F64 => DataValue::F64(ptr::read(p as *const Ieee64)),
            _ if ty.is_bool() => match ty.bytes() {
                1 => DataValue::B(ptr::read(p as *const i8) != 0),
                2 => DataValue::B(ptr::read(p as *const i16) != 0),
                4 => DataValue::B(ptr::read(p as *const i32) != 0),
                8 => DataValue::B(ptr::read(p as *const i64) != 0),
                _ => unimplemented!(),
            },
            _ if ty.is_vector() && ty.bytes() == 16 => {
                DataValue::V128(ptr::read(p as *const [u8; 16]))
            }
            _ => unimplemented!(),
        }
    }
}

/// Compile a [Function] to its executable bytes in memory.
///
/// This currently returns a [Mmap], a type from an external crate, so we wrap this up before
/// exposing it in public APIs.
fn compile(function: Function, isa: &dyn TargetIsa) -> Result<Mmap, CompilationError> {
    // Set up the context.
    let mut context = Context::new();
    context.func = function;

    // Compile and encode the result to machine code.
    let relocs = &mut NullRelocSink {};
    let traps = &mut NullTrapSink {};
    let stack_maps = &mut NullStackMapSink {};
    let code_info = context.compile(isa)?;
    let mut code_page = MmapMut::map_anon(code_info.total_size as usize)?;

    unsafe {
        context.emit_to_memory(isa, code_page.as_mut_ptr(), relocs, traps, stack_maps);
    };

    let code_page = code_page.make_exec()?;
    trace!(
        "Compiled function {} with signature {} at: {:p}",
        context.func.name,
        context.func.signature,
        code_page.as_ptr()
    );

    Ok(code_page)
}

/// Build the Cranelift IR for moving the memory-allocated [DataValue]s to their correct location
/// (e.g. register, stack) prior to calling a [CompiledFunction]. The [Function] returned by
/// [make_trampoline] is compiled to a [Trampoline]. Note that this uses the [TargetIsa]'s default
/// calling convention so we must also check that the [CompiledFunction] has the same calling
/// convention (see [SingleFunctionCompiler::compile]).
fn make_trampoline(signature: &ir::Signature, isa: &dyn TargetIsa) -> Function {
    // Create the trampoline signature: (callee_address: pointer, values_vec: pointer) -> ()
    let pointer_type = isa.pointer_type();
    let mut wrapper_sig = ir::Signature::new(isa.frontend_config().default_call_conv);
    wrapper_sig.params.push(ir::AbiParam::new(pointer_type)); // Add the `callee_address` parameter.
    wrapper_sig.params.push(ir::AbiParam::new(pointer_type)); // Add the `values_vec` parameter.

    let mut func = ir::Function::with_name_signature(ir::ExternalName::user(0, 0), wrapper_sig);

    // The trampoline has a single block filled with loads, one call to callee_address, and some loads.
    let mut builder_context = FunctionBuilderContext::new();
    let mut builder = FunctionBuilder::new(&mut func, &mut builder_context);
    let block0 = builder.create_block();
    builder.append_block_params_for_function_params(block0);
    builder.switch_to_block(block0);
    builder.seal_block(block0);

    // Extract the incoming SSA values.
    let (callee_value, values_vec_ptr_val) = {
        let params = builder.func.dfg.block_params(block0);
        (params[0], params[1])
    };

    // Load the argument values out of `values_vec`.
    let callee_args = signature
        .params
        .iter()
        .enumerate()
        .map(|(i, param)| {
            // Calculate the type to load from memory, using integers for booleans (no encodings).
            let ty = if param.value_type.is_bool() {
                Type::int(max(param.value_type.bits(), 8)).expect(
                    "to be able to convert any boolean type to its equal-width integer type",
                )
            } else {
                param.value_type
            };
            // Load the value.
            let loaded = builder.ins().load(
                ty,
                ir::MemFlags::trusted(),
                values_vec_ptr_val,
                (i * UnboxedValues::SLOT_SIZE) as i32,
            );
            // For booleans, we want to type-convert the loaded integer into a boolean and ensure
            // that we are using the architecture's canonical boolean representation (presumably
            // comparison will emit this).
            if param.value_type.is_bool() {
                builder.ins().icmp_imm(IntCC::NotEqual, loaded, 0)
            } else {
                loaded
            }
        })
        .collect::<Vec<_>>();

    // Call the passed function.
    let new_sig = builder.import_signature(signature.clone());
    let call = builder
        .ins()
        .call_indirect(new_sig, callee_value, &callee_args);

    // Store the return values into `values_vec`.
    let results = builder.func.dfg.inst_results(call).to_vec();
    for ((i, value), param) in results.iter().enumerate().zip(&signature.returns) {
        // Before storing return values, we convert booleans to their integer representation.
        let value = if param.value_type.is_bool() {
            let ty = Type::int(max(param.value_type.bits(), 8))
                .expect("to be able to convert any boolean type to its equal-width integer type");
            builder.ins().bint(ty, *value)
        } else {
            *value
        };
        // Store the value.
        builder.ins().store(
            ir::MemFlags::trusted(),
            value,
            values_vec_ptr_val,
            (i * UnboxedValues::SLOT_SIZE) as i32,
        );
    }

    builder.ins().return_(&[]);
    builder.finalize();

    func
}

#[cfg(test)]
mod test {
    use super::*;
    use cranelift_reader::{parse_functions, parse_test, ParseOptions};

    fn parse(code: &str) -> Function {
        parse_functions(code).unwrap().into_iter().nth(0).unwrap()
    }

    #[test]
    fn nop() {
        let code = String::from(
            "
            test run
            function %test() -> b8 {
            block0:
                nop
                v1 = bconst.b8 true
                return v1
            }",
        );

        // extract function
        let test_file = parse_test(code.as_str(), ParseOptions::default()).unwrap();
        assert_eq!(1, test_file.functions.len());
        let function = test_file.functions[0].0.clone();

        // execute function
        let mut compiler = SingleFunctionCompiler::with_default_host_isa();
        let compiled_function = compiler.compile(function).unwrap();
        let returned = compiled_function.call(&[]);
        assert_eq!(returned, vec![DataValue::B(true)])
    }

    #[test]
    fn trampolines() {
        let function = parse(
            "
            function %test(f32, i8, i64x2, b1) -> f32x4, b64 {
            block0(v0: f32, v1: i8, v2: i64x2, v3: b1):
                v4 = vconst.f32x4 [0x0.1 0x0.2 0x0.3 0x0.4]
                v5 = bconst.b64 true
                return v4, v5
            }",
        );

        let compiler = SingleFunctionCompiler::with_default_host_isa();
        let trampoline = make_trampoline(&function.signature, compiler.isa.as_ref());
        assert!(format!("{}", trampoline).ends_with(
            "sig0 = (f32, i8, i64x2, b1) -> f32x4, b64 fast

block0(v0: i64, v1: i64):
    v2 = load.f32 notrap aligned v1
    v3 = load.i8 notrap aligned v1+16
    v4 = load.i64x2 notrap aligned v1+32
    v5 = load.i8 notrap aligned v1+48
    v6 = icmp_imm ne v5, 0
    v7, v8 = call_indirect sig0, v0(v2, v3, v4, v6)
    store notrap aligned v7, v1
    v9 = bint.i64 v8
    store notrap aligned v9, v1+16
    return
}
"
        ));
    }
}
