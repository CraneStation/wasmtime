use crate::sys::AsFile;
use std::cell::Cell;
use std::fs::File;
use std::io;
use std::mem::ManuallyDrop;
use std::ops::Deref;
use std::os::windows::prelude::{AsRawHandle, FromRawHandle, IntoRawHandle, RawHandle};

#[derive(Debug)]
pub(crate) struct OsHandle(Cell<RawHandle>);

impl OsHandle {
    /// Tries clone `self`.
    pub(crate) fn try_clone(&self) -> io::Result<Self> {
        let handle = self.as_file().try_clone()?;
        Ok(Self(Cell::new(handle.into_raw_handle())))
    }
    /// Consumes `other` taking the ownership of the underlying
    /// `RawHandle` file handle.
    pub(crate) fn update_from(&self, other: Self) {
        let new_handle = other.into_raw_handle();
        let old_handle = self.0.get();
        self.0.set(new_handle);
        // We need to remember to close the old_handle.
        unsafe {
            File::from_raw_handle(old_handle);
        }
    }
}

impl Drop for OsHandle {
    fn drop(&mut self) {
        unsafe {
            File::from_raw_handle(self.as_raw_handle());
        }
    }
}

impl AsRawHandle for OsHandle {
    fn as_raw_handle(&self) -> RawHandle {
        self.0.get()
    }
}

impl FromRawHandle for OsHandle {
    unsafe fn from_raw_handle(handle: RawHandle) -> Self {
        Self(Cell::new(handle))
    }
}

impl IntoRawHandle for OsHandle {
    fn into_raw_handle(self) -> RawHandle {
        // We need to prevent dropping of the OsFile
        let wrapped = ManuallyDrop::new(self);
        wrapped.0.get()
    }
}

#[derive(Debug)]
pub(crate) struct OsDirHandle(OsHandle);

impl OsDirHandle {
    /// Consumes the spcified `OsHandle`.
    pub(crate) fn new(handle: OsHandle) -> io::Result<Self> {
        Ok(Self(handle))
    }
    /// Tries clone `self`.
    pub(crate) fn try_clone(&self) -> io::Result<Self> {
        let handle = self.0.try_clone()?;
        Self::new(handle)
    }
}

impl Deref for OsDirHandle {
    type Target = OsHandle;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
