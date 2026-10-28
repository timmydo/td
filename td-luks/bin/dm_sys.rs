//! Device-mapper ioctl ABI; the binary's single recorded syscall boundary.
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};

const SYS_IOCTL: usize = 16;
const DM_VERSION: usize = 0xc138_fd00;
const DM_DEV_CREATE: usize = 0xc138_fd03;
const DM_DEV_REMOVE: usize = 0xc138_fd04;
const DM_DEV_SUSPEND: usize = 0xc138_fd06;
const DM_DEV_STATUS: usize = 0xc138_fd07;
const DM_TABLE_LOAD: usize = 0xc138_fd09;
const DM_TABLE_CLEAR: usize = 0xc138_fd0a;
const DM_TABLE_DEPS: usize = 0xc138_fd0b;
const DM_TABLE_STATUS: usize = 0xc138_fd0c;
// sizeof(dm_ioctl) includes seven trailing padding bytes; replies may
// stop at offsetof(data), immediately after the 129-byte UUID.
const HEADER: usize = 312;
const MIN_REPLY: usize = 305;
const CAPACITY: usize = 65536;
const TARGET: usize = 40;
const SUSPEND: u32 = 1 << 1;
const STATUS_TABLE: u32 = 1 << 4;
const ACTIVE: u32 = 1 << 5;
const BUFFER_FULL: u32 = 1 << 8;
const SKIP_LOCKFS: u32 = 1 << 10;
const NOFLUSH: u32 = 1 << 11;
const SECURE_DATA: u32 = 1 << 15;

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() < 128
        && !matches!(name, "." | ".." | "control")
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'+'))
}
pub fn device_number(raw: u64) -> (u32, u32) {
    (
        (((raw >> 8) & 0xfff) | ((raw >> 32) & 0xfffff000)) as u32,
        ((raw & 0xff) | ((raw >> 12) & 0xffffff00)) as u32,
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Request {
    Version,
    Create,
    Remove,
    Suspend,
    Status,
    Load,
    Clear,
    Deps,
    Table,
}
impl Request {
    fn number(self) -> usize {
        match self {
            Self::Version => DM_VERSION,
            Self::Create => DM_DEV_CREATE,
            Self::Remove => DM_DEV_REMOVE,
            Self::Suspend => DM_DEV_SUSPEND,
            Self::Status => DM_DEV_STATUS,
            Self::Load => DM_TABLE_LOAD,
            Self::Clear => DM_TABLE_CLEAR,
            Self::Deps => DM_TABLE_DEPS,
            Self::Table => DM_TABLE_STATUS,
        }
    }
}
#[repr(align(8))]
struct Packet {
    bytes: [u8; CAPACITY],
}
impl Drop for Packet {
    fn drop(&mut self) {
        self.bytes.fill(0);
        std::hint::black_box(&mut self.bytes);
    }
}
impl Packet {
    fn new(name: &str) -> io::Result<Box<Self>> {
        let mut p = Box::new(Self {
            bytes: [0; CAPACITY],
        });
        p.u32(0, 4)?;
        p.u32(12, CAPACITY as u32)?;
        p.u32(16, HEADER as u32)?;
        p.u32(28, SECURE_DATA)?;
        p.text(48, 128, name)?;
        Ok(p)
    }
    fn put(&mut self, at: usize, data: &[u8]) -> io::Result<()> {
        let end = at
            .checked_add(data.len())
            .ok_or_else(|| invalid("dm packet overflow"))?;
        self.bytes
            .get_mut(at..end)
            .ok_or_else(|| invalid("dm packet bounds"))?
            .copy_from_slice(data);
        Ok(())
    }
    fn u32(&mut self, at: usize, value: u32) -> io::Result<()> {
        self.put(at, &value.to_le_bytes())
    }
    fn u64(&mut self, at: usize, value: u64) -> io::Result<()> {
        self.put(at, &value.to_le_bytes())
    }
    fn text(&mut self, at: usize, len: usize, value: &str) -> io::Result<()> {
        if value.len() >= len || value.as_bytes().contains(&0) {
            return Err(invalid("dm text field too long or contains NUL"));
        }
        self.put(at, value.as_bytes())
    }
    fn get(&self, at: usize, len: usize) -> io::Result<&[u8]> {
        let end = at
            .checked_add(len)
            .ok_or_else(|| invalid("dm reply overflow"))?;
        self.bytes
            .get(at..end)
            .ok_or_else(|| invalid("dm reply bounds"))
    }
    fn get32(&self, at: usize) -> io::Result<u32> {
        Ok(u32::from_le_bytes(
            self.get(at, 4)?.try_into().map_err(|_| invalid("dm u32"))?,
        ))
    }
    fn get64(&self, at: usize) -> io::Result<u64> {
        Ok(u64::from_le_bytes(
            self.get(at, 8)?.try_into().map_err(|_| invalid("dm u64"))?,
        ))
    }
    fn string(&self, at: usize, len: usize) -> io::Result<&str> {
        let bytes = self.get(at, len)?;
        let end = bytes
            .iter()
            .position(|b| *b == 0)
            .ok_or_else(|| invalid("unterminated dm string"))?;
        std::str::from_utf8(
            bytes
                .get(..end)
                .ok_or_else(|| invalid("dm string bounds"))?,
        )
        .map_err(|_| invalid("non-UTF-8 dm string"))
    }
    fn payload(&self) -> io::Result<(usize, usize)> {
        let end = self.get32(12)? as usize;
        let start = self.get32(16)? as usize;
        if start < HEADER || start > end || end > CAPACITY {
            return Err(invalid("invalid dm payload bounds"));
        }
        Ok((start, end))
    }
    fn crypt(name: &str, length: u64, device: (u32, u32), key: &[u8; 64]) -> io::Result<Box<Self>> {
        if length == 0 || !length.is_multiple_of(8) {
            return Err(invalid(
                "crypt target length must be whole 4096-byte sectors",
            ));
        }
        let mut p = Self::new(name)?;
        p.u32(20, 1)?;
        p.u64(HEADER, 0)?;
        p.u64(HEADER + 8, length)?;
        p.text(HEADER + 24, 16, "crypt")?;
        let mut at = HEADER + TARGET;
        let prefix = b"aes-xts-plain64 ";
        p.put(at, prefix)?;
        at += prefix.len();
        for byte in key {
            let hex = b"0123456789abcdef";
            let high = *hex
                .get(usize::from(byte >> 4))
                .ok_or_else(|| invalid("hex digit"))?;
            let low = *hex
                .get(usize::from(byte & 15))
                .ok_or_else(|| invalid("hex digit"))?;
            p.put(at, &[high, low])?;
            at += 2;
        }
        let tail = format!(" 0 {}:{} 32768 1 sector_size:4096", device.0, device.1);
        p.put(at, tail.as_bytes())?;
        at += tail.len() + 1;
        let next = (at - HEADER + 7) & !7;
        p.u32(HEADER + 20, next as u32)?;
        Ok(p)
    }
    fn crypt_params(&self) -> io::Result<(&[u8], u64)> {
        let (at, end) = self.payload()?;
        if self.get32(20)? != 1 || self.get64(at)? != 0 || self.string(at + 24, 16)? != "crypt" {
            return Err(invalid("mapping is not one crypt target"));
        }
        let start = at
            .checked_add(TARGET)
            .filter(|v| *v <= end)
            .ok_or_else(|| invalid("short crypt target"))?;
        let bytes = self.get(start, end - start)?;
        let len = bytes
            .iter()
            .position(|b| *b == 0)
            .ok_or_else(|| invalid("unterminated crypt parameters"))?;
        Ok((
            bytes
                .get(..len)
                .ok_or_else(|| invalid("crypt parameters"))?,
            self.get64(at + 8)?,
        ))
    }
}

#[inline]
#[allow(unsafe_code)]
fn syscall3(number: usize, fd: usize, request: usize, buffer: usize) -> isize {
    let result: isize;
    // SAFETY: the sole caller supplies a borrowed live File and a live,
    // aligned owned packet covering data_size. The kernel reads and writes
    // that packet; there is deliberately no nomem or readonly promise.
    unsafe {
        core::arch::asm!(
            "syscall",
            inlateout("rax") number as isize => result,
            in("rdi") fd,
            in("rsi") request,
            in("rdx") buffer,
            out("rcx") _,
            out("r11") _,
            options(nostack),
        );
    }
    result
}

pub struct Status {
    pub device: u64,
    pub name: String,
    pub uuid: String,
    pub active: bool,
    pub suspended: bool,
}
pub struct Control {
    file: File,
}
impl Control {
    pub fn open() -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(0o400_000)
            .open("/dev/mapper/control")?;
        let meta = file.metadata()?;
        if !meta.file_type().is_char_device() || device_number(meta.rdev()) != (10, 236) {
            return Err(invalid("not the device-mapper control device"));
        }
        let control = Self { file };
        let p = control.request(Request::Version, "")?;
        if p.get32(0)? != 4 || p.get32(4)? < 20 {
            return Err(invalid("device-mapper lacks the required secure-data ABI"));
        }
        Ok(control)
    }
    fn invoke(&self, request: Request, packet: &mut Packet) -> io::Result<()> {
        let result = syscall3(
            SYS_IOCTL,
            self.file.as_raw_fd() as usize,
            request.number(),
            packet.bytes.as_mut_ptr() as usize,
        );
        if result < 0 {
            return Err(io::Error::from_raw_os_error(
                result
                    .checked_neg()
                    .and_then(|v| i32::try_from(v).ok())
                    .unwrap_or(i32::MAX),
            ));
        }
        let size = packet.get32(12)? as usize;
        if !(MIN_REPLY..=CAPACITY).contains(&size) || packet.get32(28)? & BUFFER_FULL != 0 {
            return Err(invalid("device-mapper reply exceeds packet bound"));
        }
        Ok(())
    }
    fn request(&self, request: Request, name: &str) -> io::Result<Box<Packet>> {
        if request != Request::Version && !valid_name(name) {
            return Err(invalid("invalid mapping name"));
        }
        let mut p = Packet::new(name)?;
        self.invoke(request, &mut p)?;
        Ok(p)
    }
    pub fn status(&self, name: &str) -> io::Result<Option<Status>> {
        let p = match self.request(Request::Status, name) {
            Ok(p) => p,
            Err(e) if e.raw_os_error() == Some(6) => return Ok(None),
            Err(e) => return Err(e),
        };
        let flags = p.get32(28)?;
        Ok(Some(Status {
            device: p.get64(40)?,
            name: p.string(48, 128)?.into(),
            uuid: p.string(176, 129)?.into(),
            active: flags & ACTIVE != 0,
            suspended: flags & SUSPEND != 0,
        }))
    }
    pub fn set_suspended(&self, name: &str, suspend: bool) -> io::Result<()> {
        if !valid_name(name) {
            return Err(invalid("invalid mapping name"));
        }
        let mut p = Packet::new(name)?;
        p.u32(
            28,
            SECURE_DATA
                | if suspend {
                    SUSPEND | NOFLUSH | SKIP_LOCKFS
                } else {
                    0
                },
        )?;
        self.invoke(Request::Suspend, &mut p)
    }
    pub fn dependencies(&self, name: &str) -> io::Result<Vec<u64>> {
        let p = self.request(Request::Deps, name)?;
        let (at, end) = p.payload()?;
        let count = p.get32(at)? as usize;
        if count > 256 || at.checked_add(8 + count * 8).is_none_or(|n| n > end) {
            return Err(invalid("invalid dependency list"));
        }
        (0..count).map(|i| p.get64(at + 8 + i * 8)).collect()
    }
    pub fn remove(&self, name: &str) -> io::Result<()> {
        self.request(Request::Remove, name).map(drop)
    }
    pub fn matches_crypt(
        &self,
        name: &str,
        length: u64,
        device: (u32, u32),
        key: &[u8; 64],
    ) -> io::Result<bool> {
        if !valid_name(name) {
            return Err(invalid("invalid mapping name"));
        }
        let expected = Packet::crypt(name, length, device, key)?;
        let mut actual = Packet::new(name)?;
        actual.u32(28, SECURE_DATA | STATUS_TABLE)?;
        self.invoke(Request::Table, &mut actual)?;
        Ok(actual.crypt_params()? == expected.crypt_params()?)
    }
    pub fn create_crypt(
        &self,
        name: &str,
        uuid: &str,
        length: u64,
        device: (u32, u32),
        key: &[u8; 64],
    ) -> io::Result<Status> {
        if !valid_name(name) {
            return Err(invalid("invalid mapping name"));
        }
        let mut table = Packet::crypt(name, length, device, key)?;
        let mut create = Packet::new(name)?;
        create.text(176, 129, uuid)?;
        self.invoke(Request::Create, &mut create)?;
        let result = (|| {
            self.invoke(Request::Load, &mut table)?;
            self.set_suspended(name, false)?;
            let status = self
                .status(name)?
                .ok_or_else(|| invalid("created mapping disappeared"))?;
            if !status.active
                || status.suspended
                || status.uuid != uuid
                || status.name != name
                || !self.matches_crypt(name, length, device, key)?
            {
                return Err(invalid("created mapping differs from requested table"));
            }
            let deps = self.dependencies(name)?;
            if deps.len() != 1 || deps.first().map(|d| device_number(*d)) != Some(device) {
                return Err(invalid("created mapping has unexpected dependencies"));
            }
            Ok(status)
        })();
        if result.is_err() {
            let clear = self.request(Request::Clear, name);
            let remove = self.remove(name);
            if clear.is_err() || remove.is_err() {
                return Err(invalid(
                    "mapping activation failed and cleanup did not complete",
                ));
            }
        }
        result
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_packet_has_the_pinned_crypt_parameters_and_no_other_options() {
        let key = std::array::from_fn(|i| i as u8);
        let p = Packet::crypt("volume", 4096, (259, 17), &key).unwrap();
        assert_eq!(
            (
                p.get32(0).unwrap(),
                p.get32(12).unwrap(),
                p.get32(16).unwrap()
            ),
            (4, CAPACITY as u32, HEADER as u32)
        );
        assert_eq!(p.get32(28).unwrap(), SECURE_DATA);
        let (params, length) = p.crypt_params().unwrap();
        let hex: String = key.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            params,
            format!("aes-xts-plain64 {hex} 0 259:17 32768 1 sector_size:4096").as_bytes()
        );
        assert_eq!(length, 4096);
        assert_eq!(p.string(48, 128).unwrap(), "volume");
        assert_eq!(p.get32(HEADER + 20).unwrap() % 8, 0);
        assert!(Packet::crypt("volume", 7, (1, 1), &key).is_err());
        assert!(Packet::crypt("volume", 0, (1, 1), &key).is_err());
    }

    #[test]
    fn bounded_parsers_refuse_truncation_and_unterminated_fields() {
        let mut p = Packet::crypt("volume", 4096, (1, 1), &[0; 64]).unwrap();
        p.u32(12, (HEADER + TARGET) as u32).unwrap();
        assert!(p.crypt_params().is_err());
        p.u32(12, (CAPACITY + 1) as u32).unwrap();
        assert!(p.payload().is_err());
        p.u32(12, CAPACITY as u32).unwrap();
        p.u32(16, (HEADER - 1) as u32).unwrap();
        assert!(p.payload().is_err());
        p.bytes[48..176].fill(b'x');
        assert!(p.string(48, 128).is_err());
        assert!(p.get(usize::MAX, 2).is_err());
        assert!(p.put(usize::MAX, &[1]).is_err());
    }

    #[test]
    fn mapping_names_cannot_address_another_filesystem_entry() {
        for name in [
            "",
            ".",
            "..",
            "control",
            "/dev/dm-0",
            "a/b",
            "name\n",
            "a b",
            "💾",
        ] {
            assert!(!valid_name(name), "{name:?}");
        }
        assert!(!valid_name(&"a".repeat(128)));
        for name in ["td-system", "a_b.c+1", "0"] {
            assert!(valid_name(name));
        }
        assert_eq!(device_number(0xfd00), (253, 0));
        assert_eq!(device_number(0x10301), (259, 1));
    }
}
