use super::*;
use td_engine::cpio::{self, Entry, Kind};

#[path = "../../../../fixtures/secret_vm.rs"]
#[allow(dead_code)]
mod fixture;

pub(crate) const TARGETS: &[&str] = &["linux-x86-64", "td-secret-vm-test", "td-secret", "td-init", "td-firstboot", "td-login", "td-compositor", "td-busd", "td-portal", "td-jail", "btrfs-progs-x86-64"];

fn output(runner: &RecipeCheckRunner, name: &str) -> Result<PathBuf, String> {
    runner.prepare_recipe_target(name)?;
    let log = runner.build_plan(name)?;
    runner.ladder_out_from(&log, name)
}

pub(crate) fn run(runner: &RecipeCheckRunner, tpm: Option<&Path>) -> Result<(), String> {
    if let Some(executable) = tpm {
        verify_swtpm(executable)?;
    }
    let qemu = find_qemu()?;
    let (kernel, base) = build_kernel(runner)?;
    let tests = output(runner, "td-secret-vm-test")?;
    let secret = output(runner, "td-secret")?;
    let init = output(runner, "td-init")?;
    let mut files = vec![
        ("init", tests.join("bin/secret-vm-init")),
        ("bin/td-authd-tests", tests.join("bin/td-authd-tests")),
        ("bin/td-secret-tests", tests.join("bin/td-secret-tests")),
        ("bin/td-secret", secret.join("bin/td-secret")),
        ("bin/td-init", init.join("bin/td-init")),
    ];
    if tpm.is_some() {
        files.push(("bin/td-authd", tests.join("bin/td-authd")));
        for (name, destination) in [
            ("td-firstboot", "bin/td-firstboot"),
            ("td-login", "bin/td-login"),
            ("td-compositor", "bin/td-compositor"),
            ("td-busd", "bin/td-busd"),
            ("td-portal", "bin/td-portal"),
            ("td-jail", "bin/td-jail"),
        ] {
            let built = output(runner, name)?;
            files.push((destination, built.join("bin").join(name)));
        }
        let btrfs = output(runner, "btrfs-progs-x86-64")?;
        files.push(("bin/btrfs", btrfs.join("bin/btrfs")));
        files.push(("bin/mkfs.btrfs", btrfs.join("bin/mkfs.btrfs")));
    }
    let mut contents = Vec::new();
    for (name, path) in &files {
        contents.push((
            *name,
            fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?,
        ));
    }
    let base = fs::read(&base).map_err(|e| format!("read base initramfs: {e}"))?;
    if base.len() % 4 != 0 {
        return Err("base initramfs is not four-byte aligned".into());
    }
    static TPM_SEQ: AtomicU64 = AtomicU64::new(0);
    let tpm_scratch = tpm
        .map(|_| create_qmp_scratch_dir(&env::temp_dir(), &TPM_SEQ).map(|dir| Scratch { dir }))
        .transpose()?;
    let tpm_disk = tpm_scratch
        .as_ref()
        .map(|scratch| {
            let path = scratch.dir.join("sealed.img");
            let file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .map_err(|e| format!("create TPM fixture disk: {e}"))?;
            file.set_len(1_048_576)
                .map_err(|e| format!("size TPM fixture disk: {e}"))?;
            Ok::<_, String>(path)
        })
        .transpose()?;
    let store_disk = tpm_scratch.as_ref().map(|scratch| {
        let path = scratch.dir.join("store.img");
        let file = OpenOptions::new().write(true).create_new(true).open(&path)
            .map_err(|e| format!("create persistent store fixture disk: {e}"))?;
        file.set_len(256 * 1024 * 1024)
            .map_err(|e| format!("size persistent store fixture disk: {e}"))?;
        Ok::<_, String>(path)
    }).transpose()?;
    let recovery_disk = tpm_scratch.as_ref().map(|scratch| {
        let path = scratch.dir.join("recovery-store.img");
        let file = OpenOptions::new().write(true).create_new(true).open(&path)
            .map_err(|e| format!("create recovery store fixture disk: {e}"))?;
        file.set_len(256 * 1024 * 1024)
            .map_err(|e| format!("size recovery store fixture disk: {e}"))?;
        Ok::<_, String>(path)
    }).transpose()?;
    let cases = fixture::CASES.iter().map(|case| (case, false)).chain(
        fixture::TPM_CASES
            .iter()
            .chain(fixture::FIDO_CASES)
            .filter(|_| tpm.is_some())
            .map(|case| (case, true)),
    );
    for ((name, test), is_tpm) in cases {
        let emulator = match (is_tpm, tpm, tpm_scratch.as_ref()) {
            (true, Some(executable), Some(scratch)) => {
                Some(Emulator::start(executable, &scratch.dir, name)?)
            }
            (false, _, _) => None,
            _ => return Err("missing TPM fixture configuration".into()),
        };
        let mut entries: Vec<Entry<'_>> = contents
            .iter()
            .map(|(name, bytes)| Entry {
                name,
                mode: 0o755,
                kind: Kind::File(bytes),
            })
            .collect();
        entries.push(Entry {
            name: "case",
            mode: 0o444,
            kind: Kind::File(name.as_bytes()),
        });
        let archive = runner.scratch_dir().join(format!("secret-{name}.cpio"));
        let mut file =
            File::create(&archive).map_err(|e| format!("create fixture archive: {e}"))?;
        file.write_all(&base)
            .and_then(|_| file.write_all(&cpio::build(&entries).map_err(std::io::Error::other)?))
            .map_err(|e| format!("write fixture archive: {e}"))?;
        drop(file);
        println!("[qemu-secret] {name}: {test}");
        let result = boot_with_timeout(
            &qemu,
            &kernel,
            &archive,
            BootPlan {
                disk: if name.starts_with("fido-cold-recovery-") {
                    recovery_disk.as_deref().map(|path| BootDisk { path, read_only: false })
                } else if name.starts_with("fido-cold-") {
                    store_disk.as_deref().map(|path| BootDisk { path, read_only: false })
                } else {
                    tpm_disk.as_deref().filter(|_| name.starts_with("tpm-"))
                        .map(|path| BootDisk { path, read_only: *name != "tpm-seal" })
                },
                mem: "512",
                target_marker: fixture::PASS,
                kill_on_marker: false,
                extra_append: if name.starts_with("fido-") {
                    "td.hid-fixture=1"
                } else if is_tpm {
                    "td.tpm-fixture=1"
                } else {
                    "td.operation-fixture=1 td.write-intake-fixture=1"
                },
                user_net: false,
                audio: false,
                physical_input: false,
                capture_firefox_audio: false,
                tpm_socket: emulator.as_ref().map(|emulator| emulator.socket.as_path()),
            },
            runner.scratch_dir(),
            Duration::from_secs(180),
        )
        .map_err(|error| {
            emulator
                .as_ref()
                .map_or_else(|| error.clone(), |tpm| tpm.diagnostic(&error))
        })?;
        if !result.evidence.target
            || !result.exited_clean
            || result.evidence.kernel_panic
            || result
                .console
                .lines()
                .any(|line| line.starts_with(fixture::FAIL))
        {
            let error = format!(
                "secret VM {name} failed: {}\n{}",
                result.reason,
                tail(&result.console, 80)
            );
            return Err(emulator
                .as_ref()
                .map_or_else(|| error.clone(), |tpm| tpm.diagnostic(&error)));
        }
        if let Some(emulator) = emulator {
            emulator.finish()?;
        }
        println!(
            "[qemu-secret] {name} passed in {:.2}s",
            result.elapsed.as_secs_f64()
        );
    }
    println!("PASS: secret authority VM cases ({} fresh guests); credential intake, inspection and relocking; no token or TPM release claim", fixture::CASES.len());
    if tpm.is_some() {
        println!("PASS: TPM guest device, persistent sealed key, cold reopen, changed PCR and different TPM refusal; fixture measurements only, no FIDO2 or measured-deployment claim");
        println!("PASS: virtual credential creation, proof, recovery exclusion and both recovery policies; fresh assertion before TPM unseal, replay and wrong-key refusal; no physical presence or session-release claim");
        println!("PASS: guest HID discovery, production worker, signed fixture assertion and challenge refusal through the TPM, keepalive deadline and worker cleanup; no physical USB or token presence claim");
        println!("PASS: production private enrollment, unlock and named-write workers; both recovery policies, commit cancellation, locked writes and credential readback; simulated parent acknowledgements, no desktop or physical-presence claim");
        println!("PASS: production compositor attention, root authority, public sealed-descriptor credential write and generation relocking through virtual keyboard/token devices; jailed application portal retrieval, application isolation and locked refusal; no physical-presence claim");
        println!("PASS: cold Btrfs @var credential-store reopen with retained TPM state under both recovery policies; unchanged bundle, locked jailed retrieval, fresh primary or recovery assertion and per-application readback with the primary token absent during recovery; no power-loss or physical-presence claim");
    }
    Ok(())
}

pub(crate) fn options(args: &[String]) -> Result<Option<PathBuf>, String> {
    match args {
        [] => Ok(None),
        [flag, path] if flag == "--tpm" && Path::new(path).is_absolute() => {
            Ok(Some(PathBuf::from(path)))
        }
        _ => Err("usage: td-recipe-eval qemu-secret [--tpm /absolute/path/to/swtpm]".into()),
    }
}

fn verify_swtpm(executable: &Path) -> Result<(), String> {
    let version = Command::new(executable)
        .arg("--version")
        .output()
        .map_err(|e| format!("run swtpm version check: {e}"))?;
    if !version.status.success() || !version.stdout.starts_with(b"TPM emulator version 0.10.1,") {
        return Err("TPM oracle requires the documented pinned swtpm 0.10.1".into());
    }
    Ok(())
}

struct Emulator {
    child: std::process::Child,
    socket: PathBuf,
    log: PathBuf,
}

impl Emulator {
    fn start(executable: &Path, root: &Path, case: &str) -> Result<Self, String> {
        let state = if case == "tpm-other" {
            "other"
        } else {
            "primary"
        };
        fs::create_dir_all(root.join(state)).map_err(|e| format!("create TPM state: {e}"))?;
        // Short private paths use QMP's existing Unix-socket length/ownership policy.
        let socket = root.join("tpm.sock");
        match fs::remove_file(&socket) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("retire TPM control socket: {e}")),
        }
        let log_path = root.join(format!("{case}.log"));
        let log = File::create(&log_path).map_err(|e| format!("create TPM log: {e}"))?;
        let errors = log.try_clone().map_err(|e| format!("clone TPM log: {e}"))?;
        let pid_name = format!("{case}.pid");
        let pid_path = root.join(&pid_name);
        let child = Command::new(executable)
            .args(["socket", "--tpm2", "--tpmstate"])
            .arg(format!("dir={state},mode=0600"))
            .args(["--ctrl", "type=unixio,path=tpm.sock,mode=0600,terminate"])
            .arg("--pid")
            .arg(format!("file={pid_name}"))
            .current_dir(root)
            .stdin(Stdio::null())
            .stdout(log)
            .stderr(errors)
            .spawn()
            .map_err(|e| format!("start swtpm: {e}"))?;
        let mut emulator = Self {
            child,
            socket,
            log: log_path,
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = emulator
                .child
                .try_wait()
                .map_err(|e| format!("poll swtpm: {e}"))?
            {
                return Err(
                    emulator.diagnostic(&format!("swtpm exited before socket readiness: {status}"))
                );
            }
            // swtpm 0.10.1 writes its PID after listen; bind alone is too early.
            let mut pid = String::new();
            let listening = File::open(&pid_path)
                .and_then(|file| file.take(32).read_to_string(&mut pid))
                .is_ok()
                && pid.trim().parse::<u32>().ok() == Some(emulator.child.id());
            if listening
                && fs::symlink_metadata(&emulator.socket).is_ok_and(|meta| {
                    use std::os::unix::fs::FileTypeExt;
                    meta.file_type().is_socket()
                })
            {
                return Ok(emulator);
            }
            if Instant::now() >= deadline {
                return Err(emulator.diagnostic("swtpm socket readiness timed out"));
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn diagnostic(&self, message: &str) -> String {
        let mut bytes = Vec::new();
        match File::open(&self.log).and_then(|file| file.take(65_536).read_to_end(&mut bytes)) {
            Ok(_) => format!(
                "{message}\nswtpm log (first 64 KiB):\n{}",
                String::from_utf8_lossy(&bytes)
            ),
            Err(error) => format!("{message}; could not read swtpm log: {error}"),
        }
    }

    fn finish(mut self) -> Result<(), String> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = self
                .child
                .try_wait()
                .map_err(|e| format!("wait swtpm: {e}"))?
            {
                return if status.success() {
                    Ok(())
                } else {
                    Err(self.diagnostic(&format!("swtpm failed: {status}")))
                };
            }
            if Instant::now() >= deadline {
                return Err(self.diagnostic("swtpm did not exit after guest shutdown"));
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Emulator {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tpm_options_require_an_explicit_absolute_emulator() {
        assert_eq!(options(&[]).unwrap(), None);
        assert_eq!(
            options(&["--tpm".into(), "/tmp/swtpm".into()]).unwrap(),
            Some(PathBuf::from("/tmp/swtpm"))
        );
        for args in [
            vec!["--tpm"],
            vec!["--tpm", "swtpm"],
            vec!["--tpm", "/tmp/swtpm", "extra"],
            vec!["--other", "/tmp/swtpm"],
        ] {
            assert!(options(&args.into_iter().map(str::to_string).collect::<Vec<_>>()).is_err());
        }
        assert_eq!(
            tpm_chardev_arg(Path::new("/tmp/a,b.sock")),
            OsString::from("socket,id=secret-tpm,path=/tmp/a,,b.sock")
        );
        let source = include_str!("../../../../../../td-secret/src/tpm.rs");
        let names: std::collections::BTreeSet<_> =
            fixture::TPM_CASES.iter().map(|(name, _)| name).collect();
        assert_eq!(names.len(), 4);
        for (_, test) in fixture::TPM_CASES {
            assert!(source.contains(&format!("fn {}()", test.rsplit("::").next().unwrap())));
        }
        let source = include_str!("../../../../../../td-secret/src/fido_device.rs");
        for (_, test) in fixture::FIDO_CASES {
            assert!(source.contains(&format!("fn {}()", test.rsplit("::").next().unwrap())));
        }
    }

    #[test]
    fn secret_vm_requires_a_real_single_test_pass() {
        let pass =
            "test result: ok. 1 passed; 0 failed; 0 ignored; 42 filtered out; finished in 0.01s";
        assert!(fixture::test_passed(true, pass));
        assert!(!fixture::test_passed(false, pass));
        for bad in [
            "",
            "test result: ok. 0 passed; 0 failed; 0 ignored;",
            "test result: ok. 0 passed; 0 failed; 1 ignored;",
            "test result: FAILED. 0 passed; 1 failed; 0 ignored;",
        ] {
            assert!(!fixture::test_passed(true, bad));
        }
        assert!(!fixture::test_passed(
            true,
            &format!("{pass}\ntest result: ok. 0 passed; 0 failed; 0 ignored;")
        ));
    }

    #[test]
    fn secret_vm_roster_names_only_explicit_root_oracles() {
        let source = [
            include_str!("../../../../../../td-authd/tests/secret_intake.rs"),
            include_str!("../../../../../../td-authd/tests/session.rs"),
            include_str!("../../../../../../td-authd/tests/unlock.rs"),
        ]
        .join("\n");
        let names: std::collections::BTreeSet<_> =
            fixture::CASES.iter().map(|(name, _)| name).collect();
        assert_eq!(names.len(), 4);
        for (_, test) in fixture::CASES {
            let name = test.rsplit("::").next().unwrap();
            assert!(name.starts_with("root_"));
            assert!(source.contains(&format!("fn {name}()")));
        }
        let recipe = td_recipe::catalog::lookup("td-secret-vm-test").unwrap();
        assert!(recipe.checks.is_none());
        let readers = td_recipe::catalog::named_dirs("td-secret-vm-test");
        for source in ["td-authd", "td-firstboot", "td-secret", "td-busd"] {
            assert!(readers.contains(&source));
        }
        let system = crate::check_runner::recipe_closure(&["system-x86-64"]).unwrap();
        // Test executables are deliberately absent from the system closure.
        assert!(!system.iter().any(|node| node.stem == "td-secret-vm-test"));
    }
}
