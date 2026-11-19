//! RFC 6376 canonicalization over borrowed SMTP octets.
use super::{alive, Checked, Error, MAX_FIELDS};
use td_crypto::{BulkSha256, Digest};
use td_mime::{
    headers::{Field, Scanner, Status},
    time::{Deadline, Tick},
    work::{Charge, Meter},
};

const BLOCK: usize = 4096;
pub(super) const MAX_HEADERS: usize = 256 * 1024;

pub(super) struct Headers<'a> {
    pub fields: Vec<Field>,
    pub raw: &'a [u8],
    pub body: &'a [u8],
}

impl<'a> Headers<'a> {
    pub fn read(raw: &'a [u8], limit: usize, current: &dyn Fn() -> bool) -> Checked<Self> {
        let cap = limit.min(MAX_HEADERS);
        let prefix = raw.get(..cap + 2).unwrap_or(raw);
        let mut scan = Scanner::new(0, cap as u64);
        let mut fields = Vec::new();
        let mut offset = 0usize;
        let mut meter = Meter::new(
            Deadline::at(Tick(1)),
            Charge {
                io_bytes: (2 * (cap + 3)) as u64,
                records: (2 * (cap + 3)) as u64,
                ..Charge::default()
            },
        );
        loop {
            alive(current)?;
            let part = prefix.get(offset..).ok_or(Error::Invalid)?;
            let step = scan
                .poll(part, prefix.len() == raw.len(), Tick(0), &mut meter)
                .map_err(|_| Error::Unavailable)?;
            offset += step.consumed;
            match step.status {
                Status::Field(field) => {
                    if fields.len() == MAX_FIELDS {
                        return Err(Error::Unavailable);
                    }
                    td_alloc::push(&mut fields, field)?;
                }
                Status::Yield => {}
                Status::NeedInput => return Err(Error::Unavailable),
                Status::Complete(end) => {
                    let start = end.body_start as usize;
                    let body = raw.get(start..).ok_or(Error::Invalid)?;
                    let separator = raw
                        .get(end.header_bytes as usize..start)
                        .ok_or(Error::Invalid)?;
                    if !body.is_empty() && separator != b"\r\n" {
                        return Err(Error::Invalid);
                    }
                    return Ok(Self { fields, raw, body });
                }
            }
        }
    }

    pub fn name(&self, field: Field) -> Checked<&'a [u8]> {
        self.raw
            .get(field.name_start as usize..field.name_end as usize)
            .ok_or(Error::Invalid)
    }

    pub fn value(&self, field: Field) -> Checked<&'a [u8]> {
        self.raw
            .get(field.value_start as usize..field.value_end as usize)
            .ok_or(Error::Invalid)
    }
}

fn digest_error(_: td_crypto::Error) -> Error {
    Error::Unavailable
}

struct Output {
    hash: BulkSha256,
    buffer: [u8; BLOCK],
    used: usize,
    remaining: Option<u64>,
}

impl Output {
    fn new(remaining: Option<u64>) -> Checked<Self> {
        Ok(Self {
            hash: BulkSha256::try_new().map_err(digest_error)?,
            buffer: [0; BLOCK],
            used: 0,
            remaining,
        })
    }
    fn flush(&mut self) -> Checked<()> {
        let part = self.buffer.get(..self.used).ok_or(Error::Unavailable)?;
        self.hash.update(part).map_err(digest_error)?;
        self.used = 0;
        Ok(())
    }
    fn put(&mut self, byte: u8) -> Checked<()> {
        if let Some(n) = &mut self.remaining {
            if *n == 0 {
                return Ok(());
            }
            *n -= 1;
        }
        *self.buffer.get_mut(self.used).ok_or(Error::Unavailable)? = byte;
        self.used += 1;
        if self.used == BLOCK {
            self.flush()?;
        }
        Ok(())
    }
    fn crlf(&mut self) -> Checked<()> {
        self.put(b'\r')?;
        self.put(b'\n')
    }
    fn finish(mut self) -> Checked<[u8; 32]> {
        if self.remaining.is_some_and(|n| n > 0) {
            return Err(Error::Invalid);
        }
        self.flush()?;
        self.hash.finish().map_err(digest_error)
    }
}

pub(super) fn body(
    raw: &[u8],
    relaxed: bool,
    limit: Option<u64>,
    current: &dyn Fn() -> bool,
) -> Checked<[u8; 32]> {
    let mut out = Output::new(limit)?;
    let mut cr = false;
    let mut wsp = false;
    let mut pending_lines = 0usize;
    let mut nonempty = false;
    for chunk in raw.chunks(BLOCK) {
        alive(current)?;
        for &byte in chunk {
            if out.remaining == Some(0) {
                return out.finish();
            }
            if cr {
                if byte != b'\n' {
                    return Err(Error::Invalid);
                }
                pending_lines += 1;
                wsp = false;
                cr = false;
                continue;
            }
            match byte {
                b'\r' => cr = true,
                b'\n' => return Err(Error::Invalid),
                b' ' | b'\t' if relaxed => wsp = true,
                _ => {
                    while pending_lines != 0 && out.remaining != Some(0) {
                        if pending_lines.is_multiple_of(BLOCK / 2) {
                            alive(current)?;
                        }
                        out.crlf()?;
                        pending_lines -= 1;
                    }
                    if wsp {
                        out.put(b' ')?;
                    }
                    wsp = false;
                    out.put(byte)?;
                    nonempty = true;
                }
            }
        }
    }
    if cr {
        return Err(Error::Invalid);
    }
    if !relaxed || nonempty {
        out.crlf()?;
    }
    out.finish()
}

fn header(
    out: &mut Output,
    headers: &Headers<'_>,
    field: Field,
    relaxed: bool,
    skip: Option<std::ops::Range<usize>>,
    current: &dyn Fn() -> bool,
) -> Checked<()> {
    let name = headers.name(field)?;
    let value = headers.value(field)?;
    let (first, last) = match skip {
        None => (value, &[][..]),
        Some(range) => (
            value.get(..range.start).ok_or(Error::Invalid)?,
            value.get(range.end..).ok_or(Error::Invalid)?,
        ),
    };
    let prefix = if relaxed {
        name
    } else {
        headers
            .raw
            .get(field.name_start as usize..field.value_start as usize)
            .ok_or(Error::Invalid)?
    };
    for part in prefix.chunks(BLOCK) {
        alive(current)?;
        for &byte in part {
            out.put(if relaxed {
                byte.to_ascii_lowercase()
            } else {
                byte
            })?;
        }
    }
    if relaxed {
        out.put(b':')?;
    }
    let mut seen = false;
    let mut wsp = false;
    for span in [first, last] {
        for part in span.chunks(BLOCK) {
            alive(current)?;
            for &byte in part {
                match byte {
                    b'\r' | b'\n' if relaxed => {}
                    b' ' | b'\t' if relaxed => wsp = seen,
                    _ => {
                        if wsp {
                            out.put(b' ')?;
                        }
                        out.put(byte)?;
                        wsp = false;
                        seen = true;
                    }
                }
            }
        }
    }
    Ok(())
}

pub(super) fn header_hash(
    headers: &Headers<'_>,
    own: Field,
    names: &[&str],
    relaxed: bool,
    skip: std::ops::Range<usize>,
    current: &dyn Fn() -> bool,
) -> Checked<[u8; 32]> {
    let mut out = Output::new(None)?;
    let mut used = [false; MAX_FIELDS];
    let mut signed_from = false;
    for &name in names {
        alive(current)?;
        for (index, field) in headers.fields.iter().enumerate().rev() {
            let selected = used.get_mut(index).ok_or(Error::Unavailable)?;
            if !*selected
                && *field != own
                && headers.name(*field)?.eq_ignore_ascii_case(name.as_bytes())
            {
                *selected = true;
                signed_from |= name.eq_ignore_ascii_case("from");
                header(&mut out, headers, *field, relaxed, None, current)?;
                out.crlf()?;
                break;
            }
        }
    }
    if !signed_from {
        return Err(Error::Invalid);
    }
    header(&mut out, headers, own, relaxed, Some(skip), current)?;
    out.finish()
}
