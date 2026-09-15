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
    let output = Command::new(env!("CARGO_BIN_EXE_td-photo"))
        .arg("--preview")
        .arg(format!("{width}x{height}"))
        .arg(roll)
        .arg("--develop")
        .arg(position.to_string())
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
    let layout = td_photo::ui::Layout::new(Surface::new(800, 600, Scale::default()).unwrap());
    let r#box = layout
        .preview_box()
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
    let layout = td_photo::ui::Layout::new(Surface::new(800, 600, Scale::default()).unwrap());
    let r#box = layout
        .preview_box()
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
