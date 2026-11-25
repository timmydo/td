#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
use super::*;
use std::cell::RefCell;

const MIB: u64 = 1 << 20;

/// Production: one root filesystem for the data directory, the store and
/// the job directories, and the data disk for the recovery volume.
fn production(root_free: u64, data_free: u64) -> impl FnMut(Place) -> R<Volume> {
    move |place| {
        Ok(match place {
            Place::Recovery => Volume {
                device: 2081,
                mount: "/data".into(),
                available: data_free,
            },
            Place::Data | Place::Store | Place::Jobs => Volume {
                device: 2049,
                mount: "/".into(),
                available: root_free,
            },
        })
    }
}

fn sizes() -> Sizes {
    Sizes {
        data: 1200 * MIB,
        wal: 100 * MIB,
        ..Sizes::default()
    }
}

#[test]
fn a_migration_needs_its_copies_growth_and_the_receivers_reserve_beside_the_cold_copy() {
    let writes = writes(Job::Migration, &sizes()).unwrap();
    let verdicts = assess(&writes, production(9 * GIB, 30 * GIB)).unwrap();
    assert_eq!(verdicts.len(), 2);
    let data = &verdicts[0];
    assert_eq!(data.mount, "/data");
    assert_eq!(
        data.needed,
        4 * 1100 * MIB + WAL + BOOTSTRAP + RECEIVER_FREE
    );
    assert_eq!(
        data.parts,
        "3 × 1.1 GiB base backup, migration trial and replay trial + 1.1 GiB migration trial growth + 3.0 GiB WAL spool, archive and replay + 1.0 GiB bootstrap archive + 8.0 GiB kept free"
    );
    let root = &verdicts[1];
    assert_eq!(root.mount, "/");
    // The cold copy is the whole data directory, its WAL included.
    assert_eq!(root.needed, 1200 * MIB + RESERVE);
    assert!(refusal(Job::Migration, &verdicts).unwrap().is_none());
}

#[test]
fn a_short_filesystem_refuses_with_needed_against_available_and_no_command() {
    let writes = writes(Job::Migration, &sizes()).unwrap();
    let verdicts = assess(&writes, production(2300 * MIB, 30 * GIB)).unwrap();
    let message = refusal(Job::Migration, &verdicts).unwrap().unwrap();
    assert!(message.starts_with(REFUSAL), "{message}");
    assert!(
        message.contains("/ needs 5.2 GiB (1.2 GiB cold copy of the data directory + 4.0 GiB kept free) and has 2.2 GiB available"),
        "{message}"
    );
    // The filesystem with room is not named.
    assert!(!message.contains("/data"), "{message}");
    // Nothing the launcher would take for a command to follow.
    assert!(!message.contains("run td-mta/deploy"), "{message}");
    // Both short: each named, in order.
    let verdicts = assess(&writes, production(GIB, GIB)).unwrap();
    let message = refusal(Job::Migration, &verdicts).unwrap().unwrap();
    let first = message.find("/data needs").unwrap();
    let second = message.find("; / needs").unwrap();
    assert!(first < second, "{message}");
    // As the running guards want more free than theirs, exactly enough
    // is not enough.
    let needed = verdicts[1].needed;
    let verdicts = assess(&writes, production(needed + 1, 30 * GIB)).unwrap();
    assert!(refusal(Job::Migration, &verdicts).unwrap().is_none());
    let verdicts = assess(&writes, production(needed, 30 * GIB)).unwrap();
    assert!(refusal(Job::Migration, &verdicts).unwrap().is_some());
}

#[test]
fn a_switch_sums_the_closure_and_gin_rebuild_on_one_filesystem() {
    let mut sizes = sizes();
    sizes.gin = 300 * MIB;
    sizes.unpack = 150 * MIB;
    let verdicts = assess(
        &writes(Job::Switch, &sizes).unwrap(),
        production(9 * GIB, 30 * GIB),
    )
    .unwrap();
    assert_eq!(verdicts[0].mount, "/data");
    // The trial rebuilds its GIN indexes too.
    assert_eq!(verdicts[0].needed, 2 * 1100 * MIB + 2 * 300 * MIB + RESERVE);
    // The store and the data directory share the root: one reserve.
    assert_eq!(verdicts[1].mount, "/");
    assert_eq!(verdicts[1].needed, 150 * MIB + 2 * 300 * MIB + RESERVE);
    // Before the build nothing of the closure or the notes is known; a
    // zero write still claims its filesystem's reserve.
    let verdicts = assess(
        &writes(Job::Switch, &self::sizes()).unwrap(),
        production(9 * GIB, 30 * GIB),
    )
    .unwrap();
    assert_eq!(verdicts[1].needed, RESERVE);
    assert_eq!(verdicts[1].parts, "4.0 GiB kept free");
    // The return to Debian copies nothing and keeps room for the restart,
    // less than a copy's reserve: it is the way back from a full root.
    let verdicts = assess(
        &writes(Job::ToDebian, &self::sizes()).unwrap(),
        production(2 * GIB, 0),
    )
    .unwrap();
    assert_eq!(verdicts.len(), 1);
    assert_eq!(verdicts[0].needed, RESTART);
    assert!(refusal(Job::ToDebian, &verdicts).unwrap().is_none());
    let verdicts = assess(
        &writes(Job::ToDebian, &self::sizes()).unwrap(),
        production(RESTART, 0),
    )
    .unwrap();
    assert!(refusal(Job::ToDebian, &verdicts).unwrap().is_some());
    assert!(!Job::ToDebian.measures_data() && !Job::Upload.measures_data());
    assert!(Job::Migration.measures_data() && Job::Switch.measures_data());
    // The upload lands in the job directory and again in the store.
    let upload = Sizes {
        upload: 200 * MIB,
        unpack: 150 * MIB,
        ..Sizes::default()
    };
    let verdicts = assess(
        &writes(Job::Upload, &upload).unwrap(),
        production(9 * GIB, 0),
    )
    .unwrap();
    assert_eq!(verdicts.len(), 1);
    assert_eq!(verdicts[0].needed, 350 * MIB + RESERVE);
}

#[test]
fn each_place_is_looked_up_once_and_overflow_refuses() {
    let asked = RefCell::new(Vec::new());
    let mut lookup = production(9 * GIB, 30 * GIB);
    assess(&writes(Job::Migration, &sizes()).unwrap(), |place| {
        asked.borrow_mut().push(place);
        lookup(place)
    })
    .unwrap();
    assert_eq!(*asked.borrow(), [Place::Recovery, Place::Data]);
    let huge = Sizes {
        data: u64::MAX,
        ..Sizes::default()
    };
    assert!(assess(
        &writes(Job::Migration, &huge).unwrap(),
        production(9 * GIB, 30 * GIB)
    )
    .is_err());
}

#[test]
fn the_receipt_records_each_filesystem_in_bytes() {
    let verdicts = assess(
        &writes(Job::ToDebian, &sizes()).unwrap(),
        production(5 * GIB, 0),
    )
    .unwrap();
    let json = json(&verdicts).unwrap();
    let row = &json.as_arr().unwrap()[0];
    assert_eq!(row.get("mount").and_then(Json::as_str), Some("/"));
    assert_eq!(row.get("needed").and_then(Json::as_u64), Some(RESTART));
    assert_eq!(row.get("available").and_then(Json::as_u64), Some(5 * GIB));
}

#[test]
fn du_and_df_output_is_read_strictly() {
    assert_eq!(
        du_bytes("1181220864\t/var/lib/td-mta/mail/postgres").unwrap(),
        1_181_220_864
    );
    assert!(du_bytes("").is_err());
    assert!(du_bytes("12K\t/x").is_err());
    assert_eq!(
        df_row("Mounted on       Avail\n/data      12884901888").unwrap(),
        ("/data".to_string(), 12_884_901_888)
    );
    assert_eq!(
        df_row("Mounted on Avail\n/mnt/with space 42").unwrap(),
        ("/mnt/with space".to_string(), 42)
    );
    for bad in [
        "",
        "Mounted on Avail",
        "Filesystem Avail\n/ 1",
        "Mounted on Avail\n/ 1\n/data 2",
        "Mounted on Avail\n/ -",
        "Mounted on Avail\nrelative 1",
    ] {
        assert!(df_row(bad).is_err(), "{bad}");
    }
}

#[test]
fn a_place_not_there_yet_is_measured_on_its_nearest_ancestor() {
    let asked = RefCell::new(Vec::new());
    let run = |args: &[&str]| -> R<(Option<i32>, String)> {
        asked.borrow_mut().push(args.join(" "));
        Ok(match args {
            ["test", "-e", "/td/store" | "/td"] => (Some(1), String::new()),
            ["test", "-e", _] => (Some(0), String::new()),
            ["stat", .., "/"] => (Some(0), "2049".into()),
            ["df", .., "/"] => (Some(0), "Mounted on Avail\n/ 1000".into()),
            _ => return Err("unexpected".into()),
        })
    };
    let found = volume(&run, Place::Store).unwrap();
    assert_eq!(
        found,
        Volume {
            device: 2049,
            mount: "/".into(),
            available: 1000
        }
    );
    assert_eq!(
        asked.borrow().last().map(String::as_str),
        Some("df -B1 --output=target,avail -- /")
    );
    // A test that fails other than by "absent" is no answer.
    let broken = |args: &[&str]| -> R<(Option<i32>, String)> {
        Ok(match args {
            ["test", ..] => (None, String::new()),
            _ => (Some(0), String::new()),
        })
    };
    assert!(volume(&broken, Place::Store).is_err());
    // A file the live cluster removes while du walks makes it exit 1
    // with its total still printed; any other failure refuses.
    let sizes = |args: &[&str]| -> R<(Option<i32>, String)> {
        Ok(match args {
            ["du", "-s", "-x", "-B1", "--", path] if path.ends_with("/pg_wal") => {
                (Some(0), "100\tx".into())
            }
            ["du", "-s", "-x", "-B1", "--", _] => (Some(1), "1200\tx".into()),
            _ => return Err("unexpected".into()),
        })
    };
    assert_eq!(data_sizes(&sizes).unwrap(), (1200, 100));
    let failing = |_: &[&str]| -> R<(Option<i32>, String)> { Ok((Some(2), String::new())) };
    assert!(data_sizes(&failing).is_err());
}
