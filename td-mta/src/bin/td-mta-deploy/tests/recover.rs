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

#[test]
fn a_rollback_reactivates_what_the_live_release_replaced() {
    let dir = td_fs::TempDir::new("recover-rollback").unwrap();
    let op = dir.path().join("op");
    let releases = dir.path().join("releases");
    fs::create_dir_all(&op).unwrap();
    let me = op.join("deploy-rollback");
    let (old, live, tree) = ("a".repeat(40), "b".repeat(40), "c".repeat(40));
    let release = releases.join(format!("git-{old}"));
    fs::create_dir_all(&release).unwrap();
    let mut stamp = Json::Obj(Vec::new());
    set(&mut stamp, "revision", Json::Str(old.clone())).unwrap();
    set(&mut stamp, "tree", Json::Str(tree.clone())).unwrap();
    durable(&release.join("deployment-inputs.json"), &stamp).unwrap();
    let mut job_spec = Json::Obj(Vec::new());
    set(&mut job_spec, "previous", Json::Str(live.clone())).unwrap();
    set(&mut job_spec, "revision", Json::Str(live.clone())).unwrap();
    set(&mut job_spec, "mode", Json::Str("rollback".to_string())).unwrap();
    let activation = |started: f64, events: &[&str], before: u64, after: u64| {
        let mut record = receipt(started, events);
        set(&mut record, "revision", Json::Str(live.clone())).unwrap();
        set(&mut record, "previous", Json::Str(old.clone())).unwrap();
        set(&mut record, "schema_before", Json::from(before)).unwrap();
        set(&mut record, "schema_after", Json::from(after)).unwrap();
        record
    };
    let parse = |text: String| td_json::parse(&text).unwrap();
    let spec_text = format!("{job_spec}");
    let refused = || rollback_job(parse(spec_text.clone()), &op, &me, &releases).is_err();
    assert!(refused());
    let verified = activation(1.0, &["fencing", OPENING, VERIFIED], 40, 40);
    let done = job(&op, "deploy-verified", "receipt.json", &verified);
    // A later failed or socket job does not count.
    job(
        &op,
        "deploy-failed00",
        "receipt.json",
        &activation(2.0, &[FAILED], 40, 40),
    );
    let mut socket = activation(3.0, &[VERIFIED], 40, 40);
    set(
        &mut socket,
        "mode",
        Json::Str("postgres-socket".to_string()),
    )
    .unwrap();
    job(&op, "deploy-socket00", "receipt.json", &socket);
    // Nor does a configuration convergence under the live release.
    let mut converged = activation(3.5, &[VERIFIED], 40, 40);
    set(
        &mut converged,
        "mode",
        Json::Str("converge-config".to_string()),
    )
    .unwrap();
    job(&op, "deploy-converge", "receipt.json", &converged);
    let rolled = rollback_job(parse(spec_text.clone()), &op, &me, &releases).unwrap();
    assert_eq!(field(&rolled, "revision").unwrap(), old);
    assert_eq!(field(&rolled, "tree").unwrap(), tree);
    assert_eq!(field(&rolled, "previous").unwrap(), live);
    assert_eq!(
        field(&rolled, "rollback_of").unwrap(),
        done.to_str().unwrap()
    );
    // The live release must be the one it installed.
    let mut elsewhere = parse(spec_text.clone());
    set(&mut elsewhere, "previous", Json::Str("d".repeat(40))).unwrap();
    assert!(rollback_job(elsewhere, &op, &me, &releases).is_err());
    // Nothing migrated, no identity change.
    let mut migrated = activation(4.0, &[VERIFIED], 40, 41);
    job(&op, "deploy-migrate0", "receipt.json", &migrated);
    assert!(refused());
    set(&mut migrated, "schema_after", Json::from(40u64)).unwrap();
    set(&mut migrated, "recovery", Json::Str("/data/x".to_string())).unwrap();
    durable(&op.join("deploy-migrate0/receipt.json"), &migrated).unwrap();
    assert!(refused());
    let mut isolating = activation(5.0, &[VERIFIED], 40, 40);
    set(&mut isolating, "isolation_change", Json::Bool(true)).unwrap();
    job(&op, "deploy-isolate0", "receipt.json", &isolating);
    assert!(refused());
    // A verified rollback counts as an activation.
    let mut plain = activation(6.0, &[VERIFIED], 40, 40);
    set(&mut plain, "mode", Json::Str("rollback".to_string())).unwrap();
    job(&op, "deploy-plain000", "receipt.json", &plain);
    assert!(!refused());
    // A retained release whose stamp names another revision.
    set(&mut stamp, "revision", Json::Str("e".repeat(40))).unwrap();
    durable(&release.join("deployment-inputs.json"), &stamp).unwrap();
    assert!(refused());
}

#[test]
fn a_rollback_follows_a_release_kept_after_admission_and_sees_past_restores() {
    let dir = td_fs::TempDir::new("recover-kept").unwrap();
    let op = dir.path().join("op");
    let releases = dir.path().join("releases");
    fs::create_dir_all(&op).unwrap();
    let me = op.join("deploy-rollback");
    let (old, live) = ("a".repeat(40), "b".repeat(40));
    let release = releases.join(format!("git-{old}"));
    fs::create_dir_all(&release).unwrap();
    let mut stamp = Json::Obj(Vec::new());
    set(&mut stamp, "revision", Json::Str(old.clone())).unwrap();
    set(&mut stamp, "tree", Json::Str("c".repeat(40))).unwrap();
    durable(&release.join("deployment-inputs.json"), &stamp).unwrap();
    let spec = format!(r#"{{"previous":"{live}","revision":"{live}","mode":"rollback"}}"#);
    let attempt = || rollback_job(td_json::parse(&spec).unwrap(), &op, &me, &releases);
    let activation = |started: f64, events: &[&str]| {
        let mut record = receipt(started, events);
        set(&mut record, "revision", Json::Str(live.clone())).unwrap();
        set(&mut record, "previous", Json::Str(old.clone())).unwrap();
        set(&mut record, "schema_before", Json::from(40u64)).unwrap();
        set(&mut record, "schema_after", Json::from(40u64)).unwrap();
        record
    };
    // Failed and rolled back: the live release is not its.
    let mut rolled = activation(1.0, &["fencing", OPENING, FAILED]);
    set(&mut rolled, "rollback", Json::from("verified")).unwrap();
    job(&op, "deploy-rolled00", "receipt.json", &rolled);
    assert!(attempt().is_err());
    // Failed after writers returned: kept, so it installed the live one.
    let kept = job(
        &op,
        "deploy-kept0000",
        "receipt.json",
        &activation(2.0, &["fencing", OPENING, FAILED]),
    );
    let rolled = attempt().unwrap();
    assert_eq!(field(&rolled, "revision").unwrap(), old);
    assert_eq!(
        field(&rolled, "rollback_of").unwrap(),
        kept.to_str().unwrap()
    );
    // A shared-service restore's record does not hide the isolating
    // activation from the refusal that names it.
    let mut isolating = activation(3.0, &["fencing", OPENING, VERIFIED]);
    set(&mut isolating, "isolation_change", Json::Bool(true)).unwrap();
    let restored = job(&op, "deploy-isolate0", "receipt.json", &isolating);
    durable(&restored.join(SHARED), &receipt(4.0, &["fencing", FAILED])).unwrap();
    let refusal = attempt().unwrap_err().to_string();
    assert!(refusal.contains("--restore-shared-services"), "{refusal}");
    // An unretained release is named as such.
    fs::remove_file(op.join("deploy-isolate0/receipt.json")).unwrap();
    fs::remove_dir_all(&release).unwrap();
    let missing = attempt().unwrap_err().to_string();
    assert!(missing.contains("no longer retained"), "{missing}");
}

#[test]
fn a_fenced_rollback_recovers_as_an_activation() {
    let events = ["fencing", "fenced", SEALED];
    assert_eq!(
        classify("rollback", &events, true, true, false).unwrap(),
        Interrupted::Activation { stopped: true }
    );
}

#[test]
fn a_restore_returns_only_from_the_live_identity_cutover_once() {
    let dir = td_fs::TempDir::new("recover-restore").unwrap();
    let op = dir.path().join("op");
    fs::create_dir_all(&op).unwrap();
    let me = op.join("deploy-restore0");
    let (old, live) = ("a".repeat(40), "b".repeat(40));
    let spec =
        format!(r#"{{"previous":"{live}","revision":"{live}","mode":"restore-shared-services"}}"#);
    let attempt = || restore_job(td_json::parse(&spec).unwrap(), &op, &me);
    let activation = |started: f64, isolating: bool| {
        let mut record = receipt(started, &["fencing", OPENING, VERIFIED]);
        set(&mut record, "revision", Json::Str(live.clone())).unwrap();
        set(&mut record, "previous", Json::Str(old.clone())).unwrap();
        set(&mut record, "isolation_change", Json::Bool(isolating)).unwrap();
        record
    };
    assert!(attempt()
        .unwrap_err()
        .to_string()
        .contains("nothing to restore"));
    let cutover = job(
        &op,
        "deploy-isolate0",
        "receipt.json",
        &activation(1.0, true),
    );
    let restore = attempt().unwrap();
    assert_eq!(field(&restore, "previous").unwrap(), old);
    assert_eq!(field(&restore, "revision").unwrap(), live);
    assert_eq!(
        field(&restore, "restore_of").unwrap(),
        cutover.to_str().unwrap()
    );
    // A failed restore leaves it to return from; a verified one does not,
    // whether a restore job or the first controllers' record says so.
    let mut failed = receipt(2.0, &["fencing", FAILED]);
    set(
        &mut failed,
        "restore_of",
        Json::Str(cutover.to_str().unwrap().to_string()),
    )
    .unwrap();
    set(
        &mut failed,
        "mode",
        Json::Str("restore-shared-services".to_string()),
    )
    .unwrap();
    let earlier = job(&op, "deploy-restore1", "receipt.json", &failed);
    assert!(attempt().is_ok());
    let mut verified = failed.clone();
    set(
        &mut verified,
        "events",
        receipt(2.0, &["fencing", SHARED_RESTORED])
            .get("events")
            .unwrap()
            .clone(),
    )
    .unwrap();
    durable(&earlier.join("receipt.json"), &verified).unwrap();
    assert!(attempt()
        .unwrap_err()
        .to_string()
        .contains("already returned"));
    fs::remove_dir_all(&earlier).unwrap();
    assert!(attempt().is_ok());
    // A recovery that finished an interrupted restore names it too.
    set(&mut verified, "mode", Json::Str("recover".to_string())).unwrap();
    let recovery = job(&op, "deploy-recover0", "receipt.json", &verified);
    assert!(attempt()
        .unwrap_err()
        .to_string()
        .contains("already returned"));
    fs::remove_dir_all(&recovery).unwrap();
    assert!(attempt().is_ok());
    durable(&cutover.join(SHARED), &receipt(3.0, &[SHARED_RESTORED])).unwrap();
    assert!(attempt()
        .unwrap_err()
        .to_string()
        .contains("already returned"));
    fs::remove_file(cutover.join(SHARED)).unwrap();
    // A later binary activation between isolated releases installed the
    // live release: the cutover is no longer what returns.
    job(
        &op,
        "deploy-binary00",
        "receipt.json",
        &activation(4.0, false),
    );
    assert!(attempt()
        .unwrap_err()
        .to_string()
        .contains("did not isolate"));
}
