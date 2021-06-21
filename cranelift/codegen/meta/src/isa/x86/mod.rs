use crate::cdsl::isa::TargetIsa;

use crate::shared::Definitions as SharedDefinitions;

pub(crate) mod settings;

pub(crate) fn define(shared_defs: &mut SharedDefinitions) -> TargetIsa {
    let settings = settings::define(&shared_defs.settings);

    TargetIsa::new("x86", settings)
}
