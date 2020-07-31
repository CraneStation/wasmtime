//! Object file generation.

use super::trampoline::build_trampoline;
use anyhow::Error;
use cranelift_frontend::FunctionBuilderContext;
use serde::{Deserialize, Serialize};
use wasmtime_debug::DwarfSection;
use wasmtime_environ::entity::{EntityRef, PrimaryMap};
use wasmtime_environ::isa::{unwind::UnwindInfo, TargetIsa};
use wasmtime_environ::wasm::{FuncIndex, SignatureIndex};
use wasmtime_environ::{Compilation, Module, Relocations};
use wasmtime_obj::{convert_object_elf_to_loadable_file, ObjectBuilder, ObjectBuilderTarget};

pub use wasmtime_obj::utils;

/// Unwind information for object files functions (including trampolines).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ObjectUnwindInfo {
    Func(FuncIndex, UnwindInfo),
    Trampoline(SignatureIndex, UnwindInfo),
}

// Builds ELF image from the module `Compilation`.
pub(crate) fn build_object(
    isa: &dyn TargetIsa,
    module: &Module,
    compilation: Compilation,
    relocations: Relocations,
    dwarf_sections: Vec<DwarfSection>,
) -> Result<(Vec<u8>, Vec<ObjectUnwindInfo>), Error> {
    const CODE_SECTION_ALIGNMENT: u64 = 0x1000;
    assert_eq!(
        isa.triple().architecture.endianness(),
        Ok(target_lexicon::Endianness::Little)
    );

    let mut unwind_info = Vec::new();

    // Preserve function unwind info.
    unwind_info.extend(
        compilation
            .into_iter()
            .enumerate()
            .filter_map(|(index, func)| {
                func.unwind_info.as_ref().map(|info| {
                    ObjectUnwindInfo::Func(
                        FuncIndex::new(module.local.num_imported_funcs + index),
                        info.clone(),
                    )
                })
            }),
    );

    let mut trampolines = PrimaryMap::with_capacity(module.local.signatures.len());
    let mut cx = FunctionBuilderContext::new();
    // Build trampolines for every signature.
    for (i, (_, native_sig)) in module.local.signatures.iter() {
        let (func, relocs) =
            build_trampoline(isa, &mut cx, native_sig, std::mem::size_of::<u128>())?;
        // Preserve trampoline function unwind info.
        if let Some(info) = &func.unwind_info {
            unwind_info.push(ObjectUnwindInfo::Trampoline(i, info.clone()))
        }
        trampolines.push((func, relocs));
    }

    let target = ObjectBuilderTarget::new(isa.triple().architecture)?;
    let mut builder = ObjectBuilder::new(target, module);
    builder
        .set_code_alignment(CODE_SECTION_ALIGNMENT)
        .set_compilation(compilation, relocations)
        .set_trampolines(trampolines)
        .set_dwarf_sections(dwarf_sections);
    let obj = builder.build()?;

    let mut bytes = obj.write()?;

    convert_object_elf_to_loadable_file(&mut bytes);

    Ok((bytes, unwind_info))
}
