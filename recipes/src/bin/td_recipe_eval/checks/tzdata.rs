use std::fs::{self, File};
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use super::rust_toolchain::{path_basename, GLIBC_STAGE};
use crate::check_runner::{RecipeCheckRunner, TD_STORE_DIR};

use td_recipe::td_compositor_timezone::Zone;
use td_recipe::td_install_timezones as installer_timezones;

const TABLES: &[&str] = &["iso3166.tab", "zone.tab", "zone1970.tab", "zonenow.tab"];
const MAX_FILE_BYTES: u64 = 1024 * 1024;
const MAX_ENTRIES: usize = 2048;

pub(super) fn run(runner: &RecipeCheckRunner) -> Result<(), String> {
    runner.prepare_recipe_target("tzdata")?;
    let build_out = runner.build_plan("tzdata")?;
    let output = runner.ladder_out_from(&build_out, "tzdata")?;
    let glibc = runner.ladder_out_from(&build_out, "glibc-x86-64")?;
    let zones = verify_tree(&output)?;
    verify_clock_catalog(&output.join("share/zoneinfo"))?;
    let glibc = format!("{TD_STORE_DIR}/{}/{GLIBC_STAGE}", path_basename(&glibc)?);
    let zoneinfo = format!("{TD_STORE_DIR}/{}/share/zoneinfo", path_basename(&output)?);
    for (name, years, rows) in EXPECTED {
        let zone = format!("{zoneinfo}/{name}");
        let report = runner.store_ns_output(
            &[
                &format!("{glibc}/lib/ld-linux-x86-64.so.2"),
                "--library-path",
                &format!("{glibc}/lib"),
                &format!("{glibc}/bin/zdump"),
                "-i",
                "-c",
                years,
                &zone,
            ],
            None,
        )?;
        verify_report(&report, &zone, rows)?;
    }
    println!("PASS: tzdata 2026d: {zones} TZif files; td glibc reads UTC, DST transitions, fixed offsets and six Canadian zones through the 2026 transition; the native clock reads all installer choices and exact winter/summer offsets in 2100");
    Ok(())
}

const WINTER_2100: i64 = 4_102_444_800;
const SUMMER_2100: i64 = 4_118_083_200;
const CLOCK_OFFSETS: &[(&str, i32, i32)] = &[
    ("Etc/UTC", 0, 0),
    ("America/Los_Angeles", -28_800, -25_200),
    ("Europe/London", 0, 3600),
    ("Asia/Tokyo", 32_400, 32_400),
    ("Asia/Kathmandu", 20_700, 20_700),
    ("Australia/Lord_Howe", 39_600, 37_800),
    ("Pacific/Chatham", 49_500, 45_900),
    ("Europe/Dublin", 0, 3600),
    ("America/Nuuk", -7200, -3600),
    ("Pacific/Auckland", 46_800, 43_200),
    ("America/Inuvik", -21_600, -21_600),
    ("America/Vancouver", -25_200, -25_200),
];

fn verify_clock_catalog(root: &Path) -> Result<(), String> {
    let catalog = installer_timezones::Catalog::load(root)
        .map_err(|error| format!("tzdata: clock catalog: {error}"))?;
    for name in catalog.ids() {
        let bytes = read_regular(&root.join(name))?;
        let zone = Zone::parse(&bytes)
            .ok_or_else(|| format!("tzdata: native clock cannot parse {name}"))?;
        // Some Antarctic zones intentionally leave pre-settlement time unknown.
        for at in [1_767_225_600, 1_782_864_000, WINTER_2100, SUMMER_2100] {
            if zone.offset_at(at).is_none() {
                return Err(format!(
                    "tzdata: native clock has no offset for {name} at {at}"
                ));
            }
        }
    }
    for (name, winter, summer) in CLOCK_OFFSETS {
        let bytes = read_regular(&root.join(name))?;
        verify_clock_offsets(name, &bytes, *winter, *summer)?;
    }
    Ok(())
}

fn verify_clock_offsets(name: &str, bytes: &[u8], winter: i32, summer: i32) -> Result<(), String> {
    let zone =
        Zone::parse(bytes).ok_or_else(|| format!("tzdata: native clock cannot parse {name}"))?;
    for (at, expected) in [(WINTER_2100, winter), (SUMMER_2100, summer)] {
        let actual = zone.offset_at(at);
        if actual != Some(expected) {
            return Err(format!(
                "tzdata: native clock {name} at {at}: expected {expected}, got {actual:?}"
            ));
        }
    }
    Ok(())
}

// Exact civil-time expectations for this pin, including 2026d's Inuvik change.
const CANADA_MOUNTAIN_2026: &str =
    "-\t-\t-07\tMST\n2026-03-08\t03\t-06\tMDT\t1\n2026-11-01\t02\t-06\tCST\n";
const CANADA_PACIFIC_2026: &str =
    "-\t-\t-08\tPST\n2026-03-08\t03\t-07\tPDT\t1\n2026-11-01\t02\t-07\tMST\n";
const EXPECTED: &[(&str, &str, &str)] = &[
    ("Etc/UTC", "2027,2028", "-\t-\t+00\tUTC\n"),
    (
        "America/Los_Angeles",
        "2027,2028",
        "-\t-\t-08\tPST\n2027-03-14\t03\t-07\tPDT\t1\n2027-11-07\t01\t-08\tPST\n",
    ),
    (
        "Europe/London",
        "2027,2028",
        "-\t-\t+00\tGMT\n2027-03-28\t02\t+01\tBST\t1\n2027-10-31\t01\t+00\tGMT\n",
    ),
    ("Asia/Tokyo", "2027,2028", "-\t-\t+09\tJST\n"),
    ("America/Inuvik", "2027,2028", "-\t-\t-06\tCST\n"),
    ("America/Edmonton", "2026,2027", CANADA_MOUNTAIN_2026),
    ("America/Inuvik", "2026,2027", CANADA_MOUNTAIN_2026),
    ("America/Yellowknife", "2026,2027", CANADA_MOUNTAIN_2026),
    ("Canada/Mountain", "2026,2027", CANADA_MOUNTAIN_2026),
    ("America/Vancouver", "2026,2027", CANADA_PACIFIC_2026),
    ("Canada/Pacific", "2026,2027", CANADA_PACIFIC_2026),
];

fn verify_report(report: &str, zone: &str, rows: &str) -> Result<(), String> {
    let expected = format!("\nTZ=\"{zone}\"\n{rows}");
    if report != expected {
        return Err(format!("tzdata: unexpected zdump report for {zone}: expected {expected:?}, observed {report:?}"));
    }
    Ok(())
}

fn read_regular(path: &Path) -> Result<Vec<u8>, String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("tzdata: inspect {}: {error}", path.display()))?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o111 != 0 {
        return Err(format!(
            "tzdata: expected non-executable regular file {}",
            path.display()
        ));
    }
    let mut bytes = Vec::new();
    File::open(path)
        .and_then(|file| file.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes))
        .map_err(|error| format!("tzdata: read {}: {error}", path.display()))?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(format!("tzdata: oversized file {}", path.display()));
    }
    Ok(bytes)
}

fn verify_tree(output: &Path) -> Result<usize, String> {
    let version = output.join("share/doc/tzdata/version");
    let license = output.join("share/doc/tzdata/LICENSE");
    if read_regular(&version)? != b"2026d\n" {
        return Err("tzdata: unexpected release version".into());
    }
    if read_regular(&license)?.is_empty() {
        return Err("tzdata: missing license text".into());
    }
    let root = output.join("share/zoneinfo");
    installer_timezones::run(&root, &mut std::io::sink())
        .map_err(|error| format!("tzdata: installer catalog: {error}"))?;
    for (alias, zone) in [("UTC", "Etc/UTC"), ("US/Pacific", "America/Los_Angeles")] {
        if read_regular(&root.join(alias))? != read_regular(&root.join(zone))? {
            return Err(format!(
                "tzdata: compatibility alias {alias} differs from {zone}"
            ));
        }
    }
    for table in TABLES {
        let bytes = read_regular(&root.join(table))?;
        if bytes.is_empty() {
            return Err(format!("tzdata: empty geographic table {table}"));
        }
        if *table != "iso3166.tab" {
            verify_table(&root, &bytes)?;
        }
    }
    let mut pending = vec![output.to_path_buf()];
    let mut entries = 0usize;
    let mut zones = 0usize;
    let mut total = 0usize;
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory)
            .map_err(|error| format!("tzdata: list {}: {error}", directory.display()))?
        {
            let entry = entry.map_err(|error| format!("tzdata: read directory entry: {error}"))?;
            entries += 1;
            if entries > MAX_ENTRIES {
                return Err("tzdata: too many output entries".into());
            }
            let path = entry.path();
            let kind = entry
                .file_type()
                .map_err(|error| format!("tzdata: file type: {error}"))?;
            if kind.is_dir() {
                pending.push(path);
                continue;
            }
            let bytes = read_regular(&path)?;
            total += bytes.len();
            if total > 8 * 1024 * 1024 {
                return Err("tzdata: oversized zoneinfo tree".into());
            }
            if path == version || path == license {
                continue;
            }
            let relative = path
                .strip_prefix(&root)
                .map_err(|_| format!("tzdata: unexpected output file {}", path.display()))?;
            if TABLES.iter().any(|table| relative == Path::new(table)) {
                continue;
            }
            if bytes.len() < 44 || !matches!(bytes.get(..5), Some(b"TZif2" | b"TZif3")) {
                return Err(format!(
                    "tzdata: missing TZif v2/v3 header in {}",
                    path.display()
                ));
            }
            zones += 1;
        }
    }
    if zones == 0 {
        return Err("tzdata: no compiled zones".into());
    }
    Ok(zones)
}

fn verify_table(root: &Path, bytes: &[u8]) -> Result<(), String> {
    let text =
        std::str::from_utf8(bytes).map_err(|error| format!("tzdata: table UTF-8: {error}"))?;
    let mut rows = 0;
    for line in text
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
    {
        if rows >= MAX_ENTRIES {
            return Err("tzdata: too many geographic rows".into());
        }
        let zone = line
            .split('\t')
            .nth(2)
            .ok_or("tzdata: geographic row lacks a zone")?;
        if zone
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
            || !zone
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"/_-+".contains(&byte))
        {
            return Err(format!("tzdata: invalid geographic zone name {zone:?}"));
        }
        let bytes = read_regular(&root.join(zone))?;
        if !matches!(bytes.get(..5), Some(b"TZif2" | b"TZif3")) {
            return Err(format!("tzdata: geographic zone {zone} is not compiled"));
        }
        rows += 1;
    }
    if rows == 0 {
        return Err("tzdata: geographic table contains no zones".into());
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn clock_oracle_rejects_a_frozen_or_wrong_future_offset() {
        let mut bytes = Vec::new();
        for _ in 0..2 {
            bytes.extend_from_slice(b"TZif2");
            bytes.extend_from_slice(&[0; 15]);
            for count in [0u32, 0, 0, 0, 1, 4] {
                bytes.extend_from_slice(&count.to_be_bytes());
            }
            bytes.extend_from_slice(&[0; 6]);
            bytes.extend_from_slice(b"UTC\0");
        }
        bytes.extend_from_slice(b"\nUTC0\n");
        verify_clock_offsets("Etc/UTC", &bytes, 0, 0).unwrap();
        assert!(verify_clock_offsets("Europe/London", &bytes, 0, 3600).is_err());
        assert!(verify_clock_offsets("Asia/Tokyo", &bytes, 32_400, 32_400).is_err());
        bytes.pop();
        assert!(verify_clock_offsets("Etc/UTC", &bytes, 0, 0).is_err());
    }

    struct Scratch(std::path::PathBuf);

    impl Scratch {
        fn new() -> Self {
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path =
                std::env::temp_dir().join(format!("td-tzdata-{}-{nonce}", std::process::id()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn zdump_checks_require_exact_offsets_transitions_and_zone_identity() {
        for (name, _, rows) in EXPECTED {
            let report = format!("\nTZ=\"{name}\"\n{rows}");
            verify_report(&report, name, rows).unwrap();
            assert!(verify_report(&report, "another-zone", rows).is_err());
            if report.contains("2027-03-") {
                assert!(
                    verify_report(&report.replace("2027-03-", "2027-04-"), name, rows).is_err()
                );
            }
            assert!(verify_report(&format!("{report}extra\n"), name, rows).is_err());
            assert!(verify_report("", name, rows).is_err());
        }
        let report = "\nTZ=\"Asia/Tokyo\"\n-\t-\t+00\tJST\n";
        assert!(verify_report(report, "Asia/Tokyo", "-\t-\t+09\tJST\n").is_err());
    }

    #[test]
    fn geographic_tables_refuse_missing_zones_and_path_escapes() {
        let scratch = Scratch::new();
        let root = &scratch.0;
        fs::write(root.join("UTC"), b"TZif2").unwrap();
        verify_table(root, b"# comment\nXX\t+0000+00000\tUTC\n").unwrap();
        for row in [
            "XX\t0\tmissing",
            "XX\t0\t../UTC",
            "XX\t0\t/UTC",
            "XX\t0\tUTC//x",
            "XX\t0",
            "# no zones",
        ] {
            assert!(verify_table(root, row.as_bytes()).is_err(), "{row}");
        }
        fs::write(root.join("UTC"), b"not compiled").unwrap();
        assert!(verify_table(root, b"XX\t0\tUTC").is_err());
    }

    #[test]
    fn realized_tree_rejects_missing_aliases_bad_headers_and_executables() {
        let scratch = Scratch::new();
        let root = &scratch.0;
        let zones = root.join("share/zoneinfo");
        let docs = root.join("share/doc/tzdata");
        for path in [
            zones.join("Etc"),
            zones.join("America"),
            zones.join("US"),
            docs.clone(),
        ] {
            fs::create_dir_all(path).unwrap();
        }
        fs::write(docs.join("version"), b"2026d\n").unwrap();
        fs::write(docs.join("LICENSE"), b"fixture license").unwrap();
        let mut header = vec![0u8; 44];
        header.get_mut(..5).unwrap().copy_from_slice(b"TZif2");
        for name in ["Etc/UTC", "UTC", "America/Los_Angeles", "US/Pacific"] {
            fs::write(zones.join(name), &header).unwrap();
        }
        for name in TABLES {
            let row = if *name == "iso3166.tab" {
                "US\tUnited States\n"
            } else {
                "US\t+340308-1181434\tAmerica/Los_Angeles\n"
            };
            fs::write(zones.join(name), row).unwrap();
        }
        assert_eq!(verify_tree(root).unwrap(), 4);
        fs::write(zones.join("iso3166.tab"), b"CA\tCanada\n").unwrap();
        let error = verify_tree(root).unwrap_err();
        assert!(error.contains("installer catalog") && error.contains("zone1970.tab:1"));
        fs::write(zones.join("iso3166.tab"), b"US\tUnited States\n").unwrap();
        assert_eq!(verify_tree(root).unwrap(), 4);
        let alias = zones.join("US/Pacific");
        fs::remove_file(&alias).unwrap();
        assert!(verify_tree(root).is_err());
        fs::write(&alias, &header).unwrap();
        let extra = zones.join("extra");
        fs::write(&extra, b"TZif2").unwrap();
        assert!(verify_tree(root).is_err());
        fs::write(&extra, &header).unwrap();
        fs::set_permissions(&extra, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(verify_tree(root).is_err());
        fs::remove_file(&extra).unwrap();
        std::os::unix::fs::symlink("Etc/UTC", &extra).unwrap();
        assert!(verify_tree(root).is_err());
        fs::remove_file(&extra).unwrap();
        fs::write(root.join("unexpected"), b"unlisted payload").unwrap();
        assert!(verify_tree(root).is_err());
    }
}
