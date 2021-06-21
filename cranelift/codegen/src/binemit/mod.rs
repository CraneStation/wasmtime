//! Binary machine code emission.
//!
//! The `binemit` module contains code for translating Cranelift's intermediate representation into
//! binary machine code.

mod memorysink;
mod relaxation;
mod shrink;
mod stack_map;

pub use self::memorysink::{
    MemoryCodeSink, NullRelocSink, NullStackMapSink, NullTrapSink, RelocSink, StackMapSink,
    TrapSink,
};
pub use self::relaxation::relax_branches;
pub use self::shrink::shrink_instructions;
pub use self::stack_map::StackMap;
use crate::ir::entities::Value;
use crate::ir::{
    ConstantOffset, ExternalName, Function, Inst, JumpTable, Opcode, SourceLoc, TrapCode,
};
use crate::isa::TargetIsa;
pub use crate::regalloc::RegDiversions;
use core::fmt;
#[cfg(feature = "enable-serde")]
use serde::{Deserialize, Serialize};

/// Offset in bytes from the beginning of the function.
///
/// Cranelift can be used as a cross compiler, so we don't want to use a type like `usize` which
/// depends on the *host* platform, not the *target* platform.
pub type CodeOffset = u32;

/// Addend to add to the symbol value.
pub type Addend = i64;

/// Relocation kinds for every ISA
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "enable-serde", derive(Serialize, Deserialize))]
pub enum Reloc {
    /// absolute 4-byte
    Abs4,
    /// absolute 8-byte
    Abs8,
    /// x86 PC-relative 4-byte
    X86PCRel4,
    /// x86 PC-relative 4-byte offset to trailing rodata
    X86PCRelRodata4,
    /// x86 call to PC-relative 4-byte
    X86CallPCRel4,
    /// x86 call to PLT-relative 4-byte
    X86CallPLTRel4,
    /// x86 GOT PC-relative 4-byte
    X86GOTPCRel4,
    /// Arm32 call target
    Arm32Call,
    /// Arm64 call target. Encoded as bottom 26 bits of instruction. This
    /// value is sign-extended, multiplied by 4, and added to the PC of
    /// the call instruction to form the destination address.
    Arm64Call,
    /// s390x PC-relative 4-byte offset
    S390xPCRel32Dbl,

    /// Elf x86_64 32 bit signed PC relative offset to two GOT entries for GD symbol.
    ElfX86_64TlsGd,

    /// Mach-O x86_64 32 bit signed PC relative offset to a `__thread_vars` entry.
    MachOX86_64Tlv,
}

impl fmt::Display for Reloc {
    /// Display trait implementation drops the arch, since its used in contexts where the arch is
    /// already unambiguous, e.g. clif syntax with isa specified. In other contexts, use Debug.
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            Self::Abs4 => write!(f, "Abs4"),
            Self::Abs8 => write!(f, "Abs8"),
            Self::S390xPCRel32Dbl => write!(f, "PCRel32Dbl"),
            Self::X86PCRel4 => write!(f, "PCRel4"),
            Self::X86PCRelRodata4 => write!(f, "PCRelRodata4"),
            Self::X86CallPCRel4 => write!(f, "CallPCRel4"),
            Self::X86CallPLTRel4 => write!(f, "CallPLTRel4"),
            Self::X86GOTPCRel4 => write!(f, "GOTPCRel4"),
            Self::Arm32Call | Self::Arm64Call => write!(f, "Call"),

            Self::ElfX86_64TlsGd => write!(f, "ElfX86_64TlsGd"),
            Self::MachOX86_64Tlv => write!(f, "MachOX86_64Tlv"),
        }
    }
}

/// Container for information about a vector of compiled code and its supporting read-only data.
///
/// The code starts at offset 0 and is followed optionally by relocatable jump tables and copyable
/// (raw binary) read-only data.  Any padding between sections is always part of the section that
/// precedes the boundary between the sections.
#[derive(PartialEq)]
pub struct CodeInfo {
    /// Number of bytes of machine code (the code starts at offset 0).
    pub code_size: CodeOffset,

    /// Number of bytes of jumptables.
    pub jumptables_size: CodeOffset,

    /// Number of bytes of rodata.
    pub rodata_size: CodeOffset,

    /// Number of bytes in total.
    pub total_size: CodeOffset,
}

impl CodeInfo {
    /// Offset of any relocatable jump tables, or equal to rodata if there are no jump tables.
    pub fn jumptables(&self) -> CodeOffset {
        self.code_size
    }

    /// Offset of any copyable read-only data, or equal to total_size if there are no rodata.
    pub fn rodata(&self) -> CodeOffset {
        self.code_size + self.jumptables_size
    }
}

/// Abstract interface for adding bytes to the code segment.
///
/// A `CodeSink` will receive all of the machine code for a function. It also accepts relocations
/// which are locations in the code section that need to be fixed up when linking.
pub trait CodeSink {
    /// Get the current position.
    fn offset(&self) -> CodeOffset;

    /// Add 1 byte to the code section.
    fn put1(&mut self, _: u8);

    /// Add 2 bytes to the code section.
    fn put2(&mut self, _: u16);

    /// Add 4 bytes to the code section.
    fn put4(&mut self, _: u32);

    /// Add 8 bytes to the code section.
    fn put8(&mut self, _: u64);

    /// Add a relocation referencing an external symbol plus the addend at the current offset.
    fn reloc_external(&mut self, _: SourceLoc, _: Reloc, _: &ExternalName, _: Addend);

    /// Add a relocation referencing a constant.
    fn reloc_constant(&mut self, _: Reloc, _: ConstantOffset);

    /// Add a relocation referencing a jump table.
    fn reloc_jt(&mut self, _: Reloc, _: JumpTable);

    /// Add trap information for the current offset.
    fn trap(&mut self, _: TrapCode, _: SourceLoc);

    /// Machine code output is complete, jump table data may follow.
    fn begin_jumptables(&mut self);

    /// Jump table output is complete, raw read-only data may follow.
    fn begin_rodata(&mut self);

    /// Read-only data output is complete, we're done.
    fn end_codegen(&mut self);

    /// Add a stack map at the current code offset.
    fn add_stack_map(&mut self, _: &[Value], _: &Function, _: &dyn TargetIsa);

    /// Add a call site for a call with the given opcode, returning at the current offset.
    fn add_call_site(&mut self, _: Opcode, _: SourceLoc) {
        // Default implementation doesn't need to do anything.
    }
}

/// Report a bad encoding error.
#[cold]
pub fn bad_encoding(func: &Function, inst: Inst) -> ! {
    panic!(
        "Bad encoding {} for {}",
        func.encodings[inst],
        func.dfg.display_inst(inst, None)
    );
}

/// Emit a function to `sink`, given an instruction emitter function.
///
/// This function is called from the `TargetIsa::emit_function()` implementations with the
/// appropriate instruction emitter.
pub fn emit_function<CS, EI>(func: &Function, emit_inst: EI, sink: &mut CS, isa: &dyn TargetIsa)
where
    CS: CodeSink,
    EI: Fn(&Function, Inst, &mut RegDiversions, &mut CS, &dyn TargetIsa),
{
    let mut divert = RegDiversions::new();
    for block in func.layout.blocks() {
        divert.at_block(&func.entry_diversions, block);
        debug_assert_eq!(func.offsets[block], sink.offset());
        for inst in func.layout.block_insts(block) {
            emit_inst(func, inst, &mut divert, sink, isa);
        }
    }

    sink.begin_jumptables();

    // Output jump tables.
    for (jt, jt_data) in func.jump_tables.iter() {
        let jt_offset = func.jt_offsets[jt];
        for block in jt_data.iter() {
            let rel_offset: i32 = func.offsets[*block] as i32 - jt_offset as i32;
            sink.put4(rel_offset as u32)
        }
    }

    sink.begin_rodata();

    // Output constants.
    for (_, constant_data) in func.dfg.constants.iter() {
        for byte in constant_data.iter() {
            sink.put1(*byte)
        }
    }

    sink.end_codegen();
}
