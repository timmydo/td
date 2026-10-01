//! The real binary's fixed preview and its font notices: the editor's
//! scene painted by td-editor's own preview over the shared core.
#![allow(clippy::unwrap_used, reason = "asserted test fixtures")]

use td_editor::preview;

#[test]
fn the_real_binary_exposes_a_deterministic_preview_and_its_font_notices() {
    let exe = env!("CARGO_BIN_EXE_td-editor");
    let output = std::process::Command::new(exe)
        .arg("--preview")
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("DISPLAY")
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    assert!(output.stdout.starts_with(b"P6\n800 600\n255\n"));
    assert_eq!(output.stdout.len(), 15 + 800 * 600 * 3);
    let mut expected = Vec::new();
    preview::write(&mut expected).unwrap();
    assert_eq!(output.stdout, expected);
    let hash = output
        .stdout
        .iter()
        .fold(0xcbf29ce484222325u64, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
        });
    // The scrollbar reserves two text columns and changes the frame layout.
    assert_eq!(hash, 0xba167a6cba06d304, "preview checksum: {hash:016x}");
    let output = std::process::Command::new(exe)
        .arg("--font-license")
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let notices = String::from_utf8(output.stdout).unwrap();
    assert!(notices.contains("SIL OPEN FONT LICENSE Version 1.1"));
    assert!(notices.contains("64019ab811067e03a8de5990d2e6f23dcec5418e5a90caa5e5666b0524156732"));
    assert!(notices.contains("/etc/fonts/jetbrains-mono-nerd"));
}
