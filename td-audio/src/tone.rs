//! The rung-25 fixture: a deterministic tone, generated in integer arithmetic.
//!
//! §I's rung 25 is "the ALSA PCM back end alone, driven by a fixture that writes
//! a tone — no protocol, no clients", and it sits before Firefox deliberately:
//! it is testable with no browser, no jail and no protocol, which makes it the
//! cheapest place to find out that the pinned kernel's sound pins are wrong.
//!
//! # Why the sine is a table and not `f64::sin`
//!
//! Because the fixture is an ORACLE. §K's fourth test level plays a
//! deterministic tone and asserts rate, duration, non-silence and correlation
//! against the expected waveform, and the expected waveform has to be something
//! a test can recompute exactly. `f64::sin` is the platform's libm, which may
//! differ by an ULP between the build host and the target — enough to make a
//! bit-exact assertion flaky in a way that would be blamed on the audio path.
//! A 257-entry quarter-wave table with linear interpolation is exact everywhere,
//! costs no floating point at all, and stays within about 0.6 LSB of a true
//! sine at 16 bits, which is below the quantisation the format already imposes.
//!
//! The generator is also what proves the mixer: two tones at different
//! frequencies summed and read back are a test with an answer written down in
//! advance.

use crate::sink::Spec;

/// `round(sin(i * PI/2 / 256) * 32768)` for `i` in `0..=256`.
///
/// The last entry is 32768, which does not fit an `i16` — the values are scaled
/// by an amplitude of at most 32767 before they become samples, so the product
/// lands back inside the range. That is why this is `i32`.
const QUARTER_SINE: [i32; 257] = [
    0, 201, 402, 603, 804, 1005, 1206, 1407, 1608, 1809, 2009, 2210, 2411, 2611, 2811, 3012, 3212,
    3412, 3612, 3812, 4011, 4211, 4410, 4609, 4808, 5007, 5205, 5404, 5602, 5800, 5998, 6195, 6393,
    6590, 6787, 6983, 7180, 7376, 7571, 7767, 7962, 8157, 8351, 8546, 8740, 8933, 9127, 9319, 9512,
    9704, 9896, 10088, 10279, 10469, 10660, 10850, 11039, 11228, 11417, 11605, 11793, 11980, 12167,
    12354, 12540, 12725, 12910, 13095, 13279, 13463, 13646, 13828, 14010, 14192, 14373, 14553,
    14733, 14912, 15091, 15269, 15447, 15624, 15800, 15976, 16151, 16326, 16500, 16673, 16846,
    17018, 17190, 17361, 17531, 17700, 17869, 18037, 18205, 18372, 18538, 18703, 18868, 19032,
    19195, 19358, 19520, 19681, 19841, 20001, 20160, 20318, 20475, 20632, 20788, 20943, 21097,
    21251, 21403, 21555, 21706, 21856, 22006, 22154, 22302, 22449, 22595, 22740, 22884, 23028,
    23170, 23312, 23453, 23593, 23732, 23870, 24008, 24144, 24279, 24414, 24548, 24680, 24812,
    24943, 25073, 25202, 25330, 25457, 25583, 25708, 25833, 25956, 26078, 26199, 26320, 26439,
    26557, 26674, 26791, 26906, 27020, 27133, 27246, 27357, 27467, 27576, 27684, 27791, 27897,
    28002, 28106, 28209, 28311, 28411, 28511, 28610, 28707, 28803, 28899, 28993, 29086, 29178,
    29269, 29359, 29448, 29535, 29622, 29707, 29792, 29875, 29957, 30038, 30118, 30196, 30274,
    30350, 30425, 30499, 30572, 30644, 30715, 30784, 30853, 30920, 30986, 31050, 31114, 31177,
    31238, 31298, 31357, 31415, 31471, 31527, 31581, 31634, 31686, 31737, 31786, 31834, 31881,
    31927, 31972, 32015, 32058, 32099, 32138, 32177, 32214, 32251, 32286, 32319, 32352, 32383,
    32413, 32442, 32470, 32496, 32522, 32546, 32568, 32590, 32610, 32629, 32647, 32664, 32679,
    32693, 32706, 32718, 32729, 32738, 32746, 32753, 32758, 32762, 32766, 32767, 32768,
];

/// A full cycle is `2^32` phase units, so the accumulator wraps by itself.
const PHASE_QUARTER: u32 = 1 << 30;
/// Bits of a quarter-cycle below the table index: `30 - 8`.
const PHASE_FRACTION_BITS: u32 = 22;

/// `sin(phase)` scaled by 32768, for a phase in units of `2^-32` cycles.
///
/// The quadrant is the top two bits, and the remaining 30 index the table with
/// 22 bits of interpolation left over. Returns `-32768..=32768`.
pub fn sine_q15(phase: u32) -> i32 {
    let quadrant = phase >> 30;
    let within = phase & (PHASE_QUARTER - 1);
    // Quadrants 1 and 3 run the quarter-wave backwards; 2 and 3 negate it.
    let argument = if quadrant.is_multiple_of(2) {
        within
    } else {
        PHASE_QUARTER - within
    };
    let index = (argument >> PHASE_FRACTION_BITS) as usize;
    let fraction = i64::from(argument & ((1 << PHASE_FRACTION_BITS) - 1));
    let low = QUARTER_SINE.get(index).copied().unwrap_or(0);
    let high = QUARTER_SINE.get(index + 1).copied().unwrap_or(low);
    let interpolated = i64::from(low) + ((i64::from(high - low) * fraction) >> PHASE_FRACTION_BITS);
    let magnitude = i32::try_from(interpolated).unwrap_or(0);
    if quadrant >= 2 {
        -magnitude
    } else {
        magnitude
    }
}

/// The phase step per frame for `hertz` at `rate`.
///
/// `hertz * 2^32 / rate`, in `u64` so the multiply cannot wrap. A rate of zero
/// gives a step of zero, which is silence rather than a division fault.
pub fn phase_step(hertz: u32, rate: u32) -> u32 {
    if rate == 0 {
        return 0;
    }
    let step = (u64::from(hertz) << 32) / u64::from(rate);
    u32::try_from(step & 0xffff_ffff).unwrap_or(0)
}

/// One sine voice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tone {
    pub hertz: u32,
    /// Peak amplitude, `0..=32767`.
    pub amplitude: i32,
}

impl Tone {
    /// The fixture default: A above middle C at about a third of full scale,
    /// which is audible on a real machine without being unpleasant and stays
    /// well clear of clipping when two of them are mixed.
    pub const fn fixture() -> Self {
        Self {
            hertz: 440,
            amplitude: 10000,
        }
    }
}

/// One voice of a fixture run: a tone and the mixer volume it plays at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Voice {
    pub tone: Tone,
    /// The per-stream volume, on the protocol's own `PA_VOLUME_NORM` scale.
    pub volume: u32,
}

/// The voices a `--voices N` run plays.
///
/// Harmonics of the base rather than arbitrary frequencies, because a harmonic
/// series is what makes a mixing fault audible: two unrelated tones beat, and
/// beating is hard to tell from a broken mixer, while a harmonic stack either
/// sounds like one richer note or sounds wrong. Each voice is attenuated by the
/// count so the sum stays inside full scale instead of relying on the clip.
pub fn plan(base: Tone, count: u32, volume_norm: u32) -> Vec<Voice> {
    let count = count.max(1);
    (0..count)
        .map(|index| Voice {
            tone: Tone {
                hertz: base.hertz.saturating_mul(index.saturating_add(1)),
                amplitude: base.amplitude,
            },
            volume: volume_norm / count,
        })
        .collect()
}

/// What the mixer will produce for `voices` at `frame`.
///
/// The same arithmetic as `Mixer::mix` — per-stream gain in `f32`, summed, then
/// clamped into an `i16` — so the fixture's oracle is the mixer's own answer
/// rather than an approximation of it.
pub fn expected(spec: Spec, voices: &[Voice], frame: u64, volume_norm: u32) -> i16 {
    let mut sum = 0.0f32;
    for voice in voices {
        let sample = f32::from(Generator::sample_at(spec, voice.tone, frame));
        sum += sample * (voice.volume as f32 / volume_norm.max(1) as f32);
    }
    sum.clamp(f32::from(i16::MIN), f32::from(i16::MAX)) as i16
}

/// A running tone, one phase accumulator per voice.
pub struct Generator {
    spec: Spec,
    tone: Tone,
    step: u32,
    phase: u32,
    /// Frames produced so far, which is the fixture's own clock.
    produced: u64,
}

impl Generator {
    pub fn new(spec: Spec, tone: Tone) -> Self {
        Self {
            spec,
            tone,
            step: phase_step(tone.hertz, spec.rate),
            phase: 0,
            produced: 0,
        }
    }

    /// The sample value at frame `n` of a fresh generator — the closed form the
    /// oracle uses, so the test does not simply re-run the loop under test.
    pub fn sample_at(spec: Spec, tone: Tone, frame: u64) -> i16 {
        let step = phase_step(tone.hertz, spec.rate);
        let phase = (step as u64).wrapping_mul(frame) as u32;
        scale(sine_q15(phase), tone.amplitude)
    }

    pub fn frames_produced(&self) -> u64 {
        self.produced
    }

    /// Fill `out` with interleaved `S16_LE` bytes, replacing its contents.
    ///
    /// The buffer is the caller's and is reused across calls, which is the whole
    /// reason this takes a `&mut Vec` rather than returning one: the fixture's
    /// loop runs at period granularity, forever.
    pub fn fill(&mut self, out: &mut Vec<u8>, frames: usize) {
        out.clear();
        let wanted = frames.saturating_mul(self.spec.frame_bytes);
        if out.capacity() < wanted {
            out.reserve(wanted - out.capacity());
        }
        let channels = self.spec.channels.max(1);
        for _ in 0..frames {
            let sample = scale(sine_q15(self.phase), self.tone.amplitude);
            let bytes = sample.to_le_bytes();
            for _ in 0..channels {
                out.extend_from_slice(&bytes);
            }
            self.phase = self.phase.wrapping_add(self.step);
            self.produced = self.produced.saturating_add(1);
        }
        // A spec whose frame size is not `channels * 2` would silently produce
        // short frames; the sink's readback has already refused that, and this
        // is where the assumption is used.
    }
}

/// `q15 * amplitude / 32768`, saturated into an `i16`.
fn scale(q15: i32, amplitude: i32) -> i16 {
    let amplitude = amplitude.clamp(0, i16::MAX as i32);
    let scaled = (i64::from(q15) * i64::from(amplitude)) >> 15;
    scaled.clamp(i16::MIN as i64, i16::MAX as i64) as i16
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// The table is a quarter of a sine: monotone up, right endpoints, and
    /// within a whisker of the real thing at every entry.
    #[test]
    fn the_table_is_a_quarter_sine() {
        assert_eq!(QUARTER_SINE.len(), 257);
        assert_eq!(QUARTER_SINE.first().copied(), Some(0));
        assert_eq!(QUARTER_SINE.last().copied(), Some(32768));
        for pair in QUARTER_SINE.windows(2) {
            let (a, b) = (
                pair.first().copied().unwrap(),
                pair.last().copied().unwrap(),
            );
            assert!(b > a, "the quarter wave must increase: {a} then {b}");
        }
        // Against the real sine, to a whole LSB. This is what makes the table a
        // transcription rather than an arbitrary curve.
        for (index, value) in QUARTER_SINE.iter().enumerate() {
            let exact = (index as f64 * std::f64::consts::FRAC_PI_2 / 256.0).sin() * 32768.0;
            assert!(
                (f64::from(*value) - exact).abs() <= 0.5001,
                "entry {index} is {value}, not {exact}"
            );
        }
    }

    /// The four quadrants, at their cardinal points.
    #[test]
    fn the_quadrants_land_where_a_sine_lands() {
        assert_eq!(sine_q15(0), 0);
        assert_eq!(sine_q15(1 << 30), 32768);
        assert_eq!(sine_q15(1 << 31), 0);
        assert_eq!(sine_q15(3 << 30), -32768);
        // Odd symmetry: sin(-x) == -sin(x), with -x spelled as the wrap.
        for phase in [1u32 << 20, 1 << 25, 12345678, 0x4321_0000] {
            assert_eq!(
                sine_q15(phase),
                -sine_q15(phase.wrapping_add(1 << 31)),
                "phase {phase:#x}"
            );
        }
    }

    /// Interpolation really interpolates: a phase between two table entries
    /// gives a value between them, not the lower one repeated.
    #[test]
    fn interpolation_moves_between_table_entries() {
        let low = sine_q15(0);
        let mid = sine_q15(1 << 21);
        let high = sine_q15(1 << 22);
        assert!(low < mid && mid < high, "{low} {mid} {high}");
        assert_eq!(high, QUARTER_SINE.get(1).copied().unwrap());
    }

    /// The whole generator against the closed-form sine, to a bounded error.
    #[test]
    fn a_generated_tone_tracks_a_real_sine() {
        let spec = Spec::fixed();
        let tone = Tone::fixture();
        let mut generator = Generator::new(spec, tone);
        let mut out = Vec::new();
        generator.fill(&mut out, 4800);
        assert_eq!(out.len(), 4800 * 4);
        let step = f64::from(tone.hertz) / f64::from(spec.rate);
        let mut peak = 0i32;
        for frame in 0..4800usize {
            let at = frame * 4;
            let left = i16::from_le_bytes([
                out.get(at).copied().unwrap(),
                out.get(at + 1).copied().unwrap(),
            ]);
            let right = i16::from_le_bytes([
                out.get(at + 2).copied().unwrap(),
                out.get(at + 3).copied().unwrap(),
            ]);
            assert_eq!(left, right, "the fixture tone is the same in both channels");
            let exact = (2.0 * std::f64::consts::PI * step * frame as f64).sin()
                * f64::from(tone.amplitude);
            assert!(
                (f64::from(left) - exact).abs() < 4.0,
                "frame {frame}: {left} against {exact}"
            );
            peak = peak.max(i32::from(left).abs());
        }
        // It really is a tone and not near-silence.
        assert!(peak > tone.amplitude * 9 / 10, "peak {peak}");
        assert!(peak <= tone.amplitude);
    }

    /// `sample_at` is the same waveform as the loop, which is what lets a test
    /// assert playback against a formula rather than against itself.
    #[test]
    fn the_closed_form_matches_the_running_generator() {
        let spec = Spec::fixed();
        let tone = Tone {
            hertz: 997,
            amplitude: 20000,
        };
        let mut generator = Generator::new(spec, tone);
        let mut out = Vec::new();
        // Filled in two calls, so this also proves the phase carries across a
        // period boundary rather than restarting.
        generator.fill(&mut out, 100);
        let mut second = Vec::new();
        generator.fill(&mut second, 100);
        out.extend_from_slice(&second);
        for frame in 0..200u64 {
            let at = frame as usize * 4;
            let got = i16::from_le_bytes([
                out.get(at).copied().unwrap(),
                out.get(at + 1).copied().unwrap(),
            ]);
            assert_eq!(
                got,
                Generator::sample_at(spec, tone, frame),
                "frame {frame}"
            );
        }
        assert_eq!(generator.frames_produced(), 200);
    }

    /// Bit-exact reproducibility: the same request gives the same bytes, which
    /// is the property that lets the tone be a committed oracle.
    #[test]
    fn the_same_request_gives_the_same_bytes() {
        let render = || {
            let mut generator = Generator::new(Spec::fixed(), Tone::fixture());
            let mut out = Vec::new();
            generator.fill(&mut out, 1000);
            out
        };
        assert_eq!(render(), render());
        // And a nailed-down prefix, so a change to the table or the phase
        // arithmetic is visible as a diff rather than as a different sound.
        let bytes = render();
        assert_eq!(
            bytes.get(..12).unwrap(),
            &[0, 0, 0, 0, 63, 2, 63, 2, 125, 4, 125, 4]
        );
        // The second frame, derived by hand from the pinned table so this is a
        // transcription of the arithmetic and not a recording of the output:
        // step = 440 * 2^32 / 48000 = 39_370_533, whose top eight quarter-wave
        // bits index entry 9 (1809) with 1_621_798 of 2^22 towards entry 10
        // (2009), giving 1809 + (200 * 1_621_798 >> 22) = 1886; scaled by the
        // fixture's amplitude, 1886 * 10000 >> 15 = 575 = 0x023f.
        assert_eq!(phase_step(440, 48000), 39_370_533);
        assert_eq!(sine_q15(39_370_533), 1886);
        assert_eq!(i16::from_le_bytes([63, 2]), 575);
    }

    #[test]
    fn the_phase_step_is_the_frequency_ratio() {
        assert_eq!(phase_step(0, 48000), 0);
        assert_eq!(
            phase_step(48000, 48000),
            0,
            "a full cycle per frame wraps to zero"
        );
        assert_eq!(phase_step(24000, 48000), 1 << 31);
        assert_eq!(phase_step(12000, 48000), 1 << 30);
        // A zero rate is silence, not a division fault.
        assert_eq!(phase_step(440, 0), 0);
    }

    #[test]
    fn amplitude_is_clamped_rather_than_wrapped() {
        assert_eq!(scale(32768, i32::MAX), i16::MAX);
        assert_eq!(scale(-32768, i32::MAX), i16::MIN + 1);
        assert_eq!(scale(32768, 0), 0);
        assert_eq!(scale(32768, -5), 0);
    }
}
