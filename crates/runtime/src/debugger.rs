//! Debugger utils.
#![allow(missing_docs)]
use std::any::Any;
use std::sync::MutexGuard;

/// Patching the code trait
pub trait PatchableCode {
    fn patch_jit_code(&self, addr: usize, len: usize, f: &mut dyn FnMut());
}

#[derive(Debug)]
pub struct BreakpointData {
    /// Breakpoint address.
    pub pc: usize,
    data: SavedBreakpointData,
}

impl BreakpointData {
    /// Creates new BreakpointData if possible.
    pub fn new(pc: usize, patchable: &dyn PatchableCode) -> Option<Self> {
        let mut data = None;
        patchable.patch_jit_code(pc, BREAKPOINT_INSTR_LEN, &mut || {
            data = Some(unsafe { write_breakpoint_instr(pc) });
        });
        data.map(|data| BreakpointData { pc, data })
    }

    pub fn restore_code(&self, patchable: &dyn PatchableCode) {
        let pc = self.pc;
        patchable.patch_jit_code(pc, BREAKPOINT_INSTR_LEN, &mut move || unsafe {
            self.data.restore(pc);
        });
    }

    pub fn patch_code(&self, patchable: &dyn PatchableCode) {
        let pc = self.pc;
        patchable.patch_jit_code(pc, BREAKPOINT_INSTR_LEN, &mut move || unsafe {
            write_breakpoint_instr(pc);
        });
    }
}

const BREAKPOINT_INSTR: u8 = 0xCC;
const BREAKPOINT_INSTR_LEN: usize = 1;

#[derive(Debug)]
struct SavedBreakpointData(u8);

unsafe fn write_breakpoint_instr(addr: usize) -> SavedBreakpointData {
    let c = addr as *mut u8;
    let b = std::ptr::read(c);
    std::ptr::write(c, BREAKPOINT_INSTR);
    SavedBreakpointData(b)
}

impl SavedBreakpointData {
    /// Restores code changed by breakpoint.
    pub unsafe fn restore(&self, addr: usize) {
        let c = addr as *mut u8;
        std::ptr::write(c, self.0);
    }
}

#[derive(Debug, Clone)]
pub enum DebuggerPauseKind {
    Breakpoint(usize),
    Step,
}

#[derive(Debug, Clone)]
pub enum DebuggerResumeAction {
    Step,
    Continue,
}

pub type DebuggerContextData<'a, 'b> = MutexGuard<'a, Option<Box<dyn Any + Send + Sync + 'b>>>;

pub trait DebuggerContext {
    fn patchable(&self) -> &dyn PatchableCode;
    fn find_breakpoint(&self, addr: *const u8) -> Option<*const BreakpointData>;
    fn pause(&self, kind: DebuggerPauseKind) -> DebuggerResumeAction;
    fn data<'c, 'a>(&'c self) -> DebuggerContextData<'c, 'a>;

    fn as_any(&self) -> &dyn std::any::Any;
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any;
}

cfg_if::cfg_if! {
    if #[cfg(unix)] {
        unsafe fn set_stepping_flag_in_ucontext(cx: *const libc::c_void, enabled: bool) {
            let cx = &mut *(cx as *mut libc::ucontext_t);
            cfg_if::cfg_if! {
                if #[cfg(all(target_os = "linux", target_arch = "x86_64"))] {
                    let flags = &mut cx.uc_mcontext.gregs[libc::REG_EFL as usize];
                } else if #[cfg(all(target_os = "linux", target_arch = "x86"))] {
                    let flags = &mut cx.uc_mcontext.gregs[libc::REG_EFL as usize];
                } else if #[cfg(target_os = "macos")] {
                    let flags = &mut (*cx.uc_mcontext).__ss.__rflags;
                } else {
                    compile_error!("unsupported platform");
                }
            }
            if enabled {
                *flags |= 0x0100;
            } else {
                *flags &= !0x0100;
            }
        }
        unsafe fn adjust_pc_in_ucontext(cx: *const libc::c_void, delta: i64) {
            let cx = &mut *(cx as *mut libc::ucontext_t);
            cfg_if::cfg_if! {
                if #[cfg(all(target_os = "linux", target_arch = "x86_64"))] {
                    let rip = &mut cx.uc_mcontext.gregs[libc::REG_RIP as usize];
                } else if #[cfg(all(target_os = "linux", target_arch = "x86"))] {
                    let rip = &mut cx.uc_mcontext.gregs[libc::REG_EIP as usize];
                    let delta = delta as i32;
                } else if #[cfg(target_os = "macos")] {
                    let rip = &mut (*cx.uc_mcontext).__ss.__rip;
                    let delta = delta as u64;
                } else {
                    compile_error!("unsupported platform");
                }
            }
            *rip = rip.wrapping_add(delta);
        }
        unsafe fn get_pc_from_ucontext(cx: *const libc::c_void) -> *const u8 {
            let cx = &*(cx as *const libc::ucontext_t);
            cfg_if::cfg_if! {
                if #[cfg(all(target_os = "linux", target_arch = "x86_64"))] {
                    cx.uc_mcontext.gregs[libc::REG_RIP as usize] as *const u8
                } else if #[cfg(all(target_os = "linux", target_arch = "x86"))] {
                    cx.uc_mcontext.gregs[libc::REG_EIP as usize] as *const u8
                } else if #[cfg(target_os = "macos")] {
                    (*cx.uc_mcontext).__ss.__rip as *const u8
                } else {
                    compile_error!("unsupported platform");
                }
            }
        }

        struct Data {
            last: Option<*const u8>,
            stepping: bool,
        }

        unsafe impl Send for Data {}
        unsafe impl Sync for Data {}

        pub unsafe fn debugger_handler(dbg: &dyn DebuggerContext, _siginfo: *const libc::siginfo_t, context: *const libc::c_void) -> bool {
            let mut data_guard = dbg.data();
            if data_guard.is_none() {
                *data_guard = Some(Box::new(Data { last: None, stepping: false, }));
            }
            let data = data_guard.as_mut().unwrap().downcast_mut::<Data>().unwrap();

            if let Some(pc) = data.last.take() {
                // Introduce a breakpoint back.
                let b: *const _ = dbg.find_breakpoint(pc).unwrap();
                (*b).patch_code(dbg.patchable());
                if !data.stepping {
                    set_stepping_flag_in_ucontext(context, false);
                    return true;
                }
            }

            if data.stepping {
                // We are stepping.
                let action = dbg.pause(DebuggerPauseKind::Step);
                match action {
                    DebuggerResumeAction::Continue => {
                        data.stepping = false;
                        set_stepping_flag_in_ucontext(context, false);
                    }
                    DebuggerResumeAction::Step => {
                        let pc = get_pc_from_ucontext(context);
                        if let Some(ptr) = dbg.find_breakpoint(pc) {
                            (*ptr).restore_code(dbg.patchable());
                            data.last = Some(pc);
                        }
                    },
                }
                return true;
            }
            let pc_adj: isize = -(BREAKPOINT_INSTR_LEN as isize);
            let pc = get_pc_from_ucontext(context).offset(pc_adj);
            if let Some(ptr) = dbg.find_breakpoint(pc) {
                let action = dbg.pause(DebuggerPauseKind::Breakpoint(pc as usize));
                (*ptr).restore_code(dbg.patchable());
                adjust_pc_in_ucontext(context, pc_adj as i64);
                set_stepping_flag_in_ucontext(context, true);
                data.last = Some(pc);

                match action {
                    DebuggerResumeAction::Step => {
                        data.stepping = true;
                    }
                    DebuggerResumeAction::Continue => {
                        data.stepping = false;
                    }
                }
                true
            } else {
                false
            }
        }
    }
}
