pub(crate) mod clock;
pub(crate) mod fd;
pub(crate) mod osdir;
pub(crate) mod osfile;
pub(crate) mod oshandle;
pub(crate) mod osother;
pub(crate) mod path;
pub(crate) mod poll;
pub(crate) mod stdio;

use crate::handle::{Fdflags, Filestat, Filetype, HandleRights, Oflags, Rights, RightsExt};
use crate::sys::AsFile;
use crate::{Error, Result};
use std::convert::{TryFrom, TryInto};
use std::fs::File;
use std::mem::ManuallyDrop;
use std::os::windows::prelude::{AsRawHandle, FromRawHandle};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};
use std::{io, string};
use winx::file::{CreationDisposition, Flags};

impl<T: AsRawHandle> AsFile for T {
    fn as_file(&self) -> io::Result<ManuallyDrop<File>> {
        let file = unsafe { File::from_raw_handle(self.as_raw_handle()) };
        Ok(ManuallyDrop::new(file))
    }
}

pub(super) fn get_file_type(file: &File) -> io::Result<Filetype> {
    let file_type = unsafe { winx::file::get_file_type(file.as_raw_handle())? };
    let file_type = if file_type.is_char() {
        // character file: LPT device or console
        // TODO: rule out LPT device
        Filetype::CharacterDevice
    } else if file_type.is_disk() {
        // disk file: file, dir or disk device
        let meta = file.metadata()?;
        if meta.is_dir() {
            Filetype::Directory
        } else if meta.is_file() {
            Filetype::RegularFile
        } else if meta.file_type().is_symlink() {
            Filetype::SymbolicLink
        } else {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        }
    } else if file_type.is_pipe() {
        // pipe object: socket, named pipe or anonymous pipe
        // TODO: what about pipes, etc?
        Filetype::SocketStream
    } else {
        return Err(io::Error::from_raw_os_error(libc::EINVAL));
    };
    Ok(file_type)
}

pub(super) fn get_rights(file_type: &Filetype) -> io::Result<HandleRights> {
    let (base, inheriting) = match file_type {
        Filetype::BlockDevice => (
            Rights::block_device_base(),
            Rights::block_device_inheriting(),
        ),
        Filetype::CharacterDevice => (Rights::tty_base(), Rights::tty_base()),
        Filetype::SocketDgram | Filetype::SocketStream => {
            (Rights::socket_base(), Rights::socket_inheriting())
        }
        Filetype::SymbolicLink | Filetype::Unknown => (
            Rights::regular_file_base(),
            Rights::regular_file_inheriting(),
        ),
        Filetype::Directory => (Rights::directory_base(), Rights::directory_inheriting()),
        Filetype::RegularFile => (
            Rights::regular_file_base(),
            Rights::regular_file_inheriting(),
        ),
    };
    let rights = HandleRights::new(base, inheriting);
    Ok(rights)
}

pub fn preopen_dir<P: AsRef<Path>>(path: P) -> io::Result<File> {
    use std::fs::OpenOptions;
    use std::os::windows::fs::OpenOptionsExt;
    use winapi::um::winbase::FILE_FLAG_BACKUP_SEMANTICS;

    // To open a directory using CreateFile, specify the
    // FILE_FLAG_BACKUP_SEMANTICS flag as part of dwFileFlags...
    // cf. https://docs.microsoft.com/en-us/windows/desktop/api/fileapi/nf-fileapi-createfile2
    OpenOptions::new()
        .create(false)
        .write(true)
        .read(true)
        .attributes(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
}

pub(crate) fn file_serial_no(file: &File) -> io::Result<u64> {
    let info = winx::file::get_fileinfo(file)?;
    let high = info.nFileIndexHigh;
    let low = info.nFileIndexLow;
    let no = (u64::from(high) << 32) | u64::from(low);
    Ok(no)
}

impl From<string::FromUtf16Error> for Error {
    fn from(_err: string::FromUtf16Error) -> Self {
        Self::Ilseq
    }
}

fn num_hardlinks(file: &File) -> io::Result<u64> {
    Ok(winx::file::get_fileinfo(file)?.nNumberOfLinks.into())
}

fn device_id(file: &File) -> io::Result<u64> {
    Ok(winx::file::get_fileinfo(file)?.dwVolumeSerialNumber.into())
}

fn change_time(file: &File) -> io::Result<i64> {
    winx::file::change_time(file)
}

fn systemtime_to_timestamp(st: SystemTime) -> Result<u64> {
    st.duration_since(UNIX_EPOCH)
        .map_err(|_| Error::Inval)? // date earlier than UNIX_EPOCH
        .as_nanos()
        .try_into()
        .map_err(Into::into) // u128 doesn't fit into u64
}

impl TryFrom<&File> for Filestat {
    type Error = Error;

    fn try_from(file: &File) -> Result<Self> {
        match get_file_type(file)? {
            // Most (all?) file operations below are not supported for character devices and pipes on windows
            t @ Filetype::CharacterDevice | t @ Filetype::SocketStream => Ok(Filestat {
                dev: 0u64,
                ino: 0u64,
                nlink: 1u64,
                size: 0,
                atim: 0,
                ctim: 0,
                mtim: 0,
                filetype: t,
            }),
            _ => {
                let metadata = file.metadata()?;
                Ok(Filestat {
                    dev: device_id(file)?,
                    ino: file_serial_no(file)?,
                    nlink: num_hardlinks(file)?.try_into()?, // u64 doesn't fit into u32
                    size: metadata.len(),
                    atim: systemtime_to_timestamp(metadata.accessed()?)?,
                    ctim: change_time(file)?.try_into()?, // i64 doesn't fit into u64
                    mtim: systemtime_to_timestamp(metadata.modified()?)?,
                    filetype: metadata.file_type().into(),
                })
            }
        }
    }
}

impl From<Oflags> for CreationDisposition {
    fn from(oflags: Oflags) -> Self {
        if oflags.contains(Oflags::CREAT) {
            if oflags.contains(Oflags::EXCL) {
                CreationDisposition::CREATE_NEW
            } else {
                CreationDisposition::CREATE_ALWAYS
            }
        } else if oflags.contains(Oflags::TRUNC) {
            CreationDisposition::TRUNCATE_EXISTING
        } else {
            CreationDisposition::OPEN_EXISTING
        }
    }
}

impl From<Fdflags> for Flags {
    fn from(fdflags: Fdflags) -> Self {
        // Enable backup semantics so directories can be opened as files
        let mut flags = Flags::FILE_FLAG_BACKUP_SEMANTICS;

        // Note: __WASI_FDFLAGS_NONBLOCK is purposely being ignored for files
        // While Windows does inherently support a non-blocking mode on files, the WASI API will
        // treat I/O operations on files as synchronous. WASI might have an async-io API in the future.

        // Technically, Windows only supports __WASI_FDFLAGS_SYNC, but treat all the flags as the same.
        if fdflags.contains(Fdflags::DSYNC)
            || fdflags.contains(Fdflags::RSYNC)
            || fdflags.contains(Fdflags::SYNC)
        {
            flags.insert(Flags::FILE_FLAG_WRITE_THROUGH);
        }

        flags
    }
}
