use std::fs;
use std::io::{self, Read};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const MEMBERSHIP: &str = "/td-user-1000/test-0123456789abcdef";

struct ProcessStat {
    state: char,
    parent: u32,
    process_group: i64,
    session: i64,
    terminal: i64,
    starttime: u64,
}

#[test]
fn a_stale_bootstrap_snapshot_cannot_kill_the_cleanup_watcher() -> io::Result<()> {
    if effective_user_id()? == 0 {
        return Ok(());
    }

    let mut bootstrap = Command::new(env!("CARGO_BIN_EXE_td-jail"))
        .args(["--internal-cgroup-cleanup", MEMBERSHIP])
        .env_clear()
        .current_dir("/")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let stale_snapshot = bootstrap.id();
    let keepalive = bootstrap
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("cleanup bootstrap has no keepalive writer"))?;
    let mut readiness = bootstrap
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("cleanup bootstrap has no readiness reader"))?;
    let mut ready = [0_u8; 1];
    readiness.read_exact(&mut ready)?;
    if ready != [1] {
        return Err(io::Error::other("cleanup watcher sent invalid readiness"));
    }

    let (watcher, before) = only_child(stale_snapshot)?;
    if before.process_group != i64::from(watcher)
        || before.session != i64::from(watcher)
        || before.terminal != 0
    {
        return Err(io::Error::other(format!(
            "cleanup watcher is not detached: pid={watcher}, process-group={}, session={}, terminal={}",
            before.process_group, before.session, before.terminal
        )));
    }

    bootstrap.kill()?;
    let _status = bootstrap.wait()?;
    let after = process_stat(watcher)?;
    if after.starttime != before.starttime {
        return Err(io::Error::other(
            "cleanup watcher pid changed after the stale bootstrap signal",
        ));
    }
    if after.state == 'Z' {
        return Err(io::Error::other(
            "cleanup watcher exited before its keepalive reached EOF",
        ));
    }

    drop(keepalive);
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match fs::symlink_metadata(format!("/proc/{watcher}")) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
            Ok(_) => match process_stat(watcher) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
                Err(error) => return Err(error),
                Ok(stat) if stat.starttime != before.starttime => {
                    return Err(io::Error::other(
                        "cleanup watcher pid was reused after keepalive EOF",
                    ));
                }
                Ok(stat) if stat.state == 'Z' => return Ok(()),
                Ok(_) if Instant::now() >= deadline => {
                    return Err(io::Error::other(
                        "cleanup watcher stayed live after its keepalive reached EOF",
                    ));
                }
                Ok(_) => std::thread::sleep(Duration::from_millis(5)),
            },
        }
    }
}

fn effective_user_id() -> io::Result<u32> {
    let status = fs::read_to_string("/proc/self/status")?;
    status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))
        .and_then(|ids| ids.split_whitespace().nth(1))
        .ok_or_else(|| io::Error::other("/proc/self/status has no effective uid"))?
        .parse()
        .map_err(|error| io::Error::other(format!("invalid effective uid: {error}")))
}

fn only_child(parent: u32) -> io::Result<(u32, ProcessStat)> {
    let mut child = None;
    for entry in fs::read_dir("/proc")? {
        let name = entry?.file_name();
        let Some(pid) = name.to_str().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        let stat = match process_stat(pid) {
            Ok(stat) => stat,
            // Unrelated processes can exit during the scan. A hidepid
            // mount can also conceal processes owned by other users.
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied
                ) || e.raw_os_error() == Some(3) =>
            {
                continue
            }
            Err(e) => return Err(e),
        };
        if stat.parent != parent {
            continue;
        }
        if child.is_some() {
            return Err(io::Error::other("cleanup bootstrap has multiple children"));
        }
        child = Some((pid, stat));
    }
    child.ok_or_else(|| io::Error::other("cleanup bootstrap has no watcher child"))
}

fn process_stat(pid: u32) -> io::Result<ProcessStat> {
    let stat = fs::read(format!("/proc/{pid}/stat"))?;
    // A process may choose non-UTF-8 bytes and parentheses in its name.
    let fields = stat
        .rsplit(|byte| *byte == b')')
        .next()
        .and_then(|suffix| suffix.strip_prefix(b" "))
        .ok_or_else(|| io::Error::other("process stat has no command terminator"))?;
    let fields = std::str::from_utf8(fields).map_err(io::Error::other)?;
    let mut fields = fields.split_whitespace();
    let state_text = next_field(&mut fields, "state")?;
    let mut state_characters = state_text.chars();
    let state = state_characters
        .next()
        .ok_or_else(|| io::Error::other("process stat state is empty"))?;
    if state_characters.next().is_some() {
        return Err(io::Error::other("process stat state is not one character"));
    }
    let parent = parse_field(&mut fields, "parent")?;
    let process_group = parse_field(&mut fields, "process group")?;
    let session = parse_field(&mut fields, "session")?;
    let terminal = parse_field(&mut fields, "terminal")?;
    for name in [
        "terminal process group",
        "flags",
        "minor faults",
        "child minor faults",
        "major faults",
        "child major faults",
        "user time",
        "system time",
        "child user time",
        "child system time",
        "priority",
        "nice",
        "thread count",
        "interval timer",
    ] {
        let _ignored = next_field(&mut fields, name)?;
    }
    let starttime = parse_field(&mut fields, "start time")?;
    Ok(ProcessStat {
        state,
        parent,
        process_group,
        session,
        terminal,
        starttime,
    })
}

fn next_field<'a>(fields: &mut impl Iterator<Item = &'a str>, name: &str) -> io::Result<&'a str> {
    fields
        .next()
        .ok_or_else(|| io::Error::other(format!("process stat has no {name} field")))
}

fn parse_field<'a, T>(fields: &mut impl Iterator<Item = &'a str>, name: &str) -> io::Result<T>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    next_field(fields, name)?
        .parse()
        .map_err(|error| io::Error::other(format!("invalid process stat {name}: {error}")))
}
