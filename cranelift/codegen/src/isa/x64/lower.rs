//! Lowering rules for X64.

use crate::data_value::DataValue;
use crate::ir::{
    condcodes::FloatCC, condcodes::IntCC, types, AbiParam, ArgumentPurpose, ExternalName,
    Inst as IRInst, InstructionData, LibCall, Opcode, Signature, Type,
};
use crate::isa::x64::abi::*;
use crate::isa::x64::inst::args::*;
use crate::isa::x64::inst::*;
use crate::isa::{x64::X64Backend, CallConv};
use crate::machinst::lower::*;
use crate::machinst::*;
use crate::result::CodegenResult;
use crate::settings::Flags;
use alloc::boxed::Box;
use alloc::vec::Vec;
use cranelift_codegen_shared::condcodes::CondCode;
use log::trace;
use regalloc::{Reg, RegClass, Writable};
use smallvec::SmallVec;
use std::convert::TryFrom;
use target_lexicon::Triple;

/// Context passed to all lowering functions.
type Ctx<'a> = &'a mut dyn LowerCtx<I = Inst>;

//=============================================================================
// Helpers for instruction lowering.

fn is_int_or_ref_ty(ty: Type) -> bool {
    match ty {
        types::I8 | types::I16 | types::I32 | types::I64 | types::R64 => true,
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
    let inputs = ctx.get_input(input.insn, input.input);
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
    let inputs = ctx.get_input(input.insn, input.input);
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

fn lowerinput_to_reg(ctx: Ctx, input: LowerInput) -> Reg {
    ctx.use_input_reg(input);
    input.reg
}

/// Put the given input into a register, and mark it as used (side-effect).
fn put_input_in_reg(ctx: Ctx, spec: InsnInput) -> Reg {
    let input = ctx.get_input(spec.insn, spec.input);

    if let Some(c) = input.constant {
        // Generate constants fresh at each use to minimize long-range register pressure.
        let ty = ctx.input_ty(spec.insn, spec.input);
        let from_bits = ty_bits(ty);
        let masked = if from_bits < 64 {
            c & ((1u64 << from_bits) - 1)
        } else {
            c
        };

        let cst_copy = ctx.alloc_tmp(Inst::rc_for_type(ty).unwrap(), ty);
        for inst in Inst::gen_constant(cst_copy, masked, ty, |reg_class, ty| {
            ctx.alloc_tmp(reg_class, ty)
        })
        .into_iter()
        {
            ctx.emit(inst);
        }
        cst_copy.to_reg()
    } else {
        lowerinput_to_reg(ctx, input)
    }
}

/// An extension specification for `extend_input_to_reg`.
#[derive(Clone, Copy)]
enum ExtSpec {
    ZeroExtendTo32,
    ZeroExtendTo64,
    SignExtendTo32,
    SignExtendTo64,
}

/// Put the given input into a register, marking it as used, and do a zero- or signed- extension if
/// required. (This obviously causes side-effects.)
fn extend_input_to_reg(ctx: Ctx, spec: InsnInput, ext_spec: ExtSpec) -> Reg {
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
    let dst = ctx.alloc_tmp(RegClass::I64, requested_ty);
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

fn lowerinput_to_reg_mem(ctx: Ctx, input: LowerInput) -> RegMem {
    // TODO handle memory.
    RegMem::reg(lowerinput_to_reg(ctx, input))
}

/// Put the given input into a register or a memory operand.
/// Effectful: may mark the given input as used, when returning the register form.
fn input_to_reg_mem(ctx: Ctx, spec: InsnInput) -> RegMem {
    let input = ctx.get_input(spec.insn, spec.input);
    lowerinput_to_reg_mem(ctx, input)
}

/// Returns whether the given input is an immediate that can be properly sign-extended, without any
/// possible side-effect.
fn lowerinput_to_sext_imm(input: LowerInput, input_ty: Type) -> Option<u32> {
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

fn input_to_sext_imm(ctx: Ctx, spec: InsnInput) -> Option<u32> {
    let input = ctx.get_input(spec.insn, spec.input);
    let input_ty = ctx.input_ty(spec.insn, spec.input);
    lowerinput_to_sext_imm(input, input_ty)
}

fn input_to_imm(ctx: Ctx, spec: InsnInput) -> Option<u64> {
    ctx.get_input(spec.insn, spec.input).constant
}

/// Put the given input into an immediate, a register or a memory operand.
/// Effectful: may mark the given input as used, when returning the register form.
fn input_to_reg_mem_imm(ctx: Ctx, spec: InsnInput) -> RegMemImm {
    let input = ctx.get_input(spec.insn, spec.input);
    let input_ty = ctx.input_ty(spec.insn, spec.input);
    match lowerinput_to_sext_imm(input, input_ty) {
        Some(x) => RegMemImm::imm(x),
        None => match lowerinput_to_reg_mem(ctx, input) {
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
        let (sse_op, is64) = match ty.lane_bits() {
            8 => (SseOpcode::Pinsrb, false),
            16 => (SseOpcode::Pinsrw, false),
            32 => (SseOpcode::Pinsrd, false),
            64 => (SseOpcode::Pinsrd, true),
            _ => panic!("Unable to insertlane for lane size: {}", ty.lane_bits()),
        };
        ctx.emit(Inst::xmm_rm_r_imm(sse_op, src, dst, lane, is64));
    } else if ty == types::F32 {
        let sse_op = SseOpcode::Insertps;
        // Insert 32-bits from replacement (at index 00, bits 7:8) to vector (lane
        // shifted into bits 5:6).
        let lane = 0b00_00_00_00 | lane << 4;
        ctx.emit(Inst::xmm_rm_r_imm(sse_op, src, dst, lane, false));
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
        let (sse_op, is64) = match ty.lane_bits() {
            8 => (SseOpcode::Pextrb, false),
            16 => (SseOpcode::Pextrw, false),
            32 => (SseOpcode::Pextrd, false),
            64 => (SseOpcode::Pextrd, true),
            _ => panic!("Unable to extractlane for lane size: {}", ty.lane_bits()),
        };
        let src = RegMem::reg(src);
        ctx.emit(Inst::xmm_rm_r_imm(sse_op, src, dst, lane, is64));
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
                types::F32 => 0b00_00_00_00 | lane,
                // Move the value at `lane` 1 (we know it must be 1 because of the `if`
                // statement above) to lane 0 and leave lane 1 unchanged. The Cranelift type
                // checking assumption also applies here.
                types::F64 => 0b11_10_11_10,
                _ => unreachable!(),
            };
            let src = RegMem::reg(src);
            ctx.emit(Inst::xmm_rm_r_imm(sse_op, src, dst, mask, false));
        }
    } else {
        panic!("unable to emit extractlane for type: {}", ty)
    }
}

/// Emits an int comparison instruction.
///
/// Note: make sure that there are no instructions modifying the flags between a call to this
/// function and the use of the flags!
fn emit_cmp(ctx: Ctx, insn: IRInst) {
    let ty = ctx.input_ty(insn, 0);

    let inputs = [InsnInput { insn, input: 0 }, InsnInput { insn, input: 1 }];

    // TODO Try to commute the operands (and invert the condition) if one is an immediate.
    let lhs = put_input_in_reg(ctx, inputs[0]);
    let rhs = input_to_reg_mem_imm(ctx, inputs[1]);

    // Cranelift's icmp semantics want to compare lhs - rhs, while Intel gives
    // us dst - src at the machine instruction level, so invert operands.
    ctx.emit(Inst::cmp_rmi_r(ty.bytes() as u8, rhs, lhs));
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
fn emit_fcmp(ctx: Ctx, insn: IRInst, mut cond_code: FloatCC, spec: FcmpSpec) -> FcmpCondResult {
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
    let rhs = input_to_reg_mem(ctx, rhs_input);
    ctx.emit(Inst::xmm_cmp_rm_r(op, rhs, lhs));

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

fn make_libcall_sig(ctx: Ctx, insn: IRInst, call_conv: CallConv, ptr_ty: Type) -> Signature {
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

    let mut abi = X64ABICaller::from_func(&sig, &extname, dist, caller_conv)?;

    abi.emit_stack_pre_adjust(ctx);

    let vm_context = if call_conv.extends_baldrdash() { 1 } else { 0 };
    assert_eq!(inputs.len() + vm_context, abi.num_args());

    for (i, input) in inputs.iter().enumerate() {
        let arg_reg = put_input_in_reg(ctx, *input);
        abi.emit_copy_reg_to_arg(ctx, i, arg_reg);
    }
    if call_conv.extends_baldrdash() {
        let vm_context_vreg = ctx
            .get_vm_context()
            .expect("should have a VMContext to pass to libcall funcs");
        abi.emit_copy_reg_to_arg(ctx, inputs.len(), vm_context_vreg);
    }

    abi.emit_call(ctx);
    for (i, output) in outputs.iter().enumerate() {
        let retval_reg = get_output_reg(ctx, *output);
        abi.emit_copy_retval_to_reg(ctx, i, retval_reg);
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
                let input = ctx.get_input(add, i);

                // Try to pierce through uextend.
                if let Some(uextend) = matches_input(
                    ctx,
                    InsnInput {
                        insn: add,
                        input: i,
                    },
                    Opcode::Uextend,
                ) {
                    if let Some(cst) = ctx.get_input(uextend, 0).constant {
                        // Zero the upper bits.
                        let input_size = ctx.input_ty(uextend, 0).bits() as u64;
                        let shift: u64 = 64 - input_size;
                        let uext_cst: u64 = (cst << shift) >> shift;

                        let final_offset = (offset as i64).wrapping_add(uext_cst as i64);
                        if low32_will_sign_extend_to_64(final_offset as u64) {
                            let base = put_input_in_reg(ctx, add_inputs[1 - i]);
                            return Amode::imm_reg(final_offset as u32, base);
                        }
                    }
                }

                // If it's a constant, add it directly!
                if let Some(cst) = input.constant {
                    let final_offset = (offset as i64).wrapping_add(cst as i64);
                    if low32_will_sign_extend_to_64(final_offset as u64) {
                        let base = put_input_in_reg(ctx, add_inputs[1 - i]);
                        return Amode::imm_reg(final_offset as u32, base);
                    }
                }
            }

            (
                put_input_in_reg(ctx, add_inputs[0]),
                put_input_in_reg(ctx, add_inputs[1]),
                0,
            )
        };

        return Amode::imm_reg_reg_shift(offset as u32, base, index, shift);
    }

    let input = put_input_in_reg(ctx, spec);
    Amode::imm_reg(offset as u32, input)
}

//=============================================================================
// Top-level instruction lowering entry point, for one instruction.

/// Actually codegen an instruction's results into registers.
fn lower_insn_to_regs<C: LowerCtx<I = Inst>>(
    ctx: &mut C,
    insn: IRInst,
    flags: &Flags,
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
            for inst in Inst::gen_constant(dst, value, ty.unwrap(), |reg_class, ty| {
                ctx.alloc_tmp(reg_class, ty)
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
        | Opcode::Imul
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
                    Opcode::Imul => match ty {
                        types::I16X8 => SseOpcode::Pmullw,
                        types::I32X4 => SseOpcode::Pmulld,
                        types::I64X2 => {
                            // Note for I64X2 we describe a lane A as being composed of a
                            // 32-bit upper half "Ah" and a 32-bit lower half "Al".
                            // The 32-bit long hand multiplication can then be written as:
                            //    Ah Al
                            // *  Bh Bl
                            //    -----
                            //    Al * Bl
                            // + (Ah * Bl) << 32
                            // + (Al * Bh) << 32
                            //
                            // So for each lane we will compute:
                            // A * B  = (Al * Bl) + ((Ah * Bl) + (Al * Bh)) << 32
                            //
                            // Note, the algorithm will use pmuldq which operates directly on
                            // the lower 32-bit (Al or Bl) of a lane and writes the result
                            // to the full 64-bits of the lane of the destination. For this
                            // reason we don't need shifts to isolate the lower 32-bits, however
                            // we will need to use shifts to isolate the high 32-bits when doing
                            // calculations, i.e. Ah == A >> 32
                            //
                            // The full sequence then is as follows:
                            // A' = A
                            // A' = A' >> 32
                            // A' = Ah' * Bl
                            // B' = B
                            // B' = B' >> 32
                            // B' = Bh' * Al
                            // B' = B' + A'
                            // B' = B' << 32
                            // A' = A
                            // A' = Al' * Bl
                            // A' = A' + B'
                            // dst = A'

                            // Get inputs rhs=A and lhs=B and the dst register
                            let lhs = put_input_in_reg(ctx, inputs[0]);
                            let rhs = put_input_in_reg(ctx, inputs[1]);
                            let dst = get_output_reg(ctx, outputs[0]);

                            // A' = A
                            let rhs_1 = ctx.alloc_tmp(RegClass::V128, types::I64X2);
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
                            let lhs_1 = ctx.alloc_tmp(RegClass::V128, types::I64X2);
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
                            return Ok(());
                        }
                        _ => panic!("Unsupported type for packed imul instruction: {}", ty),
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
                let dst = get_output_reg(ctx, outputs[0]);

                // Move the `lhs` to the same register as `dst`.
                ctx.emit(Inst::gen_move(dst, lhs, ty));
                ctx.emit(Inst::xmm_rm_r(sse_op, rhs, dst));
            } else {
                let is_64 = ty == types::I64;
                let alu_op = match op {
                    Opcode::Iadd | Opcode::IaddIfcout => AluRmiROpcode::Add,
                    Opcode::Isub => AluRmiROpcode::Sub,
                    Opcode::Imul => AluRmiROpcode::Mul,
                    Opcode::Band => AluRmiROpcode::And,
                    Opcode::Bor => AluRmiROpcode::Or,
                    Opcode::Bxor => AluRmiROpcode::Xor,
                    _ => unreachable!(),
                };

                let (lhs, rhs) = match op {
                    Opcode::Iadd
                    | Opcode::IaddIfcout
                    | Opcode::Imul
                    | Opcode::Band
                    | Opcode::Bor
                    | Opcode::Bxor => {
                        // For commutative operations, try to commute operands if one is an
                        // immediate.
                        if let Some(imm) = input_to_sext_imm(ctx, inputs[0]) {
                            (put_input_in_reg(ctx, inputs[1]), RegMemImm::imm(imm))
                        } else {
                            (
                                put_input_in_reg(ctx, inputs[0]),
                                input_to_reg_mem_imm(ctx, inputs[1]),
                            )
                        }
                    }
                    Opcode::Isub => (
                        put_input_in_reg(ctx, inputs[0]),
                        input_to_reg_mem_imm(ctx, inputs[1]),
                    ),
                    _ => unreachable!(),
                };

                let dst = get_output_reg(ctx, outputs[0]);
                ctx.emit(Inst::mov_r_r(true, lhs, dst));
                ctx.emit(Inst::alu_rmi_r(is_64, alu_op, rhs, dst));
            }
        }

        Opcode::BandNot => {
            let ty = ty.unwrap();
            debug_assert!(ty.is_vector() && ty.bytes() == 16);
            let lhs = input_to_reg_mem(ctx, inputs[0]);
            let rhs = put_input_in_reg(ctx, inputs[1]);
            let dst = get_output_reg(ctx, outputs[0]);
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
            let dst = get_output_reg(ctx, outputs[0]);
            let ty = ty.unwrap();
            if ty.is_vector() {
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
            let dst = get_output_reg(ctx, outputs[0]);
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
            let size = ty.bytes() as u8;
            let src = put_input_in_reg(ctx, inputs[0]);
            let dst = get_output_reg(ctx, outputs[0]);
            ctx.emit(Inst::gen_move(dst, src, ty));

            if ty.is_vector() {
                let tmp = ctx.alloc_tmp(RegClass::V128, ty);
                ctx.emit(Inst::equals(ty, RegMem::from(tmp), tmp));
                ctx.emit(Inst::xor(ty, RegMem::from(tmp), dst));
            } else if ty.is_bool() {
                unimplemented!("bool bnot")
            } else {
                ctx.emit(Inst::not(size, dst));
            }
        }

        Opcode::Bitselect => {
            let ty = ty.unwrap();
            let condition = put_input_in_reg(ctx, inputs[0]);
            let if_true = put_input_in_reg(ctx, inputs[1]);
            let if_false = input_to_reg_mem(ctx, inputs[2]);
            let dst = get_output_reg(ctx, outputs[0]);

            if ty.is_vector() {
                let tmp1 = ctx.alloc_tmp(RegClass::V128, ty);
                ctx.emit(Inst::gen_move(tmp1, if_true, ty));
                ctx.emit(Inst::and(ty, RegMem::reg(condition.clone()), tmp1));

                let tmp2 = ctx.alloc_tmp(RegClass::V128, ty);
                ctx.emit(Inst::gen_move(tmp2, condition, ty));
                ctx.emit(Inst::and_not(ty, if_false, tmp2));

                ctx.emit(Inst::gen_move(dst, tmp2.to_reg(), ty));
                ctx.emit(Inst::or(ty, RegMem::from(tmp1), dst));
            } else {
                unimplemented!("scalar bitselect")
            }
        }

        Opcode::Ishl | Opcode::Ushr | Opcode::Sshr | Opcode::Rotl | Opcode::Rotr => {
            let dst_ty = ctx.output_ty(insn, 0);
            debug_assert_eq!(ctx.input_ty(insn, 0), dst_ty);

            if !dst_ty.is_vector() {
                // Scalar shifts on x86 have various encodings:
                // - shift by one bit, e.g. `SAL r/m8, 1` (not used here)
                // - shift by an immediate amount, e.g. `SAL r/m8, imm8`
                // - shift by a dynamic amount but only from the CL register, e.g. `SAL r/m8, CL`.
                // This implementation uses the last two encoding methods.
                let (size, lhs) = match dst_ty {
                    types::I8 | types::I16 => match op {
                        Opcode::Ishl => (4, put_input_in_reg(ctx, inputs[0])),
                        Opcode::Ushr => (
                            4,
                            extend_input_to_reg(ctx, inputs[0], ExtSpec::ZeroExtendTo32),
                        ),
                        Opcode::Sshr => (
                            4,
                            extend_input_to_reg(ctx, inputs[0], ExtSpec::SignExtendTo32),
                        ),
                        Opcode::Rotl | Opcode::Rotr => {
                            (dst_ty.bytes() as u8, put_input_in_reg(ctx, inputs[0]))
                        }
                        _ => unreachable!(),
                    },
                    types::I32 | types::I64 => {
                        (dst_ty.bytes() as u8, put_input_in_reg(ctx, inputs[0]))
                    }
                    _ => unreachable!("unhandled output type for shift/rotates: {}", dst_ty),
                };

                let (count, rhs) = if let Some(cst) = ctx.get_input(insn, 1).constant {
                    // Mask count, according to Cranelift's semantics.
                    let cst = (cst as u8) & (dst_ty.bits() as u8 - 1);
                    (Some(cst), None)
                } else {
                    (None, Some(put_input_in_reg(ctx, inputs[1])))
                };

                let dst = get_output_reg(ctx, outputs[0]);

                let shift_kind = match op {
                    Opcode::Ishl => ShiftKind::ShiftLeft,
                    Opcode::Ushr => ShiftKind::ShiftRightLogical,
                    Opcode::Sshr => ShiftKind::ShiftRightArithmetic,
                    Opcode::Rotl => ShiftKind::RotateLeft,
                    Opcode::Rotr => ShiftKind::RotateRight,
                    _ => unreachable!(),
                };

                let w_rcx = Writable::from_reg(regs::rcx());
                ctx.emit(Inst::mov_r_r(true, lhs, dst));
                if count.is_none() {
                    ctx.emit(Inst::mov_r_r(true, rhs.unwrap(), w_rcx));
                }
                ctx.emit(Inst::shift_r(size, shift_kind, count, dst));
            } else if dst_ty == types::I8X16 && (op == Opcode::Ishl || op == Opcode::Ushr) {
                // Since the x86 instruction set does not have any 8x16 shift instructions (even in higher feature sets
                // like AVX), we lower the `ishl.i8x16` and `ushr.i8x16` to a sequence of instructions. The basic idea,
                // whether the `shift_by` amount is an immediate or not, is to use a 16x8 shift and then mask off the
                // incorrect bits to 0s (see below for handling signs in `sshr.i8x16`).
                let src = put_input_in_reg(ctx, inputs[0]);
                let shift_by = input_to_reg_mem_imm(ctx, inputs[1]);
                let dst = get_output_reg(ctx, outputs[0]);

                // If necessary, move the shift index into the lowest bits of a vector register.
                let shift_by_moved = match &shift_by {
                    RegMemImm::Imm { .. } => shift_by.clone(),
                    RegMemImm::Reg { reg } => {
                        let tmp_shift_by = ctx.alloc_tmp(RegClass::V128, dst_ty);
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

                // Choose which mask to use to fixup the shifted lanes.
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
                        let base_mask_address = ctx.alloc_tmp(RegClass::I64, types::I64);
                        let mask_offset = ctx.alloc_tmp(RegClass::I64, types::I64);
                        let mask_constant = ctx.use_constant(VCodeConstantData::WellKnown(mask));
                        ctx.emit(Inst::lea(
                            SyntheticAmode::ConstantOffset(mask_constant),
                            base_mask_address,
                        ));
                        ctx.emit(Inst::gen_move(mask_offset, reg, types::I64));
                        ctx.emit(Inst::shift_r(8, ShiftKind::ShiftLeft, Some(4), mask_offset));
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
                let mask_value = ctx.alloc_tmp(RegClass::V128, dst_ty);
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
                // approach here: separate the low and high lanes, shift then separately, and merge them into the final
                // result. Visually, this looks like the following, where `src.i8x16 = [s0, s1, ..., s15]:
                //   low.i16x8 = [(s0, s0), (s1, s1), ..., (s7, s7)]
                //   shifted_low.i16x8 = shift each lane of `low`
                //   high.i16x8 = [(s8, s8), (s9, s9), ..., (s15, s15)]
                //   shifted_high.i16x8 = shift each lane of `high`
                //   dst.i8x16 = [s0'', s1'', ..., s15'']
                let src = put_input_in_reg(ctx, inputs[0]);
                let shift_by = input_to_reg_mem_imm(ctx, inputs[1]);
                let shift_by_ty = ctx.input_ty(insn, 1);
                let dst = get_output_reg(ctx, outputs[0]);

                // In order for PACKSSWB later to only use the high byte of each 16x8 lane, we shift right an extra 8
                // bits, relying on PSRAW to fill in the upper bits appropriately.
                let bigger_shift_by = match shift_by {
                    // When we know the shift amount at compile time, we add the extra shift amount statically.
                    RegMemImm::Imm { simm32 } => RegMemImm::imm(simm32 + 8),
                    // Otherwise we add instructions to add the extra shift amount and move the value into an XMM
                    // register.
                    RegMemImm::Reg { reg } => {
                        let bigger_shift_by_gpr = ctx.alloc_tmp(RegClass::I64, shift_by_ty);
                        ctx.emit(Inst::mov_r_r(true, reg, bigger_shift_by_gpr));

                        let is_64 = shift_by_ty == types::I64;
                        let imm = RegMemImm::imm(8);
                        ctx.emit(Inst::alu_rmi_r(
                            is_64,
                            AluRmiROpcode::Add,
                            imm,
                            bigger_shift_by_gpr,
                        ));

                        let bigger_shift_by_xmm = ctx.alloc_tmp(RegClass::V128, dst_ty);
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
                let upper_lanes = ctx.alloc_tmp(RegClass::V128, dst_ty);
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
                // like AVX512VL and AVX512F include VPSRAQ, a 128-bit instruction that would fit here, but this backend
                // does not currently have support for EVEX encodings (TODO when EVEX support is available, add an
                // alternate lowering here). To remedy this, we extract each 64-bit lane to a GPR, shift each using a
                // scalar instruction, and insert the shifted values back in the `dst` XMM register.
                let src = put_input_in_reg(ctx, inputs[0]);
                let dst = get_output_reg(ctx, outputs[0]);
                ctx.emit(Inst::gen_move(dst, src, dst_ty));

                // Extract the upper and lower lanes into temporary GPRs.
                let lower_lane = ctx.alloc_tmp(RegClass::I64, types::I64);
                emit_extract_lane(ctx, src, lower_lane, 0, types::I64);
                let upper_lane = ctx.alloc_tmp(RegClass::I64, types::I64);
                emit_extract_lane(ctx, src, upper_lane, 1, types::I64);

                // Shift each value.
                let mut shift = |reg: Writable<Reg>| {
                    let kind = ShiftKind::ShiftRightArithmetic;
                    if let Some(shift_by) = ctx.get_input(insn, 1).constant {
                        // Mask the shift amount according to Cranelift's semantics.
                        let shift_by = (shift_by as u8) & (types::I64.bits() as u8 - 1);
                        ctx.emit(Inst::shift_r(8, kind, Some(shift_by), reg));
                    } else {
                        let dynamic_shift_by = put_input_in_reg(ctx, inputs[1]);
                        let w_rcx = Writable::from_reg(regs::rcx());
                        ctx.emit(Inst::mov_r_r(true, dynamic_shift_by, w_rcx));
                        ctx.emit(Inst::shift_r(8, kind, None, reg));
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
                let dst = get_output_reg(ctx, outputs[0]);
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
                        let tmp_shift_by = ctx.alloc_tmp(RegClass::V128, dst_ty);
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
            let dst = get_output_reg(ctx, outputs[0]);
            let ty = ty.unwrap();

            if ty.is_vector() {
                // Zero's out a register and then does a packed subtraction
                // of the input from the register.

                let src = input_to_reg_mem(ctx, inputs[0]);
                let tmp = ctx.alloc_tmp(RegClass::V128, types::I32X4);

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
                let size = ty.bytes() as u8;
                let src = put_input_in_reg(ctx, inputs[0]);
                ctx.emit(Inst::gen_move(dst, src, ty));
                ctx.emit(Inst::neg(size, dst));
            }
        }

        Opcode::Clz => {
            // TODO when the x86 flags have use_lzcnt, we can use LZCNT.

            // General formula using bit-scan reverse (BSR):
            // mov -1, %dst
            // bsr %src, %tmp
            // cmovz %dst, %tmp
            // mov $(size_bits - 1), %dst
            // sub %tmp, %dst

            let (ext_spec, ty) = match ctx.input_ty(insn, 0) {
                types::I8 | types::I16 => (Some(ExtSpec::ZeroExtendTo32), types::I32),
                a if a == types::I32 || a == types::I64 => (None, a),
                _ => unreachable!(),
            };

            let src = if let Some(ext_spec) = ext_spec {
                RegMem::reg(extend_input_to_reg(ctx, inputs[0], ext_spec))
            } else {
                input_to_reg_mem(ctx, inputs[0])
            };
            let dst = get_output_reg(ctx, outputs[0]);

            let tmp = ctx.alloc_tmp(RegClass::I64, ty);
            ctx.emit(Inst::imm(
                OperandSize::from_bytes(ty.bytes()),
                u64::max_value(),
                dst,
            ));

            ctx.emit(Inst::unary_rm_r(
                ty.bytes() as u8,
                UnaryRmROpcode::Bsr,
                src,
                tmp,
            ));

            ctx.emit(Inst::cmove(
                ty.bytes() as u8,
                CC::Z,
                RegMem::reg(dst.to_reg()),
                tmp,
            ));

            ctx.emit(Inst::imm(
                OperandSize::from_bytes(ty.bytes()),
                ty.bits() as u64 - 1,
                dst,
            ));

            ctx.emit(Inst::alu_rmi_r(
                ty == types::I64,
                AluRmiROpcode::Sub,
                RegMemImm::reg(tmp.to_reg()),
                dst,
            ));
        }

        Opcode::Ctz => {
            // TODO when the x86 flags have use_bmi1, we can use TZCNT.

            // General formula using bit-scan forward (BSF):
            // bsf %src, %dst
            // mov $(size_bits), %tmp
            // cmovz %tmp, %dst
            let ty = ctx.input_ty(insn, 0);
            let ty = if ty.bits() < 32 { types::I32 } else { ty };
            debug_assert!(ty == types::I32 || ty == types::I64);

            let src = input_to_reg_mem(ctx, inputs[0]);
            let dst = get_output_reg(ctx, outputs[0]);

            let tmp = ctx.alloc_tmp(RegClass::I64, ty);
            ctx.emit(Inst::imm(OperandSize::Size32, ty.bits() as u64, tmp));

            ctx.emit(Inst::unary_rm_r(
                ty.bytes() as u8,
                UnaryRmROpcode::Bsf,
                src,
                dst,
            ));

            ctx.emit(Inst::cmove(
                ty.bytes() as u8,
                CC::Z,
                RegMem::reg(tmp.to_reg()),
                dst,
            ));
        }

        Opcode::Popcnt => {
            // TODO when the x86 flags have use_popcnt, we can use the popcnt instruction.

            let (ext_spec, ty) = match ctx.input_ty(insn, 0) {
                types::I8 | types::I16 => (Some(ExtSpec::ZeroExtendTo32), types::I32),
                a if a == types::I32 || a == types::I64 => (None, a),
                _ => unreachable!(),
            };

            let src = if let Some(ext_spec) = ext_spec {
                RegMem::reg(extend_input_to_reg(ctx, inputs[0], ext_spec))
            } else {
                input_to_reg_mem(ctx, inputs[0])
            };
            let dst = get_output_reg(ctx, outputs[0]);

            if ty == types::I64 {
                let is_64 = true;

                let tmp1 = ctx.alloc_tmp(RegClass::I64, types::I64);
                let tmp2 = ctx.alloc_tmp(RegClass::I64, types::I64);
                let cst = ctx.alloc_tmp(RegClass::I64, types::I64);

                // mov src, tmp1
                ctx.emit(Inst::mov64_rm_r(src.clone(), tmp1));

                // shr $1, tmp1
                ctx.emit(Inst::shift_r(
                    8,
                    ShiftKind::ShiftRightLogical,
                    Some(1),
                    tmp1,
                ));

                // mov 0x7777_7777_7777_7777, cst
                ctx.emit(Inst::imm(OperandSize::Size64, 0x7777777777777777, cst));

                // andq cst, tmp1
                ctx.emit(Inst::alu_rmi_r(
                    is_64,
                    AluRmiROpcode::And,
                    RegMemImm::reg(cst.to_reg()),
                    tmp1,
                ));

                // mov src, tmp2
                ctx.emit(Inst::mov64_rm_r(src, tmp2));

                // sub tmp1, tmp2
                ctx.emit(Inst::alu_rmi_r(
                    is_64,
                    AluRmiROpcode::Sub,
                    RegMemImm::reg(tmp1.to_reg()),
                    tmp2,
                ));

                // shr $1, tmp1
                ctx.emit(Inst::shift_r(
                    8,
                    ShiftKind::ShiftRightLogical,
                    Some(1),
                    tmp1,
                ));

                // and cst, tmp1
                ctx.emit(Inst::alu_rmi_r(
                    is_64,
                    AluRmiROpcode::And,
                    RegMemImm::reg(cst.to_reg()),
                    tmp1,
                ));

                // sub tmp1, tmp2
                ctx.emit(Inst::alu_rmi_r(
                    is_64,
                    AluRmiROpcode::Sub,
                    RegMemImm::reg(tmp1.to_reg()),
                    tmp2,
                ));

                // shr $1, tmp1
                ctx.emit(Inst::shift_r(
                    8,
                    ShiftKind::ShiftRightLogical,
                    Some(1),
                    tmp1,
                ));

                // and cst, tmp1
                ctx.emit(Inst::alu_rmi_r(
                    is_64,
                    AluRmiROpcode::And,
                    RegMemImm::reg(cst.to_reg()),
                    tmp1,
                ));

                // sub tmp1, tmp2
                ctx.emit(Inst::alu_rmi_r(
                    is_64,
                    AluRmiROpcode::Sub,
                    RegMemImm::reg(tmp1.to_reg()),
                    tmp2,
                ));

                // mov tmp2, dst
                ctx.emit(Inst::mov64_rm_r(RegMem::reg(tmp2.to_reg()), dst));

                // shr $4, dst
                ctx.emit(Inst::shift_r(8, ShiftKind::ShiftRightLogical, Some(4), dst));

                // add tmp2, dst
                ctx.emit(Inst::alu_rmi_r(
                    is_64,
                    AluRmiROpcode::Add,
                    RegMemImm::reg(tmp2.to_reg()),
                    dst,
                ));

                // mov $0x0F0F_0F0F_0F0F_0F0F, cst
                ctx.emit(Inst::imm(OperandSize::Size64, 0x0F0F0F0F0F0F0F0F, cst));

                // and cst, dst
                ctx.emit(Inst::alu_rmi_r(
                    is_64,
                    AluRmiROpcode::And,
                    RegMemImm::reg(cst.to_reg()),
                    dst,
                ));

                // mov $0x0101_0101_0101_0101, cst
                ctx.emit(Inst::imm(OperandSize::Size64, 0x0101010101010101, cst));

                // mul cst, dst
                ctx.emit(Inst::alu_rmi_r(
                    is_64,
                    AluRmiROpcode::Mul,
                    RegMemImm::reg(cst.to_reg()),
                    dst,
                ));

                // shr $56, dst
                ctx.emit(Inst::shift_r(
                    8,
                    ShiftKind::ShiftRightLogical,
                    Some(56),
                    dst,
                ));
            } else {
                assert_eq!(ty, types::I32);
                let is_64 = false;

                let tmp1 = ctx.alloc_tmp(RegClass::I64, types::I64);
                let tmp2 = ctx.alloc_tmp(RegClass::I64, types::I64);

                // mov src, tmp1
                ctx.emit(Inst::mov64_rm_r(src.clone(), tmp1));

                // shr $1, tmp1
                ctx.emit(Inst::shift_r(
                    4,
                    ShiftKind::ShiftRightLogical,
                    Some(1),
                    tmp1,
                ));

                // andq $0x7777_7777, tmp1
                ctx.emit(Inst::alu_rmi_r(
                    is_64,
                    AluRmiROpcode::And,
                    RegMemImm::imm(0x77777777),
                    tmp1,
                ));

                // mov src, tmp2
                ctx.emit(Inst::mov64_rm_r(src, tmp2));

                // sub tmp1, tmp2
                ctx.emit(Inst::alu_rmi_r(
                    is_64,
                    AluRmiROpcode::Sub,
                    RegMemImm::reg(tmp1.to_reg()),
                    tmp2,
                ));

                // shr $1, tmp1
                ctx.emit(Inst::shift_r(
                    4,
                    ShiftKind::ShiftRightLogical,
                    Some(1),
                    tmp1,
                ));

                // and 0x7777_7777, tmp1
                ctx.emit(Inst::alu_rmi_r(
                    is_64,
                    AluRmiROpcode::And,
                    RegMemImm::imm(0x77777777),
                    tmp1,
                ));

                // sub tmp1, tmp2
                ctx.emit(Inst::alu_rmi_r(
                    is_64,
                    AluRmiROpcode::Sub,
                    RegMemImm::reg(tmp1.to_reg()),
                    tmp2,
                ));

                // shr $1, tmp1
                ctx.emit(Inst::shift_r(
                    4,
                    ShiftKind::ShiftRightLogical,
                    Some(1),
                    tmp1,
                ));

                // and $0x7777_7777, tmp1
                ctx.emit(Inst::alu_rmi_r(
                    is_64,
                    AluRmiROpcode::And,
                    RegMemImm::imm(0x77777777),
                    tmp1,
                ));

                // sub tmp1, tmp2
                ctx.emit(Inst::alu_rmi_r(
                    is_64,
                    AluRmiROpcode::Sub,
                    RegMemImm::reg(tmp1.to_reg()),
                    tmp2,
                ));

                // mov tmp2, dst
                ctx.emit(Inst::mov64_rm_r(RegMem::reg(tmp2.to_reg()), dst));

                // shr $4, dst
                ctx.emit(Inst::shift_r(4, ShiftKind::ShiftRightLogical, Some(4), dst));

                // add tmp2, dst
                ctx.emit(Inst::alu_rmi_r(
                    is_64,
                    AluRmiROpcode::Add,
                    RegMemImm::reg(tmp2.to_reg()),
                    dst,
                ));

                // and $0x0F0F_0F0F, dst
                ctx.emit(Inst::alu_rmi_r(
                    is_64,
                    AluRmiROpcode::And,
                    RegMemImm::imm(0x0F0F0F0F),
                    dst,
                ));

                // mul $0x0101_0101, dst
                ctx.emit(Inst::alu_rmi_r(
                    is_64,
                    AluRmiROpcode::Mul,
                    RegMemImm::imm(0x01010101),
                    dst,
                ));

                // shr $24, dst
                ctx.emit(Inst::shift_r(
                    4,
                    ShiftKind::ShiftRightLogical,
                    Some(24),
                    dst,
                ));
            }
        }

        Opcode::IsNull | Opcode::IsInvalid => {
            // Null references are represented by the constant value 0; invalid references are
            // represented by the constant value -1. See `define_reftypes()` in
            // `meta/src/isa/x86/encodings.rs` to confirm.
            let src = put_input_in_reg(ctx, inputs[0]);
            let dst = get_output_reg(ctx, outputs[0]);
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
            ctx.emit(Inst::cmp_rmi_r(ty.bytes() as u8, RegMemImm::imm(imm), src));
            ctx.emit(Inst::setcc(CC::Z, dst));
        }

        Opcode::Uextend
        | Opcode::Sextend
        | Opcode::Bint
        | Opcode::Breduce
        | Opcode::Bextend
        | Opcode::Ireduce => {
            let src_ty = ctx.input_ty(insn, 0);
            let dst_ty = ctx.output_ty(insn, 0);

            // Sextend requires a sign-extended move, but all the other opcodes are simply a move
            // from a zero-extended source. Here is why this works, in each case:
            //
            // - Bint: Bool-to-int. We always represent a bool as a 0 or 1, so we merely need to
            // zero-extend here.
            //
            // - Breduce, Bextend: changing width of a boolean. We represent a bool as a 0 or 1, so
            // again, this is a zero-extend / no-op.
            //
            // - Ireduce: changing width of an integer. Smaller ints are stored with undefined
            // high-order bits, so we can simply do a copy.

            if src_ty == types::I32 && dst_ty == types::I64 && op != Opcode::Sextend {
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
                    let dst = get_output_reg(ctx, outputs[0]);
                    ctx.emit(Inst::gen_move(dst, src, types::I64));
                    return Ok(());
                }
            }

            let src = input_to_reg_mem(ctx, inputs[0]);
            let dst = get_output_reg(ctx, outputs[0]);

            let ext_mode = ExtMode::new(src_ty.bits(), dst_ty.bits());
            assert_eq!(
                src_ty.bits() < dst_ty.bits(),
                ext_mode.is_some(),
                "unexpected extension: {} -> {}",
                src_ty,
                dst_ty
            );

            if let Some(ext_mode) = ext_mode {
                if op == Opcode::Sextend {
                    ctx.emit(Inst::movsx_rm_r(ext_mode, src, dst));
                } else {
                    ctx.emit(Inst::movzx_rm_r(ext_mode, src, dst));
                }
            } else {
                ctx.emit(Inst::mov64_rm_r(src, dst));
            }
        }

        Opcode::Icmp => {
            let condcode = ctx.data(insn).cond_code().unwrap();
            let dst = get_output_reg(ctx, outputs[0]);
            let ty = ctx.input_ty(insn, 0);
            if !ty.is_vector() {
                emit_cmp(ctx, insn);
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

                // Here we decide which operand to use as the read/write `dst` (ModRM reg field)
                // and which to use as the read `input` (ModRM r/m field). In the normal case we
                // use Cranelift's first operand, the `lhs`, as `dst` but we flip the operands for
                // the less-than cases so that we can reuse the greater-than implementation.
                let input = match condcode {
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
                        let tmp = ctx.alloc_tmp(RegClass::V128, ty);
                        ctx.emit(Inst::xmm_rm_r(eq(ty), RegMem::from(tmp), tmp));
                        // Invert the result of the `PCMPEQ*`.
                        ctx.emit(Inst::xmm_rm_r(SseOpcode::Pxor, RegMem::from(tmp), dst));
                    }
                    IntCC::SignedGreaterThan | IntCC::SignedLessThan => {
                        ctx.emit(Inst::xmm_rm_r(gt(ty), input, dst))
                    }
                    IntCC::SignedGreaterThanOrEqual | IntCC::SignedLessThanOrEqual => {
                        ctx.emit(Inst::xmm_rm_r(mins(ty), input.clone(), dst));
                        ctx.emit(Inst::xmm_rm_r(eq(ty), input, dst))
                    }
                    IntCC::UnsignedGreaterThan | IntCC::UnsignedLessThan => {
                        ctx.emit(Inst::xmm_rm_r(maxu(ty), input.clone(), dst));
                        ctx.emit(Inst::xmm_rm_r(eq(ty), input, dst));
                        // Emit all 1s into the `tmp` register.
                        let tmp = ctx.alloc_tmp(RegClass::V128, ty);
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

                let dst = get_output_reg(ctx, outputs[0]);

                match emit_fcmp(ctx, insn, cond_code, FcmpSpec::Normal) {
                    FcmpCondResult::Condition(cc) => {
                        ctx.emit(Inst::setcc(cc, dst));
                    }
                    FcmpCondResult::AndConditions(cc1, cc2) => {
                        let tmp = ctx.alloc_tmp(RegClass::I64, types::I32);
                        ctx.emit(Inst::setcc(cc1, tmp));
                        ctx.emit(Inst::setcc(cc2, dst));
                        ctx.emit(Inst::alu_rmi_r(
                            false,
                            AluRmiROpcode::And,
                            RegMemImm::reg(tmp.to_reg()),
                            dst,
                        ));
                    }
                    FcmpCondResult::OrConditions(cc1, cc2) => {
                        let tmp = ctx.alloc_tmp(RegClass::I64, types::I32);
                        ctx.emit(Inst::setcc(cc1, tmp));
                        ctx.emit(Inst::setcc(cc2, dst));
                        ctx.emit(Inst::alu_rmi_r(
                            false,
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
                let dst = get_output_reg(ctx, outputs[0]);
                ctx.emit(Inst::gen_move(dst, lhs, input_ty));

                // Emit the comparison.
                ctx.emit(Inst::xmm_rm_r_imm(op, rhs, dst, imm.encode(), false));
            }
        }

        Opcode::FallthroughReturn | Opcode::Return => {
            for i in 0..ctx.num_inputs(insn) {
                let src_reg = put_input_in_reg(ctx, inputs[i]);
                let retval_reg = ctx.retval(i);
                let ty = ctx.input_ty(insn, i);
                ctx.emit(Inst::gen_move(retval_reg, src_reg, ty));
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
                        X64ABICaller::from_func(sig, &extname, dist, caller_conv)?,
                        &inputs[..],
                    )
                }

                Opcode::CallIndirect => {
                    let ptr = put_input_in_reg(ctx, inputs[0]);
                    let sig = ctx.call_sig(insn).unwrap();
                    assert_eq!(inputs.len() - 1, sig.params.len());
                    assert_eq!(outputs.len(), sig.returns.len());
                    (
                        X64ABICaller::from_ptr(sig, ptr, op, caller_conv)?,
                        &inputs[1..],
                    )
                }

                _ => unreachable!(),
            };

            abi.emit_stack_pre_adjust(ctx);
            assert_eq!(inputs.len(), abi.num_args());
            for (i, input) in inputs.iter().enumerate() {
                let arg_reg = put_input_in_reg(ctx, *input);
                abi.emit_copy_reg_to_arg(ctx, i, arg_reg);
            }
            abi.emit_call(ctx);
            for (i, output) in outputs.iter().enumerate() {
                let retval_reg = get_output_reg(ctx, *output);
                abi.emit_copy_retval_to_reg(ctx, i, retval_reg);
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
                let cc = CC::from_intcc(cond_code);

                // Verification ensures that the input is always a single-def ifcmp.
                let ifcmp = matches_input(ctx, inputs[0], Opcode::Ifcmp).unwrap();
                emit_cmp(ctx, ifcmp);

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
                        let tmp = ctx.alloc_tmp(RegClass::I64, types::I32);
                        let tmp2 = ctx.alloc_tmp(RegClass::I64, types::I32);
                        ctx.emit(Inst::setcc(cc1, tmp));
                        ctx.emit(Inst::setcc(cc2, tmp2));
                        ctx.emit(Inst::alu_rmi_r(
                            false, /* is_64 */
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
            for inst in Inst::gen_constant(dst, value, types::F64, |reg_class, ty| {
                ctx.alloc_tmp(reg_class, ty)
            }) {
                ctx.emit(inst);
            }
        }

        Opcode::F32const => {
            // TODO use cmpeqps for all 1s.
            let value = ctx.get_constant(insn).unwrap();
            let dst = get_output_reg(ctx, outputs[0]);
            for inst in Inst::gen_constant(dst, value, types::F32, |reg_class, ty| {
                ctx.alloc_tmp(reg_class, ty)
            }) {
                ctx.emit(inst);
            }
        }

        Opcode::Fadd | Opcode::Fsub | Opcode::Fmul | Opcode::Fdiv => {
            let lhs = put_input_in_reg(ctx, inputs[0]);
            let rhs = input_to_reg_mem(ctx, inputs[1]);
            let dst = get_output_reg(ctx, outputs[0]);
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
            let dst = get_output_reg(ctx, outputs[0]);
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
                    let tmp_xmm1 = ctx.alloc_tmp(RegClass::V128, output_ty);
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
                        false,
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
                    let tmp_xmm1 = ctx.alloc_tmp(RegClass::V128, types::F32);
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
                        false,
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
            let lhs = input_to_reg_mem(ctx, inputs[0]);
            let rhs = put_input_in_reg(ctx, inputs[1]);
            let dst = get_output_reg(ctx, outputs[0]);
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
            let src = input_to_reg_mem(ctx, inputs[0]);
            let dst = get_output_reg(ctx, outputs[0]);
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
            let src = input_to_reg_mem(ctx, inputs[0]);
            let dst = get_output_reg(ctx, outputs[0]);
            ctx.emit(Inst::xmm_unary_rm_r(SseOpcode::Cvtss2sd, src, dst));
        }

        Opcode::Fdemote => {
            let src = input_to_reg_mem(ctx, inputs[0]);
            let dst = get_output_reg(ctx, outputs[0]);
            ctx.emit(Inst::xmm_unary_rm_r(SseOpcode::Cvtsd2ss, src, dst));
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
                    None => input_to_reg_mem(ctx, inputs[0]),
                };

                let opcode = if output_ty == types::F32 {
                    SseOpcode::Cvtsi2ss
                } else {
                    assert_eq!(output_ty, types::F64);
                    SseOpcode::Cvtsi2sd
                };
                let dst = get_output_reg(ctx, outputs[0]);
                ctx.emit(Inst::gpr_to_xmm(opcode, src, src_size, dst));
            } else {
                let ty = ty.unwrap();
                let src = put_input_in_reg(ctx, inputs[0]);
                let dst = get_output_reg(ctx, outputs[0]);
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

        Opcode::FcvtFromUint => {
            let dst = get_output_reg(ctx, outputs[0]);
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

                        let src_copy = ctx.alloc_tmp(RegClass::I64, types::I64);
                        ctx.emit(Inst::gen_move(src_copy, src, types::I64));

                        let tmp_gpr1 = ctx.alloc_tmp(RegClass::I64, types::I64);
                        let tmp_gpr2 = ctx.alloc_tmp(RegClass::I64, types::I64);
                        ctx.emit(Inst::cvt_u64_to_float_seq(
                            ty == types::F64,
                            src_copy,
                            tmp_gpr1,
                            tmp_gpr2,
                            dst,
                        ));
                    }
                    _ => panic!("unexpected input type for FcvtFromUint: {:?}", input_ty),
                };
            } else {
                // Converting packed unsigned integers to packed floats requires a few steps.
                // There is no single instruction lowering for converting unsigned floats but there
                // is for converting packed signed integers to float (cvtdq2ps). In the steps below
                // we isolate the upper half (16 bits) and lower half (16 bits) of each lane and
                // then we convert each half separately using cvtdq2ps meant for signed integers.
                // In order for this to work for the upper half bits we must shift right by 1
                // (divide by 2) these bits in order to ensure the most significant bit is 0 not
                // signed, and then after the conversion we double the value. Finally we add the
                // converted values where addition will correctly round.
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

                assert_eq!(ctx.input_ty(insn, 0), types::I32X4);
                let src = put_input_in_reg(ctx, inputs[0]);
                let dst = get_output_reg(ctx, outputs[0]);

                // Create a temporary register
                let tmp = ctx.alloc_tmp(RegClass::V128, types::I32X4);
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

        Opcode::FcvtToUint | Opcode::FcvtToUintSat | Opcode::FcvtToSint | Opcode::FcvtToSintSat => {
            let src = put_input_in_reg(ctx, inputs[0]);
            let dst = get_output_reg(ctx, outputs[0]);

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

                let src_copy = ctx.alloc_tmp(RegClass::V128, input_ty);
                ctx.emit(Inst::gen_move(src_copy, src, input_ty));

                let tmp_xmm = ctx.alloc_tmp(RegClass::V128, input_ty);
                let tmp_gpr = ctx.alloc_tmp(RegClass::I64, output_ty);

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
                    let tmp = ctx.alloc_tmp(RegClass::V128, types::I32X4);
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
                        false,
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
                    unimplemented!("f32x4.convert_i32x4_u");
                } else {
                    // Since this branch is also guarded by a check for vector types
                    // neither Opcode::FcvtToUint nor Opcode::FcvtToSint can reach here
                    // due to vector varients not existing. The first two branches will
                    // cover all reachable cases.
                    unreachable!();
                }
            }
        }

        Opcode::Bitcast => {
            let input_ty = ctx.input_ty(insn, 0);
            let output_ty = ctx.output_ty(insn, 0);
            match (input_ty, output_ty) {
                (types::F32, types::I32) => {
                    let src = put_input_in_reg(ctx, inputs[0]);
                    let dst = get_output_reg(ctx, outputs[0]);
                    ctx.emit(Inst::xmm_to_gpr(
                        SseOpcode::Movd,
                        src,
                        dst,
                        OperandSize::Size32,
                    ));
                }
                (types::I32, types::F32) => {
                    let src = input_to_reg_mem(ctx, inputs[0]);
                    let dst = get_output_reg(ctx, outputs[0]);
                    ctx.emit(Inst::gpr_to_xmm(
                        SseOpcode::Movd,
                        src,
                        OperandSize::Size32,
                        dst,
                    ));
                }
                (types::F64, types::I64) => {
                    let src = put_input_in_reg(ctx, inputs[0]);
                    let dst = get_output_reg(ctx, outputs[0]);
                    ctx.emit(Inst::xmm_to_gpr(
                        SseOpcode::Movq,
                        src,
                        dst,
                        OperandSize::Size64,
                    ));
                }
                (types::I64, types::F64) => {
                    let src = input_to_reg_mem(ctx, inputs[0]);
                    let dst = get_output_reg(ctx, outputs[0]);
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
            let src = input_to_reg_mem(ctx, inputs[0]);
            let dst = get_output_reg(ctx, outputs[0]);

            // In both cases, generate a constant and apply a single binary instruction:
            // - to compute the absolute value, set all bits to 1 but the MSB to 0, and bit-AND the
            // src with it.
            // - to compute the negated value, set all bits to 0 but the MSB to 1, and bit-XOR the
            // src with it.
            let output_ty = ty.unwrap();
            if !output_ty.is_vector() {
                let (val, opcode) = match output_ty {
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

                for inst in Inst::gen_constant(dst, val, output_ty, |reg_class, ty| {
                    ctx.alloc_tmp(reg_class, ty)
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
                    // have used CMPPD with the same effect.
                    let tmp = ctx.alloc_tmp(RegClass::V128, output_ty);
                    let cond = FcmpImm::from(FloatCC::Equal);
                    let cmpps = Inst::xmm_rm_r_imm(
                        SseOpcode::Cmpps,
                        RegMem::reg(tmp.to_reg()),
                        tmp,
                        cond.encode(),
                        false,
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
            let dst = get_output_reg(ctx, outputs[0]);
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

            let tmp_xmm1 = ctx.alloc_tmp(RegClass::V128, types::F32);
            let tmp_xmm2 = ctx.alloc_tmp(RegClass::V128, types::F32);

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

            for inst in Inst::gen_constant(tmp_xmm1, sign_bit_cst, ty, |reg_class, ty| {
                ctx.alloc_tmp(reg_class, ty)
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
            // TODO use ROUNDSS/ROUNDSD after sse4.1.

            // Lower to VM calls when there's no access to SSE4.1.
            let ty = ty.unwrap();
            let libcall = match (ty, op) {
                (types::F32, Opcode::Ceil) => LibCall::CeilF32,
                (types::F64, Opcode::Ceil) => LibCall::CeilF64,
                (types::F32, Opcode::Floor) => LibCall::FloorF32,
                (types::F64, Opcode::Floor) => LibCall::FloorF64,
                (types::F32, Opcode::Nearest) => LibCall::NearestF32,
                (types::F64, Opcode::Nearest) => LibCall::NearestF64,
                (types::F32, Opcode::Trunc) => LibCall::TruncF32,
                (types::F64, Opcode::Trunc) => LibCall::TruncF64,
                _ => panic!(
                    "unexpected type/opcode {:?}/{:?} in Ceil/Floor/Nearest/Trunc",
                    ty, op
                ),
            };

            emit_vm_call(ctx, flags, triple, libcall, insn, inputs, outputs)?;
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
        | Opcode::Sload32Complex => {
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
                | Opcode::Sload32Complex => true,
                _ => false,
            };

            let amode = match op {
                Opcode::Load
                | Opcode::Uload8
                | Opcode::Sload8
                | Opcode::Uload16
                | Opcode::Sload16
                | Opcode::Uload32
                | Opcode::Sload32 => {
                    assert_eq!(inputs.len(), 1, "only one input for load operands");
                    lower_to_amode(ctx, inputs[0], offset)
                }

                Opcode::LoadComplex
                | Opcode::Uload8Complex
                | Opcode::Sload8Complex
                | Opcode::Uload16Complex
                | Opcode::Sload16Complex
                | Opcode::Uload32Complex
                | Opcode::Sload32Complex => {
                    assert_eq!(
                        inputs.len(),
                        2,
                        "can't handle more than two inputs in complex load"
                    );
                    let base = put_input_in_reg(ctx, inputs[0]);
                    let index = put_input_in_reg(ctx, inputs[1]);
                    let shift = 0;
                    Amode::imm_reg_reg_shift(offset as u32, base, index, shift)
                }

                _ => unreachable!(),
            };

            let dst = get_output_reg(ctx, outputs[0]);
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
                        _ if elem_ty.is_vector() && elem_ty.bits() == 128 => {
                            Inst::xmm_mov(SseOpcode::Movups, RegMem::mem(amode), dst)
                        } // TODO Specialize for different types: MOVUPD, MOVDQU
                        _ => unreachable!("unexpected type for load: {:?}", elem_ty),
                    });
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
                    Amode::imm_reg_reg_shift(offset as u32, base, index, shift)
                }

                _ => unreachable!(),
            };

            let src = put_input_in_reg(ctx, inputs[0]);

            ctx.emit(match elem_ty {
                types::F32 => Inst::xmm_mov_r_m(SseOpcode::Movss, src, addr),
                types::F64 => Inst::xmm_mov_r_m(SseOpcode::Movsd, src, addr),
                _ if elem_ty.is_vector() && elem_ty.bits() == 128 => {
                    // TODO Specialize for different types: MOVUPD, MOVDQU, etc.
                    Inst::xmm_mov_r_m(SseOpcode::Movups, src, addr)
                }
                _ => Inst::mov_r_m(elem_ty.bytes() as u8, src, addr),
            });
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
            let dst = get_output_reg(ctx, outputs[0]);
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
            let dst = get_output_reg(ctx, outputs[0]);
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
            let data = get_output_reg(ctx, outputs[0]);
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

            ctx.emit(Inst::mov_r_m(ty_access.bytes() as u8, data, addr));
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
            let dst = get_output_reg(ctx, outputs[0]);
            let (extname, _) = ctx.call_target(insn).unwrap();
            let extname = extname.clone();
            ctx.emit(Inst::LoadExtName {
                dst,
                name: Box::new(extname),
                offset: 0,
            });
        }

        Opcode::SymbolValue => {
            let dst = get_output_reg(ctx, outputs[0]);
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
            let dst = get_output_reg(ctx, outputs[0]);
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
                let rhs = put_input_in_reg(ctx, rhs_input);
                let dst = get_output_reg(ctx, outputs[0]);
                let lhs = if is_int_or_ref_ty(ty) && ty.bytes() < 4 {
                    // Special case: since the higher bits are undefined per CLIF semantics, we
                    // can just apply a 32-bit cmove here. Force inputs into registers, to
                    // avoid partial spilling out-of-bounds with memory accesses, though.
                    // Sign-extend operands to 32, then do a cmove of size 4.
                    RegMem::reg(put_input_in_reg(ctx, lhs_input))
                } else {
                    input_to_reg_mem(ctx, lhs_input)
                };

                // We request inversion of Equal to NotEqual here: taking LHS if equal would mean
                // take it if both CC::NP and CC::Z are set, the conjunction of which can't be
                // modeled with a single cmov instruction. Instead, we'll swap LHS and RHS in the
                // select operation, and invert the equal to a not-equal here.
                let fcmp_results = emit_fcmp(ctx, fcmp, cond_code, FcmpSpec::InvertEqual);

                if let FcmpCondResult::InvertedEqualOrConditions(_, _) = &fcmp_results {
                    // Keep this sync'd with the lowering of the select inputs above.
                    assert_eq!(cond_code, FloatCC::Equal);
                }

                ctx.emit(Inst::gen_move(dst, rhs, ty));

                match fcmp_results {
                    FcmpCondResult::Condition(cc) => {
                        if is_int_or_ref_ty(ty) {
                            let size = u8::max(ty.bytes() as u8, 4);
                            ctx.emit(Inst::cmove(size, cc, lhs, dst));
                        } else {
                            ctx.emit(Inst::xmm_cmove(ty == types::F64, cc, lhs, dst));
                        }
                    }
                    FcmpCondResult::AndConditions(_, _) => {
                        unreachable!(
                            "can't AND with select; see above comment about inverting equal"
                        );
                    }
                    FcmpCondResult::InvertedEqualOrConditions(cc1, cc2)
                    | FcmpCondResult::OrConditions(cc1, cc2) => {
                        if is_int_or_ref_ty(ty) {
                            let size = u8::max(ty.bytes() as u8, 4);
                            ctx.emit(Inst::cmove(size, cc1, lhs.clone(), dst));
                            ctx.emit(Inst::cmove(size, cc2, lhs, dst));
                        } else {
                            ctx.emit(Inst::xmm_cmove(ty == types::F64, cc1, lhs.clone(), dst));
                            ctx.emit(Inst::xmm_cmove(ty == types::F64, cc2, lhs, dst));
                        }
                    }
                }
            } else {
                let ty = ty.unwrap();

                let mut size = ty.bytes() as u8;
                let lhs = if is_int_or_ref_ty(ty) {
                    if size < 4 {
                        // Special case: since the higher bits are undefined per CLIF semantics, we
                        // can just apply a 32-bit cmove here. Force inputs into registers, to
                        // avoid partial spilling out-of-bounds with memory accesses, though.
                        size = 4;
                        RegMem::reg(put_input_in_reg(ctx, inputs[1]))
                    } else {
                        input_to_reg_mem(ctx, inputs[1])
                    }
                } else {
                    input_to_reg_mem(ctx, inputs[1])
                };

                let rhs = put_input_in_reg(ctx, inputs[2]);
                let dst = get_output_reg(ctx, outputs[0]);

                let cc = if let Some(icmp) = matches_input(ctx, flag_input, Opcode::Icmp) {
                    emit_cmp(ctx, icmp);
                    let cond_code = ctx.data(icmp).cond_code().unwrap();
                    CC::from_intcc(cond_code)
                } else {
                    // The input is a boolean value, compare it against zero.
                    let size = ctx.input_ty(insn, 0).bytes() as u8;
                    let test = put_input_in_reg(ctx, flag_input);
                    ctx.emit(Inst::cmp_rmi_r(size, RegMemImm::imm(0), test));
                    CC::NZ
                };

                // This doesn't affect the flags.
                ctx.emit(Inst::gen_move(dst, rhs, ty));

                if is_int_or_ref_ty(ty) {
                    ctx.emit(Inst::cmove(size, cc, lhs, dst));
                } else {
                    debug_assert!(ty == types::F32 || ty == types::F64);
                    ctx.emit(Inst::xmm_cmove(ty == types::F64, cc, lhs, dst));
                }
            }
        }

        Opcode::Selectif | Opcode::SelectifSpectreGuard => {
            let lhs = input_to_reg_mem(ctx, inputs[1]);
            let rhs = put_input_in_reg(ctx, inputs[2]);
            let dst = get_output_reg(ctx, outputs[0]);
            let ty = ctx.output_ty(insn, 0);

            // Verification ensures that the input is always a single-def ifcmp.
            let cmp_insn = ctx
                .get_input(inputs[0].insn, inputs[0].input)
                .inst
                .unwrap()
                .0;
            debug_assert_eq!(ctx.data(cmp_insn).opcode(), Opcode::Ifcmp);
            emit_cmp(ctx, cmp_insn);

            let cc = CC::from_intcc(ctx.data(insn).cond_code().unwrap());

            if is_int_or_ref_ty(ty) {
                let size = ty.bytes() as u8;
                if size == 1 {
                    // Sign-extend operands to 32, then do a cmove of size 4.
                    let lhs_se = ctx.alloc_tmp(RegClass::I64, types::I32);
                    ctx.emit(Inst::movsx_rm_r(ExtMode::BL, lhs, lhs_se));
                    ctx.emit(Inst::movsx_rm_r(ExtMode::BL, RegMem::reg(rhs), dst));
                    ctx.emit(Inst::cmove(4, cc, RegMem::reg(lhs_se.to_reg()), dst));
                } else {
                    ctx.emit(Inst::gen_move(dst, rhs, ty));
                    ctx.emit(Inst::cmove(size, cc, lhs, dst));
                }
            } else {
                debug_assert!(ty == types::F32 || ty == types::F64);
                ctx.emit(Inst::gen_move(dst, rhs, ty));
                ctx.emit(Inst::xmm_cmove(ty == types::F64, cc, lhs, dst));
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
            let size = input_ty.bytes() as u8;

            let dividend = put_input_in_reg(ctx, inputs[0]);
            let dst = get_output_reg(ctx, outputs[0]);

            ctx.emit(Inst::gen_move(
                Writable::from_reg(regs::rax()),
                dividend,
                input_ty,
            ));

            if flags.avoid_div_traps() {
                // A vcode meta-instruction is used to lower the inline checks, since they embed
                // pc-relative offsets that must not change, thus requiring regalloc to not
                // interfere by introducing spills and reloads.
                //
                // Note it keeps the result in $rax (for divide) or $rdx (for rem), so that
                // regalloc is aware of the coalescing opportunity between rax/rdx and the
                // destination register.
                let divisor = put_input_in_reg(ctx, inputs[1]);

                let divisor_copy = ctx.alloc_tmp(RegClass::I64, types::I64);
                ctx.emit(Inst::gen_move(divisor_copy, divisor, types::I64));

                let tmp = if op == Opcode::Sdiv && size == 8 {
                    Some(ctx.alloc_tmp(RegClass::I64, types::I64))
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
                let divisor = input_to_reg_mem(ctx, inputs[1]);

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
                // The remainder is in rdx.
                ctx.emit(Inst::gen_move(dst, regs::rdx(), input_ty));
            }
        }

        Opcode::Umulhi | Opcode::Smulhi => {
            let input_ty = ctx.input_ty(insn, 0);
            let size = input_ty.bytes() as u8;

            let lhs = put_input_in_reg(ctx, inputs[0]);
            let rhs = input_to_reg_mem(ctx, inputs[1]);
            let dst = get_output_reg(ctx, outputs[0]);

            // Move lhs in %rax.
            ctx.emit(Inst::gen_move(
                Writable::from_reg(regs::rax()),
                lhs,
                input_ty,
            ));

            // Emit the actual mul or imul.
            let signed = op == Opcode::Smulhi;
            ctx.emit(Inst::mul_hi(size, signed, rhs));

            // Read the result from the high part (stored in %rdx).
            ctx.emit(Inst::gen_move(dst, regs::rdx(), input_ty));
        }

        Opcode::GetPinnedReg => {
            let dst = get_output_reg(ctx, outputs[0]);
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
            let dst = get_output_reg(ctx, outputs[0]);
            let ty = ty.unwrap();
            ctx.emit(Inst::load(ty, used_constant, dst, ExtKind::None));
        }

        Opcode::RawBitcast => {
            // A raw_bitcast is just a mechanism for correcting the type of V128 values (see
            // https://github.com/bytecodealliance/wasmtime/issues/1147). As such, this IR
            // instruction should emit no machine code but a move is necessary to give the register
            // allocator a definition for the output virtual register.
            let src = put_input_in_reg(ctx, inputs[0]);
            let dst = get_output_reg(ctx, outputs[0]);
            let ty = ty.unwrap();
            ctx.emit(Inst::gen_move(dst, src, ty));
        }

        Opcode::Shuffle => {
            let ty = ty.unwrap();
            let dst = get_output_reg(ctx, outputs[0]);
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
                let tmp = ctx.alloc_tmp(RegClass::V128, types::I8X16);
                ctx.emit(Inst::load(ty, constant, tmp, ExtKind::None));
                // After loading the constructed mask in a temporary register, we use this to
                // shuffle the `dst` register (remember that, in this case, it is the same as
                // `src` so we disregard this register).
                ctx.emit(Inst::xmm_rm_r(SseOpcode::Pshufb, RegMem::from(tmp), dst));
            } else {
                // If `lhs` and `rhs` are different, we must shuffle each separately and then OR
                // them together. This is necessary due to PSHUFB semantics. As in the case above,
                // we build the `constructed_mask` for each case statically.

                // PSHUFB the `lhs` argument into `tmp0`, placing zeroes for unused lanes.
                let tmp0 = ctx.alloc_tmp(RegClass::V128, lhs_ty);
                ctx.emit(Inst::gen_move(tmp0, lhs, lhs_ty));
                let constructed_mask = mask.iter().cloned().map(zero_unknown_lane_index).collect();
                let constant = ctx.use_constant(VCodeConstantData::Generated(constructed_mask));
                let tmp1 = ctx.alloc_tmp(RegClass::V128, types::I8X16);
                ctx.emit(Inst::load(ty, constant, tmp1, ExtKind::None));
                ctx.emit(Inst::xmm_rm_r(SseOpcode::Pshufb, RegMem::from(tmp1), tmp0));

                // PSHUFB the second argument, placing zeroes for unused lanes.
                let constructed_mask = mask
                    .iter()
                    .map(|b| b.wrapping_sub(16))
                    .map(zero_unknown_lane_index)
                    .collect();
                let constant = ctx.use_constant(VCodeConstantData::Generated(constructed_mask));
                let tmp2 = ctx.alloc_tmp(RegClass::V128, types::I8X16);
                ctx.emit(Inst::load(ty, constant, tmp2, ExtKind::None));
                ctx.emit(Inst::xmm_rm_r(SseOpcode::Pshufb, RegMem::from(tmp2), dst));

                // OR the shuffled registers (the mechanism and lane-size for OR-ing the registers
                // is not important).
                ctx.emit(Inst::xmm_rm_r(SseOpcode::Orps, RegMem::from(tmp0), dst));

                // TODO when AVX512 is enabled we should replace this sequence with a single VPERMB
            }
        }

        Opcode::Swizzle => {
            // SIMD swizzle; the following inefficient implementation is due to the Wasm SIMD spec
            // requiring mask indexes greater than 15 to have the same semantics as a 0 index. For
            // the spec discussion, see https://github.com/WebAssembly/simd/issues/93. The CLIF
            // semantics match the Wasm SIMD semantics for this instruction.
            // The instruction format maps to variables like: %dst = swizzle %src, %mask
            let ty = ty.unwrap();
            let dst = get_output_reg(ctx, outputs[0]);
            let src = put_input_in_reg(ctx, inputs[0]);
            let swizzle_mask = put_input_in_reg(ctx, inputs[1]);

            // Inform the register allocator that `src` and `dst` should be in the same register.
            ctx.emit(Inst::gen_move(dst, src, ty));

            // Create a mask for zeroing out-of-bounds lanes of the swizzle mask.
            let zero_mask = ctx.alloc_tmp(RegClass::V128, types::I8X16);
            static ZERO_MASK_VALUE: [u8; 16] = [
                0x70, 0x70, 0x70, 0x70, 0x70, 0x70, 0x70, 0x70, 0x70, 0x70, 0x70, 0x70, 0x70, 0x70,
                0x70, 0x70,
            ];
            let constant = ctx.use_constant(VCodeConstantData::WellKnown(&ZERO_MASK_VALUE));
            ctx.emit(Inst::load(ty, constant, zero_mask, ExtKind::None));

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
            let dst = get_output_reg(ctx, outputs[0]);
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
            let dst = get_output_reg(ctx, outputs[0]);
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

        Opcode::Splat | Opcode::LoadSplat => {
            let ty = ty.unwrap();
            assert_eq!(ty.bits(), 128);
            let src_ty = ctx.input_ty(insn, 0);
            assert!(src_ty.bits() < 128);

            let src = match op {
                Opcode::Splat => input_to_reg_mem(ctx, inputs[0]),
                Opcode::LoadSplat => {
                    let offset = ctx.data(insn).load_store_offset().unwrap();
                    let amode = lower_to_amode(ctx, inputs[0], offset);
                    RegMem::mem(amode)
                }
                _ => unreachable!(),
            };
            let dst = get_output_reg(ctx, outputs[0]);

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
                    let tmp = ctx.alloc_tmp(RegClass::V128, ty);
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
                        false,
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
                        false,
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
            let dst = get_output_reg(ctx, outputs[0]);
            let src_ty = ctx.input_ty(insn, 0);
            assert_eq!(src_ty.bits(), 128);
            let src = put_input_in_reg(ctx, inputs[0]);
            // Set the ZF if the result is all zeroes.
            ctx.emit(Inst::xmm_cmp_rm_r(SseOpcode::Ptest, RegMem::reg(src), src));
            // If the ZF is not set, place a 1 in `dst`.
            ctx.emit(Inst::setcc(CC::NZ, dst));
        }

        Opcode::VallTrue => {
            let ty = ty.unwrap();
            let dst = get_output_reg(ctx, outputs[0]);
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
            let tmp = ctx.alloc_tmp(RegClass::V128, ty);
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
            let dst = get_output_reg(ctx, outputs[0]);
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
                    let tmp = ctx.alloc_tmp(RegClass::V128, src_ty);
                    ctx.emit(Inst::gen_move(tmp, src, src_ty));
                    ctx.emit(Inst::xmm_rm_r(SseOpcode::Packsswb, RegMem::reg(src), tmp));
                    ctx.emit(Inst::xmm_to_gpr(
                        SseOpcode::Pmovmskb,
                        tmp.to_reg(),
                        dst,
                        size,
                    ));
                    ctx.emit(Inst::shift_r(8, ShiftKind::ShiftRightLogical, Some(8), dst));
                }
                _ => unimplemented!("unknown input type {} for {}", src_ty, op),
            }
        }

        Opcode::ConstAddr => {
            let dst = get_output_reg(ctx, outputs[0]);
            debug_assert!(dst.to_reg().get_class() == RegClass::I64);

            if let InstructionData::UnaryConst {
                constant_handle, ..
            } = *ctx.data(insn)
            {
                let constant_data = ctx.get_constant_data(constant_handle);
                let constant = ctx.use_constant(VCodeConstantData::Pool(
                    constant_handle,
                    constant_data.clone(),
                ));
                ctx.emit(Inst::lea(SyntheticAmode::ConstantOffset(constant), dst));
            };
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
        | Opcode::SshrImm => {
            panic!("ALU+imm and ALU+carry ops should not appear here!");
        }
        _ => unimplemented!("unimplemented lowering for opcode {:?}", op),
    }

    Ok(())
}

//=============================================================================
// Lowering-backend trait implementation.

impl LowerBackend for X64Backend {
    type MInst = Inst;

    fn lower<C: LowerCtx<I = Inst>>(&self, ctx: &mut C, ir_inst: IRInst) -> CodegenResult<()> {
        lower_insn_to_regs(ctx, ir_inst, &self.flags, &self.triple)
    }

    fn lower_branch_group<C: LowerCtx<I = Inst>>(
        &self,
        ctx: &mut C,
        branches: &[IRInst],
        targets: &[MachLabel],
        fallthrough: Option<MachLabel>,
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
            let not_taken = match op1 {
                Opcode::Jump => targets[1],
                Opcode::Fallthrough => fallthrough.unwrap(),
                _ => unreachable!(), // assert above.
            };

            match op0 {
                Opcode::Brz | Opcode::Brnz => {
                    let flag_input = InsnInput {
                        insn: branches[0],
                        input: 0,
                    };

                    let src_ty = ctx.input_ty(branches[0], 0);

                    if let Some(icmp) = matches_input(ctx, flag_input, Opcode::Icmp) {
                        emit_cmp(ctx, icmp);

                        let cond_code = ctx.data(icmp).cond_code().unwrap();
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
                        let size_bytes = src_ty.bytes() as u8;
                        ctx.emit(Inst::cmp_rmi_r(size_bytes, RegMemImm::imm(0), src));
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
                        let byte_size = src_ty.bytes() as u8;
                        // Cranelift's icmp semantics want to compare lhs - rhs, while Intel gives
                        // us dst - src at the machine instruction level, so invert operands.
                        ctx.emit(Inst::cmp_rmi_r(byte_size, rhs, lhs));
                        ctx.emit(Inst::jmp_cond(cc, taken, not_taken));
                    } else {
                        unimplemented!("bricmp with non-int type {:?}", src_ty);
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
                    ctx.emit(Inst::cmp_rmi_r(4, RegMemImm::imm(jt_size), idx));

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
                    let tmp1 = ctx.alloc_tmp(RegClass::I64, types::I64);
                    // This temporary is used as a signed integer of 32-bits (for the wasm-table
                    // index) and then 64-bits (address addend). The small lie about the I64 type
                    // is benign, since the temporary is dead after this instruction (and its
                    // Cranelift type is thus unused).
                    let tmp2 = ctx.alloc_tmp(RegClass::I64, types::I64);

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
