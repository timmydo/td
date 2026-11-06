//! The real `media-identify` binary, reached the way `media-serve` reaches
//! it: `identify_confined` hands it an open file as stdin. Below root there is
//! no identity to change to, so this proves the descriptor handoff, the verb's
//! routing and the reply; the worker's own refusal of privilege is below.
#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use td_disks::media::{unprivileged, Media, Serve, IDENTIFY_VERB};

const BIN: &str = env!("CARGO_BIN_EXE_td-disks");

fn serve() -> Serve {
    Serve {
        socket: PathBuf::new(),
        root: PathBuf::new(),
        sys: PathBuf::new(),
        dev: PathBuf::new(),
        mounts: PathBuf::new(),
        mount: PathBuf::new(),
        umount: PathBuf::new(),
        worker: BIN.into(),
        identify_deadline: Duration::from_secs(15),
        uid: 0,
        gid: 0,
        user: String::new(),
    }
}

fn fat32(label: &[u8; 11]) -> Vec<u8> {
    let mut image = vec![0u8; 64 << 10];
    image[11..13].copy_from_slice(&512u16.to_le_bytes());
    image[13] = 8;
    image[71..82].copy_from_slice(label);
    image[82..90].copy_from_slice(b"FAT32   ");
    image[510] = 0x55;
    image[511] = 0xaa;
    image
}

fn device(dir: &Path, name: &str, bytes: &[u8]) -> File {
    let path = dir.join(name);
    fs::write(&path, bytes).unwrap();
    File::open(path).unwrap()
}

fn running_unprivileged() -> bool {
    unprivileged(&fs::read_to_string("/proc/self/status").unwrap())
}

#[test]
fn a_medium_is_identified_through_the_worker() {
    if !running_unprivileged() {
        // Root's path would change identity, which this fixture must not assume
        // it can. Said aloud, so a privileged runner cannot read it as a pass.
        println!("SKIPPED: this runner holds privilege; the worker would refuse");
        return;
    }
    let dir = td_fs::TempDir::new("td-disks-identify").unwrap();
    let found = serve()
        .identify_confined(device(dir.path(), "fat", &fat32(b"MY CAMERA  ")))
        .unwrap();
    assert_eq!(found, Some((Media::Vfat, Some("MY_CAMERA".to_string()))));

    let blank = serve()
        .identify_confined(device(dir.path(), "blank", &[0u8; 64 << 10]))
        .unwrap();
    assert_eq!(blank, None);

    // A superblock the worker refuses comes back as the request's failure.
    let mut journal = vec![0u8; 64 << 10];
    journal[1024 + 0x38..1024 + 0x3a].copy_from_slice(&[0x53, 0xef]);
    journal[1024 + 0x5c..1024 + 0x60].copy_from_slice(&4u32.to_le_bytes());
    journal[1024 + 104..1024 + 120].copy_from_slice(&[7; 16]);
    let error = serve()
        .identify_confined(device(dir.path(), "journal", &journal))
        .unwrap_err();
    assert!(error.contains("external journal"), "{error}");
}

/// The verb reads what it was handed, and answers nothing with privilege. Which
/// branch runs depends on this runner: gates hold no privilege, so the refusal
/// is otherwise `unprivileged`'s unit test and the source pin in `media.rs`.
#[test]
fn the_verb_answers_as_its_identity_allows() {
    let dir = td_fs::TempDir::new("td-disks-identify").unwrap();
    let output = Command::new(BIN)
        .arg(IDENTIFY_VERB)
        .stdin(device(dir.path(), "fat", &fat32(b"MY CAMERA  ")))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    if running_unprivileged() {
        assert!(output.status.success());
        assert_eq!(output.stdout, b"ok vfat MY_CAMERA\n");
    } else {
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("refuses to read"), "{stderr}");
    }
}

/// A stand-in worker, as a script, for what the real one never does. A child
/// writes it: a sibling test forking while this process held the file open for
/// writing would carry that descriptor until its exec, and running the script
/// then fails with ETXTBSY. None of these can run with privilege, where the
/// change of identity would meet this 0700 scratch directory.
fn scripted(dir: &Path, body: &str, deadline: Duration) -> Option<Serve> {
    use std::os::unix::fs::PermissionsExt;
    if !running_unprivileged() {
        println!("SKIPPED: this runner holds privilege");
        return None;
    }
    let script = dir.join("worker");
    let written = Command::new("/bin/sh")
        .args(["-c", "printf '%s\\n' \"$2\" > \"$1\"", "sh"])
        .arg(&script)
        .arg(format!("#!/bin/sh\n{body}"))
        .status()
        .unwrap();
    assert!(written.success());
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    let mut serve = serve();
    serve.worker = script;
    serve.identify_deadline = deadline;
    Some(serve)
}

/// A sleep on an absolute path, since the worker's environment is empty.
fn sleep_program() -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join("sleep"))
            .find(|candidate| candidate.exists())
    })
}

/// A worker that exits while a descendant keeps its reply open cannot hold the
/// request past the deadline.
#[test]
fn a_descendant_holding_the_reply_costs_only_the_request() {
    let Some(sleep) = sleep_program() else {
        println!("SKIPPED: no sleep on PATH");
        return;
    };
    let dir = td_fs::TempDir::new("td-disks-identify").unwrap();
    let body = format!("'{}' 5 &\nexit 0", sleep.display());
    let Some(serve) = scripted(dir.path(), &body, Duration::from_millis(300)) else {
        return;
    };
    let started = Instant::now();
    let error = serve
        .identify_confined(device(dir.path(), "fat", &fat32(b"MY CAMERA  ")))
        .unwrap_err();
    assert!(error.contains("did not finish"), "{error}");
    assert!(started.elapsed() < Duration::from_secs(4), "held past it");
}

/// Closing the reply is not finishing: its status is still waited for, so an
/// answer cannot be believed before the worker that gave it has succeeded.
#[test]
fn a_worker_that_closes_its_reply_and_lingers_is_not_believed() {
    let Some(sleep) = sleep_program() else {
        println!("SKIPPED: no sleep on PATH");
        return;
    };
    let dir = td_fs::TempDir::new("td-disks-identify").unwrap();
    let body = format!(
        "printf 'ok vfat\\n'\nexec >&-\nexec '{}' 5",
        sleep.display()
    );
    let Some(serve) = scripted(dir.path(), &body, Duration::from_millis(300)) else {
        return;
    };
    let error = serve
        .identify_confined(device(dir.path(), "fat", &fat32(b"MY CAMERA  ")))
        .unwrap_err();
    assert!(error.contains("did not finish"), "{error}");
}

/// A worker past its deadline dies, so a stalled one holds no disk open.
#[test]
fn a_worker_past_its_deadline_is_killed() {
    let Some(sleep) = sleep_program() else {
        println!("SKIPPED: no sleep on PATH");
        return;
    };
    let dir = td_fs::TempDir::new("td-disks-identify").unwrap();
    let recorded = dir.path().join("pid");
    let body = format!(
        "printf '%s\\n' $$ > '{}'\nexec '{}' 30",
        recorded.display(),
        sleep.display()
    );
    // Long enough for the script to record itself on a loaded host.
    let Some(serve) = scripted(dir.path(), &body, Duration::from_secs(3)) else {
        return;
    };
    let error = serve
        .identify_confined(device(dir.path(), "fat", &fat32(b"MY CAMERA  ")))
        .unwrap_err();
    assert!(error.contains("did not finish"), "{error}");
    let pid = fs::read_to_string(&recorded).unwrap();
    let proc = PathBuf::from(format!("/proc/{}", pid.trim()));
    let started = Instant::now();
    while proc.exists() {
        assert!(started.elapsed() < Duration::from_secs(10), "still running");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A worker that never stops talking is cut off rather than buffered.
#[test]
fn a_worker_answering_without_end_is_cut_off() {
    let dir = td_fs::TempDir::new("td-disks-identify").unwrap();
    let Some(serve) = scripted(
        dir.path(),
        "while :; do printf 'ok vfat\\n'; done",
        Duration::from_secs(15),
    ) else {
        return;
    };
    let error = serve
        .identify_confined(device(dir.path(), "fat", &fat32(b"MY CAMERA  ")))
        .unwrap_err();
    assert!(error.contains("answered too much"), "{error}");
}

/// A good answer from a worker that then failed is not an answer.
#[test]
fn a_failed_worker_is_not_believed() {
    let dir = td_fs::TempDir::new("td-disks-identify").unwrap();
    let Some(serve) = scripted(
        dir.path(),
        "printf 'ok vfat\\n'\nexit 1",
        Duration::from_secs(15),
    ) else {
        return;
    };
    let error = serve
        .identify_confined(device(dir.path(), "fat", &fat32(b"MY CAMERA  ")))
        .unwrap_err();
    assert!(error.contains("failed"), "{error}");
}

#[test]
fn a_missing_worker_fails_the_request() {
    let dir = td_fs::TempDir::new("td-disks-identify").unwrap();
    let mut serve = serve();
    serve.worker = dir.path().join("absent");
    assert!(serve
        .identify_confined(device(dir.path(), "fat", &fat32(b"MY CAMERA  ")))
        .is_err());
}
