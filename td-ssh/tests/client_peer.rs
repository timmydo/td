//! The real `td-ssh`, whole, against a server built on the crate's own key exchange
//! and packet layers that does what no honest server does: stops reading, holds its
//! window shut, floods. Its session loop is reached no other way (DESIGN.md section 9).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic
)]

use std::fs::{self, File};
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::Once;
use std::thread;
use std::time::{Duration, Instant};
use td_fs::TempDir;
use td_primitives::curve25519::ed25519;
use td_ssh::conn;
use td_ssh::kex::{self, Kex, KexEnvironment, Role, DEFAULT_KEX};
use td_ssh::msg;
use td_ssh::packet::{exchange_versions, PacketReader, PacketWriter, IDENTIFICATION};
use td_ssh::sys;
use td_ssh::wire::{Reader, Writer};

const SSH: &str = env!("CARGO_BIN_EXE_td-ssh");
const KEYGEN: &str = env!("CARGO_BIN_EXE_td-ssh-keygen");
const DEADLINE: Duration = Duration::from_secs(30);
const WATCHDOG: Duration = Duration::from_secs(600);
/// Well above what the budget and the kernel's buffers can take, and well below the
/// quarter of a GiB the queue held when it counted only messages.
const FLOOD_LIMIT: usize = 48 * 1024 * 1024;
/// Far more than a rekey was once allowed to hold back.
const STREAM_PAST_A_REKEY: usize = 32 * 1024 * 1024;

fn start_watchdog() {
    static STARTED: Once = Once::new();
    STARTED.call_once(|| {
        thread::spawn(|| {
            thread::sleep(WATCHDOG);
            eprintln!("tests/client_peer.rs: still running after {WATCHDOG:?}");
            std::process::exit(101);
        });
    });
}

struct Env(ed25519::SigningKey);

impl KexEnvironment for Env {
    fn random(&mut self, out: &mut [u8]) -> td_ssh::error::Result<()> {
        File::open("/dev/urandom")?.read_exact(out)?;
        Ok(())
    }

    fn host_key_blob(&mut self) -> td_ssh::error::Result<Vec<u8>> {
        Ok(kex::ed25519_public_blob(&self.0.public_key()))
    }

    fn sign(&mut self, hash: &[u8; 32]) -> td_ssh::error::Result<Vec<u8>> {
        Ok(kex::ed25519_signature_blob(&self.0.sign(hash).unwrap()))
    }

    fn check_host_key(&mut self, _blob: &[u8]) -> td_ssh::error::Result<()> {
        Ok(())
    }
}

/// One test's directory, listening socket, and the client's key.
struct Scene {
    dir: TempDir,
    listener: TcpListener,
}

impl Scene {
    fn new() -> Self {
        start_watchdog();
        let dir = TempDir::new("td-ssh-client-peer").unwrap();
        let output = Command::new(KEYGEN)
            .args(["-q", "-t", "ed25519", "-N", "", "-f"])
            .arg(dir.path().join("key"))
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        Self {
            dir,
            listener: TcpListener::bind("127.0.0.1:0").unwrap(),
        }
    }

    fn stderr_path(&self) -> PathBuf {
        self.dir.path().join("stderr")
    }

    fn stderr(&self) -> String {
        fs::read_to_string(self.stderr_path()).unwrap_or_default()
    }

    /// The client, up to its destination, which is `u@` the listener.
    fn ssh(&self, options: &[&str]) -> Command {
        self.ssh_as(options, "u@127.0.0.1")
    }

    fn ssh_as(&self, options: &[&str], destination: &str) -> Command {
        let mut command = Command::new(SSH);
        command
            .args(["-F", "/dev/null", "-p"])
            .arg(self.listener.local_addr().unwrap().port().to_string())
            .arg("-i")
            .arg(self.dir.path().join("key"))
            .args([
                "-o",
                "BatchMode=yes",
                "-o",
                "StrictHostKeyChecking=accept-new",
                "-o",
            ])
            .arg(format!(
                "UserKnownHostsFile={}",
                self.dir.path().join("known_hosts").display()
            ))
            .args(options)
            .arg(destination)
            .arg("remote command")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(File::create(self.stderr_path()).unwrap());
        command
    }

    fn accept(&self) -> Peer {
        let (mut stream, _) = self.listener.accept().unwrap();
        stream.set_read_timeout(Some(DEADLINE)).unwrap();
        let client_version = exchange_versions(&mut stream, false).unwrap();
        let mut env = Env(ed25519::SigningKey::from_seed(&[42; 32]).unwrap());
        let mut kex = Kex::new(
            Role::Server,
            &client_version,
            IDENTIFICATION,
            DEFAULT_KEX.to_vec(),
        );
        let control = stream.try_clone().unwrap();
        let mut reader = PacketReader::new(stream.try_clone().unwrap());
        let mut writer = PacketWriter::new(stream);
        conn::initial_kex(&mut reader, &mut writer, &mut kex, &mut env).unwrap();
        Peer {
            reader,
            writer,
            control,
            kex,
            env,
            client_channel: 0,
            user: String::new(),
        }
    }
}

struct Peer {
    reader: PacketReader<TcpStream>,
    writer: PacketWriter<TcpStream>,
    control: TcpStream,
    kex: Kex,
    env: Env,
    client_channel: u32,
    user: String,
}

impl Peer {
    fn send(&mut self, message: &Writer) {
        self.writer.write_packet(message.as_slice()).unwrap();
    }

    /// Past window traffic and the keepalives nobody here answers.
    fn expect(&mut self, number: u8) -> Vec<u8> {
        loop {
            let packet = self.reader.read_packet().unwrap();
            if packet[0] == number {
                return packet;
            }
            assert!(
                matches!(
                    packet[0],
                    msg::CHANNEL_WINDOW_ADJUST | msg::GLOBAL_REQUEST | msg::IGNORE
                ),
                "message {} while waiting for {number}",
                packet[0]
            );
        }
    }

    fn login(&mut self) {
        self.expect(msg::SERVICE_REQUEST);
        let mut accept = Writer::message(msg::SERVICE_ACCEPT);
        accept.str("ssh-userauth");
        self.send(&accept);
        let request = self.expect(msg::USERAUTH_REQUEST);
        self.user = Reader::new(&request[1..]).str().unwrap().to_string();
        self.writer.write_packet(&[msg::USERAUTH_SUCCESS]).unwrap();
    }

    /// Logged in, with the client's channel confirmed under this window.
    fn open(&mut self, window: u32) {
        self.login();
        let open = self.expect(msg::CHANNEL_OPEN);
        let mut reader = Reader::new(&open[1..]);
        assert_eq!(reader.str().unwrap(), "session");
        self.client_channel = reader.u32().unwrap();
        let mut confirmation = Writer::message(msg::CHANNEL_OPEN_CONFIRMATION);
        confirmation
            .u32(self.client_channel)
            .u32(0)
            .u32(window)
            .u32(1 << 15);
        self.send(&confirmation);
    }

    /// The next channel request's name, asserting whether it asks for an answer.
    fn request(&mut self, want_reply: bool) -> String {
        let request = self.expect(msg::CHANNEL_REQUEST);
        let mut reader = Reader::new(&request[1..]);
        reader.u32().unwrap();
        let name = reader.str().unwrap().to_string();
        assert_eq!(reader.bool().unwrap(), want_reply, "{name}");
        name
    }

    fn answer(&mut self, success: bool) {
        let number = if success {
            msg::CHANNEL_SUCCESS
        } else {
            msg::CHANNEL_FAILURE
        };
        let mut answer = Writer::message(number);
        answer.u32(self.client_channel);
        self.send(&answer);
    }

    /// `pty-req` granted, and the `exec` that follows it.
    fn grant_pty_and_exec(&mut self) {
        assert_eq!(self.request(true), "pty-req");
        self.answer(true);
        assert_eq!(self.request(true), "exec");
        self.answer(true);
    }

    fn data(&mut self, bytes: &[u8]) {
        let mut data = Writer::message(msg::CHANNEL_DATA);
        data.u32(self.client_channel).string(bytes);
        self.send(&data);
    }

    fn finish(&mut self, status: u32) {
        let mut exit = Writer::message(msg::CHANNEL_REQUEST);
        exit.u32(self.client_channel)
            .str("exit-status")
            .bool(false)
            .u32(status);
        self.send(&exit);
        let mut close = Writer::message(msg::CHANNEL_CLOSE);
        close.u32(self.client_channel);
        self.send(&close);
    }

    /// Sends this until the socket has taken no more for a while, or `limit` bytes
    /// have gone, and returns how much it took. The client is not read from.
    fn flood(&mut self, message: &Writer, limit: usize) -> usize {
        self.control
            .set_write_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut sent = 0;
        while sent < limit && self.writer.write_packet(message.as_slice()).is_ok() {
            sent += message.as_slice().len();
        }
        sent
    }

    /// Every reply to these is a write the client's core has to make.
    fn flood_until_the_core_is_stuck(&mut self) {
        let mut asking = Writer::message(msg::GLOBAL_REQUEST);
        asking.str("keepalive@example").bool(true);
        self.flood(&asking, FLOOD_LIMIT);
    }

    /// What arrives from here on, on another thread, as this has no `poll` either.
    fn read_in_background(mut self) -> (Receiver<Vec<u8>>, PacketWriter<TcpStream>, u32) {
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            while let Ok(packet) = self.reader.read_packet() {
                if sender.send(packet).is_err() {
                    return;
                }
            }
        });
        (receiver, self.writer, self.client_channel)
    }
}

/// A terminal the client has as stdin, as a person's would be.
struct Terminal {
    master: File,
    slave: File,
    cooked: sys::Termios,
}

impl Terminal {
    fn new() -> Self {
        let (master, path) = sys::open_pty().unwrap();
        let slave = sys::open_pty_slave(&path).unwrap();
        let cooked = sys::terminal_attributes(&slave).unwrap();
        Self {
            master,
            slave,
            cooked,
        }
    }

    fn stdin(&self) -> Stdio {
        Stdio::from(self.slave.try_clone().unwrap())
    }

    fn attributes(&self) -> sys::Termios {
        sys::terminal_attributes(&self.slave).unwrap()
    }

    fn wait_until_raw(&self) {
        within("the terminal to go raw", || {
            self.attributes() == self.cooked.raw()
        });
    }

    fn assert_cooked(&self) {
        assert_eq!(self.attributes(), self.cooked, "the terminal was left raw");
    }

    fn type_keys(&mut self, keys: &[u8]) {
        self.master.write_all(keys).unwrap();
    }

    /// More than the one piece a source waiting for credit holds in its hand.
    fn type_past_a_held_source(&mut self) {
        for _ in 0..3 {
            self.type_keys(b"some typing");
            thread::sleep(Duration::from_millis(100));
        }
    }
}

fn within(what: &str, done: impl Fn() -> bool) {
    let start = Instant::now();
    while !done() {
        assert!(start.elapsed() < DEADLINE, "never happened: {what}");
        thread::sleep(Duration::from_millis(5));
    }
}

/// The child's end, or a failure that does not leave it behind.
fn ended(child: &mut Child, scene: &Scene) -> ExitStatus {
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if start.elapsed() > DEADLINE {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the client never ended: {}", scene.stderr());
        }
        thread::sleep(Duration::from_millis(5));
    }
}

struct Killed(Child);

impl Drop for Killed {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn session_on_a_terminal(scene: &Scene, terminal: &Terminal, options: &[&str]) -> Child {
    let mut all = vec!["-tt"];
    all.extend_from_slice(options);
    scene.ssh(&all).stdin(terminal.stdin()).spawn().unwrap()
}

/// All `~.` is for: a server that stopped reading once left a client blocked in a
/// write on its only event thread, with the terminal raw, for ever.
#[test]
fn tilde_dot_ends_a_session_whose_server_stopped_reading() {
    let scene = Scene::new();
    let mut terminal = Terminal::new();
    let mut child = session_on_a_terminal(&scene, &terminal, &[]);
    let mut peer = scene.accept();
    peer.open(1 << 20);
    peer.grant_pty_and_exec();
    terminal.wait_until_raw();
    peer.flood_until_the_core_is_stuck();
    terminal.type_keys(b"\r~.");
    let status = ended(&mut child, &scene);
    assert_eq!(status.code(), Some(255), "{}", scene.stderr());
    terminal.assert_cooked();
}

/// The source waiting for window holds whatever it read, which must not be what
/// stands between the keys and the filter that looks for `~.`.
#[test]
fn tilde_dot_is_read_while_the_window_is_shut() {
    let scene = Scene::new();
    let mut terminal = Terminal::new();
    let mut child = session_on_a_terminal(&scene, &terminal, &[]);
    let mut peer = scene.accept();
    peer.open(0);
    peer.grant_pty_and_exec();
    terminal.wait_until_raw();
    terminal.type_past_a_held_source();
    terminal.type_keys(b"\r~.");
    let status = ended(&mut child, &scene);
    assert_eq!(status.code(), Some(255), "{}", scene.stderr());
    terminal.assert_cooked();
}

/// Sources stop for a rekey, the source of `~.` among them, and a server can make
/// a rekey last as long as it likes.
#[test]
fn tilde_dot_is_read_while_a_rekey_is_stuck() {
    let scene = Scene::new();
    let mut terminal = Terminal::new();
    let mut child = session_on_a_terminal(&scene, &terminal, &[]);
    let mut peer = scene.accept();
    peer.open(1 << 20);
    peer.grant_pty_and_exec();
    terminal.wait_until_raw();
    let init = peer.kex.start(&mut peer.env).unwrap();
    peer.writer.write_packet(&init).unwrap();
    assert_eq!(peer.reader.read_packet().unwrap()[0], msg::KEXINIT);
    terminal.type_past_a_held_source();
    terminal.type_keys(b"\r~.");
    let status = ended(&mut child, &scene);
    assert_eq!(status.code(), Some(255), "{}", scene.stderr());
    terminal.assert_cooked();
}

/// A write has a limit, which ends the session on its own, and the terminal comes
/// back whichever way the session went. `ServerAlive` sets a limit a test can wait
/// for. The 900 s every session otherwise has is in `session_write_limit`'s test.
#[test]
fn a_stuck_write_ends_at_its_limit_with_the_terminal_restored() {
    let scene = Scene::new();
    let terminal = Terminal::new();
    let mut child = session_on_a_terminal(
        &scene,
        &terminal,
        &["-o", "ServerAliveInterval=1", "-o", "ServerAliveCountMax=1"],
    );
    let mut peer = scene.accept();
    peer.open(1 << 20);
    peer.grant_pty_and_exec();
    terminal.wait_until_raw();
    peer.flood_until_the_core_is_stuck();
    let status = ended(&mut child, &scene);
    assert_eq!(status.code(), Some(255), "{}", scene.stderr());
    terminal.assert_cooked();
}

/// The client's core is stuck in a write of what stdin gave it, and its reader goes
/// on taking what the server sends only up to the budget.
#[test]
fn a_flood_behind_a_blocked_core_is_held_to_the_budget() {
    let scene = Scene::new();
    let child = scene
        .ssh(&[])
        .stdin(File::open("/dev/zero").unwrap())
        .spawn()
        .unwrap();
    let _child = Killed(child);
    let mut peer = scene.accept();
    peer.open(u32::MAX);
    assert_eq!(peer.request(true), "exec");
    peer.answer(true);
    thread::sleep(Duration::from_millis(500));
    let mut ignored = Writer::message(msg::IGNORE);
    ignored.string(&[0; 60_000]);
    let sent = peer.flood(&ignored, FLOOD_LIMIT);
    assert!(sent < FLOOD_LIMIT, "{sent} bytes taken while blocked");
}

/// More in hand than a rekey may hold back was once a disconnect of a healthy peer.
#[test]
fn a_slow_rekey_does_not_cut_a_busy_client() {
    let scene = Scene::new();
    let child = scene
        .ssh(&[])
        .stdin(File::open("/dev/zero").unwrap())
        .spawn()
        .unwrap();
    let _child = Killed(child);
    let mut peer = scene.accept();
    peer.open(u32::MAX);
    assert_eq!(peer.request(true), "exec");
    peer.answer(true);
    assert_eq!(peer.expect(msg::CHANNEL_DATA)[0], msg::CHANNEL_DATA);

    let init = peer.kex.start(&mut peer.env).unwrap();
    peer.writer.write_packet(&init).unwrap();
    let mut pending = loop {
        let packet = peer.reader.read_packet().unwrap();
        assert_ne!(packet[0], msg::DISCONNECT, "{}", scene.stderr());
        if packet[0] == msg::KEXINIT {
            break packet;
        }
    };
    // The source has time to pass the old bound many times over.
    thread::sleep(Duration::from_secs(2));
    while !conn::kex_step(
        &mut peer.reader,
        &mut peer.writer,
        &mut peer.kex,
        &mut peer.env,
        &pending,
    )
    .unwrap()
    {
        pending = peer.reader.read_packet().unwrap();
    }
    let mut bytes = 0;
    while bytes < STREAM_PAST_A_REKEY {
        let packet = peer.reader.read_packet().unwrap();
        assert_ne!(packet[0], msg::DISCONNECT, "{}", scene.stderr());
        if packet[0] == msg::CHANNEL_DATA {
            bytes += packet.len();
        }
    }
}

/// `ssh h yes | head -1`.
#[test]
fn a_closed_stdout_tells_the_server_and_the_client_ends_with_it() {
    let scene = Scene::new();
    let (mut ours, theirs) = UnixStream::pair().unwrap();
    let mut child = scene
        .ssh(&[])
        .stdout(Stdio::from(OwnedFd::from(theirs)))
        .spawn()
        .unwrap();
    let mut peer = scene.accept();
    peer.open(1 << 20);
    assert_eq!(peer.request(true), "exec");
    peer.answer(true);
    peer.data(b"first\n");
    let mut first = [0u8; 6];
    ours.read_exact(&mut first).unwrap();
    assert_eq!(&first, b"first\n");
    drop(ours);
    let channel = peer.client_channel;
    let (arriving, mut writer, _) = peer.read_in_background();
    let say = |writer: &mut PacketWriter<TcpStream>, message: &Writer| {
        writer.write_packet(message.as_slice()).unwrap();
    };
    let mut data = Writer::message(msg::CHANNEL_DATA);
    data.u32(channel).string(b"more\n");
    let start = Instant::now();
    loop {
        assert!(start.elapsed() < DEADLINE, "no eow: {}", scene.stderr());
        say(&mut writer, &data);
        match arriving.recv_timeout(Duration::from_millis(20)) {
            Ok(packet) if packet[0] == msg::CHANNEL_REQUEST => {
                let mut reader = Reader::new(&packet[1..]);
                reader.u32().unwrap();
                assert_eq!(reader.str().unwrap(), "eow@openssh.com");
                assert!(!reader.bool().unwrap());
                break;
            }
            _ => {}
        }
    }
    // What the server does on hearing it, the program's SIGPIPE, ends the channel.
    let mut signal = Writer::message(msg::CHANNEL_REQUEST);
    signal
        .u32(channel)
        .str("exit-signal")
        .bool(false)
        .str("PIPE")
        .bool(false)
        .str("")
        .str("");
    say(&mut writer, &signal);
    let mut close = Writer::message(msg::CHANNEL_CLOSE);
    close.u32(channel);
    say(&mut writer, &close);
    let status = ended(&mut child, &scene);
    assert_eq!(status.code(), Some(255), "{}", scene.stderr());
    assert!(
        !scene.stderr().contains("signal PIPE"),
        "{}",
        scene.stderr()
    );
}

/// The process's resident memory, in KiB.
fn resident_kib(child: &Child) -> u64 {
    let status = fs::read_to_string(format!("/proc/{}/status", child.id())).unwrap();
    status
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|kib| kib.parse().ok())
        .unwrap()
}

/// A server sending to a stdout nobody reads, a byte a message to a sixteenth of the
/// window and then as many empty messages, which the window does not count. Before,
/// each was an allocation of its own, about 55 bytes: some 14 MiB here, against
/// under 1 MiB now. What waits for that stdout is the window's bytes, not one
/// allocation a message, and the core still answers: its reply to a request sent
/// last comes after all of them.
#[test]
fn tiny_and_empty_data_to_an_unread_stdout_costs_only_its_bytes() {
    const TINY: usize = 128 * 1024;
    const EMPTY: usize = 128 * 1024;
    let scene = Scene::new();
    let (ours, theirs) = UnixStream::pair().unwrap();
    let child = scene
        .ssh(&[])
        .stdout(Stdio::from(OwnedFd::from(theirs)))
        .spawn()
        .unwrap();
    let child = Killed(child);
    let mut peer = scene.accept();
    peer.open(1 << 20);
    assert_eq!(peer.request(true), "exec");
    peer.answer(true);
    let channel = peer.client_channel;
    // An unoptimized client takes a while over a quarter of a million packets.
    peer.control
        .set_read_timeout(Some(Duration::from_secs(300)))
        .unwrap();
    let (arriving, mut writer, _) = peer.read_in_background();
    let before = resident_kib(&child.0);
    let mut tiny = Writer::message(msg::CHANNEL_DATA);
    tiny.u32(channel).string(b"x");
    for _ in 0..TINY {
        writer.write_packet(tiny.as_slice()).unwrap();
    }
    let mut empty = Writer::message(msg::CHANNEL_DATA);
    empty.u32(channel).string(b"");
    for _ in 0..EMPTY {
        writer.write_packet(empty.as_slice()).unwrap();
    }
    let mut asking = Writer::message(msg::GLOBAL_REQUEST);
    asking.str("keepalive@example").bool(true);
    writer.write_packet(asking.as_slice()).unwrap();
    let answer = loop {
        let packet = arriving
            .recv_timeout(Duration::from_secs(300))
            .unwrap_or_else(|error| {
                panic!(
                    "the core stopped answering ({error:?}, client state {:?}): {}",
                    fs::read_to_string(format!("/proc/{}/stat", child.0.id())),
                    scene.stderr()
                )
            });
        // Its stdin's end and any grant come too; the answer is what is waited for.
        if ![msg::CHANNEL_WINDOW_ADJUST, msg::CHANNEL_EOF].contains(&packet[0]) {
            break packet[0];
        }
    };
    assert_eq!(answer, msg::REQUEST_FAILURE, "{}", scene.stderr());
    let grown = resident_kib(&child.0).saturating_sub(before);
    assert!(
        grown < 4 * 1024,
        "{grown} KiB more resident for {TINY} bytes of data"
    );
    drop(ours);
}

/// A server that takes no notice of eow finishes what it had past the whole window,
/// then says how the command ended. The window for dead output was once not granted,
/// and it hung for good with stderr behind the same window.
#[test]
fn a_server_ignoring_eow_runs_past_the_window_and_its_status_stands() {
    const OUTPUT: usize = 5 * 1024 * 1024;
    let scene = Scene::new();
    let (ours, theirs) = UnixStream::pair().unwrap();
    drop(ours);
    let mut child = scene
        .ssh(&[])
        .stdout(Stdio::from(OwnedFd::from(theirs)))
        .spawn()
        .unwrap();
    let mut peer = scene.accept();
    peer.open(1 << 20);
    assert_eq!(peer.request(true), "exec");
    peer.answer(true);
    let channel = peer.client_channel;
    let (arriving, mut writer, _) = peer.read_in_background();
    let mut data = Writer::message(msg::CHANNEL_DATA);
    data.u32(channel).string(&[b'x'; 16_384]);
    let mut window = 2 * 1024 * 1024usize;
    let mut sent = 0;
    let mut told = false;
    while sent < OUTPUT {
        while window < 16_384 {
            let packet = arriving.recv_timeout(DEADLINE).unwrap_or_else(|_| {
                panic!("no window at {sent} bytes: {}", scene.stderr());
            });
            let mut reader = Reader::new(&packet[1..]);
            reader.u32().unwrap();
            match packet[0] {
                msg::CHANNEL_WINDOW_ADJUST => window += reader.u32().unwrap() as usize,
                msg::CHANNEL_REQUEST => {
                    assert_eq!(reader.str().unwrap(), "eow@openssh.com");
                    told = true;
                }
                msg::CHANNEL_EOF => {}
                other => panic!("message {other}"),
            }
        }
        writer.write_packet(data.as_slice()).unwrap();
        sent += 16_384;
        window -= 16_384;
    }
    assert!(told, "the window ran on without eow");
    let mut exit = Writer::message(msg::CHANNEL_REQUEST);
    exit.u32(channel).str("exit-status").bool(false).u32(3);
    writer.write_packet(exit.as_slice()).unwrap();
    let mut close = Writer::message(msg::CHANNEL_CLOSE);
    close.u32(channel);
    writer.write_packet(close.as_slice()).unwrap();
    let status = ended(&mut child, &scene);
    assert_eq!(status.code(), Some(3), "{}", scene.stderr());
}

/// Fills the kernel's buffers and leaves the client's core blocked writing what
/// stdin gives it, under a server that never reads again.
fn core_stuck_in_a_write_of_stdin(scene: &Scene) -> (Child, Peer) {
    let child = scene
        .ssh(&[])
        .stdin(File::open("/dev/zero").unwrap())
        .spawn()
        .unwrap();
    let mut peer = scene.accept();
    peer.open(u32::MAX);
    assert_eq!(peer.request(true), "exec");
    peer.answer(true);
    assert_eq!(peer.expect(msg::CHANNEL_DATA)[0], msg::CHANNEL_DATA);
    thread::sleep(Duration::from_millis(500));
    (child, peer)
}

/// The reader's end of connection sat queued behind the blocked send for up to the
/// write limit, and what it had queued before that was then thrown away. Half closed,
/// as a full close here would reset the connection and the kernel drop the status.
#[test]
fn a_status_behind_a_blocked_write_stands_at_the_end_of_the_connection() {
    let scene = Scene::new();
    let (child, mut peer) = core_stuck_in_a_write_of_stdin(&scene);
    let mut child = Killed(child);
    let mut exit = Writer::message(msg::CHANNEL_REQUEST);
    exit.u32(peer.client_channel)
        .str("exit-status")
        .bool(false)
        .u32(7);
    peer.send(&exit);
    peer.control.shutdown(Shutdown::Write).unwrap();
    let status = ended(&mut child.0, &scene);
    assert_eq!(status.code(), Some(7), "{}", scene.stderr());
}

#[test]
fn a_signal_behind_a_blocked_write_is_reported_at_the_end_of_the_connection() {
    let scene = Scene::new();
    let (child, mut peer) = core_stuck_in_a_write_of_stdin(&scene);
    let mut child = Killed(child);
    let mut signal = Writer::message(msg::CHANNEL_REQUEST);
    signal
        .u32(peer.client_channel)
        .str("exit-signal")
        .bool(false)
        .str("KILL")
        .bool(false)
        .str("")
        .str("");
    peer.send(&signal);
    peer.control.shutdown(Shutdown::Write).unwrap();
    let status = ended(&mut child.0, &scene);
    assert_eq!(status.code(), Some(255), "{}", scene.stderr());
    assert!(
        scene.stderr().contains("exited on signal KILL"),
        "{}",
        scene.stderr()
    );
}

/// Output queued with a status has nowhere to go, and a success would say it arrived.
#[test]
fn output_lost_behind_a_blocked_write_takes_the_status_with_it() {
    let scene = Scene::new();
    let (child, mut peer) = core_stuck_in_a_write_of_stdin(&scene);
    let mut child = Killed(child);
    peer.data(b"what the command said");
    let mut exit = Writer::message(msg::CHANNEL_REQUEST);
    exit.u32(peer.client_channel)
        .str("exit-status")
        .bool(false)
        .u32(0);
    peer.send(&exit);
    peer.control.shutdown(Shutdown::Write).unwrap();
    let status = ended(&mut child.0, &scene);
    assert_eq!(status.code(), Some(255), "{}", scene.stderr());
    assert!(scene.stderr().contains("was lost"), "{}", scene.stderr());
}

/// A failed core leaves the socket open and the reader running, and a stalled sink
/// holds the way out open for as long as this peer likes.
#[test]
fn reports_sent_to_a_failed_core_cost_no_memory() {
    const FLOODED: usize = 64 << 20;
    const CEILING_KB: u64 = 32 << 10;
    let scene = Scene::new();
    let mut child = Killed(scene.ssh(&[]).stdout(Stdio::piped()).spawn().unwrap());
    let _unread = child.0.stdout.take();
    let mut peer = scene.accept();
    peer.open(1 << 30);
    assert_eq!(peer.request(true), "exec");
    peer.answer(true);
    for _ in 0..8 {
        peer.data(&[0; 30_000]);
    }
    let mut twice = Writer::message(msg::CHANNEL_OPEN_CONFIRMATION);
    twice
        .u32(peer.client_channel)
        .u32(0)
        .u32(1 << 20)
        .u32(1 << 15);
    peer.send(&twice);
    thread::sleep(Duration::from_millis(200));
    let mut signal = Writer::message(msg::CHANNEL_REQUEST);
    signal
        .u32(peer.client_channel)
        .str("exit-signal")
        .bool(false)
        .str(&"K".repeat(60_000))
        .bool(false)
        .str("")
        .str("");
    assert!(peer.flood(&signal, FLOODED) >= FLOODED);
    let status = fs::read_to_string(format!("/proc/{}/status", child.0.id())).unwrap();
    let resident: u64 = status
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))
        .and_then(|value| value.split_whitespace().next())
        .and_then(|value| value.parse().ok())
        .unwrap();
    assert!(resident < CEILING_KB, "{resident} kB resident");
}

/// A write that failed because the server had already closed the session and gone
/// is no lost connection.
#[test]
fn a_close_behind_a_blocked_write_is_a_clean_end() {
    let scene = Scene::new();
    let (child, mut peer) = core_stuck_in_a_write_of_stdin(&scene);
    let mut child = Killed(child);
    peer.finish(7);
    peer.control.shutdown(Shutdown::Write).unwrap();
    let status = ended(&mut child.0, &scene);
    assert_eq!(status.code(), Some(7), "{}", scene.stderr());
    assert!(
        !scene.stderr().contains("Connection closed"),
        "{}",
        scene.stderr()
    );
}

/// A server's end of connection straight after its close beats our reply to it.
#[test]
fn a_close_with_the_end_of_the_connection_straight_after_is_a_clean_end() {
    for _ in 0..4 {
        let scene = Scene::new();
        let mut child = Killed(scene.ssh(&[]).spawn().unwrap());
        let mut peer = scene.accept();
        peer.open(1 << 20);
        assert_eq!(peer.request(true), "exec");
        peer.answer(true);
        peer.finish(0);
        peer.control.shutdown(Shutdown::Write).unwrap();
        let status = ended(&mut child.0, &scene);
        assert_eq!(status.code(), Some(0), "{}", scene.stderr());
        assert!(
            !scene.stderr().contains("Connection closed"),
            "{}",
            scene.stderr()
        );
    }
}

/// No status, so no command result: said at once, and in the words of a lost peer,
/// not those of the write the reader's end of connection broke.
#[test]
fn the_end_of_the_connection_ends_a_blocked_write_in_its_own_words() {
    let scene = Scene::new();
    let (child, peer) = core_stuck_in_a_write_of_stdin(&scene);
    let mut child = Killed(child);
    peer.control.shutdown(Shutdown::Write).unwrap();
    let status = ended(&mut child.0, &scene);
    assert_eq!(status.code(), Some(255), "{}", scene.stderr());
    assert!(
        scene.stderr().contains("Connection closed by remote host"),
        "{}",
        scene.stderr()
    );
}

/// A core printing into a stderr nobody reads is out of any socket's reach, so the
/// thread that reads the keys ends the process, the terminal put back first.
#[test]
fn tilde_dot_ends_a_session_stuck_printing_into_an_unread_stderr() {
    let scene = Scene::new();
    let mut terminal = Terminal::new();
    let (unread, theirs) = UnixStream::pair().unwrap();
    let mut child = scene
        .ssh(&["-tt"])
        .stdin(terminal.stdin())
        .stderr(Stdio::from(OwnedFd::from(theirs)))
        .spawn()
        .unwrap();
    let mut peer = scene.accept();
    peer.open(1 << 20);
    peer.grant_pty_and_exec();
    terminal.wait_until_raw();
    let mut signal = Writer::message(msg::CHANNEL_REQUEST);
    signal
        .u32(peer.client_channel)
        .str("exit-signal")
        .bool(false)
        .str(&"K".repeat(4096))
        .bool(false)
        .str("")
        .str("");
    peer.flood(&signal, FLOOD_LIMIT);
    terminal.type_keys(b"\r~.");
    let status = ended(&mut child, &scene);
    assert_eq!(status.code(), Some(255));
    terminal.assert_cooked();
    drop(unread);
}

/// Full and non-blocking when it starts: `EAGAIN` was an exit of 255 from `-V`, a lost
/// diagnostic from the rest.
fn output_through_a_full_nonblocking_socket(args: &[&str]) -> (Option<i32>, String) {
    let (mut ours, theirs) = UnixStream::pair().unwrap();
    theirs.set_nonblocking(true).unwrap();
    let mut filling = &theirs;
    let mut prefill = 0;
    loop {
        match filling.write(&[b'.'; 4096]) {
            Ok(written) => prefill += written,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(error) => panic!("{error}"),
        }
    }
    let mut child = Command::new(SSH)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::from(OwnedFd::from(theirs.try_clone().unwrap())))
        .stderr(Stdio::from(OwnedFd::from(theirs)))
        .spawn()
        .unwrap();
    thread::sleep(Duration::from_millis(100));
    let mut all = Vec::new();
    ours.read_to_end(&mut all).unwrap();
    let status = child.wait().unwrap();
    let text = String::from_utf8_lossy(all.get(prefill..).unwrap_or_default()).to_string();
    (status.code(), text)
}

#[test]
fn primary_output_waits_for_room_on_a_nonblocking_stream() {
    let (code, text) = output_through_a_full_nonblocking_socket(&["-V"]);
    assert_eq!(code, Some(0), "{text}");
    assert!(text.starts_with("SSH-2.0-"), "{text}");
}

#[test]
fn a_diagnostic_waits_for_room_on_a_nonblocking_stream() {
    let (code, text) = output_through_a_full_nonblocking_socket(&["-p", "nonsense", "h"]);
    assert_eq!(code, Some(255), "{text}");
    assert!(text.contains("nonsense"), "{text}");
}

/// Refused, so it stays as the person left it, and the command goes on without one.
#[test]
fn a_refused_pty_stays_cooked_and_says_so() {
    let scene = Scene::new();
    let terminal = Terminal::new();
    let mut child = session_on_a_terminal(&scene, &terminal, &[]);
    let mut peer = scene.accept();
    peer.open(1 << 20);
    assert_eq!(peer.request(true), "pty-req");
    peer.answer(false);
    assert_eq!(peer.request(true), "exec");
    peer.answer(true);
    within("the refusal to be said", || {
        scene
            .stderr()
            .contains("PTY allocation request failed on channel 0")
    });
    terminal.assert_cooked();
    peer.data(b"output\n");
    peer.finish(3);
    let status = ended(&mut child, &scene);
    assert_eq!(status.code(), Some(3), "{}", scene.stderr());
    terminal.assert_cooked();
}

/// Raw is for a terminal that exists, so it waits for the answer.
#[test]
fn a_terminal_goes_raw_on_the_answer_and_comes_back_at_the_end() {
    let scene = Scene::new();
    let terminal = Terminal::new();
    let mut child = session_on_a_terminal(&scene, &terminal, &[]);
    let mut peer = scene.accept();
    peer.open(1 << 20);
    assert_eq!(peer.request(true), "pty-req");
    // The exec comes straight after, so it has passed the point it could go raw.
    assert_eq!(peer.request(true), "exec");
    terminal.assert_cooked();
    peer.answer(true);
    terminal.wait_until_raw();
    peer.answer(true);
    peer.finish(0);
    let status = ended(&mut child, &scene);
    assert_eq!(status.code(), Some(0), "{}", scene.stderr());
    terminal.assert_cooked();
}

/// A parent's `O_NONBLOCK` on the open file description came with the descriptor.
/// `EAGAIN` meant a silent loss, and a status of 0.
#[test]
fn an_inherited_nonblocking_stdout_loses_nothing() {
    const TOTAL: usize = 3_000_000;
    let scene = Scene::new();
    let (ours, theirs) = UnixStream::pair().unwrap();
    theirs.set_nonblocking(true).unwrap();
    let mut child = scene
        .ssh(&[])
        .stdout(Stdio::from(OwnedFd::from(theirs)))
        .spawn()
        .unwrap();
    let collector = thread::spawn(move || {
        let mut ours = ours;
        // Long enough that the socket's buffer has filled.
        thread::sleep(Duration::from_millis(300));
        let mut all = Vec::new();
        ours.read_to_end(&mut all).unwrap();
        all
    });
    let mut peer = scene.accept();
    peer.open(1 << 20);
    assert_eq!(peer.request(true), "exec");
    peer.answer(true);
    let channel = peer.client_channel;
    let (arriving, mut writer, _) = peer.read_in_background();
    let mut window = 2 * 1024 * 1024usize;
    let mut sent = 0;
    while sent < TOTAL {
        while window < 16_384 {
            let packet = arriving.recv_timeout(DEADLINE).unwrap();
            if packet[0] == msg::CHANNEL_WINDOW_ADJUST {
                let mut reader = Reader::new(&packet[1..]);
                reader.u32().unwrap();
                window += reader.u32().unwrap() as usize;
            }
        }
        let piece = (TOTAL - sent).min(16_384);
        let mut data = Writer::message(msg::CHANNEL_DATA);
        data.u32(channel).string(&vec![b'x'; piece]);
        writer.write_packet(data.as_slice()).unwrap();
        sent += piece;
        window -= piece;
    }
    let mut exit = Writer::message(msg::CHANNEL_REQUEST);
    exit.u32(channel).str("exit-status").bool(false).u32(0);
    writer.write_packet(exit.as_slice()).unwrap();
    let mut close = Writer::message(msg::CHANNEL_CLOSE);
    close.u32(channel);
    writer.write_packet(close.as_slice()).unwrap();
    let status = ended(&mut child, &scene);
    assert_eq!(status.code(), Some(0), "{}", scene.stderr());
    assert_eq!(collector.join().unwrap().len(), TOTAL);
}

/// `EAGAIN` on stdin is nothing yet, which became a false end of input.
#[test]
fn an_inherited_nonblocking_stdin_is_not_an_end_of_input() {
    let scene = Scene::new();
    let (mut ours, theirs) = UnixStream::pair().unwrap();
    theirs.set_nonblocking(true).unwrap();
    let mut child = scene
        .ssh(&[])
        .stdin(Stdio::from(OwnedFd::from(theirs)))
        .spawn()
        .unwrap();
    let mut peer = scene.accept();
    peer.open(1 << 20);
    assert_eq!(peer.request(true), "exec");
    peer.answer(true);
    let channel = peer.client_channel;
    let (arriving, mut writer, _) = peer.read_in_background();
    if let Ok(packet) = arriving.recv_timeout(Duration::from_millis(300)) {
        assert_ne!(packet[0], msg::CHANNEL_EOF, "stdin has not ended");
    }
    ours.write_all(b"typed").unwrap();
    drop(ours);
    let mut received = Vec::new();
    loop {
        let packet = arriving.recv_timeout(DEADLINE).unwrap();
        if packet[0] == msg::CHANNEL_EOF {
            break;
        }
        assert_eq!(packet[0], msg::CHANNEL_DATA);
        let mut reader = Reader::new(&packet[1..]);
        reader.u32().unwrap();
        received.extend_from_slice(reader.string().unwrap());
    }
    assert_eq!(received, b"typed");
    let mut exit = Writer::message(msg::CHANNEL_REQUEST);
    exit.u32(channel).str("exit-status").bool(false).u32(0);
    writer.write_packet(exit.as_slice()).unwrap();
    let mut close = Writer::message(msg::CHANNEL_CLOSE);
    close.u32(channel);
    writer.write_packet(close.as_slice()).unwrap();
    assert_eq!(ended(&mut child, &scene).code(), Some(0));
}

/// The command finished and said how. Losing the connection afterwards changes
/// nothing about that.
#[test]
fn a_connection_lost_after_the_status_returns_the_status() {
    let scene = Scene::new();
    let mut child = scene.ssh(&[]).spawn().unwrap();
    let mut peer = scene.accept();
    peer.open(1 << 20);
    assert_eq!(peer.request(true), "exec");
    peer.answer(true);
    let mut exit = Writer::message(msg::CHANNEL_REQUEST);
    exit.u32(peer.client_channel)
        .str("exit-status")
        .bool(false)
        .u32(7);
    peer.send(&exit);
    // Not a drop: closing on client bytes unread is a reset, which throws away a
    // status not yet read, as DESIGN.md section 6 says.
    peer.control.shutdown(Shutdown::Write).unwrap();
    assert_eq!(
        ended(&mut child, &scene).code(),
        Some(7),
        "{}",
        scene.stderr()
    );
}

/// Neither `USER` nor `LOGNAME`, as under td-agent: not somebody called `unknown`.
#[test]
fn with_no_user_in_the_environment_the_name_comes_from_the_real_uid() {
    let scene = Scene::new();
    let mut command = scene.ssh_as(&[], "127.0.0.1");
    let mut child = command
        .env_remove("USER")
        .env_remove("LOGNAME")
        .spawn()
        .unwrap();
    let own = passwd_name_of_this_uid();
    let Some(own) = own else {
        // No entry for this uid: refused by name, before anything connects.
        assert_eq!(ended(&mut child, &scene).code(), Some(255));
        assert!(
            scene.stderr().contains("No user exists"),
            "{}",
            scene.stderr()
        );
        return;
    };
    let mut peer = scene.accept();
    peer.login();
    assert_eq!(peer.user, own);
    let _ = child.kill();
    let _ = child.wait();
}

fn passwd_name_of_this_uid() -> Option<String> {
    let status = fs::read_to_string("/proc/self/status").ok()?;
    let uid = status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))?
        .split_whitespace()
        .next()?
        .to_string();
    let table = fs::read_to_string("/etc/passwd").ok()?;
    table.lines().find_map(|line| {
        let fields: Vec<&str> = line.split(':').collect();
        (fields.get(2) == Some(&uid.as_str())).then(|| fields[0].to_string())
    })
}

/// A command is one packet. Past that it says so, rather than failing mid-setup
/// with a word about a packet.
#[test]
fn a_command_too_long_for_a_packet_is_refused_in_plain_words() {
    let scene = Scene::new();
    let long = "x".repeat(100 * 1024);
    let mut command = Command::new(SSH);
    let output = command
        .args(["-F", "/dev/null", "-p"])
        .arg(scene.listener.local_addr().unwrap().port().to_string())
        .arg("u@127.0.0.1")
        .arg(&long)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(255));
    let text = String::from_utf8_lossy(&output.stderr);
    assert!(text.contains("remote command is too long"), "{text}");
}
