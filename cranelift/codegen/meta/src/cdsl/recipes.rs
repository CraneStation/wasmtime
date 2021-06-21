use std::rc::Rc;

use cranelift_entity::{entity_impl, PrimaryMap};

use crate::cdsl::formats::InstructionFormat;
use crate::cdsl::instructions::InstructionPredicate;
use crate::cdsl::regs::RegClassIndex;
use crate::cdsl::settings::SettingPredicateNumber;

/// A specific register in a register class.
///
/// A register is identified by the top-level register class it belongs to and
/// its first register unit.
///
/// Specific registers are used to describe constraints on instructions where
/// some operands must use a fixed register.
///
/// Register instances can be created with the constructor, or accessed as
/// attributes on the register class: `GPR.rcx`.
#[derive(Copy, Clone, Hash, PartialEq, Eq)]
pub(crate) struct Register {
    pub regclass: RegClassIndex,
    pub unit: u8,
}

/// An operand that must be in a stack slot.
///
/// A `Stack` object can be used to indicate an operand constraint for a value
/// operand that must live in a stack slot.
#[derive(Copy, Clone, Hash, PartialEq)]
pub(crate) struct Stack {
    pub regclass: RegClassIndex,
}

#[derive(Clone, Hash, PartialEq)]
pub(crate) struct BranchRange {
    pub inst_size: u64,
    pub range: u64,
}

#[derive(Copy, Clone, Hash, PartialEq)]
pub(crate) enum OperandConstraint {
    RegClass(RegClassIndex),
    FixedReg(Register),
    TiedInput(usize),
    Stack(Stack),
}

impl Into<OperandConstraint> for RegClassIndex {
    fn into(self) -> OperandConstraint {
        OperandConstraint::RegClass(self)
    }
}

impl Into<OperandConstraint> for Register {
    fn into(self) -> OperandConstraint {
        OperandConstraint::FixedReg(self)
    }
}

impl Into<OperandConstraint> for usize {
    fn into(self) -> OperandConstraint {
        OperandConstraint::TiedInput(self)
    }
}

impl Into<OperandConstraint> for Stack {
    fn into(self) -> OperandConstraint {
        OperandConstraint::Stack(self)
    }
}

/// A recipe for encoding instructions with a given format.
///
/// Many different instructions can be encoded by the same recipe, but they
/// must all have the same instruction format.
///
/// The `operands_in` and `operands_out` arguments are tuples specifying the register
/// allocation constraints for the value operands and results respectively. The
/// possible constraints for an operand are:
///
/// - A `RegClass` specifying the set of allowed registers.
/// - A `Register` specifying a fixed-register operand.
/// - An integer indicating that this result is tied to a value operand, so
///   they must use the same register.
/// - A `Stack` specifying a value in a stack slot.
///
/// The `branch_range` argument must be provided for recipes that can encode
/// branch instructions. It is an `(origin, bits)` tuple describing the exact
/// range that can be encoded in a branch instruction.
#[derive(Clone)]
pub(crate) struct EncodingRecipe {
    /// Short mnemonic name for this recipe.
    pub name: String,

    /// Associated instruction format.
    pub format: Rc<InstructionFormat>,

    /// Base number of bytes in the binary encoded instruction.
    pub base_size: u64,

    /// Tuple of register constraints for value operands.
    pub operands_in: Vec<OperandConstraint>,

    /// Tuple of register constraints for results.
    pub operands_out: Vec<OperandConstraint>,

    /// Function name to use when computing actual size.
    pub compute_size: &'static str,

    /// `(origin, bits)` range for branches.
    pub branch_range: Option<BranchRange>,

    /// This instruction clobbers `iflags` and `fflags`; true by default.
    pub clobbers_flags: bool,

    /// Instruction predicate.
    pub inst_predicate: Option<InstructionPredicate>,

    /// ISA predicate.
    pub isa_predicate: Option<SettingPredicateNumber>,

    /// Rust code for binary emission.
    pub emit: Option<String>,
}

// Implement PartialEq ourselves: take all the fields into account but the name.
impl PartialEq for EncodingRecipe {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.format, &other.format)
            && self.base_size == other.base_size
            && self.operands_in == other.operands_in
            && self.operands_out == other.operands_out
            && self.compute_size == other.compute_size
            && self.branch_range == other.branch_range
            && self.clobbers_flags == other.clobbers_flags
            && self.inst_predicate == other.inst_predicate
            && self.isa_predicate == other.isa_predicate
            && self.emit == other.emit
    }
}

// To allow using it in a hashmap.
impl Eq for EncodingRecipe {}

#[derive(Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct EncodingRecipeNumber(u32);
entity_impl!(EncodingRecipeNumber);

pub(crate) type Recipes = PrimaryMap<EncodingRecipeNumber, EncodingRecipe>;

#[derive(Clone)]
pub(crate) struct EncodingRecipeBuilder {
    pub name: String,
    format: Rc<InstructionFormat>,
    pub base_size: u64,
    pub operands_in: Option<Vec<OperandConstraint>>,
    pub operands_out: Option<Vec<OperandConstraint>>,
    pub compute_size: Option<&'static str>,
    pub branch_range: Option<BranchRange>,
    pub emit: Option<String>,
    clobbers_flags: Option<bool>,
    inst_predicate: Option<InstructionPredicate>,
    isa_predicate: Option<SettingPredicateNumber>,
}
