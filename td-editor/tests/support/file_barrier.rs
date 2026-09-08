use super::*;
use std::sync::Mutex;

struct Barrier {
    path: PathBuf,
    armed: Arc<Mutex<Option<&'static str>>>,
    held: mpsc::Receiver<u64>,
    release: Option<mpsc::Sender<u64>>,
    thread: Option<JoinHandle<()>>,
}

impl Barrier {
    fn start(directory: &Directory) -> Self {
        let path = directory.0.join("file-barrier");
        let listener = UnixListener::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let armed = Arc::new(Mutex::new(None));
        let worker_armed = armed.clone();
        let (held_tx, held) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            let deadline = Instant::now() + TIMEOUT;
            let mut stream = loop {
                let accepted = listener.accept();
                if matches!(&accepted, Err(e) if e.kind() == io::ErrorKind::WouldBlock) {
                    assert!(Instant::now() < deadline);
                    std::thread::sleep(Duration::from_millis(2));
                    continue;
                }
                break accepted.expect("barrier accept").0;
            };
            stream.set_read_timeout(Some(TIMEOUT)).unwrap();
            stream.set_write_timeout(Some(TIMEOUT)).unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut sequence = 0;
            loop {
                let mut line = Vec::new();
                let count = reader
                    .by_ref()
                    .take(65)
                    .read_until(b'\n', &mut line)
                    .unwrap();
                if count == 0 {
                    break;
                }
                assert!(count <= 64 && line.ends_with(b"\n"));
                let line = std::str::from_utf8(&line).unwrap();
                let mut fields = line.trim_end_matches('\n').split(' ');
                assert_eq!(fields.next(), Some("td-file-v1"));
                sequence += 1;
                assert_eq!(fields.next(), Some(sequence.to_string().as_str()));
                let kind = fields.next().unwrap();
                assert!(matches!(kind, "open" | "dictionary" | "save" | "reload"));
                assert_eq!(fields.next(), None);
                let hold = {
                    let mut armed = worker_armed.lock().unwrap();
                    if armed.as_ref() == Some(&kind) {
                        armed.take();
                        true
                    } else {
                        false
                    }
                };
                if hold {
                    if held_tx.send(sequence).is_err() {
                        break;
                    }
                    let Ok(release) = released.recv_timeout(TIMEOUT) else {
                        break;
                    };
                    assert_eq!(release, sequence);
                }
                stream
                    .write_all(format!("continue {sequence}\n").as_bytes())
                    .unwrap();
            }
        });
        Self {
            path,
            armed,
            held,
            release: Some(release),
            thread: Some(thread),
        }
    }
    fn arm(&self, kind: &'static str) {
        assert!(self.armed.lock().unwrap().replace(kind).is_none());
    }
    fn held(&self) -> u64 {
        self.held.recv_timeout(TIMEOUT).unwrap()
    }
    fn release(&self, id: u64) {
        self.release.as_ref().unwrap().send(id).unwrap();
    }
    fn finish(&mut self) {
        self.release.take();
        self.thread.take().unwrap().join().unwrap();
    }
}
impl Drop for Barrier {
    fn drop(&mut self) {
        self.release.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[test]
#[ignore = "ready builds the isolated test-file-barrier editor"]
fn cancelled_reload_keeps_edits_and_old_baseline() {
    scenario(true);
}

#[test]
#[ignore = "ready builds the isolated test-file-barrier editor"]
fn cancelled_close_save_keeps_captured_snapshot_and_later_edits() {
    scenario(false);
}

fn scenario(reload: bool) {
    let compositor_directory = Directory::new();
    let directory = Directory::new();
    let mut compositor = Compositor::start(&compositor_directory);
    let mut barrier = Barrier::start(&directory);
    let file = directory.0.join("draft");
    let dictionary = directory.0.join("dictionary");
    std::fs::write(&file, b"disk").unwrap();
    std::fs::write(&dictionary, b"disk\n").unwrap();
    let display = compositor.directory.join("wayland-0");
    let mut editor = EditorProcess::start_with_barrier(
        &directory,
        &display,
        &file,
        &dictionary,
        "windows",
        Some(&barrier.path),
    );
    editor.legacy_keyboard("windows");
    let window = compositor.window();
    assert_eq!(compositor.request("fullscreen", 1024), b"ok\n");
    editor.wait_field("state", "window", "800,576,1");
    editor.rendered_at(800, 576);
    editor.ok("insert\t1\t0\t0\t0\t61");
    if reload {
        std::fs::write(&file, b"external").unwrap();
        let reply = editor.request("save\t1\t1").unwrap();
        let id = reply.strip_prefix("pending\t").unwrap();
        editor.wait_job_outcome(id, ",error,unavailable");
    } else {
        editor.ok("close-tab\t1\t1");
    }
    let state = editor.ok("state");
    let dialog = field(&state, "dialog")
        .unwrap()
        .split(',')
        .next()
        .unwrap()
        .to_owned();
    if reload {
        editor.ok(&format!("dialog-answer\t{dialog}\t1\t1\treload"));
    }
    barrier.arm(if reload { "reload" } else { "save" });
    let started = Instant::now();
    let action = if reload { "discard-reload" } else { "save" };
    let response = editor
        .request(&format!("dialog-answer\t{dialog}\t1\t1\t{action}"))
        .unwrap();
    let job = response.strip_prefix("pending\t").unwrap();
    let held = barrier.held();
    editor.wait_field("state", "native", "1,1,1,0");
    editor.wait_tab(1, "adisk");
    editor.ok(&format!("dialog-answer\t{dialog}\t1\t1\tcancel"));
    editor.wait_field("state", "dialog", "-");
    if reload {
        editor.wait_job_outcome(job, ",cancelled,-");
    }
    editor.ok("insert\t1\t1\t1\t1\t62");
    editor.wait_field("state", "tab", "1,2,1,6,2,2,0,72,0,lf");
    editor.wait_tab(2, "abdisk");
    editor.wait_field("state", "native", "1,1,1,0");
    assert_eq!(
        std::fs::read(&file).unwrap(),
        if reload {
            b"external".as_slice()
        } else {
            b"disk"
        }
    );
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "barrier evidence exceeded its budget"
    );
    let before = compositor.observe(&window);
    barrier.release(held);
    editor.wait_field("state", "native", "1,1,0,0");
    editor.wait_job_outcome(
        job,
        if reload {
            ",cancelled,-"
        } else {
            ",complete,-"
        },
    );
    editor.wait_field("state", "tab", "1,2,1,6,2,2,0,72,0,lf");
    editor.wait_tab(2, "abdisk");
    compositor.rendered_text(&mut editor, &window, 2, before, "abdisk", 2);
    assert_eq!(
        std::fs::read(&file).unwrap(),
        if reload {
            b"external".as_slice()
        } else {
            b"adisk"
        }
    );
    if reload {
        let reply = editor.request("save\t1\t2").unwrap();
        editor.wait_job_outcome(
            reply.strip_prefix("pending\t").unwrap(),
            ",error,unavailable",
        );
        let state = editor.ok("state");
        let next = field(&state, "dialog").unwrap().split(',').next().unwrap();
        assert_ne!(next, dialog);
        editor.ok(&format!("dialog-answer\t{next}\t1\t2\tcancel"));
        let rescue = directory.0.join("rescue");
        editor.job(&format!(
            "save-as\t1\t2\t{}",
            td_editor::control::hex(rescue.as_os_str().as_encoded_bytes())
        ));
        assert_eq!(std::fs::read(rescue).unwrap(), b"abdisk");
        assert_eq!(std::fs::read(&file).unwrap(), b"external");
    } else {
        editor.job("save\t1\t2");
        assert_eq!(std::fs::read(&file).unwrap(), b"abdisk");
    }
    editor.quit();
    barrier.finish();
    compositor.stop();
}
