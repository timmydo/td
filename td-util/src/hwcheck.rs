//! `hwcheck` — which of this machine's devices have a driver.
//!
//! td builds no modules, so a device works only if the kernel carries its
//! driver, and only if any firmware that driver needs was there. This walks
//! sysfs's PCI and USB devices and says what is unbound, then reads the kernel
//! log for firmware files the loader reported missing.
//!
//! `--builtin FILE` asks the other question, from any Linux system: given a td
//! kernel's `modules.builtin.modinfo`, which the recipe leaves beside its
//! bzImage, would that kernel have a driver for each device here? It compares
//! the device's modalias with the drivers' aliases and reads nothing from the
//! running kernel's bindings. That is a good answer, not the kernel's: a USB
//! driver can claim a sibling interface no alias names, so an interface whose
//! device has a matched one of the same class is counted apart, as possible; a
//! driver with no ID table has no alias; and the
//! table keeps one driver per identical alias, so the name shown is a candidate,
//! not necessarily the one that would bind.
//!
//! It is a report, so it exits 0 whatever it finds and a device that vanishes
//! mid-walk costs that device's line: the image's boot probe runs it, and a
//! diagnostic must not decide a deployment's health.

use crate::{dmesg, glob};
use std::collections::BTreeSet;
use std::fs;
use std::io::Read;
use std::path::Path;

const USAGE: &str = "usage: hwcheck [--sys DIR] [--builtin MODINFO]";
/// A td kernel's table is well under a MiB; a file far beyond it is not one.
const MAX_MODINFO_BYTES: u64 = 64 << 20;
/// A sysfs attribute this reads is a line or two.
const MAX_ATTR_BYTES: u64 = 4096;

pub fn run(args: &[String]) -> Result<u8, String> {
    let mut sys = String::from("/sys");
    let mut builtin = None;
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        let value = rest.next();
        match (arg.as_str(), value) {
            ("--sys", Some(dir)) => sys = dir.clone(),
            ("--builtin", Some(file)) => builtin = Some(read_aliases(Path::new(file))?),
            _ => return Err(format!("unrecognised option '{arg}'\n{USAGE}")),
        }
    }
    let sys = Path::new(&sys);
    if !sys.join("bus").is_dir() {
        return Err(format!("{}: no bus directory", sys.display()));
    }
    let kmsg = match builtin {
        Some(_) => None,
        None => Some(dmesg::read_kmsg("/dev/kmsg")),
    };
    let firmware = match &kmsg {
        Some(Ok(text)) => Firmware::Read {
            names: missing_firmware(text),
            complete: log_is_complete(text),
        },
        Some(Err(e)) => Firmware::Unread(e.clone()),
        None => Firmware::NotAsked,
    };
    let walked = walk(sys);
    crate::emit(&render(&walked, builtin.as_deref(), &firmware))?;
    Ok(0)
}

/// One PCI function or USB interface.
struct Device {
    bus: &'static str,
    name: String,
    /// `vendor:device`, for a search and a bug report.
    id: String,
    kind: &'static str,
    modalias: String,
    driver: Option<String>,
    /// Unbound is ordinary for it: a bridge or a chipset function.
    quiet: bool,
}

/// What the kernel log said about firmware.
enum Firmware {
    Read {
        names: BTreeSet<String>,
        complete: bool,
    },
    Unread(String),
    NotAsked,
}

/// What the walk found, and what it could not read.
struct Walked {
    devices: Vec<Device>,
    unread: Vec<String>,
}

/// Every PCI and USB device under `sys`, in name order. A device gone by the
/// time its attributes are read was unplugged, and has no line.
fn walk(sys: &Path) -> Walked {
    let mut walked = Walked {
        devices: Vec::new(),
        unread: Vec::new(),
    };
    let mut add = |dir: &Path, read: fn(&Path) -> Result<Device, String>| match read(dir) {
        Ok(device) => walked.devices.push(device),
        Err(_) if !dir.exists() => {}
        Err(e) => walked.unread.push(e),
    };
    let (pci, pci_error) = entries(&sys.join("bus/pci/devices"));
    for dir in &pci {
        add(dir, pci_device);
    }
    let (usb, usb_error) = entries(&sys.join("bus/usb/devices"));
    for dir in &usb {
        // `1-2` is a device, `1-2:1.0` the interface a driver binds, and only
        // an interface's modalias names a class.
        if file_name(dir).contains(':') {
            add(dir, usb_interface);
        }
    }
    walked.unread.extend(pci_error);
    walked.unread.extend(usb_error);
    walked
}

/// A directory's entries in name order, and what stopped the read. An absent
/// directory has none, as a machine with no USB has no `bus/usb`.
fn entries(dir: &Path) -> (Vec<std::path::PathBuf>, Option<String>) {
    let read = match fs::read_dir(dir) {
        Ok(read) => read,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (Vec::new(), None),
        Err(e) => return (Vec::new(), Some(format!("{}: {e}", dir.display()))),
    };
    let mut paths = Vec::new();
    let mut error = None;
    for entry in read {
        match entry {
            Ok(entry) => paths.push(entry.path()),
            Err(e) => error = Some(format!("{}: {e}", dir.display())),
        }
    }
    paths.sort();
    (paths, error)
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn attr(dir: &Path, name: &str) -> Result<String, String> {
    let path = dir.join(name);
    let file = fs::File::open(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut text = String::new();
    file.take(MAX_ATTR_BYTES)
        .read_to_string(&mut text)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(text.trim().to_string())
}

fn driver(dir: &Path) -> Option<String> {
    fs::read_link(dir.join("driver"))
        .ok()
        .map(|target| file_name(&target))
}

fn pci_device(dir: &Path) -> Result<Device, String> {
    let hex = |name: &str| -> Result<String, String> {
        let text = attr(dir, name)?;
        Ok(text.strip_prefix("0x").unwrap_or(&text).to_string())
    };
    let class = u32::from_str_radix(&hex("class")?, 16)
        .map_err(|e| format!("{}: class: {e}", dir.display()))?;
    let (kind, quiet) = pci_kind(class);
    Ok(Device {
        bus: "pci",
        name: file_name(dir),
        id: format!("{}:{}", hex("vendor")?, hex("device")?),
        kind,
        modalias: attr(dir, "modalias")?,
        driver: driver(dir),
        quiet,
    })
}

/// A class's name, and whether no driver is an ordinary state for it, which
/// keeps a chipset's management functions from reading as missing hardware. The
/// class code is base, subclass and programming interface, a byte each.
fn pci_kind(class: u32) -> (&'static str, bool) {
    match (class >> 16, (class >> 8) & 0xff) {
        (0x01, _) => ("storage", false),
        (0x02, _) => ("network", false),
        (0x03, _) => ("display", false),
        (0x04, _) => ("multimedia", false),
        (0x05, _) => ("memory", true),
        (0x06, _) => ("bridge", true),
        // Intel's management engine, on every Intel machine, is 0x0780.
        (0x07, 0x80) => ("management", true),
        (0x07, _) => ("communication", false),
        (0x08, 0x05) => ("sd-host", false),
        (0x08, _) => ("system", true),
        (0x0c, 0x03) => ("usb-host", false),
        (0x0c, 0x05) => ("smbus", true),
        (0x0c, _) => ("serial-bus", false),
        (0x0d, _) => ("wireless", false),
        (0x10, _) => ("crypto", true),
        (0x11, _) => ("signal", true),
        // AMD's mobile parts expose dummy functions no driver wants here.
        (0x13, _) => ("instrumentation", true),
        _ => ("other", false),
    }
}

fn usb_interface(dir: &Path) -> Result<Device, String> {
    let modalias = attr(dir, "modalias")?;
    let class = attr(dir, "bInterfaceClass")?;
    Ok(Device {
        bus: "usb",
        name: file_name(dir),
        id: usb_id(&modalias),
        kind: usb_kind(&class),
        modalias,
        driver: driver(dir),
        quiet: false,
    })
}

/// `vendor:product` from `usb:v046DpC52Bd...`.
fn usb_id(modalias: &str) -> String {
    let id = modalias.strip_prefix("usb:v").and_then(|rest| {
        let (vendor, rest) = rest.split_once('p')?;
        let (product, _) = rest.split_once('d')?;
        Some(format!(
            "{}:{}",
            vendor.to_ascii_lowercase(),
            product.to_ascii_lowercase()
        ))
    });
    id.unwrap_or_default()
}

fn usb_kind(class: &str) -> &'static str {
    match class {
        "01" => "audio",
        "02" => "cdc",
        "0a" => "cdc-data",
        "03" => "hid",
        "08" => "storage",
        "09" => "hub",
        "0e" => "video",
        "e0" => "wireless",
        _ => "other",
    }
}

/// `(driver, alias)` for each PCI and USB alias in a kernel's
/// `modules.builtin.modinfo`, whose records are NUL-separated `driver.key=value`.
fn parse_aliases(table: &[u8]) -> Vec<(String, String)> {
    let mut aliases = Vec::new();
    for record in table.split(|byte| *byte == 0) {
        let record = String::from_utf8_lossy(record);
        if let Some((driver, alias)) = record.split_once(".alias=") {
            if alias.starts_with("pci:") || alias.starts_with("usb:") {
                aliases.push((driver.to_string(), alias.to_string()));
            }
        }
    }
    aliases
}

fn read_aliases(path: &Path) -> Result<Vec<(String, String)>, String> {
    let file = fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut table = Vec::new();
    file.take(MAX_MODINFO_BYTES + 1)
        .read_to_end(&mut table)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    if table.len() as u64 > MAX_MODINFO_BYTES {
        return Err(format!("{}: too large for a modinfo table", path.display()));
    }
    let aliases = parse_aliases(&table);
    if aliases.is_empty() {
        return Err(format!("{}: no PCI or USB aliases", path.display()));
    }
    Ok(aliases)
}

/// A vendor-class USB device takes no interface-class driver unless the driver
/// names the vendor (`usb_match_one_id_intf`).
fn usb_rule_refuses(alias: &str, modalias: &str) -> bool {
    modalias.starts_with("usb:")
        && modalias.contains("dcFF")
        && alias.starts_with("usb:v*")
        && !alias.ends_with("ic*isc*ip*in*")
}

/// Each driver in the table whose alias matches this device.
fn matching<'a>(aliases: &'a [(String, String)], modalias: &str) -> Vec<&'a str> {
    let mut drivers: Vec<&str> = aliases
        .iter()
        .filter(|(_, alias)| {
            glob::matches(alias.as_bytes(), modalias.as_bytes(), false)
                && !usb_rule_refuses(alias, modalias)
        })
        .map(|(driver, _)| driver.as_str())
        .collect();
    drivers.sort_unstable();
    drivers.dedup();
    drivers
}

/// The USB device an interface belongs to: `1-2` of `1-2:1.0`.
fn usb_device(interface: &str) -> &str {
    interface.split(':').next().unwrap_or(interface)
}

/// Whether the log reaches back to boot. The marker reports loss during the
/// read; a record evicted before it is seen only in the first sequence number.
fn log_is_complete(kmsg: &str) -> bool {
    let first_seq = kmsg.lines().next().and_then(|line| line.split(',').nth(1));
    !kmsg.contains(dmesg::INCOMPLETE_MARKER) && first_seq.is_none_or(|seq| seq == "0")
}

/// The firmware names of the loader's "Direct firmware load for NAME failed".
/// A driver may ask quietly, and then the log says nothing; or may be trying a
/// newer version first or asking for a file it can do without, and then a
/// working device is named here.
fn missing_firmware(kmsg: &str) -> BTreeSet<String> {
    const BEFORE: &str = "Direct firmware load for ";
    const AFTER: &str = " failed with error";
    kmsg.lines()
        .filter_map(|line| {
            let (_, rest) = line.split_once(BEFORE)?;
            let (name, _) = rest.split_once(AFTER)?;
            Some(dmesg::unescape(name))
        })
        .collect()
}

fn render(walked: &Walked, aliases: Option<&[(String, String)]>, firmware: &Firmware) -> String {
    let devices = &walked.devices;
    let drivers: Vec<String> = devices
        .iter()
        .map(|device| match aliases {
            Some(table) => matching(table, &device.modalias).join(","),
            None => device.driver.clone().unwrap_or_default(),
        })
        .collect();
    let mut out = String::new();
    let (mut problems, mut possible, mut ignored) = (0usize, 0usize, 0usize);
    for (device, drivers_here) in devices.iter().zip(&drivers) {
        if drivers_here.is_empty() && device.quiet {
            ignored += 1;
            continue;
        }
        // Only the table has this gap: running, a claimed interface has a link.
        // A class's driver claims its own function's interfaces, not a webcam's
        // video beside a microphone, so the sibling must be of the same class.
        // CDC's control interfaces are separate functions, so only a data
        // interface goes with one.
        let sibling = aliases.is_some()
            && device.bus == "usb"
            && !matches!(device.kind, "other" | "cdc")
            && devices.iter().zip(&drivers).any(|(other, its)| {
                other.bus == "usb"
                    && (other.kind == device.kind
                        || (device.kind == "cdc-data" && other.kind == "cdc"))
                    && !its.is_empty()
                    && usb_device(&other.name) == usb_device(&device.name)
            });
        let state = if !drivers_here.is_empty() {
            drivers_here.as_str()
        } else if sibling {
            possible += 1;
            "no alias of its own; its device's driver may claim it"
        } else {
            problems += 1;
            "NO DRIVER"
        };
        out.push_str(&format!(
            "{} {} {} {}: {state}\n",
            device.bus, device.name, device.id, device.kind
        ));
    }
    for error in &walked.unread {
        out.push_str(&format!("unreadable: {error}\n"));
    }
    let mut summary = format!("{problems} without a driver, ");
    if aliases.is_some() {
        summary.push_str(&format!(
            "{possible} only if a sibling's driver claims them, "
        ));
    }
    match firmware {
        Firmware::Read { names, complete } => {
            for name in names {
                out.push_str(&format!("firmware {name}: asked for and not found\n"));
            }
            if !complete {
                out.push_str("firmware: the kernel log is incomplete, so this may miss some\n");
            }
            summary.push_str(&format!(
                "{} firmware files logged as not found (some may be optional), ",
                names.len()
            ));
        }
        Firmware::Unread(error) => {
            out.push_str(&format!(
                "firmware: the kernel log was unreadable: {error}\n"
            ));
        }
        // This table says nothing about firmware: the kernel it describes is
        // not the one running.
        Firmware::NotAsked => {}
    }
    out.push_str(&format!(
        "{summary}{ignored} bridge or chipset functions ignored\n"
    ));
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

    fn device(root: &Path, bus: &str, name: &str, attrs: &[(&str, &str)], driver: Option<&str>) {
        let dir = root.join("bus").join(bus).join("devices").join(name);
        fs::create_dir_all(&dir).unwrap();
        for (attr, value) in attrs {
            fs::write(dir.join(attr), format!("{value}\n")).unwrap();
        }
        if let Some(driver) = driver {
            std::os::unix::fs::symlink(format!("../../drivers/{driver}"), dir.join("driver"))
                .unwrap();
        }
    }

    fn pci(root: &Path, name: &str, class: &str, device_id: &str, driver: Option<&str>) {
        // sysfs prints a modalias in upper case and a class as six digits.
        let code = |range: std::ops::Range<usize>| class[2..].get(range).unwrap().to_uppercase();
        let modalias = format!(
            "pci:v00008086d0000{}sv00001028sd00000001bc{}sc{}i{}",
            device_id.to_uppercase(),
            code(0..2),
            code(2..4),
            code(4..6)
        );
        device(
            root,
            "pci",
            name,
            &[
                ("class", class),
                ("vendor", "0x8086"),
                ("device", &format!("0x{device_id}")),
                ("modalias", &modalias),
            ],
            driver,
        );
    }

    fn interface(root: &Path, name: &str, modalias: &str, class: &str, driver: Option<&str>) {
        device(
            root,
            "usb",
            name,
            &[("modalias", modalias), ("bInterfaceClass", class)],
            driver,
        );
    }

    const KEYBOARD: &str = "usb:v046DpC52Bd2411dc00dsc00dp00ic03isc01ip01in00";

    fn machine() -> td_fs::TempDir {
        let root = td_fs::TempDir::new("td-util-hwcheck").unwrap();
        let path = root.path();
        pci(path, "0000:00:00.0", "0x060000", "1234", None);
        pci(path, "0000:03:00.0", "0x020000", "15f3", Some("igc"));
        pci(path, "0000:04:00.0", "0x010802", "f1a5", None);
        interface(path, "1-2:1.0", KEYBOARD, "03", None);
        // A USB device, as opposed to its interface, has no modalias to read.
        device(path, "usb", "1-2", &[], Some("usb"));
        root
    }

    fn read(names: &[&str], complete: bool) -> Firmware {
        Firmware::Read {
            names: names.iter().map(|name| name.to_string()).collect(),
            complete,
        }
    }

    #[test]
    fn walks_pci_functions_and_usb_interfaces_in_order() {
        let root = machine();
        let walked = walk(root.path());
        assert!(walked.unread.is_empty(), "{:?}", walked.unread);
        let devices = walked.devices;
        let names: Vec<&str> = devices.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(
            names,
            ["0000:00:00.0", "0000:03:00.0", "0000:04:00.0", "1-2:1.0"]
        );
        assert_eq!(devices[1].driver.as_deref(), Some("igc"));
        assert_eq!(devices[1].id, "8086:15f3");
        assert_eq!(devices[2].kind, "storage");
        assert_eq!(devices[3].id, "046d:c52b");
        assert_eq!(devices[3].kind, "hid");
    }

    /// An attribute the kernel would have, missing, is reported; the rest of the
    /// machine still is. Its directory being gone altogether is an unplug.
    #[test]
    fn one_unreadable_device_costs_its_own_line() {
        let root = machine();
        device(root.path(), "usb", "3-1:1.0", &[], None);
        let walked = walk(root.path());
        assert_eq!(walked.devices.len(), 4);
        assert_eq!(walked.unread.len(), 1);
        assert!(walked.unread[0].contains("3-1:1.0"), "{:?}", walked.unread);
        let text = render(&walked, None, &Firmware::NotAsked);
        assert!(text.contains("unreadable: "), "{text}");
        assert!(text.contains("pci 0000:03:00.0"), "{text}");
    }

    #[test]
    fn the_report_runs_end_to_end_and_always_exits_zero() {
        let root = machine();
        let table = root.path().join("modinfo");
        fs::write(&table, b"nvme.alias=pci:v*d*sv*sd*bc01sc08i02*\0").unwrap();
        let args = [
            "--sys".to_string(),
            root.path().display().to_string(),
            "--builtin".to_string(),
            table.display().to_string(),
        ];
        assert_eq!(run(&args).unwrap(), 0);
        assert_eq!(run(&args[..2]).unwrap(), 0, "without the table");
    }

    #[test]
    fn a_machine_with_no_usb_bus_still_reports() {
        let root = td_fs::TempDir::new("td-util-hwcheck-nousb").unwrap();
        pci(root.path(), "0000:03:00.0", "0x020000", "15f3", Some("igc"));
        assert_eq!(walk(root.path()).devices.len(), 1);
    }

    #[test]
    fn unbound_devices_are_named_and_bridges_are_not() {
        let root = machine();
        let text = render(
            &walk(root.path()),
            None,
            &read(&["iwlwifi-so-a0.ucode"], true),
        );
        assert!(
            text.contains("pci 0000:03:00.0 8086:15f3 network: igc\n"),
            "{text}"
        );
        assert!(
            text.contains("pci 0000:04:00.0 8086:f1a5 storage: NO DRIVER\n"),
            "{text}"
        );
        assert!(
            text.contains("usb 1-2:1.0 046d:c52b hid: NO DRIVER\n"),
            "{text}"
        );
        assert!(!text.contains("0000:00:00.0"), "{text}");
        assert!(
            text.contains("firmware iwlwifi-so-a0.ucode: asked for and not found\n"),
            "{text}"
        );
        assert!(!text.contains("incomplete"), "{text}");
        assert!(
            text.ends_with(
                "2 without a driver, 1 firmware files logged as not found (some may be optional), \
                 1 bridge or chipset functions ignored\n"
            ),
            "{text}"
        );
    }

    #[test]
    fn a_log_that_says_nothing_is_not_read_as_all_clear() {
        let root = machine();
        let incomplete = render(&walk(root.path()), None, &read(&[], false));
        assert!(
            incomplete.contains("the kernel log is incomplete"),
            "{incomplete}"
        );
        let unread = render(
            &walk(root.path()),
            None,
            &Firmware::Unread("/dev/kmsg: Operation not permitted".to_string()),
        );
        assert!(
            unread.contains("the kernel log was unreadable: /dev/kmsg"),
            "{unread}"
        );
        assert!(!unread.contains("logged as not found"), "{unread}");
    }

    #[test]
    fn a_bound_bridge_is_still_listed() {
        let root = td_fs::TempDir::new("td-util-hwcheck-bridge").unwrap();
        pci(
            root.path(),
            "0000:00:02.1",
            "0x060400",
            "1234",
            Some("pcieport"),
        );
        let text = render(&walk(root.path()), None, &Firmware::NotAsked);
        assert!(text.contains("bridge: pcieport\n"), "{text}");
    }

    #[test]
    fn builtin_mode_matches_aliases_not_the_running_bindings() {
        let root = machine();
        // igc is bound here, but this table does not carry it; nvme binds by
        // class, and the record that is not an alias is ignored.
        let table = b"nvme.alias=pci:v*d*sv*sd*bc01sc08i02*\0nvme.license=GPL\0\
                      usbhid.alias=usb:v*p*d*dc*dsc*dp*ic03isc*ip*in*\0\
                      acpi_thing.alias=acpi*:PNP0C0A:*\0";
        let aliases = parse_aliases(table);
        assert_eq!(aliases.len(), 2);
        let text = render(&walk(root.path()), Some(&aliases), &Firmware::NotAsked);
        assert!(
            text.contains("0000:03:00.0 8086:15f3 network: NO DRIVER\n"),
            "{text}"
        );
        assert!(
            text.contains("0000:04:00.0 8086:f1a5 storage: nvme\n"),
            "{text}"
        );
        assert!(text.contains("1-2:1.0 046d:c52b hid: usbhid\n"), "{text}");
        assert!(!text.contains("firmware"), "{text}");
    }

    /// A headset's driver matches its control interface by alias and claims the
    /// streaming one, which no alias names.
    #[test]
    fn a_sibling_interface_is_not_a_missing_driver() {
        let root = td_fs::TempDir::new("td-util-hwcheck-sibling").unwrap();
        let control = "usb:v0D8Cp0012d0100dc00dsc00dp00ic01isc01ip00in00";
        let streaming = "usb:v0D8Cp0012d0100dc00dsc00dp00ic01isc02ip00in01";
        interface(root.path(), "1-1:1.0", control, "01", None);
        interface(root.path(), "1-1:1.1", streaming, "01", None);
        // The same class of interface on a device nothing claims stays a problem.
        interface(root.path(), "2-1:1.1", streaming, "01", None);
        let aliases = parse_aliases(b"snd_usb_audio.alias=usb:v*p*d*dc*dsc*dp*ic01isc01ip*in*\0");
        let text = render(&walk(root.path()), Some(&aliases), &Firmware::NotAsked);
        assert!(
            text.contains("1-1:1.0 0d8c:0012 audio: snd_usb_audio\n"),
            "{text}"
        );
        assert!(
            text.contains("1-1:1.1 0d8c:0012 audio: no alias of its own"),
            "{text}"
        );
        assert!(
            text.contains("2-1:1.1 0d8c:0012 audio: NO DRIVER\n"),
            "{text}"
        );
        assert!(
            text.contains("1 only if a sibling's driver claims them"),
            "{text}"
        );
        assert!(
            text.starts_with("usb") && text.contains("\n1 without a driver"),
            "{text}"
        );
    }

    /// A webcam's microphone says nothing about its camera, which no driver in
    /// this table serves.
    #[test]
    fn a_sibling_of_another_class_clears_nothing() {
        let root = td_fs::TempDir::new("td-util-hwcheck-webcam").unwrap();
        let video = "usb:v046Dp0825d0100dcEFdsc02dp01ic0Eisc01ip00in00";
        let audio = "usb:v046Dp0825d0100dcEFdsc02dp01ic01isc01ip00in02";
        let vendor = "usb:v046Dp0825d0100dcEFdsc02dp01icFFisc00ip00in03";
        interface(root.path(), "1-1:1.0", video, "0e", None);
        interface(root.path(), "1-1:1.2", audio, "01", None);
        interface(root.path(), "1-1:1.3", vendor, "ff", None);
        let aliases = parse_aliases(b"snd_usb_audio.alias=usb:v*p*d*dc*dsc*dp*ic01isc01ip*in*\0");
        let text = render(&walk(root.path()), Some(&aliases), &Firmware::NotAsked);
        assert!(
            text.contains("1-1:1.0 046d:0825 video: NO DRIVER\n"),
            "{text}"
        );
        assert!(
            text.contains("1-1:1.3 046d:0825 other: NO DRIVER\n"),
            "{text}"
        );
        assert!(text.contains("2 without a driver"), "{text}");
    }

    /// CDC's control interface is class 02 and its data interface 0a, which has
    /// no alias. A second control interface is another function, which Ethernet's
    /// driver does not claim.
    #[test]
    fn a_cdc_data_interface_goes_with_its_control_interface() {
        let root = td_fs::TempDir::new("td-util-hwcheck-cdc").unwrap();
        let control = "usb:v0BDAp8153d3000dc00dsc00dp00ic02isc06ip00in00";
        let data = "usb:v0BDAp8153d3000dc00dsc00dp00ic0Aisc00ip00in01";
        interface(root.path(), "1-1:1.0", control, "02", None);
        interface(root.path(), "1-1:1.1", data, "0a", None);
        let acm = "usb:v0BDAp8153d3000dc00dsc00dp00ic02isc02ip00in02";
        interface(root.path(), "1-1:1.2", acm, "02", None);
        let aliases = parse_aliases(b"cdc_ether.alias=usb:v*p*d*dc*dsc*dp*ic02isc06ip*in*\0");
        let text = render(&walk(root.path()), Some(&aliases), &Firmware::NotAsked);
        assert!(
            text.contains("1-1:1.1 0bda:8153 cdc-data: no alias of its own"),
            "{text}"
        );
        assert!(
            text.contains("1-1:1.2 0bda:8153 cdc: NO DRIVER\n"),
            "{text}"
        );
        assert!(text.contains("1 without a driver"), "{text}");
    }

    #[test]
    fn a_log_missing_its_start_is_incomplete() {
        assert!(log_is_complete("6,0,5,-;Linux version\n6,1,6,-;next\n"));
        assert!(log_is_complete(""));
        assert!(!log_is_complete("6,412,5,-;long after boot\n"));
        assert!(!log_is_complete("6,0,5,-;x\nTD-DMESG-INCOMPLETE\n"));
    }

    #[test]
    fn a_vendor_class_device_takes_no_class_driver() {
        let aliases = parse_aliases(
            b"usbhid.alias=usb:v*p*d*dc*dsc*dp*ic03isc*ip*in*\0\
              thing.alias=usb:v1234p5678d*dc*dsc*dp*ic*isc*ip*in*\0\
              number.alias=usb:v*p*d*dc*dsc*dp*ic*isc*ip*in00*\0\
              named.alias=usb:v1234p*d*dc*dsc*dp*ic03isc*ip*in*\0",
        );
        assert_eq!(
            matching(
                &aliases,
                "usb:v1234p5678d0100dcFFdsc00dp00ic03isc00ip00in00"
            ),
            ["named", "thing"]
        );
        assert_eq!(
            matching(
                &aliases,
                "usb:v1234p5678d0100dc00dsc00dp00ic03isc00ip00in00"
            ),
            ["named", "number", "thing", "usbhid"]
        );
    }

    #[test]
    fn several_drivers_can_claim_one_device() {
        let table = b"b.alias=pci:v00008086d*\0a.alias=pci:v*d00001234*\0a.alias=pci:*\0";
        let aliases = parse_aliases(table);
        assert_eq!(matching(&aliases, "pci:v00008086d00001234sv*"), ["a", "b"]);
    }

    #[test]
    fn alias_ranges_match_as_the_kernel_writes_them() {
        let aliases = parse_aliases(b"x.alias=usb:v0BDAp8153d3[0-9]dc*\0");
        assert_eq!(matching(&aliases, "usb:v0BDAp8153d31dcFF"), ["x"]);
        assert!(matching(&aliases, "usb:v0BDAp8153d41dcFF").is_empty());
    }

    #[test]
    fn firmware_failures_are_read_from_the_log() {
        let kmsg = "6,1,5,-;usb 1-2: new device\n\
                    4,2,6,-;iwlwifi 0000:02:00.0: Direct firmware load for iwlwifi-so-a0.ucode failed with error -2\n\
                    4,3,7,-;bluetooth: Direct firmware load for intel/ibt-1.sfi failed with error -2\n\
                    4,4,8,-;iwlwifi 0000:02:00.0: Direct firmware load for iwlwifi-so-a0.ucode failed with error -2\n";
        let names: Vec<String> = missing_firmware(kmsg).into_iter().collect();
        assert_eq!(names, ["intel/ibt-1.sfi", "iwlwifi-so-a0.ucode"]);
    }

    #[test]
    fn an_empty_oversized_or_unreadable_table_is_refused() {
        let root = td_fs::TempDir::new("td-util-hwcheck-table").unwrap();
        let table = root.path().join("modinfo");
        let big = fs::File::create(&table).unwrap();
        big.set_len(MAX_MODINFO_BYTES + 1).unwrap();
        assert!(read_aliases(&table).unwrap_err().contains("too large"));
        fs::write(&table, b"a.license=GPL\0").unwrap();
        assert!(read_aliases(&table)
            .unwrap_err()
            .contains("no PCI or USB aliases"));
        assert!(read_aliases(&root.path().join("absent")).is_err());
    }

    #[test]
    fn a_flag_without_its_value_is_refused() {
        assert!(run(&["--sys".to_string()]).unwrap_err().contains("usage"));
        assert!(run(&["--nope".to_string(), "x".to_string()])
            .unwrap_err()
            .contains("usage"));
    }

    #[test]
    fn pci_classes_decide_what_unbound_means() {
        assert_eq!(pci_kind(0x0c0330), ("usb-host", false));
        assert_eq!(pci_kind(0x0c8000), ("serial-bus", false), "Intel LPSS I2C");
        assert_eq!(pci_kind(0x078000), ("management", true), "Intel MEI");
        assert_eq!(pci_kind(0x070002), ("communication", false));
        assert_eq!(pci_kind(0x0c0500), ("smbus", true));
        assert_eq!(pci_kind(0x080501), ("sd-host", false));
        assert_eq!(pci_kind(0x088000), ("system", true));
        assert_eq!(pci_kind(0x120000), ("other", false), "an accelerator");
        assert_eq!(pci_kind(0x130000), ("instrumentation", true));
        assert_eq!(pci_kind(0x060400), ("bridge", true));
    }
}
