//! Lowering rules for 32-bit ARM.

use crate::ir::condcodes::IntCC;
use crate::ir::types::*;
use crate::ir::Inst as IRInst;
use crate::ir::{InstructionData, Opcode, TrapCode};
use crate::machinst::lower::*;
use crate::machinst::*;
use crate::CodegenResult;

use crate::isa::arm32::inst::*;
use crate::isa::arm32::Arm32Backend;

use super::lower_inst;

use regalloc::{Reg, RegClass, Writable};

//============================================================================
// Lowering: convert instruction outputs to result types.

/// Lower an instruction output to a 32-bit constant, if possible.
pub(crate) fn output_to_const<C: LowerCtx<I = Inst>>(ctx: &mut C, out: InsnOutput) -> Option<u64> {
    if out.output > 0 {
        None
    } else {
        let inst_data = ctx.data(out.insn);
        if inst_data.opcode() == Opcode::Null {
            Some(0)
        } else {
            match inst_data {
                &InstructionData::UnaryImm { opcode: _, imm } => {
                    // Only has Into for i64; we use u64 elsewhere, so we cast.
                    let imm: i64 = imm.into();
                    Some(imm as u64)
                }
                &InstructionData::UnaryBool { opcode: _, imm } => Some(u64::from(imm)),
                &InstructionData::UnaryIeee32 { .. } | &InstructionData::UnaryIeee64 { .. } => {
                    unimplemented!()
                }
                _ => None,
            }
        }
    }
}

/// How to handle narrow values loaded into registers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NarrowValueMode {
    None,
    /// Zero-extend to 32 bits if original is < 32 bits.
    ZeroExtend,
    /// Sign-extend to 32 bits if original is < 32 bits.
    SignExtend,
}

/// Lower an instruction output to a reg.
pub(crate) fn output_to_reg<C: LowerCtx<I = Inst>>(ctx: &mut C, out: InsnOutput) -> Writable<Reg> {
    ctx.get_output(out.insn, out.output)
}

/// Lower an instruction input to a reg.
///
/// The given register will be extended appropriately, according to `narrow_mode`.
pub(crate) fn input_to_reg<C: LowerCtx<I = Inst>>(
    ctx: &mut C,
    input: InsnInput,
    narrow_mode: NarrowValueMode,
) -> Reg {
    let ty = ctx.input_ty(input.insn, input.input);
    let from_bits = ty.bits() as u8;
    let inputs = ctx.get_input_as_source_or_const(input.insn, input.input);
    let in_reg = if let Some(c) = inputs.constant {
        let to_reg = ctx.alloc_tmp(Inst::rc_for_type(ty).unwrap(), ty);
        ctx.emit_constant(DataValue::U64(c), to_reg);
        to_reg.to_reg()
    } else {
        ctx.put_input_in_reg(input.insn, input.input)
    };

    match (narrow_mode, from_bits) {
        (NarrowValueMode::None, _) => in_reg,
        (NarrowValueMode::ZeroExtend, 1) => {
            let tmp = ctx.alloc_tmp(RegClass::I32, I32);
            ctx.emit(Inst::AluRRImm8 {
                alu_op: ALUOp::And,
                rd: tmp,
                rn: in_reg,
                imm8: UImm8::maybe_from_i64(0x1).unwrap(),
            });
            tmp.to_reg()
        }
        (NarrowValueMode::ZeroExtend, n) if n < 32 => {
            let tmp = ctx.alloc_tmp(RegClass::I32, I32);
            ctx.emit(Inst::Extend {
                rd: tmp,
                rm: in_reg,
                signed: false,
                from_bits: n,
            });
            tmp.to_reg()
        }
        (NarrowValueMode::SignExtend, n) if n < 32 => {
            let tmp = ctx.alloc_tmp(RegClass::I32, I32);
            ctx.emit(Inst::Extend {
                rd: tmp,
                rm: in_reg,
                signed: true,
                from_bits: n,
            });
            tmp.to_reg()
        }
        (NarrowValueMode::ZeroExtend, 32) | (NarrowValueMode::SignExtend, 32) => in_reg,
        _ => panic!(
            "Unsupported input width: input ty {} bits {} mode {:?}",
            ty, from_bits, narrow_mode
        ),
    }
}

pub(crate) fn lower_constant<C: LowerCtx<I = Inst>>(ctx: &mut C, rd: Writable<Reg>, value: u64) {
    // We allow sign bits for high word.
    assert!((value >> 32) == 0x0 || (value >> 32) == (1 << 32) - 1);

    for inst in Inst::load_constant(rd, (value & ((1 << 32) - 1)) as u32) {
        ctx.emit(inst);
    }
}

pub(crate) fn emit_cmp<C: LowerCtx<I = Inst>>(ctx: &mut C, insn: IRInst) {
    let inputs = [InsnInput { insn, input: 0 }, InsnInput { insn, input: 1 }];
    let rn = input_to_reg(ctx, inputs[0], NarrowValueMode::None);
    let rm = input_to_reg(ctx, inputs[1], NarrowValueMode::None);

    ctx.emit(Inst::Cmp { rn, rm });
}

pub(crate) fn lower_condcode(cc: IntCC) -> Cond {
    match cc {
        IntCC::Equal => Cond::Eq,
        IntCC::NotEqual => Cond::Ne,
        IntCC::SignedGreaterThanOrEqual => Cond::Ge,
        IntCC::SignedGreaterThan => Cond::Gt,
        IntCC::SignedLessThanOrEqual => Cond::Le,
        IntCC::SignedLessThan => Cond::Lt,
        IntCC::UnsignedGreaterThanOrEqual => Cond::Hs,
        IntCC::UnsignedGreaterThan => Cond::Hi,
        IntCC::UnsignedLessThanOrEqual => Cond::Ls,
        IntCC::UnsignedLessThan => Cond::Lo,
        IntCC::Overflow => Cond::Vs,
        IntCC::NotOverflow => Cond::Vc,
    }
}

/// Determines whether this condcode interprets inputs as signed or unsigned.
pub(crate) fn condcode_is_signed(cc: IntCC) -> bool {
    match cc {
        IntCC::Equal => false,
        IntCC::NotEqual => false,
        IntCC::SignedGreaterThanOrEqual => true,
        IntCC::SignedGreaterThan => true,
        IntCC::SignedLessThanOrEqual => true,
        IntCC::SignedLessThan => true,
        IntCC::UnsignedGreaterThanOrEqual => false,
        IntCC::UnsignedGreaterThan => false,
        IntCC::UnsignedLessThanOrEqual => false,
        IntCC::UnsignedLessThan => false,
        IntCC::Overflow => true,
        IntCC::NotOverflow => true,
    }
}

//=============================================================================
// Helpers for instruction lowering.

pub(crate) fn ldst_offset(data: &InstructionData) -> Option<i32> {
    match data {
        &InstructionData::Load { offset, .. }
        | &InstructionData::StackLoad { offset, .. }
        | &InstructionData::LoadComplex { offset, .. }
        | &InstructionData::Store { offset, .. }
        | &InstructionData::StackStore { offset, .. }
        | &InstructionData::StoreComplex { offset, .. } => Some(offset.into()),
        _ => None,
    }
}

pub(crate) fn inst_condcode(data: &InstructionData) -> Option<IntCC> {
    match data {
        &InstructionData::IntCond { cond, .. }
        | &InstructionData::BranchIcmp { cond, .. }
        | &InstructionData::IntCompare { cond, .. }
        | &InstructionData::IntCondTrap { cond, .. }
        | &InstructionData::BranchInt { cond, .. }
        | &InstructionData::IntSelect { cond, .. }
        | &InstructionData::IntCompareImm { cond, .. } => Some(cond),
        _ => None,
    }
}

pub(crate) fn inst_trapcode(data: &InstructionData) -> Option<TrapCode> {
    match data {
        &InstructionData::Trap { code, .. }
        | &InstructionData::CondTrap { code, .. }
        | &InstructionData::IntCondTrap { code, .. } => Some(code),
        &InstructionData::FloatCondTrap { code, .. } => {
            panic!("Unexpected float cond trap {:?}", code)
        }
        _ => None,
    }
}

//=============================================================================
// Lowering-backend trait implementation.

impl LowerBackend for Arm32Backend {
    type MInst = Inst;

    fn lower<C: LowerCtx<I = Inst>>(&self, ctx: &mut C, ir_inst: IRInst) -> CodegenResult<()> {
        lower_inst::lower_insn_to_regs(ctx, ir_inst)
    }

    fn lower_branch_group<C: LowerCtx<I = Inst>>(
        &self,
        ctx: &mut C,
        branches: &[IRInst],
        targets: &[MachLabel],
    ) -> CodegenResult<()> {
        lower_inst::lower_branch(ctx, branches, targets)
    }

    fn maybe_pinned_reg(&self) -> Option<Reg> {
        None
    }
}
