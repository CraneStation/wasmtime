#![allow(non_camel_case_types)]
use crate::Result;
use std::net::{TcpStream, ToSocketAddrs};

pub(crate) fn sock_connect(addr: impl ToSocketAddrs) -> Result<TcpStream> {
    TcpStream::connect(addr)
        .map_err(Into::into) // some number just to compile
}
