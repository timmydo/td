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
type TargetParameters<'a> = (u64, u64, &'a str, &'a [u8]);
const READ_ONLY: u32 = 1;
const SUSPEND: u32 = 1 << 1;
const STATUS_TABLE: u32 = 1 << 4;
const ACTIVE: u32 = 1 << 5;
const BUFFER_FULL: u32 = 1 << 8;
const SKIP_LOCKFS: u32 = 1 << 10;
const NOFLUSH: u32 = 1 << 11;
const SECURE_DATA: u32 = 1 << 15;
const DEFERRED_REMOVE: u32 = 1 << 17;

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
    #[cfg(test)]
    fn crypt(name: &str, length: u64, device: (u32, u32), key: &[u8; 64]) -> io::Result<Box<Self>> {
        Self::table(
            name,
            &[Target {
                start: 0,
                length,
                device,
                offset: 32768,
                kind: Kind::Crypt { key, iv: 0 },
            }],
        )
    }
    fn table(name: &str, targets: &[Target<'_>]) -> io::Result<Box<Self>> {
        if targets.is_empty() || targets.len() > 3 {
            return Err(invalid("a table has one to three targets"));
        }
        let mut p = Self::new(name)?;
        p.u32(20, targets.len() as u32)?;
        let mut at = HEADER;
        let mut cursor = 0u64;
        for target in targets {
            if target.start != cursor
                || target.length == 0
                || !target.length.is_multiple_of(8)
                || !target.offset.is_multiple_of(8)
                || target.offset.checked_add(target.length).is_none()
            {
                return Err(invalid("target bounds or alignment"));
            }
            cursor = cursor
                .checked_add(target.length)
                .ok_or_else(|| invalid("table length overflow"))?;
            p.u64(at, target.start)?;
            p.u64(at + 8, target.length)?;
            let mut text = at + TARGET;
            match target.kind {
                Kind::Linear => {
                    p.text(at + 24, 16, "linear")?;
                    let value =
                        format!("{}:{} {}", target.device.0, target.device.1, target.offset);
                    p.put(text, value.as_bytes())?;
                    text += value.len();
                }
                Kind::Crypt { key, iv } => {
                    if !iv.is_multiple_of(8) || iv.checked_add(target.length).is_none() {
                        return Err(invalid("crypt IV bounds"));
                    }
                    p.text(at + 24, 16, "crypt")?;
                    let prefix = b"aes-xts-plain64 ";
                    p.put(text, prefix)?;
                    text += prefix.len();
                    for byte in key {
                        let hex = b"0123456789abcdef";
                        let high = *hex
                            .get(usize::from(byte >> 4))
                            .ok_or_else(|| invalid("hex digit"))?;
                        let low = *hex
                            .get(usize::from(byte & 15))
                            .ok_or_else(|| invalid("hex digit"))?;
                        p.put(text, &[high, low])?;
                        text += 2;
                    }
                    let tail = format!(
                        " {iv} {}:{} {} 1 sector_size:4096",
                        target.device.0, target.device.1, target.offset
                    );
                    p.put(text, tail.as_bytes())?;
                    text += tail.len();
                }
            }
            let next = (text + 1 + 7) & !7;
            p.u32(at + 20, (next - at) as u32)?;
            at = next;
        }
        Ok(p)
    }
    fn target_params(&self, reply: bool) -> io::Result<Vec<TargetParameters<'_>>> {
        let (base, end) = self.payload()?;
        let count = self.get32(20)? as usize;
        if !(1..=3).contains(&count) {
            return Err(invalid("target count outside bound"));
        }
        let mut at = base;
        let mut out = Vec::with_capacity(count);
        for index in 0..count {
            let start = at
                .checked_add(TARGET)
                .filter(|n| *n <= end)
                .ok_or_else(|| invalid("short target"))?;
            // The final reply next is padded while data_size ends at the
            // parameter NUL. Never read those up-to-seven padding bytes.
            let limit = if reply && index + 1 == count {
                (end + 7) & !7
            } else {
                end
            };
            let next = (if reply { base } else { at })
                .checked_add(self.get32(at + 20)? as usize)
                .filter(|n| *n > start && *n <= limit && n.is_multiple_of(8))
                .ok_or_else(|| invalid("bad target next offset"))?;
            let bytes = self.get(start, next.min(end) - start)?;
            let len = bytes
                .iter()
                .position(|b| *b == 0)
                .ok_or_else(|| invalid("unterminated target parameters"))?;
            let kind = self.string(at + 24, 16)?;
            if !matches!(kind, "crypt" | "linear") {
                return Err(invalid("unknown target type"));
            }
            out.push((
                self.get64(at)?,
                self.get64(at + 8)?,
                kind,
                bytes
                    .get(..len)
                    .ok_or_else(|| invalid("parameter bounds"))?,
            ));
            at = next;
        }
        Ok(out)
    }

    #[cfg(test)]
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

pub enum Kind<'a> {
    Crypt { key: &'a [u8; 64], iv: u64 },
    Linear,
}
pub struct Target<'a> {
    pub start: u64,
    pub length: u64,
    pub device: (u32, u32),
    pub offset: u64,
    pub kind: Kind<'a>,
}
pub struct Status {
    pub device: u64,
    pub name: String,
    pub uuid: String,
    pub active: bool,
    pub suspended: bool,
    pub read_only: bool,
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
            read_only: flags & READ_ONLY != 0,
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
    pub fn defer_remove(&self, name: &str) -> io::Result<()> {
        if !valid_name(name) {
            return Err(invalid("invalid mapping name"));
        }
        let version = self.request(Request::Version, "")?;
        if version.get32(4)? < 27 {
            return Err(invalid("device mapper lacks deferred removal"));
        }
        let mut p = Packet::new(name)?;
        p.u32(28, SECURE_DATA | DEFERRED_REMOVE)?;
        self.invoke(Request::Remove, &mut p)
    }
    pub fn remove(&self, name: &str) -> io::Result<()> {
        self.request(Request::Remove, name).map(drop)
    }
    pub fn matches_table(&self, name: &str, targets: &[Target<'_>]) -> io::Result<bool> {
        if !valid_name(name) {
            return Err(invalid("invalid mapping name"));
        }
        let expected = Packet::table(name, targets)?;
        let mut actual = Packet::new(name)?;
        actual.u32(28, SECURE_DATA | STATUS_TABLE)?;
        self.invoke(Request::Table, &mut actual)?;
        Ok(actual.target_params(true)? == expected.target_params(false)?)
    }
    pub fn load_table(&self, name: &str, targets: &[Target<'_>]) -> io::Result<()> {
        if !valid_name(name) {
            return Err(invalid("invalid mapping name"));
        }
        let mut table = Packet::table(name, targets)?;
        self.invoke(Request::Load, &mut table)
    }
    pub fn create_table(
        &self,
        name: &str,
        uuid: &str,
        targets: &[Target<'_>],
    ) -> io::Result<Status> {
        self.create_table_mode(name, uuid, targets, false)
    }
    fn create_table_mode(
        &self,
        name: &str,
        uuid: &str,
        targets: &[Target<'_>],
        read_only: bool,
    ) -> io::Result<Status> {
        if !valid_name(name) {
            return Err(invalid("invalid mapping name"));
        }
        let mut table = Packet::table(name, targets)?;
        table.u32(28, SECURE_DATA | if read_only { READ_ONLY } else { 0 })?;
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
                || status.read_only != read_only
                || status.uuid != uuid
                || status.name != name
                || !self.matches_table(name, targets)?
            {
                return Err(invalid("created table differs"));
            }
            let mut wanted: Vec<_> = targets.iter().map(|t| t.device).collect();
            wanted.sort_unstable();
            wanted.dedup();
            let mut actual: Vec<_> = self
                .dependencies(name)?
                .into_iter()
                .map(device_number)
                .collect();
            actual.sort_unstable();
            actual.dedup();
            if wanted != actual {
                return Err(invalid("created dependencies differ"));
            }
            Ok(status)
        })();
        if result.is_err() {
            let clear = self.request(Request::Clear, name);
            let remove = self.remove(name);
            if clear.is_err() || remove.is_err() {
                return Err(invalid("table failed and cleanup incomplete"));
            }
        }
        result
    }
    pub fn matches_crypt(
        &self,
        name: &str,
        length: u64,
        device: (u32, u32),
        key: &[u8; 64],
    ) -> io::Result<bool> {
        self.matches_table(
            name,
            &[Target {
                start: 0,
                length,
                device,
                offset: 32768,
                kind: Kind::Crypt { key, iv: 0 },
            }],
        )
    }
    pub fn create_crypt(
        &self,
        name: &str,
        uuid: &str,
        length: u64,
        device: (u32, u32),
        key: &[u8; 64],
        read_only: bool,
    ) -> io::Result<Status> {
        self.create_table_mode(
            name,
            uuid,
            &[Target {
                start: 0,
                length,
                device,
                offset: 32768,
                kind: Kind::Crypt { key, iv: 0 },
            }],
            read_only,
        )
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
    fn multi_target_offsets_distinguish_request_and_reply_origins() {
        let key = [0x5a; 64];
        let targets = [
            Target {
                start: 0,
                length: 8,
                device: (253, 1),
                offset: 32768,
                kind: Kind::Crypt { key: &key, iv: 0 },
            },
            Target {
                start: 8,
                length: 8,
                device: (253, 2),
                offset: 8,
                kind: Kind::Crypt { key: &key, iv: 8 },
            },
            Target {
                start: 16,
                length: 8,
                device: (253, 1),
                offset: 32784,
                kind: Kind::Crypt { key: &key, iv: 16 },
            },
        ];
        let request = Packet::table("volume", &targets).unwrap();
        let expected = request.target_params(false).unwrap();
        assert_eq!(
            expected.iter().map(|t| (t.0, t.1, t.2)).collect::<Vec<_>>(),
            vec![(0, 8, "crypt"), (8, 8, "crypt"), (16, 8, "crypt")]
        );
        assert!(expected[1].3.ends_with(b" 8 253:2 8 1 sector_size:4096"));
        let mut reply = Packet::table("volume", &targets).unwrap();
        let mut at = HEADER;
        let mut offsets = Vec::new();
        let mut data_end = 0;
        for _ in 0..3 {
            offsets.push(at);
            let next = at + reply.get32(at + 20).unwrap() as usize;
            let params = reply.string(at + TARGET, next - at - TARGET).unwrap();
            data_end = at + TARGET + params.len() + 1;
            reply.u32(at + 20, (next - HEADER) as u32).unwrap();
            at = next;
        }
        reply.u32(12, data_end as u32).unwrap();
        assert_eq!(reply.target_params(true).unwrap(), expected);
        assert!(reply.target_params(false).is_err());
        let last = offsets[2];
        let good_next = reply.get32(last + 20).unwrap();
        reply.u32(last + 20, good_next + 8).unwrap();
        assert!(reply.target_params(true).is_err());
        reply.u32(last + 20, good_next).unwrap();
        reply.bytes[data_end - 1] = b'x';
        assert!(reply.target_params(true).is_err());
        reply.bytes[data_end - 1] = 0;
        for bad in [0, 40, 41, u32::MAX] {
            reply.u32(HEADER + 20, bad).unwrap();
            assert!(reply.target_params(true).is_err());
        }
    }

    #[test]
    fn tables_refuse_gaps_overlap_unaligned_and_overflowing_extents() {
        let make = |start, length, offset| Target {
            start,
            length,
            offset,
            device: (253, 0),
            kind: Kind::Linear,
        };
        for targets in [
            vec![],
            vec![make(8, 8, 0)],
            vec![make(0, 8, 0), make(16, 8, 8)],
            vec![make(0, 16, 0), make(8, 8, 8)],
            vec![make(0, 0, 0)],
            vec![make(0, 7, 0)],
            vec![make(0, 8, 1)],
            vec![make(0, 8, u64::MAX - 7)],
            vec![
                make(0, 8, 0),
                make(8, 8, 8),
                make(16, 8, 16),
                make(24, 8, 24),
            ],
        ] {
            assert!(Packet::table("volume", &targets).is_err());
        }
        let mut target = make(0, 8, 0);
        target.kind = Kind::Crypt {
            key: &[0; 64],
            iv: u64::MAX - 7,
        };
        assert!(Packet::table("volume", &[target]).is_err());
        let p = Packet::table("volume", &[make(0, 8, 32768)]).unwrap();
        assert_eq!(
            p.target_params(false).unwrap(),
            vec![(0, 8, "linear", b"253:0 32768" as &[u8])]
        );
        assert_eq!(DEFERRED_REMOVE, 0x20000);
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
