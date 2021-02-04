use crate::entry::{Entry, EntryHandle};
use crate::fdpool::FdPool;
use crate::handle::Handle;
use crate::string_array::{PendingString, StringArray, StringArrayError};
use crate::sys::osdir::OsDir;
use crate::sys::stdio::NullDevice;
use crate::sys::stdio::{Stderr, StderrExt, Stdin, StdinExt, Stdout, StdoutExt};
use crate::virtfs::{VirtualDir, VirtualDirEntry};
use crate::wasi::types::Fd;
use crate::Error;
use std::borrow::Borrow;
use std::cell::RefCell;
use std::collections::HashMap;
use std::convert::TryFrom;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::{env, io};

/// Possible errors when `WasiCtxBuilder` fails building
/// `WasiCtx`.
#[derive(Debug, thiserror::Error)]
pub enum WasiCtxBuilderError {
    /// General I/O error was encountered.
    #[error("general I/O error encountered: {0}")]
    Io(#[from] io::Error),
    /// Error constructing arguments
    #[error("while constructing arguments: {0}")]
    Args(#[source] StringArrayError),
    /// Error constructing environment
    #[error("while constructing environment: {0}")]
    Env(#[source] StringArrayError),
    /// The root of a VirtualDirEntry tree must be a VirtualDirEntry::Directory.
    #[error("the root of a VirtualDirEntry tree at {} must be a VirtualDirEntry::Directory", .0.display())]
    VirtualDirEntryRootNotADirectory(PathBuf),
    /// `WasiCtx` has too many opened files.
    #[error("context object has too many opened files")]
    TooManyFilesOpen,
}

type WasiCtxBuilderResult<T> = std::result::Result<T, WasiCtxBuilderError>;

enum PendingEntry {
    Thunk(fn() -> io::Result<Box<dyn Handle>>),
    Handle(Box<dyn Handle>),
}

impl std::fmt::Debug for PendingEntry {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Thunk(f) => write!(
                fmt,
                "PendingEntry::Thunk({:p})",
                f as *const fn() -> io::Result<Box<dyn Handle>>
            ),
            Self::Handle(handle) => write!(fmt, "PendingEntry::Handle({:p})", handle),
        }
    }
}

struct PendingPreopen(Box<dyn FnOnce() -> WasiCtxBuilderResult<Box<dyn Handle>>>);

impl PendingPreopen {
    fn new<F>(f: F) -> Self
    where
        F: FnOnce() -> WasiCtxBuilderResult<Box<dyn Handle>> + 'static,
    {
        Self(Box::new(f))
    }

    fn into(self) -> WasiCtxBuilderResult<Box<dyn Handle>> {
        self.0()
    }
}

/// A builder allowing customizable construction of `WasiCtx` instances.
pub struct WasiCtxBuilder {
    stdin: Option<PendingEntry>,
    stdout: Option<PendingEntry>,
    stderr: Option<PendingEntry>,
    preopens: Option<Vec<(PathBuf, PendingPreopen)>>,
    args: Option<Vec<PendingString>>,
    env: Option<HashMap<PendingString, PendingString>>,
}

impl WasiCtxBuilder {
    /// Builder for a new `WasiCtx`.
    pub fn new() -> Self {
        let stdin = Some(PendingEntry::Handle(Box::new(NullDevice::new())));
        let stdout = Some(PendingEntry::Handle(Box::new(NullDevice::new())));
        let stderr = Some(PendingEntry::Handle(Box::new(NullDevice::new())));

        Self {
            stdin,
            stdout,
            stderr,
            preopens: Some(Vec::new()),
            args: Some(Vec::new()),
            env: Some(HashMap::new()),
        }
    }

    /// Add arguments to the command-line arguments list.
    ///
    /// Arguments must be valid UTF-8 with no NUL bytes, or else `WasiCtxBuilder::build()` will fail.
    pub fn args<S: AsRef<[u8]>>(&mut self, args: impl IntoIterator<Item = S>) -> &mut Self {
        self.args
            .as_mut()
            .unwrap()
            .extend(args.into_iter().map(|a| a.as_ref().to_vec().into()));
        self
    }

    /// Add an argument to the command-line arguments list.
    ///
    /// Arguments must be valid UTF-8 with no NUL bytes, or else `WasiCtxBuilder::build()` will fail.
    pub fn arg<S: AsRef<[u8]>>(&mut self, arg: S) -> &mut Self {
        self.args
            .as_mut()
            .unwrap()
            .push(arg.as_ref().to_vec().into());
        self
    }

    /// Inherit the command-line arguments from the host process.
    ///
    /// If any arguments from the host process contain invalid UTF-8, `WasiCtxBuilder::build()` will
    /// fail.
    pub fn inherit_args(&mut self) -> &mut Self {
        let args = self.args.as_mut().unwrap();
        args.clear();
        args.extend(env::args_os().map(PendingString::OsString));
        self
    }

    /// Inherit stdin from the host process.
    pub fn inherit_stdin(&mut self) -> &mut Self {
        self.stdin = Some(PendingEntry::Thunk(Stdin::stdin));
        self
    }

    /// Inherit stdout from the host process.
    pub fn inherit_stdout(&mut self) -> &mut Self {
        self.stdout = Some(PendingEntry::Thunk(Stdout::stdout));
        self
    }

    /// Inherit stderr from the host process.
    pub fn inherit_stderr(&mut self) -> &mut Self {
        self.stderr = Some(PendingEntry::Thunk(Stderr::stderr));
        self
    }

    /// Inherit the stdin, stdout, and stderr streams from the host process.
    pub fn inherit_stdio(&mut self) -> &mut Self {
        self.stdin = Some(PendingEntry::Thunk(Stdin::stdin));
        self.stdout = Some(PendingEntry::Thunk(Stdout::stdout));
        self.stderr = Some(PendingEntry::Thunk(Stderr::stderr));
        self
    }

    /// Inherit the environment variables from the host process.
    ///
    /// If any environment variables from the host process contain invalid Unicode (UTF-16 for
    /// Windows, UTF-8 for other platforms), `WasiCtxBuilder::build()` will fail.
    pub fn inherit_env(&mut self) -> &mut Self {
        let env = self.env.as_mut().unwrap();
        env.clear();
        env.extend(std::env::vars_os().map(|(k, v)| (k.into(), v.into())));
        self
    }

    /// Add an entry to the environment.
    ///
    /// Environment variable keys and values must be valid UTF-8 with no NUL bytes, or else
    /// `WasiCtxBuilder::build()` will fail.
    pub fn env<S: AsRef<[u8]>>(&mut self, k: S, v: S) -> &mut Self {
        self.env
            .as_mut()
            .unwrap()
            .insert(k.as_ref().to_vec().into(), v.as_ref().to_vec().into());
        self
    }

    /// Add entries to the environment.
    ///
    /// Environment variable keys and values must be valid UTF-8 with no NUL bytes, or else
    /// `WasiCtxBuilder::build()` will fail.
    pub fn envs<S: AsRef<[u8]>, T: Borrow<(S, S)>>(
        &mut self,
        envs: impl IntoIterator<Item = T>,
    ) -> &mut Self {
        self.env.as_mut().unwrap().extend(envs.into_iter().map(|t| {
            let (k, v) = t.borrow();
            (k.as_ref().to_vec().into(), v.as_ref().to_vec().into())
        }));
        self
    }

    /// Provide a `Handle` to use as stdin
    pub fn stdin<T: Handle + 'static>(&mut self, handle: T) -> &mut Self {
        self.stdin = Some(PendingEntry::Handle(Box::new(handle)));
        self
    }

    /// Provide a `Handle` to use as stdout
    pub fn stdout<T: Handle + 'static>(&mut self, handle: T) -> &mut Self {
        self.stdout = Some(PendingEntry::Handle(Box::new(handle)));
        self
    }

    /// Provide a `Handle` to use as stderr
    pub fn stderr<T: Handle + 'static>(&mut self, handle: T) -> &mut Self {
        self.stderr = Some(PendingEntry::Handle(Box::new(handle)));
        self
    }

    /// Add a preopened directory.
    pub fn preopened_dir<P: AsRef<Path>>(&mut self, dir: File, guest_path: P) -> &mut Self {
        let preopen = PendingPreopen::new(move || {
            let dir = OsDir::try_from(dir).map_err(WasiCtxBuilderError::from)?;
            Ok(Box::new(dir))
        });
        self.preopens
            .as_mut()
            .unwrap()
            .push((guest_path.as_ref().to_owned(), preopen));
        self
    }

    /// Add a preopened virtual directory.
    pub fn preopened_virt<P: AsRef<Path>>(
        &mut self,
        dir: VirtualDirEntry,
        guest_path: P,
    ) -> &mut Self {
        fn populate_directory(virtentry: HashMap<String, VirtualDirEntry>, dir: &mut VirtualDir) {
            for (path, entry) in virtentry.into_iter() {
                match entry {
                    VirtualDirEntry::Directory(dir_entries) => {
                        let mut subdir = VirtualDir::new(true);
                        populate_directory(dir_entries, &mut subdir);
                        dir.add_dir(subdir, path);
                    }
                    VirtualDirEntry::File(content) => {
                        dir.add_file(content, path);
                    }
                }
            }
        }

        let guest_path_owned = guest_path.as_ref().to_owned();
        let preopen = PendingPreopen::new(move || {
            if let VirtualDirEntry::Directory(entries) = dir {
                let mut dir = VirtualDir::new(true);
                populate_directory(entries, &mut dir);
                Ok(Box::new(dir))
            } else {
                Err(WasiCtxBuilderError::VirtualDirEntryRootNotADirectory(
                    guest_path_owned,
                ))
            }
        });
        self.preopens
            .as_mut()
            .unwrap()
            .push((guest_path.as_ref().to_owned(), preopen));
        self
    }

    pub fn preopened_handle<P: AsRef<Path>>(
        &mut self,
        handle: Box<dyn Handle>,
        guest_path: P,
    ) -> &mut Self {
        let preopen = PendingPreopen::new(move || Ok(handle));
        self.preopens
            .as_mut()
            .unwrap()
            .push((guest_path.as_ref().to_owned(), preopen));
        self
    }

    /// Build a `WasiCtx`, consuming this `WasiCtxBuilder`.
    ///
    /// If any of the arguments or environment variables in this builder cannot be converted into
    /// `CString`s, either due to NUL bytes or Unicode conversions, this will fail.
    pub fn build(&mut self) -> WasiCtxBuilderResult<WasiCtx> {
        // Process arguments and environment variables into `String`s, failing quickly if they
        // contain any NUL bytes, or if conversion from `OsString` fails.
        let args =
            StringArray::from_pending_vec(self.args.take().expect("WasiCtxBuilder has args"))
                .map_err(WasiCtxBuilderError::Args)?;
        let env = StringArray::from_pending_map(self.env.take().expect("WasiCtxBuilder has env"))
            .map_err(WasiCtxBuilderError::Env)?;

        let mut entries = EntryTable::new();
        // Populate the non-preopen entries.
        for pending in vec![
            self.stdin.take().unwrap(),
            self.stdout.take().unwrap(),
            self.stderr.take().unwrap(),
        ] {
            tracing::debug!(
                pending = tracing::field::debug(&pending),
                "WasiCtx inserting entry"
            );
            let fd = match pending {
                PendingEntry::Thunk(f) => {
                    let handle = EntryHandle::from(f()?);
                    let entry = Entry::new(handle);
                    entries
                        .insert(entry)
                        .ok_or(WasiCtxBuilderError::TooManyFilesOpen)?
                }
                PendingEntry::Handle(handle) => {
                    let handle = EntryHandle::from(handle);
                    let entry = Entry::new(handle);
                    entries
                        .insert(entry)
                        .ok_or(WasiCtxBuilderError::TooManyFilesOpen)?
                }
            };
            tracing::debug!(fd = tracing::field::debug(fd), "WasiCtx inserted");
        }
        // Then add the preopen entries.
        for (guest_path, preopen) in self.preopens.take().unwrap() {
            let handle = EntryHandle::from(preopen.into()?);
            let mut entry = Entry::new(handle);
            entry.preopen_path = Some(guest_path);
            let fd = entries
                .insert(entry)
                .ok_or(WasiCtxBuilderError::TooManyFilesOpen)?;
            tracing::debug!(fd = tracing::field::debug(fd), "WasiCtx inserted",);
        }

        Ok(WasiCtx {
            args,
            env,
            entries: RefCell::new(entries),
        })
    }
}

struct EntryTable {
    fd_pool: FdPool,
    entries: HashMap<Fd, Rc<Entry>>,
}

impl EntryTable {
    fn new() -> Self {
        Self {
            fd_pool: FdPool::new(),
            entries: HashMap::new(),
        }
    }

    fn contains(&self, fd: &Fd) -> bool {
        self.entries.contains_key(fd)
    }

    fn insert(&mut self, entry: Entry) -> Option<Fd> {
        let fd = self.fd_pool.allocate()?;
        self.entries.insert(fd, Rc::new(entry));
        Some(fd)
    }

    fn insert_at(&mut self, fd: &Fd, entry: Rc<Entry>) {
        self.entries.insert(*fd, entry);
    }

    fn get(&self, fd: &Fd) -> Option<Rc<Entry>> {
        self.entries.get(fd).map(Rc::clone)
    }

    fn remove(&mut self, fd: Fd) -> Option<Rc<Entry>> {
        let entry = self.entries.remove(&fd)?;
        self.fd_pool.deallocate(fd);
        Some(entry)
    }
}

pub struct WasiCtx {
    entries: RefCell<EntryTable>,
    pub(crate) args: StringArray,
    pub(crate) env: StringArray,
}

impl WasiCtx {
    /// Make a new `WasiCtx` with some default settings.
    ///
    /// - File descriptors 0, 1, and 2 inherit stdin, stdout, and stderr from the host process.
    ///
    /// - Environment variables are inherited from the host process.
    ///
    /// To override these behaviors, use `WasiCtxBuilder`.
    pub fn new<S: AsRef<[u8]>>(args: impl IntoIterator<Item = S>) -> WasiCtxBuilderResult<Self> {
        WasiCtxBuilder::new()
            .args(args)
            .inherit_stdio()
            .inherit_env()
            .build()
    }

    /// Check if `WasiCtx` contains the specified raw WASI `fd`.
    pub(crate) fn contains_entry(&self, fd: Fd) -> bool {
        self.entries.borrow().contains(&fd)
    }

    /// Get an immutable `Entry` corresponding to the specified raw WASI `fd`.
    pub(crate) fn get_entry(&self, fd: Fd) -> Result<Rc<Entry>, Error> {
        match self.entries.borrow().get(&fd) {
            Some(entry) => Ok(entry),
            None => Err(Error::Badf),
        }
    }

    /// Insert the specified `Entry` into the `WasiCtx` object.
    ///
    /// The `Entry` will automatically get another free raw WASI `fd` assigned. Note that
    /// the two subsequent free raw WASI `fd`s do not have to be stored contiguously.
    pub(crate) fn insert_entry(&self, entry: Entry) -> Result<Fd, Error> {
        self.entries.borrow_mut().insert(entry).ok_or(Error::Mfile)
    }

    /// Insert the specified `Entry` with the specified raw WASI `fd` key into the `WasiCtx`
    /// object.
    pub(crate) fn insert_entry_at(&self, fd: Fd, entry: Rc<Entry>) {
        self.entries.borrow_mut().insert_at(&fd, entry)
    }

    /// Remove `Entry` corresponding to the specified raw WASI `fd` from the `WasiCtx` object.
    pub(crate) fn remove_entry(&self, fd: Fd) -> Result<Rc<Entry>, Error> {
        self.entries.borrow_mut().remove(fd).ok_or(Error::Badf)
    }

    /*
    pub(crate) fn args(&self) -> &impl StringArrayWriter {
        &self.args
    }

    pub(crate) fn env(&self) -> &impl StringArrayWriter {
        &self.env
    }
    */
}
