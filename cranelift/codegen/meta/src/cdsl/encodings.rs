use crate::cdsl::instructions::{
    InstSpec, Instruction,
    InstructionPredicateNumber
};
use crate::cdsl::recipes::{EncodingRecipeNumber, Recipes};
use crate::cdsl::settings::SettingPredicateNumber;
use crate::cdsl::types::ValueType;
use std::rc::Rc;

/// Encoding for a concrete instruction.
///
/// An `Encoding` object ties an instruction opcode with concrete type variables together with an
/// encoding recipe and encoding encbits.
///
/// The concrete instruction can be in three different forms:
///
/// 1. A naked opcode: `trap` for non-polymorphic instructions.
/// 2. With bound type variables: `iadd.i32` for polymorphic instructions.
/// 3. With operands providing constraints: `icmp.i32(intcc.eq, x, y)`.
///
/// If the instruction is polymorphic, all type variables must be provided.
pub(crate) struct EncodingContent {
    /// The `Instruction` or `BoundInstruction` being encoded.
    inst: InstSpec,

    /// The `EncodingRecipe` to use.
    pub recipe: EncodingRecipeNumber,

    /// Additional encoding bits to be interpreted by `recipe`.
    pub encbits: u16,

    /// An instruction predicate that must be true to allow selecting this encoding.
    pub inst_predicate: Option<InstructionPredicateNumber>,

    /// An ISA predicate that must be true to allow selecting this encoding.
    pub isa_predicate: Option<SettingPredicateNumber>,

    /// The value type this encoding has been bound to, for encodings of polymorphic instructions.
    pub bound_type: Option<ValueType>,
}

impl EncodingContent {
    pub fn inst(&self) -> &Instruction {
        self.inst.inst()
    }
    pub fn to_rust_comment(&self, recipes: &Recipes) -> String {
        format!("[{}#{:02x}]", recipes[self.recipe].name, self.encbits)
    }
}

pub(crate) type Encoding = Rc<EncodingContent>;

