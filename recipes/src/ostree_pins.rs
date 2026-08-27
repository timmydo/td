//! Exact foreign OSTree deploy pins owned by the recipe catalog.
//!
//! A mutable Flathub ref is discovery metadata only. Each entry binds that ref
//! to its reviewed commit, root content checksum, signing-key fingerprint and
//! permanent graph accounting. The control plane fetches only objects reachable
//! from that exact commit and reauthenticates the whole graph before interning.

use crate::types::{OstreeGraphStats, OstreePin};

struct PinDef {
    key: &'static str,
    repository: &'static str,
    exact_ref: &'static str,
    commit: &'static str,
    content: &'static str,
    signing_key_fingerprint: &'static str,
    cache: &'static str,
    expected: OstreeGraphStats,
}

const PINS: &[PinDef] = &[
    PinDef {
        key: "firefox-154-source",
        repository: "https://dl.flathub.org/repo",
        exact_ref: "app/org.mozilla.firefox/x86_64/stable",
        commit: "86ba63a1c2378a9525b495e1ba2c3ed9dc71ee92f67e45d8016cc4972024b410",
        content: "e511b540f42135f8703d6ea0f65abe3b798f93d4ab73ad27bf272d372a72fac3",
        signing_key_fingerprint: "6E5C05D979C76DAF93C081354184DD4D907A7CAE",
        cache: "firefox-154-86ba63a1-v2",
        expected: OstreeGraphStats {
            objects: 357,
            paths: 480,
            directories: 184,
            regular_files: 151,
            symlinks: 145,
            decoded_bytes: 333_694_837,
            transfer_bytes: 125_579_637,
        },
    },
    PinDef {
        key: "freedesktop-platform-25-08-source",
        repository: "https://dl.flathub.org/repo",
        exact_ref: "runtime/org.freedesktop.Platform/x86_64/25.08",
        commit: "bd44a6230581917d04f89812a4c21090c304d390edb73995af1c2f9fd8abf4e8",
        content: "e8c3f71b355e2248fba4e04492de33242355ddd4b552f809ea06292859200c72",
        signing_key_fingerprint: "6E5C05D979C76DAF93C081354184DD4D907A7CAE",
        cache: "freedesktop-25.08-bd44a623-v2",
        expected: OstreeGraphStats {
            objects: 14_346,
            paths: 18_196,
            directories: 1_947,
            regular_files: 13_740,
            symlinks: 2_509,
            decoded_bytes: 656_400_310,
            transfer_bytes: 241_573_468,
        },
    },
];

const FOREIGN: &[&str] = &["firefox-154-source", "freedesktop-platform-25-08-source"];

pub fn all() -> Vec<OstreePin> {
    PINS.iter().map(materialize).collect()
}

pub fn by_key(key: &str) -> Option<OstreePin> {
    PINS.iter().find(|pin| pin.key == key).map(materialize)
}

pub fn foreign_names() -> Vec<String> {
    FOREIGN.iter().map(|key| (*key).to_string()).collect()
}

fn materialize(pin: &PinDef) -> OstreePin {
    OstreePin {
        key: pin.key.into(),
        repository: pin.repository.into(),
        exact_ref: pin.exact_ref.into(),
        commit: pin.commit.into(),
        content: pin.content.into(),
        signing_key_fingerprint: pin.signing_key_fingerprint.into(),
        cache: pin.cache.into(),
        expected: pin.expected,
        foreign: FOREIGN.contains(&pin.key),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn both_reviewed_graphs_are_exact_and_foreign() {
        let pins = all();
        assert_eq!(pins.len(), 2);
        let mut keys = BTreeSet::new();
        let mut caches = BTreeSet::new();
        for pin in pins {
            assert!(pin.foreign(), "{} lost its payload mark", pin.key);
            assert_eq!(pin.commit.len(), 64);
            assert_eq!(pin.content.len(), 64);
            assert_eq!(pin.signing_key_fingerprint.len(), 40);
            assert!(pin.repository.starts_with("https://"));
            assert!(!pin.cache.contains('/'));
            assert!(pin.expected.objects > 0);
            assert!(pin.expected.paths > 0);
            assert!(keys.insert(pin.key.clone()), "duplicate key {}", pin.key);
            assert!(
                caches.insert(pin.cache.clone()),
                "duplicate cache {}",
                pin.cache
            );
        }
    }

    #[test]
    fn every_foreign_name_resolves_to_the_same_marked_pin() {
        for name in foreign_names() {
            let pin = by_key(&name).expect("foreign roster key resolves");
            assert!(pin.foreign(), "{name} resolved without its mark");
        }
    }
}
