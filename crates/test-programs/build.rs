//! Build program to generate a program which runs all the testsuites.
//!
//! By generating a separate `#[test]` test for each file, we allow cargo test
//! to automatically run the files in parallel.

fn main() {
    #[cfg(feature = "test_programs")]
    wasi_tests::build_and_generate_tests()
}

#[cfg(feature = "test_programs")]
mod wasi_tests {
    use std::env;
    use std::fs::{read_dir, File};
    use std::io::{self, Write};
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};

    pub(super) fn build_and_generate_tests() {
        // Validate if any of test sources are present and if they changed
        // This should always work since there is no submodule to init anymore
        let bin_tests = std::fs::read_dir("wasi-tests/src/bin").unwrap();
        for test in bin_tests {
            if let Ok(test_file) = test {
                let test_file_path = test_file
                    .path()
                    .into_os_string()
                    .into_string()
                    .expect("test file path");
                println!("cargo:rerun-if-changed={}", test_file_path);
            }
        }
        println!("cargo:rerun-if-changed=wasi-tests/Cargo.toml");
        println!("cargo:rerun-if-changed=wasi-tests/src/lib.rs");
        // Build tests to OUT_DIR (target/*/build/wasi-common-*/out/wasm32-wasi/release/*.wasm)
        let out_dir = PathBuf::from(
            env::var("OUT_DIR").expect("The OUT_DIR environment variable must be set"),
        );
        let mut out =
            File::create(out_dir.join("wasi_tests.rs")).expect("error generating test source file");
        build_tests("wasi-tests", &out_dir).expect("building tests");
        test_directory(&mut out, "wasi-cap-std-sync", "cap_std_sync", &out_dir)
            .expect("generating tests");
        test_directory(&mut out, "wasi-virtfs", "virtfs", &out_dir).expect("generating tests");
    }

    fn build_tests(testsuite: &str, out_dir: &Path) -> io::Result<()> {
        let mut cmd = Command::new("cargo");
        cmd.env("CARGO_PROFILE_RELEASE_DEBUG", "1");
        cmd.args(&[
            "build",
            "--release",
            "--target=wasm32-wasi",
            "--target-dir",
            out_dir.to_str().unwrap(),
        ])
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .current_dir(testsuite);
        let output = cmd.output()?;

        let status = output.status;
        if !status.success() {
            panic!(
                "Building tests failed: exit code: {}",
                status.code().unwrap()
            );
        }

        Ok(())
    }

    fn test_directory(
        out: &mut File,
        testsuite: &str,
        runtime: &str,
        out_dir: &Path,
    ) -> io::Result<()> {
        let mut dir_entries: Vec<_> = read_dir(out_dir.join("wasm32-wasi/release"))
            .expect("reading testsuite directory")
            .map(|r| r.expect("reading testsuite directory entry"))
            .filter(|dir_entry| {
                let p = dir_entry.path();
                if let Some(ext) = p.extension() {
                    // Only look at wast files.
                    if ext == "wasm" {
                        // Ignore files starting with `.`, which could be editor temporary files
                        if let Some(stem) = p.file_stem() {
                            if let Some(stemstr) = stem.to_str() {
                                if !stemstr.starts_with('.') {
                                    return true;
                                }
                            }
                        }
                    }
                }
                false
            })
            .collect();

        dir_entries.sort_by_key(|dir| dir.path());

        writeln!(
            out,
            "mod {} {{",
            Path::new(testsuite)
                .file_stem()
                .expect("testsuite filename should have a stem")
                .to_str()
                .expect("testsuite filename should be representable as a string")
                .replace("-", "_")
        )?;
        writeln!(
            out,
            "    use super::{{runtime::{} as runtime, utils, setup_log}};",
            runtime
        )?;
        for dir_entry in dir_entries {
            write_testsuite_tests(out, &dir_entry.path(), testsuite)?;
        }
        writeln!(out, "}}")?;
        Ok(())
    }

    fn write_testsuite_tests(out: &mut File, path: &Path, testsuite: &str) -> io::Result<()> {
        let stemstr = path
            .file_stem()
            .expect("file_stem")
            .to_str()
            .expect("to_str");

        writeln!(out, "    #[test]")?;
        let test_fn_name = stemstr.replace("-", "_");
        if ignore(testsuite, &test_fn_name) {
            writeln!(out, "    #[ignore]")?;
        }
        writeln!(out, "    fn r#{}() -> anyhow::Result<()> {{", test_fn_name,)?;
        writeln!(out, "        setup_log();")?;
        writeln!(
            out,
            "        let path = std::path::Path::new(r#\"{}\"#);",
            path.display()
        )?;
        writeln!(out, "        let data = wat::parse_file(path)?;")?;
        writeln!(
            out,
            "        let bin_name = utils::extract_exec_name_from_path(path)?;"
        )?;
        let workspace = if no_preopens(testsuite, stemstr) {
            "None"
        } else {
            writeln!(
                out,
                "        let workspace = utils::prepare_workspace(&bin_name)?;"
            )?;
            "Some(workspace.path())"
        };
        writeln!(
            out,
            "        runtime::{}(&data, &bin_name, {})",
            if inherit_stdio(testsuite, stemstr) {
                "instantiate_inherit_stdio"
            } else {
                "instantiate"
            },
            workspace,
        )?;
        writeln!(out, "    }}")?;
        writeln!(out)?;
        Ok(())
    }

    fn ignore(testsuite: &str, name: &str) -> bool {
        match testsuite {
            "wasi-cap-std-sync" => cap_std_sync_ignore(name),
            "wasi-virtfs" => virtfs_ignore(name),
            _ => panic!("unknown test suite: {}", testsuite),
        }
    }

    #[cfg(not(windows))]
    /// Ignore tests that aren't supported yet.
    fn cap_std_sync_ignore(name: &str) -> bool {
        [
            // Trailing slash related bugs:
            "path_rename_file_trailing_slashes",
            "remove_directory_trailing_slashes",
        ]
        .contains(&name)
    }

    #[cfg(windows)]
    /// Ignore tests that aren't supported yet.
    fn cap_std_sync_ignore(name: &str) -> bool {
        [
            // Trailing slash related bugs
            "interesting_paths",
            "path_rename_file_trailing_slashes",
            "remove_directory_trailing_slashes",
        ]
        .contains(&name)
    }

    /// Virtfs barely works at all and is not suitable for any purpose
    fn virtfs_ignore(name: &str) -> bool {
        [
            "dangling_symlink",
            "fd_flags_set",
            "file_seek_tell",
            "file_truncation",
            "interesting_paths",
            "nofollow_errors",
            "path_filestat",
            "path_link",
            "path_rename",
            "path_rename_dir_trailing_slashes",
            "path_rename_file_trailing_slashes",
            "path_symlink_trailing_slashes",
            "poll_oneoff",
            "poll_oneoff_stdio",
            "readlink",
            "remove_directory_trailing_slashes",
            "symlink_create",
            "symlink_filestat",
            "symlink_loop",
            "unlink_file_trailing_slashes",
        ]
        .contains(&name)
    }

    /// Mark tests which do not require preopens
    fn no_preopens(testsuite: &str, name: &str) -> bool {
        if testsuite.starts_with("wasi-") {
            match name {
                "big_random_buf" => true,
                "clock_time_get" => true,
                "sched_yield" => true,
                "poll_oneoff_stdio" => true,
                _ => false,
            }
        } else {
            panic!("unknown test suite {}", testsuite)
        }
    }

    /// Mark tests which require inheriting parent process stdio
    fn inherit_stdio(testsuite: &str, name: &str) -> bool {
        match testsuite {
            "wasi-cap-std-sync" => match name {
                "poll_oneoff_stdio" => true,
                _ => false,
            },
            "wasi-virtfs" => false,
            _ => panic!("unknown test suite {}", testsuite),
        }
    }
}
