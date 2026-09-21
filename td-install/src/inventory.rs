//! Read-only inventory and advisory candidates; neither grants write authority.

use crate::{invalid, paths};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

const MAX_DEVICES: usize = 4096;
const MAX_EDGES: usize = 16384;
const MAX_TEXT: usize = 256;

#[derive(Debug, PartialEq, Eq)]
struct Device {
    name: String,
    sysfs: PathBuf,
    number: String,
    bytes: u64,
    read_only: bool,
    partition: Option<u64>,
    parent: Option<String>,
    disk: Option<Disk>,
    holders: Vec<String>,
    slaves: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
struct Disk {
    sequence: u64,
    logical_sector_bytes: u64,
    removable: bool,
    model: Option<String>,
    serial: Option<String>,
    wwid: Option<String>,
}

fn text(path: &Path, optional: bool) -> io::Result<Option<String>> {
    let bytes = match paths::read_bounded(path, MAX_TEXT) {
        Ok(bytes) => bytes,
        Err(error) if optional && error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    Ok(Some(decode_text(bytes, path)?))
}

fn decode_text(bytes: Vec<u8>, path: &Path) -> io::Result<String> {
    let value = String::from_utf8(bytes)
        .map_err(|_| invalid(format!("{}: attribute is not UTF-8", path.display())))?;
    Ok(value.trim().to_owned())
}

fn identification(path: &Path) -> io::Result<Option<String>> {
    let file = match paths::open_read(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    identification_from(file, path)
}

fn identification_from(reader: impl Read, path: &Path) -> io::Result<Option<String>> {
    let mut bytes = Vec::new();
    match reader.take(MAX_TEXT as u64 + 1).read_to_end(&mut bytes) {
        Ok(_) => {}
        // Linux 7.1.4 scsi_vpd_lun_{serial,id}: no VPD page is ENXIO.
        Err(error) if error.raw_os_error() == Some(6) => return Ok(None),
        Err(error) => {
            return Err(io::Error::new(
                error.kind(),
                format!("{}: {error}", path.display()),
            ))
        }
    }
    if bytes.len() > MAX_TEXT {
        return Err(invalid(format!(
            "{}: attribute exceeds {MAX_TEXT} bytes",
            path.display()
        )));
    }
    decode_text(bytes, path).map(Some)
}

fn required(path: &Path) -> io::Result<String> {
    text(path, false)?.ok_or_else(|| invalid(format!("{}: missing attribute", path.display())))
}

fn decimal(value: &str, path: &Path) -> io::Result<u64> {
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return Err(invalid(format!(
            "{}: expected unsigned decimal",
            path.display()
        )));
    }
    value
        .parse()
        .map_err(|_| invalid(format!("{}: decimal overflow", path.display())))
}

fn number(path: &Path) -> io::Result<u64> {
    decimal(&required(path)?, path)
}

fn flag(path: &Path) -> io::Result<bool> {
    match number(path)? {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(invalid(format!("{}: expected 0 or 1", path.display()))),
    }
}

fn name(path: &Path) -> io::Result<String> {
    let value = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| invalid(format!("{}: invalid block name", path.display())))?;
    if value.is_empty()
        || value.len() > 255
        || value == "."
        || value == ".."
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-!.".contains(&b))
    {
        return Err(invalid(format!(
            "{}: unsupported block name",
            path.display()
        )));
    }
    Ok(value.to_owned())
}

fn device_number(path: &Path) -> io::Result<String> {
    let value = required(path)?;
    let (major, minor) = value
        .split_once(':')
        .ok_or_else(|| invalid(format!("{}: expected major:minor", path.display())))?;
    let major = decimal(major, path)?;
    let minor = decimal(minor, path)?;
    if major > u32::MAX.into() || minor > u32::MAX.into() {
        return Err(invalid(format!(
            "{}: device number overflow",
            path.display()
        )));
    }
    Ok(format!("{major}:{minor}"))
}

fn links(path: &Path, remaining: &mut usize) -> io::Result<Vec<String>> {
    let entries = paths::read_dir_bounded(path, *remaining).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "block relationship budget ({remaining} of {MAX_EDGES} entries remaining): {error}"
            ),
        )
    })?;
    *remaining = remaining.checked_sub(entries.len()).ok_or_else(|| {
        invalid(format!(
            "{}: block relationship limit exceeded",
            path.display()
        ))
    })?;
    entries.iter().map(|entry| name(entry)).collect()
}

fn collect(root: &Path) -> io::Result<Vec<Device>> {
    let entries = paths::read_dir_bounded(root, MAX_DEVICES)?;
    let mut devices = Vec::with_capacity(entries.len());
    let mut remaining = MAX_EDGES;
    for entry in entries {
        let name = name(&entry)?;
        let sysfs = paths::canonicalize(&entry)?;
        let partition_path = sysfs.join("partition");
        let partition = text(&partition_path, true)?
            .map(|value| decimal(&value, &partition_path))
            .transpose()?;
        if partition == Some(0) {
            return Err(invalid(format!(
                "{}: zero partition number",
                partition_path.display()
            )));
        }
        let disk = if partition.is_none() {
            let serial = match identification(&sysfs.join("serial"))? {
                Some(value) => Some(value),
                None => identification(&sysfs.join("device/serial"))?,
            };
            let wwid = match identification(&sysfs.join("wwid"))? {
                Some(value) => Some(value),
                None => identification(&sysfs.join("device/wwid"))?,
            };
            Some(Disk {
                sequence: number(&sysfs.join("diskseq"))?,
                logical_sector_bytes: number(&sysfs.join("queue/logical_block_size"))?,
                removable: flag(&sysfs.join("removable"))?,
                model: text(&sysfs.join("device/model"), true)?,
                serial,
                wwid,
            })
        } else {
            None
        };
        let size_path = sysfs.join("size");
        // Linux exports size in 512-byte units, including on 4Kn disks.
        let bytes = number(&size_path)?
            .checked_mul(512)
            .ok_or_else(|| invalid(format!("{}: capacity overflow", size_path.display())))?;
        devices.push(Device {
            name,
            number: device_number(&sysfs.join("dev"))?,
            bytes,
            read_only: flag(&sysfs.join("ro"))?,
            partition,
            parent: None,
            disk,
            holders: links(&sysfs.join("holders"), &mut remaining)?,
            slaves: if partition.is_none() {
                links(&sysfs.join("slaves"), &mut remaining)?
            } else {
                Vec::new()
            },
            sysfs,
        });
    }
    validate(&mut devices)?;
    Ok(devices)
}

fn validate(devices: &mut [Device]) -> io::Result<()> {
    let mut numbers = BTreeSet::new();
    let mut paths = BTreeMap::new();
    let mut names = BTreeMap::new();
    for (index, device) in devices.iter().enumerate() {
        if !numbers.insert(device.number.clone())
            || paths
                .insert(
                    device.sysfs.clone(),
                    (device.name.clone(), device.partition),
                )
                .is_some()
            || names.insert(device.name.clone(), index).is_some()
        {
            return Err(invalid(format!(
                "{}: duplicate block identity in inventory",
                device.name
            )));
        }
    }
    for device in devices.iter_mut() {
        if device.partition.is_some() {
            let parent = device
                .sysfs
                .parent()
                .and_then(|path| paths.get(path))
                .filter(|(_, partition)| partition.is_none())
                .ok_or_else(|| invalid(format!("{}: unresolved whole-disk parent", device.name)))?;
            device.parent = Some(parent.0.clone());
        }
    }
    for device in devices.iter() {
        for (links, holders) in [(&device.holders, true), (&device.slaves, false)] {
            for link in links {
                let peer = names
                    .get(link)
                    .and_then(|index| devices.get(*index))
                    .ok_or_else(|| {
                        invalid(format!(
                            "{}: unresolved block relationship {link}",
                            device.name
                        ))
                    })?;
                let reverse = if holders { &peer.slaves } else { &peer.holders };
                let directory = if holders { "holders" } else { "slaves" };
                if paths::canonicalize(&device.sysfs.join(directory).join(link))? != peer.sysfs
                    || link == &device.name
                    || reverse.binary_search(&device.name).is_err()
                {
                    return Err(invalid(format!(
                        "{}: inconsistent block relationship {link}",
                        device.name
                    )));
                }
            }
        }
    }
    Ok(())
}

fn quoted(output: &mut impl Write, value: &str) -> io::Result<()> {
    write!(output, "\"")?;
    for character in value.chars() {
        match character {
            '"' => write!(output, "\\\"")?,
            '\\' => write!(output, "\\\\")?,
            c if c <= '\u{1f}' => write!(output, "\\u{:04x}", u32::from(c))?,
            c => write!(output, "{c}")?,
        }
    }
    write!(output, "\"")
}

fn optional(output: &mut impl Write, value: Option<&str>) -> io::Result<()> {
    match value {
        Some(value) => quoted(output, value),
        None => write!(output, "null"),
    }
}

fn array(output: &mut impl Write, values: &[String]) -> io::Result<()> {
    write!(output, "[")?;
    for (index, value) in values.iter().enumerate() {
        if index != 0 {
            write!(output, ",")?;
        }
        quoted(output, value)?;
    }
    write!(output, "]")
}

fn observe_twice(
    mut observation: impl FnMut() -> io::Result<Vec<Device>>,
) -> io::Result<Vec<Device>> {
    let devices = observation()?;
    if devices != observation()? {
        return Err(invalid(
            "block inventory changed during collection; retry discovery".into(),
        ));
    }
    Ok(devices)
}

fn supported_disk_name(name: &str) -> bool {
    if let Some(suffix) = name.strip_prefix("vd").or_else(|| name.strip_prefix("sd")) {
        return !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_lowercase());
    }
    let Some((controller, namespace)) = name.strip_prefix("nvme").and_then(|s| s.split_once('n'))
    else {
        return false;
    };
    [controller, namespace]
        .iter()
        .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
}

fn candidate(device: &Device, devices: &[Device]) -> bool {
    let Some(disk) = &device.disk else {
        return false;
    };
    supported_disk_name(&device.name)
        && !device.read_only
        && matches!(disk.logical_sector_bytes, 512 | 4096)
        && crate::plan(disk.logical_sector_bytes, device.bytes).is_ok()
        && devices
            .iter()
            .filter(|peer| {
                peer.name == device.name || peer.parent.as_deref() == Some(device.name.as_str())
            })
            .all(|peer| peer.holders.is_empty() && peer.slaves.is_empty())
}

fn probe(device: &Device) -> io::Result<bool> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let path = Path::new("/dev").join(&device.name);
    let mut file = match paths::open_destination_claim(&path, false) {
        Ok(file) => file,
        // The path wrapper retains ErrorKind but deliberately drops raw errno.
        Err(error) if error.kind() == io::ErrorKind::ResourceBusy => return Ok(false),
        Err(error) => return Err(error),
    };
    let metadata = file.metadata()?;
    let (major, minor) = crate::device_numbers(metadata.rdev());
    if !metadata.file_type().is_block_device() || device.number != format!("{major}:{minor}") {
        return Err(invalid(format!(
            "{}: opened device differs from inventory",
            path.display()
        )));
    }
    if crate::destination_bytes(&mut file)? != device.bytes {
        return Err(invalid(format!(
            "{}: opened capacity differs from inventory",
            path.display()
        )));
    }
    // The complete sysfs observations bracket this temporary claim. It is
    // released here and cannot authorize later use of the name or device number.
    Ok(true)
}

fn discover(
    mut observation: impl FnMut() -> io::Result<Vec<Device>>,
    mut available: impl FnMut(&Device) -> io::Result<bool>,
    output: &mut impl Write,
) -> io::Result<()> {
    let devices = observation()?;
    let mut candidates = Vec::new();
    for device in &devices {
        if candidate(device, &devices) && available(device)? {
            candidates.push(device);
        }
    }
    if devices != observation()? {
        return Err(invalid(
            "block inventory changed during candidate discovery; retry discovery".into(),
        ));
    }
    write_devices(&candidates, "candidate-only", output)
}

pub fn destinations(output: &mut impl Write) -> io::Result<()> {
    discover(|| collect(Path::new("/sys/class/block")), probe, output)
}

pub fn run(root: &Path, output: &mut impl Write) -> io::Result<()> {
    let devices = observe_twice(|| collect(root))?;
    write_devices(
        &devices.iter().collect::<Vec<_>>(),
        "inventory-only",
        output,
    )
}

fn write_devices(devices: &[&Device], scope: &str, output: &mut impl Write) -> io::Result<()> {
    write!(output, "{{\"version\":1,\"scope\":")?;
    quoted(output, scope)?;
    write!(output, ",\"devices\":[")?;
    for (index, device) in devices.iter().enumerate() {
        if index != 0 {
            write!(output, ",")?;
        }
        write!(output, "{{\"name\":")?;
        quoted(output, &device.name)?;
        write!(output, ",\"device_number\":")?;
        quoted(output, &device.number)?;
        write!(
            output,
            ",\"capacity_bytes\":{},\"read_only\":{},\"parent\":",
            device.bytes, device.read_only
        )?;
        optional(output, device.parent.as_deref())?;
        write!(output, ",\"partition_number\":")?;
        match device.partition {
            Some(number) => write!(output, "{number}")?,
            None => write!(output, "null")?,
        }
        write!(output, ",\"disk\":")?;
        match &device.disk {
            Some(disk) => {
                write!(
                    output,
                    "{{\"sequence\":{},\"logical_sector_bytes\":{},\"removable\":{},\"model\":",
                    disk.sequence, disk.logical_sector_bytes, disk.removable
                )?;
                optional(output, disk.model.as_deref())?;
                write!(output, ",\"serial\":")?;
                optional(output, disk.serial.as_deref())?;
                write!(output, ",\"wwid\":")?;
                optional(output, disk.wwid.as_deref())?;
                write!(output, "}}")?;
            }
            None => write!(output, "null")?,
        }
        write!(output, ",\"holders\":")?;
        array(output, &device.holders)?;
        write!(output, ",\"slaves\":")?;
        array(output, &device.slaves)?;
        write!(output, "}}")?;
    }
    writeln!(output, "]}}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;

    struct Fixture {
        base: PathBuf,
        class: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let base = crate::scratch::path("inventory");
            let class = base.join("class/block");
            fs::create_dir_all(&class).unwrap();
            Self { base, class }
        }

        fn disk(&self, name: &str, dev: &str, sector: u64) -> PathBuf {
            let path = self.base.join("devices").join(name);
            fs::create_dir_all(path.join("queue")).unwrap();
            fs::create_dir(path.join("device")).unwrap();
            fs::create_dir(path.join("holders")).unwrap();
            fs::create_dir(path.join("slaves")).unwrap();
            for (key, value) in [
                ("dev", dev),
                ("diskseq", "7"),
                ("size", "12582912"),
                ("ro", "0"),
                ("removable", "0"),
            ] {
                fs::write(path.join(key), format!("{value}\n")).unwrap();
            }
            fs::write(path.join("queue/logical_block_size"), format!("{sector}\n")).unwrap();
            symlink(&path, self.class.join(name)).unwrap();
            path
        }

        fn partition(&self, disk: &Path, name: &str, dev: &str) -> PathBuf {
            let path = disk.join(name);
            fs::create_dir_all(path.join("holders")).unwrap();
            for (key, value) in [
                ("dev", dev),
                ("partition", "1"),
                ("size", "2048"),
                ("ro", "0"),
            ] {
                fs::write(path.join(key), format!("{value}\n")).unwrap();
            }
            symlink(&path, self.class.join(name)).unwrap();
            path
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    #[test]
    fn candidates_filter_topology_geometry_and_busy_disks() {
        let fixture = Fixture::new();
        let vda = fixture.disk("vda", "252:0", 512);
        fixture.partition(&vda, "vda1", "252:1");
        fixture.disk("nvme0n1", "259:0", 4096);
        fixture.disk("sda", "8:0", 512);
        for (name, dev, sector, key, value) in [
            ("sdb", "8:16", 512, "ro", "1"),
            ("sdc", "8:32", 512, "size", "100"),
            ("sdd", "8:48", 1024, "ro", "0"),
            ("loop0", "7:0", 512, "ro", "0"),
            ("nvme0c1n1", "259:1", 512, "ro", "0"),
            ("sr0", "11:0", 512, "ro", "0"),
        ] {
            let disk = fixture.disk(name, dev, sector);
            fs::write(disk.join(key), value).unwrap();
        }
        let mut probed = Vec::new();
        let mut output = Vec::new();
        discover(
            || collect(&fixture.class),
            |device| {
                probed.push(device.name.clone());
                Ok(device.name != "sda")
            },
            &mut output,
        )
        .unwrap();
        assert_eq!(probed, ["nvme0n1", "sda", "vda"]);
        let json = String::from_utf8(output).unwrap();
        assert!(json.contains("\"scope\":\"candidate-only\""));
        assert!(json.contains("\"name\":\"vda\""));
        assert!(json.contains("\"name\":\"nvme0n1\""));
        assert!(!json.contains("\"name\":\"sda\""));
        assert!(!json.contains("\"name\":\"vda1\""));

        // A holder of any partition excludes its whole disk even if the
        // disk itself has no holder edge and the claimed device is unmapped.
        let partition = vda.join("vda1");
        let mapped = fixture.disk("dm-0", "253:0", 512);
        symlink(&mapped, partition.join("holders/dm-0")).unwrap();
        symlink(&partition, mapped.join("slaves/vda1")).unwrap();
        let devices = collect(&fixture.class).unwrap();
        assert!(!candidate(
            devices.iter().find(|d| d.name == "vda").unwrap(),
            &devices
        ));
        assert!(!candidate(
            devices.iter().find(|d| d.name == "dm-0").unwrap(),
            &devices
        ));
    }

    #[test]
    fn whole_disk_holder_and_slave_edges_exclude_supported_names() {
        let fixture = Fixture::new();
        let first = fixture.disk("vda", "252:0", 512);
        let second = fixture.disk("sda", "8:0", 512);
        let devices = collect(&fixture.class).unwrap();
        assert!(devices.iter().all(|device| candidate(device, &devices)));
        symlink(&second, first.join("holders/sda")).unwrap();
        symlink(&first, second.join("slaves/vda")).unwrap();
        let devices = collect(&fixture.class).unwrap();
        assert!(devices.iter().all(|device| !candidate(device, &devices)));
    }

    #[test]
    fn candidate_errors_and_changes_publish_nothing() {
        let fixture = Fixture::new();
        let disk = fixture.disk("vda", "252:0", 512);
        let mut output = Vec::new();
        let error = discover(
            || collect(&fixture.class),
            |_| {
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "probe refused",
                ))
            },
            &mut output,
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(output.is_empty());
        let error = discover(
            || collect(&fixture.class),
            |_| {
                fs::write(disk.join("diskseq"), "8").unwrap();
                Ok(true)
            },
            &mut output,
        )
        .unwrap_err();
        assert!(error
            .to_string()
            .contains("changed during candidate discovery"));
        assert!(output.is_empty());
        discover(|| collect(&fixture.class), |_| Ok(false), &mut output).unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "{\"version\":1,\"scope\":\"candidate-only\",\"devices\":[]}\n"
        );
    }

    #[test]
    fn partitions_and_4kn_capacity_use_kernel_relationships_and_512_size_units() {
        let fixture = Fixture::new();
        let disk = fixture.disk("nvme0n1", "259:1", 4096);
        fixture.partition(&disk, "nvme0n1p1", "259:2");
        fs::write(disk.join("device/model"), "A disk\n").unwrap();
        fs::write(disk.join("device/serial"), "serial-1\n").unwrap();
        let devices = collect(&fixture.class).unwrap();
        assert_eq!(devices.len(), 2);
        assert_eq!(devices[0].bytes, 6 * 1024 * 1024 * 1024);
        assert_eq!(devices[0].disk.as_ref().unwrap().logical_sector_bytes, 4096);
        assert_eq!(
            devices[0].disk.as_ref().unwrap().serial.as_deref(),
            Some("serial-1")
        );
        assert_eq!(devices[1].parent.as_deref(), Some("nvme0n1"));
        assert_eq!(devices[1].bytes, 1024 * 1024);
        assert!(devices[1].disk.is_none());
    }

    #[test]
    fn complete_output_is_inventory_only_and_escapes_device_text() {
        let fixture = Fixture::new();
        let path = fixture.disk("vda", "252:0", 512);
        fs::write(path.join("device/model"), "disk\"\\\t\u{1b}\ninside\n").unwrap();
        fs::write(path.join("ro"), "1\n").unwrap();
        let mut output = Vec::new();
        run(&fixture.class, &mut output).unwrap();
        let json = String::from_utf8(output).unwrap();
        assert!(json.starts_with("{\"version\":1,\"scope\":\"inventory-only\",\"devices\":["));
        assert!(
            json.contains("\"model\":\"disk\\\"\\\\\\u0009\\u001b\\u000ainside\",\"serial\":null")
        );
        assert!(json.contains("\"read_only\":true"));
        assert_eq!(json.lines().count(), 1);
    }

    #[test]
    fn holder_graph_requires_reciprocal_resolved_links() {
        let fixture = Fixture::new();
        let disk = fixture.disk("sda", "8:0", 512);
        let part = fixture.partition(&disk, "sda1", "8:1");
        let mapped = fixture.disk("dm-0", "253:0", 512);
        symlink(&mapped, part.join("holders/dm-0")).unwrap();
        fs::remove_file(fixture.class.join("dm-0")).unwrap();
        assert!(collect(&fixture.class)
            .unwrap_err()
            .to_string()
            .contains("unresolved block relationship"));
        symlink(&mapped, fixture.class.join("dm-0")).unwrap();
        assert!(collect(&fixture.class).is_err());
        symlink(&part, mapped.join("slaves/sda1")).unwrap();
        assert!(collect(&fixture.class).is_ok());
        fs::remove_file(mapped.join("slaves/sda1")).unwrap();
        symlink(&disk, mapped.join("slaves/sda1")).unwrap();
        assert!(collect(&fixture.class).is_err());
    }

    #[test]
    fn missing_parent_and_duplicate_device_numbers_refuse_without_output() {
        let fixture = Fixture::new();
        let disk = fixture.disk("vda", "252:0", 512);
        fixture.partition(&disk, "vda1", "252:1");
        fs::remove_file(fixture.class.join("vda")).unwrap();
        let mut output = Vec::new();
        assert!(run(&fixture.class, &mut output).is_err());
        assert!(output.is_empty());
        symlink(&disk, fixture.class.join("vda")).unwrap();
        fixture.disk("vdb", "252:0", 512);
        assert!(run(&fixture.class, &mut output).is_err());
        assert!(output.is_empty());
    }

    #[test]
    fn malformed_missing_and_oversized_attributes_refuse_without_output() {
        let fixture = Fixture::new();
        let disk = fixture.disk("vda", "252:0", 512);
        for value in ["-1", "+1", "18446744073709551615", "", "3:2"] {
            fs::write(disk.join("size"), value).unwrap();
            let mut output = Vec::new();
            assert!(run(&fixture.class, &mut output).is_err());
            assert!(output.is_empty());
        }
        fs::write(disk.join("size"), "2048").unwrap();
        fs::write(disk.join("ro"), "2").unwrap();
        assert!(collect(&fixture.class).is_err());
        fs::write(disk.join("ro"), "0").unwrap();
        fs::write(disk.join("device/model"), vec![b'x'; MAX_TEXT + 1]).unwrap();
        assert!(collect(&fixture.class).is_err());
        fs::write(disk.join("device/model"), [0xff]).unwrap();
        assert!(collect(&fixture.class).is_err());
        fs::remove_file(disk.join("device/model")).unwrap();
        fs::remove_file(disk.join("diskseq")).unwrap();
        assert!(collect(&fixture.class).is_err());
    }

    #[test]
    fn optional_description_errors_are_not_silently_absent() {
        let fixture = Fixture::new();
        let disk = fixture.disk("vda", "252:0", 512);
        fs::create_dir(disk.join("device/model")).unwrap();
        let error = collect(&fixture.class).unwrap_err();
        assert!(error.to_string().contains("device/model"));
    }

    #[test]
    fn observation_detects_replacement_and_capacity_changes() {
        for (attribute, replacement) in [("diskseq", "8"), ("size", "2048")] {
            let fixture = Fixture::new();
            let disk = fixture.disk("vda", "252:0", 512);
            let mut calls = 0;
            let result = observe_twice(|| {
                calls += 1;
                if calls == 2 {
                    fs::write(disk.join(attribute), replacement).unwrap();
                }
                collect(&fixture.class)
            });
            assert!(result
                .unwrap_err()
                .to_string()
                .contains("changed during collection"));
        }
    }

    #[test]
    fn directory_and_relationship_bounds_fail_instead_of_truncating() {
        let fixture = Fixture::new();
        let disk = fixture.disk("vda", "252:0", 512);
        assert!(paths::read_dir_bounded(&fixture.class, 0).is_err());
        assert_eq!(paths::read_dir_bounded(&fixture.class, 1).unwrap().len(), 1);
        symlink(&disk, disk.join("holders/vda")).unwrap();
        let error = links(&disk.join("holders"), &mut 0)
            .unwrap_err()
            .to_string();
        assert!(error.contains("block relationship budget (0 of 16384 entries remaining)"));
        assert!(error.contains("holders"));
        let mut remaining = 1;
        assert_eq!(
            links(&disk.join("holders"), &mut remaining).unwrap(),
            ["vda"]
        );
        assert_eq!(remaining, 0);
        assert!(links(&disk.join("holders"), &mut remaining).is_err());
    }

    #[test]
    fn inventory_accepts_no_path_or_write_operands() {
        let args = |values: &[&str]| {
            values
                .iter()
                .map(std::ffi::OsString::from)
                .collect::<Vec<_>>()
                .into_iter()
        };
        assert_eq!(
            crate::parse_args(args(&["inventory"])).unwrap(),
            crate::Mode::Inventory
        );
        for values in [
            vec!["inventory", "/dev/vda"],
            vec!["inventory", "--uuid", "anything"],
            vec!["inventory", "--trusted-key", "/key"],
        ] {
            assert!(crate::parse_args(args(&values)).is_err());
        }
    }
    #[test]
    fn empty_inventory_has_a_complete_document() {
        let fixture = Fixture::new();
        let mut output = Vec::new();
        run(&fixture.class, &mut output).unwrap();
        assert_eq!(
            output,
            b"{\"version\":1,\"scope\":\"inventory-only\",\"devices\":[]}\n"
        );
    }

    #[test]
    fn optional_identity_preference_and_exact_text_bound_are_preserved() {
        let fixture = Fixture::new();
        let disk = fixture.disk("sda", "8:0", 0);
        fs::write(disk.join("device/serial"), "fallback-serial").unwrap();
        fs::write(disk.join("device/wwid"), "naa.disk-1").unwrap();
        fs::write(disk.join("device/model"), vec![b'x'; MAX_TEXT]).unwrap();
        let devices = collect(&fixture.class).unwrap();
        let record = devices[0].disk.as_ref().unwrap();
        assert_eq!(record.model.as_ref().unwrap().len(), MAX_TEXT);
        assert_eq!(record.serial.as_deref(), Some("fallback-serial"));
        assert_eq!(record.wwid.as_deref(), Some("naa.disk-1"));
        assert_eq!(record.logical_sector_bytes, 0);
        fs::write(disk.join("serial"), "direct-serial").unwrap();
        fs::write(disk.join("wwid"), "direct-wwid").unwrap();
        let devices = collect(&fixture.class).unwrap();
        let record = devices[0].disk.as_ref().unwrap();
        assert_eq!(record.serial.as_deref(), Some("direct-serial"));
        assert_eq!(record.wwid.as_deref(), Some("direct-wwid"));
        fs::write(disk.join("serial"), "").unwrap();
        let devices = collect(&fixture.class).unwrap();
        assert_eq!(
            devices[0].disk.as_ref().unwrap().serial.as_deref(),
            Some("")
        );
        fixture.partition(&disk, "sda1", "8:1");
        let second = fixture.partition(&disk, "sda2", "8:2");
        fs::write(second.join("partition"), "2").unwrap();
        let mut output = Vec::new();
        run(&fixture.class, &mut output).unwrap();
        let json = String::from_utf8(output).unwrap();
        assert!(json.contains("\"partition_number\":2"));
        assert!(json.contains("\"wwid\":\"direct-wwid\""));
    }
    #[test]
    fn unavailable_vpd_is_absent_but_other_identification_errors_refuse() {
        struct Failure(i32);
        impl Read for Failure {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::from_raw_os_error(self.0))
            }
        }
        let path = Path::new("/sys/example/device/serial");
        assert_eq!(identification_from(Failure(6), path).unwrap(), None);
        for errno in [5, 13, 22] {
            assert!(identification_from(Failure(errno), path)
                .unwrap_err()
                .to_string()
                .contains("device/serial"));
        }
        assert!(identification_from(&vec![b'x'; MAX_TEXT + 1][..], path).is_err());
        assert!(identification_from(&[0xff][..], path).is_err());
        assert_eq!(
            identification_from(&vec![b'x'; MAX_TEXT][..], path)
                .unwrap()
                .unwrap()
                .len(),
            MAX_TEXT
        );
    }
}
