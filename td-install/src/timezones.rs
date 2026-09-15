//! Read-only geographic catalog. The deployment owns the immutable root;
//! regular-file reads reject symlink leaves but do not anchor its parents.

use super::realfile;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};
use std::path::Path;

fn invalid(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

const MAX_TABLE: u64 = 128 * 1024;
const MAX_LINE: usize = 2048;
const MAX_COUNTRIES: usize = 512;
const MAX_ZONES: usize = 1024;
const MAX_TZIF: u64 = 64 * 1024;

#[derive(Debug, Eq, PartialEq)]
struct Zone {
    countries: Vec<String>,
    comment: String,
}

fn table(root: &Path, name: &str) -> io::Result<String> {
    let bytes = realfile::read_bounded_real_file(&root.join(name), name, MAX_TABLE)?;
    let text = String::from_utf8(bytes).map_err(|_| invalid(format!("{name}: invalid UTF-8")))?;
    for (index, line) in text.lines().enumerate() {
        if line.len() > MAX_LINE || line.chars().any(|c| c.is_control() && c != '\t') {
            return Err(invalid(format!(
                "{name}:{}: oversized line or control character",
                index + 1
            )));
        }
    }
    Ok(text)
}

fn rows(text: &str) -> impl Iterator<Item = (usize, &str)> {
    text.lines().enumerate().filter_map(|(i, line)| {
        (!line.is_empty() && !line.starts_with('#')).then_some((i + 1, line))
    })
}

fn country_code(code: &str) -> bool {
    code.len() == 2 && code.bytes().all(|b| b.is_ascii_uppercase())
}

fn countries(text: &str) -> io::Result<BTreeMap<String, String>> {
    let mut result = BTreeMap::new();
    for (row, line) in rows(text) {
        let fields: Vec<_> = line.split('\t').collect();
        let [code, name] = fields.as_slice() else {
            return Err(invalid(format!(
                "iso3166.tab:{row}: expected code and name"
            )));
        };
        if !country_code(code) || name.trim().is_empty() || name.len() > 256 {
            return Err(invalid(format!("iso3166.tab:{row}: invalid code or name")));
        }
        if result
            .insert((*code).to_owned(), (*name).to_owned())
            .is_some()
            || result.len() > MAX_COUNTRIES
        {
            return Err(invalid(format!(
                "iso3166.tab:{row}: duplicate or too many countries"
            )));
        }
    }
    if result.is_empty() {
        return Err(invalid("iso3166.tab: no countries".into()));
    }
    Ok(result)
}

fn zone_id(id: &str) -> bool {
    id.len() <= 128
        && id.contains('/')
        && id.split('/').all(|part| {
            !part.is_empty()
                && part != "."
                && part != ".."
                && part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_+-".contains(&b))
        })
}

fn zones(text: &str, countries: &BTreeMap<String, String>) -> io::Result<BTreeMap<String, Zone>> {
    let mut result = BTreeMap::new();
    for (row, line) in rows(text) {
        let fields: Vec<_> = line.split('\t').collect();
        let (codes, coordinates, id, comment) = match fields.as_slice() {
            [codes, coordinates, id] => (*codes, *coordinates, *id, ""),
            [codes, coordinates, id, comment] => (*codes, *coordinates, *id, *comment),
            _ => {
                return Err(invalid(format!(
                    "zone1970.tab:{row}: expected three or four fields"
                )))
            }
        };
        // Coordinates are unused display metadata; require the upstream shape.
        let coordinate_shape = matches!(coordinates.len(), 11 | 15)
            && coordinates.bytes().enumerate().all(|(i, b)| {
                if i == 0 || i == (coordinates.len() - 1) / 2 {
                    matches!(b, b'+' | b'-')
                } else {
                    b.is_ascii_digit()
                }
            });
        if !zone_id(id) || id == "Etc/UTC" || !coordinate_shape || comment.len() > 512 {
            return Err(invalid(format!(
                "zone1970.tab:{row}: invalid zone, coordinates or comment"
            )));
        }
        let mut seen = BTreeSet::new();
        let mut selected = Vec::new();
        for code in codes.split(',') {
            if !countries.contains_key(code) || !seen.insert(code) {
                return Err(invalid(format!(
                    "zone1970.tab:{row}: unknown or repeated country {code:?}"
                )));
            }
            selected.push(code.to_owned());
        }
        if result
            .insert(
                id.to_owned(),
                Zone {
                    countries: selected,
                    comment: comment.to_owned(),
                },
            )
            .is_some()
            // Reserve one catalog slot for the synthetic UTC entry.
            || result.len() >= MAX_ZONES
        {
            return Err(invalid(format!(
                "zone1970.tab:{row}: duplicate or too many zones"
            )));
        }
    }
    if result.is_empty() {
        return Err(invalid("zone1970.tab: no geographic zones".into()));
    }
    if result
        .insert(
            "Etc/UTC".into(),
            Zone {
                countries: Vec::new(),
                comment: "Coordinated Universal Time".into(),
            },
        )
        .is_some()
    {
        return Err(invalid("zone1970.tab: UTC is not a geographic zone".into()));
    }
    Ok(result)
}

fn quoted(output: &mut impl Write, text: &str) -> io::Result<()> {
    write!(output, "\"")?;
    for c in text.chars() {
        match c {
            '"' | '\\' => write!(output, "\\{c}")?,
            c if c <= '\u{1f}' => write!(output, "\\u{:04x}", u32::from(c))?,
            c => write!(output, "{c}")?,
        }
    }
    write!(output, "\"")
}

pub fn run(root: &Path, output: &mut impl Write) -> io::Result<()> {
    let countries = countries(&table(root, "iso3166.tab")?)?;
    let zones = zones(&table(root, "zone1970.tab")?, &countries)?;
    // Validate every advertised file before writing even the JSON prefix.
    // This is header screening; the tzdata recipe owns transition validation.
    for id in zones.keys() {
        let bytes = realfile::read_bounded_real_file(&root.join(id), id, MAX_TZIF)?;
        if bytes.len() < 44
            || bytes.get(..4) != Some(b"TZif")
            || !matches!(bytes.get(4), Some(b'2' | b'3'))
        {
            return Err(invalid(format!("{id}: expected a TZif v2/v3 header")));
        }
    }
    write!(
        output,
        "{{\"version\":1,\"source\":\"zone1970.tab\",\"timezones\":["
    )?;
    for (i, (id, zone)) in zones.iter().enumerate() {
        if i != 0 {
            write!(output, ",")?;
        }
        write!(output, "{{\"id\":")?;
        quoted(output, id)?;
        write!(output, ",\"countries\":[")?;
        for (j, code) in zone.countries.iter().enumerate() {
            if j != 0 {
                write!(output, ",")?;
            }
            // Every country reference was resolved while parsing the table.
            let name = countries
                .get(code)
                .ok_or_else(|| invalid("missing catalog country".into()))?;
            write!(output, "{{\"code\":")?;
            quoted(output, code)?;
            write!(output, ",\"name\":")?;
            quoted(output, name)?;
            write!(output, "}}")?;
        }
        write!(output, "],\"comment\":")?;
        quoted(output, &zone.comment)?;
        write!(output, "}}")?;
    }
    writeln!(output, "]}}")
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::DirBuilderExt;
    use std::path::PathBuf;

    fn path(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "td-timezones-{tag}-{}-{stamp}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
    }

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let root = path("timezones");
            fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
            let fixture = Self(root);
            fixture.write(
                "iso3166.tab",
                b"# countries\nUS\tUnited States\nCA\tCanada\n",
            );
            fixture.write("zone1970.tab", b"# zones\nUS\t+340308-1181434\tAmerica/Los_Angeles\tPacific\nCA,US\t+4906-11631\tAmerica/Creston\n");
            let mut header = vec![0; 44];
            header[..5].copy_from_slice(b"TZif2");
            for id in ["America/Los_Angeles", "America/Creston", "Etc/UTC"] {
                fixture.write(id, &header);
            }
            fixture
        }

        fn write(&self, name: &str, bytes: &[u8]) {
            let path = self.0.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, bytes).unwrap();
        }

        fn refused(&self) {
            let mut output = Vec::new();
            assert!(run(&self.0, &mut output).is_err());
            assert!(output.is_empty());
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn sorted_geographic_catalog_has_countries_and_utc_but_no_aliases() {
        let fixture = Fixture::new();
        fixture.write("US/Pacific", b"not read");
        let mut output = Vec::new();
        run(&fixture.0, &mut output).unwrap();
        assert_eq!(String::from_utf8(output).unwrap(), concat!(
            "{\"version\":1,\"source\":\"zone1970.tab\",\"timezones\":[",
            "{\"id\":\"America/Creston\",\"countries\":[{\"code\":\"CA\",\"name\":\"Canada\"},{\"code\":\"US\",\"name\":\"United States\"}],\"comment\":\"\"},",
            "{\"id\":\"America/Los_Angeles\",\"countries\":[{\"code\":\"US\",\"name\":\"United States\"}],\"comment\":\"Pacific\"},",
            "{\"id\":\"Etc/UTC\",\"countries\":[],\"comment\":\"Coordinated Universal Time\"}]}\n"
        ));
    }

    #[test]
    fn metadata_is_bounded_and_ambiguous_rows_refuse() {
        let fixture = Fixture::new();
        for table in [
            "",
            "US\tUnited States\nUS\tDuplicate\n",
            "ZZZ\tName\n",
            "US\t\n",
            "US\tName\textra\n",
            "US\tbad\u{1b}\n",
        ] {
            fixture.write("iso3166.tab", table.as_bytes());
            fixture.refused();
        }
        fixture.write("iso3166.tab", &[b'x'; MAX_LINE + 1]);
        fixture.refused();
        fixture.write("iso3166.tab", &vec![b'\n'; MAX_TABLE as usize + 1]);
        fixture.refused();
        fixture.write("iso3166.tab", b"US\t\xff\n");
        fixture.refused();
        fixture.write(
            "iso3166.tab",
            format!("US\t{}\n", "n".repeat(257)).as_bytes(),
        );
        fixture.refused();
        fixture.write("iso3166.tab", b"US\tUnited States\n");
        for row in [
            "",
            "US\t+3403-11814\tAmerica/Los_Angeles\nUS\t+3403-11814\tAmerica/Los_Angeles\n",
            "XX\t+3403-11814\tAmerica/Los_Angeles\n",
            "US,US\t+3403-11814\tAmerica/Los_Angeles\n",
            "US\tbroken\tAmerica/Los_Angeles\n",
            "US\t+3403-11814\tEtc/UTC\n",
            "US\t+3403-11814\tAmerica/Los_Angeles\tcomment\textra\n",
        ] {
            fixture.write("zone1970.tab", row.as_bytes());
            fixture.refused();
        }
    }

    #[test]
    fn oversized_comment_refuses() {
        let fixture = Fixture::new();
        fixture.write(
            "zone1970.tab",
            format!(
                "US\t+3403-11814\tAmerica/Los_Angeles\t{}\n",
                "c".repeat(513)
            )
            .as_bytes(),
        );
        fixture.refused();
    }

    #[test]
    fn diagnostics_count_comment_and_empty_lines() {
        let fixture = Fixture::new();
        fixture.write(
            "zone1970.tab",
            b"# comment\n\nUS\tbroken\tAmerica/Los_Angeles\n",
        );
        let error = run(&fixture.0, &mut Vec::new()).unwrap_err();
        assert!(error.to_string().contains("zone1970.tab:3:"));
    }

    #[test]
    fn zone_keys_cannot_escape_the_catalog() {
        for id in [
            "../etc/passwd",
            "/etc/passwd",
            "A/../B",
            "A/./B",
            "A//B",
            "A/B/",
            "A/B\\C",
            "A/B.C",
            "UTC",
            "A/é",
        ] {
            assert!(!zone_id(id), "{id}");
        }
        assert!(zone_id("America/Argentina/Buenos_Aires"));
        assert!(zone_id("Etc/GMT+12"));
        assert!(!zone_id(&format!("A/{}", "x".repeat(128))));
    }

    #[test]
    fn missing_truncated_wrong_format_and_oversized_files_refuse() {
        let fixture = Fixture::new();
        for bytes in [
            Vec::new(),
            b"TZif2".to_vec(),
            vec![0; 44],
            vec![0; MAX_TZIF as usize + 1],
        ] {
            fixture.write("Etc/UTC", &bytes);
            fixture.refused();
        }
        for version in [0, b'1', b'4', b'x'] {
            let mut header = vec![0; 44];
            header[..4].copy_from_slice(b"TZif");
            header[4] = version;
            fixture.write("Etc/UTC", &header);
            fixture.refused();
        }
        let mut header = vec![0; 44];
        header[..5].copy_from_slice(b"TZif3");
        fixture.write("Etc/UTC", &header);
        run(&fixture.0, &mut Vec::new()).unwrap();
        fs::remove_file(fixture.0.join("Etc/UTC")).unwrap();
        fixture.refused();
        fs::create_dir(fixture.0.join("Etc/UTC")).unwrap();
        fixture.refused();
    }

    #[test]
    fn leaf_symlinks_refuse_and_the_deployment_root_link_works() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new();
        let alias = path("zoneinfo-link");
        symlink(&fixture.0, &alias).unwrap();
        run(&alias, &mut Vec::new()).unwrap();
        fs::remove_file(alias).unwrap();
        fs::remove_file(fixture.0.join("Etc/UTC")).unwrap();
        symlink("../America/Creston", fixture.0.join("Etc/UTC")).unwrap();
        fixture.refused();
        let fixture = Fixture::new();
        fs::rename(fixture.0.join("iso3166.tab"), fixture.0.join("countries")).unwrap();
        symlink("countries", fixture.0.join("iso3166.tab")).unwrap();
        fixture.refused();
    }

    #[test]
    fn entry_limits_include_the_synthetic_utc_zone() {
        let country_table: String = (0..MAX_COUNTRIES + 1)
            .map(|i| {
                format!(
                    "{}{}\tCountry\n",
                    char::from(b'A' + (i / 26) as u8),
                    char::from(b'A' + (i % 26) as u8)
                )
            })
            .collect();
        assert!(countries(&country_table).is_err());
        let countries = countries("US\tUnited States\n").unwrap();
        let mut zone_table: String = (0..MAX_ZONES - 1)
            .map(|i| format!("US\t+3403-11814\tAmerica/Zone_{i}\n"))
            .collect();
        assert_eq!(zones(&zone_table, &countries).unwrap().len(), MAX_ZONES);
        zone_table.push_str("US\t+3403-11814\tAmerica/One_more\n");
        assert!(zones(&zone_table, &countries).is_err());
    }

    #[test]
    fn quoted_text_and_writer_failure_are_preserved() {
        let mut bytes = Vec::new();
        quoted(&mut bytes, "Côte \"name\"\\\n").unwrap();
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            "\"Côte \\\"name\\\"\\\\\\u000a\""
        );
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        assert_eq!(
            run(&Fixture::new().0, &mut Broken).unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
    }

    #[test]
    #[ignore = "requires the explicitly supplied realized IANA 2026d output"]
    fn realized_iana_2026d_catalog() {
        let root = std::env::var_os("TD_TEST_ZONEINFO_ROOT")
            .expect("set TD_TEST_ZONEINFO_ROOT to the realized share/zoneinfo directory");
        let root = Path::new(&root);
        let countries = countries(&table(root, "iso3166.tab").unwrap()).unwrap();
        let zones = zones(&table(root, "zone1970.tab").unwrap(), &countries).unwrap();
        assert_eq!(countries.len(), 249);
        assert_eq!(zones.len(), 313);
        for id in [
            "Etc/UTC",
            "America/Vancouver",
            "Asia/Tokyo",
            "Europe/London",
        ] {
            assert!(zones.contains_key(id));
        }
        run(root, &mut Vec::new()).unwrap();
    }
}
