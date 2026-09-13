//! Bounded classic-TIFF container reader over a byte slice: the header, IFDs
//! indexed in place, and typed value access with every extent checked
//! against the slice before it is read. A reader may sit at a `base` inside
//! the slice, which is how a Nikon maker note's own TIFF header is read
//! with offsets relative to itself. Nothing here reads a file, the
//! environment or a clock.

use std::fmt;

/// The largest slice a reader accepts.
pub const MAX_FILE_BYTES: usize = 512 << 20;
/// The most IFDs one walk visits, across every chain and sub-IFD.
pub const MAX_IFDS: usize = 64;
/// The longest next-pointer chain followed from one IFD.
pub const MAX_CHAIN: usize = 16;
/// The most entries one IFD may declare.
pub const MAX_ENTRIES: usize = 4096;

/// What the reader refuses; each names the item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    FileTooLarge { len: usize },
    NotTiff,
    OutOfFile { offset: usize, len: usize },
    TooManyEntries { offset: usize, count: usize },
    TooManyIfds,
    Cycle { offset: usize },
    Kind { tag: u16, kind: u16 },
    Index { tag: u16, index: usize },
    NotAscii { tag: u16 },
    Overflow,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FileTooLarge { len } => write!(f, "file of {len} bytes exceeds the ceiling"),
            Self::NotTiff => f.write_str("not a TIFF container"),
            Self::OutOfFile { offset, len } => {
                write!(f, "{len} bytes at offset {offset} lie outside the file")
            }
            Self::TooManyEntries { offset, count } => {
                write!(f, "IFD at {offset} declares {count} entries")
            }
            Self::TooManyIfds => f.write_str("too many IFDs"),
            Self::Cycle { offset } => write!(f, "IFD at {offset} visited twice"),
            Self::Kind { tag, kind } => write!(f, "tag {tag:#06x} has kind {kind}"),
            Self::Index { tag, index } => write!(f, "tag {tag:#06x} has no value {index}"),
            Self::NotAscii { tag } => write!(f, "tag {tag:#06x} is not ASCII text"),
            Self::Overflow => f.write_str("offset arithmetic overflowed"),
        }
    }
}

impl std::error::Error for Error {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Endian {
    Little,
    Big,
}

/// The TIFF field kinds the accessors read.
pub mod kind {
    pub const BYTE: u16 = 1;
    pub const ASCII: u16 = 2;
    pub const SHORT: u16 = 3;
    pub const LONG: u16 = 4;
    pub const RATIONAL: u16 = 5;
    pub const UNDEFINED: u16 = 7;
    pub const IFD: u16 = 13;
}

/// The tags this crate reads, by their TIFF, Exif and Nikon names.
pub mod tag {
    pub const NEW_SUBFILE_TYPE: u16 = 254;
    pub const IMAGE_WIDTH: u16 = 256;
    pub const IMAGE_LENGTH: u16 = 257;
    pub const BITS_PER_SAMPLE: u16 = 258;
    pub const COMPRESSION: u16 = 259;
    pub const PHOTOMETRIC: u16 = 262;
    pub const MAKE: u16 = 271;
    pub const MODEL: u16 = 272;
    pub const STRIP_OFFSETS: u16 = 273;
    pub const ORIENTATION: u16 = 274;
    pub const SAMPLES_PER_PIXEL: u16 = 277;
    pub const ROWS_PER_STRIP: u16 = 278;
    pub const STRIP_BYTE_COUNTS: u16 = 279;
    pub const SUB_IFDS: u16 = 330;
    pub const JPEG_OFFSET: u16 = 513;
    pub const JPEG_LENGTH: u16 = 514;
    pub const CFA_REPEAT_DIM: u16 = 33421;
    pub const CFA_PATTERN: u16 = 33422;
    pub const EXPOSURE_TIME: u16 = 33434;
    pub const F_NUMBER: u16 = 33437;
    pub const EXIF_IFD: u16 = 34665;
    pub const ISO: u16 = 34855;
    pub const DATE_TIME_ORIGINAL: u16 = 36867;
    pub const FOCAL_LENGTH: u16 = 37386;
    pub const MAKER_NOTE: u16 = 37500;
    pub const LENS_MODEL: u16 = 42036;
    pub const NIKON_WB_RB_LEVELS: u16 = 0x000c;
    pub const NIKON_BLACK_LEVEL: u16 = 0x003d;
    pub const NIKON_CROP_AREA: u16 = 0x0045;
    pub const NIKON_LINEARIZATION: u16 = 0x0096;
}

/// Bytes per element of a field kind; unknown kinds count one, as dcraw
/// does, so a junk entry is skippable rather than fatal.
pub fn kind_size(kind: u16) -> usize {
    match kind {
        1 | 2 | 6 | 7 => 1,
        3 | 8 => 2,
        4 | 9 | 11 | 13 => 4,
        5 | 10 | 12 => 8,
        _ => 1,
    }
}

/// One IFD entry, indexed in place: `offset` is the absolute position of
/// the value bytes (inline in the entry when they fit four bytes) and
/// `len` their extent, checked against the slice only when read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    pub tag: u16,
    pub kind: u16,
    pub count: u32,
    pub offset: usize,
    pub len: usize,
}

/// One IFD: its absolute offset, its entries in file order and the next
/// pointer as written (relative to the reader's base).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ifd {
    pub offset: usize,
    pub entries: Vec<Entry>,
    pub next: u32,
}

impl Ifd {
    /// The first entry with `tag`.
    pub fn find(&self, tag: u16) -> Option<&Entry> {
        self.entries.iter().find(|e| e.tag == tag)
    }
}

/// The IFD offsets one walk has visited, so a cycle ends the walk and
/// `MAX_IFDS` bounds it.
#[derive(Debug, Default)]
pub struct Visited {
    offsets: Vec<usize>,
}

impl Visited {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn enter(&mut self, offset: usize) -> Result<(), Error> {
        if self.offsets.contains(&offset) {
            return Err(Error::Cycle { offset });
        }
        if self.offsets.len() >= MAX_IFDS {
            return Err(Error::TooManyIfds);
        }
        self.offsets.push(offset);
        Ok(())
    }
    pub fn len(&self) -> usize {
        self.offsets.len()
    }
    pub fn is_empty(&self) -> bool {
        self.offsets.is_empty()
    }
}

/// A reader over one slice with one byte order and one base. Copying it is
/// free; it owns nothing.
#[derive(Clone, Copy, Debug)]
pub struct Reader<'a> {
    data: &'a [u8],
    endian: Endian,
    base: usize,
}

impl<'a> Reader<'a> {
    /// Parses the header at `base` and returns the reader with the first
    /// IFD's offset (relative to `base`).
    pub fn new(data: &'a [u8], base: usize) -> Result<(Self, u32), Error> {
        if data.len() > MAX_FILE_BYTES {
            return Err(Error::FileTooLarge { len: data.len() });
        }
        let end = base.checked_add(8).ok_or(Error::Overflow)?;
        let head = data.get(base..end).ok_or(Error::OutOfFile {
            offset: base,
            len: 8,
        })?;
        let endian = match head.get(0..2) {
            Some(b"II") => Endian::Little,
            Some(b"MM") => Endian::Big,
            _ => return Err(Error::NotTiff),
        };
        let reader = Self { data, endian, base };
        if reader.u16_at(base.wrapping_add(2))? != 42 {
            return Err(Error::NotTiff);
        }
        let first = reader.u32_at(base.wrapping_add(4))?;
        Ok((reader, first))
    }

    pub fn data(&self) -> &'a [u8] {
        self.data
    }

    pub fn endian(&self) -> Endian {
        self.endian
    }

    pub fn base(&self) -> usize {
        self.base
    }

    fn bytes_at(&self, offset: usize, len: usize) -> Result<&'a [u8], Error> {
        let end = offset.checked_add(len).ok_or(Error::Overflow)?;
        self.data
            .get(offset..end)
            .ok_or(Error::OutOfFile { offset, len })
    }

    pub fn u16_at(&self, offset: usize) -> Result<u16, Error> {
        let bytes = self.bytes_at(offset, 2)?;
        let array: [u8; 2] = bytes.try_into().map_err(|_| Error::Overflow)?;
        Ok(match self.endian {
            Endian::Little => u16::from_le_bytes(array),
            Endian::Big => u16::from_be_bytes(array),
        })
    }

    pub fn u32_at(&self, offset: usize) -> Result<u32, Error> {
        let bytes = self.bytes_at(offset, 4)?;
        let array: [u8; 4] = bytes.try_into().map_err(|_| Error::Overflow)?;
        Ok(match self.endian {
            Endian::Little => u32::from_le_bytes(array),
            Endian::Big => u32::from_be_bytes(array),
        })
    }

    /// Indexes the IFD at `offset` (relative to the base). The entry table
    /// must lie inside the slice; each entry's value extent is recorded and
    /// checked when it is read.
    pub fn ifd(&self, offset: u32) -> Result<Ifd, Error> {
        let at = self
            .base
            .checked_add(offset as usize)
            .ok_or(Error::Overflow)?;
        let count = usize::from(self.u16_at(at)?);
        if count > MAX_ENTRIES {
            return Err(Error::TooManyEntries { offset: at, count });
        }
        let table_len = 2 + 12 * count + 4;
        self.bytes_at(at, table_len)?;
        let mut entries = Vec::with_capacity(count);
        for index in 0..count {
            let field = at + 2 + 12 * index;
            let tag = self.u16_at(field)?;
            let kind = self.u16_at(field + 2)?;
            let count = self.u32_at(field + 4)?;
            let len = (count as usize)
                .checked_mul(kind_size(kind))
                .ok_or(Error::Overflow)?;
            let offset = if len <= 4 {
                field + 8
            } else {
                self.base
                    .checked_add(self.u32_at(field + 8)? as usize)
                    .ok_or(Error::Overflow)?
            };
            entries.push(Entry {
                tag,
                kind,
                count,
                offset,
                len,
            });
        }
        let next = self.u32_at(at + 2 + 12 * count)?;
        Ok(Ifd {
            offset: at,
            entries,
            next,
        })
    }

    /// Follows next pointers from `first`, registering each IFD in
    /// `visited`, for at most `MAX_CHAIN` IFDs.
    pub fn chain(&self, first: u32, visited: &mut Visited) -> Result<Vec<Ifd>, Error> {
        let mut out = Vec::new();
        let mut offset = first;
        while offset != 0 {
            if out.len() >= MAX_CHAIN {
                return Err(Error::TooManyIfds);
            }
            let ifd = self.ifd(offset)?;
            visited.enter(ifd.offset)?;
            offset = ifd.next;
            out.push(ifd);
        }
        Ok(out)
    }

    /// The value bytes of an entry, checked to lie inside the slice.
    pub fn bytes(&self, entry: &Entry) -> Result<&'a [u8], Error> {
        self.bytes_at(entry.offset, entry.len)
    }

    /// The `index`th element of a BYTE, UNDEFINED, SHORT, LONG or IFD field
    /// as an integer. The field's whole declared extent must lie in the
    /// slice, not only the element read.
    pub fn integer(&self, entry: &Entry, index: usize) -> Result<u32, Error> {
        self.bytes(entry)?;
        if index >= entry.count as usize {
            return Err(Error::Index {
                tag: entry.tag,
                index,
            });
        }
        let size = kind_size(entry.kind);
        let at = entry
            .offset
            .checked_add(index.checked_mul(size).ok_or(Error::Overflow)?)
            .ok_or(Error::Overflow)?;
        match entry.kind {
            kind::BYTE | kind::UNDEFINED => self
                .bytes_at(at, 1)?
                .first()
                .map(|b| u32::from(*b))
                .ok_or(Error::OutOfFile { offset: at, len: 1 }),
            kind::SHORT => self.u16_at(at).map(u32::from),
            kind::LONG | kind::IFD => self.u32_at(at),
            other => Err(Error::Kind {
                tag: entry.tag,
                kind: other,
            }),
        }
    }

    /// The `index`th element of a RATIONAL field.
    pub fn rational(&self, entry: &Entry, index: usize) -> Result<(u32, u32), Error> {
        if entry.kind != kind::RATIONAL {
            return Err(Error::Kind {
                tag: entry.tag,
                kind: entry.kind,
            });
        }
        self.bytes(entry)?;
        if index >= entry.count as usize {
            return Err(Error::Index {
                tag: entry.tag,
                index,
            });
        }
        let at = entry
            .offset
            .checked_add(index.checked_mul(8).ok_or(Error::Overflow)?)
            .ok_or(Error::Overflow)?;
        Ok((self.u32_at(at)?, self.u32_at(at + 4)?))
    }

    /// An ASCII field up to its first NUL, trimmed of surrounding blanks.
    pub fn ascii(&self, entry: &Entry) -> Result<&'a str, Error> {
        if entry.kind != kind::ASCII {
            return Err(Error::Kind {
                tag: entry.tag,
                kind: entry.kind,
            });
        }
        let bytes = self.bytes(entry)?;
        let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
        let text = bytes.get(..end).ok_or(Error::Overflow)?;
        std::str::from_utf8(text)
            .map(str::trim)
            .map_err(|_| Error::NotAscii { tag: entry.tag })
    }
}
