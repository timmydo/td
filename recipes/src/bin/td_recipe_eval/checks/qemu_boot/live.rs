//! `qemu-boot-live`: the production live profile boots from its own medium
//! into the graphical session, opens the installer wizard, and a person's
//! keys carry it through the destination and settings pages to the
//! installation service's review, back, to a second review, and through
//! the compositor's consent to an installed disk (td-install/INSTALLER.md
//! increment 6, and increment 7's firmware boot of the medium).
//!
//! The medium is `build-iso`'s, composed by the same function from the same
//! verified store deployment and signed with a key made for this run, and
//! firmware boots it as USB mass storage: the run's private copy of the
//! firmware's variables holds one boot entry, the medium's removable
//! loader with the autotest and wizard-evidence tokens as its load options,
//! which the kernel appends to its built-in command line and the live
//! selector hands on. The selector finds, mounts and authenticates the
//! medium, and an empty sparse disk after it is the only eligible
//! destination.
//!
//! Once the live session says the wizard is ready, every key is pressed
//! through QEMU's emulated keyboard only after td-setup says it showed the
//! state that key is for, so a key cannot land on a page or field it was
//! not meant for. When td-setup says the service released the review it
//! left, and again when it asks for consent to the second, QEMU's own count
//! of writes, discards and zone appends on the target must be zero.
//! Consent is given as a person gives it: the secure attention chord, the
//! menu's `I`, and Enter only once the compositor's prompt shows, pixel for
//! pixel and nothing else, exactly the disk, its size and serial, the host,
//! the account and the deployment's prefix reviewed (Enter again while the
//! prompt stays and nothing is written, since the compositor drops an Enter
//! stamped before its receipt); then Escape from the installed notice, by
//! which the target must have been written. The live phase ends once
//! td-setup says the installation completed, and the target must then hold the
//! installer's whole GPT layout.
//!
//! With the medium detached, the installed disk then cold-boots through
//! firmware twice, alone and then renamed behind a decoy disk, and each boot
//! must bind the volume the image holds and the medium's deployment, activate
//! the account and host the wizard was given, report a healthy deployment and
//! flip the compositor's pages, with a fresh machine identity and home the
//! second boot keeps; each boot's compositor must name the configured zone
//! for its clock, and its status bar, captured from the display, must end
//! that clock in the zone's offset. The account's serial login shell must
//! say who it is, its home and zone, and read back on the second boot the
//! file it wrote into that same home on the first.
use super::build_iso::{live_medium, LiveMedium};
use super::install::{
    cold_boots, image_volume_identity, installation_timeout, system_target_capacity, ColdBoots,
    Firmware, Installed, TargetDisk,
};
use super::setup_input::{disk_prompt_rows, typed, Act, SetupStep, STEP_TIMEOUT};
use super::*;

const TD_SETUP_LIVE_MARKER: &str = td_recipe::ladder::TD_SETUP_LIVE_MARKER;
/// The live volume is half of RAM and the stock session runs on the rest.
const LIVE_MEMORY_MIB: &str = "4096";
/// The destination the wizard should list: the only virtio disk. The
/// medium, USB storage attached read-only, is excluded twice: as the
/// source's disk and as unwritable.
const TARGET_KERNEL_NAME: &str = "vda";
const USERNAME: &str = "dana";
const HOSTNAME: &str = "td-wizard";
/// Typed into the time zone row, which seeks the first zone it begins.
const ZONE_SEEK: &str = "asia/tok";
const ZONE: &str = "Asia/Tokyo";
/// What the settings page starts at: the catalog's UTC.
const DEFAULT_ZONE: &str = "Etc/UTC";
/// How the installed session's status bar ends its clock in `ZONE`, which
/// keeps nine hours ahead of UTC all year.
const ZONE_ON_BAR: &str = " UTC+09:00";
/// The recipe whose face the image's compositor draws its chrome in.
const FACE_RECIPE: &str = "jetbrains-mono-nerd-font";

pub(crate) fn run(runner: &RecipeCheckRunner) -> Result<(), String> {
    let qemu = find_qemu()?;
    // Found before the long live run, which the cold boots follow.
    let (code, vars) = efi::firmware(&qemu)?;
    let (kernel, selector, deployment) = build_system(runner)?;
    // The bar draws in the image's outline face: the oracle draws the
    // expected text from the same recipe's regular style, before the long
    // run, so a face it cannot draw fails first.
    runner.prepare_recipe_target(FACE_RECIPE)?;
    let face_out = runner.build_plan(FACE_RECIPE)?;
    let face_dir = runner
        .ladder_out_from(&face_out, FACE_RECIPE)?
        .join(td_recipe::catalog::outline_face::DIR);
    let zone_text = update::BarText::render(
        crate::face_file::read(&face_dir, crate::face_file::REGULAR)?,
        ZONE_ON_BAR,
    )?;
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
    let capacity = system_target_capacity(payload_bytes)?;
    let target = TargetDisk::with_capacity(&scratch.dir, target_name, capacity)?;
    let script = script(&disk_prompt_rows(
        TARGET_KERNEL_NAME,
        capacity,
        super::setup_input::TARGET_SERIAL,
        HOSTNAME,
        USERNAME,
        &id,
    )?)?;
    let timeout = boot_timeout();
    let tokens = format!(
        "{AUTOTEST_CMDLINE_TOKEN} {} {}",
        autotest_wait_token(timeout),
        td_recipe::ladder::SETUP_INPUT_CMDLINE_TOKEN
    );
    println!("   [qemu-boot-live] booting deployment {id} live from its medium through firmware");
    let result = boot_source(
        &qemu,
        BootSource::LiveSetup {
            code: &code,
            vars: &vars,
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
            screen: None,
            shell: None,
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
    require_partitioned(&scratch.dir.join(target_name), capacity)?;
    // The medium is detached: the disk the wizard installed boots alone
    // through firmware, as the account and host it was given.
    let uuid = image_volume_identity(&scratch.dir.join(target_name))?;
    let zone_on_bar = |pixels: &[u8]| zone_text.ends(pixels);
    cold_boots(
        &qemu,
        &Firmware {
            code: &code,
            vars: &vars,
        },
        &target,
        &Installed {
            uuid: &uuid,
            id: &id,
            username: USERNAME,
            hostname: HOSTNAME,
            zone: Some(ZONE),
        },
        &ColdBoots {
            scratch: &scratch.dir,
            name: "wizard",
            timeout: installation_timeout(
                env::var("TD_QEMU_BOOT_TIMEOUT_SECS").ok().as_deref(),
                900,
            ),
            label: "qemu-boot-live",
            screen: Some(ScreenExpect {
                what: "the status bar's clock in the configured zone",
                check: &zone_on_bar,
            }),
            session: true,
        },
    )?;
    println!(
        "PASS: the live medium, booted through firmware as USB mass storage, \
         booted its signed deployment into the graphical \
         session; the installer wizard, focused with td-authd's setup intake \
         bound, took physical keys through the destination and settings pages \
         to the service's review of {TARGET_KERNEL_NAME} for {USERNAME}@{HOSTNAME} \
         in {ZONE}, released it untouched and reviewed again; consent went \
         through the compositor's secure attention prompt showing exactly that \
         disk, its size and serial, host, account and deployment prefix, \
         td-setup said the installation completed, and the target holds the \
         installer's GPT layout; with the medium detached, the installed disk \
         cold-booted through firmware twice, alone and renamed behind a decoy, \
         as a healthy {USERNAME}@{HOSTNAME} with its volume {uuid} bound, a \
         fresh machine identity and /var/home/{USERNAME} created then found, \
         its login shell reading back on the second boot the file it wrote \
         into that same home on the first, and compositor page flips, its \
         clock naming {ZONE} and its status bar ending{ZONE_ON_BAR}"
    );
    Ok(())
}

/// The installed target holds a whole, consistent GPT, primary and backup,
/// with exactly the installer's layout: the ESP from the first aligned
/// sector, then the volume to the last usable sector.
fn require_partitioned(path: &Path, capacity: u64) -> Result<(), String> {
    use std::io::{Read as _, Seek as _, SeekFrom};
    const SECTOR: u64 = 512;
    let protocol = |error: String| format!("the installed target's GPT: {error}");
    let entries = td_engine::gpt::entry_array_sectors(SECTOR).map_err(protocol)?;
    let read = |offset: u64, sectors: u64| -> Result<Vec<u8>, String> {
        let length = sectors
            .checked_mul(SECTOR)
            .and_then(|bytes| usize::try_from(bytes).ok())
            .ok_or("GPT read length overflow")?;
        let mut bytes = vec![0u8; length];
        File::open(path)
            .and_then(|mut file| {
                file.seek(SeekFrom::Start(offset))?;
                file.read_exact(&mut bytes)
            })
            .map_err(|error| format!("read the installed target's GPT: {error}"))?;
        Ok(bytes)
    };
    let disk_sectors = capacity / SECTOR;
    let backup_sectors = entries.checked_add(1).ok_or("GPT size overflow")?;
    let primary = read(0, backup_sectors.checked_add(1).ok_or("GPT size overflow")?)?;
    let backup = read(
        disk_sectors
            .checked_sub(backup_sectors)
            .and_then(|sector| sector.checked_mul(SECTOR))
            .ok_or("the target is smaller than a GPT")?,
        backup_sectors,
    )?;
    let table = td_engine::gpt::parse(&primary, &backup, SECTOR).map_err(protocol)?;
    let align = td_boot_protocol::PARTITION_ALIGN_BYTES / SECTOR;
    let esp_end = align + td_boot_protocol::ESP_BYTES / SECTOR - 1;
    let layout: Vec<_> = table
        .partitions
        .iter()
        .map(|part| {
            (
                part.type_guid,
                part.name.as_str(),
                part.start_lba,
                part.end_lba,
            )
        })
        .collect();
    let expected = [
        (
            td_engine::gpt::TYPE_ESP,
            td_boot_protocol::ESP_PARTITION_NAME,
            align,
            esp_end,
        ),
        (
            td_engine::gpt::TYPE_LINUX_FS,
            td_boot_protocol::VOLUME_PARTITION_NAME,
            esp_end + 1,
            td_engine::gpt::last_usable_lba(SECTOR, disk_sectors).map_err(protocol)?,
        ),
    ];
    if table.disk_sectors != disk_sectors || layout != expected {
        return Err(format!(
            "the installed target's GPT has {} sectors and partitions {layout:?}, \
             not {disk_sectors} and {expected:?}",
            table.disk_sectors
        ));
    }
    Ok(())
}

/// Welcome, the one destination, the settings typed a key at a time, the
/// review of exactly those, back from it until the service has released
/// the review, the same review again, consent to it showing `rows`, and the
/// completed installation. Every act waits on its own state.
fn script(rows: &[String]) -> Result<Vec<SetupStep>, String> {
    fn press(steps: &mut Vec<SetupStep>, shown: String, key: &'static str) {
        steps.push(SetupStep::press(shown, key));
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
    let review = format!(
        "page=review disk={TARGET_KERNEL_NAME} username={USERNAME} \
         hostname={HOSTNAME} zone={ZONE}"
    );
    press(&mut steps, review.clone(), "esc");
    steps.push(SetupStep {
        untouched: true,
        ..SetupStep::press(
            format!(
                "{} zone={ZONE} withdrawal=none",
                settings(3, USERNAME, HOSTNAME)
            ),
            "ret",
        )
    });
    press(&mut steps, review, "ret");
    steps.push(SetupStep {
        shown: "page=consent".into(),
        act: Act::Consent(rows.to_vec()),
        untouched: true,
        within: STEP_TIMEOUT,
    });
    // td-authd ends the attention only once the installation finished, so
    // td-setup's next poll says so.
    steps.push(SetupStep {
        shown: "page=complete".into(),
        act: Act::Keys(Vec::new()),
        untouched: false,
        within: STEP_TIMEOUT,
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
            "the live session did not carry the installer wizard through its review \
             and consent to an installation: {}\n{}",
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
        let rows = vec!["ROW".to_string()];
        let script = script(&rows).unwrap();
        let typed_keys = USERNAME.len() + HOSTNAME.len() + ZONE_SEEK.len();
        // The three tabs and Enter; Escape; Enter from the release and from
        // the second review; consent; and the completion.
        assert_eq!(script.len(), 3 + typed_keys + 4 + 1 + 2 + 1 + 1);
        let (consent, last) = (&script[script.len() - 2], &script[script.len() - 1]);
        for step in &script[..script.len() - 2] {
            assert!(step.shown.starts_with("page="), "{}", step.shown);
            let Act::Keys(keys) = &step.act else {
                panic!("{} does not press", step.shown);
            };
            assert_eq!(keys.len(), 1, "{}", step.shown);
            assert!(super::super::setup_input::setup_key(keys[0]));
            assert_eq!(step.within, STEP_TIMEOUT);
        }
        // The target is untouched at the release and at the consent asked.
        let untouched: Vec<&str> = script
            .iter()
            .filter(|step| step.untouched)
            .map(|step| step.shown.as_str())
            .collect();
        assert_eq!(untouched.len(), 2);
        assert!(untouched[0].ends_with(" withdrawal=none"));
        assert_eq!(untouched[1], "page=consent");
        assert!(matches!(&consent.act, Act::Consent(shown) if *shown == rows));
        assert_eq!(last.shown, "page=complete");
        assert!(matches!(&last.act, Act::Keys(keys) if keys.is_empty()));
        assert_eq!(last.within, STEP_TIMEOUT);
        // The same review is asked twice, and consent follows the second.
        let reviews: Vec<usize> = (0..script.len())
            .filter(|index| script[*index].shown.starts_with("page=review "))
            .collect();
        assert_eq!(reviews.len(), 2);
        assert_eq!(script[reviews[0]].shown, script[reviews[1]].shown);
        assert_eq!(reviews[1] + 1, script.len() - 2);
        // The installed target must carry exactly the installer's layout.
        let dir = std::env::temp_dir().join(format!("td-live-gpt-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let image = dir.join("target.raw");
        let capacity = 2 * 1024 * 1024 * 1024u64;
        let sectors = capacity / 512;
        let last = td_engine::gpt::last_usable_lba(512, sectors).unwrap();
        let write = |volume_end: u64, esp_name: &str| {
            let table = td_engine::gpt::build(&td_engine::gpt::Layout {
                sector_size: 512,
                disk_sectors: sectors,
                disk_guid: td_engine::gpt::Guid([7; 16]),
                align_sectors: 2048,
                partitions: vec![
                    td_engine::gpt::Partition {
                        type_guid: td_engine::gpt::TYPE_ESP,
                        unique_guid: td_engine::gpt::Guid([1; 16]),
                        start_lba: 2048,
                        end_lba: 2048 + 1024 * 1024 - 1,
                        attributes: 0,
                        name: esp_name.into(),
                    },
                    td_engine::gpt::Partition {
                        type_guid: td_engine::gpt::TYPE_LINUX_FS,
                        unique_guid: td_engine::gpt::Guid([2; 16]),
                        start_lba: 2048 + 1024 * 1024,
                        end_lba: volume_end,
                        attributes: 0,
                        name: td_boot_protocol::VOLUME_PARTITION_NAME.into(),
                    },
                ],
            })
            .unwrap();
            let file = File::create(&image).unwrap();
            file.set_len(capacity).unwrap();
            use std::os::unix::fs::FileExt as _;
            file.write_all_at(&table.primary, table.primary_offset)
                .unwrap();
            file.write_all_at(&table.backup, table.backup_offset)
                .unwrap();
        };
        write(last, td_boot_protocol::ESP_PARTITION_NAME);
        assert_eq!(require_partitioned(&image, capacity), Ok(()));
        assert!(require_partitioned(&image, capacity + 512).is_err());
        write(last - 1, td_boot_protocol::ESP_PARTITION_NAME);
        assert!(require_partitioned(&image, capacity).is_err());
        write(last, "other");
        assert!(require_partitioned(&image, capacity).is_err());
        // Nor is a whole table for a disk one sector shorter.
        {
            use std::os::unix::fs::FileExt as _;
            let short = sectors - 1;
            let table = td_engine::gpt::build(&td_engine::gpt::Layout {
                sector_size: 512,
                disk_sectors: short,
                disk_guid: td_engine::gpt::Guid([7; 16]),
                align_sectors: 2048,
                partitions: vec![
                    td_engine::gpt::Partition {
                        type_guid: td_engine::gpt::TYPE_ESP,
                        unique_guid: td_engine::gpt::Guid([1; 16]),
                        start_lba: 2048,
                        end_lba: 2048 + 1024 * 1024 - 1,
                        attributes: 0,
                        name: td_boot_protocol::ESP_PARTITION_NAME.into(),
                    },
                    td_engine::gpt::Partition {
                        type_guid: td_engine::gpt::TYPE_LINUX_FS,
                        unique_guid: td_engine::gpt::Guid([2; 16]),
                        start_lba: 2048 + 1024 * 1024,
                        end_lba: td_engine::gpt::last_usable_lba(512, short).unwrap(),
                        attributes: 0,
                        name: td_boot_protocol::VOLUME_PARTITION_NAME.into(),
                    },
                ],
            })
            .unwrap();
            let file = File::create(&image).unwrap();
            file.set_len(capacity).unwrap();
            file.write_all_at(&table.primary, 0).unwrap();
            file.write_all_at(&table.backup, capacity - table.backup.len() as u64)
                .unwrap();
        }
        assert!(require_partitioned(&image, capacity).is_err());
        // A damaged backup is not a whole table.
        write(last, td_boot_protocol::ESP_PARTITION_NAME);
        {
            use std::os::unix::fs::FileExt as _;
            let file = fs::OpenOptions::new().write(true).open(&image).unwrap();
            file.write_all_at(b"X", capacity - 512 + 16).unwrap();
        }
        assert!(require_partitioned(&image, capacity).is_err());
        fs::write(&image, b"short").unwrap();
        assert!(require_partitioned(&image, capacity).is_err());
        fs::remove_dir_all(&dir).unwrap();
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

    /// The bar ends its clock in the zone's offset, as the compositor's
    /// clock writes it from the zone /etc/timezone names.
    #[test]
    fn the_bar_shows_the_chosen_zone_as_its_offset() {
        // Asia/Tokyo has kept UTC+09:00, without daylight saving, since 1951.
        assert_eq!((ZONE, ZONE_ON_BAR), ("Asia/Tokyo", " UTC+09:00"));
        let clock = include_str!("../../../../../../td-compositor/src/clock.rs");
        assert!(clock.contains("\"UTC{sign}{:02}:{:02}\","));
        assert!(clock.contains(
            "\"{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02} {suffix}\""
        ));
        let bar = include_str!("../../../../../../td-compositor/src/bar.rs");
        assert!(
            bar.contains("Clock::load(Path::new(\"/etc/timezone\"), Path::new(\"/etc/zoneinfo\"))")
        );
        // The lines the installed-boot oracle reads are the ones written.
        assert!(bar.contains("\"\\ntd-compositor: clock zone {name}\\n\""));
        let profile = include_str!("../../../../../../td-firstboot/src/primary_profile.rs");
        assert!(profile.contains(
            "\"td-firstboot: primary home {} {home}\\nTD-PRIMARY-PROFILE-READY {expected}\\n\""
        ));
    }
}
