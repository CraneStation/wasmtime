//! Defines `Module` and related types.

// TODO: Should `ir::Function` really have a `name`?

// TODO: Factor out `ir::Function`'s `ext_funcs` and `global_values` into a struct
// shared with `DataContext`?

use super::HashMap;
use crate::data_context::DataContext;
use crate::Backend;
use cranelift_codegen::binemit::{self, CodeInfo};
use cranelift_codegen::entity::{entity_impl, PrimaryMap};
use cranelift_codegen::{ir, isa, CodegenError, Context};
use log::debug;
use std::borrow::ToOwned;
use std::convert::TryInto;
use std::string::String;
use std::vec::Vec;
use thiserror::Error;

/// A function identifier for use in the `Module` interface.
#[derive(Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FuncId(u32);
entity_impl!(FuncId, "funcid");

/// Function identifiers are namespace 0 in `ir::ExternalName`
impl From<FuncId> for ir::ExternalName {
    fn from(id: FuncId) -> Self {
        Self::User {
            namespace: 0,
            index: id.0,
        }
    }
}

/// A data object identifier for use in the `Module` interface.
#[derive(Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DataId(u32);
entity_impl!(DataId, "dataid");

/// Data identifiers are namespace 1 in `ir::ExternalName`
impl From<DataId> for ir::ExternalName {
    fn from(id: DataId) -> Self {
        Self::User {
            namespace: 1,
            index: id.0,
        }
    }
}

/// Linkage refers to where an entity is defined and who can see it.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Linkage {
    /// Defined outside of a module.
    Import,
    /// Defined inside the module, but not visible outside it.
    Local,
    /// Defined inside the module, visible outside it, and may be preempted.
    Preemptible,
    /// Defined inside the module, visible inside the current static linkage unit, but not outside.
    ///
    /// A static linkage unit is the combination of all object files passed to a linker to create
    /// an executable or dynamic library.
    Hidden,
    /// Defined inside the module, and visible outside it.
    Export,
}

impl Linkage {
    fn merge(a: Self, b: Self) -> Self {
        match a {
            Self::Export => Self::Export,
            Self::Hidden => match b {
                Self::Export => Self::Export,
                Self::Preemptible => Self::Preemptible,
                _ => Self::Hidden,
            },
            Self::Preemptible => match b {
                Self::Export => Self::Export,
                _ => Self::Preemptible,
            },
            Self::Local => match b {
                Self::Export => Self::Export,
                Self::Hidden => Self::Hidden,
                Self::Preemptible => Self::Preemptible,
                Self::Local | Self::Import => Self::Local,
            },
            Self::Import => b,
        }
    }

    /// Test whether this linkage can have a definition.
    pub fn is_definable(self) -> bool {
        match self {
            Self::Import => false,
            Self::Local | Self::Preemptible | Self::Hidden | Self::Export => true,
        }
    }

    /// Test whether this linkage will have a definition that cannot be preempted.
    pub fn is_final(self) -> bool {
        match self {
            Self::Import | Self::Preemptible => false,
            Self::Local | Self::Hidden | Self::Export => true,
        }
    }
}

/// A declared name may refer to either a function or data declaration
#[derive(Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum FuncOrDataId {
    /// When it's a FuncId
    Func(FuncId),
    /// When it's a DataId
    Data(DataId),
}

/// Mapping to `ir::ExternalName` is trivial based on the `FuncId` and `DataId` mapping.
impl From<FuncOrDataId> for ir::ExternalName {
    fn from(id: FuncOrDataId) -> Self {
        match id {
            FuncOrDataId::Func(funcid) => Self::from(funcid),
            FuncOrDataId::Data(dataid) => Self::from(dataid),
        }
    }
}

/// Information about a function which can be called.
pub struct FunctionDeclaration {
    pub name: String,
    pub linkage: Linkage,
    pub signature: ir::Signature,
}

/// Error messages for all `Module` and `Backend` methods
#[derive(Error, Debug)]
pub enum ModuleError {
    /// Indicates an identifier was used before it was declared
    #[error("Undeclared identifier: {0}")]
    Undeclared(String),
    /// Indicates an identifier was used as data/function first, but then used as the other
    #[error("Incompatible declaration of identifier: {0}")]
    IncompatibleDeclaration(String),
    /// Indicates a function identifier was declared with a
    /// different signature than declared previously
    #[error("Function {0} signature {2:?} is incompatible with previous declaration {1:?}")]
    IncompatibleSignature(String, ir::Signature, ir::Signature),
    /// Indicates an identifier was defined more than once
    #[error("Duplicate definition of identifier: {0}")]
    DuplicateDefinition(String),
    /// Indicates an identifier was defined, but was declared as an import
    #[error("Invalid to define identifier declared as an import: {0}")]
    InvalidImportDefinition(String),
    /// Indicates a too-long function was defined
    #[error("Function {0} exceeds the maximum function size")]
    FunctionTooLarge(String),
    /// Wraps a `cranelift-codegen` error
    #[error("Compilation error: {0}")]
    Compilation(#[from] CodegenError),
    /// Wraps a generic error from a backend
    #[error("Backend error: {0}")]
    Backend(#[source] anyhow::Error),
}

/// A convenient alias for a `Result` that uses `ModuleError` as the error type.
pub type ModuleResult<T> = Result<T, ModuleError>;

/// A function belonging to a `Module`.
pub struct ModuleFunction<B>
where
    B: Backend,
{
    /// The function declaration.
    pub decl: FunctionDeclaration,
    /// The compiled artifact, once it's available.
    pub compiled: Option<B::CompiledFunction>,
}

impl<B> ModuleFunction<B>
where
    B: Backend,
{
    fn merge(&mut self, linkage: Linkage, sig: &ir::Signature) -> Result<(), ModuleError> {
        self.decl.linkage = Linkage::merge(self.decl.linkage, linkage);
        if &self.decl.signature != sig {
            return Err(ModuleError::IncompatibleSignature(
                self.decl.name.clone(),
                self.decl.signature.clone(),
                sig.clone(),
            ));
        }
        Ok(())
    }
}

/// Information about a data object which can be accessed.
pub struct DataDeclaration {
    pub name: String,
    pub linkage: Linkage,
    pub writable: bool,
    pub tls: bool,
    pub align: Option<u8>,
}

/// A data object belonging to a `Module`.
struct ModuleData<B>
where
    B: Backend,
{
    /// The data object declaration.
    decl: DataDeclaration,
    /// The "compiled" artifact, once it's available.
    compiled: Option<B::CompiledData>,
}

impl<B> ModuleData<B>
where
    B: Backend,
{
    fn merge(&mut self, linkage: Linkage, writable: bool, tls: bool, align: Option<u8>) {
        self.decl.linkage = Linkage::merge(self.decl.linkage, linkage);
        self.decl.writable = self.decl.writable || writable;
        self.decl.align = self.decl.align.max(align);
        assert_eq!(
            self.decl.tls, tls,
            "Can't change TLS data object to normal or in the opposite way",
        );
    }
}

/// The functions and data objects belonging to a module.
struct ModuleContents<B>
where
    B: Backend,
{
    functions: PrimaryMap<FuncId, ModuleFunction<B>>,
    data_objects: PrimaryMap<DataId, ModuleData<B>>,
}

impl<B> ModuleContents<B>
where
    B: Backend,
{
    fn get_function_id(&self, name: &ir::ExternalName) -> FuncId {
        if let ir::ExternalName::User { namespace, index } = *name {
            debug_assert_eq!(namespace, 0);
            FuncId::from_u32(index)
        } else {
            panic!("unexpected ExternalName kind {}", name)
        }
    }

    fn get_data_id(&self, name: &ir::ExternalName) -> DataId {
        if let ir::ExternalName::User { namespace, index } = *name {
            debug_assert_eq!(namespace, 1);
            DataId::from_u32(index)
        } else {
            panic!("unexpected ExternalName kind {}", name)
        }
    }

    fn get_function_info(&self, name: &ir::ExternalName) -> &ModuleFunction<B> {
        &self.functions[self.get_function_id(name)]
    }

    /// Get the `DataDeclaration` for the function named by `name`.
    fn get_data_info(&self, name: &ir::ExternalName) -> &ModuleData<B> {
        &self.data_objects[self.get_data_id(name)]
    }
}

/// This provides a view to the state of a module which allows `ir::ExternalName`s to be translated
/// into `FunctionDeclaration`s and `DataDeclaration`s.
pub struct ModuleNamespace<'a, B: 'a>
where
    B: Backend,
{
    contents: &'a ModuleContents<B>,
}

impl<'a, B> ModuleNamespace<'a, B>
where
    B: Backend,
{
    /// Get the `FuncId` for the function named by `name`.
    pub fn get_function_id(&self, name: &ir::ExternalName) -> FuncId {
        self.contents.get_function_id(name)
    }

    /// Get the `DataId` for the data object named by `name`.
    pub fn get_data_id(&self, name: &ir::ExternalName) -> DataId {
        self.contents.get_data_id(name)
    }

    /// Get the `FunctionDeclaration` for the function named by `name`.
    pub fn get_function_decl(&self, name: &ir::ExternalName) -> &FunctionDeclaration {
        &self.contents.get_function_info(name).decl
    }

    /// Get the `DataDeclaration` for the data object named by `name`.
    pub fn get_data_decl(&self, name: &ir::ExternalName) -> &DataDeclaration {
        &self.contents.get_data_info(name).decl
    }

    /// Get the definition for the function named by `name`, along with its name
    /// and signature.
    pub fn get_function_definition(
        &self,
        name: &ir::ExternalName,
    ) -> (Option<&B::CompiledFunction>, &str, &ir::Signature) {
        let info = self.contents.get_function_info(name);
        debug_assert!(
            !info.decl.linkage.is_definable() || info.compiled.is_some(),
            "Finalization requires a definition for function {}.",
            name,
        );
        debug_assert_eq!(info.decl.linkage.is_definable(), info.compiled.is_some());

        (
            info.compiled.as_ref(),
            &info.decl.name,
            &info.decl.signature,
        )
    }

    /// Get the definition for the data object named by `name`, along with its name
    /// and writable flag
    pub fn get_data_definition(
        &self,
        name: &ir::ExternalName,
    ) -> (Option<&B::CompiledData>, &str, bool) {
        let info = self.contents.get_data_info(name);
        debug_assert!(
            !info.decl.linkage.is_definable() || info.compiled.is_some(),
            "Finalization requires a definition for data object {}.",
            name,
        );
        debug_assert_eq!(info.decl.linkage.is_definable(), info.compiled.is_some());

        (info.compiled.as_ref(), &info.decl.name, info.decl.writable)
    }

    /// Return whether `name` names a function, rather than a data object.
    pub fn is_function(&self, name: &ir::ExternalName) -> bool {
        if let ir::ExternalName::User { namespace, .. } = *name {
            namespace == 0
        } else {
            panic!("unexpected ExternalName kind {}", name)
        }
    }
}

/// A `Module` is a utility for collecting functions and data objects, and linking them together.
pub struct Module<B>
where
    B: Backend,
{
    names: HashMap<String, FuncOrDataId>,
    contents: ModuleContents<B>,
    functions_to_finalize: Vec<FuncId>,
    data_objects_to_finalize: Vec<DataId>,
    backend: B,
}

pub struct ModuleCompiledFunction {
    pub size: binemit::CodeOffset,
}

impl<B> Module<B>
where
    B: Backend,
{
    /// Create a new `Module`.
    pub fn new(backend_builder: B::Builder) -> Self {
        Self {
            names: HashMap::new(),
            contents: ModuleContents {
                functions: PrimaryMap::new(),
                data_objects: PrimaryMap::new(),
            },
            functions_to_finalize: Vec::new(),
            data_objects_to_finalize: Vec::new(),
            backend: B::new(backend_builder),
        }
    }

    /// Get the module identifier for a given name, if that name
    /// has been declared.
    pub fn get_name(&self, name: &str) -> Option<FuncOrDataId> {
        self.names.get(name).cloned()
    }

    /// Return the target information needed by frontends to produce Cranelift IR
    /// for the current target.
    pub fn target_config(&self) -> isa::TargetFrontendConfig {
        self.backend.isa().frontend_config()
    }

    /// Create a new `Context` initialized for use with this `Module`.
    ///
    /// This ensures that the `Context` is initialized with the default calling
    /// convention for the `TargetIsa`.
    pub fn make_context(&self) -> Context {
        let mut ctx = Context::new();
        ctx.func.signature.call_conv = self.backend.isa().default_call_conv();
        ctx
    }

    /// Clear the given `Context` and reset it for use with a new function.
    ///
    /// This ensures that the `Context` is initialized with the default calling
    /// convention for the `TargetIsa`.
    pub fn clear_context(&self, ctx: &mut Context) {
        ctx.clear();
        ctx.func.signature.call_conv = self.backend.isa().default_call_conv();
    }

    /// Create a new empty `Signature` with the default calling convention for
    /// the `TargetIsa`, to which parameter and return types can be added for
    /// declaring a function to be called by this `Module`.
    pub fn make_signature(&self) -> ir::Signature {
        ir::Signature::new(self.backend.isa().default_call_conv())
    }

    /// Clear the given `Signature` and reset for use with a new function.
    ///
    /// This ensures that the `Signature` is initialized with the default
    /// calling convention for the `TargetIsa`.
    pub fn clear_signature(&self, sig: &mut ir::Signature) {
        sig.clear(self.backend.isa().default_call_conv());
    }

    /// Declare a function in this module.
    pub fn declare_function(
        &mut self,
        name: &str,
        linkage: Linkage,
        signature: &ir::Signature,
    ) -> ModuleResult<FuncId> {
        // TODO: Can we avoid allocating names so often?
        use super::hash_map::Entry::*;
        match self.names.entry(name.to_owned()) {
            Occupied(entry) => match *entry.get() {
                FuncOrDataId::Func(id) => {
                    let existing = &mut self.contents.functions[id];
                    existing.merge(linkage, signature)?;
                    self.backend
                        .declare_function(id, name, existing.decl.linkage);
                    Ok(id)
                }
                FuncOrDataId::Data(..) => {
                    Err(ModuleError::IncompatibleDeclaration(name.to_owned()))
                }
            },
            Vacant(entry) => {
                let id = self.contents.functions.push(ModuleFunction {
                    decl: FunctionDeclaration {
                        name: name.to_owned(),
                        linkage,
                        signature: signature.clone(),
                    },
                    compiled: None,
                });
                entry.insert(FuncOrDataId::Func(id));
                self.backend.declare_function(id, name, linkage);
                Ok(id)
            }
        }
    }

    /// An iterator over functions that have been declared in this module.
    pub fn declared_functions(&self) -> core::slice::Iter<'_, ModuleFunction<B>> {
        self.contents.functions.values()
    }

    /// Declare a data object in this module.
    pub fn declare_data(
        &mut self,
        name: &str,
        linkage: Linkage,
        writable: bool,
        tls: bool,
        align: Option<u8>, // An alignment bigger than 128 is unlikely
    ) -> ModuleResult<DataId> {
        // TODO: Can we avoid allocating names so often?
        use super::hash_map::Entry::*;
        match self.names.entry(name.to_owned()) {
            Occupied(entry) => match *entry.get() {
                FuncOrDataId::Data(id) => {
                    let existing = &mut self.contents.data_objects[id];
                    existing.merge(linkage, writable, tls, align);
                    self.backend.declare_data(
                        id,
                        name,
                        existing.decl.linkage,
                        existing.decl.writable,
                        existing.decl.tls,
                        existing.decl.align,
                    );
                    Ok(id)
                }

                FuncOrDataId::Func(..) => {
                    Err(ModuleError::IncompatibleDeclaration(name.to_owned()))
                }
            },
            Vacant(entry) => {
                let id = self.contents.data_objects.push(ModuleData {
                    decl: DataDeclaration {
                        name: name.to_owned(),
                        linkage,
                        writable,
                        tls,
                        align,
                    },
                    compiled: None,
                });
                entry.insert(FuncOrDataId::Data(id));
                self.backend
                    .declare_data(id, name, linkage, writable, tls, align);
                Ok(id)
            }
        }
    }

    /// Use this when you're building the IR of a function to reference a function.
    ///
    /// TODO: Coalesce redundant decls and signatures.
    /// TODO: Look into ways to reduce the risk of using a FuncRef in the wrong function.
    pub fn declare_func_in_func(&self, func: FuncId, in_func: &mut ir::Function) -> ir::FuncRef {
        let decl = &self.contents.functions[func].decl;
        let signature = in_func.import_signature(decl.signature.clone());
        let colocated = decl.linkage.is_final();
        in_func.import_function(ir::ExtFuncData {
            name: ir::ExternalName::user(0, func.as_u32()),
            signature,
            colocated,
        })
    }

    /// Use this when you're building the IR of a function to reference a data object.
    ///
    /// TODO: Same as above.
    pub fn declare_data_in_func(&self, data: DataId, func: &mut ir::Function) -> ir::GlobalValue {
        let decl = &self.contents.data_objects[data].decl;
        let colocated = decl.linkage.is_final();
        func.create_global_value(ir::GlobalValueData::Symbol {
            name: ir::ExternalName::user(1, data.as_u32()),
            offset: ir::immediates::Imm64::new(0),
            colocated,
            tls: decl.tls,
        })
    }

    /// TODO: Same as above.
    pub fn declare_func_in_data(&self, func: FuncId, ctx: &mut DataContext) -> ir::FuncRef {
        ctx.import_function(ir::ExternalName::user(0, func.as_u32()))
    }

    /// TODO: Same as above.
    pub fn declare_data_in_data(&self, data: DataId, ctx: &mut DataContext) -> ir::GlobalValue {
        ctx.import_global_value(ir::ExternalName::user(1, data.as_u32()))
    }

    /// Define a function, producing the function body from the given `Context`.
    ///
    /// Returns the size of the function's code and constant data.
    ///
    /// Note: After calling this function the given `Context` will contain the compiled function.
    pub fn define_function<TS>(
        &mut self,
        func: FuncId,
        ctx: &mut Context,
        trap_sink: &mut TS,
    ) -> ModuleResult<ModuleCompiledFunction>
    where
        TS: binemit::TrapSink,
    {
        debug!(
            "defining function {}: {}",
            func,
            ctx.func.display(self.backend.isa())
        );
        let CodeInfo { total_size, .. } = ctx.compile(self.backend.isa())?;
        let info = &self.contents.functions[func];
        if info.compiled.is_some() {
            return Err(ModuleError::DuplicateDefinition(info.decl.name.clone()));
        }
        if !info.decl.linkage.is_definable() {
            return Err(ModuleError::InvalidImportDefinition(info.decl.name.clone()));
        }

        let compiled = self.backend.define_function(
            func,
            &info.decl.name,
            ctx,
            &ModuleNamespace::<B> {
                contents: &self.contents,
            },
            total_size,
            trap_sink,
        )?;

        self.contents.functions[func].compiled = Some(compiled);
        self.functions_to_finalize.push(func);
        Ok(ModuleCompiledFunction { size: total_size })
    }

    /// Define a function, taking the function body from the given `bytes`.
    ///
    /// This function is generally only useful if you need to precisely specify
    /// the emitted instructions for some reason; otherwise, you should use
    /// `define_function`.
    ///
    /// Returns the size of the function's code.
    pub fn define_function_bytes(
        &mut self,
        func: FuncId,
        bytes: &[u8],
    ) -> ModuleResult<ModuleCompiledFunction> {
        debug!("defining function {} with bytes", func);
        let info = &self.contents.functions[func];
        if info.compiled.is_some() {
            return Err(ModuleError::DuplicateDefinition(info.decl.name.clone()));
        }
        if !info.decl.linkage.is_definable() {
            return Err(ModuleError::InvalidImportDefinition(info.decl.name.clone()));
        }

        let total_size: u32 = match bytes.len().try_into() {
            Ok(total_size) => total_size,
            _ => Err(ModuleError::FunctionTooLarge(info.decl.name.clone()))?,
        };

        let compiled = self.backend.define_function_bytes(
            func,
            &info.decl.name,
            bytes,
            &ModuleNamespace::<B> {
                contents: &self.contents,
            },
        )?;

        self.contents.functions[func].compiled = Some(compiled);
        self.functions_to_finalize.push(func);
        Ok(ModuleCompiledFunction { size: total_size })
    }

    /// Define a data object, producing the data contents from the given `DataContext`.
    pub fn define_data(&mut self, data: DataId, data_ctx: &DataContext) -> ModuleResult<()> {
        let compiled = {
            let info = &self.contents.data_objects[data];
            if info.compiled.is_some() {
                return Err(ModuleError::DuplicateDefinition(info.decl.name.clone()));
            }
            if !info.decl.linkage.is_definable() {
                return Err(ModuleError::InvalidImportDefinition(info.decl.name.clone()));
            }
            Some(self.backend.define_data(
                data,
                &info.decl.name,
                info.decl.writable,
                info.decl.tls,
                info.decl.align,
                data_ctx,
                &ModuleNamespace::<B> {
                    contents: &self.contents,
                },
            )?)
        };
        self.contents.data_objects[data].compiled = compiled;
        self.data_objects_to_finalize.push(data);
        Ok(())
    }

    /// Write the address of `what` into the data for `data` at `offset`. `data` must refer to a
    /// defined data object.
    pub fn write_data_funcaddr(&mut self, data: DataId, offset: usize, what: ir::FuncRef) {
        let info = &mut self.contents.data_objects[data];
        debug_assert!(
            info.decl.linkage.is_definable(),
            "imported data cannot contain references"
        );
        self.backend.write_data_funcaddr(
            &mut info
                .compiled
                .as_mut()
                .expect("`data` must refer to a defined data object"),
            offset,
            what,
        );
    }

    /// Write the address of `what` plus `addend` into the data for `data` at `offset`. `data` must
    /// refer to a defined data object.
    pub fn write_data_dataaddr(
        &mut self,
        data: DataId,
        offset: usize,
        what: ir::GlobalValue,
        addend: binemit::Addend,
    ) {
        let info = &mut self.contents.data_objects[data];
        debug_assert!(
            info.decl.linkage.is_definable(),
            "imported data cannot contain references"
        );
        self.backend.write_data_dataaddr(
            &mut info
                .compiled
                .as_mut()
                .expect("`data` must refer to a defined data object"),
            offset,
            what,
            addend,
        );
    }

    /// Finalize all functions and data objects that are defined but not yet finalized.
    /// All symbols referenced in their bodies that are declared as needing a definition
    /// must be defined by this point.
    ///
    /// Use `get_finalized_function` and `get_finalized_data` to obtain the final
    /// artifacts.
    ///
    /// This method is not relevant for `Backend` implementations that do not provide
    /// `Backend::FinalizedFunction` or `Backend::FinalizedData`.
    pub fn finalize_definitions(&mut self) {
        for func in self.functions_to_finalize.drain(..) {
            let info = &self.contents.functions[func];
            debug_assert!(info.decl.linkage.is_definable());
            self.backend.finalize_function(
                func,
                info.compiled
                    .as_ref()
                    .expect("function must be compiled before it can be finalized"),
                &ModuleNamespace::<B> {
                    contents: &self.contents,
                },
            );
        }
        for data in self.data_objects_to_finalize.drain(..) {
            let info = &self.contents.data_objects[data];
            debug_assert!(info.decl.linkage.is_definable());
            self.backend.finalize_data(
                data,
                info.compiled
                    .as_ref()
                    .expect("data object must be compiled before it can be finalized"),
                &ModuleNamespace::<B> {
                    contents: &self.contents,
                },
            );
        }
        self.backend.publish();
    }

    /// Return the finalized artifact from the backend, if it provides one.
    pub fn get_finalized_function(&mut self, func: FuncId) -> B::FinalizedFunction {
        let info = &self.contents.functions[func];
        debug_assert!(
            !self.functions_to_finalize.iter().any(|x| *x == func),
            "function not yet finalized"
        );
        self.backend.get_finalized_function(
            info.compiled
                .as_ref()
                .expect("function must be compiled before it can be finalized"),
        )
    }

    /// Return the finalized artifact from the backend, if it provides one.
    pub fn get_finalized_data(&mut self, data: DataId) -> B::FinalizedData {
        let info = &self.contents.data_objects[data];
        debug_assert!(
            !self.data_objects_to_finalize.iter().any(|x| *x == data),
            "data object not yet finalized"
        );
        self.backend.get_finalized_data(
            info.compiled
                .as_ref()
                .expect("data object must be compiled before it can be finalized"),
        )
    }

    /// Return the target isa
    pub fn isa(&self) -> &dyn isa::TargetIsa {
        self.backend.isa()
    }

    /// Consume the module and return the resulting `Product`. Some `Backend`
    /// implementations may provide additional functionality available after
    /// a `Module` is complete.
    pub fn finish(self) -> B::Product {
        self.backend.finish(&ModuleNamespace::<B> {
            contents: &self.contents,
        })
    }
}
