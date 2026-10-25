//! `qemu-boot-server`: the headless profile's boot proof (td-svc/DESIGN.md
//! §10, "Profiles"). The `server-x86-64` deployment boots through the same
//! selector, verified kexec, loop-mounted EROFS and persistent `@var` as the
//! desktop, with no display attached, and its health transaction
//! completes with the network, OpenSSH and the confinement probes up and
//! nothing graphical started.
//!
//! Two transactional sequences, as `qemu-boot-system` runs them. In the
//! first an installed candidate boots pending, records health and is then
//! current. In the second every attempt the candidate gets ends in health
//! failure, and with no console or greeter on the machine it is healthwatch
//! that reboots each one, until td-boot falls back to the previous
//! deployment unattended. A healthy boot ends only once healthwatch has read
//! the recorded success and left the machine up, so a record the two
//! disagree on fails here rather than reboot-looping a healthy server.
//!
//! The failing boots exercise one of healthwatch's inputs, a health
//! transaction that ended without a record (the fail-target token). A
//! recorded failure, a timeout and a skip over an unmet requirement reach it
//! through the same `after=` decision, which td-svc's own tests hold.

use std::fs;
use std::path::{Path, PathBuf};

use super::host_login::HostLogin;
use super::{
    autotest_wait_token, boot_headless, boot_timeout, build_btrfs_tools, build_system_stem,
    build_td_jail_seccomp_probe, check_persistent_volume, create_persistent_volume_layout,
    find_qemu, provision_selector, require_action_marker, require_distinct_identity,
    require_same_identity, require_selected_deployment, tail, validate_primary_selection, BootDisk,
    BootPlan, BootResult, Devices, RunTrust, VolumeLayout, VolumePurpose, AUTOTEST_CMDLINE_TOKEN,
    BOOT_FAIL_TARGET_CMDLINE_TOKEN, DEPLOY_INSTALL_CMDLINE_TOKEN, GIT_RUNTIME_MARKER,
    GREETER_MARKER, RIPGREP_FD_RUNTIME_MARKER, ROOT_SSH_REFUSED_MARKER, SSHD_MARKER,
    SU_ABSENT_MARKER, SYSTEM_BOOT_SUCCESS_MARKER, SYSTEM_DEPLOY_INSTALL_MARKER,
    SYSTEM_DEPLOY_ROLLBACK_MARKER, SYSTEM_ETC_MUTABLE_MARKER, SYSTEM_ETC_RO_MARKER,
    SYSTEM_GUEST_MEMORY_MIB, SYSTEM_LOGIN_DIRECTORY_MARKER, SYSTEM_ROOT_RO_MARKER,
    SYSTEM_SHUTDOWN_MARKER, SYSTEM_STATE_OWNER_MARKER, SYSTEM_STATE_WRITABLE_MARKER,
    TD_BUSD_RUNTIME_MARKER, TD_COMPOSITOR_KMS_MARKER, TD_FIREFOX_BOOT_MARKER,
    TD_FIRSTBOOT_NEW_MARKER, TD_FIRSTBOOT_STABLE_MARKER, TD_INIT_RUNTIME_MARKER,
    TD_JAIL_KILL_REAPS_MARKER, TD_JAIL_SECCOMP_PROBE_MARKER, TD_JAIL_TRANSITION_MARKER,
    TD_LOGIN_RUNTIME_MARKER, TD_PRINCIPALS_MARKER, TD_SANDBOX_KERNEL_MARKER, TD_TXT_RUNTIME_MARKER,
    TD_UTIL_RUNTIME_MARKER, UUTILS_RUNTIME_MARKER,
};
use crate::check_runner::RecipeCheckRunner;
use td_recipe::td_boot_protocol;

pub(crate) const STEM: &str = "server-x86-64";

const HEALTH_REBOOT_MARKER: &str = td_recipe::shared::SERVER_HEALTH_REBOOT_MARKER;
const HEALTH_STEADY_MARKER: &str = td_recipe::shared::SERVER_HEALTH_STEADY_MARKER;

/// The provisioned primary's name: not the image's default, so the boot
/// shows the saved name was taken.
const ACCOUNT: &str = "ada";

/// No display: nothing on a server draws. The direct-boot PC machine keeps
/// its built-in i8042 keyboard controller, which only q35 can leave out.
const HEADLESS: Devices = Devices {
    display: false,
    keyboard: true,
};

/// Whether a boot's identity is freshly provisioned or the one an earlier
/// boot of the same volume minted.
#[derive(Clone, Copy, PartialEq)]
enum Identity {
    Fresh,
    Reused,
}

struct Boot<'a> {
    qemu: &'a str,
    bzimage: &'a Path,
    initramfs: &'a Path,
    volume: &'a Path,
    scratch: &'a Path,
}

impl Boot<'_> {
    /// One boot past the health acknowledgement to healthwatch's steady
    /// state, which ends it.
    fn healthy(&self, tokens: &str, label: &str) -> Result<BootResult, String> {
        self.run(tokens, HEALTH_STEADY_MARKER, true, None, label)
    }

    /// `healthy`, then the host's SSH logins before the boot ends.
    fn healthy_with_login(
        &self,
        tokens: &str,
        login: &HostLogin,
        label: &str,
    ) -> Result<BootResult, String> {
        self.run(tokens, HEALTH_STEADY_MARKER, true, Some(login), label)
    }

    /// One boot that must end itself: health fails, healthwatch reboots,
    /// and QEMU (`-no-reboot`) exits.
    fn failing(&self, tokens: &str, label: &str) -> Result<BootResult, String> {
        self.run(tokens, HEALTH_REBOOT_MARKER, false, None, label)
    }

    fn run(
        &self,
        tokens: &str,
        target: &str,
        kill_on_marker: bool,
        host_login: Option<&HostLogin>,
        label: &str,
    ) -> Result<BootResult, String> {
        let result = boot_headless(
            self.qemu,
            self.bzimage,
            self.initramfs,
            BootPlan {
                disk: Some(BootDisk::new(self.volume, false)),
                mem: SYSTEM_GUEST_MEMORY_MIB,
                target_marker: target,
                kill_on_marker,
                extra_append: tokens,
                user_net: true,
                audio: false,
                physical_input: false,
                capture_firefox_audio: false,
                tpm_socket: None,
                side_channel: None,
                answers: None,
                cut: false,
                keep_console: None,
                screens: None,
                devices: HEADLESS,
                screen: None,
                shell: None,
                host_login,
            },
            self.scratch,
        )?;
        println!(
            "   [qemu-boot-server] {label} elapsed: {:.2}s",
            result.elapsed.as_secs_f64()
        );
        if result.evidence.kernel_panic {
            return Err(format!(
                "the server's {label} kernel panicked. Last serial output:\n{}",
                tail(&result.console, 80)
            ));
        }
        Ok(result)
    }
}

pub(crate) fn run(runner: &RecipeCheckRunner) -> Result<(), String> {
    let qemu = find_qemu()?;
    let (bzimage, selector, deployment) = build_system_stem(runner, STEM)?;
    let (mkfs, btrfs) = build_btrfs_tools(runner)?;
    let seccomp_probe = build_td_jail_seccomp_probe(runner)?;
    // The provisioned files must be guest root's, as an image's are.
    let builder = PathBuf::from(runner.builder_command().get_program());
    let trust = RunTrust::generate_root_owned(builder)?;
    let initramfs = provision_selector(&selector, runner.scratch_dir(), &trust)?;
    let volume = runner.scratch_dir().join("server-volume.btrfs");
    let client = runner.scratch_dir().join("server-ssh-client");
    match fs::remove_dir_all(&client) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            return Err(format!("remove stale {}: {error}", client.display()));
        }
        _ => {}
    }
    let login = HostLogin::prepare(&client, STEM, ACCOUNT)?;
    let fresh_volume = || {
        create_persistent_volume_layout(
            &deployment,
            &mkfs,
            &btrfs,
            &volume,
            VolumeLayout::Transactional,
            true,
            &trust,
            Some(&seccomp_probe),
            VolumePurpose::Fixture,
            None,
            Some(login.account()),
        )
    };
    let fixture = fresh_volume()?;
    if fixture.initial_id == fixture.alternate_id {
        return Err("transaction fixture candidate did not change the deployment id".into());
    }
    println!(
        "   [qemu-boot-server] {qemu} boots the headless deployment with no display: selector -> verified kexec -> loop-mounted root.erofs + persistent @var -> td-svc system units, OpenSSH and the health transaction\n              kernel:        {}\n              initramfs:     {}\n              Btrfs volume:  {}\n              initial:       {}\n              candidate:     {}",
        bzimage.display(),
        initramfs.display(),
        volume.display(),
        fixture.initial_id,
        fixture.alternate_id
    );
    let boot = Boot {
        qemu: &qemu,
        bzimage: &bzimage,
        initramfs: &initramfs,
        volume: &volume,
        scratch: runner.scratch_dir(),
    };
    // The autotest token arms the root-login refusal fixture the volume
    // carries; the wait token bounds the health loop by this host's timeout.
    let wait_token = autotest_wait_token(boot_timeout());
    let healthy_tokens = format!("{AUTOTEST_CMDLINE_TOKEN} {wait_token}");
    let install_tokens = format!("{healthy_tokens} {DEPLOY_INSTALL_CMDLINE_TOKEN}");

    // The healthy sequence: install, then a pending candidate that records
    // health, then the same deployment acknowledged.
    let install = boot.healthy(&install_tokens, "install boot")?;
    validate_healthy(&install, Identity::Fresh, "install boot")?;
    validate_install(&install, &fixture.initial_id, "install boot")?;
    check_persistent_volume(&btrfs, &volume)?;

    let pending = boot.healthy(&healthy_tokens, "pending candidate boot")?;
    validate_healthy(&pending, Identity::Reused, "pending candidate boot")?;
    require_selected_deployment(
        &pending,
        td_boot_protocol::SELECTED_CURRENT_MARKER,
        &fixture.alternate_id,
        "pending candidate boot",
    )?;
    require_action_marker(
        &pending,
        pending.evidence.attempt_consumed,
        td_boot_protocol::ATTEMPT_CONSUMED_MARKER,
        "pending candidate",
    )?;
    require_same_identity(&install, &pending, "install", "pending candidate")?;
    check_persistent_volume(&btrfs, &volume)?;

    // The provisioned account is the primary, and the host logs in as it
    // with the provisioned key and is refused with any other.
    let acknowledged = boot.healthy_with_login(&healthy_tokens, &login, "acknowledged boot")?;
    validate_healthy(&acknowledged, Identity::Reused, "acknowledged boot")?;
    login.verdict(acknowledged.evidence.host_key.as_deref())?;
    // The run's private keys are kept only for a failure's diagnosis.
    fs::remove_dir_all(&client).map_err(|error| format!("remove {}: {error}", client.display()))?;
    require_selected_deployment(
        &acknowledged,
        td_boot_protocol::SELECTED_CURRENT_MARKER,
        &fixture.alternate_id,
        "acknowledged boot",
    )?;
    no_attempt_state(&acknowledged, "acknowledged boot")?;
    require_same_identity(&install, &acknowledged, "install", "acknowledged")?;
    check_persistent_volume(&btrfs, &volume)?;

    // The failure sequence, on a new machine: every attempt fails health
    // and reboots itself, and the boot after the last selects previous.
    let failure_fixture = fresh_volume()?;
    if failure_fixture.initial_id != fixture.initial_id
        || failure_fixture.alternate_id != fixture.alternate_id
    {
        return Err("recreated transaction fixture changed deployment ids".into());
    }
    let failure_install = boot.healthy(&install_tokens, "failure-sequence install boot")?;
    validate_healthy(
        &failure_install,
        Identity::Fresh,
        "failure-sequence install boot",
    )?;
    validate_install(
        &failure_install,
        &fixture.initial_id,
        "failure-sequence install boot",
    )?;
    require_distinct_identity(&install, &failure_install, "install", "failure install")?;
    check_persistent_volume(&btrfs, &volume)?;

    // The autotest token keeps the profiler's first capture as prompt as on the
    // healthy boots; without it an idle candidate waits out profiler evidence.
    let failing_tokens = format!("{healthy_tokens} {BOOT_FAIL_TARGET_CMDLINE_TOKEN}");
    for attempt in 1..=td_boot_protocol::DEFAULT_BOOT_ATTEMPTS {
        let label = format!("failing candidate attempt {attempt}");
        let failed = boot.failing(&failing_tokens, &label)?;
        validate_health_reboot(&failed, &label)?;
        require_selected_deployment(
            &failed,
            td_boot_protocol::SELECTED_CURRENT_MARKER,
            &fixture.alternate_id,
            &label,
        )?;
        require_action_marker(
            &failed,
            failed.evidence.attempt_consumed,
            td_boot_protocol::ATTEMPT_CONSUMED_MARKER,
            &label,
        )?;
        if failed.evidence.attempts_exhausted {
            return Err(format!(
                "the {label} exhausted the candidate before every attempt ran. \
                 Last serial output:\n{}",
                tail(&failed.console, 80)
            ));
        }
        check_persistent_volume(&btrfs, &volume)?;
    }

    let rollback = boot.healthy(&healthy_tokens, "automatic rollback boot")?;
    validate_health(&rollback, Identity::Reused, "automatic rollback boot")?;
    if !rollback.evidence.selected_previous
        || rollback.evidence.selected_current
        || !rollback.evidence.attempts_exhausted
    {
        return Err(format!(
            "the boot after every failed attempt did not fall back to previous with the \
             candidate's attempts exhausted. Last serial output:\n{}",
            tail(&rollback.console, 80)
        ));
    }
    require_selected_deployment(
        &rollback,
        td_boot_protocol::SELECTED_PREVIOUS_MARKER,
        &fixture.initial_id,
        "automatic rollback boot",
    )?;
    require_same_identity(&failure_install, &rollback, "failure install", "rollback")?;
    check_persistent_volume(&btrfs, &volume)?;

    println!(
        "PASS: server-x86-64 boots headless under qemu - an installed candidate boots pending, \
         records health ({SYSTEM_BOOT_SUCCESS_MARKER}, then {HEALTH_STEADY_MARKER}) with the \
         same host identity and is then \
         current, where the host logs in over SSH as the provisioned account {ACCOUNT:?} with \
         its provisioned key, on the host key the console reported, and is refused as root \
         and with another key; one whose every attempt fails health is rebooted by healthwatch \
         ({HEALTH_REBOOT_MARKER}, {SYSTEM_SHUTDOWN_MARKER}) until td-boot falls back to the \
         previous deployment unattended. Every healthy boot reports the read-only root and \
         /etc, writable state, OpenSSH loopback ({SSHD_MARKER}) and root's refusal \
         ({ROOT_SSH_REFUSED_MARKER}), the userland and confinement probes, and no greeter, \
         compositor, session bus or application"
    );
    Ok(())
}

/// A boot that installed the channel's candidate and rolled the trial
/// back, all from the initial deployment, with no attempt state.
fn validate_install(result: &BootResult, initial: &str, label: &str) -> Result<(), String> {
    require_selected_deployment(
        result,
        td_boot_protocol::SELECTED_CURRENT_MARKER,
        initial,
        label,
    )?;
    require_action_marker(
        result,
        result.evidence.deploy_install,
        SYSTEM_DEPLOY_INSTALL_MARKER,
        label,
    )?;
    require_action_marker(
        result,
        result.evidence.deploy_rollback,
        SYSTEM_DEPLOY_ROLLBACK_MARKER,
        label,
    )?;
    no_attempt_state(result, label)
}

fn no_attempt_state(result: &BootResult, label: &str) -> Result<(), String> {
    if result.evidence.attempt_consumed || result.evidence.attempts_exhausted {
        return Err(format!(
            "the {label} consumed or exhausted a boot-attempt budget. Last serial output:\n{}",
            tail(&result.console, 80)
        ));
    }
    Ok(())
}

/// `validate_health` for a boot that selected current unambiguously.
fn validate_healthy(result: &BootResult, identity: Identity, label: &str) -> Result<(), String> {
    validate_primary_selection(result, label)?;
    validate_health(result, identity, label)
}

/// What a healthy server boot printed, and what it must not have.
fn validate_health(result: &BootResult, identity: Identity, label: &str) -> Result<(), String> {
    let console = || tail(&result.console, 80);
    let rebooted = result
        .console
        .lines()
        .any(|line| line.trim_end() == HEALTH_REBOOT_MARKER);
    if !result.evidence.boot_success || !result.evidence.target || !result.marker_killed || rebooted
    {
        return Err(format!(
            "the server's {label} did not record deployment health \
             {SYSTEM_BOOT_SUCCESS_MARKER:?} and then reach healthwatch's steady state \
             {HEALTH_STEADY_MARKER:?} without {HEALTH_REBOOT_MARKER:?} - {}. \
             Last serial output:\n{}",
            result.reason,
            console()
        ));
    }
    let evidence = &result.evidence;
    let (firstboot, firstboot_marker) = match identity {
        Identity::Fresh => (evidence.firstboot_new, TD_FIRSTBOOT_NEW_MARKER),
        Identity::Reused => (evidence.firstboot_stable, TD_FIRSTBOOT_STABLE_MARKER),
    };
    let required = [
        (evidence.root_read_only, SYSTEM_ROOT_RO_MARKER),
        (evidence.etc_read_only, SYSTEM_ETC_RO_MARKER),
        (evidence.etc_mutable, SYSTEM_ETC_MUTABLE_MARKER),
        (evidence.state_writable, SYSTEM_STATE_WRITABLE_MARKER),
        (evidence.state_owner, SYSTEM_STATE_OWNER_MARKER),
        (evidence.login_directory, SYSTEM_LOGIN_DIRECTORY_MARKER),
        (firstboot, firstboot_marker),
        (evidence.uutils_runtime, UUTILS_RUNTIME_MARKER),
        (evidence.ripgrep_fd_runtime, RIPGREP_FD_RUNTIME_MARKER),
        (evidence.git_runtime, GIT_RUNTIME_MARKER),
        (evidence.sshd, SSHD_MARKER),
        (evidence.root_ssh_refused, ROOT_SSH_REFUSED_MARKER),
        (evidence.su_absent, SU_ABSENT_MARKER),
        (evidence.td_util_runtime, TD_UTIL_RUNTIME_MARKER),
        (evidence.td_txt_runtime, TD_TXT_RUNTIME_MARKER),
        (evidence.td_init_runtime, TD_INIT_RUNTIME_MARKER),
        (evidence.td_login_runtime, TD_LOGIN_RUNTIME_MARKER),
        (evidence.td_sandbox_kernel, TD_SANDBOX_KERNEL_MARKER),
        (evidence.td_jail_transition, TD_JAIL_TRANSITION_MARKER),
        (evidence.td_jail_seccomp, TD_JAIL_SECCOMP_PROBE_MARKER),
        (evidence.td_jail_kill_reaps, TD_JAIL_KILL_REAPS_MARKER),
    ];
    let missing: Vec<&str> = required
        .iter()
        .filter(|(seen, _)| !seen)
        .map(|(_, marker)| *marker)
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            "the server's {label} recorded health without {missing:?}. Last serial output:\n{}",
            console()
        ));
    }
    if evidence.host_key.is_none() {
        return Err(format!(
            "the server's {label} reported no per-machine host key. Last serial output:\n{}",
            console()
        ));
    }
    no_desktop(result, label)
}

/// A boot that failed health must have been rebooted by healthwatch: its
/// marker, td-svc's teardown through `/etc/shutdown`, and QEMU's own exit.
fn validate_health_reboot(result: &BootResult, label: &str) -> Result<(), String> {
    let printed = |marker: &str| result.console.lines().any(|line| line.trim_end() == marker);
    if !result.evidence.target
        || !result.exited_clean
        || !printed(SYSTEM_SHUTDOWN_MARKER)
        || printed(SYSTEM_BOOT_SUCCESS_MARKER)
    {
        return Err(format!(
            "the server's {label} was not rebooted by healthwatch after failing health: \
             {HEALTH_REBOOT_MARKER:?} and {SYSTEM_SHUTDOWN_MARKER:?} must be printed and QEMU \
             must exit by itself, without {SYSTEM_BOOT_SUCCESS_MARKER:?} - {}. \
             Last serial output:\n{}",
            result.reason,
            tail(&result.console, 80)
        ));
    }
    no_desktop(result, label)
}

fn no_desktop(result: &BootResult, label: &str) -> Result<(), String> {
    let evidence = &result.evidence;
    let desktop = [
        (evidence.greeter, GREETER_MARKER),
        (
            evidence.td_compositor_kms.is_some(),
            TD_COMPOSITOR_KMS_MARKER,
        ),
        (evidence.td_busd_runtime, TD_BUSD_RUNTIME_MARKER),
        (evidence.td_firefox, TD_FIREFOX_BOOT_MARKER),
        (evidence.principals_enrolled, TD_PRINCIPALS_MARKER),
    ];
    let present: Vec<&str> = desktop
        .iter()
        .filter(|(seen, _)| *seen)
        .map(|(_, marker)| *marker)
        .collect();
    if !present.is_empty() {
        return Err(format!(
            "the server's {label} printed desktop evidence {present:?}: something graphical, \
             a session or a console login started. Last serial output:\n{}",
            tail(&result.console, 80)
        ));
    }
    Ok(())
}
