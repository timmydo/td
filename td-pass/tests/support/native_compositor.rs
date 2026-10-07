//! The real headless `td-compositor` driving the td-pass window. td's
//! shared native harness (`td-test-compositor`) launches it from the
//! `TD_TEST_COMPOSITOR` binary with input, capture and clipboard control,
//! injects keys through its seat, captures the window's tile and holds
//! clipboard transfers; `Pass` launches td-pass against its Wayland
//! socket with every directory it reads or writes inside the test's own,
//! and reads the frames it keeps through `/proc`. The production case
//! runs the shipped backend; the `fixture` cases run the test vault
//! (`src/backend/fixture.rs`), which the gate builds with `test-vault`.

use super::*;

use std::process::ExitStatus;

use td_test_compositor::{field, Compositor, Controls, OUTPUT_HEIGHT, OUTPUT_WIDTH};

const KEY_LEFTCTRL: u32 = 29;
const KEY_Q: u32 = 16;
const KEY_ESC: u32 = 1;

/// Whether the layout lists the toplevel `window` at all.
fn listed(compositor: &Compositor, window: &str) -> bool {
    let layout = compositor.request("layout", 65536);
    let text = std::str::from_utf8(&layout).unwrap();
    text.lines()
        .filter_map(|line| line.strip_prefix("window "))
        .any(|line| field(line, "id=") == Some(window))
}

/// td-pass launched as an ordinary Wayland client, its home and runtime
/// inside `directory`; with `vault`, the test vault's case directory.
struct Pass {
    child: Child,
    log: PathBuf,
    vault: Option<PathBuf>,
}

impl Pass {
    fn start(directory: &Directory, display: &Path, vault: Option<&Path>) -> Self {
        let home = directory.0.join("home");
        std::fs::create_dir(&home).unwrap();
        let log = directory.0.join("stderr");
        let mut command = Command::new(env!("CARGO_BIN_EXE_td-pass"));
        command
            .env_clear()
            .env("WAYLAND_DISPLAY", display)
            .env("XDG_RUNTIME_DIR", &directory.0)
            .env("TMPDIR", &directory.0)
            .env("HOME", &home)
            .env("TD_UI_FACE", "bitmap")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&log).unwrap());
        if let Some(vault) = vault {
            command.env("TD_PASS_TEST_VAULT", vault);
        }
        Self {
            child: command.spawn().unwrap(),
            log,
            vault: vault.map(Path::to_path_buf),
        }
    }

    /// What td-pass said, and what its vault was asked.
    fn said(&self) -> String {
        format!(
            "stderr: {}\njournal:\n{}",
            std::fs::read_to_string(&self.log).unwrap_or_default(),
            self.journal()
        )
    }

    fn journal(&self) -> String {
        self.vault
            .as_ref()
            .and_then(|vault| std::fs::read_to_string(vault.join("journal")).ok())
            .unwrap_or_default()
    }

    fn exited(&mut self) -> Option<ExitStatus> {
        self.child.try_wait().unwrap()
    }

    /// Waits for td-pass to exit, which it does for `what`.
    #[cfg(feature = "test-vault")]
    fn exit(&mut self, what: &str) -> ExitStatus {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if let Some(status) = self.exited() {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "{what} within {TIMEOUT:?}\n{}",
                self.said()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Pass {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// `ready` is polled until it answers, each poll spaced so the client
/// it waits on has the CPU; past `TIMEOUT` the wait fails with `what`.
fn wait<T>(pass: &Pass, what: &str, mut ready: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if let Some(value) = ready() {
            return value;
        }
        assert!(
            Instant::now() < deadline,
            "{what} within {TIMEOUT:?}\n{}",
            pass.said()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The shipped window, with the shipped backend: it maps, takes the
/// keyboard and closes on Ctrl+Q through a real seat, whatever this
/// host lets its vault do; on td it refuses with td mode's reason.
#[test]
#[ignore = "ready supplies the disposable native compositor"]
fn the_window_maps_takes_the_keyboard_and_closes_on_ctrl_q() {
    let compositor_directory = Directory::new("td-pass-process");
    let mut compositor = Compositor::start(
        &compositor_directory.0,
        Controls {
            capture: true,
            clipboard: true,
        },
    );
    let client = Directory::new("td-pass-process");
    let mut pass = Pass::start(&client, &compositor.display(), None);
    let deadline = Instant::now() + TIMEOUT;
    let place = loop {
        if let Some(place) = compositor.focused_placement("td-pass") {
            break place;
        }
        if let Some(status) = pass.exited() {
            // On td the window refuses before it maps, until td mode is
            // built; nothing else may end it.
            assert!(!status.success());
            assert_eq!(
                std::fs::read_to_string(&pass.log).unwrap(),
                "td-pass: td-pass needs td's vault service on td, which this build does not reach\n"
            );
            drop(pass);
            compositor.stop();
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the td-pass window maps with the keyboard within {TIMEOUT:?}\n{}",
            pass.said()
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(place.width > 0 && place.x + place.width <= OUTPUT_WIDTH);
    assert!(place.height > 0 && place.y + place.height <= OUTPUT_HEIGHT);
    // Where swap is on storage the window first asks whether to open over
    // it, and the question takes Ctrl+Q: Escape declines it, and is
    // nothing to the locked view.
    let deadline = Instant::now() + TIMEOUT;
    let mut again = Instant::now();
    let status = loop {
        if let Some(status) = pass.exited() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "Ctrl+Q closes the window within {TIMEOUT:?}\n{}",
            pass.said()
        );
        if Instant::now() >= again {
            compositor.chord(&[], KEY_ESC);
            compositor.chord(&[KEY_LEFTCTRL], KEY_Q);
            again = Instant::now() + Duration::from_millis(500);
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(status.success(), "{}", pass.said());
    wait(&pass, "the closed window leaves the layout", || {
        (!listed(&compositor, &place.window)).then_some(())
    });
    drop(pass);
    compositor.stop();
}

/// The window over the test vault: entry selection, editing and undo,
/// copy and paste through the compositor's clipboard, saves that fail,
/// meet a stale revision or are held in flight, the questions closing
/// and locking ask, and what a lock leaves on the screen, in the
/// clipboard and in the frames the window keeps.
#[cfg(feature = "test-vault")]
mod fixture {
    use super::*;

    use std::collections::BTreeSet;
    use td_test_compositor::Placement;

    impl Pass {
        /// The contents of every frame file td-pass holds, through its
        /// descriptors: td-ui's pool files are unlinked when made.
        fn frames(&self) -> Vec<Vec<u8>> {
            let descriptors = PathBuf::from(format!("/proc/{}/fd", self.child.id()));
            let mut frames = Vec::new();
            for entry in std::fs::read_dir(&descriptors).unwrap() {
                let path = entry.unwrap().path();
                let Ok(target) = std::fs::read_link(&path) else {
                    continue;
                };
                if !target.to_string_lossy().contains("/.td-ui-shm-") {
                    continue;
                }
                if let Ok(bytes) = std::fs::read(&path) {
                    frames.push(bytes);
                }
            }
            frames
        }
    }

    const KEY_1: u32 = 2;
    const KEY_2: u32 = 3;
    const KEY_3: u32 = 4;
    const KEY_4: u32 = 5;
    const KEY_TAB: u32 = 15;
    const KEY_ENTER: u32 = 28;
    const KEY_A: u32 = 30;
    const KEY_S: u32 = 31;
    const KEY_L: u32 = 38;
    const KEY_Z: u32 = 44;
    const KEY_X: u32 = 45;
    const KEY_C: u32 = 46;
    const KEY_V: u32 = 47;
    const KEY_END: u32 = 107;
    const KEY_DOWN: u32 = 108;

    /// One case's compositor, window and vault. Fields drop in order:
    /// the window before the compositor it is a client of.
    struct Case {
        pass: Pass,
        compositor: Compositor,
        place: Placement,
        vault: Directory,
        _client: Directory,
        _session: Directory,
    }

    impl Case {
        /// td-pass over a fresh test vault, mapped with the keyboard and
        /// settled on its locked view, which is returned.
        fn start() -> (Self, Vec<u8>) {
            let case = Self::launch(&[]);
            case.wait_line("open", 1);
            let locked = case.settle();
            (case, locked)
        }

        /// td-pass over a fresh test vault whose one-shot `controls` are
        /// set before it opens, mapped with the keyboard.
        fn launch(controls: &[&str]) -> Self {
            let session = Directory::new("td-pass-process");
            let compositor = Compositor::start(
                &session.0,
                Controls {
                    capture: true,
                    clipboard: true,
                },
            );
            let client = Directory::new("td-pass-process");
            let vault = Directory::new("td-pass-process");
            for control in controls {
                std::fs::write(vault.0.join(control), b"").unwrap();
            }
            let pass = Pass::start(&client, &compositor.display(), Some(&vault.0));
            let place = wait(&pass, "the td-pass window maps with the keyboard", || {
                compositor.focused_placement("td-pass")
            });
            Self {
                pass,
                compositor,
                place,
                vault,
                _client: client,
                _session: session,
            }
        }

        fn tap(&mut self, key: u32) {
            self.compositor.chord(&[], key);
        }

        fn ctrl(&mut self, key: u32) {
            self.compositor.chord(&[KEY_LEFTCTRL], key);
        }

        fn lines(&self) -> Vec<String> {
            self.pass.journal().lines().map(str::to_owned).collect()
        }

        /// Waits until the journal holds `line` `count` times.
        fn wait_line(&self, line: &str, count: usize) {
            wait(&self.pass, &format!("the vault journals {line:?}"), || {
                (self.lines().iter().filter(|l| *l == line).count() >= count).then_some(())
            });
        }

        /// Sets the test vault's one-shot control `name`.
        fn control(&self, name: &str) {
            std::fs::write(self.vault.0.join(name), b"").unwrap();
        }

        /// Waits until the window has drawn what the vault last answered.
        /// The window takes the vault's replies before it paints, and a
        /// reply is sent right after it is journaled, so the second frame
        /// committed after this starts shows it; a window with nothing
        /// blinking commits nothing more, and 300 ms without a commit
        /// shows it has drawn its last.
        fn rest(&self) {
            let deadline = Instant::now() + TIMEOUT;
            let first = self.compositor.observe(&self.place.window).commit;
            let mut last = first;
            let mut since = Instant::now();
            loop {
                assert!(
                    Instant::now() < deadline,
                    "the window draws nothing settled"
                );
                std::thread::sleep(Duration::from_millis(20));
                let now = self.compositor.observe(&self.place.window);
                if !now.current {
                    continue;
                }
                if now.commit >= first + 2 {
                    return;
                }
                if now.commit != last {
                    last = now.commit;
                    since = Instant::now();
                } else if since.elapsed() >= Duration::from_millis(300) {
                    return;
                }
            }
        }

        /// The tile the window rests on.
        fn settle(&self) -> Vec<u8> {
            self.rest();
            self.compositor.tile(&self.place)
        }

        /// Waits until the journal holds `line` `count` times and the
        /// window rests on the answer.
        fn after(&self, line: &str, count: usize) {
            self.wait_line(line, count);
            self.rest();
        }

        fn count(&self, line: &str) -> usize {
            self.lines().iter().filter(|l| *l == line).count()
        }

        /// Waits until the window shows `tile`.
        fn wait_tile(&self, tile: &[u8], what: &str) {
            wait(&self.pass, what, || {
                (self.compositor.tile(&self.place) == tile).then_some(())
            });
        }

        /// Return on the key, Return at its presence prompt, then its PIN.
        /// A Return the window takes before the prompt shows is ignored
        /// while it unlocks, as one on an empty PIN is, so the presence
        /// prompt's is repeated until the PIN is asked for.
        fn unlock(&mut self) {
            let unlocked = self.count("unlocked");
            let asked = self.count("ask unlock None");
            let pins = self.count("ask unlock Some(Authorize)");
            self.tap(KEY_ENTER);
            self.wait_line("ask unlock None", asked + 1);
            let deadline = Instant::now() + TIMEOUT;
            while self.count("ask unlock Some(Authorize)") == pins {
                assert!(Instant::now() < deadline, "{}", self.pass.said());
                self.tap(KEY_ENTER);
                std::thread::sleep(Duration::from_millis(200));
            }
            self.rest();
            for key in [KEY_1, KEY_2, KEY_3, KEY_4] {
                self.tap(key);
            }
            self.tap(KEY_ENTER);
            self.after("unlocked", unlocked + 1);
        }

        /// Every distinct frame the window holds over a second and a
        /// half, so a blinking caret's frames are all among them.
        fn sampled(&self) -> BTreeSet<Vec<u8>> {
            let mut seen = BTreeSet::new();
            for _ in 0..15 {
                seen.extend(retained(self.pass.frames()));
                std::thread::sleep(Duration::from_millis(100));
            }
            seen
        }

        /// From the search field, the list's first entry, read into the
        /// pane, which then takes the keyboard.
        fn open_alpha(&mut self) {
            let reads = self.count("read \"Alpha\"");
            self.tap(KEY_ENTER);
            self.tap(KEY_DOWN);
            self.after("read \"Alpha\"", reads + 1);
            self.tap(KEY_ENTER);
        }

        /// The clipboard hold's reply, with its session and window
        /// checked, as `hold state`.
        fn hold(&self, verb: &str, operand: &str) -> String {
            let reply = self.compositor.clipboard(verb, operand);
            let prefix = format!(
                "ok\ntd-clipboard-v1 session={} hold=",
                self.compositor.session()
            );
            let Some(rest) = reply.strip_prefix(&prefix) else {
                return reply;
            };
            let (hold, rest) = rest.split_once(' ').unwrap();
            let state = rest
                .strip_prefix(&format!("window={} state=", self.place.window))
                .and_then(|state| state.strip_suffix('\n'))
                .unwrap();
            format!("{hold} {state}")
        }

        /// Ctrl+Q with nothing unsaved: the window locks and exits.
        fn close(mut self) {
            self.ctrl(KEY_Q);
            let status = self.pass.exit("Ctrl+Q closes the window");
            assert!(status.success(), "{}", self.pass.said());
            let Self {
                pass,
                mut compositor,
                ..
            } = self;
            drop(pass);
            compositor.stop();
        }
    }

    /// A frame is retained content when it is not all zero.
    fn retained(frames: Vec<Vec<u8>>) -> BTreeSet<Vec<u8>> {
        frames
            .into_iter()
            .filter(|frame| frame.iter().any(|byte| *byte != 0))
            .collect()
    }

    /// The first entry is selected and read; select all and copy offers
    /// it, which the compositor sees as the window's selection; the paste
    /// at its end is held by the compositor and then released to the
    /// window, which reads it from itself; a typed letter is undone; the
    /// save carries exactly the doubled text against the revision read.
    /// Locking then shows the locked view the window started on,
    /// withdraws the selection and leaves no frame shown while unlocked
    /// in the window's frame files.
    #[test]
    #[ignore = "ready supplies the disposable native compositor"]
    fn select_edit_undo_copy_paste_save_and_lock() {
        let (mut case, locked) = Case::start();
        let before = case.sampled();
        case.unlock();
        case.open_alpha();

        case.ctrl(KEY_A);
        case.ctrl(KEY_C);
        let window = case.place.window.clone();
        let armed = wait(&case.pass, "the copy is the window's selection", || {
            let reply = case.hold("arm", &window);
            reply.ends_with(" armed").then_some(reply)
        });
        let hold = armed.split_once(' ').unwrap().0.to_owned();
        case.ctrl(KEY_END);
        case.ctrl(KEY_V);
        wait(&case.pass, "the paste's transfer is held", || {
            (case.hold("status", &hold) == format!("{hold} held")).then_some(())
        });
        assert_eq!(case.hold("release", &hold), format!("{hold} released"));
        case.rest();
        case.tap(KEY_Z);
        case.ctrl(KEY_Z);
        let edits = || {
            case.lines()
                .iter()
                .filter(|l| l.starts_with("edit "))
                .count()
        };
        assert_eq!(edits(), 0);
        // The unlock authorized the session: the save asks nothing.
        case.ctrl(KEY_S);
        case.after("committed 1 Some(2)", 1);
        let lines = case.lines();
        let unlocked = lines.iter().rposition(|l| l == "unlocked").unwrap();
        let after = lines.get(unlocked..).unwrap();
        let committed = after
            .iter()
            .position(|l| l == "committed 1 Some(2)")
            .unwrap();
        let asked = after.get(..committed).unwrap();
        assert!(!asked.iter().any(|l| l.starts_with("ask ")), "{lines:?}");
        let saved: Vec<String> = case
            .lines()
            .into_iter()
            .filter(|l| l.starts_with("edit "))
            .collect();
        assert_eq!(
            saved,
            [r#"edit 1 1 "Alpha" "alpha one\nalpha two\nalpha one\nalpha two\n""#]
        );

        // What the window holds unlocked: every frame it keeps while at
        // rest, less those it kept before any entry was shown.
        let unlocked: BTreeSet<Vec<u8>> = case.sampled().difference(&before).cloned().collect();
        assert!(!unlocked.is_empty());
        case.ctrl(KEY_L);
        case.after("lock", 1);
        case.wait_tile(&locked, "the lock shows the locked view it started on");
        // One arm, once the locked view shows: the window withdrew its
        // selection when it locked, so there is none to hold.
        assert_eq!(
            case.hold("arm", &window),
            "unavailable clipboard has no client selection\n"
        );
        wait(&case.pass, "no frame shown unlocked is kept", || {
            retained(case.pass.frames())
                .is_disjoint(&unlocked)
                .then_some(())
        });
        case.close();
    }

    /// A refused save and a stale one keep the edit; closing asks, Save
    /// meets the stale revision again and keeps the window, and Discard
    /// closes it with nothing saved.
    #[test]
    #[ignore = "ready supplies the disposable native compositor"]
    fn failed_and_stale_saves_and_the_choices_closing_asks() {
        let (mut case, _) = Case::start();
        case.unlock();
        case.open_alpha();
        case.tap(KEY_X);
        let edit = r#"edit 1 1 "Alpha" "xalpha one\nalpha two\n""#;

        case.control("fail");
        case.ctrl(KEY_S);
        case.after("the test vault refused this save", 1);
        case.control("elsewhere");
        case.ctrl(KEY_S);
        let stale = "the entry changed since it was opened; nothing was saved";
        case.after(stale, 1);
        case.wait_line(edit, 2);
        case.wait_line("saved elsewhere 2", 1);

        // Cancel, Discard, Save: focus starts on Cancel.
        case.ctrl(KEY_Q);
        case.tap(KEY_TAB);
        case.tap(KEY_TAB);
        case.tap(KEY_ENTER);
        case.after(stale, 2);
        case.wait_line(edit, 3);
        std::thread::sleep(Duration::from_millis(200));
        assert!(case.pass.exited().is_none(), "{}", case.pass.said());

        case.ctrl(KEY_Q);
        case.tap(KEY_TAB);
        case.tap(KEY_ENTER);
        let status = case.pass.exit("Discard closes the window");
        assert!(status.success(), "{}", case.pass.said());
        let lines = case.lines();
        assert!(
            !lines.iter().any(|l| l.starts_with("committed")),
            "{lines:?}"
        );
        assert_eq!(lines.iter().filter(|l| l.starts_with("edit ")).count(), 3);
        let Case {
            pass,
            mut compositor,
            ..
        } = case;
        drop(pass);
        compositor.stop();
    }

    /// A save held in flight: locking asks only to discard the edit,
    /// cancels the save and locks, and nothing is saved; the vault then
    /// unlocks again.
    #[test]
    #[ignore = "ready supplies the disposable native compositor"]
    fn locking_while_a_save_is_in_flight_cancels_it() {
        let (mut case, locked) = Case::start();
        case.unlock();
        case.open_alpha();
        case.tap(KEY_X);
        case.control("hold");
        case.ctrl(KEY_S);
        case.after("held", 1);

        // The lock's question opens as Ctrl+L is read, so the keys after
        // it reach the question: Cancel, Discard, focus on Cancel.
        case.ctrl(KEY_L);
        case.tap(KEY_TAB);
        case.tap(KEY_ENTER);
        case.wait_line("lock", 1);
        let lines = case.lines();
        let at = |line: &str| lines.iter().position(|l| l == line).unwrap();
        assert!(at("held") < at("cancelled") && at("cancelled") < at("lock"));
        assert!(
            !lines.iter().any(|l| l.starts_with("committed")),
            "{lines:?}"
        );
        case.wait_tile(&locked, "the lock shows the locked view it started on");

        case.unlock();
        case.ctrl(KEY_L);
        case.after("lock", 2);
        case.close();
    }

    /// Swap on storage: the window asks before anything opens, Cancel
    /// first. Cancel keeps the vault closed and Return asks again; Open
    /// anyway opens it, and the notebook then unlocks as ever.
    #[test]
    #[ignore = "ready supplies the disposable native compositor"]
    fn swap_on_storage_opens_only_once_accepted() {
        let mut case = Case::launch(&["swap"]);
        case.after("open: swap", 1);
        let asked = case.compositor.tile(&case.place);
        // Cancel, Open anyway: focus starts on Cancel.
        case.tap(KEY_ENTER);
        case.rest();
        assert_ne!(case.compositor.tile(&case.place), asked);
        assert!(!case
            .lines()
            .iter()
            .any(|l| l.starts_with("open: swap accepted")));
        case.tap(KEY_ENTER);
        case.wait_tile(&asked, "Return asks again");
        case.tap(KEY_TAB);
        case.tap(KEY_ENTER);
        case.after("open: swap accepted", 1);
        case.unlock();
        assert_eq!(
            case.lines(),
            [
                "open: swap",
                "open: swap accepted",
                "ask unlock None",
                "ask unlock Some(Authorize)",
                "unlocked"
            ]
        );
        case.ctrl(KEY_L);
        case.after("lock", 1);
        case.close();
    }
}
