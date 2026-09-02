//! The layout of the three PCM structs, and the only place that knows it.
//!
//! `sys.rs` hands the kernel opaque byte arrays; this module is what decides
//! what a byte at an offset means. Keeping the two apart is the same split
//! `td-util` makes between its ioctl wrappers and `term.rs`: the syscall surface
//! stays about syscalls, and the layout knowledge sits next to the readback that
//! checks it.
//!
//! # Why the parameters are an enum and not an index
//!
//! §K.4 names the trap. Inside `snd_pcm_hw_params` the used intervals are
//! indexed from `FIRST_INTERVAL`, and `RATE` (11), `PERIOD_SIZE` (13) and
//! `BUFFER_SIZE` (17) are adjacent same-typed fields. An off-by-one is therefore
//! not a crash, a type error or a rejected ioctl — it is a well-formed
//! constraint on the WRONG parameter, and the first symptom is that the sound
//! comes out at the wrong pitch. So the parameters are named by an enum with
//! exactly the arms this daemon uses, and exactly one function
//! (`Interval::offset`) does the arithmetic that turns an arm into a byte
//! offset.
//!
//! # What "any" means
//!
//! A hardware-parameter block starts fully unconstrained — every mask bit set,
//! every interval `[0, u32::MAX]` — and each ioctl narrows it. That is why
//! `HW_REFINE` exists: the daemon says what it needs, the kernel says what is
//! left, and the daemon picks from what is left. Asserting a period size
//! without refining first is how a configuration that works on one card fails
//! on another.

use crate::sys::{HwParams, PcmInfo, SwParams, HW_PARAMS_LEN, PCM_INFO_LEN, SW_PARAMS_LEN};
use std::io;

/// `struct snd_mask` is `__u32 bits[(SNDRV_MASK_MAX + 31) / 32]` with
/// `SNDRV_MASK_MAX = 256`, so eight words, so 32 bytes.
const MASK_WORDS: usize = 8;
const MASK_LEN: usize = MASK_WORDS * 4;
/// `struct snd_interval` — `min`, `max`, then a word of one-bit flags.
const INTERVAL_LEN: usize = 12;

/// `flags`, then `masks[3]`, then `mres[5]`.
const MASKS_OFFSET: usize = 4;
/// `4 + 8 * 32` — the reserved masks are counted, because the intervals sit
/// after all eight of them.
const INTERVALS_OFFSET: usize = MASKS_OFFSET + 8 * MASK_LEN;
/// `260 + 21 * 12` — again the reserved intervals are counted.
const RMASK_OFFSET: usize = INTERVALS_OFFSET + 21 * INTERVAL_LEN;
const CMASK_OFFSET: usize = RMASK_OFFSET + 4;
const INFO_OFFSET: usize = CMASK_OFFSET + 4;
const MSBITS_OFFSET: usize = INFO_OFFSET + 4;
const RATE_NUM_OFFSET: usize = MSBITS_OFFSET + 4;
const RATE_DEN_OFFSET: usize = RATE_NUM_OFFSET + 4;
/// `snd_pcm_uframes_t fifo_size`, naturally aligned at 536.
const FIFO_SIZE_OFFSET: usize = RATE_DEN_OFFSET + 4;

/// `openmin:1` — the interval excludes its own minimum.
const INTERVAL_OPENMIN: u32 = 1 << 0;
/// `openmax:1`.
const INTERVAL_OPENMAX: u32 = 1 << 1;
/// `integer:1` — every value in the interval is admissible, not just some.
const INTERVAL_INTEGER: u32 = 1 << 2;
/// `empty:1` — the constraint set is unsatisfiable.
const INTERVAL_EMPTY: u32 = 1 << 3;

/// The mask parameters, by their `SNDRV_PCM_HW_PARAM_*` index.
///
/// Three arms because the kernel has three mask parameters, and this daemon
/// constrains all three: leaving `SUBFORMAT` unconstrained lets a card choose a
/// packed subformat whose frame size is not `channels * 2`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mask {
    Access,
    Format,
    Subformat,
}

/// The interval parameters this daemon reads or writes — and no others.
///
/// `SAMPLE_BITS` and `FRAME_BITS` are read-only here: they are what the frame
/// size is checked against after the kernel has chosen, which is the arithmetic
/// every transfer depends on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Interval {
    SampleBits,
    FrameBits,
    Channels,
    Rate,
    PeriodSize,
    BufferSize,
}

/// `SNDRV_PCM_ACCESS_RW_INTERLEAVED`. The whole of §K.4's "no mmap" decision is
/// this one value.
pub const ACCESS_RW_INTERLEAVED: u32 = 3;
/// `SNDRV_PCM_FORMAT_S16_LE`.
pub const FORMAT_S16_LE: u32 = 2;
/// `SNDRV_PCM_SUBFORMAT_STD`.
pub const SUBFORMAT_STD: u32 = 0;
/// `SNDRV_PCM_STREAM_PLAYBACK`, as `snd_pcm_info.stream` reports it.
pub const STREAM_PLAYBACK: i32 = 0;

impl Mask {
    /// `SNDRV_PCM_HW_PARAM_ACCESS` is 0 and `FIRST_MASK` is `ACCESS`, so the
    /// mask index and the parameter number coincide. Spelled out anyway,
    /// because the equality is a fact about the UAPI rather than a definition.
    const fn index(self) -> usize {
        match self {
            Mask::Access => 0,
            Mask::Format => 1,
            Mask::Subformat => 2,
        }
    }

    const fn offset(self) -> usize {
        MASKS_OFFSET + self.index() * MASK_LEN
    }
}

impl Interval {
    /// The `SNDRV_PCM_HW_PARAM_*` number, which is what the `rmask` bit is.
    pub const fn param(self) -> u32 {
        match self {
            Interval::SampleBits => 8,
            Interval::FrameBits => 9,
            Interval::Channels => 10,
            Interval::Rate => 11,
            Interval::PeriodSize => 13,
            Interval::BufferSize => 17,
        }
    }

    /// The ONE piece of offset arithmetic. `FIRST_INTERVAL` is `SAMPLE_BITS`
    /// (8), so an interval's slot is its parameter number less eight.
    const fn offset(self) -> usize {
        INTERVALS_OFFSET + (self.param() as usize - 8) * INTERVAL_LEN
    }

    /// The name a diagnostic uses, so a refusal says which parameter the device
    /// would not take.
    pub const fn name(self) -> &'static str {
        match self {
            Interval::SampleBits => "sample_bits",
            Interval::FrameBits => "frame_bits",
            Interval::Channels => "channels",
            Interval::Rate => "rate",
            Interval::PeriodSize => "period_size",
            Interval::BufferSize => "buffer_size",
        }
    }
}

/// An interval as the kernel returned it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Range {
    pub min: u32,
    pub max: u32,
    pub openmin: bool,
    pub openmax: bool,
    pub integer: bool,
    pub empty: bool,
}

impl Range {
    /// The single value this interval admits, if it admits exactly one.
    ///
    /// This is the readback §K.4 demands: after `HW_PARAMS` nothing observable
    /// distinguishes a mask the kernel narrowed from one it honoured, so the
    /// daemon asks what was chosen instead of assuming what was asked.
    pub fn exact(&self) -> Option<u32> {
        if self.empty || self.openmin || self.openmax || self.min != self.max {
            return None;
        }
        Some(self.min)
    }

    /// The smallest admissible value, accounting for an open lower bound.
    pub fn lowest(&self) -> Option<u32> {
        if self.empty {
            return None;
        }
        let min = if self.openmin { self.min.checked_add(1)? } else { self.min };
        let max = self.highest()?;
        if min > max {
            None
        } else {
            Some(min)
        }
    }

    /// The largest admissible value, accounting for an open upper bound.
    pub fn highest(&self) -> Option<u32> {
        if self.empty {
            return None;
        }
        if self.openmax {
            self.max.checked_sub(1)
        } else {
            Some(self.max)
        }
    }
}

fn short(what: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("PCM parameter block is too short for {what}"),
    )
}

fn get_u32(bytes: &[u8], at: usize) -> io::Result<u32> {
    let slice = bytes.get(at..at.saturating_add(4)).ok_or_else(|| short("a word"))?;
    let array: [u8; 4] = slice.try_into().map_err(|_| short("a word"))?;
    Ok(u32::from_ne_bytes(array))
}

fn put_u32(bytes: &mut [u8], at: usize, value: u32) -> io::Result<()> {
    bytes
        .get_mut(at..at.saturating_add(4))
        .ok_or_else(|| short("a word"))?
        .copy_from_slice(&value.to_ne_bytes());
    Ok(())
}

fn get_u64(bytes: &[u8], at: usize) -> io::Result<u64> {
    let slice = bytes.get(at..at.saturating_add(8)).ok_or_else(|| short("a long"))?;
    let array: [u8; 8] = slice.try_into().map_err(|_| short("a long"))?;
    Ok(u64::from_ne_bytes(array))
}

fn put_u64(bytes: &mut [u8], at: usize, value: u64) -> io::Result<()> {
    bytes
        .get_mut(at..at.saturating_add(8))
        .ok_or_else(|| short("a long"))?
        .copy_from_slice(&value.to_ne_bytes());
    Ok(())
}

/// Typed access to a `struct snd_pcm_hw_params` buffer.
pub trait HwParamsExt {
    /// Every mask bit set, every interval `[0, u32::MAX]`, `rmask` all ones —
    /// the state alsa-lib calls `any`, and the only correct starting point for
    /// a refinement.
    fn set_any(&mut self) -> io::Result<()>;
    /// Narrow a mask to exactly one value.
    fn set_mask(&mut self, mask: Mask, value: u32) -> io::Result<()>;
    /// The single value a mask admits, if it admits exactly one.
    fn mask_exact(&self, mask: Mask) -> io::Result<Option<u32>>;
    /// Narrow an interval to exactly one value.
    fn set_interval(&mut self, interval: Interval, value: u32) -> io::Result<()>;
    /// Read an interval back.
    fn interval(&self, interval: Interval) -> io::Result<Range>;
    /// The `fifo_size` the kernel reported, in frames.
    fn fifo_size(&self) -> io::Result<u64>;
    /// Require an interval to have been resolved to one value, naming it if not.
    fn require_exact(&self, interval: Interval) -> io::Result<u32>;
}

impl HwParamsExt for HwParams {
    fn set_any(&mut self) -> io::Result<()> {
        self.0 = [0u8; HW_PARAMS_LEN];
        for mask in [Mask::Access, Mask::Format, Mask::Subformat] {
            for word in 0..MASK_WORDS {
                put_u32(&mut self.0, mask.offset() + word * 4, u32::MAX)?;
            }
        }
        // Every interval from `FIRST_INTERVAL` to `LAST_INTERVAL`, including the
        // ones with no `Interval` arm. `PERIOD_TIME`, `PERIOD_BYTES`, `PERIODS`,
        // `BUFFER_TIME`, `BUFFER_BYTES` and `TICK_TIME` are parameters this
        // daemon never names, but leaving them zeroed presents `[0, 0]` — an
        // unsatisfiable constraint the kernel reports as EINVAL on a card that
        // would otherwise have worked. So they are opened BY NUMBER rather than
        // promoted to arms, because an arm is a claim that the daemon has a use
        // for the parameter.
        for param in 8u32..=19 {
            let offset = INTERVALS_OFFSET + (param as usize - 8) * INTERVAL_LEN;
            put_u32(&mut self.0, offset, 0)?;
            put_u32(&mut self.0, offset + 4, u32::MAX)?;
            put_u32(&mut self.0, offset + 8, 0)?;
        }
        put_u32(&mut self.0, RMASK_OFFSET, u32::MAX)?;
        put_u32(&mut self.0, INFO_OFFSET, u32::MAX)?;
        Ok(())
    }

    fn set_mask(&mut self, mask: Mask, value: u32) -> io::Result<()> {
        let word = usize::try_from(value / 32)
            .ok()
            .filter(|word| *word < MASK_WORDS)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "mask value is outside SNDRV_MASK_MAX")
            })?;
        for w in 0..MASK_WORDS {
            put_u32(&mut self.0, mask.offset() + w * 4, 0)?;
        }
        put_u32(&mut self.0, mask.offset() + word * 4, 1u32 << (value % 32))
    }

    fn mask_exact(&self, mask: Mask) -> io::Result<Option<u32>> {
        let mut found = None;
        for w in 0..MASK_WORDS {
            let bits = get_u32(&self.0, mask.offset() + w * 4)?;
            if bits == 0 {
                continue;
            }
            if bits.count_ones() != 1 || found.is_some() {
                return Ok(None);
            }
            found = Some(w as u32 * 32 + bits.trailing_zeros());
        }
        Ok(found)
    }

    fn set_interval(&mut self, interval: Interval, value: u32) -> io::Result<()> {
        put_u32(&mut self.0, interval.offset(), value)?;
        put_u32(&mut self.0, interval.offset() + 4, value)?;
        put_u32(&mut self.0, interval.offset() + 8, INTERVAL_INTEGER)
    }

    fn interval(&self, interval: Interval) -> io::Result<Range> {
        let flags = get_u32(&self.0, interval.offset() + 8)?;
        Ok(Range {
            min: get_u32(&self.0, interval.offset())?,
            max: get_u32(&self.0, interval.offset() + 4)?,
            openmin: flags & INTERVAL_OPENMIN != 0,
            openmax: flags & INTERVAL_OPENMAX != 0,
            integer: flags & INTERVAL_INTEGER != 0,
            empty: flags & INTERVAL_EMPTY != 0,
        })
    }

    fn fifo_size(&self) -> io::Result<u64> {
        get_u64(&self.0, FIFO_SIZE_OFFSET)
    }

    fn require_exact(&self, interval: Interval) -> io::Result<u32> {
        let range = self.interval(interval)?;
        range.exact().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "the device left {} unresolved as [{}, {}]",
                    interval.name(),
                    range.min,
                    range.max
                ),
            )
        })
    }
}

/// The software-parameter fields this daemon sets, by byte offset.
const SW_TSTAMP_MODE: usize = 0;
const SW_PERIOD_STEP: usize = 4;
const SW_AVAIL_MIN: usize = 16;
const SW_XFER_ALIGN: usize = 24;
const SW_START_THRESHOLD: usize = 32;
const SW_STOP_THRESHOLD: usize = 40;
const SW_SILENCE_THRESHOLD: usize = 48;
const SW_SILENCE_SIZE: usize = 56;
const SW_BOUNDARY: usize = 64;
const SW_PROTO: usize = 72;
const SW_TSTAMP_TYPE: usize = 76;

/// Typed access to a `struct snd_pcm_sw_params` buffer.
pub trait SwParamsExt {
    /// Fill in the whole block for a playback stream that starts on command.
    ///
    /// `start_threshold` is set to `boundary`, which is how ALSA spells "never
    /// start by yourself": the mixer primes a full buffer and then issues
    /// `START`, so playback begins at a moment the daemon chose rather than at
    /// whatever moment the ring happened to fill.
    fn set_playback(
        &mut self,
        proto: u32,
        avail_min: u64,
        buffer_size: u64,
        boundary: u64,
    ) -> io::Result<()>;
    /// The `boundary` the kernel wrote back.
    fn boundary(&self) -> io::Result<u64>;
}

impl SwParamsExt for SwParams {
    fn set_playback(
        &mut self,
        proto: u32,
        avail_min: u64,
        buffer_size: u64,
        boundary: u64,
    ) -> io::Result<()> {
        self.0 = [0u8; SW_PARAMS_LEN];
        // SNDRV_PCM_TSTAMP_NONE: this daemon derives its clock from DELAY and
        // its own frame counters (§K.3), so it never asks the kernel to stamp.
        put_u32(&mut self.0, SW_TSTAMP_MODE, 0)?;
        put_u32(&mut self.0, SW_PERIOD_STEP, 1)?;
        put_u64(&mut self.0, SW_AVAIL_MIN, avail_min)?;
        // Obsolete since the transfer alignment moved into the core, but the
        // field is still copied in, and zero has meant "unset" on kernels that
        // did read it.
        put_u64(&mut self.0, SW_XFER_ALIGN, 1)?;
        put_u64(&mut self.0, SW_START_THRESHOLD, boundary)?;
        // Stop on underrun rather than free-running: an XRUN the daemon can see
        // and recover from is worth more than silence it cannot.
        put_u64(&mut self.0, SW_STOP_THRESHOLD, buffer_size)?;
        put_u64(&mut self.0, SW_SILENCE_THRESHOLD, 0)?;
        put_u64(&mut self.0, SW_SILENCE_SIZE, 0)?;
        put_u64(&mut self.0, SW_BOUNDARY, boundary)?;
        put_u32(&mut self.0, SW_PROTO, proto)?;
        put_u32(&mut self.0, SW_TSTAMP_TYPE, 0)?;
        Ok(())
    }

    fn boundary(&self) -> io::Result<u64> {
        get_u64(&self.0, SW_BOUNDARY)
    }
}

/// The `boundary` the kernel will compute for a given buffer size.
///
/// `runtime->boundary = buffer_size; while (boundary * 2 <= LONG_MAX -
/// buffer_size) boundary *= 2;` — reproduced here so `start_threshold` can be
/// set to it in the same ioctl that would otherwise have to learn it. The value
/// is then READ BACK from what the kernel wrote and compared, so this is a
/// prediction the daemon checks rather than a constant it trusts.
pub fn boundary_for(buffer_size: u64) -> u64 {
    let long_max = i64::MAX as u64;
    let mut boundary = buffer_size;
    if boundary == 0 {
        return 0;
    }
    while let Some(doubled) = boundary.checked_mul(2) {
        if doubled > long_max.saturating_sub(buffer_size) {
            break;
        }
        boundary = doubled;
    }
    boundary
}

/// The `snd_pcm_info` fields discovery checks.
const INFO_DEVICE: usize = 0;
const INFO_SUBDEVICE: usize = 4;
const INFO_STREAM: usize = 8;
const INFO_CARD: usize = 12;
const INFO_ID: usize = 16;
const INFO_ID_LEN: usize = 64;
const INFO_NAME: usize = 80;
const INFO_NAME_LEN: usize = 80;

/// What `SNDRV_PCM_IOCTL_INFO` said about the node that was actually opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub card: i32,
    pub device: u32,
    pub subdevice: u32,
    pub stream: i32,
    pub id: String,
    pub name: String,
}

/// Typed access to a `struct snd_pcm_info` buffer.
pub trait PcmInfoExt {
    fn identity(&self) -> io::Result<Identity>;
}

impl PcmInfoExt for PcmInfo {
    fn identity(&self) -> io::Result<Identity> {
        if self.0.len() != PCM_INFO_LEN {
            return Err(short("the info block"));
        }
        Ok(Identity {
            card: get_u32(&self.0, INFO_CARD)? as i32,
            device: get_u32(&self.0, INFO_DEVICE)?,
            subdevice: get_u32(&self.0, INFO_SUBDEVICE)?,
            stream: get_u32(&self.0, INFO_STREAM)? as i32,
            id: fixed_string(&self.0, INFO_ID, INFO_ID_LEN)?,
            name: fixed_string(&self.0, INFO_NAME, INFO_NAME_LEN)?,
        })
    }
}

/// A NUL-padded fixed-width kernel string, with anything unprintable dropped.
///
/// The bytes come from a device the daemon does not control, and they end up in
/// diagnostics and — once the protocol lands — in a sink description sent to
/// clients, so they are sanitised at the boundary rather than where they are
/// used.
fn fixed_string(bytes: &[u8], at: usize, len: usize) -> io::Result<String> {
    let slice = bytes
        .get(at..at.saturating_add(len))
        .ok_or_else(|| short("a name field"))?;
    let end = slice.iter().position(|b| *b == 0).unwrap_or(slice.len());
    let text = slice.get(..end).unwrap_or(&[]);
    Ok(text
        .iter()
        .map(|b| {
            if b.is_ascii_graphic() || *b == b' ' {
                *b as char
            } else {
                '?'
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// Every offset this module computes, against the numbers a compile of
    /// `<sound/asound.h>` printed. This is the table §K.4 says an off-by-one
    /// would otherwise turn into a wrong-pitch bug rather than an error.
    #[test]
    fn the_offsets_are_the_ones_the_uapi_header_produces() {
        assert_eq!(MASKS_OFFSET, 4);
        assert_eq!(INTERVALS_OFFSET, 260);
        assert_eq!(RMASK_OFFSET, 512);
        assert_eq!(CMASK_OFFSET, 516);
        assert_eq!(INFO_OFFSET, 520);
        assert_eq!(MSBITS_OFFSET, 524);
        assert_eq!(RATE_NUM_OFFSET, 528);
        assert_eq!(RATE_DEN_OFFSET, 532);
        assert_eq!(FIFO_SIZE_OFFSET, 536);
        // The reserved tail: 536 + 8 + 64 = 608.
        assert_eq!(FIFO_SIZE_OFFSET + 8 + 64, HW_PARAMS_LEN);
        assert_eq!(Mask::Access.offset(), 4);
        assert_eq!(Mask::Format.offset(), 36);
        assert_eq!(Mask::Subformat.offset(), 68);
        // SAMPLE_BITS is FIRST_INTERVAL, so it sits at the base.
        assert_eq!(Interval::SampleBits.offset(), 260);
        assert_eq!(Interval::Channels.offset(), 260 + 2 * 12);
        assert_eq!(Interval::Rate.offset(), 260 + 3 * 12);
        assert_eq!(Interval::PeriodSize.offset(), 260 + 5 * 12);
        assert_eq!(Interval::BufferSize.offset(), 260 + 9 * 12);
        // The last interval this crate can name still lands inside the array.
        assert!(Interval::BufferSize.offset() + INTERVAL_LEN <= RMASK_OFFSET);
    }

    /// Adjacent parameters have distinct offsets — the off-by-one §K.4 warns
    /// about would make two of these equal.
    #[test]
    fn no_two_named_parameters_share_an_offset() {
        let all = [
            Interval::SampleBits,
            Interval::FrameBits,
            Interval::Channels,
            Interval::Rate,
            Interval::PeriodSize,
            Interval::BufferSize,
        ];
        let mut offsets: Vec<usize> = all.iter().map(|i| i.offset()).collect();
        let count = offsets.len();
        offsets.sort_unstable();
        offsets.dedup();
        assert_eq!(offsets.len(), count);
        // ...and the parameter numbers are the UAPI ones, since the offset is
        // derived from them.
        assert_eq!(Interval::Channels.param(), 10);
        assert_eq!(Interval::Rate.param(), 11);
        assert_eq!(Interval::PeriodSize.param(), 13);
        assert_eq!(Interval::BufferSize.param(), 17);
    }

    #[test]
    fn any_opens_every_mask_and_interval() {
        let mut params = HwParams::zeroed();
        params.set_any().unwrap();
        assert_eq!(get_u32(&params.0, RMASK_OFFSET).unwrap(), u32::MAX);
        for mask in [Mask::Access, Mask::Format, Mask::Subformat] {
            assert_eq!(params.mask_exact(mask).unwrap(), None);
            for w in 0..MASK_WORDS {
                assert_eq!(get_u32(&params.0, mask.offset() + w * 4).unwrap(), u32::MAX);
            }
        }
        // Every interval slot, including the ones with no enum arm — a zeroed
        // `[0, 0]` there is an unsatisfiable constraint the kernel rejects.
        for param in 8u32..=19 {
            let offset = INTERVALS_OFFSET + (param as usize - 8) * INTERVAL_LEN;
            assert_eq!(get_u32(&params.0, offset).unwrap(), 0);
            assert_eq!(get_u32(&params.0, offset + 4).unwrap(), u32::MAX);
        }
        // The reserved intervals stay zeroed: the kernel ignores them, and
        // opening them would be a claim about fields the UAPI does not define.
        let reserved = INTERVALS_OFFSET + 12 * INTERVAL_LEN;
        assert_eq!(get_u32(&params.0, reserved + 4).unwrap(), 0);
    }

    #[test]
    fn a_narrowed_mask_reads_back_as_the_one_value() {
        let mut params = HwParams::zeroed();
        params.set_any().unwrap();
        params.set_mask(Mask::Access, ACCESS_RW_INTERLEAVED).unwrap();
        params.set_mask(Mask::Format, FORMAT_S16_LE).unwrap();
        assert_eq!(params.mask_exact(Mask::Access).unwrap(), Some(3));
        assert_eq!(params.mask_exact(Mask::Format).unwrap(), Some(2));
        // Subformat was left open, and reads back as such.
        assert_eq!(params.mask_exact(Mask::Subformat).unwrap(), None);
        // A value in a later word lands in that word, not the first.
        params.set_mask(Mask::Format, 40).unwrap();
        assert_eq!(params.mask_exact(Mask::Format).unwrap(), Some(40));
        assert_eq!(get_u32(&params.0, Mask::Format.offset()).unwrap(), 0);
        assert_eq!(get_u32(&params.0, Mask::Format.offset() + 4).unwrap(), 1 << 8);
        // Out of range is refused rather than silently aliased into word 0.
        assert!(params.set_mask(Mask::Format, 256).is_err());
    }

    #[test]
    fn a_narrowed_interval_reads_back_as_exact() {
        let mut params = HwParams::zeroed();
        params.set_any().unwrap();
        params.set_interval(Interval::Rate, 48000).unwrap();
        let range = params.interval(Interval::Rate).unwrap();
        assert_eq!(range.exact(), Some(48000));
        assert!(range.integer);
        assert_eq!(params.require_exact(Interval::Rate).unwrap(), 48000);
        // An untouched neighbour is still open, and `require_exact` names it.
        let err = params.require_exact(Interval::PeriodSize).unwrap_err();
        assert!(err.to_string().contains("period_size"), "{err}");
    }

    #[test]
    fn an_open_or_empty_range_has_no_exact_value() {
        let open = Range { min: 4, max: 4, openmin: true, openmax: false, integer: true, empty: false };
        assert_eq!(open.exact(), None);
        let empty = Range { min: 1, max: 9, openmin: false, openmax: false, integer: true, empty: true };
        assert_eq!(empty.exact(), None);
        assert_eq!(empty.lowest(), None);
        assert_eq!(empty.highest(), None);
        let half = Range { min: 4, max: 16, openmin: true, openmax: true, integer: true, empty: false };
        assert_eq!(half.lowest(), Some(5));
        assert_eq!(half.highest(), Some(15));
        // An open bound that empties the range answers None rather than
        // inverting: `(4, 5)` admits nothing.
        let none = Range { min: 4, max: 5, openmin: true, openmax: true, integer: true, empty: false };
        assert_eq!(none.lowest(), None);
    }

    #[test]
    fn the_boundary_matches_the_kernels_own_doubling() {
        // The kernel's loop, transcribed: buffer_size doubled while it still
        // fits under LONG_MAX - buffer_size.
        for buffer in [1024u64, 4096, 48000, 65536] {
            let expected = {
                let mut b = buffer;
                while b.checked_mul(2).is_some_and(|d| d <= i64::MAX as u64 - buffer) {
                    b *= 2;
                }
                b
            };
            assert_eq!(boundary_for(buffer), expected, "buffer {buffer}");
            assert!(boundary_for(buffer) >= buffer);
            assert_eq!(boundary_for(buffer) % buffer, 0);
        }
        assert_eq!(boundary_for(0), 0);
    }

    #[test]
    fn playback_software_parameters_never_start_by_themselves() {
        let mut sw = SwParams::zeroed();
        let boundary = boundary_for(4096);
        sw.set_playback(0x0002000e, 1024, 4096, boundary).unwrap();
        assert_eq!(sw.boundary().unwrap(), boundary);
        assert_eq!(get_u64(&sw.0, SW_START_THRESHOLD).unwrap(), boundary);
        assert_eq!(get_u64(&sw.0, SW_AVAIL_MIN).unwrap(), 1024);
        assert_eq!(get_u64(&sw.0, SW_STOP_THRESHOLD).unwrap(), 4096);
        assert_eq!(get_u32(&sw.0, SW_PROTO).unwrap(), 0x0002000e);
        // The kernel rejects avail_min == 0 outright, so a zero here would be a
        // configuration that never reaches the device.
        assert_ne!(get_u64(&sw.0, SW_AVAIL_MIN).unwrap(), 0);
    }

    #[test]
    fn an_identity_reads_the_fields_the_header_places() {
        let mut info = PcmInfo::zeroed();
        put_u32(&mut info.0, INFO_DEVICE, 3).unwrap();
        put_u32(&mut info.0, INFO_SUBDEVICE, 0).unwrap();
        put_u32(&mut info.0, INFO_STREAM, 0).unwrap();
        put_u32(&mut info.0, INFO_CARD, 1).unwrap();
        info.0
            .get_mut(INFO_ID..INFO_ID + 5)
            .unwrap()
            .copy_from_slice(b"HDMI\0");
        info.0
            .get_mut(INFO_NAME..INFO_NAME + 7)
            .unwrap()
            .copy_from_slice(b"HDMI 0\0");
        let identity = info.identity().unwrap();
        assert_eq!(identity.card, 1);
        assert_eq!(identity.device, 3);
        assert_eq!(identity.stream, STREAM_PLAYBACK);
        assert_eq!(identity.id, "HDMI");
        assert_eq!(identity.name, "HDMI 0");
    }

    /// A capture node reports `stream == 1`, and a hostile or broken card can
    /// put anything in a name field; neither may reach a diagnostic unfiltered.
    #[test]
    fn a_name_field_is_sanitised_at_the_boundary() {
        let mut info = PcmInfo::zeroed();
        info.0
            .get_mut(INFO_NAME..INFO_NAME + 6)
            .unwrap()
            .copy_from_slice(b"a\x1b[2Jb");
        let identity = info.identity().unwrap();
        assert_eq!(identity.name, "a?[2Jb");
        assert!(!identity.name.contains('\x1b'));
    }
}
