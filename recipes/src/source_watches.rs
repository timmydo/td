//! Where upstream announces each source pin's releases, which `td-feed
//! outdated` reads to say which pins it has moved past (UPDATING.md).
//!
//! A table beside `source_pins.rs`, not fields in it: a watch moves no
//! pinned byte, so mending one should not re-key every recipe check the way
//! a pin table edit does. `all` joins the two and refuses a pin without a
//! watch or a watch without a pin.

use crate::source_pins;

pub enum Watch {
    /// A page whose text names each release as `pattern`, `{}` standing for
    /// its version: a directory listing, a release feed, a sparse crate index
    /// or a Git server's ref advertisement.
    Index {
        index: &'static str,
        pattern: &'static str,
    },
    /// An `Index` whose project ships a fresh line (Linux 7.3) before that
    /// line's first point release (7.3.1): only versions of three or more
    /// numeric parts count, so the watch names the newest stable release.
    PointReleases {
        index: &'static str,
        pattern: &'static str,
    },
    /// Deliberately not followed, for the reason given.
    Held(&'static str),
}

const GIT_LINUX_FIRMWARE: &str =
    "https://gitlab.com/kernel-firmware/linux-firmware.git/info/refs?service=git-upload-pack";
const GIT_NERD_FONTS: &str =
    "https://github.com/ryanoasis/nerd-fonts.git/info/refs?service=git-upload-pack";
const GIT_RIPGREP: &str =
    "https://github.com/BurntSushi/ripgrep.git/info/refs?service=git-upload-pack";
const GIT_ZLIB: &str = "https://github.com/madler/zlib.git/info/refs?service=git-upload-pack";
const RUST_STABLE: &str = "https://static.rust-lang.org/dist/channel-rust-stable.toml";
const LINUX_FIRMWARE: Watch = Watch::Index {
    index: GIT_LINUX_FIRMWARE,
    pattern: "refs/tags/{}",
};
const NERD_FONTS: Watch = Watch::Index {
    index: GIT_NERD_FONTS,
    pattern: "refs/tags/v{}",
};

const WATCHES: &[(&str, Watch)] = &[
    (
        "ca-certificates-source",
        Watch::Index {
            index: "https://curl.se/docs/caextract.html",
            pattern: "cacert-{}.pem",
        },
    ),
    (
        "curl-x86-64-source",
        Watch::Index {
            index: "https://curl.se/download/",
            pattern: "curl-{}.tar.xz",
        },
    ),
    (
        "fd-source",
        Watch::Index {
            index: "https://index.crates.io/fd/-f/fd-find",
            pattern: "\"vers\":\"{}\"",
        },
    ),
    (
        "git-x86-64-source",
        Watch::Index {
            index: "https://www.kernel.org/pub/software/scm/git/",
            pattern: "git-{}.tar.xz",
        },
    ),
    ("jetbrains-mono-nerd-font-source", NERD_FONTS),
    ("nerd-fonts-nerd-fonts-license-source", NERD_FONTS),
    ("nerd-fonts-codicons-license-source", NERD_FONTS),
    ("nerd-fonts-font-awesome-license-source", NERD_FONTS),
    ("nerd-fonts-materialdesign-license-source", NERD_FONTS),
    ("nerd-fonts-octicons-license-source", NERD_FONTS),
    ("nerd-fonts-pomicons-license-source", NERD_FONTS),
    ("nerd-fonts-powerline-extra-license-source", NERD_FONTS),
    ("nerd-fonts-powerline-symbols-license-source", NERD_FONTS),
    ("nerd-fonts-weather-icons-license-source", NERD_FONTS),
    (
        "libressl-x86-64-source",
        Watch::Index {
            index: "https://ftp.openbsd.org/pub/OpenBSD/LibreSSL/",
            pattern: "libressl-{}.tar.gz",
        },
    ),
    (
        "linux-firmware-amdgpu-dcn-3-1-4-dmcub-source",
        LINUX_FIRMWARE,
    ),
    ("linux-firmware-amdgpu-gc-11-0-1-imu-source", LINUX_FIRMWARE),
    (
        "linux-firmware-amdgpu-gc-11-0-1-mes1-source",
        LINUX_FIRMWARE,
    ),
    (
        "linux-firmware-amdgpu-gc-11-0-1-mes-2-source",
        LINUX_FIRMWARE,
    ),
    ("linux-firmware-amdgpu-gc-11-0-1-me-source", LINUX_FIRMWARE),
    ("linux-firmware-amdgpu-gc-11-0-1-mec-source", LINUX_FIRMWARE),
    ("linux-firmware-amdgpu-gc-11-0-1-pfp-source", LINUX_FIRMWARE),
    ("linux-firmware-amdgpu-gc-11-0-1-rlc-source", LINUX_FIRMWARE),
    ("linux-firmware-amdgpu-psp-13-0-4-ta-source", LINUX_FIRMWARE),
    (
        "linux-firmware-amdgpu-psp-13-0-4-toc-source",
        LINUX_FIRMWARE,
    ),
    ("linux-firmware-amdgpu-sdma-6-0-1-source", LINUX_FIRMWARE),
    ("linux-firmware-amdgpu-vcn-4-0-2-source", LINUX_FIRMWARE),
    ("linux-firmware-amdgpu-license-source", LINUX_FIRMWARE),
    ("linux-firmware-ath11k-wcn6855-amss-source", LINUX_FIRMWARE),
    (
        "linux-firmware-ath11k-wcn6855-board-2-source",
        LINUX_FIRMWARE,
    ),
    ("linux-firmware-ath11k-wcn6855-m3-source", LINUX_FIRMWARE),
    (
        "linux-firmware-ath11k-wcn6855-notice-source",
        LINUX_FIRMWARE,
    ),
    ("linux-firmware-ath11k-wcn6855-regdb-source", LINUX_FIRMWARE),
    (
        "linux-firmware-qualcomm-atheros-license-source",
        LINUX_FIRMWARE,
    ),
    (
        "linux-kernel-source",
        Watch::PointReleases {
            index: "https://www.kernel.org/releases.json",
            pattern: "linux-{}.tar.xz",
        },
    ),
    (
        "openssh-x86-64-source",
        Watch::Index {
            index: "https://cdn.openbsd.org/pub/OpenBSD/OpenSSH/portable/",
            pattern: "openssh-{}.tar.gz",
        },
    ),
    (
        "ripgrep-seed-source",
        Watch::Index {
            index: GIT_RIPGREP,
            pattern: "refs/tags/{}",
        },
    ),
    (
        "ripgrep-source",
        Watch::Index {
            index: "https://index.crates.io/ri/pg/ripgrep",
            pattern: "\"vers\":\"{}\"",
        },
    ),
    (
        "rust-rustc-dist",
        Watch::Index {
            index: RUST_STABLE,
            pattern: "rustc-{}-x86_64-unknown-linux-gnu.tar.xz",
        },
    ),
    (
        "rust-std-dist",
        Watch::Index {
            index: RUST_STABLE,
            pattern: "rust-std-{}-x86_64-unknown-linux-gnu.tar.xz",
        },
    ),
    (
        "rust-cargo-dist",
        Watch::Index {
            index: RUST_STABLE,
            pattern: "cargo-{}-x86_64-unknown-linux-gnu.tar.xz",
        },
    ),
    (
        "rust-clippy-dist",
        Watch::Index {
            index: RUST_STABLE,
            pattern: "clippy-{}-x86_64-unknown-linux-gnu.tar.xz",
        },
    ),
    (
        "tzdata-source",
        Watch::Index {
            index: "https://data.iana.org/time-zones/releases/",
            pattern: "tzdata{}.tar.gz",
        },
    ),
    (
        "uutils-source",
        Watch::Index {
            index: "https://index.crates.io/co/re/coreutils",
            pattern: "\"vers\":\"{}\"",
        },
    ),
    (
        "zlib-x86-64-source",
        Watch::Index {
            index: GIT_ZLIB,
            pattern: "refs/tags/v{}",
        },
    ),
];

/// One pin's release and watch, as `td-recipe-eval source-watches` prints
/// them.
pub struct SourceWatch {
    pub key: &'static str,
    pub version: &'static str,
    pub watch: &'static Watch,
}

/// Every pin with its watch, in pin-table order.
pub fn all() -> Result<Vec<SourceWatch>, String> {
    join(&source_pins::versions(), WATCHES)
}

fn join(
    pins: &[(&'static str, &'static str)],
    watches: &'static [(&'static str, Watch)],
) -> Result<Vec<SourceWatch>, String> {
    let mut out = Vec::new();
    for (key, version) in pins {
        let mut found = watches.iter().filter(|(name, _)| name == key);
        let (Some((_, watch)), None) = (found.next(), found.next()) else {
            return Err(format!(
                "source pin {key} needs exactly one watch in source_watches.rs"
            ));
        };
        out.push(SourceWatch {
            key,
            version,
            watch,
        });
    }
    if let Some((name, _)) = watches
        .iter()
        .find(|(name, _)| !pins.iter().any(|(key, _)| key == name))
    {
        return Err(format!("watch {name} names no source pin"));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `version` must be the release the pin fetches: a whole version, not
    /// the head of a longer one (1.3.1 inside 1.3.10).
    fn names_version(text: &str, version: &str) -> bool {
        text.match_indices(version).any(|(at, _)| {
            let before = text.get(..at).and_then(|t| t.chars().last());
            let after = text
                .get(at + version.len()..)
                .and_then(|t| t.chars().next());
            !before.is_some_and(|c| c.is_ascii_digit())
                && !after.is_some_and(|c| c.is_ascii_digit())
        })
    }

    #[test]
    fn every_pin_has_one_watch_and_every_watch_a_pin() {
        let joined = all().unwrap();
        assert_eq!(joined.len(), source_pins::versions().len());
        static EXTRA: &[(&str, Watch)] = &[("a", Watch::Held("x")), ("b", Watch::Held("y"))];
        assert!(join(&[("a", "1")], EXTRA).is_err(), "a watch names no pin");
        assert!(join(&[("a", "1"), ("b", "1"), ("c", "1")], EXTRA).is_err());
        static TWICE: &[(&str, Watch)] = &[("a", Watch::Held("x")), ("a", Watch::Held("y"))];
        assert!(
            join(&[("a", "1")], TWICE).is_err(),
            "two watches for one pin"
        );
    }

    /// `td-feed outdated` compares `version` with what the watch finds, so a
    /// bump that moves the url but not the version would read as behind
    /// forever, and one that moves the version alone would hide the bytes.
    #[test]
    fn every_watch_is_well_formed_and_matches_its_pin() {
        let pins = source_pins::all();
        for entry in all().unwrap() {
            let pin = pins.iter().find(|pin| pin.key == entry.key).unwrap();
            assert!(
                names_version(&pin.url, entry.version) || names_version(&pin.file, entry.version),
                "{}: version {} names neither its url nor its file",
                entry.key,
                entry.version
            );
            let (index, pattern) = match entry.watch {
                Watch::Index { index, pattern } | Watch::PointReleases { index, pattern } => {
                    (index, pattern)
                }
                Watch::Held(reason) => {
                    assert!(!reason.is_empty(), "{}: a hold says why", entry.key);
                    assert!(!reason.contains(['\t', '\n']), "{}", entry.key);
                    continue;
                }
            };
            assert!(
                index.starts_with("https://") || index.starts_with("http://"),
                "{}: watch index {index} is not an http(s) URL",
                entry.key
            );
            // `source-watches` prints tab-separated rows, one per line.
            assert!(
                ![index, pattern].iter().any(|f| f.contains(['\t', '\n'])),
                "{}: a watch field holds a tab or newline",
                entry.key
            );
            assert_eq!(pattern.matches("{}").count(), 1, "{}: {pattern}", entry.key);
            assert!(!pattern.starts_with("{}"), "{}: {pattern}", entry.key);
            // A pattern naming a file must name this pin's file or url.
            if !pattern.starts_with("refs/tags/") && !pattern.starts_with('"') {
                let release = pattern.replace("{}", entry.version);
                assert!(
                    pin.url.contains(&release) || pin.file.contains(&release),
                    "{}: {release} is neither its url nor its file",
                    entry.key
                );
            }
        }
        assert!(names_version("zlib-1.3.1.tar.gz", "1.3.1"));
        assert!(!names_version("zlib-1.3.10.tar.gz", "1.3.1"));
        assert!(!names_version("zlib-11.3.1.tar.gz", "1.3.1"));
    }
}
