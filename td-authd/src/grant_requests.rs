//! The human's grant requests, root's serving half (td-authd/DESIGN.md,
//! "Grant requests"): request `2c` runs td-firstboot's `grant-table`, the
//! grant table's one writer, for one validated account and directory,
//! waits for it within a bound, and answers with the account's directory
//! rows or what the request changed. Nothing here elevates: a grant
//! delegates the human's authority over the human's own files.

use crate::stderr_tail;
use crate::workload_grants::{self, Access};
use std::io::{self, Read};
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
const PATH_BYTES: usize = 1024;
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

/// What a request asks of an application account's directory rows.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Verb {
    /// Its rows from the `start`th on, as many as one answer holds.
    List {
        start: usize,
    },
    Grant(Access, Vec<u8>),
    Revoke(Vec<u8>),
}

/// One request: a verb and an application name in the registry grammar.
/// The kind is always `application`: only application accounts take
/// grants until sandbox and workspace launches go through root.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Request {
    pub verb: Verb,
    pub name: String,
}

impl Request {
    /// `2c`'s operands: a verb byte, the kind byte `00`, the name's length
    /// and the name, then a list's start index or a path of one to 1024
    /// bytes. The path's rules are the table's, answered as a refusal.
    pub(crate) fn decode(operands: &[u8]) -> Result<Self, String> {
        let invalid = || "invalid grant request".to_string();
        let [verb, 0, length, rest @ ..] = operands else {
            return Err(invalid());
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
            _ => return Err(invalid()),
        };
        Ok(Self {
            verb,
            name: name.to_string(),
        })
    }
}

/// The writer's run for `request`, or the refusal its path earns without
/// one: the fixed program, the owner, and the peer's verb, name, access
/// and path, validated, as its arguments.
fn writer(owner: u32, request: &Request) -> Result<Command, Vec<u8>> {
    // A grant names a path this deployment publishes; a revoke any row the
    // table can hold, so one a later validator refuses can be taken back.
    let path = match &request.verb {
        Verb::List { .. } => None,
        Verb::Grant(_, path) => Some(path),
        Verb::Revoke(path) => {
            workload_grants::table_path(path).map_err(|why| refusal(&why))?;
            Some(path)
        }
    };
    if let Verb::Grant(_, path) = &request.verb {
        workload_grants::validate_path(path).map_err(|why| refusal(&why))?;
    }
    let path = match path {
        Some(path) => {
            Some(std::str::from_utf8(path).map_err(|_| refusal("the path is not UTF-8"))?)
        }
        None => None,
    };
    let verb = match &request.verb {
        Verb::List { .. } => "list",
        Verb::Grant(..) => "grant",
        Verb::Revoke(_) => "revoke",
    };
    let mut command = Command::new("/bin/td-firstboot");
    command.args([
        "grant-table",
        &owner.to_string(),
        verb,
        "application",
        &request.name,
    ]);
    if let Verb::Grant(access, _) = &request.verb {
        command.args([access.as_str()]);
    }
    command.args(path);
    Ok(command)
}

/// `8b` and the reason, printable ASCII, cut to its bound.
fn refusal(reason: &str) -> Vec<u8> {
    let reason: String = crate::flathub_app::reason_text(reason)
        .chars()
        .take(REASON_BYTES)
        .collect();
    let reason = if reason.is_empty() {
        "the grant table's writer refused".to_string()
    } else {
        reason
    };
    let mut answer = vec![0x8b];
    answer.extend_from_slice(reason.as_bytes());
    answer
}

/// What the writer left: its output, its last diagnostic bytes and its
/// exit status, `None` when a signal ended it.
struct Ran {
    code: Option<i32>,
    output: Vec<u8>,
    said: Vec<u8>,
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
    let command = match writer(owner, request) {
        Ok(command) => command,
        Err(answer) => return answer,
    };
    let interpreted = run(command).and_then(|ran| {
        match ran.code {
            Some(USAGE_EXIT) => {
                return Err("td-firstboot grant-table refused root's arguments".into())
            }
            None => return Err("td-firstboot grant-table ended by a signal".into()),
            Some(_) => {}
        }
        if ran.code != Some(0) {
            let line = stderr_tail::last_reason(&ran.said).unwrap_or_default();
            return Ok(refusal(
                line.strip_prefix("td-firstboot: ").unwrap_or(&line),
            ));
        }
        interpret(&request.verb, &ran.output)
    });
    interpreted.unwrap_or_else(|why| {
        let _ = std::io::Write::write_all(
            &mut std::io::stderr().lock(),
            format!("td-authd: grant request: {why}\n").as_bytes(),
        );
        vec![0xff, 2]
    })
}

/// The answer a successful writer's exact `output` makes for `verb`.
fn interpret(verb: &Verb, output: &[u8]) -> Result<Vec<u8>, String> {
    let malformed = || "td-firstboot grant-table answered malformed".to_string();
    let text = std::str::from_utf8(output).map_err(|_| malformed())?;
    let start = match verb {
        Verb::List { start } => *start,
        Verb::Grant(..) | Verb::Revoke(_) => {
            let outcome = OUTCOMES
                .iter()
                .find(|(line, _)| *line == text)
                .map(|(_, byte)| *byte)
                .ok_or_else(malformed)?;
            // A grant never revokes, nor a revoke grants.
            if (outcome == 3) != matches!(verb, Verb::Revoke(_)) {
                return Err(malformed());
            }
            return Ok(vec![0x8a, outcome]);
        }
    };
    let (count, rows) = text
        .strip_prefix(LISTED)
        .and_then(|rest| rest.split_once('\n'))
        .ok_or_else(malformed)?;
    let total = count
        .parse::<usize>()
        .ok()
        .filter(|total| total.to_string() == count && *total <= workload_grants::MAX_DIRECTORIES)
        .ok_or_else(malformed)?;
    let mut listed = Vec::with_capacity(total);
    for row in rows.split_terminator('\n') {
        let (access, path) = row.split_once('\t').ok_or_else(malformed)?;
        let access = Access::parse(access).ok_or_else(malformed)?;
        // Every row the table holds, one a later validator refuses too.
        workload_grants::table_path(path.as_bytes()).map_err(|_| malformed())?;
        listed.push((access, path));
    }
    if listed.len() != total || !(rows.is_empty() || rows.ends_with('\n')) {
        return Err(malformed());
    }
    let mut answer = vec![0x89, u8::try_from(total).map_err(|_| malformed())?];
    // The whole listing's digest, so pages read from two tables of one
    // count do not pass for one listing.
    answer.extend_from_slice(&listing_digest(rows.as_bytes()).to_be_bytes());
    for (access, path) in listed.iter().skip(start) {
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
        let mut bytes = vec![verb, 0, name.len() as u8];
        bytes.extend_from_slice(name.as_bytes());
        bytes.extend_from_slice(tail);
        bytes
    }

    #[test]
    fn a_request_is_one_verb_the_application_kind_one_name_and_its_operand() {
        assert_eq!(
            request(&operands(0, "firefox", &[0])).unwrap(),
            Request {
                verb: Verb::List { start: 0 },
                name: "firefox".into()
            }
        );
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
        let mut sandbox = operands(0, "firefox", &[0]);
        sandbox[1] = 1;
        let mut short = operands(0, "firefox", &[0]);
        short[2] = 3;
        let mut long = operands(0, "firefox", &[0]);
        long[2] = 200;
        for bad in [
            vec![],
            vec![0],
            vec![0, 0],
            vec![0, 0, 0, 0],
            sandbox,
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
            for verb in [1, 3] {
                let request = Request::decode(&operands(verb, "firefox", path)).unwrap();
                let answer = writer(1000, &request);
                if verb == 3 && revocable {
                    assert!(answer.is_ok(), "{path:?}");
                    continue;
                }
                let answer = answer.unwrap_err();
                assert_eq!(answer[0], 0x8b, "{path:?}");
                assert!(answer[1..].iter().all(|b| (b' '..=b'~').contains(b)));
                assert!(answer.len() <= 1 + REASON_BYTES);
            }
        }
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
    }

    #[test]
    fn a_listing_is_counted_exact_and_paged_to_the_frame() {
        let list = |start: u8| operands(0, "firefox", &[start]);
        let head = |total: u8, rows: &str| {
            let mut head = vec![0x89, total];
            head.extend_from_slice(&listing_digest(rows.as_bytes()).to_be_bytes());
            head
        };
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
        ] {
            assert_eq!(
                answered(&list(0), ran(true, output, "")),
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
            assert_eq!(&page[..2], [0x89, 8]);
            let mut rest = &page[10..];
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
