use crate::cdsl::instructions::{InstructionGroupBuilder, InstructionPredicateMap};
use crate::cdsl::isa::TargetIsa;
use crate::cdsl::recipes::Recipes;
use crate::cdsl::regs::IsaRegsBuilder;

use crate::shared::Definitions as SharedDefinitions;

pub(crate) mod settings;

pub(crate) fn define(shared_defs: &mut SharedDefinitions) -> TargetIsa {
    let settings = settings::define(&shared_defs.settings);

    let inst_group = InstructionGroupBuilder::new(&mut shared_defs.all_instructions).build();

    let cpu_modes = vec![];

    TargetIsa::new(
        "x86",
        inst_group,
        settings,
        IsaRegsBuilder::new().build(),
        Recipes::new(),
        cpu_modes,
        InstructionPredicateMap::new(),
    )
}
