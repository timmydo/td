//! Identical socket-free outbound lifecycle for separate allocation observers.
use std::{hint::black_box, io::Cursor, sync::Arc};
use td_crypto::{ClockHandle, TlsPhase, UtcClock};
use td_mta::{
    config::{load, materialize, stanza, storage, stream},
    generations::GenerationSet,
    ports::Error,
    tls_admission::HandshakePool,
    tls_io::TLS_WIRE_BYTES,
    tls_policy::TlsPolicies,
};

pub const PHASES: [&str; 12] = [
    "baseline",
    "config",
    "generation",
    "buffers",
    "reserved",
    "constructed",
    "overlap",
    "two_sessions",
    "refused",
    "released",
    "repeated",
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
[certificate "public"]
mode = "acme"
[listener "smtp"]
kind = "direct_smtp"
bind = "127.0.0.1:25"
server_name = "localhost"
certificate = "public"
session_limit = 1
per_peer_limit = 1
[listener "https"]
kind = "https"
bind = "127.0.0.1:443"
certificate = "public"
[acme]
directory = "https://localhost/directory"
contact = "main@example.test"
terms_accepted = true
[listener "http01"]
kind = "http01"
bind = "127.0.0.1:80"
"#;

struct FixedClock;
impl UtcClock for FixedClock {
    fn now(&self) -> Option<u64> {
        Some(1_800_000_000)
    }
}

fn buffer() -> Box<[u8; TLS_WIRE_BYTES]> {
    vec![0; TLS_WIRE_BYTES]
        .into_boxed_slice()
        .try_into()
        .unwrap()
}

pub fn run(mut observe: impl FnMut()) {
    observe();
    {
        let mut scratch = vec![0; stream::SCRATCH_BYTES];
        let loaded = load::read(
            storage::Storage::try_new().unwrap(),
            &mut stanza::Pending::new(),
            &mut scratch,
            &mut Cursor::new(SOURCE),
        )
        .unwrap();
        let configuration = materialize::read_text(loaded, &mut scratch, |_| {
            Ok::<_, Error>(Cursor::new(b"password\n"))
        })
        .unwrap();
        drop(scratch);
        let clock = Arc::new(ClockHandle::new(FixedClock));
        let mut set = GenerationSet::at_startup();
        observe();
        let prepare = |set: &GenerationSet<TlsPolicies>| {
            TlsPolicies::prepare_clients(
                set.reserve().unwrap(),
                &configuration,
                clock.clone(),
                |_| -> Result<Cursor<&[u8]>, Error> {
                    panic!("public trust must not open material")
                },
            )
            .unwrap()
        };
        drop(set.publish(prepare(&set)).unwrap());
        let old = set.current().unwrap();
        let relay = TlsPolicies::relay(&old).unwrap();
        assert_eq!(old.value().len(), 2);
        observe();
        let pool = HandshakePool::new(2).unwrap();
        let (input, output) = (buffer(), buffer());
        let (second_input, second_output) = (buffer(), buffer());
        observe();
        let pending = TlsPolicies::reserve_session(old, relay, &pool, input, output).unwrap();
        assert_eq!(pool.available(), 1);
        observe();
        let first = pending.construct().unwrap();
        assert_eq!(black_box(first.status()).phase, TlsPhase::Handshaking);
        observe();
        drop(set.publish(prepare(&set)).unwrap());
        assert_eq!(set.available(), 0);
        observe();
        let current = set.current().unwrap();
        let acme = TlsPolicies::acme(&current).unwrap();
        let second =
            TlsPolicies::reserve_session(current.clone(), acme, &pool, second_input, second_output)
                .unwrap()
                .construct()
                .unwrap();
        assert_eq!(black_box(second.status()).phase, TlsPhase::Handshaking);
        observe();
        assert!(matches!(pool.reserve(), Err(Error::Busy)));
        assert_eq!(set.reserve().unwrap_err(), Error::Busy);
        observe();
        let (mut input, mut output) = first.into_buffers();
        let other_buffers = second.into_buffers();
        assert_eq!(set.available(), 1);
        assert_eq!(pool.available(), 2);
        observe();
        for _ in 0..32 {
            let session = TlsPolicies::reserve_session(current.clone(), acme, &pool, input, output)
                .unwrap()
                .construct()
                .unwrap();
            assert_eq!(black_box(session.status()).phase, TlsPhase::Handshaking);
            (input, output) = session.into_buffers();
        }
        observe();
        black_box((&input, &output, &other_buffers));
    }
    observe();
}
