//! `qemu-login-system` (td-secret/DESIGN.md, "Login system guest"): the
//! TPM-free full-system login guest. The test-only system image boots
//! once per phase on one disposable volume whose files are guest root's,
//! on a q35 machine whose ACPI offers suspend to RAM, its one ignored test
//! asking for the display checks below over ttyS0. The host reads each
//! login disclosure's approval key off the screen and answers with it,
//! and cuts the power of the phase that asks for a cut.

use super::super::guest_screens::{Check, GuestScreens, Holds, Reads, Screen, Wake};
use super::*;

/// The lock surface's and the attention screen's rows for each cause the
/// guest shows (td-login/TOKEN-LOGIN.md, "The login record").
const RECORD: &[&str] = &["LOGIN KEY STATE UNAVAILABLE:", "RECORD DAMAGED"];
const UNREADABLE: &[&str] = &["LOGIN KEY STATE UNAVAILABLE:", "STATE COULD NOT BE READ"];
/// The attention menu, as the update oracle reads it from 276: its
/// selections 36 apart, and its last row four lower than other screens'.
/// No hostname change waits on this guest.
const MENU: &[&str] = &[
    "TD SECURE ATTENTION",
    "U: UNLOCK  R: RECOVERY TOKEN",
    "E: ENROLL TWO TOKENS (HAVE BOTH READY)",
    "X: ENROLL WITHOUT RECOVERY - LOSS IS FINAL",
    "W: REVIEW PENDING CREDENTIAL WRITE",
    "I: REVIEW PENDING SYSTEM INSTALLATION",
    "K: LOGIN KEYS",
    "L: LOCK SCREEN",
    "B: ROLL BACK TO THE PREVIOUS SYSTEM",
    "H: NO HOSTNAME CHANGE WAITING",
    "S: DISK ENCRYPTION",
    "ESC TO RETURN",
];
/// Every phase boots within this, its locked phase's steps included.
const PHASE_TIMEOUT: Duration = Duration::from_secs(1800);

fn system_lock(pixels: &[u8], state: &[&str]) -> Result<bool, String> {
    chrome(pixels, &lock_surface_on(fixture::LOGIN_SYSTEM_HOST, state))
}

fn locked(pixels: &[u8], arguments: &[&str]) -> Result<bool, String> {
    bare(arguments)?;
    system_lock(pixels, &["PRESS CTRL+ALT+ESC TO UNLOCK"])
}

fn locked_damaged(pixels: &[u8], arguments: &[&str]) -> Result<bool, String> {
    bare(arguments)?;
    system_lock(pixels, DAMAGED)
}

fn locked_record(pixels: &[u8], arguments: &[&str]) -> Result<bool, String> {
    bare(arguments)?;
    system_lock(pixels, RECORD)
}

fn locked_unreadable(pixels: &[u8], arguments: &[&str]) -> Result<bool, String> {
    bare(arguments)?;
    system_lock(pixels, UNREADABLE)
}

fn record(pixels: &[u8], arguments: &[&str]) -> Result<bool, String> {
    bare(arguments)?;
    chrome(pixels, &attention(RECORD))
}

fn unreadable(pixels: &[u8], arguments: &[&str]) -> Result<bool, String> {
    bare(arguments)?;
    chrome(pixels, &attention(UNREADABLE))
}

fn menu_rows() -> Vec<(usize, String)> {
    MENU.iter()
        .enumerate()
        .map(|(index, row)| (CHROME_TOP + index * CHROME_PITCH, row.to_string()))
        .collect()
}

/// Exactly the menu's rows on the ground. The chrome face has no
/// parenthesis, which the compositor draws as its missing box, so `E`'s
/// row is required only to lie in its band.
fn menu(pixels: &[u8], arguments: &[&str]) -> Result<bool, String> {
    bare(arguments)?;
    let rows = menu_rows();
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
    for (top, text) in rows.iter().filter(|(_, text)| !text.contains('(')) {
        if !super::super::update::menu_row_matches(pixels, *top, text)? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn update_refused(pixels: &[u8], arguments: &[&str]) -> Result<bool, String> {
    bare(arguments)?;
    chrome(pixels, &attention(&["UPDATE CANNOT READ LOGIN KEYS"]))
}

/// `install ID`: the trusted installation prompt for the booted
/// deployment, `booted` being its manifest's ID as the host computed it.
fn install(booted: &str, pixels: &[u8], arguments: &[&str]) -> Result<bool, String> {
    let [id] = arguments else {
        return Err("install takes a deployment ID".into());
    };
    if *id != booted {
        return Err(format!(
            "install names {id}, not the booted deployment {booted}"
        ));
    }
    super::super::update::prompt_pixels_match(pixels, id)
}

/// QEMU's own frame while no scanout is set, between a compositor's exit
/// and its successor's first frame or after a wake: black, and its grey
/// "Display output is not active." in the 16-pixel text row from 384,
/// within 128 pixels of the centre, so no guest pixel.
const INACTIVE: [u8; 3] = [0xaa, 0xaa, 0xaa];
const INACTIVE_ROWS: std::ops::Range<usize> = 384..400;
const INACTIVE_COLUMNS: std::ops::Range<usize> = 512..768;

/// QEMU's inactive output: that frame and nothing else.
fn inactive(pixels: &[u8]) -> Result<bool, String> {
    let mut text = false;
    for (y, row) in rows_of(pixels)?.enumerate() {
        for (x, pixel) in row.chunks_exact(3).enumerate() {
            if pixel == INACTIVE && INACTIVE_ROWS.contains(&y) && INACTIVE_COLUMNS.contains(&x) {
                text = true;
            } else if pixel != [0; 3] {
                return Ok(false);
            }
        }
    }
    Ok(text)
}

/// What may show while locked: the lock palette, or QEMU's inactive
/// output.
fn locked_or_inactive(pixels: &[u8]) -> Result<bool, String> {
    Ok(blank_or_lock(pixels)? || inactive(pixels)?)
}

/// `killed`: the killed compositor's frame gone, its successor's not yet
/// anything but the lock.
fn killed(pixels: &[u8], arguments: &[&str]) -> Result<bool, String> {
    bare(arguments)?;
    locked_or_inactive(pixels)
}

/// The key-management screen the attention menu's `K` opens.
const KEYS: &[&str] = &[
    "LOGIN KEYS",
    "1: ENROLL ONE KEY",
    "2: ENROLL TWO KEYS",
    "A: ADD A KEY",
    "D: REMOVE KEYS",
];

/// An attention screen showing exactly `notice`.
fn notice(pixels: &[u8], arguments: &[&str], notice: &[&str]) -> Result<bool, String> {
    bare(arguments)?;
    chrome(pixels, &attention(notice))
}

/// `removing COUNT [POSITION...]`: `D`'s screen for a list of COUNT keys
/// with those positions chosen.
fn removing(pixels: &[u8], arguments: &[&str]) -> Result<bool, String> {
    let Some((count, chosen)) = arguments.split_first() else {
        return Err("removing takes a key count and the positions chosen".into());
    };
    let count = count
        .parse::<u8>()
        .ok()
        .filter(|count| (1..=8).contains(count))
        .ok_or("invalid key count")?;
    let mut last = 0;
    for position in chosen {
        let position = position.parse::<u8>().map_err(|_| "invalid position")?;
        if position <= last || position > count {
            return Err("positions are increasing within the count".into());
        }
        last = position;
    }
    let selected = if chosen.is_empty() {
        "SELECTED: NONE".to_string()
    } else {
        format!("SELECTED: {}", chosen.join(" "))
    };
    chrome(
        pixels,
        &attention(&[&format!("REMOVE: PRESS 1 TO {count} THEN ENTER"), &selected]),
    )
}

/// A prompt's rows as the guest names them, from consent's own lines for
/// the step it expects: each space written `_`, so a row is one word.
fn named_rows(arguments: &[&str]) -> Result<Vec<String>, String> {
    if arguments.is_empty() || arguments.len() > 18 {
        return Err("a prompt names 1 to 18 rows".into());
    }
    arguments
        .iter()
        .map(|row| {
            if row.is_empty() || row.len() > 77 || !row.bytes().all(|byte| byte.is_ascii_graphic())
            {
                Err(format!("invalid prompt row {row:?}"))
            } else {
                Ok(row.replace('_', " "))
            }
        })
        .collect()
}

/// `step ROW...`: a login step's prompt that takes no PIN, every pixel.
fn step(pixels: &[u8], arguments: &[&str]) -> Result<bool, String> {
    prompt_matches(pixels, &named_rows(arguments)?, None)
}

/// `step-pin MASKS ROW...`: a login PIN step's prompt with its field open
/// and that many masks.
fn step_pin(pixels: &[u8], arguments: &[&str]) -> Result<bool, String> {
    let Some((masks, rows)) = arguments.split_first() else {
        return Err("step-pin takes a mask count and rows".into());
    };
    let masks = masks
        .parse::<usize>()
        .map_err(|_| "invalid mask count".to_string())?;
    prompt_matches(
        pixels,
        &named_rows(rows)?,
        Some(("ENTER THE PIN FOR THIS KEY", masks)),
    )
}

/// `step-touch ROW...`: a login PIN step's prompt, its PIN sent and its
/// touch asked for.
fn step_touch(pixels: &[u8], arguments: &[&str]) -> Result<bool, String> {
    prompt_matches(pixels, &named_rows(arguments)?, Some(("TOUCH YOUR KEY", 0)))
}

/// `disclosure ROW...`, the approval key's row named `KEY`: a disclosure
/// step's prompt, every pixel, and the key root drew for it, which only
/// the screen shows: one of the 64, or an error when its row shows two.
fn disclosure_key(pixels: &[u8], arguments: &[&str]) -> Result<Option<String>, String> {
    let mut lines = named_rows(arguments)?;
    let mut keyed = lines.iter().enumerate().filter(|(_, row)| *row == "KEY");
    let (Some((at, _)), None) = (keyed.next(), keyed.next()) else {
        return Err("a disclosure names exactly one KEY row".into());
    };
    let top = prompt_top(lines.len())?;
    let mut shown = None;
    for first in b'2'..=b'9' {
        for second in b'2'..=b'9' {
            let row = format!(
                "APPROVE: TYPE {} THEN {}",
                char::from(first),
                char::from(second)
            );
            if super::super::update::row_matches(pixels, top + at * PROMPT_PITCH, &row)?
                && shown.replace((row, [first, second])).is_some()
            {
                return Err("the disclosure's key row matches two keys".into());
            }
        }
    }
    let Some((row, key)) = shown else {
        return Ok(None);
    };
    if let Some(line) = lines.get_mut(at) {
        *line = row;
    }
    Ok(prompt_matches(pixels, &lines, None)?.then(|| String::from_utf8_lossy(&key).into_owned()))
}

fn disclosure(pixels: &[u8], arguments: &[&str]) -> Result<bool, String> {
    Ok(disclosure_key(pixels, arguments)?.is_some())
}

/// The key a disclosure its check accepted shows, which the host answers
/// with.
fn disclosed(pixels: &[u8], arguments: &[&str]) -> Result<String, String> {
    disclosure_key(pixels, arguments)?.ok_or_else(|| "the accepted disclosure shows no key".into())
}

/// Each screen's check and hold: while locked, only what may show while
/// locked, through both touch requests to `release`, after which the
/// guest completes the touch; after an unlock no lock pixel until the
/// desktop. The menu holds the lock palette until the screen it leads to;
/// a refusal, the prompt and the desktop hold nothing, since Escape
/// returns to the desktop. `killed` holds from the old compositor's
/// frame's end to its successor's lock. `asleep` is the desktop the guest
/// suspends from, and its wake holds what may show while locked (`run`).
/// A key-management operation holds the attention screen's pixels from
/// its `K` screen to its end, across the cutover that follows a write,
/// and its notice, which Escape leaves, holds nothing.
fn login_system_checks(install: Check<'_>) -> Vec<Screen<'_>> {
    let lock: Option<Holds<'static>> = Some(&locked_or_inactive);
    vec![
        Screen {
            name: "blank",
            check: &blank,
            then: lock,
        },
        Screen {
            name: "blank-unlocked",
            check: &blank,
            then: Some(&never_locked),
        },
        Screen {
            name: "locked",
            check: &locked,
            then: lock,
        },
        Screen {
            name: "locked-damaged",
            check: &locked_damaged,
            then: lock,
        },
        Screen {
            name: "locked-record",
            check: &locked_record,
            then: lock,
        },
        Screen {
            name: "locked-unreadable",
            check: &locked_unreadable,
            then: lock,
        },
        Screen {
            name: "damaged",
            check: &damaged,
            then: lock,
        },
        Screen {
            name: "record",
            check: &record,
            then: lock,
        },
        Screen {
            name: "unreadable",
            check: &unreadable,
            then: lock,
        },
        Screen {
            name: "pin",
            check: &pin,
            then: lock,
        },
        Screen {
            name: "touch",
            check: &touch,
            then: lock,
        },
        Screen {
            name: "release",
            check: &touch,
            then: None,
        },
        Screen {
            name: "killed",
            check: &killed,
            then: lock,
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
        Screen {
            name: "menu",
            check: &menu,
            then: lock,
        },
        Screen {
            name: "update-refused",
            check: &update_refused,
            then: None,
        },
        Screen {
            name: "install",
            check: install,
            then: None,
        },
        Screen {
            name: "asleep",
            check: &desktop,
            then: None,
        },
        Screen {
            name: "keys",
            check: &|pixels, arguments| notice(pixels, arguments, KEYS),
            then: lock,
        },
        Screen {
            name: "version-refused",
            check: &|pixels, arguments| {
                notice(pixels, arguments, &["A RETAINED SYSTEM CANNOT READ KEYS"])
            },
            then: None,
        },
        Screen {
            name: "eight",
            check: &|pixels, arguments| notice(pixels, arguments, &["EIGHT KEYS ALREADY ENROLLED"]),
            then: None,
        },
        Screen {
            name: "enrolled",
            check: &|pixels, arguments| notice(pixels, arguments, &["LOGIN KEYS ENROLLED"]),
            then: None,
        },
        Screen {
            name: "added",
            check: &|pixels, arguments| notice(pixels, arguments, &["LOGIN KEY ADDED"]),
            then: None,
        },
        Screen {
            name: "removed",
            check: &|pixels, arguments| notice(pixels, arguments, &["LOGIN KEYS REMOVED"]),
            then: None,
        },
        Screen {
            name: "removing",
            check: &removing,
            then: lock,
        },
        Screen {
            name: "disclosure",
            check: &disclosure,
            then: lock,
        },
        Screen {
            name: "step",
            check: &step,
            then: lock,
        },
        Screen {
            name: "step-pin",
            check: &step_pin,
            then: lock,
        },
        Screen {
            name: "step-touch",
            check: &step_touch,
            then: lock,
        },
    ]
}

/// The screens whose answers carry what the host read: a disclosure's
/// approval key, which the guest then types.
const READS: &[(&str, Reads<'static>)] = &[("disclosure", &disclosed)];

/// Whether a phase's serial greeter logs in: on the unenrolled boots and
/// after a cutover back to the unenrolled state. Every phase prints the
/// greeter's refusal exactly once: at an enrolled boot, or after the
/// enrollment's cutover on `seed` and `unlocked`; `cut` is cut before its
/// enrollment's cutover restarts the greeter.
fn logs_in(phase: &str) -> bool {
    matches!(phase, "seed" | "removed" | "unlocked" | "cut")
}

/// What one phase's console must show beside its screens: its pass line
/// and QEMU's clean exit, or on the cut phase its cut word once, the kill
/// and no pass or shutdown; boot health behind the lock; the serial
/// greeter's one refusal line, and a login exactly where it logs in; and
/// rootcheck's login directory marker on every boot but a damaged
/// directory's.
fn phase_result(result: &BootResult, phase: &str) -> Result<(), String> {
    let lines = |wanted: &str| {
        result
            .console
            .lines()
            .filter(|line| line.trim_end_matches('\r') == wanted)
            .count()
    };
    let failed = result
        .console
        .lines()
        .any(|line| line.starts_with(&format!("secret-fixture: {}", fixture::FAIL)));
    let cut = phase == fixture::LOGIN_SYSTEM_CUT_PHASE;
    let ended = if cut {
        result.marker_killed
            && !result.exited_clean
            && !result.evidence.kernel_panic
            && lines(&format!("{} {phase}", fixture::LOGIN_SYSTEM_CUT)) == 1
            && !result.evidence.shutdown
            && !result.console.lines().any(|line| {
                line.contains(fixture::LOGIN_SYSTEM_PASS) || line.contains("test result:")
            })
    } else {
        result.exited_clean && !result.marker_killed && !result.evidence.kernel_panic
    };
    let refusals = lines(fixture::CONSOLE_REFUSED);
    let greeted = lines(GREETER_MARKER) > 0;
    let directory = !phase.starts_with("damaged-directory-");
    let why = if !result.evidence.target || failed {
        "the guest's test did not reach its end".to_string()
    } else if !ended {
        if cut {
            "QEMU was not cut once on the guest's word, before any shutdown".into()
        } else {
            "QEMU did not exit cleanly after the guest's reboot".into()
        }
    } else if !result.evidence.boot_success {
        format!("boot health did not complete ({SYSTEM_BOOT_SUCCESS_MARKER:?} absent)")
    } else if refusals != 1 || greeted != logs_in(phase) {
        format!(
            "the serial greeter printed its refusal {refusals} times and greeted {greeted}, \
             not once and {}",
            logs_in(phase)
        )
    } else if result.evidence.login_directory != directory {
        format!(
            "rootcheck's login directory marker was {}, not {directory}",
            result.evidence.login_directory
        )
    } else {
        return Ok(());
    };
    Err(format!(
        "login system {phase}: {why}: {}\n{}",
        result.reason,
        tail(&result.console, 160)
    ))
}

pub(crate) fn run(runner: &RecipeCheckRunner) -> Result<(), String> {
    if crate::checks::accel::headless_from_env()?.names != ["kvm"] {
        return Err("qemu-login-system runs on KVM only".into());
    }
    let qemu = find_qemu()?;
    let system = output(runner, "system-secret-vm-test")?;
    let deployment = system.join("deployment");
    let (kernel, _, _) = verify_deployment(&deployment)?;
    let selector = verify_selector(&system.join("boot"))?;
    let builder = PathBuf::from(runner.builder_command().get_program());
    let trust = RunTrust::generate_root_owned(builder)?;
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let scratch = Scratch {
        dir: create_qmp_scratch_dir(&env::temp_dir(), &SEQ)?,
    };
    let initramfs = provision_selector(&selector, &scratch.dir, &trust)?;
    let (mkfs, btrfs) = build_btrfs_tools(runner)?;
    let volume = scratch.dir.join("login-system.btrfs");
    create_persistent_volume(
        &deployment,
        &mkfs,
        &btrfs,
        &volume,
        &trust,
        VolumePurpose::Fixture,
    )?;
    let id = crate::sha256::sha256_file(&deployment.join("manifest"))
        .map_err(|e| format!("hash login system manifest: {e}"))?;
    let keep = runner.scratch_dir().join("login-system-screens");
    fs::create_dir_all(&keep).map_err(|e| format!("create {}: {e}", keep.display()))?;
    let booted = |pixels: &[u8], arguments: &[&str]| install(&id, pixels, arguments);
    let checks = login_system_checks(&booted);
    for phase in fixture::LOGIN_SYSTEM_PHASES {
        let order = fixture::login_system_screens(phase);
        let plan = GuestScreens {
            prompt: fixture::LOGIN_SCREEN,
            answer: fixture::LOGIN_SHOWN,
            screens: &checks,
            order: &order,
            keep: Some(&keep),
            wake: Some(Wake {
                after: "asleep",
                holds: &locked_or_inactive,
                inactive: &inactive,
            }),
            reads: READS,
        };
        let tokens = format!("td.hid-fixture=1 td.login-system={phase}");
        let cut = *phase == fixture::LOGIN_SYSTEM_CUT_PHASE;
        let marker = if cut {
            format!("{} {phase}", fixture::LOGIN_SYSTEM_CUT)
        } else {
            format!("secret-fixture: {} {phase}", fixture::LOGIN_SYSTEM_PASS)
        };
        println!("[qemu-login-system] {phase}");
        // Kept whether or not the phase passes.
        let console = runner
            .scratch_dir()
            .join(format!("login-system-{phase}.log"));
        let result = boot_source(
            &qemu,
            BootSource::Suspendable {
                kernel: &kernel,
                initramfs: &initramfs,
            },
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
                tpm_socket: None,
                side_channel: None,
                answers: None,
                cut: false,
                keep_console: Some(&console),
                screens: (!order.is_empty()).then_some(&plan),
                devices: Devices::ALL,
                screen: None,
                shell: None,
                host_login: None,
            },
            runner.scratch_dir(),
            PHASE_TIMEOUT,
        )?;
        phase_result(&result, phase)?;
        require_selected_deployment(
            &result,
            td_boot_protocol::SELECTED_CURRENT_MARKER,
            &id,
            phase,
        )?;
        // The cut ends with no shutdown, its volume as the power cut left
        // it for the next boot.
        if !cut {
            validate_persistent_shutdown(&result, phase)?;
            check_persistent_volume(&btrfs, &volume)?;
        }
        println!(
            "[qemu-login-system] {phase} passed in {:.2}s",
            result.elapsed.as_secs_f64()
        );
    }
    println!(
        "PASS: a full TPM-free system refused a first enrollment while previous lacked the tier \
         marker, enrolled two keys through the attention screen, its disclosure's approval key \
         read off the screen, and the cutover ended a held serial session with a process \
         ignoring TERM and an open SSH session, every process of both containments, the \
         greeter refusing; root SSH refused before and after; booted locked behind its client \
         with the serial greeter refused, the enforced SSH form and boot health complete, \
         unlocked with each key, relocked by Super+l, L and a killed compositor, refused \
         unmarked and wrong-version updates and admitted its own, showed each unavailable \
         cause and recovered; added keys to eight and refused a ninth; removed seven and then \
         the last behind their disclosures, the serial greeter logging in at once and the next \
         boot unlocked; completed a one-key enrollment's revocation in the generation after \
         its authority was killed between publication and restart; and after a power cut \
         right after an enrollment's publication booted with firstboot's enforced form; after \
         QEMU S3 the session was locked when the first post-wake input was routed, in the same \
         generation, and after its first change following the wake the display showed only \
         QEMU's inactive output or lock pixels; UHID keys and keyboard, scripted presence, no \
         lock surface seen after the wake (virtio-gpu has no restore), no physical-presence, \
         USB, lid, hardware suspend or YubiKey claim"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cut phase passes only on its own word once, the kill, no clean
    /// exit, panic, pass or shutdown, and the boot's other evidence.
    #[test]
    fn the_cut_phase_requires_its_word_and_the_kill_alone() {
        let cut = || BootResult {
            evidence: ConsoleEvidence {
                target: true,
                boot_success: true,
                login_directory: true,
                ..ConsoleEvidence::default()
            },
            exited_clean: false,
            marker_killed: true,
            reason: String::new(),
            console: format!(
                "{}\n{GREETER_MARKER}\n{} cut\n",
                fixture::CONSOLE_REFUSED,
                fixture::LOGIN_SYSTEM_CUT
            ),
            elapsed: Duration::from_secs(1),
            firefox_audio: FirefoxAudioCapture::NotRequested,
        };
        let phase = fixture::LOGIN_SYSTEM_CUT_PHASE;
        assert_eq!(phase_result(&cut(), phase), Ok(()));
        for case in 0..9 {
            let mut result = cut();
            match case {
                0 => result.marker_killed = false,
                1 => result.exited_clean = true,
                2 => result.evidence.kernel_panic = true,
                3 => result.evidence.shutdown = true,
                4 => result
                    .console
                    .push_str(&format!("{} cut\n", fixture::LOGIN_SYSTEM_CUT)),
                5 => result.console.push_str(&format!(
                    "secret-fixture: {} cut\n",
                    fixture::LOGIN_SYSTEM_PASS
                )),
                6 => result
                    .console
                    .push_str(&format!("{}\n", fixture::CONSOLE_REFUSED)),
                7 => result.evidence.boot_success = false,
                _ => result.evidence.target = false,
            }
            assert!(phase_result(&result, phase).is_err(), "case {case}");
        }
    }

    /// The guest's phases are the host's, in order, and every screen name
    /// it asks for has exactly one check.
    #[test]
    fn the_guest_asks_for_the_phases_and_screens_the_host_checks() {
        let source = include_str!("../../../../../../../td-secret/src/login_system_vm.rs");
        let phases = source
            .split("const PHASES: &[&str] = &[")
            .nth(1)
            .and_then(|rest| rest.split("];").next())
            .unwrap();
        let phases: Vec<&str> = phases.split('"').skip(1).step_by(2).collect();
        assert_eq!(phases, fixture::LOGIN_SYSTEM_PHASES);
        let booted = |pixels: &[u8], arguments: &[&str]| install("id", pixels, arguments);
        let checks = login_system_checks(&booted);
        let names: std::collections::BTreeSet<_> =
            checks.iter().map(|screen| screen.name).collect();
        assert_eq!(names.len(), checks.len());
        let mut asked = std::collections::BTreeSet::new();
        for phase in fixture::LOGIN_SYSTEM_PHASES {
            for name in fixture::login_system_screens(phase) {
                assert!(names.contains(name), "{name}");
                asked.insert(name);
            }
        }
        assert_eq!(asked, names);
        for name in &names {
            assert!(
                source.contains(&format!("\"{name}\"")),
                "the guest never asks for {name}"
            );
        }
        assert_eq!(
            fixture::login_system_screens("unlocked"),
            [
                "blank-unlocked",
                "desktop",
                "menu",
                "keys",
                "disclosure",
                "step-pin",
                "step-touch",
                "step-pin",
                "step-touch",
                "step-pin",
                "step-touch",
                "step",
                "locked"
            ]
        );
        // Only the disclosure's answer carries a word, the key the host
        // read; the guest reads it there alone.
        assert_eq!(READS.len(), 1);
        assert_eq!(READS[0].0, "disclosure");
        assert_eq!(source.matches("host.read(").count(), 1);
        assert_eq!(source.matches("host.read(\"disclosure\"").count(), 1);
        let cut = format!("const CUT: &str = \"{}\";", fixture::LOGIN_SYSTEM_CUT);
        assert_eq!(source.matches(&cut).count(), 1, "{cut}");
        assert!(fixture::LOGIN_SYSTEM_PHASES.contains(&fixture::LOGIN_SYSTEM_CUT_PHASE));
        let test = fixture::LOGIN_SYSTEM_TEST
            .strip_prefix("login_operation::tests::vm::system::")
            .unwrap();
        let body = source.split(&format!("fn {test}() {{")).nth(1).unwrap();
        assert!(body.trim_start().starts_with("guard(\"fido-system\");"));
        let pin = format!("const HOSTNAME: &str = \"{}\";", fixture::LOGIN_SYSTEM_HOST);
        assert_eq!(source.matches(&pin).count(), 1, "{pin}");
        for phase in fixture::LOGIN_SYSTEM_PHASES {
            assert_eq!(
                fixture::login_system_phase(&format!("console=ttyS0 td.login-system={phase}")),
                Ok(*phase)
            );
        }
        for cmdline in [
            "",
            "td.login-system=damaged",
            "td.login-system=seed td.login-system=seed",
            "td.login-system=",
        ] {
            assert!(fixture::login_system_phase(cmdline).is_err(), "{cmdline}");
        }
    }

    fn prompt_frame(lines: &[String], seconds: usize, field: Option<(&str, usize)>) -> Vec<u8> {
        super::super::tests::login_prompt_frame(lines, seconds, field)
    }

    fn frame(rows: &[(usize, String)]) -> Vec<u8> {
        let rows: Vec<(usize, &str)> = rows
            .iter()
            .map(|(top, text)| (*top, text.as_str()))
            .collect();
        super::super::tests::chrome_frame(&rows)
    }

    /// The system's lock surfaces carry its own hostname, each cause's rows
    /// match only their own screen, and the menu has its nine rows.
    #[test]
    fn a_lock_admits_its_palette_and_qemus_inactive_output_alone() {
        let at = |x: usize, y: usize| (y * 1280 + x) * 3;
        let black = vec![0; 1280 * 800 * 3];
        let mut output = black.clone();
        // QEMU's text as captured: 520..=748 by 386..=398, and the band's
        // corners.
        for (x, y) in [(520, 386), (748, 398), (512, 384), (767, 399)] {
            output[at(x, y)..at(x, y) + 3].copy_from_slice(&INACTIVE);
        }
        assert_eq!(inactive(&output), Ok(true));
        assert_eq!(locked_or_inactive(&output), Ok(true));
        let lock = frame(&lock_surface_on("td", &[]));
        assert_eq!(locked_or_inactive(&lock), Ok(true));
        assert_eq!(inactive(&lock), Ok(false));
        // Black alone is the lock palette's, not QEMU's output.
        assert_eq!(inactive(&black), Ok(false));
        for (x, y, colour) in [
            (100, 390, INACTIVE),
            (511, 390, INACTIVE),
            (768, 390, INACTIVE),
            (640, 383, INACTIVE),
            (640, 400, INACTIVE),
            (640, 390, [0xab, 0xaa, 0xaa]),
        ] {
            let mut other = output.clone();
            other[at(x, y)..at(x, y) + 3].copy_from_slice(&colour);
            assert_eq!(inactive(&other), Ok(false), "{x} {y}");
            assert_eq!(locked_or_inactive(&other), Ok(false), "{x} {y}");
        }
        assert!(locked_or_inactive(&output[3..]).is_err());
    }

    /// From the first touch request to `release`, and from a killed
    /// compositor to its successor's lock, only lock pixels or QEMU's
    /// inactive output may show; `release` ends the hold, since the guest
    /// completes the touch only after it.
    #[test]
    fn the_touch_and_kill_windows_hold_lock_pixels() {
        let booted = |pixels: &[u8], arguments: &[&str]| install("id", pixels, arguments);
        let checks = login_system_checks(&booted);
        let hold = |name: &str| {
            checks
                .iter()
                .find(|screen| screen.name == name)
                .unwrap()
                .then
        };
        let lock = frame(&lock_surface_on("td", &[]));
        let mut client = lock.clone();
        client[..3].copy_from_slice(&[0x12, 0x34, 0x56]);
        for name in ["touch", "killed", "pin", "locked"] {
            let holds = hold(name).unwrap_or_else(|| panic!("{name} holds nothing"));
            assert_eq!(holds(&lock), Ok(true), "{name}");
            assert_eq!(holds(&client), Ok(false), "{name}");
        }
        assert!(hold("release").is_none());
        // The guest asks for `release` after the touch requests and before
        // the client's window, in every unlock.
        let source = include_str!("../../../../../../../td-secret/src/login_system_vm.rs");
        let unlock = source.split("\nfn unlock(").nth(1).unwrap();
        let unlock = unlock.split("\n}\n").next().unwrap();
        let at = |needle: &str| unlock.find(needle).unwrap();
        assert!(at("host.screen(\"touch\"") < at("host.screen(\"release\""));
        assert!(at("host.screen(\"release\"") < at("key.release_touch()"));
        assert!(at("key.release_touch()") < at("host.screen(\"unlocked\""));
        assert!(unlock.contains("Presence::Held"));
        // The chord's first report is the first input after the wake.
        let suspend = source.split("\nfn suspend(").nth(1).unwrap();
        let after = suspend
            .split("fs::write(\"/sys/power/state\", b\"mem\").unwrap();")
            .nth(1)
            .unwrap();
        assert!(after
            .trim_start()
            .lines()
            .find(|line| line.contains("keyboard."))
            .unwrap()
            .contains("keyboard.report(5, ESCAPE);"));
    }

    #[test]
    fn the_system_screens_are_whole_frames() {
        let enrolled = frame(&lock_surface_on("td", &["PRESS CTRL+ALT+ESC TO UNLOCK"]));
        assert_eq!(locked(&enrolled, &[]), Ok(true));
        assert_eq!(locked_record(&enrolled, &[]), Ok(false));
        let desktop_host = frame(&lock_surface(&["PRESS CTRL+ALT+ESC TO UNLOCK"]));
        assert_eq!(locked(&desktop_host, &[]), Ok(false));
        for (surface, notice, own, own_notice) in [
            (
                frame(&lock_surface_on("td", RECORD)),
                frame(&attention(RECORD)),
                &locked_record as &dyn Fn(&[u8], &[&str]) -> Result<bool, String>,
                &record as &dyn Fn(&[u8], &[&str]) -> Result<bool, String>,
            ),
            (
                frame(&lock_surface_on("td", UNREADABLE)),
                frame(&attention(UNREADABLE)),
                &locked_unreadable,
                &unreadable,
            ),
            (
                frame(&lock_surface_on("td", DAMAGED)),
                frame(&attention(DAMAGED)),
                &locked_damaged,
                &damaged,
            ),
        ] {
            assert_eq!(own(&surface, &[]), Ok(true));
            assert_eq!(own(&notice, &[]), Ok(false));
            assert_eq!(own_notice(&notice, &[]), Ok(true));
            assert_eq!(locked(&surface, &[]), Ok(false));
            assert!(blank_or_lock(&surface).unwrap() && blank_or_lock(&notice).unwrap());
        }
        // The test face has no parenthesis either: `E`'s row drawn short.
        let drawable: Vec<(usize, String)> = menu_rows()
            .into_iter()
            .map(|(top, text)| match text.split_once(" (") {
                Some((head, _)) => (top, head.to_string()),
                None => (top, text),
            })
            .collect();
        let menu_frame = frame(&drawable);
        assert_eq!(menu(&menu_frame, &[]), Ok(true));
        let mut without_l = drawable.clone();
        without_l.retain(|(_, text)| !text.starts_with("L:"));
        assert_eq!(menu(&frame(&without_l), &[]), Ok(false));
        assert_eq!(menu(&enrolled, &[]), Ok(false));
        assert!(menu(&menu_frame, &["extra"]).is_err());
        let refused = frame(&attention(&["UPDATE CANNOT READ LOGIN KEYS"]));
        assert_eq!(update_refused(&refused, &[]), Ok(true));
        assert_eq!(update_refused(&menu_frame, &[]), Ok(false));
        let booted = "a".repeat(64);
        assert!(install(&booted, &refused, &[]).is_err());
        assert!(install(&booted, &refused, &["not-an-id"]).is_err());
        assert_eq!(install(&booted, &refused, &[&booted]), Ok(false));
        // The guest's ID must be the host's.
        assert!(install(&booted, &refused, &[&"b".repeat(64)])
            .unwrap_err()
            .contains("not the booted deployment"));
        assert_eq!(killed(&refused, &[]), Ok(true));
        let mut client = refused.clone();
        client[..3].copy_from_slice(&[0x12, 0x34, 0x56]);
        assert_eq!(killed(&client, &[]), Ok(false));
        assert!(killed(&refused, &["extra"]).is_err());
    }

    /// The key-management screens: each notice matches its own screen,
    /// `removing` its count and choice, and a prompt its rows, its time
    /// line and its field; a disclosure's key is read off its row, and a
    /// row that is not one key is no disclosure.
    #[test]
    fn key_management_screens_match_their_rows_and_read_the_key() {
        let keys = frame(&attention(KEYS));
        assert_eq!(notice(&keys, &[], KEYS), Ok(true));
        assert_eq!(notice(&keys, &[], &["LOGIN KEYS ENROLLED"]), Ok(false));
        assert!(notice(&keys, &["extra"], KEYS).is_err());
        let chosen = frame(&attention(&[
            "REMOVE: PRESS 1 TO 8 THEN ENTER",
            "SELECTED: 2 5",
        ]));
        assert_eq!(removing(&chosen, &["8", "2", "5"]), Ok(true));
        assert_eq!(removing(&chosen, &["8"]), Ok(false));
        assert_eq!(removing(&chosen, &["7", "2", "5"]), Ok(false));
        let none = frame(&attention(&[
            "REMOVE: PRESS 1 TO 1 THEN ENTER",
            "SELECTED: NONE",
        ]));
        assert_eq!(removing(&none, &["1"]), Ok(true));
        for bad in [
            &[][..],
            &["0"],
            &["9"],
            &["8", "5", "2"],
            &["8", "9"],
            &["2", "2", "2"],
        ] {
            assert!(removing(&none, bad).is_err(), "{bad:?}");
        }
        let rows = [
            "TD_SECURE_ATTENTION",
            "SESSION_USER_1000",
            "REMOVE_A_LOGIN_KEY_(1_->_0_KEYS)",
            "ACCOUNT_UID_1000",
            "REMOVE:_1:3fa2c1d0",
            "CONNECT_ONLY_ONE_ENROLLED_KEY",
            "LOGIN_WILL_NOT_NEED_A_KEY",
            "CONSOLE_LOGIN_AND_THE_ORDINARY",
            "SSH_POLICY_RETURN",
            "OPEN_SSH_SESSIONS_END_NOW",
            "KEY",
            "ESC:_CANCEL",
        ];
        let shown: Vec<String> = rows
            .iter()
            .map(|row| match *row {
                "KEY" => "APPROVE: TYPE 4 THEN 7".to_string(),
                row => row.replace('_', " "),
            })
            .collect();
        let screen = prompt_frame(&shown, 233, None);
        assert_eq!(disclosure_key(&screen, &rows), Ok(Some("47".into())));
        assert_eq!(disclosed(&screen, &rows), Ok("47".into()));
        assert_eq!(disclosure(&screen, &rows), Ok(true));
        // Another row, or no key row at all, is not this screen.
        let mut other = rows.to_vec();
        other[2] = "REMOVE_A_LOGIN_KEY_(2_->_1_KEY)";
        assert_eq!(disclosure_key(&screen, &other), Ok(None));
        assert!(disclosed(&screen, &other).is_err());
        assert!(disclosure_key(&screen, &rows[..10]).is_err());
        let doubled = [rows.to_vec(), vec!["KEY"]].concat();
        assert!(disclosure_key(&screen, &doubled).is_err());
        // A step's rows, its field's masks and its touch request.
        let plain = &rows[..6];
        let lines: Vec<String> = plain.iter().map(|row| row.replace('_', " ")).collect();
        let shown = prompt_frame(&lines, 120, None);
        assert_eq!(step(&shown, plain), Ok(true));
        assert_eq!(step_touch(&shown, plain), Ok(false));
        let field = prompt_frame(&lines, 1, Some(("ENTER THE PIN FOR THIS KEY", 3)));
        let mut masked = vec!["3"];
        masked.extend(plain);
        assert_eq!(step_pin(&field, &masked), Ok(true));
        masked[0] = "2";
        assert_eq!(step_pin(&field, &masked), Ok(false));
        assert_eq!(step(&field, plain), Ok(false));
        let touch = prompt_frame(&lines, 360, Some(("TOUCH YOUR KEY", 0)));
        assert_eq!(step_touch(&touch, plain), Ok(true));
        assert!(step(&touch, &[]).is_err());
        assert!(step(&touch, &["TWO WORDS"]).is_err());
    }
}
