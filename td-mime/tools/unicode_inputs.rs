//! Cold tooling inputs; neither this module nor the corpora enter the service.
#![forbid(unsafe_code)]
use std::{
    fs::File,
    io::{self, Read},
    path::Path,
};
#[path = "../../engine/src/sha256.rs"]
#[allow(dead_code)]
mod sha256;

#[derive(Clone, Copy, Debug)]
pub struct Pin {
    pub name: &'static str,
    pub bytes: u64,
    pub sha256: &'static str,
}
pub const PINS: &[Pin] = &[
    Pin {
        name: "UnicodeData.txt",
        bytes: 2198209,
        sha256: "2e1efc1dcb59c575eedf5ccae60f95229f706ee6d031835247d843c11d96470c",
    },
    Pin {
        name: "DerivedNormalizationProps.txt",
        bytes: 1377582,
        sha256: "71fd6a206a2c0cdd41feb6b7f656aa31091db45e9cedc926985d718397f9e488",
    },
    Pin {
        name: "NormalizationTest.txt",
        bytes: 2827429,
        sha256: "5019ffd530751a741900c849c0e010332f142a3612234639bd200b82138a87db",
    },
    Pin {
        name: "license.txt",
        bytes: 1995,
        sha256: "e7a93b009565cfce55919a381437ac4db883e9da2126fa28b91d12732bc53d96",
    },
];

pub struct Input {
    pub pin: Pin,
    pub text: String,
}

pub fn read(directory: &Path, pin: Pin) -> io::Result<Input> {
    let path = directory.join(pin.name);
    let metadata = std::fs::symlink_metadata(&path).map_err(|e| context(pin, "metadata", e))?;
    if !metadata.is_file() || metadata.len() != pin.bytes {
        return Err(invalid(pin, "expected regular file with pinned length"));
    }
    let file = File::open(&path).map_err(|e| context(pin, "open", e))?;
    read_opened(file, pin)
}

pub fn read_opened(file: File, pin: Pin) -> io::Result<Input> {
    let metadata = file
        .metadata()
        .map_err(|e| context(pin, "opened metadata", e))?;
    if !metadata.is_file() || metadata.len() != pin.bytes {
        return Err(invalid(pin, "opened file differs from pinned length/type"));
    }
    read_content(file, pin)
}

pub fn read_content(reader: impl Read, pin: Pin) -> io::Result<Input> {
    let limit = pin
        .bytes
        .checked_add(1)
        .ok_or_else(|| invalid(pin, "size overflow"))?;
    let capacity = usize::try_from(limit).map_err(|_| invalid(pin, "size exceeds host range"))?;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(capacity).map_err(|e| {
        context(
            pin,
            "reserve",
            io::Error::new(io::ErrorKind::OutOfMemory, e),
        )
    })?;
    reader
        .take(limit)
        .read_to_end(&mut bytes)
        .map_err(|e| context(pin, "read", e))?;
    if bytes.len() as u64 != pin.bytes {
        return Err(invalid(pin, "source length changed"));
    }
    let mut digest = sha256::Sha256::new();
    digest.update(&bytes);
    let value = digest.finalize();
    let mut actual = String::new();
    use std::fmt::Write;
    for byte in value {
        write!(&mut actual, "{byte:02x}")
            .map_err(|e| context(pin, "digest formatting", io::Error::other(e)))?;
    }
    if actual != pin.sha256 {
        return Err(invalid(pin, "SHA-256 differs from approved pin"));
    }
    let text = String::from_utf8(bytes)
        .map_err(|e| context(pin, "UTF-8", io::Error::new(io::ErrorKind::InvalidData, e)))?;
    Ok(Input { pin, text })
}

pub fn load(directory: &Path) -> io::Result<Vec<Input>> {
    let mut inputs = Vec::new();
    inputs
        .try_reserve_exact(PINS.len())
        .map_err(|e| io::Error::new(io::ErrorKind::OutOfMemory, e))?;
    for &pin in PINS {
        inputs.push(read(directory, pin)?);
    }
    Ok(inputs)
}
fn invalid(pin: Pin, reason: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("{}: {reason}", pin.name),
    )
}

fn context(pin: Pin, stage: &str, error: io::Error) -> io::Error {
    io::Error::new(error.kind(), format!("{}: {stage}: {error}", pin.name))
}
