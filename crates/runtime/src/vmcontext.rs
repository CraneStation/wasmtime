//! This file declares `VMContext` and several related structs which contain
//! fields that compiled wasm code accesses directly.

use crate::externref::VMExternRef;
use crate::instance::Instance;
use std::any::Any;
use std::cell::UnsafeCell;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use std::u32;
use wasmtime_environ::BuiltinFunctionIndex;

/// An imported function.
#[derive(Debug, Copy, Clone)]
#[repr(C)]
pub struct VMFunctionImport {
    /// A pointer to the imported function body.
    pub body: NonNull<VMFunctionBody>,

    /// A pointer to the `VMContext` that owns the function.
    pub vmctx: *mut VMContext,
}

#[cfg(test)]
mod test_vmfunction_import {
    use super::VMFunctionImport;
    use memoffset::offset_of;
    use std::mem::size_of;
    use wasmtime_environ::{Module, VMOffsets};

    #[test]
    fn check_vmfunction_import_offsets() {
        let module = Module::new();
        let offsets = VMOffsets::new(size_of::<*mut u8>() as u8, &module);
        assert_eq!(
            size_of::<VMFunctionImport>(),
            usize::from(offsets.size_of_vmfunction_import())
        );
        assert_eq!(
            offset_of!(VMFunctionImport, body),
            usize::from(offsets.vmfunction_import_body())
        );
        assert_eq!(
            offset_of!(VMFunctionImport, vmctx),
            usize::from(offsets.vmfunction_import_vmctx())
        );
    }
}

/// A placeholder byte-sized type which is just used to provide some amount of type
/// safety when dealing with pointers to JIT-compiled function bodies. Note that it's
/// deliberately not Copy, as we shouldn't be carelessly copying function body bytes
/// around.
#[repr(C)]
pub struct VMFunctionBody(u8);

#[cfg(test)]
mod test_vmfunction_body {
    use super::VMFunctionBody;
    use std::mem::size_of;

    #[test]
    fn check_vmfunction_body_offsets() {
        assert_eq!(size_of::<VMFunctionBody>(), 1);
    }
}

/// The fields compiled code needs to access to utilize a WebAssembly table
/// imported from another instance.
#[derive(Debug, Copy, Clone)]
#[repr(C)]
pub struct VMTableImport {
    /// A pointer to the imported table description.
    pub from: *mut VMTableDefinition,

    /// A pointer to the `VMContext` that owns the table description.
    pub vmctx: *mut VMContext,
}

#[cfg(test)]
mod test_vmtable_import {
    use super::VMTableImport;
    use memoffset::offset_of;
    use std::mem::size_of;
    use wasmtime_environ::{Module, VMOffsets};

    #[test]
    fn check_vmtable_import_offsets() {
        let module = Module::new();
        let offsets = VMOffsets::new(size_of::<*mut u8>() as u8, &module);
        assert_eq!(
            size_of::<VMTableImport>(),
            usize::from(offsets.size_of_vmtable_import())
        );
        assert_eq!(
            offset_of!(VMTableImport, from),
            usize::from(offsets.vmtable_import_from())
        );
        assert_eq!(
            offset_of!(VMTableImport, vmctx),
            usize::from(offsets.vmtable_import_vmctx())
        );
    }
}

/// The fields compiled code needs to access to utilize a WebAssembly linear
/// memory imported from another instance.
#[derive(Debug, Copy, Clone)]
#[repr(C)]
pub struct VMMemoryImport {
    /// A pointer to the imported memory description.
    pub from: *mut VMMemoryDefinition,

    /// A pointer to the `VMContext` that owns the memory description.
    pub vmctx: *mut VMContext,
}

#[cfg(test)]
mod test_vmmemory_import {
    use super::VMMemoryImport;
    use memoffset::offset_of;
    use std::mem::size_of;
    use wasmtime_environ::{Module, VMOffsets};

    #[test]
    fn check_vmmemory_import_offsets() {
        let module = Module::new();
        let offsets = VMOffsets::new(size_of::<*mut u8>() as u8, &module);
        assert_eq!(
            size_of::<VMMemoryImport>(),
            usize::from(offsets.size_of_vmmemory_import())
        );
        assert_eq!(
            offset_of!(VMMemoryImport, from),
            usize::from(offsets.vmmemory_import_from())
        );
        assert_eq!(
            offset_of!(VMMemoryImport, vmctx),
            usize::from(offsets.vmmemory_import_vmctx())
        );
    }
}

/// The fields compiled code needs to access to utilize a WebAssembly global
/// variable imported from another instance.
///
/// Note that unlike with functions, tables, and memories, `VMGlobalImport`
/// doesn't include a `vmctx` pointer. Globals are never resized, and don't
/// require a `vmctx` pointer to access.
#[derive(Debug, Copy, Clone)]
#[repr(C)]
pub struct VMGlobalImport {
    /// A pointer to the imported global variable description.
    pub from: *mut VMGlobalDefinition,
}

#[cfg(test)]
mod test_vmglobal_import {
    use super::VMGlobalImport;
    use memoffset::offset_of;
    use std::mem::size_of;
    use wasmtime_environ::{Module, VMOffsets};

    #[test]
    fn check_vmglobal_import_offsets() {
        let module = Module::new();
        let offsets = VMOffsets::new(size_of::<*mut u8>() as u8, &module);
        assert_eq!(
            size_of::<VMGlobalImport>(),
            usize::from(offsets.size_of_vmglobal_import())
        );
        assert_eq!(
            offset_of!(VMGlobalImport, from),
            usize::from(offsets.vmglobal_import_from())
        );
    }
}

/// The fields compiled code needs to access to utilize a WebAssembly linear
/// memory defined within the instance, namely the start address and the
/// size in bytes.
#[derive(Debug, Copy, Clone)]
#[repr(C)]
pub struct VMMemoryDefinition {
    /// The start address.
    pub base: *mut u8,

    /// The current logical size of this linear memory in bytes.
    pub current_length: usize,
}

#[cfg(test)]
mod test_vmmemory_definition {
    use super::VMMemoryDefinition;
    use memoffset::{offset_of, span_of};
    use std::mem::size_of;
    use wasmtime_environ::{Module, VMOffsets};

    #[test]
    fn check_vmmemory_definition_offsets() {
        let module = Module::new();
        let offsets = VMOffsets::new(size_of::<*mut u8>() as u8, &module);
        assert_eq!(
            size_of::<VMMemoryDefinition>(),
            usize::from(offsets.size_of_vmmemory_definition())
        );
        assert_eq!(
            offset_of!(VMMemoryDefinition, base),
            usize::from(offsets.vmmemory_definition_base())
        );
        assert_eq!(
            offset_of!(VMMemoryDefinition, current_length),
            usize::from(offsets.vmmemory_definition_current_length())
        );
        assert_eq!(
            span_of!(VMMemoryDefinition, current_length).len(),
            usize::from(offsets.size_of_vmmemory_definition_current_length())
        );
    }
}

/// The fields compiled code needs to access to utilize a WebAssembly table
/// defined within the instance.
#[derive(Debug, Copy, Clone)]
#[repr(C)]
pub struct VMTableDefinition {
    /// Pointer to the table data.
    pub base: *mut u8,

    /// The current number of elements in the table.
    pub current_elements: u32,
}

#[cfg(test)]
mod test_vmtable_definition {
    use super::VMTableDefinition;
    use memoffset::{offset_of, span_of};
    use std::mem::size_of;
    use wasmtime_environ::{Module, VMOffsets};

    #[test]
    fn check_vmtable_definition_offsets() {
        let module = Module::new();
        let offsets = VMOffsets::new(size_of::<*mut u8>() as u8, &module);
        assert_eq!(
            size_of::<VMTableDefinition>(),
            usize::from(offsets.size_of_vmtable_definition())
        );
        assert_eq!(
            offset_of!(VMTableDefinition, base),
            usize::from(offsets.vmtable_definition_base())
        );
        assert_eq!(
            offset_of!(VMTableDefinition, current_elements),
            usize::from(offsets.vmtable_definition_current_elements())
        );
        assert_eq!(
            span_of!(VMTableDefinition, current_elements).len(),
            usize::from(offsets.size_of_vmtable_definition_current_elements())
        );
    }
}

/// The storage for a WebAssembly global defined within the instance.
///
/// TODO: Pack the globals more densely, rather than using the same size
/// for every type.
#[derive(Debug, Copy, Clone)]
#[repr(C, align(16))]
pub struct VMGlobalDefinition {
    storage: [u8; 16],
    // If more elements are added here, remember to add offset_of tests below!
}

#[cfg(test)]
mod test_vmglobal_definition {
    use super::VMGlobalDefinition;
    use crate::externref::VMExternRef;
    use more_asserts::assert_ge;
    use std::mem::{align_of, size_of};
    use wasmtime_environ::{Module, VMOffsets};

    #[test]
    fn check_vmglobal_definition_alignment() {
        assert_ge!(align_of::<VMGlobalDefinition>(), align_of::<i32>());
        assert_ge!(align_of::<VMGlobalDefinition>(), align_of::<i64>());
        assert_ge!(align_of::<VMGlobalDefinition>(), align_of::<f32>());
        assert_ge!(align_of::<VMGlobalDefinition>(), align_of::<f64>());
        assert_ge!(align_of::<VMGlobalDefinition>(), align_of::<[u8; 16]>());
    }

    #[test]
    fn check_vmglobal_definition_offsets() {
        let module = Module::new();
        let offsets = VMOffsets::new(size_of::<*mut u8>() as u8, &module);
        assert_eq!(
            size_of::<VMGlobalDefinition>(),
            usize::from(offsets.size_of_vmglobal_definition())
        );
    }

    #[test]
    fn check_vmglobal_begins_aligned() {
        let module = Module::new();
        let offsets = VMOffsets::new(size_of::<*mut u8>() as u8, &module);
        assert_eq!(offsets.vmctx_globals_begin() % 16, 0);
    }

    #[test]
    fn check_vmglobal_can_contain_externref() {
        assert!(size_of::<VMExternRef>() <= size_of::<VMGlobalDefinition>());
    }
}

impl VMGlobalDefinition {
    /// Construct a `VMGlobalDefinition`.
    pub fn new() -> Self {
        Self { storage: [0; 16] }
    }

    /// Return a reference to the value as an i32.
    #[allow(clippy::cast_ptr_alignment)]
    pub unsafe fn as_i32(&self) -> &i32 {
        &*(self.storage.as_ref().as_ptr() as *const i32)
    }

    /// Return a mutable reference to the value as an i32.
    #[allow(clippy::cast_ptr_alignment)]
    pub unsafe fn as_i32_mut(&mut self) -> &mut i32 {
        &mut *(self.storage.as_mut().as_mut_ptr() as *mut i32)
    }

    /// Return a reference to the value as a u32.
    #[allow(clippy::cast_ptr_alignment)]
    pub unsafe fn as_u32(&self) -> &u32 {
        &*(self.storage.as_ref().as_ptr() as *const u32)
    }

    /// Return a mutable reference to the value as an u32.
    #[allow(clippy::cast_ptr_alignment)]
    pub unsafe fn as_u32_mut(&mut self) -> &mut u32 {
        &mut *(self.storage.as_mut().as_mut_ptr() as *mut u32)
    }

    /// Return a reference to the value as an i64.
    #[allow(clippy::cast_ptr_alignment)]
    pub unsafe fn as_i64(&self) -> &i64 {
        &*(self.storage.as_ref().as_ptr() as *const i64)
    }

    /// Return a mutable reference to the value as an i64.
    #[allow(clippy::cast_ptr_alignment)]
    pub unsafe fn as_i64_mut(&mut self) -> &mut i64 {
        &mut *(self.storage.as_mut().as_mut_ptr() as *mut i64)
    }

    /// Return a reference to the value as an u64.
    #[allow(clippy::cast_ptr_alignment)]
    pub unsafe fn as_u64(&self) -> &u64 {
        &*(self.storage.as_ref().as_ptr() as *const u64)
    }

    /// Return a mutable reference to the value as an u64.
    #[allow(clippy::cast_ptr_alignment)]
    pub unsafe fn as_u64_mut(&mut self) -> &mut u64 {
        &mut *(self.storage.as_mut().as_mut_ptr() as *mut u64)
    }

    /// Return a reference to the value as an f32.
    #[allow(clippy::cast_ptr_alignment)]
    pub unsafe fn as_f32(&self) -> &f32 {
        &*(self.storage.as_ref().as_ptr() as *const f32)
    }

    /// Return a mutable reference to the value as an f32.
    #[allow(clippy::cast_ptr_alignment)]
    pub unsafe fn as_f32_mut(&mut self) -> &mut f32 {
        &mut *(self.storage.as_mut().as_mut_ptr() as *mut f32)
    }

    /// Return a reference to the value as f32 bits.
    #[allow(clippy::cast_ptr_alignment)]
    pub unsafe fn as_f32_bits(&self) -> &u32 {
        &*(self.storage.as_ref().as_ptr() as *const u32)
    }

    /// Return a mutable reference to the value as f32 bits.
    #[allow(clippy::cast_ptr_alignment)]
    pub unsafe fn as_f32_bits_mut(&mut self) -> &mut u32 {
        &mut *(self.storage.as_mut().as_mut_ptr() as *mut u32)
    }

    /// Return a reference to the value as an f64.
    #[allow(clippy::cast_ptr_alignment)]
    pub unsafe fn as_f64(&self) -> &f64 {
        &*(self.storage.as_ref().as_ptr() as *const f64)
    }

    /// Return a mutable reference to the value as an f64.
    #[allow(clippy::cast_ptr_alignment)]
    pub unsafe fn as_f64_mut(&mut self) -> &mut f64 {
        &mut *(self.storage.as_mut().as_mut_ptr() as *mut f64)
    }

    /// Return a reference to the value as f64 bits.
    #[allow(clippy::cast_ptr_alignment)]
    pub unsafe fn as_f64_bits(&self) -> &u64 {
        &*(self.storage.as_ref().as_ptr() as *const u64)
    }

    /// Return a mutable reference to the value as f64 bits.
    #[allow(clippy::cast_ptr_alignment)]
    pub unsafe fn as_f64_bits_mut(&mut self) -> &mut u64 {
        &mut *(self.storage.as_mut().as_mut_ptr() as *mut u64)
    }

    /// Return a reference to the value as an u128.
    #[allow(clippy::cast_ptr_alignment)]
    pub unsafe fn as_u128(&self) -> &u128 {
        &*(self.storage.as_ref().as_ptr() as *const u128)
    }

    /// Return a mutable reference to the value as an u128.
    #[allow(clippy::cast_ptr_alignment)]
    pub unsafe fn as_u128_mut(&mut self) -> &mut u128 {
        &mut *(self.storage.as_mut().as_mut_ptr() as *mut u128)
    }

    /// Return a reference to the value as u128 bits.
    #[allow(clippy::cast_ptr_alignment)]
    pub unsafe fn as_u128_bits(&self) -> &[u8; 16] {
        &*(self.storage.as_ref().as_ptr() as *const [u8; 16])
    }

    /// Return a mutable reference to the value as u128 bits.
    #[allow(clippy::cast_ptr_alignment)]
    pub unsafe fn as_u128_bits_mut(&mut self) -> &mut [u8; 16] {
        &mut *(self.storage.as_mut().as_mut_ptr() as *mut [u8; 16])
    }

    /// Return a reference to the value as an externref.
    #[allow(clippy::cast_ptr_alignment)]
    pub unsafe fn as_externref(&self) -> &Option<VMExternRef> {
        &*(self.storage.as_ref().as_ptr() as *const Option<VMExternRef>)
    }

    /// Return a mutable reference to the value as an externref.
    #[allow(clippy::cast_ptr_alignment)]
    pub unsafe fn as_externref_mut(&mut self) -> &mut Option<VMExternRef> {
        &mut *(self.storage.as_mut().as_mut_ptr() as *mut Option<VMExternRef>)
    }

    /// Return a reference to the value as an anyfunc.
    #[allow(clippy::cast_ptr_alignment)]
    pub unsafe fn as_anyfunc(&self) -> *const VMCallerCheckedAnyfunc {
        *(self.storage.as_ref().as_ptr() as *const *const VMCallerCheckedAnyfunc)
    }

    /// Return a mutable reference to the value as an anyfunc.
    #[allow(clippy::cast_ptr_alignment)]
    pub unsafe fn as_anyfunc_mut(&mut self) -> &mut *const VMCallerCheckedAnyfunc {
        &mut *(self.storage.as_mut().as_mut_ptr() as *mut *const VMCallerCheckedAnyfunc)
    }
}

/// An index into the shared signature registry, usable for checking signatures
/// at indirect calls.
#[repr(C)]
#[derive(Debug, Eq, PartialEq, Clone, Copy, Hash)]
pub struct VMSharedSignatureIndex(u32);

#[cfg(test)]
mod test_vmshared_signature_index {
    use super::VMSharedSignatureIndex;
    use std::mem::size_of;
    use wasmtime_environ::{Module, TargetSharedSignatureIndex, VMOffsets};

    #[test]
    fn check_vmshared_signature_index() {
        let module = Module::new();
        let offsets = VMOffsets::new(size_of::<*mut u8>() as u8, &module);
        assert_eq!(
            size_of::<VMSharedSignatureIndex>(),
            usize::from(offsets.size_of_vmshared_signature_index())
        );
    }

    #[test]
    fn check_target_shared_signature_index() {
        assert_eq!(
            size_of::<VMSharedSignatureIndex>(),
            size_of::<TargetSharedSignatureIndex>()
        );
    }
}

impl VMSharedSignatureIndex {
    /// Create a new `VMSharedSignatureIndex`.
    #[inline]
    pub fn new(value: u32) -> Self {
        Self(value)
    }

    /// Returns the underlying bits of the index.
    #[inline]
    pub fn bits(&self) -> u32 {
        self.0
    }
}

impl Default for VMSharedSignatureIndex {
    #[inline]
    fn default() -> Self {
        Self::new(u32::MAX)
    }
}

/// The VM caller-checked "anyfunc" record, for caller-side signature checking.
/// It consists of the actual function pointer and a signature id to be checked
/// by the caller.
#[derive(Debug, Clone)]
#[repr(C)]
pub struct VMCallerCheckedAnyfunc {
    /// Function body.
    pub func_ptr: NonNull<VMFunctionBody>,
    /// Function signature id.
    pub type_index: VMSharedSignatureIndex,
    /// Function `VMContext`.
    pub vmctx: *mut VMContext,
    // If more elements are added here, remember to add offset_of tests below!
}

#[cfg(test)]
mod test_vmcaller_checked_anyfunc {
    use super::VMCallerCheckedAnyfunc;
    use memoffset::offset_of;
    use std::mem::size_of;
    use wasmtime_environ::{Module, VMOffsets};

    #[test]
    fn check_vmcaller_checked_anyfunc_offsets() {
        let module = Module::new();
        let offsets = VMOffsets::new(size_of::<*mut u8>() as u8, &module);
        assert_eq!(
            size_of::<VMCallerCheckedAnyfunc>(),
            usize::from(offsets.size_of_vmcaller_checked_anyfunc())
        );
        assert_eq!(
            offset_of!(VMCallerCheckedAnyfunc, func_ptr),
            usize::from(offsets.vmcaller_checked_anyfunc_func_ptr())
        );
        assert_eq!(
            offset_of!(VMCallerCheckedAnyfunc, type_index),
            usize::from(offsets.vmcaller_checked_anyfunc_type_index())
        );
        assert_eq!(
            offset_of!(VMCallerCheckedAnyfunc, vmctx),
            usize::from(offsets.vmcaller_checked_anyfunc_vmctx())
        );
    }
}

/// An array that stores addresses of builtin functions. We translate code
/// to use indirect calls. This way, we don't have to patch the code.
#[repr(C)]
pub struct VMBuiltinFunctionsArray {
    ptrs: [usize; Self::len()],
}

impl VMBuiltinFunctionsArray {
    pub const fn len() -> usize {
        BuiltinFunctionIndex::builtin_functions_total_number() as usize
    }

    pub fn initialized() -> Self {
        use crate::libcalls::*;

        let mut ptrs = [0; Self::len()];

        ptrs[BuiltinFunctionIndex::memory32_grow().index() as usize] =
            wasmtime_memory32_grow as usize;
        ptrs[BuiltinFunctionIndex::imported_memory32_grow().index() as usize] =
            wasmtime_imported_memory32_grow as usize;
        ptrs[BuiltinFunctionIndex::memory32_size().index() as usize] =
            wasmtime_memory32_size as usize;
        ptrs[BuiltinFunctionIndex::imported_memory32_size().index() as usize] =
            wasmtime_imported_memory32_size as usize;
        ptrs[BuiltinFunctionIndex::table_copy().index() as usize] = wasmtime_table_copy as usize;
        ptrs[BuiltinFunctionIndex::table_grow_funcref().index() as usize] =
            wasmtime_table_grow as usize;
        ptrs[BuiltinFunctionIndex::table_grow_externref().index() as usize] =
            wasmtime_table_grow as usize;
        ptrs[BuiltinFunctionIndex::table_init().index() as usize] = wasmtime_table_init as usize;
        ptrs[BuiltinFunctionIndex::elem_drop().index() as usize] = wasmtime_elem_drop as usize;
        ptrs[BuiltinFunctionIndex::memory_copy().index() as usize] = wasmtime_memory_copy as usize;
        ptrs[BuiltinFunctionIndex::memory_fill().index() as usize] = wasmtime_memory_fill as usize;
        ptrs[BuiltinFunctionIndex::imported_memory_fill().index() as usize] =
            wasmtime_imported_memory_fill as usize;
        ptrs[BuiltinFunctionIndex::memory_init().index() as usize] = wasmtime_memory_init as usize;
        ptrs[BuiltinFunctionIndex::data_drop().index() as usize] = wasmtime_data_drop as usize;
        ptrs[BuiltinFunctionIndex::drop_externref().index() as usize] =
            wasmtime_drop_externref as usize;
        ptrs[BuiltinFunctionIndex::activations_table_insert_with_gc().index() as usize] =
            wasmtime_activations_table_insert_with_gc as usize;
        ptrs[BuiltinFunctionIndex::externref_global_get().index() as usize] =
            wasmtime_externref_global_get as usize;
        ptrs[BuiltinFunctionIndex::externref_global_set().index() as usize] =
            wasmtime_externref_global_set as usize;
        ptrs[BuiltinFunctionIndex::table_fill_externref().index() as usize] =
            wasmtime_table_fill as usize;
        ptrs[BuiltinFunctionIndex::table_fill_funcref().index() as usize] =
            wasmtime_table_fill as usize;
        ptrs[BuiltinFunctionIndex::memory_atomic_notify().index() as usize] =
            wasmtime_memory_atomic_notify as usize;
        ptrs[BuiltinFunctionIndex::imported_memory_atomic_notify().index() as usize] =
            wasmtime_imported_memory_atomic_notify as usize;
        ptrs[BuiltinFunctionIndex::memory_atomic_wait32().index() as usize] =
            wasmtime_memory_atomic_wait32 as usize;
        ptrs[BuiltinFunctionIndex::imported_memory_atomic_wait32().index() as usize] =
            wasmtime_imported_memory_atomic_wait32 as usize;
        ptrs[BuiltinFunctionIndex::memory_atomic_wait64().index() as usize] =
            wasmtime_memory_atomic_wait64 as usize;
        ptrs[BuiltinFunctionIndex::imported_memory_atomic_wait64().index() as usize] =
            wasmtime_imported_memory_atomic_wait64 as usize;
        ptrs[BuiltinFunctionIndex::out_of_gas().index() as usize] = wasmtime_out_of_gas as usize;

        if cfg!(debug_assertions) {
            for i in 0..ptrs.len() {
                debug_assert!(ptrs[i] != 0, "index {} is not initialized", i);
            }
        }
        Self { ptrs }
    }
}

/// The storage for a WebAssembly invocation argument
///
/// TODO: These could be packed more densely, rather than using the same size for every type.
#[derive(Debug, Copy, Clone)]
#[repr(C, align(16))]
pub struct VMInvokeArgument([u8; 16]);

#[cfg(test)]
mod test_vm_invoke_argument {
    use super::VMInvokeArgument;
    use std::mem::{align_of, size_of};
    use wasmtime_environ::{Module, VMOffsets};

    #[test]
    fn check_vm_invoke_argument_alignment() {
        assert_eq!(align_of::<VMInvokeArgument>(), 16);
    }

    #[test]
    fn check_vmglobal_definition_offsets() {
        let module = Module::new();
        let offsets = VMOffsets::new(size_of::<*mut u8>() as u8, &module);
        assert_eq!(
            size_of::<VMInvokeArgument>(),
            usize::from(offsets.size_of_vmglobal_definition())
        );
    }
}

impl VMInvokeArgument {
    /// Create a new invocation argument filled with zeroes
    pub fn new() -> Self {
        Self([0; 16])
    }
}

/// Structure used to control interrupting wasm code.
#[derive(Debug)]
#[repr(C)]
pub struct VMInterrupts {
    /// Current stack limit of the wasm module.
    ///
    /// This is used to control both stack overflow as well as interrupting wasm
    /// modules. For more information see `crates/environ/src/cranelift.rs`.
    pub stack_limit: AtomicUsize,

    /// Indicator of how much fuel has been consumed and is remaining to
    /// WebAssembly.
    ///
    /// This field is typically negative and increments towards positive. Upon
    /// turning positive a wasm trap will be generated. This field is only
    /// modified if wasm is configured to consume fuel.
    pub fuel_consumed: UnsafeCell<i64>,
}

impl VMInterrupts {
    /// Flag that an interrupt should occur
    pub fn interrupt(&self) {
        self.stack_limit
            .store(wasmtime_environ::INTERRUPTED, SeqCst);
    }
}

impl Default for VMInterrupts {
    fn default() -> VMInterrupts {
        VMInterrupts {
            stack_limit: AtomicUsize::new(usize::max_value()),
            fuel_consumed: UnsafeCell::new(0),
        }
    }
}

#[cfg(test)]
mod test_vminterrupts {
    use super::VMInterrupts;
    use memoffset::offset_of;
    use std::mem::size_of;
    use wasmtime_environ::{Module, VMOffsets};

    #[test]
    fn check_vminterrupts_interrupted_offset() {
        let module = Module::new();
        let offsets = VMOffsets::new(size_of::<*mut u8>() as u8, &module);
        assert_eq!(
            offset_of!(VMInterrupts, stack_limit),
            usize::from(offsets.vminterrupts_stack_limit())
        );
    }
}

/// The VM "context", which is pointed to by the `vmctx` arg in Cranelift.
/// This has information about globals, memories, tables, and other runtime
/// state associated with the current instance.
///
/// The struct here is empty, as the sizes of these fields are dynamic, and
/// we can't describe them in Rust's type system. Sufficient memory is
/// allocated at runtime.
#[derive(Debug)]
#[repr(C, align(16))] // align 16 since globals are aligned to that and contained inside
pub struct VMContext {}

impl VMContext {
    /// Return a mutable reference to the associated `Instance`.
    ///
    /// # Safety
    /// This is unsafe because it doesn't work on just any `VMContext`, it must
    /// be a `VMContext` allocated as part of an `Instance`.
    #[allow(clippy::cast_ptr_alignment)]
    #[inline]
    pub(crate) unsafe fn instance(&self) -> &Instance {
        &*((self as *const Self as *mut u8).offset(-Instance::vmctx_offset()) as *const Instance)
    }

    /// Return a reference to the host state associated with this `Instance`.
    ///
    /// # Safety
    /// This is unsafe because it doesn't work on just any `VMContext`, it must
    /// be a `VMContext` allocated as part of an `Instance`.
    #[inline]
    pub unsafe fn host_state(&self) -> &dyn Any {
        self.instance().host_state()
    }
}

/// Trampoline function pointer type.
pub type VMTrampoline = unsafe extern "C" fn(
    *mut VMContext,        // callee vmctx
    *mut VMContext,        // caller vmctx
    *const VMFunctionBody, // function we're actually calling
    *mut u128,             // space for arguments and return values
);
