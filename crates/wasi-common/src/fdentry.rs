use crate::sys::dev_null;
use crate::sys::fdentry_impl::{determine_type_and_access_rights,
                               determine_type_and_access_rights_for_socket, OsFile};
use crate::{wasi, Error, Result};
use std::path::PathBuf;
use std::{fs, io, net};

#[derive(Debug)]
pub struct SocketDetails {
    pub socket_domain: i32,
    pub socket_type: u8,
    pub socket_protocol: i32,
}

#[derive(Debug)]
pub(crate) enum Descriptor {
    OsFile(OsFile),
    Socket(net::TcpStream),
    SocketFd(SocketDetails), // stores allocated fd, and options: domain, type and protocol
    Stdin,
    Stdout,
    Stderr,
}

impl Descriptor {
    pub(crate) fn as_file(&self) -> Result<&OsFile> {
        match self {
            Self::OsFile(file) => Ok(file),
            _ => Err(Error::EBADF),
        }
    }

    pub(crate) fn as_file_mut(&mut self) -> Result<&mut OsFile> {
        match self {
            Self::OsFile(file) => Ok(file),
            _ => Err(Error::EBADF),
        }
    }

    pub(crate) fn is_file(&self) -> bool {
        match self {
            Self::OsFile(_) => true,
            _ => false,
        }
    }

    #[allow(unused)]
    pub(crate) fn as_socket(&self) -> Result<&net::TcpStream> {
        match self {
            Descriptor::Socket(s) => Ok(s),
            // TODO: add a separate error code?
            _ => Err(Error::EBADF),
        }
    }

    pub(crate) fn is_socket(&self) -> bool {
        match self {
            Descriptor::Socket(_) => true,
            _ => false,
        }
    }

    pub(crate) fn is_socket_fd(&self) -> bool {
        match self {
            Descriptor::SocketFd(_) => true,
            _ => false,
        }
    }

    #[allow(unused)]
    pub(crate) fn is_stdin(&self) -> bool {
        match self {
            Self::Stdin => true,
            _ => false,
        }
    }

    #[allow(unused)]
    pub(crate) fn is_stdout(&self) -> bool {
        match self {
            Self::Stdout => true,
            _ => false,
        }
    }

    #[allow(unused)]
    pub(crate) fn is_stderr(&self) -> bool {
        match self {
            Self::Stderr => true,
            _ => false,
        }
    }
}

/// An abstraction struct serving as a wrapper for a host `Descriptor` object which requires
/// certain base rights `rights_base` and inheriting rights `rights_inheriting` in order to be
/// accessed correctly.
///
/// Here, the `descriptor` field stores the host `Descriptor` object (such as a file descriptor, or
/// stdin handle), and accessing it can only be done via the provided `FdEntry::as_descriptor` and
/// `FdEntry::as_descriptor_mut` methods which require a set of base and inheriting rights to be
/// specified, verifying whether the stored `Descriptor` object is valid for the rights specified.
#[derive(Debug)]
pub(crate) struct FdEntry {
    pub(crate) file_type: wasi::__wasi_filetype_t,
    descriptor: Descriptor,
    pub(crate) rights_base: wasi::__wasi_rights_t,
    pub(crate) rights_inheriting: wasi::__wasi_rights_t,
    pub(crate) preopen_path: Option<PathBuf>,
    // TODO: directories
}

impl FdEntry {
    pub(crate) fn from(file: fs::File) -> Result<Self> {
        unsafe { determine_type_and_access_rights(&file) }.map(
            |(file_type, rights_base, rights_inheriting)| Self {
                file_type,
                descriptor: Descriptor::OsFile(OsFile::from(file)),
                rights_base,
                rights_inheriting,
                preopen_path: None,
            },
        )
    }

    pub(crate) fn from_socket_details(details: SocketDetails) -> Result<Self> {
        unsafe { determine_type_and_access_rights_for_socket() }.map(
            |(file_type, rights_base, rights_inheriting)| Self {
                file_type,
                descriptor: Descriptor::SocketFd(details),
                rights_base,
                rights_inheriting,
                preopen_path: None,
            },
        )
    }

    pub(crate) fn duplicate(file: &fs::File) -> Result<Self> {
        Self::from(file.try_clone()?)
    }

    pub(crate) fn duplicate_stdin() -> Result<Self> {
        unsafe { determine_type_and_access_rights(&io::stdin()) }.map(
            |(file_type, rights_base, rights_inheriting)| Self {
                file_type,
                descriptor: Descriptor::Stdin,
                rights_base,
                rights_inheriting,
                preopen_path: None,
            },
        )
    }

    pub(crate) fn duplicate_stdout() -> Result<Self> {
        unsafe { determine_type_and_access_rights(&io::stdout()) }.map(
            |(file_type, rights_base, rights_inheriting)| Self {
                file_type,
                descriptor: Descriptor::Stdout,
                rights_base,
                rights_inheriting,
                preopen_path: None,
            },
        )
    }

    pub(crate) fn duplicate_stderr() -> Result<Self> {
        unsafe { determine_type_and_access_rights(&io::stderr()) }.map(
            |(file_type, rights_base, rights_inheriting)| Self {
                file_type,
                descriptor: Descriptor::Stderr,
                rights_base,
                rights_inheriting,
                preopen_path: None,
            },
        )
    }

    pub(crate) fn null() -> Result<Self> {
        Self::from(dev_null()?)
    }

    /// Convert this `FdEntry` into a host `Descriptor` object provided the specified
    /// `rights_base` and `rights_inheriting` rights are set on this `FdEntry` object.
    ///
    /// The `FdEntry` can only be converted into a valid `Descriptor` object if
    /// the specified set of base rights `rights_base`, and inheriting rights `rights_inheriting`
    /// is a subset of rights attached to this `FdEntry`. The check is performed using
    /// `FdEntry::validate_rights` method. If the check fails, `Error::ENOTCAPABLE` is returned.
    pub(crate) fn as_descriptor(
        &self,
        rights_base: wasi::__wasi_rights_t,
        rights_inheriting: wasi::__wasi_rights_t,
    ) -> Result<&Descriptor> {
        self.validate_rights(rights_base, rights_inheriting)?;
        Ok(&self.descriptor)
    }

    /// Convert this `FdEntry` into a mutable host `Descriptor` object provided the specified
    /// `rights_base` and `rights_inheriting` rights are set on this `FdEntry` object.
    ///
    /// The `FdEntry` can only be converted into a valid `Descriptor` object if
    /// the specified set of base rights `rights_base`, and inheriting rights `rights_inheriting`
    /// is a subset of rights attached to this `FdEntry`. The check is performed using
    /// `FdEntry::validate_rights` method. If the check fails, `Error::ENOTCAPABLE` is returned.
    pub(crate) fn as_descriptor_mut(
        &mut self,
        rights_base: wasi::__wasi_rights_t,
        rights_inheriting: wasi::__wasi_rights_t,
    ) -> Result<&mut Descriptor> {
        self.validate_rights(rights_base, rights_inheriting)?;
        Ok(&mut self.descriptor)
    }

    /// Check if this `FdEntry` object satisfies the specified base rights `rights_base`, and
    /// inheriting rights `rights_inheriting`; i.e., if rights attached to this `FdEntry` object
    /// are a superset.
    ///
    /// Upon unsuccessful check, `Error::ENOTCAPABLE` is returned.
    fn validate_rights(
        &self,
        rights_base: wasi::__wasi_rights_t,
        rights_inheriting: wasi::__wasi_rights_t,
    ) -> Result<()> {
        if !self.rights_base & rights_base != 0 || !self.rights_inheriting & rights_inheriting != 0
        {
            Err(Error::ENOTCAPABLE)
        } else {
            Ok(())
        }
    }
}
