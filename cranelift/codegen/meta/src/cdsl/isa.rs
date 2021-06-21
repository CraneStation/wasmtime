use std::collections::HashSet;
use std::iter::FromIterator;

use crate::cdsl::cpu_modes::CpuMode;
use crate::cdsl::instructions::{InstructionGroup, InstructionPredicateMap};
use crate::cdsl::recipes::Recipes;
use crate::cdsl::regs::IsaRegs;
use crate::cdsl::settings::SettingGroup;
use crate::cdsl::xform::{TransformGroupIndex, TransformGroups};

pub(crate) struct TargetIsa {
    pub name: &'static str,
    pub instructions: InstructionGroup,
    pub settings: SettingGroup,
    pub regs: IsaRegs,
    pub recipes: Recipes,
    pub cpu_modes: Vec<CpuMode>,
    pub encodings_predicates: InstructionPredicateMap,

    /// TransformGroupIndex are global to all the ISAs, while we want to have indices into the
    /// local array of transform groups that are directly used. We use this map to get this
    /// information.
    pub local_transform_groups: Vec<TransformGroupIndex>,
}

impl TargetIsa {
    pub fn new(
        name: &'static str,
        instructions: InstructionGroup,
        settings: SettingGroup,
        regs: IsaRegs,
        recipes: Recipes,
        cpu_modes: Vec<CpuMode>,
        encodings_predicates: InstructionPredicateMap,
    ) -> Self {
        // Compute the local TransformGroup index.
        let mut local_transform_groups = Vec::new();
        for cpu_mode in &cpu_modes {
            let transform_groups = cpu_mode.direct_transform_groups();
            for group_index in transform_groups {
                // find() is fine here: the number of transform group is < 5 as of June 2019.
                if local_transform_groups
                    .iter()
                    .find(|&val| group_index == *val)
                    .is_none()
                {
                    local_transform_groups.push(group_index);
                }
            }
        }

        Self {
            name,
            instructions,
            settings,
            regs,
            recipes,
            cpu_modes,
            encodings_predicates,
            local_transform_groups,
        }
    }

    /// Returns a deterministically ordered, deduplicated list of TransformGroupIndex for the
    /// transitive set of TransformGroup this TargetIsa uses.
    pub fn transitive_transform_groups(
        &self,
        all_groups: &TransformGroups,
    ) -> Vec<TransformGroupIndex> {
        let mut set = HashSet::new();

        for &root in self.local_transform_groups.iter() {
            set.insert(root);
            let mut base = root;
            // Follow the chain of chain_with.
            while let Some(chain_with) = &all_groups.get(base).chain_with {
                set.insert(*chain_with);
                base = *chain_with;
            }
        }

        let mut vec = Vec::from_iter(set);
        vec.sort();
        vec
    }

    /// Returns a deterministically ordered, deduplicated list of TransformGroupIndex for the directly
    /// reachable set of TransformGroup this TargetIsa uses.
    pub fn direct_transform_groups(&self) -> &Vec<TransformGroupIndex> {
        &self.local_transform_groups
    }
}
