use super::*;
use td_engine::cpio::{self, Entry, Kind};

#[path = "../../../../fixtures/secret_vm.rs"]
#[allow(dead_code)]
mod fixture;

pub(crate) const TARGETS: &[&str] = &["linux-x86-64", "td-secret-vm-test", "td-secret", "td-init", "td-firstboot", "td-login", "td-compositor", "td-busd", "td-portal", "td-jail", "btrfs-progs-x86-64"];

pub(crate) const SYSTEM_TARGETS: &[&str] = &["system-secret-vm-test", "btrfs-progs-x86-64"];

pub(crate) fn system_options(args: &[String]) -> Result<(PathBuf, bool), String> {
    let (args, powercuts) = match args.split_last() {
        Some((flag, rest)) if flag == "--powercuts" => (rest, true),
        _ => (args, false),
    };
    options(args).ok().flatten().map(|path| (path, powercuts))
        .ok_or_else(|| "usage: td-recipe-eval qemu-secret-system --tpm /absolute/path/to/swtpm [--powercuts]".into())
}

fn system_result(result: &BootResult, phase: &str, cut: bool) -> Result<(), String> {
    let cut_marker = format!("{} {phase}", fixture::SYSTEM_CUT);
    let ended = if cut {
        result.marker_killed && !result.exited_clean
            && result.console.lines().filter(|line| *line == cut_marker).count() == 1
            && !result.console.lines().any(|line| line == SYSTEM_SHUTDOWN_MARKER
                || line == format!("secret-fixture: {}", fixture::SYSTEM_PASS)
                || line.starts_with("secret-fixture: test result:"))
    } else { result.exited_clean && !result.marker_killed };
    if !result.evidence.target || !ended || result.evidence.kernel_panic
        || result.console.lines().any(|line| line.starts_with(&format!("secret-fixture: {}", fixture::FAIL))) {
        return Err(format!("system secret {phase} failed: {}\n{}", result.reason, tail(&result.console, 160)));
    }
    Ok(())
}

pub(crate) fn run_system(runner: &RecipeCheckRunner, tpm: &Path, powercuts: bool) -> Result<(), String> {
    verify_swtpm(tpm)?;
    let qemu = find_qemu()?;
    let system = output(runner, "system-secret-vm-test")?;
    let deployment = system.join("deployment");
    let (kernel, _, _) = verify_deployment(&deployment)?;
    let selector = verify_selector(&system.join("boot"))?;
    let trust = RunTrust::generate()?;
    static SEQ: AtomicU64 = AtomicU64::new(0);
    // Keep large disposable disks with the private TPM scratch. TMPDIR can
    // place this runtime-only fixture on a roomier filesystem than the cache.
    let scratch = Scratch { dir: create_qmp_scratch_dir(&env::temp_dir(), &SEQ)? };
    let initramfs = provision_selector(&selector, &scratch.dir, &trust)?;
    // This provisioned selector is private host fixture input, like its trust
    // key. The shipping selector remains without a measurement policy.
    let policy = cpio::build(&[Entry {
        name: "etc/td/boot-measurement", mode: 0o644,
        kind: Kind::File(b"td-selector-pcr11-v1\n"),
    }])?;
    let length = fs::metadata(&initramfs).map_err(|e| format!("stat selector: {e}"))?.len();
    let length = usize::try_from(length).map_err(|_| "selector is too large")?;
    let mut appendix = vec![0; cpio::alignment_padding(length)];
    appendix.extend_from_slice(&policy);
    OpenOptions::new().append(true).open(&initramfs)
        .and_then(|mut file| file.write_all(&appendix))
        .map_err(|e| format!("append selector measurement policy: {e}"))?;

    let (mkfs, btrfs) = build_btrfs_tools(runner)?;
    let volume = scratch.dir.join("secret-system.btrfs");
    create_persistent_volume(&deployment, &mkfs, &btrfs, &volume, &trust, VolumePurpose::Fixture)?;
    let id = crate::sha256::sha256_file(&deployment.join("manifest"))
        .map_err(|e| format!("hash system fixture manifest: {e}"))?;
    let phases: &[&str] = if powercuts { &["create", "cut-queued", "cut-written", "recover-written"] }
        else { &["create", "recover"] };
    for phase in phases {
        let cut = phase.starts_with("cut-");
        let emulator = Emulator::start(tpm, &scratch.dir, phase)?;
        let tokens = format!("td.hid-fixture=1 td.secret-system={phase}");
        let marker = if cut { format!("{} {phase}", fixture::SYSTEM_CUT) }
            else { format!("secret-fixture: {}", fixture::SYSTEM_PASS) };
        println!("[qemu-secret-system] {phase}: stock firstboot and supervised desktop");
        let result = boot_with_timeout(&qemu, &kernel, &initramfs, BootPlan {
            disk: Some(BootDisk { path: &volume, read_only: false }),
            mem: SYSTEM_GUEST_MEMORY_MIB,
            target_marker: &marker,
            kill_on_marker: cut,
            extra_append: &tokens,
            user_net: false,
            audio: true,
            physical_input: false,
            capture_firefox_audio: false,
            tpm_socket: Some(&emulator.socket),
        }, runner.scratch_dir(), Duration::from_secs(600))
            .map_err(|e| emulator.diagnostic(&e))?;
        fs::write(runner.scratch_dir().join(format!("secret-system-{phase}.log")), &result.console)
            .map_err(|e| format!("save system fixture console: {e}"))?;
        system_result(&result, phase, cut).map_err(|error| emulator.diagnostic(&error))?;
        require_selected_deployment(&result, td_boot_protocol::SELECTED_CURRENT_MARKER, &id, phase)?;
        if result.console.lines().filter(|line| line.starts_with("td-boot: TD-BOOT-MEASURED-PCR11 ")).count() != 1
            || result.console.lines().filter(|line| *line == "secret-fixture: system selector PCR verified after kexec").count() != 1 {
            return Err(format!("system secret {phase} lacks selector measurement/readback evidence"));
        }

        if !cut { validate_persistent_shutdown(&result, phase)?; }
        emulator.finish()?;
        if !cut { check_persistent_volume(&btrfs, &volume)?; }
        println!("[qemu-secret-system] {phase} passed in {:.2}s", result.elapsed.as_secs_f64());
    }
    println!("PASS: full deployment firstboot, secure-attention enrollment and named write, jailed mail receipt, generation relocking, and cold recovery with only the second token; verified selector PCR 11 across kexec; synthetic enrollment PCR 7 and UHID fixtures, no authenticated-firmware or physical-presence claim");
    if powercuts {
        println!("PASS: abrupt QEMU cuts with an unconsented submitted write and an acknowledged write; cold locked startup, no ready request, old/new credential preservation and fresh recovery consent; host storage and TPM emulator retained, no host-power-loss or torn-sector claim");
    }
    Ok(())
}

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
    fn system_options_and_guest_phases_reject_ambiguous_cuts() {
        let args = |values: &[&str]| values.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(system_options(&args(&["--tpm", "/tmp/swtpm"])).unwrap(), (PathBuf::from("/tmp/swtpm"), false));
        assert_eq!(system_options(&args(&["--tpm", "/tmp/swtpm", "--powercuts"])).unwrap(), (PathBuf::from("/tmp/swtpm"), true));
        for values in [vec![], vec!["--powercuts"], vec!["--tpm", "relative", "--powercuts"],
            vec!["--tpm", "/tmp/swtpm", "--powercuts", "--powercuts"]] {
            assert!(system_options(&args(&values)).is_err());
        }
        for phase in fixture::SYSTEM_PHASES {
            assert_eq!(fixture::system_phase(&format!("console=ttyS0 td.secret-system={phase}")).unwrap(), *phase);
        }
        for cmdline in ["", "td.secret-system=unknown", "td.secret-system=create td.secret-system=create",
            "td.secret-system=cut-queued td.secret-system=recover", "td.secret-system=create td.secret-system="] {
            assert!(fixture::system_phase(cmdline).is_err());
        }
    }

    #[test]
    fn system_cut_requires_an_observed_kill_at_one_exact_boundary() {
        let valid = || BootResult {
            evidence: ConsoleEvidence { target: true, ..ConsoleEvidence::default() },
            exited_clean: false,
            marker_killed: true,
            reason: String::new(),
            console: format!("{} cut-queued\n", fixture::SYSTEM_CUT),
            elapsed: Duration::from_secs(1),
            firefox_audio: FirefoxAudioCapture::NotRequested,
        };
        assert!(system_result(&valid(), "cut-queued", true).is_ok());
        for case in 0..10 {
            let mut result = valid();
            match case {
                0 => result.evidence.target = false,
                1 => result.marker_killed = false,
                2 => result.exited_clean = true,
                3 => result.evidence.kernel_panic = true,
                4 => result.console.clear(),
                5 => result.console.push_str(&result.console.clone()),
                6 => result.console.push_str(&format!("{SYSTEM_SHUTDOWN_MARKER}\n")),
                7 => result.console.push_str(&format!("secret-fixture: {}\n", fixture::SYSTEM_PASS)),
                8 => result.console.push_str("secret-fixture: test result: ok. 1 passed; 0 failed;\n"),
                _ => result.console.push_str(&format!("secret-fixture: {}: refused\n", fixture::FAIL)),
            }
            assert!(system_result(&result, "cut-queued", true).is_err(), "case {case}");
        }
        assert!(system_result(&valid(), "cut-written", true).is_err());
        assert!(system_result(&valid(), "cut-queued", false).is_err());
    }

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
