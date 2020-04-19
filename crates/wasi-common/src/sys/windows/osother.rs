use super::get_file_type;
use super::oshandle::OsHandle;
use crate::handle::HandleRights;
use crate::sys::osother::OsOther;
use crate::wasi::{types, RightsExt};
use std::convert::TryFrom;
use std::fs::File;
use std::io;
use std::ops::Deref;
use std::os::windows::prelude::{AsRawHandle, FromRawHandle, IntoRawHandle, RawHandle};

impl AsRawHandle for OsOther {
    fn as_raw_handle(&self) -> RawHandle {
        self.deref().as_raw_handle()
    }
}

impl TryFrom<File> for OsOther {
    type Error = io::Error;

    fn try_from(file: File) -> io::Result<Self> {
        let file_type = get_file_type(&file)?;
        let rights = get_rights(&file_type)?;
        let handle = unsafe { OsHandle::from_raw_handle(file.into_raw_handle()) };
        Ok(Self::new(file_type, rights, handle))
    }
}

fn get_rights(file_type: &types::Filetype) -> io::Result<HandleRights> {
    let (base, inheriting) = match file_type {
        types::Filetype::BlockDevice => (
            types::Rights::block_device_base(),
            types::Rights::block_device_inheriting(),
        ),
        types::Filetype::CharacterDevice => (types::Rights::tty_base(), types::Rights::tty_base()),
        types::Filetype::SocketDgram | types::Filetype::SocketStream => (
            types::Rights::socket_base(),
            types::Rights::socket_inheriting(),
        ),
        types::Filetype::SymbolicLink | types::Filetype::Unknown => (
            types::Rights::regular_file_base(),
            types::Rights::regular_file_inheriting(),
        ),
        _ => unreachable!("these should have been handled already!"),
    };
    let rights = HandleRights::new(base, inheriting);
    Ok(rights)
}
