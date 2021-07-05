use std::ops::RangeInclusive;

/// Holds the range of acceptable values to use during the generation of testcases
pub struct Config {
    pub test_case_inputs: RangeInclusive<usize>,
    pub signature_params: RangeInclusive<usize>,
    pub signature_rets: RangeInclusive<usize>,
    pub instructions_per_block: RangeInclusive<usize>,
    /// Number of variables that we allocate per function
    /// This value does not include the signature params
    pub vars_per_function: RangeInclusive<usize>,
    /// Number of blocks that we generate per function.
    /// This value does not include the entry block
    pub blocks_per_function: RangeInclusive<usize>,
    /// Number of params a block should take
    /// This value does not apply to block0 which takes the function params
    /// and is thus governed by `signature_params`
    pub block_signature_params: RangeInclusive<usize>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            test_case_inputs: 1..=10,
            signature_params: 0..=16,
            signature_rets: 0..=16,
            instructions_per_block: 0..=64,
            vars_per_function: 0..=16,
            blocks_per_function: 0..=16,
            block_signature_params: 0..=16,
        }
    }
}
