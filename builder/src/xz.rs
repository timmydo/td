//! Minimal XZ (LZMA2) reader for td-builder.
//!
//! Kept in-tree and std-only for the same reason as `tar.rs`/`gzip.rs`: the
//! bootstrap engine unpacks pinned source tarballs natively so recipes need
//! no host tar/xz (re #469).
//!
//! Scope: enough of the .xz container to decode real release tarballs —
//! stream header/footer, block headers, the index, multi-stream
//! concatenation with stream padding, and the LZMA2 filter in full
//! (dictionary/state resets, uncompressed chunks, compressed chunks with
//! new or reused properties). All four standard check types are verified:
//! None, CRC32, CRC64 (ECMA-182), and SHA-256 (reusing `sha256.rs`).
//! BCJ/delta filters are rejected by id with an error naming the filter —
//! GNU and kernel.org release tarballs are plain LZMA2.

use crate::sha256::Sha256;

const XZ_MAGIC: [u8; 6] = [0xfd, b'7', b'z', b'X', b'Z', 0x00];
const XZ_FOOTER_MAGIC: [u8; 2] = [b'Y', b'Z'];
const MAX_DICT_BYTES: u64 = 256 * 1024 * 1024;
const MAX_XZ_OUTPUT_BYTES: u64 = 4 * 1024 * 1024 * 1024;

// Range coder model (LZMA): 11-bit probabilities, renormalized at 2^24.
const RC_TOP: u32 = 1 << 24;
const RC_MODEL_TOTAL_BITS: u32 = 11;
const RC_MODEL_TOTAL: u16 = 1 << RC_MODEL_TOTAL_BITS;
const RC_MOVE_BITS: u32 = 5;
const PROB_INIT: u16 = RC_MODEL_TOTAL / 2;

const STATES: usize = 12;
const POS_STATES_MAX: usize = 16;
const LIT_CODERS_MAX: usize = 16;
const LIT_CODER_SIZE: usize = 0x300;
const DIST_STATES: usize = 4;
const DIST_SLOTS: usize = 64;
const DIST_MODEL_START: usize = 4;
const DIST_MODEL_END: usize = 14;
const FULL_DISTANCES: usize = 1 << (DIST_MODEL_END / 2);
const ALIGN_SIZE: usize = 16;
const MATCH_LEN_MIN: usize = 2;
const LEN_SYMBOLS: usize = 8;

/// Decode a complete .xz file (one or more concatenated streams with
/// optional stream padding) to its uncompressed bytes.
pub fn decompress(data: &[u8]) -> Result<Vec<u8>, String> {
    decompress_with_limit(data, MAX_XZ_OUTPUT_BYTES)
}

fn decompress_with_limit(data: &[u8], max_output_bytes: u64) -> Result<Vec<u8>, String> {
    if data.is_empty() {
        return Err("empty xz input".to_string());
    }
    let mut out = Vec::new();
    let mut pos = 0usize;
    let mut decoded_any = false;
    while pos < data.len() {
        if decoded_any {
            // Stream Padding: a four-byte-aligned run of null bytes may
            // separate (or follow) streams.
            let pad_start = pos;
            while data.get(pos) == Some(&0) {
                pos = pos
                    .checked_add(1)
                    .ok_or_else(|| "xz stream padding offset overflow".to_string())?;
            }
            let pad = pos.saturating_sub(pad_start);
            if !pad.is_multiple_of(4) {
                return Err("xz stream padding is not a multiple of four bytes".to_string());
            }
            if pos == data.len() {
                break;
            }
        }
        pos = decode_stream(data, pos, &mut out, max_output_bytes)?;
        decoded_any = true;
    }
    Ok(out)
}

/// Sizes recorded per block while decoding, checked against the index.
struct BlockRecord {
    unpadded: u64,
    uncompressed: u64,
}

fn decode_stream(
    data: &[u8],
    start: usize,
    out: &mut Vec<u8>,
    max_output_bytes: u64,
) -> Result<usize, String> {
    let header = range(data, start, 12).map_err(|_| "truncated xz stream header".to_string())?;
    if header.get(..6) != Some(&XZ_MAGIC[..]) {
        return Err("bad xz magic".to_string());
    }
    let flag0 = byte(header, 6)?;
    let flag1 = byte(header, 7)?;
    if flag0 != 0 || flag1 & 0xf0 != 0 {
        return Err(format!(
            "xz stream flags reserved bits set: 0x{flag0:02x}{flag1:02x}"
        ));
    }
    let check_id = flag1;
    // Validate the check type up front so unsupported ids fail before any
    // block work.
    let _ = check_size(check_id)?;
    let flag_bytes = range(header, 6, 2)?;
    let got_crc = crc32(flag_bytes);
    let want_crc = u32_le(header, 8)?;
    if got_crc != want_crc {
        return Err(format!(
            "xz stream header CRC32 mismatch: got {got_crc:08x}, want {want_crc:08x}"
        ));
    }

    let mut pos = start
        .checked_add(12)
        .ok_or_else(|| "xz stream offset overflow".to_string())?;
    let mut records: Vec<BlockRecord> = Vec::new();
    loop {
        let first = byte(data, pos).map_err(|_| "truncated xz stream (missing index)".to_string())?;
        if first == 0x00 {
            break; // index indicator
        }
        let (next, record) = decode_block(data, pos, check_id, out, max_output_bytes)?;
        pos = next;
        records.push(record);
    }

    // Index: indicator, record count, per-block (unpadded, uncompressed)
    // sizes, padding to a multiple of four, CRC32.
    let index_start = pos;
    let mut ipos = pos
        .checked_add(1)
        .ok_or_else(|| "xz index offset overflow".to_string())?;
    let count = read_vli(data, &mut ipos)?;
    if count != records.len() as u64 {
        return Err(format!(
            "xz index record count {count} does not match {} decoded blocks",
            records.len()
        ));
    }
    for record in &records {
        let unpadded = read_vli(data, &mut ipos)?;
        let uncompressed = read_vli(data, &mut ipos)?;
        if unpadded != record.unpadded {
            return Err(format!(
                "xz index unpadded size mismatch: index {unpadded}, block {}",
                record.unpadded
            ));
        }
        if uncompressed != record.uncompressed {
            return Err(format!(
                "xz index uncompressed size mismatch: index {uncompressed}, block {}",
                record.uncompressed
            ));
        }
    }
    while ipos.saturating_sub(index_start) % 4 != 0 {
        if byte(data, ipos)? != 0 {
            return Err("xz index padding is not zero".to_string());
        }
        ipos = ipos
            .checked_add(1)
            .ok_or_else(|| "xz index offset overflow".to_string())?;
    }
    let index_body = data
        .get(index_start..ipos)
        .ok_or_else(|| "xz index out of bounds".to_string())?;
    let got_icrc = crc32(index_body);
    let want_icrc = u32_le(data, ipos)?;
    if got_icrc != want_icrc {
        return Err(format!(
            "xz index CRC32 mismatch: got {got_icrc:08x}, want {want_icrc:08x}"
        ));
    }
    ipos = ipos
        .checked_add(4)
        .ok_or_else(|| "xz index offset overflow".to_string())?;
    let index_size = ipos.saturating_sub(index_start) as u64;

    // Stream footer: CRC32, backward size, stream flags, footer magic.
    let footer = range(data, ipos, 12).map_err(|_| "truncated xz stream footer".to_string())?;
    let want_fcrc = u32_le(footer, 0)?;
    let got_fcrc = crc32(range(footer, 4, 6)?);
    if got_fcrc != want_fcrc {
        return Err(format!(
            "xz stream footer CRC32 mismatch: got {got_fcrc:08x}, want {want_fcrc:08x}"
        ));
    }
    let backward = (u64::from(u32_le(footer, 4)?) + 1) * 4;
    if backward != index_size {
        return Err(format!(
            "xz backward size {backward} does not match index size {index_size}"
        ));
    }
    if byte(footer, 8)? != 0 || byte(footer, 9)? != check_id {
        return Err("xz stream footer flags do not match the stream header".to_string());
    }
    if footer.get(10..12) != Some(&XZ_FOOTER_MAGIC[..]) {
        return Err("bad xz footer magic".to_string());
    }
    ipos
        .checked_add(12)
        .ok_or_else(|| "xz stream offset overflow".to_string())
}

fn decode_block(
    data: &[u8],
    start: usize,
    check_id: u8,
    out: &mut Vec<u8>,
    max_output_bytes: u64,
) -> Result<(usize, BlockRecord), String> {
    let size_byte = byte(data, start)?; // caller guarantees != 0
    let header_size = (usize::from(size_byte) + 1) * 4;
    let header =
        range(data, start, header_size).map_err(|_| "truncated xz block header".to_string())?;
    let body_len = header_size.saturating_sub(4);
    let body = header
        .get(..body_len)
        .ok_or_else(|| "xz block header out of bounds".to_string())?;
    let got_crc = crc32(body);
    let want_crc = u32_le(header, body_len)?;
    if got_crc != want_crc {
        return Err(format!(
            "xz block header CRC32 mismatch: got {got_crc:08x}, want {want_crc:08x}"
        ));
    }

    let flags = byte(body, 1)?;
    if flags & 0x3c != 0 {
        return Err(format!(
            "xz block header reserved flag bits set: 0x{flags:02x}"
        ));
    }
    let filter_count = usize::from(flags & 0x03) + 1;
    let mut hpos = 2usize;
    let declared_comp = if flags & 0x40 != 0 {
        Some(read_vli(body, &mut hpos)?)
    } else {
        None
    };
    let declared_unc = if flags & 0x80 != 0 {
        Some(read_vli(body, &mut hpos)?)
    } else {
        None
    };

    let mut dict_size = 0u32;
    for _ in 0..filter_count {
        let id = read_vli(body, &mut hpos)?;
        let props_len = usize::try_from(read_vli(body, &mut hpos)?)
            .map_err(|_| "xz filter properties size did not fit usize".to_string())?;
        let props = range(body, hpos, props_len)
            .map_err(|_| "truncated xz filter properties".to_string())?;
        hpos = hpos
            .checked_add(props_len)
            .ok_or_else(|| "xz block header offset overflow".to_string())?;
        if id != 0x21 {
            return Err(format!(
                "xz block uses unsupported filter 0x{id:02x} ({}); only LZMA2 (0x21) is supported",
                filter_name(id)
            ));
        }
        if filter_count != 1 {
            return Err(format!(
                "xz block declares {filter_count} filters; only a single LZMA2 filter is supported"
            ));
        }
        if props_len != 1 {
            return Err("LZMA2 filter properties must be exactly one byte".to_string());
        }
        let bits = byte(props, 0)?;
        if bits > 40 {
            return Err(format!("invalid LZMA2 dictionary size byte 0x{bits:02x}"));
        }
        let declared: u64 = if bits == 40 {
            u64::from(u32::MAX)
        } else {
            (2u64 | u64::from(bits & 1)) << (u32::from(bits) / 2 + 11)
        };
        if declared > MAX_DICT_BYTES {
            return Err(format!(
                "LZMA2 dictionary size {declared} exceeds {MAX_DICT_BYTES} byte limit"
            ));
        }
        dict_size = u32::try_from(declared)
            .map_err(|_| "LZMA2 dictionary size did not fit u32".to_string())?;
    }
    for pad in body.get(hpos..).unwrap_or(&[]) {
        if *pad != 0 {
            return Err("xz block header padding is not zero".to_string());
        }
    }

    let data_start = start
        .checked_add(header_size)
        .ok_or_else(|| "xz block offset overflow".to_string())?;
    let out_start = out.len();
    let comp_len = lzma2_decode(data, data_start, dict_size, out, max_output_bytes)?;
    let produced = out.len().saturating_sub(out_start) as u64;
    if let Some(declared) = declared_comp {
        if declared != comp_len as u64 {
            return Err(format!(
                "xz block compressed size mismatch: header {declared}, decoded {comp_len}"
            ));
        }
    }
    if let Some(declared) = declared_unc {
        if declared != produced {
            return Err(format!(
                "xz block uncompressed size mismatch: header {declared}, decoded {produced}"
            ));
        }
    }

    // Block padding: null bytes until the compressed data size is a
    // multiple of four (the header size already is one).
    let mut pos = data_start
        .checked_add(comp_len)
        .ok_or_else(|| "xz block offset overflow".to_string())?;
    let mut padded = comp_len;
    while padded % 4 != 0 {
        if byte(data, pos).map_err(|_| "truncated xz block padding".to_string())? != 0 {
            return Err("xz block padding is not zero".to_string());
        }
        pos = pos
            .checked_add(1)
            .ok_or_else(|| "xz block offset overflow".to_string())?;
        padded += 1;
    }

    let check_len = check_size(check_id)?;
    let stored = range(data, pos, check_len).map_err(|_| "truncated xz block check".to_string())?;
    let content = out
        .get(out_start..)
        .ok_or_else(|| "xz block output out of bounds".to_string())?;
    verify_check(check_id, content, stored)?;
    pos = pos
        .checked_add(check_len)
        .ok_or_else(|| "xz block offset overflow".to_string())?;

    let unpadded = header_size as u64 + comp_len as u64 + check_len as u64;
    Ok((
        pos,
        BlockRecord {
            unpadded,
            uncompressed: produced,
        },
    ))
}

fn filter_name(id: u64) -> &'static str {
    match id {
        0x03 => "delta",
        0x04 => "x86 BCJ",
        0x05 => "PowerPC BCJ",
        0x06 => "IA-64 BCJ",
        0x07 => "ARM BCJ",
        0x08 => "ARM-Thumb BCJ",
        0x09 => "SPARC BCJ",
        0x0a => "ARM64 BCJ",
        0x0b => "RISC-V BCJ",
        _ => "unknown",
    }
}

fn check_size(check_id: u8) -> Result<usize, String> {
    match check_id {
        0x00 => Ok(0),
        0x01 => Ok(4),
        0x04 => Ok(8),
        0x0a => Ok(32),
        other => Err(format!("unsupported xz check type 0x{other:02x}")),
    }
}

fn verify_check(check_id: u8, content: &[u8], stored: &[u8]) -> Result<(), String> {
    match check_id {
        0x00 => Ok(()),
        0x01 => {
            let got = crc32(content);
            let want = u32_le(stored, 0)?;
            if got != want {
                return Err(format!(
                    "xz block CRC32 mismatch: got {got:08x}, want {want:08x}"
                ));
            }
            Ok(())
        }
        0x04 => {
            let got = crc64(content);
            let want = u64_le(stored, 0)?;
            if got != want {
                return Err(format!(
                    "xz block CRC64 mismatch: got {got:016x}, want {want:016x}"
                ));
            }
            Ok(())
        }
        0x0a => {
            let mut hasher = Sha256::new();
            hasher.update(content);
            if hasher.finalize() != *stored {
                return Err("xz block SHA-256 mismatch".to_string());
            }
            Ok(())
        }
        other => Err(format!("unsupported xz check type 0x{other:02x}")),
    }
}

/// Decode one LZMA2 compressed-data field (a chunk sequence ending with a
/// 0x00 control byte) starting at `start`, appending to `out`. Returns the
/// number of compressed bytes consumed, terminator included.
fn lzma2_decode(
    data: &[u8],
    start: usize,
    dict_size: u32,
    out: &mut Vec<u8>,
    max_output_bytes: u64,
) -> Result<usize, String> {
    let mut st = LzmaState::new();
    let mut need_dict_reset = true;
    let mut need_props = true;
    let mut dict_base = out.len();
    let mut pos = start;
    loop {
        let control = byte(data, pos).map_err(|_| "truncated LZMA2 stream".to_string())?;
        pos = pos
            .checked_add(1)
            .ok_or_else(|| "LZMA2 offset overflow".to_string())?;
        if control == 0x00 {
            return Ok(pos.saturating_sub(start));
        }
        if control >= 0xe0 || control == 0x01 {
            // Dictionary reset. The next LZMA chunk must carry new
            // properties (0xe0 does so itself).
            need_props = true;
            need_dict_reset = false;
            dict_base = out.len();
        } else if need_dict_reset {
            return Err("LZMA2 stream does not start with a dictionary reset".to_string());
        }
        if control >= 0x80 {
            // LZMA chunk: 5 unpacked-size high bits in the control byte,
            // then unpacked-size low bits, compressed size, and (for
            // control >= 0xc0) a properties byte.
            let unpacked = (usize::from(control & 0x1f) << 16)
                + usize::from(u16_be(data, pos)?)
                + 1;
            let comp = usize::from(u16_be(
                data,
                pos.checked_add(2)
                    .ok_or_else(|| "LZMA2 offset overflow".to_string())?,
            )?) + 1;
            pos = pos
                .checked_add(4)
                .ok_or_else(|| "LZMA2 offset overflow".to_string())?;
            if control >= 0xc0 {
                st.set_props(byte(data, pos)?)?;
                need_props = false;
                pos = pos
                    .checked_add(1)
                    .ok_or_else(|| "LZMA2 offset overflow".to_string())?;
            } else if need_props {
                return Err("LZMA2 chunk reuses properties before any were set".to_string());
            } else if control >= 0xa0 {
                st.reset();
            }
            let chunk = range(data, pos, comp)
                .map_err(|_| "truncated LZMA2 compressed chunk".to_string())?;
            if out.len() as u64 + unpacked as u64 > max_output_bytes {
                return Err(format!("xz output exceeds {max_output_bytes} byte limit"));
            }
            lzma_run_chunk(chunk, &mut st, out, dict_base, dict_size, unpacked)?;
            pos = pos
                .checked_add(comp)
                .ok_or_else(|| "LZMA2 offset overflow".to_string())?;
        } else if control <= 0x02 {
            // Uncompressed chunk (0x01 with dictionary reset, 0x02 without).
            let size = usize::from(u16_be(data, pos)?) + 1;
            pos = pos
                .checked_add(2)
                .ok_or_else(|| "LZMA2 offset overflow".to_string())?;
            let chunk = range(data, pos, size)
                .map_err(|_| "truncated LZMA2 uncompressed chunk".to_string())?;
            if out.len() as u64 + size as u64 > max_output_bytes {
                return Err(format!("xz output exceeds {max_output_bytes} byte limit"));
            }
            out.extend_from_slice(chunk);
            pos = pos
                .checked_add(size)
                .ok_or_else(|| "LZMA2 offset overflow".to_string())?;
        } else {
            return Err(format!("invalid LZMA2 control byte 0x{control:02x}"));
        }
    }
}

/// LZMA probability model + decoder state that persists across LZMA2
/// chunks (the range coder itself is re-initialized per chunk).
struct LzmaState {
    lc: u32,
    lit_pos_mask: usize,
    pos_mask: usize,
    state: usize,
    rep0: u32,
    rep1: u32,
    rep2: u32,
    rep3: u32,
    is_match: [u16; STATES * POS_STATES_MAX],
    is_rep: [u16; STATES],
    is_rep0: [u16; STATES],
    is_rep1: [u16; STATES],
    is_rep2: [u16; STATES],
    is_rep0_long: [u16; STATES * POS_STATES_MAX],
    dist_slot: [u16; DIST_STATES * DIST_SLOTS],
    dist_special: [u16; FULL_DISTANCES - DIST_MODEL_END],
    dist_align: [u16; ALIGN_SIZE],
    match_len: LenDecoder,
    rep_len: LenDecoder,
    // Allocated once for the maximum lc+lp (16 coders); reset refills it.
    literal: Vec<u16>,
}

struct LenDecoder {
    choice: u16,
    choice2: u16,
    low: [u16; POS_STATES_MAX * LEN_SYMBOLS],
    mid: [u16; POS_STATES_MAX * LEN_SYMBOLS],
    high: [u16; 256],
}

impl LenDecoder {
    fn new() -> LenDecoder {
        LenDecoder {
            choice: PROB_INIT,
            choice2: PROB_INIT,
            low: [PROB_INIT; POS_STATES_MAX * LEN_SYMBOLS],
            mid: [PROB_INIT; POS_STATES_MAX * LEN_SYMBOLS],
            high: [PROB_INIT; 256],
        }
    }

    fn reset(&mut self) {
        self.choice = PROB_INIT;
        self.choice2 = PROB_INIT;
        self.low.fill(PROB_INIT);
        self.mid.fill(PROB_INIT);
        self.high.fill(PROB_INIT);
    }
}

impl LzmaState {
    fn new() -> LzmaState {
        LzmaState {
            lc: 0,
            lit_pos_mask: 0,
            pos_mask: 0,
            state: 0,
            rep0: 0,
            rep1: 0,
            rep2: 0,
            rep3: 0,
            is_match: [PROB_INIT; STATES * POS_STATES_MAX],
            is_rep: [PROB_INIT; STATES],
            is_rep0: [PROB_INIT; STATES],
            is_rep1: [PROB_INIT; STATES],
            is_rep2: [PROB_INIT; STATES],
            is_rep0_long: [PROB_INIT; STATES * POS_STATES_MAX],
            dist_slot: [PROB_INIT; DIST_STATES * DIST_SLOTS],
            dist_special: [PROB_INIT; FULL_DISTANCES - DIST_MODEL_END],
            dist_align: [PROB_INIT; ALIGN_SIZE],
            match_len: LenDecoder::new(),
            rep_len: LenDecoder::new(),
            literal: vec![PROB_INIT; LIT_CODERS_MAX * LIT_CODER_SIZE],
        }
    }

    /// Decode and apply an LZMA properties byte (lc/lp/pb), then reset.
    fn set_props(&mut self, props: u8) -> Result<(), String> {
        if props > (4 * 5 + 4) * 9 + 8 {
            return Err(format!("invalid LZMA properties byte 0x{props:02x}"));
        }
        let mut p = u32::from(props);
        let mut pb = 0u32;
        while p >= 9 * 5 {
            p -= 9 * 5;
            pb += 1;
        }
        let mut lp = 0u32;
        while p >= 9 {
            p -= 9;
            lp += 1;
        }
        let lc = p;
        if lc + lp > 4 {
            return Err(format!("unsupported LZMA properties: lc {lc} + lp {lp} > 4"));
        }
        self.pos_mask = (1usize << pb) - 1;
        self.lit_pos_mask = (1usize << lp) - 1;
        self.lc = lc;
        self.reset();
        Ok(())
    }

    /// Reset decoder state and all probabilities (LZMA2 state reset).
    fn reset(&mut self) {
        self.state = 0;
        self.rep0 = 0;
        self.rep1 = 0;
        self.rep2 = 0;
        self.rep3 = 0;
        self.is_match.fill(PROB_INIT);
        self.is_rep.fill(PROB_INIT);
        self.is_rep0.fill(PROB_INIT);
        self.is_rep1.fill(PROB_INIT);
        self.is_rep2.fill(PROB_INIT);
        self.is_rep0_long.fill(PROB_INIT);
        self.dist_slot.fill(PROB_INIT);
        self.dist_special.fill(PROB_INIT);
        self.dist_align.fill(PROB_INIT);
        self.match_len.reset();
        self.rep_len.reset();
        self.literal.fill(PROB_INIT);
    }
}

/// Decode one LZMA chunk (`unpacked` output bytes from `chunk`). The output
/// buffer doubles as the dictionary: `dict_base` marks the last dictionary
/// reset, so match distances may not reach behind it.
fn lzma_run_chunk(
    chunk: &[u8],
    st: &mut LzmaState,
    out: &mut Vec<u8>,
    dict_base: usize,
    dict_size: u32,
    unpacked: usize,
) -> Result<(), String> {
    let mut rc = RangeDecoder::new(chunk)?;
    let chunk_end = out
        .len()
        .checked_add(unpacked)
        .ok_or_else(|| "LZMA chunk length overflow".to_string())?;
    while out.len() < chunk_end {
        // dict_base <= out.len() by construction (it is a saved out.len()).
        let pos_abs = out.len().saturating_sub(dict_base);
        let pos_state = pos_abs & st.pos_mask;
        let match_idx = st.state * POS_STATES_MAX + pos_state;
        if rc.bit(prob_at(&mut st.is_match, match_idx)?)? == 0 {
            decode_literal(&mut rc, st, out, pos_abs)?;
            continue;
        }
        let len;
        let dist_m1;
        if rc.bit(prob_at(&mut st.is_rep, st.state)?)? == 1 {
            // Repeated match: distance is one of the last four.
            if rc.bit(prob_at(&mut st.is_rep0, st.state)?)? == 0 {
                if rc.bit(prob_at(&mut st.is_rep0_long, match_idx)?)? == 0 {
                    // Short rep: a single byte at distance rep0.
                    st.state = if st.state < 7 { 9 } else { 11 };
                    copy_match(out, st.rep0, 1, dict_base, dict_size, chunk_end)?;
                    continue;
                }
                dist_m1 = st.rep0;
            } else {
                let picked;
                if rc.bit(prob_at(&mut st.is_rep1, st.state)?)? == 0 {
                    picked = st.rep1;
                } else {
                    if rc.bit(prob_at(&mut st.is_rep2, st.state)?)? == 0 {
                        picked = st.rep2;
                    } else {
                        picked = st.rep3;
                        st.rep3 = st.rep2;
                    }
                    st.rep2 = st.rep1;
                }
                st.rep1 = st.rep0;
                st.rep0 = picked;
                dist_m1 = picked;
            }
            st.state = if st.state < 7 { 8 } else { 11 };
            len = decode_len(&mut rc, &mut st.rep_len, pos_state)?;
        } else {
            // New match: decode length, then the distance slot machinery.
            st.rep3 = st.rep2;
            st.rep2 = st.rep1;
            st.rep1 = st.rep0;
            len = decode_len(&mut rc, &mut st.match_len, pos_state)?;
            st.rep0 = decode_distance(&mut rc, st, len)?;
            st.state = if st.state < 7 { 7 } else { 10 };
            dist_m1 = st.rep0;
        }
        copy_match(out, dist_m1, len, dict_base, dict_size, chunk_end)?;
    }
    // One trailing normalization keeps byte consumption in lockstep with
    // the encoder, so a valid chunk ends exactly at its compressed size
    // with the range coder drained (mirrors the reference decoder's
    // end-of-chunk checks).
    rc.normalize()?;
    if rc.pos != chunk.len() {
        return Err("LZMA chunk did not consume its declared compressed size".to_string());
    }
    if rc.code != 0 {
        return Err("LZMA range coder not cleanly finished at chunk end".to_string());
    }
    Ok(())
}

fn decode_literal(
    rc: &mut RangeDecoder<'_>,
    st: &mut LzmaState,
    out: &mut Vec<u8>,
    pos_abs: usize,
) -> Result<(), String> {
    let prev = if pos_abs == 0 {
        0u32
    } else {
        out.last().map(|b| u32::from(*b)).unwrap_or(0)
    };
    let lit_idx =
        ((pos_abs & st.lit_pos_mask) << st.lc) + ((prev >> (8 - st.lc)) as usize);
    let base = LIT_CODER_SIZE * lit_idx;
    let probs = st
        .literal
        .get_mut(base..base + LIT_CODER_SIZE)
        .ok_or_else(probs_oob)?;
    let mut symbol = 1usize;
    if st.state < 7 {
        while symbol < 0x100 {
            let bit = rc.bit(prob_at(probs, symbol)?)?;
            symbol = (symbol << 1) | (bit as usize);
        }
    } else {
        // Matched literal: fold in the byte at distance rep0 until the
        // decoded bits diverge from it.
        let dist = st.rep0 as usize;
        if dist >= pos_abs {
            return Err("LZMA matched-literal distance out of range".to_string());
        }
        let src = out.len() - 1 - dist;
        let mut match_byte = u32::from(
            *out.get(src)
                .ok_or_else(|| "LZMA matched-literal source out of bounds".to_string())?,
        ) << 1;
        let mut offset = 0x100u32;
        while symbol < 0x100 {
            let match_bit = match_byte & offset;
            match_byte <<= 1;
            let index = (offset + match_bit) as usize + symbol;
            let bit = rc.bit(prob_at(probs, index)?)?;
            if bit == 1 {
                symbol = (symbol << 1) | 1;
                offset &= match_bit;
            } else {
                symbol <<= 1;
                offset &= !match_bit;
            }
        }
    }
    out.push((symbol & 0xff) as u8);
    st.state = if st.state < 4 {
        0
    } else if st.state < 10 {
        st.state - 3
    } else {
        st.state - 6
    };
    Ok(())
}

fn decode_len(
    rc: &mut RangeDecoder<'_>,
    ld: &mut LenDecoder,
    pos_state: usize,
) -> Result<usize, String> {
    if rc.bit(&mut ld.choice)? == 0 {
        let base = pos_state * LEN_SYMBOLS;
        let probs = ld
            .low
            .get_mut(base..base + LEN_SYMBOLS)
            .ok_or_else(probs_oob)?;
        Ok(MATCH_LEN_MIN + bittree(rc, probs, 3)? as usize)
    } else if rc.bit(&mut ld.choice2)? == 0 {
        let base = pos_state * LEN_SYMBOLS;
        let probs = ld
            .mid
            .get_mut(base..base + LEN_SYMBOLS)
            .ok_or_else(probs_oob)?;
        Ok(MATCH_LEN_MIN + LEN_SYMBOLS + bittree(rc, probs, 3)? as usize)
    } else {
        Ok(MATCH_LEN_MIN + 2 * LEN_SYMBOLS + bittree(rc, &mut ld.high, 8)? as usize)
    }
}

/// Decode a match distance (zero-based, i.e. real distance minus one).
fn decode_distance(
    rc: &mut RangeDecoder<'_>,
    st: &mut LzmaState,
    len: usize,
) -> Result<u32, String> {
    let dist_state = (len - MATCH_LEN_MIN).min(DIST_STATES - 1);
    let slot_base = dist_state * DIST_SLOTS;
    let slot_probs = st
        .dist_slot
        .get_mut(slot_base..slot_base + DIST_SLOTS)
        .ok_or_else(probs_oob)?;
    let slot = bittree(rc, slot_probs, 6)? as usize;
    if slot < DIST_MODEL_START {
        return Ok(slot as u32);
    }
    let nbits = (slot as u32 >> 1) - 1;
    let base_val = 2u32 | (slot as u32 & 1);
    if slot < DIST_MODEL_END {
        let dist = base_val << nbits;
        // The model's probability base is dist - slot - 1, but the reverse
        // bit tree's symbol index starts at 1, so slice from dist - slot
        // (never negative: dist >= slot for every slot >= 4) and fold the
        // -1 into the tree's indexing instead.
        let offset = (dist as usize).saturating_sub(slot);
        let probs = st
            .dist_special
            .get_mut(offset..)
            .ok_or_else(probs_oob)?;
        Ok(dist + bittree_reverse_from_1(rc, probs, nbits)?)
    } else {
        let dist = rc.direct(base_val, nbits - 4)? << 4;
        Ok(dist + bittree_reverse(rc, &mut st.dist_align, 4)?)
    }
}

/// Copy `len` bytes from `dist_m1 + 1` back. The output buffer is the
/// dictionary; distances may not reach behind the last dictionary reset or
/// past the declared dictionary size, and the copy may not overrun the
/// chunk's uncompressed size. This also rejects the LZMA end-of-stream
/// marker (distance 0xffffffff), which LZMA2 chunks never contain.
fn copy_match(
    out: &mut Vec<u8>,
    dist_m1: u32,
    len: usize,
    dict_base: usize,
    dict_size: u32,
    chunk_end: usize,
) -> Result<(), String> {
    let avail = out.len().saturating_sub(dict_base);
    if u64::from(dist_m1) >= avail as u64 || u64::from(dist_m1) >= u64::from(dict_size) {
        return Err(format!(
            "LZMA match distance {} exceeds dictionary",
            u64::from(dist_m1) + 1
        ));
    }
    let new_len = out
        .len()
        .checked_add(len)
        .ok_or_else(|| "LZMA output length overflow".to_string())?;
    if new_len > chunk_end {
        return Err("LZMA match overruns the chunk's uncompressed size".to_string());
    }
    let dist = dist_m1 as usize + 1;
    // dist <= avail <= out.len(), so src is in bounds and the
    // extend_from_within ranges below never leave the buffer.
    let src = out.len() - dist;
    if dist >= len {
        out.extend_from_within(src..src + len);
        return Ok(());
    }
    // Overlapping copy: the appended bytes repeat the last `dist` bytes.
    // Every round copies a whole number of periods (the window is always a
    // multiple of `dist` until the final partial round), doubling the
    // window instead of copying byte by byte.
    let mut remaining = len;
    while remaining > 0 {
        let n = (out.len() - src).min(remaining);
        out.extend_from_within(src..src + n);
        remaining -= n;
    }
    Ok(())
}

#[inline]
fn prob_at(probs: &mut [u16], index: usize) -> Result<&mut u16, String> {
    probs.get_mut(index).ok_or_else(probs_oob)
}

fn probs_oob() -> String {
    "LZMA probability index out of bounds".to_string()
}

/// Decode a bit tree from the most significant bit; returns the value in
/// `0..2^nbits`.
#[inline]
fn bittree(rc: &mut RangeDecoder<'_>, probs: &mut [u16], nbits: u32) -> Result<u32, String> {
    let limit = 1u32 << nbits;
    let mut symbol = 1u32;
    while symbol < limit {
        let bit = rc.bit(prob_at(probs, symbol as usize)?)?;
        symbol = (symbol << 1) | bit;
    }
    Ok(symbol - limit)
}

/// Decode a bit tree from the least significant bit.
#[inline]
fn bittree_reverse(
    rc: &mut RangeDecoder<'_>,
    probs: &mut [u16],
    nbits: u32,
) -> Result<u32, String> {
    let mut symbol = 1u32;
    let mut value = 0u32;
    for i in 0..nbits {
        let bit = rc.bit(prob_at(probs, symbol as usize)?)?;
        symbol = (symbol << 1) | bit;
        value |= bit << i;
    }
    Ok(value)
}

/// `bittree_reverse` with the probability base shifted down by one: the
/// caller's slice starts at model index 1 (dist_special's base can be one
/// short of the slice start, which a slice cannot express).
#[inline]
fn bittree_reverse_from_1(
    rc: &mut RangeDecoder<'_>,
    probs: &mut [u16],
    nbits: u32,
) -> Result<u32, String> {
    let mut symbol = 1u32;
    let mut value = 0u32;
    for i in 0..nbits {
        let bit = rc.bit(prob_at(probs, (symbol as usize) - 1)?)?;
        symbol = (symbol << 1) | bit;
        value |= bit << i;
    }
    Ok(value)
}

/// The LZMA binary range decoder over one chunk's compressed bytes.
struct RangeDecoder<'a> {
    input: &'a [u8],
    pos: usize,
    range: u32,
    code: u32,
}

impl<'a> RangeDecoder<'a> {
    fn new(input: &'a [u8]) -> Result<RangeDecoder<'a>, String> {
        if input.len() < 5 {
            return Err("LZMA chunk shorter than range-coder init".to_string());
        }
        if byte(input, 0)? != 0 {
            return Err("LZMA chunk does not start with a zero byte".to_string());
        }
        let mut code = 0u32;
        for i in 1..5 {
            code = (code << 8) | u32::from(byte(input, i)?);
        }
        Ok(RangeDecoder {
            input,
            pos: 5,
            range: u32::MAX,
            code,
        })
    }

    #[inline]
    fn normalize(&mut self) -> Result<(), String> {
        if self.range < RC_TOP {
            let next = *self
                .input
                .get(self.pos)
                .ok_or_else(|| "LZMA chunk input exhausted mid-symbol".to_string())?;
            // pos < input.len() here, so the increment cannot overflow.
            self.pos += 1;
            self.range <<= 8;
            self.code = (self.code << 8) | u32::from(next);
        }
        Ok(())
    }

    #[inline]
    fn bit(&mut self, prob: &mut u16) -> Result<u32, String> {
        self.normalize()?;
        let bound = (self.range >> RC_MODEL_TOTAL_BITS) * u32::from(*prob);
        if self.code < bound {
            self.range = bound;
            *prob += (RC_MODEL_TOTAL - *prob) >> RC_MOVE_BITS;
            Ok(0)
        } else {
            self.range -= bound;
            self.code -= bound;
            *prob -= *prob >> RC_MOVE_BITS;
            Ok(1)
        }
    }

    /// Decode `nbits` fifty-fifty bits, appending them to `start`.
    fn direct(&mut self, start: u32, nbits: u32) -> Result<u32, String> {
        let mut value = start;
        for _ in 0..nbits {
            self.normalize()?;
            self.range >>= 1;
            self.code = self.code.wrapping_sub(self.range);
            let mask = 0u32.wrapping_sub(self.code >> 31);
            self.code = self.code.wrapping_add(self.range & mask);
            value = (value << 1).wrapping_add(mask.wrapping_add(1));
        }
        Ok(value)
    }
}

/// Read an xz variable-length integer (little-endian base-128, at most nine
/// bytes / 63 bits, minimal encoding required).
fn read_vli(data: &[u8], pos: &mut usize) -> Result<u64, String> {
    let mut value = 0u64;
    let mut shift = 0u32;
    loop {
        let b = byte(data, *pos).map_err(|_| "truncated xz varint".to_string())?;
        *pos = pos
            .checked_add(1)
            .ok_or_else(|| "xz varint offset overflow".to_string())?;
        value |= u64::from(b & 0x7f) << shift;
        if b & 0x80 == 0 {
            if b == 0 && shift != 0 {
                return Err("non-minimal xz varint encoding".to_string());
            }
            return Ok(value);
        }
        shift += 7;
        if shift == 63 {
            return Err("xz varint too long".to_string());
        }
    }
}

fn byte(input: &[u8], pos: usize) -> Result<u8, String> {
    input
        .get(pos)
        .copied()
        .ok_or_else(|| "unexpected EOF".to_string())
}

fn range(input: &[u8], start: usize, len: usize) -> Result<&[u8], String> {
    let end = start
        .checked_add(len)
        .ok_or_else(|| "range offset overflow".to_string())?;
    input
        .get(start..end)
        .ok_or_else(|| "unexpected EOF".to_string())
}

fn u16_be(input: &[u8], pos: usize) -> Result<u16, String> {
    let bytes = range(input, pos, 2)?;
    Ok((u16::from(byte(bytes, 0)?) << 8) | u16::from(byte(bytes, 1)?))
}

fn u32_le(input: &[u8], pos: usize) -> Result<u32, String> {
    let bytes = range(input, pos, 4)?;
    Ok(u32::from(byte(bytes, 0)?)
        | (u32::from(byte(bytes, 1)?) << 8)
        | (u32::from(byte(bytes, 2)?) << 16)
        | (u32::from(byte(bytes, 3)?) << 24))
}

fn u64_le(input: &[u8], pos: usize) -> Result<u64, String> {
    let lo = u64::from(u32_le(input, pos)?);
    let hi = u64::from(u32_le(
        input,
        pos.checked_add(4)
            .ok_or_else(|| "u64 offset overflow".to_string())?,
    )?);
    Ok(lo | (hi << 32))
}

fn crc32(input: &[u8]) -> u32 {
    let table = crc32_table();
    let mut crc = 0xffff_ffffu32;
    for b in input {
        let idx = ((crc ^ u32::from(*b)) & 0xff) as usize;
        let table_value = table.get(idx).copied().unwrap_or(0);
        crc = (crc >> 8) ^ table_value;
    }
    !crc
}

fn crc32_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    for i in 0..256usize {
        let mut crc = u32::try_from(i).unwrap_or(0);
        for _ in 0..8 {
            if crc & 1 == 0 {
                crc >>= 1;
            } else {
                crc = (crc >> 1) ^ 0xedb8_8320;
            }
        }
        if let Some(slot) = table.get_mut(i) {
            *slot = crc;
        }
    }
    table
}

/// CRC64/ECMA-182 as used by xz (reflected, init/xorout all-ones).
fn crc64(input: &[u8]) -> u64 {
    let table = crc64_table();
    let mut crc = u64::MAX;
    for b in input {
        let idx = ((crc ^ u64::from(*b)) & 0xff) as usize;
        let table_value = table.get(idx).copied().unwrap_or(0);
        crc = (crc >> 8) ^ table_value;
    }
    !crc
}

fn crc64_table() -> [u64; 256] {
    let mut table = [0u64; 256];
    for i in 0..256usize {
        let mut crc = u64::try_from(i).unwrap_or(0);
        for _ in 0..8 {
            if crc & 1 == 0 {
                crc >>= 1;
            } else {
                crc = (crc >> 1) ^ 0xc96c_5795_d787_0f42;
            }
        }
        if let Some(slot) = table.get_mut(i) {
            *slot = crc;
        }
    }
    table
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    // Vectors produced once with XZ Utils 5.4.5 (see each test for the
    // exact invocation) and embedded so the tests run offline.

    // printf 'hello world\n' | xz -9 --check=crc64
    const HELLO_CRC64_XZ: [u8; 68] = [
        0xfd, 0x37, 0x7a, 0x58, 0x5a, 0x00, 0x00, 0x04, 0xe6, 0xd6, 0xb4, 0x46,
        0x02, 0x00, 0x21, 0x01, 0x1c, 0x00, 0x00, 0x00, 0x10, 0xcf, 0x58, 0xcc,
        0x01, 0x00, 0x0b, 0x68, 0x65, 0x6c, 0x6c, 0x6f, 0x20, 0x77, 0x6f, 0x72,
        0x6c, 0x64, 0x0a, 0x00, 0xa1, 0xf2, 0xff, 0xc4, 0x6a, 0x7f, 0xbf, 0xcf,
        0x00, 0x01, 0x24, 0x0c, 0xa6, 0x18, 0xd8, 0xd8, 0x1f, 0xb6, 0xf3, 0x7d,
        0x01, 0x00, 0x00, 0x00, 0x00, 0x04, 0x59, 0x5a,
    ];

    // mixed1k() | xz -0 --check=crc32  (a real LZMA chunk, control 0xe0)
    const MIXED1K_CRC32_XZ: [u8; 136] = [
        0xfd, 0x37, 0x7a, 0x58, 0x5a, 0x00, 0x00, 0x01, 0x69, 0x22, 0xde, 0x36,
        0x02, 0x00, 0x21, 0x01, 0x0c, 0x00, 0x00, 0x00, 0x8f, 0x98, 0x41, 0x9c,
        0xe0, 0x03, 0xff, 0x00, 0x49, 0x5d, 0x00, 0x3a, 0x1a, 0x08, 0xce, 0x76,
        0xc7, 0xe5, 0xe9, 0xd6, 0x07, 0x34, 0xc3, 0xd1, 0x0e, 0xbf, 0xce, 0x55,
        0xe1, 0xaa, 0xbd, 0xe0, 0xd5, 0x3f, 0xb8, 0x2d, 0xde, 0x32, 0xac, 0xc6,
        0xb3, 0xd7, 0x76, 0x98, 0x84, 0x33, 0x4f, 0xa2, 0x1c, 0x15, 0x1e, 0x32,
        0x2e, 0x59, 0xb8, 0xc3, 0xbe, 0x70, 0xd4, 0x0d, 0x5d, 0x31, 0xfb, 0x45,
        0x5b, 0x18, 0x60, 0x9c, 0x03, 0xe6, 0xfe, 0xed, 0xd6, 0xfe, 0xbb, 0x9a,
        0xda, 0xc8, 0xf5, 0x1c, 0x15, 0xec, 0x02, 0x43, 0x00, 0x00, 0x00, 0x00,
        0xd4, 0x40, 0x12, 0x9c, 0x00, 0x01, 0x61, 0x80, 0x08, 0x00, 0x00, 0x00,
        0x5f, 0x90, 0xaf, 0x74, 0x3e, 0x30, 0x0d, 0x8b, 0x02, 0x00, 0x00, 0x00,
        0x00, 0x01, 0x59, 0x5a,
    ];

    // printf 'hello world\n' | xz -6 --check=sha256
    const HELLO_SHA256_XZ: [u8; 92] = [
        0xfd, 0x37, 0x7a, 0x58, 0x5a, 0x00, 0x00, 0x0a, 0xe1, 0xfb, 0x0c, 0xa1,
        0x02, 0x00, 0x21, 0x01, 0x16, 0x00, 0x00, 0x00, 0x74, 0x2f, 0xe5, 0xa3,
        0x01, 0x00, 0x0b, 0x68, 0x65, 0x6c, 0x6c, 0x6f, 0x20, 0x77, 0x6f, 0x72,
        0x6c, 0x64, 0x0a, 0x00, 0xa9, 0x48, 0x90, 0x4f, 0x2f, 0x0f, 0x47, 0x9b,
        0x8f, 0x81, 0x97, 0x69, 0x4b, 0x30, 0x18, 0x4b, 0x0d, 0x2e, 0xd1, 0xc1,
        0xcd, 0x2a, 0x1e, 0xc0, 0xfb, 0x85, 0xd2, 0x99, 0xa1, 0x92, 0xa4, 0x47,
        0x00, 0x01, 0x3c, 0x0c, 0xff, 0x80, 0xc3, 0x5a, 0x18, 0x9b, 0x4b, 0x9a,
        0x01, 0x00, 0x00, 0x00, 0x00, 0x0a, 0x59, 0x5a,
    ];

    // printf 'hello world\n' | xz -6 --check=none
    const HELLO_NONE_XZ: [u8; 60] = [
        0xfd, 0x37, 0x7a, 0x58, 0x5a, 0x00, 0x00, 0x00, 0xff, 0x12, 0xd9, 0x41,
        0x02, 0x00, 0x21, 0x01, 0x16, 0x00, 0x00, 0x00, 0x74, 0x2f, 0xe5, 0xa3,
        0x01, 0x00, 0x0b, 0x68, 0x65, 0x6c, 0x6c, 0x6f, 0x20, 0x77, 0x6f, 0x72,
        0x6c, 0x64, 0x0a, 0x00, 0x00, 0x01, 0x1c, 0x0c, 0x5d, 0xa4, 0x47, 0xcf,
        0x06, 0x72, 0x9e, 0x7a, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x59, 0x5a,
    ];

    // printf '' | xz  (stream with zero blocks)
    const EMPTY_XZ: [u8; 32] = [
        0xfd, 0x37, 0x7a, 0x58, 0x5a, 0x00, 0x00, 0x04, 0xe6, 0xd6, 0xb4, 0x46,
        0x00, 0x00, 0x00, 0x00, 0x1c, 0xdf, 0x44, 0x21, 0x1f, 0xb6, 0xf3, 0x7d,
        0x01, 0x00, 0x00, 0x00, 0x00, 0x04, 0x59, 0x5a,
    ];

    // 300 incompressible bytes (python random.Random(42)); xz -0 stores
    // them as an uncompressed LZMA2 chunk (control 0x01).
    const RAND300: [u8; 300] = [
        0x39, 0x0c, 0x8c, 0x7d, 0x72, 0x47, 0x34, 0x2c, 0xd8, 0x10, 0x0f, 0x2f,
        0x6f, 0x77, 0x0d, 0x65, 0xd6, 0x70, 0xe5, 0x8e, 0x03, 0x51, 0xd8, 0xae,
        0x8e, 0x4f, 0x6e, 0xac, 0x34, 0x2f, 0xc2, 0x31, 0xb7, 0xb0, 0x87, 0x16,
        0xeb, 0x3f, 0xc1, 0x28, 0x96, 0xb9, 0x62, 0x23, 0x17, 0x74, 0x94, 0x28,
        0x77, 0x33, 0xc2, 0x8e, 0xe8, 0xba, 0x53, 0xbd, 0xb5, 0x6b, 0x88, 0x24,
        0x57, 0x7d, 0x53, 0xec, 0xc2, 0x8a, 0x70, 0xa6, 0x1c, 0x75, 0x10, 0xa1,
        0xcd, 0x89, 0x21, 0x6c, 0xa1, 0x6c, 0xff, 0xca, 0xea, 0x49, 0x87, 0x47,
        0x7e, 0x86, 0xdb, 0xcc, 0xb9, 0x70, 0x46, 0xfc, 0x2e, 0x18, 0x38, 0x4e,
        0x51, 0xd8, 0x20, 0xc5, 0xc3, 0xef, 0x80, 0x05, 0x3a, 0x88, 0xae, 0x39,
        0x96, 0xde, 0x50, 0xe8, 0x01, 0x86, 0x5b, 0x36, 0x98, 0x65, 0x4e, 0xbf,
        0x52, 0x00, 0xa5, 0xfa, 0x09, 0x39, 0xb9, 0x9d, 0x7a, 0x1d, 0x7b, 0x28,
        0x2b, 0xf8, 0x23, 0x40, 0x41, 0xf3, 0x54, 0x87, 0xd8, 0x6c, 0x66, 0x9f,
        0xcc, 0xbf, 0xe0, 0xe7, 0x3d, 0x7e, 0x73, 0x20, 0xad, 0x0a, 0x75, 0x70,
        0x03, 0x24, 0x1e, 0x75, 0x22, 0x10, 0xa9, 0x24, 0x79, 0x8e, 0xf8, 0x6d,
        0x43, 0xf2, 0x7c, 0xf2, 0xd0, 0x61, 0x30, 0x31, 0xdc, 0xb5, 0xd8, 0xd2,
        0xef, 0x1b, 0x32, 0x1f, 0xce, 0xad, 0x37, 0x7f, 0x62, 0x61, 0xe5, 0x47,
        0xd8, 0x5d, 0x8e, 0xec, 0x7f, 0x26, 0xe2, 0x32, 0x19, 0x07, 0x2f, 0x79,
        0x55, 0xd0, 0xf8, 0xf6, 0x6d, 0xcd, 0x1e, 0x54, 0xc2, 0x01, 0xc7, 0x87,
        0xe8, 0x92, 0xd8, 0xf9, 0x4f, 0x61, 0x97, 0x6f, 0x1d, 0x1f, 0xa0, 0x1d,
        0x19, 0xf4, 0x50, 0x1d, 0x29, 0x5f, 0x23, 0x22, 0x78, 0xce, 0x3d, 0x7e,
        0x14, 0x29, 0xd6, 0xa1, 0x85, 0x68, 0xa0, 0x7a, 0x87, 0xca, 0x43, 0x99,
        0xea, 0xa1, 0x25, 0x04, 0xea, 0x33, 0x25, 0x6d, 0x87, 0x43, 0xb2, 0x23,
        0x7d, 0xbd, 0x91, 0x50, 0xe0, 0x9a, 0x04, 0x99, 0x35, 0x44, 0x87, 0x3b,
        0x36, 0x4f, 0x8b, 0x90, 0x6b, 0xaf, 0x68, 0x87, 0xfa, 0x80, 0x1a, 0x2f,
        0xd8, 0x8d, 0x16, 0x01, 0xaa, 0x42, 0x86, 0x52, 0xe2, 0xda, 0x04, 0x39,
    ];

    const RAND300_XZ: [u8; 360] = [
        0xfd, 0x37, 0x7a, 0x58, 0x5a, 0x00, 0x00, 0x04, 0xe6, 0xd6, 0xb4, 0x46,
        0x02, 0x00, 0x21, 0x01, 0x0c, 0x00, 0x00, 0x00, 0x8f, 0x98, 0x41, 0x9c,
        0x01, 0x01, 0x2b, 0x39, 0x0c, 0x8c, 0x7d, 0x72, 0x47, 0x34, 0x2c, 0xd8,
        0x10, 0x0f, 0x2f, 0x6f, 0x77, 0x0d, 0x65, 0xd6, 0x70, 0xe5, 0x8e, 0x03,
        0x51, 0xd8, 0xae, 0x8e, 0x4f, 0x6e, 0xac, 0x34, 0x2f, 0xc2, 0x31, 0xb7,
        0xb0, 0x87, 0x16, 0xeb, 0x3f, 0xc1, 0x28, 0x96, 0xb9, 0x62, 0x23, 0x17,
        0x74, 0x94, 0x28, 0x77, 0x33, 0xc2, 0x8e, 0xe8, 0xba, 0x53, 0xbd, 0xb5,
        0x6b, 0x88, 0x24, 0x57, 0x7d, 0x53, 0xec, 0xc2, 0x8a, 0x70, 0xa6, 0x1c,
        0x75, 0x10, 0xa1, 0xcd, 0x89, 0x21, 0x6c, 0xa1, 0x6c, 0xff, 0xca, 0xea,
        0x49, 0x87, 0x47, 0x7e, 0x86, 0xdb, 0xcc, 0xb9, 0x70, 0x46, 0xfc, 0x2e,
        0x18, 0x38, 0x4e, 0x51, 0xd8, 0x20, 0xc5, 0xc3, 0xef, 0x80, 0x05, 0x3a,
        0x88, 0xae, 0x39, 0x96, 0xde, 0x50, 0xe8, 0x01, 0x86, 0x5b, 0x36, 0x98,
        0x65, 0x4e, 0xbf, 0x52, 0x00, 0xa5, 0xfa, 0x09, 0x39, 0xb9, 0x9d, 0x7a,
        0x1d, 0x7b, 0x28, 0x2b, 0xf8, 0x23, 0x40, 0x41, 0xf3, 0x54, 0x87, 0xd8,
        0x6c, 0x66, 0x9f, 0xcc, 0xbf, 0xe0, 0xe7, 0x3d, 0x7e, 0x73, 0x20, 0xad,
        0x0a, 0x75, 0x70, 0x03, 0x24, 0x1e, 0x75, 0x22, 0x10, 0xa9, 0x24, 0x79,
        0x8e, 0xf8, 0x6d, 0x43, 0xf2, 0x7c, 0xf2, 0xd0, 0x61, 0x30, 0x31, 0xdc,
        0xb5, 0xd8, 0xd2, 0xef, 0x1b, 0x32, 0x1f, 0xce, 0xad, 0x37, 0x7f, 0x62,
        0x61, 0xe5, 0x47, 0xd8, 0x5d, 0x8e, 0xec, 0x7f, 0x26, 0xe2, 0x32, 0x19,
        0x07, 0x2f, 0x79, 0x55, 0xd0, 0xf8, 0xf6, 0x6d, 0xcd, 0x1e, 0x54, 0xc2,
        0x01, 0xc7, 0x87, 0xe8, 0x92, 0xd8, 0xf9, 0x4f, 0x61, 0x97, 0x6f, 0x1d,
        0x1f, 0xa0, 0x1d, 0x19, 0xf4, 0x50, 0x1d, 0x29, 0x5f, 0x23, 0x22, 0x78,
        0xce, 0x3d, 0x7e, 0x14, 0x29, 0xd6, 0xa1, 0x85, 0x68, 0xa0, 0x7a, 0x87,
        0xca, 0x43, 0x99, 0xea, 0xa1, 0x25, 0x04, 0xea, 0x33, 0x25, 0x6d, 0x87,
        0x43, 0xb2, 0x23, 0x7d, 0xbd, 0x91, 0x50, 0xe0, 0x9a, 0x04, 0x99, 0x35,
        0x44, 0x87, 0x3b, 0x36, 0x4f, 0x8b, 0x90, 0x6b, 0xaf, 0x68, 0x87, 0xfa,
        0x80, 0x1a, 0x2f, 0xd8, 0x8d, 0x16, 0x01, 0xaa, 0x42, 0x86, 0x52, 0xe2,
        0xda, 0x04, 0x39, 0x00, 0x9e, 0xd4, 0x2b, 0x4b, 0xb4, 0x2f, 0xcb, 0xcf,
        0x00, 0x01, 0xc4, 0x02, 0xac, 0x02, 0x00, 0x00, 0xcc, 0xa9, 0xd5, 0x37,
        0xb1, 0xc4, 0x67, 0xfb, 0x02, 0x00, 0x00, 0x00, 0x00, 0x04, 0x59, 0x5a,
    ];

    // big3m() | xz -9 --check=crc64: two LZMA chunks (controls 0xff then
    // 0x8e — the second continues the first's state without a reset).
    const BIG3M_XZ: [u8; 624] = [
        0xfd, 0x37, 0x7a, 0x58, 0x5a, 0x00, 0x00, 0x04, 0xe6, 0xd6, 0xb4, 0x46,
        0x02, 0x00, 0x21, 0x01, 0x1c, 0x00, 0x00, 0x00, 0x10, 0xcf, 0x58, 0xcc,
        0xff, 0xff, 0x3d, 0x01, 0x98, 0x5d, 0x00, 0x3a, 0x1a, 0x08, 0xce, 0x76,
        0xc7, 0xe5, 0xe9, 0xd6, 0x07, 0x34, 0xc3, 0xd1, 0x0e, 0xbf, 0xce, 0x55,
        0xe1, 0xaa, 0xbd, 0xe0, 0xe4, 0x8f, 0x98, 0x01, 0xdd, 0x8d, 0xe5, 0x07,
        0x54, 0x9e, 0x65, 0x25, 0x5f, 0x27, 0x3a, 0x6a, 0x7e, 0xb4, 0xd3, 0x49,
        0x03, 0x89, 0xce, 0xd4, 0x7d, 0x3c, 0xff, 0x9a, 0xde, 0x36, 0x1c, 0xac,
        0x11, 0x65, 0xe2, 0xca, 0xfb, 0x29, 0x89, 0x26, 0x7f, 0x03, 0x89, 0x3d,
        0x21, 0x33, 0x04, 0xab, 0x48, 0x8c, 0x0e, 0xda, 0x9e, 0x05, 0x11, 0x0e,
        0xe7, 0x32, 0xf4, 0xa9, 0xf8, 0x0d, 0xde, 0xd1, 0x86, 0x36, 0x98, 0x59,
        0x2a, 0x68, 0x36, 0xe1, 0x44, 0x9b, 0x72, 0x70, 0xb6, 0xf9, 0x15, 0x4f,
        0xac, 0xc2, 0x05, 0x02, 0x37, 0x8c, 0x8b, 0xdc, 0x80, 0xb7, 0x10, 0xbf,
        0xf1, 0xcd, 0xb3, 0xf4, 0x90, 0xd6, 0x3c, 0x19, 0x64, 0x9b, 0xe3, 0xc0,
        0x78, 0x89, 0x28, 0x16, 0xf0, 0x39, 0x66, 0x5e, 0x77, 0x28, 0x9a, 0x0a,
        0x98, 0x54, 0x6e, 0xd7, 0x8b, 0xca, 0x88, 0x3f, 0x5f, 0x31, 0xfb, 0xc8,
        0x4f, 0xe8, 0x13, 0x3d, 0xb7, 0xe3, 0x80, 0xba, 0x23, 0xa8, 0xf8, 0x06,
        0x8b, 0xd9, 0xf5, 0x7f, 0xd6, 0x0d, 0x1b, 0x3f, 0x7d, 0x77, 0xab, 0x83,
        0x14, 0x21, 0x6e, 0xd1, 0xd5, 0x7a, 0xf3, 0x3e, 0x14, 0x53, 0x62, 0x76,
        0xac, 0x59, 0xe4, 0xe5, 0x5b, 0x05, 0x08, 0xf9, 0xc7, 0xda, 0xad, 0xfc,
        0xfb, 0x52, 0x2b, 0x74, 0xcd, 0x1e, 0x5b, 0x20, 0x42, 0xf9, 0xdd, 0x53,
        0x3d, 0xf8, 0x29, 0x64, 0x09, 0x3b, 0x80, 0xcb, 0x2a, 0x6c, 0xdf, 0xb5,
        0x3b, 0xf0, 0xc4, 0xbd, 0x2e, 0x5f, 0xaa, 0x0f, 0x3e, 0x4b, 0x66, 0x42,
        0x90, 0x13, 0x0e, 0xff, 0x10, 0x93, 0xf8, 0x71, 0x78, 0x59, 0xf8, 0x0b,
        0xcd, 0xff, 0x95, 0x28, 0x46, 0x0f, 0xa9, 0xfc, 0x7c, 0xde, 0xfb, 0x9a,
        0x30, 0x2e, 0x56, 0xc0, 0x8f, 0x85, 0xf3, 0x83, 0x81, 0xc0, 0x65, 0xc4,
        0x25, 0x53, 0xf8, 0xf5, 0x91, 0x36, 0x31, 0x05, 0xa5, 0xb0, 0xee, 0x6f,
        0xc1, 0x70, 0x4d, 0x47, 0x0c, 0xd1, 0x91, 0x11, 0xaa, 0xad, 0x60, 0x1d,
        0xba, 0xce, 0xb1, 0x27, 0x18, 0x5c, 0x59, 0x86, 0xe9, 0x66, 0x52, 0x58,
        0xbe, 0xe9, 0x76, 0xac, 0x59, 0xe4, 0xe5, 0x5b, 0x05, 0x08, 0xf9, 0xc7,
        0xda, 0xad, 0xfc, 0xfb, 0x52, 0x2b, 0x74, 0xcd, 0x1e, 0x5b, 0x20, 0x42,
        0xf9, 0xdd, 0x53, 0x3d, 0xf8, 0x29, 0x64, 0x09, 0x3b, 0x80, 0xcb, 0x2a,
        0x6c, 0xdf, 0xb5, 0x3b, 0xf0, 0xc4, 0xbd, 0x2e, 0x5f, 0xaa, 0x0f, 0x3e,
        0x4b, 0x66, 0x42, 0x90, 0x13, 0x0e, 0xff, 0x10, 0x93, 0xf8, 0x71, 0x78,
        0x59, 0xf8, 0x0b, 0xcd, 0xff, 0x95, 0x28, 0x46, 0x0f, 0xa9, 0xfc, 0x7c,
        0xde, 0xfb, 0x9a, 0x30, 0x2e, 0x56, 0xc0, 0x8f, 0x85, 0xf3, 0x83, 0x81,
        0xc0, 0x65, 0xc4, 0x25, 0x53, 0xf8, 0xf5, 0x91, 0x36, 0x31, 0x05, 0xa5,
        0xb0, 0xee, 0x6f, 0xb7, 0xb8, 0xd4, 0xe9, 0x8e, 0xf5, 0xbb, 0x00, 0x8f,
        0x00, 0xec, 0x73, 0x53, 0xa7, 0xfd, 0xbe, 0xae, 0x7c, 0x31, 0x1a, 0x9f,
        0xb7, 0x8d, 0x31, 0x6e, 0x70, 0x9e, 0xa7, 0x23, 0x5f, 0xec, 0x28, 0xcb,
        0x85, 0xd1, 0x95, 0x98, 0x8a, 0x7e, 0x2a, 0x91, 0xf2, 0x27, 0x75, 0xf7,
        0x19, 0xc0, 0x06, 0x98, 0x4d, 0x98, 0xfd, 0xd8, 0xaf, 0xd5, 0x90, 0x0f,
        0xc4, 0x25, 0x53, 0xf8, 0xf5, 0x91, 0x36, 0x31, 0x05, 0xa5, 0xb0, 0xee,
        0x6f, 0xc1, 0x70, 0x4d, 0x47, 0x0c, 0xd1, 0x91, 0x11, 0xaa, 0xad, 0x60,
        0x1d, 0xba, 0xce, 0xb1, 0x27, 0x18, 0x5c, 0x59, 0x86, 0xe9, 0x66, 0x52,
        0x58, 0xbe, 0xe9, 0x76, 0xac, 0x59, 0xe4, 0xe5, 0x5b, 0x05, 0x08, 0xf9,
        0xc7, 0xda, 0xad, 0xfc, 0xfb, 0x52, 0x2b, 0x74, 0xcd, 0x1e, 0x5b, 0x20,
        0x42, 0xf9, 0xdd, 0x53, 0x3d, 0xf8, 0x29, 0x64, 0x09, 0x3b, 0x80, 0xcb,
        0x2a, 0x6c, 0xdf, 0xb5, 0x3b, 0xf0, 0xc4, 0xbd, 0x2e, 0x5f, 0xaa, 0x0f,
        0x3e, 0x4b, 0x66, 0x42, 0x90, 0x13, 0x0e, 0xd3, 0x49, 0x36, 0x6e, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x73, 0x36, 0x98, 0xef, 0xd9, 0x3d, 0xe7, 0x8f,
        0x00, 0x01, 0xc9, 0x04, 0xfa, 0xe9, 0xbb, 0x01, 0x68, 0x67, 0xec, 0x3f,
        0xb1, 0xc4, 0x67, 0xfb, 0x02, 0x00, 0x00, 0x00, 0x00, 0x04, 0x59, 0x5a,
    ];

    /// The mixed1k vector's plaintext: 40 numbered fox lines, cut at 1 KiB.
    fn mixed1k() -> Vec<u8> {
        let mut v = Vec::new();
        for i in 0..40 {
            v.extend_from_slice(
                format!("the quick brown fox {} jumps over the lazy dog\n", i % 7).as_bytes(),
            );
        }
        v.truncate(1024);
        v
    }

    /// The big3m vector's plaintext: the 45-byte sentence repeated 68386
    /// times (~2.9 MiB, spanning two LZMA2 chunks — the 2 MiB unpacked
    /// chunk cap forces the split).
    fn big3m() -> Vec<u8> {
        let unit: &[u8] = b"the quick brown fox jumps over the lazy dog. ";
        let mut v = Vec::with_capacity(unit.len() * 68386);
        for _ in 0..68386 {
            v.extend_from_slice(unit);
        }
        v
    }

    #[test]
    fn decompresses_crc64_stream() {
        assert_eq!(decompress(&HELLO_CRC64_XZ).unwrap(), b"hello world\n");
    }

    #[test]
    fn decompresses_lzma_chunk_with_crc32_check() {
        assert_eq!(decompress(&MIXED1K_CRC32_XZ).unwrap(), mixed1k());
    }

    #[test]
    fn decompresses_sha256_checked_stream() {
        assert_eq!(decompress(&HELLO_SHA256_XZ).unwrap(), b"hello world\n");
    }

    #[test]
    fn decompresses_none_checked_stream() {
        assert_eq!(decompress(&HELLO_NONE_XZ).unwrap(), b"hello world\n");
    }

    #[test]
    fn decompresses_empty_stream() {
        assert_eq!(decompress(&EMPTY_XZ).unwrap(), b"");
    }

    #[test]
    fn decompresses_uncompressed_lzma2_chunk() {
        // Incompressible input forces an uncompressed chunk (control 0x01).
        assert_eq!(RAND300_XZ[24], 0x01);
        assert_eq!(decompress(&RAND300_XZ).unwrap(), RAND300);
    }

    #[test]
    fn decompresses_multi_chunk_stream() {
        assert_eq!(decompress(&BIG3M_XZ).unwrap(), big3m());
    }

    #[test]
    fn decompresses_concatenated_streams_with_padding() {
        let mut xz = Vec::new();
        xz.extend_from_slice(&HELLO_CRC64_XZ);
        xz.extend_from_slice(&[0u8; 4]);
        xz.extend_from_slice(&HELLO_SHA256_XZ);
        xz.extend_from_slice(&[0u8; 8]); // trailing padding is fine too
        assert_eq!(decompress(&xz).unwrap(), b"hello world\nhello world\n");
    }

    #[test]
    fn misaligned_stream_padding_errors() {
        let mut xz = Vec::new();
        xz.extend_from_slice(&HELLO_CRC64_XZ);
        xz.extend_from_slice(&[0u8; 2]);
        xz.extend_from_slice(&HELLO_SHA256_XZ);
        let err = decompress(&xz).unwrap_err();
        assert!(err.contains("padding"), "got: {err}");
    }

    #[test]
    fn crc64_check_mismatch_errors() {
        // Flip a payload byte inside the uncompressed chunk.
        let mut xz = HELLO_CRC64_XZ;
        xz[30] ^= 0xff;
        let err = decompress(&xz).unwrap_err();
        assert!(err.contains("CRC64 mismatch"), "got: {err}");
    }

    #[test]
    fn sha256_check_mismatch_errors() {
        let mut xz = HELLO_SHA256_XZ;
        xz[30] ^= 0xff;
        let err = decompress(&xz).unwrap_err();
        assert!(err.contains("SHA-256 mismatch"), "got: {err}");
    }

    #[test]
    fn crc32_check_mismatch_errors() {
        // Locate the block check field from the footer's backward size and
        // corrupt its stored CRC32.
        let mut xz = MIXED1K_CRC32_XZ.to_vec();
        let n = xz.len();
        let backward = u32::from_le_bytes([xz[n - 8], xz[n - 7], xz[n - 6], xz[n - 5]]) as usize;
        let index_size = (backward + 1) * 4;
        let check_end = n - 12 - index_size;
        xz[check_end - 1] ^= 0xff;
        let err = decompress(&xz).unwrap_err();
        assert!(err.contains("block CRC32 mismatch"), "got: {err}");
    }

    #[test]
    fn corrupt_compressed_data_errors() {
        let mut xz = BIG3M_XZ.to_vec();
        let mid = xz.len() / 2; // inside the first LZMA chunk
        xz[mid] ^= 0xff;
        assert!(decompress(&xz).is_err());
    }

    #[test]
    fn truncation_errors() {
        assert!(decompress(&HELLO_CRC64_XZ[..HELLO_CRC64_XZ.len() - 1]).is_err());
        assert!(decompress(&HELLO_CRC64_XZ[..40]).is_err());
        assert!(decompress(&BIG3M_XZ[..100]).is_err());
        assert!(decompress(&[]).is_err());
    }

    #[test]
    fn bad_magic_errors() {
        let mut xz = HELLO_CRC64_XZ;
        xz[0] ^= 1;
        let err = decompress(&xz).unwrap_err();
        assert!(err.contains("magic"), "got: {err}");
    }

    #[test]
    fn unsupported_check_type_errors() {
        // Patch the check id to the reserved 0x02 and fix the header CRC so
        // the check-type rejection is what fires.
        let mut xz = HELLO_CRC64_XZ;
        xz[7] = 0x02;
        let crc = crc32(&xz[6..8]).to_le_bytes();
        xz[8..12].copy_from_slice(&crc);
        let err = decompress(&xz).unwrap_err();
        assert!(err.contains("unsupported xz check type 0x02"), "got: {err}");
    }

    #[test]
    fn bcj_filter_rejected_by_name() {
        // Patch the block header's filter id (0x21 -> 0x04, x86 BCJ) and
        // fix the header CRC so the filter rejection is what fires.
        let mut xz = HELLO_CRC64_XZ;
        xz[14] = 0x04;
        let crc = crc32(&xz[12..20]).to_le_bytes();
        xz[20..24].copy_from_slice(&crc);
        let err = decompress(&xz).unwrap_err();
        assert!(err.contains("0x04") && err.contains("x86 BCJ"), "got: {err}");
    }

    #[test]
    fn output_limit_errors() {
        let err = decompress_with_limit(&BIG3M_XZ, 1000).unwrap_err();
        assert!(err.contains("byte limit"), "got: {err}");
    }

    #[test]
    fn decodes_real_release_tarballs() {
        for name in ["binutils-2.44.tar.xz", "linux-4.14.67.tar.xz"] {
            let path = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join(".td-build-cache/sources")
                .join(name);
            if !path.exists() {
                println!("skipping {name}: {} not present", path.display());
                continue;
            }
            let data = std::fs::read(&path).unwrap();
            let started = std::time::Instant::now();
            let out = decompress(&data).unwrap();
            let elapsed = started.elapsed();
            println!(
                "{name}: {} -> {} bytes in {:.2?}",
                data.len(),
                out.len(),
                elapsed
            );
            assert!(out.len() > 10_000_000, "{name}: only {} bytes", out.len());
            assert_eq!(&out[257..262], b"ustar", "{name}: not a tar archive");
        }
    }
}
