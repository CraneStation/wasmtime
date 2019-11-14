use wasi::wasi_unstable;

fn test_sock_socket() {
    let mut sock_fd = wasi_unstable::Fd::max_value() - 1;
    let mut rights = 0;
    println!("before the call");
    let res = unsafe {
        wasi_unstable::raw::__wasi_sock_socket(1, 1, 1, &mut sock_fd, &mut rights)
    };
    println!("the result is {:?}", res);
}

fn main() {
    test_sock_socket();
}
