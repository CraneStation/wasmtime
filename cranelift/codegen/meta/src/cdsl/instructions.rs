use cranelift_entity::{entity_impl, PrimaryMap};

use std::fmt;
use std::fmt::{Display, Error, Formatter};
use std::rc::Rc;

use crate::cdsl::camel_case;
use crate::cdsl::formats::{FormatField, InstructionFormat};
use crate::cdsl::operands::Operand;
use crate::cdsl::type_inference::Constraint;
use crate::cdsl::types::{LaneType, ReferenceType, ValueType};
use crate::cdsl::typevar::TypeVar;

use crate::shared::types::{Bool, Float, Int, Reference};

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct OpcodeNumber(u32);
entity_impl!(OpcodeNumber);

pub(crate) type AllInstructions = PrimaryMap<OpcodeNumber, Instruction>;

pub(crate) struct InstructionGroupBuilder<'all_inst> {
    all_instructions: &'all_inst mut AllInstructions,
    own_instructions: Vec<Instruction>,
}

impl<'all_inst> InstructionGroupBuilder<'all_inst> {
    pub fn new(all_instructions: &'all_inst mut AllInstructions) -> Self {
        Self {
            all_instructions,
            own_instructions: Vec::new(),
        }
    }

    pub fn push(&mut self, builder: InstructionBuilder) {
        let opcode_number = OpcodeNumber(self.all_instructions.next_key().as_u32());
        let inst = builder.build(opcode_number);
        // Note this clone is cheap, since Instruction is a Rc<> wrapper for InstructionContent.
        self.own_instructions.push(inst.clone());
        self.all_instructions.push(inst);
    }

    pub fn build(self) -> InstructionGroup {
        InstructionGroup {
            instructions: self.own_instructions,
        }
    }
}

/// Every instruction must belong to exactly one instruction group. A given
/// target architecture can support instructions from multiple groups, and it
/// does not necessarily support all instructions in a group.
pub(crate) struct InstructionGroup {
    instructions: Vec<Instruction>,
}

impl InstructionGroup {
    pub fn by_name(&self, name: &'static str) -> &Instruction {
        self.instructions
            .iter()
            .find(|inst| inst.name == name)
            .unwrap_or_else(|| panic!("instruction with name '{}' does not exist", name))
    }
}

/// Instructions can have parameters bound to them to specialize them for more specific encodings
/// (e.g. the encoding for adding two float types may be different than that of adding two
/// integer types)
pub(crate) trait Bindable {
    /// Bind a parameter to an instruction
    fn bind(&self, parameter: impl Into<BindParameter>) -> BoundInstruction;
}

#[derive(Debug)]
pub(crate) struct PolymorphicInfo {
    pub use_typevar_operand: bool,
    pub ctrl_typevar: TypeVar,
    pub other_typevars: Vec<TypeVar>,
}

#[derive(Debug)]
pub(crate) struct InstructionContent {
    /// Instruction mnemonic, also becomes opcode name.
    pub name: String,
    pub camel_name: String,
    pub opcode_number: OpcodeNumber,

    /// Documentation string.
    pub doc: String,

    /// Input operands. This can be a mix of SSA value operands and other operand kinds.
    pub operands_in: Vec<Operand>,
    /// Output operands. The output operands must be SSA values or `variable_args`.
    pub operands_out: Vec<Operand>,
    /// Instruction-specific TypeConstraints.
    pub constraints: Vec<Constraint>,

    /// Instruction format, automatically derived from the input operands.
    pub format: Rc<InstructionFormat>,

    /// One of the input or output operands is a free type variable. None if the instruction is not
    /// polymorphic, set otherwise.
    pub polymorphic_info: Option<PolymorphicInfo>,

    /// Indices in operands_in of input operands that are values.
    pub value_opnums: Vec<usize>,
    /// Indices in operands_in of input operands that are immediates or entities.
    pub imm_opnums: Vec<usize>,
    /// Indices in operands_out of output operands that are values.
    pub value_results: Vec<usize>,

    /// True for instructions that terminate the block.
    pub is_terminator: bool,
    /// True for all branch or jump instructions.
    pub is_branch: bool,
    /// True for all indirect branch or jump instructions.',
    pub is_indirect_branch: bool,
    /// Is this a call instruction?
    pub is_call: bool,
    /// Is this a return instruction?
    pub is_return: bool,
    /// Is this a ghost instruction?
    pub is_ghost: bool,
    /// Can this instruction read from memory?
    pub can_load: bool,
    /// Can this instruction write to memory?
    pub can_store: bool,
    /// Can this instruction cause a trap?
    pub can_trap: bool,
    /// Does this instruction have other side effects besides can_* flags?
    pub other_side_effects: bool,
    /// Does this instruction write to CPU flags?
    pub writes_cpu_flags: bool,
    /// Should this opcode be considered to clobber all live registers, during regalloc?
    pub clobbers_all_regs: bool,
}

impl InstructionContent {
    pub fn snake_name(&self) -> &str {
        if &self.name == "return" {
            "return_"
        } else {
            &self.name
        }
    }

    pub fn all_typevars(&self) -> Vec<&TypeVar> {
        match &self.polymorphic_info {
            Some(poly) => {
                let mut result = vec![&poly.ctrl_typevar];
                result.extend(&poly.other_typevars);
                result
            }
            None => Vec::new(),
        }
    }
}

pub(crate) type Instruction = Rc<InstructionContent>;

impl Bindable for Instruction {
    fn bind(&self, parameter: impl Into<BindParameter>) -> BoundInstruction {
        BoundInstruction::new(self).bind(parameter)
    }
}

impl fmt::Display for InstructionContent {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        if !self.operands_out.is_empty() {
            let operands_out = self
                .operands_out
                .iter()
                .map(|op| op.name)
                .collect::<Vec<_>>()
                .join(", ");
            fmt.write_str(&operands_out)?;
            fmt.write_str(" = ")?;
        }

        fmt.write_str(&self.name)?;

        if !self.operands_in.is_empty() {
            let operands_in = self
                .operands_in
                .iter()
                .map(|op| op.name)
                .collect::<Vec<_>>()
                .join(", ");
            fmt.write_str(" ")?;
            fmt.write_str(&operands_in)?;
        }

        Ok(())
    }
}

pub(crate) struct InstructionBuilder {
    name: String,
    doc: String,
    format: Rc<InstructionFormat>,
    operands_in: Option<Vec<Operand>>,
    operands_out: Option<Vec<Operand>>,
    constraints: Option<Vec<Constraint>>,

    // See Instruction comments for the meaning of these fields.
    is_terminator: bool,
    is_branch: bool,
    is_indirect_branch: bool,
    is_call: bool,
    is_return: bool,
    is_ghost: bool,
    can_load: bool,
    can_store: bool,
    can_trap: bool,
    other_side_effects: bool,
    clobbers_all_regs: bool,
}

impl InstructionBuilder {
    pub fn new<S: Into<String>>(name: S, doc: S, format: &Rc<InstructionFormat>) -> Self {
        Self {
            name: name.into(),
            doc: doc.into(),
            format: format.clone(),
            operands_in: None,
            operands_out: None,
            constraints: None,

            is_terminator: false,
            is_branch: false,
            is_indirect_branch: false,
            is_call: false,
            is_return: false,
            is_ghost: false,
            can_load: false,
            can_store: false,
            can_trap: false,
            other_side_effects: false,
            clobbers_all_regs: false,
        }
    }

    pub fn operands_in(mut self, operands: Vec<&Operand>) -> Self {
        assert!(self.operands_in.is_none());
        self.operands_in = Some(operands.iter().map(|x| (*x).clone()).collect());
        self
    }

    pub fn operands_out(mut self, operands: Vec<&Operand>) -> Self {
        assert!(self.operands_out.is_none());
        self.operands_out = Some(operands.iter().map(|x| (*x).clone()).collect());
        self
    }

    pub fn constraints(mut self, constraints: Vec<Constraint>) -> Self {
        assert!(self.constraints.is_none());
        self.constraints = Some(constraints);
        self
    }

    #[allow(clippy::wrong_self_convention)]
    pub fn is_terminator(mut self, val: bool) -> Self {
        self.is_terminator = val;
        self
    }

    #[allow(clippy::wrong_self_convention)]
    pub fn is_branch(mut self, val: bool) -> Self {
        self.is_branch = val;
        self
    }

    #[allow(clippy::wrong_self_convention)]
    pub fn is_indirect_branch(mut self, val: bool) -> Self {
        self.is_indirect_branch = val;
        self
    }

    #[allow(clippy::wrong_self_convention)]
    pub fn is_call(mut self, val: bool) -> Self {
        self.is_call = val;
        self
    }

    #[allow(clippy::wrong_self_convention)]
    pub fn is_return(mut self, val: bool) -> Self {
        self.is_return = val;
        self
    }

    #[allow(clippy::wrong_self_convention)]
    pub fn is_ghost(mut self, val: bool) -> Self {
        self.is_ghost = val;
        self
    }

    pub fn can_load(mut self, val: bool) -> Self {
        self.can_load = val;
        self
    }

    pub fn can_store(mut self, val: bool) -> Self {
        self.can_store = val;
        self
    }

    pub fn can_trap(mut self, val: bool) -> Self {
        self.can_trap = val;
        self
    }

    pub fn other_side_effects(mut self, val: bool) -> Self {
        self.other_side_effects = val;
        self
    }

    fn build(self, opcode_number: OpcodeNumber) -> Instruction {
        let operands_in = self.operands_in.unwrap_or_else(Vec::new);
        let operands_out = self.operands_out.unwrap_or_else(Vec::new);

        let mut value_opnums = Vec::new();
        let mut imm_opnums = Vec::new();
        for (i, op) in operands_in.iter().enumerate() {
            if op.is_value() {
                value_opnums.push(i);
            } else if op.is_immediate_or_entityref() {
                imm_opnums.push(i);
            } else {
                assert!(op.is_varargs());
            }
        }

        let value_results = operands_out
            .iter()
            .enumerate()
            .filter_map(|(i, op)| if op.is_value() { Some(i) } else { None })
            .collect();

        verify_format(&self.name, &operands_in, &self.format);

        let polymorphic_info =
            verify_polymorphic(&operands_in, &operands_out, &self.format, &value_opnums);

        // Infer from output operands whether an instruction clobbers CPU flags or not.
        let writes_cpu_flags = operands_out.iter().any(|op| op.is_cpu_flags());

        let camel_name = camel_case(&self.name);

        Rc::new(InstructionContent {
            name: self.name,
            camel_name,
            opcode_number,
            doc: self.doc,
            operands_in,
            operands_out,
            constraints: self.constraints.unwrap_or_else(Vec::new),
            format: self.format,
            polymorphic_info,
            value_opnums,
            value_results,
            imm_opnums,
            is_terminator: self.is_terminator,
            is_branch: self.is_branch,
            is_indirect_branch: self.is_indirect_branch,
            is_call: self.is_call,
            is_return: self.is_return,
            is_ghost: self.is_ghost,
            can_load: self.can_load,
            can_store: self.can_store,
            can_trap: self.can_trap,
            other_side_effects: self.other_side_effects,
            writes_cpu_flags,
            clobbers_all_regs: self.clobbers_all_regs,
        })
    }
}

/// A thin wrapper like Option<ValueType>, but with more precise semantics.
#[derive(Clone)]
pub(crate) enum ValueTypeOrAny {
    ValueType(ValueType),
}

impl ValueTypeOrAny {
    pub fn expect(self) -> ValueType {
        match self {
            ValueTypeOrAny::ValueType(vt) => vt,
        }
    }
}

/// An parameter used for binding instructions to specific types or values
pub(crate) enum BindParameter {
    Lane(LaneType),
    Reference(ReferenceType),
}

impl From<Int> for BindParameter {
    fn from(ty: Int) -> Self {
        BindParameter::Lane(ty.into())
    }
}

impl From<Bool> for BindParameter {
    fn from(ty: Bool) -> Self {
        BindParameter::Lane(ty.into())
    }
}

impl From<Float> for BindParameter {
    fn from(ty: Float) -> Self {
        BindParameter::Lane(ty.into())
    }
}

impl From<LaneType> for BindParameter {
    fn from(ty: LaneType) -> Self {
        BindParameter::Lane(ty)
    }
}

impl From<Reference> for BindParameter {
    fn from(ty: Reference) -> Self {
        BindParameter::Reference(ty.into())
    }
}

#[derive(Clone)]
pub(crate) enum Immediate {}

impl Display for Immediate {
    fn fmt(&self, _f: &mut Formatter) -> Result<(), Error> {
        match self {
            _ => panic!(),
        }
    }
}

#[derive(Clone)]
pub(crate) struct BoundInstruction {
    pub inst: Instruction,
    pub value_types: Vec<ValueTypeOrAny>,
    pub immediate_values: Vec<Immediate>,
}

impl BoundInstruction {
    /// Construct a new bound instruction (with nothing bound yet) from an instruction
    fn new(inst: &Instruction) -> Self {
        BoundInstruction {
            inst: inst.clone(),
            value_types: vec![],
            immediate_values: vec![],
        }
    }

    /// Verify that the bindings for a BoundInstruction are correct.
    fn verify_bindings(&self) -> Result<(), String> {
        // Verify that binding types to the instruction does not violate the polymorphic rules.
        if !self.value_types.is_empty() {
            match &self.inst.polymorphic_info {
                Some(poly) => {
                    if self.value_types.len() > 1 + poly.other_typevars.len() {
                        return Err(format!(
                            "trying to bind too many types for {}",
                            self.inst.name
                        ));
                    }
                }
                None => {
                    return Err(format!(
                        "trying to bind a type for {} which is not a polymorphic instruction",
                        self.inst.name
                    ));
                }
            }
        }

        // Verify that only the right number of immediates are bound.
        let immediate_count = self
            .inst
            .operands_in
            .iter()
            .filter(|o| o.is_immediate_or_entityref())
            .count();
        if self.immediate_values.len() > immediate_count {
            return Err(format!(
                "trying to bind too many immediates ({}) to instruction {} which only expects {} \
                 immediates",
                self.immediate_values.len(),
                self.inst.name,
                immediate_count
            ));
        }

        Ok(())
    }
}

impl Bindable for BoundInstruction {
    fn bind(&self, parameter: impl Into<BindParameter>) -> BoundInstruction {
        let mut modified = self.clone();
        match parameter.into() {
            BindParameter::Lane(lane_type) => modified
                .value_types
                .push(ValueTypeOrAny::ValueType(lane_type.into())),
            BindParameter::Reference(reference_type) => {
                modified
                    .value_types
                    .push(ValueTypeOrAny::ValueType(reference_type.into()));
            }
        }
        modified.verify_bindings().unwrap();
        modified
    }
}

/// Checks that the input operands actually match the given format.
fn verify_format(inst_name: &str, operands_in: &[Operand], format: &InstructionFormat) {
    // A format is defined by:
    // - its number of input value operands,
    // - its number and names of input immediate operands,
    // - whether it has a value list or not.
    let mut num_values = 0;
    let mut num_immediates = 0;

    for operand in operands_in.iter() {
        if operand.is_varargs() {
            assert!(
                format.has_value_list,
                "instruction {} has varargs, but its format {} doesn't have a value list; you may \
                 need to use a different format.",
                inst_name, format.name
            );
        }
        if operand.is_value() {
            num_values += 1;
        }
        if operand.is_immediate_or_entityref() {
            if let Some(format_field) = format.imm_fields.get(num_immediates) {
                assert_eq!(
                    format_field.kind.rust_field_name,
                    operand.kind.rust_field_name,
                    "{}th operand of {} should be {} (according to format), not {} (according to \
                     inst definition). You may need to use a different format.",
                    num_immediates,
                    inst_name,
                    format_field.kind.rust_field_name,
                    operand.kind.rust_field_name
                );
                num_immediates += 1;
            }
        }
    }

    assert_eq!(
        num_values, format.num_value_operands,
        "inst {} doesn't have as many value input operands as its format {} declares; you may need \
         to use a different format.",
        inst_name, format.name
    );

    assert_eq!(
        num_immediates,
        format.imm_fields.len(),
        "inst {} doesn't have as many immediate input \
         operands as its format {} declares; you may need to use a different format.",
        inst_name,
        format.name
    );
}

/// Check if this instruction is polymorphic, and verify its use of type variables.
fn verify_polymorphic(
    operands_in: &[Operand],
    operands_out: &[Operand],
    format: &InstructionFormat,
    value_opnums: &[usize],
) -> Option<PolymorphicInfo> {
    // The instruction is polymorphic if it has one free input or output operand.
    let is_polymorphic = operands_in
        .iter()
        .any(|op| op.is_value() && op.type_var().unwrap().free_typevar().is_some())
        || operands_out
            .iter()
            .any(|op| op.is_value() && op.type_var().unwrap().free_typevar().is_some());

    if !is_polymorphic {
        return None;
    }

    // Verify the use of type variables.
    let tv_op = format.typevar_operand;
    let mut maybe_error_message = None;
    if let Some(tv_op) = tv_op {
        if tv_op < value_opnums.len() {
            let op_num = value_opnums[tv_op];
            let tv = operands_in[op_num].type_var().unwrap();
            let free_typevar = tv.free_typevar();
            if (free_typevar.is_some() && tv == &free_typevar.unwrap())
                || tv.singleton_type().is_some()
            {
                match is_ctrl_typevar_candidate(tv, &operands_in, &operands_out) {
                    Ok(other_typevars) => {
                        return Some(PolymorphicInfo {
                            use_typevar_operand: true,
                            ctrl_typevar: tv.clone(),
                            other_typevars,
                        });
                    }
                    Err(error_message) => {
                        maybe_error_message = Some(error_message);
                    }
                }
            }
        }
    };

    // If we reached here, it means the type variable indicated as the typevar operand couldn't
    // control every other input and output type variable. We need to look at the result type
    // variables.
    if operands_out.is_empty() {
        // No result means no other possible type variable, so it's a type inference failure.
        match maybe_error_message {
            Some(msg) => panic!("{}", msg),
            None => panic!("typevar_operand must be a free type variable"),
        }
    }

    // Otherwise, try to infer the controlling type variable by looking at the first result.
    let tv = operands_out[0].type_var().unwrap();
    let free_typevar = tv.free_typevar();
    if free_typevar.is_some() && tv != &free_typevar.unwrap() {
        panic!("first result must be a free type variable");
    }

    // At this point, if the next unwrap() fails, it means the output type couldn't be used as a
    // controlling type variable either; panicking is the right behavior.
    let other_typevars = is_ctrl_typevar_candidate(tv, &operands_in, &operands_out).unwrap();

    Some(PolymorphicInfo {
        use_typevar_operand: false,
        ctrl_typevar: tv.clone(),
        other_typevars,
    })
}

/// Verify that the use of TypeVars is consistent with `ctrl_typevar` as the controlling type
/// variable.
///
/// All polymorhic inputs must either be derived from `ctrl_typevar` or be independent free type
/// variables only used once.
///
/// All polymorphic results must be derived from `ctrl_typevar`.
///
/// Return a vector of other type variables used, or a string explaining what went wrong.
fn is_ctrl_typevar_candidate(
    ctrl_typevar: &TypeVar,
    operands_in: &[Operand],
    operands_out: &[Operand],
) -> Result<Vec<TypeVar>, String> {
    let mut other_typevars = Vec::new();

    // Check value inputs.
    for input in operands_in {
        if !input.is_value() {
            continue;
        }

        let typ = input.type_var().unwrap();
        let free_typevar = typ.free_typevar();

        // Non-polymorphic or derived from ctrl_typevar is OK.
        if free_typevar.is_none() {
            continue;
        }
        let free_typevar = free_typevar.unwrap();
        if &free_typevar == ctrl_typevar {
            continue;
        }

        // No other derived typevars allowed.
        if typ != &free_typevar {
            return Err(format!(
                "{:?}: type variable {} must be derived from {:?} while it is derived from {:?}",
                input, typ.name, ctrl_typevar, free_typevar
            ));
        }

        // Other free type variables can only be used once each.
        for other_tv in &other_typevars {
            if &free_typevar == other_tv {
                return Err(format!(
                    "non-controlling type variable {} can't be used more than once",
                    free_typevar.name
                ));
            }
        }

        other_typevars.push(free_typevar);
    }

    // Check outputs.
    for result in operands_out {
        if !result.is_value() {
            continue;
        }

        let typ = result.type_var().unwrap();
        let free_typevar = typ.free_typevar();

        // Non-polymorphic or derived from ctrl_typevar is OK.
        if free_typevar.is_none() || &free_typevar.unwrap() == ctrl_typevar {
            continue;
        }

        return Err("type variable in output not derived from ctrl_typevar".into());
    }

    Ok(other_typevars)
}

#[derive(Clone, Hash, PartialEq, Eq)]
pub(crate) enum FormatPredicateKind {
    /// Is the field member equal to the expected value (stored here)?
    IsEqual(String),
}

#[derive(Clone, Hash, PartialEq, Eq)]
pub(crate) struct FormatPredicateNode {
    format_name: &'static str,
    member_name: &'static str,
    kind: FormatPredicateKind,
}

impl FormatPredicateNode {
    fn new_raw(
        format: &InstructionFormat,
        member_name: &'static str,
        kind: FormatPredicateKind,
    ) -> Self {
        Self {
            format_name: format.name,
            member_name,
            kind,
        }
    }

    fn destructuring_member_name(&self) -> &'static str {
        match &self.kind {
            _ => self.member_name,
        }
    }

    fn rust_predicate(&self) -> String {
        match &self.kind {
            FormatPredicateKind::IsEqual(arg) => {
                format!("predicates::is_equal({}, {})", self.member_name, arg)
            }
        }
    }
}

#[derive(Clone, Hash, PartialEq, Eq)]
pub(crate) enum TypePredicateNode {
    /// Is the value argument (at the index designated by the first member) the same type as the
    /// type name (second member)?
    TypeVarCheck(usize, String),

    /// Is the controlling type variable the same type as the one designated by the type name
    /// (only member)?
    CtrlTypeVarCheck(String),
}

impl TypePredicateNode {
    fn rust_predicate(&self, func_str: &str) -> String {
        match self {
            TypePredicateNode::TypeVarCheck(index, value_type_name) => format!(
                "{}.dfg.value_type(args[{}]) == {}",
                func_str, index, value_type_name
            ),
            TypePredicateNode::CtrlTypeVarCheck(value_type_name) => {
                format!("{}.dfg.ctrl_typevar(inst) == {}", func_str, value_type_name)
            }
        }
    }
}

/// A basic node in an instruction predicate: either an atom, or an AND of two conditions.
#[derive(Clone, Hash, PartialEq, Eq)]
pub(crate) enum InstructionPredicateNode {
    FormatPredicate(FormatPredicateNode),

    TypePredicate(TypePredicateNode),

    /// An AND-combination of two or more other predicates.
    And(Vec<InstructionPredicateNode>),
}

impl InstructionPredicateNode {
    fn rust_predicate(&self, func_str: &str) -> String {
        match self {
            InstructionPredicateNode::FormatPredicate(node) => node.rust_predicate(),
            InstructionPredicateNode::TypePredicate(node) => node.rust_predicate(func_str),
            InstructionPredicateNode::And(nodes) => nodes
                .iter()
                .map(|x| x.rust_predicate(func_str))
                .collect::<Vec<_>>()
                .join(" && "),
        }
    }

    pub fn format_destructuring_member_name(&self) -> &str {
        match self {
            InstructionPredicateNode::FormatPredicate(format_pred) => {
                format_pred.destructuring_member_name()
            }
            _ => panic!("Only for leaf format predicates"),
        }
    }

    pub fn format_name(&self) -> &str {
        match self {
            InstructionPredicateNode::FormatPredicate(format_pred) => format_pred.format_name,
            _ => panic!("Only for leaf format predicates"),
        }
    }

    pub fn is_type_predicate(&self) -> bool {
        match self {
            InstructionPredicateNode::FormatPredicate(_) | InstructionPredicateNode::And(_) => {
                false
            }
            InstructionPredicateNode::TypePredicate(_) => true,
        }
    }

    fn collect_leaves(&self) -> Vec<&InstructionPredicateNode> {
        let mut ret = Vec::new();
        match self {
            InstructionPredicateNode::And(nodes) => {
                for node in nodes {
                    ret.extend(node.collect_leaves());
                }
            }
            _ => ret.push(self),
        }
        ret
    }
}

#[derive(Clone, Hash, PartialEq, Eq)]
pub(crate) struct InstructionPredicate {
    node: Option<InstructionPredicateNode>,
}

impl Into<InstructionPredicate> for InstructionPredicateNode {
    fn into(self) -> InstructionPredicate {
        InstructionPredicate { node: Some(self) }
    }
}

impl InstructionPredicate {
    pub fn new() -> Self {
        Self { node: None }
    }

    pub fn new_typevar_check(
        inst: &Instruction,
        type_var: &TypeVar,
        value_type: &ValueType,
    ) -> InstructionPredicateNode {
        let index = inst
            .value_opnums
            .iter()
            .enumerate()
            .find(|(_, &op_num)| inst.operands_in[op_num].type_var().unwrap() == type_var)
            .unwrap()
            .0;
        InstructionPredicateNode::TypePredicate(TypePredicateNode::TypeVarCheck(
            index,
            value_type.rust_name(),
        ))
    }

    pub fn new_ctrl_typevar_check(value_type: &ValueType) -> InstructionPredicateNode {
        InstructionPredicateNode::TypePredicate(TypePredicateNode::CtrlTypeVarCheck(
            value_type.rust_name(),
        ))
    }

    /// Used only for the AST module, which directly passes in the format field.
    pub fn new_is_field_equal_ast(
        format: &InstructionFormat,
        field: &FormatField,
        imm_value: String,
    ) -> InstructionPredicateNode {
        InstructionPredicateNode::FormatPredicate(FormatPredicateNode::new_raw(
            format,
            field.member,
            FormatPredicateKind::IsEqual(imm_value),
        ))
    }

    pub fn and(mut self, new_node: InstructionPredicateNode) -> Self {
        let node = self.node;
        let mut and_nodes = match node {
            Some(node) => match node {
                InstructionPredicateNode::And(nodes) => nodes,
                _ => vec![node],
            },
            _ => Vec::new(),
        };
        and_nodes.push(new_node);
        self.node = Some(InstructionPredicateNode::And(and_nodes));
        self
    }

    pub fn rust_predicate(&self, func_str: &str) -> Option<String> {
        self.node.as_ref().map(|root| root.rust_predicate(func_str))
    }

    /// Returns the type predicate if this is one, or None otherwise.
    pub fn type_predicate(&self, func_str: &str) -> Option<String> {
        let node = self.node.as_ref().unwrap();
        if node.is_type_predicate() {
            Some(node.rust_predicate(func_str))
        } else {
            None
        }
    }

    /// Returns references to all the nodes that are leaves in the condition (i.e. by flattening
    /// AND/OR).
    pub fn collect_leaves(&self) -> Vec<&InstructionPredicateNode> {
        self.node.as_ref().unwrap().collect_leaves()
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct InstructionPredicateNumber(u32);
entity_impl!(InstructionPredicateNumber);

pub(crate) type InstructionPredicateMap =
    PrimaryMap<InstructionPredicateNumber, InstructionPredicate>;

/// An instruction specification, containing an instruction that has bound types or not.
pub(crate) enum InstSpec {
    Inst(Instruction),
    Bound(BoundInstruction),
}

impl InstSpec {
    pub fn inst(&self) -> &Instruction {
        match &self {
            InstSpec::Inst(inst) => inst,
            InstSpec::Bound(bound_inst) => &bound_inst.inst,
        }
    }
}

impl Bindable for InstSpec {
    fn bind(&self, parameter: impl Into<BindParameter>) -> BoundInstruction {
        match self {
            InstSpec::Inst(inst) => inst.bind(parameter.into()),
            InstSpec::Bound(inst) => inst.bind(parameter.into()),
        }
    }
}

impl Into<InstSpec> for &Instruction {
    fn into(self) -> InstSpec {
        InstSpec::Inst(self.clone())
    }
}

impl Into<InstSpec> for BoundInstruction {
    fn into(self) -> InstSpec {
        InstSpec::Bound(self)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::cdsl::formats::InstructionFormatBuilder;
    use crate::cdsl::operands::{OperandKind, OperandKindFields};
    use crate::cdsl::typevar::TypeSetBuilder;
    use crate::shared::types::Int::{I32, I64};

    fn field_to_operand(index: usize, field: OperandKindFields) -> Operand {
        // Pretend the index string is &'static.
        let name = Box::leak(index.to_string().into_boxed_str());
        // Format's name / rust_type don't matter here.
        let kind = OperandKind::new(name, name, field);
        let operand = Operand::new(name, kind);
        operand
    }

    fn field_to_operands(types: Vec<OperandKindFields>) -> Vec<Operand> {
        types
            .iter()
            .enumerate()
            .map(|(i, f)| field_to_operand(i, f.clone()))
            .collect()
    }

    fn build_fake_instruction(
        inputs: Vec<OperandKindFields>,
        outputs: Vec<OperandKindFields>,
    ) -> Instruction {
        // Setup a format from the input operands.
        let mut format = InstructionFormatBuilder::new("fake");
        for (i, f) in inputs.iter().enumerate() {
            match f {
                OperandKindFields::TypeVar(_) => format = format.value(),
                OperandKindFields::ImmValue => {
                    format = format.imm(&field_to_operand(i, f.clone()).kind)
                }
                _ => {}
            };
        }
        let format = format.build();

        // Create the fake instruction.
        InstructionBuilder::new("fake", "A fake instruction for testing.", &format)
            .operands_in(field_to_operands(inputs).iter().collect())
            .operands_out(field_to_operands(outputs).iter().collect())
            .build(OpcodeNumber(42))
    }

    #[test]
    fn ensure_bound_instructions_can_bind_lane_types() {
        let type1 = TypeSetBuilder::new().ints(8..64).build();
        let in1 = OperandKindFields::TypeVar(TypeVar::new("a", "...", type1));
        let inst = build_fake_instruction(vec![in1], vec![]);
        inst.bind(LaneType::Int(I32));
    }

    #[test]
    fn ensure_bound_instructions_can_bind_immediates() {
        let inst = build_fake_instruction(vec![OperandKindFields::ImmValue], vec![]);
        let bound_inst = inst.bind(Immediate::IntCC(IntCC::Equal));
        assert!(bound_inst.verify_bindings().is_ok());
    }

    #[test]
    #[should_panic]
    fn ensure_instructions_fail_to_bind() {
        let inst = build_fake_instruction(vec![], vec![]);
        inst.bind(BindParameter::Lane(LaneType::Int(I32)));
        // Trying to bind to an instruction with no inputs should fail.
    }

    #[test]
    #[should_panic]
    fn ensure_bound_instructions_fail_to_bind_too_many_types() {
        let type1 = TypeSetBuilder::new().ints(8..64).build();
        let in1 = OperandKindFields::TypeVar(TypeVar::new("a", "...", type1));
        let inst = build_fake_instruction(vec![in1], vec![]);
        inst.bind(LaneType::Int(I32)).bind(LaneType::Int(I64));
    }

    #[test]
    #[should_panic]
    fn ensure_instructions_fail_to_bind_too_many_immediates() {
        let inst = build_fake_instruction(vec![OperandKindFields::ImmValue], vec![]);
        inst.bind(BindParameter::Immediate(Immediate::IntCC(IntCC::Equal)))
            .bind(BindParameter::Immediate(Immediate::IntCC(IntCC::Equal)));
        // Trying to bind too many immediates to an instruction should fail; note that the immediate
        // values are nonsensical but irrelevant to the purpose of this test.
    }
}
