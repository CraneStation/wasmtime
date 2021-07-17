//! Runtime library calls.
//!
//! Note that Wasm compilers may sometimes perform these inline rather than
//! calling them, particularly when CPUs have special instructions which compute
//! them directly.
//!
//! These functions are called by compiled Wasm code, and therefore must take
//! certain care about some things:
//!
//! * They must always be `pub extern "C"` and should only contain basic, raw
//!   i32/i64/f32/f64/pointer parameters that are safe to pass across the system
//!   ABI!
//!
//! * If any nested function propagates an `Err(trap)` out to the library
//!   function frame, we need to raise it. This involves some nasty and quite
//!   unsafe code under the covers! Notable, after raising the trap, drops
//!   **will not** be run for local variables! This can lead to things like
//!   leaking `InstanceHandle`s which leads to never deallocating JIT code,
//!   instances, and modules! Therefore, always use nested blocks to ensure
//!   drops run before raising a trap:
//!
//!   ```ignore
//!   pub extern "C" fn my_lib_function(...) {
//!       let result = {
//!           // Do everything in here so drops run at the end of the block.
//!           ...
//!       };
//!       if let Err(trap) = result {
//!           // Now we can safely raise the trap without leaking!
//!           raise_lib_trap(trap);
//!       }
//!   }
//!   ```
//!
//! * When receiving a raw `*mut u8` that is actually a `VMExternRef` reference,
//!   convert it into a proper `VMExternRef` with `VMExternRef::clone_from_raw`
//!   as soon as apossible. Any GC before raw pointer is converted into a
//!   reference can potentially collect the referenced object, which could lead
//!   to use after free. Avoid this by eagerly converting into a proper
//!   `VMExternRef`!
//!
//!   ```ignore
//!   pub unsafe extern "C" my_lib_takes_ref(raw_extern_ref: *mut u8) {
//!       // Before `clone_from_raw`, `raw_extern_ref` is potentially unrooted,
//!       // and doing GC here could lead to use after free!
//!
//!       let my_extern_ref = if raw_extern_ref.is_null() {
//!           None
//!       } else {
//!           Some(VMExternRef::clone_from_raw(raw_extern_ref))
//!       };
//!
//!       // Now that we did `clone_from_raw`, it is safe to do a GC (or do
//!       // anything else that might transitively GC, like call back into
//!       // Wasm!)
//!   }
//!   ```

use crate::externref::VMExternRef;
use crate::table::Table;
use crate::traphandlers::{raise_lib_trap, Trap};
use crate::vmcontext::{VMCallerCheckedAnyfunc, VMContext};
use std::mem;
use std::ptr::{self, NonNull};
use wasmtime_environ::wasm::{
    DataIndex, ElemIndex, GlobalIndex, MemoryIndex, TableElementType, TableIndex,
};

const TOINT_32: f32 = 1.0 / f32::EPSILON;
const TOINT_64: f64 = 1.0 / f64::EPSILON;

/// Implementation of f32.ceil
pub extern "C" fn wasmtime_f32_ceil(x: f32) -> f32 {
    x.ceil()
}

/// Implementation of f32.floor
pub extern "C" fn wasmtime_f32_floor(x: f32) -> f32 {
    x.floor()
}

/// Implementation of f32.trunc
pub extern "C" fn wasmtime_f32_trunc(x: f32) -> f32 {
    x.trunc()
}

/// Implementation of f32.nearest
#[allow(clippy::float_arithmetic, clippy::float_cmp)]
pub extern "C" fn wasmtime_f32_nearest(x: f32) -> f32 {
    // Rust doesn't have a nearest function; there's nearbyint, but it's not
    // stabilized, so do it manually.
    // Nearest is either ceil or floor depending on which is nearest or even.
    // This approach exploited round half to even default mode.
    let i = x.to_bits();
    let e = i >> 23 & 0xff;
    if e >= 0x7f_u32 + 23 {
        // Check for NaNs.
        if e == 0xff {
            // Read the 23-bits significand.
            if i & 0x7fffff != 0 {
                // Ensure it's arithmetic by setting the significand's most
                // significant bit to 1; it also works for canonical NaNs.
                return f32::from_bits(i | (1 << 22));
            }
        }
        x
    } else {
        (x.abs() + TOINT_32 - TOINT_32).copysign(x)
    }
}

/// Implementation of i64.udiv
pub extern "C" fn wasmtime_i64_udiv(x: u64, y: u64) -> u64 {
    x / y
}

/// Implementation of i64.sdiv
pub extern "C" fn wasmtime_i64_sdiv(x: i64, y: i64) -> i64 {
    x / y
}

/// Implementation of i64.urem
pub extern "C" fn wasmtime_i64_urem(x: u64, y: u64) -> u64 {
    x % y
}

/// Implementation of i64.srem
pub extern "C" fn wasmtime_i64_srem(x: i64, y: i64) -> i64 {
    x % y
}

/// Implementation of i64.ishl
pub extern "C" fn wasmtime_i64_ishl(x: i64, y: i64) -> i64 {
    x << y
}

/// Implementation of i64.ushr
pub extern "C" fn wasmtime_i64_ushr(x: u64, y: i64) -> u64 {
    x >> y
}

/// Implementation of i64.sshr
pub extern "C" fn wasmtime_i64_sshr(x: i64, y: i64) -> i64 {
    x >> y
}

/// Implementation of f64.ceil
pub extern "C" fn wasmtime_f64_ceil(x: f64) -> f64 {
    x.ceil()
}

/// Implementation of f64.floor
pub extern "C" fn wasmtime_f64_floor(x: f64) -> f64 {
    x.floor()
}

/// Implementation of f64.trunc
pub extern "C" fn wasmtime_f64_trunc(x: f64) -> f64 {
    x.trunc()
}

/// Implementation of f64.nearest
#[allow(clippy::float_arithmetic, clippy::float_cmp)]
pub extern "C" fn wasmtime_f64_nearest(x: f64) -> f64 {
    // Rust doesn't have a nearest function; there's nearbyint, but it's not
    // stabilized, so do it manually.
    // Nearest is either ceil or floor depending on which is nearest or even.
    // This approach exploited round half to even default mode.
    let i = x.to_bits();
    let e = i >> 52 & 0x7ff;
    if e >= 0x3ff_u64 + 52 {
        // Check for NaNs.
        if e == 0x7ff {
            // Read the 52-bits significand.
            if i & 0xfffffffffffff != 0 {
                // Ensure it's arithmetic by setting the significand's most
                // significant bit to 1; it also works for canonical NaNs.
                return f64::from_bits(i | (1 << 51));
            }
        }
        x
    } else {
        (x.abs() + TOINT_64 - TOINT_64).copysign(x)
    }
}

/// Implementation of probestack
pub unsafe extern "C" fn wasmtime_probestack() {
    // TODO forward to __rust_probestack ?
    panic!("TODO probestack")
}

/// Implementation of elf_tls_get_addr
pub unsafe extern "C" fn wasmtime_tls_get_addr() {
    panic!("TODO elf_tls_get_addr")
}

/// Implementation of memcpy
pub unsafe extern "C" fn wasmtime_memcpy(dest: *mut u8, src: *const u8, n: usize) -> *mut u8 {
    use std::ffi::c_void;
    libc::memcpy(dest as *mut c_void, src as *const c_void, n) as *mut u8
}

/// Implementation of memmove
pub unsafe extern "C" fn wasmtime_memmove(dest: *mut u8, src: *const u8, n: usize) -> *mut u8 {
    use std::ffi::c_void;
    libc::memmove(dest as *mut c_void, src as *const c_void, n) as *mut u8
}

/// Implementation of memset
pub unsafe extern "C" fn wasmtime_memset(s: *mut u8, c: i32, n: usize) -> *mut u8 {
    use std::ffi::c_void;
    libc::memset(s as *mut c_void, c, n) as *mut u8
}

/// Implementation of memory.grow for locally-defined 32-bit memories.
pub unsafe extern "C" fn wasmtime_memory32_grow(
    vmctx: *mut VMContext,
    delta: u32,
    memory_index: u32,
) -> u32 {
    let instance = (*vmctx).instance_mut();
    let memory_index = MemoryIndex::from_u32(memory_index);
    instance
        .memory_grow(memory_index, delta)
        .unwrap_or(u32::max_value())
}

/// Implementation of `table.grow`.
pub unsafe extern "C" fn wasmtime_table_grow(
    vmctx: *mut VMContext,
    table_index: u32,
    delta: u32,
    // NB: we don't know whether this is a pointer to a `VMCallerCheckedAnyfunc`
    // or is a `VMExternRef` until we look at the table type.
    init_value: *mut u8,
) -> u32 {
    let instance = (*vmctx).instance_mut();
    let table_index = TableIndex::from_u32(table_index);
    let element = match instance.table_element_type(table_index) {
        TableElementType::Func => (init_value as *mut VMCallerCheckedAnyfunc).into(),
        TableElementType::Val(ty) => {
            debug_assert_eq!(ty, crate::ref_type());

            let init_value = if init_value.is_null() {
                None
            } else {
                Some(VMExternRef::clone_from_raw(init_value))
            };
            init_value.into()
        }
    };
    instance
        .table_grow(table_index, delta, element)
        .unwrap_or(-1_i32 as u32)
}

/// Implementation of `table.fill`.
pub unsafe extern "C" fn wasmtime_table_fill(
    vmctx: *mut VMContext,
    table_index: u32,
    dst: u32,
    // NB: we don't know whether this is a `VMExternRef` or a pointer to a
    // `VMCallerCheckedAnyfunc` until we look at the table's element type.
    val: *mut u8,
    len: u32,
) {
    let result = {
        let instance = (*vmctx).instance_mut();
        let table_index = TableIndex::from_u32(table_index);
        let table = &mut *instance.get_table(table_index);
        match table.element_type() {
            TableElementType::Func => {
                let val = val as *mut VMCallerCheckedAnyfunc;
                table.fill(dst, val.into(), len)
            }
            TableElementType::Val(ty) => {
                debug_assert_eq!(ty, crate::ref_type());
                let val = if val.is_null() {
                    None
                } else {
                    Some(VMExternRef::clone_from_raw(val))
                };
                table.fill(dst, val.into(), len)
            }
        }
    };
    if let Err(trap) = result {
        raise_lib_trap(trap);
    }
}

/// Implementation of `table.copy`.
pub unsafe extern "C" fn wasmtime_table_copy(
    vmctx: *mut VMContext,
    dst_table_index: u32,
    src_table_index: u32,
    dst: u32,
    src: u32,
    len: u32,
) {
    let result = {
        let dst_table_index = TableIndex::from_u32(dst_table_index);
        let src_table_index = TableIndex::from_u32(src_table_index);
        let instance = (*vmctx).instance_mut();
        let dst_table = instance.get_table(dst_table_index);
        let src_table = instance.get_table(src_table_index);
        Table::copy(dst_table, src_table, dst, src, len)
    };
    if let Err(trap) = result {
        raise_lib_trap(trap);
    }
}

/// Implementation of `table.init`.
pub unsafe extern "C" fn wasmtime_table_init(
    vmctx: *mut VMContext,
    table_index: u32,
    elem_index: u32,
    dst: u32,
    src: u32,
    len: u32,
) {
    let result = {
        let table_index = TableIndex::from_u32(table_index);
        let elem_index = ElemIndex::from_u32(elem_index);
        let instance = (*vmctx).instance_mut();
        instance.table_init(table_index, elem_index, dst, src, len)
    };
    if let Err(trap) = result {
        raise_lib_trap(trap);
    }
}

/// Implementation of `elem.drop`.
pub unsafe extern "C" fn wasmtime_elem_drop(vmctx: *mut VMContext, elem_index: u32) {
    let elem_index = ElemIndex::from_u32(elem_index);
    let instance = (*vmctx).instance_mut();
    instance.elem_drop(elem_index);
}

/// Implementation of `memory.copy` for locally defined memories.
pub unsafe extern "C" fn wasmtime_memory_copy(
    vmctx: *mut VMContext,
    dst_index: u32,
    dst: u32,
    src_index: u32,
    src: u32,
    len: u32,
) {
    let result = {
        let src_index = MemoryIndex::from_u32(src_index);
        let dst_index = MemoryIndex::from_u32(dst_index);
        let instance = (*vmctx).instance_mut();
        instance.memory_copy(dst_index, dst, src_index, src, len)
    };
    if let Err(trap) = result {
        raise_lib_trap(trap);
    }
}

/// Implementation of `memory.fill` for locally defined memories.
pub unsafe extern "C" fn wasmtime_memory_fill(
    vmctx: *mut VMContext,
    memory_index: u32,
    dst: u32,
    val: u32,
    len: u32,
) {
    let result = {
        let memory_index = MemoryIndex::from_u32(memory_index);
        let instance = (*vmctx).instance_mut();
        instance.memory_fill(memory_index, dst, val, len)
    };
    if let Err(trap) = result {
        raise_lib_trap(trap);
    }
}

/// Implementation of `memory.init`.
pub unsafe extern "C" fn wasmtime_memory_init(
    vmctx: *mut VMContext,
    memory_index: u32,
    data_index: u32,
    dst: u32,
    src: u32,
    len: u32,
) {
    let result = {
        let memory_index = MemoryIndex::from_u32(memory_index);
        let data_index = DataIndex::from_u32(data_index);
        let instance = (*vmctx).instance_mut();
        instance.memory_init(memory_index, data_index, dst, src, len)
    };
    if let Err(trap) = result {
        raise_lib_trap(trap);
    }
}

/// Implementation of `data.drop`.
pub unsafe extern "C" fn wasmtime_data_drop(vmctx: *mut VMContext, data_index: u32) {
    let data_index = DataIndex::from_u32(data_index);
    let instance = (*vmctx).instance_mut();
    instance.data_drop(data_index)
}

/// Drop a `VMExternRef`.
pub unsafe extern "C" fn wasmtime_drop_externref(externref: *mut u8) {
    let externref = externref as *mut crate::externref::VMExternData;
    let externref = NonNull::new(externref).unwrap();
    crate::externref::VMExternData::drop_and_dealloc(externref);
}

/// Do a GC and insert the given `externref` into the
/// `VMExternRefActivationsTable`.
pub unsafe extern "C" fn wasmtime_activations_table_insert_with_gc(
    vmctx: *mut VMContext,
    externref: *mut u8,
) {
    let externref = VMExternRef::clone_from_raw(externref);
    let instance = (*vmctx).instance();
    let (activations_table, module_info_lookup) = (*instance.store()).externref_activations_table();
    activations_table.insert_with_gc(externref, module_info_lookup);
}

/// Perform a Wasm `global.get` for `externref` globals.
pub unsafe extern "C" fn wasmtime_externref_global_get(
    vmctx: *mut VMContext,
    index: u32,
) -> *mut u8 {
    let index = GlobalIndex::from_u32(index);
    let instance = (*vmctx).instance();
    let global = instance.defined_or_imported_global_ptr(index);
    match (*global).as_externref().clone() {
        None => ptr::null_mut(),
        Some(externref) => {
            let raw = externref.as_raw();
            let (activations_table, module_info_lookup) =
                (*instance.store()).externref_activations_table();
            activations_table.insert_with_gc(externref, module_info_lookup);
            raw
        }
    }
}

/// Perform a Wasm `global.set` for `externref` globals.
pub unsafe extern "C" fn wasmtime_externref_global_set(
    vmctx: *mut VMContext,
    index: u32,
    externref: *mut u8,
) {
    let externref = if externref.is_null() {
        None
    } else {
        Some(VMExternRef::clone_from_raw(externref))
    };

    let index = GlobalIndex::from_u32(index);
    let instance = (*vmctx).instance();
    let global = instance.defined_or_imported_global_ptr(index);

    // Swap the new `externref` value into the global before we drop the old
    // value. This protects against an `externref` with a `Drop` implementation
    // that calls back into Wasm and touches this global again (we want to avoid
    // it observing a halfway-deinitialized value).
    let old = mem::replace((*global).as_externref_mut(), externref);
    drop(old);
}

#[derive(Debug)]
struct Unimplemented(&'static str);
impl std::error::Error for Unimplemented {}
impl std::fmt::Display for Unimplemented {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::result::Result<(), std::fmt::Error> {
        write!(f, "unimplemented: {}", self.0)
    }
}

/// Implementation of `memory.atomic.notify` for locally defined memories.
pub unsafe extern "C" fn wasmtime_memory_atomic_notify(
    _vmctx: *mut VMContext,
    _memory_index: u32,
    _addr: u32,
    _count: u32,
) -> u32 {
    raise_lib_trap(Trap::User(Box::new(Unimplemented(
        "wasm atomics (fn wasmtime_memory_atomic_notify) unsupported",
    ))));
}

/// Implementation of `memory.atomic.wait32` for locally defined memories.
pub unsafe extern "C" fn wasmtime_memory_atomic_wait32(
    _vmctx: *mut VMContext,
    _memory_index: u32,
    _addr: u32,
    _expected: u32,
    _timeout: u64,
) -> u32 {
    raise_lib_trap(Trap::User(Box::new(Unimplemented(
        "wasm atomics (fn wasmtime_memory_atomic_wait32) unsupported",
    ))));
}

/// Implementation of `memory.atomic.wait64` for locally defined memories.
pub unsafe extern "C" fn wasmtime_memory_atomic_wait64(
    _vmctx: *mut VMContext,
    _memory_index: u32,
    _addr: u32,
    _expected: u64,
    _timeout: u64,
) -> u32 {
    raise_lib_trap(Trap::User(Box::new(Unimplemented(
        "wasm atomics (fn wasmtime_memory_atomic_wait32) unsupported",
    ))));
}

/// Hook for when an instance runs out of fuel.
pub unsafe extern "C" fn wasmtime_out_of_gas(vmctx: *mut VMContext) {
    match (*(*vmctx).instance().store()).out_of_gas() {
        Ok(()) => {}
        Err(err) => crate::traphandlers::raise_user_trap(err),
    }
}
