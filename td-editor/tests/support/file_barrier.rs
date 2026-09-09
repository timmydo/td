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
                assert!(matches!(
                    kind,
                    "open" | "dictionary" | "save" | "reload" | "rename" | "queued-save"
                ));
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
fn admitted_rename_preserves_edits_focus_and_duplicate_directory_views() {
    let compositor_directory = Directory::new();
    let directory = Directory::new();
    let mut compositor = Compositor::start(&compositor_directory);
    let mut barrier = Barrier::start(&directory);
    let root = directory.0.join("browse");
    std::fs::create_dir(&root).unwrap();
    let file = root.join("draft");
    let destination = root.join("renamed");
    let dictionary = directory.0.join("dictionary");
    std::fs::write(&file, b"disk").unwrap();
    std::fs::write(&dictionary, b"disk\n").unwrap();
    let mut editor = EditorProcess::start_with_barrier(
        &directory,
        &compositor.directory.join("wayland-0"),
        &file,
        &dictionary,
        "emacs",
        Some(&barrier.path),
    );
    editor.wait_keyboard("emacs");
    let window = compositor.window();
    assert_eq!(compositor.request("fullscreen", 1024), b"ok\n");
    editor.wait_field("state", "window", "800,576,1");
    editor.ok("insert\t1\t0\t0\t0\t61");
    for _ in 0..2 {
        editor.job(&format!(
            "open\t{}",
            td_editor::control::hex(root.as_os_str().as_encoded_bytes())
        ));
    }
    editor.ok("select-tab\t2\t0");
    compositor.chord(Some(KEY_LEFT_SHIFT), 19);
    editor.wait_field("prompt-state", "prompt", "path-rename");
    let state = editor.ok("state");
    let dialog = field(&state, "dialog").unwrap().split(',').next().unwrap();
    barrier.arm("rename");
    let started = Instant::now();
    let response = editor
        .request(&format!(
            "dialog-answer\t{dialog}\t2\t0\tpath\t72656e616d6564"
        ))
        .unwrap();
    let job = response.strip_prefix("pending\t").unwrap();
    let held = barrier.held();
    let state = editor.ok("state");
    assert!(state
        .split('\t')
        .any(|field| field == format!("job={job},rename,2,0,0,pending,-")));
    editor.ok("select-tab\t1\t1");
    editor.ok("insert\t1\t1\t1\t1\t62");
    editor.wait_tab(2, "abdisk");
    compositor.chord(Some(KEY_LEFT_CTRL), KEY_SPACE);
    compositor.chord(Some(KEY_LEFT_CTRL), 45); // C-x prefix, with an active mark.
    editor.wait_field("state", "prefix", "1");
    let state = editor.ok("state");
    assert_eq!(field(&state, "line-numbers"), Some("1"));
    let view = field(&state, "view").unwrap().to_owned();
    assert_eq!(std::fs::read(&file).unwrap(), b"disk");
    assert!(!destination.exists());
    assert!(started.elapsed() < Duration::from_secs(4));
    let before = compositor.observe(&window);
    barrier.release(held);
    assert_eq!(
        editor.wait_job(job),
        format!("job={job},rename,2,0,0,complete,-")
    );
    editor.wait_field("state", "active", "1");
    editor.wait_field("state", "prefix", "1");
    editor.wait_field("state", "view", &view);
    editor.wait_field("state", "tab", "1,2,1,6,2,2,0,72,0,lf");
    wait_directory_rows(&mut editor, 2, 1, &["renamed"]);
    wait_directory_rows(&mut editor, 3, 1, &["renamed"]);
    // The default two-digit gutter plus gap moves text 24px right.
    compositor.rendered_tab_text_at(&mut editor, &window, (1, 2, 32), before, "abdisk", 2);
    compositor.chord(Some(KEY_LEFT_CTRL), KEY_G);
    assert!(!file.exists());
    assert_eq!(std::fs::read(&destination).unwrap(), b"disk");
    editor.job("save\t1\t2");
    assert_eq!(std::fs::read(&destination).unwrap(), b"abdisk");
    assert!(!file.exists());
    editor.quit();
    barrier.finish();
    compositor.stop();
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

#[test]
#[ignore = "ready builds the isolated test-file-barrier editor"]
fn admitted_save_survives_unread_reply_edits_and_tab_switch() {
    admitted_save(false);
}

#[test]
#[ignore = "ready builds the isolated test-file-barrier editor"]
fn admitted_save_as_survives_unread_reply_edits_and_tab_switch() {
    admitted_save(true);
}

#[test]
#[ignore = "ready builds the isolated test-file-barrier editor"]
fn queued_save_rejects_edit_undo_before_snapshot_handoff() {
    queued_save(false);
}

#[test]
#[ignore = "ready builds the isolated test-file-barrier editor"]
fn queued_save_as_rejects_edit_undo_before_snapshot_handoff() {
    queued_save(true);
}

fn queued_save(save_as: bool) {
    let compositor_directory = Directory::new();
    let directory = Directory::new();
    let mut compositor = Compositor::start(&compositor_directory);
    let mut barrier = Barrier::start(&directory);
    let file = directory.0.join("draft");
    let destination = directory.0.join("saved copy");
    let dictionary = directory.0.join("dictionary");
    std::fs::write(&file, b"disk").unwrap();
    std::fs::write(&dictionary, b"disk\n").unwrap();
    let display = compositor.directory.join("wayland-0");
    let mut editor = EditorProcess::start_with_barriers(
        &directory,
        &display,
        &file,
        &dictionary,
        "windows",
        None,
        Some(&barrier.path),
    );
    editor.legacy_keyboard("windows");
    let window = compositor.window();
    assert_eq!(compositor.request("fullscreen", 1024), b"ok\n");
    editor.wait_field("state", "window", "800,576,1");
    editor.rendered_at(800, 576);
    editor.ok("insert\t1\t0\t0\t0\t61");
    let kind = if save_as { "save-as" } else { "save" };
    let request = if save_as {
        format!(
            "save-as\t1\t1\t{}",
            td_editor::control::hex(destination.as_os_str().as_encoded_bytes())
        )
    } else {
        "save\t1\t1".into()
    };
    barrier.arm("queued-save");
    let started = Instant::now();
    assert_eq!(editor.request(&request).unwrap(), "pending\t1");
    let held = barrier.held();
    editor.wait_field("state", "job", &format!("1,{kind},1,1,0,pending,-"));
    editor.wait_field("state", "native", "1,1,1,0");
    editor.ok("insert\t1\t1\t1\t1\t62");
    editor.ok("undo\t1\t2");
    // Identical bytes must not make a revision-stale queued Save valid again.
    editor.wait_tab(3, "adisk");
    editor.wait_field("state", "tab", "1,3,1,5,1,1,0,72,0,lf");
    editor.wait_field("state", "native", "1,1,1,0");
    assert_eq!(std::fs::read(&file).unwrap(), b"disk");
    assert!(!destination.exists());
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(4),
        "queued evidence: {elapsed:?}"
    );
    let before = compositor.observe(&window);
    barrier.release(held);
    assert_eq!(
        editor.wait_job_outcome("1", ",error,stale-revision"),
        format!("job=1,{kind},1,1,0,error,stale-revision")
    );
    editor.wait_field("state", "native", "1,1,0,0");
    editor.wait_field("state", "dialog", "-");
    editor.wait_field("state", "dialog-last", "0");
    editor.wait_field("state", "tab", "1,3,1,5,1,1,0,72,0,lf");
    compositor.rendered_text(&mut editor, &window, 3, before, "adisk", 1);
    assert_eq!(std::fs::read(&file).unwrap(), b"disk");
    assert!(!destination.exists());
    // The consumed gate can admit another revision; the old association remains.
    assert_eq!(editor.job("save\t1\t3"), "job=2,save,1,3,0,complete,-");
    editor.wait_field("state", "tab", "1,3,0,5,1,1,0,72,0,lf");
    assert_eq!(std::fs::read(&file).unwrap(), b"adisk");
    assert!(!destination.exists());
    editor.quit();
    barrier.finish();
    compositor.stop();
}

#[test]
#[ignore = "ready builds the isolated test-file-barrier editor"]
fn save_as_destination_created_after_handoff_is_not_overwritten() {
    let compositor_directory = Directory::new();
    let directory = Directory::new();
    let mut compositor = Compositor::start(&compositor_directory);
    let mut barrier = Barrier::start(&directory);
    let file = directory.0.join("draft");
    let destination = directory.0.join("contested copy");
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
    assert!(!destination.exists());
    barrier.arm("save");
    let started = Instant::now();
    let response = editor
        .request(&format!(
            "save-as\t1\t1\t{}",
            td_editor::control::hex(destination.as_os_str().as_encoded_bytes())
        ))
        .unwrap();
    assert_eq!(response, "pending\t1");
    let held = barrier.held();
    editor.wait_field("state", "job", "1,save-as,1,1,0,pending,-");
    editor.wait_field("state", "native", "1,1,1,0");
    // Creation is ordered after handoff but before any worker filesystem I/O.
    let mut external = std::fs::File::create_new(&destination).unwrap();
    external.write_all(b"external").unwrap();
    drop(external);
    editor.ok("insert\t1\t1\t1\t1\t62");
    editor.wait_tab(2, "abdisk");
    editor.wait_field("state", "tab", "1,2,1,6,2,2,0,72,0,lf");
    assert_eq!(std::fs::read(&file).unwrap(), b"disk");
    assert_eq!(std::fs::read(&destination).unwrap(), b"external");
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(4),
        "destination-race evidence exceeded four seconds: {elapsed:?}"
    );
    let before = compositor.observe(&window);
    barrier.release(held);
    assert_eq!(
        editor.wait_job_outcome("1", ",error,unavailable"),
        "job=1,save-as,1,1,0,error,unavailable"
    );
    editor.wait_field("state", "native", "1,1,0,0");
    editor.wait_field("state", "dialog", "-");
    editor.wait_field("state", "dialog-last", "0");
    editor.wait_field("state", "tab", "1,2,1,6,2,2,0,72,0,lf");
    editor.wait_tab(2, "abdisk");
    compositor.rendered_text(&mut editor, &window, 2, before, "abdisk", 2);
    assert_eq!(std::fs::read(&file).unwrap(), b"disk");
    assert_eq!(std::fs::read(&destination).unwrap(), b"external");
    // A failed Save As must not adopt the contested destination or baseline.
    assert_eq!(editor.job("save\t1\t2"), "job=2,save,1,2,0,complete,-");
    editor.wait_field("state", "tab", "1,2,0,6,2,2,0,72,0,lf");
    assert_eq!(std::fs::read(&file).unwrap(), b"abdisk");
    assert_eq!(std::fs::read(&destination).unwrap(), b"external");
    editor.quit();
    barrier.finish();
    compositor.stop();
}

#[test]
#[ignore = "ready builds the isolated test-file-barrier editor"]
fn barrier_disconnect_fails_before_write_and_leaves_document_editable() {
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
    barrier.arm("save");
    let started = Instant::now();
    assert_eq!(editor.request("save\t1\t1").unwrap(), "pending\t1");
    barrier.held();
    editor.wait_field("state", "job", "1,save,1,1,0,pending,-");
    editor.wait_field("state", "native", "1,1,1,0");
    editor.ok("insert\t1\t1\t1\t1\t62");
    editor.wait_tab(2, "abdisk");
    assert_eq!(std::fs::read(&file).unwrap(), b"disk");
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(4),
        "held evidence: {elapsed:?}"
    );
    let before = compositor.observe(&window);
    // Drop the release sender and join: the held peer closes without continue.
    barrier.finish();
    assert_eq!(
        editor.wait_job_outcome("1", ",error,unavailable"),
        "job=1,save,1,1,0,error,unavailable"
    );
    editor.wait_field("state", "native", "1,1,0,0");
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(4),
        "disconnect completion exceeded four seconds: {elapsed:?}"
    );
    editor.wait_field("state", "dialog", "-");
    editor.wait_field("state", "dialog-last", "0");
    editor.wait_field("state", "tab", "1,2,1,6,2,2,0,72,0,lf");
    editor.wait_tab(2, "abdisk");
    compositor.rendered_text(&mut editor, &window, 2, before, "abdisk", 2);
    assert_eq!(std::fs::read(&file).unwrap(), b"disk");
    // Another Save must fail without writing or leaving a job pending.
    assert_eq!(editor.request("save\t1\t2").unwrap(), "pending\t2");
    assert_eq!(
        editor.wait_job_outcome("2", ",error,unavailable"),
        "job=2,save,1,2,0,error,unavailable"
    );
    editor.wait_field("state", "native", "1,1,0,0");
    let before_edit = compositor.observe(&window);
    editor.ok("insert\t1\t2\t2\t2\t63");
    editor.wait_tab(3, "abcdisk");
    editor.wait_field("state", "tab", "1,3,1,7,3,3,0,72,0,lf");
    compositor.rendered_text(&mut editor, &window, 3, before_edit, "abcdisk", 3);
    assert_eq!(std::fs::read(&file).unwrap(), b"disk");
    // Keep a clean tab so discard has a live reply before ordinary quit.
    assert_eq!(editor.ok("new"), "2");
    editor.ok("select-tab\t1\t3");
    editor.ok("close-tab\t1\t3");
    let state = editor.ok("state");
    let dialog = field(&state, "dialog")
        .expect("close dialog field")
        .split(',')
        .next()
        .unwrap();
    assert_ne!(dialog, "-", "{state}");
    editor.ok(&format!("dialog-answer\t{dialog}\t1\t3\tdiscard"));
    editor.wait_field("state", "active", "2");
    editor.quit();
    compositor.stop();
}

fn admitted_save(save_as: bool) {
    let compositor_directory = Directory::new();
    let directory = Directory::new();
    let mut compositor = Compositor::start(&compositor_directory);
    let mut barrier = Barrier::start(&directory);
    let file = directory.0.join("draft");
    let destination = directory.0.join("saved copy");
    let refused = directory.0.join("refused");
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
    let kind = if save_as { "save-as" } else { "save" };
    let request = if save_as {
        let path = td_editor::control::hex(destination.as_os_str().as_encoded_bytes());
        format!("save-as\t1\t1\t{path}")
    } else {
        "save\t1\t1".into()
    };
    barrier.arm("save"); // Both Save and Save As use the worker's save checkpoint.
    let started = Instant::now();
    let mut unread = UnixStream::connect(&editor.socket).unwrap();
    editor.next += 1;
    let payload = format!("1\t{}\t{request}", editor.next);
    let frame = td_editor::control::frame(payload.as_bytes()).unwrap();
    write_until(&mut unread, &frame, Instant::now() + TIMEOUT).unwrap();
    let held = barrier.held(); // Worker receipt proves admission and snapshot handoff.
    drop(unread); // Deliberately never read the pending reply and never retry it.
    editor.wait_field("state", "job", &format!("1,{kind},1,1,0,pending,-"));
    editor.wait_field("state", "job-last", "1");
    editor.wait_field("state", "native", "1,1,1,0");
    let refused_path = td_editor::control::hex(refused.as_os_str().as_encoded_bytes());
    for request in [
        "save\t1\t1".into(),
        format!("save-as\t1\t1\t{refused_path}"),
        format!("open\t{refused_path}"),
        "close-tab\t1\t1".into(),
    ] {
        let response = editor.request(&request).unwrap();
        assert!(response.starts_with("error\tunavailable\t"), "{response}");
    }
    editor.wait_field("state", "job-last", "1");
    editor.wait_field("state", "dialog-last", "0");
    editor.wait_field("state", "dialog", "-");
    assert!(!refused.exists());
    editor.ok("insert\t1\t1\t1\t1\t62");
    assert_eq!(editor.ok("new"), "2");
    editor.ok("insert\t2\t0\t0\t0\t6672657368");
    editor.wait_field("state", "active", "2");
    editor.wait_field("state", "tab", "1,2,1,6,2,2,0,72,0,lf");
    let state = editor.ok("state");
    assert!(
        state
            .split('\t')
            .any(|field| field == "tab=2,1,1,5,5,5,0,72,0,lf"),
        "{state}"
    );
    assert_eq!(editor.ok("text\t1\t2\t0\t100"), "6\t61626469736b");
    assert_eq!(editor.ok("text\t2\t1\t0\t100"), "5\t6672657368");
    assert_eq!(std::fs::read(&file).unwrap(), b"disk");
    assert!(!destination.exists());
    editor.wait_field("state", "native", "1,1,1,0");
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(4),
        "admitted-save evidence exceeded four seconds: {elapsed:?}"
    );
    let before = compositor.observe(&window);
    barrier.release(held);
    assert_eq!(
        editor.wait_job("1"),
        format!("job=1,{kind},1,1,0,complete,-")
    );
    editor.wait_field("state", "native", "1,1,0,0");
    editor.wait_field("state", "active", "2");
    editor.wait_field("state", "tab", "1,2,1,6,2,2,0,72,0,lf");
    let state = editor.ok("state");
    assert!(
        state
            .split('\t')
            .any(|field| field == "tab=2,1,1,5,5,5,0,72,0,lf"),
        "{state}"
    );
    compositor.rendered_tab_text(&mut editor, &window, (2, 1), before, "fresh", 5);
    let saved_path = if save_as { &destination } else { &file };
    assert_eq!(std::fs::read(saved_path).unwrap(), b"adisk");
    if save_as {
        assert_eq!(std::fs::read(&file).unwrap(), b"disk");
    }
    // The new tab must remain unnamed, without inheriting the save destination.
    let response = editor.request("save\t2\t1").unwrap();
    assert!(
        response.starts_with("error\tinvalid-argument\t"),
        "{response}"
    );
    editor.wait_field("state", "job-last", "1");
    editor.ok("select-tab\t1\t2");
    assert_eq!(editor.job("save\t1\t2"), "job=2,save,1,2,0,complete,-");
    assert_eq!(std::fs::read(saved_path).unwrap(), b"abdisk");
    if save_as {
        assert_eq!(std::fs::read(&file).unwrap(), b"disk");
    }
    editor.wait_field("state", "tab", "1,2,0,6,2,2,0,72,0,lf");
    editor.ok("select-tab\t2\t1");
    editor.ok("close-tab\t2\t1");
    let state = editor.ok("state");
    let dialog = field(&state, "dialog")
        .expect("close dialog field")
        .split(',')
        .next()
        .unwrap();
    editor.ok(&format!("dialog-answer\t{dialog}\t2\t1\tdiscard"));
    editor.wait_field("state", "active", "1");
    editor.quit();
    barrier.finish();
    compositor.stop();
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
