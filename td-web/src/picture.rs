//! One bounded picture path for navigations and future inline pictures.

use td_ui::picture::{Picture, MAX_PICTURE_AXIS, MAX_PICTURE_PIXELS};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Format {
    Png,
    Jpeg,
    Gif,
    Webp,
    Avif,
}

impl Format {
    pub(crate) fn from_mime(mime: &str) -> Option<Self> {
        match mime {
            "image/png" => Some(Self::Png),
            "image/jpeg" | "image/pjpeg" => Some(Self::Jpeg),
            "image/gif" => Some(Self::Gif),
            "image/webp" => Some(Self::Webp),
            "image/avif" => Some(Self::Avif),
            _ => None,
        }
    }

    pub(crate) fn sniff(body: &[u8]) -> Option<Self> {
        if body.starts_with(b"\x89PNG\r\n\x1a\n") {
            Some(Self::Png)
        } else if body.starts_with(&[0xff, 0xd8, 0xff]) {
            Some(Self::Jpeg)
        } else if body.starts_with(b"GIF87a") || body.starts_with(b"GIF89a") {
            Some(Self::Gif)
        } else if body.starts_with(b"RIFF") && body.get(8..12) == Some(b"WEBP") {
            Some(Self::Webp)
        } else if td_avif::is_avif(body) {
            Some(Self::Avif)
        } else {
            None
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Png => "PNG",
            Self::Jpeg => "JPEG",
            Self::Gif => "GIF",
            Self::Webp => "WebP",
            Self::Avif => "AVIF",
        }
    }
}

fn fits(width: usize, height: usize) -> bool {
    width != 0
        && height != 0
        && width <= MAX_PICTURE_AXIS
        && height <= MAX_PICTURE_AXIS
        && width
            .checked_mul(height)
            .is_some_and(|pixels| pixels <= MAX_PICTURE_PIXELS)
}

#[derive(Debug)]
pub(crate) enum Error {
    Resources,
    /// The page root refused the decoder's working charge: room another
    /// page holds may free it.
    Room,
    Input(Format),
    Size(Format, usize, usize),
    Read(Format, Cause),
    Decode(Format, Cause),
    Picture(td_ui::picture::Error),
}
#[derive(Debug)]
pub(crate) enum Cause {
    Png(td_png::Error),
    Jpeg(td_jpeg::Error),
    Gif(td_gif::Error),
    Webp(td_webp::Error),
    Avif(td_avif::Error),
}
impl Cause {
    fn resources(&self) -> bool {
        matches!(
            self,
            Self::Png(
                td_png::Error::Allocation
                    | td_png::Error::Inflate(td_png::InflateError::Allocation)
            ) | Self::Jpeg(td_jpeg::Error::Allocation)
                | Self::Gif(td_gif::Error::Limit)
                | Self::Webp(td_webp::Error::Limit)
                | Self::Avif(td_avif::Error::Limit)
        )
    }
}
impl std::fmt::Display for Cause {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Png(e) => e.fmt(f),
            Self::Jpeg(e) => e.fmt(f),
            Self::Gif(e) => e.fmt(f),
            Self::Webp(e) => e.fmt(f),
            Self::Avif(e) => e.fmt(f),
        }
    }
}
impl Error {
    pub(crate) fn resources(&self) -> bool {
        match self {
            Self::Resources | Self::Room => true,
            Self::Read(_, c) | Self::Decode(_, c) => c.resources(),
            _ => false,
        }
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Resources | Self::Room => {
                f.write_str("There is not the memory to show this picture.")
            }
            Self::Input(format) => write!(f, "The {} is too large to read.", format.name()),
            Self::Size(format, w, h) => write!(
                f,
                "The {} is {w} by {h} pixels, too large to show.",
                format.name()
            ),
            Self::Read(format, cause) => {
                write!(f, "The {} could not be read: {cause}", format.name())
            }
            Self::Decode(format, cause) => {
                write!(f, "The {} could not be decoded: {cause}", format.name())
            }
            Self::Picture(cause) => write!(f, "The picture cannot be shown: {cause}"),
        }
    }
}

#[cfg(test)]
pub(crate) fn decode(format: Format, body: &[u8]) -> Result<Picture, Error> {
    decoded(format, body).map(|(picture, _)| picture)
}

pub(crate) fn decode_with_budget(
    format: Format,
    body: &[u8],
    memory: &td_alloc::Budget,
) -> Result<crate::memory::Resident<Picture>, Error> {
    let charge = memory
        .charge(crate::memory::PICTURE_WORK)
        .map_err(|_| Error::Room)?;
    let (picture, capacity) = decoded(format, body)?;
    resident(picture, charge, capacity)
}

/// A decoded picture kept at its capacity within `charge`: refused, it
/// is out of resources, not room another page could give.
fn resident(
    picture: Picture,
    charge: td_alloc::Charge,
    capacity: usize,
) -> Result<crate::memory::Resident<Picture>, Error> {
    crate::memory::Resident::new(picture, charge, capacity).map_err(|_| Error::Resources)
}

/// What a picture is decoded as, read from its header before any of its
/// pixels: its size as stored, and the scale it is decoded at, the
/// largest of td-jpeg's that fits td-ui's bounds (a JPEG's), else whole.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Plan {
    format: Format,
    width: usize,
    height: usize,
    scale: td_jpeg::Scale,
    /// What a format's working storage is reckoned from (`work`).
    shape: Shape,
}

/// What a format's working storage is reckoned from, beside its size.
#[derive(Clone, Copy, Debug)]
enum Shape {
    /// A PNG's bits a pixel, and whether it is interlaced.
    Png {
        bits: usize,
        interlaced: bool,
    },
    /// A JPEG's components, and whether it is progressive.
    Jpeg {
        components: usize,
        progressive: bool,
    },
    Other,
}

/// `body` read as `format` up to its pixels: refused past
/// `MAX_DOCUMENT`, for a header that does not read, or for a size td-ui
/// cannot hold at any scale.
pub(crate) fn plan(format: Format, body: &[u8]) -> Result<Plan, Error> {
    if body.len() as u64 > crate::fetch::MAX_DOCUMENT {
        return Err(Error::Input(format));
    }
    let read_error = |why| Error::Read(format, why);
    let (width, height, shape) = match format {
        Format::Png => {
            let h = td_png::header(body).map_err(|e| read_error(Cause::Png(e)))?;
            let channels = match h.colour {
                td_png::Colour::Grey | td_png::Colour::Palette => 1,
                td_png::Colour::GreyAlpha => 2,
                td_png::Colour::Rgb => 3,
                td_png::Colour::Rgba => 4,
            };
            let shape = Shape::Png {
                bits: channels * usize::from(h.depth),
                interlaced: h.interlaced,
            };
            (h.width, h.height, shape)
        }
        Format::Jpeg => {
            let h = td_jpeg::header(body).map_err(|e| read_error(Cause::Jpeg(e)))?;
            let shape = Shape::Jpeg {
                components: usize::from(h.components),
                progressive: h.progressive,
            };
            (h.width, h.height, shape)
        }
        Format::Webp => td_webp::header(body)
            .map(|h| (h.width, h.height, Shape::Other))
            .map_err(|e| read_error(Cause::Webp(e)))?,
        Format::Avif => td_avif::header(body)
            .map(|h| (h.width, h.height, Shape::Other))
            .map_err(|e| read_error(Cause::Avif(e)))?,
        Format::Gif => td_gif::header(body)
            .map(|h| (h.width, h.height, Shape::Other))
            .map_err(|e| read_error(Cause::Gif(e)))?,
    };
    let scale = if format == Format::Jpeg {
        [
            td_jpeg::Scale::Full,
            td_jpeg::Scale::Half,
            td_jpeg::Scale::Quarter,
            td_jpeg::Scale::Eighth,
        ]
        .into_iter()
        .find(|&scale| {
            fits(
                td_jpeg::scaled(width, scale),
                td_jpeg::scaled(height, scale),
            )
        })
    } else {
        fits(width, height).then_some(td_jpeg::Scale::Full)
    }
    .ok_or(Error::Size(format, width, height))?;
    Ok(Plan {
        format,
        width,
        height,
        scale,
        shape,
    })
}

const MIB: usize = 1 << 20;

/// What a JPEG decoder keeps whatever its size: nine Huffman tables of
/// 128 KiB, one more than a frame's eight so a stream redefining one
/// mid-scan stays within it, and 4 KiB of the rest.
const JPEG_TABLES: usize = 9 * (128 << 10) + (4 << 10);

/// Bytes the AV1 codec keeps a padded pixel while it decodes a frame.
const AVIF_FRAME: usize = 8;

/// `n` rounded up to a multiple of `step`, saturating.
fn up(n: usize, step: usize) -> usize {
    n.div_ceil(step).saturating_mul(step)
}

/// What decoding `plan` from a body of `body` bytes holds at its peak, at
/// most `PICTURE_WORK`: reckoned up from each decoder's stated layout
/// (DESIGN.md §10, "Memory"), the pixels it keeps, the scratch beside
/// them and the tables a decoder keeps whatever the size. An inline
/// picture's decoding is charged this to the pictures term before it
/// begins.
pub(crate) fn work(plan: &Plan, body: usize) -> usize {
    let (width, height) = (
        td_jpeg::scaled(plan.width, plan.scale),
        td_jpeg::scaled(plan.height, plan.scale),
    );
    let pixels = width.saturating_mul(height);
    let rgba = pixels.saturating_mul(4);
    // Each decoder's phases in turn, what one holds let go before the
    // next grows: its peak is the largest phase's, not their sum.
    let estimate = match plan.shape {
        // The compressed stream grown by doubling (three times the body
        // while it is replaced); that capacity beside the inflated rows,
        // reserved once and rounded up to a power of two up to 16 MiB;
        // then those rows, RGBA and two rows of scratch.
        Shape::Png { bits, interlaced } => {
            let row = 1 + plan.width.saturating_mul(bits).div_ceil(8);
            let mut inflated = plan.height.saturating_mul(row);
            if interlaced {
                // Each pass's rows start with a filter byte and round up.
                inflated = inflated.saturating_add(plan.height.saturating_mul(4) + 14);
            }
            if inflated <= 16 * MIB {
                inflated = inflated.next_power_of_two();
            }
            let gathered = body.saturating_mul(3);
            let inflating = body.saturating_mul(2).saturating_add(inflated);
            let expanding = inflated
                .saturating_add(rgba)
                .saturating_add(row.saturating_mul(2));
            gathered.max(inflating).max(expanding).saturating_add(MIB)
        }
        // A progressive frame's coefficients at the whole size, two bytes
        // each, every component at most whole and its blocks padded to
        // the largest MCU, beside the sample planes at the scale; the
        // planes beside RGB; then RGB beside td-web's RGBA. A baseline
        // frame holds no coefficients.
        Shape::Jpeg {
            components,
            progressive,
        } => {
            let planes = up(width, 32)
                .saturating_mul(up(height, 32))
                .saturating_mul(components);
            let coefficients = if progressive {
                up(plan.width, 32)
                    .saturating_mul(up(plan.height, 32))
                    .saturating_mul(components)
                    .min(td_jpeg::MAX_PROGRESSIVE_COEFFICIENTS)
                    .saturating_mul(2)
            } else {
                0
            };
            let rgb = pixels.saturating_mul(3);
            let scanning = coefficients.saturating_add(planes);
            let rendering = planes.saturating_add(rgb);
            let converting = rgb.saturating_add(rgba);
            scanning
                .max(rendering)
                .max(converting)
                .saturating_add(JPEG_TABLES)
        }
        Shape::Other => match plan.format {
            // Only the canvas, and the code tables.
            Format::Gif => rgba.saturating_add(MIB),
            // Two whole images in 32-bit pixels, or a lossy frame's
            // planes in macroblocks beside RGBA; the quarter-size maps
            // and the prefix tables.
            Format::Webp => {
                let blocks = up(width, 16).saturating_mul(up(height, 16));
                pixels
                    .saturating_mul(8)
                    .max(blocks.saturating_mul(2).saturating_add(rgba))
                    .saturating_add(pixels.saturating_mul(2))
                    .saturating_add(3 * MIB)
            }
            // The coded copy beside the codec's frames, in 64-pixel
            // superblocks; then its planes beside RGBA.
            _ => {
                let blocks = up(width, 64).saturating_mul(up(height, 64));
                let frames = blocks.saturating_mul(AVIF_FRAME);
                body.saturating_add(frames)
                    .max(blocks.saturating_mul(3).saturating_add(rgba))
                    .saturating_add(4 * MIB)
            }
        },
    };
    estimate.min(crate::memory::PICTURE_WORK)
}

fn decoded(format: Format, body: &[u8]) -> Result<(Picture, usize), Error> {
    decode_plan(plan(format, body)?, body)
}

/// An inline picture (DESIGN.md §10, "Subresources"): `body` decoded as
/// `format` with its working peak (`work`) charged to `term` first, then
/// made its own fit in place, premultiplied in the raster's order, and
/// kept at its capacity; refused for room when the term has not the
/// peak.
pub(crate) fn decode_inline(
    format: Format,
    body: &[u8],
    term: &td_alloc::Budget,
) -> Result<
    (
        crate::memory::Resident<td_ui::picture::Fitted>,
        (usize, usize),
    ),
    Error,
> {
    let plan = plan(format, body)?;
    let natural = (plan.width, plan.height);
    let charge = term
        .charge(work(&plan, body.len()))
        .map_err(|_| Error::Room)?;
    let (picture, capacity) = decode_plan(plan, body)?;
    let kept = crate::memory::Resident::new(picture.into_fitted(), charge, capacity)
        .map_err(|_| Error::Resources)?;
    Ok((kept, natural))
}

fn decode_plan(plan: Plan, body: &[u8]) -> Result<(Picture, usize), Error> {
    let Plan { format, scale, .. } = plan;
    let decode_error = |why| Error::Decode(format, why);
    let (width, height, rgba) = match format {
        Format::Png => {
            let image = td_png::decode(body).map_err(|e| decode_error(Cause::Png(e)))?;
            (image.width, image.height, image.data)
        }
        Format::Webp => {
            let image = td_webp::decode(body).map_err(|e| decode_error(Cause::Webp(e)))?;
            (image.width, image.height, image.data)
        }
        Format::Avif => {
            let image = td_avif::decode(body).map_err(|e| decode_error(Cause::Avif(e)))?;
            (image.width, image.height, image.data)
        }
        Format::Gif => {
            let image = td_gif::decode(body).map_err(|e| decode_error(Cause::Gif(e)))?;
            (image.width, image.height, image.data)
        }
        Format::Jpeg => {
            let image = td_jpeg::decode(body, scale).map_err(|e| decode_error(Cause::Jpeg(e)))?;
            let mut rgba = Vec::new();
            td_alloc::reserve_exact(&mut rgba, image.data.len() / 3 * 4)
                .map_err(|_| Error::Resources)?;
            for pixel in image.data.as_chunks::<3>().0 {
                rgba.extend_from_slice(pixel);
                rgba.push(255);
            }
            (image.width, image.height, rgba)
        }
    };
    let capacity = rgba.capacity();
    Picture::new(width, height, rgba)
        .map(|picture| (picture, capacity))
        .map_err(Error::Picture)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
pub(crate) mod tests {
    use super::*;

    pub(crate) const AVIF: &[u8] =
        include_bytes!("../../td-avif/tests/fixtures/libaom-reduced.avif");

    pub(crate) const WEBP: &[u8] = include_bytes!("../../td-webp/tests/fixtures/palette.webp");

    /// A grey baseline JPEG 16384 pixels wide and 16 tall, every block
    /// flat: one-symbol Huffman tables, each block two zero bits.
    pub(crate) fn wide_jpeg() -> Vec<u8> {
        let mut out = vec![0xff, 0xd8];
        out.extend_from_slice(&[0xff, 0xdb, 0, 67, 0]);
        out.extend_from_slice(&[1; 64]);
        out.extend_from_slice(&[0xff, 0xc0, 0, 11, 8, 0, 16, 0x40, 0, 1, 1, 0x11, 0]);
        for class in [0x00, 0x10] {
            out.extend_from_slice(&[0xff, 0xc4, 0, 20, class, 1]);
            out.extend_from_slice(&[0; 15]);
            out.push(0);
        }
        out.extend_from_slice(&[0xff, 0xda, 0, 8, 1, 1, 0, 0, 63, 0]);
        out.extend_from_slice(&[0; 1024]);
        out.extend_from_slice(&[0xff, 0xd9]);
        out
    }

    /// A JPEG too wide for td-ui at its own size is decoded at a reduced
    /// scale and keeps its frame's size as its intrinsic size.
    #[test]
    fn a_reduced_jpeg_keeps_its_frames_size() {
        let body = wide_jpeg();
        let term = td_alloc::Budget::new(64 << 20).unwrap();
        let (picture, natural) = decode_inline(Format::Jpeg, &body, &term).unwrap();
        assert_eq!((picture.width, picture.height), (8192, 8));
        assert_eq!(natural, (16384, 16));
    }

    pub(crate) const GIF: &[u8] = b"GIF89a\x02\0\x01\0\x80\0\0\0\0\0\xff\0\0\x21\xf9\x04\x01\0\0\0\0\x2c\0\0\0\0\x02\0\x01\0\0\x02\x02\x44\x0a\0\x3b";

    /// Only the root's refusal of the working charge is a want of room;
    /// a picture that cannot then be kept is out of resources.
    #[test]
    fn a_picture_not_kept_within_its_charge_is_out_of_resources() {
        let root = td_alloc::Budget::new(4096).unwrap();
        let (picture, _) = decoded(Format::Gif, GIF).unwrap();
        let charge = root.charge(16).unwrap();
        let _full = root.charge(root.limit() - root.used()).unwrap();
        assert!(matches!(
            resident(picture, charge, 1024),
            Err(Error::Resources)
        ));
        assert!(matches!(
            decode_with_budget(Format::Gif, GIF, &root),
            Err(Error::Room)
        ));
    }

    #[test]
    fn decoded_pixels_keep_credit_and_all_refusals_return_scratch() {
        let root = crate::memory::root().unwrap();
        let base = root.used();
        for (format, bytes) in [
            (
                Format::Png,
                include_bytes!("../../td-png/tests/fixtures/suite/rgb8.png").as_slice(),
            ),
            (
                Format::Jpeg,
                include_bytes!("../../td-jpeg/tests/fixtures/progressive/420-prog.jpg").as_slice(),
            ),
            (Format::Gif, GIF),
            (Format::Webp, WEBP),
            (Format::Avif, AVIF),
        ] {
            let mut succeeded = false;
            for after in 0..1024 {
                let result = td_alloc::with_reservation_limit(after, || {
                    decode_with_budget(format, bytes, &root)
                });
                match result {
                    Ok(image) => {
                        assert!(root.used() >= base + image.bytes());
                        assert!(root.used() < base + crate::memory::PICTURE_WORK);
                        let blocked = root.charge(root.limit() - root.used()).unwrap();
                        assert!(decode_with_budget(format, bytes, &root)
                            .unwrap_err()
                            .resources());
                        drop(blocked);
                        assert!(image.fit(11, 9).is_some());
                        drop(image);
                        succeeded = true;
                    }
                    Err(error) => assert!(error.resources()),
                }
                assert_eq!(root.used(), base, "{format:?}, reservation {after}");
                if succeeded {
                    break;
                }
            }
            assert!(succeeded);
        }
    }

    #[test]
    fn every_picture_reservation_can_fail_without_losing_later_decodes() {
        let png = include_bytes!("../../td-png/tests/fixtures/suite/rgb8.png");
        let jpeg = include_bytes!("../../td-jpeg/tests/fixtures/progressive/420-prog.jpg");
        for (format, bytes) in [
            (Format::Png, png.as_slice()),
            (Format::Jpeg, jpeg.as_slice()),
            (Format::Gif, GIF),
            (Format::Webp, WEBP),
            (Format::Avif, AVIF),
        ] {
            let expected = decode(format, bytes).unwrap();
            let mut succeeded = false;
            for after in 0..1024 {
                match td_alloc::with_reservation_limit(after, || decode(format, bytes)) {
                    Err(error) => assert!(
                        error.resources(),
                        "{format:?}, reservation {after}: {error}"
                    ),
                    Ok(actual) => {
                        assert!(after > 0);
                        assert_eq!(actual.fit(11, 9), expected.fit(11, 9));
                        succeeded = true;
                        break;
                    }
                }
            }
            assert!(succeeded, "{format:?}");
            assert_eq!(
                decode(format, bytes).unwrap().fit(11, 9),
                expected.fit(11, 9)
            );
        }
    }

    #[test]
    fn gif_dimensions_and_encoded_bytes_are_checked_before_decode() {
        let mut bytes = GIF[..13].to_vec();
        bytes[6..8].copy_from_slice(&((MAX_PICTURE_AXIS + 1) as u16).to_le_bytes());
        assert!(decode(Format::Gif, &bytes)
            .unwrap_err()
            .to_string()
            .contains("too large to show"));
        let bytes = vec![0; crate::fetch::MAX_DOCUMENT as usize + 1];
        for format in [
            Format::Png,
            Format::Jpeg,
            Format::Gif,
            Format::Webp,
            Format::Avif,
        ] {
            assert!(decode(format, &bytes)
                .unwrap_err()
                .to_string()
                .contains("too large to read"));
        }
        let mut truncated = GIF.to_vec();
        truncated.truncate(truncated.len() - 2);
        assert!(decode(Format::Gif, &truncated)
            .unwrap_err()
            .to_string()
            .contains("could not be decoded"));
    }

    #[test]
    fn progressive_jpeg_uses_the_existing_rgb_conversion() {
        let baseline = include_bytes!("../../td-jpeg/tests/fixtures/progressive/420-base.jpg");
        let progressive = include_bytes!("../../td-jpeg/tests/fixtures/progressive/420-prog.jpg");
        let expected = decode(Format::Jpeg, baseline).unwrap();
        let actual = decode(Format::Jpeg, progressive).unwrap();
        assert_eq!(actual.fit(35, 27), expected.fit(35, 27));
    }

    /// Each format's working peak is reckoned as its largest phase, not
    /// their sum: pinned to the figure each decoder's stated layout
    /// gives, so that any term of it changed shows.
    #[test]
    fn each_estimate_is_its_decoders_largest_phase() {
        let estimate = |format, body: &[u8]| work(&plan(format, body).unwrap(), body.len());
        let up = |n: usize, step: usize| n.div_ceil(step) * step;
        // A baseline frame holds no coefficients: its planes beside RGB,
        // or RGB beside RGBA.
        for (name, body, progressive) in [
            (
                "base",
                include_bytes!("../../td-jpeg/tests/fixtures/progressive/420-base.jpg").as_slice(),
                false,
            ),
            (
                "prog",
                include_bytes!("../../td-jpeg/tests/fixtures/progressive/420-prog.jpg").as_slice(),
                true,
            ),
        ] {
            let h = td_jpeg::header(body).unwrap();
            assert_eq!(h.progressive, progressive, "{name}");
            let pixels = h.width * h.height;
            let planes = up(h.width, 32) * up(h.height, 32) * 3;
            let coefficients = if progressive { planes * 2 } else { 0 };
            let peak = (coefficients + planes)
                .max(planes + pixels * 3)
                .max(pixels * 7);
            assert_eq!(
                estimate(Format::Jpeg, body),
                peak + 9 * 128 * 1024 + 4096,
                "{name}"
            );
        }
        // A PNG: the stream gathered; its capacity beside the inflated
        // rows; those rows beside RGBA and two rows.
        for (width, height) in [(7, 5), (300, 200)] {
            let body =
                td_png::encode_rgb8(width, height, &[9, 8, 7].repeat(width * height)).unwrap();
            let row = 1 + width * 3;
            let inflated = (height * row).next_power_of_two();
            let peak = (body.len() * 3)
                .max(body.len() * 2 + inflated)
                .max(inflated + width * height * 4 + row * 2);
            assert_eq!(estimate(Format::Png, &body), peak + MIB, "{width}x{height}");
        }
        // A body far longer than its rows, past its end: three times it,
        // as its stream is grown.
        let mut body = td_png::encode_rgb8(7, 5, &[9, 8, 7].repeat(35)).unwrap();
        body.resize(body.len() + 100_000, 0);
        assert_eq!(estimate(Format::Png, &body), 3 * body.len() + MIB);
        let gif = td_gif::header(GIF).unwrap();
        assert_eq!(estimate(Format::Gif, GIF), gif.width * gif.height * 4 + MIB);
        let webp = td_webp::header(WEBP).unwrap();
        let (pixels, blocks) = (
            webp.width * webp.height,
            up(webp.width, 16) * up(webp.height, 16),
        );
        assert_eq!(
            estimate(Format::Webp, WEBP),
            (pixels * 8).max(blocks * 2 + pixels * 4) + pixels * 2 + 3 * MIB
        );
        let avif = td_avif::header(AVIF).unwrap();
        let (pixels, blocks) = (
            avif.width * avif.height,
            up(avif.width, 64) * up(avif.height, 64),
        );
        assert_eq!(
            estimate(Format::Avif, AVIF),
            (AVIF.len() + blocks * 8).max(blocks * 3 + pixels * 4) + 4 * MIB
        );
    }
}
