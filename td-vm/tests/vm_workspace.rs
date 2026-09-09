//! Host processes prove immutable workspace binding independently of guest boot.
use std::fs::{self, File};
use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
const BIN: &str = env!("CARGO_BIN_EXE_td-vm");
struct Root(PathBuf);
impl Drop for Root {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
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
fn profile(repository: &str) -> String {
    format!("TDVM-GIT-PROFILE-1\nrepository={repository}\naddress=10.0.2.2\nport=22\nuser=test\nserver-uid=1001\nsocket=/home/test/.td-vm-registrar\nregistrar=/usr/local/libexec/td-vm-registrar\ngit=/bin/git\nhost-key=ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIAEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEB\nauthor-name=Fixture\nauthor-email=fixture@example.invalid\n")
}
fn record(home: &Path, name: &str) -> PathBuf {
    home.join("instances").join(name).join("workspace")
}
fn identity(record: &str) -> Result<&str> {
    record.lines().nth(1).ok_or_else(|| "no identity".into())
}

#[test]
fn process_workspace_snapshots_survive_defaults_retries_and_recreation() -> Result<()> {
    let root = Root(std::env::temp_dir().join(format!(
        "td-workspace-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    )));
    fs::create_dir(&root.0)?;
    let home = root.0.join("vms");
    let vm = |args: &[&str], success| {
        invoke(
            Command::new(BIN).env("TD_VM_HOME", &home).args(args),
            success,
        )
    };
    vm(&["workspace", "show", "one"], false)?;
    assert!(!home.exists());
    // Supply a tiny real qcow2 template. Firmware bytes are not executed here.
    vm(&["list"], true)?;
    let input = root.0.join("profile");
    fs::write(&input, profile("/srv/git/td.git"))?;
    fs::set_permissions(&input, fs::Permissions::from_mode(0o600))?;
    vm(
        &["git-profile", "set", input.to_str().ok_or("input path")?],
        true,
    )?;
    vm(&["workspace", "show", "missing"], false)?;
    let template = home.join("templates/base");
    fs::create_dir(&template)?;
    fs::set_permissions(&template, fs::Permissions::from_mode(0o700))?;
    invoke(
        Command::new("qemu-img")
            .args(["create", "-f", "qcow2"])
            .arg(template.join("disk.qcow2"))
            .arg("4M"),
        true,
    )?;
    vm(&["create", "one", "base", "--branch", "topic/a"], true)?;
    vm(
        &["create", "two", "base", "--branch", "topic/b", "2", "1024"],
        true,
    )?;
    let first = fs::read_to_string(record(&home, "one"))?;
    let second = fs::read_to_string(record(&home, "two"))?;
    assert_ne!(identity(&first)?, identity(&second)?);
    assert_eq!(identity(&first)?.len(), 32);
    assert_eq!(fs::metadata(record(&home, "one"))?.mode() & 0o777, 0o600);
    vm(&["create", "overlap", "base", "--branch", "topic"], false)?;
    assert!(!home.join("instances/overlap").exists());
    vm(&["create", "protected", "base", "--branch", "main"], false)?;
    let shown = vm(&["workspace", "show", "one"], true)?;
    assert!(String::from_utf8(shown.stdout)?.contains("not reserved"));
    fs::write(&input, profile("/srv/git/other.git"))?;
    vm(
        &["git-profile", "set", input.to_str().ok_or("input path")?],
        true,
    )?;
    vm(&["workspace", "prepare", "one", "topic/a"], true)?;
    vm(&["workspace", "prepare", "one", "other-branch"], false)?;
    assert_eq!(fs::read_to_string(record(&home, "one"))?, first);
    vm(&["create", "three", "base", "--branch", "topic/a"], true)?;
    assert!(fs::read_to_string(record(&home, "three"))?.contains("repository=/srv/git/other.git"));
    let create = |name| {
        Command::new(BIN)
            .env("TD_VM_HOME", &home)
            .args(["create", name, "base", "--branch", "created-branch"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
    };
    let mut a = create("creator-a")?;
    let mut b = match create("creator-b") {
        Ok(child) => child,
        Err(error) => {
            let _ = a.kill();
            let _ = a.wait();
            return Err(error.into());
        }
    };
    let a_ok = a.wait()?.success();
    let b_ok = b.wait()?.success();
    assert_ne!(
        a_ok, b_ok,
        "exactly one create may publish this planned branch"
    );
    assert_eq!(record(&home, "creator-a").exists(), a_ok);
    assert_eq!(record(&home, "creator-b").exists(), b_ok);
    // A plain VM can be prepared later without changing its disk/config.
    fs::remove_file(home.join("git-profile"))?;
    vm(&["create", "plain", "base"], true)?;
    vm(&["create", "racer-a", "base"], true)?;
    vm(&["create", "racer-b", "base"], true)?;
    vm(
        &["create", "unconfigured", "base", "--branch", "task"],
        false,
    )?;
    assert!(!record(&home, "plain").exists());
    let disk = home.join("instances/plain/disk.qcow2");
    let disk_before = fs::read(&disk)?;
    let config = fs::read(home.join("instances/plain/config"))?;
    vm(
        &["git-profile", "set", input.to_str().ok_or("input path")?],
        true,
    )?;
    let lock = File::create(home.join("locks/run-plain"))?;
    lock.lock()?;
    vm(&["workspace", "prepare", "plain", "later"], false)?;
    assert!(!record(&home, "plain").exists());
    drop(lock);
    vm(&["workspace", "prepare", "plain", "later"], true)?;
    assert_eq!(fs::read(&disk)?, disk_before);
    assert_eq!(fs::read(home.join("instances/plain/config"))?, config);
    let spawn = |name| {
        Command::new(BIN)
            .env("TD_VM_HOME", &home)
            .args(["workspace", "prepare", name, "raced-branch"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
    };
    let mut a = spawn("racer-a")?;
    let mut b = match spawn("racer-b") {
        Ok(child) => child,
        Err(error) => {
            let _ = a.kill();
            let _ = a.wait();
            return Err(error.into());
        }
    };
    let a_ok = a.wait()?.success();
    let b_ok = b.wait()?.success();
    assert_ne!(
        a_ok, b_ok,
        "exactly one process may publish this planned branch"
    );
    assert_eq!(record(&home, "racer-a").exists(), a_ok);
    assert_eq!(record(&home, "racer-b").exists(), b_ok);
    // Unknown state and aliases cannot be treated as unconfigured or deleted.
    let saved = fs::read(record(&home, "plain"))?;
    fs::write(record(&home, "plain"), "TDVM-WORKSPACE-2\nunknown\n")?;
    vm(&["workspace", "prepare", "plain", "later"], false)?;
    vm(&["delete", "plain", "--yes"], false)?;
    assert!(disk.exists());
    fs::remove_file(record(&home, "plain"))?;
    symlink(&input, record(&home, "plain"))?;
    vm(&["workspace", "prepare", "plain", "later"], false)?;
    vm(&["delete", "plain", "--yes"], false)?;
    fs::remove_file(record(&home, "plain"))?;
    fs::write(record(&home, "plain"), saved)?;
    fs::set_permissions(record(&home, "plain"), fs::Permissions::from_mode(0o600))?;
    vm(&["delete", "one", "--yes"], true)?;
    vm(&["create", "one", "base", "--branch", "recreated"], true)?;
    assert_ne!(
        identity(&fs::read_to_string(record(&home, "one"))?)?,
        identity(&first)?
    );
    assert_eq!(fs::read_to_string(record(&home, "two"))?, second);
    Ok(())
}
