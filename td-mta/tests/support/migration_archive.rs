//! CLI archive checks run by the mandatory native service leg's private root.

use std::{fs, os::unix::fs::PermissionsExt, path::Path, process::Command};
use td_crypto::{Digest, Sha256};
use td_json::{json, Json};
fn write(path: &Path, bytes: &[u8]) {
    fs::write(path, bytes).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}
fn verify(path: &Path, maximum: &str) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_td-mta"))
        .args(["migration", "verify", "--archive"])
        .arg(path)
        .args(["--max-bytes", maximum])
        .output()
        .unwrap()
}
pub fn exercise() {
    let root = td_fs::TempDir::new("migration-cli").unwrap();
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let source = json!({"version":1,"instance":"123456789abcdef0123456789abcdef0","session":"https://example.test/session","account":"a","username":"user"});
    let mut hash = Sha256::try_new().unwrap();
    hash.update(b"raw").unwrap();
    let body: String = hash
        .finish()
        .unwrap()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let email = |id: &str| json!({"id":id,"blobId":"b","size":3,"receivedAt":"2024-01-02T03:04:05.123Z","mailboxIds":{"inbox":true,"child":true},"keywords":{"$seen":true,"custom":true}});
    let manifest = json!({"version":1,"provisional":true,"coverage":"per-mailbox-query","source":source.clone(),"mailboxState":"m","emailState":"e","mailboxes":[{"id":"inbox","name":"Inbox","parentId":null,"role":"inbox","sortOrder":0,"isSubscribed":true},{"id":"child","name":"Child","parentId":"inbox","role":null,"sortOrder":0,"isSubscribed":false}],"messages":[{"email":email("one"),"body":body.clone()},{"email":email("two"),"body":body.clone()}]});
    write(&root.join("source.json"), source.to_string().as_bytes());
    write(&root.join("manifest.json"), manifest.to_string().as_bytes());
    let raw = root.join(format!("{body}.eml"));
    write(&raw, b"raw");
    let first = verify(root.path(), "1048576");
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let report = td_json::parse_slice(&first.stdout).unwrap();
    assert_eq!(report["messages"].as_u64(), Some(2));
    assert_eq!(report["messageBytes"].as_u64(), Some(6));
    assert_eq!(report["cutoverReady"].as_bool(), Some(false));
    compare_archives(root.path(), &manifest, &body);
    let mut inconsistent = manifest.clone();
    if let Json::Obj(fields) = &mut inconsistent {
        let messages = &mut fields
            .iter_mut()
            .find(|(key, _)| key == "messages")
            .unwrap()
            .1;
        let mut records = messages.as_array().unwrap().to_vec();
        if let Json::Obj(fields) = records.get_mut(1).unwrap() {
            if let Json::Obj(email) =
                &mut fields.iter_mut().find(|(key, _)| key == "email").unwrap().1
            {
                email.iter_mut().find(|(key, _)| key == "size").unwrap().1 = json!(4);
            }
        }
        *messages = json!(records);
    }
    write(
        &root.join("manifest.json"),
        inconsistent.to_string().as_bytes(),
    );
    let inconsistent = verify(root.path(), "1048576");
    assert!(!inconsistent.status.success());
    assert_eq!(inconsistent.stderr, b"migration-body-size\n");
    write(&root.join("manifest.json"), manifest.to_string().as_bytes());
    let lock = td_mta::store_fs::PrivateRoot::open(root.path().to_str().unwrap())
        .unwrap()
        .try_lock()
        .unwrap();
    let blocked = verify(root.path(), "1048576");
    assert!(!blocked.status.success());
    assert_eq!(blocked.stderr, b"migration-archive-locked\n");
    drop(lock);
    write(&root.join("partial"), b"interrupted replacement");
    let resumed = verify(root.path(), "1048576");
    assert!(resumed.status.success());
    assert_eq!(resumed.stdout, first.stdout);
    assert!(!root.join("partial").exists());
    assert_eq!(
        fs::read(root.join("manifest.json")).unwrap(),
        manifest.to_string().as_bytes()
    );
    let limited = verify(root.path(), "1");
    assert!(!limited.status.success());
    assert_eq!(limited.stderr, b"migration-archive-capacity\n");
    write(&raw, b"bad");
    let corrupt = verify(root.path(), "1048576");
    assert!(!corrupt.status.success());
    assert_eq!(corrupt.stderr, b"migration-body-digest\n");
    write(&raw, b"raw");
    let mut changed = source.clone();
    if let Json::Obj(ref mut pairs) = changed {
        for (key, value) in pairs {
            if key == "account" {
                *value = json!("other");
            }
        }
    }
    write(&root.join("source.json"), changed.to_string().as_bytes());
    let mismatch = verify(root.path(), "1048576");
    assert!(!mismatch.status.success());
    assert_eq!(mismatch.stderr, b"migration-source-mismatch\n");
    write(&root.join("source.json"), source.to_string().as_bytes());
    fs::set_permissions(&raw, fs::Permissions::from_mode(0o644)).unwrap();
    let public = verify(root.path(), "1048576");
    assert!(!public.status.success());
    assert_eq!(public.stderr, b"migration-archive-file\n");
    fs::set_permissions(&raw, fs::Permissions::from_mode(0o600)).unwrap();
    fs::remove_file(&raw).unwrap();
    std::os::unix::fs::symlink("source.json", &raw).unwrap();
    let linked = verify(root.path(), "1048576");
    assert!(!linked.status.success());
    assert_eq!(linked.stderr, b"migration-archive-file\n");
    fs::remove_file(&raw).unwrap();
    write(&raw, b"raw");
    write(&root.join("unexpected"), b"x");
    let unknown = verify(root.path(), "1048576");
    assert!(!unknown.status.success());
    assert_eq!(unknown.stderr, b"migration-archive-entry\n");
    fs::remove_file(root.join("unexpected")).unwrap();
    assert!(verify(root.path(), "1048576").status.success());
}

pub fn export_from_td_mta(
    session: &str,
    account: &str,
    password: &[u8],
    ca: &Path,
) -> (Json, Vec<String>) {
    let root = td_fs::TempDir::new("migration-roundtrip").unwrap();
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let archive = root.join("archive");
    fs::create_dir(&archive).unwrap();
    fs::set_permissions(&archive, fs::Permissions::from_mode(0o700)).unwrap();
    let secret = root.join("password");
    write(&secret, password);
    let invoke = |action| {
        Command::new(env!("CARGO_BIN_EXE_td-mta"))
            .args(["migration", action, "--archive"])
            .arg(&archive)
            .args([
                "--max-bytes",
                "104857600",
                "--session",
                session,
                "--account",
                account,
                "--username",
                "user",
                "--password-file",
            ])
            .arg(&secret)
            .arg("--ca-file")
            .arg(ca)
            .output()
            .unwrap()
    };
    let first = invoke("export");
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let second = invoke("export");
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert_eq!(first.stdout, second.stdout);
    let checked = verify(&archive, "104857600");
    assert!(checked.status.success());
    let manifest = td_json::parse_slice(&fs::read(archive.join("manifest.json")).unwrap()).unwrap();
    let inventory = invoke("inventory");
    assert!(
        inventory.status.success(),
        "{}",
        String::from_utf8_lossy(&inventory.stderr)
    );
    let inventory = td_json::parse_slice(&inventory.stdout).unwrap();
    assert_eq!(inventory["status"].as_str(), Some("inventory-matches"));
    assert_eq!(
        inventory["messages"].as_u64(),
        Some(manifest["messages"].as_array().unwrap().len() as u64)
    );
    assert_eq!(inventory["bodyDigestsVerified"].as_bool(), Some(false));
    assert_eq!(inventory["cutoverReady"].as_bool(), Some(false));
    let mut incomplete = manifest.clone();
    if let Json::Obj(fields) = &mut incomplete {
        let records = fields
            .iter_mut()
            .find(|(key, _)| key == "messages")
            .unwrap();
        let mut retained = records.1.as_array().unwrap().to_vec();
        assert!(retained.pop().is_some());
        records.1 = json!(retained);
    }
    write(
        &archive.join("manifest.json"),
        incomplete.to_string().as_bytes(),
    );
    let rejected = invoke("inventory");
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("migration-inventory-mismatch"));
    let mut altered = manifest.clone();
    if let Json::Obj(fields) = &mut altered {
        fields
            .iter_mut()
            .find(|(key, _)| key == "emailState")
            .unwrap()
            .1 = json!("stale-state");
    }
    write(
        &archive.join("manifest.json"),
        altered.to_string().as_bytes(),
    );
    let rejected = invoke("inventory");
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("migration-source-changed"));
    write(
        &archive.join("manifest.json"),
        manifest.to_string().as_bytes(),
    );
    let mut digests = Vec::new();
    for record in manifest["messages"].as_array().unwrap() {
        let digest = record["body"].as_str().unwrap();
        let raw = fs::read(archive.join(format!("{digest}.eml"))).unwrap();
        let mut hash = Sha256::try_new().unwrap();
        hash.update(&raw).unwrap();
        let actual: String = hash
            .finish()
            .unwrap()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(actual, digest);
        digests.push(actual);
    }
    (manifest, digests)
}

fn compare_archives(left: &Path, manifest: &Json, body: &str) {
    let right = td_fs::TempDir::new("migration-compare").unwrap();
    fs::set_permissions(right.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let source = json!({"version":1,"instance":"fedcba9876543210fedcba9876543210","session":"https://target.example.test/session","account":"different-account","username":"target-user"});
    let mut target = manifest.clone();
    if let Json::Obj(fields) = &mut target {
        fields
            .iter_mut()
            .find(|(key, _)| key == "source")
            .unwrap()
            .1 = source.clone();
    }
    write(&right.join("source.json"), source.to_string().as_bytes());
    write(&right.join("manifest.json"), target.to_string().as_bytes());
    let raw = right.join(format!("{body}.eml"));
    write(&raw, b"raw");
    let invoke = || {
        Command::new(env!("CARGO_BIN_EXE_td-mta"))
            .args(["migration", "compare", "--archive"])
            .arg(left)
            .arg("--against-archive")
            .arg(right.path())
            .args(["--max-bytes", "1048576"])
            .output()
            .unwrap()
    };
    let result = invoke();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let report = td_json::parse_slice(&result.stdout).unwrap();
    assert_eq!(report["status"].as_str(), Some("archives-match"));
    assert_eq!(report["messages"].as_u64(), Some(2));
    assert_eq!(report["bodyDigestsVerified"].as_bool(), Some(true));
    assert_eq!(report["cutoverReady"].as_bool(), Some(false));
    write(&raw, b"bad");
    let bad = invoke();
    assert!(!bad.status.success());
    assert_eq!(bad.stderr, b"migration-body-digest\n");
    write(&raw, b"raw");
    let mut incomplete = target.clone();
    if let Json::Obj(fields) = &mut incomplete {
        fields
            .iter_mut()
            .find(|(key, _)| key == "messages")
            .unwrap()
            .1 = json!([target["messages"].as_array().unwrap().first().unwrap()]);
    }
    write(
        &right.join("manifest.json"),
        incomplete.to_string().as_bytes(),
    );
    let bad = invoke();
    assert!(!bad.status.success());
    assert_eq!(bad.stderr, b"migration-archives-differ\n");
    write(&right.join("manifest.json"), target.to_string().as_bytes());
    let lock = td_mta::store_fs::PrivateRoot::open(right.path().to_str().unwrap())
        .unwrap()
        .try_lock()
        .unwrap();
    let blocked = invoke();
    assert!(!blocked.status.success());
    assert_eq!(blocked.stderr, b"migration-archive-locked\n");
    drop(lock);
    assert_eq!(
        fs::read(left.join("manifest.json")).unwrap(),
        manifest.to_string().as_bytes()
    );
}
