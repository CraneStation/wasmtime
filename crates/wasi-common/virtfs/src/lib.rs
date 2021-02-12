#![allow(dead_code, unused_variables, unused_imports)]
use cap_std::time::{Duration, SystemTime};
use std::any::Any;
use std::cell::{Cell, Ref, RefCell, RefMut};
use std::collections::{hash_map::Entry, HashMap};
use std::convert::TryInto;
use std::io::{Cursor, IoSlice, IoSliceMut, Read, Seek, SeekFrom, Write};
use std::ops::Deref;
use std::path::PathBuf;
use std::rc::{Rc, Weak};
use tracing::trace;
use wasi_common::{
    clocks::WasiSystemClock,
    dir::{ReaddirCursor, ReaddirEntity, WasiDir},
    file::{Advice, FdFlags, FileCaps, FileType, Filestat, OFlags, WasiFile},
    Error, ErrorExt, SystemTimeSpec,
};

pub struct Filesystem {
    // Always .get() out a Some - this is an RefCell<Option to get around a circular init problem
    root: RefCell<Option<Rc<RefCell<DirInode>>>>,
    clock: Box<dyn WasiSystemClock>,
    device_id: u64,
    next_serial: Cell<u64>,
}

impl Filesystem {
    pub fn new(clock: Box<dyn WasiSystemClock>, device_id: u64) -> Rc<Filesystem> {
        let now = clock.now(Duration::from_secs(0));
        let fs = Rc::new(Filesystem {
            root: RefCell::new(None),
            clock,
            device_id,
            next_serial: Cell::new(1),
        });
        let root = Rc::new(RefCell::new(DirInode {
            fs: Rc::downgrade(&fs),
            serial: 0,
            parent: None,
            contents: HashMap::new(),
            atim: now,
            mtim: now,
            ctim: now,
        }));
        fs.root.replace(Some(root.clone()));
        fs
    }
    pub fn root(&self) -> Box<dyn WasiDir> {
        Box::new(Dir(self
            .root
            .borrow()
            .as_ref()
            .expect("root option always Some after init")
            .clone())) as Box<dyn WasiDir>
    }
    fn now(&self) -> SystemTime {
        self.clock.now(Duration::from_secs(0))
    }
    fn fresh_serial(&self) -> u64 {
        let s = self.next_serial.get();
        self.next_serial.set(s + 1);
        s
    }
    fn new_file(self: Rc<Self>) -> Rc<RefCell<FileInode>> {
        let now = self.now();
        let serial = self.fresh_serial();
        Rc::new(RefCell::new(FileInode {
            fs: Rc::downgrade(&self),
            serial,
            nlink: 1,
            contents: Vec::new(),
            atim: now,
            ctim: now,
            mtim: now,
        }))
    }
    fn new_dir(self: Rc<Self>, parent: &Rc<RefCell<DirInode>>) -> Rc<RefCell<DirInode>> {
        let now = self.now();
        let serial = self.fresh_serial();
        let parent = Some(Rc::downgrade(parent));
        Rc::new(RefCell::new(DirInode {
            fs: Rc::downgrade(&self),
            serial,
            parent,
            contents: HashMap::new(),
            atim: now,
            mtim: now,
            ctim: now,
        }))
    }
}

#[derive(Clone)]
pub enum Inode {
    Dir(Rc<RefCell<DirInode>>),
    File(Rc<RefCell<FileInode>>),
}

impl Inode {
    fn get_filestat(&self) -> Filestat {
        match self {
            Inode::File(f) => f.borrow().get_filestat(),
            Inode::Dir(f) => f.borrow().get_filestat(),
        }
    }
}

pub struct DirInode {
    fs: Weak<Filesystem>,
    serial: u64,
    parent: Option<Weak<RefCell<DirInode>>>,
    contents: HashMap<String, Inode>,
    atim: SystemTime,
    mtim: SystemTime,
    ctim: SystemTime,
}

impl DirInode {
    pub fn fs(&self) -> Rc<Filesystem> {
        Weak::upgrade(&self.fs).unwrap()
    }
    pub fn get_filestat(&self) -> Filestat {
        Filestat {
            device_id: self.fs().device_id,
            inode: self.serial,
            filetype: FileType::Directory,
            nlink: 0,
            size: self.contents.len() as u64,
            atim: Some(self.atim.into_std()),
            ctim: Some(self.ctim.into_std()),
            mtim: Some(self.mtim.into_std()),
        }
    }
}

pub struct FileInode {
    fs: Weak<Filesystem>,
    serial: u64,
    contents: Vec<u8>,
    nlink: u64,
    atim: SystemTime,
    mtim: SystemTime,
    ctim: SystemTime,
}

impl FileInode {
    pub fn fs(&self) -> Rc<Filesystem> {
        Weak::upgrade(&self.fs).unwrap()
    }
    pub fn get_filestat(&self) -> Filestat {
        Filestat {
            device_id: self.fs().device_id,
            inode: self.serial,
            filetype: FileType::RegularFile,
            nlink: self.nlink,
            size: self.contents.len() as u64,
            atim: Some(self.atim.into_std()),
            ctim: Some(self.ctim.into_std()),
            mtim: Some(self.mtim.into_std()),
        }
    }
    pub fn update_atim(&mut self) {
        let now = self.fs().now();
        self.atim = now;
    }
    pub fn link_increment(&mut self) {
        self.nlink += 1;
    }
    pub fn link_decrement(&mut self) {
        self.nlink -= 1;
    }
}

enum FileMode {
    ReadOnly,
    WriteOnly,
    ReadWrite,
}

pub struct File {
    inode: Rc<RefCell<FileInode>>,
    position: Cell<u64>,
    fdflags: FdFlags,
    mode: FileMode,
}

impl File {
    fn is_read(&self) -> bool {
        match self.mode {
            FileMode::ReadOnly | FileMode::ReadWrite => true,
            _ => false,
        }
    }
    fn is_write(&self) -> bool {
        match self.mode {
            FileMode::WriteOnly | FileMode::ReadWrite => true,
            _ => false,
        }
    }
    fn is_append(&self) -> bool {
        self.fdflags.contains(FdFlags::APPEND)
    }
    fn inode(&self) -> Ref<FileInode> {
        self.inode.borrow()
    }
    fn inode_mut(&self) -> RefMut<FileInode> {
        self.inode.borrow_mut()
    }
}

impl WasiFile for File {
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn datasync(&self) -> Result<(), Error> {
        Ok(())
    }
    fn sync(&self) -> Result<(), Error> {
        Ok(())
    }
    fn get_filetype(&self) -> Result<FileType, Error> {
        Ok(FileType::RegularFile)
    }
    fn get_fdflags(&self) -> Result<FdFlags, Error> {
        Ok(self.fdflags)
    }
    fn set_fdflags(&mut self, fdflags: FdFlags) -> Result<(), Error> {
        self.fdflags = fdflags;
        Ok(())
    }
    fn get_filestat(&self) -> Result<Filestat, Error> {
        Ok(self.inode().get_filestat())
    }
    fn set_filestat_size(&self, size: u64) -> Result<(), Error> {
        let mut inode = self.inode.borrow_mut();
        inode.contents.resize(size.try_into()?, 0);
        Ok(())
    }
    fn advise(&self, _offset: u64, _len: u64, _advice: Advice) -> Result<(), Error> {
        Ok(())
    }
    fn allocate(&self, offset: u64, len: u64) -> Result<(), Error> {
        let mut inode = self.inode.borrow_mut();
        let size = offset.checked_add(len).ok_or_else(|| Error::overflow())?;
        if size > inode.contents.len() as u64 {
            inode.contents.resize(size.try_into()?, 0);
        }
        Ok(())
    }
    fn set_times(
        &self,
        atime: Option<SystemTimeSpec>,
        mtime: Option<SystemTimeSpec>,
    ) -> Result<(), Error> {
        let newtime = |s| match s {
            SystemTimeSpec::SymbolicNow => self.inode().fs().clock.now(Duration::from_secs(0)),
            SystemTimeSpec::Absolute(t) => t,
        };
        let mut inode = self.inode.borrow_mut();
        if let Some(atim) = atime {
            inode.atim = newtime(atim);
        }
        if let Some(mtim) = mtime {
            inode.mtim = newtime(mtim);
        }
        Ok(())
    }
    fn read_vectored(&self, bufs: &mut [IoSliceMut]) -> Result<u64, Error> {
        if !self.is_read() {
            return Err(Error::badf());
        }
        let inode = self.inode();
        let mut cursor = Cursor::new(inode.contents.as_slice());
        cursor.set_position(self.position.get());
        let nbytes = cursor.read_vectored(bufs)?;
        self.position.set(cursor.position());
        Ok(nbytes.try_into()?)
    }
    fn read_vectored_at(&self, bufs: &mut [IoSliceMut], offset: u64) -> Result<u64, Error> {
        if !self.is_read() {
            return Err(Error::badf());
        }
        let inode = self.inode();
        let mut cursor = Cursor::new(inode.contents.as_slice());
        cursor.set_position(offset);
        let nbytes = cursor.read_vectored(bufs)?;
        Ok(nbytes.try_into()?)
    }
    fn write_vectored(&self, bufs: &[IoSlice]) -> Result<u64, Error> {
        if !self.is_write() {
            return Err(Error::badf());
        }
        let mut inode = self.inode_mut();
        let mut cursor = Cursor::new(&mut inode.contents);
        cursor.set_position(self.position.get());
        let nbytes = cursor.write_vectored(bufs)?;
        self.position.set(cursor.position());
        Ok(nbytes.try_into()?)
    }
    fn write_vectored_at(&self, bufs: &[IoSlice], offset: u64) -> Result<u64, Error> {
        if !self.is_write() || self.is_append() {
            return Err(Error::badf());
        }
        let mut inode = self.inode_mut();
        let mut cursor = Cursor::new(&mut inode.contents);
        cursor.set_position(offset);
        let nbytes = cursor.write_vectored(bufs)?;
        self.position.set(cursor.position());
        Ok(nbytes.try_into()?)
    }
    fn seek(&self, pos: SeekFrom) -> Result<u64, Error> {
        if self.is_append() {
            match pos {
                SeekFrom::Current(0) => return Ok(self.position.get()),
                _ => return Err(Error::badf()),
            }
        }
        let inode = self.inode();
        let mut cursor = Cursor::new(inode.contents.as_slice());
        cursor.set_position(self.position.get());
        cursor.seek(pos)?;
        let new_position = cursor.position();
        self.position.set(new_position);
        Ok(new_position)
    }
    fn peek(&self, buf: &mut [u8]) -> Result<u64, Error> {
        if !self.is_read() {
            return Err(Error::badf());
        }
        let inode = self.inode();
        let mut cursor = Cursor::new(inode.contents.as_slice());
        cursor.set_position(self.position.get());
        let nbytes = cursor.read(buf)?;
        Ok(nbytes.try_into()?)
    }
    fn num_ready_bytes(&self) -> Result<u64, Error> {
        if !self.is_read() {
            return Err(Error::badf());
        }
        let len = self.inode().contents.len() as u64;
        Ok(len - self.position.get())
    }
}

pub struct Dir(Rc<RefCell<DirInode>>);

impl Dir {
    fn at_path<F, A>(&self, path: &str, accept_trailing_slash: bool, f: F) -> Result<A, Error>
    where
        F: FnOnce(&Dir, &str) -> Result<A, Error>,
    {
        // Doesnt even attempt to deal with trailing slashes
        if let Some(slash_index) = path.find('/') {
            let dirname = &path[..slash_index];
            let rest = &path[slash_index + 1..];
            if rest == "" {
                if accept_trailing_slash {
                    return f(self, path);
                } else {
                    return Err(Error::not_found()
                        .context("empty filename, probably related to trailing slashes"));
                }
            }
            if let Some(inode) = self.0.borrow().contents.get(dirname) {
                match inode {
                    Inode::Dir(d) => Dir(d.clone()).at_path(rest, accept_trailing_slash, f),
                    Inode::File { .. } => Err(Error::not_found()),
                }
            } else {
                Err(Error::not_found())
            }
        } else {
            f(self, path)
        }
    }
    fn inode(&self) -> Ref<DirInode> {
        self.0.borrow()
    }
    fn inode_mut(&self) -> RefMut<DirInode> {
        self.0.borrow_mut()
    }
    fn child_dir(&self, name: &str) -> Result<Rc<RefCell<DirInode>>, Error> {
        if name == "." {
            return Ok(self.0.clone());
        }
        match self.0.borrow().contents.get(name) {
            Some(Inode::Dir(d)) => Ok(d.clone()),
            _ => Err(Error::not_found()),
        }
    }
    fn child_file(&self, name: &str) -> Result<Rc<RefCell<FileInode>>, Error> {
        match self.0.borrow().contents.get(name) {
            Some(Inode::File(f)) => Ok(f.clone()),
            _ => Err(Error::not_found()),
        }
    }
}

impl WasiDir for Dir {
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn open_file(
        &self,
        _symlink_follow: bool,
        path: &str,
        oflags: OFlags,
        read: bool,
        write: bool,
        fdflags: FdFlags,
    ) -> Result<Box<dyn WasiFile>, Error> {
        let mode = if read && write {
            FileMode::ReadWrite
        } else if write {
            FileMode::WriteOnly
        } else {
            FileMode::ReadOnly
        };
        self.at_path(path, false, |dir, filename| {
            if oflags.contains(OFlags::CREATE | OFlags::EXCLUSIVE) {
                match dir.child_file(filename) {
                    Err(_notfound) => {
                        let inode = dir.inode().fs().new_file();
                        dir.inode_mut()
                            .contents
                            .insert(filename.to_owned(), Inode::File(inode.clone()));
                        Ok(Box::new(File {
                            inode,
                            position: Cell::new(0),
                            fdflags,
                            mode,
                        }) as Box<dyn WasiFile>)
                    }
                    Ok(_) => Err(Error::exist()),
                }
            } else if oflags.contains(OFlags::CREATE) {
                match dir.child_file(filename) {
                    Ok(inode) => {
                        inode.borrow_mut().update_atim();
                        Ok(Box::new(File {
                            inode,
                            position: Cell::new(0),
                            fdflags,
                            mode,
                        }) as Box<dyn WasiFile>)
                    }
                    Err(_notfound) => {
                        let inode = dir.inode().fs().new_file();
                        dir.inode_mut()
                            .contents
                            .insert(filename.to_owned(), Inode::File(inode.clone()));
                        Ok(Box::new(File {
                            inode,
                            position: Cell::new(0),
                            fdflags,
                            mode,
                        }) as Box<dyn WasiFile>)
                    }
                }
            } else {
                let inode = dir.child_file(filename)?;
                inode.borrow_mut().update_atim();
                Ok(Box::new(File {
                    inode,
                    position: Cell::new(0),
                    fdflags,
                    mode,
                }) as Box<dyn WasiFile>)
            }
        })
    }

    fn open_dir(&self, _symlink_follow: bool, path: &str) -> Result<Box<dyn WasiDir>, Error> {
        self.at_path(path, true, |dir, dirname| {
            let d = dir.child_dir(dirname)?;
            Ok(Box::new(Dir(d)) as Box<dyn WasiDir>)
        })
    }

    fn create_dir(&self, path: &str) -> Result<(), Error> {
        self.at_path(path, true, |dir, dirname| {
            let d = dir.0.borrow();
            if d.contents.contains_key(dirname) {
                return Err(Error::exist());
            }
            let inode = d.fs().new_dir(&self.0);
            drop(d); // now need a mutable borrow
            let mut d = dir.0.borrow_mut();
            d.contents.insert(dirname.to_owned(), Inode::Dir(inode));
            Ok(())
        })
    }
    fn readdir(
        &self,
        cursor: ReaddirCursor,
    ) -> Result<Box<dyn Iterator<Item = Result<ReaddirEntity, Error>>>, Error> {
        let cursor = u64::from(cursor) as usize;
        Ok(Box::new(Readdir {
            inode: self.0.clone(),
            cursor,
        }))
    }

    fn symlink(&self, src_path: &str, dest_path: &str) -> Result<(), Error> {
        todo!()
    }
    fn remove_dir(&self, path: &str) -> Result<(), Error> {
        self.at_path(path, true, |dir, dirname| {
            let mut d = dir.inode_mut();
            match d.contents.get(dirname) {
                Some(Inode::File(_)) => return Err(Error::not_dir()),
                Some(Inode::Dir(d)) => {
                    if !d.borrow().contents.is_empty() {
                        return Err(Error::not_empty());
                    }
                }
                None => return Err(Error::not_found()),
            }
            d.contents.remove(dirname);
            Ok(())
        })
    }

    fn unlink_file(&self, path: &str) -> Result<(), Error> {
        self.at_path(path, false, |dir, filename| {
            let mut d = dir.inode_mut();
            match d.contents.get(filename) {
                Some(Inode::File(f)) => f.borrow_mut().link_decrement(),
                Some(Inode::Dir(d)) => {
                    return Err(Error::is_dir());
                }
                None => return Err(Error::not_found()),
            }
            d.contents.remove(filename);
            Ok(())
        })
    }
    fn read_link(&self, path: &str) -> Result<PathBuf, Error> {
        todo!()
    }
    fn get_filestat(&self) -> Result<Filestat, Error> {
        Ok(self.0.borrow().get_filestat())
    }
    fn get_path_filestat(&self, path: &str, follow_symlinks: bool) -> Result<Filestat, Error> {
        self.at_path(path, false, |dir, filename| {
            Ok(dir
                .inode()
                .contents
                .get(filename)
                .ok_or_else(|| Error::not_found())?
                .get_filestat())
        })
    }
    fn rename(&self, src_path: &str, dest_dir: &dyn WasiDir, dest_path: &str) -> Result<(), Error> {
        todo!()
    }
    fn hard_link(
        &self,
        src_path: &str,
        target_dir: &dyn WasiDir,
        target_path: &str,
    ) -> Result<(), Error> {
        self.at_path(src_path, false, |dir, filename| {
            let src_inode = match dir
                .inode()
                .contents
                .get(filename)
                .ok_or_else(|| Error::not_found().context("link source not found"))?
            {
                Inode::Dir(_) => {
                    Err(Error::permission_denied().context("link source cannot be directory"))?
                }
                Inode::File(f) => f.clone(),
            };
            let target_dir = target_dir.as_any().downcast_ref::<Dir>().ok_or_else(|| {
                Error::not_capable().context("link destination must be inside a virtfs::Dir")
            })?;
            target_dir.at_path(target_path, false, |dir, filename| {
                if Rc::as_ptr(&dir.inode().fs()) as usize
                    != Rc::as_ptr(&src_inode.borrow().fs()) as usize
                {
                    return Err(Error::not_supported()
                        .context("link source and destination must be in same filesystem"));
                }
                if dir.inode().contents.get(filename).is_some() {
                    return Err(Error::exist().context("link destination exists"));
                }
                src_inode.borrow_mut().link_increment();
                dir.inode_mut()
                    .contents
                    .insert(filename.to_owned(), Inode::File(src_inode));
                Ok(())
            })
        })
    }
    fn set_times(
        &self,
        path: &str,
        atime: Option<wasi_common::SystemTimeSpec>,
        mtime: Option<wasi_common::SystemTimeSpec>,
        follow_symlinks: bool,
    ) -> Result<(), Error> {
        todo!()
    }
}

struct Readdir {
    inode: Rc<RefCell<DirInode>>,
    cursor: usize,
}
impl Iterator for Readdir {
    type Item = Result<ReaddirEntity, Error>;
    fn next(&mut self) -> Option<Self::Item> {
        let cursor = self.cursor;
        self.cursor = cursor + 1;
        let next = (self.cursor as u64).into();
        if cursor == 0 {
            let stat = self.inode.borrow().get_filestat();
            Some(Ok(ReaddirEntity {
                filetype: stat.filetype,
                inode: stat.inode,
                next,
                name: ".".to_owned(),
            }))
        } else if cursor == 1 {
            let dir = self.inode.borrow();
            let stat = dir
                .parent
                .as_ref()
                .map(|p| Weak::upgrade(&p).unwrap().borrow().get_filestat())
                .unwrap_or_else(|| dir.get_filestat()); // Root is its own parent
            Some(Ok(ReaddirEntity {
                filetype: stat.filetype,
                inode: stat.inode,
                next,
                name: "..".to_owned(),
            }))
        } else {
            let inode = self.inode.borrow();
            let (name, child) = inode.contents.iter().nth(cursor - 2)?;
            let stat = child.get_filestat();
            Some(Ok(ReaddirEntity {
                filetype: stat.filetype,
                inode: stat.inode,
                next,
                name: name.to_owned(),
            }))
        }
    }
}
