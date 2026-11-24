#![allow(clippy::unwrap_used, clippy::panic)]
use super::*;
use crate::common::Host;
use std::{fs, path::PathBuf, process::Command};
use td_fs::TempDir;

const OPERATIONS: &[Operation] = &[
    Operation::Deploy,
    Operation::Socket,
    Operation::Binaries,
    Operation::ToDebian,
    Operation::Retire,
    Operation::Recover,
    Operation::Rollback,
    Operation::Converge,
];

#[test]
fn a_loopback_tls_host_takes_the_stone_then_the_socket_and_nothing_else() {
    for &operation in OPERATIONS {
        for stone in [false, true] {
            // The live release predates group-socket admission.
            assert_eq!(
                step(operation, true, false, stone),
                if operation == Operation::Deploy {
                    Step::SteppingStone
                } else {
                    Step::SteppingStoneFirst
                },
                "{operation:?}"
            );
            // The stepping stone (or a later release) is live.
            assert_eq!(
                step(operation, true, true, stone),
                if operation == Operation::Socket {
                    Step::Proceed
                } else {
                    Step::SocketFirst
                },
                "{operation:?}"
            );
            // On the socket only the binary switch and convergence wait for
            // main, and only while a stepping stone is live.
            assert_eq!(
                step(operation, false, true, stone),
                if matches!(operation, Operation::Binaries | Operation::Converge) && stone {
                    Step::MainFirst
                } else {
                    Step::Proceed
                },
                "{operation:?}"
            );
        }
    }
}

fn facts(operation: Operation) -> Facts {
    Facts {
        operation,
        skip: false,
        irreversible: false,
        tls: true,
        fenced: false,
        running: false,
        admits: false,
        stone_live: false,
        stones: Stones::Verified,
        main_verified: true,
    }
}

#[test]
fn every_refusal_names_a_command_that_is_then_accepted() {
    // A held fence on loopback TLS: nothing, and no next step; a job
    // still running is waited for, not recovered.
    for &operation in OPERATIONS {
        let f = Facts {
            fenced: true,
            ..facts(operation)
        };
        assert_eq!(decide(&f), Decision::Refuse(FENCED, None));
        let f = Facts { running: true, ..f };
        assert_eq!(decide(&f), Decision::Refuse(RUNNING, None));
    }
    for stones in [Stones::Verified, Stones::Unverified] {
        let verified = stones == Stones::Verified;
        // Before the stone, every other operation names the stone deploy...
        for &operation in OPERATIONS {
            if operation == Operation::Deploy {
                continue;
            }
            let f = Facts {
                stones,
                ..facts(operation)
            };
            let Decision::Refuse(error, Some(flags)) = decide(&f) else {
                panic!("{operation:?}");
            };
            assert_eq!((error, flags), (STONE_FIRST, stone_flags(verified)));
            // ...which, run as named, deploys the stone.
            let named = Facts {
                stones,
                skip: flags.contains(&"--skip-tests"),
                irreversible: flags.contains(&"--irreversible"),
                ..facts(Operation::Deploy)
            };
            assert_eq!(decide(&named), Decision::Stone);
        }
        // A plain deploy names its missing consents, and that is accepted.
        for (skip, irreversible) in [(false, false), (true, false), (false, true)] {
            let f = Facts {
                stones,
                skip,
                irreversible,
                ..facts(Operation::Deploy)
            };
            match decide(&f) {
                Decision::Refuse(error, Some(flags)) => {
                    assert!([NO_VERDICT, IRREVERSIBLE].contains(&error));
                    let named = Facts {
                        skip: flags.contains(&"--skip-tests"),
                        irreversible: flags.contains(&"--irreversible"),
                        ..f
                    };
                    assert_eq!(decide(&named), Decision::Stone, "{skip} {irreversible}");
                }
                decision => {
                    assert_eq!(decision, Decision::Stone);
                    assert!(irreversible && (verified || skip));
                }
            }
        }
    }
    // No stone at all: nothing to name.
    for &operation in OPERATIONS {
        let f = Facts {
            stones: Stones::None,
            skip: true,
            irreversible: true,
            ..facts(operation)
        };
        assert_eq!(decide(&f), Decision::Refuse(NO_STONE, None));
    }
    // After the stone, everything names the cutover, which proceeds.
    for &operation in OPERATIONS {
        let f = Facts {
            admits: true,
            ..facts(operation)
        };
        let expected = if operation == Operation::Socket {
            Decision::Proceed
        } else {
            Decision::Refuse(SOCKET_FIRST, Some(&["--postgres-socket"]))
        };
        assert_eq!(decide(&f), expected, "{operation:?}");
    }
    // On the socket under the stone, the binary switch and convergence
    // name main, with --skip-tests when no newer main revision has a
    // verdict; that deploy proceeds.
    for operation in [Operation::Binaries, Operation::Converge] {
        let under_stone = |main_verified| Facts {
            tls: false,
            admits: true,
            stone_live: true,
            main_verified,
            ..facts(operation)
        };
        assert_eq!(
            decide(&under_stone(true)),
            Decision::Refuse(MAIN_FIRST, Some(&[]))
        );
        assert_eq!(
            decide(&under_stone(false)),
            Decision::Refuse(MAIN_UNVERIFIED, Some(&["--skip-tests"]))
        );
        let main = Facts {
            operation: Operation::Deploy,
            skip: true,
            ..under_stone(false)
        };
        assert_eq!(decide(&main), Decision::Proceed);
    }
    // A fence held on the socket is --recover's to judge.
    let fenced = Facts {
        tls: false,
        fenced: true,
        admits: true,
        ..facts(Operation::Recover)
    };
    assert_eq!(decide(&fenced), Decision::Proceed);
}

#[test]
fn the_operation_is_the_first_named_mode() {
    let of = |args: &[&str]| {
        let args: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
        operation(&super::super::parse(&args).unwrap())
    };
    assert_eq!(of(&[]), Operation::Deploy);
    assert_eq!(of(&["--skip-tests", "--irreversible"]), Operation::Deploy);
    assert_eq!(of(&["--postgres-socket"]), Operation::Socket);
    assert_eq!(of(&["--postgres-binaries"]), Operation::Binaries);
    assert_eq!(
        of(&["--postgres-binaries", "--to-debian"]),
        Operation::ToDebian
    );
    assert_eq!(
        of(&["--retire-rollback", "--irreversible"]),
        Operation::Retire
    );
    assert_eq!(of(&["--recover"]), Operation::Recover);
    assert_eq!(of(&["--rollback"]), Operation::Rollback);
    assert_eq!(of(&["--converge-config"]), Operation::Converge);
    assert_eq!(of(&["--converge-config", "--plan"]), Operation::Converge);
}

#[test]
fn the_tls_controller_host_is_read_from_its_own_sources() {
    let activate = "        fs::read_to_string(\"/proc/sys/kernel/hostname\")?.trim() == \"mx1\"\n";
    let release = "const BUILD_USER: &str = \"alice\";\nconst BUILD_ROOT: &str = \"/home/alice/x\";\nconst MIRROR: &str = \"/home/alice/src/td\";\n";
    assert_eq!(
        controller_host(activate, release).unwrap(),
        ["mx1", "alice", "/home/alice/src/td"]
    );
    let twice = format!("{release}const MIRROR: &str = \"/elsewhere\";\n");
    assert!(controller_host(activate, &twice).is_err());
    assert!(controller_host("", release).is_err());
}

#[test]
fn the_named_stepping_stone_command_carries_its_consents() {
    assert_eq!(stone_flags(true), ["--irreversible"]);
    assert_eq!(stone_flags(false), ["--skip-tests", "--irreversible"]);
    assert_eq!(
        argv(&["--postgres-socket"], Some("/home/alice/t.json")).unwrap(),
        [
            "td-mta/deploy",
            "--postgres-socket",
            "--config",
            "/home/alice/t.json"
        ]
    );
    assert_eq!(argv(&[], None).unwrap(), ["td-mta/deploy"]);
}

#[test]
fn the_boundaries_are_full_main_revisions() {
    for boundary in [GROUP_SOCKET, LAST_TCP_TLS, TLS_CONTROLLER] {
        assert_eq!(super::super::revision(boundary).unwrap(), boundary);
    }
    assert_eq!(RANGE.admits, GROUP_SOCKET);
    assert_eq!(RANGE.last_tls, LAST_TCP_TLS);
}

#[test]
fn the_redacted_runtime_connection_is_judged_as_the_controller_judges_it() {
    let ca = "/var/lib/td-mta/mail/tls/ca.pem";
    for host in ["localhost", "127.0.0.1"] {
        let location = format!("{host}:35432/td_mta?sslmode=verify-full&sslrootcert={ca}");
        assert!(redacted_runtime_tls(&location).unwrap(), "{location}");
    }
    assert!(!redacted_runtime_tls(":35432/td_mta?host=%2Fvar%2Flib%2Ftd-mta-postgres").unwrap());
    for refused in [
        "",
        "localhost:35432/td_mta?sslmode=verify-full&sslrootcert=/elsewhere/ca.pem",
        "localhost:5432/td_mta?sslmode=verify-full&sslrootcert=/var/lib/td-mta/mail/tls/ca.pem",
        ":35432/td_mta?host=%2Ftmp",
        "x@localhost:35432/td_mta?sslmode=verify-full&sslrootcert=/var/lib/td-mta/mail/tls/ca.pem",
    ] {
        assert!(redacted_runtime_tls(refused).is_err(), "{refused}");
    }
}

fn git(root: &Path, args: &[&str]) -> String {
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
    assert!(
        out.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

fn commit(root: &Path, subject: &str) -> String {
    git(root, &["commit", "--allow-empty", "-qm", subject]);
    git(root, &["rev-parse", "HEAD"])
}

#[test]
fn the_stepping_stone_is_the_newest_verified_first_parent_revision_in_range() {
    let tmp = TempDir::new("mta-deploy-stone").unwrap();
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    let live = commit(root, "live");
    let admits = commit(root, "admits the group socket");
    git(root, &["checkout", "-qb", "side", &admits]);
    let side = commit(root, "side");
    git(root, &["checkout", "-q", "main"]);
    let mid = commit(root, "mid");
    git(root, &["merge", "-q", "--no-ff", "-m", "merge", "side"]);
    let merge = git(root, &["rev-parse", "HEAD"]);
    let last = commit(root, "last to read TLS files");
    let socket_only = commit(root, "socket only");
    let tip = commit(root, "tip");
    let verdicts = root.join("verdicts");
    fs::create_dir(&verdicts).unwrap();
    let local = Local {
        host: Host::default(),
        root: root.into(),
        destination: "alice@192.0.2.10".into(),
        key: PathBuf::new(),
        ssh_options: Vec::new(),
    };
    let range = Range {
        admits: &admits,
        last_tls: &last,
    };
    let pick = |live: &str, skip: bool| local.stepping_stone(&tip, live, skip, &verdicts, &range);
    // No verdict: only --skip-tests, which takes the newest in range.
    assert_eq!(pick(&live, false).unwrap(), None);
    assert_eq!(pick(&live, true).unwrap(), Some((last.clone(), None)));
    // Verdicts outside the range, or off the first-parent line, never count.
    for rev in [&tip, &socket_only, &side, &live] {
        fs::write(verdicts.join(rev), "pass 0 1 2\n").unwrap();
    }
    assert_eq!(pick(&live, false).unwrap(), None);
    fs::write(verdicts.join(&admits), "pass 0 3 4\n").unwrap();
    assert_eq!(
        pick(&live, false).unwrap(),
        Some((admits.clone(), Some("pass 0 3 4".into())))
    );
    fs::write(verdicts.join(&mid), "pass 0 5 6\n").unwrap();
    fs::write(verdicts.join(&merge), "fail 1 7 8\n").unwrap();
    assert_eq!(
        pick(&live, false).unwrap(),
        Some((mid.clone(), Some("pass 0 5 6".into())))
    );
    // Only revisions after the live one.
    assert_eq!(pick(&mid, false).unwrap(), None);
    assert_eq!(pick(&mid, true).unwrap(), Some((last.clone(), None)));
    assert_eq!(pick(&last, true).unwrap(), None);
    // Boundaries missing from the fetched main refuse.
    for skip in [false, true] {
        assert!(local
            .stepping_stone(&live, &live, skip, &verdicts, &range)
            .is_err());
    }
}
