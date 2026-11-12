//! td-engine — the std-only, dependency-free code td's own programs share.
//!
//! Both engine bins hand-roll the same two primitives (a minimal JSON
//! value/parser/canonical writer and SHA-256, kept dependency-free on purpose
//! — see the module headers). They previously carried DIVERGED private copies;
//! this crate is the single source. The JSON module keeps the recipe surface's
//! representation (numbers as raw lexemes, objects as order-preserving `Vec`
//! entries, canonical writer with SORTED keys) plus the builder's read
//! accessors; the SHA-256 module is the lint-clean implementation plus the
//! builder's streaming file helper. `exit` is the third shared thing: the
//! status codes the two bins exchange when one runs the other.
//!
//! `crc32`, `gpt` and `fat` arrived with real-hardware boot. CRC-32 stays one
//! shared module because the xz decoder's stream checksums, gzip member
//! trailers, and GPT header and entry-array checksums all need it; a private
//! copy per format is the divergence this crate exists to prevent. `gpt`
//! computes partition-table bytes and `fat` an ESP's, both performing no I/O,
//! so the target-side
//! installer `#[path]`-includes them (together with `crc32`) exactly as
//! td-boot includes `sha256`. Neither bin uses `gpt` or `fat`.
//!
//! The `gzip` module mounts td-inflate's one std-only implementation shared
//! by builder source extraction, `td-net`'s bounded OSTree acquisition
//! (on the target only through `td-app`) and the `ostree_deploy`
//! materializer; `xz` is its LZMA2
//! counterpart, shared by builder source extraction and `td-net`'s Debian
//! index resolution. td-util and PNG consumers mount td-inflate as source.
//!
//! `elf` (the read-only ELF inspection), `dynamic_application` (the
//! offline loader-graph validator) and `ostree_deploy` moved here from the
//! builder so the image build and `td-net`'s on-demand Flathub admission
//! (APPLICATIONS.md §W.9) run one implementation. The builder keeps the ELF
//! writers, and td-net, `ostree_deploy`'s only caller, passes it the rename
//! that publishes a deploy: this crate stays free of syscalls. `flathub_app`
//! holds §W.9's declarations and the pure half of its admission: the cache's
//! state formats and rules 3 and 4.
//!
//! `local_source` is back to being shared by BOTH engine bins (re #469
//! local-source-roster split): the exclusion rule and staging shape a
//! `local_source`/`local_source_trees` recipe is copied by must be exactly
//! one implementation, since td-recipe-eval interns those bytes and
//! td-builder independently re-derives their identity from the checkout —
//! a divergence here would let the two sides silently disagree about what
//! "the same tree" means. It does no hashing of its own: this crate is
//! `unsafe`-forbidding and dependency-free, and the NAR hash stays
//! td-builder's confined syscall surface (`nar.rs`).
// `cpio` writes selector appendices without parameterizing recipes: host
// harnesses provision optional volume identities, while target td-install
// binds the chosen identity in the live environment.
// See td-install/DESIGN.md §§5–7 for the shared-source contract.
pub mod application;
pub mod application_spec;
pub mod boot_info;
pub mod boot_info_source;
pub mod bzip2;
pub mod cpio;
pub mod crc32;
pub mod elf_edit;
pub mod rust_dist;
pub mod tar;
pub mod tar_write;
// `debian_root` parses recipes/src/debian-root.lock: td-recipe-eval lists
// its pins, td-builder assembles and admits the root against it.
#[path = "../../td-alloc/src/lib.rs"]
pub mod allocation;
pub mod debian_root;
pub mod dynamic_application;
pub mod elf;
#[cfg(test)]
mod elf_synth;
// `embed_scan` reads Rust source for embeds and test modules: td-recipe-eval's
// build script and td-builder's fingerprints read through one scanner.
pub mod embed_scan;
pub mod exit;
pub mod fat;
pub mod flathub_app;
pub mod gpt;
#[path = "../../td-inflate/src/gzip.rs"]
pub mod gzip;
pub mod host_path;
pub mod image_chunks;
pub mod iso9660;
pub mod json;
pub mod launcher;
pub mod local_source;
pub mod ostree;
pub mod ostree_deploy;
pub mod permissions;
pub mod principals;
pub mod sha256;
pub mod target_profile;
// `workload_accounts` is the workload account ledger, which td-firstboot
// `#[path]`-includes to seed it and td-login and the bus policy's includers
// to read it; td-builder and td-recipe-eval do not use it.
pub mod workload_accounts;
// `workload_grants` is the workload grant table and the per-account view
// manifest: td-firstboot seeds the table, td-authd publishes from it and
// td-jail admits what the manifest lists, each `#[path]`-including it.
pub mod workload_grants;
// `mount_identity` derives mount identities from mountinfo text: td-jail
// admits mounts by it and td-authd publishes grant views by it.
pub mod mount_identity;
pub mod xz;

#[cfg(test)]
#[path = "../../td-fs/src/temp_dir.rs"]
mod test_temp;

/// `workload_accounts` keeps its own copy of the principal table's
/// application name grammar: its readers (td-login and the bus policy's
/// includers) do not compile `principals`, and a leaf file both include
/// would be staged by every recipe of either set. Hold the two equal.
#[cfg(test)]
mod account_name_grammar {
    fn admitted(name: &str) -> (bool, bool) {
        let principals = crate::principals::Registry::parse(&format!(
            "td-principals-v1\nsession\t1000\t993\t992\t991\napplication\t1000\t{name}\t65536\n"
        ));
        let ledger = crate::workload_accounts::Ledger::parse(&format!(
            "td-accounts-v1\napplication\t{name}\t65536\n"
        ));
        (principals.is_ok(), ledger.is_ok())
    }

    #[test]
    fn the_ledger_and_the_principal_table_admit_the_same_names() {
        let mut names = Vec::new();
        // Every ASCII byte first, later and last, and a non-ASCII one.
        for byte in 0..=127u8 {
            let byte = char::from(byte);
            names.push(format!("{byte}"));
            names.push(format!("{byte}a"));
            names.push(format!("a{byte}"));
            names.push(format!("a{byte}a"));
        }
        names.push("m\u{e1}il".to_string());
        for length in [63, 64, 65] {
            names.push("a".repeat(length));
            names.push(format!("{}9", "a".repeat(length - 1)));
        }
        let mut admitted_any = false;
        for name in &names {
            let (principals, ledger) = admitted(name);
            assert_eq!(principals, ledger, "{name:?}");
            admitted_any |= ledger;
        }
        // The comparison is not between two refusals of everything.
        assert!(admitted_any);
        assert_eq!(admitted(&"a".repeat(64)), (true, true));
        assert_eq!(admitted(&"a".repeat(65)), (false, false));
    }
}

/// The ledger's one live reader, which every consumer shares.
#[cfg(test)]
mod account_ledger_reader {
    #![allow(clippy::unwrap_used)]

    use crate::test_temp::TempDir;
    use crate::workload_accounts::{load_from, Kind, Ledger, FILE_NAME, MAX_BYTES};
    use std::fs;
    use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};

    const LEDGER: &str = "td-accounts-v1\napplication\tfirefox\t65536\napplication\tmail\t65537\n";

    fn fixture() -> (TempDir, (u32, u32)) {
        let root = TempDir::new("td-account-ledger").unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o755)).unwrap();
        let metadata = fs::metadata(root.path()).unwrap();
        (root, (metadata.uid(), metadata.gid()))
    }

    fn write(root: &TempDir, text: &str, mode: u32) {
        let path = root.path().join(FILE_NAME);
        let _ = fs::remove_file(&path);
        fs::write(&path, text).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn the_reader_admits_only_the_published_shape() {
        let (root, owner) = fixture();
        // Absent is an error, not an empty ledger.
        assert!(load_from(root.path(), owner).is_err());
        write(&root, LEDGER, 0o644);
        let ledger = load_from(root.path(), owner).unwrap();
        assert_eq!(ledger, Ledger::parse(LEDGER).unwrap());
        assert_eq!(ledger.name(Kind::Application, 65537), Some("mail"));
        assert_eq!(ledger.name(Kind::Application, 65538), None);
        assert_eq!(ledger.name(Kind::Workspace, 65536), None);
        for other in [
            (owner.0.wrapping_add(1), owner.1),
            (owner.0, owner.1.wrapping_add(1)),
        ] {
            assert!(load_from(root.path(), other).is_err());
        }
        for mode in [0o600, 0o640, 0o664, 0o666, 0o444, 0o755, 0o4644] {
            write(&root, LEDGER, mode);
            assert!(load_from(root.path(), owner).is_err(), "{mode:o}");
        }
        write(&root, LEDGER, 0o644);
        let path = root.path().join(FILE_NAME);
        let other = root.path().join("other");
        fs::hard_link(&path, &other).unwrap();
        assert!(load_from(root.path(), owner).is_err());
        fs::remove_file(&path).unwrap();
        symlink(&other, &path).unwrap();
        assert!(load_from(root.path(), owner).is_err());
        fs::remove_file(&path).unwrap();
        fs::remove_file(&other).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(load_from(root.path(), owner).is_err());
        fs::remove_dir(&path).unwrap();
        // Oversized, and malformed, are refused.
        write(&root, &"x".repeat(MAX_BYTES + 1), 0o644);
        assert!(load_from(root.path(), owner).is_err());
        write(&root, &LEDGER.replace("65537", "065537"), 0o644);
        assert!(load_from(root.path(), owner).is_err());
        // A directory others may write, or a link to one, is refused.
        write(&root, LEDGER, 0o644);
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o775)).unwrap();
        assert!(load_from(root.path(), owner).is_err());
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(load_from(root.path(), owner).is_ok());
        let link = root.path().with_extension("link");
        symlink(root.path(), &link).unwrap();
        assert!(load_from(&link, owner).is_err());
        fs::remove_file(link).unwrap();
    }

    #[test]
    fn a_later_ledger_extends_an_earlier_one() {
        let earlier = Ledger::parse(LEDGER).unwrap();
        let later = Ledger::parse(&format!("{LEDGER}application\tnews\t65538\n")).unwrap();
        assert!(later.extends(&earlier));
        assert!(earlier.extends(&earlier));
        assert!(!earlier.extends(&later));
        let moved = Ledger::parse(&LEDGER.replace("65537", "65539")).unwrap();
        assert!(!moved.extends(&earlier));
        assert!(earlier.extends(&Ledger::default()));
    }

    /// A republished ledger has another stamp, an untouched one the same;
    /// the stamp read beside a load is the one `stamp_from` reports.
    #[test]
    fn a_republished_ledger_has_another_stamp() {
        use crate::workload_accounts::{load_stamped_from, stamp_from};
        let (root, owner) = fixture();
        assert!(stamp_from(root.path(), owner).is_err());
        write(&root, LEDGER, 0o644);
        let (_, loaded) = load_stamped_from(root.path(), owner).unwrap();
        assert_eq!(stamp_from(root.path(), owner).unwrap(), loaded);
        assert_eq!(stamp_from(root.path(), owner).unwrap(), loaded);
        // Publication renames a new file into place: same bytes, new inode.
        let staged = root.path().join(".accounts.next");
        fs::write(&staged, LEDGER).unwrap();
        fs::set_permissions(&staged, fs::Permissions::from_mode(0o644)).unwrap();
        fs::rename(&staged, root.path().join(FILE_NAME)).unwrap();
        assert_ne!(stamp_from(root.path(), owner).unwrap(), loaded);
        assert!(stamp_from(root.path(), (owner.0.wrapping_add(1), owner.1)).is_err());
    }

    /// `tda` and canonical digits is the application account form, held to
    /// the application uid range by nothing but the ledger.
    #[test]
    fn the_application_account_name_spells_a_uid() {
        use crate::workload_accounts::{
            application_account_name, application_account_uid, APPLICATION_UIDS, LEDGER_UIDS,
            WORKLOAD_UIDS,
        };
        assert_eq!(application_account_uid("tda65536"), Some(65536));
        assert_eq!(application_account_uid("tda7"), Some(7));
        for name in ["tda065536", "tda4294967296"] {
            assert!(application_account_name(name));
            assert_eq!(application_account_uid(name), None, "{name}");
        }
        for name in ["tda", "tdadam", "tda1x", "xtda1", "TDA1", "tda-1", ""] {
            assert!(!application_account_name(name), "{name}");
            assert_eq!(application_account_uid(name), None);
        }
        assert_eq!(LEDGER_UIDS.start(), APPLICATION_UIDS.start());
        assert_eq!(APPLICATION_UIDS.end() + 1, *WORKLOAD_UIDS.start());
        assert_eq!(LEDGER_UIDS.end(), WORKLOAD_UIDS.end());
    }

    /// `tdw` and canonical digits is the sandbox and workspace account
    /// form; either reserved form is a ledger account's name.
    #[test]
    fn the_workload_account_name_spells_a_uid() {
        use crate::workload_accounts::{
            application_account_name, ledger_account_name, workload_account_name,
            workload_account_uid,
        };
        assert_eq!(workload_account_uid("tdw98304"), Some(98304));
        assert_eq!(workload_account_uid("tdw7"), Some(7));
        for name in ["tdw098304", "tdw4294967296"] {
            assert!(workload_account_name(name));
            assert_eq!(workload_account_uid(name), None, "{name}");
        }
        for name in [
            "tdw", "tdwalter", "tdw1x", "xtdw1", "TDW1", "tdw-1", "tda1", "",
        ] {
            assert!(!workload_account_name(name), "{name}");
            assert_eq!(workload_account_uid(name), None);
        }
        for name in ["tda65536", "tdw98304", "tda0", "tdw0"] {
            assert!(ledger_account_name(name), "{name}");
        }
        for name in ["tdb1", "tdv1000", "tda", "tdw", "tdwalter", "tdadam", ""] {
            assert!(!ledger_account_name(name), "{name}");
        }
        assert!(!application_account_name("tdw98304"));
    }

    /// Each ledger range has its own delegation; nothing outside them has
    /// one here.
    #[test]
    fn a_ledger_accounts_delegation_follows_its_range() {
        use crate::workload_accounts::delegation;
        for uid in [65536, 66560, 98303] {
            assert_eq!(
                delegation(uid).map(|name| name.to_string()),
                Some(format!("td-app-{uid}"))
            );
        }
        for uid in [98304, 100000, 131071] {
            assert_eq!(
                delegation(uid).map(|name| name.to_string()),
                Some(format!("td-workload-{uid}"))
            );
        }
        for uid in [0, 998, 1000, 65535, 131072, u32::MAX] {
            assert_eq!(delegation(uid), None, "{uid}");
        }
    }

    /// Each of the sandbox and workspace kinds stops at
    /// `WORKLOAD_KIND_ROWS`, half of `WORKLOAD_ROWS`, so cycling one kind's
    /// names can spend neither the other kind's rows nor the ledger's: at
    /// one kind's bound the other kind and an application are still
    /// allocated, an existing row is still returned, and a ledger already
    /// past the bound still parses.
    #[test]
    fn each_workload_kind_stops_at_its_half_and_leaves_the_rest_room() {
        use crate::workload_accounts::{Kind, Ledger, WORKLOAD_KIND_ROWS, WORKLOAD_ROWS};
        use std::collections::BTreeSet;
        assert_eq!(WORKLOAD_KIND_ROWS * 2, WORKLOAD_ROWS);
        let ledger = |sandboxes: usize, workspaces: usize| {
            let mut text = String::from("td-accounts-v1\napplication\tfirefox\t65536\n");
            for index in 0..sandboxes {
                text.push_str(&format!("sandbox\ts{index:05}\t{}\n", 98304 + index));
            }
            for index in 0..workspaces {
                text.push_str(&format!(
                    "workspace\tw{index:05}\t{}\n",
                    98304 + sandboxes + index
                ));
            }
            Ledger::parse(&text).unwrap()
        };
        let none = BTreeSet::new();
        let next = |rows: usize| 98304 + u32::try_from(rows).unwrap();
        // The last row of each kind, beside the other kind's whole half.
        for (kind, sandboxes, workspaces) in [
            (Kind::Sandbox, WORKLOAD_KIND_ROWS - 1, WORKLOAD_KIND_ROWS),
            (Kind::Workspace, WORKLOAD_KIND_ROWS, WORKLOAD_KIND_ROWS - 1),
        ] {
            let (_, uid, new) = ledger(sandboxes, workspaces)
                .allocate(kind, "last", &none)
                .unwrap();
            assert_eq!((uid, new), (next(WORKLOAD_ROWS - 1), true), "{kind:?}");
        }
        // One kind at its bound: refused for good, the other kind and an
        // application still allocated, an existing row still returned.
        for (full, other, sandboxes, workspaces, kept) in [
            (
                Kind::Sandbox,
                Kind::Workspace,
                WORKLOAD_KIND_ROWS,
                0,
                "s00007",
            ),
            (
                Kind::Workspace,
                Kind::Sandbox,
                0,
                WORKLOAD_KIND_ROWS,
                "w00007",
            ),
        ] {
            let at = ledger(sandboxes, workspaces);
            assert!(at.allocate(full, "more", &none).is_err(), "{full:?}");
            let (_, uid, new) = at.allocate(other, "more", &none).unwrap();
            assert_eq!((uid, new), (next(WORKLOAD_KIND_ROWS), true), "{other:?}");
            let (_, uid, new) = at.allocate(Kind::Application, "photo", &none).unwrap();
            assert_eq!((uid, new), (66560, true));
            assert_eq!(
                at.allocate(full, kept, &none).unwrap(),
                (at.clone(), 98311, false)
            );
        }
        // Both kinds at their bounds: only applications are allocated.
        let both = ledger(WORKLOAD_KIND_ROWS, WORKLOAD_KIND_ROWS);
        for kind in [Kind::Sandbox, Kind::Workspace] {
            assert!(both.allocate(kind, "more", &none).is_err(), "{kind:?}");
        }
        assert!(both.allocate(Kind::Application, "photo", &none).is_ok());
        // Parsing never applies the bound; a kind past it takes no row,
        // and the other kind still does.
        let past = ledger(WORKLOAD_KIND_ROWS + 1, 0);
        assert!(past.allocate(Kind::Sandbox, "more", &none).is_err());
        assert!(past.allocate(Kind::Workspace, "more", &none).is_ok());
        // An earlier deployment's ledger holding the whole of
        // `WORKLOAD_ROWS` in sandboxes takes no workspace either: the two
        // kinds still keep to it together, and applications to the rest.
        let earlier = ledger(WORKLOAD_ROWS, 0);
        for kind in [Kind::Sandbox, Kind::Workspace] {
            assert!(earlier.allocate(kind, "more", &none).is_err(), "{kind:?}");
        }
        let (_, uid, new) = earlier.allocate(Kind::Application, "photo", &none).unwrap();
        assert_eq!((uid, new), (66560, true));
        let below = ledger(WORKLOAD_ROWS - 1, 0);
        assert!(below.allocate(Kind::Workspace, "more", &none).is_ok());
    }

    /// td-agent's slots are the only workspace names: `agent-1` through
    /// `agent-960` and `review-1` through `review-64`, in canonical
    /// decimal, together the workspace kind's rows.
    #[test]
    fn workspace_slots_are_a_closed_set_of_the_kinds_size() {
        use crate::workload_accounts::{
            valid_name, workspace_slot, SlotPool, AGENT_SLOTS, REVIEW_SLOTS, WORKLOAD_KIND_ROWS,
        };
        assert_eq!(
            usize::try_from(AGENT_SLOTS + REVIEW_SLOTS).unwrap(),
            WORKLOAD_KIND_ROWS
        );
        for (name, slot) in [
            ("agent-1", (SlotPool::Agent, 1)),
            ("agent-960", (SlotPool::Agent, 960)),
            ("review-1", (SlotPool::Review, 1)),
            ("review-64", (SlotPool::Review, 64)),
        ] {
            assert_eq!(workspace_slot(name), Some(slot), "{name}");
            assert!(valid_name(name), "{name}");
        }
        for name in [
            "agent-0",
            "agent-961",
            "agent-01",
            "agent-+1",
            "agent-",
            "agent-1x",
            "agent1",
            "agent-4294967296",
            "review-0",
            "review-65",
            "review-064",
            "Agent-1",
            "agents-1",
            "default",
            "",
        ] {
            assert_eq!(workspace_slot(name), None, "{name}");
        }
        let every = (1..=AGENT_SLOTS)
            .map(|n| format!("agent-{n}"))
            .chain((1..=REVIEW_SLOTS).map(|n| format!("review-{n}")))
            .filter(|name| workspace_slot(name).is_some())
            .count();
        assert_eq!(every, WORKLOAD_KIND_ROWS);
    }

    /// Only a workspace's account has a scratch tree, whose directories
    /// stand in td-jail's order of its persistent scratch targets: `/tmp`,
    /// `/var/tmp`, `/dev/shm` and `/run`.
    #[test]
    fn only_a_workspace_has_a_scratch_tree() {
        use crate::workload_accounts::{Kind, SCRATCH, SCRATCH_NAMES};
        assert!(Kind::Workspace.scratch());
        assert!(!Kind::Sandbox.scratch());
        assert!(!Kind::Application.scratch());
        assert_eq!(SCRATCH_NAMES, ["tmp", "var-tmp", "shm", "run"]);
        assert_ne!(SCRATCH, Kind::Workspace.homes());
    }

    /// A reserved name resolves only to a row of a kind its form names:
    /// `tda` an application's, `tdw` a sandbox's or a workspace's.
    #[test]
    fn a_reserved_name_resolves_only_from_its_kinds_rows() {
        use crate::workload_accounts::{Kind, Ledger};
        let ledger = Ledger::parse(concat!(
            "td-accounts-v1\n",
            "application\tfirefox\t65536\n",
            "sandbox\tdefault\t98304\n",
            "workspace\tagent\t98305\n",
        ))
        .unwrap();
        assert_eq!(ledger.account("tda65536"), Some((Kind::Application, 65536)));
        assert_eq!(ledger.account("tdw98304"), Some((Kind::Sandbox, 98304)));
        assert_eq!(ledger.account("tdw98305"), Some((Kind::Workspace, 98305)));
        for name in [
            "tdw65536",
            "tda98304",
            "tdw98306",
            "tda65537",
            "tdw098304",
            "firefox",
            "default",
            "",
        ] {
            assert_eq!(ledger.account(name), None, "{name}");
        }
        assert_eq!(Kind::Application.homes(), "applications");
        assert_eq!(Kind::Sandbox.homes(), "workloads");
        assert_eq!(Kind::Workspace.homes(), "workloads");
        assert_eq!(
            Kind::Sandbox.home(98304),
            std::path::Path::new("/var/lib/td/workloads/98304")
        );
        assert_eq!(
            Kind::Application.home(65536),
            std::path::Path::new("/var/lib/td/applications/65536")
        );
    }
}

/// Hash-visible EROFS producer format. Change when packed bytes change.
pub const EROFS_FORMAT: &str = "zstd-shared-extents-v2";

pub mod native_cache;
