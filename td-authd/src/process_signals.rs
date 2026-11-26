//! One requester's observed process set, held from admission to delivery.
use crate::workload_accounts::{self as accounts, Kind};
use crate::{allocation, application, refused, text};
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::{fs::MetadataExt, net::UnixStream};
use std::process::Command;

pub(crate) const FRAME: usize = 9000;
const MEMBERS: usize = 256;

struct Member {
    pid: u32,
    parent: u32,
    directory: File,
}

pub(crate) struct Plan {
    members: Vec<Member>,
    uid: u32,
    kind: Kind,
    signal: String,
    summary: String,
    reply: UnixStream,
    cursor: usize,
    deadline: Option<std::time::Instant>,
}

fn number(value: Option<&str>) -> Result<u32, String> {
    value
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| "invalid signal process field".into())
}

fn status<'a>(directory: &File, buffer: &'a mut [u8]) -> Result<&'a [u8], String> {
    let fd = directory.as_raw_fd();
    let mut file = File::open(text(format_args!("/proc/self/fd/{fd}/status"))?)
        .map_err(|_| "signal member exited")?;
    let mut size = 0;
    loop {
        let remaining = buffer
            .get_mut(size..)
            .filter(|tail| !tail.is_empty())
            .ok_or("process status exceeds bound")?;
        match file.read(remaining) {
            Ok(0) => return buffer.get(..size).ok_or_else(String::new),
            Ok(count) => size += count,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return Err("signal member status unavailable".into()),
        }
    }
}

fn field<'a>(status: &'a [u8], prefix: &str) -> Result<&'a str, String> {
    let bytes = status
        .split(|&b| b == b'\n')
        .find_map(|line| line.strip_prefix(prefix.as_bytes()))
        .ok_or("missing process identity")?;
    std::str::from_utf8(bytes)
        .map(str::trim)
        .map_err(|_| "invalid process identity".into())
}

fn in_workload(cgroup: &str, uid: u32, kind: Kind, name: &str) -> Result<bool, String> {
    let delegation = accounts::delegation(uid).ok_or("target has no workload account")?;
    let prefix = text(format_args!("0::/{delegation}/"))?;
    let Some(leaf) = cgroup
        .strip_prefix(&prefix)
        .and_then(|path| path.strip_suffix('\n'))
    else {
        return Ok(false);
    };
    Ok(match kind {
        Kind::Sandbox | Kind::Workspace => leaf == "session",
        Kind::Application => leaf.rsplit_once('-').is_some_and(|(app, suffix)| {
            app == name
                && suffix.len() == 16
                && suffix
                    .bytes()
                    .all(|b| b.is_ascii_digit() || matches!(b, b'a'..=b'f'))
        }),
    })
}

impl Plan {
    pub fn prepare(body: &[u8], stream: &UnixStream, requester: i32) -> Result<Self, String> {
        Self::prepare_in(body, stream, requester, "/proc", accounts::load)
    }

    pub(super) fn prepare_in(
        body: &[u8],
        stream: &UnixStream,
        requester: i32,
        proc_root: &str,
        ledger: impl FnOnce() -> Result<accounts::Ledger, String>,
    ) -> Result<Self, String> {
        let input = std::str::from_utf8(body).map_err(|_| "invalid signal request")?;
        let mut words = input.split_ascii_whitespace();
        let signal = words.next().ok_or("missing signal")?;
        let scope = words.next().ok_or("missing scope")?;
        let selected_pid = number(words.next())?;
        let mut members: Vec<Member> = Vec::new();
        let mut account = None;
        let mut instance: Option<String> = None;
        let ledger = ledger()?;
        let mut buffer = [0; 65537];
        for pair in words {
            let (pid, inode) = pair.split_once('.').ok_or("invalid signal member")?;
            let pid = number(Some(pid))?;
            let inode = inode.parse::<u64>().map_err(|_| "invalid signal inode")?;
            if pid <= 1 || members.len() >= MEMBERS || members.iter().any(|m| m.pid == pid) {
                return Err("invalid signal set".into());
            }
            let directory = File::open(text(format_args!("{proc_root}/{pid}"))?)
                .map_err(|_| "signal member exited")?;
            if directory
                .metadata()
                .map_err(|_| "signal member exited")?
                .ino()
                != inode
            {
                return Err("signal member changed".into());
            }
            let fd = directory.as_raw_fd();
            let record = status(&directory, &mut buffer)?;
            let uid = number(field(record, "Uid:")?.split_ascii_whitespace().next())?;
            if !field(record, "Uid:")?
                .split_ascii_whitespace()
                .map(|v| v.parse::<u32>().ok())
                .eq([Some(uid); 4])
            {
                return Err("signal member credentials changed".into());
            }
            if account.is_some_and(|other| other != uid) {
                return Err("signal set spans workload accounts".into());
            }
            account = Some(uid);
            let mut ns = field(record, "NStgid:")?.split_ascii_whitespace();
            if number(ns.next())? != pid
                || ns
                    .next()
                    .and_then(|n| n.parse::<u32>().ok())
                    .is_none_or(|pid| pid <= 1)
                || ns.any(|n| n.parse::<u32>().ok().is_none_or(|pid| pid <= 1))
            {
                return Err("signal target must be an ordinary jailed process".into());
            }
            let (kind, name) = [Kind::Application, Kind::Sandbox, Kind::Workspace]
                .into_iter()
                .find_map(|kind| ledger.name(kind, uid).map(|name| (kind, name)))
                .ok_or("unknown workload account")?;
            let cgroup = application::bounded(&text(format_args!("/proc/self/fd/{fd}/cgroup"))?)?;
            if !in_workload(&cgroup, uid, kind, name)?
                || instance
                    .as_ref()
                    .is_some_and(|original| original != &cgroup)
            {
                return Err("signal set leaves its workload instance".into());
            }
            if instance.is_none() {
                instance = Some(cgroup);
            }
            let parent = number(Some(field(record, "PPid:")?))?;
            allocation::push(
                &mut members,
                Member {
                    pid,
                    parent,
                    directory,
                },
            )
            .map_err(refused)?;
        }
        let uid = account.ok_or("empty signal set")?;
        let (kind, name) = [Kind::Application, Kind::Sandbox, Kind::Workspace]
            .into_iter()
            .find_map(|k| ledger.name(k, uid).map(|name| (k, name)))
            .ok_or("unknown workload account")?;
        let summary = text(format_args!(
            "{signal} {} {name} {selected_pid} {scope} {} {requester}",
            kind.as_str(),
            members.len()
        ))?;
        if !crate::consent::process_signal_summary(&summary)
            || !members.iter().any(|m| m.pid == selected_pid)
        {
            return Err("invalid signal selection".into());
        }
        // Strict ordering proves every parent link reaches the one selected
        // root. Re-read each link after *all* directories have been pinned.
        let parent_first = matches!(signal, "SIGSTOP" | "SIGCONT");
        for (index, member) in members
            .iter()
            .enumerate()
            .filter(|(_, m)| m.pid != selected_pid)
        {
            let parent_index = members
                .iter()
                .position(|m| m.pid == member.parent)
                .ok_or("signal set leaves the selected subtree")?;
            let record = status(&member.directory, &mut buffer)?;
            if parent_index == index
                || (parent_index < index) != parent_first
                || number(Some(field(record, "PPid:")?))? != member.parent
            {
                return Err("signal subtree changed or is out of order".into());
            }
        }
        Ok(Self {
            members,
            uid,
            kind,
            signal: crate::owned(signal)?,
            summary,
            cursor: 0,
            deadline: None,
            reply: stream
                .try_clone()
                .map_err(|_| "signal result channel unavailable")?,
        })
    }

    pub fn summary(&self) -> &str {
        &self.summary
    }

    fn command(&self) -> Result<Command, String> {
        let prefix = if self.kind == Kind::Application {
            "tda"
        } else {
            "tdw"
        };
        let mut command = Command::new("/bin/td-login");
        command.args([
            "exec-service-as",
            &text(format_args!("{prefix}{}", self.uid))?,
            "--",
            "/bin/td-taskmgr",
            "--deliver-workload-signal",
            &self.signal,
        ]);
        Ok(command)
    }

    pub fn approve(&mut self) -> Result<(), String> {
        self.deadline = Some(
            std::time::Instant::now()
                .checked_add(std::time::Duration::from_secs(90))
                .ok_or("signal deadline overflow")?,
        );
        Ok(())
    }

    pub fn advance(
        &mut self,
        helper: &mut Option<crate::inspection::Inspection>,
    ) -> Result<Option<bool>, String> {
        let deadline = self.deadline.ok_or("signal set has no approval")?;
        if std::time::Instant::now() >= deadline {
            if let Some(running) = helper.take() {
                running.reap_within(std::time::Duration::from_millis(20));
            }
            return Ok(Some(false));
        }
        let result = if let Some(running) = helper {
            let result = match running.finished() {
                Ok(None) => return Ok(None),
                Ok(Some(Some(bytes))) => match bytes.as_slice() {
                    [b @ (b'S' | b'X' | b'P' | b'E')] => *b,
                    _ => b'?',
                },
                _ => b'?',
            };
            *helper = None;
            result
        } else {
            let Some(member) = self.members.get(self.cursor) else {
                return Ok(Some(true));
            };
            let launch = || {
                crate::inspection::Inspection::with_process_directory(
                    self.command()?,
                    member
                        .directory
                        .try_clone()
                        .map_err(|_| "signal member unavailable")?,
                )
            };
            match launch() {
                Ok(running) => {
                    *helper = Some(running);
                    return Ok(None);
                }
                Err(_) => b'?',
            }
        };
        if self.reply.write_all(&[result]).is_err() {
            return Ok(Some(false));
        }
        self.cursor += 1;
        if self.cursor == self.members.len() {
            return Ok(Some(true));
        }
        Ok(None)
    }
    #[cfg(test)]
    pub(super) fn expire(&mut self) {
        self.deadline = Some(std::time::Instant::now());
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn a_reviewed_set_pins_one_workload_and_preserves_each_result() {
        let tree = crate::test_temp::TempDir::new("signal-plan").unwrap();
        let (server, _client) = UnixStream::pair().unwrap();
        let uid = 98304;
        let member = |pid, parent, local_pid, uid| {
            let dir = tree.path().join(format!("{pid}"));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("status"),
                format!(
                    "Uid:\t{uid} {uid} {uid} {uid}\nNStgid:\t{pid} {local_pid}\nPPid:\t{parent}\n"
                ),
            )
            .unwrap();
            std::fs::write(
                dir.join("cgroup"),
                format!("0::/td-workload-{uid}/session\n"),
            )
            .unwrap();
            format!("{pid}.{}", dir.metadata().unwrap().ino())
        };
        let selected_member = member(20, 10, 2, uid);
        let child = member(21, 20, 3, uid);
        let sibling = member(22, 10, 4, uid);
        let init = member(19, 10, 1, uid);
        let foreign = member(23, 20, 4, uid + 1);
        let prepare = |signal, scope, members| {
            Plan::prepare_in(
                format!("{signal} {scope} 20 {members}").as_bytes(),
                &server,
                100,
                tree.path().to_str().unwrap(),
                || accounts::Ledger::parse("td-accounts-v1\nsandbox\tdemo\t98304\n"),
            )
        };
        let mut plan = prepare("SIGTERM", "subtree", format!("{child} {selected_member}")).unwrap();
        assert_eq!(plan.summary(), "SIGTERM sandbox demo 20 subtree 2 100");
        let cmd = plan.command().unwrap();
        let args = cmd
            .get_args()
            .map(|a| a.to_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            args,
            [
                "exec-service-as",
                "tdw98304",
                "--",
                "/bin/td-taskmgr",
                "--deliver-workload-signal",
                "SIGTERM",
            ]
        );
        assert!(prepare("SIGSTOP", "subtree", format!("{selected_member} {child}")).is_ok());
        assert!(prepare("SIGTERM", "selected", selected_member.clone()).is_ok());
        for members in [
            format!("{child} {selected_member}"),
            format!("{sibling} {selected_member}"),
            format!("{init} {selected_member}"),
            format!("{foreign} {selected_member}"),
            format!("{child} 20.1"),
        ] {
            assert!(prepare("SIGSTOP", "subtree", members).is_err());
        }
        assert!(prepare("SIGKILL", "selected", format!("{child} {selected_member}")).is_err());
        assert!(plan.advance(&mut None).is_err());
        plan.approve().unwrap();
    }

    #[test]
    fn packaged_processes_must_share_the_registered_application_instance() {
        let tree = crate::test_temp::TempDir::new("signal-packaged").unwrap();
        let (server, _client) = UnixStream::pair().unwrap();
        let mut members = Vec::new();
        for (pid, parent) in [(22, 20), (20, 10)] {
            let dir = tree.path().join(pid.to_string());
            std::fs::create_dir(&dir).unwrap();
            let mut status = b"Name:\tbin\xff\n".to_vec();
            status.extend_from_slice(
                format!("Uid: 65536 65536 65536 65536\nNStgid: {pid} 2\nPPid: {parent}\n")
                    .as_bytes(),
            );
            std::fs::write(dir.join("status"), status).unwrap();
            std::fs::write(
                dir.join("cgroup"),
                "0::/td-app-65536/firefox-0123456789abcdef\n",
            )
            .unwrap();
            members.push(format!("{pid}.{}", dir.metadata().unwrap().ino()));
        }
        let frame = format!("SIGTERM subtree 20 {}", members.join(" "));
        let prepare = || {
            Plan::prepare_in(
                frame.as_bytes(),
                &server,
                100,
                tree.path().to_str().unwrap(),
                || accounts::Ledger::parse("td-accounts-v1\napplication\tfirefox\t65536\n"),
            )
        };
        assert_eq!(
            prepare().unwrap().summary(),
            "SIGTERM application firefox 20 subtree 2 100"
        );
        for path in [
            "firefox-abcdef0123456789",
            "mail-0123456789abcdef",
            "session",
        ] {
            std::fs::write(
                tree.path().join("22/cgroup"),
                format!("0::/td-app-65536/{path}\n"),
            )
            .unwrap();
            assert!(prepare().is_err());
        }
    }
}
