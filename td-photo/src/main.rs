#![forbid(unsafe_code)]

//! The command line: `probe` prints what a file's container says and
//! optionally decodes its raw strip; `develop` renders it to a PPM;
//! `import`, `list`, `flag` and `edit` are the library's headless verbs;
//! `--replay` is the window without a display, the cull controller behind
//! td-ui's driven seam, and `open` the window itself (`window`), the same
//! controller on the display. This is the one place in the crate that
//! opens files, reads the clock or asks for the thread count; the library
//! modules take bytes and buffers, and the window takes its thumbnails
//! from here.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, BufWriter, Read, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use td_ui::driven::{self, Binding, Input, Outcome};
use td_ui::raster::{Composition, Scale, Surface};

use td_photo::color::{camera_color, CameraColor, Transfer};
use td_photo::develop::{self, Params, MAX_THREADS};
use td_photo::image::{read_ppm, write_ppm, Rgb8};
use td_photo::library::{self, Filter, Flag, Key, Sidecar};
use td_photo::look::{self, Look};
use td_photo::nef::{self, Nef};
use td_photo::ui::{self, Effect, Photo};
use td_photo::{camera, jpeg, tiff};

mod window;

const HELP: &str = concat!(
    "td-photo probe FILE [--decode]\n",
    "  Prints what the container says: body, raw geometry, codec, crop,\n",
    "  black level, white balance, exposure facts and embedded previews\n",
    "  with their geometry. --decode also decodes the raw strip and every\n",
    "  preview and prints their statistics and hashes.\n",
    "td-photo develop FILE OUT.ppm [--long-edge N] [--exposure STOPS]\n",
    "                 [--look STEM] [--crop \"X Y W H\"]\n",
    "  Develops the raw to 8-bit sRGB at most N pixels on the long side\n",
    "  (default 1600) with an exposure offset in stops (default 0), the\n",
    "  look STEM (see looks; default none) and the crop \"X Y W H\", four\n",
    "  fractions of the oriented frame as the sidecar spells it (default\n",
    "  the whole frame), written through a fresh\n",
    "  OUT.ppm.tmp and linked into place. OUT.ppm must not exist: td-photo\n",
    "  never overwrites a file.\n",
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
    "td-photo looks [STEM]\n",
    "  Lists the looks, one per line: stem, user or built-in, and its name\n",
    "  or why the file is refused, tab-separated. User looks are\n",
    "  $XDG_CONFIG_HOME/td-photo/looks/STEM.look (~/.config/td-photo/looks/\n",
    "  when that is not absolute) and shadow built-in ones of the same\n",
    "  stem. With STEM, prints that look's text.\n",
    "td-photo import SRC DEST\n",
    "  Copies every NEF under SRC (eight folders deep, links under SRC\n",
    "  not followed) into DEST/YYYY/YYYY-MM-DD/ by its capture time, or\n",
    "  into DEST/undated/, through NAME.part linked into place. A copy\n",
    "  already there with the same bytes is skipped; one that differs, or\n",
    "  anything else at that name, is a conflict, and a source that\n",
    "  cannot be read is unread: each is reported with why and left\n",
    "  alone, and the run fails after the rest. SRC is never written.\n",
    "td-photo list ROLL [--picks | --rejects | --unflagged]\n",
    "  One line per original in ROLL: name, flag, exposure, crop, look and\n",
    "  the sidecar's state (none, ok, or error and why), tab-separated.\n",
    "td-photo flag FILE pick | reject | clear\n",
    "  Sets or clears the cull flag in FILE's sidecar.\n",
    "td-photo edit FILE [KEY VALUE ... | reset]\n",
    "  Prints FILE's sidecar, or sets exposure STOPS (-5.00 to 5.00), crop\n",
    "  X Y W H (fractions to four decimals), look STEM or flag pick|reject;\n",
    "  a VALUE of - clears the key; reset clears all but the flag. The\n",
    "  sidecar is written through NAME.edit.tmp and renamed into place,\n",
    "  the one file td-photo replaces, since it is its own.\n",
    "td-photo open [ROLL] [--control-socket PATH]\n",
    "  Opens the window on the Wayland display, on ROLL if given: the\n",
    "  roll as a grid of thumbnails from the camera's embedded previews,\n",
    "  culled with the keys --help actions lists. --control-socket serves\n",
    "  the --replay vocabulary on a private socket at PATH (absolute, at\n",
    "  most 107 bytes) for an agent driving the live window.\n",
    "td-photo --replay [--size WxH] [ROLL]\n",
    "  The window without a display: requests on stdin, answers on\n",
    "  stdout in td-ui's driving envelope, over the cull actions; ROLL is\n",
    "  opened at the start. See DESIGN.md, Driving.\n",
    "td-photo --preview WxH [ROLL]\n",
    "  Writes what the window would show for ROLL at WxH once every\n",
    "  thumbnail it wants is in, as a PPM on stdout.\n",
    "td-photo --help actions\n",
    "  Prints the action table: name, key, arguments and what it does.\n",
    "Other: --help\n",
    "Supported: Nikon Z 8 14-bit lossless-compressed NEF (see DESIGN.md).\n",
);

const DEFAULT_LONG_EDGE: usize = 1600;

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let result = match args.as_slice() {
        [] => help(),
        [flag, sub] if flag == "--help" && sub == "actions" => help_actions(),
        [flag] if flag == "--help" => help(),
        [_, flag, ..] if flag == "--help" => help(),
        [flag, rest @ ..] if flag == "--replay" => replay(rest),
        [flag, rest @ ..] if flag == "--preview" => window::preview(rest),
        [verb, rest @ ..] if verb == "open" => window::open(rest),
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
        [verb] if verb == "looks" => looks(None),
        [verb, stem] if verb == "looks" => looks(Some(stem)),
        [verb, ..] if verb == "looks" => Err("looks takes at most STEM; see --help".to_string()),
        [verb, src, dest] if verb == "import" => import(Path::new(src), Path::new(dest)),
        [verb, ..] if verb == "import" => Err("import needs SRC and DEST; see --help".to_string()),
        [verb, roll, rest @ ..] if verb == "list" => list(Path::new(roll), rest),
        [verb] if verb == "list" => Err("list needs ROLL; see --help".to_string()),
        [verb, file, word] if verb == "flag" => flag(Path::new(file), word),
        [verb, ..] if verb == "flag" => {
            Err("flag needs FILE and pick, reject or clear; see --help".to_string())
        }
        [verb, file, rest @ ..] if verb == "edit" => edit(Path::new(file), rest),
        [verb] if verb == "edit" => Err("edit needs FILE; see --help".to_string()),
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
    check_flags(
        rest,
        &["--long-edge", "--exposure", "--look", "--crop"],
        &[],
    )?;
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
    // The look, like a bad option, is refused before the camera file is
    // read; a user look that does not parse is an error, not the built-in.
    let look = match option(rest, "--look")? {
        Some(stem) => Some(find_look(&stem)?),
        None => None,
    };
    // The crop, `X Y W H` fractions as the sidecar spells them, refused by the
    // same grammar before the decode.
    let crop = match option(rest, "--crop")? {
        Some(text) => Some(crop_fractions(
            library::Crop::parse(&text).map_err(|_| format!("--crop {text:?} is not X Y W H"))?,
        )),
        None => None,
    };
    // Refused before the decode, not only before the write.
    refuse_existing(out)?;
    let threads = threads();
    let started = Instant::now();
    let (image, info) = develop_raw(path, crop, long_edge, exposure, look.as_ref(), threads)?;
    write_atomically(out, &image)?;
    writeln!(
        io::stdout().lock(),
        "developed {}x{} -> {}x{} into {} ({} corrupt samples; decode {} ms, total {} ms)",
        info.raw_width,
        info.raw_height,
        image.width,
        image.height,
        out.display(),
        info.corrupt,
        info.decode_ms,
        started.elapsed().as_millis()
    )
    .map_err(|e| e.to_string())
}

/// What `decode_raw` reports beside the level-0 frame: the raw sub-image's
/// size, the corrupt-sample count the decoder tolerated, and the decode
/// time (which the read and parse before it are folded into, as the verb
/// has always reported it).
struct DevelopInfo {
    raw_width: usize,
    raw_height: usize,
    corrupt: usize,
    decode_ms: u128,
}

/// The per-photo development metadata the levels above 0 are made with: the
/// orientation level 2 turns by, and the white balance and camera colour
/// level 3 applies. Small and `Copy`, so the window holds it beside the
/// cached levels without keeping the whole raw frame alive.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Meta {
    orientation: u16,
    wb: [f32; 3],
    color: CameraColor,
}

/// Level 0: a decoded CFA frame and everything the pipeline needs to make
/// the levels above it without reading the file again. The window caches it
/// under `develop::RAW_CACHE_BYTES` so returning to a photo reruns level 1
/// rather than the codec.
pub(crate) struct RawFrame {
    decoded: nef::Decoded,
    cfa: nef::Cfa,
    crop: nef::Crop,
    black: u16,
    white: u16,
    meta: Meta,
}

impl RawFrame {
    /// The development metadata, `Copy`, for the levels above 0.
    pub(crate) fn meta(&self) -> Meta {
        self.meta
    }

    /// What the frame costs the raw cache, the CFA samples dominating: two
    /// bytes a sample, so the cache's byte budget bounds how many frames it
    /// holds.
    pub(crate) fn bytes(&self) -> usize {
        self.decoded.samples.len().saturating_mul(2)
    }

    /// A minimal frame of `samples` CFA samples, for the window's raw-cache
    /// tests: only `bytes` (the sample count) is load-bearing there.
    #[cfg(test)]
    pub(crate) fn synth(samples: usize) -> RawFrame {
        RawFrame {
            decoded: nef::Decoded {
                width: 2,
                height: 2,
                samples: vec![0u16; samples],
                corrupt: 0,
            },
            cfa: nef::Cfa::RGGB,
            crop: nef::Crop {
                left: 0,
                top: 0,
                width: 2,
                height: 2,
            },
            black: 0,
            white: 1,
            meta: Meta {
                orientation: 1,
                wb: [1.0, 1.0, 1.0],
                color: CameraColor {
                    rgb_cam: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
                    daylight: [1.0, 1.0, 1.0],
                },
            },
        }
    }
}

/// Level 0: read, parse and decode `path` and gather its development
/// metadata, the codec's single sequential pass. No frame is developed
/// here; the caller runs the levels above it.
pub(crate) fn decode_raw(path: &Path) -> Result<(RawFrame, DevelopInfo), String> {
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
    let decode_ms = started.elapsed().as_millis();
    let black = nef.maker.black.unwrap_or(camera.black);
    let wb = match nef.maker.wb {
        Some((r, b)) => [r, 1.0, b],
        None => color.daylight,
    };
    let info = DevelopInfo {
        raw_width: nef.raw.width,
        raw_height: nef.raw.height,
        corrupt: decoded.corrupt,
        decode_ms,
    };
    let raw = RawFrame {
        decoded,
        cfa: nef.raw.cfa,
        crop: nef.crop(),
        black,
        white: camera.white,
        meta: Meta {
            orientation: nef.orientation,
            wb,
            color,
        },
    };
    Ok((raw, info))
}

/// Level 0 to level 1: the superpixel demosaic, one RGB pixel per CFA quad
/// of the crop, black-subtracted and scaled. Cheap beside the codec, so a
/// return to a cached photo reruns it rather than decoding.
pub(crate) fn raw_level1(raw: &RawFrame, threads: usize) -> Result<develop::Level1, String> {
    develop::superpixel(
        &raw.decoded,
        raw.cfa,
        raw.crop,
        raw.black,
        raw.white,
        threads,
    )
    .map_err(|e| e.to_string())
}

/// A user crop, ten-thousandths of the oriented image, to the `0..=1`
/// fractions the develop pipeline maps back to level 1.
pub(crate) fn crop_fractions(crop: library::Crop) -> [f32; 4] {
    let unit = library::CROP_UNIT as f32;
    [
        crop.x as f32 / unit,
        crop.y as f32 / unit,
        crop.width as f32 / unit,
        crop.height as f32 / unit,
    ]
}

/// Level 1 to level 2: the `crop`'s region (the whole frame when there is
/// none) resampled to the canvas that fits `long_edge` and oriented. Rerun on
/// a crop or resize; reused across exposure and look edits.
pub(crate) fn level1_level2(
    level1: &develop::Level1,
    meta: &Meta,
    crop: Option<[f32; 4]>,
    long_edge: usize,
    threads: usize,
) -> Result<develop::Level2, String> {
    develop::level2(level1, crop, long_edge, meta.orientation, threads).map_err(|e| e.to_string())
}

/// Level 2 to the frame: the per-pixel pipeline at `stops` with `look`, then
/// the shrink to the box for a shape taller than it, the thumbnail rule.
/// What an exposure or look edit reruns; level 2 is untouched.
pub(crate) fn level2_frame(
    level2: &develop::Level2,
    meta: &Meta,
    box_w: usize,
    box_h: usize,
    stops: f32,
    look: Option<&Look>,
    threads: usize,
) -> Result<Rgb8, String> {
    let image = develop::level3(
        level2,
        meta.wb,
        &meta.color,
        &Transfer::srgb(),
        &Params {
            exposure: stops,
            threads,
            look,
        },
    )
    .map_err(|e| e.to_string())?;
    develop::shrink(image, box_w, box_h, threads).map_err(|e| e.to_string())
}

/// The raw develop the `develop` verb shares with the window's preview: a
/// file's raw sub-image parsed, decoded, demosaiced and rendered to an sRGB
/// `Rgb8` fitting `long_edge`, `crop`'s region when one is given, at
/// `exposure` stops with `look` when one is given, by way of the levels
/// above. No file is written here; the caller decides. The verb runs it whole
/// on one thread; the window runs the levels apart and caches them, and
/// `--preview` runs the whole preview through `develop_preview` below.
fn develop_raw(
    path: &Path,
    crop: Option<[f32; 4]>,
    long_edge: usize,
    exposure: f32,
    look: Option<&Look>,
    threads: usize,
) -> Result<(Rgb8, DevelopInfo), String> {
    let (raw, info) = decode_raw(path)?;
    let level1 = raw_level1(&raw, threads)?;
    let image = develop::render(
        &level1,
        crop,
        long_edge,
        raw.meta.orientation,
        raw.meta.wb,
        &raw.meta.color,
        &Transfer::srgb(),
        &Params {
            exposure,
            threads,
            look,
        },
    )
    .map_err(|e| e.to_string())?;
    Ok((image, info))
}

/// The developed preview the window blits into its box and `--preview`
/// reproduces: the file at `roll/name` developed to fit `box_w` by `box_h`
/// at the sidecar's `exposure` (hundredths of a stop) and `look` stem, or
/// `None` (a note on stderr) when it cannot be made, so the box keeps its
/// placeholder as a grid box does for a thumbnail that cannot be made. The
/// look stem is resolved here, off the turn loop's thread. Mirrors the
/// thumbnail rule: developed at the box's long edge, then shrunk to the box
/// for a shape taller than it. The `crop`, when the sidecar sets one, is the
/// region of the frame developed.
// The window's preview key: it names every input the develop turns on.
#[allow(clippy::too_many_arguments)]
fn develop_preview(
    roll: &Path,
    name: &str,
    box_w: usize,
    box_h: usize,
    exposure: i32,
    look: Option<&str>,
    crop: Option<library::Crop>,
    threads: usize,
) -> Option<Rgb8> {
    let path = roll.join(name);
    let look = match look {
        Some(stem) => match find_look(stem) {
            Ok(look) => Some(look),
            Err(why) => {
                note(&why);
                return None;
            }
        },
        None => None,
    };
    let long_edge = box_w.max(box_h);
    let stops = exposure as f32 / 100.0;
    let crop = crop.map(crop_fractions);
    let made = develop_raw(&path, crop, long_edge, stops, look.as_ref(), threads).and_then(
        |(image, _)| {
            develop::shrink(image, box_w, box_h, threads)
                .map_err(|e| format!("{}: {e}", path.display()))
        },
    );
    match made {
        Ok(image) => Some(image),
        Err(why) => {
            note(&why);
            None
        }
    }
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
    let made = make_thumbnail(path, long_edge, cached, threads())?;
    write_atomically(out, &made.image)?;
    let mut stdout = io::stdout().lock();
    match made.source {
        Source::Cached => writeln!(
            stdout,
            "thumbnail {}x{} into {} (cached; {} ms)",
            made.image.width,
            made.image.height,
            out.display(),
            started.elapsed().as_millis()
        ),
        Source::Preview {
            index,
            width,
            height,
        } => writeln!(
            stdout,
            "thumbnail {}x{} from preview {index} ({width}x{height}) into {} ({} ms)",
            made.image.width,
            made.image.height,
            out.display(),
            started.elapsed().as_millis()
        ),
    }
    .map_err(|e| e.to_string())
}

/// Where a thumbnail came from, for the verb's report.
enum Source {
    Cached,
    Preview {
        index: usize,
        width: usize,
        height: usize,
    },
}

struct Made {
    image: Rgb8,
    source: Source,
}

/// The thumbnail rule, with the cache when `cached`: the smallest embedded
/// preview covering `long_edge`, decoded at the coarsest scale that still
/// covers it, resampled to exactly `long_edge` and turned the way the
/// camera was held. The cache is an optimisation: whatever it cannot do is
/// a note on stderr and the thumbnail is made from the original regardless,
/// and an entry is stored only if the bytes read are the file the key
/// describes. The verb and the window share this one path, so the cache
/// holds one thing under one key.
fn make_thumbnail(
    path: &Path,
    long_edge: usize,
    cached: bool,
    threads: usize,
) -> Result<Made, String> {
    let mut cache = if cached {
        let before = stamp_of(path)?;
        Some((cache_key(&before, long_edge), before))
    } else {
        None
    };
    if let Some((key, _)) = &cache {
        match cache_lookup(key, long_edge) {
            Ok(Some(image)) => {
                return Ok(Made {
                    image,
                    source: Source::Cached,
                })
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
    let image = jpeg::thumbnail(bytes, long_edge, threads)
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
    Ok(Made {
        image,
        source: Source::Preview {
            index,
            width: header.width,
            height: header.height,
        },
    })
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

// ------------------------------------------------------------------ looks

/// `$XDG_CONFIG_HOME/td-photo/looks`, or `$HOME/.config/td-photo/looks`.
fn looks_dir() -> Result<PathBuf, String> {
    let base = match std::env::var_os("XDG_CONFIG_HOME").filter(|v| Path::new(v).is_absolute()) {
        Some(v) => PathBuf::from(v),
        None => {
            let home = std::env::var_os("HOME")
                .filter(|v| Path::new(v).is_absolute())
                .ok_or("neither XDG_CONFIG_HOME nor HOME names an absolute directory")?;
            PathBuf::from(home).join(".config")
        }
    };
    Ok(base.join("td-photo").join("looks"))
}

/// Reads a look file, bounded a byte past its ceiling so an oversize one
/// is refused by the format, not truncated into a valid one. A link is
/// followed (the directory is the user's configuration, often linked from
/// elsewhere), but a link to nothing is the user's file and unreadable,
/// not a stem the user has no look for; a fifo or a folder at the name is
/// refused before it is opened. The window between that check and the
/// open is the one every read of a user directory here has (DESIGN.md,
/// Files): the directory is the user's own configuration.
fn read_look(path: &Path) -> io::Result<Vec<u8>> {
    let meta = match fs::metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == io::ErrorKind::NotFound && fs::symlink_metadata(path).is_ok() => {
            return Err(io::Error::other("a link to nothing"));
        }
        Err(e) => return Err(e),
    };
    if !meta.is_file() {
        return Err(io::Error::other("not a regular file"));
    }
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(look::MAX_LOOK_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// The text of the look of `stem` and what to call it in an error: the
/// user's file when there is one, else the built-in. A user file that
/// cannot be read is an error, not a fall back to the built-in it shadows;
/// without a user directory (no absolute `XDG_CONFIG_HOME` or `HOME`) the
/// built-in set is all there is.
fn look_text(stem: &str) -> Result<(Vec<u8>, String), String> {
    if !library::valid_look(stem) {
        return Err(format!(
            "look {stem:?} is not a look stem (1 to 64 of letters, digits, - _ and ., not starting with .)"
        ));
    }
    let dir = looks_dir();
    if let Ok(dir) = &dir {
        let path = dir.join(format!("{stem}.look"));
        match read_look(&path) {
            Ok(bytes) => return Ok((bytes, path.display().to_string())),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("{}: {e}", path.display())),
        }
    }
    match (look::builtin(stem), dir) {
        (Some(text), _) => Ok((text.as_bytes().to_vec(), format!("built-in look {stem}"))),
        (None, Ok(dir)) => Err(format!(
            "look {stem}: not built in and not at {}",
            dir.join(format!("{stem}.look")).display()
        )),
        (None, Err(why)) => Err(format!(
            "look {stem}: not built in, and no user looks directory ({why})"
        )),
    }
}

/// The look of `stem`, parsed; a refusal names the file and the line.
fn find_look(stem: &str) -> Result<Look, String> {
    let (bytes, what) = look_text(stem)?;
    Look::parse(&bytes).map_err(|e| format!("{what}: {e}"))
}

/// `looks`: one line per look, by stem, or with a stem that look's text.
fn looks(stem: Option<&OsStr>) -> Result<(), String> {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    if let Some(stem) = stem {
        let stem = stem.to_str().ok_or("STEM is not UTF-8")?;
        let (bytes, what) = look_text(stem)?;
        Look::parse(&bytes).map_err(|e| format!("{what}: {e}"))?;
        return out.write_all(&bytes).map_err(|e| e.to_string());
    }
    // Stem to source and note; a user look replaces the built-in's row.
    let mut rows: BTreeMap<String, (&str, String)> = BTreeMap::new();
    for (stem, text) in look::BUILTIN {
        let name = Look::parse(text.as_bytes())
            .map_err(|e| format!("built-in look {stem}: {e}"))?
            .name()
            .unwrap_or("-")
            .to_string();
        rows.insert(stem.to_string(), ("built-in", name));
    }
    match looks_dir().and_then(|dir| user_looks(&dir)) {
        Ok(user) => {
            for (stem, note) in user {
                rows.insert(stem, ("user", note));
            }
        }
        Err(why) => {
            // The built-in set is still worth listing; the user's is not
            // reachable (no directory to resolve, or one that cannot be
            // read) and that is said, as `thumb --cache` says of a cache
            // it cannot use.
            let _ = writeln!(
                io::stderr().lock(),
                "td-photo: user looks not listed: {why}"
            );
        }
    }
    for (stem, (source, note)) in rows {
        writeln!(out, "{stem}\t{source}\t{note}").map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Every `STEM.look` in `dir` with its name, or `error` and why it is
/// refused; a stem the sidecar grammar cannot hold is listed quoted and
/// escaped, so a name with a tab or a newline in it is still one record
/// of three columns; a directory that is not there is empty, and one of
/// more than `MAX_ENTRIES` entries is refused.
fn user_looks(dir: &Path) -> Result<Vec<(String, String)>, String> {
    let named = |e: io::Error| format!("{}: {e}", dir.display());
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(named(e)),
    };
    let mut looks = Vec::new();
    for (seen, entry) in entries.enumerate() {
        if seen >= MAX_ENTRIES {
            return Err(format!(
                "{}: more than {MAX_ENTRIES} entries",
                dir.display()
            ));
        }
        let entry = entry.map_err(named)?;
        let Some(stem) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.strip_suffix(".look"))
            .map(str::to_string)
        else {
            continue;
        };
        if !library::valid_look(&stem) {
            looks.push((
                format!("{stem:?}"),
                "error not a look stem (1 to 64 of letters, digits, - _ and ., not starting with .)"
                    .to_string(),
            ));
            continue;
        }
        let note = match read_look(&entry.path()) {
            Ok(bytes) => match Look::parse(&bytes) {
                Ok(look) => look.name().unwrap_or("-").to_string(),
                Err(e) => format!("error {e}"),
            },
            Err(why) => format!("error {why}"),
        };
        looks.push((stem, note));
    }
    Ok(looks)
}

// ---------------------------------------------------------------- library

/// How deep under SRC an import looks (cards keep photos a few folders
/// down), and how many directory entries an import or a listing reads
/// before giving up.
const IMPORT_DEPTH: usize = 8;
const MAX_ENTRIES: usize = 100_000;

/// The sidecar beside an original.
fn sidecar_path(original: &Path) -> PathBuf {
    let mut name = original.as_os_str().to_owned();
    name.push(library::SIDECAR_SUFFIX);
    PathBuf::from(name)
}

/// A sidecar as found beside an original.
enum Loaded {
    /// No sidecar: the camera's defaults.
    None,
    Sidecar(Sidecar),
    /// A sidecar refused as a whole, and why: not a regular file, not
    /// readable, or outside the grammar.
    Refused(String),
}

/// Reads a sidecar, bounded a byte past its ceiling so an oversize one is
/// refused by the grammar, not truncated into a valid one. Only a regular
/// file is opened: a link, a fifo or a folder at the sidecar's name is
/// refused before anything follows it or blocks on it.
fn load_sidecar(original: &Path) -> Loaded {
    let path = sidecar_path(original);
    let read = || -> io::Result<Vec<u8>> {
        if !fs::symlink_metadata(&path)?.is_file() {
            return Err(io::Error::other("not a regular file"));
        }
        let mut bytes = Vec::new();
        fs::File::open(&path)?
            .take(library::MAX_SIDECAR_BYTES as u64 + 1)
            .read_to_end(&mut bytes)?;
        Ok(bytes)
    };
    match read() {
        Ok(bytes) => match Sidecar::parse(&bytes) {
            Ok(sidecar) => Loaded::Sidecar(sidecar),
            Err(error) => Loaded::Refused(error.to_string()),
        },
        Err(e) if e.kind() == io::ErrorKind::NotFound => Loaded::None,
        Err(e) => Loaded::Refused(e.to_string()),
    }
}

/// The sidecar a verb edits: absent is the defaults, refused is an error,
/// so half a sidecar's edits are never rewritten as the whole.
fn read_sidecar(original: &Path) -> Result<Sidecar, String> {
    match load_sidecar(original) {
        Loaded::None => Ok(Sidecar::default()),
        Loaded::Sidecar(sidecar) => Ok(sidecar),
        Loaded::Refused(why) => Err(format!("{}: {why}", sidecar_path(original).display())),
    }
}

/// Writes the sidecar through `NAME.edit.tmp`, synced, then renamed into
/// place: the one file td-photo replaces, since the sidecar is its own.
/// The temporary is created exclusively, so a stale one is reported, not
/// reused or removed; and a sidecar the reader would refuse is not
/// written, so what td-photo writes it reads.
fn write_sidecar(original: &Path, sidecar: &Sidecar) -> Result<(), String> {
    let path = sidecar_path(original);
    let text = sidecar.text();
    // An edit is the one way a sidecar grows, so the reader's ceilings are
    // held here too.
    if let Err(e) = Sidecar::parse(text.as_bytes()) {
        return Err(format!(
            "{}: not written, the reader would refuse it: {e}",
            path.display()
        ));
    }
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(".tmp");
    let temporary = PathBuf::from(temporary);
    let file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|e| format!("{}: {e}", temporary.display()))?;
    let write = |mut file: fs::File| -> io::Result<()> {
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        fs::rename(&temporary, &path)
    };
    write(file).map_err(|e| {
        // Ours to remove: created exclusively above.
        let _ = fs::remove_file(&temporary);
        format!("{}: {e}", path.display())
    })
}

/// The original a sidecar verb acts on: a regular file a roll would list.
fn original(path: &Path) -> Result<(), String> {
    let listed = path
        .file_name()
        .and_then(OsStr::to_str)
        .is_some_and(library::is_original);
    if !listed {
        return Err(format!("{}: not a NEF original", path.display()));
    }
    let meta = fs::symlink_metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if !meta.is_file() {
        return Err(format!("{}: not a regular file", path.display()));
    }
    Ok(())
}

fn flag(path: &Path, word: &OsStr) -> Result<(), String> {
    let word = word.to_str().ok_or("flag word is not UTF-8")?;
    let value = match word {
        "clear" => None,
        other => Some(
            Flag::parse(other)
                .ok_or_else(|| format!("{other:?} is not pick, reject or clear"))?
                .word(),
        ),
    };
    original(path)?;
    let mut sidecar = read_sidecar(path)?;
    sidecar
        .set(Key::Flag, value)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    write_sidecar(path, &sidecar)
}

fn edit(path: &Path, rest: &[OsString]) -> Result<(), String> {
    original(path)?;
    let mut sidecar = read_sidecar(path)?;
    if rest.is_empty() {
        return io::stdout()
            .lock()
            .write_all(sidecar.text().as_bytes())
            .map_err(|e| e.to_string());
    }
    let words = rest
        .iter()
        .map(|word| {
            word.to_str()
                .ok_or_else(|| "arguments must be UTF-8".to_string())
        })
        .collect::<Result<Vec<&str>, String>>()?;
    if words == ["reset"] {
        sidecar.reset();
        return write_sidecar(path, &sidecar);
    }
    let mut words = words.as_slice();
    while let Some((name, tail)) = words.split_first() {
        let key = Key::parse(name)
            .ok_or_else(|| format!("{name}: not a sidecar key (flag, exposure, crop, look)"))?;
        let take = match key {
            Key::Crop if tail.first() != Some(&"-") => 4,
            _ => 1,
        };
        let values = tail
            .get(..take)
            .ok_or_else(|| format!("{name} needs {take} value(s); see --help"))?;
        let value = if values == ["-"] {
            None
        } else {
            Some(values.join(" "))
        };
        sidecar
            .set(key, value.as_deref())
            .map_err(|e| format!("{}: {e}", path.display()))?;
        words = tail.get(take..).unwrap_or(&[]);
    }
    write_sidecar(path, &sidecar)
}

fn list(roll: &Path, rest: &[OsString]) -> Result<(), String> {
    check_flags(rest, &[], &["--picks", "--rejects", "--unflagged"])?;
    let switches: Vec<&str> = rest.iter().filter_map(|arg| arg.to_str()).collect();
    let filter = match switches.as_slice() {
        [] => Filter::All,
        [switch] => [Filter::Picks, Filter::Rejects, Filter::Unflagged]
            .into_iter()
            .find(|filter| switch.strip_prefix("--") == Some(filter.word()))
            .ok_or_else(|| format!("list does not take {switch}; see --help"))?,
        _ => return Err("list takes at most one of --picks, --rejects, --unflagged".to_string()),
    };
    let stdout = io::stdout();
    let mut out = stdout.lock();
    for name in read_roll(roll)? {
        let (values, status) = match load_sidecar(&roll.join(&name)) {
            Loaded::None => (Sidecar::default(), "none".to_string()),
            Loaded::Sidecar(sidecar) => (sidecar, "ok".to_string()),
            Loaded::Refused(why) => (Sidecar::default(), format!("error {why}")),
        };
        if !filter.admits(values.flag()) {
            continue;
        }
        let column = |key: Key| values.value(key).unwrap_or("-").to_string();
        writeln!(
            out,
            "{name}\t{}\t{}\t{}\t{}\t{status}",
            column(Key::Flag),
            column(Key::Exposure),
            column(Key::Crop),
            column(Key::Look)
        )
        .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// A roll's originals by name, sorted; a folder of more than `MAX_ENTRIES`
/// entries is refused. Sidecars are read by the caller, one at a time or
/// under a budget, never all at once for a listing.
fn read_roll(roll: &Path) -> Result<Vec<String>, String> {
    let named = |e: io::Error| format!("{}: {e}", roll.display());
    let mut names: Vec<String> = Vec::new();
    for (seen, entry) in fs::read_dir(roll).map_err(named)?.enumerate() {
        if seen >= MAX_ENTRIES {
            return Err(format!(
                "{}: more than {MAX_ENTRIES} entries",
                roll.display()
            ));
        }
        let entry = entry.map_err(named)?;
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        // `file_type` does not follow a link, so a link is not an original.
        if library::is_original(&name) && entry.file_type().map_err(named)?.is_file() {
            names.push(name);
        }
    }
    names.sort();
    Ok(names)
}

fn import(src: &Path, dest: &Path) -> Result<(), String> {
    // SRC itself may be a link, as a mounted card often is; nothing under
    // it is followed.
    let meta = fs::metadata(src).map_err(|e| format!("{}: {e}", src.display()))?;
    if !meta.is_dir() {
        return Err(format!("{}: not a directory", src.display()));
    }
    let mut sources = Vec::new();
    collect_originals(src, 0, &mut sources, &mut 0)?;
    sources.sort();
    let (mut imported, mut skipped, mut conflicts, mut unread) = (0usize, 0usize, 0usize, 0usize);
    let stdout = io::stdout();
    let mut out = stdout.lock();
    for source in sources {
        // A source that cannot be read is reported and the rest go on; the
        // run fails at the end for it as for a conflict.
        let data = match read_file(&source) {
            Ok(data) => data,
            Err(why) => {
                unread += 1;
                writeln!(out, "unread {why}").map_err(|e| e.to_string())?;
                continue;
            }
        };
        let folder = library::roll_folder(library::taken(&data).as_deref());
        let name = source
            .file_name()
            .ok_or_else(|| format!("{}: no file name", source.display()))?;
        let dir = dest.join(folder);
        let target = dir.join(name);
        let (word, why) = match fs::symlink_metadata(&target) {
            Ok(meta) => match conflict(&meta, &target, &data) {
                None => {
                    skipped += 1;
                    ("skipped", None)
                }
                Some(why) => {
                    conflicts += 1;
                    ("conflict", Some(why))
                }
            },
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
                match copy_in(&data, &target)? {
                    None => {
                        imported += 1;
                        ("imported", None)
                    }
                    Some(why) => {
                        conflicts += 1;
                        ("conflict", Some(why))
                    }
                }
            }
            Err(e) => return Err(format!("{}: {e}", target.display())),
        };
        match why {
            None => writeln!(out, "{word} {}", target.display()),
            Some(why) => writeln!(
                out,
                "{word} {}: {why}; source {}",
                target.display(),
                source.display()
            ),
        }
        .map_err(|e| e.to_string())?;
    }
    writeln!(
        out,
        "imported {imported}, skipped {skipped}, conflicts {conflicts}, unread {unread}"
    )
    .map_err(|e| e.to_string())?;
    if conflicts > 0 || unread > 0 {
        return Err(format!(
            "{conflicts} conflict(s) left alone, {unread} source(s) unread; see the list above"
        ));
    }
    Ok(())
}

/// Why a `target` that exists is a conflict, or `None` when it is a regular
/// file holding exactly `data`: not a regular file, a different length or
/// different bytes, or unreadable, which is reported rather than assumed
/// either way.
fn conflict(meta: &fs::Metadata, target: &Path, data: &[u8]) -> Option<String> {
    if !meta.is_file() {
        return Some("not a regular file".to_string());
    }
    if meta.len() != data.len() as u64 {
        return Some("differs".to_string());
    }
    match holds(target, data) {
        Ok(true) => None,
        Ok(false) => Some("differs".to_string()),
        Err(e) => Some(format!("cannot be compared: {e}")),
    }
}

/// Whether `target` holds exactly `data`, compared a piece at a time so a
/// second copy of the file is never in memory and no more than `data` and
/// a piece is read.
fn holds(target: &Path, data: &[u8]) -> io::Result<bool> {
    let mut file = fs::File::open(target)?;
    let mut piece = vec![0u8; 1 << 16];
    let mut rest = data;
    loop {
        let read = match file.read(&mut piece) {
            Ok(read) => read,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if read == 0 {
            return Ok(rest.is_empty());
        }
        match rest.split_at_checked(read) {
            Some((head, tail)) if Some(head) == piece.get(..read) => rest = tail,
            _ => return Ok(false),
        }
    }
}

/// Copies through `NAME.part`, synced, linked into place and the temporary
/// unlinked: the publication every write but the sidecar's uses, which
/// cannot replace. A name in the way is a conflict, returned as its reason
/// rather than failing the run: a `NAME.part` already there is a previous
/// run's and is neither reused nor removed, and a `NAME` that appeared
/// since the check is left as it is. Any other failure is the run's.
fn copy_in(data: &[u8], target: &Path) -> Result<Option<String>, String> {
    let mut part = target.as_os_str().to_owned();
    part.push(".part");
    let part = PathBuf::from(part);
    let file = match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&part)
    {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            return Ok(Some(format!("{} is in the way", part.display())));
        }
        Err(e) => return Err(format!("{}: {e}", part.display())),
    };
    let write = |mut file: fs::File| -> io::Result<()> {
        file.write_all(data)?;
        file.sync_all()?;
        publish(&part, target)
    };
    match write(file) {
        Ok(()) => Ok(None),
        Err(e) => {
            // Ours to remove: created exclusively above.
            let _ = fs::remove_file(&part);
            if e.kind() == io::ErrorKind::AlreadyExists {
                Ok(Some("appeared meanwhile".to_string()))
            } else {
                Err(format!("{}: {e}", target.display()))
            }
        }
    }
}

/// Every original under `dir`, to `IMPORT_DEPTH`, links not followed.
fn collect_originals(
    dir: &Path,
    depth: usize,
    files: &mut Vec<PathBuf>,
    seen: &mut usize,
) -> Result<(), String> {
    let named = |e: io::Error| format!("{}: {e}", dir.display());
    for entry in fs::read_dir(dir).map_err(named)? {
        let entry = entry.map_err(named)?;
        *seen += 1;
        if *seen > MAX_ENTRIES {
            return Err(format!(
                "{}: more than {MAX_ENTRIES} entries under the source",
                dir.display()
            ));
        }
        let kind = entry.file_type().map_err(named)?;
        if kind.is_dir() {
            if depth < IMPORT_DEPTH {
                collect_originals(&entry.path(), depth + 1, files, seen)?;
            }
        } else if kind.is_file() && entry.file_name().to_str().is_some_and(library::is_original) {
            files.push(entry.path());
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- driving

/// The cull controller behind td-ui's driven seam, with the adapter that
/// opens rolls and writes sidecars for it, so the replay and the window
/// share one dispatcher and one file path.
struct Session {
    ui: ui::Controller,
    /// What `wait-idle` answers: the window sets it before the reply; the
    /// replay has nothing outstanding, so it is always idle.
    idle: bool,
    /// Set when a dispatch answered `quit`: the window closes on it, the
    /// replay keeps answering.
    quit: bool,
}

/// One photo as the model takes it, from a sidecar as found.
fn photo(name: String, loaded: Loaded) -> Photo {
    match loaded {
        Loaded::None => Photo {
            name,
            ..Photo::default()
        },
        Loaded::Sidecar(sidecar) => Photo {
            name,
            sidecar: Some(sidecar),
            error: None,
        },
        Loaded::Refused(why) => Photo {
            name,
            sidecar: None,
            error: Some(why),
        },
    }
}

/// A reason that does not fit the wire: the reply carries the code, the
/// reason goes to stderr.
fn note(why: &str) {
    let _ = writeln!(io::stderr().lock(), "td-photo: {why}");
}

impl Session {
    fn new(surface: Surface) -> Session {
        Session {
            ui: ui::Controller::new(surface),
            idle: true,
            quit: false,
        }
    }

    /// Opens the roll at `path`: its originals, sorted, each with its
    /// sidecar as found, under the model's sidecar budget. A folder that
    /// cannot be read, or one past the budget, is `refused`.
    fn open(&mut self, path: &[u8]) -> Result<(), ui::Error> {
        let roll = PathBuf::from(OsString::from_vec(path.to_vec()));
        let names = match read_roll(&roll) {
            Ok(names) => names,
            Err(why) => {
                note(&why);
                return Err(ui::Error::Refused);
            }
        };
        let mut photos = Vec::with_capacity(names.len());
        let mut bytes = 0usize;
        for name in names {
            let loaded = load_sidecar(&roll.join(&name));
            let photo = photo(name, loaded);
            bytes = bytes.saturating_add(photo.bytes());
            if bytes > ui::MAX_SIDECAR_TOTAL {
                note(&format!(
                    "{}: sidecars over {} bytes between them",
                    roll.display(),
                    ui::MAX_SIDECAR_TOTAL
                ));
                return Err(ui::Error::Refused);
            }
            photos.push(photo);
        }
        // The status row names the roll by its folder.
        let label = roll.file_name().map_or_else(
            || roll.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        );
        // The budget was held while loading, so what is left to refuse is
        // the count; it says so, as every refusal does.
        self.ui.open(&label, path, photos).inspect_err(|_| {
            note(&format!(
                "{}: more than {} photos",
                roll.display(),
                ui::MAX_PHOTOS
            ));
        })
    }

    /// Carries out what a dispatch asked for, in order, and says what came
    /// of it: the dispatch's outcome, or for a flag what the file said.
    fn carry_out(&mut self, effects: Vec<Effect>, outcome: Outcome) -> Result<Outcome, ui::Error> {
        let mut outcome = outcome;
        for effect in effects {
            match effect {
                Effect::Open(path) => self.open(&path)?,
                Effect::Flag { index, name, flag } => {
                    outcome = self.edit(index, name, |sidecar| {
                        sidecar.set(Key::Flag, flag.map(Flag::word))
                    })?
                }
                Effect::Edit {
                    index,
                    name,
                    key,
                    value,
                } => {
                    outcome =
                        self.edit(index, name, |sidecar| sidecar.set(key, value.as_deref()))?
                }
                Effect::Expose { index, name, delta } => {
                    outcome = self.edit(index, name, |sidecar| {
                        let current = sidecar.exposure().unwrap_or(0);
                        let next = current
                            .saturating_add(delta)
                            .clamp(-library::MAX_EXPOSURE, library::MAX_EXPOSURE);
                        sidecar.set(Key::Exposure, Some(&library::exposure_text(next)))
                    })?
                }
                Effect::Reset { index, name } => {
                    outcome = self.edit(index, name, |sidecar| {
                        sidecar.reset();
                        Ok(())
                    })?
                }
            }
        }
        Ok(outcome)
    }

    /// Applies one sidecar edit on the file as it holds it now, not the
    /// model's copy, so an edit made since the roll opened is kept, a
    /// sidecar that became malformed refuses the edit, and a mutation that
    /// leaves the file's value unchanged writes nothing (the flag or value
    /// the file already holds is `ignored`, or `changed` only if the file
    /// differed from the model's copy); an edit that would take the roll
    /// past its sidecar budget is refused before anything is written. The
    /// model is settled from what was written, or from the file when the
    /// write failed, which is `refused`.
    fn edit(
        &mut self,
        index: usize,
        name: String,
        mutate: impl FnOnce(&mut Sidecar) -> Result<(), library::Error>,
    ) -> Result<Outcome, ui::Error> {
        let roll = self.ui.roll().ok_or(ui::Error::NoRoll)?;
        let original = PathBuf::from(OsStr::from_bytes(roll)).join(&name);
        let refuse = |session: &mut Session, why: &str| {
            note(why);
            session
                .ui
                .settle(index, photo(name.clone(), load_sidecar(&original)));
            Err(ui::Error::Refused)
        };
        let mut sidecar = match read_sidecar(&original) {
            Ok(sidecar) => sidecar,
            Err(why) => return refuse(self, &why),
        };
        let before = sidecar.clone();
        if let Err(e) = mutate(&mut sidecar) {
            return refuse(self, &format!("{}: {e}", original.display()));
        }
        if sidecar == before {
            // The value the file already holds: nothing is written, and the
            // model takes the file's word, a change only if the file
            // differed from the model's copy.
            let held = Photo {
                name,
                sidecar: Some(sidecar),
                error: None,
            };
            return Ok(if self.ui.settle(index, held) {
                Outcome::Changed
            } else {
                Outcome::Ignored
            });
        }
        if !self.ui.fits(index, ui::sidecar_bytes(&sidecar)) {
            note(&format!("{}: {}", original.display(), ui::OVER_BUDGET));
            return Err(ui::Error::Refused);
        }
        if let Err(why) = write_sidecar(&original, &sidecar) {
            return refuse(self, &why);
        }
        self.ui.settle(
            index,
            Photo {
                name,
                sidecar: Some(sidecar),
                error: None,
            },
        );
        Ok(Outcome::Changed)
    }
}

impl driven::Controller for Session {
    type Error = ui::Error;

    fn bindings(&self) -> &'static [Binding] {
        &ui::BINDINGS
    }

    fn action(&mut self, name: &str, arguments: &[&str]) -> Result<Outcome, ui::Error> {
        let (outcome, effects) = self.ui.action(name, arguments)?;
        let outcome = self.carry_out(effects, outcome)?;
        self.quit |= outcome == Outcome::Quit;
        Ok(outcome)
    }

    fn input(&mut self, input: Input<'_>) -> Result<Outcome, ui::Error> {
        let (outcome, effects) = self.ui.input(input)?;
        let outcome = self.carry_out(effects, outcome)?;
        self.quit |= outcome == Outcome::Quit;
        Ok(outcome)
    }

    fn state(&self) -> Result<String, ui::Error> {
        Ok(self.ui.state())
    }

    fn compose<R>(&self, view: impl FnOnce(&dyn Composition) -> R) -> Result<R, ui::Error> {
        Ok(view(&self.ui.scene()))
    }

    /// `photo N`: the Nth shown photo's facts. `wait-idle MS`: `idle` or
    /// `busy` as the adapter set it, for `MS` up to `MAX_WAIT_MS`; the
    /// window holds the request until one is true, the replay answers at
    /// once.
    fn request(&mut self, name: &str, arguments: &[&str]) -> Result<String, ui::Error> {
        match (name, arguments) {
            ("photo", [position]) => {
                let position = usize::try_from(td_ui::control::decimal(position)?)
                    .map_err(|_| ui::Error::BadArgument)?;
                self.ui.photo(position)
            }
            ("wait-idle", [ms]) => {
                if td_ui::control::decimal(ms)? > ui::MAX_WAIT_MS {
                    return Err(ui::Error::BadArgument);
                }
                Ok(if self.idle { "idle" } else { "busy" }.to_string())
            }
            _ => Err(td_ui::control::Error::Protocol.into()),
        }
    }
}

/// `--replay [--size WxH] [ROLL]`: the session on stdin and stdout.
fn replay(rest: &[OsString]) -> Result<(), String> {
    let mut size = None;
    let mut roll: Option<PathBuf> = None;
    let mut args = rest.iter();
    while let Some(arg) = args.next() {
        if arg == "--size" {
            let value = args
                .next()
                .and_then(|value| value.to_str())
                .ok_or("--size needs WxH")?;
            let (width, height) = value
                .split_once('x')
                .and_then(|(w, h)| Some((w.parse::<usize>().ok()?, h.parse::<usize>().ok()?)))
                .ok_or_else(|| format!("--size {value:?} is not WxH"))?;
            size = Some((width, height));
        } else if roll.is_none() && !arg.as_bytes().starts_with(b"--") {
            roll = Some(PathBuf::from(arg));
        } else {
            return Err(format!("unrecognized argument {arg:?}; see --help"));
        }
    }
    let (width, height) = size.unwrap_or((ui::DEFAULT_WIDTH, ui::DEFAULT_HEIGHT));
    let surface =
        Surface::new(width, height, Scale::default()).map_err(|e| format!("--size: {e}"))?;
    let mut session = Session::new(surface);
    if let Some(roll) = roll {
        session
            .open(roll.as_os_str().as_bytes())
            .map_err(|e| format!("{}: {e}", roll.display()))?;
    }
    td_ui::replay::run(&mut io::stdin().lock(), &mut io::stdout().lock(), |bytes| {
        driven::request(&mut session, bytes)
    })
    .map_err(|e| e.to_string())
}

/// `--help actions`: the table an agent reads instead of guessing.
fn help_actions() -> Result<(), String> {
    io::stdout()
        .lock()
        .write_all(driven::help(&ui::BINDINGS).as_bytes())
        .map_err(|e| e.to_string())
}
