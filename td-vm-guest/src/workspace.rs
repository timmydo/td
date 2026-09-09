//! Fixed SSH clone provisioning. Published workspaces are never replaced.
use super::{create_directory, directory, ensure_key, io, read, write, Result};
use crate::vm_wire::workspace::{self as wire, Plan};
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::Read;
use std::os::fd::OwnedFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

pub struct Tools<'a> {
    pub git: &'a Path,
    pub keygen: &'a Path,
    pub launcher: &'a Path,
    pub worker_lock: &'a Path,
}

fn git(repo: &Path, tools: &Tools<'_>, empty: &Path) -> Command {
    let mut command = Command::new(tools.git);
    command
        .env_clear()
        .current_dir(repo)
        .env("PATH", "/bin")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_SSH_VARIANT", "ssh")
        .arg("--no-replace-objects")
        .args([
            "-c",
            "gc.auto=0",
            "-c",
            "maintenance.auto=false",
            "-c",
            "fetch.recurseSubmodules=false",
            "-c",
            "transfer.fsckObjects=true",
            "-c",
            "fetch.fsckObjects=true",
            "-c",
            "protocol.file.allow=never",
        ])
        .arg("-c")
        .arg(format!("core.hooksPath={}", empty.display()))
        .arg("-c")
        .arg(format!("init.templateDir={}", empty.display()));
    command
}

fn worker_file(path: &Path) -> Result<File> {
    let uid = io(fs::metadata("/proc/self"), "inspect Git worker UID")?.uid();
    directory(path.parent().ok_or("Git worker lock parent")?, uid, true)?;
    let file = io(
        File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(0x20000 | 0x800)
            .open(path),
        "open Git worker lock",
    )?;
    let meta = io(file.metadata(), "inspect Git worker lock")?;
    if !meta.is_file() || meta.uid() != uid || meta.nlink() != 1 || meta.mode() & 0o077 != 0 {
        return Err("untrusted Git worker lock".into());
    }
    Ok(file)
}

fn run(mut command: Command, lock: &File, worker_path: &Path, operation: &str) -> Result<String> {
    let worker = worker_file(worker_path)?;
    worker
        .try_lock()
        .map_err(|e| format!("previous Git workers still active: {e}"))?;
    let completion = worker_file(worker_path)?;
    io(worker.set_len(0), "clear previous Git worker diagnostics")?;
    let (mut reader, writer) = io(std::os::unix::net::UnixStream::pair(), "create Git output")?;
    io(reader.set_nonblocking(true), "bound Git output")?;
    // A separate stderr lease covers descendants that redirect their stdout.
    // The helper drops its copy and reacquires independently before returning.
    let mut child = io(
        command
            .stdin(Stdio::from(io(lock.try_clone(), "retain Git lease")?))
            .stderr(Stdio::from(io(
                worker.try_clone(),
                "retain Git diagnostic lease",
            )?))
            .stdout(Stdio::from(OwnedFd::from(writer)))
            .spawn(),
        operation,
    )?;
    drop(command);
    drop(worker);
    let mut output = Vec::new();
    let mut exceeded = false;
    let mut eof = false;
    let mut buffer = [0; 4096];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => eof = true,
            Ok(count) => {
                if output.len().saturating_add(count) > 4096 {
                    exceeded = true;
                } else if !exceeded {
                    output.extend_from_slice(buffer.get(..count).ok_or("invalid Git read")?);
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) => {}
            Err(e) => {
                // Do not remove staging while an owned Git worker may still write it.
                let _ = child.wait();
                return Err(format!("read {operation} output: {e}"));
            }
        }
        match io(child.try_wait(), "observe Git worker")? {
            Some(status) if eof => {
                match completion.try_lock() {
                    Ok(()) => {}
                    Err(std::fs::TryLockError::WouldBlock) => {
                        std::thread::sleep(Duration::from_millis(20));
                        continue;
                    }
                    Err(e) => return Err(format!("observe Git worker completion: {e}")),
                }
                if !status.success() {
                    return Err(format!(
                        "{operation} failed ({status}); inspect the private td-vm git-worker.lock log"
                    ));
                }
                if exceeded {
                    return Err(format!("{operation} output exceeds limit"));
                }
                return String::from_utf8(output)
                    .map(|s| s.trim_end_matches('\n').to_string())
                    .map_err(|_| format!("{operation} returned non-UTF-8 output"));
            }
            _ => {}
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn sync_tree(path: &Path, remaining: &mut usize, depth: usize) -> Result<()> {
    if depth > 64 || *remaining == 0 {
        return Err("workspace publication exceeds tree bound".into());
    }
    *remaining -= 1;
    let meta = io(fs::symlink_metadata(path), "inspect workspace publication")?;
    if meta.is_dir() {
        for entry in io(fs::read_dir(path), "enumerate workspace publication")? {
            sync_tree(
                &io(entry, "read workspace entry")?.path(),
                remaining,
                depth + 1,
            )?;
        }
    } else if meta.file_type().is_symlink() {
        return Ok(());
    } else if !meta.is_file() {
        return Err("unexpected workspace publication entry".into());
    }
    io(
        File::open(path).and_then(|f| f.sync_all()),
        "sync workspace publication",
    )
}

fn ssh_config(plan: &Plan, state: &Path) -> String {
    format!("Host td-host\n  HostName {}\n  User {}\n  Port {}\n  HostKeyAlias td-host\n  IdentityFile {}/git/id_ed25519\n  IdentitiesOnly yes\n  IdentityAgent none\n  BatchMode yes\n  PreferredAuthentications publickey\n  StrictHostKeyChecking yes\n  UserKnownHostsFile {}/ssh/known_hosts\n  GlobalKnownHostsFile /dev/null\n  CheckHostIP no\n  UpdateHostKeys no\n  ForwardAgent no\n  ClearAllForwardings yes\n  PermitLocalCommand no\n  CanonicalizeHostname no\n  ProxyCommand none\n  ProxyJump none\n  ConnectTimeout 10\n  ServerAliveInterval 15\n  ServerAliveCountMax 3\n",
        plan.address, plan.user, plan.port, state.display(), state.display())
}

fn configuration(plan: &Plan, state: &Path, uid: u32) -> Result<()> {
    let active = state.join("ssh");
    if fs::symlink_metadata(&active).is_ok() {
        directory(&active, uid, true)?;
        if read(&active.join("plan"), uid, true, wire::LIMIT as u64)? != plan.encode()
            || read(&active.join("config"), uid, true, 4096)? != ssh_config(plan, state).as_bytes()
            || read(&active.join("known_hosts"), uid, true, 256)?
                != format!("td-host {}\n", plan.host_key).as_bytes()
        {
            return Err("existing VM SSH configuration differs; refusing replacement".into());
        }
        io(
            File::open(&active).and_then(|f| f.sync_all()),
            "sync existing SSH configuration",
        )?;
        return io(
            File::open(state).and_then(|f| f.sync_all()),
            "sync SSH configuration parent",
        );
    }
    let staging = state.join("ssh.tmp");
    remove_staging(&staging, uid)?;
    create_directory(&staging, uid)?;
    write(&staging.join("plan"), &plan.encode(), 0o600)?;
    write(
        &staging.join("config"),
        ssh_config(plan, state).as_bytes(),
        0o600,
    )?;
    write(
        &staging.join("known_hosts"),
        format!("td-host {}\n", plan.host_key).as_bytes(),
        0o600,
    )?;
    io(
        File::open(&staging).and_then(|f| f.sync_all()),
        "sync SSH configuration",
    )?;
    io(fs::rename(staging, active), "publish SSH configuration")?;
    io(
        File::open(state).and_then(|f| f.sync_all()),
        "sync SSH configuration parent",
    )
}

fn remove_staging(path: &Path, uid: u32) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => {
            directory(path, uid, true)?;
            io(fs::remove_dir_all(path), "remove interrupted clone staging")
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("inspect clone staging: {e}")),
    }
}

fn validate(
    root: &Path,
    plan: &Plan,
    uid: u32,
    tools: &Tools<'_>,
    empty: &Path,
    lock: &File,
) -> Result<()> {
    directory(root, uid, true)?;
    if read(&root.join("plan"), uid, true, wire::LIMIT as u64)? != plan.encode() {
        return Err("existing workspace belongs to another plan; refusing replacement".into());
    }
    for path in [root.join("repo"), root.join("repo/.git"), root.join("work")] {
        directory(&path, uid, false)?;
    }
    let mut command = git(&root.join("work"), tools, empty);
    command.args(["rev-parse", "--git-common-dir"]);
    let common = run(command, lock, tools.worker_lock, "inspect task worktree")?;
    let actual = io(
        fs::canonicalize(root.join("work").join(common)),
        "resolve worktree Git directory",
    )?;
    let expected = io(
        fs::canonicalize(root.join("repo/.git")),
        "resolve private Git directory",
    )?;
    if actual != expected {
        return Err("task worktree points outside its private clone".into());
    }
    Ok(())
}

pub fn prepare(
    home: &Path,
    state: &Path,
    plan: &Plan,
    uid: u32,
    tools: &Tools<'_>,
    lock: &File,
) -> Result<()> {
    if Plan::parse(&plan.encode())? != *plan {
        return Err("invalid clone plan".into());
    }
    if !state.starts_with(home) {
        return Err("VM state must be inside its private home".into());
    }
    // A restarted helper must not reclaim staging owned by surviving workers.
    let idle = worker_file(tools.worker_lock)?;
    idle.try_lock()
        .map_err(|e| format!("previous Git workers still active: {e}"))?;
    drop(idle);
    if ensure_key(state, &plan.id, uid, tools.keygen)? != plan.guest_key {
        return Err("clone plan does not match this VM's private Git identity".into());
    }
    configuration(plan, state, uid)?;
    for path in state.ancestors() {
        io(
            File::open(path).and_then(|f| f.sync_all()),
            "sync VM state ancestry",
        )?;
        if path == home {
            break;
        }
    }
    let empty = state.join("empty-template");
    create_directory(&empty, uid)?;
    if io(fs::read_dir(&empty), "inspect empty Git template")?
        .next()
        .is_some()
    {
        return Err("Git template directory must remain empty".into());
    }
    let src = home.join("src");
    match fs::create_dir(&src) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(format!("create source directory: {e}")),
    }
    directory(&src, uid, false)?;
    let active = src.join("td-vm");
    match fs::symlink_metadata(&active) {
        Ok(_) => {
            validate(&active, plan, uid, tools, &empty, lock)?;
            for path in [&src, home] {
                io(
                    File::open(path).and_then(|f| f.sync_all()),
                    "sync existing workspace parent",
                )?;
            }
            return Ok(());
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("inspect existing workspace: {e}")),
    }
    let staging = src.join(".td-vm.tmp");
    remove_staging(&staging, uid)?;
    create_directory(&staging, uid)?;
    write(&staging.join("plan"), &plan.encode(), 0o600)?;
    let repo = staging.join("repo");
    let work = staging.join("work");
    let mut command = git(&staging, tools, &empty);
    command
        .args([
            "clone",
            "--quiet",
            "--no-checkout",
            "--reject-shallow",
            "--origin",
            "origin",
            "--config",
        ])
        .arg(format!("core.sshCommand={}", tools.launcher.display()))
        .args([
            "--config",
            "ssh.variant=ssh",
            "--config",
            "fetch.recurseSubmodules=false",
            "--",
        ])
        .arg(plan.origin())
        .arg(&repo);
    run(command, lock, tools.worker_lock, "Git clone")?;
    let mut command = git(&repo, tools, &empty);
    command
        .args(["fetch", "--quiet", "--no-tags", "origin"])
        .arg(format!("+{0}:{0}", plan.retention_ref()));
    run(
        command,
        lock,
        tools.worker_lock,
        "fetch retained starting commit",
    )?;
    let mut command = git(&repo, tools, &empty);
    command
        .args(["rev-parse", "--verify"])
        .arg(format!("{}^{{commit}}", plan.retention_ref()));
    if run(command, lock, tools.worker_lock, "verify starting commit")? != plan.commit {
        return Err("fetched starting commit differs from the host plan".into());
    }
    for (key, value) in [
        ("user.name", plan.author_name.as_str()),
        ("user.email", plan.author_email.as_str()),
    ] {
        let mut command = git(&repo, tools, &empty);
        command.args(["config", "--", key, value]);
        run(command, lock, tools.worker_lock, "configure Git author")?;
    }
    let mut command = git(&repo, tools, &empty);
    command.args([
        "symbolic-ref",
        "refs/remotes/origin/HEAD",
        "refs/remotes/origin/main",
    ]);
    run(command, lock, tools.worker_lock, "configure origin HEAD")?;
    let mut command = git(&repo, tools, &empty);
    command
        .args([
            "worktree",
            "add",
            "--quiet",
            "--relative-paths",
            "-b",
            &plan.branch,
        ])
        .arg(&work)
        .arg(&plan.commit);
    run(command, lock, tools.worker_lock, "create task worktree")?;
    validate(&staging, plan, uid, tools, &empty, lock)?;
    sync_tree(&staging, &mut 1_000_000, 0)?;
    io(fs::rename(&staging, &active), "publish private workspace")?;
    for path in [&src, home] {
        io(
            File::open(path).and_then(|f| f.sync_all()),
            "sync workspace parent",
        )?;
    }
    validate(&active, plan, uid, tools, &empty, lock)
}

pub fn ssh(args: &[OsString]) -> Result<()> {
    let state = Path::new("/home/tester/.local/share/td-vm");
    let uid = io(fs::metadata("/proc/self"), "inspect SSH launcher UID")?.uid();
    if uid != 1000 {
        return Err("VM SSH launcher requires tester UID 1000".into());
    }
    directory(state, uid, true)?;
    directory(&state.join("ssh"), uid, true)?;
    let plan = Plan::parse(&read(
        &state.join("ssh/plan"),
        uid,
        true,
        wire::LIMIT as u64,
    )?)?;
    configuration(&plan, state, uid)?;
    let mut command = Command::new("/bin/ssh");
    command
        .env_clear()
        .env("PATH", "/bin")
        .arg("-F")
        .arg(state.join("ssh/config"))
        .args(args);
    if std::env::var_os("GIT_PROTOCOL").as_deref() == Some(OsStr::new("version=2")) {
        command.env("GIT_PROTOCOL", "version=2");
    }
    Err(format!("execute VM SSH client: {}", command.exec()))
}

pub(super) struct Endpoints<'a> {
    pub request: &'a Path,
    pub response: &'a Path,
    pub owner: u32,
}

#[derive(Default)]
pub struct Worker {
    observed: Option<Vec<std::result::Result<super::Stamp, std::io::ErrorKind>>>,
}
impl Worker {
    pub fn poll(
        &mut self,
        state: &Path,
        home: &Path,
        lock: &File,
        uid: u32,
        tools: &Tools<'_>,
    ) -> Result<()> {
        self.poll_at(
            state,
            home,
            lock,
            uid,
            tools,
            &Endpoints {
                request: Path::new(wire::REQUEST),
                response: Path::new(wire::RESPONSE),
                owner: 993,
            },
        )
    }

    pub(super) fn poll_at(
        &mut self,
        state: &Path,
        home: &Path,
        lock: &File,
        uid: u32,
        tools: &Tools<'_>,
        endpoints: &Endpoints<'_>,
    ) -> Result<()> {
        let request = endpoints.request;
        let response = endpoints.response;
        if matches!(fs::symlink_metadata(request), Err(e) if e.kind() == std::io::ErrorKind::NotFound)
        {
            return Ok(());
        }
        let observed = super::failure_state(state, request, response, tools.keygen);
        if self.observed.as_ref() == Some(&observed) {
            return Ok(());
        }
        let result = (|| {
            directory(
                request.parent().ok_or("workspace request parent")?,
                endpoints.owner,
                false,
            )?;
            super::clear_response(response, uid)?;
            let plan = Plan::parse(&read(request, endpoints.owner, false, wire::LIMIT as u64)?)?;
            let result = prepare(home, state, &plan, uid, tools, lock);
            let bytes = match &result {
                Ok(()) => wire::ready(&plan),
                Err(error) => wire::failure(&plan, error),
            };
            let temporary = response.with_extension("tmp");
            super::clear_response(&temporary, uid)?;
            write(&temporary, &bytes, 0o644)?;
            io(fs::rename(temporary, response), "publish workspace status")?;
            result
        })();
        self.observed = Some(super::failure_state(state, request, response, tools.keygen));
        result
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    use std::path::PathBuf;
    use std::process::Child;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    const ID: &str = "0123456789abcdef0123456789abcdef";
    struct Fixture {
        root: PathBuf,
        server: Option<Child>,
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            if let Some(server) = self.server.as_mut() {
                let _ = server.kill();
                let _ = server.wait();
            }
            let _ = fs::remove_dir_all(&self.root);
        }
    }
    fn program(name: &str) -> PathBuf {
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .map(|p| p.join(name))
            .find(|p| p.is_file())
            .or_else(|| {
                let p = PathBuf::from("/run/current-system/profile/sbin").join(name);
                p.is_file().then_some(p)
            })
            .map(|p| fs::canonicalize(p).unwrap())
            .unwrap_or_else(|| panic!("host fixture needs {name}"))
    }
    fn invoke(command: &mut Command) -> String {
        let out = command.stdin(Stdio::null()).output().unwrap();
        assert!(
            out.status.success(),
            "{command:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout)
            .unwrap()
            .trim_end_matches('\n')
            .into()
    }
    #[test]
    #[ignore = "requires host Git, OpenSSH daemon/client, rustc and GCC"]
    fn ssh_clone_keeps_retained_base_private_worktree_and_user_edits() {
        if let Some(root) = std::env::var_os("TD_VM_CLONE_CRASH_CHILD") {
            let root = PathBuf::from(root);
            let uid = fs::metadata("/proc/self").unwrap().uid();
            let plan = Plan::parse(&fs::read(root.join("provision-plan")).unwrap()).unwrap();
            let git = program("git");
            let keygen = program("ssh-keygen");
            let launcher = root.join("ssh-fixture");
            let lock = File::options()
                .read(true)
                .write(true)
                .open(root.join("home/state/lock"))
                .unwrap();
            lock.lock().unwrap();
            prepare(
                &root.join("home"),
                &root.join("home/state"),
                &plan,
                uid,
                &Tools {
                    git: &git,
                    keygen: &keygen,
                    launcher: &launcher,
                    worker_lock: &root.join("home/state/git-worker.lock"),
                },
                &lock,
            )
            .unwrap();
            return;
        }
        let uid = fs::metadata("/proc/self").unwrap().uid();
        // OpenSSH StrictModes deliberately rejects /tmp even for private children.
        let root = PathBuf::from(std::env::var_os("HOME").expect("host HOME")).join(format!(
            ".td-vm-clone-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
        let mut f = Fixture { root, server: None };
        let keygen = program("ssh-keygen");
        let git_bin = program("git");
        let ssh = program("ssh");
        let home = f.root.join("home");
        create_directory(&home, uid).unwrap();
        let state = home.join("state");
        create_directory(&state, uid).unwrap();
        let key = ensure_key(&state, ID, uid, &keygen).unwrap();
        let host_key = f.root.join("host-key");
        invoke(
            Command::new(&keygen)
                .args(["-q", "-t", "ed25519", "-N", "", "-f"])
                .arg(&host_key),
        );
        let public =
            super::super::normalize_public(&fs::read(host_key.with_extension("pub")).unwrap())
                .unwrap();
        let origin = f.root.join("origin.git");
        invoke(
            Command::new(&git_bin)
                .args(["init", "--bare", "--initial-branch=main"])
                .arg(&origin),
        );
        let seed = f.root.join("seed");
        invoke(
            Command::new(&git_bin)
                .args(["init", "--initial-branch=main"])
                .arg(&seed),
        );
        fs::write(seed.join("source"), b"retained source\n").unwrap();
        invoke(
            Command::new(&git_bin)
                .arg("-C")
                .arg(&seed)
                .args(["add", "source"]),
        );
        invoke(Command::new(&git_bin).arg("-C").arg(&seed).args([
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-m",
            "base",
        ]));
        invoke(
            Command::new(&git_bin)
                .arg("-C")
                .arg(&seed)
                .arg("push")
                .arg(&origin)
                .arg("main"),
        );
        let base = invoke(
            Command::new(&git_bin)
                .arg("--git-dir")
                .arg(&origin)
                .args(["rev-parse", "HEAD"]),
        );
        let anchor = format!("refs/td-vm/start/{ID}");
        invoke(Command::new(&git_bin).arg("--git-dir").arg(&origin).args([
            "update-ref",
            &anchor,
            &base,
        ]));
        let next = invoke(Command::new(&git_bin).arg("--git-dir").arg(&origin).args([
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit-tree",
            "HEAD^{tree}",
            "-m",
            "unrelated main",
        ]));
        for name in ["refs/heads/main", "refs/heads/task"] {
            invoke(Command::new(&git_bin).arg("--git-dir").arg(&origin).args([
                "update-ref",
                name,
                &next,
            ]));
        }
        invoke(
            Command::new(&git_bin)
                .arg("--git-dir")
                .arg(&origin)
                .args(["gc", "--prune=now"]),
        );
        let port = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap().port()
        };
        let user = invoke(Command::new(program("id")).arg("-un"));
        let plan = Plan {
            id: ID.into(),
            branch: "task".into(),
            commit: base.clone(),
            repository: origin.to_str().unwrap().into(),
            address: "127.0.0.1".into(),
            port,
            user: user.clone(),
            host_key: public,
            guest_key: key,
            author_name: "-Fixture".into(),
            author_email: "fixture@example.invalid".into(),
        };
        let launcher = f.root.join("ssh-fixture");
        let dispatcher = f.root.join("dispatcher");
        let source = f.root.join("fixture.rs");
        let config = state.join("ssh/config");
        let path = std::env::var("PATH").unwrap();
        let hold = f.root.join("hold");
        let running = f.root.join("clone-running");
        fs::write(&source, format!(r#"
use std::{{env, path::Path, process::Command, os::unix::process::CommandExt}};
fn main() {{
    let args: Vec<_> = env::args_os().collect();
    if Path::new(&args[0]).file_name().unwrap() == "ssh-fixture" {{
        let error = Command::new({ssh:?}).arg("-F").arg({config:?}).args(&args[1..]).exec();
        panic!("{{error}}");
    }}
    if args.get(1).and_then(|s| s.to_str()) == Some("lifetime-parent") {{
        Command::new(env::current_exe().unwrap()).arg("lifetime-child")
            .stdout(std::process::Stdio::null()).spawn().unwrap();
        std::process::exit(17);
    }}
    if args.get(1).and_then(|s| s.to_str()) == Some("lifetime-child") {{
        std::fs::write({running:?}, b"orphan").unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while Path::new({hold:?}).exists() && std::time::Instant::now() < deadline {{
            std::thread::sleep(std::time::Duration::from_millis(10));
        }}
        return;
    }}
    let original = env::var("SSH_ORIGINAL_COMMAND").unwrap();
    if Path::new({hold:?}).exists() {{
        std::fs::write({running:?}, b"running").unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while Path::new({hold:?}).exists() {{
            if std::time::Instant::now() > deadline {{ std::process::exit(124); }}
            std::thread::sleep(std::time::Duration::from_millis(10));
        }}
    }}
    let verb = if original == format!("git-upload-pack '{{}}'", {origin:?}) {{ "upload-pack" }}
        else if original == format!("git-receive-pack '{{}}'", {origin:?}) {{ "receive-pack" }}
        else {{ std::process::exit(126) }};
    let error = Command::new({git_bin:?}).env_clear().env("PATH", {path:?}).arg(verb).arg({origin:?}).exec();
    panic!("{{error}}");
}}
"#)).unwrap();
        invoke(
            Command::new(program("rustc"))
                .args(["--edition", "2021", "-C", "linker=gcc"])
                .arg(&source)
                .arg("-o")
                .arg(&dispatcher),
        );
        std::os::unix::fs::symlink(&dispatcher, &launcher).unwrap();
        let auth = f.root.join("authorized");
        fs::write(&auth, format!("restrict {}\n", plan.guest_key)).unwrap();
        fs::set_permissions(&auth, fs::Permissions::from_mode(0o600)).unwrap();
        let daemon = f.root.join("sshd_config");
        fs::write(&daemon, format!("ListenAddress 127.0.0.1\nPort {port}\nHostKey {}\nPidFile {}/sshd.pid\nAuthorizedKeysFile {}\nPasswordAuthentication no\nKbdInteractiveAuthentication no\nUsePAM no\nStrictModes yes\nAllowUsers {user}\nPermitUserEnvironment no\nDisableForwarding yes\nPermitTTY no\nForceCommand {}\n", host_key.display(), f.root.display(), auth.display(), dispatcher.display())).unwrap();
        let log = File::create(f.root.join("sshd.log")).unwrap();
        f.server = Some(
            Command::new(program("sshd"))
                .args(["-D", "-e", "-f"])
                .arg(&daemon)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::from(log))
                .spawn()
                .unwrap(),
        );
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
            assert!(
                f.server.as_mut().unwrap().try_wait().unwrap().is_none(),
                "{}",
                fs::read_to_string(f.root.join("sshd.log")).unwrap()
            );
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        let lock = File::options()
            .read(true)
            .write(true)
            .create_new(true)
            .open(state.join("lock"))
            .unwrap();
        let tools = Tools {
            git: &git_bin,
            keygen: &keygen,
            launcher: &launcher,
            worker_lock: &state.join("git-worker.lock"),
        };
        // Git can exit while index-pack survives with a different stdout pipe.
        // Returning now would let this same helper erase its worker's staging.
        fs::write(&hold, "hold orphan").unwrap();
        let mut orphan_parent = Command::new(&dispatcher);
        orphan_parent.arg("lifetime-parent");
        let inherited = lock.try_clone().unwrap();
        let worker_path = tools.worker_lock.to_path_buf();
        let waiting = std::thread::spawn(move || {
            run(orphan_parent, &inherited, &worker_path, "orphan fixture")
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !running.exists() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        std::thread::sleep(Duration::from_millis(200));
        let returned_with_survivor = waiting.is_finished();
        fs::remove_file(&hold).unwrap();
        assert!(waiting.join().unwrap().is_err());
        fs::remove_file(&running).unwrap();
        assert!(
            !returned_with_survivor,
            "Git exit released an attempt while a descendant survived"
        );
        // An interrupted private staging tree is discarded; no published work exists.
        fs::create_dir(home.join("src")).unwrap();
        create_directory(&home.join("src/.td-vm.tmp"), uid).unwrap();
        fs::write(home.join("src/.td-vm.tmp/partial"), "interrupted").unwrap();
        fs::write(f.root.join("provision-plan"), plan.encode()).unwrap();
        fs::write(&hold, "delay Git upload").unwrap();
        let mut helper = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "workspace::tests::ssh_clone_keeps_retained_base_private_worktree_and_user_edits",
                "--include-ignored",
            ])
            .env("TD_VM_CLONE_CRASH_CHILD", &f.root)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !running.exists() {
            if helper.try_wait().unwrap().is_some() || std::time::Instant::now() > deadline {
                let _ = helper.kill();
                let _ = helper.wait();
                panic!("clone worker did not reach SSH fixture");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        helper.kill().unwrap();
        helper.wait().unwrap();
        assert!(
            matches!(lock.try_lock(), Err(std::fs::TryLockError::WouldBlock)),
            "surviving Git/SSH lost the worker lease"
        );
        fs::remove_file(&hold).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            match lock.try_lock() {
                Ok(()) => break,
                Err(std::fs::TryLockError::WouldBlock) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10))
                }
                other => panic!("Git worker did not release its lease: {other:?}"),
            }
        }
        assert!(!home.join("src/td-vm").exists());
        // The real SSH server refuses a wrong trusted host key before publication.
        let ssh_config = state.join("ssh/known_hosts");
        let saved = fs::read(&ssh_config).unwrap();
        fs::write(&ssh_config, format!("td-host {}\n", plan.guest_key)).unwrap();
        let mut probe = Command::new(&ssh);
        probe
            .args(["-F"])
            .arg(&config)
            .args(["-o", "BatchMode=yes", "td-host", "ignored"]);
        assert!(!probe
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success());
        fs::write(&ssh_config, saved).unwrap();
        // A changed retention ref refuses publication; restoring it permits retry.
        invoke(Command::new(&git_bin).arg("--git-dir").arg(&origin).args([
            "update-ref",
            &anchor,
            &next,
        ]));
        assert!(prepare(&home, &state, &plan, uid, &tools, &lock)
            .unwrap_err()
            .contains("fetched starting commit differs"));
        assert!(!home.join("src/td-vm").exists());
        invoke(Command::new(&git_bin).arg("--git-dir").arg(&origin).args([
            "update-ref",
            &anchor,
            &base,
        ]));
        if let Err(error) = prepare(&home, &state, &plan, uid, &tools, &lock) {
            panic!(
                "{error}; Git log: {}; SSH log: {}",
                fs::read_to_string(state.join("lock")).unwrap(),
                fs::read_to_string(f.root.join("sshd.log")).unwrap()
            );
        }
        let work = home.join("src/td-vm/work");
        let repo = home.join("src/td-vm/repo");
        assert_eq!(
            invoke(
                Command::new(&git_bin)
                    .arg("-C")
                    .arg(&work)
                    .args(["rev-parse", "HEAD"])
            ),
            base
        );
        assert_eq!(
            invoke(
                Command::new(&git_bin)
                    .arg("-C")
                    .arg(&work)
                    .args(["branch", "--show-current"])
            ),
            "task"
        );
        assert_eq!(
            invoke(
                Command::new(&git_bin)
                    .arg("-C")
                    .arg(&work)
                    .args(["config", "user.name"])
            ),
            "-Fixture"
        );
        assert_eq!(
            invoke(
                Command::new(&git_bin)
                    .arg("-C")
                    .arg(&work)
                    .args(["config", "remote.origin.fetch"])
            ),
            "+refs/heads/*:refs/remotes/origin/*"
        );
        assert!(!repo.join(".git/objects/info/alternates").exists());
        assert!(!repo.join(".git/shallow").exists());
        assert!(!home.join("src/.td-vm.tmp").exists());
        fs::write(work.join("source"), "private edited source\n").unwrap();
        fs::write(work.join("untracked"), "keep me").unwrap();
        prepare(&home, &state, &plan, uid, &tools, &lock).unwrap();
        assert_eq!(
            fs::read_to_string(work.join("source")).unwrap(),
            "private edited source\n"
        );
        assert_eq!(
            fs::read_to_string(work.join("untracked")).unwrap(),
            "keep me"
        );
        let mut changed = plan.clone();
        changed.commit = next;
        assert!(prepare(&home, &state, &changed, uid, &tools, &lock).is_err());
        assert_eq!(
            fs::read_to_string(work.join("untracked")).unwrap(),
            "keep me"
        );
        invoke(
            Command::new(&git_bin)
                .arg("-C")
                .arg(&work)
                .args(["add", "source"]),
        );
        invoke(
            Command::new(&git_bin)
                .arg("-C")
                .arg(&work)
                .args(["commit", "-m", "private task"]),
        );
        let tip = invoke(
            Command::new(&git_bin)
                .arg("-C")
                .arg(&work)
                .args(["rev-parse", "HEAD"]),
        );
        invoke(Command::new(&git_bin).arg("-C").arg(&work).args([
            "push",
            "--force-with-lease",
            "-u",
            "origin",
            "task",
        ]));
        assert_eq!(
            invoke(
                Command::new(&git_bin)
                    .arg("--git-dir")
                    .arg(&origin)
                    .args(["rev-parse", "refs/heads/task"])
            ),
            tip
        );
        prepare(&home, &state, &plan, uid, &tools, &lock).unwrap();
        assert_eq!(
            invoke(
                Command::new(&git_bin)
                    .arg("-C")
                    .arg(&work)
                    .args(["rev-parse", "HEAD"])
            ),
            tip
        );
    }
}
