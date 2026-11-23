#![allow(clippy::unwrap_used)]
use super::*;
use std::{fs, os::unix::fs::PermissionsExt};
use td_fs::TempDir;

const LAUNCHER: &str = r#"{"host":"192.0.2.10","host_name":"mx1","account":"alice","identity":"~/.ssh/deploy","jmap_account":"alice","jmap_url":"https://mx.example.org/.well-known/jmap"}"#;
const HOST: &str = r#"{"host_name":"mx1","build_account":"alice","build_home":"/home/alice","public_name":"mx.example.org"}"#;

fn home() -> &'static Path {
    Path::new("/home/alice")
}

#[test]
fn the_launcher_target_names_every_value_and_nothing_else() {
    let target = Target::parse(LAUNCHER.as_bytes(), home()).unwrap();
    assert_eq!(
        target,
        Target {
            destination: "alice@192.0.2.10".into(),
            host_name: "mx1".into(),
            identity: "/home/alice/.ssh/deploy".into(),
            jmap_account: "alice".into(),
            jmap_url: "https://mx.example.org/.well-known/jmap".into(),
        }
    );
    let absolute = LAUNCHER.replace("~/.ssh/deploy", "/keys/deploy");
    assert_eq!(
        Target::parse(absolute.as_bytes(), home()).unwrap().identity,
        Path::new("/keys/deploy")
    );
    let named = LAUNCHER.replace("192.0.2.10", "mx.example.org");
    assert_eq!(
        Target::parse(named.as_bytes(), home()).unwrap().destination,
        "alice@mx.example.org"
    );
    for (from, to) in [
        // Every field is required; none has a default.
        (r#""host":"192.0.2.10","#, ""),
        (r#""host_name":"mx1","#, ""),
        (r#""account":"alice","#, ""),
        (r#""jmap_account":"alice","#, ""),
        // Unknown, duplicate and mistyped fields refuse.
        (r#""host":"#, r#""port":22,"host":"#),
        (r#""host":"#, r#""host":"192.0.2.11","host":"#),
        (r#""192.0.2.10""#, "1"),
        // Option-like, bracketed or shell-shaped values refuse.
        ("192.0.2.10", "-oProxyCommand=x"),
        ("192.0.2.10", "[2001:db8::1]"),
        ("192.0.2.10", "a b.example.org"),
        ("192.0.2.10", "192.0.2.10:22"),
        (r#""mx1""#, r#""mx 1""#),
        (r#""account":"alice""#, r#""account":"-alice""#),
        (r#""account":"alice""#, r#""account":"Alice""#),
        (r#""account":"alice""#, r#""account":"al ice""#),
        ("~/.ssh/deploy", ".ssh/deploy"),
        ("~/.ssh/deploy", "~alice/.ssh/deploy"),
        ("~/.ssh/deploy", "~/../deploy"),
        ("~/.ssh/deploy", "~/./deploy"),
        ("~/.ssh/deploy", "/keys/"),
        ("~/.ssh/deploy", "~/.ssh/%h"),
        ("~/.ssh/deploy", "~/.ssh/${KEY}"),
        ("~/.ssh/deploy", ""),
        ("https://mx.example.org", "http://mx.example.org"),
        ("https://mx.example.org", "https://mx example.org"),
        (r#""jmap_account":"alice""#, r#""jmap_account":"""#),
        (r#""jmap_account":"alice""#, r#""jmap_account":"a\nb""#),
    ] {
        let text = LAUNCHER.replacen(from, to, 1);
        assert_ne!(text, LAUNCHER, "{from}");
        assert!(Target::parse(text.as_bytes(), home()).is_err(), "{text}");
    }
    assert!(Target::parse(b"[]", home()).is_err());
    assert!(Target::parse(&[b' '; 4097], home()).is_err());
}

#[test]
fn a_refused_target_does_not_echo_its_values() {
    for (text, from) in [
        (LAUNCHER, "192.0.2.10"),
        (LAUNCHER, "~/.ssh/deploy"),
        (LAUNCHER, "https://mx.example.org/.well-known/jmap"),
        (HOST, "/home/alice"),
        (HOST, "mx.example.org"),
    ] {
        let secret = text.replace(from, "-secret-%/../x");
        let error = if text == HOST {
            HostConfig::parse(secret.as_bytes())
                .unwrap_err()
                .to_string()
        } else {
            Target::parse(secret.as_bytes(), home())
                .unwrap_err()
                .to_string()
        };
        assert!(!error.contains("secret"), "{error}");
    }
    let scratch = TempDir::new("deploy-target").unwrap();
    let missing = scratch.path().join("absent-secret.json");
    let error = Target::load(Some(&missing), home(), None)
        .unwrap_err()
        .to_string();
    assert!(
        !error.contains("secret") && error.contains("PRODUCTION.md"),
        "{error}"
    );
}

#[test]
fn the_launcher_target_is_found_under_the_configuration_directory() {
    let scratch = TempDir::new("deploy-target-xdg").unwrap();
    let base = scratch.path();
    let xdg = base.join("xdg");
    fs::create_dir_all(xdg.join("td-mta-deploy")).unwrap();
    fs::write(xdg.join("td-mta-deploy/target.json"), LAUNCHER).unwrap();
    let home = base.join("home");
    fs::create_dir_all(home.join(".config/td-mta-deploy")).unwrap();
    let other = LAUNCHER.replace("192.0.2.10", "192.0.2.20");
    fs::write(home.join(".config/td-mta-deploy/target.json"), &other).unwrap();
    let found = |xdg: Option<&Path>| Target::load(None, &home, xdg).unwrap().destination;
    assert_eq!(found(Some(&xdg)), "alice@192.0.2.10");
    assert_eq!(found(None), "alice@192.0.2.20");
    // A relative XDG_CONFIG_HOME is ignored, as the base directory rules say.
    assert_eq!(found(Some(Path::new("xdg"))), "alice@192.0.2.20");
    let explicit = base.join("explicit.json");
    fs::write(&explicit, LAUNCHER.replace("192.0.2.10", "192.0.2.30")).unwrap();
    assert_eq!(
        Target::load(Some(&explicit), &home, Some(&xdg))
            .unwrap()
            .destination,
        "alice@192.0.2.30"
    );
}

#[test]
fn the_host_configuration_names_every_value_and_nothing_else() {
    let host = HostConfig::parse(HOST.as_bytes()).unwrap();
    assert_eq!(
        host,
        HostConfig {
            host_name: "mx1".into(),
            build_account: "alice".into(),
            build_home: "/home/alice".into(),
            public_name: "mx.example.org".into(),
        }
    );
    assert_eq!(host.mirror().unwrap(), Path::new("/home/alice/src/td"));
    for (from, to) in [
        (r#""host_name":"mx1","#, ""),
        (r#""public_name":"#, r#""extra":"x","public_name":"#),
        (r#""mx1""#, r#""mx 1""#),
        (r#""mx1""#, r#""""#),
        (r#""build_account":"alice""#, r#""build_account":"-alice""#),
        ("/home/alice", "home/alice"),
        ("/home/alice", "/home/alice/"),
        ("/home/alice", "/home/../alice"),
        ("/home/alice", "/home/./alice"),
        ("/home/alice", "/home/alice/."),
        ("/home/alice", "/home//alice"),
        ("/home/alice", "/home/al:ice"),
        ("/home/alice", "/"),
        (r#""mx.example.org""#, r#""mx""#),
        (r#""mx.example.org""#, r#""192.0.2.10""#),
        (r#""mx.example.org""#, r#""mx.example.org\r\nX: y""#),
    ] {
        let text = HOST.replacen(from, to, 1);
        assert_ne!(text, HOST, "{from}");
        assert!(HostConfig::parse(text.as_bytes()).is_err(), "{text}");
    }
}

#[test]
fn the_host_configuration_file_must_be_its_owners_private_regular_file() {
    let scratch = TempDir::new("deploy-host-config").unwrap();
    let path = scratch.path().join("host.json");
    fs::write(&path, HOST).unwrap();
    let owner = fs::metadata(&path).unwrap().uid();
    let mode = |m| fs::set_permissions(&path, fs::Permissions::from_mode(m)).unwrap();
    mode(0o600);
    assert_eq!(
        HostConfig::load_file(&path, owner).unwrap().host_name,
        "mx1"
    );
    assert!(HostConfig::load_file(&path, owner.wrapping_add(1)).is_err());
    for refused in [0o640, 0o644, 0o700, 0o4600] {
        mode(refused);
        assert!(HostConfig::load_file(&path, owner).is_err(), "{refused:o}");
    }
    mode(0o600);
    let link = scratch.path().join("link.json");
    std::os::unix::fs::symlink(&path, &link).unwrap();
    assert!(HostConfig::load_file(&link, owner).is_err());
    fs::write(&path, " ".repeat(4097)).unwrap();
    assert!(HostConfig::load_file(&path, owner).is_err());
    assert!(HostConfig::load_file(&scratch.path().join("absent"), owner).is_err());
}
