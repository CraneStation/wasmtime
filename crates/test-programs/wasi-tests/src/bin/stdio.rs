use wasi_tests::{STDERR_FD, STDIN_FD, STDOUT_FD};

unsafe fn test_stdio() {
    for fd in &[STDIN_FD, STDOUT_FD, STDERR_FD] {
        wasi::fd_fdstat_get(*fd).expect("fd_fdstat_get on stdio");
        wasi::fd_filestat_get(*fd).expect("fd_filestat_get on stdio");
        wasi::fd_renumber(*fd, *fd + 100).expect("renumbering stdio");
    }
}

fn main() {
    // Run the tests.
    unsafe { test_stdio() }
}
