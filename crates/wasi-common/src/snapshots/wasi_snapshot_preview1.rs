use crate::entry::{Entry, EntryHandle};
use crate::handle::{AsBytes, HandleRights};
use crate::sys::{clock, poll};
use crate::wasi::types;
use crate::wasi::wasi_snapshot_preview1::WasiSnapshotPreview1;
use crate::{path, sched, Error, Result, WasiCtx};
use std::cmp::min;
use std::convert::TryInto;
use std::io::{self, SeekFrom};
use std::ops::Deref;
use tracing::{debug, trace};
use wiggle::{GuestPtr, GuestSliceMut};

impl<'a> WasiSnapshotPreview1 for WasiCtx {
    fn args_get<'b>(
        &self,
        argv: &GuestPtr<'b, GuestPtr<'b, u8>>,
        argv_buf: &GuestPtr<'b, u8>,
    ) -> Result<()> {
        self.args().write_to_guest(argv_buf, argv)
    }

    fn args_sizes_get(&self) -> Result<(types::Size, types::Size)> {
        Ok((self.args().number_elements, self.args().cumulative_size))
    }

    fn environ_get<'b>(
        &self,
        environ: &GuestPtr<'b, GuestPtr<'b, u8>>,
        environ_buf: &GuestPtr<'b, u8>,
    ) -> Result<()> {
        self.env().write_to_guest(environ_buf, environ)
    }

    fn environ_sizes_get(&self) -> Result<(types::Size, types::Size)> {
        Ok((self.env().number_elements, self.env().cumulative_size))
    }

    fn clock_res_get(&self, id: types::Clockid) -> Result<types::Timestamp> {
        let resolution = clock::res_get(id)?;
        Ok(resolution)
    }

    fn clock_time_get(
        &self,
        id: types::Clockid,
        _precision: types::Timestamp,
    ) -> Result<types::Timestamp> {
        let time = clock::time_get(id)?;
        Ok(time)
    }

    fn fd_advise(
        &self,
        fd: types::Fd,
        offset: types::Filesize,
        len: types::Filesize,
        advice: types::Advice,
    ) -> Result<()> {
        let required_rights = HandleRights::from_base(types::Rights::FD_ADVISE);
        let entry = self.get_entry(fd)?;
        entry
            .as_handle(required_rights)?
            .advise(advice, offset, len)
    }

    fn fd_allocate(
        &self,
        fd: types::Fd,
        offset: types::Filesize,
        len: types::Filesize,
    ) -> Result<()> {
        let required_rights = HandleRights::from_base(types::Rights::FD_ALLOCATE);
        let entry = self.get_entry(fd)?;
        entry.as_handle(required_rights)?.allocate(offset, len)
    }

    fn fd_close(&self, fd: types::Fd) -> Result<()> {
        if let Ok(fe) = self.get_entry(fd) {
            // can't close preopened files
            if fe.preopen_path.is_some() {
                return Err(Error::Notsup);
            }
        }
        self.remove_entry(fd)?;
        Ok(())
    }

    fn fd_datasync(&self, fd: types::Fd) -> Result<()> {
        let required_rights = HandleRights::from_base(types::Rights::FD_DATASYNC);
        let entry = self.get_entry(fd)?;
        entry.as_handle(required_rights)?.datasync()
    }

    fn fd_fdstat_get(&self, fd: types::Fd) -> Result<types::Fdstat> {
        let entry = self.get_entry(fd)?;
        let file = entry.as_handle(HandleRights::empty())?;
        let fs_flags = file.fdstat_get()?;
        let rights = entry.get_rights();
        let fdstat = types::Fdstat {
            fs_filetype: entry.get_file_type(),
            fs_rights_base: rights.base,
            fs_rights_inheriting: rights.inheriting,
            fs_flags,
        };
        Ok(fdstat)
    }

    fn fd_fdstat_set_flags(&self, fd: types::Fd, flags: types::Fdflags) -> Result<()> {
        let required_rights = HandleRights::from_base(types::Rights::FD_FDSTAT_SET_FLAGS);
        let entry = self.get_entry(fd)?;
        entry.as_handle(required_rights)?.fdstat_set_flags(flags)
    }

    fn fd_fdstat_set_rights(
        &self,
        fd: types::Fd,
        fs_rights_base: types::Rights,
        fs_rights_inheriting: types::Rights,
    ) -> Result<()> {
        let rights = HandleRights::new(fs_rights_base, fs_rights_inheriting);
        let entry = self.get_entry(fd)?;
        if !entry.get_rights().contains(rights) {
            return Err(Error::Notcapable);
        }
        entry.set_rights(rights);
        Ok(())
    }

    fn fd_filestat_get(&self, fd: types::Fd) -> Result<types::Filestat> {
        let required_rights = HandleRights::from_base(types::Rights::FD_FILESTAT_GET);
        let entry = self.get_entry(fd)?;
        let host_filestat = entry.as_handle(required_rights)?.filestat_get()?;
        Ok(host_filestat)
    }

    fn fd_filestat_set_size(&self, fd: types::Fd, size: types::Filesize) -> Result<()> {
        let required_rights = HandleRights::from_base(types::Rights::FD_FILESTAT_SET_SIZE);
        let entry = self.get_entry(fd)?;
        entry.as_handle(required_rights)?.filestat_set_size(size)
    }

    fn fd_filestat_set_times(
        &self,
        fd: types::Fd,
        atim: types::Timestamp,
        mtim: types::Timestamp,
        fst_flags: types::Fstflags,
    ) -> Result<()> {
        let required_rights = HandleRights::from_base(types::Rights::FD_FILESTAT_SET_TIMES);
        let entry = self.get_entry(fd)?;
        entry
            .as_handle(required_rights)?
            .filestat_set_times(atim, mtim, fst_flags)
    }

    fn fd_pread(
        &self,
        fd: types::Fd,
        iovs: &types::IovecArray<'_>,
        offset: types::Filesize,
    ) -> Result<types::Size> {
        let mut guest_slices: Vec<GuestSliceMut<'_, u8>> = Vec::new();
        for iov_ptr in iovs.iter() {
            let iov_ptr = iov_ptr?;
            let iov: types::Iovec = iov_ptr.read()?;
            guest_slices.push(iov.buf.as_array(iov.buf_len).as_slice_mut()?);
        }

        let required_rights =
            HandleRights::from_base(types::Rights::FD_READ | types::Rights::FD_SEEK);
        let entry = self.get_entry(fd)?;
        if offset > i64::max_value() as u64 {
            return Err(Error::Io);
        }

        let host_nread = {
            let mut buf = guest_slices
                .iter_mut()
                .map(|s| io::IoSliceMut::new(&mut *s))
                .collect::<Vec<io::IoSliceMut<'_>>>();
            entry
                .as_handle(required_rights)?
                .preadv(&mut buf, offset)?
                .try_into()?
        };
        Ok(host_nread)
    }

    fn fd_prestat_get(&self, fd: types::Fd) -> Result<types::Prestat> {
        // TODO: should we validate any rights here?
        let entry = self.get_entry(fd)?;
        let po_path = entry.preopen_path.as_ref().ok_or(Error::Notsup)?;
        if entry.get_file_type() != types::Filetype::Directory {
            return Err(Error::Notdir);
        }

        let path = path::from_host(po_path.as_os_str())?;
        let prestat = types::PrestatDir {
            pr_name_len: path.len().try_into()?,
        };
        Ok(types::Prestat::Dir(prestat))
    }

    fn fd_prestat_dir_name(
        &self,
        fd: types::Fd,
        path: &GuestPtr<u8>,
        path_len: types::Size,
    ) -> Result<()> {
        // TODO: should we validate any rights here?
        let entry = self.get_entry(fd)?;
        let po_path = entry.preopen_path.as_ref().ok_or(Error::Notsup)?;
        if entry.get_file_type() != types::Filetype::Directory {
            return Err(Error::Notdir);
        }

        let host_path = path::from_host(po_path.as_os_str())?;
        let host_path_len = host_path.len().try_into()?;

        if host_path_len > path_len {
            return Err(Error::Nametoolong);
        }

        trace!("     | path='{}'", host_path);

        path.as_array(host_path_len)
            .copy_from_slice(host_path.as_bytes())?;

        Ok(())
    }

    fn fd_pwrite(
        &self,
        fd: types::Fd,
        ciovs: &types::CiovecArray<'_>,
        offset: types::Filesize,
    ) -> Result<types::Size> {
        let mut guest_slices = Vec::new();
        for ciov_ptr in ciovs.iter() {
            let ciov_ptr = ciov_ptr?;
            let ciov: types::Ciovec = ciov_ptr.read()?;
            guest_slices.push(ciov.buf.as_array(ciov.buf_len).as_slice()?);
        }

        let required_rights =
            HandleRights::from_base(types::Rights::FD_WRITE | types::Rights::FD_SEEK);
        let entry = self.get_entry(fd)?;

        if offset > i64::max_value() as u64 {
            return Err(Error::Io);
        }

        let host_nwritten = {
            let buf: Vec<io::IoSlice> =
                guest_slices.iter().map(|s| io::IoSlice::new(&*s)).collect();
            entry
                .as_handle(required_rights)?
                .pwritev(&buf, offset)?
                .try_into()?
        };
        Ok(host_nwritten)
    }

    fn fd_read(&self, fd: types::Fd, iovs: &types::IovecArray<'_>) -> Result<types::Size> {
        let mut guest_slices = Vec::new();
        for iov_ptr in iovs.iter() {
            let iov_ptr = iov_ptr?;
            let iov: types::Iovec = iov_ptr.read()?;
            guest_slices.push(iov.buf.as_array(iov.buf_len).as_slice_mut()?);
        }

        let required_rights = HandleRights::from_base(types::Rights::FD_READ);
        let entry = self.get_entry(fd)?;
        let host_nread = {
            let mut slices: Vec<io::IoSliceMut> = guest_slices
                .iter_mut()
                .map(|s| io::IoSliceMut::new(&mut *s))
                .collect();
            entry
                .as_handle(required_rights)?
                .read_vectored(&mut slices)?
                .try_into()?
        };

        Ok(host_nread)
    }

    fn fd_readdir(
        &self,
        fd: types::Fd,
        buf: &GuestPtr<u8>,
        buf_len: types::Size,
        cookie: types::Dircookie,
    ) -> Result<types::Size> {
        let required_rights = HandleRights::from_base(types::Rights::FD_READDIR);
        let entry = self.get_entry(fd)?;

        let mut bufused = 0;
        let mut buf = buf.clone();
        for pair in entry.as_handle(required_rights)?.readdir(cookie)? {
            let (dirent, name) = pair?;
            let dirent_raw = dirent.as_bytes()?;
            let dirent_len: types::Size = dirent_raw.len().try_into()?;
            let name_raw = name.as_bytes();
            let name_len = name_raw.len().try_into()?;

            // Copy as many bytes of the dirent as we can, up to the end of the buffer.
            let dirent_copy_len = min(dirent_len, buf_len - bufused);
            buf.as_array(dirent_copy_len)
                .copy_from_slice(&dirent_raw[..dirent_copy_len as usize])?;

            // If the dirent struct wasn't copied entirely, return that we
            // filled the buffer, which tells libc that we're not at EOF.
            if dirent_copy_len < dirent_len {
                return Ok(buf_len);
            }

            buf = buf.add(dirent_copy_len)?;
            bufused += dirent_copy_len;

            // Copy as many bytes of the name as we can, up to the end of the buffer.
            let name_copy_len = min(name_len, buf_len - bufused);
            buf.as_array(name_copy_len)
                .copy_from_slice(&name_raw[..name_copy_len as usize])?;

            // If the dirent struct wasn't copied entirely, return that we
            // filled the buffer, which tells libc that we're not at EOF.
            if name_copy_len < name_len {
                return Ok(buf_len);
            }

            buf = buf.add(name_copy_len)?;
            bufused += name_copy_len;
        }

        Ok(bufused)
    }

    fn fd_renumber(&self, from: types::Fd, to: types::Fd) -> Result<()> {
        if !self.contains_entry(from) {
            return Err(Error::Badf);
        }

        // Don't allow renumbering over a pre-opened resource.
        // TODO: Eventually, we do want to permit this, once libpreopen in
        // userspace is capable of removing entries from its tables as well.
        if let Ok(from_fe) = self.get_entry(from) {
            if from_fe.preopen_path.is_some() {
                return Err(Error::Notsup);
            }
        }
        if let Ok(to_fe) = self.get_entry(to) {
            if to_fe.preopen_path.is_some() {
                return Err(Error::Notsup);
            }
        }
        let fe = self.remove_entry(from)?;
        self.insert_entry_at(to, fe);
        Ok(())
    }

    fn fd_seek(
        &self,
        fd: types::Fd,
        offset: types::Filedelta,
        whence: types::Whence,
    ) -> Result<types::Filesize> {
        let base = if offset == 0 && whence == types::Whence::Cur {
            types::Rights::FD_TELL
        } else {
            types::Rights::FD_SEEK | types::Rights::FD_TELL
        };
        let required_rights = HandleRights::from_base(base);
        let entry = self.get_entry(fd)?;
        let pos = match whence {
            types::Whence::Cur => SeekFrom::Current(offset),
            types::Whence::End => SeekFrom::End(offset),
            types::Whence::Set => SeekFrom::Start(offset as u64),
        };
        let host_newoffset = entry.as_handle(required_rights)?.seek(pos)?;
        Ok(host_newoffset)
    }

    fn fd_sync(&self, fd: types::Fd) -> Result<()> {
        let required_rights = HandleRights::from_base(types::Rights::FD_SYNC);
        let entry = self.get_entry(fd)?;
        entry.as_handle(required_rights)?.sync()
    }

    fn fd_tell(&self, fd: types::Fd) -> Result<types::Filesize> {
        let required_rights = HandleRights::from_base(types::Rights::FD_TELL);
        let entry = self.get_entry(fd)?;
        let host_offset = entry
            .as_handle(required_rights)?
            .seek(SeekFrom::Current(0))?;
        Ok(host_offset)
    }

    fn fd_write(&self, fd: types::Fd, ciovs: &types::CiovecArray<'_>) -> Result<types::Size> {
        let mut guest_slices = Vec::new();
        for ciov_ptr in ciovs.iter() {
            let ciov_ptr = ciov_ptr?;
            let ciov: types::Ciovec = ciov_ptr.read()?;
            guest_slices.push(ciov.buf.as_array(ciov.buf_len).as_slice()?);
        }
        let required_rights = HandleRights::from_base(types::Rights::FD_WRITE);
        let entry = self.get_entry(fd)?;
        let host_nwritten = {
            let slices: Vec<io::IoSlice> =
                guest_slices.iter().map(|s| io::IoSlice::new(&*s)).collect();
            entry
                .as_handle(required_rights)?
                .write_vectored(&slices)?
                .try_into()?
        };
        Ok(host_nwritten)
    }

    fn path_create_directory(&self, dirfd: types::Fd, path: &GuestPtr<'_, str>) -> Result<()> {
        let required_rights = HandleRights::from_base(
            types::Rights::PATH_OPEN | types::Rights::PATH_CREATE_DIRECTORY,
        );
        let entry = self.get_entry(dirfd)?;
        let path = path.as_str()?;
        let (dirfd, path) = path::get(
            &entry,
            required_rights,
            types::Lookupflags::empty(),
            path.deref(),
            false,
        )?;
        dirfd.create_directory(&path)
    }

    fn path_filestat_get(
        &self,
        dirfd: types::Fd,
        flags: types::Lookupflags,
        path: &GuestPtr<'_, str>,
    ) -> Result<types::Filestat> {
        let required_rights = HandleRights::from_base(types::Rights::PATH_FILESTAT_GET);
        let entry = self.get_entry(dirfd)?;
        let path = path.as_str()?;
        let (dirfd, path) = path::get(&entry, required_rights, flags, path.deref(), false)?;
        let host_filestat =
            dirfd.filestat_get_at(&path, flags.contains(types::Lookupflags::SYMLINK_FOLLOW))?;
        Ok(host_filestat)
    }

    fn path_filestat_set_times(
        &self,
        dirfd: types::Fd,
        flags: types::Lookupflags,
        path: &GuestPtr<'_, str>,
        atim: types::Timestamp,
        mtim: types::Timestamp,
        fst_flags: types::Fstflags,
    ) -> Result<()> {
        let required_rights = HandleRights::from_base(types::Rights::PATH_FILESTAT_SET_TIMES);
        let entry = self.get_entry(dirfd)?;
        let path = path.as_str()?;
        let (dirfd, path) = path::get(&entry, required_rights, flags, path.deref(), false)?;
        dirfd.filestat_set_times_at(
            &path,
            atim,
            mtim,
            fst_flags,
            flags.contains(types::Lookupflags::SYMLINK_FOLLOW),
        )?;
        Ok(())
    }

    fn path_link(
        &self,
        old_fd: types::Fd,
        old_flags: types::Lookupflags,
        old_path: &GuestPtr<'_, str>,
        new_fd: types::Fd,
        new_path: &GuestPtr<'_, str>,
    ) -> Result<()> {
        let required_rights = HandleRights::from_base(types::Rights::PATH_LINK_SOURCE);
        let old_entry = self.get_entry(old_fd)?;
        let (old_dirfd, old_path) = {
            // Borrow old_path for just this scope
            let old_path = old_path.as_str()?;
            path::get(
                &old_entry,
                required_rights,
                types::Lookupflags::empty(),
                old_path.deref(),
                false,
            )?
        };
        let required_rights = HandleRights::from_base(types::Rights::PATH_LINK_TARGET);
        let new_entry = self.get_entry(new_fd)?;
        let (new_dirfd, new_path) = {
            // Borrow new_path for just this scope
            let new_path = new_path.as_str()?;
            path::get(
                &new_entry,
                required_rights,
                types::Lookupflags::empty(),
                new_path.deref(),
                false,
            )?
        };
        old_dirfd.link(
            &old_path,
            new_dirfd,
            &new_path,
            old_flags.contains(types::Lookupflags::SYMLINK_FOLLOW),
        )
    }

    fn path_open(
        &self,
        dirfd: types::Fd,
        dirflags: types::Lookupflags,
        path: &GuestPtr<'_, str>,
        oflags: types::Oflags,
        fs_rights_base: types::Rights,
        fs_rights_inheriting: types::Rights,
        fdflags: types::Fdflags,
    ) -> Result<types::Fd> {
        let needed_rights = path::open_rights(
            &HandleRights::new(fs_rights_base, fs_rights_inheriting),
            oflags,
            fdflags,
        );
        trace!("     | needed_rights={}", needed_rights);
        let entry = self.get_entry(dirfd)?;
        let (dirfd, path) = {
            let path = path.as_str()?;
            path::get(
                &entry,
                needed_rights,
                dirflags,
                path.deref(),
                oflags & types::Oflags::CREAT != types::Oflags::empty(),
            )?
        };
        // which open mode do we need?
        let read = fs_rights_base & (types::Rights::FD_READ | types::Rights::FD_READDIR)
            != types::Rights::empty();
        let write = fs_rights_base
            & (types::Rights::FD_DATASYNC
                | types::Rights::FD_WRITE
                | types::Rights::FD_ALLOCATE
                | types::Rights::FD_FILESTAT_SET_SIZE)
            != types::Rights::empty();
        trace!(
            "     | calling path_open impl: read={}, write={}",
            read,
            write
        );
        let fd = dirfd.openat(&path, read, write, oflags, fdflags)?;
        let entry = Entry::new(EntryHandle::from(fd));
        // We need to manually deny the rights which are not explicitly requested
        // because Entry::from will assign maximal consistent rights.
        let mut rights = entry.get_rights();
        rights.base &= fs_rights_base;
        rights.inheriting &= fs_rights_inheriting;
        entry.set_rights(rights);
        let guest_fd = self.insert_entry(entry)?;
        Ok(guest_fd)
    }

    fn path_readlink(
        &self,
        dirfd: types::Fd,
        path: &GuestPtr<'_, str>,
        buf: &GuestPtr<u8>,
        buf_len: types::Size,
    ) -> Result<types::Size> {
        let required_rights = HandleRights::from_base(types::Rights::PATH_READLINK);
        let entry = self.get_entry(dirfd)?;
        let (dirfd, path) = {
            // borrow path for just this scope
            let path = path.as_str()?;
            path::get(
                &entry,
                required_rights,
                types::Lookupflags::empty(),
                path.deref(),
                false,
            )?
        };
        let mut slice = buf.as_array(buf_len).as_slice_mut()?;
        let host_bufused = dirfd.readlink(&path, &mut *slice)?.try_into()?;
        Ok(host_bufused)
    }

    fn path_remove_directory(&self, dirfd: types::Fd, path: &GuestPtr<'_, str>) -> Result<()> {
        let required_rights = HandleRights::from_base(types::Rights::PATH_REMOVE_DIRECTORY);
        let entry = self.get_entry(dirfd)?;
        let (dirfd, path) = {
            let path = path.as_str()?;
            path::get(
                &entry,
                required_rights,
                types::Lookupflags::empty(),
                path.deref(),
                true,
            )?
        };
        dirfd.remove_directory(&path)
    }

    fn path_rename(
        &self,
        old_fd: types::Fd,
        old_path: &GuestPtr<'_, str>,
        new_fd: types::Fd,
        new_path: &GuestPtr<'_, str>,
    ) -> Result<()> {
        let required_rights = HandleRights::from_base(types::Rights::PATH_RENAME_SOURCE);
        let entry = self.get_entry(old_fd)?;
        let (old_dirfd, old_path) = {
            let old_path = old_path.as_str()?;
            path::get(
                &entry,
                required_rights,
                types::Lookupflags::empty(),
                old_path.deref(),
                true,
            )?
        };
        let required_rights = HandleRights::from_base(types::Rights::PATH_RENAME_TARGET);
        let entry = self.get_entry(new_fd)?;
        let (new_dirfd, new_path) = {
            let new_path = new_path.as_str()?;
            path::get(
                &entry,
                required_rights,
                types::Lookupflags::empty(),
                new_path.deref(),
                true,
            )?
        };
        old_dirfd.rename(&old_path, new_dirfd, &new_path)
    }

    fn path_symlink(
        &self,
        old_path: &GuestPtr<'_, str>,
        dirfd: types::Fd,
        new_path: &GuestPtr<'_, str>,
    ) -> Result<()> {
        let required_rights = HandleRights::from_base(types::Rights::PATH_SYMLINK);
        let entry = self.get_entry(dirfd)?;
        let (new_fd, new_path) = {
            let new_path = new_path.as_str()?;
            path::get(
                &entry,
                required_rights,
                types::Lookupflags::empty(),
                new_path.deref(),
                true,
            )?
        };
        let old_path = old_path.as_str()?;
        trace!(old_path = old_path.deref());
        new_fd.symlink(&old_path, &new_path)
    }

    fn path_unlink_file(&self, dirfd: types::Fd, path: &GuestPtr<'_, str>) -> Result<()> {
        let required_rights = HandleRights::from_base(types::Rights::PATH_UNLINK_FILE);
        let entry = self.get_entry(dirfd)?;
        let (dirfd, path) = {
            let path = path.as_str()?;
            path::get(
                &entry,
                required_rights,
                types::Lookupflags::empty(),
                path.deref(),
                false,
            )?
        };
        dirfd.unlink_file(&path)?;
        Ok(())
    }

    fn poll_oneoff(
        &self,
        in_: &GuestPtr<types::Subscription>,
        out: &GuestPtr<types::Event>,
        nsubscriptions: types::Size,
    ) -> Result<types::Size> {
        if u64::from(nsubscriptions) > types::Filesize::max_value() {
            return Err(Error::Inval);
        }

        let mut subscriptions = Vec::new();
        let subs = in_.as_array(nsubscriptions);
        for sub_ptr in subs.iter() {
            let sub_ptr = sub_ptr?;
            let sub: types::Subscription = sub_ptr.read()?;
            subscriptions.push(sub);
        }

        let events = self.poll_oneoff_impl(&subscriptions)?;
        let nevents = events.len().try_into()?;

        let out_events = out.as_array(nevents);
        for (event, event_ptr) in events.into_iter().zip(out_events.iter()) {
            let event_ptr = event_ptr?;
            event_ptr.write(event)?;
        }

        trace!(nevents = nevents);

        Ok(nevents)
    }

    fn proc_exit(&self, status: types::Exitcode) -> wiggle::Trap {
        // Check that the status is within WASI's range.
        if status < 126 {
            wiggle::Trap::I32Exit(status as i32)
        } else {
            wiggle::Trap::String("exit with invalid exit status outside of [0..126)".to_owned())
        }
    }

    fn proc_raise(&self, _sig: types::Signal) -> Result<()> {
        Err(Error::Unsupported("proc_raise"))
    }

    fn sched_yield(&self) -> Result<()> {
        std::thread::yield_now();
        Ok(())
    }

    fn random_get(&self, buf: &GuestPtr<u8>, buf_len: types::Size) -> Result<()> {
        let mut slice = buf.as_array(buf_len).as_slice_mut()?;
        getrandom::getrandom(&mut *slice)?;
        Ok(())
    }

    fn sock_recv(
        &self,
        _fd: types::Fd,
        _ri_data: &types::IovecArray<'_>,
        _ri_flags: types::Riflags,
    ) -> Result<(types::Size, types::Roflags)> {
        Err(Error::Unsupported("sock_recv"))
    }

    fn sock_send(
        &self,
        _fd: types::Fd,
        _si_data: &types::CiovecArray<'_>,
        _si_flags: types::Siflags,
    ) -> Result<types::Size> {
        Err(Error::Unsupported("sock_send"))
    }

    fn sock_shutdown(&self, _fd: types::Fd, _how: types::Sdflags) -> Result<()> {
        Err(Error::Unsupported("sock_shutdown"))
    }
}

impl WasiCtx {
    pub(crate) fn poll_oneoff_impl(
        &self,
        subscriptions: &[types::Subscription],
    ) -> Result<Vec<types::Event>> {
        let mut events = Vec::new();
        let mut timeout: Option<sched::ClockEventData> = None;
        let mut fd_events = Vec::new();

        // As mandated by the WASI spec:
        // > If `nsubscriptions` is 0, returns `errno::inval`.
        if subscriptions.is_empty() {
            return Err(Error::Inval);
        }

        for subscription in subscriptions {
            match &subscription.u {
                types::SubscriptionU::Clock(clock) => {
                    let delay = clock::to_relative_ns_delay(&clock)?;
                    debug!(
                        clock = tracing::field::debug(&clock),
                        delay_ns = tracing::field::debug(delay),
                        "poll_oneoff"
                    );
                    let current = sched::ClockEventData {
                        delay,
                        userdata: subscription.userdata,
                    };
                    let timeout = timeout.get_or_insert(current);
                    if current.delay < timeout.delay {
                        *timeout = current;
                    }
                }
                types::SubscriptionU::FdRead(fd_read) => {
                    let fd = fd_read.file_descriptor;
                    let required_rights = HandleRights::from_base(
                        types::Rights::FD_READ | types::Rights::POLL_FD_READWRITE,
                    );
                    let entry = match self.get_entry(fd) {
                        Ok(entry) => entry,
                        Err(error) => {
                            events.push(types::Event {
                                userdata: subscription.userdata,
                                error: error.try_into().expect("non-trapping error"),
                                type_: types::Eventtype::FdRead,
                                fd_readwrite: types::EventFdReadwrite {
                                    nbytes: 0,
                                    flags: types::Eventrwflags::empty(),
                                },
                            });
                            continue;
                        }
                    };
                    fd_events.push(sched::FdEventData {
                        handle: entry.as_handle(required_rights)?,
                        r#type: types::Eventtype::FdRead,
                        userdata: subscription.userdata,
                    });
                }
                types::SubscriptionU::FdWrite(fd_write) => {
                    let fd = fd_write.file_descriptor;
                    let required_rights = HandleRights::from_base(
                        types::Rights::FD_WRITE | types::Rights::POLL_FD_READWRITE,
                    );
                    let entry = match self.get_entry(fd) {
                        Ok(entry) => entry,
                        Err(error) => {
                            events.push(types::Event {
                                userdata: subscription.userdata,
                                error: error.try_into().expect("non-trapping error"),
                                type_: types::Eventtype::FdWrite,
                                fd_readwrite: types::EventFdReadwrite {
                                    nbytes: 0,
                                    flags: types::Eventrwflags::empty(),
                                },
                            });
                            continue;
                        }
                    };
                    fd_events.push(sched::FdEventData {
                        handle: entry.as_handle(required_rights)?,
                        r#type: types::Eventtype::FdWrite,
                        userdata: subscription.userdata,
                    });
                }
            }
        }
        debug!(
            events = tracing::field::debug(&events),
            timeout = tracing::field::debug(timeout),
            "poll_oneoff"
        );
        // The underlying implementation should successfully and immediately return
        // if no events have been passed. Such situation may occur if all provided
        // events have been filtered out as errors in the code above.
        poll::oneoff(timeout, fd_events, &mut events)?;
        Ok(events)
    }
}
