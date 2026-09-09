#![forbid(unsafe_code)]

#[allow(dead_code)] // Shared compositor codec also carries clipboard and feed operations.
#[cfg_attr(feature = "target-recipe", path = "vm_wire.rs")]
#[cfg_attr(
    not(feature = "target-recipe"),
    path = "../../td-compositor/src/vm_wire.rs"
)]
mod vm_wire;
mod workspace;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant};
use vm_wire::git_key as protocol;
type Result<T> = std::result::Result<T, String>;

fn io<T>(value: std::io::Result<T>, action: &str) -> Result<T> {
    value.map_err(|e| format!("{action}: {e}"))
}

fn read(path: &Path, uid: u32, private: bool, limit: u64) -> Result<Vec<u8>> {
    let file = io(
        OpenOptions::new()
            .read(true)
            .custom_flags(0x20000 | 0x800)
            .open(path),
        "open VM key metadata",
    )?;
    let meta = io(file.metadata(), "inspect VM key metadata")?;
    if !meta.is_file()
        || meta.uid() != uid
        || meta.nlink() != 1
        || meta.mode() & (if private { 0o077 } else { 0o022 }) != 0
    {
        return Err("VM key metadata has an untrusted type, owner or mode".into());
    }
    let mut bytes = Vec::new();
    io(
        file.take(limit + 1).read_to_end(&mut bytes),
        "read VM key metadata",
    )?;
    if bytes.len() as u64 > limit {
        return Err("VM key metadata exceeds limit".into());
    }
    Ok(bytes)
}

fn directory(path: &Path, uid: u32, private: bool) -> Result<()> {
    let meta = io(fs::symlink_metadata(path), "inspect VM key directory")?;
    if !meta.is_dir()
        || meta.uid() != uid
        || meta.mode() & (if private { 0o077 } else { 0o022 }) != 0
    {
        return Err("VM key directory has an untrusted type, owner or mode".into());
    }
    Ok(())
}

fn create_directory(path: &Path, uid: u32) -> Result<()> {
    match DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(format!("create private VM key directory: {e}")),
    }
    directory(path, uid, true)
}

fn write(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    let mut file = io(
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(path),
        "create VM key metadata",
    )?;
    io(file.write_all(bytes), "write VM key metadata")?;
    io(
        file.set_permissions(fs::Permissions::from_mode(mode)),
        "set VM key metadata mode",
    )?;
    io(file.sync_all(), "sync VM key metadata")
}

fn keygen_run(keygen: &Path, args: &[&str], path: &Path) -> Result<Vec<u8>> {
    keygen_input(keygen, args, path, None)
}

fn keygen_input(keygen: &Path, args: &[&str], path: &Path, input: Option<File>) -> Result<Vec<u8>> {
    let (mut reader, writer) = io(UnixStream::pair(), "create guest key output")?;
    io(reader.set_nonblocking(true), "bound guest key output")?;
    let mut child = io(
        Command::new(keygen)
            .env_clear()
            .current_dir("/")
            .args(args)
            .arg(path)
            .stdin(input.map(Stdio::from).unwrap_or_else(Stdio::null))
            .stdout(Stdio::from(OwnedFd::from(writer)))
            .stderr(Stdio::null())
            .spawn(),
        "start guest key tool",
    )?;
    let result = (|| {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut bytes = Vec::new();
        let mut buffer = [0; 257];
        let mut eof = false;
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => eof = true,
                Ok(n) => {
                    bytes.extend_from_slice(buffer.get(..n).ok_or("invalid key read length")?);
                    if bytes.len() > 256 {
                        return Err("guest key tool output exceeds limit".into());
                    }
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                    ) => {}
                Err(e) => return Err(format!("read guest key tool output: {e}")),
            }
            match io(child.try_wait(), "observe guest key tool")? {
                Some(status) if !status.success() => {
                    return Err(format!("guest key tool failed ({status})"))
                }
                Some(_) if eof => return Ok(bytes),
                _ => {}
            }
            if Instant::now() >= deadline {
                return Err("guest key tool timed out".into());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    })();
    if result.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    result
}

fn normalize_public(bytes: &[u8]) -> Result<String> {
    let text = std::str::from_utf8(bytes).map_err(|_| "invalid guest public key text")?;
    let line = text
        .strip_suffix('\n')
        .ok_or("incomplete guest public key")?;
    if line.contains(['\n', '\r']) {
        return Err("extra guest public key lines".into());
    }
    let mut fields = line.split(' ');
    let algorithm = fields.next().ok_or("missing guest public key")?;
    let value = fields.next().ok_or("missing guest public key bytes")?;
    let key = format!("{algorithm} {value}");
    protocol::key(&key)?;
    Ok(key)
}

fn public_key(dir: &Path, uid: u32, keygen: &Path) -> Result<String> {
    // Bound and validate permissions before the fixed tool reads private bytes.
    let _private = read(&dir.join("id_ed25519"), uid, true, 8192)?;
    let key = normalize_public(&read(&dir.join("id_ed25519.pub"), uid, false, 256)?)?;
    let derived = normalize_public(&keygen_run(
        keygen,
        &["-y", "-P", "", "-f"],
        &dir.join("id_ed25519"),
    )?)?;
    if key != derived {
        return Err("guest private and public keys do not match".into());
    }
    verify_secret(dir, uid, keygen, &key)?;
    Ok(key)
}

fn verify_secret(dir: &Path, uid: u32, keygen: &Path, key: &str) -> Result<()> {
    let proof = dir.with_extension("proof");
    match fs::symlink_metadata(&proof) {
        Ok(_) => {
            directory(&proof, uid, true)?;
            io(fs::remove_dir_all(&proof), "remove interrupted key proof")?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("inspect key proof: {e}")),
    }
    create_directory(&proof, uid)?;
    let result = (|| {
        let challenge = proof.join("challenge");
        write(&challenge, b"td-vm private key self-test\n", 0o600)?;
        let allowed = proof.join("allowed");
        write(&allowed, format!("td-vm {key}\n").as_bytes(), 0o600)?;
        let private = dir.join("id_ed25519");
        keygen_run(
            keygen,
            &[
                "-q",
                "-Y",
                "sign",
                "-n",
                "td-vm-key",
                "-f",
                private.to_str().ok_or("invalid private key path")?,
            ],
            &challenge,
        )?;
        let signature = proof.join("challenge.sig");
        let _signature = read(&signature, uid, false, 2048)?;
        keygen_input(
            keygen,
            &[
                "-q",
                "-Y",
                "verify",
                "-n",
                "td-vm-key",
                "-I",
                "td-vm",
                "-f",
                allowed.to_str().ok_or("invalid allowed key path")?,
                "-s",
            ],
            &signature,
            Some(io(File::open(&challenge), "open key proof challenge")?),
        )?;
        Ok(())
    })();
    let cleanup = io(fs::remove_dir_all(&proof), "remove key proof");
    result.and(cleanup)
}

fn ensure_key(state: &Path, id: &str, uid: u32, keygen: &Path) -> Result<String> {
    protocol::identity(id.as_bytes())?;
    directory(state, uid, true)?;
    let active = state.join("git");
    match fs::symlink_metadata(&active) {
        Ok(_) => {
            directory(&active, uid, true)?;
            if read(&active.join("instance"), uid, true, 32)? != id.as_bytes() {
                return Err("guest is already bound to another VM identity".into());
            }
            return public_key(&active, uid, keygen);
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("inspect guest key state: {e}")),
    }
    let temporary = state.join("git.tmp");
    match fs::symlink_metadata(&temporary) {
        Ok(_) => {
            directory(&temporary, uid, true)?;
            io(
                fs::remove_dir_all(&temporary),
                "remove interrupted guest key staging",
            )?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("inspect guest key staging: {e}")),
    }
    create_directory(&temporary, uid)?;
    let result = (|| {
        write(&temporary.join("instance"), id.as_bytes(), 0o600)?;
        keygen_run(
            keygen,
            &["-q", "-t", "ed25519", "-N", "", "-C", "td-vm", "-f"],
            &temporary.join("id_ed25519"),
        )?;
        let key = public_key(&temporary, uid, keygen)?;
        for name in ["id_ed25519", "id_ed25519.pub"] {
            io(
                File::open(temporary.join(name)).and_then(|f| f.sync_all()),
                "sync guest key",
            )?;
        }
        io(
            File::open(&temporary).and_then(|f| f.sync_all()),
            "sync guest key staging",
        )?;
        io(fs::rename(&temporary, &active), "publish private guest key")?;
        io(
            File::open(state).and_then(|f| f.sync_all()),
            "sync guest key state",
        )?;
        Ok(key)
    })();
    let _ = fs::remove_dir_all(temporary);
    result
}

fn clear_response(response: &Path, uid: u32) -> Result<()> {
    directory(
        response.parent().ok_or("response has no parent")?,
        uid,
        false,
    )?;
    match fs::symlink_metadata(response) {
        Ok(meta) if (meta.is_file() || meta.file_type().is_symlink()) && meta.uid() == uid => {
            io(fs::remove_file(response), "clear guest public key")
        }
        Ok(_) => Err("unexpected guest public key entry".into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("inspect guest public key: {e}")),
    }
}

fn exchange(
    state: &Path,
    request: &Path,
    response: &Path,
    uid: u32,
    compositor: u32,
    keygen: &Path,
) -> Result<()> {
    match exchange_key(state, request, response, uid, compositor, keygen) {
        Ok(()) => Ok(()),
        Err(error) => {
            clear_response(response, uid).map_err(|cleanup| format!("{error}; {cleanup}"))?;
            Err(error)
        }
    }
}

fn exchange_key(
    state: &Path,
    request: &Path,
    response: &Path,
    uid: u32,
    compositor: u32,
    keygen: &Path,
) -> Result<()> {
    directory(
        request.parent().ok_or("request has no parent")?,
        compositor,
        false,
    )?;
    directory(
        response.parent().ok_or("response has no parent")?,
        uid,
        false,
    )?;
    let bytes = read(request, compositor, false, 32)?;
    let id = protocol::identity(&bytes)?;
    let key = ensure_key(state, id, uid, keygen)?;
    let reply = protocol::encode(id, &key)?;
    if read(response, uid, false, protocol::LIMIT as u64).is_ok_and(|current| current == reply) {
        return Ok(());
    }
    let temporary = response.with_extension("tmp");
    match fs::symlink_metadata(&temporary) {
        Ok(meta) if meta.is_file() && meta.uid() == uid => {
            io(fs::remove_file(&temporary), "remove public key staging")?
        }
        Ok(_) => return Err("unexpected public key staging entry".into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("inspect public key staging: {e}")),
    }
    let result = (|| {
        write(&temporary, &reply, 0o644)?;
        io(fs::rename(&temporary, response), "publish guest public key")
    })();
    let _ = fs::remove_file(temporary);
    result
}

#[derive(PartialEq, Eq)]
struct Stamp {
    dev: u64,
    ino: u64,
    mode: u32,
    uid: u32,
    links: u64,
    size: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
}

fn failure_state(
    state: &Path,
    request: &Path,
    response: &Path,
    keygen: &Path,
) -> Vec<std::result::Result<Stamp, std::io::ErrorKind>> {
    [
        state.to_path_buf(),
        state.join("git"),
        state.join("git/instance"),
        state.join("git/id_ed25519"),
        state.join("git/id_ed25519.pub"),
        request.to_path_buf(),
        response.to_path_buf(),
        keygen.to_path_buf(),
        request.parent().unwrap_or(request).to_path_buf(),
        response.parent().unwrap_or(response).to_path_buf(),
    ]
    .iter()
    .map(|path| {
        fs::symlink_metadata(path)
            .map(|m| Stamp {
                dev: m.dev(),
                ino: m.ino(),
                mode: m.mode(),
                uid: m.uid(),
                // The key and clone jobs share directories. Their proof and
                // reply churn must not trigger each other's failed attempts.
                links: if m.is_dir() { 0 } else { m.nlink() },
                size: if m.is_dir() { 0 } else { m.len() },
                mtime: if m.is_dir() { (0, 0) } else { (m.mtime(), m.mtime_nsec()) },
                ctime: if m.is_dir() { (0, 0) } else { (m.ctime(), m.ctime_nsec()) },
            })
            .map_err(|e| e.kind())
    })
    .collect()
}

fn serve() -> Result<()> {
    let uid = io(fs::metadata("/proc/self"), "inspect guest UID")?.uid();
    if uid != 1000 {
        return Err("VM guest helper must run as tester (UID 1000)".into());
    }
    clear_response(Path::new(protocol::RESPONSE), uid)?;
    clear_response(Path::new(vm_wire::workspace::RESPONSE), uid)?;
    let home = Path::new("/home/tester");
    directory(home, uid, false)?;
    let mut state = home.to_path_buf();
    for part in [".local", "share"] {
        state.push(part);
        match DirBuilder::new().mode(0o700).create(&state) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(format!("prepare guest home: {e}")),
        }
        directory(&state, uid, false)?;
    }
    state.push("td-vm");
    create_directory(&state, uid)?;
    let lock = io(
        OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .custom_flags(0x20000 | 0x800)
            .open(state.join("lock")),
        "open guest key lock",
    )?;
    let meta = io(lock.metadata(), "inspect guest key lock")?;
    if !meta.is_file() || meta.uid() != uid || meta.mode() & 0o077 != 0 || meta.nlink() != 1 {
        return Err("invalid guest key lock".into());
    }
    lock.try_lock()
        .map_err(|e| format!("guest key helper already active: {e}"))?;
    let mut workspace = workspace::Worker::default();
    let worker_lock = state.join("git-worker.lock");
    let tools = workspace::Tools { git: Path::new("/bin/git"), keygen: Path::new("/bin/ssh-keygen"), launcher: Path::new("/bin/td-vm-ssh"), worker_lock: &worker_lock };
    let mut workspace_error = String::new();
    let mut last_error = String::new();
    let mut published: Option<(Vec<u8>, Vec<u8>)> = None;
    let mut failed = None;
    let keygen = Path::new("/bin/ssh-keygen");
    loop {
        if let Err(error) = workspace.poll(&state, home, &lock, uid, &tools) {
            if error != workspace_error { eprintln!("td-vm-guest: {error}"); workspace_error = error; }
        }
        let request = Path::new(protocol::REQUEST);
        let response = Path::new(protocol::RESPONSE);
        let idle = matches!(fs::symlink_metadata(request), Err(e) if e.kind() == std::io::ErrorKind::NotFound);
        let unchanged = published.as_ref().is_some_and(|(id, reply)| {
            read(request, 993, false, 32).is_ok_and(|bytes| bytes == *id)
                && read(response, uid, false, protocol::LIMIT as u64)
                    .is_ok_and(|bytes| bytes == *reply)
        });
        let observed = failure_state(&state, request, response, keygen);
        if idle || unchanged || failed.as_ref() == Some(&observed) {
            std::thread::sleep(Duration::from_millis(500));
            continue;
        }
        let result = exchange(
            &state,
            Path::new(protocol::REQUEST),
            Path::new(protocol::RESPONSE),
            uid,
            993,
            keygen,
        );
        if let Err(error) = result {
            published = None;
            failed = Some(failure_state(&state, request, response, keygen));
            if error != last_error {
                eprintln!("td-vm-guest: {error}");
                last_error = error;
            }
        } else {
            failed = None;
            last_error.clear();
            published = read(response, uid, false, protocol::LIMIT as u64)
                .ok()
                .and_then(|reply| {
                    read(request, 993, false, 32)
                        .ok()
                        .filter(|id| {
                            protocol::identity(id)
                                .is_ok_and(|id| protocol::parse(&reply, id).is_ok())
                        })
                        .map(|id| (id, reply))
                });
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn main() -> ExitCode {
    if std::env::args_os().next().as_deref().and_then(|s| Path::new(s).file_name()) == Some(std::ffi::OsStr::new("td-vm-ssh")) {
        return match workspace::ssh(&std::env::args_os().skip(1).collect::<Vec<_>>()) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => { eprintln!("td-vm-ssh: {error}"); ExitCode::FAILURE }
        };
    }
    let args: Vec<_> = std::env::args().skip(1).collect();
    let result = if args.as_slice() == ["--help"] {
        println!("usage: td-vm-guest serve");
        Ok(())
    } else if args.as_slice() == ["serve"] {
        serve()
    } else {
        Err("usage: td-vm-guest serve".into())
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("td-vm-guest: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    const ID: &str = "0123456789abcdef0123456789abcdef";
    const OTHER: &str = "1123456789abcdef0123456789abcdef";

    struct Fixture {
        root: PathBuf,
        uid: u32,
        keygen: PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "td-vm-key-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            DirBuilder::new().mode(0o700).create(&root).unwrap();
            let uid = fs::metadata(&root).unwrap().uid();
            let keygen = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
                .map(|p| p.join("ssh-keygen"))
                .find(|p| p.is_file())
                .expect("ssh-keygen test prerequisite");
            for dir in ["state", "request", "response"] {
                create_directory(&root.join(dir), uid).unwrap();
            }
            Self { root, uid, keygen }
        }
        fn state(&self) -> PathBuf {
            self.root.join("state")
        }
        fn key(&self, id: &str) -> Result<String> {
            ensure_key(&self.state(), id, self.uid, &self.keygen)
        }
        fn request(&self) -> PathBuf {
            self.root.join("request/vm-git-identity")
        }
        fn response(&self) -> PathBuf {
            self.root.join("response/git-key")
        }
        fn exchange(&self) -> Result<()> {
            exchange(
                &self.state(),
                &self.request(),
                &self.response(),
                self.uid,
                self.uid,
                &self.keygen,
            )
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    #[ignore = "requires host ssh-keygen; run by the host preflight"]
    fn structurally_valid_secret_corruption_clears_success_without_repair() {
        let f = Fixture::new();
        write(&f.request(), ID.as_bytes(), 0o644).unwrap();
        f.exchange().unwrap();
        // Public repository fixture, with one private seed bit flipped.
        let damaged = r#"-----BEGIN OPENSSH PRIVATE KEY-----
b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZWQyNTUx
OQAAACDttFO36sAow04EvJONx4QLcsNSvaO4foqkQnmGvQ1XpgAAAKBfGiUOXxolDgAAAAtzc2gt
ZWQyNTUxOQAAACDttFO36sAow04EvJONx4QLcsNSvaO4foqkQnmGvQ1XpgAAAEA7XhI4oqkaNE8b
9UunfEu7W6mEcxaOPIEElYPFdCiuku20U7fqwCjDTgS8k43HhAtyw1K9o7h+iqRCeYa9DVemAAAA
FnRkLXFlbXUtYWRtaW4tc2VsZnRlc3QBAgMEBQYH
-----END OPENSSH PRIVATE KEY-----
"#;
        let public =
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIO20U7fqwCjDTgS8k43HhAtyw1K9o7h+iqRCeYa9DVem\n";
        fs::write(f.state().join("git/id_ed25519"), damaged).unwrap();
        fs::write(f.state().join("git/id_ed25519.pub"), public).unwrap();
        let extracted = keygen_run(
            &f.keygen,
            &["-y", "-P", "", "-f"],
            &f.state().join("git/id_ed25519"),
        )
        .unwrap();
        assert_eq!(normalize_public(&extracted).unwrap(), public.trim());
        assert!(f.exchange().is_err());
        assert!(!f.response().exists());
        assert_eq!(
            fs::read_to_string(f.state().join("git/id_ed25519")).unwrap(),
            damaged
        );
        assert!(!f.state().join("git.proof").exists());
        // Model both jobs after a service restart. Their proof/response writes
        // must not invalidate the other job's unchanged-failure suppression.
        let workspace_request = f.root.join("request/vm-workspace");
        let workspace_response = f.root.join("response/workspace");
        let mut plan = vm_wire::workspace::example();
        plan.guest_key = public.trim().into();
        write(&workspace_request, &plan.encode(), 0o644).unwrap();
        let worker_path = f.state().join("git-worker.lock");
        let tools = workspace::Tools {
            git: Path::new("/missing-git"), keygen: &f.keygen,
            launcher: Path::new("/missing-ssh"), worker_lock: &worker_path,
        };
        let lock_path = f.state().join("lock");
        write(&lock_path, b"", 0o600).unwrap();
        let lock = File::options().read(true).write(true).open(&lock_path).unwrap();
        lock.try_lock().unwrap();
        let endpoints = workspace::Endpoints {
            request: &workspace_request, response: &workspace_response, owner: f.uid,
        };
        let mut worker = workspace::Worker::default();
        let mut failed = None;
        let mut attempts = (0, 0);
        for _ in 0..4 {
            if worker.poll_at(&f.state(), &f.root, &lock, f.uid, &tools, &endpoints).is_err() {
                attempts.0 += 1;
            }
            let observed = failure_state(&f.state(), &f.request(), &f.response(), &f.keygen);
            if failed.as_ref() != Some(&observed) {
                assert!(f.exchange().is_err());
                attempts.1 += 1;
                failed = Some(failure_state(&f.state(), &f.request(), &f.response(), &f.keygen));
            }
        }
        assert_eq!(attempts, (1, 1), "workers retried each other's directory churn");
        fs::write(&workspace_request, plan.encode()).unwrap();
        assert!(worker.poll_at(&f.state(), &f.root, &lock, f.uid, &tools, &endpoints).is_err());
        assert!(failed == Some(failure_state(&f.state(), &f.request(), &f.response(), &f.keygen)));
        let failed = failure_state(&f.state(), &f.request(), &f.response(), &f.keygen);
        assert!(failed == failure_state(&f.state(), &f.request(), &f.response(), &f.keygen));
        fs::set_permissions(
            f.state().join("git/id_ed25519"),
            fs::Permissions::from_mode(0o400),
        )
        .unwrap();
        assert!(failed != failure_state(&f.state(), &f.request(), &f.response(), &f.keygen));
    }

    #[test]
    #[ignore = "requires host ssh-keygen; run by the host preflight"]
    fn real_keys_are_private_stable_and_unique_and_cannot_be_rebound() {
        let f = Fixture::new();
        let other = Fixture::new();
        let key = f.key(ID).unwrap();
        let private = fs::read(f.state().join("git/id_ed25519")).unwrap();
        assert_eq!(
            fs::metadata(f.state().join("git/id_ed25519"))
                .unwrap()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(f.key(ID).unwrap(), key);
        assert_ne!(other.key(ID).unwrap(), key);
        assert!(f.key(OTHER).unwrap_err().contains("another VM identity"));
        assert_eq!(fs::read(f.state().join("git/id_ed25519")).unwrap(), private);
        fs::copy(
            other.state().join("git/id_ed25519.pub"),
            f.state().join("git/id_ed25519.pub"),
        )
        .unwrap();
        assert!(f.key(ID).unwrap_err().contains("do not match"));
        assert_eq!(fs::read(f.state().join("git/id_ed25519")).unwrap(), private);
    }

    #[test]
    #[ignore = "requires host ssh-keygen; run by the host preflight"]
    fn interrupted_staging_recovers_but_published_damage_never_regenerates() {
        let f = Fixture::new();
        create_directory(&f.state().join("git.tmp"), f.uid).unwrap();
        fs::write(f.state().join("git.tmp/incomplete"), b"interrupted").unwrap();
        f.key(ID).unwrap();
        assert!(!f.state().join("git.tmp").exists());
        let public = fs::read(f.state().join("git/id_ed25519.pub")).unwrap();
        fs::write(f.state().join("git/id_ed25519"), b"corrupted").unwrap();
        assert!(f.key(ID).is_err());
        assert_eq!(
            fs::read(f.state().join("git/id_ed25519.pub")).unwrap(),
            public
        );
        assert_eq!(
            fs::read(f.state().join("git/id_ed25519")).unwrap(),
            b"corrupted"
        );
    }

    #[test]
    #[ignore = "requires host ssh-keygen; run by the host preflight"]
    fn only_valid_owned_requests_create_keys_and_only_public_bytes_are_published() {
        let f = Fixture::new();
        assert!(f.exchange().is_err());
        write(&f.request(), b"invalid", 0o644).unwrap();
        assert!(f.exchange().is_err());
        assert!(!f.state().join("git").exists());
        fs::write(f.request(), ID).unwrap();
        fs::set_permissions(f.request(), fs::Permissions::from_mode(0o666)).unwrap();
        assert!(f.exchange().is_err());
        fs::set_permissions(f.request(), fs::Permissions::from_mode(0o644)).unwrap();
        assert!(exchange(
            &f.state(),
            &f.request(),
            &f.response(),
            f.uid,
            f.uid + 1,
            &f.keygen
        )
        .is_err());
        assert!(!f.state().join("git").exists());
        f.exchange().unwrap();
        let reply = fs::read(f.response()).unwrap();
        assert_eq!(protocol::parse(&reply, ID).unwrap(), f.key(ID).unwrap());
        assert_eq!(fs::metadata(f.response()).unwrap().mode() & 0o777, 0o644);
        assert!(!String::from_utf8_lossy(&reply).contains("PRIVATE"));
        f.exchange().unwrap();
        assert_eq!(fs::read(f.response()).unwrap(), reply);
        fs::write(f.request(), OTHER).unwrap();
        assert!(f.exchange().is_err());
        assert!(!f.response().exists());
        fs::write(f.request(), ID).unwrap();
        f.exchange().unwrap();
        assert_eq!(fs::read(f.response()).unwrap(), reply);
        // A fresh helper exchange revalidates after restart; stale public
        // success must disappear when the persistent pair is damaged.
        fs::write(f.state().join("git/id_ed25519"), b"damaged").unwrap();
        assert!(f.exchange().is_err());
        assert!(!f.response().exists());
    }

    #[test]
    #[ignore = "requires host ssh-keygen; run by the host preflight"]
    fn links_are_refused_without_touching_their_targets() {
        let f = Fixture::new();
        let outside = f.root.join("outside");
        write(&outside, ID.as_bytes(), 0o644).unwrap();
        std::os::unix::fs::symlink(&outside, f.request()).unwrap();
        assert!(f.exchange().is_err());
        assert!(!f.state().join("git").exists());
        fs::remove_file(f.request()).unwrap();
        fs::hard_link(&outside, f.request()).unwrap();
        assert!(f.exchange().is_err());
        assert!(!f.state().join("git").exists());
        assert_eq!(fs::read(outside).unwrap(), ID.as_bytes());
    }
}
