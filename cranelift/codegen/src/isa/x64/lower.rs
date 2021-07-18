//! Lowering rules for X64.

use crate::data_value::DataValue;
use crate::ir::{
    condcodes::FloatCC, condcodes::IntCC, types, AbiParam, ArgumentPurpose, ExternalName,
    Inst as IRInst, InstructionData, LibCall, Opcode, Signature, Type,
};
use crate::isa::x64::abi::*;
use crate::isa::x64::inst::args::*;
use crate::isa::x64::inst::*;
use crate::isa::{x64::settings as x64_settings, x64::X64Backend, CallConv};
use crate::machinst::lower::*;
use crate::machinst::*;
use crate::result::CodegenResult;
use crate::settings::{Flags, TlsModel};
use alloc::boxed::Box;
use alloc::vec::Vec;
use cranelift_codegen_shared::condcodes::CondCode;
use log::trace;
use regalloc::{Reg, RegClass, Writable};
use smallvec::{smallvec, SmallVec};
use std::convert::TryFrom;
use target_lexicon::Triple;

//=============================================================================
// Helpers for instruction lowering.

fn is_int_or_ref_ty(ty: Type) -> bool {
    match ty {
        types::I8 | types::I16 | types::I32 | types::I64 | types::R64 => true,
        types::B1 | types::B8 | types::B16 | types::B32 | types::B64 => true,
        types::R32 => panic!("shouldn't have 32-bits refs on x64"),
        _ => false,
    }
}

fn is_bool_ty(ty: Type) -> bool {
    match ty {
        types::B1 | types::B8 | types::B16 | types::B32 | types::B64 => true,
        types::R32 => panic!("shouldn't have 32-bits refs on x64"),
        _ => false,
    }
}

/// This is target-word-size dependent.  And it excludes booleans and reftypes.
fn is_valid_atomic_transaction_ty(ty: Type) -> bool {
    match ty {
        types::I8 | types::I16 | types::I32 | types::I64 => true,
        _ => false,
    }
}

/// Returns whether the given specified `input` is a result produced by an instruction with Opcode
/// `op`.
// TODO investigate failures with checking against the result index.
fn matches_input<C: LowerCtx<I = Inst>>(
    ctx: &mut C,
    input: InsnInput,
    op: Opcode,
) -> Option<IRInst> {
    let inputs = ctx.get_input_as_source_or_const(input.insn, input.input);
    inputs.inst.and_then(|(src_inst, _)| {
        let data = ctx.data(src_inst);
        if data.opcode() == op {
            return Some(src_inst);
        }
        None
    })
}

/// Returns whether the given specified `input` is a result produced by an instruction with any of
/// the opcodes specified in `ops`.
fn matches_input_any<C: LowerCtx<I = Inst>>(
    ctx: &mut C,
    input: InsnInput,
    ops: &[Opcode],
) -> Option<IRInst> {
    let inputs = ctx.get_input_as_source_or_const(input.insn, input.input);
    inputs.inst.and_then(|(src_inst, _)| {
        let data = ctx.data(src_inst);
        for &op in ops {
            if data.opcode() == op {
                return Some(src_inst);
            }
        }
        None
    })
}

/// Emits instruction(s) to generate the given 64-bit constant value into a newly-allocated
/// temporary register, returning that register.
fn generate_constant<C: LowerCtx<I = Inst>>(ctx: &mut C, ty: Type, c: u64) -> ValueRegs<Reg> {
    let from_bits = ty_bits(ty);
    let masked = if from_bits < 64 {
        c & ((1u64 << from_bits) - 1)
    } else {
        c
    };

    let cst_copy = ctx.alloc_tmp(ty);
    for inst in Inst::gen_constant(cst_copy, masked as u128, ty, |ty| {
        ctx.alloc_tmp(ty).only_reg().unwrap()
    })
    .into_iter()
    {
        ctx.emit(inst);
    }
    non_writable_value_regs(cst_copy)
}

/// Put the given input into possibly multiple registers, and mark it as used (side-effect).
fn put_input_in_regs<C: LowerCtx<I = Inst>>(ctx: &mut C, spec: InsnInput) -> ValueRegs<Reg> {
    let ty = ctx.input_ty(spec.insn, spec.input);
    let input = ctx.get_input_as_source_or_const(spec.insn, spec.input);

    if let Some(c) = input.constant {
        // Generate constants fresh at each use to minimize long-range register pressure.
        generate_constant(ctx, ty, c)
    } else {
        ctx.put_input_in_regs(spec.insn, spec.input)
    }
}

/// Put the given input into a register, and mark it as used (side-effect).
fn put_input_in_reg<C: LowerCtx<I = Inst>>(ctx: &mut C, spec: InsnInput) -> Reg {
    put_input_in_regs(ctx, spec)
        .only_reg()
        .expect("Multi-register value not expected")
}

/// Determines whether a load operation (indicated by `src_insn`) can be merged
/// into the current lowering point. If so, returns the address-base source (as
/// an `InsnInput`) and an offset from that address from which to perform the
/// load.
fn is_mergeable_load<C: LowerCtx<I = Inst>>(
    ctx: &mut C,
    src_insn: IRInst,
) -> Option<(InsnInput, i32)> {
    let insn_data = ctx.data(src_insn);
    let inputs = ctx.num_inputs(src_insn);
    if inputs != 1 {
        return None;
    }

    let load_ty = ctx.output_ty(src_insn, 0);
    if ty_bits(load_ty) < 32 {
        // Narrower values are handled by ALU insts that are at least 32 bits
        // wide, which is normally OK as we ignore upper buts; but, if we
        // generate, e.g., a direct-from-memory 32-bit add for a byte value and
        // the byte is the last byte in a page, the extra data that we load is
        // incorrectly accessed. So we only allow loads to merge for
        // 32-bit-and-above widths.
        return None;
    }

    // Just testing the opcode is enough, because the width will always match if
    // the type does (and the type should match if the CLIF is properly
    // constructed).
    if insn_data.opcode() == Opcode::Load {
        let offset = insn_data
            .load_store_offset()
            .expect("load should have offset");
        Some((
            InsnInput {
                insn: src_insn,
                input: 0,
            },
            offset,
        ))
    } else {
        None
    }
}

/// Put the given input into a register or a memory operand.
/// Effectful: may mark the given input as used, when returning the register form.
fn input_to_reg_mem<C: LowerCtx<I = Inst>>(ctx: &mut C, spec: InsnInput) -> RegMem {
    let inputs = ctx.get_input_as_source_or_const(spec.insn, spec.input);

    if let Some(c) = inputs.constant {
        // Generate constants fresh at each use to minimize long-range register pressure.
        let ty = ctx.input_ty(spec.insn, spec.input);
        return RegMem::reg(generate_constant(ctx, ty, c).only_reg().unwrap());
    }

    if let Some((src_insn, 0)) = inputs.inst {
        if let Some((addr_input, offset)) = is_mergeable_load(ctx, src_insn) {
            ctx.sink_inst(src_insn);
            let amode = lower_to_amode(ctx, addr_input, offset);
            return RegMem::mem(amode);
        }
    }

    RegMem::reg(
        ctx.put_input_in_regs(spec.insn, spec.input)
            .only_reg()
            .unwrap(),
    )
}

/// An extension specification for `extend_input_to_reg`.
#[derive(Clone, Copy)]
enum ExtSpec {
    ZeroExtendTo32,
    ZeroExtendTo64,
    SignExtendTo32,
    #[allow(dead_code)] // not used just yet but may be used in the future!
    SignExtendTo64,
}

/// Put the given input into a register, marking it as used, and do a zero- or signed- extension if
/// required. (This obviously causes side-effects.)
fn extend_input_to_reg<C: LowerCtx<I = Inst>>(
    ctx: &mut C,
    spec: InsnInput,
    ext_spec: ExtSpec,
) -> Reg {
    let requested_size = match ext_spec {
        ExtSpec::ZeroExtendTo32 | ExtSpec::SignExtendTo32 => 32,
        ExtSpec::ZeroExtendTo64 | ExtSpec::SignExtendTo64 => 64,
    };
    let input_size = ctx.input_ty(spec.insn, spec.input).bits();

    let requested_ty = if requested_size == 32 {
        types::I32
    } else {
        types::I64
    };

    let ext_mode = match (input_size, requested_size) {
        (a, b) if a == b => return put_input_in_reg(ctx, spec),
        (1, 8) => return put_input_in_reg(ctx, spec),
        (a, b) => ExtMode::new(a, b).expect(&format!("invalid extension: {} -> {}", a, b)),
    };

    let src = input_to_reg_mem(ctx, spec);
    let dst = ctx.alloc_tmp(requested_ty).only_reg().unwrap();
    match ext_spec {
        ExtSpec::ZeroExtendTo32 | ExtSpec::ZeroExtendTo64 => {
            ctx.emit(Inst::movzx_rm_r(ext_mode, src, dst))
        }
        ExtSpec::SignExtendTo32 | ExtSpec::SignExtendTo64 => {
            ctx.emit(Inst::movsx_rm_r(ext_mode, src, dst))
        }
    }
    dst.to_reg()
}

/// Returns whether the given input is an immediate that can be properly sign-extended, without any
/// possible side-effect.
fn non_reg_input_to_sext_imm(input: NonRegInput, input_ty: Type) -> Option<u32> {
    input.constant.and_then(|x| {
        // For i64 instructions (prefixed with REX.W), require that the immediate will sign-extend
        // to 64 bits. For other sizes, it doesn't matter and we can just use the plain
        // constant.
        if input_ty.bytes() != 8 || low32_will_sign_extend_to_64(x) {
            Some(x as u32)
        } else {
            None
        }
    })
}

fn input_to_imm<C: LowerCtx<I = Inst>>(ctx: &mut C, spec: InsnInput) -> Option<u64> {
    ctx.get_input_as_source_or_const(spec.insn, spec.input)
        .constant
}

/// Put the given input into an immediate, a register or a memory operand.
/// Effectful: may mark the given input as used, when returning the register form.
fn input_to_reg_mem_imm<C: LowerCtx<I = Inst>>(ctx: &mut C, spec: InsnInput) -> RegMemImm {
    let input = ctx.get_input_as_source_or_const(spec.insn, spec.input);
    let input_ty = ctx.input_ty(spec.insn, spec.input);
    match non_reg_input_to_sext_imm(input, input_ty) {
        Some(x) => RegMemImm::imm(x),
        None => match input_to_reg_mem(ctx, spec) {
            RegMem::Reg { reg } => RegMemImm::reg(reg),
            RegMem::Mem { addr } => RegMemImm::mem(addr),
        },
    }
}

/// Emit an instruction to insert a value `src` into a lane of `dst`.
fn emit_insert_lane<C: LowerCtx<I = Inst>>(
    ctx: &mut C,
    src: RegMem,
    dst: Writable<Reg>,
    lane: u8,
    ty: Type,
) {
    if !ty.is_float() {
        let (sse_op, size) = match ty.lane_bits() {
            8 => (SseOpcode::Pinsrb, OperandSize::Size32),
            16 => (SseOpcode::Pinsrw, OperandSize::Size32),
            32 => (SseOpcode::Pinsrd, OperandSize::Size32),
            64 => (SseOpcode::Pinsrd, OperandSize::Size64),
            _ => panic!("Unable to insertlane for lane size: {}", ty.lane_bits()),
        };
        ctx.emit(Inst::xmm_rm_r_imm(sse_op, src, dst, lane, size));
    } else if ty == types::F32 {
        let sse_op = SseOpcode::Insertps;
        // Insert 32-bits from replacement (at index 00, bits 7:8) to vector (lane
        // shifted into bits 5:6).
        let lane = 0b00_00_00_00 | lane << 4;
        ctx.emit(Inst::xmm_rm_r_imm(
            sse_op,
            src,
            dst,
            lane,
            OperandSize::Size32,
        ));
    } else if ty == types::F64 {
        let sse_op = match lane {
            // Move the lowest quadword in replacement to vector without changing
            // the upper bits.
            0 => SseOpcode::Movsd,
            // Move the low 64 bits of replacement vector to the high 64 bits of the
            // vector.
            1 => SseOpcode::Movlhps,
            _ => unreachable!(),
        };
        // Here we use the `xmm_rm_r` encoding because it correctly tells the register
        // allocator how we are using `dst`: we are using `dst` as a `mod` whereas other
        // encoding formats like `xmm_unary_rm_r` treat it as a `def`.
        ctx.emit(Inst::xmm_rm_r(sse_op, src, dst));
    } else {
        panic!("unable to emit insertlane for type: {}", ty)
    }
}

/// Emit an instruction to extract a lane of `src` into `dst`.
fn emit_extract_lane<C: LowerCtx<I = Inst>>(
    ctx: &mut C,
    src: Reg,
    dst: Writable<Reg>,
    lane: u8,
    ty: Type,
) {
    if !ty.is_float() {
        let (sse_op, size) = match ty.lane_bits() {
            8 => (SseOpcode::Pextrb, OperandSize::Size32),
            16 => (SseOpcode::Pextrw, OperandSize::Size32),
            32 => (SseOpcode::Pextrd, OperandSize::Size32),
            64 => (SseOpcode::Pextrd, OperandSize::Size64),
            _ => panic!("Unable to extractlane for lane size: {}", ty.lane_bits()),
        };
        let src = RegMem::reg(src);
        ctx.emit(Inst::xmm_rm_r_imm(sse_op, src, dst, lane, size));
    } else if ty == types::F32 || ty == types::F64 {
        if lane == 0 {
            // Remove the extractlane instruction, leaving the float where it is. The upper
            // bits will remain unchanged; for correctness, this relies on Cranelift type
            // checking to avoid using those bits.
            ctx.emit(Inst::gen_move(dst, src, ty));
        } else {
            // Otherwise, shuffle the bits in `lane` to the lowest lane.
            let sse_op = SseOpcode::Pshufd;
            let mask = match ty {
                // Move the value at `lane` to lane 0, copying existing value at lane 0 to
                // other lanes. Again, this relies on Cranelift type checking to avoid
                // using those bits.
                types::F32 => {
                    assert!(lane > 0 && lane < 4);
                    0b00_00_00_00 | lane
                }
                // Move the value at `lane` 1 (we know it must be 1 because of the `if`
                // statement above) to lane 0 and leave lane 1 unchanged. The Cranelift type
                // checking assumption also applies here.
                types::F64 => {
                    assert!(lane == 1);
                    0b11_10_11_10
                }
                _ => unreachable!(),
            };
            let src = RegMem::reg(src);
            ctx.emit(Inst::xmm_rm_r_imm(
                sse_op,
                src,
                dst,
                mask,
                OperandSize::Size32,
            ));
        }
    } else {
        panic!("unable to emit extractlane for type: {}", ty)
    }
}

/// Emits an int comparison instruction.
///
/// Note: make sure that there are no instructions modifying the flags between a call to this
/// function and the use of the flags!
///
/// Takes the condition code that will be tested, and returns
/// the condition code that should be used. This allows us to
/// synthesize comparisons out of multiple instructions for
/// special cases (e.g., 128-bit integers).
fn emit_cmp<C: LowerCtx<I = Inst>>(ctx: &mut C, insn: IRInst, cc: IntCC) -> IntCC {
    let ty = ctx.input_ty(insn, 0);

    let inputs = [InsnInput { insn, input: 0 }, InsnInput { insn, input: 1 }];

    if ty == types::I128 {
        // We need to compare both halves and combine the results appropriately.
        let cmp1 = ctx.alloc_tmp(types::I64).only_reg().unwrap();
        let cmp2 = ctx.alloc_tmp(types::I64).only_reg().unwrap();
        let lhs = put_input_in_regs(ctx, inputs[0]);
        let lhs_lo = lhs.regs()[0];
        let lhs_hi = lhs.regs()[1];
        let rhs = put_input_in_regs(ctx, inputs[1]);
        let rhs_lo = RegMemImm::reg(rhs.regs()[0]);
        let rhs_hi = RegMemImm::reg(rhs.regs()[1]);
        match cc {
            IntCC::Equal => {
                ctx.emit(Inst::cmp_rmi_r(OperandSize::Size64, rhs_hi, lhs_hi));
                ctx.emit(Inst::setcc(CC::Z, cmp1));
                ctx.emit(Inst::cmp_rmi_r(OperandSize::Size64, rhs_lo, lhs_lo));
                ctx.emit(Inst::setcc(CC::Z, cmp2));
                ctx.emit(Inst::alu_rmi_r(
                    OperandSize::Size64,
                    AluRmiROpcode::And,
                    RegMemImm::reg(cmp1.to_reg()),
                    cmp2,
                ));
                ctx.emit(Inst::alu_rmi_r(
                    OperandSize::Size64,
                    AluRmiROpcode::And,
                    RegMemImm::imm(1),
                    cmp2,
                ));
                IntCC::NotEqual
            }
            IntCC::NotEqual => {
                ctx.emit(Inst::cmp_rmi_r(OperandSize::Size64, rhs_hi, lhs_hi));
                ctx.emit(Inst::setcc(CC::NZ, cmp1));
                ctx.emit(Inst::cmp_rmi_r(OperandSize::Size64, rhs_lo, lhs_lo));
                ctx.emit(Inst::setcc(CC::NZ, cmp2));
                ctx.emit(Inst::alu_rmi_r(
                    OperandSize::Size64,
                    AluRmiROpcode::Or,
                    RegMemImm::reg(cmp1.to_reg()),
                    cmp2,
                ));
                ctx.emit(Inst::alu_rmi_r(
                    OperandSize::Size64,
                    AluRmiROpcode::And,
                    RegMemImm::imm(1),
                    cmp2,
                ));
                IntCC::NotEqual
            }
            IntCC::SignedLessThan
            | IntCC::SignedLessThanOrEqual
            | IntCC::SignedGreaterThan
            | IntCC::SignedGreaterThanOrEqual
            | IntCC::UnsignedLessThan
            | IntCC::UnsignedLessThanOrEqual
            | IntCC::UnsignedGreaterThan
            | IntCC::UnsignedGreaterThanOrEqual => {
                // Result = (lhs_hi <> rhs_hi) ||
                //          (lhs_hi == rhs_hi && lhs_lo <> rhs_lo)
                let cmp3 = ctx.alloc_tmp(types::I64).only_reg().unwrap();
                ctx.emit(Inst::cmp_rmi_r(OperandSize::Size64, rhs_hi, lhs_hi));
                ctx.emit(Inst::setcc(CC::from_intcc(cc.without_equal()), cmp1));
                ctx.emit(Inst::setcc(CC::Z, cmp2));
                ctx.emit(Inst::cmp_rmi_r(OperandSize::Size64, rhs_lo, lhs_lo));
                ctx.emit(Inst::setcc(CC::from_intcc(cc.unsigned()), cmp3));
                ctx.emit(Inst::alu_rmi_r(
                    OperandSize::Size64,
                    AluRmiROpcode::And,
                    RegMemImm::reg(cmp2.to_reg()),
                    cmp3,
                ));
                ctx.emit(Inst::alu_rmi_r(
                    OperandSize::Size64,
                    AluRmiROpcode::Or,
                    RegMemImm::reg(cmp1.to_reg()),
                    cmp3,
                ));
                ctx.emit(Inst::alu_rmi_r(
                    OperandSize::Size64,
                    AluRmiROpcode::And,
                    RegMemImm::imm(1),
                    cmp3,
                ));
                IntCC::NotEqual
            }
            _ => panic!("Unhandled IntCC in I128 comparison: {:?}", cc),
        }
    } else {
        // TODO Try to commute the operands (and invert the condition) if one is an immediate.
        let lhs = put_input_in_reg(ctx, inputs[0]);
        // We force the RHS into a register, and disallow load-op fusion, because we
        // do not have a transitive guarantee that this cmp-site will be the sole
        // user of the value. Consider: the icmp might be the only user of a load,
        // but there may be multiple users of the icmp (e.g.  select or bint
        // instructions) that each invoke `emit_cmp()`. If we were to allow a load
        // to sink to the *latest* one, but other sites did not permit sinking, then
        // we would be missing the load for other cmp-sites.
        let rhs = put_input_in_reg(ctx, inputs[1]);

        // Cranelift's icmp semantics want to compare lhs - rhs, while Intel gives
        // us dst - src at the machine instruction level, so invert operands.
        ctx.emit(Inst::cmp_rmi_r(
            OperandSize::from_ty(ty),
            RegMemImm::reg(rhs),
            lhs,
        ));
        cc
    }
}

/// A specification for a fcmp emission.
enum FcmpSpec {
    /// Normal flow.
    Normal,

    /// Avoid emitting Equal at all costs by inverting it to NotEqual, and indicate when that
    /// happens with `InvertedEqualOrConditions`.
    ///
    /// This is useful in contexts where it is hard/inefficient to produce a single instruction (or
    /// sequence of instructions) that check for an "AND" combination of condition codes; see for
    /// instance lowering of Select.
    InvertEqual,
}

/// This explains how to interpret the results of an fcmp instruction.
enum FcmpCondResult {
    /// The given condition code must be set.
    Condition(CC),

    /// Both condition codes must be set.
    AndConditions(CC, CC),

    /// Either of the conditions codes must be set.
    OrConditions(CC, CC),

    /// The associated spec was set to `FcmpSpec::InvertEqual` and Equal has been inverted. Either
    /// of the condition codes must be set, and the user must invert meaning of analyzing the
    /// condition code results. When the spec is set to `FcmpSpec::Normal`, then this case can't be
    /// reached.
    InvertedEqualOrConditions(CC, CC),
}

/// Emits a float comparison instruction.
///
/// Note: make sure that there are no instructions modifying the flags between a call to this
/// function and the use of the flags!
fn emit_fcmp<C: LowerCtx<I = Inst>>(
    ctx: &mut C,
    insn: IRInst,
    mut cond_code: FloatCC,
    spec: FcmpSpec,
) -> FcmpCondResult {
    let (flip_operands, inverted_equal) = match cond_code {
        FloatCC::LessThan
        | FloatCC::LessThanOrEqual
        | FloatCC::UnorderedOrGreaterThan
        | FloatCC::UnorderedOrGreaterThanOrEqual => {
            cond_code = cond_code.reverse();
            (true, false)
        }
        FloatCC::Equal => {
            let inverted_equal = match spec {
                FcmpSpec::Normal => false,
                FcmpSpec::InvertEqual => {
                    cond_code = FloatCC::NotEqual; // same as .inverse()
                    true
                }
            };
            (false, inverted_equal)
        }
        _ => (false, false),
    };

    // The only valid CC constructed with `from_floatcc` can be put in the flag
    // register with a direct float comparison; do this here.
    let op = match ctx.input_ty(insn, 0) {
        types::F32 => SseOpcode::Ucomiss,
        types::F64 => SseOpcode::Ucomisd,
        _ => panic!("Bad input type to Fcmp"),
    };

    let inputs = &[InsnInput { insn, input: 0 }, InsnInput { insn, input: 1 }];
    let (lhs_input, rhs_input) = if flip_operands {
        (inputs[1], inputs[0])
    } else {
        (inputs[0], inputs[1])
    };
    let lhs = put_input_in_reg(ctx, lhs_input);
    // See above in `emit_cmp()`. We must only use the reg/reg form of the
    // comparison in order to avoid issues with merged loads.
    let rhs = put_input_in_reg(ctx, rhs_input);
    ctx.emit(Inst::xmm_cmp_rm_r(op, RegMem::reg(rhs), lhs));

    let cond_result = match cond_code {
        FloatCC::Equal => FcmpCondResult::AndConditions(CC::NP, CC::Z),
        FloatCC::NotEqual if inverted_equal => {
            FcmpCondResult::InvertedEqualOrConditions(CC::P, CC::NZ)
        }
        FloatCC::NotEqual if !inverted_equal => FcmpCondResult::OrConditions(CC::P, CC::NZ),
        _ => FcmpCondResult::Condition(CC::from_floatcc(cond_code)),
    };

    cond_result
}

fn emit_bitrev<C: LowerCtx<I = Inst>>(ctx: &mut C, src: Reg, dst: Writable<Reg>, ty: Type) {
    let bits = ty.bits();
    let const_mask = if bits == 64 {
        0xffff_ffff_ffff_ffff
    } else {
        (1u64 << bits) - 1
    };
    let tmp0 = ctx.alloc_tmp(types::I64).only_reg().unwrap();
    let tmp1 = ctx.alloc_tmp(types::I64).only_reg().unwrap();
    let tmp2 = ctx.alloc_tmp(types::I64).only_reg().unwrap();

    ctx.emit(Inst::gen_move(tmp0, src, types::I64));

    // Swap 1-bit units.
    // tmp1 = src
    ctx.emit(Inst::gen_move(tmp1, tmp0.to_reg(), types::I64));
    // tmp2 = 0b0101..
    ctx.emit(Inst::imm(
        OperandSize::Size64,
        0x5555_5555_5555_5555 & const_mask,
        tmp2,
    ));
    // tmp1 = src >> 1
    ctx.emit(Inst::shift_r(
        OperandSize::Size64,
        ShiftKind::ShiftRightLogical,
        Some(1),
        tmp1,
    ));
    // tmp1 = (src >> 1) & 0b0101..
    ctx.emit(Inst::alu_rmi_r(
        OperandSize::Size64,
        AluRmiROpcode::And,
        RegMemImm::reg(tmp2.to_reg()),
        tmp1,
    ));
    // tmp2 = src & 0b0101..
    ctx.emit(Inst::alu_rmi_r(
        OperandSize::Size64,
        AluRmiROpcode::And,
        RegMemImm::reg(tmp0.to_reg()),
        tmp2,
    ));
    // tmp2 = (src & 0b0101..) << 1
    ctx.emit(Inst::shift_r(
        OperandSize::Size64,
        ShiftKind::ShiftLeft,
        Some(1),
        tmp2,
    ));
    // tmp0 = (src >> 1) & 0b0101.. | (src & 0b0101..) << 1
    ctx.emit(Inst::gen_move(tmp0, tmp2.to_reg(), types::I64));
    ctx.emit(Inst::alu_rmi_r(
        OperandSize::Size64,
        AluRmiROpcode::Or,
        RegMemImm::reg(tmp1.to_reg()),
        tmp0,
    ));

    // Swap 2-bit units.
    ctx.emit(Inst::gen_move(tmp1, tmp0.to_reg(), types::I64));
    ctx.emit(Inst::imm(
        OperandSize::Size64,
        0x3333_3333_3333_3333 & const_mask,
        tmp2,
    ));
    ctx.emit(Inst::shift_r(
        OperandSize::Size64,
        ShiftKind::ShiftRightLogical,
        Some(2),
        tmp1,
    ));
    ctx.emit(Inst::alu_rmi_r(
        OperandSize::Size64,
        AluRmiROpcode::And,
        RegMemImm::reg(tmp2.to_reg()),
        tmp1,
    ));
    ctx.emit(Inst::alu_rmi_r(
        OperandSize::Size64,
        AluRmiROpcode::And,
        RegMemImm::reg(tmp0.to_reg()),
        tmp2,
    ));
    ctx.emit(Inst::shift_r(
        OperandSize::Size64,
        ShiftKind::ShiftLeft,
        Some(2),
        tmp2,
    ));
    ctx.emit(Inst::gen_move(tmp0, tmp2.to_reg(), types::I64));
    ctx.emit(Inst::alu_rmi_r(
        OperandSize::Size64,
        AluRmiROpcode::Or,
        RegMemImm::reg(tmp1.to_reg()),
        tmp0,
    ));

    // Swap 4-bit units.
    ctx.emit(Inst::gen_move(tmp1, tmp0.to_reg(), types::I64));
    ctx.emit(Inst::imm(
        OperandSize::Size64,
        0x0f0f_0f0f_0f0f_0f0f & const_mask,
        tmp2,
    ));
    ctx.emit(Inst::shift_r(
        OperandSize::Size64,
        ShiftKind::ShiftRightLogical,
        Some(4),
        tmp1,
    ));
    ctx.emit(Inst::alu_rmi_r(
        OperandSize::Size64,
        AluRmiROpcode::And,
        RegMemImm::reg(tmp2.to_reg()),
        tmp1,
    ));
    ctx.emit(Inst::alu_rmi_r(
        OperandSize::Size64,
        AluRmiROpcode::And,
        RegMemImm::reg(tmp0.to_reg()),
        tmp2,
    ));
    ctx.emit(Inst::shift_r(
        OperandSize::Size64,
        ShiftKind::ShiftLeft,
        Some(4),
        tmp2,
    ));
    ctx.emit(Inst::gen_move(tmp0, tmp2.to_reg(), types::I64));
    ctx.emit(Inst::alu_rmi_r(
        OperandSize::Size64,
        AluRmiROpcode::Or,
        RegMemImm::reg(tmp1.to_reg()),
        tmp0,
    ));

    if bits > 8 {
        // Swap 8-bit units.
        ctx.emit(Inst::gen_move(tmp1, tmp0.to_reg(), types::I64));
        ctx.emit(Inst::imm(
            OperandSize::Size64,
            0x00ff_00ff_00ff_00ff & const_mask,
            tmp2,
        ));
        ctx.emit(Inst::shift_r(
            OperandSize::Size64,
            ShiftKind::ShiftRightLogical,
            Some(8),
            tmp1,
        ));
        ctx.emit(Inst::alu_rmi_r(
            OperandSize::Size64,
            AluRmiROpcode::And,
            RegMemImm::reg(tmp2.to_reg()),
            tmp1,
        ));
        ctx.emit(Inst::alu_rmi_r(
            OperandSize::Size64,
            AluRmiROpcode::And,
            RegMemImm::reg(tmp0.to_reg()),
            tmp2,
        ));
        ctx.emit(Inst::shift_r(
            OperandSize::Size64,
            ShiftKind::ShiftLeft,
            Some(8),
            tmp2,
        ));
        ctx.emit(Inst::gen_move(tmp0, tmp2.to_reg(), types::I64));
        ctx.emit(Inst::alu_rmi_r(
            OperandSize::Size64,
            AluRmiROpcode::Or,
            RegMemImm::reg(tmp1.to_reg()),
            tmp0,
        ));
    }

    if bits > 16 {
        // Swap 16-bit units.
        ctx.emit(Inst::gen_move(tmp1, tmp0.to_reg(), types::I64));
        ctx.emit(Inst::imm(
            OperandSize::Size64,
            0x0000_ffff_0000_ffff & const_mask,
            tmp2,
        ));
        ctx.emit(Inst::shift_r(
            OperandSize::Size64,
            ShiftKind::ShiftRightLogical,
            Some(16),
            tmp1,
        ));
        ctx.emit(Inst::alu_rmi_r(
            OperandSize::Size64,
            AluRmiROpcode::And,
            RegMemImm::reg(tmp2.to_reg()),
            tmp1,
        ));
        ctx.emit(Inst::alu_rmi_r(
            OperandSize::Size64,
            AluRmiROpcode::And,
            RegMemImm::reg(tmp0.to_reg()),
            tmp2,
        ));
        ctx.emit(Inst::shift_r(
            OperandSize::Size64,
            ShiftKind::ShiftLeft,
            Some(16),
            tmp2,
        ));
        ctx.emit(Inst::gen_move(tmp0, tmp2.to_reg(), types::I64));
        ctx.emit(Inst::alu_rmi_r(
            OperandSize::Size64,
            AluRmiROpcode::Or,
            RegMemImm::reg(tmp1.to_reg()),
            tmp0,
        ));
    }

    if bits > 32 {
        // Swap 32-bit units.
        ctx.emit(Inst::gen_move(tmp1, tmp0.to_reg(), types::I64));
        ctx.emit(Inst::imm(
            OperandSize::Size64,
            0x0000_0000_ffff_ffff & const_mask,
            tmp2,
        ));
        ctx.emit(Inst::shift_r(
            OperandSize::Size64,
            ShiftKind::ShiftRightLogical,
            Some(32),
            tmp1,
        ));
        ctx.emit(Inst::alu_rmi_r(
            OperandSize::Size64,
            AluRmiROpcode::And,
            RegMemImm::reg(tmp2.to_reg()),
            tmp1,
        ));
        ctx.emit(Inst::alu_rmi_r(
            OperandSize::Size64,
            AluRmiROpcode::And,
            RegMemImm::reg(tmp0.to_reg()),
            tmp2,
        ));
        ctx.emit(Inst::shift_r(
            OperandSize::Size64,
            ShiftKind::ShiftLeft,
            Some(32),
            tmp2,
        ));
        ctx.emit(Inst::gen_move(tmp0, tmp2.to_reg(), types::I64));
        ctx.emit(Inst::alu_rmi_r(
            OperandSize::Size64,
            AluRmiROpcode::Or,
            RegMemImm::reg(tmp1.to_reg()),
            tmp0,
        ));
    }

    ctx.emit(Inst::gen_move(dst, tmp0.to_reg(), types::I64));
}

fn emit_shl_i128<C: LowerCtx<I = Inst>>(
    ctx: &mut C,
    src: ValueRegs<Reg>,
    dst: ValueRegs<Writable<Reg>>,
    amt_src: Reg,
) {
    let src_lo = src.regs()[0];
    let src_hi = src.regs()[1];
    let dst_lo = dst.regs()[0];
    let dst_hi = dst.regs()[1];

    // mov tmp1, src_lo
    // shl tmp1, amt_src
    // mov tmp2, src_hi
    // shl tmp2, amt_src
    // mov amt, 64
    // sub amt, amt_src
    // mov tmp3, src_lo
    // shr tmp3, amt
    // xor dst_lo, dst_lo
    // test amt_src, 127
    // cmovz tmp3, dst_lo
    // or tmp3, tmp2
    // mov amt, amt_src
    // and amt, 64
    // cmovz dst_hi, tmp3
    // cmovz dst_lo, tmp1
    // cmovnz dst_hi, tmp1

    let tmp1 = ctx.alloc_tmp(types::I64).only_reg().unwrap();
    let tmp2 = ctx.alloc_tmp(types::I64).only_reg().unwrap();
    let tmp3 = ctx.alloc_tmp(types::I64).only_reg().unwrap();
    let amt = ctx.alloc_tmp(types::I64).only_reg().unwrap();

    ctx.emit(Inst::gen_move(tmp1, src_lo, types::I64));
    ctx.emit(Inst::gen_move(
        Writable::from_reg(regs::rcx()),
        amt_src,
        types::I64,
    ));
    ctx.emit(Inst::shift_r(
        OperandSize::Size64,
        ShiftKind::ShiftLeft,
        None,
        tmp1,
    ));

    ctx.emit(Inst::gen_move(tmp2, src_hi, types::I64));
    ctx.emit(Inst::gen_move(
        Writable::from_reg(regs::rcx()),
        amt_src,
        types::I64,
    ));
    ctx.emit(Inst::shift_r(
        OperandSize::Size64,
        ShiftKind::ShiftLeft,
        None,
        tmp2,
    ));

    ctx.emit(Inst::imm(OperandSize::Size64, 64, amt));
    ctx.emit(Inst::alu_rmi_r(
        OperandSize::Size64,
        AluRmiROpcode::Sub,
        RegMemImm::reg(amt_src),
        amt,
    ));

    ctx.emit(Inst::gen_move(tmp3, src_lo, types::I64));
    ctx.emit(Inst::gen_move(
        Writable::from_reg(regs::rcx()),
        amt.to_reg(),
        types::I64,
    ));
    ctx.emit(Inst::shift_r(
        OperandSize::Size64,
        ShiftKind::ShiftRightLogical,
        None,
        tmp3,
    ));
    ctx.emit(Inst::alu_rmi_r(
        OperandSize::Size64,
        AluRmiROpcode::Xor,
        RegMemImm::reg(dst_lo.to_reg()),
        dst_lo,
    ));

    ctx.emit(Inst::test_rmi_r(
        OperandSize::Size64,
        RegMemImm::imm(127),
        amt_src,
    ));
    ctx.emit(Inst::cmove(
        OperandSize::Size64,
        CC::Z,
        RegMem::reg(dst_lo.to_reg()),
        tmp3,
    ));

    ctx.emit(Inst::alu_rmi_r(
        OperandSize::Size64,
        AluRmiROpcode::Or,
        RegMemImm::reg(tmp2.to_reg()),
        tmp3,
    ));

    // This isn't semantically necessary, but it keeps the
    // register allocator happy, because it cannot otherwise
    // infer that cmovz + cmovnz always defines dst_hi.
    ctx.emit(Inst::alu_rmi_r(
        OperandSize::Size64,
        AluRmiROpcode::Xor,
        RegMemImm::reg(dst_hi.to_reg()),
        dst_hi,
    ));

    ctx.emit(Inst::gen_move(amt, amt_src, types::I64));
    ctx.emit(Inst::alu_rmi_r(
        OperandSize::Size64,
        AluRmiROpcode::And,
        RegMemImm::imm(64),
        amt,
    ));
    ctx.emit(Inst::cmove(
        OperandSize::Size64,
        CC::Z,
        RegMem::reg(tmp3.to_reg()),
        dst_hi,
    ));
    ctx.emit(Inst::cmove(
        OperandSize::Size64,
        CC::Z,
        RegMem::reg(tmp1.to_reg()),
        dst_lo,
    ));
    ctx.emit(Inst::cmove(
        OperandSize::Size64,
        CC::NZ,
        RegMem::reg(tmp1.to_reg()),
        dst_hi,
    ));
}

fn emit_shr_i128<C: LowerCtx<I = Inst>>(
    ctx: &mut C,
    src: ValueRegs<Reg>,
    dst: ValueRegs<Writable<Reg>>,
    amt_src: Reg,
    is_signed: bool,
) {
    let src_lo = src.regs()[0];
    let src_hi = src.regs()[1];
    let dst_lo = dst.regs()[0];
    let dst_hi = dst.regs()[1];

    // mov tmp1, src_hi
    // {u,s}shr tmp1, amt_src
    // mov tmp2, src_lo
    // ushr tmp2, amt_src
    // mov amt, 64
    // sub amt, amt_src
    // mov tmp3, src_hi
    // shl tmp3, amt
    // xor dst_lo, dst_lo
    // test amt_src, 127
    // cmovz tmp3, dst_lo
    // or tmp3, tmp2
    // if is_signed:
    //   mov dst_hi, src_hi
    //   sshr dst_hi, 63  // get the sign bit
    // else:
    //   xor dst_hi, dst_hi
    // mov amt, amt_src
    // and amt, 64
    // cmovz dst_hi, tmp1
    // cmovz dst_lo, tmp3
    // cmovnz dst_lo, tmp1

    let tmp1 = ctx.alloc_tmp(types::I64).only_reg().unwrap();
    let tmp2 = ctx.alloc_tmp(types::I64).only_reg().unwrap();
    let tmp3 = ctx.alloc_tmp(types::I64).only_reg().unwrap();
    let amt = ctx.alloc_tmp(types::I64).only_reg().unwrap();

    let shift_kind = if is_signed {
        ShiftKind::ShiftRightArithmetic
    } else {
        ShiftKind::ShiftRightLogical
    };

    ctx.emit(Inst::gen_move(tmp1, src_hi, types::I64));
    ctx.emit(Inst::gen_move(
        Writable::from_reg(regs::rcx()),
        amt_src,
        types::I64,
    ));
    ctx.emit(Inst::shift_r(OperandSize::Size64, shift_kind, None, tmp1));

    ctx.emit(Inst::gen_move(tmp2, src_lo, types::I64));
    ctx.emit(Inst::gen_move(
        Writable::from_reg(regs::rcx()),
        amt_src,
        types::I64,
    ));
    // N.B.: right-shift of *lower* half is *always* unsigned (its MSB is not a sign bit).
    ctx.emit(Inst::shift_r(
        OperandSize::Size64,
        ShiftKind::ShiftRightLogical,
        None,
        tmp2,
    ));

    ctx.emit(Inst::imm(OperandSize::Size64, 64, amt));
    ctx.emit(Inst::alu_rmi_r(
        OperandSize::Size64,
        AluRmiROpcode::Sub,
        RegMemImm::reg(amt_src),
        amt,
    ));

    ctx.emit(Inst::gen_move(tmp3, src_hi, types::I64));
    ctx.emit(Inst::gen_move(
        Writable::from_reg(regs::rcx()),
        amt.to_reg(),
        types::I64,
    ));
    ctx.emit(Inst::shift_r(
        OperandSize::Size64,
        ShiftKind::ShiftLeft,
        None,
        tmp3,
    ));

    ctx.emit(Inst::alu_rmi_r(
        OperandSize::Size64,
        AluRmiROpcode::Xor,
        RegMemImm::reg(dst_lo.to_reg()),
        dst_lo,
    ));
    ctx.emit(Inst::test_rmi_r(
        OperandSize::Size64,
        RegMemImm::imm(127),
        amt_src,
    ));
    ctx.emit(Inst::cmove(
        OperandSize::Size64,
        CC::Z,
        RegMem::reg(dst_lo.to_reg()),
        tmp3,
    ));

    ctx.emit(Inst::alu_rmi_r(
        OperandSize::Size64,
        AluRmiROpcode::Or,
        RegMemImm::reg(tmp2.to_reg()),
        tmp3,
    ));

    if is_signed {
        ctx.emit(Inst::gen_move(dst_hi, src_hi, types::I64));
        ctx.emit(Inst::shift_r(
            OperandSize::Size64,
            ShiftKind::ShiftRightArithmetic,
            Some(63),
            dst_hi,
        ));
    } else {
        ctx.emit(Inst::alu_rmi_r(
            OperandSize::Size64,
            AluRmiROpcode::Xor,
            RegMemImm::reg(dst_hi.to_reg()),
            dst_hi,
        ));
    }
    // This isn't semantically necessary, but it keeps the
    // register allocator happy, because it cannot otherwise
    // infer that cmovz + cmovnz always defines dst_lo.
    ctx.emit(Inst::alu_rmi_r(
        OperandSize::Size64,
        AluRmiROpcode::Xor,
        RegMemImm::reg(dst_lo.to_reg()),
        dst_lo,
    ));

    ctx.emit(Inst::gen_move(amt, amt_src, types::I64));
    ctx.emit(Inst::alu_rmi_r(
        OperandSize::Size64,
        AluRmiROpcode::And,
        RegMemImm::imm(64),
        amt,
    ));
    ctx.emit(Inst::cmove(
        OperandSize::Size64,
        CC::Z,
        RegMem::reg(tmp1.to_reg()),
        dst_hi,
    ));
    ctx.emit(Inst::cmove(
        OperandSize::Size64,
        CC::Z,
        RegMem::reg(tmp3.to_reg()),
        dst_lo,
    ));
    ctx.emit(Inst::cmove(
        OperandSize::Size64,
        CC::NZ,
        RegMem::reg(tmp1.to_reg()),
        dst_lo,
    ));
}

fn make_libcall_sig<C: LowerCtx<I = Inst>>(
    ctx: &mut C,
    insn: IRInst,
    call_conv: CallConv,
    ptr_ty: Type,
) -> Signature {
    let mut sig = Signature::new(call_conv);
    for i in 0..ctx.num_inputs(insn) {
        sig.params.push(AbiParam::new(ctx.input_ty(insn, i)));
    }
    for i in 0..ctx.num_outputs(insn) {
        sig.returns.push(AbiParam::new(ctx.output_ty(insn, i)));
    }
    if call_conv.extends_baldrdash() {
        // Adds the special VMContext parameter to the signature.
        sig.params
            .push(AbiParam::special(ptr_ty, ArgumentPurpose::VMContext));
    }
    sig
}

fn emit_vm_call<C: LowerCtx<I = Inst>>(
    ctx: &mut C,
    flags: &Flags,
    triple: &Triple,
    libcall: LibCall,
    insn: IRInst,
    inputs: SmallVec<[InsnInput; 4]>,
    outputs: SmallVec<[InsnOutput; 2]>,
) -> CodegenResult<()> {
    let extname = ExternalName::LibCall(libcall);

    let dist = if flags.use_colocated_libcalls() {
        RelocDistance::Near
    } else {
        RelocDistance::Far
    };

    // TODO avoid recreating signatures for every single Libcall function.
    let call_conv = CallConv::for_libcall(flags, CallConv::triple_default(triple));
    let sig = make_libcall_sig(ctx, insn, call_conv, types::I64);
    let caller_conv = ctx.abi().call_conv();

    let mut abi = X64ABICaller::from_func(&sig, &extname, dist, caller_conv, flags)?;

    abi.emit_stack_pre_adjust(ctx);

    let vm_context = if call_conv.extends_baldrdash() { 1 } else { 0 };
    assert_eq!(inputs.len() + vm_context, abi.num_args());

    for (i, input) in inputs.iter().enumerate() {
        let arg_reg = put_input_in_reg(ctx, *input);
        abi.emit_copy_regs_to_arg(ctx, i, ValueRegs::one(arg_reg));
    }
    if call_conv.extends_baldrdash() {
        let vm_context_vreg = ctx
            .get_vm_context()
            .expect("should have a VMContext to pass to libcall funcs");
        abi.emit_copy_regs_to_arg(ctx, inputs.len(), ValueRegs::one(vm_context_vreg));
    }

    abi.emit_call(ctx);
    for (i, output) in outputs.iter().enumerate() {
        let retval_reg = get_output_reg(ctx, *output).only_reg().unwrap();
        abi.emit_copy_retval_to_regs(ctx, i, ValueRegs::one(retval_reg));
    }
    abi.emit_stack_post_adjust(ctx);

    Ok(())
}

/// Returns whether the given input is a shift by a constant value less or equal than 3.
/// The goal is to embed it within an address mode.
fn matches_small_constant_shift<C: LowerCtx<I = Inst>>(
    ctx: &mut C,
    spec: InsnInput,
) -> Option<(InsnInput, u8)> {
    matches_input(ctx, spec, Opcode::Ishl).and_then(|shift| {
        match input_to_imm(
            ctx,
            InsnInput {
                insn: shift,
                input: 1,
            },
        ) {
            Some(shift_amt) if shift_amt <= 3 => Some((
                InsnInput {
                    insn: shift,
                    input: 0,
                },
                shift_amt as u8,
            )),
            _ => None,
        }
    })
}

/// Lowers an instruction to one of the x86 addressing modes.
///
/// Note: the 32-bit offset in Cranelift has to be sign-extended, which maps x86's behavior.
fn lower_to_amode<C: LowerCtx<I = Inst>>(ctx: &mut C, spec: InsnInput, offset: i32) -> Amode {
    let flags = ctx
        .memflags(spec.insn)
        .expect("Instruction with amode should have memflags");

    // We now either have an add that we must materialize, or some other input; as well as the
    // final offset.
    if let Some(add) = matches_input(ctx, spec, Opcode::Iadd) {
        debug_assert_eq!(ctx.output_ty(add, 0), types::I64);
        let add_inputs = &[
            InsnInput {
                insn: add,
                input: 0,
            },
            InsnInput {
                insn: add,
                input: 1,
            },
        ];

        // TODO heap_addr legalization generates a uext64 *after* the shift, so these optimizations
        // aren't happening in the wasm case. We could do better, given some range analysis.
        let (base, index, shift) = if let Some((shift_input, shift_amt)) =
            matches_small_constant_shift(ctx, add_inputs[0])
        {
            (
                put_input_in_reg(ctx, add_inputs[1]),
                put_input_in_reg(ctx, shift_input),
                shift_amt,
            )
        } else if let Some((shift_input, shift_amt)) =
            matches_small_constant_shift(ctx, add_inputs[1])
        {
            (
                put_input_in_reg(ctx, add_inputs[0]),
                put_input_in_reg(ctx, shift_input),
                shift_amt,
            )
        } else {
            for i in 0..=1 {
                // Try to pierce through uextend.
                if let Some(uextend) = matches_input(
                    ctx,
                    InsnInput {
                        insn: add,
                        input: i,
                    },
                    Opcode::Uextend,
                ) {
                    if let Some(cst) = ctx.get_input_as_source_or_const(uextend, 0).constant {
                        // Zero the upper bits.
                        let input_size = ctx.input_ty(uextend, 0).bits() as u64;
                        let shift: u64 = 64 - input_size;
                        let uext_cst: u64 = (cst << shift) >> shift;

                        let final_offset = (offset as i64).wrapping_add(uext_cst as i64);
                        if low32_will_sign_extend_to_64(final_offset as u64) {
                            let base = put_input_in_reg(ctx, add_inputs[1 - i]);
                            return Amode::imm_reg(final_offset as u32, base).with_flags(flags);
                        }
                    }
                }

                // If it's a constant, add it directly!
                if let Some(cst) = ctx.get_input_as_source_or_const(add, i).constant {
                    let final_offset = (offset as i64).wrapping_add(cst as i64);
                    if low32_will_sign_extend_to_64(final_offset as u64) {
                        let base = put_input_in_reg(ctx, add_inputs[1 - i]);
                        return Amode::imm_reg(final_offset as u32, base).with_flags(flags);
                    }
                }
            }

            (
                put_input_in_reg(ctx, add_inputs[0]),
                put_input_in_reg(ctx, add_inputs[1]),
                0,
            )
        };

        return Amode::imm_reg_reg_shift(offset as u32, base, index, shift).with_flags(flags);
    }

    let input = put_input_in_reg(ctx, spec);
    Amode::imm_reg(offset as u32, input).with_flags(flags)
}

fn emit_moves<C: LowerCtx<I = Inst>>(
    ctx: &mut C,
    dst: ValueRegs<Writable<Reg>>,
    src: ValueRegs<Reg>,
    ty: Type,
) {
    let (_, tys) = Inst::rc_for_type(ty).unwrap();
    for ((dst, src), ty) in dst.regs().iter().zip(src.regs().iter()).zip(tys.iter()) {
        ctx.emit(Inst::gen_move(*dst, *src, *ty));
    }
}

fn emit_cmoves<C: LowerCtx<I = Inst>>(
    ctx: &mut C,
    size: u8,
    cc: CC,
    src: ValueRegs<Reg>,
    dst: ValueRegs<Writable<Reg>>,
) {
    let size = size / src.len() as u8;
    let size = u8::max(size, 4); // at least 32 bits
    for (dst, src) in dst.regs().iter().zip(src.regs().iter()) {
        ctx.emit(Inst::cmove(
            OperandSize::from_bytes(size.into()),
            cc,
            RegMem::reg(*src),
            *dst,
        ));
    }
}

fn emit_clz<C: LowerCtx<I = Inst>>(
    ctx: &mut C,
    orig_ty: Type,
    ty: Type,
    src: Reg,
    dst: Writable<Reg>,
) {
    let src = RegMem::reg(src);
    let tmp = ctx.alloc_tmp(ty).only_reg().unwrap();
    ctx.emit(Inst::imm(OperandSize::from_ty(ty), u64::max_value(), dst));

    ctx.emit(Inst::unary_rm_r(
        OperandSize::from_ty(ty),
        UnaryRmROpcode::Bsr,
        src,
        tmp,
    ));

    ctx.emit(Inst::cmove(
        OperandSize::from_ty(ty),
        CC::Z,
        RegMem::reg(dst.to_reg()),
        tmp,
    ));

    ctx.emit(Inst::imm(
        OperandSize::from_ty(ty),
        orig_ty.bits() as u64 - 1,
        dst,
    ));

    ctx.emit(Inst::alu_rmi_r(
        if ty == types::I64 {
            OperandSize::Size64
        } else {
            OperandSize::Size32
        },
        AluRmiROpcode::Sub,
        RegMemImm::reg(tmp.to_reg()),
        dst,
    ));
}

fn emit_ctz<C: LowerCtx<I = Inst>>(
    ctx: &mut C,
    orig_ty: Type,
    ty: Type,
    src: Reg,
    dst: Writable<Reg>,
) {
    let src = RegMem::reg(src);
    let tmp = ctx.alloc_tmp(ty).only_reg().unwrap();
    ctx.emit(Inst::imm(OperandSize::Size32, orig_ty.bits() as u64, tmp));

    ctx.emit(Inst::unary_rm_r(
        OperandSize::from_ty(ty),
        UnaryRmROpcode::Bsf,
        src,
        dst,
    ));

    ctx.emit(Inst::cmove(
        OperandSize::from_ty(ty),
        CC::Z,
        RegMem::reg(tmp.to_reg()),
        dst,
    ));
}

//=============================================================================
// Top-level instruction lowering entry point, for one instruction.

/// Actually codegen an instruction's results into registers.
fn lower_insn_to_regs<C: LowerCtx<I = Inst>>(
    ctx: &mut C,
    insn: IRInst,
    flags: &Flags,
    isa_flags: &x64_settings::Flags,
    triple: &Triple,
) -> CodegenResult<()> {
    let op = ctx.data(insn).opcode();

    let inputs: SmallVec<[InsnInput; 4]> = (0..ctx.num_inputs(insn))
        .map(|i| InsnInput { insn, input: i })
        .collect();
    let outputs: SmallVec<[InsnOutput; 2]> = (0..ctx.num_outputs(insn))
        .map(|i| InsnOutput { insn, output: i })
        .collect();

    let ty = if outputs.len() > 0 {
        Some(ctx.output_ty(insn, 0))
    } else {
        None
    };

    match op {
        Opcode::Iconst | Opcode::Bconst | Opcode::Null => {
            let value = ctx
                .get_constant(insn)
                .expect("constant value for iconst et al");
            let dst = get_output_reg(ctx, outputs[0]);
            for inst in Inst::gen_constant(dst, value as u128, ty.unwrap(), |ty| {
                ctx.alloc_tmp(ty).only_reg().unwrap()
            }) {
                ctx.emit(inst);
            }
        }

        Opcode::Iadd
        | Opcode::IaddIfcout
        | Opcode::SaddSat
        | Opcode::UaddSat
        | Opcode::Isub
        | Opcode::SsubSat
        | Opcode::UsubSat
        | Opcode::AvgRound
        | Opcode::Band
        | Opcode::Bor
        | Opcode::Bxor => {
            let ty = ty.unwrap();
            if ty.lane_count() > 1 {
                let sse_op = match op {
                    Opcode::Iadd => match ty {
                        types::I8X16 => SseOpcode::Paddb,
                        types::I16X8 => SseOpcode::Paddw,
                        types::I32X4 => SseOpcode::Paddd,
                        types::I64X2 => SseOpcode::Paddq,
                        _ => panic!("Unsupported type for packed iadd instruction: {}", ty),
                    },
                    Opcode::SaddSat => match ty {
                        types::I8X16 => SseOpcode::Paddsb,
                        types::I16X8 => SseOpcode::Paddsw,
                        _ => panic!("Unsupported type for packed sadd_sat instruction: {}", ty),
                    },
                    Opcode::UaddSat => match ty {
                        types::I8X16 => SseOpcode::Paddusb,
                        types::I16X8 => SseOpcode::Paddusw,
                        _ => panic!("Unsupported type for packed uadd_sat instruction: {}", ty),
                    },
                    Opcode::Isub => match ty {
                        types::I8X16 => SseOpcode::Psubb,
                        types::I16X8 => SseOpcode::Psubw,
                        types::I32X4 => SseOpcode::Psubd,
                        types::I64X2 => SseOpcode::Psubq,
                        _ => panic!("Unsupported type for packed isub instruction: {}", ty),
                    },
                    Opcode::SsubSat => match ty {
                        types::I8X16 => SseOpcode::Psubsb,
                        types::I16X8 => SseOpcode::Psubsw,
                        _ => panic!("Unsupported type for packed ssub_sat instruction: {}", ty),
                    },
                    Opcode::UsubSat => match ty {
                        types::I8X16 => SseOpcode::Psubusb,
                        types::I16X8 => SseOpcode::Psubusw,
                        _ => panic!("Unsupported type for packed usub_sat instruction: {}", ty),
                    },
                    Opcode::AvgRound => match ty {
                        types::I8X16 => SseOpcode::Pavgb,
                        types::I16X8 => SseOpcode::Pavgw,
                        _ => panic!("Unsupported type for packed avg_round instruction: {}", ty),
                    },
                    Opcode::Band => match ty {
                        types::F32X4 => SseOpcode::Andps,
                        types::F64X2 => SseOpcode::Andpd,
                        _ => SseOpcode::Pand,
                    },
                    Opcode::Bor => match ty {
                        types::F32X4 => SseOpcode::Orps,
                        types::F64X2 => SseOpcode::Orpd,
                        _ => SseOpcode::Por,
                    },
                    Opcode::Bxor => match ty {
                        types::F32X4 => SseOpcode::Xorps,
                        types::F64X2 => SseOpcode::Xorpd,
                        _ => SseOpcode::Pxor,
                    },
                    _ => panic!("Unsupported packed instruction: {}", op),
                };
                let lhs = put_input_in_reg(ctx, inputs[0]);
                let rhs = input_to_reg_mem(ctx, inputs[1]);
                let dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();

                // Move the `lhs` to the same register as `dst`.
                ctx.emit(Inst::gen_move(dst, lhs, ty));
                ctx.emit(Inst::xmm_rm_r(sse_op, rhs, dst));
            } else if ty == types::I128 || ty == types::B128 {
                let alu_ops = match op {
                    Opcode::Iadd => (AluRmiROpcode::Add, AluRmiROpcode::Adc),
                    Opcode::Isub => (AluRmiROpcode::Sub, AluRmiROpcode::Sbb),
                    Opcode::Band => (AluRmiROpcode::And, AluRmiROpcode::And),
                    Opcode::Bor => (AluRmiROpcode::Or, AluRmiROpcode::Or),
                    Opcode::Bxor => (AluRmiROpcode::Xor, AluRmiROpcode::Xor),
                    _ => panic!("Unsupported opcode with 128-bit integers: {:?}", op),
                };
                let lhs = put_input_in_regs(ctx, inputs[0]);
                let rhs = put_input_in_regs(ctx, inputs[1]);
                let dst = get_output_reg(ctx, outputs[0]);
                assert_eq!(lhs.len(), 2);
                assert_eq!(rhs.len(), 2);
                assert_eq!(dst.len(), 2);

                // For add, sub, and, or, xor: just do ops on lower then upper
                // half. Carry-flag propagation is implicit (add/adc, sub/sbb).
                ctx.emit(Inst::gen_move(dst.regs()[0], lhs.regs()[0], types::I64));
                ctx.emit(Inst::gen_move(dst.regs()[1], lhs.regs()[1], types::I64));
                ctx.emit(Inst::alu_rmi_r(
                    OperandSize::Size64,
                    alu_ops.0,
                    RegMemImm::reg(rhs.regs()[0]),
                    dst.regs()[0],
                ));
                ctx.emit(Inst::alu_rmi_r(
                    OperandSize::Size64,
                    alu_ops.1,
                    RegMemImm::reg(rhs.regs()[1]),
                    dst.regs()[1],
                ));
            } else {
                let size = if ty == types::I64 {
                    OperandSize::Size64
                } else {
                    OperandSize::Size32
                };
                let alu_op = match op {
                    Opcode::Iadd | Opcode::IaddIfcout => AluRmiROpcode::Add,
                    Opcode::Isub => AluRmiROpcode::Sub,
                    Opcode::Band => AluRmiROpcode::And,
                    Opcode::Bor => AluRmiROpcode::Or,
                    Opcode::Bxor => AluRmiROpcode::Xor,
                    _ => unreachable!(),
                };

                let (lhs, rhs) = match op {
                    Opcode::Iadd
                    | Opcode::IaddIfcout
                    | Opcode::Band
                    | Opcode::Bor
                    | Opcode::Bxor => {
                        // For commutative operations, try to commute operands if one is an
                        // immediate or direct memory reference. Do so by converting LHS to RMI; if
                        // reg, then always convert RHS to RMI; else, use LHS as RMI and convert
                        // RHS to reg.
                        let lhs = input_to_reg_mem_imm(ctx, inputs[0]);
                        if let RegMemImm::Reg { reg: lhs_reg } = lhs {
                            let rhs = input_to_reg_mem_imm(ctx, inputs[1]);
                            (lhs_reg, rhs)
                        } else {
                            let rhs_reg = put_input_in_reg(ctx, inputs[1]);
                            (rhs_reg, lhs)
                        }
                    }
                    Opcode::Isub => (
                        put_input_in_reg(ctx, inputs[0]),
                        input_to_reg_mem_imm(ctx, inputs[1]),
                    ),
                    _ => unreachable!(),
                };

                let dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
                ctx.emit(Inst::mov_r_r(OperandSize::Size64, lhs, dst));
                ctx.emit(Inst::alu_rmi_r(size, alu_op, rhs, dst));
            }
        }

        Opcode::Imul => {
            let ty = ty.unwrap();

            // Check for ext_mul_* instructions which are being shared here under imul. We must
            // check first for operands that are opcodes since checking for types is not enough.
            if let Some(_) = matches_input_any(
                ctx,
                inputs[0],
                &[
                    Opcode::SwidenHigh,
                    Opcode::SwidenLow,
                    Opcode::UwidenHigh,
                    Opcode::UwidenLow,
                ],
            ) {
                // Optimized ext_mul_* lowerings are based on optimized lowerings
                // here: https://github.com/WebAssembly/simd/pull/376
                if let Some(swiden0_high) = matches_input(ctx, inputs[0], Opcode::SwidenHigh) {
                    if let Some(swiden1_high) = matches_input(ctx, inputs[1], Opcode::SwidenHigh) {
                        let swiden_input = &[
                            InsnInput {
                                insn: swiden0_high,
                                input: 0,
                            },
                            InsnInput {
                                insn: swiden1_high,
                                input: 0,
                            },
                        ];
                        let input0_ty = ctx.input_ty(swiden0_high, 0);
                        let input1_ty = ctx.input_ty(swiden1_high, 0);
                        let output_ty = ctx.output_ty(insn, 0);
                        let lhs = put_input_in_reg(ctx, swiden_input[0]);
                        let rhs = put_input_in_reg(ctx, swiden_input[1]);
                        let dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();

                        match (input0_ty, input1_ty, output_ty) {
                            (types::I8X16, types::I8X16, types::I16X8) => {
                                // i16x8.extmul_high_i8x16_s

                                let tmp_reg = ctx.alloc_tmp(types::I16X8).only_reg().unwrap();
                                ctx.emit(Inst::gen_move(tmp_reg, lhs, output_ty));
                                ctx.emit(Inst::xmm_rm_r_imm(
                                    SseOpcode::Palignr,
                                    RegMem::reg(lhs),
                                    tmp_reg,
                                    8,
                                    OperandSize::Size32,
                                ));
                                ctx.emit(Inst::xmm_mov(
                                    SseOpcode::Pmovsxbw,
                                    RegMem::reg(tmp_reg.to_reg()),
                                    tmp_reg,
                                ));

                                ctx.emit(Inst::gen_move(dst, rhs, output_ty));
                                ctx.emit(Inst::xmm_rm_r_imm(
                                    SseOpcode::Palignr,
                                    RegMem::reg(rhs),
                                    dst,
                                    8,
                                    OperandSize::Size32,
                                ));
                                ctx.emit(Inst::xmm_mov(
                                    SseOpcode::Pmovsxbw,
                                    RegMem::reg(dst.to_reg()),
                                    dst,
                                ));
                                ctx.emit(Inst::xmm_rm_r(
                                    SseOpcode::Pmullw,
                                    RegMem::reg(tmp_reg.to_reg()),
                                    dst,
                                ));
                            }
                            (types::I16X8, types::I16X8, types::I32X4) => {
                                // i32x4.extmul_high_i16x8_s
                                ctx.emit(Inst::gen_move(dst, lhs, input0_ty));
                                let tmp_reg = ctx.alloc_tmp(types::I16X8).only_reg().unwrap();
                                ctx.emit(Inst::gen_move(tmp_reg, lhs, input0_ty));
                                ctx.emit(Inst::xmm_rm_r(SseOpcode::Pmullw, RegMem::reg(rhs), dst));
                                ctx.emit(Inst::xmm_rm_r(
                                    SseOpcode::Pmulhw,
                                    RegMem::reg(rhs),
                                    tmp_reg,
                                ));
                                ctx.emit(Inst::xmm_rm_r(
                                    SseOpcode::Punpckhwd,
                                    RegMem::from(tmp_reg),
                                    dst,
                                ));
                            }
                            (types::I32X4, types::I32X4, types::I64X2) => {
                                // i64x2.extmul_high_i32x4_s
                                let tmp_reg = ctx.alloc_tmp(types::I32X4).only_reg().unwrap();
                                ctx.emit(Inst::xmm_rm_r_imm(
                                    SseOpcode::Pshufd,
                                    RegMem::reg(lhs),
                                    tmp_reg,
                                    0xFA,
                                    OperandSize::Size32,
                                ));
                                ctx.emit(Inst::xmm_rm_r_imm(
                                    SseOpcode::Pshufd,
                                    RegMem::reg(rhs),
                                    dst,
                                    0xFA,
                                    OperandSize::Size32,
                                ));
                                ctx.emit(Inst::xmm_rm_r(
                                    SseOpcode::Pmuldq,
                                    RegMem::reg(tmp_reg.to_reg()),
                                    dst,
                                ));
                            }
                            // Note swiden_high only allows types: I8X16, I16X8, and I32X4
                            _ => panic!("Unsupported extmul_low_signed type"),
                        }
                    }
                } else if let Some(swiden0_low) = matches_input(ctx, inputs[0], Opcode::SwidenLow) {
                    if let Some(swiden1_low) = matches_input(ctx, inputs[1], Opcode::SwidenLow) {
                        let swiden_input = &[
                            InsnInput {
                                insn: swiden0_low,
                                input: 0,
                            },
                            InsnInput {
                                insn: swiden1_low,
                                input: 0,
                            },
                        ];
                        let input0_ty = ctx.input_ty(swiden0_low, 0);
                        let input1_ty = ctx.input_ty(swiden1_low, 0);
                        let output_ty = ctx.output_ty(insn, 0);
                        let lhs = put_input_in_reg(ctx, swiden_input[0]);
                        let rhs = put_input_in_reg(ctx, swiden_input[1]);
                        let dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();

                        match (input0_ty, input1_ty, output_ty) {
                            (types::I8X16, types::I8X16, types::I16X8) => {
                                // i32x4.extmul_low_i8x16_s
                                let tmp_reg = ctx.alloc_tmp(types::I16X8).only_reg().unwrap();
                                ctx.emit(Inst::xmm_mov(
                                    SseOpcode::Pmovsxbw,
                                    RegMem::reg(lhs),
                                    tmp_reg,
                                ));
                                ctx.emit(Inst::xmm_mov(SseOpcode::Pmovsxbw, RegMem::reg(rhs), dst));
                                ctx.emit(Inst::xmm_rm_r(
                                    SseOpcode::Pmullw,
                                    RegMem::reg(tmp_reg.to_reg()),
                                    dst,
                                ));
                            }
                            (types::I16X8, types::I16X8, types::I32X4) => {
                                // i32x4.extmul_low_i16x8_s
                                ctx.emit(Inst::gen_move(dst, lhs, input0_ty));
                                let tmp_reg = ctx.alloc_tmp(types::I16X8).only_reg().unwrap();
                                ctx.emit(Inst::gen_move(tmp_reg, lhs, input0_ty));
                                ctx.emit(Inst::xmm_rm_r(SseOpcode::Pmullw, RegMem::reg(rhs), dst));
                                ctx.emit(Inst::xmm_rm_r(
                                    SseOpcode::Pmulhw,
                                    RegMem::reg(rhs),
                                    tmp_reg,
                                ));
                                ctx.emit(Inst::xmm_rm_r(
                                    SseOpcode::Punpcklwd,
                                    RegMem::from(tmp_reg),
                                    dst,
                                ));
                            }
                            (types::I32X4, types::I32X4, types::I64X2) => {
                                // i64x2.extmul_low_i32x4_s
                                let tmp_reg = ctx.alloc_tmp(types::I32X4).only_reg().unwrap();
                                ctx.emit(Inst::xmm_rm_r_imm(
                                    SseOpcode::Pshufd,
                                    RegMem::reg(lhs),
                                    tmp_reg,
                                    0x50,
                                    OperandSize::Size32,
                                ));
                                ctx.emit(Inst::xmm_rm_r_imm(
                                    SseOpcode::Pshufd,
                                    RegMem::reg(rhs),
                                    dst,
                                    0x50,
                                    OperandSize::Size32,
                                ));
                                ctx.emit(Inst::xmm_rm_r(
                                    SseOpcode::Pmuldq,
                                    RegMem::reg(tmp_reg.to_reg()),
                                    dst,
                                ));
                            }
                            // Note swiden_low only allows types: I8X16, I16X8, and I32X4
                            _ => panic!("Unsupported extmul_low_signed type"),
                        }
                    }
                } else if let Some(uwiden0_high) = matches_input(ctx, inputs[0], Opcode::UwidenHigh)
                {
                    if let Some(uwiden1_high) = matches_input(ctx, inputs[1], Opcode::UwidenHigh) {
                        let uwiden_input = &[
                            InsnInput {
                                insn: uwiden0_high,
                                input: 0,
                            },
                            InsnInput {
                                insn: uwiden1_high,
                                input: 0,
                            },
                        ];
                        let input0_ty = ctx.input_ty(uwiden0_high, 0);
                        let input1_ty = ctx.input_ty(uwiden1_high, 0);
                        let output_ty = ctx.output_ty(insn, 0);
                        let lhs = put_input_in_reg(ctx, uwiden_input[0]);
                        let rhs = put_input_in_reg(ctx, uwiden_input[1]);
                        let dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();

                        match (input0_ty, input1_ty, output_ty) {
                            (types::I8X16, types::I8X16, types::I16X8) => {
                                // i16x8.extmul_high_i8x16_u
                                let tmp_reg = ctx.alloc_tmp(types::I16X8).only_reg().unwrap();
                                ctx.emit(Inst::gen_move(tmp_reg, lhs, output_ty));
                                ctx.emit(Inst::xmm_rm_r_imm(
                                    SseOpcode::Palignr,
                                    RegMem::reg(lhs),
                                    tmp_reg,
                                    8,
                                    OperandSize::Size32,
                                ));
                                ctx.emit(Inst::xmm_mov(
                                    SseOpcode::Pmovzxbw,
                                    RegMem::reg(tmp_reg.to_reg()),
                                    tmp_reg,
                                ));
                                ctx.emit(Inst::gen_move(dst, rhs, output_ty));
                                ctx.emit(Inst::xmm_rm_r_imm(
                                    SseOpcode::Palignr,
                                    RegMem::reg(rhs),
                                    dst,
                                    8,
                                    OperandSize::Size32,
                                ));
                                ctx.emit(Inst::xmm_mov(
                                    SseOpcode::Pmovzxbw,
                                    RegMem::reg(dst.to_reg()),
                                    dst,
                                ));
                                ctx.emit(Inst::xmm_rm_r(
                                    SseOpcode::Pmullw,
                                    RegMem::reg(tmp_reg.to_reg()),
                                    dst,
                                ));
                            }
                            (types::I16X8, types::I16X8, types::I32X4) => {
                                // i32x4.extmul_high_i16x8_u
                                ctx.emit(Inst::gen_move(dst, lhs, input0_ty));
                                let tmp_reg = ctx.alloc_tmp(types::I16X8).only_reg().unwrap();
                                ctx.emit(Inst::gen_move(tmp_reg, lhs, input0_ty));
                                ctx.emit(Inst::xmm_rm_r(SseOpcode::Pmullw, RegMem::reg(rhs), dst));
                                ctx.emit(Inst::xmm_rm_r(
                                    SseOpcode::Pmulhuw,
                                    RegMem::reg(rhs),
                                    tmp_reg,
                                ));
                                ctx.emit(Inst::xmm_rm_r(
                                    SseOpcode::Punpckhwd,
                                    RegMem::from(tmp_reg),
                                    dst,
                                ));
                            }
                            (types::I32X4, types::I32X4, types::I64X2) => {
                                // i64x2.extmul_high_i32x4_u
                                let tmp_reg = ctx.alloc_tmp(types::I32X4).only_reg().unwrap();
                                ctx.emit(Inst::xmm_rm_r_imm(
                                    SseOpcode::Pshufd,
                                    RegMem::reg(lhs),
                                    tmp_reg,
                                    0xFA,
                                    OperandSize::Size32,
                                ));
                                ctx.emit(Inst::xmm_rm_r_imm(
                                    SseOpcode::Pshufd,
                                    RegMem::reg(rhs),
                                    dst,
                                    0xFA,
                                    OperandSize::Size32,
                                ));
                                ctx.emit(Inst::xmm_rm_r(
                                    SseOpcode::Pmuludq,
                                    RegMem::reg(tmp_reg.to_reg()),
                                    dst,
                                ));
                            }
                            // Note uwiden_high only allows types: I8X16, I16X8, and I32X4
                            _ => panic!("Unsupported extmul_high_unsigned type"),
                        }
                    }
                } else if let Some(uwiden0_low) = matches_input(ctx, inputs[0], Opcode::UwidenLow) {
                    if let Some(uwiden1_low) = matches_input(ctx, inputs[1], Opcode::UwidenLow) {
                        let uwiden_input = &[
                            InsnInput {
                                insn: uwiden0_low,
                                input: 0,
                            },
                            InsnInput {
                                insn: uwiden1_low,
                                input: 0,
                            },
                        ];

                        let input0_ty = ctx.input_ty(uwiden0_low, 0);
                        let input1_ty = ctx.input_ty(uwiden1_low, 0);
                        let output_ty = ctx.output_ty(insn, 0);
                        let lhs = put_input_in_reg(ctx, uwiden_input[0]);
                        let rhs = put_input_in_reg(ctx, uwiden_input[1]);
                        let dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();

                        match (input0_ty, input1_ty, output_ty) {
                            (types::I8X16, types::I8X16, types::I16X8) => {
                                // i16x8.extmul_low_i8x16_u
                                let tmp_reg = ctx.alloc_tmp(types::I16X8).only_reg().unwrap();
                                ctx.emit(Inst::xmm_mov(
                                    SseOpcode::Pmovzxbw,
                                    RegMem::reg(lhs),
                                    tmp_reg,
                                ));
                                ctx.emit(Inst::xmm_mov(SseOpcode::Pmovzxbw, RegMem::reg(rhs), dst));
                                ctx.emit(Inst::xmm_rm_r(
                                    SseOpcode::Pmullw,
                                    RegMem::reg(tmp_reg.to_reg()),
                                    dst,
                                ));
                            }
                            (types::I16X8, types::I16X8, types::I32X4) => {
                                // i32x4.extmul_low_i16x8_u
                                ctx.emit(Inst::gen_move(dst, lhs, input0_ty));
                                let tmp_reg = ctx.alloc_tmp(types::I16X8).only_reg().unwrap();
                                ctx.emit(Inst::gen_move(tmp_reg, lhs, input0_ty));
                                ctx.emit(Inst::xmm_rm_r(SseOpcode::Pmullw, RegMem::reg(rhs), dst));
                                ctx.emit(Inst::xmm_rm_r(
                                    SseOpcode::Pmulhuw,
                                    RegMem::reg(rhs),
                                    tmp_reg,
                                ));
                                ctx.emit(Inst::xmm_rm_r(
                                    SseOpcode::Punpcklwd,
                                    RegMem::from(tmp_reg),
                                    dst,
                                ));
                            }
                            (types::I32X4, types::I32X4, types::I64X2) => {
                                // i64x2.extmul_low_i32x4_u
                                let tmp_reg = ctx.alloc_tmp(types::I32X4).only_reg().unwrap();
                                ctx.emit(Inst::xmm_rm_r_imm(
                                    SseOpcode::Pshufd,
                                    RegMem::reg(lhs),
                                    tmp_reg,
                                    0x50,
                                    OperandSize::Size32,
                                ));
                                ctx.emit(Inst::xmm_rm_r_imm(
                                    SseOpcode::Pshufd,
                                    RegMem::reg(rhs),
                                    dst,
                                    0x50,
                                    OperandSize::Size32,
                                ));
                                ctx.emit(Inst::xmm_rm_r(
                                    SseOpcode::Pmuludq,
                                    RegMem::reg(tmp_reg.to_reg()),
                                    dst,
                                ));
                            }
                            // Note uwiden_low only allows types: I8X16, I16X8, and I32X4
                            _ => panic!("Unsupported extmul_low_unsigned type"),
                        }
                    }
                } else {
                    panic!("Unsupported imul operation for type: {}", ty);
                }
            } else if ty == types::I64X2 {
                // Eventually one of these should be `input_to_reg_mem` (TODO).
                let lhs = put_input_in_reg(ctx, inputs[0]);
                let rhs = put_input_in_reg(ctx, inputs[1]);
                let dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();

                if isa_flags.use_avx512vl_simd() && isa_flags.use_avx512dq_simd() {
                    // With the right AVX512 features (VL + DQ) this operation
                    // can lower to a single operation.
                    ctx.emit(Inst::xmm_rm_r_evex(
                        Avx512Opcode::Vpmullq,
                        RegMem::reg(rhs),
                        lhs,
                        dst,
                    ));
                } else {
                    // Otherwise, for I64X2 multiplication we describe a lane A as being
                    // composed of a 32-bit upper half "Ah" and a 32-bit lower half
                    // "Al". The 32-bit long hand multiplication can then be written
                    // as:
                    //    Ah Al
                    // *  Bh Bl
                    //    -----
                    //    Al * Bl
                    // + (Ah * Bl) << 32
                    // + (Al * Bh) << 32
                    //
                    // So for each lane we will compute:
                    //   A * B  = (Al * Bl) + ((Ah * Bl) + (Al * Bh)) << 32
                    //
                    // Note, the algorithm will use pmuldq which operates directly
                    // on the lower 32-bit (Al or Bl) of a lane and writes the
                    // result to the full 64-bits of the lane of the destination.
                    // For this reason we don't need shifts to isolate the lower
                    // 32-bits, however, we will need to use shifts to isolate the
                    // high 32-bits when doing calculations, i.e., Ah == A >> 32.
                    //
                    // The full sequence then is as follows:
                    //   A' = A
                    //   A' = A' >> 32
                    //   A' = Ah' * Bl
                    //   B' = B
                    //   B' = B' >> 32
                    //   B' = Bh' * Al
                    //   B' = B' + A'
                    //   B' = B' << 32
                    //   A' = A
                    //   A' = Al' * Bl
                    //   A' = A' + B'
                    //   dst = A'

                    // A' = A
                    let rhs_1 = ctx.alloc_tmp(types::I64X2).only_reg().unwrap();
                    ctx.emit(Inst::gen_move(rhs_1, rhs, ty));

                    // A' = A' >> 32
                    // A' = Ah' * Bl
                    ctx.emit(Inst::xmm_rmi_reg(
                        SseOpcode::Psrlq,
                        RegMemImm::imm(32),
                        rhs_1,
                    ));
                    ctx.emit(Inst::xmm_rm_r(
                        SseOpcode::Pmuludq,
                        RegMem::reg(lhs.clone()),
                        rhs_1,
                    ));

                    // B' = B
                    let lhs_1 = ctx.alloc_tmp(types::I64X2).only_reg().unwrap();
                    ctx.emit(Inst::gen_move(lhs_1, lhs, ty));

                    // B' = B' >> 32
                    // B' = Bh' * Al
                    ctx.emit(Inst::xmm_rmi_reg(
                        SseOpcode::Psrlq,
                        RegMemImm::imm(32),
                        lhs_1,
                    ));
                    ctx.emit(Inst::xmm_rm_r(SseOpcode::Pmuludq, RegMem::reg(rhs), lhs_1));

                    // B' = B' + A'
                    // B' = B' << 32
                    ctx.emit(Inst::xmm_rm_r(
                        SseOpcode::Paddq,
                        RegMem::reg(rhs_1.to_reg()),
                        lhs_1,
                    ));
                    ctx.emit(Inst::xmm_rmi_reg(
                        SseOpcode::Psllq,
                        RegMemImm::imm(32),
                        lhs_1,
                    ));

                    // A' = A
                    // A' = Al' * Bl
                    // A' = A' + B'
                    // dst = A'
                    ctx.emit(Inst::gen_move(rhs_1, rhs, ty));
                    ctx.emit(Inst::xmm_rm_r(
                        SseOpcode::Pmuludq,
                        RegMem::reg(lhs.clone()),
                        rhs_1,
                    ));
                    ctx.emit(Inst::xmm_rm_r(
                        SseOpcode::Paddq,
                        RegMem::reg(lhs_1.to_reg()),
                        rhs_1,
                    ));
                    ctx.emit(Inst::gen_move(dst, rhs_1.to_reg(), ty));
                }
            } else if ty.lane_count() > 1 {
                // Emit single instruction lowerings for the remaining vector
                // multiplications.
                let sse_op = match ty {
                    types::I16X8 => SseOpcode::Pmullw,
                    types::I32X4 => SseOpcode::Pmulld,
                    _ => panic!("Unsupported type for packed imul instruction: {}", ty),
                };
                let lhs = put_input_in_reg(ctx, inputs[0]);
                let rhs = input_to_reg_mem(ctx, inputs[1]);
                let dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();

                // Move the `lhs` to the same register as `dst`.
                ctx.emit(Inst::gen_move(dst, lhs, ty));
                ctx.emit(Inst::xmm_rm_r(sse_op, rhs, dst));
            } else if ty == types::I128 || ty == types::B128 {
                // Handle 128-bit multiplications.
                let lhs = put_input_in_regs(ctx, inputs[0]);
                let rhs = put_input_in_regs(ctx, inputs[1]);
                let dst = get_output_reg(ctx, outputs[0]);
                assert_eq!(lhs.len(), 2);
                assert_eq!(rhs.len(), 2);
                assert_eq!(dst.len(), 2);

                // mul:
                //   dst_lo = lhs_lo * rhs_lo
                //   dst_hi = umulhi(lhs_lo, rhs_lo) + lhs_lo * rhs_hi + lhs_hi * rhs_lo
                //
                // so we emit:
                //   mov dst_lo, lhs_lo
                //   mul dst_lo, rhs_lo
                //   mov dst_hi, lhs_lo
                //   mul dst_hi, rhs_hi
                //   mov tmp, lhs_hi
                //   mul tmp, rhs_lo
                //   add dst_hi, tmp
                //   mov rax, lhs_lo
                //   umulhi rhs_lo  // implicit rax arg/dst
                //   add dst_hi, rax
                let tmp = ctx.alloc_tmp(types::I64).only_reg().unwrap();
                ctx.emit(Inst::gen_move(dst.regs()[0], lhs.regs()[0], types::I64));
                ctx.emit(Inst::alu_rmi_r(
                    OperandSize::Size64,
                    AluRmiROpcode::Mul,
                    RegMemImm::reg(rhs.regs()[0]),
                    dst.regs()[0],
                ));
                ctx.emit(Inst::gen_move(dst.regs()[1], lhs.regs()[0], types::I64));
                ctx.emit(Inst::alu_rmi_r(
                    OperandSize::Size64,
                    AluRmiROpcode::Mul,
                    RegMemImm::reg(rhs.regs()[1]),
                    dst.regs()[1],
                ));
                ctx.emit(Inst::gen_move(tmp, lhs.regs()[1], types::I64));
                ctx.emit(Inst::alu_rmi_r(
                    OperandSize::Size64,
                    AluRmiROpcode::Mul,
                    RegMemImm::reg(rhs.regs()[0]),
                    tmp,
                ));
                ctx.emit(Inst::alu_rmi_r(
                    OperandSize::Size64,
                    AluRmiROpcode::Add,
                    RegMemImm::reg(tmp.to_reg()),
                    dst.regs()[1],
                ));
                ctx.emit(Inst::gen_move(
                    Writable::from_reg(regs::rax()),
                    lhs.regs()[0],
                    types::I64,
                ));
                ctx.emit(Inst::mul_hi(
                    OperandSize::Size64,
                    /* signed = */ false,
                    RegMem::reg(rhs.regs()[0]),
                ));
                ctx.emit(Inst::alu_rmi_r(
                    OperandSize::Size64,
                    AluRmiROpcode::Add,
                    RegMemImm::reg(regs::rdx()),
                    dst.regs()[1],
                ));
            } else {
                let size = if ty == types::I64 {
                    OperandSize::Size64
                } else {
                    OperandSize::Size32
                };
                let alu_op = AluRmiROpcode::Mul;

                // For commutative operations, try to commute operands if one is
                // an immediate or direct memory reference. Do so by converting
                // LHS to RMI; if reg, then always convert RHS to RMI; else, use
                // LHS as RMI and convert RHS to reg.
                let lhs = input_to_reg_mem_imm(ctx, inputs[0]);
                let (lhs, rhs) = if let RegMemImm::Reg { reg: lhs_reg } = lhs {
                    let rhs = input_to_reg_mem_imm(ctx, inputs[1]);
                    (lhs_reg, rhs)
                } else {
                    let rhs_reg = put_input_in_reg(ctx, inputs[1]);
                    (rhs_reg, lhs)
                };

                let dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
                ctx.emit(Inst::mov_r_r(OperandSize::Size64, lhs, dst));
                ctx.emit(Inst::alu_rmi_r(size, alu_op, rhs, dst));
            }
        }

        Opcode::BandNot => {
            let ty = ty.unwrap();
            debug_assert!(ty.is_vector() && ty.bytes() == 16);
            let lhs = input_to_reg_mem(ctx, inputs[0]);
            let rhs = put_input_in_reg(ctx, inputs[1]);
            let dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let sse_op = match ty {
                types::F32X4 => SseOpcode::Andnps,
                types::F64X2 => SseOpcode::Andnpd,
                _ => SseOpcode::Pandn,
            };
            // Note the flipping of operands: the `rhs` operand is used as the destination instead
            // of the `lhs` as in the other bit operations above (e.g. `band`).
            ctx.emit(Inst::gen_move(dst, rhs, ty));
            ctx.emit(Inst::xmm_rm_r(sse_op, lhs, dst));
        }

        Opcode::Iabs => {
            let src = input_to_reg_mem(ctx, inputs[0]);
            let dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let ty = ty.unwrap();
            if ty == types::I64X2 {
                if isa_flags.use_avx512vl_simd() && isa_flags.use_avx512f_simd() {
                    ctx.emit(Inst::xmm_unary_rm_r_evex(Avx512Opcode::Vpabsq, src, dst));
                } else {
                    // If `VPABSQ` from AVX512 is unavailable, we use a separate register, `tmp`, to
                    // contain the results of `0 - src` and then blend in those results with
                    // `BLENDVPD` if the MSB of `tmp` was set to 1 (i.e. if `tmp` was negative or,
                    // conversely, if `src` was originally positive).

                    // Emit all 0s into the `tmp` register.
                    let tmp = ctx.alloc_tmp(ty).only_reg().unwrap();
                    ctx.emit(Inst::xmm_rm_r(SseOpcode::Pxor, RegMem::from(tmp), tmp));
                    // Subtract the lanes from 0 and set up `dst`.
                    ctx.emit(Inst::xmm_rm_r(SseOpcode::Psubq, src.clone(), tmp));
                    ctx.emit(Inst::gen_move(dst, tmp.to_reg(), ty));
                    // Choose the subtracted lanes when `tmp` has an MSB of 1. BLENDVPD's semantics
                    // require the "choice" mask to be in XMM0.
                    ctx.emit(Inst::gen_move(
                        Writable::from_reg(regs::xmm0()),
                        tmp.to_reg(),
                        ty,
                    ));
                    ctx.emit(Inst::xmm_rm_r(SseOpcode::Blendvpd, src, dst));
                }
            } else if ty.is_vector() {
                let opcode = match ty {
                    types::I8X16 => SseOpcode::Pabsb,
                    types::I16X8 => SseOpcode::Pabsw,
                    types::I32X4 => SseOpcode::Pabsd,
                    _ => panic!("Unsupported type for packed iabs instruction: {}", ty),
                };
                ctx.emit(Inst::xmm_unary_rm_r(opcode, src, dst));
            } else {
                unimplemented!("iabs is unimplemented for non-vector type: {}", ty);
            }
        }

        Opcode::Imax | Opcode::Umax | Opcode::Imin | Opcode::Umin => {
            let lhs = put_input_in_reg(ctx, inputs[0]);
            let rhs = input_to_reg_mem(ctx, inputs[1]);
            let dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let ty = ty.unwrap();
            if ty.is_vector() {
                let sse_op = match op {
                    Opcode::Imax => match ty {
                        types::I8X16 => SseOpcode::Pmaxsb,
                        types::I16X8 => SseOpcode::Pmaxsw,
                        types::I32X4 => SseOpcode::Pmaxsd,
                        _ => panic!("Unsupported type for packed {} instruction: {}", op, ty),
                    },
                    Opcode::Umax => match ty {
                        types::I8X16 => SseOpcode::Pmaxub,
                        types::I16X8 => SseOpcode::Pmaxuw,
                        types::I32X4 => SseOpcode::Pmaxud,
                        _ => panic!("Unsupported type for packed {} instruction: {}", op, ty),
                    },
                    Opcode::Imin => match ty {
                        types::I8X16 => SseOpcode::Pminsb,
                        types::I16X8 => SseOpcode::Pminsw,
                        types::I32X4 => SseOpcode::Pminsd,
                        _ => panic!("Unsupported type for packed {} instruction: {}", op, ty),
                    },
                    Opcode::Umin => match ty {
                        types::I8X16 => SseOpcode::Pminub,
                        types::I16X8 => SseOpcode::Pminuw,
                        types::I32X4 => SseOpcode::Pminud,
                        _ => panic!("Unsupported type for packed {} instruction: {}", op, ty),
                    },
                    _ => unreachable!("This is a bug: the external and internal `match op` should be over the same opcodes."),
                };

                // Move the `lhs` to the same register as `dst`.
                ctx.emit(Inst::gen_move(dst, lhs, ty));
                ctx.emit(Inst::xmm_rm_r(sse_op, rhs, dst));
            } else {
                panic!("Unsupported type for {} instruction: {}", op, ty);
            }
        }

        Opcode::Bnot => {
            let ty = ty.unwrap();

            if ty.is_vector() {
                let src = put_input_in_reg(ctx, inputs[0]);
                let dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
                ctx.emit(Inst::gen_move(dst, src, ty));
                let tmp = ctx.alloc_tmp(ty).only_reg().unwrap();
                ctx.emit(Inst::equals(ty, RegMem::from(tmp), tmp));
                ctx.emit(Inst::xor(ty, RegMem::from(tmp), dst));
            } else if ty == types::I128 || ty == types::B128 {
                let src = put_input_in_regs(ctx, inputs[0]);
                let dst = get_output_reg(ctx, outputs[0]);
                ctx.emit(Inst::gen_move(dst.regs()[0], src.regs()[0], types::I64));
                ctx.emit(Inst::not(OperandSize::Size64, dst.regs()[0]));
                ctx.emit(Inst::gen_move(dst.regs()[1], src.regs()[1], types::I64));
                ctx.emit(Inst::not(OperandSize::Size64, dst.regs()[1]));
            } else if ty.is_bool() {
                unimplemented!("bool bnot")
            } else {
                let src = put_input_in_reg(ctx, inputs[0]);
                let dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
                ctx.emit(Inst::gen_move(dst, src, ty));
                ctx.emit(Inst::not(OperandSize::from_ty(ty), dst));
            }
        }

        Opcode::Bitselect => {
            let ty = ty.unwrap();
            let condition = put_input_in_reg(ctx, inputs[0]);
            let if_true = put_input_in_reg(ctx, inputs[1]);
            let if_false = input_to_reg_mem(ctx, inputs[2]);
            let dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();

            if ty.is_vector() {
                let tmp1 = ctx.alloc_tmp(ty).only_reg().unwrap();
                ctx.emit(Inst::gen_move(tmp1, if_true, ty));
                ctx.emit(Inst::and(ty, RegMem::reg(condition.clone()), tmp1));

                let tmp2 = ctx.alloc_tmp(ty).only_reg().unwrap();
                ctx.emit(Inst::gen_move(tmp2, condition, ty));
                ctx.emit(Inst::and_not(ty, if_false, tmp2));

                ctx.emit(Inst::gen_move(dst, tmp2.to_reg(), ty));
                ctx.emit(Inst::or(ty, RegMem::from(tmp1), dst));
            } else {
                unimplemented!("no lowering for scalar bitselect instruction")
            }
        }

        Opcode::Vselect => {
            let ty = ty.unwrap();
            let condition = put_input_in_reg(ctx, inputs[0]);
            let condition_ty = ctx.input_ty(insn, 0);
            let if_true = input_to_reg_mem(ctx, inputs[1]);
            let if_false = put_input_in_reg(ctx, inputs[2]);
            let dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();

            if ty.is_vector() {
                // `vselect` relies on the bit representation of the condition:
                // vector boolean types are defined in Cranelift to be all 1s or
                // all 0s. This lowering relies on that fact to use x86's
                // variable blend instructions, which look at the _high_bit_ of
                // the condition mask. All the bits of vector booleans will
                // match (all 1s or all 0s), so we can just use the high bit.
                assert!(condition_ty.lane_type().is_bool());

                // Variable blend instructions expect the condition mask to be
                // in XMM0.
                let xmm0 = Writable::from_reg(regs::xmm0());
                ctx.emit(Inst::gen_move(xmm0, condition, ty));

                // Match up the source and destination registers for regalloc.
                ctx.emit(Inst::gen_move(dst, if_false, ty));

                // Technically PBLENDVB would work in all cases (since the bytes
                // inside the mask will be all 1s or 0s we can blend
                // byte-by-byte instead of word-by-word, e.g.) but
                // type-specialized versions are included here for clarity when
                // troubleshooting and due to slight improvements in
                // latency/throughput on certain processor families.
                let opcode = match condition_ty {
                    types::B64X2 => SseOpcode::Blendvpd,
                    types::B32X4 => SseOpcode::Blendvps,
                    types::B16X8 | types::B8X16 => SseOpcode::Pblendvb,
                    _ => unimplemented!("unable lower vselect for type: {}", condition_ty),
                };
                ctx.emit(Inst::xmm_rm_r(opcode, if_true, dst));
            } else {
                unimplemented!("no lowering for scalar vselect instruction")
            }
        }

        Opcode::Ishl | Opcode::Ushr | Opcode::Sshr | Opcode::Rotl | Opcode::Rotr => {
            let dst_ty = ctx.output_ty(insn, 0);
            debug_assert_eq!(ctx.input_ty(insn, 0), dst_ty);

            if !dst_ty.is_vector() && dst_ty.bits() <= 64 {
                // Scalar shifts on x86 have various encodings:
                // - shift by one bit, e.g. `SAL r/m8, 1` (not used here)
                // - shift by an immediate amount, e.g. `SAL r/m8, imm8`
                // - shift by a dynamic amount but only from the CL register, e.g. `SAL r/m8, CL`.
                // This implementation uses the last two encoding methods.
                let (size, lhs) = match dst_ty {
                    types::I8 | types::I16 => match op {
                        Opcode::Ishl => (OperandSize::Size32, put_input_in_reg(ctx, inputs[0])),
                        Opcode::Ushr => (
                            OperandSize::Size32,
                            extend_input_to_reg(ctx, inputs[0], ExtSpec::ZeroExtendTo32),
                        ),
                        Opcode::Sshr => (
                            OperandSize::Size32,
                            extend_input_to_reg(ctx, inputs[0], ExtSpec::SignExtendTo32),
                        ),
                        Opcode::Rotl | Opcode::Rotr => (
                            OperandSize::from_ty(dst_ty),
                            put_input_in_reg(ctx, inputs[0]),
                        ),
                        _ => unreachable!(),
                    },
                    types::I32 | types::I64 => (
                        OperandSize::from_ty(dst_ty),
                        put_input_in_reg(ctx, inputs[0]),
                    ),
                    _ => unreachable!("unhandled output type for shift/rotates: {}", dst_ty),
                };

                let (count, rhs) =
                    if let Some(cst) = ctx.get_input_as_source_or_const(insn, 1).constant {
                        // Mask count, according to Cranelift's semantics.
                        let cst = (cst as u8) & (dst_ty.bits() as u8 - 1);
                        (Some(cst), None)
                    } else {
                        // We can ignore upper registers if shift amount is multi-reg, because we
                        // are taking the shift amount mod 2^(lhs_width) anyway.
                        (None, Some(put_input_in_regs(ctx, inputs[1]).regs()[0]))
                    };

                let dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();

                let shift_kind = match op {
                    Opcode::Ishl => ShiftKind::ShiftLeft,
                    Opcode::Ushr => ShiftKind::ShiftRightLogical,
                    Opcode::Sshr => ShiftKind::ShiftRightArithmetic,
                    Opcode::Rotl => ShiftKind::RotateLeft,
                    Opcode::Rotr => ShiftKind::RotateRight,
                    _ => unreachable!(),
                };

                let w_rcx = Writable::from_reg(regs::rcx());
                ctx.emit(Inst::mov_r_r(OperandSize::Size64, lhs, dst));
                if count.is_none() {
                    ctx.emit(Inst::mov_r_r(OperandSize::Size64, rhs.unwrap(), w_rcx));
                }
                ctx.emit(Inst::shift_r(size, shift_kind, count, dst));
            } else if dst_ty == types::I128 {
                let amt_src = put_input_in_regs(ctx, inputs[1]).regs()[0];
                let src = put_input_in_regs(ctx, inputs[0]);
                let dst = get_output_reg(ctx, outputs[0]);

                match op {
                    Opcode::Ishl => {
                        emit_shl_i128(ctx, src, dst, amt_src);
                    }
                    Opcode::Ushr => {
                        emit_shr_i128(ctx, src, dst, amt_src, /* is_signed = */ false);
                    }
                    Opcode::Sshr => {
                        emit_shr_i128(ctx, src, dst, amt_src, /* is_signed = */ true);
                    }
                    Opcode::Rotl => {
                        // (mov tmp, src)
                        // (shl.i128 tmp, amt)
                        // (mov dst, src)
                        // (ushr.i128 dst, 128-amt)
                        // (or dst, tmp)
                        let tmp = ctx.alloc_tmp(types::I128);
                        emit_shl_i128(ctx, src, tmp, amt_src);
                        let inv_amt = ctx.alloc_tmp(types::I64).only_reg().unwrap();
                        ctx.emit(Inst::imm(OperandSize::Size64, 128, inv_amt));
                        ctx.emit(Inst::alu_rmi_r(
                            OperandSize::Size64,
                            AluRmiROpcode::Sub,
                            RegMemImm::reg(amt_src),
                            inv_amt,
                        ));
                        emit_shr_i128(
                            ctx,
                            src,
                            dst,
                            inv_amt.to_reg(),
                            /* is_signed = */ false,
                        );
                        ctx.emit(Inst::alu_rmi_r(
                            OperandSize::Size64,
                            AluRmiROpcode::Or,
                            RegMemImm::reg(tmp.regs()[0].to_reg()),
                            dst.regs()[0],
                        ));
                        ctx.emit(Inst::alu_rmi_r(
                            OperandSize::Size64,
                            AluRmiROpcode::Or,
                            RegMemImm::reg(tmp.regs()[1].to_reg()),
                            dst.regs()[1],
                        ));
                    }
                    Opcode::Rotr => {
                        // (mov tmp, src)
                        // (ushr.i128 tmp, amt)
                        // (mov dst, src)
                        // (shl.i128 dst, 128-amt)
                        // (or dst, tmp)
                        let tmp = ctx.alloc_tmp(types::I128);
                        emit_shr_i128(ctx, src, tmp, amt_src, /* is_signed = */ false);
                        let inv_amt = ctx.alloc_tmp(types::I64).only_reg().unwrap();
                        ctx.emit(Inst::imm(OperandSize::Size64, 128, inv_amt));
                        ctx.emit(Inst::alu_rmi_r(
                            OperandSize::Size64,
                            AluRmiROpcode::Sub,
                            RegMemImm::reg(amt_src),
                            inv_amt,
                        ));
                        emit_shl_i128(ctx, src, dst, inv_amt.to_reg());
                        ctx.emit(Inst::alu_rmi_r(
                            OperandSize::Size64,
                            AluRmiROpcode::Or,
                            RegMemImm::reg(tmp.regs()[0].to_reg()),
                            dst.regs()[0],
                        ));
                        ctx.emit(Inst::alu_rmi_r(
                            OperandSize::Size64,
                            AluRmiROpcode::Or,
                            RegMemImm::reg(tmp.regs()[1].to_reg()),
                            dst.regs()[1],
                        ));
                    }
                    _ => unreachable!(),
                }
            } else if dst_ty == types::I8X16 && (op == Opcode::Ishl || op == Opcode::Ushr) {
                // Since the x86 instruction set does not have any 8x16 shift instructions (even in higher feature sets
                // like AVX), we lower the `ishl.i8x16` and `ushr.i8x16` to a sequence of instructions. The basic idea,
                // whether the `shift_by` amount is an immediate or not, is to use a 16x8 shift and then mask off the
                // incorrect bits to 0s (see below for handling signs in `sshr.i8x16`).
                let src = put_input_in_reg(ctx, inputs[0]);
                let shift_by = input_to_reg_mem_imm(ctx, inputs[1]);
                let dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();

                // If necessary, move the shift index into the lowest bits of a vector register.
                let shift_by_moved = match &shift_by {
                    RegMemImm::Imm { .. } => shift_by.clone(),
                    RegMemImm::Reg { reg } => {
                        let tmp_shift_by = ctx.alloc_tmp(dst_ty).only_reg().unwrap();
                        ctx.emit(Inst::gpr_to_xmm(
                            SseOpcode::Movd,
                            RegMem::reg(*reg),
                            OperandSize::Size32,
                            tmp_shift_by,
                        ));
                        RegMemImm::reg(tmp_shift_by.to_reg())
                    }
                    RegMemImm::Mem { .. } => unimplemented!("load shift amount to XMM register"),
                };

                // Shift `src` using 16x8. Unfortunately, a 16x8 shift will only be correct for half of the lanes;
                // the others must be fixed up with the mask below.
                let shift_opcode = match op {
                    Opcode::Ishl => SseOpcode::Psllw,
                    Opcode::Ushr => SseOpcode::Psrlw,
                    _ => unimplemented!("{} is not implemented for type {}", op, dst_ty),
                };
                ctx.emit(Inst::gen_move(dst, src, dst_ty));
                ctx.emit(Inst::xmm_rmi_reg(shift_opcode, shift_by_moved, dst));

                // Choose which mask to use to fixup the shifted lanes. Since we must use a 16x8 shift, we need to fix
                // up the bits that migrate from one half of the lane to the other. Each 16-byte mask (which rustfmt
                // forces to multiple lines) is indexed by the shift amount: e.g. if we shift right by 0 (no movement),
                // we want to retain all the bits so we mask with `0xff`; if we shift right by 1, we want to retain all
                // bits except the MSB so we mask with `0x7f`; etc.
                const USHR_MASKS: [u8; 128] = [
                    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
                    0xff, 0xff, 0xff, 0x7f, 0x7f, 0x7f, 0x7f, 0x7f, 0x7f, 0x7f, 0x7f, 0x7f, 0x7f,
                    0x7f, 0x7f, 0x7f, 0x7f, 0x7f, 0x7f, 0x3f, 0x3f, 0x3f, 0x3f, 0x3f, 0x3f, 0x3f,
                    0x3f, 0x3f, 0x3f, 0x3f, 0x3f, 0x3f, 0x3f, 0x3f, 0x3f, 0x1f, 0x1f, 0x1f, 0x1f,
                    0x1f, 0x1f, 0x1f, 0x1f, 0x1f, 0x1f, 0x1f, 0x1f, 0x1f, 0x1f, 0x1f, 0x1f, 0x0f,
                    0x0f, 0x0f, 0x0f, 0x0f, 0x0f, 0x0f, 0x0f, 0x0f, 0x0f, 0x0f, 0x0f, 0x0f, 0x0f,
                    0x0f, 0x0f, 0x07, 0x07, 0x07, 0x07, 0x07, 0x07, 0x07, 0x07, 0x07, 0x07, 0x07,
                    0x07, 0x07, 0x07, 0x07, 0x07, 0x03, 0x03, 0x03, 0x03, 0x03, 0x03, 0x03, 0x03,
                    0x03, 0x03, 0x03, 0x03, 0x03, 0x03, 0x03, 0x03, 0x01, 0x01, 0x01, 0x01, 0x01,
                    0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01,
                ];
                const SHL_MASKS: [u8; 128] = [
                    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
                    0xff, 0xff, 0xff, 0xfe, 0xfe, 0xfe, 0xfe, 0xfe, 0xfe, 0xfe, 0xfe, 0xfe, 0xfe,
                    0xfe, 0xfe, 0xfe, 0xfe, 0xfe, 0xfe, 0xfc, 0xfc, 0xfc, 0xfc, 0xfc, 0xfc, 0xfc,
                    0xfc, 0xfc, 0xfc, 0xfc, 0xfc, 0xfc, 0xfc, 0xfc, 0xfc, 0xf8, 0xf8, 0xf8, 0xf8,
                    0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf8, 0xf0,
                    0xf0, 0xf0, 0xf0, 0xf0, 0xf0, 0xf0, 0xf0, 0xf0, 0xf0, 0xf0, 0xf0, 0xf0, 0xf0,
                    0xf0, 0xf0, 0xe0, 0xe0, 0xe0, 0xe0, 0xe0, 0xe0, 0xe0, 0xe0, 0xe0, 0xe0, 0xe0,
                    0xe0, 0xe0, 0xe0, 0xe0, 0xe0, 0xc0, 0xc0, 0xc0, 0xc0, 0xc0, 0xc0, 0xc0, 0xc0,
                    0xc0, 0xc0, 0xc0, 0xc0, 0xc0, 0xc0, 0xc0, 0xc0, 0x80, 0x80, 0x80, 0x80, 0x80,
                    0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80,
                ];
                let mask = match op {
                    Opcode::Ishl => &SHL_MASKS,
                    Opcode::Ushr => &USHR_MASKS,
                    _ => unimplemented!("{} is not implemented for type {}", op, dst_ty),
                };

                // Figure out the address of the shift mask.
                let mask_address = match shift_by {
                    RegMemImm::Imm { simm32 } => {
                        // When the shift amount is known, we can statically (i.e. at compile time) determine the mask to
                        // use and only emit that.
                        debug_assert!(simm32 < 8);
                        let mask_offset = simm32 as usize * 16;
                        let mask_constant = ctx.use_constant(VCodeConstantData::WellKnown(
                            &mask[mask_offset..mask_offset + 16],
                        ));
                        SyntheticAmode::ConstantOffset(mask_constant)
                    }
                    RegMemImm::Reg { reg } => {
                        // Otherwise, we must emit the entire mask table and dynamically (i.e. at run time) find the correct
                        // mask offset in the table. We do this use LEA to find the base address of the mask table and then
                        // complex addressing to offset to the right mask: `base_address + shift_by * 4`
                        let base_mask_address = ctx.alloc_tmp(types::I64).only_reg().unwrap();
                        let mask_offset = ctx.alloc_tmp(types::I64).only_reg().unwrap();
                        let mask_constant = ctx.use_constant(VCodeConstantData::WellKnown(mask));
                        ctx.emit(Inst::lea(
                            SyntheticAmode::ConstantOffset(mask_constant),
                            base_mask_address,
                        ));
                        ctx.emit(Inst::gen_move(mask_offset, reg, types::I64));
                        ctx.emit(Inst::shift_r(
                            OperandSize::Size64,
                            ShiftKind::ShiftLeft,
                            Some(4),
                            mask_offset,
                        ));
                        Amode::imm_reg_reg_shift(
                            0,
                            base_mask_address.to_reg(),
                            mask_offset.to_reg(),
                            0,
                        )
                        .into()
                    }
                    RegMemImm::Mem { addr: _ } => unimplemented!("load mask address"),
                };

                // Load the mask into a temporary register, `mask_value`.
                let mask_value = ctx.alloc_tmp(dst_ty).only_reg().unwrap();
                ctx.emit(Inst::load(dst_ty, mask_address, mask_value, ExtKind::None));

                // Remove the bits that would have disappeared in a true 8x16 shift. TODO in the future,
                // this AND instruction could be coalesced with the load above.
                let sse_op = match dst_ty {
                    types::F32X4 => SseOpcode::Andps,
                    types::F64X2 => SseOpcode::Andpd,
                    _ => SseOpcode::Pand,
                };
                ctx.emit(Inst::xmm_rm_r(sse_op, RegMem::from(mask_value), dst));
            } else if dst_ty == types::I8X16 && op == Opcode::Sshr {
                // Since the x86 instruction set does not have an 8x16 shift instruction and the approach used for
                // `ishl` and `ushr` cannot be easily used (the masks do not preserve the sign), we use a different
                // approach here: separate the low and high lanes, shift them separately, and merge them into the final
                // result. Visually, this looks like the following, where `src.i8x16 = [s0, s1, ..., s15]:
                //   low.i16x8 = [(s0, s0), (s1, s1), ..., (s7, s7)]
                //   shifted_low.i16x8 = shift each lane of `low`
                //   high.i16x8 = [(s8, s8), (s9, s9), ..., (s15, s15)]
                //   shifted_high.i16x8 = shift each lane of `high`
                //   dst.i8x16 = [s0'', s1'', ..., s15'']
                let src = put_input_in_reg(ctx, inputs[0]);
                let shift_by = input_to_reg_mem_imm(ctx, inputs[1]);
                let shift_by_ty = ctx.input_ty(insn, 1);
                let dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();

                // In order for PACKSSWB later to only use the high byte of each 16x8 lane, we shift right an extra 8
                // bits, relying on PSRAW to fill in the upper bits appropriately.
                let bigger_shift_by = match shift_by {
                    // When we know the shift amount at compile time, we add the extra shift amount statically.
                    RegMemImm::Imm { simm32 } => RegMemImm::imm(simm32 + 8),
                    // Otherwise we add instructions to add the extra shift amount and move the value into an XMM
                    // register.
                    RegMemImm::Reg { reg } => {
                        let bigger_shift_by_gpr = ctx.alloc_tmp(shift_by_ty).only_reg().unwrap();
                        ctx.emit(Inst::mov_r_r(OperandSize::Size64, reg, bigger_shift_by_gpr));

                        let size = if shift_by_ty == types::I64 {
                            OperandSize::Size64
                        } else {
                            OperandSize::Size32
                        };
                        let imm = RegMemImm::imm(8);
                        ctx.emit(Inst::alu_rmi_r(
                            size,
                            AluRmiROpcode::Add,
                            imm,
                            bigger_shift_by_gpr,
                        ));

                        let bigger_shift_by_xmm = ctx.alloc_tmp(dst_ty).only_reg().unwrap();
                        ctx.emit(Inst::gpr_to_xmm(
                            SseOpcode::Movd,
                            RegMem::from(bigger_shift_by_gpr),
                            OperandSize::Size32,
                            bigger_shift_by_xmm,
                        ));
                        RegMemImm::reg(bigger_shift_by_xmm.to_reg())
                    }
                    RegMemImm::Mem { .. } => unimplemented!("load shift amount to XMM register"),
                };

                // Unpack and shift the lower lanes of `src` into the `dst` register.
                ctx.emit(Inst::gen_move(dst, src, dst_ty));
                ctx.emit(Inst::xmm_rm_r(SseOpcode::Punpcklbw, RegMem::from(dst), dst));
                ctx.emit(Inst::xmm_rmi_reg(
                    SseOpcode::Psraw,
                    bigger_shift_by.clone(),
                    dst,
                ));

                // Unpack and shift the upper lanes of `src` into a temporary register, `upper_lanes`.
                let upper_lanes = ctx.alloc_tmp(dst_ty).only_reg().unwrap();
                ctx.emit(Inst::gen_move(upper_lanes, src, dst_ty));
                ctx.emit(Inst::xmm_rm_r(
                    SseOpcode::Punpckhbw,
                    RegMem::from(upper_lanes),
                    upper_lanes,
                ));
                ctx.emit(Inst::xmm_rmi_reg(
                    SseOpcode::Psraw,
                    bigger_shift_by,
                    upper_lanes,
                ));

                // Merge the upper and lower shifted lanes into `dst`.
                ctx.emit(Inst::xmm_rm_r(
                    SseOpcode::Packsswb,
                    RegMem::from(upper_lanes),
                    dst,
                ));
            } else if dst_ty == types::I64X2 && op == Opcode::Sshr {
                // The `sshr.i8x16` CLIF instruction has no single x86 instruction in the older feature sets; newer ones
                // like AVX512VL + AVX512F include VPSRAQ, a 128-bit instruction that would fit here, but this backend
                // does not currently have support for EVEX encodings (TODO when EVEX support is available, add an
                // alternate lowering here). To remedy this, we extract each 64-bit lane to a GPR, shift each using a
                // scalar instruction, and insert the shifted values back in the `dst` XMM register.
                let src = put_input_in_reg(ctx, inputs[0]);
                let dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
                ctx.emit(Inst::gen_move(dst, src, dst_ty));

                // Extract the upper and lower lanes into temporary GPRs.
                let lower_lane = ctx.alloc_tmp(types::I64).only_reg().unwrap();
                emit_extract_lane(ctx, src, lower_lane, 0, types::I64);
                let upper_lane = ctx.alloc_tmp(types::I64).only_reg().unwrap();
                emit_extract_lane(ctx, src, upper_lane, 1, types::I64);

                // Shift each value.
                let mut shift = |reg: Writable<Reg>| {
                    let kind = ShiftKind::ShiftRightArithmetic;
                    if let Some(shift_by) = ctx.get_input_as_source_or_const(insn, 1).constant {
                        // Mask the shift amount according to Cranelift's semantics.
                        let shift_by = (shift_by as u8) & (types::I64.bits() as u8 - 1);
                        ctx.emit(Inst::shift_r(
                            OperandSize::Size64,
                            kind,
                            Some(shift_by),
                            reg,
                        ));
                    } else {
                        let dynamic_shift_by = put_input_in_reg(ctx, inputs[1]);
                        let w_rcx = Writable::from_reg(regs::rcx());
                        ctx.emit(Inst::mov_r_r(OperandSize::Size64, dynamic_shift_by, w_rcx));
                        ctx.emit(Inst::shift_r(OperandSize::Size64, kind, None, reg));
                    };
                };
                shift(lower_lane);
                shift(upper_lane);

                // Insert the scalar values back into the `dst` vector.
                emit_insert_lane(ctx, RegMem::from(lower_lane), dst, 0, types::I64);
                emit_insert_lane(ctx, RegMem::from(upper_lane), dst, 1, types::I64);
            } else {
                // For the remaining packed shifts not covered above, x86 has implementations that can either:
                // - shift using an immediate
                // - shift using a dynamic value given in the lower bits of another XMM register.
                let src = put_input_in_reg(ctx, inputs[0]);
                let shift_by = input_to_reg_mem_imm(ctx, inputs[1]);
                let dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
                let sse_op = match dst_ty {
                    types::I16X8 => match op {
                        Opcode::Ishl => SseOpcode::Psllw,
                        Opcode::Ushr => SseOpcode::Psrlw,
                        Opcode::Sshr => SseOpcode::Psraw,
                        _ => unimplemented!("{} is not implemented for type {}", op, dst_ty),
                    },
                    types::I32X4 => match op {
                        Opcode::Ishl => SseOpcode::Pslld,
                        Opcode::Ushr => SseOpcode::Psrld,
                        Opcode::Sshr => SseOpcode::Psrad,
                        _ => unimplemented!("{} is not implemented for type {}", op, dst_ty),
                    },
                    types::I64X2 => match op {
                        Opcode::Ishl => SseOpcode::Psllq,
                        Opcode::Ushr => SseOpcode::Psrlq,
                        _ => unimplemented!("{} is not implemented for type {}", op, dst_ty),
                    },
                    _ => unreachable!(),
                };

                // If necessary, move the shift index into the lowest bits of a vector register.
                let shift_by = match shift_by {
                    RegMemImm::Imm { .. } => shift_by,
                    RegMemImm::Reg { reg } => {
                        let tmp_shift_by = ctx.alloc_tmp(dst_ty).only_reg().unwrap();
                        ctx.emit(Inst::gpr_to_xmm(
                            SseOpcode::Movd,
                            RegMem::reg(reg),
                            OperandSize::Size32,
                            tmp_shift_by,
                        ));
                        RegMemImm::reg(tmp_shift_by.to_reg())
                    }
                    RegMemImm::Mem { .. } => unimplemented!("load shift amount to XMM register"),
                };

                // Move the `src` to the same register as `dst`.
                ctx.emit(Inst::gen_move(dst, src, dst_ty));

                ctx.emit(Inst::xmm_rmi_reg(sse_op, shift_by, dst));
            }
        }

        Opcode::Ineg => {
            let dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let ty = ty.unwrap();

            if ty.is_vector() {
                // Zero's out a register and then does a packed subtraction
                // of the input from the register.

                let src = input_to_reg_mem(ctx, inputs[0]);
                let tmp = ctx.alloc_tmp(types::I32X4).only_reg().unwrap();

                let subtract_opcode = match ty {
                    types::I8X16 => SseOpcode::Psubb,
                    types::I16X8 => SseOpcode::Psubw,
                    types::I32X4 => SseOpcode::Psubd,
                    types::I64X2 => SseOpcode::Psubq,
                    _ => panic!("Unsupported type for Ineg instruction, found {}", ty),
                };

                // Note we must zero out a tmp instead of using the destination register since
                // the desitnation could be an alias for the source input register
                ctx.emit(Inst::xmm_rm_r(
                    SseOpcode::Pxor,
                    RegMem::reg(tmp.to_reg()),
                    tmp,
                ));
                ctx.emit(Inst::xmm_rm_r(subtract_opcode, src, tmp));
                ctx.emit(Inst::xmm_unary_rm_r(
                    SseOpcode::Movapd,
                    RegMem::reg(tmp.to_reg()),
                    dst,
                ));
            } else {
                let src = put_input_in_reg(ctx, inputs[0]);
                ctx.emit(Inst::gen_move(dst, src, ty));
                ctx.emit(Inst::neg(OperandSize::from_ty(ty), dst));
            }
        }

        Opcode::Clz => {
            let orig_ty = ty.unwrap();

            if isa_flags.use_lzcnt() && (orig_ty == types::I32 || orig_ty == types::I64) {
                // We can use a plain lzcnt instruction here. Note no special handling is required
                // for zero inputs, because the machine instruction does what the CLIF expects for
                // zero, i.e. it returns zero.
                let src = input_to_reg_mem(ctx, inputs[0]);
                let dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
                ctx.emit(Inst::unary_rm_r(
                    OperandSize::from_ty(orig_ty),
                    UnaryRmROpcode::Lzcnt,
                    src,
                    dst,
                ));
                return Ok(());
            }

            // General formula using bit-scan reverse (BSR):
            // mov -1, %dst
            // bsr %src, %tmp
            // cmovz %dst, %tmp
            // mov $(size_bits - 1), %dst
            // sub %tmp, %dst

            if orig_ty == types::I128 {
                // clz upper, tmp1
                // clz lower, dst
                // add dst, 64
                // cmp tmp1, 64
                // cmovnz tmp1, dst
                let dsts = get_output_reg(ctx, outputs[0]);
                let dst = dsts.regs()[0];
                let tmp1 = ctx.alloc_tmp(types::I64).only_reg().unwrap();
                let srcs = put_input_in_regs(ctx, inputs[0]);
                let src_lo = srcs.regs()[0];
                let src_hi = srcs.regs()[1];
                emit_clz(ctx, types::I64, types::I64, src_hi, tmp1);
                emit_clz(ctx, types::I64, types::I64, src_lo, dst);
                ctx.emit(Inst::alu_rmi_r(
                    OperandSize::Size64,
                    AluRmiROpcode::Add,
                    RegMemImm::imm(64),
                    dst,
                ));
                ctx.emit(Inst::cmp_rmi_r(
                    OperandSize::Size64,
                    RegMemImm::imm(64),
                    tmp1.to_reg(),
                ));
                ctx.emit(Inst::cmove(
                    OperandSize::Size64,
                    CC::NZ,
                    RegMem::reg(tmp1.to_reg()),
                    dst,
                ));
                ctx.emit(Inst::alu_rmi_r(
                    OperandSize::Size64,
                    AluRmiROpcode::Xor,
                    RegMemImm::reg(dsts.regs()[1].to_reg()),
                    dsts.regs()[1],
                ));
            } else {
                let (ext_spec, ty) = match orig_ty {
                    types::I8 | types::I16 => (Some(ExtSpec::ZeroExtendTo32), types::I32),
                    a if a == types::I32 || a == types::I64 => (None, a),
                    _ => unreachable!(),
                };
                let src = if let Some(ext_spec) = ext_spec {
                    extend_input_to_reg(ctx, inputs[0], ext_spec)
                } else {
                    put_input_in_reg(ctx, inputs[0])
                };

                let dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
                emit_clz(ctx, orig_ty, ty, src, dst);
            }
        }

        Opcode::Ctz => {
            let orig_ty = ctx.input_ty(insn, 0);

            if isa_flags.use_bmi1() && (orig_ty == types::I32 || orig_ty == types::I64) {
                // We can use a plain tzcnt instruction here. Note no special handling is required
                // for zero inputs, because the machine instruction does what the CLIF expects for
                // zero, i.e. it returns zero.
                let src = input_to_reg_mem(ctx, inputs[0]);
                let dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
                ctx.emit(Inst::unary_rm_r(
                    OperandSize::from_ty(orig_ty),
                    UnaryRmROpcode::Tzcnt,
                    src,
                    dst,
                ));
                return Ok(());
            }

            // General formula using bit-scan forward (BSF):
            // bsf %src, %dst
            // mov $(size_bits), %tmp
            // cmovz %tmp, %dst
            if orig_ty == types::I128 {
                // ctz src_lo, dst
                // ctz src_hi, tmp1
                // add tmp1, 64
                // cmp dst, 64
                // cmovz tmp1, dst
                let dsts = get_output_reg(ctx, outputs[0]);
                let dst = dsts.regs()[0];
                let tmp1 = ctx.alloc_tmp(types::I64).only_reg().unwrap();
                let srcs = put_input_in_regs(ctx, inputs[0]);
                let src_lo = srcs.regs()[0];
                let src_hi = srcs.regs()[1];
                emit_ctz(ctx, types::I64, types::I64, src_lo, dst);
                emit_ctz(ctx, types::I64, types::I64, src_hi, tmp1);
                ctx.emit(Inst::alu_rmi_r(
                    OperandSize::Size64,
                    AluRmiROpcode::Add,
                    RegMemImm::imm(64),
                    tmp1,
                ));
                ctx.emit(Inst::cmp_rmi_r(
                    OperandSize::Size64,
                    RegMemImm::imm(64),
                    dst.to_reg(),
                ));
                ctx.emit(Inst::cmove(
                    OperandSize::Size64,
                    CC::Z,
                    RegMem::reg(tmp1.to_reg()),
                    dst,
                ));
                ctx.emit(Inst::alu_rmi_r(
                    OperandSize::Size64,
                    AluRmiROpcode::Xor,
                    RegMemImm::reg(dsts.regs()[1].to_reg()),
                    dsts.regs()[1],
                ));
            } else {
                let ty = if orig_ty.bits() < 32 {
                    types::I32
                } else {
                    orig_ty
                };
                debug_assert!(ty == types::I32 || ty == types::I64);

                let src = put_input_in_reg(ctx, inputs[0]);
                let dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
                emit_ctz(ctx, orig_ty, ty, src, dst);
            }
        }

        Opcode::Popcnt => {
            let ty_tmp = ty.unwrap();
            if !ty_tmp.is_vector() {
                let (ext_spec, ty) = match ctx.input_ty(insn, 0) {
                    types::I8 | types::I16 => (Some(ExtSpec::ZeroExtendTo32), types::I32),
                    a if a == types::I32 || a == types::I64 || a == types::I128 => (None, a),
                    _ => unreachable!(),
                };

                if isa_flags.use_popcnt() {
                    match ty {
                        types::I32 | types::I64 => {
                            let src = input_to_reg_mem(ctx, inputs[0]);
                            let dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
                            ctx.emit(Inst::unary_rm_r(
                                OperandSize::from_ty(ty),
                                UnaryRmROpcode::Popcnt,
                                src,
                                dst,
                            ));
                            return Ok(());
                        }

                        types::I128 => {
                            // The number of ones in a 128-bits value is the plain sum of the number of
                            // ones in its low and high parts. No risk of overflow here.
                            let dsts = get_output_reg(ctx, outputs[0]);
                            let dst = dsts.regs()[0];
                            let tmp = ctx.alloc_tmp(types::I64).only_reg().unwrap();
                            let srcs = put_input_in_regs(ctx, inputs[0]);
                            let src_lo = srcs.regs()[0];
                            let src_hi = srcs.regs()[1];

                            ctx.emit(Inst::unary_rm_r(
                                OperandSize::Size64,
                                UnaryRmROpcode::Popcnt,
                                RegMem::reg(src_lo),
                                dst,
                            ));
                            ctx.emit(Inst::unary_rm_r(
                                OperandSize::Size64,
                                UnaryRmROpcode::Popcnt,
                                RegMem::reg(src_hi),
                                tmp,
                            ));
                            ctx.emit(Inst::alu_rmi_r(
                                OperandSize::Size64,
                                AluRmiROpcode::Add,
                                RegMemImm::reg(tmp.to_reg()),
                                dst,
                            ));

                            // Zero the result's high component.
                            ctx.emit(Inst::alu_rmi_r(
                                OperandSize::Size64,
                                AluRmiROpcode::Xor,
                                RegMemImm::reg(dsts.regs()[1].to_reg()),
                                dsts.regs()[1],
                            ));

                            return Ok(());
                        }
                        _ => {}
                    }
                }

                let (srcs, ty): (SmallVec<[RegMem; 2]>, Type) = if let Some(ext_spec) = ext_spec {
                    (
                        smallvec![RegMem::reg(extend_input_to_reg(ctx, inputs[0], ext_spec))],
                        ty,
                    )
                } else if ty == types::I128 {
                    let regs = put_input_in_regs(ctx, inputs[0]);
                    (
                        smallvec![RegMem::reg(regs.regs()[0]), RegMem::reg(regs.regs()[1])],
                        types::I64,
                    )
                } else {
                    // N.B.: explicitly put input in a reg here because the width of the instruction
                    // into which this RM op goes may not match the width of the input type (in fact,
                    // it won't for i32.popcnt), and we don't want a larger than necessary load.
                    (smallvec![RegMem::reg(put_input_in_reg(ctx, inputs[0]))], ty)
                };

                let mut dsts: SmallVec<[Reg; 2]> = smallvec![];
                for src in srcs {
                    let dst = ctx.alloc_tmp(types::I64).only_reg().unwrap();
                    dsts.push(dst.to_reg());
                    if ty == types::I64 {
                        let tmp1 = ctx.alloc_tmp(types::I64).only_reg().unwrap();
                        let tmp2 = ctx.alloc_tmp(types::I64).only_reg().unwrap();
                        let cst = ctx.alloc_tmp(types::I64).only_reg().unwrap();

                        // mov src, tmp1
                        ctx.emit(Inst::mov64_rm_r(src.clone(), tmp1));

                        // shr $1, tmp1
                        ctx.emit(Inst::shift_r(
                            OperandSize::Size64,
                            ShiftKind::ShiftRightLogical,
                            Some(1),
                            tmp1,
                        ));

                        // mov 0x7777_7777_7777_7777, cst
                        ctx.emit(Inst::imm(OperandSize::Size64, 0x7777777777777777, cst));

                        // andq cst, tmp1
                        ctx.emit(Inst::alu_rmi_r(
                            OperandSize::Size64,
                            AluRmiROpcode::And,
                            RegMemImm::reg(cst.to_reg()),
                            tmp1,
                        ));

                        // mov src, tmp2
                        ctx.emit(Inst::mov64_rm_r(src, tmp2));

                        // sub tmp1, tmp2
                        ctx.emit(Inst::alu_rmi_r(
                            OperandSize::Size64,
                            AluRmiROpcode::Sub,
                            RegMemImm::reg(tmp1.to_reg()),
                            tmp2,
                        ));

                        // shr $1, tmp1
                        ctx.emit(Inst::shift_r(
                            OperandSize::Size64,
                            ShiftKind::ShiftRightLogical,
                            Some(1),
                            tmp1,
                        ));

                        // and cst, tmp1
                        ctx.emit(Inst::alu_rmi_r(
                            OperandSize::Size64,
                            AluRmiROpcode::And,
                            RegMemImm::reg(cst.to_reg()),
                            tmp1,
                        ));

                        // sub tmp1, tmp2
                        ctx.emit(Inst::alu_rmi_r(
                            OperandSize::Size64,
                            AluRmiROpcode::Sub,
                            RegMemImm::reg(tmp1.to_reg()),
                            tmp2,
                        ));

                        // shr $1, tmp1
                        ctx.emit(Inst::shift_r(
                            OperandSize::Size64,
                            ShiftKind::ShiftRightLogical,
                            Some(1),
                            tmp1,
                        ));

                        // and cst, tmp1
                        ctx.emit(Inst::alu_rmi_r(
                            OperandSize::Size64,
                            AluRmiROpcode::And,
                            RegMemImm::reg(cst.to_reg()),
                            tmp1,
                        ));

                        // sub tmp1, tmp2
                        ctx.emit(Inst::alu_rmi_r(
                            OperandSize::Size64,
                            AluRmiROpcode::Sub,
                            RegMemImm::reg(tmp1.to_reg()),
                            tmp2,
                        ));

                        // mov tmp2, dst
                        ctx.emit(Inst::mov64_rm_r(RegMem::reg(tmp2.to_reg()), dst));

                        // shr $4, dst
                        ctx.emit(Inst::shift_r(
                            OperandSize::Size64,
                            ShiftKind::ShiftRightLogical,
                            Some(4),
                            dst,
                        ));

                        // add tmp2, dst
                        ctx.emit(Inst::alu_rmi_r(
                            OperandSize::Size64,
                            AluRmiROpcode::Add,
                            RegMemImm::reg(tmp2.to_reg()),
                            dst,
                        ));

                        // mov $0x0F0F_0F0F_0F0F_0F0F, cst
                        ctx.emit(Inst::imm(OperandSize::Size64, 0x0F0F0F0F0F0F0F0F, cst));

                        // and cst, dst
                        ctx.emit(Inst::alu_rmi_r(
                            OperandSize::Size64,
                            AluRmiROpcode::And,
                            RegMemImm::reg(cst.to_reg()),
                            dst,
                        ));

                        // mov $0x0101_0101_0101_0101, cst
                        ctx.emit(Inst::imm(OperandSize::Size64, 0x0101010101010101, cst));

                        // mul cst, dst
                        ctx.emit(Inst::alu_rmi_r(
                            OperandSize::Size64,
                            AluRmiROpcode::Mul,
                            RegMemImm::reg(cst.to_reg()),
                            dst,
                        ));

                        // shr $56, dst
                        ctx.emit(Inst::shift_r(
                            OperandSize::Size64,
                            ShiftKind::ShiftRightLogical,
                            Some(56),
                            dst,
                        ));
                    } else {
                        assert_eq!(ty, types::I32);

                        let tmp1 = ctx.alloc_tmp(types::I64).only_reg().unwrap();
                        let tmp2 = ctx.alloc_tmp(types::I64).only_reg().unwrap();

                        // mov src, tmp1
                        ctx.emit(Inst::mov64_rm_r(src.clone(), tmp1));

                        // shr $1, tmp1
                        ctx.emit(Inst::shift_r(
                            OperandSize::Size32,
                            ShiftKind::ShiftRightLogical,
                            Some(1),
                            tmp1,
                        ));

                        // andq $0x7777_7777, tmp1
                        ctx.emit(Inst::alu_rmi_r(
                            OperandSize::Size32,
                            AluRmiROpcode::And,
                            RegMemImm::imm(0x77777777),
                            tmp1,
                        ));

                        // mov src, tmp2
                        ctx.emit(Inst::mov64_rm_r(src, tmp2));

                        // sub tmp1, tmp2
                        ctx.emit(Inst::alu_rmi_r(
                            OperandSize::Size32,
                            AluRmiROpcode::Sub,
                            RegMemImm::reg(tmp1.to_reg()),
                            tmp2,
                        ));

                        // shr $1, tmp1
                        ctx.emit(Inst::shift_r(
                            OperandSize::Size32,
                            ShiftKind::ShiftRightLogical,
                            Some(1),
                            tmp1,
                        ));

                        // and 0x7777_7777, tmp1
                        ctx.emit(Inst::alu_rmi_r(
                            OperandSize::Size32,
                            AluRmiROpcode::And,
                            RegMemImm::imm(0x77777777),
                            tmp1,
                        ));

                        // sub tmp1, tmp2
                        ctx.emit(Inst::alu_rmi_r(
                            OperandSize::Size32,
                            AluRmiROpcode::Sub,
                            RegMemImm::reg(tmp1.to_reg()),
                            tmp2,
                        ));

                        // shr $1, tmp1
                        ctx.emit(Inst::shift_r(
                            OperandSize::Size32,
                            ShiftKind::ShiftRightLogical,
                            Some(1),
                            tmp1,
                        ));

                        // and $0x7777_7777, tmp1
                        ctx.emit(Inst::alu_rmi_r(
                            OperandSize::Size32,
                            AluRmiROpcode::And,
                            RegMemImm::imm(0x77777777),
                            tmp1,
                        ));

                        // sub tmp1, tmp2
                        ctx.emit(Inst::alu_rmi_r(
                            OperandSize::Size32,
                            AluRmiROpcode::Sub,
                            RegMemImm::reg(tmp1.to_reg()),
                            tmp2,
                        ));

                        // mov tmp2, dst
                        ctx.emit(Inst::mov64_rm_r(RegMem::reg(tmp2.to_reg()), dst));

                        // shr $4, dst
                        ctx.emit(Inst::shift_r(
                            OperandSize::Size32,
                            ShiftKind::ShiftRightLogical,
                            Some(4),
                            dst,
                        ));

                        // add tmp2, dst
                        ctx.emit(Inst::alu_rmi_r(
                            OperandSize::Size32,
                            AluRmiROpcode::Add,
                            RegMemImm::reg(tmp2.to_reg()),
                            dst,
                        ));

                        // and $0x0F0F_0F0F, dst
                        ctx.emit(Inst::alu_rmi_r(
                            OperandSize::Size32,
                            AluRmiROpcode::And,
                            RegMemImm::imm(0x0F0F0F0F),
                            dst,
                        ));

                        // mul $0x0101_0101, dst
                        ctx.emit(Inst::alu_rmi_r(
                            OperandSize::Size32,
                            AluRmiROpcode::Mul,
                            RegMemImm::imm(0x01010101),
                            dst,
                        ));

                        // shr $24, dst
                        ctx.emit(Inst::shift_r(
                            OperandSize::Size32,
                            ShiftKind::ShiftRightLogical,
                            Some(24),
                            dst,
                        ));
                    }
                }

                if dsts.len() == 1 {
                    let final_dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
                    ctx.emit(Inst::gen_move(final_dst, dsts[0], types::I64));
                } else {
                    assert!(dsts.len() == 2);
                    let final_dst = get_output_reg(ctx, outputs[0]);
                    ctx.emit(Inst::gen_move(final_dst.regs()[0], dsts[0], types::I64));
                    ctx.emit(Inst::alu_rmi_r(
                        OperandSize::Size64,
                        AluRmiROpcode::Add,
                        RegMemImm::reg(dsts[1]),
                        final_dst.regs()[0],
                    ));
                    ctx.emit(Inst::alu_rmi_r(
                        OperandSize::Size64,
                        AluRmiROpcode::Xor,
                        RegMemImm::reg(final_dst.regs()[1].to_reg()),
                        final_dst.regs()[1],
                    ));
                }
            } else {
                // Lower `popcount` for vectors.
                let ty = ty.unwrap();
                let src = put_input_in_reg(ctx, inputs[0]);
                let dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();

                if isa_flags.use_avx512vl_simd() && isa_flags.use_avx512bitalg_simd() {
                    // When AVX512VL and AVX512BITALG are available,
                    // `popcnt.i8x16` can be lowered to a single instruction.
                    assert_eq!(ty, types::I8X16);
                    ctx.emit(Inst::xmm_unary_rm_r_evex(
                        Avx512Opcode::Vpopcntb,
                        RegMem::reg(src),
                        dst,
                    ));
                } else {
                    // For SIMD 4.4 we use Mula's algorithm (https://arxiv.org/pdf/1611.07612.pdf)
                    //
                    //__m128i count_bytes ( __m128i v) {
                    //    __m128i lookup = _mm_setr_epi8(0, 1, 1, 2, 1, 2, 2, 3, 1, 2, 2, 3, 2, 3, 3, 4);
                    //    __m128i low_mask = _mm_set1_epi8 (0x0f);
                    //    __m128i lo = _mm_and_si128 (v, low_mask);
                    //    __m128i hi = _mm_and_si128 (_mm_srli_epi16 (v, 4), low_mask);
                    //    __m128i cnt1 = _mm_shuffle_epi8 (lookup, lo);
                    //    __m128i cnt2 = _mm_shuffle_epi8 (lookup, hi);
                    //    return _mm_add_epi8 (cnt1, cnt2);
                    //}
                    //
                    // Details of the above algorithm can be found in the reference noted above, but the basics
                    // are to create a lookup table that pre populates the popcnt values for each number [0,15].
                    // The algorithm uses shifts to isolate 4 bit sections of the vector, pshufb as part of the
                    // lookup process, and adds together the results.

                    // __m128i lookup = _mm_setr_epi8(0, 1, 1, 2, 1, 2, 2, 3, 1, 2, 2, 3, 2, 3, 3, 4);
                    static POPCOUNT_4BIT: [u8; 16] = [
                        0x00, 0x01, 0x01, 0x02, 0x01, 0x02, 0x02, 0x03, 0x01, 0x02, 0x02, 0x03,
                        0x02, 0x03, 0x03, 0x04,
                    ];
                    let lookup = ctx.use_constant(VCodeConstantData::WellKnown(&POPCOUNT_4BIT));

                    // Create a mask for lower 4bits of each subword.
                    static LOW_MASK: [u8; 16] = [0x0F; 16];
                    let low_mask_const = ctx.use_constant(VCodeConstantData::WellKnown(&LOW_MASK));
                    let low_mask = ctx.alloc_tmp(types::I8X16).only_reg().unwrap();
                    ctx.emit(Inst::xmm_load_const(low_mask_const, low_mask, ty));

                    // __m128i lo = _mm_and_si128 (v, low_mask);
                    let lo = ctx.alloc_tmp(types::I8X16).only_reg().unwrap();
                    ctx.emit(Inst::gen_move(lo, low_mask.to_reg(), types::I8X16));
                    ctx.emit(Inst::xmm_rm_r(SseOpcode::Pand, RegMem::reg(src), lo));

                    // __m128i hi = _mm_and_si128 (_mm_srli_epi16 (v, 4), low_mask);
                    ctx.emit(Inst::gen_move(dst, src, ty));
                    ctx.emit(Inst::xmm_rmi_reg(SseOpcode::Psrlw, RegMemImm::imm(4), dst));
                    let tmp = ctx.alloc_tmp(types::I8X16).only_reg().unwrap();
                    ctx.emit(Inst::gen_move(tmp, low_mask.to_reg(), types::I8X16));
                    ctx.emit(Inst::xmm_rm_r(
                        SseOpcode::Pand,
                        RegMem::reg(dst.to_reg()),
                        tmp,
                    ));

                    // __m128i cnt1 = _mm_shuffle_epi8 (lookup, lo);
                    let tmp2 = ctx.alloc_tmp(types::I8X16).only_reg().unwrap();
                    ctx.emit(Inst::xmm_load_const(lookup, tmp2, ty));
                    ctx.emit(Inst::gen_move(dst, tmp2.to_reg(), types::I8X16));

                    ctx.emit(Inst::xmm_rm_r(
                        SseOpcode::Pshufb,
                        RegMem::reg(lo.to_reg()),
                        dst,
                    ));

                    // __m128i cnt2 = _mm_shuffle_epi8 (lookup , hi) ;
                    ctx.emit(Inst::xmm_rm_r(
                        SseOpcode::Pshufb,
                        RegMem::reg(tmp.to_reg()),
                        tmp2,
                    ));

                    // return _mm_add_epi8 (cnt1 , cnt2 ) ;
                    ctx.emit(Inst::xmm_rm_r(
                        SseOpcode::Paddb,
                        RegMem::reg(tmp2.to_reg()),
                        dst,
                    ));
                }
            }
        }

        Opcode::Bitrev => {
            let ty = ctx.input_ty(insn, 0);
            assert!(
                ty == types::I8
                    || ty == types::I16
                    || ty == types::I32
                    || ty == types::I64
                    || ty == types::I128
            );

            if ty == types::I128 {
                let src = put_input_in_regs(ctx, inputs[0]);
                let dst = get_output_reg(ctx, outputs[0]);
                emit_bitrev(ctx, src.regs()[0], dst.regs()[1], types::I64);
                emit_bitrev(ctx, src.regs()[1], dst.regs()[0], types::I64);
            } else {
                let src = put_input_in_reg(ctx, inputs[0]);
                let dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
                emit_bitrev(ctx, src, dst, ty);
            }
        }

        Opcode::IsNull | Opcode::IsInvalid => {
            // Null references are represented by the constant value 0; invalid references are
            // represented by the constant value -1. See `define_reftypes()` in
            // `meta/src/isa/x86/encodings.rs` to confirm.
            let src = put_input_in_reg(ctx, inputs[0]);
            let dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let ty = ctx.input_ty(insn, 0);
            let imm = match op {
                Opcode::IsNull => {
                    // TODO could use tst src, src for IsNull
                    0
                }
                Opcode::IsInvalid => {
                    // We can do a 32-bit comparison even in 64-bits mode, as the constant is then
                    // sign-extended.
                    0xffffffff
                }
                _ => unreachable!(),
            };
            ctx.emit(Inst::cmp_rmi_r(
                OperandSize::from_ty(ty),
                RegMemImm::imm(imm),
                src,
            ));
            ctx.emit(Inst::setcc(CC::Z, dst));
        }

        Opcode::Uextend | Opcode::Sextend | Opcode::Breduce | Opcode::Bextend | Opcode::Ireduce => {
            let src_ty = ctx.input_ty(insn, 0);
            let dst_ty = ctx.output_ty(insn, 0);

            if src_ty == types::I128 {
                assert!(dst_ty.bits() <= 64);
                assert!(op == Opcode::Ireduce);
                let src = put_input_in_regs(ctx, inputs[0]);
                let dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
                ctx.emit(Inst::gen_move(dst, src.regs()[0], types::I64));
            } else if dst_ty == types::I128 {
                assert!(src_ty.bits() <= 64);
                let src = put_input_in_reg(ctx, inputs[0]);
                let dst = get_output_reg(ctx, outputs[0]);
                assert!(op == Opcode::Uextend || op == Opcode::Sextend);
                // Extend to 64 bits first.

                let ext_mode = ExtMode::new(src_ty.bits(), /* dst bits = */ 64);
                if let Some(ext_mode) = ext_mode {
                    if op == Opcode::Sextend {
                        ctx.emit(Inst::movsx_rm_r(ext_mode, RegMem::reg(src), dst.regs()[0]));
                    } else {
                        ctx.emit(Inst::movzx_rm_r(ext_mode, RegMem::reg(src), dst.regs()[0]));
                    }
                } else {
                    ctx.emit(Inst::mov64_rm_r(RegMem::reg(src), dst.regs()[0]));
                }

                // Now generate the top 64 bits.
                if op == Opcode::Sextend {
                    // Sign-extend: move dst[0] into dst[1] and arithmetic-shift right by 63 bits
                    // to spread the sign bit across all bits.
                    ctx.emit(Inst::gen_move(
                        dst.regs()[1],
                        dst.regs()[0].to_reg(),
                        types::I64,
                    ));
                    ctx.emit(Inst::shift_r(
                        OperandSize::Size64,
                        ShiftKind::ShiftRightArithmetic,
                        Some(63),
                        dst.regs()[1],
                    ));
                } else {
                    // Zero-extend: just zero the top word.
                    ctx.emit(Inst::alu_rmi_r(
                        OperandSize::Size64,
                        AluRmiROpcode::Xor,
                        RegMemImm::reg(dst.regs()[1].to_reg()),
                        dst.regs()[1],
                    ));
                }
            } else {
                // Sextend requires a sign-extended move, but all the other opcodes are simply a move
                // from a zero-extended source. Here is why this works, in each case:
                //
                // - Breduce, Bextend: changing width of a boolean. We
                //   represent a bool as a 0 or -1, so Breduce can mask, while
                //   Bextend must sign-extend.
                //
                // - Ireduce: changing width of an integer. Smaller ints are stored with undefined
                // high-order bits, so we can simply do a copy.
                let is_sextend = match op {
                    Opcode::Sextend | Opcode::Bextend => true,
                    _ => false,
                };
                if src_ty == types::I32 && dst_ty == types::I64 && !is_sextend {
                    // As a particular x64 extra-pattern matching opportunity, all the ALU opcodes on
                    // 32-bits will zero-extend the upper 32-bits, so we can even not generate a
                    // zero-extended move in this case.
                    // TODO add loads and shifts here.
                    if let Some(_) = matches_input_any(
                        ctx,
                        inputs[0],
                        &[
                            Opcode::Iadd,
                            Opcode::IaddIfcout,
                            Opcode::Isub,
                            Opcode::Imul,
                            Opcode::Band,
                            Opcode::Bor,
                            Opcode::Bxor,
                        ],
                    ) {
                        let src = put_input_in_reg(ctx, inputs[0]);
                        let dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
                        ctx.emit(Inst::gen_move(dst, src, types::I64));
                        return Ok(());
                    }
                }

                let src = input_to_reg_mem(ctx, inputs[0]);
                let dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();

                let ext_mode = ExtMode::new(src_ty.bits(), dst_ty.bits());
                assert_eq!(
                    src_ty.bits() < dst_ty.bits(),
                    ext_mode.is_some(),
                    "unexpected extension: {} -> {}",
                    src_ty,
                    dst_ty
                );

                if let Some(ext_mode) = ext_mode {
                    if is_sextend {
                        ctx.emit(Inst::movsx_rm_r(ext_mode, src, dst));
                    } else {
                        ctx.emit(Inst::movzx_rm_r(ext_mode, src, dst));
                    }
                } else {
                    ctx.emit(Inst::mov64_rm_r(src, dst));
                }
            }
        }

        Opcode::Bint => {
            // Booleans are stored as all-zeroes (0) or all-ones (-1). We AND
            // out the LSB to give a 0 / 1-valued integer result.
            let rn = put_input_in_reg(ctx, inputs[0]);
            let rd = get_output_reg(ctx, outputs[0]);
            let ty = ctx.output_ty(insn, 0);

            ctx.emit(Inst::gen_move(rd.regs()[0], rn, types::I64));
            ctx.emit(Inst::alu_rmi_r(
                OperandSize::Size64,
                AluRmiROpcode::And,
                RegMemImm::imm(1),
                rd.regs()[0],
            ));

            if ty == types::I128 {
                let upper = rd.regs()[1];
                ctx.emit(Inst::alu_rmi_r(
                    OperandSize::Size64,
                    AluRmiROpcode::Xor,
                    RegMemImm::reg(upper.to_reg()),
                    upper,
                ));
            }
        }

        Opcode::Icmp => {
            let condcode = ctx.data(insn).cond_code().unwrap();
            let dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let ty = ctx.input_ty(insn, 0);
            if !ty.is_vector() {
                let condcode = emit_cmp(ctx, insn, condcode);
                let cc = CC::from_intcc(condcode);
                ctx.emit(Inst::setcc(cc, dst));
            } else {
                assert_eq!(ty.bits(), 128);
                let eq = |ty| match ty {
                    types::I8X16 => SseOpcode::Pcmpeqb,
                    types::I16X8 => SseOpcode::Pcmpeqw,
                    types::I32X4 => SseOpcode::Pcmpeqd,
                    types::I64X2 => SseOpcode::Pcmpeqq,
                    _ => panic!(
                        "Unable to find an instruction for {} for type: {}",
                        condcode, ty
                    ),
                };
                let gt = |ty| match ty {
                    types::I8X16 => SseOpcode::Pcmpgtb,
                    types::I16X8 => SseOpcode::Pcmpgtw,
                    types::I32X4 => SseOpcode::Pcmpgtd,
                    types::I64X2 => SseOpcode::Pcmpgtq,
                    _ => panic!(
                        "Unable to find an instruction for {} for type: {}",
                        condcode, ty
                    ),
                };
                let maxu = |ty| match ty {
                    types::I8X16 => SseOpcode::Pmaxub,
                    types::I16X8 => SseOpcode::Pmaxuw,
                    types::I32X4 => SseOpcode::Pmaxud,
                    _ => panic!(
                        "Unable to find an instruction for {} for type: {}",
                        condcode, ty
                    ),
                };
                let mins = |ty| match ty {
                    types::I8X16 => SseOpcode::Pminsb,
                    types::I16X8 => SseOpcode::Pminsw,
                    types::I32X4 => SseOpcode::Pminsd,
                    _ => panic!(
                        "Unable to find an instruction for {} for type: {}",
                        condcode, ty
                    ),
                };
                let minu = |ty| match ty {
                    types::I8X16 => SseOpcode::Pminub,
                    types::I16X8 => SseOpcode::Pminuw,
                    types::I32X4 => SseOpcode::Pminud,
                    _ => panic!(
                        "Unable to find an instruction for {} for type: {}",
                        condcode, ty
                    ),
                };

                // Here we decide which operand to use as the read/write `dst` (ModRM reg field) and
                // which to use as the read `input` (ModRM r/m field). In the normal case we use
                // Cranelift's first operand, the `lhs`, as `dst` but we flip the operands for the
                // less-than cases so that we can reuse the greater-than implementation.
                //
                // In a surprising twist, the operands for i64x2 `gte`/`sle` must also be flipped
                // from the normal order because of the special-case lowering for these instructions
                // (i.e. we use PCMPGTQ with flipped operands and negate the result).
                let input = match condcode {
                    IntCC::SignedLessThanOrEqual if ty == types::I64X2 => {
                        let lhs = put_input_in_reg(ctx, inputs[0]);
                        let rhs = input_to_reg_mem(ctx, inputs[1]);
                        ctx.emit(Inst::gen_move(dst, lhs, ty));
                        rhs
                    }
                    IntCC::SignedGreaterThanOrEqual if ty == types::I64X2 => {
                        let lhs = input_to_reg_mem(ctx, inputs[0]);
                        let rhs = put_input_in_reg(ctx, inputs[1]);
                        ctx.emit(Inst::gen_move(dst, rhs, ty));
                        lhs
                    }
                    IntCC::SignedLessThan
                    | IntCC::SignedLessThanOrEqual
                    | IntCC::UnsignedLessThan
                    | IntCC::UnsignedLessThanOrEqual => {
                        let lhs = input_to_reg_mem(ctx, inputs[0]);
                        let rhs = put_input_in_reg(ctx, inputs[1]);
                        ctx.emit(Inst::gen_move(dst, rhs, ty));
                        lhs
                    }
                    _ => {
                        let lhs = put_input_in_reg(ctx, inputs[0]);
                        let rhs = input_to_reg_mem(ctx, inputs[1]);
                        ctx.emit(Inst::gen_move(dst, lhs, ty));
                        rhs
                    }
                };

                match condcode {
                    IntCC::Equal => ctx.emit(Inst::xmm_rm_r(eq(ty), input, dst)),
                    IntCC::NotEqual => {
                        ctx.emit(Inst::xmm_rm_r(eq(ty), input, dst));
                        // Emit all 1s into the `tmp` register.
                        let tmp = ctx.alloc_tmp(ty).only_reg().unwrap();
                        ctx.emit(Inst::xmm_rm_r(eq(ty), RegMem::from(tmp), tmp));
                        // Invert the result of the `PCMPEQ*`.
                        ctx.emit(Inst::xmm_rm_r(SseOpcode::Pxor, RegMem::from(tmp), dst));
                    }
                    IntCC::SignedGreaterThan | IntCC::SignedLessThan => {
                        ctx.emit(Inst::xmm_rm_r(gt(ty), input, dst))
                    }
                    IntCC::SignedGreaterThanOrEqual | IntCC::SignedLessThanOrEqual
                        if ty != types::I64X2 =>
                    {
                        ctx.emit(Inst::xmm_rm_r(mins(ty), input.clone(), dst));
                        ctx.emit(Inst::xmm_rm_r(eq(ty), input, dst))
                    }
                    IntCC::SignedGreaterThanOrEqual | IntCC::SignedLessThanOrEqual
                        if ty == types::I64X2 =>
                    {
                        // The PMINS* instruction is only available in AVX512VL/F so we must instead
                        // compare with flipped operands and negate the result (emitting one more
                        // instruction).
                        ctx.emit(Inst::xmm_rm_r(gt(ty), input, dst));
                        // Emit all 1s into the `tmp` register.
                        let tmp = ctx.alloc_tmp(ty).only_reg().unwrap();
                        ctx.emit(Inst::xmm_rm_r(eq(ty), RegMem::from(tmp), tmp));
                        // Invert the result of the `PCMPGT*`.
                        ctx.emit(Inst::xmm_rm_r(SseOpcode::Pxor, RegMem::from(tmp), dst));
                    }
                    IntCC::UnsignedGreaterThan | IntCC::UnsignedLessThan => {
                        ctx.emit(Inst::xmm_rm_r(maxu(ty), input.clone(), dst));
                        ctx.emit(Inst::xmm_rm_r(eq(ty), input, dst));
                        // Emit all 1s into the `tmp` register.
                        let tmp = ctx.alloc_tmp(ty).only_reg().unwrap();
                        ctx.emit(Inst::xmm_rm_r(eq(ty), RegMem::from(tmp), tmp));
                        // Invert the result of the `PCMPEQ*`.
                        ctx.emit(Inst::xmm_rm_r(SseOpcode::Pxor, RegMem::from(tmp), dst));
                    }
                    IntCC::UnsignedGreaterThanOrEqual | IntCC::UnsignedLessThanOrEqual => {
                        ctx.emit(Inst::xmm_rm_r(minu(ty), input.clone(), dst));
                        ctx.emit(Inst::xmm_rm_r(eq(ty), input, dst))
                    }
                    _ => unimplemented!("Unimplemented comparison code for icmp: {}", condcode),
                }
            }
        }

        Opcode::Fcmp => {
            let cond_code = ctx.data(insn).fp_cond_code().unwrap();
            let input_ty = ctx.input_ty(insn, 0);
            if !input_ty.is_vector() {
                // Unordered is returned by setting ZF, PF, CF <- 111
                // Greater than by ZF, PF, CF <- 000
                // Less than by ZF, PF, CF <- 001
                // Equal by ZF, PF, CF <- 100
                //
                // Checking the result of comiss is somewhat annoying because you don't have setcc
                // instructions that explicitly check simultaneously for the condition (i.e. eq, le,
                // gt, etc) *and* orderedness.
                //
                // So that might mean we need more than one setcc check and then a logical "and" or
                // "or" to determine both, in some cases.  However knowing that if the parity bit is
                // set, then the result was considered unordered and knowing that if the parity bit is
                // set, then both the ZF and CF flag bits must also be set we can get away with using
                // one setcc for most condition codes.

                let dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();

                match emit_fcmp(ctx, insn, cond_code, FcmpSpec::Normal) {
                    FcmpCondResult::Condition(cc) => {
                        ctx.emit(Inst::setcc(cc, dst));
                    }
                    FcmpCondResult::AndConditions(cc1, cc2) => {
                        let tmp = ctx.alloc_tmp(types::I32).only_reg().unwrap();
                        ctx.emit(Inst::setcc(cc1, tmp));
                        ctx.emit(Inst::setcc(cc2, dst));
                        ctx.emit(Inst::alu_rmi_r(
                            OperandSize::Size32,
                            AluRmiROpcode::And,
                            RegMemImm::reg(tmp.to_reg()),
                            dst,
                        ));
                    }
                    FcmpCondResult::OrConditions(cc1, cc2) => {
                        let tmp = ctx.alloc_tmp(types::I32).only_reg().unwrap();
                        ctx.emit(Inst::setcc(cc1, tmp));
                        ctx.emit(Inst::setcc(cc2, dst));
                        ctx.emit(Inst::alu_rmi_r(
                            OperandSize::Size32,
                            AluRmiROpcode::Or,
                            RegMemImm::reg(tmp.to_reg()),
                            dst,
                        ));
                    }
                    FcmpCondResult::InvertedEqualOrConditions(_, _) => unreachable!(),
                }
            } else {
                let op = match input_ty {
                    types::F32X4 => SseOpcode::Cmpps,
                    types::F64X2 => SseOpcode::Cmppd,
                    _ => panic!("Bad input type to fcmp: {}", input_ty),
                };

                // Since some packed comparisons are not available, some of the condition codes
                // must be inverted, with a corresponding `flip` of the operands.
                let (imm, flip) = match cond_code {
                    FloatCC::GreaterThan => (FcmpImm::LessThan, true),
                    FloatCC::GreaterThanOrEqual => (FcmpImm::LessThanOrEqual, true),
                    FloatCC::UnorderedOrLessThan => (FcmpImm::UnorderedOrGreaterThan, true),
                    FloatCC::UnorderedOrLessThanOrEqual => {
                        (FcmpImm::UnorderedOrGreaterThanOrEqual, true)
                    }
                    FloatCC::OrderedNotEqual | FloatCC::UnorderedOrEqual => {
                        panic!("unsupported float condition code: {}", cond_code)
                    }
                    _ => (FcmpImm::from(cond_code), false),
                };

                // Determine the operands of the comparison, possibly by flipping them.
                let (lhs, rhs) = if flip {
                    (
                        put_input_in_reg(ctx, inputs[1]),
                        input_to_reg_mem(ctx, inputs[0]),
                    )
                } else {
                    (
                        put_input_in_reg(ctx, inputs[0]),
                        input_to_reg_mem(ctx, inputs[1]),
                    )
                };

                // Move the `lhs` to the same register as `dst`; this may not emit an actual move
                // but ensures that the registers are the same to match x86's read-write operand
                // encoding.
                let dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
                ctx.emit(Inst::gen_move(dst, lhs, input_ty));

                // Emit the comparison.
                ctx.emit(Inst::xmm_rm_r_imm(
                    op,
                    rhs,
                    dst,
                    imm.encode(),
                    OperandSize::Size32,
                ));
            }
        }

        Opcode::FallthroughReturn | Opcode::Return => {
            for i in 0..ctx.num_inputs(insn) {
                let src_reg = put_input_in_regs(ctx, inputs[i]);
                let retval_reg = ctx.retval(i);
                let ty = ctx.input_ty(insn, i);
                assert!(src_reg.len() == retval_reg.len());
                let (_, tys) = Inst::rc_for_type(ty)?;
                for ((&src, &dst), &ty) in src_reg
                    .regs()
                    .iter()
                    .zip(retval_reg.regs().iter())
                    .zip(tys.iter())
                {
                    ctx.emit(Inst::gen_move(dst, src, ty));
                }
            }
            // N.B.: the Ret itself is generated by the ABI.
        }

        Opcode::Call | Opcode::CallIndirect => {
            let caller_conv = ctx.abi().call_conv();
            let (mut abi, inputs) = match op {
                Opcode::Call => {
                    let (extname, dist) = ctx.call_target(insn).unwrap();
                    let sig = ctx.call_sig(insn).unwrap();
                    assert_eq!(inputs.len(), sig.params.len());
                    assert_eq!(outputs.len(), sig.returns.len());
                    (
                        X64ABICaller::from_func(sig, &extname, dist, caller_conv, flags)?,
                        &inputs[..],
                    )
                }

                Opcode::CallIndirect => {
                    let ptr = put_input_in_reg(ctx, inputs[0]);
                    let sig = ctx.call_sig(insn).unwrap();
                    assert_eq!(inputs.len() - 1, sig.params.len());
                    assert_eq!(outputs.len(), sig.returns.len());
                    (
                        X64ABICaller::from_ptr(sig, ptr, op, caller_conv, flags)?,
                        &inputs[1..],
                    )
                }

                _ => unreachable!(),
            };

            abi.emit_stack_pre_adjust(ctx);
            assert_eq!(inputs.len(), abi.num_args());
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

        Opcode::Debugtrap => {
            ctx.emit(Inst::Hlt);
        }

        Opcode::Trap | Opcode::ResumableTrap => {
            let trap_code = ctx.data(insn).trap_code().unwrap();
            ctx.emit_safepoint(Inst::Ud2 { trap_code });
        }

        Opcode::Trapif | Opcode::Trapff => {
            let trap_code = ctx.data(insn).trap_code().unwrap();

            if matches_input(ctx, inputs[0], Opcode::IaddIfcout).is_some() {
                let cond_code = ctx.data(insn).cond_code().unwrap();
                // The flags must not have been clobbered by any other instruction between the
                // iadd_ifcout and this instruction, as verified by the CLIF validator; so we can
                // simply use the flags here.
                let cc = CC::from_intcc(cond_code);

                ctx.emit_safepoint(Inst::TrapIf { trap_code, cc });
            } else if op == Opcode::Trapif {
                let cond_code = ctx.data(insn).cond_code().unwrap();

                // Verification ensures that the input is always a single-def ifcmp.
                let ifcmp = matches_input(ctx, inputs[0], Opcode::Ifcmp).unwrap();
                let cond_code = emit_cmp(ctx, ifcmp, cond_code);
                let cc = CC::from_intcc(cond_code);

                ctx.emit_safepoint(Inst::TrapIf { trap_code, cc });
            } else {
                let cond_code = ctx.data(insn).fp_cond_code().unwrap();

                // Verification ensures that the input is always a single-def ffcmp.
                let ffcmp = matches_input(ctx, inputs[0], Opcode::Ffcmp).unwrap();

                match emit_fcmp(ctx, ffcmp, cond_code, FcmpSpec::Normal) {
                    FcmpCondResult::Condition(cc) => {
                        ctx.emit_safepoint(Inst::TrapIf { trap_code, cc })
                    }
                    FcmpCondResult::AndConditions(cc1, cc2) => {
                        // A bit unfortunate, but materialize the flags in their own register, and
                        // check against this.
                        let tmp = ctx.alloc_tmp(types::I32).only_reg().unwrap();
                        let tmp2 = ctx.alloc_tmp(types::I32).only_reg().unwrap();
                        ctx.emit(Inst::setcc(cc1, tmp));
                        ctx.emit(Inst::setcc(cc2, tmp2));
                        ctx.emit(Inst::alu_rmi_r(
                            OperandSize::Size32,
                            AluRmiROpcode::And,
                            RegMemImm::reg(tmp.to_reg()),
                            tmp2,
                        ));
                        ctx.emit_safepoint(Inst::TrapIf {
                            trap_code,
                            cc: CC::NZ,
                        });
                    }
                    FcmpCondResult::OrConditions(cc1, cc2) => {
                        ctx.emit_safepoint(Inst::TrapIf { trap_code, cc: cc1 });
                        ctx.emit_safepoint(Inst::TrapIf { trap_code, cc: cc2 });
                    }
                    FcmpCondResult::InvertedEqualOrConditions(_, _) => unreachable!(),
                };
            };
        }

        Opcode::F64const => {
            // TODO use cmpeqpd for all 1s.
            let value = ctx.get_constant(insn).unwrap();
            let dst = get_output_reg(ctx, outputs[0]);
            for inst in Inst::gen_constant(dst, value as u128, types::F64, |ty| {
                ctx.alloc_tmp(ty).only_reg().unwrap()
            }) {
                ctx.emit(inst);
            }
        }

        Opcode::F32const => {
            // TODO use cmpeqps for all 1s.
            let value = ctx.get_constant(insn).unwrap();
            let dst = get_output_reg(ctx, outputs[0]);
            for inst in Inst::gen_constant(dst, value as u128, types::F32, |ty| {
                ctx.alloc_tmp(ty).only_reg().unwrap()
            }) {
                ctx.emit(inst);
            }
        }

        Opcode::WideningPairwiseDotProductS => {
            let lhs = put_input_in_reg(ctx, inputs[0]);
            let rhs = input_to_reg_mem(ctx, inputs[1]);
            let dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let ty = ty.unwrap();

            ctx.emit(Inst::gen_move(dst, lhs, ty));

            if ty == types::I32X4 {
                ctx.emit(Inst::xmm_rm_r(SseOpcode::Pmaddwd, rhs, dst));
            } else {
                panic!(
                    "Opcode::WideningPairwiseDotProductS: unsupported laneage: {:?}",
                    ty
                );
            }
        }

        Opcode::Fadd | Opcode::Fsub | Opcode::Fmul | Opcode::Fdiv => {
            let lhs = put_input_in_reg(ctx, inputs[0]);
            // We can't guarantee the RHS (if a load) is 128-bit aligned, so we
            // must avoid merging a load here.
            let rhs = RegMem::reg(put_input_in_reg(ctx, inputs[1]));
            let dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let ty = ty.unwrap();

            // Move the `lhs` to the same register as `dst`; this may not emit an actual move
            // but ensures that the registers are the same to match x86's read-write operand
            // encoding.
            ctx.emit(Inst::gen_move(dst, lhs, ty));

            // Note: min and max can't be handled here, because of the way Cranelift defines them:
            // if any operand is a NaN, they must return the NaN operand, while the x86 machine
            // instruction will return the second operand if either operand is a NaN.
            let sse_op = match ty {
                types::F32 => match op {
                    Opcode::Fadd => SseOpcode::Addss,
                    Opcode::Fsub => SseOpcode::Subss,
                    Opcode::Fmul => SseOpcode::Mulss,
                    Opcode::Fdiv => SseOpcode::Divss,
                    _ => unreachable!(),
                },
                types::F64 => match op {
                    Opcode::Fadd => SseOpcode::Addsd,
                    Opcode::Fsub => SseOpcode::Subsd,
                    Opcode::Fmul => SseOpcode::Mulsd,
                    Opcode::Fdiv => SseOpcode::Divsd,
                    _ => unreachable!(),
                },
                types::F32X4 => match op {
                    Opcode::Fadd => SseOpcode::Addps,
                    Opcode::Fsub => SseOpcode::Subps,
                    Opcode::Fmul => SseOpcode::Mulps,
                    Opcode::Fdiv => SseOpcode::Divps,
                    _ => unreachable!(),
                },
                types::F64X2 => match op {
                    Opcode::Fadd => SseOpcode::Addpd,
                    Opcode::Fsub => SseOpcode::Subpd,
                    Opcode::Fmul => SseOpcode::Mulpd,
                    Opcode::Fdiv => SseOpcode::Divpd,
                    _ => unreachable!(),
                },
                _ => panic!(
                    "invalid type: expected one of [F32, F64, F32X4, F64X2], found {}",
                    ty
                ),
            };
            ctx.emit(Inst::xmm_rm_r(sse_op, rhs, dst));
        }

        Opcode::Fmin | Opcode::Fmax => {
            let lhs = put_input_in_reg(ctx, inputs[0]);
            let rhs = put_input_in_reg(ctx, inputs[1]);
            let dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let is_min = op == Opcode::Fmin;
            let output_ty = ty.unwrap();
            ctx.emit(Inst::gen_move(dst, rhs, output_ty));
            if !output_ty.is_vector() {
                let op_size = match output_ty {
                    types::F32 => OperandSize::Size32,
                    types::F64 => OperandSize::Size64,
                    _ => panic!("unexpected type {:?} for fmin/fmax", output_ty),
                };
                ctx.emit(Inst::xmm_min_max_seq(op_size, is_min, lhs, dst));
            } else {
                // X64's implementation of floating point min and floating point max does not
                // propagate NaNs and +0's in a way that is friendly to the SIMD spec. For the
                // scalar approach we use jumps to handle cases where NaN and +0 propagation is
                // not consistent with what is needed. However for packed floating point min and
                // floating point max we implement a different approach to avoid the sequence
                // of jumps that would be required on a per lane basis. Because we do not need to
                // lower labels and jumps but do need ctx for creating temporaries we implement
                // the lowering here in lower.rs instead of emit.rs as is done in the case for scalars.
                // The outline of approach is as follows:
                //
                // First we preform the Min/Max in both directions. This is because in the
                // case of an operand's lane containing a NaN or in the case of the lanes of the
                // two operands containing 0 but with mismatched signs, x64 will return the second
                // operand regardless of its contents. So in order to make sure we capture NaNs and
                // normalize NaNs and 0 values we capture the operation in both directions and merge the
                // results. Then we normalize the results through operations that create a mask for the
                // lanes containing NaNs, we use that mask to adjust NaNs to quite NaNs and normalize
                // 0s.
                //
                // The following sequence is generated for min:
                //
                // movap{s,d} %lhs, %tmp
                // minp{s,d} %dst, %tmp
                // minp,{s,d} %lhs, %dst
                // orp{s,d} %dst, %tmp
                // cmpp{s,d} %tmp, %dst, $3
                // orps{s,d} %dst, %tmp
                // psrl{s,d} {$10, $13}, %dst
                // andnp{s,d} %tmp, %dst
                //
                // and for max the sequence is:
                //
                // movap{s,d} %lhs, %tmp
                // minp{s,d} %dst, %tmp
                // minp,{s,d} %lhs, %dst
                // xorp{s,d} %tmp, %dst
                // orp{s,d} %dst, %tmp
                // subp{s,d} %dst, %tmp
                // cmpp{s,d} %tmp, %dst, $3
                // psrl{s,d} {$10, $13}, %dst
                // andnp{s,d} %tmp, %dst

                if is_min {
                    let (mov_op, min_op, or_op, cmp_op, shift_op, shift_by, andn_op) =
                        match output_ty {
                            types::F32X4 => (
                                SseOpcode::Movaps,
                                SseOpcode::Minps,
                                SseOpcode::Orps,
                                SseOpcode::Cmpps,
                                SseOpcode::Psrld,
                                10,
                                SseOpcode::Andnps,
                            ),
                            types::F64X2 => (
                                SseOpcode::Movapd,
                                SseOpcode::Minpd,
                                SseOpcode::Orpd,
                                SseOpcode::Cmppd,
                                SseOpcode::Psrlq,
                                13,
                                SseOpcode::Andnpd,
                            ),
                            _ => unimplemented!("unsupported op type {:?}", output_ty),
                        };

                    // Copy lhs into tmp
                    let tmp_xmm1 = ctx.alloc_tmp(output_ty).only_reg().unwrap();
                    ctx.emit(Inst::xmm_mov(mov_op, RegMem::reg(lhs), tmp_xmm1));

                    // Perform min in reverse direction
                    ctx.emit(Inst::xmm_rm_r(min_op, RegMem::from(dst), tmp_xmm1));

                    // Perform min in original direction
                    ctx.emit(Inst::xmm_rm_r(min_op, RegMem::reg(lhs), dst));

                    // X64 handles propagation of -0's and Nans differently between left and right
                    // operands. After doing the min in both directions, this OR will
                    // guarrentee capture of -0's and Nan in our tmp register
                    ctx.emit(Inst::xmm_rm_r(or_op, RegMem::from(dst), tmp_xmm1));

                    // Compare unordered to create mask for lanes containing NaNs and then use
                    // that mask to saturate the NaN containing lanes in the tmp register with 1s.
                    // TODO: Would a check for NaN and then a jump be better here in the
                    // common case than continuing on to normalize NaNs that might not exist?
                    let cond = FcmpImm::from(FloatCC::Unordered);
                    ctx.emit(Inst::xmm_rm_r_imm(
                        cmp_op,
                        RegMem::reg(tmp_xmm1.to_reg()),
                        dst,
                        cond.encode(),
                        OperandSize::Size32,
                    ));
                    ctx.emit(Inst::xmm_rm_r(or_op, RegMem::reg(dst.to_reg()), tmp_xmm1));

                    // The dst register holds a mask for lanes containing NaNs.
                    // We take that mask and shift in preparation for creating a different mask
                    // to normalize NaNs (create a quite NaN) by zeroing out the appropriate
                    // number of least signficant bits. We shift right each lane by 10 bits
                    // (1 sign + 8 exp. + 1 MSB sig.) for F32X4 and by 13 bits (1 sign +
                    // 11 exp. + 1 MSB sig.) for F64X2.
                    ctx.emit(Inst::xmm_rmi_reg(shift_op, RegMemImm::imm(shift_by), dst));

                    // Finally we do a nand with the tmp register to produce the final results
                    // in the dst.
                    ctx.emit(Inst::xmm_rm_r(andn_op, RegMem::reg(tmp_xmm1.to_reg()), dst));
                } else {
                    let (
                        mov_op,
                        max_op,
                        xor_op,
                        or_op,
                        sub_op,
                        cmp_op,
                        shift_op,
                        shift_by,
                        andn_op,
                    ) = match output_ty {
                        types::F32X4 => (
                            SseOpcode::Movaps,
                            SseOpcode::Maxps,
                            SseOpcode::Xorps,
                            SseOpcode::Orps,
                            SseOpcode::Subps,
                            SseOpcode::Cmpps,
                            SseOpcode::Psrld,
                            10,
                            SseOpcode::Andnps,
                        ),
                        types::F64X2 => (
                            SseOpcode::Movapd,
                            SseOpcode::Maxpd,
                            SseOpcode::Xorpd,
                            SseOpcode::Orpd,
                            SseOpcode::Subpd,
                            SseOpcode::Cmppd,
                            SseOpcode::Psrlq,
                            13,
                            SseOpcode::Andnpd,
                        ),
                        _ => unimplemented!("unsupported op type {:?}", output_ty),
                    };

                    // Copy lhs into tmp.
                    let tmp_xmm1 = ctx.alloc_tmp(types::F32).only_reg().unwrap();
                    ctx.emit(Inst::xmm_mov(mov_op, RegMem::reg(lhs), tmp_xmm1));

                    // Perform max in reverse direction.
                    ctx.emit(Inst::xmm_rm_r(max_op, RegMem::reg(dst.to_reg()), tmp_xmm1));

                    // Perform max in original direction.
                    ctx.emit(Inst::xmm_rm_r(max_op, RegMem::reg(lhs), dst));

                    // Get the difference between the two results and store in tmp.
                    // Max uses a different approach than min to account for potential
                    // discrepancies with plus/minus 0.
                    ctx.emit(Inst::xmm_rm_r(xor_op, RegMem::reg(tmp_xmm1.to_reg()), dst));

                    // X64 handles propagation of -0's and Nans differently between left and right
                    // operands. After doing the max in both directions, this OR will
                    // guarentee capture of 0's and Nan in our tmp register.
                    ctx.emit(Inst::xmm_rm_r(or_op, RegMem::reg(dst.to_reg()), tmp_xmm1));

                    // Capture NaNs and sign discrepancies.
                    ctx.emit(Inst::xmm_rm_r(sub_op, RegMem::reg(dst.to_reg()), tmp_xmm1));

                    // Compare unordered to create mask for lanes containing NaNs and then use
                    // that mask to saturate the NaN containing lanes in the tmp register with 1s.
                    let cond = FcmpImm::from(FloatCC::Unordered);
                    ctx.emit(Inst::xmm_rm_r_imm(
                        cmp_op,
                        RegMem::reg(tmp_xmm1.to_reg()),
                        dst,
                        cond.encode(),
                        OperandSize::Size32,
                    ));

                    // The dst register holds a mask for lanes containing NaNs.
                    // We take that mask and shift in preparation for creating a different mask
                    // to normalize NaNs (create a quite NaN) by zeroing out the appropriate
                    // number of least signficant bits. We shift right each lane by 10 bits
                    // (1 sign + 8 exp. + 1 MSB sig.) for F32X4 and by 13 bits (1 sign +
                    // 11 exp. + 1 MSB sig.) for F64X2.
                    ctx.emit(Inst::xmm_rmi_reg(shift_op, RegMemImm::imm(shift_by), dst));

                    // Finally we do a nand with the tmp register to produce the final results
                    // in the dst.
                    ctx.emit(Inst::xmm_rm_r(andn_op, RegMem::reg(tmp_xmm1.to_reg()), dst));
                }
            }
        }

        Opcode::FminPseudo | Opcode::FmaxPseudo => {
            // We can't guarantee the RHS (if a load) is 128-bit aligned, so we
            // must avoid merging a load here.
            let lhs = RegMem::reg(put_input_in_reg(ctx, inputs[0]));
            let rhs = put_input_in_reg(ctx, inputs[1]);
            let dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let ty = ty.unwrap();
            ctx.emit(Inst::gen_move(dst, rhs, ty));
            let sse_opcode = match (ty, op) {
                (types::F32X4, Opcode::FminPseudo) => SseOpcode::Minps,
                (types::F32X4, Opcode::FmaxPseudo) => SseOpcode::Maxps,
                (types::F64X2, Opcode::FminPseudo) => SseOpcode::Minpd,
                (types::F64X2, Opcode::FmaxPseudo) => SseOpcode::Maxpd,
                _ => unimplemented!("unsupported type {} for {}", ty, op),
            };
            ctx.emit(Inst::xmm_rm_r(sse_opcode, lhs, dst));
        }

        Opcode::Sqrt => {
            // We can't guarantee the RHS (if a load) is 128-bit aligned, so we
            // must avoid merging a load here.
            let src = RegMem::reg(put_input_in_reg(ctx, inputs[0]));
            let dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let ty = ty.unwrap();

            let sse_op = match ty {
                types::F32 => SseOpcode::Sqrtss,
                types::F64 => SseOpcode::Sqrtsd,
                types::F32X4 => SseOpcode::Sqrtps,
                types::F64X2 => SseOpcode::Sqrtpd,
                _ => panic!(
                    "invalid type: expected one of [F32, F64, F32X4, F64X2], found {}",
                    ty
                ),
            };

            ctx.emit(Inst::xmm_unary_rm_r(sse_op, src, dst));
        }

        Opcode::Fpromote => {
            // We can't guarantee the RHS (if a load) is 128-bit aligned, so we
            // must avoid merging a load here.
            let src = RegMem::reg(put_input_in_reg(ctx, inputs[0]));
            let dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            ctx.emit(Inst::xmm_unary_rm_r(SseOpcode::Cvtss2sd, src, dst));
        }

        Opcode::FvpromoteLow => {
            let src = RegMem::reg(put_input_in_reg(ctx, inputs[0]));
            let dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            ctx.emit(Inst::xmm_unary_rm_r(
                SseOpcode::Cvtps2pd,
                RegMem::from(src),
                dst,
            ));
        }

        Opcode::Fdemote => {
            // We can't guarantee the RHS (if a load) is 128-bit aligned, so we
            // must avoid merging a load here.
            let src = RegMem::reg(put_input_in_reg(ctx, inputs[0]));
            let dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            ctx.emit(Inst::xmm_unary_rm_r(SseOpcode::Cvtsd2ss, src, dst));
        }

        Opcode::Fvdemote => {
            let src = RegMem::reg(put_input_in_reg(ctx, inputs[0]));
            let dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            ctx.emit(Inst::xmm_unary_rm_r(
                SseOpcode::Cvtpd2ps,
                RegMem::from(src),
                dst,
            ));
        }

        Opcode::FcvtFromSint => {
            let output_ty = ty.unwrap();
            if !output_ty.is_vector() {
                let (ext_spec, src_size) = match ctx.input_ty(insn, 0) {
                    types::I8 | types::I16 => (Some(ExtSpec::SignExtendTo32), OperandSize::Size32),
                    types::I32 => (None, OperandSize::Size32),
                    types::I64 => (None, OperandSize::Size64),
                    _ => unreachable!(),
                };

                let src = match ext_spec {
                    Some(ext_spec) => RegMem::reg(extend_input_to_reg(ctx, inputs[0], ext_spec)),
                    None => RegMem::reg(put_input_in_reg(ctx, inputs[0])),
                };

                let opcode = if output_ty == types::F32 {
                    SseOpcode::Cvtsi2ss
                } else {
                    assert_eq!(output_ty, types::F64);
                    SseOpcode::Cvtsi2sd
                };
                let dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
                ctx.emit(Inst::gpr_to_xmm(opcode, src, src_size, dst));
            } else {
                let ty = ty.unwrap();
                let src = put_input_in_reg(ctx, inputs[0]);
                let dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
                let opcode = match ctx.input_ty(insn, 0) {
                    types::I32X4 => SseOpcode::Cvtdq2ps,
                    _ => {
                        unimplemented!("unable to use type {} for op {}", ctx.input_ty(insn, 0), op)
                    }
                };
                ctx.emit(Inst::gen_move(dst, src, ty));
                ctx.emit(Inst::xmm_rm_r(opcode, RegMem::from(dst), dst));
            }
        }
        Opcode::FcvtLowFromSint => {
            let src = RegMem::reg(put_input_in_reg(ctx, inputs[0]));
            let dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            ctx.emit(Inst::xmm_unary_rm_r(
                SseOpcode::Cvtdq2pd,
                RegMem::from(src),
                dst,
            ));
        }
        Opcode::FcvtFromUint => {
            let dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let ty = ty.unwrap();

            let input_ty = ctx.input_ty(insn, 0);
            if !ty.is_vector() {
                match input_ty {
                    types::I8 | types::I16 | types::I32 => {
                        // Conversion from an unsigned int smaller than 64-bit is easy: zero-extend +
                        // do a signed conversion (which won't overflow).
                        let opcode = if ty == types::F32 {
                            SseOpcode::Cvtsi2ss
                        } else {
                            assert_eq!(ty, types::F64);
                            SseOpcode::Cvtsi2sd
                        };

                        let src = RegMem::reg(extend_input_to_reg(
                            ctx,
                            inputs[0],
                            ExtSpec::ZeroExtendTo64,
                        ));
                        ctx.emit(Inst::gpr_to_xmm(opcode, src, OperandSize::Size64, dst));
                    }

                    types::I64 => {
                        let src = put_input_in_reg(ctx, inputs[0]);

                        let src_copy = ctx.alloc_tmp(types::I64).only_reg().unwrap();
                        ctx.emit(Inst::gen_move(src_copy, src, types::I64));

                        let tmp_gpr1 = ctx.alloc_tmp(types::I64).only_reg().unwrap();
                        let tmp_gpr2 = ctx.alloc_tmp(types::I64).only_reg().unwrap();
                        ctx.emit(Inst::cvt_u64_to_float_seq(
                            if ty == types::F64 {
                                OperandSize::Size64
                            } else {
                                OperandSize::Size32
                            },
                            src_copy,
                            tmp_gpr1,
                            tmp_gpr2,
                            dst,
                        ));
                    }
                    _ => panic!("unexpected input type for FcvtFromUint: {:?}", input_ty),
                };
            } else if let Some(uwiden) = matches_input(ctx, inputs[0], Opcode::UwidenLow) {
                let uwiden_input = InsnInput {
                    insn: uwiden,
                    input: 0,
                };
                let src = put_input_in_reg(ctx, uwiden_input);
                let dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
                let input_ty = ctx.input_ty(uwiden, 0);
                let output_ty = ctx.output_ty(insn, 0);

                // Matches_input further obfuscates which Wasm instruction this is ultimately
                // lowering. Check here that the types are as expected for F64x2ConvertLowI32x4U.
                debug_assert!(input_ty == types::I32X4 || output_ty == types::F64X2);

                // Algorithm uses unpcklps to help create a float that is equivalent
                // 0x1.0p52 + double(src). 0x1.0p52 is unique because at this exponent
                // every value of the mantissa represents a corresponding uint32 number.
                // When we subtract 0x1.0p52 we are left with double(src).
                let uint_mask = ctx.alloc_tmp(types::I32X4).only_reg().unwrap();
                ctx.emit(Inst::gen_move(dst, src, types::I32X4));

                static UINT_MASK: [u8; 16] = [
                    0x00, 0x00, 0x30, 0x43, 0x00, 0x00, 0x30, 0x43, 0x00, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x00, 0x00,
                ];

                let uint_mask_const = ctx.use_constant(VCodeConstantData::WellKnown(&UINT_MASK));

                ctx.emit(Inst::xmm_load_const(
                    uint_mask_const,
                    uint_mask,
                    types::I32X4,
                ));

                // Creates 0x1.0p52 + double(src)
                ctx.emit(Inst::xmm_rm_r(
                    SseOpcode::Unpcklps,
                    RegMem::from(uint_mask),
                    dst,
                ));

                static UINT_MASK_HIGH: [u8; 16] = [
                    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x30, 0x43, 0x00, 0x00, 0x00, 0x00, 0x00,
                    0x00, 0x30, 0x43,
                ];

                let uint_mask_high_const =
                    ctx.use_constant(VCodeConstantData::WellKnown(&UINT_MASK_HIGH));
                let uint_mask_high = ctx.alloc_tmp(types::I32X4).only_reg().unwrap();
                ctx.emit(Inst::xmm_load_const(
                    uint_mask_high_const,
                    uint_mask_high,
                    types::I32X4,
                ));

                // 0x1.0p52 + double(src) - 0x1.0p52
                ctx.emit(Inst::xmm_rm_r(
                    SseOpcode::Subpd,
                    RegMem::from(uint_mask_high),
                    dst,
                ));
            } else {
                assert_eq!(ctx.input_ty(insn, 0), types::I32X4);
                let src = put_input_in_reg(ctx, inputs[0]);
                let dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();

                if isa_flags.use_avx512vl_simd() && isa_flags.use_avx512f_simd() {
                    // When AVX512VL and AVX512F are available,
                    // `fcvt_from_uint` can be lowered to a single instruction.
                    ctx.emit(Inst::xmm_unary_rm_r_evex(
                        Avx512Opcode::Vcvtudq2ps,
                        RegMem::reg(src),
                        dst,
                    ));
                } else {
                    // Converting packed unsigned integers to packed floats
                    // requires a few steps. There is no single instruction
                    // lowering for converting unsigned floats but there is for
                    // converting packed signed integers to float (cvtdq2ps). In
                    // the steps below we isolate the upper half (16 bits) and
                    // lower half (16 bits) of each lane and then we convert
                    // each half separately using cvtdq2ps meant for signed
                    // integers. In order for this to work for the upper half
                    // bits we must shift right by 1 (divide by 2) these bits in
                    // order to ensure the most significant bit is 0 not signed,
                    // and then after the conversion we double the value.
                    // Finally we add the converted values where addition will
                    // correctly round.
                    //
                    // Sequence:
                    // -> A = 0xffffffff
                    // -> Ah = 0xffff0000
                    // -> Al = 0x0000ffff
                    // -> Convert(Al) // Convert int to float
                    // -> Ah = Ah >> 1 // Shift right 1 to assure Ah conversion isn't treated as signed
                    // -> Convert(Ah) // Convert .. with no loss of significant digits from previous shift
                    // -> Ah = Ah + Ah // Double Ah to account for shift right before the conversion.
                    // -> dst = Ah + Al // Add the two floats together

                    // Create a temporary register
                    let tmp = ctx.alloc_tmp(types::I32X4).only_reg().unwrap();
                    ctx.emit(Inst::xmm_unary_rm_r(
                        SseOpcode::Movapd,
                        RegMem::reg(src),
                        tmp,
                    ));
                    ctx.emit(Inst::gen_move(dst, src, ty));

                    // Get the low 16 bits
                    ctx.emit(Inst::xmm_rmi_reg(SseOpcode::Pslld, RegMemImm::imm(16), tmp));
                    ctx.emit(Inst::xmm_rmi_reg(SseOpcode::Psrld, RegMemImm::imm(16), tmp));

                    // Get the high 16 bits
                    ctx.emit(Inst::xmm_rm_r(SseOpcode::Psubd, RegMem::from(tmp), dst));

                    // Convert the low 16 bits
                    ctx.emit(Inst::xmm_rm_r(SseOpcode::Cvtdq2ps, RegMem::from(tmp), tmp));

                    // Shift the high bits by 1, convert, and double to get the correct value.
                    ctx.emit(Inst::xmm_rmi_reg(SseOpcode::Psrld, RegMemImm::imm(1), dst));
                    ctx.emit(Inst::xmm_rm_r(SseOpcode::Cvtdq2ps, RegMem::from(dst), dst));
                    ctx.emit(Inst::xmm_rm_r(
                        SseOpcode::Addps,
                        RegMem::reg(dst.to_reg()),
                        dst,
                    ));

                    // Add together the two converted values.
                    ctx.emit(Inst::xmm_rm_r(
                        SseOpcode::Addps,
                        RegMem::reg(tmp.to_reg()),
                        dst,
                    ));
                }
            }
        }

        Opcode::FcvtToUint | Opcode::FcvtToUintSat | Opcode::FcvtToSint | Opcode::FcvtToSintSat => {
            let src = put_input_in_reg(ctx, inputs[0]);
            let dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();

            let input_ty = ctx.input_ty(insn, 0);
            if !input_ty.is_vector() {
                let src_size = if input_ty == types::F32 {
                    OperandSize::Size32
                } else {
                    assert_eq!(input_ty, types::F64);
                    OperandSize::Size64
                };

                let output_ty = ty.unwrap();
                let dst_size = if output_ty == types::I32 {
                    OperandSize::Size32
                } else {
                    assert_eq!(output_ty, types::I64);
                    OperandSize::Size64
                };

                let to_signed = op == Opcode::FcvtToSint || op == Opcode::FcvtToSintSat;
                let is_sat = op == Opcode::FcvtToUintSat || op == Opcode::FcvtToSintSat;

                let src_copy = ctx.alloc_tmp(input_ty).only_reg().unwrap();
                ctx.emit(Inst::gen_move(src_copy, src, input_ty));

                let tmp_xmm = ctx.alloc_tmp(input_ty).only_reg().unwrap();
                let tmp_gpr = ctx.alloc_tmp(output_ty).only_reg().unwrap();

                if to_signed {
                    ctx.emit(Inst::cvt_float_to_sint_seq(
                        src_size, dst_size, is_sat, src_copy, dst, tmp_gpr, tmp_xmm,
                    ));
                } else {
                    ctx.emit(Inst::cvt_float_to_uint_seq(
                        src_size, dst_size, is_sat, src_copy, dst, tmp_gpr, tmp_xmm,
                    ));
                }
            } else {
                if op == Opcode::FcvtToSintSat {
                    // Sets destination to zero if float is NaN
                    assert_eq!(types::F32X4, ctx.input_ty(insn, 0));
                    let tmp = ctx.alloc_tmp(types::I32X4).only_reg().unwrap();
                    ctx.emit(Inst::xmm_unary_rm_r(
                        SseOpcode::Movapd,
                        RegMem::reg(src),
                        tmp,
                    ));
                    ctx.emit(Inst::gen_move(dst, src, input_ty));
                    let cond = FcmpImm::from(FloatCC::Equal);
                    ctx.emit(Inst::xmm_rm_r_imm(
                        SseOpcode::Cmpps,
                        RegMem::reg(tmp.to_reg()),
                        tmp,
                        cond.encode(),
                        OperandSize::Size32,
                    ));
                    ctx.emit(Inst::xmm_rm_r(
                        SseOpcode::Andps,
                        RegMem::reg(tmp.to_reg()),
                        dst,
                    ));

                    // Sets top bit of tmp if float is positive
                    // Setting up to set top bit on negative float values
                    ctx.emit(Inst::xmm_rm_r(
                        SseOpcode::Pxor,
                        RegMem::reg(dst.to_reg()),
                        tmp,
                    ));

                    // Convert the packed float to packed doubleword.
                    ctx.emit(Inst::xmm_rm_r(
                        SseOpcode::Cvttps2dq,
                        RegMem::reg(dst.to_reg()),
                        dst,
                    ));

                    // Set top bit only if < 0
                    // Saturate lane with sign (top) bit.
                    ctx.emit(Inst::xmm_rm_r(
                        SseOpcode::Pand,
                        RegMem::reg(dst.to_reg()),
                        tmp,
                    ));
                    ctx.emit(Inst::xmm_rmi_reg(SseOpcode::Psrad, RegMemImm::imm(31), tmp));

                    // On overflow 0x80000000 is returned to a lane.
                    // Below sets positive overflow lanes to 0x7FFFFFFF
                    // Keeps negative overflow lanes as is.
                    ctx.emit(Inst::xmm_rm_r(
                        SseOpcode::Pxor,
                        RegMem::reg(tmp.to_reg()),
                        dst,
                    ));
                } else if op == Opcode::FcvtToUintSat {
                    // The algorithm for converting floats to unsigned ints is a little tricky. The
                    // complication arises because we are converting from a signed 64-bit int with a positive
                    // integer range from 1..INT_MAX (0x1..0x7FFFFFFF) to an unsigned integer with an extended
                    // range from (INT_MAX+1)..UINT_MAX. It's this range from (INT_MAX+1)..UINT_MAX
                    // (0x80000000..0xFFFFFFFF) that needs to be accounted for as a special case since our
                    // conversion instruction (cvttps2dq) only converts as high as INT_MAX (0x7FFFFFFF), but
                    // which conveniently setting underflows and overflows (smaller than MIN_INT or larger than
                    // MAX_INT) to be INT_MAX+1 (0x80000000). Nothing that the range (INT_MAX+1)..UINT_MAX includes
                    // precisely INT_MAX values we can correctly account for and convert every value in this range
                    // if we simply subtract INT_MAX+1 before doing the cvttps2dq conversion. After the subtraction
                    // every value originally (INT_MAX+1)..UINT_MAX is now the range (0..INT_MAX).
                    // After the conversion we add INT_MAX+1 back to this converted value, noting again that
                    // values we are trying to account for were already set to INT_MAX+1 during the original conversion.
                    // We simply have to create a mask and make sure we are adding together only the lanes that need
                    // to be accounted for. Digesting it all the steps then are:
                    //
                    // Step 1 - Account for NaN and negative floats by setting these src values to zero.
                    // Step 2 - Make a copy (tmp1) of the src value since we need to convert twice for
                    //          reasons described above.
                    // Step 3 - Convert the original src values. This will convert properly all floats up to INT_MAX
                    // Step 4 - Subtract INT_MAX from the copy set (tmp1). Note, all zero and negative values are those
                    //          values that were originally in the range (0..INT_MAX). This will come in handy during
                    //          step 7 when we zero negative lanes.
                    // Step 5 - Create a bit mask for tmp1 that will correspond to all lanes originally less than
                    //          UINT_MAX that are now less than INT_MAX thanks to the subtraction.
                    // Step 6 - Convert the second set of values (tmp1)
                    // Step 7 - Prep the converted second set by zeroing out negative lanes (these have already been
                    //          converted correctly with the first set) and by setting overflow lanes to 0x7FFFFFFF
                    //          as this will allow us to properly saturate overflow lanes when adding to 0x80000000
                    // Step 8 - Add the orginal converted src and the converted tmp1 where float values originally less
                    //          than and equal to INT_MAX will be unchanged, float values originally between INT_MAX+1 and
                    //          UINT_MAX will add together (INT_MAX) + (SRC - INT_MAX), and float values originally
                    //          greater than UINT_MAX will be saturated to UINT_MAX (0xFFFFFFFF) after adding (0x8000000 + 0x7FFFFFFF).
                    //
                    //
                    // The table below illustrates the result after each step where it matters for the converted set.
                    // Note the original value range (original src set) is the final dst in Step 8:
                    //
                    // Original src set:
                    // | Original Value Range |    Step 1    |         Step 3         |          Step 8           |
                    // |  -FLT_MIN..FLT_MAX   | 0.0..FLT_MAX | 0..INT_MAX(w/overflow) | 0..UINT_MAX(w/saturation) |
                    //
                    // Copied src set (tmp1):
                    // |    Step 2    |                  Step 4                  |
                    // | 0.0..FLT_MAX | (0.0-(INT_MAX+1))..(FLT_MAX-(INT_MAX+1)) |
                    //
                    // |                       Step 6                        |                 Step 7                 |
                    // | (0-(INT_MAX+1))..(UINT_MAX-(INT_MAX+1))(w/overflow) | ((INT_MAX+1)-(INT_MAX+1))..(INT_MAX+1) |

                    // Create temporaries
                    assert_eq!(types::F32X4, ctx.input_ty(insn, 0));
                    let tmp1 = ctx.alloc_tmp(types::I32X4).only_reg().unwrap();
                    let tmp2 = ctx.alloc_tmp(types::I32X4).only_reg().unwrap();

                    // Converting to unsigned int so if float src is negative or NaN
                    // will first set to zero.
                    ctx.emit(Inst::xmm_rm_r(SseOpcode::Pxor, RegMem::from(tmp2), tmp2));
                    ctx.emit(Inst::gen_move(dst, src, input_ty));
                    ctx.emit(Inst::xmm_rm_r(SseOpcode::Maxps, RegMem::from(tmp2), dst));

                    // Set tmp2 to INT_MAX+1. It is important to note here that after it looks
                    // like we are only converting INT_MAX (0x7FFFFFFF) but in fact because
                    // single precision IEEE-754 floats can only accurately represent contingous
                    // integers up to 2^23 and outside of this range it rounds to the closest
                    // integer that it can represent. In the case of INT_MAX, this value gets
                    // represented as 0x4f000000 which is the integer value (INT_MAX+1).

                    ctx.emit(Inst::xmm_rm_r(SseOpcode::Pcmpeqd, RegMem::from(tmp2), tmp2));
                    ctx.emit(Inst::xmm_rmi_reg(SseOpcode::Psrld, RegMemImm::imm(1), tmp2));
                    ctx.emit(Inst::xmm_rm_r(
                        SseOpcode::Cvtdq2ps,
                        RegMem::from(tmp2),
                        tmp2,
                    ));

                    // Make a copy of these lanes and then do the first conversion.
                    // Overflow lanes greater than the maximum allowed signed value will
                    // set to 0x80000000. Negative and NaN lanes will be 0x0
                    ctx.emit(Inst::xmm_mov(SseOpcode::Movaps, RegMem::from(dst), tmp1));
                    ctx.emit(Inst::xmm_rm_r(SseOpcode::Cvttps2dq, RegMem::from(dst), dst));

                    // Set lanes to src - max_signed_int
                    ctx.emit(Inst::xmm_rm_r(SseOpcode::Subps, RegMem::from(tmp2), tmp1));

                    // Create mask for all positive lanes to saturate (i.e. greater than
                    // or equal to the maxmimum allowable unsigned int).
                    let cond = FcmpImm::from(FloatCC::LessThanOrEqual);
                    ctx.emit(Inst::xmm_rm_r_imm(
                        SseOpcode::Cmpps,
                        RegMem::from(tmp1),
                        tmp2,
                        cond.encode(),
                        OperandSize::Size32,
                    ));

                    // Convert those set of lanes that have the max_signed_int factored out.
                    ctx.emit(Inst::xmm_rm_r(
                        SseOpcode::Cvttps2dq,
                        RegMem::from(tmp1),
                        tmp1,
                    ));

                    // Prepare converted lanes by zeroing negative lanes and prepping lanes
                    // that have positive overflow (based on the mask) by setting these lanes
                    // to 0x7FFFFFFF
                    ctx.emit(Inst::xmm_rm_r(SseOpcode::Pxor, RegMem::from(tmp2), tmp1));
                    ctx.emit(Inst::xmm_rm_r(SseOpcode::Pxor, RegMem::from(tmp2), tmp2));
                    ctx.emit(Inst::xmm_rm_r(SseOpcode::Pmaxsd, RegMem::from(tmp2), tmp1));

                    // Add this second set of converted lanes to the original to properly handle
                    // values greater than max signed int.
                    ctx.emit(Inst::xmm_rm_r(SseOpcode::Paddd, RegMem::from(tmp1), dst));
                } else {
                    // Since this branch is also guarded by a check for vector types
                    // neither Opcode::FcvtToUint nor Opcode::FcvtToSint can reach here
                    // due to vector varients not existing. The first two branches will
                    // cover all reachable cases.
                    unreachable!();
                }
            }
        }
        Opcode::UwidenHigh | Opcode::UwidenLow | Opcode::SwidenHigh | Opcode::SwidenLow => {
            let input_ty = ctx.input_ty(insn, 0);
            let output_ty = ctx.output_ty(insn, 0);
            let src = put_input_in_reg(ctx, inputs[0]);
            let dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            if output_ty.is_vector() {
                match op {
                    Opcode::SwidenLow => match (input_ty, output_ty) {
                        (types::I8X16, types::I16X8) => {
                            ctx.emit(Inst::xmm_mov(SseOpcode::Pmovsxbw, RegMem::reg(src), dst));
                        }
                        (types::I16X8, types::I32X4) => {
                            ctx.emit(Inst::xmm_mov(SseOpcode::Pmovsxwd, RegMem::reg(src), dst));
                        }
                        _ => unreachable!(),
                    },
                    Opcode::SwidenHigh => match (input_ty, output_ty) {
                        (types::I8X16, types::I16X8) => {
                            ctx.emit(Inst::gen_move(dst, src, output_ty));
                            ctx.emit(Inst::xmm_rm_r_imm(
                                SseOpcode::Palignr,
                                RegMem::reg(src),
                                dst,
                                8,
                                OperandSize::Size32,
                            ));
                            ctx.emit(Inst::xmm_mov(SseOpcode::Pmovsxbw, RegMem::from(dst), dst));
                        }
                        (types::I16X8, types::I32X4) => {
                            ctx.emit(Inst::gen_move(dst, src, output_ty));
                            ctx.emit(Inst::xmm_rm_r_imm(
                                SseOpcode::Palignr,
                                RegMem::reg(src),
                                dst,
                                8,
                                OperandSize::Size32,
                            ));
                            ctx.emit(Inst::xmm_mov(SseOpcode::Pmovsxwd, RegMem::from(dst), dst));
                        }
                        _ => unreachable!(),
                    },
                    Opcode::UwidenLow => match (input_ty, output_ty) {
                        (types::I8X16, types::I16X8) => {
                            ctx.emit(Inst::xmm_mov(SseOpcode::Pmovzxbw, RegMem::reg(src), dst));
                        }
                        (types::I16X8, types::I32X4) => {
                            ctx.emit(Inst::xmm_mov(SseOpcode::Pmovzxwd, RegMem::reg(src), dst));
                        }
                        _ => unreachable!(
                            "In UwidenLow: input_ty {:?}, output_ty {:?}",
                            input_ty, output_ty
                        ),
                    },
                    Opcode::UwidenHigh => match (input_ty, output_ty) {
                        (types::I8X16, types::I16X8) => {
                            ctx.emit(Inst::gen_move(dst, src, output_ty));
                            ctx.emit(Inst::xmm_rm_r_imm(
                                SseOpcode::Palignr,
                                RegMem::reg(src),
                                dst,
                                8,
                                OperandSize::Size32,
                            ));
                            ctx.emit(Inst::xmm_mov(SseOpcode::Pmovzxbw, RegMem::from(dst), dst));
                        }
                        (types::I16X8, types::I32X4) => {
                            ctx.emit(Inst::gen_move(dst, src, output_ty));
                            ctx.emit(Inst::xmm_rm_r_imm(
                                SseOpcode::Palignr,
                                RegMem::reg(src),
                                dst,
                                8,
                                OperandSize::Size32,
                            ));
                            ctx.emit(Inst::xmm_mov(SseOpcode::Pmovzxwd, RegMem::from(dst), dst));
                        }
                        _ => unreachable!(),
                    },
                    _ => unreachable!(),
                }
            } else {
                panic!("Unsupported non-vector type for widen instruction {:?}", ty);
            }
        }
        Opcode::Snarrow | Opcode::Unarrow => {
            let input_ty = ctx.input_ty(insn, 0);
            let output_ty = ctx.output_ty(insn, 0);
            let src1 = put_input_in_reg(ctx, inputs[0]);
            let src2 = put_input_in_reg(ctx, inputs[1]);
            let dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            if output_ty.is_vector() {
                match op {
                    Opcode::Snarrow => match (input_ty, output_ty) {
                        (types::I16X8, types::I8X16) => {
                            ctx.emit(Inst::gen_move(dst, src1, input_ty));
                            ctx.emit(Inst::xmm_rm_r(SseOpcode::Packsswb, RegMem::reg(src2), dst));
                        }
                        (types::I32X4, types::I16X8) => {
                            ctx.emit(Inst::gen_move(dst, src1, input_ty));
                            ctx.emit(Inst::xmm_rm_r(SseOpcode::Packssdw, RegMem::reg(src2), dst));
                        }
                        _ => unreachable!(),
                    },
                    Opcode::Unarrow => match (input_ty, output_ty) {
                        (types::I16X8, types::I8X16) => {
                            ctx.emit(Inst::gen_move(dst, src1, input_ty));
                            ctx.emit(Inst::xmm_rm_r(SseOpcode::Packuswb, RegMem::reg(src2), dst));
                        }
                        (types::I32X4, types::I16X8) => {
                            ctx.emit(Inst::gen_move(dst, src1, input_ty));
                            ctx.emit(Inst::xmm_rm_r(SseOpcode::Packusdw, RegMem::reg(src2), dst));
                        }
                        _ => unreachable!(),
                    },
                    _ => unreachable!(),
                }
            } else {
                panic!("Unsupported non-vector type for widen instruction {:?}", ty);
            }
        }
        Opcode::Bitcast => {
            let input_ty = ctx.input_ty(insn, 0);
            let output_ty = ctx.output_ty(insn, 0);
            match (input_ty, output_ty) {
                (types::F32, types::I32) => {
                    let src = put_input_in_reg(ctx, inputs[0]);
                    let dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
                    ctx.emit(Inst::xmm_to_gpr(
                        SseOpcode::Movd,
                        src,
                        dst,
                        OperandSize::Size32,
                    ));
                }
                (types::I32, types::F32) => {
                    let src = input_to_reg_mem(ctx, inputs[0]);
                    let dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
                    ctx.emit(Inst::gpr_to_xmm(
                        SseOpcode::Movd,
                        src,
                        OperandSize::Size32,
                        dst,
                    ));
                }
                (types::F64, types::I64) => {
                    let src = put_input_in_reg(ctx, inputs[0]);
                    let dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
                    ctx.emit(Inst::xmm_to_gpr(
                        SseOpcode::Movq,
                        src,
                        dst,
                        OperandSize::Size64,
                    ));
                }
                (types::I64, types::F64) => {
                    let src = input_to_reg_mem(ctx, inputs[0]);
                    let dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
                    ctx.emit(Inst::gpr_to_xmm(
                        SseOpcode::Movq,
                        src,
                        OperandSize::Size64,
                        dst,
                    ));
                }
                _ => unreachable!("invalid bitcast from {:?} to {:?}", input_ty, output_ty),
            }
        }

        Opcode::Fabs | Opcode::Fneg => {
            let src = RegMem::reg(put_input_in_reg(ctx, inputs[0]));
            let dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();

            // In both cases, generate a constant and apply a single binary instruction:
            // - to compute the absolute value, set all bits to 1 but the MSB to 0, and bit-AND the
            // src with it.
            // - to compute the negated value, set all bits to 0 but the MSB to 1, and bit-XOR the
            // src with it.
            let output_ty = ty.unwrap();
            if !output_ty.is_vector() {
                let (val, opcode): (u64, _) = match output_ty {
                    types::F32 => match op {
                        Opcode::Fabs => (0x7fffffff, SseOpcode::Andps),
                        Opcode::Fneg => (0x80000000, SseOpcode::Xorps),
                        _ => unreachable!(),
                    },
                    types::F64 => match op {
                        Opcode::Fabs => (0x7fffffffffffffff, SseOpcode::Andpd),
                        Opcode::Fneg => (0x8000000000000000, SseOpcode::Xorpd),
                        _ => unreachable!(),
                    },
                    _ => panic!("unexpected type {:?} for Fabs", output_ty),
                };

                for inst in Inst::gen_constant(ValueRegs::one(dst), val as u128, output_ty, |ty| {
                    ctx.alloc_tmp(ty).only_reg().unwrap()
                }) {
                    ctx.emit(inst);
                }

                ctx.emit(Inst::xmm_rm_r(opcode, src, dst));
            } else {
                // Eventually vector constants should be available in `gen_constant` and this block
                // can be merged with the one above (TODO).
                if output_ty.bits() == 128 {
                    // Move the `lhs` to the same register as `dst`; this may not emit an actual move
                    // but ensures that the registers are the same to match x86's read-write operand
                    // encoding.
                    let src = put_input_in_reg(ctx, inputs[0]);
                    ctx.emit(Inst::gen_move(dst, src, output_ty));

                    // Generate an all 1s constant in an XMM register. This uses CMPPS but could
                    // have used CMPPD with the same effect. Note, we zero the temp we allocate
                    // because if not, there is a chance that the register we use could be initialized
                    // with NaN .. in which case the CMPPS would fail since NaN != NaN.
                    let tmp = ctx.alloc_tmp(output_ty).only_reg().unwrap();
                    ctx.emit(Inst::xmm_rm_r(SseOpcode::Xorps, RegMem::from(tmp), tmp));
                    let cond = FcmpImm::from(FloatCC::Equal);
                    let cmpps = Inst::xmm_rm_r_imm(
                        SseOpcode::Cmpps,
                        RegMem::reg(tmp.to_reg()),
                        tmp,
                        cond.encode(),
                        OperandSize::Size32,
                    );
                    ctx.emit(cmpps);

                    // Shift the all 1s constant to generate the mask.
                    let lane_bits = output_ty.lane_bits();
                    let (shift_opcode, opcode, shift_by) = match (op, lane_bits) {
                        (Opcode::Fabs, 32) => (SseOpcode::Psrld, SseOpcode::Andps, 1),
                        (Opcode::Fabs, 64) => (SseOpcode::Psrlq, SseOpcode::Andpd, 1),
                        (Opcode::Fneg, 32) => (SseOpcode::Pslld, SseOpcode::Xorps, 31),
                        (Opcode::Fneg, 64) => (SseOpcode::Psllq, SseOpcode::Xorpd, 63),
                        _ => unreachable!(
                            "unexpected opcode and lane size: {:?}, {} bits",
                            op, lane_bits
                        ),
                    };
                    let shift = Inst::xmm_rmi_reg(shift_opcode, RegMemImm::imm(shift_by), tmp);
                    ctx.emit(shift);

                    // Apply shifted mask (XOR or AND).
                    let mask = Inst::xmm_rm_r(opcode, RegMem::reg(tmp.to_reg()), dst);
                    ctx.emit(mask);
                } else {
                    panic!("unexpected type {:?} for Fabs", output_ty);
                }
            }
        }

        Opcode::Fcopysign => {
            let dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let lhs = put_input_in_reg(ctx, inputs[0]);
            let rhs = put_input_in_reg(ctx, inputs[1]);

            let ty = ty.unwrap();

            // We're going to generate the following sequence:
            //
            // movabs     $INT_MIN, tmp_gpr1
            // mov{d,q}   tmp_gpr1, tmp_xmm1
            // movap{s,d} tmp_xmm1, dst
            // andnp{s,d} src_1, dst
            // movap{s,d} src_2, tmp_xmm2
            // andp{s,d}  tmp_xmm1, tmp_xmm2
            // orp{s,d}   tmp_xmm2, dst

            let tmp_xmm1 = ctx.alloc_tmp(types::F32).only_reg().unwrap();
            let tmp_xmm2 = ctx.alloc_tmp(types::F32).only_reg().unwrap();

            let (sign_bit_cst, mov_op, and_not_op, and_op, or_op) = match ty {
                types::F32 => (
                    0x8000_0000,
                    SseOpcode::Movaps,
                    SseOpcode::Andnps,
                    SseOpcode::Andps,
                    SseOpcode::Orps,
                ),
                types::F64 => (
                    0x8000_0000_0000_0000,
                    SseOpcode::Movapd,
                    SseOpcode::Andnpd,
                    SseOpcode::Andpd,
                    SseOpcode::Orpd,
                ),
                _ => {
                    panic!("unexpected type {:?} for copysign", ty);
                }
            };

            for inst in Inst::gen_constant(ValueRegs::one(tmp_xmm1), sign_bit_cst, ty, |ty| {
                ctx.alloc_tmp(ty).only_reg().unwrap()
            }) {
                ctx.emit(inst);
            }
            ctx.emit(Inst::xmm_mov(mov_op, RegMem::reg(tmp_xmm1.to_reg()), dst));
            ctx.emit(Inst::xmm_rm_r(and_not_op, RegMem::reg(lhs), dst));
            ctx.emit(Inst::xmm_mov(mov_op, RegMem::reg(rhs), tmp_xmm2));
            ctx.emit(Inst::xmm_rm_r(
                and_op,
                RegMem::reg(tmp_xmm1.to_reg()),
                tmp_xmm2,
            ));
            ctx.emit(Inst::xmm_rm_r(or_op, RegMem::reg(tmp_xmm2.to_reg()), dst));
        }

        Opcode::Ceil | Opcode::Floor | Opcode::Nearest | Opcode::Trunc => {
            let ty = ty.unwrap();
            if isa_flags.use_sse41() {
                let mode = match op {
                    Opcode::Ceil => RoundImm::RoundUp,
                    Opcode::Floor => RoundImm::RoundDown,
                    Opcode::Nearest => RoundImm::RoundNearest,
                    Opcode::Trunc => RoundImm::RoundZero,
                    _ => panic!("unexpected opcode {:?} in Ceil/Floor/Nearest/Trunc", op),
                };
                let op = match ty {
                    types::F32 => SseOpcode::Roundss,
                    types::F64 => SseOpcode::Roundsd,
                    types::F32X4 => SseOpcode::Roundps,
                    types::F64X2 => SseOpcode::Roundpd,
                    _ => panic!("unexpected type {:?} in Ceil/Floor/Nearest/Trunc", ty),
                };
                let src = input_to_reg_mem(ctx, inputs[0]);
                let dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
                ctx.emit(Inst::xmm_rm_r_imm(
                    op,
                    src,
                    dst,
                    mode.encode(),
                    OperandSize::Size32,
                ));
            } else {
                // Lower to VM calls when there's no access to SSE4.1.
                // Note, for vector types on platforms that don't support sse41
                // the execution will panic here.
                let libcall = match (op, ty) {
                    (Opcode::Ceil, types::F32) => LibCall::CeilF32,
                    (Opcode::Ceil, types::F64) => LibCall::CeilF64,
                    (Opcode::Floor, types::F32) => LibCall::FloorF32,
                    (Opcode::Floor, types::F64) => LibCall::FloorF64,
                    (Opcode::Nearest, types::F32) => LibCall::NearestF32,
                    (Opcode::Nearest, types::F64) => LibCall::NearestF64,
                    (Opcode::Trunc, types::F32) => LibCall::TruncF32,
                    (Opcode::Trunc, types::F64) => LibCall::TruncF64,
                    _ => panic!(
                        "unexpected type/opcode {:?}/{:?} in Ceil/Floor/Nearest/Trunc",
                        ty, op
                    ),
                };
                emit_vm_call(ctx, flags, triple, libcall, insn, inputs, outputs)?;
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
        | Opcode::Uload32x2 => {
            let offset = ctx.data(insn).load_store_offset().unwrap();

            let elem_ty = match op {
                Opcode::Sload8 | Opcode::Uload8 | Opcode::Sload8Complex | Opcode::Uload8Complex => {
                    types::I8
                }
                Opcode::Sload16
                | Opcode::Uload16
                | Opcode::Sload16Complex
                | Opcode::Uload16Complex => types::I16,
                Opcode::Sload32
                | Opcode::Uload32
                | Opcode::Sload32Complex
                | Opcode::Uload32Complex => types::I32,
                Opcode::Sload8x8
                | Opcode::Uload8x8
                | Opcode::Sload8x8Complex
                | Opcode::Uload8x8Complex => types::I8X8,
                Opcode::Sload16x4
                | Opcode::Uload16x4
                | Opcode::Sload16x4Complex
                | Opcode::Uload16x4Complex => types::I16X4,
                Opcode::Sload32x2
                | Opcode::Uload32x2
                | Opcode::Sload32x2Complex
                | Opcode::Uload32x2Complex => types::I32X2,
                Opcode::Load | Opcode::LoadComplex => ctx.output_ty(insn, 0),
                _ => unimplemented!(),
            };

            let ext_mode = ExtMode::new(elem_ty.bits(), 64);

            let sign_extend = match op {
                Opcode::Sload8
                | Opcode::Sload8Complex
                | Opcode::Sload16
                | Opcode::Sload16Complex
                | Opcode::Sload32
                | Opcode::Sload32Complex
                | Opcode::Sload8x8
                | Opcode::Sload8x8Complex
                | Opcode::Sload16x4
                | Opcode::Sload16x4Complex
                | Opcode::Sload32x2
                | Opcode::Sload32x2Complex => true,
                _ => false,
            };

            let amode = match op {
                Opcode::Load
                | Opcode::Uload8
                | Opcode::Sload8
                | Opcode::Uload16
                | Opcode::Sload16
                | Opcode::Uload32
                | Opcode::Sload32
                | Opcode::Sload8x8
                | Opcode::Uload8x8
                | Opcode::Sload16x4
                | Opcode::Uload16x4
                | Opcode::Sload32x2
                | Opcode::Uload32x2 => {
                    assert_eq!(inputs.len(), 1, "only one input for load operands");
                    lower_to_amode(ctx, inputs[0], offset)
                }

                Opcode::LoadComplex
                | Opcode::Uload8Complex
                | Opcode::Sload8Complex
                | Opcode::Uload16Complex
                | Opcode::Sload16Complex
                | Opcode::Uload32Complex
                | Opcode::Sload32Complex
                | Opcode::Sload8x8Complex
                | Opcode::Uload8x8Complex
                | Opcode::Sload16x4Complex
                | Opcode::Uload16x4Complex
                | Opcode::Sload32x2Complex
                | Opcode::Uload32x2Complex => {
                    assert_eq!(
                        inputs.len(),
                        2,
                        "can't handle more than two inputs in complex load"
                    );
                    let base = put_input_in_reg(ctx, inputs[0]);
                    let index = put_input_in_reg(ctx, inputs[1]);
                    let shift = 0;
                    let flags = ctx.memflags(insn).expect("load should have memflags");
                    Amode::imm_reg_reg_shift(offset as u32, base, index, shift).with_flags(flags)
                }
                _ => unreachable!(),
            };

            if elem_ty == types::I128 {
                let dsts = get_output_reg(ctx, outputs[0]);
                ctx.emit(Inst::mov64_m_r(amode.clone(), dsts.regs()[0]));
                ctx.emit(Inst::mov64_m_r(amode.offset(8), dsts.regs()[1]));
            } else {
                let dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
                let is_xmm = elem_ty.is_float() || elem_ty.is_vector();
                match (sign_extend, is_xmm) {
                    (true, false) => {
                        // The load is sign-extended only when the output size is lower than 64 bits,
                        // so ext-mode is defined in this case.
                        ctx.emit(Inst::movsx_rm_r(ext_mode.unwrap(), RegMem::mem(amode), dst));
                    }
                    (false, false) => {
                        if elem_ty.bytes() == 8 {
                            // Use a plain load.
                            ctx.emit(Inst::mov64_m_r(amode, dst))
                        } else {
                            // Use a zero-extended load.
                            ctx.emit(Inst::movzx_rm_r(ext_mode.unwrap(), RegMem::mem(amode), dst))
                        }
                    }
                    (_, true) => {
                        ctx.emit(match elem_ty {
                            types::F32 => Inst::xmm_mov(SseOpcode::Movss, RegMem::mem(amode), dst),
                            types::F64 => Inst::xmm_mov(SseOpcode::Movsd, RegMem::mem(amode), dst),
                            types::I8X8 => {
                                if sign_extend == true {
                                    Inst::xmm_mov(SseOpcode::Pmovsxbw, RegMem::mem(amode), dst)
                                } else {
                                    Inst::xmm_mov(SseOpcode::Pmovzxbw, RegMem::mem(amode), dst)
                                }
                            }
                            types::I16X4 => {
                                if sign_extend == true {
                                    Inst::xmm_mov(SseOpcode::Pmovsxwd, RegMem::mem(amode), dst)
                                } else {
                                    Inst::xmm_mov(SseOpcode::Pmovzxwd, RegMem::mem(amode), dst)
                                }
                            }
                            types::I32X2 => {
                                if sign_extend == true {
                                    Inst::xmm_mov(SseOpcode::Pmovsxdq, RegMem::mem(amode), dst)
                                } else {
                                    Inst::xmm_mov(SseOpcode::Pmovzxdq, RegMem::mem(amode), dst)
                                }
                            }
                            _ if elem_ty.is_vector() && elem_ty.bits() == 128 => {
                                Inst::xmm_mov(SseOpcode::Movups, RegMem::mem(amode), dst)
                            }
                            // TODO Specialize for different types: MOVUPD, MOVDQU
                            _ => unreachable!(
                                "unexpected type for load: {:?} - {:?}",
                                elem_ty,
                                elem_ty.bits()
                            ),
                        });
                    }
                }
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
            let offset = ctx.data(insn).load_store_offset().unwrap();

            let elem_ty = match op {
                Opcode::Istore8 | Opcode::Istore8Complex => types::I8,
                Opcode::Istore16 | Opcode::Istore16Complex => types::I16,
                Opcode::Istore32 | Opcode::Istore32Complex => types::I32,
                Opcode::Store | Opcode::StoreComplex => ctx.input_ty(insn, 0),
                _ => unreachable!(),
            };

            let addr = match op {
                Opcode::Store | Opcode::Istore8 | Opcode::Istore16 | Opcode::Istore32 => {
                    assert_eq!(inputs.len(), 2, "only one input for store memory operands");
                    lower_to_amode(ctx, inputs[1], offset)
                }

                Opcode::StoreComplex
                | Opcode::Istore8Complex
                | Opcode::Istore16Complex
                | Opcode::Istore32Complex => {
                    assert_eq!(
                        inputs.len(),
                        3,
                        "can't handle more than two inputs in complex store"
                    );
                    let base = put_input_in_reg(ctx, inputs[1]);
                    let index = put_input_in_reg(ctx, inputs[2]);
                    let shift = 0;
                    let flags = ctx.memflags(insn).expect("store should have memflags");
                    Amode::imm_reg_reg_shift(offset as u32, base, index, shift).with_flags(flags)
                }

                _ => unreachable!(),
            };

            if elem_ty == types::I128 {
                let srcs = put_input_in_regs(ctx, inputs[0]);
                ctx.emit(Inst::store(types::I64, srcs.regs()[0], addr.clone()));
                ctx.emit(Inst::store(types::I64, srcs.regs()[1], addr.offset(8)));
            } else {
                let src = put_input_in_reg(ctx, inputs[0]);
                ctx.emit(Inst::store(elem_ty, src, addr));
            }
        }

        Opcode::AtomicRmw => {
            // This is a simple, general-case atomic update, based on a loop involving
            // `cmpxchg`.  Note that we could do much better than this in the case where the old
            // value at the location (that is to say, the SSA `Value` computed by this CLIF
            // instruction) is not required.  In that case, we could instead implement this
            // using a single `lock`-prefixed x64 read-modify-write instruction.  Also, even in
            // the case where the old value is required, for the `add` and `sub` cases, we can
            // use the single instruction `lock xadd`.  However, those improvements have been
            // left for another day.
            // TODO: filed as https://github.com/bytecodealliance/wasmtime/issues/2153
            let dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let mut addr = put_input_in_reg(ctx, inputs[0]);
            let mut arg2 = put_input_in_reg(ctx, inputs[1]);
            let ty_access = ty.unwrap();
            assert!(is_valid_atomic_transaction_ty(ty_access));

            // Make sure that both args are in virtual regs, since in effect we have to do a
            // parallel copy to get them safely to the AtomicRmwSeq input regs, and that's not
            // guaranteed safe if either is in a real reg.
            addr = ctx.ensure_in_vreg(addr, types::I64);
            arg2 = ctx.ensure_in_vreg(arg2, types::I64);

            // Move the args to the preordained AtomicRMW input regs.  Note that `AtomicRmwSeq`
            // operates at whatever width is specified by `ty`, so there's no need to
            // zero-extend `arg2` in the case of `ty` being I8/I16/I32.
            ctx.emit(Inst::gen_move(
                Writable::from_reg(regs::r9()),
                addr,
                types::I64,
            ));
            ctx.emit(Inst::gen_move(
                Writable::from_reg(regs::r10()),
                arg2,
                types::I64,
            ));

            // Now the AtomicRmwSeq (pseudo-) instruction itself
            let op = inst_common::AtomicRmwOp::from(ctx.data(insn).atomic_rmw_op().unwrap());
            ctx.emit(Inst::AtomicRmwSeq { ty: ty_access, op });

            // And finally, copy the preordained AtomicRmwSeq output reg to its destination.
            ctx.emit(Inst::gen_move(dst, regs::rax(), types::I64));
        }

        Opcode::AtomicCas => {
            // This is very similar to, but not identical to, the `AtomicRmw` case.  As with
            // `AtomicRmw`, there's no need to zero-extend narrow values here.
            let dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let addr = lower_to_amode(ctx, inputs[0], 0);
            let expected = put_input_in_reg(ctx, inputs[1]);
            let replacement = put_input_in_reg(ctx, inputs[2]);
            let ty_access = ty.unwrap();
            assert!(is_valid_atomic_transaction_ty(ty_access));

            // Move the expected value into %rax.  Because there's only one fixed register on
            // the input side, we don't have to use `ensure_in_vreg`, as is necessary in the
            // `AtomicRmw` case.
            ctx.emit(Inst::gen_move(
                Writable::from_reg(regs::rax()),
                expected,
                types::I64,
            ));
            ctx.emit(Inst::LockCmpxchg {
                ty: ty_access,
                src: replacement,
                dst: addr.into(),
            });
            // And finally, copy the old value at the location to its destination reg.
            ctx.emit(Inst::gen_move(dst, regs::rax(), types::I64));
        }

        Opcode::AtomicLoad => {
            // This is a normal load.  The x86-TSO memory model provides sufficient sequencing
            // to satisfy the CLIF synchronisation requirements for `AtomicLoad` without the
            // need for any fence instructions.
            let data = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let addr = lower_to_amode(ctx, inputs[0], 0);
            let ty_access = ty.unwrap();
            assert!(is_valid_atomic_transaction_ty(ty_access));

            let rm = RegMem::mem(addr);
            if ty_access == types::I64 {
                ctx.emit(Inst::mov64_rm_r(rm, data));
            } else {
                let ext_mode = ExtMode::new(ty_access.bits(), 64).expect(&format!(
                    "invalid extension during AtomicLoad: {} -> {}",
                    ty_access.bits(),
                    64
                ));
                ctx.emit(Inst::movzx_rm_r(ext_mode, rm, data));
            }
        }

        Opcode::AtomicStore => {
            // This is a normal store, followed by an `mfence` instruction.
            let data = put_input_in_reg(ctx, inputs[0]);
            let addr = lower_to_amode(ctx, inputs[1], 0);
            let ty_access = ctx.input_ty(insn, 0);
            assert!(is_valid_atomic_transaction_ty(ty_access));

            ctx.emit(Inst::store(ty_access, data, addr));
            ctx.emit(Inst::Fence {
                kind: FenceKind::MFence,
            });
        }

        Opcode::Fence => {
            ctx.emit(Inst::Fence {
                kind: FenceKind::MFence,
            });
        }

        Opcode::FuncAddr => {
            let dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let (extname, _) = ctx.call_target(insn).unwrap();
            let extname = extname.clone();
            ctx.emit(Inst::LoadExtName {
                dst,
                name: Box::new(extname),
                offset: 0,
            });
        }

        Opcode::SymbolValue => {
            let dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let (extname, _, offset) = ctx.symbol_value(insn).unwrap();
            let extname = extname.clone();
            ctx.emit(Inst::LoadExtName {
                dst,
                name: Box::new(extname),
                offset,
            });
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
            let dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let offset: i32 = offset.into();
            let inst = ctx
                .abi()
                .stackslot_addr(stack_slot, u32::try_from(offset).unwrap(), dst);
            ctx.emit(inst);
        }

        Opcode::Select => {
            let flag_input = inputs[0];
            if let Some(fcmp) = matches_input(ctx, flag_input, Opcode::Fcmp) {
                let cond_code = ctx.data(fcmp).fp_cond_code().unwrap();

                // For equal, we flip the operands, because we can't test a conjunction of
                // CPU flags with a single cmove; see InvertedEqualOrConditions doc comment.
                let (lhs_input, rhs_input) = match cond_code {
                    FloatCC::Equal => (inputs[2], inputs[1]),
                    _ => (inputs[1], inputs[2]),
                };

                let ty = ctx.output_ty(insn, 0);
                let rhs = put_input_in_regs(ctx, rhs_input);
                let dst = get_output_reg(ctx, outputs[0]);
                let lhs = put_input_in_regs(ctx, lhs_input);

                // We request inversion of Equal to NotEqual here: taking LHS if equal would mean
                // take it if both CC::NP and CC::Z are set, the conjunction of which can't be
                // modeled with a single cmov instruction. Instead, we'll swap LHS and RHS in the
                // select operation, and invert the equal to a not-equal here.
                let fcmp_results = emit_fcmp(ctx, fcmp, cond_code, FcmpSpec::InvertEqual);

                if let FcmpCondResult::InvertedEqualOrConditions(_, _) = &fcmp_results {
                    // Keep this sync'd with the lowering of the select inputs above.
                    assert_eq!(cond_code, FloatCC::Equal);
                }

                emit_moves(ctx, dst, rhs, ty);

                let operand_size = if ty == types::F64 {
                    OperandSize::Size64
                } else {
                    OperandSize::Size32
                };
                match fcmp_results {
                    FcmpCondResult::Condition(cc) => {
                        if is_int_or_ref_ty(ty) || ty == types::I128 || ty == types::B128 {
                            let size = ty.bytes() as u8;
                            emit_cmoves(ctx, size, cc, lhs, dst);
                        } else {
                            ctx.emit(Inst::xmm_cmove(
                                operand_size,
                                cc,
                                RegMem::reg(lhs.only_reg().unwrap()),
                                dst.only_reg().unwrap(),
                            ));
                        }
                    }
                    FcmpCondResult::AndConditions(_, _) => {
                        unreachable!(
                            "can't AND with select; see above comment about inverting equal"
                        );
                    }
                    FcmpCondResult::InvertedEqualOrConditions(cc1, cc2)
                    | FcmpCondResult::OrConditions(cc1, cc2) => {
                        if is_int_or_ref_ty(ty) || ty == types::I128 {
                            let size = ty.bytes() as u8;
                            emit_cmoves(ctx, size, cc1, lhs.clone(), dst);
                            emit_cmoves(ctx, size, cc2, lhs, dst);
                        } else {
                            ctx.emit(Inst::xmm_cmove(
                                operand_size,
                                cc1,
                                RegMem::reg(lhs.only_reg().unwrap()),
                                dst.only_reg().unwrap(),
                            ));
                            ctx.emit(Inst::xmm_cmove(
                                operand_size,
                                cc2,
                                RegMem::reg(lhs.only_reg().unwrap()),
                                dst.only_reg().unwrap(),
                            ));
                        }
                    }
                }
            } else {
                let ty = ty.unwrap();

                let size = ty.bytes() as u8;
                let lhs = put_input_in_regs(ctx, inputs[1]);
                let rhs = put_input_in_regs(ctx, inputs[2]);
                let dst = get_output_reg(ctx, outputs[0]);

                let cc = if let Some(icmp) = matches_input(ctx, flag_input, Opcode::Icmp) {
                    let cond_code = ctx.data(icmp).cond_code().unwrap();
                    let cond_code = emit_cmp(ctx, icmp, cond_code);
                    CC::from_intcc(cond_code)
                } else {
                    let sel_ty = ctx.input_ty(insn, 0);
                    let size = OperandSize::from_ty(ctx.input_ty(insn, 0));
                    let test = put_input_in_reg(ctx, flag_input);
                    let test_input = if sel_ty == types::B1 {
                        // The input is a boolean value; test the LSB for nonzero with:
                        //     test reg, 1
                        RegMemImm::imm(1)
                    } else {
                        // The input is an integer; test the whole value for
                        // nonzero with:
                        //     test reg, reg
                        //
                        // (It doesn't make sense to have a boolean wider than
                        // one bit here -- which bit would cause us to select an
                        // input?)
                        assert!(!is_bool_ty(sel_ty));
                        RegMemImm::reg(test)
                    };
                    ctx.emit(Inst::test_rmi_r(size, test_input, test));
                    CC::NZ
                };

                // This doesn't affect the flags.
                emit_moves(ctx, dst, rhs, ty);

                if is_int_or_ref_ty(ty) || ty == types::I128 {
                    emit_cmoves(ctx, size, cc, lhs, dst);
                } else {
                    debug_assert!(ty == types::F32 || ty == types::F64);
                    ctx.emit(Inst::xmm_cmove(
                        if ty == types::F64 {
                            OperandSize::Size64
                        } else {
                            OperandSize::Size32
                        },
                        cc,
                        RegMem::reg(lhs.only_reg().unwrap()),
                        dst.only_reg().unwrap(),
                    ));
                }
            }
        }

        Opcode::Selectif | Opcode::SelectifSpectreGuard => {
            let lhs = put_input_in_regs(ctx, inputs[1]);
            let rhs = put_input_in_regs(ctx, inputs[2]);
            let dst = get_output_reg(ctx, outputs[0]);
            let ty = ctx.output_ty(insn, 0);

            // Verification ensures that the input is always a single-def ifcmp.
            let cmp_insn = ctx
                .get_input_as_source_or_const(inputs[0].insn, inputs[0].input)
                .inst
                .unwrap()
                .0;
            debug_assert_eq!(ctx.data(cmp_insn).opcode(), Opcode::Ifcmp);
            let cond_code = ctx.data(insn).cond_code().unwrap();
            let cond_code = emit_cmp(ctx, cmp_insn, cond_code);

            let cc = CC::from_intcc(cond_code);

            if is_int_or_ref_ty(ty) || ty == types::I128 {
                let size = ty.bytes() as u8;
                emit_moves(ctx, dst, rhs, ty);
                emit_cmoves(ctx, size, cc, lhs, dst);
            } else {
                debug_assert!(ty == types::F32 || ty == types::F64);
                emit_moves(ctx, dst, rhs, ty);
                ctx.emit(Inst::xmm_cmove(
                    if ty == types::F64 {
                        OperandSize::Size64
                    } else {
                        OperandSize::Size32
                    },
                    cc,
                    RegMem::reg(lhs.only_reg().unwrap()),
                    dst.only_reg().unwrap(),
                ));
            }
        }

        Opcode::Udiv | Opcode::Urem | Opcode::Sdiv | Opcode::Srem => {
            let kind = match op {
                Opcode::Udiv => DivOrRemKind::UnsignedDiv,
                Opcode::Sdiv => DivOrRemKind::SignedDiv,
                Opcode::Urem => DivOrRemKind::UnsignedRem,
                Opcode::Srem => DivOrRemKind::SignedRem,
                _ => unreachable!(),
            };
            let is_div = kind.is_div();

            let input_ty = ctx.input_ty(insn, 0);
            let size = OperandSize::from_ty(input_ty);

            let dividend = put_input_in_reg(ctx, inputs[0]);
            let dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();

            ctx.emit(Inst::gen_move(
                Writable::from_reg(regs::rax()),
                dividend,
                input_ty,
            ));

            // Always do explicit checks for `srem`: otherwise, INT_MIN % -1 is not handled properly.
            if flags.avoid_div_traps() || op == Opcode::Srem {
                // A vcode meta-instruction is used to lower the inline checks, since they embed
                // pc-relative offsets that must not change, thus requiring regalloc to not
                // interfere by introducing spills and reloads.
                //
                // Note it keeps the result in $rax (for divide) or $rdx (for rem), so that
                // regalloc is aware of the coalescing opportunity between rax/rdx and the
                // destination register.
                let divisor = put_input_in_reg(ctx, inputs[1]);

                let divisor_copy = ctx.alloc_tmp(types::I64).only_reg().unwrap();
                ctx.emit(Inst::gen_move(divisor_copy, divisor, types::I64));

                let tmp = if op == Opcode::Sdiv && size == OperandSize::Size64 {
                    Some(ctx.alloc_tmp(types::I64).only_reg().unwrap())
                } else {
                    None
                };
                // TODO use xor
                ctx.emit(Inst::imm(
                    OperandSize::Size32,
                    0,
                    Writable::from_reg(regs::rdx()),
                ));
                ctx.emit(Inst::checked_div_or_rem_seq(kind, size, divisor_copy, tmp));
            } else {
                // We don't want more than one trap record for a single instruction,
                // so let's not allow the "mem" case (load-op merging) here; force
                // divisor into a register instead.
                let divisor = RegMem::reg(put_input_in_reg(ctx, inputs[1]));

                // Fill in the high parts:
                if kind.is_signed() {
                    // sign-extend the sign-bit of al into ah for size 1, or rax into rdx, for
                    // signed opcodes.
                    ctx.emit(Inst::sign_extend_data(size));
                } else if input_ty == types::I8 {
                    ctx.emit(Inst::movzx_rm_r(
                        ExtMode::BL,
                        RegMem::reg(regs::rax()),
                        Writable::from_reg(regs::rax()),
                    ));
                } else {
                    // zero for unsigned opcodes.
                    ctx.emit(Inst::imm(
                        OperandSize::Size64,
                        0,
                        Writable::from_reg(regs::rdx()),
                    ));
                }

                // Emit the actual idiv.
                ctx.emit(Inst::div(size, kind.is_signed(), divisor));
            }

            // Move the result back into the destination reg.
            if is_div {
                // The quotient is in rax.
                ctx.emit(Inst::gen_move(dst, regs::rax(), input_ty));
            } else {
                if size == OperandSize::Size8 {
                    // The remainder is in AH. Right-shift by 8 bits then move from rax.
                    ctx.emit(Inst::shift_r(
                        OperandSize::Size64,
                        ShiftKind::ShiftRightLogical,
                        Some(8),
                        Writable::from_reg(regs::rax()),
                    ));
                    ctx.emit(Inst::gen_move(dst, regs::rax(), input_ty));
                } else {
                    // The remainder is in rdx.
                    ctx.emit(Inst::gen_move(dst, regs::rdx(), input_ty));
                }
            }
        }

        Opcode::Umulhi | Opcode::Smulhi => {
            let input_ty = ctx.input_ty(insn, 0);

            let lhs = put_input_in_reg(ctx, inputs[0]);
            let rhs = input_to_reg_mem(ctx, inputs[1]);
            let dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();

            // Move lhs in %rax.
            ctx.emit(Inst::gen_move(
                Writable::from_reg(regs::rax()),
                lhs,
                input_ty,
            ));

            // Emit the actual mul or imul.
            let signed = op == Opcode::Smulhi;
            ctx.emit(Inst::mul_hi(OperandSize::from_ty(input_ty), signed, rhs));

            // Read the result from the high part (stored in %rdx).
            ctx.emit(Inst::gen_move(dst, regs::rdx(), input_ty));
        }

        Opcode::GetPinnedReg => {
            let dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            ctx.emit(Inst::gen_move(dst, regs::pinned_reg(), types::I64));
        }

        Opcode::SetPinnedReg => {
            let src = put_input_in_reg(ctx, inputs[0]);
            ctx.emit(Inst::gen_move(
                Writable::from_reg(regs::pinned_reg()),
                src,
                types::I64,
            ));
        }

        Opcode::Vconst => {
            let used_constant = if let &InstructionData::UnaryConst {
                constant_handle, ..
            } = ctx.data(insn)
            {
                ctx.use_constant(VCodeConstantData::Pool(
                    constant_handle,
                    ctx.get_constant_data(constant_handle).clone(),
                ))
            } else {
                unreachable!("vconst should always have unary_const format")
            };
            // TODO use Inst::gen_constant() instead.
            let dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let ty = ty.unwrap();
            ctx.emit(Inst::xmm_load_const(used_constant, dst, ty));
        }

        Opcode::RawBitcast => {
            // A raw_bitcast is just a mechanism for correcting the type of V128 values (see
            // https://github.com/bytecodealliance/wasmtime/issues/1147). As such, this IR
            // instruction should emit no machine code but a move is necessary to give the register
            // allocator a definition for the output virtual register.
            let src = put_input_in_reg(ctx, inputs[0]);
            let dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let ty = ty.unwrap();
            ctx.emit(Inst::gen_move(dst, src, ty));
        }

        Opcode::Shuffle => {
            let ty = ty.unwrap();
            let dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let lhs_ty = ctx.input_ty(insn, 0);
            let lhs = put_input_in_reg(ctx, inputs[0]);
            let rhs = put_input_in_reg(ctx, inputs[1]);
            let mask = match ctx.get_immediate(insn) {
                Some(DataValue::V128(bytes)) => bytes.to_vec(),
                _ => unreachable!("shuffle should always have a 16-byte immediate"),
            };

            // A mask-building helper: in 128-bit SIMD, 0-15 indicate which lane to read from and a
            // 1 in the most significant position zeroes the lane.
            let zero_unknown_lane_index = |b: u8| if b > 15 { 0b10000000 } else { b };

            ctx.emit(Inst::gen_move(dst, rhs, ty));
            if rhs == lhs {
                // If `lhs` and `rhs` are the same we can use a single PSHUFB to shuffle the XMM
                // register. We statically build `constructed_mask` to zero out any unknown lane
                // indices (may not be completely necessary: verification could fail incorrect mask
                // values) and fix the indexes to all point to the `dst` vector.
                let constructed_mask = mask
                    .iter()
                    // If the mask is greater than 15 it still may be referring to a lane in b.
                    .map(|&b| if b > 15 { b.wrapping_sub(16) } else { b })
                    .map(zero_unknown_lane_index)
                    .collect();
                let constant = ctx.use_constant(VCodeConstantData::Generated(constructed_mask));
                let tmp = ctx.alloc_tmp(types::I8X16).only_reg().unwrap();
                ctx.emit(Inst::xmm_load_const(constant, tmp, ty));
                // After loading the constructed mask in a temporary register, we use this to
                // shuffle the `dst` register (remember that, in this case, it is the same as
                // `src` so we disregard this register).
                ctx.emit(Inst::xmm_rm_r(SseOpcode::Pshufb, RegMem::from(tmp), dst));
            } else {
                if isa_flags.use_avx512vl_simd() && isa_flags.use_avx512vbmi_simd() {
                    assert!(
                        mask.iter().all(|b| *b < 32),
                        "shuffle mask values must be between 0 and 31"
                    );

                    // Load the mask into the destination register.
                    let constant = ctx.use_constant(VCodeConstantData::Generated(mask.into()));
                    ctx.emit(Inst::xmm_load_const(constant, dst, ty));

                    // VPERMI2B has the exact semantics of Wasm's shuffle:
                    // permute the bytes in `src1` and `src2` using byte indexes
                    // in `dst` and store the byte results in `dst`.
                    ctx.emit(Inst::xmm_rm_r_evex(
                        Avx512Opcode::Vpermi2b,
                        RegMem::reg(rhs),
                        lhs,
                        dst,
                    ));
                } else {
                    // If `lhs` and `rhs` are different, we must shuffle each separately and then OR
                    // them together. This is necessary due to PSHUFB semantics. As in the case above,
                    // we build the `constructed_mask` for each case statically.

                    // PSHUFB the `lhs` argument into `tmp0`, placing zeroes for unused lanes.
                    let tmp0 = ctx.alloc_tmp(lhs_ty).only_reg().unwrap();
                    ctx.emit(Inst::gen_move(tmp0, lhs, lhs_ty));
                    let constructed_mask =
                        mask.iter().cloned().map(zero_unknown_lane_index).collect();
                    let constant = ctx.use_constant(VCodeConstantData::Generated(constructed_mask));
                    let tmp1 = ctx.alloc_tmp(types::I8X16).only_reg().unwrap();
                    ctx.emit(Inst::xmm_load_const(constant, tmp1, ty));
                    ctx.emit(Inst::xmm_rm_r(SseOpcode::Pshufb, RegMem::from(tmp1), tmp0));

                    // PSHUFB the second argument, placing zeroes for unused lanes.
                    let constructed_mask = mask
                        .iter()
                        .map(|b| b.wrapping_sub(16))
                        .map(zero_unknown_lane_index)
                        .collect();
                    let constant = ctx.use_constant(VCodeConstantData::Generated(constructed_mask));
                    let tmp2 = ctx.alloc_tmp(types::I8X16).only_reg().unwrap();
                    ctx.emit(Inst::xmm_load_const(constant, tmp2, ty));
                    ctx.emit(Inst::xmm_rm_r(SseOpcode::Pshufb, RegMem::from(tmp2), dst));

                    // OR the shuffled registers (the mechanism and lane-size for OR-ing the registers
                    // is not important).
                    ctx.emit(Inst::xmm_rm_r(SseOpcode::Orps, RegMem::from(tmp0), dst));
                }
            }
        }

        Opcode::Swizzle => {
            // SIMD swizzle; the following inefficient implementation is due to the Wasm SIMD spec
            // requiring mask indexes greater than 15 to have the same semantics as a 0 index. For
            // the spec discussion, see https://github.com/WebAssembly/simd/issues/93. The CLIF
            // semantics match the Wasm SIMD semantics for this instruction.
            // The instruction format maps to variables like: %dst = swizzle %src, %mask
            let ty = ty.unwrap();
            let dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let src = put_input_in_reg(ctx, inputs[0]);
            let swizzle_mask = put_input_in_reg(ctx, inputs[1]);

            // Inform the register allocator that `src` and `dst` should be in the same register.
            ctx.emit(Inst::gen_move(dst, src, ty));

            // Create a mask for zeroing out-of-bounds lanes of the swizzle mask.
            let zero_mask = ctx.alloc_tmp(types::I8X16).only_reg().unwrap();
            static ZERO_MASK_VALUE: [u8; 16] = [
                0x70, 0x70, 0x70, 0x70, 0x70, 0x70, 0x70, 0x70, 0x70, 0x70, 0x70, 0x70, 0x70, 0x70,
                0x70, 0x70,
            ];
            let constant = ctx.use_constant(VCodeConstantData::WellKnown(&ZERO_MASK_VALUE));
            ctx.emit(Inst::xmm_load_const(constant, zero_mask, ty));

            // Use the `zero_mask` on a writable `swizzle_mask`.
            let swizzle_mask = Writable::from_reg(swizzle_mask);
            ctx.emit(Inst::xmm_rm_r(
                SseOpcode::Paddusb,
                RegMem::from(zero_mask),
                swizzle_mask,
            ));

            // Shuffle `dst` using the fixed-up `swizzle_mask`.
            ctx.emit(Inst::xmm_rm_r(
                SseOpcode::Pshufb,
                RegMem::from(swizzle_mask),
                dst,
            ));
        }

        Opcode::Insertlane => {
            // The instruction format maps to variables like: %dst = insertlane %in_vec, %src, %lane
            let ty = ty.unwrap();
            let dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let in_vec = put_input_in_reg(ctx, inputs[0]);
            let src_ty = ctx.input_ty(insn, 1);
            debug_assert!(!src_ty.is_vector());
            let src = input_to_reg_mem(ctx, inputs[1]);
            let lane = if let InstructionData::TernaryImm8 { imm, .. } = ctx.data(insn) {
                *imm
            } else {
                unreachable!();
            };
            debug_assert!(lane < ty.lane_count() as u8);

            ctx.emit(Inst::gen_move(dst, in_vec, ty));
            emit_insert_lane(ctx, src, dst, lane, ty.lane_type());
        }

        Opcode::Extractlane => {
            // The instruction format maps to variables like: %dst = extractlane %src, %lane
            let ty = ty.unwrap();
            let dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let src_ty = ctx.input_ty(insn, 0);
            assert_eq!(src_ty.bits(), 128);
            let src = put_input_in_reg(ctx, inputs[0]);
            let lane = if let InstructionData::BinaryImm8 { imm, .. } = ctx.data(insn) {
                *imm
            } else {
                unreachable!();
            };
            debug_assert!(lane < src_ty.lane_count() as u8);

            emit_extract_lane(ctx, src, dst, lane, ty);
        }

        Opcode::ScalarToVector => {
            // When moving a scalar value to a vector register, we must be handle several
            // situations:
            //  1. a scalar float is already in an XMM register, so we simply move it
            //  2. a scalar of any other type resides in a GPR register: MOVD moves the bits to an
            //     XMM register and zeroes the upper bits
            //  3. a scalar (float or otherwise) that has previously been loaded from memory (e.g.
            //     the default lowering of Wasm's `load[32|64]_zero`) can be lowered to a single
            //     MOVSS/MOVSD instruction; to do this, we rely on `input_to_reg_mem` to sink the
            //     unused load.
            let src = input_to_reg_mem(ctx, inputs[0]);
            let src_ty = ctx.input_ty(insn, 0);
            let dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let dst_ty = ty.unwrap();
            assert!(src_ty == dst_ty.lane_type() && dst_ty.bits() == 128);
            match src {
                RegMem::Reg { reg } => {
                    if src_ty.is_float() {
                        // Case 1: when moving a scalar float, we simply move from one XMM register
                        // to another, expecting the register allocator to elide this. Here we
                        // assume that the upper bits of a scalar float have not been munged with
                        // (the same assumption the old backend makes).
                        ctx.emit(Inst::gen_move(dst, reg, dst_ty));
                    } else {
                        // Case 2: when moving a scalar value of any other type, use MOVD to zero
                        // the upper lanes.
                        let src_size = match src_ty.bits() {
                            32 => OperandSize::Size32,
                            64 => OperandSize::Size64,
                            _ => unimplemented!("invalid source size for type: {}", src_ty),
                        };
                        ctx.emit(Inst::gpr_to_xmm(SseOpcode::Movd, src, src_size, dst));
                    }
                }
                RegMem::Mem { .. } => {
                    // Case 3: when presented with `load + scalar_to_vector`, coalesce into a single
                    // MOVSS/MOVSD instruction.
                    let opcode = match src_ty.bits() {
                        32 => SseOpcode::Movss,
                        64 => SseOpcode::Movsd,
                        _ => unimplemented!("unable to move scalar to vector for type: {}", src_ty),
                    };
                    ctx.emit(Inst::xmm_mov(opcode, src, dst));
                }
            }
        }

        Opcode::Splat => {
            let ty = ty.unwrap();
            assert_eq!(ty.bits(), 128);
            let src_ty = ctx.input_ty(insn, 0);
            assert!(src_ty.bits() < 128);

            let src = input_to_reg_mem(ctx, inputs[0]);
            let dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();

            // We know that splat will overwrite all of the lanes of `dst` but it takes several
            // instructions to do so. Because of the multiple instructions, there is no good way to
            // declare `dst` a `def` except with the following pseudo-instruction.
            ctx.emit(Inst::xmm_uninit_value(dst));

            // TODO: eventually many of these sequences could be optimized with AVX's VBROADCAST*
            // and VPBROADCAST*.
            match ty.lane_bits() {
                8 => {
                    emit_insert_lane(ctx, src, dst, 0, ty.lane_type());
                    // Initialize a register with all 0s.
                    let tmp = ctx.alloc_tmp(ty).only_reg().unwrap();
                    ctx.emit(Inst::xmm_rm_r(SseOpcode::Pxor, RegMem::from(tmp), tmp));
                    // Shuffle the lowest byte lane to all other lanes.
                    ctx.emit(Inst::xmm_rm_r(SseOpcode::Pshufb, RegMem::from(tmp), dst))
                }
                16 => {
                    emit_insert_lane(ctx, src.clone(), dst, 0, ty.lane_type());
                    emit_insert_lane(ctx, src, dst, 1, ty.lane_type());
                    // Shuffle the lowest two lanes to all other lanes.
                    ctx.emit(Inst::xmm_rm_r_imm(
                        SseOpcode::Pshufd,
                        RegMem::from(dst),
                        dst,
                        0,
                        OperandSize::Size32,
                    ))
                }
                32 => {
                    emit_insert_lane(ctx, src, dst, 0, ty.lane_type());
                    // Shuffle the lowest lane to all other lanes.
                    ctx.emit(Inst::xmm_rm_r_imm(
                        SseOpcode::Pshufd,
                        RegMem::from(dst),
                        dst,
                        0,
                        OperandSize::Size32,
                    ))
                }
                64 => {
                    emit_insert_lane(ctx, src.clone(), dst, 0, ty.lane_type());
                    emit_insert_lane(ctx, src, dst, 1, ty.lane_type());
                }
                _ => panic!("Invalid type to splat: {}", ty),
            }
        }

        Opcode::VanyTrue => {
            let dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let src_ty = ctx.input_ty(insn, 0);
            assert_eq!(src_ty.bits(), 128);
            let src = put_input_in_reg(ctx, inputs[0]);
            // Set the ZF if the result is all zeroes.
            ctx.emit(Inst::xmm_cmp_rm_r(SseOpcode::Ptest, RegMem::reg(src), src));
            // If the ZF is not set, place a 1 in `dst`.
            ctx.emit(Inst::setcc(CC::NZ, dst));
        }

        Opcode::VallTrue => {
            let dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let src_ty = ctx.input_ty(insn, 0);
            assert_eq!(src_ty.bits(), 128);
            let src = input_to_reg_mem(ctx, inputs[0]);

            let eq = |ty: Type| match ty.lane_bits() {
                8 => SseOpcode::Pcmpeqb,
                16 => SseOpcode::Pcmpeqw,
                32 => SseOpcode::Pcmpeqd,
                64 => SseOpcode::Pcmpeqq,
                _ => panic!("Unable to find an instruction for {} for type: {}", op, ty),
            };

            // Initialize a register with all 0s.
            let tmp = ctx.alloc_tmp(src_ty).only_reg().unwrap();
            ctx.emit(Inst::xmm_rm_r(SseOpcode::Pxor, RegMem::from(tmp), tmp));
            // Compare to see what lanes are filled with all 1s.
            ctx.emit(Inst::xmm_rm_r(eq(src_ty), src, tmp));
            // Set the ZF if the result is all zeroes.
            ctx.emit(Inst::xmm_cmp_rm_r(
                SseOpcode::Ptest,
                RegMem::from(tmp),
                tmp.to_reg(),
            ));
            // If the ZF is set, place a 1 in `dst`.
            ctx.emit(Inst::setcc(CC::Z, dst));
        }

        Opcode::VhighBits => {
            let src = put_input_in_reg(ctx, inputs[0]);
            let src_ty = ctx.input_ty(insn, 0);
            debug_assert!(src_ty.is_vector() && src_ty.bits() == 128);
            let dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            debug_assert!(dst.to_reg().get_class() == RegClass::I64);

            // The Intel specification allows using both 32-bit and 64-bit GPRs as destination for
            // the "move mask" instructions. This is controlled by the REX.R bit: "In 64-bit mode,
            // the instruction can access additional registers when used with a REX.R prefix. The
            // default operand size is 64-bit in 64-bit mode" (PMOVMSKB in IA Software Development
            // Manual, vol. 2). This being the case, we will always clear REX.W since its use is
            // unnecessary (`OperandSize` is used for setting/clearing REX.W).
            let size = OperandSize::Size32;

            match src_ty {
                types::I8X16 | types::B8X16 => {
                    ctx.emit(Inst::xmm_to_gpr(SseOpcode::Pmovmskb, src, dst, size))
                }
                types::I32X4 | types::B32X4 | types::F32X4 => {
                    ctx.emit(Inst::xmm_to_gpr(SseOpcode::Movmskps, src, dst, size))
                }
                types::I64X2 | types::B64X2 | types::F64X2 => {
                    ctx.emit(Inst::xmm_to_gpr(SseOpcode::Movmskpd, src, dst, size))
                }
                types::I16X8 | types::B16X8 => {
                    // There is no x86 instruction for extracting the high bit of 16-bit lanes so
                    // here we:
                    // - duplicate the 16-bit lanes of `src` into 8-bit lanes:
                    //     PACKSSWB([x1, x2, ...], [x1, x2, ...]) = [x1', x2', ..., x1', x2', ...]
                    // - use PMOVMSKB to gather the high bits; now we have duplicates, though
                    // - shift away the bottom 8 high bits to remove the duplicates.
                    let tmp = ctx.alloc_tmp(src_ty).only_reg().unwrap();
                    ctx.emit(Inst::gen_move(tmp, src, src_ty));
                    ctx.emit(Inst::xmm_rm_r(SseOpcode::Packsswb, RegMem::reg(src), tmp));
                    ctx.emit(Inst::xmm_to_gpr(
                        SseOpcode::Pmovmskb,
                        tmp.to_reg(),
                        dst,
                        size,
                    ));
                    ctx.emit(Inst::shift_r(
                        OperandSize::Size64,
                        ShiftKind::ShiftRightLogical,
                        Some(8),
                        dst,
                    ));
                }
                _ => unimplemented!("unknown input type {} for {}", src_ty, op),
            }
        }

        Opcode::Iconcat => {
            let ty = ctx.output_ty(insn, 0);
            assert_eq!(
                ty,
                types::I128,
                "Iconcat not expected to be used for non-128-bit type"
            );
            assert_eq!(ctx.input_ty(insn, 0), types::I64);
            assert_eq!(ctx.input_ty(insn, 1), types::I64);
            let lo = put_input_in_reg(ctx, inputs[0]);
            let hi = put_input_in_reg(ctx, inputs[1]);
            let dst = get_output_reg(ctx, outputs[0]);
            ctx.emit(Inst::gen_move(dst.regs()[0], lo, types::I64));
            ctx.emit(Inst::gen_move(dst.regs()[1], hi, types::I64));
        }

        Opcode::Isplit => {
            let ty = ctx.input_ty(insn, 0);
            assert_eq!(
                ty,
                types::I128,
                "Iconcat not expected to be used for non-128-bit type"
            );
            assert_eq!(ctx.output_ty(insn, 0), types::I64);
            assert_eq!(ctx.output_ty(insn, 1), types::I64);
            let src = put_input_in_regs(ctx, inputs[0]);
            let dst_lo = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let dst_hi = get_output_reg(ctx, outputs[1]).only_reg().unwrap();
            ctx.emit(Inst::gen_move(dst_lo, src.regs()[0], types::I64));
            ctx.emit(Inst::gen_move(dst_hi, src.regs()[1], types::I64));
        }

        Opcode::TlsValue => match flags.tls_model() {
            TlsModel::ElfGd => {
                let dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
                let (name, _, _) = ctx.symbol_value(insn).unwrap();
                let symbol = name.clone();
                ctx.emit(Inst::ElfTlsGetAddr { symbol });
                ctx.emit(Inst::gen_move(dst, regs::rax(), types::I64));
            }
            TlsModel::Macho => {
                let dst = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
                let (name, _, _) = ctx.symbol_value(insn).unwrap();
                let symbol = name.clone();
                ctx.emit(Inst::MachOTlsGetAddr { symbol });
                ctx.emit(Inst::gen_move(dst, regs::rax(), types::I64));
            }
            _ => {
                todo!(
                    "Unimplemented TLS model in x64 backend: {:?}",
                    flags.tls_model()
                );
            }
        },

        // Unimplemented opcodes below. These are not currently used by Wasm
        // lowering or other known embeddings, but should be either supported or
        // removed eventually.
        Opcode::Uload8x8Complex
        | Opcode::Sload8x8Complex
        | Opcode::Uload16x4Complex
        | Opcode::Sload16x4Complex
        | Opcode::Uload32x2Complex
        | Opcode::Sload32x2Complex => {
            unimplemented!("Vector load {:?} not implemented", op);
        }

        Opcode::Cls => unimplemented!("Cls not supported"),

        Opcode::Fma => unimplemented!("Fma not supported"),

        Opcode::BorNot | Opcode::BxorNot => {
            unimplemented!("or-not / xor-not opcodes not implemented");
        }

        Opcode::Bmask => unimplemented!("Bmask not implemented"),

        Opcode::Trueif | Opcode::Trueff => unimplemented!("trueif / trueff not implemented"),

        Opcode::ConstAddr => unimplemented!("ConstAddr not implemented"),

        Opcode::Vsplit | Opcode::Vconcat => {
            unimplemented!("Vector split/concat ops not implemented.");
        }

        Opcode::SqmulRoundSat | Opcode::Uunarrow => {
            unimplemented!("unimplemented lowering for opcode {:?}", op)
        }

        // Opcodes that should be removed by legalization. These should
        // eventually be removed if/when we replace in-situ legalization with
        // something better.
        Opcode::Ifcmp | Opcode::Ffcmp => {
            panic!("Should never reach ifcmp/ffcmp as isel root!");
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

        Opcode::StackLoad | Opcode::StackStore => {
            panic!("Direct stack memory access not supported; should have been legalized");
        }

        Opcode::GlobalValue => {
            panic!("global_value should have been removed by legalization!");
        }

        Opcode::HeapAddr => {
            panic!("heap_addr should have been removed by legalization!");
        }

        Opcode::TableAddr => {
            panic!("table_addr should have been removed by legalization!");
        }

        Opcode::Safepoint => {
            panic!("safepoint instructions not used by new backend's safepoints!");
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
        | Opcode::Regfill
        | Opcode::Copy
        | Opcode::DummySargT => {
            panic!("Unused opcode should not be encountered.");
        }

        Opcode::JumpTableEntry | Opcode::JumpTableBase => {
            panic!("Should not appear: we handle BrTable directly");
        }

        Opcode::Trapz | Opcode::Trapnz | Opcode::ResumableTrapnz => {
            panic!("trapz / trapnz / resumable_trapnz should have been removed by legalization!");
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

        Opcode::Nop => {
            // Nothing.
        }
    }

    Ok(())
}

//=============================================================================
// Lowering-backend trait implementation.

impl LowerBackend for X64Backend {
    type MInst = Inst;

    fn lower<C: LowerCtx<I = Inst>>(&self, ctx: &mut C, ir_inst: IRInst) -> CodegenResult<()> {
        lower_insn_to_regs(ctx, ir_inst, &self.flags, &self.x64_flags, &self.triple)
    }

    fn lower_branch_group<C: LowerCtx<I = Inst>>(
        &self,
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

            trace!(
                "lowering two-branch group: opcodes are {:?} and {:?}",
                op0,
                op1
            );
            assert!(op1 == Opcode::Jump || op1 == Opcode::Fallthrough);

            let taken = targets[0];
            // not_taken target is the target of the second branch, even if it is a Fallthrough
            // instruction: because we reorder blocks while we lower, the fallthrough in the new
            // order is not (necessarily) the same as the fallthrough in CLIF. So we use the
            // explicitly-provided target.
            let not_taken = targets[1];

            match op0 {
                Opcode::Brz | Opcode::Brnz => {
                    let flag_input = InsnInput {
                        insn: branches[0],
                        input: 0,
                    };

                    let src_ty = ctx.input_ty(branches[0], 0);

                    if let Some(icmp) = matches_input(ctx, flag_input, Opcode::Icmp) {
                        let cond_code = ctx.data(icmp).cond_code().unwrap();
                        let cond_code = emit_cmp(ctx, icmp, cond_code);

                        let cond_code = if op0 == Opcode::Brz {
                            cond_code.inverse()
                        } else {
                            cond_code
                        };

                        let cc = CC::from_intcc(cond_code);
                        ctx.emit(Inst::jmp_cond(cc, taken, not_taken));
                    } else if let Some(fcmp) = matches_input(ctx, flag_input, Opcode::Fcmp) {
                        let cond_code = ctx.data(fcmp).fp_cond_code().unwrap();
                        let cond_code = if op0 == Opcode::Brz {
                            cond_code.inverse()
                        } else {
                            cond_code
                        };
                        match emit_fcmp(ctx, fcmp, cond_code, FcmpSpec::Normal) {
                            FcmpCondResult::Condition(cc) => {
                                ctx.emit(Inst::jmp_cond(cc, taken, not_taken));
                            }
                            FcmpCondResult::AndConditions(cc1, cc2) => {
                                ctx.emit(Inst::jmp_if(cc1.invert(), not_taken));
                                ctx.emit(Inst::jmp_cond(cc2.invert(), not_taken, taken));
                            }
                            FcmpCondResult::OrConditions(cc1, cc2) => {
                                ctx.emit(Inst::jmp_if(cc1, taken));
                                ctx.emit(Inst::jmp_cond(cc2, taken, not_taken));
                            }
                            FcmpCondResult::InvertedEqualOrConditions(_, _) => unreachable!(),
                        }
                    } else if src_ty == types::I128 {
                        let src = put_input_in_regs(
                            ctx,
                            InsnInput {
                                insn: branches[0],
                                input: 0,
                            },
                        );
                        let (half_cc, comb_op) = match op0 {
                            Opcode::Brz => (CC::Z, AluRmiROpcode::And8),
                            Opcode::Brnz => (CC::NZ, AluRmiROpcode::Or8),
                            _ => unreachable!(),
                        };
                        let tmp1 = ctx.alloc_tmp(types::I64).only_reg().unwrap();
                        let tmp2 = ctx.alloc_tmp(types::I64).only_reg().unwrap();
                        ctx.emit(Inst::cmp_rmi_r(
                            OperandSize::Size64,
                            RegMemImm::imm(0),
                            src.regs()[0],
                        ));
                        ctx.emit(Inst::setcc(half_cc, tmp1));
                        ctx.emit(Inst::cmp_rmi_r(
                            OperandSize::Size64,
                            RegMemImm::imm(0),
                            src.regs()[1],
                        ));
                        ctx.emit(Inst::setcc(half_cc, tmp2));
                        ctx.emit(Inst::alu_rmi_r(
                            OperandSize::Size32,
                            comb_op,
                            RegMemImm::reg(tmp1.to_reg()),
                            tmp2,
                        ));
                        ctx.emit(Inst::jmp_cond(CC::NZ, taken, not_taken));
                    } else if is_int_or_ref_ty(src_ty) || is_bool_ty(src_ty) {
                        let src = put_input_in_reg(
                            ctx,
                            InsnInput {
                                insn: branches[0],
                                input: 0,
                            },
                        );
                        let cc = match op0 {
                            Opcode::Brz => CC::Z,
                            Opcode::Brnz => CC::NZ,
                            _ => unreachable!(),
                        };
                        // See case for `Opcode::Select` above re: testing the
                        // boolean input.
                        let test_input = if src_ty == types::B1 {
                            // test src, 1
                            RegMemImm::imm(1)
                        } else {
                            assert!(!is_bool_ty(src_ty));
                            // test src, src
                            RegMemImm::reg(src)
                        };

                        ctx.emit(Inst::test_rmi_r(
                            OperandSize::from_ty(src_ty),
                            test_input,
                            src,
                        ));
                        ctx.emit(Inst::jmp_cond(cc, taken, not_taken));
                    } else {
                        unimplemented!("brz/brnz with non-int type {:?}", src_ty);
                    }
                }

                Opcode::BrIcmp => {
                    let src_ty = ctx.input_ty(branches[0], 0);
                    if is_int_or_ref_ty(src_ty) || is_bool_ty(src_ty) {
                        let lhs = put_input_in_reg(
                            ctx,
                            InsnInput {
                                insn: branches[0],
                                input: 0,
                            },
                        );
                        let rhs = input_to_reg_mem_imm(
                            ctx,
                            InsnInput {
                                insn: branches[0],
                                input: 1,
                            },
                        );
                        let cc = CC::from_intcc(ctx.data(branches[0]).cond_code().unwrap());
                        // Cranelift's icmp semantics want to compare lhs - rhs, while Intel gives
                        // us dst - src at the machine instruction level, so invert operands.
                        ctx.emit(Inst::cmp_rmi_r(OperandSize::from_ty(src_ty), rhs, lhs));
                        ctx.emit(Inst::jmp_cond(cc, taken, not_taken));
                    } else {
                        unimplemented!("bricmp with non-int type {:?}", src_ty);
                    }
                }

                Opcode::Brif => {
                    let flag_input = InsnInput {
                        insn: branches[0],
                        input: 0,
                    };

                    if let Some(ifcmp) = matches_input(ctx, flag_input, Opcode::Ifcmp) {
                        let cond_code = ctx.data(branches[0]).cond_code().unwrap();
                        let cond_code = emit_cmp(ctx, ifcmp, cond_code);
                        let cc = CC::from_intcc(cond_code);
                        ctx.emit(Inst::jmp_cond(cc, taken, not_taken));
                    } else if let Some(ifcmp_sp) = matches_input(ctx, flag_input, Opcode::IfcmpSp) {
                        let operand = put_input_in_reg(
                            ctx,
                            InsnInput {
                                insn: ifcmp_sp,
                                input: 0,
                            },
                        );
                        let ty = ctx.input_ty(ifcmp_sp, 0);
                        ctx.emit(Inst::cmp_rmi_r(
                            OperandSize::from_ty(ty),
                            RegMemImm::reg(regs::rsp()),
                            operand,
                        ));
                        let cond_code = ctx.data(branches[0]).cond_code().unwrap();
                        let cc = CC::from_intcc(cond_code);
                        ctx.emit(Inst::jmp_cond(cc, taken, not_taken));
                    } else {
                        // Should be disallowed by flags checks in verifier.
                        unimplemented!("Brif with non-ifcmp input");
                    }
                }
                Opcode::Brff => {
                    let flag_input = InsnInput {
                        insn: branches[0],
                        input: 0,
                    };

                    if let Some(ffcmp) = matches_input(ctx, flag_input, Opcode::Ffcmp) {
                        let cond_code = ctx.data(branches[0]).fp_cond_code().unwrap();
                        match emit_fcmp(ctx, ffcmp, cond_code, FcmpSpec::Normal) {
                            FcmpCondResult::Condition(cc) => {
                                ctx.emit(Inst::jmp_cond(cc, taken, not_taken));
                            }
                            FcmpCondResult::AndConditions(cc1, cc2) => {
                                ctx.emit(Inst::jmp_if(cc1.invert(), not_taken));
                                ctx.emit(Inst::jmp_cond(cc2.invert(), not_taken, taken));
                            }
                            FcmpCondResult::OrConditions(cc1, cc2) => {
                                ctx.emit(Inst::jmp_if(cc1, taken));
                                ctx.emit(Inst::jmp_cond(cc2, taken, not_taken));
                            }
                            FcmpCondResult::InvertedEqualOrConditions(_, _) => unreachable!(),
                        }
                    } else {
                        // Should be disallowed by flags checks in verifier.
                        unimplemented!("Brff with input not from ffcmp");
                    }
                }

                _ => panic!("unexpected branch opcode: {:?}", op0),
            }
        } else {
            assert_eq!(branches.len(), 1);

            // Must be an unconditional branch or trap.
            let op = ctx.data(branches[0]).opcode();
            match op {
                Opcode::Jump | Opcode::Fallthrough => {
                    ctx.emit(Inst::jmp_known(targets[0]));
                }

                Opcode::BrTable => {
                    let jt_size = targets.len() - 1;
                    assert!(jt_size <= u32::max_value() as usize);
                    let jt_size = jt_size as u32;

                    let idx = extend_input_to_reg(
                        ctx,
                        InsnInput {
                            insn: branches[0],
                            input: 0,
                        },
                        ExtSpec::ZeroExtendTo32,
                    );

                    // Bounds-check (compute flags from idx - jt_size) and branch to default.
                    ctx.emit(Inst::cmp_rmi_r(
                        OperandSize::Size32,
                        RegMemImm::imm(jt_size),
                        idx,
                    ));

                    // Emit the compound instruction that does:
                    //
                    // lea $jt, %rA
                    // movsbl [%rA, %rIndex, 2], %rB
                    // add %rB, %rA
                    // j *%rA
                    // [jt entries]
                    //
                    // This must be *one* instruction in the vcode because we cannot allow regalloc
                    // to insert any spills/fills in the middle of the sequence; otherwise, the
                    // lea PC-rel offset to the jumptable would be incorrect.  (The alternative
                    // is to introduce a relocation pass for inlined jumptables, which is much
                    // worse.)

                    // This temporary is used as a signed integer of 64-bits (to hold addresses).
                    let tmp1 = ctx.alloc_tmp(types::I64).only_reg().unwrap();
                    // This temporary is used as a signed integer of 32-bits (for the wasm-table
                    // index) and then 64-bits (address addend). The small lie about the I64 type
                    // is benign, since the temporary is dead after this instruction (and its
                    // Cranelift type is thus unused).
                    let tmp2 = ctx.alloc_tmp(types::I64).only_reg().unwrap();

                    let targets_for_term: Vec<MachLabel> = targets.to_vec();
                    let default_target = targets[0];

                    let jt_targets: Vec<MachLabel> = targets.iter().skip(1).cloned().collect();

                    ctx.emit(Inst::JmpTableSeq {
                        idx,
                        tmp1,
                        tmp2,
                        default_target,
                        targets: jt_targets,
                        targets_for_term,
                    });
                }

                _ => panic!("Unknown branch type {:?}", op),
            }
        }

        Ok(())
    }

    fn maybe_pinned_reg(&self) -> Option<Reg> {
        Some(regs::pinned_reg())
    }
}
