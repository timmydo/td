//! Host-only disposable VM proof using the real target task manager and compositor.
use super::*;
const DIRECTORY: &str = "/run/td-compositor/1000";
const EVIDENCE: &str = "/run/user/1000/taskmgr-evidence";
const FRAME: usize = 800 * 600 * 3;
pub(super) const PROOF_PREFIX: &str =
    "TD-TASKMGR-VM: uid=1000 caps=0 session=human pid-view=outer stop=ok resume=ok capture=";
pub(super) const IMAGE_PROOF: &str =
    "TD-TASKMGR-VM: programs loaded from read-only deployment EROFS; taskmgr runtime/debug indexed";

fn human(arguments: &[&str]) -> Command {
    let mut command = Command::new("/bin/td-login");
    command.args(["exec-as", "alice", "--"]).args(arguments);
    command
}
fn line(output: impl Read + Send + 'static) -> String {
    let (send, receive) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        use std::io::BufRead;
        std::io::BufReader::new(output)
            .take(4097)
            .read_line(&mut text)
            .unwrap();
        assert!(text.len() <= 4096);
        send.send(text).unwrap();
    });
    let text = receive.recv_timeout(Duration::from_secs(20)).unwrap();
    reader.join().unwrap();
    text
}
pub(super) fn mount_image() {
    assert!(Command::new("/bin/busybox")
        .args(["mount", "-t", "sysfs", "none", "/sys"])
        .status()
        .unwrap()
        .success());
    let until = Instant::now() + Duration::from_secs(5);
    let device = loop {
        if let Ok(device) = fs::read_to_string("/sys/class/block/vda/dev") {
            break device;
        }
        assert!(Instant::now() < until, "image block device did not appear");
        std::thread::sleep(Duration::from_millis(20));
    };
    let (major, minor) = device.trim().split_once(':').unwrap();
    assert!(Command::new("/bin/busybox")
        .args(["mknod", "/dev/vda", "b", major, minor])
        .status()
        .unwrap()
        .success());
    fs::create_dir("/image").unwrap();
    assert!(Command::new("/bin/busybox")
        .args(["mount", "-t", "erofs", "-o", "ro", "/dev/vda", "/image"])
        .status()
        .unwrap()
        .success());
    std::os::unix::fs::symlink("/image/td", "/td").unwrap();
    for binary in [
        "td-authd",
        "td-firstboot",
        "td-login",
        "td-compositor",
        "td-taskmgr",
    ] {
        fs::remove_file(format!("/bin/{binary}")).unwrap();
        std::os::unix::fs::symlink(format!("/image/bin/{binary}"), format!("/bin/{binary}"))
            .unwrap();
        assert!(fs::canonicalize(format!("/bin/{binary}"))
            .unwrap()
            .starts_with("/image/td/store"));
    }
    let canonical = fs::canonicalize("/bin/td-taskmgr").unwrap();
    let runtime = Path::new("/").join(canonical.strip_prefix("/image").unwrap());
    let debug = runtime
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("lib/debug/bin/td-taskmgr.debug");
    let mut index = String::new();
    fs::File::open("/image/etc/td-profiler-objects.tsv")
        .unwrap()
        .take(16 * 1024 * 1024 + 1)
        .read_to_string(&mut index)
        .unwrap();
    assert!(index.len() <= 16 * 1024 * 1024 && index.starts_with("td-profiler-objects-v1\n"));
    assert!(index.lines().any(|line| {
        let mut fields = line.split('\t');
        fields.next() == runtime.to_str() && fields.next() == debug.to_str()
    }));
    assert!(
        fs::metadata(Path::new("/image").join(debug.strip_prefix("/").unwrap()))
            .unwrap()
            .len()
            > 0
    );
    println!("{IMAGE_PROOF}");
}
pub(super) fn run() {
    fs::create_dir_all("/run/td-compositor").unwrap();
    std::os::unix::fs::chown("/run/td-compositor", Some(1000), Some(1000)).unwrap();
    let mut compositor = Guest(
        human(&[
            "/bin/td-compositor",
            "headless",
            "--session-dir",
            DIRECTORY,
            "--width",
            "800",
            "--height",
            "600",
            "--input-control",
            "enabled",
            "--capture-control",
            "enabled",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap(),
    );
    let ready = line(compositor.0.stdout.take().unwrap());
    let session = ready
        .strip_prefix("TD-COMPOSITOR-HEADLESS-READY version=2 session=")
        .unwrap()
        .strip_suffix(" width=800 height=600 scale=1\n")
        .unwrap();
    assert_eq!(session.len(), 32);
    fs::write("/run/user/1000/taskmgr-session", session).unwrap();
    let mut driver = Guest(human(&["/pair-probe", "--taskmgr-driver"]).spawn().unwrap());
    let (first, second) = UnixStream::pair().unwrap();
    let mut authority = Guest(
        Command::new("/bin/td-authd")
            .args([
                "terminal-serve",
                "--user",
                "alice",
                "--uid",
                "1000",
                "--peer-uid",
                "993",
            ])
            .stdin(Stdio::from(std::os::fd::OwnedFd::from(first)))
            .spawn()
            .unwrap(),
    );
    let mut peer = Guest(
        Command::new("/bin/td-login")
            .args([
                "exec-service-as",
                "tdc1000",
                "--",
                "/pair-probe",
                "--taskmgr-peer",
            ])
            .stdin(Stdio::from(std::os::fd::OwnedFd::from(second)))
            .spawn()
            .unwrap(),
    );
    assert!(wait_for(&mut driver.0, Duration::from_secs(90)).success());
    assert!(wait(&mut peer.0).success());
    assert!(!wait(&mut authority.0).success());
    compositor.0.stdin.take();
    assert!(wait(&mut compositor.0).success());
    let evidence = fs::read_to_string(EVIDENCE).unwrap();
    assert!(evidence
        .starts_with("uid=1000 caps=0 session=human pid-view=outer stop=ok resume=ok capture="));
    println!("TD-TASKMGR-VM: {evidence}");
}
pub(super) fn peer() {
    let mut connection = channel::Channel::from_stdin(0).unwrap();
    assert_eq!(connection.receive().unwrap(), b"TDLA002\n");
    connection.send(b"TDLA002\n").unwrap();
    assert_eq!(connection.receive().unwrap(), [0x80]);
    connection.send(&[7]).unwrap();
    assert_eq!(
        connection.receive().unwrap(),
        [0x81, 0, 0, 0, 0, 0, 0, 0, 1]
    );
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        connection.send(&[2, 0, 0, 0, 0, 0, 0, 0, 1]).unwrap();
        let status = connection.receive().unwrap();
        if status == [0x82, 1] {
            break;
        }
        assert_eq!(status, [0x82, 0], "task manager failed: {status:?}");
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(Path::new(EVIDENCE).is_file());
}
pub(super) fn owned() {
    fs::write("/proc/self/comm", "td-tm-fixture").unwrap();
    println!("owned {}", std::process::id());
    std::io::stdout().flush().unwrap();
    park();
}
struct Driver {
    session: String,
    action: u64,
}
impl Driver {
    fn request(&self, text: &str, limit: usize) -> Vec<u8> {
        let mut stream = UnixStream::connect(format!("{DIRECTORY}/td-control")).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        writeln!(stream, "{text}").unwrap();
        let mut reply = Vec::new();
        stream
            .take(limit as u64 + 1)
            .read_to_end(&mut reply)
            .unwrap();
        assert!(reply.len() <= limit);
        reply
    }
    fn key(&mut self, code: u32, down: bool) {
        self.action += 1;
        let reply = self.request(
            &format!(
                "key {} {} {code} {}",
                self.session,
                self.action,
                if down { "down" } else { "up" }
            ),
            1024,
        );
        assert_eq!(
            reply,
            format!(
                "ok\ntd-action-v1 session={} action={}\n",
                self.session, self.action
            )
            .as_bytes()
        );
    }
    fn tap(&mut self, code: u32) {
        self.key(code, true);
        self.key(code, false);
    }
    fn ctrl(&mut self, code: u32) {
        self.key(29, true);
        self.tap(code);
        self.key(29, false);
    }
    fn window(&self) -> Option<String> {
        let layout = self.request("layout", 65536);
        let text = std::str::from_utf8(&layout).unwrap();
        text.lines()
            .filter(|line| line.starts_with("window "))
            .find(|line| field(line, "app_id=") == Some("td-taskmgr"))
            .map(|line| field(line, "id=").unwrap().to_string())
    }
    fn observe(&self, window: &str) -> (u64, u64, u64, bool) {
        let reply = self.request(&format!("observe-client {} {window}", self.session), 1024);
        let text = std::str::from_utf8(&reply).unwrap();
        assert!(text.starts_with(&format!(
            "ok\ntd-client-v1 session={} window={window} ",
            self.session
        )));
        (
            field(text, "client=").unwrap().parse().unwrap(),
            field(text, "commit=").unwrap().parse().unwrap(),
            field(text, "output=").unwrap().parse().unwrap(),
            field(text, "current=") == Some("yes"),
        )
    }
    fn capture(&self, window: &str) -> (u64, Vec<u8>) {
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            assert!(Instant::now() < until, "capture did not settle");
            let before = self.observe(window);
            if !before.3 {
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
            let reply = self.request("capture", FRAME + 128);
            let prefix = format!("ok\nP6\n# td-output-v1 session={} output=", self.session);
            let body = reply.strip_prefix(prefix.as_bytes()).unwrap();
            let end = body.iter().position(|b| *b == b'\n').unwrap();
            let output: u64 = std::str::from_utf8(&body[..end]).unwrap().parse().unwrap();
            let pixels = body[end + 1..].strip_prefix(b"800 600\n255\n").unwrap();
            assert_eq!(pixels.len(), FRAME);
            let after = self.observe(window);
            if before.0 != after.0 || before.1 != after.1 || !after.3 {
                continue;
            }
            assert!(before.0 > 0 && before.1 > 0 && output > before.2 && output <= after.2);
            return (after.1, pixels.to_vec());
        }
    }
    fn menu(&mut self, down: usize, window: &str) {
        self.tap(68); // F10
        self.tap(106); // Right into selected-process scope.
        for _ in 0..down {
            self.tap(108);
        }
        self.tap(28);
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            let (_, pixels) = self.capture(window);
            let layout = self.request("layout", 65536);
            let text = std::str::from_utf8(&layout).unwrap();
            let row = text
                .lines()
                .find(|line| field(line, "id=") == Some(window))
                .unwrap();
            let axis = |key| field(row, key).unwrap().parse::<usize>().unwrap();
            let (x, y, height) = (axis("x="), axis("y="), axis("height="));
            // Shared confirmation: y=24, height=client-72, two 24px actions.
            let offset = ((y + height - 96 + 1) * 800 + x + 17) * 3;
            if pixels.get(offset..offset + 3) == Some(&[0xc9, 0xc1, 0xb2])
                && pixels.get(offset + 24 * 800 * 3..offset + 24 * 800 * 3 + 3)
                    == Some(&[0xe1, 0xdb, 0xcf])
            {
                break;
            }
            if Instant::now() >= until {
                let mut palette = Vec::<[u8; 3]>::new();
                let mut diagnostic = String::new();
                use std::fmt::Write;
                for pixel in pixels.as_chunks::<3>().0 {
                    let pixel = *pixel;
                    let index = palette.iter().position(|p| *p == pixel).unwrap_or_else(|| {
                        assert!(palette.len() < 256);
                        palette.push(pixel);
                        palette.len() - 1
                    });
                    write!(diagnostic, "{index:02x}").unwrap();
                }
                let mut colors = String::new();
                for color in palette {
                    for byte in color {
                        write!(colors, "{byte:02x}").unwrap();
                    }
                }
                println!("TD-TASKMGR-PALETTE: {colors}");
                println!("TD-TASKMGR-FRAME-INDEXED: {diagnostic}");
                println!("TD-TASKMGR-LAYOUT: {text}");
                panic!("default-Cancel confirmation was not painted");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
fn field<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    text.split_whitespace()
        .find_map(|part| part.strip_prefix(key))
}
fn stopped(pid: u32, wanted: bool) {
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        let stat = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
        let state = stat
            .rsplit_once(") ")
            .unwrap()
            .1
            .split_whitespace()
            .next()
            .unwrap();
        if matches!(state, "T" | "t") == wanted {
            return;
        }
        assert!(
            Instant::now() < until,
            "owned child state {state}, wanted stopped={wanted}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn verify_identity() {
    // Identification is read-only; every signal target below comes from Child::id.
    let mut found = 0;
    let executable = fs::canonicalize("/bin/td-taskmgr").unwrap();
    for entry in fs::read_dir("/proc").unwrap() {
        let entry = entry.unwrap();
        let Ok(pid) = entry.file_name().to_str().unwrap().parse::<u32>() else {
            continue;
        };
        if fs::read_link(entry.path().join("exe")).ok().as_deref() != Some(executable.as_path()) {
            continue;
        }
        found += 1;
        let status = fs::read_to_string(entry.path().join("status")).unwrap();
        for key in ["Uid:", "Gid:"] {
            let values: Vec<_> = status
                .lines()
                .find_map(|l| l.strip_prefix(key))
                .unwrap()
                .split_whitespace()
                .collect();
            assert_eq!(values, ["1000"; 4]);
        }
        for key in ["CapPrm:", "CapEff:", "CapAmb:"] {
            assert_eq!(
                status
                    .lines()
                    .find_map(|l| l.strip_prefix(key))
                    .unwrap()
                    .trim(),
                "0000000000000000"
            );
        }
        assert_eq!(
            status
                .lines()
                .find_map(|l| l.strip_prefix("NStgid:"))
                .unwrap()
                .trim(),
            pid.to_string()
        );
        assert_eq!(
            fs::read_to_string(entry.path().join("cgroup")).unwrap(),
            "0::/td-user-1000/session\n"
        );
        for fd in 0..=2 {
            assert_eq!(
                fs::metadata(entry.path().join(format!("fd/{fd}")))
                    .unwrap()
                    .rdev(),
                0x103
            );
        }
    }
    assert_eq!(found, 1, "one authority-launched task manager");
}
pub(super) fn driver() {
    let mut target = Guest(
        Command::new("/pair-probe")
            .arg("--taskmgr-owned")
            .stdout(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    assert_eq!(
        line(target.0.stdout.take().unwrap()).trim(),
        format!("owned {}", target.0.id())
    );
    let mut driver = Driver {
        session: fs::read_to_string("/run/user/1000/taskmgr-session").unwrap(),
        action: 0,
    };
    let until = Instant::now() + Duration::from_secs(10);
    let window = loop {
        if let Some(window) = driver.window() {
            break window;
        }
        assert!(Instant::now() < until, "task manager mapping");
        std::thread::sleep(Duration::from_millis(20));
    };
    verify_identity();
    let (commit, first) = driver.capture(&window);
    std::thread::sleep(Duration::from_millis(1200));
    let (later, pixels) = driver.capture(&window);
    assert!(
        later > commit && first != pixels,
        "live observation repaint"
    );
    driver.ctrl(33); // Search the owned fixture's unique comm, then End selects it.
    for code in [20, 32, 12, 20, 50, 12, 33, 23, 45, 20, 22, 19, 18] {
        driver.tap(code);
    }
    driver.tap(15);
    driver.tap(107);
    driver.menu(2, &window);
    let (_, confirmation) = driver.capture(&window);
    assert_ne!(confirmation, pixels, "mapped confirmation");
    driver.tap(28); // Default Cancel must not stop the child.
    std::thread::sleep(Duration::from_millis(100));
    stopped(target.0.id(), false);
    for (choice, wanted) in [(2, true), (3, false)] {
        driver.menu(choice, &window);
        driver.tap(15);
        driver.tap(28);
        stopped(target.0.id(), wanted);
        std::thread::sleep(Duration::from_millis(100));
        driver.tap(1);
    }
    let hash = confirmation.iter().fold(0xcbf29ce484222325u64, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x100000001b3)
    });
    fs::write(
        EVIDENCE,
        format!(
            "uid=1000 caps=0 session=human pid-view=outer stop=ok resume=ok capture={hash:016x}\n"
        ),
    )
    .unwrap();
    driver.ctrl(16);
}
