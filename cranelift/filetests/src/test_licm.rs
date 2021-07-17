//! Test command for testing the LICM pass.
//!
//! The `licm` test command runs each function through the LICM pass after ensuring
//! that all instructions are legal for the target.
//!
//! The resulting function is sent to `filecheck`.

use crate::subtest::{run_filecheck, Context, SubTest};
use cranelift_codegen;
use cranelift_codegen::ir::Function;
use cranelift_reader::TestCommand;
use std::borrow::Cow;

struct TestLICM;

pub fn subtest(parsed: &TestCommand) -> anyhow::Result<Box<dyn SubTest>> {
    assert_eq!(parsed.command, "licm");
    if !parsed.options.is_empty() {
        anyhow::bail!("No options allowed on {}", parsed);
    }
    Ok(Box::new(TestLICM))
}

impl SubTest for TestLICM {
    fn name(&self) -> &'static str {
        "licm"
    }

    fn needs_isa(&self) -> bool {
        true
    }

    fn is_mutating(&self) -> bool {
        true
    }

    fn run(&self, func: Cow<Function>, context: &Context) -> anyhow::Result<()> {
        let isa = context.isa.expect("LICM needs an ISA");
        let mut comp_ctx = cranelift_codegen::Context::for_function(func.into_owned());

        comp_ctx.flowgraph();
        comp_ctx.compute_loop_analysis();
        comp_ctx
            .licm(isa)
            .map_err(|e| crate::pretty_anyhow_error(&comp_ctx.func, context.isa, Into::into(e)))?;

        let text = comp_ctx.func.display(context.isa).to_string();
        run_filecheck(&text, context)
    }
}
