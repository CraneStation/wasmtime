use crate::handle::{
    Advice, Dircookie, Dirent, Fdflags, Filesize, Filestat, Filetype, Fstflags, Handle,
    HandleRights, Oflags, Rights, RightsExt, Size, DIRCOOKIE_START,
};
use crate::sched::Timestamp;
use crate::{Error, Result};
use std::any::Any;
use std::cell::{Cell, RefCell};
use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::convert::TryInto;
use std::io;
use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use tracing::trace;

pub mod pipe;

/// An entry in a virtual filesystem
pub enum VirtualDirEntry {
    /// The contents of a child directory
    Directory(HashMap<String, VirtualDirEntry>),
    /// A file
    File(Box<dyn FileContents>),
}

impl VirtualDirEntry {
    /// Construct an empty directory
    pub fn empty_directory() -> Self {
        Self::Directory(HashMap::new())
    }
}

/// Files and directories may be moved, and for implementation reasons retain a reference to their
/// parent Handle, so files that can be moved must provide an interface to update their parent
/// reference.
pub(crate) trait MovableFile {
    fn set_parent(&self, new_parent: Option<Box<dyn Handle>>);
}

pub trait FileContents {
    /// The implementation-defined maximum size of the store corresponding to a `FileContents`
    /// implementation.
    fn max_size(&self) -> Filesize;
    /// The current number of bytes this `FileContents` describes.
    fn size(&self) -> Filesize;
    /// Resize to hold `new_size` number of bytes, or error if this is not possible.
    fn resize(&mut self, new_size: Filesize) -> Result<()>;
    /// Write a list of `IoSlice` starting at `offset`. `offset` plus the total size of all `iovs`
    /// is guaranteed to not exceed `max_size`. Implementations must not indicate more bytes have
    /// been written than can be held by `iovs`.
    fn pwritev(&mut self, iovs: &[io::IoSlice], offset: Filesize) -> Result<usize>;
    /// Read from the file from `offset`, filling a list of `IoSlice`. The returend size must not
    /// be more than the capactiy of `iovs`, and must not exceed the limit reported by
    /// `self.max_size()`.
    fn preadv(&self, iovs: &mut [io::IoSliceMut], offset: Filesize) -> Result<usize>;
    /// Write contents from `buf` to this file starting at `offset`. `offset` plus the length of
    /// `buf` is guaranteed to not exceed `max_size`. Implementations must not indicate more bytes
    /// have been written than the size of `buf`.
    fn pwrite(&mut self, buf: &[u8], offset: Filesize) -> Result<usize>;
    /// Read from the file at `offset`, filling `buf`. The returned size must not be more than the
    /// capacity of `buf`, and `offset` plus the returned size must not exceed `self.max_size()`.
    fn pread(&self, buf: &mut [u8], offset: Filesize) -> Result<usize>;
}

impl FileContents for VecFileContents {
    fn max_size(&self) -> Filesize {
        std::usize::MAX as Filesize
    }

    fn size(&self) -> Filesize {
        self.content.len() as Filesize
    }

    fn resize(&mut self, new_size: Filesize) -> Result<()> {
        let new_size: usize = new_size.try_into().map_err(|_| Error::Inval)?;
        self.content.resize(new_size, 0);
        Ok(())
    }

    fn preadv(&self, iovs: &mut [io::IoSliceMut], offset: Filesize) -> Result<usize> {
        let mut read_total = 0usize;
        for iov in iovs.iter_mut() {
            let skip: u64 = read_total.try_into().map_err(|_| Error::Inval)?;
            let read = self.pread(iov, offset + skip)?;
            read_total = read_total.checked_add(read).expect("FileContents::preadv must not be called when reads could total to more bytes than the return value can hold");
        }
        Ok(read_total)
    }

    fn pwritev(&mut self, iovs: &[io::IoSlice], offset: Filesize) -> Result<usize> {
        let mut write_total = 0usize;
        for iov in iovs.iter() {
            let skip: u64 = write_total.try_into().map_err(|_| Error::Inval)?;
            let written = self.pwrite(iov, offset + skip)?;
            write_total = write_total.checked_add(written).expect("FileContents::pwritev must not be called when writes could total to more bytes than the return value can hold");
        }
        Ok(write_total)
    }

    fn pread(&self, buf: &mut [u8], offset: Filesize) -> Result<usize> {
        trace!(buffer_length = buf.len(), offset = offset, "pread");
        let offset: usize = offset.try_into().map_err(|_| Error::Inval)?;

        let data_remaining = self.content.len().saturating_sub(offset);

        let read_count = std::cmp::min(buf.len(), data_remaining);

        (&mut buf[..read_count]).copy_from_slice(&self.content[offset..][..read_count]);

        Ok(read_count)
    }

    fn pwrite(&mut self, buf: &[u8], offset: Filesize) -> Result<usize> {
        let offset: usize = offset.try_into().map_err(|_| Error::Inval)?;

        let write_end = offset.checked_add(buf.len()).ok_or(Error::Fbig)?;

        if write_end > self.content.len() {
            self.content.resize(write_end, 0);
        }

        (&mut self.content[offset..][..buf.len()]).copy_from_slice(buf);

        Ok(buf.len())
    }
}

pub struct VecFileContents {
    content: Vec<u8>,
}

impl VecFileContents {
    pub fn new() -> Self {
        Self {
            content: Vec::new(),
        }
    }

    pub fn with_content(content: Vec<u8>) -> Self {
        Self { content }
    }
}

/// An `InMemoryFile` is a shared handle to some underlying data. The relationship is analagous to
/// a filesystem wherein a file descriptor is one view into a possibly-shared underlying collection
/// of data and permissions on a filesystem.
pub struct InMemoryFile {
    rights: Cell<HandleRights>,
    cursor: Cell<Filesize>,
    parent: Rc<RefCell<Option<Box<dyn Handle>>>>,
    fd_flags: Cell<Fdflags>,
    data: Rc<RefCell<Box<dyn FileContents>>>,
}

impl InMemoryFile {
    pub fn memory_backed() -> Self {
        Self::new(Box::new(VecFileContents::new()))
    }

    pub fn new(contents: Box<dyn FileContents>) -> Self {
        let rights = HandleRights::new(
            Rights::regular_file_base(),
            Rights::regular_file_inheriting(),
        );
        let rights = Cell::new(rights);
        Self {
            rights,
            cursor: Cell::new(0),
            fd_flags: Cell::new(Fdflags::empty()),
            parent: Rc::new(RefCell::new(None)),
            data: Rc::new(RefCell::new(contents)),
        }
    }
}

impl MovableFile for InMemoryFile {
    fn set_parent(&self, new_parent: Option<Box<dyn Handle>>) {
        *self.parent.borrow_mut() = new_parent;
    }
}

impl Handle for InMemoryFile {
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn try_clone(&self) -> io::Result<Box<dyn Handle>> {
        Ok(Box::new(Self {
            rights: self.rights.clone(),
            cursor: Cell::new(0),
            fd_flags: self.fd_flags.clone(),
            parent: Rc::clone(&self.parent),
            data: Rc::clone(&self.data),
        }))
    }
    fn get_file_type(&self) -> Filetype {
        Filetype::RegularFile
    }
    fn get_rights(&self) -> HandleRights {
        self.rights.get()
    }
    fn set_rights(&self, rights: HandleRights) {
        self.rights.set(rights)
    }
    // FdOps
    fn advise(&self, _advice: Advice, _offset: Filesize, _len: Filesize) -> Result<()> {
        // we'll just ignore advice for now, unless it's totally invalid
        Ok(())
    }
    fn allocate(&self, offset: Filesize, len: Filesize) -> Result<()> {
        let new_limit = offset.checked_add(len).ok_or(Error::Fbig)?;
        let mut data = self.data.borrow_mut();

        if new_limit > data.max_size() {
            return Err(Error::Fbig);
        }

        if new_limit > data.size() {
            data.resize(new_limit)?;
        }

        Ok(())
    }
    fn fdstat_get(&self) -> Result<Fdflags> {
        Ok(self.fd_flags.get())
    }
    fn fdstat_set_flags(&self, fdflags: Fdflags) -> Result<()> {
        self.fd_flags.set(fdflags);
        Ok(())
    }
    fn filestat_get(&self) -> Result<Filestat> {
        let stat = Filestat {
            dev: 0,
            ino: 0,
            nlink: 0,
            size: self.data.borrow().size(),
            atim: 0,
            ctim: 0,
            mtim: 0,
            filetype: self.get_file_type(),
        };
        Ok(stat)
    }
    fn filestat_set_size(&self, st_size: Filesize) -> Result<()> {
        let mut data = self.data.borrow_mut();
        if st_size > data.max_size() {
            return Err(Error::Fbig);
        }
        data.resize(st_size)
    }
    fn preadv(&self, buf: &mut [io::IoSliceMut], offset: Filesize) -> Result<usize> {
        self.data.borrow_mut().preadv(buf, offset)
    }
    fn pwritev(&self, buf: &[io::IoSlice], offset: Filesize) -> Result<usize> {
        self.data.borrow_mut().pwritev(buf, offset)
    }
    fn read_vectored(&self, iovs: &mut [io::IoSliceMut]) -> Result<usize> {
        trace!("read_vectored(iovs={:?})", iovs);
        trace!("     | *read_start={:?}", self.cursor.get());
        let read = self.data.borrow_mut().preadv(iovs, self.cursor.get())?;
        let offset: u64 = read.try_into().map_err(|_| Error::Inval)?;
        let update = self.cursor.get().checked_add(offset).ok_or(Error::Inval)?;
        self.cursor.set(update);
        Ok(read)
    }
    fn seek(&self, offset: SeekFrom) -> Result<Filesize> {
        let content_len = self.data.borrow().size();
        match offset {
            SeekFrom::Current(offset) => {
                let new_cursor = if offset < 0 {
                    self.cursor
                        .get()
                        .checked_sub(offset.wrapping_neg() as u64)
                        .ok_or(Error::Inval)?
                } else {
                    self.cursor
                        .get()
                        .checked_add(offset as u64)
                        .ok_or(Error::Inval)?
                };
                self.cursor.set(std::cmp::min(content_len, new_cursor));
            }
            SeekFrom::End(offset) => {
                // A negative offset from the end would be past the end of the file,
                let offset: u64 = offset.try_into().map_err(|_| Error::Inval)?;
                self.cursor.set(content_len.saturating_sub(offset));
            }
            SeekFrom::Start(offset) => {
                // A negative offset from the end would be before the start of the file.
                let offset: u64 = offset.try_into().map_err(|_| Error::Inval)?;
                self.cursor.set(std::cmp::min(content_len, offset));
            }
        }

        Ok(self.cursor.get())
    }
    fn write_vectored(&self, iovs: &[io::IoSlice]) -> Result<usize> {
        trace!("write_vectored(iovs={:?})", iovs);
        let mut data = self.data.borrow_mut();

        let append_mode = self.fd_flags.get().contains(Fdflags::APPEND);
        trace!("     | fd_flags={}", self.fd_flags.get());

        // If this file is in append mode, we write to the end.
        let write_start = if append_mode {
            data.size()
        } else {
            self.cursor.get()
        };

        let max_size = iovs
            .iter()
            .map(|iov| {
                let cast_iovlen: Size = iov
                    .len()
                    .try_into()
                    .expect("iovec are bounded by wasi max sizes");
                cast_iovlen
            })
            .fold(Some(0u32), |len, iov| len.and_then(|x| x.checked_add(iov)))
            .expect("write_vectored will not be called with invalid iovs");

        if let Some(end) = write_start.checked_add(max_size as Filesize) {
            if end > data.max_size() {
                return Err(Error::Fbig);
            }
        } else {
            return Err(Error::Fbig);
        }

        trace!("     | *write_start={:?}", write_start);
        let written = data.pwritev(iovs, write_start)?;

        // If we are not appending, adjust the cursor appropriately for the write, too. This can't
        // overflow, as we checked against that before writing any data.
        if !append_mode {
            let update = self.cursor.get() + written as u64;
            self.cursor.set(update);
        }

        Ok(written)
    }
    // PathOps
    fn create_directory(&self, _path: &str) -> Result<()> {
        Err(Error::Notdir)
    }
    fn openat(
        &self,
        path: &str,
        _read: bool,
        _write: bool,
        oflags: Oflags,
        _fd_flags: Fdflags,
    ) -> Result<Box<dyn Handle>> {
        if oflags.contains(Oflags::DIRECTORY) {
            tracing::trace!(
                "InMemoryFile::openat was passed oflags DIRECTORY, but {:?} is a file.",
                path
            );
            return Err(Error::Notdir);
        }

        if path == "." {
            return self.try_clone().map_err(Into::into);
        } else if path == ".." {
            match &*self.parent.borrow() {
                Some(file) => file.try_clone().map_err(Into::into),
                None => self.try_clone().map_err(Into::into),
            }
        } else {
            Err(Error::Acces)
        }
    }
    fn link(
        &self,
        _old_path: &str,
        _new_handle: Box<dyn Handle>,
        _new_path: &str,
        _follow: bool,
    ) -> Result<()> {
        Err(Error::Notdir)
    }
    fn readlink(&self, _path: &str, _buf: &mut [u8]) -> Result<usize> {
        Err(Error::Notdir)
    }
    fn readlinkat(&self, _path: &str) -> Result<String> {
        Err(Error::Notdir)
    }
    fn rename(&self, _old_path: &str, _new_handle: Box<dyn Handle>, _new_path: &str) -> Result<()> {
        Err(Error::Notdir)
    }
    fn remove_directory(&self, _path: &str) -> Result<()> {
        Err(Error::Notdir)
    }
    fn symlink(&self, _old_path: &str, _new_path: &str) -> Result<()> {
        Err(Error::Notdir)
    }
    fn unlink_file(&self, _path: &str) -> Result<()> {
        Err(Error::Notdir)
    }
}

/// A clonable read/write directory.
pub struct VirtualDir {
    rights: Cell<HandleRights>,
    writable: bool,
    // All copies of this `VirtualDir` must share `parent`, and changes in one copy's `parent`
    // must be reflected in all handles, so they share `Rc` of an underlying `parent`.
    parent: Rc<RefCell<Option<Box<dyn Handle>>>>,
    entries: Rc<RefCell<HashMap<PathBuf, Box<dyn Handle>>>>,
}

impl VirtualDir {
    pub fn new(writable: bool) -> Self {
        let rights = HandleRights::new(Rights::directory_base(), Rights::directory_inheriting());
        let rights = Cell::new(rights);
        Self {
            rights,
            writable,
            parent: Rc::new(RefCell::new(None)),
            entries: Rc::new(RefCell::new(HashMap::new())),
        }
    }

    pub fn with_dir<P: AsRef<Path>>(mut self, dir: Self, path: P) -> Self {
        self.add_dir(dir, path);
        self
    }

    pub fn add_dir<P: AsRef<Path>>(&mut self, dir: Self, path: P) {
        let entry = Box::new(dir);
        entry.set_parent(Some(self.try_clone().expect("can clone self")));
        self.entries
            .borrow_mut()
            .insert(path.as_ref().to_owned(), entry);
    }

    pub fn with_file<P: AsRef<Path>>(mut self, content: Box<dyn FileContents>, path: P) -> Self {
        self.add_file(content, path);
        self
    }

    pub fn add_file<P: AsRef<Path>>(&mut self, content: Box<dyn FileContents>, path: P) {
        let entry = Box::new(InMemoryFile::new(content));
        entry.set_parent(Some(self.try_clone().expect("can clone self")));
        self.entries
            .borrow_mut()
            .insert(path.as_ref().to_owned(), entry);
    }
}

impl MovableFile for VirtualDir {
    fn set_parent(&self, new_parent: Option<Box<dyn Handle>>) {
        *self.parent.borrow_mut() = new_parent;
    }
}

const SELF_DIR_COOKIE: u32 = 0;
const PARENT_DIR_COOKIE: u32 = 1;

// This MUST be the number of constants above. This limit is used to prevent allocation of files
// that would wrap and be mapped to the same dir cookies as `self` or `parent`.
const RESERVED_ENTRY_COUNT: u32 = 2;

impl Handle for VirtualDir {
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn try_clone(&self) -> io::Result<Box<dyn Handle>> {
        Ok(Box::new(Self {
            rights: self.rights.clone(),
            writable: self.writable,
            parent: Rc::clone(&self.parent),
            entries: Rc::clone(&self.entries),
        }))
    }
    fn get_file_type(&self) -> Filetype {
        Filetype::Directory
    }
    fn get_rights(&self) -> HandleRights {
        self.rights.get()
    }
    fn set_rights(&self, rights: HandleRights) {
        self.rights.set(rights)
    }
    // FdOps
    fn filestat_get(&self) -> Result<Filestat> {
        let stat = Filestat {
            dev: 0,
            ino: 0,
            nlink: 0,
            size: 0,
            atim: 0,
            ctim: 0,
            mtim: 0,
            filetype: self.get_file_type(),
        };
        Ok(stat)
    }
    fn readdir(
        &self,
        cookie: Dircookie,
    ) -> Result<Box<dyn Iterator<Item = Result<(Dirent, String)>>>> {
        struct VirtualDirIter {
            start: u32,
            entries: Rc<RefCell<HashMap<PathBuf, Box<dyn Handle>>>>,
        }
        impl Iterator for VirtualDirIter {
            type Item = Result<(Dirent, String)>;

            fn next(&mut self) -> Option<Self::Item> {
                tracing::trace!("VirtualDirIter::next continuing from {}", self.start);
                if self.start == SELF_DIR_COOKIE {
                    self.start += 1;
                    let name = ".".to_owned();
                    let dirent = Dirent {
                        d_next: self.start as u64,
                        d_ino: 0,
                        d_namlen: name.len() as _,
                        d_type: Filetype::Directory,
                    };
                    return Some(Ok((dirent, name)));
                }
                if self.start == PARENT_DIR_COOKIE {
                    self.start += 1;
                    let name = "..".to_owned();
                    let dirent = Dirent {
                        d_next: self.start as u64,
                        d_ino: 0,
                        d_namlen: name.len() as _,
                        d_type: Filetype::Directory,
                    };
                    return Some(Ok((dirent, name)));
                }

                let entries = self.entries.borrow();

                // Adjust `start` to be an appropriate number of HashMap entries.
                let start = self.start - RESERVED_ENTRY_COUNT;
                if start as usize >= entries.len() {
                    return None;
                }

                self.start += 1;

                let (path, file) = entries
                    .iter()
                    .skip(start as usize)
                    .next()
                    .expect("seeked less than the length of entries");

                let name = path
                    .to_str()
                    .expect("wasi paths are valid utf8 strings")
                    .to_owned();
                let dirent = || -> Result<Dirent> {
                    let dirent = Dirent {
                        d_namlen: name.len().try_into()?,
                        d_type: file.get_file_type(),
                        d_ino: 0,
                        d_next: self.start as u64,
                    };
                    Ok(dirent)
                };
                Some(dirent().map(|dirent| (dirent, name)))
            }
        }
        let cookie = match cookie.try_into() {
            Ok(cookie) => cookie,
            Err(_) => {
                // Cookie is larger than u32. it doesn't seem like there's an explicit error
                // condition in POSIX or WASI, so just start from the start?
                0
            }
        };
        Ok(Box::new(VirtualDirIter {
            start: cookie,
            entries: Rc::clone(&self.entries),
        }))
    }
    // PathOps
    fn create_directory(&self, path: &str) -> Result<()> {
        let mut entries = self.entries.borrow_mut();
        match entries.entry(PathBuf::from(path)) {
            Entry::Occupied(_) => Err(Error::Exist),
            Entry::Vacant(v) => {
                if self.writable {
                    let new_dir = Box::new(Self::new(true));
                    new_dir.set_parent(Some(self.try_clone()?));
                    v.insert(new_dir);
                    Ok(())
                } else {
                    Err(Error::Acces)
                }
            }
        }
    }
    fn filestat_get_at(&self, path: &str, _follow: bool) -> Result<Filestat> {
        let stat = self
            .openat(path, false, false, Oflags::empty(), Fdflags::empty())?
            .filestat_get()?;
        Ok(stat)
    }
    fn filestat_set_times_at(
        &self,
        path: &str,
        atim: Timestamp,
        mtim: Timestamp,
        fst_flags: Fstflags,
        _follow: bool,
    ) -> Result<()> {
        self.openat(path, false, false, Oflags::empty(), Fdflags::empty())?
            .filestat_set_times(atim, mtim, fst_flags)?;
        Ok(())
    }
    fn openat(
        &self,
        path: &str,
        _read: bool,
        _write: bool,
        oflags: Oflags,
        fd_flags: Fdflags,
    ) -> Result<Box<dyn Handle>> {
        if path == "." {
            return self.try_clone().map_err(Into::into);
        } else if path == ".." {
            match &*self.parent.borrow() {
                Some(file) => {
                    return file.try_clone().map_err(Into::into);
                }
                None => {
                    return self.try_clone().map_err(Into::into);
                }
            }
        }

        // openat may have been passed a path with a trailing slash, but files are mapped to paths
        // with trailing slashes normalized out.
        let file_name = Path::new(path).file_name().ok_or(Error::Inval)?;
        let mut entries = self.entries.borrow_mut();
        let entry_count = entries.len();
        match entries.entry(Path::new(file_name).to_path_buf()) {
            Entry::Occupied(e) => {
                let creat_excl_mask = Oflags::CREAT | Oflags::EXCL;
                if (oflags & creat_excl_mask) == creat_excl_mask {
                    tracing::trace!("VirtualDir::openat was passed oflags CREAT|EXCL, but the file {:?} exists.", file_name);
                    return Err(Error::Exist);
                }

                if oflags.contains(Oflags::DIRECTORY)
                    && e.get().get_file_type() != Filetype::Directory
                {
                    tracing::trace!(
                        "VirtualDir::openat was passed oflags DIRECTORY, but {:?} is a file.",
                        file_name
                    );
                    return Err(Error::Notdir);
                }

                e.get().try_clone().map_err(Into::into)
            }
            Entry::Vacant(v) => {
                if oflags.contains(Oflags::CREAT) {
                    if self.writable {
                        // Enforce a hard limit at `u32::MAX - 2` files.
                        // This is to have a constant limit (rather than target-dependent limit we
                        // would have with `usize`. The limit is the full `u32` range minus two so we
                        // can reserve "self" and "parent" cookie values.
                        if entry_count >= (std::u32::MAX - RESERVED_ENTRY_COUNT) as usize {
                            return Err(Error::Nospc);
                        }

                        tracing::trace!(
                            "VirtualDir::openat creating an InMemoryFile named {}",
                            path
                        );

                        let file = Box::new(InMemoryFile::memory_backed());
                        file.fd_flags.set(fd_flags);
                        file.set_parent(Some(self.try_clone().expect("can clone self")));
                        v.insert(file).try_clone().map_err(Into::into)
                    } else {
                        Err(Error::Acces)
                    }
                } else {
                    Err(Error::Noent)
                }
            }
        }
    }
    fn readlinkat(&self, _path: &str) -> Result<String> {
        // Files are not symbolic links or directories, faithfully report Notdir.
        Err(Error::Notdir)
    }
    fn remove_directory(&self, path: &str) -> Result<()> {
        let trimmed_path = path.trim_end_matches('/');
        let mut entries = self.entries.borrow_mut();
        match entries.entry(Path::new(trimmed_path).to_path_buf()) {
            Entry::Occupied(e) => {
                // first, does this name a directory?
                if e.get().get_file_type() != Filetype::Directory {
                    return Err(Error::Notdir);
                }

                // Okay, but is the directory empty?
                let iter = e.get().readdir(DIRCOOKIE_START)?;
                if iter.skip(RESERVED_ENTRY_COUNT as usize).next().is_some() {
                    return Err(Error::Notempty);
                }

                // Alright, it's an empty directory. We can remove it.
                let removed = e.remove_entry();

                // TODO refactor
                // And sever the file's parent ref to avoid Rc cycles.
                if let Some(dir) = removed.1.as_any().downcast_ref::<Self>() {
                    dir.set_parent(None);
                } else if let Some(file) = removed.1.as_any().downcast_ref::<InMemoryFile>() {
                    file.set_parent(None);
                } else {
                    panic!("neither VirtualDir nor InMemoryFile");
                }

                Ok(())
            }
            Entry::Vacant(_) => {
                tracing::trace!(
                    "VirtualDir::remove_directory failed to remove {}, no such entry",
                    trimmed_path
                );
                Err(Error::Noent)
            }
        }
    }
    fn unlink_file(&self, path: &str) -> Result<()> {
        let trimmed_path = path.trim_end_matches('/');

        // Special case: we may be unlinking this directory itself if path is `"."`. In that case,
        // fail with Isdir, since this is a directory. Alternatively, we may be unlinking `".."`,
        // which is bound the same way, as this is by definition contained in a directory.
        if trimmed_path == "." || trimmed_path == ".." {
            return Err(Error::Isdir);
        }

        let mut entries = self.entries.borrow_mut();
        match entries.entry(Path::new(trimmed_path).to_path_buf()) {
            Entry::Occupied(e) => {
                // Directories must be removed through `remove_directory`, not `unlink_file`.
                if e.get().get_file_type() == Filetype::Directory {
                    return Err(Error::Isdir);
                }

                let removed = e.remove_entry();

                // TODO refactor
                // Sever the file's parent ref to avoid Rc cycles.
                if let Some(dir) = removed.1.as_any().downcast_ref::<Self>() {
                    dir.set_parent(None);
                } else if let Some(file) = removed.1.as_any().downcast_ref::<InMemoryFile>() {
                    file.set_parent(None);
                } else {
                    panic!("neither VirtualDir nor InMemoryFile");
                }

                Ok(())
            }
            Entry::Vacant(_) => {
                tracing::trace!(
                    "VirtualDir::unlink_file failed to remove {}, no such entry",
                    trimmed_path
                );
                Err(Error::Noent)
            }
        }
    }
}
