#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
use super::*;
use std::os::unix::fs::{FileTypeExt, MetadataExt};

fn config() -> Config {
    Config::parse(&["--user", "tester", "--uid", "1000", "--peer-uid", "993"].map(String::from))
        .unwrap()
}

#[test]
fn only_canonical_disjoint_session_identities_are_configurable() {
    let valid = ["--user", "tester", "--uid", "1000", "--peer-uid", "993"].map(String::from);
    assert_eq!(config().peer_uid(), 993);
    for name in ["123".to_string(), "a".repeat(32)] {
        let mut args = valid.clone();
        args[1] = name;
        assert!(Config::parse(&args).is_ok());
    }
    let mut too_long = valid.clone();
    too_long[1] = "a".repeat(33);
    assert!(Config::parse(&too_long).is_err());
    for (index, values) in [
        (1, vec!["", "../root", "root:0", "A", "a b", "-tester"]),
        (
            3,
            vec!["0", "999", "01000", "+1000", "65534", "4294967296", "1001"],
        ),
        (5, vec!["0", "1000", "0993", "+993", "65534"]),
    ] {
        for value in values {
            let mut args = valid.clone();
            args[index] = value.into();
            assert!(Config::parse(&args).is_err(), "{args:?}");
        }
    }
    assert!(Config::parse(&valid[..5]).is_err());
    let mut extra = valid.to_vec();
    extra.push("ignored".into());
    assert!(Config::parse(&extra).is_err());
}

#[test]
fn the_caller_can_only_start_poll_or_keep_the_channel_alive() {
    assert_eq!(request(&[1]).unwrap(), Request::Start);
    assert_eq!(request(&[3]).unwrap(), Request::Heartbeat);
    let mut poll = vec![2];
    poll.extend_from_slice(&17u64.to_be_bytes());
    assert_eq!(request(&poll).unwrap(), Request::Poll(17));
    for bytes in [
        vec![],
        vec![0],
        vec![1, 0],
        vec![3, 0],
        vec![2],
        vec![2, 0],
        vec![2; 10],
        b"/bin/sh".to_vec(),
    ] {
        assert!(request(&bytes).is_err(), "{bytes:?}");
    }
    let mut zero = vec![2];
    zero.extend_from_slice(&0u64.to_be_bytes());
    assert!(request(&zero).is_err());
}

#[test]
fn fixed_commands_select_the_account_and_all_terminal_arguments() {
    let config = config();
    let check = config.checker();
    assert_eq!(check.get_program(), "/bin/td-firstboot");
    assert_eq!(
        check.get_args().collect::<Vec<_>>(),
        ["check-launch-session", "tester", "1000", "993"]
    );
    let terminal = config.terminal("000102030405060708090a0b0c0d0e0f", 17);
    assert_eq!(terminal.get_program(), "/bin/td-login");
    assert_eq!(
        terminal.get_args().collect::<Vec<_>>(),
        [
            "exec-as",
            "tester",
            "--",
            "/bin/td-authd",
            "terminal-exec",
            "1000",
            "000102030405060708090a0b0c0d0e0f",
            "17",
        ]
    );
    let terminal = terminal_command(1000, "000102030405060708090a0b0c0d0e0f", 17);
    assert_eq!(terminal.get_program(), "/bin/td-term");
    assert_eq!(
        terminal.get_envs().collect::<Vec<_>>(),
        [(
            std::ffi::OsStr::new("TD_CONTROL_SOCKET"),
            Some(std::ffi::OsStr::new("/run/td-compositor/1000/td-control")),
        )]
    );
    assert_eq!(
        terminal.get_args().collect::<Vec<_>>(),
        [
            "run",
            "--socket",
            "/run/td-compositor/1000/wayland-0",
            "--ready-socket",
            "/run/user/1000/td-auth-terminal-000102030405060708090a0b0c0d0e0f-17.ready",
        ]
    );
}

fn probe_command() -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.args([
        "--exact",
        "launch::tests::spawn_probe",
        "--nocapture",
        "--ignored",
    ]);
    command.env("TD_SHOULD_NOT_REACH_CHILD", "secret");
    command
}

fn probe_diagnostics() -> String {
    // Child stderr must stay null for this probe. Report the parent's ambient
    // inheritable descriptors without reading their contents or environment.
    let mut descriptors = Vec::new();
    if let Ok(entries) = std::fs::read_dir("/proc/self/fd") {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(fd) = name.to_str().and_then(|name| name.parse::<u32>().ok()) else {
                continue;
            };
            if fd < 3 {
                continue;
            }
            let flags = std::fs::read_to_string(format!("/proc/self/fdinfo/{fd}"))
                .ok()
                .and_then(|info| info.lines().find_map(|line| {
                    line.strip_prefix("flags:")
                        .and_then(|value| u32::from_str_radix(value.trim(), 8).ok())
                }));
            if flags.is_some_and(|flags| flags & 0o2000000 == 0) {
                descriptors.push((fd, std::fs::read_link(entry.path())));
            }
        }
    }
    descriptors.sort_by_key(|(fd, _)| *fd);
    format!("parent inheritable descriptors: {descriptors:?}; /dev/null: {:?}",
        std::fs::metadata("/dev/null").map(|m| (m.file_type().is_char_device(), m.rdev())))
}

fn wait_for_probe(child: &mut std::process::Child) {
    let status = child.wait().unwrap();
    assert!(status.success(), "launch probe exited {status}; {}", probe_diagnostics());
}

#[test]
#[ignore = "exec-only fixture: requires sanitized standard descriptors"]
fn spawn_probe() {
    assert!(std::env::vars_os().next().is_none());
    assert_eq!(std::env::current_dir().unwrap(), std::path::Path::new("/"));
    for fd in [0, 1, 2] {
        let metadata = std::fs::metadata(format!("/proc/self/fd/{fd}")).unwrap();
        assert!(metadata.file_type().is_char_device());
        assert_eq!(metadata.rdev(), 0x103, "fd {fd} is not /dev/null");
    }
    let mut descriptors: Vec<u32> = std::fs::read_dir("/proc/self/fd")
        .unwrap()
        .map(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_str()
                .unwrap()
                .parse()
                .unwrap()
        })
        .collect();
    descriptors.sort_unstable();
    assert_eq!(descriptors, [0, 1, 2, 3]);
}

#[test]
fn real_exec_replaces_stdio_discards_environment_and_has_no_extra_descriptors() {
    let mut child = spawn(&mut probe_command()).unwrap();
    wait_for_probe(&mut child);
}

#[test]
fn a_polled_completion_retires_its_handle_and_unknown_handles_fail() {
    let mut launches = Launches::new("000102030405060708090a0b0c0d0e0f".into());
    launches
        .children
        .insert(1, spawn(&mut probe_command()).unwrap());
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let response = launches.answer(&config(), Request::Poll(1)).unwrap();
        if response == [0x82, 1] {
            break;
        }
        assert_eq!(response, [0x82, 0], "launch probe failed; {}", probe_diagnostics());
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
    }
    assert!(launches.answer(&config(), Request::Poll(1)).is_err());
    assert_eq!(
        launches.answer(&config(), Request::Heartbeat).unwrap(),
        [0x83]
    );
    assert!(launches.children.is_empty());
}

#[test]
fn capacity_counts_unacknowledged_completions_and_cannot_spawn_over_the_limit() {
    let mut launches = Launches::new("000102030405060708090a0b0c0d0e0f".into());
    for handle in 1..=LIMIT as u64 {
        let mut child = spawn(&mut probe_command()).unwrap();
        wait_for_probe(&mut child);
        launches.children.insert(handle, child);
    }
    assert_eq!(
        launches.answer(&config(), Request::Start).unwrap(),
        [0xff, 1]
    );
    assert_eq!(launches.next, 1);
}

#[test]
fn handle_exhaustion_fails_before_a_child_can_start() {
    let mut launches = Launches::new("000102030405060708090a0b0c0d0e0f".into());
    launches.next = u64::MAX;
    assert!(launches
        .answer(&config(), Request::Start)
        .unwrap_err()
        .contains("exhausted"));
    assert!(launches.children.is_empty());
}

#[test]
fn a_missing_standard_fd_cannot_hide_an_inherited_fd_as_the_iterator() {
    for missing in [0, 1, 2] {
        let enumerated = std::cell::Cell::new(false);
        let result = audit_descriptors(
            |fd| {
                if fd == missing {
                    Err("closed standard descriptor".into())
                } else {
                    Ok((1, fd as u64))
                }
            },
            || {
                enumerated.set(true);
                Ok(vec![0, 1, 2, 3])
            },
        );
        assert!(result.is_err());
        assert!(
            !enumerated.get(),
            "the iterator could reuse the missing standard fd"
        );
    }
    assert!(audit_descriptors(|fd| Ok((1, fd as u64)), || Ok(vec![0, 1, 2, 3])).is_ok());
    assert!(audit_descriptors(|fd| Ok((1, fd as u64)), || Ok(vec![0, 1, 2, 3, 4])).is_err());
}

#[test]
fn the_unprivileged_exec_requires_the_exact_placed_session() {
    let status = "Uid:\t1000\t1000\t1000\t1000\nGid:\t1000\t1000\t1000\t1000\n";
    assert!(require_session_process(1000, status, "0::/td-user-1000/session\n").is_ok());
    for cgroup in [
        "",
        "0::/\n",
        "0::/td-svc/pair\n",
        "0::/td-user-1001/session\n",
        "0::/td-user-1000/session-extra\n",
        "0::/td-user-1000/session\n1:cpu:/\n",
    ] {
        assert!(require_session_process(1000, status, cgroup).is_err());
    }
    assert!(require_session_process(
        1000,
        &status.replace("1000", "0"),
        "0::/td-user-1000/session\n"
    )
    .is_err());
    assert!(require_session_process(
        1000,
        &status.replace("1000\n", "0\n"),
        "0::/td-user-1000/session\n"
    )
    .is_err());
}

#[test]
fn readiness_names_include_fresh_generations_and_fit_unix_socket_bounds() {
    let a = generation().unwrap();
    let b = generation().unwrap();
    assert_ne!(a, b);
    assert_eq!(a.len(), 32);
    assert_eq!(b.len(), 32);
    let first = terminal_command(1000, &a, u64::MAX);
    let second = terminal_command(1000, &b, u64::MAX);
    assert_ne!(first.get_args().last(), second.get_args().last());
    assert!(first.get_args().last().unwrap().len() < 108);
}

#[test]
fn root_startup_checks_every_credential_column_and_single_threadedness() {
    let status = "Uid:\t0\t0\t0\t0\nGid:\t0\t0\t0\t0\nThreads:\t1\n";
    assert!(require_root_status(status).is_ok());
    for bad in [
        status.replace("Uid:", "Other:"),
        status.replace("Gid:", "Other:"),
        status.replace("Threads:", "Other:"),
        status.replace("\t1\n", "\t2\n"),
        status.repeat(400),
    ] {
        assert!(require_root_status(&bad).is_err());
    }
    for kind in ["Uid:", "Gid:"] {
        for column in 0..4 {
            let mut values = ["0"; 4];
            values[column] = "1000";
            let bad = status.replace(
                &format!("{kind}\t0\t0\t0\t0"),
                &format!("{kind}\t{}", values.join("\t")),
            );
            assert!(require_root_status(&bad).is_err());
        }
    }
}

#[test]
fn a_late_validator_success_is_refused_even_after_reaping() {
    let mut child = spawn(&mut probe_command()).unwrap();
    wait_for_probe(&mut child);
    assert!(wait_check(&mut child, Instant::now())
        .unwrap_err()
        .contains("timed out"));
    assert!(CHECK_TIMEOUT < crate::channel::TIMEOUT);
}

#[test]
#[ignore = "exec-only delayed validator fixture"]
fn delayed_validator_probe() {
    thread::sleep(Duration::from_secs(10));
}

#[test]
fn a_validator_timeout_kills_and_reaps_the_owned_child() {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.args([
        "--exact",
        "launch::tests::delayed_validator_probe",
        "--nocapture",
        "--ignored",
    ]);
    let mut child = spawn(&mut command).unwrap();
    let start = Instant::now();
    assert!(wait_check(&mut child, start + Duration::from_millis(50))
        .unwrap_err()
        .contains("timed out"));
    assert!(start.elapsed() < Duration::from_secs(2));
    assert!(child.try_wait().unwrap().is_some());
}

#[test]
fn a_private_stdin_does_not_excuse_an_inherited_controlling_terminal() {
    assert!(require_no_terminal("12 (td-authd) S 1 12 12 0 0\n").is_ok());
    assert!(require_no_terminal("12 (td-authd) S 1 12 12 1025 0\n").is_err());
    assert!(require_no_terminal("12 (name) containing) S 1 12 12 0 0\n").is_ok());
    assert!(require_no_terminal("12 (td-authd) S 1 12 12\n").is_err());
}

#[test]
fn log_descriptors_must_not_alias_the_private_channel() {
    for alias in [1, 2] {
        assert!(audit_descriptors(
            |fd| Ok((1, if fd == alias { 0 } else { fd as u64 })),
            || Ok(vec![0, 1, 2, 3]),
        )
        .is_err());
    }
    assert!(audit_descriptors(
        |fd| Ok((1, if fd == 0 { 10 } else { 20 })),
        || Ok(vec![0, 1, 2, 3]),
    )
    .is_ok());
}
