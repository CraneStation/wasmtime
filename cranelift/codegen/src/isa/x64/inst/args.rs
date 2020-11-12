//! Instruction operand sub-components (aka "parts"): definitions and printing.

use super::regs::{self, show_ireg_sized};
use super::EmitState;
use crate::ir::condcodes::{FloatCC, IntCC};
use crate::isa::x64::inst::Inst;
use crate::machinst::*;
use regalloc::{
    PrettyPrint, PrettyPrintSized, RealRegUniverse, Reg, RegClass, RegUsageCollector,
    RegUsageMapper, Writable,
};
use std::fmt;
use std::string::String;

/// A possible addressing mode (amode) that can be used in instructions.
/// These denote a 64-bit value only.
#[derive(Clone)]
pub enum Amode {
    /// Immediate sign-extended and a Register.
    ImmReg { simm32: u32, base: Reg },

    /// sign-extend-32-to-64(Immediate) + Register1 + (Register2 << Shift)
    ImmRegRegShift {
        simm32: u32,
        base: Reg,
        index: Reg,
        shift: u8, /* 0 .. 3 only */
    },

    /// sign-extend-32-to-64(Immediate) + RIP (instruction pointer).
    /// To wit: not supported in 32-bits mode.
    RipRelative { target: MachLabel },
}

impl Amode {
    pub(crate) fn imm_reg(simm32: u32, base: Reg) -> Self {
        debug_assert!(base.get_class() == RegClass::I64);
        Self::ImmReg { simm32, base }
    }

    pub(crate) fn imm_reg_reg_shift(simm32: u32, base: Reg, index: Reg, shift: u8) -> Self {
        debug_assert!(base.get_class() == RegClass::I64);
        debug_assert!(index.get_class() == RegClass::I64);
        debug_assert!(shift <= 3);
        Self::ImmRegRegShift {
            simm32,
            base,
            index,
            shift,
        }
    }

    pub(crate) fn rip_relative(target: MachLabel) -> Self {
        Self::RipRelative { target }
    }

    /// Add the regs mentioned by `self` to `collector`.
    pub(crate) fn get_regs_as_uses(&self, collector: &mut RegUsageCollector) {
        match self {
            Amode::ImmReg { base, .. } => {
                collector.add_use(*base);
            }
            Amode::ImmRegRegShift { base, index, .. } => {
                collector.add_use(*base);
                collector.add_use(*index);
            }
            Amode::RipRelative { .. } => {
                // RIP isn't involved in regalloc.
            }
        }
    }
}

impl PrettyPrint for Amode {
    fn show_rru(&self, mb_rru: Option<&RealRegUniverse>) -> String {
        match self {
            Amode::ImmReg { simm32, base } => {
                format!("{}({})", *simm32 as i32, base.show_rru(mb_rru))
            }
            Amode::ImmRegRegShift {
                simm32,
                base,
                index,
                shift,
            } => format!(
                "{}({},{},{})",
                *simm32 as i32,
                base.show_rru(mb_rru),
                index.show_rru(mb_rru),
                1 << shift
            ),
            Amode::RipRelative { ref target } => format!("label{}(%rip)", target.get()),
        }
    }
}

/// A Memory Address. These denote a 64-bit value only.
/// Used for usual addressing modes as well as addressing modes used during compilation, when the
/// moving SP offset is not known.
#[derive(Clone)]
pub enum SyntheticAmode {
    /// A real amode.
    Real(Amode),

    /// A (virtual) offset to the "nominal SP" value, which will be recomputed as we push and pop
    /// within the function.
    NominalSPOffset { simm32: u32 },

    /// A virtual offset to a constant that will be emitted in the constant section of the buffer.
    ConstantOffset(VCodeConstant),
}

impl SyntheticAmode {
    pub(crate) fn nominal_sp_offset(simm32: u32) -> Self {
        SyntheticAmode::NominalSPOffset { simm32 }
    }

    /// Add the regs mentioned by `self` to `collector`.
    pub(crate) fn get_regs_as_uses(&self, collector: &mut RegUsageCollector) {
        match self {
            SyntheticAmode::Real(addr) => addr.get_regs_as_uses(collector),
            SyntheticAmode::NominalSPOffset { .. } => {
                // Nothing to do; the base is SP and isn't involved in regalloc.
            }
            SyntheticAmode::ConstantOffset(_) => {}
        }
    }

    pub(crate) fn map_uses<RUM: RegUsageMapper>(&mut self, map: &RUM) {
        match self {
            SyntheticAmode::Real(addr) => addr.map_uses(map),
            SyntheticAmode::NominalSPOffset { .. } => {
                // Nothing to do.
            }
            SyntheticAmode::ConstantOffset(_) => {}
        }
    }

    pub(crate) fn finalize(&self, state: &mut EmitState, buffer: &MachBuffer<Inst>) -> Amode {
        match self {
            SyntheticAmode::Real(addr) => addr.clone(),
            SyntheticAmode::NominalSPOffset { simm32 } => {
                let off = *simm32 as i64 + state.virtual_sp_offset;
                // TODO will require a sequence of add etc.
                assert!(
                    off <= u32::max_value() as i64,
                    "amode finalize: add sequence NYI"
                );
                Amode::imm_reg(off as u32, regs::rsp())
            }
            SyntheticAmode::ConstantOffset(c) => {
                Amode::rip_relative(buffer.get_label_for_constant(*c))
            }
        }
    }
}

impl Into<SyntheticAmode> for Amode {
    fn into(self) -> SyntheticAmode {
        SyntheticAmode::Real(self)
    }
}

impl Into<SyntheticAmode> for VCodeConstant {
    fn into(self) -> SyntheticAmode {
        SyntheticAmode::ConstantOffset(self)
    }
}

impl PrettyPrint for SyntheticAmode {
    fn show_rru(&self, mb_rru: Option<&RealRegUniverse>) -> String {
        match self {
            SyntheticAmode::Real(addr) => addr.show_rru(mb_rru),
            SyntheticAmode::NominalSPOffset { simm32 } => {
                format!("rsp({} + virtual offset)", *simm32 as i32)
            }
            SyntheticAmode::ConstantOffset(c) => format!("const({})", c.as_u32()),
        }
    }
}

/// An operand which is either an integer Register, a value in Memory or an Immediate.  This can
/// denote an 8, 16, 32 or 64 bit value.  For the Immediate form, in the 8- and 16-bit case, only
/// the lower 8 or 16 bits of `simm32` is relevant.  In the 64-bit case, the value denoted by
/// `simm32` is its sign-extension out to 64 bits.
#[derive(Clone)]
pub enum RegMemImm {
    Reg { reg: Reg },
    Mem { addr: SyntheticAmode },
    Imm { simm32: u32 },
}

impl RegMemImm {
    pub(crate) fn reg(reg: Reg) -> Self {
        debug_assert!(reg.get_class() == RegClass::I64 || reg.get_class() == RegClass::V128);
        Self::Reg { reg }
    }
    pub(crate) fn mem(addr: impl Into<SyntheticAmode>) -> Self {
        Self::Mem { addr: addr.into() }
    }
    pub(crate) fn imm(simm32: u32) -> Self {
        Self::Imm { simm32 }
    }

    /// Asserts that in register mode, the reg class is the one that's expected.
    pub(crate) fn assert_regclass_is(&self, expected_reg_class: RegClass) {
        if let Self::Reg { reg } = self {
            debug_assert_eq!(reg.get_class(), expected_reg_class);
        }
    }

    /// Add the regs mentioned by `self` to `collector`.
    pub(crate) fn get_regs_as_uses(&self, collector: &mut RegUsageCollector) {
        match self {
            Self::Reg { reg } => collector.add_use(*reg),
            Self::Mem { addr } => addr.get_regs_as_uses(collector),
            Self::Imm { .. } => {}
        }
    }

    pub(crate) fn to_reg(&self) -> Option<Reg> {
        match self {
            Self::Reg { reg } => Some(*reg),
            _ => None,
        }
    }
}

impl PrettyPrint for RegMemImm {
    fn show_rru(&self, mb_rru: Option<&RealRegUniverse>) -> String {
        self.show_rru_sized(mb_rru, 8)
    }
}

impl PrettyPrintSized for RegMemImm {
    fn show_rru_sized(&self, mb_rru: Option<&RealRegUniverse>, size: u8) -> String {
        match self {
            Self::Reg { reg } => show_ireg_sized(*reg, mb_rru, size),
            Self::Mem { addr } => addr.show_rru(mb_rru),
            Self::Imm { simm32 } => format!("${}", *simm32 as i32),
        }
    }
}

/// An operand which is either an integer Register or a value in Memory.  This can denote an 8, 16,
/// 32, 64, or 128 bit value.
#[derive(Clone)]
pub enum RegMem {
    Reg { reg: Reg },
    Mem { addr: SyntheticAmode },
}

impl RegMem {
    pub(crate) fn reg(reg: Reg) -> Self {
        debug_assert!(reg.get_class() == RegClass::I64 || reg.get_class() == RegClass::V128);
        Self::Reg { reg }
    }
    pub(crate) fn mem(addr: impl Into<SyntheticAmode>) -> Self {
        Self::Mem { addr: addr.into() }
    }
    /// Asserts that in register mode, the reg class is the one that's expected.
    pub(crate) fn assert_regclass_is(&self, expected_reg_class: RegClass) {
        if let Self::Reg { reg } = self {
            debug_assert_eq!(reg.get_class(), expected_reg_class);
        }
    }
    /// Add the regs mentioned by `self` to `collector`.
    pub(crate) fn get_regs_as_uses(&self, collector: &mut RegUsageCollector) {
        match self {
            RegMem::Reg { reg } => collector.add_use(*reg),
            RegMem::Mem { addr, .. } => addr.get_regs_as_uses(collector),
        }
    }
    pub(crate) fn to_reg(&self) -> Option<Reg> {
        match self {
            RegMem::Reg { reg } => Some(*reg),
            _ => None,
        }
    }
}

impl From<Writable<Reg>> for RegMem {
    fn from(r: Writable<Reg>) -> Self {
        RegMem::reg(r.to_reg())
    }
}

impl PrettyPrint for RegMem {
    fn show_rru(&self, mb_rru: Option<&RealRegUniverse>) -> String {
        self.show_rru_sized(mb_rru, 8)
    }
}

impl PrettyPrintSized for RegMem {
    fn show_rru_sized(&self, mb_rru: Option<&RealRegUniverse>, size: u8) -> String {
        match self {
            RegMem::Reg { reg } => show_ireg_sized(*reg, mb_rru, size),
            RegMem::Mem { addr, .. } => addr.show_rru(mb_rru),
        }
    }
}

/// Some basic ALU operations.  TODO: maybe add Adc, Sbb.
#[derive(Copy, Clone, PartialEq)]
pub enum AluRmiROpcode {
    Add,
    Sub,
    And,
    Or,
    Xor,
    /// The signless, non-extending (N x N -> N, for N in {32,64}) variant.
    Mul,
}

impl fmt::Debug for AluRmiROpcode {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        let name = match self {
            AluRmiROpcode::Add => "add",
            AluRmiROpcode::Sub => "sub",
            AluRmiROpcode::And => "and",
            AluRmiROpcode::Or => "or",
            AluRmiROpcode::Xor => "xor",
            AluRmiROpcode::Mul => "imul",
        };
        write!(fmt, "{}", name)
    }
}

impl fmt::Display for AluRmiROpcode {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

#[derive(Clone, PartialEq)]
pub enum UnaryRmROpcode {
    /// Bit-scan reverse.
    Bsr,
    /// Bit-scan forward.
    Bsf,
}

impl fmt::Debug for UnaryRmROpcode {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        match self {
            UnaryRmROpcode::Bsr => write!(fmt, "bsr"),
            UnaryRmROpcode::Bsf => write!(fmt, "bsf"),
        }
    }
}

impl fmt::Display for UnaryRmROpcode {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

pub(crate) enum InstructionSet {
    SSE,
    SSE2,
    SSSE3,
    SSE41,
    SSE42,
}

/// Some SSE operations requiring 2 operands r/m and r.
#[derive(Clone, Copy, PartialEq)]
pub enum SseOpcode {
    Addps,
    Addpd,
    Addss,
    Addsd,
    Andps,
    Andpd,
    Andnps,
    Andnpd,
    Comiss,
    Comisd,
    Cmpps,
    Cmppd,
    Cmpss,
    Cmpsd,
    Cvtdq2ps,
    Cvtsd2ss,
    Cvtsd2si,
    Cvtsi2ss,
    Cvtsi2sd,
    Cvtss2si,
    Cvtss2sd,
    Cvttps2dq,
    Cvttss2si,
    Cvttsd2si,
    Divps,
    Divpd,
    Divss,
    Divsd,
    Insertps,
    Maxps,
    Maxpd,
    Maxss,
    Maxsd,
    Minps,
    Minpd,
    Minss,
    Minsd,
    Movaps,
    Movapd,
    Movd,
    Movdqa,
    Movdqu,
    Movlhps,
    Movmskps,
    Movmskpd,
    Movq,
    Movss,
    Movsd,
    Movups,
    Movupd,
    Mulps,
    Mulpd,
    Mulss,
    Mulsd,
    Orps,
    Orpd,
    Pabsb,
    Pabsw,
    Pabsd,
    Packsswb,
    Paddb,
    Paddd,
    Paddq,
    Paddw,
    Paddsb,
    Paddsw,
    Paddusb,
    Paddusw,
    Pand,
    Pandn,
    Pavgb,
    Pavgw,
    Pcmpeqb,
    Pcmpeqw,
    Pcmpeqd,
    Pcmpeqq,
    Pcmpgtb,
    Pcmpgtw,
    Pcmpgtd,
    Pcmpgtq,
    Pextrb,
    Pextrw,
    Pextrd,
    Pinsrb,
    Pinsrw,
    Pinsrd,
    Pmaxsb,
    Pmaxsw,
    Pmaxsd,
    Pmaxub,
    Pmaxuw,
    Pmaxud,
    Pminsb,
    Pminsw,
    Pminsd,
    Pminub,
    Pminuw,
    Pminud,
    Pmovmskb,
    Pmulld,
    Pmullw,
    Pmuludq,
    Por,
    Pshufb,
    Pshufd,
    Psllw,
    Pslld,
    Psllq,
    Psraw,
    Psrad,
    Psrlw,
    Psrld,
    Psrlq,
    Psubb,
    Psubd,
    Psubq,
    Psubw,
    Psubsb,
    Psubsw,
    Psubusb,
    Psubusw,
    Ptest,
    Punpckhbw,
    Punpcklbw,
    Pxor,
    Rcpss,
    Roundss,
    Roundsd,
    Rsqrtss,
    Sqrtps,
    Sqrtpd,
    Sqrtss,
    Sqrtsd,
    Subps,
    Subpd,
    Subss,
    Subsd,
    Ucomiss,
    Ucomisd,
    Xorps,
    Xorpd,
}

impl SseOpcode {
    /// Which `InstructionSet` is the first supporting this opcode?
    pub(crate) fn available_from(&self) -> InstructionSet {
        use InstructionSet::*;
        match self {
            SseOpcode::Addps
            | SseOpcode::Addss
            | SseOpcode::Andps
            | SseOpcode::Andnps
            | SseOpcode::Comiss
            | SseOpcode::Cmpps
            | SseOpcode::Cmpss
            | SseOpcode::Cvtsi2ss
            | SseOpcode::Cvtss2si
            | SseOpcode::Cvttss2si
            | SseOpcode::Divps
            | SseOpcode::Divss
            | SseOpcode::Maxps
            | SseOpcode::Maxss
            | SseOpcode::Minps
            | SseOpcode::Minss
            | SseOpcode::Movaps
            | SseOpcode::Movlhps
            | SseOpcode::Movmskps
            | SseOpcode::Movss
            | SseOpcode::Movups
            | SseOpcode::Mulps
            | SseOpcode::Mulss
            | SseOpcode::Orps
            | SseOpcode::Rcpss
            | SseOpcode::Rsqrtss
            | SseOpcode::Sqrtps
            | SseOpcode::Sqrtss
            | SseOpcode::Subps
            | SseOpcode::Subss
            | SseOpcode::Ucomiss
            | SseOpcode::Xorps => SSE,

            SseOpcode::Addpd
            | SseOpcode::Addsd
            | SseOpcode::Andpd
            | SseOpcode::Andnpd
            | SseOpcode::Cmppd
            | SseOpcode::Cmpsd
            | SseOpcode::Comisd
            | SseOpcode::Cvtdq2ps
            | SseOpcode::Cvtsd2ss
            | SseOpcode::Cvtsd2si
            | SseOpcode::Cvtsi2sd
            | SseOpcode::Cvtss2sd
            | SseOpcode::Cvttps2dq
            | SseOpcode::Cvttsd2si
            | SseOpcode::Divpd
            | SseOpcode::Divsd
            | SseOpcode::Maxpd
            | SseOpcode::Maxsd
            | SseOpcode::Minpd
            | SseOpcode::Minsd
            | SseOpcode::Movapd
            | SseOpcode::Movd
            | SseOpcode::Movmskpd
            | SseOpcode::Movq
            | SseOpcode::Movsd
            | SseOpcode::Movupd
            | SseOpcode::Movdqa
            | SseOpcode::Movdqu
            | SseOpcode::Mulpd
            | SseOpcode::Mulsd
            | SseOpcode::Orpd
            | SseOpcode::Packsswb
            | SseOpcode::Paddb
            | SseOpcode::Paddd
            | SseOpcode::Paddq
            | SseOpcode::Paddw
            | SseOpcode::Paddsb
            | SseOpcode::Paddsw
            | SseOpcode::Paddusb
            | SseOpcode::Paddusw
            | SseOpcode::Pand
            | SseOpcode::Pandn
            | SseOpcode::Pavgb
            | SseOpcode::Pavgw
            | SseOpcode::Pcmpeqb
            | SseOpcode::Pcmpeqw
            | SseOpcode::Pcmpeqd
            | SseOpcode::Pcmpgtb
            | SseOpcode::Pcmpgtw
            | SseOpcode::Pcmpgtd
            | SseOpcode::Pextrw
            | SseOpcode::Pinsrw
            | SseOpcode::Pmaxsw
            | SseOpcode::Pmaxub
            | SseOpcode::Pminsw
            | SseOpcode::Pminub
            | SseOpcode::Pmovmskb
            | SseOpcode::Pmullw
            | SseOpcode::Pmuludq
            | SseOpcode::Por
            | SseOpcode::Pshufd
            | SseOpcode::Psllw
            | SseOpcode::Pslld
            | SseOpcode::Psllq
            | SseOpcode::Psraw
            | SseOpcode::Psrad
            | SseOpcode::Psrlw
            | SseOpcode::Psrld
            | SseOpcode::Psrlq
            | SseOpcode::Psubb
            | SseOpcode::Psubd
            | SseOpcode::Psubq
            | SseOpcode::Psubw
            | SseOpcode::Psubsb
            | SseOpcode::Psubsw
            | SseOpcode::Psubusb
            | SseOpcode::Psubusw
            | SseOpcode::Punpckhbw
            | SseOpcode::Punpcklbw
            | SseOpcode::Pxor
            | SseOpcode::Sqrtpd
            | SseOpcode::Sqrtsd
            | SseOpcode::Subpd
            | SseOpcode::Subsd
            | SseOpcode::Ucomisd
            | SseOpcode::Xorpd => SSE2,

            SseOpcode::Pabsb | SseOpcode::Pabsw | SseOpcode::Pabsd | SseOpcode::Pshufb => SSSE3,

            SseOpcode::Insertps
            | SseOpcode::Pcmpeqq
            | SseOpcode::Pextrb
            | SseOpcode::Pextrd
            | SseOpcode::Pinsrb
            | SseOpcode::Pinsrd
            | SseOpcode::Pmaxsb
            | SseOpcode::Pmaxsd
            | SseOpcode::Pmaxuw
            | SseOpcode::Pmaxud
            | SseOpcode::Pminsb
            | SseOpcode::Pminsd
            | SseOpcode::Pminuw
            | SseOpcode::Pminud
            | SseOpcode::Pmulld
            | SseOpcode::Ptest
            | SseOpcode::Roundss
            | SseOpcode::Roundsd => SSE41,

            SseOpcode::Pcmpgtq => SSE42,
        }
    }

    /// Returns the src operand size for an instruction.
    pub(crate) fn src_size(&self) -> u8 {
        match self {
            SseOpcode::Movd => 4,
            _ => 8,
        }
    }
}

impl fmt::Debug for SseOpcode {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        let name = match self {
            SseOpcode::Addps => "addps",
            SseOpcode::Addpd => "addpd",
            SseOpcode::Addss => "addss",
            SseOpcode::Addsd => "addsd",
            SseOpcode::Andpd => "andpd",
            SseOpcode::Andps => "andps",
            SseOpcode::Andnps => "andnps",
            SseOpcode::Andnpd => "andnpd",
            SseOpcode::Cmpps => "cmpps",
            SseOpcode::Cmppd => "cmppd",
            SseOpcode::Cmpss => "cmpss",
            SseOpcode::Cmpsd => "cmpsd",
            SseOpcode::Comiss => "comiss",
            SseOpcode::Comisd => "comisd",
            SseOpcode::Cvtdq2ps => "cvtdq2ps",
            SseOpcode::Cvtsd2ss => "cvtsd2ss",
            SseOpcode::Cvtsd2si => "cvtsd2si",
            SseOpcode::Cvtsi2ss => "cvtsi2ss",
            SseOpcode::Cvtsi2sd => "cvtsi2sd",
            SseOpcode::Cvtss2si => "cvtss2si",
            SseOpcode::Cvtss2sd => "cvtss2sd",
            SseOpcode::Cvttps2dq => "cvttps2dq",
            SseOpcode::Cvttss2si => "cvttss2si",
            SseOpcode::Cvttsd2si => "cvttsd2si",
            SseOpcode::Divps => "divps",
            SseOpcode::Divpd => "divpd",
            SseOpcode::Divss => "divss",
            SseOpcode::Divsd => "divsd",
            SseOpcode::Insertps => "insertps",
            SseOpcode::Maxps => "maxps",
            SseOpcode::Maxpd => "maxpd",
            SseOpcode::Maxss => "maxss",
            SseOpcode::Maxsd => "maxsd",
            SseOpcode::Minps => "minps",
            SseOpcode::Minpd => "minpd",
            SseOpcode::Minss => "minss",
            SseOpcode::Minsd => "minsd",
            SseOpcode::Movaps => "movaps",
            SseOpcode::Movapd => "movapd",
            SseOpcode::Movd => "movd",
            SseOpcode::Movdqa => "movdqa",
            SseOpcode::Movdqu => "movdqu",
            SseOpcode::Movlhps => "movlhps",
            SseOpcode::Movmskps => "movmskps",
            SseOpcode::Movmskpd => "movmskpd",
            SseOpcode::Movq => "movq",
            SseOpcode::Movss => "movss",
            SseOpcode::Movsd => "movsd",
            SseOpcode::Movups => "movups",
            SseOpcode::Movupd => "movupd",
            SseOpcode::Mulps => "mulps",
            SseOpcode::Mulpd => "mulpd",
            SseOpcode::Mulss => "mulss",
            SseOpcode::Mulsd => "mulsd",
            SseOpcode::Orpd => "orpd",
            SseOpcode::Orps => "orps",
            SseOpcode::Pabsb => "pabsb",
            SseOpcode::Pabsw => "pabsw",
            SseOpcode::Pabsd => "pabsd",
            SseOpcode::Packsswb => "packsswb",
            SseOpcode::Paddb => "paddb",
            SseOpcode::Paddd => "paddd",
            SseOpcode::Paddq => "paddq",
            SseOpcode::Paddw => "paddw",
            SseOpcode::Paddsb => "paddsb",
            SseOpcode::Paddsw => "paddsw",
            SseOpcode::Paddusb => "paddusb",
            SseOpcode::Paddusw => "paddusw",
            SseOpcode::Pand => "pand",
            SseOpcode::Pandn => "pandn",
            SseOpcode::Pavgb => "pavgb",
            SseOpcode::Pavgw => "pavgw",
            SseOpcode::Pcmpeqb => "pcmpeqb",
            SseOpcode::Pcmpeqw => "pcmpeqw",
            SseOpcode::Pcmpeqd => "pcmpeqd",
            SseOpcode::Pcmpeqq => "pcmpeqq",
            SseOpcode::Pcmpgtb => "pcmpgtb",
            SseOpcode::Pcmpgtw => "pcmpgtw",
            SseOpcode::Pcmpgtd => "pcmpgtd",
            SseOpcode::Pcmpgtq => "pcmpgtq",
            SseOpcode::Pextrb => "pextrb",
            SseOpcode::Pextrw => "pextrw",
            SseOpcode::Pextrd => "pextrd",
            SseOpcode::Pinsrb => "pinsrb",
            SseOpcode::Pinsrw => "pinsrw",
            SseOpcode::Pinsrd => "pinsrd",
            SseOpcode::Pmaxsb => "pmaxsb",
            SseOpcode::Pmaxsw => "pmaxsw",
            SseOpcode::Pmaxsd => "pmaxsd",
            SseOpcode::Pmaxub => "pmaxub",
            SseOpcode::Pmaxuw => "pmaxuw",
            SseOpcode::Pmaxud => "pmaxud",
            SseOpcode::Pminsb => "pminsb",
            SseOpcode::Pminsw => "pminsw",
            SseOpcode::Pminsd => "pminsd",
            SseOpcode::Pminub => "pminub",
            SseOpcode::Pminuw => "pminuw",
            SseOpcode::Pminud => "pminud",
            SseOpcode::Pmovmskb => "pmovmskb",
            SseOpcode::Pmulld => "pmulld",
            SseOpcode::Pmullw => "pmullw",
            SseOpcode::Pmuludq => "pmuludq",
            SseOpcode::Por => "por",
            SseOpcode::Pshufb => "pshufb",
            SseOpcode::Pshufd => "pshufd",
            SseOpcode::Psllw => "psllw",
            SseOpcode::Pslld => "pslld",
            SseOpcode::Psllq => "psllq",
            SseOpcode::Psraw => "psraw",
            SseOpcode::Psrad => "psrad",
            SseOpcode::Psrlw => "psrlw",
            SseOpcode::Psrld => "psrld",
            SseOpcode::Psrlq => "psrlq",
            SseOpcode::Psubb => "psubb",
            SseOpcode::Psubd => "psubd",
            SseOpcode::Psubq => "psubq",
            SseOpcode::Psubw => "psubw",
            SseOpcode::Psubsb => "psubsb",
            SseOpcode::Psubsw => "psubsw",
            SseOpcode::Psubusb => "psubusb",
            SseOpcode::Psubusw => "psubusw",
            SseOpcode::Ptest => "ptest",
            SseOpcode::Punpckhbw => "punpckhbw",
            SseOpcode::Punpcklbw => "punpcklbw",
            SseOpcode::Pxor => "pxor",
            SseOpcode::Rcpss => "rcpss",
            SseOpcode::Roundss => "roundss",
            SseOpcode::Roundsd => "roundsd",
            SseOpcode::Rsqrtss => "rsqrtss",
            SseOpcode::Sqrtps => "sqrtps",
            SseOpcode::Sqrtpd => "sqrtpd",
            SseOpcode::Sqrtss => "sqrtss",
            SseOpcode::Sqrtsd => "sqrtsd",
            SseOpcode::Subps => "subps",
            SseOpcode::Subpd => "subpd",
            SseOpcode::Subss => "subss",
            SseOpcode::Subsd => "subsd",
            SseOpcode::Ucomiss => "ucomiss",
            SseOpcode::Ucomisd => "ucomisd",
            SseOpcode::Xorps => "xorps",
            SseOpcode::Xorpd => "xorpd",
        };
        write!(fmt, "{}", name)
    }
}

impl fmt::Display for SseOpcode {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

/// This defines the ways a value can be extended: either signed- or zero-extension, or none for
/// types that are not extended. Contrast with [ExtMode], which defines the widths from and to which
/// values can be extended.
#[derive(Clone, PartialEq)]
pub enum ExtKind {
    None,
    SignExtend,
    ZeroExtend,
}

/// These indicate ways of extending (widening) a value, using the Intel
/// naming: B(yte) = u8, W(ord) = u16, L(ong)word = u32, Q(uad)word = u64
#[derive(Clone, PartialEq)]
pub enum ExtMode {
    /// Byte -> Longword.
    BL,
    /// Byte -> Quadword.
    BQ,
    /// Word -> Longword.
    WL,
    /// Word -> Quadword.
    WQ,
    /// Longword -> Quadword.
    LQ,
}

impl ExtMode {
    /// Calculate the `ExtMode` from passed bit lengths of the from/to types.
    pub(crate) fn new(from_bits: u16, to_bits: u16) -> Option<ExtMode> {
        match (from_bits, to_bits) {
            (1, 8) | (1, 16) | (1, 32) | (8, 16) | (8, 32) => Some(ExtMode::BL),
            (1, 64) | (8, 64) => Some(ExtMode::BQ),
            (16, 32) => Some(ExtMode::WL),
            (16, 64) => Some(ExtMode::WQ),
            (32, 64) => Some(ExtMode::LQ),
            _ => None,
        }
    }

    /// Return the source register size in bytes.
    pub(crate) fn src_size(&self) -> u8 {
        match self {
            ExtMode::BL | ExtMode::BQ => 1,
            ExtMode::WL | ExtMode::WQ => 2,
            ExtMode::LQ => 4,
        }
    }

    /// Return the destination register size in bytes.
    pub(crate) fn dst_size(&self) -> u8 {
        match self {
            ExtMode::BL | ExtMode::WL => 4,
            ExtMode::BQ | ExtMode::WQ | ExtMode::LQ => 8,
        }
    }
}

impl fmt::Debug for ExtMode {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        let name = match self {
            ExtMode::BL => "bl",
            ExtMode::BQ => "bq",
            ExtMode::WL => "wl",
            ExtMode::WQ => "wq",
            ExtMode::LQ => "lq",
        };
        write!(fmt, "{}", name)
    }
}

impl fmt::Display for ExtMode {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

/// These indicate the form of a scalar shift/rotate: left, signed right, unsigned right.
#[derive(Clone)]
pub enum ShiftKind {
    ShiftLeft,
    /// Inserts zeros in the most significant bits.
    ShiftRightLogical,
    /// Replicates the sign bit in the most significant bits.
    ShiftRightArithmetic,
    RotateLeft,
    RotateRight,
}

impl fmt::Debug for ShiftKind {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        let name = match self {
            ShiftKind::ShiftLeft => "shl",
            ShiftKind::ShiftRightLogical => "shr",
            ShiftKind::ShiftRightArithmetic => "sar",
            ShiftKind::RotateLeft => "rol",
            ShiftKind::RotateRight => "ror",
        };
        write!(fmt, "{}", name)
    }
}

impl fmt::Display for ShiftKind {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

/// What kind of division or remainer instruction this is?
#[derive(Clone)]
pub enum DivOrRemKind {
    SignedDiv,
    UnsignedDiv,
    SignedRem,
    UnsignedRem,
}

impl DivOrRemKind {
    pub(crate) fn is_signed(&self) -> bool {
        match self {
            DivOrRemKind::SignedDiv | DivOrRemKind::SignedRem => true,
            _ => false,
        }
    }

    pub(crate) fn is_div(&self) -> bool {
        match self {
            DivOrRemKind::SignedDiv | DivOrRemKind::UnsignedDiv => true,
            _ => false,
        }
    }
}

/// These indicate condition code tests.  Not all are represented since not all are useful in
/// compiler-generated code.
#[derive(Copy, Clone)]
#[repr(u8)]
pub enum CC {
    ///  overflow
    O = 0,
    /// no overflow
    NO = 1,

    /// < unsigned
    B = 2,
    /// >= unsigned
    NB = 3,

    /// zero
    Z = 4,
    /// not-zero
    NZ = 5,

    /// <= unsigned
    BE = 6,
    /// > unsigned
    NBE = 7,

    /// negative
    S = 8,
    /// not-negative
    NS = 9,

    /// < signed
    L = 12,
    /// >= signed
    NL = 13,

    /// <= signed
    LE = 14,
    /// > signed
    NLE = 15,

    /// parity
    P = 10,

    /// not parity
    NP = 11,
}

impl CC {
    pub(crate) fn from_intcc(intcc: IntCC) -> Self {
        match intcc {
            IntCC::Equal => CC::Z,
            IntCC::NotEqual => CC::NZ,
            IntCC::SignedGreaterThanOrEqual => CC::NL,
            IntCC::SignedGreaterThan => CC::NLE,
            IntCC::SignedLessThanOrEqual => CC::LE,
            IntCC::SignedLessThan => CC::L,
            IntCC::UnsignedGreaterThanOrEqual => CC::NB,
            IntCC::UnsignedGreaterThan => CC::NBE,
            IntCC::UnsignedLessThanOrEqual => CC::BE,
            IntCC::UnsignedLessThan => CC::B,
            IntCC::Overflow => CC::O,
            IntCC::NotOverflow => CC::NO,
        }
    }

    pub(crate) fn invert(&self) -> Self {
        match self {
            CC::O => CC::NO,
            CC::NO => CC::O,

            CC::B => CC::NB,
            CC::NB => CC::B,

            CC::Z => CC::NZ,
            CC::NZ => CC::Z,

            CC::BE => CC::NBE,
            CC::NBE => CC::BE,

            CC::S => CC::NS,
            CC::NS => CC::S,

            CC::L => CC::NL,
            CC::NL => CC::L,

            CC::LE => CC::NLE,
            CC::NLE => CC::LE,

            CC::P => CC::NP,
            CC::NP => CC::P,
        }
    }

    pub(crate) fn from_floatcc(floatcc: FloatCC) -> Self {
        match floatcc {
            FloatCC::Ordered => CC::NP,
            FloatCC::Unordered => CC::P,
            // Alias for NE
            FloatCC::OrderedNotEqual => CC::NZ,
            // Alias for E
            FloatCC::UnorderedOrEqual => CC::Z,
            // Alias for A
            FloatCC::GreaterThan => CC::NBE,
            // Alias for AE
            FloatCC::GreaterThanOrEqual => CC::NB,
            FloatCC::UnorderedOrLessThan => CC::B,
            FloatCC::UnorderedOrLessThanOrEqual => CC::BE,
            FloatCC::Equal
            | FloatCC::NotEqual
            | FloatCC::LessThan
            | FloatCC::LessThanOrEqual
            | FloatCC::UnorderedOrGreaterThan
            | FloatCC::UnorderedOrGreaterThanOrEqual => panic!(
                "{:?} can't be lowered to a CC code; treat as special case.",
                floatcc
            ),
        }
    }

    pub(crate) fn get_enc(self) -> u8 {
        self as u8
    }
}

impl fmt::Debug for CC {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        let name = match self {
            CC::O => "o",
            CC::NO => "no",
            CC::B => "b",
            CC::NB => "nb",
            CC::Z => "z",
            CC::NZ => "nz",
            CC::BE => "be",
            CC::NBE => "nbe",
            CC::S => "s",
            CC::NS => "ns",
            CC::L => "l",
            CC::NL => "nl",
            CC::LE => "le",
            CC::NLE => "nle",
            CC::P => "p",
            CC::NP => "np",
        };
        write!(fmt, "{}", name)
    }
}

impl fmt::Display for CC {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

/// Encode the ways that floats can be compared. This is used in float comparisons such as `cmpps`,
/// e.g.; it is distinguished from other float comparisons (e.g. `ucomiss`) in that those use EFLAGS
/// whereas [FcmpImm] is used as an immediate.
pub(crate) enum FcmpImm {
    Equal = 0x00,
    LessThan = 0x01,
    LessThanOrEqual = 0x02,
    Unordered = 0x03,
    NotEqual = 0x04,
    UnorderedOrGreaterThanOrEqual = 0x05,
    UnorderedOrGreaterThan = 0x06,
    Ordered = 0x07,
}

impl FcmpImm {
    pub(crate) fn encode(self) -> u8 {
        self as u8
    }
}

impl From<FloatCC> for FcmpImm {
    fn from(cond: FloatCC) -> Self {
        match cond {
            FloatCC::Equal => FcmpImm::Equal,
            FloatCC::LessThan => FcmpImm::LessThan,
            FloatCC::LessThanOrEqual => FcmpImm::LessThanOrEqual,
            FloatCC::Unordered => FcmpImm::Unordered,
            FloatCC::NotEqual => FcmpImm::NotEqual,
            FloatCC::UnorderedOrGreaterThanOrEqual => FcmpImm::UnorderedOrGreaterThanOrEqual,
            FloatCC::UnorderedOrGreaterThan => FcmpImm::UnorderedOrGreaterThan,
            FloatCC::Ordered => FcmpImm::Ordered,
            _ => panic!("unable to create comparison predicate for {}", cond),
        }
    }
}

/// An operand's size in bits.
#[derive(Clone, Copy, PartialEq)]
pub enum OperandSize {
    Size32,
    Size64,
}

impl OperandSize {
    pub(crate) fn from_bytes(num_bytes: u32) -> Self {
        match num_bytes {
            1 | 2 | 4 => OperandSize::Size32,
            8 => OperandSize::Size64,
            _ => unreachable!(),
        }
    }

    pub(crate) fn to_bytes(&self) -> u8 {
        match self {
            Self::Size32 => 4,
            Self::Size64 => 8,
        }
    }

    pub(crate) fn to_bits(&self) -> u8 {
        match self {
            Self::Size32 => 32,
            Self::Size64 => 64,
        }
    }
}

/// An x64 memory fence kind.
#[derive(Clone)]
#[allow(dead_code)]
pub enum FenceKind {
    /// `mfence` instruction ("Memory Fence")
    MFence,
    /// `lfence` instruction ("Load Fence")
    LFence,
    /// `sfence` instruction ("Store Fence")
    SFence,
}
