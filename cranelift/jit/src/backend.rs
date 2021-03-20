//! Defines `JITModule`.

use crate::{compiled_blob::CompiledBlob, memory::Memory};
use cranelift_codegen::isa::TargetIsa;
use cranelift_codegen::settings::Configurable;
use cranelift_codegen::{self, ir, settings};
use cranelift_codegen::{
    binemit::{Addend, CodeInfo, CodeOffset, Reloc, RelocSink, StackMapSink, TrapSink},
    CodegenError,
};
use cranelift_entity::SecondaryMap;
use cranelift_module::{
    DataContext, DataDescription, DataId, FuncId, Init, Linkage, Module, ModuleCompiledFunction,
    ModuleDeclarations, ModuleError, ModuleResult, RelocRecord,
};
use cranelift_native;
#[cfg(not(windows))]
use libc;
use log::info;
use std::cell::RefCell;
use std::collections::HashMap;
use std::convert::{TryFrom, TryInto};
use std::ffi::CString;
use std::io::Write;
use std::ptr;
use std::ptr::NonNull;
use target_lexicon::PointerWidth;
#[cfg(windows)]
use winapi;

const EXECUTABLE_DATA_ALIGNMENT: u64 = 0x10;
const WRITABLE_DATA_ALIGNMENT: u64 = 0x8;
const READONLY_DATA_ALIGNMENT: u64 = 0x1;

/// A builder for `JITModule`.
pub struct JITBuilder {
    isa: Box<dyn TargetIsa>,
    symbols: HashMap<String, *const u8>,
    lookup_symbols: Vec<Box<dyn Fn(&str) -> Option<*const u8>>>,
    libcall_names: Box<dyn Fn(ir::LibCall) -> String + Send + Sync>,
    hotswap_enabled: bool,
}

impl JITBuilder {
    /// Create a new `JITBuilder`.
    ///
    /// The `libcall_names` function provides a way to translate `cranelift_codegen`'s `ir::LibCall`
    /// enum to symbols. LibCalls are inserted in the IR as part of the legalization for certain
    /// floating point instructions, and for stack probes. If you don't know what to use for this
    /// argument, use `cranelift_module::default_libcall_names()`.
    pub fn new(libcall_names: Box<dyn Fn(ir::LibCall) -> String + Send + Sync>) -> Self {
        let mut flag_builder = settings::builder();
        // On at least AArch64, "colocated" calls use shorter-range relocations,
        // which might not reach all definitions; we can't handle that here, so
        // we require long-range relocation types.
        flag_builder.set("use_colocated_libcalls", "false").unwrap();
        flag_builder.set("is_pic", "true").unwrap();
        let isa_builder = cranelift_native::builder().unwrap_or_else(|msg| {
            panic!("host machine is not supported: {}", msg);
        });
        let isa = isa_builder.finish(settings::Flags::new(flag_builder));
        Self::with_isa(isa, libcall_names)
    }

    /// Create a new `JITBuilder` with an arbitrary target. This is mainly
    /// useful for testing.
    ///
    /// To create a `JITBuilder` for native use, use the `new` constructor
    /// instead.
    ///
    /// The `libcall_names` function provides a way to translate `cranelift_codegen`'s `ir::LibCall`
    /// enum to symbols. LibCalls are inserted in the IR as part of the legalization for certain
    /// floating point instructions, and for stack probes. If you don't know what to use for this
    /// argument, use `cranelift_module::default_libcall_names()`.
    pub fn with_isa(
        isa: Box<dyn TargetIsa>,
        libcall_names: Box<dyn Fn(ir::LibCall) -> String + Send + Sync>,
    ) -> Self {
        let symbols = HashMap::new();
        let lookup_symbols = vec![Box::new(lookup_with_dlsym) as Box<_>];
        Self {
            isa,
            symbols,
            lookup_symbols,
            libcall_names,
            hotswap_enabled: false,
        }
    }

    /// Define a symbol in the internal symbol table.
    ///
    /// The JIT will use the symbol table to resolve names that are declared,
    /// but not defined, in the module being compiled.  A common example is
    /// external functions.  With this method, functions and data can be exposed
    /// to the code being compiled which are defined by the host.
    ///
    /// If a symbol is defined more than once, the most recent definition will
    /// be retained.
    ///
    /// If the JIT fails to find a symbol in its internal table, it will fall
    /// back to a platform-specific search (this typically involves searching
    /// the current process for public symbols, followed by searching the
    /// platform's C runtime).
    pub fn symbol<K>(&mut self, name: K, ptr: *const u8) -> &mut Self
    where
        K: Into<String>,
    {
        self.symbols.insert(name.into(), ptr);
        self
    }

    /// Define multiple symbols in the internal symbol table.
    ///
    /// Using this is equivalent to calling `symbol` on each element.
    pub fn symbols<It, K>(&mut self, symbols: It) -> &mut Self
    where
        It: IntoIterator<Item = (K, *const u8)>,
        K: Into<String>,
    {
        for (name, ptr) in symbols {
            self.symbols.insert(name.into(), ptr);
        }
        self
    }

    /// Add a symbol lookup fn.
    ///
    /// Symbol lookup fn's are used to lookup symbols when they couldn't be found in the internal
    /// symbol table. Symbol lookup fn's are called in reverse of the order in which they were added.
    pub fn symbol_lookup_fn(
        &mut self,
        symbol_lookup_fn: Box<dyn Fn(&str) -> Option<*const u8>>,
    ) -> &mut Self {
        self.lookup_symbols.push(symbol_lookup_fn);
        self
    }

    /// Enable or disable hotswap support. See [`JITModule::prepare_for_function_redefine`]
    /// for more information.
    ///
    /// Enabling hotswap support requires PIC code.
    pub fn hotswap(&mut self, enabled: bool) -> &mut Self {
        self.hotswap_enabled = enabled;
        self
    }
}

/// A `JITModule` implements `Module` and emits code and data into memory where it can be
/// directly called and accessed.
///
/// See the `JITBuilder` for a convenient way to construct `JITModule` instances.
pub struct JITModule {
    isa: Box<dyn TargetIsa>,
    hotswap_enabled: bool,
    symbols: RefCell<HashMap<String, *const u8>>,
    lookup_symbols: Vec<Box<dyn Fn(&str) -> Option<*const u8>>>,
    libcall_names: Box<dyn Fn(ir::LibCall) -> String>,
    memory: MemoryHandle,
    declarations: ModuleDeclarations,
    function_got_entries: SecondaryMap<FuncId, Option<NonNull<*const u8>>>,
    function_plt_entries: SecondaryMap<FuncId, Option<NonNull<[u8; 16]>>>,
    data_object_got_entries: SecondaryMap<DataId, Option<NonNull<*const u8>>>,
    libcall_got_entries: HashMap<ir::LibCall, NonNull<*const u8>>,
    libcall_plt_entries: HashMap<ir::LibCall, NonNull<[u8; 16]>>,
    compiled_functions: SecondaryMap<FuncId, Option<CompiledBlob>>,
    compiled_data_objects: SecondaryMap<DataId, Option<CompiledBlob>>,
    functions_to_finalize: Vec<FuncId>,
    data_objects_to_finalize: Vec<DataId>,
}

/// A handle to allow freeing memory allocated by the `Module`.
struct MemoryHandle {
    code: Memory,
    readonly: Memory,
    writable: Memory,
}

impl JITModule {
    /// Free memory allocated for code and data segments of compiled functions.
    ///
    /// # Safety
    ///
    /// Because this function invalidates any pointers retrived from the
    /// corresponding module, it should only be used when none of the functions
    /// from that module are currently executing and none of the `fn` pointers
    /// are called afterwards.
    pub unsafe fn free_memory(&mut self) {
        self.memory.code.free_memory();
        self.memory.readonly.free_memory();
        self.memory.writable.free_memory();
    }

    fn lookup_symbol(&self, name: &str) -> Option<*const u8> {
        match self.symbols.borrow_mut().entry(name.to_owned()) {
            std::collections::hash_map::Entry::Occupied(occ) => Some(*occ.get()),
            std::collections::hash_map::Entry::Vacant(vac) => {
                let ptr = self
                    .lookup_symbols
                    .iter()
                    .rev() // Try last lookup function first
                    .find_map(|lookup| lookup(name));
                if let Some(ptr) = ptr {
                    vac.insert(ptr);
                }
                ptr
            }
        }
    }

    unsafe fn write_plt_entry_bytes(plt_ptr: *mut [u8; 16], got_ptr: *mut *const u8) {
        assert!(
            cfg!(target_arch = "x86_64"),
            "PLT is currently only supported on x86_64"
        );
        // jmp *got_ptr; ud2; ud2; ud2; ud2; ud2
        let mut plt_val = [
            0xff, 0x25, 0, 0, 0, 0, 0x0f, 0x0b, 0x0f, 0x0b, 0x0f, 0x0b, 0x0f, 0x0b, 0x0f, 0x0b,
        ];
        let what = got_ptr as isize - 4;
        let at = plt_ptr as isize + 2;
        plt_val[2..6].copy_from_slice(&i32::to_ne_bytes(i32::try_from(what - at).unwrap()));
        std::ptr::write(plt_ptr, plt_val);
    }

    fn get_address(&self, name: &ir::ExternalName) -> *const u8 {
        match *name {
            ir::ExternalName::User { .. } => {
                let (name, linkage) = if ModuleDeclarations::is_function(name) {
                    if self.hotswap_enabled {
                        return self.get_plt_address(name);
                    } else {
                        let func_id = FuncId::from_name(name);
                        match &self.compiled_functions[func_id] {
                            Some(compiled) => return compiled.ptr,
                            None => {
                                let decl = self.declarations.get_function_decl(func_id);
                                (&decl.name, decl.linkage)
                            }
                        }
                    }
                } else {
                    let data_id = DataId::from_name(name);
                    match &self.compiled_data_objects[data_id] {
                        Some(compiled) => return compiled.ptr,
                        None => {
                            let decl = self.declarations.get_data_decl(data_id);
                            (&decl.name, decl.linkage)
                        }
                    }
                };
                if let Some(ptr) = self.lookup_symbol(name) {
                    ptr
                } else if linkage == Linkage::Preemptible {
                    0 as *const u8
                } else {
                    panic!("can't resolve symbol {}", name);
                }
            }
            ir::ExternalName::LibCall(ref libcall) => {
                let sym = (self.libcall_names)(*libcall);
                self.lookup_symbol(&sym)
                    .unwrap_or_else(|| panic!("can't resolve libcall {}", sym))
            }
            _ => panic!("invalid ExternalName {}", name),
        }
    }

    fn get_got_address(&self, name: &ir::ExternalName) -> *const u8 {
        match *name {
            ir::ExternalName::User { .. } => {
                if ModuleDeclarations::is_function(name) {
                    let func_id = FuncId::from_name(name);
                    self.function_got_entries[func_id]
                        .unwrap()
                        .as_ptr()
                        .cast::<u8>()
                } else {
                    let data_id = DataId::from_name(name);
                    self.data_object_got_entries[data_id]
                        .unwrap()
                        .as_ptr()
                        .cast::<u8>()
                }
            }
            ir::ExternalName::LibCall(ref libcall) => self
                .libcall_got_entries
                .get(libcall)
                .unwrap_or_else(|| panic!("can't resolve libcall {}", libcall))
                .as_ptr()
                .cast::<u8>(),
            _ => panic!("invalid ExternalName {}", name),
        }
    }

    fn get_plt_address(&self, name: &ir::ExternalName) -> *const u8 {
        match *name {
            ir::ExternalName::User { .. } => {
                if ModuleDeclarations::is_function(name) {
                    let func_id = FuncId::from_name(name);
                    self.function_plt_entries[func_id]
                        .unwrap()
                        .as_ptr()
                        .cast::<u8>()
                } else {
                    unreachable!("PLT relocations can only have functions as target");
                }
            }
            ir::ExternalName::LibCall(ref libcall) => self
                .libcall_plt_entries
                .get(libcall)
                .unwrap_or_else(|| panic!("can't resolve libcall {}", libcall))
                .as_ptr()
                .cast::<u8>(),
            _ => panic!("invalid ExternalName {}", name),
        }
    }

    /// Returns the address of a finalized function.
    pub fn get_finalized_function(&self, func_id: FuncId) -> *const u8 {
        let info = &self.compiled_functions[func_id];
        assert!(
            !self.functions_to_finalize.iter().any(|x| *x == func_id),
            "function not yet finalized"
        );
        info.as_ref()
            .expect("function must be compiled before it can be finalized")
            .ptr
    }

    /// Returns the address and size of a finalized data object.
    pub fn get_finalized_data(&self, data_id: DataId) -> (*const u8, usize) {
        let info = &self.compiled_data_objects[data_id];
        assert!(
            !self.data_objects_to_finalize.iter().any(|x| *x == data_id),
            "data object not yet finalized"
        );
        let compiled = info
            .as_ref()
            .expect("data object must be compiled before it can be finalized");

        (compiled.ptr, compiled.size)
    }

    fn record_function_for_perf(&self, ptr: *mut u8, size: usize, name: &str) {
        // The Linux perf tool supports JIT code via a /tmp/perf-$PID.map file,
        // which contains memory regions and their associated names.  If we
        // are profiling with perf and saving binaries to PERF_BUILDID_DIR
        // for post-profile analysis, write information about each function
        // we define.
        if cfg!(target_os = "linux") && ::std::env::var_os("PERF_BUILDID_DIR").is_some() {
            let mut map_file = ::std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(format!("/tmp/perf-{}.map", ::std::process::id()))
                .unwrap();

            let _ = writeln!(map_file, "{:x} {:x} {}", ptr as usize, size, name);
        }
    }

    /// Finalize all functions and data objects that are defined but not yet finalized.
    /// All symbols referenced in their bodies that are declared as needing a definition
    /// must be defined by this point.
    ///
    /// Use `get_finalized_function` and `get_finalized_data` to obtain the final
    /// artifacts.
    pub fn finalize_definitions(&mut self) {
        for func in std::mem::take(&mut self.functions_to_finalize) {
            let decl = self.declarations.get_function_decl(func);
            assert!(decl.linkage.is_definable());
            let func = self.compiled_functions[func]
                .as_ref()
                .expect("function must be compiled before it can be finalized");
            func.perform_relocations(
                |name| self.get_address(name),
                |name| self.get_got_address(name),
                |name| self.get_plt_address(name),
            );
        }

        for data in std::mem::take(&mut self.data_objects_to_finalize) {
            let decl = self.declarations.get_data_decl(data);
            assert!(decl.linkage.is_definable());
            let data = self.compiled_data_objects[data]
                .as_ref()
                .expect("data object must be compiled before it can be finalized");
            data.perform_relocations(
                |name| self.get_address(name),
                |name| self.get_got_address(name),
                |name| self.get_plt_address(name),
            );
        }

        // Now that we're done patching, prepare the memory for execution!
        self.memory.readonly.set_readonly();
        self.memory.code.set_readable_and_executable();
    }

    /// Create a new `JITModule`.
    pub fn new(builder: JITBuilder) -> Self {
        if builder.hotswap_enabled {
            assert!(
                builder.isa.flags().is_pic(),
                "Hotswapping requires PIC code"
            );
        }

        let mut memory = MemoryHandle {
            code: Memory::new(),
            readonly: Memory::new(),
            writable: Memory::new(),
        };

        let mut libcall_got_entries = HashMap::new();
        let mut libcall_plt_entries = HashMap::new();

        // Pre-create a GOT and PLT entry for each libcall.
        let all_libcalls = if builder.isa.flags().is_pic() {
            ir::LibCall::all_libcalls()
        } else {
            &[] // Not PIC, so no GOT and PLT entries necessary
        };
        for &libcall in all_libcalls {
            let got_entry = memory
                .writable
                .allocate(
                    std::mem::size_of::<*const u8>(),
                    std::mem::align_of::<*const u8>().try_into().unwrap(),
                )
                .unwrap()
                .cast::<*const u8>();
            libcall_got_entries.insert(libcall, NonNull::new(got_entry).unwrap());
            let sym = (builder.libcall_names)(libcall);
            let addr = if let Some(addr) = builder
                .symbols
                .get(&sym)
                .copied()
                .or_else(|| lookup_with_dlsym(&sym))
            {
                addr
            } else {
                continue;
            };
            unsafe {
                std::ptr::write(got_entry, addr);
            }
            let plt_entry = memory
                .code
                .allocate(std::mem::size_of::<[u8; 16]>(), EXECUTABLE_DATA_ALIGNMENT)
                .unwrap()
                .cast::<[u8; 16]>();
            libcall_plt_entries.insert(libcall, NonNull::new(plt_entry).unwrap());
            unsafe {
                Self::write_plt_entry_bytes(plt_entry, got_entry);
            }
        }

        Self {
            isa: builder.isa,
            hotswap_enabled: builder.hotswap_enabled,
            symbols: RefCell::new(builder.symbols),
            lookup_symbols: builder.lookup_symbols,
            libcall_names: builder.libcall_names,
            memory,
            declarations: ModuleDeclarations::default(),
            function_got_entries: SecondaryMap::new(),
            function_plt_entries: SecondaryMap::new(),
            data_object_got_entries: SecondaryMap::new(),
            libcall_got_entries,
            libcall_plt_entries,
            compiled_functions: SecondaryMap::new(),
            compiled_data_objects: SecondaryMap::new(),
            functions_to_finalize: Vec::new(),
            data_objects_to_finalize: Vec::new(),
        }
    }

    /// Allow a single future `define_function` on a previously defined function. This allows for
    /// hot code swapping and lazy compilation of functions.
    ///
    /// This requires hotswap support to be enabled first using [`JITBuilder::hotswap`].
    pub fn prepare_for_function_redefine(&mut self, func_id: FuncId) -> ModuleResult<()> {
        assert!(self.hotswap_enabled, "Hotswap support is not enabled");
        let decl = self.declarations.get_function_decl(func_id);
        if !decl.linkage.is_definable() {
            return Err(ModuleError::InvalidImportDefinition(decl.name.clone()));
        }

        if self.compiled_functions[func_id].is_none() {
            return Err(ModuleError::Backend(anyhow::anyhow!(
                "Tried to redefine not yet defined function {}",
                decl.name
            )));
        }

        self.compiled_functions[func_id] = None;

        // FIXME return some kind of handle that allows for deallocating the function

        Ok(())
    }
}

impl Module for JITModule {
    fn isa(&self) -> &dyn TargetIsa {
        &*self.isa
    }

    fn declarations(&self) -> &ModuleDeclarations {
        &self.declarations
    }

    fn declare_function(
        &mut self,
        name: &str,
        linkage: Linkage,
        signature: &ir::Signature,
    ) -> ModuleResult<FuncId> {
        let (id, _decl) = self
            .declarations
            .declare_function(name, linkage, signature)?;
        if self.function_got_entries[id].is_none() && self.isa.flags().is_pic() {
            let got_entry = self
                .memory
                .writable
                .allocate(
                    std::mem::size_of::<*const u8>(),
                    std::mem::align_of::<*const u8>().try_into().unwrap(),
                )
                .unwrap()
                .cast::<*const u8>();
            self.function_got_entries[id] = Some(NonNull::new(got_entry).unwrap());
            // FIXME populate got entries with a null pointer when defined
            let val = self.lookup_symbol(name).unwrap_or(std::ptr::null());
            unsafe {
                std::ptr::write(got_entry, val);
            }
            let plt_entry = self
                .memory
                .code
                .allocate(std::mem::size_of::<[u8; 16]>(), EXECUTABLE_DATA_ALIGNMENT)
                .unwrap()
                .cast::<[u8; 16]>();
            self.record_function_for_perf(
                plt_entry as *mut _,
                std::mem::size_of::<[u8; 16]>(),
                &format!("{}@plt", name),
            );
            self.function_plt_entries[id] = Some(NonNull::new(plt_entry).unwrap());
            unsafe {
                Self::write_plt_entry_bytes(plt_entry, got_entry);
            }
        }
        Ok(id)
    }

    fn declare_data(
        &mut self,
        name: &str,
        linkage: Linkage,
        writable: bool,
        tls: bool,
    ) -> ModuleResult<DataId> {
        assert!(!tls, "JIT doesn't yet support TLS");
        let (id, _decl) = self
            .declarations
            .declare_data(name, linkage, writable, tls)?;
        if self.data_object_got_entries[id].is_none() && self.isa.flags().is_pic() {
            let got_entry = self
                .memory
                .writable
                .allocate(
                    std::mem::size_of::<*const u8>(),
                    std::mem::align_of::<*const u8>().try_into().unwrap(),
                )
                .unwrap()
                .cast::<*const u8>();
            self.data_object_got_entries[id] = Some(NonNull::new(got_entry).unwrap());
            // FIXME populate got entries with a null pointer when defined
            let val = self.lookup_symbol(name).unwrap_or(std::ptr::null());
            unsafe {
                std::ptr::write(got_entry, val);
            }
        }
        Ok(id)
    }

    /// Use this when you're building the IR of a function to reference a function.
    ///
    /// TODO: Coalesce redundant decls and signatures.
    /// TODO: Look into ways to reduce the risk of using a FuncRef in the wrong function.
    fn declare_func_in_func(&self, func: FuncId, in_func: &mut ir::Function) -> ir::FuncRef {
        let decl = self.declarations.get_function_decl(func);
        let signature = in_func.import_signature(decl.signature.clone());
        let colocated = !self.hotswap_enabled && decl.linkage.is_final();
        in_func.import_function(ir::ExtFuncData {
            name: ir::ExternalName::user(0, func.as_u32()),
            signature,
            colocated,
        })
    }

    /// Use this when you're building the IR of a function to reference a data object.
    ///
    /// TODO: Same as above.
    fn declare_data_in_func(&self, data: DataId, func: &mut ir::Function) -> ir::GlobalValue {
        let decl = self.declarations.get_data_decl(data);
        let colocated = !self.hotswap_enabled && decl.linkage.is_final();
        func.create_global_value(ir::GlobalValueData::Symbol {
            name: ir::ExternalName::user(1, data.as_u32()),
            offset: ir::immediates::Imm64::new(0),
            colocated,
            tls: decl.tls,
        })
    }

    /// TODO: Same as above.
    fn declare_func_in_data(&self, func: FuncId, ctx: &mut DataContext) -> ir::FuncRef {
        ctx.import_function(ir::ExternalName::user(0, func.as_u32()))
    }

    /// TODO: Same as above.
    fn declare_data_in_data(&self, data: DataId, ctx: &mut DataContext) -> ir::GlobalValue {
        ctx.import_global_value(ir::ExternalName::user(1, data.as_u32()))
    }

    fn define_function(
        &mut self,
        id: FuncId,
        ctx: &mut cranelift_codegen::Context,
        trap_sink: &mut dyn TrapSink,
        stack_map_sink: &mut dyn StackMapSink,
    ) -> ModuleResult<ModuleCompiledFunction> {
        info!("defining function {}: {}", id, ctx.func.display(self.isa()));
        let CodeInfo {
            total_size: code_size,
            ..
        } = ctx.compile(self.isa())?;

        let decl = self.declarations.get_function_decl(id);
        if !decl.linkage.is_definable() {
            return Err(ModuleError::InvalidImportDefinition(decl.name.clone()));
        }

        if !self.compiled_functions[id].is_none() {
            return Err(ModuleError::DuplicateDefinition(decl.name.to_owned()));
        }

        let size = code_size as usize;
        let ptr = self
            .memory
            .code
            .allocate(size, EXECUTABLE_DATA_ALIGNMENT)
            .expect("TODO: handle OOM etc.");

        let mut reloc_sink = JITRelocSink::default();
        unsafe { ctx.emit_to_memory(&*self.isa, ptr, &mut reloc_sink, trap_sink, stack_map_sink) };

        self.record_function_for_perf(ptr, size, &decl.name);
        self.compiled_functions[id] = Some(CompiledBlob {
            ptr,
            size,
            relocs: reloc_sink.relocs,
        });

        if self.isa.flags().is_pic() {
            unsafe {
                std::ptr::write(self.function_got_entries[id].unwrap().as_ptr(), ptr);
            }
        }

        if self.hotswap_enabled {
            self.compiled_functions[id]
                .as_ref()
                .unwrap()
                .perform_relocations(
                    |name| match *name {
                        ir::ExternalName::User { .. } => {
                            unreachable!("non GOT or PLT relocation in function {} to {}", id, name)
                        }
                        ir::ExternalName::LibCall(ref libcall) => self
                            .libcall_plt_entries
                            .get(libcall)
                            .unwrap_or_else(|| panic!("can't resolve libcall {}", libcall))
                            .as_ptr()
                            .cast::<u8>(),
                        _ => panic!("invalid ExternalName {}", name),
                    },
                    |name| self.get_got_address(name),
                    |name| self.get_plt_address(name),
                );
        } else {
            self.functions_to_finalize.push(id);
        }

        Ok(ModuleCompiledFunction { size: code_size })
    }

    fn define_function_bytes(
        &mut self,
        id: FuncId,
        bytes: &[u8],
        relocs: &[RelocRecord],
    ) -> ModuleResult<ModuleCompiledFunction> {
        info!("defining function {} with bytes", id);
        let total_size: u32 = match bytes.len().try_into() {
            Ok(total_size) => total_size,
            _ => Err(CodegenError::CodeTooLarge)?,
        };

        let decl = self.declarations.get_function_decl(id);
        if !decl.linkage.is_definable() {
            return Err(ModuleError::InvalidImportDefinition(decl.name.clone()));
        }

        if !self.compiled_functions[id].is_none() {
            return Err(ModuleError::DuplicateDefinition(decl.name.to_owned()));
        }

        let size = bytes.len();
        let ptr = self
            .memory
            .code
            .allocate(size, EXECUTABLE_DATA_ALIGNMENT)
            .expect("TODO: handle OOM etc.");

        unsafe {
            ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, size);
        }

        self.record_function_for_perf(ptr, size, &decl.name);
        self.compiled_functions[id] = Some(CompiledBlob {
            ptr,
            size,
            relocs: relocs.to_vec(),
        });

        if self.isa.flags().is_pic() {
            unsafe {
                std::ptr::write(self.function_got_entries[id].unwrap().as_ptr(), ptr);
            }
        }

        if self.hotswap_enabled {
            self.compiled_functions[id]
                .as_ref()
                .unwrap()
                .perform_relocations(
                    |name| unreachable!("non GOT or PLT relocation in function {} to {}", id, name),
                    |name| self.get_got_address(name),
                    |name| self.get_plt_address(name),
                );
        } else {
            self.functions_to_finalize.push(id);
        }

        Ok(ModuleCompiledFunction { size: total_size })
    }

    fn define_data(&mut self, id: DataId, data: &DataContext) -> ModuleResult<()> {
        let decl = self.declarations.get_data_decl(id);
        if !decl.linkage.is_definable() {
            return Err(ModuleError::InvalidImportDefinition(decl.name.clone()));
        }

        if !self.compiled_data_objects[id].is_none() {
            return Err(ModuleError::DuplicateDefinition(decl.name.to_owned()));
        }

        assert!(!decl.tls, "JIT doesn't yet support TLS");

        let &DataDescription {
            ref init,
            function_decls: _,
            data_decls: _,
            function_relocs: _,
            data_relocs: _,
            custom_segment_section: _,
            align,
        } = data.description();

        let size = init.size();
        let ptr = if decl.writable {
            self.memory
                .writable
                .allocate(size, align.unwrap_or(WRITABLE_DATA_ALIGNMENT))
                .expect("TODO: handle OOM etc.")
        } else {
            self.memory
                .readonly
                .allocate(size, align.unwrap_or(READONLY_DATA_ALIGNMENT))
                .expect("TODO: handle OOM etc.")
        };

        match *init {
            Init::Uninitialized => {
                panic!("data is not initialized yet");
            }
            Init::Zeros { .. } => {
                unsafe { ptr::write_bytes(ptr, 0, size) };
            }
            Init::Bytes { ref contents } => {
                let src = contents.as_ptr();
                unsafe { ptr::copy_nonoverlapping(src, ptr, size) };
            }
        }

        let pointer_reloc = match self.isa.triple().pointer_width().unwrap() {
            PointerWidth::U16 => panic!(),
            PointerWidth::U32 => Reloc::Abs4,
            PointerWidth::U64 => Reloc::Abs8,
        };
        let relocs = data
            .description()
            .all_relocs(pointer_reloc)
            .collect::<Vec<_>>();

        self.compiled_data_objects[id] = Some(CompiledBlob { ptr, size, relocs });
        self.data_objects_to_finalize.push(id);
        if self.isa.flags().is_pic() {
            unsafe {
                std::ptr::write(self.data_object_got_entries[id].unwrap().as_ptr(), ptr);
            }
        }

        Ok(())
    }
}

#[cfg(not(windows))]
fn lookup_with_dlsym(name: &str) -> Option<*const u8> {
    let c_str = CString::new(name).unwrap();
    let c_str_ptr = c_str.as_ptr();
    let sym = unsafe { libc::dlsym(libc::RTLD_DEFAULT, c_str_ptr) };
    if sym.is_null() {
        None
    } else {
        Some(sym as *const u8)
    }
}

#[cfg(windows)]
fn lookup_with_dlsym(name: &str) -> Option<*const u8> {
    const MSVCRT_DLL: &[u8] = b"msvcrt.dll\0";

    let c_str = CString::new(name).unwrap();
    let c_str_ptr = c_str.as_ptr();

    unsafe {
        let handles = [
            // try to find the searched symbol in the currently running executable
            ptr::null_mut(),
            // try to find the searched symbol in local c runtime
            winapi::um::libloaderapi::GetModuleHandleA(MSVCRT_DLL.as_ptr() as *const i8),
        ];

        for handle in &handles {
            let addr = winapi::um::libloaderapi::GetProcAddress(*handle, c_str_ptr);
            if addr.is_null() {
                continue;
            }
            return Some(addr as *const u8);
        }

        None
    }
}

#[derive(Default)]
struct JITRelocSink {
    relocs: Vec<RelocRecord>,
}

impl RelocSink for JITRelocSink {
    fn reloc_external(
        &mut self,
        offset: CodeOffset,
        _srcloc: ir::SourceLoc,
        reloc: Reloc,
        name: &ir::ExternalName,
        addend: Addend,
    ) {
        self.relocs.push(RelocRecord {
            offset,
            reloc,
            name: name.clone(),
            addend,
        });
    }

    fn reloc_jt(&mut self, _offset: CodeOffset, reloc: Reloc, _jt: ir::JumpTable) {
        match reloc {
            Reloc::X86PCRelRodata4 => {
                // Not necessary to record this unless we are going to split apart code and its
                // jumptbl/rodata.
            }
            _ => {
                panic!("Unhandled reloc");
            }
        }
    }

    fn reloc_constant(&mut self, _offset: CodeOffset, reloc: Reloc, _constant: ir::ConstantOffset) {
        match reloc {
            Reloc::X86PCRelRodata4 => {
                // Not necessary to record this unless we are going to split apart code and its
                // jumptbl/rodata.
            }
            _ => {
                panic!("Unhandled reloc");
            }
        }
    }
}
