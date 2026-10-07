//! `qemu-login-system` (td-secret/DESIGN.md, "Login system guest"): the
//! TPM-free full-system login guest. The test-only system image boots
//! once per phase on one disposable volume whose files are guest root's,
//! on a q35 machine whose ACPI offers suspend to RAM, its one ignored test
//! asking for the display checks below over ttyS0.

use super::super::guest_screens::{Check, GuestScreens, Holds, Screen, Wake};
use super::*;

/// The lock surface's and the attention screen's rows for each cause the
/// guest shows (td-login/TOKEN-LOGIN.md, "The login record").
const RECORD: &[&str] = &["LOGIN KEY STATE UNAVAILABLE:", "RECORD DAMAGED"];
const UNREADABLE: &[&str] = &["LOGIN KEY STATE UNAVAILABLE:", "STATE COULD NOT BE READ"];
/// The attention menu, as the update oracle reads it from 276: its
/// selections 36 apart, and its last row one lower than other screens'.
const MENU: &[&str] = &[
    "TD SECURE ATTENTION",
    "U: UNLOCK  R: RECOVERY TOKEN",
    "E: ENROLL TWO TOKENS (HAVE BOTH READY)",
    "X: ENROLL WITHOUT RECOVERY - LOSS IS FINAL",
    "W: REVIEW PENDING CREDENTIAL WRITE",
    "I: REVIEW PENDING SYSTEM INSTALLATION",
    "K: LOGIN KEYS",
    "L: LOCK SCREEN",
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

/// Each screen's check and hold: while locked, only what may show while
/// locked, through both touch requests to `release`, after which the
/// guest completes the touch; after an unlock no lock pixel until the
/// desktop. The menu holds the lock palette until the screen it leads to;
/// a refusal, the prompt and the desktop hold nothing, since Escape
/// returns to the desktop. `killed` holds from the old compositor's
/// frame's end to its successor's lock. `asleep` is the desktop the guest
/// suspends from, and its wake holds what may show while locked (`run`).
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
    ]
}

/// What one phase's console must show beside its screens: its pass line,
/// QEMU's clean exit, and boot health behind the lock; the serial
/// greeter's one refusal line and no shell on every boot but the
/// unenrolled seed's, which logs in; and rootcheck's login directory
/// marker on every boot but a damaged directory's.
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
    let enrolled = phase != "seed";
    let refusals = lines(fixture::CONSOLE_REFUSED);
    let greeted = lines(GREETER_MARKER) > 0;
    let directory = !phase.starts_with("damaged-directory-");
    let why = if !result.evidence.target || failed {
        "the guest's test did not pass".to_string()
    } else if !result.exited_clean || result.marker_killed || result.evidence.kernel_panic {
        "QEMU did not exit cleanly after the guest's reboot".into()
    } else if !result.evidence.boot_success {
        format!("boot health did not complete ({SYSTEM_BOOT_SUCCESS_MARKER:?} absent)")
    } else if enrolled && (refusals != 1 || greeted) {
        format!(
            "the serial greeter printed its refusal {refusals} times and greeted {greeted}, \
             not once and never"
        )
    } else if !enrolled && (refusals != 0 || !greeted) {
        "the unenrolled seed's serial greeter did not log in".into()
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
        };
        let tokens = format!("td.hid-fixture=1 td.login-system={phase}");
        let marker = format!("secret-fixture: {} {phase}", fixture::LOGIN_SYSTEM_PASS);
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
                kill_on_marker: false,
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
        validate_persistent_shutdown(&result, phase)?;
        check_persistent_volume(&btrfs, &volume)?;
        println!(
            "[qemu-login-system] {phase} passed in {:.2}s",
            result.elapsed.as_secs_f64()
        );
    }
    println!(
        "PASS: a full TPM-free system enrolled through the worker over its root-owned volume's \
         marked deployment, booted locked behind its client with the serial greeter refused and \
         boot health complete, unlocked with each key, relocked by Super+l, L and a killed \
         compositor, refused unmarked and wrong-version updates and admitted its own, showed \
         each unavailable cause and recovered; after QEMU S3 the session was locked when the \
         first post-wake input was routed, in the same generation, and after its first \
         change following the wake the display showed only QEMU's inactive output or lock \
         pixels; UHID key and keyboard, simulated \
         enrollment root, no lock surface seen after the wake (virtio-gpu has no restore), no \
         physical-presence, lid or hardware suspend claim"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
            fixture::login_system_screens("seed"),
            ["blank-unlocked", "desktop"]
        );
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
}
