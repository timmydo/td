//! The pictures an HTML message carries in its own parts, for its `cid:`
//! URLs (README "HTML mail"): asked of the account's own server, never
//! another, one at a time, each no larger than `MAX_BODY`; decoded
//! through td-picture within `TERM`, which a decoding's reckoned peak is
//! charged to first and the pictures kept fill; and kept for the
//! message's pages to lay, each laid in a copy fitted to its box within
//! `FITTED`. A picture that does not come, or does not decode or fit,
//! leaves its `img` as the page lays one without a picture.

use crate::jmap::types::{BodyPart, Email};
use td_alloc::{Budget, BudgetShared};
use td_page::inline::Inline;

/// The largest picture a message's part is fetched for.
pub const MAX_BODY: u64 = 2 << 20;
/// The most pictures one message asks for.
pub const MAX_PICTURES: usize = 16;
/// The term a message's decoded pictures are kept in, each decoding's
/// reckoned peak charged to it first: no one picture needs more, and
/// together they fill it.
pub const TERM: usize = 24 << 20;
/// The term the copies fitted to a layout's boxes are charged to.
pub const FITTED: usize = 16 << 20;
/// The overlap steps one layout's fitted copies may take (td-web's).
pub const FIT_WORK: u64 = 32 << 20;
/// The most distinct `cid:` URLs read from the HTML, the longest of them,
/// and the most parts matched against them: hostile HTML or a message of
/// many parts costs a bounded scan.
const MAX_URLS: usize = 64;
const MAX_URL: usize = 1024;
const MAX_PARTS: usize = 256;
const LIMITS: td_picture::Limits = td_picture::Limits {
    body: MAX_BODY,
    work: TERM,
};
/// The part types td-picture decodes, as mailers name them: another is
/// not fetched.
const DECODED: &[&str] = &[
    "image/png",
    "image/jpeg",
    "image/pjpeg",
    "image/jpg",
    "image/gif",
    "image/webp",
    "image/avif",
];

/// One part a message's HTML names by its Content-ID.
pub struct Wanted {
    pub blob_id: String,
    pub cid: String,
    pub content_type: String,
}

/// A message's pictures: the parts asked for, the one awaited, and those
/// come and decoded.
pub struct Pictures {
    email: String,
    asked: Vec<Wanted>,
    /// The next of `asked` to fetch, and the one fetched and not answered.
    next: usize,
    waiting: Option<usize>,
    shown: Vec<(usize, BudgetShared<Inline>)>,
    term: Budget,
    fitted: Budget,
}

impl Pictures {
    /// The pictures `email`'s `html` names by `cid:` URL among its parts,
    /// at most `MAX_PICTURES`, to be asked for; none when it names none,
    /// or when td-alloc refuses the terms.
    pub fn of(email: &Email, html: &str) -> Option<Self> {
        let urls = cid_urls(html)?;
        if urls.is_empty() {
            return None;
        }
        let mut asked = Vec::new();
        let parts = [email.html_body.as_deref(), email.attachments.as_deref()];
        for part in parts.into_iter().flatten().flatten().take(MAX_PARTS) {
            if asked.len() >= MAX_PICTURES {
                break;
            }
            let Some(wanted) = wanted(part, &urls) else {
                continue;
            };
            if asked.iter().any(|had: &Wanted| had.cid == wanted.cid) {
                continue;
            }
            td_alloc::push(&mut asked, wanted).ok()?;
        }
        if asked.is_empty() {
            return None;
        }
        Some(Self {
            email: td_alloc::string(&email.id).ok()?,
            asked,
            next: 0,
            waiting: None,
            shown: Vec::new(),
            term: Budget::new(TERM + 4 * Budget::metadata_bytes()).ok()?,
            fitted: Budget::new(FITTED + 4 * Budget::metadata_bytes()).ok()?,
        })
    }

    /// Whether these are message `email`'s.
    pub fn of_email(&self, email: &str) -> bool {
        self.email == email
    }

    /// The message these are.
    pub fn email(&self) -> &str {
        &self.email
    }

    /// The next part to fetch, now awaited: each blob once, though several
    /// Content-IDs name it; none when all have been.
    pub fn next_ask(&mut self) -> Option<&Wanted> {
        self.waiting = None;
        while self.next < self.asked.len() {
            let at = self.next;
            self.next += 1;
            let blob = self.asked.get(at).map(|part| part.blob_id.as_str());
            let fetched = self.asked.get(..at).is_some_and(|before| {
                before
                    .iter()
                    .any(|part| Some(part.blob_id.as_str()) == blob)
            });
            if !fetched {
                self.waiting = Some(at);
                return self.asked.get(at);
            }
        }
        None
    }

    /// The answer for part `blob_id`, its `body` or none: `None` when it
    /// is not the one awaited, else whether a picture was kept. Kept, it
    /// is shown for every Content-ID naming that blob.
    pub fn answered(&mut self, blob_id: &str, body: Option<&[u8]>) -> Option<bool> {
        let at = self.waiting.filter(|&at| {
            self.asked
                .get(at)
                .is_some_and(|part| part.blob_id == blob_id)
        })?;
        self.waiting = None;
        Some(body.is_some_and(|body| self.keep(at, body)))
    }

    fn keep(&mut self, at: usize, body: &[u8]) -> bool {
        let Some(part) = self.asked.get(at) else {
            return false;
        };
        let format = td_picture::Format::sniff(body)
            .or_else(|| td_picture::Format::from_mime(&part.content_type));
        let Some(format) = format else {
            return false;
        };
        let Ok((picture, natural)) = td_picture::decode_inline(format, body, &self.term, LIMITS)
        else {
            return false;
        };
        let Ok(inline) = BudgetShared::new(&self.term, Inline::new(picture, natural, None)) else {
            return false;
        };
        let mut kept = false;
        for (index, part) in self.asked.iter().enumerate().skip(at) {
            if part.blob_id != blob_of(&self.asked, at) {
                continue;
            }
            let Ok(shared) = inline.try_clone() else {
                break;
            };
            if td_alloc::push(&mut self.shown, (index, shared)).is_err() {
                break;
            }
            kept = true;
        }
        kept
    }

    /// How many have come: a page laid with fewer is laid again.
    pub fn count(&self) -> usize {
        self.shown.len()
    }

    /// The picture an `img`'s `src` names, if it has come.
    pub fn named(&self, src: &str) -> Option<&BudgetShared<Inline>> {
        self.shown.iter().find_map(|(at, inline)| {
            let part = self.asked.get(*at)?;
            names(src, &part.cid).then_some(inline)
        })
    }

    /// The term a layout's fitted copies are charged to.
    pub fn fitted(&self) -> &Budget {
        &self.fitted
    }
}

fn blob_of(asked: &[Wanted], at: usize) -> &str {
    asked.get(at).map_or("", |part| part.blob_id.as_str())
}

/// `part` as a picture one of `urls` names: one with a blob, a
/// Content-ID (without the angle brackets a server may leave on it), a
/// type td-picture decodes and a size within `MAX_BODY`.
fn wanted(part: &BodyPart, urls: &[&str]) -> Option<Wanted> {
    let blob_id = part.blob_id.as_deref()?;
    let cid = part.cid.as_deref()?;
    let cid = cid
        .strip_prefix('<')
        .and_then(|cid| cid.strip_suffix('>'))
        .unwrap_or(cid);
    let content_type = part.r#type.as_deref()?;
    let decoded = DECODED
        .iter()
        .any(|kind| kind.eq_ignore_ascii_case(content_type));
    if cid.is_empty()
        || !decoded
        || part.size.is_some_and(|size| size > MAX_BODY)
        || !urls.iter().any(|url| names(url, cid))
    {
        return None;
    }
    Some(Wanted {
        blob_id: td_alloc::string(blob_id).ok()?,
        cid: td_alloc::string(cid).ok()?,
        content_type: td_alloc::string(content_type).ok()?,
    })
}

/// The distinct `cid:` URLs in `html`, its scheme in any case, each read
/// to the next quote, space or angle bracket, in one pass: at most
/// `MAX_URLS`, none longer than `MAX_URL`. None when td-alloc refuses.
fn cid_urls(html: &str) -> Option<Vec<&str>> {
    let bytes = html.as_bytes();
    let mut urls: Vec<&str> = Vec::new();
    let mut at = 0;
    while let Some(rest) = bytes.get(at..) {
        let Some(found) = rest
            .windows(4)
            .position(|four| four.eq_ignore_ascii_case(b"cid:"))
        else {
            break;
        };
        let start = at + found;
        let tail = bytes.get(start..).unwrap_or_default();
        let len = tail
            .iter()
            .position(|&byte| {
                matches!(byte, b'"' | b'\'' | b'>' | b'<') || byte.is_ascii_whitespace()
            })
            .unwrap_or(tail.len());
        at = start + len;
        let Some(url) = html.get(start..at).filter(|url| url.len() <= MAX_URL) else {
            continue;
        };
        if !urls.contains(&url) {
            td_alloc::push(&mut urls, url).ok()?;
            if urls.len() >= MAX_URLS {
                break;
            }
        }
    }
    Some(urls)
}

/// Whether `url`, a `cid:` URL, names `cid`: its scheme in any case, then
/// its rest percent-decoded byte for byte (RFC 2392).
pub fn names(url: &str, cid: &str) -> bool {
    let Some(rest) = url
        .get(..4)
        .filter(|scheme| scheme.eq_ignore_ascii_case("cid:"))
        .and_then(|_| url.get(4..))
    else {
        return false;
    };
    let mut want = cid.bytes();
    let mut have = rest.bytes();
    loop {
        let got = match have.next() {
            Some(b'%') => {
                let hi = have.next().and_then(hex);
                let lo = have.next().and_then(hex);
                match hi.zip(lo) {
                    Some((hi, lo)) => Some(hi << 4 | lo),
                    None => return false,
                }
            }
            other => other,
        };
        match (got, want.next()) {
            (None, None) => return true,
            (Some(got), Some(wanted)) if got == wanted => {}
            _ => return false,
        }
    }
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
pub(crate) mod tests {
    use super::*;

    /// A PNG `width` by `height`, one colour.
    pub(crate) fn png(width: usize, height: usize) -> Vec<u8> {
        td_png::encode_rgb8(width, height, &[40, 90, 160].repeat(width * height)).unwrap()
    }

    /// A PNG's signature and header alone, `width` by `height`, 8-bit
    /// RGB: enough for a plan, which reads no further.
    fn header_only(width: u32, height: u32) -> Vec<u8> {
        let mut ihdr = b"IHDR".to_vec();
        ihdr.extend_from_slice(&width.to_be_bytes());
        ihdr.extend_from_slice(&height.to_be_bytes());
        ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
        let mut crc = !0u32;
        for byte in &ihdr {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                crc = if crc & 1 == 1 {
                    (crc >> 1) ^ 0xedb8_8320
                } else {
                    crc >> 1
                };
            }
        }
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png.extend_from_slice(&13u32.to_be_bytes());
        png.extend_from_slice(&ihdr);
        png.extend_from_slice(&(!crc).to_be_bytes());
        png
    }

    /// Message `e1`'s pictures asked for: each `(blob, cid)`, a PNG.
    fn asking(parts: &[(&str, &str)]) -> Pictures {
        Pictures {
            email: "e1".to_string(),
            asked: parts
                .iter()
                .map(|(blob, cid)| Wanted {
                    blob_id: blob.to_string(),
                    cid: cid.to_string(),
                    content_type: "image/png".to_string(),
                })
                .collect(),
            next: 0,
            waiting: None,
            shown: Vec::new(),
            term: Budget::new(TERM + 4 * Budget::metadata_bytes()).unwrap(),
            fitted: Budget::new(FITTED + 4 * Budget::metadata_bytes()).unwrap(),
        }
    }

    /// Message `e1`'s pictures with each `(cid, body)` come, decoded.
    pub(crate) fn all_come(parts: &[(&str, &[u8])]) -> Pictures {
        let blobs: Vec<String> = (0..parts.len()).map(|n| format!("b{n}")).collect();
        let asked: Vec<(&str, &str)> = blobs
            .iter()
            .zip(parts)
            .map(|(blob, (cid, _))| (blob.as_str(), *cid))
            .collect();
        let mut pictures = asking(&asked);
        for (blob, (_, body)) in blobs.iter().zip(parts) {
            assert_eq!(pictures.next_ask().unwrap().blob_id, *blob);
            assert_eq!(pictures.answered(blob, Some(body)), Some(true));
        }
        pictures
    }

    /// Message `e1`'s pictures with one come: `cid`'s, `body` decoded.
    pub(crate) fn come(cid: &str, body: &[u8]) -> Pictures {
        let mut pictures = asking(&[("b1", cid)]);
        assert_eq!(pictures.next_ask().unwrap().blob_id, "b1");
        assert_eq!(pictures.answered("b1", Some(body)), Some(true));
        pictures
    }

    fn part(blob: &str, kind: &str, cid: Option<&str>, size: Option<u64>) -> BodyPart {
        BodyPart {
            part_id: blob.to_string(),
            blob_id: Some(blob.to_string()),
            r#type: Some(kind.to_string()),
            name: None,
            size,
            cid: cid.map(str::to_string),
        }
    }

    fn email(parts: Vec<BodyPart>) -> Email {
        use std::collections::HashMap;
        Email {
            id: "e1".to_string(),
            thread_id: None,
            from: None,
            to: None,
            cc: None,
            reply_to: None,
            subject: None,
            received_at: None,
            sent_at: None,
            preview: None,
            text_body: None,
            html_body: None,
            body_values: HashMap::new(),
            keywords: HashMap::new(),
            mailbox_ids: HashMap::new(),
            message_id: None,
            references: None,
            attachments: Some(parts),
            extra: HashMap::new(),
        }
    }

    /// A `cid:` URL names a Content-ID by its scheme in any case and its
    /// rest percent-decoded, byte for byte.
    #[test]
    fn a_cid_url_names_its_content_id() {
        assert!(names("cid:logo@e.example", "logo@e.example"));
        assert!(names("CID:logo%40e.example", "logo@e.example"));
        assert!(names("cid:a%2fb", "a/b"));
        assert!(!names("cid:logo@e.example", "Logo@e.example"));
        assert!(!names("cid:logo", "logo@e.example"));
        assert!(!names("cid:logo@e.example.x", "logo@e.example"));
        assert!(!names("cid:a%2", "a"));
        assert!(!names("mid:logo@e.example", "logo@e.example"));
        assert!(!names("cid", ""));
    }

    /// Only a part with a blob, a Content-ID, a type td-picture decodes
    /// and a size within the bound, named by a `cid:` URL in the HTML, is
    /// asked for, once each Content-ID, at most `MAX_PICTURES`, from the
    /// HTML body's parts and the attachments; a Content-ID left in angle
    /// brackets is matched without them; HTML naming none asks for none.
    #[test]
    fn only_the_pictures_the_html_names_are_asked_for() {
        let html = "<img src='cid:a%40x'><img src=\"CID:big\"><IMG src=cid:text>\
                    <img src=cid:svg><img src=cid:jpg><img src=cid:inline>";
        let mut email = email(vec![
            part("b-a", "image/png", Some("a@x"), Some(100)),
            part("b-a-again", "image/png", Some("a@x"), None),
            part("b-unnamed", "image/png", Some("unnamed"), None),
            part("b-text", "text/plain", Some("text"), None),
            part("b-svg", "image/svg+xml", Some("svg"), None),
            part("b-jpg", "IMAGE/JPG", Some("<jpg>"), None),
            part("b-big", "image/png", Some("big"), Some(MAX_BODY + 1)),
            part("b-none", "image/png", None, None),
        ]);
        email.html_body = Some(vec![part("b-inline", "image/gif", Some("inline"), None)]);
        let pictures = Pictures::of(&email, html).unwrap();
        let asked: Vec<(&str, &str)> = pictures
            .asked
            .iter()
            .map(|p| (p.blob_id.as_str(), p.cid.as_str()))
            .collect();
        assert_eq!(
            asked,
            [("b-inline", "inline"), ("b-a", "a@x"), ("b-jpg", "jpg")]
        );
        assert!(pictures.of_email("e1") && !pictures.of_email("e2"));
        assert!(Pictures::of(&email, "<img src='https://e.example/a'>").is_none());
        let many: Vec<BodyPart> = (0..MAX_PICTURES + 4)
            .map(|n| part(&format!("b{n}"), "image/gif", Some(&format!("p{n}")), None))
            .collect();
        let html: String = (0..MAX_PICTURES + 4)
            .map(|n| format!("<img src=cid:p{n}>"))
            .collect();
        let pictures = Pictures::of(&super::tests::email(many), &html).unwrap();
        assert_eq!(pictures.asked.len(), MAX_PICTURES);
    }

    /// The HTML is read once for its `cid:` URLs, however hostile: a run
    /// of `cid:` with nothing between, which a scan from each occurrence
    /// to its end makes quadratic, and many parts to match, cost one pass
    /// and a bounded match. At most `MAX_URLS` distinct URLs are read, and
    /// none longer than `MAX_URL`.
    #[test]
    fn the_html_is_read_once_for_its_cid_urls() {
        let hostile = "cid:".repeat(128 << 10);
        assert!(cid_urls(&hostile).unwrap().is_empty());
        let parts: Vec<BodyPart> = (0..2 * MAX_PARTS)
            .map(|n| part(&format!("b{n}"), "image/png", Some(&format!("c{n}")), None))
            .collect();
        let email = email(parts);
        let started = std::time::Instant::now();
        assert!(Pictures::of(&email, &hostile).is_none());
        let many = "<img src=cid:c1 >".repeat(32 << 10);
        assert_eq!(Pictures::of(&email, &many).unwrap().asked.len(), 1);
        // Quadratic, these took minutes; read once, well under a second
        // in a debug build.
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        let distinct: String = (0..2 * MAX_URLS).map(|n| format!("cid:u{n} ")).collect();
        let urls = cid_urls(&distinct).unwrap();
        assert_eq!(urls.len(), MAX_URLS);
        assert_eq!(urls.first(), Some(&"cid:u0"));
        let long = format!("cid:{} cid:short", "x".repeat(MAX_URL));
        assert_eq!(cid_urls(&long).unwrap(), ["cid:short"]);
    }

    /// Pictures are asked for one at a time, each blob once: the next on
    /// the answer for the one awaited, whether a picture or not; an
    /// answer for another is not taken. A blob two Content-IDs name is
    /// shown for both.
    #[test]
    fn pictures_are_asked_for_one_at_a_time() {
        let mut pictures = asking(&[("b1", "one"), ("b2", "two"), ("b1", "again")]);
        assert_eq!(pictures.answered("b1", Some(&png(2, 2))), None);
        assert_eq!(pictures.next_ask().unwrap().blob_id, "b1");
        assert_eq!(pictures.answered("b2", Some(&png(2, 2))), None);
        assert_eq!(pictures.answered("b1", Some(&png(3, 5))), Some(true));
        assert_eq!(pictures.answered("b1", Some(&png(3, 5))), None);
        assert_eq!(pictures.count(), 2);
        assert_eq!(
            pictures.named("cid:again").map(|p| p.natural()),
            Some((3, 5))
        );
        assert_eq!(pictures.next_ask().unwrap().blob_id, "b2");
        assert_eq!(pictures.answered("b2", None), Some(false));
        assert!(pictures.next_ask().is_none());
        assert!(pictures.named("cid:two").is_none());
    }

    /// A picture come decodes and is named by its `cid:` URL; a body not
    /// a picture, and one whose decoding alone would pass the term, are
    /// not kept; and the pictures kept fill the term, so one whose
    /// decoding does not fit beside them is not kept either.
    #[test]
    fn a_picture_come_is_kept_within_the_term() {
        let pictures = come("a@x", &png(40, 60));
        assert_eq!(pictures.count(), 1);
        let inline = pictures.named("cid:a%40x").unwrap();
        assert_eq!(inline.natural(), (40, 60));
        assert!(pictures.named("cid:b@x").is_none());
        let mut fresh = asking(&[("b1", "a"), ("b2", "b")]);
        fresh.next_ask();
        assert_eq!(fresh.answered("b1", Some(b"not a picture")), Some(false));
        // A picture whose reckoned peak passes the term is refused before
        // its pixels: 3000 by 3000 is 36 MB of RGBA alone.
        let huge = header_only(3000, 3000);
        assert!(matches!(
            td_picture::plan(td_picture::Format::Png, &huge, LIMITS),
            Err(td_picture::Error::Work(..))
        ));
        fresh.next_ask();
        assert_eq!(fresh.answered("b2", Some(&huge)), Some(false));
        assert_eq!(fresh.count(), 0);
        let big = png(1600, 1200);
        let blobs: Vec<String> = (0..4).map(|n| format!("b{n}")).collect();
        let parts: Vec<(&str, &str)> = blobs.iter().map(|b| (b.as_str(), b.as_str())).collect();
        let mut filling = asking(&parts);
        let kept: Vec<bool> = blobs
            .iter()
            .map(|blob| {
                filling.next_ask();
                filling.answered(blob, Some(&big)).unwrap()
            })
            .collect();
        assert_eq!(kept.first(), Some(&true));
        assert_eq!(kept.last(), Some(&false));
    }
}
