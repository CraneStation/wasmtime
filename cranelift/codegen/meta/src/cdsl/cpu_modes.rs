use std::collections::{HashMap, HashSet};
use std::iter::FromIterator;

use crate::cdsl::types::ValueType;
use crate::cdsl::xform::TransformGroupIndex;

pub(crate) struct CpuMode {
    pub name: &'static str,
    default_legalize: Option<TransformGroupIndex>,
    monomorphic_legalize: Option<TransformGroupIndex>,
    typed_legalize: HashMap<ValueType, TransformGroupIndex>,
}

impl CpuMode {
    /// Returns a deterministically ordered, deduplicated list of TransformGroupIndex for the directly
    /// reachable set of TransformGroup this TargetIsa uses.
    pub fn direct_transform_groups(&self) -> Vec<TransformGroupIndex> {
        let mut set = HashSet::new();
        if let Some(i) = &self.default_legalize {
            set.insert(*i);
        }
        if let Some(i) = &self.monomorphic_legalize {
            set.insert(*i);
        }
        set.extend(self.typed_legalize.values().cloned());
        let mut ret = Vec::from_iter(set);
        ret.sort();
        ret
    }
}
