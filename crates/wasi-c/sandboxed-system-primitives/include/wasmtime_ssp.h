/*
 * Part of the Wasmtime Project, under the Apache License v2.0 with LLVM Exceptions.
 * See https://github.com/bytecodealliance/wasmtime/blob/master/LICENSE for license information.
 *
 * This file declares an interface similar to WASI, but augmented to expose
 * some implementation details such as the curfds arguments that we pass
 * around to avoid storing them in TLS.
 */

#ifndef WASMTIME_SSP_H
#define WASMTIME_SSP_H

#include <stddef.h>
#include <stdint.h>

_Static_assert(_Alignof(int8_t) == 1, "non-wasi data layout");
_Static_assert(_Alignof(uint8_t) == 1, "non-wasi data layout");
_Static_assert(_Alignof(int16_t) == 2, "non-wasi data layout");
_Static_assert(_Alignof(uint16_t) == 2, "non-wasi data layout");
_Static_assert(_Alignof(int32_t) == 4, "non-wasi data layout");
_Static_assert(_Alignof(uint32_t) == 4, "non-wasi data layout");
_Static_assert(_Alignof(int64_t) == 8, "non-wasi data layout");
_Static_assert(_Alignof(uint64_t) == 8, "non-wasi data layout");

#ifdef __cplusplus
extern "C" {
#endif

typedef uint8_t __wasi_advice_t;
#define __WASI_ADVICE_NORMAL     (0)
#define __WASI_ADVICE_SEQUENTIAL (1)
#define __WASI_ADVICE_RANDOM     (2)
#define __WASI_ADVICE_WILLNEED   (3)
#define __WASI_ADVICE_DONTNEED   (4)
#define __WASI_ADVICE_NOREUSE    (5)

typedef uint32_t __wasi_clockid_t;
#define __WASI_CLOCK_REALTIME           (0)
#define __WASI_CLOCK_MONOTONIC          (1)
#define __WASI_CLOCK_PROCESS_CPUTIME_ID (2)
#define __WASI_CLOCK_THREAD_CPUTIME_ID  (3)

typedef uint64_t __wasi_device_t;

typedef uint64_t __wasi_dircookie_t;
#define __WASI_DIRCOOKIE_START (0)

typedef uint16_t __wasi_errno_t;
#define __WASI_ESUCCESS        (0)
#define __WASI_E2BIG           (1)
#define __WASI_EACCES          (2)
#define __WASI_EADDRINUSE      (3)
#define __WASI_EADDRNOTAVAIL   (4)
#define __WASI_EAFNOSUPPORT    (5)
#define __WASI_EAGAIN          (6)
#define __WASI_EALREADY        (7)
#define __WASI_EBADF           (8)
#define __WASI_EBADMSG         (9)
#define __WASI_EBUSY           (10)
#define __WASI_ECANCELED       (11)
#define __WASI_ECHILD          (12)
#define __WASI_ECONNABORTED    (13)
#define __WASI_ECONNREFUSED    (14)
#define __WASI_ECONNRESET      (15)
#define __WASI_EDEADLK         (16)
#define __WASI_EDESTADDRREQ    (17)
#define __WASI_EDOM            (18)
#define __WASI_EDQUOT          (19)
#define __WASI_EEXIST          (20)
#define __WASI_EFAULT          (21)
#define __WASI_EFBIG           (22)
#define __WASI_EHOSTUNREACH    (23)
#define __WASI_EIDRM           (24)
#define __WASI_EILSEQ          (25)
#define __WASI_EINPROGRESS     (26)
#define __WASI_EINTR           (27)
#define __WASI_EINVAL          (28)
#define __WASI_EIO             (29)
#define __WASI_EISCONN         (30)
#define __WASI_EISDIR          (31)
#define __WASI_ELOOP           (32)
#define __WASI_EMFILE          (33)
#define __WASI_EMLINK          (34)
#define __WASI_EMSGSIZE        (35)
#define __WASI_EMULTIHOP       (36)
#define __WASI_ENAMETOOLONG    (37)
#define __WASI_ENETDOWN        (38)
#define __WASI_ENETRESET       (39)
#define __WASI_ENETUNREACH     (40)
#define __WASI_ENFILE          (41)
#define __WASI_ENOBUFS         (42)
#define __WASI_ENODEV          (43)
#define __WASI_ENOENT          (44)
#define __WASI_ENOEXEC         (45)
#define __WASI_ENOLCK          (46)
#define __WASI_ENOLINK         (47)
#define __WASI_ENOMEM          (48)
#define __WASI_ENOMSG          (49)
#define __WASI_ENOPROTOOPT     (50)
#define __WASI_ENOSPC          (51)
#define __WASI_ENOSYS          (52)
#define __WASI_ENOTCONN        (53)
#define __WASI_ENOTDIR         (54)
#define __WASI_ENOTEMPTY       (55)
#define __WASI_ENOTRECOVERABLE (56)
#define __WASI_ENOTSOCK        (57)
#define __WASI_ENOTSUP         (58)
#define __WASI_ENOTTY          (59)
#define __WASI_ENXIO           (60)
#define __WASI_EOVERFLOW       (61)
#define __WASI_EOWNERDEAD      (62)
#define __WASI_EPERM           (63)
#define __WASI_EPIPE           (64)
#define __WASI_EPROTO          (65)
#define __WASI_EPROTONOSUPPORT (66)
#define __WASI_EPROTOTYPE      (67)
#define __WASI_ERANGE          (68)
#define __WASI_EROFS           (69)
#define __WASI_ESPIPE          (70)
#define __WASI_ESRCH           (71)
#define __WASI_ESTALE          (72)
#define __WASI_ETIMEDOUT       (73)
#define __WASI_ETXTBSY         (74)
#define __WASI_EXDEV           (75)
#define __WASI_ENOTCAPABLE     (76)

typedef uint16_t __wasi_eventrwflags_t;
#define __WASI_EVENT_FD_READWRITE_HANGUP (0x0001)

typedef uint8_t __wasi_eventtype_t;
#define __WASI_EVENTTYPE_CLOCK          (0)
#define __WASI_EVENTTYPE_FD_READ        (1)
#define __WASI_EVENTTYPE_FD_WRITE       (2)

typedef uint32_t __wasi_exitcode_t;

typedef uint32_t __wasi_fd_t;

typedef uint16_t __wasi_fdflags_t;
#define __WASI_FDFLAG_APPEND   (0x0001)
#define __WASI_FDFLAG_DSYNC    (0x0002)
#define __WASI_FDFLAG_NONBLOCK (0x0004)
#define __WASI_FDFLAG_RSYNC    (0x0008)
#define __WASI_FDFLAG_SYNC     (0x0010)

typedef int64_t __wasi_filedelta_t;

typedef uint64_t __wasi_filesize_t;

typedef uint8_t __wasi_filetype_t;
#define __WASI_FILETYPE_UNKNOWN          (0)
#define __WASI_FILETYPE_BLOCK_DEVICE     (1)
#define __WASI_FILETYPE_CHARACTER_DEVICE (2)
#define __WASI_FILETYPE_DIRECTORY        (3)
#define __WASI_FILETYPE_REGULAR_FILE     (4)
#define __WASI_FILETYPE_SOCKET_DGRAM     (5)
#define __WASI_FILETYPE_SOCKET_STREAM    (6)
#define __WASI_FILETYPE_SYMBOLIC_LINK    (7)

typedef uint16_t __wasi_fstflags_t;
#define __WASI_FILESTAT_SET_ATIM     (0x0001)
#define __WASI_FILESTAT_SET_ATIM_NOW (0x0002)
#define __WASI_FILESTAT_SET_MTIM     (0x0004)
#define __WASI_FILESTAT_SET_MTIM_NOW (0x0008)

typedef uint64_t __wasi_inode_t;

typedef uint32_t __wasi_linkcount_t;

typedef uint32_t __wasi_lookupflags_t;
#define __WASI_LOOKUP_SYMLINK_FOLLOW (0x00000001)

typedef uint16_t __wasi_oflags_t;
#define __WASI_O_CREAT     (0x0001)
#define __WASI_O_DIRECTORY (0x0002)
#define __WASI_O_EXCL      (0x0004)
#define __WASI_O_TRUNC     (0x0008)

typedef uint16_t __wasi_riflags_t;
#define __WASI_SOCK_RECV_PEEK    (0x0001)
#define __WASI_SOCK_RECV_WAITALL (0x0002)

typedef uint64_t __wasi_rights_t;
#define __WASI_RIGHT_FD_DATASYNC             (0x0000000000000001)
#define __WASI_RIGHT_FD_READ                 (0x0000000000000002)
#define __WASI_RIGHT_FD_SEEK                 (0x0000000000000004)
#define __WASI_RIGHT_FD_FDSTAT_SET_FLAGS     (0x0000000000000008)
#define __WASI_RIGHT_FD_SYNC                 (0x0000000000000010)
#define __WASI_RIGHT_FD_TELL                 (0x0000000000000020)
#define __WASI_RIGHT_FD_WRITE                (0x0000000000000040)
#define __WASI_RIGHT_FD_ADVISE               (0x0000000000000080)
#define __WASI_RIGHT_FD_ALLOCATE             (0x0000000000000100)
#define __WASI_RIGHT_PATH_CREATE_DIRECTORY   (0x0000000000000200)
#define __WASI_RIGHT_PATH_CREATE_FILE        (0x0000000000000400)
#define __WASI_RIGHT_PATH_LINK_SOURCE        (0x0000000000000800)
#define __WASI_RIGHT_PATH_LINK_TARGET        (0x0000000000001000)
#define __WASI_RIGHT_PATH_OPEN               (0x0000000000002000)
#define __WASI_RIGHT_FD_READDIR              (0x0000000000004000)
#define __WASI_RIGHT_PATH_READLINK           (0x0000000000008000)
#define __WASI_RIGHT_PATH_RENAME_SOURCE      (0x0000000000010000)
#define __WASI_RIGHT_PATH_RENAME_TARGET      (0x0000000000020000)
#define __WASI_RIGHT_PATH_FILESTAT_GET       (0x0000000000040000)
#define __WASI_RIGHT_PATH_FILESTAT_SET_SIZE  (0x0000000000080000)
#define __WASI_RIGHT_PATH_FILESTAT_SET_TIMES (0x0000000000100000)
#define __WASI_RIGHT_FD_FILESTAT_GET         (0x0000000000200000)
#define __WASI_RIGHT_FD_FILESTAT_SET_SIZE    (0x0000000000400000)
#define __WASI_RIGHT_FD_FILESTAT_SET_TIMES   (0x0000000000800000)
#define __WASI_RIGHT_PATH_SYMLINK            (0x0000000001000000)
#define __WASI_RIGHT_PATH_REMOVE_DIRECTORY   (0x0000000002000000)
#define __WASI_RIGHT_PATH_UNLINK_FILE        (0x0000000004000000)
#define __WASI_RIGHT_POLL_FD_READWRITE       (0x0000000008000000)
#define __WASI_RIGHT_SOCK_SHUTDOWN           (0x0000000010000000)
#define __WASI_RIGHT_SOCK_CONNECT            (0x0000000020000000)

typedef uint16_t __wasi_roflags_t;
#define __WASI_SOCK_RECV_DATA_TRUNCATED (0x0001)

typedef uint8_t __wasi_sdflags_t;
#define __WASI_SHUT_RD (0x01)
#define __WASI_SHUT_WR (0x02)

typedef uint16_t __wasi_siflags_t;

typedef uint8_t __wasi_signal_t;
// 0 is reserved; POSIX has special semantics for kill(pid, 0).
#define __WASI_SIGHUP    (1)
#define __WASI_SIGINT    (2)
#define __WASI_SIGQUIT   (3)
#define __WASI_SIGILL    (4)
#define __WASI_SIGTRAP   (5)
#define __WASI_SIGABRT   (6)
#define __WASI_SIGBUS    (7)
#define __WASI_SIGFPE    (8)
#define __WASI_SIGKILL   (9)
#define __WASI_SIGUSR1   (10)
#define __WASI_SIGSEGV   (11)
#define __WASI_SIGUSR2   (12)
#define __WASI_SIGPIPE   (13)
#define __WASI_SIGALRM   (14)
#define __WASI_SIGTERM   (15)
#define __WASI_SIGCHLD   (16)
#define __WASI_SIGCONT   (17)
#define __WASI_SIGSTOP   (18)
#define __WASI_SIGTSTP   (19)
#define __WASI_SIGTTIN   (20)
#define __WASI_SIGTTOU   (21)
#define __WASI_SIGURG    (22)
#define __WASI_SIGXCPU   (23)
#define __WASI_SIGXFSZ   (24)
#define __WASI_SIGVTALRM (25)
#define __WASI_SIGPROF   (26)
#define __WASI_SIGWINCH  (27)
#define __WASI_SIGPOLL   (28)
#define __WASI_SIGPWR    (29)
#define __WASI_SIGSYS    (30)

typedef uint16_t __wasi_subclockflags_t;
#define __WASI_SUBSCRIPTION_CLOCK_ABSTIME (0x0001)

typedef uint64_t __wasi_timestamp_t;

typedef uint64_t __wasi_userdata_t;

typedef uint8_t __wasi_whence_t;
#define __WASI_WHENCE_CUR (0)
#define __WASI_WHENCE_END (1)
#define __WASI_WHENCE_SET (2)

typedef uint8_t __wasi_preopentype_t;
#define __WASI_PREOPENTYPE_DIR              (0)

struct fd_table;
struct fd_prestats;
struct argv_environ_values;

typedef struct __wasi_dirent_t {
    __wasi_dircookie_t d_next;
    __wasi_inode_t d_ino;
    uint32_t d_namlen;
    __wasi_filetype_t d_type;
} __wasi_dirent_t;
_Static_assert(offsetof(__wasi_dirent_t, d_next) == 0, "non-wasi data layout");
_Static_assert(offsetof(__wasi_dirent_t, d_ino) == 8, "non-wasi data layout");
_Static_assert(offsetof(__wasi_dirent_t, d_namlen) == 16, "non-wasi data layout");
_Static_assert(offsetof(__wasi_dirent_t, d_type) == 20, "non-wasi data layout");
_Static_assert(sizeof(__wasi_dirent_t) == 24, "non-wasi data layout");
_Static_assert(_Alignof(__wasi_dirent_t) == 8, "non-wasi data layout");

typedef struct __wasi_event_t {
    __wasi_userdata_t userdata;
    __wasi_errno_t error;
    __wasi_eventtype_t type;
    union __wasi_event_u {
        struct __wasi_event_u_fd_readwrite_t {
            __wasi_filesize_t nbytes;
            __wasi_eventrwflags_t flags;
        } fd_readwrite;
    } u;
} __wasi_event_t;
_Static_assert(offsetof(__wasi_event_t, userdata) == 0, "non-wasi data layout");
_Static_assert(offsetof(__wasi_event_t, error) == 8, "non-wasi data layout");
_Static_assert(offsetof(__wasi_event_t, type) == 10, "non-wasi data layout");
_Static_assert(
    offsetof(__wasi_event_t, u.fd_readwrite.nbytes) == 16, "non-wasi data layout");
_Static_assert(
    offsetof(__wasi_event_t, u.fd_readwrite.flags) == 24, "non-wasi data layout");
_Static_assert(sizeof(__wasi_event_t) == 32, "non-wasi data layout");
_Static_assert(_Alignof(__wasi_event_t) == 8, "non-wasi data layout");

typedef struct __wasi_prestat_t {
    __wasi_preopentype_t pr_type;
    union __wasi_prestat_u {
        struct __wasi_prestat_u_dir_t {
            size_t pr_name_len;
        } dir;
    } u;
} __wasi_prestat_t;
_Static_assert(offsetof(__wasi_prestat_t, pr_type) == 0, "non-wasi data layout");
_Static_assert(sizeof(void *) != 4 ||
    offsetof(__wasi_prestat_t, u.dir.pr_name_len) == 4, "non-wasi data layout");
_Static_assert(sizeof(void *) != 8 ||
    offsetof(__wasi_prestat_t, u.dir.pr_name_len) == 8, "non-wasi data layout");
_Static_assert(sizeof(void *) != 4 ||
    sizeof(__wasi_prestat_t) == 8, "non-wasi data layout");
_Static_assert(sizeof(void *) != 8 ||
    sizeof(__wasi_prestat_t) == 16, "non-wasi data layout");
_Static_assert(sizeof(void *) != 4 ||
    _Alignof(__wasi_prestat_t) == 4, "non-wasi data layout");
_Static_assert(sizeof(void *) != 8 ||
    _Alignof(__wasi_prestat_t) == 8, "non-wasi data layout");

typedef struct __wasi_fdstat_t {
    __wasi_filetype_t fs_filetype;
    __wasi_fdflags_t fs_flags;
    __wasi_rights_t fs_rights_base;
    __wasi_rights_t fs_rights_inheriting;
} __wasi_fdstat_t;
_Static_assert(
    offsetof(__wasi_fdstat_t, fs_filetype) == 0, "non-wasi data layout");
_Static_assert(offsetof(__wasi_fdstat_t, fs_flags) == 2, "non-wasi data layout");
_Static_assert(
    offsetof(__wasi_fdstat_t, fs_rights_base) == 8, "non-wasi data layout");
_Static_assert(
    offsetof(__wasi_fdstat_t, fs_rights_inheriting) == 16,
    "non-wasi data layout");
_Static_assert(sizeof(__wasi_fdstat_t) == 24, "non-wasi data layout");
_Static_assert(_Alignof(__wasi_fdstat_t) == 8, "non-wasi data layout");

typedef struct __wasi_filestat_t {
    __wasi_device_t st_dev;
    __wasi_inode_t st_ino;
    __wasi_filetype_t st_filetype;
    __wasi_linkcount_t st_nlink;
    __wasi_filesize_t st_size;
    __wasi_timestamp_t st_atim;
    __wasi_timestamp_t st_mtim;
    __wasi_timestamp_t st_ctim;
} __wasi_filestat_t;
_Static_assert(offsetof(__wasi_filestat_t, st_dev) == 0, "non-wasi data layout");
_Static_assert(offsetof(__wasi_filestat_t, st_ino) == 8, "non-wasi data layout");
_Static_assert(
    offsetof(__wasi_filestat_t, st_filetype) == 16, "non-wasi data layout");
_Static_assert(
    offsetof(__wasi_filestat_t, st_nlink) == 20, "non-wasi data layout");
_Static_assert(
    offsetof(__wasi_filestat_t, st_size) == 24, "non-wasi data layout");
_Static_assert(
    offsetof(__wasi_filestat_t, st_atim) == 32, "non-wasi data layout");
_Static_assert(
    offsetof(__wasi_filestat_t, st_mtim) == 40, "non-wasi data layout");
_Static_assert(
    offsetof(__wasi_filestat_t, st_ctim) == 48, "non-wasi data layout");
_Static_assert(sizeof(__wasi_filestat_t) == 56, "non-wasi data layout");
_Static_assert(_Alignof(__wasi_filestat_t) == 8, "non-wasi data layout");

typedef struct __wasi_ciovec_t {
    const void *buf;
    size_t buf_len;
} __wasi_ciovec_t;
_Static_assert(offsetof(__wasi_ciovec_t, buf) == 0, "non-wasi data layout");
_Static_assert(sizeof(void *) != 4 ||
    offsetof(__wasi_ciovec_t, buf_len) == 4, "non-wasi data layout");
_Static_assert(sizeof(void *) != 8 ||
    offsetof(__wasi_ciovec_t, buf_len) == 8, "non-wasi data layout");
_Static_assert(sizeof(void *) != 4 ||
    sizeof(__wasi_ciovec_t) == 8, "non-wasi data layout");
_Static_assert(sizeof(void *) != 8 ||
    sizeof(__wasi_ciovec_t) == 16, "non-wasi data layout");
_Static_assert(sizeof(void *) != 4 ||
    _Alignof(__wasi_ciovec_t) == 4, "non-wasi data layout");
_Static_assert(sizeof(void *) != 8 ||
    _Alignof(__wasi_ciovec_t) == 8, "non-wasi data layout");

typedef struct __wasi_iovec_t {
    void *buf;
    size_t buf_len;
} __wasi_iovec_t;
_Static_assert(offsetof(__wasi_iovec_t, buf) == 0, "non-wasi data layout");
_Static_assert(sizeof(void *) != 4 ||
    offsetof(__wasi_iovec_t, buf_len) == 4, "non-wasi data layout");
_Static_assert(sizeof(void *) != 8 ||
    offsetof(__wasi_iovec_t, buf_len) == 8, "non-wasi data layout");
_Static_assert(sizeof(void *) != 4 ||
    sizeof(__wasi_iovec_t) == 8, "non-wasi data layout");
_Static_assert(sizeof(void *) != 8 ||
    sizeof(__wasi_iovec_t) == 16, "non-wasi data layout");
_Static_assert(sizeof(void *) != 4 ||
    _Alignof(__wasi_iovec_t) == 4, "non-wasi data layout");
_Static_assert(sizeof(void *) != 8 ||
    _Alignof(__wasi_iovec_t) == 8, "non-wasi data layout");

typedef struct __wasi_subscription_t {
    __wasi_userdata_t userdata;
    __wasi_eventtype_t type;
    union __wasi_subscription_u {
        struct __wasi_subscription_u_clock_t {
            __wasi_userdata_t identifier;
            __wasi_clockid_t clock_id;
            __wasi_timestamp_t timeout;
            __wasi_timestamp_t precision;
            __wasi_subclockflags_t flags;
        } clock;
        struct __wasi_subscription_u_fd_readwrite_t {
            __wasi_fd_t fd;
        } fd_readwrite;
    } u;
} __wasi_subscription_t;
_Static_assert(
    offsetof(__wasi_subscription_t, userdata) == 0, "non-wasi data layout");
_Static_assert(
    offsetof(__wasi_subscription_t, type) == 8, "non-wasi data layout");
_Static_assert(
    offsetof(__wasi_subscription_t, u.clock.identifier) == 16,
    "non-wasi data layout");
_Static_assert(
    offsetof(__wasi_subscription_t, u.clock.clock_id) == 24,
    "non-wasi data layout");
_Static_assert(
    offsetof(__wasi_subscription_t, u.clock.timeout) == 32, "non-wasi data layout");
_Static_assert(
    offsetof(__wasi_subscription_t, u.clock.precision) == 40,
    "non-wasi data layout");
_Static_assert(
    offsetof(__wasi_subscription_t, u.clock.flags) == 48, "non-wasi data layout");
_Static_assert(
    offsetof(__wasi_subscription_t, u.fd_readwrite.fd) == 16,
    "non-wasi data layout");
_Static_assert(sizeof(__wasi_subscription_t) == 56, "non-wasi data layout");
_Static_assert(_Alignof(__wasi_subscription_t) == 8, "non-wasi data layout");

#if defined(WASMTIME_SSP_WASI_API)
#define WASMTIME_SSP_SYSCALL_NAME(name) \
    asm("__wasi_" #name)
#else
#define WASMTIME_SSP_SYSCALL_NAME(name)
#endif

__wasi_errno_t wasmtime_ssp_args_get(
#if !defined(WASMTIME_SSP_STATIC_CURFDS)
    struct argv_environ_values *arg_environ,
#endif
    char **argv,
    char *argv_buf
) WASMTIME_SSP_SYSCALL_NAME(args_get) __attribute__((__warn_unused_result__));

__wasi_errno_t wasmtime_ssp_args_sizes_get(
#if !defined(WASMTIME_SSP_STATIC_CURFDS)
    struct argv_environ_values *arg_environ,
#endif
    size_t *argc,
    size_t *argv_buf_size
) WASMTIME_SSP_SYSCALL_NAME(args_sizes_get) __attribute__((__warn_unused_result__));

__wasi_errno_t wasmtime_ssp_clock_res_get(
    __wasi_clockid_t clock_id,
    __wasi_timestamp_t *resolution
) WASMTIME_SSP_SYSCALL_NAME(clock_res_get) __attribute__((__warn_unused_result__));

__wasi_errno_t wasmtime_ssp_clock_time_get(
    __wasi_clockid_t clock_id,
    __wasi_timestamp_t precision,
    __wasi_timestamp_t *time
) WASMTIME_SSP_SYSCALL_NAME(clock_time_get) __attribute__((__warn_unused_result__));

__wasi_errno_t wasmtime_ssp_environ_get(
#if !defined(WASMTIME_SSP_STATIC_CURFDS)
    struct argv_environ_values *arg_environ,
#endif
    char **environ,
    char *environ_buf
) WASMTIME_SSP_SYSCALL_NAME(environ_get) __attribute__((__warn_unused_result__));

__wasi_errno_t wasmtime_ssp_environ_sizes_get(
#if !defined(WASMTIME_SSP_STATIC_CURFDS)
    struct argv_environ_values *arg_environ,
#endif
    size_t *environ_count,
    size_t *environ_buf_size
) WASMTIME_SSP_SYSCALL_NAME(environ_sizes_get) __attribute__((__warn_unused_result__));

__wasi_errno_t wasmtime_ssp_fd_prestat_get(
#if !defined(WASMTIME_SSP_STATIC_CURFDS)
    struct fd_prestats *prestats,
#endif
    __wasi_fd_t fd,
    __wasi_prestat_t *buf
) WASMTIME_SSP_SYSCALL_NAME(fd_prestat_get) __attribute__((__warn_unused_result__));

__wasi_errno_t wasmtime_ssp_fd_prestat_dir_name(
#if !defined(WASMTIME_SSP_STATIC_CURFDS)
    struct fd_prestats *prestats,
#endif
    __wasi_fd_t fd,
    char *path,
    size_t path_len
) WASMTIME_SSP_SYSCALL_NAME(fd_prestat_dir_name) __attribute__((__warn_unused_result__));

__wasi_errno_t wasmtime_ssp_fd_close(
#if !defined(WASMTIME_SSP_STATIC_CURFDS)
    struct fd_table *curfds,
    struct fd_prestats *prestats,
#endif
    __wasi_fd_t fd
) WASMTIME_SSP_SYSCALL_NAME(fd_close) __attribute__((__warn_unused_result__));

__wasi_errno_t wasmtime_ssp_fd_datasync(
#if !defined(WASMTIME_SSP_STATIC_CURFDS)
    struct fd_table *curfds,
#endif
    __wasi_fd_t fd
) WASMTIME_SSP_SYSCALL_NAME(fd_datasync) __attribute__((__warn_unused_result__));

__wasi_errno_t wasmtime_ssp_fd_pread(
#if !defined(WASMTIME_SSP_STATIC_CURFDS)
    struct fd_table *curfds,
#endif
    __wasi_fd_t fd,
    const __wasi_iovec_t *iovs,
    size_t iovs_len,
    __wasi_filesize_t offset,
    size_t *nread
) WASMTIME_SSP_SYSCALL_NAME(fd_pread) __attribute__((__warn_unused_result__));

__wasi_errno_t wasmtime_ssp_fd_pwrite(
#if !defined(WASMTIME_SSP_STATIC_CURFDS)
    struct fd_table *curfds,
#endif
    __wasi_fd_t fd,
    const __wasi_ciovec_t *iovs,
    size_t iovs_len,
    __wasi_filesize_t offset,
    size_t *nwritten
) WASMTIME_SSP_SYSCALL_NAME(fd_pwrite) __attribute__((__warn_unused_result__));

__wasi_errno_t wasmtime_ssp_fd_read(
#if !defined(WASMTIME_SSP_STATIC_CURFDS)
    struct fd_table *curfds,
#endif
    __wasi_fd_t fd,
    const __wasi_iovec_t *iovs,
    size_t iovs_len,
    size_t *nread
) WASMTIME_SSP_SYSCALL_NAME(fd_read) __attribute__((__warn_unused_result__));

__wasi_errno_t wasmtime_ssp_fd_renumber(
#if !defined(WASMTIME_SSP_STATIC_CURFDS)
    struct fd_table *curfds,
    struct fd_prestats *prestats,
#endif
    __wasi_fd_t from,
    __wasi_fd_t to
) WASMTIME_SSP_SYSCALL_NAME(fd_renumber) __attribute__((__warn_unused_result__));

__wasi_errno_t wasmtime_ssp_fd_seek(
#if !defined(WASMTIME_SSP_STATIC_CURFDS)
    struct fd_table *curfds,
#endif
    __wasi_fd_t fd,
    __wasi_filedelta_t offset,
    __wasi_whence_t whence,
    __wasi_filesize_t *newoffset
) WASMTIME_SSP_SYSCALL_NAME(fd_seek) __attribute__((__warn_unused_result__));

__wasi_errno_t wasmtime_ssp_fd_tell(
#if !defined(WASMTIME_SSP_STATIC_CURFDS)
    struct fd_table *curfds,
#endif
    __wasi_fd_t fd,
    __wasi_filesize_t *newoffset
) WASMTIME_SSP_SYSCALL_NAME(fd_tell) __attribute__((__warn_unused_result__));

__wasi_errno_t wasmtime_ssp_fd_fdstat_get(
#if !defined(WASMTIME_SSP_STATIC_CURFDS)
    struct fd_table *curfds,
#endif
    __wasi_fd_t fd,
    __wasi_fdstat_t *buf
) WASMTIME_SSP_SYSCALL_NAME(fd_fdstat_get) __attribute__((__warn_unused_result__));

__wasi_errno_t wasmtime_ssp_fd_fdstat_set_flags(
#if !defined(WASMTIME_SSP_STATIC_CURFDS)
    struct fd_table *curfds,
#endif
    __wasi_fd_t fd,
    __wasi_fdflags_t flags
) WASMTIME_SSP_SYSCALL_NAME(fd_fdstat_set_flags) __attribute__((__warn_unused_result__));

__wasi_errno_t wasmtime_ssp_fd_fdstat_set_rights(
#if !defined(WASMTIME_SSP_STATIC_CURFDS)
    struct fd_table *curfds,
#endif
    __wasi_fd_t fd,
    __wasi_rights_t fs_rights_base,
    __wasi_rights_t fs_rights_inheriting
) WASMTIME_SSP_SYSCALL_NAME(fd_fdstat_set_rights) __attribute__((__warn_unused_result__));

__wasi_errno_t wasmtime_ssp_fd_sync(
#if !defined(WASMTIME_SSP_STATIC_CURFDS)
    struct fd_table *curfds,
#endif
    __wasi_fd_t fd
) WASMTIME_SSP_SYSCALL_NAME(fd_sync) __attribute__((__warn_unused_result__));

__wasi_errno_t wasmtime_ssp_fd_write(
#if !defined(WASMTIME_SSP_STATIC_CURFDS)
    struct fd_table *curfds,
#endif
    __wasi_fd_t fd,
    const __wasi_ciovec_t *iovs,
    size_t iovs_len,
    size_t *nwritten
) WASMTIME_SSP_SYSCALL_NAME(fd_write) __attribute__((__warn_unused_result__));

__wasi_errno_t wasmtime_ssp_fd_advise(
#if !defined(WASMTIME_SSP_STATIC_CURFDS)
    struct fd_table *curfds,
#endif
    __wasi_fd_t fd,
    __wasi_filesize_t offset,
    __wasi_filesize_t len,
    __wasi_advice_t advice
) WASMTIME_SSP_SYSCALL_NAME(fd_advise) __attribute__((__warn_unused_result__));

__wasi_errno_t wasmtime_ssp_fd_allocate(
#if !defined(WASMTIME_SSP_STATIC_CURFDS)
    struct fd_table *curfds,
#endif
    __wasi_fd_t fd,
    __wasi_filesize_t offset,
    __wasi_filesize_t len
) WASMTIME_SSP_SYSCALL_NAME(fd_allocate) __attribute__((__warn_unused_result__));

__wasi_errno_t wasmtime_ssp_path_create_directory(
#if !defined(WASMTIME_SSP_STATIC_CURFDS)
    struct fd_table *curfds,
#endif
    __wasi_fd_t fd,
    const char *path,
    size_t path_len
) WASMTIME_SSP_SYSCALL_NAME(path_create_directory) __attribute__((__warn_unused_result__));

__wasi_errno_t wasmtime_ssp_path_link(
#if !defined(WASMTIME_SSP_STATIC_CURFDS)
    struct fd_table *curfds,
#endif
    __wasi_fd_t old_fd,
    __wasi_lookupflags_t old_flags,
    const char *old_path,
    size_t old_path_len,
    __wasi_fd_t new_fd,
    const char *new_path,
    size_t new_path_len
) WASMTIME_SSP_SYSCALL_NAME(path_link) __attribute__((__warn_unused_result__));

__wasi_errno_t wasmtime_ssp_path_open(
#if !defined(WASMTIME_SSP_STATIC_CURFDS)
    struct fd_table *curfds,
#endif
    __wasi_fd_t dirfd,
    __wasi_lookupflags_t dirflags,
    const char *path,
    size_t path_len,
    __wasi_oflags_t oflags,
    __wasi_rights_t fs_rights_base,
    __wasi_rights_t fs_rights_inheriting,
    __wasi_fdflags_t fs_flags,
    __wasi_fd_t *fd
) WASMTIME_SSP_SYSCALL_NAME(path_open) __attribute__((__warn_unused_result__));

__wasi_errno_t wasmtime_ssp_fd_readdir(
#if !defined(WASMTIME_SSP_STATIC_CURFDS)
    struct fd_table *curfds,
#endif
    __wasi_fd_t fd,
    void *buf,
    size_t buf_len,
    __wasi_dircookie_t cookie,
    size_t *bufused
) WASMTIME_SSP_SYSCALL_NAME(fd_readdir) __attribute__((__warn_unused_result__));

__wasi_errno_t wasmtime_ssp_path_readlink(
#if !defined(WASMTIME_SSP_STATIC_CURFDS)
    struct fd_table *curfds,
#endif
    __wasi_fd_t fd,
    const char *path,
    size_t path_len,
    char *buf,
    size_t buf_len,
    size_t *bufused
) WASMTIME_SSP_SYSCALL_NAME(path_readlink) __attribute__((__warn_unused_result__));

__wasi_errno_t wasmtime_ssp_path_rename(
#if !defined(WASMTIME_SSP_STATIC_CURFDS)
    struct fd_table *curfds,
#endif
    __wasi_fd_t old_fd,
    const char *old_path,
    size_t old_path_len,
    __wasi_fd_t new_fd,
    const char *new_path,
    size_t new_path_len
) WASMTIME_SSP_SYSCALL_NAME(path_rename) __attribute__((__warn_unused_result__));

__wasi_errno_t wasmtime_ssp_fd_filestat_get(
#if !defined(WASMTIME_SSP_STATIC_CURFDS)
    struct fd_table *curfds,
#endif
    __wasi_fd_t fd,
    __wasi_filestat_t *buf
) WASMTIME_SSP_SYSCALL_NAME(fd_filestat_get) __attribute__((__warn_unused_result__));

__wasi_errno_t wasmtime_ssp_fd_filestat_set_times(
#if !defined(WASMTIME_SSP_STATIC_CURFDS)
    struct fd_table *curfds,
#endif
    __wasi_fd_t fd,
    __wasi_timestamp_t st_atim,
    __wasi_timestamp_t st_mtim,
    __wasi_fstflags_t fstflags
) WASMTIME_SSP_SYSCALL_NAME(fd_filestat_set_times) __attribute__((__warn_unused_result__));

__wasi_errno_t wasmtime_ssp_fd_filestat_set_size(
#if !defined(WASMTIME_SSP_STATIC_CURFDS)
    struct fd_table *curfds,
#endif
    __wasi_fd_t fd,
    __wasi_filesize_t st_size
) WASMTIME_SSP_SYSCALL_NAME(fd_filestat_set_size) __attribute__((__warn_unused_result__));

__wasi_errno_t wasmtime_ssp_path_filestat_get(
#if !defined(WASMTIME_SSP_STATIC_CURFDS)
    struct fd_table *curfds,
#endif
    __wasi_fd_t fd,
    __wasi_lookupflags_t flags,
    const char *path,
    size_t path_len,
    __wasi_filestat_t *buf
) WASMTIME_SSP_SYSCALL_NAME(path_filestat_get) __attribute__((__warn_unused_result__));

__wasi_errno_t wasmtime_ssp_path_filestat_set_times(
#if !defined(WASMTIME_SSP_STATIC_CURFDS)
    struct fd_table *curfds,
#endif
    __wasi_fd_t fd,
    __wasi_lookupflags_t flags,
    const char *path,
    size_t path_len,
    __wasi_timestamp_t st_atim,
    __wasi_timestamp_t st_mtim,
    __wasi_fstflags_t fstflags
) WASMTIME_SSP_SYSCALL_NAME(path_filestat_set_times) __attribute__((__warn_unused_result__));

__wasi_errno_t wasmtime_ssp_path_symlink(
#if !defined(WASMTIME_SSP_STATIC_CURFDS)
    struct fd_table *curfds,
#endif
    const char *old_path,
    size_t old_path_len,
    __wasi_fd_t fd,
    const char *new_path,
    size_t new_path_len
) WASMTIME_SSP_SYSCALL_NAME(path_symlink) __attribute__((__warn_unused_result__));

__wasi_errno_t wasmtime_ssp_path_unlink_file(
#if !defined(WASMTIME_SSP_STATIC_CURFDS)
    struct fd_table *curfds,
#endif
    __wasi_fd_t fd,
    const char *path,
    size_t path_len
) WASMTIME_SSP_SYSCALL_NAME(path_unlink_file) __attribute__((__warn_unused_result__));

__wasi_errno_t wasmtime_ssp_path_remove_directory(
#if !defined(WASMTIME_SSP_STATIC_CURFDS)
    struct fd_table *curfds,
#endif
    __wasi_fd_t fd,
    const char *path,
    size_t path_len
) WASMTIME_SSP_SYSCALL_NAME(path_remove_directory) __attribute__((__warn_unused_result__));

__wasi_errno_t wasmtime_ssp_poll_oneoff(
#if !defined(WASMTIME_SSP_STATIC_CURFDS)
    struct fd_table *curfds,
#endif
    const __wasi_subscription_t *in,
    __wasi_event_t *out,
    size_t nsubscriptions,
    size_t *nevents
) WASMTIME_SSP_SYSCALL_NAME(poll_oneoff) __attribute__((__warn_unused_result__));

_Noreturn void wasmtime_ssp_proc_exit(
    __wasi_exitcode_t rval
) WASMTIME_SSP_SYSCALL_NAME(proc_exit);

__wasi_errno_t wasmtime_ssp_proc_raise(
    __wasi_signal_t sig
) WASMTIME_SSP_SYSCALL_NAME(proc_raise) __attribute__((__warn_unused_result__));

__wasi_errno_t wasmtime_ssp_random_get(
    void *buf,
    size_t buf_len
) WASMTIME_SSP_SYSCALL_NAME(random_get) __attribute__((__warn_unused_result__));

__wasi_errno_t wasmtime_ssp_sock_recv(
#if !defined(WASMTIME_SSP_STATIC_CURFDS)
    struct fd_table *curfds,
#endif
    __wasi_fd_t sock,
    const __wasi_iovec_t *ri_data,
    size_t ri_data_len,
    __wasi_riflags_t ri_flags,
    size_t *ro_datalen,
    __wasi_roflags_t *ro_flags
) WASMTIME_SSP_SYSCALL_NAME(sock_recv) __attribute__((__warn_unused_result__));

__wasi_errno_t wasmtime_ssp_sock_send(
#if !defined(WASMTIME_SSP_STATIC_CURFDS)
    struct fd_table *curfds,
#endif
    __wasi_fd_t sock,
    const __wasi_ciovec_t *si_data,
    size_t si_data_len,
    __wasi_siflags_t si_flags,
    size_t *so_datalen
) WASMTIME_SSP_SYSCALL_NAME(sock_send) __attribute__((__warn_unused_result__));

__wasi_errno_t wasmtime_ssp_sock_shutdown(
#if !defined(WASMTIME_SSP_STATIC_CURFDS)
    struct fd_table *curfds,
#endif
    __wasi_fd_t sock,
    __wasi_sdflags_t how
) WASMTIME_SSP_SYSCALL_NAME(sock_shutdown) __attribute__((__warn_unused_result__));

__wasi_errno_t wasmtime_ssp_sched_yield(void)
    WASMTIME_SSP_SYSCALL_NAME(sched_yield) __attribute__((__warn_unused_result__));

#ifdef __cplusplus
}
#endif

#undef WASMTIME_SSP_SYSCALL_NAME

#endif
