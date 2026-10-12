#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
use super::*;

#[derive(Default)]
struct Backend<const N: usize = 8> {
    source: Vec<[u8; N]>,
    position: usize,
    files: [Vec<u8>; 2],
    sealed: [usize; 2],
    calls: usize,
    fail: Option<usize>,
    turns: usize,
    bytes: u64,
    ops: usize,
    reset: usize,
    wrong_length: bool,
    skip_alternate: bool,
    admitted: bool,
    seals: usize,
    wrong_seal: Option<usize>,
    inconsistent: bool,
}
fn index(file: File) -> usize {
    match file {
        File::A => 0,
        File::B => 1,
    }
}
impl<const N: usize> Backend<N> {
    fn call(&mut self) -> Result<(), ()> {
        self.calls += 1;
        if self.fail == Some(self.calls) {
            Err(())
        } else {
            Ok(())
        }
    }
    fn op(&mut self) -> Result<(), ()> {
        assert!(self.admitted, "operation before poll admission");
        self.admitted = false;
        self.ops += 1;
        assert!(self.ops <= 1, "more than one operation in a poll");
        self.call()
    }
    fn values(&self, file: File) -> Vec<[u8; N]> {
        let (records, remainder) = self.files[index(file)].as_chunks::<N>();
        assert!(remainder.is_empty());
        records.to_vec()
    }
}
impl<const N: usize> Job<N> for Backend<N> {
    type Error = ();
    fn admit(&mut self, bytes: u64) -> Result<(), ()> {
        self.call()?;
        self.turns += 1;
        self.bytes += bytes;
        self.ops = 0;
        self.admitted = true;
        Ok(())
    }
    fn next(&mut self, record: &mut [u8; N]) -> Result<Next, ()> {
        self.op()?;
        if let Some(value) = self.source.get(self.position) {
            *record = *value;
            self.position += 1;
            Ok(if self.skip_alternate && self.position.is_multiple_of(2) {
                Next::Skipped
            } else {
                Next::Record
            })
        } else {
            Ok(Next::Complete)
        }
    }
    fn compare(&mut self, a: &[u8; N], b: &[u8; N]) -> Result<Ordering, ()> {
        self.op()?;
        Ok(if self.inconsistent {
            Ordering::Greater
        } else {
            a.cmp(b)
        })
    }
    fn append(&mut self, file: File, record: &[u8; N]) -> Result<(), ()> {
        self.op()?;
        self.files[index(file)].extend_from_slice(record);
        Ok(())
    }
    fn read(&mut self, file: File, offset: u64, record: &mut [u8; N]) -> Result<(), ()> {
        self.op()?;
        let offset = usize::try_from(offset).unwrap();
        assert!(offset + N <= self.sealed[index(file)]);
        record.copy_from_slice(&self.files[index(file)][offset..offset + N]);
        Ok(())
    }
    fn reset(&mut self, file: File) -> Result<(), ()> {
        self.op()?;
        self.reset += 1;
        self.files[index(file)].clear();
        self.sealed[index(file)] = 0;
        Ok(())
    }
    fn seal(&mut self, file: File) -> Result<u64, ()> {
        self.op()?;
        let length = self.files[index(file)].len();
        self.sealed[index(file)] = length;
        self.seals += 1;
        Ok(length as u64 + u64::from(self.wrong_length || self.wrong_seal == Some(self.seals)))
    }
}
fn source(count: usize) -> Vec<[u8; 8]> {
    (0..count)
        .rev()
        .map(|id| {
            let key = ((id * 17) % 11) as u32;
            let mut record = [0; 8];
            record[..4].copy_from_slice(&key.to_be_bytes());
            record[4..].copy_from_slice(&(id as u32).to_be_bytes());
            record
        })
        .collect()
}
fn drive<const N: usize>(
    sorter: &mut Sorter<'_, N>,
    backend: &mut Backend<N>,
) -> Result<Output, Error<()>> {
    loop {
        backend.admitted = false;
        match sorter.poll(backend)? {
            Progress::Pending => {}
            Progress::Complete(output) => return Ok(output),
        }
        assert!(backend.turns < 2_000_000);
    }
}
#[test]
fn all_run_and_eight_way_pass_boundaries_match_an_independent_sort() {
    for batch in [1, 2, 7, 32, 256] {
        for count in [
            0,
            1,
            batch - 1,
            batch,
            batch + 1,
            batch * 8,
            batch * 8 + 1,
            batch * 64 + 1,
        ] {
            let mut backend = Backend {
                source: source(count),
                ..Backend::default()
            };
            let mut expected = backend.source.clone();
            expected.sort();
            let mut run = vec![[0; 8]; batch];
            let mut heads = [[0; 8]; MAX_MERGE_RUNS];
            let mut sorter = Sorter::new(&mut run, &mut heads).unwrap();
            let output = drive(&mut sorter, &mut backend).unwrap();
            assert_eq!(output.records, count as u64);
            assert_eq!(backend.values(output.file), expected);
            assert_eq!(backend.sealed[index(output.file)], count * 8);
            assert_eq!(backend.position, count);
            let mut passes = 0;
            let mut width = batch;
            while width < count {
                passes += 1;
                width *= 8;
            }
            assert_eq!(backend.bytes, (count as u64 * (2 + 2 * passes) + 1) * 8);
            if count > batch * 64 {
                assert!(backend.reset >= 4);
            }
            let calls = backend.calls;
            assert_eq!(sorter.poll(&mut backend), Err(Error::Completed));
            assert_eq!(backend.calls, calls);
        }
    }
}
#[test]
fn every_admission_and_backend_failure_retires_without_partial_completion() {
    let mut baseline = Backend {
        source: source(19),
        ..Backend::default()
    };
    let mut run = [[0; 8]; 2];
    let mut heads = [[0; 8]; 8];
    let mut sorter = Sorter::new(&mut run, &mut heads).unwrap();
    drive(&mut sorter, &mut baseline).unwrap();
    for fail in 1..=baseline.calls {
        let mut backend = Backend {
            source: source(19),
            fail: Some(fail),
            ..Backend::default()
        };
        let mut run = [[0; 8]; 2];
        let mut heads = [[0; 8]; 8];
        let mut sorter = Sorter::new(&mut run, &mut heads).unwrap();
        assert_eq!(drive(&mut sorter, &mut backend), Err(Error::Backend(())));
        assert_eq!(backend.calls, fail);
        assert_eq!(sorter.poll(&mut backend), Err(Error::Failed));
        assert_eq!(backend.calls, fail);
    }
    let mut backend = Backend {
        source: source(19),
        fail: Some(baseline.calls + 1),
        ..Backend::default()
    };
    let mut sorter = Sorter::new(&mut run, &mut heads).unwrap();
    assert!(drive(&mut sorter, &mut backend).is_ok());
}
#[test]
fn buffer_and_record_bounds_precede_all_effects_and_seal_lengths_are_exact() {
    let mut run = [[0; 8]; 257];
    let mut heads = [[0; 8]; 9];
    assert!(matches!(
        Sorter::new(&mut run, &mut heads[..8]),
        Err(Error::Invalid)
    ));
    assert!(matches!(
        Sorter::new(&mut run[..0], &mut heads[..8]),
        Err(Error::Invalid)
    ));
    for size in [0, 7, 9] {
        assert!(matches!(
            Sorter::new(&mut run[..1], &mut heads[..size]),
            Err(Error::Invalid)
        ));
    }
    assert!(matches!(
        Sorter::new(&mut [[0; 0]; 1], &mut [[0; 0]; 8]),
        Err(Error::Invalid)
    ));
    assert!(matches!(
        Sorter::new(&mut [[0; 2049]; 1], &mut [[0; 2049]; 8]),
        Err(Error::Invalid)
    ));
    assert!(Sorter::new(&mut [[0; 2048]; 1], &mut [[0; 2048]; 8]).is_ok());
    let mut backend = Backend {
        source: source(3),
        wrong_length: true,
        ..Backend::default()
    };
    let mut sorter = Sorter::new(&mut run[..2], &mut heads[..8]).unwrap();
    assert_eq!(drive(&mut sorter, &mut backend), Err(Error::Length));
    assert_eq!(sorter.poll(&mut backend), Err(Error::Failed));
    assert_eq!(Sorter::<8>::offset::<()>(u64::MAX), Err(Error::Overflow));
}
#[test]
fn equal_records_remain_present_and_empty_input_is_sealed() {
    for count in [0, 1, 17, 513] {
        let mut backend = Backend {
            source: vec![[42; 8]; count],
            ..Backend::default()
        };
        let mut run = [[0; 8]; 3];
        let mut heads = [[0; 8]; 8];
        let mut sorter = Sorter::new(&mut run, &mut heads).unwrap();
        let output = drive(&mut sorter, &mut backend).unwrap();
        assert_eq!(backend.values(output.file), vec![[42; 8]; count]);
        assert_eq!(output.records, count as u64);
        assert_eq!(backend.sealed[index(output.file)], count * 8);
    }
}

#[test]
fn excluded_source_candidates_yield_without_ending_the_scan() {
    let mut backend = Backend {
        source: source(103),
        skip_alternate: true,
        ..Backend::default()
    };
    let mut expected: Vec<_> = backend.source.iter().step_by(2).copied().collect();
    expected.sort();
    let mut run = [[0; 8]; 2];
    let mut heads = [[0; 8]; 8];
    let mut sorter = Sorter::new(&mut run, &mut heads).unwrap();
    let output = drive(&mut sorter, &mut backend).unwrap();
    assert_eq!(output.records, 52);
    assert_eq!(backend.values(output.file), expected);
    assert_eq!(backend.position, 103);
}

#[test]
fn full_width_records_reverse_input_and_late_seal_mismatch() {
    let mut wide_source = Vec::new();
    for id in (0..513u64).rev() {
        let mut value = [0x5a; 2048];
        value[..8].copy_from_slice(&id.to_be_bytes());
        wide_source.push(value);
    }
    let mut backend = Backend::<2048> {
        source: wide_source,
        ..Backend::default()
    };
    let mut expected = backend.source.clone();
    expected.sort();
    let mut run = vec![[0; 2048]; 256];
    let mut heads = vec![[0; 2048]; 8];
    let mut sorter = Sorter::new(&mut run, &mut heads).unwrap();
    let output = drive(&mut sorter, &mut backend).unwrap();
    assert_eq!(backend.values(output.file), expected);
    assert!(
        backend.turns < 25_000,
        "heap construction must stay below quadratic turns"
    );
    assert!(!format!("{sorter:?}").contains("90, 90"));
    let mut backend = Backend {
        source: source(19),
        wrong_seal: Some(2),
        ..Backend::default()
    };
    let mut run = [[0; 8]; 2];
    let mut heads = [[0; 8]; 8];
    let mut sorter = Sorter::new(&mut run, &mut heads).unwrap();
    assert_eq!(drive(&mut sorter, &mut backend), Err(Error::Length));
    assert_eq!(backend.seals, 2);
    assert_eq!(sorter.poll(&mut backend), Err(Error::Failed));
}
#[test]
fn inconsistent_comparator_still_terminates_without_losing_records() {
    let mut backend = Backend {
        source: source(513),
        inconsistent: true,
        ..Backend::default()
    };
    let mut expected = backend.source.clone();
    expected.sort();
    let mut run = [[0; 8]; 7];
    let mut heads = [[0; 8]; 8];
    let mut sorter = Sorter::new(&mut run, &mut heads).unwrap();
    let output = drive(&mut sorter, &mut backend).unwrap();
    let mut actual = backend.values(output.file);
    actual.sort();
    assert_eq!(actual, expected);
}
