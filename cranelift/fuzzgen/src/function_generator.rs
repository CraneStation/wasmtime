use crate::config::Config;
use anyhow::Result;
use arbitrary::Unstructured;
use cranelift::codegen::ir::types::*;
use cranelift::codegen::ir::{
    AbiParam, Block, ExternalName, Function, Opcode, Signature, Type, Value,
};
use cranelift::codegen::isa::CallConv;
use cranelift::frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift::prelude::{EntityRef, InstBuilder};

type BlockSignature = Vec<Type>;

fn insert_opcode_arity_0(
    _fgen: &mut FunctionGenerator,
    builder: &mut FunctionBuilder,
    opcode: Opcode,
    _args: &'static [Type],
    _rets: &'static [Type],
) -> Result<()> {
    builder.ins().NullAry(opcode, INVALID);
    Ok(())
}

fn insert_opcode_arity_2(
    fgen: &mut FunctionGenerator,
    builder: &mut FunctionBuilder,
    opcode: Opcode,
    args: &'static [Type],
    rets: &'static [Type],
) -> Result<()> {
    let arg0 = fgen.get_variable_of_type(args[0])?;
    let arg0 = builder.use_var(arg0);

    let arg1 = fgen.get_variable_of_type(args[1])?;
    let arg1 = builder.use_var(arg1);

    let typevar = rets[0];
    let (inst, dfg) = builder.ins().Binary(opcode, typevar, arg0, arg1);
    let results = dfg.inst_results(inst).to_vec();

    for (val, ty) in results.into_iter().zip(rets) {
        let var = fgen.get_variable_of_type(*ty)?;
        builder.def_var(var, val);
    }
    Ok(())
}

type OpcodeInserter = fn(
    fgen: &mut FunctionGenerator,
    builder: &mut FunctionBuilder,
    Opcode,
    &'static [Type],
    &'static [Type],
) -> Result<()>;

// TODO: Derive this from the `cranelift-meta` generator.
const OPCODE_SIGNATURES: &'static [(
    Opcode,
    &'static [Type], // Args
    &'static [Type], // Rets
    OpcodeInserter,
)] = &[
    (Opcode::Nop, &[], &[], insert_opcode_arity_0),
    // Iadd
    (Opcode::Iadd, &[I8, I8], &[I8], insert_opcode_arity_2),
    (Opcode::Iadd, &[I16, I16], &[I16], insert_opcode_arity_2),
    (Opcode::Iadd, &[I32, I32], &[I32], insert_opcode_arity_2),
    (Opcode::Iadd, &[I64, I64], &[I64], insert_opcode_arity_2),
    // Isub
    (Opcode::Isub, &[I8, I8], &[I8], insert_opcode_arity_2),
    (Opcode::Isub, &[I16, I16], &[I16], insert_opcode_arity_2),
    (Opcode::Isub, &[I32, I32], &[I32], insert_opcode_arity_2),
    (Opcode::Isub, &[I64, I64], &[I64], insert_opcode_arity_2),
    // Imul
    (Opcode::Imul, &[I8, I8], &[I8], insert_opcode_arity_2),
    (Opcode::Imul, &[I16, I16], &[I16], insert_opcode_arity_2),
    (Opcode::Imul, &[I32, I32], &[I32], insert_opcode_arity_2),
    (Opcode::Imul, &[I64, I64], &[I64], insert_opcode_arity_2),
    // Udiv
    (Opcode::Udiv, &[I8, I8], &[I8], insert_opcode_arity_2),
    (Opcode::Udiv, &[I16, I16], &[I16], insert_opcode_arity_2),
    (Opcode::Udiv, &[I32, I32], &[I32], insert_opcode_arity_2),
    (Opcode::Udiv, &[I64, I64], &[I64], insert_opcode_arity_2),
    // Sdiv
    (Opcode::Sdiv, &[I8, I8], &[I8], insert_opcode_arity_2),
    (Opcode::Sdiv, &[I16, I16], &[I16], insert_opcode_arity_2),
    (Opcode::Sdiv, &[I32, I32], &[I32], insert_opcode_arity_2),
    (Opcode::Sdiv, &[I64, I64], &[I64], insert_opcode_arity_2),
];

pub struct FunctionGenerator<'r, 'data>
where
    'data: 'r,
{
    u: &'r mut Unstructured<'data>,
    config: &'r Config,
    vars: Vec<(Type, Variable)>,
    blocks: Vec<(Block, BlockSignature)>,
}

impl<'r, 'data> FunctionGenerator<'r, 'data>
where
    'data: 'r,
{
    pub fn new(u: &'r mut Unstructured<'data>, config: &'r Config) -> Self {
        Self {
            u,
            config,
            vars: vec![],
            blocks: vec![],
        }
    }

    fn generate_callconv(&mut self) -> Result<CallConv> {
        // TODO: Generate random CallConvs per target
        Ok(CallConv::SystemV)
    }

    fn generate_type(&mut self) -> Result<Type> {
        // TODO: It would be nice if we could get these directly from cranelift
        let scalars = [
            // IFLAGS, FFLAGS,
            // B1, B8, B16, B32, B64, B128,
            I8, I16, I32, I64,
            // I128,
            // F32, F64,
            // R32, R64,
        ];
        // TODO: vector types

        let ty = self.u.choose(&scalars[..])?;
        Ok(*ty)
    }

    fn generate_abi_param(&mut self) -> Result<AbiParam> {
        // TODO: Generate more advanced abi params (structs/purposes/extensions/etc...)
        let ty = self.generate_type()?;
        Ok(AbiParam::new(ty))
    }

    fn generate_signature(&mut self) -> Result<Signature> {
        let callconv = self.generate_callconv()?;
        let mut sig = Signature::new(callconv);

        for _ in 0..self.u.int_in_range(self.config.signature_params.clone())? {
            sig.params.push(self.generate_abi_param()?);
        }

        for _ in 0..self.u.int_in_range(self.config.signature_rets.clone())? {
            sig.returns.push(self.generate_abi_param()?);
        }

        Ok(sig)
    }

    /// Creates a new var
    fn create_var(&mut self, builder: &mut FunctionBuilder, ty: Type) -> Result<Variable> {
        let id = self.vars.len();
        let var = Variable::new(id);
        builder.declare_var(var, ty);
        self.vars.push((ty, var));
        Ok(var)
    }

    fn vars_of_type(&self, ty: Type) -> Vec<Variable> {
        self.vars
            .iter()
            .filter(|(var_ty, _)| *var_ty == ty)
            .map(|(_, v)| *v)
            .collect()
    }

    /// Get a variable of type `ty` from the current function
    fn get_variable_of_type(&mut self, ty: Type) -> Result<Variable> {
        let opts = self.vars_of_type(ty);
        let var = self.u.choose(&opts[..])?;
        Ok(*var)
    }

    /// Generates an instruction(`iconst`/`fconst`/etc...) to introduce a constant value
    fn generate_const(&mut self, builder: &mut FunctionBuilder, ty: Type) -> Result<Value> {
        let imm64 = match ty {
            I8 => self.u.arbitrary::<i8>()? as i64,
            I16 => self.u.arbitrary::<i16>()? as i64,
            I32 => self.u.arbitrary::<i32>()? as i64,
            I64 => self.u.arbitrary::<i64>()?,
            _ => unreachable!(),
        };
        let val = builder.ins().iconst(ty, imm64);

        Ok(val)
    }

    fn generate_return(&mut self, builder: &mut FunctionBuilder) -> Result<()> {
        let ret_params = builder.func.signature.returns.clone();

        let vars = ret_params
            .iter()
            .map(|p| self.get_variable_of_type(p.value_type))
            .collect::<Result<Vec<_>>>()?;

        let vals = vars
            .into_iter()
            .map(|v| builder.use_var(v))
            .collect::<Vec<_>>();

        builder.ins().return_(&vals[..]);
        Ok(())
    }

    fn generate_jump(&mut self, builder: &mut FunctionBuilder) -> Result<()> {
        let (block, signature) = {
            let target = self.u.choose(&self.blocks[..])?;
            let target = target.clone();
            target
        };

        let vars = signature
            .iter()
            .map(|ty| self.get_variable_of_type(*ty))
            .collect::<Result<Vec<_>>>()?;

        let vals = vars
            .into_iter()
            .map(|v| builder.use_var(v))
            .collect::<Vec<_>>();

        builder.ins().jump(*block, &vals[..]);
        Ok(())
    }

    /// Inserts a random instruction into the block
    fn generate_instruction(&mut self, builder: &mut FunctionBuilder) -> Result<()> {
        let (op, args, rets, inserter) = *self.u.choose(OPCODE_SIGNATURES)?;
        inserter(self, builder, op, args, rets)
    }

    /// Creates a random amount of blocks in this function
    fn generate_blocks(
        &mut self,
        builder: &mut FunctionBuilder,
        sig: &Signature,
    ) -> Result<Vec<(Block, BlockSignature)>> {
        let extra_block_count = self
            .u
            .int_in_range(self.config.blocks_per_function.clone())?;

        // We must always have at least one block, so we generate the "extra" blocks and add 1 for
        // the entry block.
        let block_count = 1 + extra_block_count;

        let blocks = (0..block_count)
            .map(|i| {
                let block = builder.create_block();

                // The first block has to have the function signature, but for the rest of them we generate
                // a random signature;
                if i == 0 {
                    builder.append_block_params_for_function_params(block);
                    Ok((block, sig.params.iter().map(|a| a.value_type).collect()))
                } else {
                    Ok((block, self.generate_block_signature()?))
                }
            })
            .collect::<Result<Vec<_>>>()?;

        Ok(blocks)
    }

    fn generate_block_signature(&mut self) -> Result<BlockSignature> {
        let param_count = self
            .u
            .int_in_range(self.config.block_signature_params.clone())?;

        let mut params = Vec::with_capacity(param_count);
        for _ in 0..param_count {
            params.push(self.generate_type()?);
        }
        Ok(params)
    }

    /// We generate a function in multiple stages: by first creating a random number of empty
    ///
    /// * First we generate a random number of empty blocks
    /// * Then we generate a random pool of variables to be used throughout the function
    /// * We then visit each block and generate random instructions
    ///
    /// Because we generate all blocks and variables up front we already know everything that
    /// we need when generating instructions (i.e. jump targets / variables)
    pub fn generate(mut self) -> Result<Function> {
        let sig = self.generate_signature()?;

        let mut fn_builder_ctx = FunctionBuilderContext::new();
        let mut func = Function::with_name_signature(ExternalName::user(0, 0), sig.clone());

        let mut builder = FunctionBuilder::new(&mut func, &mut fn_builder_ctx);

        self.blocks = self.generate_blocks(&mut builder, &sig)?;

        // Main instruction generation loop
        for (i, (block, signature)) in self.blocks.clone().iter().enumerate() {
            builder.switch_to_block(*block);

            if i == 0 {
                // Define variables for the function signature
                for (i, param) in sig.params.iter().enumerate() {
                    let var = self.create_var(&mut builder, param.value_type)?;
                    let block_param = builder.block_params(*block)[i];
                    builder.def_var(var, block_param);
                }

                // Create a pool of vars that are going to be used in this function
                for _ in 0..self.u.int_in_range(self.config.vars_per_function.clone())? {
                    let ty = self.generate_type()?;
                    let var = self.create_var(&mut builder, ty)?;
                    let value = self.generate_const(&mut builder, ty)?;
                    builder.def_var(var, value);
                }
            } else {
                // Define variables for the block params
                for (i, ty) in signature.iter().enumerate() {
                    let var = self.get_variable_of_type(*ty)?;
                    let block_param = builder.block_params(*block)[i];
                    builder.def_var(var, block_param);
                }
            }

            // Generate block instructions
            for _ in 0..self
                .u
                .int_in_range(self.config.instructions_per_block.clone())?
            {
                self.generate_instruction(&mut builder)?;
            }

            // We always need to exit safely out of a block.
            // For block 0 this means a return, but for other block it is a jump into any other
            // random block.
            if i == 0 {
                // TODO: We should make this, part of the regular instruction selection
                self.generate_return(&mut builder)?;
            } else {
                self.generate_jump(&mut builder)?;
            }
        }

        builder.seal_all_blocks();
        builder.finalize();

        Ok(func)
    }
}
