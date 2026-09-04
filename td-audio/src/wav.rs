//! RIFF/WAVE, written and read, and the oracle §K's fourth test level needs.
//!
//! §K's headless testing ladder ends at QEMU's `-audiodev wav` backend: play a
//! deterministic tone, terminate so the WAV header finalises, then assert rate,
//! duration, non-silence and correlation with the expected waveform. That last
//! step is the interesting one — a check for "the file is not all zeroes" passes
//! on noise, and a check for an exact byte match fails on any resampling or
//! mixing QEMU does on the way out. Correlation is the assertion that survives
//! both.
//!
//! The writer exists so the tone fixture can be exercised on a machine with no
//! sound card at all: `td-audio tone --wav FILE` runs the same generator through
//! the same code path and leaves a file the same analyser reads.

use crate::sink::Spec;
use std::io;

/// The canonical 44-byte PCM header: `RIFF`, `WAVE`, a 16-byte `fmt ` chunk with
/// format tag 1, then `data`.
pub const HEADER_LEN: usize = 44;

/// Build the header for `data_len` bytes of interleaved PCM at `spec`.
pub fn header(spec: Spec, data_len: u32) -> io::Result<Vec<u8>> {
    let channels = u16::try_from(spec.channels)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "too many channels for WAVE"))?;
    let bits = u16::try_from(spec.frame_bytes.saturating_mul(8) / usize::from(channels.max(1)))
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "sample width overflows"))?;
    let block_align = u16::try_from(spec.frame_bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "frame size overflows"))?;
    let byte_rate = spec
        .rate
        .checked_mul(u32::from(block_align))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "byte rate overflows"))?;

    let mut out = Vec::with_capacity(HEADER_LEN);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(data_len.saturating_add(36)).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&spec.rate.to_le_bytes());
    out.extend_from_slice(&byte_rate.to_le_bytes());
    out.extend_from_slice(&block_align.to_le_bytes());
    out.extend_from_slice(&bits.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    Ok(out)
}

/// A parsed WAVE file: what it says it is, and its samples.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wave {
    pub rate: u32,
    pub channels: u16,
    pub bits: u16,
    /// Interleaved samples, in file order.
    pub samples: Vec<i16>,
}

impl Wave {
    pub fn frames(&self) -> usize {
        self.samples.len() / usize::from(self.channels.max(1))
    }

    /// Duration in milliseconds, from the frame count and the declared rate.
    pub fn duration_ms(&self) -> u64 {
        if self.rate == 0 {
            return 0;
        }
        (self.frames() as u64).saturating_mul(1000) / u64::from(self.rate)
    }

    /// The largest absolute sample. Zero is silence, which §K says is a failure
    /// rather than a pass.
    pub fn peak(&self) -> i32 {
        self.samples
            .iter()
            .map(|s| i32::from(*s).abs())
            .max()
            .unwrap_or(0)
    }

    /// One channel's samples.
    pub fn channel(&self, index: u16) -> Vec<i16> {
        let channels = usize::from(self.channels.max(1));
        self.samples
            .iter()
            .skip(usize::from(index))
            .step_by(channels)
            .copied()
            .collect()
    }
}

fn malformed(what: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("not a usable WAVE file: {what}"),
    )
}

fn tag(bytes: &[u8], at: usize) -> io::Result<&[u8]> {
    bytes
        .get(at..at.saturating_add(4))
        .ok_or_else(|| malformed("truncated"))
}

fn le_u32(bytes: &[u8], at: usize) -> io::Result<u32> {
    let slice = bytes
        .get(at..at.saturating_add(4))
        .ok_or_else(|| malformed("truncated"))?;
    let array: [u8; 4] = slice.try_into().map_err(|_| malformed("truncated"))?;
    Ok(u32::from_le_bytes(array))
}

fn le_u16(bytes: &[u8], at: usize) -> io::Result<u16> {
    let slice = bytes
        .get(at..at.saturating_add(2))
        .ok_or_else(|| malformed("truncated"))?;
    let array: [u8; 2] = slice.try_into().map_err(|_| malformed("truncated"))?;
    Ok(u16::from_le_bytes(array))
}

/// Parse a 16-bit PCM WAVE file.
///
/// Chunks are walked rather than assumed at fixed offsets: QEMU's `wav` backend
/// writes the canonical layout, but a `LIST` chunk between `fmt ` and `data` is
/// legal and common enough that a parser which assumes byte 44 is data will one
/// day read a chunk header as audio and report a loud click.
pub fn parse(bytes: &[u8]) -> io::Result<Wave> {
    if tag(bytes, 0)? != b"RIFF" || tag(bytes, 8)? != b"WAVE" {
        return Err(malformed("no RIFF/WAVE signature"));
    }
    let mut at = 12usize;
    let mut format: Option<(u32, u16, u16)> = None;
    let mut data: Option<&[u8]> = None;
    while at.saturating_add(8) <= bytes.len() {
        let id = tag(bytes, at)?;
        // A length taken from the file, so every add around it saturates. The
        // loop bound already keeps `at` inside the buffer, which is why this
        // was never reachable — but a walk over attacker-shaped bytes should
        // not depend on a second invariant to stay in range.
        let len = le_u32(bytes, at.saturating_add(4))? as usize;
        let body_at = at.saturating_add(8);
        let body = bytes
            .get(body_at..body_at.saturating_add(len))
            // A final chunk whose declared length runs past the end is exactly
            // what an interrupted capture looks like; take what is there rather
            // than discarding a recording that is otherwise fine.
            .or_else(|| bytes.get(body_at..))
            .ok_or_else(|| malformed("a chunk runs past the end"))?;
        match id {
            b"fmt " => {
                if le_u16(body, 0)? != 1 {
                    return Err(malformed("only uncompressed PCM is read here"));
                }
                format = Some((le_u32(body, 4)?, le_u16(body, 2)?, le_u16(body, 14)?));
            }
            b"data" => data = Some(body),
            _ => {}
        }
        // Chunks are word-aligned, so an odd length carries a pad byte.
        at = at
            .saturating_add(8)
            .saturating_add(len)
            .saturating_add(len % 2);
    }
    let (rate, channels, bits) = format.ok_or_else(|| malformed("no fmt chunk"))?;
    let data = data.ok_or_else(|| malformed("no data chunk"))?;
    if bits != 16 {
        return Err(malformed("only 16-bit samples are read here"));
    }
    Ok(Wave {
        rate,
        channels,
        bits,
        samples: data
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| i16::from_le_bytes(*pair))
            .collect(),
    })
}

/// Normalised cross-correlation of two signals at zero lag, in `-1.0..=1.0`.
///
/// This is the §K oracle's "correlation with the expected waveform". It is
/// scale-invariant, so a backend that applied gain still correlates at 1.0,
/// and it collapses towards zero for noise, silence or a different frequency —
/// which are the three ways a sound path can be broken while still producing
/// bytes.
pub fn correlation(a: &[i16], b: &[i16]) -> f64 {
    let n = a.len().min(b.len());
    if n == 0 {
        return 0.0;
    }
    let (mut sum_a, mut sum_b) = (0.0f64, 0.0f64);
    for index in 0..n {
        sum_a += f64::from(a.get(index).copied().unwrap_or(0));
        sum_b += f64::from(b.get(index).copied().unwrap_or(0));
    }
    let (mean_a, mean_b) = (sum_a / n as f64, sum_b / n as f64);
    let (mut cov, mut var_a, mut var_b) = (0.0f64, 0.0f64, 0.0f64);
    for index in 0..n {
        let da = f64::from(a.get(index).copied().unwrap_or(0)) - mean_a;
        let db = f64::from(b.get(index).copied().unwrap_or(0)) - mean_b;
        cov += da * db;
        var_a += da * da;
        var_b += db * db;
    }
    if var_a <= 0.0 || var_b <= 0.0 {
        return 0.0;
    }
    cov / (var_a.sqrt() * var_b.sqrt())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::tone::{Generator, Tone};

    fn rendered(frames: usize) -> Vec<u8> {
        let mut generator = Generator::new(Spec::fixed(), Tone::fixture());
        let mut out = Vec::new();
        generator.fill(&mut out, frames);
        out
    }

    #[test]
    fn the_header_is_the_canonical_forty_four_bytes() {
        let head = header(Spec::fixed(), 4000).unwrap();
        assert_eq!(head.len(), HEADER_LEN);
        assert_eq!(head.get(..4).unwrap(), b"RIFF");
        assert_eq!(head.get(8..12).unwrap(), b"WAVE");
        assert_eq!(le_u32(&head, 4).unwrap(), 4036);
        assert_eq!(le_u16(&head, 20).unwrap(), 1, "uncompressed PCM");
        assert_eq!(le_u16(&head, 22).unwrap(), 2, "stereo");
        assert_eq!(le_u32(&head, 24).unwrap(), 48000);
        assert_eq!(le_u32(&head, 28).unwrap(), 48000 * 4, "byte rate");
        assert_eq!(le_u16(&head, 32).unwrap(), 4, "block align");
        assert_eq!(le_u16(&head, 34).unwrap(), 16, "bits per sample");
        assert_eq!(head.get(36..40).unwrap(), b"data");
        assert_eq!(le_u32(&head, 40).unwrap(), 4000);
    }

    #[test]
    fn what_is_written_reads_back_as_what_was_written() {
        let pcm = rendered(4800);
        let mut file = header(Spec::fixed(), pcm.len() as u32).unwrap();
        file.extend_from_slice(&pcm);
        let wave = parse(&file).unwrap();
        assert_eq!(wave.rate, 48000);
        assert_eq!(wave.channels, 2);
        assert_eq!(wave.bits, 16);
        assert_eq!(wave.frames(), 4800);
        assert_eq!(wave.duration_ms(), 100);
        assert!(wave.peak() > 9000, "peak {}", wave.peak());
        // The oracle: the file correlates with the waveform that produced it.
        let expected: Vec<i16> = (0..4800)
            .map(|f| Generator::sample_at(Spec::fixed(), Tone::fixture(), f))
            .collect();
        assert!(correlation(&wave.channel(0), &expected) > 0.999);
    }

    /// A `LIST` chunk between `fmt ` and `data` does not shift the audio.
    #[test]
    fn a_chunk_between_fmt_and_data_is_walked_over() {
        let pcm = rendered(480);
        let mut file = header(Spec::fixed(), pcm.len() as u32).unwrap();
        // Splice a 10-byte LIST chunk (odd body, so it carries a pad byte) in
        // front of the data chunk.
        let mut spliced: Vec<u8> = file.get(..36).unwrap().to_vec();
        spliced.extend_from_slice(b"LIST");
        spliced.extend_from_slice(&9u32.to_le_bytes());
        spliced.extend_from_slice(b"INFOhello");
        spliced.push(0);
        spliced.extend_from_slice(file.get(36..).unwrap());
        spliced.extend_from_slice(&pcm);
        file.extend_from_slice(&pcm);
        let straight = parse(&file).unwrap();
        let with_list = parse(&spliced).unwrap();
        assert_eq!(straight.samples, with_list.samples);
    }

    /// A capture cut short mid-write still parses: the data chunk's declared
    /// length is a claim, and the bytes that arrived are the recording.
    #[test]
    fn a_truncated_capture_yields_what_arrived() {
        let pcm = rendered(4800);
        let mut file = header(Spec::fixed(), pcm.len() as u32).unwrap();
        file.extend_from_slice(pcm.get(..1000).unwrap());
        let wave = parse(&file).unwrap();
        assert_eq!(wave.samples.len(), 500);
    }

    #[test]
    fn something_that_is_not_a_wave_is_refused() {
        assert!(parse(b"").is_err());
        assert!(parse(b"RIFF\0\0\0\0NOTAWAVE").is_err());
        let mut headerless = b"RIFF\x24\x00\x00\x00WAVE".to_vec();
        headerless.extend_from_slice(b"data\x00\x00\x00\x00");
        assert!(parse(&headerless)
            .unwrap_err()
            .to_string()
            .contains("no fmt chunk"));
    }

    /// Correlation does what the oracle needs: one for the same wave at any
    /// gain, near zero for silence, noise or the wrong frequency.
    #[test]
    fn correlation_separates_the_tone_from_everything_else() {
        let spec = Spec::fixed();
        let tone = Tone::fixture();
        let expected: Vec<i16> = (0..4800)
            .map(|f| Generator::sample_at(spec, tone, f))
            .collect();
        assert!((correlation(&expected, &expected) - 1.0).abs() < 1e-9);

        let quiet: Vec<i16> = expected.iter().map(|s| s / 4).collect();
        assert!(
            correlation(&expected, &quiet) > 0.999,
            "gain must not matter"
        );

        let silence = vec![0i16; 4800];
        assert_eq!(correlation(&expected, &silence), 0.0);

        // A different frequency: 440 Hz against 997 Hz over a tenth of a second.
        let other: Vec<i16> = (0..4800)
            .map(|f| {
                Generator::sample_at(
                    spec,
                    Tone {
                        hertz: 997,
                        amplitude: 10000,
                    },
                    f,
                )
            })
            .collect();
        assert!(
            correlation(&expected, &other).abs() < 0.1,
            "{}",
            correlation(&expected, &other)
        );

        // A deterministic pseudo-random signal, so "not silence" is not enough
        // to pass the oracle.
        let mut state = 0x1234_5678u32;
        let noise: Vec<i16> = (0..4800)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (state >> 16) as i16
            })
            .collect();
        assert!(correlation(&expected, &noise).abs() < 0.1);
        assert_eq!(correlation(&[], &expected), 0.0);
    }
}
