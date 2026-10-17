#![deny(unsafe_code)]
#![allow(clippy::unwrap_used)]
use std::process::{Command, Output};
fn invoke(args: &[&str]) -> Output {
    let binary = if option_env!("TD_MTA_PORTABLE_CLI") == Some("1") {
        "/artifacts/td-mta"
    } else {
        env!("CARGO_BIN_EXE_td-mta")
    };
    Command::new(binary).args(args).output().unwrap()
}
#[test]
fn help_names_postgres_serving_and_omits_local_store_commands() {
    let output = invoke(&["--help"]);
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).unwrap();
    assert!(help.contains("serve --connection-file PATH --profile NAME"));
    assert!(help.contains("postgres device-create"));
    for old in [
        "store init",
        "store verify",
        "backup --root",
        "restore --root",
        "device create --root",
        "config check --config",
    ] {
        assert!(!help.contains(old));
    }
}
#[test]
fn obsolete_commands_refuse_without_accessing_a_store() {
    for args in [
        vec!["store", "init", "--root", "/absent"],
        vec!["store", "verify", "--root", "/absent", "--all"],
        vec!["backup", "--root", "/absent", "--destination", "/absent"],
        vec!["restore", "--root", "/absent", "--destination", "/absent"],
        vec!["device", "list", "--root", "/absent"],
        vec!["config", "check", "--config", "/absent"],
    ] {
        assert_eq!(invoke(&args).status.code(), Some(2));
    }
}
#[test]
fn serving_and_control_require_explicit_bootstrap_arguments() {
    for args in [
        vec!["serve"],
        vec!["serve", "--smtp-only", "--config", "/absent"],
        vec!["serve", "--mailbox-access", "--config", "/absent"],
        vec![
            "serve",
            "--connection-file",
            "/absent",
            "--profile",
            "desktop",
        ],
        vec![
            "serve",
            "--connection-file",
            "/absent",
            "--profile",
            "desktop",
            "--runtime",
            "/absent",
            "--scratch",
            "/absent",
            "--profile",
            "cloud",
        ],
        vec!["status"],
        vec!["status", "--runtime", "/absent"],
        vec!["stop"],
    ] {
        assert_eq!(invoke(&args).status.code(), Some(2));
    }
}
