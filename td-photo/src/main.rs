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
use td_photo::image::{read_ppm, write_ppm, Rgb8};
use td_photo::nef::{self, Nef};
use td_photo::{camera, jpeg, tiff};

const HELP: &str = concat!(
    "td-photo probe FILE [--decode]\n",
    "  Prints what the container says: body, raw geometry, codec, crop,\n",
    "  black level, white balance, exposure facts and embedded previews\n",
    "  with their geometry. --decode also decodes the raw strip and every\n",
    "  preview and prints their statistics and hashes.\n",
    "td-photo develop FILE OUT.ppm [--long-edge N] [--exposure STOPS]\n",
    "  Develops the raw to 8-bit sRGB at most N pixels on the long side\n",
    "  (default 1600) with an exposure offset in stops (default 0), written\n",
    "  through a fresh OUT.ppm.tmp and linked into place. OUT.ppm must not\n",
    "  exist: td-photo never overwrites a file.\n",
    "td-photo thumb FILE OUT.ppm [--long-edge N] [--cache]\n",
    "  Writes the thumbnail: the smallest embedded preview that covers N\n",
    "  pixels on the long side (default 400), decoded at the coarsest\n",
    "  scale that still covers N, resampled to exactly N and turned the\n",
    "  way the camera was held. OUT.ppm must not exist. --cache answers\n",
    "  from and fills the thumbnail cache; a cache that cannot be used is\n",
    "  reported on stderr and the thumbnail is written all the same.\n",
    "td-photo cache path | clear\n",
    "  Prints the cache directory ($XDG_CACHE_HOME/td-photo when that is\n",
    "  absolute, else ~/.cache/td-photo), or removes every thumbnail.\n",
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
        [verb, file, out, rest @ ..] if verb == "thumb" => {
            thumb_file(Path::new(file), Path::new(out), rest)
        }
        [verb, ..] if verb == "thumb" => {
            Err("thumb needs FILE and OUT.ppm; see --help".to_string())
        }
        [verb, sub] if verb == "cache" && sub == "path" => cache_path(),
        [verb, sub] if verb == "cache" && sub == "clear" => cache_clear(),
        [verb, ..] if verb == "cache" => Err("cache needs path or clear; see --help".to_string()),
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
    read_file_stamped(path).map(|(data, _)| data)
}

/// What a cache key says about an original: where the name resolved, how
/// long the file was and when it last changed.
#[derive(Clone, PartialEq, Eq)]
struct Stamp {
    resolved: PathBuf,
    len: u64,
    secs: u64,
    nanos: u32,
}

fn stamp(resolved: PathBuf, meta: &fs::Metadata) -> Stamp {
    let modified = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .unwrap_or_default();
    Stamp {
        resolved,
        len: meta.len(),
        secs: modified.as_secs(),
        nanos: modified.subsec_nanos(),
    }
}

/// The stamp of `path` as it stands, without reading it.
fn stamp_of(path: &Path) -> Result<Stamp, String> {
    let named = |e: io::Error| format!("{}: {e}", path.display());
    let resolved = fs::canonicalize(path).map_err(named)?;
    let meta = fs::metadata(&resolved).map_err(named)?;
    if !meta.is_file() {
        return Err(format!("{}: not a regular file", path.display()));
    }
    Ok(stamp(resolved, &meta))
}

/// `read_file`, with the stamp of what was read: the metadata is taken
/// from the open file after the last byte, and the name resolved again,
/// so a caller can tell whether the bytes are the file a stamp described;
/// a stamp that cannot be taken is `None`, and nothing is cached.
fn read_file_stamped(path: &Path) -> Result<(Vec<u8>, Option<Stamp>), String> {
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
    // The stamp serves the cache alone: a name renamed away meanwhile
    // leaves the bytes good and the stamp `None`.
    let after = file
        .metadata()
        .ok()
        .and_then(|meta| fs::canonicalize(path).ok().map(|r| stamp(r, &meta)));
    Ok((data, after))
}

fn describe(nef: &Nef, data: &[u8], out: &mut impl Write) -> io::Result<()> {
    writeln!(out, "size: {}", data.len())?;
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
        match data.get(p.offset..p.offset.saturating_add(p.len)) {
            Some(bytes) => match jpeg::header(bytes) {
                Ok(h) => writeln!(
                    out,
                    "preview: {}+{} {}x{} {} component(s) sampling {}x{}",
                    p.offset, p.len, h.width, h.height, h.components, h.sampling.0, h.sampling.1
                )?,
                Err(e) => writeln!(out, "preview: {}+{} ({e})", p.offset, p.len)?,
            },
            None => writeln!(out, "preview: {}+{} (outside the file)", p.offset, p.len)?,
        }
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
    describe(&nef, &data, &mut out).map_err(|e| e.to_string())?;
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
        // Every preview at full scale, hashed the way jpeg_ref.py hashes
        // its RGB bytes, so a new body's previews can be checked against
        // the oracle without committing them.
        for (index, p) in nef.previews.iter().enumerate() {
            let Some(bytes) = data.get(p.offset..p.offset.saturating_add(p.len)) else {
                continue;
            };
            let started = Instant::now();
            match jpeg::decode(bytes, jpeg::Scale::Full) {
                Ok(image) => writeln!(
                    out,
                    "preview-decode: {index} {}x{} fnv1a64 {:#018x}, {} ms",
                    image.width,
                    image.height,
                    fnv1a64(&image.data),
                    started.elapsed().as_millis()
                ),
                Err(e) => writeln!(out, "preview-decode: {index} {e}"),
            }
            .map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

/// FNV-1a-64 over bytes, the hash the fixture README records.
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in bytes {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
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
    let mut temporary = out.as_os_str().to_owned();
    temporary.push(".tmp");
    write_via(&PathBuf::from(temporary), out, image)
}

/// `write_atomically` through a temporary of the caller's naming.
fn write_via(temporary: &Path, out: &Path, image: &Rgb8) -> Result<(), String> {
    refuse_existing(out)?;
    let file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(temporary)
        .map_err(|e| format!("{}: {e}", temporary.display()))?;
    let write = |file: fs::File| -> io::Result<()> {
        let mut writer = BufWriter::new(file);
        write_ppm(image, &mut writer)?;
        writer
            .into_inner()
            .map_err(|e| e.into_error())?
            .sync_all()?;
        publish(temporary, out)
    };
    write(file).map_err(|e| {
        // Ours to remove: created exclusively above, so nothing that was
        // there before is touched.
        let _ = fs::remove_file(temporary);
        format!("{}: {e}", out.display())
    })
}

// ------------------------------------------------------------- thumbnails

const DEFAULT_THUMB_EDGE: usize = 400;
/// The most bytes a cached thumbnail may be when read back.
const MAX_THUMB_FILE_BYTES: u64 = 64 << 20;

/// The preview the thumbnail rule picks: the smallest whose long edge
/// covers `long_edge`, else the largest; previews that are not baseline
/// JPEG are skipped.
fn choose_preview<'a>(
    nef: &Nef,
    data: &'a [u8],
    long_edge: usize,
) -> Option<(usize, &'a [u8], jpeg::Header)> {
    let mut best: Option<(usize, &'a [u8], jpeg::Header)> = None;
    for (index, p) in nef.previews.iter().enumerate() {
        let Some(bytes) = data.get(p.offset..p.offset.saturating_add(p.len)) else {
            continue;
        };
        let Ok(header) = jpeg::header(bytes) else {
            continue;
        };
        let long = header.width.max(header.height);
        let better = match &best {
            None => true,
            Some((_, _, b)) => {
                let best_long = b.width.max(b.height);
                if best_long >= long_edge {
                    long >= long_edge && long < best_long
                } else {
                    long > best_long
                }
            }
        };
        if better {
            best = Some((index, bytes, header));
        }
    }
    best
}

fn thumb_file(path: &Path, out: &Path, rest: &[OsString]) -> Result<(), String> {
    check_flags(rest, &["--long-edge"], &["--cache"])?;
    let long_edge = match option(rest, "--long-edge")? {
        Some(text) => text
            .parse::<usize>()
            .ok()
            .filter(|n| (16..=nef::MAX_AXIS).contains(n))
            .ok_or_else(|| format!("--long-edge {text:?} is not 16..=16384"))?,
        None => DEFAULT_THUMB_EDGE,
    };
    let cached = rest.iter().any(|a| a == "--cache");
    refuse_existing(out)?;
    let started = Instant::now();
    // The cache is an optimisation: whatever it cannot do is a note on
    // stderr, and the thumbnail is written from the original regardless.
    let mut cache = if cached {
        let before = stamp_of(path)?;
        Some((cache_key(&before, long_edge), before))
    } else {
        None
    };
    if let Some((key, _)) = &cache {
        match cache_lookup(key, long_edge) {
            Ok(Some(image)) => {
                write_atomically(out, &image)?;
                return writeln!(
                    io::stdout().lock(),
                    "thumbnail {}x{} into {} (cached; {} ms)",
                    image.width,
                    image.height,
                    out.display(),
                    started.elapsed().as_millis()
                )
                .map_err(|e| e.to_string());
            }
            Ok(None) => {}
            Err(note) => {
                cache_note(&note);
                cache = None;
            }
        }
    }
    let (data, after) = read_file_stamped(path)?;
    let nef = nef::parse(&data).map_err(|e| format!("{}: {e}", path.display()))?;
    let (index, bytes, header) = choose_preview(&nef, &data, long_edge)
        .ok_or_else(|| format!("{}: no baseline JPEG preview", path.display()))?;
    let image = jpeg::thumbnail(bytes, long_edge, threads())
        .map_err(|e| format!("{}: preview {index}: {e}", path.display()))?;
    let image = develop::orient(image, nef.orientation);
    // Stored only if the bytes are the file the key describes: an
    // original replaced between the stamp and the read keys itself anew.
    if let Some((key, before)) = &cache {
        if Some(before) == after.as_ref() {
            if let Err(note) = cache_store(key, &image) {
                cache_note(&note);
            }
        }
    }
    write_atomically(out, &image)?;
    writeln!(
        io::stdout().lock(),
        "thumbnail {}x{} from preview {index} ({}x{}) into {} ({} ms)",
        image.width,
        image.height,
        header.width,
        header.height,
        out.display(),
        started.elapsed().as_millis()
    )
    .map_err(|e| e.to_string())
}

// ------------------------------------------------------------------ cache

/// `$XDG_CACHE_HOME/td-photo`, or `$HOME/.cache/td-photo`.
fn cache_dir() -> Result<PathBuf, String> {
    let base = match std::env::var_os("XDG_CACHE_HOME").filter(|v| Path::new(v).is_absolute()) {
        Some(v) => PathBuf::from(v),
        None => {
            let home = std::env::var_os("HOME")
                .filter(|v| Path::new(v).is_absolute())
                .ok_or("neither XDG_CACHE_HOME nor HOME names an absolute directory")?;
            PathBuf::from(home).join(".cache")
        }
    };
    Ok(base.join("td-photo"))
}

/// Whether `path`, one of the cache's own directories, is present as a
/// real directory. `cache clear` unlinks inside these, so a symlink in
/// their place (which could lead into the library) is refused rather than
/// followed; the base directory above them is the user's and may be
/// anything.
fn own_dir(path: &Path) -> Result<bool, String> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => Ok(true),
        Ok(_) => Err(format!(
            "{}: not a directory of the cache's own (a symlink or a file)",
            path.display()
        )),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// The thumbnails directory when it exists as the cache's own.
fn thumbs_dir() -> Result<Option<PathBuf>, String> {
    let base = cache_dir()?;
    if !own_dir(&base)? {
        return Ok(None);
    }
    let thumbs = base.join("thumbs");
    if !own_dir(&thumbs)? {
        return Ok(None);
    }
    Ok(Some(thumbs))
}

fn cache_note(note: &str) {
    // A failed write to stderr has nowhere to be reported.
    let _ = writeln!(
        io::stderr().lock(),
        "td-photo: cache: {note}; the thumbnail is written without it"
    );
}

/// A cached thumbnail's key: FNV-1a-64 of the original's resolved path,
/// byte length and modification time, then the long edge, so an original
/// that was edited or replaced, as far as those tell, never answers from
/// a stale thumbnail.
fn cache_key(stamp: &Stamp, long_edge: usize) -> String {
    let mut bytes = stamp.resolved.as_os_str().as_encoded_bytes().to_vec();
    bytes.push(0);
    bytes.extend_from_slice(&stamp.len.to_le_bytes());
    bytes.extend_from_slice(&stamp.secs.to_le_bytes());
    bytes.extend_from_slice(&stamp.nanos.to_le_bytes());
    format!("{:016x}-{long_edge}", fnv1a64(&bytes))
}

/// Whether a file name is one this crate's cache writes: an entry
/// `KEY-N.ppm`, or the temporary `KEY-N.ppm.PID.tmp` a fill goes through.
fn is_cache_name(name: &str) -> bool {
    let stem = match name.strip_suffix(".ppm") {
        Some(stem) => stem,
        None => {
            let Some(rest) = name.strip_suffix(".tmp") else {
                return false;
            };
            let Some((stem, pid)) = rest.rsplit_once('.') else {
                return false;
            };
            if pid.is_empty() || !pid.bytes().all(|b| b.is_ascii_digit()) {
                return false;
            }
            let Some(stem) = stem.strip_suffix(".ppm") else {
                return false;
            };
            stem
        }
    };
    let Some((hash, edge)) = stem.split_once('-') else {
        return false;
    };
    hash.len() == 16
        && hash.bytes().all(|b| b.is_ascii_hexdigit())
        && !edge.is_empty()
        && edge.bytes().all(|b| b.is_ascii_digit())
}

/// The cached thumbnail under `key`, or `None` for a miss. Only a regular
/// file of the exact shape `write_ppm` produces, under the ceiling and no
/// larger than the asked edge, is a hit; anything else in the entry's
/// place is a miss, and an entry of the right kind with the wrong content
/// is unlinked so the miss refills it.
fn cache_lookup(key: &str, long_edge: usize) -> Result<Option<Rgb8>, String> {
    let Some(dir) = thumbs_dir()? else {
        return Ok(None);
    };
    let path = dir.join(format!("{key}.ppm"));
    match fs::symlink_metadata(&path) {
        Ok(meta) if meta.is_file() => {}
        // A symlink, a directory or a device in the entry's place is not
        // ours: neither followed, opened nor removed.
        Ok(_) => return Ok(None),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    }
    let mut bytes = Vec::new();
    let read = fs::File::open(&path).and_then(|file| {
        (&file)
            .take(MAX_THUMB_FILE_BYTES + 1)
            .read_to_end(&mut bytes)
    });
    if read.is_err() || bytes.len() as u64 > MAX_THUMB_FILE_BYTES {
        return Ok(None);
    }
    // The one fact the key promises that the bytes can contradict is the
    // long edge, so an entry past it is malformed too.
    let image = read_ppm(&bytes).filter(|image| image.width.max(image.height) <= long_edge);
    if image.is_none() {
        // Ours by name and kind; a failed unlink leaves a miss, as before.
        let _ = fs::remove_file(&path);
    }
    Ok(image)
}

/// Fills the entry under `key` unless one is present. The temporary is
/// named for this process so two fillers never contend for a name, and a
/// temporary a killed process left behind blocks nothing (`cache clear`
/// removes it).
fn cache_store(key: &str, image: &Rgb8) -> Result<(), String> {
    let base = cache_dir()?;
    own_dir(&base)?;
    let dir = base.join("thumbs");
    own_dir(&dir)?;
    fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let path = dir.join(format!("{key}.ppm"));
    // Present already (another process got there first): the key says it
    // is the same thumbnail.
    if fs::symlink_metadata(&path).is_ok() {
        return Ok(());
    }
    let temporary = dir.join(format!("{key}.ppm.{}.tmp", std::process::id()));
    match write_via(&temporary, &path, image) {
        Ok(()) => Ok(()),
        // Lost the race to publish it: fine for the same reason.
        Err(_) if fs::symlink_metadata(&path).is_ok() => Ok(()),
        Err(e) => Err(e),
    }
}

/// Prints the cache directory as the bytes it is, for a script to use.
fn cache_path() -> Result<(), String> {
    let dir = cache_dir()?;
    let mut out = io::stdout().lock();
    out.write_all(dir.as_os_str().as_encoded_bytes())
        .and_then(|()| out.write_all(b"\n"))
        .map_err(|e| e.to_string())
}

/// Removes the thumbnails, and only files named the way this crate names
/// them, so a stray file in the directory is left alone.
fn cache_clear() -> Result<(), String> {
    let Some(dir) = thumbs_dir()? else {
        return writeln!(
            io::stdout().lock(),
            "removed 0 thumbnails from {}",
            cache_dir()?.join("thumbs").display()
        )
        .map_err(|e| e.to_string());
    };
    let entries = fs::read_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let mut removed = 0usize;
    for entry in entries {
        let entry = entry.map_err(|e| format!("{}: {e}", dir.display()))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !is_cache_name(name) {
            continue;
        }
        let path = entry.path();
        let is_file = fs::symlink_metadata(&path).is_ok_and(|m| m.is_file());
        if !is_file {
            continue;
        }
        fs::remove_file(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        removed += 1;
    }
    writeln!(
        io::stdout().lock(),
        "removed {removed} thumbnails from {}",
        dir.display()
    )
    .map_err(|e| e.to_string())
}
