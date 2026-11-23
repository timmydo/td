#![allow(clippy::unwrap_used)]
use super::*;

fn receipt(started: f64, events: &[&str]) -> Json {
    let mut rows = Vec::new();
    for (i, event) in events.iter().enumerate() {
        rows.push(Json::Arr(vec![
            Json::from(started + i as f64),
            Json::Str(event.to_string()),
        ]));
    }
    let mut record = Json::Obj(Vec::new());
    set(&mut record, "started", Json::from(started)).unwrap();
    set(
        &mut record,
        "status",
        Json::Str(events.last().unwrap_or(&"preflight").to_string()),
    )
    .unwrap();
    set(&mut record, "events", Json::Arr(rows)).unwrap();
    record
}

fn job(op: &Path, name: &str, file: &str, record: &Json) -> PathBuf {
    let dir = op.join(name);
    fs::create_dir_all(&dir).unwrap();
    durable(&dir.join(file), record).unwrap();
    dir
}

#[test]
fn admission_began_only_once_the_socket_fence_went() {
    let opening = ["fencing", "fenced", OPENING];
    let failed = ["fencing", "fenced", OPENING, FAILED];
    for events in [&opening[..], &failed[..]] {
        assert_eq!(
            classify("activate", events, true, false, false).unwrap(),
            Interrupted::Admitting
        );
        assert_eq!(
            classify("postgres-binaries", events, true, false, false).unwrap(),
            Interrupted::Admitting
        );
    }
    // The socket fence still closed, or none used: the deployment fence
    // is what admission removes first, and it is there.
    assert_eq!(
        classify("postgres-to-debian", &opening, true, true, false).unwrap(),
        Interrupted::Switch
    );
    assert_eq!(
        classify("activate", &opening, false, false, false).unwrap(),
        Interrupted::Activation { stopped: false }
    );
    assert_eq!(
        classify("restore-shared-services", &opening, false, false, false).unwrap(),
        Interrupted::Shared
    );
    // A later step than admission's event: the rollback's own steps.
    let rolling = [
        "fencing",
        OPENING,
        "restoring original primary before writer admission",
    ];
    assert_eq!(
        classify("activate", &rolling, true, false, false).unwrap(),
        Interrupted::Activation { stopped: false }
    );
}

#[test]
fn an_activation_stopped_the_primary_only_after_the_sealed_point() {
    let before = ["fencing", "fenced"];
    let after = [
        "fencing",
        "fenced",
        SEALED,
        "migrating fenced candidate database",
    ];
    assert_eq!(
        classify("activate", &before, true, true, false).unwrap(),
        Interrupted::Activation { stopped: false }
    );
    assert_eq!(
        classify("activate", &after, true, true, false).unwrap(),
        Interrupted::Activation { stopped: true }
    );
    assert!(classify("postgres-socket", &before, true, true, false).is_err());
    assert!(classify("retire-rollback", &before, false, false, false).is_err());
}

#[test]
fn messages_and_the_last_step_skip_the_failure() {
    let record = receipt(1.0, &["fencing", OPENING, FAILED]);
    let events = messages(&record).unwrap();
    assert_eq!(events, ["fencing", OPENING, FAILED]);
    assert_eq!(last_step(&events), Some(OPENING));
    assert_eq!(last_step(&[FAILED]), None);
    assert!(messages(&Json::Obj(Vec::new())).is_err());
}

#[test]
fn the_fence_names_its_job_or_the_latest_that_fenced() {
    let dir = td_fs::TempDir::new("recover-owner").unwrap();
    let op = dir.path();
    let me = op.join("deploy-recover0");
    fs::create_dir(&me).unwrap();
    // Nothing fenced: neither an empty nor a torn fence has an owner.
    assert!(owner("", op, &me).is_err());
    assert!(owner("/var/lib/elsewhere/deploy-abcdefgh\n", op, &me).is_err());
    // A finished job never owns an unnamed fence, even as the latest.
    let mut done = receipt(11.0, &["fencing", "fenced", OPENING, "verified deployment"]);
    set(&mut done, "listeners_up_utc", Json::from(11.0)).unwrap();
    job(op, "deploy-done0000", "receipt.json", &done);
    let mut rolled = receipt(12.0, &["fencing", FAILED]);
    set(&mut rolled, "rollback", Json::from("verified")).unwrap();
    job(op, "deploy-rolled00", "receipt.json", &rolled);
    for shape in ["", "x", "/var/lib/elsewhere/deploy-abcdefgh\n"] {
        assert!(owner(shape, op, &me).is_err(), "{shape}");
    }
    job(
        op,
        "deploy-old00000",
        "receipt.json",
        &receipt(1.0, &["fencing", "verified deployment"]),
    );
    // A failed fence write: its job's status is the failure.
    let fencing = job(
        op,
        "deploy-fence000",
        "receipt.json",
        &receipt(5.0, &["fencing", FAILED]),
    );
    job(
        op,
        "deploy-later000",
        "receipt.json",
        &receipt(9.0, &[FAILED]),
    );
    job(
        op,
        "deploy-recover0",
        "receipt.json",
        &receipt(10.0, &["fencing"]),
    );
    fs::create_dir(op.join("not-a-job")).unwrap();
    let named = job(
        op,
        "deploy-abcdefgh",
        "receipt.json",
        &receipt(0.5, &["fencing"]),
    );
    let text = format!("{}\n", named.display());
    assert_eq!(owner(&text, op, &me).unwrap(), (named.clone(), false));
    // Empty, torn, elsewhere or missing: the latest job that fenced.
    for torn in [
        String::new(),
        format!("{}/deploy-abc", op.display()),
        "/var/lib/elsewhere/deploy-abcdefgh\n".to_string(),
        format!("{}/deploy-gone0000\n", op.display()),
    ] {
        assert_eq!(
            owner(&torn, op, &me).unwrap(),
            (fencing.clone(), true),
            "{torn}"
        );
    }
    // A shared-service restore's record stands for its directory.
    let shared = job(
        op,
        "deploy-shared00",
        "receipt.json",
        &receipt(2.0, &["verified deployment"]),
    );
    durable(&shared.join(SHARED), &receipt(7.0, &["fencing"])).unwrap();
    assert_eq!(owner("", op, &me).unwrap(), (shared, true));
    // An unreadable record stops the inference.
    fs::write(op.join("deploy-later000/receipt.json"), "{").unwrap();
    assert!(owner("", op, &me).is_err());
}

#[test]
fn a_recovery_that_chose_rollback_never_goes_forward() {
    let events = ["fencing", "fenced", OPENING];
    assert_eq!(
        classify("activate", &events, true, false, false).unwrap(),
        Interrupted::Admitting
    );
    assert_eq!(
        classify("activate", &events, true, false, true).unwrap(),
        Interrupted::Activation { stopped: false }
    );
    assert_eq!(
        classify("postgres-binaries", &events, true, false, true).unwrap(),
        Interrupted::Switch
    );
}

#[test]
fn only_an_admission_without_its_end_is_unfinished() {
    let record = receipt(1.0, &["fencing", OPENING, FAILED]);
    assert!(unfinished_admission(&record).unwrap());
    for end in ["rollback", "recovery_status", "listeners_up_utc"] {
        let mut ended = receipt(1.0, &["fencing", OPENING]);
        set(&mut ended, end, Json::from(1.0)).unwrap();
        assert!(!unfinished_admission(&ended).unwrap(), "{end}");
    }
    let verified = receipt(1.0, &["fencing", OPENING, "verified deployment"]);
    assert!(!unfinished_admission(&verified).unwrap());
}

#[test]
fn the_original_primary_goes_back_from_any_point_of_either_swap() {
    let dir = td_fs::TempDir::new("recover-swap").unwrap();
    let root = dir.path();
    let pgdata = root.join("pgdata");
    let original = root.join("original-pgdata");
    let unreleased = root.join("unreleased-pgdata");
    let mark = |path: &Path, text: &str| {
        fs::create_dir(path).unwrap();
        fs::write(path.join("which"), text).unwrap();
    };
    let which = || fs::read_to_string(pgdata.join("which")).unwrap();
    // Never swapped.
    mark(&pgdata, "original");
    assert!(!swap_back(&pgdata, &original, &unreleased).unwrap());
    assert_eq!(which(), "original");
    // Between the migration's two renames.
    fs::rename(&pgdata, &original).unwrap();
    assert!(swap_back(&pgdata, &original, &unreleased).unwrap());
    assert_eq!(which(), "original");
    assert!(!unreleased.exists());
    // Both renames done: the migrated primary is kept aside.
    fs::rename(&pgdata, &original).unwrap();
    mark(&pgdata, "migrated");
    assert!(swap_back(&pgdata, &original, &unreleased).unwrap());
    assert_eq!(which(), "original");
    assert_eq!(
        fs::read_to_string(unreleased.join("which")).unwrap(),
        "migrated"
    );
    // Between the rollback's two renames.
    fs::rename(&pgdata, &original).unwrap();
    assert!(swap_back(&pgdata, &original, &unreleased).unwrap());
    assert_eq!(which(), "original");
    // Swapped back: nothing moves again.
    assert!(!swap_back(&pgdata, &original, &unreleased).unwrap());
    // Never two candidates for the one aside.
    fs::rename(&pgdata, &original).unwrap();
    mark(&pgdata, "another");
    assert!(swap_back(&pgdata, &original, &unreleased).is_err());
    assert!(original.exists());
}

#[test]
fn recorded_timers_are_required() {
    let mut record = receipt(1.0, &["fencing"]);
    assert!(timers(&record).is_err());
    let list = vec![
        Json::Str("td-mta-health.timer".to_string()),
        Json::Str("td-mta-maint-backup.timer".to_string()),
    ];
    set(&mut record, "timers", Json::Arr(list)).unwrap();
    assert_eq!(
        timers(&record).unwrap(),
        ["td-mta-health.timer", "td-mta-maint-backup.timer"]
    );
}
