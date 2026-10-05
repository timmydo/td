//! A real `td-jail --workspace` launch. Ignored by default: it needs
//! unprivileged user namespaces and a host `/bin/sh`, which the build
//! sandbox does not promise. Run it with `cargo test -- --ignored`.

use std::error::Error;
use std::fs;
use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixDatagram, UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const PROBE: &str = r#"#!/bin/sh
read -r request
echo "request=$request"
echo "uid=$(id -u) owner=$(stat -c %u .)"
echo "cwd=$(pwd)"
echo "home=$HOME path=$PATH"
touch built && echo "worktree=writable"
touch "$HOME/h" && echo "home=writable"
touch "$SHARED/x" 2>/dev/null || echo "shared=read-only"
touch /usr/x 2>/dev/null || echo "usr=read-only"
printf '#!/bin/sh\necho ran\n' > "$TMPDIR/s" && chmod +x "$TMPDIR/s" && "$TMPDIR/s" | sed 's/^/tmp=/'
grep -q '^Seccomp:[[:space:]]*2' /proc/self/status && echo "seccomp=filtered"
grep -q '^CapEff:[[:space:]]*0*$' /proc/self/status && echo "capabilities=none"
ls /proc | grep -c '^[0-9]' | sed 's/^/processes=/'
ls -A /etc | tr '\n' ' ' | sed 's/^/etc=/'; echo
ls / | tr '\n' ' ' | sed 's/^/root=/'; echo
"#;

/// A scratch workspace under `name` with the shell probe as its entry;
/// returns its directory and its spec.
fn prepare(name: &str) -> Result<(PathBuf, PathBuf), Box<dyn Error>> {
    prepare_with(name, None)
}

/// As `prepare`, with `entry` as the entry when given.
fn prepare_with(name: &str, entry: Option<&Path>) -> Result<(PathBuf, PathBuf), Box<dyn Error>> {
    let base = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = fs::remove_dir_all(&base);
    for name in ["home", "tree", "shared", "bin"] {
        fs::create_dir_all(base.join(name))?;
    }
    let base = fs::canonicalize(&base)?;
    fs::set_permissions(base.join("home"), fs::Permissions::from_mode(0o700))?;
    let probe = base.join("bin/probe");
    let entry = entry.map_or(probe.clone(), Path::to_path_buf);
    let shared = base.join("shared");
    fs::write(
        &probe,
        PROBE.replace(
            "$SHARED",
            shared.to_str().ok_or("scratch path is not UTF-8")?,
        ),
    )?;
    fs::set_permissions(&probe, fs::Permissions::from_mode(0o755))?;
    let spec = base.join("spec");
    fs::write(
        &spec,
        format!(
            "format=1\nentry={}\npath=/usr\nhome={}\nworktree={}\nread={}\n",
            entry.display(),
            base.join("home").display(),
            base.join("tree").display(),
            shared.display()
        ),
    )?;
    fs::set_permissions(&spec, fs::Permissions::from_mode(0o600))?;
    Ok((base, spec))
}

#[test]
#[ignore = "needs unprivileged user namespaces and a host /bin/sh"]
fn a_workspace_launch_is_confined_and_speaks_on_its_channel() -> Result<(), Box<dyn Error>> {
    let (base, spec) = prepare("workspace-launch")?;
    let (mut ours, theirs) = UnixStream::pair()?;
    let child = Command::new(env!("CARGO_BIN_EXE_td-jail"))
        .arg("--workspace")
        .arg(std::process::id().to_string())
        .arg(&spec)
        .stdin(Stdio::from(OwnedFd::from(theirs.try_clone()?)))
        .stdout(Stdio::from(OwnedFd::from(theirs)))
        .stderr(Stdio::piped())
        .spawn()?;
    ours.write_all(b"hello\n")?;
    let mut reply = String::new();
    ours.read_to_string(&mut reply)?;
    let output = child.wait_with_output()?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{reply}\n{stderr}");

    let uid = fs::metadata(&base)?.uid();
    for line in [
        "request=hello".to_string(),
        format!("uid={uid} owner={uid}"),
        format!("cwd={}", base.join("tree").display()),
        "worktree=writable".into(),
        "home=writable".into(),
        // The spec's path directory first, then the fixed list.
        "path=/usr:/usr/local/bin:/usr/bin:/bin:".into(),
        "shared=read-only".into(),
        "usr=read-only".into(),
        "tmp=ran".into(),
        "seccomp=filtered".into(),
        "capabilities=none".into(),
        "etc=".into(),
    ] {
        assert!(reply.contains(&line), "{line} absent from:\n{reply}");
    }
    assert!(base.join("tree/built").exists());
    let etc = reply
        .lines()
        .find(|line| line.starts_with("etc="))
        .ok_or("no etc line")?;
    for absent in ["shadow", "sudoers", "ssh"] {
        assert!(!etc.split(' ').any(|name| name == absent), "{etc}");
    }
    let root = reply
        .lines()
        .find(|line| line.starts_with("root="))
        .ok_or("no root line")?;
    assert!(
        !root.split(' ').any(|name| name == "root" || name == "boot"),
        "{root}"
    );
    fs::remove_dir_all(&base)?;
    Ok(())
}

const CHAIN_PROBE: &str = r#"#!/bin/sh
read -r request
cd "$C" || exit 1
t() { if eval "$2" 2>/dev/null; then echo "$1=yes"; else echo "$1=no"; fi; }
t objects 'touch "$R/objects/x"'
t alternates 'echo x >> "$R/objects/info/alternates"'
t alternates-new 'touch "$R/objects/info/new"'
t objects-mv 'mv "$R/objects" "$R/o2"'
t refs 'touch "$R/refs/x"'
for f in commondir config config.worktree shallow; do
  t "$f" 'echo x >> "$R/$f"'
  t "$f-rm" 'rm -f "$R/$f"'
  t "$f-mv" 'mv "$R/$f" "$R/$f.2"'
done
for d in branches hooks info remotes worktrees; do
  t "$d" 'touch "$R/$d/x"'
  t "$d-mv" 'mv "$R/$d" "$R/$d.2"'
done
t linked 'touch "$R/worktrees/id/x"'
for f in commondir config.worktree gitdir; do
  t "linked-$f" 'echo x >> "$R/worktrees/id/$f"'
  t "linked-$f-rm" 'rm -f "$R/worktrees/id/$f"'
done
t linked-new 'mkdir "$R/worktrees/new"'
t linked-mv 'mv "$R/worktrees/id" "$R/worktrees/id2"'
t dotgit 'echo x >> "$C/.git"'
t dotgit-rm 'rm -f "$C/.git"'
t tree 'touch "$C/f"'
t store 'touch "$S/x"'
if [ -n "$GIT" ]; then
  "$GIT" add f && "$GIT" -c user.name=t -c user.email=t@t commit -q -m one && echo "commit=made"
  "$GIT" --git-dir="$R" log --format=%s -1 main | sed 's/^/root-log=/'
  "$GIT" rev-parse --is-shallow-repository | sed 's/^/is-shallow=/'
fi
"#;

/// The git mount chain (td-agent/DESIGN.md §8) over a linked worktree
/// laid out as td-agent lays it: what tells git what to execute or where
/// to look is read-only, no link of the chain moves, and a jailed git
/// still commits.
#[test]
#[ignore = "needs unprivileged user namespaces and a host /bin/sh"]
fn the_git_chain_protects_a_repository_and_a_jailed_git_commits() -> Result<(), Box<dyn Error>> {
    let (base, _) = prepare("workspace-chain")?;
    let store = base.join("store/objects");
    let repository = base.join("ws/r.git");
    let checkout = base.join("tree/r");
    for dir in [
        "store/objects/info",
        "store/objects/pack",
        "ws/r.git/objects/info",
        "ws/r.git/objects/pack",
        "ws/r.git/refs/heads",
        "ws/r.git/refs/tags",
        "ws/r.git/branches",
        "ws/r.git/hooks",
        "ws/r.git/info",
        "ws/r.git/remotes",
        "ws/r.git/worktrees/id",
        "tree/r",
    ] {
        fs::create_dir_all(base.join(dir))?;
    }
    let write = |path: PathBuf, text: String| fs::write(path, text);
    write(repository.join("HEAD"), "ref: refs/heads/main\n".into())?;
    write(
        repository.join("config"),
        "[core]\n\trepositoryformatversion = 0\n\tbare = true\n".into(),
    )?;
    // The root's `commondir` names itself: git refuses an empty one.
    write(repository.join("commondir"), ".\n".into())?;
    for empty in ["config.worktree", "shallow"] {
        write(repository.join(empty), String::new())?;
    }
    write(
        repository.join("objects/info/alternates"),
        format!("{}\n", store.display()),
    )?;
    let linked = repository.join("worktrees/id");
    write(linked.join("HEAD"), "ref: refs/heads/main\n".into())?;
    write(linked.join("commondir"), "../..\n".into())?;
    write(
        linked.join("gitdir"),
        format!("{}\n", checkout.join(".git").display()),
    )?;
    write(linked.join("config.worktree"), String::new())?;
    write(
        checkout.join(".git"),
        format!("gitdir: {}\n", linked.display()),
    )?;
    let probe = base.join("bin/chain");
    write(
        probe.clone(),
        CHAIN_PROBE
            .replace("$C", &checkout.display().to_string())
            .replace("$R", &repository.display().to_string())
            .replace("$S", &store.display().to_string())
            .replace("$GIT", &bound_git().unwrap_or_default()),
    )?;
    fs::set_permissions(&probe, fs::Permissions::from_mode(0o755))?;
    let spec = base.join("chain-spec");
    write(
        spec.clone(),
        format!(
            "format=1\nentry={}\nhome={}\ncheckout={}\nrepository={}\nread={}\n",
            probe.display(),
            base.join("home").display(),
            checkout.display(),
            repository.display(),
            store.display()
        ),
    )?;
    fs::set_permissions(&spec, fs::Permissions::from_mode(0o600))?;
    let (mut ours, theirs) = UnixStream::pair()?;
    let child = Command::new(env!("CARGO_BIN_EXE_td-jail"))
        .arg("--workspace")
        .arg(std::process::id().to_string())
        .arg(&spec)
        .stdin(Stdio::from(OwnedFd::from(theirs.try_clone()?)))
        .stdout(Stdio::from(OwnedFd::from(theirs)))
        .stderr(Stdio::piped())
        .spawn()?;
    // A refused launch closes the channel; its reason is on stderr.
    let _ = ours.write_all(b"go\n");
    let mut reply = String::new();
    let _ = ours.read_to_string(&mut reply);
    let output = child.wait_with_output()?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{reply}\n{stderr}");
    let mut expected: Vec<String> = [
        "objects=yes",
        "alternates=no",
        "alternates-new=no",
        "objects-mv=no",
        "refs=yes",
        "linked=yes",
        "linked-new=no",
        "linked-mv=no",
        "dotgit=no",
        "dotgit-rm=no",
        "tree=yes",
        "store=no",
    ]
    .iter()
    .map(|line| line.to_string())
    .collect();
    for name in ["commondir", "config", "config.worktree", "shallow"] {
        for probe in ["", "-rm", "-mv"] {
            expected.push(format!("{name}{probe}=no"));
        }
    }
    for name in ["branches", "hooks", "info", "remotes", "worktrees"] {
        expected.push(format!("{name}=no"));
        expected.push(format!("{name}-mv=no"));
    }
    for name in ["commondir", "config.worktree", "gitdir"] {
        expected.push(format!("linked-{name}=no"));
        expected.push(format!("linked-{name}-rm=no"));
    }
    for line in &expected {
        assert!(
            reply.lines().any(|got| got == line),
            "{line} absent from:\n{reply}"
        );
    }
    assert_eq!(
        fs::read_to_string(repository.join("config"))?,
        "[core]\n\trepositoryformatversion = 0\n\tbare = true\n"
    );
    if bound_git().is_some() {
        assert!(
            // The empty protected `shallow` makes git call it shallow.
            ["commit=made", "root-log=one", "is-shallow=true"]
                .iter()
                .all(|line| reply.lines().any(|got| got == *line)),
            "{reply}\n{stderr}"
        );
        assert!(fs::read_to_string(repository.join("refs/heads/main")).is_ok());
    }
    fs::remove_dir_all(&base)?;
    Ok(())
}

/// The host's git as the jail sees it: where the first `git` on PATH
/// resolves, when that lies in a system tree the jail binds (a store-based
/// host keeps it in a profile the jail lacks).
fn bound_git() -> Option<String> {
    let path = std::env::var_os("PATH")?;
    let found = std::env::split_paths(&path)
        .map(|dir| dir.join("git"))
        .find(|candidate| candidate.is_file())?;
    let resolved = fs::canonicalize(found).ok()?;
    ["/usr/", "/bin/", "/gnu/", "/nix/"]
        .iter()
        .any(|tree| resolved.starts_with(tree))
        .then(|| resolved.display().to_string())
}

/// A datagram pair is no channel: it could address any socket on the host,
/// and the filter leaves `sendto` alone.
#[test]
#[ignore = "needs unprivileged user namespaces and a host /bin/sh"]
fn a_datagram_channel_is_refused() -> Result<(), Box<dyn Error>> {
    let (base, spec) = prepare("workspace-datagram")?;
    let (_ours, theirs) = UnixDatagram::pair()?;
    let output = Command::new(env!("CARGO_BIN_EXE_td-jail"))
        .arg("--workspace")
        .arg(std::process::id().to_string())
        .arg(&spec)
        .stdin(Stdio::from(OwnedFd::from(theirs.try_clone()?)))
        .stdout(Stdio::from(OwnedFd::from(theirs)))
        .stderr(Stdio::piped())
        .output()?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(
        stderr.contains("not an unnamed, connected Unix stream socket"),
        "{stderr}"
    );
    fs::remove_dir_all(&base)?;
    Ok(())
}

/// The workspace program as the kernel applies it, seen from a Rust
/// program inside: no Unix socket of its own, a stream pair but no
/// datagram one, a child spawned on std's fork path (which makes a
/// sequenced-packet pair), and loopback alone. The entry is this
/// test binary, running `rust_probe_inside`.
#[test]
#[ignore = "needs unprivileged user namespaces and a host /bin/sh"]
fn the_workspace_filter_holds_for_a_rust_program() -> Result<(), Box<dyn Error>> {
    let me = fs::canonicalize(std::env::current_exe()?)?;
    let (base, spec) = prepare_with("workspace-rust", Some(&me))?;
    let (mut ours, theirs) = UnixStream::pair()?;
    let child = Command::new(env!("CARGO_BIN_EXE_td-jail"))
        .arg("--workspace")
        .arg(std::process::id().to_string())
        .arg(&spec)
        .args([
            "--exact",
            "rust_probe_inside",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .stdin(Stdio::from(OwnedFd::from(theirs.try_clone()?)))
        .stdout(Stdio::from(OwnedFd::from(theirs)))
        .stderr(Stdio::piped())
        .spawn()?;
    let mut reply = String::new();
    ours.read_to_string(&mut reply)?;
    let output = child.wait_with_output()?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{reply}\n{stderr}");
    for line in [
        "unix-socket=refused",
        "stream-pair=made",
        "datagram-pair=refused",
        "fork-path-spawn=ran",
        "interfaces=lo",
    ] {
        // Whole lines: `interfaces=lo,eth0` is no isolated network.
        assert!(
            reply.lines().any(|got| got == line),
            "{line} absent from:\n{reply}"
        );
    }
    fs::remove_dir_all(&base)?;
    Ok(())
}

/// Run inside the jail by the test above; outside it does nothing.
#[test]
#[ignore = "run inside a workspace instance"]
fn rust_probe_inside() -> Result<(), Box<dyn Error>> {
    if !std::env::current_exe()?.starts_with("/opt/workspace/bin") {
        return Ok(());
    }
    let refused = |result: std::io::Result<()>| match result {
        Err(error) if error.raw_os_error() == Some(1) => "refused",
        Err(_) => "failed-otherwise",
        Ok(()) => "made",
    };
    let home = std::env::var("HOME")?;
    // Off libtest's own `test ... ` line, so each report is a whole line.
    println!();
    println!(
        "unix-socket={}",
        refused(UnixListener::bind(Path::new(&home).join("s")).map(drop))
    );
    println!("stream-pair={}", refused(UnixStream::pair().map(drop)));
    println!("datagram-pair={}", refused(UnixDatagram::pair().map(drop)));
    // A bare name with a changed PATH takes std's fork path, which makes a
    // sequenced-packet pair first: this fails with EPERM without it.
    let ran = Command::new("sh")
        .env("PATH", "/usr/bin:/bin:/run/current-system/profile/bin")
        .args(["-c", "exit 0"])
        .status()
        .is_ok_and(|status| status.success());
    println!("fork-path-spawn={}", if ran { "ran" } else { "failed" });
    let interfaces: Vec<String> = fs::read_to_string("/proc/net/dev")?
        .lines()
        .skip(2)
        .filter_map(|line| line.split(':').next().map(|name| name.trim().to_string()))
        .collect();
    println!("interfaces={}", interfaces.join(","));
    Ok(())
}
