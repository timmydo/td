#![deny(unsafe_code)]

use std::fs::{self, File};
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};

pub const CASES: &[(&str, &str)] = &[
    (
        "intake",
        "secret_intake::tests::root_public_client_uses_the_human_identity_and_immutable_descriptor",
    ),
    (
        "prepare",
        "session::tests::root_session_preparation_failure_and_generation_exit_relock",
    ),
    (
        "inspect",
        "session::tests::root_inspection_observes_file_state_without_publishing_or_repairing",
    ),
    (
        "supervise",
        "unlock::tests::root_supervisor_relocks_after_the_production_worker_refuses",
    ),
    (
        "supervise-login",
        "session::tests::root_login_supervision_meets_the_production_worker",
    ),
];
pub const TPM_CASES: &[(&str, &str)] = &[
    (
        "tpm-seal",
        "tpm::tests::qemu_device_seals_to_persistent_state",
    ),
    (
        "tpm-reopen",
        "tpm::tests::qemu_device_reopens_after_cold_boot",
    ),
    ("tpm-pcr", "tpm::tests::qemu_device_refuses_changed_pcr"),
    ("tpm-other", "tpm::tests::qemu_device_refuses_another_tpm"),
];
pub const FIDO_CASES: &[(&str, &str)] = &[
    (
        "fido-hid",
        "fido_device::vm_tests::qemu_hid_assertion_uses_production_worker_and_guest_tpm",
    ),
    (
        "fido-deadline",
        "fido_device::vm_tests::qemu_hid_keepalives_cannot_extend_the_worker_deadline",
    ),
    (
        "fido-enroll-single",
        "fido_device::vm_tests::qemu_hid_enrolls_unrecoverable_and_unseals_with_a_fresh_assertion",
    ),
    (
        "fido-enroll-recovery",
        "fido_device::vm_tests::qemu_hid_enrolls_recovery_and_refuses_replays_and_wrong_keys",
    ),
    (
        "fido-operations-single",
        "fido_device::vm_tests::qemu_private_workers_enroll_unlock_write_and_cancel_without_recovery",
    ),
    (
        "fido-operations-recovery",
        "fido_device::vm_tests::qemu_private_workers_enroll_unlock_and_write_with_recovery",
    ),
    (
        "fido-desktop",
        "fido_device::vm_tests::desktop::qemu_compositor_enrolls_unlocks_and_authorizes_public_credential_write",
    ),
    (
        "fido-cold-create",
        "fido_device::vm_tests::desktop::qemu_desktop_creates_persistent_store",
    ),
    (
        "fido-cold-reopen",
        "fido_device::vm_tests::desktop::qemu_desktop_reopens_persistent_store_locked",
    ),
    (
        "fido-cold-recovery-create",
        "fido_device::vm_tests::desktop::qemu_desktop_creates_persistent_recovery_store",
    ),
    (
        "fido-cold-recovery-reopen",
        "fido_device::vm_tests::desktop::qemu_desktop_recovers_persistent_store_without_primary",
    ),
];
/// The login worker over UHID virtual keys; these guests need no TPM.
pub const LOGIN_CASES: &[(&str, &str)] = &[
    (
        "login-unlock",
        "login_operation::tests::vm::qemu_login_worker_unlocks_each_key_and_refuses_wrong_pins_strangers_and_counts",
    ),
    (
        "login-blocked",
        "login_operation::tests::vm::qemu_login_worker_blocks_after_three_wrong_pins_until_the_key_is_reinserted",
    ),
    (
        "login-enroll-one",
        "login_operation::tests::vm::qemu_login_worker_enrolls_one_key_whose_record_unlocks_and_is_then_removed",
    ),
    (
        "login-enroll-two",
        "login_operation::tests::vm::qemu_login_worker_enrolls_two_keys_across_a_swap_of_devices",
    ),
    (
        "login-add-remove",
        "login_operation::tests::vm::qemu_login_worker_adds_a_key_across_a_swap_and_removes_the_authorizing_one",
    ),
    (
        "login-keepalive",
        "login_operation::tests::vm::qemu_login_worker_waits_through_keepalives_for_a_slow_touch",
    ),
    (
        "login-probe",
        "login_operation::tests::vm::qemu_login_worker_refuses_a_key_whose_credprotect_default_fails_the_probe",
    ),
    (
        "login-refusals",
        "login_operation::tests::vm::qemu_login_worker_ends_on_denied_presence_always_uv_and_a_list_too_small",
    ),
    (
        "login-verify",
        "login_operation::tests::vm::qemu_login_worker_never_unlocks_a_tampered_record_or_a_stale_signature",
    ),
    (
        "login-changed",
        "login_operation::tests::vm::qemu_login_worker_refuses_a_changed_record_or_an_unshared_version_without_writing",
    ),
    (
        "login-eight",
        "login_operation::tests::vm::qemu_login_worker_adds_keys_to_eight_and_refuses_a_ninth_before_any_token",
    ),
    (
        LOGIN_DESKTOP,
        "login_operation::tests::vm::qemu_login_desktop_starts_every_generation_locked_and_unlocks_with_the_key",
    ),
];
/// The paired desktop over a record the worker enrolled: a login case
/// whose host checks the display at each step the guest names.
pub const LOGIN_DESKTOP: &str = "login-desktop";
/// The guest's hostname, which the lock surface shows.
pub const LOGIN_DESKTOP_HOST: &str = "td-login-desktop";
/// The guest asks for a display check on its console as `SCREEN NAME
/// [ARGUMENT...]` and waits on ttyS0 for the host's `SHOWN NAME`.
pub const LOGIN_SCREEN: &str = "TD-LOGIN-SCREEN";
pub const LOGIN_SHOWN: &str = "TD-LOGIN-SHOWN";
/// The checks the guest asks for, in its order: the first generation
/// locked from its first frame, a wrong PIN and an unlock; the relock of
/// a restarted generation; a damaged directory's; and an unenrolled one.
pub const LOGIN_DESKTOP_SCREENS: &[&str] = &[
    "blank",
    "locked",
    "locked",
    "pin",
    "pin",
    "wrong-pin",
    "locked",
    "not-enrolled",
    "locked",
    "pin",
    "pin",
    "touch",
    "unlocked",
    "blank",
    "locked",
    "blank",
    "locked-damaged",
    "damaged",
    "locked-damaged",
    "blank-unlocked",
    "desktop",
];
/// The login record store across power cuts: one TPM-free guest test,
/// booted once per phase, in this order, on one disposable disk.
pub const LOGIN_CUT_CASE: (&str, &str) = (
    "login-powercut",
    "login_operation::tests::vm::qemu_login_record_survives_power_cuts_inside_its_writes",
);
pub const LOGIN_CUT: &str = "TD-LOGIN-CUT";
/// A clean setup, ten boots each checking the cut before it and then cut
/// inside or just after its own write, and a clean final check.
pub const LOGIN_CUT_PHASES: &[&str] = &[
    "setup",
    "enroll-created",
    "enroll-synced",
    "enroll-renamed",
    "enroll-committed",
    "add-written",
    "add-attempted",
    "add-synced",
    "remove-attempted",
    "remove-unlinked",
    "remove-synced",
    "final",
];

/// The one power-cut phase the command line names.
pub fn login_cut_phase(cmdline: &str) -> Result<&str, String> {
    let mut phases = cmdline
        .split_ascii_whitespace()
        .filter_map(|token| token.strip_prefix("td.login-cut="));
    let phase = phases
        .next()
        .filter(|phase| LOGIN_CUT_PHASES.contains(phase))
        .ok_or("unknown or missing login power-cut phase")?;
    if phases.next().is_some() {
        return Err("duplicate login power-cut phase".into());
    }
    Ok(phase)
}

pub const SYSTEM_TEST: &str =
    "fido_device::vm_tests::desktop::system::qemu_installed_system_secret_lifecycle";
pub const SYSTEM_PASS: &str = "TD-SECRET-SYSTEM-PASS";
pub const SYSTEM_CUT: &str = "TD-SECRET-SYSTEM-CUT";
pub const SYSTEM_PHASES: &[&str] = &[
    "create",
    "recover",
    "cut-queued",
    "cut-written",
    "recover-written",
];

pub fn system_phase(cmdline: &str) -> Result<&str, String> {
    let mut phases = cmdline
        .split_ascii_whitespace()
        .filter_map(|token| token.strip_prefix("td.secret-system="));
    let phase = phases
        .next()
        .filter(|phase| SYSTEM_PHASES.contains(phase))
        .ok_or("unknown or missing system secret phase")?;
    if phases.next().is_some() {
        return Err("duplicate system secret phase".into());
    }
    Ok(phase)
}

/// qemu-login-system (td-secret/DESIGN.md, "Login system guest"): the
/// full-system TPM-free login guest, its one test booted once per phase,
/// in this order, on one disposable volume beneath the stock supervisor.
pub const LOGIN_SYSTEM_TEST: &str =
    "login_operation::tests::vm::system::qemu_login_system_locks_unlocks_and_refuses_on_a_full_system";
pub const LOGIN_SYSTEM_PASS: &str = "TD-LOGIN-SYSTEM-PASS";
/// `seed` enrolls; `locked` is the enrolled machine's boot and its
/// relocks; each damage named after `damaged-` boots unavailable and is
/// repaired, and the `repaired-` boot after it is an enrolled one again.
pub const LOGIN_SYSTEM_PHASES: &[&str] = &[
    "seed",
    "locked",
    "damaged-directory-mode",
    "repaired-directory-mode",
    "damaged-directory-owner",
    "repaired-directory-owner",
    "damaged-directory-file",
    "repaired-directory-file",
    "damaged-record-mode",
    "repaired-record-mode",
    "damaged-record-links",
    "repaired-record-links",
    "damaged-record-truncated",
    "repaired-record-truncated",
    "damaged-record-version",
    "repaired-record-version",
];
/// The stock image's hostname, which its lock surface shows.
pub const LOGIN_SYSTEM_HOST: &str = "td";
/// The serial greeter's one refusal line (td-login/THREAT-MODEL.md §3).
pub const CONSOLE_REFUSED: &str =
    "td-login: login keys enrolled or unavailable; console login refused";
/// One unlock as the guest asks the host to see it: the PIN step empty and
/// with four masks, the touch request around the worker's check and again
/// once it is known alive, the touch request once more just before the
/// guest completes the touch, the client's window, and the desktop.
const UNLOCK: &[&str] = &[
    "pin", "pin", "touch", "touch", "release", "unlocked", "desktop",
];

/// The screens each phase asks for, in order. `seed` boots unlocked from
/// the blank to the desktop, once its serial greeter has logged in and
/// been stopped so that no shell reads the answers. `locked` starts
/// locked under a client, then unlocks, relocks with `Super+l`, unlocks,
/// relocks with `L`, unlocks, queues three updates, relocks by a killed
/// compositor, restarts the pair with the state helper failing until it
/// answers again, unlocks, and last suspends to RAM, after which its
/// virtio-gpu card shows nothing more.
pub fn login_system_screens(phase: &str) -> Vec<&'static str> {
    let mut screens = Vec::new();
    if phase == "seed" {
        // The cutover's restarted pair locks on the enrolled record.
        screens.extend(["blank-unlocked", "desktop", "locked"]);
    } else if phase == "locked" {
        screens.extend(["blank", "locked", "locked"]);
        screens.extend(UNLOCK);
        screens.push("locked");
        screens.extend(UNLOCK);
        screens.extend(["menu", "locked"]);
        screens.extend(UNLOCK);
        for _ in 0..2 {
            screens.extend(["menu", "update-refused", "desktop"]);
        }
        screens.extend(["menu", "install", "desktop", "killed", "locked"]);
        screens.extend([
            "locked-unreadable",
            "unreadable",
            "locked-unreadable",
            "locked",
        ]);
        screens.extend(UNLOCK);
        screens.push("asleep");
    } else if phase.starts_with("damaged-directory-") {
        screens.extend(["blank", "locked-damaged", "damaged", "locked-damaged"]);
    } else if phase.starts_with("damaged-record-") {
        screens.extend(["blank", "locked-record", "record", "locked-record"]);
    } else if phase.starts_with("repaired-") {
        screens.extend(["blank", "locked"]);
    }
    screens
}

/// The one login system phase the command line names.
pub fn login_system_phase(cmdline: &str) -> Result<&str, String> {
    let mut phases = cmdline
        .split_ascii_whitespace()
        .filter_map(|token| token.strip_prefix("td.login-system="));
    let phase = phases
        .next()
        .filter(|phase| LOGIN_SYSTEM_PHASES.contains(phase))
        .ok_or("unknown or missing login system phase")?;
    if phases.next().is_some() {
        return Err("duplicate login system phase".into());
    }
    Ok(phase)
}

pub const PASS: &str = "TD-SECRET-VM-PASS";
pub const FAIL: &str = "TD-SECRET-VM-FAIL";

pub fn test_passed(success: bool, output: &str) -> bool {
    success
        && output
            .lines()
            .rfind(|line| line.starts_with("test result: "))
            .is_some_and(|line| line.starts_with("test result: ok. 1 passed; 0 failed; 0 ignored;"))
}

fn applet(args: &[&str]) -> Result<(), String> {
    let status = Command::new("/bin/td-init")
        .args(args)
        .status()
        .map_err(|e| format!("td-init {args:?}: {e}"))?;
    if !status.success() {
        return Err(format!("td-init {args:?}: {status}"));
    }
    Ok(())
}

fn run() -> Result<(), String> {
    if std::process::id() != 1 {
        return Err("fixture must be guest PID 1".into());
    }
    for dir in ["/proc", "/sys", "/dev", "/run", "/tmp"] {
        fs::create_dir_all(dir).map_err(|e| format!("create {dir}: {e}"))?;
    }
    for (kind, path) in [
        ("proc", "/proc"),
        ("sysfs", "/sys"),
        ("devtmpfs", "/dev"),
        ("tmpfs", "/run"),
    ] {
        applet(&["mount", "-t", kind, kind, path])?;
    }
    // Authority configuration needs protected ancestors, including initramfs root.
    for path in ["/", "/run"] {
        fs::set_permissions(path, fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("chmod {path}: {e}"))?;
    }
    fs::set_permissions("/tmp", fs::Permissions::from_mode(0o1777))
        .map_err(|e| format!("chmod /tmp: {e}"))?;
    let selected = fs::read_to_string("/case").map_err(|e| format!("read case: {e}"))?;
    let (_, test, executable) = CASES
        .iter()
        .map(|(name, test)| (*name, *test, "/bin/td-authd-tests"))
        .chain(
            TPM_CASES
                .iter()
                .chain(FIDO_CASES)
                .chain(LOGIN_CASES)
                .chain(std::iter::once(&LOGIN_CUT_CASE))
                .map(|(name, test)| (*name, *test, "/bin/td-secret-tests")),
        )
        .find(|(name, _, _)| *name == selected)
        .ok_or_else(|| "unknown VM case".to_string())?;
    if selected == LOGIN_CUT_CASE.0 {
        let cmdline =
            fs::read_to_string("/proc/cmdline").map_err(|e| format!("read command line: {e}"))?;
        login_cut_phase(&cmdline)?;
    }
    let log = File::create("/run/test.log").map_err(|e| format!("test log: {e}"))?;
    let errors = log
        .try_clone()
        .map_err(|e| format!("clone test log: {e}"))?;
    let status = Command::new(executable)
        .args(["--exact", test, "--ignored", "--test-threads=1"])
        .env_clear()
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(errors)
        .status()
        .map_err(|e| format!("run {test}: {e}"))?;
    let mut bytes = Vec::new();
    File::open("/run/test.log")
        .map_err(|e| format!("read test log: {e}"))?
        .take(1_048_577)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("read test log: {e}"))?;
    print!("{}", String::from_utf8_lossy(&bytes));
    if bytes.len() > 1_048_576 {
        return Err("test log exceeded 1 MiB".into());
    }
    let output = std::str::from_utf8(&bytes).map_err(|e| format!("decode test log: {e}"))?;
    if !test_passed(status.success(), output) {
        return Err(format!("{test} did not pass exactly one test: {status}"));
    }
    println!("{PASS}");
    Ok(())
}

fn system() -> Result<(), String> {
    if std::process::id() == 1 {
        return Err("system fixture must run beneath the stock supervisor".into());
    }
    let cmdline =
        fs::read_to_string("/proc/cmdline").map_err(|e| format!("read command line: {e}"))?;
    let tokens: Vec<_> = cmdline.split_ascii_whitespace().collect();
    // Exactly one of the image's two guests: the secret store's or the
    // login keys'.
    let (test, pass) = match (system_phase(&cmdline), login_system_phase(&cmdline)) {
        (Ok(_), Err(_)) => (SYSTEM_TEST, SYSTEM_PASS.to_string()),
        (Err(_), Ok(phase)) => (LOGIN_SYSTEM_TEST, format!("{LOGIN_SYSTEM_PASS} {phase}")),
        _ => return Err("system secret fixture was not explicitly selected".into()),
    };
    if !tokens.contains(&"td.hid-fixture=1")
        || fs::read("/case").map_err(|e| format!("read image fixture marker: {e}"))?
            != b"fido-system"
    {
        return Err("system secret fixture was not explicitly selected".into());
    }
    let log = File::create("/run/td-secret-system-test.log")
        .map_err(|e| format!("create system test log: {e}"))?;
    let errors = log
        .try_clone()
        .map_err(|e| format!("clone system test log: {e}"))?;
    let status = Command::new("/bin/td-secret-tests")
        .args([
            "--exact",
            test,
            "--ignored",
            "--test-threads=1",
            "--nocapture",
        ])
        .env_clear()
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(errors)
        .status()
        .map_err(|e| format!("run system secret test: {e}"))?;
    let mut bytes = Vec::new();
    File::open("/run/td-secret-system-test.log")
        .and_then(|file| file.take(1_048_577).read_to_end(&mut bytes))
        .map_err(|e| format!("read system test log: {e}"))?;
    print!("{}", String::from_utf8_lossy(&bytes));
    if bytes.len() > 1_048_576 || !test_passed(status.success(), &String::from_utf8_lossy(&bytes)) {
        return Err(format!(
            "system secret test failed or exceeded its log ceiling: {status}"
        ));
    }
    println!("{pass}");
    Ok(())
}

fn main() -> std::process::ExitCode {
    if std::env::args().skip(1).eq(["--system"]) {
        if let Err(error) = system() {
            eprintln!("{FAIL}: {error}");
        }
        // QEMU's S3 wake resets the q35 chipset, after which the guest's
        // ACPI soft-off no longer powers it down; a reset still ends the
        // login system's boots under -no-reboot.
        let end = match fs::read_to_string("/proc/cmdline") {
            Ok(cmdline) if login_system_phase(&cmdline).is_ok() => "reboot",
            _ => "poweroff",
        };
        match Command::new("/bin/td-svc").arg(end).status() {
            Ok(status) if status.success() => return std::process::ExitCode::SUCCESS,
            result => {
                eprintln!("{FAIL}: system shutdown: {result:?}");
                return std::process::ExitCode::FAILURE;
            }
        }
    }
    if std::process::id() != 1 {
        eprintln!("{FAIL}: fixture must be guest PID 1");
        return std::process::ExitCode::FAILURE;
    }
    if let Err(error) = run() {
        eprintln!("{FAIL}: {error}");
    }
    if let Err(error) = applet(&["poweroff", "-f"]) {
        eprintln!("{FAIL}: {error}");
    }
    loop {
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}
