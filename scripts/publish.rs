//! Helper script to publish the wasmtime and cranelift suites of crates
//!
//! See documentation in `docs/contributing-release-process.md` for more
//! information, but in a nutshell:
//!
//! * `./publish bump` - bump crate versions in-tree
//! * `./publish verify` - verify crates can be published to crates.io
//! * `./publish publish` - actually publish crates to crates.io

use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

// note that this list must be topologically sorted by dependencies
const CRATES_TO_PUBLISH: &[&str] = &[
    // peepmatic
    "peepmatic-traits",
    "peepmatic-macro",
    "peepmatic-automata",
    "peepmatic-test-operator",
    "peepmatic-runtime",
    "peepmatic",
    "peepmatic-souper",
    // cranelift
    "cranelift-entity",
    "cranelift-bforest",
    "cranelift-codegen-shared",
    "cranelift-codegen-meta",
    "cranelift-codegen",
    "cranelift-reader",
    "cranelift-serde",
    "cranelift-module",
    "cranelift-preopt",
    "cranelift-frontend",
    "cranelift-wasm",
    "cranelift-native",
    "cranelift-object",
    "cranelift-interpreter",
    "cranelift",
    "cranelift-jit",
    // wig/wiggle
    "wiggle-generate",
    "wiggle-macro",
    "wiggle",
    "wiggle-borrow",
    "wasmtime-wiggle-macro",
    // wasi-common
    "wasi-common",
    "wasi-cap-std-sync",
    // wasmtime
    "lightbeam",
    "wasmtime-jit-debug",
    "wasmtime-fiber",
    "wasmtime-environ",
    "wasmtime-runtime",
    "wasmtime-debug",
    "wasmtime-profiling",
    "wasmtime-obj",
    "wasmtime-cranelift",
    "wasmtime-lightbeam",
    "wasmtime-jit",
    "wasmtime-cache",
    "wasmtime",
    "wasmtime-wiggle",
    "wasmtime-wasi",
    "wasmtime-wasi-nn",
    "wasmtime-wasi-crypto",
    "wasmtime-rust-macro",
    "wasmtime-rust",
    "wasmtime-wast",
    "wasmtime-cli",
];

struct Crate {
    manifest: PathBuf,
    name: String,
    version: String,
    next_version: String,
    publish: bool,
}

fn main() {
    let mut crates = Vec::new();
    crates.push(read_crate("./Cargo.toml".as_ref()));
    find_crates("crates".as_ref(), &mut crates);
    find_crates("cranelift".as_ref(), &mut crates);

    let pos = CRATES_TO_PUBLISH
        .iter()
        .enumerate()
        .map(|(i, c)| (*c, i))
        .collect::<HashMap<_, _>>();
    crates.sort_by_key(|krate| pos.get(&krate.name[..]));

    match &env::args().nth(1).expect("must have one argument")[..] {
        "bump" => {
            for krate in crates.iter() {
                bump_version(&krate, &crates);
            }
            // update the lock file
            assert!(Command::new("cargo")
                .arg("fetch")
                .status()
                .unwrap()
                .success());
        }

        "publish" => {
            for krate in crates.iter() {
                publish(&krate);
            }
            println!("");
            println!("===================================================================");
            println!("");
            println!("Don't forget to push a git tag for this release!");
            println!("");
            println!("    $ git tag vX.Y.Z");
            println!("    $ git push git@github.com:bytecodealliance/wasmtime.git vX.Y.Z");
        }

        "verify" => {
            verify(&crates);
        }

        s => panic!("unknown command: {}", s),
    }
}

fn find_crates(dir: &Path, dst: &mut Vec<Crate>) {
    if dir.join("Cargo.toml").exists() {
        let krate = read_crate(&dir.join("Cargo.toml"));
        if !krate.publish || CRATES_TO_PUBLISH.iter().any(|c| krate.name == *c) {
            dst.push(krate);
        } else {
            panic!("failed to find {:?} in whitelist or blacklist", krate.name);
        }
    }

    for entry in dir.read_dir().unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            find_crates(&entry.path(), dst);
        }
    }
}

fn read_crate(manifest: &Path) -> Crate {
    let mut name = None;
    let mut version = None;
    let mut publish = true;
    for line in fs::read_to_string(manifest).unwrap().lines() {
        if name.is_none() && line.starts_with("name = \"") {
            name = Some(
                line.replace("name = \"", "")
                    .replace("\"", "")
                    .trim()
                    .to_string(),
            );
        }
        if version.is_none() && line.starts_with("version = \"") {
            version = Some(
                line.replace("version = \"", "")
                    .replace("\"", "")
                    .trim()
                    .to_string(),
            );
        }
        if line.starts_with("publish = false") {
            publish = false;
        }
    }
    let name = name.unwrap();
    let version = version.unwrap();
    let next_version = if CRATES_TO_PUBLISH.contains(&&name[..]) {
        bump(&version)
    } else {
        version.clone()
    };
    if ["witx", "wasi-crypto"].contains(&&name[..]) {
        publish = false;
    }
    Crate {
        manifest: manifest.to_path_buf(),
        name,
        version,
        next_version,
        publish,
    }
}

fn bump_version(krate: &Crate, crates: &[Crate]) {
    let contents = fs::read_to_string(&krate.manifest).unwrap();

    let mut new_manifest = String::new();
    let mut is_deps = false;
    for line in contents.lines() {
        let mut rewritten = false;
        if !is_deps && line.starts_with("version =") {
            if CRATES_TO_PUBLISH.contains(&&krate.name[..]) {
                println!(
                    "bump `{}` {} => {}",
                    krate.name, krate.version, krate.next_version
                );
                new_manifest.push_str(&line.replace(&krate.version, &krate.next_version));
                rewritten = true;
            }
        }

        is_deps = if line.starts_with("[") {
            line.contains("dependencies")
        } else {
            is_deps
        };

        for other in crates {
            if !is_deps || !line.starts_with(&format!("{} ", other.name)) {
                continue;
            }
            if !line.contains(&other.version) {
                if !line.contains("version =") {
                    continue;
                }
                panic!(
                    "{:?} has a dep on {} but doesn't list version {}",
                    krate.manifest, other.name, other.version
                );
            }
            rewritten = true;
            new_manifest.push_str(&line.replace(&other.version, &other.next_version));
            break;
        }
        if !rewritten {
            new_manifest.push_str(line);
        }
        new_manifest.push_str("\n");
    }
    fs::write(&krate.manifest, new_manifest).unwrap();
}

/// Performs a major version bump increment on the semver version `version`.
///
/// This function will perform a semver-major-version bump on the `version`
/// specified. This is used to calculate the next version of a crate in this
/// repository since we're currently making major version bumps for all our
/// releases. This may end up getting tweaked as we stabilize crates and start
/// doing more minor/patch releases, but for now this should do the trick.
fn bump(version: &str) -> String {
    let mut iter = version.split('.').map(|s| s.parse::<u32>().unwrap());
    let major = iter.next().expect("major version");
    let minor = iter.next().expect("minor version");
    let patch = iter.next().expect("patch version");
    if major != 0 {
        format!("{}.0.0", major + 1)
    } else if minor != 0 {
        format!("0.{}.0", minor + 1)
    } else {
        format!("0.0.{}", patch + 1)
    }
}

fn publish(krate: &Crate) {
    if !CRATES_TO_PUBLISH.iter().any(|s| *s == krate.name) {
        return;
    }
    let status = Command::new("cargo")
        .arg("publish")
        .current_dir(krate.manifest.parent().unwrap())
        .arg("--no-verify")
        .status()
        .expect("failed to run cargo");
    if !status.success() {
        println!("FAIL: failed to publish `{}`: {}", krate.name, status);
    }

    // Note that the status is ignored here. This fails most of the time because
    // the owner is already set and present, so we only want to add this to
    // crates which haven't previously been published.
    Command::new("cargo")
        .arg("owner")
        .arg("-a")
        .arg("github:bytecodealliance:wasmtime-publish")
        .arg(&krate.name)
        .status()
        .expect("failed to run cargo");
}

// Verify the current tree is publish-able to crates.io. The intention here is
// that we'll run `cargo package` on everything which verifies the build as-if
// it were published to crates.io. This requires using an incrementally-built
// directory registry generated from `cargo vendor` because the versions
// referenced from `Cargo.toml` may not exist on crates.io.
fn verify(crates: &[Crate]) {
    drop(fs::remove_dir_all(".cargo"));
    drop(fs::remove_dir_all("vendor"));
    let vendor = Command::new("cargo")
        .arg("vendor")
        .stderr(Stdio::inherit())
        .output()
        .unwrap();
    assert!(vendor.status.success());

    fs::create_dir_all(".cargo").unwrap();
    fs::write(".cargo/config.toml", vendor.stdout).unwrap();

    // Vendor witx which wasn't vendored because it's a path dependency, but
    // it'll need to be in our directory registry for crates that depend on it.
    let witx = crates
        .iter()
        .find(|c| c.name == "witx" && c.manifest.iter().any(|p| p == "wasi-common"))
        .unwrap();
    verify_and_vendor(&witx);

    // Vendor wasi-crypto which is also a path dependency
    let wasi_crypto = crates.iter().find(|c| c.name == "wasi-crypto").unwrap();
    verify_and_vendor(&wasi_crypto);

    for krate in crates {
        if !krate.publish {
            continue;
        }
        verify_and_vendor(&krate);
    }

    fn verify_and_vendor(krate: &Crate) {
        let mut cmd = Command::new("cargo");
        cmd.arg("package")
            .arg("--manifest-path")
            .arg(&krate.manifest)
            .env("CARGO_TARGET_DIR", "./target");
        if krate.name.contains("lightbeam")
            || krate.name == "witx"
            || krate.name.contains("wasi-nn")
        {
            cmd.arg("--no-verify");
        }
        let status = cmd.status().unwrap();
        assert!(status.success(), "failed to verify {:?}", &krate.manifest);
        let tar = Command::new("tar")
            .arg("xf")
            .arg(format!(
                "../target/package/{}-{}.crate",
                krate.name, krate.version
            ))
            .current_dir("./vendor")
            .status()
            .unwrap();
        assert!(tar.success());
        fs::write(
            format!(
                "./vendor/{}-{}/.cargo-checksum.json",
                krate.name, krate.version
            ),
            "{\"files\":{}}",
        )
        .unwrap();
    }
}
