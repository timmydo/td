//! The human's grant requests, root's serving half (td-authd/DESIGN.md,
//! "Grant requests"): request `2c`, and the workspace intake's grant
//! verbs and its deletion of a sandbox, run td-firstboot's `grant-table`,
//! the grant table's one writer, for one validated account and directory
//! or a sandbox's network, wait for it within a bound, and read back the
//! account's directory rows and a sandbox's network, or what the request
//! changed. This file is the one place root runs the writer. Nothing here
//! elevates: a grant delegates the human's authority over the human's own
//! files.

use crate::stderr_tail;
use crate::workload_accounts::Kind;
use crate::workload_grants::{self, Access, Network};
use std::io::{self, Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// How long the serving loop waits for the writer: td-firstboot waits a
/// second at most for the principal lock and then syncs one small file,
/// which an install's I/O can slow. The compositor waits a margin longer.
const WRITER_TIME: Duration = Duration::from_secs(3);
/// td-firstboot's exit status for arguments it refuses: root built them,
/// so it is root's fault, not a refusal for the human.
const USAGE_EXIT: i32 = 2;
const READ_STEP: Duration = Duration::from_millis(10);
/// The most root reads of the writer's output: a listing of every
/// directory row an account may hold at the longest path.
const OUTPUT_BYTES: usize = 64 * 1024;
/// A grant path's bound, the table's.
pub(crate) const PATH_BYTES: usize = 1024;
/// A frame's payload bound (td-authd/DESIGN.md, "Private channel").
const FRAME_BYTES: usize = crate::channel::MAX_MESSAGE;
/// The longest refusal reason an answer carries.
const REASON_BYTES: usize = crate::software::REASON_BYTES;
/// What `grant-table` prints, td-firstboot's markers.
const LISTED: &str = "TD-GRANTS\t";
const OUTCOMES: &[(&str, u8)] = &[
    ("TD-GRANT-ADDED\n", 0),
    ("TD-GRANT-CHANGED\n", 1),
    ("TD-GRANT-UNCHANGED\n", 2),
    ("TD-GRANT-REVOKED\n", 3),
];
/// A clear's answer, then how many rows it took and a newline.
const CLEARED: &str = "TD-GRANT-CLEARED\t";
/// A listing's network byte for an application, whose network its
/// declaration sets, not the table.
const NETWORK_DECLARED: u8 = 0xff;

/// What a request asks of an account's directory rows, or of a sandbox's
/// network.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Verb {
    /// Its rows from the `start`th on, as many as one answer holds.
    List {
        start: usize,
    },
    Grant(Access, Vec<u8>),
    Revoke(Vec<u8>),
    /// A sandbox's network, off or the host's.
    Network(Network),
    /// Every row of a sandbox, directory, socket and network alike, when
    /// the workspace intake deletes it; no `2c` request names it.
    Clear,
}

impl Verb {
    /// A copy of this verb in fallibly grown storage.
    pub(crate) fn copy(&self) -> Result<Self, String> {
        let path = |path: &[u8]| crate::allocation::copy(path).map_err(crate::refused);
        Ok(match self {
            Self::List { start } => Self::List { start: *start },
            Self::Grant(access, granted) => Self::Grant(*access, path(granted)?),
            Self::Revoke(revoked) => Self::Revoke(path(revoked)?),
            Self::Network(network) => Self::Network(*network),
            Self::Clear => Self::Clear,
        })
    }
}

/// One request: a verb, and an account by kind, `application` or
/// `sandbox`, and a name in the registry grammar. A workspace's account
/// takes no grants until its launches go through root.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Request {
    pub verb: Verb,
    pub kind: Kind,
    pub name: String,
}

impl Request {
    /// `2c`'s operands: a verb byte, the kind byte, `00` application or
    /// `01` sandbox, the name's length and the name, then a list's start
    /// index, a path of one to 1024 bytes, or a sandbox's network byte,
    /// `00` off or `01` host. The path's rules are the table's, answered
    /// as a refusal.
    pub(crate) fn decode(operands: &[u8]) -> Result<Self, String> {
        let invalid = || "invalid grant request".to_string();
        let [verb, kind, length, rest @ ..] = operands else {
            return Err(invalid());
        };
        let kind = match kind {
            0 => Kind::Application,
            1 => Kind::Sandbox,
            _ => return Err(invalid()),
        };
        let (name, tail) = rest
            .split_at_checked(usize::from(*length))
            .ok_or_else(invalid)?;
        let name = std::str::from_utf8(name)
            .ok()
            .filter(|name| crate::consent::application_name(name))
            .ok_or_else(invalid)?;
        let path = || {
            (!tail.is_empty() && tail.len() <= PATH_BYTES)
                .then(|| tail.to_vec())
                .ok_or_else(invalid)
        };
        let verb = match (*verb, tail) {
            (0, [start]) if usize::from(*start) < workload_grants::MAX_DIRECTORIES => Verb::List {
                start: usize::from(*start),
            },
            (1, _) => Verb::Grant(Access::ReadOnly, path()?),
            (2, _) => Verb::Grant(Access::ReadWrite, path()?),
            (3, _) => Verb::Revoke(path()?),
            // An application's network is its declaration's.
            (4, [0]) if kind == Kind::Sandbox => Verb::Network(Network::Off),
            (4, [1]) if kind == Kind::Sandbox => Verb::Network(Network::Host),
            _ => return Err(invalid()),
        };
        Ok(Self {
            verb,
            kind,
            name: name.to_string(),
        })
    }
}

/// The writer's run for `request`, or the refusal its path earns without
/// one: the fixed program, the owner, and the peer's verb, kind, name,
/// access or network and path, validated, as its arguments.
fn writer(owner: u32, request: &Request) -> Result<Command, String> {
    // A grant names a path this deployment publishes; a revoke any row the
    // table can hold, so one a later validator refuses can be taken back.
    let path = match &request.verb {
        Verb::List { .. } | Verb::Network(_) | Verb::Clear => None,
        Verb::Grant(_, path) => Some(path),
        Verb::Revoke(path) => {
            workload_grants::table_path(path)?;
            Some(path)
        }
    };
    if let Verb::Grant(_, path) = &request.verb {
        workload_grants::validate_path(path)?;
    }
    let path = match path {
        Some(path) => Some(std::str::from_utf8(path).map_err(|_| "the path is not UTF-8")?),
        None => None,
    };
    let (verb, word) = match &request.verb {
        Verb::List { .. } => ("list", None),
        Verb::Grant(access, _) => ("grant", Some(access.as_str())),
        Verb::Revoke(_) => ("revoke", None),
        Verb::Network(network) => ("network", Some(network.as_str())),
        Verb::Clear => ("clear", None),
    };
    let mut command = Command::new("/bin/td-firstboot");
    command.args([
        "grant-table",
        &owner.to_string(),
        verb,
        request.kind.as_str(),
        &request.name,
    ]);
    // A grant's access or a sandbox's network, one fixed word.
    command.args(word);
    command.args(path);
    Ok(command)
}

/// `8b` and the reason, printable ASCII, cut to its bound.
fn refusal(reason: &str) -> Vec<u8> {
    let mut answer = vec![0x8b];
    answer.extend_from_slice(refusal_text(reason).as_bytes());
    answer
}

/// A refusal's reason as the human is shown it: printable ASCII, cut to
/// its bound, never empty.
pub(crate) fn refusal_text(reason: &str) -> String {
    let reason: String = crate::flathub_app::reason_text(reason)
        .chars()
        .take(REASON_BYTES)
        .collect();
    if reason.is_empty() {
        "the grant table's writer refused".to_string()
    } else {
        reason
    }
}

/// What the writer left: its output, its last diagnostic bytes and its
/// exit status, `None` when a signal ended it.
struct Ran {
    code: Option<i32>,
    output: Vec<u8>,
    said: Vec<u8>,
}

/// What a successful writer said, read exactly.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Written {
    /// The account's directory rows, and a sandbox's network (none for an
    /// application, whose network its declaration sets); `rows` is the
    /// listing as the writer printed it, which a page's digest covers.
    Listed {
        network: Option<Network>,
        directories: Vec<(Access, String)>,
        rows: String,
    },
    /// A change's outcome as `8a` carries it: `00` added, `01` changed,
    /// `02` unchanged or `03` revoked; a network's is `01` or `02`.
    Changed(u8),
    /// How many rows a clear took.
    Cleared(usize),
}

/// Why a request did not get the writer's answer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Failed {
    /// Root's validation or the writer refused, saying why: the human's to
    /// read.
    Refused(String),
    /// Root could not run the writer, it outran its bound, a signal ended
    /// it, it refused root's own arguments or it said what root cannot
    /// read: root's failure, which root logs and the human is not shown.
    Root(String),
}

/// Runs the writer for `request`, as every authority child runs, and reads
/// exactly what it said.
pub(crate) fn perform(owner: u32, request: &Request) -> Result<Written, Failed> {
    perform_with(owner, request, run)
}

fn perform_with(
    owner: u32,
    request: &Request,
    run: impl FnOnce(Command) -> Result<Ran, String>,
) -> Result<Written, Failed> {
    let command = writer(owner, request).map_err(Failed::Refused)?;
    let ran = run(command).map_err(Failed::Root)?;
    match ran.code {
        Some(0) => interpret(request, &ran.output).map_err(Failed::Root),
        Some(USAGE_EXIT) => Err(Failed::Root(
            "td-firstboot grant-table refused root's arguments".into(),
        )),
        None => Err(Failed::Root(
            "td-firstboot grant-table ended by a signal".into(),
        )),
        Some(_) => {
            let line = stderr_tail::last_reason(&ran.said).unwrap_or_default();
            Err(Failed::Refused(crate::message(format_args!(
                "{}",
                line.strip_prefix("td-firstboot: ").unwrap_or(&line)
            ))))
        }
    }
}

/// Root's answer to `2c`: a listing's page (`89`), what a change did
/// (`8a`), the writer's refusal (`8b`), or `ff 02` when root could not run
/// it or read what it said.
pub(crate) fn answer(owner: u32, request: &Request) -> Vec<u8> {
    answer_with(owner, request, run)
}

fn answer_with(
    owner: u32,
    request: &Request,
    run: impl FnOnce(Command) -> Result<Ran, String>,
) -> Vec<u8> {
    let answered = perform_with(owner, request, run)
        .and_then(|written| page(request, &written).map_err(Failed::Root));
    match answered {
        Ok(answer) => answer,
        Err(Failed::Refused(reason)) => refusal(&reason),
        Err(Failed::Root(why)) => {
            let _ = writeln!(std::io::stderr().lock(), "td-authd: grant request: {why}");
            vec![0xff, 2]
        }
    }
}

/// What a successful writer's exact `output` says for `request`.
fn interpret(request: &Request, output: &[u8]) -> Result<Written, String> {
    let malformed = || crate::message(format_args!("td-firstboot grant-table answered malformed"));
    let text = std::str::from_utf8(output).map_err(|_| malformed())?;
    match &request.verb {
        Verb::List { .. } => {}
        Verb::Clear => {
            let count = text
                .strip_prefix(CLEARED)
                .and_then(|rest| rest.strip_suffix('\n'))
                .ok_or_else(malformed)?;
            return count
                .parse::<usize>()
                .ok()
                .filter(|_| crate::workload_accounts::canonical_decimal(count))
                .map(Written::Cleared)
                .ok_or_else(malformed);
        }
        verb => {
            let outcome = OUTCOMES
                .iter()
                .find(|(line, _)| *line == text)
                .map(|(_, byte)| *byte)
                .ok_or_else(malformed)?;
            // A grant never revokes, nor a revoke grants, and a network
            // is changed or stands.
            let fits = match verb {
                Verb::Grant(..) => outcome <= 2,
                Verb::Revoke(_) => outcome == 3,
                Verb::Network(_) => matches!(outcome, 1 | 2),
                Verb::List { .. } | Verb::Clear => false,
            };
            if !fits {
                return Err(malformed());
            }
            return Ok(Written::Changed(outcome));
        }
    }
    let (head, rows) = text
        .strip_prefix(LISTED)
        .and_then(|rest| rest.split_once('\n'))
        .ok_or_else(malformed)?;
    // A sandbox's count is followed by its network; an application's
    // network is its declaration's, which no listing carries.
    let (count, network) = match request.kind {
        Kind::Sandbox => {
            let (count, network) = head.split_once('\t').ok_or_else(malformed)?;
            (count, Some(Network::parse(network).ok_or_else(malformed)?))
        }
        Kind::Application => (head, None),
        // `decode` admits no workspace.
        Kind::Workspace => return Err(malformed()),
    };
    let total = count
        .parse::<usize>()
        .ok()
        .filter(|total| {
            crate::workload_accounts::canonical_decimal(count)
                && *total <= workload_grants::MAX_DIRECTORIES
        })
        .ok_or_else(malformed)?;
    let mut directories = crate::allocation::with_capacity(total).map_err(crate::refused)?;
    for row in rows.split_terminator('\n') {
        let (access, path) = row.split_once('\t').ok_or_else(malformed)?;
        let access = Access::parse(access).ok_or_else(malformed)?;
        // Every row the table holds, one a later validator refuses too.
        workload_grants::table_path(path.as_bytes()).map_err(|_| malformed())?;
        crate::allocation::push(&mut directories, (access, crate::owned(path)?))
            .map_err(crate::refused)?;
    }
    if directories.len() != total || !(rows.is_empty() || rows.ends_with('\n')) {
        return Err(malformed());
    }
    Ok(Written::Listed {
        network,
        directories,
        rows: crate::owned(rows)?,
    })
}

/// `2c`'s answer for what the writer said: `8a` and a change's outcome,
/// or a listing's page from the request's start row, as many rows as fit
/// a frame.
fn page(request: &Request, written: &Written) -> Result<Vec<u8>, String> {
    let malformed = || crate::message(format_args!("td-firstboot grant-table answered malformed"));
    let (start, network, directories, rows) = match (&request.verb, written) {
        (_, Written::Changed(outcome)) => return Ok(vec![0x8a, *outcome]),
        (
            Verb::List { start },
            Written::Listed {
                network,
                directories,
                rows,
            },
        ) => (*start, network, directories, rows),
        _ => return Err(malformed()),
    };
    let network = match network {
        Some(Network::Off) => 0,
        Some(Network::Host) => 1,
        None => NETWORK_DECLARED,
    };
    let total = u8::try_from(directories.len()).map_err(|_| malformed())?;
    let mut answer = vec![0x89, total, network];
    // The whole listing's digest, so pages read from two tables of one
    // count do not pass for one listing.
    answer.extend_from_slice(&listing_digest(rows.as_bytes()).to_be_bytes());
    for (access, path) in directories.iter().skip(start) {
        let length = u16::try_from(path.len()).map_err(|_| malformed())?;
        if answer.len() + 3 + path.len() > FRAME_BYTES {
            break;
        }
        answer.push(match access {
            Access::ReadOnly => 0,
            Access::ReadWrite => 1,
        });
        answer.extend_from_slice(&length.to_be_bytes());
        answer.extend_from_slice(path.as_bytes());
    }
    Ok(answer)
}

/// FNV-1a over an account's whole listing: a generation for its pages, not
/// a security property.
fn listing_digest(rows: &[u8]) -> u64 {
    rows.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// The writer, killed and reaped unless it was reaped already.
struct Writer(Child);
impl Drop for Writer {
    fn drop(&mut self) {
        if let Ok(None) = self.0.try_wait() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

/// Reads what waits on `socket` into `into`, at most `limit` bytes in all
/// (its tail when `tail`); whether it reached end of file.
fn drain(
    socket: &mut UnixStream,
    into: &mut Vec<u8>,
    limit: usize,
    tail: bool,
) -> Result<bool, String> {
    let mut buffer = [0u8; 4096];
    loop {
        match socket.read(&mut buffer) {
            Ok(0) => return Ok(true),
            Ok(count) => {
                let read = buffer.get(..count).unwrap_or_default();
                if tail {
                    stderr_tail::push(into, read);
                } else {
                    into.extend_from_slice(read);
                    if into.len() > limit {
                        return Err("td-firstboot grant-table answered beyond its bound".into());
                    }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(false),
            Err(e) => return Err(format!("read td-firstboot grant-table: {e}")),
        }
    }
}

/// Runs the writer as every authority child runs: null stdin, `/`, no
/// environment, its stdout and stderr private sockets root drains without
/// blocking, all within `WRITER_TIME`.
fn run(mut command: Command) -> Result<Ran, String> {
    let deadline = Instant::now()
        .checked_add(WRITER_TIME)
        .ok_or("grant writer deadline overflow")?;
    let (mut output_wire, output_end) = UnixStream::pair().map_err(|e| e.to_string())?;
    let (mut said_wire, said_end) = UnixStream::pair().map_err(|e| e.to_string())?;
    output_wire
        .set_nonblocking(true)
        .and_then(|()| said_wire.set_nonblocking(true))
        .map_err(|e| e.to_string())?;
    let spawned = command
        .env_clear()
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::from(OwnedFd::from(output_end)))
        .stderr(Stdio::from(OwnedFd::from(said_end)))
        .spawn();
    // The Command keeps its copies of the endpoints until dropped; EOF
    // comes only once the writer is their last holder.
    drop(command);
    let mut writer = Writer(spawned.map_err(|e| format!("start td-firstboot grant-table: {e}"))?);
    let (mut output, mut said) = (Vec::new(), Vec::new());
    let (mut output_done, mut said_done) = (false, false);
    loop {
        if !output_done {
            output_done = drain(&mut output_wire, &mut output, OUTPUT_BYTES, false)?;
        }
        if !said_done {
            said_done = drain(&mut said_wire, &mut said, 0, true)?;
        }
        if output_done && said_done {
            if let Some(status) = writer.0.try_wait().map_err(|e| e.to_string())? {
                return Ok(Ran {
                    code: status.code(),
                    output,
                    said,
                });
            }
        }
        if Instant::now() >= deadline {
            return Err("td-firstboot grant-table did not answer in time".into());
        }
        std::thread::sleep(READ_STEP);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn request(bytes: &[u8]) -> Result<Request, String> {
        Request::decode(bytes)
    }

    fn operands(verb: u8, name: &str, tail: &[u8]) -> Vec<u8> {
        account(verb, 0, name, tail)
    }

    /// `2c`'s operands for the account of kind byte `kind`.
    fn account(verb: u8, kind: u8, name: &str, tail: &[u8]) -> Vec<u8> {
        let mut bytes = vec![verb, kind, name.len() as u8];
        bytes.extend_from_slice(name.as_bytes());
        bytes.extend_from_slice(tail);
        bytes
    }

    #[test]
    fn a_request_is_one_verb_one_account_and_its_operand() {
        assert_eq!(
            request(&operands(0, "firefox", &[0])).unwrap(),
            Request {
                verb: Verb::List { start: 0 },
                kind: Kind::Application,
                name: "firefox".into()
            }
        );
        // A sandbox takes every verb an application does, and its network.
        for (bytes, verb) in [
            (account(0, 1, "default", &[2]), Verb::List { start: 2 }),
            (
                account(2, 1, "default", b"src/td"),
                Verb::Grant(Access::ReadWrite, b"src/td".to_vec()),
            ),
            (
                account(3, 1, "default", b"src/td"),
                Verb::Revoke(b"src/td".to_vec()),
            ),
            (account(4, 1, "default", &[0]), Verb::Network(Network::Off)),
            (account(4, 1, "default", &[1]), Verb::Network(Network::Host)),
        ] {
            assert_eq!(
                request(&bytes).unwrap(),
                Request {
                    verb,
                    kind: Kind::Sandbox,
                    name: "default".into()
                }
            );
        }
        assert_eq!(
            request(&operands(0, "firefox", &[31])).unwrap().verb,
            Verb::List { start: 31 }
        );
        assert_eq!(
            request(&operands(1, "firefox", b"My Pictures"))
                .unwrap()
                .verb,
            Verb::Grant(Access::ReadOnly, b"My Pictures".to_vec())
        );
        assert_eq!(
            request(&operands(2, "web", b"Downloads")).unwrap().verb,
            Verb::Grant(Access::ReadWrite, b"Downloads".to_vec())
        );
        assert_eq!(
            request(&operands(3, "firefox", b"Downloads")).unwrap().verb,
            Verb::Revoke(b"Downloads".to_vec())
        );
        // A reserved path is a well-formed request root refuses by answer.
        assert!(request(&operands(1, "firefox", b".ssh")).is_ok());
        let longest = vec![b'a'; PATH_BYTES];
        assert!(request(&operands(1, "firefox", &longest)).is_ok());
        let mut short = operands(0, "firefox", &[0]);
        short[2] = 3;
        let mut long = operands(0, "firefox", &[0]);
        long[2] = 200;
        for bad in [
            vec![],
            vec![0],
            vec![0, 0],
            vec![0, 0, 0, 0],
            // A workspace takes no grants; no other kind exists.
            account(0, 2, "agent", &[0]),
            account(1, 2, "agent", b"src"),
            account(0, 3, "firefox", &[0]),
            account(0, 0xff, "firefox", &[0]),
            // An application's network is its declaration's.
            account(4, 0, "firefox", &[1]),
            account(4, 0, "firefox", &[0]),
            account(4, 1, "default", &[]),
            account(4, 1, "default", &[2]),
            account(4, 1, "default", &[1, 0]),
            account(4, 1, "default", b"host"),
            account(5, 1, "default", &[0]),
            account(0, 1, "Default", &[0]),
            short,
            long,
            operands(0, "firefox", &[]),
            operands(0, "firefox", &[32]),
            operands(0, "firefox", &[0, 0]),
            operands(0, "Firefox", &[0]),
            operands(0, "", &[0]),
            operands(1, "firefox", &[]),
            operands(3, "firefox", &[]),
            operands(1, "firefox", &vec![b'a'; PATH_BYTES + 1]),
            operands(4, "firefox", b"Downloads"),
        ] {
            assert!(request(&bad).is_err(), "{bad:?}");
        }
    }

    fn args(command: &Command) -> Vec<String> {
        command
            .get_args()
            .map(|arg| arg.to_str().unwrap().to_string())
            .collect()
    }

    /// The writer's argv holds the fixed verb and kind, the owner, and the
    /// peer's name, access and validated path; an invalid path never
    /// reaches it.
    #[test]
    fn the_writer_runs_with_exactly_the_validated_operands() {
        let list = Request::decode(&operands(0, "firefox", &[3])).unwrap();
        let command = writer(1000, &list).unwrap();
        assert_eq!(command.get_program(), "/bin/td-firstboot");
        assert_eq!(
            args(&command),
            ["grant-table", "1000", "list", "application", "firefox"]
        );
        let grant = Request::decode(&operands(1, "firefox", b"My Pictures")).unwrap();
        assert_eq!(
            args(&writer(1000, &grant).unwrap()),
            [
                "grant-table",
                "1000",
                "grant",
                "application",
                "firefox",
                "ro",
                "My Pictures"
            ]
        );
        let revoke = Request::decode(&operands(3, "firefox", b"Downloads")).unwrap();
        assert_eq!(
            args(&writer(1000, &revoke).unwrap()),
            [
                "grant-table",
                "1000",
                "revoke",
                "application",
                "firefox",
                "Downloads"
            ]
        );
        // A sandbox's request names its kind; its network is one fixed
        // word, without a path.
        for (bytes, expected) in [
            (
                account(0, 1, "default", &[0]),
                &["list", "sandbox", "default"][..],
            ),
            (
                account(2, 1, "default", b"src/td"),
                &["grant", "sandbox", "default", "rw", "src/td"],
            ),
            (
                account(3, 1, "default", b"src/td"),
                &["revoke", "sandbox", "default", "src/td"],
            ),
            (
                account(4, 1, "default", &[1]),
                &["network", "sandbox", "default", "host"],
            ),
            (
                account(4, 1, "default", &[0]),
                &["network", "sandbox", "default", "off"],
            ),
        ] {
            let request = Request::decode(&bytes).unwrap();
            let mut argv = vec!["grant-table".to_string(), "1000".into()];
            argv.extend(expected.iter().map(|arg| (*arg).to_string()));
            assert_eq!(args(&writer(1000, &request).unwrap()), argv);
        }
        // A grant names only a path this deployment publishes; a revoke
        // any the table can hold, so a row a later validator refuses can
        // be taken back.
        for (path, revocable) in [
            (&b".ssh"[..], true),
            (b".local", true),
            (b"a/../b", true),
            (b"/etc", true),
            ("Down\u{202E}loads".as_bytes(), true),
            (b"a\0b", false),
            (b"a\x07b", false),
            (b"a\xffb", false),
        ] {
            for (verb, kind) in [(1, 0), (3, 0), (2, 1), (3, 1)] {
                let request = Request::decode(&account(verb, kind, "firefox", path)).unwrap();
                let answer = writer(1000, &request);
                if verb == 3 && revocable {
                    assert!(answer.is_ok(), "{path:?}");
                    continue;
                }
                let reason = answer.unwrap_err();
                assert!(!reason.is_empty(), "{path:?}");
                // A writer that ran would answer `ff 02`, never the refusal.
                let answer = answer_with(1000, &request, |_| Err("the writer ran".into()));
                assert_eq!(answer[0], 0x8b, "{path:?}");
                assert!(answer[1..].iter().all(|b| (b' '..=b'~').contains(b)));
                assert!(answer.len() <= 1 + REASON_BYTES);
            }
        }
    }

    /// The intake's deletion clears a sandbox's rows through the same
    /// writer, with the fixed word `clear` and no path, and reads back
    /// how many rows it took; no `2c` request names it.
    #[test]
    fn a_clear_runs_the_writer_and_reads_back_its_count() {
        let clear = Request {
            verb: Verb::Clear,
            kind: Kind::Sandbox,
            name: "oracle".into(),
        };
        assert_eq!(
            args(&writer(1000, &clear).unwrap()),
            ["grant-table", "1000", "clear", "sandbox", "oracle"]
        );
        let perform = |output: &str| perform_with(1000, &clear, |_| ran(true, output, ""));
        assert_eq!(perform("TD-GRANT-CLEARED\t3\n"), Ok(Written::Cleared(3)));
        assert_eq!(perform("TD-GRANT-CLEARED\t0\n"), Ok(Written::Cleared(0)));
        for output in [
            "TD-GRANT-CLEARED\t03\n",
            "TD-GRANT-CLEARED\t3",
            "TD-GRANT-CLEARED\t\n",
            "TD-GRANT-REVOKED\n",
            "TD-GRANTS\t0\toff\n",
        ] {
            assert!(
                matches!(perform(output), Err(Failed::Root(_))),
                "{output:?}"
            );
        }
        assert_eq!(
            perform_with(1000, &clear, |_| ran(false, "", "td-firstboot: busy\n")),
            Err(Failed::Refused("busy".into()))
        );
        // No `2c` operands decode to a clear.
        for verb in 0..=255u8 {
            if let Ok(request) = Request::decode(&account(verb, 1, "oracle", &[0])) {
                assert_ne!(request.verb, Verb::Clear);
            }
        }
        // A clear has no page: `2c` never asked for one.
        assert!(page(&clear, &Written::Cleared(1)).is_err());
    }

    /// The intake's listing and changes read exactly what `2c` reads.
    #[test]
    fn the_intake_reads_what_2c_reads() {
        let list = Request {
            verb: Verb::List { start: 0 },
            kind: Kind::Sandbox,
            name: "oracle".into(),
        };
        assert_eq!(
            perform_with(1000, &list, |_| ran(
                true,
                "TD-GRANTS\t1\thost\nrw\tsrc\n",
                ""
            )),
            Ok(Written::Listed {
                network: Some(Network::Host),
                directories: vec![(Access::ReadWrite, "src".into())],
                rows: "rw\tsrc\n".into(),
            })
        );
        let grant = Request {
            verb: Verb::Grant(Access::ReadOnly, b"src".to_vec()),
            kind: Kind::Sandbox,
            name: "oracle".into(),
        };
        assert_eq!(
            perform_with(1000, &grant, |_| ran(true, "TD-GRANT-ADDED\n", "")),
            Ok(Written::Changed(0))
        );
        assert!(matches!(
            perform_with(1000, &grant, |_| Err("did not answer in time".into())),
            Err(Failed::Root(_))
        ));
    }

    fn ran(success: bool, output: &str, said: &str) -> Result<Ran, String> {
        Ok(Ran {
            code: Some(i32::from(!success)),
            output: output.as_bytes().to_vec(),
            said: said.as_bytes().to_vec(),
        })
    }

    fn answered(bytes: &[u8], ran: Result<Ran, String>) -> Vec<u8> {
        answer_with(1000, &Request::decode(bytes).unwrap(), |_| ran)
    }

    #[test]
    fn a_change_answers_what_the_writer_did_and_a_refusal_its_reason() {
        let grant = operands(1, "firefox", b"Pictures");
        let revoke = operands(3, "firefox", b"Pictures");
        for (line, byte) in [
            ("TD-GRANT-ADDED\n", 0),
            ("TD-GRANT-CHANGED\n", 1),
            ("TD-GRANT-UNCHANGED\n", 2),
        ] {
            assert_eq!(answered(&grant, ran(true, line, "")), [0x8a, byte]);
            assert_eq!(answered(&revoke, ran(true, line, "")), [0xff, 2]);
        }
        assert_eq!(
            answered(&revoke, ran(true, "TD-GRANT-REVOKED\n", "")),
            [0x8a, 3]
        );
        assert_eq!(
            answered(&grant, ran(true, "TD-GRANT-REVOKED\n", "")),
            [0xff, 2]
        );
        for output in ["", "TD-GRANT-ADDED", "TD-GRANT-ADDED\nextra\n", "ok\n"] {
            assert_eq!(answered(&grant, ran(true, output, "")), [0xff, 2]);
        }
        let refused = answered(
            &grant,
            ran(false, "", "td-firstboot: ~/Pictures does not exist\n"),
        );
        assert_eq!(refused, b"\x8b~/Pictures does not exist");
        let quoted = answered(
            &grant,
            ran(
                false,
                "",
                "td-firstboot: application \"mail\" runs as you\n",
            ),
        );
        assert_eq!(quoted, b"\x8bapplication \"mail\" runs as you");
        assert_eq!(answered(&grant, ran(false, "", "")), {
            let mut silent = vec![0x8b];
            silent.extend_from_slice(b"the grant table's writer refused");
            silent
        });
        assert_eq!(
            answered(&grant, Err("did not answer in time".into())),
            [0xff, 2]
        );
        // Arguments the writer refuses are root's fault, not the human's:
        // the usage text that ends its stderr is no reason to show.
        let usage = Ok(Ran {
            code: Some(USAGE_EXIT),
            output: Vec::new(),
            said: b"td-firstboot: bad\nusage: td-firstboot ...\n".to_vec(),
        });
        assert_eq!(answered(&grant, usage), [0xff, 2]);
        let killed = Ok(Ran {
            code: None,
            output: Vec::new(),
            said: Vec::new(),
        });
        assert_eq!(answered(&grant, killed), [0xff, 2]);
        // A sandbox's network is changed or stands; it is never added or
        // revoked.
        let network = account(4, 1, "default", &[1]);
        for (line, answer) in [
            ("TD-GRANT-CHANGED\n", &[0x8a, 1][..]),
            ("TD-GRANT-UNCHANGED\n", &[0x8a, 2]),
            ("TD-GRANT-ADDED\n", &[0xff, 2]),
            ("TD-GRANT-REVOKED\n", &[0xff, 2]),
            ("TD-GRANTS\t0\toff\n", &[0xff, 2]),
        ] {
            assert_eq!(answered(&network, ran(true, line, "")), answer, "{line}");
        }
        let refused = answered(
            &network,
            ran(
                false,
                "",
                "td-firstboot: sandbox \"default\" has no account yet\n",
            ),
        );
        assert_eq!(refused, b"\x8bsandbox \"default\" has no account yet");
    }

    /// An `89` page's head: the count, the network byte and the digest.
    fn page_head(total: u8, network: u8, rows: &str) -> Vec<u8> {
        let mut head = vec![0x89, total, network];
        head.extend_from_slice(&listing_digest(rows.as_bytes()).to_be_bytes());
        head
    }

    #[test]
    fn a_listing_is_counted_exact_and_paged_to_the_frame() {
        let list = |start: u8| operands(0, "firefox", &[start]);
        let head = |total: u8, rows: &str| page_head(total, NETWORK_DECLARED, rows);
        assert_eq!(
            answered(&list(0), ran(true, "TD-GRANTS\t0\n", "")),
            head(0, "")
        );
        let two = "ro\tMy Pictures\nrw\tDownloads\n";
        let output = format!("TD-GRANTS\t2\n{two}");
        assert_eq!(
            answered(&list(0), ran(true, &output, "")),
            [
                &head(2, two)[..],
                b"\x00\x00\x0bMy Pictures\x01\x00\x09Downloads"
            ]
            .concat()
        );
        assert_eq!(
            answered(&list(1), ran(true, &output, "")),
            [&head(2, two)[..], b"\x01\x00\x09Downloads"].concat()
        );
        assert_eq!(
            answered(&list(5), ran(true, "TD-GRANTS\t2\nro\ta\nrw\tb\n", "")),
            head(2, "ro\ta\nrw\tb\n")
        );
        // Two tables of one count differ in their digest.
        assert_ne!(head(1, "ro\ta\n"), head(1, "ro\tb\n"));
        // A row a later validator refuses is listed: it can be revoked.
        assert_eq!(
            answered(&list(0), ran(true, "TD-GRANTS\t1\nro\t.ssh\n", "")),
            [&head(1, "ro\t.ssh\n")[..], b"\x00\x00\x04.ssh"].concat()
        );
        for output in [
            "TD-GRANTS\t1\n",
            "TD-GRANTS\t1\nro\ta\nrw\tb\n",
            "TD-GRANTS\t01\nro\ta\n",
            "TD-GRANTS\t33\n",
            "TD-GRANTS\t1\nrx\ta\n",
            "TD-GRANTS\t1\nro\ta\u{7}b\n",
            "TD-GRANTS\t1\nro\ta",
            "TD-GRANTS\t1\nro\ta\textra\n",
            "TD-GRANTS 1\nro\ta\n",
            "TD-GRANT-ADDED\n",
            // An application's listing carries no network.
            "TD-GRANTS\t0\toff\n",
            "TD-GRANTS\t1\thost\nro\ta\n",
        ] {
            assert_eq!(
                answered(&list(0), ran(true, output, "")),
                [0xff, 2],
                "{output:?}"
            );
        }
        // A sandbox's carries its network, which the page's head holds.
        let sandbox = |start: u8| account(0, 1, "default", &[start]);
        assert_eq!(
            answered(&sandbox(0), ran(true, "TD-GRANTS\t0\toff\n", "")),
            page_head(0, 0, "")
        );
        assert_eq!(
            answered(&sandbox(0), ran(true, "TD-GRANTS\t1\thost\nrw\tsrc\n", "")),
            [&page_head(1, 1, "rw\tsrc\n")[..], b"\x01\x00\x03src"].concat()
        );
        // A later page repeats the network beside the count and digest.
        let two = "ro\ta\nrw\tsrc\n";
        let output = format!("TD-GRANTS\t2\thost\n{two}");
        assert_eq!(
            answered(&sandbox(1), ran(true, &output, "")),
            [&page_head(2, 1, two)[..], b"\x01\x00\x03src"].concat()
        );
        assert_eq!(
            answered(&sandbox(2), ran(true, &output, "")),
            page_head(2, 1, two)
        );
        for output in [
            "TD-GRANTS\t0\n",
            "TD-GRANTS\t0\ton\n",
            "TD-GRANTS\t0\toff\thost\n",
            "TD-GRANTS\t0\t\n",
            "TD-GRANTS\toff\t0\n",
        ] {
            assert_eq!(
                answered(&sandbox(0), ran(true, output, "")),
                [0xff, 2],
                "{output:?}"
            );
        }
        // Long rows: each page holds what fits a frame, and the pages
        // together hold every row once.
        let long: Vec<String> = (0..8)
            .map(|index| format!("{index}{}", "n".repeat(250)))
            .map(|name| [name.as_str(); 4].join("/"))
            .collect();
        let mut output = format!("TD-GRANTS\t{}\n", long.len());
        for path in &long {
            output.push_str(&format!("rw\t{path}\n"));
        }
        let mut seen = Vec::new();
        while seen.len() < long.len() {
            let page = answered(&list(seen.len() as u8), ran(true, &output, ""));
            assert!(page.len() <= FRAME_BYTES);
            assert_eq!(&page[..3], [0x89, 8, NETWORK_DECLARED]);
            let mut rest = &page[11..];
            let before = seen.len();
            while let [1, high, low, tail @ ..] = rest {
                let length = usize::from(u16::from_be_bytes([*high, *low]));
                seen.push(String::from_utf8(tail[..length].to_vec()).unwrap());
                rest = &tail[length..];
            }
            assert!(rest.is_empty());
            assert!(seen.len() > before);
        }
        assert_eq!(seen, long);
    }

    /// A writer that does not answer in time is killed and reaped, and
    /// root says so in time of its own.
    #[test]
    fn a_writer_past_its_bound_is_killed_and_reaped() {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command.args([
            "--exact",
            "grant_requests::tests::writer_sleep_fixture",
            "--ignored",
        ]);
        let started = Instant::now();
        let error = run(command).err().unwrap();
        assert!(error.contains("in time"), "{error}");
        assert!(started.elapsed() < WRITER_TIME + Duration::from_secs(2));
    }

    #[test]
    #[ignore = "exec-only writer the bound test kills"]
    fn writer_sleep_fixture() {
        std::thread::sleep(Duration::from_secs(30));
    }

    /// This test binary run as the writer: one ignored fixture, which
    /// writes past the harness's own first lines and exits.
    fn fixture(name: &str) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command.args([
            "--exact",
            &format!("grant_requests::tests::{name}"),
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ]);
        command
    }

    /// A writer that answers is read to its end with its exit status; one
    /// that fails leaves its last diagnostic; one that writes past the
    /// bound is refused, and each is reaped.
    #[test]
    fn a_writer_is_read_whole_within_its_bounds() {
        let ran = run(fixture("writer_answer_fixture")).unwrap();
        assert_eq!(ran.code, Some(0));
        assert!(ran.output.ends_with(b"TD-GRANT-ADDED\n"));
        let ran = run(fixture("writer_refusal_fixture")).unwrap();
        assert_eq!(ran.code, Some(1));
        assert_eq!(
            stderr_tail::last_reason(&ran.said).as_deref(),
            Some("td-firstboot: ~/Pictures does not exist")
        );
        let error = run(fixture("writer_flood_fixture")).err().unwrap();
        assert!(error.contains("beyond its bound"), "{error}");
    }

    #[test]
    #[ignore = "exec-only writer"]
    fn writer_answer_fixture() {
        use std::io::Write;
        std::io::stdout().write_all(b"TD-GRANT-ADDED\n").unwrap();
        std::io::stdout().flush().unwrap();
        std::process::exit(0);
    }

    #[test]
    #[ignore = "exec-only writer"]
    fn writer_refusal_fixture() {
        use std::io::Write;
        std::io::stderr()
            .write_all(b"td-firstboot: ~/Pictures does not exist\n")
            .unwrap();
        std::process::exit(1);
    }

    #[test]
    #[ignore = "exec-only writer"]
    fn writer_flood_fixture() {
        use std::io::Write;
        let _ = std::io::stdout().write_all(&vec![b'x'; OUTPUT_BYTES + 1]);
        std::process::exit(0);
    }
}
