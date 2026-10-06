use super::guest_screens::{GuestScreens, Screen};
use super::*;
use td_engine::cpio::{self, Entry, Kind};

#[path = "../../../../fixtures/secret_vm.rs"]
#[allow(dead_code)]
mod fixture;

pub(crate) const TARGETS: &[&str] = &[
    "linux-x86-64",
    "td-secret-vm-test",
    "td-secret",
    "td-init",
    "td-firstboot",
    "td-login",
    "td-compositor",
    "td-busd",
    "td-portal",
    "td-jail",
    "btrfs-progs-x86-64",
];

pub(crate) const SYSTEM_TARGETS: &[&str] = &["system-secret-vm-test", "btrfs-progs-x86-64"];

pub(crate) fn system_options(args: &[String]) -> Result<(PathBuf, bool), String> {
    let (args, powercuts) = match args.split_last() {
        Some((flag, rest)) if flag == "--powercuts" => (rest, true),
        _ => (args, false),
    };
    options(args)
        .ok()
        .flatten()
        .map(|path| (path, powercuts))
        .ok_or_else(|| {
            "usage: td-recipe-eval qemu-secret-system --tpm /absolute/path/to/swtpm [--powercuts]"
                .into()
        })
}

fn system_result(result: &BootResult, phase: &str, cut: bool) -> Result<(), String> {
    let cut_marker = format!("{} {phase}", fixture::SYSTEM_CUT);
    let ended = if cut {
        result.marker_killed
            && !result.exited_clean
            && result
                .console
                .lines()
                .filter(|line| *line == cut_marker)
                .count()
                == 1
            && !result.console.lines().any(|line| {
                line == SYSTEM_SHUTDOWN_MARKER
                    || line == format!("secret-fixture: {}", fixture::SYSTEM_PASS)
                    || line.starts_with("secret-fixture: test result:")
            })
    } else {
        result.exited_clean && !result.marker_killed
    };
    if !result.evidence.target
        || !ended
        || result.evidence.kernel_panic
        || result
            .console
            .lines()
            .any(|line| line.starts_with(&format!("secret-fixture: {}", fixture::FAIL)))
    {
        return Err(format!(
            "system secret {phase} failed: {}\n{}",
            result.reason,
            tail(&result.console, 160)
        ));
    }
    Ok(())
}

pub(crate) fn run_system(
    runner: &RecipeCheckRunner,
    tpm: &Path,
    powercuts: bool,
) -> Result<(), String> {
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
    let scratch = Scratch {
        dir: create_qmp_scratch_dir(&env::temp_dir(), &SEQ)?,
    };
    let initramfs = provision_selector(&selector, &scratch.dir, &trust)?;
    // This provisioned selector is private host fixture input, like its trust
    // key. The shipping selector remains without a measurement policy.
    let policy = cpio::build(&[Entry {
        name: "etc/td/boot-measurement",
        mode: 0o644,
        kind: Kind::File(b"td-selector-pcr11-v1\n"),
    }])?;
    let length = fs::metadata(&initramfs)
        .map_err(|e| format!("stat selector: {e}"))?
        .len();
    let length = usize::try_from(length).map_err(|_| "selector is too large")?;
    let mut appendix = vec![0; cpio::alignment_padding(length)];
    appendix.extend_from_slice(&policy);
    OpenOptions::new()
        .append(true)
        .open(&initramfs)
        .and_then(|mut file| file.write_all(&appendix))
        .map_err(|e| format!("append selector measurement policy: {e}"))?;

    let (mkfs, btrfs) = build_btrfs_tools(runner)?;
    let volume = scratch.dir.join("secret-system.btrfs");
    create_persistent_volume(
        &deployment,
        &mkfs,
        &btrfs,
        &volume,
        &trust,
        VolumePurpose::Fixture,
    )?;
    let id = crate::sha256::sha256_file(&deployment.join("manifest"))
        .map_err(|e| format!("hash system fixture manifest: {e}"))?;
    let phases: &[&str] = if powercuts {
        &["create", "cut-queued", "cut-written", "recover-written"]
    } else {
        &["create", "recover"]
    };
    for phase in phases {
        let cut = phase.starts_with("cut-");
        let emulator = Emulator::start(tpm, &scratch.dir, phase)?;
        let tokens = format!("td.hid-fixture=1 td.secret-system={phase}");
        let marker = if cut {
            format!("{} {phase}", fixture::SYSTEM_CUT)
        } else {
            format!("secret-fixture: {}", fixture::SYSTEM_PASS)
        };
        println!("[qemu-secret-system] {phase}: stock firstboot and supervised desktop");
        let result = boot_with_timeout(
            &qemu,
            &kernel,
            &initramfs,
            BootPlan {
                disk: Some(BootDisk::new(&volume, false)),
                mem: SYSTEM_GUEST_MEMORY_MIB,
                target_marker: &marker,
                kill_on_marker: cut,
                extra_append: &tokens,
                user_net: false,
                audio: true,
                physical_input: false,
                capture_firefox_audio: false,
                tpm_socket: Some(&emulator.socket),
                side_channel: None,
                answers: None,
                cut: false,
                keep_console: None,
                screens: None,
                devices: Devices::ALL,
                screen: None,
                shell: None,
            },
            runner.scratch_dir(),
            Duration::from_secs(600),
        )
        .map_err(|e| emulator.diagnostic(&e))?;
        fs::write(
            runner
                .scratch_dir()
                .join(format!("secret-system-{phase}.log")),
            &result.console,
        )
        .map_err(|e| format!("save system fixture console: {e}"))?;
        system_result(&result, phase, cut).map_err(|error| emulator.diagnostic(&error))?;
        require_selected_deployment(
            &result,
            td_boot_protocol::SELECTED_CURRENT_MARKER,
            &id,
            phase,
        )?;
        if result
            .console
            .lines()
            .filter(|line| line.starts_with("td-boot: TD-BOOT-MEASURED-PCR11 "))
            .count()
            != 1
            || result
                .console
                .lines()
                .filter(|line| *line == "secret-fixture: system selector PCR verified after kexec")
                .count()
                != 1
        {
            return Err(format!(
                "system secret {phase} lacks selector measurement/readback evidence"
            ));
        }

        if !cut {
            validate_persistent_shutdown(&result, phase)?;
        }
        emulator.finish()?;
        if !cut {
            check_persistent_volume(&btrfs, &volume)?;
        }
        println!(
            "[qemu-secret-system] {phase} passed in {:.2}s",
            result.elapsed.as_secs_f64()
        );
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

pub(crate) fn run(
    runner: &RecipeCheckRunner,
    tpm: Option<&Path>,
    only: Option<&str>,
) -> Result<(), String> {
    if let Some(executable) = tpm {
        verify_swtpm(executable)?;
    }
    if let Some(name) = only {
        selected(name, tpm.is_some())?;
    }
    let chosen = |name: &str| only.is_none_or(|only| only == name);
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
    // The paired desktop: login-desktop's, and with the portal the TPM
    // desktop guests'.
    files.push(("bin/td-authd", tests.join("bin/td-authd")));
    let paired = [
        ("td-firstboot", "bin/td-firstboot"),
        ("td-login", "bin/td-login"),
        ("td-compositor", "bin/td-compositor"),
    ];
    let portal = [
        ("td-busd", "bin/td-busd"),
        ("td-portal", "bin/td-portal"),
        ("td-jail", "bin/td-jail"),
    ];
    for (name, destination) in paired.iter().chain(portal.iter().filter(|_| tpm.is_some())) {
        let built = output(runner, name)?;
        files.push((destination, built.join("bin").join(name)));
    }
    // The login power-cut guest's volume, and the TPM cold-store guests'.
    let btrfs = output(runner, "btrfs-progs-x86-64")?;
    files.push(("bin/btrfs", btrfs.join("bin/btrfs")));
    files.push(("bin/mkfs.btrfs", btrfs.join("bin/mkfs.btrfs")));
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
    let store_disk = tpm_scratch
        .as_ref()
        .map(|scratch| {
            let path = scratch.dir.join("store.img");
            let file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .map_err(|e| format!("create persistent store fixture disk: {e}"))?;
            file.set_len(256 * 1024 * 1024)
                .map_err(|e| format!("size persistent store fixture disk: {e}"))?;
            Ok::<_, String>(path)
        })
        .transpose()?;
    let recovery_disk = tpm_scratch
        .as_ref()
        .map(|scratch| {
            let path = scratch.dir.join("recovery-store.img");
            let file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .map_err(|e| format!("create recovery store fixture disk: {e}"))?;
            file.set_len(256 * 1024 * 1024)
                .map_err(|e| format!("size recovery store fixture disk: {e}"))?;
            Ok::<_, String>(path)
        })
        .transpose()?;
    let cases = fixture::CASES
        .iter()
        .chain(fixture::LOGIN_CASES)
        .map(|case| (case, false))
        .chain(
            fixture::TPM_CASES
                .iter()
                .chain(fixture::FIDO_CASES)
                .filter(|_| tpm.is_some())
                .map(|case| (case, true)),
        )
        .filter(|((name, _), _)| chosen(name));
    let keep = runner.scratch_dir().join("login-desktop-screens");
    let screens = login_desktop_screens();
    let desktop_screens = GuestScreens {
        prompt: fixture::LOGIN_SCREEN,
        answer: fixture::LOGIN_SHOWN,
        screens: &screens,
        order: fixture::LOGIN_DESKTOP_SCREENS,
        keep: Some(&keep),
    };
    for ((name, test), is_tpm) in cases {
        let emulator = match (is_tpm, tpm, tpm_scratch.as_ref()) {
            (true, Some(executable), Some(scratch)) => {
                Some(Emulator::start(executable, &scratch.dir, name)?)
            }
            (false, _, _) => None,
            _ => return Err("missing TPM fixture configuration".into()),
        };
        let archive = case_archive(runner, &base, &contents, name)?;
        let desktop = *name == fixture::LOGIN_DESKTOP;
        if desktop {
            // Only this run's refused captures.
            match fs::remove_dir_all(&keep) {
                Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                    return Err(format!("clear {}: {error}", keep.display()));
                }
                _ => {}
            }
            fs::create_dir_all(&keep).map_err(|e| format!("create {}: {e}", keep.display()))?;
        }
        println!("[qemu-secret] {name}: {test}");
        let result = boot_with_timeout(
            &qemu,
            &kernel,
            &archive,
            BootPlan {
                disk: if name.starts_with("fido-cold-recovery-") {
                    recovery_disk
                        .as_deref()
                        .map(|path| BootDisk::new(path, false))
                } else if name.starts_with("fido-cold-") {
                    store_disk.as_deref().map(|path| BootDisk::new(path, false))
                } else {
                    tpm_disk
                        .as_deref()
                        .filter(|_| name.starts_with("tpm-"))
                        .map(|path| BootDisk::new(path, *name != "tpm-seal"))
                },
                mem: "512",
                target_marker: fixture::PASS,
                kill_on_marker: false,
                extra_append: if name.starts_with("fido-") || name.starts_with("login-") {
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
                side_channel: None,
                answers: None,
                cut: false,
                keep_console: None,
                screens: desktop.then_some(&desktop_screens),
                devices: Devices::ALL,
                screen: None,
                shell: None,
            },
            runner.scratch_dir(),
            Duration::from_secs(if desktop { 300 } else { 180 }),
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
    if let Some(name) = only {
        if name == fixture::LOGIN_CUT_CASE.0 {
            run_powercuts(runner, &qemu, &kernel, &base, &contents)?;
        }
        println!("PASS: the selected secret guest {name} alone; no other guest ran");
        return Ok(());
    }
    run_powercuts(runner, &qemu, &kernel, &base, &contents)?;
    println!("PASS: secret authority VM cases ({} fresh guests); credential intake, inspection, relocking and login-worker supervision; no token or TPM release claim", fixture::CASES.len());
    println!("PASS: login-key worker over UHID virtual keys ({} fresh guests); production discovery, HID worker and operation lock for unlock, one- and two-key enrollment, addition and removal; additions to eight and a ninth refused; wrong PIN, PIN AUTH BLOCKED until reinsertion, PIN BLOCKED, no key, two keys, a stranger key, denied presence, alwaysUv, a list too small and a credProtect probe refusal; a tampered verifier or public key and a stale signature; a record changed before token I/O or at the commit and an unshared record version, with no write; keepalives through a slow touch; simulated root acknowledgements and tier marker, no TPM, physical-presence or YubiKey claim", fixture::LOGIN_CASES.len() - 1);
    println!("PASS: login-desktop over a record the worker enrolled, the production compositor and authority checked through QMP captures: after a blank screen only blank or lock pixels are captured until the lock surface with its hostname and username, at boot and after a restart; a wrong PIN stays locked; while locked every capture shows only lock pixels; a key not in the record is refused at identify; the UHID key and its PIN typed on a UHID keyboard unlock to the client's window; a damaged directory locks and its chord shows the cause with no worker started; unenrolled starts unlocked; sampled captures and simulated root at enrollment, no physical-presence or YubiKey claim");
    println!("PASS: login record across abrupt QEMU kills ({} cold boots of one disposable Btrfs @var); cuts at every publication and removal stage and after a commit leave the old record or the whole new one, an unlink done or not, synced temporaries removed by the next write, and a rename or unlink before its directory sync lost; persistent virtual keys unlock whichever record survived; guest crash only, host storage retained, no host-power-loss, write-cache flush or torn-sector claim", fixture::LOGIN_CUT_PHASES.len());
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

/// The base initramfs, the guest files, and the case the fixture runs.
fn case_archive(
    runner: &RecipeCheckRunner,
    base: &[u8],
    contents: &[(&str, Vec<u8>)],
    name: &str,
) -> Result<PathBuf, String> {
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
    let mut file = File::create(&archive).map_err(|e| format!("create fixture archive: {e}"))?;
    file.write_all(base)
        .and_then(|_| file.write_all(&cpio::build(&entries).map_err(std::io::Error::other)?))
        .map_err(|e| format!("write fixture archive: {e}"))?;
    Ok(archive)
}

/// A cut phase ends on its own marker, killed by the host; setup and the
/// final check end as every other guest does.
fn powercut_result(result: &BootResult, phase: &str) -> Result<(), String> {
    let cut = format!("{} {phase}", fixture::LOGIN_CUT);
    let cuts = |line: &&str| line.starts_with(fixture::LOGIN_CUT);
    let ended = if is_cut(phase) {
        result.marker_killed
            && !result.exited_clean
            && result.console.lines().filter(cuts).count() == 1
            && result.console.lines().any(|line| line == cut)
            && !result
                .console
                .lines()
                .any(|line| line.starts_with(fixture::PASS) || line.starts_with("test result:"))
    } else {
        result.exited_clean
            && !result.marker_killed
            && !result.console.lines().any(|line| cuts(&line))
    };
    if !result.evidence.target
        || !ended
        || result.evidence.kernel_panic
        || result
            .console
            .lines()
            .any(|line| line.starts_with(fixture::FAIL))
    {
        return Err(format!(
            "login power-cut {phase} failed: {}\n{}",
            result.reason,
            tail(&result.console, 80)
        ));
    }
    Ok(())
}

fn is_cut(phase: &str) -> bool {
    !matches!(phase, "setup" | "final")
}

/// One guest, booted once per phase on one fresh disk. Each cut phase is
/// killed with SIGKILL when the guest names it, inside or just after its
/// write; the next boot finds what the disk kept.
fn run_powercuts(
    runner: &RecipeCheckRunner,
    qemu: &str,
    kernel: &Path,
    base: &[u8],
    contents: &[(&str, Vec<u8>)],
) -> Result<(), String> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let scratch = Scratch {
        dir: create_qmp_scratch_dir(&env::temp_dir(), &SEQ)?,
    };
    let disk = scratch.dir.join("login.img");
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&disk)
        .and_then(|file| file.set_len(256 * 1024 * 1024))
        .map_err(|e| format!("create login power-cut disk: {e}"))?;
    let (name, test) = fixture::LOGIN_CUT_CASE;
    let archive = case_archive(runner, base, contents, name)?;
    for phase in fixture::LOGIN_CUT_PHASES {
        let cut = is_cut(phase);
        let marker = if cut {
            format!("{} {phase}", fixture::LOGIN_CUT)
        } else {
            fixture::PASS.to_string()
        };
        let tokens = format!("td.hid-fixture=1 td.login-cut={phase}");
        println!("[qemu-secret] {name} {phase}: {test}");
        let result = boot_with_timeout(
            qemu,
            kernel,
            &archive,
            BootPlan {
                disk: Some(BootDisk::new(&disk, false)),
                mem: "512",
                target_marker: &marker,
                kill_on_marker: cut,
                extra_append: &tokens,
                user_net: false,
                audio: false,
                physical_input: false,
                capture_firefox_audio: false,
                tpm_socket: None,
                side_channel: None,
                screen: None,
                shell: None,
                answers: None,
                cut: false,
                keep_console: None,
                screens: None,
                devices: Devices::ALL,
            },
            runner.scratch_dir(),
            Duration::from_secs(180),
        )?;
        fs::write(
            runner
                .scratch_dir()
                .join(format!("secret-{name}-{phase}.log")),
            &result.console,
        )
        .map_err(|e| format!("save login power-cut console: {e}"))?;
        powercut_result(&result, phase)?;
        println!(
            "[qemu-secret] {name} {phase} passed in {:.2}s",
            result.elapsed.as_secs_f64()
        );
    }
    Ok(())
}

// login-desktop's screens (td-secret/DESIGN.md, "Login desktop guest"),
// each judged over a whole 1280x800 RGB capture.

/// The attention screen's and lock surface's ground and ink
/// (td-compositor/src/attention.rs).
const GROUND: [u8; 3] = [0x18, 0x20, 0x28];
const INK: [u8; 3] = [0xff, 0xff, 0xff];
/// What the guest paints before a generation starts, which no compositor
/// paints: magenta in either byte order.
const BLANK: [u8; 3] = [0xff, 0x00, 0xff];
/// The status bar's band and ground (td-compositor/src/bar.rs) and a
/// window's title band, focused or not (td-compositor/src/scene.rs), as
/// captured.
const BAR_BAND: usize = 24;
const BAR_GROUND: [u8; 3] = [0x20, 0x14, 0x18];
const TITLES: &[[u8; 3]] = &[[0x60, 0x28, 0x50], [0x3c, 0x34, 0x38]];
/// The chrome rows' place: the first at 276, 36 pixels apart, each 14
/// tall; an attention screen's last row comes below its seven.
const CHROME_TOP: usize = 276;
const CHROME_PITCH: usize = 36;
const CHROME_HEIGHT: usize = 14;
const ATTENTION_LAST: usize = CHROME_TOP + 7 * CHROME_PITCH;
/// A trusted prompt's rows: Unifont doubled, 40 pixels apart, centred.
const PROMPT_PITCH: usize = 40;
const PROMPT_HEIGHT: usize = 32;
/// The PIN field: a row gap below the prompt, its text row, a row gap and
/// one row of 6-pixel masks at an 8-pixel advance from 24 pixels in.
const FIELD_GAP: usize = 8;
const MASK_SIDE: usize = 6;
const MASK_ADVANCE: usize = 8;

fn whole(pixels: &[u8]) -> Result<(), String> {
    if pixels.len() != 1280 * 800 * 3 {
        return Err("expected a 1280x800 RGB capture".into());
    }
    Ok(())
}

fn rows_of(pixels: &[u8]) -> Result<std::slice::ChunksExact<'_, u8>, String> {
    whole(pixels)?;
    Ok(pixels.chunks_exact(1280 * 3))
}

/// Every pixel is one of `colours`.
fn only(pixels: &[u8], colours: &[[u8; 3]]) -> Result<bool, String> {
    whole(pixels)?;
    Ok(pixels
        .chunks_exact(3)
        .all(|pixel| colours.iter().any(|colour| pixel == colour)))
}

/// A screen named without arguments: one given any is the guest's error,
/// refused at once rather than waited for.
fn bare(arguments: &[&str]) -> Result<(), String> {
    match arguments {
        [] => Ok(()),
        _ => Err(format!("takes no arguments, was given {arguments:?}")),
    }
}

fn blank(pixels: &[u8], arguments: &[&str]) -> Result<bool, String> {
    bare(arguments)?;
    only(pixels, &[BLANK])
}

/// While a generation is locked, and until a locked generation's first
/// frame: the blank, a black output, or the ground and ink the lock
/// surface and the attention screens paint, and nothing else, so no
/// client or desktop pixel.
fn blank_or_lock(pixels: &[u8]) -> Result<bool, String> {
    only(pixels, &[BLANK, [0; 3], GROUND, INK])
}

/// Until an unlocked generation's first frame: no attention or lock pixel.
fn never_locked(pixels: &[u8]) -> Result<bool, String> {
    whole(pixels)?;
    Ok(!pixels.chunks_exact(3).any(|pixel| pixel == GROUND))
}

/// Exactly these chrome rows on the ground, and nothing else.
fn chrome(pixels: &[u8], rows: &[(usize, String)]) -> Result<bool, String> {
    for (y, row) in rows_of(pixels)?.enumerate() {
        if rows
            .iter()
            .any(|(top, _)| (*top..top + CHROME_HEIGHT).contains(&y))
        {
            continue;
        }
        if !row.chunks_exact(3).all(|pixel| pixel == GROUND) {
            return Ok(false);
        }
    }
    for (top, text) in rows {
        if !super::update::menu_row_matches(pixels, *top, text)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// The lock surface: the hostname, the username, `LOCKED` and the state's
/// rows (td-login/TOKEN-LOGIN.md, "Session lock").
fn lock_surface(state: &[&str]) -> Vec<(usize, String)> {
    [fixture::LOGIN_DESKTOP_HOST, "tester", "locked"]
        .into_iter()
        .chain(state.iter().copied())
        .enumerate()
        .map(|(index, row)| (CHROME_TOP + index * CHROME_PITCH, row.to_ascii_uppercase()))
        .collect()
}

/// An attention screen's title, a notice's rows and its last row.
fn attention(notice: &[&str]) -> Vec<(usize, String)> {
    std::iter::once("TD SECURE ATTENTION")
        .chain(notice.iter().copied())
        .enumerate()
        .map(|(index, row)| (CHROME_TOP + index * CHROME_PITCH, row.to_string()))
        .chain(std::iter::once((ATTENTION_LAST, "ESC TO RETURN".into())))
        .collect()
}

const DAMAGED: &[&str] = &["LOGIN KEY STATE UNAVAILABLE:", "DIRECTORY DAMAGED"];

fn locked(pixels: &[u8], arguments: &[&str]) -> Result<bool, String> {
    bare(arguments)?;
    chrome(pixels, &lock_surface(&["PRESS CTRL+ALT+ESC TO UNLOCK"]))
}

fn locked_damaged(pixels: &[u8], arguments: &[&str]) -> Result<bool, String> {
    bare(arguments)?;
    chrome(pixels, &lock_surface(DAMAGED))
}

fn wrong_pin(pixels: &[u8], arguments: &[&str]) -> Result<bool, String> {
    bare(arguments)?;
    chrome(pixels, &attention(&["WRONG PIN"]))
}

fn not_enrolled(pixels: &[u8], arguments: &[&str]) -> Result<bool, String> {
    bare(arguments)?;
    chrome(pixels, &attention(&["THIS KEY IS NOT ENROLLED HERE"]))
}

fn damaged(pixels: &[u8], arguments: &[&str]) -> Result<bool, String> {
    bare(arguments)?;
    chrome(pixels, &attention(DAMAGED))
}

/// The unlock's PIN step, as td-authd's consent describes it and the
/// compositor presents it with its time line, and beneath it the PIN
/// field's row with `masks` masks, or the touch request: every pixel.
fn pin_step(
    pixels: &[u8],
    fingerprint: &str,
    retries: &str,
    field: &str,
    masks: usize,
) -> Result<bool, String> {
    if fingerprint.len() != 8
        || !fingerprint
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        || !matches!(retries, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8")
        || masks > 63
    {
        return Err("invalid PIN step arguments".into());
    }
    let lines = [
        "TD SECURE ATTENTION".to_string(),
        "SESSION USER 1000".into(),
        "UNLOCK SESSION WITH A LOGIN KEY".into(),
        "ACCOUNT UID 1000".into(),
        format!("UNLOCK WITH KEY {fingerprint}"),
        format!("{retries} PIN ATTEMPTS LEFT ON THIS KEY"),
        "ENTER ITS PIN, THEN TOUCH THE KEY".into(),
        "ESC TO CANCEL".into(),
    ];
    // Eight rows and the time line, centred.
    let height = (lines.len() + 1) * PROMPT_PITCH - FIELD_GAP;
    let top = (800 - height) / 2;
    let time = top + lines.len() * PROMPT_PITCH;
    let text = top + height + FIELD_GAP;
    let band = text + PROMPT_HEIGHT + FIELD_GAP;
    for (y, row) in rows_of(pixels)?.enumerate() {
        let in_rows =
            (top..time + PROMPT_HEIGHT).contains(&y) && (y - top) % PROMPT_PITCH < PROMPT_HEIGHT;
        if in_rows || (text..text + PROMPT_HEIGHT).contains(&y) {
            continue;
        }
        let masked = (band..band + MASK_SIDE).contains(&y);
        for (x, pixel) in row.chunks_exact(3).enumerate() {
            let ink = masked
                && x >= 24
                && (x - 24) / MASK_ADVANCE < masks
                && (x - 24) % MASK_ADVANCE < MASK_SIDE;
            if pixel != if ink { INK } else { GROUND } {
                return Ok(false);
            }
        }
    }
    for (index, line) in lines.iter().enumerate() {
        if !super::update::row_matches(pixels, top + index * PROMPT_PITCH, line)? {
            return Ok(false);
        }
    }
    let mut timed = false;
    for seconds in 1..=120 {
        let unit = if seconds == 1 { "SECOND" } else { "SECONDS" };
        if super::update::row_matches(
            pixels,
            time,
            &format!("TIME LEFT WHEN SHOWN: {seconds} {unit}"),
        )? {
            timed = true;
            break;
        }
    }
    Ok(timed && super::update::row_matches(pixels, text, field)?)
}

/// `pin FINGERPRINT RETRIES MASKS`: the PIN field open with that many masks.
fn pin(pixels: &[u8], arguments: &[&str]) -> Result<bool, String> {
    let [fingerprint, retries, masks] = arguments else {
        return Err("pin takes a fingerprint, retries and a mask count".into());
    };
    let masks = masks
        .parse::<usize>()
        .map_err(|_| "invalid mask count".to_string())?;
    pin_step(
        pixels,
        fingerprint,
        retries,
        "ENTER THE PIN FOR THIS KEY",
        masks,
    )
}

/// `touch FINGERPRINT RETRIES`: the PIN sent, the touch asked for.
fn touch(pixels: &[u8], arguments: &[&str]) -> Result<bool, String> {
    let [fingerprint, retries] = arguments else {
        return Err("touch takes a fingerprint and retries".into());
    };
    pin_step(pixels, fingerprint, retries, "TOUCH YOUR KEY", 0)
}

/// The ordinary screen: no attention or lock pixel, and the status bar's
/// band mostly its ground.
fn desktop(pixels: &[u8], arguments: &[&str]) -> Result<bool, String> {
    bare(arguments)?;
    let band = pixels
        .get(..1280 * 3 * BAR_BAND)
        .ok_or("expected a 1280x800 RGB capture")?;
    let ground = band
        .chunks_exact(3)
        .filter(|pixel| *pixel == BAR_GROUND)
        .count();
    Ok(never_locked(pixels)? && ground * 2 > 1280 * BAR_BAND)
}

/// The desktop with the client's window on glass: its title band.
fn unlocked(pixels: &[u8], arguments: &[&str]) -> Result<bool, String> {
    let titled = pixels
        .chunks_exact(3)
        .filter(|pixel| TITLES.iter().any(|title| pixel == title))
        .count();
    Ok(desktop(pixels, arguments)? && titled >= 1000)
}

/// Each screen's check, and what holds from it until the next is
/// accepted: while locked, only lock pixels, the next screen asked for or
/// not; after an unlock, no lock pixel until the next blank. The touch
/// request's successor is the unlock, and the desktop is the last.
fn login_desktop_screens() -> Vec<Screen<'static>> {
    vec![
        Screen {
            name: "blank",
            check: &blank,
            then: Some(&blank_or_lock),
        },
        Screen {
            name: "blank-unlocked",
            check: &blank,
            then: Some(&never_locked),
        },
        Screen {
            name: "locked",
            check: &locked,
            then: Some(&blank_or_lock),
        },
        Screen {
            name: "locked-damaged",
            check: &locked_damaged,
            then: Some(&blank_or_lock),
        },
        Screen {
            name: "pin",
            check: &pin,
            then: Some(&blank_or_lock),
        },
        Screen {
            name: "touch",
            check: &touch,
            then: None,
        },
        Screen {
            name: "wrong-pin",
            check: &wrong_pin,
            then: Some(&blank_or_lock),
        },
        Screen {
            name: "not-enrolled",
            check: &not_enrolled,
            then: Some(&blank_or_lock),
        },
        Screen {
            name: "damaged",
            check: &damaged,
            then: Some(&blank_or_lock),
        },
        Screen {
            name: "unlocked",
            check: &unlocked,
            then: Some(&never_locked),
        },
        Screen {
            name: "desktop",
            check: &desktop,
            then: None,
        },
    ]
}

pub(crate) fn options(args: &[String]) -> Result<Option<PathBuf>, String> {
    match args {
        [] => Ok(None),
        [flag, path] if flag == "--tpm" && Path::new(path).is_absolute() => {
            Ok(Some(PathBuf::from(path)))
        }
        _ => Err(USAGE.into()),
    }
}

const USAGE: &str =
    "usage: td-recipe-eval qemu-secret [--tpm /absolute/path/to/swtpm] [--case NAME]";

/// `options`, and the one guest a trailing `--case NAME` selects.
pub(crate) fn selection(args: &[String]) -> Result<(Option<PathBuf>, Option<String>), String> {
    match args {
        [rest @ .., flag, name] if flag == "--case" => {
            let tpm = options(rest)?;
            selected(name, tpm.is_some())?;
            Ok((tpm, Some(name.clone())))
        }
        _ => Ok((options(args)?, None)),
    }
}

/// Guests that open what an earlier guest of the same run left on its
/// disk or TPM state, and that guest: none can run alone.
const DEPENDENT: &[(&str, &str)] = &[
    ("tpm-reopen", "tpm-seal"),
    ("tpm-pcr", "tpm-seal"),
    ("tpm-other", "tpm-seal"),
    ("fido-cold-reopen", "fido-cold-create"),
    ("fido-cold-recovery-reopen", "fido-cold-recovery-create"),
];

/// A guest name `--case` may select: a TPM guest only with `--tpm`, and
/// no guest that depends on an earlier one.
fn selected(name: &str, tpm: bool) -> Result<(), String> {
    if let Some((_, first)) = DEPENDENT.iter().find(|(case, _)| *case == name) {
        return Err(format!(
            "secret guest {name} opens what {first} leaves and cannot run alone; run qemu-secret --tpm without --case"
        ));
    }
    let plain = fixture::CASES
        .iter()
        .chain(fixture::LOGIN_CASES)
        .chain(std::iter::once(&fixture::LOGIN_CUT_CASE))
        .any(|(case, _)| *case == name);
    let needs_tpm = fixture::TPM_CASES
        .iter()
        .chain(fixture::FIDO_CASES)
        .any(|(case, _)| *case == name);
    match (plain, needs_tpm, tpm) {
        (true, _, _) | (_, true, true) => Ok(()),
        (_, true, false) => Err(format!("secret guest {name} needs --tpm")),
        _ => Err(format!("no secret guest is named {name}; {USAGE}")),
    }
}

pub(super) fn verify_swtpm(executable: &Path) -> Result<(), String> {
    let version = Command::new(executable)
        .arg("--version")
        .output()
        .map_err(|e| format!("run swtpm version check: {e}"))?;
    if !version.status.success() || !version.stdout.starts_with(b"TPM emulator version 0.10.1,") {
        return Err("TPM oracle requires the documented pinned swtpm 0.10.1".into());
    }
    Ok(())
}

pub(super) struct Emulator {
    child: std::process::Child,
    pub(super) socket: PathBuf,
    log: PathBuf,
}

impl Emulator {
    pub(super) fn start(executable: &Path, root: &Path, case: &str) -> Result<Self, String> {
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

    pub(super) fn diagnostic(&self, message: &str) -> String {
        let mut bytes = Vec::new();
        match File::open(&self.log).and_then(|file| file.take(65_536).read_to_end(&mut bytes)) {
            Ok(_) => format!(
                "{message}\nswtpm log (first 64 KiB):\n{}",
                String::from_utf8_lossy(&bytes)
            ),
            Err(error) => format!("{message}; could not read swtpm log: {error}"),
        }
    }

    pub(super) fn finish(mut self) -> Result<(), String> {
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
        assert_eq!(
            system_options(&args(&["--tpm", "/tmp/swtpm"])).unwrap(),
            (PathBuf::from("/tmp/swtpm"), false)
        );
        assert_eq!(
            system_options(&args(&["--tpm", "/tmp/swtpm", "--powercuts"])).unwrap(),
            (PathBuf::from("/tmp/swtpm"), true)
        );
        for values in [
            vec![],
            vec!["--powercuts"],
            vec!["--tpm", "relative", "--powercuts"],
            vec!["--tpm", "/tmp/swtpm", "--powercuts", "--powercuts"],
        ] {
            assert!(system_options(&args(&values)).is_err());
        }
        for phase in fixture::SYSTEM_PHASES {
            assert_eq!(
                fixture::system_phase(&format!("console=ttyS0 td.secret-system={phase}")).unwrap(),
                *phase
            );
        }
        for cmdline in [
            "",
            "td.secret-system=unknown",
            "td.secret-system=create td.secret-system=create",
            "td.secret-system=cut-queued td.secret-system=recover",
            "td.secret-system=create td.secret-system=",
        ] {
            assert!(fixture::system_phase(cmdline).is_err());
        }
    }

    #[test]
    fn system_cut_requires_an_observed_kill_at_one_exact_boundary() {
        let valid = || BootResult {
            evidence: ConsoleEvidence {
                target: true,
                ..ConsoleEvidence::default()
            },
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
                6 => result
                    .console
                    .push_str(&format!("{SYSTEM_SHUTDOWN_MARKER}\n")),
                7 => result
                    .console
                    .push_str(&format!("secret-fixture: {}\n", fixture::SYSTEM_PASS)),
                8 => result
                    .console
                    .push_str("secret-fixture: test result: ok. 1 passed; 0 failed;\n"),
                _ => result
                    .console
                    .push_str(&format!("secret-fixture: {}: refused\n", fixture::FAIL)),
            }
            assert!(
                system_result(&result, "cut-queued", true).is_err(),
                "case {case}"
            );
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
    fn login_guest_roster_is_pinned_and_each_guest_guards_its_own_case() {
        let names: Vec<&str> = fixture::LOGIN_CASES.iter().map(|(name, _)| *name).collect();
        assert_eq!(
            names,
            [
                "login-unlock",
                "login-blocked",
                "login-enroll-one",
                "login-enroll-two",
                "login-add-remove",
                "login-keepalive",
                "login-probe",
                "login-refusals",
                "login-verify",
                "login-changed",
                "login-eight",
                "login-desktop",
            ]
        );
        let source = include_str!("../../../../../../td-secret/src/login_vm.rs");
        for (name, test) in fixture::LOGIN_CASES {
            let function = test.strip_prefix("login_operation::tests::vm::").unwrap();
            assert!(
                function.starts_with("qemu_login_worker_")
                    || (*name == fixture::LOGIN_DESKTOP
                        && function.starts_with("qemu_login_desktop_"))
            );
            let body = source.split(&format!("fn {function}() {{")).nth(1).unwrap();
            assert!(
                body.trim_start()
                    .starts_with(&format!("guard(\"{name}\");")),
                "{name}"
            );
        }
        // The module's every guest is on a roster, and only those.
        assert_eq!(
            source.matches("#[ignore = ").count(),
            fixture::LOGIN_CASES.len() + 1
        );
        let all: std::collections::BTreeSet<_> = fixture::CASES
            .iter()
            .chain(fixture::TPM_CASES)
            .chain(fixture::FIDO_CASES)
            .chain(fixture::LOGIN_CASES)
            .chain(std::iter::once(&fixture::LOGIN_CUT_CASE))
            .map(|(name, _)| name)
            .collect();
        assert_eq!(all.len(), 5 + 4 + 11 + 12 + 1);
        assert!(fixture::LOGIN_CASES
            .iter()
            .all(|(name, _)| name.starts_with("login-")));
    }

    #[test]
    fn login_power_cut_phases_are_the_guests_boots_in_order() {
        let (name, test) = fixture::LOGIN_CUT_CASE;
        assert_eq!(name, "login-powercut");
        let source = include_str!("../../../../../../td-secret/src/login_vm.rs");
        let function = test.strip_prefix("login_operation::tests::vm::").unwrap();
        let body = source
            .split(&format!("fn {function}() -> Result<(), String> {{"))
            .nth(1)
            .unwrap();
        assert!(body
            .trim_start()
            .starts_with(&format!("guard(\"{name}\");")));
        // The guest's boots, between its setup and final check.
        let table = source
            .split("const BOOTS: &[Boot] = &[")
            .nth(1)
            .and_then(|rest| rest.split("\n];").next())
            .unwrap();
        let boots: Vec<&str> = table
            .split("phase: \"")
            .skip(1)
            .map(|rest| rest.split('"').next().unwrap())
            .collect();
        let phases = fixture::LOGIN_CUT_PHASES;
        assert_eq!(phases.first(), Some(&"setup"));
        assert_eq!(phases.last(), Some(&"final"));
        assert_eq!(boots, phases[1..phases.len() - 1]);
        assert_eq!(phases.iter().filter(|phase| is_cut(phase)).count(), 10);
        for phase in phases {
            assert_eq!(
                fixture::login_cut_phase(&format!("console=ttyS0 td.login-cut={phase}")),
                Ok(*phase)
            );
        }
        for cmdline in [
            "",
            "td.login-cut=unknown",
            "td.login-cut=setup td.login-cut=setup",
            "td.login-cut=setup td.login-cut=final",
            "td.login-cut=",
        ] {
            assert!(fixture::login_cut_phase(cmdline).is_err(), "{cmdline}");
        }
    }

    #[test]
    fn a_login_cut_requires_an_observed_kill_at_its_own_marker() {
        let cut = || BootResult {
            evidence: ConsoleEvidence {
                target: true,
                ..ConsoleEvidence::default()
            },
            exited_clean: false,
            marker_killed: true,
            reason: String::new(),
            console: format!("boot\n{} add-written\n", fixture::LOGIN_CUT),
            elapsed: Duration::from_secs(1),
            firefox_audio: FirefoxAudioCapture::NotRequested,
        };
        assert!(powercut_result(&cut(), "add-written").is_ok());
        for case in 0..10 {
            let mut result = cut();
            match case {
                0 => result.evidence.target = false,
                1 => result.marker_killed = false,
                2 => result.exited_clean = true,
                3 => result.evidence.kernel_panic = true,
                4 => result.console.clear(),
                5 => result
                    .console
                    .push_str(&format!("{} add-written\n", fixture::LOGIN_CUT)),
                6 => result
                    .console
                    .push_str(&format!("{} add-synced\n", fixture::LOGIN_CUT)),
                7 => result.console.push_str(&format!("{}\n", fixture::PASS)),
                8 => result
                    .console
                    .push_str("test result: ok. 1 passed; 0 failed;\n"),
                _ => result
                    .console
                    .push_str(&format!("{}: refused\n", fixture::FAIL)),
            }
            assert!(
                powercut_result(&result, "add-written").is_err(),
                "case {case}"
            );
        }
        assert!(powercut_result(&cut(), "add-synced").is_err());
        // Setup and the final check end cleanly, with no cut.
        assert!(powercut_result(&cut(), "setup").is_err());
        let clean = || BootResult {
            exited_clean: true,
            marker_killed: false,
            console: format!("{}\n", fixture::PASS),
            ..cut()
        };
        for phase in ["setup", "final"] {
            assert!(powercut_result(&clean(), phase).is_ok());
            let mut result = clean();
            result
                .console
                .push_str(&format!("{} {phase}\n", fixture::LOGIN_CUT));
            assert!(powercut_result(&result, phase).is_err());
        }
        assert!(powercut_result(&clean(), "enroll-created").is_err());
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
        assert_eq!(names.len(), 5);
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
    /// The guest asks for exactly the host's order of screens, each one
    /// the host checks, in the console words the host reads.
    #[test]
    fn login_desktop_asks_for_the_screens_the_host_checks_in_order() {
        let source = include_str!("../../../../../../td-secret/src/login_vm.rs");
        let (_, test) = fixture::LOGIN_CASES
            .iter()
            .find(|(name, _)| *name == fixture::LOGIN_DESKTOP)
            .unwrap();
        let function = test.strip_prefix("login_operation::tests::vm::").unwrap();
        let body = source
            .split(&format!("fn {function}() {{"))
            .nth(1)
            .unwrap()
            .split("\n}\n")
            .next()
            .unwrap();
        let asked: Vec<&str> = body
            .split("host.screen(\"")
            .skip(1)
            .map(|rest| rest.split('"').next().unwrap())
            .collect();
        assert_eq!(asked, fixture::LOGIN_DESKTOP_SCREENS);
        let screens = login_desktop_screens();
        let names: std::collections::BTreeSet<_> =
            screens.iter().map(|screen| screen.name).collect();
        assert_eq!(names.len(), screens.len());
        assert_eq!(
            names,
            fixture::LOGIN_DESKTOP_SCREENS.iter().copied().collect()
        );
        for pin in [
            format!("const SCREEN: &str = \"{}\";", fixture::LOGIN_SCREEN),
            format!("const SHOWN: &str = \"{}\";", fixture::LOGIN_SHOWN),
            format!(
                "const HOSTNAME: &str = \"{}\";",
                fixture::LOGIN_DESKTOP_HOST
            ),
        ] {
            assert_eq!(source.matches(&pin).count(), 1, "{pin}");
        }
    }

    /// Chrome rows drawn from the compositor's own 5x7 glyphs, doubled
    /// from 24 pixels in, on the attention ground.
    fn chrome_frame(rows: &[(usize, &str)]) -> Vec<u8> {
        let chrome = include_str!("../../../../../../td-compositor/src/ui.rs");
        let mut pixels = GROUND.repeat(1280 * 800);
        for (top, text) in rows {
            for (column, character) in text.chars().enumerate() {
                let prefix = format!("b'{character}' => [");
                let glyph = chrome
                    .lines()
                    .find_map(|line| line.trim().strip_prefix(&prefix)?.strip_suffix("],"))
                    .unwrap();
                for (y, bits) in glyph
                    .split(',')
                    .map(|row| row.trim().parse::<u8>().unwrap())
                    .enumerate()
                {
                    for x in (0..5).filter(|x| bits & (1 << (4 - x)) != 0) {
                        for (dx, dy) in [(0, 0), (0, 1), (1, 0), (1, 1)] {
                            let at =
                                ((top + y * 2 + dy) * 1280 + 24 + column * 12 + x * 2 + dx) * 3;
                            pixels[at..at + 3].copy_from_slice(&INK);
                        }
                    }
                }
            }
        }
        pixels
    }

    fn put(pixels: &mut [u8], x: usize, y: usize, colour: [u8; 3]) {
        let at = (y * 1280 + x) * 3;
        pixels[at..at + 3].copy_from_slice(&colour);
    }

    #[test]
    fn login_desktop_lock_and_attention_screens_are_whole_frames() {
        let enrolled = chrome_frame(&[
            (276, "TD-LOGIN-DESKTOP"),
            (312, "TESTER"),
            (348, "LOCKED"),
            (384, "PRESS CTRL+ALT+ESC TO UNLOCK"),
        ]);
        assert!(locked(&enrolled, &[]).unwrap());
        assert!(locked(&enrolled, &["extra"]).is_err());
        assert!(!locked_damaged(&enrolled, &[]).unwrap());
        assert!(blank_or_lock(&enrolled).unwrap());
        assert!(!never_locked(&enrolled).unwrap());
        // A client's pixel anywhere, a cursor or a bar, is no lock surface.
        for (x, y, colour) in [
            (0, 0, BAR_GROUND),
            (640, 700, [0x11, 0x22, 0x33]),
            (1279, 799, INK),
            (24, 276, GROUND),
        ] {
            let mut shown = enrolled.clone();
            put(&mut shown, x, y, colour);
            assert!(!locked(&shown, &[]).unwrap(), "{x},{y}");
        }
        let mut leaked = enrolled.clone();
        put(&mut leaked, 0, 0, BAR_GROUND);
        assert!(!blank_or_lock(&leaked).unwrap());
        let damaged_lock = chrome_frame(&[
            (276, "TD-LOGIN-DESKTOP"),
            (312, "TESTER"),
            (348, "LOCKED"),
            (384, "LOGIN KEY STATE UNAVAILABLE:"),
            (420, "DIRECTORY DAMAGED"),
        ]);
        assert!(locked_damaged(&damaged_lock, &[]).unwrap());
        assert!(!locked(&damaged_lock, &[]).unwrap());
        let wrong = chrome_frame(&[
            (276, "TD SECURE ATTENTION"),
            (312, "WRONG PIN"),
            (528, "ESC TO RETURN"),
        ]);
        assert!(wrong_pin(&wrong, &[]).unwrap());
        assert!(!damaged(&wrong, &[]).unwrap());
        assert!(!not_enrolled(&wrong, &[]).unwrap());
        let stranger = chrome_frame(&[
            (276, "TD SECURE ATTENTION"),
            (312, "THIS KEY IS NOT ENROLLED HERE"),
            (528, "ESC TO RETURN"),
        ]);
        assert!(not_enrolled(&stranger, &[]).unwrap());
        assert!(!wrong_pin(&stranger, &[]).unwrap());
        assert!(!locked(&wrong, &[]).unwrap());
        let cause = chrome_frame(&[
            (276, "TD SECURE ATTENTION"),
            (312, "LOGIN KEY STATE UNAVAILABLE:"),
            (348, "DIRECTORY DAMAGED"),
            (528, "ESC TO RETURN"),
        ]);
        assert!(damaged(&cause, &[]).unwrap());
        assert!(!wrong_pin(&cause, &[]).unwrap());
        assert!(locked(&[0; 3], &[]).is_err());
    }

    #[test]
    fn login_desktop_blank_and_desktop_screens() {
        let blank_frame = BLANK.repeat(1280 * 800);
        assert!(blank(&blank_frame, &[]).unwrap());
        assert!(blank(&blank_frame, &["x"]).is_err());
        assert!(blank_or_lock(&blank_frame).unwrap());
        assert!(never_locked(&blank_frame).unwrap());
        let mut black = blank_frame.clone();
        put(&mut black, 3, 3, [0, 0, 0]);
        assert!(!blank(&black, &[]).unwrap());
        assert!(blank_or_lock(&black).unwrap());
        let mut touched = blank_frame.clone();
        put(&mut touched, 3, 3, BAR_GROUND);
        assert!(!blank_or_lock(&touched).unwrap());
        // The ordinary screen: the bar's band over the desktop's ground.
        let mut screen = [0x20, 0x25, 0x30].repeat(1280 * 800);
        screen[..1280 * 3 * BAR_BAND].copy_from_slice(&BAR_GROUND.repeat(1280 * BAR_BAND));
        assert!(desktop(&screen, &[]).unwrap());
        assert!(!unlocked(&screen, &[]).unwrap());
        for y in 100..110 {
            for x in 100..200 {
                put(&mut screen, x, y, TITLES[0]);
            }
        }
        assert!(unlocked(&screen, &[]).unwrap());
        // Any attention pixel, and a missing bar, are no desktop.
        let mut attention = screen.clone();
        put(&mut attention, 640, 400, GROUND);
        assert!(!desktop(&attention, &[]).unwrap());
        assert!(!never_locked(&attention).unwrap());
        let mut barless = screen.clone();
        barless[..1280 * 3 * BAR_BAND].copy_from_slice(&[0x20, 0x25, 0x30].repeat(1280 * BAR_BAND));
        assert!(!desktop(&barless, &[]).unwrap());
    }

    /// The unlock's PIN step as the compositor lays it out: eight Unifont
    /// rows and the time line, doubled and centred, then the field.
    fn prompt_frame(retries: u8, field: &str, masks: usize, seconds: u32) -> Vec<u8> {
        let mut pixels = GROUND.repeat(1280 * 800);
        let rows = [
            "TD SECURE ATTENTION".to_string(),
            "SESSION USER 1000".into(),
            "UNLOCK SESSION WITH A LOGIN KEY".into(),
            "ACCOUNT UID 1000".into(),
            "UNLOCK WITH KEY 3fa2c1d0".into(),
            format!("{retries} PIN ATTEMPTS LEFT ON THIS KEY"),
            "ENTER ITS PIN, THEN TOUCH THE KEY".into(),
            "ESC TO CANCEL".into(),
            format!("TIME LEFT WHEN SHOWN: {seconds} SECONDS"),
        ];
        fn text(pixels: &mut [u8], top: usize, row: &str) {
            for (column, character) in row.bytes().enumerate() {
                for y in 0..32 {
                    for x in 0..16 {
                        if super::super::update::ascii_pixel(character, x / 2, y / 2).unwrap() {
                            put(pixels, 24 + column * 16 + x, top + y, INK);
                        }
                    }
                }
            }
        }
        for (index, row) in rows.iter().enumerate() {
            text(&mut pixels, 224 + index * 40, row);
        }
        text(&mut pixels, 584, field);
        for mask in 0..masks {
            for y in 624..630 {
                for x in 0..6 {
                    put(&mut pixels, 24 + mask * 8 + x, y, INK);
                }
            }
        }
        pixels
    }

    #[test]
    fn login_desktop_pin_step_is_the_whole_prompt_and_field() {
        let open = prompt_frame(8, "ENTER THE PIN FOR THIS KEY", 4, 117);
        assert!(pin(&open, &["3fa2c1d0", "8", "4"]).unwrap());
        assert!(!pin(&open, &["3fa2c1d0", "8", "3"]).unwrap());
        assert!(!pin(&open, &["3fa2c1d0", "8", "5"]).unwrap());
        assert!(!pin(&open, &["3fa2c1d0", "7", "4"]).unwrap());
        assert!(!pin(&open, &["3fa2c1d1", "8", "4"]).unwrap());
        assert!(!touch(&open, &["3fa2c1d0", "8"]).unwrap());
        let empty = prompt_frame(7, "ENTER THE PIN FOR THIS KEY", 0, 120);
        assert!(pin(&empty, &["3fa2c1d0", "7", "0"]).unwrap());
        let touching = prompt_frame(7, "TOUCH YOUR KEY", 0, 120);
        assert!(touch(&touching, &["3fa2c1d0", "7"]).unwrap());
        assert!(!pin(&touching, &["3fa2c1d0", "7", "0"]).unwrap());
        // No other pixel: a stray mask, or one below the field.
        for (x, y) in [(24 + 4 * 8, 624), (24, 640), (1279, 0)] {
            let mut stray = open.clone();
            put(&mut stray, x, y, INK);
            assert!(!pin(&stray, &["3fa2c1d0", "8", "4"]).unwrap(), "{x},{y}");
        }
        for arguments in [
            &["3fa2c1d0", "8"][..],
            &["3FA2C1D0", "8", "4"],
            &["3fa2c1d0", "9", "4"],
            &["3fa2c1d0", "8", "64"],
            &["3fa2c1d0", "8", "x"],
        ] {
            assert!(pin(&open, arguments).is_err(), "{arguments:?}");
        }
    }

    #[test]
    fn qemu_secret_selects_one_guest_by_name() {
        let args = |values: &[&str]| values.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(selection(&args(&[])).unwrap(), (None, None));
        assert_eq!(
            selection(&args(&["--case", "login-desktop"])).unwrap(),
            (None, Some("login-desktop".into()))
        );
        assert_eq!(
            selection(&args(&["--tpm", "/tmp/swtpm", "--case", "fido-desktop"])).unwrap(),
            (
                Some(PathBuf::from("/tmp/swtpm")),
                Some("fido-desktop".into())
            )
        );
        assert_eq!(
            selection(&args(&["--case", "login-powercut"])).unwrap(),
            (None, Some("login-powercut".into()))
        );
        for values in [
            &["--case", "fido-desktop"][..],
            &["--case", "nothing"],
            &["--case"],
            &["--case", "login-desktop", "--tpm", "/tmp/swtpm"],
        ] {
            assert!(selection(&args(values)).is_err(), "{values:?}");
        }
        // A guest that opens what an earlier one left cannot run alone;
        // the guest that leaves it can.
        for (dependent, first) in DEPENDENT {
            let error =
                selection(&args(&["--tpm", "/tmp/swtpm", "--case", dependent])).unwrap_err();
            assert!(error.contains(first), "{error}");
            assert!(selection(&args(&["--tpm", "/tmp/swtpm", "--case", first])).is_ok());
        }
        // Every guest that opens a disk an earlier one wrote is named.
        let opened: Vec<&str> = fixture::TPM_CASES
            .iter()
            .chain(fixture::FIDO_CASES)
            .map(|(name, _)| *name)
            .filter(|name| {
                (name.starts_with("tpm-") && *name != "tpm-seal")
                    || (name.starts_with("fido-cold-") && !name.ends_with("-create"))
            })
            .collect();
        let named: Vec<&str> = DEPENDENT.iter().map(|(name, _)| *name).collect();
        assert_eq!(opened, named);
    }

    /// While locked, every screen holds only lock pixels until the next is
    /// accepted; the touch request and the desktop hold nothing, and the
    /// unlock holds no lock pixel.
    #[test]
    fn login_desktop_locked_screens_hold_only_lock_pixels() {
        let lock = chrome_frame(&[
            (276, "TD-LOGIN-DESKTOP"),
            (312, "TESTER"),
            (348, "LOCKED"),
            (384, "PRESS CTRL+ALT+ESC TO UNLOCK"),
        ]);
        let mut client = lock.clone();
        put(&mut client, 640, 400, [0x11, 0x22, 0x33]);
        let mut bar = lock.clone();
        put(&mut bar, 0, 0, BAR_GROUND);
        let blank_frame = BLANK.repeat(1280 * 800);
        let screens = login_desktop_screens();
        for screen in &screens {
            let holds = screen.then;
            match screen.name {
                "blank" | "locked" | "locked-damaged" | "pin" | "wrong-pin" | "not-enrolled"
                | "damaged" => {
                    let holds = holds.unwrap();
                    assert!(holds(&lock).unwrap(), "{}", screen.name);
                    assert!(holds(&blank_frame).unwrap(), "{}", screen.name);
                    assert!(!holds(&client).unwrap(), "{}", screen.name);
                    assert!(!holds(&bar).unwrap(), "{}", screen.name);
                }
                "blank-unlocked" | "unlocked" => {
                    let holds = holds.unwrap();
                    assert!(!holds(&lock).unwrap(), "{}", screen.name);
                    assert!(holds(&blank_frame).unwrap(), "{}", screen.name);
                }
                "touch" | "desktop" => assert!(holds.is_none(), "{}", screen.name),
                other => panic!("unexpected screen {other}"),
            }
        }
    }
}
