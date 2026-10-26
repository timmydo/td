#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
use super::*;
use std::os::unix::fs::{FileTypeExt, MetadataExt};

fn primary(name: &str) -> crate::primary_account::PrimaryAccount {
    crate::primary_account::parse(&format!("{name}:x:1000:1000:human:/home/{name}:/bin/sh\n"))
        .unwrap()
}

fn config() -> Config {
    Config::parse(&["--user", "tester", "--uid", "1000", "--peer-uid", "993"].map(String::from))
        .unwrap()
}

#[test]
fn only_canonical_disjoint_session_identities_are_configurable() {
    let valid = ["--user", "tester", "--uid", "1000", "--peer-uid", "993"].map(String::from);
    assert_eq!(config().peer_uid(), 993);
    for name in ["a1_b-c".to_string(), "a".repeat(32)] {
        let mut args = valid.clone();
        args[1] = name;
        assert!(Config::parse(&args).is_ok());
    }
    let mut too_long = valid.clone();
    too_long[1] = "a".repeat(33);
    assert!(Config::parse(&too_long).is_err());
    for (index, values) in [
        // The primary account's rule, which request 1a's answers carry:
        // check-launch-session alone would admit the leading digit or
        // underscore.
        (
            1,
            vec![
                "", "../root", "root:0", "A", "a b", "-tester", "123", "_tester", "a.b",
            ],
        ),
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
fn primary_configuration_resolves_once_and_keeps_the_ledger_and_helper_checks() {
    let mut loads = 0;
    let config = Config::parse_with_primary(
        &["--primary", "--peer-uid", "993"].map(String::from),
        || {
            loads += 1;
            Ok(primary("alice"))
        },
    )
    .unwrap();
    assert_eq!(loads, 1);
    assert_eq!(config.peer_uid(), 993);
    assert_eq!(
        config.checker().get_args().collect::<Vec<_>>(),
        ["check-launch-session", "alice", "1000", "993"]
    );
    assert_eq!(
        config
            .terminal("generation", 1, &Launch::Fixed(Program::Home))
            .get_args()
            .collect::<Vec<_>>(),
        [
            "exec-as",
            "alice",
            "--",
            "/bin/td-authd",
            "terminal-exec",
            "1000",
            "generation",
            "1"
        ]
    );
    assert!(Config::parse_with_primary(
        &["--primary", "--peer-uid", "993"].map(String::from),
        || Err("invalid primary database".into()),
    )
    .is_err_and(|error| error == "invalid primary database"));
}

#[test]
fn malformed_primary_configuration_never_reads_the_account_database() {
    for args in [
        vec!["--user", "alice", "--uid"],
        vec!["arbitrary", "three", "arguments"],
        vec!["--primary"],
        vec!["--primary", "--peer-uid"],
        vec!["--primary", "--peer-uid", "0"],
        vec!["--primary", "--peer-uid", "1000"],
        vec!["--primary", "--peer-uid", "0993"],
        vec!["--primary", "--peer-uid", "+993"],
        vec!["--primary", "--peer-uid", "4294967296"],
        vec!["--primary", "--uid", "1000"],
        vec!["--primary", "--peer-uid", "993", "extra"],
        vec!["--primary", "--uid", "1000", "--peer-uid", "993"],
        vec!["--primary", "--user", "alice", "--peer-uid", "993"],
        vec![
            "--user",
            "alice",
            "--primary",
            "--uid",
            "1000",
            "--peer-uid",
            "993",
        ],
    ] {
        let mut loaded = false;
        let args: Vec<_> = args.into_iter().map(String::from).collect();
        assert!(Config::parse_with_primary(&args, || {
            loaded = true;
            Ok(primary("alice"))
        })
        .is_err());
        assert!(!loaded, "{args:?}");
    }
    assert!(Config::parse_with_primary(
        &["--user", "alice", "--uid", "1000", "--peer-uid", "993"].map(String::from),
        || Err("named configuration must not resolve a primary".into()),
    )
    .is_ok());
}

#[test]
fn the_caller_can_only_start_poll_or_keep_the_channel_alive() {
    assert_eq!(request(&[1]).unwrap(), Request::Start(Program::Home));
    assert_eq!(request(&[4]).unwrap(), Request::Start(Program::Task));
    assert_eq!(request(&[7]).unwrap(), Request::Start(Program::TaskManager));
    assert_eq!(request(&[8]).unwrap(), Request::Start(Program::Editor));
    assert_eq!(request(&[9]).unwrap(), Request::Start(Program::Photo));
    assert_eq!(request(&[0x0a]).unwrap(), Request::Start(Program::Review));
    assert_eq!(request(&[0x0b]).unwrap(), Request::Start(Program::Dua));
    assert_eq!(request(&[0x0c]).unwrap(), Request::Start(Program::Agent));
    assert_eq!(
        request(b"\x0dfirefox").unwrap(),
        Request::Supervised(Supervised::Start("firefox".into()))
    );
    assert_eq!(
        request(&[0x0e]).unwrap(),
        Request::Supervised(Supervised::Update)
    );
    assert_eq!(
        request(b"\x0ffirefox").unwrap(),
        Request::UnitState("firefox".into())
    );
    assert_eq!(
        request(b"\x24\x01firefox").unwrap(),
        Request::Software(crate::software::Request {
            verb: crate::application_cache::Verb::Install,
            name: "firefox".into(),
        })
    );
    assert_eq!(request(&[0x22]).unwrap(), Request::Start(Program::Software));
    assert_eq!(request(&[3]).unwrap(), Request::Heartbeat);
    let mut poll = vec![2];
    poll.extend_from_slice(&17u64.to_be_bytes());
    assert_eq!(request(&poll).unwrap(), Request::Poll(17));
    for bytes in [
        vec![],
        vec![0],
        vec![1, 0],
        vec![3, 0],
        vec![4, 0],
        vec![5],
        vec![5, 0],
        vec![6],
        vec![6, 0],
        vec![7, 0],
        vec![8, 0],
        vec![9, 0],
        vec![0x0a, 0],
        vec![0x0b, 0],
        vec![0x0c, 0],
        vec![0x0d],
        vec![0x0d, 0],
        b"\x0dFirefox".to_vec(),
        b"\x0d../firefox".to_vec(),
        vec![0x0e, 0],
        vec![0x0f],
        vec![0x0f, 0],
        b"\x0f-firefox".to_vec(),
        vec![0x24],
        vec![0x24, 1],
        b"\x24\x05firefox".to_vec(),
        b"\x24\x01/bin/sh".to_vec(),
        vec![0x22, 0],
        b"\x22Firefox".to_vec(),
        vec![0x23, 0],
        b"\x23firefox".to_vec(),
        vec![0x24],
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
    let terminal = config.terminal(
        "000102030405060708090a0b0c0d0e0f",
        17,
        &Launch::Fixed(Program::Home),
    );
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
    let terminal = terminal_command(
        1000,
        "000102030405060708090a0b0c0d0e0f",
        17,
        Program::Home,
        None,
        || Ok(primary("tester")),
    )
    .unwrap();
    assert_eq!(terminal.get_program(), "/bin/td-term");
    assert_eq!(
        terminal.get_envs().collect::<Vec<_>>(),
        [(
            std::ffi::OsStr::new("TD_CONTROL_SOCKET"),
            Some(std::ffi::OsStr::new("/run/td-compositor/1000/td-control")),
        )]
    );
    let task = config.terminal(
        "000102030405060708090a0b0c0d0e0f",
        18,
        &Launch::Fixed(Program::Task),
    );
    assert_eq!(task.get_args().last(), Some(std::ffi::OsStr::new("task")));
    let task = terminal_command(
        1000,
        "000102030405060708090a0b0c0d0e0f",
        18,
        Program::Task,
        None,
        || Ok(primary("tester")),
    )
    .unwrap();
    assert!(task.get_args().collect::<Vec<_>>().windows(2).any(|pair| {
        pair == [
            std::ffi::OsStr::new("--working-directory"),
            std::ffi::OsStr::new("/home/tester/src/td-vm/work"),
        ]
    }));
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
                .and_then(|info| {
                    info.lines().find_map(|line| {
                        line.strip_prefix("flags:")
                            .and_then(|value| u32::from_str_radix(value.trim(), 8).ok())
                    })
                });
            if flags.is_some_and(|flags| flags & 0o2000000 == 0) {
                descriptors.push((fd, std::fs::read_link(entry.path())));
            }
        }
    }
    descriptors.sort_by_key(|(fd, _)| *fd);
    format!(
        "parent inheritable descriptors: {descriptors:?}; /dev/null: {:?}",
        std::fs::metadata("/dev/null").map(|m| (m.file_type().is_char_device(), m.rdev()))
    )
}

fn wait_for_probe(child: &mut std::process::Child) {
    let status = child.wait().unwrap();
    assert!(
        status.success(),
        "launch probe exited {status}; {}",
        probe_diagnostics()
    );
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
        assert_eq!(
            response,
            [0x82, 0],
            "launch probe failed; {}",
            probe_diagnostics()
        );
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
        launches
            .answer(&config(), Request::Start(Program::Home))
            .unwrap(),
        [0xff, 1]
    );
    assert_eq!(launches.next, 1);
}

#[test]
fn handle_exhaustion_fails_before_a_child_can_start() {
    let mut launches = Launches::new("000102030405060708090a0b0c0d0e0f".into());
    launches.next = u64::MAX;
    assert!(launches
        .answer(&config(), Request::Start(Program::Home))
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
    let first = terminal_command(1000, &a, u64::MAX, Program::Home, None, || {
        Ok(primary("tester"))
    })
    .unwrap();
    let second = terminal_command(1000, &b, u64::MAX, Program::Home, None, || {
        Ok(primary("tester"))
    })
    .unwrap();
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
fn an_operation_audit_admits_aliases_and_only_standard_descriptors() {
    for missing in [0, 1, 2] {
        let enumerated = std::cell::Cell::new(false);
        let result = audit_operation_descriptors(
            |fd| {
                if fd == missing {
                    Err("closed standard descriptor".into())
                } else {
                    Ok((1, 0))
                }
            },
            || {
                enumerated.set(true);
                Ok(vec![0, 1, 2, 3])
            },
        );
        assert!(result.is_err());
        assert!(!enumerated.get(), "the iterator could reuse it");
    }
    // One file such as /dev/null on all three is admitted.
    assert!(audit_operation_descriptors(|_| Ok((1, 0)), || Ok(vec![0, 1, 2, 3])).is_ok());
    assert!(audit_operation_descriptors(|_| Ok((1, 0)), || Ok(vec![0, 1, 2, 3, 4])).is_err());
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

#[test]
fn task_manager_has_fixed_unprivileged_exec_and_display_only() {
    let wrapper = config().terminal(
        "000102030405060708090a0b0c0d0e0f",
        17,
        &Launch::Fixed(Program::TaskManager),
    );
    assert_eq!(wrapper.get_program(), "/bin/td-login");
    assert_eq!(wrapper.get_args().last().unwrap(), "taskmgr");
    let command = terminal_command(
        1000,
        "000102030405060708090a0b0c0d0e0f",
        17,
        Program::TaskManager,
        None,
        || Ok(primary("tester")),
    )
    .unwrap();
    assert_eq!(command.get_program(), "/bin/td-taskmgr");
    assert_eq!(command.get_args().count(), 0);
    assert_eq!(
        command.get_envs().collect::<Vec<_>>(),
        vec![(
            std::ffi::OsStr::new("WAYLAND_DISPLAY"),
            Some(std::ffi::OsStr::new("/run/td-compositor/1000/wayland-0"))
        )]
    );
}

/// `22` with a name opens td-software at that application: the name rides
/// the credential helper's argv after `software` and becomes td-software's
/// one `--select`, checked in the registry grammar at both ends; no other
/// program takes one.
#[test]
fn software_opens_at_an_application_named_in_the_registry_grammar() {
    assert_eq!(
        request(b"\x22firefox").unwrap(),
        Request::SoftwareAt("firefox".into())
    );
    assert_eq!(request(&[0x22]).unwrap(), Request::Start(Program::Software));
    for bad in [
        &b"\x22Firefox"[..],
        b"\x22../firefox",
        b"\x22fire fox",
        b"\x22-x",
        b"\x22\xff",
        &[0x22, 0],
    ] {
        assert!(request(bad).is_err(), "{bad:?}");
    }
    let long = [&[0x22][..], "a".repeat(65).as_bytes()].concat();
    assert!(request(&long).is_err());
    let wrapper = config().terminal(
        "000102030405060708090a0b0c0d0e0f",
        17,
        &Launch::SoftwareAt("firefox".into()),
    );
    let args: Vec<_> = wrapper.get_args().map(|a| a.to_str().unwrap()).collect();
    assert_eq!(
        args.get(args.len().saturating_sub(2)..),
        Some(&["software", "firefox"][..])
    );
    let longest = [&[0x22][..], "a".repeat(64).as_bytes()].concat();
    assert_eq!(
        request(&longest).unwrap(),
        Request::SoftwareAt("a".repeat(64))
    );
    let command = terminal_command(
        1000,
        "000102030405060708090a0b0c0d0e0f",
        17,
        Program::Software,
        Some("firefox"),
        || Err(std::io::Error::other("no account read")),
    )
    .unwrap();
    assert_eq!(command.get_program(), "/bin/td-software");
    assert_eq!(
        command.get_args().collect::<Vec<_>>(),
        ["--select", "firefox"]
    );
    for (program, name) in [
        (Program::Editor, "firefox"),
        (Program::Home, "firefox"),
        (Program::Software, "Firefox"),
        (Program::Software, "--help"),
    ] {
        assert!(
            terminal_command(
                1000,
                "000102030405060708090a0b0c0d0e0f",
                17,
                program,
                Some(name),
                || Err(std::io::Error::other("no account read")),
            )
            .is_err(),
            "{program:?} {name}"
        );
    }
}

/// terminal-exec takes an application after `software` alone, in the
/// registry grammar. Only its arguments are read here: a good form would
/// go on to exec in a test process that passes the session check.
#[test]
fn terminal_exec_takes_an_application_only_for_td_software() {
    let generation = "000102030405060708090a0b0c0d0e0f";
    let arguments = |tail: &[&str]| {
        let mut arguments = vec!["1000".to_string(), generation.into(), "1".into()];
        arguments.extend(tail.iter().map(|word| word.to_string()));
        arguments
    };
    for bad in [
        &["editor", "firefox"][..],
        &["software", "Firefox"],
        &["software", "firefox", "mail"],
        &["software", ""],
    ] {
        let arguments = arguments(bad);
        assert!(terminal_arguments(&arguments)
            .unwrap_err()
            .contains("terminal-exec requires"));
        assert!(terminal_exec(&arguments)
            .unwrap_err()
            .contains("terminal-exec requires"));
    }
    let selected = arguments(&["software", "firefox"]);
    assert_eq!(
        terminal_arguments(&selected).unwrap(),
        (1000, generation, 1, Program::Software, Some("firefox"))
    );
    let plain = arguments(&["software"]);
    assert_eq!(
        terminal_arguments(&plain).unwrap(),
        (1000, generation, 1, Program::Software, None)
    );
}

/// td-software starts outside the home, as the task manager does, and is
/// told only where the compositor's control socket is.
#[test]
fn software_has_a_fixed_exec_the_display_and_the_control_socket() {
    let wrapper = config().terminal(
        "000102030405060708090a0b0c0d0e0f",
        17,
        &Launch::Fixed(Program::Software),
    );
    assert_eq!(wrapper.get_program(), "/bin/td-login");
    assert_eq!(wrapper.get_args().last().unwrap(), "software");
    let command = terminal_command(
        1000,
        "000102030405060708090a0b0c0d0e0f",
        17,
        Program::Software,
        None,
        || Err(std::io::Error::other("no account read")),
    )
    .unwrap();
    assert_eq!(command.get_program(), "/bin/td-software");
    assert_eq!(command.get_args().count(), 0);
    assert_eq!(command.get_current_dir(), None);
    let os = std::ffi::OsStr::new;
    assert_eq!(
        command.get_envs().collect::<Vec<_>>(),
        vec![
            (
                os("TD_CONTROL_SOCKET"),
                Some(os("/run/td-compositor/1000/td-control"))
            ),
            (
                os("WAYLAND_DISPLAY"),
                Some(os("/run/td-compositor/1000/wayland-0"))
            ),
        ]
    );
}

#[test]
fn every_selection_literal_names_its_program_back() {
    for program in [
        Program::Task,
        Program::TaskManager,
        Program::Editor,
        Program::Photo,
        Program::Review,
        Program::Dua,
        Program::Agent,
        Program::Software,
    ] {
        let literal = program.selection().unwrap();
        assert_eq!(Program::from_selection(literal), Some(program), "{literal}");
    }
    assert_eq!(Program::Home.selection(), None);
    for literal in [
        "",
        "home",
        "codex",
        "claude",
        "Review",
        "/bin/td-review",
        "review ",
        "td-dua",
        "td-agent",
        "td-software",
    ] {
        assert_eq!(Program::from_selection(literal), None, "{literal:?}");
    }
}

#[test]
fn desktop_programs_exec_fixed_binaries_from_the_validated_home() {
    let none: &[&str] = &[];
    for (program, selection, binary, expected) in [
        (Program::Editor, "editor", "/bin/td-editor", none),
        (Program::Photo, "photo", "/bin/td-photo", none),
        (
            Program::Review,
            "review",
            "/bin/td-review",
            &["--choose-repo"][..],
        ),
        (Program::Dua, "dua", "/bin/td-dua", none),
        (Program::Agent, "agent", "/bin/td-agent", none),
    ] {
        let wrapper = config().terminal(
            "000102030405060708090a0b0c0d0e0f",
            17,
            &Launch::Fixed(program),
        );
        assert_eq!(wrapper.get_program(), "/bin/td-login");
        assert_eq!(wrapper.get_args().last().unwrap(), selection);
        for name in ["alice", "bob"] {
            let command = terminal_command(
                1000,
                "000102030405060708090a0b0c0d0e0f",
                17,
                program,
                None,
                || Ok(primary(name)),
            )
            .unwrap();
            assert_eq!(command.get_program(), binary);
            let home = format!("/home/{name}");
            assert_eq!(command.get_current_dir(), Some(std::path::Path::new(&home)));
            let arguments: Vec<_> = command.get_args().collect();
            assert_eq!(arguments, expected, "{binary}");
            let os = std::ffi::OsStr::new;
            let mut envs = vec![(
                os("WAYLAND_DISPLAY"),
                Some(os("/run/td-compositor/1000/wayland-0")),
            )];
            // td-agent alone: its runtime directory, and the jail and
            // td-txt its tools run through.
            if program == Program::Agent {
                envs = vec![
                    (os("TD_AGENT_JAIL"), Some(os("/bin/td-jail"))),
                    (os("TD_AGENT_TXT"), Some(os("/bin/td-txt"))),
                    envs[0],
                    (os("XDG_RUNTIME_DIR"), Some(os("/run/user/1000"))),
                ];
            }
            assert_eq!(command.get_envs().collect::<Vec<_>>(), envs, "{binary}");
        }
    }
}

#[test]
fn the_task_directory_follows_the_validated_primary_account() {
    for name in ["alice", "bob"] {
        let command = terminal_command(
            1000,
            "000102030405060708090a0b0c0d0e0f",
            1,
            Program::Task,
            None,
            || Ok(primary(name)),
        )
        .unwrap();
        let expected = format!("/home/{name}/src/td-vm/work");
        assert!(command
            .get_args()
            .collect::<Vec<_>>()
            .windows(2)
            .any(|pair| {
                pair == [
                    std::ffi::OsStr::new("--working-directory"),
                    std::ffi::OsStr::new(&expected),
                ]
            }));
    }
}

#[test]
fn only_launches_placed_in_the_account_require_the_primary_account() {
    for program in [
        Program::Home,
        Program::TaskManager,
        Program::Task,
        Program::Editor,
        Program::Photo,
        Program::Review,
        Program::Dua,
        Program::Agent,
        Program::Software,
    ] {
        let mut loaded = false;
        let result = terminal_command(
            1000,
            "000102030405060708090a0b0c0d0e0f",
            1,
            program,
            None,
            || {
                loaded = true;
                Err(std::io::Error::other("invalid primary account"))
            },
        );
        let requires_account = !matches!(
            program,
            Program::Home | Program::TaskManager | Program::Software
        );
        assert_eq!(loaded, requires_account);
        assert_eq!(result.is_err(), requires_account);
    }
}

#[test]
fn only_td_svcs_exact_status_line_for_the_unit_names_a_state() {
    use UnitState::*;
    for (line, state) in [
        ("firefox ready pid=17 failures=0\n", Running),
        ("firefox ready pid=- failures=0\n", Idle),
        ("firefox skipped pid=- failures=0\n", Idle),
        ("firefox stopped pid=- failures=0\n", Idle),
        ("firefox down pid=- failures=0\n", Starting),
        ("firefox starting pid=17 failures=1\n", Starting),
        (
            "firefox stopping pid=- failures=0 leaf=unproven\n",
            Starting,
        ),
        ("firefox failed pid=- failures=2\n", Failed),
        ("firefox held pid=- failures=9\n", Failed),
    ] {
        assert_eq!(unit_state(line, "firefox"), Some(state), "{line:?}");
    }
    for line in [
        "",
        "firefox ready pid=17 failures=0",
        "firefox ready pid=17 failures=0\n\n",
        "firefox ready pid=17 failures=0\nfirefox ready pid=17 failures=0\n",
        "firefox-update ready pid=17 failures=0\n",
        "mail ready pid=17 failures=0\n",
        "firefox  ready pid=17 failures=0\n",
        "firefox ready pid= failures=0\n",
        "firefox ready pid=1x failures=0\n",
        "firefox ready pid=17 failures=\n",
        "firefox ready pid=17\n",
        "firefox ready pid=17 failures=0 leaf=proven\n",
        "firefox ready pid=17 failures=0 leaf=unproven\n",
        "firefox failed pid=- failures=0 leaf=unproven\n",
        "firefox ready pid=17 failures=0 leaf=unproven x\n",
        "firefox running pid=17 failures=0\n",
        "error: unknown service firefox\n",
    ] {
        assert_eq!(unit_state(line, "firefox"), None, "{line:?}");
    }
}

fn status_reply(line: &'static [u8]) -> Result<crate::inspection::Inspection, String> {
    crate::inspection::tests::printed(line, 0, false, STATE_REPLY, STATE_TIME)
}

fn running(_: &str) -> Result<crate::inspection::Inspection, String> {
    status_reply(b"firefox ready pid=17 failures=0\n")
}

fn idle(_: &str) -> Result<crate::inspection::Inspection, String> {
    status_reply(b"firefox ready pid=- failures=0\n")
}

fn busy(_: &str) -> Result<crate::inspection::Inspection, String> {
    crate::inspection::tests::printed(
        b"error: supervisor did not answer in time\n",
        1,
        false,
        STATE_REPLY,
        STATE_TIME,
    )
}

/// Asks `0f` until the answer is not `from`, or fails after five seconds.
fn answer_after(probe: &mut UnitProbe, from: UnitState) -> UnitState {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let state = probe.answer("firefox", false);
        if state != from {
            return state;
        }
        assert!(Instant::now() < deadline, "still {from:?}");
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn the_unit_state_is_read_by_one_bounded_client_at_a_time() {
    let mut probe = UnitProbe::new(running);
    assert_eq!(probe.answer("firefox", false), UnitState::Unknown);
    assert!(probe.client.is_some());
    assert_eq!(
        answer_after(&mut probe, UnitState::Unknown),
        UnitState::Running
    );
    // Within the interval the last read answers and no client starts.
    assert!(probe.client.is_none());
    probe.began = Some(Instant::now());
    assert_eq!(probe.answer("firefox", false), UnitState::Running);
    assert!(probe.client.is_none());
    // A start request in flight answers starting and starts no client,
    // and a start does not let one start sooner once it is over.
    probe.restart("firefox");
    assert_eq!(probe.answer("firefox", true), UnitState::Starting);
    assert!(probe.client.is_none());
    assert_eq!(probe.answer("firefox", false), UnitState::Starting);
    assert!(probe.client.is_none());
}

#[test]
fn a_read_that_began_before_a_start_request_cannot_answer_for_it() {
    let mut probe = UnitProbe::new(idle);
    assert_eq!(probe.answer("firefox", false), UnitState::Unknown);
    probe.restart("firefox");
    probe.status = running;
    // The idle client is drained and discarded, then a fresh one reads;
    // until it does, the start is starting.
    assert_eq!(
        answer_after(&mut probe, UnitState::Starting),
        UnitState::Running
    );
}

#[test]
fn a_failed_start_request_stands_until_the_unit_is_seen_going_up() {
    let mut probe = UnitProbe::new(idle);
    probe.restart("firefox");
    probe.start_failed = true;
    assert_eq!(probe.answer("firefox", false), UnitState::Failed);
    let deadline = Instant::now() + Duration::from_secs(5);
    while probe.last.is_none() {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
        assert_eq!(probe.answer("firefox", false), UnitState::Failed);
    }
    assert_eq!(probe.last, Some(UnitState::Idle));
    probe.status = running;
    probe.began = None;
    assert_eq!(
        answer_after(&mut probe, UnitState::Failed),
        UnitState::Running
    );
    assert!(!probe.start_failed);
}

#[test]
fn a_busy_or_failed_client_leaves_the_last_read_standing() {
    let mut probe = UnitProbe::new(running);
    probe.answer("firefox", false);
    assert_eq!(
        answer_after(&mut probe, UnitState::Unknown),
        UnitState::Running
    );
    probe.status = busy;
    probe.began = None;
    assert_eq!(probe.answer("firefox", false), UnitState::Running);
    let deadline = Instant::now() + Duration::from_secs(5);
    while probe.client.is_some() {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
        probe.advance();
    }
    assert_eq!(probe.last, Some(UnitState::Running));
}

fn late(_: &str) -> Result<crate::inspection::Inspection, String> {
    crate::inspection::tests::printed(
        b"firefox failed pid=- failures=1\n",
        0,
        true,
        STATE_REPLY,
        Duration::from_millis(50),
    )
}

#[test]
fn a_client_past_its_bound_is_killed_and_answers_nothing() {
    let mut probe = UnitProbe::new(running);
    probe.answer("firefox", false);
    assert_eq!(
        answer_after(&mut probe, UnitState::Unknown),
        UnitState::Running
    );
    probe.status = late;
    probe.began = None;
    probe.answer("firefox", false);
    let deadline = Instant::now() + Duration::from_secs(5);
    while probe.client.is_some() {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
        probe.advance();
    }
    assert_eq!(probe.last, Some(UnitState::Running));
}

#[test]
#[ignore = "exec-only failing supervised client fixture"]
fn failing_supervised_probe() {
    std::process::exit(1);
}

fn supervised_child(test: &str) -> Child {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.args(["--exact", test, "--ignored"]);
    spawn(&mut command).unwrap()
}

/// Heartbeats until no supervised client is left.
fn reap_all(launches: &mut Launches) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !launches.supervised.is_empty() {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
        assert_eq!(
            launches.answer(&config(), Request::Heartbeat).unwrap(),
            [0x83]
        );
    }
}

#[test]
fn a_start_request_that_failed_reads_failed_and_an_update_does_not() {
    let generation = "000102030405060708090a0b0c0d0e0f".to_string();
    let mut launches = Launches::with_status(generation.clone(), idle);
    launches.unit.restart("firefox");
    launches.supervised.insert(
        Supervised::Start("firefox".into()),
        (
            supervised_child("launch::tests::failing_supervised_probe"),
            Instant::now(),
        ),
    );
    reap_all(&mut launches);
    assert_eq!(
        launches
            .answer(&config(), Request::UnitState("firefox".into()))
            .unwrap(),
        [0x85, 3]
    );
    let mut updated = Launches::with_status(generation, idle);
    updated.supervised.insert(
        Supervised::Update,
        (
            supervised_child("launch::tests::failing_supervised_probe"),
            Instant::now(),
        ),
    );
    reap_all(&mut updated);
    assert!(!updated.unit.start_failed);
    assert_eq!(
        updated
            .answer(&config(), Request::UnitState("firefox".into()))
            .unwrap(),
        [0x85, 4]
    );
}

#[test]
fn a_start_in_flight_reads_starting_and_starts_no_status_client() {
    let mut launches = Launches::with_status("000102030405060708090a0b0c0d0e0f".into(), running);
    launches.supervised.insert(
        Supervised::Start("firefox".into()),
        (
            supervised_child("launch::tests::delayed_validator_probe"),
            Instant::now(),
        ),
    );
    assert_eq!(
        launches
            .answer(&config(), Request::UnitState("firefox".into()))
            .unwrap(),
        [0x85, 1]
    );
    assert!(launches.unit.client.is_none());
    if let Some((mut child, _)) = launches
        .supervised
        .remove(&Supervised::Start("firefox".into()))
    {
        child.kill().unwrap();
        child.wait().unwrap();
    }
}

#[test]
fn a_start_that_cannot_spawn_reads_failed_until_the_unit_goes_up() {
    // The host has no supervisor client to start, which is the case under
    // test; a host that has one is not this fixture.
    if std::path::Path::new("/bin/td-svc").exists() {
        return;
    }
    let mut launches = Launches::with_status("000102030405060708090a0b0c0d0e0f".into(), running);
    assert_eq!(
        launches
            .answer(
                &config(),
                Request::Supervised(Supervised::Start("firefox".into()))
            )
            .unwrap(),
        [0xff, 2]
    );
    assert_eq!(launches.unit.epoch, 1);
    assert_eq!(
        launches
            .answer(&config(), Request::UnitState("firefox".into()))
            .unwrap(),
        [0x85, 3]
    );
    // A read of the unit running clears it.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let answer = launches
            .answer(&config(), Request::UnitState("firefox".into()))
            .unwrap();
        if answer == [0x85, 2] {
            break;
        }
        assert_eq!(answer, [0x85, 3]);
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn the_unit_state_request_answers_85_and_its_state() {
    let mut launches = Launches::with_status("000102030405060708090a0b0c0d0e0f".into(), running);
    assert_eq!(
        launches
            .answer(&config(), Request::UnitState("firefox".into()))
            .unwrap(),
        [0x85, 4]
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let answer = launches
            .answer(&config(), Request::UnitState("firefox".into()))
            .unwrap();
        if answer == [0x85, 2] {
            break;
        }
        assert_eq!(answer, [0x85, 4]);
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
    }
    // A heartbeat reaps a finished client too.
    launches.unit.began = None;
    launches
        .answer(&config(), Request::UnitState("firefox".into()))
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while launches.unit.client.is_some() {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
        assert_eq!(
            launches.answer(&config(), Request::Heartbeat).unwrap(),
            [0x83]
        );
    }
}

#[test]
fn a_start_or_read_names_an_application_the_registry_holds() {
    let generation = "000102030405060708090a0b0c0d0e0f".to_string();
    let mut launches = Launches::with_status(generation.clone(), running);
    let news = Request::Supervised(Supervised::Start("news".into()));
    assert_eq!(launches.answer(&config(), news).unwrap(), [0xff, 4]);
    assert!(launches.supervised.is_empty());
    assert_eq!(launches.unit.epoch, 0);
    assert_eq!(
        launches
            .answer(&config(), Request::UnitState("news".into()))
            .unwrap(),
        [0xff, 4]
    );
    assert!(launches.unit.name.is_none());
    // A registry root cannot read refuses both, and is read again.
    let mut unread = Launches::with(
        generation,
        running,
        crate::software::Requests::with_registry(|| Err("no registry".into())),
    );
    for request in [
        Request::Supervised(Supervised::Start("firefox".into())),
        Request::UnitState("firefox".into()),
    ] {
        assert_eq!(unread.answer(&config(), request).unwrap(), [0xff, 2]);
    }
    assert!(unread.supervised.is_empty());
}

#[test]
fn starts_in_flight_are_one_per_unit_and_bounded_in_all() {
    let mut launches = Launches::with(
        "000102030405060708090a0b0c0d0e0f".into(),
        running,
        crate::software::Requests::with_registry(|| {
            Ok((0..=SUPERVISED_LIMIT).map(|n| format!("app{n}")).collect())
        }),
    );
    for n in 0..SUPERVISED_LIMIT {
        launches.supervised.insert(
            Supervised::Start(format!("app{n}")),
            (
                supervised_child("launch::tests::delayed_validator_probe"),
                Instant::now(),
            ),
        );
    }
    // The unit's own start in flight is joined; any other is refused
    // while the table is full.
    let joined = Request::Supervised(Supervised::Start("app0".into()));
    assert_eq!(launches.answer(&config(), joined).unwrap(), [0xff, 1]);
    let full = Request::Supervised(Supervised::Start(format!("app{SUPERVISED_LIMIT}")));
    assert_eq!(launches.answer(&config(), full).unwrap(), [0xff, 5]);
    assert_eq!(launches.supervised.len(), SUPERVISED_LIMIT);
    for (_, (mut child, _)) in std::mem::take(&mut launches.supervised) {
        child.kill().unwrap();
        child.wait().unwrap();
    }
}

#[test]
fn the_probe_follows_the_last_application_named() {
    let mut probe = UnitProbe::new(running);
    probe.answer("firefox", false);
    assert_eq!(
        answer_after(&mut probe, UnitState::Unknown),
        UnitState::Running
    );
    let epoch = probe.epoch;
    // A read of another application starts over on it, starting nothing
    // it has not been asked to.
    probe.began = None;
    assert_eq!(probe.answer("mail", false), UnitState::Unknown);
    assert_eq!(probe.name.as_deref(), Some("mail"));
    assert_eq!(probe.epoch, epoch.wrapping_add(1));
    assert!(!probe.asked);
    // The running fixture prints Firefox's line, which says nothing of
    // mail's.
    let deadline = Instant::now() + Duration::from_secs(5);
    while probe.client.is_some() {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
        probe.advance();
    }
    assert_eq!(probe.last, Some(UnitState::Unknown));
}

#[test]
fn a_failed_start_of_another_application_leaves_the_followed_one() {
    let mut launches = Launches::with_status("000102030405060708090a0b0c0d0e0f".into(), idle);
    launches.unit.restart("firefox");
    launches.supervised.insert(
        Supervised::Start("mail".into()),
        (
            supervised_child("launch::tests::failing_supervised_probe"),
            Instant::now(),
        ),
    );
    reap_all(&mut launches);
    assert!(!launches.unit.start_failed);
}

#[test]
fn a_second_start_of_a_unit_in_flight_joins_it() {
    let mut launches = Launches::with_status("000102030405060708090a0b0c0d0e0f".into(), running);
    launches.supervised.insert(
        Supervised::Start("firefox".into()),
        (
            supervised_child("launch::tests::delayed_validator_probe"),
            Instant::now(),
        ),
    );
    let again = Request::Supervised(Supervised::Start("firefox".into()));
    assert_eq!(launches.answer(&config(), again).unwrap(), [0xff, 1]);
    assert_eq!(launches.supervised.len(), 1);
    // The update unit is not an application's to start.
    let update = Request::Supervised(Supervised::Start("firefox-update".into()));
    assert_eq!(launches.answer(&config(), update).unwrap(), [0xff, 4]);
    for (_, (mut child, _)) in std::mem::take(&mut launches.supervised) {
        child.kill().unwrap();
        child.wait().unwrap();
    }
}
