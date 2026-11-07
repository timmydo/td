//! Actual restricted SSH administration, with acquisition inside the guest.
use super::super::{self as boot, release_delivery::Delivery};
use super::{Guest, Options, Result};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub(crate) mod public;

const PROFILE: &str = "system-x86-64";
const CLIENT: &str = "/var/home/tester/.td-admin-oracle/client admin-client";
const COMMIT: &str = "ffffffffffffffffffffffffffffffffffffffff";

fn copy_tree(source: &Path, destination: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(source).map_err(|e| e.to_string())?;
    if meta.is_dir() {
        fs::create_dir_all(destination).map_err(|e| e.to_string())?;
        for entry in fs::read_dir(source).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            copy_tree(&entry.path(), &destination.join(entry.file_name()))?;
        }
    } else if meta.is_file() {
        fs::copy(source, destination).map_err(|e| e.to_string())?;
    } else {
        return Err("special file in private admin fixture".into());
    }
    fs::set_permissions(destination, meta.permissions()).map_err(|e| e.to_string())
}

pub(in super::super) fn stage(fixture: &Path, seed: &Path) -> Result<()> {
    copy_tree(&fixture.join("public"), &seed.join("td/admin-fixture"))?;
    copy_tree(&fixture.join("policy"), &seed.join("@var/lib/td-admin"))
}

fn readable(path: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    let mode = if meta.is_dir() {
        0o755
    } else if meta.is_file() {
        0o644
    } else {
        return Err("special fixture file".into());
    };
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(|e| e.to_string())?;
    if meta.is_dir() {
        for entry in fs::read_dir(path).map_err(|e| e.to_string())? {
            readable(&entry.map_err(|e| e.to_string())?.path())?;
        }
    }
    Ok(())
}

fn publish(
    delivery: &Delivery,
    root: &str,
    key: &str,
    candidate: &Path,
    sequence: u64,
    head: u64,
    lifetime: u64,
    label: &str,
    public: &Path,
) -> Result<()> {
    let issued = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_secs();
    let index = delivery.command(&[
        "publish",
        &delivery.path("repo")?,
        &delivery.path(root)?,
        &delivery.path(key)?,
        PROFILE,
        COMMIT,
        &sequence.to_string(),
        &issued.to_string(),
        &(issued + 14400).to_string(),
        candidate.to_str().ok_or("candidate UTF-8")?,
    ])?;
    let promoted = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_secs();
    delivery.command(&[
        "promote",
        &delivery.path("repo")?,
        &delivery.path(root)?,
        &delivery.path(key)?,
        PROFILE,
        index.trim_end(),
        &head.to_string(),
        &promoted.to_string(),
        &(promoted + lifetime).to_string(),
    ])?;
    fs::copy(
        Path::new(&delivery.path("repo")?).join("heads/system-x86-64"),
        public.join("heads").join(label),
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

fn prepare(
    runner: &crate::check_runner::RecipeCheckRunner,
    work: &Path,
    deployment: &Path,
) -> Result<(String, String)> {
    let delivery = Delivery::new(runner, PROFILE)?;
    let public = work.join("public");
    fs::create_dir_all(public.join("heads")).map_err(|e| e.to_string())?;
    let policy = work.join("policy");
    fs::create_dir_all(policy.join("releases")).map_err(|e| e.to_string())?;
    fs::set_permissions(policy.join("releases"), fs::Permissions::from_mode(0o700))
        .map_err(|e| e.to_string())?;
    delivery.command(&[
        "init",
        policy
            .join("releases/state")
            .to_str()
            .ok_or("policy UTF-8")?,
        &delivery.path("root")?,
        PROFILE,
    ])?;
    fs::write(
        policy.join("releases/endpoint"),
        b"http://127.0.0.1:28180\n",
    )
    .map_err(|e| e.to_string())?;
    let keygen = boot::find_qemu_tool("ssh-keygen").ok_or("ssh-keygen required")?;
    let key = work.join("ssh-key");
    let status = Command::new(keygen)
        .args([
            "-q",
            "-t",
            "ed25519",
            "-N",
            "",
            "-C",
            "td-admin-disposable-oracle",
            "-f",
        ])
        .arg(&key)
        .stdin(Stdio::null())
        .status()
        .map_err(|e| e.to_string())?;
    if !status.success() {
        return Err("fixture SSH key generation failed".into());
    }
    fs::copy(work.join("ssh-key.pub"), policy.join("authorized_keys"))
        .map_err(|e| e.to_string())?;
    // This key is public test furniture on a disposable disk, never a real key.
    fs::copy(key, public.join("ssh-key")).map_err(|e| e.to_string())?;
    let candidate = work.join("candidate");
    let first = boot::create_bootable_candidate(deployment, &candidate)?;
    let second_candidate = work.join("second");
    let second = boot::create_bootable_candidate(&candidate, &second_candidate)?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_secs();
    publish(
        &delivery,
        "root",
        "release.key",
        &candidate,
        1,
        1,
        120,
        "expired",
        &public,
    )?;
    // Publish signs issue time into the index. Promote the SAME immutable index
    // for the unexpired head rather than reusing its sequence with other bytes.
    let old = fs::read_to_string(public.join("heads/expired")).map_err(|e| e.to_string())?;
    let digest = old.lines().nth(6).ok_or("fixture head index")?;
    delivery.command(&[
        "promote",
        &delivery.path("repo")?,
        &delivery.path("root")?,
        &delivery.path("release.key")?,
        PROFILE,
        digest,
        "2",
        &now.to_string(),
        &(now + 10800).to_string(),
    ])?;
    fs::copy(
        Path::new(&delivery.path("repo")?).join("heads/system-x86-64"),
        public.join("heads/valid"),
    )
    .map_err(|e| e.to_string())?;
    delivery.command(&[
        "keygen",
        &delivery.path("next.key")?,
        &delivery.path("next.pub")?,
    ])?;
    delivery.command(&[
        "root",
        "2",
        &(now + 86400).to_string(),
        &delivery.path("root.key")?,
        &delivery.path("next.pub")?,
        &delivery.path("root.key")?,
        &delivery.path("root2")?,
    ])?;
    publish(
        &delivery, "root2", "next.key", &candidate, 1, 1, 10800, "rotated", &public,
    )?;
    publish(
        &delivery,
        "root2",
        "next.key",
        &second_candidate,
        2,
        2,
        10800,
        "bad",
        &public,
    )?;
    copy_tree(Path::new(&delivery.path("repo")?), &public.join("repo"))?;
    runner.prepare_recipe_target("td-install-qemu-test")?;
    let built = runner.build_plan("td-install-qemu-test")?;
    fs::copy(
        runner
            .ladder_out_from(&built, "td-install-qemu-test")?
            .join("bin/td-install-qemu-test"),
        public.join("client"),
    )
    .map_err(|e| e.to_string())?;
    readable(&public)?;
    fs::set_permissions(public.join("client"), fs::Permissions::from_mode(0o755))
        .map_err(|e| e.to_string())?;
    Ok((first, second))
}

fn shutdown(mut guest: Guest) -> Result<()> {
    guest.until(Duration::from_secs(600), |line| {
        Ok(boot::reports_marker(line, td_recipe::shared::SYSTEM_SHUTDOWN_MARKER).then_some(()))
    })?;
    let end = Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(status) = guest.child.try_wait().map_err(|e| e.to_string())? {
            return if status.success() {
                Ok(())
            } else {
                Err(format!("guest reboot: {status}"))
            };
        }
        if Instant::now() >= end {
            return Err("guest did not exit after orderly reboot".into());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

pub(crate) fn run(runner: &crate::check_runner::RecipeCheckRunner) -> Result<()> {
    let accel = match crate::checks::accel::headless_from_env()?.names {
        ["kvm"] => "kvm",
        ["tcg"] => "tcg",
        _ => return Err("one VM accelerator required".into()),
    };
    let (kernel, template, deployment) = boot::build_system(runner)?;
    let (mkfs, btrfs) = boot::build_btrfs_tools(runner)?;
    let owned = boot::release_delivery::fixture_temp::TempDir::new("td-admin-oracle")
        .map_err(|e| e.to_string())?;
    let work = owned.path();
    let (first, second) = prepare(runner, work, &deployment)?;
    let identity = boot::RunVolume::generate_root_owned(PathBuf::from(
        runner.builder_command().get_program(),
    ))?;
    let selector = boot::provision_selector(&template, work, &identity)?;
    let volume = work.join("volume.btrfs");
    boot::create_persistent_volume_layout(
        &deployment,
        &mkfs,
        &btrfs,
        &volume,
        boot::VolumeLayout::Admin(work.to_path_buf()),
        false,
        &identity,
        None,
        boot::VolumePurpose::Fixture,
        None,
        None,
        None,
    )?;
    super::overlay(&volume, "raw", &work.join("disk.qcow2"))?;
    let options = Options {
        kernel,
        selector,
        disk: volume,
        format: "raw".into(),
        work: work.to_path_buf(),
        timeout: Duration::from_secs(5400),
        rollback: false,
        accel,
    };
    let deadline = Instant::now() + options.timeout;
    let disk = work.join("disk.qcow2");
    let result = (|| {
        let mut guest = Guest::healthy(&options, 1, deadline, &disk, |_| Ok(()))?;
        guest.command("mkdir -p /var/home/tester/.td-admin-oracle && chmod 700 /var/home/tester/.td-admin-oracle && cp /run/td-volume/td/admin-fixture/client /var/home/tester/.td-admin-oracle/client && chmod 700 /var/home/tester/.td-admin-oracle/client")?;
        let lines = guest.command(&format!("{CLIENT} exercise"))?;
        for marker in [
            "TD-ADMIN-EXPIRED-REFUSED",
            "TD-ADMIN-INTERRUPTED",
            "TD-ADMIN-RESUMED-INSTALLED",
            "TD-ADMIN-ROTATED",
            "TD-ADMIN-ROOT-ROLLBACK-REFUSED",
            "TD-ADMIN-EXERCISE-OK",
        ] {
            if !lines.iter().any(|line| boot::reports_marker(line, marker)) {
                return Err(format!("missing {marker}: {lines:?}"));
            }
            println!("{marker}");
        }
        // The operator-only public mode's own line, so its client and its stdin, the
        // guest's console rather than the fixture's /dev/null, are proved on every run.
        let status = public::ssh(&mut guest, "status")?;
        if !status.iter().any(|line| line.starts_with("job ")) {
            return Err(format!("public mode's SSH line got no job: {status:?}"));
        }
        println!("TD-ADMIN-PUBLIC-LINE");
        guest.send(&format!("{CLIENT} reboot"))?;
        shutdown(guest)?;
        let mut guest = Guest::healthy(&options, 2, deadline, &disk, |_| Ok(()))?;
        let cmdline = guest.scalar("cat /proc/cmdline", |line| super::kernel_id(line).is_some())?;
        if super::kernel_id(&cmdline) != Some(first.as_str()) {
            return Err("downloaded candidate did not boot".into());
        }
        guest.send(&format!("{CLIENT} bad"))?;
        guest.until(Duration::from_secs(60), |line| {
            Ok(boot::reports_marker(line, "TD-ADMIN-DEFAULT-REBOOT-SUBMITTED").then_some(()))
        })?;
        shutdown(guest)?;
        for (pass, remaining) in [(3, 2), (4, 1), (5, 0)] {
            Guest::spawn(
                &options,
                pass,
                deadline,
                &disk,
                boot::BOOT_FAIL_TARGET_CMDLINE_TOKEN,
            )?
            .failed_boot(&second, remaining)?;
        }
        let mut evidence = super::RollbackEvidence::default();
        let guest = Guest::healthy(&options, 6, deadline, &disk, |line| {
            evidence.observe(line, &second, &first, true)
        })?;
        evidence.finish(true)?;
        guest.stop()?;
        println!("PASS: qemu-admin-update: guest SSH update, expiry, interrupted HTTP/cache resume, root rotation/rollback refusal, default reboot, health and automatic rollback");
        Ok(())
    })();
    let logs = runner.scratch_dir().join("admin-update");
    fs::create_dir_all(&logs).map_err(|e| e.to_string())?;
    for pass in 1..=6 {
        for prefix in ["serial", "qemu"] {
            let name = format!("{prefix}-{pass}.log");
            if work.join(&name).exists() {
                fs::copy(work.join(&name), logs.join(name)).map_err(|e| e.to_string())?;
            }
        }
    }
    result
}
