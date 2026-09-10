use super::*;
#[cfg(feature = "test-file-barrier")]
#[path = "file_barrier.rs"]
mod fixture;
use std::io::{BufRead, BufReader};
use std::sync::mpsc;

const FRAME_BYTES: usize = 800 * 600 * 3;
const KEY_Q: u32 = 16;
const KEY_W: u32 = 17;
const KEY_R: u32 = 19;
const KEY_Y: u32 = 21;
const KEY_G: u32 = 34;
const KEY_C: u32 = 46;
const KEY_LEFT_ALT: u32 = 56;
const KEY_SPACE: u32 = 57;
const KEY_HOME: u32 = 102;
const KEY_RIGHT: u32 = 106;
const KEY_END: u32 = 107;

fn wait_directory_rows(
    editor: &mut EditorProcess,
    tab: u64,
    revision: u64,
    names: &[&str],
) -> String {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let state = editor.ok("state");
        if state
            .split('\t')
            .any(|field| field.starts_with(&format!("tab={tab},{revision},")))
        {
            break;
        }
        assert!(Instant::now() < deadline, "directory revision: {state}");
    }
    let reply = editor.ok(&format!("text\t{tab}\t{revision}\t0\t4096"));
    let (length, hex) = reply.split_once('\t').unwrap();
    if length == "0" {
        assert_eq!(hex, "-");
        assert!(names.is_empty());
        return String::new();
    }
    let (pairs, remainder) = hex.as_bytes().as_chunks::<2>();
    assert!(remainder.is_empty());
    let text = String::from_utf8(
        pairs
            .iter()
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect(),
    )
    .unwrap();
    assert_eq!(text.len(), length.parse::<usize>().unwrap());
    assert_eq!(
        text.lines()
            .map(|line| line.split_whitespace().last().unwrap())
            .collect::<Vec<_>>(),
        names
    );
    assert!(text
        .lines()
        .all(|line| line.split_whitespace().count() == 8));
    text
}

#[test]
#[ignore = "ready supplies the disposable native compositor"]
fn native_cross_directory_copy_move_refresh_and_later_save() {
    for profile in ["windows", "emacs"] {
        let compositor_directory = Directory::new();
        let directory = Directory::new();
        let mut compositor = Compositor::start(&compositor_directory);
        let source = directory.0.join("source");
        let destination = directory.0.join("destination");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&destination).unwrap();
        let file = source.join("file");
        std::fs::write(&file, b"disk").unwrap();
        let dictionary = directory.0.join("dictionary");
        std::fs::write(&dictionary, b"disk\n").unwrap();
        let mut editor = EditorProcess::start_with_profile(
            &directory,
            &compositor.directory.join("wayland-0"),
            &file,
            &dictionary,
            profile,
        );
        editor.wait_keyboard(profile);
        let window = compositor.window();
        assert_eq!(compositor.request("fullscreen", 1024), b"ok\n");
        editor.wait_field("state", "window", "800,576,1");
        editor.ok("insert\t1\t0\t0\t0\t65646974");
        for path in [&source, &destination] {
            editor.job(&format!(
                "open\t{}",
                td_editor::control::hex(path.as_os_str().as_encoded_bytes())
            ));
        }
        editor.ok("select-tab\t2\t0");
        for (key, scope, name) in [(KEY_C, "copy", "copy"), (KEY_R, "rename", "moved")] {
            compositor.chord(Some(KEY_LEFT_SHIFT), key);
            editor.wait_field("prompt-state", "prompt", &format!("path-{scope}"));
            let state = editor.ok("state");
            let dialog = field(&state, "dialog").unwrap().split(',').next().unwrap();
            let relative = format!("../destination/{name}");
            assert!(editor
                .job(&format!(
                    "dialog-answer\t{dialog}\t2\t0\tpath\t{}",
                    td_editor::control::hex(relative.as_bytes())
                ))
                .contains(&format!(",{scope},2,0,0,complete,-")));
            assert_eq!(std::fs::read(destination.join(name)).unwrap(), b"disk");
        }
        wait_directory_rows(&mut editor, 2, 1, &[]);
        let listing = wait_directory_rows(&mut editor, 3, 2, &["copy", "moved"]);
        let before = compositor.observe(&window);
        editor.ok("select-tab\t3\t2");
        compositor.rendered_tab_text(&mut editor, &window, (3, 2), before, &listing[..10], 0);
        assert!(!file.exists());
        assert_eq!(editor.ok("text\t1\t1\t0\t100"), "8\t656469746469736b");
        editor.ok("select-tab\t1\t1");
        editor.ok("undo\t1\t1");
        editor.ok("redo\t1\t2");
        editor.job("save\t1\t3");
        assert_eq!(
            std::fs::read(destination.join("moved")).unwrap(),
            b"editdisk"
        );
        assert_eq!(std::fs::read(destination.join("copy")).unwrap(), b"disk");
        assert!(!file.exists());
        editor.quit();
        compositor.stop();
    }
}

#[test]
#[ignore = "ready supplies the disposable native compositor"]
fn native_directory_file_copy_menu_keys_and_literal_remote_completion() {
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    for profile in ["windows", "emacs"] {
        let compositor_directory = Directory::new();
        let directory = Directory::new();
        let mut compositor = Compositor::start(&compositor_directory);
        let root = directory.0.join("browse");
        std::fs::create_dir(&root).unwrap();
        let source = root.join("a-source");
        std::fs::write(&source, b"disk\0\xff").unwrap();
        std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o6751)).unwrap();
        let expected = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o751)
            .open(directory.0.join("expected-mode"))
            .unwrap()
            .metadata()
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        let document = directory.0.join("document");
        let dictionary = directory.0.join("dictionary");
        std::fs::write(&document, b"document").unwrap();
        std::fs::write(&dictionary, b"document\n").unwrap();
        let mut editor = EditorProcess::start_with_profile(
            &directory,
            &compositor.directory.join("wayland-0"),
            &document,
            &dictionary,
            profile,
        );
        editor.wait_keyboard(profile);
        let window = compositor.window();
        assert_eq!(compositor.request("fullscreen", 1024), b"ok\n");
        editor.wait_field("state", "window", "800,576,1");
        editor.job(&format!(
            "open\t{}",
            td_editor::control::hex(root.as_os_str().as_encoded_bytes())
        ));
        compositor.click(270, 32);
        editor.wait_field("state", "modal", "0,0,0,0,1,0,0,0,0");
        compositor.click(270, 300); // Directory > Copy File.
        editor.wait_field("prompt-state", "prompt", "path-copy");
        let state = editor.ok("state");
        let id = field(&state, "dialog").unwrap().split(',').next().unwrap();
        editor.ok(&format!("dialog-answer\t{id}\t2\t0\tcancel"));
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1);
        compositor.chord(Some(KEY_LEFT_SHIFT), KEY_C); // C
        editor.wait_field("prompt-state", "prompt", "path-copy");
        let state = editor.ok("state");
        let next = field(&state, "dialog").unwrap().split(',').next().unwrap();
        assert_ne!(id, next);
        assert!(editor
            .request(&format!("dialog-answer\t{id}\t2\t0\tpath\t6e2dff"))
            .unwrap()
            .starts_with("error\tinvalid-argument"));
        let before = compositor.observe(&window);
        assert!(editor
            .job(&format!("dialog-answer\t{next}\t2\t0\tpath\t6e2dff"))
            .contains(",copy,2,0,0,complete,-"));
        let path = root.join(std::ffi::OsString::from_vec(b"n-\xff".to_vec()));
        let meta = std::fs::symlink_metadata(&path).unwrap();
        assert!(meta.is_file());
        assert_eq!(std::fs::read(&path).unwrap(), b"disk\0\xff");
        assert_eq!(std::fs::read(&source).unwrap(), b"disk\0\xff");
        assert_eq!(meta.permissions().mode() & 0o7777, expected);
        let listing = wait_directory_rows(&mut editor, 2, 1, &["a-source", "n-\\xff"]);
        compositor.rendered_tab_text(&mut editor, &window, (2, 1), before, &listing[..10], 0);
        let state = editor.ok("state");
        editor.ok(&format!(
            "key\t2\t1\t{}\t43",
            field(&state, "input-generation").unwrap()
        ));
        editor.wait_field("prompt-state", "prompt", "path-copy");
        let state = editor.ok("state");
        let id = field(&state, "dialog").unwrap().split(',').next().unwrap();
        let response = editor
            .request(&format!("dialog-answer\t{id}\t2\t1\tpath\t6e2dff"))
            .unwrap();
        let job = response.strip_prefix("pending\t").unwrap();
        assert_eq!(
            editor.wait_job_outcome(job, ",error,unavailable"),
            format!("job={job},copy,2,1,0,error,unavailable")
        );
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 2);
        assert_eq!(std::fs::read(document).unwrap(), b"document");
        editor.quit();
        compositor.stop();
    }
}

#[test]
#[ignore = "ready supplies the disposable native compositor"]
fn native_directory_mkdir_menu_keys_and_literal_remote_completion() {
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::fs::PermissionsExt;
    for profile in ["windows", "emacs"] {
        let compositor_directory = Directory::new();
        let directory = Directory::new();
        let mut compositor = Compositor::start(&compositor_directory);
        let root = directory.0.join("browse");
        std::fs::create_dir(&root).unwrap();
        let document = directory.0.join("document");
        let dictionary = directory.0.join("dictionary");
        std::fs::write(&document, b"document").unwrap();
        std::fs::write(&dictionary, b"document\n").unwrap();
        let mut editor = EditorProcess::start_with_profile(
            &directory,
            &compositor.directory.join("wayland-0"),
            &document,
            &dictionary,
            profile,
        );
        editor.wait_keyboard(profile);
        let window = compositor.window();
        assert_eq!(compositor.request("fullscreen", 1024), b"ok\n");
        editor.wait_field("state", "window", "800,576,1");
        editor.job(&format!(
            "open\t{}",
            td_editor::control::hex(root.as_os_str().as_encoded_bytes())
        ));
        compositor.click(270, 32);
        editor.wait_field("state", "modal", "0,0,0,0,1,0,0,0,0");
        compositor.click(270, 276); // Directory > New Directory.
        editor.wait_field("prompt-state", "prompt", "path-mkdir");
        let state = editor.ok("state");
        let id = field(&state, "dialog").unwrap().split(',').next().unwrap();
        editor.ok(&format!("dialog-answer\t{id}\t2\t0\tcancel"));
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
        compositor.chord(Some(KEY_LEFT_SHIFT), 13); // +
        editor.wait_field("prompt-state", "prompt", "path-mkdir");
        let state = editor.ok("state");
        let next = field(&state, "dialog").unwrap().split(',').next().unwrap();
        assert_ne!(id, next);
        assert!(editor
            .request(&format!("dialog-answer\t{id}\t2\t0\tpath\t6e2dff"))
            .unwrap()
            .starts_with("error\tinvalid-argument"));
        let before = compositor.observe(&window);
        assert!(editor
            .job(&format!("dialog-answer\t{next}\t2\t0\tpath\t6e2dff"))
            .contains(",mkdir,2,0,0,complete,-"));
        let path = root.join(std::ffi::OsString::from_vec(b"n-\xff".to_vec()));
        let meta = std::fs::symlink_metadata(&path).unwrap();
        assert!(meta.is_dir());
        assert_eq!(meta.permissions().mode() & 0o077, 0);
        let listing = wait_directory_rows(&mut editor, 2, 1, &["n-\\xff/"]);
        compositor.rendered_tab_text(&mut editor, &window, (2, 1), before, &listing[..10], 0);
        let state = editor.ok("state");
        editor.ok(&format!(
            "key\t2\t1\t{}\t2b",
            field(&state, "input-generation").unwrap()
        ));
        editor.wait_field("prompt-state", "prompt", "path-mkdir");
        let state = editor.ok("state");
        let id = field(&state, "dialog").unwrap().split(',').next().unwrap();
        let response = editor
            .request(&format!("dialog-answer\t{id}\t2\t1\tpath\t6e2dff"))
            .unwrap();
        let job = response.strip_prefix("pending\t").unwrap();
        assert_eq!(
            editor.wait_job_outcome(job, ",error,unavailable"),
            format!("job={job},mkdir,2,1,0,error,unavailable")
        );
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1);
        assert_eq!(std::fs::read(document).unwrap(), b"document");
        editor.quit();
        compositor.stop();
    }
}

#[test]
#[ignore = "ready supplies the disposable native compositor"]
fn native_directory_marks_cancel_and_confirm_literal_deletion() {
    use std::os::unix::ffi::OsStringExt;
    for profile in ["windows", "emacs"] {
        let compositor_directory = Directory::new();
        let directory = Directory::new();
        let mut compositor = Compositor::start(&compositor_directory);
        let root = directory.0.join("browse");
        std::fs::create_dir(&root).unwrap();
        let victim = root.join(std::ffi::OsString::from_vec(b"a-\xff".to_vec()));
        let keep = root.join("z-keep");
        let document = directory.0.join("document");
        let dictionary = directory.0.join("dictionary");
        std::fs::write(&victim, b"victim").unwrap();
        std::fs::write(&keep, b"keep").unwrap();
        std::fs::write(&document, b"document").unwrap();
        std::fs::write(&dictionary, b"document\n").unwrap();
        let mut editor = EditorProcess::start_with_profile(
            &directory,
            &compositor.directory.join("wayland-0"),
            &document,
            &dictionary,
            profile,
        );
        editor.wait_keyboard(profile);
        let window = compositor.window();
        assert_eq!(compositor.request("fullscreen", 1024), b"ok\n");
        editor.wait_field("state", "window", "800,576,1");
        editor.job(&format!(
            "open\t{}",
            td_editor::control::hex(root.as_os_str().as_encoded_bytes())
        ));
        compositor.chord(None, 32); // d
        editor.wait_field("state", "directory-marks", "2,1");
        // Marking advances; return to the first row before unmarking.
        editor.ok("select-range\t2\t1\t0\t0");
        compositor.chord(None, 22); // u
        editor.wait_field("state", "directory-marks", "2,0");
        editor.ok("select-range\t2\t2\t0\t0");
        compositor.chord(None, 32);
        editor.wait_field("state", "directory-marks", "2,1");
        compositor.click(270, 32);
        editor.wait_field("state", "modal", "0,0,0,0,1,0,0,0,0");
        compositor.click(270, 252); // Directory > Delete Marked Entries.
        editor.wait_field("prompt-state", "prompt", "path-delete");
        let state = editor.ok("state");
        let id = field(&state, "dialog").unwrap().split(',').next().unwrap();
        editor.ok(&format!("dialog-answer\t{id}\t2\t3\tcancel"));
        assert_eq!(std::fs::read(&victim).unwrap(), b"victim");
        let state = editor.ok("state");
        editor.ok(&format!(
            "key\t2\t3\t{}\t78",
            field(&state, "input-generation").unwrap()
        ));
        editor.wait_field("prompt-state", "prompt", "path-delete");
        let state = editor.ok("state");
        let next = field(&state, "dialog").unwrap().split(',').next().unwrap();
        assert_ne!(id, next);
        assert!(
            editor
                .request(&format!("dialog-answer\t{id}\t2\t3\tpath\t44454c455445"))
                .unwrap()
                .starts_with("error\tinvalid-argument")
        );
        let before = compositor.observe(&window);
        assert!(
            editor
                .job(&format!("dialog-answer\t{next}\t2\t3\tpath\t44454c455445"))
                .contains(",delete,2,3,0,complete,-")
        );
        let listing = wait_directory_rows(&mut editor, 2, 4, &["z-keep"]);
        compositor.rendered_tab_text(&mut editor, &window, (2, 4), before, &listing[..10], 0);
        assert!(std::fs::symlink_metadata(victim).is_err());
        assert_eq!(std::fs::read(keep).unwrap(), b"keep");
        assert_eq!(std::fs::read(document).unwrap(), b"document");
        editor.quit();
        compositor.stop();
    }
}

#[test]
#[ignore = "ready supplies the disposable native compositor"]
fn native_directory_rename_keeps_dirty_file_tabs_and_remote_outcomes() {
    use std::os::unix::ffi::OsStringExt;
    for profile in ["windows", "emacs"] {
        let compositor_directory = Directory::new();
        let directory = Directory::new();
        let mut compositor = Compositor::start(&compositor_directory);
        let root = directory.0.join("browse");
        std::fs::create_dir(&root).unwrap();
        let file = root.join("old");
        std::fs::write(&file, b"body").unwrap();
        let dictionary = directory.0.join("dictionary");
        std::fs::write(&dictionary, b"body\n").unwrap();
        let mut editor = EditorProcess::start_with_profile(
            &directory,
            &compositor.directory.join("wayland-0"),
            &file,
            &dictionary,
            profile,
        );
        editor.wait_keyboard(profile);
        let window = compositor.window();
        assert_eq!(compositor.request("fullscreen", 1024), b"ok\n");
        editor.wait_field("state", "window", "800,576,1");
        editor.ok("insert\t1\t0\t0\t0\t65646974");
        editor.job(&format!(
            "open\t{}",
            td_editor::control::hex(root.as_os_str().as_encoded_bytes())
        ));
        wait_directory_rows(&mut editor, 2, 0, &["old"]);
        compositor.chord(Some(KEY_LEFT_SHIFT), 19); // R, in both profiles.
        editor.wait_field("prompt-state", "prompt", "path-rename");
        let state = editor.ok("state");
        let dialog = field(&state, "dialog")
            .unwrap()
            .split(',')
            .next()
            .unwrap()
            .to_owned();
        editor.ok(&format!("dialog-answer\t{dialog}\t2\t0\tcancel"));
        assert_eq!(std::fs::read(&file).unwrap(), b"body");
        // A delivered decoded key opens the same prompt; it cannot answer it.
        let state = editor.ok("state");
        editor.ok(&format!(
            "key\t2\t0\t{}\t52",
            field(&state, "input-generation").unwrap()
        ));
        editor.wait_field("prompt-state", "prompt", "path-rename");
        let state = editor.ok("state");
        let next = field(&state, "dialog")
            .unwrap()
            .split(',')
            .next()
            .unwrap()
            .to_owned();
        assert_ne!(dialog, next);
        assert!(editor
            .request(&format!("dialog-answer\t{dialog}\t2\t0\tpath\t6e6577"))
            .unwrap()
            .starts_with("error\tinvalid-argument"));
        let reply = editor
            .request(&format!("dialog-answer\t{next}\t2\t0\tpath\t6f6c64"))
            .unwrap();
        let job = reply.strip_prefix("pending\t").unwrap();
        assert!(editor
            .wait_job_outcome(job, ",error,unavailable")
            .contains(",rename,2,0,0,"));
        assert_eq!(std::fs::read(&file).unwrap(), b"body");
        compositor.click(270, 32);
        editor.wait_field("state", "modal", "0,0,0,0,1,0,0,0,0");
        compositor.click(270, 180); // Directory > Rename Entry.
        editor.wait_field("prompt-state", "prompt", "path-rename");
        let state = editor.ok("state");
        let dialog = field(&state, "dialog").unwrap().split(',').next().unwrap();
        let before = compositor.observe(&window);
        assert!(editor
            .job(&format!("dialog-answer\t{dialog}\t2\t0\tpath\t6e65772dff"))
            .contains(",rename,2,0,0,complete,-"));
        let listing = wait_directory_rows(&mut editor, 2, 1, &["new-\\xff"]);
        compositor.rendered_tab_text(&mut editor, &window, (2, 1), before, &listing[..10], 0);
        let destination = root.join(std::ffi::OsString::from_vec(b"new-\xff".to_vec()));
        editor.wait_field(
            "state",
            "directory-entry",
            &format!(
                "2,{}",
                td_editor::control::hex(destination.as_os_str().as_encoded_bytes())
            ),
        );
        assert!(!file.exists());
        assert_eq!(std::fs::read(&destination).unwrap(), b"body");
        assert_eq!(editor.ok("text\t1\t1\t0\t100"), "8\t65646974626f6479");
        editor.ok("select-tab\t1\t1");
        editor.ok("undo\t1\t1");
        editor.ok("redo\t1\t2");
        editor.job("save\t1\t3");
        assert_eq!(std::fs::read(&destination).unwrap(), b"editbody");
        assert!(!file.exists());
        editor.quit();
        compositor.stop();
    }
}

#[test]
#[ignore = "ready supplies the disposable native compositor"]
fn native_directory_details_sort_and_copy_selected_entry() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    for profile in ["windows", "emacs"] {
        let compositor_directory = Directory::new();
        let directory = Directory::new();
        let mut compositor = Compositor::start(&compositor_directory);
        let root = directory.0.join("browse");
        std::fs::create_dir_all(root.join("child")).unwrap();
        std::fs::write(root.join("a"), b"one").unwrap();
        std::fs::write(root.join("z"), b"larger payload").unwrap();
        for (name, mode, seconds) in [("a", 0o640, 0), ("z", 0o755, 951_782_400)] {
            let file = std::fs::File::open(root.join(name)).unwrap();
            file.set_permissions(std::fs::Permissions::from_mode(mode)).unwrap();
            file.set_modified(std::time::UNIX_EPOCH + Duration::from_secs(seconds)).unwrap();
        }
        let dictionary = directory.0.join("dictionary");
        std::fs::write(&dictionary, b"one\n").unwrap();
        let mut editor = EditorProcess::start_with_profile(&directory,
            &compositor.directory.join("wayland-0"), &root, &dictionary, profile);
        editor.wait_keyboard(profile);
        let window = compositor.window();
        let before = compositor.observe(&window);
        assert_eq!(compositor.request("fullscreen", 1024), b"ok\n");
        editor.wait_field("state", "window", "800,576,1");
        let listing = wait_directory_rows(&mut editor, 1, 0, &["child/", "a", "z"]);
        let meta = std::fs::metadata(root.join("a")).unwrap();
        let expected = format!("  -rw-r-----   1 {:>5} {:>5}          3 1970-01-01 00:00Z a", meta.uid(), meta.gid());
        assert_eq!(listing.lines().nth(1).unwrap(), expected);
        assert!(listing.lines().nth(2).unwrap().contains("14 2000-02-29 00:00Z z"));
        editor.rendered_at(800, 576);
        compositor.rendered_rows(&window, before, 88, &[&expected], td_editor::render::PAPER);
        compositor.chord(None, 31); // s: size
        wait_directory_rows(&mut editor, 1, 1, &["child/", "z", "a"]);
        editor.wait_field("state", "directory-sort", "1,size,0");
        compositor.chord(None, 108);
        compositor.chord(None, 108);
        let selected = format!("1,{}", td_editor::control::hex(root.join("a").as_os_str().as_encoded_bytes()));
        editor.wait_field("state", "directory-entry", &selected);
        compositor.chord(None, KEY_W);
        let expected_path = root.join("a").to_str().unwrap().to_owned();
        editor.wait_field("clipboard-state", "source-bytes", &expected_path.len().to_string());
        compositor.click(270, 32); // Directory menu
        editor.wait_field("state", "modal", "0,0,0,0,1,0,0,0,0");
        compositor.click(270, 132); // Sort by Modified
        wait_directory_rows(&mut editor, 1, 2, &["child/", "z", "a"]);
        editor.wait_field("state", "directory-sort", "1,modified,0");
        editor.wait_field("state", "directory-entry", &selected);
        compositor.chord(Some(KEY_LEFT_SHIFT), 31); // S: reverse
        wait_directory_rows(&mut editor, 1, 3, &["child/", "a", "z"]);
        editor.wait_field("state", "directory-sort", "1,modified,1");
        editor.wait_field("state", "directory-entry", &selected);
        compositor.chord(None, 108); // Select z; menu must replace the earlier a offer.
        let expected_path = root.join("z").to_str().unwrap().to_owned();
        editor.wait_field("state", "directory-entry", &format!("1,{}",
            td_editor::control::hex(expected_path.as_bytes())));
        compositor.click(270, 32);
        editor.wait_field("state", "modal", "0,0,0,0,1,0,0,0,0");
        compositor.click(270, 60); // Copy Entry Full Path
        editor.wait_field("state", "modal", "0,0,0,0,0,0,0,0,0");
        compositor.chord(None, KEY_G);
        wait_directory_rows(&mut editor, 1, 4, &["child/", "a", "z"]);
        editor.wait_field("state", "directory-sort", "1,modified,1");
        // Decoded sorting uses the same revision/input fences.
        let state = editor.ok("state");
        editor.ok(&format!("key\t1\t4\t{}\t73", field(&state, "input-generation").unwrap()));
        wait_directory_rows(&mut editor, 1, 5, &["child/", "z", "a"]);
        editor.wait_field("state", "directory-sort", "1,name,1");
        assert_eq!(editor.ok("new"), "2");
        compositor.chord(Some(KEY_LEFT_CTRL), if profile == "windows" { 47 } else { KEY_Y });
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let state = editor.ok("state");
            if state.split('\t').any(|f| f.starts_with("tab=2,1,")) { break; }
            assert!(Instant::now() < deadline, "entry clipboard paste: {state}");
        }
        assert_eq!(editor.ok("text\t2\t1\t0\t4096"),
            format!("{}\t{}", expected_path.len(), td_editor::control::hex(expected_path.as_bytes())));
        editor.ok("undo\t2\t1");
        assert_eq!(std::fs::read(root.join("a")).unwrap(), b"one");
        editor.quit();
        compositor.stop();
    }
}

#[test]
#[ignore = "ready supplies the disposable native compositor"]
fn native_minibuffer_keeps_document_visible_and_pages_large_completions() {
    let lines = ["first", "second", "third", "fourth", "fifth", "sixth", "seventh"];
    let contents = lines.join("\n");
    for profile in ["windows", "emacs"] {
        let compositor_directory = Directory::new();
        let directory = Directory::new();
        let mut compositor = Compositor::start(&compositor_directory);
        std::fs::write(directory.0.join("draft"), &contents).unwrap();
        for i in 0..320 {
            std::fs::write(directory.0.join(format!("item{i:04}")), format!("payload {i}")).unwrap();
        }
        let socket = directory.0.join("control");
        let log = directory.0.join("stderr");
        let child = Command::new(env!("CARGO_BIN_EXE_td-editor"))
            .arg("--control-socket").arg(&socket).arg(format!("--keys={profile}"))
            .arg("draft").current_dir(&directory.0).env_clear()
            .env("WAYLAND_DISPLAY", compositor.directory.join("wayland-0"))
            .env("XDG_RUNTIME_DIR", &directory.0).env("TMPDIR", &directory.0)
            .stdin(Stdio::null()).stdout(Stdio::null())
            .stderr(Stdio::from(std::fs::File::create(&log).unwrap())).spawn().unwrap();
        let mut editor = EditorProcess { child, socket, log, next: 0 };
        editor.legacy_keyboard(profile);
        let window = compositor.window();
        assert_eq!(compositor.request("fullscreen", 1024), b"ok\n");
        editor.wait_field("state", "window", "800,576,1");
        editor.rendered_at(800, 576);
        for prompt in ["find-forward", "replace", "path-open"] {
            let before = compositor.observe(&window);
            match (prompt, profile) {
                ("find-forward", "windows") => compositor.chord(Some(KEY_LEFT_CTRL), 33),
                ("find-forward", _) => compositor.chord(Some(KEY_LEFT_CTRL), 31),
                ("replace", "windows") => compositor.chord(Some(KEY_LEFT_CTRL), 35),
                ("replace", _) => {
                    compositor.click(68, 32);
                    editor.wait_field("state", "modal", "0,0,0,0,1,0,0,0,0");
                    compositor.click(68, 324);
                }
                (_, "windows") => compositor.chord(Some(KEY_LEFT_CTRL), 24),
                _ => { compositor.chord(Some(KEY_LEFT_CTRL), KEY_X); compositor.chord(Some(KEY_LEFT_CTRL), 33); }
            }
            editor.wait_field("prompt-state", "prompt", prompt);
            editor.wait_field("state", "minibuffer", "6,96");
            editor.rendered_at(800, 576);
            compositor.rendered_rows(&window, before, 168, &lines, td_editor::render::PAPER);
            editor.wait_tab(0, &contents);
            if prompt != "path-open" {
                compositor.chord(None, KEY_ESCAPE);
                editor.wait_field("prompt-state", "prompt", "none");
            }
        }
        for code in [23, 20, 18, 50] { compositor.chord(None, code); } // item
        let before = compositor.observe(&window);
        compositor.chord(None, 15);
        editor.wait_field("prompt-state", "completion", "ready");
        editor.wait_field("prompt-state", "completion-count", "320");
        editor.wait_field("state", "minibuffer", "15,240");
        let page = editor.ok("prompt-state");
        assert_eq!(field(&page, "completion-page-size"), Some("12"));
        assert_eq!(page.matches("completion-item=").count(), 12);
        editor.rendered_at(800, 576);
        compositor.rendered_rows(&window, before, 312, &lines, td_editor::render::PAPER);
        compositor.rendered_rows(&window, before, 272, &["  item0011"], td_editor::render::CHROME);
        for _ in 0..26 { compositor.chord(None, 109); } // PageDown
        editor.wait_field("prompt-state", "completion-selected", "312");
        let page = editor.ok("prompt-state");
        assert_eq!(page.matches("completion-item=").count(), 8);
        assert!(page.contains("completion-item=319,6974656d30333139"));
        compositor.chord(None, 104); // PageUp
        editor.wait_field("prompt-state", "completion-selected", "300");
        compositor.chord(None, 109);
        compositor.chord(None, 109);
        editor.wait_field("prompt-state", "completion-selected", "319");
        compositor.chord(None, 28);
        editor.wait_field("state", "active", "2");
        assert_eq!(editor.ok("text\t2\t0\t0\t100"), "11\t7061796c6f616420333139");
        editor.wait_tab(0, &contents);
        assert_eq!(std::fs::read_to_string(directory.0.join("draft")).unwrap(), contents);
        editor.quit();
        compositor.stop();
    }
}

#[test]
#[ignore = "ready supplies the disposable native compositor"]
fn native_directory_tabs_reuse_shift_open_refresh_and_copy_path() {
    for profile in ["windows", "emacs"] {
        let compositor_directory = Directory::new();
        let directory = Directory::new();
        let mut compositor = Compositor::start(&compositor_directory);
        let root = directory.0.join("browse");
        std::fs::create_dir_all(root.join("child")).unwrap();
        std::fs::write(root.join("child/note"), b"body").unwrap();
        let dictionary = directory.0.join("dictionary");
        std::fs::write(&dictionary, b"body\n").unwrap();
        let display = compositor.directory.join("wayland-0");
        let mut editor =
            EditorProcess::start_with_profile(&directory, &display, &root, &dictionary, profile);
        editor.wait_keyboard(profile);
        let window = compositor.window();
        assert_eq!(compositor.request("fullscreen", 1024), b"ok\n");
        editor.wait_field("state", "window", "800,576,1");
        editor.rendered_at(800, 576);
        assert_eq!(field(&editor.ok("state"), "line-numbers"), Some("1"));
        let listing = wait_directory_rows(&mut editor, 1, 0, &["child/"]);
        assert!(editor.request("insert\t1\t0\t0\t0\t78").unwrap().starts_with("error\tunavailable\t"));
        editor.wait_tab(0, &listing);
        let before = compositor.observe(&window);
        compositor.chord(None, 28); // Enter reuses tab 1.
        let listing = wait_directory_rows(&mut editor, 1, 1, &["note"]);
        compositor.rendered_text(&mut editor, &window, 1, before, &listing[..10], 0);
        let state = editor.ok("state");
        assert_eq!(state.matches("\ttab=").count(), 1);
        assert_eq!(field(&state, "tab-kind"), Some("1,directory"));
        assert_eq!(
            field(&state, "directory"),
            Some(
                format!(
                    "1,1,{}",
                    td_editor::control::hex(root.join("child").as_os_str().as_encoded_bytes())
                )
                .as_str()
            )
        );
        compositor.chord(None, 28); // Opening a file always keeps origin.
        editor.wait_field("state", "active", "2");
        assert_eq!(editor.ok("text\t2\t0\t0\t100"), "4\t626f6479");
        editor.ok("select-tab\t1\t1");
        compositor.chord(Some(KEY_LEFT_SHIFT), 7); // ^ returns to parent.
        wait_directory_rows(&mut editor, 1, 2, &["child/"]);
        compositor.key(KEY_LEFT_SHIFT, true);
        compositor.click(40, 80); // Shift-click opens child in a third tab.
        compositor.key(KEY_LEFT_SHIFT, false);
        editor.wait_field("state", "active", "3");
        wait_directory_rows(&mut editor, 3, 0, &["note"]);
        compositor.click(40, 80); // Already-open note selects tab 2 and keeps 3.
        editor.wait_field("state", "active", "2");
        assert_eq!(editor.ok("state").matches("\ttab=").count(), 3);
        editor.ok("select-tab\t3\t0");
        compositor.chord(None, KEY_Q); // q closes only the directory tab.
        editor.wait_field("state", "active", "1");
        assert_eq!(editor.ok("state").matches("\ttab=").count(), 2);
        editor.ok("select-tab\t1\t2");
        std::fs::write(root.join("added"), b"new").unwrap();
        compositor.chord(None, KEY_G);
        wait_directory_rows(&mut editor, 1, 3, &["child/", "added"]);
        compositor.chord(None, 68); // F10.
        editor.wait_field("state", "modal", "0,0,0,0,1,0,0,0,0");
        compositor.click(60, 180); // File > Copy Full File Path, including directory.
        editor.wait_field("state", "modal", "0,0,0,0,0,0,0,0,0");
        editor.wait_field(
            "clipboard-state",
            "source-bytes",
            &root.as_os_str().as_encoded_bytes().len().to_string(),
        );
        editor.ok("select-tab\t2\t0");
        compositor.chord(
            Some(KEY_LEFT_CTRL),
            if profile == "windows" { 47 } else { KEY_Y },
        );
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let state = editor.ok("state");
            if state.split('\t').any(|row| row.starts_with("tab=2,1,")) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "directory clipboard paste: {state}"
            );
        }
        let expected = format!("{}body", root.display());
        assert_eq!(
            editor.ok("text\t2\t1\t0\t4096"),
            format!(
                "{}\t{}",
                expected.len(),
                td_editor::control::hex(expected.as_bytes())
            )
        );
        editor.job("save\t2\t1");
        editor.job(&format!("open\t{}", td_editor::control::hex(root.as_os_str().as_encoded_bytes())));
        editor.wait_field("state", "active", "4");
        let state = editor.ok("state");
        assert!(state.contains("tab-kind=4,directory"));
        let token = field(&state, "input-generation").unwrap();
        editor.ok(&format!("key\t4\t0\t{token}\t52657475726e"));
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let state = editor.ok("state");
            if state.split('\t').any(|row| row.starts_with("tab=4,1,")) { break; }
            assert!(Instant::now() < deadline, "remote directory navigation: {state}");
        }
        wait_directory_rows(&mut editor, 4, 1, &["note"]);
        assert_eq!(
            std::fs::read(root.join("child/note")).unwrap(),
            expected.as_bytes()
        );
        editor.quit();
        compositor.stop();
    }
}

#[test]
#[ignore = "ready supplies the disposable native compositor"]
fn native_path_completion_lists_cycles_and_opens_literal_relative_file() {
    use td_editor::render::{CHROME, Draw, Geometry, GlyphStyle, INK, Primitive, Raster, Scale};
    for profile in ["windows", "emacs"] {
        let compositor_directory = Directory::new();
        let directory = Directory::new();
        let mut compositor = Compositor::start(&compositor_directory);
        for (name, bytes) in [("draft", "keep"), ("alpha", "one"), ("alpine", "two")] {
            std::fs::write(directory.0.join(name), bytes).unwrap();
        }
        let socket = directory.0.join("control");
        let log = directory.0.join("stderr");
        let child = Command::new(env!("CARGO_BIN_EXE_td-editor"))
            .arg("--control-socket").arg(&socket)
            .arg(format!("--keys={profile}")).arg("draft")
            .current_dir(&directory.0)
            .env_clear()
            .env("WAYLAND_DISPLAY", compositor.directory.join("wayland-0"))
            .env("XDG_RUNTIME_DIR", &directory.0)
            .env("TMPDIR", &directory.0)
            .stdin(Stdio::null()).stdout(Stdio::null())
            .stderr(Stdio::from(std::fs::File::create(&log).unwrap()))
            .spawn().unwrap();
        let mut editor = EditorProcess { child, socket, log, next: 0 };
        editor.legacy_keyboard(profile);
        let window = compositor.window();
        assert_eq!(compositor.request("fullscreen", 1024), b"ok\n");
        editor.wait_field("state", "window", "800,576,1");
        editor.rendered_at(800, 576);
        if profile == "emacs" {
            compositor.chord(Some(KEY_LEFT_CTRL), KEY_X);
            compositor.chord(Some(KEY_LEFT_CTRL), 33); // C-f
        } else {
            compositor.chord(Some(KEY_LEFT_CTRL), 24); // C-o
        }
        editor.wait_field("prompt-state", "prompt", "path-open");
        compositor.chord(None, KEY_A);
        compositor.chord(None, 38); // l
        editor.wait_field("prompt-state", "text", "616c");
        let before = compositor.observe(&window);
        compositor.chord(None, 15); // Tab
        editor.wait_field("prompt-state", "completion", "ready");
        editor.wait_field("prompt-state", "text", "616c70");
        let state = editor.ok("prompt-state");
        assert_eq!(field(&state, "completion-count"), Some("2"));
        assert!(state.contains("completion-item=0,616c706861"));
        assert!(state.contains("completion-item=1,616c70696e65"));
        editor.rendered_at(800, 576);
        let font = td_editor::font::pinned().unwrap();
        let geometry = Geometry::new(64, 32, Scale::new(1).unwrap()).unwrap();
        let mut expected = CHROME.to_le_bytes().repeat(64 * 32);
        let mut raster = Raster::new(&mut expected, &font, geometry, 64 * 4).unwrap();
        for (row, text) in ["  alpha", "  alpine"].into_iter().enumerate() {
            for (column, scalar) in text.chars().enumerate() {
                raster.draw(Draw {
                    clip: geometry.bounds(),
                    primitive: Primitive::Glyph {
                        x: (column * 8) as i64,
                        y: (row * 16) as i64,
                        scalar,
                        style: GlyphStyle::medium(INK, CHROME),
                    },
                });
            }
        }
        let deadline = Instant::now() + TIMEOUT;
        loop {
            assert!(Instant::now() < deadline, "completion list pixel deadline");
            let first = compositor.observe(&window);
            if !first.current || first.commit <= before.commit {
                continue;
            }
            let capture = compositor.request("capture", FRAME_BYTES + 128);
            let (output, pixels) = ppm(&capture, &compositor.session).unwrap();
            let second = compositor.observe(&window);
            if !second.current || second.commit != first.commit {
                continue;
            }
            assert_eq!(first.client, before.client);
            assert_eq!(second.client, first.client);
            assert!(output > first.output && output <= second.output);
            if (0..32).all(|y| {
                (0..64).all(|x| {
                    let source = ((y + 96) * 800 + x + 8) * 3;
                    let target = (y * 64 + x) * 4;
                    pixels[source..source + 3]
                        == [expected[target + 2], expected[target + 1], expected[target]]
                })
            }) {
                break;
            }
        }
        compositor.chord(Some(KEY_LEFT_SHIFT), 15); // Shift+Tab selects last.
        editor.wait_field("prompt-state", "text", "616c70696e65");
        compositor.chord(None, 108); // Down wraps to first.
        editor.wait_field("prompt-state", "text", "616c706861");
        editor.wait_tab(0, "keep");
        compositor.chord(None, 28); // Return opens, never inserts path text.
        editor.wait_field("state", "active", "2");
        assert_eq!(editor.ok("text\t2\t0\t0\t100"), "3\t6f6e65");
        assert_eq!(std::fs::read(directory.0.join("draft")).unwrap(), b"keep");
        assert_eq!(std::fs::read(directory.0.join("alpha")).unwrap(), b"one");
        editor.quit();
        compositor.stop();
    }
}

#[test]
#[ignore = "ready supplies the disposable native compositor"]
fn ordinary_invocation_keeps_foreground_lifetime_and_inherited_stdin() {
    use std::io::Seek;
    let compositor_directory = Directory::new();
    let directory = Directory::new();
    let mut compositor = Compositor::start(&compositor_directory);
    let first_name = "-draft é;$";
    let first = directory.0.join(first_name);
    let second = directory.0.join("new draft");
    let input = directory.0.join("caller-input");
    let output = directory.0.join("caller-output");
    let socket = directory.0.join("control");
    let log = directory.0.join("stderr");
    std::fs::write(&first, b"old\n").unwrap();
    std::fs::write(&input, b"caller-owned unread input\n").unwrap();
    let mut inherited = std::fs::File::open(&input).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_td-editor"))
        .arg("--control-socket").arg(&socket)
        .arg("--").arg(first_name).arg("new draft")
        .current_dir(&directory.0)
        .env_clear()
        .env("WAYLAND_DISPLAY", compositor.directory.join("wayland-0"))
        .env("XDG_RUNTIME_DIR", &directory.0)
        .env("TMPDIR", &directory.0)
        .stdin(Stdio::from(inherited.try_clone().unwrap()))
        .stdout(Stdio::from(std::fs::File::create(&output).unwrap()))
        .stderr(Stdio::from(std::fs::File::create(&log).unwrap()))
        .spawn().unwrap();
    let mut editor = EditorProcess { child, socket, log, next: 0 };
    editor.wait_keyboard("windows");
    editor.wait_field("state", "active", "2");
    assert!(!second.exists());
    let window = compositor.window();
    assert_eq!(compositor.request("fullscreen", 1024), b"ok\n");
    editor.wait_field("state", "window", "800,576,1");
    editor.rendered_at(800, 576);
    editor.ok("set-line-numbers\t2\t0\t0");
    editor.ok("select-tab\t1\t0");
    assert_eq!(editor.ok("text\t1\t0\t0\t100"), "4\t6f6c640a");
    let before = compositor.observe(&window);
    editor.ok("insert\t1\t0\t0\t0\t78");
    compositor.rendered_text(&mut editor, &window, 1, before, "xold", 1);
    assert!(editor.child.try_wait().unwrap().is_none());
    assert_eq!(inherited.stream_position().unwrap(), 0);
    assert_eq!(std::fs::metadata(&output).unwrap().len(), 0);
    assert_eq!(editor.job("save\t1\t1"), "job=1,save,1,1,0,complete,-");
    assert_eq!(std::fs::read(&first).unwrap(), b"xold\n");
    editor.ok("select-tab\t2\t0");
    editor.ok("insert\t2\t0\t0\t0\t6e65770a");
    assert_eq!(editor.job("save\t2\t1"), "job=2,save,2,1,0,complete,-");
    assert_eq!(std::fs::read(&second).unwrap(), b"new\n");
    assert!(editor.child.try_wait().unwrap().is_none());
    editor.quit();
    assert_eq!(inherited.stream_position().unwrap(), 0);
    assert_eq!(std::fs::metadata(&output).unwrap().len(), 0);
    assert_eq!(std::fs::metadata(&editor.log).unwrap().len(), 0);
    compositor.stop();
}

struct Compositor {
    child: Child,
    directory: PathBuf,
    session: String,
    action: u64,
    output: Option<JoinHandle<()>>,
}

#[derive(Debug, Clone, Copy)]
struct Observation {
    client: u64,
    commit: u64,
    output: u64,
    current: bool,
}

fn identity(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn number(value: &str) -> Result<u64> {
    if value.is_empty()
        || (value.len() > 1 && value.starts_with('0'))
        || !value.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err("noncanonical compositor counter".into());
    }
    value
        .parse()
        .map_err(|_| "compositor counter overflow".into())
}

fn observation(reply: &[u8], session: &str, window: &str) -> Result<Observation> {
    if reply.len() > 1024 {
        return Err("compositor observation limit".into());
    }
    let text = std::str::from_utf8(reply).map_err(|_| "compositor observation UTF-8")?;
    let prefix = format!("ok\ntd-client-v1 session={session} window={window} client=");
    let body = text
        .strip_prefix(&prefix)
        .ok_or("compositor observation identity")?;
    let (client, body) = body
        .split_once(" commit=")
        .ok_or("compositor client field")?;
    let (commit, body) = body
        .split_once(" output=")
        .ok_or("compositor commit field")?;
    let (output, current) = body
        .split_once(" current=")
        .ok_or("compositor output field")?;
    let observation = Observation {
        client: number(client)?,
        commit: number(commit)?,
        output: number(output)?,
        current: match current {
            "yes\n" => true,
            "no\n" => false,
            _ => return Err("compositor current field".into()),
        },
    };
    if observation.client == 0
        || (observation.current && (observation.commit == 0 || observation.output == 0))
    {
        return Err("invalid compositor observation counters".into());
    }
    Ok(observation)
}

fn ppm<'a>(reply: &'a [u8], session: &str) -> Result<(u64, &'a [u8])> {
    if reply.len() > FRAME_BYTES + 128 {
        return Err("compositor capture limit".into());
    }
    let prefix = format!("ok\nP6\n# td-output-v1 session={session} output=");
    let body = reply
        .strip_prefix(prefix.as_bytes())
        .ok_or("capture session or format")?;
    let newline = body
        .iter()
        .position(|byte| *byte == b'\n')
        .ok_or("capture output line")?;
    let output =
        number(std::str::from_utf8(&body[..newline]).map_err(|_| "capture counter UTF-8")?)?;
    let pixels = body[newline + 1..]
        .strip_prefix(b"800 600\n255\n")
        .ok_or("capture geometry")?;
    if output == 0 || pixels.len() != FRAME_BYTES {
        return Err("capture size or counter".into());
    }
    Ok((output, pixels))
}

impl Compositor {
    fn rendered_rows(
        &self,
        window: &str,
        after: Observation,
        top: usize,
        lines: &[&str],
        background: u32,
    ) {
        let expected: Vec<_> = lines
            .iter()
            .map(|line| text_pixels_on(line, background))
            .collect();
        let deadline = Instant::now() + TIMEOUT;
        loop {
            assert!(
                Instant::now() < deadline,
                "minibuffer covered document pixel deadline"
            );
            let first = self.observe(window);
            if !first.current || first.commit <= after.commit {
                continue;
            }
            let capture = self.request("capture", FRAME_BYTES + 128);
            let (output, pixels) = ppm(&capture, &self.session).unwrap();
            let second = self.observe(window);
            if !second.current || first.commit != second.commit {
                continue;
            }
            assert_eq!(first.client, after.client);
            assert_eq!(second.client, first.client);
            assert!(output > first.output && output <= second.output);
            if lines
                .iter()
                .zip(&expected)
                .enumerate()
                .all(|(row, (line, glyphs))| {
                    let width = line.len() * 8;
                    (0..16).all(|y| {
                        (0..width).all(|x| {
                            if row == 0 && x == 0 {
                                return true;
                            } // blinking caret only
                            let source = ((top + row * 16 + y) * 800 + 8 + x) * 3;
                            let target = (y * width + x) * 4;
                            pixels[source..source + 3]
                                == [glyphs[target + 2], glyphs[target + 1], glyphs[target]]
                        })
                    })
                })
            {
                break;
            }
        }
    }

    fn start(directory: &Directory) -> Self {
        Self::start_with_clipboard(directory, false)
    }

    fn start_with_clipboard(directory: &Directory, clipboard: bool) -> Self {
        let binary = PathBuf::from(
            std::env::var_os("TD_TEST_COMPOSITOR")
                .expect("set TD_TEST_COMPOSITOR to an explicitly built td-compositor; see README"),
        );
        assert!(
            binary.is_absolute(),
            "compositor test tool must be an absolute path"
        );
        let session_dir = directory.0.join("session");
        let mut command = Command::new(binary);
        if clipboard {
            command.args(["headless", "--clipboard-control", "enabled"]);
        } else {
            command.arg("headless");
        }
        let child = command
            .arg("--session-dir")
            .arg(&session_dir)
            .args([
                "--width",
                "800",
                "--height",
                "600",
                "--input-control",
                "enabled",
                "--capture-control",
                "enabled",
            ])
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(std::fs::File::create(directory.0.join("compositor.log")).unwrap())
            .spawn()
            .unwrap();
        // Establish cleanup before any later setup or readiness can unwind.
        let mut compositor = Self {
            child,
            directory: session_dir,
            session: String::new(),
            action: 0,
            output: None,
        };
        let stdout = compositor.child.stdout.take().unwrap();
        let (send, receive) = mpsc::sync_channel(1);
        compositor.output = Some(
            std::thread::Builder::new()
                .spawn(move || {
                    let mut reader = BufReader::new(stdout);
                    let mut line = String::new();
                    if reader.by_ref().take(4097).read_line(&mut line).is_ok() && line.len() <= 4096
                    {
                        let _ = send.send(line);
                    }
                    let _ = std::io::copy(&mut reader, &mut std::io::sink());
                })
                .unwrap(),
        );
        let ready = receive
            .recv_timeout(TIMEOUT)
            .expect("compositor readiness deadline");
        let session = ready
            .strip_prefix("TD-COMPOSITOR-HEADLESS-READY version=2 session=")
            .and_then(|line| line.strip_suffix(" width=800 height=600 scale=1\n"))
            .expect("compositor readiness grammar");
        assert!(identity(session));
        compositor.session = session.to_string();
        compositor
    }

    fn request(&self, line: &str, limit: usize) -> Vec<u8> {
        let deadline = Instant::now() + TIMEOUT;
        let mut stream = UnixStream::connect(self.directory.join("td-control")).unwrap();
        write_until(&mut stream, format!("{line}\n").as_bytes(), deadline).unwrap();
        let mut reply = Vec::new();
        let mut chunk = [0; 16384];
        loop {
            stream
                .set_read_timeout(Some(remaining(deadline).unwrap()))
                .unwrap();
            let count = match stream.read(&mut chunk) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                result => result.expect("compositor reply"),
            };
            if count == 0 {
                break;
            }
            assert!(reply.len() + count <= limit, "compositor reply byte bound");
            reply.extend_from_slice(&chunk[..count]);
        }
        reply
    }

    fn key(&mut self, key: u32, down: bool) {
        // Timestamps follow the receipt counter; callers do not send action IDs.
        let time = self.action + 1;
        let line = format!(
            "key {} {time} {key} {}",
            self.session,
            if down { "down" } else { "up" }
        );
        self.receipt(&line);
    }

    fn receipt(&mut self, line: &str) {
        let action = self.action + 1;
        let expected = format!(
            "ok\ntd-action-v1 session={} action={action}\n",
            self.session
        );
        assert_eq!(self.request(line, 1024), expected.as_bytes());
        self.action = action;
    }

    fn pointer(&mut self, x: u32, y: u32, buttons: u8) {
        self.pointer_frame(x, y, buttons, 0, 0);
    }

    fn pointer_frame(&mut self, x: u32, y: u32, buttons: u8, vertical: i32, horizontal: i32) {
        let time = self.action + 1;
        self.receipt(&format!(
            "pointer {} {time} {x} {y} {buttons} {vertical} {horizontal}",
            self.session
        ));
    }

    fn click(&mut self, x: u32, y: u32) {
        self.pointer(x, y, 1);
        self.pointer(x, y, 0);
    }

    fn stop(&mut self) {
        self.child.stdin.take();
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success());
                break;
            }
            assert!(Instant::now() < deadline, "compositor owner-EOF deadline");
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(!self.directory.exists());
    }

    fn chord(&mut self, modifier: Option<u32>, key: u32) {
        if let Some(modifier) = modifier {
            self.key(modifier, true);
        }
        self.key(key, true);
        self.key(key, false);
        if let Some(modifier) = modifier {
            self.key(modifier, false);
        }
    }

    fn window(&self) -> String {
        let windows = self.windows();
        assert_eq!(
            windows.len(),
            1,
            "fixture expects exactly one editor window"
        );
        windows.into_iter().next().unwrap()
    }

    fn windows(&self) -> Vec<String> {
        let layout = self.request("layout", 65536);
        let text = std::str::from_utf8(&layout).unwrap();
        text.lines()
            .filter_map(|line| line.strip_prefix("window id="))
            .map(|line| {
                let window = line.split_whitespace().next().expect("layout window ID");
                let id = window.strip_prefix('@').expect("layout window ID sigil");
                assert!(number(id).expect("canonical layout window ID") > 0);
                window.to_string()
            })
            .collect()
    }

    fn observe(&self, window: &str) -> Observation {
        let request = format!("observe-client {} {window}", self.session);
        observation(&self.request(&request, 1024), &self.session, window).unwrap()
    }

    fn rendered_text(
        &self,
        editor: &mut EditorProcess,
        window: &str,
        revision: u64,
        after: Observation,
        text: &str,
        caret: usize,
    ) {
        self.rendered_tab_text(editor, window, (1, revision), after, text, caret);
    }

    fn rendered_tab_text(
        &self,
        editor: &mut EditorProcess,
        window: &str,
        (tab, revision): (u64, u64),
        after: Observation,
        text: &str,
        caret: usize,
    ) {
        self.rendered_tab_text_at(editor, window, (tab, revision, 8), after, text, caret);
    }

    fn rendered_tab_text_at(
        &self,
        editor: &mut EditorProcess,
        window: &str,
        (tab, revision, left): (u64, u64, usize),
        after: Observation,
        text: &str,
        caret: usize,
    ) {
        let state = editor.ok("state");
        let generation = field(&state, "window-generation").unwrap();
        let frame = editor.ok(&format!("wait-frame\t{generation}"));
        let fields: Vec<_> = frame.split(',').collect();
        assert_eq!(
            &fields[2..],
            &[&tab.to_string(), &revision.to_string(), "800", "576", "1"]
        );
        let expected = text_pixels(text);
        let width = text.len() * 8;
        assert!(caret <= text.len() && left + width <= 800);
        let deadline = Instant::now() + TIMEOUT;
        loop {
            assert!(
                Instant::now() < deadline,
                "editor did not produce correlated text pixels"
            );
            let first = self.observe(window);
            if !first.current || first.commit <= after.commit {
                continue;
            }
            assert_eq!(first.client, after.client);
            let capture = self.request("capture", FRAME_BYTES + 128);
            let (output, pixels) = ppm(&capture, &self.session).unwrap();
            let second = self.observe(window);
            if !second.current || second.commit != first.commit {
                continue;
            }
            assert_eq!(second.client, first.client);
            assert!(output > first.output && output <= second.output);
            // Desktop bar stays 24px high in fullscreen; the document starts
            // at surface (8,48), hence output (8,72). Ignore only the
            // one-pixel caret column when it falls inside the sampled prefix.
            // This compares text pixels, not caret visibility.
            let equal = (0..16).all(|y| {
                (0..width).all(|x| {
                    if x == caret * 8 {
                        return true;
                    }
                    let source = ((y + 72) * 800 + x + left) * 3;
                    let target = (y * width + x) * 4;
                    pixels[source..source + 3]
                        == [expected[target + 2], expected[target + 1], expected[target]]
                })
            });
            if equal {
                return;
            }
        }
    }
}

impl Drop for Compositor {
    fn drop(&mut self) {
        self.child.stdin.take();
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
        if let Some(output) = self.output.take() {
            let _ = output.join();
        }
    }
}

fn text_pixels(text: &str) -> Vec<u8> {
    text_pixels_on(text, td_editor::render::PAPER)
}

fn text_pixels_on(text: &str, background: u32) -> Vec<u8> {
    use td_editor::render::{Draw, Geometry, GlyphStyle, INK, Primitive, Raster, Scale};
    assert!(text.is_ascii() && !text.is_empty() && text.len() <= 98);
    let font = td_editor::font::pinned().unwrap();
    let width = text.len() * 8;
    let geometry = Geometry::new(width, 16, Scale::new(1).unwrap()).unwrap();
    let mut pixels = background.to_le_bytes().repeat(width * 16);
    let mut raster = Raster::new(&mut pixels, &font, geometry, width * 4).unwrap();
    for (column, scalar) in text.chars().enumerate() {
        raster.draw(Draw {
            clip: geometry.bounds(),
            primitive: Primitive::Glyph {
                x: (column * 8) as i64,
                y: 0,
                scalar,
                style: GlyphStyle::medium(INK, background),
            },
        });
    }
    pixels
}

fn numbered_pixels(
    compositor: &Compositor,
    editor: &mut EditorProcess,
    window: &str,
    after: Observation,
    enabled: bool,
) {
    use td_editor::render::{
        Draw, Geometry, GlyphStyle, INK, LINE_NUMBER, PAPER, Primitive, Raster, Scale,
    };
    let state = editor.ok("state");
    let frame = editor.ok(&format!(
        "wait-frame\t{}",
        field(&state, "window-generation").unwrap()
    ));
    assert!(frame.ends_with(",1,0,800,576,1"), "{frame}");
    let font = td_editor::font::pinned().unwrap();
    let geometry = Geometry::new(56, 48, Scale::new(1).unwrap()).unwrap();
    let mut expected = PAPER.to_le_bytes().repeat(56 * 48);
    let mut raster = Raster::new(&mut expected, &font, geometry, 56 * 4).unwrap();
    for (row, text) in ["abc", "x", ""].into_iter().enumerate() {
        let digits = (row + 1).to_string();
        for (text, x, ink) in [
            (if enabled { digits.as_str() } else { "" }, 16, LINE_NUMBER),
            (text, if enabled { 32 } else { 8 }, INK),
        ] {
            for (column, scalar) in text.chars().enumerate() {
                raster.draw(Draw {
                    clip: geometry.bounds(),
                    primitive: Primitive::Glyph {
                        x: x + (column * 8) as i64,
                        y: (row * 16) as i64,
                        scalar,
                        style: GlyphStyle::medium(ink, PAPER),
                    },
                });
            }
        }
    }
    let deadline = Instant::now() + TIMEOUT;
    loop {
        assert!(
            Instant::now() < deadline,
            "numbered document pixel deadline"
        );
        let first = compositor.observe(window);
        if !first.current || first.commit <= after.commit {
            continue;
        }
        assert_eq!(first.client, after.client);
        let capture = compositor.request("capture", FRAME_BYTES + 128);
        let (output, pixels) = ppm(&capture, &compositor.session).unwrap();
        let second = compositor.observe(window);
        if !second.current || second.commit != first.commit {
            continue;
        }
        assert_eq!(second.client, first.client);
        assert!(output > first.output && output <= second.output);
        let same = (0..48).all(|y| {
            (0..56).all(|x| {
                if y < 16 && x == if enabled { 32 } else { 8 } {
                    return true;
                }
                let source = ((y + 72) * 800 + x) * 3;
                let target = (y * 56 + x) * 4;
                pixels[source..source + 3]
                    == [expected[target + 2], expected[target + 1], expected[target]]
            })
        });
        if same {
            break;
        }
    }
}

#[test]
#[ignore = "requires explicit built TD_TEST_COMPOSITOR; ready prepares it"]
fn native_line_numbers_default_menu_remote_and_pointer() {
    for profile in ["windows", "emacs"] {
        let compositor_directory = Directory::new();
        let directory = Directory::new();
        let mut compositor = Compositor::start(&compositor_directory);
        let file = directory.0.join("draft");
        let dictionary = directory.0.join("dictionary");
        std::fs::write(&file, b"abc\nx\n").unwrap();
        std::fs::write(&dictionary, b"abc\nx\n").unwrap();
        let display = compositor.directory.join("wayland-0");
        let mut editor =
            EditorProcess::start_with_profile(&directory, &display, &file, &dictionary, profile);
        editor.wait_keyboard(profile); // Exercise the actual production default.
        editor.wait_field("state", "line-numbers", "1");
        let window = compositor.window();
        let before = compositor.observe(&window);
        assert_eq!(compositor.request("fullscreen", 1024), b"ok\n");
        editor.wait_field("state", "window", "800,576,1");
        numbered_pixels(&compositor, &mut editor, &window, before, true);
        let before = compositor.observe(&window);
        compositor.click(136, 32); // Format header.
        editor.wait_field("state", "modal", "0,0,0,0,1,0,0,0,0");
        compositor.click(136, 252); // Line Numbers, zero-based Format row eight.
        editor.wait_field("state", "line-numbers", "0");
        editor.wait_field("state", "modal", "0,0,0,0,0,0,0,0,0");
        numbered_pixels(&compositor, &mut editor, &window, before, false);
        let before = compositor.observe(&window);
        editor.ok("set-line-numbers\t1\t0\t1");
        editor.wait_field("state", "line-numbers", "1");
        numbered_pixels(&compositor, &mut editor, &window, before, true);
        compositor.click(40, 80);
        editor.wait_field("state", "tab", "1,0,0,6,1,1,0,72,0,lf");
        compositor.click(16, 80); // Gutter cannot change the selection.
                                  // A later menu opening fences consumption of the preceding click.
        compositor.click(136, 32);
        editor.wait_field("state", "modal", "0,0,0,0,1,0,0,0,0");
        editor.wait_field("state", "tab", "1,0,0,6,1,1,0,72,0,lf");
        compositor.chord(None, KEY_ESCAPE);
        editor.wait_field("state", "modal", "0,0,0,0,0,0,0,0,0");
        editor.wait_tab(0, "abc\nx\n");
        assert_eq!(std::fs::read(&file).unwrap(), b"abc\nx\n");
        editor.quit();
        compositor.stop();
    }
}

fn keyboard_profile(profile: &str) {
    let compositor_directory = Directory::new();
    let directory = Directory::new();
    let mut compositor = Compositor::start(&compositor_directory);
    let file = directory.0.join("draft");
    let dictionary = directory.0.join("dictionary");
    std::fs::write(&file, b"one\n").unwrap();
    std::fs::write(&dictionary, b"one\n").unwrap();
    let display = compositor.directory.join("wayland-0");
    let mut editor =
        EditorProcess::start_with_profile(&directory, &display, &file, &dictionary, profile);
    editor.legacy_keyboard(profile);
    let window = compositor.window();
    assert_eq!(compositor.request("fullscreen", 1024), b"ok\n");
    editor.wait_field("state", "window", "800,576,1");
    editor.rendered_at(800, 576);
    let before = compositor.observe(&window);
    compositor.chord(Some(KEY_LEFT_SHIFT), KEY_A);
    editor.wait_tab(1, "Aone\n");
    compositor.rendered_text(&mut editor, &window, 1, before, "Aone", 1);
    let before = compositor.observe(&window);
    compositor.chord(None, KEY_B);
    editor.wait_tab(2, "Abone\n");
    compositor.rendered_text(&mut editor, &window, 2, before, "Abone", 2);
    let before = compositor.observe(&window);
    compositor.chord(
        Some(KEY_LEFT_CTRL),
        if profile == "windows" {
            KEY_Z
        } else {
            KEY_SLASH
        },
    );
    editor.wait_tab(3, "Aone\n");
    compositor.rendered_text(&mut editor, &window, 3, before, "Aone", 1);
    editor.job("save\t1\t3");
    assert_eq!(std::fs::read(&file).unwrap(), b"Aone\n");
    editor.quit();
    compositor.stop();
}

#[test]
#[ignore = "requires explicit built TD_TEST_COMPOSITOR; ready prepares it"]
fn native_windows_keyboard() {
    keyboard_profile("windows");
}

#[test]
#[ignore = "requires explicit built TD_TEST_COMPOSITOR; ready prepares it"]
fn native_emacs_keyboard() {
    keyboard_profile("emacs");
}

#[test]
#[ignore = "requires explicit built TD_TEST_COMPOSITOR; ready prepares it"]
fn native_pointer_selection_and_menus() {
    let compositor_directory = Directory::new();
    let directory = Directory::new();
    let mut compositor = Compositor::start(&compositor_directory);
    let file = directory.0.join("draft");
    let dictionary = directory.0.join("dictionary");
    std::fs::write(&file, b"one two\n").unwrap();
    std::fs::write(&dictionary, b"one\ntwo\n").unwrap();
    let display = compositor.directory.join("wayland-0");
    let mut editor = EditorProcess::start(&directory, &display, &file, &dictionary);
    editor.legacy_keyboard("windows");
    let window = compositor.window();
    assert_eq!(compositor.request("fullscreen", 1024), b"ok\n");
    editor.wait_field("state", "window", "800,576,1");
    editor.rendered_at(800, 576);
    // Output includes the 24px desktop bar: document origin is (8,72).
    // Literal 8px-cell expectations are independent of editor hit testing.
    compositor.pointer(9, 80, 0);
    editor.wait_field("state", "pointer-ready", "1");
    compositor.pointer(9, 80, 1);
    compositor.pointer(33, 80, 1);
    editor.wait_field("state", "tab", "1,0,0,8,0,3,0,72,0,lf");
    compositor.pointer(33, 80, 0);
    compositor.pointer(65, 80, 0);
    let before = compositor.observe(&window);
    compositor.chord(None, KEY_B);
    // Unheld motion must not extend the selection: replace only "one".
    editor.wait_tab(1, "b two\n");
    compositor.rendered_text(&mut editor, &window, 1, before, "b two", 1);
    compositor.chord(Some(KEY_LEFT_CTRL), KEY_Z);
    editor.wait_tab(2, "one two\n");
    let before = compositor.observe(&window);
    compositor.click(65, 80); // Collapse Undo's restored selection after "two".
    editor.wait_field("state", "tab", "1,2,0,8,7,7,0,72,0,lf");
    compositor.rendered_text(&mut editor, &window, 2, before, "one two", 7);
    compositor.click(68, 32); // Edit header: surface y=8 plus desktop bar.
    editor.wait_field("state", "modal", "0,0,0,0,1,0,0,0,0");
    compositor.click(68, 252); // Find: panel y=24, zero-based row eight.
    editor.wait_field("prompt-state", "prompt", "find-forward");
    let before = compositor.observe(&window);
    compositor.chord(None, KEY_ESCAPE);
    editor.wait_field("prompt-state", "prompt", "none");
    editor.wait_tab(2, "one two\n");
    compositor.rendered_text(&mut editor, &window, 2, before, "one two", 7);
    editor.job("save\t1\t2");
    assert_eq!(std::fs::read(&file).unwrap(), b"one two\n");
    editor.quit();
    compositor.stop();
}

#[test]
#[ignore = "requires explicit built TD_TEST_COMPOSITOR; ready prepares it"]
fn native_vertical_wheel_scrolls_without_editing() {
    let compositor_directory = Directory::new();
    let directory = Directory::new();
    let mut compositor = Compositor::start(&compositor_directory);
    let file = directory.0.join("draft");
    let dictionary = directory.0.join("dictionary");
    let text: String = (0..64).map(|row| format!("row{row:02}\n")).collect();
    std::fs::write(&file, &text).unwrap();
    std::fs::write(&dictionary, b"row\n").unwrap();
    let display = compositor.directory.join("wayland-0");
    let mut editor = EditorProcess::start(&directory, &display, &file, &dictionary);
    editor.legacy_keyboard("windows");
    let window = compositor.window();
    assert_eq!(compositor.request("fullscreen", 1024), b"ok\n");
    editor.wait_field("state", "window", "800,576,1");
    editor.rendered_at(800, 576);
    compositor.pointer(400, 80, 0);
    editor.wait_field("state", "pointer-ready", "1");
    for (detents, row, repeat) in [
        (-1, 3, false),
        (-1, 6, false),
        (2, 0, false),
        (-120, 34, true),
        (1, 31, false),
        (120, 0, true),
        (-1, 3, false),
    ] {
        let before = compositor.observe(&window);
        compositor.pointer_frame(400, 80, 0, detents, 0);
        editor.wait_field("state", "view", &format!("1,{row},0,96,31,1,downstream,-"));
        editor.wait_field("state", "tab", "1,0,0,384,0,0,0,72,0,lf");
        assert_eq!(
            editor.ok("text\t1\t0\t0\t384"),
            format!("384\t{}", td_editor::control::hex(text.as_bytes()))
        );
        let prefix = format!("row{row:02}");
        // The caret stays on row zero. Scrolled rows compare every pixel;
        // prefix.len() places the optional one-column mask outside the crop.
        compositor.rendered_text(
            &mut editor,
            &window,
            0,
            before,
            &prefix,
            if row == 0 { 0 } else { prefix.len() },
        );
        if repeat {
            // A clamped no-op owes no redraw. The following inward report
            // proves the resulting viewport without demanding a new frame here.
            compositor.pointer_frame(400, 80, 0, detents, 0);
        }
    }
    // Drag the visible scrollbar through the real pointer path; the caret
    // remains on row zero while the frame shows the bottom of the document.
    compositor.pointer_frame(400, 80, 0, 120, 0);
    editor.wait_field("state", "view", "1,0,0,96,31,1,downstream,-");
    let before = compositor.observe(&window);
    compositor.pointer(786, 74, 1);
    compositor.pointer(100, 575, 1);
    compositor.pointer(100, 575, 0);
    editor.wait_field("state", "view", "1,34,0,96,31,1,downstream,-");
    editor.wait_field("state", "tab", "1,0,0,384,0,0,0,72,0,lf");
    compositor.rendered_text(&mut editor, &window, 0, before, "row34", 5);
    // Decoded control owns its own thumb gesture and can return to the top.
    for (phase, x, y) in [("press", 786, 314), ("move", 100, 0), ("release", 100, 0)] {
        let state = editor.ok("state");
        let generation = field(&state, "input-generation").unwrap();
        editor.ok(&format!(
            "pointer\t1\t0\t{generation}\t{phase}\t{x}\t{y}\t0"
        ));
    }
    editor.wait_field("state", "view", "1,0,0,96,31,1,downstream,-");
    assert_eq!(std::fs::read(&file).unwrap(), text.as_bytes());
    editor.quit();
    compositor.stop();
}

#[test]
#[ignore = "requires explicit built TD_TEST_COMPOSITOR; ready prepares it"]
fn native_horizontal_wheel_respects_wrap_and_clamps_columns() {
    let compositor_directory = Directory::new();
    let directory = Directory::new();
    let mut compositor = Compositor::start(&compositor_directory);
    let file = directory.0.join("draft");
    let dictionary = directory.0.join("dictionary");
    let text = "abcdefghijklmnopqrstuvwxyz".repeat(5);
    std::fs::write(&file, &text).unwrap();
    std::fs::write(&dictionary, b"word\n").unwrap();
    let display = compositor.directory.join("wayland-0");
    let mut editor = EditorProcess::start(&directory, &display, &file, &dictionary);
    editor.legacy_keyboard("windows");
    let window = compositor.window();
    assert_eq!(compositor.request("fullscreen", 1024), b"ok\n");
    editor.wait_field("state", "window", "800,576,1");
    editor.rendered_at(800, 576);
    compositor.pointer(400, 80, 0);
    editor.wait_field("state", "pointer-ready", "1");
    compositor.pointer_frame(400, 80, 0, 0, 120); // Soft Wrap suppresses this.
    compositor.click(140, 32); // Format header.
    editor.wait_field("state", "modal", "0,0,0,0,1,0,0,0,0");
    // Menu admission follows the wheel frame on the same native pointer stream.
    editor.wait_field("state", "view", "1,0,0,96,31,1,downstream,-");
    compositor.click(140, 60); // Soft Wrap, first row.
    editor.wait_field("state", "view", "1,0,0,96,31,0,downstream,-");
    editor.wait_field("state", "modal", "0,0,0,0,0,0,0,0,0");
    compositor.pointer(400, 80, 0);
    for (columns, left, prefix, repeat) in [
        (1, 3, "defgh", false),
        (120, 35, "jklmn", true),
        (-1, 32, "ghijk", false),
        (-120, 0, "abcde", true),
        (1, 3, "defgh", false),
    ] {
        let before = compositor.observe(&window);
        // Both axes in one report: the single logical row cannot scroll down.
        compositor.pointer_frame(400, 80, 0, -1, columns);
        editor.wait_field("state", "view", &format!("1,0,{left},96,31,0,downstream,-"));
        editor.wait_field("state", "tab", "1,0,0,130,0,0,0,72,0,lf");
        assert_eq!(
            editor.ok("text\t1\t0\t0\t130"),
            format!("130\t{}", td_editor::control::hex(text.as_bytes()))
        );
        compositor.rendered_text(
            &mut editor,
            &window,
            0,
            before,
            prefix,
            if left == 0 { 0 } else { prefix.len() },
        );
        if repeat {
            // As above, the following inward report fences this clamped no-op.
            compositor.pointer_frame(400, 80, 0, -1, columns);
        }
    }
    assert_eq!(std::fs::read(&file).unwrap(), text.as_bytes());
    editor.quit();
    compositor.stop();
}

#[test]
#[ignore = "requires explicit built TD_TEST_COMPOSITOR; ready prepares it"]
fn native_clipboard_transfers_cut_snapshot_between_editors() {
    clipboard_between_editors("windows", ClipboardOperation::Cut);
}

#[test]
#[ignore = "requires explicit built TD_TEST_COMPOSITOR; ready prepares it"]
fn native_emacs_clipboard_transfers_marked_snapshot_between_editors() {
    clipboard_between_editors("emacs", ClipboardOperation::Cut);
}

#[test]
#[ignore = "requires explicit built TD_TEST_COMPOSITOR; ready prepares it"]
fn native_windows_copy_preserves_selection_and_transfers_snapshot() {
    clipboard_between_editors("windows", ClipboardOperation::Copy);
}

#[test]
#[ignore = "requires explicit built TD_TEST_COMPOSITOR; ready prepares it"]
fn native_emacs_copy_preserves_selection_and_transfers_snapshot() {
    clipboard_between_editors("emacs", ClipboardOperation::Copy);
}

enum ClipboardOperation {
    Copy,
    Cut,
}

#[test]
#[ignore = "ready supplies the disposable native compositor"]
fn native_file_menu_copies_full_path_to_another_editor_without_selection() {
    for profile in ["windows", "emacs"] {
        let compositor_directory = Directory::new();
        let source_directory = Directory::new();
        let destination_directory = Directory::new();
        let mut compositor = Compositor::start(&compositor_directory);
        let source_path = source_directory.0.join("a path é;$");
        let dictionary = source_directory.0.join("dictionary");
        std::fs::write(&source_path, b"unchanged\n").unwrap();
        std::fs::write(&dictionary, b"unchanged\n").unwrap();
        let display = compositor.directory.join("wayland-0");
        let mut source = EditorProcess::start_with_profile(
            &source_directory,
            &display,
            &source_path,
            &dictionary,
            profile,
        );
        source.legacy_keyboard(profile);
        assert_eq!(compositor.request("fullscreen", 1024), b"ok\n");
        source.wait_field("state", "window", "800,576,1");
        source.rendered_at(800, 576);
        source.wait_field("state", "tab", "1,0,0,10,0,0,0,72,0,lf");
        compositor.pointer(400, 80, 0);
        source.wait_field("state", "pointer-ready", "1");
        compositor.click(12, 36); // File header, including compositor bar.
        source.wait_field("state", "modal", "0,0,0,0,1,0,0,0,0");
        compositor.click(100, 180); // File row five: Copy Full File Path.
        source.wait_field(
            "prompt-state",
            "notice",
            &td_editor::control::hex(b"Full file path offered to clipboard."),
        );
        let expected = std::fs::canonicalize(&source_path)
            .unwrap()
            .into_os_string()
            .into_string()
            .unwrap();
        source.wait_field("clipboard-state", "source-bytes", &expected.len().to_string());
        source.wait_field("state", "tab", "1,0,0,10,0,0,0,72,0,lf");
        assert_eq!(std::fs::read(&source_path).unwrap(), b"unchanged\n");
        assert_eq!(compositor.request("fullscreen", 1024), b"ok\n");
        let destination_path = destination_directory.0.join("destination");
        std::fs::write(&destination_path, b"").unwrap();
        let mut destination = EditorProcess::start_with_profile(
            &destination_directory,
            &display,
            &destination_path,
            &dictionary,
            profile,
        );
        destination.legacy_keyboard(profile);
        destination.wait_field("clipboard-state", "selection", "utf8");
        assert_eq!(compositor.request("fullscreen", 1024), b"ok\n");
        destination.wait_field("state", "window", "800,576,1");
        destination.rendered_at(800, 576);
        compositor.chord(
            Some(KEY_LEFT_CTRL),
            if profile == "emacs" { KEY_Y } else { KEY_V },
        );
        destination.wait_tab(1, &expected);
        destination.job("save\t1\t1");
        assert_eq!(std::fs::read(&destination_path).unwrap(), expected.as_bytes());
        source.wait_tab(0, "unchanged\n"); // Revision zero of tab one.
        destination.quit();
        source.quit();
        compositor.stop();
    }
}

fn copy_clipboard_text(compositor: &mut Compositor, profile: &str) {
    if profile == "emacs" {
        compositor.chord(Some(KEY_LEFT_ALT), KEY_W);
    } else {
        compositor.chord(Some(KEY_LEFT_CTRL), KEY_C);
    }
}

fn select_clipboard_text(compositor: &mut Compositor, profile: &str) {
    if profile == "emacs" {
        compositor.chord(Some(KEY_LEFT_CTRL), KEY_HOME);
        compositor.chord(Some(KEY_LEFT_CTRL), KEY_SPACE); // Set mark.
        compositor.chord(Some(KEY_LEFT_CTRL), KEY_END); // Extend to document end.
    } else {
        compositor.chord(Some(KEY_LEFT_CTRL), KEY_A);
    }
}

fn clipboard_between_editors(profile: &str, operation: ClipboardOperation) {
    assert!(matches!(profile, "windows" | "emacs"), "unsupported key profile");
    let compositor_directory = Directory::new();
    let source_directory = Directory::new();
    let destination_directory = Directory::new();
    let mut compositor = Compositor::start_with_clipboard(&compositor_directory, true);
    let source_path = source_directory.0.join("source");
    let source_dictionary = source_directory.0.join("dictionary");
    let text = "clip café e\u{301} 🦀\nsecond line\n";
    std::fs::write(&source_path, text).unwrap();
    std::fs::write(&source_dictionary, b"clip\nline\nsecond\n").unwrap();
    let display = compositor.directory.join("wayland-0");
    let mut source = EditorProcess::start_with_profile(
        &source_directory,
        &display,
        &source_path,
        &source_dictionary,
        profile,
    );
    source.legacy_keyboard(profile);
    let source_window = compositor.window();
    assert_eq!(compositor.request("fullscreen", 1024), b"ok\n");
    source.wait_field("state", "window", "800,576,1");
    source.rendered_at(800, 576);
    select_clipboard_text(&mut compositor, profile);
    // CONTROL.md tab: ID, revision, dirty, bytes, anchor, caret,
    // auto-fill, fill-column, BOM, ending. Keep the full wire oracle literal.
    let initial_selection = format!("1,0,0,{0},0,{0},0,72,0,lf", text.len());
    source.wait_field("state", "tab", &initial_selection);
    let paste_key = if profile == "emacs" { KEY_Y } else { KEY_V };
    let source_revision = match operation {
        ClipboardOperation::Cut => {
            let cut_key = if profile == "emacs" { KEY_W } else { KEY_X };
            compositor.chord(Some(KEY_LEFT_CTRL), cut_key);
            source.wait_tab(1, "");
            2
        }
        ClipboardOperation::Copy => {
            let offered = td_editor::control::hex(b"Selection offered to clipboard.");
            assert_ne!(
                field(&source.ok("prompt-state"), "notice"),
                Some(offered.as_str()),
                "Copy must start without prior selection-offered feedback"
            );
            copy_clipboard_text(&mut compositor, profile);
            source.wait_field("prompt-state", "notice", &offered);
            source.wait_field("state", "tab", &initial_selection);
            source.wait_tab(0, text);
            assert_eq!(std::fs::read(&source_path).unwrap(), text.as_bytes());
            1
        }
    };
    let before = compositor.observe(&source_window);
    compositor.chord(None, KEY_B);
    source.wait_tab(source_revision, "b");
    compositor.rendered_text(&mut source, &source_window, source_revision, before, "b", 1);
    source.job(&format!("save\t1\t{source_revision}"));
    assert_eq!(std::fs::read(&source_path).unwrap(), b"b");
    let collapsed = format!("1,{source_revision},0,1,1,1,0,72,0,lf");
    source.wait_field("state", "tab", &collapsed);
    let empty_copy = td_editor::control::hex(b"Nothing selected to copy.");
    assert_ne!(
        field(&source.ok("prompt-state"), "notice"),
        Some(empty_copy.as_str())
    );
    copy_clipboard_text(&mut compositor, profile);
    source.wait_field("prompt-state", "notice", &empty_copy);
    source.wait_field("state", "tab", &collapsed);
    source.wait_tab(source_revision, "b");
    assert_eq!(std::fs::read(&source_path).unwrap(), b"b");
    source.wait_field("clipboard-state", "device", "1");
    source.wait_field("clipboard-state", "source-bytes", &text.len().to_string());
    // The destination must still receive the original offer, not empty text.
    // Reveal tiling before mapping the destination. The reply fences the
    // compositor layout change; no intermediate source frame is sampled.
    assert_eq!(compositor.request("fullscreen", 1024), b"ok\n");

    let destination_path = destination_directory.0.join("destination");
    let destination_dictionary = destination_directory.0.join("dictionary");
    std::fs::write(&destination_path, b"").unwrap();
    std::fs::write(&destination_dictionary, b"clip\nline\nsecond\n").unwrap();
    let mut destination = EditorProcess::start_with_profile(
        &destination_directory,
        &display,
        &destination_path,
        &destination_dictionary,
        profile,
    );
    destination.legacy_keyboard(profile);
    destination.wait_field("state", "focus", "1");
    source.wait_field("state", "focus", "0");
    source.wait_field("clipboard-state", "focus", "0");
    source.wait_field("clipboard-state", "source-bytes", &text.len().to_string());
    destination.wait_field("clipboard-state", "selection", "utf8");
    destination.wait_field("clipboard-state", "source-bytes", "-");
    let windows = compositor.windows();
    assert_eq!(windows.len(), 2);
    assert!(windows.contains(&source_window));
    let destination_window = windows.iter().find(|id| **id != source_window).unwrap();
    assert_eq!(compositor.request("fullscreen", 1024), b"ok\n");
    destination.wait_field("state", "window", "800,576,1");
    destination.rendered_at(800, 576);
    destination.wait_tab(0, ""); // An offer is not an insertion.
    let before_paste = compositor.observe(destination_window);
    assert_ne!(before_paste.client, before.client);
    let hold_reply = |state: &str| {
        format!(
            "ok\ntd-clipboard-v1 session={} hold=1 window={destination_window} state={state}\n",
            compositor.session,
        )
    };
    // Pin the first hold in this fresh compositor; do not accept arbitrary IDs.
    assert_eq!(
        compositor.request(
            &format!("clipboard-arm {} {destination_window}", compositor.session,),
            1024
        ),
        hold_reply("armed").as_bytes()
    );
    let held = hold_reply("held");
    let armed = hold_reply("armed");
    let released = hold_reply("released");
    let paste_started = Instant::now();
    compositor.chord(Some(KEY_LEFT_CTRL), paste_key);
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let reply = compositor.request(&format!("clipboard-status {} 1", compositor.session), 1024);
        if reply == held.as_bytes() {
            break;
        }
        assert_eq!(reply, armed.as_bytes(), "hold failed before receiving the transfer");
        assert!(
            Instant::now() < deadline,
            "native Paste never reached the hold: {}",
            String::from_utf8_lossy(&reply)
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    destination.wait_field("clipboard-state", "incoming", "1");
    source.wait_field("clipboard-state", "outgoing", "0");
    destination.wait_field("state", "tab", "1,0,0,0,0,0,0,72,0,lf");
    destination.wait_tab(0, "");
    let pasting = td_editor::control::hex(b"Pasting UTF-8 text; Escape cancels.");
    destination.wait_field("prompt-state", "notice", &pasting);
    if profile == "emacs" {
        compositor.chord(Some(KEY_LEFT_CTRL), KEY_G);
    } else {
        compositor.chord(None, KEY_ESCAPE);
    }
    destination.wait_field("clipboard-state", "incoming", "0");
    // A timeout followed by Cancel could clear both fields too. Finish
    // within four seconds measured before Paste, below its five-second
    // reader deadline, or fail closed even if all state assertions pass.
    destination.wait_field("prompt-state", "notice", "-");
    assert!(
        paste_started.elapsed() < Duration::from_secs(4),
        "native Cancel exceeded its evidence budget"
    );
    destination.wait_field("state", "tab", "1,0,0,0,0,0,0,72,0,lf");
    destination.wait_tab(0, "");
    compositor.rendered_text(&mut destination, destination_window, 0, before_paste, " ", 0);
    let send_failed = b"Clipboard send failed:";
    let prior_notice = source.ok("prompt-state");
    let prior_notice = field(&prior_notice, "notice").unwrap();
    assert!(
        prior_notice == "-"
            || !td_editor::control::unhex(prior_notice).unwrap().starts_with(send_failed)
    );
    let release_started = Instant::now();
    assert_eq!(compositor.request(&format!(
        "clipboard-release {} 1", compositor.session,
    ), 1024), released.as_bytes());
    // Fence source processing, not just compositor queue admission or a
    // transient outgoing=0 observed before DataSourceSend reached it.
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let reply = source.ok("prompt-state");
        let notice = field(&reply, "notice").unwrap();
        if notice != "-" && td_editor::control::unhex(notice).unwrap().starts_with(send_failed) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "source did not observe the cancelled receiver: {reply}"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    source.wait_field("clipboard-state", "outgoing", "0");
    assert!(
        release_started.elapsed() < Duration::from_secs(4),
        "source failure exceeded its evidence budget"
    );
    destination.wait_field("clipboard-state", "incoming", "0");
    destination.wait_field("state", "tab", "1,0,0,0,0,0,0,72,0,lf");
    destination.wait_tab(0, "");
    source.wait_tab(source_revision, "b");
    source.wait_field("state", "tab", &collapsed);
    assert_eq!(std::fs::read(&source_path).unwrap(), b"b");
    assert_eq!(std::fs::read(&destination_path).unwrap(), b"");
    // A second held Paste loses focus before any source send. Compositor
    // invalidation closes its endpoint, so EOF may race the keyboard leave;
    // this checks the settled native state, not which cancellation wins.
    let hold_id = 2;
    let hold_reply = |state: &str| {
        format!(
            "ok\ntd-clipboard-v1 session={} hold={hold_id} window={destination_window} state={state}\n",
            compositor.session,
        )
    };
    let armed = hold_reply("armed");
    assert_eq!(
        compositor.request(
            &format!("clipboard-arm {} {destination_window}", compositor.session,),
            1024
        ),
        armed.as_bytes()
    );
    let held = hold_reply("held");
    let invalidated = hold_reply("invalidated");
    let focus_paste_started = Instant::now();
    compositor.chord(Some(KEY_LEFT_CTRL), paste_key);
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let reply = compositor.request(
            &format!("clipboard-status {} {hold_id}", compositor.session), 1024,
        );
        if reply == held.as_bytes() {
            break;
        }
        assert_eq!(reply, armed.as_bytes(), "second hold failed before receive");
        assert!(
            Instant::now() < deadline,
            "second native Paste never reached the hold"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    destination.wait_field("clipboard-state", "incoming", "1");
    destination.wait_field("prompt-state", "notice", &pasting);
    let before_focus = compositor.observe(destination_window);
    assert_eq!(
        compositor.request(&format!("focus {source_window}"), 1024), b"ok\n",
    );
    destination.wait_field("state", "focus", "0");
    destination.wait_field("clipboard-state", "focus", "0");
    destination.wait_field("clipboard-state", "selection", "none");
    destination.wait_field("clipboard-state", "incoming", "0");
    assert_ne!(
        field(&destination.ok("prompt-state"), "notice"), Some(pasting.as_str()),
    );
    destination.wait_field("state", "tab", "1,0,0,0,0,0,0,72,0,lf");
    destination.wait_tab(0, "");
    source.wait_field("state", "focus", "1");
    source.wait_field("state", "tab", &collapsed);
    source.wait_tab(source_revision, "b");
    assert_eq!(compositor.request(&format!(
        "clipboard-status {} {hold_id}", compositor.session,
    ), 1024), invalidated.as_bytes());
    assert_eq!(compositor.request(&format!(
        "clipboard-release {} {hold_id}", compositor.session,
    ), 1024), b"unavailable clipboard hold has no releasable transfer\n");
    assert_eq!(std::fs::read(&source_path).unwrap(), b"b");
    assert_eq!(std::fs::read(&destination_path).unwrap(), b"");
    assert!(
        focus_paste_started.elapsed() < Duration::from_secs(4),
        "focus loss exceeded its transfer evidence budget"
    );
    assert_eq!(
        compositor.request(&format!("focus {destination_window}"), 1024), b"ok\n",
    );
    destination.wait_field("state", "focus", "1");
    destination.wait_field("clipboard-state", "selection", "utf8");
    // Named focus reveals the tiled layout; restore the capture geometry.
    assert_eq!(compositor.request("fullscreen", 1024), b"ok\n");
    destination.wait_field("state", "window", "800,576,1");
    destination.wait_field("state", "tab", "1,0,0,0,0,0,0,72,0,lf");
    source.wait_field("state", "focus", "0");
    compositor.rendered_text(
        &mut destination, destination_window, 0, before_focus, " ", 0,
    );
    // Returning focus never revives the old descriptor or its hold identity.
    assert_eq!(compositor.request(&format!(
        "clipboard-status {} {hold_id}", compositor.session,
    ), 1024), invalidated.as_bytes());
    let before_paste = compositor.observe(destination_window);
    // A fresh Paste after cancellation must still consume the original offer.
    compositor.chord(Some(KEY_LEFT_CTRL), paste_key);
    destination.wait_tab(1, text);
    destination.wait_field("clipboard-state", "incoming", "0");
    source.wait_field("clipboard-state", "outgoing", "0");
    // The ASCII prefix proves transported pixels; full UTF-8 is checked above.
    // The caret is on the final empty line; mask column 4 is outside the crop.
    compositor.rendered_text(
        &mut destination,
        destination_window,
        1,
        before_paste,
        "clip",
        4,
    );
    destination.job("save\t1\t1");
    assert_eq!(std::fs::read(&destination_path).unwrap(), text.as_bytes());
    source.wait_tab(source_revision, "b");
    let saved_destination = format!("1,1,0,{0},{0},{0},0,72,0,lf", text.len());
    destination.wait_field("state", "tab", &saved_destination);
    source.wait_field("state", "tab", &collapsed);
    let hold_id = 3;
    let hold_reply = |state: &str| {
        format!(
            "ok\ntd-clipboard-v1 session={} hold={hold_id} window={destination_window} state={state}\n",
            compositor.session,
        )
    };
    let armed = hold_reply("armed");
    assert_eq!(
        compositor.request(
            &format!("clipboard-arm {} {destination_window}", compositor.session,),
            1024
        ),
        armed.as_bytes()
    );
    let held = hold_reply("held");
    let invalidated = hold_reply("invalidated");
    let owner_exit_paste_started = Instant::now();
    compositor.chord(Some(KEY_LEFT_CTRL), paste_key);
    let deadline = Instant::now() + TIMEOUT;
    let status_command = format!("clipboard-status {} {hold_id}", compositor.session);
    loop {
        let reply = compositor.request(&status_command, 1024);
        if reply == held.as_bytes() {
            break;
        }
        assert_eq!(
            reply, armed.as_bytes(), "owner-exit hold failed before receive: {}",
            String::from_utf8_lossy(&reply),
        );
        assert!(
            Instant::now() < deadline,
            "owner-exit Paste never reached the hold"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    destination.wait_field("clipboard-state", "incoming", "1");
    destination.wait_field("prompt-state", "notice", &pasting);
    destination.wait_field("state", "tab", &saved_destination);
    destination.wait_tab(1, text);
    source.wait_field("clipboard-state", "outgoing", "0");
    source.quit();
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let remaining = compositor.windows();
        assert!(
            remaining.contains(destination_window),
            "destination window exited prematurely: {remaining:?}"
        );
        if remaining.as_slice() == std::slice::from_ref(destination_window) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "source window still live: {remaining:?}"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    destination.wait_field("state", "focus", "1");
    destination.wait_field("clipboard-state", "selection", "none");
    destination.wait_field("clipboard-state", "incoming", "0");
    destination.wait_field("state", "tab", &saved_destination);
    destination.wait_tab(1, text);
    assert_eq!(compositor.request(&status_command, 1024), invalidated.as_bytes());
    assert_eq!(compositor.request(&format!(
        "clipboard-release {} {hold_id}", compositor.session,
    ), 1024), b"unavailable clipboard hold has no releasable transfer\n");
    assert_eq!(std::fs::read(&destination_path).unwrap(), text.as_bytes());
    assert_eq!(std::fs::read(&source_path).unwrap(), b"b");
    assert!(
        owner_exit_paste_started.elapsed() < Duration::from_secs(4),
        "owner exit exceeded its transfer evidence budget"
    );
    select_clipboard_text(&mut compositor, profile);
    destination.wait_field("clipboard-state", "selection", "none");
    let selected = format!("1,1,0,{0},0,{0},0,72,0,lf", text.len());
    destination.wait_field("state", "tab", &selected);
    let no_offer = td_editor::control::hex(b"Clipboard has no supported UTF-8 text offer.");
    assert_ne!(
        field(&destination.ok("prompt-state"), "notice"),
        Some(no_offer.as_str())
    );
    compositor.chord(Some(KEY_LEFT_CTRL), paste_key);
    // Pin the native refusal, not just unchanged text: stale identical data
    // could replace the selection without visibly changing its bytes.
    destination.wait_field("prompt-state", "notice", &no_offer);
    destination.wait_field("state", "tab", &selected);
    destination.wait_tab(1, text);
    let before_collapse = compositor.observe(destination_window);
    if profile == "emacs" {
        compositor.chord(Some(KEY_LEFT_CTRL), KEY_G); // Deactivate and collapse mark.
        destination.wait_field("prompt-state", "notice", "-");
    } else {
        compositor.chord(None, KEY_RIGHT); // Collapse to the selection end.
    }
    destination.wait_field(
        "state",
        "tab",
        &format!("1,1,0,{0},{0},{0},0,72,0,lf", text.len()),
    );
    if profile == "windows" {
        destination.wait_field("prompt-state", "notice", &no_offer);
    }
    compositor.rendered_text(
        &mut destination,
        destination_window,
        1,
        before_collapse,
        "clip",
        4,
    );
    destination.wait_tab(1, text);
    assert_eq!(std::fs::read(&destination_path).unwrap(), text.as_bytes());
    assert_eq!(std::fs::read(&source_path).unwrap(), b"b");
    destination.quit();
    compositor.stop();
}

#[test]
#[ignore = "requires an explicitly built TD_TEST_COMPOSITOR; ready runs this case"]
fn native_control_edit_spelling_save_and_dirty_close() {
    let compositor_directory = Directory::new();
    let directory = Directory::new();
    let mut compositor = Compositor::start(&compositor_directory);
    let file = directory.0.join("-draft");
    let dictionary = directory.0.join("dictionary");
    std::fs::write(&file, b"\xef\xbb\xbfone\r\nwrng\r\n").unwrap();
    std::fs::write(&dictionary, b"one\nwarm\nwrong\n").unwrap();
    let display = compositor.directory.join("wayland-0");
    let mut editor = EditorProcess::start(&directory, &display, &file, &dictionary);
    editor.legacy_keyboard("windows");
    let window = compositor.window();
    assert_eq!(compositor.request("fullscreen", 1024), b"ok\n");
    editor.wait_field("state", "window", "800,576,1");
    editor.rendered_at(800, 576);
    let before = compositor.observe(&window);
    control_edit_jobs_and_close_dialogs(&mut editor, &file);
    // Caret is on the final empty line, outside the first-row prefix.
    compositor.rendered_text(&mut editor, &window, 4, before, "warm", 4);
    assert_eq!(std::fs::read(&file).unwrap(), b"\xef\xbb\xbfwarm one\r\nwrong\r\n");
    editor.quit();
    compositor.stop();
}

#[test]
#[ignore = "requires an explicitly built TD_TEST_COMPOSITOR; ready runs this case"]
fn native_menu_prompts_and_fill_column() {
    let compositor_directory = Directory::new();
    let directory = Directory::new();
    let mut compositor = Compositor::start(&compositor_directory);
    let file = directory.0.join("draft");
    let dictionary = directory.0.join("dictionary");
    std::fs::write(&file, b"one wrng\n").unwrap();
    std::fs::write(&dictionary, b"one\nwrong\n").unwrap();
    let display = compositor.directory.join("wayland-0");
    let mut editor = EditorProcess::start(&directory, &display, &file, &dictionary);
    editor.legacy_keyboard("windows");
    let window = compositor.window();
    assert_eq!(compositor.request("fullscreen", 1024), b"ok\n");
    editor.wait_field("state", "window", "800,576,1");
    editor.rendered_at(800, 576);
    compositor.pointer(9, 80, 0);
    editor.wait_field("state", "pointer-ready", "1");
    let before = compositor.observe(&window);
    control_menu_prompts_and_fill(&mut editor, &file, true, |process, _, group, row| {
        // Literal output coordinates include the 24px desktop bar.
        // Native clicks have no revision field; the prompt answers pin it.
        let (x, prompt) = match (group, row) {
            (1, 8) => (68, "find-forward"),
            (1, 11) => (68, "replace"),
            (3, 1) => (196, "command"),
            _ => panic!("unexpected shared menu choice"),
        };
        compositor.click(x, 32); // Desktop bar (24) plus header inset (8).
        process.wait_field("state", "modal", "0,0,0,0,1,0,0,0,0");
        compositor.click(x, 60 + row as u32 * 24); // Two bars plus row center (12).
        process.wait_field("prompt-state", "prompt", prompt);
    });
    let filled = format!("{}word\nword", "word ".repeat(15));
    editor.wait_field("state", "tab", "1,3,0,84,0,0,0,80,0,lf");
    editor.wait_tab(3, &filled);
    editor.wait_field("prompt-state", "prompt", "none");
    compositor.rendered_text(&mut editor, &window, 3, before, "word", 0);
    assert_eq!(std::fs::read(&file).unwrap(), filled.as_bytes());
    editor.quit();
    compositor.stop();
}

#[test]
#[ignore = "requires an explicitly built TD_TEST_COMPOSITOR; ready runs this case"]
fn native_display_loss_preserves_unsaved_file_and_retires_control() {
    let compositor_directory = Directory::new();
    let directory = Directory::new();
    let mut compositor = Compositor::start(&compositor_directory);
    let file = directory.0.join("draft");
    let dictionary = directory.0.join("dictionary");
    std::fs::write(&file, b"original\n").unwrap();
    std::fs::write(&dictionary, b"original\n").unwrap();
    let display = compositor.directory.join("wayland-0");
    let mut editor = EditorProcess::start(&directory, &display, &file, &dictionary);
    editor.legacy_keyboard("windows");
    let window = compositor.window();
    assert_eq!(compositor.request("fullscreen", 1024), b"ok\n");
    editor.wait_field("state", "window", "800,576,1");
    editor.rendered_at(800, 576);
    let before = compositor.observe(&window);
    editor.ok("insert\t1\t0\t0\t0\t78");
    editor.wait_field("state", "tab", "1,1,1,10,1,1,0,72,0,lf");
    editor.wait_tab(1, "xoriginal\n");
    compositor.rendered_text(&mut editor, &window, 1, before, "xoriginal", 1);
    assert_eq!(std::fs::read(&file).unwrap(), b"original\n");
    // Normal owned compositor EOF closes Wayland while this dirty client lives.
    compositor.stop();
    assert_eq!(editor.exit().code(), Some(1));
    let diagnostic = std::fs::read_to_string(&editor.log).unwrap();
    // A focused caret redraw can observe the closed socket before receive does.
    assert!(
        diagnostic.contains("Wayland compositor disconnected")
            || diagnostic.contains("Wayland receive:")
            || diagnostic.contains("Wayland write:"),
        "{diagnostic}"
    );
    assert_eq!(std::fs::read(&file).unwrap(), b"original\n");
    assert_eq!(std::fs::read(&dictionary).unwrap(), b"original\n");
    assert!(!editor.socket.exists());
}

#[test]
#[ignore = "requires an explicitly built TD_TEST_COMPOSITOR; ready runs this case"]
fn native_conflict_reload_requires_fresh_dialog_and_explicit_discard() {
    let compositor_directory = Directory::new();
    let directory = Directory::new();
    let mut compositor = Compositor::start(&compositor_directory);
    let file = directory.0.join("draft");
    let dictionary = directory.0.join("dictionary");
    std::fs::write(&file, b"original\n").unwrap();
    std::fs::write(&dictionary, b"original\noutside\n").unwrap();
    let display = compositor.directory.join("wayland-0");
    let mut editor = EditorProcess::start(&directory, &display, &file, &dictionary);
    editor.legacy_keyboard("windows");
    let window = compositor.window();
    assert_eq!(compositor.request("fullscreen", 1024), b"ok\n");
    editor.wait_field("state", "window", "800,576,1");
    editor.rendered_at(800, 576);
    editor.ok("insert\t1\t0\t0\t0\t78");
    editor.wait_field("state", "tab", "1,1,1,10,1,1,0,72,0,lf");
    editor.wait_tab(1, "xoriginal\n");
    std::fs::write(&file, b"outside\n").unwrap();
    let conflict = |process: &mut EditorProcess| {
        let response = process.request("save\t1\t1").unwrap();
        let job = response.strip_prefix("pending\t").expect(&response);
        assert_eq!(
            process.wait_job_outcome(job, ",error,unavailable"),
            format!("job={job},save,1,1,0,error,unavailable")
        );
        let state = process.ok("state");
        let dialog = field(&state, "dialog").unwrap();
        let (id, rest) = dialog.split_once(',').unwrap();
        assert_eq!(rest, "conflict,question,1,1,cancel+reload+save-as");
        id.to_owned()
    };
    let first = conflict(&mut editor);
    editor.ok(&format!("dialog-answer\t{first}\t1\t1\tcancel"));
    editor.wait_field("state", "dialog", "-");
    let second = conflict(&mut editor);
    assert_ne!(first, second);
    assert!(editor
        .request(&format!("dialog-answer\t{first}\t1\t1\treload"))
        .unwrap()
        .starts_with("error\tinvalid-argument\t"));
    assert!(editor
        .request(&format!("dialog-answer\t{second}\t1\t1\tdiscard-reload"))
        .unwrap()
        .starts_with("error\tunavailable\t"));
    assert_eq!(
        editor.ok(&format!("dialog-answer\t{second}\t1\t1\treload")),
        format!("dialog\t{second}")
    );
    editor.wait_field(
        "state",
        "dialog",
        &format!("{second},conflict,discard,1,1,cancel+discard-reload"),
    );
    editor.wait_field("state", "tab", "1,1,1,10,1,1,0,72,0,lf");
    editor.wait_tab(1, "xoriginal\n");
    assert_eq!(std::fs::read(&file).unwrap(), b"outside\n");
    editor.ok(&format!("dialog-answer\t{second}\t1\t1\tcancel"));
    editor.wait_field("state", "dialog", "-");
    editor.wait_field("state", "tab", "1,1,1,10,1,1,0,72,0,lf");
    editor.wait_tab(1, "xoriginal\n");
    assert_eq!(std::fs::read(&file).unwrap(), b"outside\n");
    let third = conflict(&mut editor);
    assert_ne!(third, first);
    assert_ne!(third, second);
    assert!(editor
        .request(&format!("dialog-answer\t{second}\t1\t1\tdiscard-reload"))
        .unwrap()
        .starts_with("error\tinvalid-argument\t"));
    assert_eq!(
        editor.ok(&format!("dialog-answer\t{third}\t1\t1\treload")),
        format!("dialog\t{third}")
    );
    editor.wait_field(
        "state",
        "dialog",
        &format!("{third},conflict,discard,1,1,cancel+discard-reload"),
    );
    let before = compositor.observe(&window);
    let response = editor
        .request(&format!("dialog-answer\t{third}\t1\t1\tdiscard-reload"))
        .unwrap();
    let job = response.strip_prefix("pending\t").expect(&response);
    assert_eq!(
        editor.wait_job(job),
        format!("job={job},reload,1,1,0,complete,-")
    );
    editor.wait_field("state", "dialog", "-");
    editor.wait_field("state", "tab", "1,2,0,8,0,0,0,72,0,lf");
    editor.wait_tab(2, "outside\n");
    compositor.rendered_text(&mut editor, &window, 2, before, "outside", 0);
    assert_eq!(std::fs::read(&file).unwrap(), b"outside\n");
    editor.quit();
    compositor.stop();
}

#[test]
#[ignore = "requires an explicitly built TD_TEST_COMPOSITOR; ready runs this case"]
fn native_open_and_save_as_preserve_literal_paths_and_dirty_duplicate() {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};

    let compositor_directory = Directory::new();
    let directory = Directory::new();
    let mut compositor = Compositor::start(&compositor_directory);
    let file = directory.0.join("draft");
    let dictionary = directory.0.join("dictionary");
    let missing = directory.0.join(std::ffi::OsString::from_vec(
        b"-missing \xff".to_vec(),
    ));
    let destination = directory.0.join(std::ffi::OsString::from_vec(
        b"-saved \xfe".to_vec(),
    ));
    let missing_hex = td_editor::control::hex(missing.as_os_str().as_bytes());
    let destination_hex = td_editor::control::hex(destination.as_os_str().as_bytes());
    std::fs::write(&file, b"base\n").unwrap();
    std::fs::write(&dictionary, b"base\nnew\n").unwrap();
    let display = compositor.directory.join("wayland-0");
    let mut editor = EditorProcess::start(&directory, &display, &file, &dictionary);
    editor.legacy_keyboard("windows");
    let window = compositor.window();
    assert_eq!(compositor.request("fullscreen", 1024), b"ok\n");
    editor.wait_field("state", "window", "800,576,1");
    editor.rendered_at(800, 576);
    let second_tab = |process: &mut EditorProcess, row: &str, revision: u64, text: &str| {
        let state = process.ok("state");
        assert_eq!(field(&state, "active"), Some("2"));
        process.wait_tab(0, "base\n");
        let tabs: Vec<_> = state.split('\t')
            .filter(|field| field.starts_with("tab="))
            .collect();
        assert_eq!(tabs.len(), 2);
        assert_eq!(tabs.first().copied(), Some("tab=1,0,0,5,0,0,0,72,0,lf"));
        assert_eq!(tabs.get(1).and_then(|tab| tab.strip_prefix("tab=")), Some(row));
        assert_eq!(
            process.ok(&format!("text\t2\t{revision}\t0\t100")),
            format!("{}\t{}", text.len(), td_editor::control::hex(text.as_bytes()))
        );
    };
    let file_job = |process: &mut EditorProcess, request: &str, fields: &str| {
        let response = process.request(request).unwrap();
        let job = response.strip_prefix("pending\t").expect(&response);
        assert_eq!(process.wait_job(job), format!("job={job},{fields},complete,-"));
    };
    file_job(&mut editor, &format!("open\t{missing_hex}"), "open,2,0,0");
    second_tab(&mut editor, "2,0,1,0,0,0,0,72,0,lf", 0, "");
    assert!(!missing.exists());
    editor.ok("insert\t2\t0\t0\t0\t6e65770a");
    file_job(
        &mut editor,
        &format!("save-as\t2\t1\t{destination_hex}"),
        "save-as,2,1,0",
    );
    second_tab(&mut editor, "2,1,0,4,4,4,0,72,0,lf", 1, "new\n");
    assert_eq!(std::fs::read(&destination).unwrap(), b"new\n");
    assert!(!missing.exists());
    editor.ok("select-range\t2\t1\t0\t0");
    editor.ok("insert\t2\t1\t0\t0\t78");
    editor.ok("select-tab\t1\t0");
    editor.wait_field("state", "active", "1");
    let before = compositor.observe(&window);
    file_job(&mut editor, &format!("open\t{destination_hex}"), "open,2,2,0");
    second_tab(&mut editor, "2,2,1,5,1,1,0,72,0,lf", 2, "xnew\n");
    compositor.rendered_tab_text(&mut editor, &window, (2, 2), before, "xnew", 1);
    assert_eq!(std::fs::read(&destination).unwrap(), b"new\n");
    file_job(&mut editor, "save\t2\t2", "save,2,2,0");
    second_tab(&mut editor, "2,2,0,5,1,1,0,72,0,lf", 2, "xnew\n");
    assert_eq!(std::fs::read(&destination).unwrap(), b"xnew\n");
    assert_eq!(std::fs::read(&file).unwrap(), b"base\n");
    assert!(!missing.exists());
    editor.quit();
    compositor.stop();
}

#[cfg(not(feature = "test-file-barrier"))]
#[test]
#[ignore = "ready checks that the ordinary editor has no fixture channel"]
fn ordinary_editor_ignores_file_barrier_environment() {
    let compositor_directory = Directory::new();
    let directory = Directory::new();
    let mut compositor = Compositor::start(&compositor_directory);
    let file = directory.0.join("draft");
    let dictionary = directory.0.join("dictionary");
    std::fs::write(&file, b"ordinary").unwrap();
    std::fs::write(&dictionary, b"ordinary\n").unwrap();
    let display = compositor.directory.join("wayland-0");
    let missing_barrier = directory.0.join("must-not-connect");
    let mut editor = EditorProcess::start_with_barriers(
        &directory, &display, &file, &dictionary, "windows",
        Some(&missing_barrier), Some(&missing_barrier),
    );
    editor.wait_keyboard("windows");
    editor.wait_tab(0, "ordinary");
    editor.job("save\t1\t0");
    assert_eq!(std::fs::read(file).unwrap(), b"ordinary");
    assert!(!missing_barrier.exists());
    editor.quit();
    compositor.stop();
}

#[test]
fn native_observation_and_capture_decoders_refuse_stale_or_truncated_evidence() {
    let session = "00000000000000000000000000000007";
    let reply = format!(
        "ok\ntd-client-v1 session={session} window=@1 client=2 commit=3 output=4 current=yes\n"
    );
    assert!(
        observation(reply.as_bytes(), session, "@1")
            .unwrap()
            .current
    );
    for end in 0..reply.len() {
        assert!(observation(&reply.as_bytes()[..end], session, "@1").is_err());
    }
    assert!(observation(reply.as_bytes(), "00000000000000000000000000000008", "@1").is_err());
    assert!(observation(reply.as_bytes(), session, "@2").is_err());
    for (from, to) in [
        ("client=2", "client=0"),
        ("commit=3", "commit=0"),
        ("commit=3", "commit=03"),
        ("output=4", "output=18446744073709551616"),
        ("current=yes\n", "current=yes\nextra\n"),
    ] {
        assert!(observation(reply.replace(from, to).as_bytes(), session, "@1").is_err());
    }
    let mut capture =
        format!("ok\nP6\n# td-output-v1 session={session} output=4\n800 600\n255\n").into_bytes();
    let header = capture.len();
    capture.resize(header + FRAME_BYTES, 7);
    assert_eq!(ppm(&capture, session).unwrap().0, 4);
    for end in 0..header {
        assert!(ppm(&capture[..end], session).is_err());
    }
    assert!(ppm(&capture[..capture.len() - 1], session).is_err());
    assert!(ppm(&capture, "00000000000000000000000000000008").is_err());
    capture.push(7);
    assert!(ppm(&capture, session).is_err());
}
