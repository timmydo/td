#![forbid(unsafe_code)]

//! The command line: `probe` prints what a file's container says and
//! optionally decodes its raw strip; `develop` renders it to a PPM. This is
//! the one place in the crate that opens files, reads the clock or asks
//! for the thread count; the library modules take bytes and buffers.

use std::ffi::OsString;
use std::fs;
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use td_photo::color::{camera_color, Transfer};
use td_photo::develop::{self, Params, MAX_THREADS};
use td_photo::image::{write_ppm, Rgb8};
use td_photo::nef::{self, Nef};
use td_photo::{camera, tiff};

const HELP: &str = concat!(
    "td-photo probe FILE [--decode]\n",
    "  Prints what the container says: body, raw geometry, codec, crop,\n",
    "  black level, white balance, exposure facts and embedded previews.\n",
    "  --decode also decodes the raw strip and prints its statistics.\n",
    "td-photo develop FILE OUT.ppm [--long-edge N] [--exposure STOPS]\n",
    "  Develops the raw to 8-bit sRGB at most N pixels on the long side\n",
    "  (default 1600) with an exposure offset in stops (default 0), written\n",
    "  through a fresh OUT.ppm.tmp and renamed into place. OUT.ppm must not\n",
    "  exist: td-photo never overwrites a file.\n",
    "Other: --help\n",
    "Supported: Nikon Z 8 14-bit lossless-compressed NEF (see DESIGN.md).\n",
);

const DEFAULT_LONG_EDGE: usize = 1600;

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let result = match args.as_slice() {
        [] => help(),
        [flag] if flag == "--help" => help(),
        [_, flag, ..] if flag == "--help" => help(),
        [verb, file, rest @ ..] if verb == "probe" => probe(Path::new(file), rest),
        [verb] if verb == "probe" => Err("probe needs FILE; see --help".to_string()),
        [verb, file, out, rest @ ..] if verb == "develop" => {
            develop_file(Path::new(file), Path::new(out), rest)
        }
        [verb, ..] if verb == "develop" => {
            Err("develop needs FILE and OUT.ppm; see --help".to_string())
        }
        _ => Err("unrecognized arguments; see --help".to_string()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            // A failed write to stderr has nowhere to be reported.
            let _ = writeln!(io::stderr().lock(), "td-photo: {message}");
            ExitCode::FAILURE
        }
    }
}

fn help() -> Result<(), String> {
    io::stdout()
        .lock()
        .write_all(HELP.as_bytes())
        .map_err(|e| e.to_string())
}

/// The value after `name` among `rest`, if present.
fn option(rest: &[OsString], name: &str) -> Result<Option<String>, String> {
    let mut iter = rest.iter();
    while let Some(arg) = iter.next() {
        if arg == name {
            let value = iter.next().ok_or_else(|| format!("{name} needs a value"))?;
            return value
                .to_str()
                .map(|s| Some(s.to_string()))
                .ok_or_else(|| format!("{name} value is not UTF-8"));
        }
    }
    Ok(None)
}

fn check_flags(
    rest: &[OsString],
    allowed_options: &[&str],
    allowed_switches: &[&str],
) -> Result<(), String> {
    let mut iter = rest.iter();
    while let Some(arg) = iter.next() {
        let text = arg.to_str().unwrap_or("");
        if allowed_options.contains(&text) {
            iter.next();
        } else if !allowed_switches.contains(&text) {
            return Err(format!("unrecognized argument {text:?}; see --help"));
        }
    }
    Ok(())
}

fn threads() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .min(MAX_THREADS)
}

/// Reads a regular file of at most `MAX_FILE_BYTES`. The ceiling bounds
/// the read itself, not only the size the file claimed before it: a file
/// that grows, or a device whose length says nothing, is refused too.
fn read_file(path: &Path) -> Result<Vec<u8>, String> {
    let named = |e: io::Error| format!("{}: {e}", path.display());
    let file = fs::File::open(path).map_err(named)?;
    let meta = file.metadata().map_err(named)?;
    if !meta.is_file() {
        return Err(format!("{}: not a regular file", path.display()));
    }
    let ceiling = tiff::MAX_FILE_BYTES as u64;
    if meta.len() > ceiling {
        return Err(format!(
            "{}: {} bytes exceeds the {} byte ceiling",
            path.display(),
            meta.len(),
            ceiling
        ));
    }
    let mut data = Vec::with_capacity(meta.len() as usize);
    (&file)
        .take(ceiling + 1)
        .read_to_end(&mut data)
        .map_err(named)?;
    if data.len() as u64 > ceiling {
        return Err(format!(
            "{}: grew past the {ceiling} byte ceiling while being read",
            path.display()
        ));
    }
    Ok(data)
}

fn describe(nef: &Nef, len: usize, out: &mut impl Write) -> io::Result<()> {
    writeln!(out, "size: {len}")?;
    writeln!(out, "make: {}", nef.make)?;
    writeln!(out, "model: {}", nef.model)?;
    match camera::find(&nef.make, &nef.model) {
        Some(c) => writeln!(out, "camera: known (black {}, white {})", c.black, c.white)?,
        None => writeln!(out, "camera: unknown (not in the table)")?,
    }
    let raw = &nef.raw;
    writeln!(
        out,
        "raw: {}x{} {}-bit compression {} cfa {} strip {}+{}",
        raw.width,
        raw.height,
        raw.bits,
        raw.compression,
        raw.cfa.name(),
        raw.strip.0,
        raw.strip.1
    )?;
    let crop = nef.crop();
    writeln!(
        out,
        "crop: {},{} {}x{}{}",
        crop.left,
        crop.top,
        crop.width,
        crop.height,
        if nef.maker.crop.is_some() {
            ""
        } else {
            " (full frame)"
        }
    )?;
    match nef.maker.black {
        Some(b) => writeln!(out, "black: {b}")?,
        None => writeln!(out, "black: (camera table)")?,
    }
    match nef.maker.wb {
        Some((r, b)) => writeln!(out, "white balance: r {r:.4} b {b:.4}")?,
        None => writeln!(out, "white balance: (daylight)")?,
    }
    if matches!(nef.orientation, 1 | 3 | 6 | 8) {
        writeln!(out, "orientation: {}", nef.orientation)?;
    } else {
        writeln!(out, "orientation: {} (treated as 1)", nef.orientation)?;
    }
    let e = &nef.exposure;
    let mut facts = Vec::new();
    if let Some((n, d)) = e.time {
        if n < d && n != 0 {
            facts.push(format!("1/{} s", d / n));
        } else if d != 0 {
            facts.push(format!("{:.1} s", n as f32 / d as f32));
        }
    }
    if let Some((n, d)) = e.aperture {
        if d != 0 {
            facts.push(format!("f/{:.1}", n as f32 / d as f32));
        }
    }
    if let Some(iso) = e.iso {
        facts.push(format!("ISO {iso}"));
    }
    if let Some((n, d)) = e.focal_length {
        if d != 0 {
            facts.push(format!("{:.0} mm", n as f32 / d as f32));
        }
    }
    writeln!(out, "exposure: {}", facts.join(" "))?;
    if let Some(taken) = &e.taken {
        writeln!(out, "taken: {taken}")?;
    }
    if let Some(lens) = &e.lens {
        writeln!(out, "lens: {lens}")?;
    }
    for p in &nef.previews {
        writeln!(out, "preview: {}+{}", p.offset, p.len)?;
    }
    match nef.maker.linearization {
        Some((offset, len)) => writeln!(out, "linearization: {offset}+{len}")?,
        None => writeln!(out, "linearization: none")?,
    }
    Ok(())
}

fn probe(path: &Path, rest: &[OsString]) -> Result<(), String> {
    check_flags(rest, &[], &["--decode"])?;
    let decode = rest.iter().any(|a| a == "--decode");
    let data = read_file(path)?;
    let nef = nef::parse(&data).map_err(|e| format!("{}: {e}", path.display()))?;
    let stdout = io::stdout();
    let mut out = stdout.lock();
    writeln!(out, "file: {}", path.display()).map_err(|e| e.to_string())?;
    describe(&nef, data.len(), &mut out).map_err(|e| e.to_string())?;
    if let (Some((offset, len)), nef::COMPRESSION_NIKON) =
        (nef.maker.linearization, nef.raw.compression)
    {
        if let Some(table) = data.get(offset..offset.saturating_add(len)) {
            match nef::HuffmanParams::parse(table, nef.raw.bits, nef.maker.endian) {
                Ok(p) => writeln!(
                    out,
                    "huffman: tree {} vpred {:?} max {} split {} curve {}",
                    p.tree,
                    p.vpred,
                    p.max,
                    p.split,
                    p.curve.as_ref().map_or(0, Vec::len)
                )
                .map_err(|e| e.to_string())?,
                Err(e) => writeln!(out, "huffman: {e}").map_err(|e| e.to_string())?,
            }
        }
    }
    if decode {
        let started = Instant::now();
        let decoded = nef::decode(&nef, &data).map_err(|e| format!("{}: {e}", path.display()))?;
        let elapsed = started.elapsed().as_millis();
        let (mut min, mut max, mut sum) = (u16::MAX, 0u16, 0u64);
        // FNV-1a over the samples as little-endian bytes, the oracle
        // tests/fixtures/README.md records for the reference frame.
        let mut hash: u64 = 0xcbf29ce484222325;
        for s in &decoded.samples {
            min = min.min(*s);
            max = max.max(*s);
            sum += u64::from(*s);
            for b in s.to_le_bytes() {
                hash ^= u64::from(b);
                hash = hash.wrapping_mul(0x100000001b3);
            }
        }
        let mean = sum as f64 / decoded.samples.len().max(1) as f64;
        writeln!(
            out,
            "decode: {} samples, {} corrupt, min {min} max {max} mean {mean:.1}, {elapsed} ms",
            decoded.samples.len(),
            decoded.corrupt
        )
        .map_err(|e| e.to_string())?;
        writeln!(out, "fnv1a64: {hash:#018x}").map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn develop_file(path: &Path, out: &Path, rest: &[OsString]) -> Result<(), String> {
    check_flags(rest, &["--long-edge", "--exposure"], &[])?;
    let long_edge = match option(rest, "--long-edge")? {
        Some(text) => text
            .parse::<usize>()
            .ok()
            .filter(|n| (16..=nef::MAX_AXIS).contains(n))
            .ok_or_else(|| format!("--long-edge {text:?} is not 16..=16384"))?,
        None => DEFAULT_LONG_EDGE,
    };
    let exposure = match option(rest, "--exposure")? {
        Some(text) => text
            .parse::<f32>()
            .ok()
            .filter(|v| v.is_finite() && (-5.0..=5.0).contains(v))
            .ok_or_else(|| format!("--exposure {text:?} is not -5..=5 stops"))?,
        None => 0.0,
    };
    // Refused before the decode, not only before the write.
    refuse_existing(out)?;
    let threads = threads();
    let started = Instant::now();
    let data = read_file(path)?;
    let nef = nef::parse(&data).map_err(|e| format!("{}: {e}", path.display()))?;
    let camera = camera::find(&nef.make, &nef.model).ok_or_else(|| {
        format!(
            "{}: unsupported camera {:?} {:?}",
            path.display(),
            nef.make,
            nef.model
        )
    })?;
    let color = camera_color(&camera.xyz_to_cam).map_err(|e| e.to_string())?;
    let decoded = nef::decode(&nef, &data).map_err(|e| format!("{}: {e}", path.display()))?;
    let decoded_at = started.elapsed().as_millis();
    let black = nef.maker.black.unwrap_or(camera.black);
    let level1 = develop::superpixel(
        &decoded,
        nef.raw.cfa,
        nef.crop(),
        black,
        camera.white,
        threads,
    )
    .map_err(|e| e.to_string())?;
    let wb = match nef.maker.wb {
        Some((r, b)) => [r, 1.0, b],
        None => color.daylight,
    };
    let image = develop::render(
        &level1,
        long_edge,
        nef.orientation,
        wb,
        &color,
        &Transfer::srgb(),
        &Params { exposure, threads },
    )
    .map_err(|e| e.to_string())?;
    write_atomically(out, &image)?;
    writeln!(
        io::stdout().lock(),
        "developed {}x{} -> {}x{} into {} ({} corrupt samples; decode {} ms, total {} ms)",
        nef.raw.width,
        nef.raw.height,
        image.width,
        image.height,
        out.display(),
        decoded.corrupt,
        decoded_at,
        started.elapsed().as_millis()
    )
    .map_err(|e| e.to_string())
}

/// The never-overwrite rule, checked by name: anything at `out` (a file, a
/// link, a directory, the input itself) is a refusal.
fn refuse_existing(out: &Path) -> Result<(), String> {
    if fs::symlink_metadata(out).is_ok() {
        return Err(format!(
            "{}: already exists; td-photo never overwrites, choose another name",
            out.display()
        ));
    }
    Ok(())
}

/// Gives the finished temporary the name `out` without replacing anything
/// that appeared there meanwhile: a hard link fails on an existing name
/// where a rename would replace it. A file system without links (FAT, some
/// network mounts) falls back to a second check and a rename, which keeps
/// the window the link closes.
fn publish(temporary: &Path, out: &Path) -> io::Result<()> {
    match fs::hard_link(temporary, out) {
        Ok(()) => {
            // Published once the link lands: a failed unlink of our own
            // temporary leaves a stray `OUT.tmp`, not a failed write.
            let _ = fs::remove_file(temporary);
            Ok(())
        }
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "already exists; td-photo never overwrites",
        )),
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::PermissionDenied | io::ErrorKind::Unsupported
            ) =>
        {
            if fs::symlink_metadata(out).is_ok() {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "already exists; td-photo never overwrites",
                ));
            }
            fs::rename(temporary, out)
        }
        Err(e) => Err(e),
    }
}

/// Writes through a fresh `OUT.tmp` and publishes it as `out`. Nothing that
/// already exists at `out` is replaced, and the temporary is created
/// exclusively so a link planted there is not followed; both are checked
/// before a byte is written, and the publication itself cannot replace.
fn write_atomically(out: &Path, image: &Rgb8) -> Result<(), String> {
    refuse_existing(out)?;
    let mut temporary = out.as_os_str().to_owned();
    temporary.push(".tmp");
    let temporary = PathBuf::from(temporary);
    let file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|e| format!("{}: {e}", temporary.display()))?;
    let write = |file: fs::File| -> io::Result<()> {
        let mut writer = BufWriter::new(file);
        write_ppm(image, &mut writer)?;
        writer
            .into_inner()
            .map_err(|e| e.into_error())?
            .sync_all()?;
        publish(&temporary, out)
    };
    write(file).map_err(|e| {
        // Ours to remove: created exclusively above, so nothing that was
        // there before is touched.
        let _ = fs::remove_file(&temporary);
        format!("{}: {e}", out.display())
    })
}
