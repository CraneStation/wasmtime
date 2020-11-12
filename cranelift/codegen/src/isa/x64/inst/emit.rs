use crate::binemit::{Addend, Reloc};
use crate::ir::immediates::{Ieee32, Ieee64};
use crate::ir::TrapCode;
use crate::isa::x64::inst::args::*;
use crate::isa::x64::inst::*;
use crate::machinst::{inst_common, MachBuffer, MachInstEmit, MachLabel};
use core::convert::TryInto;
use log::debug;
use regalloc::{Reg, RegClass, Writable};

fn low8_will_sign_extend_to_64(x: u32) -> bool {
    let xs = (x as i32) as i64;
    xs == ((xs << 56) >> 56)
}

fn low8_will_sign_extend_to_32(x: u32) -> bool {
    let xs = x as i32;
    xs == ((xs << 24) >> 24)
}

//=============================================================================
// Instructions and subcomponents: emission

// For all of the routines that take both a memory-or-reg operand (sometimes
// called "E" in the Intel documentation) and a reg-only operand ("G" in
// Intelese), the order is always G first, then E.
//
// "enc" in the following means "hardware register encoding number".

#[inline(always)]
fn encode_modrm(m0d: u8, enc_reg_g: u8, rm_e: u8) -> u8 {
    debug_assert!(m0d < 4);
    debug_assert!(enc_reg_g < 8);
    debug_assert!(rm_e < 8);
    ((m0d & 3) << 6) | ((enc_reg_g & 7) << 3) | (rm_e & 7)
}

#[inline(always)]
fn encode_sib(shift: u8, enc_index: u8, enc_base: u8) -> u8 {
    debug_assert!(shift < 4);
    debug_assert!(enc_index < 8);
    debug_assert!(enc_base < 8);
    ((shift & 3) << 6) | ((enc_index & 7) << 3) | (enc_base & 7)
}

/// Get the encoding number of a GPR.
#[inline(always)]
fn int_reg_enc(reg: Reg) -> u8 {
    debug_assert!(reg.is_real());
    debug_assert_eq!(reg.get_class(), RegClass::I64);
    reg.get_hw_encoding()
}

/// Get the encoding number of any register.
#[inline(always)]
fn reg_enc(reg: Reg) -> u8 {
    debug_assert!(reg.is_real());
    reg.get_hw_encoding()
}

/// A small bit field to record a REX prefix specification:
/// - bit 0 set to 1 indicates REX.W must be 0 (cleared).
/// - bit 1 set to 1 indicates the REX prefix must always be emitted.
#[repr(transparent)]
#[derive(Clone, Copy)]
struct RexFlags(u8);

impl RexFlags {
    /// By default, set the W field, and don't always emit.
    #[inline(always)]
    fn set_w() -> Self {
        Self(0)
    }
    /// Creates a new RexPrefix for which the REX.W bit will be cleared.
    #[inline(always)]
    fn clear_w() -> Self {
        Self(1)
    }

    #[inline(always)]
    fn always_emit(&mut self) -> &mut Self {
        self.0 = self.0 | 2;
        self
    }

    #[inline(always)]
    fn must_clear_w(&self) -> bool {
        (self.0 & 1) != 0
    }
    #[inline(always)]
    fn must_always_emit(&self) -> bool {
        (self.0 & 2) != 0
    }

    #[inline(always)]
    fn emit_two_op(&self, sink: &mut MachBuffer<Inst>, enc_g: u8, enc_e: u8) {
        let w = if self.must_clear_w() { 0 } else { 1 };
        let r = (enc_g >> 3) & 1;
        let x = 0;
        let b = (enc_e >> 3) & 1;
        let rex = 0x40 | (w << 3) | (r << 2) | (x << 1) | b;
        if rex != 0x40 || self.must_always_emit() {
            sink.put1(rex);
        }
    }

    #[inline(always)]
    fn emit_three_op(&self, sink: &mut MachBuffer<Inst>, enc_g: u8, enc_index: u8, enc_base: u8) {
        let w = if self.must_clear_w() { 0 } else { 1 };
        let r = (enc_g >> 3) & 1;
        let x = (enc_index >> 3) & 1;
        let b = (enc_base >> 3) & 1;
        let rex = 0x40 | (w << 3) | (r << 2) | (x << 1) | b;
        if rex != 0x40 || self.must_always_emit() {
            sink.put1(rex);
        }
    }
}

/// We may need to include one or more legacy prefix bytes before the REX prefix.  This enum
/// covers only the small set of possibilities that we actually need.
enum LegacyPrefixes {
    /// No prefix bytes
    None,
    /// Operand Size Override -- here, denoting "16-bit operation"
    _66,
    /// The Lock prefix
    _F0,
    /// Operand size override and Lock
    _66F0,
    /// REPNE, but no specific meaning here -- is just an opcode extension
    _F2,
    /// REP/REPE, but no specific meaning here -- is just an opcode extension
    _F3,
}

impl LegacyPrefixes {
    #[inline(always)]
    fn emit(&self, sink: &mut MachBuffer<Inst>) {
        match self {
            LegacyPrefixes::_66 => sink.put1(0x66),
            LegacyPrefixes::_F0 => sink.put1(0xF0),
            LegacyPrefixes::_66F0 => {
                // I don't think the order matters, but in any case, this is the same order that
                // the GNU assembler uses.
                sink.put1(0x66);
                sink.put1(0xF0);
            }
            LegacyPrefixes::_F2 => sink.put1(0xF2),
            LegacyPrefixes::_F3 => sink.put1(0xF3),
            LegacyPrefixes::None => (),
        }
    }
}

/// This is the core 'emit' function for instructions that reference memory.
///
/// For an instruction that has as operands a reg encoding `enc_g` and a memory address `mem_e`,
/// create and emit:
/// - first the legacy prefixes, if any
/// - then the REX prefix, if needed
/// - then caller-supplied opcode byte(s) (`opcodes` and `num_opcodes`),
/// - then the MOD/RM byte,
/// - then optionally, a SIB byte,
/// - and finally optionally an immediate that will be derived from the `mem_e` operand.
///
/// For most instructions up to and including SSE4.2, that will be the whole instruction: this is
/// what we call "standard" instructions, and abbreviate "std" in the name here. VEX-prefixed
/// instructions will require their own emitter functions.
///
/// This will also work for 32-bits x86 instructions, assuming no REX prefix is provided.
///
/// The opcodes are written bigendianly for the convenience of callers.  For example, if the opcode
/// bytes to be emitted are, in this order, F3 0F 27, then the caller should pass `opcodes` ==
/// 0xF3_0F_27 and `num_opcodes` == 3.
///
/// The register operand is represented here not as a `Reg` but as its hardware encoding, `enc_g`.
/// `rex` can specify special handling for the REX prefix.  By default, the REX prefix will
/// indicate a 64-bit operation and will be deleted if it is redundant (0x40).  Note that for a
/// 64-bit operation, the REX prefix will normally never be redundant, since REX.W must be 1 to
/// indicate a 64-bit operation.
fn emit_std_enc_mem(
    sink: &mut MachBuffer<Inst>,
    state: &EmitState,
    prefixes: LegacyPrefixes,
    opcodes: u32,
    mut num_opcodes: usize,
    enc_g: u8,
    mem_e: &Amode,
    rex: RexFlags,
) {
    // General comment for this function: the registers in `mem_e` must be
    // 64-bit integer registers, because they are part of an address
    // expression.  But `enc_g` can be derived from a register of any class.

    let srcloc = state.cur_srcloc();
    if srcloc != SourceLoc::default() {
        sink.add_trap(srcloc, TrapCode::HeapOutOfBounds);
    }

    prefixes.emit(sink);

    match mem_e {
        Amode::ImmReg { simm32, base } => {
            // First, the REX byte.
            let enc_e = int_reg_enc(*base);
            rex.emit_two_op(sink, enc_g, enc_e);

            // Now the opcode(s).  These include any other prefixes the caller
            // hands to us.
            while num_opcodes > 0 {
                num_opcodes -= 1;
                sink.put1(((opcodes >> (num_opcodes << 3)) & 0xFF) as u8);
            }

            // Now the mod/rm and associated immediates.  This is
            // significantly complicated due to the multiple special cases.
            if *simm32 == 0
                && enc_e != regs::ENC_RSP
                && enc_e != regs::ENC_RBP
                && enc_e != regs::ENC_R12
                && enc_e != regs::ENC_R13
            {
                // FIXME JRS 2020Feb11: those four tests can surely be
                // replaced by a single mask-and-compare check.  We should do
                // that because this routine is likely to be hot.
                sink.put1(encode_modrm(0, enc_g & 7, enc_e & 7));
            } else if *simm32 == 0 && (enc_e == regs::ENC_RSP || enc_e == regs::ENC_R12) {
                sink.put1(encode_modrm(0, enc_g & 7, 4));
                sink.put1(0x24);
            } else if low8_will_sign_extend_to_32(*simm32)
                && enc_e != regs::ENC_RSP
                && enc_e != regs::ENC_R12
            {
                sink.put1(encode_modrm(1, enc_g & 7, enc_e & 7));
                sink.put1((simm32 & 0xFF) as u8);
            } else if enc_e != regs::ENC_RSP && enc_e != regs::ENC_R12 {
                sink.put1(encode_modrm(2, enc_g & 7, enc_e & 7));
                sink.put4(*simm32);
            } else if (enc_e == regs::ENC_RSP || enc_e == regs::ENC_R12)
                && low8_will_sign_extend_to_32(*simm32)
            {
                // REX.B distinguishes RSP from R12
                sink.put1(encode_modrm(1, enc_g & 7, 4));
                sink.put1(0x24);
                sink.put1((simm32 & 0xFF) as u8);
            } else if enc_e == regs::ENC_R12 || enc_e == regs::ENC_RSP {
                //.. wait for test case for RSP case
                // REX.B distinguishes RSP from R12
                sink.put1(encode_modrm(2, enc_g & 7, 4));
                sink.put1(0x24);
                sink.put4(*simm32);
            } else {
                unreachable!("ImmReg");
            }
        }

        Amode::ImmRegRegShift {
            simm32,
            base: reg_base,
            index: reg_index,
            shift,
        } => {
            let enc_base = int_reg_enc(*reg_base);
            let enc_index = int_reg_enc(*reg_index);

            // The rex byte.
            rex.emit_three_op(sink, enc_g, enc_index, enc_base);

            // All other prefixes and opcodes.
            while num_opcodes > 0 {
                num_opcodes -= 1;
                sink.put1(((opcodes >> (num_opcodes << 3)) & 0xFF) as u8);
            }

            // modrm, SIB, immediates.
            if low8_will_sign_extend_to_32(*simm32) && enc_index != regs::ENC_RSP {
                sink.put1(encode_modrm(1, enc_g & 7, 4));
                sink.put1(encode_sib(*shift, enc_index & 7, enc_base & 7));
                sink.put1(*simm32 as u8);
            } else if enc_index != regs::ENC_RSP {
                sink.put1(encode_modrm(2, enc_g & 7, 4));
                sink.put1(encode_sib(*shift, enc_index & 7, enc_base & 7));
                sink.put4(*simm32);
            } else {
                panic!("ImmRegRegShift");
            }
        }

        Amode::RipRelative { ref target } => {
            // First, the REX byte, with REX.B = 0.
            rex.emit_two_op(sink, enc_g, 0);

            // Now the opcode(s).  These include any other prefixes the caller
            // hands to us.
            while num_opcodes > 0 {
                num_opcodes -= 1;
                sink.put1(((opcodes >> (num_opcodes << 3)) & 0xFF) as u8);
            }

            // RIP-relative is mod=00, rm=101.
            sink.put1(encode_modrm(0, enc_g & 7, 0b101));

            let offset = sink.cur_offset();
            sink.use_label_at_offset(offset, *target, LabelUse::JmpRel32);
            sink.put4(0);
        }
    }
}

/// This is the core 'emit' function for instructions that do not reference memory.
///
/// This is conceptually the same as emit_modrm_sib_enc_ge, except it is for the case where the E
/// operand is a register rather than memory.  Hence it is much simpler.
fn emit_std_enc_enc(
    sink: &mut MachBuffer<Inst>,
    prefixes: LegacyPrefixes,
    opcodes: u32,
    mut num_opcodes: usize,
    enc_g: u8,
    enc_e: u8,
    rex: RexFlags,
) {
    // EncG and EncE can be derived from registers of any class, and they
    // don't even have to be from the same class.  For example, for an
    // integer-to-FP conversion insn, one might be RegClass::I64 and the other
    // RegClass::V128.

    // The legacy prefixes.
    prefixes.emit(sink);

    // The rex byte.
    rex.emit_two_op(sink, enc_g, enc_e);

    // All other prefixes and opcodes.
    while num_opcodes > 0 {
        num_opcodes -= 1;
        sink.put1(((opcodes >> (num_opcodes << 3)) & 0xFF) as u8);
    }

    // Now the mod/rm byte.  The instruction we're generating doesn't access
    // memory, so there is no SIB byte or immediate -- we're done.
    sink.put1(encode_modrm(3, enc_g & 7, enc_e & 7));
}

// These are merely wrappers for the above two functions that facilitate passing
// actual `Reg`s rather than their encodings.

fn emit_std_reg_mem(
    sink: &mut MachBuffer<Inst>,
    state: &EmitState,
    prefixes: LegacyPrefixes,
    opcodes: u32,
    num_opcodes: usize,
    reg_g: Reg,
    mem_e: &Amode,
    rex: RexFlags,
) {
    let enc_g = reg_enc(reg_g);
    emit_std_enc_mem(
        sink,
        state,
        prefixes,
        opcodes,
        num_opcodes,
        enc_g,
        mem_e,
        rex,
    );
}

fn emit_std_reg_reg(
    sink: &mut MachBuffer<Inst>,
    prefixes: LegacyPrefixes,
    opcodes: u32,
    num_opcodes: usize,
    reg_g: Reg,
    reg_e: Reg,
    rex: RexFlags,
) {
    let enc_g = reg_enc(reg_g);
    let enc_e = reg_enc(reg_e);
    emit_std_enc_enc(sink, prefixes, opcodes, num_opcodes, enc_g, enc_e, rex);
}

/// Write a suitable number of bits from an imm64 to the sink.
fn emit_simm(sink: &mut MachBuffer<Inst>, size: u8, simm32: u32) {
    match size {
        8 | 4 => sink.put4(simm32),
        2 => sink.put2(simm32 as u16),
        1 => sink.put1(simm32 as u8),
        _ => unreachable!(),
    }
}

/// A small helper to generate a signed conversion instruction.
fn emit_signed_cvt(
    sink: &mut MachBuffer<Inst>,
    info: &EmitInfo,
    state: &mut EmitState,
    src: Reg,
    dst: Writable<Reg>,
    to_f64: bool,
) {
    // Handle an unsigned int, which is the "easy" case: a signed conversion will do the
    // right thing.
    let op = if to_f64 {
        SseOpcode::Cvtsi2sd
    } else {
        SseOpcode::Cvtsi2ss
    };
    let inst = Inst::gpr_to_xmm(op, RegMem::reg(src), OperandSize::Size64, dst);
    inst.emit(sink, info, state);
}

/// Emits a one way conditional jump if CC is set (true).
fn one_way_jmp(sink: &mut MachBuffer<Inst>, cc: CC, label: MachLabel) {
    let cond_start = sink.cur_offset();
    let cond_disp_off = cond_start + 2;
    sink.use_label_at_offset(cond_disp_off, label, LabelUse::JmpRel32);
    sink.put1(0x0F);
    sink.put1(0x80 + cc.get_enc());
    sink.put4(0x0);
}

/// Emits a relocation, attaching the current source location as well.
fn emit_reloc(
    sink: &mut MachBuffer<Inst>,
    state: &EmitState,
    kind: Reloc,
    name: &ExternalName,
    addend: Addend,
) {
    let srcloc = state.cur_srcloc();
    sink.add_reloc(srcloc, kind, name, addend);
}

/// The top-level emit function.
///
/// Important!  Do not add improved (shortened) encoding cases to existing
/// instructions without also adding tests for those improved encodings.  That
/// is a dangerous game that leads to hard-to-track-down errors in the emitted
/// code.
///
/// For all instructions, make sure to have test coverage for all of the
/// following situations.  Do this by creating the cross product resulting from
/// applying the following rules to each operand:
///
/// (1) for any insn that mentions a register: one test using a register from
///     the group [rax, rcx, rdx, rbx, rsp, rbp, rsi, rdi] and a second one
///     using a register from the group [r8, r9, r10, r11, r12, r13, r14, r15].
///     This helps detect incorrect REX prefix construction.
///
/// (2) for any insn that mentions a byte register: one test for each of the
///     four encoding groups [al, cl, dl, bl], [spl, bpl, sil, dil],
///     [r8b .. r11b] and [r12b .. r15b].  This checks that
///     apparently-redundant REX prefixes are retained when required.
///
/// (3) for any insn that contains an immediate field, check the following
///     cases: field is zero, field is in simm8 range (-128 .. 127), field is
///     in simm32 range (-0x8000_0000 .. 0x7FFF_FFFF).  This is because some
///     instructions that require a 32-bit immediate have a short-form encoding
///     when the imm is in simm8 range.
///
/// Rules (1), (2) and (3) don't apply for registers within address expressions
/// (`Addr`s).  Those are already pretty well tested, and the registers in them
/// don't have any effect on the containing instruction (apart from possibly
/// require REX prefix bits).
///
/// When choosing registers for a test, avoid using registers with the same
/// offset within a given group.  For example, don't use rax and r8, since they
/// both have the lowest 3 bits as 000, and so the test won't detect errors
/// where those 3-bit register sub-fields are confused by the emitter.  Instead
/// use (eg) rax (lo3 = 000) and r9 (lo3 = 001).  Similarly, don't use (eg) cl
/// and bpl since they have the same offset in their group; use instead (eg) cl
/// and sil.
///
/// For all instructions, also add a test that uses only low-half registers
/// (rax .. rdi, xmm0 .. xmm7) etc, so as to check that any redundant REX
/// prefixes are correctly omitted.  This low-half restriction must apply to
/// _all_ registers in the insn, even those in address expressions.
///
/// Following these rules creates large numbers of test cases, but it's the
/// only way to make the emitter reliable.
///
/// Known possible improvements:
///
/// * there's a shorter encoding for shl/shr/sar by a 1-bit immediate.  (Do we
///   care?)
pub(crate) fn emit(
    inst: &Inst,
    sink: &mut MachBuffer<Inst>,
    info: &EmitInfo,
    state: &mut EmitState,
) {
    if let Some(iset_requirement) = inst.isa_requirement() {
        match iset_requirement {
            // Cranelift assumes SSE2 at least.
            InstructionSet::SSE | InstructionSet::SSE2 => {}
            InstructionSet::SSSE3 => assert!(info.isa_flags.has_ssse3()),
            InstructionSet::SSE41 => assert!(info.isa_flags.has_sse41()),
            InstructionSet::SSE42 => assert!(info.isa_flags.has_sse42()),
        }
    }

    match inst {
        Inst::AluRmiR {
            is_64,
            op,
            src,
            dst: reg_g,
        } => {
            let rex = if *is_64 {
                RexFlags::set_w()
            } else {
                RexFlags::clear_w()
            };

            if *op == AluRmiROpcode::Mul {
                // We kinda freeloaded Mul into RMI_R_Op, but it doesn't fit the usual pattern, so
                // we have to special-case it.
                match src {
                    RegMemImm::Reg { reg: reg_e } => {
                        emit_std_reg_reg(
                            sink,
                            LegacyPrefixes::None,
                            0x0FAF,
                            2,
                            reg_g.to_reg(),
                            *reg_e,
                            rex,
                        );
                    }

                    RegMemImm::Mem { addr } => {
                        let amode = addr.finalize(state, sink);
                        emit_std_reg_mem(
                            sink,
                            state,
                            LegacyPrefixes::None,
                            0x0FAF,
                            2,
                            reg_g.to_reg(),
                            &amode,
                            rex,
                        );
                    }

                    RegMemImm::Imm { simm32 } => {
                        let use_imm8 = low8_will_sign_extend_to_32(*simm32);
                        let opcode = if use_imm8 { 0x6B } else { 0x69 };
                        // Yes, really, reg_g twice.
                        emit_std_reg_reg(
                            sink,
                            LegacyPrefixes::None,
                            opcode,
                            1,
                            reg_g.to_reg(),
                            reg_g.to_reg(),
                            rex,
                        );
                        emit_simm(sink, if use_imm8 { 1 } else { 4 }, *simm32);
                    }
                }
            } else {
                let (opcode_r, opcode_m, subopcode_i) = match op {
                    AluRmiROpcode::Add => (0x01, 0x03, 0),
                    AluRmiROpcode::Sub => (0x29, 0x2B, 5),
                    AluRmiROpcode::And => (0x21, 0x23, 4),
                    AluRmiROpcode::Or => (0x09, 0x0B, 1),
                    AluRmiROpcode::Xor => (0x31, 0x33, 6),
                    AluRmiROpcode::Mul => panic!("unreachable"),
                };

                match src {
                    RegMemImm::Reg { reg: reg_e } => {
                        // GCC/llvm use the swapped operand encoding (viz., the R/RM vs RM/R
                        // duality). Do this too, so as to be able to compare generated machine
                        // code easily.
                        emit_std_reg_reg(
                            sink,
                            LegacyPrefixes::None,
                            opcode_r,
                            1,
                            *reg_e,
                            reg_g.to_reg(),
                            rex,
                        );
                        // NB: if this is ever extended to handle byte size ops, be sure to retain
                        // redundant REX prefixes.
                    }

                    RegMemImm::Mem { addr } => {
                        // Here we revert to the "normal" G-E ordering.
                        let amode = addr.finalize(state, sink);
                        emit_std_reg_mem(
                            sink,
                            state,
                            LegacyPrefixes::None,
                            opcode_m,
                            1,
                            reg_g.to_reg(),
                            &amode,
                            rex,
                        );
                    }

                    RegMemImm::Imm { simm32 } => {
                        let use_imm8 = low8_will_sign_extend_to_32(*simm32);
                        let opcode = if use_imm8 { 0x83 } else { 0x81 };
                        // And also here we use the "normal" G-E ordering.
                        let enc_g = int_reg_enc(reg_g.to_reg());
                        emit_std_enc_enc(
                            sink,
                            LegacyPrefixes::None,
                            opcode,
                            1,
                            subopcode_i,
                            enc_g,
                            rex,
                        );
                        emit_simm(sink, if use_imm8 { 1 } else { 4 }, *simm32);
                    }
                }
            }
        }

        Inst::UnaryRmR { size, op, src, dst } => {
            let (prefix, rex_flags) = match size {
                2 => (LegacyPrefixes::_66, RexFlags::clear_w()),
                4 => (LegacyPrefixes::None, RexFlags::clear_w()),
                8 => (LegacyPrefixes::None, RexFlags::set_w()),
                _ => unreachable!(),
            };

            let (opcode, num_opcodes) = match op {
                UnaryRmROpcode::Bsr => (0x0fbd, 2),
                UnaryRmROpcode::Bsf => (0x0fbc, 2),
            };

            match src {
                RegMem::Reg { reg: src } => emit_std_reg_reg(
                    sink,
                    prefix,
                    opcode,
                    num_opcodes,
                    dst.to_reg(),
                    *src,
                    rex_flags,
                ),
                RegMem::Mem { addr: src } => {
                    let amode = src.finalize(state, sink);
                    emit_std_reg_mem(
                        sink,
                        state,
                        prefix,
                        opcode,
                        num_opcodes,
                        dst.to_reg(),
                        &amode,
                        rex_flags,
                    );
                }
            }
        }

        Inst::Not { size, src } => {
            let (opcode, prefix, rex_flags) = match size {
                1 => (0xF6, LegacyPrefixes::None, RexFlags::clear_w()),
                2 => (0xF7, LegacyPrefixes::_66, RexFlags::clear_w()),
                4 => (0xF7, LegacyPrefixes::None, RexFlags::clear_w()),
                8 => (0xF7, LegacyPrefixes::None, RexFlags::set_w()),
                _ => unreachable!("{}", size),
            };

            let subopcode = 2;
            let src = int_reg_enc(src.to_reg());
            emit_std_enc_enc(sink, prefix, opcode, 1, subopcode, src, rex_flags)
        }

        Inst::Neg { size, src } => {
            let (opcode, prefix, rex_flags) = match size {
                1 => (0xF6, LegacyPrefixes::None, RexFlags::clear_w()),
                2 => (0xF7, LegacyPrefixes::_66, RexFlags::clear_w()),
                4 => (0xF7, LegacyPrefixes::None, RexFlags::clear_w()),
                8 => (0xF7, LegacyPrefixes::None, RexFlags::set_w()),
                _ => unreachable!("{}", size),
            };

            let subopcode = 3;
            let src = int_reg_enc(src.to_reg());
            emit_std_enc_enc(sink, prefix, opcode, 1, subopcode, src, rex_flags)
        }

        Inst::Div {
            size,
            signed,
            divisor,
        } => {
            let (opcode, prefix, rex_flags) = match size {
                1 => (0xF6, LegacyPrefixes::None, RexFlags::clear_w()),
                2 => (0xF7, LegacyPrefixes::_66, RexFlags::clear_w()),
                4 => (0xF7, LegacyPrefixes::None, RexFlags::clear_w()),
                8 => (0xF7, LegacyPrefixes::None, RexFlags::set_w()),
                _ => unreachable!("{}", size),
            };

            let loc = state.cur_srcloc();
            sink.add_trap(loc, TrapCode::IntegerDivisionByZero);

            let subopcode = if *signed { 7 } else { 6 };
            match divisor {
                RegMem::Reg { reg } => {
                    let src = int_reg_enc(*reg);
                    emit_std_enc_enc(sink, prefix, opcode, 1, subopcode, src, rex_flags)
                }
                RegMem::Mem { addr: src } => {
                    let amode = src.finalize(state, sink);
                    emit_std_enc_mem(sink, state, prefix, opcode, 1, subopcode, &amode, rex_flags);
                }
            }
        }

        Inst::MulHi { size, signed, rhs } => {
            let (prefix, rex_flags) = match size {
                2 => (LegacyPrefixes::_66, RexFlags::clear_w()),
                4 => (LegacyPrefixes::None, RexFlags::clear_w()),
                8 => (LegacyPrefixes::None, RexFlags::set_w()),
                _ => unreachable!(),
            };

            let subopcode = if *signed { 5 } else { 4 };
            match rhs {
                RegMem::Reg { reg } => {
                    let src = int_reg_enc(*reg);
                    emit_std_enc_enc(sink, prefix, 0xF7, 1, subopcode, src, rex_flags)
                }
                RegMem::Mem { addr: src } => {
                    let amode = src.finalize(state, sink);
                    emit_std_enc_mem(sink, state, prefix, 0xF7, 1, subopcode, &amode, rex_flags);
                }
            }
        }

        Inst::SignExtendData { size } => match size {
            1 => {
                sink.put1(0x66);
                sink.put1(0x98);
            }
            2 => {
                sink.put1(0x66);
                sink.put1(0x99);
            }
            4 => sink.put1(0x99),
            8 => {
                sink.put1(0x48);
                sink.put1(0x99);
            }
            _ => unreachable!(),
        },

        Inst::CheckedDivOrRemSeq {
            kind,
            size,
            divisor,
            tmp,
        } => {
            // Generates the following code sequence:
            //
            // ;; check divide by zero:
            // cmp 0 %divisor
            // jnz $after_trap
            // ud2
            // $after_trap:
            //
            // ;; for signed modulo/div:
            // cmp -1 %divisor
            // jnz $do_op
            // ;;   for signed modulo, result is 0
            //    mov #0, %rdx
            //    j $done
            // ;;   for signed div, check for integer overflow against INT_MIN of the right size
            // cmp INT_MIN, %rax
            // jnz $do_op
            // ud2
            //
            // $do_op:
            // ;; if signed
            //     cdq ;; sign-extend from rax into rdx
            // ;; else
            //     mov #0, %rdx
            // idiv %divisor
            //
            // $done:
            debug_assert!(info.flags().avoid_div_traps());

            // Check if the divisor is zero, first.
            let inst = Inst::cmp_rmi_r(*size, RegMemImm::imm(0), divisor.to_reg());
            inst.emit(sink, info, state);

            let inst = Inst::trap_if(CC::Z, TrapCode::IntegerDivisionByZero);
            inst.emit(sink, info, state);

            let (do_op, done_label) = if kind.is_signed() {
                // Now check if the divisor is -1.
                let inst = Inst::cmp_rmi_r(*size, RegMemImm::imm(0xffffffff), divisor.to_reg());
                inst.emit(sink, info, state);

                let do_op = sink.get_label();

                // If not equal, jump to do-op.
                one_way_jmp(sink, CC::NZ, do_op);

                // Here, divisor == -1.
                if !kind.is_div() {
                    // x % -1 = 0; put the result into the destination, $rdx.
                    let done_label = sink.get_label();

                    let inst = Inst::imm(
                        OperandSize::from_bytes(*size as u32),
                        0,
                        Writable::from_reg(regs::rdx()),
                    );
                    inst.emit(sink, info, state);

                    let inst = Inst::jmp_known(done_label);
                    inst.emit(sink, info, state);

                    (Some(do_op), Some(done_label))
                } else {
                    // Check for integer overflow.
                    if *size == 8 {
                        let tmp = tmp.expect("temporary for i64 sdiv");

                        let inst = Inst::imm(OperandSize::Size64, 0x8000000000000000, tmp);
                        inst.emit(sink, info, state);

                        let inst = Inst::cmp_rmi_r(8, RegMemImm::reg(tmp.to_reg()), regs::rax());
                        inst.emit(sink, info, state);
                    } else {
                        let inst = Inst::cmp_rmi_r(*size, RegMemImm::imm(0x80000000), regs::rax());
                        inst.emit(sink, info, state);
                    }

                    // If not equal, jump over the trap.
                    let inst = Inst::trap_if(CC::Z, TrapCode::IntegerOverflow);
                    inst.emit(sink, info, state);

                    (Some(do_op), None)
                }
            } else {
                (None, None)
            };

            if let Some(do_op) = do_op {
                sink.bind_label(do_op);
            }

            assert!(
                *size > 1,
                "CheckedDivOrRemSeq for i8 is not yet implemented"
            );

            // Fill in the high parts:
            if kind.is_signed() {
                // sign-extend the sign-bit of rax into rdx, for signed opcodes.
                let inst = Inst::sign_extend_data(*size);
                inst.emit(sink, info, state);
            } else {
                // zero for unsigned opcodes.
                let inst = Inst::imm(OperandSize::Size64, 0, Writable::from_reg(regs::rdx()));
                inst.emit(sink, info, state);
            }

            let inst = Inst::div(*size, kind.is_signed(), RegMem::reg(divisor.to_reg()));
            inst.emit(sink, info, state);

            // Lowering takes care of moving the result back into the right register, see comment
            // there.

            if let Some(done) = done_label {
                sink.bind_label(done);
            }
        }

        Inst::Imm {
            dst_is_64,
            simm64,
            dst,
        } => {
            let enc_dst = int_reg_enc(dst.to_reg());
            if *dst_is_64 {
                if low32_will_sign_extend_to_64(*simm64) {
                    // Sign-extended move imm32.
                    emit_std_enc_enc(
                        sink,
                        LegacyPrefixes::None,
                        0xC7,
                        1,
                        /* subopcode */ 0,
                        enc_dst,
                        RexFlags::set_w(),
                    );
                    sink.put4(*simm64 as u32);
                } else {
                    sink.put1(0x48 | ((enc_dst >> 3) & 1));
                    sink.put1(0xB8 | (enc_dst & 7));
                    sink.put8(*simm64);
                }
            } else {
                if ((enc_dst >> 3) & 1) == 1 {
                    sink.put1(0x41);
                }
                sink.put1(0xB8 | (enc_dst & 7));
                sink.put4(*simm64 as u32);
            }
        }

        Inst::MovRR { is_64, src, dst } => {
            let rex = if *is_64 {
                RexFlags::set_w()
            } else {
                RexFlags::clear_w()
            };
            emit_std_reg_reg(sink, LegacyPrefixes::None, 0x89, 1, *src, dst.to_reg(), rex);
        }

        Inst::MovzxRmR { ext_mode, src, dst } => {
            let (opcodes, num_opcodes, mut rex_flags) = match ext_mode {
                ExtMode::BL => {
                    // MOVZBL is (REX.W==0) 0F B6 /r
                    (0x0FB6, 2, RexFlags::clear_w())
                }
                ExtMode::BQ => {
                    // MOVZBQ is (REX.W==1) 0F B6 /r
                    // I'm not sure why the Intel manual offers different
                    // encodings for MOVZBQ than for MOVZBL.  AIUI they should
                    // achieve the same, since MOVZBL is just going to zero out
                    // the upper half of the destination anyway.
                    (0x0FB6, 2, RexFlags::set_w())
                }
                ExtMode::WL => {
                    // MOVZWL is (REX.W==0) 0F B7 /r
                    (0x0FB7, 2, RexFlags::clear_w())
                }
                ExtMode::WQ => {
                    // MOVZWQ is (REX.W==1) 0F B7 /r
                    (0x0FB7, 2, RexFlags::set_w())
                }
                ExtMode::LQ => {
                    // This is just a standard 32 bit load, and we rely on the
                    // default zero-extension rule to perform the extension.
                    // Note that in reg/reg mode, gcc seems to use the swapped form R/RM, which we
                    // don't do here, since it's the same encoding size.
                    // MOV r/m32, r32 is (REX.W==0) 8B /r
                    (0x8B, 1, RexFlags::clear_w())
                }
            };

            match src {
                RegMem::Reg { reg: src } => {
                    match ext_mode {
                        ExtMode::BL | ExtMode::BQ => {
                            // A redundant REX prefix must be emitted for certain register inputs.
                            let enc_src = int_reg_enc(*src);
                            if enc_src >= 4 && enc_src <= 7 {
                                rex_flags.always_emit();
                            };
                        }
                        _ => {}
                    }
                    emit_std_reg_reg(
                        sink,
                        LegacyPrefixes::None,
                        opcodes,
                        num_opcodes,
                        dst.to_reg(),
                        *src,
                        rex_flags,
                    )
                }

                RegMem::Mem { addr: src } => {
                    let src = &src.finalize(state, sink);

                    emit_std_reg_mem(
                        sink,
                        state,
                        LegacyPrefixes::None,
                        opcodes,
                        num_opcodes,
                        dst.to_reg(),
                        src,
                        rex_flags,
                    )
                }
            }
        }

        Inst::Mov64MR { src, dst } => {
            let src = &src.finalize(state, sink);

            emit_std_reg_mem(
                sink,
                state,
                LegacyPrefixes::None,
                0x8B,
                1,
                dst.to_reg(),
                src,
                RexFlags::set_w(),
            )
        }

        Inst::LoadEffectiveAddress { addr, dst } => {
            let amode = addr.finalize(state, sink);

            emit_std_reg_mem(
                sink,
                state,
                LegacyPrefixes::None,
                0x8D,
                1,
                dst.to_reg(),
                &amode,
                RexFlags::set_w(),
            );
        }

        Inst::MovsxRmR { ext_mode, src, dst } => {
            let (opcodes, num_opcodes, mut rex_flags) = match ext_mode {
                ExtMode::BL => {
                    // MOVSBL is (REX.W==0) 0F BE /r
                    (0x0FBE, 2, RexFlags::clear_w())
                }
                ExtMode::BQ => {
                    // MOVSBQ is (REX.W==1) 0F BE /r
                    (0x0FBE, 2, RexFlags::set_w())
                }
                ExtMode::WL => {
                    // MOVSWL is (REX.W==0) 0F BF /r
                    (0x0FBF, 2, RexFlags::clear_w())
                }
                ExtMode::WQ => {
                    // MOVSWQ is (REX.W==1) 0F BF /r
                    (0x0FBF, 2, RexFlags::set_w())
                }
                ExtMode::LQ => {
                    // MOVSLQ is (REX.W==1) 63 /r
                    (0x63, 1, RexFlags::set_w())
                }
            };

            match src {
                RegMem::Reg { reg: src } => {
                    match ext_mode {
                        ExtMode::BL | ExtMode::BQ => {
                            // A redundant REX prefix must be emitted for certain register inputs.
                            let enc_src = int_reg_enc(*src);
                            if enc_src >= 4 && enc_src <= 7 {
                                rex_flags.always_emit();
                            };
                        }
                        _ => {}
                    }
                    emit_std_reg_reg(
                        sink,
                        LegacyPrefixes::None,
                        opcodes,
                        num_opcodes,
                        dst.to_reg(),
                        *src,
                        rex_flags,
                    )
                }

                RegMem::Mem { addr: src } => {
                    let src = &src.finalize(state, sink);

                    emit_std_reg_mem(
                        sink,
                        state,
                        LegacyPrefixes::None,
                        opcodes,
                        num_opcodes,
                        dst.to_reg(),
                        src,
                        rex_flags,
                    )
                }
            }
        }

        Inst::MovRM { size, src, dst } => {
            let dst = &dst.finalize(state, sink);

            match size {
                1 => {
                    // This is one of the few places where the presence of a
                    // redundant REX prefix changes the meaning of the
                    // instruction.
                    let mut rex = RexFlags::clear_w();

                    let enc_src = int_reg_enc(*src);
                    if enc_src >= 4 && enc_src <= 7 {
                        rex.always_emit();
                    };

                    // MOV r8, r/m8 is (REX.W==0) 88 /r
                    emit_std_reg_mem(sink, state, LegacyPrefixes::None, 0x88, 1, *src, dst, rex)
                }

                2 => {
                    // MOV r16, r/m16 is 66 (REX.W==0) 89 /r
                    emit_std_reg_mem(
                        sink,
                        state,
                        LegacyPrefixes::_66,
                        0x89,
                        1,
                        *src,
                        dst,
                        RexFlags::clear_w(),
                    )
                }

                4 => {
                    // MOV r32, r/m32 is (REX.W==0) 89 /r
                    emit_std_reg_mem(
                        sink,
                        state,
                        LegacyPrefixes::None,
                        0x89,
                        1,
                        *src,
                        dst,
                        RexFlags::clear_w(),
                    )
                }

                8 => {
                    // MOV r64, r/m64 is (REX.W==1) 89 /r
                    emit_std_reg_mem(
                        sink,
                        state,
                        LegacyPrefixes::None,
                        0x89,
                        1,
                        *src,
                        dst,
                        RexFlags::set_w(),
                    )
                }

                _ => panic!("x64::Inst::Mov_R_M::emit: unreachable"),
            }
        }

        Inst::ShiftR {
            size,
            kind,
            num_bits,
            dst,
        } => {
            let enc_dst = int_reg_enc(dst.to_reg());
            let subopcode = match kind {
                ShiftKind::RotateLeft => 0,
                ShiftKind::RotateRight => 1,
                ShiftKind::ShiftLeft => 4,
                ShiftKind::ShiftRightLogical => 5,
                ShiftKind::ShiftRightArithmetic => 7,
            };

            match num_bits {
                None => {
                    let (opcode, prefix, rex_flags) = match size {
                        1 => (0xD2, LegacyPrefixes::None, RexFlags::clear_w()),
                        2 => (0xD3, LegacyPrefixes::_66, RexFlags::clear_w()),
                        4 => (0xD3, LegacyPrefixes::None, RexFlags::clear_w()),
                        8 => (0xD3, LegacyPrefixes::None, RexFlags::set_w()),
                        _ => unreachable!("{}", size),
                    };

                    // SHL/SHR/SAR %cl, reg8 is (REX.W==0) D2 /subopcode
                    // SHL/SHR/SAR %cl, reg16 is 66 (REX.W==0) D3 /subopcode
                    // SHL/SHR/SAR %cl, reg32 is (REX.W==0) D3 /subopcode
                    // SHL/SHR/SAR %cl, reg64 is (REX.W==1) D3 /subopcode
                    emit_std_enc_enc(sink, prefix, opcode, 1, subopcode, enc_dst, rex_flags);
                }

                Some(num_bits) => {
                    let (opcode, prefix, rex_flags) = match size {
                        1 => (0xC0, LegacyPrefixes::None, RexFlags::clear_w()),
                        2 => (0xC1, LegacyPrefixes::_66, RexFlags::clear_w()),
                        4 => (0xC1, LegacyPrefixes::None, RexFlags::clear_w()),
                        8 => (0xC1, LegacyPrefixes::None, RexFlags::set_w()),
                        _ => unreachable!("{}", size),
                    };

                    // SHL/SHR/SAR $ib, reg8 is (REX.W==0) C0 /subopcode
                    // SHL/SHR/SAR $ib, reg16 is 66 (REX.W==0) C1 /subopcode
                    // SHL/SHR/SAR $ib, reg32 is (REX.W==0) C1 /subopcode ib
                    // SHL/SHR/SAR $ib, reg64 is (REX.W==1) C1 /subopcode ib
                    // When the shift amount is 1, there's an even shorter encoding, but we don't
                    // bother with that nicety here.
                    emit_std_enc_enc(sink, prefix, opcode, 1, subopcode, enc_dst, rex_flags);
                    sink.put1(*num_bits);
                }
            }
        }

        Inst::XmmRmiReg { opcode, src, dst } => {
            let rex = RexFlags::clear_w();
            let prefix = LegacyPrefixes::_66;
            if let RegMemImm::Imm { simm32 } = src {
                let (opcode_bytes, reg_digit) = match opcode {
                    SseOpcode::Psllw => (0x0F71, 6),
                    SseOpcode::Pslld => (0x0F72, 6),
                    SseOpcode::Psllq => (0x0F73, 6),
                    SseOpcode::Psraw => (0x0F71, 4),
                    SseOpcode::Psrad => (0x0F72, 4),
                    SseOpcode::Psrlw => (0x0F71, 2),
                    SseOpcode::Psrld => (0x0F72, 2),
                    SseOpcode::Psrlq => (0x0F73, 2),
                    _ => panic!("invalid opcode: {}", opcode),
                };
                let dst_enc = reg_enc(dst.to_reg());
                emit_std_enc_enc(sink, prefix, opcode_bytes, 2, reg_digit, dst_enc, rex);
                let imm = (*simm32)
                    .try_into()
                    .expect("the immediate must be convertible to a u8");
                sink.put1(imm);
            } else {
                let opcode_bytes = match opcode {
                    SseOpcode::Psllw => 0x0FF1,
                    SseOpcode::Pslld => 0x0FF2,
                    SseOpcode::Psllq => 0x0FF3,
                    SseOpcode::Psraw => 0x0FE1,
                    SseOpcode::Psrad => 0x0FE2,
                    SseOpcode::Psrlw => 0x0FD1,
                    SseOpcode::Psrld => 0x0FD2,
                    SseOpcode::Psrlq => 0x0FD3,
                    _ => panic!("invalid opcode: {}", opcode),
                };

                match src {
                    RegMemImm::Reg { reg } => {
                        emit_std_reg_reg(sink, prefix, opcode_bytes, 2, dst.to_reg(), *reg, rex);
                    }
                    RegMemImm::Mem { addr } => {
                        let addr = &addr.finalize(state, sink);
                        emit_std_reg_mem(
                            sink,
                            state,
                            prefix,
                            opcode_bytes,
                            2,
                            dst.to_reg(),
                            addr,
                            rex,
                        );
                    }
                    RegMemImm::Imm { .. } => unreachable!(),
                }
            };
        }

        Inst::CmpRmiR {
            size,
            src: src_e,
            dst: reg_g,
        } => {
            let mut prefix = LegacyPrefixes::None;
            if *size == 2 {
                prefix = LegacyPrefixes::_66;
            }

            let mut rex = match size {
                8 => RexFlags::set_w(),
                4 | 2 => RexFlags::clear_w(),
                1 => {
                    let mut rex = RexFlags::clear_w();
                    // Here, a redundant REX prefix changes the meaning of the instruction.
                    let enc_g = int_reg_enc(*reg_g);
                    if enc_g >= 4 && enc_g <= 7 {
                        rex.always_emit();
                    }
                    rex
                }
                _ => panic!("x64::Inst::Cmp_RMI_R::emit: unreachable"),
            };

            match src_e {
                RegMemImm::Reg { reg: reg_e } => {
                    if *size == 1 {
                        // Check whether the E register forces the use of a redundant REX.
                        let enc_e = int_reg_enc(*reg_e);
                        if enc_e >= 4 && enc_e <= 7 {
                            rex.always_emit();
                        }
                    }

                    // Use the swapped operands encoding, to stay consistent with the output of
                    // gcc/llvm.
                    let opcode = if *size == 1 { 0x38 } else { 0x39 };
                    emit_std_reg_reg(sink, prefix, opcode, 1, *reg_e, *reg_g, rex);
                }

                RegMemImm::Mem { addr } => {
                    let addr = &addr.finalize(state, sink);
                    // Whereas here we revert to the "normal" G-E ordering.
                    let opcode = if *size == 1 { 0x3A } else { 0x3B };
                    emit_std_reg_mem(sink, state, prefix, opcode, 1, *reg_g, addr, rex);
                }

                RegMemImm::Imm { simm32 } => {
                    // FIXME JRS 2020Feb11: there are shorter encodings for
                    // cmp $imm, rax/eax/ax/al.
                    let use_imm8 = low8_will_sign_extend_to_32(*simm32);

                    // And also here we use the "normal" G-E ordering.
                    let opcode = if *size == 1 {
                        0x80
                    } else if use_imm8 {
                        0x83
                    } else {
                        0x81
                    };

                    let enc_g = int_reg_enc(*reg_g);
                    emit_std_enc_enc(sink, prefix, opcode, 1, 7 /*subopcode*/, enc_g, rex);
                    emit_simm(sink, if use_imm8 { 1 } else { *size }, *simm32);
                }
            }
        }

        Inst::Setcc { cc, dst } => {
            let opcode = 0x0f90 + cc.get_enc() as u32;
            let mut rex_flags = RexFlags::clear_w();
            rex_flags.always_emit();
            emit_std_enc_enc(
                sink,
                LegacyPrefixes::None,
                opcode,
                2,
                0,
                reg_enc(dst.to_reg()),
                rex_flags,
            );
        }

        Inst::Cmove {
            size,
            cc,
            src,
            dst: reg_g,
        } => {
            let (prefix, rex_flags) = match size {
                2 => (LegacyPrefixes::_66, RexFlags::clear_w()),
                4 => (LegacyPrefixes::None, RexFlags::clear_w()),
                8 => (LegacyPrefixes::None, RexFlags::set_w()),
                _ => unreachable!("invalid size spec for cmove"),
            };
            let opcode = 0x0F40 + cc.get_enc() as u32;
            match src {
                RegMem::Reg { reg: reg_e } => {
                    emit_std_reg_reg(sink, prefix, opcode, 2, reg_g.to_reg(), *reg_e, rex_flags);
                }
                RegMem::Mem { addr } => {
                    let addr = &addr.finalize(state, sink);
                    emit_std_reg_mem(
                        sink,
                        state,
                        prefix,
                        opcode,
                        2,
                        reg_g.to_reg(),
                        addr,
                        rex_flags,
                    );
                }
            }
        }

        Inst::XmmCmove {
            is_64,
            cc,
            src,
            dst,
        } => {
            // Lowering of the Select IR opcode when the input is an fcmp relies on the fact that
            // this doesn't clobber flags. Make sure to not do so here.
            let next = sink.get_label();

            // Jump if cc is *not* set.
            one_way_jmp(sink, cc.invert(), next);

            let op = if *is_64 {
                SseOpcode::Movsd
            } else {
                SseOpcode::Movss
            };
            let inst = Inst::xmm_unary_rm_r(op, src.clone(), *dst);
            inst.emit(sink, info, state);

            sink.bind_label(next);
        }

        Inst::Push64 { src } => {
            match src {
                RegMemImm::Reg { reg } => {
                    let enc_reg = int_reg_enc(*reg);
                    let rex = 0x40 | ((enc_reg >> 3) & 1);
                    if rex != 0x40 {
                        sink.put1(rex);
                    }
                    sink.put1(0x50 | (enc_reg & 7));
                }

                RegMemImm::Mem { addr } => {
                    let addr = &addr.finalize(state, sink);
                    emit_std_enc_mem(
                        sink,
                        state,
                        LegacyPrefixes::None,
                        0xFF,
                        1,
                        6, /*subopcode*/
                        addr,
                        RexFlags::clear_w(),
                    );
                }

                RegMemImm::Imm { simm32 } => {
                    if low8_will_sign_extend_to_64(*simm32) {
                        sink.put1(0x6A);
                        sink.put1(*simm32 as u8);
                    } else {
                        sink.put1(0x68);
                        sink.put4(*simm32);
                    }
                }
            }
        }

        Inst::Pop64 { dst } => {
            let enc_dst = int_reg_enc(dst.to_reg());
            if enc_dst >= 8 {
                // 0x41 == REX.{W=0, B=1}.  It seems that REX.W is irrelevant here.
                sink.put1(0x41);
            }
            sink.put1(0x58 + (enc_dst & 7));
        }

        Inst::CallKnown { dest, opcode, .. } => {
            if let Some(s) = state.take_stack_map() {
                sink.add_stack_map(StackMapExtent::UpcomingBytes(5), s);
            }
            sink.put1(0xE8);
            // The addend adjusts for the difference between the end of the instruction and the
            // beginning of the immediate field.
            emit_reloc(sink, state, Reloc::X86CallPCRel4, &dest, -4);
            sink.put4(0);
            if opcode.is_call() {
                let loc = state.cur_srcloc();
                sink.add_call_site(loc, *opcode);
            }
        }

        Inst::CallUnknown { dest, opcode, .. } => {
            let start_offset = sink.cur_offset();
            match dest {
                RegMem::Reg { reg } => {
                    let reg_enc = int_reg_enc(*reg);
                    emit_std_enc_enc(
                        sink,
                        LegacyPrefixes::None,
                        0xFF,
                        1,
                        2, /*subopcode*/
                        reg_enc,
                        RexFlags::clear_w(),
                    );
                }

                RegMem::Mem { addr } => {
                    let addr = &addr.finalize(state, sink);
                    emit_std_enc_mem(
                        sink,
                        state,
                        LegacyPrefixes::None,
                        0xFF,
                        1,
                        2, /*subopcode*/
                        addr,
                        RexFlags::clear_w(),
                    );
                }
            }
            if let Some(s) = state.take_stack_map() {
                sink.add_stack_map(StackMapExtent::StartedAtOffset(start_offset), s);
            }
            if opcode.is_call() {
                let loc = state.cur_srcloc();
                sink.add_call_site(loc, *opcode);
            }
        }

        Inst::Ret {} => sink.put1(0xC3),

        Inst::JmpKnown { dst } => {
            let br_start = sink.cur_offset();
            let br_disp_off = br_start + 1;
            let br_end = br_start + 5;

            sink.use_label_at_offset(br_disp_off, *dst, LabelUse::JmpRel32);
            sink.add_uncond_branch(br_start, br_end, *dst);

            sink.put1(0xE9);
            // Placeholder for the label value.
            sink.put4(0x0);
        }

        Inst::JmpIf { cc, taken } => {
            let cond_start = sink.cur_offset();
            let cond_disp_off = cond_start + 2;

            sink.use_label_at_offset(cond_disp_off, *taken, LabelUse::JmpRel32);
            // Since this is not a terminator, don't enroll in the branch inversion mechanism.

            sink.put1(0x0F);
            sink.put1(0x80 + cc.get_enc());
            // Placeholder for the label value.
            sink.put4(0x0);
        }

        Inst::JmpCond {
            cc,
            taken,
            not_taken,
        } => {
            // If taken.
            let cond_start = sink.cur_offset();
            let cond_disp_off = cond_start + 2;
            let cond_end = cond_start + 6;

            sink.use_label_at_offset(cond_disp_off, *taken, LabelUse::JmpRel32);
            let inverted: [u8; 6] = [0x0F, 0x80 + (cc.invert().get_enc()), 0x00, 0x00, 0x00, 0x00];
            sink.add_cond_branch(cond_start, cond_end, *taken, &inverted[..]);

            sink.put1(0x0F);
            sink.put1(0x80 + cc.get_enc());
            // Placeholder for the label value.
            sink.put4(0x0);

            // If not taken.
            let uncond_start = sink.cur_offset();
            let uncond_disp_off = uncond_start + 1;
            let uncond_end = uncond_start + 5;

            sink.use_label_at_offset(uncond_disp_off, *not_taken, LabelUse::JmpRel32);
            sink.add_uncond_branch(uncond_start, uncond_end, *not_taken);

            sink.put1(0xE9);
            // Placeholder for the label value.
            sink.put4(0x0);
        }

        Inst::JmpUnknown { target } => {
            match target {
                RegMem::Reg { reg } => {
                    let reg_enc = int_reg_enc(*reg);
                    emit_std_enc_enc(
                        sink,
                        LegacyPrefixes::None,
                        0xFF,
                        1,
                        4, /*subopcode*/
                        reg_enc,
                        RexFlags::clear_w(),
                    );
                }

                RegMem::Mem { addr } => {
                    let addr = &addr.finalize(state, sink);
                    emit_std_enc_mem(
                        sink,
                        state,
                        LegacyPrefixes::None,
                        0xFF,
                        1,
                        4, /*subopcode*/
                        addr,
                        RexFlags::clear_w(),
                    );
                }
            }
        }

        Inst::JmpTableSeq {
            idx,
            tmp1,
            tmp2,
            ref targets,
            default_target,
            ..
        } => {
            // This sequence is *one* instruction in the vcode, and is expanded only here at
            // emission time, because we cannot allow the regalloc to insert spills/reloads in
            // the middle; we depend on hardcoded PC-rel addressing below.
            //
            // We don't have to worry about emitting islands, because the only label-use type has a
            // maximum range of 2 GB. If we later consider using shorter-range label references,
            // this will need to be revisited.

            // Save index in a tmp (the live range of ridx only goes to start of this
            // sequence; rtmp1 or rtmp2 may overwrite it).

            // We generate the following sequence:
            // ;; generated by lowering: cmp #jmp_table_size, %idx
            // jnb $default_target
            // movl %idx, %tmp2
            // lea start_of_jump_table_offset(%rip), %tmp1
            // movslq [%tmp1, %tmp2, 4], %tmp2 ;; shift of 2, viz. multiply index by 4
            // addq %tmp2, %tmp1
            // j *%tmp1
            // $start_of_jump_table:
            // -- jump table entries
            one_way_jmp(sink, CC::NB, *default_target); // idx unsigned >= jmp table size

            // Copy the index (and make sure to clear the high 32-bits lane of tmp2).
            let inst = Inst::movzx_rm_r(ExtMode::LQ, RegMem::reg(*idx), *tmp2);
            inst.emit(sink, info, state);

            // Load base address of jump table.
            let start_of_jumptable = sink.get_label();
            let inst = Inst::lea(Amode::rip_relative(start_of_jumptable), *tmp1);
            inst.emit(sink, info, state);

            // Load value out of the jump table. It's a relative offset to the target block, so it
            // might be negative; use a sign-extension.
            let inst = Inst::movsx_rm_r(
                ExtMode::LQ,
                RegMem::mem(Amode::imm_reg_reg_shift(0, tmp1.to_reg(), tmp2.to_reg(), 2)),
                *tmp2,
            );
            inst.emit(sink, info, state);

            // Add base of jump table to jump-table-sourced block offset.
            let inst = Inst::alu_rmi_r(
                true, /* is_64 */
                AluRmiROpcode::Add,
                RegMemImm::reg(tmp2.to_reg()),
                *tmp1,
            );
            inst.emit(sink, info, state);

            // Branch to computed address.
            let inst = Inst::jmp_unknown(RegMem::reg(tmp1.to_reg()));
            inst.emit(sink, info, state);

            // Emit jump table (table of 32-bit offsets).
            sink.bind_label(start_of_jumptable);
            let jt_off = sink.cur_offset();
            for &target in targets.iter() {
                let word_off = sink.cur_offset();
                // off_into_table is an addend here embedded in the label to be later patched at
                // the end of codegen. The offset is initially relative to this jump table entry;
                // with the extra addend, it'll be relative to the jump table's start, after
                // patching.
                let off_into_table = word_off - jt_off;
                sink.use_label_at_offset(word_off, target, LabelUse::PCRel32);
                sink.put4(off_into_table);
            }
        }

        Inst::TrapIf { cc, trap_code } => {
            let else_label = sink.get_label();

            // Jump over if the invert of CC is set (i.e. CC is not set).
            one_way_jmp(sink, cc.invert(), else_label);

            // Trap!
            let inst = Inst::trap(*trap_code);
            inst.emit(sink, info, state);

            sink.bind_label(else_label);
        }

        Inst::XmmUnaryRmR {
            op,
            src: src_e,
            dst: reg_g,
        } => {
            let rex = RexFlags::clear_w();

            let (prefix, opcode, num_opcodes) = match op {
                SseOpcode::Cvtss2sd => (LegacyPrefixes::_F3, 0x0F5A, 2),
                SseOpcode::Cvtsd2ss => (LegacyPrefixes::_F2, 0x0F5A, 2),
                SseOpcode::Movaps => (LegacyPrefixes::None, 0x0F28, 2),
                SseOpcode::Movapd => (LegacyPrefixes::_66, 0x0F28, 2),
                SseOpcode::Movdqa => (LegacyPrefixes::_66, 0x0F6F, 2),
                SseOpcode::Movdqu => (LegacyPrefixes::_F3, 0x0F6F, 2),
                SseOpcode::Movsd => (LegacyPrefixes::_F2, 0x0F10, 2),
                SseOpcode::Movss => (LegacyPrefixes::_F3, 0x0F10, 2),
                SseOpcode::Movups => (LegacyPrefixes::None, 0x0F10, 2),
                SseOpcode::Movupd => (LegacyPrefixes::_66, 0x0F10, 2),
                SseOpcode::Pabsb => (LegacyPrefixes::_66, 0x0F381C, 3),
                SseOpcode::Pabsw => (LegacyPrefixes::_66, 0x0F381D, 3),
                SseOpcode::Pabsd => (LegacyPrefixes::_66, 0x0F381E, 3),
                SseOpcode::Sqrtps => (LegacyPrefixes::None, 0x0F51, 2),
                SseOpcode::Sqrtpd => (LegacyPrefixes::_66, 0x0F51, 2),
                SseOpcode::Sqrtss => (LegacyPrefixes::_F3, 0x0F51, 2),
                SseOpcode::Sqrtsd => (LegacyPrefixes::_F2, 0x0F51, 2),
                _ => unimplemented!("Opcode {:?} not implemented", op),
            };

            match src_e {
                RegMem::Reg { reg: reg_e } => {
                    emit_std_reg_reg(
                        sink,
                        prefix,
                        opcode,
                        num_opcodes,
                        reg_g.to_reg(),
                        *reg_e,
                        rex,
                    );
                }
                RegMem::Mem { addr } => {
                    let addr = &addr.finalize(state, sink);
                    emit_std_reg_mem(
                        sink,
                        state,
                        prefix,
                        opcode,
                        num_opcodes,
                        reg_g.to_reg(),
                        addr,
                        rex,
                    );
                }
            };
        }

        Inst::XmmRmR {
            op,
            src: src_e,
            dst: reg_g,
        } => {
            let rex = RexFlags::clear_w();
            let (prefix, opcode, length) = match op {
                SseOpcode::Addps => (LegacyPrefixes::None, 0x0F58, 2),
                SseOpcode::Addpd => (LegacyPrefixes::_66, 0x0F58, 2),
                SseOpcode::Addss => (LegacyPrefixes::_F3, 0x0F58, 2),
                SseOpcode::Addsd => (LegacyPrefixes::_F2, 0x0F58, 2),
                SseOpcode::Andps => (LegacyPrefixes::None, 0x0F54, 2),
                SseOpcode::Andpd => (LegacyPrefixes::_66, 0x0F54, 2),
                SseOpcode::Andnps => (LegacyPrefixes::None, 0x0F55, 2),
                SseOpcode::Andnpd => (LegacyPrefixes::_66, 0x0F55, 2),
                SseOpcode::Cvttps2dq => (LegacyPrefixes::_F3, 0x0F5B, 2),
                SseOpcode::Cvtdq2ps => (LegacyPrefixes::None, 0x0F5B, 2),
                SseOpcode::Divps => (LegacyPrefixes::None, 0x0F5E, 2),
                SseOpcode::Divpd => (LegacyPrefixes::_66, 0x0F5E, 2),
                SseOpcode::Divss => (LegacyPrefixes::_F3, 0x0F5E, 2),
                SseOpcode::Divsd => (LegacyPrefixes::_F2, 0x0F5E, 2),
                SseOpcode::Maxps => (LegacyPrefixes::None, 0x0F5F, 2),
                SseOpcode::Maxpd => (LegacyPrefixes::_66, 0x0F5F, 2),
                SseOpcode::Maxss => (LegacyPrefixes::_F3, 0x0F5F, 2),
                SseOpcode::Maxsd => (LegacyPrefixes::_F2, 0x0F5F, 2),
                SseOpcode::Minps => (LegacyPrefixes::None, 0x0F5D, 2),
                SseOpcode::Minpd => (LegacyPrefixes::_66, 0x0F5D, 2),
                SseOpcode::Minss => (LegacyPrefixes::_F3, 0x0F5D, 2),
                SseOpcode::Minsd => (LegacyPrefixes::_F2, 0x0F5D, 2),
                SseOpcode::Movlhps => (LegacyPrefixes::None, 0x0F16, 2),
                SseOpcode::Movsd => (LegacyPrefixes::_F2, 0x0F10, 2),
                SseOpcode::Mulps => (LegacyPrefixes::None, 0x0F59, 2),
                SseOpcode::Mulpd => (LegacyPrefixes::_66, 0x0F59, 2),
                SseOpcode::Mulss => (LegacyPrefixes::_F3, 0x0F59, 2),
                SseOpcode::Mulsd => (LegacyPrefixes::_F2, 0x0F59, 2),
                SseOpcode::Orpd => (LegacyPrefixes::_66, 0x0F56, 2),
                SseOpcode::Orps => (LegacyPrefixes::None, 0x0F56, 2),
                SseOpcode::Packsswb => (LegacyPrefixes::_66, 0x0F63, 2),
                SseOpcode::Paddb => (LegacyPrefixes::_66, 0x0FFC, 2),
                SseOpcode::Paddd => (LegacyPrefixes::_66, 0x0FFE, 2),
                SseOpcode::Paddq => (LegacyPrefixes::_66, 0x0FD4, 2),
                SseOpcode::Paddw => (LegacyPrefixes::_66, 0x0FFD, 2),
                SseOpcode::Paddsb => (LegacyPrefixes::_66, 0x0FEC, 2),
                SseOpcode::Paddsw => (LegacyPrefixes::_66, 0x0FED, 2),
                SseOpcode::Paddusb => (LegacyPrefixes::_66, 0x0FDC, 2),
                SseOpcode::Paddusw => (LegacyPrefixes::_66, 0x0FDD, 2),
                SseOpcode::Pand => (LegacyPrefixes::_66, 0x0FDB, 2),
                SseOpcode::Pandn => (LegacyPrefixes::_66, 0x0FDF, 2),
                SseOpcode::Pavgb => (LegacyPrefixes::_66, 0x0FE0, 2),
                SseOpcode::Pavgw => (LegacyPrefixes::_66, 0x0FE3, 2),
                SseOpcode::Pcmpeqb => (LegacyPrefixes::_66, 0x0F74, 2),
                SseOpcode::Pcmpeqw => (LegacyPrefixes::_66, 0x0F75, 2),
                SseOpcode::Pcmpeqd => (LegacyPrefixes::_66, 0x0F76, 2),
                SseOpcode::Pcmpeqq => (LegacyPrefixes::_66, 0x0F3829, 3),
                SseOpcode::Pcmpgtb => (LegacyPrefixes::_66, 0x0F64, 2),
                SseOpcode::Pcmpgtw => (LegacyPrefixes::_66, 0x0F65, 2),
                SseOpcode::Pcmpgtd => (LegacyPrefixes::_66, 0x0F66, 2),
                SseOpcode::Pcmpgtq => (LegacyPrefixes::_66, 0x0F3837, 3),
                SseOpcode::Pmaxsb => (LegacyPrefixes::_66, 0x0F383C, 3),
                SseOpcode::Pmaxsw => (LegacyPrefixes::_66, 0x0FEE, 2),
                SseOpcode::Pmaxsd => (LegacyPrefixes::_66, 0x0F383D, 3),
                SseOpcode::Pmaxub => (LegacyPrefixes::_66, 0x0FDE, 2),
                SseOpcode::Pmaxuw => (LegacyPrefixes::_66, 0x0F383E, 3),
                SseOpcode::Pmaxud => (LegacyPrefixes::_66, 0x0F383F, 3),
                SseOpcode::Pminsb => (LegacyPrefixes::_66, 0x0F3838, 3),
                SseOpcode::Pminsw => (LegacyPrefixes::_66, 0x0FEA, 2),
                SseOpcode::Pminsd => (LegacyPrefixes::_66, 0x0F3839, 3),
                SseOpcode::Pminub => (LegacyPrefixes::_66, 0x0FDA, 2),
                SseOpcode::Pminuw => (LegacyPrefixes::_66, 0x0F383A, 3),
                SseOpcode::Pminud => (LegacyPrefixes::_66, 0x0F383B, 3),
                SseOpcode::Pmulld => (LegacyPrefixes::_66, 0x0F3840, 3),
                SseOpcode::Pmullw => (LegacyPrefixes::_66, 0x0FD5, 2),
                SseOpcode::Pmuludq => (LegacyPrefixes::_66, 0x0FF4, 2),
                SseOpcode::Por => (LegacyPrefixes::_66, 0x0FEB, 2),
                SseOpcode::Pshufb => (LegacyPrefixes::_66, 0x0F3800, 3),
                SseOpcode::Psubb => (LegacyPrefixes::_66, 0x0FF8, 2),
                SseOpcode::Psubd => (LegacyPrefixes::_66, 0x0FFA, 2),
                SseOpcode::Psubq => (LegacyPrefixes::_66, 0x0FFB, 2),
                SseOpcode::Psubw => (LegacyPrefixes::_66, 0x0FF9, 2),
                SseOpcode::Psubsb => (LegacyPrefixes::_66, 0x0FE8, 2),
                SseOpcode::Psubsw => (LegacyPrefixes::_66, 0x0FE9, 2),
                SseOpcode::Psubusb => (LegacyPrefixes::_66, 0x0FD8, 2),
                SseOpcode::Psubusw => (LegacyPrefixes::_66, 0x0FD9, 2),
                SseOpcode::Punpckhbw => (LegacyPrefixes::_66, 0x0F68, 2),
                SseOpcode::Punpcklbw => (LegacyPrefixes::_66, 0x0F60, 2),
                SseOpcode::Pxor => (LegacyPrefixes::_66, 0x0FEF, 2),
                SseOpcode::Subps => (LegacyPrefixes::None, 0x0F5C, 2),
                SseOpcode::Subpd => (LegacyPrefixes::_66, 0x0F5C, 2),
                SseOpcode::Subss => (LegacyPrefixes::_F3, 0x0F5C, 2),
                SseOpcode::Subsd => (LegacyPrefixes::_F2, 0x0F5C, 2),
                SseOpcode::Xorps => (LegacyPrefixes::None, 0x0F57, 2),
                SseOpcode::Xorpd => (LegacyPrefixes::_66, 0x0F57, 2),
                _ => unimplemented!("Opcode {:?} not implemented", op),
            };

            match src_e {
                RegMem::Reg { reg: reg_e } => {
                    emit_std_reg_reg(sink, prefix, opcode, length, reg_g.to_reg(), *reg_e, rex);
                }
                RegMem::Mem { addr } => {
                    let addr = &addr.finalize(state, sink);
                    emit_std_reg_mem(
                        sink,
                        state,
                        prefix,
                        opcode,
                        length,
                        reg_g.to_reg(),
                        addr,
                        rex,
                    );
                }
            }
        }

        Inst::XmmMinMaxSeq {
            size,
            is_min,
            lhs,
            rhs_dst,
        } => {
            // Generates the following sequence:
            // cmpss/cmpsd %lhs, %rhs_dst
            // jnz do_min_max
            // jp propagate_nan
            //
            // ;; ordered and equal: propagate the sign bit (for -0 vs 0):
            // {and,or}{ss,sd} %lhs, %rhs_dst
            // j done
            //
            // ;; to get the desired NaN behavior (signalling NaN transformed into a quiet NaN, the
            // ;; NaN value is returned), we add both inputs.
            // propagate_nan:
            // add{ss,sd} %lhs, %rhs_dst
            // j done
            //
            // do_min_max:
            // {min,max}{ss,sd} %lhs, %rhs_dst
            //
            // done:
            let done = sink.get_label();
            let propagate_nan = sink.get_label();
            let do_min_max = sink.get_label();

            let (add_op, cmp_op, and_op, or_op, min_max_op) = match size {
                OperandSize::Size32 => (
                    SseOpcode::Addss,
                    SseOpcode::Ucomiss,
                    SseOpcode::Andps,
                    SseOpcode::Orps,
                    if *is_min {
                        SseOpcode::Minss
                    } else {
                        SseOpcode::Maxss
                    },
                ),
                OperandSize::Size64 => (
                    SseOpcode::Addsd,
                    SseOpcode::Ucomisd,
                    SseOpcode::Andpd,
                    SseOpcode::Orpd,
                    if *is_min {
                        SseOpcode::Minsd
                    } else {
                        SseOpcode::Maxsd
                    },
                ),
            };

            let inst = Inst::xmm_cmp_rm_r(cmp_op, RegMem::reg(*lhs), rhs_dst.to_reg());
            inst.emit(sink, info, state);

            one_way_jmp(sink, CC::NZ, do_min_max);
            one_way_jmp(sink, CC::P, propagate_nan);

            // Ordered and equal. The operands are bit-identical unless they are zero
            // and negative zero. These instructions merge the sign bits in that
            // case, and are no-ops otherwise.
            let op = if *is_min { or_op } else { and_op };
            let inst = Inst::xmm_rm_r(op, RegMem::reg(*lhs), *rhs_dst);
            inst.emit(sink, info, state);

            let inst = Inst::jmp_known(done);
            inst.emit(sink, info, state);

            // x86's min/max are not symmetric; if either operand is a NaN, they return the
            // read-only operand: perform an addition between the two operands, which has the
            // desired NaN propagation effects.
            sink.bind_label(propagate_nan);
            let inst = Inst::xmm_rm_r(add_op, RegMem::reg(*lhs), *rhs_dst);
            inst.emit(sink, info, state);

            one_way_jmp(sink, CC::P, done);

            sink.bind_label(do_min_max);
            let inst = Inst::xmm_rm_r(min_max_op, RegMem::reg(*lhs), *rhs_dst);
            inst.emit(sink, info, state);

            sink.bind_label(done);
        }

        Inst::XmmRmRImm {
            op,
            src,
            dst,
            imm,
            is64,
        } => {
            let (prefix, opcode, len) = match op {
                SseOpcode::Cmpps => (LegacyPrefixes::None, 0x0FC2, 2),
                SseOpcode::Cmppd => (LegacyPrefixes::_66, 0x0FC2, 2),
                SseOpcode::Cmpss => (LegacyPrefixes::_F3, 0x0FC2, 2),
                SseOpcode::Cmpsd => (LegacyPrefixes::_F2, 0x0FC2, 2),
                SseOpcode::Insertps => (LegacyPrefixes::_66, 0x0F3A21, 3),
                SseOpcode::Pinsrb => (LegacyPrefixes::_66, 0x0F3A20, 3),
                SseOpcode::Pinsrw => (LegacyPrefixes::_66, 0x0FC4, 2),
                SseOpcode::Pinsrd => (LegacyPrefixes::_66, 0x0F3A22, 3),
                SseOpcode::Pextrb => (LegacyPrefixes::_66, 0x0F3A14, 3),
                SseOpcode::Pextrw => (LegacyPrefixes::_66, 0x0FC5, 2),
                SseOpcode::Pextrd => (LegacyPrefixes::_66, 0x0F3A16, 3),
                SseOpcode::Pshufd => (LegacyPrefixes::_66, 0x0F70, 2),
                _ => unimplemented!("Opcode {:?} not implemented", op),
            };
            let rex = if *is64 {
                RexFlags::set_w()
            } else {
                RexFlags::clear_w()
            };
            let regs_swapped = match *op {
                // These opcodes (and not the SSE2 version of PEXTRW) flip the operand
                // encoding: `dst` in ModRM's r/m, `src` in ModRM's reg field.
                SseOpcode::Pextrb | SseOpcode::Pextrd => true,
                // The rest of the opcodes have the customary encoding: `dst` in ModRM's reg,
                // `src` in ModRM's r/m field.
                _ => false,
            };
            match src {
                RegMem::Reg { reg } => {
                    if regs_swapped {
                        emit_std_reg_reg(sink, prefix, opcode, len, *reg, dst.to_reg(), rex);
                    } else {
                        emit_std_reg_reg(sink, prefix, opcode, len, dst.to_reg(), *reg, rex);
                    }
                }
                RegMem::Mem { addr } => {
                    let addr = &addr.finalize(state, sink);
                    assert!(
                        !regs_swapped,
                        "No existing way to encode a mem argument in the ModRM r/m field."
                    );
                    emit_std_reg_mem(sink, state, prefix, opcode, len, dst.to_reg(), addr, rex);
                }
            }
            sink.put1(*imm);
        }

        Inst::XmmUninitializedValue { .. } => {
            // This instruction format only exists to declare a register as a `def`; no code is
            // emitted.
        }

        Inst::XmmMovRM { op, src, dst } => {
            let (prefix, opcode) = match op {
                SseOpcode::Movaps => (LegacyPrefixes::None, 0x0F29),
                SseOpcode::Movapd => (LegacyPrefixes::_66, 0x0F29),
                SseOpcode::Movdqa => (LegacyPrefixes::_66, 0x0F7F),
                SseOpcode::Movdqu => (LegacyPrefixes::_F3, 0x0F7F),
                SseOpcode::Movss => (LegacyPrefixes::_F3, 0x0F11),
                SseOpcode::Movsd => (LegacyPrefixes::_F2, 0x0F11),
                SseOpcode::Movups => (LegacyPrefixes::None, 0x0F11),
                SseOpcode::Movupd => (LegacyPrefixes::_66, 0x0F11),
                _ => unimplemented!("Opcode {:?} not implemented", op),
            };
            let dst = &dst.finalize(state, sink);
            emit_std_reg_mem(
                sink,
                state,
                prefix,
                opcode,
                2,
                *src,
                dst,
                RexFlags::clear_w(),
            );
        }

        Inst::XmmToGpr {
            op,
            src,
            dst,
            dst_size,
        } => {
            let (prefix, opcode, dst_first) = match op {
                SseOpcode::Cvttss2si => (LegacyPrefixes::_F3, 0x0F2C, true),
                SseOpcode::Cvttsd2si => (LegacyPrefixes::_F2, 0x0F2C, true),
                // Movd and movq use the same opcode; the presence of the REX prefix (set below)
                // actually determines which is used.
                SseOpcode::Movd | SseOpcode::Movq => (LegacyPrefixes::_66, 0x0F7E, false),
                SseOpcode::Movmskps => (LegacyPrefixes::None, 0x0F50, true),
                SseOpcode::Movmskpd => (LegacyPrefixes::_66, 0x0F50, true),
                SseOpcode::Pmovmskb => (LegacyPrefixes::_66, 0x0FD7, true),
                _ => panic!("unexpected opcode {:?}", op),
            };
            let rex = match dst_size {
                OperandSize::Size32 => RexFlags::clear_w(),
                OperandSize::Size64 => RexFlags::set_w(),
            };

            let (src, dst) = if dst_first {
                (dst.to_reg(), *src)
            } else {
                (*src, dst.to_reg())
            };

            emit_std_reg_reg(sink, prefix, opcode, 2, src, dst, rex);
        }

        Inst::GprToXmm {
            op,
            src: src_e,
            dst: reg_g,
            src_size,
        } => {
            let (prefix, opcode) = match op {
                // Movd and movq use the same opcode; the presence of the REX prefix (set below)
                // actually determines which is used.
                SseOpcode::Movd | SseOpcode::Movq => (LegacyPrefixes::_66, 0x0F6E),
                SseOpcode::Cvtsi2ss => (LegacyPrefixes::_F3, 0x0F2A),
                SseOpcode::Cvtsi2sd => (LegacyPrefixes::_F2, 0x0F2A),
                _ => panic!("unexpected opcode {:?}", op),
            };
            let rex = match *src_size {
                OperandSize::Size32 => RexFlags::clear_w(),
                OperandSize::Size64 => RexFlags::set_w(),
            };
            match src_e {
                RegMem::Reg { reg: reg_e } => {
                    emit_std_reg_reg(sink, prefix, opcode, 2, reg_g.to_reg(), *reg_e, rex);
                }
                RegMem::Mem { addr } => {
                    let addr = &addr.finalize(state, sink);
                    emit_std_reg_mem(sink, state, prefix, opcode, 2, reg_g.to_reg(), addr, rex);
                }
            }
        }

        Inst::XmmCmpRmR { op, src, dst } => {
            let rex = RexFlags::clear_w();
            let (prefix, opcode, len) = match op {
                SseOpcode::Ptest => (LegacyPrefixes::_66, 0x0F3817, 3),
                SseOpcode::Ucomisd => (LegacyPrefixes::_66, 0x0F2E, 2),
                SseOpcode::Ucomiss => (LegacyPrefixes::None, 0x0F2E, 2),
                _ => unimplemented!("Emit xmm cmp rm r"),
            };

            match src {
                RegMem::Reg { reg } => {
                    emit_std_reg_reg(sink, prefix, opcode, len, *dst, *reg, rex);
                }
                RegMem::Mem { addr } => {
                    let addr = &addr.finalize(state, sink);
                    emit_std_reg_mem(sink, state, prefix, opcode, len, *dst, addr, rex);
                }
            }
        }

        Inst::CvtUint64ToFloatSeq {
            to_f64,
            src,
            dst,
            tmp_gpr1,
            tmp_gpr2,
        } => {
            // Note: this sequence is specific to 64-bit mode; a 32-bit mode would require a
            // different sequence.
            //
            // Emit the following sequence:
            //
            //  cmp 0, %src
            //  jl handle_negative
            //
            //  ;; handle positive, which can't overflow
            //  cvtsi2sd/cvtsi2ss %src, %dst
            //  j done
            //
            //  ;; handle negative: see below for an explanation of what it's doing.
            //  handle_negative:
            //  mov %src, %tmp_gpr1
            //  shr $1, %tmp_gpr1
            //  mov %src, %tmp_gpr2
            //  and $1, %tmp_gpr2
            //  or %tmp_gpr1, %tmp_gpr2
            //  cvtsi2sd/cvtsi2ss %tmp_gpr2, %dst
            //  addsd/addss %dst, %dst
            //
            //  done:

            assert_ne!(src, tmp_gpr1);
            assert_ne!(src, tmp_gpr2);
            assert_ne!(tmp_gpr1, tmp_gpr2);

            let handle_negative = sink.get_label();
            let done = sink.get_label();

            // If x seen as a signed int64 is not negative, a signed-conversion will do the right
            // thing.
            // TODO use tst src, src here.
            let inst = Inst::cmp_rmi_r(8, RegMemImm::imm(0), src.to_reg());
            inst.emit(sink, info, state);

            one_way_jmp(sink, CC::L, handle_negative);

            // Handle a positive int64, which is the "easy" case: a signed conversion will do the
            // right thing.
            emit_signed_cvt(sink, info, state, src.to_reg(), *dst, *to_f64);

            let inst = Inst::jmp_known(done);
            inst.emit(sink, info, state);

            sink.bind_label(handle_negative);

            // Divide x by two to get it in range for the signed conversion, keep the LSB, and
            // scale it back up on the FP side.
            let inst = Inst::gen_move(*tmp_gpr1, src.to_reg(), types::I64);
            inst.emit(sink, info, state);

            // tmp_gpr1 := src >> 1
            let inst = Inst::shift_r(8, ShiftKind::ShiftRightLogical, Some(1), *tmp_gpr1);
            inst.emit(sink, info, state);

            let inst = Inst::gen_move(*tmp_gpr2, src.to_reg(), types::I64);
            inst.emit(sink, info, state);

            let inst = Inst::alu_rmi_r(
                true, /* 64bits */
                AluRmiROpcode::And,
                RegMemImm::imm(1),
                *tmp_gpr2,
            );
            inst.emit(sink, info, state);

            let inst = Inst::alu_rmi_r(
                true, /* 64bits */
                AluRmiROpcode::Or,
                RegMemImm::reg(tmp_gpr1.to_reg()),
                *tmp_gpr2,
            );
            inst.emit(sink, info, state);

            emit_signed_cvt(sink, info, state, tmp_gpr2.to_reg(), *dst, *to_f64);

            let add_op = if *to_f64 {
                SseOpcode::Addsd
            } else {
                SseOpcode::Addss
            };
            let inst = Inst::xmm_rm_r(add_op, RegMem::reg(dst.to_reg()), *dst);
            inst.emit(sink, info, state);

            sink.bind_label(done);
        }

        Inst::CvtFloatToSintSeq {
            src_size,
            dst_size,
            is_saturating,
            src,
            dst,
            tmp_gpr,
            tmp_xmm,
        } => {
            // Emits the following common sequence:
            //
            // cvttss2si/cvttsd2si %src, %dst
            // cmp %dst, 1
            // jno done
            //
            // Then, for saturating conversions:
            //
            // ;; check for NaN
            // cmpss/cmpsd %src, %src
            // jnp not_nan
            // xor %dst, %dst
            //
            // ;; positive inputs get saturated to INT_MAX; negative ones to INT_MIN, which is
            // ;; already in %dst.
            // xorpd %tmp_xmm, %tmp_xmm
            // cmpss/cmpsd %src, %tmp_xmm
            // jnb done
            // mov/movaps $INT_MAX, %dst
            //
            // done:
            //
            // Then, for non-saturating conversions:
            //
            // ;; check for NaN
            // cmpss/cmpsd %src, %src
            // jnp not_nan
            // ud2 trap BadConversionToInteger
            //
            // ;; check if INT_MIN was the correct result, against a magic constant:
            // not_nan:
            // movaps/mov $magic, %tmp_gpr
            // movq/movd %tmp_gpr, %tmp_xmm
            // cmpss/cmpsd %tmp_xmm, %src
            // jnb/jnbe $check_positive
            // ud2 trap IntegerOverflow
            //
            // ;; if positive, it was a real overflow
            // check_positive:
            // xorpd %tmp_xmm, %tmp_xmm
            // cmpss/cmpsd %src, %tmp_xmm
            // jnb done
            // ud2 trap IntegerOverflow
            //
            // done:

            let src = src.to_reg();

            let (cast_op, cmp_op, trunc_op) = match src_size {
                OperandSize::Size64 => (SseOpcode::Movq, SseOpcode::Ucomisd, SseOpcode::Cvttsd2si),
                OperandSize::Size32 => (SseOpcode::Movd, SseOpcode::Ucomiss, SseOpcode::Cvttss2si),
            };

            let done = sink.get_label();
            let not_nan = sink.get_label();

            // The truncation.
            let inst = Inst::xmm_to_gpr(trunc_op, src, *dst, *dst_size);
            inst.emit(sink, info, state);

            // Compare against 1, in case of overflow the dst operand was INT_MIN.
            let inst = Inst::cmp_rmi_r(dst_size.to_bytes(), RegMemImm::imm(1), dst.to_reg());
            inst.emit(sink, info, state);

            one_way_jmp(sink, CC::NO, done); // no overflow => done

            // Check for NaN.

            let inst = Inst::xmm_cmp_rm_r(cmp_op, RegMem::reg(src), src);
            inst.emit(sink, info, state);

            one_way_jmp(sink, CC::NP, not_nan); // go to not_nan if not a NaN

            if *is_saturating {
                // For NaN, emit 0.
                let inst = Inst::alu_rmi_r(
                    *dst_size == OperandSize::Size64,
                    AluRmiROpcode::Xor,
                    RegMemImm::reg(dst.to_reg()),
                    *dst,
                );
                inst.emit(sink, info, state);

                let inst = Inst::jmp_known(done);
                inst.emit(sink, info, state);

                sink.bind_label(not_nan);

                // If the input was positive, saturate to INT_MAX.

                // Zero out tmp_xmm.
                let inst =
                    Inst::xmm_rm_r(SseOpcode::Xorpd, RegMem::reg(tmp_xmm.to_reg()), *tmp_xmm);
                inst.emit(sink, info, state);

                let inst = Inst::xmm_cmp_rm_r(cmp_op, RegMem::reg(src), tmp_xmm.to_reg());
                inst.emit(sink, info, state);

                // Jump if >= to done.
                one_way_jmp(sink, CC::NB, done);

                // Otherwise, put INT_MAX.
                if *dst_size == OperandSize::Size64 {
                    let inst = Inst::imm(OperandSize::Size64, 0x7fffffffffffffff, *dst);
                    inst.emit(sink, info, state);
                } else {
                    let inst = Inst::imm(OperandSize::Size32, 0x7fffffff, *dst);
                    inst.emit(sink, info, state);
                }
            } else {
                let check_positive = sink.get_label();

                let inst = Inst::trap(TrapCode::BadConversionToInteger);
                inst.emit(sink, info, state);

                // Check if INT_MIN was the correct result: determine the smallest floating point
                // number that would convert to INT_MIN, put it in a temporary register, and compare
                // against the src register.
                // If the src register is less (or in some cases, less-or-equal) than the threshold,
                // trap!

                sink.bind_label(not_nan);

                let mut no_overflow_cc = CC::NB; // >=
                let output_bits = dst_size.to_bits();
                match *src_size {
                    OperandSize::Size32 => {
                        let cst = Ieee32::pow2(output_bits - 1).neg().bits();
                        let inst = Inst::imm(OperandSize::Size32, cst as u64, *tmp_gpr);
                        inst.emit(sink, info, state);
                    }
                    OperandSize::Size64 => {
                        // An f64 can represent `i32::min_value() - 1` exactly with precision to spare,
                        // so there are values less than -2^(N-1) that convert correctly to INT_MIN.
                        let cst = if output_bits < 64 {
                            no_overflow_cc = CC::NBE; // >
                            Ieee64::fcvt_to_sint_negative_overflow(output_bits)
                        } else {
                            Ieee64::pow2(output_bits - 1).neg()
                        };
                        let inst = Inst::imm(OperandSize::Size64, cst.bits(), *tmp_gpr);
                        inst.emit(sink, info, state);
                    }
                }

                let inst =
                    Inst::gpr_to_xmm(cast_op, RegMem::reg(tmp_gpr.to_reg()), *src_size, *tmp_xmm);
                inst.emit(sink, info, state);

                let inst = Inst::xmm_cmp_rm_r(cmp_op, RegMem::reg(tmp_xmm.to_reg()), src);
                inst.emit(sink, info, state);

                // jump over trap if src >= or > threshold
                one_way_jmp(sink, no_overflow_cc, check_positive);

                let inst = Inst::trap(TrapCode::IntegerOverflow);
                inst.emit(sink, info, state);

                // If positive, it was a real overflow.

                sink.bind_label(check_positive);

                // Zero out the tmp_xmm register.
                let inst =
                    Inst::xmm_rm_r(SseOpcode::Xorpd, RegMem::reg(tmp_xmm.to_reg()), *tmp_xmm);
                inst.emit(sink, info, state);

                let inst = Inst::xmm_cmp_rm_r(cmp_op, RegMem::reg(src), tmp_xmm.to_reg());
                inst.emit(sink, info, state);

                one_way_jmp(sink, CC::NB, done); // jump over trap if 0 >= src

                let inst = Inst::trap(TrapCode::IntegerOverflow);
                inst.emit(sink, info, state);
            }

            sink.bind_label(done);
        }

        Inst::CvtFloatToUintSeq {
            src_size,
            dst_size,
            is_saturating,
            src,
            dst,
            tmp_gpr,
            tmp_xmm,
        } => {
            // The only difference in behavior between saturating and non-saturating is how we
            // handle errors. Emits the following sequence:
            //
            // movaps/mov 2**(int_width - 1), %tmp_gpr
            // movq/movd %tmp_gpr, %tmp_xmm
            // cmpss/cmpsd %tmp_xmm, %src
            // jnb is_large
            //
            // ;; check for NaN inputs
            // jnp not_nan
            // -- non-saturating: ud2 trap BadConversionToInteger
            // -- saturating: xor %dst, %dst; j done
            //
            // not_nan:
            // cvttss2si/cvttsd2si %src, %dst
            // cmp 0, %dst
            // jnl done
            // -- non-saturating: ud2 trap IntegerOverflow
            // -- saturating: xor %dst, %dst; j done
            //
            // is_large:
            // subss/subsd %tmp_xmm, %src ; <-- we clobber %src here
            // cvttss2si/cvttss2sd %tmp_x, %dst
            // cmp 0, %dst
            // jnl next_is_large
            // -- non-saturating: ud2 trap IntegerOverflow
            // -- saturating: movaps $UINT_MAX, %dst; j done
            //
            // next_is_large:
            // add 2**(int_width -1), %dst ;; 2 instructions for 64-bits integers
            //
            // done:

            assert_ne!(tmp_xmm, src, "tmp_xmm clobbers src!");

            let (sub_op, cast_op, cmp_op, trunc_op) = if *src_size == OperandSize::Size64 {
                (
                    SseOpcode::Subsd,
                    SseOpcode::Movq,
                    SseOpcode::Ucomisd,
                    SseOpcode::Cvttsd2si,
                )
            } else {
                (
                    SseOpcode::Subss,
                    SseOpcode::Movd,
                    SseOpcode::Ucomiss,
                    SseOpcode::Cvttss2si,
                )
            };

            let done = sink.get_label();

            let cst = if *src_size == OperandSize::Size64 {
                Ieee64::pow2(dst_size.to_bits() - 1).bits()
            } else {
                Ieee32::pow2(dst_size.to_bits() - 1).bits() as u64
            };

            let inst = Inst::imm(*src_size, cst, *tmp_gpr);
            inst.emit(sink, info, state);

            let inst =
                Inst::gpr_to_xmm(cast_op, RegMem::reg(tmp_gpr.to_reg()), *src_size, *tmp_xmm);
            inst.emit(sink, info, state);

            let inst = Inst::xmm_cmp_rm_r(cmp_op, RegMem::reg(tmp_xmm.to_reg()), src.to_reg());
            inst.emit(sink, info, state);

            let handle_large = sink.get_label();
            one_way_jmp(sink, CC::NB, handle_large); // jump to handle_large if src >= large_threshold

            let not_nan = sink.get_label();
            one_way_jmp(sink, CC::NP, not_nan); // jump over trap if not NaN

            if *is_saturating {
                // Emit 0.
                let inst = Inst::alu_rmi_r(
                    *dst_size == OperandSize::Size64,
                    AluRmiROpcode::Xor,
                    RegMemImm::reg(dst.to_reg()),
                    *dst,
                );
                inst.emit(sink, info, state);

                let inst = Inst::jmp_known(done);
                inst.emit(sink, info, state);
            } else {
                // Trap.
                let inst = Inst::trap(TrapCode::BadConversionToInteger);
                inst.emit(sink, info, state);
            }

            sink.bind_label(not_nan);

            // Actual truncation for small inputs: if the result is not positive, then we had an
            // overflow.

            let inst = Inst::xmm_to_gpr(trunc_op, src.to_reg(), *dst, *dst_size);
            inst.emit(sink, info, state);

            let inst = Inst::cmp_rmi_r(dst_size.to_bytes(), RegMemImm::imm(0), dst.to_reg());
            inst.emit(sink, info, state);

            one_way_jmp(sink, CC::NL, done); // if dst >= 0, jump to done

            if *is_saturating {
                // The input was "small" (< 2**(width -1)), so the only way to get an integer
                // overflow is because the input was too small: saturate to the min value, i.e. 0.
                let inst = Inst::alu_rmi_r(
                    *dst_size == OperandSize::Size64,
                    AluRmiROpcode::Xor,
                    RegMemImm::reg(dst.to_reg()),
                    *dst,
                );
                inst.emit(sink, info, state);

                let inst = Inst::jmp_known(done);
                inst.emit(sink, info, state);
            } else {
                // Trap.
                let inst = Inst::trap(TrapCode::IntegerOverflow);
                inst.emit(sink, info, state);
            }

            // Now handle large inputs.

            sink.bind_label(handle_large);

            let inst = Inst::xmm_rm_r(sub_op, RegMem::reg(tmp_xmm.to_reg()), *src);
            inst.emit(sink, info, state);

            let inst = Inst::xmm_to_gpr(trunc_op, src.to_reg(), *dst, *dst_size);
            inst.emit(sink, info, state);

            let inst = Inst::cmp_rmi_r(dst_size.to_bytes(), RegMemImm::imm(0), dst.to_reg());
            inst.emit(sink, info, state);

            let next_is_large = sink.get_label();
            one_way_jmp(sink, CC::NL, next_is_large); // if dst >= 0, jump to next_is_large

            if *is_saturating {
                // The input was "large" (>= 2**(width -1)), so the only way to get an integer
                // overflow is because the input was too large: saturate to the max value.
                let inst = Inst::imm(
                    OperandSize::Size64,
                    if *dst_size == OperandSize::Size64 {
                        u64::max_value()
                    } else {
                        u32::max_value() as u64
                    },
                    *dst,
                );
                inst.emit(sink, info, state);

                let inst = Inst::jmp_known(done);
                inst.emit(sink, info, state);
            } else {
                let inst = Inst::trap(TrapCode::IntegerOverflow);
                inst.emit(sink, info, state);
            }

            sink.bind_label(next_is_large);

            if *dst_size == OperandSize::Size64 {
                let inst = Inst::imm(OperandSize::Size64, 1 << 63, *tmp_gpr);
                inst.emit(sink, info, state);

                let inst = Inst::alu_rmi_r(
                    true,
                    AluRmiROpcode::Add,
                    RegMemImm::reg(tmp_gpr.to_reg()),
                    *dst,
                );
                inst.emit(sink, info, state);
            } else {
                let inst =
                    Inst::alu_rmi_r(false, AluRmiROpcode::Add, RegMemImm::imm(1 << 31), *dst);
                inst.emit(sink, info, state);
            }

            sink.bind_label(done);
        }

        Inst::LoadExtName { dst, name, offset } => {
            // The full address can be encoded in the register, with a relocation.
            // Generates: movabsq $name, %dst
            let enc_dst = int_reg_enc(dst.to_reg());
            sink.put1(0x48 | ((enc_dst >> 3) & 1));
            sink.put1(0xB8 | (enc_dst & 7));
            emit_reloc(sink, state, Reloc::Abs8, name, *offset);
            if info.flags().emit_all_ones_funcaddrs() {
                sink.put8(u64::max_value());
            } else {
                sink.put8(0);
            }
        }

        Inst::LockCmpxchg { ty, src, dst } => {
            // lock cmpxchg{b,w,l,q} %src, (dst)
            // Note that 0xF0 is the Lock prefix.
            let (prefix, rex, opcodes) = match *ty {
                types::I8 => {
                    let mut rex_flags = RexFlags::clear_w();
                    let enc_src = int_reg_enc(*src);
                    if enc_src >= 4 && enc_src <= 7 {
                        rex_flags.always_emit();
                    };
                    (LegacyPrefixes::_F0, rex_flags, 0x0FB0)
                }
                types::I16 => (LegacyPrefixes::_66F0, RexFlags::clear_w(), 0x0FB1),
                types::I32 => (LegacyPrefixes::_F0, RexFlags::clear_w(), 0x0FB1),
                types::I64 => (LegacyPrefixes::_F0, RexFlags::set_w(), 0x0FB1),
                _ => unreachable!(),
            };
            let amode = dst.finalize(state, sink);
            emit_std_reg_mem(sink, state, prefix, opcodes, 2, *src, &amode, rex);
        }

        Inst::AtomicRmwSeq { ty, op } => {
            // Emit this:
            //
            //    mov{zbq,zwq,zlq,q}     (%r9), %rax  // rax = old value
            //   again:
            //    movq                   %rax, %r11   // rax = old value, r11 = old value
            //    `op`q                  %r10, %r11   // rax = old value, r11 = new value
            //    lock cmpxchg{b,w,l,q}  %r11, (%r9)  // try to store new value
            //    jnz again // If this is taken, rax will have a "revised" old value
            //
            // Operand conventions:
            //    IN:  %r9 (addr), %r10 (2nd arg for `op`)
            //    OUT: %rax (old value), %r11 (trashed), %rflags (trashed)
            //
            // In the case where the operation is 'xchg', the "`op`q" instruction is instead
            //   movq                    %r10, %r11
            // so that we simply write in the destination, the "2nd arg for `op`".
            let rax = regs::rax();
            let r9 = regs::r9();
            let r10 = regs::r10();
            let r11 = regs::r11();
            let rax_w = Writable::from_reg(rax);
            let r11_w = Writable::from_reg(r11);
            let amode = Amode::imm_reg(0, r9);
            let again_label = sink.get_label();

            // mov{zbq,zwq,zlq,q} (%r9), %rax
            // No need to call `add_trap` here, since the `i1` emit will do that.
            let i1 = Inst::load(*ty, amode.clone(), rax_w, ExtKind::ZeroExtend);
            i1.emit(sink, info, state);

            // again:
            sink.bind_label(again_label);

            // movq %rax, %r11
            let i2 = Inst::mov_r_r(true, rax, r11_w);
            i2.emit(sink, info, state);

            // opq %r10, %r11
            let r10_rmi = RegMemImm::reg(r10);
            let i3 = if *op == inst_common::AtomicRmwOp::Xchg {
                Inst::mov_r_r(true, r10, r11_w)
            } else {
                let alu_op = match op {
                    inst_common::AtomicRmwOp::Add => AluRmiROpcode::Add,
                    inst_common::AtomicRmwOp::Sub => AluRmiROpcode::Sub,
                    inst_common::AtomicRmwOp::And => AluRmiROpcode::And,
                    inst_common::AtomicRmwOp::Or => AluRmiROpcode::Or,
                    inst_common::AtomicRmwOp::Xor => AluRmiROpcode::Xor,
                    inst_common::AtomicRmwOp::Xchg => unreachable!(),
                };
                Inst::alu_rmi_r(true, alu_op, r10_rmi, r11_w)
            };
            i3.emit(sink, info, state);

            // lock cmpxchg{b,w,l,q} %r11, (%r9)
            // No need to call `add_trap` here, since the `i4` emit will do that.
            let i4 = Inst::LockCmpxchg {
                ty: *ty,
                src: r11,
                dst: amode.into(),
            };
            i4.emit(sink, info, state);

            // jnz again
            one_way_jmp(sink, CC::NZ, again_label);
        }

        Inst::Fence { kind } => {
            sink.put1(0x0F);
            sink.put1(0xAE);
            match kind {
                FenceKind::MFence => sink.put1(0xF0), // mfence = 0F AE F0
                FenceKind::LFence => sink.put1(0xE8), // lfence = 0F AE E8
                FenceKind::SFence => sink.put1(0xF8), // sfence = 0F AE F8
            }
        }

        Inst::Hlt => {
            sink.put1(0xcc);
        }

        Inst::Ud2 { trap_code } => {
            let cur_srcloc = state.cur_srcloc();
            sink.add_trap(cur_srcloc, *trap_code);
            if let Some(s) = state.take_stack_map() {
                sink.add_stack_map(StackMapExtent::UpcomingBytes(2), s);
            }
            sink.put1(0x0f);
            sink.put1(0x0b);
        }

        Inst::VirtualSPOffsetAdj { offset } => {
            debug!(
                "virtual sp offset adjusted by {} -> {}",
                offset,
                state.virtual_sp_offset + offset
            );
            state.virtual_sp_offset += offset;
        }

        Inst::Nop { len } => {
            // These encodings can all be found in Intel's architecture manual, at the NOP
            // instruction description.
            let mut len = *len;
            while len != 0 {
                let emitted = u8::min(len, 9);
                match emitted {
                    0 => {}
                    1 => sink.put1(0x90), // NOP
                    2 => {
                        // 66 NOP
                        sink.put1(0x66);
                        sink.put1(0x90);
                    }
                    3 => {
                        // NOP [EAX]
                        sink.put1(0x0F);
                        sink.put1(0x1F);
                        sink.put1(0x00);
                    }
                    4 => {
                        // NOP 0(EAX), with 0 a 1-byte immediate.
                        sink.put1(0x0F);
                        sink.put1(0x1F);
                        sink.put1(0x40);
                        sink.put1(0x00);
                    }
                    5 => {
                        // NOP [EAX, EAX, 1]
                        sink.put1(0x0F);
                        sink.put1(0x1F);
                        sink.put1(0x44);
                        sink.put1(0x00);
                        sink.put1(0x00);
                    }
                    6 => {
                        // 66 NOP [EAX, EAX, 1]
                        sink.put1(0x66);
                        sink.put1(0x0F);
                        sink.put1(0x1F);
                        sink.put1(0x44);
                        sink.put1(0x00);
                        sink.put1(0x00);
                    }
                    7 => {
                        // NOP 0[EAX], but 0 is a 4 bytes immediate.
                        sink.put1(0x0F);
                        sink.put1(0x1F);
                        sink.put1(0x80);
                        sink.put1(0x00);
                        sink.put1(0x00);
                        sink.put1(0x00);
                        sink.put1(0x00);
                    }
                    8 => {
                        // NOP 0[EAX, EAX, 1], with 0 a 4 bytes immediate.
                        sink.put1(0x0F);
                        sink.put1(0x1F);
                        sink.put1(0x84);
                        sink.put1(0x00);
                        sink.put1(0x00);
                        sink.put1(0x00);
                        sink.put1(0x00);
                        sink.put1(0x00);
                    }
                    9 => {
                        // 66 NOP 0[EAX, EAX, 1], with 0 a 4 bytes immediate.
                        sink.put1(0x66);
                        sink.put1(0x0F);
                        sink.put1(0x1F);
                        sink.put1(0x84);
                        sink.put1(0x00);
                        sink.put1(0x00);
                        sink.put1(0x00);
                        sink.put1(0x00);
                        sink.put1(0x00);
                    }
                    _ => unreachable!(),
                }
                len -= emitted;
            }
        }

        Inst::EpiloguePlaceholder => {
            // Generate no code.
        }
    }

    state.clear_post_insn();
}
