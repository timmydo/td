//! Linux source reads and runtime ABI conversion, outside pure metric parsers.
use crate::budget::{Budget, Charge};
use std::fmt::Write as _;
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::path::Path;
use std::sync::Arc;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Units {
    pub page_size: u64,
    pub ticks_per_second: u64,
}
/// The caller charges this fixed storage before constructing a worker reader.
#[derive(Debug)]
pub struct Reader {
    bytes: Vec<u8>,
    process_path: String,
    _charge: Charge,
}
impl Reader {
    pub fn new(budget: &Arc<Budget>, maximum: usize) -> io::Result<Self> {
        if maximum == 0 || maximum > 1024 * 1024 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid source reader bound",
            ));
        }
        let requested = maximum + 1 + 64 + std::mem::size_of::<Self>();
        let mut charge = budget.charge(requested).map_err(io::Error::other)?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(maximum + 1)
            .map_err(|_| io::Error::other("source buffer allocation failed"))?;
        bytes.resize(maximum + 1, 0);
        let mut process_path = String::new();
        process_path
            .try_reserve_exact(64)
            .map_err(|_| io::Error::other("source path allocation failed"))?;
        let actual = bytes.capacity() + process_path.capacity() + std::mem::size_of::<Self>();
        charge
            .additional(actual.saturating_sub(requested))
            .map_err(io::Error::other)?;
        Ok(Self {
            bytes,
            process_path,
            _charge: charge,
        })
    }
    pub fn storage_bytes(&self) -> usize {
        self.bytes.capacity() + self.process_path.capacity() + std::mem::size_of::<Self>()
    }
    pub fn read(&mut self, mut source: impl Read, limit: usize) -> io::Result<&[u8]> {
        if limit == 0 || limit >= self.bytes.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "read exceeds reserved source bound",
            ));
        }
        let mut used = 0;
        let mut interruptions = 0;
        loop {
            let target = self
                .bytes
                .get_mut(used..limit + 1)
                .ok_or_else(|| io::Error::other("invalid source buffer range"))?;
            match source.read(target) {
                Ok(0) => {
                    return self
                        .bytes
                        .get(..used)
                        .ok_or_else(|| io::Error::other("invalid source result range"))
                }
                Ok(count) => {
                    used = used
                        .checked_add(count)
                        .ok_or_else(|| io::Error::other("source length overflow"))?;
                    if used > limit {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "source exceeds byte limit",
                        ));
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                    interruptions += 1;
                    if interruptions > 32 {
                        return Err(error);
                    }
                }
                Err(error) => return Err(error),
            }
        }
    }
    pub fn path(&mut self, path: &Path, limit: usize) -> io::Result<&[u8]> {
        self.read(File::open(path)?, limit)
    }
    pub fn process_directory(&mut self, pid: u32) -> io::Result<File> {
        if pid == 0 {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        self.process_path.clear();
        write!(&mut self.process_path, "/proc/{pid}")
            .map_err(|_| io::Error::other("source path formatting failed"))?;
        File::open(&self.process_path)
    }
    /// A bounded command prefix; one extra byte establishes truncation.
    pub fn process_command(&mut self, directory: &File) -> io::Result<(&[u8], bool)> {
        self.process_path.clear();
        write!(
            &mut self.process_path,
            "/proc/self/fd/{}/cmdline",
            directory.as_raw_fd()
        )
        .map_err(|_| io::Error::other("source path formatting failed"))?;
        let mut source = File::open(&self.process_path)?;
        let limit = 4096usize;
        if self.bytes.len() <= limit {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let mut used = 0;
        let mut interrupted = 0;
        while used <= limit {
            let target = self
                .bytes
                .get_mut(used..limit + 1)
                .ok_or_else(|| io::Error::other("invalid command buffer range"))?;
            match source.read(target) {
                Ok(0) => break,
                Ok(count) => {
                    used = used
                        .checked_add(count)
                        .ok_or_else(|| io::Error::other("command length overflow"))?
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                    interrupted += 1;
                    if interrupted > 32 {
                        return Err(error);
                    }
                }
                Err(error) => return Err(error),
            }
        }
        let bytes = self
            .bytes
            .get(..used.min(limit))
            .ok_or_else(|| io::Error::other("invalid command prefix range"))?;
        Ok((bytes, used > limit))
    }
    /// `name` is a fixed component chosen by the adapter, never process text.
    pub fn process_file(
        &mut self,
        directory: &File,
        name: &str,
        limit: usize,
    ) -> io::Result<&[u8]> {
        if !matches!(name, "stat" | "status" | "cmdline") {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unsupported process source",
            ));
        }
        // Resolving the retained procfs directory cannot follow later PID reuse.
        self.process_path.clear();
        // Fixed prefix, an i32 descriptor and a whitelisted name fit 64 bytes.
        write!(
            &mut self.process_path,
            "/proc/self/fd/{}/{name}",
            directory.as_raw_fd()
        )
        .map_err(|_| io::Error::other("source path formatting failed"))?;
        let source = File::open(&self.process_path)?;
        self.read(source, limit)
    }
}
/// Native-width, native-endian auxv records belong only in this ABI adapter.
pub fn auxv(bytes: &[u8]) -> io::Result<Units> {
    let malformed = || {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid runtime auxiliary vector",
        )
    };
    let word = std::mem::size_of::<usize>();
    if bytes.len() > 4096 || !bytes.len().is_multiple_of(2 * word) {
        return Err(malformed());
    }
    let mut page = None;
    let mut ticks = None;
    let mut ended = false;
    for pair in bytes.chunks_exact(2 * word) {
        let kind = pair
            .get(..word)
            .and_then(|b| b.try_into().ok())
            .map(usize::from_ne_bytes)
            .ok_or_else(malformed)?;
        let value = pair
            .get(word..)
            .and_then(|b| b.try_into().ok())
            .map(usize::from_ne_bytes)
            .ok_or_else(malformed)?;
        if kind == 0 {
            ended = true;
            break;
        }
        let slot = match kind {
            6 => &mut page,
            17 => &mut ticks,
            _ => continue,
        };
        if slot.replace(value as u64).is_some() {
            return Err(malformed());
        }
    }
    let page_size = page.filter(|n| *n > 0).ok_or_else(malformed)?;
    let ticks_per_second = ticks.filter(|n| *n > 0).ok_or_else(malformed)?;
    if !ended {
        return Err(malformed());
    }
    Ok(Units {
        page_size,
        ticks_per_second,
    })
}
pub fn runtime_units(reader: &mut Reader) -> io::Result<Units> {
    auxv(reader.path(Path::new("/proc/self/auxv"), 4096)?)
}
