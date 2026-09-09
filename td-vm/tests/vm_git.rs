//! Host Git protocol tests; the sandbox gate runs --bins without host Git.
use std::error::Error;
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{symlink, DirBuilderExt, OpenOptionsExt};
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

type Result<T> = std::result::Result<T, Box<dyn Error>>;
const BIN: &str = env!("CARGO_BIN_EXE_td-vm-git");
const KEY_A: &str = "AAAAC3NzaC1lZDI1NTE5AAAAIAEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEB";
const KEY_B: &str = "AAAAC3NzaC1lZDI1NTE5AAAAIAICAgICAgICAgICAgICAgICAgICAgICAgICAgICAgIC";
const A: &str = "0123456789abcdef0123456789abcdef";
const B: &str = "1123456789abcdef0123456789abcdef";

// Only these permission fixtures need a mapped filesystem root. Other
// td-vm host tests keep their existing network/QEMU environment.
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

struct Fixture {
    root: PathBuf,
    git: PathBuf,
    oid: String,
    pack: Vec<u8>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn program(name: &str) -> Result<PathBuf> {
    for parent in std::env::split_paths(&std::env::var_os("PATH").ok_or("no PATH")?) {
        let candidate = parent.join(name);
        if candidate.is_file() {
            return Ok(fs::canonicalize(candidate)?);
        }
    }
    Err(format!("missing host fixture program {name}").into())
}

impl Fixture {
    fn new(format: &str) -> Result<Self> {
        let root = std::env::temp_dir().join(format!(
            "td-vm-git-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
        ));
        DirBuilder::new().mode(0o700).create(&root)?;
        let mut fixture = Self {
            root,
            git: program("git")?,
            oid: String::new(),
            pack: Vec::new(),
        };
        fixture.git(
            &[
                "init",
                "--bare",
                "--initial-branch=main",
                &format!("--object-format={format}"),
                "origin.git",
            ],
            &[],
        )?;
        fixture.git(
            &[
                "init",
                "--initial-branch=main",
                &format!("--object-format={format}"),
                "seed",
            ],
            &[],
        )?;
        fixture.git(
            &["-C", "seed", "commit", "--allow-empty", "-m", "baseline"],
            &[],
        )?;
        fixture.oid = String::from_utf8(fixture.git(&["-C", "seed", "rev-parse", "HEAD"], &[])?)?
            .trim()
            .into();
        fixture.pack = fixture.git(&["-C", "seed", "pack-objects", "--stdout", "--all"], &[])?;
        fixture.git(&["-C", "seed", "push", "../origin.git", "main"], &[])?;
        let text = format!(
            "TDVM-GIT-2\ndispatcher={BIN}\nkey={A} {KEY_A}\nkey={B} {KEY_B}\nrepository={}\ngit={}\npath={}\nbranch={A} vm-a\nbranch={B} vm-b\n",
            fixture.repo().display(),
            fixture.git.display(),
            std::env::var("PATH")?
        );
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(fixture.root.join("policy"))?
            .write_all(text.as_bytes())?;
        Ok(fixture)
    }

    fn repo(&self) -> PathBuf {
        self.root.join("origin.git")
    }

    fn git(&self, args: &[&str], input: &[u8]) -> Result<Vec<u8>> {
        let mut child = Command::new(&self.git)
            .current_dir(&self.root)
            .args(args)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").ok_or("no PATH")?)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "Fixture")
            .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
            .env("GIT_COMMITTER_NAME", "Fixture")
            .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        child.stdin.take().ok_or("no stdin")?.write_all(input)?;
        let output = child.wait_with_output()?;
        if !output.status.success() {
            return Err(
                format!("Git {args:?}: {}", String::from_utf8_lossy(&output.stderr)).into(),
            );
        }
        Ok(output.stdout)
    }

    fn serve(&self, id: &str, command: &str, input: &[u8]) -> Result<Output> {
        let mut child = Command::new(BIN)
            .args(["serve"])
            .arg(self.root.join("policy"))
            .arg(id)
            .arg(if id == A { KEY_A } else { KEY_B })
            .env("SSH_ORIGINAL_COMMAND", command)
            // A caller cannot turn off hooks or choose another identity.
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "core.hooksPath")
            .env("GIT_CONFIG_VALUE_0", "/dev/null")
            .env("TD_VM_GIT_INSTANCE", B)
            .env("TD_VM_GIT_HOOKS", "/dev/null")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        child.stdin.take().ok_or("no stdin")?.write_all(input)?;
        Ok(child.wait_with_output()?)
    }

    fn push(&self, id: &str, updates: &[(&str, &str, &str)]) -> Result<String> {
        let mut input = Vec::new();
        for (index, (old, new, name)) in updates.iter().enumerate() {
            let caps = if index == 0 {
                format!(
                    "\0report-status atomic object-format={}",
                    if self.oid.len() == 40 {
                        "sha1"
                    } else {
                        "sha256"
                    }
                )
            } else {
                String::new()
            };
            let payload = format!("{old} {new} {name}{caps}\n");
            write!(input, "{:04x}{payload}", payload.len() + 4)?;
        }
        input.extend_from_slice(b"0000");
        input.extend_from_slice(&self.pack);
        let output = self.serve(
            id,
            &format!("git-receive-pack '{}'", self.repo().display()),
            &input,
        )?;
        Ok(format!(
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ))
    }

    fn refs(&self) -> Result<String> {
        Ok(String::from_utf8(self.git(
            &[
                "--git-dir=origin.git",
                "for-each-ref",
                "--format=%(refname)",
            ],
            &[],
        )?)?)
    }
}

#[test]
fn real_receive_restricts_each_instance_and_preserves_host_hooks() -> Result<()> {
    if in_trusted_root("real_receive_restricts_each_instance_and_preserves_host_hooks")? {
        return Ok(());
    }
    for format in ["sha1", "sha256"] {
        let f = Fixture::new(format)?;
        let zero = "0".repeat(f.oid.len());
        let result = f.push(A, &[(&zero, &f.oid, "refs/heads/vm-a")])?;
        assert!(result.contains("ok refs/heads/vm-a"), "{result}");
        assert!(f.refs()?.contains("refs/heads/vm-a"));
        for (old, new, name) in [
            (zero.as_str(), f.oid.as_str(), "refs/heads/vm-b"),
            (zero.as_str(), f.oid.as_str(), "refs/tags/guest"),
            (f.oid.as_str(), zero.as_str(), "refs/heads/main"),
            (f.oid.as_str(), zero.as_str(), "refs/heads/vm-a"),
            (zero.as_str(), f.oid.as_str(), "refs/heads/vm-a/child"),
            (zero.as_str(), f.oid.as_str(), "refs/td-vm/retained"),
        ] {
            let result = f.push(A, &[(old, new, name)])?;
            assert!(
                result.contains("pre-receive hook declined"),
                "{name}: {result}"
            );
        }
        let result = f.push(
            B,
            &[
                (&zero, &f.oid, "refs/heads/vm-b"),
                (&f.oid, &zero, "refs/heads/vm-a"),
            ],
        )?;
        assert!(result.contains("pre-receive hook declined"), "{result}");
        assert!(!f.refs()?.contains("refs/heads/vm-b"));
        // A relative core.hooksPath still runs the existing pre-receive.
        fs::create_dir(f.repo().join("custom-hooks"))?;
        f.git(
            &[
                "--git-dir=origin.git",
                "config",
                "core.hooksPath",
                "custom-hooks",
            ],
            &[],
        )?;
        symlink(program("false")?, f.repo().join("custom-hooks/pre-receive"))?;
        let result = f.push(B, &[(&zero, &f.oid, "refs/heads/vm-b")])?;
        assert!(result.contains("pre-receive hook declined"), "{result}");
        fs::remove_file(f.repo().join("custom-hooks/pre-receive"))?;
        symlink(program("false")?, f.repo().join("custom-hooks/update"))?;
        let result = f.push(B, &[(&zero, &f.oid, "refs/heads/vm-b")])?;
        assert!(result.contains("hook declined"), "{result}");
        fs::remove_file(f.repo().join("custom-hooks/update"))?;
        let result = f.push(B, &[(&zero, &f.oid, "refs/heads/vm-b")])?;
        assert!(result.contains("ok refs/heads/vm-b"), "{result}");
        assert_eq!(
            fs::read_dir(&f.root)?.count(),
            3,
            "session directories were not cleaned"
        );
    }
    Ok(())
}

#[test]
fn endpoint_refuses_unregistered_identity_arbitrary_commands_and_exposed_policy() -> Result<()> {
    if in_trusted_root(
        "endpoint_refuses_unregistered_identity_arbitrary_commands_and_exposed_policy",
    )? {
        return Ok(());
    }
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new("sha1")?;
    for (id, command) in [
        (A, "sh".into()),
        (A, "git-upload-pack '/somewhere/else'".into()),
        ("f", format!("git-upload-pack '{}'", f.repo().display())),
    ] {
        assert!(!f.serve(id, &command, &[])?.status.success());
    }
    f.git(
        &[
            "--git-dir=origin.git",
            "config",
            "receive.procReceiveRefs",
            "refs/heads/",
        ],
        &[],
    )?;
    let rewritten = f.serve(
        A,
        &format!("git-receive-pack '{}'", f.repo().display()),
        &[],
    )?;
    assert!(!rewritten.status.success());
    assert!(String::from_utf8_lossy(&rewritten.stderr).contains("ref rewriting is unsupported"));
    f.git(
        &[
            "--git-dir=origin.git",
            "config",
            "--unset",
            "receive.procReceiveRefs",
        ],
        &[],
    )?;
    f.git(
        &[
            "--git-dir=origin.git",
            "config",
            "hook.probe.event",
            "pre-receive",
        ],
        &[],
    )?;
    let chain = f.serve(
        A,
        &format!("git-receive-pack '{}'", f.repo().display()),
        &[],
    )?;
    assert!(!chain.status.success());
    assert!(
        String::from_utf8_lossy(&chain.stderr).contains("configured hook chains are unsupported")
    );
    fs::set_permissions(f.root.join("policy"), fs::Permissions::from_mode(0o644))?;
    assert!(!f
        .serve(A, &format!("git-upload-pack '{}'", f.repo().display()), &[])?
        .status
        .success());
    Ok(())
}

#[test]
fn receive_hook_rereads_policy_and_refuses_changed_session_profile() -> Result<()> {
    if in_trusted_root("receive_hook_rereads_policy_and_refuses_changed_session_profile")? {
        return Ok(());
    }
    let f = Fixture::new("sha1")?;
    let hook = f.root.join("pre-receive");
    symlink(BIN, &hook)?;
    let policy_path = f.root.join("policy");
    let policy = fs::read_to_string(&policy_path)?;
    let invoke = || -> Result<bool> {
        let mut child = Command::new(&hook)
            .env_clear()
            .env("TD_VM_GIT_POLICY", &policy_path)
            .env("TD_VM_GIT_INSTANCE", A)
            .env("TD_VM_GIT_KEY", KEY_A)
            .env("TD_VM_GIT_REPOSITORY", f.repo())
            .env("TD_VM_GIT_EXECUTABLE", &f.git)
            .env("TD_VM_GIT_PATH", std::env::var("PATH")?)
            .env("TD_VM_GIT_FORMAT", "sha1")
            .env("TD_VM_GIT_HOOKS", f.repo().join("hooks"))
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        let input = format!("{} {} refs/heads/vm-a\n", "0".repeat(40), f.oid);
        // A revoked session can exit before reading its pipe.
        let _ = child
            .stdin
            .take()
            .ok_or("no stdin")?
            .write_all(input.as_bytes());
        Ok(child.wait()?.success())
    };
    assert!(invoke()?);
    for updated in [
        policy.replace(&format!("branch={A} vm-a\n"), ""),
        policy.replace(
            &format!("branch={A} vm-a"),
            &format!("branch={A} reassigned"),
        ),
        policy.replace("repository=", "repository=/changed"),
        policy.replace("git=", "git=/changed"),
        policy.replace("path=", "path=/changed:"),
    ] {
        fs::write(&policy_path, updated)?;
        assert!(!invoke()?);
    }
    Ok(())
}

#[test]
fn git_launches_original_hooks_with_their_adjacent_resources_and_shell_fallback() -> Result<()> {
    if in_trusted_root(
        "git_launches_original_hooks_with_their_adjacent_resources_and_shell_fallback",
    )? {
        return Ok(());
    }
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new("sha1")?;
    let hooks = f.root.join("shared-hooks");
    fs::create_dir(&hooks)?;
    fs::write(hooks.join("sibling"), "host resource")?;
    f.git(
        &[
            "--git-dir=origin.git",
            "config",
            "core.hooksPath",
            "../shared-hooks",
        ],
        &[],
    )?;
    for name in ["pre-receive", "update", "post-receive"] {
        // Deliberately no shebang: this exercises Git's ENOEXEC fallback.
        fs::write(
            hooks.join(name),
            "test -f \"${0%/*}/sibling\" || exit 1\nprintf '%s\\n' \"$0\" > \"${0}.called\"\n",
        )?;
        fs::set_permissions(hooks.join(name), fs::Permissions::from_mode(0o700))?;
    }
    let result = f.push(A, &[(&"0".repeat(40), &f.oid, "refs/heads/vm-a")])?;
    assert!(result.contains("ok refs/heads/vm-a"), "{result}");
    for name in ["pre-receive", "update", "post-receive"] {
        assert_eq!(
            fs::read_to_string(hooks.join(format!("{name}.called")))?,
            format!("../shared-hooks/{name}\n")
        );
    }
    Ok(())
}

#[test]
fn early_success_hook_accepts_a_transaction_larger_than_pipe_capacity() -> Result<()> {
    if in_trusted_root("early_success_hook_accepts_a_transaction_larger_than_pipe_capacity")? {
        return Ok(());
    }
    let f = Fixture::new("sha1")?;
    symlink(program("true")?, f.repo().join("hooks/pre-receive"))?;
    let names: Vec<_> = (0..1500)
        .map(|index| format!("refs/heads/topic-{index}"))
        .collect();
    let mut policy = fs::read_to_string(f.root.join("policy"))?;
    for index in 0..1500 {
        policy.push_str(&format!("branch={A} topic-{index}\n"));
    }
    fs::write(f.root.join("policy"), policy)?;
    let zero = "0".repeat(40);
    let updates: Vec<_> = names
        .iter()
        .map(|name| (zero.as_str(), f.oid.as_str(), name.as_str()))
        .collect();
    let result = f.push(A, &updates)?;
    assert!(!result.contains("hook declined"), "{result}");
    assert!(result.contains("ok refs/heads/topic-1499"), "{result}");
    assert_eq!(f.refs()?.lines().count(), 1501);
    Ok(())
}

#[test]
fn reserved_symbolic_ref_cannot_update_main() -> Result<()> {
    if in_trusted_root("reserved_symbolic_ref_cannot_update_main")? {
        return Ok(());
    }
    let mut f = Fixture::new("sha1")?;
    let baseline = f.oid.clone();
    f.git(
        &[
            "--git-dir=origin.git",
            "symbolic-ref",
            "refs/heads/vm-a",
            "refs/heads/main",
        ],
        &[],
    )?;
    f.git(
        &[
            "-C",
            "seed",
            "commit",
            "--allow-empty",
            "-m",
            "topic change",
        ],
        &[],
    )?;
    f.oid = String::from_utf8(f.git(&["-C", "seed", "rev-parse", "HEAD"], &[])?)?
        .trim()
        .into();
    f.pack = f.git(&["-C", "seed", "pack-objects", "--stdout", "--all"], &[])?;
    let result = f.push(A, &[(&baseline, &f.oid, "refs/heads/vm-a")])?;
    assert!(result.contains("pre-receive hook declined"), "{result}");
    assert_eq!(
        String::from_utf8(f.git(&["--git-dir=origin.git", "rev-parse", "main"], &[])?)?.trim(),
        baseline
    );
    f.git(
        &[
            "--git-dir=origin.git",
            "symbolic-ref",
            "refs/heads/vm-a",
            "refs/heads/vm-b",
        ],
        &[],
    )?;
    let dangling = f.push(A, &[(&"0".repeat(40), &f.oid, "refs/heads/vm-a")])?;
    assert!(dangling.contains("pre-receive hook declined"), "{dangling}");
    assert!(!f.refs()?.contains("refs/heads/vm-b"));
    Ok(())
}

fn admin(args: &[&str], success: bool) -> Result<Output> {
    let output = Command::new(BIN).args(args).stdin(Stdio::null()).output()?;
    assert_eq!(
        output.status.success(),
        success,
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(output)
}

fn initialize_registry(f: &Fixture) -> Result<PathBuf> {
    let path = f.root.join("profile 'quoted'").join("registry");
    admin(
        &[
            "init",
            path.to_str().ok_or("path")?,
            f.repo().to_str().ok_or("repo")?,
            f.git.to_str().ok_or("git")?,
            &std::env::var("PATH")?,
            BIN,
        ],
        true,
    )?;
    Ok(path)
}

#[test]
fn enrollment_lookup_reservations_and_revocation_publish_one_registry() -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    if in_trusted_root("enrollment_lookup_reservations_and_revocation_publish_one_registry")? {
        return Ok(());
    }
    let f = Fixture::new("sha1")?;
    let registry = initialize_registry(&f)?;
    let path = registry.to_str().ok_or("registry path")?;
    let key_file = f.root.join("public-key");
    let key_path = key_file.to_str().ok_or("key path")?;
    fs::write(
        &key_file,
        format!("ssh-ed25519 {KEY_A} ignored human comment\n"),
    )?;
    assert!(
        admin(&["authorized-keys", path, "ssh-ed25519", KEY_A], true)?
            .stdout
            .is_empty()
    );
    admin(&["enroll", path, A, "topic-a", key_path], true)?;
    assert_eq!(
        String::from_utf8(f.git(
            &["--git-dir=origin.git", "rev-parse", "refs/heads/topic-a"],
            &[]
        )?)?
        .trim(),
        f.oid
    );
    let lookup = admin(&["authorized-keys", path, "ssh-ed25519", KEY_A], true)?;
    let line = String::from_utf8(lookup.stdout)?;
    assert!(line.starts_with("restrict,command=\""));
    assert!(line.contains("profile '\\''quoted'\\''/registry"), "{line}");
    assert!(line.contains(&format!("{A} {KEY_A}")));
    assert!(line.ends_with(&format!("ssh-ed25519 {KEY_A} td-vm:{A}\n")));
    assert_eq!(line.lines().count(), 1);
    assert!(admin(&["authorized-keys", path, "ssh-rsa", KEY_A], true)?
        .stdout
        .is_empty());
    let before = fs::read(&registry)?;
    let inode = fs::metadata(&registry)?.ino();
    admin(&["enroll", path, A, "topic-a", key_path], true)?;
    assert_eq!(
        fs::metadata(&registry)?.ino(),
        inode,
        "idempotent enrollment must not rewrite"
    );
    admin(&["enroll", path, B, "topic-b", key_path], false)?;
    assert_eq!(fs::read(&registry)?, before);
    fs::write(&key_file, format!("ssh-ed25519 {KEY_B}\n"))?;
    admin(&["enroll", path, B, "topic-a", key_path], false)?;
    admin(&["enroll", path, B, "topic-b", key_path], true)?;
    admin(&["reserve", path, A, "topic-a/child"], false)?;
    admin(&["reserve", path, A, "main"], false)?;
    f.git(
        &[
            "--git-dir=origin.git",
            "update-ref",
            "refs/heads/human-topic",
            &f.oid,
        ],
        &[],
    )?;
    admin(&["reserve", path, A, "human-topic"], false)?;
    admin(&["reserve", path, A, "topic-extra"], true)?;
    let before = fs::read(&registry)?;
    admin(
        &[
            "init",
            path,
            f.repo().to_str().ok_or("repo")?,
            f.git.to_str().ok_or("git")?,
            &std::env::var("PATH")?,
            BIN,
        ],
        false,
    )?;
    assert_eq!(fs::read(&registry)?, before);
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .open(registry.parent().ok_or("parent")?.join(".td-vm-git.lock"))?;
    lock.try_lock()?;
    admin(&["reserve", path, A, "busy-topic"], false)?;
    assert_eq!(fs::read(&registry)?, before);
    drop(lock);
    admin(&["reserve", path, A, "busy-topic"], true)?;
    admin(&["revoke", path, A], true)?;
    assert!(
        admin(&["authorized-keys", path, "ssh-ed25519", KEY_A], true)?
            .stdout
            .is_empty()
    );
    assert!(
        !admin(&["authorized-keys", path, "ssh-ed25519", KEY_B], true)?
            .stdout
            .is_empty()
    );
    let revoked = fs::read(&registry)?;
    admin(&["revoke", path, A], true)?;
    assert_eq!(fs::read(&registry)?, revoked);
    assert_eq!(fs::metadata(&registry)?.mode() & 0o777, 0o600);
    assert_eq!(fs::read_dir(registry.parent().ok_or("parent")?)?.count(), 2);
    assert!(f.refs()?.contains("refs/heads/human-topic"));
    Ok(())
}

#[test]
fn competing_enrollments_cannot_share_a_branch_or_lose_a_key() -> Result<()> {
    if in_trusted_root("competing_enrollments_cannot_share_a_branch_or_lose_a_key")? {
        return Ok(());
    }
    let f = Fixture::new("sha1")?;
    let registry = initialize_registry(&f)?;
    let first = f.root.join("first.pub");
    let second = f.root.join("second.pub");
    fs::write(&first, format!("ssh-ed25519 {KEY_A}\n"))?;
    fs::write(&second, format!("ssh-ed25519 {KEY_B}\n"))?;
    let start = |id: &str, key: &PathBuf| {
        Command::new(BIN)
            .arg("enroll")
            .arg(&registry)
            .args([id, "contested"])
            .arg(key)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
    };
    let mut a = start(A, &first)?;
    let mut b = start(B, &second)?;
    assert_ne!(a.wait()?.success(), b.wait()?.success());
    let bytes = fs::read_to_string(&registry)?;
    assert_eq!(
        bytes
            .lines()
            .filter(|line| line.starts_with("key="))
            .count(),
        1
    );
    assert_eq!(
        bytes
            .lines()
            .filter(|line| line.starts_with("branch="))
            .count(),
        1
    );
    let a = admin(
        &[
            "authorized-keys",
            registry.to_str().ok_or("path")?,
            "ssh-ed25519",
            KEY_A,
        ],
        true,
    )?;
    let b = admin(
        &[
            "authorized-keys",
            registry.to_str().ok_or("path")?,
            "ssh-ed25519",
            KEY_B,
        ],
        true,
    )?;
    assert_ne!(a.stdout.is_empty(), b.stdout.is_empty());
    Ok(())
}

#[test]
fn old_forced_command_cannot_rejoin_a_reenrolled_instance() -> Result<()> {
    if in_trusted_root("old_forced_command_cannot_rejoin_a_reenrolled_instance")? {
        return Ok(());
    }
    let f = Fixture::new("sha1")?;
    let registry = initialize_registry(&f)?;
    let path = registry.to_str().ok_or("path")?;
    let key = f.root.join("key.pub");
    fs::write(&key, format!("ssh-ed25519 {KEY_A}\n"))?;
    admin(
        &[
            "enroll",
            path,
            A,
            "before-revoke",
            key.to_str().ok_or("key")?,
        ],
        true,
    )?;
    admin(&["revoke", path, A], true)?;
    fs::write(&key, format!("ssh-ed25519 {KEY_B}\n"))?;
    admin(
        &[
            "enroll",
            path,
            A,
            "after-revoke",
            key.to_str().ok_or("key")?,
        ],
        true,
    )?;
    let old = Command::new(BIN)
        .args(["serve", path, A, KEY_A])
        .env(
            "SSH_ORIGINAL_COMMAND",
            format!("git-upload-pack '{}'", f.repo().display()),
        )
        .stdin(Stdio::null())
        .output()?;
    assert!(!old.status.success());
    assert!(old.stdout.is_empty());
    assert!(
        admin(&["authorized-keys", path, "ssh-ed25519", KEY_A], true)?
            .stdout
            .is_empty()
    );
    assert!(
        !admin(&["authorized-keys", path, "ssh-ed25519", KEY_B], true)?
            .stdout
            .is_empty()
    );
    let alternate = f.root.join("alternate-dispatcher");
    symlink(BIN, &alternate)?;
    let legacy = Command::new(&alternate)
        .args(["serve", path, A])
        .env("TD_VM_GIT_EXECUTABLE", program("true")?)
        .env("TD_VM_GIT_HOOKS", "/tmp")
        .env("TD_VM_GIT_REPOSITORY", &f.root)
        .stdin(Stdio::null())
        .output()?;
    assert!(
        !legacy.status.success(),
        "legacy argv must not fall through to internal hook mode"
    );
    Ok(())
}

#[test]
fn concurrent_human_ref_creation_cannot_be_enrolled() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if in_trusted_root("concurrent_human_ref_creation_cannot_be_enrolled")? {
        return Ok(());
    }
    let f = Fixture::new("sha256")?;
    let registry = initialize_registry(&f)?;
    let wrapper = f.root.join("git-wrapper");
    let shell = program("sh")?;
    let quote = |value: &str| format!("'{}'", value.replace('\'', "'\\''"));
    fs::write(&wrapper, format!(
        "#!{}\nif [ \"$4\" = symbolic-ref ]; then\n  {} --git-dir {} update-ref refs/heads/human-race {} || exit 2\n  exit 1\nfi\nexec {} \"$@\"\n",
        shell.display(), quote(f.git.to_str().ok_or("git")?),
        quote(f.repo().to_str().ok_or("repo")?), f.oid,
        quote(f.git.to_str().ok_or("git")?)
    ))?;
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o700))?;
    let policy = fs::read_to_string(&registry)?.replace(
        &format!("git={}\n", f.git.display()),
        &format!("git={}\n", wrapper.display()),
    );
    fs::write(&registry, &policy)?;
    let key = f.root.join("key.pub");
    fs::write(&key, format!("ssh-ed25519 {KEY_A}\n"))?;
    admin(
        &[
            "enroll",
            registry.to_str().ok_or("registry")?,
            A,
            "human-race",
            key.to_str().ok_or("key")?,
        ],
        false,
    )?;
    assert!(f.refs()?.contains("refs/heads/human-race"));
    assert_eq!(fs::read_to_string(&registry)?, policy);
    Ok(())
}

#[test]
fn reservation_requires_head_and_respects_reference_transaction_veto() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if in_trusted_root("reservation_requires_head_and_respects_reference_transaction_veto")? {
        return Ok(());
    }
    let f = Fixture::new("sha256")?;
    let registry = initialize_registry(&f)?;
    let path = registry.to_str().ok_or("registry")?;
    let key = f.root.join("key.pub");
    fs::write(&key, format!("ssh-ed25519 {KEY_A}\n"))?;
    let key_path = key.to_str().ok_or("key")?;
    let initial = fs::read(&registry)?;
    f.git(
        &[
            "--git-dir=origin.git",
            "symbolic-ref",
            "HEAD",
            "refs/heads/missing",
        ],
        &[],
    )?;
    admin(&["enroll", path, A, "new-topic", key_path], false)?;
    assert_eq!(fs::read(&registry)?, initial);
    assert!(!f.refs()?.contains("refs/heads/new-topic"));
    f.git(
        &[
            "--git-dir=origin.git",
            "symbolic-ref",
            "HEAD",
            "refs/heads/main",
        ],
        &[],
    )?;
    let hook = f.repo().join("hooks/reference-transaction");
    fs::write(&hook, format!("#!{}\nexit 1\n", program("sh")?.display()))?;
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o700))?;
    admin(&["enroll", path, A, "new-topic", key_path], false)?;
    assert_eq!(fs::read(&registry)?, initial);
    assert!(!f.refs()?.contains("refs/heads/new-topic"));
    fs::remove_file(hook)?;
    admin(&["enroll", path, A, "new-topic", key_path], true)?;
    assert!(f.refs()?.contains("refs/heads/new-topic"));
    Ok(())
}
