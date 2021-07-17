//! Test command for testing the postopt pass.
//!
//! The resulting function is sent to `filecheck`.

use crate::subtest::{run_filecheck, Context, SubTest};
use cranelift_codegen;
use cranelift_codegen::ir::Function;
use cranelift_reader::TestCommand;
use std::borrow::Cow;

struct TestPostopt;

pub fn subtest(parsed: &TestCommand) -> anyhow::Result<Box<dyn SubTest>> {
    assert_eq!(parsed.command, "postopt");
    if !parsed.options.is_empty() {
        anyhow::bail!("No options allowed on {}", parsed);
    }
    Ok(Box::new(TestPostopt))
}

impl SubTest for TestPostopt {
    fn name(&self) -> &'static str {
        "postopt"
    }

    fn needs_isa(&self) -> bool {
        true
    }

    fn is_mutating(&self) -> bool {
        true
    }

    fn run(&self, func: Cow<Function>, context: &Context) -> anyhow::Result<()> {
        let mut comp_ctx = cranelift_codegen::Context::for_function(func.into_owned());
        let isa = context.isa.expect("postopt needs an ISA");

        comp_ctx.flowgraph();
        comp_ctx
            .postopt(isa)
            .map_err(|e| crate::pretty_anyhow_error(&comp_ctx.func, context.isa, Into::into(e)))?;

        let text = comp_ctx.func.display(isa).to_string();
        run_filecheck(&text, context)
    }
}
