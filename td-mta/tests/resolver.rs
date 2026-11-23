#![allow(clippy::unwrap_used)]
use std::{
    path::Path,
    time::{Duration, Instant},
};
use td_mta::resolver::{resolve_host, Error};

#[test]
fn resolver_rejects_invalid_executable_hosts_and_request_deadlines() {
    let executable = Path::new(env!("CARGO_BIN_EXE_td-mta"));
    assert_eq!(
        resolve_host(
            Path::new("td-mta"),
            "localhost",
            Instant::now() + Duration::from_secs(3)
        ),
        Err(Error::Invalid)
    );
    assert_eq!(
        resolve_host(executable, "localhost", Instant::now()),
        Err(Error::Deadline)
    );
    assert_eq!(
        resolve_host(
            executable,
            "localhost",
            Instant::now() + Duration::from_secs(6)
        ),
        Err(Error::Invalid)
    );
    assert_eq!(
        resolve_host(
            executable,
            "127.0.0.1",
            Instant::now() + Duration::from_secs(3)
        ),
        Err(Error::Invalid)
    );
}

#[test]
#[ignore = "host qualification requires platform localhost resolution"]
fn real_helper_resolves_localhost() {
    let executable = Path::new(env!("CARGO_BIN_EXE_td-mta"));
    let addresses = resolve_host(
        executable,
        "localhost",
        Instant::now() + Duration::from_secs(3),
    )
    .unwrap();
    assert!(!addresses.is_empty());
    assert!(addresses.iter().all(|address| address.is_loopback()));
}
