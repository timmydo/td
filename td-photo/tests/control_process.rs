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

use std::io::{self, Read};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use td_test_compositor::{remaining, write_until, Directory};
use td_ui::control::{frame, Decoder};
use td_ui::raster::{Scale, Surface};

const TIMEOUT: Duration = Duration::from_secs(10);

#[path = "support/native_compositor.rs"]
mod native_compositor;

#[path = "support/synth_nef.rs"]
mod synth_nef;

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
    let dir = Directory::new("td-photo-process");
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
    // So does a contrast: the box differs from the same exposure without.
    std::fs::write(
        roll.join("DSC_0001.NEF.edit"),
        "td-photo edit 1\nexposure 1.50\ncontrast 1.00\n",
    )
    .unwrap();
    let steeper = preview_develop(&dir, 800, 600, &roll, 0);
    assert_ne!(
        box_pixels(&brighter, 800, r#box),
        box_pixels(&steeper, 800, r#box),
        "the contrast did not reach the developed pixels"
    );
    // And a tonal region: the shadows lifted over the same exposure.
    std::fs::write(
        roll.join("DSC_0001.NEF.edit"),
        "td-photo edit 1\nexposure 1.50\nshadows 1.00\n",
    )
    .unwrap();
    let lifted = preview_develop(&dir, 800, 600, &roll, 0);
    assert_ne!(
        box_pixels(&brighter, 800, r#box),
        box_pixels(&lifted, 800, r#box),
        "the shadows did not reach the developed pixels"
    );
    // And clarity.
    std::fs::write(
        roll.join("DSC_0001.NEF.edit"),
        "td-photo edit 1\nexposure 1.50\nclarity 1.00\n",
    )
    .unwrap();
    let clearer = preview_develop(&dir, 800, 600, &roll, 0);
    assert_ne!(
        box_pixels(&brighter, 800, r#box),
        box_pixels(&clearer, 800, r#box),
        "the clarity did not reach the developed pixels"
    );
    // And a look's intensity: half the mono look is neither the colour nor
    // the whole look's grey.
    let with_look = |intensity: &str| {
        std::fs::write(
            roll.join("DSC_0001.NEF.edit"),
            format!("td-photo edit 1\nexposure 1.50\nlook mono\n{intensity}"),
        )
        .unwrap();
        preview_develop(&dir, 800, 600, &roll, 0)
    };
    let whole = with_look("");
    let half = with_look("look-intensity 0.50\n");
    let more = with_look("look-intensity 1.50\n");
    assert_ne!(
        box_pixels(&more, 800, r#box),
        box_pixels(&whole, 800, r#box),
        "an intensity past the whole did not reach the developed pixels"
    );
    for other in [&brighter, &whole] {
        assert_ne!(
            box_pixels(other, 800, r#box),
            box_pixels(&half, 800, r#box),
            "the look intensity did not reach the developed pixels"
        );
    }
    // A default look stands in for a sidecar that sets none, and a
    // sidecar's `look none` keeps it out.
    let config = dir.0.join("config");
    std::fs::create_dir_all(config.join("td-photo")).unwrap();
    std::fs::write(
        config.join("td-photo/default-look"),
        "td-photo default-look 1\nlook mono\n",
    )
    .unwrap();
    let with_default = |sidecar: &str| {
        std::fs::write(roll.join("DSC_0001.NEF.edit"), sidecar).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_td-photo"))
            .args(["--preview", "800x600"])
            .arg(&roll)
            .args(["--develop", "0"])
            .env_clear()
            .env("XDG_CACHE_HOME", dir.0.join("cache"))
            .env("XDG_CONFIG_HOME", &config)
            .output()
            .unwrap();
        assert!(output.status.success());
        output.stdout[b"P6\n800 600\n255\n".len()..].to_vec()
    };
    assert_eq!(
        box_pixels(
            &with_default("td-photo edit 1\nexposure 1.50\n"),
            800,
            r#box
        ),
        box_pixels(&whole, 800, r#box),
        "the default look did not stand in"
    );
    assert_eq!(
        box_pixels(
            &with_default("td-photo edit 1\nexposure 1.50\nlook none\n"),
            800,
            r#box
        ),
        box_pixels(&brighter, 800, r#box),
        "look none did not keep the default out"
    );
    // The export develops with the default too: mono, so grey.
    std::fs::write(
        roll.join("DSC_0001.NEF.edit"),
        "td-photo edit 1\nexposure 1.50\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_td-photo"))
        .arg("export")
        .arg(roll.join("DSC_0001.NEF"))
        .env_clear()
        .env("XDG_CACHE_HOME", dir.0.join("cache"))
        .env("XDG_CONFIG_HOME", &config)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let exported = std::fs::read_dir(roll.join("exported"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.extension().is_some_and(|e| e == "jpg"))
        .unwrap();
    let image = td_jpeg::decode(&std::fs::read(exported).unwrap(), td_jpeg::Scale::Full).unwrap();
    assert!(
        image
            .data
            .chunks(3)
            .all(|p| p[0].abs_diff(p[1]) <= 2 && p[1].abs_diff(p[2]) <= 2),
        "the default look did not reach the export"
    );
}

#[test]
fn preview_develop_reflects_the_crop() {
    // A crop written to the sidecar changes the develop box the same way an
    // exposure does: `--preview --develop` develops the cropped region, so the
    // shared crop the window, the verb and the preview read reaches the pixels.
    let dir = Directory::new("td-photo-process");
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
    let dir = Directory::new("td-photo-process");
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
    let dir = Directory::new("td-photo-process");
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
/// encoder td-photo exports with, td-jpeg's.
fn flat_jpeg(width: usize, height: usize, rgb: [u8; 3]) -> Vec<u8> {
    let mut encoder = td_jpeg::Encoder::new(width, height, td_photo::jpeg::QUALITY).unwrap();
    let rows: Vec<u8> = rgb
        .iter()
        .copied()
        .cycle()
        .take(width * height * 3)
        .collect();
    encoder.encode_rows(&rows, &td_jpeg::InOrder).unwrap();
    encoder.finish(&td_jpeg::InOrder).unwrap()
}

/// A baseline JPEG of a diagonal ramp, for a preview with every tone.
fn ramp_jpeg(width: usize, height: usize) -> Vec<u8> {
    let mut encoder = td_jpeg::Encoder::new(width, height, td_photo::jpeg::QUALITY).unwrap();
    let rows: Vec<u8> = (0..width * height)
        .flat_map(|i| {
            let v = ((i % width + i / width) * 255 / (width + height)) as u8;
            [v, v / 2 + 40, 255 - v]
        })
        .collect();
    encoder.encode_rows(&rows, &td_jpeg::InOrder).unwrap();
    encoder.finish(&td_jpeg::InOrder).unwrap()
}

#[test]
fn preview_develop_compares_the_cameras_jpeg() {
    // The embedded preview a flat colour unlike the develop: with
    // --compare the box holds that colour, without it the develop.
    let dir = Directory::new("td-photo-compare");
    let roll = dir.0.join("roll");
    std::fs::create_dir_all(&roll).unwrap();
    let (w, h) = (16usize, 12usize);
    let samples: Vec<u16> = (0..w * h).map(|i| 1008 + (i as u16 % 4000)).collect();
    let colour = [0x20u8, 0xc0, 0x40];
    std::fs::write(
        roll.join("DSC_0000.NEF"),
        synth_nef::nef_with_preview(w, h, &samples, &flat_jpeg(w, h, colour)),
    )
    .unwrap();
    let r#box = binary_layout(800, 600)
        .develop_box()
        .expect("a develop box on an 800x600 surface");
    let developed = box_pixels(&preview_develop(&dir, 800, 600, &roll, 0), 800, r#box);
    let compared = box_pixels(
        &preview_develop_args(&dir, 800, 600, &roll, 0, &["--compare"]),
        800,
        r#box,
    );
    let near = |p: &[u8]| p.iter().zip(colour).all(|(a, b)| a.abs_diff(b) <= 4);
    // The 16 by 12 preview is never enlarged: it sits in the box.
    let held = compared.chunks(3).filter(|p| near(p)).count();
    assert!(held >= 16 * 12 / 2, "{held}");
    assert_eq!(developed.chunks(3).filter(|p| near(p)).count(), 0);
    // Refused without --develop, and with --single.
    let bin = env!("CARGO_BIN_EXE_td-photo");
    for args in [
        vec!["--preview", "800x600", roll.to_str().unwrap(), "--compare"],
        vec![
            "--preview",
            "800x600",
            roll.to_str().unwrap(),
            "--single",
            "--compare",
        ],
    ] {
        let output = std::process::Command::new(bin)
            .args(&args)
            .env("XDG_CONFIG_HOME", dir.0.join("config"))
            .output()
            .unwrap();
        assert!(!output.status.success(), "{args:?}");
        assert!(String::from_utf8_lossy(&output.stderr).contains("--compare needs --develop"));
    }
}

#[test]
fn match_look_writes_the_cameras_look_once() {
    let dir = Directory::new("td-photo-match");
    let (roll, config) = (dir.0.join("roll"), dir.0.join("config"));
    std::fs::create_dir_all(&roll).unwrap();
    let (w, h) = (16usize, 12usize);
    for i in 0..3u16 {
        let samples: Vec<u16> = (0..w * h)
            .map(|j| 1008 + (j as u16 * 37 + i * 500) % 9000)
            .collect();
        std::fs::write(
            roll.join(format!("DSC_000{i}.NEF")),
            synth_nef::nef_with_preview(w, h, &samples, &ramp_jpeg(w, h)),
        )
        .unwrap();
    }
    let run = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_td-photo"))
            .args(args)
            .env("XDG_CONFIG_HOME", &config)
            .env("XDG_CACHE_HOME", dir.0.join("cache"))
            .output()
            .unwrap();
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    };
    let roll_s = roll.to_str().unwrap();
    let (ok, stdout, stderr) = run(&["match-look", roll_s]);
    assert!(ok, "{stderr}");
    assert!(stdout.contains("from 3 photos"), "{stdout}");
    let written = config.join("td-photo/looks/camera-match.look");
    let look = td_photo::look::Look::parse(&std::fs::read(&written).unwrap()).unwrap();
    assert_eq!(look.name(), Some("Camera match"));
    // Never over a look already there, which is left as it was.
    let before = std::fs::read(&written).unwrap();
    let (ok, _, stderr) = run(&["match-look", roll_s]);
    assert!(!ok);
    assert!(stderr.contains("already there"), "{stderr}");
    assert_eq!(std::fs::read(&written).unwrap(), before);
    assert!(run(&["match-look", roll_s, "--name", "z8-standard"]).0);
    let named = config.join("td-photo/looks/z8-standard.look");
    let look = td_photo::look::Look::parse(&std::fs::read(&named).unwrap()).unwrap();
    assert_eq!(look.name(), Some("Camera match (z8-standard)"));
    // The temporary is gone, written or not.
    let leftovers: Vec<_> = std::fs::read_dir(config.join("td-photo/looks"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .filter(|name| name.to_string_lossy().ends_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
    let (ok, _, stderr) = run(&["match-look", roll_s, "--name", ".hidden"]);
    assert!(!ok);
    assert!(stderr.contains("not a look stem"), "{stderr}");
    let (ok, stdout, _) = run(&["looks"]);
    assert!(
        ok && stdout.contains("camera-match\tuser\tCamera match"),
        "{stdout}"
    );
    // Previews that do not decode are skipped, each noted, and none is
    // nothing to match.
    let bare = dir.0.join("bare");
    std::fs::create_dir_all(&bare).unwrap();
    let samples: Vec<u16> = (0..w * h).map(|j| 1008 + j as u16 * 20).collect();
    std::fs::write(
        bare.join("DSC_0001.NEF"),
        synth_nef::uncompressed_nef(w, h, &samples),
    )
    .unwrap();
    let (ok, _, stderr) = run(&["match-look", bare.to_str().unwrap(), "--name", "none-here"]);
    assert!(!ok);
    assert!(stderr.contains("skipped DSC_0001.NEF"), "{stderr}");
    assert!(
        stderr.contains("no photo with both a preview and a raw"),
        "{stderr}"
    );
    assert!(!config.join("td-photo/looks/none-here.look").exists());
    // A looks directory that cannot be made is refused before any
    // develop: over the roll whose photos are each skipped with a note,
    // none is.
    let blocked = dir.0.join("blocked");
    std::fs::write(&blocked, b"a file").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_td-photo"))
        .args(["match-look", bare.to_str().unwrap()])
        .env("XDG_CONFIG_HOME", &blocked)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(!stderr.contains("skipped"), "{stderr}");
    assert!(stderr.contains("blocked"), "{stderr}");
}

/// `td-photo --replay ARGS` over `requests` under `config`, closed when
/// they are written: each reply's fields after `1`, and stderr.
fn replay_under(config: &Path, args: &[&str], requests: &[&[&str]]) -> (Vec<Vec<String>>, String) {
    use std::io::Write;
    let mut child = Command::new(env!("CARGO_BIN_EXE_td-photo"))
        .arg("--replay")
        .args(args)
        .env("XDG_CONFIG_HOME", config)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    {
        let mut stdin = child.stdin.take().unwrap();
        for (id, words) in requests.iter().enumerate() {
            let line = format!("1\t{}\t{}", id + 1, words.join("\t"));
            stdin.write_all(&frame(line.as_bytes()).unwrap()).unwrap();
        }
    }
    let output = child.wait_with_output().unwrap();
    let mut decoder = Decoder::default();
    let mut replies = Vec::new();
    for byte in &output.stdout {
        decoder.push(&[*byte]).unwrap();
        if decoder.payload().is_some() {
            let payload = std::mem::take(&mut decoder).finish().unwrap();
            let line = String::from_utf8(payload).unwrap();
            replies.push(line.split('\t').skip(1).map(str::to_string).collect());
        }
    }
    (
        replies,
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// The window's `match-look` fits the open roll as the verb does, into
/// `camera-match` and then the next free stem, never over a look there,
/// and lists the look it wrote; a roll with nothing to match is refused
/// with no look written.
#[test]
fn the_window_fits_a_camera_match_under_a_free_stem() {
    let dir = Directory::new("td-photo-window-match");
    let (roll, config) = (dir.0.join("roll"), dir.0.join("config"));
    std::fs::create_dir_all(&roll).unwrap();
    let (w, h) = (16usize, 12usize);
    for i in 0..2u16 {
        let samples: Vec<u16> = (0..w * h)
            .map(|j| 1008 + (j as u16 * 37 + i * 500) % 9000)
            .collect();
        std::fs::write(
            roll.join(format!("DSC_000{i}.NEF")),
            synth_nef::nef_with_preview(w, h, &samples, &ramp_jpeg(w, h)),
        )
        .unwrap();
    }
    let looks = config.join("td-photo/looks");
    let roll_s = roll.to_str().unwrap();
    // The session lists the look it wrote: picked by its place among
    // the looks, the band's order, which only a reported list holds.
    let mut stems: Vec<&str> = td_photo::look::BUILTIN
        .iter()
        .map(|(stem, _)| *stem)
        .chain(["camera-match"])
        .collect();
    stems.sort_unstable();
    let place = stems.iter().position(|s| *s == "camera-match").unwrap() + 1;
    assert!(place <= 9, "{stems:?}");
    let pick = format!("look-{place}");
    let (replies, stderr) = replay_under(
        &config,
        &[roll_s],
        &[
            &["action", "develop"],
            &["action", "match-look"],
            &["action", &pick],
            &["photo", "0"],
            &["action", "match-look"],
        ],
    );
    assert_eq!(replies[1], ["2", "ok", "changed"], "{stderr}");
    assert_eq!(replies[2], ["3", "ok", "changed"], "{replies:?}");
    assert!(
        replies[3].iter().any(|field| field == "camera-match"),
        "{replies:?}"
    );
    assert_eq!(replies[4], ["5", "ok", "changed"], "{stderr}");
    for (stem, name) in [
        ("camera-match", "Camera match"),
        ("camera-match-2", "Camera match (camera-match-2)"),
    ] {
        let text = std::fs::read(looks.join(format!("{stem}.look"))).unwrap();
        let look = td_photo::look::Look::parse(&text).unwrap();
        assert_eq!(look.name(), Some(name));
    }
    // Nothing to match: refused, no third look.
    let bare = dir.0.join("bare");
    std::fs::create_dir_all(&bare).unwrap();
    let samples: Vec<u16> = (0..w * h).map(|j| 1008 + j as u16 * 20).collect();
    std::fs::write(
        bare.join("DSC_0001.NEF"),
        synth_nef::uncompressed_nef(w, h, &samples),
    )
    .unwrap();
    let (replies, stderr) = replay_under(
        &config,
        &[bare.to_str().unwrap()],
        &[&["action", "match-look"]],
    );
    assert_eq!(replies[0][..3], ["1", "error", "refused"], "{stderr}");
    assert!(
        stderr.contains("no photo with both a preview and a raw"),
        "{stderr}"
    );
    assert!(!looks.join("camera-match-3.look").exists());
}

#[test]
fn preview_develop_blits_the_filmstrip_thumbnails() {
    // Three decodable NEFs whose embedded previews are flat colours: in
    // develop the strip under the box carries each photo's thumbnail, in
    // roll order around the cursor, and the cursor's box is outlined.
    let dir = Directory::new("td-photo-process");
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
