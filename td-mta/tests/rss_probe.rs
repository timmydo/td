//! Unwrapped process observations; sampled RSS is not a transient peak bound.
#![cfg(test)]
#![forbid(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::panic)]

#[path = "support/entropy_worker_scenario.rs"]
mod entropy_worker_scenario;
#[path = "support/tls_allocation_scenario.rs"]
mod tls_allocation_scenario;
#[path = "support/tls_fragment_scenario.rs"]
mod tls_fragment_scenario;
#[path = "support/tls_handshake_scenario.rs"]
mod tls_handshake_scenario;

use std::{
    fs::File,
    hint::black_box,
    io::{Read, Seek},
};

fn parse_rss(input: &[u8]) -> Result<u64, &'static str> {
    let text = std::str::from_utf8(input).map_err(|_| "non-UTF8 rollup")?;
    if !text.ends_with('\n') {
        return Err("incomplete rollup");
    }
    let mut rss = None;
    for line in text.lines() {
        let Some(value) = line.strip_prefix("Rss:") else {
            continue;
        };
        let mut fields = value.split_ascii_whitespace();
        let number = fields.next().ok_or("missing RSS")?;
        if !number.bytes().all(|b| b.is_ascii_digit()) {
            return Err("invalid RSS digits");
        }
        let value = number.parse::<u64>().map_err(|_| "invalid RSS value")?;
        if value == 0 || fields.next() != Some("kB") || fields.next().is_some() {
            return Err("invalid RSS units or fields");
        }
        if rss.replace(value).is_some() {
            return Err("duplicate RSS");
        }
    }
    rss.ok_or("missing RSS field")
}

struct Observer {
    rollup: File,
    scratch: [u8; 4096],
}
impl Observer {
    fn new() -> Self {
        Self {
            rollup: File::open("/proc/self/smaps_rollup").unwrap(),
            scratch: [0; 4096],
        }
    }
    fn sample(&mut self) -> u64 {
        self.rollup.rewind().unwrap();
        let count = self.rollup.read(&mut self.scratch).unwrap();
        assert!(count != 0 && count < self.scratch.len());
        // One complete read; do not assemble a potentially inconsistent walk.
        assert_eq!(self.rollup.read(&mut [0; 1]).unwrap(), 0);
        parse_rss(self.scratch.get(..count).unwrap()).unwrap()
    }
}

fn controls() {
    assert_eq!(parse_rss(b"header\nRss:   123 kB\nPss: 12 kB\n"), Ok(123));
    for bad in [
        b"Rss: 0 kB\n".as_slice(),
        b"Rss: -1 kB\n",
        b"Rss: 1 MB\n",
        b"Rss: 1 kB extra\n",
        b"Rss: 1 kB",
        b"Pss: 1 kB\n",
        b"Rss: 1 kB\nRss: 2 kB\n",
        b"Rss: 18446744073709551616 kB\n",
        b"Rss: \xff kB\n",
    ] {
        assert!(parse_rss(bad).is_err());
    }
    let mut observer = Observer::new();
    let before = observer.sample();
    let mut touched = vec![0u8; black_box(16 * 1024 * 1024)];
    for page in touched.chunks_mut(4096) {
        *page.first_mut().unwrap() = 0x5a;
    }
    black_box(&mut touched);
    let live = observer.sample();
    // Require a large resident increase, without asserting an exact page delta.
    assert!(live.checked_sub(before).unwrap() >= 8 * 1024);
    drop(touched);
    let dropped = observer.sample();
    println!("rss control baseline {before}\nrss control touched {live}\nrss control dropped {dropped}\nrss-observation-v2: control passed");
}

fn observe<const N: usize>(scenario: &str, phases: [&str; N], run: impl FnOnce(&mut dyn FnMut())) {
    let mut observer = Observer::new();
    let mut samples = [0; N];
    let mut slots = samples.iter_mut();
    run(&mut || *slots.next().unwrap() = observer.sample());
    assert!(slots.next().is_none());
    for (phase, rss) in phases.into_iter().zip(samples) {
        println!("rss {scenario} {phase} {rss}");
    }
    println!("rss-observation-v2: {scenario} passed");
}

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("--tls-clients") => observe("client", tls_allocation_scenario::PHASES, |f| {
            tls_allocation_scenario::run(f)
        }),
        Some("--tls-handshake") => observe("handshake", tls_handshake_scenario::PHASES, |f| {
            tls_handshake_scenario::run(f)
        }),
        Some("--entropy-workers") => observe("entropy", entropy_worker_scenario::PHASES, |f| {
            entropy_worker_scenario::run(f)
        }),
        Some("--tls-fragments") => observe("fragment", tls_fragment_scenario::PHASES, |f| {
            tls_fragment_scenario::run(f)
        }),
        Some("--tls-large-chain") => observe("large-chain", tls_handshake_scenario::PHASES, |f| {
            tls_handshake_scenario::run_large(f)
        }),
        Some("--tls-generations") => observe("generation", tls_generation_scenario::PHASES, |f| {
            tls_generation_scenario::run(f)
        }),
        _ => controls(),
    }
}

#[path = "support/tls_generation_scenario.rs"]
mod tls_generation_scenario;
