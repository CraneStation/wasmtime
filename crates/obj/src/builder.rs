//! Object file builder.
//!
//! Creates ELF image based on `Compilation` information. The ELF contains
//! functions and trampolines in the ".text" section. It also contains all
//! relocation records for linking stage. If DWARF sections exist, their
//! content will be written as well.
//!
//! The object file has symbols for each function and trampoline, as well as
//! symbols that refer libcalls.
//!
//! The function symbol names have format "_wasm_function_N", where N is
//! `FuncIndex`. The defined wasm function symbols refer to a JIT compiled
//! function body, the imported wasm function do not. The trampolines symbol
//! names have format "_trampoline_N", where N is `SignatureIndex`.

#![allow(missing_docs)]

use anyhow::bail;
use object::write::{
    Object, Relocation as ObjectRelocation, SectionId, StandardSegment, Symbol, SymbolId,
    SymbolSection,
};
use object::{
    elf, Architecture, BinaryFormat, Endianness, RelocationEncoding, RelocationKind, SectionKind,
    SymbolFlags, SymbolKind, SymbolScope,
};
use std::collections::HashMap;
use target_lexicon::Triple;
use wasmtime_debug::{DwarfSection, DwarfSectionRelocTarget};
use wasmtime_environ::entity::{EntityRef, PrimaryMap};
use wasmtime_environ::ir::{LibCall, Reloc};
use wasmtime_environ::isa::unwind::UnwindInfo;
use wasmtime_environ::wasm::{DefinedFuncIndex, FuncIndex, SignatureIndex};
use wasmtime_environ::{CompiledFunction, CompiledFunctions, Module, Relocation, RelocationTarget};

fn to_object_relocations<'a>(
    it: impl Iterator<Item = &'a Relocation> + 'a,
    off: u64,
    module: &'a Module,
    funcs: &'a PrimaryMap<FuncIndex, Option<SymbolId>>,
    libcalls: &'a HashMap<LibCall, SymbolId>,
    compiled_funcs: &'a CompiledFunctions,
) -> impl Iterator<Item = ObjectRelocation> + 'a {
    it.filter_map(move |r| {
        let (symbol, symbol_offset) = match r.reloc_target {
            RelocationTarget::UserFunc(index) => (funcs[index].unwrap(), 0),
            RelocationTarget::LibCall(call) => (libcalls[&call], 0),
            RelocationTarget::JumpTable(f, jt) => {
                let df = module.defined_func_index(f).unwrap();
                let offset = *compiled_funcs
                    .get(df)
                    .and_then(|f| f.jt_offsets.get(jt))
                    .expect("func jump table");
                (funcs[f].unwrap(), offset)
            }
        };
        let (kind, encoding, size) = match r.reloc {
            Reloc::Abs4 => (RelocationKind::Absolute, RelocationEncoding::Generic, 32),
            Reloc::Abs8 => (RelocationKind::Absolute, RelocationEncoding::Generic, 64),
            Reloc::X86PCRel4 => (RelocationKind::Relative, RelocationEncoding::Generic, 32),
            Reloc::X86CallPCRel4 => (RelocationKind::Relative, RelocationEncoding::X86Branch, 32),
            // TODO: Get Cranelift to tell us when we can use
            // R_X86_64_GOTPCRELX/R_X86_64_REX_GOTPCRELX.
            Reloc::X86CallPLTRel4 => (
                RelocationKind::PltRelative,
                RelocationEncoding::X86Branch,
                32,
            ),
            Reloc::X86GOTPCRel4 => (RelocationKind::GotRelative, RelocationEncoding::Generic, 32),
            Reloc::ElfX86_64TlsGd => (
                RelocationKind::Elf(elf::R_X86_64_TLSGD),
                RelocationEncoding::Generic,
                32,
            ),
            Reloc::X86PCRelRodata4 => {
                return None;
            }
            Reloc::Arm64Call => (
                RelocationKind::Elf(elf::R_AARCH64_CALL26),
                RelocationEncoding::Generic,
                32,
            ),
            other => unimplemented!("Unimplemented relocation {:?}", other),
        };
        Some(ObjectRelocation {
            offset: off + r.offset as u64,
            size,
            kind,
            encoding,
            symbol,
            addend: r.addend.wrapping_add(symbol_offset as i64),
        })
    })
}

fn to_object_architecture(
    arch: target_lexicon::Architecture,
) -> Result<Architecture, anyhow::Error> {
    use target_lexicon::Architecture::*;
    Ok(match arch {
        X86_32(_) => Architecture::I386,
        X86_64 => Architecture::X86_64,
        Arm(_) => Architecture::Arm,
        Aarch64(_) => Architecture::Aarch64,
        architecture => {
            anyhow::bail!("target architecture {:?} is unsupported", architecture,);
        }
    })
}

const TEXT_SECTION_NAME: &[u8] = b".text";

fn process_unwind_info(info: &UnwindInfo, obj: &mut Object, code_section: SectionId) {
    if let UnwindInfo::WindowsX64(info) = &info {
        // Windows prefers Unwind info after the code -- writing it here.
        let unwind_size = info.emit_size();
        let mut unwind_info = vec![0; unwind_size];
        info.emit(&mut unwind_info);
        let _off = obj.append_section_data(code_section, &unwind_info, 4);
    }
}

/// Builds ELF image from the module `Compilation`.
// const CODE_SECTION_ALIGNMENT: u64 = 0x1000;

/// Iterates through all `LibCall` members and all runtime exported functions.
#[macro_export]
macro_rules! for_each_libcall {
    ($op:ident) => {
        $op![
            (UdivI64, wasmtime_i64_udiv),
            (UdivI64, wasmtime_i64_udiv),
            (SdivI64, wasmtime_i64_sdiv),
            (UremI64, wasmtime_i64_urem),
            (SremI64, wasmtime_i64_srem),
            (IshlI64, wasmtime_i64_ishl),
            (UshrI64, wasmtime_i64_ushr),
            (SshrI64, wasmtime_i64_sshr),
            (CeilF32, wasmtime_f32_ceil),
            (FloorF32, wasmtime_f32_floor),
            (TruncF32, wasmtime_f32_trunc),
            (NearestF32, wasmtime_f32_nearest),
            (CeilF64, wasmtime_f64_ceil),
            (FloorF64, wasmtime_f64_floor),
            (TruncF64, wasmtime_f64_trunc),
            (NearestF64, wasmtime_f64_nearest)
        ];
    };
}

fn write_libcall_symbols(obj: &mut Object) -> HashMap<LibCall, SymbolId> {
    let mut libcalls = HashMap::new();
    macro_rules! add_libcall_symbol {
        [$(($libcall:ident, $export:ident)),*] => {{
            $(
                let symbol_id = obj.add_symbol(Symbol {
                    name: stringify!($export).as_bytes().to_vec(),
                    value: 0,
                    size: 0,
                    kind: SymbolKind::Text,
                    scope: SymbolScope::Linkage,
                    weak: true,
                    section: SymbolSection::Undefined,
                    flags: SymbolFlags::None,
                });
                libcalls.insert(LibCall::$libcall, symbol_id);
            )+
        }};
    }
    for_each_libcall!(add_libcall_symbol);

    libcalls
}

pub mod utils {
    use wasmtime_environ::entity::EntityRef;
    use wasmtime_environ::wasm::{FuncIndex, SignatureIndex};

    pub const FUNCTION_PREFIX: &str = "_wasm_function_";
    pub const TRAMPOLINE_PREFIX: &str = "_trampoline_";

    pub fn func_symbol_name(index: FuncIndex) -> String {
        format!("_wasm_function_{}", index.index())
    }

    pub fn try_parse_func_name(name: &str) -> Option<FuncIndex> {
        if !name.starts_with(FUNCTION_PREFIX) {
            return None;
        }
        name[FUNCTION_PREFIX.len()..]
            .parse()
            .ok()
            .map(FuncIndex::new)
    }

    pub fn trampoline_symbol_name(index: SignatureIndex) -> String {
        format!("_trampoline_{}", index.index())
    }

    pub fn try_parse_trampoline_name(name: &str) -> Option<SignatureIndex> {
        if !name.starts_with(TRAMPOLINE_PREFIX) {
            return None;
        }
        name[TRAMPOLINE_PREFIX.len()..]
            .parse()
            .ok()
            .map(SignatureIndex::new)
    }
}

pub struct ObjectBuilderTarget {
    pub(crate) binary_format: BinaryFormat,
    pub(crate) architecture: Architecture,
    pub(crate) endianness: Endianness,
}

impl ObjectBuilderTarget {
    pub fn new(arch: target_lexicon::Architecture) -> Result<Self, anyhow::Error> {
        Ok(Self {
            binary_format: BinaryFormat::Elf,
            architecture: to_object_architecture(arch)?,
            endianness: match arch.endianness().unwrap() {
                target_lexicon::Endianness::Little => object::Endianness::Little,
                target_lexicon::Endianness::Big => object::Endianness::Big,
            },
        })
    }

    pub fn from_triple(triple: &Triple) -> Result<Self, anyhow::Error> {
        let binary_format = match triple.binary_format {
            target_lexicon::BinaryFormat::Elf => object::BinaryFormat::Elf,
            target_lexicon::BinaryFormat::Coff => object::BinaryFormat::Coff,
            target_lexicon::BinaryFormat::Macho => object::BinaryFormat::MachO,
            target_lexicon::BinaryFormat::Wasm => {
                bail!("binary format wasm is unsupported");
            }
            target_lexicon::BinaryFormat::Unknown => {
                bail!("binary format is unknown");
            }
            other => bail!("binary format {} is unsupported", other),
        };
        let architecture = to_object_architecture(triple.architecture)?;
        let endianness = match triple.endianness().unwrap() {
            target_lexicon::Endianness::Little => object::Endianness::Little,
            target_lexicon::Endianness::Big => object::Endianness::Big,
        };
        Ok(Self {
            binary_format,
            architecture,
            endianness,
        })
    }
}

fn prepend_prefix(prefix: &[u8], name: &[u8]) -> Vec<u8> {
    let mut result = prefix.to_vec();
    result.extend(name);
    result
}

pub struct ObjectBuilder<'a> {
    target: ObjectBuilderTarget,
    module: &'a Module,
    code_alignment: u64,
    compilation: &'a CompiledFunctions,
    trampolines: Vec<(SignatureIndex, CompiledFunction)>,
    dwarf_sections: Vec<DwarfSection>,
    meta: Vec<u8>,
    prefix: Vec<u8>,
    use_debug_names: bool,
}

impl<'a> ObjectBuilder<'a> {
    pub fn new(
        target: ObjectBuilderTarget,
        module: &'a Module,
        compilation: &'a CompiledFunctions,
    ) -> Self {
        Self {
            target,
            module,
            code_alignment: 1,
            trampolines: Vec::new(),
            dwarf_sections: vec![],
            compilation,
            meta: vec![],
            prefix: vec![],
            use_debug_names: false,
        }
    }

    pub fn set_code_alignment(&mut self, code_alignment: u64) -> &mut Self {
        self.code_alignment = code_alignment;
        self
    }

    pub fn set_trampolines(
        &mut self,
        trampolines: Vec<(SignatureIndex, CompiledFunction)>,
    ) -> &mut Self {
        self.trampolines = trampolines;
        self
    }

    pub fn set_meta(&mut self, meta: &[u8]) -> &mut Self {
        self.meta = meta.to_vec();
        self
    }

    pub fn set_dwarf_sections(&mut self, dwarf_sections: Vec<DwarfSection>) -> &mut Self {
        self.dwarf_sections = dwarf_sections;
        self
    }

    pub fn set_prefix(&mut self, prefix: &str) -> &mut Self {
        self.prefix = prefix.as_bytes().to_vec();
        self
    }

    pub fn set_use_debug_names(&mut self, use_debug_names: bool) -> &mut Self {
        self.use_debug_names = use_debug_names;
        self
    }

    pub fn build(self) -> Result<Object, anyhow::Error> {
        let mut obj = Object::new(
            self.target.binary_format,
            self.target.architecture,
            self.target.endianness,
        );

        let module = self.module;

        // Entire code (functions and trampolines) will be placed
        // in the ".text" section.
        let section_id = obj.add_section(
            obj.segment_name(StandardSegment::Text).to_vec(),
            TEXT_SECTION_NAME.to_vec(),
            SectionKind::Text,
        );

        // Create symbols for imports -- needed during linking, or skip for COFF.
        let skip_imports = self.target.binary_format == BinaryFormat::Coff;
        let mut func_symbols = PrimaryMap::with_capacity(self.compilation.len());
        for index in 0..module.num_imported_funcs {
            if skip_imports {
                func_symbols.push(None);
            } else {
                let symbol_id = obj.add_symbol(Symbol {
                    name: prepend_prefix(
                        &self.prefix,
                        utils::func_symbol_name(FuncIndex::new(index)).as_bytes(),
                    ),
                    value: 0,
                    size: 0,
                    kind: SymbolKind::Text,
                    scope: SymbolScope::Linkage,
                    weak: false,
                    section: SymbolSection::Undefined,
                    flags: SymbolFlags::None,
                });
                func_symbols.push(Some(symbol_id));
            }
        }

        let mut append_func = |name: Vec<u8>, func: &CompiledFunction| {
            let off = obj.append_section_data(section_id, &func.body, 1);
            let symbol_id = obj.add_symbol(Symbol {
                name,
                value: off,
                size: func.body.len() as u64,
                kind: SymbolKind::Text,
                scope: SymbolScope::Linkage,
                weak: false,
                section: SymbolSection::Section(section_id),
                flags: SymbolFlags::None,
            });
            // Preserve function unwind info.
            if let Some(info) = &func.unwind_info {
                process_unwind_info(info, &mut obj, section_id);
            }
            symbol_id
        };

        // Create symbols and section data for the compiled functions.
        for (index, func) in self.compilation.iter() {
            let index = module.func_index(index);
            let maybe_debug_name = if let Some(name) = self.get_debug_name(index) {
                name
            } else {
                utils::func_symbol_name(index)
            };
            let name = prepend_prefix(&self.prefix, maybe_debug_name.as_bytes());
            let symbol_id = append_func(name, func);
            func_symbols.push(Some(symbol_id));
        }
        let mut trampolines = Vec::new();
        for (i, func) in self.trampolines.iter() {
            let name = prepend_prefix(&self.prefix, utils::trampoline_symbol_name(*i).as_bytes());
            trampolines.push(append_func(name, func));
        }

        obj.append_section_data(section_id, &[], self.code_alignment);

        self.write_metadata_and_func_references(&mut obj, &func_symbols, &trampolines)?;

        // If we have DWARF data, write it in the object file.
        let (debug_bodies, debug_relocs) = self
            .dwarf_sections
            .into_iter()
            .map(|s| ((s.name, s.body), (s.name, s.relocs)))
            .unzip::<_, _, Vec<_>, Vec<_>>();
        let mut dwarf_sections_ids = HashMap::new();
        for (name, body) in debug_bodies {
            let segment = obj.segment_name(StandardSegment::Debug).to_vec();
            let section_id = obj.add_section(segment, name.as_bytes().to_vec(), SectionKind::Debug);
            dwarf_sections_ids.insert(name.to_string(), section_id);
            obj.append_section_data(section_id, &body, 1);
        }

        let libcalls = write_libcall_symbols(&mut obj);

        // Write all functions relocations.
        for (index, func) in self.compilation.into_iter() {
            let func_index = module.func_index(index);
            let (_, off) = obj
                .symbol_section_and_offset(func_symbols[func_index].unwrap())
                .unwrap();
            for r in to_object_relocations(
                func.relocations.iter(),
                off,
                module,
                &func_symbols,
                &libcalls,
                &self.compilation,
            ) {
                obj.add_relocation(section_id, r)?;
            }
        }

        for ((_, func), symbol) in self.trampolines.iter().zip(trampolines) {
            let (_, off) = obj.symbol_section_and_offset(symbol).unwrap();
            for r in to_object_relocations(
                func.relocations.iter(),
                off,
                module,
                &func_symbols,
                &libcalls,
                &self.compilation,
            ) {
                obj.add_relocation(section_id, r)?;
            }
        }

        // Write all debug data relocations.
        for (name, relocs) in debug_relocs {
            let section_id = *dwarf_sections_ids.get(name).unwrap();
            for reloc in relocs {
                let target_symbol = match reloc.target {
                    DwarfSectionRelocTarget::Func(index) => {
                        func_symbols[module.func_index(DefinedFuncIndex::new(index))].unwrap()
                    }
                    DwarfSectionRelocTarget::Section(name) => {
                        obj.section_symbol(*dwarf_sections_ids.get(name).unwrap())
                    }
                };
                obj.add_relocation(
                    section_id,
                    ObjectRelocation {
                        offset: u64::from(reloc.offset),
                        size: reloc.size << 3,
                        kind: RelocationKind::Absolute,
                        encoding: RelocationEncoding::Generic,
                        symbol: target_symbol,
                        addend: i64::from(reloc.addend),
                    },
                )?;
            }
        }

        Ok(obj)
    }

    fn get_debug_name(&self, index: FuncIndex) -> Option<String> {
        if !self.use_debug_names || !self.module.func_names.contains_key(&index) {
            return None;
        }
        let name = &self.module.func_names[&index];
        if name.as_bytes().contains(&0) {
            // Ignore the names with 0-byte in them.
            return None;
        }
        Some(name.clone())
    }

    fn write_metadata_and_func_references(
        &self,
        obj: &mut Object,
        func_symbols: &PrimaryMap<FuncIndex, Option<SymbolId>>,
        trampolines: &[SymbolId],
    ) -> Result<(), anyhow::Error> {
        if self.meta.is_empty() {
            return Ok(());
        }

        let segment = obj
            .segment_name(object::write::StandardSegment::Data)
            .to_vec();
        let meta_section_id =
            obj.add_section(segment, b".wasm".to_vec(), object::SectionKind::Data);
        let name = prepend_prefix(&self.prefix, b"wasmtime_meta");
        let off =
            obj.append_section_data(meta_section_id, &(self.meta.len() as u64).to_le_bytes(), 1);
        let off_ = obj.append_section_data(meta_section_id, &self.meta, 1);
        let mut size = 8 + self.meta.len();
        assert!(off + 8 == off_);

        for (index, _) in self.compilation.into_iter() {
            let off_ = obj.append_section_data(meta_section_id, &[0; 8], 1);
            let func_index = self.module.func_index(index);
            let target_symbol = func_symbols[func_index].unwrap();
            obj.add_relocation(
                meta_section_id,
                ObjectRelocation {
                    offset: u64::from(off_),
                    size: 64,
                    kind: RelocationKind::Absolute,
                    encoding: RelocationEncoding::Generic,
                    symbol: target_symbol,
                    addend: 0,
                },
            )?;
            size += 8;
        }
        for &target_symbol in trampolines.iter() {
            let off_ = obj.append_section_data(meta_section_id, &[0; 8], 1);
            obj.add_relocation(
                meta_section_id,
                ObjectRelocation {
                    offset: u64::from(off_),
                    size: 64,
                    kind: RelocationKind::Absolute,
                    encoding: RelocationEncoding::Generic,
                    symbol: target_symbol,
                    addend: 0,
                },
            )?;
            size += 8;
        }

        obj.add_symbol(Symbol {
            name,
            value: off,
            size: size as u64,
            kind: SymbolKind::Text,
            scope: SymbolScope::Linkage,
            weak: false,
            section: SymbolSection::Section(meta_section_id),
            flags: SymbolFlags::None,
        });
        Ok(())
    }
}
