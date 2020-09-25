#![allow(missing_docs)]

use crate::runtime::{Engine, EngineInner};
use std::sync::{Arc, Weak};
use wasmtime_jit::CompiledModule;

pub use wasmtime_runtime::debugger::{
    BreakpointData, DebuggerContext, DebuggerContextData, DebuggerPauseKind, DebuggerResumeAction,
    PatchableCode,
};

pub trait DebuggerJitCodeRegistration: std::marker::Send + std::marker::Sync {
    fn id(&self) -> u32;
}

pub trait DebuggerAgent: std::marker::Send + std::marker::Sync {
    fn pause(&mut self, kind: DebuggerPauseKind) -> DebuggerResumeAction;
    fn register_module(&mut self, module: DebuggerModule) -> Box<dyn DebuggerJitCodeRegistration>;
    fn add_breakpoints(&self, module_id: u32, addr: u64);
    fn find_breakpoint(&self, addr: usize) -> Option<*const BreakpointData>;
}

pub struct DebuggerModule<'a> {
    module: Weak<CompiledModule>,
    module_id: usize,
    engine: Weak<EngineInner>,
    bytes: &'a [u8],
}

impl<'a> DebuggerModule<'a> {
    pub(crate) fn new(
        module: &Arc<CompiledModule>,
        engine: Weak<EngineInner>,
        bytes: &'a [u8],
    ) -> Self {
        Self {
            module: Arc::downgrade(module),
            module_id: module.module().id,
            engine,
            bytes,
        }
    }
    pub fn bytes(&self) -> &[u8] {
        self.bytes
    }
    pub fn compiled_module(&self) -> Weak<CompiledModule> {
        self.module.clone()
    }
    pub fn engine(&self) -> Engine {
        self.engine.upgrade().unwrap().into()
    }
    fn module(&self) -> Arc<CompiledModule> {
        self.module.upgrade().unwrap()
    }
    pub fn module_id(&self) -> usize {
        self.module_id
    }
    pub fn ranges(&self) -> Vec<(usize, usize)> {
        self.module().jit_code_ranges().collect()
    }
    pub fn name(&self) -> Option<String> {
        self.module().module().name.clone()
    }
}

pub(crate) struct NullDebuggerAgent;

impl DebuggerAgent for NullDebuggerAgent {
    fn pause(&mut self, _kind: DebuggerPauseKind) -> DebuggerResumeAction {
        DebuggerResumeAction::Continue
    }
    fn register_module(&mut self, _module: DebuggerModule) -> Box<dyn DebuggerJitCodeRegistration> {
        struct NullReg;
        impl DebuggerJitCodeRegistration for NullReg {
            fn id(&self) -> u32 {
                0
            }
        }
        Box::new(NullReg)
    }
    fn find_breakpoint(&self, _addr: usize) -> Option<*const BreakpointData> {
        None
    }
    fn add_breakpoints(&self, _module_id: u32, _addr: u64) {
        panic!()
    }
}
