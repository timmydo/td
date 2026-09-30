//! Sixteen large files profiles; sampled owners are not aggregate admission.
#![cfg(test)]
#![allow(clippy::unwrap_used, clippy::panic)]
use super::tls_handshake_scenario::large_material_names;
use std::{fmt::Write, hint::black_box, io::Cursor, sync::Arc};
use td_crypto::{ClockHandle, UtcClock};
use td_mta::{
    config::{load, materialize, stanza, storage, stream},
    generations::GenerationSet,
    ports::Error,
    tls_policy::{MaterialKind, PolicyCoverage, TlsPolicies},
};

pub const PHASES: [&str; 11] = [
    "baseline",
    "material",
    "config",
    "first",
    "candidate",
    "overlap",
    "refused",
    "old_released",
    "repeated",
    "current_released",
    "dropped",
];
const SOURCE: &str = r#"version = 1
[server]
hostname = "localhost"
jmap_origin = "https://localhost"
[account "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"]
username = "private-user"
[domain "example.test"]
[alias "main@example.test"]
account = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
[identity "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"]
account = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
email = "main@example.test"
[resolver "primary"]
address = "127.0.0.1:53"
[relay]
host = "localhost"
port = 465
username = "private-relay"
password_file = "/password"
ca_file = "/root"
"#;

struct FixedClock;
impl UtcClock for FixedClock {
    fn now(&self) -> Option<u64> {
        Some(1_800_000_000)
    }
}

pub fn run(mut observe: impl FnMut()) {
    observe();
    {
        let mut names = vec!["localhost".to_owned()];
        for i in 2..16 {
            names.push(format!("mta-sts.example{i}.test"));
        }
        let borrowed: Vec<_> = names.iter().map(String::as_str).collect();
        let (chain, key, ca) = large_material_names(&borrowed);
        drop(borrowed);
        drop(names);
        observe();
        let mut text = SOURCE.to_owned();
        for i in 0..16 {
            write!(text, "[certificate \"profile{i}\"]\nmode = \"files\"\nchain_file = \"/chain{i}\"\nkey_file = \"/key{i}\"\n").unwrap();
            if i >= 2 {
                write!(text, "[domain \"example{i}.test\"]\nmta_sts = \"testing\"\nmta_sts_certificate = \"profile{i}\"\n").unwrap();
            }
        }
        text.push_str(
            r#"[listener "smtp"]
kind = "direct_smtp"
bind = "127.0.0.1:25"
server_name = "localhost"
certificate = "profile0"
session_limit = 1
per_peer_limit = 1
[listener "https"]
kind = "https"
bind = "127.0.0.1:443"
certificate = "profile1"
"#,
        );
        let mut scratch = vec![0; stream::SCRATCH_BYTES];
        let loaded = load::read(
            storage::Storage::try_new().unwrap(),
            &mut stanza::Pending::new(),
            &mut scratch,
            &mut Cursor::new(text.as_bytes()),
        )
        .unwrap();
        let config = materialize::read_text(loaded, &mut scratch, |_| {
            Ok::<_, Error>(Cursor::new(b"password\n"))
        })
        .unwrap();
        drop(text);
        drop(scratch);
        let clock = Arc::new(ClockHandle::new(FixedClock));
        let mut set = GenerationSet::at_startup();
        observe();
        let prepare = |set: &GenerationSet<TlsPolicies>| {
            let (mut chains, mut keys, mut roots) = (0, 0, 0);
            let prepared =
                TlsPolicies::prepare(set.reserve().unwrap(), &config, clock.clone(), |request| {
                    let bytes = match request.kind() {
                        MaterialKind::Chain => {
                            chains += 1;
                            &chain
                        }
                        MaterialKind::Key => {
                            keys += 1;
                            &key
                        }
                        MaterialKind::RelayCa => {
                            roots += 1;
                            &ca
                        }
                        _ => panic!("unexpected material role"),
                    };
                    Ok::<_, Error>(Cursor::new(bytes.as_slice()))
                })
                .unwrap();
            assert_eq!((chains, keys, roots), (16, 16, 1));
            prepared
        };
        drop(set.publish(prepare(&set)).unwrap());
        let old = set.current().unwrap();
        assert_eq!(old.value().len(), 3);
        assert_eq!(old.value().coverage(), PolicyCoverage::Complete);
        observe();
        let candidate = prepare(&set);
        assert_eq!(set.available(), 0);
        observe();
        drop(set.publish(candidate).unwrap());
        observe();
        assert_eq!(set.reserve().unwrap_err(), Error::Busy);
        observe();
        drop(old);
        assert_eq!(set.available(), 1);
        observe();
        for _ in 0..4 {
            drop(set.publish(prepare(&set)).unwrap());
            assert_eq!(set.available(), 1);
        }
        observe();
        drop(set);
        observe();
        black_box((&chain, &key, &ca, &config, &clock));
    }
    observe();
}
