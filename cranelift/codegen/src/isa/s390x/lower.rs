//! Lowering rules for S390x.

use crate::ir::condcodes::{FloatCC, IntCC};
use crate::ir::Inst as IRInst;
use crate::ir::{types, Endianness, InstructionData, MemFlags, Opcode, TrapCode, Type};
use crate::isa::s390x::abi::*;
use crate::isa::s390x::inst::*;
use crate::isa::s390x::settings as s390x_settings;
use crate::isa::s390x::S390xBackend;
use crate::machinst::lower::*;
use crate::machinst::*;
use crate::settings::Flags;
use crate::CodegenResult;
use alloc::boxed::Box;
use alloc::vec::Vec;
use core::convert::TryFrom;
use regalloc::{Reg, Writable};
use smallvec::SmallVec;

//=============================================================================
// Helpers for instruction lowering.

fn ty_is_int(ty: Type) -> bool {
    match ty {
        types::B1 | types::B8 | types::B16 | types::B32 | types::B64 => true,
        types::I8 | types::I16 | types::I32 | types::I64 | types::R64 => true,
        types::F32 | types::F64 => false,
        types::IFLAGS | types::FFLAGS => panic!("Unexpected flags type"),
        _ => panic!("ty_is_int() on unknown type: {:?}", ty),
    }
}

fn ty_is_float(ty: Type) -> bool {
    !ty_is_int(ty)
}

fn is_valid_atomic_transaction_ty(ty: Type) -> bool {
    match ty {
        types::I8 | types::I16 | types::I32 | types::I64 => true,
        _ => false,
    }
}

fn choose_32_64<T: Copy>(ty: Type, op32: T, op64: T) -> T {
    let bits = ty_bits(ty);
    if bits <= 32 {
        op32
    } else if bits == 64 {
        op64
    } else {
        panic!("choose_32_64 on > 64 bits!")
    }
}

//============================================================================
// Lowering: convert instruction inputs to forms that we can use.

/// Lower an instruction input to a 64-bit constant, if possible.
fn input_matches_const<C: LowerCtx<I = Inst>>(ctx: &mut C, input: InsnInput) -> Option<u64> {
    let input = ctx.get_input_as_source_or_const(input.insn, input.input);
    input.constant
}

/// Return false if instruction input cannot have the value Imm, true otherwise.
fn input_maybe_imm<C: LowerCtx<I = Inst>>(ctx: &mut C, input: InsnInput, imm: u64) -> bool {
    if let Some(c) = input_matches_const(ctx, input) {
        let ty = ctx.input_ty(input.insn, input.input);
        let from_bits = ty_bits(ty) as u8;
        let mask = if from_bits < 64 {
            (1u64 << ty_bits(ty)) - 1
        } else {
            0xffff_ffff_ffff_ffff
        };
        c & mask == imm & mask
    } else {
        true
    }
}

/// Lower an instruction input to a 16-bit signed constant, if possible.
fn input_matches_simm16<C: LowerCtx<I = Inst>>(ctx: &mut C, input: InsnInput) -> Option<i16> {
    if let Some(imm_value) = input_matches_const(ctx, input) {
        if let Ok(imm) = i16::try_from(imm_value as i64) {
            return Some(imm);
        }
    }
    None
}

/// Lower an instruction input to a 32-bit signed constant, if possible.
fn input_matches_simm32<C: LowerCtx<I = Inst>>(ctx: &mut C, input: InsnInput) -> Option<i32> {
    if let Some(imm_value) = input_matches_const(ctx, input) {
        if let Ok(imm) = i32::try_from(imm_value as i64) {
            return Some(imm);
        }
    }
    None
}

/// Lower an instruction input to a 32-bit unsigned constant, if possible.
fn input_matches_uimm32<C: LowerCtx<I = Inst>>(ctx: &mut C, input: InsnInput) -> Option<u32> {
    if let Some(imm_value) = input_matches_const(ctx, input) {
        if let Ok(imm) = u32::try_from(imm_value) {
            return Some(imm);
        }
    }
    None
}

/// Lower a negated instruction input to a 16-bit signed constant, if possible.
fn negated_input_matches_simm16<C: LowerCtx<I = Inst>>(
    ctx: &mut C,
    input: InsnInput,
) -> Option<i16> {
    if let Some(imm_value) = input_matches_const(ctx, input) {
        if let Ok(imm) = i16::try_from(-(imm_value as i64)) {
            return Some(imm);
        }
    }
    None
}

/// Lower a negated instruction input to a 32-bit signed constant, if possible.
fn negated_input_matches_simm32<C: LowerCtx<I = Inst>>(
    ctx: &mut C,
    input: InsnInput,
) -> Option<i32> {
    if let Some(imm_value) = input_matches_const(ctx, input) {
        if let Ok(imm) = i32::try_from(-(imm_value as i64)) {
            return Some(imm);
        }
    }
    None
}

/// Lower an instruction input to a 16-bit shifted constant, if possible.
fn input_matches_uimm16shifted<C: LowerCtx<I = Inst>>(
    ctx: &mut C,
    input: InsnInput,
) -> Option<UImm16Shifted> {
    if let Some(imm_value) = input_matches_const(ctx, input) {
        return UImm16Shifted::maybe_from_u64(imm_value);
    }
    None
}

/// Lower an instruction input to a 32-bit shifted constant, if possible.
fn input_matches_uimm32shifted<C: LowerCtx<I = Inst>>(
    ctx: &mut C,
    input: InsnInput,
) -> Option<UImm32Shifted> {
    if let Some(imm_value) = input_matches_const(ctx, input) {
        return UImm32Shifted::maybe_from_u64(imm_value);
    }
    None
}

/// Lower an instruction input to a 16-bit inverted shifted constant, if possible.
fn input_matches_uimm16shifted_inv<C: LowerCtx<I = Inst>>(
    ctx: &mut C,
    input: InsnInput,
) -> Option<UImm16Shifted> {
    if let Some(imm_value) = input_matches_const(ctx, input) {
        if let Some(imm) = UImm16Shifted::maybe_from_u64(!imm_value) {
            return Some(imm.negate_bits());
        }
    }
    None
}

/// Lower an instruction input to a 32-bit inverted shifted constant, if possible.
fn input_matches_uimm32shifted_inv<C: LowerCtx<I = Inst>>(
    ctx: &mut C,
    input: InsnInput,
) -> Option<UImm32Shifted> {
    if let Some(imm_value) = input_matches_const(ctx, input) {
        if let Some(imm) = UImm32Shifted::maybe_from_u64(!imm_value) {
            return Some(imm.negate_bits());
        }
    }
    None
}

/// Checks for an instance of `op` feeding the given input.
fn input_matches_insn<C: LowerCtx<I = Inst>>(
    c: &mut C,
    input: InsnInput,
    op: Opcode,
) -> Option<IRInst> {
    let inputs = c.get_input_as_source_or_const(input.insn, input.input);
    if let Some((src_inst, _)) = inputs.inst {
        let data = c.data(src_inst);
        if data.opcode() == op {
            return Some(src_inst);
        }
    }
    None
}

/// Checks for an instance of `op` feeding the given input, possibly via a conversion `conv` (e.g.,
/// Bint or a bitcast).
fn input_matches_insn_via_conv<C: LowerCtx<I = Inst>>(
    c: &mut C,
    input: InsnInput,
    op: Opcode,
    conv: Opcode,
) -> Option<IRInst> {
    let inputs = c.get_input_as_source_or_const(input.insn, input.input);
    if let Some((src_inst, _)) = inputs.inst {
        let data = c.data(src_inst);
        if data.opcode() == op {
            return Some(src_inst);
        }
        if data.opcode() == conv {
            let inputs = c.get_input_as_source_or_const(src_inst, 0);
            if let Some((src_inst, _)) = inputs.inst {
                let data = c.data(src_inst);
                if data.opcode() == op {
                    return Some(src_inst);
                }
            }
        }
    }
    None
}

fn input_matches_load_insn<C: LowerCtx<I = Inst>>(
    ctx: &mut C,
    input: InsnInput,
    op: Opcode,
) -> Option<MemArg> {
    if let Some(insn) = input_matches_insn(ctx, input, op) {
        let inputs: SmallVec<[InsnInput; 4]> = (0..ctx.num_inputs(insn))
            .map(|i| InsnInput { insn, input: i })
            .collect();
        let off = ctx.data(insn).load_store_offset().unwrap();
        let flags = ctx.memflags(insn).unwrap();
        let endianness = flags.endianness(Endianness::Big);
        if endianness == Endianness::Big {
            let mem = lower_address(ctx, &inputs[..], off, flags);
            ctx.sink_inst(insn);
            return Some(mem);
        }
    }
    None
}

fn input_matches_mem<C: LowerCtx<I = Inst>>(ctx: &mut C, input: InsnInput) -> Option<MemArg> {
    if ty_bits(ctx.input_ty(input.insn, input.input)) >= 32 {
        return input_matches_load_insn(ctx, input, Opcode::Load);
    }
    None
}

fn input_matches_sext16_mem<C: LowerCtx<I = Inst>>(
    ctx: &mut C,
    input: InsnInput,
) -> Option<MemArg> {
    if ty_bits(ctx.input_ty(input.insn, input.input)) == 16 {
        return input_matches_load_insn(ctx, input, Opcode::Load);
    }
    if ty_bits(ctx.input_ty(input.insn, input.input)) >= 32 {
        return input_matches_load_insn(ctx, input, Opcode::Sload16);
    }
    None
}

fn input_matches_sext32_mem<C: LowerCtx<I = Inst>>(
    ctx: &mut C,
    input: InsnInput,
) -> Option<MemArg> {
    if ty_bits(ctx.input_ty(input.insn, input.input)) > 32 {
        return input_matches_load_insn(ctx, input, Opcode::Sload32);
    }
    None
}

fn input_matches_sext32_reg<C: LowerCtx<I = Inst>>(ctx: &mut C, input: InsnInput) -> Option<Reg> {
    if let Some(insn) = input_matches_insn(ctx, input, Opcode::Sextend) {
        if ty_bits(ctx.input_ty(insn, 0)) == 32 {
            let reg = put_input_in_reg(ctx, InsnInput { insn, input: 0 }, NarrowValueMode::None);
            return Some(reg);
        }
    }
    None
}

fn input_matches_uext32_reg<C: LowerCtx<I = Inst>>(ctx: &mut C, input: InsnInput) -> Option<Reg> {
    if let Some(insn) = input_matches_insn(ctx, input, Opcode::Uextend) {
        if ty_bits(ctx.input_ty(insn, 0)) == 32 {
            let reg = put_input_in_reg(ctx, InsnInput { insn, input: 0 }, NarrowValueMode::None);
            return Some(reg);
        }
    }
    None
}

fn input_matches_uext16_mem<C: LowerCtx<I = Inst>>(
    ctx: &mut C,
    input: InsnInput,
) -> Option<MemArg> {
    if ty_bits(ctx.input_ty(input.insn, input.input)) == 16 {
        return input_matches_load_insn(ctx, input, Opcode::Load);
    }
    if ty_bits(ctx.input_ty(input.insn, input.input)) >= 32 {
        return input_matches_load_insn(ctx, input, Opcode::Uload16);
    }
    None
}

fn input_matches_uext32_mem<C: LowerCtx<I = Inst>>(
    ctx: &mut C,
    input: InsnInput,
) -> Option<MemArg> {
    if ty_bits(ctx.input_ty(input.insn, input.input)) > 32 {
        return input_matches_load_insn(ctx, input, Opcode::Uload32);
    }
    None
}

//============================================================================
// Lowering: force instruction input into a register

/// How to handle narrow values loaded into registers; see note on `narrow_mode`
/// parameter to `put_input_in_*` below.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NarrowValueMode {
    None,
    /// Zero-extend to 32 bits if original is < 32 bits.
    ZeroExtend32,
    /// Sign-extend to 32 bits if original is < 32 bits.
    SignExtend32,
    /// Zero-extend to 64 bits if original is < 64 bits.
    ZeroExtend64,
    /// Sign-extend to 64 bits if original is < 64 bits.
    SignExtend64,
}

fn extend_memory_to_reg<C: LowerCtx<I = Inst>>(
    ctx: &mut C,
    mem: MemArg,
    from_ty: Type,
    to_ty: Type,
    signed: bool,
) -> Reg {
    let rd = ctx.alloc_tmp(to_ty).only_reg().unwrap();
    ctx.emit(match (signed, ty_bits(to_ty), ty_bits(from_ty)) {
        (false, 32, 8) => Inst::Load32ZExt8 { rd, mem },
        (false, 32, 16) => Inst::Load32ZExt16 { rd, mem },
        (true, 32, 8) => Inst::Load32SExt8 { rd, mem },
        (true, 32, 16) => Inst::Load32SExt16 { rd, mem },
        (false, 64, 8) => Inst::Load64ZExt8 { rd, mem },
        (false, 64, 16) => Inst::Load64ZExt16 { rd, mem },
        (false, 64, 32) => Inst::Load64ZExt32 { rd, mem },
        (true, 64, 8) => Inst::Load64SExt8 { rd, mem },
        (true, 64, 16) => Inst::Load64SExt16 { rd, mem },
        (true, 64, 32) => Inst::Load64SExt32 { rd, mem },
        _ => panic!("Unsupported size in load"),
    });
    rd.to_reg()
}

/// Sign-extend the low `from_bits` bits of `value` to a full u64.
fn sign_extend_to_u64(value: u64, from_bits: u8) -> u64 {
    assert!(from_bits <= 64);
    if from_bits >= 64 {
        value
    } else {
        (((value << (64 - from_bits)) as i64) >> (64 - from_bits)) as u64
    }
}

/// Zero-extend the low `from_bits` bits of `value` to a full u64.
fn zero_extend_to_u64(value: u64, from_bits: u8) -> u64 {
    assert!(from_bits <= 64);
    if from_bits >= 64 {
        value
    } else {
        value & ((1u64 << from_bits) - 1)
    }
}

/// Lower an instruction input to a reg.
///
/// The given register will be extended appropriately, according to
/// `narrow_mode` and the input's type.
fn put_input_in_reg<C: LowerCtx<I = Inst>>(
    ctx: &mut C,
    input: InsnInput,
    narrow_mode: NarrowValueMode,
) -> Reg {
    let signed = match narrow_mode {
        NarrowValueMode::SignExtend32 | NarrowValueMode::SignExtend64 => true,
        NarrowValueMode::ZeroExtend32 | NarrowValueMode::ZeroExtend64 => false,
        _ => false,
    };
    let ty = ctx.input_ty(input.insn, input.input);
    let from_bits = ty_bits(ty) as u8;
    let ext_ty = match narrow_mode {
        NarrowValueMode::None => ty,
        NarrowValueMode::ZeroExtend32 | NarrowValueMode::SignExtend32 => types::I32,
        NarrowValueMode::ZeroExtend64 | NarrowValueMode::SignExtend64 => types::I64,
    };
    let to_bits = ty_bits(ext_ty) as u8;
    assert!(to_bits >= from_bits);

    if let Some(c) = input_matches_const(ctx, input) {
        let extended = if from_bits == to_bits {
            c
        } else if signed {
            sign_extend_to_u64(c, from_bits)
        } else {
            zero_extend_to_u64(c, from_bits)
        };
        let masked = zero_extend_to_u64(extended, to_bits);

        // Generate constants fresh at each use to minimize long-range register pressure.
        let to_reg = ctx.alloc_tmp(ext_ty).only_reg().unwrap();
        for inst in Inst::gen_constant(ValueRegs::one(to_reg), masked as u128, ext_ty, |ty| {
            ctx.alloc_tmp(ty).only_reg().unwrap()
        })
        .into_iter()
        {
            ctx.emit(inst);
        }
        to_reg.to_reg()
    } else if to_bits == from_bits {
        ctx.put_input_in_regs(input.insn, input.input)
            .only_reg()
            .unwrap()
    } else if let Some(mem) = input_matches_load_insn(ctx, input, Opcode::Load) {
        extend_memory_to_reg(ctx, mem, ty, ext_ty, signed)
    } else {
        let rd = ctx.alloc_tmp(ext_ty).only_reg().unwrap();
        let rn = ctx
            .put_input_in_regs(input.insn, input.input)
            .only_reg()
            .unwrap();
        ctx.emit(Inst::Extend {
            rd,
            rn,
            signed,
            from_bits,
            to_bits,
        });
        rd.to_reg()
    }
}

//============================================================================
// Lowering: addressing mode support. Takes instruction directly, rather
// than an `InsnInput`, to do more introspection.

/// Lower the address of a load or store.
fn lower_address<C: LowerCtx<I = Inst>>(
    ctx: &mut C,
    addends: &[InsnInput],
    offset: i32,
    flags: MemFlags,
) -> MemArg {
    // Handle one reg and offset.
    if addends.len() == 1 {
        if offset == 0 {
            if let Some(add) = input_matches_insn(ctx, addends[0], Opcode::Iadd) {
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

                let ra = put_input_in_reg(ctx, add_inputs[0], NarrowValueMode::None);
                let rb = put_input_in_reg(ctx, add_inputs[1], NarrowValueMode::None);
                return MemArg::reg_plus_reg(ra, rb, flags);
            }
        }

        if let Some(symbol) = input_matches_insn(ctx, addends[0], Opcode::SymbolValue) {
            let (extname, dist, ext_offset) = ctx.symbol_value(symbol).unwrap();
            let ext_offset = ext_offset + i64::from(offset);
            if dist == RelocDistance::Near && (ext_offset & 1) == 0 {
                if let Ok(offset) = i32::try_from(ext_offset) {
                    return MemArg::Symbol {
                        name: Box::new(extname.clone()),
                        offset,
                        flags,
                    };
                }
            }
        }

        let reg = put_input_in_reg(ctx, addends[0], NarrowValueMode::None);
        return MemArg::reg_plus_off(reg, offset as i64, flags);
    }

    // Handle two regs and a zero offset.
    if addends.len() == 2 && offset == 0 {
        let ra = put_input_in_reg(ctx, addends[0], NarrowValueMode::None);
        let rb = put_input_in_reg(ctx, addends[1], NarrowValueMode::None);
        return MemArg::reg_plus_reg(ra, rb, flags);
    }

    // Otherwise, generate add instructions.
    let addr = ctx.alloc_tmp(types::I64).only_reg().unwrap();

    // Get the const into a reg.
    lower_constant_u64(ctx, addr.clone(), offset as u64);

    // Add each addend to the address.
    for addend in addends {
        let reg = put_input_in_reg(ctx, *addend, NarrowValueMode::None);

        ctx.emit(Inst::AluRRR {
            alu_op: ALUOp::Add64,
            rd: addr.clone(),
            rn: addr.to_reg(),
            rm: reg.clone(),
        });
    }

    MemArg::reg(addr.to_reg(), flags)
}

//============================================================================
// Lowering: generating constants.

fn lower_constant_u64<C: LowerCtx<I = Inst>>(ctx: &mut C, rd: Writable<Reg>, value: u64) {
    for inst in Inst::load_constant64(rd, value) {
        ctx.emit(inst);
    }
}

fn lower_constant_u32<C: LowerCtx<I = Inst>>(ctx: &mut C, rd: Writable<Reg>, value: u32) {
    for inst in Inst::load_constant32(rd, value) {
        ctx.emit(inst);
    }
}

fn lower_constant_f32<C: LowerCtx<I = Inst>>(ctx: &mut C, rd: Writable<Reg>, value: f32) {
    ctx.emit(Inst::load_fp_constant32(rd, value));
}

fn lower_constant_f64<C: LowerCtx<I = Inst>>(ctx: &mut C, rd: Writable<Reg>, value: f64) {
    ctx.emit(Inst::load_fp_constant64(rd, value));
}

//============================================================================
// Lowering: miscellaneous helpers.

/// Emit code to invert the value of type ty in register rd.
fn lower_bnot<C: LowerCtx<I = Inst>>(ctx: &mut C, ty: Type, rd: Writable<Reg>) {
    let alu_op = choose_32_64(ty, ALUOp::Xor32, ALUOp::Xor64);
    ctx.emit(Inst::AluRUImm32Shifted {
        alu_op,
        rd,
        imm: UImm32Shifted::maybe_from_u64(0xffff_ffff).unwrap(),
    });
    if ty_bits(ty) > 32 {
        ctx.emit(Inst::AluRUImm32Shifted {
            alu_op,
            rd,
            imm: UImm32Shifted::maybe_from_u64(0xffff_ffff_0000_0000).unwrap(),
        });
    }
}

/// Emit code to bitcast between integer and floating-point values.
fn lower_bitcast<C: LowerCtx<I = Inst>>(
    ctx: &mut C,
    rd: Writable<Reg>,
    output_ty: Type,
    rn: Reg,
    input_ty: Type,
) {
    match (input_ty, output_ty) {
        (types::I64, types::F64) => {
            ctx.emit(Inst::MovToFpr { rd, rn });
        }
        (types::F64, types::I64) => {
            ctx.emit(Inst::MovFromFpr { rd, rn });
        }
        (types::I32, types::F32) => {
            let tmp = ctx.alloc_tmp(types::I64).only_reg().unwrap();
            ctx.emit(Inst::ShiftRR {
                shift_op: ShiftOp::LShL64,
                rd: tmp,
                rn,
                shift_imm: SImm20::maybe_from_i64(32).unwrap(),
                shift_reg: None,
            });
            ctx.emit(Inst::MovToFpr {
                rd,
                rn: tmp.to_reg(),
            });
        }
        (types::F32, types::I32) => {
            let tmp = ctx.alloc_tmp(types::I64).only_reg().unwrap();
            ctx.emit(Inst::MovFromFpr { rd: tmp, rn });
            ctx.emit(Inst::ShiftRR {
                shift_op: ShiftOp::LShR64,
                rd,
                rn: tmp.to_reg(),
                shift_imm: SImm20::maybe_from_i64(32).unwrap(),
                shift_reg: None,
            });
        }
        _ => unreachable!("invalid bitcast from {:?} to {:?}", input_ty, output_ty),
    }
}

//=============================================================================
// Lowering: comparisons

/// Determines whether this condcode interprets inputs as signed or
/// unsigned.  See the documentation for the `icmp` instruction in
/// cranelift-codegen/meta/src/shared/instructions.rs for further insights
/// into this.
pub fn condcode_is_signed(cc: IntCC) -> bool {
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

fn lower_icmp_to_flags<C: LowerCtx<I = Inst>>(
    ctx: &mut C,
    insn: IRInst,
    is_signed: bool,
    may_sink_memory: bool,
) {
    let ty = ctx.input_ty(insn, 0);
    let bits = ty_bits(ty);
    let narrow_mode = match (bits <= 32, is_signed) {
        (true, true) => NarrowValueMode::SignExtend32,
        (true, false) => NarrowValueMode::ZeroExtend32,
        (false, true) => NarrowValueMode::SignExtend64,
        (false, false) => NarrowValueMode::ZeroExtend64,
    };
    let inputs = [
        InsnInput {
            insn: insn,
            input: 0,
        },
        InsnInput {
            insn: insn,
            input: 1,
        },
    ];
    let ty = ctx.input_ty(insn, 0);
    let rn = put_input_in_reg(ctx, inputs[0], narrow_mode);
    if is_signed {
        let op = choose_32_64(ty, CmpOp::CmpS32, CmpOp::CmpS64);
        // Try matching immedate operand.
        if let Some(imm) = input_matches_simm16(ctx, inputs[1]) {
            return ctx.emit(Inst::CmpRSImm16 { op, rn, imm });
        }
        if let Some(imm) = input_matches_simm32(ctx, inputs[1]) {
            return ctx.emit(Inst::CmpRSImm32 { op, rn, imm });
        }
        // If sinking memory loads is allowed, try matching memory operand.
        if may_sink_memory {
            if let Some(mem) = input_matches_mem(ctx, inputs[1]) {
                return ctx.emit(Inst::CmpRX { op, rn, mem });
            }
            if let Some(mem) = input_matches_sext16_mem(ctx, inputs[1]) {
                let op = choose_32_64(ty, CmpOp::CmpS32Ext16, CmpOp::CmpS64Ext16);
                return ctx.emit(Inst::CmpRX { op, rn, mem });
            }
            if let Some(mem) = input_matches_sext32_mem(ctx, inputs[1]) {
                return ctx.emit(Inst::CmpRX {
                    op: CmpOp::CmpS64Ext32,
                    rn,
                    mem,
                });
            }
        }
        // Try matching sign-extension in register.
        if let Some(rm) = input_matches_sext32_reg(ctx, inputs[1]) {
            return ctx.emit(Inst::CmpRR {
                op: CmpOp::CmpS64Ext32,
                rn,
                rm,
            });
        }
        // If no special case matched above, fall back to a register compare.
        let rm = put_input_in_reg(ctx, inputs[1], narrow_mode);
        return ctx.emit(Inst::CmpRR { op, rn, rm });
    } else {
        let op = choose_32_64(ty, CmpOp::CmpL32, CmpOp::CmpL64);
        // Try matching immedate operand.
        if let Some(imm) = input_matches_uimm32(ctx, inputs[1]) {
            return ctx.emit(Inst::CmpRUImm32 { op, rn, imm });
        }
        // If sinking memory loads is allowed, try matching memory operand.
        if may_sink_memory {
            if let Some(mem) = input_matches_mem(ctx, inputs[1]) {
                return ctx.emit(Inst::CmpRX { op, rn, mem });
            }
            if let Some(mem) = input_matches_uext16_mem(ctx, inputs[1]) {
                match &mem {
                    &MemArg::Symbol { .. } => {
                        let op = choose_32_64(ty, CmpOp::CmpL32Ext16, CmpOp::CmpL64Ext16);
                        return ctx.emit(Inst::CmpRX { op, rn, mem });
                    }
                    _ => {
                        let reg_ty = choose_32_64(ty, types::I32, types::I64);
                        let rm = extend_memory_to_reg(ctx, mem, ty, reg_ty, false);
                        return ctx.emit(Inst::CmpRR { op, rn, rm });
                    }
                }
            }
            if let Some(mem) = input_matches_uext32_mem(ctx, inputs[1]) {
                return ctx.emit(Inst::CmpRX {
                    op: CmpOp::CmpL64Ext32,
                    rn,
                    mem,
                });
            }
        }
        // Try matching zero-extension in register.
        if let Some(rm) = input_matches_uext32_reg(ctx, inputs[1]) {
            return ctx.emit(Inst::CmpRR {
                op: CmpOp::CmpL64Ext32,
                rn,
                rm,
            });
        }
        // If no special case matched above, fall back to a register compare.
        let rm = put_input_in_reg(ctx, inputs[1], narrow_mode);
        return ctx.emit(Inst::CmpRR { op, rn, rm });
    }
}

fn lower_fcmp_to_flags<C: LowerCtx<I = Inst>>(ctx: &mut C, insn: IRInst) {
    let ty = ctx.input_ty(insn, 0);
    let bits = ty_bits(ty);
    let inputs = [
        InsnInput {
            insn: insn,
            input: 0,
        },
        InsnInput {
            insn: insn,
            input: 1,
        },
    ];
    let rn = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
    let rm = put_input_in_reg(ctx, inputs[1], NarrowValueMode::None);
    match bits {
        32 => {
            ctx.emit(Inst::FpuCmp32 { rn, rm });
        }
        64 => {
            ctx.emit(Inst::FpuCmp64 { rn, rm });
        }
        _ => panic!("Unknown float size"),
    }
}

fn lower_boolean_to_flags<C: LowerCtx<I = Inst>>(ctx: &mut C, input: InsnInput) -> Cond {
    if let Some(icmp_insn) = input_matches_insn_via_conv(ctx, input, Opcode::Icmp, Opcode::Bint) {
        // FIXME: If the Icmp (and Bint) only have a single use, we can still allow sinking memory
        let may_sink_memory = false;
        let condcode = ctx.data(icmp_insn).cond_code().unwrap();
        let is_signed = condcode_is_signed(condcode);
        lower_icmp_to_flags(ctx, icmp_insn, is_signed, may_sink_memory);
        Cond::from_intcc(condcode)
    } else if let Some(fcmp_insn) =
        input_matches_insn_via_conv(ctx, input, Opcode::Fcmp, Opcode::Bint)
    {
        let condcode = ctx.data(fcmp_insn).fp_cond_code().unwrap();
        lower_fcmp_to_flags(ctx, fcmp_insn);
        Cond::from_floatcc(condcode)
    } else {
        let ty = ctx.input_ty(input.insn, input.input);
        let narrow_mode = if ty.bits() < 32 {
            NarrowValueMode::ZeroExtend32
        } else {
            NarrowValueMode::None
        };
        let rn = put_input_in_reg(ctx, input, narrow_mode);
        let op = choose_32_64(ty, CmpOp::CmpS32, CmpOp::CmpS64);
        ctx.emit(Inst::CmpRSImm16 { op, rn, imm: 0 });
        Cond::from_intcc(IntCC::NotEqual)
    }
}

fn lower_flags_to_bool_result<C: LowerCtx<I = Inst>>(
    ctx: &mut C,
    cond: Cond,
    rd: Writable<Reg>,
    ty: Type,
) {
    if ty_bits(ty) == 1 {
        lower_constant_u32(ctx, rd, 0);
        ctx.emit(Inst::CMov32SImm16 { rd, cond, imm: 1 });
    } else if ty_bits(ty) < 64 {
        lower_constant_u32(ctx, rd, 0);
        ctx.emit(Inst::CMov32SImm16 { rd, cond, imm: -1 });
    } else {
        lower_constant_u64(ctx, rd, 0);
        ctx.emit(Inst::CMov64SImm16 { rd, cond, imm: -1 });
    }
}

//============================================================================
// Lowering: main entry point for lowering a instruction

fn lower_insn_to_regs<C: LowerCtx<I = Inst>>(
    ctx: &mut C,
    insn: IRInst,
    flags: &Flags,
    isa_flags: &s390x_settings::Flags,
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
        Opcode::Nop => {
            // Nothing.
        }

        Opcode::Copy | Opcode::Ireduce | Opcode::Breduce => {
            // Smaller ints / bools have the high bits undefined, so any reduce
            // operation is simply a copy.
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let rn = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
            let ty = ctx.input_ty(insn, 0);
            ctx.emit(Inst::gen_move(rd, rn, ty));
        }

        Opcode::Iconst | Opcode::Bconst | Opcode::Null => {
            let value = ctx.get_constant(insn).unwrap();
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let ty = ty.unwrap();
            if ty.bits() <= 32 {
                lower_constant_u32(ctx, rd, value as u32);
            } else {
                lower_constant_u64(ctx, rd, value);
            }
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
            let ty = ty.unwrap();
            let alu_op = choose_32_64(ty, ALUOp::Add32, ALUOp::Add64);
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let rn = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
            if let Some(imm) = input_matches_simm16(ctx, inputs[1]) {
                ctx.emit(Inst::AluRRSImm16 {
                    alu_op,
                    rd,
                    rn,
                    imm,
                });
            } else if let Some(imm) = input_matches_simm32(ctx, inputs[1]) {
                ctx.emit(Inst::gen_move(rd, rn, ty));
                ctx.emit(Inst::AluRSImm32 { alu_op, rd, imm });
            } else if let Some(mem) = input_matches_mem(ctx, inputs[1]) {
                ctx.emit(Inst::gen_move(rd, rn, ty));
                ctx.emit(Inst::AluRX { alu_op, rd, mem });
            } else if let Some(mem) = input_matches_sext16_mem(ctx, inputs[1]) {
                let alu_op = choose_32_64(ty, ALUOp::Add32Ext16, ALUOp::Add64Ext16);
                ctx.emit(Inst::gen_move(rd, rn, ty));
                ctx.emit(Inst::AluRX { alu_op, rd, mem });
            } else if let Some(mem) = input_matches_sext32_mem(ctx, inputs[1]) {
                ctx.emit(Inst::gen_move(rd, rn, ty));
                ctx.emit(Inst::AluRX {
                    alu_op: ALUOp::Add64Ext32,
                    rd,
                    mem,
                });
            } else if let Some(rm) = input_matches_sext32_reg(ctx, inputs[1]) {
                ctx.emit(Inst::gen_move(rd, rn, ty));
                ctx.emit(Inst::AluRR {
                    alu_op: ALUOp::Add64Ext32,
                    rd,
                    rm,
                });
            } else {
                let rm = put_input_in_reg(ctx, inputs[1], NarrowValueMode::None);
                ctx.emit(Inst::AluRRR { alu_op, rd, rn, rm });
            }
        }
        Opcode::Isub => {
            let ty = ty.unwrap();
            let alu_op = choose_32_64(ty, ALUOp::Sub32, ALUOp::Sub64);
            let neg_op = choose_32_64(ty, ALUOp::Add32, ALUOp::Add64);
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let rn = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
            if let Some(imm) = negated_input_matches_simm16(ctx, inputs[1]) {
                ctx.emit(Inst::AluRRSImm16 {
                    alu_op: neg_op,
                    rd,
                    rn,
                    imm,
                });
            } else if let Some(imm) = negated_input_matches_simm32(ctx, inputs[1]) {
                ctx.emit(Inst::gen_move(rd, rn, ty));
                ctx.emit(Inst::AluRSImm32 {
                    alu_op: neg_op,
                    rd,
                    imm,
                });
            } else if let Some(mem) = input_matches_mem(ctx, inputs[1]) {
                ctx.emit(Inst::gen_move(rd, rn, ty));
                ctx.emit(Inst::AluRX { alu_op, rd, mem });
            } else if let Some(mem) = input_matches_sext16_mem(ctx, inputs[1]) {
                let alu_op = choose_32_64(ty, ALUOp::Sub32Ext16, ALUOp::Sub64Ext16);
                ctx.emit(Inst::gen_move(rd, rn, ty));
                ctx.emit(Inst::AluRX { alu_op, rd, mem });
            } else if let Some(mem) = input_matches_sext32_mem(ctx, inputs[1]) {
                ctx.emit(Inst::gen_move(rd, rn, ty));
                ctx.emit(Inst::AluRX {
                    alu_op: ALUOp::Sub64Ext32,
                    rd,
                    mem,
                });
            } else if let Some(rm) = input_matches_sext32_reg(ctx, inputs[1]) {
                ctx.emit(Inst::gen_move(rd, rn, ty));
                ctx.emit(Inst::AluRR {
                    alu_op: ALUOp::Sub64Ext32,
                    rd,
                    rm,
                });
            } else {
                let rm = put_input_in_reg(ctx, inputs[1], NarrowValueMode::None);
                ctx.emit(Inst::AluRRR { alu_op, rd, rn, rm });
            }
        }
        Opcode::IaddIfcout => {
            // This only supports the operands emitted by dynamic_addr.
            let ty = ty.unwrap();
            assert!(ty == types::I32 || ty == types::I64);
            let alu_op = choose_32_64(ty, ALUOp::Add32, ALUOp::Add64);
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let rn = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
            let imm = input_matches_uimm32(ctx, inputs[1]).unwrap();
            ctx.emit(Inst::gen_move(rd, rn, ty));
            // Note that this will emit AL(G)FI, which sets the condition
            // code to indicate an (unsigned) carry bit.
            ctx.emit(Inst::AluRUImm32 { alu_op, rd, imm });
        }

        Opcode::UaddSat | Opcode::SaddSat => unimplemented!(),
        Opcode::UsubSat | Opcode::SsubSat => unimplemented!(),

        Opcode::Iabs => {
            let ty = ty.unwrap();
            let op = choose_32_64(ty, UnaryOp::Abs32, UnaryOp::Abs64);
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            if let Some(rn) = input_matches_sext32_reg(ctx, inputs[0]) {
                ctx.emit(Inst::UnaryRR {
                    op: UnaryOp::Abs64Ext32,
                    rd,
                    rn,
                });
            } else {
                let narrow_mode = if ty.bits() < 32 {
                    NarrowValueMode::SignExtend32
                } else {
                    NarrowValueMode::None
                };
                let rn = put_input_in_reg(ctx, inputs[0], narrow_mode);
                ctx.emit(Inst::UnaryRR { op, rd, rn });
            }
        }
        Opcode::Ineg => {
            let ty = ty.unwrap();
            let op = choose_32_64(ty, UnaryOp::Neg32, UnaryOp::Neg64);
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            if let Some(rn) = input_matches_sext32_reg(ctx, inputs[0]) {
                ctx.emit(Inst::UnaryRR {
                    op: UnaryOp::Neg64Ext32,
                    rd,
                    rn,
                });
            } else {
                let rn = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
                ctx.emit(Inst::UnaryRR { op, rd, rn });
            }
        }

        Opcode::Imul => {
            let ty = ty.unwrap();
            let alu_op = choose_32_64(ty, ALUOp::Mul32, ALUOp::Mul64);
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let rn = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
            if let Some(imm) = input_matches_simm16(ctx, inputs[1]) {
                ctx.emit(Inst::gen_move(rd, rn, ty));
                ctx.emit(Inst::AluRSImm16 { alu_op, rd, imm });
            } else if let Some(imm) = input_matches_simm32(ctx, inputs[1]) {
                ctx.emit(Inst::gen_move(rd, rn, ty));
                ctx.emit(Inst::AluRSImm32 { alu_op, rd, imm });
            } else if let Some(mem) = input_matches_mem(ctx, inputs[1]) {
                ctx.emit(Inst::gen_move(rd, rn, ty));
                ctx.emit(Inst::AluRX { alu_op, rd, mem });
            } else if let Some(mem) = input_matches_sext16_mem(ctx, inputs[1]) {
                let alu_op = choose_32_64(ty, ALUOp::Mul32Ext16, ALUOp::Mul64Ext16);
                ctx.emit(Inst::gen_move(rd, rn, ty));
                ctx.emit(Inst::AluRX { alu_op, rd, mem });
            } else if let Some(mem) = input_matches_sext32_mem(ctx, inputs[1]) {
                ctx.emit(Inst::gen_move(rd, rn, ty));
                ctx.emit(Inst::AluRX {
                    alu_op: ALUOp::Mul64Ext32,
                    rd,
                    mem,
                });
            } else if let Some(rm) = input_matches_sext32_reg(ctx, inputs[1]) {
                ctx.emit(Inst::gen_move(rd, rn, ty));
                ctx.emit(Inst::AluRR {
                    alu_op: ALUOp::Mul64Ext32,
                    rd,
                    rm,
                });
            } else {
                let rm = put_input_in_reg(ctx, inputs[1], NarrowValueMode::None);
                ctx.emit(Inst::AluRRR { alu_op, rd, rn, rm });
            }
        }

        Opcode::Umulhi | Opcode::Smulhi => {
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let is_signed = op == Opcode::Smulhi;
            let input_ty = ctx.input_ty(insn, 0);
            assert!(ctx.input_ty(insn, 1) == input_ty);
            assert!(ctx.output_ty(insn, 0) == input_ty);

            match input_ty {
                types::I64 => {
                    let rn = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
                    let rm = put_input_in_reg(ctx, inputs[1], NarrowValueMode::None);

                    if is_signed {
                        ctx.emit(Inst::SMulWide { rn, rm });
                        ctx.emit(Inst::gen_move(rd, gpr(0), input_ty));
                    } else {
                        ctx.emit(Inst::gen_move(writable_gpr(1), rm, input_ty));
                        ctx.emit(Inst::UMulWide { rn });
                        ctx.emit(Inst::gen_move(rd, gpr(0), input_ty));
                    }
                }
                types::I32 => {
                    let narrow_mode = if is_signed {
                        NarrowValueMode::SignExtend64
                    } else {
                        NarrowValueMode::ZeroExtend64
                    };
                    let rn = put_input_in_reg(ctx, inputs[0], narrow_mode);
                    let rm = put_input_in_reg(ctx, inputs[1], narrow_mode);
                    ctx.emit(Inst::AluRRR {
                        alu_op: ALUOp::Mul64,
                        rd,
                        rn,
                        rm,
                    });
                    let shift_op = if is_signed {
                        ShiftOp::AShR64
                    } else {
                        ShiftOp::LShR64
                    };
                    ctx.emit(Inst::ShiftRR {
                        shift_op,
                        rd,
                        rn: rd.to_reg(),
                        shift_imm: SImm20::maybe_from_i64(32).unwrap(),
                        shift_reg: None,
                    });
                }
                types::I16 | types::I8 => {
                    let narrow_mode = if is_signed {
                        NarrowValueMode::SignExtend32
                    } else {
                        NarrowValueMode::ZeroExtend32
                    };
                    let rn = put_input_in_reg(ctx, inputs[0], narrow_mode);
                    let rm = put_input_in_reg(ctx, inputs[1], narrow_mode);
                    ctx.emit(Inst::AluRRR {
                        alu_op: ALUOp::Mul32,
                        rd,
                        rn,
                        rm,
                    });
                    let shift_op = if is_signed {
                        ShiftOp::AShR32
                    } else {
                        ShiftOp::LShR32
                    };
                    let shift_amt = match input_ty {
                        types::I16 => 16,
                        types::I8 => 8,
                        _ => unreachable!(),
                    };
                    ctx.emit(Inst::ShiftRR {
                        shift_op,
                        rd,
                        rn: rd.to_reg(),
                        shift_imm: SImm20::maybe_from_i64(shift_amt).unwrap(),
                        shift_reg: None,
                    });
                }
                _ => {
                    panic!("Unsupported argument type for umulhi/smulhi: {}", input_ty);
                }
            }
        }

        Opcode::Udiv | Opcode::Urem => {
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let ty = ty.unwrap();

            let rn = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
            if ty_bits(ty) <= 32 {
                lower_constant_u32(ctx, writable_gpr(0), 0);
                if ty_bits(ty) < 32 {
                    ctx.emit(Inst::Extend {
                        rd: writable_gpr(1),
                        rn,
                        signed: false,
                        from_bits: ty_bits(ty) as u8,
                        to_bits: 32,
                    });
                } else {
                    ctx.emit(Inst::mov32(writable_gpr(1), rn));
                }
            } else {
                lower_constant_u64(ctx, writable_gpr(0), 0);
                ctx.emit(Inst::mov64(writable_gpr(1), rn));
            }

            let narrow_mode = if ty.bits() < 32 {
                NarrowValueMode::ZeroExtend32
            } else {
                NarrowValueMode::None
            };
            let rm = put_input_in_reg(ctx, inputs[1], narrow_mode);

            if input_maybe_imm(ctx, inputs[1], 0) && flags.avoid_div_traps() {
                ctx.emit(Inst::CmpTrapRSImm16 {
                    op: choose_32_64(ty, CmpOp::CmpS32, CmpOp::CmpS64),
                    rn: rm,
                    imm: 0,
                    cond: Cond::from_intcc(IntCC::Equal),
                    trap_code: TrapCode::IntegerDivisionByZero,
                });
            }

            if ty_bits(ty) <= 32 {
                ctx.emit(Inst::UDivMod32 { rn: rm });
            } else {
                ctx.emit(Inst::UDivMod64 { rn: rm });
            }

            if op == Opcode::Udiv {
                ctx.emit(Inst::gen_move(rd, gpr(1), ty));
            } else {
                ctx.emit(Inst::gen_move(rd, gpr(0), ty));
            }
        }

        Opcode::Sdiv | Opcode::Srem => {
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let ty = ty.unwrap();

            let rn = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
            if ty_bits(ty) < 64 {
                ctx.emit(Inst::Extend {
                    rd: writable_gpr(1),
                    rn,
                    signed: true,
                    from_bits: ty_bits(ty) as u8,
                    to_bits: 64,
                });
            } else {
                ctx.emit(Inst::mov64(writable_gpr(1), rn));
            }

            let narrow_mode = if ty.bits() < 32 {
                NarrowValueMode::SignExtend32
            } else {
                NarrowValueMode::None
            };
            let rm = put_input_in_reg(ctx, inputs[1], narrow_mode);

            if input_maybe_imm(ctx, inputs[1], 0) && flags.avoid_div_traps() {
                ctx.emit(Inst::CmpTrapRSImm16 {
                    op: choose_32_64(ty, CmpOp::CmpS32, CmpOp::CmpS64),
                    rn: rm,
                    imm: 0,
                    cond: Cond::from_intcc(IntCC::Equal),
                    trap_code: TrapCode::IntegerDivisionByZero,
                });
            }

            if input_maybe_imm(ctx, inputs[1], 0xffff_ffff_ffff_ffff) {
                if op == Opcode::Sdiv {
                    let tmp = ctx.alloc_tmp(ty).only_reg().unwrap();
                    if ty_bits(ty) <= 32 {
                        lower_constant_u32(ctx, tmp, (1 << (ty_bits(ty) - 1)) - 1);
                    } else {
                        lower_constant_u64(ctx, tmp, (1 << (ty_bits(ty) - 1)) - 1);
                    }
                    ctx.emit(Inst::AluRRR {
                        alu_op: choose_32_64(ty, ALUOp::Xor32, ALUOp::Xor64),
                        rd: tmp,
                        rn: tmp.to_reg(),
                        rm: gpr(1),
                    });
                    ctx.emit(Inst::AluRRR {
                        alu_op: choose_32_64(ty, ALUOp::And32, ALUOp::And64),
                        rd: tmp,
                        rn: tmp.to_reg(),
                        rm,
                    });
                    ctx.emit(Inst::CmpTrapRSImm16 {
                        op: choose_32_64(ty, CmpOp::CmpS32, CmpOp::CmpS64),
                        rn: tmp.to_reg(),
                        imm: -1,
                        cond: Cond::from_intcc(IntCC::Equal),
                        trap_code: TrapCode::IntegerOverflow,
                    });
                } else {
                    if ty_bits(ty) > 32 {
                        ctx.emit(Inst::CmpRSImm16 {
                            op: CmpOp::CmpS64,
                            rn: rm,
                            imm: -1,
                        });
                        ctx.emit(Inst::CMov64SImm16 {
                            rd: writable_gpr(1),
                            cond: Cond::from_intcc(IntCC::Equal),
                            imm: 0,
                        });
                    }
                }
            }

            if ty_bits(ty) <= 32 {
                ctx.emit(Inst::SDivMod32 { rn: rm });
            } else {
                ctx.emit(Inst::SDivMod64 { rn: rm });
            }

            if op == Opcode::Sdiv {
                ctx.emit(Inst::gen_move(rd, gpr(1), ty));
            } else {
                ctx.emit(Inst::gen_move(rd, gpr(0), ty));
            }
        }

        Opcode::Uextend | Opcode::Sextend => {
            let ty = ty.unwrap();
            let to_bits = ty_bits(ty) as u8;
            let to_bits = std::cmp::max(32, to_bits);
            let narrow_mode = match (op, to_bits) {
                (Opcode::Uextend, 32) => NarrowValueMode::ZeroExtend32,
                (Opcode::Uextend, 64) => NarrowValueMode::ZeroExtend64,
                (Opcode::Sextend, 32) => NarrowValueMode::SignExtend32,
                (Opcode::Sextend, 64) => NarrowValueMode::SignExtend64,
                _ => unreachable!(),
            };
            let rn = put_input_in_reg(ctx, inputs[0], narrow_mode);
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            ctx.emit(Inst::gen_move(rd, rn, ty));
        }

        Opcode::Ishl | Opcode::Ushr | Opcode::Sshr => {
            let ty = ty.unwrap();
            let size = ty_bits(ty);
            let narrow_mode = match (op, size) {
                (Opcode::Ishl, _) => NarrowValueMode::None,
                (Opcode::Ushr, 64) => NarrowValueMode::ZeroExtend64,
                (Opcode::Ushr, _) => NarrowValueMode::ZeroExtend32,
                (Opcode::Sshr, 64) => NarrowValueMode::SignExtend64,
                (Opcode::Sshr, _) => NarrowValueMode::SignExtend32,
                _ => unreachable!(),
            };
            let shift_op = match op {
                Opcode::Ishl => choose_32_64(ty, ShiftOp::LShL32, ShiftOp::LShL64),
                Opcode::Ushr => choose_32_64(ty, ShiftOp::LShR32, ShiftOp::LShR64),
                Opcode::Sshr => choose_32_64(ty, ShiftOp::AShR32, ShiftOp::AShR64),
                _ => unreachable!(),
            };
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let rn = put_input_in_reg(ctx, inputs[0], narrow_mode);
            if let Some(imm) = input_matches_const(ctx, inputs[1]) {
                let imm = imm & if size < 64 { 31 } else { 63 };
                let shift_imm = SImm20::maybe_from_i64(imm as i64).unwrap();
                let shift_reg = None;
                ctx.emit(Inst::ShiftRR {
                    shift_op,
                    rd,
                    rn,
                    shift_imm,
                    shift_reg,
                });
            } else {
                let rm = put_input_in_reg(ctx, inputs[1], NarrowValueMode::None);
                let shift_imm = SImm20::zero();
                let shift_reg = if size < 64 {
                    let tmp = ctx.alloc_tmp(types::I64).only_reg().unwrap();
                    ctx.emit(Inst::gen_move(tmp, rm, types::I64));
                    ctx.emit(Inst::AluRUImm16Shifted {
                        alu_op: ALUOp::And64,
                        rd: tmp,
                        imm: UImm16Shifted::maybe_from_u64(31).unwrap(),
                    });
                    Some(tmp.to_reg())
                } else {
                    Some(rm)
                };
                ctx.emit(Inst::ShiftRR {
                    shift_op,
                    rd,
                    rn,
                    shift_imm,
                    shift_reg,
                });
            }
        }

        Opcode::Rotr | Opcode::Rotl => {
            // s390x doesn't have a right-rotate instruction, but a right rotation of K places is
            // effectively a left rotation of N - K places, if N is the integer's bit size. We
            // implement right rotations with this trick.
            //
            // For a 32-bit or 64-bit rotate-left, we can use the ROR instruction directly.
            //
            // For a < 32-bit rotate-left, we synthesize this as:
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

            let is_rotr = op == Opcode::Rotr;

            let ty = ty.unwrap();
            let ty_bits_size = ty_bits(ty) as u64;

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

            if ty_bits_size == 32 || ty_bits_size == 64 {
                let shift_op = choose_32_64(ty, ShiftOp::RotL32, ShiftOp::RotL64);
                if let Some(imm) = input_matches_const(ctx, inputs[1]) {
                    let shiftcount = imm & (ty_bits_size - 1);
                    let shiftcount = if is_rotr {
                        ty_bits_size - shiftcount
                    } else {
                        shiftcount
                    };
                    ctx.emit(Inst::ShiftRR {
                        shift_op,
                        rd,
                        rn,
                        shift_imm: SImm20::maybe_from_i64(shiftcount as i64).unwrap(),
                        shift_reg: None,
                    });
                } else {
                    let rm = put_input_in_reg(ctx, inputs[1], NarrowValueMode::None);
                    let rm = if is_rotr {
                        // Really ty_bits_size - rn, but the upper bits of the result are
                        // ignored (because of the implicit masking done by the instruction),
                        // so this is equivalent to negating the input.
                        let op = choose_32_64(ty, UnaryOp::Neg32, UnaryOp::Neg64);
                        let tmp = ctx.alloc_tmp(ty).only_reg().unwrap();
                        ctx.emit(Inst::UnaryRR {
                            op,
                            rd: tmp,
                            rn: rm,
                        });
                        tmp.to_reg()
                    } else {
                        rm
                    };
                    ctx.emit(Inst::ShiftRR {
                        shift_op,
                        rd,
                        rn,
                        shift_imm: SImm20::zero(),
                        shift_reg: Some(rm),
                    });
                }
            } else {
                debug_assert!(ty_bits_size < 32);

                if let Some(imm) = input_matches_const(ctx, inputs[1]) {
                    let rot_count = imm & (ty_bits_size - 1);
                    let (lshl_count, lshr_count) = if is_rotr {
                        (ty_bits_size - rot_count, rot_count)
                    } else {
                        (rot_count, ty_bits_size - rot_count)
                    };

                    let tmp1 = ctx.alloc_tmp(types::I32).only_reg().unwrap();
                    ctx.emit(Inst::ShiftRR {
                        shift_op: ShiftOp::LShL32,
                        rd: tmp1,
                        rn,
                        shift_imm: SImm20::maybe_from_i64(lshl_count as i64).unwrap(),
                        shift_reg: None,
                    });

                    let tmp2 = ctx.alloc_tmp(types::I32).only_reg().unwrap();
                    ctx.emit(Inst::ShiftRR {
                        shift_op: ShiftOp::LShR32,
                        rd: tmp2,
                        rn,
                        shift_imm: SImm20::maybe_from_i64(lshr_count as i64).unwrap(),
                        shift_reg: None,
                    });

                    ctx.emit(Inst::AluRRR {
                        alu_op: ALUOp::Orr32,
                        rd,
                        rn: tmp1.to_reg(),
                        rm: tmp2.to_reg(),
                    });
                } else {
                    let rm = put_input_in_reg(ctx, inputs[1], NarrowValueMode::None);
                    let tmp1 = ctx.alloc_tmp(types::I32).only_reg().unwrap();
                    let tmp2 = ctx.alloc_tmp(types::I32).only_reg().unwrap();

                    ctx.emit(Inst::mov32(tmp1, rm));
                    ctx.emit(Inst::UnaryRR {
                        op: UnaryOp::Neg32,
                        rd: tmp2,
                        rn: rm,
                    });

                    ctx.emit(Inst::AluRUImm16Shifted {
                        alu_op: ALUOp::And32,
                        rd: tmp1,
                        imm: UImm16Shifted::maybe_from_u64(ty_bits_size - 1).unwrap(),
                    });
                    ctx.emit(Inst::AluRUImm16Shifted {
                        alu_op: ALUOp::And32,
                        rd: tmp2,
                        imm: UImm16Shifted::maybe_from_u64(ty_bits_size - 1).unwrap(),
                    });

                    let (lshr, lshl) = if is_rotr { (tmp2, tmp1) } else { (tmp1, tmp2) };

                    ctx.emit(Inst::ShiftRR {
                        shift_op: ShiftOp::LShL32,
                        rd: lshl,
                        rn,
                        shift_imm: SImm20::zero(),
                        shift_reg: Some(lshl.to_reg()),
                    });

                    ctx.emit(Inst::ShiftRR {
                        shift_op: ShiftOp::LShR32,
                        rd: lshr,
                        rn,
                        shift_imm: SImm20::zero(),
                        shift_reg: Some(lshr.to_reg()),
                    });

                    ctx.emit(Inst::AluRRR {
                        alu_op: ALUOp::Orr32,
                        rd,
                        rn: lshl.to_reg(),
                        rm: lshr.to_reg(),
                    });
                }
            }
        }

        Opcode::Bnot => {
            let ty = ty.unwrap();
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let rn = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
            if isa_flags.has_mie2() {
                ctx.emit(Inst::AluRRR {
                    alu_op: choose_32_64(ty, ALUOp::OrrNot32, ALUOp::OrrNot64),
                    rd,
                    rn,
                    rm: rn,
                });
            } else {
                ctx.emit(Inst::gen_move(rd, rn, ty));
                lower_bnot(ctx, ty, rd);
            }
        }

        Opcode::Band => {
            let ty = ty.unwrap();
            let alu_op = choose_32_64(ty, ALUOp::And32, ALUOp::And64);
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let rn = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
            if let Some(imm) = input_matches_uimm16shifted_inv(ctx, inputs[1]) {
                ctx.emit(Inst::gen_move(rd, rn, ty));
                ctx.emit(Inst::AluRUImm16Shifted { alu_op, rd, imm });
            } else if let Some(imm) = input_matches_uimm32shifted_inv(ctx, inputs[1]) {
                ctx.emit(Inst::gen_move(rd, rn, ty));
                ctx.emit(Inst::AluRUImm32Shifted { alu_op, rd, imm });
            } else if let Some(mem) = input_matches_mem(ctx, inputs[1]) {
                ctx.emit(Inst::gen_move(rd, rn, ty));
                ctx.emit(Inst::AluRX { alu_op, rd, mem });
            } else {
                let rm = put_input_in_reg(ctx, inputs[1], NarrowValueMode::None);
                ctx.emit(Inst::AluRRR { alu_op, rd, rn, rm });
            }
        }

        Opcode::Bor => {
            let ty = ty.unwrap();
            let alu_op = choose_32_64(ty, ALUOp::Orr32, ALUOp::Orr64);
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let rn = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
            if let Some(imm) = input_matches_uimm16shifted(ctx, inputs[1]) {
                ctx.emit(Inst::gen_move(rd, rn, ty));
                ctx.emit(Inst::AluRUImm16Shifted { alu_op, rd, imm });
            } else if let Some(imm) = input_matches_uimm32shifted(ctx, inputs[1]) {
                ctx.emit(Inst::gen_move(rd, rn, ty));
                ctx.emit(Inst::AluRUImm32Shifted { alu_op, rd, imm });
            } else if let Some(mem) = input_matches_mem(ctx, inputs[1]) {
                ctx.emit(Inst::gen_move(rd, rn, ty));
                ctx.emit(Inst::AluRX { alu_op, rd, mem });
            } else {
                let rm = put_input_in_reg(ctx, inputs[1], NarrowValueMode::None);
                ctx.emit(Inst::AluRRR { alu_op, rd, rn, rm });
            }
        }

        Opcode::Bxor => {
            let ty = ty.unwrap();
            let alu_op = choose_32_64(ty, ALUOp::Xor32, ALUOp::Xor64);
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let rn = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
            if let Some(imm) = input_matches_uimm32shifted(ctx, inputs[1]) {
                ctx.emit(Inst::gen_move(rd, rn, ty));
                ctx.emit(Inst::AluRUImm32Shifted { alu_op, rd, imm });
            } else if let Some(mem) = input_matches_mem(ctx, inputs[1]) {
                ctx.emit(Inst::gen_move(rd, rn, ty));
                ctx.emit(Inst::AluRX { alu_op, rd, mem });
            } else {
                let rm = put_input_in_reg(ctx, inputs[1], NarrowValueMode::None);
                ctx.emit(Inst::AluRRR { alu_op, rd, rn, rm });
            }
        }

        Opcode::BandNot | Opcode::BorNot | Opcode::BxorNot => {
            let ty = ty.unwrap();
            let alu_op = match (op, isa_flags.has_mie2()) {
                (Opcode::BandNot, true) => choose_32_64(ty, ALUOp::AndNot32, ALUOp::AndNot64),
                (Opcode::BorNot, true) => choose_32_64(ty, ALUOp::OrrNot32, ALUOp::OrrNot64),
                (Opcode::BxorNot, true) => choose_32_64(ty, ALUOp::XorNot32, ALUOp::XorNot64),
                (Opcode::BandNot, false) => choose_32_64(ty, ALUOp::And32, ALUOp::And64),
                (Opcode::BorNot, false) => choose_32_64(ty, ALUOp::Orr32, ALUOp::Orr64),
                (Opcode::BxorNot, false) => choose_32_64(ty, ALUOp::Xor32, ALUOp::Xor64),
                _ => unreachable!(),
            };
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let rn = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
            let rm = put_input_in_reg(ctx, inputs[1], NarrowValueMode::None);
            ctx.emit(Inst::AluRRR { alu_op, rd, rn, rm });
            if !isa_flags.has_mie2() {
                lower_bnot(ctx, ty, rd);
            }
        }

        Opcode::Bitselect => {
            let ty = ty.unwrap();
            let tmp = ctx.alloc_tmp(types::I64).only_reg().unwrap();
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let rcond = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
            let rn = put_input_in_reg(ctx, inputs[1], NarrowValueMode::None);
            let rm = put_input_in_reg(ctx, inputs[2], NarrowValueMode::None);
            ctx.emit(Inst::AluRRR {
                alu_op: choose_32_64(ty, ALUOp::And32, ALUOp::And64),
                rd: tmp,
                rn,
                rm: rcond,
            });
            if isa_flags.has_mie2() {
                ctx.emit(Inst::AluRRR {
                    alu_op: choose_32_64(ty, ALUOp::AndNot32, ALUOp::AndNot64),
                    rd,
                    rn: rm,
                    rm: rcond,
                });
            } else {
                ctx.emit(Inst::AluRRR {
                    alu_op: choose_32_64(ty, ALUOp::And32, ALUOp::And64),
                    rd,
                    rn: rm,
                    rm: rcond,
                });
                lower_bnot(ctx, ty, rd);
            }
            ctx.emit(Inst::AluRRR {
                alu_op: choose_32_64(ty, ALUOp::Orr32, ALUOp::Orr64),
                rd,
                rn: rd.to_reg(),
                rm: tmp.to_reg(),
            });
        }

        Opcode::Bextend | Opcode::Bmask => {
            // Bextend and Bmask both simply sign-extend. This works for:
            // - Bextend, because booleans are stored as 0 / -1, so we
            //   sign-extend the -1 to a -1 in the wider width.
            // - Bmask, because the resulting integer mask value must be
            //   all-ones (-1) if the argument is true.
            //
            // For a sign-extension from a 1-bit value (Case 1 below), we need
            // to do things a bit specially, because the ISA does not have a
            // 1-to-N-bit sign extension instruction.  For 8-bit or wider
            // sources (Case 2 below), we do a sign extension normally.

            let from_ty = ctx.input_ty(insn, 0);
            let to_ty = ctx.output_ty(insn, 0);
            let from_bits = ty_bits(from_ty);
            let to_bits = ty_bits(to_ty);

            assert!(
                from_bits <= 64 && to_bits <= 64,
                "Vector Bextend not supported yet"
            );

            if from_bits >= to_bits {
                // Just a move.
                let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
                let rn = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
                let ty = ctx.input_ty(insn, 0);
                ctx.emit(Inst::gen_move(rd, rn, ty));
            } else if from_bits == 1 {
                assert!(to_bits >= 8);
                // Case 1: 1-bit to N-bit extension: use a shift-left /
                // shift-right sequence to create a 0 / -1 result.
                let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
                let rn = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
                let shl_op = choose_32_64(to_ty, ShiftOp::LShL32, ShiftOp::LShL64);
                let shr_op = choose_32_64(to_ty, ShiftOp::AShR32, ShiftOp::AShR64);
                let count = if to_bits > 32 { 63 } else { 31 };
                ctx.emit(Inst::ShiftRR {
                    shift_op: shl_op,
                    rd,
                    rn,
                    shift_imm: SImm20::maybe_from_i64(count.into()).unwrap(),
                    shift_reg: None,
                });
                ctx.emit(Inst::ShiftRR {
                    shift_op: shr_op,
                    rd,
                    rn: rd.to_reg(),
                    shift_imm: SImm20::maybe_from_i64(count.into()).unwrap(),
                    shift_reg: None,
                });
            } else {
                // Case 2: 8-or-more-bit to N-bit extension: just sign-extend. A
                // `true` (all ones, or `-1`) will be extended to -1 with the
                // larger width.
                assert!(from_bits >= 8);
                let narrow_mode = if to_bits == 64 {
                    NarrowValueMode::SignExtend64
                } else {
                    assert!(to_bits <= 32);
                    NarrowValueMode::SignExtend32
                };
                let rn = put_input_in_reg(ctx, inputs[0], narrow_mode);
                let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
                ctx.emit(Inst::gen_move(rd, rn, to_ty));
            }
        }

        Opcode::Bint => {
            // Booleans are stored as all-zeroes (0) or all-ones (-1). We AND
            // out the LSB to give a 0 / 1-valued integer result.
            let ty = ty.unwrap();
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let rn = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
            if ty_bits(ty) <= 16 {
                ctx.emit(Inst::gen_move(rd, rn, ty));
                ctx.emit(Inst::AluRUImm16Shifted {
                    alu_op: ALUOp::And32,
                    rd,
                    imm: UImm16Shifted::maybe_from_u64(1).unwrap(),
                });
            } else if ty_bits(ty) <= 32 {
                ctx.emit(Inst::gen_move(rd, rn, ty));
                ctx.emit(Inst::AluRUImm32Shifted {
                    alu_op: ALUOp::And32,
                    rd,
                    imm: UImm32Shifted::maybe_from_u64(1).unwrap(),
                });
            } else {
                let tmp = ctx.alloc_tmp(types::I64).only_reg().unwrap();
                lower_constant_u64(ctx, tmp, 1);
                ctx.emit(Inst::AluRRR {
                    alu_op: ALUOp::And64,
                    rd,
                    rn,
                    rm: tmp.to_reg(),
                });
            }
        }

        Opcode::Clz => {
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let rn = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
            let ty = ty.unwrap();
            let ty_bits_size = ty_bits(ty);

            let rn = if ty_bits_size < 64 {
                let tmp = ctx.alloc_tmp(types::I64).only_reg().unwrap();
                ctx.emit(Inst::Extend {
                    rd: tmp,
                    rn,
                    signed: false,
                    from_bits: ty_bits_size as u8,
                    to_bits: 64,
                });
                tmp.to_reg()
            } else {
                rn
            };

            ctx.emit(Inst::Flogr { rn });
            ctx.emit(Inst::gen_move(rd, gpr(0), ty));

            if ty_bits_size < 64 {
                ctx.emit(Inst::AluRSImm16 {
                    alu_op: ALUOp::Add32,
                    rd,
                    imm: -(64 - ty_bits_size as i16),
                });
            }
        }

        Opcode::Cls => {
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let rn = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
            let ty = ty.unwrap();
            let ty_bits_size = ty_bits(ty);

            let rn = if ty_bits_size < 64 {
                let tmp = ctx.alloc_tmp(types::I64).only_reg().unwrap();
                ctx.emit(Inst::Extend {
                    rd: tmp,
                    rn,
                    signed: true,
                    from_bits: ty_bits_size as u8,
                    to_bits: 64,
                });
                tmp.to_reg()
            } else {
                rn
            };

            // tmp = rn ^ ((signed)rn >> 63)
            let tmp = ctx.alloc_tmp(types::I64).only_reg().unwrap();
            ctx.emit(Inst::ShiftRR {
                shift_op: ShiftOp::AShR64,
                rd: tmp,
                rn,
                shift_imm: SImm20::maybe_from_i64(63).unwrap(),
                shift_reg: None,
            });
            ctx.emit(Inst::AluRRR {
                alu_op: ALUOp::Xor64,
                rd: tmp,
                rn: tmp.to_reg(),
                rm: rn,
            });

            ctx.emit(Inst::Flogr { rn });
            ctx.emit(Inst::gen_move(rd, gpr(0), ty));

            if ty_bits_size < 64 {
                ctx.emit(Inst::AluRSImm16 {
                    alu_op: ALUOp::Add32,
                    rd,
                    imm: -(64 - ty_bits_size as i16),
                });
            }
        }

        Opcode::Ctz => {
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let rn = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
            let ty = ty.unwrap();
            let ty_bits_size = ty_bits(ty);

            let rn = if ty_bits_size < 64 {
                let tmp = ctx.alloc_tmp(types::I64).only_reg().unwrap();
                ctx.emit(Inst::gen_move(tmp, rn, ty));
                ctx.emit(Inst::AluRUImm16Shifted {
                    alu_op: ALUOp::Orr64,
                    rd: tmp,
                    imm: UImm16Shifted::maybe_from_u64(1u64 << ty_bits_size).unwrap(),
                });
                tmp.to_reg()
            } else {
                rn
            };

            // tmp = rn & -rn
            let tmp = ctx.alloc_tmp(types::I64).only_reg().unwrap();
            ctx.emit(Inst::UnaryRR {
                op: UnaryOp::Neg64,
                rd: tmp,
                rn,
            });
            ctx.emit(Inst::AluRRR {
                alu_op: ALUOp::And64,
                rd: tmp,
                rn: tmp.to_reg(),
                rm: rn,
            });

            ctx.emit(Inst::Flogr { rn: tmp.to_reg() });
            if ty_bits_size == 64 {
                ctx.emit(Inst::CMov64SImm16 {
                    rd: writable_gpr(0),
                    cond: Cond::from_intcc(IntCC::Equal),
                    imm: -1,
                });
            }

            if ty_bits_size <= 32 {
                lower_constant_u32(ctx, rd, 63);
            } else {
                lower_constant_u64(ctx, rd, 63);
            }
            let alu_op = choose_32_64(ty, ALUOp::Sub32, ALUOp::Sub64);
            ctx.emit(Inst::AluRRR {
                alu_op,
                rd,
                rn: rd.to_reg(),
                rm: gpr(0),
            });
        }

        Opcode::Bitrev => unimplemented!(),

        Opcode::Popcnt => {
            let ty = ty.unwrap();
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            if ty_bits(ty) <= 8 {
                let rn = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
                ctx.emit(Inst::UnaryRR {
                    op: UnaryOp::PopcntByte,
                    rd,
                    rn,
                });
            } else if isa_flags.has_mie2() {
                let rn = put_input_in_reg(ctx, inputs[0], NarrowValueMode::ZeroExtend64);
                ctx.emit(Inst::UnaryRR {
                    op: UnaryOp::PopcntReg,
                    rd,
                    rn,
                });
            } else {
                let rn = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
                ctx.emit(Inst::UnaryRR {
                    op: UnaryOp::PopcntByte,
                    rd,
                    rn,
                });
                let tmp = ctx.alloc_tmp(types::I64).only_reg().unwrap();
                let mut shift = ty_bits(ty) as u8;
                while shift > 8 {
                    shift = shift / 2;
                    ctx.emit(Inst::ShiftRR {
                        shift_op: choose_32_64(ty, ShiftOp::LShL32, ShiftOp::LShL64),
                        rd: tmp,
                        rn: rd.to_reg(),
                        shift_imm: SImm20::maybe_from_i64(shift.into()).unwrap(),
                        shift_reg: None,
                    });
                    ctx.emit(Inst::AluRR {
                        alu_op: choose_32_64(ty, ALUOp::Add32, ALUOp::Add64),
                        rd,
                        rm: tmp.to_reg(),
                    });
                }
                let shift = ty_bits(ty) as u8 - 8;
                ctx.emit(Inst::ShiftRR {
                    shift_op: choose_32_64(ty, ShiftOp::LShR32, ShiftOp::LShR64),
                    rd,
                    rn: rd.to_reg(),
                    shift_imm: SImm20::maybe_from_i64(shift.into()).unwrap(),
                    shift_reg: None,
                });
            }
        }

        Opcode::Fadd | Opcode::Fsub | Opcode::Fmul | Opcode::Fdiv => {
            let bits = ty_bits(ctx.output_ty(insn, 0));
            let fpu_op = match (op, bits) {
                (Opcode::Fadd, 32) => FPUOp2::Add32,
                (Opcode::Fadd, 64) => FPUOp2::Add64,
                (Opcode::Fsub, 32) => FPUOp2::Sub32,
                (Opcode::Fsub, 64) => FPUOp2::Sub64,
                (Opcode::Fmul, 32) => FPUOp2::Mul32,
                (Opcode::Fmul, 64) => FPUOp2::Mul64,
                (Opcode::Fdiv, 32) => FPUOp2::Div32,
                (Opcode::Fdiv, 64) => FPUOp2::Div64,
                _ => panic!("Unknown op/bits combination"),
            };
            let rn = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
            let rm = put_input_in_reg(ctx, inputs[1], NarrowValueMode::None);
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            ctx.emit(Inst::mov64(rd, rn));
            ctx.emit(Inst::FpuRRR { fpu_op, rd, rm });
        }

        Opcode::Fmin | Opcode::Fmax => {
            let bits = ty_bits(ctx.output_ty(insn, 0));
            let fpu_op = match (op, bits) {
                (Opcode::Fmin, 32) => FPUOp2::Min32,
                (Opcode::Fmin, 64) => FPUOp2::Min64,
                (Opcode::Fmax, 32) => FPUOp2::Max32,
                (Opcode::Fmax, 64) => FPUOp2::Max64,
                _ => panic!("Unknown op/bits combination"),
            };
            let rn = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
            let rm = put_input_in_reg(ctx, inputs[1], NarrowValueMode::None);
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            ctx.emit(Inst::FpuVecRRR { fpu_op, rd, rn, rm });
        }

        Opcode::Sqrt | Opcode::Fneg | Opcode::Fabs | Opcode::Fpromote | Opcode::Fdemote => {
            let bits = ty_bits(ctx.output_ty(insn, 0));
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
            let rn = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            ctx.emit(Inst::FpuRR { fpu_op, rd, rn });
        }

        Opcode::Ceil | Opcode::Floor | Opcode::Trunc | Opcode::Nearest => {
            let bits = ty_bits(ctx.output_ty(insn, 0));
            let op = match (op, bits) {
                (Opcode::Ceil, 32) => FpuRoundMode::Plus32,
                (Opcode::Ceil, 64) => FpuRoundMode::Plus64,
                (Opcode::Floor, 32) => FpuRoundMode::Minus32,
                (Opcode::Floor, 64) => FpuRoundMode::Minus64,
                (Opcode::Trunc, 32) => FpuRoundMode::Zero32,
                (Opcode::Trunc, 64) => FpuRoundMode::Zero64,
                (Opcode::Nearest, 32) => FpuRoundMode::Nearest32,
                (Opcode::Nearest, 64) => FpuRoundMode::Nearest64,
                _ => panic!("Unknown op/bits combination"),
            };
            let rn = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            ctx.emit(Inst::FpuRound { op, rd, rn });
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
            ctx.emit(Inst::mov64(rd, ra));
            ctx.emit(Inst::FpuRRRR { fpu_op, rd, rn, rm });
        }

        Opcode::Fcopysign => {
            let ty = ctx.output_ty(insn, 0);
            let bits = ty_bits(ty) as u8;
            assert!(bits == 32 || bits == 64);
            let rn = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
            let rm = put_input_in_reg(ctx, inputs[1], NarrowValueMode::None);
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();

            ctx.emit(Inst::FpuCopysign { rd, rn, rm });
        }

        Opcode::FcvtFromUint | Opcode::FcvtFromSint => {
            let in_bits = ty_bits(ctx.input_ty(insn, 0));
            let out_bits = ty_bits(ctx.output_ty(insn, 0));
            let signed = op == Opcode::FcvtFromSint;
            let op = match (signed, in_bits, out_bits) {
                (false, 32, 32) => IntToFpuOp::U32ToF32,
                (true, 32, 32) => IntToFpuOp::I32ToF32,
                (false, 32, 64) => IntToFpuOp::U32ToF64,
                (true, 32, 64) => IntToFpuOp::I32ToF64,
                (false, 64, 32) => IntToFpuOp::U64ToF32,
                (true, 64, 32) => IntToFpuOp::I64ToF32,
                (false, 64, 64) => IntToFpuOp::U64ToF64,
                (true, 64, 64) => IntToFpuOp::I64ToF64,
                _ => panic!("Unknown input/output-bits combination"),
            };
            let narrow_mode = match (signed, in_bits) {
                (false, 32) => NarrowValueMode::ZeroExtend32,
                (true, 32) => NarrowValueMode::SignExtend32,
                (false, 64) => NarrowValueMode::ZeroExtend64,
                (true, 64) => NarrowValueMode::SignExtend64,
                _ => panic!("Unknown input size"),
            };
            let rn = put_input_in_reg(ctx, inputs[0], narrow_mode);
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            ctx.emit(Inst::IntToFpu { op, rd, rn });
        }

        Opcode::FcvtToUint | Opcode::FcvtToSint => {
            let in_bits = ty_bits(ctx.input_ty(insn, 0));
            let out_bits = ty_bits(ctx.output_ty(insn, 0));
            let signed = op == Opcode::FcvtToSint;
            let op = match (signed, in_bits, out_bits) {
                (false, 32, 32) => FpuToIntOp::F32ToU32,
                (true, 32, 32) => FpuToIntOp::F32ToI32,
                (false, 32, 64) => FpuToIntOp::F32ToU64,
                (true, 32, 64) => FpuToIntOp::F32ToI64,
                (false, 64, 32) => FpuToIntOp::F64ToU32,
                (true, 64, 32) => FpuToIntOp::F64ToI32,
                (false, 64, 64) => FpuToIntOp::F64ToU64,
                (true, 64, 64) => FpuToIntOp::F64ToI64,
                _ => panic!("Unknown input/output-bits combination"),
            };

            let rn = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();

            // First, check whether the input is a NaN and trap if so.
            if in_bits == 32 {
                ctx.emit(Inst::FpuCmp32 { rn, rm: rn });
            } else {
                ctx.emit(Inst::FpuCmp64 { rn, rm: rn });
            }
            ctx.emit(Inst::TrapIf {
                trap_code: TrapCode::BadConversionToInteger,
                cond: Cond::from_floatcc(FloatCC::Unordered),
            });

            // Perform the conversion.  If this sets CC 3, we have a
            // "special case".  Since we already exluded the case where
            // the input was a NaN, the only other option is that the
            // conversion overflowed the target type.
            ctx.emit(Inst::FpuToInt { op, rd, rn });
            ctx.emit(Inst::TrapIf {
                trap_code: TrapCode::IntegerOverflow,
                cond: Cond::from_floatcc(FloatCC::Unordered),
            });
        }

        Opcode::FcvtToUintSat | Opcode::FcvtToSintSat => {
            let in_bits = ty_bits(ctx.input_ty(insn, 0));
            let out_bits = ty_bits(ctx.output_ty(insn, 0));
            let signed = op == Opcode::FcvtToSintSat;
            let op = match (signed, in_bits, out_bits) {
                (false, 32, 32) => FpuToIntOp::F32ToU32,
                (true, 32, 32) => FpuToIntOp::F32ToI32,
                (false, 32, 64) => FpuToIntOp::F32ToU64,
                (true, 32, 64) => FpuToIntOp::F32ToI64,
                (false, 64, 32) => FpuToIntOp::F64ToU32,
                (true, 64, 32) => FpuToIntOp::F64ToI32,
                (false, 64, 64) => FpuToIntOp::F64ToU64,
                (true, 64, 64) => FpuToIntOp::F64ToI64,
                _ => panic!("Unknown input/output-bits combination"),
            };

            let rn = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();

            // Perform the conversion.
            ctx.emit(Inst::FpuToInt { op, rd, rn });

            // In most special cases, the Z instruction already yields the
            // result expected by Cranelift semantic.  The only exception
            // it the case where the input was a Nan.  We explicitly check
            // for that and force the output to 0 in that case.
            if in_bits == 32 {
                ctx.emit(Inst::FpuCmp32 { rn, rm: rn });
            } else {
                ctx.emit(Inst::FpuCmp64 { rn, rm: rn });
            }
            let cond = Cond::from_floatcc(FloatCC::Unordered);
            if out_bits <= 32 {
                ctx.emit(Inst::CMov32SImm16 { rd, cond, imm: 0 });
            } else {
                ctx.emit(Inst::CMov64SImm16 { rd, cond, imm: 0 });
            }
        }

        Opcode::FcvtLowFromSint => unimplemented!("FcvtLowFromSint"),

        Opcode::Bitcast => {
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let rn = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
            let input_ty = ctx.input_ty(insn, 0);
            let output_ty = ctx.output_ty(insn, 0);
            lower_bitcast(ctx, rd, output_ty, rn, input_ty);
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
            let off = ctx.data(insn).load_store_offset().unwrap();
            let flags = ctx.memflags(insn).unwrap();
            let endianness = flags.endianness(Endianness::Big);
            let elem_ty = ctx.output_ty(insn, 0);
            let is_float = ty_is_float(elem_ty);
            let to_bits = ty_bits(elem_ty);
            let from_bits = match op {
                Opcode::Load | Opcode::LoadComplex => to_bits,
                Opcode::Sload8 | Opcode::Uload8 | Opcode::Sload8Complex | Opcode::Uload8Complex => {
                    8
                }
                Opcode::Sload16
                | Opcode::Uload16
                | Opcode::Sload16Complex
                | Opcode::Uload16Complex => 16,
                Opcode::Sload32
                | Opcode::Uload32
                | Opcode::Sload32Complex
                | Opcode::Uload32Complex => 32,
                _ => unreachable!(),
            };
            let ext_bits = if to_bits < 32 { 32 } else { to_bits };
            let sign_extend = match op {
                Opcode::Sload8
                | Opcode::Sload8Complex
                | Opcode::Sload16
                | Opcode::Sload16Complex
                | Opcode::Sload32
                | Opcode::Sload32Complex => true,
                _ => false,
            };

            let mem = lower_address(ctx, &inputs[..], off, flags);
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();

            if endianness == Endianness::Big {
                ctx.emit(match (ext_bits, from_bits, sign_extend, is_float) {
                    (32, 32, _, true) => Inst::FpuLoad32 { rd, mem },
                    (64, 64, _, true) => Inst::FpuLoad64 { rd, mem },
                    (32, 32, _, false) => Inst::Load32 { rd, mem },
                    (64, 64, _, false) => Inst::Load64 { rd, mem },
                    (32, 8, false, _) => Inst::Load32ZExt8 { rd, mem },
                    (32, 8, true, _) => Inst::Load32SExt8 { rd, mem },
                    (32, 16, false, _) => Inst::Load32ZExt16 { rd, mem },
                    (32, 16, true, _) => Inst::Load32SExt16 { rd, mem },
                    (64, 8, false, _) => Inst::Load64ZExt8 { rd, mem },
                    (64, 8, true, _) => Inst::Load64SExt8 { rd, mem },
                    (64, 16, false, _) => Inst::Load64ZExt16 { rd, mem },
                    (64, 16, true, _) => Inst::Load64SExt16 { rd, mem },
                    (64, 32, false, _) => Inst::Load64ZExt32 { rd, mem },
                    (64, 32, true, _) => Inst::Load64SExt32 { rd, mem },
                    _ => panic!("Unsupported size in load"),
                });
            } else if !is_float {
                ctx.emit(match (ext_bits, from_bits, sign_extend) {
                    (_, 16, _) => Inst::LoadRev16 { rd, mem },
                    (_, 32, _) => Inst::LoadRev32 { rd, mem },
                    (_, 64, _) => Inst::LoadRev64 { rd, mem },
                    (32, 8, false) => Inst::Load32ZExt8 { rd, mem },
                    (32, 8, true) => Inst::Load32SExt8 { rd, mem },
                    (64, 8, false) => Inst::Load64ZExt8 { rd, mem },
                    (64, 8, true) => Inst::Load64SExt8 { rd, mem },
                    _ => panic!("Unsupported size in load"),
                });
                if to_bits > from_bits && from_bits > 8 {
                    ctx.emit(Inst::Extend {
                        rd,
                        rn: rd.to_reg(),
                        signed: sign_extend,
                        from_bits: from_bits as u8,
                        to_bits: to_bits as u8,
                    });
                }
            } else if isa_flags.has_vxrs_ext2() {
                ctx.emit(match from_bits {
                    32 => Inst::FpuLoadRev32 { rd, mem },
                    64 => Inst::FpuLoadRev64 { rd, mem },
                    _ => panic!("Unsupported size in load"),
                });
            } else {
                match from_bits {
                    32 => {
                        let tmp = ctx.alloc_tmp(types::I32).only_reg().unwrap();
                        ctx.emit(Inst::LoadRev32 { rd: tmp, mem });
                        lower_bitcast(ctx, rd, elem_ty, tmp.to_reg(), types::I32);
                    }
                    64 => {
                        let tmp = ctx.alloc_tmp(types::I64).only_reg().unwrap();
                        ctx.emit(Inst::LoadRev64 { rd: tmp, mem });
                        lower_bitcast(ctx, rd, elem_ty, tmp.to_reg(), types::I64);
                    }
                    _ => panic!("Unsupported size in load"),
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
            let off = ctx.data(insn).load_store_offset().unwrap();
            let flags = ctx.memflags(insn).unwrap();
            let endianness = flags.endianness(Endianness::Big);
            let elem_ty = match op {
                Opcode::Istore8 | Opcode::Istore8Complex => types::I8,
                Opcode::Istore16 | Opcode::Istore16Complex => types::I16,
                Opcode::Istore32 | Opcode::Istore32Complex => types::I32,
                Opcode::Store | Opcode::StoreComplex => ctx.input_ty(insn, 0),
                _ => unreachable!(),
            };

            let mem = lower_address(ctx, &inputs[1..], off, flags);

            if ty_is_float(elem_ty) {
                let rd = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
                if endianness == Endianness::Big {
                    ctx.emit(match ty_bits(elem_ty) {
                        32 => Inst::FpuStore32 { rd, mem },
                        64 => Inst::FpuStore64 { rd, mem },
                        _ => panic!("Unsupported size in store"),
                    });
                } else if isa_flags.has_vxrs_ext2() {
                    ctx.emit(match ty_bits(elem_ty) {
                        32 => Inst::FpuStoreRev32 { rd, mem },
                        64 => Inst::FpuStoreRev64 { rd, mem },
                        _ => panic!("Unsupported size in store"),
                    });
                } else {
                    match ty_bits(elem_ty) {
                        32 => {
                            let tmp = ctx.alloc_tmp(types::I32).only_reg().unwrap();
                            lower_bitcast(ctx, tmp, types::I32, rd, elem_ty);
                            ctx.emit(Inst::StoreRev32 {
                                rd: tmp.to_reg(),
                                mem,
                            });
                        }
                        64 => {
                            let tmp = ctx.alloc_tmp(types::I64).only_reg().unwrap();
                            lower_bitcast(ctx, tmp, types::I64, rd, elem_ty);
                            ctx.emit(Inst::StoreRev64 {
                                rd: tmp.to_reg(),
                                mem,
                            });
                        }
                        _ => panic!("Unsupported size in load"),
                    }
                }
            } else if ty_bits(elem_ty) <= 16 {
                if let Some(imm) = input_matches_const(ctx, inputs[0]) {
                    ctx.emit(match (endianness, ty_bits(elem_ty)) {
                        (_, 1) | (_, 8) => Inst::StoreImm8 {
                            imm: imm as u8,
                            mem,
                        },
                        (Endianness::Big, 16) => Inst::StoreImm16 {
                            imm: imm as i16,
                            mem,
                        },
                        (Endianness::Little, 16) => Inst::StoreImm16 {
                            imm: (imm as i16).swap_bytes(),
                            mem,
                        },
                        _ => panic!("Unsupported size in store"),
                    });
                } else {
                    let rd = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
                    ctx.emit(match (endianness, ty_bits(elem_ty)) {
                        (_, 1) | (_, 8) => Inst::Store8 { rd, mem },
                        (Endianness::Big, 16) => Inst::Store16 { rd, mem },
                        (Endianness::Little, 16) => Inst::StoreRev16 { rd, mem },
                        _ => panic!("Unsupported size in store"),
                    });
                }
            } else if endianness == Endianness::Big {
                if let Some(imm) = input_matches_simm16(ctx, inputs[0]) {
                    ctx.emit(match ty_bits(elem_ty) {
                        32 => Inst::StoreImm32SExt16 { imm, mem },
                        64 => Inst::StoreImm64SExt16 { imm, mem },
                        _ => panic!("Unsupported size in store"),
                    });
                } else {
                    let rd = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
                    ctx.emit(match ty_bits(elem_ty) {
                        32 => Inst::Store32 { rd, mem },
                        64 => Inst::Store64 { rd, mem },
                        _ => panic!("Unsupported size in store"),
                    });
                }
            } else {
                let rd = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
                ctx.emit(match ty_bits(elem_ty) {
                    32 => Inst::StoreRev32 { rd, mem },
                    64 => Inst::StoreRev64 { rd, mem },
                    _ => panic!("Unsupported size in store"),
                });
            }
        }

        Opcode::StackLoad | Opcode::StackStore => {
            panic!("Direct stack memory access not supported; should not be used by Wasm");
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

        Opcode::ConstAddr => unimplemented!(),

        Opcode::FuncAddr => {
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let (extname, dist) = ctx.call_target(insn).unwrap();
            let extname = extname.clone();
            if dist == RelocDistance::Near {
                ctx.emit(Inst::LoadAddr {
                    rd,
                    mem: MemArg::Symbol {
                        name: Box::new(extname),
                        offset: 0,
                        flags: MemFlags::trusted(),
                    },
                });
            } else {
                ctx.emit(Inst::LoadExtNameFar {
                    rd,
                    name: Box::new(extname),
                    offset: 0,
                });
            }
        }

        Opcode::SymbolValue => {
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let (extname, dist, offset) = ctx.symbol_value(insn).unwrap();
            let extname = extname.clone();
            if dist == RelocDistance::Near && (offset & 1) == 0 && i32::try_from(offset).is_ok() {
                ctx.emit(Inst::LoadAddr {
                    rd,
                    mem: MemArg::Symbol {
                        name: Box::new(extname),
                        offset: i32::try_from(offset).unwrap(),
                        flags: MemFlags::trusted(),
                    },
                });
            } else {
                ctx.emit(Inst::LoadExtNameFar {
                    rd,
                    name: Box::new(extname),
                    offset,
                });
            }
        }

        Opcode::HeapAddr => {
            panic!("heap_addr should have been removed by legalization!");
        }

        Opcode::TableAddr => {
            panic!("table_addr should have been removed by legalization!");
        }

        Opcode::GlobalValue => {
            panic!("global_value should have been removed by legalization!");
        }

        Opcode::TlsValue => {
            unimplemented!("Thread-local storage support not implemented!");
        }

        Opcode::GetPinnedReg | Opcode::SetPinnedReg => {
            unimplemented!("Pinned register support not implemented!");
        }

        Opcode::Icmp => {
            let condcode = ctx.data(insn).cond_code().unwrap();
            let cond = Cond::from_intcc(condcode);
            let is_signed = condcode_is_signed(condcode);
            lower_icmp_to_flags(ctx, insn, is_signed, true);

            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let ty = ctx.output_ty(insn, 0);
            lower_flags_to_bool_result(ctx, cond, rd, ty);
        }

        Opcode::Fcmp => {
            let condcode = ctx.data(insn).fp_cond_code().unwrap();
            let cond = Cond::from_floatcc(condcode);
            lower_fcmp_to_flags(ctx, insn);

            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let ty = ctx.output_ty(insn, 0);
            lower_flags_to_bool_result(ctx, cond, rd, ty);
        }

        Opcode::IsNull | Opcode::IsInvalid => {
            // Null references are represented by the constant value 0; invalid
            // references are represented by the constant value -1.
            let cond = Cond::from_intcc(IntCC::Equal);
            let imm = match op {
                Opcode::IsNull => 0,
                Opcode::IsInvalid => -1,
                _ => unreachable!(),
            };
            let rn = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
            ctx.emit(Inst::CmpRSImm16 {
                op: CmpOp::CmpS64,
                rn,
                imm,
            });

            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let ty = ctx.output_ty(insn, 0);
            lower_flags_to_bool_result(ctx, cond, rd, ty);
        }

        Opcode::Select => {
            let ty = ctx.output_ty(insn, 0);
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let rn = put_input_in_reg(ctx, inputs[1], NarrowValueMode::None);
            let rm = put_input_in_reg(ctx, inputs[2], NarrowValueMode::None);
            let cond = lower_boolean_to_flags(ctx, inputs[0]);
            ctx.emit(Inst::gen_move(rd, rm, ty));
            if ty_is_float(ty) {
                if ty_bits(ty) < 64 {
                    ctx.emit(Inst::FpuCMov32 { rd, cond, rm: rn });
                } else {
                    ctx.emit(Inst::FpuCMov64 { rd, cond, rm: rn });
                }
            } else {
                if ty_bits(ty) < 64 {
                    ctx.emit(Inst::CMov32 { rd, cond, rm: rn });
                } else {
                    ctx.emit(Inst::CMov64 { rd, cond, rm: rn });
                }
            }
        }

        Opcode::SelectifSpectreGuard => {
            let ty = ctx.output_ty(insn, 0);
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let rn = put_input_in_reg(ctx, inputs[1], NarrowValueMode::None);
            let rm = put_input_in_reg(ctx, inputs[2], NarrowValueMode::None);
            let condcode = ctx.data(insn).cond_code().unwrap();
            let cond = Cond::from_intcc(condcode);
            let is_signed = condcode_is_signed(condcode);

            // Verification ensures that the input is always a single-def ifcmp.
            let cmp_insn = ctx
                .get_input_as_source_or_const(inputs[0].insn, inputs[0].input)
                .inst
                .unwrap()
                .0;
            debug_assert_eq!(ctx.data(cmp_insn).opcode(), Opcode::Ifcmp);
            lower_icmp_to_flags(ctx, cmp_insn, is_signed, true);

            ctx.emit(Inst::gen_move(rd, rm, ty));
            if ty_is_float(ty) {
                if ty_bits(ty) < 64 {
                    ctx.emit(Inst::FpuCMov32 { rd, cond, rm: rn });
                } else {
                    ctx.emit(Inst::FpuCMov64 { rd, cond, rm: rn });
                }
            } else {
                if ty_bits(ty) < 64 {
                    ctx.emit(Inst::CMov32 { rd, cond, rm: rn });
                } else {
                    ctx.emit(Inst::CMov64 { rd, cond, rm: rn });
                }
            }
        }

        Opcode::Trap | Opcode::ResumableTrap => {
            let trap_code = ctx.data(insn).trap_code().unwrap();
            ctx.emit_safepoint(Inst::Trap { trap_code })
        }

        Opcode::Trapz | Opcode::Trapnz | Opcode::ResumableTrapnz => {
            let cond = lower_boolean_to_flags(ctx, inputs[0]);
            let negated = op == Opcode::Trapz;
            let cond = if negated { cond.invert() } else { cond };
            let trap_code = ctx.data(insn).trap_code().unwrap();
            ctx.emit_safepoint(Inst::TrapIf { trap_code, cond });
        }

        Opcode::Trapif => {
            let condcode = ctx.data(insn).cond_code().unwrap();
            let mut cond = Cond::from_intcc(condcode);
            let is_signed = condcode_is_signed(condcode);

            let cmp_insn = ctx
                .get_input_as_source_or_const(inputs[0].insn, inputs[0].input)
                .inst
                .unwrap()
                .0;
            if ctx.data(cmp_insn).opcode() == Opcode::IaddIfcout {
                // The flags must not have been clobbered by any other instruction between the
                // iadd_ifcout and this instruction, as verified by the CLIF validator; so we
                // can simply rely on the condition code here.
                //
                // IaddIfcout is implemented via a ADD LOGICAL instruction, which sets the
                // the condition code as follows:
                //   0   Result zero; no carry
                //   1   Result not zero; no carry
                //   2   Result zero; carry
                //   3   Result not zero; carry
                // This means "carry" corresponds to condition code 2 or 3, i.e.
                // a condition mask of 2 | 1.
                //
                // As this does not match any of the encodings used with a normal integer
                // comparsion, this cannot be represented by any IntCC value.  We need to
                // remap the IntCC::UnsignedGreaterThan value that we have here as result
                // of the unsigned_add_overflow_condition call to the correct mask.
                assert!(condcode == IntCC::UnsignedGreaterThan);
                cond = Cond::from_mask(2 | 1);
            } else {
                // Verification ensures that the input is always a single-def ifcmp
                debug_assert_eq!(ctx.data(cmp_insn).opcode(), Opcode::Ifcmp);
                lower_icmp_to_flags(ctx, cmp_insn, is_signed, true);
            }

            let trap_code = ctx.data(insn).trap_code().unwrap();
            ctx.emit_safepoint(Inst::TrapIf { trap_code, cond });
        }

        Opcode::Debugtrap => {
            ctx.emit(Inst::Debugtrap);
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
                        S390xABICaller::from_func(sig, &extname, dist, caller_conv, flags)?,
                        &inputs[..],
                    )
                }
                Opcode::CallIndirect => {
                    let ptr = put_input_in_reg(ctx, inputs[0], NarrowValueMode::ZeroExtend64);
                    let sig = ctx.call_sig(insn).unwrap();
                    assert!(inputs.len() - 1 == sig.params.len());
                    assert!(outputs.len() == sig.returns.len());
                    (
                        S390xABICaller::from_ptr(sig, ptr, op, caller_conv, flags)?,
                        &inputs[1..],
                    )
                }
                _ => unreachable!(),
            };

            assert!(inputs.len() == abi.num_args());
            for (i, input) in inputs.iter().enumerate() {
                let arg_reg = put_input_in_reg(ctx, *input, NarrowValueMode::None);
                abi.emit_copy_regs_to_arg(ctx, i, ValueRegs::one(arg_reg));
            }
            abi.emit_call(ctx);
            for (i, output) in outputs.iter().enumerate() {
                let retval_reg = get_output_reg(ctx, *output).only_reg().unwrap();
                abi.emit_copy_retval_to_regs(ctx, i, ValueRegs::one(retval_reg));
            }
            abi.accumulate_outgoing_args_size(ctx);
        }

        Opcode::FallthroughReturn | Opcode::Return => {
            for (i, input) in inputs.iter().enumerate() {
                let reg = put_input_in_reg(ctx, *input, NarrowValueMode::None);
                let retval_reg = ctx.retval(i).only_reg().unwrap();
                let ty = ctx.input_ty(insn, i);
                ctx.emit(Inst::gen_move(retval_reg, reg, ty));
            }
            // N.B.: the Ret itself is generated by the ABI.
        }

        Opcode::AtomicRmw => {
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let addr = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
            let rn = put_input_in_reg(ctx, inputs[1], NarrowValueMode::None);
            let flags = ctx.memflags(insn).unwrap();
            let endianness = flags.endianness(Endianness::Big);
            let ty = ty.unwrap();
            assert!(is_valid_atomic_transaction_ty(ty));
            if endianness == Endianness::Little {
                unimplemented!("Little-endian atomic operations not implemented");
            }
            if ty_bits(ty) < 32 {
                unimplemented!("Sub-word atomic operations not implemented");
            }
            let op = inst_common::AtomicRmwOp::from(ctx.data(insn).atomic_rmw_op().unwrap());
            let (alu_op, rn) = match op {
                AtomicRmwOp::And => (choose_32_64(ty, ALUOp::And32, ALUOp::And64), rn),
                AtomicRmwOp::Or => (choose_32_64(ty, ALUOp::Orr32, ALUOp::Orr64), rn),
                AtomicRmwOp::Xor => (choose_32_64(ty, ALUOp::Xor32, ALUOp::Xor64), rn),
                AtomicRmwOp::Add => (choose_32_64(ty, ALUOp::Add32, ALUOp::Add64), rn),
                AtomicRmwOp::Sub => {
                    let tmp_ty = choose_32_64(ty, types::I32, types::I64);
                    let tmp = ctx.alloc_tmp(tmp_ty).only_reg().unwrap();
                    let neg_op = choose_32_64(ty, UnaryOp::Neg32, UnaryOp::Neg64);
                    ctx.emit(Inst::UnaryRR {
                        op: neg_op,
                        rd: tmp,
                        rn,
                    });
                    (choose_32_64(ty, ALUOp::Add32, ALUOp::Add64), tmp.to_reg())
                }
                _ => unimplemented!("AtomicRmw operation type {:?} not implemented", op),
            };
            let mem = MemArg::reg(addr, flags);
            ctx.emit(Inst::AtomicRmw {
                alu_op,
                rd,
                rn,
                mem,
            });
        }
        Opcode::AtomicCas => {
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();
            let addr = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
            let rm = put_input_in_reg(ctx, inputs[1], NarrowValueMode::None);
            let rn = put_input_in_reg(ctx, inputs[2], NarrowValueMode::None);
            let flags = ctx.memflags(insn).unwrap();
            let endianness = flags.endianness(Endianness::Big);
            let ty = ty.unwrap();
            assert!(is_valid_atomic_transaction_ty(ty));
            if endianness == Endianness::Little {
                unimplemented!("Little-endian atomic operations not implemented");
            }
            if ty_bits(ty) < 32 {
                unimplemented!("Sub-word atomic operations not implemented");
            }
            let mem = MemArg::reg(addr, flags);
            ctx.emit(Inst::gen_move(rd, rm, ty));
            if ty_bits(ty) == 32 {
                ctx.emit(Inst::AtomicCas32 { rd, rn, mem });
            } else {
                ctx.emit(Inst::AtomicCas64 { rd, rn, mem });
            }
        }
        Opcode::AtomicLoad => {
            let flags = ctx.memflags(insn).unwrap();
            let endianness = flags.endianness(Endianness::Big);
            let ty = ty.unwrap();
            assert!(is_valid_atomic_transaction_ty(ty));

            let mem = lower_address(ctx, &inputs[..], 0, flags);
            let rd = get_output_reg(ctx, outputs[0]).only_reg().unwrap();

            if endianness == Endianness::Big {
                ctx.emit(match ty_bits(ty) {
                    8 => Inst::Load32ZExt8 { rd, mem },
                    16 => Inst::Load32ZExt16 { rd, mem },
                    32 => Inst::Load32 { rd, mem },
                    64 => Inst::Load64 { rd, mem },
                    _ => panic!("Unsupported size in load"),
                });
            } else {
                ctx.emit(match ty_bits(ty) {
                    8 => Inst::Load32ZExt8 { rd, mem },
                    16 => Inst::LoadRev16 { rd, mem },
                    32 => Inst::LoadRev32 { rd, mem },
                    64 => Inst::LoadRev64 { rd, mem },
                    _ => panic!("Unsupported size in load"),
                });
            }
        }
        Opcode::AtomicStore => {
            let flags = ctx.memflags(insn).unwrap();
            let endianness = flags.endianness(Endianness::Big);
            let ty = ctx.input_ty(insn, 0);
            assert!(is_valid_atomic_transaction_ty(ty));

            let mem = lower_address(ctx, &inputs[1..], 0, flags);

            if ty_bits(ty) <= 16 {
                if let Some(imm) = input_matches_const(ctx, inputs[0]) {
                    ctx.emit(match (endianness, ty_bits(ty)) {
                        (_, 8) => Inst::StoreImm8 {
                            imm: imm as u8,
                            mem,
                        },
                        (Endianness::Big, 16) => Inst::StoreImm16 {
                            imm: imm as i16,
                            mem,
                        },
                        (Endianness::Little, 16) => Inst::StoreImm16 {
                            imm: (imm as i16).swap_bytes(),
                            mem,
                        },
                        _ => panic!("Unsupported size in store"),
                    });
                } else {
                    let rd = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
                    ctx.emit(match (endianness, ty_bits(ty)) {
                        (_, 8) => Inst::Store8 { rd, mem },
                        (Endianness::Big, 16) => Inst::Store16 { rd, mem },
                        (Endianness::Little, 16) => Inst::StoreRev16 { rd, mem },
                        _ => panic!("Unsupported size in store"),
                    });
                }
            } else if endianness == Endianness::Big {
                if let Some(imm) = input_matches_simm16(ctx, inputs[0]) {
                    ctx.emit(match ty_bits(ty) {
                        32 => Inst::StoreImm32SExt16 { imm, mem },
                        64 => Inst::StoreImm64SExt16 { imm, mem },
                        _ => panic!("Unsupported size in store"),
                    });
                } else {
                    let rd = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
                    ctx.emit(match ty_bits(ty) {
                        32 => Inst::Store32 { rd, mem },
                        64 => Inst::Store64 { rd, mem },
                        _ => panic!("Unsupported size in store"),
                    });
                }
            } else {
                let rd = put_input_in_reg(ctx, inputs[0], NarrowValueMode::None);
                ctx.emit(match ty_bits(ty) {
                    32 => Inst::StoreRev32 { rd, mem },
                    64 => Inst::StoreRev64 { rd, mem },
                    _ => panic!("Unsupported size in store"),
                });
            }

            ctx.emit(Inst::Fence);
        }
        Opcode::Fence => {
            ctx.emit(Inst::Fence);
        }

        Opcode::RawBitcast
        | Opcode::Splat
        | Opcode::Swizzle
        | Opcode::Insertlane
        | Opcode::Extractlane
        | Opcode::Imin
        | Opcode::Umin
        | Opcode::Imax
        | Opcode::Umax
        | Opcode::AvgRound
        | Opcode::FminPseudo
        | Opcode::FmaxPseudo
        | Opcode::Uload8x8
        | Opcode::Uload8x8Complex
        | Opcode::Sload8x8
        | Opcode::Sload8x8Complex
        | Opcode::Uload16x4
        | Opcode::Uload16x4Complex
        | Opcode::Sload16x4
        | Opcode::Sload16x4Complex
        | Opcode::Uload32x2
        | Opcode::Uload32x2Complex
        | Opcode::Sload32x2
        | Opcode::Sload32x2Complex
        | Opcode::Vconst
        | Opcode::Shuffle
        | Opcode::Vsplit
        | Opcode::Vconcat
        | Opcode::Vselect
        | Opcode::VanyTrue
        | Opcode::VallTrue
        | Opcode::VhighBits
        | Opcode::ScalarToVector
        | Opcode::Snarrow
        | Opcode::Unarrow
        | Opcode::Uunarrow
        | Opcode::SwidenLow
        | Opcode::SwidenHigh
        | Opcode::UwidenLow
        | Opcode::UwidenHigh
        | Opcode::WideningPairwiseDotProductS
        | Opcode::SqmulRoundSat
        | Opcode::FvpromoteLow
        | Opcode::Fvdemote
        | Opcode::ExtendedPairwiseAddSigned
        | Opcode::ExtendedPairwiseAddUnsigned => {
            // TODO
            unimplemented!("Vector ops not implemented.");
        }

        Opcode::Isplit | Opcode::Iconcat => unimplemented!("Wide integer ops not implemented."),

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
        | Opcode::DummySargT
        | Opcode::IfcmpSp
        | Opcode::Regspill
        | Opcode::Regfill => {
            panic!("Unused opcode should not be encountered.");
        }

        Opcode::Ifcmp
        | Opcode::Ffcmp
        | Opcode::Trapff
        | Opcode::Trueif
        | Opcode::Trueff
        | Opcode::Selectif => {
            panic!("Flags opcode should not be encountered.");
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

        Opcode::JumpTableEntry | Opcode::JumpTableBase => {
            panic!("Should not appear: we handle BrTable directly");
        }

        Opcode::Safepoint => {
            panic!("safepoint instructions not used by new backend's safepoints!");
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
        | Opcode::X86Palignr
        | Opcode::X86ElfTlsGetAddr
        | Opcode::X86MachoTlsGetAddr => {
            panic!("x86-specific opcode in supposedly arch-neutral IR!");
        }
    }

    Ok(())
}

//============================================================================
// Lowering: main entry point for lowering a branch group

fn lower_branch<C: LowerCtx<I = Inst>>(
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
        let not_taken = BranchTarget::Label(targets[1]);

        match op0 {
            Opcode::Brz | Opcode::Brnz => {
                let flag_input = InsnInput {
                    insn: branches[0],
                    input: 0,
                };
                let cond = lower_boolean_to_flags(ctx, flag_input);
                let negated = op0 == Opcode::Brz;
                let cond = if negated { cond.invert() } else { cond };
                ctx.emit(Inst::CondBr {
                    taken,
                    not_taken,
                    cond,
                });
            }

            Opcode::Brif => {
                let condcode = ctx.data(branches[0]).cond_code().unwrap();
                let cond = Cond::from_intcc(condcode);
                let is_signed = condcode_is_signed(condcode);

                // Verification ensures that the input is always a single-def ifcmp.
                let cmp_insn = ctx
                    .get_input_as_source_or_const(branches[0], 0)
                    .inst
                    .unwrap()
                    .0;
                debug_assert_eq!(ctx.data(cmp_insn).opcode(), Opcode::Ifcmp);
                lower_icmp_to_flags(ctx, cmp_insn, is_signed, true);

                ctx.emit(Inst::CondBr {
                    taken,
                    not_taken,
                    cond,
                });
            }

            Opcode::Brff => unreachable!(),

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
                let jt_size = targets.len() - 1;
                assert!(jt_size <= std::u32::MAX as usize);

                // Load up jump table element index.
                let ridx = put_input_in_reg(
                    ctx,
                    InsnInput {
                        insn: branches[0],
                        input: 0,
                    },
                    NarrowValueMode::ZeroExtend64,
                );

                // Temp registers needed by the compound instruction.
                let rtmp1 = ctx.alloc_tmp(types::I64).only_reg().unwrap();
                let rtmp2 = ctx.alloc_tmp(types::I64).only_reg().unwrap();

                // Emit the compound instruction that does:
                //
                // clgfi %rIdx, <jt-size>
                // jghe <default-target>
                // sllg %rTmp2, %rIdx, 2
                // larl %rTmp1, <jt-base>
                // lgf %rTmp2, 0(%rTmp2, %rTmp1)
                // agrk %rTmp1, %rTmp1, %rTmp2
                // br %rA
                // [jt entries]
                //
                // This must be *one* instruction in the vcode because
                // we cannot allow regalloc to insert any spills/fills
                // in the middle of the sequence; otherwise, the ADR's
                // PC-rel offset to the jumptable would be incorrect.
                // (The alternative is to introduce a relocation pass
                // for inlined jumptables, which is much worse, IMHO.)

                let default_target = BranchTarget::Label(targets[0]);
                let jt_targets: Vec<BranchTarget> = targets
                    .iter()
                    .skip(1)
                    .map(|bix| BranchTarget::Label(*bix))
                    .collect();
                let targets_for_term: Vec<MachLabel> = targets.to_vec();
                ctx.emit(Inst::JTSequence {
                    ridx,
                    rtmp1,
                    rtmp2,
                    info: Box::new(JTSequenceInfo {
                        default_target,
                        targets: jt_targets,
                        targets_for_term: targets_for_term,
                    }),
                });
            }

            _ => panic!("Unknown branch type!"),
        }
    }

    Ok(())
}

//=============================================================================
// Lowering-backend trait implementation.

impl LowerBackend for S390xBackend {
    type MInst = Inst;

    fn lower<C: LowerCtx<I = Inst>>(&self, ctx: &mut C, ir_inst: IRInst) -> CodegenResult<()> {
        lower_insn_to_regs(ctx, ir_inst, &self.flags, &self.isa_flags)
    }

    fn lower_branch_group<C: LowerCtx<I = Inst>>(
        &self,
        ctx: &mut C,
        branches: &[IRInst],
        targets: &[MachLabel],
    ) -> CodegenResult<()> {
        lower_branch(ctx, branches, targets)
    }
}
