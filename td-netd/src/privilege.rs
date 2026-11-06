//! td-netd's one credential change (UNSAFE.md §2). `up` and `watch` parse what
//! the network sends, frames off the air included, so a root start prepares
//! what only root can and then drops to its own account, keeping exactly the
//! two capabilities configuring a link takes. Nothing ever regains root: the
//! process execs nothing and starts no thread, which `confinement.rs` holds.
//!
//! The drop is fixed-purpose, like td-profiler's, rather than td-login's: a
//! switch through td-login clears every capability by design.

use crate::sys;
use crate::wifi::{CONFIG_DIR, CONFIG_PATH, CONTROL_DIR};
use std::fs::{self, File};
use std::io::{self, Write};
use std::os::unix::fs::{
    fchown, DirBuilderExt, FileExt, MetadataExt, OpenOptionsExt, PermissionsExt,
};
use std::sync::OnceLock;

/// The account `system-x86-64` declares. Its uid is read back by the image's
/// boot health and by td-install, which carries the state it owns.
pub const NETD_UID: u32 = 984;
pub const NETD_GID: u32 = 984;

const CAP_NET_BIND_SERVICE: u32 = 10;
const CAP_NET_ADMIN: u32 = 12;
/// Port 68 for DHCP; interface and route ioctls and nl80211 for the rest.
const KEPT: u32 = (1 << CAP_NET_BIND_SERVICE) | (1 << CAP_NET_ADMIN);

const O_NOFOLLOW: i32 = 0o400000;
const O_NONBLOCK: i32 = 0o4000;
const O_DIRECTORY: i32 = 0o200000;

const MAX_ACCOUNTS: u64 = 1 << 20;

/// The two files outside td-netd's own directory it keeps writing. Each stays
/// root's, at the path and inode readers already bind and open, and is
/// written through the descriptor root opened.
static RESOLV_CONF: OnceLock<File> = OnceLock::new();
static HOSTS: OnceLock<File> = OnceLock::new();

/// Replace what `path` holds, through root's descriptor when there is one.
fn publish(retained: Option<&File>, path: &str, body: &str) -> io::Result<()> {
    match retained {
        // The body first, so a reader never finds the file empty: td-jail
        // refuses to bind an empty resolv.conf.
        Some(file) => {
            file.write_all_at(body.as_bytes(), 0)?;
            file.set_len(body.len() as u64)
        }
        None => fs::write(path, body),
    }
}

pub fn publish_resolv_conf(path: &str, body: &str) -> io::Result<()> {
    publish(RESOLV_CONF.get(), path, body)
}

pub fn publish_hosts(path: &str, body: &str) -> io::Result<()> {
    publish(HOSTS.get(), path, body)
}

/// A root start's whole transition. Below root there is nothing to give up,
/// which is how the crate's tests and an owner's `td-netd up` reach this.
///
/// `no_servers` is what a resolv.conf with no lease says: this makes that file
/// before there is a lease, and td-jail refuses to bind an empty one.
pub fn shed(
    wifi_owner: Option<u32>,
    resolv_conf: &str,
    no_servers: &str,
    hosts: &str,
) -> io::Result<()> {
    let status = fs::read_to_string("/proc/self/status")?;
    match root_columns(&status) {
        0 => return Ok(()),
        4 => {}
        _ => {
            return Err(io::Error::other(
                "started with root in only some uid columns",
            ))
        }
    }
    if field(&status, "Threads") != Some("1") {
        return Err(io::Error::other(
            "the credential change is per thread and this process has several",
        ));
    }
    prepare_run(wifi_owner)?;
    // Not fatal, as a volume that cannot be changed only costs saved networks.
    if let Err(error) = prepare_state() {
        let _ = writeln!(io::stderr(), "td-netd: {CONFIG_DIR}: {error}");
    }
    // Hosts first: nothing below may leave a resolv.conf empty if this fails.
    let table = open_published(hosts)?;
    let resolv = open_published(resolv_conf)?;
    if resolv.metadata()?.len() == 0 {
        publish(Some(&resolv), resolv_conf, no_servers)?;
    }
    let _ = RESOLV_CONF.set(resolv);
    let _ = HOSTS.set(table);
    sys::drop_credentials(NETD_UID, NETD_GID, KEPT)?;
    verified(&fs::read_to_string("/proc/self/status")?).map_err(io::Error::other)
}

fn open_published(path: &str) -> io::Result<File> {
    fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o644)
        .open(path)
        .map_err(|e| io::Error::new(e.kind(), format!("open {path}: {e}")))
}

/// A directory held open, so the changes below land on what was checked even
/// if a previous run of this account left something in its place.
fn own_directory(path: &str, mode: u32, gid: u32) -> io::Result<()> {
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Err(e) if e.kind() != io::ErrorKind::AlreadyExists => return Err(e),
        _ => {}
    }
    let directory = fs::OpenOptions::new()
        .read(true)
        .custom_flags(O_NOFOLLOW | O_DIRECTORY)
        .open(path)?;
    fchown(&directory, Some(NETD_UID), Some(gid))?;
    directory.set_permissions(fs::Permissions::from_mode(mode))
}

/// `/run/td-netd` for what td-netd writes, and beneath it the owner's way in:
/// mode 0710 in the owner's private group, so only that account reaches the
/// control socket. Anything wrong with that way in costs Wi-Fi control, said
/// aloud, and never the network: `bind_control` finds no directory, or one an
/// earlier start of this boot prepared.
fn prepare_run(wifi_owner: Option<u32>) -> io::Result<()> {
    own_directory(crate::RUN_DIR, 0o755, NETD_GID)?;
    if let Some(owner) = wifi_owner {
        if let Err(error) = prepare_control(owner) {
            let _ = writeln!(io::stderr(), "td-netd: no Wi-Fi control: {error}");
        }
    }
    Ok(())
}

fn prepare_control(owner: u32) -> io::Result<()> {
    let (passwd, group) = (read_bounded("/etc/passwd")?, read_bounded("/etc/group")?);
    let gid = private_gid(&passwd, &group, owner)
        .ok_or_else(|| io::Error::other(format!("uid {owner} has no private primary group")))?;
    own_directory(CONTROL_DIR, 0o710, gid)
}

fn read_bounded(path: &str) -> io::Result<String> {
    use std::io::Read;
    let mut text = String::new();
    File::open(path)?
        .take(MAX_ACCOUNTS)
        .read_to_string(&mut text)?;
    Ok(text)
}

/// Saved networks an older deployment or td-install left root's become this
/// account's. A file that is not plain, or has a second name, keeps its owner:
/// handing it over would give this account whatever that name is.
fn prepare_state() -> io::Result<()> {
    own_directory(CONFIG_DIR, 0o700, NETD_GID)?;
    let file = match fs::OpenOptions::new()
        .read(true)
        .custom_flags(O_NOFOLLOW | O_NONBLOCK)
        .open(CONFIG_PATH)
    {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    let meta = file.metadata()?;
    if !state_file_may_change_hands(meta.is_file(), meta.nlink(), meta.mode()) {
        return Err(io::Error::other(
            "the saved networks are not a private file",
        ));
    }
    fchown(&file, Some(NETD_UID), Some(NETD_GID))
}

fn state_file_may_change_hands(regular: bool, links: u64, mode: u32) -> bool {
    regular && links == 1 && mode & 0o077 == 0
}

/// The owner's primary gid, if the group holds that account alone.
fn private_gid(passwd: &str, group: &str, uid: u32) -> Option<u32> {
    let account = passwd.lines().find_map(|line| {
        let fields: Vec<&str> = line.split(':').collect();
        (fields.get(2)?.parse::<u32>().ok()? == uid)
            .then(|| Some((*fields.first()?, fields.get(3)?.parse::<u32>().ok()?)))?
    });
    let (name, gid) = account?;
    let sharers = passwd
        .lines()
        .filter(|line| line.split(':').nth(3).and_then(|g| g.parse::<u32>().ok()) == Some(gid))
        .count();
    let groups: Vec<&str> = group
        .lines()
        .filter(|line| line.split(':').nth(2).and_then(|g| g.parse::<u32>().ok()) == Some(gid))
        .collect();
    let [only] = groups.as_slice() else {
        return None;
    };
    let members = only.split(':').nth(3).unwrap_or("");
    (sharers == 1 && members.split(',').all(|m| m.is_empty() || m == name)).then_some(gid)
}

fn field<'a>(status: &'a str, name: &str) -> Option<&'a str> {
    status
        .lines()
        .find_map(|line| line.strip_prefix(name)?.strip_prefix(':'))
        .map(str::trim)
}

/// How many of the four uid columns are root; an unreadable row counts as
/// partial, which refuses.
fn root_columns(status: &str) -> usize {
    let columns: Vec<&str> = field(status, "Uid")
        .map(|row| row.split_whitespace().collect())
        .unwrap_or_default();
    if columns.len() != 4 {
        return 1;
    }
    columns.iter().filter(|column| **column == "0").count()
}

/// What the kernel says after the change. Believed over the calls' returns,
/// and the process refuses to go near the network on any difference.
fn verified(status: &str) -> Result<(), String> {
    let four = |id: u32| format!("{id}\t{id}\t{id}\t{id}");
    let kept = format!("{KEPT:016x}");
    let wanted = [
        ("Uid", four(NETD_UID)),
        ("Gid", four(NETD_GID)),
        ("Groups", String::new()),
        ("CapInh", format!("{:016x}", 0)),
        ("CapPrm", kept.clone()),
        ("CapEff", kept),
        ("CapAmb", format!("{:016x}", 0)),
        ("NoNewPrivs", "1".to_string()),
    ];
    for (name, value) in &wanted {
        let seen = field(status, name);
        if seen != Some(value.as_str()) {
            return Err(format!(
                "the credential change did not take: {name} reads {seen:?}, wanted {value:?}"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn status(uid: &str, groups: &str, inh: &str, prm: &str, eff: &str, nnp: &str) -> String {
        format!(
            "Name:\ttd-netd\nThreads:\t1\nUid:\t{uid}\nGid:\t984\t984\t984\t984\nGroups:\t{groups}\n\
             CapInh:\t{inh}\nCapPrm:\t{prm}\nCapEff:\t{eff}\nCapAmb:\t0000000000000000\n\
             NoNewPrivs:\t{nnp}\n"
        )
    }

    const NONE: &str = "0000000000000000";
    const KEPT_HEX: &str = "0000000000001400";
    const UID: &str = "984\t984\t984\t984";

    /// Exactly the two capabilities: bits 10 and 12, and nothing else.
    #[test]
    fn the_kept_set_is_exactly_the_two_a_link_needs() {
        assert_eq!(KEPT, 0x1400);
        assert_eq!(NETD_UID, 984);
        assert_eq!(NETD_GID, 984);
    }

    #[test]
    fn only_the_exact_outcome_is_believed() {
        assert_eq!(
            verified(&status(UID, "", NONE, KEPT_HEX, KEPT_HEX, "1")),
            Ok(())
        );
        for (name, bad) in [
            (
                "a saved root uid",
                status("984\t984\t0\t984", "", NONE, KEPT_HEX, KEPT_HEX, "1"),
            ),
            (
                "a kept group",
                status(UID, "0", NONE, KEPT_HEX, KEPT_HEX, "1"),
            ),
            (
                "an inheritable",
                status(UID, "", KEPT_HEX, KEPT_HEX, KEPT_HEX, "1"),
            ),
            (
                "a wider permitted",
                status(UID, "", NONE, "0000000000001401", KEPT_HEX, "1"),
            ),
            (
                "a lost effective",
                status(UID, "", NONE, KEPT_HEX, NONE, "1"),
            ),
            (
                "no NoNewPrivs",
                status(UID, "", NONE, KEPT_HEX, KEPT_HEX, "0"),
            ),
            ("an empty status", String::new()),
        ] {
            assert!(verified(&bad).is_err(), "{name} was believed");
        }
    }

    #[test]
    fn a_partial_root_refuses_and_only_all_or_none_proceeds() {
        let row = |uid: &str| format!("Uid:\t{uid}\n");
        assert_eq!(root_columns(&row("0\t0\t0\t0")), 4);
        assert_eq!(root_columns(&row("1000\t1000\t1000\t1000")), 0);
        assert_eq!(root_columns(&row("1000\t0\t0\t0")), 3);
        assert_eq!(root_columns(&row("0\t0")), 1, "a short row is not none");
        assert_eq!(root_columns(""), 1, "no row is not none");
    }

    #[test]
    fn a_control_group_must_hold_the_owner_alone() {
        let passwd = "root:x:0:0::/root:/bin/sh\ntester:x:1000:1000::/home/tester:/bin/sh\n";
        let group = "root:x:0:\ntester:x:1000:\nsvc-log:x:988:tester\n";
        assert_eq!(private_gid(passwd, group, 1000), Some(1000));
        let named = "tester:x:1000:tester\n";
        assert_eq!(private_gid(passwd, named, 1000), Some(1000));
        assert_eq!(private_gid(passwd, group, 1001), None, "no account");
        assert_eq!(private_gid(passwd, "root:x:0:\n", 1000), None, "no group");
        let member = "tester:x:1000:mallory\n";
        assert_eq!(private_gid(passwd, member, 1000), None, "a second member");
        let twice = "tester:x:1000:\nother:x:1000:\n";
        assert_eq!(private_gid(passwd, twice, 1000), None, "a second group row");
        let shared = format!("{passwd}other:x:1001:1000::/:/bin/sh\n");
        assert_eq!(private_gid(&shared, group, 1000), None, "a second account");
    }

    /// A previous run of this account could hardlink any file on the volume
    /// here; root handing that over would give the account the other name.
    #[test]
    fn only_a_plain_private_single_named_file_changes_hands() {
        assert!(state_file_may_change_hands(true, 1, 0o100600));
        assert!(
            !state_file_may_change_hands(true, 2, 0o100600),
            "a hard link"
        );
        assert!(!state_file_may_change_hands(false, 1, 0o010600), "a FIFO");
        assert!(!state_file_may_change_hands(true, 1, 0o100640), "shared");
    }

    #[test]
    fn publishing_without_a_retained_descriptor_writes_the_path() {
        let dir = td_fs::TempDir::new("td-netd-privilege").unwrap();
        let path = dir.path().join("resolv.conf");
        let path = path.to_str().unwrap();
        publish(None, path, "nameserver 10.0.0.1\n").unwrap();
        assert_eq!(fs::read_to_string(path).unwrap(), "nameserver 10.0.0.1\n");
        // Through a descriptor the inode survives and a shorter body leaves
        // nothing of a longer one, which a reader that bound it relies on.
        let held = fs::OpenOptions::new().write(true).open(path).unwrap();
        let before = fs::metadata(path).unwrap().ino();
        publish(Some(&held), path, "x\n").unwrap();
        assert_eq!(fs::read_to_string(path).unwrap(), "x\n");
        assert_eq!(fs::metadata(path).unwrap().ino(), before);
    }
}
