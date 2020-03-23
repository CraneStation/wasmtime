use wasmtime::*;
use anyhow::Result;

#[test]
fn link_undefined() -> Result<()> {
    let store = Store::default();
    let mut linker = Linker::new(&store);
    let module = Module::new(&store, r#"(module (import "" "" (func)))"#)?;
    assert!(linker.instantiate(&module).is_err());
    let module = Module::new(&store, r#"(module (import "" "" (global i32)))"#)?;
    assert!(linker.instantiate(&module).is_err());
    let module = Module::new(&store, r#"(module (import "" "" (memory 1)))"#)?;
    assert!(linker.instantiate(&module).is_err());
    let module = Module::new(&store, r#"(module (import "" "" (table 1 funcref)))"#)?;
    assert!(linker.instantiate(&module).is_err());
    Ok(())
}

#[test]
fn link_twice_bad() -> Result<()> {
    let store = Store::default();
    let mut linker = Linker::new(&store);

    // functions
    linker.func("", "", || {})?;
    assert!(linker.func("", "", || {}).is_err());
    assert!(linker.func("", "", || -> Result<(), Trap> { loop {} }).is_err());
    linker.func("", "", |_: i32| {})?;

    // globals
    let ty = GlobalType::new(ValType::I32, Mutability::Const);
    let global = Global::new(&store, ty, Val::I32(0))?;
    linker.define("", "", global.clone())?;
    assert!(linker.define("", "", global.clone()).is_err());

    let ty = GlobalType::new(ValType::I32, Mutability::Var);
    let global = Global::new(&store, ty, Val::I32(0))?;
    linker.define("", "", global.clone())?;
    assert!(linker.define("", "", global.clone()).is_err());

    let ty = GlobalType::new(ValType::I64, Mutability::Const);
    let global = Global::new(&store, ty, Val::I64(0))?;
    linker.define("", "", global.clone())?;
    assert!(linker.define("", "", global.clone()).is_err());

    // memories
    let ty = MemoryType::new(Limits::new(1, None));
    let memory = Memory::new(&store, ty);
    linker.define("", "", memory.clone())?;
    assert!(linker.define("", "", memory.clone()).is_err());
    let ty = MemoryType::new(Limits::new(2, None));
    let memory = Memory::new(&store, ty);
    assert!(linker.define("", "", memory.clone()).is_err());

    // tables
    let ty = TableType::new(ValType::FuncRef, Limits::new(1, None));
    let table = Table::new(&store, ty, Val::AnyRef(AnyRef::Null))?;
    linker.define("", "", table.clone())?;
    assert!(linker.define("", "", table.clone()).is_err());
    let ty = TableType::new(ValType::FuncRef, Limits::new(2, None));
    let table = Table::new(&store, ty, Val::AnyRef(AnyRef::Null))?;
    assert!(linker.define("", "", table.clone()).is_err());
    Ok(())
}
