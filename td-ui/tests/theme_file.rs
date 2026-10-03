#![forbid(unsafe_code)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

//! The theme file against a scratch directory: no file is no theme, a
//! written choice reads back from a private file in a private directory,
//! a crash's sibling is replaced, and what is not a short regular file
//! naming a theme is refused with its path.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use td_ui::theme::{DUSK, MAX_FILE_BYTES, MOSS};
use td_ui::theme_file::{read, write};

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("td-ui-theme-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn mode(path: &std::path::Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn a_choice_is_kept_privately_and_read_back() {
    let dir = scratch("kept");
    let path = dir.join("config/td-news/theme");
    assert_eq!(read(&path), Ok(None), "no file is no theme");
    write(&path, &DUSK).unwrap();
    assert_eq!(read(&path), Ok(Some(&DUSK)));
    assert_eq!(fs::read_to_string(&path).unwrap(), "dusk\n");
    assert_eq!(mode(&path), 0o600);
    assert_eq!(mode(&dir.join("config/td-news")), 0o700);
    assert_eq!(mode(&dir.join("config")), 0o700);
    // A sibling a crash left is replaced, and none is left behind.
    fs::write(path.with_extension("new"), "junk").unwrap();
    write(&path, &MOSS).unwrap();
    assert_eq!(read(&path), Ok(Some(&MOSS)));
    assert!(!path.with_extension("new").exists());
    // The sibling appends to the name, so a path that already ends in
    // `.new` is not its own sibling: a write that fails leaves its old
    // file, where removing the sibling first would have lost it.
    let odd = dir.join("odd.new");
    fs::write(&odd, "moss\n").unwrap();
    fs::create_dir(dir.join("odd.new.new")).unwrap();
    assert!(write(&odd, &DUSK).is_err());
    assert_eq!(read(&odd), Ok(Some(&MOSS)));
    fs::remove_dir(dir.join("odd.new.new")).unwrap();
    write(&odd, &DUSK).unwrap();
    assert_eq!(read(&odd), Ok(Some(&DUSK)));
    assert!(!dir.join("odd.new.new").exists());
    // A lock another holds is refused at once, the file as it was.
    let held = fs::File::open(path.parent().unwrap()).unwrap();
    held.lock().unwrap();
    assert!(write(&path, &DUSK).is_err());
    assert_eq!(read(&path), Ok(Some(&MOSS)));
    drop(held);
    // A link is replaced, not written through.
    let target = dir.join("target");
    fs::write(&target, "moss\n").unwrap();
    let link = dir.join("link");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    write(&link, &DUSK).unwrap();
    assert!(!fs::symlink_metadata(&link).unwrap().is_symlink());
    assert_eq!(fs::read_to_string(&target).unwrap(), "moss\n");
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn what_is_not_a_short_file_naming_a_theme_is_refused_with_its_path() {
    let dir = scratch("refused");
    let unknown = dir.join("unknown");
    fs::write(&unknown, "plaid\n").unwrap();
    let long = dir.join("long");
    fs::write(&long, " ".repeat(MAX_FILE_BYTES + 1)).unwrap();
    let directory = dir.join("directory");
    fs::create_dir(&directory).unwrap();
    let device = PathBuf::from("/dev/null");
    for path in [&unknown, &long, &directory, &device] {
        let why = read(path).unwrap_err();
        assert!(why.starts_with(&path.display().to_string()), "{why}");
    }
    // A directory is not replaced, nor a file made beneath a file.
    assert!(write(&directory, &DUSK).is_err());
    assert!(write(&dir.join("unknown/theme"), &DUSK).is_err());
    fs::remove_dir_all(&dir).unwrap();
}
