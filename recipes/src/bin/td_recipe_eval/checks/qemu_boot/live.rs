//! `qemu-boot-live`: the production live profile boots from its own medium
//! into the graphical session, opens the installer wizard, and a person's
//! keys carry it through the destination and settings pages to the
//! installation service's review and back (td-install/INSTALLER.md
//! increment 6).
//!
//! The medium is `build-iso`'s, composed by the same function from the same
//! verified store deployment and signed with a key made for this run. The
//! boot is direct, kernel and live selector from that medium, so the
//! autotest and wizard-evidence tokens can be appended; the ISO is attached
//! read-only as a virtio disk, which the selector finds, mounts and
//! authenticates, and an empty sparse disk follows it as the only eligible
//! destination. No automated check boots this medium through firmware or
//! optical media; `./test-iso` does so by hand.
//!
//! Once the live session says the wizard is ready, every key is pressed
//! through QEMU's emulated keyboard only after td-setup says it showed the
//! state that key is for, so a key cannot land on a page or field it was
//! not meant for. The run ends once td-setup says the service released the
//! review it left; QEMU's own count of writes, discards and zone appends
//! on the target must then be zero, and the target must still have no
//! allocated block.
use super::build_iso::{live_medium, LiveMedium};
use super::install::{system_target_capacity, TargetDisk};
use super::setup_input::{typed, SetupStep};
use super::*;
use std::os::unix::fs::MetadataExt;

const TD_SETUP_LIVE_MARKER: &str = td_recipe::ladder::TD_SETUP_LIVE_MARKER;
/// The live volume is half of RAM and the stock session runs on the rest.
const LIVE_MEMORY_MIB: &str = "4096";
/// The destination the wizard should list: the disk after the medium.
const TARGET_KERNEL_NAME: &str = "vdb";
const USERNAME: &str = "dana";
const HOSTNAME: &str = "td-wizard";
/// Typed into the time zone row, which seeks the first zone it begins.
const ZONE_SEEK: &str = "asia/tok";
const ZONE: &str = "Asia/Tokyo";
/// What the settings page starts at: the catalog's UTC.
const DEFAULT_ZONE: &str = "Etc/UTC";

pub(crate) fn run(runner: &RecipeCheckRunner) -> Result<(), String> {
    let qemu = find_qemu()?;
    let (kernel, selector, deployment) = build_system(runner)?;
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let scratch = Scratch {
        dir: create_scratch_dir(runner.scratch_dir(), &SEQ)?,
    };
    let trust = RunTrust::generate()?;
    let LiveMedium {
        id,
        selector: live,
        payloads,
    } = live_medium(&selector, &deployment, &scratch.dir, &trust)?;
    let iso = scratch.dir.join("live.iso");
    media::write_image_with_payloads(&iso, &kernel, &live, &payloads)?;
    let mut payload_bytes = 0u64;
    for (_, path) in &payloads {
        let bytes = fs::metadata(path)
            .map_err(|error| format!("stat {}: {error}", path.display()))?
            .len();
        payload_bytes = payload_bytes
            .checked_add(bytes)
            .ok_or("live payload length overflow")?;
    }
    let target_name = "target.raw";
    let target = TargetDisk::with_capacity(
        &scratch.dir,
        target_name,
        system_target_capacity(payload_bytes)?,
    )?;
    let script = script()?;
    let timeout = boot_timeout();
    let tokens = format!(
        "{AUTOTEST_CMDLINE_TOKEN} {} {}",
        autotest_wait_token(timeout),
        td_recipe::ladder::SETUP_INPUT_CMDLINE_TOKEN
    );
    println!("   [qemu-boot-live] booting deployment {id} live from its medium");
    let result = boot_source(
        &qemu,
        BootSource::LiveSetup {
            kernel: &kernel,
            initramfs: &live,
            target: &target,
            script: &script,
        },
        BootPlan {
            disk: Some(BootDisk::new(&iso, true)),
            mem: LIVE_MEMORY_MIB,
            target_marker: TD_SETUP_LIVE_MARKER,
            kill_on_marker: false,
            extra_append: &tokens,
            user_net: false,
            // The stock session supervises the emulated sound device.
            audio: true,
            physical_input: false,
            capture_firefox_audio: false,
            tpm_socket: None,
        },
        &scratch.dir,
        timeout,
    )?;
    println!(
        "   [qemu-boot-live] elapsed: {:.2}s",
        result.elapsed.as_secs_f64()
    );
    for (sequence, state) in &result.evidence.td_setup_shown {
        println!("   [qemu-boot-live] td-setup showed {sequence}: {state}");
    }
    require_live_session(&result)?;
    let allocated = fs::metadata(scratch.dir.join(target_name))
        .map_err(|error| format!("stat the target disk: {error}"))?
        .blocks();
    if allocated != 0 {
        return Err(format!(
            "the withdrawn review left {allocated} allocated blocks on the target disk"
        ));
    }
    println!(
        "PASS: the live medium booted its signed deployment into the graphical \
         session; the installer wizard, focused with td-authd's setup intake \
         bound, took physical keys through the destination and settings pages \
         to the service's review of {TARGET_KERNEL_NAME} for {USERNAME}@{HOSTNAME} \
         in {ZONE}; the review left was released, and the target took no write"
    );
    Ok(())
}

/// Welcome, the one destination, the settings typed a key at a time, the
/// review of exactly those, and back from it until the service has
/// released the review. Every key waits on its own state.
fn script() -> Result<Vec<SetupStep>, String> {
    fn press(steps: &mut Vec<SetupStep>, shown: String, key: &'static str) {
        steps.push(SetupStep {
            shown,
            keys: vec![key],
        });
    }
    fn prefix(text: &str, index: usize) -> Result<&str, String> {
        text.get(..=index)
            .ok_or_else(|| format!("{text:?} has no prefix through {index}"))
    }
    let settings = |field: usize, username: &str, hostname: &str| {
        format!("page=settings field={field} username={username} hostname={hostname}")
    };
    let mut steps = Vec::new();
    press(&mut steps, "page=welcome".into(), "ret");
    press(
        &mut steps,
        "page=destinations disks=1 selected=-".into(),
        "down",
    );
    press(
        &mut steps,
        format!("page=destinations disks=1 selected={TARGET_KERNEL_NAME}"),
        "ret",
    );
    // The catalog has arrived once UTC is chosen.
    let mut shown = format!(
        "{} seek= zone={DEFAULT_ZONE} withdrawal=none",
        settings(0, "", "")
    );
    for (index, key) in typed(USERNAME)?.into_iter().enumerate() {
        press(&mut steps, shown, key);
        shown = settings(0, prefix(USERNAME, index)?, "");
    }
    press(&mut steps, shown, "tab");
    shown = settings(1, USERNAME, "");
    for (index, key) in typed(HOSTNAME)?.into_iter().enumerate() {
        press(&mut steps, shown, key);
        shown = settings(1, USERNAME, prefix(HOSTNAME, index)?);
    }
    // Past the fixed keyboard row to the time zone.
    press(&mut steps, shown, "tab");
    press(&mut steps, settings(2, USERNAME, HOSTNAME), "tab");
    shown = format!(
        "{} seek= zone={DEFAULT_ZONE}",
        settings(3, USERNAME, HOSTNAME)
    );
    for (index, key) in typed(ZONE_SEEK)?.into_iter().enumerate() {
        press(&mut steps, shown, key);
        shown = format!(
            "{} seek={}",
            settings(3, USERNAME, HOSTNAME),
            prefix(ZONE_SEEK, index)?
        );
    }
    press(&mut steps, format!("{shown} zone={ZONE}"), "ret");
    press(
        &mut steps,
        format!(
            "page=review disk={TARGET_KERNEL_NAME} username={USERNAME} \
             hostname={HOSTNAME} zone={ZONE}"
        ),
        "esc",
    );
    steps.push(SetupStep {
        shown: format!(
            "{} zone={ZONE} withdrawal=none",
            settings(3, USERNAME, HOSTNAME)
        ),
        keys: Vec::new(),
    });
    Ok(steps)
}

/// The live session reported the wizard's window focused with the intake
/// bound, and the script ran to its end.
fn require_live_session(result: &BootResult) -> Result<(), String> {
    let ready = result
        .console
        .lines()
        .any(|line| line.trim_end() == TD_SETUP_LIVE_MARKER);
    if !result.marker_killed || !result.evidence.target || !ready {
        return Err(format!(
            "the live session did not carry the installer wizard to its review and back: \
             {}\n{}",
            result.reason,
            tail(&result.console, 160)
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_script_types_only_its_closed_keys_and_waits_on_distinct_states() {
        let script = script().unwrap();
        // One step per key, and the last waits on the release.
        let typed_keys = USERNAME.len() + HOSTNAME.len() + ZONE_SEEK.len();
        // The three tabs and Enter, Escape, and the release.
        assert_eq!(script.len(), 3 + typed_keys + 4 + 1 + 1);
        for step in &script[..script.len() - 1] {
            assert!(step.shown.starts_with("page="), "{}", step.shown);
            assert_eq!(step.keys.len(), 1, "{}", step.shown);
            assert!(super::super::setup_input::setup_key(step.keys[0]));
        }
        let last = script.last().unwrap();
        assert!(last.keys.is_empty() && last.shown.ends_with(" withdrawal=none"));
        // The defaults the script starts from are td-setup's.
        let settings = include_str!("../../../../../../td-setup/src/settings.rs");
        assert!(settings.contains(&format!("const DEFAULT_ZONE: &str = \"{DEFAULT_ZONE}\";")));
        // Each step waits on a state its predecessor's keys change.
        for pair in script.windows(2) {
            assert_ne!(pair[0].shown, pair[1].shown);
        }
        assert_eq!(
            typed("td-wizard/").unwrap(),
            ["t", "d", "minus", "w", "i", "z", "a", "r", "d", "slash"]
        );
        assert!(typed("Dana").is_err());
        assert!(typed("a b").is_err());
        // The zone the seek types finds is the one the review expects.
        assert!(ZONE.to_ascii_lowercase().starts_with(ZONE_SEEK));
    }
}
