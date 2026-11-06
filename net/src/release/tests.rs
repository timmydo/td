#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
use super::*;
use std::path::PathBuf;

#[test]
fn explicit_reanchor_creates_new_trust_history_without_mutating_the_old_one() {
    let old = Fixture::new();
    let new = Fixture::new();
    let mut previous = State::parse(&text(&old.state, SMALL * 3).unwrap()).unwrap();
    previous.head_version = 99;
    previous.sequence = 77;
    previous.profile = "server-x86-64".into();
    save_state(&old.state, &previous).unwrap();
    let before = fs::read(&old.state).unwrap();
    let root = text(&new.root, SMALL).unwrap();
    assert!(format::Root::parse(&root, Some(&previous.root)).is_err());
    let output = old.dir.join("replacement");
    let args = vec![
        "release".into(),
        "reanchor".into(),
        old.state.to_str().unwrap().into(),
        new.root.to_str().unwrap().into(),
        output.to_str().unwrap().into(),
    ];
    command(&args).unwrap();
    let changed = State::parse(&text(&output, SMALL * 3).unwrap()).unwrap();
    assert_eq!(changed.root.text, root);
    assert_eq!(changed.profile, "server-x86-64");
    assert_eq!((changed.head_version, changed.sequence), (0, 0));
    assert_eq!(fs::read(&old.state).unwrap(), before);
    assert!(command(&args).is_err());
    fs::remove_file(&output).unwrap();
    fs::write(&new.root, "not a signed root\n").unwrap();
    assert!(command(&args).is_err());
    assert!(!output.exists());
}

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
    assert_eq!(
        fs::read_dir(&f.cache).unwrap().count(),
        1,
        "only the cache lease remains"
    );
    fetch_seeded(
        &f.base(),
        &f.state,
        &f.cache,
        &f.dir.join("repeat"),
        Some(&f.deployment),
    )
    .unwrap();
}

#[test]
fn a_complete_download_cache_avoids_reopening_the_seed() {
    let f = Fixture::new();
    f.fetch("initial").unwrap();
    fs::remove_dir_all(f.repo.join("objects")).unwrap();
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

#[test]
fn operator_commands_stage_and_explicitly_promote_with_monotonic_counters() {
    let f = Fixture::new();
    let profile = "system-x86-64";
    let original = fs::read(f.repo.join("heads").join(profile)).unwrap();
    let root = Root::parse(&text(&f.root, SMALL).unwrap(), None).unwrap();
    let mut latest = String::new();
    for expected in [2, 3] {
        publisher::publish_next(
            &f.repo,
            &f.root,
            &f.key,
            profile,
            &"2".repeat(40),
            &f.deployment,
        )
        .unwrap();
        assert_eq!(
            fs::read(f.repo.join("heads").join(profile)).unwrap(),
            original
        );
        let mut maximum = 0;
        for entry in fs::read_dir(f.repo.join("releases")).unwrap() {
            let entry = entry.unwrap();
            let release = Release::parse(
                &text(&entry.path(), INDEX_LIMIT).unwrap(),
                &root,
                now().unwrap(),
                profile,
            )
            .unwrap();
            if release.sequence > maximum {
                maximum = release.sequence;
                latest = entry.file_name().into_string().unwrap();
            }
        }
        assert_eq!(maximum, expected);
    }
    publisher::promote_next(&f.repo, &f.root, &f.key, profile, &latest).unwrap();
    let head = Head::parse(
        &text(&f.repo.join("heads").join(profile), SMALL).unwrap(),
        &root,
        now().unwrap(),
        profile,
    )
    .unwrap();
    assert_eq!((head.version, &head.release), (2, &latest));
    let release_count = fs::read_dir(f.repo.join("releases")).unwrap().count();
    publisher::promote_next(&f.repo, &f.root, &f.key, profile, &latest).unwrap();
    let head = Head::parse(
        &text(&f.repo.join("heads").join(profile), SMALL).unwrap(),
        &root,
        now().unwrap(),
        profile,
    )
    .unwrap();
    assert_eq!((head.version, &head.release), (3, &latest));
    assert_eq!(
        fs::read_dir(f.repo.join("releases")).unwrap().count(),
        release_count
    );
    f.fetch("operator").unwrap();
    verify(&f.state, &f.dir.join("operator")).unwrap();
}

#[test]
fn legacy_promotion_refuses_an_expired_index_without_replacing_the_head() {
    let f = Fixture::new();
    let profile = "system-x86-64";
    let time = now().unwrap();
    let original = text(&f.repo.join("releases").join(&f.release), INDEX_LIMIT).unwrap();
    let mut lines: Vec<_> = original.lines().map(str::to_string).collect();
    lines.pop();
    lines[3] = (time - 7200).to_string();
    lines[4] = (time - 3600).to_string();
    let expired = format::signed(&(lines.join("\n") + "\n"), &fs::read(&f.key).unwrap()).unwrap();
    let digest = hash(expired.as_bytes());
    fs::write(f.repo.join("releases").join(&digest), expired).unwrap();
    let head = format::signed(
        &format!(
            "td-release-head-v1\n1\n2\n{}\n{}\n{profile}\n{digest}\n",
            time - 7200,
            time - 3600
        ),
        &fs::read(&f.key).unwrap(),
    )
    .unwrap();
    fs::write(f.repo.join("heads").join(profile), &head).unwrap();
    assert!(
        publisher::promote_next(&f.repo, &f.root, &f.key, profile, &digest)
            .unwrap_err()
            .contains("publish a fresh index")
    );
    assert_eq!(
        text(&f.repo.join("heads").join(profile), SMALL).unwrap(),
        head
    );
    publisher::publish_next(
        &f.repo,
        &f.root,
        &f.key,
        profile,
        &"3".repeat(40),
        &f.deployment,
    )
    .unwrap();
}

#[test]
fn automatic_promotion_checks_objects_before_changing_discovery() {
    let f = Fixture::new();
    let path = f.repo.join("heads/system-x86-64");
    let head = fs::read(&path).unwrap();
    let object = fs::read_dir(f.repo.join("objects"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    fs::write(object, b"corrupt").unwrap();
    assert!(
        publisher::promote_next(&f.repo, &f.root, &f.key, "system-x86-64", &f.release)
            .unwrap_err()
            .contains("corrupt or missing")
    );
    assert_eq!(fs::read(&path).unwrap(), head);
}

#[test]
fn next_publication_reclaims_interrupted_index_writes_only() {
    let f = Fixture::new();
    let releases = f.repo.join("releases");
    let partial = releases.join(format!(".{}.123.0.tmp", "a".repeat(64)));
    fs::write(&partial, b"interrupted signed index").unwrap();
    let outside = f.dir.join("outside");
    fs::write(&outside, b"preserved").unwrap();
    let linked = releases.join(format!(".{}.123.1.tmp", "b".repeat(64)));
    std::os::unix::fs::symlink(&outside, &linked).unwrap();
    publisher::publish_next(
        &f.repo,
        &f.root,
        &f.key,
        "system-x86-64",
        &"2".repeat(40),
        &f.deployment,
    )
    .unwrap();
    assert!(!partial.exists());
    assert!(!linked.exists());
    assert_eq!(fs::read(&outside).unwrap(), b"preserved");
    let unrelated = releases.join(".notes");
    fs::write(&unrelated, b"keep").unwrap();
    assert!(publisher::publish_next(
        &f.repo,
        &f.root,
        &f.key,
        "system-x86-64",
        &"2".repeat(40),
        &f.deployment
    )
    .is_err());
    assert_eq!(fs::read(unrelated).unwrap(), b"keep");
}

#[test]
fn direct_seed_reuse_preserves_remote_object_resume() {
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
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut cache = cache::Cache::open(&f.cache, cache::DEFAULT_LIMIT, deadline).unwrap();
    let seed = seed::scan(&release, &mut cache, &f.deployment, deadline).unwrap();
    let first = &release.payloads[0].objects[0];
    let local = seed.get(&first.digest, first.size).unwrap().unwrap();
    let remote = vec![17u8; 1024];
    let interrupted = vec![18u8; 1024];
    for bytes in [&remote, &interrupted] {
        assert!(seed
            .get(&hash(bytes), bytes.len() as u64)
            .unwrap()
            .is_none());
        fs::write(f.repo.join("objects").join(hash(bytes)), bytes).unwrap();
    }
    let expected = [local.as_slice(), remote.as_slice(), interrupted.as_slice()].concat();
    let payload = format::Payload {
        name: "bzImage",
        size: expected.len() as u64,
        digest: hash(&expected),
        objects: [&local, &remote, &interrupted]
            .into_iter()
            .map(|bytes| format::Object {
                size: bytes.len() as u64,
                digest: hash(bytes),
            })
            .collect(),
    };
    let server = Server::start(
        f.repo.clone(),
        Some(format!("objects/{}", hash(&interrupted))),
    );
    assert!(assemble(
        &payload,
        &mut cache,
        &seed,
        &mut Source::new(&server.base).unwrap(),
        &mut Vec::new()
    )
    .is_err());
    drop(server);
    assert!(!f.cache.join(&first.digest).exists());
    assert!(f.cache.join(hash(&remote)).exists());
    assert!(!f.cache.join(hash(&interrupted)).exists());
    let server = Server::start(f.repo.clone(), None);
    let mut output = Vec::new();
    let counts = assemble(
        &payload,
        &mut cache,
        &seed,
        &mut Source::new(&server.base).unwrap(),
        &mut output,
    )
    .unwrap();
    assert_eq!(output, expected);
    assert_eq!(counts, (1024, first.size + 1024, first.size));
    assert_eq!(
        *server.requests.lock().unwrap(),
        vec![format!("objects/{}", hash(&interrupted))]
    );
    assert!(!f.cache.join(&first.digest).exists());
    assert!(f.cache.join(hash(&interrupted)).exists());
}

fn s3_args(f: &Fixture, mock: &crate::s3::tests::Mock, cred: &Path) -> Vec<String> {
    vec![
        f.repo.to_str().unwrap().into(),
        f.root.to_str().unwrap().into(),
        "system-x86-64".into(),
        mock.endpoint.clone(),
        "auto".into(),
        "td-releases".into(),
        cred.to_str().unwrap().into(),
        "test".into(),
    ]
}
fn s3_upload(args: &[String]) -> Result<()> {
    upload::command(&args.iter().map(String::as_str).collect::<Vec<_>>())
}

#[test]
fn s3_release_upload_resumes_and_publishes_only_authenticated_files_head_last() {
    let f = Fixture::new();
    let mock = crate::s3::tests::Mock::new();
    let cred = f.dir.join("credentials");
    crate::s3::tests::write_credentials(&cred);
    let args = s3_args(&f, &mock, &cred);
    fs::write(f.repo.join("private.key"), b"DO NOT UPLOAD").unwrap();
    let orphan = "f".repeat(64);
    fs::write(f.repo.join("objects").join(&orphan), b"orphan").unwrap();
    let mut dry = args.clone();
    dry.push("--dry-run".into());
    s3_upload(&dry).unwrap();
    assert!(mock.store.lock().unwrap().requests.is_empty());
    let index = format!("releases/{}", f.release);
    mock.store.lock().unwrap().fail = Some((index.clone(), 503, 3));
    assert!(s3_upload(&args).is_err());
    {
        let store = mock.store.lock().unwrap();
        assert!(!store.files.contains_key("heads/system-x86-64"));
        assert!(store.files.keys().any(|k| k.starts_with("objects/")));
        assert!(!store.files.contains_key("private.key"));
        assert!(!store.files.contains_key(&format!("objects/{orphan}")));
    }
    mock.store.lock().unwrap().requests.clear();
    s3_upload(&args).unwrap();
    {
        let store = mock.store.lock().unwrap();
        let puts: Vec<_> = store
            .requests
            .iter()
            .filter(|(m, _)| m == "PUT")
            .map(|(_, k)| k.as_str())
            .collect();
        assert_eq!(puts, vec![index.as_str(), "heads/system-x86-64"]);
        assert_eq!(
            store.files["heads/system-x86-64"].headers["cache-control"],
            crate::s3::NO_STORE
        );
        assert_eq!(
            store.files[&index].headers["cache-control"],
            crate::s3::IMMUTABLE
        );
    }
    // The published repository is usable by the real authenticated downloader.
    let remote_repo = f.dir.join("remote");
    fs::create_dir(&remote_repo).unwrap();
    for (key, file) in &mock.store.lock().unwrap().files {
        let path = remote_repo.join(key);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, &file.bytes).unwrap();
    }
    let output = f.dir.join("remote-output");
    fetch(
        &format!("file:{}", remote_repo.display()),
        &f.state,
        &f.cache,
        &output,
    )
    .unwrap();
    for (name, _) in format::FILES {
        assert_eq!(
            fs::read(output.join(name)).unwrap(),
            fs::read(f.deployment.join(name)).unwrap()
        );
    }
}

#[test]
fn s3_corrupt_local_object_refuses_before_network_and_remote_conflict_never_promotes() {
    let f = Fixture::new();
    let mock = crate::s3::tests::Mock::new();
    let cred = f.dir.join("credentials");
    crate::s3::tests::write_credentials(&cred);
    let args = s3_args(&f, &mock, &cred);
    let object = fs::read_dir(f.repo.join("objects"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let original = fs::read(&object).unwrap();
    fs::write(&object, b"broken").unwrap();
    assert!(s3_upload(&args).is_err());
    assert!(mock.store.lock().unwrap().requests.is_empty());
    fs::write(&object, original).unwrap();
    let key = format!("objects/{}", object.file_name().unwrap().to_str().unwrap());
    mock.store.lock().unwrap().files.insert(
        key,
        crate::s3::tests::Stored {
            bytes: b"foreign".to_vec(),
            headers: Default::default(),
        },
    );
    assert!(s3_upload(&args).is_err());
    assert!(!mock
        .store
        .lock()
        .unwrap()
        .files
        .contains_key("heads/system-x86-64"));
}

#[test]
fn s3_head_compare_and_swap_refuses_races_and_stale_publishers() {
    let f = Fixture::new();
    let mock = crate::s3::tests::Mock::new();
    let cred = f.dir.join("credentials");
    crate::s3::tests::write_credentials(&cred);
    let args = s3_args(&f, &mock, &cred);
    s3_upload(&args).unwrap();
    let path = f.repo.join("heads/system-x86-64");
    let old = fs::read(&path).unwrap();
    let time = now().unwrap();
    publisher::promote(
        &f.repo,
        &f.root,
        &f.key,
        "system-x86-64",
        &f.release,
        2,
        time,
        time + 3600,
    )
    .unwrap();
    let newer = fs::read(&path).unwrap();
    publisher::promote(
        &f.repo,
        &f.root,
        &f.key,
        "system-x86-64",
        &f.release,
        3,
        time,
        time + 3600,
    )
    .unwrap();
    let newest = fs::read(&path).unwrap();
    fs::write(&path, &newer).unwrap();
    mock.store.lock().unwrap().race_head = Some(newest.clone());
    assert!(s3_upload(&args).is_err());
    assert_eq!(
        mock.store.lock().unwrap().files["heads/system-x86-64"].bytes,
        newest
    );
    fs::write(&path, old).unwrap();
    mock.store.lock().unwrap().requests.clear();
    assert!(s3_upload(&args).is_err());
    assert!(mock
        .store
        .lock()
        .unwrap()
        .requests
        .iter()
        .all(|(m, _)| m != "PUT"));
    fs::write(&path, &newest).unwrap();
    publisher::promote(
        &f.repo,
        &f.root,
        &f.key,
        "system-x86-64",
        &f.release,
        4,
        time,
        time + 3600,
    )
    .unwrap();
    // Commit a changed head, lose its reply, then reject the old ETag on retry.
    mock.store.lock().unwrap().requests.clear();
    mock.store.lock().unwrap().lose_reply = Some("heads/system-x86-64".into());
    s3_upload(&args).unwrap();
    let store = mock.store.lock().unwrap();
    assert_eq!(
        store.files["heads/system-x86-64"].bytes,
        fs::read(&path).unwrap()
    );
    assert_eq!(
        store
            .requests
            .iter()
            .filter(|(m, k)| m == "PUT" && k == "heads/system-x86-64")
            .count(),
        2
    );
    assert_eq!(
        store.requests.last(),
        Some(&("GET".into(), "heads/system-x86-64".into()))
    );
}

#[test]
fn s3_rotation_keeps_intermediate_roots_and_expired_head_is_not_uploaded() {
    let f = Fixture::new();
    let mock = crate::s3::tests::Mock::new();
    let cred = f.dir.join("credentials");
    crate::s3::tests::write_credentials(&cred);
    let args = s3_args(&f, &mock, &cred);
    s3_upload(&args).unwrap();
    let head_path = f.repo.join("heads/system-x86-64");
    let old = fs::read(&head_path).unwrap();
    let root = f.dir.join("root2");
    let key = f.dir.join("key2");
    let public = f.dir.join("public2");
    let (secret, pubkey) = sig::keygen().unwrap();
    fs::write(&key, secret).unwrap();
    fs::write(&public, sig::to_hex(&pubkey)).unwrap();
    let time = now().unwrap();
    publisher::root(
        2,
        time + format::DAY * 90,
        &f.private,
        &public,
        &f.private,
        &root,
    )
    .unwrap();
    publisher::publish_next(
        &f.repo,
        &root,
        &key,
        "system-x86-64",
        &"2".repeat(40),
        &f.deployment,
    )
    .unwrap();
    let selected = fs::read_dir(f.repo.join("releases"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .find(|s| s != &f.release)
        .unwrap();
    publisher::promote_next(&f.repo, &root, &key, "system-x86-64", &selected).unwrap();
    let mut rotated = args.clone();
    rotated[1] = root.to_str().unwrap().into();
    s3_upload(&rotated).unwrap();
    {
        let store = mock.store.lock().unwrap();
        assert!(store.files.contains_key("roots/1"));
        assert!(store.files.contains_key("roots/2"));
    }
    fs::write(&head_path, old).unwrap();
    mock.store.lock().unwrap().requests.clear();
    assert!(s3_upload(&args).is_err());
    assert!(mock
        .store
        .lock()
        .unwrap()
        .requests
        .iter()
        .all(|(m, _)| m != "PUT"));
    let expired = format!(
        "td-release-head-v1\n1\n2\n{}\n{}\nsystem-x86-64\n{}\n",
        time - 100,
        time - 1,
        f.release
    );
    fs::write(
        &head_path,
        format::signed(&expired, &fs::read(&f.key).unwrap()).unwrap(),
    )
    .unwrap();
    mock.store.lock().unwrap().requests.clear();
    assert!(s3_upload(&args).is_err());
    assert!(mock.store.lock().unwrap().requests.is_empty());
}

#[test]
fn s3_cleanup_is_explicit_and_retains_selected_and_recent_content() {
    let f = Fixture::new();
    let mock = crate::s3::tests::Mock::new();
    let cred = f.dir.join("credentials");
    crate::s3::tests::write_credentials(&cred);
    s3_upload(&s3_args(&f, &mock, &cred)).unwrap();
    let time = now().unwrap();
    let original = text(&f.repo.join("releases").join(&f.release), INDEX_LIMIT).unwrap();
    let mut lines: Vec<String> = original.lines().map(str::to_string).collect();
    lines.pop();
    lines[2] = "2".into();
    lines[3] = (time - 10 * format::DAY).to_string();
    lines[4] = (time - 9 * format::DAY).to_string();
    let expired = format::signed(&(lines.join("\n") + "\n"), &fs::read(&f.key).unwrap()).unwrap();
    let old_index = f.repo.join("releases").join(hash(expired.as_bytes()));
    fs::write(&old_index, expired).unwrap();
    let old = f.repo.join("objects").join(hash(b"old orphan"));
    let recent = f.repo.join("objects").join(hash(b"recent orphan"));
    fs::write(&old, b"old orphan").unwrap();
    fs::write(&recent, b"recent orphan").unwrap();
    File::open(&old)
        .unwrap()
        .set_times(
            std::fs::FileTimes::new()
                .set_modified(SystemTime::now() - Duration::from_secs(8 * format::DAY)),
        )
        .unwrap();
    let mut client = mock.client(&cred);
    for path in [&old, &recent, &old_index] {
        let bytes = fs::read(path).unwrap();
        let key = path.strip_prefix(&f.repo).unwrap().to_str().unwrap();
        client
            .immutable(
                key,
                &mut File::open(path).unwrap(),
                bytes.len() as u64,
                &hash(&bytes),
            )
            .unwrap();
    }
    let args: Vec<_> = [
        f.repo.to_str().unwrap(),
        f.root.to_str().unwrap(),
        &mock.endpoint,
        "auto",
        "td-releases",
        cred.to_str().unwrap(),
        "test",
    ]
    .into_iter()
    .collect();
    let before = mock.store.lock().unwrap().files.len();
    maintenance::gc(&args).unwrap();
    assert_eq!(mock.store.lock().unwrap().files.len(), before);
    assert!(!mock
        .store
        .lock()
        .unwrap()
        .requests
        .iter()
        .any(|(m, _)| m == "DELETE"));
    let mut delete = args.clone();
    delete.push("--delete");
    assert!(maintenance::gc(&delete).is_err());
    delete.push("--exclusive-publisher");
    // Unknown remote history makes a local reachability proof incomplete.
    let unknown = format!("releases/{}", "a".repeat(64));
    mock.store.lock().unwrap().files.insert(
        unknown.clone(),
        crate::s3::tests::Stored {
            bytes: b"unknown".to_vec(),
            headers: Default::default(),
        },
    );
    assert!(maintenance::gc(&delete).is_err());
    assert_eq!(mock.store.lock().unwrap().files.len(), before + 1);
    mock.store.lock().unwrap().files.remove(&unknown);
    // A mismatched head or object also refuses before deletion.
    let head_key = "heads/system-x86-64";
    let saved = mock.store.lock().unwrap().files[head_key].clone();
    mock.store
        .lock()
        .unwrap()
        .files
        .get_mut(head_key)
        .unwrap()
        .bytes = b"changed".to_vec();
    assert!(maintenance::gc(&delete).is_err());
    mock.store
        .lock()
        .unwrap()
        .files
        .insert(head_key.into(), saved);
    let old_key = old.strip_prefix(&f.repo).unwrap().to_str().unwrap();
    mock.store
        .lock()
        .unwrap()
        .files
        .get_mut(old_key)
        .unwrap()
        .headers
        .insert("x-amz-meta-sha256".into(), "wrong".into());
    assert!(maintenance::gc(&delete).is_err());
    mock.store
        .lock()
        .unwrap()
        .files
        .get_mut(old_key)
        .unwrap()
        .headers
        .insert("x-amz-meta-sha256".into(), hash(b"old orphan"));
    // Eligibility changes while remote deletion runs. Local reclamation must
    // use the original plan, leaving this now-old object for the next pass.
    mock.store.lock().unwrap().age_on_delete = Some(recent.clone());
    maintenance::gc(&delete).unwrap();
    let store = mock.store.lock().unwrap();
    assert_eq!(store.files.len(), before - 2);
    assert!(store.files.contains_key(&format!("releases/{}", f.release)));
    assert!(store
        .files
        .contains_key(recent.strip_prefix(&f.repo).unwrap().to_str().unwrap()));
    let deletes: Vec<_> = store
        .requests
        .iter()
        .filter(|(m, _)| m == "DELETE")
        .map(|(_, k)| k.as_str())
        .collect();
    assert_eq!(
        deletes,
        vec![
            old_index.strip_prefix(&f.repo).unwrap().to_str().unwrap(),
            old_key
        ]
    );
    assert!(!old.exists());
    assert!(!old_index.exists());
    assert!(recent.exists());
    drop(store);
    // A retry can recover after remote deletion succeeded but local unlink did not.
    fs::write(&old, b"old orphan").unwrap();
    File::open(&old)
        .unwrap()
        .set_times(
            std::fs::FileTimes::new()
                .set_modified(SystemTime::now() - Duration::from_secs(9 * format::DAY)),
        )
        .unwrap();
    maintenance::gc(&delete).unwrap();
    assert!(!old.exists());
    assert!(!recent.exists());
}

#[test]
fn static_policy_has_no_clock_deadline_but_keeps_authentication_and_floors() {
    let f = Fixture::new();
    let root_path = f.dir.join("static-root");
    let public = f.dir.join("release.pub");
    command(
        &[
            "release",
            "root",
            "2",
            f.private.to_str().unwrap(),
            public.to_str().unwrap(),
            f.private.to_str().unwrap(),
            root_path.to_str().unwrap(),
        ]
        .map(str::to_string),
    )
    .unwrap();
    let root = Root::parse(&text(&root_path, SMALL).unwrap(), None).unwrap();
    assert_eq!(root.expires, 0);
    root.fresh(u64::MAX).unwrap();
    publisher::publish_next(
        &f.repo,
        &root_path,
        &f.key,
        "system-x86-64",
        &"2".repeat(40),
        &f.deployment,
    )
    .unwrap();
    let release_path = fs::read_dir(f.repo.join("releases"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| text(p, INDEX_LIMIT).unwrap().lines().nth(1) == Some("2"))
        .unwrap();
    let digest = release_path.file_name().unwrap().to_str().unwrap();
    publisher::promote_next(&f.repo, &root_path, &f.key, "system-x86-64", digest).unwrap();
    f.fetch("static-upgrade").unwrap();
    let mut state = State::parse(&text(&f.state, SMALL * 3).unwrap()).unwrap();
    assert_eq!(state.root.version, 2);
    let head = text(&f.repo.join("heads/system-x86-64"), SMALL).unwrap();
    let release = text(&release_path, INDEX_LIMIT).unwrap();
    for time in [0, u64::MAX] {
        let admitted = metadata(&mut state, &head, &release, time).unwrap();
        assert_eq!(admitted.expires, 0);
    }
    // Signed policy, rather than an online timestamp, decides whether expiry applies.
    let old_root = Root::parse(&text(&f.root, SMALL).unwrap(), None).unwrap();
    assert!(old_root.lifetime(0, 0, 0, format::DAY).is_err());
    assert!(root.lifetime(0, 100, 0, format::DAY).is_err());
    let mut changed: Vec<String> = head.lines().map(str::to_string).collect();
    changed.pop();
    changed[2] = "2".into();
    let newer = format::signed(&(changed.join("\n") + "\n"), &fs::read(&f.key).unwrap()).unwrap();
    metadata(&mut state, &newer, &release, 0).unwrap();
    assert!(matches!(metadata(&mut state, &head, &release, 0), Err(e) if e.contains("replay")));
    let mut bad = newer.into_bytes();
    bad[0] = b'X';
    assert!(metadata(&mut state, std::str::from_utf8(&bad).unwrap(), &release, 0).is_err());
    assert!(Head::parse(&head, &root, 0, "server-x86-64").is_err());
    assert!(command(&["release", "renew"].map(str::to_string)).is_err());
    assert!(command(&["release", "renew-upload-s3"].map(str::to_string)).is_err());
}

#[test]
fn static_collection_retains_selected_and_recently_superseded_indexes() {
    let f = Fixture::new();
    let root_path = f.dir.join("static-root");
    publisher::root(
        2,
        0,
        &f.private,
        &f.dir.join("release.pub"),
        &f.private,
        &root_path,
    )
    .unwrap();
    let root = Root::parse(&text(&root_path, SMALL).unwrap(), None).unwrap();
    let time = now().unwrap();
    let mut indexes = Vec::new();
    for (sequence, days) in [(1, 60), (2, 40)] {
        publisher::publish(
            &f.repo,
            &root_path,
            &f.key,
            "system-x86-64",
            &"2".repeat(40),
            sequence,
            time - days * format::DAY,
            0,
            &f.deployment,
        )
        .unwrap();
        let path = fs::read_dir(f.repo.join("releases"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| {
                let text = text(p, INDEX_LIMIT).unwrap();
                text.lines().nth(1) == Some("2")
                    && text.lines().nth(2) == Some(&sequence.to_string())
            })
            .unwrap();
        indexes.push(path);
    }
    let old = indexes[0].file_name().unwrap().to_str().unwrap();
    let current = indexes[1].file_name().unwrap().to_str().unwrap();
    publisher::promote(
        &f.repo,
        &root_path,
        &f.key,
        "system-x86-64",
        old,
        1,
        time - 50 * format::DAY,
        0,
    )
    .unwrap();
    publisher::promote(
        &f.repo,
        &root_path,
        &f.key,
        "system-x86-64",
        current,
        2,
        time,
        0,
    )
    .unwrap();
    assert!(!publisher::garbage(&f.repo).unwrap().contains(&indexes[0]));
    let earlier = format::signed(
        &format!(
            "td-release-head-v1\n2\n2\n{}\n0\nsystem-x86-64\n{current}\n",
            time - 8 * format::DAY
        ),
        &fs::read(&f.key).unwrap(),
    )
    .unwrap();
    Head::parse(&earlier, &root, 0, "system-x86-64").unwrap();
    fs::write(f.repo.join("heads/system-x86-64"), earlier).unwrap();
    // Backdating the signed head cannot shorten its actual publication hold.
    assert!(!publisher::garbage(&f.repo).unwrap().contains(&indexes[0]));
    fs::write(
        f.repo.join("retention-hold-until"),
        format!("{}\n", time - 1),
    )
    .unwrap();
    let garbage = publisher::garbage(&f.repo).unwrap();
    assert!(garbage.contains(&indexes[0]));
    assert!(!garbage.contains(&indexes[1]));
    assert!(garbage
        .iter()
        .all(|p| p.parent() == Some(&f.repo.join("releases"))));
    let mock = crate::s3::tests::Mock::new();
    let cred = f.dir.join("credentials");
    crate::s3::tests::write_credentials(&cred);
    let mut args = s3_args(&f, &mock, &cred);
    args[1] = root_path.to_str().unwrap().into();
    let mut dry = args.clone();
    dry.push("--dry-run".into());
    s3_upload(&dry).unwrap();
    assert!(publisher::garbage(&f.repo).unwrap().contains(&indexes[0]));
    s3_upload(&args).unwrap();
    assert!(!publisher::garbage(&f.repo).unwrap().contains(&indexes[0]));
    fs::remove_file(f.repo.join("retention-hold-until")).unwrap();
    assert!(publisher::garbage(&f.repo).is_err());
    // Returning to explicitly finite policy must not drop acquisition grace.
    fs::write(
        f.repo.join("retention-hold-until"),
        format!("{}\n", time - 1),
    )
    .unwrap();
    let finite = f.dir.join("finite-root");
    publisher::root(
        3,
        time + 365 * format::DAY,
        &f.private,
        &f.dir.join("release.pub"),
        &f.private,
        &finite,
    )
    .unwrap();
    publisher::publish_next(
        &f.repo,
        &finite,
        &f.key,
        "system-x86-64",
        &"3".repeat(40),
        &f.deployment,
    )
    .unwrap();
    let next = fs::read_dir(f.repo.join("releases"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| text(p, INDEX_LIMIT).unwrap().lines().nth(1) == Some("3"))
        .unwrap();
    publisher::promote_next(
        &f.repo,
        &finite,
        &f.key,
        "system-x86-64",
        next.file_name().unwrap().to_str().unwrap(),
    )
    .unwrap();
    assert!(!publisher::garbage(&f.repo).unwrap().contains(&indexes[0]));
    assert!(!publisher::garbage(&f.repo).unwrap().contains(&indexes[1]));
    // A delayed finite upload must extend the inherited static history's hold.
    fs::write(
        f.repo.join("retention-hold-until"),
        format!("{}\n", time - 1),
    )
    .unwrap();
    args[1] = finite.to_str().unwrap().into();
    s3_upload(&args).unwrap();
    assert!(!publisher::garbage(&f.repo).unwrap().contains(&indexes[0]));
}
