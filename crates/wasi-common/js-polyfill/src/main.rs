use std::cell::RefCell;
use wasi_common::{WasiCtx, WasiCtxBuilder};

extern "C" {
    fn loadFiles();
}

thread_local! {
    static WASI_CTX: RefCell<Option<WasiCtx>> = RefCell::new(None);
}

fn main() {}

#[no_mangle]
unsafe fn get_wasi_context() -> *mut WasiCtx {
    WASI_CTX.with(|ctx| {
        ctx.borrow_mut()
            .as_mut()
            .expect("WasiCtx should be initialized by now") as *mut _
    })
}

#[allow(non_snake_case)]
#[no_mangle]
fn handleFiles() {
    WASI_CTX.with(|ctx| {
        let wasi_ctx = WasiCtxBuilder::new()
            .inherit_stdio()
            .build()
            .expect("could build WasiCtx with stdio inherited");
        ctx.replace(Some(wasi_ctx));
    });
    unsafe { loadFiles() }
}
