//! This module defines 32-bit ARM specific machine instruction types.

#![allow(dead_code)]

use crate::binemit::CodeOffset;
use crate::ir::types::{B1, B16, B32, B8, I16, I32, I8, IFLAGS};
use crate::ir::{ExternalName, Opcode, TrapCode, Type};
use crate::machinst::*;
use crate::{settings, CodegenError, CodegenResult};

use regalloc::{PrettyPrint, RealRegUniverse, Reg, RegClass, SpillSlot, VirtualReg, Writable};
use regalloc::{RegUsageCollector, RegUsageMapper};

use alloc::boxed::Box;
use alloc::vec::Vec;
use smallvec::{smallvec, SmallVec};
use std::string::{String, ToString};

mod args;
pub use self::args::*;
mod emit;
pub use self::emit::*;
mod regs;
pub use self::regs::*;
pub mod unwind;

#[cfg(test)]
mod emit_tests;

//=============================================================================
// Instructions (top level): definition

/// An ALU operation. This can be paired with several instruction formats
/// below (see `Inst`) in any combination.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum ALUOp {
    Add,
    Adds,
    Adc,
    Adcs,
    Qadd,
    Sub,
    Subs,
    Sbc,
    Sbcs,
    Rsb,
    Qsub,
    Mul,
    Smull,
    Umull,
    Udiv,
    Sdiv,
    And,
    Orr,
    Orn,
    Eor,
    Bic,
    Lsl,
    Lsr,
    Asr,
    Ror,
}

/// An ALU operation with one argument.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum ALUOp1 {
    Mvn,
    Mov,
}

/// An operation on the bits of a register.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum BitOp {
    Rbit,
    Rev,
    Clz,
}

/// Additional information for (direct) Call instructions, left out of line to lower the size of
/// the Inst enum.
#[derive(Clone, Debug)]
pub struct CallInfo {
    pub dest: ExternalName,
    pub uses: Vec<Reg>,
    pub defs: Vec<Writable<Reg>>,
    pub opcode: Opcode,
}

/// Additional information for CallInd instructions, left out of line to lower the size of the Inst
/// enum.
#[derive(Clone, Debug)]
pub struct CallIndInfo {
    pub rm: Reg,
    pub uses: Vec<Reg>,
    pub defs: Vec<Writable<Reg>>,
    pub opcode: Opcode,
}

/// Instruction formats.
#[derive(Clone, Debug)]
pub enum Inst {
    /// A no-op of zero size.
    Nop0,

    /// A no-op that is two bytes large.
    Nop2,

    /// An ALU operation with two register sources and one register destination.
    AluRRR {
        alu_op: ALUOp,
        rd: Writable<Reg>,
        rn: Reg,
        rm: Reg,
    },

    /// An ALU operation with two register sources, one of which can be optionally shifted
    /// and one register destination.
    AluRRRShift {
        alu_op: ALUOp,
        rd: Writable<Reg>,
        rn: Reg,
        rm: Reg,
        shift: Option<ShiftOpAndAmt>,
    },

    /// An ALU operation with one register source, which can be optionally shifted
    /// and one register destination.
    AluRRShift {
        alu_op: ALUOp1,
        rd: Writable<Reg>,
        rm: Reg,
        shift: Option<ShiftOpAndAmt>,
    },

    /// An ALU operation with two register sources and two register destinations.
    AluRRRR {
        alu_op: ALUOp,
        rd_hi: Writable<Reg>,
        rd_lo: Writable<Reg>,
        rn: Reg,
        rm: Reg,
    },

    /// An ALU operation with a register source and a 12-bit immediate source,
    /// and a register destination.
    AluRRImm12 {
        alu_op: ALUOp,
        rd: Writable<Reg>,
        rn: Reg,
        imm12: UImm12,
    },

    /// An ALU operation with a register source and a 8-bit immediate source,
    /// and a register destination.
    ///
    /// In fact these instructions take a `modified immediate constant` operand,
    /// which is encoded as a 12-bit immediate. The only case used here
    /// is when high 4 bits of that 12-immediate are zeros.
    /// In this case operand is simple 8-bit immediate.
    /// For all possible operands see
    /// https://static.docs.arm.com/ddi0406/c/DDI0406C_C_arm_architecture_reference_manual.pdf#G10.4954509
    AluRRImm8 {
        alu_op: ALUOp,
        rd: Writable<Reg>,
        rn: Reg,
        imm8: UImm8,
    },

    /// An ALU operation with a 8-bit immediate and a register destination.
    /// See `AluRRImm8` description above.
    AluRImm8 {
        alu_op: ALUOp1,
        rd: Writable<Reg>,
        imm8: UImm8,
    },

    /// A bit operation with a register source and a register destination.
    BitOpRR {
        bit_op: BitOp,
        rd: Writable<Reg>,
        rm: Reg,
    },

    /// A mov instruction with a GPR source and a GPR destination.
    Mov {
        rd: Writable<Reg>,
        rm: Reg,
    },

    /// A move instruction with a 16-bit immediate source and a register destination.
    MovImm16 {
        rd: Writable<Reg>,
        imm16: u16,
    },

    /// A move top instruction, which writes 16-bit immediate to the top
    /// halfword of the destination register.
    Movt {
        rd: Writable<Reg>,
        imm16: u16,
    },

    /// A compare instruction with two register arguments.
    Cmp {
        rn: Reg,
        rm: Reg,
    },

    /// A compare instruction with a register operand and a 8-bit immediate operand.
    CmpImm8 {
        rn: Reg,
        imm8: u8,
    },

    /// A store instruction, which stores to memory 8, 16 or 32-bit operand.
    Store {
        rt: Reg,
        mem: AMode,
        bits: u8,
    },

    /// A load instruction, which loads from memory 8, 16 or 32-bit operand,
    /// which can be sign- or zero-extended.
    Load {
        rt: Writable<Reg>,
        mem: AMode,
        bits: u8,
        sign_extend: bool,
    },

    /// Load address referenced by `mem` into `rd`.
    LoadAddr {
        rd: Writable<Reg>,
        mem: AMode,
    },

    /// A sign- or zero-extend operation.
    Extend {
        rd: Writable<Reg>,
        rm: Reg,
        from_bits: u8,
        signed: bool,
    },

    // An If-Then instruction, which makes up to four instructions conditinal.
    It {
        cond: Cond,
        insts: Vec<CondInst>,
    },

    /// A push instuction, which stores registers to the stack and updates sp.
    Push {
        reg_list: Vec<Reg>,
    },

    /// A pop instuction, which load registers from the stack and updates sp.
    Pop {
        reg_list: Vec<Writable<Reg>>,
    },

    /// A machine call instruction.
    Call {
        info: Box<CallInfo>,
    },

    /// A machine indirect-call instruction.
    CallInd {
        info: Box<CallIndInfo>,
    },

    /// Load an inline symbol reference.
    LoadExtName {
        rt: Writable<Reg>,
        name: Box<ExternalName>,
        offset: i32,
    },

    /// A return instruction, which is encoded as `bx lr`.
    Ret,

    /// An unconditional branch.
    Jump {
        dest: BranchTarget,
    },

    /// A conditional branch.
    CondBr {
        taken: BranchTarget,
        not_taken: BranchTarget,
        cond: Cond,
    },

    /// An indirect branch through a register, augmented with set of all
    /// possible successors.
    IndirectBr {
        rm: Reg,
        targets: Vec<MachLabel>,
    },

    /// A conditional trap: execute a `udf` if the condition is true. This is
    /// one VCode instruction because it uses embedded control flow; it is
    /// logically a single-in, single-out region, but needs to appear as one
    /// unit to the register allocator.
    TrapIf {
        cond: Cond,
        trap_info: TrapCode,
    },

    /// An instruction guaranteed to always be undefined and to trigger an illegal instruction at
    /// runtime.
    Udf {
        trap_info: TrapCode,
    },

    /// A "breakpoint" instruction, used for e.g. traps and debug breakpoints.
    Bkpt,

    /// Marker, no-op in generated code: SP "virtual offset" is adjusted.
    VirtualSPOffsetAdj {
        offset: i64,
    },

    /// A placeholder instruction, generating no code, meaning that a function epilogue must be
    /// inserted there.
    EpiloguePlaceholder,
}

/// An instruction inside an it block.
#[derive(Clone, Debug)]
pub struct CondInst {
    inst: Inst,
    // In which case execute the instruction:
    // true => when it condition is met
    // false => otherwise.
    then: bool,
}

impl CondInst {
    pub fn new(inst: Inst, then: bool) -> Self {
        match inst {
            Inst::It { .. }
            | Inst::Ret { .. }
            | Inst::Jump { .. }
            | Inst::CondBr { .. }
            | Inst::TrapIf { .. }
            | Inst::EpiloguePlaceholder { .. }
            | Inst::LoadExtName { .. } => panic!("Instruction {:?} cannot occur in it block", inst),
            _ => Self { inst, then },
        }
    }
}

impl Inst {
    /// Create a move instruction.
    pub fn mov(to_reg: Writable<Reg>, from_reg: Reg) -> Inst {
        Inst::Mov {
            rd: to_reg,
            rm: from_reg,
        }
    }

    /// Create an instruction that loads a constant.
    pub fn load_constant(rd: Writable<Reg>, value: u32) -> SmallVec<[Inst; 4]> {
        let mut insts = smallvec![];
        let imm_lo = (value & 0xffff) as u16;
        let imm_hi = (value >> 16) as u16;

        if imm_lo != 0 || imm_hi == 0 {
            // imm_lo == 0 && imm_hi == 0 => we have to overwrite reg value with 0
            insts.push(Inst::MovImm16 { rd, imm16: imm_lo });
        }
        if imm_hi != 0 {
            insts.push(Inst::Movt { rd, imm16: imm_hi });
        }

        insts
    }

    /// Generic constructor for a load (zero-extending where appropriate).
    pub fn gen_load(into_reg: Writable<Reg>, mem: AMode, ty: Type) -> Inst {
        assert!(ty.bits() <= 32);
        // Load 8 bits for B1.
        let bits = std::cmp::max(ty.bits(), 8) as u8;

        Inst::Load {
            rt: into_reg,
            mem,
            bits,
            sign_extend: false,
        }
    }

    /// Generic constructor for a store.
    pub fn gen_store(from_reg: Reg, mem: AMode, ty: Type) -> Inst {
        assert!(ty.bits() <= 32);
        // Store 8 bits for B1.
        let bits = std::cmp::max(ty.bits(), 8) as u8;

        Inst::Store {
            rt: from_reg,
            mem,
            bits,
        }
    }
}

//=============================================================================
// Instructions: get_regs

fn memarg_regs(memarg: &AMode, collector: &mut RegUsageCollector) {
    match memarg {
        &AMode::RegReg(rn, rm, ..) => {
            collector.add_use(rn);
            collector.add_use(rm);
        }
        &AMode::RegOffset12(rn, ..) | &AMode::RegOffset(rn, _) => {
            collector.add_use(rn);
        }
        &AMode::SPOffset(..) | &AMode::NominalSPOffset(..) => {
            collector.add_use(sp_reg());
        }
        &AMode::FPOffset(..) => {
            collector.add_use(fp_reg());
        }
        &AMode::PCRel(_) => {}
    }
}

fn arm32_get_regs(inst: &Inst, collector: &mut RegUsageCollector) {
    match inst {
        &Inst::Nop0
        | &Inst::Nop2
        | &Inst::Ret
        | &Inst::VirtualSPOffsetAdj { .. }
        | &Inst::EpiloguePlaceholder
        | &Inst::Jump { .. }
        | &Inst::CondBr { .. }
        | &Inst::Bkpt
        | &Inst::Udf { .. }
        | &Inst::TrapIf { .. } => {}
        &Inst::AluRRR { rd, rn, rm, .. } => {
            collector.add_def(rd);
            collector.add_use(rn);
            collector.add_use(rm);
        }
        &Inst::AluRRRShift { rd, rn, rm, .. } => {
            collector.add_def(rd);
            collector.add_use(rn);
            collector.add_use(rm);
        }
        &Inst::AluRRShift { rd, rm, .. } => {
            collector.add_def(rd);
            collector.add_use(rm);
        }
        &Inst::AluRRRR {
            rd_hi,
            rd_lo,
            rn,
            rm,
            ..
        } => {
            collector.add_def(rd_hi);
            collector.add_def(rd_lo);
            collector.add_use(rn);
            collector.add_use(rm);
        }
        &Inst::AluRRImm12 { rd, rn, .. } => {
            collector.add_def(rd);
            collector.add_use(rn);
        }
        &Inst::AluRRImm8 { rd, rn, .. } => {
            collector.add_def(rd);
            collector.add_use(rn);
        }
        &Inst::AluRImm8 { rd, .. } => {
            collector.add_def(rd);
        }
        &Inst::BitOpRR { rd, rm, .. } => {
            collector.add_def(rd);
            collector.add_use(rm);
        }
        &Inst::Mov { rd, rm, .. } => {
            collector.add_def(rd);
            collector.add_use(rm);
        }
        &Inst::MovImm16 { rd, .. } => {
            collector.add_def(rd);
        }
        &Inst::Movt { rd, .. } => {
            collector.add_def(rd);
        }
        &Inst::Cmp { rn, rm } => {
            collector.add_use(rn);
            collector.add_use(rm);
        }
        &Inst::CmpImm8 { rn, .. } => {
            collector.add_use(rn);
        }
        &Inst::Store { rt, ref mem, .. } => {
            collector.add_use(rt);
            memarg_regs(mem, collector);
        }
        &Inst::Load { rt, ref mem, .. } => {
            collector.add_def(rt);
            memarg_regs(mem, collector);
        }
        &Inst::LoadAddr { rd, mem: _ } => {
            collector.add_def(rd);
        }
        &Inst::Extend { rd, rm, .. } => {
            collector.add_def(rd);
            collector.add_use(rm);
        }
        &Inst::It { ref insts, .. } => {
            for inst in insts.iter() {
                arm32_get_regs(&inst.inst, collector);
            }
        }
        &Inst::Push { ref reg_list } => {
            for reg in reg_list {
                collector.add_use(*reg);
            }
        }
        &Inst::Pop { ref reg_list } => {
            for reg in reg_list {
                collector.add_def(*reg);
            }
        }
        &Inst::Call { ref info, .. } => {
            collector.add_uses(&*info.uses);
            collector.add_defs(&*info.defs);
        }
        &Inst::CallInd { ref info, .. } => {
            collector.add_uses(&*info.uses);
            collector.add_defs(&*info.defs);
            collector.add_use(info.rm);
        }
        &Inst::LoadExtName { rt, .. } => {
            collector.add_def(rt);
        }
        &Inst::IndirectBr { rm, .. } => {
            collector.add_use(rm);
        }
    }
}

//=============================================================================
// Instructions: map_regs

fn arm32_map_regs<RUM: RegUsageMapper>(inst: &mut Inst, mapper: &RUM) {
    fn map_use<RUM: RegUsageMapper>(m: &RUM, r: &mut Reg) {
        if r.is_virtual() {
            let new = m.get_use(r.to_virtual_reg()).unwrap().to_reg();
            *r = new;
        }
    }

    fn map_def<RUM: RegUsageMapper>(m: &RUM, r: &mut Writable<Reg>) {
        if r.to_reg().is_virtual() {
            let new = m.get_def(r.to_reg().to_virtual_reg()).unwrap().to_reg();
            *r = Writable::from_reg(new);
        }
    }

    fn map_mod<RUM: RegUsageMapper>(m: &RUM, r: &mut Writable<Reg>) {
        if r.to_reg().is_virtual() {
            let new = m.get_mod(r.to_reg().to_virtual_reg()).unwrap().to_reg();
            *r = Writable::from_reg(new);
        }
    }

    fn map_mem<RUM: RegUsageMapper>(m: &RUM, mem: &mut AMode) {
        match mem {
            &mut AMode::RegReg(ref mut rn, ref mut rm, ..) => {
                map_use(m, rn);
                map_use(m, rm);
            }
            &mut AMode::RegOffset12(ref mut rn, ..) | &mut AMode::RegOffset(ref mut rn, ..) => {
                map_use(m, rn)
            }
            &mut AMode::SPOffset(..)
            | &mut AMode::FPOffset(..)
            | &mut AMode::NominalSPOffset(..)
            | &mut AMode::PCRel(_) => {}
        };
    }

    match inst {
        &mut Inst::Nop0
        | &mut Inst::Nop2
        | &mut Inst::Ret
        | &mut Inst::VirtualSPOffsetAdj { .. }
        | &mut Inst::EpiloguePlaceholder
        | &mut Inst::Jump { .. }
        | &mut Inst::CondBr { .. }
        | &mut Inst::Bkpt
        | &mut Inst::Udf { .. }
        | &mut Inst::TrapIf { .. } => {}
        &mut Inst::AluRRR {
            ref mut rd,
            ref mut rn,
            ref mut rm,
            ..
        } => {
            map_def(mapper, rd);
            map_use(mapper, rn);
            map_use(mapper, rm);
        }
        &mut Inst::AluRRRShift {
            ref mut rd,
            ref mut rn,
            ref mut rm,
            ..
        } => {
            map_def(mapper, rd);
            map_use(mapper, rn);
            map_use(mapper, rm);
        }
        &mut Inst::AluRRShift {
            ref mut rd,
            ref mut rm,
            ..
        } => {
            map_def(mapper, rd);
            map_use(mapper, rm);
        }
        &mut Inst::AluRRRR {
            ref mut rd_hi,
            ref mut rd_lo,
            ref mut rn,
            ref mut rm,
            ..
        } => {
            map_def(mapper, rd_hi);
            map_def(mapper, rd_lo);
            map_use(mapper, rn);
            map_use(mapper, rm);
        }
        &mut Inst::AluRRImm12 {
            ref mut rd,
            ref mut rn,
            ..
        } => {
            map_def(mapper, rd);
            map_use(mapper, rn);
        }
        &mut Inst::AluRRImm8 {
            ref mut rd,
            ref mut rn,
            ..
        } => {
            map_def(mapper, rd);
            map_use(mapper, rn);
        }
        &mut Inst::AluRImm8 { ref mut rd, .. } => {
            map_def(mapper, rd);
        }
        &mut Inst::BitOpRR {
            ref mut rd,
            ref mut rm,
            ..
        } => {
            map_def(mapper, rd);
            map_use(mapper, rm);
        }
        &mut Inst::Mov {
            ref mut rd,
            ref mut rm,
            ..
        } => {
            map_def(mapper, rd);
            map_use(mapper, rm);
        }
        &mut Inst::MovImm16 { ref mut rd, .. } => {
            map_def(mapper, rd);
        }
        &mut Inst::Movt { ref mut rd, .. } => {
            map_def(mapper, rd);
        }
        &mut Inst::Cmp {
            ref mut rn,
            ref mut rm,
        } => {
            map_use(mapper, rn);
            map_use(mapper, rm);
        }
        &mut Inst::CmpImm8 { ref mut rn, .. } => {
            map_use(mapper, rn);
        }
        &mut Inst::Store {
            ref mut rt,
            ref mut mem,
            ..
        } => {
            map_use(mapper, rt);
            map_mem(mapper, mem);
        }
        &mut Inst::Load {
            ref mut rt,
            ref mut mem,
            ..
        } => {
            map_def(mapper, rt);
            map_mem(mapper, mem);
        }
        &mut Inst::LoadAddr {
            ref mut rd,
            ref mut mem,
        } => {
            map_def(mapper, rd);
            map_mem(mapper, mem);
        }
        &mut Inst::Extend {
            ref mut rd,
            ref mut rm,
            ..
        } => {
            map_def(mapper, rd);
            map_use(mapper, rm);
        }
        &mut Inst::It { ref mut insts, .. } => {
            for inst in insts.iter_mut() {
                arm32_map_regs(&mut inst.inst, mapper);
            }
        }
        &mut Inst::Push { ref mut reg_list } => {
            for reg in reg_list {
                map_use(mapper, reg);
            }
        }
        &mut Inst::Pop { ref mut reg_list } => {
            for reg in reg_list {
                map_def(mapper, reg);
            }
        }
        &mut Inst::Call { ref mut info } => {
            for r in info.uses.iter_mut() {
                map_use(mapper, r);
            }
            for r in info.defs.iter_mut() {
                map_def(mapper, r);
            }
        }
        &mut Inst::CallInd { ref mut info, .. } => {
            for r in info.uses.iter_mut() {
                map_use(mapper, r);
            }
            for r in info.defs.iter_mut() {
                map_def(mapper, r);
            }
            map_use(mapper, &mut info.rm);
        }
        &mut Inst::LoadExtName { ref mut rt, .. } => {
            map_def(mapper, rt);
        }
        &mut Inst::IndirectBr { ref mut rm, .. } => {
            map_use(mapper, rm);
        }
    }
}

//=============================================================================
// Instructions: misc functions and external interface

impl MachInst for Inst {
    type LabelUse = LabelUse;

    fn get_regs(&self, collector: &mut RegUsageCollector) {
        arm32_get_regs(self, collector)
    }

    fn map_regs<RUM: RegUsageMapper>(&mut self, mapper: &RUM) {
        arm32_map_regs(self, mapper);
    }

    fn is_move(&self) -> Option<(Writable<Reg>, Reg)> {
        match self {
            &Inst::Mov { rd, rm } => Some((rd, rm)),
            _ => None,
        }
    }

    fn is_epilogue_placeholder(&self) -> bool {
        if let Inst::EpiloguePlaceholder = self {
            true
        } else {
            false
        }
    }

    fn is_term<'a>(&'a self) -> MachTerminator<'a> {
        match self {
            &Inst::Ret | &Inst::EpiloguePlaceholder => MachTerminator::Ret,
            &Inst::Jump { dest } => MachTerminator::Uncond(dest.as_label().unwrap()),
            &Inst::CondBr {
                taken, not_taken, ..
            } => MachTerminator::Cond(taken.as_label().unwrap(), not_taken.as_label().unwrap()),
            &Inst::IndirectBr { ref targets, .. } => MachTerminator::Indirect(&targets[..]),
            _ => MachTerminator::None,
        }
    }

    fn gen_move(to_reg: Writable<Reg>, from_reg: Reg, _ty: Type) -> Inst {
        assert_eq!(from_reg.get_class(), RegClass::I32);
        assert_eq!(to_reg.to_reg().get_class(), from_reg.get_class());

        Inst::mov(to_reg, from_reg)
    }

    fn gen_constant<F: FnMut(RegClass, Type) -> Writable<Reg>>(
        to_reg: Writable<Reg>,
        value: DataValue,
        _alloc_tmp: F,
    ) -> SmallVec<[Inst; 4]> {
        match value.ty() {
            B1 | I8 | B8 | I16 | B16 | I32 | B32 => {
                let v = value
                    .to_integer()
                    .expect("a value that can be converted to a scalar integer");
                if v >= (1 << 32) || v < -(1 << 32) {
                    panic!("Cannot load constant value {}", value)
                }
                Inst::load_constant(to_reg, value as u32)
            }
            _ => unimplemented!(),
        }
    }

    fn gen_zero_len_nop() -> Inst {
        Inst::Nop0
    }

    fn gen_nop(preferred_size: usize) -> Inst {
        assert!(preferred_size >= 2);
        Inst::Nop2
    }

    fn maybe_direct_reload(&self, _reg: VirtualReg, _slot: SpillSlot) -> Option<Inst> {
        None
    }

    fn rc_for_type(ty: Type) -> CodegenResult<RegClass> {
        match ty {
            I8 | I16 | I32 | B1 | B8 | B16 | B32 => Ok(RegClass::I32),
            IFLAGS => Ok(RegClass::I32),
            _ => Err(CodegenError::Unsupported(format!(
                "Unexpected SSA-value type: {}",
                ty
            ))),
        }
    }

    fn gen_jump(target: MachLabel) -> Inst {
        Inst::Jump {
            dest: BranchTarget::Label(target),
        }
    }

    fn reg_universe(_flags: &settings::Flags) -> RealRegUniverse {
        create_reg_universe()
    }

    fn worst_case_size() -> CodeOffset {
        // It inst with four 32-bit instructions
        2 + 4 * 4
    }

    fn ref_type_regclass(_: &settings::Flags) -> RegClass {
        RegClass::I32
    }
}

//=============================================================================
// Pretty-printing of instructions.

fn mem_finalize_for_show(
    mem: &AMode,
    mb_rru: Option<&RealRegUniverse>,
    state: &EmitState,
) -> (String, AMode) {
    let (mem_insts, mem) = mem_finalize(mem, state);
    let mut mem_str = mem_insts
        .into_iter()
        .map(|inst| inst.show_rru(mb_rru))
        .collect::<Vec<_>>()
        .join(" ; ");
    if !mem_str.is_empty() {
        mem_str += " ; ";
    }

    (mem_str, mem)
}

impl PrettyPrint for Inst {
    fn show_rru(&self, mb_rru: Option<&RealRegUniverse>) -> String {
        self.pretty_print(mb_rru, &mut EmitState::default())
    }
}

impl Inst {
    fn print_with_state(&self, mb_rru: Option<&RealRegUniverse>, state: &mut EmitState) -> String {
        fn op_name(alu_op: ALUOp) -> &'static str {
            match alu_op {
                ALUOp::Add => "add",
                ALUOp::Adds => "adds",
                ALUOp::Adc => "adc",
                ALUOp::Adcs => "adcs",
                ALUOp::Qadd => "qadd",
                ALUOp::Sub => "sub",
                ALUOp::Subs => "subs",
                ALUOp::Sbc => "sbc",
                ALUOp::Sbcs => "sbcs",
                ALUOp::Rsb => "rsb",
                ALUOp::Qsub => "qsub",
                ALUOp::Mul => "mul",
                ALUOp::Smull => "smull",
                ALUOp::Umull => "umull",
                ALUOp::Udiv => "udiv",
                ALUOp::Sdiv => "sdiv",
                ALUOp::And => "and",
                ALUOp::Orr => "orr",
                ALUOp::Orn => "orn",
                ALUOp::Eor => "eor",
                ALUOp::Bic => "bic",
                ALUOp::Lsl => "lsl",
                ALUOp::Lsr => "lsr",
                ALUOp::Asr => "asr",
                ALUOp::Ror => "ror",
            }
        }

        fn reg_shift_str(
            shift: &Option<ShiftOpAndAmt>,
            mb_rru: Option<&RealRegUniverse>,
        ) -> String {
            if let Some(ref shift) = shift {
                format!(", {}", shift.show_rru(mb_rru))
            } else {
                "".to_string()
            }
        }

        match self {
            &Inst::Nop0 => "nop-zero-len".to_string(),
            &Inst::Nop2 => "nop".to_string(),
            &Inst::AluRRR { alu_op, rd, rn, rm } => {
                let op = op_name(alu_op);
                let rd = rd.show_rru(mb_rru);
                let rn = rn.show_rru(mb_rru);
                let rm = rm.show_rru(mb_rru);
                format!("{} {}, {}, {}", op, rd, rn, rm)
            }
            &Inst::AluRRRShift {
                alu_op,
                rd,
                rn,
                rm,
                ref shift,
            } => {
                let op = op_name(alu_op);
                let rd = rd.show_rru(mb_rru);
                let rn = rn.show_rru(mb_rru);
                let rm = rm.show_rru(mb_rru);
                let shift = reg_shift_str(shift, mb_rru);
                format!("{} {}, {}, {}{}", op, rd, rn, rm, shift)
            }
            &Inst::AluRRShift {
                alu_op,
                rd,
                rm,
                ref shift,
            } => {
                let op = match alu_op {
                    ALUOp1::Mvn => "mvn",
                    ALUOp1::Mov => "mov",
                };
                let rd = rd.show_rru(mb_rru);
                let rm = rm.show_rru(mb_rru);
                let shift = reg_shift_str(shift, mb_rru);
                format!("{} {}, {}{}", op, rd, rm, shift)
            }
            &Inst::AluRRRR {
                alu_op,
                rd_hi,
                rd_lo,
                rn,
                rm,
            } => {
                let op = op_name(alu_op);
                let rd_hi = rd_hi.show_rru(mb_rru);
                let rd_lo = rd_lo.show_rru(mb_rru);
                let rn = rn.show_rru(mb_rru);
                let rm = rm.show_rru(mb_rru);
                format!("{} {}, {}, {}, {}", op, rd_lo, rd_hi, rn, rm)
            }
            &Inst::AluRRImm12 {
                alu_op,
                rd,
                rn,
                imm12,
            } => {
                let op = op_name(alu_op);
                let rd = rd.show_rru(mb_rru);
                let rn = rn.show_rru(mb_rru);
                let imm = imm12.show_rru(mb_rru);
                format!("{} {}, {}, {}", op, rd, rn, imm)
            }
            &Inst::AluRRImm8 {
                alu_op,
                rd,
                rn,
                imm8,
            } => {
                let op = op_name(alu_op);
                let rd = rd.show_rru(mb_rru);
                let rn = rn.show_rru(mb_rru);
                let imm = imm8.show_rru(mb_rru);
                format!("{} {}, {}, {}", op, rd, rn, imm)
            }
            &Inst::AluRImm8 { alu_op, rd, imm8 } => {
                let op = match alu_op {
                    ALUOp1::Mvn => "mvn",
                    ALUOp1::Mov => "mov",
                };
                let rd = rd.show_rru(mb_rru);
                let imm = imm8.show_rru(mb_rru);
                format!("{} {}, {}", op, rd, imm)
            }
            &Inst::BitOpRR { bit_op, rd, rm } => {
                let op = match bit_op {
                    BitOp::Rbit => "rbit",
                    BitOp::Rev => "rev",
                    BitOp::Clz => "clz",
                };
                let rd = rd.show_rru(mb_rru);
                let rm = rm.show_rru(mb_rru);
                format!("{} {}, {}", op, rd, rm)
            }
            &Inst::Mov { rd, rm } => {
                let rd = rd.show_rru(mb_rru);
                let rm = rm.show_rru(mb_rru);
                format!("mov {}, {}", rd, rm)
            }
            &Inst::MovImm16 { rd, imm16 } => {
                let rd = rd.show_rru(mb_rru);
                format!("mov {}, #{}", rd, imm16)
            }
            &Inst::Movt { rd, imm16 } => {
                let rd = rd.show_rru(mb_rru);
                format!("movt {}, #{}", rd, imm16)
            }
            &Inst::Cmp { rn, rm } => {
                let rn = rn.show_rru(mb_rru);
                let rm = rm.show_rru(mb_rru);
                format!("cmp {}, {}", rn, rm)
            }
            &Inst::CmpImm8 { rn, imm8 } => {
                let rn = rn.show_rru(mb_rru);
                format!("cmp {}, #{}", rn, imm8)
            }
            &Inst::Store {
                rt, ref mem, bits, ..
            } => {
                let op = match bits {
                    32 => "str",
                    16 => "strh",
                    8 => "strb",
                    _ => panic!("Invalid bit amount {}", bits),
                };
                let rt = rt.show_rru(mb_rru);
                let (mem_str, mem) = mem_finalize_for_show(mem, mb_rru, state);
                let mem = mem.show_rru(mb_rru);
                format!("{}{} {}, {}", mem_str, op, rt, mem)
            }
            &Inst::Load {
                rt,
                ref mem,
                bits,
                sign_extend,
                ..
            } => {
                let op = match (bits, sign_extend) {
                    (32, _) => "ldr",
                    (16, true) => "ldrsh",
                    (16, false) => "ldrh",
                    (8, true) => "ldrsb",
                    (8, false) => "ldrb",
                    (_, _) => panic!("Invalid bit amount {}", bits),
                };
                let rt = rt.show_rru(mb_rru);
                let (mem_str, mem) = mem_finalize_for_show(mem, mb_rru, state);
                let mem = mem.show_rru(mb_rru);
                format!("{}{} {}, {}", mem_str, op, rt, mem)
            }
            &Inst::LoadAddr { rd, ref mem } => {
                let mut ret = String::new();
                let (mem_insts, mem) = mem_finalize(mem, state);
                for inst in mem_insts.into_iter() {
                    ret.push_str(&inst.show_rru(mb_rru));
                }
                let inst = match mem {
                    AMode::RegReg(rn, rm, shift) => {
                        let shift = u32::from(shift);
                        let shift_amt = ShiftOpShiftImm::maybe_from_shift(shift).unwrap();
                        let shift = ShiftOpAndAmt::new(ShiftOp::LSL, shift_amt);
                        Inst::AluRRRShift {
                            alu_op: ALUOp::Add,
                            rd,
                            rn,
                            rm,
                            shift: Some(shift),
                        }
                    }
                    AMode::RegOffset12(reg, imm12) => Inst::AluRRImm12 {
                        alu_op: ALUOp::Add,
                        rd,
                        rn: reg,
                        imm12,
                    },
                    _ => unreachable!(),
                };
                ret.push_str(&inst.show_rru(mb_rru));
                ret
            }
            &Inst::Extend {
                rd,
                rm,
                from_bits,
                signed,
            } => {
                let op = match (from_bits, signed) {
                    (16, true) => "sxth",
                    (16, false) => "uxth",
                    (8, true) => "sxtb",
                    (8, false) => "uxtb",
                    _ => panic!("Unsupported extend case: {:?}", self),
                };
                let rd = rd.show_rru(mb_rru);
                let rm = rm.show_rru(mb_rru);
                format!("{} {}, {}", op, rd, rm)
            }
            &Inst::It { cond, ref insts } => {
                let te: String = insts
                    .iter()
                    .skip(1)
                    .map(|i| if i.then { "t" } else { "e" })
                    .collect();
                let cond = cond.show_rru(mb_rru);
                let mut ret = format!("it{} {}", te, cond);
                for inst in insts.into_iter() {
                    ret.push_str(" ; ");
                    ret.push_str(&inst.inst.show_rru(mb_rru));
                }
                ret
            }
            &Inst::Push { ref reg_list } => {
                assert!(!reg_list.is_empty());
                let first_reg = reg_list[0].show_rru(mb_rru);
                let regs: String = reg_list
                    .iter()
                    .skip(1)
                    .map(|r| [",", &r.show_rru(mb_rru)].join(" "))
                    .collect();
                format!("push {{{}{}}}", first_reg, regs)
            }
            &Inst::Pop { ref reg_list } => {
                assert!(!reg_list.is_empty());
                let first_reg = reg_list[0].show_rru(mb_rru);
                let regs: String = reg_list
                    .iter()
                    .skip(1)
                    .map(|r| [",", &r.show_rru(mb_rru)].join(" "))
                    .collect();
                format!("pop {{{}{}}}", first_reg, regs)
            }
            &Inst::Call { .. } => format!("bl 0"),
            &Inst::CallInd { ref info, .. } => {
                let rm = info.rm.show_rru(mb_rru);
                format!("blx {}", rm)
            }
            &Inst::LoadExtName {
                rt,
                ref name,
                offset,
            } => {
                let rt = rt.show_rru(mb_rru);
                format!("ldr {}, [pc, #4] ; b 4 ; data {:?} + {}", rt, name, offset)
            }
            &Inst::Ret => "bx lr".to_string(),
            &Inst::VirtualSPOffsetAdj { offset } => format!("virtual_sp_offset_adjust {}", offset),
            &Inst::EpiloguePlaceholder => "epilogue placeholder".to_string(),
            &Inst::Jump { ref dest } => {
                let dest = dest.show_rru(mb_rru);
                format!("b {}", dest)
            }
            &Inst::CondBr {
                ref taken,
                ref not_taken,
                ref cond,
            } => {
                let taken = taken.show_rru(mb_rru);
                let not_taken = not_taken.show_rru(mb_rru);
                let c = cond.show_rru(mb_rru);
                format!("b{} {} ; b {}", c, taken, not_taken)
            }
            &Inst::IndirectBr { rm, .. } => {
                let rm = rm.show_rru(mb_rru);
                format!("bx {}", rm)
            }
            &Inst::Udf { .. } => "udf #0".to_string(),
            &Inst::Bkpt => "bkpt #0".to_string(),
            &Inst::TrapIf { cond, .. } => {
                let c = cond.invert().show_rru(mb_rru);
                format!("b{} 2 ; udf #0", c)
            }
        }
    }
}

//=============================================================================
// Label fixups and jump veneers.

/// Different forms of label references for different instruction formats.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LabelUse {
    /// 20-bit branch offset used by 32-bit conditional jumps.
    Branch20,

    /// 24-bit branch offset used by 32-bit uncoditional jump instruction.
    Branch24,
}

impl MachInstLabelUse for LabelUse {
    /// Alignment for veneer code. Every instruction must be 4-byte-aligned.
    const ALIGN: CodeOffset = 2;

    // Branches range:
    // 20-bit sign-extended immediate gives us range [-(2^19), 2^19 - 1].
    // Left-shifted by 1 => [-(2^20), 2^20 - 2].
    // PC is start of this instruction + 4 bytes => [-(2^20) + 4, 2^20 + 2].
    // Likewise for Branch24.

    /// Maximum PC-relative range (positive), inclusive.
    fn max_pos_range(self) -> CodeOffset {
        match self {
            LabelUse::Branch20 => (1 << 20) + 2,
            LabelUse::Branch24 => (1 << 24) + 2,
        }
    }

    /// Maximum PC-relative range (negative).
    fn max_neg_range(self) -> CodeOffset {
        match self {
            LabelUse::Branch20 => (1 << 20) - 4,
            LabelUse::Branch24 => (1 << 24) - 4,
        }
    }

    /// Size of window into code needed to do the patch.
    fn patch_size(self) -> CodeOffset {
        4
    }

    /// Perform the patch.
    fn patch(self, buffer: &mut [u8], use_offset: CodeOffset, label_offset: CodeOffset) {
        let off = (label_offset as i64) - (use_offset as i64);
        debug_assert!(off <= self.max_pos_range() as i64);
        debug_assert!(off >= -(self.max_neg_range() as i64));
        let off = off - 4;
        match self {
            LabelUse::Branch20 => {
                let off = off as u32 >> 1;
                let imm11 = (off & 0x7ff) as u16;
                let imm6 = ((off >> 11) & 0x3f) as u16;
                let j1 = ((off >> 17) & 0x1) as u16;
                let j2 = ((off >> 18) & 0x1) as u16;
                let s = ((off >> 19) & 0x1) as u16;
                let insn_fst = u16::from_le_bytes([buffer[0], buffer[1]]);
                let insn_fst = (insn_fst & !0x43f) | imm6 | (s << 10);
                let insn_snd = u16::from_le_bytes([buffer[2], buffer[3]]);
                let insn_snd = (insn_snd & !0x2fff) | imm11 | (j2 << 11) | (j1 << 13);
                buffer[0..2].clone_from_slice(&u16::to_le_bytes(insn_fst));
                buffer[2..4].clone_from_slice(&u16::to_le_bytes(insn_snd));
            }
            LabelUse::Branch24 => {
                let off = off as u32 >> 1;
                let imm11 = (off & 0x7ff) as u16;
                let imm10 = ((off >> 11) & 0x3ff) as u16;
                let s = ((off >> 23) & 0x1) as u16;
                let j1 = (((off >> 22) & 0x1) as u16 ^ s) ^ 0x1;
                let j2 = (((off >> 21) & 0x1) as u16 ^ s) ^ 0x1;
                let insn_fst = u16::from_le_bytes([buffer[0], buffer[1]]);
                let insn_fst = (insn_fst & !0x07ff) | imm10 | (s << 10);
                let insn_snd = u16::from_le_bytes([buffer[2], buffer[3]]);
                let insn_snd = (insn_snd & !0x2fff) | imm11 | (j2 << 11) | (j1 << 13);
                buffer[0..2].clone_from_slice(&u16::to_le_bytes(insn_fst));
                buffer[2..4].clone_from_slice(&u16::to_le_bytes(insn_snd));
            }
        }
    }

    fn supports_veneer(self) -> bool {
        false
    }

    fn veneer_size(self) -> CodeOffset {
        0
    }

    fn generate_veneer(
        self,
        _buffer: &mut [u8],
        _veneer_offset: CodeOffset,
    ) -> (CodeOffset, LabelUse) {
        panic!("Veneer not supported yet.")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patch_branch20() {
        let label_use = LabelUse::Branch20;
        let mut buffer = 0x8000_f000_u32.to_le_bytes(); // beq
        let use_offset: CodeOffset = 0;
        let label_offset: CodeOffset = label_use.max_pos_range();
        label_use.patch(&mut buffer, use_offset, label_offset);
        assert_eq!(u16::from_le_bytes([buffer[0], buffer[1]]), 0xf03f);
        assert_eq!(u16::from_le_bytes([buffer[2], buffer[3]]), 0xafff);

        let mut buffer = 0x8000_f000_u32.to_le_bytes(); // beq
        let use_offset = label_use.max_neg_range();
        let label_offset: CodeOffset = 0;
        label_use.patch(&mut buffer, use_offset, label_offset);
        assert_eq!(u16::from_le_bytes([buffer[0], buffer[1]]), 0xf400);
        assert_eq!(u16::from_le_bytes([buffer[2], buffer[3]]), 0x8000);
    }

    #[test]
    fn patch_branch24() {
        let label_use = LabelUse::Branch24;
        let mut buffer = 0x9000_f000_u32.to_le_bytes(); // b
        let use_offset: CodeOffset = 0;
        let label_offset: CodeOffset = label_use.max_pos_range();
        label_use.patch(&mut buffer, use_offset, label_offset);
        assert_eq!(u16::from_le_bytes([buffer[0], buffer[1]]), 0xf3ff);
        assert_eq!(u16::from_le_bytes([buffer[2], buffer[3]]), 0x97ff);

        let mut buffer = 0x9000_f000_u32.to_le_bytes(); // b
        let use_offset = label_use.max_neg_range();
        let label_offset: CodeOffset = 0;
        label_use.patch(&mut buffer, use_offset, label_offset);
        assert_eq!(u16::from_le_bytes([buffer[0], buffer[1]]), 0xf400);
        assert_eq!(u16::from_le_bytes([buffer[2], buffer[3]]), 0x9000);
    }
}
