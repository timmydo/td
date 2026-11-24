//! The primary account's own td-admin: a local update approved on secure
//! attention's `H`, the read-only status socket, a refused `cancel`, and the
//! menu's `U` (td-update/ADMIN.md, "Local consent" and "The attention
//! menu's U"). It runs before the SSH exercise, whose release server is not
//! yet listening, so the approved job installs nothing; the admin key
//! cancels `U`'s before it can.
use super::super::{
    hostname_row, menu_pixels_match, menu_row_matches, prompt_shown, selectors, APPROVAL_WAIT,
    HOSTNAME_TOP, HOSTNAME_WAITING,
};
use super::*;
use std::thread;

/// Where the waiting client writes, in the session user's home.
const OUTPUT: &str = "/var/home/tester/td-admin-local-request";
/// The fixture's admin key, which only cancels the `U` job here.
const KEY: &str = "/var/home/tester/.td-admin-oracle/key";
const WORDS: &str = "admin-update update --no-reboot";

/// Whether `pixels` are tag 46's prompt for a signed release that does
/// not restart (td-authd/src/consent.rs): as many rows as tag 12's.
fn release_prompt_shown(pixels: &[u8]) -> Result<bool> {
    prompt_shown(
        pixels,
        HOSTNAME_TOP,
        &[
            "INSTALL THE LATEST SIGNED RELEASE",
            "REQUESTER UID 1000",
            "FROM THE CONFIGURED PUBLISHER",
            "THE VERSION IS CHOSEN WHEN IT RUNS",
            "DOES NOT RESTART",
        ],
    )
}

/// Where the primary account's `cancel` is refused (td-update/src/admin.rs).
const REFUSED: &str = "connect to admin service (use the dedicated admin SSH key)";

/// `ls -lnd`'s permissions, owner and group for `path`: the image has no
/// `stat`.
fn owner_and_mode(guest: &mut Guest, path: &str) -> Result<String> {
    let line = guest.scalar(&format!("echo TD-STAT=$(ls -lnd {path})"), |line| {
        line.starts_with("TD-STAT=")
    })?;
    let mut fields = line.trim_start_matches("TD-STAT=").split_whitespace();
    match (fields.next(), fields.next(), fields.next(), fields.next()) {
        (Some(mode), Some(_), Some(uid), Some(gid)) => Ok(format!("{mode} {uid}:{gid}")),
        _ => Err(format!("ls -lnd {path} gave {line}")),
    }
}

/// `td-admin WORDS` as the primary account: its exit status and output.
/// Each verb it runs only reads or is refused, so a reply the kernel's
/// console split, its status not a whole number, is asked again.
fn admin(guest: &mut Guest, words: &str) -> Result<(u8, Vec<String>)> {
    for _ in 0..3 {
        let mut lines = guest.command(&format!(
            "/bin/td-admin {words} 2>&1; echo TD-ADMIN-LOCAL=$?"
        ))?;
        let Some(status) = lines
            .iter()
            .rev()
            .find_map(|line| line.strip_prefix("TD-ADMIN-LOCAL="))
            .and_then(|status| status.trim_end().parse().ok())
        else {
            continue;
        };
        lines.retain(|line| !line.starts_with("TD-ADMIN-LOCAL="));
        return Ok((status, lines));
    }
    Err(format!("td-admin {words} gave no status"))
}

/// The latest job's ID and phase, from its status's first line; none
/// before the service's first job.
fn latest(lines: &[String]) -> Option<(u64, &str)> {
    lines.iter().find_map(|line| {
        let (id, phase) = line.strip_prefix("job ")?.split_once(": ")?;
        Some((id.parse().ok()?, phase))
    })
}

/// The latest job through the primary account's status socket once it is
/// later than job `after` and has ended. `failed` installs nothing; that
/// it was the kind asked shows in its queued line or one of `phases`,
/// whichever it last recorded (`submit` already required the queued job to
/// be exactly the kind and reboot choice approved).
fn ended(guest: &mut Guest, after: u64, phases: &[&str]) -> Result<u64> {
    let end = Instant::now() + Duration::from_secs(600);
    let mut seen = false;
    loop {
        let (status, lines) = admin(guest, "status")?;
        if status != 0 {
            return Err(format!("td-admin status as the primary account: {lines:?}"));
        }
        if let Some((id, phase)) = latest(&lines).filter(|(id, _)| *id > after) {
            // Each phase is one status line: the first, or `Prior state`'s.
            seen |= lines
                .iter()
                .any(|line| phases.iter().any(|phase| line.contains(phase)));
            if phase == "installed" || phase == "installing" {
                return Err(format!("job {id} installed: {lines:?}"));
            }
            if phase == "failed" {
                if !seen {
                    return Err(format!("job {id} is not the kind asked: {lines:?}"));
                }
                return Ok(id);
            }
        }
        if Instant::now() >= end {
            return Err(format!("no job after {after} ended: {lines:?}"));
        }
        thread::sleep(Duration::from_secs(1));
    }
}

pub(super) fn run(guest: &mut Guest, work: &Path) -> Result<()> {
    let capture = work.join("admin-local.ppm");
    let uid = guest.scalar("echo TD-ORACLE-UID=$(id -u)", |line| {
        line.starts_with("TD-ORACLE-UID=")
    })?;
    if !boot::reports_marker(&uid, "TD-ORACLE-UID=1000") {
        return Err(format!("the serial session is not UID 1000: {uid}"));
    }
    // The authority boundary is the sockets' owners and modes; the control
    // socket's directory is not even searchable here.
    for (path, expected) in [
        ("/run/td-admin", "drwx--x--- 0:986"),
        ("/run/td-admin-status", "drwxr-x--- 0:1000"),
        ("/run/td-admin-status/status", "srw-rw---- 0:1000"),
    ] {
        // The kernel's console shares the serial line and can split a
        // reply, so a mismatch or an unreadable reply is read again
        // before it counts.
        let mut found = Err(String::new());
        for _ in 0..5 {
            found = owner_and_mode(guest, path);
            if found.as_deref() == Ok(expected) {
                break;
            }
            thread::sleep(Duration::from_secs(1));
        }
        match found {
            Ok(found) if found == expected => {}
            Ok(found) => return Err(format!("{path} is {found}, not {expected}")),
            Err(error) => return Err(error),
        }
    }
    let control = guest.scalar(
        "if test -e /run/td-admin/control; then echo TD-CONTROL=seen; else echo TD-CONTROL=hidden; fi",
        |line| line.starts_with("TD-CONTROL="),
    )?;
    if !boot::reports_marker(&control, "TD-CONTROL=hidden") {
        return Err("the primary account can reach the control socket".into());
    }
    println!("TD-ADMIN-LOCAL-SOCKETS");
    let before = selectors(guest)?;
    // The fresh fixture disk has no job yet, so this is 0; were the reply
    // garbled, 0 still only admits the next job.
    let (_, lines) = admin(guest, "status")?;
    let first = latest(&lines).map_or(0, |(id, _)| id);
    // `update` queues on the settings intake and waits for its receipt.
    guest.command(&format!(
        "(/bin/td-admin update --no-reboot; echo TD-ADMIN-CLIENT=$?) > {OUTPUT} 2>&1 & true"
    ))?;
    let waits =
        format!("A settings change to {WORDS} waits. Press Ctrl+Alt+Escape, then H to review it.");
    let end = Instant::now() + Duration::from_secs(30);
    loop {
        let lines = guest.command(&format!("cat {OUTPUT}"))?;
        if lines.iter().any(|line| boot::reports_marker(line, &waits)) {
            break;
        }
        if lines
            .iter()
            .any(|line| line.starts_with("TD-ADMIN-CLIENT="))
            || Instant::now() >= end
        {
            return Err(format!("the local update was not admitted: {lines:?}"));
        }
        thread::sleep(Duration::from_millis(500));
    }
    guest.hostname_menu(&capture, &hostname_row(HOSTNAME_WAITING, 0))?;
    guest.key(&["h"])?;
    guest.screen_until(
        &capture,
        Duration::from_secs(20),
        "tag 46's signed-release prompt",
        release_prompt_shown,
    )?;
    guest.approve()?;
    // The screen says so and stays until Escape; a chord would close it.
    guest.screen_until(
        &capture,
        Duration::from_secs(30),
        "the submitted notice",
        |pixels| {
            Ok(menu_row_matches(pixels, 276, "TD SECURE ATTENTION")?
                && menu_row_matches(pixels, 312, "UPDATE JOB SUBMITTED")?)
        },
    )?;
    guest.key(&["esc"])?;
    guest.screen_until(
        &capture,
        Duration::from_secs(20),
        "attention closing",
        |pixels| Ok(!menu_row_matches(pixels, 276, "TD SECURE ATTENTION")?),
    )?;
    let completed =
        format!("Settings change completed: {WORDS}. The admin service started that job.");
    let end = Instant::now() + Duration::from_secs(60);
    let lines = loop {
        let lines = guest.command(&format!("cat {OUTPUT}"))?;
        if let Some(status) = lines
            .iter()
            .find_map(|line| line.strip_prefix("TD-ADMIN-CLIENT="))
        {
            if !boot::reports_marker(status, "0") {
                return Err(format!("the approved local update failed: {lines:?}"));
            }
            break lines;
        }
        if Instant::now() >= end {
            return Err(format!("the approved client did not exit: {lines:?}"));
        }
        thread::sleep(Duration::from_millis(500));
    };
    if !lines
        .iter()
        .any(|line| boot::reports_marker(line, &completed))
    {
        return Err(format!(
            "the approved client did not report its job: {lines:?}"
        ));
    }
    println!("TD-ADMIN-LOCAL-APPROVED");
    // Followed through the status socket; nothing serves the release yet.
    let local = ended(
        guest,
        first,
        &["queued update", "downloading signed release"],
    )?;
    let (status, lines) = admin(guest, "logs")?;
    if status != 0 {
        return Err(format!("td-admin logs as the primary account: {lines:?}"));
    }
    // Refused by the control socket's permissions, not for another reason.
    let (status, lines) = admin(guest, "cancel")?;
    if status == 0 || !lines.iter().any(|line| line.contains(REFUSED)) {
        return Err(format!(
            "the primary account's cancel was not refused at the socket: {status} {lines:?}"
        ));
    }
    if selectors(guest)? != before {
        return Err("the failed local update changed the selectors".into());
    }
    println!("TD-ADMIN-LOCAL-STATUS-ONLY");
    // `U` on the menu once it has been on glass past the settle.
    guest.key(&["ctrl", "alt", "esc"])?;
    guest.screen_until(
        &capture,
        Duration::from_secs(20),
        "secure-attention menu for U",
        menu_pixels_match,
    )?;
    thread::sleep(APPROVAL_WAIT);
    guest.key(&["u"])?;
    guest.screen_until(
        &capture,
        Duration::from_secs(30),
        "U's started notice",
        |pixels| {
            Ok(menu_row_matches(pixels, 276, "TD SECURE ATTENTION")?
                && menu_row_matches(pixels, 312, "UPDATE FROM MAIN STARTED")?
                && menu_row_matches(pixels, 348, "IT INSTALLS FOR THE NEXT BOOT")?
                && menu_row_matches(pixels, 384, "TD-ADMIN STATUS FOLLOWS IT")?)
        },
    )?;
    guest.key(&["esc"])?;
    guest.screen_until(
        &capture,
        Duration::from_secs(20),
        "attention closing after U",
        |pixels| Ok(!menu_row_matches(pixels, 276, "TD SECURE ATTENTION")?),
    )?;
    // `submit` saw it queued; the guest may reach upstream, so the admin
    // key ends the job here, whatever source phase it is in.
    guest.command(&format!(
        "cp /run/td-volume/td/admin-fixture/ssh-key {KEY} && chmod 600 {KEY}"
    ))?;
    super::public::ssh(guest, "cancel")?;
    ended(
        guest,
        local,
        &["queued source-update", "fetching main", "building"],
    )?;
    if selectors(guest)? != before {
        return Err("U's cancelled job changed the selectors".into());
    }
    println!("TD-ADMIN-LOCAL-U");
    Ok(())
}

#[cfg(test)]
mod tests {
    /// Every row, line and phase the phase waits on is its owner's own.
    #[test]
    fn the_local_phase_reads_its_owners_own_text() {
        let consent = include_str!("../../../../../../../../td-authd/src/consent.rs");
        for row in [
            "\"INSTALL THE LATEST SIGNED RELEASE\",",
            "\"FROM THE CONFIGURED PUBLISHER\",",
            "\"THE VERSION IS CHOSEN WHEN IT RUNS\",",
            "\"DOES NOT RESTART\"",
            "lines.push(format!(\"REQUESTER UID {requester}\"));",
        ] {
            assert!(consent.contains(row), "{row}");
        }
        let attention = include_str!("../../../../../../../../td-compositor/src/attention.rs");
        for row in [
            "Notice::UpdateStarted => \"UPDATE FROM MAIN STARTED\",",
            "Notice::AdminUpdateSubmitted => \"UPDATE JOB SUBMITTED\",",
            "\"IT INSTALLS FOR THE NEXT BOOT\",",
            "\"TD-ADMIN STATUS FOLLOWS IT\",",
        ] {
            assert!(attention.contains(row), "{row}");
        }
        let client = include_str!("../../../../../../../../td-authd/src/set_hostname.rs");
        for line in [
            "\"A settings change to {} waits. Press Ctrl+Alt+Escape, then H to review it.\",",
            "\"Settings change completed: {}. The admin service started that job.\",",
        ] {
            assert!(client.contains(line), "{line}");
        }
        let admin = include_str!("../../../../../../../../td-update/src/admin.rs");
        for text in [
            "Request::Update { reboot: false } => Some(\"admin-update update --no-reboot\"),",
            "Request::Update { reboot } => (reboot, \"update\"),",
            "Request::SourceUpdate { reboot } => (reboot, \"source-update\"),",
            "let text = format!(\"job {id}: queued {operation}\\nreboot {reboot}\\n\");",
            "let text = format!(\"job {}: {phase}\\n{}\\n\", job.id, detail);",
            "record(shared, \"downloading signed release\", endpoint)?;",
            "record(shared, \"fetching main\", UPSTREAM)?;",
            "        \"building\",\n        &format!(\"commit {revision}\\nreboot {reboot}\"),",
            "record(&worker, \"failed\", &format!(\"{error}\\nPrior state: {prior}\"));",
            "\"connect to admin service (use the dedicated admin SSH key)\"",
            "const STATUS_DIRECTORY: &str = \"/run/td-admin-status\";",
            "const STATUS_SOCKET: &str = \"/run/td-admin-status/status\";",
        ] {
            assert!(admin.contains(text), "{text}");
        }
        assert_eq!(super::WORDS, "admin-update update --no-reboot");
        assert_eq!(
            super::REFUSED,
            "connect to admin service (use the dedicated admin SSH key)"
        );
        let lines = ["x".to_string(), "job 12: fetching main".to_string()];
        assert_eq!(super::latest(&lines), Some((12, "fetching main")));
        assert_eq!(super::latest(&["job x: failed".to_string()]), None);
    }
}
