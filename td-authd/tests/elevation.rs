#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
use super::*;
use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

/// The image's table: UID 1000 may perform every v1 operation.
pub(crate) const V1: &str =
    "td-elevation-v1\n1000\tdeploy-rollback\tset-hostname\tdeploy-publish\n";

/// A table granting `uid` every v1 operation, outside the grammar's UID
/// range when a test runs as such a UID.
pub(crate) fn granting(uid: u32) -> Table {
    Table {
        rows: vec![(uid, Operation::ALL.to_vec())],
    }
}

/// A private directory holding `text` as the table, mode 0444, owned as
/// the directory is; removed on drop.
pub(crate) struct Etc {
    pub(crate) path: PathBuf,
    pub(crate) owner: (u32, u32),
}

impl Etc {
    pub(crate) fn new(text: Option<&str>) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "td-elevation-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        if let Some(text) = text {
            let table = path.join(TABLE);
            fs::write(&table, text).unwrap();
            fs::set_permissions(&table, fs::Permissions::from_mode(0o444)).unwrap();
        }
        let metadata = fs::metadata(&path).unwrap();
        Self {
            path,
            owner: (metadata.uid(), metadata.gid()),
        }
    }

    pub(crate) fn load(&self) -> Result<Table, String> {
        Table::load_from(&self.path, self.owner)
    }
}

impl Drop for Etc {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[test]
fn the_v1_table_grants_uid_1000_every_operation_and_no_one_else() {
    let table = Table::parse(V1).unwrap();
    for &operation in Operation::ALL {
        assert!(table.grants(1000, operation), "{operation:?}");
        assert!(!table.grants(1001, operation), "{operation:?}");
        assert!(!table.grants(0, operation), "{operation:?}");
    }
    let rollback_only =
        Table::parse("td-elevation-v1\n1000\tdeploy-rollback\n1001\tset-hostname\n").unwrap();
    assert!(rollback_only.grants(1000, Operation::DeployRollback));
    assert!(!rollback_only.grants(1000, Operation::SetHostname));
    assert!(rollback_only.grants(1001, Operation::SetHostname));
    assert!(!rollback_only.grants(1001, Operation::DeployRollback));
    // A table of no rows is well formed and grants nothing.
    let empty = Table::parse("td-elevation-v1\n").unwrap();
    assert!(!empty.grants(1000, Operation::DeployRollback));
}

#[test]
fn every_departure_from_the_canonical_grammar_refuses() {
    for text in [
        "",
        "td-elevation-v1",
        "td-elevation-v2\n1000\tdeploy-rollback\n",
        "td-elevation-v1\n\n",
        "td-elevation-v1\n1000\n",
        "td-elevation-v1\n1000\t\n",
        "td-elevation-v1\n1000\tdeploy-rollback\t\n",
        "td-elevation-v1\n1000 deploy-rollback\n",
        "td-elevation-v1\n1000\tDEPLOY-ROLLBACK\n",
        "td-elevation-v1\n1000\televate\n",
        "td-elevation-v1\n1000\tdeploy-rollback\tdeploy-rollback\n",
        "td-elevation-v1\n1000\tset-hostname\tdeploy-rollback\n",
        "td-elevation-v1\n01000\tdeploy-rollback\n",
        "td-elevation-v1\n+1000\tdeploy-rollback\n",
        "td-elevation-v1\n999\tdeploy-rollback\n",
        "td-elevation-v1\n0\tdeploy-rollback\n",
        "td-elevation-v1\n65534\tdeploy-rollback\n",
        "td-elevation-v1\n100000\tdeploy-rollback\n",
        "td-elevation-v1\n1001\tdeploy-rollback\n1000\tdeploy-rollback\n",
        "td-elevation-v1\n1000\tdeploy-rollback\n1000\tset-hostname\n",
        "td-elevation-v1\r\n1000\tdeploy-rollback\n",
        "td-elevation-v1\n1000\tdeploy-rollback\r\n",
        " td-elevation-v1\n1000\tdeploy-rollback\n",
        "# td\ntd-elevation-v1\n1000\tdeploy-rollback\n",
    ] {
        assert!(Table::parse(text).is_err(), "{text:?}");
    }
    let mut rows = String::from("td-elevation-v1\n");
    for uid in 1000..1000 + ROWS {
        rows.push_str(&format!("{uid}\tdeploy-rollback\n"));
    }
    assert!(Table::parse(&rows).is_ok());
    rows.push_str(&format!("{}\tdeploy-rollback\n", 1000 + ROWS));
    assert_eq!(
        Table::parse(&rows).unwrap_err(),
        "the elevation table has too many rows"
    );
}

#[test]
fn only_a_protected_single_link_mode_0444_table_of_its_owner_is_read() {
    let good = Etc::new(Some(V1));
    assert_eq!(good.load().unwrap(), Table::parse(V1).unwrap());
    // Missing.
    assert!(Etc::new(None).load().is_err());
    // Another owner than the directory's, as root's table is in production.
    assert!(Table::load_from(&good.path, (good.owner.0.wrapping_add(1), good.owner.1)).is_err());
    // Any mode but 0444.
    for mode in [0o644, 0o440, 0o400, 0o4444, 0o666] {
        let etc = Etc::new(Some(V1));
        fs::set_permissions(etc.path.join(TABLE), fs::Permissions::from_mode(mode)).unwrap();
        assert!(etc.load().is_err(), "{mode:o}");
    }
    // A writable directory.
    let open = Etc::new(Some(V1));
    fs::set_permissions(&open.path, fs::Permissions::from_mode(0o775)).unwrap();
    assert!(open.load().is_err());
    // A second link.
    let linked = Etc::new(Some(V1));
    fs::hard_link(linked.path.join(TABLE), linked.path.join("copy")).unwrap();
    assert!(linked.load().is_err());
    // A link in the table's place, and a directory that is a link.
    let link = Etc::new(None);
    symlink(good.path.join(TABLE), link.path.join(TABLE)).unwrap();
    assert!(link.load().is_err());
    let aliased = Etc::new(None);
    let alias = aliased.path.join("alias");
    symlink(&good.path, &alias).unwrap();
    assert!(Table::load_from(&alias, good.owner).is_err());
    // A directory in the table's place.
    let directory = Etc::new(None);
    fs::create_dir(directory.path.join(TABLE)).unwrap();
    assert!(directory.load().is_err());
    // Past the bound, though well formed.
    let large = Etc::new(None);
    let mut text = String::from(V1);
    while u64::try_from(text.len()).unwrap() <= LIMIT {
        text.push_str("td-elevation-v1\n");
    }
    fs::write(large.path.join(TABLE), &text).unwrap();
    fs::set_permissions(large.path.join(TABLE), fs::Permissions::from_mode(0o444)).unwrap();
    assert!(large.load().is_err());
    // A malformed one.
    assert!(Etc::new(Some("td-elevation-v1\n1000\tsudo\n"))
        .load()
        .is_err());
    // Production reads root's /etc.
    assert_eq!(DIRECTORY, "/etc");
    assert_eq!(TABLE, "td-elevation.tsv");
}
