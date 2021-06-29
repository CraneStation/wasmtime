//! Memory management for linear memories.
//!
//! `RuntimeLinearMemory` is to WebAssembly linear memories what `Table` is to WebAssembly tables.

use crate::mmap::Mmap;
use crate::vmcontext::VMMemoryDefinition;
use crate::ResourceLimiter;
use anyhow::{bail, Result};
use more_asserts::{assert_ge, assert_le};
use std::convert::TryFrom;
use wasmtime_environ::{MemoryPlan, MemoryStyle, WASM_MAX_PAGES, WASM_PAGE_SIZE};

/// A memory allocator
pub trait RuntimeMemoryCreator: Send + Sync {
    /// Create new RuntimeLinearMemory
    fn new_memory(&self, plan: &MemoryPlan) -> Result<Box<dyn RuntimeLinearMemory>>;
}

/// A default memory allocator used by Wasmtime
pub struct DefaultMemoryCreator;

impl RuntimeMemoryCreator for DefaultMemoryCreator {
    /// Create new MmapMemory
    fn new_memory(&self, plan: &MemoryPlan) -> Result<Box<dyn RuntimeLinearMemory>> {
        Ok(Box::new(MmapMemory::new(plan)?) as _)
    }
}

/// A linear memory
pub trait RuntimeLinearMemory: Send + Sync {
    /// Returns the number of allocated wasm pages.
    fn size(&self) -> u32;

    /// Returns the maximum number of pages the memory can grow to.
    /// Returns `None` if the memory is unbounded.
    fn maximum(&self) -> Option<u32>;

    /// Grow memory by the specified amount of wasm pages.
    ///
    /// Returns `None` if memory can't be grown by the specified amount
    /// of wasm pages.
    fn grow(&mut self, delta: u32) -> Option<u32>;

    /// Return a `VMMemoryDefinition` for exposing the memory to compiled wasm code.
    fn vmmemory(&self) -> VMMemoryDefinition;
}

/// A linear memory instance.
#[derive(Debug)]
pub struct MmapMemory {
    // The underlying allocation.
    mmap: WasmMmap,

    // The optional maximum size in wasm pages of this linear memory.
    maximum: Option<u32>,

    // Size in bytes of extra guard pages before the start and after the end to
    // optimize loads and stores with constant offsets.
    pre_guard_size: usize,
    offset_guard_size: usize,
}

#[derive(Debug)]
struct WasmMmap {
    // Our OS allocation of mmap'd memory.
    alloc: Mmap,
    // The current logical size in wasm pages of this linear memory.
    size: u32,
}

impl MmapMemory {
    /// Create a new linear memory instance with specified minimum and maximum number of wasm pages.
    pub fn new(plan: &MemoryPlan) -> Result<Self> {
        // `maximum` cannot be set to more than `65536` pages.
        assert_le!(plan.memory.minimum, WASM_MAX_PAGES);
        assert!(plan.memory.maximum.is_none() || plan.memory.maximum.unwrap() <= WASM_MAX_PAGES);

        let offset_guard_bytes = plan.offset_guard_size as usize;
        let pre_guard_bytes = plan.pre_guard_size as usize;

        let minimum_pages = match plan.style {
            MemoryStyle::Dynamic => plan.memory.minimum,
            MemoryStyle::Static { bound } => {
                assert_ge!(bound, plan.memory.minimum);
                bound
            }
        } as usize;
        let minimum_bytes = minimum_pages.checked_mul(WASM_PAGE_SIZE as usize).unwrap();
        let request_bytes = pre_guard_bytes
            .checked_add(minimum_bytes)
            .unwrap()
            .checked_add(offset_guard_bytes)
            .unwrap();
        let mapped_pages = plan.memory.minimum as usize;
        let accessible_bytes = mapped_pages * WASM_PAGE_SIZE as usize;

        let mut mmap = WasmMmap {
            alloc: Mmap::accessible_reserved(0, request_bytes, /* is_stack = */ false)?,
            size: plan.memory.minimum,
        };
        if accessible_bytes > 0 {
            mmap.alloc
                .make_accessible(pre_guard_bytes, accessible_bytes)?;
        }

        Ok(Self {
            mmap: mmap.into(),
            maximum: plan.memory.maximum,
            pre_guard_size: pre_guard_bytes,
            offset_guard_size: offset_guard_bytes,
        })
    }
}

impl RuntimeLinearMemory for MmapMemory {
    /// Returns the number of allocated wasm pages.
    fn size(&self) -> u32 {
        self.mmap.size
    }

    /// Returns the maximum number of pages the memory can grow to.
    /// Returns `None` if the memory is unbounded.
    fn maximum(&self) -> Option<u32> {
        self.maximum
    }

    /// Grow memory by the specified amount of wasm pages.
    ///
    /// Returns `None` if memory can't be grown by the specified amount
    /// of wasm pages.
    fn grow(&mut self, delta: u32) -> Option<u32> {
        // Optimization of memory.grow 0 calls.
        if delta == 0 {
            return Some(self.mmap.size);
        }

        let new_pages = match self.mmap.size.checked_add(delta) {
            Some(new_pages) => new_pages,
            // Linear memory size overflow.
            None => return None,
        };
        let prev_pages = self.mmap.size;

        if let Some(maximum) = self.maximum {
            if new_pages > maximum {
                // Linear memory size would exceed the declared maximum.
                return None;
            }
        }

        // Wasm linear memories are never allowed to grow beyond what is
        // indexable. If the memory has no maximum, enforce the greatest
        // limit here.
        if new_pages > WASM_MAX_PAGES {
            // Linear memory size would exceed the index range.
            return None;
        }
        // FIXME: https://github.com/bytecodealliance/wasmtime/issues/3022
        if new_pages == WASM_MAX_PAGES {
            return None;
        }

        let delta_bytes = usize::try_from(delta).unwrap() * WASM_PAGE_SIZE as usize;
        let prev_bytes = usize::try_from(prev_pages).unwrap() * WASM_PAGE_SIZE as usize;
        let new_bytes = usize::try_from(new_pages).unwrap() * WASM_PAGE_SIZE as usize;

        if new_bytes > self.mmap.alloc.len() - self.offset_guard_size - self.pre_guard_size {
            // If the new size is within the declared maximum, but needs more memory than we
            // have on hand, it's a dynamic heap and it can move.
            let request_bytes = self
                .pre_guard_size
                .checked_add(new_bytes)?
                .checked_add(self.offset_guard_size)?;

            let mut new_mmap =
                Mmap::accessible_reserved(0, request_bytes, /* is_stack = */ false).ok()?;
            new_mmap
                .make_accessible(self.pre_guard_size, new_bytes)
                .ok()?;

            new_mmap.as_mut_slice()[self.pre_guard_size..][..prev_bytes]
                .copy_from_slice(&self.mmap.alloc.as_slice()[self.pre_guard_size..][..prev_bytes]);

            self.mmap.alloc = new_mmap;
        } else if delta_bytes > 0 {
            // Make the newly allocated pages accessible.
            self.mmap
                .alloc
                .make_accessible(self.pre_guard_size + prev_bytes, delta_bytes)
                .ok()?;
        }

        self.mmap.size = new_pages;

        Some(prev_pages)
    }

    /// Return a `VMMemoryDefinition` for exposing the memory to compiled wasm code.
    fn vmmemory(&self) -> VMMemoryDefinition {
        VMMemoryDefinition {
            base: unsafe { self.mmap.alloc.as_mut_ptr().add(self.pre_guard_size) },
            current_length: u32::try_from(self.mmap.size as usize * WASM_PAGE_SIZE as usize)
                .unwrap(),
        }
    }
}

/// Representation of a runtime wasm linear memory.
pub enum Memory {
    /// A "static" memory where the lifetime of the backing memory is managed
    /// elsewhere. Currently used with the pooling allocator.
    Static {
        /// The memory in the host for this wasm memory. The length of this
        /// slice is the maximum size of the memory that can be grown to.
        base: &'static mut [u8],

        /// The current size, in wasm pages, of this memory.
        size: u32,

        /// A callback which makes portions of `base` accessible for when memory
        /// is grown. Otherwise it's expected that accesses to `base` will
        /// fault.
        make_accessible: fn(*mut u8, usize) -> Result<()>,

        /// Stores the pages in the linear memory that have faulted as guard pages when using the `uffd` feature.
        /// These pages need their protection level reset before the memory can grow.
        #[cfg(all(feature = "uffd", target_os = "linux"))]
        guard_page_faults: Vec<(usize, usize, fn(*mut u8, usize) -> Result<()>)>,
    },

    /// A "dynamic" memory whose data is managed at runtime and lifetime is tied
    /// to this instance.
    Dynamic(Box<dyn RuntimeLinearMemory>),
}

impl Memory {
    /// Create a new dynamic (movable) memory instance for the specified plan.
    pub fn new_dynamic(
        plan: &MemoryPlan,
        creator: &dyn RuntimeMemoryCreator,
        limiter: Option<&mut dyn ResourceLimiter>,
    ) -> Result<Self> {
        Self::limit_new(plan, limiter)?;
        Ok(Memory::Dynamic(creator.new_memory(plan)?))
    }

    /// Create a new static (immovable) memory instance for the specified plan.
    pub fn new_static(
        plan: &MemoryPlan,
        base: &'static mut [u8],
        make_accessible: fn(*mut u8, usize) -> Result<()>,
        limiter: Option<&mut dyn ResourceLimiter>,
    ) -> Result<Self> {
        Self::limit_new(plan, limiter)?;

        let base = match plan.memory.maximum {
            Some(max) if (max as usize) < base.len() / (WASM_PAGE_SIZE as usize) => {
                &mut base[..(max * WASM_PAGE_SIZE) as usize]
            }
            _ => base,
        };

        if plan.memory.minimum > 0 {
            make_accessible(
                base.as_mut_ptr(),
                plan.memory.minimum as usize * WASM_PAGE_SIZE as usize,
            )?;
        }

        Ok(Memory::Static {
            base,
            size: plan.memory.minimum,
            make_accessible,
            #[cfg(all(feature = "uffd", target_os = "linux"))]
            guard_page_faults: Vec::new(),
        })
    }

    fn limit_new(plan: &MemoryPlan, limiter: Option<&mut dyn ResourceLimiter>) -> Result<()> {
        // FIXME: https://github.com/bytecodealliance/wasmtime/issues/3022
        if plan.memory.minimum == WASM_MAX_PAGES {
            bail!(
                "memory minimum size of {} pages exceeds memory limits",
                plan.memory.minimum
            );
        }
        if let Some(limiter) = limiter {
            if !limiter.memory_growing(0, plan.memory.minimum, plan.memory.maximum) {
                bail!(
                    "memory minimum size of {} pages exceeds memory limits",
                    plan.memory.minimum
                );
            }
        }
        Ok(())
    }

    /// Returns the number of allocated wasm pages.
    pub fn size(&self) -> u32 {
        match self {
            Memory::Static { size, .. } => *size,
            Memory::Dynamic(mem) => mem.size(),
        }
    }

    /// Returns the maximum number of pages the memory can grow to at runtime.
    ///
    /// Returns `None` if the memory is unbounded.
    ///
    /// The runtime maximum may not be equal to the maximum from the linear memory's
    /// Wasm type when it is being constrained by an instance allocator.
    pub fn maximum(&self) -> Option<u32> {
        match self {
            Memory::Static { base, .. } => Some((base.len() / (WASM_PAGE_SIZE as usize)) as u32),
            Memory::Dynamic(mem) => mem.maximum(),
        }
    }

    /// Returns whether or not the underlying storage of the memory is "static".
    pub(crate) fn is_static(&self) -> bool {
        if let Memory::Static { .. } = self {
            true
        } else {
            false
        }
    }

    /// Grow memory by the specified amount of wasm pages.
    ///
    /// Returns `None` if memory can't be grown by the specified amount
    /// of wasm pages.
    ///
    /// # Safety
    ///
    /// Resizing the memory can reallocate the memory buffer for dynamic memories.
    /// An instance's `VMContext` may have pointers to the memory's base and will
    /// need to be fixed up after growing the memory.
    ///
    /// Generally, prefer using `InstanceHandle::memory_grow`, which encapsulates
    /// this unsafety.
    pub unsafe fn grow(
        &mut self,
        delta: u32,
        limiter: Option<&mut dyn ResourceLimiter>,
    ) -> Option<u32> {
        let old_size = self.size();
        if delta == 0 {
            return Some(old_size);
        }

        let new_size = old_size.checked_add(delta)?;
        let maximum = self.maximum();

        if let Some(limiter) = limiter {
            if !limiter.memory_growing(old_size, new_size, maximum) {
                return None;
            }
        }

        #[cfg(all(feature = "uffd", target_os = "linux"))]
        {
            if self.is_static() {
                // Reset any faulted guard pages before growing the memory.
                self.reset_guard_pages().ok()?;
            }
        }

        match self {
            Memory::Static {
                base,
                size,
                make_accessible,
                ..
            } => {
                if new_size > maximum.unwrap_or(WASM_MAX_PAGES) {
                    return None;
                }
                // FIXME: https://github.com/bytecodealliance/wasmtime/issues/3022
                if new_size == WASM_MAX_PAGES {
                    return None;
                }

                let start = usize::try_from(old_size).unwrap() * WASM_PAGE_SIZE as usize;
                let len = usize::try_from(delta).unwrap() * WASM_PAGE_SIZE as usize;

                make_accessible(base.as_mut_ptr().add(start), len).ok()?;

                *size = new_size;

                Some(old_size)
            }
            Memory::Dynamic(mem) => mem.grow(delta),
        }
    }

    /// Return a `VMMemoryDefinition` for exposing the memory to compiled wasm code.
    pub fn vmmemory(&self) -> VMMemoryDefinition {
        match self {
            Memory::Static { base, size, .. } => VMMemoryDefinition {
                base: base.as_ptr() as *mut _,
                current_length: u32::try_from(*size as usize * WASM_PAGE_SIZE as usize).unwrap(),
            },
            Memory::Dynamic(mem) => mem.vmmemory(),
        }
    }

    /// Records a faulted guard page in a static memory.
    ///
    /// This is used to track faulted guard pages that need to be reset for the uffd feature.
    ///
    /// This function will panic if called on a dynamic memory.
    #[cfg(all(feature = "uffd", target_os = "linux"))]
    pub(crate) fn record_guard_page_fault(
        &mut self,
        page_addr: *mut u8,
        size: usize,
        reset: fn(*mut u8, usize) -> Result<()>,
    ) {
        match self {
            Memory::Static {
                guard_page_faults, ..
            } => {
                guard_page_faults.push((page_addr as usize, size, reset));
            }
            Memory::Dynamic(_) => {
                unreachable!("dynamic memories should not have guard page faults")
            }
        }
    }

    /// Resets the previously faulted guard pages of a static memory.
    ///
    /// This is used to reset the protection of any guard pages that were previously faulted.
    ///
    /// This function will panic if called on a dynamic memory.
    #[cfg(all(feature = "uffd", target_os = "linux"))]
    pub(crate) fn reset_guard_pages(&mut self) -> Result<()> {
        match self {
            Memory::Static {
                guard_page_faults, ..
            } => {
                for (addr, len, reset) in guard_page_faults.drain(..) {
                    reset(addr as *mut u8, len)?;
                }
            }
            Memory::Dynamic(_) => {
                unreachable!("dynamic memories should not have guard page faults")
            }
        }

        Ok(())
    }
}

// The default memory representation is an empty memory that cannot grow.
impl Default for Memory {
    fn default() -> Self {
        Memory::Static {
            base: &mut [],
            size: 0,
            make_accessible: |_, _| unreachable!(),
            #[cfg(all(feature = "uffd", target_os = "linux"))]
            guard_page_faults: Vec::new(),
        }
    }
}
