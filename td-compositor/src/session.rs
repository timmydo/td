//! Cross-identity socket admission for the configured graphical session.

use crate::{control, ready, sys};
use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub(crate) const HUMAN_UID: u32 = 1000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SocketPolicy {
    Private,
    HumanSession,
}

impl SocketPolicy {
    pub(crate) fn for_authority(authority: bool) -> Self {
        if authority {
            Self::HumanSession
        } else {
            Self::Private
        }
    }

    pub(crate) fn mode(self) -> u32 {
        match self {
            Self::Private => 0o600,
            Self::HumanSession => 0o666,
        }
    }

    pub(crate) fn admit(self, stream: &UnixStream) -> Result<(), String> {
        if self == Self::Private {
            return Ok(());
        }
        require_human(sys::peer_uid(stream)?)
    }
}

fn require_human(uid: u32) -> Result<(), String> {
    if uid == HUMAN_UID {
        Ok(())
    } else {
        Err(format!("compositor session refuses peer uid {uid}"))
    }
}

const AUTHORITY_ARMED: &str = "TD-TERMINAL-AUTHORITY-ARMED";
const AUTHORITY_READY: &str = "TD-TERMINAL-AUTHORITY-READY";
const AUTHORITY_DONE: &str = "TD-TERMINAL-AUTHORITY-OK";
const INPUT_TOKEN: &str = "td.firefox-input=1";
const CONTROL: &str = "/run/td-compositor/1000/td-control";
const HUMAN_RUNTIME: &str = "/run/user/1000";

/// Composed boot diagnostic: the host supplies the two physical key chords.
/// This observes the stock launch path; it is not a consent or identity proof.
pub(crate) fn probe_terminal_authority() -> Result<(), String> {
    require_human(process_uid()?)?;
    let mut cmdline = String::new();
    std::fs::File::open("/proc/cmdline")
        .and_then(|file| file.take(8193).read_to_string(&mut cmdline))
        .map_err(|error| format!("read terminal probe command line: {error}"))?;
    if cmdline.len() > 8192 {
        return Err("terminal probe command line exceeds 8192 bytes".into());
    }
    if !cmdline
        .split_ascii_whitespace()
        .any(|word| word == INPUT_TOKEN)
    {
        return Ok(());
    }
    let baseline = window_ids(&layout()?)?;
    if !authority_sockets(Path::new(HUMAN_RUNTIME))?.is_empty() {
        return Err("terminal authority probe found an earlier launch".into());
    }
    emit(AUTHORITY_ARMED)?;
    let deadline = Instant::now()
        .checked_add(Duration::from_secs(30))
        .ok_or_else(|| "could not bound the terminal authority stage".to_string())?;
    let (socket, handle) = loop {
        let sockets = authority_sockets(Path::new(HUMAN_RUNTIME))?;
        if sockets.len() > 1 {
            return Err("terminal authority probe observed multiple launches".into());
        }
        let report = layout()?;
        let current = window_ids(&report)?;
        if !baseline.is_subset(&current) {
            return Err("terminal authority probe lost a baseline window".into());
        }
        let added: Vec<_> = current.difference(&baseline).copied().collect();
        if added.len() > 1 {
            return Err("terminal authority probe observed multiple new windows".into());
        }
        if let (Some(socket), Some(handle)) = (sockets.first(), added.first()) {
            if terminal_is_focused(&report, *handle) {
                match ready::probe(socket) {
                    Ok(()) => {
                        if Instant::now() >= deadline {
                            return Err(
                                "terminal authority launch timed out during readiness".into()
                            );
                        }
                        break (socket.clone(), *handle);
                    }
                    Err(error) if Instant::now() >= deadline => {
                        return Err(format!("terminal authority readiness timed out: {error}"));
                    }
                    Err(_) => {}
                }
            }
        }
        wait_for_probe(deadline, "launch")?;
    };
    emit(AUTHORITY_READY)?;
    let deadline = Instant::now()
        .checked_add(Duration::from_secs(30))
        .ok_or_else(|| "could not bound the terminal authority stage".to_string())?;
    loop {
        let report = layout()?;
        let current = window_ids(&report)?;
        if Instant::now() >= deadline
            && current.contains(&handle)
            && !terminal_is_focused(&report, handle)
        {
            return Err("terminal authority close timed out after losing terminal focus".into());
        }
        if !baseline.is_subset(&current) {
            return Err("terminal authority close lost a baseline window".into());
        }
        let removed = match std::fs::symlink_metadata(&socket) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
            Err(error) => return Err(format!("inspect terminal teardown: {error}")),
            Ok(_) => false,
        };
        if removed && !current.contains(&handle) {
            return emit(AUTHORITY_DONE);
        }
        wait_for_probe(deadline, "close")?;
    }
}

pub(crate) fn process_uid() -> Result<u32, String> {
    let mut status = String::new();
    std::fs::File::open("/proc/self/status")
        .and_then(|file| file.take(8193).read_to_string(&mut status))
        .map_err(|error| format!("read terminal probe credentials: {error}"))?;
    if status.len() > 8192 {
        return Err("terminal probe status exceeds 8192 bytes".into());
    }
    crate::pty::effective_uid(&status)
}

fn emit(marker: &str) -> Result<(), String> {
    let mut out = std::io::stdout().lock();
    writeln!(out, "\n{marker}")
        .and_then(|()| out.flush())
        .map_err(|error| format!("write terminal authority evidence: {error}"))
}

fn wait_for_probe(deadline: Instant, stage: &str) -> Result<(), String> {
    if Instant::now() >= deadline {
        return Err(format!("terminal authority {stage} timed out"));
    }
    std::thread::sleep(Duration::from_millis(100));
    Ok(())
}

fn layout() -> Result<String, String> {
    control::ask(Path::new(CONTROL), control::Request::Layout)
        .map_err(|error| error.message().to_string())
}

fn window_ids(report: &str) -> Result<BTreeSet<u64>, String> {
    let mut handles = BTreeSet::new();
    for line in report
        .lines()
        .filter_map(|line| line.strip_prefix("window id=@"))
    {
        let word = line
            .split_ascii_whitespace()
            .next()
            .ok_or_else(|| "terminal probe window has no handle".to_string())?;
        let handle = word
            .parse::<u64>()
            .map_err(|_| "terminal probe window has an invalid handle".to_string())?;
        if handle == 0 || !handles.insert(handle) {
            return Err("terminal probe window handle is zero or repeated".into());
        }
    }
    Ok(handles)
}

fn terminal_is_focused(report: &str, handle: u64) -> bool {
    let prefix = format!("window id=@{handle} ");
    report.lines().any(|line| {
        let Some(line) = line.strip_prefix(&prefix) else {
            return false;
        };
        let Some((fields, title)) = line.split_once(" title=") else {
            return false;
        };
        let Some((fields, _app_id)) = fields.split_once(" app_id=") else {
            return false;
        };
        let fields: Vec<_> = fields.split_ascii_whitespace().collect();
        title == crate::term_client::TITLE
            && fields.contains(&"visible=true")
            && fields.contains(&"focused=true")
            && fields.iter().any(|field| {
                field
                    .strip_prefix("width=")
                    .and_then(|word| word.parse::<u32>().ok())
                    .is_some_and(|n| n > 0)
            })
            && fields.iter().any(|field| {
                field
                    .strip_prefix("height=")
                    .and_then(|word| word.parse::<u32>().ok())
                    .is_some_and(|n| n > 0)
            })
    })
}

fn authority_name(name: &str) -> bool {
    let Some(body) = name
        .strip_prefix("td-auth-terminal-")
        .and_then(|body| body.strip_suffix(".ready"))
    else {
        return false;
    };
    let Some((generation, handle)) = body.split_once('-') else {
        return false;
    };
    generation.len() == 32
        && generation
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        && !handle.starts_with('0')
        && handle.bytes().all(|byte| byte.is_ascii_digit())
        && handle.parse::<u64>().is_ok_and(|value| value > 0)
}

fn authority_sockets(runtime: &Path) -> Result<Vec<PathBuf>, String> {
    authority_sockets_owned(runtime, HUMAN_UID)
}

fn authority_sockets_owned(runtime: &Path, owner: u32) -> Result<Vec<PathBuf>, String> {
    let entries =
        std::fs::read_dir(runtime).map_err(|error| format!("read terminal runtime: {error}"))?;
    let mut sockets = Vec::new();
    for (index, entry) in entries.enumerate() {
        if index >= 256 {
            return Err("terminal runtime exceeds 256 entries".into());
        }
        let entry = entry.map_err(|error| format!("read terminal runtime entry: {error}"))?;
        if !entry.file_name().to_str().is_some_and(authority_name) {
            continue;
        }
        let metadata = match std::fs::symlink_metadata(entry.path()) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(format!("inspect authority readiness socket: {error}")),
        };
        if !metadata.file_type().is_socket() || metadata.uid() != owner {
            return Err("authority readiness path is not a human socket".into());
        }
        // Publication sets permissions after bind. Its initial mode is not ready.
        if metadata.mode() & 0o7777 == 0o600 {
            sockets.push(entry.path());
        }
    }
    Ok(sockets)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn readiness_waits_for_publication_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let root =
            std::env::temp_dir().join(format!("td-authority-publication-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("td-auth-terminal-0123456789abcdef0123456789abcdef-1.ready");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let owner = std::fs::symlink_metadata(&path).unwrap().uid();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(authority_sockets_owned(&root, owner).unwrap().is_empty());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            authority_sockets_owned(&root, owner).unwrap(),
            vec![path.clone()]
        );
        std::fs::remove_file(&path).unwrap();
        assert!(authority_sockets_owned(&root, owner).unwrap().is_empty());
        std::fs::write(&path, b"not a socket").unwrap();
        assert!(authority_sockets_owned(&root, owner).is_err());
        drop(listener);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn admission_uses_the_kernel_peer_instead_of_socket_contents() {
        let (mut client, server) = UnixStream::pair().unwrap();
        std::io::Write::write_all(&mut client, b"uid=1000\n").unwrap();
        let uid = process_uid().unwrap();
        assert_eq!(
            SocketPolicy::HumanSession.admit(&server).is_ok(),
            uid == 1000
        );
        assert!(SocketPolicy::Private.admit(&server).is_ok());
    }

    #[test]
    fn only_the_configured_human_crosses_the_identity_boundary() {
        assert_eq!(HUMAN_UID, 1000);
        assert_eq!(SocketPolicy::for_authority(false), SocketPolicy::Private);
        assert_eq!(
            SocketPolicy::for_authority(true),
            SocketPolicy::HumanSession
        );
        assert_eq!(SocketPolicy::Private.mode(), 0o600);
        assert_eq!(SocketPolicy::HumanSession.mode(), 0o666);
        assert!(require_human(1000).is_ok());
        for uid in [0, 991, 992, 993, 994, 1001, 65534, 65536, u32::MAX] {
            assert!(require_human(uid).is_err());
        }
    }

    #[test]
    fn readiness_names_pin_generation_and_non_reused_handle_spelling() {
        let prefix = "td-auth-terminal-0123456789abcdef0123456789abcdef-";
        assert!(authority_name(&format!("{prefix}1.ready")));
        assert!(authority_name(&format!(
            "{prefix}18446744073709551615.ready"
        )));
        for suffix in [
            "0.ready",
            "01.ready",
            "+1.ready",
            "-1.ready",
            ".ready",
            "18446744073709551616.ready",
            "1.ready.extra",
        ] {
            assert!(!authority_name(&format!("{prefix}{suffix}")));
        }
        assert!(!authority_name(
            "td-auth-terminal-ABCDEF0123456789abcdef0123456789-1.ready"
        ));
        assert!(!authority_name("td-auth-terminal-abc-1.ready"));
    }

    #[test]
    fn terminal_layout_requires_a_new_focused_visible_positive_window() {
        let row = "window id=@7 object=1:4 workspace=1 x=0 y=24 width=800 height=600 visible=true focused=true fullscreen=false floating=false parent= app_id= title=td terminal\n";
        assert_eq!(window_ids(row).unwrap(), BTreeSet::from([7]));
        assert!(terminal_is_focused(row, 7));
        assert!(!terminal_is_focused(row, 8));
        for (from, to) in [
            ("visible=true", "visible=false"),
            ("focused=true", "focused=false"),
            ("width=800", "width=0"),
            ("height=600", "height=0"),
            ("title=td terminal", "title=other focused=true"),
        ] {
            assert!(!terminal_is_focused(&row.replace(from, to), 7));
        }
        let forged = row.replace("focused=true", "focused=false").replace(
            "app_id=",
            "app_id=focused=true visible=true width=1 height=1",
        );
        assert!(!terminal_is_focused(&forged, 7));
        assert!(window_ids(&format!("{row}{row}")).is_err());
        assert!(window_ids(&row.replace("id=@7", "id=@0")).is_err());
    }
}
