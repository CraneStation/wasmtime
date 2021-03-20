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
use std::sync::Mutex;
use std::{borrow, mem, process};
use target_lexicon::Architecture;
use wasmtime_environ::entity::PrimaryMap;
use wasmtime_environ::wasm::DefinedFuncIndex;
use wasmtime_environ::Module;
use wasmtime_jit_debug::perf_jitdump::*;
use wasmtime_runtime::VMFunctionBody;

use object::elf;

/// Interface for driving the creation of jitdump files
pub struct JitDumpAgent {
    // Note that we use a mutex internally to serialize writing out to our
    // `jitdump_file` within this process, since multiple threads may be sharing
    // this jit agent.
    state: Mutex<State>,
}

struct State {
    jitdump_file: JitDumpFile,

    /// Flag for experimenting with dumping code load record
    /// after each function (true) or after each module. This
    /// flag is currently set to true.
    dump_funcs: bool,
}

impl JitDumpAgent {
    /// Intialize a JitDumpAgent and write out the header
    pub fn new() -> Result<Self> {
        let filename = format!("./jit-{}.dump", process::id());

        let e_machine = match target_lexicon::HOST.architecture {
            Architecture::X86_64 => elf::EM_X86_64 as u32,
            Architecture::X86_32(_) => elf::EM_386 as u32,
            Architecture::Arm(_) => elf::EM_ARM as u32,
            Architecture::Aarch64(_) => elf::EM_AARCH64 as u32,
            _ => unimplemented!("unrecognized architecture"),
        };

        let jitdump_file = JitDumpFile::new(filename, e_machine)?;

        Ok(JitDumpAgent {
            state: Mutex::new(State {
                jitdump_file,
                dump_funcs: true,
            }),
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
                let timestamp = self.jitdump_file.get_time_stamp();
                let name = super::debug_name(module, idx);
                if let Err(err) =
                    self.jitdump_file
                        .dump_code_load_record(&name, code_buffer, timestamp, pid, tid)
                {
                    println!("Jitdump: write_code_load_failed_record failed: {:?}\n", err);
                }
            }
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
            let timestamp = self.jitdump_file.get_time_stamp();
            if let Err(err) = self.jitdump_file.dump_code_load_record(
                module_name,
                code_buffer,
                timestamp,
                pid,
                tid,
            ) {
                println!("Jitdump: write_code_load_failed_record failed: {:?}\n", err);
            }
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
                    clr.index = self.jitdump_file.next_code_index();
                    self.dump_debug_info(&unit, &dwarf, clr.address, clr.size, None)?;

                    clr.header.timestamp = self.jitdump_file.get_time_stamp();

                    unsafe {
                        let code_buffer: &[u8] =
                            std::slice::from_raw_parts(clr.address as *const u8, clr.size as usize);
                        let _ =
                            self.jitdump_file
                                .write_code_load_record(&clr_name, clr, code_buffer);
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
        let timestamp = self.jitdump_file.get_time_stamp();
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

            let _ = self.jitdump_file.write_debug_info_record(debug_info_record);
            let _ = self.jitdump_file.write_debug_info_entries(debug_entries);
        }
        Ok(())
    }
}

trait Reader: gimli::Reader<Offset = usize> + Send + Sync {}

impl<'input, Endian> Reader for gimli::EndianSlice<'input, Endian> where
    Endian: gimli::Endianity + Send + Sync
{
}
