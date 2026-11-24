#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
use super::*;
use td_fs::TempDir;

const DIGEST: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";

fn outcome(line: &str) -> Json {
    let json = line
        .strip_prefix("td-mta-deploy-outcome ")
        .and_then(|rest| rest.strip_suffix('\n'))
        .unwrap();
    assert!(!json.contains('\n'));
    td_json::parse(json).unwrap()
}

fn launched() -> Session {
    Session {
        reported: false,
        launched: Some(Launch {
            receipt: "/var/lib/td-mta-operator/deploy-00000001/receipt.json".into(),
            unit: "td-mta-deploy-0001".into(),
        }),
        again: [
            "td-mta/deploy",
            "--converge-config",
            "--plan",
            "--config",
            "/t.json",
        ]
        .iter()
        .map(|w| w.to_string())
        .collect(),
    }
}

/// Runs `finish` and returns its line and whether it returned an error.
fn finished(receipt: Option<&str>, job: Job, public: R<f64>) -> (Json, bool) {
    let mut out = Vec::new();
    let mut session = launched();
    let carried = ["--config", "/t.json", "--jmap-account", "ops"];
    let receipt = match receipt {
        Some("unreadable") => Receipt::Unreadable,
        Some(text) if text.starts_with("refused:") => Receipt::Refused(&text[8..]),
        Some(text) => Receipt::Text(text),
        None => Receipt::Missing,
    };
    let failed = finish(&mut out, &mut session, receipt, job, public, &carried).is_err();
    assert!(session.reported);
    (outcome(std::str::from_utf8(&out).unwrap()), failed)
}

fn result(line: &Json) -> &str {
    line.get("result").and_then(Json::as_str).unwrap()
}

#[test]
fn a_plan_names_the_bound_applying_command_and_other_results_end_the_sequence() {
    let receipt = format!(
        r#"{{"converge_result":"planned","config_digest":"{DIGEST}","config_selected":"01","config_changes":[{{"profile":"frontend","section":"limits","key":"event_streams","before":4,"after":null}}]}}"#
    );
    let (planned, failed) = finished(Some(&receipt), Job::Succeeded, Ok(1.0));
    assert!(!failed);
    assert_eq!(result(&planned), "planned");
    assert!(planned.get("error").is_none());
    assert_eq!(
        planned.get("selected_version").and_then(Json::as_str),
        Some("01")
    );
    assert_eq!(
        planned
            .get("changes")
            .and_then(Json::as_arr)
            .map(<[Json]>::len),
        Some(1)
    );
    let next = planned.get("next").unwrap();
    let argv: Vec<_> = next
        .get("argv")
        .and_then(Json::as_arr)
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(
        argv,
        [
            "td-mta/deploy",
            "--converge-config",
            "--expect-digest",
            DIGEST,
            "--config",
            "/t.json",
            "--jmap-account",
            "ops"
        ]
    );
    assert_eq!(next.get("needs").and_then(Json::as_arr), Some(&[][..]));
    // Applying it parses as one bound job.
    let args: Vec<String> = argv[1..].iter().map(|s| s.to_string()).collect();
    let parsed = parse(&args).unwrap();
    assert!(parsed.converge && !parsed.plan && parsed.expect == Some(DIGEST));
    for settled in ["converged", "unchanged", "restored", "failed", "uncertain"] {
        let receipt = format!(r#"{{"converge_result":"{settled}","config_digest":"{DIGEST}"}}"#);
        let (line, _) = finished(Some(&receipt), Job::Succeeded, Ok(1.0));
        assert_eq!(result(&line), settled);
        assert!(line.get("next").unwrap().is_null(), "{settled}");
    }
    // A plan whose public check failed does not invite applying it.
    let (failed_plan, failed) =
        finished(Some(&receipt), Job::Succeeded, Err("no mailboxes".into()));
    assert!(failed);
    assert!(failed_plan.get("next").unwrap().is_null());
}

#[test]
fn the_receipt_settles_the_result_and_otherwise_the_job_state_does() {
    let (restored, failed) = finished(
        Some(
            r#"{"converge_result":"restored","error":"probe","config_selected":"01","config_version":"02","config_previous_version":"01"}"#,
        ),
        Job::Failed,
        Ok(1.0),
    );
    assert!(failed);
    assert_eq!(result(&restored), "restored");
    assert_eq!(restored.get("error").and_then(Json::as_str), Some("probe"));
    assert_eq!(
        restored.get("config_version").and_then(Json::as_str),
        Some("02")
    );
    assert_eq!(
        restored
            .get("previous_config_version")
            .and_then(Json::as_str),
        Some("01")
    );
    assert_eq!(
        restored.get("unit").and_then(Json::as_str),
        Some("td-mta-deploy-0001")
    );
    let (uncertain, _) = finished(
        Some(
            r#"{"converge_result":"uncertain","config_selected":null,"selection_request":"aa","rollback_request":"bb"}"#,
        ),
        Job::Failed,
        Ok(1.0),
    );
    assert_eq!(uncertain.get("request").and_then(Json::as_str), Some("bb"));
    assert!(uncertain.get("selected_version").unwrap().is_null());
    // A lost connection before the job settled is not a refusal.
    let (lost, failed) = finished(Some(r#"{"status":"publishing"}"#), Job::Lost, Ok(1.0));
    assert!(failed);
    assert_eq!(result(&lost), "unknown");
    assert!(lost.get("receipt").is_some());
    let (lost, _) = finished(None, Job::Lost, Ok(1.0));
    assert_eq!(result(&lost), "unknown");
    // No receipt: the supervisor refused before reserving one.
    let (refused, _) = finished(None, Job::Failed, Ok(1.0));
    assert_eq!(result(&refused), "refused");
    // A receipt without a settled result: the job died or still runs.
    let (died, failed) = finished(Some(r#"{"error":"held lock"}"#), Job::Failed, Ok(1.0));
    assert!(failed);
    assert_eq!(result(&died), "unknown");
    assert_eq!(died.get("error").and_then(Json::as_str), Some("held lock"));
    for receipt in [r#"{"converge_result":"other"}"#, "not json", "unreadable"] {
        for job in [Job::Succeeded, Job::Failed] {
            let (line, failed) = finished(Some(receipt), job, Ok(1.0));
            assert!(failed, "{receipt}");
            assert_eq!(result(&line), "unknown", "{receipt}");
        }
    }
    // A preflight refusal reports the job's reason, however it was read.
    for job in [Job::Failed, Job::Lost, Job::Succeeded] {
        let (line, failed) = finished(
            Some(r#"{"converge_result":"refused","error":"profile readiness timeout"}"#),
            job,
            Ok(1.0),
        );
        assert!(failed);
        assert_eq!(result(&line), "refused");
        assert_eq!(
            line.get("error").and_then(Json::as_str),
            Some("profile readiness timeout")
        );
    }
    // A settled result survives a lost connection, and a plan keeps next.
    let settled = format!(r#"{{"converge_result":"planned","config_digest":"{DIGEST}"}}"#);
    let (planned, failed) = finished(Some(&settled), Job::Lost, Ok(1.0));
    assert!(!failed);
    assert_eq!(result(&planned), "planned");
    assert!(!planned.get("next").unwrap().is_null());
    let (restored, failed) = finished(
        Some(r#"{"converge_result":"restored"}"#),
        Job::Lost,
        Ok(1.0),
    );
    assert!(failed);
    assert_eq!(result(&restored), "restored");
    // Converged, then the launcher's own check failed: still converged.
    let (converged, failed) = finished(
        Some(r#"{"converge_result":"converged"}"#),
        Job::Succeeded,
        Err("no mailboxes".into()),
    );
    assert!(failed);
    assert_eq!(result(&converged), "converged");
    assert!(converged
        .get("error")
        .and_then(Json::as_str)
        .unwrap()
        .starts_with("public JMAP check"));
}

#[test]
fn a_launcher_error_reports_once_refused_before_the_job_and_unknown_after() {
    let mut out = Vec::new();
    let mut session = Session::default();
    unreported(&mut out, &mut session, "no target", &[]).unwrap();
    unreported(&mut out, &mut session, "again", &[]).unwrap();
    let text = std::str::from_utf8(&out).unwrap();
    assert_eq!(text.lines().count(), 1);
    let line = outcome(text);
    assert_eq!(result(&line), "refused");
    assert!(line.get("receipt").is_none());
    let mut out = Vec::new();
    let mut session = launched();
    unreported(&mut out, &mut session, "cat failed", &[]).unwrap();
    let line = outcome(std::str::from_utf8(&out).unwrap());
    assert_eq!(result(&line), "unknown");
    assert!(line.get("receipt").is_some());
    // After finish has reported, a later error adds nothing.
    let mut out = Vec::new();
    let mut session = launched();
    let _ = finish(
        &mut out,
        &mut session,
        Receipt::Missing,
        Job::Failed,
        Ok(1.0),
        &[],
    );
    unreported(&mut out, &mut session, "later", &[]).unwrap();
    assert_eq!(std::str::from_utf8(&out).unwrap().lines().count(), 1);
    let args: Vec<String> = ["--converge-config", "--config", "/t.json", "--plan"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    assert_eq!(carried(&args).unwrap(), ["--config", "/t.json"]);
}

#[test]
fn the_policy_comes_from_the_live_revision_or_the_command_refuses() {
    let tmp = TempDir::new("mta-deploy-policy").unwrap();
    let root = tmp.path();
    let run = |args: &[&str]| {
        let out = Command::new("git")
            .current_dir(root)
            .args([
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@localhost",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success());
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    };
    run(&["init", "-q", "-b", "main"]);
    run(&["commit", "--allow-empty", "-qm", "older release"]);
    let older = run(&["rev-parse", "HEAD"]);
    fs::create_dir(root.join("td-mta")).unwrap();
    fs::write(root.join(POLICY_PATH), b"# none\n").unwrap();
    run(&["add", POLICY_PATH]);
    run(&["commit", "-qm", "policy"]);
    let live = run(&["rev-parse", "HEAD"]);
    // The working tree is not what the live release reviewed.
    fs::write(root.join(POLICY_PATH), b"[limits \"x\"]\n").unwrap();
    let local = Local {
        ssh_options: Vec::new(),
        host: Host::default(),
        root: root.into(),
        destination: "alice@192.0.2.10".into(),
        key: PathBuf::new(),
    };
    assert_eq!(policy(&local, &live).unwrap(), b"# none\n");
    assert!(policy(&local, &older)
        .unwrap_err()
        .to_string()
        .contains("deploy a release that has it first"));
    fs::write(root.join(POLICY_PATH), vec![b'#'; MAX_POLICY_BYTES + 1]).unwrap();
    run(&["commit", "-qam", "too large"]);
    let large = run(&["rev-parse", "HEAD"]);
    assert!(policy(&local, &large).is_err());
}

#[test]
fn convergence_is_its_own_job_and_plan_needs_it() {
    let one = ["--converge-config".to_owned()];
    let parsed = parse(&one).unwrap();
    assert!(parsed.converge && !parsed.plan);
    let two = ["--converge-config".to_owned(), "--plan".to_owned()];
    let planned = parse(&two).unwrap();
    assert!(planned.converge && planned.plan);
    let digest = "c".repeat(64);
    for refused in [
        &["--plan"][..],
        &["--expect-digest", digest.as_str()][..],
        &[
            "--converge-config",
            "--plan",
            "--expect-digest",
            digest.as_str(),
        ][..],
        &["--converge-config", "--expect-digest", "C"][..],
        &["--converge-config", "--skip-tests"][..],
        &["--converge-config", "--postgres-socket"][..],
        &["--converge-config", "--postgres-binaries"][..],
        &["--converge-config", "--retire-rollback", "--irreversible"][..],
        &["--converge-config", "--irreversible"][..],
        &["--converge-config", "--window-seconds", "60"][..],
        &["--converge-config", "--recover"][..],
        &["--converge-config", "--rollback"][..],
        &["--recover", "--converge-config"][..],
        &["--rollback", "--plan"][..],
        &["--recover", "--plan"][..],
        &["--recover", "--expect-digest", digest.as_str()][..],
        &["--rollback", "--expect-digest", digest.as_str()][..],
        &["--converge-config", "--to-debian"][..],
    ] {
        let args: Vec<String> = refused.iter().map(|s| s.to_string()).collect();
        assert!(parse(&args).is_err(), "{refused:?}");
    }
    for own_job in [
        &["--converge-config", "--postgres-binaries"][..],
        &["--converge-config", "--irreversible"][..],
        &["--converge-config", "--window-seconds", "60"][..],
        &["--converge-config", "--recover"][..],
        &["--converge-config", "--rollback"][..],
    ] {
        let args: Vec<String> = own_job.iter().map(|s| s.to_string()).collect();
        let error = parse(&args).err().unwrap().to_string();
        assert!(error.starts_with("configuration convergence"), "{error}");
    }
}

#[test]
fn the_reviewed_policy_parses_and_names_only_production_profiles() {
    let policy = include_bytes!("../../../../production-overrides.conf");
    assert!(policy.len() <= MAX_POLICY_BYTES);
    let policy = td_mta::config::converge::Policy::parse(policy).unwrap();
    policy.check_profiles(crate::activate::PROFILES).unwrap();
}

#[test]
fn a_job_refused_before_its_receipt_reports_why_and_the_lock_is_busy() {
    let locked = format!("refused:{}", crate::LOCKED);
    let (busy, failed) = finished(Some(&locked), Job::Failed, Ok(1.0));
    assert!(failed);
    assert_eq!(result(&busy), "busy");
    assert_eq!(
        busy.get("error").and_then(Json::as_str),
        Some(crate::LOCKED)
    );
    assert_eq!(
        busy.get("next").unwrap().get("argv").unwrap().to_string(),
        r#"["td-mta/deploy","--converge-config","--plan","--config","/t.json"]"#
    );
    let (refused, failed) = finished(
        Some("refused:job operation does not match the requested mode"),
        Job::Failed,
        Ok(1.0),
    );
    assert!(failed);
    assert_eq!(result(&refused), "refused");
    assert_eq!(
        refused.get("error").and_then(Json::as_str),
        Some("job operation does not match the requested mode")
    );
    assert!(refused.get("next").unwrap().is_null());
}
