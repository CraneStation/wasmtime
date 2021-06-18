//! Instruction Set Architectures.
//!
//! The `isa` module provides a `TargetIsa` trait which provides the behavior specialization needed
//! by the ISA-independent code generator. The sub-modules of this module provide definitions for
//! the instruction sets that Cranelift can target. Each sub-module has it's own implementation of
//! `TargetIsa`.
//!
//! # Constructing a `TargetIsa` instance
//!
//! The target ISA is built from the following information:
//!
//! - The name of the target ISA as a string. Cranelift is a cross-compiler, so the ISA to target
//!   can be selected dynamically. Individual ISAs can be left out when Cranelift is compiled, so a
//!   string is used to identify the proper sub-module.
//! - Values for settings that apply to all ISAs. This is represented by a `settings::Flags`
//!   instance.
//! - Values for ISA-specific settings.
//!
//! The `isa::lookup()` function is the main entry point which returns an `isa::Builder`
//! appropriate for the requested ISA:
//!
//! ```
//! # #[macro_use] extern crate target_lexicon;
//! use cranelift_codegen::isa;
//! use cranelift_codegen::settings::{self, Configurable};
//! use std::str::FromStr;
//! use target_lexicon::Triple;
//!
//! let shared_builder = settings::builder();
//! let shared_flags = settings::Flags::new(shared_builder);
//!
//! match isa::lookup(triple!("x86_64")) {
//!     Err(_) => {
//!         // The x86_64 target ISA is not available.
//!     }
//!     Ok(mut isa_builder) => {
//!         isa_builder.set("use_popcnt", "on");
//!         let isa = isa_builder.finish(shared_flags);
//!     }
//! }
//! ```
//!
//! The configured target ISA trait object is a `Box<TargetIsa>` which can be used for multiple
//! concurrent function compilations.

pub use crate::isa::call_conv::CallConv;
pub use crate::isa::constraints::{
    BranchRange, ConstraintKind, OperandConstraint, RecipeConstraints,
};
pub use crate::isa::enc_tables::Encodings;
pub use crate::isa::encoding::{base_size, EncInfo, Encoding};
pub use crate::isa::registers::{regs_overlap, RegClass, RegClassIndex, RegInfo, RegUnit};
pub use crate::isa::stack::{StackBase, StackBaseMask, StackRef};

use crate::binemit;
use crate::flowgraph;
use crate::ir;
#[cfg(feature = "unwind")]
use crate::isa::unwind::systemv::RegisterMappingError;
use crate::machinst::{MachBackend, UnwindInfoKind};
use crate::regalloc;
use crate::result::CodegenResult;
use crate::settings;
use crate::settings::SetResult;
use crate::timing;
use alloc::{borrow::Cow, boxed::Box, vec::Vec};
use core::any::Any;
use core::fmt;
use core::fmt::{Debug, Formatter};
use core::hash::Hasher;
use target_lexicon::{triple, Architecture, OperatingSystem, PointerWidth, Triple};

#[cfg(feature = "riscv")]
mod riscv;

// This module is made public here for benchmarking purposes. No guarantees are
// made regarding API stability.
#[cfg(feature = "x86")]
pub mod x64;

#[cfg(feature = "arm32")]
mod arm32;

#[cfg(feature = "arm64")]
pub(crate) mod aarch64;

#[cfg(feature = "s390x")]
mod s390x;

pub mod unwind;

mod call_conv;
mod constraints;
mod enc_tables;
mod encoding;
pub mod registers;
mod stack;

#[cfg(test)]
mod test_utils;

/// Returns a builder that can create a corresponding `TargetIsa`
/// or `Err(LookupError::SupportDisabled)` if not enabled.
macro_rules! isa_builder {
    ($name: ident, $cfg_terms: tt, $triple: ident) => {{
        #[cfg $cfg_terms]
        {
            Ok($name::isa_builder($triple))
        }
        #[cfg(not $cfg_terms)]
        {
            Err(LookupError::SupportDisabled)
        }
    }};
}

/// Look for an ISA for the given `triple`, selecting the backend variant given
/// by `variant` if available.
pub fn lookup_variant(triple: Triple) -> Result<Builder, LookupError> {
    match triple.architecture {
        Architecture::Riscv32 { .. } | Architecture::Riscv64 { .. } => {
            isa_builder!(riscv, (feature = "riscv"), triple)
        }
        Architecture::X86_64 => {
            isa_builder!(x64, (feature = "x86"), triple)
        }
        Architecture::Arm { .. } => isa_builder!(arm32, (feature = "arm32"), triple),
        Architecture::Aarch64 { .. } => isa_builder!(aarch64, (feature = "arm64"), triple),
        Architecture::S390x { .. } => isa_builder!(s390x, (feature = "s390x"), triple),
        _ => Err(LookupError::Unsupported),
    }
}

/// Look for an ISA for the given `triple`.
/// Return a builder that can create a corresponding `TargetIsa`.
pub fn lookup(triple: Triple) -> Result<Builder, LookupError> {
    lookup_variant(triple)
}

/// Look for a supported ISA with the given `name`.
/// Return a builder that can create a corresponding `TargetIsa`.
pub fn lookup_by_name(name: &str) -> Result<Builder, LookupError> {
    use alloc::str::FromStr;
    lookup(triple!(name))
}

/// Describes reason for target lookup failure
#[derive(PartialEq, Eq, Copy, Clone, Debug)]
pub enum LookupError {
    /// Support for this target was disabled in the current build.
    SupportDisabled,

    /// Support for this target has not yet been implemented.
    Unsupported,
}

// This is manually implementing Error and Display instead of using thiserror to reduce the amount
// of dependencies used by Cranelift.
impl std::error::Error for LookupError {}

impl fmt::Display for LookupError {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        match self {
            LookupError::SupportDisabled => write!(f, "Support for this target is disabled"),
            LookupError::Unsupported => {
                write!(f, "Support for this target has not been implemented yet")
            }
        }
    }
}

/// Builder for a `TargetIsa`.
/// Modify the ISA-specific settings before creating the `TargetIsa` trait object with `finish`.
#[derive(Clone)]
pub struct Builder {
    triple: Triple,
    setup: settings::Builder,
    constructor: fn(Triple, settings::Flags, settings::Builder) -> Box<dyn TargetIsa>,
}

impl Builder {
    /// Gets the triple for the builder.
    pub fn triple(&self) -> &Triple {
        &self.triple
    }

    /// Iterates the available settings in the builder.
    pub fn iter(&self) -> impl Iterator<Item = settings::Setting> {
        self.setup.iter()
    }

    /// Combine the ISA-specific settings with the provided ISA-independent settings and allocate a
    /// fully configured `TargetIsa` trait object.
    pub fn finish(self, shared_flags: settings::Flags) -> Box<dyn TargetIsa> {
        (self.constructor)(self.triple, shared_flags, self.setup)
    }
}

impl settings::Configurable for Builder {
    fn set(&mut self, name: &str, value: &str) -> SetResult<()> {
        self.setup.set(name, value)
    }

    fn enable(&mut self, name: &str) -> SetResult<()> {
        self.setup.enable(name)
    }
}

/// After determining that an instruction doesn't have an encoding, how should we proceed to
/// legalize it?
///
/// The `Encodings` iterator returns a legalization function to call.
pub type Legalize =
    fn(ir::Inst, &mut ir::Function, &mut flowgraph::ControlFlowGraph, &dyn TargetIsa) -> bool;

/// This struct provides information that a frontend may need to know about a target to
/// produce Cranelift IR for the target.
#[derive(Clone, Copy, Hash)]
pub struct TargetFrontendConfig {
    /// The default calling convention of the target.
    pub default_call_conv: CallConv,

    /// The pointer width of the target.
    pub pointer_width: PointerWidth,
}

impl TargetFrontendConfig {
    /// Get the pointer type of this target.
    pub fn pointer_type(self) -> ir::Type {
        ir::Type::int(u16::from(self.pointer_bits())).unwrap()
    }

    /// Get the width of pointers on this target, in units of bits.
    pub fn pointer_bits(self) -> u8 {
        self.pointer_width.bits()
    }

    /// Get the width of pointers on this target, in units of bytes.
    pub fn pointer_bytes(self) -> u8 {
        self.pointer_width.bytes()
    }
}

/// Methods that are specialized to a target ISA. Implies a Display trait that shows the
/// shared flags, as well as any isa-specific flags.
pub trait TargetIsa: fmt::Display + Send + Sync {
    /// Get the name of this ISA.
    fn name(&self) -> &'static str;

    /// Get the target triple that was used to make this trait object.
    fn triple(&self) -> &Triple;

    /// Get the ISA-independent flags that were used to make this trait object.
    fn flags(&self) -> &settings::Flags;

    /// Get the ISA-dependent flag values that were used to make this trait object.
    fn isa_flags(&self) -> Vec<settings::Value>;

    /// Hashes all flags, both ISA-independent and ISA-specific, into the
    /// specified hasher.
    fn hash_all_flags(&self, hasher: &mut dyn Hasher);

    /// Get the default calling convention of this target.
    fn default_call_conv(&self) -> CallConv {
        CallConv::triple_default(self.triple())
    }

    /// Get the endianness of this ISA.
    fn endianness(&self) -> ir::Endianness {
        match self.triple().endianness().unwrap() {
            target_lexicon::Endianness::Little => ir::Endianness::Little,
            target_lexicon::Endianness::Big => ir::Endianness::Big,
        }
    }

    /// Get the pointer type of this ISA.
    fn pointer_type(&self) -> ir::Type {
        ir::Type::int(u16::from(self.pointer_bits())).unwrap()
    }

    /// Get the width of pointers on this ISA.
    fn pointer_width(&self) -> PointerWidth {
        self.triple().pointer_width().unwrap()
    }

    /// Get the width of pointers on this ISA, in units of bits.
    fn pointer_bits(&self) -> u8 {
        self.pointer_width().bits()
    }

    /// Get the width of pointers on this ISA, in units of bytes.
    fn pointer_bytes(&self) -> u8 {
        self.pointer_width().bytes()
    }

    /// Get the information needed by frontends producing Cranelift IR.
    fn frontend_config(&self) -> TargetFrontendConfig {
        TargetFrontendConfig {
            default_call_conv: self.default_call_conv(),
            pointer_width: self.pointer_width(),
        }
    }

    /// Does the CPU implement scalar comparisons using a CPU flags register?
    fn uses_cpu_flags(&self) -> bool {
        false
    }

    /// Does the CPU implement multi-register addressing?
    fn uses_complex_addresses(&self) -> bool {
        false
    }

    /// Get a data structure describing the registers in this ISA.
    fn register_info(&self) -> RegInfo;

    #[cfg(feature = "unwind")]
    /// Map a Cranelift register to its corresponding DWARF register.
    fn map_dwarf_register(&self, _: RegUnit) -> Result<u16, RegisterMappingError> {
        Err(RegisterMappingError::UnsupportedArchitecture)
    }

    #[cfg(feature = "unwind")]
    /// Map a regalloc::Reg to its corresponding DWARF register.
    fn map_regalloc_reg_to_dwarf(&self, _: ::regalloc::Reg) -> Result<u16, RegisterMappingError> {
        Err(RegisterMappingError::UnsupportedArchitecture)
    }

    /// Returns an iterator over legal encodings for the instruction.
    fn legal_encodings<'a>(
        &'a self,
        func: &'a ir::Function,
        inst: &'a ir::InstructionData,
        ctrl_typevar: ir::Type,
    ) -> Encodings<'a>;

    /// Encode an instruction after determining it is legal.
    ///
    /// If `inst` can legally be encoded in this ISA, produce the corresponding `Encoding` object.
    /// Otherwise, return `Legalize` action.
    ///
    /// This is also the main entry point for determining if an instruction is legal.
    fn encode(
        &self,
        func: &ir::Function,
        inst: &ir::InstructionData,
        ctrl_typevar: ir::Type,
    ) -> Result<Encoding, Legalize> {
        let mut iter = self.legal_encodings(func, inst, ctrl_typevar);
        iter.next().ok_or_else(|| iter.legalize())
    }

    /// Get a data structure describing the instruction encodings in this ISA.
    fn encoding_info(&self) -> EncInfo;

    /// Legalize a function signature.
    ///
    /// This is used to legalize both the signature of the function being compiled and any called
    /// functions. The signature should be modified by adding `ArgumentLoc` annotations to all
    /// arguments and return values.
    ///
    /// Arguments with types that are not supported by the ABI can be expanded into multiple
    /// arguments:
    ///
    /// - Integer types that are too large to fit in a register can be broken into multiple
    ///   arguments of a smaller integer type.
    /// - Floating point types can be bit-cast to an integer type of the same size, and possible
    ///   broken into smaller integer types.
    /// - Vector types can be bit-cast and broken down into smaller vectors or scalars.
    ///
    /// The legalizer will adapt argument and return values as necessary at all ABI boundaries.
    ///
    /// When this function is called to legalize the signature of the function currently being
    /// compiled, `current` is true. The legalized signature can then also contain special purpose
    /// arguments and return values such as:
    ///
    /// - A `link` argument representing the link registers on RISC architectures that don't push
    ///   the return address on the stack.
    /// - A `link` return value which will receive the value that was passed to the `link`
    ///   argument.
    /// - An `sret` argument can be added if one wasn't present already. This is necessary if the
    ///   signature returns more values than registers are available for returning values.
    /// - An `sret` return value can be added if the ABI requires a function to return its `sret`
    ///   argument in a register.
    ///
    /// Arguments and return values for the caller's frame pointer and other callee-saved registers
    /// should not be added by this function. These arguments are not added until after register
    /// allocation.
    fn legalize_signature(&self, sig: &mut Cow<ir::Signature>, current: bool);

    /// Get the register class that should be used to represent an ABI argument or return value of
    /// type `ty`. This should be the top-level register class that contains the argument
    /// registers.
    ///
    /// This function can assume that it will only be asked to provide register classes for types
    /// that `legalize_signature()` produces in `ArgumentLoc::Reg` entries.
    fn regclass_for_abi_type(&self, ty: ir::Type) -> RegClass;

    /// Get the set of allocatable registers that can be used when compiling `func`.
    ///
    /// This set excludes reserved registers like the stack pointer and other special-purpose
    /// registers.
    fn allocatable_registers(&self, func: &ir::Function) -> regalloc::RegisterSet;

    /// Compute the stack layout and insert prologue and epilogue code into `func`.
    ///
    /// Return an error if the stack frame is too large.
    fn prologue_epilogue(&self, func: &mut ir::Function) -> CodegenResult<()> {
        let _tt = timing::prologue_epilogue();
        // This default implementation is unlikely to be good enough.
        use crate::ir::stackslot::{StackOffset, StackSize};
        use crate::stack_layout::layout_stack;

        let word_size = StackSize::from(self.pointer_bytes());

        // Account for the SpiderMonkey standard prologue pushes.
        if func.signature.call_conv.extends_baldrdash() {
            let bytes = StackSize::from(self.flags().baldrdash_prologue_words()) * word_size;
            let mut ss = ir::StackSlotData::new(ir::StackSlotKind::IncomingArg, bytes);
            ss.offset = Some(-(bytes as StackOffset));
            func.stack_slots.push(ss);
        }

        let is_leaf = func.is_leaf();
        layout_stack(&mut func.stack_slots, is_leaf, word_size)?;
        Ok(())
    }

    /// Emit binary machine code for a single instruction into the `sink` trait object.
    ///
    /// Note that this will call `put*` methods on the `sink` trait object via its vtable which
    /// is not the fastest way of emitting code.
    ///
    /// This function is under the "testing_hooks" feature, and is only suitable for use by
    /// test harnesses. It increases code size, and is inefficient.
    #[cfg(feature = "testing_hooks")]
    fn emit_inst(
        &self,
        func: &ir::Function,
        inst: ir::Inst,
        divert: &mut regalloc::RegDiversions,
        sink: &mut dyn binemit::CodeSink,
    );

    /// Emit a whole function into memory.
    fn emit_function_to_memory(&self, func: &ir::Function, sink: &mut binemit::MemoryCodeSink);

    /// IntCC condition for Unsigned Addition Overflow (Carry).
    fn unsigned_add_overflow_condition(&self) -> ir::condcodes::IntCC;

    /// IntCC condition for Unsigned Subtraction Overflow (Borrow/Carry).
    fn unsigned_sub_overflow_condition(&self) -> ir::condcodes::IntCC;

    /// Returns the flavor of unwind information emitted for this target.
    fn unwind_info_kind(&self) -> UnwindInfoKind {
        match self.triple().operating_system {
            #[cfg(feature = "unwind")]
            OperatingSystem::Windows => UnwindInfoKind::Windows,
            #[cfg(feature = "unwind")]
            _ => UnwindInfoKind::SystemV,
            #[cfg(not(feature = "unwind"))]
            _ => UnwindInfoKind::None,
        }
    }

    /// Creates unwind information for the function.
    ///
    /// Returns `None` if there is no unwind information for the function.
    #[cfg(feature = "unwind")]
    fn create_unwind_info(
        &self,
        _func: &ir::Function,
    ) -> CodegenResult<Option<unwind::UnwindInfo>> {
        // By default, an ISA has no unwind information
        Ok(None)
    }

    /// Creates a new System V Common Information Entry for the ISA.
    ///
    /// Returns `None` if the ISA does not support System V unwind information.
    #[cfg(feature = "unwind")]
    fn create_systemv_cie(&self) -> Option<gimli::write::CommonInformationEntry> {
        // By default, an ISA cannot create a System V CIE
        None
    }

    /// Get the new-style MachBackend, if this is an adapter around one.
    fn get_mach_backend(&self) -> Option<&dyn MachBackend> {
        None
    }

    /// Return an [Any] reference for downcasting to the ISA-specific implementation of this trait
    /// with `isa.as_any().downcast_ref::<isa::foo::Isa>()`.
    fn as_any(&self) -> &dyn Any;
}

impl Debug for &dyn TargetIsa {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "TargetIsa {{ triple: {:?}, pointer_width: {:?}}}",
            self.triple(),
            self.pointer_width()
        )
    }
}
