#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use super::*;
use std::os::unix::process::ExitStatusExt;

fn words(command: &Command) -> Vec<String> {
    command
        .get_args()
        .map(|arg| arg.to_str().unwrap().to_string())
        .collect()
}

/// A request's bytes, as td-sandbox will send them once it launches
/// through root.
fn encode(request: &Request) -> Vec<u8> {
    format!(
        "start {} {} {}\n",
        request.name,
        request.runtime.unwrap_or("native"),
        request.term
    )
    .into_bytes()
}

/// A request is one exact line: the verb, a ledger name, `native` or a
/// declared runtime, and a terminal type, and nothing else.
#[test]
fn a_request_is_one_exact_bounded_line() {
    let request = Request::parse(b"start default native xterm-256color\n").unwrap();
    assert_eq!(
        request,
        Request {
            name: "default".into(),
            runtime: None,
            term: "xterm-256color".into(),
        }
    );
    assert_eq!(
        encode(&request),
        b"start default native xterm-256color\n".to_vec()
    );
    let runtime = Request::parse(b"start tools freedesktop-sdk-25-08 linux\n").unwrap();
    assert_eq!(runtime.runtime, Some("freedesktop-sdk-25-08"));
    assert_eq!(Request::parse(&encode(&runtime)).unwrap(), runtime);
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
    peer.write_all(&[b'a'; MAX_REQUEST + 8]).unwrap();
    assert!(read_request(&mut root).unwrap_err().contains("too long"));

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
/// and td-sandbox's relay: every word fixed or checked.
#[test]
fn the_launch_runs_the_jail_as_the_sandboxs_account() {
    let spec = spec_path(98304, 3).unwrap();
    let helper = helper(
        98304,
        4242,
        &spec,
        "xterm",
        Path::new("/td/store/x-td-sh/bin/td-sh"),
    );
    assert_eq!(helper.get_program(), "/bin/td-login");
    assert_eq!(
        words(&helper),
        [
            "exec-service-as",
            "tdw98304",
            "--",
            "/bin/td-authd",
            "workspace-exec",
            "98304",
            "4242",
            "/run/td-authd/workspaces/98304/3",
            "xterm",
            "/td/store/x-td-sh/bin/td-sh",
        ]
    );
    let arguments: Vec<String> = words(&helper).split_off(5);
    let exec = Exec::parse(&arguments).unwrap();
    let jail = exec.jail();
    assert_eq!(jail.get_program(), "/bin/td-jail");
    assert_eq!(
        words(&jail),
        [
            "--workspace",
            "4242",
            "/run/td-authd/workspaces/98304/3",
            "--inside",
            "pty",
            "xterm",
            "/td/store/x-td-sh/bin/td-sh",
            "-i",
        ]
    );
    assert_eq!(jail.get_envs().count(), 0);
    assert_eq!(jail.get_current_dir(), Some(Path::new("/")));
}

/// workspace-exec takes only a sandbox's or workspace's uid, a launcher,
/// that uid's spec below root's tree, a terminal type and an absolute
/// shell.
#[test]
fn workspace_exec_refuses_anything_but_its_own_launch() {
    let good = [
        "98304",
        "4242",
        "/run/td-authd/workspaces/98304/3",
        "xterm",
        "/bin/sh",
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
        (3, ""),
        (3, "xterm;sh"),
        (4, "sh"),
        (4, ""),
    ] {
        let mut wrong = good;
        wrong[index] = value;
        assert!(parse(&wrong).is_err(), "{index} {value:?}");
    }
    assert!(parse(&good[..4]).is_err());
}

/// At most `MAX_LAUNCHES` run at once, and a launch that ends gives its
/// place back.
#[test]
fn launches_are_bounded_and_give_their_place_back() {
    let active = Arc::new(AtomicUsize::new(0));
    let held: Vec<Slot> = (0..MAX_LAUNCHES)
        .map(|_| Slot::take(&active).unwrap())
        .collect();
    assert!(Slot::take(&active).is_none());
    drop(held);
    assert_eq!(active.load(Ordering::Acquire), 0);
    assert!(Slot::take(&active).is_some());
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

/// A refusal before the answer reaches the requester as one line and the
/// connection closes, with nothing run; an admitted launch answers `ok`,
/// hands the connection on past the request, and removes its spec when
/// the launch ends, failed or not.
#[test]
fn a_launch_answers_once_and_removes_its_spec() {
    let temp = crate::test_temp::TempDir::new("td-workspace-handle").unwrap();
    let ran = std::cell::Cell::new(0);
    let refused = |_: UnixStream, _: &Prepared, _: &str| {
        ran.set(ran.get() + 1);
        Err::<(ExitStatus, String), String>("ran".into())
    };
    let (root, mut peer) = UnixStream::pair().unwrap();
    peer.write_all(b"start default native xterm\n").unwrap();
    handle(
        root,
        1,
        |_, _| Err("sandbox default has no account".into()),
        refused,
    );
    let mut answer = Vec::new();
    peer.read_to_end(&mut answer).unwrap();
    assert_eq!(answer, b"refused sandbox default has no account\n");

    let prepared_too = std::cell::Cell::new(false);
    let (root, mut peer) = UnixStream::pair().unwrap();
    peer.write_all(b"start tools\n").unwrap();
    handle(
        root,
        2,
        |_, _| {
            prepared_too.set(true);
            Err("prepared".into())
        },
        refused,
    );
    let mut answer = Vec::new();
    peer.read_to_end(&mut answer).unwrap();
    assert!(answer.starts_with(b"refused "));
    assert!(!prepared_too.get());
    assert_eq!(ran.get(), 0);

    for succeeds in [true, false] {
        let spec = temp.path().join(format!("spec-{succeeds}"));
        fs::write(&spec, "format=1\n").unwrap();
        let (root, mut peer) = UnixStream::pair().unwrap();
        peer.write_all(b"start default native xterm\n\x01").unwrap();
        let prepare = |request: &Request, id: u64| {
            assert_eq!((request.name.as_str(), id), ("default", 9));
            Ok(Prepared {
                uid: 98304,
                spec: spec.clone(),
                shell: PathBuf::from("/bin/sh"),
            })
        };
        let launch = |mut channel: UnixStream, prepared: &Prepared, term: &str| {
            assert!(prepared.spec.exists());
            assert_eq!(term, "xterm");
            // The channel is the connection, past the request.
            let mut first = [0u8; 1];
            channel.read_exact(&mut first).unwrap();
            assert_eq!(first, [1]);
            if succeeds {
                Ok((ExitStatus::from_raw(0), String::new()))
            } else {
                Err("start the workspace helper: no such file".into())
            }
        };
        handle(root, 9, prepare, launch);
        let mut answer = Vec::new();
        peer.read_to_end(&mut answer).unwrap();
        assert_eq!(answer, STARTED);
        assert!(!spec.exists());
    }
    temp.cleanup().unwrap();
}
