//! This module defines x86_64-specific machine instruction types.

use crate::binemit::{CodeOffset, StackMap};
use crate::ir::{types, ExternalName, Opcode, SourceLoc, TrapCode, Type};
use crate::isa::x64::settings as x64_settings;
use crate::machinst::*;
use crate::{settings, settings::Flags, CodegenError, CodegenResult};
use alloc::boxed::Box;
use alloc::vec::Vec;
use regalloc::{
    PrettyPrint, PrettyPrintSized, RealRegUniverse, Reg, RegClass, RegUsageCollector,
    RegUsageMapper, SpillSlot, VirtualReg, Writable,
};
use smallvec::SmallVec;
use std::fmt;
use std::string::{String, ToString};

pub mod args;
mod emit;
#[cfg(test)]
mod emit_tests;
pub mod regs;

use args::*;
use regs::{create_reg_universe_systemv, show_ireg_sized};

//=============================================================================
// Instructions (top level): definition

// Don't build these directly.  Instead use the Inst:: functions to create them.

/// Instructions.  Destinations are on the RIGHT (a la AT&T syntax).
#[derive(Clone)]
pub enum Inst {
    /// Nops of various sizes, including zero.
    Nop { len: u8 },

    // =====================================
    // Integer instructions.
    /// Integer arithmetic/bit-twiddling: (add sub and or xor mul adc? sbb?) (32 64) (reg addr imm) reg
    AluRmiR {
        is_64: bool,
        op: AluRmiROpcode,
        src: RegMemImm,
        dst: Writable<Reg>,
    },

    /// Instructions on GPR that only read src and defines dst (dst is not modified): bsr, etc.
    UnaryRmR {
        size: u8, // 2, 4 or 8
        op: UnaryRmROpcode,
        src: RegMem,
        dst: Writable<Reg>,
    },

    /// Bitwise not
    Not {
        size: u8, // 1, 2, 4 or 8
        src: Writable<Reg>,
    },

    /// Integer negation
    Neg {
        size: u8, // 1, 2, 4 or 8
        src: Writable<Reg>,
    },

    /// Integer quotient and remainder: (div idiv) $rax $rdx (reg addr)
    Div {
        size: u8, // 1, 2, 4 or 8
        signed: bool,
        divisor: RegMem,
        loc: SourceLoc,
    },

    /// The high bits (RDX) of a (un)signed multiply: RDX:RAX := RAX * rhs.
    MulHi { size: u8, signed: bool, rhs: RegMem },

    /// A synthetic sequence to implement the right inline checks for remainder and division,
    /// assuming the dividend is in %rax.
    /// Puts the result back into %rax if is_div, %rdx if !is_div, to mimic what the div
    /// instruction does.
    /// The generated code sequence is described in the emit's function match arm for this
    /// instruction.
    ///
    /// Note: %rdx is marked as modified by this instruction, to avoid an early clobber problem
    /// with the temporary and divisor registers. Make sure to zero %rdx right before this
    /// instruction, or you might run into regalloc failures where %rdx is live before its first
    /// def!
    CheckedDivOrRemSeq {
        kind: DivOrRemKind,
        size: u8,
        /// The divisor operand. Note it's marked as modified so that it gets assigned a register
        /// different from the temporary.
        divisor: Writable<Reg>,
        tmp: Option<Writable<Reg>>,
        loc: SourceLoc,
    },

    /// Do a sign-extend based on the sign of the value in rax into rdx: (cwd cdq cqo)
    /// or al into ah: (cbw)
    SignExtendData {
        size: u8, // 1, 2, 4 or 8
    },

    /// Constant materialization: (imm32 imm64) reg.
    /// Either: movl $imm32, %reg32 or movabsq $imm64, %reg32.
    Imm {
        dst_is_64: bool,
        simm64: u64,
        dst: Writable<Reg>,
    },

    /// GPR to GPR move: mov (64 32) reg reg.
    MovRR {
        is_64: bool,
        src: Reg,
        dst: Writable<Reg>,
    },

    /// Zero-extended loads, except for 64 bits: movz (bl bq wl wq lq) addr reg.
    /// Note that the lq variant doesn't really exist since the default zero-extend rule makes it
    /// unnecessary. For that case we emit the equivalent "movl AM, reg32".
    MovzxRmR {
        ext_mode: ExtMode,
        src: RegMem,
        dst: Writable<Reg>,
        /// Source location, if the memory access can be out-of-bounds.
        srcloc: Option<SourceLoc>,
    },

    /// A plain 64-bit integer load, since MovZX_RM_R can't represent that.
    Mov64MR {
        src: SyntheticAmode,
        dst: Writable<Reg>,
        /// Source location, if the memory access can be out-of-bounds.
        srcloc: Option<SourceLoc>,
    },

    /// Loads the memory address of addr into dst.
    LoadEffectiveAddress {
        addr: SyntheticAmode,
        dst: Writable<Reg>,
    },

    /// Sign-extended loads and moves: movs (bl bq wl wq lq) addr reg.
    MovsxRmR {
        ext_mode: ExtMode,
        src: RegMem,
        dst: Writable<Reg>,
        /// Source location, if the memory access can be out-of-bounds.
        srcloc: Option<SourceLoc>,
    },

    /// Integer stores: mov (b w l q) reg addr.
    MovRM {
        size: u8, // 1, 2, 4 or 8.
        src: Reg,
        dst: SyntheticAmode,
        /// Source location, if the memory access can be out-of-bounds.
        srcloc: Option<SourceLoc>,
    },

    /// Arithmetic shifts: (shl shr sar) (b w l q) imm reg.
    ShiftR {
        size: u8, // 1, 2, 4 or 8
        kind: ShiftKind,
        /// shift count: Some(0 .. #bits-in-type - 1), or None to mean "%cl".
        num_bits: Option<u8>,
        dst: Writable<Reg>,
    },

    /// Arithmetic SIMD shifts.
    XmmRmiReg {
        opcode: SseOpcode,
        src: RegMemImm,
        dst: Writable<Reg>,
    },

    /// Integer comparisons/tests: cmp (b w l q) (reg addr imm) reg.
    CmpRmiR {
        size: u8, // 1, 2, 4 or 8
        src: RegMemImm,
        dst: Reg,
    },

    /// Materializes the requested condition code in the destination reg.
    Setcc { cc: CC, dst: Writable<Reg> },

    /// Integer conditional move.
    /// Overwrites the destination register.
    Cmove {
        /// Possible values are 2, 4 or 8. Checked in the related factory.
        size: u8,
        cc: CC,
        src: RegMem,
        dst: Writable<Reg>,
    },

    // =====================================
    // Stack manipulation.
    /// pushq (reg addr imm)
    Push64 { src: RegMemImm },

    /// popq reg
    Pop64 { dst: Writable<Reg> },

    // =====================================
    // Floating-point operations.
    /// XMM (scalar or vector) binary op: (add sub and or xor mul adc? sbb?) (32 64) (reg addr) reg
    XmmRmR {
        op: SseOpcode,
        src: RegMem,
        dst: Writable<Reg>,
    },

    /// XMM (scalar or vector) unary op: mov between XMM registers (32 64) (reg addr) reg, sqrt,
    /// etc.
    ///
    /// This differs from XMM_RM_R in that the dst register of XmmUnaryRmR is not used in the
    /// computation of the instruction dst value and so does not have to be a previously valid
    /// value. This is characteristic of mov instructions.
    XmmUnaryRmR {
        op: SseOpcode,
        src: RegMem,
        dst: Writable<Reg>,
        /// Source location, if the memory access can be out-of-bounds.
        srcloc: Option<SourceLoc>,
    },

    /// XMM (scalar or vector) unary op (from xmm to reg/mem): stores, movd, movq
    XmmMovRM {
        op: SseOpcode,
        src: Reg,
        dst: SyntheticAmode,
        /// Source location, if the memory access can be out-of-bounds.
        srcloc: Option<SourceLoc>,
    },

    /// XMM (vector) unary op (to move a constant value into an xmm register): movups
    XmmLoadConstSeq {
        val: Vec<u8>,
        dst: Writable<Reg>,
        ty: Type,
    },

    /// XMM (scalar) unary op (from xmm to integer reg): movd, movq, cvtts{s,d}2si
    XmmToGpr {
        op: SseOpcode,
        src: Reg,
        dst: Writable<Reg>,
        dst_size: OperandSize,
    },

    /// XMM (scalar) unary op (from integer to float reg): movd, movq, cvtsi2s{s,d}
    GprToXmm {
        op: SseOpcode,
        src: RegMem,
        dst: Writable<Reg>,
        src_size: OperandSize,
    },

    /// Converts an unsigned int64 to a float32/float64.
    CvtUint64ToFloatSeq {
        /// Is the target a 64-bits or 32-bits register?
        to_f64: bool,
        /// A copy of the source register, fed by lowering. It is marked as modified during
        /// register allocation to make sure that the temporary registers differ from the src
        /// register, since both registers are live at the same time in the generated code
        /// sequence.
        src: Writable<Reg>,
        dst: Writable<Reg>,
        tmp_gpr1: Writable<Reg>,
        tmp_gpr2: Writable<Reg>,
    },

    /// Converts a scalar xmm to a signed int32/int64.
    CvtFloatToSintSeq {
        dst_size: OperandSize,
        src_size: OperandSize,
        is_saturating: bool,
        /// A copy of the source register, fed by lowering. It is marked as modified during
        /// register allocation to make sure that the temporary xmm register differs from the src
        /// register, since both registers are live at the same time in the generated code
        /// sequence.
        src: Writable<Reg>,
        dst: Writable<Reg>,
        tmp_gpr: Writable<Reg>,
        tmp_xmm: Writable<Reg>,
        srcloc: SourceLoc,
    },

    /// Converts a scalar xmm to an unsigned int32/int64.
    CvtFloatToUintSeq {
        src_size: OperandSize,
        dst_size: OperandSize,
        is_saturating: bool,
        /// A copy of the source register, fed by lowering, reused as a temporary. It is marked as
        /// modified during register allocation to make sure that the temporary xmm register
        /// differs from the src register, since both registers are live at the same time in the
        /// generated code sequence.
        src: Writable<Reg>,
        dst: Writable<Reg>,
        tmp_gpr: Writable<Reg>,
        tmp_xmm: Writable<Reg>,
        srcloc: SourceLoc,
    },

    /// A sequence to compute min/max with the proper NaN semantics for xmm registers.
    XmmMinMaxSeq {
        size: OperandSize,
        is_min: bool,
        lhs: Reg,
        rhs_dst: Writable<Reg>,
    },

    /// XMM (scalar) conditional move.
    /// Overwrites the destination register if cc is set.
    XmmCmove {
        /// Whether the cmove is moving either 32 or 64 bits.
        is_64: bool,
        cc: CC,
        src: RegMem,
        dst: Writable<Reg>,
    },

    /// Float comparisons/tests: cmp (b w l q) (reg addr imm) reg.
    XmmCmpRmR {
        op: SseOpcode,
        src: RegMem,
        dst: Reg,
    },

    /// A binary XMM instruction with an 8-bit immediate: e.g. cmp (ps pd) imm (reg addr) reg
    XmmRmRImm {
        op: SseOpcode,
        src: RegMem,
        dst: Writable<Reg>,
        imm: u8,
        is64: bool,
    },

    // =====================================
    // Control flow instructions.
    /// Direct call: call simm32.
    CallKnown {
        dest: ExternalName,
        uses: Vec<Reg>,
        defs: Vec<Writable<Reg>>,
        loc: SourceLoc,
        opcode: Opcode,
    },

    /// Indirect call: callq (reg mem).
    CallUnknown {
        dest: RegMem,
        uses: Vec<Reg>,
        defs: Vec<Writable<Reg>>,
        loc: SourceLoc,
        opcode: Opcode,
    },

    /// Return.
    Ret,

    /// A placeholder instruction, generating no code, meaning that a function epilogue must be
    /// inserted there.
    EpiloguePlaceholder,

    /// Jump to a known target: jmp simm32.
    JmpKnown { dst: MachLabel },

    /// One-way conditional branch: jcond cond target.
    ///
    /// This instruction is useful when we have conditional jumps depending on more than two
    /// conditions, see for instance the lowering of Brz/brnz with Fcmp inputs.
    ///
    /// A note of caution: in contexts where the branch target is another block, this has to be the
    /// same successor as the one specified in the terminator branch of the current block.
    /// Otherwise, this might confuse register allocation by creating new invisible edges.
    JmpIf { cc: CC, taken: MachLabel },

    /// Two-way conditional branch: jcond cond target target.
    /// Emitted as a compound sequence; the MachBuffer will shrink it as appropriate.
    JmpCond {
        cc: CC,
        taken: MachLabel,
        not_taken: MachLabel,
    },

    /// Jump-table sequence, as one compound instruction (see note in lower.rs for rationale).
    /// The generated code sequence is described in the emit's function match arm for this
    /// instruction.
    /// See comment in lowering about the temporaries signedness.
    JmpTableSeq {
        idx: Reg,
        tmp1: Writable<Reg>,
        tmp2: Writable<Reg>,
        default_target: MachLabel,
        targets: Vec<MachLabel>,
        targets_for_term: Vec<MachLabel>,
    },

    /// Indirect jump: jmpq (reg mem).
    JmpUnknown { target: RegMem },

    /// Traps if the condition code is set.
    TrapIf {
        cc: CC,
        trap_code: TrapCode,
        srcloc: SourceLoc,
    },

    /// A debug trap.
    Hlt,

    /// An instruction that will always trigger the illegal instruction exception.
    Ud2 { trap_info: (SourceLoc, TrapCode) },

    /// Loads an external symbol in a register, with a relocation: movabsq $name, dst
    LoadExtName {
        dst: Writable<Reg>,
        name: Box<ExternalName>,
        srcloc: SourceLoc,
        offset: i64,
    },

    // =====================================
    // Instructions pertaining to atomic memory accesses.
    /// A standard (native) `lock cmpxchg src, (amode)`, with register conventions:
    ///
    /// `dst`  (read) address
    /// `src`  (read) replacement value
    /// %rax   (modified) in: expected value, out: value that was actually at `dst`
    /// %rflags is written.  Do not assume anything about it after the instruction.
    ///
    /// The instruction "succeeded" iff the lowest `ty` bits of %rax afterwards are the same as
    /// they were before.
    LockCmpxchg {
        ty: Type, // I8, I16, I32 or I64
        src: Reg,
        dst: SyntheticAmode,
        srcloc: Option<SourceLoc>,
    },

    /// A synthetic instruction, based on a loop around a native `lock cmpxchg` instruction.
    /// This atomically modifies a value in memory and returns the old value.  The sequence
    /// consists of an initial "normal" load from `dst`, followed by a loop which computes the
    /// new value and tries to compare-and-swap ("CAS") it into `dst`, using the native
    /// instruction `lock cmpxchg{b,w,l,q}` .  The loop iterates until the CAS is successful.
    /// If there is no contention, there will be only one pass through the loop body.  The
    /// sequence does *not* perform any explicit memory fence instructions
    /// (mfence/sfence/lfence).
    ///
    /// Note that the transaction is atomic in the sense that, as observed by some other thread,
    /// `dst` either has the initial or final value, but no other.  It isn't atomic in the sense
    /// of guaranteeing that no other thread writes to `dst` in between the initial load and the
    /// CAS -- but that would cause the CAS to fail unless the other thread's last write before
    /// the CAS wrote the same value that was already there.  In other words, this
    /// implementation suffers (unavoidably) from the A-B-A problem.
    ///
    /// This instruction sequence has fixed register uses as follows:
    ///
    /// %r9   (read) address
    /// %r10  (read) second operand for `op`
    /// %r11  (written) scratch reg; value afterwards has no meaning
    /// %rax  (written) the old value at %r9
    /// %rflags is written.  Do not assume anything about it after the instruction.
    AtomicRmwSeq {
        ty: Type, // I8, I16, I32 or I64
        op: inst_common::AtomicRmwOp,
        srcloc: Option<SourceLoc>,
    },

    /// A memory fence (mfence, lfence or sfence).
    Fence { kind: FenceKind },

    // =====================================
    // Meta-instructions generating no code.
    /// Marker, no-op in generated code: SP "virtual offset" is adjusted. This
    /// controls how MemArg::NominalSPOffset args are lowered.
    VirtualSPOffsetAdj { offset: i64 },

    /// Provides a way to tell the register allocator that the upcoming sequence of instructions
    /// will overwrite `dst` so it should be considered as a `def`; use this with care.
    ///
    /// This is useful when we have a sequence of instructions whose register usages are nominally
    /// `mod`s, but such that the combination of operations creates a result that is independent of
    /// the initial register value. It's thus semantically a `def`, not a `mod`, when all the
    /// instructions are taken together, so we want to ensure the register is defined (its
    /// live-range starts) prior to the sequence to keep analyses happy.
    ///
    /// One alternative would be a compound instruction that somehow encapsulates the others and
    /// reports its own `def`s/`use`s/`mod`s; this adds complexity (the instruction list is no
    /// longer flat) and requires knowledge about semantics and initial-value independence anyway.
    XmmUninitializedValue { dst: Writable<Reg> },
}

pub(crate) fn low32_will_sign_extend_to_64(x: u64) -> bool {
    let xs = x as i64;
    xs == ((xs << 32) >> 32)
}

impl Inst {
    fn isa_requirement(&self) -> Option<InstructionSet> {
        match self {
            // These instructions are part of SSE2, which is a basic requirement in Cranelift, and
            // don't have to be checked.
            Inst::AluRmiR { .. }
            | Inst::AtomicRmwSeq { .. }
            | Inst::CallKnown { .. }
            | Inst::CallUnknown { .. }
            | Inst::CheckedDivOrRemSeq { .. }
            | Inst::Cmove { .. }
            | Inst::CmpRmiR { .. }
            | Inst::CvtFloatToSintSeq { .. }
            | Inst::CvtFloatToUintSeq { .. }
            | Inst::CvtUint64ToFloatSeq { .. }
            | Inst::Div { .. }
            | Inst::EpiloguePlaceholder
            | Inst::Fence { .. }
            | Inst::Hlt
            | Inst::Imm { .. }
            | Inst::JmpCond { .. }
            | Inst::JmpIf { .. }
            | Inst::JmpKnown { .. }
            | Inst::JmpTableSeq { .. }
            | Inst::JmpUnknown { .. }
            | Inst::LoadEffectiveAddress { .. }
            | Inst::LoadExtName { .. }
            | Inst::LockCmpxchg { .. }
            | Inst::Mov64MR { .. }
            | Inst::MovRM { .. }
            | Inst::MovRR { .. }
            | Inst::MovsxRmR { .. }
            | Inst::MovzxRmR { .. }
            | Inst::MulHi { .. }
            | Inst::Neg { .. }
            | Inst::Not { .. }
            | Inst::Nop { .. }
            | Inst::Pop64 { .. }
            | Inst::Push64 { .. }
            | Inst::Ret
            | Inst::Setcc { .. }
            | Inst::ShiftR { .. }
            | Inst::SignExtendData { .. }
            | Inst::TrapIf { .. }
            | Inst::Ud2 { .. }
            | Inst::UnaryRmR { .. }
            | Inst::VirtualSPOffsetAdj { .. }
            | Inst::XmmCmove { .. }
            | Inst::XmmCmpRmR { .. }
            | Inst::XmmLoadConstSeq { .. }
            | Inst::XmmMinMaxSeq { .. }
            | Inst::XmmUninitializedValue { .. } => None,

            // These use dynamic SSE opcodes.
            Inst::GprToXmm { op, .. }
            | Inst::XmmMovRM { op, .. }
            | Inst::XmmRmiReg { opcode: op, .. }
            | Inst::XmmRmR { op, .. }
            | Inst::XmmRmRImm { op, .. }
            | Inst::XmmToGpr { op, .. }
            | Inst::XmmUnaryRmR { op, .. } => Some(op.available_from()),
        }
    }
}

// Handy constructors for Insts.

impl Inst {
    pub(crate) fn nop(len: u8) -> Self {
        debug_assert!(len <= 16);
        Self::Nop { len }
    }

    pub(crate) fn alu_rmi_r(
        is_64: bool,
        op: AluRmiROpcode,
        src: RegMemImm,
        dst: Writable<Reg>,
    ) -> Self {
        src.assert_regclass_is(RegClass::I64);
        debug_assert!(dst.to_reg().get_class() == RegClass::I64);
        Self::AluRmiR {
            is_64,
            op,
            src,
            dst,
        }
    }

    pub(crate) fn unary_rm_r(
        size: u8,
        op: UnaryRmROpcode,
        src: RegMem,
        dst: Writable<Reg>,
    ) -> Self {
        src.assert_regclass_is(RegClass::I64);
        debug_assert!(dst.to_reg().get_class() == RegClass::I64);
        debug_assert!(size == 8 || size == 4 || size == 2);
        Self::UnaryRmR { size, op, src, dst }
    }

    pub(crate) fn not(size: u8, src: Writable<Reg>) -> Inst {
        debug_assert_eq!(src.to_reg().get_class(), RegClass::I64);
        debug_assert!(size == 8 || size == 4 || size == 2 || size == 1);
        Inst::Not { size, src }
    }

    pub(crate) fn neg(size: u8, src: Writable<Reg>) -> Inst {
        debug_assert_eq!(src.to_reg().get_class(), RegClass::I64);
        debug_assert!(size == 8 || size == 4 || size == 2 || size == 1);
        Inst::Neg { size, src }
    }

    pub(crate) fn div(size: u8, signed: bool, divisor: RegMem, loc: SourceLoc) -> Inst {
        divisor.assert_regclass_is(RegClass::I64);
        debug_assert!(size == 8 || size == 4 || size == 2 || size == 1);
        Inst::Div {
            size,
            signed,
            divisor,
            loc,
        }
    }

    pub(crate) fn mul_hi(size: u8, signed: bool, rhs: RegMem) -> Inst {
        rhs.assert_regclass_is(RegClass::I64);
        debug_assert!(size == 8 || size == 4 || size == 2 || size == 1);
        Inst::MulHi { size, signed, rhs }
    }

    pub(crate) fn checked_div_or_rem_seq(
        kind: DivOrRemKind,
        size: u8,
        divisor: Writable<Reg>,
        tmp: Option<Writable<Reg>>,
        loc: SourceLoc,
    ) -> Inst {
        debug_assert!(size == 8 || size == 4 || size == 2 || size == 1);
        debug_assert!(divisor.to_reg().get_class() == RegClass::I64);
        debug_assert!(tmp
            .map(|tmp| tmp.to_reg().get_class() == RegClass::I64)
            .unwrap_or(true));
        Inst::CheckedDivOrRemSeq {
            kind,
            size,
            divisor,
            tmp,
            loc,
        }
    }

    pub(crate) fn sign_extend_data(size: u8) -> Inst {
        debug_assert!(size == 8 || size == 4 || size == 2 || size == 1);
        Inst::SignExtendData { size }
    }

    pub(crate) fn imm(size: OperandSize, simm64: u64, dst: Writable<Reg>) -> Inst {
        debug_assert!(dst.to_reg().get_class() == RegClass::I64);
        // Try to generate a 32-bit immediate when the upper high bits are zeroed (which matches
        // the semantics of movl).
        let dst_is_64 = size == OperandSize::Size64 && simm64 > u32::max_value() as u64;
        Inst::Imm {
            dst_is_64,
            simm64,
            dst,
        }
    }

    pub(crate) fn mov_r_r(is_64: bool, src: Reg, dst: Writable<Reg>) -> Inst {
        debug_assert!(src.get_class() == RegClass::I64);
        debug_assert!(dst.to_reg().get_class() == RegClass::I64);
        Inst::MovRR { is_64, src, dst }
    }

    // TODO Can be replaced by `Inst::move` (high-level) and `Inst::unary_rm_r` (low-level)
    pub(crate) fn xmm_mov(
        op: SseOpcode,
        src: RegMem,
        dst: Writable<Reg>,
        srcloc: Option<SourceLoc>,
    ) -> Inst {
        src.assert_regclass_is(RegClass::V128);
        debug_assert!(dst.to_reg().get_class() == RegClass::V128);
        Inst::XmmUnaryRmR {
            op,
            src,
            dst,
            srcloc,
        }
    }

    pub(crate) fn xmm_load_const_seq(val: Vec<u8>, dst: Writable<Reg>, ty: Type) -> Inst {
        debug_assert!(val.len() == 16);
        debug_assert!(dst.to_reg().get_class() == RegClass::V128);
        debug_assert!(ty.is_vector() && ty.bits() == 128);
        Inst::XmmLoadConstSeq { val, dst, ty }
    }

    /// Convenient helper for unary float operations.
    pub(crate) fn xmm_unary_rm_r(op: SseOpcode, src: RegMem, dst: Writable<Reg>) -> Inst {
        src.assert_regclass_is(RegClass::V128);
        debug_assert!(dst.to_reg().get_class() == RegClass::V128);
        Inst::XmmUnaryRmR {
            op,
            src,
            dst,
            srcloc: None,
        }
    }

    pub(crate) fn xmm_rm_r(op: SseOpcode, src: RegMem, dst: Writable<Reg>) -> Self {
        src.assert_regclass_is(RegClass::V128);
        debug_assert!(dst.to_reg().get_class() == RegClass::V128);
        Inst::XmmRmR { op, src, dst }
    }

    pub(crate) fn xmm_uninit_value(dst: Writable<Reg>) -> Self {
        debug_assert!(dst.to_reg().get_class() == RegClass::V128);
        Inst::XmmUninitializedValue { dst }
    }

    pub(crate) fn xmm_mov_r_m(
        op: SseOpcode,
        src: Reg,
        dst: impl Into<SyntheticAmode>,
        srcloc: Option<SourceLoc>,
    ) -> Inst {
        debug_assert!(src.get_class() == RegClass::V128);
        Inst::XmmMovRM {
            op,
            src,
            dst: dst.into(),
            srcloc,
        }
    }

    pub(crate) fn xmm_to_gpr(
        op: SseOpcode,
        src: Reg,
        dst: Writable<Reg>,
        dst_size: OperandSize,
    ) -> Inst {
        debug_assert!(src.get_class() == RegClass::V128);
        debug_assert!(dst.to_reg().get_class() == RegClass::I64);
        Inst::XmmToGpr {
            op,
            src,
            dst,
            dst_size,
        }
    }

    pub(crate) fn gpr_to_xmm(
        op: SseOpcode,
        src: RegMem,
        src_size: OperandSize,
        dst: Writable<Reg>,
    ) -> Inst {
        src.assert_regclass_is(RegClass::I64);
        debug_assert!(dst.to_reg().get_class() == RegClass::V128);
        Inst::GprToXmm {
            op,
            src,
            dst,
            src_size,
        }
    }

    pub(crate) fn xmm_cmp_rm_r(op: SseOpcode, src: RegMem, dst: Reg) -> Inst {
        src.assert_regclass_is(RegClass::V128);
        debug_assert!(dst.get_class() == RegClass::V128);
        Inst::XmmCmpRmR { op, src, dst }
    }

    pub(crate) fn cvt_u64_to_float_seq(
        to_f64: bool,
        src: Writable<Reg>,
        tmp_gpr1: Writable<Reg>,
        tmp_gpr2: Writable<Reg>,
        dst: Writable<Reg>,
    ) -> Inst {
        debug_assert!(src.to_reg().get_class() == RegClass::I64);
        debug_assert!(tmp_gpr1.to_reg().get_class() == RegClass::I64);
        debug_assert!(tmp_gpr2.to_reg().get_class() == RegClass::I64);
        debug_assert!(dst.to_reg().get_class() == RegClass::V128);
        Inst::CvtUint64ToFloatSeq {
            src,
            dst,
            tmp_gpr1,
            tmp_gpr2,
            to_f64,
        }
    }

    pub(crate) fn cvt_float_to_sint_seq(
        src_size: OperandSize,
        dst_size: OperandSize,
        is_saturating: bool,
        src: Writable<Reg>,
        dst: Writable<Reg>,
        tmp_gpr: Writable<Reg>,
        tmp_xmm: Writable<Reg>,
        srcloc: SourceLoc,
    ) -> Inst {
        debug_assert!(src.to_reg().get_class() == RegClass::V128);
        debug_assert!(tmp_xmm.to_reg().get_class() == RegClass::V128);
        debug_assert!(tmp_gpr.to_reg().get_class() == RegClass::I64);
        debug_assert!(dst.to_reg().get_class() == RegClass::I64);
        Inst::CvtFloatToSintSeq {
            src_size,
            dst_size,
            is_saturating,
            src,
            dst,
            tmp_gpr,
            tmp_xmm,
            srcloc,
        }
    }

    pub(crate) fn cvt_float_to_uint_seq(
        src_size: OperandSize,
        dst_size: OperandSize,
        is_saturating: bool,
        src: Writable<Reg>,
        dst: Writable<Reg>,
        tmp_gpr: Writable<Reg>,
        tmp_xmm: Writable<Reg>,
        srcloc: SourceLoc,
    ) -> Inst {
        debug_assert!(src.to_reg().get_class() == RegClass::V128);
        debug_assert!(tmp_xmm.to_reg().get_class() == RegClass::V128);
        debug_assert!(tmp_gpr.to_reg().get_class() == RegClass::I64);
        debug_assert!(dst.to_reg().get_class() == RegClass::I64);
        Inst::CvtFloatToUintSeq {
            src_size,
            dst_size,
            is_saturating,
            src,
            dst,
            tmp_gpr,
            tmp_xmm,
            srcloc,
        }
    }

    pub(crate) fn xmm_min_max_seq(
        size: OperandSize,
        is_min: bool,
        lhs: Reg,
        rhs_dst: Writable<Reg>,
    ) -> Inst {
        debug_assert_eq!(lhs.get_class(), RegClass::V128);
        debug_assert_eq!(rhs_dst.to_reg().get_class(), RegClass::V128);
        Inst::XmmMinMaxSeq {
            size,
            is_min,
            lhs,
            rhs_dst,
        }
    }

    pub(crate) fn xmm_rm_r_imm(
        op: SseOpcode,
        src: RegMem,
        dst: Writable<Reg>,
        imm: u8,
        is64: bool,
    ) -> Inst {
        Inst::XmmRmRImm {
            op,
            src,
            dst,
            imm,
            is64,
        }
    }

    pub(crate) fn movzx_rm_r(
        ext_mode: ExtMode,
        src: RegMem,
        dst: Writable<Reg>,
        srcloc: Option<SourceLoc>,
    ) -> Inst {
        src.assert_regclass_is(RegClass::I64);
        debug_assert!(dst.to_reg().get_class() == RegClass::I64);
        Inst::MovzxRmR {
            ext_mode,
            src,
            dst,
            srcloc,
        }
    }

    pub(crate) fn xmm_rmi_reg(opcode: SseOpcode, src: RegMemImm, dst: Writable<Reg>) -> Inst {
        src.assert_regclass_is(RegClass::V128);
        debug_assert!(dst.to_reg().get_class() == RegClass::V128);
        Inst::XmmRmiReg { opcode, src, dst }
    }

    pub(crate) fn movsx_rm_r(
        ext_mode: ExtMode,
        src: RegMem,
        dst: Writable<Reg>,
        srcloc: Option<SourceLoc>,
    ) -> Inst {
        src.assert_regclass_is(RegClass::I64);
        debug_assert!(dst.to_reg().get_class() == RegClass::I64);
        Inst::MovsxRmR {
            ext_mode,
            src,
            dst,
            srcloc,
        }
    }

    pub(crate) fn mov64_m_r(
        src: impl Into<SyntheticAmode>,
        dst: Writable<Reg>,
        srcloc: Option<SourceLoc>,
    ) -> Inst {
        debug_assert!(dst.to_reg().get_class() == RegClass::I64);
        Inst::Mov64MR {
            src: src.into(),
            dst,
            srcloc,
        }
    }

    /// A convenience function to be able to use a RegMem as the source of a move.
    pub(crate) fn mov64_rm_r(src: RegMem, dst: Writable<Reg>, srcloc: Option<SourceLoc>) -> Inst {
        src.assert_regclass_is(RegClass::I64);
        match src {
            RegMem::Reg { reg } => Self::mov_r_r(true, reg, dst),
            RegMem::Mem { addr } => Self::mov64_m_r(addr, dst, srcloc),
        }
    }

    pub(crate) fn mov_r_m(
        size: u8, // 1, 2, 4 or 8
        src: Reg,
        dst: impl Into<SyntheticAmode>,
        srcloc: Option<SourceLoc>,
    ) -> Inst {
        debug_assert!(size == 8 || size == 4 || size == 2 || size == 1);
        debug_assert!(src.get_class() == RegClass::I64);
        Inst::MovRM {
            size,
            src,
            dst: dst.into(),
            srcloc,
        }
    }

    pub(crate) fn lea(addr: impl Into<SyntheticAmode>, dst: Writable<Reg>) -> Inst {
        debug_assert!(dst.to_reg().get_class() == RegClass::I64);
        Inst::LoadEffectiveAddress {
            addr: addr.into(),
            dst,
        }
    }

    pub(crate) fn shift_r(
        size: u8,
        kind: ShiftKind,
        num_bits: Option<u8>,
        dst: Writable<Reg>,
    ) -> Inst {
        debug_assert!(size == 8 || size == 4 || size == 2 || size == 1);
        debug_assert!(if let Some(num_bits) = num_bits {
            num_bits < size * 8
        } else {
            true
        });
        debug_assert!(dst.to_reg().get_class() == RegClass::I64);
        Inst::ShiftR {
            size,
            kind,
            num_bits,
            dst,
        }
    }

    /// Does a comparison of dst - src for operands of size `size`, as stated by the machine
    /// instruction semantics. Be careful with the order of parameters!
    pub(crate) fn cmp_rmi_r(
        size: u8, // 1, 2, 4 or 8
        src: RegMemImm,
        dst: Reg,
    ) -> Inst {
        src.assert_regclass_is(RegClass::I64);
        debug_assert!(size == 8 || size == 4 || size == 2 || size == 1);
        debug_assert!(dst.get_class() == RegClass::I64);
        Inst::CmpRmiR { size, src, dst }
    }

    pub(crate) fn trap(srcloc: SourceLoc, trap_code: TrapCode) -> Inst {
        Inst::Ud2 {
            trap_info: (srcloc, trap_code),
        }
    }

    pub(crate) fn setcc(cc: CC, dst: Writable<Reg>) -> Inst {
        debug_assert!(dst.to_reg().get_class() == RegClass::I64);
        Inst::Setcc { cc, dst }
    }

    pub(crate) fn cmove(size: u8, cc: CC, src: RegMem, dst: Writable<Reg>) -> Inst {
        debug_assert!(size == 8 || size == 4 || size == 2);
        debug_assert!(dst.to_reg().get_class() == RegClass::I64);
        Inst::Cmove { size, cc, src, dst }
    }

    pub(crate) fn xmm_cmove(is_64: bool, cc: CC, src: RegMem, dst: Writable<Reg>) -> Inst {
        src.assert_regclass_is(RegClass::V128);
        debug_assert!(dst.to_reg().get_class() == RegClass::V128);
        Inst::XmmCmove {
            is_64,
            cc,
            src,
            dst,
        }
    }

    pub(crate) fn push64(src: RegMemImm) -> Inst {
        src.assert_regclass_is(RegClass::I64);
        Inst::Push64 { src }
    }

    pub(crate) fn pop64(dst: Writable<Reg>) -> Inst {
        debug_assert!(dst.to_reg().get_class() == RegClass::I64);
        Inst::Pop64 { dst }
    }

    pub(crate) fn call_known(
        dest: ExternalName,
        uses: Vec<Reg>,
        defs: Vec<Writable<Reg>>,
        loc: SourceLoc,
        opcode: Opcode,
    ) -> Inst {
        Inst::CallKnown {
            dest,
            uses,
            defs,
            loc,
            opcode,
        }
    }

    pub(crate) fn call_unknown(
        dest: RegMem,
        uses: Vec<Reg>,
        defs: Vec<Writable<Reg>>,
        loc: SourceLoc,
        opcode: Opcode,
    ) -> Inst {
        dest.assert_regclass_is(RegClass::I64);
        Inst::CallUnknown {
            dest,
            uses,
            defs,
            loc,
            opcode,
        }
    }

    pub(crate) fn ret() -> Inst {
        Inst::Ret
    }

    pub(crate) fn epilogue_placeholder() -> Inst {
        Inst::EpiloguePlaceholder
    }

    pub(crate) fn jmp_known(dst: MachLabel) -> Inst {
        Inst::JmpKnown { dst }
    }

    pub(crate) fn jmp_if(cc: CC, taken: MachLabel) -> Inst {
        Inst::JmpIf { cc, taken }
    }

    pub(crate) fn jmp_cond(cc: CC, taken: MachLabel, not_taken: MachLabel) -> Inst {
        Inst::JmpCond {
            cc,
            taken,
            not_taken,
        }
    }

    pub(crate) fn jmp_unknown(target: RegMem) -> Inst {
        target.assert_regclass_is(RegClass::I64);
        Inst::JmpUnknown { target }
    }

    pub(crate) fn trap_if(cc: CC, trap_code: TrapCode, srcloc: SourceLoc) -> Inst {
        Inst::TrapIf {
            cc,
            trap_code,
            srcloc,
        }
    }

    /// Choose which instruction to use for loading a register value from memory. For loads smaller
    /// than 64 bits, this method expects a way to extend the value (i.e. [ExtKind::SignExtend],
    /// [ExtKind::ZeroExtend]); loads with no extension necessary will ignore this.
    pub(crate) fn load(
        ty: Type,
        from_addr: impl Into<SyntheticAmode>,
        to_reg: Writable<Reg>,
        ext_kind: ExtKind,
        srcloc: Option<SourceLoc>,
    ) -> Inst {
        let rc = to_reg.to_reg().get_class();
        match rc {
            RegClass::I64 => {
                let ext_mode = match ty.bytes() {
                    1 => Some(ExtMode::BQ),
                    2 => Some(ExtMode::WQ),
                    4 => Some(ExtMode::LQ),
                    8 => None,
                    _ => unreachable!("the type should never use a scalar load: {}", ty),
                };
                if let Some(ext_mode) = ext_mode {
                    // Values smaller than 64 bits must be extended in some way.
                    match ext_kind {
                        ExtKind::SignExtend => {
                            Inst::movsx_rm_r(ext_mode, RegMem::mem(from_addr), to_reg, srcloc)
                        }
                        ExtKind::ZeroExtend => {
                            Inst::movzx_rm_r(ext_mode, RegMem::mem(from_addr), to_reg, srcloc)
                        }
                        ExtKind::None => panic!(
                            "expected an extension kind for extension mode: {:?}",
                            ext_mode
                        ),
                    }
                } else {
                    // 64-bit values can be moved directly.
                    Inst::mov64_m_r(from_addr, to_reg, srcloc)
                }
            }
            RegClass::V128 => {
                let opcode = match ty {
                    types::F32 => SseOpcode::Movss,
                    types::F64 => SseOpcode::Movsd,
                    types::F32X4 => SseOpcode::Movups,
                    types::F64X2 => SseOpcode::Movupd,
                    _ if ty.is_vector() && ty.bits() == 128 => SseOpcode::Movdqu,
                    _ => unimplemented!("unable to load type: {}", ty),
                };
                Inst::xmm_unary_rm_r(opcode, RegMem::mem(from_addr), to_reg)
            }
            _ => panic!("unable to generate load for register class: {:?}", rc),
        }
    }

    /// Choose which instruction to use for storing a register value to memory.
    pub(crate) fn store(
        ty: Type,
        from_reg: Reg,
        to_addr: impl Into<SyntheticAmode>,
        srcloc: Option<SourceLoc>,
    ) -> Inst {
        let rc = from_reg.get_class();
        match rc {
            RegClass::I64 => {
                // Always store the full register, to ensure that the high bits are properly set
                // when doing a full reload.
                Inst::mov_r_m(8 /* bytes */, from_reg, to_addr, srcloc)
            }
            RegClass::V128 => {
                let opcode = match ty {
                    types::F32 => SseOpcode::Movss,
                    types::F64 => SseOpcode::Movsd,
                    types::F32X4 => SseOpcode::Movups,
                    types::F64X2 => SseOpcode::Movupd,
                    _ if ty.is_vector() && ty.bits() == 128 => SseOpcode::Movdqu,
                    _ => unimplemented!("unable to store type: {}", ty),
                };
                Inst::xmm_mov_r_m(opcode, from_reg, to_addr, srcloc)
            }
            _ => panic!("unable to generate store for register class: {:?}", rc),
        }
    }
}

// Inst helpers.

impl Inst {
    /// In certain cases, instructions of this format can act as a definition of an XMM register,
    /// producing a value that is independent of its initial value.
    ///
    /// For example, a vector equality comparison (`cmppd` or `cmpps`) that compares a register to
    /// itself will generate all ones as a result, regardless of its value. From the register
    /// allocator's point of view, we should (i) record the first register, which is normally a
    /// mod, as a def instead; and (ii) not record the second register as a use, because it is the
    /// same as the first register (already handled).
    fn produces_const(&self) -> bool {
        match self {
            Self::AluRmiR { op, src, dst, .. } => {
                src.to_reg() == Some(dst.to_reg())
                    && (*op == AluRmiROpcode::Xor || *op == AluRmiROpcode::Sub)
            }

            Self::XmmRmR { op, src, dst, .. } => {
                src.to_reg() == Some(dst.to_reg())
                    && (*op == SseOpcode::Xorps
                        || *op == SseOpcode::Xorpd
                        || *op == SseOpcode::Pxor
                        || *op == SseOpcode::Pcmpeqb
                        || *op == SseOpcode::Pcmpeqw
                        || *op == SseOpcode::Pcmpeqd
                        || *op == SseOpcode::Pcmpeqq)
            }

            Self::XmmRmRImm {
                op, src, dst, imm, ..
            } => {
                src.to_reg() == Some(dst.to_reg())
                    && (*op == SseOpcode::Cmppd || *op == SseOpcode::Cmpps)
                    && *imm == FcmpImm::Equal.encode()
            }

            _ => false,
        }
    }
}

//=============================================================================
// Instructions: printing

impl PrettyPrint for Inst {
    fn show_rru(&self, mb_rru: Option<&RealRegUniverse>) -> String {
        fn ljustify(s: String) -> String {
            let w = 7;
            if s.len() >= w {
                s
            } else {
                let need = usize::min(w, w - s.len());
                s + &format!("{nil: <width$}", nil = "", width = need)
            }
        }

        fn ljustify2(s1: String, s2: String) -> String {
            ljustify(s1 + &s2)
        }

        fn suffix_lq(is_64: bool) -> String {
            (if is_64 { "q" } else { "l" }).to_string()
        }

        fn size_lq(is_64: bool) -> u8 {
            if is_64 {
                8
            } else {
                4
            }
        }

        fn suffix_bwlq(size: u8) -> String {
            match size {
                1 => "b".to_string(),
                2 => "w".to_string(),
                4 => "l".to_string(),
                8 => "q".to_string(),
                _ => panic!("Inst(x64).show.suffixBWLQ: size={}", size),
            }
        }

        match self {
            Inst::Nop { len } => format!("{} len={}", ljustify("nop".to_string()), len),

            Inst::AluRmiR {
                is_64,
                op,
                src,
                dst,
            } => format!(
                "{} {}, {}",
                ljustify2(op.to_string(), suffix_lq(*is_64)),
                src.show_rru_sized(mb_rru, size_lq(*is_64)),
                show_ireg_sized(dst.to_reg(), mb_rru, size_lq(*is_64)),
            ),

            Inst::UnaryRmR { src, dst, op, size } => format!(
                "{} {}, {}",
                ljustify2(op.to_string(), suffix_bwlq(*size)),
                src.show_rru_sized(mb_rru, *size),
                show_ireg_sized(dst.to_reg(), mb_rru, *size),
            ),

            Inst::Not { size, src } => format!(
                "{} {}",
                ljustify2("not".to_string(), suffix_bwlq(*size)),
                show_ireg_sized(src.to_reg(), mb_rru, *size)
            ),

            Inst::Neg { size, src } => format!(
                "{} {}",
                ljustify2("neg".to_string(), suffix_bwlq(*size)),
                show_ireg_sized(src.to_reg(), mb_rru, *size)
            ),

            Inst::Div {
                size,
                signed,
                divisor,
                ..
            } => format!(
                "{} {}",
                ljustify(if *signed {
                    "idiv".to_string()
                } else {
                    "div".into()
                }),
                divisor.show_rru_sized(mb_rru, *size)
            ),

            Inst::MulHi {
                size, signed, rhs, ..
            } => format!(
                "{} {}",
                ljustify(if *signed {
                    "imul".to_string()
                } else {
                    "mul".to_string()
                }),
                rhs.show_rru_sized(mb_rru, *size)
            ),

            Inst::CheckedDivOrRemSeq {
                kind,
                size,
                divisor,
                ..
            } => format!(
                "{} $rax:$rdx, {}",
                match kind {
                    DivOrRemKind::SignedDiv => "sdiv",
                    DivOrRemKind::UnsignedDiv => "udiv",
                    DivOrRemKind::SignedRem => "srem",
                    DivOrRemKind::UnsignedRem => "urem",
                },
                show_ireg_sized(divisor.to_reg(), mb_rru, *size),
            ),

            Inst::SignExtendData { size } => match size {
                1 => "cbw",
                2 => "cwd",
                4 => "cdq",
                8 => "cqo",
                _ => unreachable!(),
            }
            .into(),

            Inst::XmmUnaryRmR { op, src, dst, .. } => format!(
                "{} {}, {}",
                ljustify(op.to_string()),
                src.show_rru_sized(mb_rru, op.src_size()),
                show_ireg_sized(dst.to_reg(), mb_rru, 8),
            ),

            Inst::XmmMovRM { op, src, dst, .. } => format!(
                "{} {}, {}",
                ljustify(op.to_string()),
                show_ireg_sized(*src, mb_rru, 8),
                dst.show_rru(mb_rru),
            ),

            Inst::XmmRmR { op, src, dst } => format!(
                "{} {}, {}",
                ljustify(op.to_string()),
                src.show_rru_sized(mb_rru, 8),
                show_ireg_sized(dst.to_reg(), mb_rru, 8),
            ),

            Inst::XmmMinMaxSeq {
                lhs,
                rhs_dst,
                is_min,
                size,
            } => format!(
                "{} {}, {}",
                ljustify2(
                    if *is_min {
                        "xmm min seq ".to_string()
                    } else {
                        "xmm max seq ".to_string()
                    },
                    match size {
                        OperandSize::Size32 => "f32",
                        OperandSize::Size64 => "f64",
                    }
                    .into()
                ),
                show_ireg_sized(*lhs, mb_rru, 8),
                show_ireg_sized(rhs_dst.to_reg(), mb_rru, 8),
            ),

            Inst::XmmRmRImm { op, src, dst, imm, is64 } => format!(
                "{} ${}, {}, {}",
                ljustify(format!("{}{}", op.to_string(), if *is64 { ".w" } else { "" })),
                imm,
                src.show_rru(mb_rru),
                dst.show_rru(mb_rru),
            ),

            Inst::XmmUninitializedValue { dst } => format!(
                "{} {}",
                ljustify("uninit".into()),
                dst.show_rru(mb_rru),
            ),

            Inst::XmmLoadConstSeq { val, dst, .. } => {
                format!("load_const ${:?}, {}", val, dst.show_rru(mb_rru),)
            }

            Inst::XmmToGpr {
                op,
                src,
                dst,
                dst_size,
            } => {
                let dst_size = match dst_size {
                    OperandSize::Size32 => 4,
                    OperandSize::Size64 => 8,
                };
                format!(
                    "{} {}, {}",
                    ljustify(op.to_string()),
                    src.show_rru(mb_rru),
                    show_ireg_sized(dst.to_reg(), mb_rru, dst_size),
                )
            }

            Inst::GprToXmm {
                op,
                src,
                src_size,
                dst,
            } => format!(
                "{} {}, {}",
                ljustify(op.to_string()),
                src.show_rru_sized(mb_rru, src_size.to_bytes()),
                dst.show_rru(mb_rru)
            ),

            Inst::XmmCmpRmR { op, src, dst } => format!(
                "{} {}, {}",
                ljustify(op.to_string()),
                src.show_rru_sized(mb_rru, 8),
                show_ireg_sized(*dst, mb_rru, 8),
            ),

            Inst::CvtUint64ToFloatSeq {
                src, dst, to_f64, ..
            } => format!(
                "{} {}, {}",
                ljustify(format!(
                    "u64_to_{}_seq",
                    if *to_f64 { "f64" } else { "f32" }
                )),
                show_ireg_sized(src.to_reg(), mb_rru, 8),
                dst.show_rru(mb_rru),
            ),

            Inst::CvtFloatToSintSeq {
                src,
                dst,
                src_size,
                dst_size,
                ..
            } => format!(
                "{} {}, {}",
                ljustify(format!(
                    "cvt_float{}_to_sint{}_seq",
                    if *src_size == OperandSize::Size64 {
                        "64"
                    } else {
                        "32"
                    },
                    if *dst_size == OperandSize::Size64 {
                        "64"
                    } else {
                        "32"
                    }
                )),
                show_ireg_sized(src.to_reg(), mb_rru, 8),
                show_ireg_sized(dst.to_reg(), mb_rru, dst_size.to_bytes()),
            ),

            Inst::CvtFloatToUintSeq {
                src,
                dst,
                src_size,
                dst_size,
                ..
            } => format!(
                "{} {}, {}",
                ljustify(format!(
                    "cvt_float{}_to_uint{}_seq",
                    if *src_size == OperandSize::Size64 {
                        "64"
                    } else {
                        "32"
                    },
                    if *dst_size == OperandSize::Size64 {
                        "64"
                    } else {
                        "32"
                    }
                )),
                show_ireg_sized(src.to_reg(), mb_rru, 8),
                show_ireg_sized(dst.to_reg(), mb_rru, dst_size.to_bytes()),
            ),

            Inst::Imm {
                dst_is_64,
                simm64,
                dst,
            } => {
                if *dst_is_64 {
                    format!(
                        "{} ${}, {}",
                        ljustify("movabsq".to_string()),
                        *simm64 as i64,
                        show_ireg_sized(dst.to_reg(), mb_rru, 8)
                    )
                } else {
                    format!(
                        "{} ${}, {}",
                        ljustify("movl".to_string()),
                        (*simm64 as u32) as i32,
                        show_ireg_sized(dst.to_reg(), mb_rru, 4)
                    )
                }
            }

            Inst::MovRR { is_64, src, dst } => format!(
                "{} {}, {}",
                ljustify2("mov".to_string(), suffix_lq(*is_64)),
                show_ireg_sized(*src, mb_rru, size_lq(*is_64)),
                show_ireg_sized(dst.to_reg(), mb_rru, size_lq(*is_64))
            ),

            Inst::MovzxRmR {
                ext_mode, src, dst, ..
            } => {
                if *ext_mode == ExtMode::LQ {
                    format!(
                        "{} {}, {}",
                        ljustify("movl".to_string()),
                        src.show_rru_sized(mb_rru, ext_mode.src_size()),
                        show_ireg_sized(dst.to_reg(), mb_rru, 4)
                    )
                } else {
                    format!(
                        "{} {}, {}",
                        ljustify2("movz".to_string(), ext_mode.to_string()),
                        src.show_rru_sized(mb_rru, ext_mode.src_size()),
                        show_ireg_sized(dst.to_reg(), mb_rru, ext_mode.dst_size())
                    )
                }
            }

            Inst::Mov64MR { src, dst, .. } => format!(
                "{} {}, {}",
                ljustify("movq".to_string()),
                src.show_rru(mb_rru),
                dst.show_rru(mb_rru)
            ),

            Inst::LoadEffectiveAddress { addr, dst } => format!(
                "{} {}, {}",
                ljustify("lea".to_string()),
                addr.show_rru(mb_rru),
                dst.show_rru(mb_rru)
            ),

            Inst::MovsxRmR {
                ext_mode, src, dst, ..
            } => format!(
                "{} {}, {}",
                ljustify2("movs".to_string(), ext_mode.to_string()),
                src.show_rru_sized(mb_rru, ext_mode.src_size()),
                show_ireg_sized(dst.to_reg(), mb_rru, ext_mode.dst_size())
            ),

            Inst::MovRM { size, src, dst, .. } => format!(
                "{} {}, {}",
                ljustify2("mov".to_string(), suffix_bwlq(*size)),
                show_ireg_sized(*src, mb_rru, *size),
                dst.show_rru(mb_rru)
            ),

            Inst::ShiftR {
                size,
                kind,
                num_bits,
                dst,
            } => match num_bits {
                None => format!(
                    "{} %cl, {}",
                    ljustify2(kind.to_string(), suffix_bwlq(*size)),
                    show_ireg_sized(dst.to_reg(), mb_rru, *size)
                ),

                Some(num_bits) => format!(
                    "{} ${}, {}",
                    ljustify2(kind.to_string(), suffix_bwlq(*size)),
                    num_bits,
                    show_ireg_sized(dst.to_reg(), mb_rru, *size)
                ),
            },

            Inst::XmmRmiReg { opcode, src, dst } => format!(
                "{} {}, {}",
                ljustify(opcode.to_string()),
                src.show_rru(mb_rru),
                dst.to_reg().show_rru(mb_rru)
            ),

            Inst::CmpRmiR { size, src, dst } => format!(
                "{} {}, {}",
                ljustify2("cmp".to_string(), suffix_bwlq(*size)),
                src.show_rru_sized(mb_rru, *size),
                show_ireg_sized(*dst, mb_rru, *size)
            ),

            Inst::Setcc { cc, dst } => format!(
                "{} {}",
                ljustify2("set".to_string(), cc.to_string()),
                show_ireg_sized(dst.to_reg(), mb_rru, 1)
            ),

            Inst::Cmove { size, cc, src, dst } => format!(
                "{} {}, {}",
                ljustify(format!("cmov{}{}", cc.to_string(), suffix_bwlq(*size))),
                src.show_rru_sized(mb_rru, *size),
                show_ireg_sized(dst.to_reg(), mb_rru, *size)
            ),

            Inst::XmmCmove {
                is_64,
                cc,
                src,
                dst,
            } => {
                let size = if *is_64 { 8 } else { 4 };
                format!(
                    "j{} $next; mov{} {}, {}; $next: ",
                    cc.invert().to_string(),
                    if *is_64 { "sd" } else { "ss" },
                    src.show_rru_sized(mb_rru, size),
                    show_ireg_sized(dst.to_reg(), mb_rru, size)
                )
            }

            Inst::Push64 { src } => {
                format!("{} {}", ljustify("pushq".to_string()), src.show_rru(mb_rru))
            }

            Inst::Pop64 { dst } => {
                format!("{} {}", ljustify("popq".to_string()), dst.show_rru(mb_rru))
            }

            Inst::CallKnown { dest, .. } => format!("{} {:?}", ljustify("call".to_string()), dest),

            Inst::CallUnknown { dest, .. } => format!(
                "{} *{}",
                ljustify("call".to_string()),
                dest.show_rru(mb_rru)
            ),

            Inst::Ret => "ret".to_string(),

            Inst::EpiloguePlaceholder => "epilogue placeholder".to_string(),

            Inst::JmpKnown { dst } => {
                format!("{} {}", ljustify("jmp".to_string()), dst.to_string())
            }

            Inst::JmpIf { cc, taken } => format!(
                "{} {}",
                ljustify2("j".to_string(), cc.to_string()),
                taken.to_string(),
            ),

            Inst::JmpCond {
                cc,
                taken,
                not_taken,
            } => format!(
                "{} {}; j {}",
                ljustify2("j".to_string(), cc.to_string()),
                taken.to_string(),
                not_taken.to_string()
            ),

            Inst::JmpTableSeq { idx, .. } => {
                format!("{} {}", ljustify("br_table".into()), idx.show_rru(mb_rru))
            }

            Inst::JmpUnknown { target } => format!(
                "{} *{}",
                ljustify("jmp".to_string()),
                target.show_rru(mb_rru)
            ),

            Inst::TrapIf { cc, trap_code, .. } => {
                format!("j{} ; ud2 {} ;", cc.invert().to_string(), trap_code)
            }

            Inst::LoadExtName {
                dst, name, offset, ..
            } => format!(
                "{} {}+{}, {}",
                ljustify("movaps".into()),
                name,
                offset,
                show_ireg_sized(dst.to_reg(), mb_rru, 8),
            ),

            Inst::LockCmpxchg { ty, src, dst, .. } => {
                let size = ty.bytes() as u8;
                format!("lock cmpxchg{} {}, {}",
                        suffix_bwlq(size), show_ireg_sized(*src, mb_rru, size), dst.show_rru(mb_rru))
            }

            Inst::AtomicRmwSeq { ty, op, .. } => {
                format!(
                    "atomically {{ {}_bits_at_[%r9]) {:?}= %r10; %rax = old_value_at_[%r9]; %r11, %rflags = trash }}",
                    ty.bits(), op)
            },

            Inst::Fence { kind } => {
                match kind {
                    FenceKind::MFence => "mfence".to_string(),
                    FenceKind::LFence => "lfence".to_string(),
                    FenceKind::SFence => "sfence".to_string(),
                }
            }

            Inst::VirtualSPOffsetAdj { offset } => format!("virtual_sp_offset_adjust {}", offset),

            Inst::Hlt => "hlt".into(),

            Inst::Ud2 { trap_info } => format!("ud2 {}", trap_info.1),
        }
    }
}

// Temp hook for legacy printing machinery
impl fmt::Debug for Inst {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        // Print the insn without a Universe :-(
        write!(fmt, "{}", self.show_rru(None))
    }
}

fn x64_get_regs(inst: &Inst, collector: &mut RegUsageCollector) {
    // This is a bit subtle. If some register is in the modified set, then it may not be in either
    // the use or def sets. However, enforcing that directly is somewhat difficult. Instead,
    // regalloc.rs will "fix" this for us by removing the the modified set from the use and def
    // sets.
    match inst {
        Inst::AluRmiR { src, dst, .. } => {
            if inst.produces_const() {
                // No need to account for src, since src == dst.
                collector.add_def(*dst);
            } else {
                src.get_regs_as_uses(collector);
                collector.add_mod(*dst);
            }
        }
        Inst::Not { src, .. } => {
            collector.add_mod(*src);
        }
        Inst::Neg { src, .. } => {
            collector.add_mod(*src);
        }
        Inst::Div { size, divisor, .. } => {
            collector.add_mod(Writable::from_reg(regs::rax()));
            if *size == 1 {
                collector.add_def(Writable::from_reg(regs::rdx()));
            } else {
                collector.add_mod(Writable::from_reg(regs::rdx()));
            }
            divisor.get_regs_as_uses(collector);
        }
        Inst::MulHi { rhs, .. } => {
            collector.add_mod(Writable::from_reg(regs::rax()));
            collector.add_def(Writable::from_reg(regs::rdx()));
            rhs.get_regs_as_uses(collector);
        }
        Inst::CheckedDivOrRemSeq { divisor, tmp, .. } => {
            // Mark both fixed registers as mods, to avoid an early clobber problem in codegen
            // (i.e. the temporary is allocated one of the fixed registers). This requires writing
            // the rdx register *before* the instruction, which is not too bad.
            collector.add_mod(Writable::from_reg(regs::rax()));
            collector.add_mod(Writable::from_reg(regs::rdx()));
            collector.add_mod(*divisor);
            if let Some(tmp) = tmp {
                collector.add_def(*tmp);
            }
        }
        Inst::SignExtendData { size } => match size {
            1 => collector.add_mod(Writable::from_reg(regs::rax())),
            2 | 4 | 8 => {
                collector.add_use(regs::rax());
                collector.add_def(Writable::from_reg(regs::rdx()));
            }
            _ => unreachable!(),
        },
        Inst::UnaryRmR { src, dst, .. } | Inst::XmmUnaryRmR { src, dst, .. } => {
            src.get_regs_as_uses(collector);
            collector.add_def(*dst);
        }
        Inst::XmmRmR { src, dst, .. } => {
            if inst.produces_const() {
                // No need to account for src, since src == dst.
                collector.add_def(*dst);
            } else {
                src.get_regs_as_uses(collector);
                collector.add_mod(*dst);
            }
        }
        Inst::XmmRmRImm { op, src, dst, .. } => {
            if inst.produces_const() {
                // No need to account for src, since src == dst.
                collector.add_def(*dst);
            } else if *op == SseOpcode::Pextrb
                || *op == SseOpcode::Pextrw
                || *op == SseOpcode::Pextrd
                || *op == SseOpcode::Pshufd
            {
                src.get_regs_as_uses(collector);
                collector.add_def(*dst);
            } else {
                src.get_regs_as_uses(collector);
                collector.add_mod(*dst);
            }
        }
        Inst::XmmUninitializedValue { dst } => collector.add_def(*dst),
        Inst::XmmLoadConstSeq { dst, .. } => collector.add_def(*dst),
        Inst::XmmMinMaxSeq { lhs, rhs_dst, .. } => {
            collector.add_use(*lhs);
            collector.add_mod(*rhs_dst);
        }
        Inst::XmmRmiReg { src, dst, .. } => {
            src.get_regs_as_uses(collector);
            collector.add_mod(*dst);
        }
        Inst::XmmMovRM { src, dst, .. } => {
            collector.add_use(*src);
            dst.get_regs_as_uses(collector);
        }
        Inst::XmmCmpRmR { src, dst, .. } => {
            src.get_regs_as_uses(collector);
            collector.add_use(*dst);
        }
        Inst::Imm { dst, .. } => {
            collector.add_def(*dst);
        }
        Inst::MovRR { src, dst, .. } | Inst::XmmToGpr { src, dst, .. } => {
            collector.add_use(*src);
            collector.add_def(*dst);
        }
        Inst::GprToXmm { src, dst, .. } => {
            src.get_regs_as_uses(collector);
            collector.add_def(*dst);
        }
        Inst::CvtUint64ToFloatSeq {
            src,
            dst,
            tmp_gpr1,
            tmp_gpr2,
            ..
        } => {
            collector.add_mod(*src);
            collector.add_def(*dst);
            collector.add_def(*tmp_gpr1);
            collector.add_def(*tmp_gpr2);
        }
        Inst::CvtFloatToSintSeq {
            src,
            dst,
            tmp_xmm,
            tmp_gpr,
            ..
        }
        | Inst::CvtFloatToUintSeq {
            src,
            dst,
            tmp_gpr,
            tmp_xmm,
            ..
        } => {
            collector.add_mod(*src);
            collector.add_def(*dst);
            collector.add_def(*tmp_gpr);
            collector.add_def(*tmp_xmm);
        }
        Inst::MovzxRmR { src, dst, .. } => {
            src.get_regs_as_uses(collector);
            collector.add_def(*dst);
        }
        Inst::Mov64MR { src, dst, .. } | Inst::LoadEffectiveAddress { addr: src, dst } => {
            src.get_regs_as_uses(collector);
            collector.add_def(*dst)
        }
        Inst::MovsxRmR { src, dst, .. } => {
            src.get_regs_as_uses(collector);
            collector.add_def(*dst);
        }
        Inst::MovRM { src, dst, .. } => {
            collector.add_use(*src);
            dst.get_regs_as_uses(collector);
        }
        Inst::ShiftR { num_bits, dst, .. } => {
            if num_bits.is_none() {
                collector.add_use(regs::rcx());
            }
            collector.add_mod(*dst);
        }
        Inst::CmpRmiR { src, dst, .. } => {
            src.get_regs_as_uses(collector);
            collector.add_use(*dst); // yes, really `add_use`
        }
        Inst::Setcc { dst, .. } => {
            collector.add_def(*dst);
        }
        Inst::Cmove { src, dst, .. } | Inst::XmmCmove { src, dst, .. } => {
            src.get_regs_as_uses(collector);
            collector.add_mod(*dst);
        }
        Inst::Push64 { src } => {
            src.get_regs_as_uses(collector);
            collector.add_mod(Writable::from_reg(regs::rsp()));
        }
        Inst::Pop64 { dst } => {
            collector.add_def(*dst);
        }

        Inst::CallKnown {
            ref uses, ref defs, ..
        } => {
            collector.add_uses(uses);
            collector.add_defs(defs);
        }

        Inst::CallUnknown {
            ref uses,
            ref defs,
            dest,
            ..
        } => {
            collector.add_uses(uses);
            collector.add_defs(defs);
            dest.get_regs_as_uses(collector);
        }

        Inst::JmpTableSeq {
            ref idx,
            ref tmp1,
            ref tmp2,
            ..
        } => {
            collector.add_use(*idx);
            collector.add_def(*tmp1);
            collector.add_def(*tmp2);
        }

        Inst::JmpUnknown { target } => {
            target.get_regs_as_uses(collector);
        }

        Inst::LoadExtName { dst, .. } => {
            collector.add_def(*dst);
        }

        Inst::LockCmpxchg { src, dst, .. } => {
            dst.get_regs_as_uses(collector);
            collector.add_use(*src);
            collector.add_mod(Writable::from_reg(regs::rax()));
        }

        Inst::AtomicRmwSeq { .. } => {
            collector.add_use(regs::r9());
            collector.add_use(regs::r10());
            collector.add_def(Writable::from_reg(regs::r11()));
            collector.add_def(Writable::from_reg(regs::rax()));
        }

        Inst::Ret
        | Inst::EpiloguePlaceholder
        | Inst::JmpKnown { .. }
        | Inst::JmpIf { .. }
        | Inst::JmpCond { .. }
        | Inst::Nop { .. }
        | Inst::TrapIf { .. }
        | Inst::VirtualSPOffsetAdj { .. }
        | Inst::Hlt
        | Inst::Ud2 { .. }
        | Inst::Fence { .. } => {
            // No registers are used.
        }
    }
}

//=============================================================================
// Instructions and subcomponents: map_regs

fn map_use<RUM: RegUsageMapper>(m: &RUM, r: &mut Reg) {
    if let Some(reg) = r.as_virtual_reg() {
        let new = m.get_use(reg).unwrap().to_reg();
        *r = new;
    }
}

fn map_def<RUM: RegUsageMapper>(m: &RUM, r: &mut Writable<Reg>) {
    if let Some(reg) = r.to_reg().as_virtual_reg() {
        let new = m.get_def(reg).unwrap().to_reg();
        *r = Writable::from_reg(new);
    }
}

fn map_mod<RUM: RegUsageMapper>(m: &RUM, r: &mut Writable<Reg>) {
    if let Some(reg) = r.to_reg().as_virtual_reg() {
        let new = m.get_mod(reg).unwrap().to_reg();
        *r = Writable::from_reg(new);
    }
}

impl Amode {
    fn map_uses<RUM: RegUsageMapper>(&mut self, map: &RUM) {
        match self {
            Amode::ImmReg { ref mut base, .. } => map_use(map, base),
            Amode::ImmRegRegShift {
                ref mut base,
                ref mut index,
                ..
            } => {
                map_use(map, base);
                map_use(map, index);
            }
            Amode::RipRelative { .. } => {
                // RIP isn't involved in regalloc.
            }
        }
    }
}

impl RegMemImm {
    fn map_uses<RUM: RegUsageMapper>(&mut self, map: &RUM) {
        match self {
            RegMemImm::Reg { ref mut reg } => map_use(map, reg),
            RegMemImm::Mem { ref mut addr } => addr.map_uses(map),
            RegMemImm::Imm { .. } => {}
        }
    }

    fn map_as_def<RUM: RegUsageMapper>(&mut self, mapper: &RUM) {
        match self {
            Self::Reg { reg } => {
                let mut writable_src = Writable::from_reg(*reg);
                map_def(mapper, &mut writable_src);
                *self = Self::reg(writable_src.to_reg());
            }
            _ => panic!("unexpected RegMemImm kind in map_src_reg_as_def"),
        }
    }
}

impl RegMem {
    fn map_uses<RUM: RegUsageMapper>(&mut self, map: &RUM) {
        match self {
            RegMem::Reg { ref mut reg } => map_use(map, reg),
            RegMem::Mem { ref mut addr, .. } => addr.map_uses(map),
        }
    }

    fn map_as_def<RUM: RegUsageMapper>(&mut self, mapper: &RUM) {
        match self {
            Self::Reg { reg } => {
                let mut writable_src = Writable::from_reg(*reg);
                map_def(mapper, &mut writable_src);
                *self = Self::reg(writable_src.to_reg());
            }
            _ => panic!("unexpected RegMem kind in map_src_reg_as_def"),
        }
    }
}

fn x64_map_regs<RUM: RegUsageMapper>(inst: &mut Inst, mapper: &RUM) {
    // Note this must be carefully synchronized with x64_get_regs.
    let produces_const = inst.produces_const();

    match inst {
        // ** Nop
        Inst::AluRmiR {
            ref mut src,
            ref mut dst,
            ..
        } => {
            if produces_const {
                src.map_as_def(mapper);
                map_def(mapper, dst);
            } else {
                src.map_uses(mapper);
                map_mod(mapper, dst);
            }
        }
        Inst::Not { src, .. } | Inst::Neg { src, .. } => map_mod(mapper, src),
        Inst::Div { divisor, .. } => divisor.map_uses(mapper),
        Inst::MulHi { rhs, .. } => rhs.map_uses(mapper),
        Inst::CheckedDivOrRemSeq { divisor, tmp, .. } => {
            map_mod(mapper, divisor);
            if let Some(tmp) = tmp {
                map_def(mapper, tmp)
            }
        }
        Inst::SignExtendData { .. } => {}
        Inst::XmmUnaryRmR {
            ref mut src,
            ref mut dst,
            ..
        }
        | Inst::UnaryRmR {
            ref mut src,
            ref mut dst,
            ..
        } => {
            src.map_uses(mapper);
            map_def(mapper, dst);
        }
        Inst::XmmRmRImm {
            ref op,
            ref mut src,
            ref mut dst,
            ..
        } => {
            if produces_const {
                src.map_as_def(mapper);
                map_def(mapper, dst);
            } else if *op == SseOpcode::Pextrb
                || *op == SseOpcode::Pextrw
                || *op == SseOpcode::Pextrd
                || *op == SseOpcode::Pshufd
            {
                src.map_uses(mapper);
                map_def(mapper, dst);
            } else {
                src.map_uses(mapper);
                map_mod(mapper, dst);
            }
        }
        Inst::XmmRmR {
            ref mut src,
            ref mut dst,
            ..
        } => {
            if produces_const {
                src.map_as_def(mapper);
                map_def(mapper, dst);
            } else {
                src.map_uses(mapper);
                map_mod(mapper, dst);
            }
        }
        Inst::XmmRmiReg {
            ref mut src,
            ref mut dst,
            ..
        } => {
            src.map_uses(mapper);
            map_mod(mapper, dst);
        }
        Inst::XmmUninitializedValue { ref mut dst, .. } => {
            map_def(mapper, dst);
        }
        Inst::XmmLoadConstSeq { ref mut dst, .. } => {
            map_def(mapper, dst);
        }
        Inst::XmmMinMaxSeq {
            ref mut lhs,
            ref mut rhs_dst,
            ..
        } => {
            map_use(mapper, lhs);
            map_mod(mapper, rhs_dst);
        }
        Inst::XmmMovRM {
            ref mut src,
            ref mut dst,
            ..
        } => {
            map_use(mapper, src);
            dst.map_uses(mapper);
        }
        Inst::XmmCmpRmR {
            ref mut src,
            ref mut dst,
            ..
        } => {
            src.map_uses(mapper);
            map_use(mapper, dst);
        }
        Inst::Imm { ref mut dst, .. } => map_def(mapper, dst),
        Inst::MovRR {
            ref mut src,
            ref mut dst,
            ..
        }
        | Inst::XmmToGpr {
            ref mut src,
            ref mut dst,
            ..
        } => {
            map_use(mapper, src);
            map_def(mapper, dst);
        }
        Inst::GprToXmm {
            ref mut src,
            ref mut dst,
            ..
        } => {
            src.map_uses(mapper);
            map_def(mapper, dst);
        }
        Inst::CvtUint64ToFloatSeq {
            ref mut src,
            ref mut dst,
            ref mut tmp_gpr1,
            ref mut tmp_gpr2,
            ..
        } => {
            map_mod(mapper, src);
            map_def(mapper, dst);
            map_def(mapper, tmp_gpr1);
            map_def(mapper, tmp_gpr2);
        }
        Inst::CvtFloatToSintSeq {
            ref mut src,
            ref mut dst,
            ref mut tmp_xmm,
            ref mut tmp_gpr,
            ..
        }
        | Inst::CvtFloatToUintSeq {
            ref mut src,
            ref mut dst,
            ref mut tmp_gpr,
            ref mut tmp_xmm,
            ..
        } => {
            map_mod(mapper, src);
            map_def(mapper, dst);
            map_def(mapper, tmp_gpr);
            map_def(mapper, tmp_xmm);
        }
        Inst::MovzxRmR {
            ref mut src,
            ref mut dst,
            ..
        } => {
            src.map_uses(mapper);
            map_def(mapper, dst);
        }
        Inst::Mov64MR { src, dst, .. } | Inst::LoadEffectiveAddress { addr: src, dst } => {
            src.map_uses(mapper);
            map_def(mapper, dst);
        }
        Inst::MovsxRmR {
            ref mut src,
            ref mut dst,
            ..
        } => {
            src.map_uses(mapper);
            map_def(mapper, dst);
        }
        Inst::MovRM {
            ref mut src,
            ref mut dst,
            ..
        } => {
            map_use(mapper, src);
            dst.map_uses(mapper);
        }
        Inst::ShiftR { ref mut dst, .. } => {
            map_mod(mapper, dst);
        }
        Inst::CmpRmiR {
            ref mut src,
            ref mut dst,
            ..
        } => {
            src.map_uses(mapper);
            map_use(mapper, dst);
        }
        Inst::Setcc { ref mut dst, .. } => map_def(mapper, dst),
        Inst::Cmove {
            ref mut src,
            ref mut dst,
            ..
        }
        | Inst::XmmCmove {
            ref mut src,
            ref mut dst,
            ..
        } => {
            src.map_uses(mapper);
            map_mod(mapper, dst)
        }
        Inst::Push64 { ref mut src } => src.map_uses(mapper),
        Inst::Pop64 { ref mut dst } => {
            map_def(mapper, dst);
        }

        Inst::CallKnown {
            ref mut uses,
            ref mut defs,
            ..
        } => {
            for r in uses.iter_mut() {
                map_use(mapper, r);
            }
            for r in defs.iter_mut() {
                map_def(mapper, r);
            }
        }

        Inst::CallUnknown {
            ref mut uses,
            ref mut defs,
            ref mut dest,
            ..
        } => {
            for r in uses.iter_mut() {
                map_use(mapper, r);
            }
            for r in defs.iter_mut() {
                map_def(mapper, r);
            }
            dest.map_uses(mapper);
        }

        Inst::JmpTableSeq {
            ref mut idx,
            ref mut tmp1,
            ref mut tmp2,
            ..
        } => {
            map_use(mapper, idx);
            map_def(mapper, tmp1);
            map_def(mapper, tmp2);
        }

        Inst::JmpUnknown { ref mut target } => target.map_uses(mapper),

        Inst::LoadExtName { ref mut dst, .. } => map_def(mapper, dst),

        Inst::LockCmpxchg {
            ref mut src,
            ref mut dst,
            ..
        } => {
            map_use(mapper, src);
            dst.map_uses(mapper);
        }

        Inst::Ret
        | Inst::EpiloguePlaceholder
        | Inst::JmpKnown { .. }
        | Inst::JmpCond { .. }
        | Inst::JmpIf { .. }
        | Inst::Nop { .. }
        | Inst::TrapIf { .. }
        | Inst::VirtualSPOffsetAdj { .. }
        | Inst::Ud2 { .. }
        | Inst::Hlt
        | Inst::AtomicRmwSeq { .. }
        | Inst::Fence { .. } => {
            // Instruction doesn't explicitly mention any regs, so it can't have any virtual
            // regs that we'd need to remap.  Hence no action required.
        }
    }
}

//=============================================================================
// Instructions: misc functions and external interface

impl MachInst for Inst {
    fn get_regs(&self, collector: &mut RegUsageCollector) {
        x64_get_regs(&self, collector)
    }

    fn map_regs<RUM: RegUsageMapper>(&mut self, mapper: &RUM) {
        x64_map_regs(self, mapper);
    }

    fn is_move(&self) -> Option<(Writable<Reg>, Reg)> {
        match self {
            // Note (carefully!) that a 32-bit mov *isn't* a no-op since it zeroes
            // out the upper 32 bits of the destination.  For example, we could
            // conceivably use `movl %reg, %reg` to zero out the top 32 bits of
            // %reg.
            Self::MovRR {
                is_64, src, dst, ..
            } if *is_64 => Some((*dst, *src)),
            // Note as well that MOVS[S|D] when used in the `XmmUnaryRmR` context are pure moves of
            // scalar floating-point values (and annotate `dst` as `def`s to the register allocator)
            // whereas the same operation in a packed context, e.g. `XMM_RM_R`, is used to merge a
            // value into the lowest lane of a vector (not a move).
            Self::XmmUnaryRmR { op, src, dst, .. }
                if *op == SseOpcode::Movss
                    || *op == SseOpcode::Movsd
                    || *op == SseOpcode::Movaps
                    || *op == SseOpcode::Movapd
                    || *op == SseOpcode::Movups
                    || *op == SseOpcode::Movupd
                    || *op == SseOpcode::Movdqa
                    || *op == SseOpcode::Movdqu =>
            {
                if let RegMem::Reg { reg } = src {
                    Some((*dst, *reg))
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    fn is_epilogue_placeholder(&self) -> bool {
        if let Self::EpiloguePlaceholder = self {
            true
        } else {
            false
        }
    }

    fn is_term<'a>(&'a self) -> MachTerminator<'a> {
        match self {
            // Interesting cases.
            &Self::Ret | &Self::EpiloguePlaceholder => MachTerminator::Ret,
            &Self::JmpKnown { dst } => MachTerminator::Uncond(dst),
            &Self::JmpCond {
                taken, not_taken, ..
            } => MachTerminator::Cond(taken, not_taken),
            &Self::JmpTableSeq {
                ref targets_for_term,
                ..
            } => MachTerminator::Indirect(&targets_for_term[..]),
            // All other cases are boring.
            _ => MachTerminator::None,
        }
    }

    fn gen_move(dst_reg: Writable<Reg>, src_reg: Reg, ty: Type) -> Inst {
        let rc_dst = dst_reg.to_reg().get_class();
        let rc_src = src_reg.get_class();
        // If this isn't true, we have gone way off the rails.
        debug_assert!(rc_dst == rc_src);
        match rc_dst {
            RegClass::I64 => Inst::mov_r_r(true, src_reg, dst_reg),
            RegClass::V128 => {
                // The Intel optimization manual, in "3.5.1.13 Zero-Latency MOV Instructions",
                // doesn't include MOVSS/MOVSD as instructions with zero-latency. Use movaps for
                // those, which may write more lanes that we need, but are specified to have
                // zero-latency.
                let opcode = match ty {
                    types::F32 | types::F64 | types::F32X4 => SseOpcode::Movaps,
                    types::F64X2 => SseOpcode::Movapd,
                    _ if ty.is_vector() && ty.bits() == 128 => SseOpcode::Movdqa,
                    _ => unimplemented!("unable to move type: {}", ty),
                };
                Inst::xmm_unary_rm_r(opcode, RegMem::reg(src_reg), dst_reg)
            }
            _ => panic!("gen_move(x64): unhandled regclass {:?}", rc_dst),
        }
    }

    fn gen_zero_len_nop() -> Inst {
        Inst::Nop { len: 0 }
    }

    fn gen_nop(preferred_size: usize) -> Inst {
        Inst::nop((preferred_size % 16) as u8)
    }

    fn maybe_direct_reload(&self, _reg: VirtualReg, _slot: SpillSlot) -> Option<Inst> {
        None
    }

    fn rc_for_type(ty: Type) -> CodegenResult<RegClass> {
        match ty {
            types::I8
            | types::I16
            | types::I32
            | types::I64
            | types::B1
            | types::B8
            | types::B16
            | types::B32
            | types::B64
            | types::R32
            | types::R64 => Ok(RegClass::I64),
            types::F32 | types::F64 => Ok(RegClass::V128),
            _ if ty.bits() == 128 => Ok(RegClass::V128),
            types::IFLAGS | types::FFLAGS => Ok(RegClass::I64),
            _ => Err(CodegenError::Unsupported(format!(
                "Unexpected SSA-value type: {}",
                ty
            ))),
        }
    }

    fn gen_jump(label: MachLabel) -> Inst {
        Inst::jmp_known(label)
    }

    fn gen_constant<F: FnMut(RegClass, Type) -> Writable<Reg>>(
        to_reg: Writable<Reg>,
        value: u64,
        ty: Type,
        mut alloc_tmp: F,
    ) -> SmallVec<[Self; 4]> {
        let mut ret = SmallVec::new();
        if ty == types::F32 {
            if value == 0 {
                ret.push(Inst::xmm_rm_r(
                    SseOpcode::Xorps,
                    RegMem::reg(to_reg.to_reg()),
                    to_reg,
                ));
            } else {
                let tmp = alloc_tmp(RegClass::I64, types::I32);
                ret.push(Inst::imm(OperandSize::Size32, value, tmp));

                ret.push(Inst::gpr_to_xmm(
                    SseOpcode::Movd,
                    RegMem::reg(tmp.to_reg()),
                    OperandSize::Size32,
                    to_reg,
                ));
            }
        } else if ty == types::F64 {
            if value == 0 {
                ret.push(Inst::xmm_rm_r(
                    SseOpcode::Xorpd,
                    RegMem::reg(to_reg.to_reg()),
                    to_reg,
                ));
            } else {
                let tmp = alloc_tmp(RegClass::I64, types::I64);
                ret.push(Inst::imm(OperandSize::Size64, value, tmp));

                ret.push(Inst::gpr_to_xmm(
                    SseOpcode::Movq,
                    RegMem::reg(tmp.to_reg()),
                    OperandSize::Size64,
                    to_reg,
                ));
            }
        } else {
            // Must be an integer type.
            debug_assert!(
                ty == types::B1
                    || ty == types::I8
                    || ty == types::B8
                    || ty == types::I16
                    || ty == types::B16
                    || ty == types::I32
                    || ty == types::B32
                    || ty == types::I64
                    || ty == types::B64
                    || ty == types::R32
                    || ty == types::R64
            );
            if value == 0 {
                ret.push(Inst::alu_rmi_r(
                    ty == types::I64,
                    AluRmiROpcode::Xor,
                    RegMemImm::reg(to_reg.to_reg()),
                    to_reg,
                ));
            } else {
                ret.push(Inst::imm(
                    OperandSize::from_bytes(ty.bytes()),
                    value,
                    to_reg,
                ));
            }
        }
        ret
    }

    fn reg_universe(flags: &Flags) -> RealRegUniverse {
        create_reg_universe_systemv(flags)
    }

    fn worst_case_size() -> CodeOffset {
        15
    }

    fn ref_type_regclass(_: &settings::Flags) -> RegClass {
        RegClass::I64
    }

    type LabelUse = LabelUse;
}

/// State carried between emissions of a sequence of instructions.
#[derive(Default, Clone, Debug)]
pub struct EmitState {
    /// Addend to convert nominal-SP offsets to real-SP offsets at the current
    /// program point.
    pub(crate) virtual_sp_offset: i64,
    /// Offset of FP from nominal-SP.
    pub(crate) nominal_sp_to_fp: i64,
    /// Safepoint stack map for upcoming instruction, as provided to `pre_safepoint()`.
    stack_map: Option<StackMap>,
}

/// Constant state used during emissions of a sequence of instructions.
pub struct EmitInfo {
    flags: settings::Flags,
    isa_flags: x64_settings::Flags,
}

impl EmitInfo {
    pub(crate) fn new(flags: settings::Flags, isa_flags: x64_settings::Flags) -> Self {
        Self { flags, isa_flags }
    }
}

impl MachInstEmitInfo for EmitInfo {
    fn flags(&self) -> &Flags {
        &self.flags
    }
}

impl MachInstEmit for Inst {
    type State = EmitState;
    type Info = EmitInfo;

    fn emit(&self, sink: &mut MachBuffer<Inst>, info: &Self::Info, state: &mut Self::State) {
        emit::emit(self, sink, info, state);
    }

    fn pretty_print(&self, mb_rru: Option<&RealRegUniverse>, _: &mut Self::State) -> String {
        self.show_rru(mb_rru)
    }
}

impl MachInstEmitState<Inst> for EmitState {
    fn new(abi: &dyn ABICallee<I = Inst>) -> Self {
        EmitState {
            virtual_sp_offset: 0,
            nominal_sp_to_fp: abi.frame_size() as i64,
            stack_map: None,
        }
    }

    fn pre_safepoint(&mut self, stack_map: StackMap) {
        self.stack_map = Some(stack_map);
    }
}

impl EmitState {
    fn take_stack_map(&mut self) -> Option<StackMap> {
        self.stack_map.take()
    }

    fn clear_post_insn(&mut self) {
        self.stack_map = None;
    }
}

/// A label-use (internal relocation) in generated code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LabelUse {
    /// A 32-bit offset from location of relocation itself, added to the existing value at that
    /// location. Used for control flow instructions which consider an offset from the start of the
    /// next instruction (so the size of the payload -- 4 bytes -- is subtracted from the payload).
    JmpRel32,

    /// A 32-bit offset from location of relocation itself, added to the existing value at that
    /// location.
    PCRel32,
}

impl MachInstLabelUse for LabelUse {
    const ALIGN: CodeOffset = 1;

    fn max_pos_range(self) -> CodeOffset {
        match self {
            LabelUse::JmpRel32 | LabelUse::PCRel32 => 0x7fff_ffff,
        }
    }

    fn max_neg_range(self) -> CodeOffset {
        match self {
            LabelUse::JmpRel32 | LabelUse::PCRel32 => 0x8000_0000,
        }
    }

    fn patch_size(self) -> CodeOffset {
        match self {
            LabelUse::JmpRel32 | LabelUse::PCRel32 => 4,
        }
    }

    fn patch(self, buffer: &mut [u8], use_offset: CodeOffset, label_offset: CodeOffset) {
        let pc_rel = (label_offset as i64) - (use_offset as i64);
        debug_assert!(pc_rel <= self.max_pos_range() as i64);
        debug_assert!(pc_rel >= -(self.max_neg_range() as i64));
        let pc_rel = pc_rel as u32;
        match self {
            LabelUse::JmpRel32 => {
                let addend = u32::from_le_bytes([buffer[0], buffer[1], buffer[2], buffer[3]]);
                let value = pc_rel.wrapping_add(addend).wrapping_sub(4);
                buffer.copy_from_slice(&value.to_le_bytes()[..]);
            }
            LabelUse::PCRel32 => {
                let addend = u32::from_le_bytes([buffer[0], buffer[1], buffer[2], buffer[3]]);
                let value = pc_rel.wrapping_add(addend);
                buffer.copy_from_slice(&value.to_le_bytes()[..]);
            }
        }
    }

    fn supports_veneer(self) -> bool {
        match self {
            LabelUse::JmpRel32 | LabelUse::PCRel32 => false,
        }
    }

    fn veneer_size(self) -> CodeOffset {
        match self {
            LabelUse::JmpRel32 | LabelUse::PCRel32 => 0,
        }
    }

    fn generate_veneer(self, _: &mut [u8], _: CodeOffset) -> (CodeOffset, LabelUse) {
        match self {
            LabelUse::JmpRel32 | LabelUse::PCRel32 => {
                panic!("Veneer not supported for JumpRel32 label-use.");
            }
        }
    }
}
