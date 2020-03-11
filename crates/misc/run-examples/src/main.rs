use std::collections::BTreeSet;
use std::process::Command;

fn main() {
    let examples = std::fs::read_dir("examples").unwrap();
    let examples = examples
        .map(|e| {
            let e = e.unwrap();
            let path = e.path();
            let dir = e.metadata().unwrap().is_dir();
            (path.file_stem().unwrap().to_str().unwrap().to_owned(), dir)
        })
        .collect::<BTreeSet<_>>();

    println!("======== Building libwasmtime.a ===========");
    run(Command::new("cargo").args(&["build", "-p", "wasmtime-c-api", "--release"]));

    for (example, is_dir) in examples {
        if example == "README" {
            continue;
        }
        if is_dir {
            println!("======== Rust wasm file `{}` ============", example);
            run(Command::new("cargo")
                .arg("build")
                .arg("-p")
                .arg(format!("example-{}-wasm", example))
                .arg("--target")
                .arg("wasm32-unknown-unknown"));
        }
        println!("======== Rust example `{}` ============", example);
        run(Command::new("cargo")
            .arg("run")
            .arg("--example")
            .arg(&example));

        println!("======== C example `{}` ============", example);
        let mut cmd = cc::Build::new()
            .opt_level(0)
            .cargo_metadata(false)
            .target(env!("TARGET"))
            .host(env!("TARGET"))
            .include("crates/c-api/include")
            .include("crates/c-api/wasm-c-api/include")
            .get_compiler()
            .to_command();
        if is_dir {
            cmd.arg(format!("examples/{}/main.c", example));
        } else {
            cmd.arg(format!("examples/{}.c", example));
        }
        cmd.arg("target/release/libwasmtime.a");
        if cfg!(target_os = "linux") {
            cmd.arg("-lpthread").arg("-ldl").arg("-lm");
        }
        cmd.arg("-o").arg("foo");
        run(&mut cmd);

        run(&mut Command::new("./foo"));
    }
}

fn run(cmd: &mut Command) {
    let s = cmd.status().unwrap();
    if !s.success() {
        eprintln!("failed to run {:?}", cmd);
        eprintln!("status: {}", s);
        std::process::exit(1);
    }
}
