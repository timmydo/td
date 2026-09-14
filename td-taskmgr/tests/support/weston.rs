//! Optional real Weston compatibility leg; no downloaded runtime dependency.
use super::native_compositor::{chord, counter, wait_state, TaskProcess};
use super::*;
struct Server(Child);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
#[test]
#[ignore = "set TD_TEST_WESTON to an installed absolute Weston executable"]
fn real_weston_configures_releases_buffers_and_keeps_collection_live() {
    let binary =
        PathBuf::from(std::env::var_os("TD_TEST_WESTON").expect("TD_TEST_WESTON required"));
    assert!(binary.is_absolute());
    let directory = Directory::new();
    let log = directory.0.join("weston.log");
    let mut server = Server(
        Command::new(binary)
            .args([
                "--backend=headless-backend.so",
                "--use-pixman",
                "--width=1280",
                "--height=960",
                "--idle-time=0",
                "--no-config",
                "--socket=wayland-test",
                "--logger-scopes=proto",
            ])
            .arg(format!("--log={}", log.display()))
            .env_remove("WAYLAND_SOCKET")
            .env_remove("WAYLAND_DISPLAY")
            .env("XDG_RUNTIME_DIR", &directory.0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(directory.0.join("weston.stderr")).unwrap())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + TIMEOUT;
    while !directory.0.join("wayland-test").exists() {
        assert!(server.0.try_wait().unwrap().is_none(), "Weston exited");
        assert!(Instant::now() < deadline, "Weston readiness deadline");
        std::thread::sleep(Duration::from_millis(20));
    }
    let client_directory = Directory::new();
    let client = TaskProcess::start(&client_directory, &directory.0.join("wayland-test"));
    while !client_directory.0.join("control").exists() {
        assert!(Instant::now() < deadline, "client startup deadline");
        std::thread::sleep(Duration::from_millis(20));
    }
    let initial = wait_state(&client, |s| {
        counter(s, "retained") >= 2 && counter(s, "presentations") >= 2
    });
    chord(&client, "C-f");
    chord(&client, "t");
    wait_state(&client, |s| s.get("query").is_some_and(|q| q == "74"));
    chord(&client, "Escape");
    for _ in 0..4 {
        chord(&client, "S-Tab");
    }
    chord(&client, "Right");
    wait_state(&client, |s| s.get("tab").is_some_and(|tab| tab == "CPU"));
    wait_state(&client, |s| {
        counter(s, "newest_ns") > counter(&initial, "newest_ns")
    });
    // Scope wire evidence to the exact client that set our app id.
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let mut bytes = Vec::new();
        std::fs::File::open(&log)
            .unwrap()
            .take(4 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .unwrap();
        assert!(bytes.len() <= 4 * 1024 * 1024);
        let text = String::from_utf8(bytes).unwrap();
        let identity = text
            .lines()
            .find(|line| line.contains("set_app_id(\"td-taskmgr\")"))
            .and_then(|line| line.split_once("client "))
            .and_then(|(_, tail)| tail.split_whitespace().next());
        if let Some(identity) = identity {
            let marker = format!("client {identity} ");
            let own: Vec<&str> = text.lines().filter(|line| line.contains(&marker)).collect();
            let attachments = own
                .iter()
                .filter(|line| line.contains("rq wl_surface@") && line.contains(".attach("))
                .count();
            let callbacks = own
                .iter()
                .filter(|line| line.contains("ev wl_callback@") && line.contains(".done("))
                .count();
            let releases = own
                .iter()
                .filter(|line| line.contains("ev wl_buffer@") && line.contains(".release("))
                .count();
            let configured = own
                .iter()
                .any(|line| line.contains("ev xdg_toplevel@") && line.contains(".configure("));
            if attachments >= 2 && callbacks >= 2 && releases >= 1 && configured {
                break;
            }
        }
        assert!(
            Instant::now() < deadline,
            "missing client-specific Weston protocol evidence"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(client.request(3, &["action", "quit"]), ["ok", "quit"]);
    assert!(client.finish());
}
