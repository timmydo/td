//! The client for root's workspace slots and instance launches. Root owns
//! accounts and specs; this client sends only a slot, argv and directory plan.

use std::fmt::Write as _;
use std::io;
use std::os::unix::net::UnixStream;
use std::path::Path;

use crate::jail::Policy;
use crate::workload_accounts as accounts;
use crate::workload_grants::{self as grants, Role};
use crate::workspace_intake::{Answer, Intake};

pub use crate::workspace_intake::LOG;

const MAX_REQUEST: usize = 64 * 1024;
const MAX_ARGUMENTS: usize = 128;

fn allocation<E>(_: E) -> io::Error {
    io::ErrorKind::OutOfMemory.into()
}

/// A validated name from the fixed conversation or review pool.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Slot<'a>(&'a str);

impl<'a> Slot<'a> {
    pub fn parse(name: &'a str) -> io::Result<Self> {
        accounts::workspace_slot(name)
            .map(|_| Self(name))
            .ok_or_else(|| io::Error::other("invalid workspace slot"))
    }

    pub fn name(self) -> &'a str {
        self.0
    }
}

/// Root's view of a slot; a held slot must never be silently recycled.
#[derive(Debug, PartialEq, Eq)]
pub enum Status {
    Absent,
    Free(u32),
    Held(u32),
}

#[derive(Debug, PartialEq, Eq)]
pub enum Created {
    Ready(u32),
    Held,
}

/// Connects only to td's root intake. No launch or slot operation falls
/// back to a local jail or to an unconfined process.
pub struct Root<'a> {
    transport: crate::workspace_intake::Root<'a, MAX_REQUEST>,
}

impl Default for Root<'_> {
    fn default() -> Self {
        Self {
            transport: crate::workspace_intake::Root {
                patience: std::time::Duration::from_secs(10),
                ..crate::workspace_intake::Root::new()
            },
        }
    }
}

impl Root<'_> {
    fn ask(&self, verb: &str, slot: Slot<'_>) -> io::Result<Answer> {
        let request = td_alloc::format(format_args!("workspace {verb} {}\n", slot.name()))
            .map_err(allocation)?;
        self.transport.ask(&request)
    }

    pub fn create(&self, slot: Slot<'_>) -> io::Result<Created> {
        match self.ask("create", slot)? {
            Answer::Refused(reason) if reason == crate::workspace_intake::EXISTS => {
                Ok(Created::Held)
            }
            answer => uid(&answer.accepted()?).map(Created::Ready),
        }
    }

    pub fn status(&self, slot: Slot<'_>) -> io::Result<Status> {
        status(&self.ask("status", slot)?.accepted()?)
    }

    pub fn delete(&self, slot: Slot<'_>) -> io::Result<()> {
        if self.ask("delete", slot)?.accepted()?.is_empty() {
            Ok(())
        } else {
            Err(amiss())
        }
    }

    /// `home` is the human's canonical home, not the account's private
    /// home. Policy directories must already be canonical and admitted by
    /// workspace preparation. Root repeats admission and publishes views.
    /// The returned stream begins at the tool host's first byte, not at
    /// the intake answer; `ok` alone is not evidence the tool host started.
    pub fn launch(
        &self,
        slot: Slot<'_>,
        home: &Path,
        policy: &Policy,
        arguments: &[String],
    ) -> io::Result<UnixStream> {
        self.transport
            .launch(&launch_request(slot, home, policy, arguments)?)
    }
}

fn amiss() -> io::Error {
    io::Error::other("root's workspace intake answered amiss")
}

fn uid(text: &str) -> io::Result<u32> {
    text.strip_suffix('\n')
        .filter(|number| accounts::canonical_decimal(number))
        .and_then(|number| number.parse::<u32>().ok())
        .filter(|number| accounts::WORKLOAD_UIDS.contains(number))
        .ok_or_else(amiss)
}

fn status(text: &str) -> io::Result<Status> {
    if text == "absent\n" {
        return Ok(Status::Absent);
    }
    let (state, number) = text.split_once(' ').ok_or_else(amiss)?;
    let uid = uid(number)?;
    match state {
        "free" => Ok(Status::Free(uid)),
        "held" => Ok(Status::Held(uid)),
        _ => Err(amiss()),
    }
}

fn append(request: &mut String, text: &str) -> io::Result<()> {
    if text.len() > MAX_REQUEST.saturating_sub(request.len()) {
        return Err(io::Error::other("workspace launch exceeds 64 KiB"));
    }
    td_alloc::push_str(request, text).map_err(allocation)
}

pub(crate) fn launch_request(
    slot: Slot<'_>,
    home: &Path,
    policy: &Policy,
    arguments: &[String],
) -> io::Result<String> {
    let runtime = policy.runtime.as_deref().unwrap_or("native");
    if policy.runtime.as_ref().is_some_and(|name| {
        !policy.development || crate::flathub_app::flathub_runtime(name).is_none()
    }) {
        return Err(io::Error::other("invalid runtime for workspace profile"));
    }
    if arguments.is_empty()
        || arguments.len() > MAX_ARGUMENTS
        || arguments.first().is_some_and(String::is_empty)
        || arguments.iter().any(|word| word.contains('\0'))
    {
        return Err(io::Error::other("invalid workspace entry arguments"));
    }
    let mut request = td_alloc::format(format_args!(
        "workspace launch {} {} {runtime}",
        slot.name(),
        if policy.development {
            "development"
        } else {
            "isolated"
        }
    ))
    .map_err(allocation)?;
    for word in arguments {
        append(&mut request, " x")?;
        let bytes = word.len().checked_mul(2).ok_or_else(allocation_overflow)?;
        if bytes > MAX_REQUEST.saturating_sub(request.len()) {
            return Err(io::Error::other("workspace launch exceeds 64 KiB"));
        }
        td_alloc::reserve_string(&mut request, bytes).map_err(allocation)?;
        for byte in word.bytes() {
            write!(&mut request, "{byte:02x}").map_err(allocation)?;
        }
    }
    append(&mut request, "\n")?;
    let home = home
        .to_str()
        .filter(|home| home.starts_with('/') && *home != "/" && !home.ends_with('/'))
        .ok_or_else(|| io::Error::other("workspace needs the human's canonical home"))?;
    let mut entries: Vec<(Role, &str)> = Vec::new();
    for (role, paths) in [
        (Role::Worktree, &policy.worktrees),
        (Role::Checkout, &policy.checkouts),
        (Role::Repository, &policy.repositories),
        (Role::Read, &policy.objects),
        (Role::Read, &policy.read),
        (Role::Write, &policy.write),
    ] {
        for path in paths {
            let relative = path
                .to_str()
                .and_then(|path| path.strip_prefix(home))
                .and_then(|path| path.strip_prefix('/'))
                .ok_or_else(|| {
                    io::Error::other("workspace directory is outside the human's home")
                })?;
            grants::validate_role(role, relative.as_bytes()).map_err(io::Error::other)?;
            if entries.len() == grants::MAX_DIRECTORIES
                || entries
                    .iter()
                    .any(|(_, other)| grants::overlapping(other, relative))
            {
                return Err(io::Error::other(
                    "workspace directories overlap or exceed 32",
                ));
            }
            td_alloc::push(&mut entries, (role, relative)).map_err(allocation)?;
            append(&mut request, role.as_str())?;
            append(&mut request, " ")?;
            append(&mut request, relative)?;
            append(&mut request, "\n")?;
        }
    }
    append(&mut request, "end\n")?;
    Ok(request)
}

fn allocation_overflow() -> io::Error {
    io::ErrorKind::OutOfMemory.into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;
    use std::time::Duration;

    fn policy() -> Policy {
        Policy {
            account: None,
            development: true,
            home: "/irrelevant/private-home".into(),
            scratch: vec!["/irrelevant/scratch".into()],
            worktrees: vec!["/home/test/src/my project".into()],
            checkouts: vec![],
            repositories: vec![],
            objects: vec![],
            read: vec![],
            write: vec![],
            directory: None,
            runtime: None,
        }
    }

    fn request(policy: &Policy, args: &[String]) -> io::Result<String> {
        launch_request(
            Slot::parse("agent-1")?,
            Path::new("/home/test"),
            policy,
            args,
        )
    }

    #[test]
    fn slots_and_account_answers_are_exact() -> io::Result<()> {
        for name in ["agent-1", "agent-960", "review-1", "review-64"] {
            assert_eq!(Slot::parse(name)?.name(), name);
        }
        for name in [
            "agent-0",
            "agent-961",
            "agent-01",
            "review-65",
            "default",
            "agent-1\n",
        ] {
            assert!(Slot::parse(name).is_err(), "{name}");
        }
        assert_eq!(status("absent\n")?, Status::Absent);
        assert_eq!(status("free 98304\n")?, Status::Free(98304));
        assert_eq!(status("held 131071\n")?, Status::Held(131071));
        for text in [
            "free 0\n",
            "free 1000\n",
            "free 098304\n",
            "held 131072\n",
            "free 98304",
            "absent\nmore\n",
            "gone 98304\n",
        ] {
            assert!(status(text).is_err(), "{text}");
        }
        Ok(())
    }

    #[test]
    fn launch_encodes_arguments_and_all_directory_roles() -> io::Result<()> {
        let mut p = policy();
        p.checkouts.push("/home/test/src/checkout".into());
        p.repositories
            .push("/home/test/.local/share/td-agent/ws/w/r.git".into());
        p.objects
            .push("/home/test/.local/share/td-agent/store/s.git/objects".into());
        p.read.push("/home/test/Documents".into());
        p.write.push("/home/test/output".into());
        assert_eq!(request(&p, &["tool-host".into(), "a b\nλ".into(), String::new()])?,
            "workspace launch agent-1 development native x746f6f6c2d686f7374 x6120620acebb x\n\
             worktree src/my project\ncheckout src/checkout\nrepository .local/share/td-agent/ws/w/r.git\n\
             read .local/share/td-agent/store/s.git/objects\nread Documents\nwrite output\nend\n");
        p.development = false;
        assert!(request(&p, &["maintain".into()])?.contains(" isolated native "));
        p.runtime = Some("freedesktop-sdk-25-08".into());
        assert!(request(&p, &["tool-host".into()]).is_err());
        p.development = true;
        assert!(request(&p, &["tool-host".into()])?.contains(" development freedesktop-sdk-25-08 "));
        p.runtime = Some("bogus\nend".into());
        assert!(request(&p, &["tool-host".into()]).is_err());
        Ok(())
    }

    #[test]
    fn plans_refuse_sensitive_paths_overlap_and_protocol_injection() {
        for path in [
            "/home/test",
            "/home/tester/x",
            "/srv/x",
            "/home/test/.ssh",
            "/home/test/src/../.ssh",
            "/home/test/src\nend",
            "/home/test/.local/share/td-agent/store/s.git/objects",
        ] {
            let mut p = policy();
            p.worktrees = vec![path.into()];
            assert!(request(&p, &["tool-host".into()]).is_err(), "{path}");
        }
        for path in [
            "/home/test/src",
            "/home/test/src/my project",
            "/home/test/src/my project/child",
        ] {
            let mut p = policy();
            p.read.push(path.into());
            assert!(request(&p, &["tool-host".into()]).is_err(), "{path}");
        }
        let mut p = policy();
        p.read = (0..32).map(|n| format!("/home/test/d{n}").into()).collect();
        assert!(request(&p, &["tool-host".into()]).is_err());
        p.read.pop();
        assert!(request(&p, &["tool-host".into()]).is_ok());
        for args in [
            vec![],
            vec![String::new()],
            vec!["a\0b".into()],
            vec!["x".into(); 129],
            vec!["x".repeat(MAX_REQUEST)],
        ] {
            assert!(request(&policy(), &args).is_err());
        }
    }

    #[test]
    fn allocation_refusals_are_recoverable() -> io::Result<()> {
        let p = policy();
        let args = vec!["tool-host".into()];
        let mut passed = false;
        for budget in 0..64 {
            match td_alloc::with_reservation_limit(budget, || request(&p, &args)) {
                Ok(_) => {
                    passed = true;
                    break;
                }
                Err(e) => assert_eq!(e.kind(), io::ErrorKind::OutOfMemory),
            }
        }
        assert!(passed);
        request(&p, &args)?;
        Ok(())
    }

    #[test]
    fn socket_requests_preserve_refusals_and_first_host_byte() -> io::Result<()> {
        let dir = td_fs::TempDir::new("agent-intake")?;
        let path = dir.join("socket");
        let listener = UnixListener::bind(&path)?;
        let root = Root {
            transport: crate::workspace_intake::Root {
                socket: &path,
                patience: Duration::ZERO,
            },
        };
        let server = std::thread::spawn(move || -> io::Result<()> {
            for (expected, answer) in [
                ("workspace create agent-1\n", &b"ok 98304\n"[..]),
                ("workspace create agent-1\n", b"refused exists\n"),
                ("workspace status agent-1\n", b"ok held 98304\n"),
                ("workspace delete agent-1\n", b"ok\n"),
                ("workspace launch agent-1 development native x746f6f6c2d686f7374\nworktree src/my project\nend\n", b"ok\n\x04\x00"),
            ] {
                let (mut stream, _) = listener.accept()?;
                stream.set_read_timeout(Some(Duration::from_secs(2)))?;
                let mut got = vec![0; expected.len()];
                stream.read_exact(&mut got)?;
                assert_eq!(got, expected.as_bytes());
                stream.write_all(answer)?;
            }
            Ok(())
        });
        let slot = Slot::parse("agent-1")?;
        assert_eq!(root.create(slot)?, Created::Ready(98304));
        assert_eq!(root.create(slot)?, Created::Held);
        assert_eq!(root.status(slot)?, Status::Held(98304));
        root.delete(slot)?;
        let mut host = root.launch(
            slot,
            Path::new("/home/test"),
            &policy(),
            &["tool-host".into()],
        )?;
        let mut first = [0; 2];
        host.read_exact(&mut first)?;
        assert_eq!(first, [4, 0]);
        assert!(host.read_timeout()?.is_none());
        assert!(host.write_timeout()?.is_none());
        server
            .join()
            .map_err(|_| io::Error::other("server panicked"))??;
        Ok(())
    }
}
