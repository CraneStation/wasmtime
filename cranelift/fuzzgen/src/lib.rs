use crate::config::Config;
use crate::function_generator::FunctionGenerator;
use anyhow::Result;
use arbitrary::{Arbitrary, Unstructured};
use cranelift::codegen::data_value::DataValue;
use cranelift::codegen::ir::types::*;
use cranelift::codegen::ir::Function;
use cranelift::prelude::*;

mod config;
mod function_generator;

pub type TestCaseInput = Vec<DataValue>;

#[derive(Debug)]
pub struct TestCase {
    pub func: Function,
    /// Generate multiple test inputs for each test case.
    /// This allows us to get more coverage per compilation, which may be somewhat expensive.
    pub inputs: Vec<TestCaseInput>,
}

impl<'a> Arbitrary<'a> for TestCase {
    fn arbitrary(u: &mut Unstructured<'a>) -> arbitrary::Result<Self> {
        FuzzGen::new(u)
            .generate_test()
            .map_err(|_| arbitrary::Error::IncorrectFormat)
    }
}

pub struct FuzzGen<'r, 'data>
where
    'data: 'r,
{
    u: &'r mut Unstructured<'data>,
    config: Config,
}

impl<'r, 'data> FuzzGen<'r, 'data>
where
    'data: 'r,
{
    pub fn new(u: &'r mut Unstructured<'data>) -> Self {
        Self {
            u,
            config: Config::default(),
        }
    }

    fn generate_datavalue(&mut self, ty: Type) -> Result<DataValue> {
        Ok(match ty {
            ty if ty.is_int() => {
                let imm64 = match ty {
                    I8 => self.u.arbitrary::<i8>()? as i64,
                    I16 => self.u.arbitrary::<i16>()? as i64,
                    I32 => self.u.arbitrary::<i32>()? as i64,
                    I64 => self.u.arbitrary::<i64>()?,
                    _ => unreachable!(),
                };
                DataValue::from_integer(imm64, ty)?
            }
            ty if ty.is_bool() => DataValue::B(bool::arbitrary(self.u)?),
            _ => unimplemented!(),
        })
    }

    fn generate_test_inputs(&mut self, signature: &Signature) -> Result<Vec<TestCaseInput>> {
        let num_tests = self.u.int_in_range(self.config.test_case_inputs.clone())?;
        let mut inputs = Vec::with_capacity(num_tests);

        for _ in 0..num_tests {
            let test_args = signature
                .params
                .iter()
                .map(|p| self.generate_datavalue(p.value_type))
                .collect::<Result<TestCaseInput>>()?;

            inputs.push(test_args);
        }

        Ok(inputs)
    }

    pub fn generate_test(mut self) -> Result<TestCase> {
        let func = FunctionGenerator::new(&mut self.u, &self.config).generate()?;
        let inputs = self.generate_test_inputs(&func.signature)?;

        Ok(TestCase { func, inputs })
    }
}
