//! Regenerates `runtime/wasi/`: wit-bindgen's C bindings for the world in
//! `third_party/wasi/wit/pico.wit` (the WASI interfaces pico's runtime imports directly on
//! wasm32-wasip2; decision 257). Run from the repository root:
//!
//!     cargo run -p pico-wasi-bindgen
//!
//! The output is checked in; the component type the bindings need is embedded at link time
//! (`pico-driver`'s `wasi.rs`), so no object file is.

use wit_bindgen_core::Files;
use wit_bindgen_core::wit_parser::Resolve;

fn main() -> anyhow::Result<()> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut resolve = Resolve::default();
    let (pkg, _) = resolve.push_path(root.join("third_party/wasi/wit"))?;
    let world = resolve.select_world(&[pkg], Some("imports"))?;
    let mut files = Files::default();
    let opts = wit_bindgen_c::Opts {
        no_object_file: true,
        ..Default::default()
    };
    opts.build().generate(&mut resolve, world, &mut files)?;
    let out = root.join("runtime/wasi");
    std::fs::create_dir_all(&out)?;
    for (name, bytes) in files.iter() {
        std::fs::write(out.join(name), bytes)?;
        println!("wrote runtime/wasi/{name}");
    }
    Ok(())
}
