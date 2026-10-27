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
//! Then the published form: the deployment composed by `server-image` into
//! a disk image for a provisioned account, cold-booted by UEFI firmware from
//! its ESP with no command line injected, to the steady state and the
//! host's SSH logins. That boot carries a NoCloud seed, whose hostname it
//! takes and whose key admits the provisioned account. The acknowledged
//! boot instead reads a mock EC2 IMDSv2 its DMI names, with the same
//! effect, and the pending candidate's boot a mock Azure IMDS, then reports
//! ready to a mock wireserver. Every other boot has none, and the metadata
//! key is refused.
//!
//! The failing boots exercise one of healthwatch's inputs, a health
//! transaction that ended without a record (the fail-target token). A
//! recorded failure, a timeout and a skip over an unmet requirement reach it
//! through the same `after=` decision, which td-svc's own tests hold.

use std::fs;
use std::path::{Path, PathBuf};

use super::cloud_mock::{self, CloudMock};
use super::host_login::HostLogin;
use super::nocloud;
use super::server_image::{self, Format, ImageInputs};
use super::{
    autotest_wait_token, boot_headless, boot_source, boot_timeout, build_btrfs_tools,
    build_system_stem, build_td_jail_seccomp_probe, check_persistent_volume,
    create_persistent_volume_layout, efi, find_qemu, provision_selector, require_action_marker,
    require_distinct_identity, require_same_identity, require_selected_deployment, tail,
    validate_primary_selection, BootDisk, BootPlan, BootResult, BootSource, Devices,
    FirmwareAttachment, RunTrust, VolumeLayout, VolumePurpose, AUTOTEST_CMDLINE_TOKEN,
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
/// Each published form's own account, so its boot shows the name it
/// carries was taken rather than an earlier volume's.
const IMAGE_ACCOUNT: &str = "grace";
const ISO_ACCOUNT: &str = "hedy";
/// The hostname the disk image's seed names, not the image's default.
const SEED_HOSTNAME: &str = "td-seeded";
const METADATA_NONE_MARKER: &str = "TD-METADATA-NONE";
const CLOUD_METADATA_NONE_MARKER: &str = "TD-CLOUD-METADATA-NONE";
/// The one public key the mock Azure IMDS offers; no login uses it.
const AZURE_KEY: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIGmTjqOMdHoPzoqbE+wEE5Q4aNmEGC0tgJbQOkxqdgUi td-azure";
/// A boot with no seed and no cloud.
const UNSEEDED: &[&str] = &[METADATA_NONE_MARKER, CLOUD_METADATA_NONE_MARKER];
/// A live boot's volume is half of RAM, as `qemu-boot-live` gives it.
const LIVE_MEMORY_MIB: &str = "4096";

/// The two forms `server-image` publishes.
#[derive(Clone, Copy, PartialEq)]
enum Published {
    Disk,
    Iso,
}

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
        self.run(tokens, HEALTH_STEADY_MARKER, true, None, None, label)
    }

    /// `healthy` on a cloud `mock`.
    fn healthy_on(
        &self,
        tokens: &str,
        mock: &CloudMock,
        label: &str,
    ) -> Result<BootResult, String> {
        self.run(tokens, HEALTH_STEADY_MARKER, true, None, Some(mock), label)
    }

    /// `healthy` on a cloud `mock`, then the host's SSH logins before the
    /// boot ends.
    fn healthy_with_login(
        &self,
        tokens: &str,
        login: &HostLogin,
        mock: &CloudMock,
        label: &str,
    ) -> Result<BootResult, String> {
        self.run(
            tokens,
            HEALTH_STEADY_MARKER,
            true,
            Some(login),
            Some(mock),
            label,
        )
    }

    /// One boot that must end itself: health fails, healthwatch reboots,
    /// and QEMU (`-no-reboot`) exits.
    fn failing(&self, tokens: &str, label: &str) -> Result<BootResult, String> {
        self.run(tokens, HEALTH_REBOOT_MARKER, false, None, None, label)
    }

    fn run(
        &self,
        tokens: &str,
        target: &str,
        kill_on_marker: bool,
        host_login: Option<&HostLogin>,
        cloud: Option<&CloudMock>,
        label: &str,
    ) -> Result<BootResult, String> {
        let result = boot_headless(
            self.qemu,
            self.bzimage,
            self.initramfs,
            BootPlan {
                disk: Some(BootDisk::new(self.volume, false)),
                seed: None,
                cloud,
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
    // Resolved first: the image boot that needs it runs last.
    let firmware = efi::firmware(&qemu)?;
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
    validate_healthy(&install, Identity::Fresh, "install boot", UNSEEDED)?;
    validate_install(&install, &fixture.initial_id, "install boot")?;
    check_persistent_volume(&btrfs, &volume)?;

    // On Azure: named and keyed by its IMDS, and reported ready.
    let azure_dir = fresh_dir(&runner.scratch_dir().join("server-azure"))?;
    let azure = CloudMock::azure(&azure_dir, AZURE_KEY)?;
    let pending = boot.healthy_on(&healthy_tokens, &azure, "pending candidate boot")?;
    let applied = format!(
        "TD-CLOUD-METADATA-APPLIED azure hostname={} keys=1",
        cloud_mock::AZURE_HOSTNAME
    );
    let ready = format!(
        "TD-CLOUD-METADATA-READY azure incarnation={}",
        cloud_mock::AZURE_INCARNATION
    );
    validate_healthy(
        &pending,
        Identity::Reused,
        "pending candidate boot",
        &[METADATA_NONE_MARKER, &applied, &ready],
    )?;
    let requests = azure.requests()?;
    if requests != cloud_mock::EXPECTED_AZURE {
        return Err(format!(
            "the pending candidate boot should have asked the mock Azure IMDS and \
             wireserver {:?}, but asked {requests:?}",
            cloud_mock::EXPECTED_AZURE
        ));
    }
    fs::remove_dir_all(&azure_dir)
        .map_err(|error| format!("remove {}: {error}", azure_dir.display()))?;
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
    // with the provisioned key, and with the key EC2's metadata offers, and
    // is refused with any other.
    let imds = fresh_dir(&runner.scratch_dir().join("server-imds"))?;
    let mock = CloudMock::ec2(&imds, &login.seed_key()?)?;
    let acknowledged =
        boot.healthy_with_login(&healthy_tokens, &login, &mock, "acknowledged boot")?;
    let applied = format!(
        "TD-CLOUD-METADATA-APPLIED ec2 hostname={} keys=1",
        cloud_mock::HOSTNAME_LABEL
    );
    validate_healthy(
        &acknowledged,
        Identity::Reused,
        "acknowledged boot",
        &[METADATA_NONE_MARKER, &applied],
    )?;
    let requests = mock.requests()?;
    if requests != cloud_mock::EXPECTED {
        return Err(format!(
            "the acknowledged boot should have asked the mock IMDS for a token and then \
             {:?} with it, but asked {requests:?}",
            cloud_mock::EXPECTED
        ));
    }
    fs::remove_dir_all(&imds).map_err(|error| format!("remove {}: {error}", imds.display()))?;
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
        UNSEEDED,
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

    let inputs = ImageInputs {
        kernel: bzimage,
        selector,
        deployment,
        mkfs,
        btrfs,
    };
    image_boot(runner, &qemu, &firmware, &inputs, Published::Disk)?;
    image_boot(runner, &qemu, &firmware, &inputs, Published::Iso)?;

    println!(
        "PASS: server-x86-64 boots headless under qemu - an installed candidate boots pending, \
         records health ({SYSTEM_BOOT_SUCCESS_MARKER}, then {HEALTH_STEADY_MARKER}) with the \
         same host identity and is then \
         current, where the host logs in over SSH as the provisioned account {ACCOUNT:?} with \
         its provisioned key and with the key a mock EC2 IMDSv2 offers, which also names \
         the host, as a mock Azure IMDS did on the pending boot before it reported ready \
         to a mock wireserver, on the host key the console reported, and is refused as root \
         and with another key; one whose every attempt fails health is rebooted by healthwatch \
         ({HEALTH_REBOOT_MARKER}, {SYSTEM_SHUTDOWN_MARKER}) until td-boot falls back to the \
         previous deployment unattended; and the disk image and live medium server-image \
         composes, each for its own account ({IMAGE_ACCOUNT:?}, {ISO_ACCOUNT:?}), cold-boot \
         under UEFI firmware to the same steady state and logins, the disk image named and \
         keyed by its NoCloud seed. Every healthy boot reports the read-only root and \
         /etc, writable state, OpenSSH loopback ({SSHD_MARKER}) and root's refusal \
         ({ROOT_SSH_REFUSED_MARKER}), the userland and confinement probes, and no greeter, \
         compositor, session bus or application"
    );
    Ok(())
}

/// Compose a published form for a provisioned account of its own and
/// cold-boot it under firmware, with no command line from the host. A disk's
/// ESP kernel and selector find the volume by the identity the image bound;
/// a live medium's selector authenticates the medium and its seed stages the
/// signed provision into the RAM volume. Either way the account logs in over
/// SSH.
fn image_boot(
    runner: &RecipeCheckRunner,
    qemu: &str,
    (code, vars_template): &(PathBuf, PathBuf),
    inputs: &ImageInputs,
    form: Published,
) -> Result<(), String> {
    let (account, name, label, attachment, mem) = match form {
        Published::Disk => (
            IMAGE_ACCOUNT,
            "server.img",
            "image firmware boot",
            // As EC2 Nitro presents an EBS root volume; the installed
            // candidate's boots above use virtio.
            FirmwareAttachment::Nvme,
            SYSTEM_GUEST_MEMORY_MIB,
        ),
        Published::Iso => (
            ISO_ACCOUNT,
            "server.iso",
            "live medium firmware boot",
            FirmwareAttachment::Optical,
            LIVE_MEMORY_MIB,
        ),
    };
    let dir = runner.scratch_dir().join("server-image");
    match fs::remove_dir_all(&dir) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            return Err(format!("remove stale {}: {error}", dir.display()));
        }
        _ => {}
    }
    fs::create_dir(&dir).map_err(|error| format!("create {}: {error}", dir.display()))?;
    let client = dir.join("ssh-client");
    let login = HostLogin::prepare(&client, STEM, account)?;
    let trust = server_image::root_owned_trust(runner)?;
    let disk = dir.join(name);
    let id = match form {
        Published::Disk => server_image::compose(
            inputs,
            &trust,
            login.account(),
            server_image::DEFAULT_DISK_GIB * 1024 * 1024 * 1024,
            Format::Raw,
            &dir,
            &disk,
        )?,
        Published::Iso => server_image::compose_iso(inputs, &trust, login.account(), &dir, &disk)?,
    };
    // As `cloud-localds` writes one: the hostname in meta-data, keys in
    // user-data under a user whose name the baked account overrides, and
    // a key of a type the server does not admit, which is skipped.
    let seed = match form {
        Published::Disk => {
            let path = dir.join("seed.iso");
            let user = format!(
                "#cloud-config\n\
                 users:\n  \
                 - name: someone-else\n    \
                 ssh_authorized_keys:\n      \
                 - {}\n      \
                 - ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAAAgQ== not-ed25519\n",
                login.seed_key()?
            );
            nocloud::write_seed(
                &path,
                &format!("instance-id: td-qemu-boot-server\nlocal-hostname: {SEED_HOSTNAME}\n"),
                &user,
            )?;
            Some(path)
        }
        Published::Iso => None,
    };
    let vars = dir.join("vars.fd");
    efi::copy_input(vars_template, &vars)?;
    println!(
        "   [qemu-boot-server] firmware cold-boots the composed {} {} (deployment {id})",
        if form == Published::Iso {
            "live medium"
        } else {
            "image"
        },
        disk.display()
    );
    let result = boot_source(
        qemu,
        BootSource::Firmware {
            code,
            vars: &vars,
            attachment,
            installation_target: None,
        },
        BootPlan {
            // The live medium is read-only optical media and its boot writes
            // to RAM; the disk image takes its own boot's writes.
            disk: Some(BootDisk::new(&disk, form == Published::Iso)),
            seed: seed.as_deref(),
            cloud: None,
            mem,
            target_marker: HEALTH_STEADY_MARKER,
            kill_on_marker: true,
            extra_append: "",
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
            host_login: Some(&login),
        },
        &dir,
        boot_timeout(),
    )?;
    println!(
        "   [qemu-boot-server] {label} elapsed: {:.2}s",
        result.elapsed.as_secs_f64()
    );
    match form {
        Published::Disk => {
            // The volume partition of the NVMe namespace, not a virtio disk.
            let on_nvme = result.console.lines().any(|line| {
                line.starts_with("TD-BOOT-VOLUME ") && line.trim_end().ends_with(" /dev/nvme0n1p2")
            });
            if !on_nvme {
                return Err(format!(
                    "the {label} did not find its volume on the NVMe disk. Last serial \
                     output:\n{}",
                    tail(&result.console, 80)
                ));
            }
            validate_primary_selection(&result, label)?;
            require_selected_deployment(
                &result,
                td_boot_protocol::SELECTED_CURRENT_MARKER,
                &id,
                label,
            )?;
        }
        // A live selector selects no slot: it boots the medium's deployment
        // onto a volume it formats in RAM, which the boot then finds.
        Published::Iso => {
            let on_ram = result.console.lines().any(|line| {
                line.starts_with("TD-BOOT-VOLUME ") && line.trim_end().ends_with(" /dev/ram0")
            });
            let named = format!("{}{id} ", super::encrypted_boot::LIVE_DEPLOYMENT_PREFIX);
            let booted = result.console.lines().any(|line| line.starts_with(&named));
            if !on_ram || !booted {
                return Err(format!(
                    "the {label} did not boot deployment {id} from its RAM volume. Last \
                     serial output:\n{}",
                    tail(&result.console, 80)
                ));
            }
        }
    }
    let evidence = &result.evidence;
    let rebooted = result
        .console
        .lines()
        .any(|line| line.trim_end() == HEALTH_REBOOT_MARKER);
    if evidence.kernel_panic
        || !evidence.boot_success
        || !evidence.target
        || !result.marker_killed
        || rebooted
        || !evidence.firstboot_new
        || !evidence.sshd
        || evidence.host_key.is_none()
    {
        return Err(format!(
            "the {label} did not mint a new identity, start OpenSSH, record health \
             {SYSTEM_BOOT_SUCCESS_MARKER:?} and reach {HEALTH_STEADY_MARKER:?} - {}. \
             Last serial output:\n{}",
            result.reason,
            tail(&result.console, 80)
        ));
    }
    no_attempt_state(&result, label)?;
    no_desktop(&result, label)?;
    match form {
        Published::Disk => validate_seeded(&result, label)?,
        Published::Iso => validate_metadata(&result, label, UNSEEDED)?,
    }
    login.verdict(evidence.host_key.as_deref())?;
    fs::remove_dir_all(&dir).map_err(|error| format!("remove {}: {error}", dir.display()))
}

/// The seed, the only drive on q35's AHCI, was read once, its hostname
/// taken and applied, one key written and the RSA key skipped, and the
/// cloud sources stood aside.
fn validate_seeded(result: &BootResult, label: &str) -> Result<(), String> {
    let expected = [
        "TD-METADATA-SKIPPED 1 key(s) other than plain ssh-ed25519".to_string(),
        format!("TD-METADATA-APPLIED nocloud /dev/sr0 hostname={SEED_HOSTNAME} keys=1"),
        format!("TD-HOSTNAME-READY {SEED_HOSTNAME}"),
        "TD-CLOUD-METADATA-SKIPPED nocloud".to_string(),
    ];
    let metadata: Vec<&str> = result
        .console
        .lines()
        .map(str::trim_end)
        .filter(|line| {
            line.starts_with("TD-METADATA-")
                || line.starts_with("TD-CLOUD-METADATA-")
                || line.starts_with("TD-HOSTNAME-READY ")
        })
        .collect();
    if metadata != expected {
        return Err(format!(
            "the {label} should have reported {expected:?} for its NoCloud seed, but \
             reported {metadata:?}. Last serial output:\n{}",
            tail(&result.console, 80)
        ));
    }
    Ok(())
}

/// `path`, with no stale tree from an earlier run under it.
fn fresh_dir(path: &Path) -> Result<PathBuf, String> {
    match fs::remove_dir_all(path) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            Err(format!("remove stale {}: {error}", path.display()))
        }
        _ => Ok(path.to_owned()),
    }
}

/// What the two metadata units reported, in order: `expected`.
fn validate_metadata(result: &BootResult, label: &str, expected: &[&str]) -> Result<(), String> {
    let metadata: Vec<&str> = result
        .console
        .lines()
        .map(str::trim_end)
        .filter(|line| line.starts_with("TD-METADATA-") || line.starts_with("TD-CLOUD-METADATA-"))
        .collect();
    if metadata != expected {
        return Err(format!(
            "the {label} should have reported {expected:?} from its metadata units, but \
             reported {metadata:?}. Last serial output:\n{}",
            tail(&result.console, 80)
        ));
    }
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

/// `validate_health` for a boot that selected current unambiguously, and
/// reported `metadata`.
fn validate_healthy(
    result: &BootResult,
    identity: Identity,
    label: &str,
    metadata: &[&str],
) -> Result<(), String> {
    validate_primary_selection(result, label)?;
    validate_health(result, identity, label)?;
    validate_metadata(result, label, metadata)
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
