//! Sixteen large files profiles; sampled owners are not aggregate admission.
#![cfg(test)]
#![allow(clippy::unwrap_used, clippy::panic)]
use super::tls_handshake_scenario::{
    large_material_names, large_routing_material, large_trust_bundle,
};
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

#[derive(Clone, Copy, PartialEq)]
pub enum Scenario {
    Ordinary,
    Routing,
    Trust,
}
impl Scenario {
    pub fn label(self) -> &'static str {
        match self {
            Self::Ordinary => "generation",
            Self::Routing => "generation-routing",
            Self::Trust => "generation-trust",
        }
    }
}

pub fn run(scenario: Scenario, mut observe: impl FnMut()) {
    let routing = scenario == Scenario::Routing;
    let trust = scenario == Scenario::Trust;
    observe();
    {
        let (materials, text) = match scenario {
            Scenario::Ordinary => ordinary_inputs(),
            Scenario::Routing => routing_inputs(),
            Scenario::Trust => trust_inputs(),
        };
        observe();
        let mut scratch = vec![0; stream::SCRATCH_BYTES];
        let loaded = load::read(
            storage::Storage::try_new().unwrap(),
            &mut stanza::Pending::new(),
            &mut scratch,
            &mut Cursor::new(text.as_bytes()),
        )
        .unwrap_or_else(|failure| match failure.error() {
            storage::BuildError::Input(stream::Error::Handler(e)) => {
                panic!("fixture handler: {e:?}")
            }
            storage::BuildError::Input(e) => panic!("fixture stream: {e:?}"),
            storage::BuildError::Configuration(e) => panic!("fixture configuration: {e:?}"),
        });
        let config = materialize::read_text(loaded, &mut scratch, |_| {
            Ok::<_, Error>(Cursor::new(b"password\n"))
        })
        .unwrap();
        if routing {
            config
                .candidate()
                .with_graph(|records, text| {
                    let view = records.view(text).unwrap();
                    assert_eq!(view.certificates().unwrap().len(), 16);
                    assert_eq!(view.listeners().unwrap().len(), 16);
                    assert_eq!(view.domains().unwrap().len(), 256);
                    assert_eq!(view.binding_count(), 257);
                    let long = (0..view.binding_count())
                        .filter(|&i| view.binding(i).unwrap().unwrap().name.len() == 251)
                        .count();
                    assert_eq!(long, 255);
                })
                .unwrap();
        }
        if trust {
            config
                .candidate()
                .with_graph(|records, text| {
                    let view = records.view(text).unwrap();
                    assert_eq!(view.certificates().unwrap().len(), 16);
                    assert_eq!(view.listeners().unwrap().len(), 16);
                    assert_eq!(view.gateways().unwrap().len(), 15);
                })
                .unwrap();
        }
        drop(text);
        drop(scratch);
        let clock = Arc::new(ClockHandle::new(FixedClock));
        let mut set = GenerationSet::at_startup();
        observe();
        let prepare = |set: &GenerationSet<TlsPolicies>| {
            let (mut chains, mut keys, mut roots, mut gateways) = (0, 0, 0, 0);
            let prepared =
                TlsPolicies::prepare(set.reserve().unwrap(), &config, clock.clone(), |request| {
                    let index = if routing
                        && matches!(request.kind(), MaterialKind::Chain | MaterialKind::Key)
                    {
                        request
                            .profile()
                            .unwrap()
                            .strip_prefix("profile")
                            .unwrap()
                            .parse::<usize>()
                            .unwrap()
                    } else {
                        0
                    };
                    let (chain, key, ca) = materials.get(index).unwrap();
                    let bytes = match request.kind() {
                        MaterialKind::Chain => {
                            chains += 1;
                            chain
                        }
                        MaterialKind::Key => {
                            keys += 1;
                            key
                        }
                        MaterialKind::RelayCa => {
                            roots += 1;
                            ca
                        }
                        MaterialKind::GatewayCa if trust => {
                            gateways += 1;
                            ca
                        }
                        _ => panic!("unexpected material role"),
                    };
                    Ok::<_, Error>(Cursor::new(bytes.as_slice()))
                })
                .unwrap();
            assert_eq!((chains, keys, roots), (16, 16, 1));
            assert_eq!(gateways, if trust { 15 } else { 0 });
            prepared
        };
        drop(set.publish(prepare(&set)).unwrap());
        let old = set.current().unwrap();
        assert_eq!(old.value().len(), if routing || trust { 17 } else { 3 });
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
        black_box((&materials, &config, &clock));
    }
    observe();
}

type Material = (Vec<u8>, Vec<u8>, Vec<u8>);
fn ordinary_inputs() -> (Vec<Material>, String) {
    let mut names = vec!["localhost".to_owned()];
    for i in 2..16 {
        names.push(format!("mta-sts.example{i}.test"));
    }
    let borrowed: Vec<_> = names.iter().map(String::as_str).collect();
    let materials = vec![large_material_names(&borrowed)];
    let mut text = SOURCE.to_owned();
    for i in 0..16 {
        profile(&mut text, i);
        if i >= 2 {
            write!(text, "[domain \"example{i}.test\"]\nmta_sts = \"testing\"\nmta_sts_certificate = \"profile{i}\"\n").unwrap();
        }
    }
    text.push_str("[listener \"smtp\"]\nkind = \"direct_smtp\"\nbind = \"127.0.0.1:25\"\nserver_name = \"localhost\"\ncertificate = \"profile0\"\nsession_limit = 1\nper_peer_limit = 1\n");
    text.push_str("[listener \"https\"]\nkind = \"https\"\nbind = \"127.0.0.1:443\"\ncertificate = \"profile1\"\n");
    (materials, text)
}
fn profile(text: &mut String, i: usize) {
    write!(text, "[certificate \"profile{i}\"]\nmode = \"files\"\nchain_file = \"/chain{i}\"\nkey_file = \"/key{i}\"\n").unwrap();
}
fn routing_inputs() -> (Vec<Material>, String) {
    let mut text = SOURCE.replace(
        "[domain \"example.test\"]",
        "[domain \"example.test\"]\nmta_sts = \"testing\"\nmta_sts_certificate = \"profile15\"",
    );
    let mut names: Vec<Vec<String>> = (0..16).map(|_| Vec::new()).collect();
    names.get_mut(0).unwrap().push("localhost".to_owned());
    names
        .get_mut(15)
        .unwrap()
        .push("mta-sts.example.test".to_owned());
    for i in 0..255 {
        let domain = format!(
            "{}.{}.{}.{}{:03}",
            "a".repeat(63),
            "b".repeat(63),
            "c".repeat(63),
            "d".repeat(48),
            i
        );
        let name = format!("mta-sts.{domain}");
        assert_eq!(domain.len(), 243);
        assert_eq!(name.len(), 251);
        let profile = i % 16;
        names.get_mut(profile).unwrap().push(name);
        writeln!(text, "[domain \"{domain}\"]\nmta_sts = \"testing\"\nmta_sts_certificate = \"profile{profile}\"").unwrap();
    }
    let mut materials = Vec::new();
    for (i, names) in names.iter().enumerate() {
        profile(&mut text, i);
        let borrowed: Vec<_> = names.iter().map(String::as_str).collect();
        let (chain, key, ca) = large_routing_material(&borrowed);
        materials.push((chain, key, if i == 0 { ca } else { Vec::new() }));
    }
    text.push_str("[listener \"smtp\"]\nkind = \"direct_smtp\"\nbind = \"127.0.0.1:25\"\nserver_name = \"localhost\"\ncertificate = \"profile0\"\nsession_limit = 1\nper_peer_limit = 1\n");
    for i in 1..16 {
        writeln!(text, "[listener \"https{i}\"]\nkind = \"https\"\nbind = \"127.0.0.{i}:443\"\ncertificate = \"profile0\"").unwrap();
    }
    assert_eq!(materials.len(), 16);
    assert_eq!(names.iter().map(Vec::len).sum::<usize>(), 257);
    (materials, text)
}

fn trust_inputs() -> (Vec<Material>, String) {
    let (mut materials, mut text) = ordinary_inputs();
    let start = text.find("[listener \"smtp\"]").unwrap();
    let end = text[start..].find("[listener \"https\"]").unwrap() + start;
    text.replace_range(start..end, "");
    // Explicit MX allows a gateway-only receiver with no direct SMTP listener.
    text = text.lines().fold(String::new(), |mut out, line| {
        writeln!(out, "{line}").unwrap();
        if line.starts_with("[domain ") {
            out.push_str("mx_host = \"mx.gateway.test\"\n");
        }
        out
    });
    for i in 0..15 {
        writeln!(text, "[gateway \"upstream{i}\"]\nca_file = \"/gateway-ca{i}\"\nclient_cert_sha256 = \"{}\"\n[gateway_peer \"upstream{i}\"]\nnetwork = \"192.0.2.0/24\"\n[listener \"gateway{i}\"]\nkind = \"gateway_smtp\"\nbind = \"127.0.0.{}:2525\"\nserver_name = \"localhost\"\ncertificate = \"profile0\"\ngateway = \"upstream{i}\"\nsession_limit = 1\nper_peer_limit = 1", "11".repeat(32), i + 1).unwrap();
    }
    text.push_str("[limits]\nsmtp_sessions = 15\nindex_cache_bytes = 524288\nmemory_budget_bytes = 134217728\n");
    materials.get_mut(0).unwrap().2 = large_trust_bundle();
    (materials, text)
}
