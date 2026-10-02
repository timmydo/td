//! `qemu-boot-live`: the production live profile boots from its own medium
//! into the graphical session and opens the installer wizard
//! (td-install/INSTALLER.md increment 6).
//!
//! The medium is `build-iso`'s, composed by the same function from the same
//! verified store deployment and signed with a key made for this run. The
//! boot is direct, kernel and live selector from that medium, so the
//! autotest token can be appended; the ISO is attached read-only as a virtio
//! disk, which the selector finds, mounts and authenticates. No automated
//! check boots this medium through firmware or optical media; `./test-iso`
//! does so by hand.
use super::build_iso::{live_medium, LiveMedium};
use super::*;

const TD_SETUP_LIVE_MARKER: &str = td_recipe::ladder::TD_SETUP_LIVE_MARKER;
/// The live volume is half of RAM and the stock session runs on the rest.
const LIVE_MEMORY_MIB: &str = "4096";

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
    let timeout = boot_timeout();
    let tokens = format!("{AUTOTEST_CMDLINE_TOKEN} {}", autotest_wait_token(timeout));
    println!("   [qemu-boot-live] booting deployment {id} live from its medium");
    let result = boot_with_timeout(
        &qemu,
        &kernel,
        &live,
        BootPlan {
            disk: Some(BootDisk::new(&iso, true)),
            mem: LIVE_MEMORY_MIB,
            target_marker: TD_SETUP_LIVE_MARKER,
            kill_on_marker: true,
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
    require_live_session(&result)?;
    println!(
        "PASS: the live medium booted its signed deployment into the graphical \
         session, which opened the installer wizard focused with td-authd's setup \
         intake bound"
    );
    Ok(())
}

/// The live session reported the wizard's window focused with the intake
/// bound; the marker's unit and the intake are both a live boot's only.
fn require_live_session(result: &BootResult) -> Result<(), String> {
    if !result.marker_killed
        || !result
            .console
            .lines()
            .any(|line| line.trim_end() == TD_SETUP_LIVE_MARKER)
    {
        return Err(format!(
            "the live session did not open the installer wizard: {}\n{}",
            result.reason,
            tail(&result.console, 160)
        ));
    }
    Ok(())
}
