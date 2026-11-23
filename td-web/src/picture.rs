//! td-web's pictures, navigations and inline alike, through td-picture at
//! td-web's own limits: a body no larger than a document's, and at most
//! `PICTURE_WORK` held by one decoding.

#[cfg(test)]
use td_picture::Plan;
pub(crate) use td_picture::{Error, Format};

/// What td-web admits as a picture (DESIGN.md §10, "Memory").
const LIMITS: td_picture::Limits = td_picture::Limits {
    body: crate::fetch::MAX_DOCUMENT,
    work: crate::memory::PICTURE_WORK,
};

#[cfg(test)]
pub(crate) fn plan(format: Format, body: &[u8]) -> Result<Plan, Error> {
    td_picture::plan(format, body, LIMITS)
}

#[cfg(test)]
pub(crate) fn work(plan: &Plan, body: usize) -> usize {
    td_picture::work(plan, body)
}

pub(crate) fn decode_with_budget(
    format: Format,
    body: &[u8],
    memory: &td_alloc::Budget,
) -> Result<td_page::memory::Resident<td_ui::picture::Picture>, Error> {
    td_picture::decode_with_budget(format, body, memory, LIMITS)
}

pub(crate) fn decode_inline(
    format: Format,
    body: &[u8],
    term: &td_alloc::Budget,
) -> Result<
    (
        td_page::memory::Resident<td_ui::picture::Fitted>,
        (usize, usize),
    ),
    Error,
> {
    td_picture::decode_inline(format, body, term, LIMITS)
}

/// The pictures td-web's other tests read.
#[cfg(test)]
pub(crate) mod tests {
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

    pub(crate) const GIF: &[u8] = b"GIF89a\x02\0\x01\0\x80\0\0\0\0\0\xff\0\0\x21\xf9\x04\x01\0\0\0\0\x2c\0\0\0\0\x02\0\x01\0\0\x02\x02\x44\x0a\0\x3b";

    /// The costliest picture td-web admits, a 16-bit RGBA interlaced PNG
    /// at td-ui's pixel bound in a body of a whole document, is reckoned
    /// within `PICTURE_WORK`: td-picture refuses a picture whose reckoned
    /// peak passes its caller's cap, and td-web's covers every picture it
    /// admits, so it refuses none that way.
    #[test]
    #[allow(clippy::unwrap_used)]
    fn td_webs_costliest_picture_is_admitted() {
        let mut ihdr = b"IHDR".to_vec();
        ihdr.extend_from_slice(&8192u32.to_be_bytes());
        ihdr.extend_from_slice(&2048u32.to_be_bytes());
        ihdr.extend_from_slice(&[16, 6, 0, 0, 1]);
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png.extend_from_slice(&13u32.to_be_bytes());
        png.extend_from_slice(&ihdr);
        png.extend_from_slice(&crc32(&ihdr).to_be_bytes());
        png.resize(crate::fetch::MAX_DOCUMENT as usize, 0);
        assert_eq!(8192 * 2048, td_ui::picture::MAX_PICTURE_PIXELS);
        let plan = super::plan(super::Format::Png, &png).unwrap();
        let peak = super::work(&plan, png.len());
        assert!(peak > 190 << 20 && peak <= crate::memory::PICTURE_WORK);
    }

    /// The CRC-32 a PNG chunk closes with.
    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = !0u32;
        for byte in bytes {
            crc ^= u32::from(*byte);
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

    /// td-web's limits reach the decoder: a body past a document's is
    /// refused, and a PNG's estimate, which grows with its body, is held
    /// to `PICTURE_WORK`.
    #[test]
    #[allow(clippy::unwrap_used)]
    fn td_webs_limits_bound_its_pictures() {
        let mut past = GIF.to_vec();
        past.resize(crate::fetch::MAX_DOCUMENT as usize + 1, 0);
        assert!(matches!(
            super::plan(super::Format::Gif, &past),
            Err(super::Error::Input(super::Format::Gif))
        ));
        past.truncate(crate::fetch::MAX_DOCUMENT as usize);
        assert!(super::plan(super::Format::Gif, &past).is_ok());
        let png = td_png::encode_rgb8(1, 1, &[0, 0, 0]).unwrap();
        let plan = super::plan(super::Format::Png, &png).unwrap();
        assert_eq!(super::work(&plan, usize::MAX), crate::memory::PICTURE_WORK);
    }
}
