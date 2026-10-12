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
            Ok((image.width, image.height, rgba))
        }
        other => Err(format!("{other} is not a picture td-agent takes")),
    }
}

/// An RGBA picture at half its size, each pixel the mean of the two by
/// two it covers, an odd last row or column alone.
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
            let mut sum = [0u32; 4];
            let mut n = 0;
            for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                let (sx, sy) = (2 * x + dx, 2 * y + dy);
                if sx >= width || sy >= height {
                    continue;
                }
                let p = pixel(sx, sy).ok_or("a picture shorter than its size")?;
                for (total, channel) in sum.iter_mut().zip(p) {
                    *total += u32::from(channel);
                }
                n += 1;
            }
            out.extend(sum.map(|total| ((total + n / 2) / n.max(1)) as u8));
        }
    }
    Ok((w, h, out))
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
    let mime = sniff(bytes).ok_or("the clipboard's picture is neither a PNG nor a JPEG")?;
    // Its size as its header says, refused before it is decoded whole.
    let (width, height) = match mime {
        PNG => td_png::header(bytes)
            .map(|h| (h.width, h.height))
            .map_err(|e| format!("not a PNG td-agent reads: {e}"))?,
        _ => td_jpeg::header(bytes)
            .map(|h| (h.width, h.height))
            .map_err(|e| format!("not a JPEG td-agent reads: {e}"))?,
    };
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

/// The pictures a conversation's messages carry, each as often as a
/// message names it: what a request body's placeholders may stand for.
pub fn known(events: &[crate::store::Event]) -> Vec<Reference> {
    events
        .iter()
        .filter_map(|e| match &e.kind {
            crate::store::Kind::User { pictures, .. } => Some(pictures.iter()),
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
                        "[td-agent: the {}x{} picture {} the person pasted is left out: {}]",
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

    /// A `width` by `height` eight-bit RGB PNG of one colour, its pixels
    /// in one stored deflate block.
    pub(crate) fn png(width: u32, height: u32, rgb: [u8; 3]) -> Vec<u8> {
        let row: Vec<u8> = std::iter::once(0)
            .chain(rgb.iter().copied().cycle().take(3 * width as usize))
            .collect();
        let raw = row.repeat(height as usize);
        let (mut a, mut b) = (1u32, 0u32);
        for &byte in &raw {
            a = (a + u32::from(byte)) % 65521;
            b = (b + a) % 65521;
        }
        let mut z = vec![0x78, 0x01, 1];
        z.extend_from_slice(&(raw.len() as u16).to_le_bytes());
        z.extend_from_slice(&(!(raw.len() as u16)).to_le_bytes());
        z.extend_from_slice(&raw);
        z.extend_from_slice(&((b << 16) | a).to_be_bytes());
        let mut ihdr = width.to_be_bytes().to_vec();
        ihdr.extend_from_slice(&height.to_be_bytes());
        ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
        let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
        out.extend(chunk(b"IHDR", &ihdr));
        out.extend(chunk(b"IDAT", &z));
        out.extend(chunk(b"IEND", &[]));
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
            said.contains("the 2x1 picture [image 1] the person pasted is left out: this model takes no pictures"),
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
