//! Dummy implementations of things that a Wasm module can import.
use wasmtime::*;
mod wencoder_generator;
use wencoder_generator::WencoderGenerator;

/// Create a set of dummy functions/globals/etc for the given imports.
pub fn dummy_linker<'module>(store: &Store, module: &Module) -> Linker {
    let mut linker = Linker::new(store);
    linker.allow_shadowing(true);
    for import in module.imports() {
        match import.name() {
            Some(name) => {
                linker
                    .define(import.module(), name, dummy_extern(store, import.ty()))
                    .unwrap();
            }
            None => match import.ty() {
                ExternType::Instance(ty) => {
                    for ty in ty.exports() {
                        linker
                            .define(import.module(), ty.name(), dummy_extern(store, ty.ty()))
                            .unwrap();
                    }
                }
                other => {
                    linker
                        .define_name(import.module(), dummy_extern(store, other))
                        .unwrap();
                }
            },
        }
    }
    linker
}

/// Construct a dummy `Extern` from its type signature
pub fn dummy_extern(store: &Store, ty: ExternType) -> Extern {
    match ty {
        ExternType::Func(func_ty) => Extern::Func(dummy_func(store, func_ty)),
        ExternType::Global(global_ty) => Extern::Global(dummy_global(store, global_ty)),
        ExternType::Table(table_ty) => Extern::Table(dummy_table(store, table_ty)),
        ExternType::Memory(mem_ty) => Extern::Memory(dummy_memory(store, mem_ty)),
        ExternType::Instance(instance_ty) => Extern::Instance(dummy_instance(store, instance_ty)),
        ExternType::Module(module_ty) => Extern::Module(dummy_module(store, module_ty)),
    }
}
/// Construct a dummy function for the given function type
pub fn dummy_func(store: &Store, ty: FuncType) -> Func {
    Func::new(store, ty.clone(), move |_, _, results| {
        for (ret_ty, result) in ty.results().zip(results) {
            *result = dummy_value(ret_ty);
        }
        Ok(())
    })
}

/// Construct a dummy value for the given value type.
pub fn dummy_value(val_ty: ValType) -> Val {
    match val_ty {
        ValType::I32 => Val::I32(0),
        ValType::I64 => Val::I64(0),
        ValType::F32 => Val::F32(0),
        ValType::F64 => Val::F64(0),
        ValType::V128 => Val::V128(0),
        ValType::ExternRef => Val::ExternRef(None),
        ValType::FuncRef => Val::FuncRef(None),
    }
}

/// Construct a sequence of dummy values for the given types.
pub fn dummy_values(val_tys: impl IntoIterator<Item = ValType>) -> Vec<Val> {
    val_tys.into_iter().map(dummy_value).collect()
}

/// Construct a dummy global for the given global type.
pub fn dummy_global(store: &Store, ty: GlobalType) -> Global {
    let val = dummy_value(ty.content().clone());
    Global::new(store, ty, val).unwrap()
}

/// Construct a dummy table for the given table type.
pub fn dummy_table(store: &Store, ty: TableType) -> Table {
    let init_val = dummy_value(ty.element().clone());
    Table::new(store, ty, init_val).unwrap()
}

/// Construct a dummy memory for the given memory type.
pub fn dummy_memory(store: &Store, ty: MemoryType) -> Memory {
    Memory::new(store, ty).unwrap()
}

/// Construct a dummy instance for the given instance type.
///
/// This is done by using the expected type to generate a module on-the-fly
/// which we the instantiate.
pub fn dummy_instance(store: &Store, ty: InstanceType) -> Instance {
    let mut wgen = WencoderGenerator::new();
    for ty in ty.exports() {
        wgen.export(&ty);
    }
    let module = Module::new(store.engine(), &wgen.finish()).unwrap();
    Instance::new(store, &module, &[]).unwrap()
}
/// Construct a dummy module for the given module type.
///
/// This is done by using the expected type to generate a module on-the-fly.
pub fn dummy_module(store: &Store, ty: ModuleType) -> Module {
    let mut wgen = WencoderGenerator::new();
    for ty in ty.imports() {
        wgen.import(&ty);
    }
    for ty in ty.exports() {
        wgen.export(&ty)
    }
    Module::new(store.engine(), &wgen.finish()).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn store() -> Store {
        let mut config = Config::default();
        config.wasm_module_linking(true);
        config.wasm_multi_memory(true);
        let engine = wasmtime::Engine::new(&config).unwrap();
        Store::new(&engine)
    }

    #[test]
    fn dummy_table_import() {
        let store = store();
        let table = dummy_table(
            &store,
            TableType::new(ValType::ExternRef, Limits::at_least(10)),
        );
        assert_eq!(table.size(), 10);
        for i in 0..10 {
            assert!(table.get(i).unwrap().unwrap_externref().is_none());
        }
    }

    #[test]
    fn dummy_global_import() {
        let store = store();
        let global = dummy_global(&store, GlobalType::new(ValType::I32, Mutability::Const));
        assert_eq!(global.val_type(), ValType::I32);
        assert_eq!(global.mutability(), Mutability::Const);
    }

    #[test]
    fn dummy_memory_import() {
        let store = store();
        let memory = dummy_memory(&store, MemoryType::new(Limits::at_least(1)));
        assert_eq!(memory.size(), 1);
    }

    #[test]
    fn dummy_function_import() {
        let store = store();
        let func_ty = FuncType::new(vec![ValType::I32], vec![ValType::I64]);
        let func = dummy_func(&store, func_ty.clone());
        assert_eq!(func.ty(), func_ty);
    }

    #[test]
    fn dummy_instance_import() {
        let store = store();

        let mut instance_ty = InstanceType::new();

        // Functions.
        instance_ty.add_named_export("func0", FuncType::new(vec![ValType::I32], vec![]).into());
        instance_ty.add_named_export("func1", FuncType::new(vec![], vec![ValType::I64]).into());

        // Globals.
        instance_ty.add_named_export(
            "global0",
            GlobalType::new(ValType::I32, Mutability::Const).into(),
        );
        instance_ty.add_named_export(
            "global1",
            GlobalType::new(ValType::I64, Mutability::Var).into(),
        );

        // Tables.
        instance_ty.add_named_export(
            "table0",
            TableType::new(ValType::ExternRef, Limits::at_least(1)).into(),
        );
        instance_ty.add_named_export(
            "table1",
            TableType::new(ValType::ExternRef, Limits::at_least(1)).into(),
        );

        // Memories.
        instance_ty.add_named_export("memory0", MemoryType::new(Limits::at_least(1)).into());
        instance_ty.add_named_export("memory1", MemoryType::new(Limits::at_least(1)).into());

        // Modules.
        instance_ty.add_named_export("module0", ModuleType::new().into());
        instance_ty.add_named_export("module1", ModuleType::new().into());

        // Instances.
        instance_ty.add_named_export("instance0", InstanceType::new().into());
        instance_ty.add_named_export("instance1", InstanceType::new().into());

        let instance = dummy_instance(&store, instance_ty.clone());

        let mut expected_exports = vec![
            "func0",
            "func1",
            "global0",
            "global1",
            "table0",
            "table1",
            "memory0",
            "memory1",
            "module0",
            "module1",
            "instance0",
            "instance1",
        ]
        .into_iter()
        .collect::<HashSet<_>>();
        for exp in instance.ty().exports() {
            let was_expected = expected_exports.remove(exp.name());
            assert!(was_expected);
        }
        assert!(expected_exports.is_empty());
    }

    #[test]
    fn dummy_module_import() {
        let store = store();

        let mut module_ty = ModuleType::new();

        // Multiple exported and imported functions.
        module_ty.add_named_export("func0", FuncType::new(vec![ValType::I32], vec![]).into());
        module_ty.add_named_export("func1", FuncType::new(vec![], vec![ValType::I64]).into());
        module_ty.add_named_import(
            "func2",
            None,
            FuncType::new(vec![ValType::I64], vec![]).into(),
        );
        module_ty.add_named_import(
            "func3",
            None,
            FuncType::new(vec![], vec![ValType::I32]).into(),
        );

        // Multiple exported and imported globals.
        module_ty.add_named_export(
            "global0",
            GlobalType::new(ValType::I32, Mutability::Const).into(),
        );
        module_ty.add_named_export(
            "global1",
            GlobalType::new(ValType::I64, Mutability::Var).into(),
        );
        module_ty.add_named_import(
            "global2",
            None,
            GlobalType::new(ValType::I32, Mutability::Var).into(),
        );
        module_ty.add_named_import(
            "global3",
            None,
            GlobalType::new(ValType::I64, Mutability::Const).into(),
        );

        // Multiple exported and imported tables.
        module_ty.add_named_export(
            "table0",
            TableType::new(ValType::ExternRef, Limits::at_least(1)).into(),
        );
        module_ty.add_named_export(
            "table1",
            TableType::new(ValType::ExternRef, Limits::at_least(1)).into(),
        );
        module_ty.add_named_import(
            "table2",
            None,
            TableType::new(ValType::ExternRef, Limits::at_least(1)).into(),
        );
        module_ty.add_named_import(
            "table3",
            None,
            TableType::new(ValType::ExternRef, Limits::at_least(1)).into(),
        );

        // Multiple exported and imported memories.
        module_ty.add_named_export("memory0", MemoryType::new(Limits::at_least(1)).into());
        module_ty.add_named_export("memory1", MemoryType::new(Limits::at_least(1)).into());
        module_ty.add_named_import("memory2", None, MemoryType::new(Limits::at_least(1)).into());
        module_ty.add_named_import("memory3", None, MemoryType::new(Limits::at_least(1)).into());

        // An exported and an imported module.
        module_ty.add_named_export("module0", ModuleType::new().into());
        module_ty.add_named_import("module1", None, ModuleType::new().into());

        // An exported and an imported instance.
        module_ty.add_named_export("instance0", InstanceType::new().into());
        module_ty.add_named_import("instance1", None, InstanceType::new().into());

        // Create the module.
        let module = dummy_module(&store, module_ty);

        // Check that we have the expected exports.
        assert!(module.get_export("func0").is_some());
        assert!(module.get_export("func1").is_some());
        assert!(module.get_export("global0").is_some());
        assert!(module.get_export("global1").is_some());
        assert!(module.get_export("table0").is_some());
        assert!(module.get_export("table1").is_some());
        assert!(module.get_export("memory0").is_some());
        assert!(module.get_export("memory1").is_some());
        assert!(module.get_export("instance0").is_some());
        assert!(module.get_export("module0").is_some());

        // Check that we have the exported imports.
        let mut expected_imports = vec![
            "func2",
            "func3",
            "global2",
            "global3",
            "table2",
            "table3",
            "memory2",
            "memory3",
            "instance1",
            "module1",
        ]
        .into_iter()
        .collect::<HashSet<_>>();
        for imp in module.imports() {
            assert!(imp.name().is_none());
            let was_expected = expected_imports.remove(imp.module());
            assert!(was_expected);
        }
        assert!(expected_imports.is_empty());
    }
}
