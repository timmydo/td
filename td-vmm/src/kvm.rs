//! Linux x86-64 KVM ABI. One thread owns the VM and every mapping.
use crate::{boot, invalid, serial::Uart};
use std::{
    fs::{File, OpenOptions},
    io::{self, Write},
    os::fd::{AsRawFd, FromRawFd},
    os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt},
    ptr::NonNull,
};

const KVM_GET_SUPPORTED_CPUID: usize = 0xc008ae05;
const KVM_SET_CPUID2: usize = 0x4008ae90;
const KVM_GET_SREGS: usize = 0x8138ae83;
const KVM_SET_SREGS: usize = 0x4138ae84;
const KVM_SET_REGS: usize = 0x4090ae82;
const KVM_GET_API_VERSION: usize = 0xae00;
const KVM_GET_VCPU_MMAP_SIZE: usize = 0xae04;
const KVM_CREATE_VM: usize = 0xae01;
const KVM_SET_TSS_ADDR: usize = 0xae47;
const KVM_SET_IDENTITY_MAP_ADDR: usize = 0x4008ae48;
const KVM_SET_USER_MEMORY_REGION: usize = 0x4020ae46;
const KVM_CREATE_IRQCHIP: usize = 0xae60;
const KVM_CREATE_PIT2: usize = 0x4040ae77;
const KVM_CREATE_VCPU: usize = 0xae41;
const KVM_RUN: usize = 0xae80;
const KVM_IRQ_LINE: usize = 0x4008ae61;

#[allow(unsafe_code)]
fn syscall(n: usize, args: [usize; 6]) -> io::Result<usize> {
    let [a, b, c, d, e, f] = args;
    let out: isize;
    // SAFETY: private wrappers below bind each number to the Linux x86-64 ABI
    // and retain each referenced object across the synchronous call.
    unsafe {
        std::arch::asm!(
            "syscall",
            inlateout("rax") n => out,
            in("rdi") a, in("rsi") b, in("rdx") c,
            in("r10") d, in("r8") e, in("r9") f,
            lateout("rcx") _, lateout("r11") _, options(nostack)
        );
    }
    if (-4095..0).contains(&out) {
        Err(io::Error::from_raw_os_error(-out as i32))
    } else {
        Ok(out as usize)
    }
}

fn ioctl(file: &File, request: usize, value: usize) -> io::Result<usize> {
    syscall(16, [file.as_raw_fd() as usize, request, value, 0, 0, 0])
}

#[allow(unsafe_code)]
fn fresh(fd: usize) -> io::Result<File> {
    let fd = i32::try_from(fd).map_err(|_| invalid("invalid KVM descriptor"))?;
    // SAFETY: called only with a new owned descriptor returned by a create ioctl.
    Ok(unsafe { File::from_raw_fd(fd) })
}

struct Mapping {
    address: NonNull<u8>,
    len: usize,
}
impl Mapping {
    fn map(len: usize, file: Option<&File>) -> io::Result<Self> {
        let (flags, fd) = match file {
            Some(f) => (1, f.as_raw_fd()),
            None => (0x22, -1),
        };
        let ptr = syscall(9, [0, len, 3, flags, fd as usize, 0])? as *mut u8;
        Ok(Self {
            address: NonNull::new(ptr).ok_or_else(|| invalid("null mapping"))?,
            len,
        })
    }

    #[allow(unsafe_code)]
    fn bytes(&mut self) -> &mut [u8] {
        // SAFETY: the unique owner has a live read/write mapping. Guest RAM
        // is borrowed only before registration. kvm_run is borrowed only
        // between synchronous KVM_RUN calls on its sole thread.
        unsafe { std::slice::from_raw_parts_mut(self.address.as_ptr(), self.len) }
    }
}
impl Drop for Mapping {
    fn drop(&mut self) {
        let _ = syscall(11, [self.address.as_ptr() as usize, self.len, 0, 0, 0, 0]);
    }
}
pub(super) struct Ram(Mapping);
impl Ram {
    pub(super) fn new() -> io::Result<Self> {
        Ok(Self(Mapping::map(boot::RAM, None)?))
    }
    pub(super) fn bytes(&mut self) -> &mut [u8] {
        self.0.bytes()
    }
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct Segment {
    base: u64,
    limit: u32,
    selector: u16,
    kind: u8,
    present: u8,
    dpl: u8,
    db: u8,
    s: u8,
    l: u8,
    g: u8,
    avl: u8,
    unusable: u8,
    padding: u8,
}
impl Segment {
    fn flat(selector: u16, kind: u8) -> Self {
        Self {
            limit: u32::MAX,
            selector,
            kind,
            present: 1,
            db: 1,
            s: 1,
            g: 1,
            ..Self::default()
        }
    }
}
#[repr(C)]
#[derive(Default)]
struct Table {
    base: u64,
    limit: u16,
    padding: [u16; 3],
}
#[repr(C)]
#[derive(Default)]
struct Sregs {
    cs: Segment,
    ds: Segment,
    es: Segment,
    fs: Segment,
    gs: Segment,
    ss: Segment,
    tr: Segment,
    ldt: Segment,
    gdt: Table,
    idt: Table,
    cr0: u64,
    cr2: u64,
    cr3: u64,
    cr4: u64,
    cr8: u64,
    efer: u64,
    apic_base: u64,
    interrupt_bitmap: [u64; 4],
}
const _: () = assert!(size_of::<Sregs>() == 312);

/// Fixed structure size encoded by Linux _IOW/_IOR on x86-64.
const fn abi_size(request: usize) -> usize {
    (request >> 16) & 0x3fff
}

fn input<const REQUEST: usize, T>(value: &T) -> usize {
    const {
        assert!(size_of::<T>() == abi_size(REQUEST));
    }
    std::ptr::from_ref(value) as usize
}

fn set_cpu(kvm: &File, cpu: &File, entry: usize) -> io::Result<()> {
    #[repr(C)]
    struct CpuId {
        count: u32,
        padding: u32,
        entries: [[u32; 10]; 256],
    }
    const _: () = assert!(std::mem::offset_of!(CpuId, entries) == 8);
    const _: () = assert!(size_of::<[u32; 10]>() == 40);
    const _: () = assert!(size_of::<CpuId>() == 8 + 256 * 40);
    let mut supported = CpuId {
        count: 0,
        padding: 0,
        entries: [[0; 10]; 256],
    };
    let capacity = u32::try_from(supported.entries.len()).map_err(|_| invalid("CPUID capacity"))?;
    supported.count = capacity;
    ioctl(kvm, KVM_GET_SUPPORTED_CPUID, (&raw mut supported) as usize)?;
    if supported.count > capacity {
        return Err(invalid("unsupported CPUID count"));
    }
    ioctl(cpu, KVM_SET_CPUID2, (&raw const supported) as usize)?;
    let mut s = Sregs::default();
    ioctl(cpu, KVM_GET_SREGS, (&raw mut s) as usize)?;
    s.cs = Segment::flat(16, 11);
    s.ds = Segment::flat(24, 3);
    s.es = s.ds;
    s.ss = s.ds;
    s.fs = s.ds;
    s.gs = s.ds;
    s.gdt = Table {
        base: boot::GDT as u64,
        limit: 31,
        ..Table::default()
    };
    s.cr0 |= 1;
    ioctl(cpu, KVM_SET_SREGS, input::<KVM_SET_SREGS, _>(&s))?;
    let regs: [u64; 18] = [
        0,
        0,
        0,
        0,
        boot::PARAMS as u64,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        entry as u64,
        2,
    ];
    ioctl(cpu, KVM_SET_REGS, input::<KVM_SET_REGS, _>(&regs))?;
    Ok(())
}

pub(super) fn execute(ram: Ram, entry: usize) -> io::Result<()> {
    let kvm = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(0x20000)
        .open("/dev/kvm")?;
    let metadata = kvm.metadata()?;
    if !metadata.file_type().is_char_device() || metadata.rdev() != 0xae8 {
        return Err(invalid("expected /dev/kvm character device 10:232"));
    }
    if ioctl(&kvm, KVM_GET_API_VERSION, 0)? != 12 {
        return Err(invalid("unsupported KVM API version"));
    }
    let len = ioctl(&kvm, KVM_GET_VCPU_MMAP_SIZE, 0)?;
    if !(4096..=1 << 20).contains(&len) {
        return Err(invalid("unexpected KVM run mapping size"));
    }
    let vm = fresh(ioctl(&kvm, KVM_CREATE_VM, 0)?)?;
    ioctl(&vm, KVM_SET_TSS_ADDR, 0xfffbd000)?;
    let identity = 0xfffbc000u64;
    ioctl(
        &vm,
        KVM_SET_IDENTITY_MAP_ADDR,
        input::<KVM_SET_IDENTITY_MAP_ADDR, _>(&identity),
    )?;
    let region = [0u64, 0, boot::RAM as u64, ram.0.address.as_ptr() as u64];
    ioctl(
        &vm,
        KVM_SET_USER_MEMORY_REGION,
        input::<KVM_SET_USER_MEMORY_REGION, _>(&region),
    )?;
    ioctl(&vm, KVM_CREATE_IRQCHIP, 0)?;
    let pit = [0u32; 16];
    ioctl(&vm, KVM_CREATE_PIT2, input::<KVM_CREATE_PIT2, _>(&pit))?;
    let cpu = fresh(ioctl(&vm, KVM_CREATE_VCPU, 0)?)?;
    set_cpu(&kvm, &cpu, entry)?;
    let mut run = Mapping::map(len, Some(&cpu))?;
    let mut uart = Uart::default();
    let mut console = io::stdout().lock();
    let mut clock_select = 0u8;
    loop {
        match ioctl(&cpu, KVM_RUN, 0) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            result => result?,
        };
        let map = run.bytes();
        match u32::from_le_bytes(field(map, 8)?) {
            2 => {
                let [direction, width] = field(map, 32)?;
                let port = u16::from_le_bytes(field(map, 34)?);
                let count = u32::from_le_bytes(field(map, 36)?) as usize;
                let offset = usize::try_from(u64::from_le_bytes(field(map, 40)?))
                    .map_err(|_| invalid("bad I/O offset"))?;
                if !matches!(width, 1 | 2 | 4) || direction > 1 {
                    return Err(invalid("bad KVM I/O exit"));
                }
                let size = count
                    .checked_mul(usize::from(width))
                    .ok_or_else(|| invalid("bad I/O count"))?;
                let data = map
                    .get_mut(offset..)
                    .and_then(|b| b.get_mut(..size))
                    .ok_or_else(|| invalid("bad I/O range"))?;
                for word in data.chunks_exact_mut(usize::from(width)) {
                    if direction == 0 {
                        word.fill(0xff);
                    }
                    if let [value] = word {
                        match port {
                            0x3f8..=0x3ff => {
                                if direction == 0 {
                                    *value = uart.read(port);
                                } else {
                                    uart.write(port, *value, &mut console)?;
                                }
                                let irq = [4u32, u32::from(uart.irq())];
                                ioctl(&vm, KVM_IRQ_LINE, input::<KVM_IRQ_LINE, _>(&irq))?;
                            }
                            0x70 if direction == 1 => clock_select = *value & 0x7f,
                            0x71 if direction == 0 => {
                                *value = clock_register(clock_select, td_civil::now_unix());
                            }
                            0x64 if direction == 1 && *value == 0xfe => return console.flush(),
                            _ => {}
                        }
                    }
                }
            }
            6 => {
                let [write] = field(map, 52)?;
                if write == 0 {
                    map.get_mut(40..48)
                        .ok_or_else(|| invalid("short MMIO exit"))?
                        .fill(0xff);
                }
            }
            8 => return Err(invalid("KVM guest shutdown (possible triple fault)")),
            reason => {
                eprintln!("td-vmm: KVM exit {reason}");
                return Err(invalid("unhandled KVM exit"));
            }
        }
    }
}

fn field<const N: usize>(bytes: &[u8], start: usize) -> io::Result<[u8; N]> {
    bytes
        .get(start..)
        .and_then(|b| b.get(..N))
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| invalid("short KVM run mapping"))
}

/// The initial RTC is read-only UTC, in 24-hour packed BCD.
fn clock_register(index: u8, unix: i64) -> u8 {
    let date = td_civil::unix_to_civil_utc(unix);
    let value = match index {
        0 => date.second,
        2 => date.minute,
        4 => date.hour,
        6 => ((unix.div_euclid(86400) + 4).rem_euclid(7) + 1) as u8,
        7 => date.day,
        8 => date.month,
        9 => date.year.rem_euclid(100) as u8,
        0x32 => date.year.div_euclid(100).rem_euclid(100) as u8,
        0x0a => return 0x26,
        0x0b => return 2,
        0x0d => return 0x80,
        _ => return 0,
    };
    (value / 10) << 4 | (value % 10)
}
