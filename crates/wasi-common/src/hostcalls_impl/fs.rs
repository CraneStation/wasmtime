#![allow(non_camel_case_types)]
use super::fs_helpers::path_get;
use crate::ctx::WasiCtx;
use crate::fdentry::{Descriptor, FdEntry};
use crate::helpers::*;
use crate::memory::*;
use crate::sys::fdentry_impl::determine_type_rights;
use crate::sys::hostcalls_impl::fs_helpers::path_open_rights;
use crate::sys::{host_impl, hostcalls_impl};
use crate::{helpers, host, wasi, wasi32, Error, Result};
use filetime::{set_file_handle_times, FileTime};
use log::trace;
use std::convert::TryInto;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::mem;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub(crate) unsafe fn fd_close(wasi_ctx: &mut WasiCtx, fd: wasi::__wasi_fd_t) -> Result<()> {
    trace!("fd_close(fd={:?})", fd);

    if let Ok(fe) = wasi_ctx.get_fd_entry(fd) {
        // can't close preopened files
        if fe.preopen_path.is_some() {
            return Err(Error::ENOTSUP);
        }
    }

    wasi_ctx.remove_fd_entry(fd)?;
    Ok(())
}

pub(crate) unsafe fn fd_datasync(wasi_ctx: &WasiCtx, fd: wasi::__wasi_fd_t) -> Result<()> {
    trace!("fd_datasync(fd={:?})", fd);

    let fd = wasi_ctx
        .get_fd_entry(fd)?
        .as_descriptor(wasi::__WASI_RIGHT_FD_DATASYNC, 0)?
        .as_file()?;

    fd.sync_data().map_err(Into::into)
}

pub(crate) unsafe fn fd_pread(
    wasi_ctx: &WasiCtx,
    memory: &mut [u8],
    fd: wasi::__wasi_fd_t,
    iovs_ptr: wasi32::uintptr_t,
    iovs_len: wasi32::size_t,
    offset: wasi::__wasi_filesize_t,
    nread: wasi32::uintptr_t,
) -> Result<()> {
    trace!(
        "fd_pread(fd={:?}, iovs_ptr={:#x?}, iovs_len={:?}, offset={}, nread={:#x?})",
        fd,
        iovs_ptr,
        iovs_len,
        offset,
        nread
    );

    let fd = wasi_ctx
        .get_fd_entry(fd)?
        .as_descriptor(wasi::__WASI_RIGHT_FD_READ, 0)?
        .as_file()?;

    let iovs = dec_iovec_slice(memory, iovs_ptr, iovs_len)?;

    if offset > i64::max_value() as u64 {
        return Err(Error::EIO);
    }
    let buf_size = iovs.iter().map(|v| v.buf_len).sum();
    let mut buf = vec![0; buf_size];
    let host_nread = hostcalls_impl::fd_pread(fd, &mut buf, offset)?;
    let mut buf_offset = 0;
    let mut left = host_nread;
    for iov in &iovs {
        if left == 0 {
            break;
        }
        let vec_len = std::cmp::min(iov.buf_len, left);
        std::slice::from_raw_parts_mut(iov.buf as *mut u8, vec_len)
            .copy_from_slice(&buf[buf_offset..buf_offset + vec_len]);
        buf_offset += vec_len;
        left -= vec_len;
    }

    trace!("     | *nread={:?}", host_nread);

    enc_usize_byref(memory, nread, host_nread)
}

pub(crate) unsafe fn fd_pwrite(
    wasi_ctx: &WasiCtx,
    memory: &mut [u8],
    fd: wasi::__wasi_fd_t,
    iovs_ptr: wasi32::uintptr_t,
    iovs_len: wasi32::size_t,
    offset: wasi::__wasi_filesize_t,
    nwritten: wasi32::uintptr_t,
) -> Result<()> {
    trace!(
        "fd_pwrite(fd={:?}, iovs_ptr={:#x?}, iovs_len={:?}, offset={}, nwritten={:#x?})",
        fd,
        iovs_ptr,
        iovs_len,
        offset,
        nwritten
    );

    let fd = wasi_ctx
        .get_fd_entry(fd)?
        .as_descriptor(wasi::__WASI_RIGHT_FD_WRITE, 0)?
        .as_file()?;
    let iovs = dec_ciovec_slice(memory, iovs_ptr, iovs_len)?;

    if offset > i64::max_value() as u64 {
        return Err(Error::EIO);
    }
    let buf_size = iovs.iter().map(|v| v.buf_len).sum();
    let mut buf = Vec::with_capacity(buf_size);
    for iov in &iovs {
        buf.extend_from_slice(std::slice::from_raw_parts(
            iov.buf as *const u8,
            iov.buf_len,
        ));
    }
    let host_nwritten = hostcalls_impl::fd_pwrite(fd, &buf, offset)?;

    trace!("     | *nwritten={:?}", host_nwritten);

    enc_usize_byref(memory, nwritten, host_nwritten)
}

pub(crate) unsafe fn fd_read(
    wasi_ctx: &mut WasiCtx,
    memory: &mut [u8],
    fd: wasi::__wasi_fd_t,
    iovs_ptr: wasi32::uintptr_t,
    iovs_len: wasi32::size_t,
    nread: wasi32::uintptr_t,
) -> Result<()> {
    trace!(
        "fd_read(fd={:?}, iovs_ptr={:#x?}, iovs_len={:?}, nread={:#x?})",
        fd,
        iovs_ptr,
        iovs_len,
        nread
    );

    let mut iovs = dec_iovec_slice(memory, iovs_ptr, iovs_len)?;
    let mut iovs: Vec<io::IoSliceMut> = iovs
        .iter_mut()
        .map(|vec| host::iovec_to_host_mut(vec))
        .collect();

    let maybe_host_nread = match wasi_ctx
        .get_fd_entry_mut(fd)?
        .as_descriptor_mut(wasi::__WASI_RIGHT_FD_READ, 0)?
    {
        Descriptor::OsFile(file) => file.read_vectored(&mut iovs),
        Descriptor::Stdin => io::stdin().lock().read_vectored(&mut iovs),
        _ => return Err(Error::EBADF),
    };

    let host_nread = maybe_host_nread?;

    trace!("     | *nread={:?}", host_nread);

    enc_usize_byref(memory, nread, host_nread)
}

pub(crate) unsafe fn fd_renumber(
    wasi_ctx: &mut WasiCtx,
    from: wasi::__wasi_fd_t,
    to: wasi::__wasi_fd_t,
) -> Result<()> {
    trace!("fd_renumber(from={:?}, to={:?})", from, to);

    if !wasi_ctx.contains_fd_entry(from) || !wasi_ctx.contains_fd_entry(to) {
        return Err(Error::EBADF);
    }

    let from_fe = wasi_ctx.get_fd_entry(from)?;
    let to_fe = wasi_ctx.get_fd_entry(to)?;

    // Don't allow renumbering over a pre-opened resource.
    // TODO: Eventually, we do want to permit this, once libpreopen in
    // userspace is capable of removing entries from its tables as well.
    if from_fe.preopen_path.is_some() || to_fe.preopen_path.is_some() {
        return Err(Error::ENOTSUP);
    }

    // check if stdio fds
    // TODO should we renumber stdio fds?
    if !from_fe.as_descriptor(0, 0)?.is_file() || !to_fe.as_descriptor(0, 0)?.is_file() {
        return Err(Error::EBADF);
    }

    let fe_from_dup = from_fe
        .as_descriptor(0, 0)?
        .as_file()
        .and_then(|file| FdEntry::duplicate(file))?;

    wasi_ctx.insert_fd_entry_at(to, fe_from_dup);
    wasi_ctx.remove_fd_entry(from)?;

    Ok(())
}

pub(crate) unsafe fn fd_seek(
    wasi_ctx: &mut WasiCtx,
    memory: &mut [u8],
    fd: wasi::__wasi_fd_t,
    offset: wasi::__wasi_filedelta_t,
    whence: wasi::__wasi_whence_t,
    newoffset: wasi32::uintptr_t,
) -> Result<()> {
    trace!(
        "fd_seek(fd={:?}, offset={:?}, whence={}, newoffset={:#x?})",
        fd,
        offset,
        wasi::whence_to_str(whence),
        newoffset
    );

    let rights = if offset == 0 && whence == wasi::__WASI_WHENCE_CUR {
        wasi::__WASI_RIGHT_FD_TELL
    } else {
        wasi::__WASI_RIGHT_FD_SEEK | wasi::__WASI_RIGHT_FD_TELL
    };
    let fd = wasi_ctx
        .get_fd_entry_mut(fd)?
        .as_descriptor_mut(rights, 0)?
        .as_file_mut()?;

    let pos = match whence {
        wasi::__WASI_WHENCE_CUR => SeekFrom::Current(offset),
        wasi::__WASI_WHENCE_END => SeekFrom::End(offset),
        wasi::__WASI_WHENCE_SET => SeekFrom::Start(offset as u64),
        _ => return Err(Error::EINVAL),
    };
    let host_newoffset = fd.seek(pos)?;

    trace!("     | *newoffset={:?}", host_newoffset);

    enc_filesize_byref(memory, newoffset, host_newoffset)
}

pub(crate) unsafe fn fd_tell(
    wasi_ctx: &mut WasiCtx,
    memory: &mut [u8],
    fd: wasi::__wasi_fd_t,
    newoffset: wasi32::uintptr_t,
) -> Result<()> {
    trace!("fd_tell(fd={:?}, newoffset={:#x?})", fd, newoffset);

    let fd = wasi_ctx
        .get_fd_entry_mut(fd)?
        .as_descriptor_mut(wasi::__WASI_RIGHT_FD_TELL, 0)?
        .as_file_mut()?;

    let host_offset = fd.seek(SeekFrom::Current(0))?;

    trace!("     | *newoffset={:?}", host_offset);

    enc_filesize_byref(memory, newoffset, host_offset)
}

pub(crate) unsafe fn fd_fdstat_get(
    wasi_ctx: &WasiCtx,
    memory: &mut [u8],
    fd: wasi::__wasi_fd_t,
    fdstat_ptr: wasi32::uintptr_t, // *mut wasi::__wasi_fdstat_t
) -> Result<()> {
    trace!("fd_fdstat_get(fd={:?}, fdstat_ptr={:#x?})", fd, fdstat_ptr);

    let mut fdstat = dec_fdstat_byref(memory, fdstat_ptr)?;
    let wasi_fd = wasi_ctx.get_fd_entry(fd)?.as_descriptor(0, 0)?.as_file()?;

    let fs_flags = hostcalls_impl::fd_fdstat_get(wasi_fd)?;

    let fe = wasi_ctx.get_fd_entry(fd)?;
    fdstat.fs_filetype = fe.file_type;
    fdstat.fs_rights_base = fe.rights_base;
    fdstat.fs_rights_inheriting = fe.rights_inheriting;
    fdstat.fs_flags = fs_flags;

    trace!("     | *buf={:?}", fdstat);

    enc_fdstat_byref(memory, fdstat_ptr, fdstat)
}

pub(crate) unsafe fn fd_fdstat_set_flags(
    wasi_ctx: &WasiCtx,
    fd: wasi::__wasi_fd_t,
    fdflags: wasi::__wasi_fdflags_t,
) -> Result<()> {
    trace!("fd_fdstat_set_flags(fd={:?}, fdflags={:#x?})", fd, fdflags);

    let fd = wasi_ctx.get_fd_entry(fd)?.as_descriptor(0, 0)?.as_file()?;

    hostcalls_impl::fd_fdstat_set_flags(fd, fdflags)
}

pub(crate) unsafe fn fd_fdstat_set_rights(
    wasi_ctx: &mut WasiCtx,
    fd: wasi::__wasi_fd_t,
    fs_rights_base: wasi::__wasi_rights_t,
    fs_rights_inheriting: wasi::__wasi_rights_t,
) -> Result<()> {
    trace!(
        "fd_fdstat_set_rights(fd={:?}, fs_rights_base={:#x?}, fs_rights_inheriting={:#x?})",
        fd,
        fs_rights_base,
        fs_rights_inheriting
    );

    let fe = wasi_ctx.get_fd_entry_mut(fd)?;
    if fe.rights_base & fs_rights_base != fs_rights_base
        || fe.rights_inheriting & fs_rights_inheriting != fs_rights_inheriting
    {
        return Err(Error::ENOTCAPABLE);
    }
    fe.rights_base = fs_rights_base;
    fe.rights_inheriting = fs_rights_inheriting;

    Ok(())
}

pub(crate) unsafe fn fd_sync(wasi_ctx: &WasiCtx, fd: wasi::__wasi_fd_t) -> Result<()> {
    trace!("fd_sync(fd={:?})", fd);

    let fd = wasi_ctx
        .get_fd_entry(fd)?
        .as_descriptor(wasi::__WASI_RIGHT_FD_SYNC, 0)?
        .as_file()?;
    fd.sync_all().map_err(Into::into)
}

pub(crate) unsafe fn fd_write(
    wasi_ctx: &mut WasiCtx,
    memory: &mut [u8],
    fd: wasi::__wasi_fd_t,
    iovs_ptr: wasi32::uintptr_t,
    iovs_len: wasi32::size_t,
    nwritten: wasi32::uintptr_t,
) -> Result<()> {
    trace!(
        "fd_write(fd={:?}, iovs_ptr={:#x?}, iovs_len={:?}, nwritten={:#x?})",
        fd,
        iovs_ptr,
        iovs_len,
        nwritten
    );

    let iovs = dec_ciovec_slice(memory, iovs_ptr, iovs_len)?;
    let iovs: Vec<io::IoSlice> = iovs.iter().map(|vec| host::ciovec_to_host(vec)).collect();

    // perform unbuffered writes
    let host_nwritten = match wasi_ctx
        .get_fd_entry_mut(fd)?
        .as_descriptor_mut(wasi::__WASI_RIGHT_FD_WRITE, 0)?
    {
        Descriptor::OsFile(file) => file.write_vectored(&iovs)?,
        Descriptor::Socket(_) => return Err(Error::EBADF),
        Descriptor::SocketFd(_) => return Err(Error::EBADF),
        Descriptor::Stdin => return Err(Error::EBADF),
        Descriptor::Stdout => {
            // lock for the duration of the scope
            let stdout = io::stdout();
            let mut stdout = stdout.lock();
            let nwritten = stdout.write_vectored(&iovs)?;
            stdout.flush()?;
            nwritten
        }
        Descriptor::Stderr => io::stderr().lock().write_vectored(&iovs)?,
    };

    trace!("     | *nwritten={:?}", host_nwritten);

    enc_usize_byref(memory, nwritten, host_nwritten)
}

pub(crate) unsafe fn fd_advise(
    wasi_ctx: &WasiCtx,
    fd: wasi::__wasi_fd_t,
    offset: wasi::__wasi_filesize_t,
    len: wasi::__wasi_filesize_t,
    advice: wasi::__wasi_advice_t,
) -> Result<()> {
    trace!(
        "fd_advise(fd={:?}, offset={}, len={}, advice={:?})",
        fd,
        offset,
        len,
        advice
    );

    let fd = wasi_ctx
        .get_fd_entry(fd)?
        .as_descriptor(wasi::__WASI_RIGHT_FD_ADVISE, 0)?
        .as_file()?;

    hostcalls_impl::fd_advise(fd, advice, offset, len)
}

pub(crate) unsafe fn fd_allocate(
    wasi_ctx: &WasiCtx,
    fd: wasi::__wasi_fd_t,
    offset: wasi::__wasi_filesize_t,
    len: wasi::__wasi_filesize_t,
) -> Result<()> {
    trace!("fd_allocate(fd={:?}, offset={}, len={})", fd, offset, len);

    let fd = wasi_ctx
        .get_fd_entry(fd)?
        .as_descriptor(wasi::__WASI_RIGHT_FD_ALLOCATE, 0)?
        .as_file()?;

    let metadata = fd.metadata()?;

    let current_size = metadata.len();
    let wanted_size = offset.checked_add(len).ok_or(Error::E2BIG)?;
    // This check will be unnecessary when rust-lang/rust#63326 is fixed
    if wanted_size > i64::max_value() as u64 {
        return Err(Error::E2BIG);
    }

    if wanted_size > current_size {
        fd.set_len(wanted_size).map_err(Into::into)
    } else {
        Ok(())
    }
}

pub(crate) unsafe fn path_create_directory(
    wasi_ctx: &WasiCtx,
    memory: &mut [u8],
    dirfd: wasi::__wasi_fd_t,
    path_ptr: wasi32::uintptr_t,
    path_len: wasi32::size_t,
) -> Result<()> {
    trace!(
        "path_create_directory(dirfd={:?}, path_ptr={:#x?}, path_len={})",
        dirfd,
        path_ptr,
        path_len,
    );

    let path = dec_slice_of_u8(memory, path_ptr, path_len).and_then(helpers::path_from_slice)?;

    trace!("     | (path_ptr,path_len)='{}'", path);

    let rights = wasi::__WASI_RIGHT_PATH_OPEN | wasi::__WASI_RIGHT_PATH_CREATE_DIRECTORY;
    let fe = wasi_ctx.get_fd_entry(dirfd)?;
    let resolved = path_get(fe, rights, 0, 0, path, false)?;

    hostcalls_impl::path_create_directory(resolved)
}

pub(crate) unsafe fn path_link(
    wasi_ctx: &WasiCtx,
    memory: &mut [u8],
    old_dirfd: wasi::__wasi_fd_t,
    old_flags: wasi::__wasi_lookupflags_t,
    old_path_ptr: wasi32::uintptr_t,
    old_path_len: wasi32::size_t,
    new_dirfd: wasi::__wasi_fd_t,
    new_path_ptr: wasi32::uintptr_t,
    new_path_len: wasi32::size_t,
) -> Result<()> {
    trace!(
        "path_link(old_dirfd={:?}, old_flags={:?}, old_path_ptr={:#x?}, old_path_len={}, new_dirfd={:?}, new_path_ptr={:#x?}, new_path_len={})",
        old_dirfd,
        old_flags,
        old_path_ptr,
        old_path_len,
        new_dirfd,
        new_path_ptr,
        new_path_len,
    );

    let old_path = dec_slice_of_u8(memory, old_path_ptr, old_path_len).and_then(path_from_slice)?;
    let new_path = dec_slice_of_u8(memory, new_path_ptr, new_path_len).and_then(path_from_slice)?;

    trace!("     | (old_path_ptr,old_path_len)='{}'", old_path);
    trace!("     | (new_path_ptr,new_path_len)='{}'", new_path);

    let old_fe = wasi_ctx.get_fd_entry(old_dirfd)?;
    let new_fe = wasi_ctx.get_fd_entry(new_dirfd)?;
    let resolved_old = path_get(
        old_fe,
        wasi::__WASI_RIGHT_PATH_LINK_SOURCE,
        0,
        0,
        old_path,
        false,
    )?;
    let resolved_new = path_get(
        new_fe,
        wasi::__WASI_RIGHT_PATH_LINK_TARGET,
        0,
        0,
        new_path,
        false,
    )?;

    hostcalls_impl::path_link(resolved_old, resolved_new)
}

pub(crate) unsafe fn path_open(
    wasi_ctx: &mut WasiCtx,
    memory: &mut [u8],
    dirfd: wasi::__wasi_fd_t,
    dirflags: wasi::__wasi_lookupflags_t,
    path_ptr: wasi32::uintptr_t,
    path_len: wasi32::size_t,
    oflags: wasi::__wasi_oflags_t,
    fs_rights_base: wasi::__wasi_rights_t,
    fs_rights_inheriting: wasi::__wasi_rights_t,
    fs_flags: wasi::__wasi_fdflags_t,
    fd_out_ptr: wasi32::uintptr_t,
) -> Result<()> {
    trace!(
        "path_open(dirfd={:?}, dirflags={:?}, path_ptr={:#x?}, path_len={:?}, oflags={:#x?}, fs_rights_base={:#x?}, fs_rights_inheriting={:#x?}, fs_flags={:#x?}, fd_out_ptr={:#x?})",
        dirfd,
        dirflags,
        path_ptr,
        path_len,
        oflags,
        fs_rights_base,
        fs_rights_inheriting,
        fs_flags,
        fd_out_ptr
    );

    // pre-encode fd_out_ptr to -1 in case of error in opening a path
    enc_fd_byref(memory, fd_out_ptr, wasi::__wasi_fd_t::max_value())?;

    let path = dec_slice_of_u8(memory, path_ptr, path_len).and_then(path_from_slice)?;

    trace!("     | (path_ptr,path_len)='{}'", path);

    let (needed_base, needed_inheriting) =
        path_open_rights(fs_rights_base, fs_rights_inheriting, oflags, fs_flags);
    let fe = wasi_ctx.get_fd_entry(dirfd)?;
    let resolved = path_get(
        fe,
        needed_base,
        needed_inheriting,
        dirflags,
        path,
        oflags & wasi::__WASI_O_CREAT != 0,
    )?;

    // which open mode do we need?
    let read = fs_rights_base & (wasi::__WASI_RIGHT_FD_READ | wasi::__WASI_RIGHT_FD_READDIR) != 0;
    let write = fs_rights_base
        & (wasi::__WASI_RIGHT_FD_DATASYNC
            | wasi::__WASI_RIGHT_FD_WRITE
            | wasi::__WASI_RIGHT_FD_ALLOCATE
            | wasi::__WASI_RIGHT_FD_FILESTAT_SET_SIZE)
        != 0;

    let fd = hostcalls_impl::path_open(resolved, read, write, oflags, fs_flags)?;

    // Determine the type of the new file descriptor and which rights contradict with this type
    let (_ty, max_base, max_inheriting) = determine_type_rights(&fd)?;
    let mut fe = FdEntry::from(fd)?;
    fe.rights_base &= max_base;
    fe.rights_inheriting &= max_inheriting;
    let guest_fd = wasi_ctx.insert_fd_entry(fe)?;

    trace!("     | *fd={:?}", guest_fd);

    enc_fd_byref(memory, fd_out_ptr, guest_fd)
}

pub(crate) unsafe fn fd_readdir(
    wasi_ctx: &mut WasiCtx,
    memory: &mut [u8],
    fd: wasi::__wasi_fd_t,
    buf: wasi32::uintptr_t,
    buf_len: wasi32::size_t,
    cookie: wasi::__wasi_dircookie_t,
    buf_used: wasi32::uintptr_t,
) -> Result<()> {
    trace!(
        "fd_readdir(fd={:?}, buf={:#x?}, buf_len={}, cookie={:#x?}, buf_used={:#x?})",
        fd,
        buf,
        buf_len,
        cookie,
        buf_used,
    );

    enc_usize_byref(memory, buf_used, 0)?;

    let file = wasi_ctx
        .get_fd_entry_mut(fd)?
        .as_descriptor_mut(wasi::__WASI_RIGHT_FD_READDIR, 0)?
        .as_file_mut()?;
    let host_buf = dec_slice_of_mut_u8(memory, buf, buf_len)?;

    trace!("     | (buf,buf_len)={:?}", host_buf);

    let host_bufused = hostcalls_impl::fd_readdir(file, host_buf, cookie)?;

    trace!("     | *buf_used={:?}", host_bufused);

    enc_usize_byref(memory, buf_used, host_bufused)
}

pub(crate) unsafe fn path_readlink(
    wasi_ctx: &WasiCtx,
    memory: &mut [u8],
    dirfd: wasi::__wasi_fd_t,
    path_ptr: wasi32::uintptr_t,
    path_len: wasi32::size_t,
    buf_ptr: wasi32::uintptr_t,
    buf_len: wasi32::size_t,
    buf_used: wasi32::uintptr_t,
) -> Result<()> {
    trace!(
        "path_readlink(dirfd={:?}, path_ptr={:#x?}, path_len={:?}, buf_ptr={:#x?}, buf_len={}, buf_used={:#x?})",
        dirfd,
        path_ptr,
        path_len,
        buf_ptr,
        buf_len,
        buf_used,
    );

    enc_usize_byref(memory, buf_used, 0)?;

    let path = dec_slice_of_u8(memory, path_ptr, path_len).and_then(helpers::path_from_slice)?;

    trace!("     | (path_ptr,path_len)='{}'", &path);

    let fe = wasi_ctx.get_fd_entry(dirfd)?;
    let resolved = path_get(fe, wasi::__WASI_RIGHT_PATH_READLINK, 0, 0, &path, false)?;

    let mut buf = dec_slice_of_mut_u8(memory, buf_ptr, buf_len)?;

    let host_bufused = hostcalls_impl::path_readlink(resolved, &mut buf)?;

    trace!("     | (buf_ptr,*buf_used)={:?}", buf);
    trace!("     | *buf_used={:?}", host_bufused);

    enc_usize_byref(memory, buf_used, host_bufused)
}

pub(crate) unsafe fn path_rename(
    wasi_ctx: &WasiCtx,
    memory: &mut [u8],
    old_dirfd: wasi::__wasi_fd_t,
    old_path_ptr: wasi32::uintptr_t,
    old_path_len: wasi32::size_t,
    new_dirfd: wasi::__wasi_fd_t,
    new_path_ptr: wasi32::uintptr_t,
    new_path_len: wasi32::size_t,
) -> Result<()> {
    trace!(
        "path_rename(old_dirfd={:?}, old_path_ptr={:#x?}, old_path_len={:?}, new_dirfd={:?}, new_path_ptr={:#x?}, new_path_len={:?})",
        old_dirfd,
        old_path_ptr,
        old_path_len,
        new_dirfd,
        new_path_ptr,
        new_path_len,
    );

    let old_path = dec_slice_of_u8(memory, old_path_ptr, old_path_len).and_then(path_from_slice)?;
    let new_path = dec_slice_of_u8(memory, new_path_ptr, new_path_len).and_then(path_from_slice)?;

    trace!("     | (old_path_ptr,old_path_len)='{}'", old_path);
    trace!("     | (new_path_ptr,new_path_len)='{}'", new_path);

    let old_fe = wasi_ctx.get_fd_entry(old_dirfd)?;
    let new_fe = wasi_ctx.get_fd_entry(new_dirfd)?;
    let resolved_old = path_get(
        old_fe,
        wasi::__WASI_RIGHT_PATH_RENAME_SOURCE,
        0,
        0,
        old_path,
        true,
    )?;
    let resolved_new = path_get(
        new_fe,
        wasi::__WASI_RIGHT_PATH_RENAME_TARGET,
        0,
        0,
        new_path,
        true,
    )?;

    log::debug!("path_rename resolved_old={:?}", resolved_old);
    log::debug!("path_rename resolved_new={:?}", resolved_new);

    hostcalls_impl::path_rename(resolved_old, resolved_new)
}

pub(crate) unsafe fn fd_filestat_get(
    wasi_ctx: &WasiCtx,
    memory: &mut [u8],
    fd: wasi::__wasi_fd_t,
    filestat_ptr: wasi32::uintptr_t,
) -> Result<()> {
    trace!(
        "fd_filestat_get(fd={:?}, filestat_ptr={:#x?})",
        fd,
        filestat_ptr
    );

    let fd = wasi_ctx.get_fd_entry(fd)?.as_descriptor(0, 0)?.as_file()?;

    let host_filestat = hostcalls_impl::fd_filestat_get_impl(fd)?;

    trace!("     | *filestat_ptr={:?}", host_filestat);

    enc_filestat_byref(memory, filestat_ptr, host_filestat)
}

pub(crate) unsafe fn fd_filestat_set_times(
    wasi_ctx: &WasiCtx,
    fd: wasi::__wasi_fd_t,
    st_atim: wasi::__wasi_timestamp_t,
    st_mtim: wasi::__wasi_timestamp_t,
    fst_flags: wasi::__wasi_fstflags_t,
) -> Result<()> {
    trace!(
        "fd_filestat_set_times(fd={:?}, st_atim={}, st_mtim={}, fst_flags={:#x?})",
        fd,
        st_atim,
        st_mtim,
        fst_flags
    );

    let fd = wasi_ctx
        .get_fd_entry(fd)?
        .as_descriptor(wasi::__WASI_RIGHT_FD_FILESTAT_SET_TIMES, 0)?
        .as_file()?;

    fd_filestat_set_times_impl(fd, st_atim, st_mtim, fst_flags)
}

pub(crate) fn fd_filestat_set_times_impl(
    fd: &File,
    st_atim: wasi::__wasi_timestamp_t,
    st_mtim: wasi::__wasi_timestamp_t,
    fst_flags: wasi::__wasi_fstflags_t,
) -> Result<()> {
    let set_atim = fst_flags & wasi::__WASI_FILESTAT_SET_ATIM != 0;
    let set_atim_now = fst_flags & wasi::__WASI_FILESTAT_SET_ATIM_NOW != 0;
    let set_mtim = fst_flags & wasi::__WASI_FILESTAT_SET_MTIM != 0;
    let set_mtim_now = fst_flags & wasi::__WASI_FILESTAT_SET_MTIM_NOW != 0;

    if (set_atim && set_atim_now) || (set_mtim && set_mtim_now) {
        return Err(Error::EINVAL);
    }
    let atim = if set_atim {
        let time = UNIX_EPOCH + Duration::from_nanos(st_atim);
        Some(FileTime::from_system_time(time))
    } else if set_atim_now {
        let time = SystemTime::now();
        Some(FileTime::from_system_time(time))
    } else {
        None
    };

    let mtim = if set_mtim {
        let time = UNIX_EPOCH + Duration::from_nanos(st_mtim);
        Some(FileTime::from_system_time(time))
    } else if set_mtim_now {
        let time = SystemTime::now();
        Some(FileTime::from_system_time(time))
    } else {
        None
    };
    set_file_handle_times(fd, atim, mtim).map_err(Into::into)
}

pub(crate) unsafe fn fd_filestat_set_size(
    wasi_ctx: &WasiCtx,
    fd: wasi::__wasi_fd_t,
    st_size: wasi::__wasi_filesize_t,
) -> Result<()> {
    trace!("fd_filestat_set_size(fd={:?}, st_size={})", fd, st_size);

    let fd = wasi_ctx
        .get_fd_entry(fd)?
        .as_descriptor(wasi::__WASI_RIGHT_FD_FILESTAT_SET_SIZE, 0)?
        .as_file()?;

    // This check will be unnecessary when rust-lang/rust#63326 is fixed
    if st_size > i64::max_value() as u64 {
        return Err(Error::E2BIG);
    }
    fd.set_len(st_size).map_err(Into::into)
}

pub(crate) unsafe fn path_filestat_get(
    wasi_ctx: &WasiCtx,
    memory: &mut [u8],
    dirfd: wasi::__wasi_fd_t,
    dirflags: wasi::__wasi_lookupflags_t,
    path_ptr: wasi32::uintptr_t,
    path_len: wasi32::size_t,
    filestat_ptr: wasi32::uintptr_t,
) -> Result<()> {
    trace!(
        "path_filestat_get(dirfd={:?}, dirflags={:?}, path_ptr={:#x?}, path_len={}, filestat_ptr={:#x?})",
        dirfd,
        dirflags,
        path_ptr,
        path_len,
        filestat_ptr
    );

    let path = dec_slice_of_u8(memory, path_ptr, path_len).and_then(path_from_slice)?;

    trace!("     | (path_ptr,path_len)='{}'", path);

    let fe = wasi_ctx.get_fd_entry(dirfd)?;
    let resolved = path_get(
        fe,
        wasi::__WASI_RIGHT_PATH_FILESTAT_GET,
        0,
        dirflags,
        path,
        false,
    )?;
    let host_filestat = hostcalls_impl::path_filestat_get(resolved, dirflags)?;

    trace!("     | *filestat_ptr={:?}", host_filestat);

    enc_filestat_byref(memory, filestat_ptr, host_filestat)
}

pub(crate) unsafe fn path_filestat_set_times(
    wasi_ctx: &WasiCtx,
    memory: &mut [u8],
    dirfd: wasi::__wasi_fd_t,
    dirflags: wasi::__wasi_lookupflags_t,
    path_ptr: wasi32::uintptr_t,
    path_len: wasi32::size_t,
    st_atim: wasi::__wasi_timestamp_t,
    st_mtim: wasi::__wasi_timestamp_t,
    fst_flags: wasi::__wasi_fstflags_t,
) -> Result<()> {
    trace!(
        "path_filestat_set_times(dirfd={:?}, dirflags={:?}, path_ptr={:#x?}, path_len={}, st_atim={}, st_mtim={}, fst_flags={:#x?})",
        dirfd,
        dirflags,
        path_ptr,
        path_len,
        st_atim, st_mtim,
        fst_flags
    );

    let path = dec_slice_of_u8(memory, path_ptr, path_len).and_then(path_from_slice)?;

    trace!("     | (path_ptr,path_len)='{}'", path);

    let fe = wasi_ctx.get_fd_entry(dirfd)?;
    let resolved = path_get(
        fe,
        wasi::__WASI_RIGHT_PATH_FILESTAT_SET_TIMES,
        0,
        dirflags,
        path,
        false,
    )?;

    hostcalls_impl::path_filestat_set_times(resolved, dirflags, st_atim, st_mtim, fst_flags)
}

pub(crate) unsafe fn path_symlink(
    wasi_ctx: &WasiCtx,
    memory: &mut [u8],
    old_path_ptr: wasi32::uintptr_t,
    old_path_len: wasi32::size_t,
    dirfd: wasi::__wasi_fd_t,
    new_path_ptr: wasi32::uintptr_t,
    new_path_len: wasi32::size_t,
) -> Result<()> {
    trace!(
        "path_symlink(old_path_ptr={:#x?}, old_path_len={}, dirfd={:?}, new_path_ptr={:#x?}, new_path_len={})",
        old_path_ptr,
        old_path_len,
        dirfd,
        new_path_ptr,
        new_path_len
    );

    let old_path = dec_slice_of_u8(memory, old_path_ptr, old_path_len).and_then(path_from_slice)?;
    let new_path = dec_slice_of_u8(memory, new_path_ptr, new_path_len).and_then(path_from_slice)?;

    trace!("     | (old_path_ptr,old_path_len)='{}'", old_path);
    trace!("     | (new_path_ptr,new_path_len)='{}'", new_path);

    let fe = wasi_ctx.get_fd_entry(dirfd)?;
    let resolved_new = path_get(fe, wasi::__WASI_RIGHT_PATH_SYMLINK, 0, 0, new_path, true)?;

    hostcalls_impl::path_symlink(old_path, resolved_new)
}

pub(crate) unsafe fn path_unlink_file(
    wasi_ctx: &WasiCtx,
    memory: &mut [u8],
    dirfd: wasi::__wasi_fd_t,
    path_ptr: wasi32::uintptr_t,
    path_len: wasi32::size_t,
) -> Result<()> {
    trace!(
        "path_unlink_file(dirfd={:?}, path_ptr={:#x?}, path_len={})",
        dirfd,
        path_ptr,
        path_len
    );

    let path = dec_slice_of_u8(memory, path_ptr, path_len).and_then(path_from_slice)?;

    trace!("     | (path_ptr,path_len)='{}'", path);

    let fe = wasi_ctx.get_fd_entry(dirfd)?;
    let resolved = path_get(fe, wasi::__WASI_RIGHT_PATH_UNLINK_FILE, 0, 0, path, false)?;

    hostcalls_impl::path_unlink_file(resolved)
}

pub(crate) unsafe fn path_remove_directory(
    wasi_ctx: &WasiCtx,
    memory: &mut [u8],
    dirfd: wasi::__wasi_fd_t,
    path_ptr: wasi32::uintptr_t,
    path_len: wasi32::size_t,
) -> Result<()> {
    trace!(
        "path_remove_directory(dirfd={:?}, path_ptr={:#x?}, path_len={})",
        dirfd,
        path_ptr,
        path_len
    );

    let path = dec_slice_of_u8(memory, path_ptr, path_len).and_then(path_from_slice)?;

    trace!("     | (path_ptr,path_len)='{}'", path);

    let fe = wasi_ctx.get_fd_entry(dirfd)?;
    let resolved = path_get(
        fe,
        wasi::__WASI_RIGHT_PATH_REMOVE_DIRECTORY,
        0,
        0,
        path,
        true,
    )?;

    log::debug!("path_remove_directory resolved={:?}", resolved);

    hostcalls_impl::path_remove_directory(resolved)
}

pub(crate) unsafe fn fd_prestat_get(
    wasi_ctx: &WasiCtx,
    memory: &mut [u8],
    fd: wasi::__wasi_fd_t,
    prestat_ptr: wasi32::uintptr_t,
) -> Result<()> {
    trace!(
        "fd_prestat_get(fd={:?}, prestat_ptr={:#x?})",
        fd,
        prestat_ptr
    );

    // TODO: should we validate any rights here?
    let fe = wasi_ctx.get_fd_entry(fd)?;
    let po_path = fe.preopen_path.as_ref().ok_or(Error::ENOTSUP)?;
    if fe.file_type != wasi::__WASI_FILETYPE_DIRECTORY {
        return Err(Error::ENOTDIR);
    }

    let path = host_impl::path_from_host(po_path.as_os_str())?;

    enc_prestat_byref(
        memory,
        prestat_ptr,
        host::__wasi_prestat_t {
            pr_type: wasi::__WASI_PREOPENTYPE_DIR,
            u: host::__wasi_prestat_u {
                dir: host::__wasi_prestat_dir {
                    pr_name_len: path.len(),
                },
            },
        },
    )
}

pub(crate) unsafe fn fd_prestat_dir_name(
    wasi_ctx: &WasiCtx,
    memory: &mut [u8],
    fd: wasi::__wasi_fd_t,
    path_ptr: wasi32::uintptr_t,
    path_len: wasi32::size_t,
) -> Result<()> {
    trace!(
        "fd_prestat_dir_name(fd={:?}, path_ptr={:#x?}, path_len={})",
        fd,
        path_ptr,
        path_len
    );

    // TODO: should we validate any rights here?
    let fe = wasi_ctx.get_fd_entry(fd)?;
    let po_path = fe.preopen_path.as_ref().ok_or(Error::ENOTSUP)?;
    if fe.file_type != wasi::__WASI_FILETYPE_DIRECTORY {
        return Err(Error::ENOTDIR);
    }

    let path = host_impl::path_from_host(po_path.as_os_str())?;

    if path.len() > dec_usize(path_len) {
        return Err(Error::ENAMETOOLONG);
    }

    trace!("     | (path_ptr,path_len)='{}'", path);

    enc_slice_of_u8(memory, path.as_bytes(), path_ptr)
}

#[allow(dead_code)] // trouble with sockets
#[derive(Clone, Copy, Debug)]
#[repr(u8)]
pub(crate) enum FileType {
    Unknown = wasi::__WASI_FILETYPE_UNKNOWN,
    BlockDevice = wasi::__WASI_FILETYPE_BLOCK_DEVICE,
    CharacterDevice = wasi::__WASI_FILETYPE_CHARACTER_DEVICE,
    Directory = wasi::__WASI_FILETYPE_DIRECTORY,
    RegularFile = wasi::__WASI_FILETYPE_REGULAR_FILE,
    SocketDgram = wasi::__WASI_FILETYPE_SOCKET_DGRAM,
    SocketStream = wasi::__WASI_FILETYPE_SOCKET_STREAM,
    Symlink = wasi::__WASI_FILETYPE_SYMBOLIC_LINK,
}

impl FileType {
    pub(crate) fn to_wasi(&self) -> wasi::__wasi_filetype_t {
        *self as wasi::__wasi_filetype_t
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Dirent {
    pub name: String,
    pub ftype: FileType,
    pub ino: u64,
    pub cookie: wasi::__wasi_dircookie_t,
}

impl Dirent {
    #![allow(unused)] // temporarily, until BSD catches up with this change
    /// Serialize the directory entry to the format define by `__wasi_fd_readdir`,
    /// so that the serialized entries can be concatenated by the implementation.
    pub fn to_wasi_raw(&self) -> Result<Vec<u8>> {
        use std::slice;

        let name = self.name.as_bytes();
        let namlen = name.len();
        let dirent_size = mem::size_of::<wasi::__wasi_dirent_t>();
        let offset = dirent_size.checked_add(namlen).ok_or(Error::EOVERFLOW)?;

        let mut raw = Vec::<u8>::with_capacity(offset);
        raw.resize(offset, 0);

        let sys_dirent = raw.as_mut_ptr() as *mut wasi::__wasi_dirent_t;
        unsafe {
            *sys_dirent = wasi::__wasi_dirent_t {
                d_namlen: namlen.try_into()?,
                d_ino: self.ino,
                d_next: self.cookie,
                d_type: self.ftype.to_wasi(),
            };
        }

        let sys_name = unsafe { sys_dirent.offset(1) as *mut u8 };
        let sys_name = unsafe { slice::from_raw_parts_mut(sys_name, namlen) };
        sys_name.copy_from_slice(&name);

        Ok(raw)
    }
}
