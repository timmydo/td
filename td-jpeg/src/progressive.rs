//! Huffman progressive scans, T.81 Annex G. Coefficients stay quantised
//! until EOI; each component captures its quantiser at its first scan.

use super::*;

pub(super) struct Scan {
    components: Vec<ScanComponent>,
    ss: usize,
    se: usize,
    ah: u8,
    al: u8,
}

pub(super) fn scan(data: &[u8], frame: &Frame, tables: &Tables) -> Result<Scan, Error> {
    let count = usize::from(*data.first().ok_or(Error::Segment)?);
    if count == 0 || count > frame.components.len() || data.len() != 4 + 2 * count {
        return Err(Error::Scan);
    }
    let at = 1 + 2 * count;
    let ss = usize::from(*data.get(at).ok_or(Error::Segment)?);
    let se = usize::from(*data.get(at + 1).ok_or(Error::Segment)?);
    let approx = *data.get(at + 2).ok_or(Error::Segment)?;
    let (ah, al) = (approx >> 4, approx & 15);
    if ss > se
        || se > 63
        || (ss == 0 && se != 0)
        || (ss != 0 && count != 1)
        || ah > 13
        || al > 13
        || (ah != 0 && ah != al + 1)
    {
        return Err(Error::Scan);
    }
    let mut components: Vec<ScanComponent> = buffer(count)?;
    for k in 0..count {
        let id = *data.get(1 + 2 * k).ok_or(Error::Segment)?;
        let selectors = *data.get(2 + 2 * k).ok_or(Error::Segment)?;
        let (dc, ac) = (selectors >> 4, selectors & 15);
        let index = frame
            .components
            .iter()
            .position(|c| c.id == id)
            .ok_or(Error::Scan)?;
        if components.last().is_some_and(|last| last.index >= index)
            || dc > 3
            || ac > 3
            || (ss == 0 && ac != 0)
            || (ss != 0 && dc != 0)
        {
            return Err(Error::Scan);
        }
        if (ss == 0 && ah == 0 && tables.dc.get(usize::from(dc)).is_none_or(Option::is_none))
            || (ss != 0 && tables.ac.get(usize::from(ac)).is_none_or(Option::is_none))
        {
            return Err(Error::Table);
        }
        components.push(ScanComponent { index, dc, ac });
    }
    Ok(Scan {
        components,
        ss,
        se,
        ah,
        al,
    })
}

struct ComponentData {
    blocks: Vec<[i16; 64]>,
    stride: usize,
    rows: usize,
    actual_width: usize,
    actual_height: usize,
    bh: usize,
    bv: usize,
    xshift: u32,
    yshift: u32,
    quant: Option<[u16; 64]>,
    // Lowest bit sent for each zigzag coefficient, None before first use.
    precision: [Option<u8>; 64],
    pred: i32,
    samples: Vec<u8>,
}

fn planes(frame: &Frame) -> Result<Vec<ComponentData>, Error> {
    let (mx, my) = frame.mcu_grid();
    let (hm, vm) = frame.max_sampling();
    let single = frame.components.len() == 1;
    // Validate the total before allocating any coefficient plane.
    let total: usize = frame
        .components
        .iter()
        .map(|c| {
            let (bh, bv) = frame.blocks(c);
            mx * my * bh * bv * 64
        })
        .sum();
    if total > MAX_PROGRESSIVE_COEFFICIENTS {
        return Err(Error::Limit);
    }
    let mut result = buffer(frame.components.len())?;
    for c in &frame.components {
        let (bh, bv) = frame.blocks(c);
        result.push(ComponentData {
            blocks: filled(mx * my * bh * bv, [0; 64])?,
            stride: mx * bh,
            rows: my * bv,
            actual_width: if single {
                mx
            } else {
                (frame.width * bh).div_ceil(hm * 8)
            },
            actual_height: if single {
                my
            } else {
                (frame.height * bv).div_ceil(vm * 8)
            },
            bh,
            bv,
            xshift: if single { 0 } else { (hm / bh - 1) as u32 },
            yshift: if single { 0 } else { (vm / bv - 1) as u32 },
            quant: None,
            precision: [None; 64],
            pred: 0,
            samples: Vec::new(),
        });
    }
    Ok(result)
}

pub(super) fn decode(
    data: &[u8],
    mut parsed: Parsed,
    scale: Scale,
    sink: &mut dyn FnMut(&[i32; 64]),
) -> Result<Rgb8, Error> {
    let mut planes = planes(&parsed.frame)?;
    let mut scans = 0usize;
    let mut work = 0usize;
    let mut current = parsed.progression.take().ok_or(Error::Scan)?;
    let mut pos = parsed.data_start;
    loop {
        scans += 1;
        if scans > MAX_SCANS {
            return Err(Error::Limit);
        }
        for sc in &current.components {
            let p = planes.get_mut(sc.index).ok_or(Error::Scan)?;
            if current.ss != 0 && p.precision.first().copied().flatten().is_none() {
                return Err(Error::Scan);
            }
            for prior in p
                .precision
                .get_mut(current.ss..=current.se)
                .ok_or(Error::Scan)?
            {
                if (current.ah == 0 && prior.is_some())
                    || (current.ah != 0 && *prior != Some(current.ah))
                {
                    return Err(Error::Scan);
                }
                *prior = Some(current.al);
            }
            if p.quant.is_none() {
                let comp = parsed.frame.components.get(sc.index).ok_or(Error::Scan)?;
                p.quant = Some(tables_quant(&parsed.tables, comp.tq)?);
            }
            p.pred = 0;
            let visits = if current.components.len() == 1 {
                p.actual_width * p.actual_height
            } else {
                p.blocks.len()
            };
            work = work
                .checked_add(visits * (current.se - current.ss + 1))
                .ok_or(Error::Limit)?;
            if work > MAX_PROGRESSIVE_WORK {
                return Err(Error::Limit);
            }
        }
        pos = decode_scan(data, pos, &parsed, &current, &mut planes)?;
        loop {
            parsed.segments += 1;
            if parsed.segments > MAX_SEGMENTS {
                return Err(Error::Limit);
            }
            if data.get(pos) != Some(&0xff) {
                return Err(Error::NotJpeg);
            }
            while data.get(pos) == Some(&0xff) {
                pos += 1;
            }
            let marker = *data.get(pos).ok_or(Error::Truncated)?;
            pos += 1;
            if marker == 0xd9 {
                if planes
                    .iter()
                    .any(|p| p.precision.first().copied().flatten().is_none())
                {
                    return Err(Error::Scan);
                }
                return finish(&parsed.frame, scale, planes, sink);
            }
            if marker == 0 || marker == 0xd8 || (0xd0..=0xd7).contains(&marker) {
                return Err(Error::Scan);
            }
            let len = usize::from(u16_at(data, pos)?);
            if len < 2 {
                return Err(Error::Segment);
            }
            let end = pos.checked_add(len).ok_or(Error::Truncated)?;
            let segment = data.get(pos + 2..end).ok_or(Error::Truncated)?;
            pos = end;
            match marker {
                0xdb => parse_quant(segment, &mut parsed.tables, &mut parsed.definitions)?,
                0xc4 => parse_huffman(segment, &mut parsed.tables, &mut parsed.definitions)?,
                0xdd if segment.len() == 2 => parsed.restart = usize::from(u16_at(segment, 0)?),
                0xdd => return Err(Error::Segment),
                0xda => {
                    current = scan(segment, &parsed.frame, &parsed.tables)?;
                    break;
                }
                0xc0..=0xcf | 0xde | 0xdf => return Err(Error::Unsupported(marker)),
                0xe0..=0xef | 0xfe => {}
                _ => return Err(Error::Segment),
            }
        }
    }
}

fn decode_scan(
    data: &[u8],
    pos: usize,
    parsed: &Parsed,
    scan: &Scan,
    planes: &mut [ComponentData],
) -> Result<usize, Error> {
    let single = scan.components.len() == 1;
    let (mx, my) = if single {
        let sc = scan.components.first().ok_or(Error::Scan)?;
        let p = planes.get(sc.index).ok_or(Error::Scan)?;
        (p.actual_width, p.actual_height)
    } else {
        parsed.frame.mcu_grid()
    };
    let mut bits = Bits::new(data, pos);
    let mut eob = 0usize;
    let mut rst = 0u8;
    for mcu in 0..mx * my {
        if parsed.restart != 0 && mcu != 0 && mcu.is_multiple_of(parsed.restart) {
            if eob != 0 {
                return Err(Error::Run);
            }
            bits.restart(rst)?;
            rst = (rst + 1) % 8;
            for p in planes.iter_mut() {
                p.pred = 0;
            }
        }
        for sc in &scan.components {
            let p = planes.get_mut(sc.index).ok_or(Error::Scan)?;
            let (bh, bv) = if single { (1, 1) } else { (p.bh, p.bv) };
            for y in 0..bv {
                for x in 0..bh {
                    let at = (mcu / mx * bv + y) * p.stride + mcu % mx * bh + x;
                    let block = p.blocks.get_mut(at).ok_or(Error::Scan)?;
                    if scan.ss == 0 {
                        let dc = block.first_mut().ok_or(Error::Scan)?;
                        if scan.ah == 0 {
                            let table = parsed
                                .tables
                                .dc
                                .get(usize::from(sc.dc))
                                .and_then(Option::as_ref)
                                .ok_or(Error::Table)?;
                            let size = table.decode(&mut bits)?;
                            if size > 11 {
                                return Err(Error::Huffman);
                            }
                            p.pred = p
                                .pred
                                .checked_add(extend(bits.get(u32::from(size)), size))
                                .ok_or(Error::Huffman)?;
                            *dc = coefficient(p.pred, scan.al)?;
                        } else {
                            *dc |= (bits.get(1) as i16) << scan.al;
                        }
                    } else {
                        let ac = parsed
                            .tables
                            .ac
                            .get(usize::from(sc.ac))
                            .and_then(Option::as_ref)
                            .ok_or(Error::Table)?;
                        if scan.ah == 0 {
                            ac_first(&mut bits, ac, scan, block, &mut eob)?;
                        } else {
                            ac_refine(&mut bits, ac, scan, block, &mut eob)?;
                        }
                    }
                    if bits.overrun() {
                        return Err(Error::Truncated);
                    }
                }
            }
        }
    }
    if eob != 0 {
        return Err(Error::Run);
    }
    // DC refinement may consume no Huffman lookahead. Deliver the remaining
    // real bytes so excess data is detected even at an exact byte boundary.
    bits.refill();
    if bits.excess() {
        return Err(Error::Scan);
    }
    bits.marker()
        .map(|_| bits.pos)
        .ok_or(if data.len().saturating_sub(bits.pos) < 2 {
            Error::Truncated
        } else {
            Error::Scan
        })
}

fn coefficient(value: i32, al: u8) -> Result<i16, Error> {
    value
        .checked_mul(1 << al)
        .and_then(|v| i16::try_from(v).ok())
        .ok_or(Error::Huffman)
}

fn ac_first(
    bits: &mut Bits<'_>,
    table: &Lut,
    scan: &Scan,
    block: &mut [i16; 64],
    eob: &mut usize,
) -> Result<(), Error> {
    if *eob != 0 {
        *eob -= 1;
        return Ok(());
    }
    let mut k = scan.ss;
    while k <= scan.se {
        let rs = table.decode(bits)?;
        let (run, size) = (usize::from(rs >> 4), rs & 15);
        if size == 0 {
            if run == 15 {
                k += 16;
                if k > scan.se + 1 {
                    return Err(Error::Run);
                }
            } else {
                *eob = (1 << run) + bits.get(run as u32) as usize - 1;
                break;
            }
        } else {
            if size > 10 {
                return Err(Error::Huffman);
            }
            k += run;
            if k > scan.se {
                return Err(Error::Run);
            }
            let natural = usize::from(*ZIGZAG.get(k).ok_or(Error::Run)?);
            *block.get_mut(natural).ok_or(Error::Run)? =
                coefficient(extend(bits.get(u32::from(size)), size), scan.al)?;
            k += 1;
        }
    }
    Ok(())
}

fn correct(bits: &mut Bits<'_>, value: &mut i16, al: u8) -> Result<(), Error> {
    if bits.get(1) != 0 {
        let delta = 1i32 << al;
        let old = i32::from(*value);
        *value = i16::try_from(if old < 0 { old - delta } else { old + delta })
            .map_err(|_| Error::Huffman)?;
    }
    Ok(())
}

fn ac_refine(
    bits: &mut Bits<'_>,
    table: &Lut,
    scan: &Scan,
    block: &mut [i16; 64],
    eob: &mut usize,
) -> Result<(), Error> {
    let mut k = scan.ss;
    if *eob == 0 {
        while k <= scan.se {
            let rs = table.decode(bits)?;
            let (run, size) = (usize::from(rs >> 4), rs & 15);
            if size > 1 {
                return Err(Error::Huffman);
            }
            if size == 0 && run != 15 {
                *eob = (1 << run) + bits.get(run as u32) as usize;
                break;
            }
            let new = if size == 1 {
                if bits.get(1) == 0 {
                    -(1i16 << scan.al)
                } else {
                    1i16 << scan.al
                }
            } else {
                0
            };
            let mut zeros = if size == 0 { 16 } else { run };
            let mut completed = false;
            while k <= scan.se {
                let natural = usize::from(*ZIGZAG.get(k).ok_or(Error::Run)?);
                let value = block.get_mut(natural).ok_or(Error::Run)?;
                if *value != 0 {
                    correct(bits, value, scan.al)?;
                } else if zeros != 0 {
                    zeros -= 1;
                    if zeros == 0 && size == 0 {
                        k += 1;
                        completed = true;
                        break;
                    }
                } else {
                    *value = new;
                    k += 1;
                    completed = true;
                    break;
                }
                k += 1;
            }
            if !completed {
                return Err(Error::Run);
            }
        }
    }
    if *eob != 0 {
        while k <= scan.se {
            let natural = usize::from(*ZIGZAG.get(k).ok_or(Error::Run)?);
            let value = block.get_mut(natural).ok_or(Error::Run)?;
            if *value != 0 {
                correct(bits, value, scan.al)?;
            }
            k += 1;
        }
        *eob -= 1;
    }
    Ok(())
}

fn finish(
    frame: &Frame,
    scale: Scale,
    mut planes: Vec<ComponentData>,
    sink: &mut dyn FnMut(&[i32; 64]),
) -> Result<Rgb8, Error> {
    let n = scale.n();
    for p in &mut planes {
        p.samples = filled(p.stride * p.rows * n * n, 0)?;
    }
    let (mx, my) = frame.mcu_grid();
    let mut block = [0i32; 64];
    let mut pixels = [0u8; 64];
    // Match baseline's MCU/component/block callback order, after all scans.
    for y in 0..my {
        for x in 0..mx {
            for p in &mut planes {
                let quant = p.quant.as_ref().ok_or(Error::Table)?;
                for by in 0..p.bv {
                    for bx in 0..p.bh {
                        let ox = x * p.bh + bx;
                        let oy = y * p.bv + by;
                        let source = p.blocks.get(oy * p.stride + ox).ok_or(Error::Scan)?;
                        for (k, &natural) in ZIGZAG.iter().enumerate() {
                            let natural = usize::from(natural);
                            *block.get_mut(natural).ok_or(Error::Run)? =
                                i32::from(*source.get(natural).ok_or(Error::Run)?)
                                    * i32::from(*quant.get(k).ok_or(Error::Table)?);
                        }
                        sink(&block);
                        idct(&block, scale, &mut pixels);
                        for (dy, row) in pixels.chunks_exact(n).take(n).enumerate() {
                            let start = (oy * n + dy) * p.stride * n + ox * n;
                            p.samples
                                .get_mut(start..start + n)
                                .ok_or(Error::Scan)?
                                .copy_from_slice(row);
                        }
                    }
                }
            }
        }
    }
    // Release the larger coefficient planes before allocating RGB output.
    for p in &mut planes {
        p.blocks = Vec::new();
    }
    render(frame, scale, &planes, |p, x, y| {
        p.samples
            .get((y >> p.yshift) * p.stride * n + (x >> p.xshift))
            .copied()
            .unwrap_or(0)
    })
}
