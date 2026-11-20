#![allow(clippy::unwrap_used)]
use super::*;
use std::os::unix::fs::symlink;

#[test]
fn opens_an_actual_path_descriptor_and_pins_the_symlink_itself() {
    let tmp = td_fs::TempDir::new("mta-clock-openat").unwrap();
    let root = File::open(tmp.path()).unwrap();
    std::fs::create_dir(tmp.path().join("runtime")).unwrap();
    let dir = directory(&root, "runtime").unwrap();
    symlink("/nonexistent", tmp.path().join("runtime/queue-clock")).unwrap();
    let leaf = journal(&dir).unwrap();
    assert!(leaf.metadata().unwrap().file_type().is_symlink());
    for (fd, extra) in [(&dir, DIRECTORY), (&leaf, 0)] {
        let info =
            std::fs::read_to_string(format!("/proc/self/fdinfo/{}", fd.as_raw_fd())).unwrap();
        let octal = info
            .lines()
            .find_map(|s| s.strip_prefix("flags:"))
            .unwrap()
            .trim();
        let bits = i32::from_str_radix(octal, 8).unwrap();
        let wanted = PATH | NOFOLLOW | CLOEXEC | extra;
        assert_eq!(bits & wanted, wanted);
    }
    for invalid in ["", ".", "..", "/", "a/b", "a\0b", &"x".repeat(256)] {
        assert_eq!(
            directory(&root, invalid).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }
}
