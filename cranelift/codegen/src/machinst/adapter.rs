//! Adapter for a `MachBackend` to implement the `TargetIsa` trait.

use crate::binemit;
use crate::ir;
use crate::isa::{EncInfo, Encoding, Encodings, Legalize, RegClass, RegInfo, TargetIsa};
use crate::machinst::*;
use crate::regalloc::RegisterSet;
use crate::settings::{self, Flags};

#[cfg(feature = "testing_hooks")]
use crate::regalloc::RegDiversions;

#[cfg(feature = "unwind")]
use crate::isa::unwind::systemv::RegisterMappingError;

use core::any::Any;
use std::borrow::Cow;
use std::fmt;
use target_lexicon::Triple;

/// A wrapper around a `MachBackend` that provides a `TargetIsa` impl.
pub struct TargetIsaAdapter {
    backend: Box<dyn MachBackend + Send + Sync + 'static>,
    triple: Triple,
}

impl TargetIsaAdapter {
    /// Create a new `TargetIsa` wrapper around a `MachBackend`.
    pub fn new<B: MachBackend + Send + Sync + 'static>(backend: B) -> TargetIsaAdapter {
        let triple = backend.triple();
        TargetIsaAdapter {
            backend: Box::new(backend),
            triple,
        }
    }
}

impl fmt::Display for TargetIsaAdapter {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("MachBackend")
            .field("name", &self.backend.name())
            .field("triple", &self.backend.triple())
            .field("flags", &format!("{}", self.backend.flags()))
            .finish()
    }
}

impl TargetIsa for TargetIsaAdapter {
    fn name(&self) -> &'static str {
        self.backend.name()
    }

    fn triple(&self) -> &Triple {
        &self.triple
    }

    fn flags(&self) -> &Flags {
        self.backend.flags()
    }

    fn isa_flags(&self) -> Vec<settings::Value> {
        self.backend.isa_flags()
    }

    fn hash_all_flags(&self, hasher: &mut dyn Hasher) {
        self.backend.hash_all_flags(hasher);
    }

    fn register_info(&self) -> RegInfo {
        // Called from function's Display impl, so we need a stub here.
        RegInfo {
            banks: &[],
            classes: &[],
        }
    }

    fn legal_encodings<'a>(
        &'a self,
        _func: &'a ir::Function,
        _inst: &'a ir::InstructionData,
        _ctrl_typevar: ir::Type,
    ) -> Encodings<'a> {
        panic!("Should not be called when new-style backend is available!")
    }

    fn encode(
        &self,
        _func: &ir::Function,
        _inst: &ir::InstructionData,
        _ctrl_typevar: ir::Type,
    ) -> Result<Encoding, Legalize> {
        panic!("Should not be called when new-style backend is available!")
    }

    fn encoding_info(&self) -> EncInfo {
        panic!("Should not be called when new-style backend is available!")
    }

    fn legalize_signature(&self, _sig: &mut Cow<ir::Signature>, _current: bool) {
        panic!("Should not be called when new-style backend is available!")
    }

    fn regclass_for_abi_type(&self, _ty: ir::Type) -> RegClass {
        panic!("Should not be called when new-style backend is available!")
    }

    fn allocatable_registers(&self, _func: &ir::Function) -> RegisterSet {
        panic!("Should not be called when new-style backend is available!")
    }

    fn prologue_epilogue(&self, _func: &mut ir::Function) -> CodegenResult<()> {
        panic!("Should not be called when new-style backend is available!")
    }

    #[cfg(feature = "testing_hooks")]
    fn emit_inst(
        &self,
        _func: &ir::Function,
        _inst: ir::Inst,
        _divert: &mut RegDiversions,
        _sink: &mut dyn binemit::CodeSink,
    ) {
        panic!("Should not be called when new-style backend is available!")
    }

    /// Emit a whole function into memory.
    fn emit_function_to_memory(&self, _func: &ir::Function, _sink: &mut binemit::MemoryCodeSink) {
        panic!("Should not be called when new-style backend is available!")
    }

    fn get_mach_backend(&self) -> Option<&dyn MachBackend> {
        Some(&*self.backend)
    }

    fn unsigned_add_overflow_condition(&self) -> ir::condcodes::IntCC {
        self.backend.unsigned_add_overflow_condition()
    }

    fn unsigned_sub_overflow_condition(&self) -> ir::condcodes::IntCC {
        self.backend.unsigned_sub_overflow_condition()
    }

    #[cfg(feature = "unwind")]
    fn create_systemv_cie(&self) -> Option<gimli::write::CommonInformationEntry> {
        self.backend.create_systemv_cie()
    }

    #[cfg(feature = "unwind")]
    fn map_regalloc_reg_to_dwarf(&self, r: Reg) -> Result<u16, RegisterMappingError> {
        self.backend.map_reg_to_dwarf(r)
    }

    fn as_any(&self) -> &dyn Any {
        self as &dyn Any
    }
}
