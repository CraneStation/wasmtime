// Part of the Wasmtime Project, under the Apache License v2.0 with LLVM Exceptions.
// See https://github.com/bytecodealliance/wasmtime/blob/master/LICENSE for license information.
//
// Significant parts of this file are derived from cloudabi-utils. See
// https://github.com/bytecodealliance/wasmtime/blob/master/lib/wasi/sandboxed-system-primitives/src/LICENSE
// for license information.
//
// The upstream file contains the following copyright notice:
//
// Copyright (c) 2016 Nuxi, https://nuxi.nl/

#ifndef RIGHTS_H
#define RIGHTS_H

#define RIGHTS_ALL                                                         \
  (__WASI_RIGHT_FD_DATASYNC | __WASI_RIGHT_FD_READ |                       \
   __WASI_RIGHT_FD_SEEK | __WASI_RIGHT_FD_FDSTAT_SET_FLAGS |               \
   __WASI_RIGHT_FD_SYNC | __WASI_RIGHT_FD_TELL | __WASI_RIGHT_FD_WRITE |   \
   __WASI_RIGHT_FD_ADVISE | __WASI_RIGHT_FD_ALLOCATE |                     \
   __WASI_RIGHT_PATH_CREATE_DIRECTORY | __WASI_RIGHT_PATH_CREATE_FILE |    \
   __WASI_RIGHT_PATH_LINK_SOURCE | __WASI_RIGHT_PATH_LINK_TARGET |         \
   __WASI_RIGHT_PATH_OPEN | __WASI_RIGHT_FD_READDIR |                      \
   __WASI_RIGHT_PATH_READLINK | __WASI_RIGHT_PATH_RENAME_SOURCE |          \
   __WASI_RIGHT_PATH_RENAME_TARGET | __WASI_RIGHT_PATH_FILESTAT_GET |      \
   __WASI_RIGHT_PATH_FILESTAT_SET_SIZE |                                   \
   __WASI_RIGHT_PATH_FILESTAT_SET_TIMES |                                  \
   __WASI_RIGHT_FD_FILESTAT_GET | __WASI_RIGHT_FD_FILESTAT_SET_TIMES |     \
   __WASI_RIGHT_FD_FILESTAT_SET_SIZE |                                     \
   __WASI_RIGHT_PATH_SYMLINK | __WASI_RIGHT_PATH_UNLINK_FILE |             \
   __WASI_RIGHT_PATH_REMOVE_DIRECTORY |                                    \
   __WASI_RIGHT_POLL_FD_READWRITE | __WASI_RIGHT_SOCK_SHUTDOWN |           \
   __WASI_RIGHT_SOCK_CONNECT)

// Block and character device interaction is outside the scope of
// CloudABI. Simply allow everything.
#define RIGHTS_BLOCK_DEVICE_BASE RIGHTS_ALL
#define RIGHTS_BLOCK_DEVICE_INHERITING RIGHTS_ALL
#define RIGHTS_CHARACTER_DEVICE_BASE RIGHTS_ALL
#define RIGHTS_CHARACTER_DEVICE_INHERITING RIGHTS_ALL

// Only allow directory operations on directories. Directories can only
// yield file descriptors to other directories and files.
#define RIGHTS_DIRECTORY_BASE                                          \
  (__WASI_RIGHT_FD_FDSTAT_SET_FLAGS | __WASI_RIGHT_FD_SYNC |           \
   __WASI_RIGHT_FD_ADVISE | __WASI_RIGHT_PATH_CREATE_DIRECTORY |       \
   __WASI_RIGHT_PATH_CREATE_FILE | __WASI_RIGHT_PATH_LINK_SOURCE |     \
   __WASI_RIGHT_PATH_LINK_TARGET | __WASI_RIGHT_PATH_OPEN |            \
   __WASI_RIGHT_FD_READDIR | __WASI_RIGHT_PATH_READLINK |              \
   __WASI_RIGHT_PATH_RENAME_SOURCE | __WASI_RIGHT_PATH_RENAME_TARGET | \
   __WASI_RIGHT_PATH_FILESTAT_GET |                                    \
   __WASI_RIGHT_PATH_FILESTAT_SET_SIZE |                               \
   __WASI_RIGHT_PATH_FILESTAT_SET_TIMES |                              \
   __WASI_RIGHT_FD_FILESTAT_GET | __WASI_RIGHT_FD_FILESTAT_SET_TIMES | \
   __WASI_RIGHT_PATH_SYMLINK | __WASI_RIGHT_PATH_UNLINK_FILE |         \
   __WASI_RIGHT_PATH_REMOVE_DIRECTORY |                                \
   __WASI_RIGHT_POLL_FD_READWRITE)
#define RIGHTS_DIRECTORY_INHERITING \
  (RIGHTS_DIRECTORY_BASE | RIGHTS_REGULAR_FILE_BASE)

// Operations that apply to regular files.
#define RIGHTS_REGULAR_FILE_BASE                                           \
  (__WASI_RIGHT_FD_DATASYNC | __WASI_RIGHT_FD_READ |                       \
   __WASI_RIGHT_FD_SEEK | __WASI_RIGHT_FD_FDSTAT_SET_FLAGS |               \
   __WASI_RIGHT_FD_SYNC | __WASI_RIGHT_FD_TELL | __WASI_RIGHT_FD_WRITE |   \
   __WASI_RIGHT_FD_ADVISE | __WASI_RIGHT_FD_ALLOCATE |                     \
   __WASI_RIGHT_FD_FILESTAT_GET | __WASI_RIGHT_FD_FILESTAT_SET_SIZE |      \
   __WASI_RIGHT_FD_FILESTAT_SET_TIMES | __WASI_RIGHT_POLL_FD_READWRITE)
#define RIGHTS_REGULAR_FILE_INHERITING 0

// Operations that apply to sockets and socket pairs.
#define RIGHTS_SOCKET_BASE                                     \
  (__WASI_RIGHT_FD_READ | __WASI_RIGHT_FD_FDSTAT_SET_FLAGS |   \
   __WASI_RIGHT_FD_WRITE | __WASI_RIGHT_FD_FILESTAT_GET |      \
   __WASI_RIGHT_POLL_FD_READWRITE | __WASI_RIGHT_SOCK_SHUTDOWN | \
   __WASI_RIGHT_SOCK_CONNECT)
#define RIGHTS_SOCKET_INHERITING RIGHTS_ALL

// Operations that apply to TTYs.
#define RIGHTS_TTY_BASE                                        \
  (__WASI_RIGHT_FD_READ | __WASI_RIGHT_FD_FDSTAT_SET_FLAGS |   \
   __WASI_RIGHT_FD_WRITE | __WASI_RIGHT_FD_FILESTAT_GET |      \
   __WASI_RIGHT_POLL_FD_READWRITE)
#define RIGHTS_TTY_INHERITING 0

#endif
