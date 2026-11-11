#![forbid(unsafe_code)]
#[path = "../tools/generate.rs"]
mod generator;
fn main() -> std::io::Result<()> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    // All inputs authenticate before any output is touched.
    for (path, bytes) in generator::generate(&root.join("data"))? {
        std::fs::write(root.join(path), bytes)?;
    }
    Ok(())
}
