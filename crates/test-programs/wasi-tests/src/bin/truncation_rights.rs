use std::{env, mem, process};
use wasi_old::wasi_unstable;
use wasi_tests::open_scratch_directory;
use wasi_tests::utils::{cleanup_file, close_fd, create_file};
use wasi_tests::wasi_wrappers::{wasi_fd_fdstat_get, wasi_path_open};

unsafe fn test_truncation_rights(dir_fd: wasi_unstable::Fd) {
    // Create a file in the scratch directory.
    create_file(dir_fd, "file");

    // Get the rights for the scratch directory.
    let mut dir_fdstat: wasi_unstable::FdStat = mem::zeroed();
    let mut status = wasi_fd_fdstat_get(dir_fd, &mut dir_fdstat);
    assert_eq!(
        status,
        wasi_unstable::raw::__WASI_ESUCCESS,
        "calling fd_fdstat on the scratch directory"
    );
    assert_eq!(
        dir_fdstat.fs_filetype,
        wasi_unstable::FILETYPE_DIRECTORY,
        "expected the scratch directory to be a directory",
    );
    assert_eq!(
        dir_fdstat.fs_flags, 0,
        "expected the scratch directory to have no special flags",
    );
    assert_eq!(
        dir_fdstat.fs_rights_base & wasi_unstable::RIGHT_FD_FILESTAT_SET_SIZE,
        0,
        "directories shouldn't have the fd_filestat_set_size right",
    );

    // If we have the right to set sizes from paths, test that it works.
    if (dir_fdstat.fs_rights_base & wasi_unstable::RIGHT_PATH_FILESTAT_SET_SIZE) == 0 {
        eprintln!("implementation doesn't support setting file sizes, skipping");
    } else {
        // Test that we can truncate the file.
        let mut file_fd: wasi_unstable::Fd = wasi_unstable::Fd::max_value() - 1;
        status = wasi_path_open(
            dir_fd,
            0,
            "file",
            wasi_unstable::O_TRUNC,
            0,
            0,
            0,
            &mut file_fd,
        );
        assert_eq!(
            status,
            wasi_unstable::raw::__WASI_ESUCCESS,
            "truncating a file"
        );
        close_fd(file_fd);

        let mut rights_base: wasi_unstable::Rights = dir_fdstat.fs_rights_base;
        let mut rights_inheriting: wasi_unstable::Rights = dir_fdstat.fs_rights_inheriting;

        if (rights_inheriting & wasi_unstable::RIGHT_FD_FILESTAT_SET_SIZE) == 0 {
            eprintln!("implementation doesn't support setting file sizes through file descriptors, skipping");
        } else {
            rights_inheriting &= !wasi_unstable::RIGHT_FD_FILESTAT_SET_SIZE;
            assert!(
                wasi_unstable::fd_fdstat_set_rights(dir_fd, rights_base, rights_inheriting).is_ok(),
                "droping fd_filestat_set_size inheriting right on a directory",
            );
        }

        // Test that we can truncate the file without the
        // wasi_unstable::RIGHT_FD_FILESTAT_SET_SIZE right.
        status = wasi_path_open(
            dir_fd,
            0,
            "file",
            wasi_unstable::O_TRUNC,
            0,
            0,
            0,
            &mut file_fd,
        );
        assert_eq!(
            status,
            wasi_unstable::raw::__WASI_ESUCCESS,
            "truncating a file without fd_filestat_set_size right",
        );
        close_fd(file_fd);

        rights_base &= !wasi_unstable::RIGHT_PATH_FILESTAT_SET_SIZE;
        assert!(
            wasi_unstable::fd_fdstat_set_rights(dir_fd, rights_base, rights_inheriting).is_ok(),
            "droping path_filestat_set_size base right on a directory",
        );

        // Test that clearing wasi_unstable::RIGHT_PATH_FILESTAT_SET_SIZE actually
        // took effect.
        status = wasi_fd_fdstat_get(dir_fd, &mut dir_fdstat);
        assert_eq!(
            status,
            wasi_unstable::raw::__WASI_ESUCCESS,
            "reading the fdstat from a directory",
        );
        assert_eq!(
            (dir_fdstat.fs_rights_base & wasi_unstable::RIGHT_PATH_FILESTAT_SET_SIZE),
            0,
            "reading the fdstat from a directory",
        );

        // Test that we can truncate the file without the
        // wasi_unstable::RIGHT_PATH_FILESTAT_SET_SIZE, but having
        // the wasi_unstable::RIGHT_FD_WRITE right.
        status = wasi_path_open(
            dir_fd,
            0,
            "file",
            wasi_unstable::O_TRUNC,
            wasi_unstable::RIGHT_FD_WRITE,
            0,
            0,
            &mut file_fd,
        );
        assert_eq!(
            status,
            wasi_unstable::raw::__WASI_ESUCCESS,
            "truncating a file without path_filestat_set_size right but with RIGHT_FD_WRITE",
        );

        // Test that we can't truncate the file without the
        // wasi_unstable::RIGHT_PATH_FILESTAT_SET_SIZE
        // and wasi_unstable::RIGHT_FD_WRITE rights.
        status = wasi_path_open(
            dir_fd,
            0,
            "file",
            wasi_unstable::O_TRUNC,
            0,
            0,
            0,
            &mut file_fd,
        );
        assert_eq!(
            status,
            wasi_unstable::raw::__WASI_ENOTCAPABLE,
            "truncating a file without path_filestat_set_size right and without RIGHT_FD_WRITE",
        );
        assert_eq!(
            file_fd,
            wasi_unstable::Fd::max_value(),
            "failed open should set the file descriptor to -1",
        );
    }

    cleanup_file(dir_fd, "file");
}

fn main() {
    let mut args = env::args();
    let prog = args.next().unwrap();
    let arg = if let Some(arg) = args.next() {
        arg
    } else {
        eprintln!("usage: {} <scratch directory>", prog);
        process::exit(1);
    };

    // Open scratch directory
    let dir_fd = match open_scratch_directory(&arg) {
        Ok(dir_fd) => dir_fd,
        Err(err) => {
            eprintln!("{}", err);
            process::exit(1)
        }
    };

    // Run the tests.
    unsafe { test_truncation_rights(dir_fd) }
}
