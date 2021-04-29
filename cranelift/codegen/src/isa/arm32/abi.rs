//! Implementation of the 32-bit ARM ABI.

use crate::ir;
use crate::ir::types::*;
use crate::isa;
use crate::isa::arm32::inst::*;
use crate::machinst::*;
use crate::settings;
use crate::{CodegenError, CodegenResult};
use alloc::boxed::Box;
use alloc::vec::Vec;
use regalloc::{RealReg, Reg, RegClass, Set, Writable};
use smallvec::{smallvec, SmallVec};

/// Support for the ARM ABI from the callee side (within a function body).
pub(crate) type Arm32ABICallee = ABICalleeImpl<Arm32MachineDeps>;

/// Support for the ARM ABI from the caller side (at a callsite).
pub(crate) type Arm32ABICaller = ABICallerImpl<Arm32MachineDeps>;

/// This is the limit for the size of argument and return-value areas on the
/// stack. We place a reasonable limit here to avoid integer overflow issues
/// with 32-bit arithmetic: for now, 128 MB.
static STACK_ARG_RET_SIZE_LIMIT: u64 = 128 * 1024 * 1024;

/// ARM-specific ABI behavior. This struct just serves as an implementation
/// point for the trait; it is never actually instantiated.
pub(crate) struct Arm32MachineDeps;

impl Into<AMode> for StackAMode {
    fn into(self) -> AMode {
        match self {
            StackAMode::FPOffset(off, ty) => AMode::FPOffset(off, ty),
            StackAMode::NominalSPOffset(off, ty) => AMode::NominalSPOffset(off, ty),
            StackAMode::SPOffset(off, ty) => AMode::SPOffset(off, ty),
        }
    }
}

impl ABIMachineSpec for Arm32MachineDeps {
    type I = Inst;

    fn word_bits() -> u32 {
        32
    }

    /// Return required stack alignment in bytes.
    fn stack_align(_call_conv: isa::CallConv) -> u32 {
        8
    }

    fn compute_arg_locs(
        _call_conv: isa::CallConv,
        _flags: &settings::Flags,
        params: &[ir::AbiParam],
        args_or_rets: ArgsOrRets,
        add_ret_area_ptr: bool,
    ) -> CodegenResult<(Vec<ABIArg>, i64, Option<usize>)> {
        let mut next_rreg = 0;
        let mut next_stack: u64 = 0;
        let mut ret = vec![];
        let mut stack_args = vec![];

        let max_reg_val = 4; // r0-r3

        for i in 0..params.len() {
            let param = params[i];

            // Validate "purpose".
            match &param.purpose {
                &ir::ArgumentPurpose::VMContext
                | &ir::ArgumentPurpose::Normal
                | &ir::ArgumentPurpose::StackLimit
                | &ir::ArgumentPurpose::SignatureId => {}
                _ => panic!(
                    "Unsupported argument purpose {:?} in signature: {:?}",
                    param.purpose, params
                ),
            }
            assert!(param.value_type.bits() <= 32);

            if next_rreg < max_reg_val {
                let reg = rreg(next_rreg);

                ret.push(ABIArg::reg(
                    reg.to_real_reg(),
                    param.value_type,
                    param.extension,
                    param.purpose,
                ));
                next_rreg += 1;
            } else {
                // Arguments are stored on stack in reversed order.
                // https://static.docs.arm.com/ihi0042/g/aapcs32.pdf

                // Stack offset is not known yet. Store param info for later.
                stack_args.push((param.value_type, param.extension, param.purpose));
                next_stack += 4;
            }
        }

        let extra_arg = if add_ret_area_ptr {
            debug_assert!(args_or_rets == ArgsOrRets::Args);
            if next_rreg < max_reg_val {
                ret.push(ABIArg::reg(
                    rreg(next_rreg).to_real_reg(),
                    I32,
                    ir::ArgumentExtension::None,
                    ir::ArgumentPurpose::Normal,
                ));
            } else {
                stack_args.push((
                    I32,
                    ir::ArgumentExtension::None,
                    ir::ArgumentPurpose::Normal,
                ));
                next_stack += 4;
            }
            Some(ret.len() - 1)
        } else {
            None
        };

        // Now we can assign proper stack offsets to params.
        let max_stack = next_stack;
        for (ty, ext, purpose) in stack_args.into_iter().rev() {
            next_stack -= 4;
            ret.push(ABIArg::stack(
                (max_stack - next_stack) as i64,
                ty,
                ext,
                purpose,
            ));
        }
        assert_eq!(next_stack, 0);

        next_stack = (next_stack + 7) & !7;

        // To avoid overflow issues, limit the arg/return size to something
        // reasonable -- here, 128 MB.
        if next_stack > STACK_ARG_RET_SIZE_LIMIT {
            return Err(CodegenError::ImplLimitExceeded);
        }

        Ok((ret, next_stack as i64, extra_arg))
    }

    fn fp_to_arg_offset(_call_conv: isa::CallConv, _flags: &settings::Flags) -> i64 {
        8 // frame pointer and link register
    }

    fn gen_load_stack(mem: StackAMode, into_reg: Writable<Reg>, ty: Type) -> Inst {
        Inst::gen_load(into_reg, mem.into(), ty)
    }

    fn gen_store_stack(mem: StackAMode, from_reg: Reg, ty: Type) -> Inst {
        Inst::gen_store(from_reg, mem.into(), ty)
    }

    fn gen_move(to_reg: Writable<Reg>, from_reg: Reg, ty: Type) -> Inst {
        Inst::gen_move(to_reg, from_reg, ty)
    }

    fn gen_extend(
        to_reg: Writable<Reg>,
        from_reg: Reg,
        is_signed: bool,
        from_bits: u8,
        to_bits: u8,
    ) -> Inst {
        assert!(to_bits == 32);
        assert!(from_bits < 32);
        Inst::Extend {
            rd: to_reg,
            rm: from_reg,
            signed: is_signed,
            from_bits,
        }
    }

    fn gen_ret() -> Inst {
        Inst::Ret
    }

    fn gen_epilogue_placeholder() -> Inst {
        Inst::EpiloguePlaceholder
    }

    fn gen_add_imm(into_reg: Writable<Reg>, from_reg: Reg, imm: u32) -> SmallInstVec<Inst> {
        let mut insts = SmallVec::new();

        if let Some(imm12) = UImm12::maybe_from_i64(imm as i64) {
            insts.push(Inst::AluRRImm12 {
                alu_op: ALUOp::Add,
                rd: into_reg,
                rn: from_reg,
                imm12,
            });
        } else {
            let scratch2 = writable_tmp2_reg();
            insts.extend(Inst::load_constant(scratch2, imm));
            insts.push(Inst::AluRRRShift {
                alu_op: ALUOp::Add,
                rd: into_reg,
                rn: from_reg,
                rm: scratch2.to_reg(),
                shift: None,
            });
        }
        insts
    }

    fn gen_stack_lower_bound_trap(limit_reg: Reg) -> SmallInstVec<Inst> {
        let mut insts = SmallVec::new();
        insts.push(Inst::Cmp {
            rn: sp_reg(),
            rm: limit_reg,
        });
        insts.push(Inst::TrapIf {
            trap_info: ir::TrapCode::StackOverflow,
            // Here `Lo` == "less than" when interpreting the two
            // operands as unsigned integers.
            cond: Cond::Lo,
        });
        insts
    }

    fn gen_get_stack_addr(mem: StackAMode, into_reg: Writable<Reg>, _ty: Type) -> Inst {
        let mem = mem.into();
        Inst::LoadAddr { rd: into_reg, mem }
    }

    fn get_stacklimit_reg() -> Reg {
        ip_reg()
    }

    fn gen_load_base_offset(into_reg: Writable<Reg>, base: Reg, offset: i32, ty: Type) -> Inst {
        let mem = AMode::RegOffset(base, offset as i64);
        Inst::gen_load(into_reg, mem, ty)
    }

    fn gen_store_base_offset(base: Reg, offset: i32, from_reg: Reg, ty: Type) -> Inst {
        let mem = AMode::RegOffset(base, offset as i64);
        Inst::gen_store(from_reg, mem, ty)
    }

    fn gen_sp_reg_adjust(amount: i32) -> SmallInstVec<Inst> {
        let mut ret = SmallVec::new();

        if amount == 0 {
            return ret;
        }
        let (amount, is_sub) = if amount > 0 {
            (amount, false)
        } else {
            (-amount, true)
        };

        let alu_op = if is_sub { ALUOp::Sub } else { ALUOp::Add };

        if let Some(imm12) = UImm12::maybe_from_i64(amount as i64) {
            ret.push(Inst::AluRRImm12 {
                alu_op,
                rd: writable_sp_reg(),
                rn: sp_reg(),
                imm12,
            });
        } else {
            let tmp = writable_ip_reg();
            ret.extend(Inst::load_constant(tmp, amount as u32));
            ret.push(Inst::AluRRRShift {
                alu_op,
                rd: writable_sp_reg(),
                rn: sp_reg(),
                rm: tmp.to_reg(),
                shift: None,
            });
        }
        ret
    }

    fn gen_nominal_sp_adj(offset: i32) -> Inst {
        let offset = i64::from(offset);
        Inst::VirtualSPOffsetAdj { offset }
    }

    fn gen_prologue_frame_setup(_: &settings::Flags) -> SmallInstVec<Inst> {
        let mut ret = SmallVec::new();
        let reg_list = vec![fp_reg(), lr_reg()];
        ret.push(Inst::Push { reg_list });
        ret.push(Inst::Mov {
            rd: writable_fp_reg(),
            rm: sp_reg(),
        });
        ret
    }

    fn gen_epilogue_frame_restore(_: &settings::Flags) -> SmallInstVec<Inst> {
        let mut ret = SmallVec::new();
        ret.push(Inst::Mov {
            rd: writable_sp_reg(),
            rm: fp_reg(),
        });
        let reg_list = vec![writable_fp_reg(), writable_lr_reg()];
        ret.push(Inst::Pop { reg_list });
        ret
    }

    fn gen_probestack(_: u32) -> SmallInstVec<Self::I> {
        // TODO: implement if we ever require stack probes on ARM32 (unlikely
        // unless Lucet is ported)
        smallvec![]
    }

    /// Returns stack bytes used as well as instructions. Does not adjust
    /// nominal SP offset; caller will do that.
    fn gen_clobber_save(
        _call_conv: isa::CallConv,
        _setup_frame: bool,
        _flags: &settings::Flags,
        clobbered_callee_saves: &Vec<Writable<RealReg>>,
        fixed_frame_storage_size: u32,
        _outgoing_args_size: u32,
    ) -> (u64, SmallVec<[Inst; 16]>) {
        let mut insts = SmallVec::new();
        if fixed_frame_storage_size > 0 {
            insts.extend(Self::gen_sp_reg_adjust(-(fixed_frame_storage_size as i32)).into_iter());
        }
        let mut clobbered_vec: Vec<_> = clobbered_callee_saves
            .into_iter()
            .map(|r| r.to_reg().to_reg())
            .collect();
        if clobbered_vec.len() % 2 == 1 {
            // For alignment purposes.
            clobbered_vec.push(ip_reg());
        }
        let stack_used = clobbered_vec.len() * 4;
        if !clobbered_vec.is_empty() {
            insts.push(Inst::Push {
                reg_list: clobbered_vec,
            });
        }

        (stack_used as u64, insts)
    }

    fn gen_clobber_restore(
        call_conv: isa::CallConv,
        _flags: &settings::Flags,
        clobbers: &Set<Writable<RealReg>>,
        _fixed_frame_storage_size: u32,
        _outgoing_args_size: u32,
    ) -> SmallVec<[Inst; 16]> {
        let mut insts = SmallVec::new();
        let clobbered_vec = Self::get_clobbered_callee_saves(call_conv, clobbers);
        let mut clobbered_vec: Vec<_> = clobbered_vec
            .into_iter()
            .map(|r| Writable::from_reg(r.to_reg().to_reg()))
            .collect();
        if clobbered_vec.len() % 2 == 1 {
            clobbered_vec.push(writable_ip_reg());
        }
        if !clobbered_vec.is_empty() {
            insts.push(Inst::Pop {
                reg_list: clobbered_vec,
            });
        }
        insts
    }

    fn gen_call(
        dest: &CallDest,
        uses: Vec<Reg>,
        defs: Vec<Writable<Reg>>,
        opcode: ir::Opcode,
        tmp: Writable<Reg>,
        _callee_conv: isa::CallConv,
        _caller_conv: isa::CallConv,
    ) -> SmallVec<[(InstIsSafepoint, Inst); 2]> {
        let mut insts = SmallVec::new();
        match &dest {
            &CallDest::ExtName(ref name, RelocDistance::Near) => insts.push((
                InstIsSafepoint::Yes,
                Inst::Call {
                    info: Box::new(CallInfo {
                        dest: name.clone(),
                        uses,
                        defs,
                        opcode,
                    }),
                },
            )),
            &CallDest::ExtName(ref name, RelocDistance::Far) => {
                insts.push((
                    InstIsSafepoint::No,
                    Inst::LoadExtName {
                        rt: tmp,
                        name: Box::new(name.clone()),
                        offset: 0,
                    },
                ));
                insts.push((
                    InstIsSafepoint::Yes,
                    Inst::CallInd {
                        info: Box::new(CallIndInfo {
                            rm: tmp.to_reg(),
                            uses,
                            defs,
                            opcode,
                        }),
                    },
                ));
            }
            &CallDest::Reg(reg) => insts.push((
                InstIsSafepoint::Yes,
                Inst::CallInd {
                    info: Box::new(CallIndInfo {
                        rm: *reg,
                        uses,
                        defs,
                        opcode,
                    }),
                },
            )),
        }

        insts
    }

    fn gen_memcpy(
        _call_conv: isa::CallConv,
        _dst: Reg,
        _src: Reg,
        _size: usize,
    ) -> SmallVec<[Self::I; 8]> {
        unimplemented!("StructArgs not implemented for ARM32 yet");
    }

    fn get_number_of_spillslots_for_value(rc: RegClass, _ty: Type) -> u32 {
        match rc {
            RegClass::I32 => 1,
            _ => panic!("Unexpected register class!"),
        }
    }

    fn get_virtual_sp_offset_from_state(s: &EmitState) -> i64 {
        s.virtual_sp_offset
    }

    fn get_nominal_sp_to_fp(s: &EmitState) -> i64 {
        s.nominal_sp_to_fp
    }

    fn get_regs_clobbered_by_call(_: isa::CallConv) -> Vec<Writable<Reg>> {
        let mut caller_saved = Vec::new();
        for i in 0..15 {
            let r = writable_rreg(i);
            if is_reg_clobbered_by_call(r.to_reg().to_real_reg()) {
                caller_saved.push(r);
            }
        }
        caller_saved
    }

    fn get_ext_mode(
        _call_conv: isa::CallConv,
        specified: ir::ArgumentExtension,
    ) -> ir::ArgumentExtension {
        specified
    }

    fn get_clobbered_callee_saves(
        _call_conv: isa::CallConv,
        regs: &Set<Writable<RealReg>>,
    ) -> Vec<Writable<RealReg>> {
        let mut ret = Vec::new();
        for &reg in regs.iter() {
            if is_callee_save(reg.to_reg()) {
                ret.push(reg);
            }
        }

        // Sort registers for deterministic code output.
        ret.sort_by_key(|r| r.to_reg().get_index());
        ret
    }

    fn is_frame_setup_needed(
        _is_leaf: bool,
        _stack_args_size: u32,
        _num_clobbered_callee_saves: usize,
        _fixed_frame_storage_size: u32,
    ) -> bool {
        true
    }
}

fn is_callee_save(r: RealReg) -> bool {
    let enc = r.get_hw_encoding();
    4 <= enc && enc <= 10
}

fn is_reg_clobbered_by_call(r: RealReg) -> bool {
    let enc = r.get_hw_encoding();
    enc <= 3
}
