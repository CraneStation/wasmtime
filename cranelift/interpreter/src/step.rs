//! The [step] function interprets a single Cranelift instruction given its [State] and
//! [InstructionContext]; the interpretation is generic over [Value]s.
use crate::instruction::InstructionContext;
use crate::state::{MemoryError, State};
use crate::value::{Value, ValueConversionKind, ValueError, ValueResult};
use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
use cranelift_codegen::ir::{
    types, Block, FuncRef, Function, InstructionData, Opcode, TrapCode, Value as ValueRef,
};
use log::trace;
use smallvec::{smallvec, SmallVec};
use std::ops::RangeFrom;
use thiserror::Error;

/// Interpret a single Cranelift instruction. Note that program traps and interpreter errors are
/// distinct: a program trap results in `Ok(Flow::Trap(...))` whereas an interpretation error (e.g.
/// the types of two values are incompatible) results in `Err(...)`.
#[allow(unused_variables)]
pub fn step<'a, V, I>(
    state: &mut dyn State<'a, V>,
    inst_context: I,
) -> Result<ControlFlow<'a, V>, StepError>
where
    V: Value,
    I: InstructionContext,
{
    let inst = inst_context.data();
    let ctrl_ty = inst_context.controlling_type().unwrap();
    trace!(
        "Step: {}{}",
        inst.opcode(),
        if ctrl_ty.is_invalid() {
            String::new()
        } else {
            format!(".{}", ctrl_ty)
        }
    );

    // The following closures make the `step` implementation much easier to express. Note that they
    // frequently close over the `state` or `inst_context` for brevity.

    // Retrieve the current value for an instruction argument.
    let arg = |index: usize| -> Result<V, StepError> {
        let value_ref = inst_context.args()[index];
        state
            .get_value(value_ref)
            .ok_or(StepError::UnknownValue(value_ref))
    };

    // Retrieve the current values for all of an instruction's arguments.
    let args = || -> Result<SmallVec<[V; 1]>, StepError> {
        state
            .collect_values(inst_context.args())
            .map_err(|v| StepError::UnknownValue(v))
    };

    // Retrieve the current values for a range of an instruction's arguments.
    let args_range = |indexes: RangeFrom<usize>| -> Result<SmallVec<[V; 1]>, StepError> {
        Ok(SmallVec::<[V; 1]>::from(&args()?[indexes]))
    };

    // Retrieve the immediate value for an instruction, expecting it to exist.
    let imm = || -> V { V::from(inst.imm_value().unwrap()) };

    // Retrieve the immediate value for an instruction and convert it to the controlling type of the
    // instruction. For example, since `InstructionData` stores all integer immediates in a 64-bit
    // size, this will attempt to convert `iconst.i8 ...` to an 8-bit size.
    let imm_as_ctrl_ty =
        || -> Result<V, ValueError> { V::convert(imm(), ValueConversionKind::Exact(ctrl_ty)) };

    // Indicate that the result of a step is to assign a single value to an instruction's results.
    let assign = |value: V| ControlFlow::Assign(smallvec![value]);

    // Interpret a binary instruction with the given `op`, assigning the resulting value to the
    // instruction's results.
    let binary = |op: fn(V, V) -> ValueResult<V>,
                  left: V,
                  right: V|
     -> ValueResult<ControlFlow<V>> { Ok(assign(op(left, right)?)) };

    // Same as `binary`, but converts the values to their unsigned form before the operation and
    // back to signed form afterwards. Since Cranelift types have no notion of signedness, this
    // enables operations that depend on sign.
    let binary_unsigned =
        |op: fn(V, V) -> ValueResult<V>, left: V, right: V| -> ValueResult<ControlFlow<V>> {
            Ok(assign(
                op(
                    left.convert(ValueConversionKind::ToUnsigned)?,
                    right.convert(ValueConversionKind::ToUnsigned)?,
                )?
                .convert(ValueConversionKind::ToSigned)?,
            ))
        };

    // Choose whether to assign `left` or `right` to the instruction's result based on a `condition`.
    let choose = |condition: bool, left: V, right: V| -> ControlFlow<V> {
        assign(if condition { left } else { right })
    };

    // Retrieve an instruction's branch destination; expects the instruction to be a branch.
    let branch = || -> Block { inst.branch_destination().unwrap() };

    // Based on `condition`, indicate where to continue the control flow.
    let branch_when = |condition: bool| -> Result<ControlFlow<V>, StepError> {
        let branch_args = match inst {
            InstructionData::Jump { .. } => args_range(0..),
            InstructionData::BranchInt { .. }
            | InstructionData::BranchFloat { .. }
            | InstructionData::Branch { .. } => args_range(1..),
            InstructionData::BranchIcmp { .. } => args_range(2..),
            _ => panic!("Unrecognized branch inst: {:?}", inst),
        }?;

        Ok(if condition {
            ControlFlow::ContinueAt(branch(), branch_args)
        } else {
            ControlFlow::Continue
        })
    };

    // Retrieve an instruction's trap code; expects the instruction to be a trap.
    let trap_code = || -> TrapCode { inst.trap_code().unwrap() };

    // Based on `condition`, either trap or not.
    let trap_when = |condition: bool, trap: CraneliftTrap| -> ControlFlow<V> {
        if condition {
            ControlFlow::Trap(trap)
        } else {
            ControlFlow::Continue
        }
    };

    // Helper for summing a sequence of values.
    fn sum<V: Value>(head: V, tail: SmallVec<[V; 1]>) -> ValueResult<i64> {
        let mut acc = head;
        for t in tail {
            acc = Value::add(acc, t)?;
        }
        acc.into_int()
    }

    // Interpret a Cranelift instruction.
    Ok(match inst.opcode() {
        Opcode::Jump | Opcode::Fallthrough => ControlFlow::ContinueAt(branch(), args()?),
        Opcode::Brz => branch_when(match arg(0)?.ty() {
            ty if ty.is_bool() => !arg(0)?.into_bool()?,
            ty if ty.is_int() => arg(0)?.into_int()? == 0,
            _ => return Err(StepError::ValueError(ValueError::InvalidValue(types::B1))),
        })?,
        Opcode::Brnz => branch_when(match arg(0)?.ty() {
            ty if ty.is_bool() => arg(0)?.into_bool()?,
            ty if ty.is_int() => arg(0)?.into_int()? != 0,
            _ => return Err(StepError::ValueError(ValueError::InvalidValue(types::B1))),
        })?,
        Opcode::BrIcmp => branch_when(icmp(inst.cond_code().unwrap(), &arg(0)?, &arg(1)?)?)?,
        Opcode::Brif => branch_when(state.has_iflag(inst.cond_code().unwrap()))?,
        Opcode::Brff => branch_when(state.has_fflag(inst.fp_cond_code().unwrap()))?,
        Opcode::BrTable => unimplemented!("BrTable"),
        Opcode::JumpTableEntry => unimplemented!("JumpTableEntry"),
        Opcode::JumpTableBase => unimplemented!("JumpTableBase"),
        Opcode::IndirectJumpTableBr => unimplemented!("IndirectJumpTableBr"),
        Opcode::Trap => ControlFlow::Trap(CraneliftTrap::User(trap_code())),
        Opcode::Debugtrap => ControlFlow::Trap(CraneliftTrap::Debug),
        Opcode::ResumableTrap => ControlFlow::Trap(CraneliftTrap::Resumable),
        Opcode::Trapz => trap_when(!arg(0)?.into_bool()?, CraneliftTrap::User(trap_code())),
        Opcode::Trapnz => trap_when(arg(0)?.into_bool()?, CraneliftTrap::User(trap_code())),
        Opcode::ResumableTrapnz => trap_when(arg(0)?.into_bool()?, CraneliftTrap::Resumable),
        Opcode::Trapif => trap_when(
            state.has_iflag(inst.cond_code().unwrap()),
            CraneliftTrap::User(trap_code()),
        ),
        Opcode::Trapff => trap_when(
            state.has_fflag(inst.fp_cond_code().unwrap()),
            CraneliftTrap::User(trap_code()),
        ),
        Opcode::Return => ControlFlow::Return(args()?),
        Opcode::FallthroughReturn => ControlFlow::Return(args()?),
        Opcode::Call => {
            if let InstructionData::Call { func_ref, .. } = inst {
                let function = state
                    .get_function(func_ref)
                    .ok_or(StepError::UnknownFunction(func_ref))?;
                ControlFlow::Call(function, args()?)
            } else {
                unreachable!()
            }
        }
        Opcode::CallIndirect => unimplemented!("CallIndirect"),
        Opcode::FuncAddr => unimplemented!("FuncAddr"),
        Opcode::Load
        | Opcode::LoadComplex
        | Opcode::Uload8
        | Opcode::Uload8Complex
        | Opcode::Sload8
        | Opcode::Sload8Complex
        | Opcode::Uload16
        | Opcode::Uload16Complex
        | Opcode::Sload16
        | Opcode::Sload16Complex
        | Opcode::Uload32
        | Opcode::Uload32Complex
        | Opcode::Sload32
        | Opcode::Sload32Complex
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
        | Opcode::Sload32x2Complex => {
            let address = sum(imm(), args()?)? as usize;
            let ctrl_ty = inst_context.controlling_type().unwrap();
            let (load_ty, kind) = match inst.opcode() {
                Opcode::Load | Opcode::LoadComplex => (ctrl_ty, None),
                Opcode::Uload8 | Opcode::Uload8Complex => {
                    (types::I8, Some(ValueConversionKind::ZeroExtend(ctrl_ty)))
                }
                Opcode::Sload8 | Opcode::Sload8Complex => {
                    (types::I8, Some(ValueConversionKind::SignExtend(ctrl_ty)))
                }
                Opcode::Uload16 | Opcode::Uload16Complex => {
                    (types::I16, Some(ValueConversionKind::ZeroExtend(ctrl_ty)))
                }
                Opcode::Sload16 | Opcode::Sload16Complex => {
                    (types::I16, Some(ValueConversionKind::SignExtend(ctrl_ty)))
                }
                Opcode::Uload32 | Opcode::Uload32Complex => {
                    (types::I32, Some(ValueConversionKind::ZeroExtend(ctrl_ty)))
                }
                Opcode::Sload32 | Opcode::Sload32Complex => {
                    (types::I32, Some(ValueConversionKind::SignExtend(ctrl_ty)))
                }
                Opcode::Uload8x8
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
                | Opcode::Sload32x2Complex => unimplemented!(),
                _ => unreachable!(),
            };
            let loaded = state.load_heap(address, load_ty)?;
            let extended = if let Some(c) = kind {
                loaded.convert(c)?
            } else {
                loaded
            };
            ControlFlow::Assign(smallvec!(extended))
        }
        Opcode::Store
        | Opcode::StoreComplex
        | Opcode::Istore8
        | Opcode::Istore8Complex
        | Opcode::Istore16
        | Opcode::Istore16Complex
        | Opcode::Istore32
        | Opcode::Istore32Complex => {
            let address = sum(imm(), args_range(1..)?)? as usize;
            let kind = match inst.opcode() {
                Opcode::Store | Opcode::StoreComplex => None,
                Opcode::Istore8 | Opcode::Istore8Complex => {
                    Some(ValueConversionKind::Truncate(types::I8))
                }
                Opcode::Istore16 | Opcode::Istore16Complex => {
                    Some(ValueConversionKind::Truncate(types::I16))
                }
                Opcode::Istore32 | Opcode::Istore32Complex => {
                    Some(ValueConversionKind::Truncate(types::I32))
                }
                _ => unreachable!(),
            };
            let reduced = if let Some(c) = kind {
                arg(0)?.convert(c)?
            } else {
                arg(0)?
            };
            state.store_heap(address, reduced)?;
            ControlFlow::Continue
        }
        Opcode::StackLoad => {
            let address = sum(imm(), args_range(1..)?)? as usize;
            let load_ty = inst_context.controlling_type().unwrap();
            let loaded = state.load_stack(address, load_ty)?;
            ControlFlow::Assign(smallvec!(loaded))
        }
        Opcode::StackStore => {
            let address = sum(imm(), args_range(1..)?)? as usize;
            let arg0 = arg(0)?;
            state.store_stack(address, arg0)?;
            ControlFlow::Continue
        }
        Opcode::StackAddr => unimplemented!("StackAddr"),
        Opcode::GlobalValue => unimplemented!("GlobalValue"),
        Opcode::SymbolValue => unimplemented!("SymbolValue"),
        Opcode::TlsValue => unimplemented!("TlsValue"),
        Opcode::HeapAddr => unimplemented!("HeapAddr"),
        Opcode::GetPinnedReg => unimplemented!("GetPinnedReg"),
        Opcode::SetPinnedReg => unimplemented!("SetPinnedReg"),
        Opcode::TableAddr => unimplemented!("TableAddr"),
        Opcode::Iconst => assign(Value::int(imm().into_int()?, ctrl_ty)?),
        Opcode::F32const => assign(imm()),
        Opcode::F64const => assign(imm()),
        Opcode::Bconst => assign(imm()),
        Opcode::Vconst => unimplemented!("Vconst"),
        Opcode::ConstAddr => unimplemented!("ConstAddr"),
        Opcode::Null => unimplemented!("Null"),
        Opcode::Nop => ControlFlow::Continue,
        Opcode::Select => choose(arg(0)?.into_bool()?, arg(1)?, arg(2)?),
        Opcode::Selectif => choose(state.has_iflag(inst.cond_code().unwrap()), arg(1)?, arg(2)?),
        Opcode::SelectifSpectreGuard => unimplemented!("SelectifSpectreGuard"),
        Opcode::Bitselect => {
            let mask_a = Value::and(arg(0)?, arg(1)?)?;
            let mask_b = Value::and(Value::not(arg(0)?)?, arg(2)?)?;
            assign(Value::or(mask_a, mask_b)?)
        }
        Opcode::Copy => assign(arg(0)?),
        Opcode::Spill => unimplemented!("Spill"),
        Opcode::Fill => unimplemented!("Fill"),
        Opcode::FillNop => assign(arg(0)?),
        Opcode::DummySargT => unimplemented!("DummySargT"),
        Opcode::Regmove => ControlFlow::Continue,
        Opcode::CopySpecial => ControlFlow::Continue,
        Opcode::CopyToSsa => assign(arg(0)?),
        Opcode::CopyNop => unimplemented!("CopyNop"),
        Opcode::AdjustSpDown => unimplemented!("AdjustSpDown"),
        Opcode::AdjustSpUpImm => unimplemented!("AdjustSpUpImm"),
        Opcode::AdjustSpDownImm => unimplemented!("AdjustSpDownImm"),
        Opcode::IfcmpSp => unimplemented!("IfcmpSp"),
        Opcode::Regspill => unimplemented!("Regspill"),
        Opcode::Regfill => unimplemented!("Regfill"),
        Opcode::Safepoint => unimplemented!("Safepoint"),
        Opcode::Icmp => assign(Value::bool(
            icmp(inst.cond_code().unwrap(), &arg(0)?, &arg(1)?)?,
            ctrl_ty.as_bool(),
        )?),
        Opcode::IcmpImm => assign(Value::bool(
            icmp(inst.cond_code().unwrap(), &arg(0)?, &imm_as_ctrl_ty()?)?,
            ctrl_ty.as_bool(),
        )?),
        Opcode::Ifcmp | Opcode::IfcmpImm => {
            let arg0 = arg(0)?;
            let arg1 = match inst.opcode() {
                Opcode::Ifcmp => arg(1)?,
                Opcode::IfcmpImm => imm_as_ctrl_ty()?,
                _ => unreachable!(),
            };
            state.clear_flags();
            for f in &[
                IntCC::Equal,
                IntCC::NotEqual,
                IntCC::SignedLessThan,
                IntCC::SignedGreaterThanOrEqual,
                IntCC::SignedGreaterThan,
                IntCC::SignedLessThanOrEqual,
                IntCC::UnsignedLessThan,
                IntCC::UnsignedGreaterThanOrEqual,
                IntCC::UnsignedGreaterThan,
                IntCC::UnsignedLessThanOrEqual,
            ] {
                if icmp(*f, &arg0, &arg1)? {
                    state.set_iflag(*f);
                }
            }
            ControlFlow::Continue
        }
        Opcode::Imin => choose(Value::gt(&arg(1)?, &arg(0)?)?, arg(0)?, arg(1)?),
        Opcode::Umin => choose(
            Value::gt(
                &arg(1)?.convert(ValueConversionKind::ToUnsigned)?,
                &arg(0)?.convert(ValueConversionKind::ToUnsigned)?,
            )?,
            arg(0)?,
            arg(1)?,
        ),
        Opcode::Imax => choose(Value::gt(&arg(0)?, &arg(1)?)?, arg(0)?, arg(1)?),
        Opcode::Umax => choose(
            Value::gt(
                &arg(0)?.convert(ValueConversionKind::ToUnsigned)?,
                &arg(1)?.convert(ValueConversionKind::ToUnsigned)?,
            )?,
            arg(0)?,
            arg(1)?,
        ),
        Opcode::AvgRound => {
            let sum = Value::add(arg(0)?, arg(1)?)?;
            let one = Value::int(1, arg(0)?.ty())?;
            let inc = Value::add(sum, one)?;
            let two = Value::int(2, arg(0)?.ty())?;
            binary(Value::div, inc, two)?
        }
        Opcode::Iadd => binary(Value::add, arg(0)?, arg(1)?)?,
        Opcode::UaddSat => unimplemented!("UaddSat"),
        Opcode::SaddSat => unimplemented!("SaddSat"),
        Opcode::Isub => binary(Value::sub, arg(0)?, arg(1)?)?,
        Opcode::UsubSat => unimplemented!("UsubSat"),
        Opcode::SsubSat => unimplemented!("SsubSat"),
        Opcode::Ineg => binary(Value::sub, Value::int(0, ctrl_ty)?, arg(0)?)?,
        Opcode::Iabs => unimplemented!("Iabs"),
        Opcode::Imul => binary(Value::mul, arg(0)?, arg(1)?)?,
        Opcode::Umulhi => unimplemented!("Umulhi"),
        Opcode::Smulhi => unimplemented!("Smulhi"),
        Opcode::Udiv => binary_unsigned(Value::div, arg(0)?, arg(1)?)?,
        Opcode::Sdiv => binary(Value::div, arg(0)?, arg(1)?)?,
        Opcode::Urem => binary_unsigned(Value::rem, arg(0)?, arg(1)?)?,
        Opcode::Srem => binary(Value::rem, arg(0)?, arg(1)?)?,
        Opcode::IaddImm => binary(Value::add, arg(0)?, imm_as_ctrl_ty()?)?,
        Opcode::ImulImm => binary(Value::mul, arg(0)?, imm_as_ctrl_ty()?)?,
        Opcode::UdivImm => binary_unsigned(Value::div, arg(0)?, imm())?,
        Opcode::SdivImm => binary(Value::div, arg(0)?, imm_as_ctrl_ty()?)?,
        Opcode::UremImm => binary_unsigned(Value::rem, arg(0)?, imm())?,
        Opcode::SremImm => binary(Value::rem, arg(0)?, imm_as_ctrl_ty()?)?,
        Opcode::IrsubImm => binary(Value::sub, imm_as_ctrl_ty()?, arg(0)?)?,
        Opcode::IaddCin => unimplemented!("IaddCin"),
        Opcode::IaddIfcin => unimplemented!("IaddIfcin"),
        Opcode::IaddCout => unimplemented!("IaddCout"),
        Opcode::IaddIfcout => unimplemented!("IaddIfcout"),
        Opcode::IaddCarry => unimplemented!("IaddCarry"),
        Opcode::IaddIfcarry => unimplemented!("IaddIfcarry"),
        Opcode::IsubBin => unimplemented!("IsubBin"),
        Opcode::IsubIfbin => unimplemented!("IsubIfbin"),
        Opcode::IsubBout => unimplemented!("IsubBout"),
        Opcode::IsubIfbout => unimplemented!("IsubIfbout"),
        Opcode::IsubBorrow => unimplemented!("IsubBorrow"),
        Opcode::IsubIfborrow => unimplemented!("IsubIfborrow"),
        Opcode::Band => binary(Value::and, arg(0)?, arg(1)?)?,
        Opcode::Bor => binary(Value::or, arg(0)?, arg(1)?)?,
        Opcode::Bxor => binary(Value::xor, arg(0)?, arg(1)?)?,
        Opcode::Bnot => assign(Value::not(arg(0)?)?),
        Opcode::BandNot => binary(Value::and, arg(0)?, Value::not(arg(1)?)?)?,
        Opcode::BorNot => binary(Value::or, arg(0)?, Value::not(arg(1)?)?)?,
        Opcode::BxorNot => binary(Value::xor, arg(0)?, Value::not(arg(1)?)?)?,
        Opcode::BandImm => binary(Value::and, arg(0)?, imm_as_ctrl_ty()?)?,
        Opcode::BorImm => binary(Value::or, arg(0)?, imm_as_ctrl_ty()?)?,
        Opcode::BxorImm => binary(Value::xor, arg(0)?, imm_as_ctrl_ty()?)?,
        Opcode::Rotl => binary(Value::rotl, arg(0)?, arg(1)?)?,
        Opcode::Rotr => binary(Value::rotr, arg(0)?, arg(1)?)?,
        Opcode::RotlImm => binary(Value::rotl, arg(0)?, imm_as_ctrl_ty()?)?,
        Opcode::RotrImm => binary(Value::rotr, arg(0)?, imm_as_ctrl_ty()?)?,
        Opcode::Ishl => binary(Value::shl, arg(0)?, arg(1)?)?,
        Opcode::Ushr => binary(Value::ushr, arg(0)?, arg(1)?)?,
        Opcode::Sshr => binary(Value::ishr, arg(0)?, arg(1)?)?,
        Opcode::IshlImm => binary(Value::shl, arg(0)?, imm_as_ctrl_ty()?)?,
        Opcode::UshrImm => binary(Value::ushr, arg(0)?, imm_as_ctrl_ty()?)?,
        Opcode::SshrImm => binary(Value::ishr, arg(0)?, imm_as_ctrl_ty()?)?,
        Opcode::Bitrev => unimplemented!("Bitrev"),
        Opcode::Clz => unimplemented!("Clz"),
        Opcode::Cls => unimplemented!("Cls"),
        Opcode::Ctz => unimplemented!("Ctz"),
        Opcode::Popcnt => unimplemented!("Popcnt"),
        Opcode::Fcmp => assign(Value::bool(
            fcmp(inst.fp_cond_code().unwrap(), &arg(0)?, &arg(1)?)?,
            ctrl_ty.as_bool(),
        )?),
        Opcode::Ffcmp => {
            let arg0 = arg(0)?;
            let arg1 = arg(1)?;
            state.clear_flags();
            for f in &[
                FloatCC::Ordered,
                FloatCC::Unordered,
                FloatCC::Equal,
                FloatCC::NotEqual,
                FloatCC::OrderedNotEqual,
                FloatCC::UnorderedOrEqual,
                FloatCC::LessThan,
                FloatCC::LessThanOrEqual,
                FloatCC::GreaterThan,
                FloatCC::GreaterThanOrEqual,
                FloatCC::UnorderedOrLessThan,
                FloatCC::UnorderedOrLessThanOrEqual,
                FloatCC::UnorderedOrGreaterThan,
                FloatCC::UnorderedOrGreaterThanOrEqual,
            ] {
                if fcmp(*f, &arg0, &arg1)? {
                    state.set_fflag(*f);
                }
            }
            ControlFlow::Continue
        }
        Opcode::Fadd => binary(Value::add, arg(0)?, arg(1)?)?,
        Opcode::Fsub => binary(Value::sub, arg(0)?, arg(1)?)?,
        Opcode::Fmul => binary(Value::mul, arg(0)?, arg(1)?)?,
        Opcode::Fdiv => binary(Value::div, arg(0)?, arg(1)?)?,
        Opcode::Sqrt => unimplemented!("Sqrt"),
        Opcode::Fma => unimplemented!("Fma"),
        Opcode::Fneg => binary(Value::sub, Value::float(0, ctrl_ty)?, arg(0)?)?,
        Opcode::Fabs => unimplemented!("Fabs"),
        Opcode::Fcopysign => unimplemented!("Fcopysign"),
        Opcode::Fmin => choose(
            Value::is_nan(&arg(0)?)? || Value::lt(&arg(0)?, &arg(1)?)?,
            arg(0)?,
            arg(1)?,
        ),
        Opcode::FminPseudo => unimplemented!("FminPseudo"),
        Opcode::Fmax => choose(
            Value::is_nan(&arg(0)?)? || Value::gt(&arg(0)?, &arg(1)?)?,
            arg(0)?,
            arg(1)?,
        ),
        Opcode::FmaxPseudo => unimplemented!("FmaxPseudo"),
        Opcode::Ceil => unimplemented!("Ceil"),
        Opcode::Floor => unimplemented!("Floor"),
        Opcode::Trunc => unimplemented!("Trunc"),
        Opcode::Nearest => unimplemented!("Nearest"),
        Opcode::IsNull => unimplemented!("IsNull"),
        Opcode::IsInvalid => unimplemented!("IsInvalid"),
        Opcode::Trueif => choose(
            state.has_iflag(inst.cond_code().unwrap()),
            Value::bool(true, ctrl_ty)?,
            Value::bool(false, ctrl_ty)?,
        ),
        Opcode::Trueff => choose(
            state.has_fflag(inst.fp_cond_code().unwrap()),
            Value::bool(true, ctrl_ty)?,
            Value::bool(false, ctrl_ty)?,
        ),
        Opcode::Bitcast
        | Opcode::RawBitcast
        | Opcode::ScalarToVector
        | Opcode::Breduce
        | Opcode::Bextend
        | Opcode::Bint
        | Opcode::Bmask
        | Opcode::Ireduce => assign(Value::convert(
            arg(0)?,
            ValueConversionKind::Exact(ctrl_ty),
        )?),
        Opcode::Snarrow => assign(Value::convert(
            arg(0)?,
            ValueConversionKind::Truncate(ctrl_ty),
        )?),
        Opcode::Sextend => assign(Value::convert(
            arg(0)?,
            ValueConversionKind::SignExtend(ctrl_ty),
        )?),
        Opcode::Unarrow => assign(Value::convert(
            arg(0)?,
            ValueConversionKind::Truncate(ctrl_ty),
        )?),
        Opcode::Uunarrow => unimplemented!("Uunarrow"),
        Opcode::Uextend => assign(Value::convert(
            arg(0)?,
            ValueConversionKind::ZeroExtend(ctrl_ty),
        )?),
        Opcode::Fpromote => assign(Value::convert(
            arg(0)?,
            ValueConversionKind::Exact(ctrl_ty),
        )?),
        Opcode::Fdemote => assign(Value::convert(
            arg(0)?,
            ValueConversionKind::RoundNearestEven(ctrl_ty),
        )?),
        Opcode::Shuffle => unimplemented!("Shuffle"),
        Opcode::Swizzle => unimplemented!("Swizzle"),
        Opcode::Splat => unimplemented!("Splat"),
        Opcode::Insertlane => unimplemented!("Insertlane"),
        Opcode::Extractlane => unimplemented!("Extractlane"),
        Opcode::VhighBits => unimplemented!("VhighBits"),
        Opcode::Vsplit => unimplemented!("Vsplit"),
        Opcode::Vconcat => unimplemented!("Vconcat"),
        Opcode::Vselect => unimplemented!("Vselect"),
        Opcode::VanyTrue => unimplemented!("VanyTrue"),
        Opcode::VallTrue => unimplemented!("VallTrue"),
        Opcode::SwidenLow => unimplemented!("SwidenLow"),
        Opcode::SwidenHigh => unimplemented!("SwidenHigh"),
        Opcode::UwidenLow => unimplemented!("UwidenLow"),
        Opcode::UwidenHigh => unimplemented!("UwidenHigh"),
        Opcode::FcvtToUint => unimplemented!("FcvtToUint"),
        Opcode::FcvtToUintSat => unimplemented!("FcvtToUintSat"),
        Opcode::FcvtToSint => unimplemented!("FcvtToSint"),
        Opcode::FcvtToSintSat => unimplemented!("FcvtToSintSat"),
        Opcode::FcvtFromUint => unimplemented!("FcvtFromUint"),
        Opcode::FcvtFromSint => unimplemented!("FcvtFromSint"),
        Opcode::FcvtLowFromSint => unimplemented!("FcvtLowFromSint"),
        Opcode::FvpromoteLow => unimplemented!("FvpromoteLow"),
        Opcode::Fvdemote => unimplemented!("Fvdemote"),
        Opcode::Isplit => unimplemented!("Isplit"),
        Opcode::Iconcat => unimplemented!("Iconcat"),
        Opcode::AtomicRmw => unimplemented!("AtomicRmw"),
        Opcode::AtomicCas => unimplemented!("AtomicCas"),
        Opcode::AtomicLoad => unimplemented!("AtomicLoad"),
        Opcode::AtomicStore => unimplemented!("AtomicStore"),
        Opcode::Fence => unimplemented!("Fence"),
        Opcode::WideningPairwiseDotProductS => unimplemented!("WideningPairwiseDotProductS"),
        Opcode::SqmulRoundSat => unimplemented!("SqmulRoundSat"),

        // TODO: these instructions should be removed once the new backend makes these obsolete
        // (see https://github.com/bytecodealliance/wasmtime/issues/1936); additionally, the
        // "all-arch" feature for cranelift-codegen would become unnecessary for this crate.
        Opcode::X86Udivmodx
        | Opcode::X86Sdivmodx
        | Opcode::X86Umulx
        | Opcode::X86Smulx
        | Opcode::X86Cvtt2si
        | Opcode::X86Vcvtudq2ps
        | Opcode::X86Fmin
        | Opcode::X86Fmax
        | Opcode::X86Push
        | Opcode::X86Pop
        | Opcode::X86Bsr
        | Opcode::X86Bsf
        | Opcode::X86Pshufd
        | Opcode::X86Pshufb
        | Opcode::X86Pblendw
        | Opcode::X86Pextr
        | Opcode::X86Pinsr
        | Opcode::X86Insertps
        | Opcode::X86Punpckh
        | Opcode::X86Punpckl
        | Opcode::X86Movsd
        | Opcode::X86Movlhps
        | Opcode::X86Psll
        | Opcode::X86Psrl
        | Opcode::X86Psra
        | Opcode::X86Pmullq
        | Opcode::X86Pmuludq
        | Opcode::X86Ptest
        | Opcode::X86Pmaxs
        | Opcode::X86Pmaxu
        | Opcode::X86Pmins
        | Opcode::X86Pminu
        | Opcode::X86Palignr
        | Opcode::X86ElfTlsGetAddr
        | Opcode::X86MachoTlsGetAddr => unimplemented!("x86 instruction: {}", inst.opcode()),
    })
}

#[derive(Error, Debug)]
pub enum StepError {
    #[error("unable to retrieve value from SSA reference: {0}")]
    UnknownValue(ValueRef),
    #[error("unable to find the following function: {0}")]
    UnknownFunction(FuncRef),
    #[error("cannot step with these values")]
    ValueError(#[from] ValueError),
    #[error("failed to access memory")]
    MemoryError(#[from] MemoryError),
}

/// Enumerate the ways in which the control flow can change based on a single step in a Cranelift
/// interpreter.
#[derive(Debug)]
pub enum ControlFlow<'a, V> {
    /// Return one or more values from an instruction to be assigned to a left-hand side, e.g.:
    /// in `v0 = iadd v1, v2`, the sum of `v1` and `v2` is assigned to `v0`.
    Assign(SmallVec<[V; 1]>),
    /// Continue to the next available instruction, e.g.: in `nop`, we expect to resume execution
    /// at the instruction after it.
    Continue,
    /// Jump to another block with the given parameters, e.g.: in `brz v0, block42, [v1, v2]`, if
    /// the condition is true, we continue execution at the first instruction of `block42` with the
    /// values in `v1` and `v2` filling in the block parameters.
    ContinueAt(Block, SmallVec<[V; 1]>),
    /// Indicates a call the given [Function] with the supplied arguments.
    Call(&'a Function, SmallVec<[V; 1]>),
    /// Return from the current function with the given parameters, e.g.: `return [v1, v2]`.
    Return(SmallVec<[V; 1]>),
    /// Stop with a program-generated trap; note that these are distinct from errors that may occur
    /// during interpretation.
    Trap(CraneliftTrap),
}

impl<'a, V> ControlFlow<'a, V> {
    /// For convenience, we can unwrap the [ControlFlow] state assuming that it is a
    /// [ControlFlow::Return], panicking otherwise.
    pub fn unwrap_return(self) -> Vec<V> {
        if let ControlFlow::Return(values) = self {
            values.into_vec()
        } else {
            panic!("expected the control flow to be in the return state")
        }
    }
}

#[derive(Error, Debug, PartialEq)]
pub enum CraneliftTrap {
    #[error("user code: {0}")]
    User(TrapCode),
    #[error("user debug")]
    Debug,
    #[error("resumable")]
    Resumable,
}

/// Compare two values using the given integer condition `code`.
fn icmp<V>(code: IntCC, left: &V, right: &V) -> ValueResult<bool>
where
    V: Value,
{
    Ok(match code {
        IntCC::Equal => Value::eq(left, right)?,
        IntCC::NotEqual => !Value::eq(left, right)?,
        IntCC::SignedGreaterThan => Value::gt(left, right)?,
        IntCC::SignedGreaterThanOrEqual => Value::ge(left, right)?,
        IntCC::SignedLessThan => Value::lt(left, right)?,
        IntCC::SignedLessThanOrEqual => Value::le(left, right)?,
        IntCC::UnsignedGreaterThan => Value::gt(
            &left.clone().convert(ValueConversionKind::ToUnsigned)?,
            &right.clone().convert(ValueConversionKind::ToUnsigned)?,
        )?,
        IntCC::UnsignedGreaterThanOrEqual => Value::ge(
            &left.clone().convert(ValueConversionKind::ToUnsigned)?,
            &right.clone().convert(ValueConversionKind::ToUnsigned)?,
        )?,
        IntCC::UnsignedLessThan => Value::lt(
            &left.clone().convert(ValueConversionKind::ToUnsigned)?,
            &right.clone().convert(ValueConversionKind::ToUnsigned)?,
        )?,
        IntCC::UnsignedLessThanOrEqual => Value::le(
            &left.clone().convert(ValueConversionKind::ToUnsigned)?,
            &right.clone().convert(ValueConversionKind::ToUnsigned)?,
        )?,
        IntCC::Overflow => Value::overflow(left, right)?,
        IntCC::NotOverflow => !Value::overflow(left, right)?,
    })
}

/// Compare two values using the given floating point condition `code`.
fn fcmp<V>(code: FloatCC, left: &V, right: &V) -> ValueResult<bool>
where
    V: Value,
{
    Ok(match code {
        FloatCC::Ordered => {
            Value::eq(left, right)? || Value::lt(left, right)? || Value::gt(left, right)?
        }
        FloatCC::Unordered => Value::uno(left, right)?,
        FloatCC::Equal => Value::eq(left, right)?,
        FloatCC::NotEqual => {
            Value::lt(left, right)? || Value::gt(left, right)? || Value::uno(left, right)?
        }
        FloatCC::OrderedNotEqual => Value::lt(left, right)? || Value::gt(left, right)?,
        FloatCC::UnorderedOrEqual => Value::eq(left, right)? || Value::uno(left, right)?,
        FloatCC::LessThan => Value::lt(left, right)?,
        FloatCC::LessThanOrEqual => Value::lt(left, right)? || Value::eq(left, right)?,
        FloatCC::GreaterThan => Value::gt(left, right)?,
        FloatCC::GreaterThanOrEqual => Value::gt(left, right)? || Value::eq(left, right)?,
        FloatCC::UnorderedOrLessThan => Value::uno(left, right)? || Value::lt(left, right)?,
        FloatCC::UnorderedOrLessThanOrEqual => {
            Value::uno(left, right)? || Value::lt(left, right)? || Value::eq(left, right)?
        }
        FloatCC::UnorderedOrGreaterThan => Value::uno(left, right)? || Value::gt(left, right)?,
        FloatCC::UnorderedOrGreaterThanOrEqual => {
            Value::uno(left, right)? || Value::gt(left, right)? || Value::eq(left, right)?
        }
    })
}
