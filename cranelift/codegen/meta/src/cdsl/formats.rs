use crate::cdsl::operands::OperandKind;
use std::fmt;
use std::rc::Rc;

/// An immediate field in an instruction format.
///
/// This corresponds to a single member of a variant of the `InstructionData`
/// data type.
#[derive(Debug)]
pub(crate) struct FormatField {
    /// Immediate operand kind.
    pub kind: OperandKind,

    /// Member name in InstructionData variant.
    pub member: &'static str,
}

/// Every instruction opcode has a corresponding instruction format which determines the number of
/// operands and their kinds. Instruction formats are identified structurally, i.e., the format of
/// an instruction is derived from the kinds of operands used in its declaration.
///
/// The instruction format stores two separate lists of operands: Immediates and values. Immediate
/// operands (including entity references) are represented as explicit members in the
/// `InstructionData` variants. The value operands are stored differently, depending on how many
/// there are.  Beyond a certain point, instruction formats switch to an external value list for
/// storing value arguments. Value lists can hold an arbitrary number of values.
///
/// All instruction formats must be predefined in the meta shared/formats.rs module.
#[derive(Debug)]
pub(crate) struct InstructionFormat {
    /// Instruction format name in CamelCase. This is used as a Rust variant name in both the
    /// `InstructionData` and `InstructionFormat` enums.
    pub name: &'static str,

    pub num_value_operands: usize,

    pub has_value_list: bool,

    pub imm_fields: Vec<FormatField>,

    /// Index of the value input operand that is used to infer the controlling type variable. By
    /// default, this is `0`, the first `value` operand. The index is relative to the values only,
    /// ignoring immediate operands.
    pub typevar_operand: Option<usize>,
}

/// A tuple serving as a key to deduplicate InstructionFormat.
#[derive(Hash, PartialEq, Eq)]
pub(crate) struct FormatStructure {
    pub num_value_operands: usize,
    pub has_value_list: bool,
    /// Tuples of (Rust field name / Rust type) for each immediate field.
    pub imm_field_names: Vec<(&'static str, &'static str)>,
}

impl fmt::Display for InstructionFormat {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        let imm_args = self
            .imm_fields
            .iter()
            .map(|field| format!("{}: {}", field.member, field.kind.rust_type))
            .collect::<Vec<_>>()
            .join(", ");
        fmt.write_fmt(format_args!(
            "{}(imms=({}), vals={})",
            self.name, imm_args, self.num_value_operands
        ))?;
        Ok(())
    }
}

impl InstructionFormat {
    /// Returns a tuple that uniquely identifies the structure.
    pub fn structure(&self) -> FormatStructure {
        FormatStructure {
            num_value_operands: self.num_value_operands,
            has_value_list: self.has_value_list,
            imm_field_names: self
                .imm_fields
                .iter()
                .map(|field| (field.kind.rust_field_name, field.kind.rust_type))
                .collect::<Vec<_>>(),
        }
    }
}

pub(crate) struct InstructionFormatBuilder {
    name: &'static str,
    num_value_operands: usize,
    has_value_list: bool,
    imm_fields: Vec<FormatField>,
    typevar_operand: Option<usize>,
}

impl InstructionFormatBuilder {
    pub fn new(name: &'static str) -> Self {
        Self {
            name,
            num_value_operands: 0,
            has_value_list: false,
            imm_fields: Vec::new(),
            typevar_operand: None,
        }
    }

    pub fn value(mut self) -> Self {
        self.num_value_operands += 1;
        self
    }

    pub fn varargs(mut self) -> Self {
        self.has_value_list = true;
        self
    }

    pub fn imm(mut self, operand_kind: &OperandKind) -> Self {
        let field = FormatField {
            kind: operand_kind.clone(),
            member: operand_kind.rust_field_name,
        };
        self.imm_fields.push(field);
        self
    }

    pub fn imm_with_name(mut self, member: &'static str, operand_kind: &OperandKind) -> Self {
        let field = FormatField {
            kind: operand_kind.clone(),
            member,
        };
        self.imm_fields.push(field);
        self
    }

    pub fn typevar_operand(mut self, operand_index: usize) -> Self {
        assert!(self.typevar_operand.is_none());
        assert!(self.has_value_list || operand_index < self.num_value_operands);
        self.typevar_operand = Some(operand_index);
        self
    }

    pub fn build(self) -> Rc<InstructionFormat> {
        let typevar_operand = if self.typevar_operand.is_some() {
            self.typevar_operand
        } else if self.has_value_list || self.num_value_operands > 0 {
            // Default to the first value operand, if there's one.
            Some(0)
        } else {
            None
        };

        Rc::new(InstructionFormat {
            name: self.name,
            num_value_operands: self.num_value_operands,
            has_value_list: self.has_value_list,
            imm_fields: self.imm_fields,
            typevar_operand,
        })
    }
}
