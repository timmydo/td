//! The installed-image workspace-account oracle. No model or credentials
//! are used: it drives the same Bench and conversation deletion as the UI.

use crate::workspace_slot::error;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
fn joined(base: &Path, name: &str) -> Result<PathBuf, String> {
    crate::workspace_slot::joined(base, name).map_err(error)
}
use crate::host::{Call, Client, Up};
use crate::store::{Conversation, Id, Role, StateDir};

pub const MARKER: &str = crate::workload_accounts::WORKSPACE_ORACLE_MARKER;

fn need(ok: bool, why: &'static str) -> Result<(), String> {
    if ok {
        Ok(())
    } else {
        Err(error(why))
    }
}
fn string(text: &str) -> Result<String, String> {
    td_alloc::string(text).map_err(error)
}
fn path(path: &Path) -> Result<String, String> {
    string(
        path.to_str()
            .ok_or_else(|| error("oracle path is not UTF-8"))?,
    )
}
fn words(values: &[&str]) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    for value in values {
        td_alloc::push(&mut out, string(value)?).map_err(error)?;
    }
    Ok(out)
}

pub fn run() -> Result<(), String> {
    let cmdline = fs::read_to_string("/proc/cmdline").map_err(error)?;
    if !cmdline
        .split_whitespace()
        .any(|word| word == "td.autotest=1")
    {
        return Ok(());
    }
    need(
        crate::workspace_slot::product().map_err(error)?,
        "workspace oracle needs installed td",
    )?;
    need(
        fs::metadata("/proc/self").map_err(error)?.uid() == 1000,
        "workspace oracle needs the human account",
    )?;
    let home = fs::canonicalize(std::env::var_os("HOME").ok_or_else(|| error("no HOME"))?)
        .map_err(error)?;
    let fixture = td_fs::TempDir::new_in(&home, "workspace-oracle").map_err(error)?;
    let first = joined(fixture.path(), "first")?;
    let second = joined(fixture.path(), "second")?;
    for dir in [&first, &second] {
        fs::create_dir(dir).map_err(error)?;
    }
    let secret = joined(fixture.path(), "ungranted")?;
    fs::write(&secret, b"world readable but ungranted").map_err(error)?;
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o644)).map_err(error)?;
    let state = StateDir::from_env(std::env::var_os("XDG_STATE_HOME"), std::env::var_os("HOME"))?;
    state.ensure()?;
    let a = Id::random()?;
    let b = Id::random()?;
    // These are real conversation records, owned and deleted through the
    // production store. The only temporary grant trees have TempDir owners.
    let result = exercise(&state, &a, &b, &first, &second, &secret);
    let clean_a = state.delete(&a);
    let clean_b = state.delete(&b);
    result?;
    need(
        clean_a?.is_none() && clean_b?.is_none(),
        "workspace oracle cleanup did not finish",
    )?;
    writeln!(std::io::stdout().lock(), "{MARKER}").map_err(error)
}

fn exercise(
    state: &StateDir,
    a: &Id,
    b: &Id,
    first: &Path,
    second: &Path,
    secret: &Path,
) -> Result<(), String> {
    let wa = crate::workspace::Workspace::Directory(PathBuf::from(path(first)?));
    let wb = crate::workspace::Workspace::Directory(PathBuf::from(path(second)?));
    let (ca, _) = Conversation::create(state, a, Role::Conversation, Some(wa), Duration::ZERO)?;
    let (cb, _) = Conversation::create(state, b, Role::Conversation, Some(wb), Duration::ZERO)?;
    // Reopen one conversation from the pre-account layout, including all
    // private scratch roots. The other starts with no legacy data.
    let legacy = crate::workspace::jail_dir(state, a);
    td_fs::private_dir(&legacy).map_err(error)?;
    for tree in [
        "home",
        "runtime/tmp",
        "runtime/var-tmp",
        "runtime/shm",
        "runtime/run",
    ] {
        let tree = joined(&legacy, tree)?;
        fs::create_dir_all(&tree).map_err(error)?;
        fs::write(joined(&tree, "upgrade-marker")?, b"old-private-state").map_err(error)?;
    }
    let wa = ca
        .meta()
        .workspace
        .as_ref()
        .ok_or_else(|| error("missing first workspace"))?;
    let wb = cb
        .meta()
        .workspace
        .as_ref()
        .ok_or_else(|| error("missing second workspace"))?;
    let mut ba = crate::bench::Bench::default();
    let mut bb = crate::bench::Bench::default();
    ba.prepare(wa, state, a, &[], &[], None)?;
    bb.prepare(wb, state, b, &[], &[], None)?;
    let aa = crate::workspace_slot::ensure(state, a)?
        .ok_or_else(|| error("first workspace bypassed root"))?;
    let ab = crate::workspace_slot::ensure(state, b)?
        .ok_or_else(|| error("second workspace bypassed root"))?;
    let (pa, _) = crate::workspace::policy(wa, state, a, &[], &[])?;
    need(
        aa.name() != ab.name(),
        "two conversations shared an account",
    )?;
    let ua = probe(&mut ba, first, second, secret, "upgrade")?;
    let ub = probe(&mut bb, second, first, secret, "check")?;
    need(ua != ub, "two slots launched as the same uid")?;
    need(
        fs::metadata(joined(first, "probe")?).map_err(error)?.uid() == 1000,
        "idmapped write was not the human's on disk",
    )?;
    let again =
        crate::workspace_slot::ensure(state, a)?.ok_or_else(|| error("lost persistent slot"))?;
    need(again == aa, "reopening a conversation changed its account")?;

    // An entry which ignores its channel must end when that channel is
    // closed, independently of the tool host's cooperative cancellation.
    let mut held = aa
        .launch(&pa, &words(&["workspace-probe", "hold"])?)
        .map_err(error)?;
    held.set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(error)?;
    let mut ready = [0; 8];
    held.read_exact(&mut ready).map_err(error)?;
    need(&ready == b"holding\n", "nonreading entry did not start")?;
    held.shutdown(std::net::Shutdown::Write).map_err(error)?;
    let mut byte = [0];
    need(
        held.read(&mut byte).map_err(error)? == 0,
        "root did not end the nonreading entry",
    )?;
    drop(held);
    drop(ba);
    drop(bb);
    // Root may still be releasing views just after it closes the socket.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match crate::workspace_slot::delete(state, a) {
            Ok(()) => break,
            Err(_) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            Err(why) => return Err(why),
        }
    }
    // Same conversation obtains the freed slot afresh; neither its home
    // nor any of its persistent scratch may carry the old instance's data.
    let mut fresh = crate::bench::Bench::default();
    let policy = fresh.prepare(wa, state, a, &[], &[], None)?;
    need(
        policy
            .account
            .as_ref()
            .is_some_and(|account| account.name() == aa.name()),
        "emptied slot was not reused",
    )?;
    need(
        probe(&mut fresh, first, second, secret, "fresh")? == ua,
        "slot reuse changed its ledger uid",
    )?;
    drop(fresh);
    Ok(())
}

fn probe(
    bench: &mut crate::bench::Bench,
    own: &Path,
    other: &Path,
    secret: &Path,
    mode: &str,
) -> Result<u32, String> {
    let args = words(&[
        "workspace-probe",
        mode,
        &path(own)?,
        &path(other)?,
        &path(secret)?,
    ])?;
    let call = Call::Serve {
        program: string("/opt/workspace/bin/td-agent")?,
        args,
        workdir: None,
    };
    let mut client = bench.take(&call, 0)?;
    let output = run_call(&mut client, call)?;
    output.trim().parse().map_err(error)
}

fn run_call(client: &mut Client, call: Call) -> Result<String, String> {
    let id = client.call(call)?;
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut output = Vec::new();
    while Instant::now() < deadline {
        match client.next_reply(Duration::from_millis(50)) {
            Some(Ok(Up::RawOutput { id: got, bytes })) if got == id => {
                need(
                    output.len().saturating_add(bytes.len()) <= 1024,
                    "probe said too much",
                )?;
                td_alloc::extend_from_slice(&mut output, &bytes).map_err(error)?;
            }
            Some(Ok(Up::Done { id: got, outcome })) if got == id => {
                let done = outcome?;
                need(done.text == "exit status 0", "workspace probe failed")?;
                return String::from_utf8(output).map_err(error);
            }
            Some(Err(why)) => return Err(why),
            _ => {}
        }
    }
    Err(error("workspace probe timed out"))
}

/// An unprivileged entry used only by the image oracle. It gets no new
/// authority: its caller supplies paths subject to the ordinary jail.
pub fn inside(args: &[String]) -> Result<(), String> {
    match args {
        [mode] if mode == "offline-network" || mode == "host-network" => {
            let offline = mode == "offline-network";
            network_devices(&proc_text("/proc/net/dev")?, offline)?;
            if offline {
                offline_routes(
                    &proc_text("/proc/net/route")?,
                    &proc_text("/proc/net/ipv6_route")?,
                )?;
            }
            Ok(())
        }
        [hold] if hold == "hold" => {
            std::io::stdout().write_all(b"holding\n").map_err(error)?;
            loop {
                std::thread::park();
            }
        }
        [mode, own, other, secret] if mode == "check" || mode == "fresh" || mode == "upgrade" => {
            let uid = fs::metadata("/proc/self").map_err(error)?.uid();
            need(
                (98304..=131071).contains(&uid),
                "workspace still runs as the human",
            )?;
            need(
                fs::read_dir(other).is_err() && fs::read(secret).is_err(),
                "workspace reached an ungranted directory or file",
            )?;
            let home =
                PathBuf::from(std::env::var_os("HOME").ok_or_else(|| error("no private home"))?);
            for marker in [
                joined(&home, "oracle-marker")?,
                PathBuf::from("/tmp/oracle-marker"),
                PathBuf::from("/var/tmp/oracle-marker"),
                PathBuf::from("/dev/shm/oracle-marker"),
                PathBuf::from("/run/oracle-marker"),
            ] {
                let imported = joined(
                    marker
                        .parent()
                        .ok_or_else(|| error("marker has no parent"))?,
                    "upgrade-marker",
                )?;
                if mode == "upgrade" {
                    need(
                        fs::read(&imported).map_err(error)? == b"old-private-state",
                        "old private data was not imported",
                    )?;
                }
                if mode == "fresh" {
                    need(
                        !marker.try_exists().map_err(error)?
                            && !imported.try_exists().map_err(error)?,
                        "slot was reused with old home or scratch contents",
                    )?;
                }
                fs::write(marker, b"private").map_err(error)?;
            }
            fs::write(
                joined(Path::new(own), "probe")?,
                b"written through the view",
            )
            .map_err(error)?;
            writeln!(std::io::stdout().lock(), "{uid}").map_err(error)
        }
        _ => Err(error("invalid workspace probe arguments")),
    }
}

fn proc_text(path: &str) -> Result<String, String> {
    String::from_utf8(td_fs::read_bounded(Path::new(path), 1024 * 1024).map_err(error)?)
        .map_err(error)
}

/// A new network namespace may contain Linux's inert default SIT tunnel.
/// It is not a host interface: offline admits only zero counters for it,
/// then checks both routing tables too. Host mode must find another link.
fn network_devices(text: &str, offline: bool) -> Result<(), String> {
    let mut lo = false;
    let mut sit = false;
    let mut external = false;
    for line in text.lines().skip(2) {
        let (name, counters) = line
            .split_once(':')
            .ok_or_else(|| error("malformed network device"))?;
        match name.trim() {
            "lo" if !lo => lo = true,
            "sit0" if !sit => {
                sit = true;

                need(
                    !offline
                        || (counters.split_ascii_whitespace().count() == 16
                            && counters.split_ascii_whitespace().all(|n| n == "0")),
                    "offline SIT tunnel has traffic",
                )?;
            }
            "lo" | "sit0" => return Err(error("duplicate network device")),
            _ => external = true,
        }
    }
    need(
        lo && external != offline,
        "network namespace has the wrong interfaces",
    )
}

fn offline_routes(ipv4: &str, ipv6: &str) -> Result<(), String> {
    need(
        ipv4.lines()
            .next()
            .is_some_and(|line| line.starts_with("Iface")),
        "missing IPv4 route header",
    )?;
    for line in ipv4.lines().skip(1) {
        need(
            line.split_ascii_whitespace().count() == 11
                && line.split_ascii_whitespace().next() == Some("lo"),
            "offline namespace has an IPv4 route outside loopback",
        )?;
    }
    for line in ipv6.lines() {
        need(
            line.split_ascii_whitespace().count() == 10
                && line.split_ascii_whitespace().last() == Some("lo"),
            "offline namespace has an IPv6 route outside loopback",
        )?;
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    const DEV: &str = "header\nheader\n lo: 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0\n";
    const SIT: &str = " sit0: 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0\n";
    #[test]
    fn inert_tunnel_is_offline_but_not_evidence_of_host_networking() {
        network_devices(DEV, true).unwrap();
        network_devices(&format!("{DEV}{SIT}"), true).unwrap();
        assert!(network_devices(&format!("{DEV}{SIT}"), false).is_err());
        assert!(network_devices(
            &format!("{DEV}{}", SIT.replace("0 0 0 0 0", "1 0 0 0 0")),
            true
        )
        .is_err());
        let host = format!("{DEV}{SIT} eth0: 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0\n");
        assert!(network_devices(&host, true).is_err());
        network_devices(&host, false).unwrap();
        assert!(network_devices("", true).is_err());
        offline_routes("Iface\n", "").unwrap();
        offline_routes("Iface\nlo 0 0 0 0 0 0 0 0 0 0\n", "0 0 0 0 0 0 0 0 0 lo\n").unwrap();
        assert!(offline_routes("Iface\neth0 0 0 0 0 0 0 0 0 0 0\n", "").is_err());
        assert!(offline_routes("Iface\n", "0 0 0 0 0 0 0 0 0 sit0\n").is_err());
        assert!(offline_routes("Iface\n", "0 0 0 0 0 0 0 0 0 eth0\n").is_err());
    }
}
