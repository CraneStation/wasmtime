//! This is an example of working with multi-value modules and dealing with
//! multi-value functions.
//!
//! Note that the `Func::wrap*` interfaces cannot be used to return multiple
//! values just yet, so we need to use the more dynamic `Func::new` and
//! `Func::call` methods.

// You can execute this example with `cargo run --example multi`

use anyhow::Result;

fn main() -> Result<()> {
    use wasmtime::*;

    println!("Initializing...");
    let engine = Engine::default();
    let mut store = Store::new(&engine, ());

    // Compile.
    println!("Compiling module...");
    let module = Module::from_file(&engine, "examples/multi.wat")?;

    // Create a host function which takes multiple parameters and returns
    // multiple results.
    println!("Creating callback...");
    let callback_func = Func::wrap(&mut store, |a: i32, b: i64| -> (i64, i32) {
        (b + 1, a + 1)
    });

    // Instantiate.
    println!("Instantiating module...");
    let instance = Instance::new(&mut store, &module, &[callback_func.into()])?;

    // Extract exports.
    println!("Extracting export...");
    let g = instance.get_typed_func::<(i32, i64), (i64, i32), _>(&mut store, "g")?;

    // Call `$g`.
    println!("Calling export \"g\"...");
    let (a, b) = g.call(&mut store, (1, 3))?;

    println!("Printing result...");
    println!("> {} {}", a, b);

    assert_eq!(a, 4);
    assert_eq!(b, 2);

    // Call `$round_trip_many`.
    println!("Calling export \"round_trip_many\"...");
    let round_trip_many = instance
        .get_typed_func::<
        (i64, i64, i64, i64, i64, i64, i64, i64, i64, i64),
        (i64, i64, i64, i64, i64, i64, i64, i64, i64, i64),
        _,
        >
        (&mut store, "round_trip_many")?;
    let results = round_trip_many.call(&mut store, (0, 1, 2, 3, 4, 5, 6, 7, 8, 9))?;

    println!("Printing result...");
    println!("> {:?}", results);
    assert_eq!(results, (0, 1, 2, 3, 4, 5, 6, 7, 8, 9));

    Ok(())
}
