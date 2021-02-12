use anyhow::Context;
use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;
use wasi_common::{
    pipe::{ReadPipe, WritePipe},
    table::Table,
    WasiCtx,
};
use wasmtime::{Linker, Module, Store};

pub fn instantiate(data: &[u8], bin_name: &str, workspace: Option<&Path>) -> anyhow::Result<()> {
    let stdout = WritePipe::new_in_memory();
    let stderr = WritePipe::new_in_memory();

    let r = {
        let store = Store::default();

        // Create our wasi context.
        let mut builder = WasiCtx::builder(
            wasi_cap_std_sync::random_ctx(),
            wasi_cap_std_sync::clocks_ctx(),
            wasi_cap_std_sync::sched_ctx(), // We shouldnt actually use this, but we will until we make a virtual scheduler
            Rc::new(RefCell::new(Table::new())),
        );

        builder = builder
            .arg(bin_name)?
            .arg(".")?
            .stdin(Box::new(ReadPipe::from(Vec::new())))
            .stdout(Box::new(stdout.clone()))
            .stderr(Box::new(stderr.clone()));

        // fs needs to outlive instance
        let _fs = if workspace.is_some() {
            let fs = wasi_virtfs::Filesystem::new(wasi_cap_std_sync::clocks_ctx().system, 420); // XXX this is duplicated - should be reference counted so I can use the same in the builder...
            builder = builder.preopened_dir(fs.root(), ".")?;
            Some(fs)
        } else {
            None
        };
        let wasi = wasmtime_wasi::Wasi::new(&store, builder.build()?);

        let mut linker = Linker::new(&store);

        wasi.add_to_linker(&mut linker)?;

        let module = Module::new(store.engine(), &data).context("failed to create wasm module")?;
        let instance = linker.instantiate(&module)?;
        let start = instance.get_func("_start").unwrap();
        let with_type = start.get0::<()>()?;
        with_type().map_err(anyhow::Error::from)
    };

    match r {
        Ok(()) => Ok(()),
        Err(trap) => {
            let stdout = stdout
                .try_into_inner()
                .expect("sole ref to stdout")
                .into_inner();
            if !stdout.is_empty() {
                println!("guest stdout:\n{}\n===", String::from_utf8_lossy(&stdout));
            }
            let stderr = stderr
                .try_into_inner()
                .expect("sole ref to stderr")
                .into_inner();
            if !stderr.is_empty() {
                println!("guest stderr:\n{}\n===", String::from_utf8_lossy(&stderr));
            }
            Err(trap.context(format!("error while testing Wasm module '{}'", bin_name,)))
        }
    }
}
