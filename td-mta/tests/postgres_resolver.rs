#![allow(clippy::unwrap_used)]
use std::{
    path::Path,
    time::{Duration, Instant},
};
use td_mta::{
    postgres_bootstrap::Bootstrap,
    postgres_resolver::{resolve, Error},
};

#[test]
fn resolver_rejects_invalid_executable_and_request_deadlines() {
    let bootstrap =
        Bootstrap::parse("postgresql://fixture:unused@localhost/mail", "desktop").unwrap();
    let executable = Path::new(env!("CARGO_BIN_EXE_td-mta"));
    assert_eq!(
        resolve(
            Path::new("td-mta"),
            &bootstrap,
            Instant::now() + Duration::from_secs(3)
        ),
        Err(Error::Invalid)
    );
    assert_eq!(
        resolve(executable, &bootstrap, Instant::now()),
        Err(Error::Deadline)
    );
    assert_eq!(
        resolve(
            executable,
            &bootstrap,
            Instant::now() + Duration::from_secs(6)
        ),
        Err(Error::Invalid)
    );
}

#[test]
#[ignore = "host qualification requires platform localhost resolution"]
fn real_helper_resolves_localhost_twice() {
    let bootstrap =
        Bootstrap::parse("postgresql://fixture:unused@localhost/mail", "desktop").unwrap();
    let executable = Path::new(env!("CARGO_BIN_EXE_td-mta"));
    for _ in 0..2 {
        let addresses = resolve(
            executable,
            &bootstrap,
            Instant::now() + Duration::from_secs(3),
        )
        .unwrap();
        assert!(!addresses.is_empty());
        assert!(addresses.iter().all(|address| address.is_loopback()));
    }
}
