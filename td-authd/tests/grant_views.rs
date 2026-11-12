#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use super::*;
use crate::test_temp::TempDir;
use std::collections::BTreeMap;
use std::os::unix::fs::symlink;

/// The table's rows of the `kind` account `name`, as `publish` takes them.
fn table_grants(
    table: &Table,
    kind: Kind,
    name: &str,
    mountinfo: &str,
    home: &Path,
) -> Result<Vec<Grant>, String> {
    grants_for(table.directories(kind, name), kind, mountinfo, home)
}

fn owner(path: &Path) -> (u32, u32) {
    let metadata = fs::metadata(path).unwrap();
    (metadata.uid(), metadata.gid())
}

fn private(path: &Path, mode: u32) {
    fs::create_dir_all(path).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

/// A grant's source is the human's own directory, reached name by name
/// without a link, writable by no one else and on the home's own mount.
#[test]
fn a_granted_source_is_the_humans_own_directory_on_the_homes_mount() {
    let temp = TempDir::new("td-grant-source").unwrap();
    let home_path = temp.path().join("home");
    private(&home_path, 0o700);
    let identity = owner(&home_path);
    let home = File::open(&home_path).unwrap();
    for (path, mode) in [("Downloads", 0o700), ("Documents/taxes", 0o755)] {
        private(&home_path.join(path), mode);
        let source = human_directory(&home, path, identity).unwrap().unwrap();
        let shown = fs::metadata(home_path.join(path)).unwrap();
        assert_eq!((source.device, source.inode), (shown.dev(), shown.ino()));
    }
    // Absent, at the leaf or above it: no view, no refusal.
    assert!(human_directory(&home, "Music", identity).unwrap().is_none());
    assert!(human_directory(&home, "Music/new", identity)
        .unwrap()
        .is_none());
    // Group or other write, at the leaf or above it.
    for mode in [0o770, 0o707, 0o775] {
        fs::set_permissions(
            home_path.join("Documents"),
            fs::Permissions::from_mode(mode),
        )
        .unwrap();
        assert!(
            human_directory(&home, "Documents/taxes", identity).is_err(),
            "{mode:o}"
        );
        fs::set_permissions(
            home_path.join("Downloads"),
            fs::Permissions::from_mode(mode),
        )
        .unwrap();
        assert!(human_directory(&home, "Downloads", identity).is_err());
        fs::set_permissions(
            home_path.join("Downloads"),
            fs::Permissions::from_mode(0o700),
        )
        .unwrap();
    }
    fs::set_permissions(
        home_path.join("Documents"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    // Another owner.
    for foreign in [
        (identity.0.wrapping_add(1), identity.1),
        (identity.0, identity.1.wrapping_add(1)),
    ] {
        assert!(human_directory(&home, "Downloads", foreign).is_err());
    }
    // A link at the leaf or at an intermediate name, and a file.
    symlink("Downloads", home_path.join("Linked")).unwrap();
    assert!(human_directory(&home, "Linked", identity).is_err());
    symlink("Documents", home_path.join("Via")).unwrap();
    assert!(human_directory(&home, "Via/taxes", identity).is_err());
    fs::write(home_path.join("File"), b"").unwrap();
    assert!(human_directory(&home, "File", identity).is_err());
    // What the table's validator refuses, refused here too.
    private(&home_path.join(".ssh"), 0o700);
    for path in [".ssh", "../home", "Downloads/", "", ".config/td-mail"] {
        assert!(human_directory(&home, path, identity).is_err(), "{path:?}");
    }
    temp.cleanup().unwrap();
}

/// A directory reached across a mount stacked over the home is not the
/// human's, however it is owned: `/proc` below `/` stands in for one.
#[test]
fn a_source_across_a_stacked_mount_is_refused() {
    let root = File::open("/").unwrap();
    let error = human_directory(&root, "proc", owner(Path::new("/proc")))
        .err()
        .unwrap();
    assert!(error.contains("stacked"), "{error}");
}

/// Read-only grants take the read-only view, read-write grants the
/// writable one; both nosuid, nodev and noexec under their idmap.
#[test]
fn a_grants_access_selects_its_fixed_attributes() {
    assert_eq!(view_access(Access::ReadOnly, false), ViewAccess::ReadOnly);
    assert_eq!(view_access(Access::ReadWrite, false), ViewAccess::Writable);
    assert_eq!(ViewAccess::ReadOnly.attributes(), 0x10000f);
    assert_eq!(ViewAccess::Writable.attributes(), 0x10000e);
    for access in [Access::ReadOnly, Access::ReadWrite] {
        let options = format!("{},nosuid,nodev,noexec,relatime,idmapped", access.as_str());
        assert!(view_options(&options, access, false));
        for missing in [access.as_str(), "nosuid", "nodev", "noexec", "idmapped"] {
            let without = options
                .split(',')
                .filter(|option| *option != missing)
                .collect::<Vec<_>>()
                .join(",");
            assert!(!view_options(&without, access, false), "{missing}");
        }
    }
    assert!(!view_options(
        "rw,nosuid,nodev,noexec,idmapped",
        Access::ReadOnly,
        false
    ));
    assert!(!view_options(
        "ro,nosuid,nodev,noexec,idmapped",
        Access::ReadWrite,
        false
    ));
}

/// The account's kind alone chooses executable views: a sandbox's and a
/// workspace's are the same words without NOEXEC (8), an application's
/// never lack it, whatever the row.
#[test]
fn the_accounts_kind_alone_makes_views_executable() {
    assert!(!executable_views(Kind::Application));
    assert!(executable_views(Kind::Sandbox));
    assert!(executable_views(Kind::Workspace));
    assert_eq!(
        view_access(Access::ReadOnly, true),
        ViewAccess::ReadOnlyExecutable
    );
    assert_eq!(
        view_access(Access::ReadWrite, true),
        ViewAccess::WritableExecutable
    );
    assert_eq!(ViewAccess::ReadOnlyExecutable.attributes(), 0x100007);
    assert_eq!(ViewAccess::WritableExecutable.attributes(), 0x100006);
    for access in [Access::ReadOnly, Access::ReadWrite] {
        let noexec = view_access(access, false).attributes();
        let exec = view_access(access, true).attributes();
        assert_eq!(noexec & 8, 8);
        assert_eq!(exec, noexec & !8);
        assert_eq!(
            view_access(access, executable_views(Kind::Application)).attributes() & 8,
            8
        );
        // An executable view lacks noexec, which its source's mount never
        // carries, and needs the rest; a noexec view is never executable.
        let options = format!("{},nosuid,nodev,relatime,idmapped", access.as_str());
        assert!(view_options(&options, access, true));
        assert!(!view_options(&format!("{options},noexec"), access, true));
        assert!(!view_options(&options, access, false));
        assert!(view_options(&format!("{options},noexec"), access, false));
        for missing in [access.as_str(), "nosuid", "nodev", "idmapped"] {
            let without = options
                .split(',')
                .filter(|option| *option != missing)
                .collect::<Vec<_>>()
                .join(",");
            assert!(!view_options(&without, access, true), "{missing}");
        }
    }
    // A sandbox's rows are its own and published executable; another
    // kind's rows of the same name are not its.
    let table = Table::parse(
        "td-grants-v1\napplication\tdefault\tdirectory\tro\tMusic\nsandbox\tdefault\tdirectory\trw\tsandbox\n",
    )
    .unwrap();
    let sandbox = table_grants(&table, Kind::Sandbox, "default", STOCK, Path::new(HOME)).unwrap();
    assert_eq!(sandbox.len(), 1);
    assert_eq!(sandbox[0].path, "sandbox");
    assert!(sandbox[0].executable);
    let application =
        table_grants(&table, Kind::Application, "default", STOCK, Path::new(HOME)).unwrap();
    assert_eq!(application.len(), 1);
    assert_eq!(application[0].path, "Music");
    assert!(!application[0].executable);
}

/// A sandbox's view of a source on a noexec mount could not execute,
/// which td-jail refuses of an executable grant, so that row gets no view
/// and a warning while the others publish; an application's view of it,
/// noexec anyway, is published.
#[test]
fn an_executable_view_skips_a_source_on_a_noexec_mount() {
    let mount = |id: u64, mountpoint: &str, options: &str| MountRow {
        id,
        parent: 1,
        device: "0:19".into(),
        root: "/".into(),
        mountpoint: mountpoint.into(),
        options: options.into(),
    };
    let mounts = [
        mount(25, "/var", "rw,nosuid,nodev,relatime"),
        mount(
            26,
            "/var/home/alice/media",
            "rw,nosuid,nodev,noexec,relatime",
        ),
    ];
    executable_source(&mounts, 25).unwrap();
    assert!(executable_source(&mounts, 26)
        .unwrap_err()
        .contains("noexec"));
    assert!(executable_source(&mounts, 27).is_err());

    let executable = |access, path: &str| Grant {
        executable: true,
        ..grant(access, path)
    };
    let grants = [
        executable(Access::ReadWrite, "src"),
        executable(Access::ReadOnly, "media"),
    ];
    let mut host = Fake::new(&[("src", 11), ("media", 12)]);
    host.noexec.push("media".into());
    run(&mut host, &grants).unwrap();
    assert!(host.calls.contains(&"publish 0 src".to_string()));
    assert!(!host.calls.iter().any(|call| call.starts_with("publish 1")));
    assert!(host
        .calls
        .iter()
        .any(|call| call.starts_with("warn grant of \"media\" gets no view")));
    assert_eq!(host.calls.last().map(String::as_str), Some("manifest [0]"));
    // An application's view of the same source is noexec anyway.
    let mut host = Fake::new(&[("media", 12)]);
    host.noexec.push("media".into());
    run(&mut host, &[grant(Access::ReadOnly, "media")]).unwrap();
    assert!(host.calls.contains(&"publish 0 media".to_string()));
}

/// A launch publishes views only for a uid of its account's kind: a
/// sandbox's never at an application's uid, nor the reverse, nor the
/// human's, before anything is read.
#[test]
fn views_are_published_only_for_the_kinds_own_uids() {
    for (kind, uid) in [
        (Kind::Sandbox, 65536),
        (Kind::Sandbox, 1000),
        (Kind::Workspace, 98303),
        (Kind::Application, 98304),
        (Kind::Application, 1000),
    ] {
        let error = publish(kind, "default", uid).unwrap_err();
        assert!(
            error.contains("of the launch's kind"),
            "{kind:?} {uid}: {error}"
        );
    }
}

/// td-authd and td-jail derive what a view must show by one function, so
/// on the stock layout, where the home is a plain directory below the
/// `/var` subvolume's mount, both name the subvolume's root joined with the
/// home's offset and the row's path. The literal is td-jail's own test's.
#[test]
fn the_derivation_keeps_the_homes_offset_below_its_mount() {
    // `.ssh/keys` stands for a row a later validator refuses: held, never
    // published, and no reason to leave the account's other rows out.
    let table = Table::parse(
        "td-grants-v1\napplication\tfirefox\tdirectory\tro\t.ssh/keys\napplication\tfirefox\tdirectory\tro\tMusic/new\napplication\tfirefox\tdirectory\trw\tDownloads\napplication\tmail\tdirectory\trw\tMail\n",
    )
    .unwrap();
    assert_eq!(table.refused(Kind::Application, "firefox").len(), 1);
    let grants =
        table_grants(&table, Kind::Application, "firefox", STOCK, Path::new(HOME)).unwrap();
    assert_eq!(
        grants
            .iter()
            .map(|grant| (grant.path.as_str(), grant.access, grant.identity.clone()))
            .collect::<Vec<_>>(),
        [
            (
                "Music/new",
                Access::ReadOnly,
                identity("0:19", "/@var/home/alice/Music/new")
            ),
            (
                "Downloads",
                Access::ReadWrite,
                identity("0:19", "/@var/home/alice/Downloads")
            ),
        ]
    );
    // A home that is its own mount contributes no offset.
    let own = format!("{STOCK}30 25 0:31 /@home/alice {HOME} rw - btrfs /dev/vdb rw\n");
    let grants = table_grants(&table, Kind::Application, "firefox", &own, Path::new(HOME)).unwrap();
    assert_eq!(
        grants[1].identity,
        identity("0:31", "/@home/alice/Downloads")
    );
    // A view the kernel shows for the stock layout is kept as is.
    let stock = table_grants(&table, Kind::Application, "firefox", STOCK, Path::new(HOME)).unwrap();
    let downloads = &stock[1..];
    let mut host = Fake::new(&[("Downloads", 11)]);
    host.mount(0, &grant(Access::ReadWrite, "Downloads"), RW, 11);
    run(&mut host, downloads).unwrap();
    assert_eq!(
        host.calls,
        [
            "present",
            "manifest [0]",
            "source Downloads",
            "manifest [0]"
        ]
    );
}

const UID: u32 = 65536;
const HOME: &str = "/var/home/alice";
const STOCK: &str = "22 1 0:19 /@ / rw,relatime shared:1 - btrfs /dev/vda2 rw\n\
                     25 22 0:19 /@var /var rw,relatime shared:2 - btrfs /dev/vda2 rw\n";
const RW: &str = "rw,nosuid,nodev,noexec,relatime,idmapped";
const RO: &str = "ro,nosuid,nodev,noexec,relatime,idmapped";

fn identity(device: &str, root: &str) -> MountIdentity {
    MountIdentity {
        device: device.into(),
        root: root.into(),
    }
}

fn grant(access: Access, path: &str) -> Grant {
    Grant {
        access,
        path: path.into(),
        identity: identity("0:19", &format!("/@var/home/alice/{path}")),
        kind: Kind::Application,
        executable: false,
    }
}

/// A recording stand-in for the system: sources by path (an inode, or a
/// refusal), the mounts at each view index, and every call in order. Each
/// manifest it is given must list only views mounted and showing their
/// grant at that moment, so no manifest a launch writes is ever untrue.
struct Fake {
    sources: Vec<(String, Result<u64, String>)>,
    /// Sources on a noexec mount.
    noexec: Vec<String>,
    views: BTreeMap<usize, Present>,
    grants: Vec<Grant>,
    calls: Vec<String>,
}

impl Fake {
    fn new(sources: &[(&str, u64)]) -> Self {
        Self {
            sources: sources
                .iter()
                .map(|(path, inode)| ((*path).to_string(), Ok(*inode)))
                .collect(),
            noexec: Vec::new(),
            views: BTreeMap::new(),
            grants: Vec::new(),
            calls: Vec::new(),
        }
    }

    fn refuse(mut self, path: &str) -> Self {
        self.sources
            .push((path.to_string(), Err("group-writable".to_string())));
        self
    }

    fn mount(&mut self, index: usize, grant: &Grant, options: &str, inode: u64) {
        self.views.insert(
            index,
            Present {
                index,
                mounts: vec![MountRow {
                    id: 100 + index as u64,
                    parent: 25,
                    device: grant.identity.device.clone(),
                    root: grant.identity.root.clone(),
                    mountpoint: format!("{}/{UID}/{index}", grants::VIEW_ROOT).into(),
                    options: options.into(),
                }],
                shown: Some((7, inode, UID, UID)),
            },
        );
    }

    fn released_before_any_source(&self, index: usize) -> bool {
        let released = self
            .calls
            .iter()
            .position(|call| *call == format!("release {index}"));
        let sourced = self
            .calls
            .iter()
            .position(|call| call.starts_with("source"));
        matches!((released, sourced), (Some(r), Some(s)) if r < s)
            || matches!((released, sourced), (Some(_), None))
    }
}

impl Host for Fake {
    type Source = (String, u64);

    fn present(&mut self) -> Result<Vec<Present>, String> {
        self.calls.push("present".into());
        Ok(self.views.values().cloned().collect())
    }

    fn source(&mut self, grant: &Grant) -> Result<Found<(String, u64)>, String> {
        let path = grant.path.as_str();
        self.calls.push(format!("source {path}"));
        grants::validate_view(grant.kind, grant.access, path.as_bytes())?;
        if grant.executable && self.noexec.iter().any(|name| name == path) {
            return Err("its mount is noexec".into());
        }
        match self.sources.iter().find(|(name, _)| name == path) {
            None => Ok(None),
            Some((_, Ok(inode))) => Ok(Some(((path.to_string(), *inode), (7, *inode)))),
            Some((_, Err(error))) => Err(error.clone()),
        }
    }

    fn publish(
        &mut self,
        index: usize,
        source: &(String, u64),
        granted: &Grant,
    ) -> Result<(), String> {
        self.calls.push(format!("publish {index} {}", source.0));
        assert!(!self.views.contains_key(&index), "published over a view");
        let options = if granted.access == Access::ReadWrite {
            RW
        } else {
            RO
        };
        // An executable view's word lacks noexec.
        let options = if granted.executable {
            options.replace(",noexec", "")
        } else {
            options.to_string()
        };
        let grant = grant(granted.access, &source.0);
        self.mount(index, &grant, &options, source.1);
        Ok(())
    }

    fn release(&mut self, index: usize) -> Result<(), String> {
        self.calls.push(format!("release {index}"));
        self.views.remove(&index);
        Ok(())
    }

    fn write_manifest(&mut self, manifest: &Manifest) -> Result<(), String> {
        let indices: Vec<String> = manifest
            .views()
            .iter()
            .map(|view| view.index.to_string())
            .collect();
        self.calls.push(format!("manifest [{}]", indices.join(",")));
        for view in manifest.views() {
            let mounted = self
                .views
                .get(&view.index)
                .expect("a listed view is mounted");
            let grant = self
                .grants
                .iter()
                .find(|grant| grant.path == view.path)
                .expect("a listed view is of a grant");
            assert!(
                shows(mounted, grant),
                "listed view {} shows its grant",
                view.index
            );
        }
        Ok(())
    }

    fn warn(&mut self, message: &str) {
        self.calls.push(format!("warn {message}"));
    }
}

fn run(host: &mut Fake, grants: &[Grant]) -> Result<(), String> {
    host.grants = grants.to_vec();
    let kind = grants.first().map_or(Kind::Application, |grant| grant.kind);
    reconcile(host, kind, UID, grants).map(drop)
}

/// A first launch publishes every present source from index 0 and lists
/// them; an absent one gets no view.
#[test]
fn a_first_launch_publishes_each_present_source() {
    let grants = [
        grant(Access::ReadWrite, "Downloads"),
        grant(Access::ReadOnly, "Music"),
    ];
    let mut host = Fake::new(&[("Downloads", 11)]);
    run(&mut host, &grants).unwrap();
    assert_eq!(
        host.calls,
        [
            "present",
            "manifest []",
            "source Downloads",
            "publish 0 Downloads",
            "source Music",
            "present",
            "manifest [0]",
        ]
    );
    // Unchanged: the next launch keeps it and publishes nothing.
    host.calls.clear();
    run(&mut host, &grants).unwrap();
    assert_eq!(
        host.calls,
        [
            "present",
            "manifest [0]",
            "source Downloads",
            "source Music",
            "manifest [0]"
        ]
    );
}

/// A revoked row's view, and any view no row accounts for, is dropped from
/// the manifest and released before a single source is opened, so a row
/// whose source is refused cannot keep it mounted.
#[test]
fn a_revoked_view_is_released_before_any_source_is_opened() {
    let downloads = grant(Access::ReadWrite, "Downloads");
    let documents = grant(Access::ReadOnly, "Documents");
    let mut host = Fake::new(&[("Downloads", 11)]).refuse("Documents");
    host.mount(0, &downloads, RW, 11);
    host.mount(3, &documents, RO, 12);
    // Documents is still granted, but Downloads was revoked.
    run(&mut host, std::slice::from_ref(&documents)).unwrap();
    assert!(host.released_before_any_source(0), "{:?}", host.calls);
    assert_eq!(host.calls.first().map(String::as_str), Some("present"));
    assert_eq!(host.calls.get(1).map(String::as_str), Some("manifest [3]"));
    // Documents' source is refused: it gets no view, its old one goes, and
    // the launch goes on.
    assert!(host
        .calls
        .iter()
        .any(|call| call.starts_with("warn grant of \"Documents\"")));
    assert_eq!(host.calls.last().map(String::as_str), Some("release 3"));
    assert!(host.views.is_empty());
}

/// One row whose source is refused costs that row its view, not the
/// launch: the others are published and listed.
#[test]
fn a_refused_source_skips_its_row_and_the_others_publish() {
    let grants = [
        grant(Access::ReadWrite, "Downloads"),
        grant(Access::ReadOnly, "Music"),
        grant(Access::ReadOnly, "Pictures"),
    ];
    let mut host = Fake::new(&[("Downloads", 11), ("Pictures", 13)]).refuse("Music");
    run(&mut host, &grants).unwrap();
    assert_eq!(
        host.calls.last().map(String::as_str),
        Some("manifest [0,1]")
    );
    assert_eq!(
        host.calls
            .iter()
            .filter(|call| call.starts_with("warn"))
            .count(),
        1
    );
    assert_eq!(host.views.len(), 2);
}

/// A view that shows something other than its row as granted is replaced:
/// by identity, access, restrictions or stack in the first phase, by the
/// inode or owner a stat through it shows in the second, with the new
/// view published at a free index and listed before the old one goes.
#[test]
fn a_stale_view_is_released_and_its_row_published_again() {
    let downloads = grant(Access::ReadWrite, "Downloads");
    let stale_identity = [
        grant(Access::ReadWrite, "Old"),
        Grant {
            identity: identity("0:19", "/@var/home/alice/Downloads//deleted"),
            ..downloads.clone()
        },
        Grant {
            identity: identity("0:20", "/@var/home/alice/Downloads"),
            ..downloads.clone()
        },
    ];
    for stale in stale_identity {
        let mut host = Fake::new(&[("Downloads", 11)]);
        host.mount(0, &stale, RW, 11);
        run(&mut host, std::slice::from_ref(&downloads)).unwrap();
        assert!(host.released_before_any_source(0), "{:?}", host.calls);
        assert!(host.calls.contains(&"publish 0 Downloads".to_string()));
        assert_eq!(host.calls.last().map(String::as_str), Some("manifest [0]"));
    }
    for options in [
        RO,
        "rw,nosuid,nodev,idmapped",
        "rw,nosuid,nodev,noexec",
        "rw,nodev,noexec,idmapped",
    ] {
        let mut host = Fake::new(&[("Downloads", 11)]);
        host.mount(0, &downloads, options, 11);
        run(&mut host, std::slice::from_ref(&downloads)).unwrap();
        assert!(host.released_before_any_source(0), "{options}");
    }
    let mut host = Fake::new(&[("Downloads", 11)]);
    host.mount(0, &downloads, RW, 11);
    let doubled = host.views[&0].mounts[0].clone();
    host.views.get_mut(&0).unwrap().mounts.push(doubled);
    run(&mut host, std::slice::from_ref(&downloads)).unwrap();
    assert!(host.released_before_any_source(0));
    // Replaced or renamed in place, or shown to another owner: the new view
    // goes up beside it, the manifest moves, then the old view goes.
    for shown in [(7, 99, UID, UID), (7, 11, 1000, 1000)] {
        let mut host = Fake::new(&[("Downloads", 11)]);
        host.mount(0, &downloads, RW, 11);
        host.views.get_mut(&0).unwrap().shown = Some(shown);
        run(&mut host, std::slice::from_ref(&downloads)).unwrap();
        assert_eq!(
            host.calls,
            [
                "present",
                "manifest [0]",
                "source Downloads",
                "publish 1 Downloads",
                "present",
                "manifest [1]",
                "release 0",
            ]
        );
    }
    // An access that changed either way.
    let documents = grant(Access::ReadOnly, "Documents");
    let mut host = Fake::new(&[("Documents", 12)]);
    host.mount(4, &documents, RW, 12);
    run(&mut host, std::slice::from_ref(&documents)).unwrap();
    assert!(host.released_before_any_source(4));
    assert_eq!(host.calls.last().map(String::as_str), Some("manifest [0]"));
    // An empty mountpoint a failed launch left is removed.
    let mut host = Fake::new(&[("Downloads", 11)]);
    host.views.insert(
        0,
        Present {
            index: 0,
            mounts: Vec::new(),
            shown: None,
        },
    );
    run(&mut host, std::slice::from_ref(&downloads)).unwrap();
    assert!(host.released_before_any_source(0));
    // Two views of one grant: one kept, the other released.
    let mut host = Fake::new(&[("Downloads", 11)]);
    host.mount(0, &downloads, RW, 11);
    host.mount(1, &downloads, RW, 11);
    run(&mut host, std::slice::from_ref(&downloads)).unwrap();
    assert!(host.released_before_any_source(1));
    assert_eq!(host.calls.last().map(String::as_str), Some("manifest [0]"));
}

/// A workspace's views come from its launch's plan, never the table:
/// td-agent's own trees below reserved ones publish, executable, at their
/// role's access and are listed; at another access, or as any other
/// kind's rows, they get no view.
#[test]
fn a_workspaces_plan_publishes_td_agents_own_trees_only_for_it() {
    let repository = ".local/share/td-agent/ws/w-01234567/td.git";
    let objects = ".local/share/td-agent/store/local-x-0123456789abcdef.git/objects";
    let workspace = |access, path: &str| Grant {
        kind: Kind::Workspace,
        executable: true,
        ..grant(access, path)
    };
    let plan = [
        workspace(Access::ReadWrite, repository),
        workspace(Access::ReadOnly, objects),
        workspace(Access::ReadWrite, "src"),
    ];
    let mut host = Fake::new(&[(repository, 11), (objects, 12), ("src", 13)]);
    run(&mut host, &plan).unwrap();
    assert_eq!(
        host.calls.last().map(String::as_str),
        Some("manifest [0,1,2]")
    );
    assert!(!host.calls.iter().any(|call| call.starts_with("warn")));
    for refused in [
        workspace(Access::ReadWrite, objects),
        workspace(Access::ReadOnly, repository),
        grant(Access::ReadWrite, repository),
        Grant {
            kind: Kind::Sandbox,
            executable: true,
            ..grant(Access::ReadWrite, repository)
        },
    ] {
        let mut host = Fake::new(&[(repository, 11), (objects, 12)]);
        run(&mut host, std::slice::from_ref(&refused)).unwrap();
        assert_eq!(
            host.calls.last().map(String::as_str),
            Some("manifest []"),
            "{refused:?}"
        );
        assert!(host.calls.iter().any(|call| call.starts_with("warn")));
    }
    // The table's publication takes no workspace: its views are its plan's.
    let refused = publish(Kind::Workspace, "agent-1", 98304).unwrap_err();
    assert!(refused.contains("plan"), "{refused}");
}

/// A published view that does not show its source refuses the launch, and
/// the manifest stays the one written before it.
#[test]
fn a_published_view_that_shows_something_else_refuses() {
    struct Wrong(Fake);
    impl Host for Wrong {
        type Source = (String, u64);
        fn present(&mut self) -> Result<Vec<Present>, String> {
            self.0.present()
        }
        fn source(&mut self, grant: &Grant) -> Result<Found<Self::Source>, String> {
            self.0.source(grant)
        }
        fn publish(
            &mut self,
            index: usize,
            source: &Self::Source,
            grant: &Grant,
        ) -> Result<(), String> {
            self.0
                .publish(index, &(source.0.clone(), source.1 + 1), grant)
        }
        fn release(&mut self, index: usize) -> Result<(), String> {
            self.0.release(index)
        }
        fn write_manifest(&mut self, manifest: &Manifest) -> Result<(), String> {
            self.0.write_manifest(manifest)
        }
        fn warn(&mut self, message: &str) {
            self.0.warn(message);
        }
    }
    let downloads = grant(Access::ReadWrite, "Downloads");
    let mut host = Wrong(Fake::new(&[("Downloads", 11)]));
    host.0.grants = vec![downloads.clone()];
    assert!(reconcile(
        &mut host,
        Kind::Application,
        UID,
        std::slice::from_ref(&downloads)
    )
    .is_err());
    assert_eq!(host.0.calls.last().map(String::as_str), Some("present"));
    assert!(
        host.0
            .calls
            .iter()
            .filter(|call| call.starts_with("manifest"))
            .count()
            == 1
    );
}

/// The launch lock is a private file in root's private directory, so no
/// account can open it, and a held lock is waited for only to a deadline.
#[test]
fn the_launch_lock_is_root_private_and_bounded() {
    let temp = TempDir::new("td-grant-lock").unwrap();
    let tree = File::open(temp.path()).unwrap();
    let identity = owner(temp.path());
    let locks = private_directory(&tree, LOCKS, identity).unwrap();
    let metadata = fs::symlink_metadata(temp.path().join(LOCKS)).unwrap();
    assert_eq!(metadata.mode() & 0o7777, 0o700);
    let held = lock(&locks, "65536", identity, Duration::from_millis(10)).unwrap();
    let file = fs::symlink_metadata(temp.path().join(LOCKS).join("65536")).unwrap();
    assert_eq!(file.mode() & 0o7777, 0o600);
    // Waited for on another thread, so a lock that never gives up fails
    // the test rather than hanging it.
    let waiting = locks.try_clone().unwrap();
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let started = Instant::now();
        let outcome = lock(&waiting, "65536", identity, Duration::from_millis(200)).err();
        let _ = sender.send((outcome, started.elapsed()));
    });
    let (error, waited) = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("a held lock is given up at its deadline");
    let error = error.unwrap();
    assert!(error.contains("deadline"), "{error}");
    assert!(waited >= Duration::from_millis(200));
    drop(held);
    lock(&locks, "65536", identity, Duration::from_millis(10)).unwrap();
    // Another owner's, a widened mode, or a link is no lock of root's.
    let foreign = (identity.0.wrapping_add(1), identity.1);
    assert!(lock(&locks, "65536", foreign, Duration::ZERO).is_err());
    assert!(private_directory(&tree, LOCKS, foreign).is_err());
    fs::set_permissions(
        temp.path().join(LOCKS).join("65536"),
        fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    assert!(lock(&locks, "65536", identity, Duration::ZERO).is_err());
    symlink("65536", temp.path().join(LOCKS).join("65537")).unwrap();
    assert!(lock(&locks, "65537", identity, Duration::ZERO).is_err());
    // A widened lock directory is narrowed again.
    fs::set_permissions(temp.path().join(LOCKS), fs::Permissions::from_mode(0o755)).unwrap();
    private_directory(&tree, LOCKS, identity).unwrap();
    let metadata = fs::symlink_metadata(temp.path().join(LOCKS)).unwrap();
    assert_eq!(metadata.mode() & 0o7777, 0o700);
    temp.cleanup().unwrap();
}

/// The manifest is root's, readable by the account's group alone, and
/// replaced whole.
#[test]
fn the_manifest_is_written_private_to_the_account() {
    let temp = TempDir::new("td-grant-manifest").unwrap();
    let directory = File::open(temp.path()).unwrap();
    let identity = owner(temp.path());
    let manifest = Manifest::new(vec![View {
        index: 2,
        access: Access::ReadWrite,
        path: "Downloads".into(),
    }])
    .unwrap();
    fs::write(temp.path().join(STAGED_MANIFEST), b"interrupted").unwrap();
    write_manifest(&directory, identity, &manifest).unwrap();
    let path = temp.path().join(grants::MANIFEST_NAME);
    let metadata = fs::symlink_metadata(&path).unwrap();
    assert_eq!(metadata.mode() & 0o7777, 0o640);
    assert_eq!((metadata.uid(), metadata.gid()), identity);
    assert_eq!(metadata.nlink(), 1);
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "td-grant-views-v1\n2\trw\tDownloads\n"
    );
    assert!(!temp.path().join(STAGED_MANIFEST).exists());
    write_manifest(&directory, identity, &Manifest::default()).unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), "td-grant-views-v1\n");
    // A link planted at the staging name is not followed.
    let elsewhere = temp.path().join("elsewhere");
    fs::write(&elsewhere, b"kept").unwrap();
    symlink(&elsewhere, temp.path().join(STAGED_MANIFEST)).unwrap();
    write_manifest(&directory, identity, &manifest).unwrap();
    assert_eq!(fs::read(&elsewhere).unwrap(), b"kept");
    temp.cleanup().unwrap();
}

/// A launch reading the table just as td-firstboot renames its successor
/// over it holds the earlier table at no link, which it takes; two links,
/// another mode or another owner it refuses.
#[test]
fn the_table_a_publish_renamed_over_is_still_the_published_table() {
    let temp = TempDir::new("td-grant-table-read").unwrap();
    let path = temp.path().join(grants::FILE_NAME);
    let write = |at: &Path| {
        fs::write(at, "td-grants-v1\n").unwrap();
        fs::set_permissions(at, fs::Permissions::from_mode(grants::MODE)).unwrap();
    };
    write(&path);
    let identity = owner(&path);
    let opened = File::open(&path).unwrap();
    assert!(published_table(&opened.metadata().unwrap(), identity));
    let successor = temp.path().join("successor");
    write(&successor);
    fs::rename(&successor, &path).unwrap();
    let unlinked = opened.metadata().unwrap();
    assert_eq!(unlinked.nlink(), 0);
    assert!(published_table(&unlinked, identity));
    assert!(!published_table(&unlinked, (identity.0 + 1, identity.1)));
    let second = temp.path().join("second");
    fs::hard_link(&path, &second).unwrap();
    assert!(!published_table(&fs::metadata(&path).unwrap(), identity));
    fs::remove_file(&second).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
    assert!(!published_table(&fs::metadata(&path).unwrap(), identity));
    temp.cleanup().unwrap();
}
