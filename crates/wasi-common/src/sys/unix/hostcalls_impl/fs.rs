#![allow(non_camel_case_types)]
#![allow(unused_unsafe)]
use crate::helpers::systemtime_to_timestamp;
use crate::hostcalls_impl::{FileType, PathGet};
use crate::sys::host_impl;
use crate::{wasi, Error, Result};
use std::convert::TryInto;
use std::fs::{File, Metadata};
use std::os::unix::fs::FileExt;
use std::os::unix::prelude::{AsRawFd, FromRawFd};

cfg_if::cfg_if! {
    if #[cfg(any(target_os = "linux",
                 target_os = "andoird",
                 target_os = "emscripten"))] {
        pub(crate) use super::super::linux::hostcalls_impl::*;
    } else if #[cfg(any(
            target_os = "macos",
            target_os = "netbsd",
            target_os = "freebsd",
            target_os = "openbsd",
            target_os = "ios",
            target_os = "dragonfly"
    ))] {
        pub(crate) use super::super::bsd::hostcalls_impl::*;
    }
}

pub(crate) fn fd_pread(
    file: &File,
    buf: &mut [u8],
    offset: wasi::__wasi_filesize_t,
) -> Result<usize> {
    file.read_at(buf, offset).map_err(Into::into)
}

pub(crate) fn fd_pwrite(file: &File, buf: &[u8], offset: wasi::__wasi_filesize_t) -> Result<usize> {
    file.write_at(buf, offset).map_err(Into::into)
}

pub(crate) fn fd_fdstat_get(fd: &File) -> Result<wasi::__wasi_fdflags_t> {
    use yanix::{
        file::OFlag,
        sys::{fcntl, F_GETFL},
    };
    let flags = fcntl(fd.as_raw_fd(), F_GETFL)?;
    let flags = OFlag::from_bits_truncate(flags);
    Ok(host_impl::fdflags_from_nix(flags))
}

pub(crate) fn fd_fdstat_set_flags(fd: &File, fdflags: wasi::__wasi_fdflags_t) -> Result<()> {
    use yanix::sys::{fcntl, F_SETFL};
    let nix_flags = host_impl::nix_from_fdflags(fdflags);
    let _ = fcntl(fd.as_raw_fd(), F_SETFL(nix_flags))?;
    Ok(())
}

pub(crate) fn path_create_directory(resolved: PathGet) -> Result<()> {
    use yanix::file::{mkdirat, Mode};
    let mode = Mode::S_IRWXU | Mode::S_IRWXG | Mode::S_IRWXO;
    mkdirat(resolved.dirfd().as_raw_fd(), resolved.path(), mode).map_err(Into::into)
}

pub(crate) fn path_link(resolved_old: PathGet, resolved_new: PathGet) -> Result<()> {
    use yanix::file::{linkat, AtFlags};
    let flags = AtFlags::AT_SYMLINK_FOLLOW;
    linkat(
        resolved_old.dirfd().as_raw_fd(),
        resolved_old.path(),
        resolved_new.dirfd().as_raw_fd(),
        resolved_new.path(),
        flags,
    )
    .map_err(Into::into)
}

pub(crate) fn path_open(
    resolved: PathGet,
    read: bool,
    write: bool,
    oflags: wasi::__wasi_oflags_t,
    fs_flags: wasi::__wasi_fdflags_t,
) -> Result<File> {
    use yanix::{
        errno::Errno,
        file::{openat, AtFlags, Mode, OFlag},
        sys::{fstatat, SFlag},
    };

    let mut nix_all_oflags = if read && write {
        OFlag::O_RDWR
    } else if write {
        OFlag::O_WRONLY
    } else {
        OFlag::O_RDONLY
    };

    // on non-Capsicum systems, we always want nofollow
    nix_all_oflags.insert(OFlag::O_NOFOLLOW);

    // convert open flags
    nix_all_oflags.insert(host_impl::nix_from_oflags(oflags));

    // convert file descriptor flags
    nix_all_oflags.insert(host_impl::nix_from_fdflags(fs_flags));

    // Call openat. Use mode 0o666 so that we follow whatever the user's
    // umask is, but don't set the executable flag, because it isn't yet
    // meaningful for WASI programs to create executable files.

    log::debug!("path_open resolved = {:?}", resolved);
    log::debug!("path_open oflags = {:?}", nix_all_oflags);

    let new_fd = match openat(
        resolved.dirfd().as_raw_fd(),
        resolved.path(),
        nix_all_oflags,
        Mode::from_bits_truncate(0o666),
    ) {
        Ok(fd) => fd,
        Err(e) => {
            if let yanix::Error::Errno(errno) = e {
                match errno {
                    // Linux returns ENXIO instead of EOPNOTSUPP when opening a socket
                    Errno::ENXIO => {
                        if let Ok(stat) = fstatat(
                            resolved.dirfd().as_raw_fd(),
                            resolved.path(),
                            AtFlags::AT_SYMLINK_NOFOLLOW,
                        ) {
                            if SFlag::from_bits_truncate(stat.st_mode).contains(SFlag::S_IFSOCK) {
                                return Err(Error::ENOTSUP);
                            } else {
                                return Err(Error::ENXIO);
                            }
                        } else {
                            return Err(Error::ENXIO);
                        }
                    }
                    // Linux returns ENOTDIR instead of ELOOP when using O_NOFOLLOW|O_DIRECTORY
                    // on a symlink.
                    Errno::ENOTDIR
                        if !(nix_all_oflags & (OFlag::O_NOFOLLOW | OFlag::O_DIRECTORY))
                            .is_empty() =>
                    {
                        if let Ok(stat) = fstatat(
                            resolved.dirfd().as_raw_fd(),
                            resolved.path(),
                            AtFlags::AT_SYMLINK_NOFOLLOW,
                        ) {
                            if SFlag::from_bits_truncate(stat.st_mode).contains(SFlag::S_IFLNK) {
                                return Err(Error::ELOOP);
                            }
                        }
                        return Err(Error::ENOTDIR);
                    }
                    // FreeBSD returns EMLINK instead of ELOOP when using O_NOFOLLOW on
                    // a symlink.
                    Errno::EMLINK if !(nix_all_oflags & OFlag::O_NOFOLLOW).is_empty() => {
                        return Err(Error::ELOOP);
                    }
                    errno => return Err(host_impl::errno_from_nix(errno)),
                }
            } else {
                return Err(e.into());
            }
        }
    };

    log::debug!("path_open (host) new_fd = {:?}", new_fd);

    // Determine the type of the new file descriptor and which rights contradict with this type
    Ok(unsafe { File::from_raw_fd(new_fd) })
}

pub(crate) fn path_readlink(resolved: PathGet, buf: &mut [u8]) -> Result<usize> {
    use std::cmp::min;
    use yanix::file::readlinkat;
    let read_link = readlinkat(resolved.dirfd().as_raw_fd(), resolved.path())
        .map_err(Into::into)
        .and_then(host_impl::path_from_host)?;
    let copy_len = min(read_link.len(), buf.len());
    if copy_len > 0 {
        buf[..copy_len].copy_from_slice(&read_link.as_bytes()[..copy_len]);
    }
    Ok(copy_len)
}

pub(crate) fn fd_filestat_get_impl(file: &std::fs::File) -> Result<wasi::__wasi_filestat_t> {
    use std::os::unix::fs::MetadataExt;

    let metadata = file.metadata()?;
    Ok(wasi::__wasi_filestat_t {
        dev: metadata.dev(),
        ino: metadata.ino(),
        nlink: metadata.nlink().try_into()?, // u64 doesn't fit into u32
        size: metadata.len(),
        atim: systemtime_to_timestamp(metadata.accessed()?)?,
        ctim: metadata.ctime().try_into()?, // i64 doesn't fit into u64
        mtim: systemtime_to_timestamp(metadata.modified()?)?,
        filetype: filetype(file, &metadata)?.to_wasi(),
    })
}

fn filetype(file: &File, metadata: &Metadata) -> Result<FileType> {
    use std::os::unix::fs::FileTypeExt;
    let ftype = metadata.file_type();
    if ftype.is_file() {
        Ok(FileType::RegularFile)
    } else if ftype.is_dir() {
        Ok(FileType::Directory)
    } else if ftype.is_symlink() {
        Ok(FileType::Symlink)
    } else if ftype.is_char_device() {
        Ok(FileType::CharacterDevice)
    } else if ftype.is_block_device() {
        Ok(FileType::BlockDevice)
    } else if ftype.is_socket() {
        use yanix::socket::{get_socket_type, SockType};
        match get_socket_type(file.as_raw_fd())? {
            SockType::Datagram => Ok(FileType::SocketDgram),
            SockType::Stream => Ok(FileType::SocketStream),
            _ => Ok(FileType::Unknown),
        }
    } else {
        Ok(FileType::Unknown)
    }
}

pub(crate) fn path_filestat_get(
    resolved: PathGet,
    dirflags: wasi::__wasi_lookupflags_t,
) -> Result<wasi::__wasi_filestat_t> {
    use yanix::{file::AtFlags, sys::fstatat};
    let atflags = match dirflags {
        0 => AtFlags::empty(),
        _ => AtFlags::AT_SYMLINK_NOFOLLOW,
    };
    let filestat = fstatat(resolved.dirfd().as_raw_fd(), resolved.path(), atflags)?;
    host_impl::filestat_from_nix(filestat)
}

pub(crate) fn path_filestat_set_times(
    resolved: PathGet,
    dirflags: wasi::__wasi_lookupflags_t,
    st_atim: wasi::__wasi_timestamp_t,
    st_mtim: wasi::__wasi_timestamp_t,
    fst_flags: wasi::__wasi_fstflags_t,
) -> Result<()> {
    use super::super::filetime::*;
    use std::time::{Duration, UNIX_EPOCH};

    let set_atim = fst_flags & wasi::__WASI_FSTFLAGS_ATIM != 0;
    let set_atim_now = fst_flags & wasi::__WASI_FSTFLAGS_ATIM_NOW != 0;
    let set_mtim = fst_flags & wasi::__WASI_FSTFLAGS_MTIM != 0;
    let set_mtim_now = fst_flags & wasi::__WASI_FSTFLAGS_MTIM_NOW != 0;

    if (set_atim && set_atim_now) || (set_mtim && set_mtim_now) {
        return Err(Error::EINVAL);
    }

    let symlink_nofollow = wasi::__WASI_LOOKUPFLAGS_SYMLINK_FOLLOW != dirflags;
    let atim = if set_atim {
        let time = UNIX_EPOCH + Duration::from_nanos(st_atim);
        FileTime::FileTime(filetime::FileTime::from_system_time(time))
    } else if set_atim_now {
        FileTime::Now
    } else {
        FileTime::Omit
    };
    let mtim = if set_mtim {
        let time = UNIX_EPOCH + Duration::from_nanos(st_mtim);
        FileTime::FileTime(filetime::FileTime::from_system_time(time))
    } else if set_mtim_now {
        FileTime::Now
    } else {
        FileTime::Omit
    };

    utimensat(
        resolved.dirfd(),
        resolved.path(),
        atim,
        mtim,
        symlink_nofollow,
    )
    .map_err(Into::into)
}

pub(crate) fn path_remove_directory(resolved: PathGet) -> Result<()> {
    use yanix::file::{unlinkat, AtFlags};
    unlinkat(
        resolved.dirfd().as_raw_fd(),
        resolved.path(),
        AtFlags::AT_REMOVEDIR,
    )
    .map_err(Into::into)
}
