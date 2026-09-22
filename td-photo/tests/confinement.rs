#![forbid(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! Source-level contracts the compiler cannot express: the crate's file
//! inventory, that it forbids `unsafe` and declares the toolkit as its one
//! dependency, that its pure modules reach no file, environment, clock,
//! network or process and the window opens no file, which files name which
//! toolkit modules, that photo pixels reach a frame through one blitter,
//! and the budgets DESIGN.md names, by value.

use std::collections::BTreeSet;
use std::path::Path;

fn root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn read(relative: &str) -> String {
    std::fs::read_to_string(root().join(relative)).unwrap_or_else(|e| panic!("{relative}: {e}"))
}

fn names(dir: &str, extension: &str) -> BTreeSet<String> {
    std::fs::read_dir(root().join(dir))
        .unwrap()
        .map(|e| e.unwrap())
        .filter(|e| e.path().extension().is_some_and(|x| x == extension))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect()
}

const PURE: &[&str] = &[
    "av1.rs",
    "camera.rs",
    "cdf.rs",
    "color.rs",
    "develop.rs",
    "image.rs",
    "jpeg.rs",
    "library.rs",
    "look.rs",
    "nef.rs",
    "settings.rs",
    "tiff.rs",
    "transform.rs",
    "ui.rs",
];

#[test]
fn source_inventory_is_closed() {
    assert!(!root().join("build.rs").exists(), "no build script");
    let expected: BTreeSet<String> = PURE
        .iter()
        .chain(["lib.rs", "main.rs", "window.rs"].iter())
        .map(|s| s.to_string())
        .collect();
    assert_eq!(names("src", "rs"), expected);
    let tests: BTreeSet<String> = [
        "av1.rs",
        "confinement.rs",
        "control_process.rs",
        "develop.rs",
        "jpeg.rs",
        "library.rs",
        "look.rs",
        "nef.rs",
        "ui.rs",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    assert_eq!(names("tests", "rs"), tests);
    let support: BTreeSet<String> = ["native_compositor.rs", "synth_nef.rs"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    assert_eq!(names("tests/support", "rs"), support);
    let fixtures: BTreeSet<String> = [
        "README.md",
        "jpeg_ref.py",
        "nikon_ref.py",
        "z8-rows.bin",
        "z8-thumb.jpg",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let actual: BTreeSet<String> = std::fs::read_dir(root().join("tests/fixtures"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(actual, fixtures);
}

#[test]
fn the_crate_forbids_unsafe_and_includes_nothing() {
    assert!(read("src/lib.rs").starts_with("#![forbid(unsafe_code)]"));
    assert!(read("src/main.rs").starts_with("#![forbid(unsafe_code)]"));
    for name in PURE.iter().chain(["lib.rs", "main.rs", "window.rs"].iter()) {
        let text = read(&format!("src/{name}")).replace("#![forbid(unsafe_code)]", "");
        assert!(!text.contains("unsafe"), "{name} names unsafe");
        assert!(!text.contains("include!"), "{name} uses include!");
        assert!(!text.contains("include_bytes!"), "{name} embeds bytes");
        assert!(!text.contains("include_str!"), "{name} embeds text");
        assert!(!text.contains("cfg_attr"), "{name} uses cfg_attr");
    }
}

#[test]
fn the_manifest_declares_the_toolkit_alone_and_joins_the_gate() {
    let manifest = read("Cargo.toml");
    assert!(manifest.contains("[workspace]\n"), "own workspace root");
    // The one sibling, in the one spelling the lock guard admits.
    assert!(
        manifest.contains("\n[dependencies]\ntd-ui = { path = \"../td-ui\" }\n"),
        "the toolkit by path"
    );
    assert_eq!(manifest.matches("path =").count(), 1, "one dependency");
    assert_eq!(manifest.matches("[dependencies]").count(), 1);
    assert!(!manifest.contains("[dev-dependencies]"));
    assert!(!manifest.contains("[build-dependencies]"));
    assert!(!manifest.contains("[target"));
    assert!(!manifest.contains("[patch"));
    assert!(!manifest.contains("[replace"));
    assert!(manifest.contains("[package.metadata.td-gate]\nclippy-all-targets = true\n"));
    // The window's native case runs under the compositor `ready` builds, and
    // binds its control socket under a root the gate's namespace must show
    // as the caller's, which the builder's trusted-root fixture supplies.
    assert!(manifest.contains("\nnative-compositor-tests = true\n"));
    assert!(manifest.contains("\ntrusted-test-root = true\n"));
    for lint in [
        "unwrap_used",
        "expect_used",
        "panic",
        "unreachable",
        "todo",
        "unimplemented",
        "indexing_slicing",
    ] {
        assert!(manifest.contains(&format!("{lint} = \"deny\"")), "{lint}");
    }
    let lock = read("Cargo.lock");
    assert_eq!(lock.matches("[[package]]").count(), 2);
    assert!(lock.contains("name = \"td-photo\""));
    assert!(lock.contains("name = \"td-ui\""));
    assert!(!lock.contains("source ="), "no registry or git source");
    assert!(
        !root().join(".cargo").exists(),
        "no crate-local cargo config"
    );
    assert_eq!(read(".gitignore"), "/target/\n");
}

#[test]
fn pure_modules_reach_no_file_environment_clock_network_or_process() {
    for name in PURE {
        let text = read(&format!("src/{name}"));
        for forbidden in [
            "std::fs",
            "std::env",
            "std::time",
            "std::net",
            "std::process",
            "std::thread::available_parallelism",
            "File::",
            "env::var",
            "Instant::",
            "SystemTime",
        ] {
            assert!(!text.contains(forbidden), "{name} names {forbidden}");
        }
    }
    // `main` is the one module that opens files and reads the clock, and it
    // bounds the read itself rather than trusting the length it was told.
    let main = read("src/main.rs");
    assert!(main.contains("fs::File::open(path)"));
    assert!(main.contains(".take(ceiling + 1)"));
    // The inline test module opts out of the two negative pins, as
    // `#[cfg(test)]` code may; the production slice, everything before
    // that module's own marker (a `#[cfg(test)]` helper earlier in the
    // file is production too), is what is held.
    let production = main
        .split_once("#[cfg(test)]\nmod tests {")
        .map_or(main.as_str(), |(production, _)| production);
    assert!(!production.contains("fs::read("), "an unbounded read");
    assert!(main.contains("Instant::now"));
    // Output is created exclusively and nothing existing is replaced.
    assert!(main.contains(".create_new(true)"));
    assert!(main.contains("fs::symlink_metadata(out).is_ok()"));
    assert!(!main.contains("File::create("), "a truncating create");
    // Publication is a link, which cannot replace. A rename is the fallback
    // for a file system without links, and the write of td-photo's own
    // files, the sidecar and the export settings, the ones it replaces,
    // through one temporary path (`replace_own`; DESIGN.md, Files).
    // Culling's move into `rejected/` is the same rule over the original:
    // linked, then its old name dropped, with the same fallback. No
    // fourth rename.
    assert!(main.contains("fs::hard_link(temporary, out)"));
    assert!(main.contains("fs::rename(temporary, out)"));
    assert!(main.contains("fs::rename(&temporary, path)"));
    assert_eq!(main.matches("fn replace_own(").count(), 1);
    assert_eq!(main.matches("replace_own(&path, &text)").count(), 1);
    assert_eq!(
        main.matches("replace_own(&path, &settings.text())").count(),
        1
    );
    assert!(main.contains("fs::hard_link(from, to)"));
    assert!(main.contains("fs::rename(from, to)"));
    assert_eq!(main.matches("fs::rename(").count(), 3);
    assert_eq!(main.matches("fn move_file(").count(), 1);
    assert!(main.contains("move_rejects_with(roll, &mut move_file)"));
    assert_eq!(main.matches("fn move_rejects_with(").count(), 1);
    assert!(!main.contains("println!"), "a panicking print");
    // The window opens no file and reads no clock of its own: its
    // thumbnails come from `main`'s rule on the pool's threads, its time is
    // the turn clock the toolkit hands it, and its threads are named and
    // joined when it closes; `main` spawns none of its own.
    let window = read("src/window.rs");
    for forbidden in [
        "std::fs",
        "fs::",
        "File::",
        "std::process",
        "Instant::",
        "SystemTime",
        "println!",
    ] {
        assert!(!window.contains(forbidden), "window.rs names {forbidden}");
    }
    assert!(window.contains("thread::Builder::new()"));
    assert!(window.contains("thread.join()"));
    assert!(!window.contains("thread::spawn("), "an unnamed spawn");
    assert!(
        !main.contains("thread::spawn") && !main.contains("thread::Builder"),
        "main.rs spawns"
    );
    // Only `develop` spreads work across threads, with scoped threads
    // that cannot outlive the call.
    for name in PURE {
        let text = read(&format!("src/{name}"));
        let spawns = text.matches("thread::").count();
        if *name == "develop.rs" {
            assert!(spawns > 0);
            assert!(!text.contains("thread::spawn("), "unscoped spawn");
            assert!(text.contains("thread::scope"));
        } else {
            assert_eq!(spawns, 0, "{name} uses threads");
        }
    }
}

/// Which files may name which toolkit modules (td-ui/DESIGN.md, Public
/// surface): the controller the pure seam, raster, chrome and control;
/// `main` the seam, the replay runner, the raster's surface and control;
/// `window` the client, the wire, the display, the font, the pointer, the
/// socket and its worker, the seam, the raster and control. A braced group
/// after the crate's path would read as no name, so the scanner refuses
/// one: name one item per line.
#[test]
fn the_toolkit_is_named_only_where_the_design_says() {
    fn modules(text: &str) -> BTreeSet<String> {
        text.match_indices("td_ui::")
            .map(|(at, _)| {
                let name = text[at + 7..]
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .collect::<String>();
                assert!(
                    !name.is_empty(),
                    "td_ui:: at byte {at} is followed by no name; name one item per line"
                );
                name
            })
            .collect()
    }
    let set = |names: &[&str]| names.iter().map(|s| s.to_string()).collect::<BTreeSet<_>>();
    for name in PURE.iter().chain(["lib.rs"].iter()) {
        let text = read(&format!("src/{name}"));
        let expected = if *name == "ui.rs" {
            set(&[
                "CELL_HEIGHT",
                "CELL_WIDTH",
                "chrome",
                "control",
                "driven",
                "finder",
                "raster",
            ])
        } else {
            set(&[])
        };
        assert_eq!(modules(&text), expected, "{name}");
    }
    assert_eq!(
        modules(&read("src/main.rs")),
        set(&["control", "driven", "finder", "raster", "replay"])
    );
    assert_eq!(
        modules(&read("src/window.rs")),
        set(&[
            "client",
            "control",
            "control_socket",
            "control_worker",
            "driven",
            "font",
            "keyboard",
            "pointer",
            "raster",
            "wayland",
            "wire",
        ])
    );
}

#[test]
fn budgets_are_the_documented_values() {
    assert_eq!(td_photo::tiff::MAX_FILE_BYTES, 512 << 20);
    assert_eq!(td_photo::tiff::MAX_IFDS, 64);
    assert_eq!(td_photo::tiff::MAX_CHAIN, 16);
    assert_eq!(td_photo::tiff::MAX_ENTRIES, 4096);
    assert_eq!(td_photo::ui::MAX_PHOTOS, 100_000);
    assert_eq!(td_photo::ui::MAX_SIDECAR_TOTAL, 64 << 20);
    assert_eq!(td_photo::ui::THUMB_CACHE_BYTES, 256 << 20);
    assert_eq!(td_photo::ui::MAX_WAIT_MS, 4_000);
    assert_eq!(td_photo::look::MAX_LOOK_BYTES, 4096);
    assert_eq!(td_photo::look::MAX_OPERATIONS, 16);
    assert_eq!(td_photo::look::MAX_CURVE_POINTS, 16);
    assert_eq!(td_photo::look::MAX_NAME, 64);
    assert_eq!(td_photo::look::HEADER, "td-photo look 1");
    assert_eq!(td_photo::ui::CONTROL_JOBS_PER_TURN, 8);
    // The exposure steps develop mode nudges by: a third of a stop and a
    // tenth, in hundredths.
    assert_eq!(td_photo::ui::EXPOSURE_STEP, 33);
    assert_eq!(td_photo::ui::EXPOSURE_FINE, 10);
    assert_eq!(
        (td_photo::ui::THUMB_WIDTH, td_photo::ui::THUMB_HEIGHT),
        (160, 120)
    );
    assert_eq!(td_photo::nef::MAX_AXIS, 16384);
    assert_eq!(td_photo::image::MAX_AXIS, td_photo::nef::MAX_AXIS);
    assert_eq!(td_photo::image::MAX_IMAGE_PIXELS, 64 << 20);
    assert_eq!(td_photo::nef::MAX_RANGE, 32768);
    assert_eq!(td_photo::jpeg::MAX_PREVIEW_SAMPLES, 128 << 20);
    assert_eq!(td_photo::jpeg::MAX_TABLE_DEFINITIONS, 32);
    assert_eq!(td_photo::jpeg::QUALITY, 92);
    // The cache unlinks only names of its own shape inside directories of
    // its own, never a tree; fills go through per-process temporaries; a
    // thumbnail is turned like a development.
    let main = read("src/main.rs");
    assert!(main.contains("fn is_cache_name("));
    assert!(main.contains("if !is_cache_name(name)"));
    let production = main
        .split_once("#[cfg(test)]\nmod tests {")
        .map_or(main.as_str(), |(production, _)| production);
    assert!(!production.contains("remove_dir"), "a directory removal");
    assert!(main.contains("fn own_dir("));
    assert!(main.contains("XDG_CACHE_HOME"));
    assert!(main.contains(".take(MAX_THUMB_FILE_BYTES + 1)"));
    assert!(main.contains("fn write_via("));
    assert!(main.contains(".ppm.{}.tmp\", std::process::id()"));
    assert!(main.contains("develop::orient(image, nef.orientation)"));
    // Photo pixels reach a frame through `ui::blit` alone: the window names
    // it and writes no frame bytes itself; the verb and the window share
    // one thumbnail rule, so the cache holds one thing under one key.
    assert_eq!(read("src/ui.rs").matches("pub fn blit(").count(), 1);
    let window = read("src/window.rs");
    assert!(window.contains("ui::blit("));
    // The wait ceiling (pinned above) sits under the transport's five-second
    // deadline per request, the quit grace covers the seam's largest reply,
    // and one of the worker's connections stays free of held waits.
    assert!(window.contains("const QUIT_GRACE_MS: u64 = 500;"));
    assert!(window.contains("const MAX_WAITERS: usize = CONNECTIONS - 1;"));
    for write in ["copy_from_slice", "as_chunks", "chunks_exact", "pixels["] {
        assert!(
            !window.contains(write),
            "window.rs writes frame bytes: {write}"
        );
    }
    assert_eq!(main.matches("fn make_thumbnail(").count(), 1);
    assert_eq!(main.matches("jpeg::thumbnail(").count(), 1);
    assert!(window.contains("make_thumbnail(") && !window.contains("jpeg::"));
    // Export develops in bands straight into the encoder, through a
    // temporary of the export's own name published by the link rule, and
    // the encoder spreads its transform through `develop`'s bands rather
    // than threads of its own (pinned above: no `thread::` outside
    // `develop.rs`).
    assert_eq!(main.matches("fn export(path: &Path").count(), 1);
    assert_eq!(main.matches("fn export_file(").count(), 1);
    assert!(main.contains("develop::export_band("));
    assert!(main.contains("jpeg::Encoder::new("));
    // The window's export is the verb's runner on a pool thread, queued by
    // the session and handed over each turn; no decode on the turn thread.
    assert!(window.contains("crate::export_file("));
    assert!(window.contains("fn submit_exports("));
    assert!(main.contains("exports: Option<Vec<ExportRequest>>"));
    assert!(main.contains("format!(\"{stem}.jpg.tmp\")"));
    assert!(main.contains("const EXPORT_BAND_ROWS: usize = 64;"));
    assert!(main.contains("const MAX_EXPORT_NAMES: u32 = 1000;"));
    let jpeg = read("src/jpeg.rs");
    assert!(jpeg.contains("crate::develop::bands("));
    assert!(jpeg.contains("pub const QUALITY: u8 = 92;"));
    assert_eq!(td_photo::nef::MAX_RAW_SAMPLES, 128 << 20);
    assert_eq!(td_photo::nef::MAX_SUB_IFDS, 16);
    assert_eq!(td_photo::develop::MAX_THREADS, 16);
    assert_eq!(td_photo::develop::RAW_CACHE_BYTES, 512 << 20);
    assert_eq!(td_photo::nef::COMPRESSION_NIKON, 34713);
    assert_eq!(td_photo::nef::COMPRESSION_NONE, 1);
    assert_eq!(td_photo::nef::PHOTOMETRIC_CFA, 32803);
    // The design's table is the one the crate carries.
    let design = read("DESIGN.md");
    for line in ["11423 -4564 -1123", "-4816 12895  2119", "-210  1061  7282"] {
        assert!(design.contains(line), "DESIGN.md matrix row {line}");
    }
    assert!(design.contains("black\n1008, white 15892"));
}

#[test]
fn the_fixture_is_the_documented_slice() {
    let bytes = std::fs::read(root().join("tests/fixtures/z8-rows.bin")).unwrap();
    assert_eq!(bytes.len(), 18192);
    let readme = read("tests/fixtures/README.md");
    assert!(readme.contains("0x44df96cc9a8684a0"));
    assert!(readme.contains("0xe09ae870943b71be"));
    assert!(readme.contains("18192"));
    let thumb = std::fs::read(root().join("tests/fixtures/z8-thumb.jpg")).unwrap();
    assert_eq!(thumb.len(), 13063);
    assert!(thumb.starts_with(&[0xFF, 0xD8]) && thumb.ends_with(&[0xFF, 0xD9]));
    for hash in [
        "0x4012c2335efb6bfa",
        "0x15cf1df123ee575d",
        "0x164a0dbefb89395d",
        "0x7c9a6f6b601504ab",
        "0x8e6c3e050993ae30",
    ] {
        assert!(readme.contains(hash), "README lacks {hash}");
    }
    // The oracle's contract is stated where the constants live.
    let jpeg = read("src/jpeg.rs");
    assert!(jpeg.contains("floor(v + 128 + 0.5)"));
    assert!(jpeg.contains("1.402") && jpeg.contains("0.344136") && jpeg.contains("0.714136"));
    assert!(jpeg.contains("1.772"));
}
