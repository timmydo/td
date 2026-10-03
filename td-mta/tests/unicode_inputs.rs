#![forbid(unsafe_code)]
#![cfg(test)]
#![allow(clippy::unwrap_used, clippy::panic)]
#[path = "../tools/unicode_inputs.rs"]
mod inputs;
use std::{
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};
fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("unicode/17.0.0")
}

#[test]
fn committed_corpus_matches_every_approved_pin() {
    let loaded = inputs::load(&root()).unwrap();
    assert_eq!(loaded.len(), 4);
    assert_eq!(
        loaded.iter().map(|i| i.text.len()).sum::<usize>(),
        6_405_215
    );
    for input in &loaded {
        assert_eq!(input.text.len() as u64, input.pin.bytes);
    }
    let tests = loaded
        .iter()
        .find(|i| i.pin.name == "NormalizationTest.txt")
        .unwrap();
    assert!(tests.text.starts_with("# NormalizationTest-17.0.0.txt\n"));
}

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "td-mta-unicode-inputs-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
#[test]
fn missing_tampered_wrong_size_and_non_file_inputs_refuse() {
    let temp = Scratch::new();
    let pin = *inputs::PINS.first().unwrap();
    let path = temp.0.join(pin.name);
    assert_eq!(
        inputs::load(&temp.0).err().unwrap().kind(),
        std::io::ErrorKind::NotFound
    );
    let bytes = std::fs::read(root().join(pin.name)).unwrap();
    std::fs::write(&path, &bytes).unwrap();
    assert_eq!(inputs::read(&temp.0, pin).unwrap().text.as_bytes(), bytes);
    let mut changed = bytes;
    *changed.first_mut().unwrap() ^= 1;
    std::fs::write(&path, &changed).unwrap();
    let error = inputs::read(&temp.0, pin).err().unwrap();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert!(error.to_string().contains("SHA-256"));
    changed.pop();
    std::fs::write(&path, &changed).unwrap();
    let opened = std::fs::File::open(&path).unwrap();
    assert!(inputs::read_opened(opened, pin)
        .err()
        .unwrap()
        .to_string()
        .contains("opened file differs"));
    let error = inputs::read(&temp.0, pin).err().unwrap();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert!(error.to_string().contains("expected regular file"));
    changed.extend_from_slice(b"xx");
    std::fs::write(&path, &changed).unwrap();
    let opened = std::fs::File::open(&path).unwrap();
    assert!(inputs::read_opened(opened, pin)
        .err()
        .unwrap()
        .to_string()
        .contains("opened file differs"));
    let error = inputs::read(&temp.0, pin).err().unwrap();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert!(error.to_string().contains("expected regular file"));
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    let opened = std::fs::File::open(&path).unwrap();
    assert!(inputs::read_opened(opened, pin)
        .err()
        .unwrap()
        .to_string()
        .contains("opened file differs"));
    let error = inputs::read(&temp.0, pin).err().unwrap();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    assert!(error.to_string().contains("expected regular file"));
    std::fs::remove_dir(&path).unwrap();
    std::os::unix::fs::symlink(root().join(pin.name), &path).unwrap();
    assert!(inputs::read(&temp.0, pin)
        .err()
        .unwrap()
        .to_string()
        .contains("expected regular file"));
}

#[test]
fn opened_streams_are_bounded_and_report_context() {
    use std::io::{self, Read};
    let pin = inputs::Pin {
        name: "test.txt",
        bytes: 3,
        sha256: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
    };
    assert_eq!(inputs::read_content(&b"abc"[..], pin).unwrap().text, "abc");
    assert!(inputs::read_content(&b"ab"[..], pin)
        .err()
        .unwrap()
        .to_string()
        .contains("source length changed"));
    struct Excess(usize);
    impl Read for Excess {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            let count = out.len().min(16 - self.0);
            out.get_mut(..count).unwrap().fill(b'a');
            self.0 += count;
            Ok(count)
        }
    }
    let mut reader = Excess(0);
    assert!(inputs::read_content(&mut reader, pin)
        .err()
        .unwrap()
        .to_string()
        .contains("source length changed"));
    assert_eq!(reader.0, 4);
    let invalid_text = inputs::Pin {
        name: "test.txt",
        bytes: 1,
        sha256: "a8100ae6aa1940d0b663bb31cd466142ebbdbd5187131b92d93818987832eb89",
    };
    let error = inputs::read_content(&b"\xff"[..], invalid_text)
        .err()
        .unwrap();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert!(error.to_string().starts_with("test.txt: UTF-8:"));
    struct Broken;
    impl Read for Broken {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::PermissionDenied, "injected"))
        }
    }
    let error = inputs::read_content(Broken, pin).err().unwrap();
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    assert_eq!(error.to_string(), "test.txt: read: injected");
}
