//! Memory management for executable code.

use crate::object::{
    utils::{try_parse_func_name, try_parse_trampoline_name},
    ObjectUnwindInfo,
};
use crate::unwind::UnwindRegistry;
use object::read::{File as ObjectFile, Object, ObjectSection};
use region;
use std::collections::BTreeMap;
use std::mem::ManuallyDrop;
use std::{cmp, mem};
use wasmtime_environ::{
    isa::{unwind::UnwindInfo, TargetIsa},
    wasm::{FuncIndex, SignatureIndex},
    CompiledFunction,
};
use wasmtime_runtime::{Mmap, VMFunctionBody};

struct CodeMemoryEntry {
    mmap: ManuallyDrop<Mmap>,
    registry: ManuallyDrop<UnwindRegistry>,
    len: usize,
}

impl CodeMemoryEntry {
    fn with_capacity(cap: usize) -> Result<Self, String> {
        let mmap = ManuallyDrop::new(Mmap::with_at_least(cap)?);
        let registry = ManuallyDrop::new(UnwindRegistry::new(mmap.as_ptr() as usize));
        Ok(Self {
            mmap,
            registry,
            len: 0,
        })
    }

    fn range(&self) -> (usize, usize) {
        let start = self.mmap.as_ptr() as usize;
        let end = start + self.len;
        (start, end)
    }
}

impl Drop for CodeMemoryEntry {
    fn drop(&mut self) {
        unsafe {
            // The registry needs to be dropped before the mmap
            ManuallyDrop::drop(&mut self.registry);
            ManuallyDrop::drop(&mut self.mmap);
        }
    }
}

pub(crate) struct CodeMemoryObjectAllocation<'a> {
    buf: &'a mut [u8],
    funcs: BTreeMap<FuncIndex, (usize, usize)>,
    trampolines: BTreeMap<SignatureIndex, (usize, usize)>,
}

impl<'a> CodeMemoryObjectAllocation<'a> {
    pub fn code_range(self) -> &'a mut [u8] {
        self.buf
    }
    pub fn funcs(&'a self) -> impl Iterator<Item = (FuncIndex, &'a mut [VMFunctionBody])> + 'a {
        let buf = self.buf as *const _ as *mut [u8];
        self.funcs.iter().map(move |(i, (start, len))| {
            (*i, unsafe {
                CodeMemory::view_as_mut_vmfunc_slice(&mut (*buf)[*start..*start + *len])
            })
        })
    }
    pub fn trampolines(
        &'a self,
    ) -> impl Iterator<Item = (SignatureIndex, &'a mut [VMFunctionBody])> + 'a {
        let buf = self.buf as *const _ as *mut [u8];
        self.trampolines.iter().map(move |(i, (start, len))| {
            (*i, unsafe {
                CodeMemory::view_as_mut_vmfunc_slice(&mut (*buf)[*start..*start + *len])
            })
        })
    }
}

/// Memory manager for executable code.
pub struct CodeMemory {
    current: Option<CodeMemoryEntry>,
    entries: Vec<CodeMemoryEntry>,
    published: usize,
}

fn _assert() {
    fn _assert_send_sync<T: Send + Sync>() {}
    _assert_send_sync::<CodeMemory>();
}

impl CodeMemory {
    /// Create a new `CodeMemory` instance.
    pub fn new() -> Self {
        Self {
            current: None,
            entries: Vec::new(),
            published: 0,
        }
    }

    /// Allocate a continuous memory block for a single compiled function.
    /// TODO: Reorganize the code that calls this to emit code directly into the
    /// mmap region rather than into a Vec that we need to copy in.
    pub fn allocate_for_function<'a>(
        &mut self,
        func: &'a CompiledFunction,
    ) -> Result<&mut [VMFunctionBody], String> {
        let size = Self::function_allocation_size(func);

        let (buf, registry, start) = self.allocate(size)?;

        let (_, _, vmfunc) = Self::copy_function(func, start as u32, buf, registry);

        Ok(vmfunc)
    }

    /// Make all allocated memory executable.
    pub fn publish(&mut self, isa: &dyn TargetIsa) {
        self.push_current(0)
            .expect("failed to push current memory map");

        for CodeMemoryEntry {
            mmap: m,
            registry: r,
            ..
        } in &mut self.entries[self.published..]
        {
            // Remove write access to the pages due to the relocation fixups.
            r.publish(isa)
                .expect("failed to publish function unwind registry");

            if !m.is_empty() {
                unsafe {
                    region::protect(m.as_mut_ptr(), m.len(), region::Protection::READ_EXECUTE)
                }
                .expect("unable to make memory readonly and executable");
            }
        }

        self.published = self.entries.len();
    }

    /// Quickly patches published code.
    pub fn patch_published(&self, addr: usize, len: usize, f: &mut dyn FnMut()) {
        // TODO make &mut self?
        debug_assert!(len > 0);
        // Slow lookup of code range to patch.
        for CodeMemoryEntry { mmap: m, .. } in &self.entries[..self.published] {
            let ptr = m.as_ptr() as usize;
            let m_len = m.len();
            if ptr > addr + len || ptr + m_len < addr {
                return;
            }
            assert!(ptr <= addr && addr + len <= ptr + m_len);
            unsafe {
                region::protect(m.as_ptr(), m.len(), region::Protection::READ_WRITE)
                    .expect("unable to make memory writable");
            }
            f();
            unsafe {
                region::protect(m.as_ptr(), m.len(), region::Protection::READ_EXECUTE)
                    .expect("unable to make memory readonly and executable");
            }
        }
    }

    /// Allocate `size` bytes of memory which can be made executable later by
    /// calling `publish()`. Note that we allocate the memory as writeable so
    /// that it can be written to and patched, though we make it readonly before
    /// actually executing from it.
    ///
    /// A few values are returned:
    ///
    /// * A mutable slice which references the allocated memory
    /// * A function table instance where unwind information is registered
    /// * The offset within the current mmap that the slice starts at
    ///
    /// TODO: Add an alignment flag.
    fn allocate(&mut self, size: usize) -> Result<(&mut [u8], &mut UnwindRegistry, usize), String> {
        assert!(size > 0);

        if match &self.current {
            Some(e) => e.mmap.len() - e.len < size,
            None => true,
        } {
            self.push_current(cmp::max(0x10000, size))?;
        }

        let e = self.current.as_mut().unwrap();
        let old_position = e.len;
        e.len += size;

        Ok((
            &mut e.mmap.as_mut_slice()[old_position..e.len],
            &mut e.registry,
            old_position,
        ))
    }

    /// Calculates the allocation size of the given compiled function.
    fn function_allocation_size(func: &CompiledFunction) -> usize {
        match &func.unwind_info {
            Some(UnwindInfo::WindowsX64(info)) => {
                // Windows unwind information is required to be emitted into code memory
                // This is because it must be a positive relative offset from the start of the memory
                // Account for necessary unwind information alignment padding (32-bit alignment)
                ((func.body.len() + 3) & !3) + info.emit_size()
            }
            _ => func.body.len(),
        }
    }

    /// Copies the data of the compiled function to the given buffer.
    ///
    /// This will also add the function to the current unwind registry.
    fn copy_function<'a>(
        func: &CompiledFunction,
        func_start: u32,
        buf: &'a mut [u8],
        registry: &mut UnwindRegistry,
    ) -> (u32, &'a mut [u8], &'a mut [VMFunctionBody]) {
        let func_len = func.body.len();
        let mut func_end = func_start + (func_len as u32);

        let (body, mut remainder) = buf.split_at_mut(func_len);
        body.copy_from_slice(&func.body);
        let vmfunc = Self::view_as_mut_vmfunc_slice(body);

        if let Some(UnwindInfo::WindowsX64(info)) = &func.unwind_info {
            // Windows unwind information is written following the function body
            // Keep unwind information 32-bit aligned (round up to the nearest 4 byte boundary)
            let unwind_start = (func_end + 3) & !3;
            let unwind_size = info.emit_size();
            let padding = (unwind_start - func_end) as usize;

            let (slice, r) = remainder.split_at_mut(padding + unwind_size);

            info.emit(&mut slice[padding..]);

            func_end = unwind_start + (unwind_size as u32);
            remainder = r;
        }

        if let Some(info) = &func.unwind_info {
            registry
                .register(func_start, func_len as u32, info)
                .expect("failed to register unwind information");
        }

        (func_end, remainder, vmfunc)
    }

    /// Convert mut a slice from u8 to VMFunctionBody.
    fn view_as_mut_vmfunc_slice(slice: &mut [u8]) -> &mut [VMFunctionBody] {
        let byte_ptr: *mut [u8] = slice;
        let body_ptr = byte_ptr as *mut [VMFunctionBody];
        unsafe { &mut *body_ptr }
    }

    /// Pushes the current entry and allocates a new one with the given size.
    fn push_current(&mut self, new_size: usize) -> Result<(), String> {
        let previous = mem::replace(
            &mut self.current,
            if new_size == 0 {
                None
            } else {
                Some(CodeMemoryEntry::with_capacity(cmp::max(0x10000, new_size))?)
            },
        );

        if let Some(e) = previous {
            self.entries.push(e);
        }

        Ok(())
    }

    /// Returns all published segment ranges.
    pub fn published_ranges<'a>(&'a self) -> impl Iterator<Item = (usize, usize)> + 'a {
        self.entries[..self.published]
            .iter()
            .map(|entry| entry.range())
    }

    /// Allocates and copies the ELF image code section into CodeMemory.
    /// Returns references to functions and trampolines defined there.
    pub(crate) fn allocate_for_object<'a>(
        &'a mut self,
        obj: &ObjectFile,
        unwind_info: &[ObjectUnwindInfo],
    ) -> Result<CodeMemoryObjectAllocation<'a>, String> {
        let text_section = obj.section_by_name(".text").unwrap();

        if text_section.size() == 0 {
            // No code in the image.
            return Ok(CodeMemoryObjectAllocation {
                buf: &mut [],
                funcs: BTreeMap::new(),
                trampolines: BTreeMap::new(),
            });
        }

        // Allocate chunk memory that spans entire code section.
        let (buf, registry, start) = self.allocate(text_section.size() as usize)?;
        buf.copy_from_slice(
            text_section
                .data()
                .map_err(|_| "cannot read section data".to_string())?,
        );

        // Track locations of all defined functions and trampolines.
        let mut funcs = BTreeMap::new();
        let mut trampolines = BTreeMap::new();
        for (_id, sym) in obj.symbols() {
            match sym.name() {
                Some(name) => {
                    if let Some(index) = try_parse_func_name(name) {
                        let is_import = sym.section_index().is_none();
                        if !is_import {
                            funcs.insert(
                                index,
                                (start + sym.address() as usize, sym.size() as usize),
                            );
                        }
                    } else if let Some(index) = try_parse_trampoline_name(name) {
                        trampolines
                            .insert(index, (start + sym.address() as usize, sym.size() as usize));
                    }
                }
                None => (),
            }
        }

        // Register all unwind entiries for functions and trampolines.
        // TODO will `u32` type for start/len be enough for large code base.
        for i in unwind_info {
            match i {
                ObjectUnwindInfo::Func(func_index, info) => {
                    let (start, len) = funcs.get(&func_index).unwrap();
                    registry
                        .register(*start as u32, *len as u32, &info)
                        .expect("failed to register unwind information");
                }
                ObjectUnwindInfo::Trampoline(trampoline_index, info) => {
                    let (start, len) = trampolines.get(&trampoline_index).unwrap();
                    registry
                        .register(*start as u32, *len as u32, &info)
                        .expect("failed to register unwind information");
                }
            }
        }

        Ok(CodeMemoryObjectAllocation {
            buf: &mut buf[..text_section.size() as usize],
            funcs,
            trampolines,
        })
    }
}
