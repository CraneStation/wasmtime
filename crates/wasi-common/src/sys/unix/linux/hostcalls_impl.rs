use crate::fdentry::Descriptor;
use crate::hostcalls_impl::PathGet;
use crate::Result;
use std::os::unix::prelude::AsRawFd;

pub(crate) fn path_unlink_file(resolved: PathGet) -> Result<()> {
    use yanix::file::{unlinkat, AtFlag};

    match resolved.dirfd() {
        Descriptor::OsHandle(file) => {
            unsafe { unlinkat(file.as_raw_fd(), resolved.path(), AtFlag::empty()) }
                .map_err(Into::into)
        }
        Descriptor::VirtualFile(virt) => virt.unlink_file(resolved.path()),
        Descriptor::Stdin | Descriptor::Stdout | Descriptor::Stderr => {
            unreachable!("streams do not have paths and should not be accessible via PathGet");
        }
    }
}

pub(crate) fn path_symlink(old_path: &str, resolved: PathGet) -> Result<()> {
    use yanix::file::symlinkat;

    log::debug!("path_symlink old_path = {:?}", old_path);
    log::debug!("path_symlink resolved = {:?}", resolved);

    match resolved.dirfd() {
        Descriptor::OsHandle(file) => {
            unsafe { symlinkat(old_path, file.as_raw_fd(), resolved.path()) }.map_err(Into::into)
        }
        Descriptor::VirtualFile(_) => {
            unimplemented!("virtual path_symlink");
        }
        Descriptor::Stdin | Descriptor::Stdout | Descriptor::Stderr => {
            unreachable!("streams do not have paths and should not be accessible via PathGet");
        }
    }
}

pub(crate) fn path_rename(resolved_old: PathGet, resolved_new: PathGet) -> Result<()> {
    use yanix::file::renameat;
    match (resolved_old.dirfd(), resolved_new.dirfd()) {
        (Descriptor::OsHandle(resolved_old_file), Descriptor::OsHandle(resolved_new_file)) => {
            unsafe {
                renameat(
                    resolved_old_file.as_raw_fd(),
                    resolved_old.path(),
                    resolved_new_file.as_raw_fd(),
                    resolved_new.path(),
                )
            }
            .map_err(Into::into)
        }
        _ => {
            unimplemented!("path_link with one or more virtual files");
        }
    }
}

pub(crate) mod fd_readdir_impl {
    use crate::sys::fdentry_impl::OsHandle;
    use crate::Result;
    use yanix::dir::Dir;

    pub(crate) fn get_dir_from_os_handle(os_handle: &mut OsHandle) -> Result<Box<Dir>> {
        // We need to duplicate the fd, because `opendir(3)`:
        //     After a successful call to fdopendir(), fd is used internally by the implementation,
        //     and should not otherwise be used by the application.
        // `opendir(3p)` also says that it's undefined behavior to
        // modify the state of the fd in a different way than by accessing DIR*.
        //
        // Still, rewinddir will be needed because the two file descriptors
        // share progress. But we can safely execute closedir now.
        let fd = os_handle.try_clone()?;
        // TODO This doesn't look very clean. Can we do something about it?
        // Boxing is needed here in order to satisfy `yanix`'s trait requirement for the `DirIter`
        // where `T: Deref<Target = Dir>`.
        Ok(Box::new(Dir::from(fd)?))
    }
}
