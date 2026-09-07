//! Transfer the checkout's exact application graph pins through the host feed.
use super::*;
use crate::ostree::{self, AcquireSpec, GraphStats};
use std::collections::BTreeSet;

const MAX_PLAN_BYTES: u64 = 1024 * 1024;
const MAX_PINS: usize = 128;

struct Pin {
    key: String,
    cache: String,
    spec: AcquireSpec,
    expected: [u64; 6],
}

impl Pin {
    fn validate(&self, stats: GraphStats) -> Result<(), String> {
        let actual = [
            stats.objects as u64,
            stats.paths as u64,
            stats.directories as u64,
            stats.regular_files as u64,
            stats.symlinks as u64,
            stats.decoded_bytes,
        ];
        for ((label, actual), expected) in [
            "objects",
            "paths",
            "directories",
            "regular",
            "symlinks",
            "decoded-bytes",
        ]
        .into_iter()
        .zip(actual)
        .zip(self.expected)
        {
            if actual != expected {
                return Err(format!(
                    "{} graph {label} is {actual}, but its recipe pin requires {expected}",
                    self.key
                ));
            }
        }
        Ok(())
    }
}

fn plain_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && !value.starts_with('.')
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
}

fn count(value: &str, label: &str) -> Result<u64, String> {
    let digits = value
        .strip_prefix(label)
        .ok_or_else(|| format!("missing graph field {label}"))?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("invalid graph count {value}"));
    }
    digits
        .parse()
        .map_err(|_| format!("graph count overflows: {value}"))
}

fn parse_pins(text: &str) -> Result<Vec<Pin>, String> {
    if text.len() as u64 > MAX_PLAN_BYTES {
        return Err("graph pin roster exceeds byte limit".into());
    }
    let mut pins = Vec::new();
    let mut keys = BTreeSet::new();
    let mut caches = BTreeSet::new();
    for line in text.lines().filter(|line| !line.is_empty()) {
        let fields: Vec<_> = line.split('\t').collect();
        let [key, repository, exact_ref, commit, content, fingerprint, cache, objects, paths, directories, regular, symlinks, decoded, transfer] =
            fields.as_slice()
        else {
            return Err("OSTree pin must have exactly fourteen TSV fields".into());
        };
        if !plain_name(key) {
            return Err(format!("invalid graph key {key:?}"));
        }
        if !plain_name(cache) {
            return Err(format!("invalid graph cache name {cache:?}"));
        }
        // This records pin review, not repeated signature verification; checksums authorize bytes.
        if fingerprint.len() != 40 || !fingerprint.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(format!("invalid reviewed fingerprint for graph {key}"));
        }
        if !keys.insert(*key) {
            return Err(format!("duplicate graph key {key}"));
        }
        if !caches.insert(*cache) {
            return Err(format!("duplicate graph cache destination {cache}"));
        }
        let expected = [
            count(objects, "objects=")?,
            count(paths, "paths=")?,
            count(directories, "directories=")?,
            count(regular, "regular=")?,
            count(symlinks, "symlinks=")?,
            count(decoded, "decoded-bytes=")?,
        ];
        // Upstream may recompress filez; observed transport size is not identity.
        let _ = count(transfer, "observed-transfer-bytes=")?;
        if pins.len() >= MAX_PINS {
            return Err("graph pin roster exceeds count limit".into());
        }
        pins.push(Pin {
            key: (*key).into(),
            cache: (*cache).into(),
            spec: AcquireSpec::parse(repository, exact_ref, commit, content)?,
            expected,
        });
    }
    if pins.is_empty() {
        return Err("evaluator returned no graph pins".into());
    }
    Ok(pins)
}

fn transfer(pins: &[Pin], cache: &Path, store: &Path, base: Option<&str>) -> Result<(), String> {
    if base.is_some() {
        std::fs::create_dir_all(cache)
            .map_err(|error| format!("mkdir {}: {error}", cache.display()))?;
        require_disk_backed_for(cache, "HOME for the private OSTree cache")
            .map_err(|error| error.to_string())?;
    }
    let mut failures = Vec::new();
    for pin in pins {
        let destination = cache.join(&pin.cache);
        let result = match base {
            Some(base) => ostree::acquire_from_feed(&pin.spec, &destination, base, |stats| {
                pin.validate(stats)
            })
            .map(|(stats, fetched)| (stats, if fetched { "fetched" } else { "reused" })),
            None => {
                ostree::export_feed(&pin.spec, &destination, store, |stats| pin.validate(stats))
                    .map(|stats| (stats, "exported"))
            }
        };
        match result {
            Ok((stats, action)) => eprintln!(
                ">> td-feed graphs: {} {action} and verified ({} objects)",
                pin.key, stats.objects
            ),
            Err(error) => failures.push(format!("{}: {error}", pin.key)),
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(format!("{} graph(s) failed:\n{}\nResolve local cache errors; missing host graphs need the declared recipe warm, then td-feed export graphs. These transfer commands never fall back upstream.", failures.len(), failures.join("\n")))
    }
}

pub(super) fn run(root: &Path, consume: bool) -> Result<(), String> {
    let evaluator = recipe_eval_tool(root)?;
    let mut command = Command::new(evaluator);
    command.arg("ostree-pins").current_dir(root);
    let pins = parse_pins(&vendor::command_text(
        command,
        "ostree-pins",
        MAX_PLAN_BYTES,
    )?)?;
    let home = std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .ok_or("graph transfer requires HOME")?;
    let base = if consume {
        Some(configured_consumer_feed_base()?)
    } else {
        None
    };
    transfer(
        &pins,
        &PathBuf::from(home).join(".td/ostree"),
        &feed_dir().join("store"),
        base.as_deref(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row() -> String {
        format!("firefox-source\thttps://example.invalid/repo\tapp/org.mozilla.firefox/x86_64/stable\t{}\t{}\t{}\tfirefox-1.0\tobjects=6\tpaths=2\tdirectories=1\tregular=1\tsymlinks=0\tdecoded-bytes=5\tobserved-transfer-bytes=123", "a".repeat(64), "b".repeat(64), "C".repeat(40))
    }

    #[test]
    fn graph_roster_rejects_unsafe_or_ambiguous_authority() {
        let row = row();
        assert_eq!(parse_pins(&row).unwrap().len(), 1);
        for invalid in [
            String::new(),
            format!("{row}\n{row}"),
            row.replace("firefox-1.0", "../cache"),
            row.replace("firefox-1.0", ".hidden"),
            row.replace("objects=6", "objects=+6"),
            row.replace("objects=6", "unknown=6"),
            format!("{row}\textra"),
            row.replace("https://example.invalid/repo", "http://evil.invalid/repo"),
            row.replace(&"C".repeat(40), "bad"),
        ] {
            assert!(parse_pins(&invalid).is_err());
        }
        assert!(parse_pins(&"x".repeat(MAX_PLAN_BYTES as usize + 1)).is_err());
        let oversized = (0..=MAX_PINS)
            .map(|i| {
                row.replace("firefox-source", &format!("pin{i}"))
                    .replace("firefox-1.0", &format!("cache{i}"))
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(parse_pins(&oversized).is_err());
    }

    #[test]
    fn graph_accounting_checks_semantics_without_pinning_compression_size() {
        let pin = parse_pins(&row()).unwrap().remove(0);
        let stats = GraphStats {
            objects: 6,
            paths: 2,
            directories: 1,
            regular_files: 1,
            symlinks: 0,
            path_bytes: 5,
            decoded_bytes: 5,
            transfer_bytes: 999,
        };
        assert!(pin.validate(stats).is_ok());
        assert!(pin
            .validate(GraphStats {
                decoded_bytes: 6,
                ..stats
            })
            .is_err());
    }
}
