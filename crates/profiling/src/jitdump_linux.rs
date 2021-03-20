//! Support for jitdump files which can be used by perf for profiling jitted code.
//! Spec definitions for the output format is as described here:
//! <https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/tools/perf/Documentation/jitdump-specification.txt>
//!
//! Usage Example:
//!     Record
//!         sudo perf record -k 1 -e instructions:u target/debug/wasmtime -g --jitdump test.wasm
//!     Combine
//!         sudo perf inject -v -j -i perf.data -o perf.jit.data
//!     Report
//!         sudo perf report -i perf.jit.data -F+period,srcline
//! Note: For descriptive results, the WASM file being executed should contain dwarf debug data

use crate::ProfilingAgent;
use anyhow::Result;
use object::{Object, ObjectSection};
use scroll::{IOwrite, SizeWith, NATIVE};
use serde::{Deserialize, Serialize};
use std::fmt::Debug;
use std::fs::{File, OpenOptions};
use std::io;
use std::io::Write;
use std::os::unix::prelude::*;
use std::ptr;
use std::sync::Mutex;
use std::{borrow, mem, process};
use target_lexicon::Architecture;
use wasmtime_environ::entity::PrimaryMap;
use wasmtime_environ::wasm::DefinedFuncIndex;
use wasmtime_environ::Module;
use wasmtime_runtime::VMFunctionBody;

use object::elf;

/// Defines jitdump record types
#[repr(u32)]
pub enum RecordId {
    /// Value 0: JIT_CODE_LOAD: record describing a jitted function
    JitCodeLoad = 0,
    /// Value 1: JIT_CODE_MOVE: record describing an already jitted function which is moved
    _JitCodeMove = 1,
    /// Value 2: JIT_CODE_DEBUG_INFO: record describing the debug information for a jitted function
    JitCodeDebugInfo = 2,
    /// Value 3: JIT_CODE_CLOSE: record marking the end of the jit runtime (optional)
    _JitCodeClose = 3,
    /// Value 4: JIT_CODE_UNWINDING_INFO: record describing a function unwinding information
    _JitCodeUnwindingInfo = 4,
}

/// Each record starts with this fixed size record header which describes the record that follows
#[derive(Serialize, Deserialize, Debug, Default, Clone, Copy, IOwrite, SizeWith)]
#[repr(C)]
pub struct RecordHeader {
    /// uint32_t id: a value identifying the record type (see below)
    id: u32,
    /// uint32_t total_size: the size in bytes of the record including the header.
    record_size: u32,
    /// uint64_t timestamp: a timestamp of when the record was created.
    timestamp: u64,
}

/// The CodeLoadRecord is used for describing jitted functions
#[derive(Serialize, Deserialize, Debug, Default, Clone, Copy, IOwrite, SizeWith)]
#[repr(C)]
pub struct CodeLoadRecord {
    /// Fixed sized header that describes this record
    header: RecordHeader,
    /// uint32_t pid: OS process id of the runtime generating the jitted code
    pid: u32,
    /// uint32_t tid: OS thread identification of the runtime thread generating the jitted code
    tid: u32,
    /// uint64_t vma: virtual address of jitted code start
    virtual_address: u64,
    /// uint64_t code_addr: code start address for the jitted code. By default vma = code_addr
    address: u64,
    /// uint64_t code_size: size in bytes of the generated jitted code
    size: u64,
    /// uint64_t code_index: unique identifier for the jitted code (see below)
    index: u64,
}

/// Describes source line information for a jitted function
#[derive(Serialize, Deserialize, Debug, Default)]
#[repr(C)]
pub struct DebugEntry {
    /// uint64_t code_addr: address of function for which the debug information is generated
    address: u64,
    /// uint32_t line: source file line number (starting at 1)
    line: u32,
    /// uint32_t discrim: column discriminator, 0 is default
    discriminator: u32,
    /// char name[n]: source file name in ASCII, including null termination
    filename: String,
}

/// Describes debug information for a jitted function. An array of debug entries are
/// appended to this record during writting. Note, this record must preceed the code
/// load record that describes the same jitted function.
#[derive(Serialize, Deserialize, Debug, Default, Clone, Copy, IOwrite, SizeWith)]
#[repr(C)]
pub struct DebugInfoRecord {
    /// Fixed sized header that describes this record
    header: RecordHeader,
    /// uint64_t code_addr: address of function for which the debug information is generated
    address: u64,
    /// uint64_t nr_entry: number of debug entries for the function appended to this record
    count: u64,
}

/// Fixed-sized header for each jitdump file
#[derive(Serialize, Deserialize, Debug, Default, IOwrite, SizeWith)]
#[repr(C)]
pub struct FileHeader {
    /// uint32_t magic: a magic number tagging the file type. The value is 4-byte long and represents the
    /// string "JiTD" in ASCII form. It is 0x4A695444 or 0x4454694a depending on the endianness. The field can
    /// be used to detect the endianness of the file
    magic: u32,
    /// uint32_t version: a 4-byte value representing the format version. It is currently set to 2
    version: u32,
    /// uint32_t total_size: size in bytes of file header
    size: u32,
    /// uint32_t elf_mach: ELF architecture encoding (ELF e_machine value as specified in /usr/include/elf.h)
    e_machine: u32,
    /// uint32_t pad1: padding. Reserved for future use
    pad1: u32,
    /// uint32_t pid: JIT runtime process identification (OS specific)
    pid: u32,
    /// uint64_t timestamp: timestamp of when the file was created
    timestamp: u64,
    /// uint64_t flags: a bitmask of flags
    flags: u64,
}

/// Interface for driving the creation of jitdump files
pub struct JitDumpAgent {
    // Note that we use a mutex internally to serialize writing out to our
    // `jitdump_file` within this process, since multiple threads may be sharing
    // this jit agent.
    state: Mutex<State>,
}

struct State {
    /// File instance for the jit dump file
    jitdump_file: File,

    map_addr: usize,

    /// Unique identifier for jitted code
    code_index: u64,

    /// Flag for experimenting with dumping code load record
    /// after each function (true) or after each module. This
    /// flag is currently set to true.
    dump_funcs: bool,
}

impl JitDumpAgent {
    /// Intialize a JitDumpAgent and write out the header
    pub fn new() -> Result<Self> {
        let filename = format!("./jit-{}.dump", process::id());
        let jitdump_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&filename)?;

        // After we make our `*.dump` file we execute an `mmap` syscall,
        // specifically with executable permissions, to map it into our address
        // space. This is required so `perf inject` will work later. The `perf
        // inject` command will see that an mmap syscall happened, and it'll see
        // the filename we mapped, and that'll trigger it to actually read and
        // parse the file.
        //
        // To match what some perf examples are doing we keep this `mmap` alive
        // until this agent goes away.
        let map_addr = unsafe {
            let ptr = libc::mmap(
                ptr::null_mut(),
                libc::sysconf(libc::_SC_PAGESIZE) as usize,
                libc::PROT_EXEC | libc::PROT_READ,
                libc::MAP_PRIVATE,
                jitdump_file.as_raw_fd(),
                0,
            );
            if ptr == libc::MAP_FAILED {
                return Err(io::Error::last_os_error().into());
            }
            ptr as usize
        };
        let mut state = State {
            jitdump_file,
            map_addr,
            code_index: 0,
            dump_funcs: true,
        };
        state.write_file_header()?;
        Ok(JitDumpAgent {
            state: Mutex::new(state),
        })
    }
}

impl ProfilingAgent for JitDumpAgent {
    fn module_load(
        &self,
        module: &Module,
        functions: &PrimaryMap<DefinedFuncIndex, *mut [VMFunctionBody]>,
        dbg_image: Option<&[u8]>,
    ) {
        self.state
            .lock()
            .unwrap()
            .module_load(module, functions, dbg_image);
    }
}

impl State {
    /// Returns timestamp from a single source
    fn get_time_stamp(&self) -> u64 {
        // We need to use `CLOCK_MONOTONIC` on Linux which is what `Instant`
        // conveniently also uses, but `Instant` doesn't allow us to get access
        // to nanoseconds as an internal detail, so we calculate the nanoseconds
        // ourselves here.
        unsafe {
            let mut ts = mem::MaybeUninit::zeroed();
            assert_eq!(
                libc::clock_gettime(libc::CLOCK_MONOTONIC, ts.as_mut_ptr()),
                0
            );
            let ts = ts.assume_init();
            // TODO: What does it mean for either sec or nsec to be negative?
            (ts.tv_sec * 1_000_000_000 + ts.tv_nsec) as u64
        }
    }

    /// Returns the ELF machine architecture.
    fn get_e_machine(&self) -> u32 {
        match target_lexicon::HOST.architecture {
            Architecture::X86_64 => elf::EM_X86_64 as u32,
            Architecture::X86_32(_) => elf::EM_386 as u32,
            Architecture::Arm(_) => elf::EM_ARM as u32,
            Architecture::Aarch64(_) => elf::EM_AARCH64 as u32,
            _ => unimplemented!("unrecognized architecture"),
        }
    }

    fn write_file_header(&mut self) -> Result<()> {
        let header = FileHeader {
            timestamp: self.get_time_stamp(),
            e_machine: self.get_e_machine(),
            magic: 0x4A695444,
            version: 1,
            size: mem::size_of::<FileHeader>() as u32,
            pad1: 0,
            pid: process::id(),
            flags: 0,
        };

        self.jitdump_file.iowrite_with(header, NATIVE)?;
        Ok(())
    }

    fn write_code_load_record(
        &mut self,
        record_name: &str,
        cl_record: CodeLoadRecord,
        code_buffer: &[u8],
    ) -> Result<()> {
        self.jitdump_file.iowrite_with(cl_record, NATIVE)?;
        self.jitdump_file.write_all(record_name.as_bytes())?;
        self.jitdump_file.write_all(b"\0")?;
        self.jitdump_file.write_all(code_buffer)?;
        Ok(())
    }

    /// Write DebugInfoRecord to open jit dump file.
    /// Must be written before the corresponding CodeLoadRecord.
    fn write_debug_info_record(&mut self, dir_record: DebugInfoRecord) -> Result<()> {
        self.jitdump_file.iowrite_with(dir_record, NATIVE)?;
        Ok(())
    }

    /// Write DebugInfoRecord to open jit dump file.
    /// Must be written before the corresponding CodeLoadRecord.
    fn write_debug_info_entries(&mut self, die_entries: Vec<DebugEntry>) -> Result<()> {
        for entry in die_entries.iter() {
            self.jitdump_file.iowrite_with(entry.address, NATIVE)?;
            self.jitdump_file.iowrite_with(entry.line, NATIVE)?;
            self.jitdump_file
                .iowrite_with(entry.discriminator, NATIVE)?;
            self.jitdump_file.write_all(entry.filename.as_bytes())?;
            self.jitdump_file.write_all(b"\0")?;
        }
        Ok(())
    }

    /// Sent when a method is compiled and loaded into memory by the VM.
    pub fn module_load(
        &mut self,
        module: &Module,
        functions: &PrimaryMap<DefinedFuncIndex, *mut [VMFunctionBody]>,
        dbg_image: Option<&[u8]>,
    ) -> () {
        let pid = process::id();
        let tid = pid; // ThreadId does appear to track underlying thread. Using PID.

        for (idx, func) in functions.iter() {
            let code_buffer = unsafe { &*(*func as *mut [u8]) };
            if let Some(img) = &dbg_image {
                if let Err(err) = self.dump_from_debug_image(img, "wasm", code_buffer, pid, tid) {
                    println!(
                        "Jitdump: module_load failed dumping from debug image: {:?}\n",
                        err
                    );
                }
            } else {
                let timestamp = self.get_time_stamp();
                let name = super::debug_name(module, idx);
                self.dump_code_load_record(&name, code_buffer, timestamp, pid, tid);
            }
        }
    }

    fn dump_code_load_record(
        &mut self,
        method_name: &str,
        code_buffer: &[u8],
        timestamp: u64,
        pid: u32,
        tid: u32,
    ) -> () {
        let name_len = method_name.len() + 1;
        let size_limit = mem::size_of::<CodeLoadRecord>();

        let rh = RecordHeader {
            id: RecordId::JitCodeLoad as u32,
            record_size: size_limit as u32 + name_len as u32 + code_buffer.len() as u32,
            timestamp,
        };

        let clr = CodeLoadRecord {
            header: rh,
            pid,
            tid,
            virtual_address: code_buffer.as_ptr() as u64,
            address: code_buffer.as_ptr() as u64,
            size: code_buffer.len() as u64,
            index: self.code_index,
        };
        self.code_index += 1;

        if let Err(err) = self.write_code_load_record(method_name, clr, code_buffer) {
            println!("Jitdump: write_code_load_failed_record failed: {:?}\n", err);
        }
    }

    /// Attempts to dump debuginfo data structures, adding method and line level
    /// for the jitted function.
    pub fn dump_from_debug_image(
        &mut self,
        dbg_image: &[u8],
        module_name: &str,
        code_buffer: &[u8],
        pid: u32,
        tid: u32,
    ) -> Result<()> {
        let file = object::File::parse(&dbg_image).unwrap();
        let endian = if file.is_little_endian() {
            gimli::RunTimeEndian::Little
        } else {
            gimli::RunTimeEndian::Big
        };

        let load_section = |id: gimli::SectionId| -> Result<borrow::Cow<[u8]>> {
            if let Some(section) = file.section_by_name(id.name()) {
                Ok(section.data()?.into())
            } else {
                Ok((&[] as &[u8]).into())
            }
        };

        let load_section_sup = |_| Ok(borrow::Cow::Borrowed(&[][..]));
        let dwarf_cow = gimli::Dwarf::load(&load_section, &load_section_sup)?;
        let borrow_section: &dyn for<'a> Fn(
            &'a borrow::Cow<[u8]>,
        )
            -> gimli::EndianSlice<'a, gimli::RunTimeEndian> =
            &|section| gimli::EndianSlice::new(&*section, endian);

        let dwarf = dwarf_cow.borrow(&borrow_section);

        let mut iter = dwarf.units();
        while let Some(header) = iter.next()? {
            let unit = match dwarf.unit(header) {
                Ok(unit) => unit,
                Err(_err) => {
                    return Ok(());
                }
            };
            self.dump_entries(unit, &dwarf, module_name, code_buffer, pid, tid)?;
            // TODO: Temp exit to avoid duplicate addresses being covered by only
            // processing the top unit
            break;
        }
        if !self.dump_funcs {
            let timestamp = self.get_time_stamp();
            self.dump_code_load_record(module_name, code_buffer, timestamp, pid, tid);
        }
        Ok(())
    }

    fn dump_entries<R: Reader>(
        &mut self,
        unit: gimli::Unit<R>,
        dwarf: &gimli::Dwarf<R>,
        module_name: &str,
        code_buffer: &[u8],
        pid: u32,
        tid: u32,
    ) -> Result<()> {
        let mut depth = 0;
        let mut entries = unit.entries();
        while let Some((delta_depth, entry)) = entries.next_dfs()? {
            if self.dump_funcs {
                let record_header = RecordHeader {
                    id: RecordId::JitCodeLoad as u32,
                    record_size: 0,
                    timestamp: 0,
                };

                let mut clr = CodeLoadRecord {
                    header: record_header,
                    pid,
                    tid,
                    virtual_address: 0,
                    address: 0,
                    size: 0,
                    index: 0,
                };
                let mut clr_name: String = String::from(module_name);
                let mut get_debug_entry = false;
                depth += delta_depth;
                assert!(depth >= 0);

                if entry.tag() == gimli::constants::DW_TAG_subprogram {
                    get_debug_entry = true;

                    let mut attrs = entry.attrs();
                    while let Some(attr) = attrs.next()? {
                        if let Some(n) = attr.name().static_string() {
                            if n == "DW_AT_low_pc" {
                                clr.address = match attr.value() {
                                    gimli::AttributeValue::Addr(address) => address,
                                    _ => 0,
                                };
                                clr.virtual_address = clr.address;
                            } else if n == "DW_AT_high_pc" {
                                clr.size = match attr.value() {
                                    gimli::AttributeValue::Udata(data) => data,
                                    _ => 0,
                                };
                            } else if n == "DW_AT_name" {
                                clr_name = match attr.value() {
                                    gimli::AttributeValue::DebugStrRef(offset) => {
                                        if let Ok(s) = dwarf.debug_str.get_str(offset) {
                                            clr_name.push_str("::");
                                            clr_name.push_str(&s.to_string_lossy()?);
                                            clr_name
                                        } else {
                                            clr_name.push_str("::");
                                            clr_name.push_str("?");
                                            clr_name
                                        }
                                    }
                                    _ => {
                                        clr_name.push_str("??");
                                        clr_name
                                    }
                                };
                            }
                        }
                    }
                }
                if get_debug_entry {
                    //  TODO: Temp check to make sure well only formed data is processed.
                    if clr.address == 0 {
                        continue;
                    }
                    //  TODO: Temp check to make sure well only formed data is processed.
                    if clr_name == "?" {
                        continue;
                    }
                    if clr.address == 0 || clr.size == 0 {
                        clr.address = code_buffer.as_ptr() as u64;
                        clr.virtual_address = code_buffer.as_ptr() as u64;
                        clr.size = code_buffer.len() as u64;
                    }
                    clr.header.record_size = mem::size_of::<CodeLoadRecord>() as u32
                        + (clr_name.len() + 1) as u32
                        + clr.size as u32;
                    clr.index = self.code_index;
                    self.code_index += 1;
                    self.dump_debug_info(&unit, &dwarf, clr.address, clr.size, None)?;

                    clr.header.timestamp = self.get_time_stamp();

                    unsafe {
                        let code_buffer: &[u8] =
                            std::slice::from_raw_parts(clr.address as *const u8, clr.size as usize);
                        let _ = self.write_code_load_record(&clr_name, clr, code_buffer);
                    }
                }
            } else {
                let mut func_name: String = String::from("?");
                let mut func_addr = 0;
                let mut func_size = 0;

                let mut get_debug_entry = false;
                depth += delta_depth;
                assert!(depth >= 0);
                if entry.tag() == gimli::constants::DW_TAG_subprogram {
                    get_debug_entry = true;

                    let mut attrs = entry.attrs();
                    while let Some(attr) = attrs.next()? {
                        if let Some(n) = attr.name().static_string() {
                            if n == "DW_AT_low_pc" {
                                func_addr = match attr.value() {
                                    gimli::AttributeValue::Addr(address) => address,
                                    _ => 0,
                                };
                            } else if n == "DW_AT_high_pc" {
                                func_size = match attr.value() {
                                    gimli::AttributeValue::Udata(data) => data,
                                    _ => 0,
                                };
                            } else if n == "DW_AT_name" {
                                func_name = match attr.value() {
                                    gimli::AttributeValue::DebugStrRef(offset) => {
                                        if let Ok(s) = dwarf.debug_str.get_str(offset) {
                                            func_name.clear();
                                            func_name.push_str(&s.to_string_lossy()?);
                                            func_name
                                        } else {
                                            func_name.push_str("?");
                                            func_name
                                        }
                                    }
                                    _ => {
                                        func_name.push_str("??");
                                        func_name
                                    }
                                };
                            }
                        }
                    }
                }
                if get_debug_entry {
                    //  TODO: Temp check to make sure well only formed data is processed.
                    if func_addr == 0 {
                        continue;
                    }
                    //  TODO: Temp check to make sure well only formed data is processed.
                    if func_name == "?" {
                        continue;
                    }
                    self.dump_debug_info(
                        &unit,
                        &dwarf,
                        func_addr,
                        func_size,
                        Some(func_name.as_str()),
                    )?;
                }
            }
        }
        Ok(())
    }

    fn dump_debug_info<R: Reader>(
        &mut self,
        unit: &gimli::Unit<R>,
        dwarf: &gimli::Dwarf<R>,
        address: u64,
        size: u64,
        file_suffix: Option<&str>,
    ) -> Result<()> {
        let timestamp = self.get_time_stamp();
        if let Some(program) = unit.line_program.clone() {
            let mut debug_info_record = DebugInfoRecord {
                header: RecordHeader {
                    id: RecordId::JitCodeDebugInfo as u32,
                    record_size: 0,
                    timestamp,
                },
                address,
                count: 0,
            };

            let mut debug_entries = Vec::new();
            let mut debug_entries_total_filenames_len = 0;
            let mut rows = program.rows();
            while let Some((header, row)) = rows.next_row()? {
                let row_file_index = row.file_index() - 1;
                let myfile = dwarf
                    .attr_string(
                        &unit,
                        header.file_names()[row_file_index as usize].path_name(),
                    )
                    .unwrap();
                let filename = myfile.to_string_lossy()?;
                let line = row.line().unwrap_or(0);
                let column = match row.column() {
                    gimli::ColumnType::Column(column) => column,
                    gimli::ColumnType::LeftEdge => 0,
                };

                if (row.address() < address) || (row.address() > (address + size)) {
                    continue;
                }
                let mut debug_entry = DebugEntry {
                    address: row.address(),
                    line: line as u32,
                    discriminator: column as u32,
                    filename: filename.to_string(),
                };

                if let Some(suffix) = file_suffix {
                    debug_entry.filename.push_str("::");
                    debug_entry.filename.push_str(suffix);
                }

                debug_entries_total_filenames_len += debug_entry.filename.len() + 1;
                debug_entries.push(debug_entry);
            }

            debug_info_record.count = debug_entries.len() as u64;

            let debug_entries_size = (debug_info_record.count
                * (mem::size_of::<DebugEntry>() as u64 - mem::size_of::<String>() as u64))
                + debug_entries_total_filenames_len as u64;
            debug_info_record.header.record_size =
                mem::size_of::<DebugInfoRecord>() as u32 + debug_entries_size as u32;

            let _ = self.write_debug_info_record(debug_info_record);
            let _ = self.write_debug_info_entries(debug_entries);
        }
        Ok(())
    }
}

impl Drop for State {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(
                self.map_addr as *mut _,
                libc::sysconf(libc::_SC_PAGESIZE) as usize,
            );
        }
    }
}

trait Reader: gimli::Reader<Offset = usize> + Send + Sync {}

impl<'input, Endian> Reader for gimli::EndianSlice<'input, Endian> where
    Endian: gimli::Endianity + Send + Sync
{
}
