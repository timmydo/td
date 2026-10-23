//! Build-environment adapter for the shared pinned Rust transform.
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
pub use td_engine::rust_dist::GLIBC_X86_64_STAGE;
use td_engine::rust_dist::{unpack_rust_component, RustDistInputs};

fn from_drv_env() -> Result<RustDistInputs, String> {
    let g = |k: &str| -> Result<String, String> {
        std::env::var(k).map_err(|_| format!("env {k} unset"))
    };
    let map =
        crate::json::parse(&g("TD_INPUT_MAP")?).map_err(|e| format!("TD_INPUT_MAP JSON: {e}"))?;
    let pick = |name: &str| -> Result<PathBuf, String> {
        map.get(name)
            .and_then(crate::json::Json::as_str)
            .map(PathBuf::from)
            .ok_or_else(|| format!("rust-dist: lock is missing the `{name}' input"))
    };
    let unpack = |tar: &Path, tag: &str, required: &str| -> Result<PathBuf, String> {
        let dir = mktemp_dir(tag)?;
        unpack_rust_component(tar, &dir, required)?;
        Ok(dir)
    };
    Ok(RustDistInputs {
        rustc_src: unpack(
            Path::new(&g("TD_SRC")?),
            "td-rust-dist-rustc",
            "rustc/bin/rustc",
        )?,
        std_src: unpack(
            &pick("rust-std-dist")?,
            "td-rust-dist-std",
            "rust-std-x86_64-unknown-linux-gnu/lib/rustlib",
        )?,
        cargo_src: unpack(
            &pick("rust-cargo-dist")?,
            "td-rust-dist-cargo",
            "cargo/bin/cargo",
        )?,
        clippy_src: unpack(
            &pick("rust-clippy-dist")?,
            "td-rust-dist-clippy",
            "clippy-preview/bin/clippy-driver",
        )?,
        glibc: pick("glibc-x86-64")?.join(GLIBC_X86_64_STAGE),
        libgcc_s: pick("gcc-runtime")?.join("lib/libgcc_s.so.1"),
        libz: pick("zlib-x86-64")?.join("lib/libz.so.1"),
        out: PathBuf::from(g("out")?),
    })
}

pub fn run_rust_dist_build() -> Result<(), String> {
    td_engine::rust_dist::assemble(&from_drv_env()?)
}

fn mktemp_dir(prefix: &str) -> Result<PathBuf, String> {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("{prefix}-{}-{nanos}-{n}", std::process::id()));
    fs::create_dir_all(&dir).map_err(ioerr("mktemp dir"))?;
    Ok(dir)
}

fn ioerr(ctx: &'static str) -> impl Fn(io::Error) -> String {
    move |e| format!("{ctx}: {e}")
}
