#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
use super::*;
use std::path::PathBuf;

struct Fixture {
    dir: crate::test_temp::TempDir,
    root: PathBuf,
    repo: PathBuf,
    key: PathBuf,
    private: PathBuf,
    state: PathBuf,
    cache: PathBuf,
    deployment: PathBuf,
    release: String,
}
impl Fixture {
    fn new() -> Self {
        let dir = crate::test_temp::TempDir::new("td-release-test").unwrap();
        let root = dir.join("root");
        let repo = dir.join("repo");
        let key = dir.join("release.key");
        let private = dir.join("root.key");
        let public = dir.join("release.pub");
        let (secret, _) = sig::keygen().unwrap();
        fs::write(&private, secret).unwrap();
        let (secret, bytes) = sig::keygen().unwrap();
        fs::write(&key, secret).unwrap();
        fs::write(&public, sig::to_hex(&bytes)).unwrap();
        let time = now().unwrap();
        publisher::root(
            1,
            time + 365 * format::DAY,
            &private,
            &public,
            &private,
            &root,
        )
        .unwrap();
        let deployment = dir.join("input");
        fs::create_dir(&deployment).unwrap();
        let mut manifest = String::from("td-deployment-v1\n");
        for (i, (name, _)) in format::FILES.iter().enumerate() {
            let bytes: Vec<_> = (0..700_000)
                .map(|n| td_engine::image_chunks::gear(((n + i) % 251) as u8) as u8)
                .collect();
            fs::write(deployment.join(name), &bytes).unwrap();
            manifest.push_str(&format!("{}  {name}\n", hash(&bytes)));
        }
        fs::write(deployment.join("manifest"), manifest).unwrap();
        publisher::publish(
            &repo,
            &root,
            &key,
            "system-x86-64",
            &"1".repeat(40),
            1,
            time,
            time + format::DAY * 2,
            &deployment,
        )
        .unwrap();
        let release = fs::read_dir(repo.join("releases"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .file_name()
            .into_string()
            .unwrap();
        publisher::promote(
            &repo,
            &root,
            &key,
            "system-x86-64",
            &release,
            1,
            time,
            time + 3600,
        )
        .unwrap();
        let state = dir.join("state");
        let trust = State::init(&text(&root, SMALL).unwrap(), "system-x86-64", time).unwrap();
        replace(&state, trust.text().as_bytes()).unwrap();
        let cache = dir.join("cache");
        Self {
            dir,
            root,
            repo,
            key,
            private,
            state,
            cache,
            deployment,
            release,
        }
    }
    fn base(&self) -> String {
        format!("file:{}", self.repo.display())
    }
    fn fetch(&self, name: &str) -> Result<()> {
        fetch(&self.base(), &self.state, &self.cache, &self.dir.join(name))
    }
}

#[test]
fn reconstructed_bundle_verifies_and_corrupt_cache_is_repaired() {
    let f = Fixture::new();
    f.fetch("first").unwrap();
    let receipt = verify(&f.state, &f.dir.join("first")).unwrap();
    assert_eq!(receipt.0, "1".repeat(40));
    assert_eq!(
        receipt.1,
        hash(&read(&f.deployment.join("manifest"), 4096).unwrap())
    );
    for (name, _) in format::FILES {
        assert_eq!(
            fs::read(f.dir.join("first").join(name)).unwrap(),
            fs::read(f.deployment.join(name)).unwrap()
        );
    }
    let entry = fs::read_dir(&f.cache)
        .unwrap()
        .map(|e| e.unwrap())
        .find(|e| e.file_name() != "lock")
        .unwrap();
    fs::write(entry.path(), b"corrupt").unwrap();
    f.fetch("second").unwrap();
    assert_eq!(
        hash(&fs::read(entry.path()).unwrap()),
        entry.file_name().to_str().unwrap()
    );
    fs::write(f.dir.join("second/bzImage"), b"invalid").unwrap();
    assert!(verify(&f.state, &f.dir.join("second"))
        .unwrap_err()
        .contains("size mismatch"));
}

#[test]
fn corrupt_or_missing_objects_leave_no_bundle_and_resume_keeps_verified_objects() {
    let f = Fixture::new();
    let root = Root::parse(&text(&f.root, SMALL).unwrap(), None).unwrap();
    let release = Release::parse(
        &text(&f.repo.join("releases").join(&f.release), INDEX_LIMIT).unwrap(),
        &root,
        now().unwrap(),
        "system-x86-64",
    )
    .unwrap();
    let objects = &release.payloads[0].objects;
    let second = &objects[1].digest;
    let missing = f.repo.join("objects").join(second);
    let saved = fs::read(&missing).unwrap();
    fs::remove_file(&missing).unwrap();
    assert!(f.fetch("out").is_err());
    assert!(!f.dir.join("out").exists());
    assert!(f.cache.join(&objects[0].digest).exists());
    fs::write(&missing, b"corrupt").unwrap();
    assert!(f
        .fetch("out")
        .unwrap_err()
        .contains("hash or length mismatch"));
    fs::write(&missing, saved).unwrap();
    f.fetch("out").unwrap();
}

#[test]
fn expiry_profile_replay_equivocation_and_clock_rollback_refuse() {
    let f = Fixture::new();
    let mut state = State::parse(&text(&f.state, SMALL * 3).unwrap()).unwrap();
    let head = text(&f.repo.join("heads/system-x86-64"), SMALL).unwrap();
    let release = text(&f.repo.join("releases").join(&f.release), INDEX_LIMIT).unwrap();
    let time = now().unwrap();
    assert!(metadata(&mut state, &head, &release, time + 3601).is_err());
    assert!(Head::parse(&head, &state.root, time, "server-x86-64").is_err());
    metadata(&mut state, &head, &release, time).unwrap();
    assert!(metadata(&mut state, &head, &release, time - 1).is_err());
    publisher::promote(
        &f.repo,
        &f.root,
        &f.key,
        "system-x86-64",
        &f.release,
        2,
        time,
        time + 3500,
    )
    .unwrap();
    let next = text(&f.repo.join("heads/system-x86-64"), SMALL).unwrap();
    metadata(&mut state, &next, &release, time).unwrap();
    assert!(metadata(&mut state, &head, &release, time)
        .err()
        .unwrap()
        .contains("replay"));
    let forged_body = next
        .lines()
        .take(7)
        .collect::<Vec<_>>()
        .join("\n")
        .replace(&(time + 3500).to_string(), &(time + 3400).to_string())
        + "\n";
    let equivocation = format::signed(&forged_body, &fs::read(&f.key).unwrap()).unwrap();
    assert!(metadata(&mut state, &equivocation, &release, time)
        .err()
        .unwrap()
        .contains("equivocation"));
    assert!(publisher::promote(
        &f.repo,
        &f.root,
        &f.key,
        "system-x86-64",
        &f.release,
        1,
        time,
        time + 3500
    )
    .is_err());
}

#[test]
fn sequential_root_rotation_revokes_old_key_even_when_later_metadata_fails() {
    let f = Fixture::new();
    let time = now().unwrap();
    let new_private = f.dir.join("new-root.key");
    let new_public = f.dir.join("new-release.pub");
    fs::write(&new_private, sig::keygen().unwrap().0).unwrap();
    fs::write(&new_public, sig::to_hex(&sig::keygen().unwrap().1)).unwrap();
    let rotated_path = f.dir.join("rotated");
    publisher::root(
        2,
        time + format::DAY,
        &new_private,
        &new_public,
        &f.private,
        &rotated_path,
    )
    .unwrap();
    let rotated = text(&rotated_path, SMALL).unwrap();
    let mut state = State::parse(&text(&f.state, SMALL * 3).unwrap()).unwrap();
    advance_roots(
        &mut state,
        2,
        |_| Ok(rotated.clone()),
        |s| replace(&f.state, s.text().as_bytes()),
    )
    .unwrap();
    let persisted = State::parse(&text(&f.state, SMALL * 3).unwrap()).unwrap();
    assert_eq!(persisted.root.version, 2);
    let old_head = text(&f.repo.join("heads/system-x86-64"), SMALL).unwrap();
    assert!(Head::parse(&old_head, &persisted.root, time, "system-x86-64").is_err());
    assert!(advance_roots(&mut state, 1, |_| panic!(), |_| panic!()).is_err());
    let unrelated = Fixture::new();
    assert!(Root::parse(
        &rotated,
        Some(&Root::parse(&text(&unrelated.root, SMALL).unwrap(), None).unwrap())
    )
    .is_err());
    assert!(advance_roots(&mut state, 100, |_| panic!(), |_| panic!()).is_err());
}

#[test]
fn parser_rejects_bad_signatures_and_signed_bad_indexes() {
    let f = Fixture::new();
    let root = Root::parse(&text(&f.root, SMALL).unwrap(), None).unwrap();
    let signed = text(&f.repo.join("releases").join(&f.release), INDEX_LIMIT).unwrap();
    let changed = signed.replace("file bzImage", "file ../bad");
    assert!(Release::parse(&changed, &root, now().unwrap(), "system-x86-64").is_err());
    let body = signed
        .rsplit_once('\n')
        .unwrap()
        .0
        .rsplit_once('\n')
        .unwrap()
        .0
        .to_string()
        + "\n";
    for bad in [
        body.replace("file bzImage", "file ../bad"),
        body.replace("700000", "9999999999999"),
        body.replace("file bzImage 700000", "file bzImage 0700000"),
    ] {
        let signed = format::signed(&bad, &fs::read(&f.key).unwrap()).unwrap();
        assert!(Release::parse(&signed, &root, now().unwrap(), "system-x86-64").is_err());
    }
    assert!(Source::new("http://example.com/repo").is_err());
    assert!(Source::new("https://user@example.com/repo").is_err());
    assert!(Source::new("https://example.com/repo?token=x").is_err());
}

struct Server {
    base: String,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    requests: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl Server {
    fn start(repo: PathBuf, broken: Option<String>) -> Self {
        use std::sync::{
            atomic::{AtomicBool, Ordering},
            Arc, Mutex,
        };
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stopped = stop.clone();
        let seen = requests.clone();
        let worker = std::thread::spawn(move || {
            while !stopped.load(Ordering::Relaxed) {
                let (mut stream, _) = match listener.accept() {
                    Ok(pair) => pair,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(e) => panic!("accept: {e}"),
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut request = Vec::new();
                let mut byte = [0];
                while request.len() < 4096 && !request.ends_with(b"\r\n\r\n") {
                    if stream.read(&mut byte).unwrap_or(0) == 0 {
                        break;
                    }
                    request.extend(byte);
                }
                let request = String::from_utf8(request).unwrap();
                let path = request
                    .split_whitespace()
                    .nth(1)
                    .unwrap()
                    .trim_start_matches('/');
                assert!(!path.contains(".."));
                seen.lock().unwrap().push(path.to_string());
                let body = fs::read(repo.join(path)).unwrap();
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .unwrap();
                let body = if broken.as_deref() == Some(path) {
                    &body[..body.len() / 2]
                } else {
                    &body
                };
                let _ = stream.write_all(body);
            }
        });
        Self {
            base,
            stop,
            requests,
            worker: Some(worker),
        }
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            worker.join().unwrap();
        }
    }
}

#[test]
fn http_interruption_resumes_and_a_warm_transfer_requests_only_metadata() {
    let f = Fixture::new();
    let root = Root::parse(&text(&f.root, SMALL).unwrap(), None).unwrap();
    let release = Release::parse(
        &text(&f.repo.join("releases").join(&f.release), INDEX_LIMIT).unwrap(),
        &root,
        now().unwrap(),
        "system-x86-64",
    )
    .unwrap();
    let first = release.payloads[0].objects[0].digest.clone();
    let broken = format!("objects/{}", release.payloads[0].objects[1].digest);
    let server = Server::start(f.repo.clone(), Some(broken));
    assert!(fetch(&server.base, &f.state, &f.cache, &f.dir.join("out")).is_err());
    assert!(!f.dir.join("out").exists());
    assert!(f.cache.join(&first).exists());
    drop(server);
    let server = Server::start(f.repo.clone(), None);
    fetch(&server.base, &f.state, &f.cache, &f.dir.join("out")).unwrap();
    assert!(!server
        .requests
        .lock()
        .unwrap()
        .contains(&format!("objects/{first}")));
    server.requests.lock().unwrap().clear();
    fetch(&server.base, &f.state, &f.cache, &f.dir.join("warm")).unwrap();
    assert_eq!(
        *server.requests.lock().unwrap(),
        vec![
            "heads/system-x86-64".to_string(),
            format!("releases/{}", f.release)
        ]
    );
    verify(&f.state, &f.dir.join("warm")).unwrap();
}

#[test]
fn output_space_failure_is_reported_and_verified_cache_survives() {
    struct Full;
    impl Write for Full {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::from_raw_os_error(28))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let f = Fixture::new();
    let root = Root::parse(&text(&f.root, SMALL).unwrap(), None).unwrap();
    let release = Release::parse(
        &text(&f.repo.join("releases").join(&f.release), INDEX_LIMIT).unwrap(),
        &root,
        now().unwrap(),
        "system-x86-64",
    )
    .unwrap();
    directory(&f.cache).unwrap();
    let mut source = Source::new(&f.base()).unwrap();
    let mut cache = cache::Cache::open(&f.cache, cache::DEFAULT_LIMIT, source.deadline).unwrap();
    let error = assemble(
        &release.payloads[0],
        &mut cache,
        &seed::Seed::default(),
        &mut source,
        &mut Full,
    )
    .unwrap_err();
    assert!(error.contains("space"), "{error}");
    assert!(f
        .cache
        .join(&release.payloads[0].objects[0].digest)
        .exists());
    f.fetch("retry").unwrap();
}

#[test]
fn collection_retains_current_objects_and_graces_recent_orphans() {
    let f = Fixture::new();
    let recent = f.repo.join("objects").join(hash(b"recent orphan"));
    let old = f.repo.join("objects").join(hash(b"old orphan"));
    fs::write(&recent, b"recent orphan").unwrap();
    fs::write(&old, b"old orphan").unwrap();
    File::open(&old)
        .unwrap()
        .set_times(
            std::fs::FileTimes::new()
                .set_modified(SystemTime::now() - Duration::from_secs(8 * format::DAY)),
        )
        .unwrap();
    publisher::gc(&f.repo, &f.root, false).unwrap();
    assert!(old.exists());
    publisher::gc(&f.repo, &f.root, true).unwrap();
    assert!(!old.exists());
    assert!(recent.exists());
    f.fetch("after-gc").unwrap();
}

#[test]
fn output_suffix_is_appended_and_does_not_delete_a_sibling() {
    let f = Fixture::new();
    fs::create_dir(f.dir.join("bundle.partial")).unwrap();
    fs::write(f.dir.join("bundle.partial/sentinel"), b"unrelated").unwrap();
    f.fetch("bundle.v2").unwrap();
    assert_eq!(
        fs::read(f.dir.join("bundle.partial/sentinel")).unwrap(),
        b"unrelated"
    );
    assert!(!f.dir.join("bundle.v2.partial").exists());
}

#[test]
fn missing_selected_index_refuses_collection_before_any_deletion() {
    let f = Fixture::new();
    let objects: Vec<_> = fs::read_dir(f.repo.join("objects"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    for object in &objects {
        File::open(object)
            .unwrap()
            .set_times(
                std::fs::FileTimes::new()
                    .set_modified(SystemTime::now() - Duration::from_secs(8 * format::DAY)),
            )
            .unwrap();
    }
    fs::remove_file(f.repo.join("releases").join(&f.release)).unwrap();
    assert!(publisher::gc(&f.repo, &f.root, true)
        .unwrap_err()
        .contains("selected release index missing"));
    assert!(objects.iter().all(|p| p.exists()));
}

#[test]
fn root_epoch_recovers_poisoned_counters_and_retry_retains_the_rotation_proof() {
    let f = Fixture::new();
    let time = now().unwrap();
    let mut state = State::parse(&text(&f.state, SMALL * 3).unwrap()).unwrap();
    let original = text(&f.repo.join("releases").join(&f.release), INDEX_LIMIT).unwrap();
    let mut body: Vec<_> = original.lines().map(String::from).collect();
    body.pop();
    body[2] = u64::MAX.to_string();
    let poisoned_release =
        format::signed(&(body.join("\n") + "\n"), &fs::read(&f.key).unwrap()).unwrap();
    let digest = hash(poisoned_release.as_bytes());
    fs::write(f.repo.join("releases").join(&digest), &poisoned_release).unwrap();
    let poisoned_head = format::signed(
        &format!(
            "td-release-head-v1\n1\n{}\n{time}\n{}\nsystem-x86-64\n{digest}\n",
            u64::MAX,
            time + 3600
        ),
        &fs::read(&f.key).unwrap(),
    )
    .unwrap();
    fs::write(f.repo.join("heads/system-x86-64"), &poisoned_head).unwrap();
    metadata(&mut state, &poisoned_head, &poisoned_release, time).unwrap();
    save_state(&f.state, &state).unwrap();
    let independent = f.dir.join("privileged-state");
    save_state(&independent, &state).unwrap();
    let failure_state = f.dir.join("privileged-failure-state");
    save_state(&failure_state, &state).unwrap();
    let new_private = f.dir.join("new-root.key");
    let new_public = f.dir.join("new-release.pub");
    let new_key = f.dir.join("new-release.key");
    fs::write(&new_private, sig::keygen().unwrap().0).unwrap();
    let (secret, public) = sig::keygen().unwrap();
    fs::write(&new_key, secret).unwrap();
    fs::write(&new_public, sig::to_hex(&public)).unwrap();
    let new_root = f.dir.join("new-root");
    publisher::root(
        2,
        time + format::DAY,
        &new_private,
        &new_public,
        &f.private,
        &new_root,
    )
    .unwrap();
    publisher::publish(
        &f.repo,
        &new_root,
        &new_key,
        "system-x86-64",
        &"2".repeat(40),
        1,
        time,
        time + 7200,
        &f.deployment,
    )
    .unwrap();
    let new_release = fs::read_dir(f.repo.join("releases"))
        .unwrap()
        .map(|e| e.unwrap())
        .find(|e| text(&e.path(), INDEX_LIMIT).unwrap().lines().nth(1) == Some("2"))
        .unwrap();
    let digest = new_release.file_name().into_string().unwrap();
    publisher::promote(
        &f.repo,
        &new_root,
        &new_key,
        "system-x86-64",
        &digest,
        1,
        time,
        time + 3600,
    )
    .unwrap();
    let object = fs::read_dir(f.repo.join("objects"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let bytes = fs::read(&object).unwrap();
    fs::remove_file(&object).unwrap();
    assert!(f.fetch("out").is_err());
    learn(&failure_state, &sidecar(&f.state, ".metadata")).unwrap();
    let learned = State::parse(&text(&failure_state, SMALL * 3).unwrap()).unwrap();
    assert_eq!(learned.root.version, 2);
    assert_eq!(learned.sequence, 1);
    let new_head = fs::read(f.repo.join("heads/system-x86-64")).unwrap();
    fs::write(f.repo.join("heads/system-x86-64"), &poisoned_head).unwrap();
    // Exactly the admin retry: its worker receives only privileged state.
    replace(&f.state, learned.text().as_bytes()).unwrap();
    assert!(f.fetch("out").unwrap_err().contains("root rollback"));
    fs::write(f.repo.join("heads/system-x86-64"), new_head).unwrap();
    assert_eq!(
        State::parse(&text(&f.state, SMALL * 3).unwrap())
            .unwrap()
            .root
            .version,
        2
    );
    fs::write(&object, bytes).unwrap();
    f.fetch("out").unwrap();
    assert!(f.dir.join("out/root-2").exists());
    assert_eq!(
        verify(&independent, &f.dir.join("out")).unwrap().0,
        "2".repeat(40)
    );
}

#[test]
fn learning_expired_signed_evidence_preserves_floors_but_cannot_install_it() {
    let f = Fixture::new();
    let time = now().unwrap();
    let evidence = f.dir.join("evidence");
    directory(&evidence).unwrap();
    let original = text(&f.repo.join("releases").join(&f.release), INDEX_LIMIT).unwrap();
    let mut body: Vec<_> = original.lines().map(String::from).collect();
    body.pop();
    body[2] = "2".into();
    body[3] = (time - 7200).to_string();
    body[4] = (time + 7200).to_string();
    let release = format::signed(&(body.join("\n") + "\n"), &fs::read(&f.key).unwrap()).unwrap();
    let head = format::signed(
        &format!(
            "td-release-head-v1\n1\n2\n{}\n{}\nsystem-x86-64\n{}\n",
            time - 7200,
            time - 3600,
            hash(release.as_bytes())
        ),
        &fs::read(&f.key).unwrap(),
    )
    .unwrap();
    fs::write(evidence.join("head"), &head).unwrap();
    fs::write(evidence.join("release"), &release).unwrap();
    learn(&f.state, &evidence).unwrap();
    let mut state = State::parse(&text(&f.state, SMALL * 3).unwrap()).unwrap();
    assert_eq!((state.head_version, state.sequence), (2, 2));
    let time = now().unwrap();
    assert!(metadata(&mut state, &head, &release, time).is_err());
    let old = text(&f.repo.join("heads/system-x86-64"), SMALL).unwrap();
    assert!(metadata(&mut state, &old, &original, time)
        .err()
        .unwrap()
        .contains("replay"));
    // A forged higher counter has no effect; learning only parses signed proofs.
    fs::write(evidence.join("head"), head.replace("\n2\n", "\n99\n")).unwrap();
    learn(&f.state, &evidence).unwrap();
    assert_eq!(
        State::parse(&text(&f.state, SMALL * 3).unwrap())
            .unwrap()
            .head_version,
        2
    );
}

#[test]
fn learning_cannot_hide_a_protected_state_persistence_failure() {
    let f = Fixture::new();
    let evidence = f.dir.join("evidence");
    directory(&evidence).unwrap();
    fs::copy(f.repo.join("heads/system-x86-64"), evidence.join("head")).unwrap();
    fs::copy(
        f.repo.join("releases").join(&f.release),
        evidence.join("release"),
    )
    .unwrap();
    let before = fs::read(&f.state).unwrap();
    fs::write(
        sidecar(&f.state, ".roots"),
        b"blocks protected history publication",
    )
    .unwrap();
    assert!(learn(&f.state, &evidence).is_err());
    assert_eq!(fs::read(&f.state).unwrap(), before);
}

#[test]
fn explicit_partitions_round_trip_through_the_same_signed_downloader() {
    let f = Fixture::new();
    let plans = f.dir.join("partitions");
    directory(&plans).unwrap();
    for (name, _) in format::FILES {
        fs::write(
            plans.join(name),
            "131072\n131072\n131072\n131072\n131072\n44640\n",
        )
        .unwrap();
    }
    let time = now().unwrap();
    publisher::publish_with_partitions(
        &f.repo,
        &f.root,
        &f.key,
        "system-x86-64",
        &"2".repeat(40),
        2,
        time,
        time + 7200,
        &f.deployment,
        Some(&plans),
    )
    .unwrap();
    let entry = fs::read_dir(f.repo.join("releases"))
        .unwrap()
        .map(|e| e.unwrap())
        .find(|e| text(&e.path(), INDEX_LIMIT).unwrap().lines().nth(2) == Some("2"))
        .unwrap();
    publisher::promote(
        &f.repo,
        &f.root,
        &f.key,
        "system-x86-64",
        entry.file_name().to_str().unwrap(),
        2,
        time,
        time + 3600,
    )
    .unwrap();
    f.fetch("partitioned").unwrap();
    verify(&f.state, &f.dir.join("partitioned")).unwrap();
    for bad in ["0\n", "262145\n", "1\n", "700000", "01\n"] {
        fs::write(plans.join("bzImage"), bad).unwrap();
        assert!(publisher::publish_with_partitions(
            &f.repo,
            &f.root,
            &f.key,
            "system-x86-64",
            &"3".repeat(40),
            3,
            time,
            time + 7200,
            &f.deployment,
            Some(&plans)
        )
        .is_err());
    }
}

#[test]
fn local_seed_completes_a_cold_fetch_without_repository_objects() {
    let f = Fixture::new();
    fs::remove_dir_all(f.repo.join("objects")).unwrap();
    assert!(f.fetch("cold").is_err());
    fetch_seeded(
        &f.base(),
        &f.state,
        &f.cache,
        &f.dir.join("seeded"),
        Some(&f.deployment),
    )
    .unwrap();
    verify(&f.state, &f.dir.join("seeded")).unwrap();
    for (name, _) in format::FILES {
        assert_eq!(
            fs::read(f.dir.join("seeded").join(name)).unwrap(),
            fs::read(f.deployment.join(name)).unwrap()
        );
    }
    // A complete verified cache avoids reopening the seed on a later update.
    fetch_seeded(
        &f.base(),
        &f.state,
        &f.cache,
        &f.dir.join("warm"),
        Some(&f.dir.join("absent")),
    )
    .unwrap();
}

#[test]
fn local_seed_does_not_authorize_changed_payload_bytes() {
    let f = Fixture::new();
    fs::write(f.deployment.join("bzImage"), vec![0u8; 700_000]).unwrap();
    fs::remove_dir_all(f.repo.join("objects")).unwrap();
    assert!(fetch_seeded(
        &f.base(),
        &f.state,
        &f.cache,
        &f.dir.join("bad"),
        Some(&f.deployment),
    )
    .is_err());
    assert!(!f.dir.join("bad").exists());
}

#[test]
fn local_seed_refuses_links_and_oversized_files_before_reading() {
    let f = Fixture::new();
    let path = f.deployment.join("bzImage");
    fs::remove_file(&path).unwrap();
    std::os::unix::fs::symlink("initramfs.cpio", &path).unwrap();
    assert!(fetch_seeded(
        &f.base(),
        &f.state,
        &f.cache,
        &f.dir.join("linked"),
        Some(&f.deployment),
    )
    .is_err());
    fs::remove_file(&path).unwrap();
    File::create(&path)
        .unwrap()
        .set_len(256 * 1024 * 1024 + 1)
        .unwrap();
    assert!(fetch_seeded(
        &f.base(),
        &f.state,
        &f.cache,
        &f.dir.join("large"),
        Some(&f.deployment),
    )
    .unwrap_err()
    .contains("seed payload size exceeds bound"));
}

#[test]
fn cache_budget_smaller_than_image_still_reuses_the_local_seed() {
    for limit in [td_engine::image_chunks::MAX as u64, 0] {
        let f = Fixture::new();
        f.fetch("initial").unwrap();
        fs::remove_dir_all(f.repo.join("objects")).unwrap();
        fetch_budgeted(
            &f.base(),
            &f.state,
            &f.cache,
            &f.dir.join("limited"),
            Some(&f.deployment),
            limit,
        )
        .unwrap();
        verify(&f.state, &f.dir.join("limited")).unwrap();
        let bytes: u64 = fs::read_dir(&f.cache)
            .unwrap()
            .map(|e| e.unwrap())
            .filter(|e| e.file_name() != "lock")
            .map(|e| e.metadata().unwrap().len())
            .sum();
        assert!(bytes <= limit, "{bytes} > {limit}");
    }
}

#[test]
fn cache_evicts_the_least_recently_used_object() {
    let owned = crate::test_temp::TempDir::new("release-cache-lru").unwrap();
    let path = owned.join("cache");
    directory(&path).unwrap();
    let a = vec![1; 100];
    let b = vec![2; 100];
    let c = vec![3; 100];
    for (bytes, age) in [(&a, 1), (&b, 2)] {
        let entry = path.join(hash(bytes));
        fs::write(&entry, bytes).unwrap();
        File::open(&entry)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(age)))
            .unwrap();
    }
    let mut cache =
        cache::Cache::open(&path, 200, Instant::now() + Duration::from_secs(10)).unwrap();
    assert_eq!(cache.get(&hash(&a), 100).unwrap(), Some(a.clone()));
    cache.put(&hash(&c), &c).unwrap();
    assert!(path.join(hash(&a)).exists());
    assert!(!path.join(hash(&b)).exists());
    assert!(path.join(hash(&c)).exists());
}

#[test]
fn cache_reclaims_only_its_owned_names_and_never_follows_links() {
    let owned = crate::test_temp::TempDir::new("release-cache-cleanup").unwrap();
    let path = owned.join("cache");
    directory(&path).unwrap();
    let outside = owned.join("outside");
    fs::write(&outside, b"preserve").unwrap();
    let digest = "a".repeat(64);
    std::os::unix::fs::symlink(&outside, path.join(&digest)).unwrap();
    fs::write(path.join(format!(".{digest}.12.0.tmp")), b"partial").unwrap();
    cache::Cache::open(&path, 0, Instant::now() + Duration::from_secs(10)).unwrap();
    assert_eq!(fs::read_dir(&path).unwrap().count(), 0);
    assert_eq!(fs::read(&outside).unwrap(), b"preserve");
    fs::write(path.join("unrelated"), b"preserve too").unwrap();
    assert!(cache::Cache::open(&path, 0, Instant::now() + Duration::from_secs(10)).is_err());
    assert_eq!(fs::read(path.join("unrelated")).unwrap(), b"preserve too");
}

#[test]
fn seed_bytes_are_reverified_after_the_scan() {
    use std::os::unix::fs::FileExt;
    let f = Fixture::new();
    directory(&f.cache).unwrap();
    let root = Root::parse(&text(&f.root, SMALL).unwrap(), None).unwrap();
    let release = Release::parse(
        &text(&f.repo.join("releases").join(&f.release), INDEX_LIMIT).unwrap(),
        &root,
        now().unwrap(),
        "system-x86-64",
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut cache = cache::Cache::open(&f.cache, 0, deadline).unwrap();
    let seed = seed::scan(&release, &mut cache, &f.deployment, deadline).unwrap();
    let object = &release.payloads[0].objects[0];
    let bytes = seed.get(&object.digest, object.size).unwrap().unwrap();
    OpenOptions::new()
        .write(true)
        .open(f.deployment.join("bzImage"))
        .unwrap()
        .write_all_at(&[bytes[0] ^ 0xff], 0)
        .unwrap();
    assert!(seed
        .get(&object.digest, object.size)
        .unwrap_err()
        .contains("changed after scanning"));
}
