#![allow(missing_docs)]

use cranelift_codegen;

pub fn builder() -> cranelift_codegen::isa::Builder {
    cranelift_native::builder().expect("host machine is not a supported target")
}

pub fn builder_without_flags() -> cranelift_codegen::isa::Builder {
    cranelift_native::builder_with_options(false).expect("host machine is not a supported target")
}

pub use cranelift_codegen::isa::lookup;
