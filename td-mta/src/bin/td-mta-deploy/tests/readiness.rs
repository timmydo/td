#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
use super::*;

fn command(line: &str) -> Json {
    let args: Vec<_> = line.split_ascii_whitespace().map(Json::from).collect();
    Json::Arr(vec![
        args.first().unwrap().clone(),
        Json::Arr(args),
        Json::Arr(vec![]),
    ])
}

fn checks(rows: &[Json], legacy: bool) -> bool {
    check_commands(
        &Json::Obj(vec![("data".into(), Json::Arr(rows.to_vec()))]),
        legacy,
        HEALTH_CHECKS,
    )
    .is_ok()
}

#[test]
fn aggregate_preserves_effective_commands_and_refuses_drift() {
    let mut rows: Vec<_> = HEALTH_CHECKS
        .lines()
        .filter_map(|s| s.strip_prefix("ExecStart="))
        .filter(|s| !s.is_empty())
        .map(command)
        .collect();
    assert!(checks(&rows, false));
    assert!(!checks(&rows[1..], true));
    for profile in crate::activate::PROFILES {
        rows.insert(5, command(&format!("/opt/td-mta/bin/td-mta status --json --require-ready --runtime /var/lib/td-mta/mail/{profile}/runtime")));
    }
    assert!(checks(&rows, true));
    assert!(!checks(&rows, false));
    rows.push(command("/usr/bin/site-healthcheck"));
    assert!(!checks(&rows, true));
    rows.pop();
    if let Json::Arr(fields) = &mut rows[0] {
        fields[2] = Json::Arr(vec!["ignore-failure".into()]);
    }
    assert!(!checks(&rows, true));
}

#[test]
fn four_ordered_readiness_jobs_survive_identity_rollback() {
    let root = Path::new("/etc/systemd/system");
    let permanent = settings(root).unwrap();
    let reversible = crate::isolation::backup_files().unwrap();
    assert!(permanent.iter().all(|s| !reversible.contains(&s.path)));
    let (aggregate, jobs) = permanent.split_last().unwrap();
    assert_eq!(jobs.len(), 4);
    for key in ["Requires=", "After="] {
        let names: Vec<_> = aggregate
            .text
            .lines()
            .find_map(|l| l.strip_prefix(key))
            .unwrap()
            .split_ascii_whitespace()
            .collect();
        let units: Vec<_> = jobs
            .iter()
            .map(|s| s.path.file_name().unwrap().to_str().unwrap())
            .collect();
        assert_eq!(names, units);
    }
    for (role, job) in crate::activate::PROFILES.iter().zip(jobs) {
        assert!(job.text.contains("User=td-mta\nGroup=td-mta\n"));
        assert!(job.text.contains(&format!(
            "\"--runtime\" \"/var/lib/td-mta/mail/{role}/runtime\"\n"
        )));
        assert!(reversible.contains(&root.join(format!(
            "td-mta-maint-mail-{role}.service.d/95-td-mta-identity.conf"
        ))));
    }
}

#[test]
fn each_profile_must_run_its_exact_probe_with_literal_arguments() {
    let mut row = command("/opt/td-mta/bin/td-mta status --json --require-ready --runtime /var/lib/td-mta/mail/receive/runtime");
    let probe = |row: &Json, role| {
        check_job(
            &Json::Obj(vec![("data".into(), Json::Arr(vec![row.clone()]))]),
            role,
        )
        .is_ok()
    };
    assert!(!probe(&row, "receive"));
    if let Json::Arr(fields) = &mut row {
        fields[2] = Json::Arr(vec!["no-env-expand".into()]);
    }
    assert!(probe(&row, "receive"));
    assert!(!probe(&row, "relay"));
}
