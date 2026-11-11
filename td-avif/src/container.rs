use crate::{
    dimensions, Colour, Error, Header, MAX_BOXES, MAX_EXTENTS, MAX_INPUT_BYTES, MAX_ITEMS,
    MAX_PROPERTIES,
};

struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}
impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8], Error> {
        let end = self.pos.checked_add(n).ok_or(Error::Limit)?;
        let out = self.bytes.get(self.pos..end).ok_or(Error::Truncated)?;
        self.pos = end;
        Ok(out)
    }
    fn number(&mut self, n: usize) -> Result<usize, Error> {
        if n > 8 {
            return Err(Error::Container);
        }
        self.take(n)?.iter().try_fold(0usize, |v, b| {
            v.checked_mul(256)
                .and_then(|v| v.checked_add(usize::from(*b)))
                .ok_or(Error::Limit)
        })
    }
    fn rest(&mut self) -> Result<&'a [u8], Error> {
        self.take(self.bytes.len() - self.pos)
    }
    fn done(&self) -> Result<(), Error> {
        if self.pos == self.bytes.len() {
            Ok(())
        } else {
            Err(Error::Container)
        }
    }
    fn full(&mut self, versions: &[usize], flags: usize) -> Result<usize, Error> {
        let version = self.number(1)?;
        if !versions.contains(&version) || self.number(3)? & !flags != 0 {
            return Err(Error::Unsupported);
        }
        Ok(version)
    }
}

#[derive(Clone, Copy)]
struct BoxData<'a> {
    kind: &'a [u8],
    bytes: &'a [u8],
    start: usize,
}
struct Boxes<'a> {
    reader: Reader<'a>,
    base: usize,
}
impl<'a> Boxes<'a> {
    fn new(bytes: &'a [u8], base: usize) -> Self {
        Self {
            reader: Reader::new(bytes),
            base,
        }
    }
    fn next(&mut self, budget: &mut usize) -> Result<Option<BoxData<'a>>, Error> {
        if self.reader.pos == self.reader.bytes.len() {
            return Ok(None);
        }
        *budget = budget.checked_sub(1).ok_or(Error::Limit)?;
        let start = self.reader.pos;
        let mut size = self.reader.number(4)?;
        let kind = self.reader.take(4)?;
        if size == 1 {
            size = self.reader.number(8)?;
        } else if size == 0 {
            size = self.reader.bytes.len() - start;
        }
        let header = self.reader.pos - start;
        let length = size.checked_sub(header).ok_or(Error::Container)?;
        let body_start = self.base.checked_add(self.reader.pos).ok_or(Error::Limit)?;
        let bytes = self.reader.take(length)?;
        Ok(Some(BoxData {
            kind,
            bytes,
            start: body_start,
        }))
    }
}

fn unique<'a>(slot: &mut Option<BoxData<'a>>, value: BoxData<'a>) -> Result<(), Error> {
    if slot.replace(value).is_some() {
        Err(Error::Container)
    } else {
        Ok(())
    }
}
fn push<T>(out: &mut Vec<T>, value: T, limit: usize) -> Result<(), Error> {
    if out.len() >= limit {
        return Err(Error::Limit);
    }
    td_alloc::reserve(out, 1).map_err(|_| Error::Limit)?;
    out.push(value);
    Ok(())
}

pub(super) struct Parsed<'a> {
    pub header: Header,
    pub config: [u8; 4],
    pub extents: Vec<&'a [u8]>,
    pub length: usize,
    pub colour: Option<Colour>,
}

pub(super) fn parse(bytes: &[u8]) -> Result<Parsed<'_>, Error> {
    if bytes.len() > MAX_INPUT_BYTES {
        return Err(Error::Limit);
    }
    let mut budget = MAX_BOXES;
    let mut boxes = Boxes::new(bytes, 0);
    let first = boxes.next(&mut budget)?.ok_or(Error::NotAvif)?;
    avif_brand(first)?;
    let mut meta = None;
    let mut mdats = Vec::new();
    while let Some(b) = boxes.next(&mut budget)? {
        match b.kind {
            b"ftyp" => return Err(Error::Container),
            b"moov" => return Err(Error::Unsupported),
            b"meta" => unique(&mut meta, b)?,
            b"mdat" => push(&mut mdats, b.start..b.start + b.bytes.len(), MAX_BOXES)?,
            _ => {}
        }
    }
    let meta = meta.ok_or(Error::Container)?;
    let mut r = Reader::new(meta.bytes);
    r.full(&[0], 0)?;
    let mut children = Boxes::new(r.rest()?, meta.start + 4);
    let (mut pitm, mut iloc, mut iinf, mut iprp, mut idat, mut hdlr) =
        (None, None, None, None, None, None);
    while let Some(b) = children.next(&mut budget)? {
        match b.kind {
            b"pitm" => unique(&mut pitm, b)?,
            b"iloc" => unique(&mut iloc, b)?,
            b"iinf" => unique(&mut iinf, b)?,
            b"iprp" => unique(&mut iprp, b)?,
            b"idat" => unique(&mut idat, b)?,
            b"hdlr" => unique(&mut hdlr, b)?,
            // References can affect rendering (alpha, derived images, premultiplied data).
            b"iref" => references(b, &mut budget)?,
            b"dinf" => return Err(Error::Unsupported),
            _ => {}
        }
    }
    let mut h = Reader::new(hdlr.ok_or(Error::Container)?.bytes);
    h.full(&[0], 0)?;
    if h.number(4)? != 0 || h.take(4)? != b"pict" {
        return Err(Error::Container);
    }
    h.take(12)?;
    let mut p = Reader::new(pitm.ok_or(Error::Container)?.bytes);
    let version = p.full(&[0, 1], 0)?;
    let primary = p.number(if version == 0 { 2 } else { 4 })?;
    p.done()?;
    if primary == 0 {
        return Err(Error::Container);
    }
    item_type(iinf.ok_or(Error::Container)?, primary, &mut budget)?;
    let (header, config, colour) = properties(iprp.ok_or(Error::Container)?, primary, &mut budget)?;
    let (extents, length) = locations(iloc.ok_or(Error::Container)?, primary, bytes, idat, &mdats)?;
    Ok(Parsed {
        header,
        config,
        extents,
        length,
        colour,
    })
}

fn references(b: BoxData<'_>, budget: &mut usize) -> Result<(), Error> {
    let mut r = Reader::new(b.bytes);
    let version = r.full(&[0, 1], 0)?;
    let width = if version == 0 { 2 } else { 4 };
    let mut boxes = Boxes::new(r.rest()?, b.start + 4);
    while let Some(child) = boxes.next(budget)? {
        if child.kind != b"cdsc" && child.kind != b"thmb" {
            return Err(Error::Unsupported);
        }
        let mut r = Reader::new(child.bytes);
        r.number(width)?;
        let n = r.number(2)?;
        if n > MAX_ITEMS {
            return Err(Error::Limit);
        }
        r.take(n * width)?;
        r.done()?;
    }
    Ok(())
}

fn item_type(b: BoxData<'_>, primary: usize, budget: &mut usize) -> Result<(), Error> {
    let mut r = Reader::new(b.bytes);
    let v = r.full(&[0, 1], 0)?;
    let n = r.number(if v == 0 { 2 } else { 4 })?;
    if n > MAX_ITEMS {
        return Err(Error::Limit);
    }
    let base = b.start + r.pos;
    let mut boxes = Boxes::new(r.rest()?, base);
    let mut ids = Vec::new();
    let mut found = false;
    for _ in 0..n {
        let child = boxes.next(budget)?.ok_or(Error::Container)?;
        if child.kind != b"infe" {
            return Err(Error::Container);
        }
        let mut r = Reader::new(child.bytes);
        let version = r.full(&[2, 3], 1)?;
        let id = r.number(if version == 2 { 2 } else { 4 })?;
        if id == 0 || ids.contains(&id) {
            return Err(Error::Container);
        }
        push(&mut ids, id, MAX_ITEMS)?;
        let protection = r.number(2)?;
        let kind = r.take(4)?;
        if !r.rest()?.contains(&0) {
            return Err(Error::Container);
        }
        if id == primary {
            if kind != b"av01" || protection != 0 {
                return Err(Error::Unsupported);
            }
            found = true;
        }
    }
    if boxes.next(budget)?.is_some() || !found {
        return Err(Error::Container);
    }
    Ok(())
}

fn properties(
    b: BoxData<'_>,
    primary: usize,
    budget: &mut usize,
) -> Result<(Header, [u8; 4], Option<Colour>), Error> {
    let mut boxes = Boxes::new(b.bytes, b.start);
    let (mut ipco, mut ipma) = (None, None);
    while let Some(child) = boxes.next(budget)? {
        match child.kind {
            b"ipco" => unique(&mut ipco, child)?,
            b"ipma" => unique(&mut ipma, child)?,
            _ => {}
        }
    }
    let ipco = ipco.ok_or(Error::Container)?;
    let mut boxes = Boxes::new(ipco.bytes, ipco.start);
    let mut props = Vec::new();
    while let Some(child) = boxes.next(budget)? {
        push(&mut props, child, MAX_PROPERTIES)?;
    }
    let mut r = Reader::new(ipma.ok_or(Error::Container)?.bytes);
    let version = r.number(1)?;
    let flags = r.number(3)?;
    if version > 1 || flags > 1 {
        return Err(Error::Unsupported);
    }
    let n = r.number(4)?;
    if n > MAX_ITEMS {
        return Err(Error::Limit);
    }
    let mut ids = Vec::new();
    let mut selected = Vec::new();
    for _ in 0..n {
        let id = r.number(if version == 0 { 2 } else { 4 })?;
        if id == 0 || ids.contains(&id) {
            return Err(Error::Container);
        }
        push(&mut ids, id, MAX_ITEMS)?;
        let count = r.number(1)?;
        for _ in 0..count {
            let association = r.number(1 + flags)?;
            let essential_bit = 1 << (7 + flags * 8);
            let index = association & (essential_bit - 1);
            if index == 0 {
                if association != 0 {
                    return Err(Error::Container);
                }
                continue;
            }
            let prop = *props.get(index - 1).ok_or(Error::Container)?;
            if id == primary {
                if selected
                    .iter()
                    .any(|(p, _): &(BoxData<'_>, bool)| p.start == prop.start)
                {
                    return Err(Error::Container);
                }
                push(
                    &mut selected,
                    (prop, association & essential_bit != 0),
                    MAX_PROPERTIES,
                )?;
            }
        }
    }
    r.done()?;
    let (mut header, mut config, mut pixel, mut colour) = (None, None, None, None);
    for (p, essential) in selected {
        let mut r = Reader::new(p.bytes);
        match p.kind {
            b"ispe" => {
                if header.is_some() {
                    return Err(Error::Container);
                }
                r.full(&[0], 0)?;
                header = Some(dimensions(r.number(4)?, r.number(4)?)?);
                r.done()?;
            }
            b"av1C" => {
                if config.is_some() {
                    return Err(Error::Container);
                }
                let value: [u8; 4] = r.take(4)?.try_into().map_err(|_| Error::Container)?;
                if value.first() != Some(&0x81) {
                    return Err(Error::Unsupported);
                }
                // Config OBUs and nonzero presentation delay need their own validation.
                if value.get(3) != Some(&0) || r.done().is_err() {
                    return Err(Error::Unsupported);
                }
                config = Some(value);
            }
            b"pixi" => {
                if pixel.is_some() {
                    return Err(Error::Container);
                }
                r.full(&[0], 0)?;
                let n = r.number(1)?;
                if n != 1 && n != 3 {
                    return Err(Error::Unsupported);
                }
                let depths = r.take(n)?;
                if !depths.iter().all(|d| *d == 8 || *d == 10 || *d == 12)
                    || depths.iter().any(|d| Some(d) != depths.first())
                {
                    return Err(Error::Container);
                }
                pixel = Some((n, depths.first().copied().ok_or(Error::Container)?));
                r.done()?;
            }
            b"colr" => {
                if colour.is_some() {
                    return Err(Error::Container);
                }
                if r.take(4)? != b"nclx" {
                    return Err(Error::Unsupported);
                }
                let primaries = r.number(2)? as u16;
                let transfer = r.number(2)? as u16;
                let matrix = r.number(2)? as u16;
                let range = r.number(1)?;
                if range & 0x7f != 0 {
                    return Err(Error::Container);
                }
                colour = Some(Colour {
                    primaries,
                    transfer,
                    matrix,
                    full_range: range & 0x80 != 0,
                });
                r.done()?;
            }
            // Transformative properties cannot silently be ignored even when nonessential.
            b"clap" | b"irot" | b"imir" | b"auxC" | b"lsel" | b"a1op" => {
                return Err(Error::Unsupported)
            }
            _ if essential => return Err(Error::Unsupported),
            _ => {}
        }
    }
    let config = config.ok_or(Error::Container)?;
    let value = config.get(2).copied().ok_or(Error::Container)?;
    if let Some((channels, depth)) = pixel {
        let expected_channels = if value & 16 != 0 { 1 } else { 3 };
        let expected_depth = if value & 64 == 0 {
            8
        } else if value & 32 == 0 {
            10
        } else {
            12
        };
        if channels != expected_channels || depth != expected_depth {
            return Err(Error::Container);
        }
    }
    Ok((header.ok_or(Error::Container)?, config, colour))
}

fn locations<'a>(
    b: BoxData<'_>,
    primary: usize,
    bytes: &'a [u8],
    idat: Option<BoxData<'a>>,
    mdats: &[std::ops::Range<usize>],
) -> Result<(Vec<&'a [u8]>, usize), Error> {
    let mut r = Reader::new(b.bytes);
    let version = r.full(&[0, 1, 2], 0)?;
    let sizes = r.number(2)?;
    let offset_size = (sizes >> 12) & 15;
    let length_size = (sizes >> 8) & 15;
    let base_size = (sizes >> 4) & 15;
    let index_size = sizes & 15;
    if [offset_size, length_size, base_size, index_size]
        .iter()
        .any(|v| ![0, 4, 8].contains(v))
        || (version == 0 && index_size != 0)
    {
        return Err(Error::Unsupported);
    }
    let n = r.number(if version < 2 { 2 } else { 4 })?;
    if n > MAX_ITEMS {
        return Err(Error::Limit);
    }
    let mut ids = Vec::new();
    let mut extents = Vec::new();
    let mut total = 0usize;
    let mut extent_budget = MAX_EXTENTS;
    for _ in 0..n {
        let id = r.number(if version < 2 { 2 } else { 4 })?;
        if id == 0 || ids.contains(&id) {
            return Err(Error::Container);
        }
        push(&mut ids, id, MAX_ITEMS)?;
        let method = if version == 0 { 0 } else { r.number(2)? };
        let reference = r.number(2)?;
        let base = r.number(base_size)?;
        let count = r.number(2)?;
        extent_budget = extent_budget.checked_sub(count).ok_or(Error::Limit)?;
        for _ in 0..count {
            let index = if version != 0 {
                r.number(index_size)?
            } else {
                0
            };
            let offset = r.number(offset_size)?;
            let length = r.number(length_size)?;
            if id != primary {
                continue;
            }
            if reference != 0 || method > 1 || index != 0 || length_size == 0 {
                return Err(Error::Unsupported);
            }
            let start = base.checked_add(offset).ok_or(Error::Limit)?;
            let end = start.checked_add(length).ok_or(Error::Limit)?;
            let source = if method == 1 {
                idat.ok_or(Error::Container)?.bytes
            } else {
                if !mdats.iter().any(|m| start >= m.start && end <= m.end) {
                    return Err(Error::Container);
                }
                bytes
            };
            let extent = source.get(start..end).ok_or(Error::Truncated)?;
            total = total
                .checked_add(length)
                .filter(|v| *v <= MAX_INPUT_BYTES)
                .ok_or(Error::Limit)?;
            push(&mut extents, extent, MAX_EXTENTS)?;
        }
    }
    r.done()?;
    if total == 0 {
        return Err(Error::Container);
    }
    Ok((extents, total))
}

fn avif_brand(first: BoxData<'_>) -> Result<(), Error> {
    if first.kind != b"ftyp" || first.bytes.len() < 8 || !first.bytes.len().is_multiple_of(4) {
        return Err(Error::NotAvif);
    }
    if first
        .bytes
        .as_chunks::<4>()
        .0
        .iter()
        .enumerate()
        .any(|(i, brand)| i != 1 && brand == b"avis")
    {
        return Err(Error::Unsupported);
    }
    if !first
        .bytes
        .as_chunks::<4>()
        .0
        .iter()
        .enumerate()
        .any(|(i, brand)| i != 1 && brand == b"avif")
    {
        return Err(Error::NotAvif);
    }
    Ok(())
}

pub(super) fn is_avif(bytes: &[u8]) -> bool {
    if bytes.len() > MAX_INPUT_BYTES {
        return false;
    }
    let mut budget = 1;
    Boxes::new(bytes, 0)
        .next(&mut budget)
        .ok()
        .flatten()
        .is_some_and(|first| avif_brand(first).is_ok())
}
