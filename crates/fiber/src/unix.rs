//! The unix fiber implementation has some platform-specific details
//! (naturally) but there's a few details of the stack layout which are common
//! amongst all platforms using this file. Remember that none of this applies to
//! Windows, which is entirely separate.
//!
//! The stack is expected to look pretty standard with a guard page at the end.
//! Currently allocation happens in this file but this is probably going to be
//! refactored to happen somewhere else. Otherwise though the stack layout is
//! expected to look like so:
//!
//!
//! ```text
//! 0xB000 +-----------------------+   <- top of stack
//!        | &Cell<RunResult>      |   <- where to store results
//! 0xAff8 +-----------------------+
//!        | *const u8             |   <- last sp to resume from
//! 0xAff0 +-----------------------+   <- 16-byte aligned
//!        |                       |
//!        ~        ...            ~   <- actual native stack space to use
//!        |                       |
//! 0x1000 +-----------------------+
//!        |  guard page           |
//! 0x0000 +-----------------------+
//! ```
//!
//! Here `0xAff8` is filled in temporarily while `resume` is running. The fiber
//! started with 0xB000 as a parameter so it knows how to find this.
//! Additionally `resumes` stores state at 0xAff0 to restart execution, and
//! `suspend`, which has 0xB000 so it can find this, will read that and write
//! its own resumption information into this slot as well.

use crate::RunResult;
use std::cell::Cell;
use std::io;
use std::ptr;

#[derive(Debug)]
pub struct FiberStack {
    // The top of the stack; for stacks allocated by the fiber implementation itself,
    // the base address of the allocation will be `top.sub(len.unwrap())`
    top: *mut u8,
    // The length of the stack; `None` when the stack was not created by this implementation.
    len: Option<usize>,
}

/// Maps a new stack with a guard page. Returns (top, len).
#[cfg(not(target_os = "openbsd"))]
unsafe fn mmap_new_stack(size: usize, page_size: usize) -> io::Result<(*mut u8, usize)> {
    // Add in one page for a guard page and then ask for some memory.
    let mmap_len = size + page_size;
    let mmap = libc::mmap(
        ptr::null_mut(),
        mmap_len,
        libc::PROT_NONE,
        libc::MAP_ANON | libc::MAP_PRIVATE,
        -1,
        0,
    );
    if mmap == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }

    if libc::mprotect(
        mmap.cast::<u8>().add(page_size).cast(),
        size,
        libc::PROT_READ | libc::PROT_WRITE,
    ) != 0
    {
        return Err(io::Error::last_os_error());
    }

    Ok((mmap.cast::<u8>().add(mmap_len).cast(), mmap_len))
}

/// Maps a new stack with a guard page. Returns (top, len).
#[cfg(target_os = "openbsd")]
unsafe fn mmap_new_stack(size: usize, page_size: usize) -> io::Result<(*mut u8, usize)> {
    // On OpenBSD, we need to use MAP_STACK to specify that a page can contain a
    // stack; otherwise, we will get a SIGSEGV when it is used as such.
    //
    // Note also that MAP_STACK must be specified with PROT_READ|PROT_WRITE, and
    // must be specified without a fixed location (see
    // /usr/src/sys/uvm/uvm_mmap.c:sys_mmap() flag validation), so we cannot do
    // the mmap/mprotect trick as above to get a guard page and we cannot
    // munmap/mmap to set MAP_STACK in a fixed location. Instead, we need to (i)
    // do a mmap to get the stack at the kernel's choice of location, and then
    // (ii) mprotect the lowest page to get a guard page.

    const MAP_STACK: libc::c_int = 0x4000; // from <sys/mman.h>, not in `libc`.

    // Add in one page for a guard page and then ask for some memory.
    let mmap_len = size + page_size;
    let mmap = libc::mmap(
        ptr::null_mut(),
        mmap_len,
        libc::PROT_READ | libc::PROT_WRITE,
        libc::MAP_ANON | libc::MAP_PRIVATE | MAP_STACK,
        -1,
        0,
    );
    if mmap == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }

    if libc::mprotect(mmap, page_size, libc::PROT_NONE) != 0 {
        return Err(io::Error::last_os_error());
    }

    Ok((mmap.cast::<u8>().add(mmap_len).cast(), mmap_len))
}

impl FiberStack {
    pub fn new(size: usize) -> io::Result<Self> {
        unsafe {
            // Round up our stack size request to the nearest multiple of the
            // page size.
            let page_size = libc::sysconf(libc::_SC_PAGESIZE) as usize;
            let size = if size == 0 {
                page_size
            } else {
                (size + (page_size - 1)) & (!(page_size - 1))
            };

            let (top, len) = mmap_new_stack(size, page_size)?;

            Ok(Self {
                top,
                len: Some(len),
            })
        }
    }

    pub unsafe fn from_top_ptr(top: *mut u8) -> io::Result<Self> {
        Ok(Self { top, len: None })
    }

    pub fn top(&self) -> Option<*mut u8> {
        Some(self.top)
    }
}

impl Drop for FiberStack {
    fn drop(&mut self) {
        unsafe {
            if let Some(len) = self.len {
                let ret = libc::munmap(self.top.sub(len) as _, len);
                debug_assert!(ret == 0);
            }
        }
    }
}

pub struct Fiber;

pub struct Suspend(*mut u8);

extern "C" {
    fn wasmtime_fiber_init(
        top_of_stack: *mut u8,
        entry: extern "C" fn(*mut u8, *mut u8),
        entry_arg0: *mut u8,
    );
    fn wasmtime_fiber_switch(top_of_stack: *mut u8);
}

extern "C" fn fiber_start<F, A, B, C>(arg0: *mut u8, top_of_stack: *mut u8)
where
    F: FnOnce(A, &super::Suspend<A, B, C>) -> C,
{
    unsafe {
        let inner = Suspend(top_of_stack);
        let initial = inner.take_resume::<A, B, C>();
        super::Suspend::<A, B, C>::execute(inner, initial, Box::from_raw(arg0.cast::<F>()))
    }
}

impl Fiber {
    pub fn new<F, A, B, C>(stack: &FiberStack, func: F) -> io::Result<Self>
    where
        F: FnOnce(A, &super::Suspend<A, B, C>) -> C,
    {
        unsafe {
            let data = Box::into_raw(Box::new(func)).cast();
            wasmtime_fiber_init(stack.top, fiber_start::<F, A, B, C>, data);
        }

        Ok(Self)
    }

    pub(crate) fn resume<A, B, C>(&self, stack: &FiberStack, result: &Cell<RunResult<A, B, C>>) {
        unsafe {
            // Store where our result is going at the very tip-top of the
            // stack, otherwise known as our reserved slot for this information.
            //
            // In the diagram above this is updating address 0xAff8
            let addr = stack.top.cast::<usize>().offset(-1);
            addr.write(result as *const _ as usize);

            wasmtime_fiber_switch(stack.top);

            // null this out to help catch use-after-free
            addr.write(0);
        }
    }
}

impl Suspend {
    pub(crate) fn switch<A, B, C>(&self, result: RunResult<A, B, C>) -> A {
        unsafe {
            // Calculate 0xAff8 and then write to it
            (*self.result_location::<A, B, C>()).set(result);
            wasmtime_fiber_switch(self.0);
            self.take_resume::<A, B, C>()
        }
    }

    unsafe fn take_resume<A, B, C>(&self) -> A {
        match (*self.result_location::<A, B, C>()).replace(RunResult::Executing) {
            RunResult::Resuming(val) => val,
            _ => panic!("not in resuming state"),
        }
    }

    unsafe fn result_location<A, B, C>(&self) -> *const Cell<RunResult<A, B, C>> {
        let ret = self.0.cast::<*const u8>().offset(-1).read();
        assert!(!ret.is_null());
        ret.cast()
    }
}
