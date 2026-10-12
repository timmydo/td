#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;

struct Input {
    bytes: Vec<u8>,
    checks: usize,
    reads: usize,
    read_bytes: usize,
    fail_check: Option<usize>,
    fail_read: bool,
}
impl Input {
    fn new(bytes: impl Into<Vec<u8>>) -> Self {
        Self {
            bytes: bytes.into(),
            checks: 0,
            reads: 0,
            read_bytes: 0,
            fail_check: None,
            fail_read: false,
        }
    }
}
impl Source for Input {
    fn check(&mut self) -> Result<(), Error> {
        self.checks += 1;
        if self.fail_check == Some(self.checks) {
            Err(Error::ExecutionDeadline)
        } else {
            Ok(())
        }
    }
    fn read(&mut self, offset: u64, output: &mut [u8]) -> Result<(), Error> {
        self.reads += 1;
        self.read_bytes += output.len();
        assert!(output.len() <= 4096);
        if self.fail_read {
            return Err(Error::Storage(crate::ports::Error::Capacity));
        }
        output.copy_from_slice(&self.bytes[offset as usize..offset as usize + output.len()]);
        Ok(())
    }
}
#[test]
fn direct_children_preserve_absolute_ranges_and_decoded_keys() {
    let json = r#"{"\u0061":[{"nested":true},null],"β":"escaped\nvalue","last":1e2}"#.as_bytes();
    let mut bytes = b"prefix!".to_vec();
    bytes.extend_from_slice(json);
    bytes.extend_from_slice(b"suffix");
    let mut source = Input::new(bytes);
    let mut walk = Children::new(7..7 + json.len() as u64, scan::Kind::Object).unwrap();
    let mut items = Vec::new();
    loop {
        let (checks, reads) = (source.checks, source.reads);
        let step = walk.poll(&mut source).unwrap();
        assert!(source.checks - checks <= 2);
        assert!(source.reads - reads <= 1);
        match step {
            Progress::Pending => (),
            Progress::Item(item) => items.push((
                item.key.bytes().to_vec(),
                item.kind,
                source.bytes[item.range.start as usize..item.range.end as usize].to_vec(),
            )),
            Progress::Complete => break,
        }
    }
    assert_eq!(
        items,
        vec![
            (
                b"a".to_vec(),
                scan::Kind::Array,
                br#"[{"nested":true},null]"#.to_vec()
            ),
            (
                "β".as_bytes().to_vec(),
                scan::Kind::String,
                br#""escaped\nvalue""#.to_vec()
            ),
            (b"last".to_vec(), scan::Kind::Number, b"1e2".to_vec()),
        ]
    );
    assert_eq!(source.read_bytes, json.len());
    let reads = source.reads;
    assert!(matches!(
        walk.poll(&mut source).unwrap(),
        Progress::Complete
    ));
    assert_eq!(source.reads, reads);
    source.fail_check = Some(source.checks + 1);
    assert!(matches!(
        walk.poll(&mut source),
        Err(Error::ExecutionDeadline)
    ));
}
#[test]
fn large_nested_child_yields_with_one_scanner_turn_and_fixed_cursor_size() {
    let text = "x".repeat(100_000);
    let bytes = format!(r#"[{{"ignored":"{text}"}},false,[]]"#).into_bytes();
    let mut source = Input::new(bytes);
    let mut walk = Children::new(0..source.bytes.len() as u64, scan::Kind::Array).unwrap();
    let mut pending = 0;
    let mut items = 0;
    loop {
        let before = walk.scanner.offset();
        let reads = source.reads;
        match walk.poll(&mut source).unwrap() {
            Progress::Pending => pending += 1,
            Progress::Item(_) => items += 1,
            Progress::Complete => break,
        }
        assert!(walk.scanner.offset() - before <= scan::TURN_BYTES as u64);
        assert!(source.reads - reads <= 1);
    }
    assert!(pending >= 100_000 / scan::TURN_BYTES);
    assert_eq!(items, 3);
    assert!(std::mem::size_of::<Children>() <= 8192);
}
#[test]
fn source_failures_are_sticky_and_postcheck_withholds_an_item() {
    for failure in 1..=10 {
        let mut source = Input::new(b"[true]".to_vec());
        let mut walk = Children::new(0..6, scan::Kind::Array).unwrap();
        source.fail_check = Some(failure);
        loop {
            match walk.poll(&mut source) {
                Err(Error::ExecutionDeadline) => {
                    let counts = (source.reads, source.checks);
                    source.fail_check = None;
                    assert!(matches!(
                        walk.poll(&mut source),
                        Err(Error::ExecutionDeadline)
                    ));
                    assert_eq!((source.reads, source.checks), counts);
                    break;
                }
                Ok(Progress::Complete) => panic!("failure was not reached"),
                Ok(_) => (),
                _ => panic!("wrong result"),
            }
        }
    }
    let mut source = Input::new(b"[true]".to_vec());
    let mut walk = Children::new(0..6, scan::Kind::Array).unwrap();
    // Start array, start scalar, end scalar: the sixth check is after Item creation.
    source.fail_check = Some(6);
    assert!(matches!(walk.poll(&mut source).unwrap(), Progress::Pending));
    assert!(matches!(walk.poll(&mut source).unwrap(), Progress::Pending));
    assert!(matches!(
        walk.poll(&mut source),
        Err(Error::ExecutionDeadline)
    ));
    let mut source = Input::new(b"[]".to_vec());
    source.fail_read = true;
    let mut walk = Children::new(0..2, scan::Kind::Array).unwrap();
    assert!(matches!(walk.next(&mut source), Err(Error::Storage(_))));
    source.fail_read = false;
    assert!(matches!(walk.next(&mut source), Err(Error::Storage(_))));
    assert_eq!(source.reads, 1);
}
#[test]
fn invalid_shapes_ranges_keys_and_corrupt_retained_json_refuse() {
    assert!(Children::new(Range { start: 2, end: 1 }, scan::Kind::Object).is_err());
    assert!(Children::new(0..2, scan::Kind::String).is_err());
    for (bytes, expected, error) in [
        (b"[]".to_vec(), scan::Kind::Object, Error::InvalidArguments),
        (b"[tru]".to_vec(), scan::Kind::Array, Error::State),
        (
            format!("{{\"{}\":0}}", "k".repeat(257)).into_bytes(),
            scan::Kind::Object,
            Error::InvalidArguments,
        ),
    ] {
        let mut source = Input::new(bytes);
        let mut walk = Children::new(0..source.bytes.len() as u64, expected).unwrap();
        assert!(matches!(walk.next(&mut source), Err(e) if e == error));
        let counts = (source.reads, source.checks);
        assert!(matches!(walk.next(&mut source), Err(e) if e == error));
        assert_eq!((source.reads, source.checks), counts);
    }
}

#[test]
fn refills_preserve_escaped_keys_and_child_ranges_from_a_nonzero_origin() {
    let json = format!(
        "{}{{\"\\u0061\":\"{}\",\"end\":null}}",
        " ".repeat(4093),
        "z".repeat(12000)
    );
    let prefix = b"unrelated prefix bytes";
    let mut bytes = prefix.to_vec();
    bytes.extend_from_slice(json.as_bytes());
    bytes.extend_from_slice(b" unrelated suffix");
    let mut source = Input::new(bytes);
    let mut walk = Children::new(
        prefix.len() as u64..(prefix.len() + json.len()) as u64,
        scan::Kind::Object,
    )
    .unwrap();
    let first = walk.next(&mut source).unwrap().unwrap();
    assert_eq!(first.key.bytes(), b"a");
    assert_eq!(first.kind, scan::Kind::String);
    assert_eq!(
        &source.bytes[first.range.start as usize..first.range.end as usize],
        format!("\"{}\"", "z".repeat(12000)).as_bytes()
    );
    let second = walk.next(&mut source).unwrap().unwrap();
    assert_eq!(second.key.bytes(), b"end");
    assert_eq!(second.kind, scan::Kind::Null);
    assert_eq!(
        &source.bytes[second.range.start as usize..second.range.end as usize],
        b"null"
    );
    assert!(walk.next(&mut source).unwrap().is_none());
    assert_eq!(source.read_bytes, json.len());
    assert!(source.reads > 3);
}
