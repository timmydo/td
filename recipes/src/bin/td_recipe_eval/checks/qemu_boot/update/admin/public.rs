//! Operator-run public HTTPS acquisition on an older, disposable deployment.
use super::*;

pub(crate) struct Public {
    base: PathBuf,
    root: PathBuf,
    url: String,
    commit: String,
    deployment: String,
}
impl Public {
    pub(crate) fn parse(args: &[String]) -> Result<Self> {
        let [flag, base, root, url, commit, deployment] = args else {
            return Err("usage: qemu-admin-update --public BASE_DEPLOYMENT ROOT HTTPS_URL COMMIT DEPLOYMENT_ID".into());
        };
        if flag != "--public"
            || !url.starts_with("https://")
            || url.len() > 2048
            || url.chars().any(char::is_whitespace)
            || url.contains(['\r', '\n', '#', '?', '@'])
        {
            return Err(
                "public oracle requires an HTTPS repository without userinfo, query or fragment"
                    .into(),
            );
        }
        for (value, length) in [(commit, 40), (deployment, 64)] {
            if value.len() != length
                || !value
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            {
                return Err("invalid expected release identity".into());
            }
        }
        Ok(Self {
            base: fs::canonicalize(base).map_err(|e| e.to_string())?,
            root: fs::canonicalize(root).map_err(|e| e.to_string())?,
            url: url.clone(),
            commit: commit.clone(),
            deployment: deployment.clone(),
        })
    }
}
const SSH: &str = "ssh -F /dev/null -T -o BatchMode=yes -o IdentitiesOnly=yes -o StrictHostKeyChecking=accept-new -o GlobalKnownHostsFile=/dev/null -o UserKnownHostsFile=/var/home/tester/.td-admin-oracle/known_hosts -o ConnectTimeout=10 -i /var/home/tester/.td-admin-oracle/key td-admin@127.0.0.1";
pub(super) fn ssh(guest: &mut Guest, command: &str) -> Result<Vec<String>> {
    guest.command(&format!("{SSH} '{command}'"))
}
fn job(guest: &mut Guest, verb: &str, previous: &mut u64) -> Result<(String, String)> {
    let submission_end = Instant::now() + Duration::from_secs(60);
    let id = loop {
        let receipt = ssh(guest, verb)?;
        let id = receipt
            .iter()
            .find_map(|line| {
                line.strip_prefix("job ")
                    .and_then(|line| line.split_once(':'))
                    .and_then(|(id, _)| id.parse::<u64>().ok())
            })
            .ok_or("public SSH update omitted job ID")?;
        if id > *previous {
            *previous = id;
            break id;
        }
        if Instant::now() >= submission_end {
            return Err("previous public admin job remained active".into());
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    let end = Instant::now() + Duration::from_secs(1800);
    loop {
        let status = ssh(guest, &format!("status {id}"))?.join("\n");
        if status
            .lines()
            .any(|line| line == format!("job {id}: failed"))
        {
            return Err(format!(
                "public admin job failed: {status}\n{}",
                ssh(guest, &format!("logs {id}"))?.join("\n")
            ));
        }
        if status.lines().any(|line| {
            line == format!(
                "job {id}: {}",
                if verb.starts_with("rollback") {
                    "rolled back"
                } else {
                    "installed"
                }
            )
        }) {
            return Ok((status, ssh(guest, &format!("logs {id}"))?.join("\n")));
        }
        if Instant::now() >= end {
            return Err(format!("public admin job timed out: {status}"));
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}
fn seeded_bytes(logs: &str) -> Result<u64> {
    let mut counters = logs.lines().filter_map(|line| {
        line.strip_prefix("release seed: ")
            .and_then(|line| line.strip_suffix(" verified object bytes from local deployment"))
    });
    let value = counters
        .next()
        .ok_or("public update omitted installed-deployment seed evidence")?
        .parse::<u64>()
        .map_err(|_| "invalid installed-deployment seed counter")?;
    if value == 0 || counters.next().is_some() {
        return Err("public update did not prove nonzero installed-deployment seeding".into());
    }
    Ok(value)
}
fn booted(guest: &mut Guest, expected: &str) -> Result<()> {
    let line = guest.scalar("cat /proc/cmdline", |line| {
        super::super::kernel_id(line).is_some()
    })?;
    if super::super::kernel_id(&line) != Some(expected) {
        return Err("public update booted unexpected deployment".into());
    }
    Ok(())
}
fn reboot(mut guest: Guest) -> Result<()> {
    guest.send(&format!("{SSH} reboot"))?;
    shutdown(guest)
}
pub(crate) fn run(runner: &crate::check_runner::RecipeCheckRunner, options: &Public) -> Result<()> {
    boot::verify_deployment(&options.base)?;
    let old =
        crate::sha256::sha256_file(&options.base.join("manifest")).map_err(|e| e.to_string())?;
    if old == options.deployment {
        return Err("public update oracle needs an older, different deployment".into());
    }
    let accel = match crate::checks::accel::headless_from_env()?.names {
        ["kvm"] => "kvm",
        ["tcg"] => "tcg",
        _ => return Err("one accelerator required".into()),
    };
    let (kernel, template, _) = boot::build_system(runner)?;
    let (mkfs, btrfs) = boot::build_btrfs_tools(runner)?;
    let owned = boot::release_delivery::fixture_temp::TempDir::new("td-public-admin-oracle")
        .map_err(|e| e.to_string())?;
    let work = owned.path();
    let public = work.join("public");
    fs::create_dir(&public).map_err(|e| e.to_string())?;
    let policy = work.join("policy");
    fs::create_dir_all(policy.join("releases")).map_err(|e| e.to_string())?;
    fs::set_permissions(policy.join("releases"), fs::Permissions::from_mode(0o700))
        .map_err(|e| e.to_string())?;
    // Only the supplied public root enters this policy. Fixture signing keys are unrelated.
    let delivery = Delivery::new(runner, PROFILE)?;
    delivery.command(&[
        "init",
        policy
            .join("releases/state")
            .to_str()
            .ok_or("policy UTF-8")?,
        options.root.to_str().ok_or("root UTF-8")?,
        PROFILE,
    ])?;
    fs::write(
        policy.join("releases/endpoint"),
        format!("{}\n", options.url),
    )
    .map_err(|e| e.to_string())?;
    let keygen = boot::find_qemu_tool("ssh-keygen").ok_or("ssh-keygen required")?;
    let status = Command::new(keygen)
        .args([
            "-q",
            "-t",
            "ed25519",
            "-N",
            "",
            "-C",
            "td-public-disposable-oracle",
            "-f",
        ])
        .arg(work.join("ssh-key"))
        .stdin(Stdio::null())
        .status()
        .map_err(|e| e.to_string())?;
    if !status.success() {
        return Err("fixture SSH key generation failed".into());
    }
    fs::copy(work.join("ssh-key.pub"), policy.join("authorized_keys"))
        .map_err(|e| e.to_string())?;
    fs::copy(work.join("ssh-key"), public.join("ssh-key")).map_err(|e| e.to_string())?;
    readable(&public)?;
    let identity = boot::RunVolume::generate_root_owned(PathBuf::from(
        runner.builder_command().get_program(),
    ))?;
    let selector = boot::provision_selector(&template, work, &identity)?;
    let volume = work.join("volume.btrfs");
    boot::create_persistent_volume_layout(
        &options.base,
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
    super::super::overlay(&volume, "raw", &work.join("disk.qcow2"))?;
    let vm = Options {
        kernel,
        selector,
        disk: volume,
        format: "raw".into(),
        work: work.to_path_buf(),
        timeout: Duration::from_secs(7200),
        rollback: false,
        accel,
    };
    let deadline = Instant::now() + vm.timeout;
    let disk = work.join("disk.qcow2");
    let result = (|| {
        let mut guest = Guest::healthy(&vm, 1, deadline, &disk, |_| Ok(()))?;
        booted(&mut guest, &old)?;
        guest.command("mkdir -p /var/home/tester/.td-admin-oracle && chmod 700 /var/home/tester/.td-admin-oracle && cp /run/td-volume/td/admin-fixture/ssh-key /var/home/tester/.td-admin-oracle/key && chmod 600 /var/home/tester/.td-admin-oracle/key")?;
        let mut previous = 0;
        let (status, logs) = job(&mut guest, "update --no-reboot", &mut previous)?;
        if !status.contains(&options.commit)
            || !status.contains(&options.deployment)
            || !logs.contains("reused output bytes")
            || logs.contains(", 0 reused output bytes")
        {
            return Err(format!(
                "public update identity/reuse not established: {status}\n{logs}"
            ));
        }
        let seeded = seeded_bytes(&logs)?;
        println!("TD-PUBLIC-UPDATE-INSTALLED seeded={seeded}\n{status}\n{logs}");
        let (_, cached) = job(&mut guest, "update --no-reboot", &mut previous)?;
        if !cached.contains(", 0 new object bytes,") {
            return Err(format!(
                "repeat public update did not reuse cached payload: {cached}"
            ));
        }
        println!("TD-PUBLIC-UPDATE-CACHE-REUSED\n{cached}");
        reboot(guest)?;
        let mut guest = Guest::healthy(&vm, 2, deadline, &disk, |_| Ok(()))?;
        booted(&mut guest, &options.deployment)?;
        let build = guest.scalar("cat /etc/os-release", |line| line.starts_with("BUILD_ID="))?;
        if build != format!("BUILD_ID=\"{}\"", options.commit) {
            return Err(format!("unexpected BUILD_ID: {build}"));
        }
        println!("TD-PUBLIC-UPDATE-BOOTED {build}");
        job(&mut guest, "rollback --no-reboot", &mut previous)?;
        reboot(guest)?;
        let mut guest = Guest::healthy(&vm, 3, deadline, &disk, |_| Ok(()))?;
        booted(&mut guest, &old)?;
        guest.stop()?;
        println!("PASS: public HTTPS admin update from {old} to {}, cached retry, healthy reboot and rollback",options.deployment);
        Ok(())
    })();
    let logs = runner.scratch_dir().join("public-admin-update");
    fs::create_dir_all(&logs).map_err(|e| e.to_string())?;
    for pass in 1..=3 {
        for prefix in ["serial", "qemu"] {
            let name = format!("{prefix}-{pass}.log");
            if work.join(&name).exists() {
                fs::copy(work.join(&name), logs.join(name)).map_err(|e| e.to_string())?;
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn public_arguments_bind_expected_identity_and_refuse_non_https() {
        let temp = boot::release_delivery::fixture_temp::TempDir::new("public-admin-args").unwrap();
        let root = temp.path().join("root");
        fs::write(&root, "public metadata").unwrap();
        let mut args = vec![
            "--public".to_string(),
            temp.path().display().to_string(),
            root.display().to_string(),
            "https://releases.example.org".to_string(),
            "a".repeat(40),
            "b".repeat(64),
        ];
        assert!(Public::parse(&args).is_ok());
        for url in [
            "http://releases.example.org",
            "https://user@host",
            "https://host/?query",
            "https://host/#fragment",
            "https://host/\nsecond",
        ] {
            args[3] = url.into();
            assert!(Public::parse(&args).is_err());
        }
        args[3] = "https://releases.example.org".into();
        args[4] = "A".repeat(40);
        assert!(Public::parse(&args).is_err());
        args[4] = "a".repeat(40);
        args[5] = "b".repeat(63);
        assert!(Public::parse(&args).is_err());
        assert!(Public::parse(&[]).is_err());
    }
    #[test]
    fn duplicate_chunk_cache_hits_do_not_prove_installed_deployment_seeding() {
        let aggregate="release transfer: 3 requests, 20 response body bytes, 10 new object bytes, 10 reused output bytes";
        assert!(seeded_bytes(aggregate).is_err());
        assert!(seeded_bytes(&format!(
            "release seed: 0 verified object bytes from local deployment\n{aggregate}"
        ))
        .is_err());
        let seed = "release seed: 123 verified object bytes from local deployment";
        assert_eq!(seeded_bytes(&format!("{seed}\n{aggregate}")).unwrap(), 123);
        assert!(seeded_bytes(&format!("{seed}\n{seed}")).is_err());
    }
}
