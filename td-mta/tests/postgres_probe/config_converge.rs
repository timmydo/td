//! `postgres config-converge` against a real selected configuration: plan,
//! refusals, publication, idempotent retry, selection and rollback.
use super::{deadline, sql, Fixture, ProbeChild, PASSWORD};
use std::{
    fs::{self, File},
    io::Write,
    path::Path,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};
use td_json::Json;
use td_mta::{
    ids::StoreEpoch, postgres_configuration_store as versions, postgres_probe::Request,
    postgres_selection as selection,
};

fn converge(
    fixture: &Fixture,
    connection: &Path,
    epoch: StoreEpoch,
    version: Option<versions::Version>,
    policy: &[u8],
    expected: i32,
) -> Json {
    let output_path = fixture.root.join("converge-output");
    let output = File::create(&output_path).unwrap();
    run(connection, epoch, version, policy, expected, output);
    let text = fs::read_to_string(output_path).unwrap();
    assert!(text.len() < 64 * 1024);
    assert!(!text.contains(std::str::from_utf8(PASSWORD).unwrap()));
    assert!(!text.contains(connection.to_str().unwrap()));
    assert!(!text.contains("relay-user"), "{text}");
    td_json::parse(text.trim()).unwrap()
}

fn run(
    connection: &Path,
    epoch: StoreEpoch,
    version: Option<versions::Version>,
    policy: &[u8],
    expected: i32,
    output: File,
) {
    let epoch = epoch.to_string();
    let version = version.map(|v| v.to_string());
    let mut arguments = vec![
        "postgres",
        "config-converge",
        "--connection-file",
        connection.to_str().unwrap(),
        "--epoch",
        &epoch,
    ];
    match &version {
        Some(version) => arguments.extend(["--version", version.as_str()]),
        None => arguments.push("--plan-only"),
    }
    let mut child = ProbeChild(
        Command::new(env!("CARGO_BIN_EXE_td-mta"))
            .env_clear()
            .args(&arguments)
            .stdin(Stdio::piped())
            .stdout(output.try_clone().unwrap())
            .stderr(output)
            .spawn()
            .unwrap(),
    );
    let mut input = child.0.stdin.take().unwrap();
    input.write_all(policy).unwrap();
    drop(input);
    let stop = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            assert_eq!(status.code(), Some(expected));
            break;
        }
        assert!(Instant::now() < stop, "config-converge timed out");
        thread::sleep(Duration::from_millis(10));
    }
}

fn changes(report: &Json) -> Vec<String> {
    report
        .get("changes")
        .and_then(Json::as_arr)
        .unwrap()
        .iter()
        .map(|c| {
            format!(
                "{}.{}{}.{} {}->{}",
                c.get("profile").and_then(Json::as_str).unwrap(),
                c.get("section").and_then(Json::as_str).unwrap(),
                c.get("listener")
                    .and_then(Json::as_str)
                    .map(|l| format!("[{l}]"))
                    .unwrap_or_default(),
                c.get("key").and_then(Json::as_str).unwrap(),
                c.get("before").unwrap(),
                c.get("after").unwrap(),
            )
        })
        .collect()
}

fn text<'a>(report: &'a Json, key: &str) -> Option<&'a str> {
    report.get(key).and_then(Json::as_str)
}

fn flag(report: &Json, key: &str) -> Option<bool> {
    report.get(key).and_then(Json::as_bool)
}

fn select(request: &Request<'_>, epoch: StoreEpoch, generation: u64, version: versions::Version) {
    let candidate = versions::read(request, deadline(), epoch, version).unwrap();
    let change = selection::prepare(epoch, generation, version, &candidate).unwrap();
    let selected = selection::publish(request, deadline(), change).unwrap();
    assert_eq!(selected.generation, generation + 1);
}

pub(super) fn exercise(
    fixture: &Fixture,
    request: &Request<'_>,
    epoch: StoreEpoch,
    connection: &Path,
    original: versions::Version,
) {
    let count = || sql(fixture, "SELECT count(*) FROM td_mta.config_versions");
    let before = count();
    let policy = b"# Reviewed differences.\n[limits \"desktop\"]\nevent_streams = 4\n";
    // A plan validates and reports without publishing.
    let plan = converge(fixture, connection, epoch, None, policy, 0);
    assert_eq!(text(&plan, "status"), Some("ok"));
    assert_eq!(flag(&plan, "changed"), Some(true));
    assert_eq!(flag(&plan, "published"), Some(false));
    assert_eq!(flag(&plan, "content_validated"), Some(true));
    assert_eq!(flag(&plan, "profile_activated"), Some(false));
    assert!(plan.get("version").unwrap().is_null());
    assert_eq!(plan.get("generation").and_then(Json::as_u64), Some(1));
    assert_eq!(
        text(&plan, "selected_version"),
        Some(original.to_string().as_str())
    );
    assert_eq!(
        changes(&plan),
        [
            "cloud.listener[smtp].session_limit 1->32",
            "cloud.listener[smtp].per_peer_limit 1->2",
            "desktop.limits.event_streams null->4",
        ]
    );
    let profiles = plan.get("profiles").and_then(Json::as_arr).unwrap();
    let desktop = profiles
        .iter()
        .find(|p| text(p, "name") == Some("desktop"))
        .unwrap();
    assert_eq!(desktop.get("event_streams").and_then(Json::as_u64), Some(4));
    assert!(
        desktop
            .get("unassigned_bytes")
            .and_then(Json::as_u64)
            .unwrap()
            >= 32 << 20
    );
    assert_eq!(count(), before);
    // Refusals name their cause, publish nothing and print no sources.
    for (policy, code) in [
        (&b"[limits \"absent\"]\n"[..], "policy-unknown-profile"),
        (
            b"[limits \"cloud\"]\nsmtp_sessions = 64\n",
            "message-buffer-headroom",
        ),
        (
            b"[limits \"cloud\"]\nhttps_connections = 64\n",
            "memory-budget",
        ),
        (b"[listener \"cloud\"]\n", "policy-syntax"),
        // Each profile's plan passes; the candidate's shared quotas differ.
        (
            b"[disk \"desktop\"]\nbody_bytes = 1048576\n",
            "source-or-material-validation",
        ),
    ] {
        let version = versions::Version::from_bytes([0x30; 16]);
        let refused = converge(fixture, connection, epoch, Some(version), policy, 1);
        assert_eq!(text(&refused, "status"), Some("error"));
        assert_eq!(text(&refused, "error"), Some(code));
    }
    assert_eq!(count(), before);
    // Publication stores the retained version and selects nothing; a retry
    // with the same version resolves to the same receipt.
    let converged = versions::Version::from_bytes([0x31; 16]);
    let published = converge(fixture, connection, epoch, Some(converged), policy, 0);
    assert_eq!(flag(&published, "published"), Some(true));
    assert_eq!(
        text(&published, "version"),
        Some(converged.to_string().as_str())
    );
    assert_eq!(text(&published, "digest"), text(&plan, "digest"));
    let retried = converge(fixture, connection, epoch, Some(converged), policy, 0);
    assert_eq!(text(&retried, "digest"), text(&plan, "digest"));
    assert_eq!(flag(&retried, "published"), Some(true));
    assert_eq!(text(&retried, "version"), text(&published, "version"));
    // A report lost after publication: the same ID and policy settle it.
    let lost = versions::Version::from_bytes([0x33; 16]);
    run(
        connection,
        epoch,
        Some(lost),
        policy,
        1,
        File::options().write(true).open("/dev/full").unwrap(),
    );
    // The version is durable although its report was lost.
    assert!(versions::read_sources(request, deadline(), epoch, lost).is_ok());
    let settled = converge(fixture, connection, epoch, Some(lost), policy, 0);
    assert_eq!(text(&settled, "version"), Some(lost.to_string().as_str()));
    assert_eq!(text(&settled, "digest"), text(&plan, "digest"));
    let syntax = converge(
        fixture,
        connection,
        epoch,
        None,
        b"\n[limits \"cloud\"]\nnone = 1\n",
        1,
    );
    assert_eq!(syntax.get("line").and_then(Json::as_u64), Some(3));
    let current = selection::observe(request, deadline(), epoch)
        .unwrap()
        .unwrap();
    assert_eq!((current.generation, current.version), (1, original));
    let sources = versions::read_sources(request, deadline(), epoch, converged).unwrap();
    let (_, stored) = sources.parts().unwrap();
    let source =
        |name| std::str::from_utf8(stored.iter().find(|p| p.name == name).unwrap().source).unwrap();
    assert!(source("cloud").contains("session_limit = 32\nper_peer_limit = 2\n"));
    assert!(source("desktop").ends_with("[limits]\nevent_streams = 4\n"));
    // Once selected, converging again changes and publishes nothing.
    select(request, epoch, 1, converged);
    let fresh = versions::Version::from_bytes([0x32; 16]);
    let settled = converge(fixture, connection, epoch, Some(fresh), policy, 0);
    assert_eq!(flag(&settled, "changed"), Some(false));
    assert_eq!(flag(&settled, "published"), Some(false));
    assert!(settled.get("version").unwrap().is_null());
    assert!(changes(&settled).is_empty());
    assert_eq!(settled.get("generation").and_then(Json::as_u64), Some(2));
    assert_eq!(text(&settled, "digest"), text(&plan, "digest"));
    assert_eq!(
        versions::read_sources(request, deadline(), epoch, fresh).err(),
        Some(versions::Error::Missing)
    );
    // The previous selection stays restorable, and convergence notices it.
    select(request, epoch, 2, original);
    let restored = converge(fixture, connection, epoch, None, policy, 0);
    assert_eq!(flag(&restored, "changed"), Some(true));
    assert_eq!(restored.get("generation").and_then(Json::as_u64), Some(3));
    assert_eq!(changes(&restored), changes(&plan));
}
