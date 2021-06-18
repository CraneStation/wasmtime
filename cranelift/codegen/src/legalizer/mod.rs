//! Legalize instructions.
//!
//! A legal instruction is one that can be mapped directly to a machine code instruction for the
//! target ISA. The `legalize_function()` function takes as input any function and transforms it
//! into an equivalent function using only legal instructions.
//!
//! The characteristics of legal instructions depend on the target ISA, so any given instruction
//! can be legal for one ISA and illegal for another.
//!
//! Besides transforming instructions, the legalizer also fills out the `function.encodings` map
//! which provides a legal encoding recipe for every instruction.
//!
//! The legalizer does not deal with register allocation constraints. These constraints are derived
//! from the encoding recipes, and solved later by the register allocator.

use crate::bitset::BitSet;
use crate::cursor::{Cursor, FuncCursor};
use crate::flowgraph::ControlFlowGraph;
use crate::ir::types::{I32, I64};
use crate::ir::{self, InstBuilder, MemFlags};
use crate::isa::TargetIsa;

#[cfg(any(
    feature = "arm32",
    feature = "arm64",
    feature = "riscv"
))]
use crate::predicates;
#[cfg(any(
    feature = "arm32",
    feature = "arm64",
    feature = "riscv"
))]
use alloc::vec::Vec;

use crate::timing;
use alloc::collections::BTreeSet;

mod boundary;
mod call;
mod globalvalue;
mod heap;
mod libcall;
mod split;
mod table;

use self::call::expand_call;
use self::globalvalue::expand_global_value;
use self::heap::expand_heap_addr;
pub(crate) use self::libcall::expand_as_libcall;
use self::table::expand_table_addr;

enum LegalizeInstResult {
    Done,
    Legalized,
    SplitLegalizePending,
}

/// Legalize `inst` for `isa`.
fn legalize_inst(
    inst: ir::Inst,
    pos: &mut FuncCursor,
    cfg: &mut ControlFlowGraph,
    isa: &dyn TargetIsa,
) -> LegalizeInstResult {
    let opcode = pos.func.dfg[inst].opcode();

    // Check for ABI boundaries that need to be converted to the legalized signature.
    if opcode.is_call() {
        if boundary::handle_call_abi(isa, inst, pos.func, cfg) {
            return LegalizeInstResult::Legalized;
        }
    } else if opcode.is_return() {
        if boundary::handle_return_abi(inst, pos.func, cfg) {
            return LegalizeInstResult::Legalized;
        }
    } else if opcode.is_branch() {
        split::simplify_branch_arguments(&mut pos.func.dfg, inst);
    } else if opcode == ir::Opcode::Isplit {
        pos.use_srcloc(inst);

        let arg = match pos.func.dfg[inst] {
            ir::InstructionData::Unary { arg, .. } => pos.func.dfg.resolve_aliases(arg),
            _ => panic!("Expected isplit: {}", pos.func.dfg.display_inst(inst, None)),
        };

        match pos.func.dfg.value_def(arg) {
            ir::ValueDef::Result(inst, _num) => {
                if let ir::InstructionData::Binary {
                    opcode: ir::Opcode::Iconcat,
                    ..
                } = pos.func.dfg[inst]
                {
                    // `arg` was created by an `iconcat` instruction.
                } else {
                    // `arg` was not created by an `iconcat` instruction. Don't try to resolve it,
                    // as otherwise `split::isplit` will re-insert the original `isplit`, causing
                    // an endless loop.
                    return LegalizeInstResult::SplitLegalizePending;
                }
            }
            ir::ValueDef::Param(_block, _num) => {}
        }

        let res = pos.func.dfg.inst_results(inst).to_vec();
        assert_eq!(res.len(), 2);
        let (resl, resh) = (res[0], res[1]); // Prevent borrowck error

        // Remove old isplit
        pos.func.dfg.clear_results(inst);
        pos.remove_inst();

        let curpos = pos.position();
        let srcloc = pos.srcloc();
        let (xl, xh) = split::isplit(pos.func, cfg, curpos, srcloc, arg);

        pos.func.dfg.change_to_alias(resl, xl);
        pos.func.dfg.change_to_alias(resh, xh);

        return LegalizeInstResult::Legalized;
    }

    match pos.func.update_encoding(inst, isa) {
        Ok(()) => LegalizeInstResult::Done,
        Err(action) => {
            // We should transform the instruction into legal equivalents.
            // If the current instruction was replaced, we need to double back and revisit
            // the expanded sequence. This is both to assign encodings and possible to
            // expand further.
            // There's a risk of infinite looping here if the legalization patterns are
            // unsound. Should we attempt to detect that?
            if action(inst, pos.func, cfg, isa) {
                return LegalizeInstResult::Legalized;
            }

            // We don't have any pattern expansion for this instruction either.
            // Try converting it to a library call as a last resort.
            if expand_as_libcall(inst, pos.func, isa) {
                LegalizeInstResult::Legalized
            } else {
                LegalizeInstResult::Done
            }
        }
    }
}

/// Legalize `func` for `isa`.
///
/// - Transform any instructions that don't have a legal representation in `isa`.
/// - Fill out `func.encodings`.
///
pub fn legalize_function(func: &mut ir::Function, cfg: &mut ControlFlowGraph, isa: &dyn TargetIsa) {
    let _tt = timing::legalize();
    debug_assert!(cfg.is_valid());

    boundary::legalize_signatures(func, isa);

    func.encodings.resize(func.dfg.num_insts());

    let mut pos = FuncCursor::new(func);
    let func_begin = pos.position();

    // Split block params before trying to legalize instructions, so that the newly introduced
    // isplit instructions get legalized.
    while let Some(block) = pos.next_block() {
        split::split_block_params(pos.func, cfg, block);
    }

    pos.set_position(func_begin);

    // This must be a set to prevent trying to legalize `isplit` and `vsplit` twice in certain cases.
    let mut pending_splits = BTreeSet::new();

    // Process blocks in layout order. Some legalization actions may split the current block or append
    // new ones to the end. We need to make sure we visit those new blocks too.
    while let Some(_block) = pos.next_block() {
        // Keep track of the cursor position before the instruction being processed, so we can
        // double back when replacing instructions.
        let mut prev_pos = pos.position();

        while let Some(inst) = pos.next_inst() {
            match legalize_inst(inst, &mut pos, cfg, isa) {
                // Remember this position in case we need to double back.
                LegalizeInstResult::Done => prev_pos = pos.position(),

                // Go back and legalize the inserted return value conversion instructions.
                LegalizeInstResult::Legalized => pos.set_position(prev_pos),

                // The argument of a `isplit` or `vsplit` instruction didn't resolve to a
                // `iconcat` or `vconcat` instruction. Try again after legalizing the rest of
                // the instructions.
                LegalizeInstResult::SplitLegalizePending => {
                    pending_splits.insert(inst);
                }
            }
        }
    }

    // Try legalizing `isplit` and `vsplit` instructions, which could not previously be legalized.
    for inst in pending_splits {
        pos.goto_inst(inst);
        legalize_inst(inst, &mut pos, cfg, isa);
    }

    // Now that we've lowered all br_tables, we don't need the jump tables anymore.
    if !isa.flags().enable_jump_tables() {
        pos.func.jump_tables.clear();
    }
}

/// Perform a simple legalization by expansion of the function, without
/// platform-specific transforms.
pub fn simple_legalize(func: &mut ir::Function, cfg: &mut ControlFlowGraph, isa: &dyn TargetIsa) {
    macro_rules! expand_imm_op {
        ($pos:ident, $inst:ident: $from:ident => $to:ident) => {{
            let (arg, imm) = match $pos.func.dfg[$inst] {
                ir::InstructionData::BinaryImm64 {
                    opcode: _,
                    arg,
                    imm,
                } => (arg, imm),
                _ => panic!(
                    concat!("Expected ", stringify!($from), ": {}"),
                    $pos.func.dfg.display_inst($inst, None)
                ),
            };
            let ty = $pos.func.dfg.value_type(arg);
            let imm = $pos.ins().iconst(ty, imm);
            $pos.func.dfg.replace($inst).$to(arg, imm);
        }};

        ($pos:ident, $inst:ident<$ty:ident>: $from:ident => $to:ident) => {{
            let (arg, imm) = match $pos.func.dfg[$inst] {
                ir::InstructionData::BinaryImm64 {
                    opcode: _,
                    arg,
                    imm,
                } => (arg, imm),
                _ => panic!(
                    concat!("Expected ", stringify!($from), ": {}"),
                    $pos.func.dfg.display_inst($inst, None)
                ),
            };
            let imm = $pos.ins().iconst($ty, imm);
            $pos.func.dfg.replace($inst).$to(arg, imm);
        }};
    }

    let mut pos = FuncCursor::new(func);
    let func_begin = pos.position();
    pos.set_position(func_begin);
    while let Some(_block) = pos.next_block() {
        let mut prev_pos = pos.position();
        while let Some(inst) = pos.next_inst() {
            match pos.func.dfg[inst].opcode() {
                // control flow
                ir::Opcode::BrIcmp => expand_br_icmp(inst, &mut pos.func, cfg, isa),
                ir::Opcode::Trapnz | ir::Opcode::Trapz | ir::Opcode::ResumableTrapnz => {
                    expand_cond_trap(inst, &mut pos.func, cfg, isa);
                }

                // memory and constants
                ir::Opcode::GlobalValue => expand_global_value(inst, &mut pos.func, cfg, isa),
                ir::Opcode::HeapAddr => expand_heap_addr(inst, &mut pos.func, cfg, isa),
                ir::Opcode::StackLoad => expand_stack_load(inst, &mut pos.func, cfg, isa),
                ir::Opcode::StackStore => expand_stack_store(inst, &mut pos.func, cfg, isa),
                ir::Opcode::TableAddr => expand_table_addr(inst, &mut pos.func, cfg, isa),

                // bitops
                ir::Opcode::BandImm => expand_imm_op!(pos, inst: band_imm => band),
                ir::Opcode::BorImm => expand_imm_op!(pos, inst: bor_imm => bor),
                ir::Opcode::BxorImm => expand_imm_op!(pos, inst: bxor_imm => bxor),
                ir::Opcode::IaddImm => expand_imm_op!(pos, inst: iadd_imm => iadd),

                // bitshifting
                ir::Opcode::IshlImm => expand_imm_op!(pos, inst<I32>: ishl_imm => ishl),
                ir::Opcode::RotlImm => expand_imm_op!(pos, inst<I32>: rotl_imm => rotl),
                ir::Opcode::RotrImm => expand_imm_op!(pos, inst<I32>: rotr_imm => rotr),
                ir::Opcode::SshrImm => expand_imm_op!(pos, inst<I32>: sshr_imm => sshr),
                ir::Opcode::UshrImm => expand_imm_op!(pos, inst<I32>: ushr_imm => ushr),

                // math
                ir::Opcode::IrsubImm => {
                    let (arg, imm) = match pos.func.dfg[inst] {
                        ir::InstructionData::BinaryImm64 {
                            opcode: _,
                            arg,
                            imm,
                        } => (arg, imm),
                        _ => panic!(
                            "Expected irsub_imm: {}",
                            pos.func.dfg.display_inst(inst, None)
                        ),
                    };
                    let ty = pos.func.dfg.value_type(arg);
                    let imm = pos.ins().iconst(ty, imm);
                    pos.func.dfg.replace(inst).isub(imm, arg); // note: arg order reversed
                }
                ir::Opcode::ImulImm => expand_imm_op!(pos, inst: imul_imm => imul),
                ir::Opcode::SdivImm => expand_imm_op!(pos, inst: sdiv_imm => sdiv),
                ir::Opcode::SremImm => expand_imm_op!(pos, inst: srem_imm => srem),
                ir::Opcode::UdivImm => expand_imm_op!(pos, inst: udiv_imm => udiv),
                ir::Opcode::UremImm => expand_imm_op!(pos, inst: urem_imm => urem),

                // comparisons
                ir::Opcode::IfcmpImm => expand_imm_op!(pos, inst: ifcmp_imm => ifcmp),
                ir::Opcode::IcmpImm => {
                    let (cc, x, y) = match pos.func.dfg[inst] {
                        ir::InstructionData::IntCompareImm {
                            opcode: _,
                            cond,
                            arg,
                            imm,
                        } => (cond, arg, imm),
                        _ => panic!(
                            "Expected ircmp_imm: {}",
                            pos.func.dfg.display_inst(inst, None)
                        ),
                    };
                    let ty = pos.func.dfg.value_type(x);
                    let y = pos.ins().iconst(ty, y);
                    pos.func.dfg.replace(inst).icmp(cc, x, y);
                }

                _ => {
                    prev_pos = pos.position();
                    continue;
                }
            };

            // Legalization implementations require fixpoint loop here.
            // TODO: fix this.
            pos.set_position(prev_pos);
        }
    }
}

// Include legalization patterns that were generated by `gen_legalizer.rs` from the
// `TransformGroup` in `cranelift-codegen/meta/shared/legalize.rs`.
//
// Concretely, this defines private functions `narrow()`, and `expand()`.
include!(concat!(env!("OUT_DIR"), "/legalizer.rs"));

/// Custom expansion for conditional trap instructions.
/// TODO: Add CFG support to the Rust DSL patterns so we won't have to do this.
fn expand_cond_trap(
    inst: ir::Inst,
    func: &mut ir::Function,
    cfg: &mut ControlFlowGraph,
    _isa: &dyn TargetIsa,
) {
    // Parse the instruction.
    let trapz;
    let (arg, code, opcode) = match func.dfg[inst] {
        ir::InstructionData::CondTrap { opcode, arg, code } => {
            // We want to branch *over* an unconditional trap.
            trapz = match opcode {
                ir::Opcode::Trapz => true,
                ir::Opcode::Trapnz | ir::Opcode::ResumableTrapnz => false,
                _ => panic!("Expected cond trap: {}", func.dfg.display_inst(inst, None)),
            };
            (arg, code, opcode)
        }
        _ => panic!("Expected cond trap: {}", func.dfg.display_inst(inst, None)),
    };

    // Split the block after `inst`:
    //
    //     trapnz arg
    //     ..
    //
    // Becomes:
    //
    //     brz arg, new_block_resume
    //     jump new_block_trap
    //
    //   new_block_trap:
    //     trap
    //
    //   new_block_resume:
    //     ..
    let old_block = func.layout.pp_block(inst);
    let new_block_trap = func.dfg.make_block();
    let new_block_resume = func.dfg.make_block();

    // Replace trap instruction by the inverted condition.
    if trapz {
        func.dfg.replace(inst).brnz(arg, new_block_resume, &[]);
    } else {
        func.dfg.replace(inst).brz(arg, new_block_resume, &[]);
    }

    // Add jump instruction after the inverted branch.
    let mut pos = FuncCursor::new(func).after_inst(inst);
    pos.use_srcloc(inst);
    pos.ins().jump(new_block_trap, &[]);

    // Insert the new label and the unconditional trap terminator.
    pos.insert_block(new_block_trap);

    match opcode {
        ir::Opcode::Trapz | ir::Opcode::Trapnz => {
            pos.ins().trap(code);
        }
        ir::Opcode::ResumableTrapnz => {
            pos.ins().resumable_trap(code);
            pos.ins().jump(new_block_resume, &[]);
        }
        _ => unreachable!(),
    }

    // Insert the new label and resume the execution when the trap fails.
    pos.insert_block(new_block_resume);

    // Finally update the CFG.
    cfg.recompute_block(pos.func, old_block);
    cfg.recompute_block(pos.func, new_block_resume);
    cfg.recompute_block(pos.func, new_block_trap);
}

/// Jump tables.
fn expand_br_table(
    inst: ir::Inst,
    func: &mut ir::Function,
    cfg: &mut ControlFlowGraph,
    isa: &dyn TargetIsa,
) {
    if isa.flags().enable_jump_tables() {
        expand_br_table_jt(inst, func, cfg, isa);
    } else {
        expand_br_table_conds(inst, func, cfg, isa);
    }
}

/// Expand br_table to jump table.
fn expand_br_table_jt(
    inst: ir::Inst,
    func: &mut ir::Function,
    cfg: &mut ControlFlowGraph,
    isa: &dyn TargetIsa,
) {
    use crate::ir::condcodes::IntCC;

    let (arg, default_block, table) = match func.dfg[inst] {
        ir::InstructionData::BranchTable {
            opcode: ir::Opcode::BrTable,
            arg,
            destination,
            table,
        } => (arg, destination, table),
        _ => panic!("Expected br_table: {}", func.dfg.display_inst(inst, None)),
    };

    // Rewrite:
    //
    //     br_table $idx, default_block, $jt
    //
    // To:
    //
    //     $oob = ifcmp_imm $idx, len($jt)
    //     brif uge $oob, default_block
    //     jump fallthrough_block
    //
    //   fallthrough_block:
    //     $base = jump_table_base.i64 $jt
    //     $rel_addr = jump_table_entry.i64 $idx, $base, 4, $jt
    //     $addr = iadd $base, $rel_addr
    //     indirect_jump_table_br $addr, $jt

    let block = func.layout.pp_block(inst);
    let jump_table_block = func.dfg.make_block();

    let mut pos = FuncCursor::new(func).at_inst(inst);
    pos.use_srcloc(inst);

    // Bounds check.
    let table_size = pos.func.jump_tables[table].len() as i64;
    let oob = pos
        .ins()
        .icmp_imm(IntCC::UnsignedGreaterThanOrEqual, arg, table_size);

    pos.ins().brnz(oob, default_block, &[]);
    pos.ins().jump(jump_table_block, &[]);
    pos.insert_block(jump_table_block);

    let addr_ty = isa.pointer_type();

    let arg = if pos.func.dfg.value_type(arg) == addr_ty {
        arg
    } else {
        pos.ins().uextend(addr_ty, arg)
    };

    let base_addr = pos.ins().jump_table_base(addr_ty, table);
    let entry = pos
        .ins()
        .jump_table_entry(arg, base_addr, I32.bytes() as u8, table);

    let addr = pos.ins().iadd(base_addr, entry);
    pos.ins().indirect_jump_table_br(addr, table);

    pos.remove_inst();
    cfg.recompute_block(pos.func, block);
    cfg.recompute_block(pos.func, jump_table_block);
}

/// Expand br_table to series of conditionals.
fn expand_br_table_conds(
    inst: ir::Inst,
    func: &mut ir::Function,
    cfg: &mut ControlFlowGraph,
    _isa: &dyn TargetIsa,
) {
    use crate::ir::condcodes::IntCC;

    let (arg, default_block, table) = match func.dfg[inst] {
        ir::InstructionData::BranchTable {
            opcode: ir::Opcode::BrTable,
            arg,
            destination,
            table,
        } => (arg, destination, table),
        _ => panic!("Expected br_table: {}", func.dfg.display_inst(inst, None)),
    };

    let block = func.layout.pp_block(inst);

    // This is a poor man's jump table using just a sequence of conditional branches.
    let table_size = func.jump_tables[table].len();
    let mut cond_failed_block = vec![];
    if table_size >= 1 {
        cond_failed_block = alloc::vec::Vec::with_capacity(table_size - 1);
        for _ in 0..table_size - 1 {
            cond_failed_block.push(func.dfg.make_block());
        }
    }

    let mut pos = FuncCursor::new(func).at_inst(inst);
    pos.use_srcloc(inst);

    // Ignore the lint for this loop as the range needs to be 0 to table_size
    #[allow(clippy::needless_range_loop)]
    for i in 0..table_size {
        let dest = pos.func.jump_tables[table].as_slice()[i];
        let t = pos.ins().icmp_imm(IntCC::Equal, arg, i as i64);
        pos.ins().brnz(t, dest, &[]);
        // Jump to the next case.
        if i < table_size - 1 {
            let block = cond_failed_block[i];
            pos.ins().jump(block, &[]);
            pos.insert_block(block);
        }
    }

    // `br_table` jumps to the default destination if nothing matches
    pos.ins().jump(default_block, &[]);

    pos.remove_inst();
    cfg.recompute_block(pos.func, block);
    for failed_block in cond_failed_block.into_iter() {
        cfg.recompute_block(pos.func, failed_block);
    }
}

/// Expand the select instruction.
///
/// Conditional moves are available in some ISAs for some register classes. The remaining selects
/// are handled by a branch.
fn expand_select(
    inst: ir::Inst,
    func: &mut ir::Function,
    cfg: &mut ControlFlowGraph,
    _isa: &dyn TargetIsa,
) {
    let (ctrl, tval, fval) = match func.dfg[inst] {
        ir::InstructionData::Ternary {
            opcode: ir::Opcode::Select,
            args,
        } => (args[0], args[1], args[2]),
        _ => panic!("Expected select: {}", func.dfg.display_inst(inst, None)),
    };

    // Replace `result = select ctrl, tval, fval` with:
    //
    //   brnz ctrl, new_block(tval)
    //   jump new_block(fval)
    // new_block(result):
    let old_block = func.layout.pp_block(inst);
    let result = func.dfg.first_result(inst);
    func.dfg.clear_results(inst);
    let new_block = func.dfg.make_block();
    func.dfg.attach_block_param(new_block, result);

    func.dfg.replace(inst).brnz(ctrl, new_block, &[tval]);
    let mut pos = FuncCursor::new(func).after_inst(inst);
    pos.use_srcloc(inst);
    pos.ins().jump(new_block, &[fval]);
    pos.insert_block(new_block);

    cfg.recompute_block(pos.func, new_block);
    cfg.recompute_block(pos.func, old_block);
}

fn expand_br_icmp(
    inst: ir::Inst,
    func: &mut ir::Function,
    cfg: &mut ControlFlowGraph,
    _isa: &dyn TargetIsa,
) {
    let (cond, a, b, destination, block_args) = match func.dfg[inst] {
        ir::InstructionData::BranchIcmp {
            cond,
            destination,
            ref args,
            ..
        } => (
            cond,
            args.get(0, &func.dfg.value_lists).unwrap(),
            args.get(1, &func.dfg.value_lists).unwrap(),
            destination,
            args.as_slice(&func.dfg.value_lists)[2..].to_vec(),
        ),
        _ => panic!("Expected br_icmp {}", func.dfg.display_inst(inst, None)),
    };

    let old_block = func.layout.pp_block(inst);
    func.dfg.clear_results(inst);

    let icmp_res = func.dfg.replace(inst).icmp(cond, a, b);
    let mut pos = FuncCursor::new(func).after_inst(inst);
    pos.use_srcloc(inst);
    pos.ins().brnz(icmp_res, destination, &block_args);

    cfg.recompute_block(pos.func, destination);
    cfg.recompute_block(pos.func, old_block);
}

/// Expand illegal `f32const` and `f64const` instructions.
fn expand_fconst(
    inst: ir::Inst,
    func: &mut ir::Function,
    _cfg: &mut ControlFlowGraph,
    _isa: &dyn TargetIsa,
) {
    let ty = func.dfg.value_type(func.dfg.first_result(inst));
    debug_assert!(!ty.is_vector(), "Only scalar fconst supported: {}", ty);

    // In the future, we may want to generate constant pool entries for these constants, but for
    // now use an `iconst` and a bit cast.
    let mut pos = FuncCursor::new(func).at_inst(inst);
    pos.use_srcloc(inst);
    let ival = match pos.func.dfg[inst] {
        ir::InstructionData::UnaryIeee32 {
            opcode: ir::Opcode::F32const,
            imm,
        } => pos.ins().iconst(ir::types::I32, i64::from(imm.bits())),
        ir::InstructionData::UnaryIeee64 {
            opcode: ir::Opcode::F64const,
            imm,
        } => pos.ins().iconst(ir::types::I64, imm.bits() as i64),
        _ => panic!("Expected fconst: {}", pos.func.dfg.display_inst(inst, None)),
    };
    pos.func.dfg.replace(inst).bitcast(ty, ival);
}

/// Expand illegal `stack_load` instructions.
fn expand_stack_load(
    inst: ir::Inst,
    func: &mut ir::Function,
    _cfg: &mut ControlFlowGraph,
    isa: &dyn TargetIsa,
) {
    let ty = func.dfg.value_type(func.dfg.first_result(inst));
    let addr_ty = isa.pointer_type();

    let mut pos = FuncCursor::new(func).at_inst(inst);
    pos.use_srcloc(inst);

    let (stack_slot, offset) = match pos.func.dfg[inst] {
        ir::InstructionData::StackLoad {
            opcode: _opcode,
            stack_slot,
            offset,
        } => (stack_slot, offset),
        _ => panic!(
            "Expected stack_load: {}",
            pos.func.dfg.display_inst(inst, None)
        ),
    };

    let addr = pos.ins().stack_addr(addr_ty, stack_slot, offset);

    // Stack slots are required to be accessible and aligned.
    let mflags = MemFlags::trusted();
    pos.func.dfg.replace(inst).load(ty, mflags, addr, 0);
}

/// Expand illegal `stack_store` instructions.
fn expand_stack_store(
    inst: ir::Inst,
    func: &mut ir::Function,
    _cfg: &mut ControlFlowGraph,
    isa: &dyn TargetIsa,
) {
    let addr_ty = isa.pointer_type();

    let mut pos = FuncCursor::new(func).at_inst(inst);
    pos.use_srcloc(inst);

    let (val, stack_slot, offset) = match pos.func.dfg[inst] {
        ir::InstructionData::StackStore {
            opcode: _opcode,
            arg,
            stack_slot,
            offset,
        } => (arg, stack_slot, offset),
        _ => panic!(
            "Expected stack_store: {}",
            pos.func.dfg.display_inst(inst, None)
        ),
    };

    let addr = pos.ins().stack_addr(addr_ty, stack_slot, offset);

    let mut mflags = MemFlags::new();
    // Stack slots are required to be accessible and aligned.
    mflags.set_notrap();
    mflags.set_aligned();
    pos.func.dfg.replace(inst).store(mflags, val, addr, 0);
}

/// Split a load into two parts before `iconcat`ing the result together.
fn narrow_load(
    inst: ir::Inst,
    func: &mut ir::Function,
    _cfg: &mut ControlFlowGraph,
    isa: &dyn TargetIsa,
) {
    let mut pos = FuncCursor::new(func).at_inst(inst);
    pos.use_srcloc(inst);

    let (ptr, offset, flags) = match pos.func.dfg[inst] {
        ir::InstructionData::Load {
            opcode: ir::Opcode::Load,
            arg,
            offset,
            flags,
        } => (arg, offset, flags),
        _ => panic!("Expected load: {}", pos.func.dfg.display_inst(inst, None)),
    };

    let res_ty = pos.func.dfg.ctrl_typevar(inst);
    let small_ty = res_ty.half_width().expect("Can't narrow load");

    let al = pos.ins().load(small_ty, flags, ptr, offset);
    let ah = pos.ins().load(
        small_ty,
        flags,
        ptr,
        offset.try_add_i64(8).expect("load offset overflow"),
    );
    let (al, ah) = match flags.endianness(isa.endianness()) {
        ir::Endianness::Little => (al, ah),
        ir::Endianness::Big => (ah, al),
    };
    pos.func.dfg.replace(inst).iconcat(al, ah);
}

/// Split a store into two parts after `isplit`ing the value.
fn narrow_store(
    inst: ir::Inst,
    func: &mut ir::Function,
    _cfg: &mut ControlFlowGraph,
    isa: &dyn TargetIsa,
) {
    let mut pos = FuncCursor::new(func).at_inst(inst);
    pos.use_srcloc(inst);

    let (val, ptr, offset, flags) = match pos.func.dfg[inst] {
        ir::InstructionData::Store {
            opcode: ir::Opcode::Store,
            args,
            offset,
            flags,
        } => (args[0], args[1], offset, flags),
        _ => panic!("Expected store: {}", pos.func.dfg.display_inst(inst, None)),
    };

    let (al, ah) = pos.ins().isplit(val);
    let (al, ah) = match flags.endianness(isa.endianness()) {
        ir::Endianness::Little => (al, ah),
        ir::Endianness::Big => (ah, al),
    };
    pos.ins().store(flags, al, ptr, offset);
    pos.ins().store(
        flags,
        ah,
        ptr,
        offset.try_add_i64(8).expect("store offset overflow"),
    );
    pos.remove_inst();
}

/// Expands an illegal iconst value by splitting it into two.
fn narrow_iconst(
    inst: ir::Inst,
    func: &mut ir::Function,
    _cfg: &mut ControlFlowGraph,
    isa: &dyn TargetIsa,
) {
    let imm: i64 = if let ir::InstructionData::UnaryImm {
        opcode: ir::Opcode::Iconst,
        imm,
    } = &func.dfg[inst]
    {
        (*imm).into()
    } else {
        panic!("unexpected instruction in narrow_iconst");
    };

    let mut pos = FuncCursor::new(func).at_inst(inst);
    pos.use_srcloc(inst);

    let ty = pos.func.dfg.ctrl_typevar(inst);
    if isa.pointer_bits() == 32 && ty == I64 {
        let low = pos.ins().iconst(I32, imm & 0xffffffff);
        let high = pos.ins().iconst(I32, imm >> 32);
        // The instruction has as many results as iconcat, so no need to replace them.
        pos.func.dfg.replace(inst).iconcat(low, high);
        return;
    }

    unimplemented!("missing encoding or legalization for iconst.{:?}", ty);
}

fn narrow_icmp_imm(
    inst: ir::Inst,
    func: &mut ir::Function,
    _cfg: &mut ControlFlowGraph,
    _isa: &dyn TargetIsa,
) {
    use crate::ir::condcodes::{CondCode, IntCC};

    let (arg, cond, imm): (ir::Value, IntCC, i64) = match func.dfg[inst] {
        ir::InstructionData::IntCompareImm {
            opcode: ir::Opcode::IcmpImm,
            arg,
            cond,
            imm,
        } => (arg, cond, imm.into()),
        _ => panic!("unexpected instruction in narrow_icmp_imm"),
    };

    let mut pos = FuncCursor::new(func).at_inst(inst);
    pos.use_srcloc(inst);

    let ty = pos.func.dfg.ctrl_typevar(inst);
    let ty_half = ty.half_width().unwrap();

    let mask = ((1u128 << ty_half.bits()) - 1) as i64;
    let imm_low = pos.ins().iconst(ty_half, imm & mask);
    let imm_high = pos.ins().iconst(
        ty_half,
        imm.checked_shr(ty_half.bits().into()).unwrap_or(0) & mask,
    );
    let (arg_low, arg_high) = pos.ins().isplit(arg);

    match cond {
        IntCC::Equal => {
            let res_low = pos.ins().icmp(cond, arg_low, imm_low);
            let res_high = pos.ins().icmp(cond, arg_high, imm_high);
            pos.func.dfg.replace(inst).band(res_low, res_high);
        }
        IntCC::NotEqual => {
            let res_low = pos.ins().icmp(cond, arg_low, imm_low);
            let res_high = pos.ins().icmp(cond, arg_high, imm_high);
            pos.func.dfg.replace(inst).bor(res_low, res_high);
        }
        IntCC::SignedGreaterThan
        | IntCC::SignedGreaterThanOrEqual
        | IntCC::SignedLessThan
        | IntCC::SignedLessThanOrEqual
        | IntCC::UnsignedGreaterThan
        | IntCC::UnsignedGreaterThanOrEqual
        | IntCC::UnsignedLessThan
        | IntCC::UnsignedLessThanOrEqual => {
            let b1 = pos.ins().icmp(cond.without_equal(), arg_high, imm_high);
            let b2 = pos
                .ins()
                .icmp(cond.inverse().without_equal(), arg_high, imm_high);
            let b3 = pos.ins().icmp(cond.unsigned(), arg_low, imm_low);
            let c1 = pos.ins().bnot(b2);
            let c2 = pos.ins().band(c1, b3);
            pos.func.dfg.replace(inst).bor(b1, c2);
        }
        _ => unimplemented!("missing legalization for condition {:?}", cond),
    }
}
