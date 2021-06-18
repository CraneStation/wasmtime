//! This crate generates Rust sources for use by
//! [`cranelift_codegen`](../cranelift_codegen/index.html).
#[macro_use]
mod cdsl;
mod srcgen;

pub mod error;
pub mod isa;

mod gen_binemit;
mod gen_encodings;
mod gen_inst;
mod gen_legalizer;
mod gen_registers;
mod gen_settings;
mod gen_types;

mod default_map;
mod shared;
mod unique_table;

/// Generate an ISA from an architecture string (e.g. "x86_64").
pub fn isa_from_arch(arch: &str) -> Result<isa::Isa, String> {
    isa::Isa::from_arch(arch).ok_or_else(|| format!("no supported isa found for arch `{}`", arch))
}

/// Generates all the Rust source files used in Cranelift from the meta-language.
pub fn generate(
    old_backend_isas: &[isa::Isa],
    new_backend_isas: &[isa::Isa],
    out_dir: &str,
) -> Result<(), error::Error> {
    // Create all the definitions:
    // - common definitions.
    let mut shared_defs = shared::define();

    gen_settings::generate(
        &shared_defs.settings,
        gen_settings::ParentGroup::None,
        "settings.rs",
        &out_dir,
    )?;
    gen_types::generate("types.rs", &out_dir)?;

    // - per ISA definitions.
    let target_isas = isa::define(old_backend_isas, &mut shared_defs);

    // At this point, all definitions are done.
    let all_formats = shared_defs.verify_instruction_formats();

    // Generate all the code.
    gen_inst::generate(
        all_formats,
        &shared_defs.all_instructions,
        "opcodes.rs",
        "inst_builder.rs",
        &out_dir,
    )?;

    let extra_legalization_groups: &[&'static str] = if !new_backend_isas.is_empty() {
        // The new backend only requires the "expand" legalization group.
        &["expand"]
    } else {
        &[]
    };

    gen_legalizer::generate(
        &target_isas,
        &shared_defs.transform_groups,
        extra_legalization_groups,
        "legalize",
        &out_dir,
    )?;

    for isa in target_isas {
        gen_registers::generate(&isa, &format!("registers-{}.rs", isa.name), &out_dir)?;

        gen_settings::generate(
            &isa.settings,
            gen_settings::ParentGroup::Shared,
            &format!("settings-{}.rs", isa.name),
            &out_dir,
        )?;

        gen_encodings::generate(
            &shared_defs,
            &isa,
            &format!("encoding-{}.rs", isa.name),
            &out_dir,
        )?;

        gen_binemit::generate(
            &isa.name,
            &isa.recipes,
            &format!("binemit-{}.rs", isa.name),
            &out_dir,
        )?;
    }

    for isa in new_backend_isas {
        match isa {
            isa::Isa::X86 => {
                // If the old backend ISAs contained x86, this file has already been generated.
                if old_backend_isas.iter().any(|isa| *isa == isa::Isa::X86) {
                    continue;
                }

                let settings = crate::isa::x86::settings::define(&shared_defs.settings);
                gen_settings::generate(
                    &settings,
                    gen_settings::ParentGroup::Shared,
                    "settings-x86.rs",
                    &out_dir,
                )?;
            }
            isa::Isa::Arm64 => {
                // aarch64 doesn't have platform-specific settings.
            }
            isa::Isa::S390x => {
                // s390x doesn't have platform-specific settings.
            }
            isa::Isa::Arm32 => todo!(),
        }
    }

    Ok(())
}
