//! Run manually in release mode. Alternating paired samples; no timing gate.
#![allow(clippy::unwrap_used)]
use std::hint::black_box;
use std::time::Instant;
use td_alloc::{Arena, Budget, Buffer};

fn sample(budgeted: bool, bytes: usize, iterations: usize, root: &Budget) -> u128 {
    let start = Instant::now();
    for _ in 0..iterations {
        if budgeted {
            let request = root.child(4 * 1024 * 1024).unwrap();
            let bytes = Buffer::filled(&request, bytes, 0u8).unwrap();
            let tokens = Buffer::filled(&request, 32768, [0u32; 6]).unwrap();
            black_box((&*bytes, &*tokens));
        } else {
            let bytes = td_alloc::filled(bytes, 0u8).unwrap().into_boxed_slice();
            let tokens = td_alloc::filled(32768, [0u32; 6])
                .unwrap()
                .into_boxed_slice();
            black_box((&*bytes, &*tokens));
        }
    }
    start.elapsed().as_nanos() / iterations as u128
}
fn growth_sample(data_limit: usize, initial: usize, final_len: usize) -> u128 {
    let root = Budget::new(Budget::metadata_bytes() + data_limit).unwrap();
    let start = Instant::now();
    for _ in 0..200 {
        let mut arena = Arena::with_capacity(&root, initial).unwrap();
        for i in 0..final_len {
            black_box(arena.try_insert(i as u8).unwrap());
        }
        black_box(arena);
    }
    assert_eq!(root.used(), Budget::metadata_bytes());
    start.elapsed().as_nanos() / 200
}
fn main() {
    for (limit, initial, end) in [(5000, 2048, 2500), (40000, 16384, 20000)] {
        let mut samples = [0; 9];
        for sample in &mut samples {
            *sample = growth_sample(limit, initial, end);
        }
        samples.sort_unstable();
        println!(
            "arena_limit={limit} initial={initial} length={end} median_ns={} samples={samples:?}",
            samples.get(4).unwrap()
        );
    }
    let root = Budget::new(8 * 1024 * 1024).unwrap();
    for bytes in [4096, 1024 * 1024] {
        let mut baseline = [0; 9];
        let mut budgeted = [0; 9];
        sample(false, bytes, 100, &root);
        sample(true, bytes, 100, &root);
        for (i, (old, new)) in baseline.iter_mut().zip(&mut budgeted).enumerate() {
            if i % 2 == 0 {
                *old = sample(false, bytes, 2000, &root);
                *new = sample(true, bytes, 2000, &root);
            } else {
                *new = sample(true, bytes, 2000, &root);
                *old = sample(false, bytes, 2000, &root);
            }
        }
        baseline.sort_unstable();
        budgeted.sort_unstable();
        println!("bytes={bytes} tokens=32768 baseline_ns={} budgeted_ns={} baseline_samples={baseline:?} budgeted_samples={budgeted:?}",
            baseline.get(4).unwrap(), budgeted.get(4).unwrap());
        assert_eq!(root.used(), Budget::metadata_bytes());
    }
}
