use std::collections::{hash_map, HashMap, HashSet};
use std::iter::FromIterator;

use crate::cdsl::encodings::Encoding;
use crate::cdsl::types::ValueType;
use crate::cdsl::xform::TransformGroupIndex;

pub(crate) struct CpuMode {
    pub name: &'static str,
    default_legalize: Option<TransformGroupIndex>,
    monomorphic_legalize: Option<TransformGroupIndex>,
    typed_legalize: HashMap<ValueType, TransformGroupIndex>,
    pub encodings: Vec<Encoding>,
}

impl CpuMode {
    pub fn get_default_legalize_code(&self) -> TransformGroupIndex {
        self.default_legalize
            .expect("a finished CpuMode must have a default legalize code")
    }
    pub fn get_legalize_code_for(&self, typ: &Option<ValueType>) -> TransformGroupIndex {
        match typ {
            Some(typ) => self
                .typed_legalize
                .get(typ)
                .copied()
                .unwrap_or_else(|| self.get_default_legalize_code()),
            None => self
                .monomorphic_legalize
                .unwrap_or_else(|| self.get_default_legalize_code()),
        }
    }
    pub fn get_legalized_types(&self) -> hash_map::Keys<ValueType, TransformGroupIndex> {
        self.typed_legalize.keys()
    }

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
