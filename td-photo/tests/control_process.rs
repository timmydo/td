//! The td-photo window under the real headless td-compositor: a render and
//! transport oracle, not a wire fixture. `ready` supplies the disposable
//! compositor through `TD_TEST_COMPOSITOR`; the native case launches the
//! window on a roll with its control socket, waits for its toplevel to
//! present, drives it over the socket and the seat, and holds the captured
//! pixels to the crate's own `--preview` of the same roll.
//!
//! The file is named `control_process` and mounts `native_compositor` so the
//! gate's native runner (`builder/src/native_tests.rs`), which hardcodes
//! `--test control_process` filtered to `native_compositor::`, discovers it.
#![forbid(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use td_ui::control::{frame, Decoder};
use td_ui::raster::{Scale, Surface};

type Result<T> = std::result::Result<T, String>;
const TIMEOUT: Duration = Duration::from_secs(10);
static NEXT: AtomicU64 = AtomicU64::new(0);

#[path = "support/native_compositor.rs"]
mod native_compositor;

#[path = "support/synth_nef.rs"]
mod synth_nef;

/// A short-lived private directory for a compositor session or a client's
/// runtime, at a Linux-socket-length path independent of TMPDIR.
struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let path = Path::new("/tmp").join(format!(
            "td-photo-process-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        Self(path)
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn remaining(deadline: Instant) -> io::Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|duration| !duration.is_zero())
        .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "native I/O deadline"))
}

fn write_until(stream: &mut UnixStream, mut bytes: &[u8], deadline: Instant) -> io::Result<()> {
    while !bytes.is_empty() {
        stream.set_write_timeout(Some(remaining(deadline)?))?;
        match stream.write(bytes) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => bytes = &bytes[n..],
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

// ------------------------------------------------- headless develop preview

/// `td-photo --preview WxH ROLL --develop POSITION`, the RGB rows of its PPM,
/// under a private cache. The developed frame the window shows in develop
/// mode with its cursor at `position`, made on the calling thread with no
/// compositor, so this runs in the ordinary gate.
fn preview_develop(
    dir: &Directory,
    width: usize,
    height: usize,
    roll: &Path,
    position: usize,
) -> Vec<u8> {
    preview_develop_args(dir, width, height, roll, position, &[])
}

/// `preview_develop` with `extra` arguments after `--develop POSITION`.
fn preview_develop_args(
    dir: &Directory,
    width: usize,
    height: usize,
    roll: &Path,
    position: usize,
    extra: &[&str],
) -> Vec<u8> {
    let output = Command::new(env!("CARGO_BIN_EXE_td-photo"))
        .arg("--preview")
        .arg(format!("{width}x{height}"))
        .arg(roll)
        .arg("--develop")
        .arg(position.to_string())
        .args(extra)
        .env_clear()
        .env("XDG_CACHE_HOME", dir.0.join("cache"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let header = format!("P6\n{width} {height}\n255\n");
    output
        .stdout
        .strip_prefix(header.as_bytes())
        .expect("preview PPM header")
        .to_vec()
}

/// The pixels within `rect` on a `width`-wide RGB frame, row by row: the
/// develop box alone, so a comparison sees the developed image and not the
/// facts line, whose exposure text changes on its own.
fn box_pixels(pixels: &[u8], width: usize, rect: td_ui::raster::Rect) -> Vec<u8> {
    let (bx, by) = (rect.x as usize, rect.y as usize);
    let (bw, bh) = (rect.width as usize, rect.height as usize);
    let mut out = Vec::with_capacity(bw * bh * 3);
    for y in by..by + bh {
        let start = (y * width + bx) * 3;
        out.extend_from_slice(&pixels[start..start + bw * 3]);
    }
    out
}

/// Whether the pixels within `rect` are not all one colour, so an image, not
/// the flat placeholder, fills the box.
/// The layout the binary lays on a surface of that size: its look band
/// holds the built-in looks (the preview runs with a home that has no
/// user looks), which wrap on the region's width and place the box.
fn binary_layout(width: usize, height: usize) -> td_photo::ui::Layout {
    let mut stems: Vec<String> = td_photo::look::BUILTIN
        .iter()
        .map(|(stem, _)| stem.to_string())
        .collect();
    stems.sort();
    td_photo::ui::Layout::with_looks(
        Surface::new(width, height, Scale::default()).unwrap(),
        &stems,
    )
}

fn varies(pixels: &[u8], width: usize, rect: td_ui::raster::Rect) -> bool {
    let region = box_pixels(pixels, width, rect);
    let (chunks, _) = region.as_chunks::<3>();
    match chunks.first() {
        Some(first) => chunks.iter().any(|px| px != first),
        None => false,
    }
}

#[test]
fn preview_develop_reflects_the_sidecar() {
    // A synthesized decodable NEF developed through `--preview --develop`:
    // the develop box carries pixels, and an exposure written to the sidecar
    // changes them, so the developed preview reflects the sidecar the window,
    // the verb and the preview share.
    let dir = Directory::new();
    let roll = dir.0.join("roll");
    std::fs::create_dir(&roll).unwrap();
    let (w, h) = (64usize, 48usize);
    let samples: Vec<u16> = (0..w * h).map(|i| 1008 + (i as u16 % 4000)).collect();
    std::fs::write(
        roll.join("DSC_0001.NEF"),
        synth_nef::uncompressed_nef(w, h, &samples),
    )
    .unwrap();

    let base = preview_develop(&dir, 800, 600, &roll, 0);
    let layout = binary_layout(800, 600);
    let r#box = layout
        .develop_box()
        .expect("a develop box on an 800x600 surface");
    assert!(
        varies(&base, 800, r#box),
        "the develop box carries no developed image"
    );

    std::fs::write(
        roll.join("DSC_0001.NEF.edit"),
        "td-photo edit 1\nexposure 1.50\n",
    )
    .unwrap();
    let brighter = preview_develop(&dir, 800, 600, &roll, 0);
    // The develop box itself changes, not merely the facts line's exposure
    // text: the exposure reached the developed pixels.
    assert_ne!(
        box_pixels(&base, 800, r#box),
        box_pixels(&brighter, 800, r#box),
        "the exposure did not reach the developed pixels"
    );
}

#[test]
fn preview_develop_reflects_the_crop() {
    // A crop written to the sidecar changes the develop box the same way an
    // exposure does: `--preview --develop` develops the cropped region, so the
    // shared crop the window, the verb and the preview read reaches the pixels.
    let dir = Directory::new();
    let roll = dir.0.join("roll");
    std::fs::create_dir(&roll).unwrap();
    let (w, h) = (64usize, 48usize);
    let samples: Vec<u16> = (0..w * h).map(|i| 1008 + (i as u16 % 4000)).collect();
    std::fs::write(
        roll.join("DSC_0001.NEF"),
        synth_nef::uncompressed_nef(w, h, &samples),
    )
    .unwrap();

    let base = preview_develop(&dir, 800, 600, &roll, 0);
    let layout = binary_layout(800, 600);
    let r#box = layout
        .develop_box()
        .expect("a develop box on an 800x600 surface");
    assert!(varies(&base, 800, r#box), "no developed image to crop");

    std::fs::write(
        roll.join("DSC_0001.NEF.edit"),
        "td-photo edit 1\ncrop 0.2500 0.2500 0.5000 0.5000\n",
    )
    .unwrap();
    let cropped = preview_develop(&dir, 800, 600, &roll, 0);
    // The develop box itself changes: the crop reached the developed pixels.
    assert_ne!(
        box_pixels(&base, 800, r#box),
        box_pixels(&cropped, 800, r#box),
        "the crop did not reach the developed pixels"
    );
}

#[test]
fn preview_single_shows_the_developed_photo_in_the_single_view() {
    // The cull single view develops the cursor photo into its box as
    // develop does, its sidecar's edits applied: `--preview --single` of
    // a decodable NEF carries an image in the single view's box (the
    // larger one over the whole area), and an exposure written to the
    // sidecar changes it.
    let dir = Directory::new();
    let roll = dir.0.join("roll");
    std::fs::create_dir(&roll).unwrap();
    let (w, h) = (64usize, 48usize);
    let samples: Vec<u16> = (0..w * h).map(|i| 1008 + (i as u16 % 4000)).collect();
    std::fs::write(
        roll.join("DSC_0001.NEF"),
        synth_nef::uncompressed_nef(w, h, &samples),
    )
    .unwrap();
    let single = |dir: &Directory| {
        let output = Command::new(env!("CARGO_BIN_EXE_td-photo"))
            .args(["--preview", "800x600"])
            .arg(&roll)
            .args(["--single", "0"])
            .env_clear()
            .env("XDG_CACHE_HOME", dir.0.join("cache"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
            .stdout
            .strip_prefix(b"P6\n800 600\n255\n")
            .expect("preview PPM header")
            .to_vec()
    };
    let layout = binary_layout(800, 600);
    let r#box = layout
        .preview_box()
        .expect("a single-view box on an 800x600 surface");
    assert_ne!(Some(r#box), layout.develop_box());
    let base = single(&dir);
    assert!(
        varies(&base, 800, r#box),
        "the single view's box is a placeholder"
    );
    std::fs::write(
        roll.join("DSC_0001.NEF.edit"),
        "td-photo edit 1\nexposure 1.50\n",
    )
    .unwrap();
    let brighter = single(&dir);
    assert_ne!(
        box_pixels(&base, 800, r#box),
        box_pixels(&brighter, 800, r#box),
        "the exposure did not reach the single view's pixels"
    );
    // Without a position the cursor's photo, the first: the same frame.
    let cursors = Command::new(env!("CARGO_BIN_EXE_td-photo"))
        .args(["--preview", "800x600"])
        .arg(&roll)
        .arg("--single")
        .env_clear()
        .env("XDG_CACHE_HOME", dir.0.join("cache"))
        .output()
        .unwrap();
    assert!(cursors.status.success());
    assert_eq!(
        cursors.stdout.strip_prefix(b"P6\n800 600\n255\n"),
        Some(brighter.as_slice())
    );
    // `--zoom` is develop's, the two views are one flag either way round,
    // a position past the roll or past usize is refused under the flag's
    // name, and the view needs a roll.
    let wide = format!("{}0", usize::MAX);
    for (args, message) in [
        (["--single", "0", "--zoom"].as_slice(), "--zoom"),
        (["--single", "--develop"].as_slice(), "only once"),
        (["--develop", "--single"].as_slice(), "only once"),
        (["--single", "1"].as_slice(), "--single 1: no-photo"),
        (["--single", wide.as_str()].as_slice(), "--single POSITION"),
    ] {
        let refused = Command::new(env!("CARGO_BIN_EXE_td-photo"))
            .args(["--preview", "800x600"])
            .arg(&roll)
            .args(args)
            .env_clear()
            .output()
            .unwrap();
        assert!(!refused.status.success(), "{args:?}");
        let stderr = String::from_utf8_lossy(&refused.stderr);
        assert!(stderr.contains(message), "{args:?}: {stderr}");
    }
    let rollless = Command::new(env!("CARGO_BIN_EXE_td-photo"))
        .args(["--preview", "800x600", "--single"])
        .env_clear()
        .output()
        .unwrap();
    assert!(!rollless.status.success());
    assert!(String::from_utf8_lossy(&rollless.stderr).contains("needs a ROLL"));
}

#[test]
fn preview_develop_zooms_the_box_to_the_centre_at_100() {
    // A 1600x1200 NEF, dark but for a bright square at its centre: the
    // fit view shrinks the square with the frame, while `--zoom` shows the
    // box's window of the photosites around the centre at 100%, where the
    // square is its own size, so its pixels in the box multiply by the
    // square of the fit's reduction (about a fifth on an 800x600 surface,
    // whose box the built-in looks' rows shorten), and a fit that never
    // enlarges tells the two apart. The square renders near white with a
    // magenta cast, which the box's placeholder and the chrome, warmer
    // and darker in blue, do not reach.
    let dir = Directory::new();
    let roll = dir.0.join("roll");
    std::fs::create_dir(&roll).unwrap();
    let (w, h) = (1600usize, 1200usize);
    let samples: Vec<u16> = (0..w * h)
        .map(|i| {
            let (x, y) = (i % w, i / w);
            if (750..850).contains(&x) && (550..650).contains(&y) {
                12_000
            } else {
                1100
            }
        })
        .collect();
    std::fs::write(
        roll.join("DSC_0001.NEF"),
        synth_nef::uncompressed_nef(w, h, &samples),
    )
    .unwrap();
    let layout = binary_layout(800, 600);
    let r#box = layout
        .develop_box()
        .expect("a develop box on an 800x600 surface");
    let bright = |pixels: &[u8]| {
        let region = box_pixels(pixels, 800, r#box);
        let (chunks, _) = region.as_chunks::<3>();
        chunks
            .iter()
            .filter(|px| px[0] > 240 && px[2] > 240)
            .count()
    };
    let fit = preview_develop(&dir, 800, 600, &roll, 0);
    let zoomed = preview_develop_args(&dir, 800, 600, &roll, 0, &["--zoom"]);
    assert!(varies(&fit, 800, r#box) && varies(&zoomed, 800, r#box));
    let (at_fit, at_100) = (bright(&fit), bright(&zoomed));
    assert!(at_fit > 0, "no bright square at the fit");
    assert!(
        at_100 > at_fit * 8,
        "the square is {at_fit} bright pixels at the fit and {at_100} at 100%"
    );
    // The square is a hundred pixels on a side at 100%.
    assert!((9_000..=11_000).contains(&at_100), "{at_100}");
    // `--zoom` needs `--develop` before it.
    let refused = Command::new(env!("CARGO_BIN_EXE_td-photo"))
        .args(["--preview", "800x600"])
        .arg(&roll)
        .arg("--zoom")
        .env_clear()
        .output()
        .unwrap();
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("--zoom needs --develop"));
}

/// A flat baseline JPEG of one colour, `width` by `height`, through the
/// crate's own encoder.
fn flat_jpeg(width: usize, height: usize, rgb: [u8; 3]) -> Vec<u8> {
    let mut encoder =
        td_photo::jpeg::Encoder::new(width, height, td_photo::jpeg::QUALITY, 1).unwrap();
    let rows: Vec<u8> = rgb
        .iter()
        .copied()
        .cycle()
        .take(width * height * 3)
        .collect();
    encoder.encode_rows(&rows).unwrap();
    encoder.finish().unwrap()
}

#[test]
fn preview_develop_blits_the_filmstrip_thumbnails() {
    // Three decodable NEFs whose embedded previews are flat colours: in
    // develop the strip under the box carries each photo's thumbnail, in
    // roll order around the cursor, and the cursor's box is outlined.
    let dir = Directory::new();
    let roll = dir.0.join("roll");
    std::fs::create_dir_all(&roll).unwrap();
    let (w, h) = (16usize, 12usize);
    let samples: Vec<u16> = (0..w * h).map(|i| 1008 + (i as u16 % 4000)).collect();
    let colours = [[0x20u8, 0x40, 0x60], [0x80, 0x90, 0xa0], [0xc0, 0xb0, 0xa0]];
    for (i, rgb) in colours.iter().enumerate() {
        std::fs::write(
            roll.join(format!("DSC_000{i}.NEF")),
            synth_nef::nef_with_preview(w, h, &samples, &flat_jpeg(w, h, *rgb)),
        )
        .unwrap();
    }
    let pixels = preview_develop(&dir, 800, 600, &roll, 1);
    let at = |x: usize, y: usize| {
        let i = (y * 800 + x) * 3;
        [pixels[i], pixels[i + 1], pixels[i + 2]]
    };
    let near = |a: [u8; 3], b: [u8; 3]| a.iter().zip(b).all(|(p, q)| p.abs_diff(q) <= 4);
    // The boxes at 800 by 600: from 224, 168 apart, at y 452, 160 by 120;
    // a 16 by 12 thumbnail is never enlarged, so it sits at each box's
    // middle, the placeholder around it.
    for (n, rgb) in colours.iter().enumerate() {
        let (x, y) = (224 + n * 168, 452);
        let middle = at(x + 80, y + 60);
        assert!(near(middle, *rgb), "box {n}: {middle:?} for {rgb:?}");
        let corner = at(x + 2, y + 2);
        assert!(!near(corner, *rgb), "box {n} corner is the placeholder");
    }
    // The cursor's outline in the middle box's padding.
    let selected = [
        (td_ui::raster::SELECTED >> 16) as u8,
        (td_ui::raster::SELECTED >> 8) as u8,
        td_ui::raster::SELECTED as u8,
    ];
    assert_eq!(at(224 + 168 - 3, 452 - 3), selected);
    assert_ne!(at(224 - 3, 452 - 3), selected);
    // The developed box above carries the raw's pixels, not a preview.
    let ui = td_photo::ui::Controller::new(Surface::new(800, 600, Scale::default()).unwrap());
    let r#box = ui.layout().develop_box().unwrap();
    assert!(varies(&pixels, 800, r#box));
}
