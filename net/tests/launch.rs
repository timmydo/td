#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! td-launch end to end over the built td-net. This test binary is the
//! launched program: run with `TD_LAUNCH_TEST_MARK`, the
//! `launched_program` test checks what a launched application would see
//! and writes the mark, so a launch that never ran it fails rather than
//! passes; `TD_LAUNCH_TEST_EXIT` is the code it then exits with, and
//! `TD_LAUNCH_TEST_HOLD` keeps it running until it is killed.

use std::os::unix::net::UnixStream;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

const TD_NET: &str = env!("CARGO_BIN_EXE_td-net");
const MARK: &str = "TD_LAUNCH_TEST_MARK";
const EXIT: &str = "TD_LAUNCH_TEST_EXIT";
const HOLD: &str = "TD_LAUNCH_TEST_HOLD";
/// Where the launched program writes the companion variables it was given.
const COMPANIONS: &str = "TD_LAUNCH_TEST_COMPANIONS";
/// Where the launched program records whether an egress relay answers in
/// its runtime directory.
const EGRESS: &str = "TD_LAUNCH_TEST_EGRESS";

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("td-launch-it-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Run as the launched program: its runtime directory is the launch's own,
/// named by its pid, which is the launch's; its fetch socket answers; and
/// its display is the session's made absolute.
#[test]
fn launched_program() {
    let Some(mark) = std::env::var_os(MARK) else {
        return;
    };
    let runtime = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").unwrap());
    assert_eq!(
        runtime.file_name().unwrap().to_str().unwrap(),
        std::process::id().to_string()
    );
    let session = runtime.parent().unwrap();
    assert_eq!(session.file_name().unwrap(), "td-launch");
    let socket = runtime.join("td-fetch/socket");
    UnixStream::connect(&socket).unwrap();
    assert_eq!(
        Path::new(&std::env::var_os("WAYLAND_DISPLAY").unwrap()),
        session.parent().unwrap().join("wayland-9")
    );
    std::fs::write(&mark, socket.as_os_str().as_encoded_bytes()).unwrap();
    if let Some(record) = std::env::var_os(EGRESS) {
        let egress = runtime.join("td-egress/socket");
        let served = UnixStream::connect(&egress).is_ok();
        std::fs::write(record, if served { "served" } else { "none" }).unwrap();
    }
    if let Some(record) = std::env::var_os(COMPANIONS) {
        let given = ["TD_AGENT_JAIL", "TD_AGENT_TXT"]
            .iter()
            .map(|var| match std::env::var_os(var) {
                Some(value) => format!("{var}={}\n", value.to_string_lossy()),
                None => format!("{var} unset\n"),
            })
            .collect::<String>();
        std::fs::write(record, given).unwrap();
    }
    if std::env::var_os(HOLD).is_some() {
        loop {
            std::thread::sleep(Duration::from_secs(1));
        }
    }
    if let Some(code) = std::env::var_os(EXIT) {
        std::process::exit(code.to_str().unwrap().parse().unwrap());
    }
}

/// `command` run as the launched program under `runtime`, its mark at
/// `mark`.
fn as_child<'a>(command: &'a mut Command, runtime: &Path, mark: &Path) -> &'a mut Command {
    command
        .args(["--exact", "launched_program", "--nocapture"])
        .env(MARK, mark)
        .env("XDG_RUNTIME_DIR", runtime)
        .env("WAYLAND_DISPLAY", "wayland-9")
        .env_remove("WAYLAND_SOCKET")
        .env_remove(EXIT)
        .env_remove(HOLD)
        .env_remove(COMPANIONS)
        .env_remove(EGRESS)
        .stdin(Stdio::null())
}

fn launch(runtime: &Path, mark: &Path) -> Command {
    let mut command = Command::new(TD_NET);
    command.arg("launch").arg(std::env::current_exe().unwrap());
    as_child(&mut command, runtime, mark);
    command
}

/// The launch's runtime directory and the service's socket are gone, the
/// service having ended with its program.
fn nothing_served(runtime: &Path, mark: &Path) {
    let socket = PathBuf::from(std::fs::read_to_string(mark).unwrap());
    until("the service to remove its socket", || !socket.exists());
    until("the service to remove its directory", || {
        !socket.parent().unwrap().exists()
    });
    assert!(socket.starts_with(runtime.join("td-launch")));
}

#[test]
fn a_launch_serves_its_program_and_ends_with_it() {
    let base = scratch("launch");
    let (runtime, mark) = (base.join("run"), base.join("mark"));
    std::fs::create_dir_all(&runtime).unwrap();
    let status = launch(&runtime, &mark).status().unwrap();
    assert!(status.success(), "{status}");
    nothing_served(&runtime, &mark);
    std::fs::remove_dir_all(&base).unwrap();
}

#[test]
fn the_launch_exits_as_its_program_does() {
    let base = scratch("exit");
    let (runtime, mark) = (base.join("run"), base.join("mark"));
    std::fs::create_dir_all(&runtime).unwrap();
    let status = launch(&runtime, &mark).env(EXIT, "3").status().unwrap();
    assert_eq!(status.code(), Some(3));
    nothing_served(&runtime, &mark);
    std::fs::remove_dir_all(&base).unwrap();
}

#[test]
fn a_killed_program_takes_its_service_along() {
    let base = scratch("kill");
    let (runtime, mark) = (base.join("run"), base.join("mark"));
    std::fs::create_dir_all(&runtime).unwrap();
    let mut child = launch(&runtime, &mark)
        .env(HOLD, "1")
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    until("the program to run", || mark.exists());
    let socket = PathBuf::from(std::fs::read_to_string(&mark).unwrap());
    UnixStream::connect(&socket).unwrap();
    child.kill().unwrap();
    let status: ExitStatus = child.wait().unwrap();
    assert_eq!(status.signal(), Some(9), "the launch is the program");
    nothing_served(&runtime, &mark);
    // The next launch sweeps the killed one's directory.
    let leftover = socket.parent().unwrap().parent().unwrap().to_path_buf();
    std::fs::remove_file(&mark).unwrap();
    assert!(launch(&runtime, &mark).status().unwrap().success());
    assert!(!leftover.exists(), "swept");
    std::fs::remove_dir_all(&base).unwrap();
}

#[test]
fn the_service_ends_when_its_parent_is_not_the_one_named() {
    let dir = scratch("parent");
    let socket = dir.join("td-fetch/socket");
    // Its parent is this test, never pid 1.
    let mut service = Command::new(TD_NET)
        .args(["fetchd", "run", "--exit-with-parent", "1", "--socket"])
        .arg(&socket)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    until("the service to exit", || {
        service.try_wait().unwrap().is_some()
    });
    assert!(!socket.exists(), "its socket is removed");
    assert!(!socket.parent().unwrap().exists(), "and its directory");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_refused_start_removes_nothing_it_did_not_make() {
    let dir = scratch("refused");
    let socket = dir.join("td-fetch/socket");
    std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
    std::fs::write(&socket, b"not a socket").unwrap();
    let status = Command::new(TD_NET)
        .args(["fetchd", "run", "--exit-with-parent", "1", "--socket"])
        .arg(&socket)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(1));
    assert_eq!(std::fs::read(&socket).unwrap(), b"not a socket");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_program_is_a_path_and_one_that_cannot_run_leaves_nothing() {
    let base = scratch("refuse");
    let (runtime, mark) = (base.join("run"), base.join("mark"));
    std::fs::create_dir_all(&runtime).unwrap();
    let status = Command::new(TD_NET)
        .args(["launch", "td-news"])
        .env("XDG_RUNTIME_DIR", &runtime)
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(2), "a name is not looked up on PATH");
    let mut missing = Command::new(TD_NET);
    missing.arg("launch").arg(base.join("absent"));
    let status = as_child(&mut missing, &runtime, &mark)
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(1));
    let left: Vec<_> = std::fs::read_dir(runtime.join("td-launch"))
        .unwrap()
        .collect();
    assert!(left.is_empty(), "the launch's directory is removed");
    std::fs::remove_dir_all(&base).unwrap();
}

/// `command`'s status. A copy just written can be busy while another
/// test's fork still holds its write descriptor (ETXTBSY) until that
/// child execs, so that is retried.
fn status_of_a_fresh_copy(command: &mut Command) -> std::process::ExitStatus {
    let mut tries = 0;
    loop {
        match command.status() {
            Err(e) if e.raw_os_error() == Some(26) && tries < 50 => {
                tries += 1;
                std::thread::sleep(Duration::from_millis(20));
            }
            status => break status.unwrap(),
        }
    }
}

#[test]
fn a_link_named_for_an_application_launches_it_from_beside_the_binary() {
    let base = scratch("link");
    let (lib, bin) = (base.join("lib"), base.join("bin"));
    std::fs::create_dir_all(&lib).unwrap();
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::copy(TD_NET, lib.join("td-net")).unwrap();
    std::fs::copy(std::env::current_exe().unwrap(), lib.join("td-news")).unwrap();
    std::os::unix::fs::symlink(lib.join("td-net"), bin.join("td-news")).unwrap();
    let (runtime, mark) = (base.join("run"), base.join("mark"));
    std::fs::create_dir_all(&runtime).unwrap();
    let mut command = Command::new(bin.join("td-news"));
    let egress = base.join("egress");
    as_child(&mut command, &runtime, &mark).env(EGRESS, &egress);
    let status = status_of_a_fresh_copy(&mut command);
    assert!(status.success(), "{status}");
    nothing_served(&runtime, &mark);
    // No relay for an application that is not td-agent.
    assert_eq!(std::fs::read_to_string(&egress).unwrap(), "none");
    std::fs::remove_dir_all(&base).unwrap();
}

#[test]
fn the_agent_is_given_its_companions_and_never_an_inherited_one() {
    let base = scratch("companions");
    let (lib, bin) = (base.join("lib"), base.join("bin"));
    std::fs::create_dir_all(&lib).unwrap();
    // The launch names what it finds beside its own resolved path.
    let lib = std::fs::canonicalize(&lib).unwrap();
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::copy(TD_NET, lib.join("td-net")).unwrap();
    std::fs::copy(std::env::current_exe().unwrap(), lib.join("td-agent")).unwrap();
    std::fs::write(lib.join("td-jail"), b"").unwrap();
    std::os::unix::fs::symlink(lib.join("td-net"), bin.join("td-agent")).unwrap();
    let (runtime, mark, record) = (base.join("run"), base.join("mark"), base.join("record"));
    std::fs::create_dir_all(&runtime).unwrap();
    let mut command = Command::new(bin.join("td-agent"));
    let egress = base.join("egress");
    as_child(&mut command, &runtime, &mark)
        .env(EGRESS, &egress)
        .env(COMPANIONS, &record)
        .env("TD_AGENT_JAIL", "/stale/td-jail")
        .env("TD_AGENT_TXT", "/stale/td-txt");
    let status = status_of_a_fresh_copy(&mut command);
    assert!(status.success(), "{status}");
    // The jail beside it replaces the inherited one; the absent td-txt
    // takes the inherited one away.
    assert_eq!(
        std::fs::read_to_string(&record).unwrap(),
        format!(
            "TD_AGENT_JAIL={}\nTD_AGENT_TXT unset\n",
            lib.join("td-jail").display()
        )
    );
    // td-agent's launch serves the egress relay beside the fetch service.
    assert_eq!(std::fs::read_to_string(&egress).unwrap(), "served");
    std::fs::remove_dir_all(&base).unwrap();
}

#[test]
fn the_companions_are_listed_for_the_installer() {
    let output = Command::new(TD_NET)
        .args(["launch", "--companions"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "td-jail\ntd-txt\n"
    );
}

#[test]
fn the_names_are_listed_for_the_installer() {
    let output = Command::new(TD_NET)
        .args(["launch", "--names"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "td-agent\ntd-mail\ntd-news\n"
    );
}
