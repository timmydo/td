//! Pictures a person's message carries (DESIGN.md §6, Pictures): pasted
//! bytes, checked once in the window, staged under the state directory's
//! `pasted/<conversation>/<digest>`, adopted by the conversation's
//! process into its own `pictures/<digest>`, written once, and named in
//! its log by reference. A request carries a picture as a placeholder
//! part (`td-picture:`), which `inline` turns into the picture's data
//! URL as the request is sent, or into a stand-in for a model that takes
//! none, so the log and every view built from it stay small and the body
//! rebuilt from the log still says exactly what was sent.

use crate::store::{Id, StateDir};
use std::path::{Path, PathBuf};
use td_json::Json;

pub const PNG: &str = "image/png";
pub const JPEG: &str = "image/jpeg";
/// The most one picture may hold: the least of the providers' bounds on
/// one picture (Anthropic's five megabytes).
pub const MAX_PICTURE_BYTES: u64 = 5_000_000;
/// The longest side a provider takes.
pub const MAX_PICTURE_AXIS: u32 = 8000;
/// The most pictures one message carries.
pub const MAX_PICTURES: usize = 8;
/// The most picture bytes a conversation's messages carry in all, so a
/// request with every one, in base64, stays inside the fetch service's
/// 32 MiB with room for the text.
pub const MAX_CONVERSATION_PICTURE_BYTES: u64 = 20_000_000;
/// The longest side a picture past the bounds is brought within: the
/// most a provider looks at (OpenAI's 2048; Anthropic scales to 1568).
pub const SHRUNK_AXIS: usize = 2048;
/// The quality a shrunk picture is written as a JPEG at.
pub const SHRUNK_QUALITY: u8 = 85;
/// The most threads a shrink's JPEG transform runs on.
const SHRINK_THREADS: usize = 8;
/// The longest label, `image 1` and its like.
pub const MAX_LABEL: usize = 32;
/// The state directory's staging directory, and a conversation's own.
pub const PASTED: &str = "pasted";
pub const KEPT: &str = "pictures";
/// What a placeholder part's URL starts with.
const SCHEME: &str = "td-picture:";
/// The most tokens a picture is counted as before a reply says: a
/// provider scales a large picture down to about a megapixel, at most
/// about 1600 tokens.
const MAX_PICTURE_TOKENS: u64 = 1600;

/// A picture as a log names it: its content's SHA-256, what it is, its
/// size, and the label the message's text calls it by.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Reference {
    pub digest: String,
    pub mime: &'static str,
    pub width: u32,
    pub height: u32,
    pub bytes: u64,
    pub label: String,
}

impl Reference {
    pub fn to_json(&self) -> Json {
        Json::Obj(vec![
            ("digest".into(), Json::Str(self.digest.clone())),
            ("mime".into(), Json::Str(self.mime.into())),
            ("width".into(), Json::from(u64::from(self.width))),
            ("height".into(), Json::from(u64::from(self.height))),
            ("bytes".into(), Json::from(self.bytes)),
            ("label".into(), Json::Str(self.label.clone())),
        ])
    }

    /// A reference as `to_json` wrote it, every field within its bounds.
    pub fn from_json(value: &Json) -> Result<Self, String> {
        let text = |name: &str| {
            value
                .get(name)
                .and_then(Json::as_str)
                .ok_or_else(|| format!("a picture's {name} is missing"))
        };
        let number = |name: &str| {
            value
                .get(name)
                .and_then(Json::as_u64)
                .ok_or_else(|| format!("a picture's {name} is missing"))
        };
        let digest = text("digest")?;
        if !digest_ok(digest) {
            return Err("a picture's digest is not a SHA-256".into());
        }
        let mime = match text("mime")? {
            PNG => PNG,
            JPEG => JPEG,
            other => return Err(format!("a picture's type {other} is not taken")),
        };
        let axis = |name: &str| -> Result<u32, String> {
            u32::try_from(number(name)?)
                .ok()
                .filter(|&n| (1..=MAX_PICTURE_AXIS).contains(&n))
                .ok_or_else(|| format!("a picture's {name} is out of range"))
        };
        let (width, height) = (axis("width")?, axis("height")?);
        let bytes = number("bytes")?;
        if !(1..=MAX_PICTURE_BYTES).contains(&bytes) {
            return Err("a picture's length is out of range".into());
        }
        let label = text("label")?;
        if !label_ok(label) {
            return Err("a picture's label is malformed".into());
        }
        Ok(Self {
            digest: digest.to_string(),
            mime,
            width,
            height,
            bytes,
            label: label.to_string(),
        })
    }

    /// Its placeholder part in a request, which `inline` replaces.
    pub fn part(&self) -> Json {
        let url = format!(
            "{SCHEME}{};{}x{};{}",
            self.mime, self.width, self.height, self.digest
        );
        Json::Obj(vec![
            ("type".into(), Json::Str("image_url".into())),
            (
                "image_url".into(),
                Json::Obj(vec![("url".into(), Json::Str(url))]),
            ),
        ])
    }

    /// How a copy or a text-only view says it: `[image 1]`.
    pub fn stand_in(&self) -> String {
        format!("[{}]", self.label)
    }
}

/// A lowercase hex SHA-256.
fn digest_ok(digest: &str) -> bool {
    digest.len() == 64
        && digest
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// A label of letters, digits and spaces, not empty and at most
/// `MAX_LABEL` bytes, so it stands in brackets as it is.
fn label_ok(label: &str) -> bool {
    !label.is_empty()
        && label.len() <= MAX_LABEL
        && label
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b' ')
}

/// What a pasted picture is, by its first bytes: PNG's signature or
/// JPEG's start of image. The clipboard's word is not taken for it.
fn sniff(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(PNG)
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some(JPEG)
    } else {
        None
    }
}

/// Decodes a picture: eight-bit RGBA, its width and height. One wider or
/// taller than `most` is brought within it for showing, a JPEG at the
/// least DCT scale that does and a PNG halved until it does; `most` of
/// `usize::MAX` decodes it whole.
pub fn decode(mime: &str, bytes: &[u8], most: usize) -> Result<(usize, usize, Vec<u8>), String> {
    match mime {
        PNG => {
            let image =
                td_png::decode(bytes).map_err(|e| format!("not a PNG td-agent reads: {e}"))?;
            let (mut width, mut height, mut rgba) = (image.width, image.height, image.data);
            while width.max(height) > most.max(1) {
                (width, height, rgba) = halve(width, height, &rgba)?;
            }
            Ok((width, height, rgba))
        }
        JPEG => {
            let header =
                td_jpeg::header(bytes).map_err(|e| format!("not a JPEG td-agent reads: {e}"))?;
            let longest = header.width.max(header.height);
            let scale = [
                td_jpeg::Scale::Full,
                td_jpeg::Scale::Half,
                td_jpeg::Scale::Quarter,
                td_jpeg::Scale::Eighth,
            ]
            .into_iter()
            .find(|&scale| td_jpeg::scaled(longest, scale) <= most)
            .unwrap_or(td_jpeg::Scale::Eighth);
            let image = td_jpeg::decode(bytes, scale)
                .map_err(|e| format!("not a JPEG td-agent reads: {e}"))?;
            let mut rgba = Vec::new();
            rgba.try_reserve_exact(image.data.len() / 3 * 4)
                .map_err(|_| "no memory for the picture".to_string())?;
            for pixel in image.data.as_chunks::<3>().0 {
                rgba.extend_from_slice(pixel);
                rgba.push(255);
            }
            orient(image.width, image.height, rgba, orientation(bytes))
        }
        other => Err(format!("{other} is not a picture td-agent takes")),
    }
}

/// An RGBA picture at half its size, each pixel the mean of the two by
/// two it covers, an odd last row or column alone: its alpha the mean
/// alpha, its colour the mean weighted by alpha, so a transparent
/// pixel's colour, which nothing shows, does not tint its neighbours.
fn halve(width: usize, height: usize, rgba: &[u8]) -> Result<(usize, usize, Vec<u8>), String> {
    let (w, h) = (width.div_ceil(2), height.div_ceil(2));
    let mut out = Vec::new();
    out.try_reserve_exact(w.saturating_mul(h).saturating_mul(4))
        .map_err(|_| "no memory for the picture".to_string())?;
    let pixel = |x: usize, y: usize| {
        rgba.get((y * width + x) * 4..)
            .and_then(|p| p.first_chunk::<4>())
            .copied()
    };
    for y in 0..h {
        for x in 0..w {
            let mut colour = [0u32; 3];
            let (mut alpha, mut n) = (0u32, 0u32);
            for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                let (sx, sy) = (2 * x + dx, 2 * y + dy);
                if sx >= width || sy >= height {
                    continue;
                }
                let [r, g, b, a] = pixel(sx, sy).ok_or("a picture shorter than its size")?;
                for (total, channel) in colour.iter_mut().zip([r, g, b]) {
                    *total += u32::from(channel) * u32::from(a);
                }
                alpha += u32::from(a);
                n += 1;
            }
            out.extend(
                colour.map(|total| (total + alpha / 2).checked_div(alpha).unwrap_or(0) as u8),
            );
            out.push(((alpha + n / 2) / n.max(1)) as u8);
        }
    }
    Ok((w, h, out))
}

/// The EXIF orientation a JPEG's APP1 segment gives (TIFF tag 0x0112),
/// 1 to 8; 1, as it is stored, where it gives none or one out of range.
fn orientation(jpeg: &[u8]) -> u8 {
    let u16_at = |b: &[u8], at: usize, big: bool| {
        let pair = *b.get(at..)?.first_chunk::<2>()?;
        Some(if big {
            u16::from_be_bytes(pair)
        } else {
            u16::from_le_bytes(pair)
        })
    };
    let u32_at = |b: &[u8], at: usize, big: bool| {
        let quad = *b.get(at..)?.first_chunk::<4>()?;
        Some(if big {
            u32::from_be_bytes(quad)
        } else {
            u32::from_le_bytes(quad)
        })
    };
    let mut at = 2;
    // The segments before the scan: a marker, a length, its payload.
    while jpeg.get(at) == Some(&0xff) {
        // Fill bytes may stand before a marker.
        while jpeg.get(at + 1) == Some(&0xff) {
            at += 1;
        }
        let Some(&marker) = jpeg.get(at + 1) else {
            break;
        };
        if marker == 0xda || marker == 0xd9 {
            break;
        }
        let Some(length) = u16_at(jpeg, at + 2, true).map(usize::from) else {
            break;
        };
        let payload = jpeg.get(at + 4..at + 2 + length).unwrap_or_default();
        let exif = (marker == 0xe1)
            .then(|| payload.strip_prefix(b"Exif\0\0"))
            .flatten();
        if let Some(tiff) = exif {
            let big = match tiff.get(..2) {
                Some(b"MM") => true,
                Some(b"II") => false,
                _ => return 1,
            };
            let Some(ifd) = u32_at(tiff, 4, big).and_then(|o| usize::try_from(o).ok()) else {
                return 1;
            };
            let count = u16_at(tiff, ifd, big).unwrap_or(0);
            for entry in 0..usize::from(count) {
                let at = ifd.saturating_add(2 + entry * 12);
                if u16_at(tiff, at, big) == Some(0x0112) {
                    return match u16_at(tiff, at.saturating_add(8), big) {
                        Some(o @ 1..=8) => o as u8,
                        _ => 1,
                    };
                }
            }
            return 1;
        }
        at += 2 + length;
    }
    1
}

/// An RGBA picture turned as EXIF orientation `o` says to show it:
/// mirrored, turned a half, or turned a quarter either way, transposed
/// or transversed, the last four swapping its sides.
fn orient(
    width: usize,
    height: usize,
    rgba: Vec<u8>,
    o: u8,
) -> Result<(usize, usize, Vec<u8>), String> {
    if !(2..=8).contains(&o) {
        return Ok((width, height, rgba));
    }
    let (w, h) = if o >= 5 {
        (height, width)
    } else {
        (width, height)
    };
    let mut out = Vec::new();
    out.try_reserve_exact(rgba.len())
        .map_err(|_| "no memory for the picture".to_string())?;
    for y in 0..h {
        for x in 0..w {
            let (sx, sy) = match o {
                2 => (width - 1 - x, y),
                3 => (width - 1 - x, height - 1 - y),
                4 => (x, height - 1 - y),
                5 => (y, x),
                6 => (y, height - 1 - x),
                7 => (width - 1 - y, height - 1 - x),
                _ => (width - 1 - y, x),
            };
            let at = (sy * width + sx) * 4;
            let pixel = rgba
                .get(at..at + 4)
                .ok_or("a picture shorter than its size")?;
            out.extend_from_slice(pixel);
        }
    }
    Ok((w, h, out))
}

/// Whether a pasted picture is to be written anew before it is taken:
/// past the bounds a provider takes (longer than `MAX_PICTURE_BYTES`, or
/// wider or taller than `MAX_PICTURE_AXIS` as its header says), or a JPEG
/// its EXIF says to turn, so what is sent is what is shown, whatever a
/// provider makes of EXIF. One neither PNG nor JPEG is refused.
pub fn must_rewrite(bytes: &[u8]) -> Result<bool, String> {
    let mime = sniff(bytes).ok_or("the picture is neither a PNG nor a JPEG")?;
    Ok(!within_bounds(mime, bytes)? || (mime == JPEG && orientation(bytes) != 1))
}

/// Whether a picture is within the bytes and sides a provider takes.
fn within_bounds(mime: &str, bytes: &[u8]) -> Result<bool, String> {
    let (width, height) = size(mime, bytes)?;
    let axis = MAX_PICTURE_AXIS as usize;
    Ok(bytes.len() as u64 <= MAX_PICTURE_BYTES && width <= axis && height <= axis)
}

/// A picture `must_rewrite` names made one to take: decoded within
/// `SHRUNK_AXIS` a side (a JPEG at its DCT scale, a PNG halved by two by
/// two means), its transparency laid over white, and written as a JPEG at
/// `SHRUNK_QUALITY`, halved again while that is still past
/// `MAX_PICTURE_BYTES`, a JPEG turned as its EXIF says. What it was, for
/// a note: shrunk, or only turned upright where it was within bounds.
pub fn shrink(bytes: &[u8]) -> Result<(Vec<u8>, String), String> {
    shrink_within(bytes, MAX_PICTURE_BYTES)
}

/// `shrink` within `most` bytes.
fn shrink_within(bytes: &[u8], most: u64) -> Result<(Vec<u8>, String), String> {
    let mime = sniff(bytes).ok_or("the picture is neither a PNG nor a JPEG")?;
    let (was_width, was_height) = size(mime, bytes)?;
    let (mut width, mut height, mut rgba) = decode(mime, bytes, SHRUNK_AXIS)?;
    while width.max(height) > SHRUNK_AXIS {
        (width, height, rgba) = halve(width, height, &rgba)?;
    }
    loop {
        let jpeg = jpeg_of(width, height, &rgba)?;
        if jpeg.len() as u64 <= most {
            let said = format!(
                "{} from {was_width}x{was_height} {}, {:.1} MB",
                if within_bounds(mime, bytes)? {
                    "turned upright"
                } else {
                    "shrunk"
                },
                if mime == PNG { "PNG" } else { "JPEG" },
                bytes.len() as f64 / 1e6
            );
            return Ok((jpeg, said));
        }
        if width.max(height) <= 64 {
            return Err("the picture does not come within the bounds".into());
        }
        (width, height, rgba) = halve(width, height, &rgba)?;
    }
}

/// A picture's width and height as its header says.
fn size(mime: &str, bytes: &[u8]) -> Result<(usize, usize), String> {
    match mime {
        PNG => td_png::header(bytes)
            .map(|h| (h.width, h.height))
            .map_err(|e| format!("not a PNG td-agent reads: {e}")),
        JPEG => td_jpeg::header(bytes)
            .map(|h| (h.width, h.height))
            .map_err(|e| format!("not a JPEG td-agent reads: {e}")),
        other => Err(format!("{other} is not a picture td-agent takes")),
    }
}

/// An RGBA picture as a baseline JPEG, its alpha laid over white, fed to
/// the encoder a band of rows at a time.
fn jpeg_of(width: usize, height: usize, rgba: &[u8]) -> Result<Vec<u8>, String> {
    const BAND: usize = 64;
    let pool = Pool(
        std::thread::available_parallelism()
            .map_or(1, |n| n.get())
            .min(SHRINK_THREADS),
    );
    let mut encoder =
        td_jpeg::Encoder::new(width, height, SHRUNK_QUALITY).map_err(|e| e.to_string())?;
    let mut rgb = Vec::new();
    rgb.try_reserve_exact(width.saturating_mul(BAND).saturating_mul(3))
        .map_err(|_| "no memory for the picture".to_string())?;
    for band in rgba.chunks(width.saturating_mul(BAND).saturating_mul(4).max(4)) {
        rgb.clear();
        for &[r, g, b, a] in band.as_chunks::<4>().0 {
            let a = u32::from(a);
            for c in [r, g, b] {
                rgb.push(((u32::from(c) * a + 255 * (255 - a) + 127) / 255) as u8);
            }
        }
        encoder
            .encode_rows(&rgb, &pool)
            .map_err(|e| e.to_string())?;
    }
    encoder.finish(&pool).map_err(|e| e.to_string())
}

/// The encoder's pieces taken from one queue by this many scoped
/// threads, the caller's among them.
struct Pool(usize);

impl td_jpeg::Runner for Pool {
    fn run<T: Send>(&self, items: Vec<T>, work: &(dyn Fn(T) + Sync)) {
        let queue = std::sync::Mutex::new(items);
        let next = || queue.lock().unwrap_or_else(|e| e.into_inner()).pop();
        std::thread::scope(|scope| {
            for _ in 1..self.0 {
                // One that cannot start leaves its share to the others.
                let _ = std::thread::Builder::new().spawn_scoped(scope, || {
                    while let Some(item) = next() {
                        work(item);
                    }
                });
            }
            while let Some(item) = next() {
                work(item);
            }
        });
    }
}

/// A pasted picture, checked: what it is, by its bytes and a full decode,
/// within the bounds a provider takes; labelled `label`.
pub fn check(bytes: &[u8], label: &str) -> Result<Reference, String> {
    let length = bytes.len() as u64;
    if length > MAX_PICTURE_BYTES {
        return Err(format!(
            "a picture is at most {} MB; this one is {:.1} MB",
            MAX_PICTURE_BYTES / 1_000_000,
            length as f64 / 1e6
        ));
    }
    let mime = sniff(bytes).ok_or("the picture is neither a PNG nor a JPEG")?;
    // Its size as its header says, refused before it is decoded whole.
    let (width, height) = size(mime, bytes)?;
    let axis = |n: usize| {
        u32::try_from(n)
            .ok()
            .filter(|&n| (1..=MAX_PICTURE_AXIS).contains(&n))
    };
    let (Some(width), Some(height)) = (axis(width), axis(height)) else {
        return Err(format!(
            "a picture is at most {MAX_PICTURE_AXIS} pixels a side; this one is {width}x{height}"
        ));
    };
    // Decoded whole, so none is taken that cannot be shown; its pixels
    // are not kept.
    match mime {
        PNG => drop(td_png::decode(bytes).map_err(|e| format!("not a PNG td-agent reads: {e}"))?),
        _ => drop(
            td_jpeg::decode(bytes, td_jpeg::Scale::Full)
                .map_err(|e| format!("not a JPEG td-agent reads: {e}"))?,
        ),
    }
    if !label_ok(label) {
        return Err("a malformed picture label".into());
    }
    Ok(Reference {
        digest: crate::sha256::hex_digest(bytes),
        mime,
        width,
        height,
        bytes: length,
        label: label.to_string(),
    })
}

/// Where the window stages conversation `id`'s pictures for its process.
pub fn staging(state: &StateDir, id: &Id) -> PathBuf {
    state.root().join(PASTED).join(id.as_str())
}

/// Where conversation `dir` keeps its pictures.
pub fn kept(dir: &Path) -> PathBuf {
    dir.join(KEPT)
}

/// Writes a checked picture where conversation `id`'s process takes it.
pub fn stage(state: &StateDir, id: &Id, reference: &Reference, bytes: &[u8]) -> Result<(), String> {
    let dir = staging(state, id);
    create_private(&dir)?;
    crate::store::replace(&dir, &reference.digest, bytes)
}

/// Removes a staged picture that will not be taken.
pub fn unstage(state: &StateDir, id: &Id, reference: &Reference) {
    let _ = std::fs::remove_file(staging(state, id).join(&reference.digest));
}

/// A refused message's pictures, read back for its composer: each from
/// where it was staged, or, when the conversation had taken it before
/// refusing the message, from where the conversation keeps it. A staged
/// copy is removed unless `pending` says another message still waiting
/// to be taken names it; one that cannot be read back is missed from the
/// composer.
pub fn unstage_all(
    state: &StateDir,
    id: &Id,
    references: &[Reference],
    pending: impl Fn(&str) -> bool,
) -> Vec<(Reference, std::sync::Arc<[u8]>)> {
    let mut back = Vec::new();
    for reference in references {
        let path = staging(state, id).join(&reference.digest);
        let staged = crate::store::read_bounded(&path, MAX_PICTURE_BYTES)
            .ok()
            .filter(|bytes| verify(reference, bytes).is_ok());
        if let Some(bytes) = staged.or_else(|| read(&state.conversation(id), reference).ok()) {
            back.push((reference.clone(), std::sync::Arc::from(bytes)));
        }
        if !pending(&reference.digest) {
            unstage(state, id, reference);
        }
    }
    back
}

/// Creates `dir` and its parents, owner-only.
fn create_private(dir: &Path) -> Result<(), String> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|e| format!("{}: {e}", dir.display()))
}

/// Takes a staged picture into conversation `dir`'s own: its bytes read
/// whole and checked against the reference, then written once under its
/// digest, durably, and the staged copy removed. A picture already kept,
/// sent again after a restart, is checked where it is.
pub fn adopt(state: &StateDir, id: &Id, dir: &Path, reference: &Reference) -> Result<(), String> {
    let staged = staging(state, id).join(&reference.digest);
    let kept_dir = kept(dir);
    if read(dir, reference).is_ok() {
        let _ = std::fs::remove_file(&staged);
        return Ok(());
    }
    let bytes = crate::store::read_bounded(&staged, MAX_PICTURE_BYTES).map_err(|e| {
        format!(
            "{reference_label}: {e}",
            reference_label = reference.stand_in()
        )
    })?;
    verify(reference, &bytes)?;
    create_private(&kept_dir)?;
    crate::store::replace(&kept_dir, &reference.digest, &bytes)?;
    let _ = std::fs::remove_file(&staged);
    Ok(())
}

/// A picture a tool returned, as its base64 `data`, made one to keep
/// labelled `label`: decoded, a PNG or a JPEG by its bytes whatever the
/// tool called it, written anew as a pasted one is where `must_rewrite`
/// says or where it is past `SHRUNK_AXIS` a side, which no provider looks
/// past and the window need not decode, and checked.
pub fn from_tool(data: &str, label: &str) -> Result<(Reference, Vec<u8>), String> {
    let bytes = td_encoding::from_base64(data).ok_or("its base64 is malformed")?;
    let mime = sniff(&bytes).ok_or("the picture is neither a PNG nor a JPEG")?;
    let (width, height) = size(mime, &bytes)?;
    let bytes = if must_rewrite(&bytes)? || width.max(height) > SHRUNK_AXIS {
        shrink(&bytes)?.0
    } else {
        bytes
    };
    Ok((check(&bytes, label)?, bytes))
}

/// Writes a picture into conversation `dir`'s own, once and durably under
/// its digest; one already kept is checked where it is.
pub fn keep(dir: &Path, reference: &Reference, bytes: &[u8]) -> Result<(), String> {
    if read(dir, reference).is_ok() {
        return Ok(());
    }
    verify(reference, bytes)?;
    let kept_dir = kept(dir);
    create_private(&kept_dir)?;
    crate::store::replace(&kept_dir, &reference.digest, bytes)
}

/// That `bytes` are what `reference` names.
fn verify(reference: &Reference, bytes: &[u8]) -> Result<(), String> {
    if bytes.len() as u64 != reference.bytes
        || crate::sha256::hex_digest(bytes) != reference.digest
        || sniff(bytes) != Some(reference.mime)
    {
        return Err(format!(
            "{} is not the picture its message names",
            reference.stand_in()
        ));
    }
    Ok(())
}

/// A kept picture's bytes, checked against its reference.
pub fn read(dir: &Path, reference: &Reference) -> Result<Vec<u8>, String> {
    let path = kept(dir).join(&reference.digest);
    let bytes = crate::store::read_bounded(&path, MAX_PICTURE_BYTES)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    verify(reference, &bytes)?;
    Ok(bytes)
}

/// The pictures a conversation's messages and tool results carry, each
/// as often as one names it: what a request body's placeholders may stand
/// for.
pub fn known(events: &[crate::store::Event]) -> Vec<Reference> {
    events
        .iter()
        .filter_map(|e| match &e.kind {
            crate::store::Kind::User { pictures, .. }
            | crate::store::Kind::ToolResult { pictures, .. } => Some(pictures.iter()),
            _ => None,
        })
        .flatten()
        .cloned()
        .collect()
}

/// The placeholder parts in a request body that stand for a picture in
/// `known`, each with its byte range in the body and the reference it
/// names. A placeholder is found only as the exact part `Reference::part`
/// writes, whose quotes a JSON string's text can never hold unescaped;
/// one that names no picture of `known`, or names one more often than
/// the messages do, is left as it is, so JSON another party wrote into
/// the body (a provider's reasoning details, an MCP server's schema)
/// can neither fail a request nor read past the conversation's own
/// pictures.
fn placeholders<'a>(
    body: &str,
    known: &'a [Reference],
) -> Vec<(std::ops::Range<usize>, &'a Reference)> {
    const OPEN: &str = "{\"type\":\"image_url\",\"image_url\":{\"url\":\"td-picture:";
    let mut left: Vec<Option<&Reference>> = known.iter().map(Some).collect();
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(found) = body.get(from..).and_then(|rest| rest.find(OPEN)) {
        let start = from + found;
        let fields = start + OPEN.len();
        from = fields;
        // Its URL ends at the first quote, which must close the part.
        let Some(quote) = body.get(fields..).and_then(|rest| rest.find('"')) else {
            break;
        };
        let end = fields + quote;
        if body.get(end..end + 3) != Some("\"}}") {
            continue;
        }
        let inner = body.get(fields..end).unwrap_or_default();
        let named = left.iter_mut().find(|r| {
            r.is_some_and(|r| inner == format!("{};{}x{};{}", r.mime, r.width, r.height, r.digest))
        });
        if let Some(reference) = named.and_then(Option::take) {
            from = end + 3;
            out.push((start..from, reference));
        }
    }
    out
}

/// What pictures weigh in a request as §14's estimate counts it: four
/// bytes for each token each costs.
pub fn weight(pictures: &[Reference]) -> u64 {
    pictures
        .iter()
        .map(|p| tokens(p).saturating_mul(4))
        .fold(0, u64::saturating_add)
}

/// The tokens a picture is counted as: its pixels over 750, as providers
/// count them, at most `MAX_PICTURE_TOKENS`.
fn tokens(reference: &Reference) -> u64 {
    (u64::from(reference.width) * u64::from(reference.height) / 750 + 1).min(MAX_PICTURE_TOKENS)
}

/// A request body's size as §14's estimate counts it: its bytes, each
/// picture's placeholder counted as four bytes for each token the
/// picture costs rather than as its own bytes.
pub fn weighted(body: &str, known: &[Reference]) -> u64 {
    placeholders(body, known)
        .iter()
        .fold(body.len() as u64, |size, (range, reference)| {
            size.saturating_sub(range.len() as u64)
                .saturating_add(tokens(reference).saturating_mul(4))
        })
}

/// `body` as it is sent: each placeholder part of a picture in `known`
/// the picture's data URL, read from conversation `dir` and checked; or,
/// where `taken` is false (the model takes no pictures), or the picture
/// cannot be read, a text part saying it was left out and why.
pub fn inline(body: &str, dir: &Path, taken: bool, known: &[Reference]) -> String {
    let found = placeholders(body, known);
    if found.is_empty() {
        return body.to_string();
    }
    let mut out = String::new();
    let mut at = 0;
    for (range, reference) in found {
        out.push_str(body.get(at..range.start).unwrap_or_default());
        at = range.end;
        let read = if taken {
            read(dir, reference).map(Some)
        } else {
            Ok(None)
        };
        let part = match read {
            Ok(Some(bytes)) => Json::Obj(vec![
                ("type".into(), Json::Str("image_url".into())),
                (
                    "image_url".into(),
                    Json::Obj(vec![(
                        "url".into(),
                        Json::Str(format!(
                            "data:{};base64,{}",
                            reference.mime,
                            td_encoding::base64(&bytes)
                        )),
                    )]),
                ),
            ]),
            left_out => Json::Obj(vec![
                ("type".into(), Json::Str("text".into())),
                (
                    "text".into(),
                    Json::Str(format!(
                        "[td-agent: the {}x{} picture {} is left out: {}]",
                        reference.width,
                        reference.height,
                        reference.stand_in(),
                        match left_out {
                            Err(why) => format!("it could not be read ({why})"),
                            _ => "this model takes no pictures".into(),
                        }
                    )),
                ),
            ]),
        };
        out.push_str(&part.to_string());
    }
    out.push_str(body.get(at..).unwrap_or_default());
    out
}

#[cfg(test)]
pub(crate) mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::unreachable
    )]

    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("td-agent-picture-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = !0u32;
        for &b in bytes {
            crc ^= u32::from(b);
            for _ in 0..8 {
                crc = if crc & 1 == 1 {
                    (crc >> 1) ^ 0xedb8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }

    fn chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut out = (data.len() as u32).to_be_bytes().to_vec();
        let mut body = kind.to_vec();
        body.extend_from_slice(data);
        out.extend_from_slice(&body);
        out.extend_from_slice(&crc32(&body).to_be_bytes());
        out
    }

    /// A `width` by `height` eight-bit RGB PNG of one colour.
    pub(crate) fn png(width: u32, height: u32, rgb: [u8; 3]) -> Vec<u8> {
        let pixels = rgb.repeat(width as usize * height as usize);
        png_of(width, height, 3, &pixels)
    }

    /// A `width` by `height` eight-bit PNG of these RGB (`channels` 3) or
    /// RGBA (4) pixels, in stored deflate blocks.
    fn png_of(width: u32, height: u32, channels: u8, pixels: &[u8]) -> Vec<u8> {
        let stride = width as usize * usize::from(channels);
        let raw: Vec<u8> = pixels
            .chunks(stride)
            .flat_map(|row| std::iter::once(0).chain(row.iter().copied()))
            .collect();
        let (mut a, mut b) = (1u32, 0u32);
        for &byte in &raw {
            a = (a + u32::from(byte)) % 65521;
            b = (b + a) % 65521;
        }
        // Stored blocks, each at most a 16-bit length, the last marked.
        let mut z = vec![0x78, 0x01];
        let blocks: Vec<&[u8]> = raw.chunks(65535).collect();
        for (i, block) in blocks.iter().enumerate() {
            z.push(u8::from(i + 1 == blocks.len()));
            z.extend_from_slice(&(block.len() as u16).to_le_bytes());
            z.extend_from_slice(&(!(block.len() as u16)).to_le_bytes());
            z.extend_from_slice(block);
        }
        z.extend_from_slice(&((b << 16) | a).to_be_bytes());
        let mut ihdr = width.to_be_bytes().to_vec();
        ihdr.extend_from_slice(&height.to_be_bytes());
        ihdr.extend_from_slice(&[8, if channels == 4 { 6 } else { 2 }, 0, 0, 0]);
        let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
        out.extend(chunk(b"IHDR", &ihdr));
        out.extend(chunk(b"IDAT", &z));
        out.extend(chunk(b"IEND", &[]));
        out
    }

    /// A `width` by `height` JPEG of the colour `at(x, y)` gives, written
    /// by td-jpeg's encoder.
    fn jpeg_of_rgb(width: usize, height: usize, at: impl Fn(usize, usize) -> [u8; 3]) -> Vec<u8> {
        let rgb: Vec<u8> = (0..height)
            .flat_map(|y| (0..width).map(move |x| (x, y)))
            .flat_map(|(x, y)| at(x, y))
            .collect();
        let mut encoder = td_jpeg::Encoder::new(width, height, 95).unwrap();
        encoder.encode_rows(&rgb, &td_jpeg::InOrder).unwrap();
        encoder.finish(&td_jpeg::InOrder).unwrap()
    }

    /// `jpeg` with an EXIF APP1 segment after its SOI saying orientation
    /// `o`, its TIFF big- or little-endian.
    fn exif(jpeg: &[u8], o: u16, big: bool) -> Vec<u8> {
        let u16b = |v: u16| {
            if big {
                v.to_be_bytes()
            } else {
                v.to_le_bytes()
            }
        };
        let u32b = |v: u32| {
            if big {
                v.to_be_bytes()
            } else {
                v.to_le_bytes()
            }
        };
        let mut tiff = if big { b"MM".to_vec() } else { b"II".to_vec() };
        tiff.extend(u16b(42));
        tiff.extend(u32b(8));
        tiff.extend(u16b(1));
        tiff.extend(u16b(0x0112));
        tiff.extend(u16b(3));
        tiff.extend(u32b(1));
        tiff.extend(u16b(o));
        tiff.extend([0, 0]);
        tiff.extend(u32b(0));
        let mut payload = b"Exif\0\0".to_vec();
        payload.extend(tiff);
        let mut out = jpeg[..2].to_vec();
        out.extend([0xff, 0xe1]);
        out.extend(((payload.len() + 2) as u16).to_be_bytes());
        out.extend(payload);
        out.extend_from_slice(&jpeg[2..]);
        out
    }

    /// An eight-by-eight grey baseline JPEG: one component, every
    /// coefficient zero, so each sample is mid-grey.
    fn jpeg() -> Vec<u8> {
        let mut out = vec![0xff, 0xd8];
        // DQT: table 0, every step 1.
        out.extend_from_slice(&[0xff, 0xdb, 0, 67, 0]);
        out.extend_from_slice(&[1; 64]);
        // SOF0: eight bits, 8x8, one component sampled 1x1 on table 0.
        out.extend_from_slice(&[0xff, 0xc0, 0, 11, 8, 0, 8, 0, 8, 1, 1, 0x11, 0]);
        // DHT: DC table 0 and AC table 0, each one one-bit code, for
        // category 0 and for end of block.
        for class in [0x00, 0x10] {
            out.extend_from_slice(&[0xff, 0xc4, 0, 20, class, 1]);
            out.extend_from_slice(&[0; 15]);
            out.push(0);
        }
        // SOS, then the one block: DC difference 0, end of block.
        out.extend_from_slice(&[0xff, 0xda, 0, 8, 1, 1, 0x00, 0, 63, 0]);
        out.extend_from_slice(&[0x3f, 0xff, 0xd9]);
        out
    }

    #[test]
    fn a_png_is_checked_and_named() {
        let bytes = png(3, 2, [10, 20, 30]);
        let r = check(&bytes, "image 1").unwrap();
        assert_eq!((r.mime, r.width, r.height), (PNG, 3, 2));
        assert_eq!(r.bytes, bytes.len() as u64);
        assert_eq!(r.digest, crate::sha256::hex_digest(&bytes));
        assert_eq!(r.stand_in(), "[image 1]");
        let (w, h, rgba) = decode(PNG, &bytes, usize::MAX).unwrap();
        assert_eq!((w, h), (3, 2));
        assert_eq!(rgba.get(..4), Some(&[10, 20, 30, 255][..]));
    }

    #[test]
    fn a_jpeg_is_checked_and_decoded_opaque() {
        let bytes = jpeg();
        let r = check(&bytes, "image 2").unwrap();
        assert_eq!((r.mime, r.width, r.height), (JPEG, 8, 8));
        let (w, h, rgba) = decode(JPEG, &bytes, usize::MAX).unwrap();
        assert_eq!((w, h, rgba.len()), (8, 8, 8 * 8 * 4));
        assert!(rgba.as_chunks::<4>().0.iter().all(|p| p[3] == 255));
        // Shown no wider than four, it is decoded at half scale.
        let (w, h, _) = decode(JPEG, &bytes, 4).unwrap();
        assert_eq!((w, h), (4, 4));
    }

    #[test]
    fn a_png_too_large_to_show_is_halved_until_it_fits() {
        let bytes = png(5, 3, [200, 100, 50]);
        let (w, h, rgba) = decode(PNG, &bytes, 2).unwrap();
        assert_eq!((w, h), (2, 1));
        assert_eq!(rgba, [200, 100, 50, 255, 200, 100, 50, 255]);
        let (rgba, odd) = ([0, 0, 0, 255, 255, 255, 255, 255, 9, 9, 9, 9], 3);
        assert_eq!(
            halve(odd, 1, &rgba).unwrap(),
            (2, 1, vec![128, 128, 128, 255, 9, 9, 9, 9])
        );
    }

    /// A picture past the bounds is brought within `SHRUNK_AXIS` and
    /// written as a JPEG that `check` takes, its transparency over white.
    #[test]
    fn a_picture_past_the_bounds_is_shrunk_to_one_within() {
        let wide = png(MAX_PICTURE_AXIS + 2, 3, [10, 200, 30]);
        assert!(must_rewrite(&wide).unwrap());
        assert!(check(&wide, "image 1").is_err());
        let (shrunk, said) = shrink(&wide).unwrap();
        assert!(!must_rewrite(&shrunk).unwrap());
        let r = check(&shrunk, "image 1").unwrap();
        assert_eq!(r.mime, JPEG);
        assert_eq!((r.width, r.height), (2001, 1));
        assert!(said.starts_with("shrunk from 8002x3 PNG, "), "{said}");
        let (_, _, rgba) = decode(JPEG, &shrunk, usize::MAX).unwrap();
        let [r, g, b, _] = rgba[..4] else {
            unreachable!()
        };
        assert!(r.abs_diff(10) <= 8 && g.abs_diff(200) <= 8 && b.abs_diff(30) <= 8);
        // Transparency is laid over white.
        assert_eq!(
            jpeg_of(1, 1, &[0, 0, 0, 0]).map(|j| decode(JPEG, &j, usize::MAX).unwrap().2),
            Ok(vec![255, 255, 255, 255])
        );
        assert!(!must_rewrite(&png(2, 2, [0; 3])).unwrap());
        assert!(must_rewrite(b"GIF89a").is_err());
        // Past the bytes alone, within the sides: written as a JPEG at its
        // own size.
        let heavy = png(1400, 1300, [90; 3]);
        assert!(heavy.len() as u64 > MAX_PICTURE_BYTES);
        assert!(must_rewrite(&heavy).unwrap());
        let (shrunk, said) = shrink(&heavy).unwrap();
        let r = check(&shrunk, "image 1").unwrap();
        assert_eq!((r.mime, r.width, r.height), (JPEG, 1400, 1300));
        assert!(said.starts_with("shrunk from 1400x1300 PNG, "), "{said}");
    }

    /// A transparent pixel's colour, which nothing shows, does not tint
    /// its neighbours as the picture is halved: transparent black between
    /// opaque white comes out white over white.
    #[test]
    fn a_halved_pictures_transparent_pixels_do_not_tint_it() {
        let pixels: Vec<u8> = (0..8002 * 4)
            .flat_map(|i| if i % 2 == 0 { [0, 0, 0, 0] } else { [255; 4] })
            .collect();
        let (shrunk, _) = shrink(&png_of(8002, 4, 4, &pixels)).unwrap();
        let (w, h, rgba) = decode(JPEG, &shrunk, usize::MAX).unwrap();
        assert_eq!((w, h), (2001, 1));
        assert!(rgba.iter().all(|&c| c >= 250), "{:?}", &rgba[..16]);
        // Halving alone: colour weighted by alpha, alpha the mean.
        let (_, _, half) = halve(2, 1, &[0, 0, 0, 0, 200, 100, 50, 128]).unwrap();
        assert_eq!(half, [200, 100, 50, 64]);
        let (_, _, none) = halve(2, 1, &[9, 9, 9, 0, 7, 7, 7, 0]).unwrap();
        assert_eq!(none, [0, 0, 0, 0]);
    }

    /// Still past the bytes once written, a picture is halved again until
    /// it is within them.
    #[test]
    fn a_shrunk_picture_still_too_long_is_halved_again() {
        let mut seed = 7u32;
        let pixels: Vec<u8> = (0..256 * 256 * 3)
            .map(|_| {
                seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
                (seed >> 16) as u8
            })
            .collect();
        let bytes = png_of(256, 256, 3, &pixels);
        let (_, _, rgba) = decode(PNG, &bytes, usize::MAX).unwrap();
        let whole = jpeg_of(256, 256, &rgba).unwrap().len() as u64;
        let (shrunk, _) = shrink_within(&bytes, whole - 1).unwrap();
        let r = check(&shrunk, "image 1").unwrap();
        assert_eq!((r.width, r.height), (128, 128));
        assert!((shrunk.len() as u64) < whole);
        // Nothing comes within no bytes: refused once it is small.
        assert!(shrink_within(&bytes, 0).is_err());
    }

    /// A JPEG past the sides is decoded at the DCT scale that brings it
    /// within `SHRUNK_AXIS`.
    #[test]
    fn a_jpeg_past_the_bounds_is_shrunk_at_its_dct_scale() {
        let wide = jpeg_of_rgb(8002, 8, |_, _| [40, 120, 220]);
        assert!(must_rewrite(&wide).unwrap());
        let (shrunk, said) = shrink(&wide).unwrap();
        let r = check(&shrunk, "image 1").unwrap();
        assert_eq!((r.width, r.height), (2001, 2));
        assert!(said.starts_with("shrunk from 8002x8 JPEG, "), "{said}");
    }

    /// A JPEG is shown, and written anew, turned as its EXIF orientation
    /// says, read big- or little-endian: every one of the eight.
    #[test]
    fn a_jpeg_is_turned_as_its_exif_orientation_says() {
        // Thirty-two by sixteen in four quarters: red, green over blue,
        // yellow.
        const QUARTERS: [[u8; 3]; 4] = [[255, 0, 0], [0, 255, 0], [0, 0, 255], [255, 255, 0]];
        let stored = jpeg_of_rgb(32, 16, |x, y| {
            QUARTERS[usize::from(y >= 8) * 2 + usize::from(x >= 16)]
        });
        let quarter = |p: &[u8]| {
            (0..4)
                .min_by_key(|&q| {
                    QUARTERS[q]
                        .iter()
                        .zip(p)
                        .map(|(&a, &b)| u32::from(a.abs_diff(b)).pow(2))
                        .sum::<u32>()
                })
                .unwrap()
        };
        // The quarters shown at the top left and the top right.
        let (r, g, b, y) = (0, 1, 2, 3);
        for (o, corners) in [
            (1, (r, g)),
            (2, (g, r)),
            (3, (y, b)),
            (4, (b, y)),
            (5, (r, b)),
            (6, (b, r)),
            (7, (y, g)),
            (8, (g, y)),
        ] {
            let bytes = exif(&stored, o, o % 2 == 0);
            assert_eq!(u16::from(orientation(&bytes)), o);
            let (w, h, rgba) = decode(JPEG, &bytes, usize::MAX).unwrap();
            let size = if o >= 5 { (16, 32) } else { (32, 16) };
            assert_eq!((w, h), size, "orientation {o}");
            let top_right = (w - 1) * 4;
            assert_eq!(
                (
                    quarter(&rgba[..4]),
                    quarter(&rgba[top_right..top_right + 4])
                ),
                corners,
                "orientation {o}"
            );
            // Turned, it is written anew, so what is sent is what is shown.
            assert_eq!(must_rewrite(&bytes).unwrap(), o != 1);
            if o == 1 {
                continue;
            }
            let (shrunk, said) = shrink(&bytes).unwrap();
            assert!(
                said.starts_with("turned upright from 32x16 JPEG, "),
                "{said}"
            );
            let r = check(&shrunk, "image 1").unwrap();
            assert_eq!((r.width as usize, r.height as usize), size);
            assert_eq!(orientation(&shrunk), 1);
        }
        assert_eq!(orientation(&stored), 1);
        assert_eq!(orientation(&exif(&stored, 9, true)), 1);
        let mut cut = exif(&stored, 6, true);
        cut.truncate(20);
        assert_eq!(orientation(&cut), 1);
        // Fill bytes before the marker are passed over.
        let turned = exif(&stored, 6, true);
        let mut filled = turned[..2].to_vec();
        filled.extend([0xff, 0xff]);
        filled.extend_from_slice(&turned[2..]);
        assert_eq!(orientation(&filled), 6);
    }

    /// A tool's picture is decoded from its base64, a PNG or a JPEG by
    /// its bytes, written anew past the bounds as a pasted one is, and
    /// kept once under its digest; the conversation's tool results' are
    /// known to its requests.
    #[test]
    fn a_tools_picture_is_decoded_kept_and_known() {
        let bytes = png(2, 1, [9, 8, 7]);
        let (reference, decoded) = from_tool(&td_encoding::base64(&bytes), "tool image 1").unwrap();
        assert_eq!(decoded, bytes);
        assert_eq!(reference, check(&bytes, "tool image 1").unwrap());
        assert_eq!(reference.stand_in(), "[tool image 1]");
        assert!(from_tool("not base64", "tool image 1")
            .unwrap_err()
            .contains("base64"));
        assert_eq!(
            from_tool(&td_encoding::base64(b"GIF89a"), "tool image 1").unwrap_err(),
            "the picture is neither a PNG nor a JPEG"
        );
        let wide = png(MAX_PICTURE_AXIS + 2, 3, [1; 3]);
        let (shrunk, _) = from_tool(&td_encoding::base64(&wide), "tool image 2").unwrap();
        assert_eq!((shrunk.mime, shrunk.width, shrunk.height), (JPEG, 2001, 1));
        // Within the bounds but past what a provider looks at: shrunk too,
        // where a paste would be kept as it is.
        let large = png(SHRUNK_AXIS as u32 + 2, 2, [1; 3]);
        assert!(!must_rewrite(&large).unwrap());
        let (halved, _) = from_tool(&td_encoding::base64(&large), "tool image 3").unwrap();
        assert_eq!((halved.mime, halved.width, halved.height), (JPEG, 1025, 1));
        let at_most = png(SHRUNK_AXIS as u32, 1, [1; 3]);
        let (kept, _) = from_tool(&td_encoding::base64(&at_most), "tool image 4").unwrap();
        assert_eq!((kept.mime, kept.width), (PNG, 2048));

        let dir = scratch("tool");
        keep(&dir, &reference, &decoded).unwrap();
        assert_eq!(read(&dir, &reference).unwrap(), bytes);
        keep(&dir, &reference, &decoded).unwrap();
        // Bytes that are not what the reference names are not kept.
        let other = check(&png(1, 1, [0; 3]), "tool image 3").unwrap();
        assert!(keep(&dir, &other, &bytes).is_err());
        assert!(read(&dir, &other).is_err());

        let events = [crate::store::Event {
            seq: 1,
            time: 0,
            kind: crate::store::Kind::ToolResult {
                reply: 0,
                id: "c1".into(),
                name: "mcp__s__t".into(),
                call: 0,
                content: "[tool image 1]".into(),
                error: false,
                kept: None,
                digest: None,
                digests: Vec::new(),
                pictures: vec![reference.clone()],
            },
        }];
        assert_eq!(known(&events), [reference]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn what_is_not_a_picture_within_bounds_is_refused() {
        assert!(check(b"GIF89a", "image 1").is_err());
        assert!(check(&png(2, 2, [0; 3]), "image]").is_err());
        assert!(check(&png(2, 2, [0; 3]), "").is_err());
        // Too wide, said from its header before any pixel is decoded:
        // its pixels are cut short, which a decode would refuse first.
        let mut wide = png(MAX_PICTURE_AXIS + 1, 1, [0; 3]);
        wide.truncate(33);
        let why = check(&wide, "image 1").unwrap_err();
        assert!(why.contains("pixels a side"), "{why}");
        let mut long = png(1, 1, [0; 3]);
        long.resize(MAX_PICTURE_BYTES as usize + 1, 0);
        let why = check(&long, "image 1").unwrap_err();
        assert!(why.contains("at most 5 MB"), "{why}");
    }

    #[test]
    fn a_reference_reads_back_as_written_and_bounded() {
        let r = check(&png(2, 2, [0; 3]), "image 1").unwrap();
        assert_eq!(Reference::from_json(&r.to_json()).unwrap(), r);
        let with = |name: &str, value: Json| {
            let Json::Obj(mut fields) = r.to_json() else {
                unreachable!()
            };
            for field in &mut fields {
                if field.0 == name {
                    field.1 = value.clone();
                }
            }
            Reference::from_json(&Json::Obj(fields))
        };
        assert!(with("digest", Json::Str("../../x".into())).is_err());
        assert!(with("mime", Json::Str("image/gif".into())).is_err());
        assert!(with("width", Json::from(0u64)).is_err());
        assert!(with("height", Json::from(u64::from(MAX_PICTURE_AXIS) + 1)).is_err());
        assert!(with("bytes", Json::from(MAX_PICTURE_BYTES + 1)).is_err());
        assert!(with("label", Json::Str("a]b".into())).is_err());
    }

    #[test]
    fn a_staged_picture_is_adopted_once_and_checked() {
        let state = StateDir::at(scratch("adopt"));
        let id = Id::parse(&"a".repeat(32)).unwrap();
        let dir = state.conversation(&id);
        let bytes = png(2, 2, [1, 2, 3]);
        let r = check(&bytes, "image 1").unwrap();
        stage(&state, &id, &r, &bytes).unwrap();
        adopt(&state, &id, &dir, &r).unwrap();
        assert_eq!(read(&dir, &r).unwrap(), bytes);
        assert!(!staging(&state, &id).join(&r.digest).exists());
        // Sent again after a restart, it is found where it is kept.
        adopt(&state, &id, &dir, &r).unwrap();
        // Staged bytes that are not the picture named are refused.
        let other = png(2, 2, [9, 9, 9]);
        let mut lying = check(&other, "image 2").unwrap();
        stage(&state, &id, &lying, &other).unwrap();
        lying.bytes += 1;
        assert!(adopt(&state, &id, &dir, &lying).is_err());
        assert!(read(&dir, &lying).is_err());
    }

    #[test]
    fn a_refused_messages_pictures_come_back_staged_or_kept() {
        let state = StateDir::at(scratch("back"));
        let id = Id::parse(&"b".repeat(32)).unwrap();
        let dir = state.conversation(&id);
        let (one, two) = (png(1, 1, [1; 3]), png(1, 1, [2; 3]));
        let (r1, r2) = (
            check(&one, "image 1").unwrap(),
            check(&two, "image 2").unwrap(),
        );
        stage(&state, &id, &r1, &one).unwrap();
        stage(&state, &id, &r2, &two).unwrap();
        // The first taken before the message was refused.
        adopt(&state, &id, &dir, &r1).unwrap();
        let back = unstage_all(&state, &id, &[r1.clone(), r2.clone()], |_| false);
        let back: Vec<_> = back.into_iter().map(|(r, b)| (r, b.to_vec())).collect();
        assert_eq!(back, [(r1, one), (r2.clone(), two)]);
        assert!(!staging(&state, &id).join(&r2.digest).exists());
    }

    #[test]
    fn a_request_carries_each_picture_as_sent() {
        let state = StateDir::at(scratch("inline"));
        let id = Id::parse(&"c".repeat(32)).unwrap();
        let dir = state.conversation(&id);
        let bytes = png(2, 1, [5; 3]);
        let r = check(&bytes, "image 1").unwrap();
        stage(&state, &id, &r, &bytes).unwrap();
        adopt(&state, &id, &dir, &r).unwrap();
        let known = [r.clone()];
        // The person's own text that spells a placeholder out is text.
        let spelled = r.part().to_string();
        let message = crate::prompt::message_parts("user", &spelled, &known);
        let body = format!("{{\"messages\":[{message}]}}");
        assert_eq!(placeholders(&body, &known).len(), 1);
        let sent = inline(&body, &dir, true, &known);
        let parsed = td_json::parse(&sent).unwrap();
        let messages = parsed.get("messages").and_then(|m| m.as_arr()).unwrap();
        let content = messages[0].get("content").and_then(|c| c.as_arr()).unwrap();
        assert_eq!(
            content[0].get("text").and_then(Json::as_str),
            Some(spelled.as_str())
        );
        assert_eq!(
            content[1].get("text").and_then(Json::as_str),
            Some("[image 1]")
        );
        let url = content[2]
            .get("image_url")
            .and_then(|u| u.get("url"))
            .and_then(Json::as_str)
            .unwrap();
        assert_eq!(
            url,
            format!("data:image/png;base64,{}", td_encoding::base64(&bytes))
        );
        // To a model that takes none, a word in its place.
        let said = inline(&body, &dir, false, &known);
        assert!(
            said.contains("the 2x1 picture [image 1] is left out: this model takes no pictures"),
            "{said}"
        );
        assert!(placeholders(&said, &known).is_empty());
        // One that cannot be read is said to be left out, and why; the
        // request still goes.
        std::fs::remove_file(kept(&dir).join(&r.digest)).unwrap();
        let said = inline(&body, &dir, true, &known);
        assert!(said.contains("it could not be read"), "{said}");
    }

    /// Only the pictures the person's messages carry, each as often as
    /// they carry it, are expanded or weighed: a placeholder that JSON
    /// another party wrote spells out is left as it is.
    #[test]
    fn a_placeholder_no_message_carries_is_left_as_it_is() {
        let state = StateDir::at(scratch("forged"));
        let id = Id::parse(&"d".repeat(32)).unwrap();
        let dir = state.conversation(&id);
        let bytes = png(30, 25, [5; 3]);
        let r = check(&bytes, "image 1").unwrap();
        stage(&state, &id, &r, &bytes).unwrap();
        adopt(&state, &id, &dir, &r).unwrap();
        let known = [r.clone()];
        let part = r.part().to_string();
        let mut other = check(&png(1, 1, [0; 3]), "image 2").unwrap();
        other.digest = "e".repeat(64);
        // A provider's details naming the picture a second time, and an
        // unknown one; and a broken part before a good one.
        let body = format!(
            "[{part},{{\"details\":[{part},{}]}},{{\"type\":\"image_url\",\"image_url\":{{\"url\":\"td-picture:x\"}},{part}]",
            other.part()
        );
        let found = placeholders(&body, &known);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0.start, 1);
        let sent = inline(&body, &dir, true, &known);
        assert_eq!(sent.matches("data:image/png;base64,").count(), 1);
        assert_eq!(sent.matches(&part).count(), 2);
        assert!(sent.contains(&other.part().to_string()));
        // Carried three times, each is expanded: the broken part does
        // not hide the good one after it.
        let thrice = [r.clone(), r.clone(), r.clone()];
        let found = placeholders(&body, &thrice);
        assert_eq!(found.len(), 3);
        assert_eq!(found[2].0.end, body.len() - 1);
        // Weighed as the tokens it costs: 30x25/750 + 1 = 2, eight bytes.
        assert_eq!(
            weighted(&part, &known),
            8,
            "{part} is weighed as its picture"
        );
        assert_eq!(weighted(&part, &[]), part.len() as u64);
    }
}
