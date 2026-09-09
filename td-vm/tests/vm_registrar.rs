//! Real local-account and Git-registry process fixtures.
use std::error::Error;
use std::fs::{self, DirBuilder};
use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
type Result<T> = std::result::Result<T, Box<dyn Error>>;
const BIN: &str = env!("CARGO_BIN_EXE_td-vm-registrar");
const GIT_BIN: &str = env!("CARGO_BIN_EXE_td-vm-git");
const ID: &str = "0123456789abcdef0123456789abcdef";
const KEY: &str = "AAAAC3NzaC1lZDI1NTE5AAAAIAEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEB";
fn in_trusted_root(name: &str) -> Result<bool> {
    if std::env::var_os("TD_VM_GIT_TEST_INNER").as_deref() == Some(std::ffi::OsStr::new("1")) {
        return Ok(false);
    }
    let builder = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or("crate has no repository parent")?
        .join("target/release/td-builder");
    let output = Command::new(builder)
        .arg("run-capped")
        .arg(std::env::current_exe()?)
        .args(["--exact", name, "--nocapture"])
        .env("TD_TEST_TRUSTED_ROOT", "1")
        .env("TD_VM_GIT_TEST_INNER", "1")
        .output()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    if !output.status.success()
        || !stdout
            .lines()
            .any(|line| line.starts_with("test result: ok. 1 passed; 0 failed;"))
    {
        return Err(format!(
            "trusted-root {name}: {}\n{stdout}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    Ok(true)
}

struct Root(PathBuf);
impl Drop for Root {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
struct Server(Child);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn invoke(command: &mut Command, success: bool) -> Result<Output> {
    let output = command.stdin(Stdio::null()).output()?;
    assert_eq!(
        output.status.success(),
        success,
        "{command:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(output)
}
fn host_git() -> Result<PathBuf> {
    for parent in std::env::split_paths(&std::env::var_os("PATH").ok_or("PATH")?) {
        let candidate = parent.join("git");
        if candidate.is_file() {
            return Ok(fs::canonicalize(candidate)?);
        }
    }
    Err("host Git not found".into())
}
fn fixture() -> Result<(Root, PathBuf)> {
    let root = Root(std::env::temp_dir().join(format!(
        "td-registrar-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    )));
    DirBuilder::new().mode(0o700).create(&root.0)?;
    let git = host_git()?;
    let repo = root.0.join("origin.git");
    invoke(
        Command::new(&git)
            .args(["init", "--bare", "--initial-branch=main"])
            .arg(&repo),
        true,
    )?;
    let seed = root.0.join("seed");
    invoke(
        Command::new(&git)
            .args(["init", "--initial-branch=main"])
            .arg(&seed),
        true,
    )?;
    invoke(
        Command::new(&git).arg("-C").arg(&seed).args([
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "--allow-empty",
            "-m",
            "baseline",
        ]),
        true,
    )?;
    invoke(
        Command::new(&git)
            .arg("-C")
            .arg(&seed)
            .arg("push")
            .arg(&repo)
            .arg("main"),
        true,
    )?;
    let policy = root.0.join("private/registry");
    fs::copy(GIT_BIN, root.0.join("td-vm-git"))?;
    fs::set_permissions(root.0.join("td-vm-git"), fs::Permissions::from_mode(0o700))?;
    invoke(
        Command::new(GIT_BIN)
            .arg("init")
            .arg(&policy)
            .arg(&repo)
            .arg(git)
            .arg(std::env::var("PATH")?)
            .arg(installed(&policy)?),
        true,
    )?;
    Ok((root, policy))
}
fn installed(policy: &Path) -> Result<PathBuf> {
    Ok(policy
        .parent()
        .and_then(Path::parent)
        .ok_or("fixture root")?
        .join("td-vm-git"))
}
fn launch(directory: &Path, policy: &Path, operator: u32) -> Result<Server> {
    let mut server = Server(
        Command::new(BIN)
            .arg("serve")
            .arg(directory)
            .arg(policy)
            .arg(installed(policy)?)
            .arg(operator.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()?,
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while UnixStream::connect(directory.join("control")).is_err() {
        if server.0.try_wait()?.is_some() {
            return Err("registrar failed to start".into());
        }
        if Instant::now() > deadline {
            return Err("registrar startup timed out".into());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(server)
}
fn request(directory: &Path, uid: u32, args: &[&str], success: bool) -> Result<Output> {
    let repository = directory.parent().ok_or("fixture root")?.join("origin.git");
    let mut words = args.to_vec();
    if matches!(args.first(), Some(&"enroll" | &"reserve" | &"revoke")) {
        words.insert(1, repository.to_str().ok_or("origin path")?);
    }
    invoke(
        Command::new(BIN)
            .arg("request")
            .arg(directory)
            .arg(uid.to_string())
            .args(words),
        success,
    )
}

#[test]
fn registrar_authenticates_accounts_and_preserves_registry_across_restart() -> Result<()> {
    if in_trusted_root("registrar_authenticates_accounts_and_preserves_registry_across_restart")? {
        return Ok(());
    }
    let (root, policy) = fixture()?;
    let uid = fs::metadata("/proc/self")?.uid();
    let directory = root.0.join("socket");
    let initial = fs::read(&policy)?;
    let wrong = launch(&directory, &policy, uid + 1)?;
    request(&directory, uid, &["enroll", ID, "task", KEY], false)?;
    assert_eq!(fs::read(&policy)?, initial);
    drop(wrong);
    // A killed server leaves its socket; the next lock owner may replace it.
    let mut server = launch(&directory, &policy, uid)?;
    request(&directory, uid, &["ping"], true)?;
    fs::write(&policy, "invalid registry\n")?;
    request(&directory, uid, &["ping"], false)?;
    fs::write(&policy, &initial)?;
    request(&directory, uid, &["ping"], true)?;
    invoke(
        Command::new(BIN)
            .arg("serve")
            .arg(&directory)
            .arg(&policy)
            .arg(installed(&policy)?)
            .arg(uid.to_string()),
        false,
    )?;
    assert!(server.0.try_wait()?.is_none());
    request(&directory, uid + 1, &["enroll", ID, "task", KEY], false)?;
    assert_eq!(fs::read(&policy)?, initial);
    request(&directory, uid, &["enroll", ID, "task", KEY], true)?;
    request(&directory, uid, &["reserve", ID, "followup"], true)?;
    let enrolled = fs::read(&policy)?;
    invoke(Command::new(BIN).arg("request").arg(&directory).arg(uid.to_string())
        .args(["revoke", "/srv/git/wrong.git", ID]), false)?;
    assert_eq!(fs::read(&policy)?, enrolled);
    let mut bad = UnixStream::connect(directory.join("control"))?;
    bad.write_all(b"TDVM-REGISTRAR-2\nrevoke ../private/registry\n")?;
    bad.shutdown(Shutdown::Write)?;
    bad.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut response = String::new();
    bad.read_to_string(&mut response)?;
    assert!(response.contains("ERROR"));
    assert_eq!(fs::read(&policy)?, enrolled);
    drop(server);
    let _server = launch(&directory, &policy, uid)?;
    request(&directory, uid, &["enroll", ID, "task", KEY], true)?;
    assert_eq!(fs::read(&policy)?, enrolled);
    request(&directory, uid, &["revoke", ID], true)?;
    request(&directory, uid, &["revoke", ID], true)?;
    assert_eq!(fs::read(&policy)?, initial);
    assert_eq!(fs::read_dir(policy.parent().ok_or("parent")?)?.count(), 2);
    assert_eq!(fs::metadata(directory)?.mode() & 0o777, 0o711);
    Ok(())
}

#[test]
fn client_sends_no_request_to_a_different_account() -> Result<()> {
    if in_trusted_root("client_sends_no_request_to_a_different_account")? {
        return Ok(());
    }
    let (root, _policy) = fixture()?;
    let directory = root.0.join("fake");
    fs::create_dir(&directory)?;
    let listener = UnixListener::bind(directory.join("control"))?;
    let wrong = fs::metadata("/proc/self")?.uid() + 1;
    request(&directory, wrong, &["enroll", ID, "task", KEY], false)?;
    let (mut stream, _) = listener.accept()?;
    stream.set_read_timeout(Some(Duration::from_secs(1)))?;
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes)?;
    assert!(bytes.is_empty(), "untrusted server received request bytes");
    Ok(())
}

#[test]
fn registrar_refuses_exposed_registry_and_non_socket_replacement() -> Result<()> {
    if in_trusted_root("registrar_refuses_exposed_registry_and_non_socket_replacement")? {
        return Ok(());
    }
    let (root, policy) = fixture()?;
    let uid = fs::metadata("/proc/self")?.uid();
    let directory = root.0.join("socket");
    fs::set_permissions(&policy, fs::Permissions::from_mode(0o644))?;
    invoke(
        Command::new(BIN)
            .arg("serve")
            .arg(&directory)
            .arg(&policy)
            .arg(installed(&policy)?)
            .arg(uid.to_string()),
        false,
    )?;
    assert!(!directory.exists());
    fs::set_permissions(&policy, fs::Permissions::from_mode(0o600))?;
    DirBuilder::new().mode(0o711).create(&directory)?;
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o711))?;
    fs::write(directory.join("control"), "must remain")?;
    invoke(
        Command::new(BIN)
            .arg("serve")
            .arg(&directory)
            .arg(&policy)
            .arg(installed(&policy)?)
            .arg(uid.to_string()),
        false,
    )?;
    assert_eq!(
        fs::read_to_string(directory.join("control"))?,
        "must remain"
    );
    Ok(())
}

#[test]
fn manager_profile_authenticates_exact_origin_and_preserves_failed_updates() -> Result<()> {
    if in_trusted_root("manager_profile_authenticates_exact_origin_and_preserves_failed_updates")? {
        return Ok(());
    }
    let (root, policy) = fixture()?;
    let uid = fs::metadata("/proc/self")?.uid();
    let directory = root.0.join("socket");
    let _server = launch(&directory, &policy, uid)?;
    let client = root.0.join("td-vm-registrar");
    let git = root.0.join("git");
    for (source, destination) in [(PathBuf::from(BIN), &client), (host_git()?, &git)] {
        fs::copy(source, destination)?;
        fs::set_permissions(destination, fs::Permissions::from_mode(0o700))?;
    }
    let home = root.0.join("vms");
    let input = root.0.join("profile");
    let repository = root.0.join("origin.git");
    let profile = format!("TDVM-GIT-PROFILE-1\nrepository={}\naddress=10.0.2.2\nport=22\nuser=test\nserver-uid={uid}\nsocket={}\nregistrar={}\ngit={}\nhost-key=ssh-ed25519 {KEY}\nauthor-name=Fixture\nauthor-email=fixture@example.invalid\n", repository.display(), directory.display(), client.display(), git.display());
    fs::write(&input, &profile)?;
    fs::set_permissions(&input, fs::Permissions::from_mode(0o600))?;
    let manager = |args: &[&str], success| {
        invoke(
            Command::new(env!("CARGO_BIN_EXE_td-vm"))
                .env("TD_VM_HOME", &home)
                .args(args),
            success,
        )
    };
    manager(&["git-profile", "show"], false)?;
    assert!(!home.exists(), "read-only probe created state");
    manager(
        &["git-profile", "set", input.to_str().ok_or("profile path")?],
        true,
    )?;
    let original = fs::read(home.join("git-profile"))?;
    let registry = fs::read(&policy)?;
    assert_eq!(
        fs::metadata(home.join("git-profile"))?.mode() & 0o777,
        0o600
    );
    let check = manager(&["git-profile", "check"], true)?;
    assert!(String::from_utf8(check.stdout)?
        .contains("Guest SSH and workspace readiness remain unverified"));
    assert_eq!(
        fs::read(&policy)?,
        registry,
        "profile check changed registry"
    );
    let origin = request(&directory, uid, &["origin"], true)?;
    assert!(!origin.stdout.is_empty());
    request(&directory, uid + 1, &["origin"], false)?;
    fs::write(&input, "not a profile\n")?;
    manager(
        &["git-profile", "set", input.to_str().ok_or("profile path")?],
        false,
    )?;
    assert_eq!(fs::read(home.join("git-profile"))?, original);
    fs::write(
        &input,
        profile.replace(
            &format!("server-uid={uid}"),
            &format!("server-uid={}", uid + 1),
        ),
    )?;
    manager(
        &["git-profile", "set", input.to_str().ok_or("profile path")?],
        true,
    )?;
    manager(&["git-profile", "check"], false)?;
    // A second valid bare repository must not pass against the first registrar.
    let other = root.0.join("other.git");
    invoke(
        Command::new(&git)
            .args(["clone", "--bare"])
            .arg(&repository)
            .arg(&other),
        true,
    )?;
    fs::write(
        &input,
        profile.replace(
            &format!("repository={}", repository.display()),
            &format!("repository={}", other.display()),
        ),
    )?;
    manager(
        &["git-profile", "set", input.to_str().ok_or("profile path")?],
        true,
    )?;
    let refused = manager(&["git-profile", "check"], false)?;
    assert!(String::from_utf8(refused.stderr)?.contains("different repository"));
    assert_eq!(fs::read(&policy)?, registry);
    // An origin with a different default branch cannot be provisioned.
    invoke(
        Command::new(&git).arg("-C").arg(&repository).args([
            "symbolic-ref",
            "HEAD",
            "refs/heads/other",
        ]),
        true,
    )?;
    request(&directory, uid, &["origin"], false)?;
    request(&directory, uid, &["ping"], true)?;
    Ok(())
}

fn enroll_reply(home: &Path, name: &str, key: &str, success: bool) -> Result<Output> {
    use std::io::BufRead;
    let directory = home.join("instances").join(name);
    let record = fs::read_to_string(directory.join("workspace"))?;
    let id = record.lines().nth(1).ok_or("missing instance ID")?.to_string();
    let path = directory.join("bridge");
    let listener = UnixListener::bind(&path)?;
    listener.set_nonblocking(true)?;
    let key = key.to_string();
    let peer = std::thread::spawn(move || -> std::result::Result<(), String> {
        let deadline = Instant::now() + Duration::from_secs(10);
        let (mut stream, _) = loop {
            match listener.accept() {
                Ok(pair) => break pair,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
                Err(e) => return Err(format!("guest fixture accept: {e}")),
            }
        };
        stream.set_read_timeout(Some(Duration::from_secs(5))).map_err(|e| e.to_string())?;
        let mut line = String::new();
        std::io::BufReader::new((&stream).take(1024)).read_line(&mut line).map_err(|e| e.to_string())?;
        let mut fields = line.split_whitespace();
        assert_eq!(fields.next(), Some("TDVM1"));
        let request = fields.next().ok_or("missing request ID")?;
        assert_eq!(fields.next(), Some("git-key"));
        assert_eq!(fields.next(), Some("0"));
        assert_eq!(fields.next(), Some("32"));
        let encoded: String = id.bytes().map(|byte| format!("{byte:02x}")).collect();
        assert_eq!(fields.next(), Some(encoded.as_str()));
        assert!(fields.next().is_none());
        let data = format!("TDVM-GIT-KEY-1\n{id}\nssh-ed25519 {key}\n");
        let encoded: String = data.bytes().map(|byte| format!("{byte:02x}")).collect();
        writeln!(stream, "TDVM1 {request} ok 0 {} {encoded}", data.len()).map_err(|e| e.to_string())?;
        Ok(())
    });
    let result = invoke(Command::new(env!("CARGO_BIN_EXE_td-vm")).env("TD_VM_HOME", home).args(["workspace", "enroll", name]), success);
    let joined = peer.join().map_err(|_| "guest fixture panicked")?;
    fs::remove_file(path)?;
    joined?;
    result
}

#[test]
fn manager_enrollment_retries_exact_keys_and_revokes_before_disk_deletion() -> Result<()> {
    if in_trusted_root("manager_enrollment_retries_exact_keys_and_revokes_before_disk_deletion")? { return Ok(()); }
    let (root, policy) = fixture()?;
    let uid = fs::metadata("/proc/self")?.uid();
    let socket = root.0.join("socket");
    let server = launch(&socket, &policy, uid)?;
    let client = root.0.join("td-vm-registrar");
    let git = root.0.join("git");
    for (source, destination) in [(PathBuf::from(BIN), &client), (host_git()?, &git)] {
        fs::copy(source, destination)?;
        fs::set_permissions(destination, fs::Permissions::from_mode(0o700))?;
    }
    let repository = root.0.join("origin.git");
    let home = root.0.join("vms");
    let input = root.0.join("profile");
    fs::write(&input, format!("TDVM-GIT-PROFILE-1\nrepository={}\naddress=10.0.2.2\nport=22\nuser=test\nserver-uid={uid}\nsocket={}\nregistrar={}\ngit={}\nhost-key=ssh-ed25519 {KEY}\nauthor-name=Fixture\nauthor-email=fixture@example.invalid\n", repository.display(), socket.display(), client.display(), git.display()))?;
    fs::set_permissions(&input, fs::Permissions::from_mode(0o600))?;
    let manager = |args: &[&str], success| invoke(Command::new(env!("CARGO_BIN_EXE_td-vm")).env("TD_VM_HOME", &home).args(args), success);
    manager(&["git-profile", "set", input.to_str().ok_or("profile path")?], true)?;
    let template = home.join("templates/base");
    DirBuilder::new().mode(0o700).create(&template)?;
    invoke(Command::new("qemu-img").args(["create", "-f", "qcow2"]).arg(template.join("disk.qcow2")).arg("4M"), true)?;
    for name in ["one", "two"] { manager(&["create", name, "base"], true)?; }
    let record = home.join("instances/one/workspace");
    let original = fs::read_to_string(&record)?;
    let id = original.lines().nth(1).ok_or("missing ID")?;
    let git_cmd = |args: &[&str]| invoke(Command::new(&git).arg("--git-dir").arg(&repository).args(args), true);
    let main = git_cmd(&["rev-parse", "refs/heads/main"])?.stdout;
    // An existing unowned branch refuses enrollment after the pending record
    // is durable. Clearing this fixture conflict allows the same-key retry.
    git_cmd(&["update-ref", "refs/heads/one", "refs/heads/main"])?;
    enroll_reply(&home, "one", KEY, false)?;
    let pending = fs::read_to_string(&record)?;
    assert!(pending.starts_with("TDVM-WORKSPACE-2\n"));
    assert!(pending.contains(&format!("\npending\nssh-ed25519 {KEY}\n")));
    assert!(!fs::read_to_string(&policy)?.contains(&format!("key={id} ")));
    git_cmd(&["update-ref", "-d", "refs/heads/one"])?;
    enroll_reply(&home, "one", KEY, true)?;
    let enrolled = fs::read_to_string(&record)?;
    assert!(enrolled.contains(&format!("\nenrolled\nssh-ed25519 {KEY}\n")));
    let authority = fs::read(&policy)?;
    enroll_reply(&home, "one", KEY, true)?;
    assert_eq!(fs::read_to_string(&record)?, enrolled);
    assert_eq!(fs::read(&policy)?, authority);
    let other_key = format!("{}C", KEY.strip_suffix('B').ok_or("fixture key suffix")?);
    enroll_reply(&home, "one", &other_key, false)?;
    assert_eq!(fs::read_to_string(&record)?, enrolled);
    assert_eq!(fs::read(&policy)?, authority);
    enroll_reply(&home, "two", &other_key, true)?;
    let second = fs::read_to_string(home.join("instances/two/workspace"))?;
    let second_id = second.lines().nth(1).ok_or("second ID")?;
    assert_eq!(git_cmd(&["rev-parse", "refs/heads/one"])?.stdout, main);
    drop(server);
    manager(&["delete", "one", "--yes"], false)?;
    assert!(home.join("instances/one/disk.qcow2").is_file());
    assert!(fs::read_to_string(&record)?.contains("\nrevoking\n"));
    manager(&["workspace", "enroll", "one"], false)?;
    let _server = launch(&socket, &policy, uid)?;
    manager(&["delete", "one", "--yes"], true)?;
    assert!(!home.join("instances/one").exists());
    let authority = fs::read_to_string(&policy)?;
    assert!(!authority.contains(&format!("key={id} ")));
    assert!(authority.contains(&format!("key={second_id} {other_key}")));
    assert_eq!(git_cmd(&["rev-parse", "refs/heads/main"])?.stdout, main);
    assert_eq!(git_cmd(&["rev-parse", "refs/heads/one"])?.stdout, main);
    assert_eq!(fs::read_to_string(home.join("instances/two/workspace"))?, second);
    Ok(())
}

#[test]
fn registrar_crash_retains_dispatcher_lifetime_before_rebind() -> Result<()> {
    if in_trusted_root("registrar_crash_retains_dispatcher_lifetime_before_rebind")? { return Ok(()); }
    let (root, policy) = fixture()?;
    let uid = fs::metadata("/proc/self")?.uid();
    let socket = root.0.join("socket");
    let source = root.0.join("delayed.rs");
    let marker = root.0.join("running");
    let release = root.0.join("release");
    let done = root.0.join("done");
    // A host-only Rust dispatcher fixture keeps the production stdin contract.
    fs::write(&source, format!(r#"
use std::{{fs, path::Path, process::{{Command, Stdio}}, time::{{Duration, Instant}}}};
fn main() {{
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let delayed = args.iter().any(|arg| arg == "enroll");
    if delayed {{
        fs::write({marker:?}, std::process::id().to_string()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        while !Path::new({release:?}).exists() {{
            if Instant::now() >= deadline {{ std::process::exit(124); }}
            std::thread::sleep(Duration::from_millis(5));
        }}
    }}
    let status = Command::new({GIT_BIN:?}).args(args).stdin(Stdio::inherit()).status().unwrap();
    if delayed {{ fs::write({done:?}, b"done").unwrap(); }}
    std::process::exit(status.code().unwrap_or(125));
}}
"#))?;
    invoke(Command::new("rustc").args(["--edition", "2021", "-C", "linker=gcc"]).arg(&source).arg("-o").arg(installed(&policy)?), true)?;
    let mut server = launch(&socket, &policy, uid)?;
    let client_socket = socket.clone();
    let client = std::thread::spawn(move || {
        request(&client_socket, uid, &["enroll", ID, "task", KEY], false)
            .map(|_| ()).map_err(|error| error.to_string())
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    while !marker.exists() {
        if Instant::now() >= deadline { return Err("delayed dispatcher did not start".into()); }
        std::thread::sleep(Duration::from_millis(5));
    }
    server.0.kill()?;
    server.0.wait()?;
    client.join().map_err(|_| "client fixture panicked")??;
    let replacement = launch(&socket, &policy, uid);
    let refused = replacement.is_err();
    drop(replacement);
    fs::write(&release, b"release")?;
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done.exists() {
        if Instant::now() >= deadline { return Err("delayed dispatcher did not finish".into()); }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(refused, "replacement listener overtook an unfinished enrollment");
    assert!(fs::read_to_string(&policy)?.contains(&format!("key={ID} {KEY}")));
    let _server = loop {
        match launch(&socket, &policy, uid) {
            Ok(server) => break server,
            Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
            Err(error) => return Err(error),
        }
    };
    request(&socket, uid, &["revoke", ID], true)?;
    assert!(!fs::read_to_string(&policy)?.contains(&format!("key={ID} ")));
    Ok(())
}
