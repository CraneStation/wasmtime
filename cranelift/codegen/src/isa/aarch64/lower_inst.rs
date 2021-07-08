//! Lower a single Cranelift instruction into vcode.

use crate::binemit::CodeOffset;
use crate::ir::condcodes::FloatCC;
use crate::ir::types::*;
use crate::ir::Inst as IRInst;
use crate::ir::{InstructionData, Opcode, TrapCode};
use crate::isa::aarch64::settings as aarch64_settings;
use crate::machinst::lower::*;
use crate::machinst::*;
use crate::settings::{Flags, TlsModel};
use crate::{CodegenError, CodegenResult};

use crate::isa::aarch64::abi::*;
use crate::isa::aarch64::inst::*;

use regalloc::Writable;

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::convert::TryFrom;

use super::lower::*;

/// Actually codegen an instruction's results into registers.
pub(crate) fn lower_insn_to_regs<C: LowerCtx<I = Inst>>(
    ctx: &mut C,
    insn: IRInst,
    flags: &Flags,
    isa_flags: &aarch64_settings::Flags,
) -> CodegenResult<()> {
    let op = ctx.data(insn).opcode();
    let inputs = insn_inputs(ctx, insn);
    let outputs = insn_outputs(ctx, insn);
    let ty = if outputs.len() > 0 {
        Some(ctx.output_ty(insn, 0))
    } else {
        None
    };

    match op {
        Opcode::Iconst | Opcode::Bconst | Opcode::Null => {
            let value = ctx.get_constant(insn).unwrap();
            // Sign extend constant if necessary
            let value = match ty.unwrap() {
                I8 => (((value as i64) << 56) >> 56) as u64,
                I16 => (((value as i64) << 48) >> 48) as u64,
                I32 => (((value as i64) << 32) >> 32) as u64,
                I64 | R64 => value,
                ty if ty.is_bool() => value,
                ty => unreachable!("Unknown type for const: {}", ty),
            };
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            lower_constant_u64(ctx, rd, value);
        }
        Opcode::F32const => {
            let value = f32::from_bits(ctx.get_constant(insn).unwrap() as u32);
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            lower_constant_f32(ctx, rd, value);
        }
        Opcode::F64const => {
            let value = f64::from_bits(ctx.get_constant(insn).unwrap());
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            lower_constant_f64(ctx, rd, value);
        }
        Opcode::Iadd => {
            match ty.unwrap() {
                ty if ty.is_vector() => {
                    let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
                    let rm = put_input_in_reg(ctx, inputs[1], NarrowValueMode::None);
                    let rn = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
                    ctx.emit(Inst::VecRRR {
                        rd,
                        rn,
                        rm,
                        alu_op: VecALUOp::Add,
                        size: VectorSize::from_ty(ty),
                    });
                }
                I128 => {
                    let lhs = put_input_in_regs(ctx, inputs[0]);
                    let rhs = put_input_in_regs(ctx, inputs[1]);
                    let dst = get_output_reg(ctx, outputs[0]);
                    assert_eq!(lhs.len(), 2);
                    assert_eq!(rhs.len(), 2);
                    assert_eq!(dst.len(), 2);

                    // adds    x0, x0, x2
                    // adc     x1, x1, x3

                    ctx.emit(Inst::AluRRR {
                        alu_op: ALUOp::AddS64,
                        rd: dst.regs()[0],
                        rn: lhs.regs()[0],
                        rm: rhs.regs()[0],
                    });
                    ctx.emit(Inst::AluRRR {
                        alu_op: ALUOp::Adc64,
                        rd: dst.regs()[1],
                        rn: lhs.regs()[1],
                        rm: rhs.regs()[1],
                    });
                }
                ty => {
                    let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
                    let mul_insn = if let Some(mul_insn) =
                        maybe_input_insn(ctx, inputs[1], Opcode::Imul)
                    {
                        Some((mul_insn, 0))
                    } else if let Some(mul_insn) = maybe_input_insn(ctx, inputs[0], Opcode::Imul) {
                        Some((mul_insn, 1))
                    } else {
                        None
                    };
                    // If possible combine mul + add into madd.
                    if let Some((insn, addend_idx)) = mul_insn {
                        let alu_op = choose_32_64(ty, ALUOp3::MAdd32, ALUOp3::MAdd64);
                        let rn_input = InsnInput { insn, input: 0 };
                        let rm_input = InsnInput { insn, input: 1 };

                        let rn = put_input_in_reg(ctx, rn_input, NarrowValueMode::None);
                        let rm = put_input_in_reg(ctx, rm_input, NarrowValueMode::None);
                        let ra = put_input_in_reg(ctx, inputs[addend_idx], NarrowValueMode::None);

                        ctx.emit(Inst::AluRRRR {
                            alu_op,
                            rd,
                            rn,
                            rm,
                            ra,
                        });
                    } else {
                        let rn = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
                        let (rm, negated) = put_input_in_rse_imm12_maybe_negated(
                            ctx,
                            inputs[1],
                            ty_bits(ty),
                            NarrowValueMode::None,
                        );
                        let alu_op = if !negated {
                            choose_32_64(ty, ALUOp::Add32, ALUOp::Add64)
                        } else {
                            choose_32_64(ty, ALUOp::Sub32, ALUOp::Sub64)
                        };
                        ctx.emit(alu_inst_imm12(alu_op, rd, rn, rm));
                    }
                }
            }
        }
        Opcode::Isub => {
            let ty = ty.unwrap();
            if ty == I128 {
                let lhs = put_input_in_regs(ctx, inputs[0]);
                let rhs = put_input_in_regs(ctx, inputs[1]);
                let dst = get_output_reg(ctx, outputs[0]);
                assert_eq!(lhs.len(), 2);
                assert_eq!(rhs.len(), 2);
                assert_eq!(dst.len(), 2);

                // subs    x0, x0, x2
                // sbc     x1, x1, x3

                ctx.emit(Inst::AluRRR {
                    alu_op: ALUOp::SubS64,
                    rd: dst.regs()[0],
                    rn: lhs.regs()[0],
                    rm: rhs.regs()[0],
                });
                ctx.emit(Inst::AluRRR {
                    alu_op: ALUOp::Sbc64,
                    rd: dst.regs()[1],
                    rn: lhs.regs()[1],
                    rm: rhs.regs()[1],
                });
            } else {
                let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
                let rn = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
                if !ty.is_vector() {
                    let (rm, negated) = put_input_in_rse_imm12_maybe_negated(
                        ctx,
                        inputs[1],
                        ty_bits(ty),
                        NarrowValueMode::None,
                    );
                    let alu_op = if !negated {
                        choose_32_64(ty, ALUOp::Sub32, ALUOp::Sub64)
                    } else {
                        choose_32_64(ty, ALUOp::Add32, ALUOp::Add64)
                    };
                    ctx.emit(alu_inst_imm12(alu_op, rd, rn, rm));
                } else {
                    let rm = put_input_in_reg(ctx, inputs[1], NarrowValueMode::None);
                    ctx.emit(Inst::VecRRR {
                        rd,
                        rn,
                        rm,
                        alu_op: VecALUOp::Sub,
                        size: VectorSize::from_ty(ty),
                    });
                }
            }
        }
        Opcode::UaddSat | Opcode::SaddSat | Opcode::UsubSat | Opcode::SsubSat => {
            let ty = ty.unwrap();
            assert!(ty.is_vector());
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let rn = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
            let rm = put_input_in_reg(ctx, inputs[1], NarrowValueMode::None);

            let alu_op = match op {
                Opcode::UaddSat => VecALUOp::Uqadd,
                Opcode::SaddSat => VecALUOp::Sqadd,
                Opcode::UsubSat => VecALUOp::Uqsub,
                Opcode::SsubSat => VecALUOp::Sqsub,
                _ => unreachable!(),
            };

            ctx.emit(Inst::VecRRR {
                rd,
                rn,
                rm,
                alu_op,
                size: VectorSize::from_ty(ty),
            });
        }

        Opcode::Ineg => {
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let ty = ty.unwrap();
            if !ty.is_vector() {
                let rn = zero_reg();
                let rm = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
                let alu_op = choose_32_64(ty, ALUOp::Sub32, ALUOp::Sub64);
                ctx.emit(Inst::AluRRR { alu_op, rd, rn, rm });
            } else {
                let rn = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
                ctx.emit(Inst::VecMisc {
                    op: VecMisc2::Neg,
                    rd,
                    rn,
                    size: VectorSize::from_ty(ty),
                });
            }
        }

        Opcode::Imul => {
            let lhs = put_input_in_regs(ctx, inputs[0]);
            let rhs = put_input_in_regs(ctx, inputs[1]);
            let dst = get_output_reg(ctx, outputs[0]);

            let rd = dst.regs()[0];
            let rn = lhs.regs()[0];
            let rm = rhs.regs()[0];

            let ty = ty.unwrap();
            match ty {
                I128 => {
                    assert_eq!(lhs.len(), 2);
                    assert_eq!(rhs.len(), 2);
                    assert_eq!(dst.len(), 2);

                    // 128bit mul formula:
                    //   dst_lo = lhs_lo * rhs_lo
                    //   dst_hi = umulhi(lhs_lo, rhs_lo) + (lhs_lo * rhs_hi) + (lhs_hi * rhs_lo)
                    //
                    // We can convert the above formula into the following
                    // umulh   dst_hi, lhs_lo, rhs_lo
                    // madd    dst_hi, lhs_lo, rhs_hi, dst_hi
                    // madd    dst_hi, lhs_hi, rhs_lo, dst_hi
                    // mul     dst_lo, lhs_lo, rhs_lo

                    ctx.emit(Inst::AluRRR {
                        alu_op: ALUOp::UMulH,
                        rd: dst.regs()[1],
                        rn: lhs.regs()[0],
                        rm: rhs.regs()[0],
                    });
                    ctx.emit(Inst::AluRRRR {
                        alu_op: ALUOp3::MAdd64,
                        rd: dst.regs()[1],
                        rn: lhs.regs()[0],
                        rm: rhs.regs()[1],
                        ra: dst.regs()[1].to_reg(),
                    });
                    ctx.emit(Inst::AluRRRR {
                        alu_op: ALUOp3::MAdd64,
                        rd: dst.regs()[1],
                        rn: lhs.regs()[1],
                        rm: rhs.regs()[0],
                        ra: dst.regs()[1].to_reg(),
                    });
                    ctx.emit(Inst::AluRRRR {
                        alu_op: ALUOp3::MAdd64,
                        rd: dst.regs()[0],
                        rn: lhs.regs()[0],
                        rm: rhs.regs()[0],
                        ra: zero_reg(),
                    });
                }
                ty if !ty.is_vector() => {
                    let alu_op = choose_32_64(ty, ALUOp3::MAdd32, ALUOp3::MAdd64);
                    ctx.emit(Inst::AluRRRR {
                        alu_op,
                        rd,
                        rn,
                        rm,
                        ra: zero_reg(),
                    });
                }
                I64X2 => {
                    let tmp1 = ctx.alloc_tmp(I64X2).only_reg().unwrap();
                    let tmp2 = ctx.alloc_tmp(I64X2).only_reg().unwrap();

                    // This I64X2 multiplication is performed with several 32-bit
                    // operations.

                    // 64-bit numbers x and y, can be represented as:
                    //   x = a + 2^32(b)
                    //   y = c + 2^32(d)

                    // A 64-bit multiplication is:
                    //   x * y = ac + 2^32(ad + bc) + 2^64(bd)
                    // note: `2^64(bd)` can be ignored, the value is too large to fit in
                    // 64 bits.

                    // This sequence implements a I64X2 multiply, where the registers
                    // `rn` and `rm` are split up into 32-bit components:
                    //   rn = |d|c|b|a|
                    //   rm = |h|g|f|e|
                    //
                    //   rn * rm = |cg + 2^32(ch + dg)|ae + 2^32(af + be)|
                    //
                    //  The sequence is:
                    //  rev64 rd.4s, rm.4s
                    //  mul rd.4s, rd.4s, rn.4s
                    //  xtn tmp1.2s, rn.2d
                    //  addp rd.4s, rd.4s, rd.4s
                    //  xtn tmp2.2s, rm.2d
                    //  shll rd.2d, rd.2s, #32
                    //  umlal rd.2d, tmp2.2s, tmp1.2s

                    // Reverse the 32-bit elements in the 64-bit words.
                    //   rd = |g|h|e|f|
                    ctx.emit(Inst::VecMisc {
                        op: VecMisc2::Rev64,
                        rd,
                        rn: rm,
                        size: VectorSize::Size32x4,
                    });

                    // Calculate the high half components.
                    //   rd = |dg|ch|be|af|
                    //
                    // Note that this 32-bit multiply of the high half
                    // discards the bits that would overflow, same as
                    // if 64-bit operations were used. Also the Shll
                    // below would shift out the overflow bits anyway.
                    ctx.emit(Inst::VecRRR {
                        alu_op: VecALUOp::Mul,
                        rd,
                        rn: rd.to_reg(),
                        rm: rn,
                        size: VectorSize::Size32x4,
                    });

                    // Extract the low half components of rn.
                    //   tmp1 = |c|a|
                    ctx.emit(Inst::VecRRNarrow {
                        op: VecRRNarrowOp::Xtn64,
                        rd: tmp1,
                        rn,
                        high_half: false,
                    });

                    // Sum the respective high half components.
                    //   rd = |dg+ch|be+af||dg+ch|be+af|
                    ctx.emit(Inst::VecRRR {
                        alu_op: VecALUOp::Addp,
                        rd: rd,
                        rn: rd.to_reg(),
                        rm: rd.to_reg(),
                        size: VectorSize::Size32x4,
                    });

                    // Extract the low half components of rm.
                    //   tmp2 = |g|e|
                    ctx.emit(Inst::VecRRNarrow {
                        op: VecRRNarrowOp::Xtn64,
                        rd: tmp2,
                        rn: rm,
                        high_half: false,
                    });

                    // Shift the high half components, into the high half.
                    //   rd = |dg+ch << 32|be+af << 32|
                    ctx.emit(Inst::VecRRLong {
                        op: VecRRLongOp::Shll32,
                        rd,
                        rn: rd.to_reg(),
                        high_half: false,
                    });

                    // Multiply the low components together, and accumulate with the high
                    // half.
                    //   rd = |rd[1] + cg|rd[0] + ae|
                    ctx.emit(Inst::VecRRR {
                        alu_op: VecALUOp::Umlal,
                        rd,
                        rn: tmp2.to_reg(),
                        rm: tmp1.to_reg(),
                        size: VectorSize::Size32x2,
                    });
                }
                ty if ty.is_vector() => {
                    ctx.emit(Inst::VecRRR {
                        alu_op: VecALUOp::Mul,
                        rd,
                        rn,
                        rm,
                        size: VectorSize::from_ty(ty),
                    });
                }
                _ => panic!("Unable to emit mul for {}", ty),
            }
        }

        Opcode::Umulhi | Opcode::Smulhi => {
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let is_signed = op == Opcode::Smulhi;
            let input_ty = ctx.input_ty(insn, 0);
            assert!(ctx.input_ty(insn, 1) == input_ty);
            assert!(ctx.output_ty(insn, 0) == input_ty);

            match input_ty {
                I64 => {
                    let rn = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
                    let rm = put_input_in_reg(ctx, inputs[1], NarrowValueMode::None);
                    let alu_op = if is_signed {
                        ALUOp::SMulH
                    } else {
                        ALUOp::UMulH
                    };
                    ctx.emit(Inst::AluRRR { alu_op, rd, rn, rm });
                }
                I32 | I16 | I8 => {
                    let narrow_mode = if is_signed {
                        NarrowValueMode::SignExtend64
                    } else {
                        NarrowValueMode::ZeroExtend64
                    };
                    let rn = put_input_in_reg(ctx, inputs[0], narrow_mode);
                    let rm = put_input_in_reg(ctx, inputs[1], narrow_mode);
                    let ra = zero_reg();
                    ctx.emit(Inst::AluRRRR {
                        alu_op: ALUOp3::MAdd64,
                        rd,
                        rn,
                        rm,
                        ra,
                    });
                    let shift_op = if is_signed {
                        ALUOp::Asr64
                    } else {
                        ALUOp::Lsr64
                    };
                    let shift_amt = match input_ty {
                        I32 => 32,
                        I16 => 16,
                        I8 => 8,
                        _ => unreachable!(),
                    };
                    ctx.emit(Inst::AluRRImmShift {
                        alu_op: shift_op,
                        rd,
                        rn: rd.to_reg(),
                        immshift: ImmShift::maybe_from_u64(shift_amt).unwrap(),
                    });
                }
                _ => {
                    panic!("Unsupported argument type for umulhi/smulhi: {}", input_ty);
                }
            }
        }

        Opcode::Udiv | Opcode::Sdiv | Opcode::Urem | Opcode::Srem => {
            let is_signed = match op {
                Opcode::Udiv | Opcode::Urem => false,
                Opcode::Sdiv | Opcode::Srem => true,
                _ => unreachable!(),
            };
            let is_rem = match op {
                Opcode::Udiv | Opcode::Sdiv => false,
                Opcode::Urem | Opcode::Srem => true,
                _ => unreachable!(),
            };
            let narrow_mode = if is_signed {
                NarrowValueMode::SignExtend64
            } else {
                NarrowValueMode::ZeroExtend64
            };
            // TODO: Add SDiv32 to implement 32-bit directly, rather
            // than extending the input.
            let div_op = if is_signed {
                ALUOp::SDiv64
            } else {
                ALUOp::UDiv64
            };

            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let rn = put_input_in_reg(ctx, inputs[0], narrow_mode);
            let rm = put_input_in_reg(ctx, inputs[1], narrow_mode);
            // The div instruction does not trap on divide by zero or signed overflow
            // so checks are inserted below.
            //
            //   div rd, rn, rm
            ctx.emit(Inst::AluRRR {
                alu_op: div_op,
                rd,
                rn,
                rm,
            });

            if is_rem {
                // Remainder (rn % rm) is implemented as:
                //
                //   tmp = rn / rm
                //   rd = rn - (tmp*rm)
                //
                // use 'rd' for tmp and you have:
                //
                //   div rd, rn, rm       ; rd = rn / rm
                //   cbnz rm, #8          ; branch over trap
                //   udf                  ; divide by zero
                //   msub rd, rd, rm, rn  ; rd = rn - rd * rm

                // Check for divide by 0.
                let trap_code = TrapCode::IntegerDivisionByZero;
                ctx.emit(Inst::TrapIf {
                    trap_code,
                    kind: CondBrKind::Zero(rm),
                });

                ctx.emit(Inst::AluRRRR {
                    alu_op: ALUOp3::MSub64,
                    rd: rd,
                    rn: rd.to_reg(),
                    rm: rm,
                    ra: rn,
                });
            } else {
                if div_op == ALUOp::SDiv64 {
                    //   cbnz rm, #8
                    //   udf ; divide by zero
                    //   cmn rm, 1
                    //   ccmp rn, 1, #nzcv, eq
                    //   b.vc #8
                    //   udf ; signed overflow

                    // Check for divide by 0.
                    let trap_code = TrapCode::IntegerDivisionByZero;
                    ctx.emit(Inst::TrapIf {
                        trap_code,
                        kind: CondBrKind::Zero(rm),
                    });

                    // Check for signed overflow. The only case is min_value / -1.
                    let ty = ty.unwrap();
                    // The following checks must be done in 32-bit or 64-bit, depending
                    // on the input type. Even though the initial div instruction is
                    // always done in 64-bit currently.
                    let size = OperandSize::from_ty(ty);
                    // Check RHS is -1.
                    ctx.emit(Inst::AluRRImm12 {
                        alu_op: choose_32_64(ty, ALUOp::AddS32, ALUOp::AddS64),
                        rd: writable_zero_reg(),
                        rn: rm,
                        imm12: Imm12::maybe_from_u64(1).unwrap(),
                    });
                    // Check LHS is min_value, by subtracting 1 and branching if
                    // there is overflow.
                    ctx.emit(Inst::CCmpImm {
                        size,
                        rn,
                        imm: UImm5::maybe_from_u8(1).unwrap(),
                        nzcv: NZCV::new(false, false, false, false),
                        cond: Cond::Eq,
                    });
                    let trap_code = TrapCode::IntegerOverflow;
                    ctx.emit(Inst::TrapIf {
                        trap_code,
                        kind: CondBrKind::Cond(Cond::Vs),
                    });
                } else {
                    //   cbnz rm, #8
                    //   udf ; divide by zero

                    // Check for divide by 0.
                    let trap_code = TrapCode::IntegerDivisionByZero;
                    ctx.emit(Inst::TrapIf {
                        trap_code,
                        kind: CondBrKind::Zero(rm),
                    });
                }
            }
        }

        Opcode::Uextend | Opcode::Sextend => {
            let output_ty = ty.unwrap();
            let input_ty = ctx.input_ty(insn, 0);
            let from_bits = ty_bits(input_ty) as u8;
            let to_bits = ty_bits(output_ty) as u8;
            let to_bits = std::cmp::max(32, to_bits);
            assert!(from_bits <= to_bits);

            let signed = op == Opcode::Sextend;
            let dst = get_output_reg(ctx, outputs[0]);
            let src =
                if let Some(extract_insn) = maybe_input_insn(ctx, inputs[0], Opcode::Extractlane) {
                    put_input_in_regs(
                        ctx,
                        InsnInput {
                            insn: extract_insn,
                            input: 0,
                        },
                    )
                } else {
                    put_input_in_regs(ctx, inputs[0])
                };

            let needs_extend = from_bits < to_bits && to_bits <= 64;
            // For i128, we want to extend the lower half, except if it is already 64 bits.
            let needs_lower_extend = to_bits > 64 && from_bits < 64;
            let pass_through_lower = to_bits > 64 && !needs_lower_extend;

            if needs_extend || needs_lower_extend {
                let rn = src.regs()[0];
                let rd = dst.regs()[0];

                if let Some(extract_insn) = maybe_input_insn(ctx, inputs[0], Opcode::Extractlane) {
                    let idx =
                        if let InstructionData::BinaryImm8 { imm, .. } = ctx.data(extract_insn) {
                            *imm
                        } else {
                            unreachable!();
                        };

                    let size = VectorSize::from_ty(ctx.input_ty(extract_insn, 0));

                    if signed {
                        let scalar_size = OperandSize::from_ty(output_ty);

                        ctx.emit(Inst::MovFromVecSigned {
                            rd,
                            rn,
                            idx,
                            size,
                            scalar_size,
                        });
                    } else {
                        ctx.emit(Inst::MovFromVec { rd, rn, idx, size });
                    }
                } else {
                    // If we reach this point, we weren't able to incorporate the extend as
                    // a register-mode on another instruction, so we have a 'None'
                    // narrow-value/extend mode here, and we emit the explicit instruction.
                    let rn = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
                    ctx.emit(Inst::Extend {
                        rd,
                        rn,
                        signed,
                        from_bits,
                        to_bits: std::cmp::min(64, to_bits),
                    });
                }
            } else if pass_through_lower {
                ctx.emit(Inst::gen_move(dst.regs()[0], src.regs()[0], I64));
            }

            if output_ty == I128 {
                if signed {
                    ctx.emit(Inst::AluRRImmShift {
                        alu_op: ALUOp::Asr64,
                        rd: dst.regs()[1],
                        rn: dst.regs()[0].to_reg(),
                        immshift: ImmShift::maybe_from_u64(63).unwrap(),
                    });
                } else {
                    lower_constant_u64(ctx, dst.regs()[1], 0);
                }
            }
        }

        Opcode::Bnot => {
            let out_regs = get_output_reg(ctx, outputs[0]);
            let ty = ty.unwrap();
            if ty == I128 {
                // TODO: We can merge this block with the one below once we support immlogic here
                let in_regs = put_input_in_regs(ctx, inputs[0]);
                ctx.emit(Inst::AluRRR {
                    alu_op: ALUOp::OrrNot64,
                    rd: out_regs.regs()[0],
                    rn: zero_reg(),
                    rm: in_regs.regs()[0],
                });
                ctx.emit(Inst::AluRRR {
                    alu_op: ALUOp::OrrNot64,
                    rd: out_regs.regs()[1],
                    rn: zero_reg(),
                    rm: in_regs.regs()[1],
                });
            } else if !ty.is_vector() {
                let rd = out_regs.only_reg().unwrap();
                let rm = put_input_in_rs_immlogic(ctx, inputs[0], NarrowValueMode::None);
                let alu_op = choose_32_64(ty, ALUOp::OrrNot32, ALUOp::OrrNot64);
                // NOT rd, rm ==> ORR_NOT rd, zero, rm
                ctx.emit(alu_inst_immlogic(alu_op, rd, zero_reg(), rm));
            } else {
                let rd = out_regs.only_reg().unwrap();
                let rm = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
                ctx.emit(Inst::VecMisc {
                    op: VecMisc2::Not,
                    rd,
                    rn: rm,
                    size: VectorSize::from_ty(ty),
                });
            }
        }

        Opcode::Band
        | Opcode::Bor
        | Opcode::Bxor
        | Opcode::BandNot
        | Opcode::BorNot
        | Opcode::BxorNot => {
            let out_regs = get_output_reg(ctx, outputs[0]);
            let ty = ty.unwrap();
            if ty == I128 {
                // TODO: Support immlogic here
                let lhs = put_input_in_regs(ctx, inputs[0]);
                let rhs = put_input_in_regs(ctx, inputs[1]);
                let alu_op = match op {
                    Opcode::Band => ALUOp::And64,
                    Opcode::Bor => ALUOp::Orr64,
                    Opcode::Bxor => ALUOp::Eor64,
                    Opcode::BandNot => ALUOp::AndNot64,
                    Opcode::BorNot => ALUOp::OrrNot64,
                    Opcode::BxorNot => ALUOp::EorNot64,
                    _ => unreachable!(),
                };

                ctx.emit(Inst::AluRRR {
                    alu_op,
                    rd: out_regs.regs()[0],
                    rn: lhs.regs()[0],
                    rm: rhs.regs()[0],
                });
                ctx.emit(Inst::AluRRR {
                    alu_op,
                    rd: out_regs.regs()[1],
                    rn: lhs.regs()[1],
                    rm: rhs.regs()[1],
                });
            } else if !ty.is_vector() {
                let rd = out_regs.only_reg().unwrap();
                let rn = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
                let rm = put_input_in_rs_immlogic(ctx, inputs[1], NarrowValueMode::None);
                let alu_op = match op {
                    Opcode::Band => choose_32_64(ty, ALUOp::And32, ALUOp::And64),
                    Opcode::Bor => choose_32_64(ty, ALUOp::Orr32, ALUOp::Orr64),
                    Opcode::Bxor => choose_32_64(ty, ALUOp::Eor32, ALUOp::Eor64),
                    Opcode::BandNot => choose_32_64(ty, ALUOp::AndNot32, ALUOp::AndNot64),
                    Opcode::BorNot => choose_32_64(ty, ALUOp::OrrNot32, ALUOp::OrrNot64),
                    Opcode::BxorNot => choose_32_64(ty, ALUOp::EorNot32, ALUOp::EorNot64),
                    _ => unreachable!(),
                };
                ctx.emit(alu_inst_immlogic(alu_op, rd, rn, rm));
            } else {
                let alu_op = match op {
                    Opcode::Band => VecALUOp::And,
                    Opcode::BandNot => VecALUOp::Bic,
                    Opcode::Bor => VecALUOp::Orr,
                    Opcode::Bxor => VecALUOp::Eor,
                    _ => unreachable!(),
                };

                let rn = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
                let rm = put_input_in_reg(ctx, inputs[1], NarrowValueMode::None);
                let rd = out_regs.only_reg().unwrap();

                ctx.emit(Inst::VecRRR {
                    alu_op,
                    rd,
                    rn,
                    rm,
                    size: VectorSize::from_ty(ty),
                });
            }
        }

        Opcode::Ishl | Opcode::Ushr | Opcode::Sshr => {
            let out_regs = get_output_reg(ctx, outputs[0]);
            let ty = ty.unwrap();
            if ty == I128 {
                let src = put_input_in_regs(ctx, inputs[0]);
                let amt = lower_shift_amt(ctx, inputs[1], ty, out_regs.regs()[0]).unwrap_reg();

                match op {
                    Opcode::Ishl => emit_shl_i128(ctx, src, out_regs, amt),
                    Opcode::Ushr => {
                        emit_shr_i128(ctx, src, out_regs, amt, /* is_signed = */ false)
                    }
                    Opcode::Sshr => {
                        emit_shr_i128(ctx, src, out_regs, amt, /* is_signed = */ true)
                    }
                    _ => unreachable!(),
                };
            } else if !ty.is_vector() {
                let rd = out_regs.only_reg().unwrap();
                let size = OperandSize::from_bits(ty_bits(ty));
                let narrow_mode = match (op, size) {
                    (Opcode::Ishl, _) => NarrowValueMode::None,
                    (Opcode::Ushr, OperandSize::Size64) => NarrowValueMode::ZeroExtend64,
                    (Opcode::Ushr, OperandSize::Size32) => NarrowValueMode::ZeroExtend32,
                    (Opcode::Sshr, OperandSize::Size64) => NarrowValueMode::SignExtend64,
                    (Opcode::Sshr, OperandSize::Size32) => NarrowValueMode::SignExtend32,
                    _ => unreachable!(),
                };
                let rn = put_input_in_reg(ctx, inputs[0], narrow_mode);
                let rm = lower_shift_amt(ctx, inputs[1], ty, out_regs.regs()[0]);
                let alu_op = match op {
                    Opcode::Ishl => choose_32_64(ty, ALUOp::Lsl32, ALUOp::Lsl64),
                    Opcode::Ushr => choose_32_64(ty, ALUOp::Lsr32, ALUOp::Lsr64),
                    Opcode::Sshr => choose_32_64(ty, ALUOp::Asr32, ALUOp::Asr64),
                    _ => unreachable!(),
                };
                ctx.emit(alu_inst_immshift(alu_op, rd, rn, rm));
            } else {
                let rd = out_regs.only_reg().unwrap();
                let rn = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
                let size = VectorSize::from_ty(ty);
                let (alu_op, is_right_shift) = match op {
                    Opcode::Ishl => (VecALUOp::Sshl, false),
                    Opcode::Ushr => (VecALUOp::Ushl, true),
                    Opcode::Sshr => (VecALUOp::Sshl, true),
                    _ => unreachable!(),
                };

                let rm = if is_right_shift {
                    // Right shifts are implemented with a negative left shift.
                    let tmp = ctx.alloc_tmp(I32).only_reg().unwrap();
                    let rm = put_input_in_reg(ctx, inputs[1], NarrowValueMode::None);
                    let rn = zero_reg();
                    ctx.emit(Inst::AluRRR {
                        alu_op: ALUOp::Sub32,
                        rd: tmp,
                        rn,
                        rm,
                    });
                    tmp.to_reg()
                } else {
                    put_input_in_reg(ctx, inputs[1], NarrowValueMode::None)
                };

                ctx.emit(Inst::VecDup { rd, rn: rm, size });

                ctx.emit(Inst::VecRRR {
                    alu_op,
                    rd,
                    rn,
                    rm: rd.to_reg(),
                    size,
                });
            }
        }

        Opcode::Rotr | Opcode::Rotl => {
            // aarch64 doesn't have a left-rotate instruction, but a left rotation of K places is
            // effectively a right rotation of N - K places, if N is the integer's bit size. We
            // implement left rotations with this trick.
            //
            // For a 32-bit or 64-bit rotate-right, we can use the ROR instruction directly.
            //
            // For a < 32-bit rotate-right, we synthesize this as:
            //
            //    rotr rd, rn, rm
            //
            //       =>
            //
            //    zero-extend rn, <32-or-64>
            //    and tmp_masked_rm, rm, <bitwidth - 1>
            //    sub tmp1, tmp_masked_rm, <bitwidth>
            //    sub tmp1, zero, tmp1  ; neg
            //    lsr tmp2, rn, tmp_masked_rm
            //    lsl rd, rn, tmp1
            //    orr rd, rd, tmp2
            //
            // For a constant amount, we can instead do:
            //
            //    zero-extend rn, <32-or-64>
            //    lsr tmp2, rn, #<shiftimm>
            //    lsl rd, rn, <bitwidth - shiftimm>
            //    orr rd, rd, tmp2

            let is_rotl = op == Opcode::Rotl;

            let ty = ty.unwrap();
            let ty_bits_size = ty_bits(ty) as u8;

            // TODO: We can do much better codegen if we have a constant amt
            if ty == I128 {
                let dst = get_output_reg(ctx, outputs[0]);
                let src = put_input_in_regs(ctx, inputs[0]);
                let amt_src = put_input_in_regs(ctx, inputs[1]).regs()[0];

                let tmp = ctx.alloc_tmp(I128);
                let inv_amt = ctx.alloc_tmp(I64).only_reg().unwrap();

                lower_constant_u64(ctx, inv_amt, 128);
                ctx.emit(Inst::AluRRR {
                    alu_op: ALUOp::Sub64,
                    rd: inv_amt,
                    rn: inv_amt.to_reg(),
                    rm: amt_src,
                });

                if is_rotl {
                    // rotl
                    // (shl.i128 tmp, amt)
                    // (ushr.i128 dst, 128-amt)

                    emit_shl_i128(ctx, src, tmp, amt_src);
                    emit_shr_i128(
                        ctx,
                        src,
                        dst,
                        inv_amt.to_reg(),
                        /* is_signed = */ false,
                    );
                } else {
                    // rotr
                    // (ushr.i128 tmp, amt)
                    // (shl.i128 dst, 128-amt)

                    emit_shr_i128(ctx, src, tmp, amt_src, /* is_signed = */ false);
                    emit_shl_i128(ctx, src, dst, inv_amt.to_reg());
                }

                ctx.emit(Inst::AluRRR {
                    alu_op: ALUOp::Orr64,
                    rd: dst.regs()[0],
                    rn: dst.regs()[0].to_reg(),
                    rm: tmp.regs()[0].to_reg(),
                });
                ctx.emit(Inst::AluRRR {
                    alu_op: ALUOp::Orr64,
                    rd: dst.regs()[1],
                    rn: dst.regs()[1].to_reg(),
                    rm: tmp.regs()[1].to_reg(),
                });

                return Ok(());
            }

            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let rn = put_input_in_reg(
                ctx,
                inputs[0],
                if ty_bits_size <= 32 {
                    NarrowValueMode::ZeroExtend32
                } else {
                    NarrowValueMode::ZeroExtend64
                },
            );
            let rm = put_input_in_reg_immshift(ctx, inputs[1], ty_bits(ty));

            if ty_bits_size == 32 || ty_bits_size == 64 {
                let alu_op = choose_32_64(ty, ALUOp::RotR32, ALUOp::RotR64);
                match rm {
                    ResultRegImmShift::ImmShift(mut immshift) => {
                        if is_rotl {
                            immshift.imm = ty_bits_size.wrapping_sub(immshift.value());
                        }
                        immshift.imm &= ty_bits_size - 1;
                        ctx.emit(Inst::AluRRImmShift {
                            alu_op,
                            rd,
                            rn,
                            immshift,
                        });
                    }

                    ResultRegImmShift::Reg(rm) => {
                        let rm = if is_rotl {
                            // Really ty_bits_size - rn, but the upper bits of the result are
                            // ignored (because of the implicit masking done by the instruction),
                            // so this is equivalent to negating the input.
                            let alu_op = choose_32_64(ty, ALUOp::Sub32, ALUOp::Sub64);
                            let tmp = ctx.alloc_tmp(ty).only_reg().unwrap();
                            ctx.emit(Inst::AluRRR {
                                alu_op,
                                rd: tmp,
                                rn: zero_reg(),
                                rm,
                            });
                            tmp.to_reg()
                        } else {
                            rm
                        };
                        ctx.emit(Inst::AluRRR { alu_op, rd, rn, rm });
                    }
                }
            } else {
                debug_assert!(ty_bits_size < 32);

                match rm {
                    ResultRegImmShift::Reg(reg) => {
                        let reg = if is_rotl {
                            // Really ty_bits_size - rn, but the upper bits of the result are
                            // ignored (because of the implicit masking done by the instruction),
                            // so this is equivalent to negating the input.
                            let tmp = ctx.alloc_tmp(I32).only_reg().unwrap();
                            ctx.emit(Inst::AluRRR {
                                alu_op: ALUOp::Sub32,
                                rd: tmp,
                                rn: zero_reg(),
                                rm: reg,
                            });
                            tmp.to_reg()
                        } else {
                            reg
                        };

                        // Explicitly mask the rotation count.
                        let tmp_masked_rm = ctx.alloc_tmp(I32).only_reg().unwrap();
                        ctx.emit(Inst::AluRRImmLogic {
                            alu_op: ALUOp::And32,
                            rd: tmp_masked_rm,
                            rn: reg,
                            imml: ImmLogic::maybe_from_u64((ty_bits_size - 1) as u64, I32).unwrap(),
                        });
                        let tmp_masked_rm = tmp_masked_rm.to_reg();

                        let tmp1 = ctx.alloc_tmp(I32).only_reg().unwrap();
                        let tmp2 = ctx.alloc_tmp(I32).only_reg().unwrap();
                        ctx.emit(Inst::AluRRImm12 {
                            alu_op: ALUOp::Sub32,
                            rd: tmp1,
                            rn: tmp_masked_rm,
                            imm12: Imm12::maybe_from_u64(ty_bits_size as u64).unwrap(),
                        });
                        ctx.emit(Inst::AluRRR {
                            alu_op: ALUOp::Sub32,
                            rd: tmp1,
                            rn: zero_reg(),
                            rm: tmp1.to_reg(),
                        });
                        ctx.emit(Inst::AluRRR {
                            alu_op: ALUOp::Lsr32,
                            rd: tmp2,
                            rn,
                            rm: tmp_masked_rm,
                        });
                        ctx.emit(Inst::AluRRR {
                            alu_op: ALUOp::Lsl32,
                            rd,
                            rn,
                            rm: tmp1.to_reg(),
                        });
                        ctx.emit(Inst::AluRRR {
                            alu_op: ALUOp::Orr32,
                            rd,
                            rn: rd.to_reg(),
                            rm: tmp2.to_reg(),
                        });
                    }

                    ResultRegImmShift::ImmShift(mut immshift) => {
                        if is_rotl {
                            immshift.imm = ty_bits_size.wrapping_sub(immshift.value());
                        }
                        immshift.imm &= ty_bits_size - 1;

                        let tmp1 = ctx.alloc_tmp(I32).only_reg().unwrap();
                        ctx.emit(Inst::AluRRImmShift {
                            alu_op: ALUOp::Lsr32,
                            rd: tmp1,
                            rn,
                            immshift: immshift.clone(),
                        });

                        let amount = immshift.value() & (ty_bits_size - 1);
                        let opp_shift =
                            ImmShift::maybe_from_u64(ty_bits_size as u64 - amount as u64).unwrap();
                        ctx.emit(Inst::AluRRImmShift {
                            alu_op: ALUOp::Lsl32,
                            rd,
                            rn,
                            immshift: opp_shift,
                        });

                        ctx.emit(Inst::AluRRR {
                            alu_op: ALUOp::Orr32,
                            rd,
                            rn: rd.to_reg(),
                            rm: tmp1.to_reg(),
                        });
                    }
                }
            }
        }

        Opcode::Bitrev | Opcode::Clz | Opcode::Cls | Opcode::Ctz => {
            let ty = ty.unwrap();
            let op_ty = match ty {
                I8 | I16 | I32 => I32,
                I64 | I128 => I64,
                _ => panic!("Unsupported type for Bitrev/Clz/Cls"),
            };
            let bitop = match op {
                Opcode::Clz | Opcode::Cls | Opcode::Bitrev => BitOp::from((op, op_ty)),
                Opcode::Ctz => BitOp::from((Opcode::Bitrev, op_ty)),
                _ => unreachable!(),
            };

            if ty == I128 {
                let out_regs = get_output_reg(ctx, outputs[0]);
                let in_regs = put_input_in_regs(ctx, inputs[0]);

                let in_lo = in_regs.regs()[0];
                let in_hi = in_regs.regs()[1];
                let out_lo = out_regs.regs()[0];
                let out_hi = out_regs.regs()[1];

                if op == Opcode::Bitrev || op == Opcode::Ctz {
                    ctx.emit(Inst::BitRR {
                        rd: out_hi,
                        rn: in_lo,
                        op: bitop,
                    });
                    ctx.emit(Inst::BitRR {
                        rd: out_lo,
                        rn: in_hi,
                        op: bitop,
                    });
                }

                if op == Opcode::Ctz {
                    // We have reduced the problem to a clz by reversing the inputs previouly
                    emit_clz_i128(ctx, out_regs.map(|r| r.to_reg()), out_regs);
                } else if op == Opcode::Clz {
                    emit_clz_i128(ctx, in_regs, out_regs);
                } else if op == Opcode::Cls {
                    // cls out_hi, in_hi
                    // cls out_lo, in_lo
                    // eon sign_eq, in_hi, in_lo
                    // lsr sign_eq, sign_eq, #63
                    // madd out_lo, out_lo, sign_eq, sign_eq
                    // cmp out_hi, #63
                    // csel out_lo, out_lo, xzr, eq
                    // add  out_lo, out_lo, out_hi
                    // mov  out_hi, 0

                    let sign_eq = ctx.alloc_tmp(I64).only_reg().unwrap();
                    let xzr = writable_zero_reg();

                    ctx.emit(Inst::BitRR {
                        rd: out_lo,
                        rn: in_lo,
                        op: bitop,
                    });
                    ctx.emit(Inst::BitRR {
                        rd: out_hi,
                        rn: in_hi,
                        op: bitop,
                    });
                    ctx.emit(Inst::AluRRR {
                        alu_op: ALUOp::EorNot64,
                        rd: sign_eq,
                        rn: in_hi,
                        rm: in_lo,
                    });
                    ctx.emit(Inst::AluRRImmShift {
                        alu_op: ALUOp::Lsr64,
                        rd: sign_eq,
                        rn: sign_eq.to_reg(),
                        immshift: ImmShift::maybe_from_u64(63).unwrap(),
                    });
                    ctx.emit(Inst::AluRRRR {
                        alu_op: ALUOp3::MAdd64,
                        rd: out_lo,
                        rn: out_lo.to_reg(),
                        rm: sign_eq.to_reg(),
                        ra: sign_eq.to_reg(),
                    });
                    ctx.emit(Inst::AluRRImm12 {
                        alu_op: ALUOp::SubS64,
                        rd: xzr,
                        rn: out_hi.to_reg(),
                        imm12: Imm12::maybe_from_u64(63).unwrap(),
                    });
                    ctx.emit(Inst::CSel {
                        cond: Cond::Eq,
                        rd: out_lo,
                        rn: out_lo.to_reg(),
                        rm: xzr.to_reg(),
                    });
                    ctx.emit(Inst::AluRRR {
                        alu_op: ALUOp::Add64,
                        rd: out_lo,
                        rn: out_lo.to_reg(),
                        rm: out_hi.to_reg(),
                    });
                    lower_constant_u64(ctx, out_hi, 0);
                }
            } else {
                let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
                let needs_zext = match op {
                    Opcode::Bitrev | Opcode::Ctz => false,
                    Opcode::Clz | Opcode::Cls => true,
                    _ => unreachable!(),
                };
                let narrow_mode = if needs_zext && ty_bits(ty) == 64 {
                    NarrowValueMode::ZeroExtend64
                } else if needs_zext {
                    NarrowValueMode::ZeroExtend32
                } else {
                    NarrowValueMode::None
                };
                let rn = put_input_in_reg(ctx, inputs[0], narrow_mode);

                ctx.emit(Inst::BitRR { rd, rn, op: bitop });

                // Both bitrev and ctz use a bit-reverse (rbit) instruction; ctz to reduce the problem
                // to a clz, and bitrev as the main operation.
                if op == Opcode::Bitrev || op == Opcode::Ctz {
                    // Reversing an n-bit value (n < 32) with a 32-bit bitrev instruction will place
                    // the reversed result in the highest n bits, so we need to shift them down into
                    // place.
                    let right_shift = match ty {
                        I8 => Some(24),
                        I16 => Some(16),
                        I32 => None,
                        I64 => None,
                        _ => panic!("Unsupported type for Bitrev"),
                    };
                    if let Some(s) = right_shift {
                        ctx.emit(Inst::AluRRImmShift {
                            alu_op: ALUOp::Lsr32,
                            rd,
                            rn: rd.to_reg(),
                            immshift: ImmShift::maybe_from_u64(s).unwrap(),
                        });
                    }
                }

                if op == Opcode::Ctz {
                    ctx.emit(Inst::BitRR {
                        op: BitOp::from((Opcode::Clz, op_ty)),
                        rd,
                        rn: rd.to_reg(),
                    });
                }
            }
        }

        Opcode::Popcnt => {
            let ty = ty.unwrap();

            if ty.is_vector() {
                let lane_type = ty.lane_type();
                let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
                let rn = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);

                if lane_type != I8 {
                    return Err(CodegenError::Unsupported(format!(
                        "Unsupported SIMD vector lane type: {:?}",
                        lane_type
                    )));
                }

                ctx.emit(Inst::VecMisc {
                    op: VecMisc2::Cnt,
                    rd,
                    rn,
                    size: VectorSize::from_ty(ty),
                });
            } else {
                let out_regs = get_output_reg(ctx, outputs[0]);
                let in_regs = put_input_in_regs(ctx, inputs[0]);
                let size = if ty == I128 {
                    ScalarSize::Size64
                } else {
                    ScalarSize::from_operand_size(OperandSize::from_ty(ty))
                };

                let vec_size = if ty == I128 {
                    VectorSize::Size8x16
                } else {
                    VectorSize::Size8x8
                };

                let tmp = ctx.alloc_tmp(I8X16).only_reg().unwrap();

                // fmov tmp, in_lo
                // if ty == i128:
                //     mov tmp.d[1], in_hi
                //
                // cnt tmp.16b, tmp.16b / cnt tmp.8b, tmp.8b
                // addv tmp, tmp.16b / addv tmp, tmp.8b / addp tmp.8b, tmp.8b, tmp.8b / (no instruction for 8-bit inputs)
                //
                // umov out_lo, tmp.b[0]
                // if ty == i128:
                //     mov out_hi, 0

                ctx.emit(Inst::MovToFpu {
                    rd: tmp,
                    rn: in_regs.regs()[0],
                    size,
                });

                if ty == I128 {
                    ctx.emit(Inst::MovToVec {
                        rd: tmp,
                        rn: in_regs.regs()[1],
                        idx: 1,
                        size: VectorSize::Size64x2,
                    });
                }

                ctx.emit(Inst::VecMisc {
                    op: VecMisc2::Cnt,
                    rd: tmp,
                    rn: tmp.to_reg(),
                    size: vec_size,
                });

                match ScalarSize::from_ty(ty) {
                    ScalarSize::Size8 => {}
                    ScalarSize::Size16 => {
                        // ADDP is usually cheaper than ADDV.
                        ctx.emit(Inst::VecRRR {
                            alu_op: VecALUOp::Addp,
                            rd: tmp,
                            rn: tmp.to_reg(),
                            rm: tmp.to_reg(),
                            size: VectorSize::Size8x8,
                        });
                    }
                    ScalarSize::Size32 | ScalarSize::Size64 | ScalarSize::Size128 => {
                        ctx.emit(Inst::VecLanes {
                            op: VecLanesOp::Addv,
                            rd: tmp,
                            rn: tmp.to_reg(),
                            size: vec_size,
                        });
                    }
                }

                ctx.emit(Inst::MovFromVec {
                    rd: out_regs.regs()[0],
                    rn: tmp.to_reg(),
                    idx: 0,
                    size: VectorSize::Size8x16,
                });

                if ty == I128 {
                    lower_constant_u64(ctx, out_regs.regs()[1], 0);
                }
            }
        }

        Opcode::Load
        | Opcode::Uload8
        | Opcode::Sload8
        | Opcode::Uload16
        | Opcode::Sload16
        | Opcode::Uload32
        | Opcode::Sload32
        | Opcode::LoadComplex
        | Opcode::Uload8Complex
        | Opcode::Sload8Complex
        | Opcode::Uload16Complex
        | Opcode::Sload16Complex
        | Opcode::Uload32Complex
        | Opcode::Sload32Complex
        | Opcode::Sload8x8
        | Opcode::Uload8x8
        | Opcode::Sload16x4
        | Opcode::Uload16x4
        | Opcode::Sload32x2
        | Opcode::Uload32x2
        | Opcode::Uload8x8Complex
        | Opcode::Sload8x8Complex
        | Opcode::Uload16x4Complex
        | Opcode::Sload16x4Complex
        | Opcode::Uload32x2Complex
        | Opcode::Sload32x2Complex => {
            let sign_extend = match op {
                Opcode::Sload8
                | Opcode::Sload8Complex
                | Opcode::Sload16
                | Opcode::Sload16Complex
                | Opcode::Sload32
                | Opcode::Sload32Complex => true,
                _ => false,
            };
            let flags = ctx
                .memflags(insn)
                .expect("Load instruction should have memflags");

            let out_ty = ctx.output_ty(insn, 0);
            if out_ty == I128 {
                let off = ctx.data(insn).load_store_offset().unwrap();
                let mem = lower_pair_address(ctx, &inputs[..], off);
                let dst = get_output_reg(ctx, outputs[0]);
                ctx.emit(Inst::LoadP64 {
                    rt: dst.regs()[0],
                    rt2: dst.regs()[1],
                    mem,
                    flags,
                });
            } else {
                lower_load(
                    ctx,
                    insn,
                    &inputs[..],
                    outputs[0],
                    |ctx, dst, elem_ty, mem| {
                        let rd = dst.only_reg().unwrap();
                        let is_float = ty_has_float_or_vec_representation(elem_ty);
                        ctx.emit(match (ty_bits(elem_ty), sign_extend, is_float) {
                            (1, _, _) => Inst::ULoad8 { rd, mem, flags },
                            (8, false, _) => Inst::ULoad8 { rd, mem, flags },
                            (8, true, _) => Inst::SLoad8 { rd, mem, flags },
                            (16, false, _) => Inst::ULoad16 { rd, mem, flags },
                            (16, true, _) => Inst::SLoad16 { rd, mem, flags },
                            (32, false, false) => Inst::ULoad32 { rd, mem, flags },
                            (32, true, false) => Inst::SLoad32 { rd, mem, flags },
                            (32, _, true) => Inst::FpuLoad32 { rd, mem, flags },
                            (64, _, false) => Inst::ULoad64 { rd, mem, flags },
                            // Note that we treat some of the vector loads as scalar floating-point loads,
                            // which is correct in a little endian environment.
                            (64, _, true) => Inst::FpuLoad64 { rd, mem, flags },
                            (128, _, true) => Inst::FpuLoad128 { rd, mem, flags },
                            _ => panic!("Unsupported size in load"),
                        });

                        let vec_extend = match op {
                            Opcode::Sload8x8 => Some(VecExtendOp::Sxtl8),
                            Opcode::Sload8x8Complex => Some(VecExtendOp::Sxtl8),
                            Opcode::Uload8x8 => Some(VecExtendOp::Uxtl8),
                            Opcode::Uload8x8Complex => Some(VecExtendOp::Uxtl8),
                            Opcode::Sload16x4 => Some(VecExtendOp::Sxtl16),
                            Opcode::Sload16x4Complex => Some(VecExtendOp::Sxtl16),
                            Opcode::Uload16x4 => Some(VecExtendOp::Uxtl16),
                            Opcode::Uload16x4Complex => Some(VecExtendOp::Uxtl16),
                            Opcode::Sload32x2 => Some(VecExtendOp::Sxtl32),
                            Opcode::Sload32x2Complex => Some(VecExtendOp::Sxtl32),
                            Opcode::Uload32x2 => Some(VecExtendOp::Uxtl32),
                            Opcode::Uload32x2Complex => Some(VecExtendOp::Uxtl32),
                            _ => None,
                        };

                        if let Some(t) = vec_extend {
                            let rd = dst.only_reg().unwrap();
                            ctx.emit(Inst::VecExtend {
                                t,
                                rd,
                                rn: rd.to_reg(),
                                high_half: false,
                            });
                        }
                    },
                );
            }
        }

        Opcode::Store
        | Opcode::Istore8
        | Opcode::Istore16
        | Opcode::Istore32
        | Opcode::StoreComplex
        | Opcode::Istore8Complex
        | Opcode::Istore16Complex
        | Opcode::Istore32Complex => {
            let off = ctx.data(insn).load_store_offset().unwrap();
            let elem_ty = match op {
                Opcode::Istore8 | Opcode::Istore8Complex => I8,
                Opcode::Istore16 | Opcode::Istore16Complex => I16,
                Opcode::Istore32 | Opcode::Istore32Complex => I32,
                Opcode::Store | Opcode::StoreComplex => ctx.input_ty(insn, 0),
                _ => unreachable!(),
            };
            let is_float = ty_has_float_or_vec_representation(elem_ty);
            let flags = ctx
                .memflags(insn)
                .expect("Store instruction should have memflags");

            let dst = put_input_in_regs(ctx, inputs[0]);

            if elem_ty == I128 {
                let mem = lower_pair_address(ctx, &inputs[1..], off);
                ctx.emit(Inst::StoreP64 {
                    rt: dst.regs()[0],
                    rt2: dst.regs()[1],
                    mem,
                    flags,
                });
            } else {
                let rd = dst.only_reg().unwrap();
                let mem = lower_address(ctx, elem_ty, &inputs[1..], off);
                ctx.emit(match (ty_bits(elem_ty), is_float) {
                    (1, _) | (8, _) => Inst::Store8 { rd, mem, flags },
                    (16, _) => Inst::Store16 { rd, mem, flags },
                    (32, false) => Inst::Store32 { rd, mem, flags },
                    (32, true) => Inst::FpuStore32 { rd, mem, flags },
                    (64, false) => Inst::Store64 { rd, mem, flags },
                    (64, true) => Inst::FpuStore64 { rd, mem, flags },
                    (128, _) => Inst::FpuStore128 { rd, mem, flags },
                    _ => panic!("Unsupported size in store"),
                });
            }
        }

        Opcode::StackAddr => {
            let (stack_slot, offset) = match *ctx.data(insn) {
                InstructionData::StackLoad {
                    opcode: Opcode::StackAddr,
                    stack_slot,
                    offset,
                } => (stack_slot, offset),
                _ => unreachable!(),
            };
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let offset: i32 = offset.into();
            let inst = ctx
                .abi()
                .stackslot_addr(stack_slot, u32::try_from(offset).unwrap(), rd);
            ctx.emit(inst);
        }

        Opcode::AtomicRmw => {
            let r_dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let mut r_addr = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
            let mut r_arg2 = put_input_in_reg(ctx, inputs[1], NarrowValueMode::None);
            let ty_access = ty.unwrap();
            assert!(is_valid_atomic_transaction_ty(ty_access));
            // Make sure that both args are in virtual regs, since in effect
            // we have to do a parallel copy to get them safely to the AtomicRMW input
            // regs, and that's not guaranteed safe if either is in a real reg.
            r_addr = ctx.ensure_in_vreg(r_addr, I64);
            r_arg2 = ctx.ensure_in_vreg(r_arg2, I64);
            // Move the args to the preordained AtomicRMW input regs
            ctx.emit(Inst::gen_move(Writable::from_reg(xreg(25)), r_addr, I64));
            ctx.emit(Inst::gen_move(Writable::from_reg(xreg(26)), r_arg2, I64));
            // Now the AtomicRMW insn itself
            let op = inst_common::AtomicRmwOp::from(ctx.data(insn).atomic_rmw_op().unwrap());
            ctx.emit(Inst::AtomicRMW { ty: ty_access, op });
            // And finally, copy the preordained AtomicRMW output reg to its destination.
            ctx.emit(Inst::gen_move(r_dst, xreg(27), I64));
            // Also, x24 and x28 are trashed.  `fn aarch64_get_regs` must mention that.
        }

        Opcode::AtomicCas => {
            let r_dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let mut r_addr = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
            let mut r_expected = put_input_in_reg(ctx, inputs[1], NarrowValueMode::None);
            let mut r_replacement = put_input_in_reg(ctx, inputs[2], NarrowValueMode::None);
            let ty_access = ty.unwrap();
            assert!(is_valid_atomic_transaction_ty(ty_access));

            if isa_flags.use_lse() {
                ctx.emit(Inst::gen_move(r_dst, r_expected, ty_access));
                ctx.emit(Inst::AtomicCAS {
                    rs: r_dst,
                    rt: r_replacement,
                    rn: r_addr,
                    ty: ty_access,
                });
            } else {
                // This is very similar to, but not identical to, the AtomicRmw case.  Note
                // that the AtomicCASLoop sequence does its own masking, so we don't need to worry
                // about zero-extending narrow (I8/I16/I32) values here.
                // Make sure that all three args are in virtual regs.  See corresponding comment
                // for `Opcode::AtomicRmw` above.
                r_addr = ctx.ensure_in_vreg(r_addr, I64);
                r_expected = ctx.ensure_in_vreg(r_expected, I64);
                r_replacement = ctx.ensure_in_vreg(r_replacement, I64);
                // Move the args to the preordained AtomicCASLoop input regs
                ctx.emit(Inst::gen_move(Writable::from_reg(xreg(25)), r_addr, I64));
                ctx.emit(Inst::gen_move(
                    Writable::from_reg(xreg(26)),
                    r_expected,
                    I64,
                ));
                ctx.emit(Inst::gen_move(
                    Writable::from_reg(xreg(28)),
                    r_replacement,
                    I64,
                ));
                // Now the AtomicCASLoop itself, implemented in the normal way, with an LL-SC loop
                ctx.emit(Inst::AtomicCASLoop { ty: ty_access });
                // And finally, copy the preordained AtomicCASLoop output reg to its destination.
                ctx.emit(Inst::gen_move(r_dst, xreg(27), I64));
                // Also, x24 and x28 are trashed.  `fn aarch64_get_regs` must mention that.
            }
        }

        Opcode::AtomicLoad => {
            let r_data = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let r_addr = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
            let ty_access = ty.unwrap();
            assert!(is_valid_atomic_transaction_ty(ty_access));
            ctx.emit(Inst::AtomicLoad {
                ty: ty_access,
                r_data,
                r_addr,
            });
        }

        Opcode::AtomicStore => {
            let r_data = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
            let r_addr = put_input_in_reg(ctx, inputs[1], NarrowValueMode::None);
            let ty_access = ctx.input_ty(insn, 0);
            assert!(is_valid_atomic_transaction_ty(ty_access));
            ctx.emit(Inst::AtomicStore {
                ty: ty_access,
                r_data,
                r_addr,
            });
        }

        Opcode::Fence => {
            ctx.emit(Inst::Fence {});
        }

        Opcode::StackLoad | Opcode::StackStore => {
            panic!("Direct stack memory access not supported; should not be used by Wasm");
        }

        Opcode::HeapAddr => {
            panic!("heap_addr should have been removed by legalization!");
        }

        Opcode::TableAddr => {
            panic!("table_addr should have been removed by legalization!");
        }

        Opcode::Nop => {
            // Nothing.
        }

        Opcode::Select => {
            let flag_input = inputs[0];
            let cond = if let Some(icmp_insn) =
                maybe_input_insn_via_conv(ctx, flag_input, Opcode::Icmp, Opcode::Bint)
            {
                let condcode = ctx.data(icmp_insn).cond_code().unwrap();
                lower_icmp(ctx, icmp_insn, condcode, IcmpOutput::Flags)?.unwrap_cond()
            } else if let Some(fcmp_insn) =
                maybe_input_insn_via_conv(ctx, flag_input, Opcode::Fcmp, Opcode::Bint)
            {
                let condcode = ctx.data(fcmp_insn).fp_cond_code().unwrap();
                let cond = lower_fp_condcode(condcode);
                lower_fcmp_or_ffcmp_to_flags(ctx, fcmp_insn);
                cond
            } else {
                let (cmp_op, narrow_mode) = if ty_bits(ctx.input_ty(insn, 0)) > 32 {
                    (ALUOp::SubS64, NarrowValueMode::ZeroExtend64)
                } else {
                    (ALUOp::SubS32, NarrowValueMode::ZeroExtend32)
                };

                let rcond = put_input_in_reg(ctx, inputs[0], narrow_mode);
                // cmp rcond, #0
                ctx.emit(Inst::AluRRR {
                    alu_op: cmp_op,
                    rd: writable_zero_reg(),
                    rn: rcond,
                    rm: zero_reg(),
                });
                Cond::Ne
            };

            // csel.cond rd, rn, rm
            let ty = ctx.output_ty(insn, 0);
            let bits = ty_bits(ty);
            let is_float = ty_has_float_or_vec_representation(ty);

            let dst = get_output_reg(ctx, outputs[0]);
            let lhs = put_input_in_regs(ctx, inputs[1]);
            let rhs = put_input_in_regs(ctx, inputs[2]);

            let rd = dst.regs()[0];
            let rn = lhs.regs()[0];
            let rm = rhs.regs()[0];

            match (is_float, bits) {
                (true, 32) => ctx.emit(Inst::FpuCSel32 { cond, rd, rn, rm }),
                (true, 64) => ctx.emit(Inst::FpuCSel64 { cond, rd, rn, rm }),
                (true, 128) => ctx.emit(Inst::VecCSel { cond, rd, rn, rm }),
                (false, 128) => {
                    ctx.emit(Inst::CSel {
                        cond,
                        rd: dst.regs()[0],
                        rn: lhs.regs()[0],
                        rm: rhs.regs()[0],
                    });
                    ctx.emit(Inst::CSel {
                        cond,
                        rd: dst.regs()[1],
                        rn: lhs.regs()[1],
                        rm: rhs.regs()[1],
                    });
                }
                (_, _) => ctx.emit(Inst::CSel { cond, rd, rn, rm }),
            }
        }

        Opcode::Selectif | Opcode::SelectifSpectreGuard => {
            let condcode = ctx.data(insn).cond_code().unwrap();
            // Verification ensures that the input is always a
            // single-def ifcmp.
            let ifcmp_insn = maybe_input_insn(ctx, inputs[0], Opcode::Ifcmp).unwrap();
            let cond = lower_icmp(ctx, ifcmp_insn, condcode, IcmpOutput::Flags)?.unwrap_cond();

            // csel.COND rd, rn, rm
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let rn = put_input_in_reg(ctx, inputs[1], NarrowValueMode::None);
            let rm = put_input_in_reg(ctx, inputs[2], NarrowValueMode::None);
            let ty = ctx.output_ty(insn, 0);
            let bits = ty_bits(ty);
            let is_float = ty_has_float_or_vec_representation(ty);
            if is_float && bits == 32 {
                ctx.emit(Inst::FpuCSel32 { cond, rd, rn, rm });
            } else if is_float && bits == 64 {
                ctx.emit(Inst::FpuCSel64 { cond, rd, rn, rm });
            } else {
                ctx.emit(Inst::CSel { cond, rd, rn, rm });
            }
        }

        Opcode::Bitselect | Opcode::Vselect => {
            let ty = ty.unwrap();
            if !ty.is_vector() {
                debug_assert_ne!(Opcode::Vselect, op);
                let tmp = ctx.alloc_tmp(I64).only_reg().unwrap();
                let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
                let rcond = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
                let rn = put_input_in_reg(ctx, inputs[1], NarrowValueMode::None);
                let rm = put_input_in_reg(ctx, inputs[2], NarrowValueMode::None);
                // AND rTmp, rn, rcond
                ctx.emit(Inst::AluRRR {
                    alu_op: ALUOp::And64,
                    rd: tmp,
                    rn,
                    rm: rcond,
                });
                // BIC rd, rm, rcond
                ctx.emit(Inst::AluRRR {
                    alu_op: ALUOp::AndNot64,
                    rd,
                    rn: rm,
                    rm: rcond,
                });
                // ORR rd, rd, rTmp
                ctx.emit(Inst::AluRRR {
                    alu_op: ALUOp::Orr64,
                    rd,
                    rn: rd.to_reg(),
                    rm: tmp.to_reg(),
                });
            } else {
                let rcond = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
                let rn = put_input_in_reg(ctx, inputs[1], NarrowValueMode::None);
                let rm = put_input_in_reg(ctx, inputs[2], NarrowValueMode::None);
                let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
                ctx.emit(Inst::gen_move(rd, rcond, ty));

                ctx.emit(Inst::VecRRR {
                    alu_op: VecALUOp::Bsl,
                    rd,
                    rn,
                    rm,
                    size: VectorSize::from_ty(ty),
                });
            }
        }

        Opcode::Trueif => {
            let condcode = ctx.data(insn).cond_code().unwrap();
            // Verification ensures that the input is always a
            // single-def ifcmp.
            let ifcmp_insn = maybe_input_insn(ctx, inputs[0], Opcode::Ifcmp).unwrap();
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            lower_icmp(ctx, ifcmp_insn, condcode, IcmpOutput::Register(rd))?;
        }

        Opcode::Trueff => {
            let condcode = ctx.data(insn).fp_cond_code().unwrap();
            let cond = lower_fp_condcode(condcode);
            let ffcmp_insn = maybe_input_insn(ctx, inputs[0], Opcode::Ffcmp).unwrap();
            lower_fcmp_or_ffcmp_to_flags(ctx, ffcmp_insn);
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            materialize_bool_result(ctx, insn, rd, cond);
        }

        Opcode::IsNull | Opcode::IsInvalid => {
            // Null references are represented by the constant value 0; invalid references are
            // represented by the constant value -1. See `define_reftypes()` in
            // `meta/src/isa/x86/encodings.rs` to confirm.
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let rn = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
            let ty = ctx.input_ty(insn, 0);
            let (alu_op, const_value) = match op {
                Opcode::IsNull => {
                    // cmp rn, #0
                    (choose_32_64(ty, ALUOp::SubS32, ALUOp::SubS64), 0)
                }
                Opcode::IsInvalid => {
                    // cmn rn, #1
                    (choose_32_64(ty, ALUOp::AddS32, ALUOp::AddS64), 1)
                }
                _ => unreachable!(),
            };
            let const_value = ResultRSEImm12::Imm12(Imm12::maybe_from_u64(const_value).unwrap());
            ctx.emit(alu_inst_imm12(alu_op, writable_zero_reg(), rn, const_value));
            materialize_bool_result(ctx, insn, rd, Cond::Eq);
        }

        Opcode::Copy => {
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let rn = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
            let ty = ctx.input_ty(insn, 0);
            ctx.emit(Inst::gen_move(rd, rn, ty));
        }

        Opcode::Breduce | Opcode::Ireduce => {
            // Smaller integers/booleans are stored with high-order bits
            // undefined, so we can simply do a copy.
            let rn = put_input_in_regs(ctx, inputs[0]).regs()[0];
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let ty = ctx.input_ty(insn, 0);
            ctx.emit(Inst::gen_move(rd, rn, ty));
        }

        Opcode::Bextend | Opcode::Bmask => {
            // Bextend and Bmask both simply sign-extend. This works for:
            // - Bextend, because booleans are stored as 0 / -1, so we
            //   sign-extend the -1 to a -1 in the wider width.
            // - Bmask, because the resulting integer mask value must be
            //   all-ones (-1) if the argument is true.

            let from_ty = ctx.input_ty(insn, 0);
            let to_ty = ctx.output_ty(insn, 0);
            let from_bits = ty_bits(from_ty);
            let to_bits = ty_bits(to_ty);

            assert!(
                from_bits <= 64 && to_bits <= 64,
                "Vector Bextend not supported yet"
            );
            assert!(from_bits <= to_bits);

            if from_bits == to_bits {
                // Nothing.
            } else {
                let rn = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
                let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
                let to_bits = if to_bits == 64 {
                    64
                } else {
                    assert!(to_bits <= 32);
                    32
                };
                let from_bits = from_bits as u8;
                ctx.emit(Inst::Extend {
                    rd,
                    rn,
                    signed: true,
                    from_bits,
                    to_bits,
                });
            }
        }

        Opcode::Bint => {
            // Booleans are stored as all-zeroes (0) or all-ones (-1). We AND
            // out the LSB to give a 0 / 1-valued integer result.
            let rn = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let output_bits = ty_bits(ctx.output_ty(insn, 0));

            let (imm_ty, alu_op) = if output_bits > 32 {
                (I64, ALUOp::And64)
            } else {
                (I32, ALUOp::And32)
            };
            ctx.emit(Inst::AluRRImmLogic {
                alu_op,
                rd,
                rn,
                imml: ImmLogic::maybe_from_u64(1, imm_ty).unwrap(),
            });
        }

        Opcode::Bitcast => {
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let ity = ctx.input_ty(insn, 0);
            let oty = ctx.output_ty(insn, 0);
            let ity_bits = ty_bits(ity);
            let ity_vec_reg = ty_has_float_or_vec_representation(ity);
            let oty_bits = ty_bits(oty);
            let oty_vec_reg = ty_has_float_or_vec_representation(oty);

            debug_assert_eq!(ity_bits, oty_bits);

            match (ity_vec_reg, oty_vec_reg) {
                (true, true) => {
                    let narrow_mode = if ity_bits <= 32 {
                        NarrowValueMode::ZeroExtend32
                    } else {
                        NarrowValueMode::ZeroExtend64
                    };
                    let rm = put_input_in_reg(ctx, inputs[0], narrow_mode);
                    ctx.emit(Inst::gen_move(rd, rm, oty));
                }
                (false, false) => {
                    let rm = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
                    ctx.emit(Inst::gen_move(rd, rm, oty));
                }
                (false, true) => {
                    let rn = put_input_in_reg(ctx, inputs[0], NarrowValueMode::ZeroExtend64);
                    ctx.emit(Inst::MovToFpu {
                        rd,
                        rn,
                        size: ScalarSize::Size64,
                    });
                }
                (true, false) => {
                    let rn = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
                    let size = VectorSize::from_lane_size(ScalarSize::from_bits(oty_bits), true);

                    ctx.emit(Inst::MovFromVec {
                        rd,
                        rn,
                        idx: 0,
                        size,
                    });
                }
            }
        }

        Opcode::FallthroughReturn | Opcode::Return => {
            for (i, input) in inputs.iter().enumerate() {
                // N.B.: according to the AArch64 ABI, the top bits of a register
                // (above the bits for the value's type) are undefined, so we
                // need not extend the return values.
                let src_regs = put_input_in_regs(ctx, *input);
                let retval_regs = ctx.retval(i);

                assert_eq!(src_regs.len(), retval_regs.len());
                let ty = ctx.input_ty(insn, i);
                let (_, tys) = Inst::rc_for_type(ty)?;

                src_regs
                    .regs()
                    .iter()
                    .zip(retval_regs.regs().iter())
                    .zip(tys.iter())
                    .for_each(|((&src, &dst), &ty)| {
                        ctx.emit(Inst::gen_move(dst, src, ty));
                    });
            }
            // N.B.: the Ret itself is generated by the ABI.
        }

        Opcode::Ifcmp | Opcode::Ffcmp => {
            // An Ifcmp/Ffcmp must always be seen as a use of a brif/brff or trueif/trueff
            // instruction. This will always be the case as long as the IR uses an Ifcmp/Ffcmp from
            // the same block, or a dominating block. In other words, it cannot pass through a BB
            // param (phi). The flags pass of the verifier will ensure this.
            panic!("Should never reach ifcmp as isel root!");
        }

        Opcode::Icmp => {
            let condcode = ctx.data(insn).cond_code().unwrap();
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            lower_icmp(ctx, insn, condcode, IcmpOutput::Register(rd))?;
        }

        Opcode::Fcmp => {
            let condcode = ctx.data(insn).fp_cond_code().unwrap();
            let cond = lower_fp_condcode(condcode);
            let ty = ctx.input_ty(insn, 0);
            let rn = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
            let rm = put_input_in_reg(ctx, inputs[1], NarrowValueMode::None);
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();

            if !ty.is_vector() {
                match ty_bits(ty) {
                    32 => {
                        ctx.emit(Inst::FpuCmp32 { rn, rm });
                    }
                    64 => {
                        ctx.emit(Inst::FpuCmp64 { rn, rm });
                    }
                    _ => panic!("Bad float size"),
                }
                materialize_bool_result(ctx, insn, rd, cond);
            } else {
                lower_vector_compare(ctx, rd, rn, rm, ty, cond)?;
            }
        }

        Opcode::JumpTableEntry | Opcode::JumpTableBase => {
            panic!("Should not appear: we handle BrTable directly");
        }

        Opcode::Debugtrap => {
            ctx.emit(Inst::Brk);
        }

        Opcode::Trap | Opcode::ResumableTrap => {
            let trap_code = ctx.data(insn).trap_code().unwrap();
            ctx.emit_safepoint(Inst::Udf { trap_code });
        }

        Opcode::Trapif | Opcode::Trapff => {
            let trap_code = ctx.data(insn).trap_code().unwrap();

            let cond = if maybe_input_insn(ctx, inputs[0], Opcode::IaddIfcout).is_some() {
                let condcode = ctx.data(insn).cond_code().unwrap();
                let cond = lower_condcode(condcode);
                // The flags must not have been clobbered by any other
                // instruction between the iadd_ifcout and this instruction, as
                // verified by the CLIF validator; so we can simply use the
                // flags here.
                cond
            } else if op == Opcode::Trapif {
                let condcode = ctx.data(insn).cond_code().unwrap();

                // Verification ensures that the input is always a single-def ifcmp.
                let ifcmp_insn = maybe_input_insn(ctx, inputs[0], Opcode::Ifcmp).unwrap();
                lower_icmp(ctx, ifcmp_insn, condcode, IcmpOutput::Flags)?.unwrap_cond()
            } else {
                let condcode = ctx.data(insn).fp_cond_code().unwrap();
                let cond = lower_fp_condcode(condcode);

                // Verification ensures that the input is always a
                // single-def ffcmp.
                let ffcmp_insn = maybe_input_insn(ctx, inputs[0], Opcode::Ffcmp).unwrap();
                lower_fcmp_or_ffcmp_to_flags(ctx, ffcmp_insn);
                cond
            };

            ctx.emit_safepoint(Inst::TrapIf {
                trap_code,
                kind: CondBrKind::Cond(cond),
            });
        }

        Opcode::Safepoint => {
            panic!("safepoint instructions not used by new backend's safepoints!");
        }

        Opcode::Trapz | Opcode::Trapnz | Opcode::ResumableTrapnz => {
            panic!("trapz / trapnz / resumable_trapnz should have been removed by legalization!");
        }

        Opcode::FuncAddr => {
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let (extname, _) = ctx.call_target(insn).unwrap();
            let extname = extname.clone();
            ctx.emit(Inst::LoadExtName {
                rd,
                name: Box::new(extname),
                offset: 0,
            });
        }

        Opcode::GlobalValue => {
            panic!("global_value should have been removed by legalization!");
        }

        Opcode::SymbolValue => {
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let (extname, _, offset) = ctx.symbol_value(insn).unwrap();
            let extname = extname.clone();
            ctx.emit(Inst::LoadExtName {
                rd,
                name: Box::new(extname),
                offset,
            });
        }

        Opcode::Call | Opcode::CallIndirect => {
            let caller_conv = ctx.abi().call_conv();
            let (mut abi, inputs) = match op {
                Opcode::Call => {
                    let (extname, dist) = ctx.call_target(insn).unwrap();
                    let extname = extname.clone();
                    let sig = ctx.call_sig(insn).unwrap();
                    assert!(inputs.len() == sig.params.len());
                    assert!(outputs.len() == sig.returns.len());
                    (
                        AArch64ABICaller::from_func(sig, &extname, dist, caller_conv, flags)?,
                        &inputs[..],
                    )
                }
                Opcode::CallIndirect => {
                    let ptr = put_input_in_reg(ctx, inputs[0], NarrowValueMode::ZeroExtend64);
                    let sig = ctx.call_sig(insn).unwrap();
                    assert!(inputs.len() - 1 == sig.params.len());
                    assert!(outputs.len() == sig.returns.len());
                    (
                        AArch64ABICaller::from_ptr(sig, ptr, op, caller_conv, flags)?,
                        &inputs[1..],
                    )
                }
                _ => unreachable!(),
            };

            abi.emit_stack_pre_adjust(ctx);
            assert!(inputs.len() == abi.num_args());
            for i in abi.get_copy_to_arg_order() {
                let input = inputs[i];
                let arg_regs = put_input_in_regs(ctx, input);
                abi.emit_copy_regs_to_arg(ctx, i, arg_regs);
            }
            abi.emit_call(ctx);
            for (i, output) in outputs.iter().enumerate() {
                let retval_regs = get_output_reg(ctx, *output);
                abi.emit_copy_retval_to_regs(ctx, i, retval_regs);
            }
            abi.emit_stack_post_adjust(ctx);
        }

        Opcode::GetPinnedReg => {
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            ctx.emit(Inst::gen_move(rd, xreg(PINNED_REG), I64));
        }

        Opcode::SetPinnedReg => {
            let rm = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
            ctx.emit(Inst::gen_move(writable_xreg(PINNED_REG), rm, I64));
        }

        Opcode::Spill
        | Opcode::Fill
        | Opcode::FillNop
        | Opcode::Regmove
        | Opcode::CopySpecial
        | Opcode::CopyToSsa
        | Opcode::CopyNop
        | Opcode::AdjustSpDown
        | Opcode::AdjustSpUpImm
        | Opcode::AdjustSpDownImm
        | Opcode::IfcmpSp
        | Opcode::Regspill
        | Opcode::Regfill => {
            panic!("Unused opcode should not be encountered.");
        }

        Opcode::Jump
        | Opcode::Fallthrough
        | Opcode::Brz
        | Opcode::Brnz
        | Opcode::BrIcmp
        | Opcode::Brif
        | Opcode::Brff
        | Opcode::IndirectJumpTableBr
        | Opcode::BrTable => {
            panic!("Branch opcode reached non-branch lowering logic!");
        }

        Opcode::Vconst => {
            let value = const_param_to_u128(ctx, insn).expect("Invalid immediate bytes");
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            lower_constant_f128(ctx, rd, value);
        }

        Opcode::RawBitcast => {
            let rm = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let ty = ctx.input_ty(insn, 0);
            ctx.emit(Inst::gen_move(rd, rm, ty));
        }

        Opcode::Extractlane => {
            if let InstructionData::BinaryImm8 { imm, .. } = ctx.data(insn) {
                let idx = *imm;
                let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
                let rn = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
                let size = VectorSize::from_ty(ctx.input_ty(insn, 0));
                let ty = ty.unwrap();

                if ty_has_int_representation(ty) {
                    ctx.emit(Inst::MovFromVec { rd, rn, idx, size });
                // Plain moves are faster on some processors.
                } else if idx == 0 {
                    ctx.emit(Inst::gen_move(rd, rn, ty));
                } else {
                    ctx.emit(Inst::FpuMoveFromVec { rd, rn, idx, size });
                }
            } else {
                unreachable!();
            }
        }

        Opcode::Insertlane => {
            let idx = if let InstructionData::TernaryImm8 { imm, .. } = ctx.data(insn) {
                *imm
            } else {
                unreachable!();
            };
            let input_ty = ctx.input_ty(insn, 1);
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let rm = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
            let rn = put_input_in_reg(ctx, inputs[1], NarrowValueMode::None);
            let ty = ty.unwrap();
            let size = VectorSize::from_ty(ty);

            ctx.emit(Inst::gen_move(rd, rm, ty));

            if ty_has_int_representation(input_ty) {
                ctx.emit(Inst::MovToVec { rd, rn, idx, size });
            } else {
                ctx.emit(Inst::VecMovElement {
                    rd,
                    rn,
                    dest_idx: idx,
                    src_idx: 0,
                    size,
                });
            }
        }

        Opcode::Splat => {
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let size = VectorSize::from_ty(ty.unwrap());

            if let Some((_, insn)) = maybe_input_insn_multi(
                ctx,
                inputs[0],
                &[
                    Opcode::Bconst,
                    Opcode::F32const,
                    Opcode::F64const,
                    Opcode::Iconst,
                ],
            ) {
                lower_splat_const(ctx, rd, ctx.get_constant(insn).unwrap(), size);
            } else if let Some(insn) =
                maybe_input_insn_via_conv(ctx, inputs[0], Opcode::Iconst, Opcode::Ireduce)
            {
                lower_splat_const(ctx, rd, ctx.get_constant(insn).unwrap(), size);
            } else if let Some(insn) =
                maybe_input_insn_via_conv(ctx, inputs[0], Opcode::Bconst, Opcode::Breduce)
            {
                lower_splat_const(ctx, rd, ctx.get_constant(insn).unwrap(), size);
            } else if let Some((_, insn)) = maybe_input_insn_multi(
                ctx,
                inputs[0],
                &[
                    Opcode::Uload8,
                    Opcode::Sload8,
                    Opcode::Uload16,
                    Opcode::Sload16,
                    Opcode::Uload32,
                    Opcode::Sload32,
                    Opcode::Load,
                ],
            ) {
                ctx.sink_inst(insn);
                let load_inputs = insn_inputs(ctx, insn);
                let load_outputs = insn_outputs(ctx, insn);
                lower_load(
                    ctx,
                    insn,
                    &load_inputs[..],
                    load_outputs[0],
                    |ctx, _rd, _elem_ty, mem| {
                        let tmp = ctx.alloc_tmp(I64).only_reg().unwrap();
                        let (addr, addr_inst) = Inst::gen_load_addr(tmp, mem);
                        if let Some(addr_inst) = addr_inst {
                            ctx.emit(addr_inst);
                        }
                        ctx.emit(Inst::VecLoadReplicate { rd, rn: addr, size });
                    },
                );
            } else {
                let input_ty = ctx.input_ty(insn, 0);
                let rn = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
                let inst = if ty_has_int_representation(input_ty) {
                    Inst::VecDup { rd, rn, size }
                } else {
                    Inst::VecDupFromFpu { rd, rn, size }
                };

                ctx.emit(inst);
            }
        }

        Opcode::ScalarToVector => {
            let rn = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let input_ty = ctx.input_ty(insn, 0);
            if (input_ty == I32 && ty.unwrap() == I32X4)
                || (input_ty == I64 && ty.unwrap() == I64X2)
            {
                ctx.emit(Inst::MovToFpu {
                    rd,
                    rn,
                    size: ScalarSize::from_ty(input_ty),
                });
            } else {
                return Err(CodegenError::Unsupported(format!(
                    "ScalarToVector: unsupported types {:?} -> {:?}",
                    input_ty, ty
                )));
            }
        }

        Opcode::VallTrue if ctx.input_ty(insn, 0) == I64X2 => {
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let rm = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
            let tmp = ctx.alloc_tmp(I64X2).only_reg().unwrap();

            // cmeq vtmp.2d, vm.2d, #0
            // addp dtmp, vtmp.2d
            // fcmp dtmp, dtmp
            // cset xd, eq
            //
            // Note that after the ADDP the value of the temporary register will
            // be either 0 when all input elements are true, i.e. non-zero, or a
            // NaN otherwise (either -1 or -2 when represented as an integer);
            // NaNs are the only floating-point numbers that compare unequal to
            // themselves.

            ctx.emit(Inst::VecMisc {
                op: VecMisc2::Cmeq0,
                rd: tmp,
                rn: rm,
                size: VectorSize::Size64x2,
            });
            ctx.emit(Inst::VecRRPair {
                op: VecPairOp::Addp,
                rd: tmp,
                rn: tmp.to_reg(),
            });
            ctx.emit(Inst::FpuCmp64 {
                rn: tmp.to_reg(),
                rm: tmp.to_reg(),
            });
            materialize_bool_result(ctx, insn, rd, Cond::Eq);
        }

        Opcode::VanyTrue | Opcode::VallTrue => {
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let rm = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
            let src_ty = ctx.input_ty(insn, 0);
            let tmp = ctx.alloc_tmp(src_ty).only_reg().unwrap();

            // This operation is implemented by using umaxp or uminv to
            // create a scalar value, which is then compared against zero.
            //
            // umaxp vn.16b, vm.16, vm.16 / uminv bn, vm.16b
            // mov xm, vn.d[0]
            // cmp xm, #0
            // cset xm, ne

            let size = VectorSize::from_ty(ctx.input_ty(insn, 0));

            if op == Opcode::VanyTrue {
                ctx.emit(Inst::VecRRR {
                    alu_op: VecALUOp::Umaxp,
                    rd: tmp,
                    rn: rm,
                    rm: rm,
                    size,
                });
            } else {
                ctx.emit(Inst::VecLanes {
                    op: VecLanesOp::Uminv,
                    rd: tmp,
                    rn: rm,
                    size,
                });
            };

            ctx.emit(Inst::MovFromVec {
                rd,
                rn: tmp.to_reg(),
                idx: 0,
                size: VectorSize::Size64x2,
            });

            ctx.emit(Inst::AluRRImm12 {
                alu_op: ALUOp::SubS64,
                rd: writable_zero_reg(),
                rn: rd.to_reg(),
                imm12: Imm12::zero(),
            });

            materialize_bool_result(ctx, insn, rd, Cond::Ne);
        }

        Opcode::VhighBits => {
            let dst_r = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let src_v = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
            let ty = ctx.input_ty(insn, 0);
            // All three sequences use one integer temporary and two vector temporaries.  The
            // shift is done early so as to give the register allocator the possibility of using
            // the same reg for `tmp_v1` and `src_v` in the case that this is the last use of
            // `src_v`.  See https://github.com/WebAssembly/simd/pull/201 for the background and
            // derivation of these sequences.  Alternative sequences are discussed in
            // https://github.com/bytecodealliance/wasmtime/issues/2296, although they are not
            // used here.
            let tmp_r0 = ctx.alloc_tmp(I64).only_reg().unwrap();
            let tmp_v0 = ctx.alloc_tmp(I8X16).only_reg().unwrap();
            let tmp_v1 = ctx.alloc_tmp(I8X16).only_reg().unwrap();
            match ty {
                I8X16 => {
                    // sshr  tmp_v1.16b, src_v.16b, #7
                    // mov   tmp_r0, #0x0201
                    // movk  tmp_r0, #0x0804, lsl 16
                    // movk  tmp_r0, #0x2010, lsl 32
                    // movk  tmp_r0, #0x8040, lsl 48
                    // dup   tmp_v0.2d, tmp_r0
                    // and   tmp_v1.16b, tmp_v1.16b, tmp_v0.16b
                    // ext   tmp_v0.16b, tmp_v1.16b, tmp_v1.16b, #8
                    // zip1  tmp_v0.16b, tmp_v1.16b, tmp_v0.16b
                    // addv  tmp_v0h, tmp_v0.8h
                    // mov   dst_r, tmp_v0.h[0]
                    ctx.emit(Inst::VecShiftImm {
                        op: VecShiftImmOp::Sshr,
                        rd: tmp_v1,
                        rn: src_v,
                        size: VectorSize::Size8x16,
                        imm: 7,
                    });
                    lower_splat_const(ctx, tmp_v0, 0x8040201008040201u64, VectorSize::Size64x2);
                    ctx.emit(Inst::VecRRR {
                        alu_op: VecALUOp::And,
                        rd: tmp_v1,
                        rn: tmp_v1.to_reg(),
                        rm: tmp_v0.to_reg(),
                        size: VectorSize::Size8x16,
                    });
                    ctx.emit(Inst::VecExtract {
                        rd: tmp_v0,
                        rn: tmp_v1.to_reg(),
                        rm: tmp_v1.to_reg(),
                        imm4: 8,
                    });
                    ctx.emit(Inst::VecRRR {
                        alu_op: VecALUOp::Zip1,
                        rd: tmp_v0,
                        rn: tmp_v1.to_reg(),
                        rm: tmp_v0.to_reg(),
                        size: VectorSize::Size8x16,
                    });
                    ctx.emit(Inst::VecLanes {
                        op: VecLanesOp::Addv,
                        rd: tmp_v0,
                        rn: tmp_v0.to_reg(),
                        size: VectorSize::Size16x8,
                    });
                    ctx.emit(Inst::MovFromVec {
                        rd: dst_r,
                        rn: tmp_v0.to_reg(),
                        idx: 0,
                        size: VectorSize::Size16x8,
                    });
                }
                I16X8 => {
                    // sshr  tmp_v1.8h, src_v.8h, #15
                    // mov   tmp_r0, #0x1
                    // movk  tmp_r0, #0x2, lsl 16
                    // movk  tmp_r0, #0x4, lsl 32
                    // movk  tmp_r0, #0x8, lsl 48
                    // dup   tmp_v0.2d, tmp_r0
                    // shl   tmp_r0, tmp_r0, #4
                    // mov   tmp_v0.d[1], tmp_r0
                    // and   tmp_v0.16b, tmp_v1.16b, tmp_v0.16b
                    // addv  tmp_v0h, tmp_v0.8h
                    // mov   dst_r, tmp_v0.h[0]
                    ctx.emit(Inst::VecShiftImm {
                        op: VecShiftImmOp::Sshr,
                        rd: tmp_v1,
                        rn: src_v,
                        size: VectorSize::Size16x8,
                        imm: 15,
                    });
                    lower_constant_u64(ctx, tmp_r0, 0x0008000400020001u64);
                    ctx.emit(Inst::VecDup {
                        rd: tmp_v0,
                        rn: tmp_r0.to_reg(),
                        size: VectorSize::Size64x2,
                    });
                    ctx.emit(Inst::AluRRImmShift {
                        alu_op: ALUOp::Lsl64,
                        rd: tmp_r0,
                        rn: tmp_r0.to_reg(),
                        immshift: ImmShift { imm: 4 },
                    });
                    ctx.emit(Inst::MovToVec {
                        rd: tmp_v0,
                        rn: tmp_r0.to_reg(),
                        idx: 1,
                        size: VectorSize::Size64x2,
                    });
                    ctx.emit(Inst::VecRRR {
                        alu_op: VecALUOp::And,
                        rd: tmp_v0,
                        rn: tmp_v1.to_reg(),
                        rm: tmp_v0.to_reg(),
                        size: VectorSize::Size8x16,
                    });
                    ctx.emit(Inst::VecLanes {
                        op: VecLanesOp::Addv,
                        rd: tmp_v0,
                        rn: tmp_v0.to_reg(),
                        size: VectorSize::Size16x8,
                    });
                    ctx.emit(Inst::MovFromVec {
                        rd: dst_r,
                        rn: tmp_v0.to_reg(),
                        idx: 0,
                        size: VectorSize::Size16x8,
                    });
                }
                I32X4 => {
                    // sshr  tmp_v1.4s, src_v.4s, #31
                    // mov   tmp_r0, #0x1
                    // movk  tmp_r0, #0x2, lsl 32
                    // dup   tmp_v0.2d, tmp_r0
                    // shl   tmp_r0, tmp_r0, #2
                    // mov   tmp_v0.d[1], tmp_r0
                    // and   tmp_v0.16b, tmp_v1.16b, tmp_v0.16b
                    // addv  tmp_v0s, tmp_v0.4s
                    // mov   dst_r, tmp_v0.s[0]
                    ctx.emit(Inst::VecShiftImm {
                        op: VecShiftImmOp::Sshr,
                        rd: tmp_v1,
                        rn: src_v,
                        size: VectorSize::Size32x4,
                        imm: 31,
                    });
                    lower_constant_u64(ctx, tmp_r0, 0x0000000200000001u64);
                    ctx.emit(Inst::VecDup {
                        rd: tmp_v0,
                        rn: tmp_r0.to_reg(),
                        size: VectorSize::Size64x2,
                    });
                    ctx.emit(Inst::AluRRImmShift {
                        alu_op: ALUOp::Lsl64,
                        rd: tmp_r0,
                        rn: tmp_r0.to_reg(),
                        immshift: ImmShift { imm: 2 },
                    });
                    ctx.emit(Inst::MovToVec {
                        rd: tmp_v0,
                        rn: tmp_r0.to_reg(),
                        idx: 1,
                        size: VectorSize::Size64x2,
                    });
                    ctx.emit(Inst::VecRRR {
                        alu_op: VecALUOp::And,
                        rd: tmp_v0,
                        rn: tmp_v1.to_reg(),
                        rm: tmp_v0.to_reg(),
                        size: VectorSize::Size8x16,
                    });
                    ctx.emit(Inst::VecLanes {
                        op: VecLanesOp::Addv,
                        rd: tmp_v0,
                        rn: tmp_v0.to_reg(),
                        size: VectorSize::Size32x4,
                    });
                    ctx.emit(Inst::MovFromVec {
                        rd: dst_r,
                        rn: tmp_v0.to_reg(),
                        idx: 0,
                        size: VectorSize::Size32x4,
                    });
                }
                I64X2 => {
                    // mov dst_r, src_v.d[0]
                    // mov tmp_r0, src_v.d[1]
                    // lsr dst_r, dst_r, #63
                    // lsr tmp_r0, tmp_r0, #63
                    // add dst_r, dst_r, tmp_r0, lsl #1
                    ctx.emit(Inst::MovFromVec {
                        rd: dst_r,
                        rn: src_v,
                        idx: 0,
                        size: VectorSize::Size64x2,
                    });
                    ctx.emit(Inst::MovFromVec {
                        rd: tmp_r0,
                        rn: src_v,
                        idx: 1,
                        size: VectorSize::Size64x2,
                    });
                    ctx.emit(Inst::AluRRImmShift {
                        alu_op: ALUOp::Lsr64,
                        rd: dst_r,
                        rn: dst_r.to_reg(),
                        immshift: ImmShift::maybe_from_u64(63).unwrap(),
                    });
                    ctx.emit(Inst::AluRRImmShift {
                        alu_op: ALUOp::Lsr64,
                        rd: tmp_r0,
                        rn: tmp_r0.to_reg(),
                        immshift: ImmShift::maybe_from_u64(63).unwrap(),
                    });
                    ctx.emit(Inst::AluRRRShift {
                        alu_op: ALUOp::Add32,
                        rd: dst_r,
                        rn: dst_r.to_reg(),
                        rm: tmp_r0.to_reg(),
                        shiftop: ShiftOpAndAmt::new(
                            ShiftOp::LSL,
                            ShiftOpShiftImm::maybe_from_shift(1).unwrap(),
                        ),
                    });
                }
                _ => panic!("arm64 isel: VhighBits unhandled, ty = {:?}", ty),
            }
        }

        Opcode::Shuffle => {
            let mask = const_param_to_u128(ctx, insn).expect("Invalid immediate mask bytes");
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let rn = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
            let rn2 = put_input_in_reg(ctx, inputs[1], NarrowValueMode::None);
            // 2 register table vector lookups require consecutive table registers;
            // we satisfy this constraint by hardcoding the usage of v29 and v30.
            let temp = writable_vreg(29);
            let temp2 = writable_vreg(30);
            let input_ty = ctx.input_ty(insn, 0);
            assert_eq!(input_ty, ctx.input_ty(insn, 1));
            // Make sure that both inputs are in virtual registers, since it is
            // not guaranteed that we can get them safely to the temporaries if
            // either is in a real register.
            let rn = ctx.ensure_in_vreg(rn, input_ty);
            let rn2 = ctx.ensure_in_vreg(rn2, input_ty);

            lower_constant_f128(ctx, rd, mask);
            ctx.emit(Inst::gen_move(temp, rn, input_ty));
            ctx.emit(Inst::gen_move(temp2, rn2, input_ty));
            ctx.emit(Inst::VecTbl2 {
                rd,
                rn: temp.to_reg(),
                rn2: temp2.to_reg(),
                rm: rd.to_reg(),
                is_extension: false,
            });
        }

        Opcode::Swizzle => {
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let rm = put_input_in_reg(ctx, inputs[1], NarrowValueMode::None);
            let rn = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);

            ctx.emit(Inst::VecTbl {
                rd,
                rn,
                rm,
                is_extension: false,
            });
        }

        Opcode::Isplit => {
            assert_eq!(
                ctx.input_ty(insn, 0),
                I128,
                "Isplit only implemented for i128's"
            );
            assert_eq!(ctx.output_ty(insn, 0), I64);
            assert_eq!(ctx.output_ty(insn, 1), I64);

            let src_regs = put_input_in_regs(ctx, inputs[0]);
            let dst_lo = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let dst_hi = get_output_reg(ctx, outputs[1]).only_reg().unwrap();

            ctx.emit(Inst::gen_move(dst_lo, src_regs.regs()[0], I64));
            ctx.emit(Inst::gen_move(dst_hi, src_regs.regs()[1], I64));
        }

        Opcode::Iconcat => {
            assert_eq!(
                ctx.output_ty(insn, 0),
                I128,
                "Iconcat only implemented for i128's"
            );
            assert_eq!(ctx.input_ty(insn, 0), I64);
            assert_eq!(ctx.input_ty(insn, 1), I64);

            let src_lo = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
            let src_hi = put_input_in_reg(ctx, inputs[1], NarrowValueMode::None);
            let dst = get_output_reg(ctx, outputs[0]);

            ctx.emit(Inst::gen_move(dst.regs()[0], src_lo, I64));
            ctx.emit(Inst::gen_move(dst.regs()[1], src_hi, I64));
        }

        Opcode::Imax | Opcode::Umax | Opcode::Umin | Opcode::Imin => {
            let alu_op = match op {
                Opcode::Umin => VecALUOp::Umin,
                Opcode::Imin => VecALUOp::Smin,
                Opcode::Umax => VecALUOp::Umax,
                Opcode::Imax => VecALUOp::Smax,
                _ => unreachable!(),
            };
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let rn = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
            let rm = put_input_in_reg(ctx, inputs[1], NarrowValueMode::None);
            let ty = ty.unwrap();
            ctx.emit(Inst::VecRRR {
                alu_op,
                rd,
                rn,
                rm,
                size: VectorSize::from_ty(ty),
            });
        }

        Opcode::WideningPairwiseDotProductS => {
            let r_y = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let r_a = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
            let r_b = put_input_in_reg(ctx, inputs[1], NarrowValueMode::None);
            let ty = ty.unwrap();
            if ty == I32X4 {
                let tmp = ctx.alloc_tmp(I8X16).only_reg().unwrap();
                // The args have type I16X8.
                // "y = i32x4.dot_i16x8_s(a, b)"
                // => smull  tmp, a, b
                //    smull2 y,   a, b
                //    addp   y,   tmp, y
                ctx.emit(Inst::VecRRR {
                    alu_op: VecALUOp::Smull,
                    rd: tmp,
                    rn: r_a,
                    rm: r_b,
                    size: VectorSize::Size16x8,
                });
                ctx.emit(Inst::VecRRR {
                    alu_op: VecALUOp::Smull2,
                    rd: r_y,
                    rn: r_a,
                    rm: r_b,
                    size: VectorSize::Size16x8,
                });
                ctx.emit(Inst::VecRRR {
                    alu_op: VecALUOp::Addp,
                    rd: r_y,
                    rn: tmp.to_reg(),
                    rm: r_y.to_reg(),
                    size: VectorSize::Size32x4,
                });
            } else {
                return Err(CodegenError::Unsupported(format!(
                    "Opcode::WideningPairwiseDotProductS: unsupported laneage: {:?}",
                    ty
                )));
            }
        }

        Opcode::Fadd | Opcode::Fsub | Opcode::Fmul | Opcode::Fdiv | Opcode::Fmin | Opcode::Fmax => {
            let ty = ty.unwrap();
            let bits = ty_bits(ty);
            let rn = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
            let rm = put_input_in_reg(ctx, inputs[1], NarrowValueMode::None);
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            if !ty.is_vector() {
                let fpu_op = match (op, bits) {
                    (Opcode::Fadd, 32) => FPUOp2::Add32,
                    (Opcode::Fadd, 64) => FPUOp2::Add64,
                    (Opcode::Fsub, 32) => FPUOp2::Sub32,
                    (Opcode::Fsub, 64) => FPUOp2::Sub64,
                    (Opcode::Fmul, 32) => FPUOp2::Mul32,
                    (Opcode::Fmul, 64) => FPUOp2::Mul64,
                    (Opcode::Fdiv, 32) => FPUOp2::Div32,
                    (Opcode::Fdiv, 64) => FPUOp2::Div64,
                    (Opcode::Fmin, 32) => FPUOp2::Min32,
                    (Opcode::Fmin, 64) => FPUOp2::Min64,
                    (Opcode::Fmax, 32) => FPUOp2::Max32,
                    (Opcode::Fmax, 64) => FPUOp2::Max64,
                    _ => panic!("Unknown op/bits combination"),
                };
                ctx.emit(Inst::FpuRRR { fpu_op, rd, rn, rm });
            } else {
                let alu_op = match op {
                    Opcode::Fadd => VecALUOp::Fadd,
                    Opcode::Fsub => VecALUOp::Fsub,
                    Opcode::Fdiv => VecALUOp::Fdiv,
                    Opcode::Fmax => VecALUOp::Fmax,
                    Opcode::Fmin => VecALUOp::Fmin,
                    Opcode::Fmul => VecALUOp::Fmul,
                    _ => unreachable!(),
                };

                ctx.emit(Inst::VecRRR {
                    rd,
                    rn,
                    rm,
                    alu_op,
                    size: VectorSize::from_ty(ty),
                });
            }
        }

        Opcode::FminPseudo | Opcode::FmaxPseudo => {
            let ty = ctx.input_ty(insn, 0);
            if ty == F32X4 || ty == F64X2 {
                // pmin(a,b) => bitsel(b, a, cmpgt(a, b))
                // pmax(a,b) => bitsel(b, a, cmpgt(b, a))
                let r_dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
                let r_a = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
                let r_b = put_input_in_reg(ctx, inputs[1], NarrowValueMode::None);
                // Since we're going to write the output register `r_dst` anyway, we might as
                // well first use it to hold the comparison result.  This has the slightly unusual
                // effect that we modify the output register in the first instruction (`fcmgt`)
                // but read both the inputs again in the second instruction (`bsl`), which means
                // that the output register can't be either of the input registers.  Regalloc
                // should handle this correctly, nevertheless.
                ctx.emit(Inst::VecRRR {
                    alu_op: VecALUOp::Fcmgt,
                    rd: r_dst,
                    rn: if op == Opcode::FminPseudo { r_a } else { r_b },
                    rm: if op == Opcode::FminPseudo { r_b } else { r_a },
                    size: if ty == F32X4 {
                        VectorSize::Size32x4
                    } else {
                        VectorSize::Size64x2
                    },
                });
                ctx.emit(Inst::VecRRR {
                    alu_op: VecALUOp::Bsl,
                    rd: r_dst,
                    rn: r_b,
                    rm: r_a,
                    size: VectorSize::Size8x16,
                });
            } else {
                panic!("Opcode::FminPseudo | Opcode::FmaxPseudo: unhandled type");
            }
        }

        Opcode::Sqrt | Opcode::Fneg | Opcode::Fabs | Opcode::Fpromote | Opcode::Fdemote => {
            let ty = ty.unwrap();
            let bits = ty_bits(ty);
            let rn = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            if !ty.is_vector() {
                let fpu_op = match (op, bits) {
                    (Opcode::Sqrt, 32) => FPUOp1::Sqrt32,
                    (Opcode::Sqrt, 64) => FPUOp1::Sqrt64,
                    (Opcode::Fneg, 32) => FPUOp1::Neg32,
                    (Opcode::Fneg, 64) => FPUOp1::Neg64,
                    (Opcode::Fabs, 32) => FPUOp1::Abs32,
                    (Opcode::Fabs, 64) => FPUOp1::Abs64,
                    (Opcode::Fpromote, 32) => panic!("Cannot promote to 32 bits"),
                    (Opcode::Fpromote, 64) => FPUOp1::Cvt32To64,
                    (Opcode::Fdemote, 32) => FPUOp1::Cvt64To32,
                    (Opcode::Fdemote, 64) => panic!("Cannot demote to 64 bits"),
                    _ => panic!("Unknown op/bits combination"),
                };
                ctx.emit(Inst::FpuRR { fpu_op, rd, rn });
            } else {
                let op = match op {
                    Opcode::Fabs => VecMisc2::Fabs,
                    Opcode::Fneg => VecMisc2::Fneg,
                    Opcode::Sqrt => VecMisc2::Fsqrt,
                    _ => unimplemented!(),
                };

                ctx.emit(Inst::VecMisc {
                    op,
                    rd,
                    rn,
                    size: VectorSize::from_ty(ty),
                });
            }
        }

        Opcode::Ceil | Opcode::Floor | Opcode::Trunc | Opcode::Nearest => {
            let ty = ctx.output_ty(insn, 0);
            if !ty.is_vector() {
                let bits = ty_bits(ty);
                let op = match (op, bits) {
                    (Opcode::Ceil, 32) => FpuRoundMode::Plus32,
                    (Opcode::Ceil, 64) => FpuRoundMode::Plus64,
                    (Opcode::Floor, 32) => FpuRoundMode::Minus32,
                    (Opcode::Floor, 64) => FpuRoundMode::Minus64,
                    (Opcode::Trunc, 32) => FpuRoundMode::Zero32,
                    (Opcode::Trunc, 64) => FpuRoundMode::Zero64,
                    (Opcode::Nearest, 32) => FpuRoundMode::Nearest32,
                    (Opcode::Nearest, 64) => FpuRoundMode::Nearest64,
                    _ => panic!("Unknown op/bits combination (scalar)"),
                };
                let rn = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
                let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
                ctx.emit(Inst::FpuRound { op, rd, rn });
            } else {
                let (op, size) = match (op, ty) {
                    (Opcode::Ceil, F32X4) => (VecMisc2::Frintp, VectorSize::Size32x4),
                    (Opcode::Ceil, F64X2) => (VecMisc2::Frintp, VectorSize::Size64x2),
                    (Opcode::Floor, F32X4) => (VecMisc2::Frintm, VectorSize::Size32x4),
                    (Opcode::Floor, F64X2) => (VecMisc2::Frintm, VectorSize::Size64x2),
                    (Opcode::Trunc, F32X4) => (VecMisc2::Frintz, VectorSize::Size32x4),
                    (Opcode::Trunc, F64X2) => (VecMisc2::Frintz, VectorSize::Size64x2),
                    (Opcode::Nearest, F32X4) => (VecMisc2::Frintn, VectorSize::Size32x4),
                    (Opcode::Nearest, F64X2) => (VecMisc2::Frintn, VectorSize::Size64x2),
                    _ => panic!("Unknown op/ty combination (vector){:?}", ty),
                };
                let rn = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
                let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
                ctx.emit(Inst::VecMisc { op, rd, rn, size });
            }
        }

        Opcode::Fma => {
            let bits = ty_bits(ctx.output_ty(insn, 0));
            let fpu_op = match bits {
                32 => FPUOp3::MAdd32,
                64 => FPUOp3::MAdd64,
                _ => panic!("Unknown op size"),
            };
            let rn = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
            let rm = put_input_in_reg(ctx, inputs[1], NarrowValueMode::None);
            let ra = put_input_in_reg(ctx, inputs[2], NarrowValueMode::None);
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            ctx.emit(Inst::FpuRRRR {
                fpu_op,
                rn,
                rm,
                ra,
                rd,
            });
        }

        Opcode::Fcopysign => {
            // Copy the sign bit from inputs[1] to inputs[0]. We use the following sequence:
            //
            // This is a scalar Fcopysign.
            // This uses scalar NEON operations for 64-bit and vector operations (2S) for 32-bit.
            // In the latter case it still sets all bits except the lowest 32 to 0.
            //
            //  mov vd, vn
            //  ushr vtmp, vm, #63 / #31
            //  sli vd, vtmp, #63 / #31

            let ty = ctx.output_ty(insn, 0);
            let bits = ty_bits(ty) as u8;
            assert!(bits == 32 || bits == 64);
            let rn = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
            let rm = put_input_in_reg(ctx, inputs[1], NarrowValueMode::None);
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let tmp = ctx.alloc_tmp(F64).only_reg().unwrap();

            // Copy LHS to rd.
            ctx.emit(Inst::gen_move(rd, rn, ty));

            // Copy the sign bit to the lowest bit in tmp.
            let imm = FPURightShiftImm::maybe_from_u8(bits - 1, bits).unwrap();
            ctx.emit(Inst::FpuRRI {
                fpu_op: choose_32_64(ty, FPUOpRI::UShr32(imm), FPUOpRI::UShr64(imm)),
                rd: tmp,
                rn: rm,
            });

            // Insert the bit from tmp into the sign bit of rd.
            let imm = FPULeftShiftImm::maybe_from_u8(bits - 1, bits).unwrap();
            ctx.emit(Inst::FpuRRI {
                fpu_op: choose_32_64(ty, FPUOpRI::Sli32(imm), FPUOpRI::Sli64(imm)),
                rd,
                rn: tmp.to_reg(),
            });
        }

        Opcode::FcvtToUint | Opcode::FcvtToSint => {
            let in_bits = ty_bits(ctx.input_ty(insn, 0));
            let out_bits = ty_bits(ctx.output_ty(insn, 0));
            let signed = op == Opcode::FcvtToSint;
            let op = match (signed, in_bits, out_bits) {
                (false, 32, 8) | (false, 32, 16) | (false, 32, 32) => FpuToIntOp::F32ToU32,
                (true, 32, 8) | (true, 32, 16) | (true, 32, 32) => FpuToIntOp::F32ToI32,
                (false, 32, 64) => FpuToIntOp::F32ToU64,
                (true, 32, 64) => FpuToIntOp::F32ToI64,
                (false, 64, 8) | (false, 64, 16) | (false, 64, 32) => FpuToIntOp::F64ToU32,
                (true, 64, 8) | (true, 64, 16) | (true, 64, 32) => FpuToIntOp::F64ToI32,
                (false, 64, 64) => FpuToIntOp::F64ToU64,
                (true, 64, 64) => FpuToIntOp::F64ToI64,
                _ => panic!("Unknown input/output-bits combination"),
            };

            let rn = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();

            // First, check the output: it's important to carry the NaN conversion before the
            // in-bounds conversion, per wasm semantics.

            // Check that the input is not a NaN.
            if in_bits == 32 {
                ctx.emit(Inst::FpuCmp32 { rn, rm: rn });
            } else {
                ctx.emit(Inst::FpuCmp64 { rn, rm: rn });
            }
            let trap_code = TrapCode::BadConversionToInteger;
            ctx.emit(Inst::TrapIf {
                trap_code,
                kind: CondBrKind::Cond(lower_fp_condcode(FloatCC::Unordered)),
            });

            let tmp = ctx.alloc_tmp(I8X16).only_reg().unwrap();

            // Check that the input is in range, with "truncate towards zero" semantics. This means
            // we allow values that are slightly out of range:
            // - for signed conversions, we allow values strictly greater than INT_MIN-1 (when this
            // can be represented), and strictly less than INT_MAX+1 (when this can be
            // represented).
            // - for unsigned conversions, we allow values strictly greater than -1, and strictly
            // less than UINT_MAX+1 (when this can be represented).

            if in_bits == 32 {
                // From float32.
                let (low_bound, low_cond, high_bound) = match (signed, out_bits) {
                    (true, 8) => (
                        i8::min_value() as f32 - 1.,
                        FloatCC::GreaterThan,
                        i8::max_value() as f32 + 1.,
                    ),
                    (true, 16) => (
                        i16::min_value() as f32 - 1.,
                        FloatCC::GreaterThan,
                        i16::max_value() as f32 + 1.,
                    ),
                    (true, 32) => (
                        i32::min_value() as f32, // I32_MIN - 1 isn't precisely representable as a f32.
                        FloatCC::GreaterThanOrEqual,
                        i32::max_value() as f32 + 1.,
                    ),
                    (true, 64) => (
                        i64::min_value() as f32, // I64_MIN - 1 isn't precisely representable as a f32.
                        FloatCC::GreaterThanOrEqual,
                        i64::max_value() as f32 + 1.,
                    ),
                    (false, 8) => (-1., FloatCC::GreaterThan, u8::max_value() as f32 + 1.),
                    (false, 16) => (-1., FloatCC::GreaterThan, u16::max_value() as f32 + 1.),
                    (false, 32) => (-1., FloatCC::GreaterThan, u32::max_value() as f32 + 1.),
                    (false, 64) => (-1., FloatCC::GreaterThan, u64::max_value() as f32 + 1.),
                    _ => panic!("Unknown input/output-bits combination"),
                };

                // >= low_bound
                lower_constant_f32(ctx, tmp, low_bound);
                ctx.emit(Inst::FpuCmp32 {
                    rn,
                    rm: tmp.to_reg(),
                });
                let trap_code = TrapCode::IntegerOverflow;
                ctx.emit(Inst::TrapIf {
                    trap_code,
                    kind: CondBrKind::Cond(lower_fp_condcode(low_cond).invert()),
                });

                // <= high_bound
                lower_constant_f32(ctx, tmp, high_bound);
                ctx.emit(Inst::FpuCmp32 {
                    rn,
                    rm: tmp.to_reg(),
                });
                let trap_code = TrapCode::IntegerOverflow;
                ctx.emit(Inst::TrapIf {
                    trap_code,
                    kind: CondBrKind::Cond(lower_fp_condcode(FloatCC::LessThan).invert()),
                });
            } else {
                // From float64.
                let (low_bound, low_cond, high_bound) = match (signed, out_bits) {
                    (true, 8) => (
                        i8::min_value() as f64 - 1.,
                        FloatCC::GreaterThan,
                        i8::max_value() as f64 + 1.,
                    ),
                    (true, 16) => (
                        i16::min_value() as f64 - 1.,
                        FloatCC::GreaterThan,
                        i16::max_value() as f64 + 1.,
                    ),
                    (true, 32) => (
                        i32::min_value() as f64 - 1.,
                        FloatCC::GreaterThan,
                        i32::max_value() as f64 + 1.,
                    ),
                    (true, 64) => (
                        i64::min_value() as f64, // I64_MIN - 1 is not precisely representable as an i64.
                        FloatCC::GreaterThanOrEqual,
                        i64::max_value() as f64 + 1.,
                    ),
                    (false, 8) => (-1., FloatCC::GreaterThan, u8::max_value() as f64 + 1.),
                    (false, 16) => (-1., FloatCC::GreaterThan, u16::max_value() as f64 + 1.),
                    (false, 32) => (-1., FloatCC::GreaterThan, u32::max_value() as f64 + 1.),
                    (false, 64) => (-1., FloatCC::GreaterThan, u64::max_value() as f64 + 1.),
                    _ => panic!("Unknown input/output-bits combination"),
                };

                // >= low_bound
                lower_constant_f64(ctx, tmp, low_bound);
                ctx.emit(Inst::FpuCmp64 {
                    rn,
                    rm: tmp.to_reg(),
                });
                let trap_code = TrapCode::IntegerOverflow;
                ctx.emit(Inst::TrapIf {
                    trap_code,
                    kind: CondBrKind::Cond(lower_fp_condcode(low_cond).invert()),
                });

                // <= high_bound
                lower_constant_f64(ctx, tmp, high_bound);
                ctx.emit(Inst::FpuCmp64 {
                    rn,
                    rm: tmp.to_reg(),
                });
                let trap_code = TrapCode::IntegerOverflow;
                ctx.emit(Inst::TrapIf {
                    trap_code,
                    kind: CondBrKind::Cond(lower_fp_condcode(FloatCC::LessThan).invert()),
                });
            };

            // Do the conversion.
            ctx.emit(Inst::FpuToInt { op, rd, rn });
        }

        Opcode::FcvtFromUint | Opcode::FcvtFromSint => {
            let ty = ty.unwrap();
            let signed = op == Opcode::FcvtFromSint;
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();

            if ty.is_vector() {
                let op = if signed {
                    VecMisc2::Scvtf
                } else {
                    VecMisc2::Ucvtf
                };
                let rn = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);

                ctx.emit(Inst::VecMisc {
                    op,
                    rd,
                    rn,
                    size: VectorSize::from_ty(ty),
                });
            } else {
                let in_bits = ty_bits(ctx.input_ty(insn, 0));
                let out_bits = ty_bits(ty);
                let op = match (signed, in_bits, out_bits) {
                    (false, 8, 32) | (false, 16, 32) | (false, 32, 32) => IntToFpuOp::U32ToF32,
                    (true, 8, 32) | (true, 16, 32) | (true, 32, 32) => IntToFpuOp::I32ToF32,
                    (false, 8, 64) | (false, 16, 64) | (false, 32, 64) => IntToFpuOp::U32ToF64,
                    (true, 8, 64) | (true, 16, 64) | (true, 32, 64) => IntToFpuOp::I32ToF64,
                    (false, 64, 32) => IntToFpuOp::U64ToF32,
                    (true, 64, 32) => IntToFpuOp::I64ToF32,
                    (false, 64, 64) => IntToFpuOp::U64ToF64,
                    (true, 64, 64) => IntToFpuOp::I64ToF64,
                    _ => panic!("Unknown input/output-bits combination"),
                };
                let narrow_mode = match (signed, in_bits) {
                    (false, 8) | (false, 16) | (false, 32) => NarrowValueMode::ZeroExtend32,
                    (true, 8) | (true, 16) | (true, 32) => NarrowValueMode::SignExtend32,
                    (false, 64) => NarrowValueMode::ZeroExtend64,
                    (true, 64) => NarrowValueMode::SignExtend64,
                    _ => panic!("Unknown input size"),
                };
                let rn = put_input_in_reg(ctx, inputs[0], narrow_mode);
                ctx.emit(Inst::IntToFpu { op, rd, rn });
            }
        }

        Opcode::FcvtToUintSat | Opcode::FcvtToSintSat => {
            let ty = ty.unwrap();
            let out_signed = op == Opcode::FcvtToSintSat;
            let rn = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();

            if ty.is_vector() {
                let op = if out_signed {
                    VecMisc2::Fcvtzs
                } else {
                    VecMisc2::Fcvtzu
                };

                ctx.emit(Inst::VecMisc {
                    op,
                    rd,
                    rn,
                    size: VectorSize::from_ty(ty),
                });
            } else {
                let in_ty = ctx.input_ty(insn, 0);
                let in_bits = ty_bits(in_ty);
                let out_bits = ty_bits(ty);
                // FIMM Vtmp1, u32::MAX or u64::MAX or i32::MAX or i64::MAX
                // FMIN Vtmp2, Vin, Vtmp1
                // FIMM Vtmp1, 0 or 0 or i32::MIN or i64::MIN
                // FMAX Vtmp2, Vtmp2, Vtmp1
                // (if signed) FIMM Vtmp1, 0
                // FCMP Vin, Vin
                // FCSEL Vtmp2, Vtmp1, Vtmp2, NE  // on NaN, select 0
                // convert Rout, Vtmp2

                assert!(in_bits == 32 || in_bits == 64);
                assert!(out_bits == 32 || out_bits == 64);

                let min: f64 = match (out_bits, out_signed) {
                    (32, true) => std::i32::MIN as f64,
                    (32, false) => 0.0,
                    (64, true) => std::i64::MIN as f64,
                    (64, false) => 0.0,
                    _ => unreachable!(),
                };

                let max = match (out_bits, out_signed) {
                    (32, true) => std::i32::MAX as f64,
                    (32, false) => std::u32::MAX as f64,
                    (64, true) => std::i64::MAX as f64,
                    (64, false) => std::u64::MAX as f64,
                    _ => unreachable!(),
                };

                let rtmp1 = ctx.alloc_tmp(in_ty).only_reg().unwrap();
                let rtmp2 = ctx.alloc_tmp(in_ty).only_reg().unwrap();

                if in_bits == 32 {
                    lower_constant_f32(ctx, rtmp1, max as f32);
                } else {
                    lower_constant_f64(ctx, rtmp1, max);
                }
                ctx.emit(Inst::FpuRRR {
                    fpu_op: choose_32_64(in_ty, FPUOp2::Min32, FPUOp2::Min64),
                    rd: rtmp2,
                    rn: rn,
                    rm: rtmp1.to_reg(),
                });
                if in_bits == 32 {
                    lower_constant_f32(ctx, rtmp1, min as f32);
                } else {
                    lower_constant_f64(ctx, rtmp1, min);
                }
                ctx.emit(Inst::FpuRRR {
                    fpu_op: choose_32_64(in_ty, FPUOp2::Max32, FPUOp2::Max64),
                    rd: rtmp2,
                    rn: rtmp2.to_reg(),
                    rm: rtmp1.to_reg(),
                });
                if out_signed {
                    if in_bits == 32 {
                        lower_constant_f32(ctx, rtmp1, 0.0);
                    } else {
                        lower_constant_f64(ctx, rtmp1, 0.0);
                    }
                }
                if in_bits == 32 {
                    ctx.emit(Inst::FpuCmp32 { rn: rn, rm: rn });
                    ctx.emit(Inst::FpuCSel32 {
                        rd: rtmp2,
                        rn: rtmp1.to_reg(),
                        rm: rtmp2.to_reg(),
                        cond: Cond::Ne,
                    });
                } else {
                    ctx.emit(Inst::FpuCmp64 { rn: rn, rm: rn });
                    ctx.emit(Inst::FpuCSel64 {
                        rd: rtmp2,
                        rn: rtmp1.to_reg(),
                        rm: rtmp2.to_reg(),
                        cond: Cond::Ne,
                    });
                }

                let cvt = match (in_bits, out_bits, out_signed) {
                    (32, 32, false) => FpuToIntOp::F32ToU32,
                    (32, 32, true) => FpuToIntOp::F32ToI32,
                    (32, 64, false) => FpuToIntOp::F32ToU64,
                    (32, 64, true) => FpuToIntOp::F32ToI64,
                    (64, 32, false) => FpuToIntOp::F64ToU32,
                    (64, 32, true) => FpuToIntOp::F64ToI32,
                    (64, 64, false) => FpuToIntOp::F64ToU64,
                    (64, 64, true) => FpuToIntOp::F64ToI64,
                    _ => unreachable!(),
                };
                ctx.emit(Inst::FpuToInt {
                    op: cvt,
                    rd,
                    rn: rtmp2.to_reg(),
                });
            }
        }

        Opcode::IaddIfcout => {
            // This is a two-output instruction that is needed for the
            // legalizer's explicit heap-check sequence, among possible other
            // uses. Its second output is a flags output only ever meant to
            // check for overflow using the
            // `backend.unsigned_add_overflow_condition()` condition.
            //
            // Note that the CLIF validation will ensure that no flag-setting
            // operation comes between this IaddIfcout and its use (e.g., a
            // Trapif). Thus, we can rely on implicit communication through the
            // processor flags rather than explicitly generating flags into a
            // register. We simply use the variant of the add instruction that
            // sets flags (`adds`) here.

            // Note that the second output (the flags) need not be generated,
            // because flags are never materialized into a register; the only
            // instructions that can use a value of type `iflags` or `fflags`
            // will look directly for the flags-producing instruction (which can
            // always be found, by construction) and merge it.

            // Now handle the iadd as above, except use an AddS opcode that sets
            // flags.
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let rn = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
            let rm = put_input_in_rse_imm12(ctx, inputs[1], NarrowValueMode::None);
            let ty = ty.unwrap();
            let alu_op = choose_32_64(ty, ALUOp::AddS32, ALUOp::AddS64);
            ctx.emit(alu_inst_imm12(alu_op, rd, rn, rm));
        }

        Opcode::IaddImm
        | Opcode::ImulImm
        | Opcode::UdivImm
        | Opcode::SdivImm
        | Opcode::UremImm
        | Opcode::SremImm
        | Opcode::IrsubImm
        | Opcode::IaddCin
        | Opcode::IaddIfcin
        | Opcode::IaddCout
        | Opcode::IaddCarry
        | Opcode::IaddIfcarry
        | Opcode::IsubBin
        | Opcode::IsubIfbin
        | Opcode::IsubBout
        | Opcode::IsubIfbout
        | Opcode::IsubBorrow
        | Opcode::IsubIfborrow
        | Opcode::BandImm
        | Opcode::BorImm
        | Opcode::BxorImm
        | Opcode::RotlImm
        | Opcode::RotrImm
        | Opcode::IshlImm
        | Opcode::UshrImm
        | Opcode::SshrImm
        | Opcode::IcmpImm
        | Opcode::IfcmpImm => {
            panic!("ALU+imm and ALU+carry ops should not appear here!");
        }

        #[cfg(feature = "x86")]
        Opcode::X86Udivmodx
        | Opcode::X86Sdivmodx
        | Opcode::X86Umulx
        | Opcode::X86Smulx
        | Opcode::X86Cvtt2si
        | Opcode::X86Fmin
        | Opcode::X86Fmax
        | Opcode::X86Push
        | Opcode::X86Pop
        | Opcode::X86Bsr
        | Opcode::X86Bsf
        | Opcode::X86Pblendw
        | Opcode::X86Pshufd
        | Opcode::X86Pshufb
        | Opcode::X86Pextr
        | Opcode::X86Pinsr
        | Opcode::X86Insertps
        | Opcode::X86Movsd
        | Opcode::X86Movlhps
        | Opcode::X86Palignr
        | Opcode::X86Psll
        | Opcode::X86Psrl
        | Opcode::X86Psra
        | Opcode::X86Ptest
        | Opcode::X86Pmaxs
        | Opcode::X86Pmaxu
        | Opcode::X86Pmins
        | Opcode::X86Pminu
        | Opcode::X86Pmullq
        | Opcode::X86Pmuludq
        | Opcode::X86Punpckh
        | Opcode::X86Punpckl
        | Opcode::X86Vcvtudq2ps
        | Opcode::X86ElfTlsGetAddr
        | Opcode::X86MachoTlsGetAddr => {
            panic!("x86-specific opcode in supposedly arch-neutral IR!");
        }

        Opcode::DummySargT => unreachable!(),

        Opcode::Iabs => {
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let rn = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
            let ty = ty.unwrap();
            ctx.emit(Inst::VecMisc {
                op: VecMisc2::Abs,
                rd,
                rn,
                size: VectorSize::from_ty(ty),
            });
        }
        Opcode::AvgRound => {
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let rn = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
            let rm = put_input_in_reg(ctx, inputs[1], NarrowValueMode::None);
            let ty = ty.unwrap();
            ctx.emit(Inst::VecRRR {
                alu_op: VecALUOp::Urhadd,
                rd,
                rn,
                rm,
                size: VectorSize::from_ty(ty),
            });
        }

        Opcode::Snarrow | Opcode::Unarrow | Opcode::Uunarrow => {
            let nonzero_high_half = maybe_input_insn(ctx, inputs[1], Opcode::Vconst)
                .map_or(true, |insn| {
                    const_param_to_u128(ctx, insn).expect("Invalid immediate bytes") != 0
                });
            let op = match (op, ty.unwrap().lane_type()) {
                (Opcode::Snarrow, I8) => VecRRNarrowOp::Sqxtn16,
                (Opcode::Snarrow, I16) => VecRRNarrowOp::Sqxtn32,
                (Opcode::Snarrow, I32) => VecRRNarrowOp::Sqxtn64,
                (Opcode::Unarrow, I8) => VecRRNarrowOp::Sqxtun16,
                (Opcode::Unarrow, I16) => VecRRNarrowOp::Sqxtun32,
                (Opcode::Unarrow, I32) => VecRRNarrowOp::Sqxtun64,
                (Opcode::Uunarrow, I8) => VecRRNarrowOp::Uqxtn16,
                (Opcode::Uunarrow, I16) => VecRRNarrowOp::Uqxtn32,
                (Opcode::Uunarrow, I32) => VecRRNarrowOp::Uqxtn64,
                (_, lane_type) => {
                    return Err(CodegenError::Unsupported(format!(
                        "Unsupported SIMD vector lane type: {:?}",
                        lane_type
                    )))
                }
            };
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let rn = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);

            ctx.emit(Inst::VecRRNarrow {
                op,
                rd,
                rn,
                high_half: false,
            });

            if nonzero_high_half {
                let rn = put_input_in_reg(ctx, inputs[1], NarrowValueMode::None);

                ctx.emit(Inst::VecRRNarrow {
                    op,
                    rd,
                    rn,
                    high_half: true,
                });
            }
        }

        Opcode::SwidenLow | Opcode::SwidenHigh | Opcode::UwidenLow | Opcode::UwidenHigh => {
            let lane_type = ty.unwrap().lane_type();
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let rn = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
            let (t, high_half) = match (lane_type, op) {
                (I16, Opcode::SwidenLow) => (VecExtendOp::Sxtl8, false),
                (I16, Opcode::SwidenHigh) => (VecExtendOp::Sxtl8, true),
                (I16, Opcode::UwidenLow) => (VecExtendOp::Uxtl8, false),
                (I16, Opcode::UwidenHigh) => (VecExtendOp::Uxtl8, true),
                (I32, Opcode::SwidenLow) => (VecExtendOp::Sxtl16, false),
                (I32, Opcode::SwidenHigh) => (VecExtendOp::Sxtl16, true),
                (I32, Opcode::UwidenLow) => (VecExtendOp::Uxtl16, false),
                (I32, Opcode::UwidenHigh) => (VecExtendOp::Uxtl16, true),
                (I64, Opcode::SwidenLow) => (VecExtendOp::Sxtl32, false),
                (I64, Opcode::SwidenHigh) => (VecExtendOp::Sxtl32, true),
                (I64, Opcode::UwidenLow) => (VecExtendOp::Uxtl32, false),
                (I64, Opcode::UwidenHigh) => (VecExtendOp::Uxtl32, true),
                _ => {
                    return Err(CodegenError::Unsupported(format!(
                        "Unsupported SIMD vector lane type: {:?}",
                        lane_type
                    )));
                }
            };

            ctx.emit(Inst::VecExtend {
                t,
                rd,
                rn,
                high_half,
            });
        }

        Opcode::TlsValue => match flags.tls_model() {
            TlsModel::ElfGd => {
                let dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
                let (name, _, _) = ctx.symbol_value(insn).unwrap();
                let symbol = name.clone();
                ctx.emit(Inst::ElfTlsGetAddr { symbol });

                let x0 = xreg(0);
                ctx.emit(Inst::gen_move(dst, x0, I64));
            }
            _ => {
                todo!(
                    "Unimplemented TLS model in AArch64 backend: {:?}",
                    flags.tls_model()
                );
            }
        },

        Opcode::SqmulRoundSat => {
            let ty = ty.unwrap();

            if !ty.is_vector() || (ty.lane_type() != I16 && ty.lane_type() != I32) {
                return Err(CodegenError::Unsupported(format!(
                    "Unsupported type: {:?}",
                    ty
                )));
            }

            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let rn = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
            let rm = put_input_in_reg(ctx, inputs[1], NarrowValueMode::None);

            ctx.emit(Inst::VecRRR {
                alu_op: VecALUOp::Sqrdmulh,
                rd,
                rn,
                rm,
                size: VectorSize::from_ty(ty),
            });
        }

        Opcode::FcvtLowFromSint => {
            let ty = ty.unwrap();

            if ty != F64X2 {
                return Err(CodegenError::Unsupported(format!(
                    "FcvtLowFromSint: Unsupported type: {:?}",
                    ty
                )));
            }

            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let rn = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);

            ctx.emit(Inst::VecExtend {
                t: VecExtendOp::Sxtl32,
                rd,
                rn,
                high_half: false,
            });
            ctx.emit(Inst::VecMisc {
                op: VecMisc2::Scvtf,
                rd,
                rn: rd.to_reg(),
                size: VectorSize::Size64x2,
            });
        }

        Opcode::FvpromoteLow => {
            debug_assert_eq!(ty.unwrap(), F64X2);

            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let rn = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);

            ctx.emit(Inst::VecRRLong {
                op: VecRRLongOp::Fcvtl32,
                rd,
                rn,
                high_half: false,
            });
        }

        Opcode::Fvdemote => {
            debug_assert_eq!(ty.unwrap(), F32X4);

            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let rn = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);

            ctx.emit(Inst::VecRRNarrow {
                op: VecRRNarrowOp::Fcvtn64,
                rd,
                rn,
                high_half: false,
            });
        }

        Opcode::ConstAddr | Opcode::Vconcat | Opcode::Vsplit => unimplemented!("lowering {}", op),
    }

    Ok(())
}

pub(crate) fn lower_branch<C: LowerCtx<I = Inst>>(
    ctx: &mut C,
    branches: &[IRInst],
    targets: &[MachLabel],
) -> CodegenResult<()> {
    // A block should end with at most two branches. The first may be a
    // conditional branch; a conditional branch can be followed only by an
    // unconditional branch or fallthrough. Otherwise, if only one branch,
    // it may be an unconditional branch, a fallthrough, a return, or a
    // trap. These conditions are verified by `is_ebb_basic()` during the
    // verifier pass.
    assert!(branches.len() <= 2);

    if branches.len() == 2 {
        // Must be a conditional branch followed by an unconditional branch.
        let op0 = ctx.data(branches[0]).opcode();
        let op1 = ctx.data(branches[1]).opcode();

        assert!(op1 == Opcode::Jump || op1 == Opcode::Fallthrough);
        let taken = BranchTarget::Label(targets[0]);
        // not_taken target is the target of the second branch, even if it is a Fallthrough
        // instruction: because we reorder blocks while we lower, the fallthrough in the new
        // order is not (necessarily) the same as the fallthrough in CLIF. So we use the
        // explicitly-provided target.
        let not_taken = BranchTarget::Label(targets[1]);

        match op0 {
            Opcode::Brz | Opcode::Brnz => {
                let ty = ctx.input_ty(branches[0], 0);
                let flag_input = InsnInput {
                    insn: branches[0],
                    input: 0,
                };
                if let Some(icmp_insn) =
                    maybe_input_insn_via_conv(ctx, flag_input, Opcode::Icmp, Opcode::Bint)
                {
                    let condcode = ctx.data(icmp_insn).cond_code().unwrap();
                    let cond =
                        lower_icmp(ctx, icmp_insn, condcode, IcmpOutput::Flags)?.unwrap_cond();
                    let negated = op0 == Opcode::Brz;
                    let cond = if negated { cond.invert() } else { cond };

                    ctx.emit(Inst::CondBr {
                        taken,
                        not_taken,
                        kind: CondBrKind::Cond(cond),
                    });
                } else if let Some(fcmp_insn) =
                    maybe_input_insn_via_conv(ctx, flag_input, Opcode::Fcmp, Opcode::Bint)
                {
                    let condcode = ctx.data(fcmp_insn).fp_cond_code().unwrap();
                    let cond = lower_fp_condcode(condcode);
                    let negated = op0 == Opcode::Brz;
                    let cond = if negated { cond.invert() } else { cond };

                    lower_fcmp_or_ffcmp_to_flags(ctx, fcmp_insn);
                    ctx.emit(Inst::CondBr {
                        taken,
                        not_taken,
                        kind: CondBrKind::Cond(cond),
                    });
                } else {
                    let rt = if ty == I128 {
                        let tmp = ctx.alloc_tmp(I64).only_reg().unwrap();
                        let input = put_input_in_regs(ctx, flag_input);
                        ctx.emit(Inst::AluRRR {
                            alu_op: ALUOp::Orr64,
                            rd: tmp,
                            rn: input.regs()[0],
                            rm: input.regs()[1],
                        });
                        tmp.to_reg()
                    } else {
                        put_input_in_reg(ctx, flag_input, NarrowValueMode::ZeroExtend64)
                    };
                    let kind = match op0 {
                        Opcode::Brz => CondBrKind::Zero(rt),
                        Opcode::Brnz => CondBrKind::NotZero(rt),
                        _ => unreachable!(),
                    };
                    ctx.emit(Inst::CondBr {
                        taken,
                        not_taken,
                        kind,
                    });
                }
            }
            Opcode::BrIcmp => {
                let condcode = ctx.data(branches[0]).cond_code().unwrap();
                let cond = lower_icmp(ctx, branches[0], condcode, IcmpOutput::Flags)?.unwrap_cond();

                ctx.emit(Inst::CondBr {
                    taken,
                    not_taken,
                    kind: CondBrKind::Cond(cond),
                });
            }

            Opcode::Brif => {
                let condcode = ctx.data(branches[0]).cond_code().unwrap();

                let flag_input = InsnInput {
                    insn: branches[0],
                    input: 0,
                };
                if let Some(ifcmp_insn) = maybe_input_insn(ctx, flag_input, Opcode::Ifcmp) {
                    let cond =
                        lower_icmp(ctx, ifcmp_insn, condcode, IcmpOutput::Flags)?.unwrap_cond();
                    ctx.emit(Inst::CondBr {
                        taken,
                        not_taken,
                        kind: CondBrKind::Cond(cond),
                    });
                } else {
                    // If the ifcmp result is actually placed in a
                    // register, we need to move it back into the flags.
                    let rn = put_input_in_reg(ctx, flag_input, NarrowValueMode::None);
                    ctx.emit(Inst::MovToNZCV { rn });
                    ctx.emit(Inst::CondBr {
                        taken,
                        not_taken,
                        kind: CondBrKind::Cond(lower_condcode(condcode)),
                    });
                }
            }

            Opcode::Brff => {
                let condcode = ctx.data(branches[0]).fp_cond_code().unwrap();
                let cond = lower_fp_condcode(condcode);
                let kind = CondBrKind::Cond(cond);
                let flag_input = InsnInput {
                    insn: branches[0],
                    input: 0,
                };
                if let Some(ffcmp_insn) = maybe_input_insn(ctx, flag_input, Opcode::Ffcmp) {
                    lower_fcmp_or_ffcmp_to_flags(ctx, ffcmp_insn);
                    ctx.emit(Inst::CondBr {
                        taken,
                        not_taken,
                        kind,
                    });
                } else {
                    // If the ffcmp result is actually placed in a
                    // register, we need to move it back into the flags.
                    let rn = put_input_in_reg(ctx, flag_input, NarrowValueMode::None);
                    ctx.emit(Inst::MovToNZCV { rn });
                    ctx.emit(Inst::CondBr {
                        taken,
                        not_taken,
                        kind,
                    });
                }
            }

            _ => unimplemented!(),
        }
    } else {
        // Must be an unconditional branch or an indirect branch.
        let op = ctx.data(branches[0]).opcode();
        match op {
            Opcode::Jump | Opcode::Fallthrough => {
                assert!(branches.len() == 1);
                // In the Fallthrough case, the machine-independent driver
                // fills in `targets[0]` with our fallthrough block, so this
                // is valid for both Jump and Fallthrough.
                ctx.emit(Inst::Jump {
                    dest: BranchTarget::Label(targets[0]),
                });
            }

            Opcode::BrTable => {
                // Expand `br_table index, default, JT` to:
                //
                //   emit_island  // this forces an island at this point
                //                // if the jumptable would push us past
                //                // the deadline
                //   subs idx, #jt_size
                //   b.hs default
                //   adr vTmp1, PC+16
                //   ldr vTmp2, [vTmp1, idx, lsl #2]
                //   add vTmp2, vTmp2, vTmp1
                //   br vTmp2
                //   [jumptable offsets relative to JT base]
                let jt_size = targets.len() - 1;
                assert!(jt_size <= std::u32::MAX as usize);

                ctx.emit(Inst::EmitIsland {
                    needed_space: 4 * (6 + jt_size) as CodeOffset,
                });

                let ridx = put_input_in_reg(
                    ctx,
                    InsnInput {
                        insn: branches[0],
                        input: 0,
                    },
                    NarrowValueMode::ZeroExtend32,
                );

                let rtmp1 = ctx.alloc_tmp(I32).only_reg().unwrap();
                let rtmp2 = ctx.alloc_tmp(I32).only_reg().unwrap();

                // Bounds-check, leaving condition codes for JTSequence's
                // branch to default target below.
                if let Some(imm12) = Imm12::maybe_from_u64(jt_size as u64) {
                    ctx.emit(Inst::AluRRImm12 {
                        alu_op: ALUOp::SubS32,
                        rd: writable_zero_reg(),
                        rn: ridx,
                        imm12,
                    });
                } else {
                    lower_constant_u64(ctx, rtmp1, jt_size as u64);
                    ctx.emit(Inst::AluRRR {
                        alu_op: ALUOp::SubS32,
                        rd: writable_zero_reg(),
                        rn: ridx,
                        rm: rtmp1.to_reg(),
                    });
                }

                // Emit the compound instruction that does:
                //
                // b.hs default
                // adr rA, jt
                // ldrsw rB, [rA, rIndex, UXTW 2]
                // add rA, rA, rB
                // br rA
                // [jt entries]
                //
                // This must be *one* instruction in the vcode because
                // we cannot allow regalloc to insert any spills/fills
                // in the middle of the sequence; otherwise, the ADR's
                // PC-rel offset to the jumptable would be incorrect.
                // (The alternative is to introduce a relocation pass
                // for inlined jumptables, which is much worse, IMHO.)

                let jt_targets: Vec<BranchTarget> = targets
                    .iter()
                    .skip(1)
                    .map(|bix| BranchTarget::Label(*bix))
                    .collect();
                let default_target = BranchTarget::Label(targets[0]);
                let targets_for_term: Vec<MachLabel> = targets.to_vec();
                ctx.emit(Inst::JTSequence {
                    ridx,
                    rtmp1,
                    rtmp2,
                    info: Box::new(JTSequenceInfo {
                        targets: jt_targets,
                        default_target,
                        targets_for_term,
                    }),
                });
            }

            _ => panic!("Unknown branch type!"),
        }
    }

    Ok(())
}
