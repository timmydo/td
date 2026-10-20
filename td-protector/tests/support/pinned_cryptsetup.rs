//! The pinned cryptsetup, the first `cryptsetup` on PATH. Header sizes and
//! keyslot layouts are that release's, so another one is refused rather
//! than measured.

use std::path::PathBuf;
use std::process::Command;

/// The release the cryptsetup-x86-64 recipe's version line pins; the build
/// flags that follow it there vary by host build and are not compared.
const RELEASE: &str = "cryptsetup 2.8.8 ";

pub fn path() -> PathBuf {
    let Some(path) = td_fs::executable_on_path("cryptsetup") else {
        panic!("cryptsetup not found on PATH; the test needs the pinned cryptsetup 2.8.8");
    };
    let version = Command::new(&path)
        .arg("--version")
        .output()
        .unwrap_or_else(|e| panic!("{} --version: {e}", path.display()));
    let stdout = String::from_utf8_lossy(&version.stdout);
    assert!(
        version.status.success() && stdout.starts_with(RELEASE),
        "{} is not cryptsetup 2.8.8: {stdout}",
        path.display()
    );
    path
}
