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
            Self::Resources => true,
            Self::Read(_, c) | Self::Decode(_, c) => c.resources(),
            _ => false,
        }
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Resources => f.write_str("There is not the memory to show this picture."),
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

pub(crate) fn decode(format: Format, body: &[u8]) -> Result<Picture, Error> {
    if body.len() as u64 > crate::fetch::MAX_DOCUMENT {
        return Err(Error::Input(format));
    }
    let read_error = |why| Error::Read(format, why);
    let (width, height) = match format {
        Format::Png => td_png::header(body)
            .map(|h| (h.width, h.height))
            .map_err(|e| read_error(Cause::Png(e)))?,
        Format::Jpeg => td_jpeg::header(body)
            .map(|h| (h.width, h.height))
            .map_err(|e| read_error(Cause::Jpeg(e)))?,
        Format::Webp => td_webp::header(body)
            .map(|h| (h.width, h.height))
            .map_err(|e| read_error(Cause::Webp(e)))?,
        Format::Avif => td_avif::header(body)
            .map(|h| (h.width, h.height))
            .map_err(|e| read_error(Cause::Avif(e)))?,
        Format::Gif => td_gif::header(body)
            .map(|h| (h.width, h.height))
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
    Picture::new(width, height, rgba).map_err(Error::Picture)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
pub(crate) mod tests {
    use super::*;

    pub(crate) const AVIF: &[u8] =
        include_bytes!("../../td-avif/tests/fixtures/libaom-reduced.avif");

    pub(crate) const WEBP: &[u8] = include_bytes!("../../td-webp/tests/fixtures/palette.webp");

    pub(crate) const GIF: &[u8] = b"GIF89a\x02\0\x01\0\x80\0\0\0\0\0\xff\0\0\x21\xf9\x04\x01\0\0\0\0\x2c\0\0\0\0\x02\0\x01\0\0\x02\x02\x44\x0a\0\x3b";

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
}
