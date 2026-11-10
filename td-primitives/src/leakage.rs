//! Timing-leakage measurement after dudect (Reparaz, Balasch and Verbauwhede,
//! "Dude, is my code constant time?", DATE 2017): two classes of input,
//! interleaved by a coin flip, each call timed alone, and Welch's t between
//! the classes' times, over all of them and over each percentile crop.
//! |t| above 4.5 says the classes take different times. It is run on demand,
//! never in a gate: timing is noisy, and a passing run is evidence for that
//! machine, build and sample count only. td-crypto reads this file through
//! `#[path]` for the paths it owns, so it names nothing of either crate.

use std::hint::black_box;
use std::time::Instant;

/// The conventional threshold for |t|.
pub(crate) const THRESHOLD: f64 = 4.5;

/// Upper tails dropped, as fractions kept: dudect crops at percentiles
/// because interrupts and migrations only ever add time.
const CROPS: &[f64] = &[0.5, 0.75, 0.9, 0.95, 0.99, 0.999];

/// Inputs prepared per batch, outside the timed calls.
const BATCH: usize = 4096;

/// xorshift64*, for choosing classes and random inputs: statistics need no
/// secrecy, and the inputs are prepared before any timing.
pub(crate) struct Rng(u64);

impl Rng {
    /// One SplitMix64 step from the seed, so distinct seeds start distinct
    /// streams and none starts at xorshift's fixed point, zero.
    pub(crate) fn new(seed: u64) -> Self {
        let mut z = seed.wrapping_add(0x9e37_79b9_7f4a_7c15);
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^= z >> 31;
        Self(if z == 0 { 1 } else { z })
    }

    pub(crate) fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    pub(crate) fn fill(&mut self, out: &mut [u8]) {
        for chunk in out.chunks_mut(8) {
            let word = self.next_u64().to_le_bytes();
            for (byte, value) in chunk.iter_mut().zip(word) {
                *byte = value;
            }
        }
    }

    pub(crate) fn bytes<const N: usize>(&mut self) -> [u8; N] {
        let mut out = [0u8; N];
        self.fill(&mut out);
        out
    }
}

/// Welford's running mean and squared deviations.
#[derive(Clone, Copy, Default)]
struct Moments {
    n: f64,
    mean: f64,
    m2: f64,
}

impl Moments {
    fn push(&mut self, x: f64) {
        self.n += 1.0;
        let delta = x - self.mean;
        self.mean += delta / self.n;
        self.m2 += delta * (x - self.mean);
    }

    fn variance(&self) -> f64 {
        if self.n < 2.0 {
            0.0
        } else {
            self.m2 / (self.n - 1.0)
        }
    }
}

/// Welch's t between two samples; 0 when either is too small to say.
fn welch(a: &Moments, b: &Moments) -> f64 {
    let spread = a.variance() / a.n + b.variance() / b.n;
    if a.n < 2.0 || b.n < 2.0 || spread <= 0.0 {
        return 0.0;
    }
    (a.mean - b.mean) / spread.sqrt()
}

/// One path's result: the largest |t| over the uncropped samples and each
/// crop, and where it was found.
pub(crate) struct Outcome {
    pub(crate) name: &'static str,
    pub(crate) samples: usize,
    pub(crate) fixed: usize,
    pub(crate) t: f64,
    pub(crate) kept: f64,
}

impl Outcome {
    pub(crate) fn leaks(&self) -> bool {
        self.t.abs() > THRESHOLD
    }

    pub(crate) fn line(&self) -> String {
        format!(
            "leakage {}: {} samples ({} fixed), max |t| {:.2} keeping {:.1}% {}",
            self.name,
            self.samples,
            self.fixed,
            self.t.abs(),
            self.kept * 100.0,
            if self.leaks() { "LEAKS" } else { "ok" }
        )
    }
}

/// The largest |t| over all samples and each crop of `times`, which holds
/// (class, nanoseconds) pairs, class `true` for the fixed one.
fn largest_t(times: &[(bool, f64)]) -> (f64, f64) {
    let mut sorted: Vec<f64> = times.iter().map(|(_, time)| *time).collect();
    sorted.sort_by(f64::total_cmp);
    let mut best = (0.0f64, 1.0f64);
    let mut limits = vec![(f64::INFINITY, 1.0)];
    for kept in CROPS {
        let at = ((sorted.len() as f64 * kept) as usize).min(sorted.len().saturating_sub(1));
        if let Some(limit) = sorted.get(at) {
            limits.push((*limit, *kept));
        }
    }
    for (limit, kept) in limits {
        let (mut fixed, mut random) = (Moments::default(), Moments::default());
        for (class, time) in times {
            if *time <= limit {
                if *class {
                    fixed.push(*time);
                } else {
                    random.push(*time);
                }
            }
        }
        let t = welch(&fixed, &random);
        if t.abs() > best.0.abs() {
            best = (t, kept);
        }
    }
    best
}

/// Time `run` over `samples` inputs, each made by `input(fixed, rng)` with
/// `fixed` from a coin flip, `repeat` calls per measurement so that a fast
/// path rises above the clock's resolution. Inputs are made a batch at a
/// time before any of the batch is timed; a first batch only warms up.
pub(crate) fn measure<I>(
    name: &'static str,
    samples: usize,
    repeat: usize,
    seed: u64,
    mut input: impl FnMut(bool, &mut Rng) -> I,
    mut run: impl FnMut(&I),
) -> Outcome {
    let mut rng = Rng::new(seed);
    let mut times = Vec::with_capacity(samples);
    let mut batch = Vec::with_capacity(BATCH);
    let mut warm = true;
    while times.len() < samples {
        batch.clear();
        let size = if warm {
            BATCH.min(samples)
        } else {
            BATCH.min(samples - times.len())
        };
        for _ in 0..size {
            let fixed = rng.next_u64() & 1 == 0;
            batch.push((fixed, input(fixed, &mut rng)));
        }
        for (fixed, value) in &batch {
            let start = Instant::now();
            for _ in 0..repeat {
                run(black_box(value));
            }
            let elapsed = start.elapsed().as_nanos() as f64;
            if !warm {
                times.push((*fixed, elapsed));
            }
        }
        warm = false;
    }
    let (t, kept) = largest_t(&times);
    Outcome {
        name,
        samples: times.len(),
        fixed: times.iter().filter(|(class, _)| *class).count(),
        t,
        kept,
    }
}

/// Report every outcome, then require each control to show the leak it was
/// built to show and every other path to show none.
pub(crate) fn conclude(controls: &[Outcome], paths: &[Outcome]) -> Result<(), String> {
    for control in controls {
        println!("{} (positive control)", control.line());
    }
    for outcome in paths {
        println!("{}", outcome.line());
    }
    let blind: Vec<&str> = controls
        .iter()
        .filter(|control| !control.leaks())
        .map(|control| control.name)
        .collect();
    if !blind.is_empty() {
        return Err(format!(
            "positive control below |t| {THRESHOLD}, so this run cannot see a leak of its size: {}",
            blind.join(", ")
        ));
    }
    let leaking: Vec<&str> = paths
        .iter()
        .filter(|outcome| outcome.leaks())
        .map(|outcome| outcome.name)
        .collect();
    if leaking.is_empty() {
        Ok(())
    } else {
        Err(format!("|t| above {THRESHOLD}: {}", leaking.join(", ")))
    }
}

/// An early-exit comparison, the positive control: it returns at the first
/// differing byte, so equal inputs take longer than ones that differ first.
pub(crate) fn early_exit_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    for (x, y) in a.iter().zip(b) {
        if x != y {
            return false;
        }
    }
    true
}

/// [`early_exit_eq`] on equal N-byte inputs against inputs differing at
/// their first byte, 8 calls a timing.
fn early_exit_control<const N: usize>(name: &'static str, seed: u64, samples: usize) -> Outcome {
    measure(
        name,
        samples,
        8,
        seed,
        |fixed, rng| {
            let a = rng.bytes::<N>();
            let mut b = a;
            if !fixed {
                if let Some(first) = b.first_mut() {
                    *first ^= 1;
                }
            }
            (a, b)
        },
        |(a, b)| {
            black_box(early_exit_eq(black_box(a), black_box(b)));
        },
    )
}

/// The positive controls every run measures first: an early exit over 256
/// bytes, and one over 32 bytes, the size and calls per timing of the
/// smallest measured path, `ct::eq`'s. Each must exceed the threshold, or
/// the run could not have seen a leak of that size; neither says anything
/// of a smaller one.
pub(crate) fn controls(samples: usize) -> Vec<Outcome> {
    vec![
        early_exit_control::<256>("control: early-exit comparison, 256 bytes", 1, samples),
        early_exit_control::<32>("control: early-exit comparison, 32 bytes", 13, samples),
    ]
}

/// Samples per measured path, from `TD_LEAKAGE_SAMPLES` (default one
/// million, and never below a batch). A value that is not a number refuses
/// rather than quietly measuring the default.
pub(crate) fn samples() -> Result<usize, String> {
    sample_count(std::env::var("TD_LEAKAGE_SAMPLES"))
}

fn sample_count(value: Result<String, std::env::VarError>) -> Result<usize, String> {
    match value {
        Err(std::env::VarError::NotPresent) => Ok(1_000_000),
        Ok(value) => value
            .parse::<usize>()
            .map(|count| count.max(BATCH))
            .map_err(|_| format!("TD_LEAKAGE_SAMPLES is not a count: {value:?}")),
        Err(e) => Err(format!("TD_LEAKAGE_SAMPLES: {e}")),
    }
}

#[cfg(test)]
mod statistics {
    use super::*;

    #[test]
    fn welch_separates_shifted_classes_and_not_equal_ones() {
        let mut rng = Rng::new(7);
        let same: Vec<(bool, f64)> = (0..20_000)
            .map(|i| (i % 2 == 0, 100.0 + (rng.next_u64() % 10) as f64))
            .collect();
        assert!(largest_t(&same).0.abs() < THRESHOLD);
        let shifted: Vec<(bool, f64)> = (0..20_000)
            .map(|i| {
                let fixed = i % 2 == 0;
                (
                    fixed,
                    100.0 + (rng.next_u64() % 10) as f64 + if fixed { 2.0 } else { 0.0 },
                )
            })
            .collect();
        assert!(largest_t(&shifted).0.abs() > THRESHOLD);
    }

    #[test]
    fn a_sample_count_that_is_not_a_number_refuses() {
        use std::env::VarError;
        assert_eq!(sample_count(Err(VarError::NotPresent)), Ok(1_000_000));
        assert_eq!(sample_count(Ok("5000".into())), Ok(5000));
        assert_eq!(sample_count(Ok("10".into())), Ok(BATCH));
        for bad in ["", "1e6", "-1", "many"] {
            assert!(sample_count(Ok(bad.into())).is_err(), "{bad}");
        }
    }

    #[test]
    fn seeds_start_distinct_streams() {
        // Under `seed | 1`, 2 and 3 were one stream.
        let first: Vec<u64> = (0..8u64).map(|seed| Rng::new(seed).next_u64()).collect();
        let mut distinct = first.clone();
        distinct.sort_unstable();
        distinct.dedup();
        assert_eq!(distinct.len(), first.len());
    }

    #[test]
    fn the_control_returns_at_the_first_difference() {
        assert!(early_exit_eq(b"abc", b"abc"));
        assert!(!early_exit_eq(b"abc", b"abd"));
        assert!(!early_exit_eq(b"abc", b"ab"));
    }
}
