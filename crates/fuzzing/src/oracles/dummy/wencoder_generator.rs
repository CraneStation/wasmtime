use wasm_encoder::{
    CodeSection, ElementSection, Export, ExportSection, Function, FunctionSection, GlobalSection,
    ImportSection, InstanceSection, Instruction, Limits, MemorySection, Module, ModuleSection,
    TableSection, TypeSection,
};
use wasmtime::*;

pub struct WencoderGenerator {
    function_section: FunctionSection,
    type_section: TypeSection,
    memory_section: MemorySection,
    table_section: TableSection,
    global_section: GlobalSection,
    import_section: ImportSection,
    instance_section: InstanceSection,
    code_section: CodeSection,
    export_section: ExportSection,
    module_section: ModuleSection,
    element_section: ElementSection,

    num_types: u32,
    num_funcs: u32,
    num_globals: u32,
    num_tables: u32,
    num_memories: u32,
    num_instances: u32,
    num_modules: u32,
    modules_for_instantiate: Vec<u32>,
}

impl WencoderGenerator {
    pub fn new() -> WencoderGenerator {
        WencoderGenerator {
            type_section: TypeSection::new(),
            import_section: ImportSection::new(),
            function_section: FunctionSection::new(),
            table_section: TableSection::new(),
            memory_section: MemorySection::new(),
            global_section: GlobalSection::new(),
            export_section: ExportSection::new(),
            element_section: ElementSection::new(),
            instance_section: InstanceSection::new(),
            code_section: CodeSection::new(),
            module_section: ModuleSection::new(),

            num_types: 0,
            num_funcs: 0,
            num_globals: 0,
            num_tables: 0,
            num_memories: 0,
            num_instances: 0,
            num_modules: 0,

            modules_for_instantiate: Vec::new(),
        }
    }
    pub fn finish(self) -> Vec<u8> {
        let mut module = Module::new();

        module.section(&self.module_section);
        module.section(&self.type_section);
        module.section(&self.instance_section);
        module.section(&self.import_section);
        module.section(&self.function_section);
        module.section(&self.table_section);
        module.section(&self.memory_section);
        module.section(&self.global_section);
        module.section(&self.export_section);
        module.section(&self.element_section);
        module.section(&self.code_section);

        module.finish()
    }
    pub fn import(&mut self, ty: &ImportType<'_>) {
        let num_types = self.num_types;

        let item_ty = ty.ty();
        self.item_import(&item_ty);

        self.import_section.import(
            ty.module(),
            ty.name(),
            extern_to_entity(&ty.ty(), num_types),
        );
    }
    pub fn export(&mut self, ty: &ExportType<'_>) {
        let item_ty = ty.ty();
        let num_whatever = self.item_export(&item_ty);

        self.export_section
            .export(&ty.name(), extern_to_export(&item_ty, num_whatever));
    }
    fn item_import(&mut self, ty: &ExternType) {
        match ty {
            ExternType::Memory(mem) => {
                self.memory_section_add_item(mem);
            }
            ExternType::Table(table) => {
                self.table_section_add_item(table);
            }
            ExternType::Global(global) => {
                self.global_section_add_item(global);
            }
            ExternType::Func(v) => {
                self.type_section_add_item_func(v);
            }
            ExternType::Module(v) => {
                self.type_section_add_item_module(&v);

                for import in v.imports() {
                    self.import(&import);
                }
                for export in v.exports() {
                    self.export(&export);
                }
            }
            ExternType::Instance(v) => {
                self.type_section_add_item_instance(&v);
                for export in v.exports() {
                    self.export(&export);
                }
            }
        }
    }
    fn item_export(&mut self, ty: &ExternType) -> u32 {
        match ty {
            ExternType::Memory(mem) => self.memory_section_add_item(mem) - 1,
            ExternType::Table(table) => self.table_section_add_item(table) - 1,
            ExternType::Global(global) => self.global_section_add_item(global) - 1,
            ExternType::Func(v) => {
                self.type_section_add_item_func(v);
                self.func_section_add_item(v, self.num_types - 1) - 1
            }
            ExternType::Module(v) => {
                for import in v.imports() {
                    self.import(&import);
                }
                for export in v.exports() {
                    self.export(&export);
                }

                let module = &mut Module::new();
                self.module_init(module, &v);
                self.module_section.module(module);
                self.num_modules += 1;
                self.modules_for_instantiate.push(self.num_modules - 1);
                self.num_modules - 1
            }
            ExternType::Instance(v) => {
                self.type_section_add_item_instance(&v);

                if !self.modules_for_instantiate.is_empty() {
                    self.instance_section.instantiate(
                        self.modules_for_instantiate.remove(0),
                        v.exports()
                            .into_iter()
                            .enumerate()
                            .map(|(i, it)| (it.name(), extern_to_export(&it.ty(), i as u32))),
                    );
                    self.num_instances += 1;
                }
                self.num_instances - 1
            }
        }
    }
    fn memory_section_add_item(&mut self, mem: &MemoryType) -> u32 {
        self.memory_section.memory(wasm_encoder::MemoryType {
            limits: Limits {
                min: mem.limits().min(),
                max: mem.limits().max(),
            },
        });
        self.num_memories += 1;
        self.num_memories
    }
    fn table_section_add_item(&mut self, table: &TableType) -> u32 {
        self.table_section.table(wasm_encoder::TableType {
            element_type: wasm_encoder::ValType::FuncRef,
            limits: Limits {
                min: table.limits().min(),
                max: table.limits().max(),
            },
        });
        self.num_tables += 1;
        self.num_tables
    }
    fn global_section_add_item(&mut self, global: &GlobalType) -> u32 {
        self.global_section.global(
            wasm_encoder::GlobalType {
                val_type: value_to_value(global.content()),
                mutable: global.mutability() == Mutability::Var,
            },
            value_to_instruction(&global.content()),
        );
        self.num_globals += 1;
        self.num_globals
    }
    fn type_section_add_item_func(&mut self, item_type: &FuncType) -> u32 {
        self.type_section.function(
            item_type.params().into_iter().map(|it| value_to_value(&it)),
            item_type
                .results()
                .into_iter()
                .map(|it| value_to_value(&it)),
        );
        self.num_types += 1;
        self.num_types
    }
    fn type_section_add_item_module(&mut self, item_type: &ModuleType) -> u32 {
        self.type_section.module(
            item_type
                .imports()
                .into_iter()
                .enumerate()
                .map(|(i, it)| (it.module(), it.name(), extern_to_entity(&it.ty(), i as u32))),
            item_type
                .exports()
                .into_iter()
                .enumerate()
                .map(|(i, it)| (it.name(), extern_to_entity(&it.ty(), i as u32))),
        );
        self.num_types += 1;
        self.num_types
    }
    fn type_section_add_item_instance(&mut self, item_type: &InstanceType) -> u32 {
        self.type_section.instance(
            item_type
                .exports()
                .into_iter()
                .enumerate()
                .map(|(i, t)| (t.name(), extern_to_entity(&t.ty(), i as u32))),
        );
        self.num_types += 1;
        self.num_types
    }
    fn func_section_add_item(&mut self, item_type: &FuncType, type_index: u32) -> u32 {
        self.function_section.function(type_index);
        self.num_funcs += 1;

        let locals = vec![];
        let mut func = Function::new(locals);
        for t in item_type.results() {
            func.instruction(value_to_instruction(&t));
        }
        func.instruction(Instruction::End);
        self.code_section.function(&func);
        self.num_funcs
    }
    fn module_init<'a>(&mut self, module: &'a mut Module, item_type: &ModuleType) -> &'a Module {
        let mut import_section = ImportSection::new();

        for (i, imp) in item_type.imports().into_iter().enumerate() {
            import_section.import(
                imp.module(),
                imp.name(),
                extern_to_entity(&imp.ty(), i as u32),
            );
        }
        module.section(&import_section);

        let mut export_section = ExportSection::new();
        for (i, exp) in item_type.exports().into_iter().enumerate() {
            export_section.export(&exp.name(), extern_to_export(&exp.ty(), i as u32));
        }
        module.section(&export_section);
        module
    }
}
fn value_to_instruction(ty: &ValType) -> Instruction {
    match ty {
        ValType::I32 => Instruction::I32Const(0),
        ValType::I64 => Instruction::I64Const(0),
        ValType::F32 => Instruction::F32Const(0.0),
        ValType::F64 => Instruction::F64Const(0.0),
        ValType::V128 => Instruction::V128Const(0i128),
        ValType::ExternRef => Instruction::RefNull(wasm_encoder::ValType::ExternRef),
        ValType::FuncRef => Instruction::RefNull(wasm_encoder::ValType::FuncRef),
    }
}
fn extern_to_entity(val: &wasmtime::ExternType, num_types: u32) -> wasm_encoder::EntityType {
    match val {
        wasmtime::ExternType::Func(_) => wasm_encoder::EntityType::Function(num_types),
        wasmtime::ExternType::Global(x) => {
            wasm_encoder::EntityType::Global(wasm_encoder::GlobalType {
                val_type: value_to_value(x.content()),
                mutable: x.mutability() == Mutability::Var,
            })
        }
        wasmtime::ExternType::Table(x) => {
            wasm_encoder::EntityType::Table(wasm_encoder::TableType {
                element_type: value_to_value(&x.element()),
                limits: wasm_encoder::Limits {
                    min: x.limits().min(),
                    max: x.limits().max(),
                },
            })
        }
        wasmtime::ExternType::Memory(x) => {
            wasm_encoder::EntityType::Memory(wasm_encoder::MemoryType {
                limits: wasm_encoder::Limits {
                    min: x.limits().min(),
                    max: x.limits().max(),
                },
            })
        }
        wasmtime::ExternType::Instance(_) => wasm_encoder::EntityType::Instance(num_types),
        wasmtime::ExternType::Module(_) => wasm_encoder::EntityType::Module(num_types),
    }
}
fn value_to_value(from: &ValType) -> wasm_encoder::ValType {
    match from {
        ValType::I32 => wasm_encoder::ValType::I32,
        ValType::I64 => wasm_encoder::ValType::I64,
        ValType::F32 => wasm_encoder::ValType::F32,
        ValType::F64 => wasm_encoder::ValType::F64,
        ValType::V128 => wasm_encoder::ValType::V128,
        ValType::ExternRef => wasm_encoder::ValType::ExternRef,
        ValType::FuncRef => wasm_encoder::ValType::FuncRef,
    }
}
fn extern_to_export(val: &wasmtime::ExternType, index: u32) -> wasm_encoder::Export {
    match val {
        wasmtime::ExternType::Func(_) => Export::Function(index),
        wasmtime::ExternType::Global(_) => Export::Global(index),
        wasmtime::ExternType::Table(_) => Export::Table(index),
        wasmtime::ExternType::Memory(_) => Export::Memory(index),
        wasmtime::ExternType::Instance(_) => Export::Instance(index),
        wasmtime::ExternType::Module(_) => Export::Module(index),
    }
}
