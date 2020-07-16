use crate::result::{CodegenResult, CodegenError};
use crate::isa::spirv::SpirvBackend;
use crate::machinst::lower::*;
use crate::machinst::buffer::MachLabel;
use crate::ir::Inst as IRInst;
use super::inst::Inst;
use crate::ir::{InstructionData, Opcode, Type};
use regalloc::{Reg, Writable};
use spirv_headers::Op;

type Ctx<'a> = &'a mut dyn LowerCtx<I = Inst>;

fn input_to_reg<'a>(ctx: Ctx<'a>, iri: IRInst, input: usize) -> Reg {
    let inputs = ctx.get_input(iri, input);
    ctx.use_input_reg(inputs);
    inputs.reg
}

fn output_to_reg<'a>(ctx: Ctx<'a>, iri: IRInst, output: usize) -> Writable<Reg> {
    ctx.get_output(iri, output)
}

impl LowerBackend for SpirvBackend {
    type MInst = Inst;

    fn lower<C: LowerCtx<I = Self::MInst>>(&self, ctx: &mut C, inst: IRInst) -> CodegenResult<()> {
        let op = ctx.data(inst).opcode();

        match op {
            Opcode::Iconst => {
                let regD = output_to_reg(ctx, inst, 0);
            }
            Opcode::Iadd | Opcode::Isub => {
                let regD = output_to_reg(ctx, inst, 0);
                let regL = input_to_reg(ctx, inst, 0);
                let regR = input_to_reg(ctx, inst, 1);

                // jb-todo: we need to do two more things here;
                // a) find out the SPIR-V relevant type for `regD` to pass in, instead of None
                // b) register that type globally to keep track if needed

                let reg_d_ty = ctx.output_ty(inst, 0);

                ctx.emit(Inst::new(Op::IAdd, None, Some(regD), vec![regL, regR]));
            },
            Opcode::FallthroughReturn | Opcode::Return => {
                for i in 0..ctx.num_inputs(inst) {
                    let src_reg = input_to_reg(ctx, inst, i);
                    let retval_reg = ctx.retval(i);
                    //ctx.emit(Inst::mov_r_r(true, src_reg, retval_reg));
                }
                // N.B.: the Ret itself is generated by the ABI.
            }
            _ => {
                println!("Missing OP-code implementation")
            }
        }

        Ok(())
    }

    fn lower_branch_group<C: LowerCtx<I = Self::MInst>>(
        &self,
        ctx: &mut C,
        insts: &[IRInst],
        targets: &[MachLabel],
        fallthrough: Option<MachLabel>,
    ) -> CodegenResult<()> {
        Err(CodegenError::Unsupported(format!("lower_branch_group")))
    }
}