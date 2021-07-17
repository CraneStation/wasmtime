#!/bin/bash
set -euo pipefail

WORKDIR=assets                      # our workdir
JS_LIBRARY=$WORKDIR/load-files.js   # JS helper lib which we use in main.rs to handle loading WASI binaries in Emscripten
EM_SHELL=$WORKDIR/shell.html        # basic Emscripten shell env
WASI=$WORKDIR/wasi.js               # WASI polyfill JS glue code
POLYFILL=$WORKDIR/polyfill.html     # our output

EXPORTED_FUNCTIONS=(
  _main
  _get_wasi_context
  _handleFiles
  _wasi_common_args_get
  _wasi_common_args_sizes_get
  _wasi_common_clock_res_get
  _wasi_common_clock_time_get
  _wasi_common_environ_get
  _wasi_common_environ_sizes_get
  _wasi_common_fd_advise
  _wasi_common_fd_allocate
  _wasi_common_fd_close
  _wasi_common_fd_datasync
  _wasi_common_fd_fdstat_get
  _wasi_common_fd_fdstat_set_flags
  _wasi_common_fd_fdstat_set_rights
  _wasi_common_fd_filestat_get
  _wasi_common_fd_filestat_set_size
  _wasi_common_fd_filestat_set_times
  _wasi_common_fd_pread
  _wasi_common_fd_prestat_dir_name
  _wasi_common_fd_prestat_get
  _wasi_common_fd_pwrite
  _wasi_common_fd_read
  _wasi_common_fd_readdir
  _wasi_common_fd_renumber
  _wasi_common_fd_seek
  _wasi_common_fd_sync
  _wasi_common_fd_tell
  _wasi_common_fd_write
  _wasi_common_path_create_directory
  _wasi_common_path_filestat_get
  _wasi_common_path_filestat_set_times
  _wasi_common_path_link
  _wasi_common_path_open
  _wasi_common_path_readlink
  _wasi_common_path_remove_directory
  _wasi_common_path_rename
  _wasi_common_path_symlink
  _wasi_common_path_unlink_file
  _wasi_common_poll_oneoff
  _wasi_common_proc_exit
  _wasi_common_proc_raise
  _wasi_common_random_get
  _wasi_common_sched_yield
  _wasi_common_sock_recv
  _wasi_common_sock_send
  _wasi_common_sock_shutdown
)
EXPORTED_FUNCTIONS_CONCAT=$(printf ",'%s'" "${EXPORTED_FUNCTIONS[@]}")
EXPORTED_FUNCTIONS_CONCAT=${EXPORTED_FUNCTIONS_CONCAT:1}

cargo +nightly rustc --target wasm32-unknown-emscripten --release -vv -- -C link-args="--js-library ${JS_LIBRARY} --shell-file ${EM_SHELL} --pre-js ${WASI} -s EXPORTED_FUNCTIONS=[${EXPORTED_FUNCTIONS_CONCAT}] -o ${POLYFILL}"
