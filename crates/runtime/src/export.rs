use crate::vmcontext::{
    VMCallerCheckedAnyfunc, VMContext, VMGlobalDefinition, VMMemoryDefinition, VMTableDefinition,
};
use crate::RuntimeInstance;
use std::any::Any;
use std::ptr::NonNull;
use wasmtime_environ::wasm::Global;
use wasmtime_environ::{MemoryPlan, TablePlan};

/// The value of an export passed from one instance to another.
pub enum Export {
    /// A function export value.
    Function(ExportFunction),

    /// A table export value.
    Table(ExportTable),

    /// A memory export value.
    Memory(ExportMemory),

    /// A global export value.
    Global(ExportGlobal),

    /// An instance
    Instance(RuntimeInstance),

    /// A module
    Module(Box<dyn Any>),
}

/// A function export value.
#[derive(Debug, Clone)]
pub struct ExportFunction {
    /// The `VMCallerCheckedAnyfunc` for this exported function.
    ///
    /// Note that exported functions cannot be a null funcref, so this is a
    /// non-null pointer.
    pub anyfunc: NonNull<VMCallerCheckedAnyfunc>,
}

impl From<ExportFunction> for Export {
    fn from(func: ExportFunction) -> Export {
        Export::Function(func)
    }
}

/// A table export value.
#[derive(Debug, Clone)]
pub struct ExportTable {
    /// The address of the table descriptor.
    pub definition: *mut VMTableDefinition,
    /// Pointer to the containing `VMContext`.
    pub vmctx: *mut VMContext,
    /// The table declaration, used for compatibilty checking.
    pub table: TablePlan,
}

impl From<ExportTable> for Export {
    fn from(func: ExportTable) -> Export {
        Export::Table(func)
    }
}

/// A memory export value.
#[derive(Debug, Clone)]
pub struct ExportMemory {
    /// The address of the memory descriptor.
    pub definition: *mut VMMemoryDefinition,
    /// Pointer to the containing `VMContext`.
    pub vmctx: *mut VMContext,
    /// The memory declaration, used for compatibilty checking.
    pub memory: MemoryPlan,
}

impl From<ExportMemory> for Export {
    fn from(func: ExportMemory) -> Export {
        Export::Memory(func)
    }
}

/// A global export value.
#[derive(Debug, Clone)]
pub struct ExportGlobal {
    /// The address of the global storage.
    pub definition: *mut VMGlobalDefinition,
    /// Pointer to a `VMContext` which has a lifetime at least as long as the
    /// global. This may not be the `VMContext` which defines the global.
    pub vmctx: *mut VMContext,
    /// The global declaration, used for compatibilty checking.
    pub global: Global,
}

impl From<ExportGlobal> for Export {
    fn from(func: ExportGlobal) -> Export {
        Export::Global(func)
    }
}
