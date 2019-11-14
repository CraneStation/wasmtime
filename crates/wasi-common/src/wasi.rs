//! Types and constants shared between 32-bit and 64-bit wasi. Types involving
//! pointer or `usize`-sized data are excluded here, so this file only contains
//! fixed-size types, so it's host/target independent.

#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(dead_code)]

use wig::witx_wasi_types;

witx_wasi_types!("unstable" "wasi_unstable_preview0");

pub(crate) const RIGHTS_ALL: __wasi_rights_t = __WASI_RIGHT_FD_DATASYNC
    | __WASI_RIGHT_FD_READ
    | __WASI_RIGHT_FD_SEEK
    | __WASI_RIGHT_FD_FDSTAT_SET_FLAGS
    | __WASI_RIGHT_FD_SYNC
    | __WASI_RIGHT_FD_TELL
    | __WASI_RIGHT_FD_WRITE
    | __WASI_RIGHT_FD_ADVISE
    | __WASI_RIGHT_FD_ALLOCATE
    | __WASI_RIGHT_PATH_CREATE_DIRECTORY
    | __WASI_RIGHT_PATH_CREATE_FILE
    | __WASI_RIGHT_PATH_LINK_SOURCE
    | __WASI_RIGHT_PATH_LINK_TARGET
    | __WASI_RIGHT_PATH_OPEN
    | __WASI_RIGHT_FD_READDIR
    | __WASI_RIGHT_PATH_READLINK
    | __WASI_RIGHT_PATH_RENAME_SOURCE
    | __WASI_RIGHT_PATH_RENAME_TARGET
    | __WASI_RIGHT_PATH_FILESTAT_GET
    | __WASI_RIGHT_PATH_FILESTAT_SET_SIZE
    | __WASI_RIGHT_PATH_FILESTAT_SET_TIMES
    | __WASI_RIGHT_FD_FILESTAT_GET
    | __WASI_RIGHT_FD_FILESTAT_SET_SIZE
    | __WASI_RIGHT_FD_FILESTAT_SET_TIMES
    | __WASI_RIGHT_PATH_SYMLINK
    | __WASI_RIGHT_PATH_UNLINK_FILE
    | __WASI_RIGHT_PATH_REMOVE_DIRECTORY
    | __WASI_RIGHT_POLL_FD_READWRITE
    | __WASI_RIGHT_SOCK_SHUTDOWN;
//    | __WASI_RIGHT_SOCK_CONNECT;

// Block and character device interaction is outside the scope of
// WASI. Simply allow everything.
pub(crate) const RIGHTS_BLOCK_DEVICE_BASE: __wasi_rights_t = RIGHTS_ALL;
pub(crate) const RIGHTS_BLOCK_DEVICE_INHERITING: __wasi_rights_t = RIGHTS_ALL;
pub(crate) const RIGHTS_CHARACTER_DEVICE_BASE: __wasi_rights_t = RIGHTS_ALL;
pub(crate) const RIGHTS_CHARACTER_DEVICE_INHERITING: __wasi_rights_t = RIGHTS_ALL;

// Only allow directory operations on directories. Directories can only
// yield file descriptors to other directories and files.
pub(crate) const RIGHTS_DIRECTORY_BASE: __wasi_rights_t = __WASI_RIGHT_FD_FDSTAT_SET_FLAGS
    | __WASI_RIGHT_FD_SYNC
    | __WASI_RIGHT_FD_ADVISE
    | __WASI_RIGHT_PATH_CREATE_DIRECTORY
    | __WASI_RIGHT_PATH_CREATE_FILE
    | __WASI_RIGHT_PATH_LINK_SOURCE
    | __WASI_RIGHT_PATH_LINK_TARGET
    | __WASI_RIGHT_PATH_OPEN
    | __WASI_RIGHT_FD_READDIR
    | __WASI_RIGHT_PATH_READLINK
    | __WASI_RIGHT_PATH_RENAME_SOURCE
    | __WASI_RIGHT_PATH_RENAME_TARGET
    | __WASI_RIGHT_PATH_FILESTAT_GET
    | __WASI_RIGHT_PATH_FILESTAT_SET_SIZE
    | __WASI_RIGHT_PATH_FILESTAT_SET_TIMES
    | __WASI_RIGHT_FD_FILESTAT_GET
    | __WASI_RIGHT_FD_FILESTAT_SET_TIMES
    | __WASI_RIGHT_PATH_SYMLINK
    | __WASI_RIGHT_PATH_UNLINK_FILE
    | __WASI_RIGHT_PATH_REMOVE_DIRECTORY
    | __WASI_RIGHT_POLL_FD_READWRITE;
pub(crate) const RIGHTS_DIRECTORY_INHERITING: __wasi_rights_t =
    RIGHTS_DIRECTORY_BASE | RIGHTS_REGULAR_FILE_BASE;

// Operations that apply to regular files.
pub(crate) const RIGHTS_REGULAR_FILE_BASE: __wasi_rights_t = __WASI_RIGHT_FD_DATASYNC
    | __WASI_RIGHT_FD_READ
    | __WASI_RIGHT_FD_SEEK
    | __WASI_RIGHT_FD_FDSTAT_SET_FLAGS
    | __WASI_RIGHT_FD_SYNC
    | __WASI_RIGHT_FD_TELL
    | __WASI_RIGHT_FD_WRITE
    | __WASI_RIGHT_FD_ADVISE
    | __WASI_RIGHT_FD_ALLOCATE
    | __WASI_RIGHT_FD_FILESTAT_GET
    | __WASI_RIGHT_FD_FILESTAT_SET_SIZE
    | __WASI_RIGHT_FD_FILESTAT_SET_TIMES
    | __WASI_RIGHT_POLL_FD_READWRITE;
pub(crate) const RIGHTS_REGULAR_FILE_INHERITING: __wasi_rights_t = 0;

// Operations that apply to sockets and socket pairs.
pub(crate) const RIGHTS_SOCKET_BASE: __wasi_rights_t = __WASI_RIGHT_FD_READ
    | __WASI_RIGHT_FD_FDSTAT_SET_FLAGS
    | __WASI_RIGHT_FD_WRITE
    | __WASI_RIGHT_FD_FILESTAT_GET
    | __WASI_RIGHT_POLL_FD_READWRITE
    | __WASI_RIGHT_SOCK_SHUTDOWN;
//    | __WASI_RIGHT_SOCK_CONNECT;
pub(crate) const RIGHTS_SOCKET_INHERITING: __wasi_rights_t = RIGHTS_ALL;

// Operations that apply to TTYs.
pub(crate) const RIGHTS_TTY_BASE: __wasi_rights_t = __WASI_RIGHT_FD_READ
    | __WASI_RIGHT_FD_FDSTAT_SET_FLAGS
    | __WASI_RIGHT_FD_WRITE
    | __WASI_RIGHT_FD_FILESTAT_GET
    | __WASI_RIGHT_POLL_FD_READWRITE;
#[allow(unused)]
pub(crate) const RIGHTS_TTY_INHERITING: __wasi_rights_t = 0;

pub fn strerror(errno: __wasi_errno_t) -> &'static str {
    match errno {
        __WASI_ESUCCESS => "__WASI_ESUCCESS",
        __WASI_E2BIG => "__WASI_E2BIG",
        __WASI_EACCES => "__WASI_EACCES",
        __WASI_EADDRINUSE => "__WASI_EADDRINUSE",
        __WASI_EADDRNOTAVAIL => "__WASI_EADDRNOTAVAIL",
        __WASI_EAFNOSUPPORT => "__WASI_EAFNOSUPPORT",
        __WASI_EAGAIN => "__WASI_EAGAIN",
        __WASI_EALREADY => "__WASI_EALREADY",
        __WASI_EBADF => "__WASI_EBADF",
        __WASI_EBADMSG => "__WASI_EBADMSG",
        __WASI_EBUSY => "__WASI_EBUSY",
        __WASI_ECANCELED => "__WASI_ECANCELED",
        __WASI_ECHILD => "__WASI_ECHILD",
        __WASI_ECONNABORTED => "__WASI_ECONNABORTED",
        __WASI_ECONNREFUSED => "__WASI_ECONNREFUSED",
        __WASI_ECONNRESET => "__WASI_ECONNRESET",
        __WASI_EDEADLK => "__WASI_EDEADLK",
        __WASI_EDESTADDRREQ => "__WASI_EDESTADDRREQ",
        __WASI_EDOM => "__WASI_EDOM",
        __WASI_EDQUOT => "__WASI_EDQUOT",
        __WASI_EEXIST => "__WASI_EEXIST",
        __WASI_EFAULT => "__WASI_EFAULT",
        __WASI_EFBIG => "__WASI_EFBIG",
        __WASI_EHOSTUNREACH => "__WASI_EHOSTUNREACH",
        __WASI_EIDRM => "__WASI_EIDRM",
        __WASI_EILSEQ => "__WASI_EILSEQ",
        __WASI_EINPROGRESS => "__WASI_EINPROGRESS",
        __WASI_EINTR => "__WASI_EINTR",
        __WASI_EINVAL => "__WASI_EINVAL",
        __WASI_EIO => "__WASI_EIO",
        __WASI_EISCONN => "__WASI_EISCONN",
        __WASI_EISDIR => "__WASI_EISDIR",
        __WASI_ELOOP => "__WASI_ELOOP",
        __WASI_EMFILE => "__WASI_EMFILE",
        __WASI_EMLINK => "__WASI_EMLINK",
        __WASI_EMSGSIZE => "__WASI_EMSGSIZE",
        __WASI_EMULTIHOP => "__WASI_EMULTIHOP",
        __WASI_ENAMETOOLONG => "__WASI_ENAMETOOLONG",
        __WASI_ENETDOWN => "__WASI_ENETDOWN",
        __WASI_ENETRESET => "__WASI_ENETRESET",
        __WASI_ENETUNREACH => "__WASI_ENETUNREACH",
        __WASI_ENFILE => "__WASI_ENFILE",
        __WASI_ENOBUFS => "__WASI_ENOBUFS",
        __WASI_ENODEV => "__WASI_ENODEV",
        __WASI_ENOENT => "__WASI_ENOENT",
        __WASI_ENOEXEC => "__WASI_ENOEXEC",
        __WASI_ENOLCK => "__WASI_ENOLCK",
        __WASI_ENOLINK => "__WASI_ENOLINK",
        __WASI_ENOMEM => "__WASI_ENOMEM",
        __WASI_ENOMSG => "__WASI_ENOMSG",
        __WASI_ENOPROTOOPT => "__WASI_ENOPROTOOPT",
        __WASI_ENOSPC => "__WASI_ENOSPC",
        __WASI_ENOSYS => "__WASI_ENOSYS",
        __WASI_ENOTCONN => "__WASI_ENOTCONN",
        __WASI_ENOTDIR => "__WASI_ENOTDIR",
        __WASI_ENOTEMPTY => "__WASI_ENOTEMPTY",
        __WASI_ENOTRECOVERABLE => "__WASI_ENOTRECOVERABLE",
        __WASI_ENOTSOCK => "__WASI_ENOTSOCK",
        __WASI_ENOTSUP => "__WASI_ENOTSUP",
        __WASI_ENOTTY => "__WASI_ENOTTY",
        __WASI_ENXIO => "__WASI_ENXIO",
        __WASI_EOVERFLOW => "__WASI_EOVERFLOW",
        __WASI_EOWNERDEAD => "__WASI_EOWNERDEAD",
        __WASI_EPERM => "__WASI_EPERM",
        __WASI_EPIPE => "__WASI_EPIPE",
        __WASI_EPROTO => "__WASI_EPROTO",
        __WASI_EPROTONOSUPPORT => "__WASI_EPROTONOSUPPORT",
        __WASI_EPROTOTYPE => "__WASI_EPROTOTYPE",
        __WASI_ERANGE => "__WASI_ERANGE",
        __WASI_EROFS => "__WASI_EROFS",
        __WASI_ESPIPE => "__WASI_ESPIPE",
        __WASI_ESRCH => "__WASI_ESRCH",
        __WASI_ESTALE => "__WASI_ESTALE",
        __WASI_ETIMEDOUT => "__WASI_ETIMEDOUT",
        __WASI_ETXTBSY => "__WASI_ETXTBSY",
        __WASI_EXDEV => "__WASI_EXDEV",
        __WASI_ENOTCAPABLE => "__WASI_ENOTCAPABLE",
        other => panic!("Undefined errno value {:?}", other),
    }
}

pub fn whence_to_str(whence: __wasi_whence_t) -> &'static str {
    match whence {
        __WASI_WHENCE_CUR => "__WASI_WHENCE_CUR",
        __WASI_WHENCE_END => "__WASI_WHENCE_END",
        __WASI_WHENCE_SET => "__WASI_WHENCE_SET",
        other => panic!("Undefined whence value {:?}", other),
    }
}

pub const __WASI_DIRCOOKIE_START: __wasi_dircookie_t = 0;

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn bindgen_test_layout_wasi_dirent_t() {
        assert_eq!(
            ::std::mem::size_of::<__wasi_dirent_t>(),
            24usize,
            concat!("Size of: ", stringify!(__wasi_dirent_t))
        );
        assert_eq!(
            unsafe { &(*(::std::ptr::null::<__wasi_dirent_t>())).d_next as *const _ as usize },
            0usize,
            concat!(
                "Offset of field: ",
                stringify!(__wasi_dirent_t),
                "::",
                stringify!(d_next)
            )
        );
        assert_eq!(
            unsafe { &(*(::std::ptr::null::<__wasi_dirent_t>())).d_ino as *const _ as usize },
            8usize,
            concat!(
                "Offset of field: ",
                stringify!(__wasi_dirent_t),
                "::",
                stringify!(d_ino)
            )
        );
        assert_eq!(
            unsafe { &(*(::std::ptr::null::<__wasi_dirent_t>())).d_namlen as *const _ as usize },
            16usize,
            concat!(
                "Offset of field: ",
                stringify!(__wasi_dirent_t),
                "::",
                stringify!(d_namlen)
            )
        );
        assert_eq!(
            unsafe { &(*(::std::ptr::null::<__wasi_dirent_t>())).d_type as *const _ as usize },
            20usize,
            concat!(
                "Offset of field: ",
                stringify!(__wasi_dirent_t),
                "::",
                stringify!(d_type)
            )
        );
    }

    #[test]
    fn bindgen_test_layout___wasi_event_t___wasi_event_u___wasi_event_u_fd_readwrite_t() {
        assert_eq!(
            ::std::mem::size_of::<__wasi_event_fd_readwrite_t>(),
            16usize,
            concat!("Size of: ", stringify!(__wasi_event_fd_readwrite_t))
        );
        assert_eq!(
            ::std::mem::align_of::<__wasi_event_fd_readwrite_t>(),
            8usize,
            concat!("Alignment of ", stringify!(__wasi_event_fd_readwrite_t))
        );
        assert_eq!(
            unsafe {
                &(*(::std::ptr::null::<__wasi_event_fd_readwrite_t>())).nbytes as *const _ as usize
            },
            0usize,
            concat!(
                "Offset of field: ",
                stringify!(__wasi_event_fd_readwrite_t),
                "::",
                stringify!(nbytes)
            )
        );
        assert_eq!(
            unsafe {
                &(*(::std::ptr::null::<__wasi_event_fd_readwrite_t>())).flags as *const _ as usize
            },
            8usize,
            concat!(
                "Offset of field: ",
                stringify!(__wasi_event_fd_readwrite_t),
                "::",
                stringify!(flags)
            )
        );
    }

    #[test]
    fn bindgen_test_layout___wasi_event_t___wasi_event_u() {
        assert_eq!(
            ::std::mem::size_of::<__wasi_event_u>(),
            16usize,
            concat!("Size of: ", stringify!(__wasi_event_u))
        );
        assert_eq!(
            ::std::mem::align_of::<__wasi_event_u>(),
            8usize,
            concat!("Alignment of ", stringify!(__wasi_event_u))
        );
        assert_eq!(
            unsafe { &(*(::std::ptr::null::<__wasi_event_u>())).fd_readwrite as *const _ as usize },
            0usize,
            concat!(
                "Offset of field: ",
                stringify!(__wasi_event_u),
                "::",
                stringify!(fd_readwrite)
            )
        );
    }

    #[test]
    fn bindgen_test_layout___wasi_event_t() {
        assert_eq!(
            ::std::mem::size_of::<__wasi_event_t>(),
            32usize,
            concat!("Size of: ", stringify!(__wasi_event_t))
        );
        assert_eq!(
            ::std::mem::align_of::<__wasi_event_t>(),
            8usize,
            concat!("Alignment of ", stringify!(__wasi_event_t))
        );
        assert_eq!(
            unsafe { &(*(::std::ptr::null::<__wasi_event_t>())).userdata as *const _ as usize },
            0usize,
            concat!(
                "Offset of field: ",
                stringify!(__wasi_event_t),
                "::",
                stringify!(userdata)
            )
        );
        assert_eq!(
            unsafe { &(*(::std::ptr::null::<__wasi_event_t>())).error as *const _ as usize },
            8usize,
            concat!(
                "Offset of field: ",
                stringify!(__wasi_event_t),
                "::",
                stringify!(error)
            )
        );
        assert_eq!(
            unsafe { &(*(::std::ptr::null::<__wasi_event_t>())).r#type as *const _ as usize },
            10usize,
            concat!(
                "Offset of field: ",
                stringify!(__wasi_event_t),
                "::",
                stringify!(r#type)
            )
        );
        assert_eq!(
            unsafe { &(*(::std::ptr::null::<__wasi_event_t>())).u as *const _ as usize },
            16usize,
            concat!(
                "Offset of field: ",
                stringify!(__wasi_event_t),
                "::",
                stringify!(u)
            )
        );
    }

    #[test]
    fn bindgen_test_layout_wasi_event_t() {
        assert_eq!(
            ::std::mem::size_of::<__wasi_event_t>(),
            32usize,
            concat!("Size of: ", stringify!(__wasi_event_t))
        );
        assert_eq!(
            unsafe { &(*(::std::ptr::null::<__wasi_event_t>())).userdata as *const _ as usize },
            0usize,
            concat!(
                "Offset of field: ",
                stringify!(__wasi_event_t),
                "::",
                stringify!(userdata)
            )
        );
        assert_eq!(
            unsafe { &(*(::std::ptr::null::<__wasi_event_t>())).error as *const _ as usize },
            8usize,
            concat!(
                "Offset of field: ",
                stringify!(__wasi_event_t),
                "::",
                stringify!(error)
            )
        );
        assert_eq!(
            unsafe { &(*(::std::ptr::null::<__wasi_event_t>())).r#type as *const _ as usize },
            10usize,
            concat!(
                "Offset of field: ",
                stringify!(__wasi_event_t),
                "::",
                stringify!(r#type)
            )
        );
    }

    #[test]
    fn bindgen_test_layout_wasi_fdstat_t() {
        assert_eq!(
            ::std::mem::size_of::<__wasi_fdstat_t>(),
            24usize,
            concat!("Size of: ", stringify!(__wasi_fdstat_t))
        );
        assert_eq!(
            unsafe { &(*(::std::ptr::null::<__wasi_fdstat_t>())).fs_filetype as *const _ as usize },
            0usize,
            concat!(
                "Offset of field: ",
                stringify!(__wasi_fdstat_t),
                "::",
                stringify!(fs_filetype)
            )
        );
        assert_eq!(
            unsafe { &(*(::std::ptr::null::<__wasi_fdstat_t>())).fs_flags as *const _ as usize },
            2usize,
            concat!(
                "Offset of field: ",
                stringify!(__wasi_fdstat_t),
                "::",
                stringify!(fs_flags)
            )
        );
        assert_eq!(
            unsafe {
                &(*(::std::ptr::null::<__wasi_fdstat_t>())).fs_rights_base as *const _ as usize
            },
            8usize,
            concat!(
                "Offset of field: ",
                stringify!(__wasi_fdstat_t),
                "::",
                stringify!(fs_rights_base)
            )
        );
        assert_eq!(
            unsafe {
                &(*(::std::ptr::null::<__wasi_fdstat_t>())).fs_rights_inheriting as *const _
                    as usize
            },
            16usize,
            concat!(
                "Offset of field: ",
                stringify!(__wasi_fdstat_t),
                "::",
                stringify!(fs_rights_inheriting)
            )
        );
    }

    #[test]
    fn bindgen_test_layout_wasi_filestat_t() {
        assert_eq!(
            ::std::mem::size_of::<__wasi_filestat_t>(),
            56usize,
            concat!("Size of: ", stringify!(__wasi_filestat_t))
        );
        assert_eq!(
            unsafe { &(*(::std::ptr::null::<__wasi_filestat_t>())).st_dev as *const _ as usize },
            0usize,
            concat!(
                "Offset of field: ",
                stringify!(__wasi_filestat_t),
                "::",
                stringify!(st_dev)
            )
        );
        assert_eq!(
            unsafe { &(*(::std::ptr::null::<__wasi_filestat_t>())).st_ino as *const _ as usize },
            8usize,
            concat!(
                "Offset of field: ",
                stringify!(__wasi_filestat_t),
                "::",
                stringify!(st_ino)
            )
        );
        assert_eq!(
            unsafe {
                &(*(::std::ptr::null::<__wasi_filestat_t>())).st_filetype as *const _ as usize
            },
            16usize,
            concat!(
                "Offset of field: ",
                stringify!(__wasi_filestat_t),
                "::",
                stringify!(st_filetype)
            )
        );
        assert_eq!(
            unsafe { &(*(::std::ptr::null::<__wasi_filestat_t>())).st_nlink as *const _ as usize },
            20usize,
            concat!(
                "Offset of field: ",
                stringify!(__wasi_filestat_t),
                "::",
                stringify!(st_nlink)
            )
        );
        assert_eq!(
            unsafe { &(*(::std::ptr::null::<__wasi_filestat_t>())).st_size as *const _ as usize },
            24usize,
            concat!(
                "Offset of field: ",
                stringify!(__wasi_filestat_t),
                "::",
                stringify!(st_size)
            )
        );
        assert_eq!(
            unsafe { &(*(::std::ptr::null::<__wasi_filestat_t>())).st_atim as *const _ as usize },
            32usize,
            concat!(
                "Offset of field: ",
                stringify!(__wasi_filestat_t),
                "::",
                stringify!(st_atim)
            )
        );
        assert_eq!(
            unsafe { &(*(::std::ptr::null::<__wasi_filestat_t>())).st_mtim as *const _ as usize },
            40usize,
            concat!(
                "Offset of field: ",
                stringify!(__wasi_filestat_t),
                "::",
                stringify!(st_mtim)
            )
        );
        assert_eq!(
            unsafe { &(*(::std::ptr::null::<__wasi_filestat_t>())).st_ctim as *const _ as usize },
            48usize,
            concat!(
                "Offset of field: ",
                stringify!(__wasi_filestat_t),
                "::",
                stringify!(st_ctim)
            )
        );
    }

    #[test]
    fn bindgen_test_layout___wasi_subscription_clock_t() {
        assert_eq!(
            ::std::mem::size_of::<__wasi_subscription_clock_t>(),
            40usize,
            concat!("Size of: ", stringify!(__wasi_subscription_clock_t))
        );
        assert_eq!(
            ::std::mem::align_of::<__wasi_subscription_clock_t>(),
            8usize,
            concat!("Alignment of ", stringify!(__wasi_subscription_clock_t))
        );
        assert_eq!(
            unsafe {
                &(*(::std::ptr::null::<__wasi_subscription_clock_t>())).identifier as *const _
                    as usize
            },
            0usize,
            concat!(
                "Offset of field: ",
                stringify!(__wasi_subscription_clock_t),
                "::",
                stringify!(identifier)
            )
        );
        assert_eq!(
            unsafe {
                &(*(::std::ptr::null::<__wasi_subscription_clock_t>())).clock_id as *const _
                    as usize
            },
            8usize,
            concat!(
                "Offset of field: ",
                stringify!(__wasi_subscription_clock_t),
                "::",
                stringify!(clock_id)
            )
        );
        assert_eq!(
            unsafe {
                &(*(::std::ptr::null::<__wasi_subscription_clock_t>())).timeout as *const _ as usize
            },
            16usize,
            concat!(
                "Offset of field: ",
                stringify!(__wasi_subscription_clock_t),
                "::",
                stringify!(timeout)
            )
        );
        assert_eq!(
            unsafe {
                &(*(::std::ptr::null::<__wasi_subscription_clock_t>())).precision as *const _
                    as usize
            },
            24usize,
            concat!(
                "Offset of field: ",
                stringify!(__wasi_subscription_clock_t),
                "::",
                stringify!(precision)
            )
        );
        assert_eq!(
            unsafe {
                &(*(::std::ptr::null::<__wasi_subscription_clock_t>())).flags as *const _ as usize
            },
            32usize,
            concat!(
                "Offset of field: ",
                stringify!(__wasi_subscription_clock_t),
                "::",
                stringify!(flags)
            )
        );
    }

    #[test]
    fn bindgen_test_layout___wasi_subscription_t___wasi_subscription_u___wasi_subscription_u_fd_readwrite_t(
    ) {
        assert_eq!(
            ::std::mem::size_of::<__wasi_subscription_fd_readwrite_t>(),
            4usize,
            concat!("Size of: ", stringify!(__wasi_subscription_fd_readwrite_t))
        );
        assert_eq!(
            ::std::mem::align_of::<__wasi_subscription_fd_readwrite_t>(),
            4usize,
            concat!(
                "Alignment of ",
                stringify!(__wasi_subscription_fd_readwrite_t)
            )
        );
        assert_eq!(
            unsafe {
                &(*(::std::ptr::null::<__wasi_subscription_fd_readwrite_t>())).file_descriptor
                    as *const _ as usize
            },
            0usize,
            concat!(
                "Offset of field: ",
                stringify!(__wasi_subscription_fd_readwrite_t),
                "::",
                stringify!(fd)
            )
        );
    }

    #[test]
    fn bindgen_test_layout___wasi_subscription_t___wasi_subscription_u() {
        assert_eq!(
            ::std::mem::size_of::<__wasi_subscription_u>(),
            40usize,
            concat!("Size of: ", stringify!(__wasi_subscription_u))
        );
        assert_eq!(
            ::std::mem::align_of::<__wasi_subscription_u>(),
            8usize,
            concat!("Alignment of ", stringify!(__wasi_subscription_u))
        );
        assert_eq!(
            unsafe { &(*(::std::ptr::null::<__wasi_subscription_u>())).clock as *const _ as usize },
            0usize,
            concat!(
                "Offset of field: ",
                stringify!(__wasi_subscription_u),
                "::",
                stringify!(clock)
            )
        );
        assert_eq!(
            unsafe {
                &(*(::std::ptr::null::<__wasi_subscription_u>())).fd_readwrite as *const _ as usize
            },
            0usize,
            concat!(
                "Offset of field: ",
                stringify!(__wasi_subscription_u),
                "::",
                stringify!(fd_readwrite)
            )
        );
    }

    #[test]
    fn bindgen_test_layout___wasi_subscription_t() {
        assert_eq!(
            ::std::mem::size_of::<__wasi_subscription_t>(),
            56usize,
            concat!("Size of: ", stringify!(__wasi_subscription_t))
        );
        assert_eq!(
            ::std::mem::align_of::<__wasi_subscription_t>(),
            8usize,
            concat!("Alignment of ", stringify!(__wasi_subscription_t))
        );
        assert_eq!(
            unsafe {
                &(*(::std::ptr::null::<__wasi_subscription_t>())).userdata as *const _ as usize
            },
            0usize,
            concat!(
                "Offset of field: ",
                stringify!(__wasi_subscription_t),
                "::",
                stringify!(userdata)
            )
        );
        assert_eq!(
            unsafe {
                &(*(::std::ptr::null::<__wasi_subscription_t>())).r#type as *const _ as usize
            },
            8usize,
            concat!(
                "Offset of field: ",
                stringify!(__wasi_subscription_t),
                "::",
                stringify!(r#type)
            )
        );
        assert_eq!(
            unsafe { &(*(::std::ptr::null::<__wasi_subscription_t>())).u as *const _ as usize },
            16usize,
            concat!(
                "Offset of field: ",
                stringify!(__wasi_subscription_t),
                "::",
                stringify!(u)
            )
        );
    }

    #[test]
    fn bindgen_test_layout___wasi_filestat_t() {
        assert_eq!(
            ::std::mem::size_of::<__wasi_filestat_t>(),
            56usize,
            concat!("Size of: ", stringify!(__wasi_filestat_t))
        );
        assert_eq!(
            ::std::mem::align_of::<__wasi_filestat_t>(),
            8usize,
            concat!("Alignment of ", stringify!(__wasi_filestat_t))
        );
        assert_eq!(
            unsafe { &(*(::std::ptr::null::<__wasi_filestat_t>())).st_dev as *const _ as usize },
            0usize,
            concat!(
                "Offset of field: ",
                stringify!(__wasi_filestat_t),
                "::",
                stringify!(st_dev)
            )
        );
        assert_eq!(
            unsafe { &(*(::std::ptr::null::<__wasi_filestat_t>())).st_ino as *const _ as usize },
            8usize,
            concat!(
                "Offset of field: ",
                stringify!(__wasi_filestat_t),
                "::",
                stringify!(st_ino)
            )
        );
        assert_eq!(
            unsafe {
                &(*(::std::ptr::null::<__wasi_filestat_t>())).st_filetype as *const _ as usize
            },
            16usize,
            concat!(
                "Offset of field: ",
                stringify!(__wasi_filestat_t),
                "::",
                stringify!(st_filetype)
            )
        );
        assert_eq!(
            unsafe { &(*(::std::ptr::null::<__wasi_filestat_t>())).st_nlink as *const _ as usize },
            20usize,
            concat!(
                "Offset of field: ",
                stringify!(__wasi_filestat_t),
                "::",
                stringify!(st_nlink)
            )
        );
        assert_eq!(
            unsafe { &(*(::std::ptr::null::<__wasi_filestat_t>())).st_size as *const _ as usize },
            24usize,
            concat!(
                "Offset of field: ",
                stringify!(__wasi_filestat_t),
                "::",
                stringify!(st_size)
            )
        );
        assert_eq!(
            unsafe { &(*(::std::ptr::null::<__wasi_filestat_t>())).st_atim as *const _ as usize },
            32usize,
            concat!(
                "Offset of field: ",
                stringify!(__wasi_filestat_t),
                "::",
                stringify!(st_atim)
            )
        );
        assert_eq!(
            unsafe { &(*(::std::ptr::null::<__wasi_filestat_t>())).st_mtim as *const _ as usize },
            40usize,
            concat!(
                "Offset of field: ",
                stringify!(__wasi_filestat_t),
                "::",
                stringify!(st_mtim)
            )
        );
        assert_eq!(
            unsafe { &(*(::std::ptr::null::<__wasi_filestat_t>())).st_ctim as *const _ as usize },
            48usize,
            concat!(
                "Offset of field: ",
                stringify!(__wasi_filestat_t),
                "::",
                stringify!(st_ctim)
            )
        );
    }

    #[test]
    fn bindgen_test_layout___wasi_fdstat_t() {
        assert_eq!(
            ::std::mem::size_of::<__wasi_fdstat_t>(),
            24usize,
            concat!("Size of: ", stringify!(__wasi_fdstat_t))
        );
        assert_eq!(
            ::std::mem::align_of::<__wasi_fdstat_t>(),
            8usize,
            concat!("Alignment of ", stringify!(__wasi_fdstat_t))
        );
        assert_eq!(
            unsafe { &(*(::std::ptr::null::<__wasi_fdstat_t>())).fs_filetype as *const _ as usize },
            0usize,
            concat!(
                "Offset of field: ",
                stringify!(__wasi_fdstat_t),
                "::",
                stringify!(fs_filetype)
            )
        );
        assert_eq!(
            unsafe { &(*(::std::ptr::null::<__wasi_fdstat_t>())).fs_flags as *const _ as usize },
            2usize,
            concat!(
                "Offset of field: ",
                stringify!(__wasi_fdstat_t),
                "::",
                stringify!(fs_flags)
            )
        );
        assert_eq!(
            unsafe {
                &(*(::std::ptr::null::<__wasi_fdstat_t>())).fs_rights_base as *const _ as usize
            },
            8usize,
            concat!(
                "Offset of field: ",
                stringify!(__wasi_fdstat_t),
                "::",
                stringify!(fs_rights_base)
            )
        );
        assert_eq!(
            unsafe {
                &(*(::std::ptr::null::<__wasi_fdstat_t>())).fs_rights_inheriting as *const _
                    as usize
            },
            16usize,
            concat!(
                "Offset of field: ",
                stringify!(__wasi_fdstat_t),
                "::",
                stringify!(fs_rights_inheriting)
            )
        );
    }

    #[test]
    fn bindgen_test_layout___wasi_dirent_t() {
        assert_eq!(
            ::std::mem::size_of::<__wasi_dirent_t>(),
            24usize,
            concat!("Size of: ", stringify!(__wasi_dirent_t))
        );
        assert_eq!(
            ::std::mem::align_of::<__wasi_dirent_t>(),
            8usize,
            concat!("Alignment of ", stringify!(__wasi_dirent_t))
        );
        assert_eq!(
            unsafe { &(*(::std::ptr::null::<__wasi_dirent_t>())).d_next as *const _ as usize },
            0usize,
            concat!(
                "Offset of field: ",
                stringify!(__wasi_dirent_t),
                "::",
                stringify!(d_next)
            )
        );
        assert_eq!(
            unsafe { &(*(::std::ptr::null::<__wasi_dirent_t>())).d_ino as *const _ as usize },
            8usize,
            concat!(
                "Offset of field: ",
                stringify!(__wasi_dirent_t),
                "::",
                stringify!(d_ino)
            )
        );
        assert_eq!(
            unsafe { &(*(::std::ptr::null::<__wasi_dirent_t>())).d_namlen as *const _ as usize },
            16usize,
            concat!(
                "Offset of field: ",
                stringify!(__wasi_dirent_t),
                "::",
                stringify!(d_namlen)
            )
        );
        assert_eq!(
            unsafe { &(*(::std::ptr::null::<__wasi_dirent_t>())).d_type as *const _ as usize },
            20usize,
            concat!(
                "Offset of field: ",
                stringify!(__wasi_dirent_t),
                "::",
                stringify!(d_type)
            )
        );
    }
}
