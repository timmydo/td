use std::fs;
use std::io::{self, Read};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const MEMBERSHIP: &str = "/td-user-1000/test-0123456789abcdef";

struct ProcessStat {
    state: char,
    process_group: u32,
    session: u32,
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

    let children = fs::read_to_string(format!(
        "/proc/{stale_snapshot}/task/{stale_snapshot}/children"
    ))?;
    let mut child_pids = children.split_whitespace();
    let watcher: u32 = child_pids
        .next()
        .ok_or_else(|| io::Error::other("cleanup bootstrap has no watcher child"))?
        .parse()
        .map_err(|error| io::Error::other(format!("invalid cleanup watcher pid: {error}")))?;
    if child_pids.next().is_some() || watcher == stale_snapshot {
        return Err(io::Error::other(
            "cleanup watcher is not the bootstrap's one distinct child",
        ));
    }
    let before = process_stat(watcher)?;
    if before.process_group != watcher || before.session != watcher || before.terminal != 0 {
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

fn process_stat(pid: u32) -> io::Result<ProcessStat> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let (_, fields) = stat
        .rsplit_once(") ")
        .ok_or_else(|| io::Error::other("process stat has no command terminator"))?;
    let mut fields = fields.split_whitespace();
    let state_text = next_field(&mut fields, "state")?;
    let mut state_characters = state_text.chars();
    let state = state_characters
        .next()
        .ok_or_else(|| io::Error::other("process stat state is empty"))?;
    if state_characters.next().is_some() {
        return Err(io::Error::other("process stat state is not one character"));
    }
    let _parent = next_field(&mut fields, "parent")?;
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

fn parse_field<'a, T>(
    fields: &mut impl Iterator<Item = &'a str>,
    name: &str,
) -> io::Result<T>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    next_field(fields, name)?
        .parse()
        .map_err(|error| io::Error::other(format!("invalid process stat {name}: {error}")))
}
