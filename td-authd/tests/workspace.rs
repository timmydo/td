#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use super::*;
use std::cell::{Cell, RefCell};
use std::os::unix::process::ExitStatusExt;

fn words(command: &Command) -> Vec<String> {
    command
        .get_args()
        .map(|arg| arg.to_str().unwrap().to_string())
        .collect()
}

/// A `start` request's bytes, as td-sandbox sends them.
fn encode(name: &str, launch: &Launch) -> Vec<u8> {
    format!(
        "start {name} {} {}\n",
        launch.runtime.unwrap_or("native"),
        launch.term
    )
    .into_bytes()
}

/// A `run` word, as td-sandbox encodes one.
fn word(argument: &str) -> String {
    let mut word = String::from("x");
    for byte in argument.bytes() {
        word.push_str(&format!("{byte:02x}"));
    }
    word
}

fn shell(runtime: Option<&'static str>, term: &str) -> Verb {
    Verb::Launch(Launch {
        runtime,
        mode: Mode::Pty,
        term: term.into(),
        command: None,
    })
}

/// A request is one exact line: the verb, a ledger name, and exactly the
/// verb's operands, and nothing else.
#[test]
fn a_request_is_one_exact_bounded_line() {
    let request = Request::parse(b"start default native xterm-256color\n").unwrap();
    assert_eq!(
        request,
        Request {
            name: "default".into(),
            verb: shell(None, "xterm-256color"),
        }
    );
    let Verb::Launch(launch) = &request.verb else {
        panic!("{request:?}");
    };
    assert_eq!(
        encode(&request.name, launch),
        b"start default native xterm-256color\n".to_vec()
    );
    let runtime = Request::parse(b"start tools freedesktop-sdk-25-08 linux\n").unwrap();
    assert_eq!(runtime.verb, shell(Some("freedesktop-sdk-25-08"), "linux"));
    // A command's words are hex, so any argument is one word: an empty
    // one, one with spaces, a newline or a quote.
    let line = format!(
        "run oracle native pipes dumb {} {} {} {}\n",
        word("/bin/sh"),
        word("-c"),
        word("echo 'a b'\nexit 3"),
        word("")
    );
    assert_eq!(
        Request::parse(line.as_bytes()).unwrap(),
        Request {
            name: "oracle".into(),
            verb: Verb::Launch(Launch {
                runtime: None,
                mode: Mode::Pipes,
                term: "dumb".into(),
                command: Some(vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    "echo 'a b'\nexit 3".into(),
                    String::new(),
                ]),
            }),
        }
    );
    let pty = format!("run tools freedesktop-sdk-25-08 pty xterm {}\n", word("ls"));
    assert!(matches!(
        Request::parse(pty.as_bytes()).unwrap().verb,
        Verb::Launch(Launch {
            mode: Mode::Pty,
            runtime: Some("freedesktop-sdk-25-08"),
            ..
        })
    ));
    for (line, verb) in [
        ("create default\n", Verb::Create),
        ("delete default\n", Verb::Delete),
        ("status default\n", Verb::Status),
        (
            "grants default\n",
            Verb::Grants(grant_requests::Verb::List { start: 0 }),
        ),
        (
            "grant default rw src/my project\n",
            Verb::Grants(grant_requests::Verb::Grant(
                Access::ReadWrite,
                b"src/my project".to_vec(),
            )),
        ),
        // A reserved path is the writer's to refuse, by its reason.
        (
            "grant default ro .ssh\n",
            Verb::Grants(grant_requests::Verb::Grant(
                Access::ReadOnly,
                b".ssh".to_vec(),
            )),
        ),
        (
            "revoke default src/my project\n",
            Verb::Grants(grant_requests::Verb::Revoke(b"src/my project".to_vec())),
        ),
        (
            "network default host\n",
            Verb::Grants(grant_requests::Verb::Network(Network::Host)),
        ),
        (
            "network default off\n",
            Verb::Grants(grant_requests::Verb::Network(Network::Off)),
        ),
    ] {
        assert_eq!(
            Request::parse(line.as_bytes()).unwrap(),
            Request {
                name: "default".into(),
                verb
            },
            "{line:?}"
        );
    }
    let longest = format!(
        "grant default rw {}\n",
        "a".repeat(grant_requests::PATH_BYTES)
    );
    assert!(Request::parse(longest.as_bytes()).is_ok());
    let too_long = format!(
        "grant default rw {}\n",
        "a".repeat(grant_requests::PATH_BYTES + 1)
    );
    let too_many = format!(
        "run default native pipes dumb {}\n",
        vec![word("a"); MAX_ARGUMENTS + 1].join(" ")
    );
    let most = format!(
        "run default native pipes dumb {}\n",
        vec![word("a"); MAX_ARGUMENTS].join(" ")
    );
    assert!(Request::parse(most.as_bytes()).is_ok());
    for refused in [
        &b"start default native xterm"[..],
        b"start default native xterm\n\n",
        b"start default native xterm\nmore",
        b"start  default native xterm\n",
        b"start default native\n",
        b"start default native xterm extra\n",
        b"shell default native xterm\n",
        b"create default native xterm\n",
        b"Start default native xterm\n",
        b"start 1box native xterm\n",
        b"start -x native xterm\n",
        b"start Default native xterm\n",
        b"start default org.freedesktop.Sdk xterm\n",
        b"start default Native xterm\n",
        b"start default native \n",
        b"start default native xterm;rm\n",
        b"start default native xt\xffrm\n",
        b"create\n",
        b"create \n",
        b"create 1box\n",
        b"create default extra\n",
        b"delete -x\n",
        b"status default x\n",
        b"grants default x\n",
        b"grant default rx src\n",
        b"grant default rw\n",
        b"grant default rw \n",
        b"grant default rw a\tb\n",
        b"grant default rw a\x7fb\n",
        b"grant 1box rw src\n",
        b"revoke default\n",
        b"revoke default \n",
        b"network default on\n",
        b"network default host x\n",
        b"clear default\n",
        b"run default native pipes dumb\n",
        b"run default native pipes dumb \n",
        b"run default native tty dumb x6c73\n",
        b"run default native pipes dumb ls\n",
        b"run default native pipes dumb x6\n",
        b"run default native pipes dumb x6C73\n",
        b"run default native pipes dumb x+f\n",
        b"run default native pipes dumb x\n",
        b"run default native pipes dumb x6c00\n",
        b"run default native pipes dumb xff\n",
        b"run default native pipes dumb x6c73  x61\n",
        b"run default nonsense pipes dumb x6c73\n",
        b"run default native pipes dumb; x6c73\n",
        too_long.as_bytes(),
        too_many.as_bytes(),
    ] {
        assert!(Request::parse(refused).is_err(), "{refused:?}");
    }
    let long = format!("start default native {}\n", "x".repeat(MAX_TERM + 1));
    assert!(Request::parse(long.as_bytes()).is_err());
    let name = "a".repeat(65);
    assert!(Request::parse(format!("start {name} native xterm\n").as_bytes()).is_err());
    let oversized = vec![b'a'; MAX_REQUEST + 1];
    assert!(Request::parse(&oversized).is_err());
}

/// The request is read to its newline and no further, so what follows is
/// left for the jail, within its bound and deadline, and the socket's
/// timeout is cleared before the jail inherits it.
#[test]
fn the_request_is_read_exactly_within_its_bound_and_deadline() {
    let (mut root, mut peer) = UnixStream::pair().unwrap();
    peer.write_all(b"start default native xterm\n\x01\x00\x00\x00\x00")
        .unwrap();
    let request = read_request(&mut root).unwrap();
    assert_eq!(request.name, "default");
    assert_eq!(root.read_timeout().unwrap(), None);
    let mut rest = [0u8; 5];
    root.read_exact(&mut rest).unwrap();
    assert_eq!(rest, [1, 0, 0, 0, 0]);

    let (mut root, mut peer) = UnixStream::pair().unwrap();
    let writer = std::thread::spawn(move || {
        let _ = peer.write_all(&[b'a'; MAX_REQUEST + 8]);
        peer
    });
    assert!(read_request(&mut root).unwrap_err().contains("too long"));
    drop(root);
    drop(writer.join().unwrap());

    let (mut root, peer) = UnixStream::pair().unwrap();
    drop(peer);
    assert!(read_request(&mut root).unwrap_err().contains("ended early"));

    // A peer that sends part of a line and holds the connection open is
    // given up at the deadline, not waited for.
    let (mut root, mut peer) = UnixStream::pair().unwrap();
    peer.write_all(b"start default").unwrap();
    let begun = Instant::now();
    assert!(read_request(&mut root).unwrap_err().contains("timed out"));
    assert!(begun.elapsed() >= REQUEST_DEADLINE);
    assert!(begun.elapsed() < REQUEST_DEADLINE + Duration::from_secs(2));
    drop(peer);
}

/// Only the primary human's programs are admitted, by the peer's kernel
/// credentials: not root, an application's account, a sandbox's own
/// account, or a peer whose credentials cannot be read.
#[test]
fn only_the_primary_human_is_admitted() {
    assert!(admitted(Ok(1000)));
    for uid in [0, 999, 1001, 65536, 98303, 98304, 131071, u32::MAX] {
        assert!(!admitted(Ok(uid)), "{uid}");
    }
    assert!(!admitted(Err(io::Error::other("no credentials"))));
}

/// A refusal is one printable line within its bound, and never without a
/// reason, which td-sandbox would take for an answer amiss.
#[test]
fn a_refusal_is_one_bounded_printable_line() {
    assert_eq!(
        refusal("sandbox x has no account").unwrap(),
        b"refused sandbox x has no account\n"
    );
    assert_eq!(refusal("").unwrap(), b"refused out of memory\n");
    let line = refusal(&format!("{}\n\u{1b}[2J{}", "a", "b".repeat(400))).unwrap();
    assert!(line.starts_with(b"refused a??[2J"));
    assert_eq!(line.len(), "refused ".len() + MAX_REASON + 1);
    assert_eq!(line.iter().filter(|byte| **byte == b'\n').count(), 1);
    assert!(line
        .iter()
        .all(|byte| (b' '..=b'~').contains(byte) || *byte == b'\n'));
}

/// Only a sandbox's own ledger row admits a launch: none is added, and an
/// application or workspace of the same name is no sandbox.
#[test]
fn a_launch_needs_the_sandboxs_own_ledger_row() {
    let ledger = accounts::Ledger::parse(concat!(
        "td-accounts-v1\n",
        "application\tfirefox\t65536\n",
        "sandbox\tdefault\t98304\n",
        "workspace\tagent\t98305\n",
    ))
    .unwrap();
    assert_eq!(sandbox_uid(&ledger, "default"), Ok(98304));
    for name in ["firefox", "agent", "tools"] {
        let error = sandbox_uid(&ledger, name).unwrap_err();
        assert!(error.contains("has no account"), "{error}");
    }
}

/// The spec names the profile the network row selects, the runtime, the
/// entry and the account's home, and nothing the jail would bind of the
/// human's: those are the manifest's views.
#[test]
fn the_spec_maps_the_network_row_and_names_only_the_account() {
    let entry = Path::new("/td/store/abc-td-sandbox/bin/td-sandbox");
    let home = Kind::Sandbox.home(98304);
    assert_eq!(
        spec_text(Network::Off, None, entry, &home).unwrap(),
        "format=1\nprofile=development-offline\nentry=/td/store/abc-td-sandbox/bin/td-sandbox\nhome=/var/lib/td/workloads/98304\n"
    );
    assert_eq!(
        spec_text(Network::Host, Some("freedesktop-platform-25-08"), entry, &home).unwrap(),
        "format=1\nprofile=development\nruntime=freedesktop-platform-25-08\nentry=/td/store/abc-td-sandbox/bin/td-sandbox\nhome=/var/lib/td/workloads/98304\n"
    );
    assert!(spec_text(Network::Off, None, Path::new("/td/a\nwrite=/x"), &home).is_err());
    assert_eq!(
        spec_path(98304, 7).unwrap(),
        Path::new("/run/td-authd/workspaces/98304/7")
    );
}

/// Root runs the credential helper for the sandbox's account, which runs
/// workspace-exec, and that runs td-jail's workspace kind with root's spec
/// and td-sandbox's relay: every word fixed or checked, the command last.
#[test]
fn the_launch_runs_the_jail_as_the_sandboxs_account() {
    let spec = spec_path(98304, 3).unwrap();
    for (mode, term, command) in [
        (
            Mode::Pty,
            "xterm",
            vec!["/td/store/x-td-sh/bin/td-sh".to_string(), "-i".into()],
        ),
        (
            Mode::Pipes,
            "dumb",
            vec!["/bin/sh".to_string(), "-c".into(), "id -u; exit 3".into()],
        ),
    ] {
        let helper = helper(98304, 4242, &spec, mode, term, &command);
        assert_eq!(helper.get_program(), "/bin/td-login");
        let mut expected: Vec<String> = [
            "exec-service-as",
            "tdw98304",
            "--",
            "/bin/td-authd",
            "workspace-exec",
            "98304",
            "4242",
            "/run/td-authd/workspaces/98304/3",
            mode.as_str(),
            term,
            "--",
        ]
        .map(String::from)
        .to_vec();
        expected.extend(command.iter().cloned());
        assert_eq!(words(&helper), expected);
        let arguments: Vec<String> = words(&helper).split_off(5);
        let exec = Exec::parse(&arguments).unwrap();
        let jail = exec.jail();
        assert_eq!(jail.get_program(), "/bin/td-jail");
        let mut expected: Vec<String> = [
            "--workspace",
            "4242",
            "/run/td-authd/workspaces/98304/3",
            "--inside",
            mode.as_str(),
            term,
        ]
        .map(String::from)
        .to_vec();
        expected.extend(command.iter().cloned());
        assert_eq!(words(&jail), expected);
        assert_eq!(jail.get_envs().count(), 0);
        assert_eq!(jail.get_current_dir(), Some(Path::new("/")));
    }
}

/// workspace-exec takes only a sandbox's or workspace's uid, a launcher,
/// that uid's spec below root's tree, a relay mode, a terminal type and a
/// command after `--`.
#[test]
fn workspace_exec_refuses_anything_but_its_own_launch() {
    let good = [
        "98304",
        "4242",
        "/run/td-authd/workspaces/98304/3",
        "pty",
        "xterm",
        "--",
        "/bin/sh",
        "-i",
    ];
    let parse = |words: &[&str]| {
        Exec::parse(
            &words
                .iter()
                .map(|word| word.to_string())
                .collect::<Vec<_>>(),
        )
    };
    assert!(parse(&good).is_ok());
    assert!(parse(&good[..7]).is_ok());
    for (index, value) in [
        (0, "1000"),
        (0, "65536"),
        (0, "98303"),
        (0, "131072"),
        (0, "098304"),
        (1, "0"),
        (1, "04242"),
        (1, "-1"),
        (2, "/run/td-authd/workspaces/98305/3"),
        (2, "/run/td-authd/workspaces/98304/../98305/3"),
        (2, "/run/td-authd/workspaces/98304/03"),
        (2, "/run/td-authd/workspaces/98304/"),
        (2, "/home/tester/spec"),
        (3, "tty"),
        (3, ""),
        (4, ""),
        (4, "xterm;sh"),
        (5, "-"),
        (6, ""),
    ] {
        let mut wrong = good;
        wrong[index] = value;
        assert!(parse(&wrong).is_err(), "{index} {value:?}");
    }
    assert!(parse(&good[..6]).is_err());
    let mut long: Vec<&str> = good[..6].to_vec();
    long.extend(std::iter::repeat_n("a", MAX_ARGUMENTS + 1));
    assert!(parse(&long).is_err());
}

/// At most `MAX_LAUNCHES` run at once, and a launch that ends gives its
/// place back.
#[test]
fn launches_are_bounded_and_give_their_place_back() {
    let active = Arc::new(AtomicUsize::new(0));
    let held: Vec<Slot> = (0..MAX_REQUESTS)
        .map(|_| Slot::take(&active).unwrap())
        .collect();
    assert!(Slot::take(&active).is_none());
    drop(held);
    assert_eq!(active.load(Ordering::Acquire), 0);
    assert!(Slot::take(&active).is_some());
    // Launches fill at most `MAX_LAUNCHES` of the connections, so other
    // verbs keep room while every shell is open.
    const { assert!(MAX_REQUESTS > MAX_LAUNCHES) };
    let claims = Active::default();
    let shells: Vec<Claim<'_>> = (0..MAX_LAUNCHES)
        .map(|index| {
            claims
                .claim(&format!("s{index}"), &shell(None, "xterm"))
                .unwrap()
        })
        .collect();
    let refused = claims
        .claim("other", &shell(None, "xterm"))
        .err()
        .unwrap_or_default();
    assert!(refused.contains("too many"), "{refused}");
    drop(claims.claim("other", &Verb::Status).unwrap());
    drop(claims.claim("other", &Verb::Create).unwrap());
    drop(shells);
    drop(claims.claim("other", &shell(None, "xterm")).unwrap());
}

/// A failed accept never ends the service, whose shells would end with
/// it: a signal or a peer that gave up is retried at once, anything else
/// after a pause that grows with each failure in a row and is bounded.
#[test]
fn a_failed_accept_pauses_and_never_ends_the_service() {
    use io::ErrorKind;
    assert_eq!(accept_pause(ErrorKind::Interrupted, 1), None);
    assert_eq!(accept_pause(ErrorKind::ConnectionAborted, 3), None);
    for kind in [
        ErrorKind::Other,
        ErrorKind::OutOfMemory,
        ErrorKind::QuotaExceeded,
    ] {
        assert_eq!(accept_pause(kind, 1), Some(ACCEPT_PAUSE));
        assert_eq!(accept_pause(kind, 2), Some(ACCEPT_PAUSE * 2));
        assert_eq!(accept_pause(kind, 6), Some(ACCEPT_PAUSE_MAX));
        assert_eq!(accept_pause(kind, u32::MAX), Some(ACCEPT_PAUSE_MAX));
    }
    // The loop leaves only through its startup.
    let serve = include_str!("../src/workspace.rs")
        .split("pub(crate) fn serve()")
        .nth(1)
        .unwrap()
        .split("\n}\n")
        .next()
        .unwrap();
    let looped = serve.split("    loop {").nth(1).unwrap();
    assert_eq!(looped.matches("return").count(), 0);
    assert_eq!(looped.matches("?;").count(), 0);
}

/// A runtime the request names must be installed before the answer, so
/// the requester hears it; native needs none.
#[test]
fn a_named_runtime_must_be_installed_before_the_answer() {
    let temp = crate::test_temp::TempDir::new("td-workspace-runtime").unwrap();
    let store = temp.path();
    require_runtime(store, None).unwrap();
    let refused = require_runtime(store, Some("freedesktop-sdk-25-08")).unwrap_err();
    assert!(refused.contains("is not installed"), "{refused}");
    fs::create_dir(store.join("freedesktop-sdk-25-08")).unwrap();
    std::os::unix::fs::symlink("generations/1", store.join("freedesktop-sdk-25-08/current"))
        .unwrap();
    require_runtime(store, Some("freedesktop-sdk-25-08")).unwrap();
    temp.cleanup().unwrap();
}

/// The spec tree is made below its base, its owner's with no other
/// writer, emptied when the service starts, and each spec is a new file
/// of the owner's and the account's group's, mode 0640, in the account's
/// 0710 directory; a spec of the same launch is never written twice.
#[test]
fn the_spec_tree_is_the_owners_and_each_spec_is_new() {
    let temp = crate::test_temp::TempDir::new("td-workspace-specs").unwrap();
    let base = fs::canonicalize(temp.path()).unwrap();
    let me = fs::metadata(&base).unwrap();
    // The test's own uid and group stand for root, and its group for the
    // account's, which a non-root owner may give a file.
    let (owner, group) = (me.uid(), me.gid());
    let root = base.join("td-authd/workspaces");
    let tree = SpecTree {
        base: &base,
        root: &root,
        owner: (owner, group),
    };
    tree.prepare().unwrap();
    for path in [base.join("td-authd"), root.clone()] {
        let metadata = fs::metadata(&path).unwrap();
        assert_eq!(metadata.uid(), owner);
        assert_eq!(metadata.mode() & 0o7777, 0o755, "{}", path.display());
    }
    let spec = tree.write(group, 3, "format=1\n").unwrap();
    assert_eq!(spec, root.join(group.to_string()).join("3"));
    let directory = fs::metadata(root.join(group.to_string())).unwrap();
    assert_eq!((directory.uid(), directory.gid()), (owner, group));
    assert_eq!(directory.mode() & 0o7777, 0o710);
    let written = fs::metadata(&spec).unwrap();
    assert_eq!((written.uid(), written.gid()), (owner, group));
    assert_eq!(written.mode() & 0o7777, 0o640);
    assert_eq!(written.nlink(), 1);
    assert_eq!(fs::read_to_string(&spec).unwrap(), "format=1\n");
    assert!(tree.write(group, 3, "format=2\n").is_err());
    assert_eq!(fs::read_to_string(&spec).unwrap(), "format=1\n");
    // A link in its place is never followed.
    let elsewhere = base.join("elsewhere");
    std::os::unix::fs::symlink(&elsewhere, root.join(group.to_string()).join("4")).unwrap();
    assert!(tree.write(group, 4, "format=1\n").is_err());
    assert!(!elsewhere.exists());
    // A restart empties the tree.
    tree.prepare().unwrap();
    assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
    // Another writer on the way refuses, as do another owner and a tree
    // outside its base.
    fs::set_permissions(base.join("td-authd"), fs::Permissions::from_mode(0o775)).unwrap();
    assert!(tree.prepare().is_err());
    fs::set_permissions(base.join("td-authd"), fs::Permissions::from_mode(0o755)).unwrap();
    assert!(ensure_parents(&base.join("x"), &base.join("td-authd"), (owner, group)).is_err());
    assert!(ensure_parents(
        &base,
        &base.join("td-authd"),
        (owner.wrapping_add(1), group)
    )
    .is_err());
    assert!(owned_directory(&root, (owner, group.wrapping_add(1))).is_err());
    // The service's own tree is root's, below /run.
    let live = specs();
    assert_eq!(
        (live.base, live.root, live.owner),
        (Path::new("/run"), Path::new(WORKSPACE_SPECS), (0, 0))
    );
    temp.cleanup().unwrap();
}

/// Root's work as a test sees it: each verb's answer chosen, and what was
/// asked recorded.
#[derive(Default)]
struct Fake {
    spec: PathBuf,
    prepare: Option<String>,
    run: Option<String>,
    create: Option<String>,
    delete: Option<String>,
    account: Option<String>,
    state: Option<State>,
    written: RefCell<Option<Result<Written, Failed>>>,
    asked: RefCell<Vec<String>>,
}

impl Fake {
    fn asked(&self, what: impl Into<String>) {
        self.asked.borrow_mut().push(what.into());
    }
}

fn failing<T>(why: &Option<String>, ok: T) -> Result<T, String> {
    match why {
        Some(why) => Err(why.clone()),
        None => Ok(ok),
    }
}

impl Serve for Fake {
    fn prepare(&self, name: &str, launch: &Launch, id: u64) -> Result<Prepared, String> {
        self.asked(format!("prepare {name} {id} {:?}", launch.mode));
        failing(
            &self.prepare,
            Prepared {
                uid: 98304,
                spec: self.spec.clone(),
                command: launch
                    .command
                    .clone()
                    .unwrap_or_else(|| vec!["/bin/sh".into(), "-i".into()]),
            },
        )
    }

    fn run(
        &self,
        mut stream: UnixStream,
        prepared: &Prepared,
        launch: &Launch,
    ) -> Result<(ExitStatus, String), String> {
        assert!(prepared.spec.exists());
        self.asked(format!(
            "run {} {}",
            launch.term,
            prepared.command.join(" ")
        ));
        // The channel is the connection, past the request.
        let mut first = [0u8; 1];
        stream.read_exact(&mut first).unwrap();
        assert_eq!(first, [1]);
        failing(&self.run, (ExitStatus::from_raw(0), String::new()))
    }

    fn state(&self, _name: &str) -> Result<Option<State>, String> {
        Ok(self.state)
    }

    fn create(&self, name: &str) -> Result<u32, String> {
        self.asked(format!("create {name}"));
        failing(&self.create, 98304)
    }

    fn delete(&self, name: &str) -> Result<(), String> {
        self.asked(format!("delete {name}"));
        failing(&self.delete, ())
    }

    fn account(&self, name: &str) -> Result<u32, String> {
        self.asked(format!("account {name}"));
        failing(&self.account, 98304)
    }

    fn grants(&self, request: &grant_requests::Request) -> Result<Written, Failed> {
        assert_eq!(request.kind, Kind::Sandbox);
        self.asked(format!("grants {} {:?}", request.name, request.verb));
        self.written
            .borrow_mut()
            .take()
            .unwrap_or(Err(Failed::Root("unexpected".into())))
    }
}

/// `request`'s bytes through `handle` against `serve`, and what the
/// requester read back.
fn exchange(request: &[u8], active: &Active, serve: &Fake) -> Vec<u8> {
    let (root, mut peer) = UnixStream::pair().unwrap();
    peer.write_all(request).unwrap();
    handle(root, 9, active, serve);
    let mut answer = Vec::new();
    peer.read_to_end(&mut answer).unwrap();
    answer
}

/// A refusal before the answer reaches the requester as one line and the
/// connection closes, with nothing run; an admitted launch answers `ok`,
/// hands the connection on past the request, and removes its spec when
/// the launch ends, failed or not.
#[test]
fn a_launch_answers_once_and_removes_its_spec() {
    let temp = crate::test_temp::TempDir::new("td-workspace-handle").unwrap();
    let active = Active::default();
    let fake = Fake {
        prepare: Some("sandbox default has no account".into()),
        ..Fake::default()
    };
    assert_eq!(
        exchange(b"start default native xterm\n", &active, &fake),
        b"refused sandbox default has no account\n"
    );
    assert_eq!(*fake.asked.borrow(), ["prepare default 9 Pty"]);
    let fake = Fake::default();
    let answer = exchange(b"start tools\n", &active, &fake);
    assert!(answer.starts_with(b"refused "));
    assert!(fake.asked.borrow().is_empty());

    for succeeds in [true, false] {
        let spec = temp.path().join(format!("spec-{succeeds}"));
        fs::write(&spec, "format=1\n").unwrap();
        let fake = Fake {
            spec: spec.clone(),
            run: (!succeeds).then(|| "start the workspace helper: no such file".into()),
            ..Fake::default()
        };
        let request = format!(
            "run default native pipes dumb {} {}\n\x01",
            word("id"),
            word("-u")
        );
        assert_eq!(exchange(request.as_bytes(), &active, &fake), STARTED);
        assert_eq!(
            *fake.asked.borrow(),
            ["prepare default 9 Pipes", "run dumb id -u"]
        );
        assert!(!spec.exists());
    }
    // Every launch gave its sandbox back.
    assert!(active.busy().launches.is_empty());
    temp.cleanup().unwrap();
}

/// A verb other than a launch is answered once: a creation with the
/// account's uid, a deletion `ok`, a listing with the account, its
/// network and its rows, a change with what it did; a refusal is the
/// writer's reason, and root's own failure names only the log.
#[test]
fn every_other_verb_is_answered_once() {
    let active = Active::default();
    let fake = Fake::default();
    assert_eq!(exchange(b"create default\n", &active, &fake), b"ok 98304\n");
    assert_eq!(exchange(b"delete default\n", &active, &fake), b"ok\n");
    assert_eq!(*fake.asked.borrow(), ["create default", "delete default"]);
    let fake = Fake {
        create: Some("td-firstboot allocate-workload failed".into()),
        ..Fake::default()
    };
    assert_eq!(
        exchange(b"create default\n", &active, &fake),
        b"refused td-firstboot allocate-workload failed\n"
    );

    let listed = |written: Result<Written, Failed>| Fake {
        written: RefCell::new(Some(written)),
        ..Fake::default()
    };
    let fake = listed(Ok(Written::Listed {
        network: Some(Network::Host),
        directories: vec![
            (Access::ReadOnly, "My Pictures".into()),
            (Access::ReadWrite, "src/td".into()),
        ],
        rows: "ro\tMy Pictures\nrw\tsrc/td\n".into(),
    }));
    assert_eq!(
        exchange(b"grants default\n", &active, &fake),
        b"ok 98304 host 2\nro\tMy Pictures\nrw\tsrc/td\n"
    );
    assert_eq!(
        *fake.asked.borrow(),
        ["account default", "grants default List { start: 0 }"]
    );
    // A listing names the account, so a sandbox without one is refused
    // before the writer runs.
    let fake = Fake {
        account: Some("sandbox tools has no account".into()),
        ..Fake::default()
    };
    assert_eq!(
        exchange(b"grants tools\n", &active, &fake),
        b"refused sandbox tools has no account\n"
    );
    assert_eq!(*fake.asked.borrow(), ["account tools"]);
    for (line, outcome, answer) in [
        (&b"grant default rw src\n"[..], 0, &b"ok added\n"[..]),
        (b"grant default ro src\n", 1, b"ok changed\n"),
        (b"grant default ro src\n", 2, b"ok unchanged\n"),
        (b"revoke default src\n", 3, b"ok revoked\n"),
        (b"network default host\n", 1, b"ok changed\n"),
        (b"network default off\n", 2, b"ok unchanged\n"),
    ] {
        let fake = listed(Ok(Written::Changed(outcome)));
        assert_eq!(exchange(line, &active, &fake), answer, "{line:?}");
    }
    // The writer's refusal of the request itself is final; a busy table,
    // or a writer that failed, may pass.
    let fake = listed(Err(Failed::Refused("~/src does not exist".into())));
    assert_eq!(
        exchange(b"grant default rw src\n", &active, &fake),
        b"refused final: ~/src does not exist\n"
    );
    let fake = listed(Err(Failed::Refused(workload_grants::BUSY.into())));
    assert_eq!(
        exchange(b"grant default rw src\n", &active, &fake),
        format!("refused {}\n", workload_grants::BUSY).into_bytes()
    );
    let fake = listed(Err(Failed::Root("did not answer in time".into())));
    assert_eq!(
        exchange(b"grant default rw src\n", &active, &fake),
        format!("refused the grant table's writer failed; see {LOG}\n").into_bytes()
    );
    // A writer that answers a change with a listing is no answer.
    let fake = listed(Ok(Written::Cleared(1)));
    assert!(exchange(b"network default off\n", &active, &fake).starts_with(b"refused "));
}

/// A sandbox is deleted only while none of its launches runs, and no
/// launch or other verb starts on it while it is being deleted; each hold
/// is given back.
#[test]
fn a_deletion_and_a_launch_of_one_sandbox_exclude_each_other() {
    let active = Active::default();
    let launch = shell(None, "xterm");
    let held = active.claim("default", &launch).unwrap();
    let refused = active
        .claim("default", &Verb::Delete)
        .err()
        .unwrap_or_default();
    assert!(refused.contains("is running"), "{refused}");
    // Another sandbox is untouched, and a second launch of this one may
    // run beside the first.
    drop(active.claim("tools", &Verb::Delete).unwrap());
    let second = active.claim("default", &launch).unwrap();
    drop(held);
    assert!(active.claim("default", &Verb::Delete).is_err());
    drop(second);
    let deleting = active.claim("default", &Verb::Delete).unwrap();
    let launch_again = || shell(None, "xterm");
    for verb in [
        launch,
        Verb::Delete,
        Verb::Create,
        Verb::Status,
        Verb::Grants(grant_requests::Verb::Network(Network::Host)),
    ] {
        let refused = active.claim("default", &verb).err().unwrap_or_default();
        assert!(
            refused.contains("being created or deleted"),
            "{verb:?}: {refused}"
        );
    }
    // A creation runs alone as a deletion does.
    let creating = active.claim("tools", &Verb::Create).unwrap();
    assert!(active.claim("tools", &launch_again()).is_err());
    drop(creating);
    drop(deleting);
    // A grant read or change holds its sandbox against a creation or a
    // deletion, beside its launches and other changes.
    let change = Verb::Grants(grant_requests::Verb::Network(Network::Off));
    let changing = active.claim("default", &change).unwrap();
    let refused = active
        .claim("default", &Verb::Create)
        .err()
        .unwrap_or_default();
    assert!(refused.contains("being read or changed"), "{refused}");
    drop(active.claim("default", &Verb::Status).unwrap());
    drop(active.claim("default", &launch_again()).unwrap());
    drop(changing);
    drop(active.claim("default", &Verb::Delete).unwrap());
    let busy = active.busy();
    assert!(busy.launches.is_empty() && busy.changes.is_empty() && busy.exclusive.is_empty());
    assert_eq!(busy.running, 0);
    drop(busy);
    // Through `handle`: a delete while a shell runs is refused, and
    // nothing is deleted.
    let fake = Fake::default();
    let running = active.claim("default", &shell(None, "xterm")).unwrap();
    assert_eq!(
        exchange(b"delete default\n", &active, &fake),
        b"refused a shell or command of sandbox default is running; end it first\n"
    );
    assert!(fake.asked.borrow().is_empty());
    drop(running);
}

/// A deletion clears every grant row the name holds through the writer
/// first, then has the account, prepared again, empty its home; a refused
/// clear stops it before the home, a name without an account has nothing
/// more, and the ledger is never written.
#[test]
fn a_deletion_clears_the_rows_before_the_account_empties_its_home() {
    let steps = RefCell::new(Vec::new());
    let clear = |request: &grant_requests::Request| {
        assert_eq!(request.verb, grant_requests::Verb::Clear);
        assert_eq!(
            (request.kind, request.name.as_str()),
            (Kind::Sandbox, "oracle")
        );
        steps.borrow_mut().push("clear".to_string());
        Ok(Written::Cleared(2))
    };
    let account = || {
        steps.borrow_mut().push("account".to_string());
        Ok(Some(98304))
    };
    let prepare = || {
        steps.borrow_mut().push("prepare".to_string());
        Ok(98304)
    };
    let empty = |uid: u32| {
        steps.borrow_mut().push(format!("empty {uid}"));
        Ok(())
    };
    let mark = |uid: u32| {
        steps.borrow_mut().push(format!("mark {uid}"));
        Ok(())
    };
    delete("oracle", clear, account, prepare, empty, mark).unwrap();
    assert_eq!(
        *steps.borrow(),
        ["clear", "account", "prepare", "empty 98304", "mark 98304"]
    );

    let emptied = Cell::new(false);
    let refused = delete(
        "oracle",
        |_| Err(Failed::Refused(workload_grants::BUSY.into())),
        || panic!("the ledger read after a refused clear"),
        || panic!("prepared after a refused clear"),
        |_| {
            emptied.set(true);
            Ok(())
        },
        |_| panic!("marked after a refused clear"),
    )
    .unwrap_err();
    assert_eq!(refused, workload_grants::BUSY);
    assert!(!emptied.get());
    let failed = delete(
        "oracle",
        |_| Err(Failed::Root("ended by a signal".into())),
        || Ok(Some(98304)),
        || Ok(98304),
        |_| Ok(()),
        |_| Ok(()),
    )
    .unwrap_err();
    assert!(failed.contains(LOG), "{failed}");
    // A clear answered with anything else is no clear.
    assert!(delete(
        "oracle",
        |_| Ok(Written::Changed(3)),
        || Ok(Some(98304)),
        || Ok(98304),
        |_| Ok(()),
        |_| Ok(()),
    )
    .is_err());
    // Rows are keyed by name: one without an account is cleared and done.
    delete(
        "gone",
        |_| Ok(Written::Cleared(0)),
        || Ok(None),
        || panic!("prepared a name without an account"),
        |_| panic!("emptied a name without an account"),
        |_| panic!("marked a name without an account"),
    )
    .unwrap();
    // td-firstboot's account must be the ledger's.
    assert!(delete(
        "oracle",
        |_| Ok(Written::Cleared(0)),
        || Ok(Some(98304)),
        || Ok(98305),
        |_| panic!("emptied another account's home"),
        |_| panic!("marked another account's home"),
    )
    .is_err());
    // A home not emptied is not marked deleted.
    let failed = delete(
        "oracle",
        |_| Ok(Written::Cleared(0)),
        || Ok(Some(98304)),
        || Ok(98304),
        |_| Err("the home nests too deep".into()),
        |_| panic!("marked a home not emptied"),
    )
    .unwrap_err();
    assert!(failed.contains("too deep"), "{failed}");
}

/// A creation never goes over a sandbox that holds files or grants: that
/// is `exists`, and nothing is allocated or unmarked. A name the ledger
/// lacks, or a sandbox holding nothing, a deleted one among them, is
/// allocated or returned, and its mark taken.
#[test]
fn a_creation_never_goes_over_a_held_sandbox() {
    let held = |deleted| State {
        uid: 98304,
        deleted,
        held: true,
    };
    for state in [held(false), held(true)] {
        let refused = create(
            Some(state),
            || panic!("allocated over a held sandbox"),
            |_| panic!("unmarked a held sandbox"),
        )
        .unwrap_err();
        assert_eq!(refused, EXISTS);
    }
    let steps = RefCell::new(Vec::new());
    for state in [
        None,
        Some(State {
            uid: 98304,
            deleted: true,
            held: false,
        }),
        Some(State {
            uid: 98304,
            deleted: false,
            held: false,
        }),
    ] {
        steps.borrow_mut().clear();
        let uid = create(
            state,
            || {
                steps.borrow_mut().push("allocate".to_string());
                Ok(98304)
            },
            |uid| {
                steps.borrow_mut().push(format!("unmark {uid}"));
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(uid, 98304);
        assert_eq!(*steps.borrow(), ["allocate", "unmark 98304"]);
    }
    let free = Some(State {
        uid: 98304,
        deleted: true,
        held: false,
    });
    assert!(create(free, || Ok(98305), |_| panic!("unmarked another")).is_err());
    // Through `handle`, the refusal is the one word td-sandbox reads.
    let fake = Fake {
        create: Some(EXISTS.into()),
        ..Fake::default()
    };
    assert_eq!(
        exchange(b"create default\n", &Active::default(), &fake),
        b"refused exists\n"
    );
}

/// Root reads a home's top-level listing and one fixed name in it, by a
/// descriptor that follows no link: the mark, a regular file of root's,
/// is no file the sandbox holds; anything else, a link or a file named
/// as the mark that is not root's among them, is. A deletion's mark is
/// a new file, never one followed through a link, and creating again
/// takes it.
#[test]
fn root_reads_a_home_by_its_listing_and_its_mark() {
    let temp = crate::test_temp::TempDir::new("td-workspace-mark").unwrap();
    let base = fs::canonicalize(temp.path()).unwrap();
    let me = fs::metadata(&base).unwrap().uid();
    let home = base.join("home");
    fs::create_dir(&home).unwrap();
    // The test's own uid stands for root as the mark's owner.
    let other = me.wrapping_add(1);
    assert!(!marked(&home, me, me).unwrap());
    assert!(!holds_files(&home, me, me).unwrap());
    mark_deleted(&home, me, me).unwrap();
    mark_deleted(&home, me, me).unwrap();
    assert!(marked(&home, me, me).unwrap());
    assert!(!holds_files(&home, me, me).unwrap());
    // A mark that is not root's is a file the sandbox holds.
    assert!(!marked(&home, me, other).unwrap());
    assert!(holds_files(&home, me, other).unwrap());
    unmark(&home, me, me).unwrap();
    assert!(!home.join(DELETED_MARK).exists());
    unmark(&home, me, me).unwrap();
    // A link in the mark's place is never followed, nor taken for it.
    let elsewhere = base.join("elsewhere");
    std::os::unix::fs::symlink(&elsewhere, home.join(DELETED_MARK)).unwrap();
    assert!(mark_deleted(&home, me, me).is_err());
    assert!(!elsewhere.exists());
    assert!(!marked(&home, me, me).unwrap());
    assert!(holds_files(&home, me, me).unwrap());
    unmark(&home, me, me).unwrap();
    assert!(fs::symlink_metadata(home.join(DELETED_MARK)).is_ok());
    fs::remove_file(home.join(DELETED_MARK)).unwrap();
    fs::write(home.join("file"), "x").unwrap();
    assert!(holds_files(&home, me, me).unwrap());
    // Another account's home, or a link in the home's place, is refused.
    assert!(holds_files(&home, other, me).is_err());
    assert!(mark_deleted(&home, other, me).is_err());
    let linked = base.join("linked");
    std::os::unix::fs::symlink(&home, &linked).unwrap();
    assert!(holds_files(&linked, me, me).is_err());
    assert!(marked(&linked, me, me).is_err());
    temp.cleanup().unwrap();
}

/// `status` answers whether the ledger holds the sandbox and whether it
/// holds files or grants; a deleted sandbox takes no launch and no grant
/// change, refused as `deleted`, and is still listed.
#[test]
fn status_names_a_sandbox_held_free_or_absent_and_a_deleted_one_refuses() {
    let active = Active::default();
    let at = |state| Fake {
        state,
        ..Fake::default()
    };
    assert_eq!(exchange(b"status x\n", &active, &at(None)), b"ok absent\n");
    let state = |deleted, held| {
        Some(State {
            uid: 98304,
            deleted,
            held,
        })
    };
    assert_eq!(
        exchange(b"status x\n", &active, &at(state(false, true))),
        b"ok held 98304\n"
    );
    assert_eq!(
        exchange(b"status x\n", &active, &at(state(true, false))),
        b"ok free 98304\n"
    );
    let deleted = at(state(true, false));
    for line in [
        &b"start x native xterm\n"[..],
        b"grant x rw src\n",
        b"revoke x src\n",
        b"network x host\n",
    ] {
        assert_eq!(
            exchange(line, &active, &deleted),
            b"refused deleted\n",
            "{line:?}"
        );
    }
    assert!(deleted.asked.borrow().is_empty());
    // A grant request `2c` made while a deletion ran leaves the deleted
    // sandbox held: a creation is refused as existing, and a deletion,
    // which a mark does not refuse, clears it.
    let raced = at(state(true, true));
    assert_eq!(exchange(b"delete x\n", &active, &raced), b"ok\n");
    assert_eq!(*raced.asked.borrow(), ["delete x"]);
    assert_eq!(
        create(state(true, true), || Ok(98304), |_| Ok(())),
        Err(EXISTS.into())
    );
}

/// The account empties its home as itself, through the credential
/// helper: root runs no walk of its own.
#[test]
fn the_account_empties_its_own_home() {
    let command = emptier(98304);
    assert_eq!(command.get_program(), "/bin/td-login");
    assert_eq!(
        words(&command),
        [
            "exec-service-as",
            "tdw98304",
            "--",
            "/bin/td-authd",
            "workspace-empty",
            "98304"
        ]
    );
    for wrong in [&[][..], &["1000"], &["65536"], &["98304", "x"], &["098304"]] {
        let arguments: Vec<String> = wrong.iter().map(|word| word.to_string()).collect();
        assert!(empty(&arguments).is_err(), "{wrong:?}");
    }
}

/// Emptying removes everything below the home and keeps the home: nested
/// directories, ones whose mode the account narrowed, files and links,
/// a link never followed. A home not the account's is refused, and one
/// nesting past the bound refuses rather than recursing without end.
#[test]
fn emptying_a_home_removes_everything_below_it_and_follows_no_link() {
    let temp = crate::test_temp::TempDir::new("td-workspace-empty").unwrap();
    let base = fs::canonicalize(temp.path()).unwrap();
    let me = fs::metadata(&base).unwrap().uid();
    let home = base.join("home");
    let outside = base.join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("kept"), "kept").unwrap();
    fs::create_dir_all(home.join("a/b/c")).unwrap();
    fs::write(home.join("a/b/c/file"), "x").unwrap();
    fs::write(home.join("top"), "x").unwrap();
    std::os::unix::fs::symlink(&outside, home.join("link")).unwrap();
    std::os::unix::fs::symlink(&outside, home.join("a/link")).unwrap();
    fs::set_permissions(home.join("a/b"), fs::Permissions::from_mode(0o500)).unwrap();
    fs::set_permissions(home.join("a"), fs::Permissions::from_mode(0o000)).unwrap();
    empty_home(&home, me).unwrap();
    assert!(home.is_dir());
    assert_eq!(fs::read_dir(&home).unwrap().count(), 0);
    assert_eq!(fs::read_to_string(outside.join("kept")).unwrap(), "kept");
    // Another account's home is refused untouched.
    fs::write(home.join("mine"), "x").unwrap();
    assert!(empty_home(&home, me.wrapping_add(1)).is_err());
    assert!(home.join("mine").exists());
    // A link in the home's place is never followed.
    let linked = base.join("linked");
    std::os::unix::fs::symlink(&outside, &linked).unwrap();
    assert!(empty_home(&linked, me).is_err());
    assert!(outside.join("kept").exists());
    // Past the bound, the emptying refuses.
    let mut deep = home.clone();
    for _ in 0..EMPTY_DEPTH {
        deep.push("d");
    }
    fs::create_dir_all(&deep).unwrap();
    let refused = empty_home(&home, me).unwrap_err();
    assert!(refused.contains("deeper"), "{refused}");
    temp.cleanup().unwrap();
}

/// A workspace's account empties each directory of its scratch tree as it
/// empties its home, keeping the four directories; a link in one is never
/// followed. An account without a tree, a sandbox's, has nothing more to
/// empty; a tree of another account's directories, or one missing a
/// directory, refuses.
#[test]
fn emptying_a_scratch_tree_empties_each_directory_and_keeps_it() {
    let temp = crate::test_temp::TempDir::new("td-workspace-scratch").unwrap();
    let base = fs::canonicalize(temp.path()).unwrap();
    let me = fs::metadata(&base).unwrap().uid();
    let outside = base.join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("kept"), "kept").unwrap();
    // No tree: nothing to empty.
    empty_scratch(&base, me).unwrap();
    assert!(!base.join(accounts::SCRATCH).exists());
    let tree = base.join(accounts::SCRATCH).join(me.to_string());
    fs::create_dir_all(&tree).unwrap();
    for name in accounts::SCRATCH_NAMES {
        let directory = tree.join(name);
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o1777)).unwrap();
        fs::create_dir_all(directory.join("job/sub")).unwrap();
        fs::write(directory.join("job/sub/socket"), "x").unwrap();
        fs::write(directory.join("file"), "x").unwrap();
        std::os::unix::fs::symlink(&outside, directory.join("link")).unwrap();
        fs::set_permissions(directory.join("job"), fs::Permissions::from_mode(0o000)).unwrap();
    }
    empty_scratch(&base, me).unwrap();
    for name in accounts::SCRATCH_NAMES {
        let directory = tree.join(name);
        assert!(directory.is_dir(), "{name}");
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 0, "{name}");
        // Left at root's mode, not the walk's.
        let mode = fs::symlink_metadata(&directory).unwrap().mode() & 0o7777;
        assert_eq!(mode, 0o1777, "{name}");
    }
    assert_eq!(fs::read_to_string(outside.join("kept")).unwrap(), "kept");
    // Another account's directories are refused untouched.
    fs::write(tree.join("tmp/mine"), "x").unwrap();
    let theirs = base
        .join(accounts::SCRATCH)
        .join(me.wrapping_add(1).to_string());
    fs::rename(&tree, &theirs).unwrap();
    assert!(empty_scratch(&base, me.wrapping_add(1)).is_err());
    assert!(theirs.join("tmp/mine").exists());
    // A tree missing a directory refuses.
    fs::rename(&theirs, &tree).unwrap();
    fs::remove_dir_all(tree.join("run")).unwrap();
    assert!(empty_scratch(&base, me).is_err());
    temp.cleanup().unwrap();
}

/// The intake has one path to a new ledger row, td-firstboot's
/// `allocate-workload` behind `ALLOCATE_SANDBOX`, which only `create`
/// takes; launches and deletions prepare an existing row and add none.
/// It runs no grant table writer of its own: grant_requests is the one.
#[test]
fn the_intake_allocates_only_at_create_and_writes_grants_only_through_the_writer() {
    let source = include_str!("../src/workspace.rs")
        .split("#[cfg(test)]")
        .next()
        .unwrap();
    assert_eq!(source.matches("ALLOCATE_SANDBOX").count(), 1);
    let create = source
        .split("fn create(&self, name: &str) -> Result<u32, String> {")
        .nth(1)
        .unwrap()
        .split("\n    }\n")
        .next()
        .unwrap();
    assert!(create.contains("application::admit(&application::ALLOCATE_SANDBOX,"));
    // A creation reads the sandbox's state and destroys nothing: it
    // neither deletes, clears nor empties, so it cannot go over a held
    // sandbox; the free `create` refuses one before allocating.
    assert!(create.contains("self.state(name)?,"));
    for absent in ["delete", "clear", "empty", "Clear"] {
        assert!(!create.contains(absent), "{absent}");
    }
    let free = source
        .split("\nfn create(\n")
        .nth(1)
        .unwrap()
        .split("\n}\n")
        .next()
        .unwrap();
    assert!(free.find("return Err(EXISTS.into());").unwrap() < free.find("allocate()?").unwrap());
    assert_eq!(source.matches("PREPARE_SANDBOX").count(), 2);
    for absent in [
        "allocate-workload",
        "allocate-application",
        "/bin/td-firstboot",
        "grant-table",
    ] {
        assert!(!source.contains(absent), "{absent}");
    }
    assert_eq!(source.matches("grant_requests::perform(").count(), 1);
    // Across the crate, only grant_requests runs the writer.
    for (file, text) in [
        ("workspace.rs", include_str!("../src/workspace.rs")),
        ("launch.rs", include_str!("../src/launch.rs")),
        ("application.rs", include_str!("../src/application.rs")),
        ("main.rs", include_str!("../src/main.rs")),
    ] {
        assert!(!text.contains("\"grant-table\""), "{file}");
    }
    assert!(include_str!("../src/grant_requests.rs").contains("\"grant-table\","));
}
