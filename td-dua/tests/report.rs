//! The cleanup report over a real directory.
#![forbid(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use td_dua::report::{self, Options, Reason, Report};
use td_dua::scan::{cachedir_tagged, scan, Progress};
use td_dua::tree::{Kind, Measure};

const TAG: &[u8] = b"Signature: 8a477f597d28d172789f06886806bc55\n# a cache\n";
const DAY: u64 = 24 * 60 * 60;

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("td-dua-report-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        Self(fs::canonicalize(path).unwrap())
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn write(root: &Path, relative: &str, bytes: usize) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, vec![7u8; bytes]).unwrap();
}

/// The clock the files were written by, as the report's `now`.
fn now() -> i64 {
    let since = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    i64::try_from(since.as_secs()).unwrap()
}

/// Sets a file's modification time `days` before now.
fn age(root: &Path, relative: &str, days: u64) {
    let file = fs::File::options()
        .write(true)
        .open(root.join(relative))
        .unwrap();
    let at = u64::try_from(now()).unwrap() - days * DAY;
    file.set_modified(UNIX_EPOCH + Duration::from_secs(at))
        .unwrap();
}

fn apparent() -> Options {
    Options {
        measure: Measure::Apparent,
        min_size: 1000,
        ..Options::default()
    }
}

fn build(root: &Path, options: Options) -> Report {
    let tree = scan(root, &Progress::default()).unwrap();
    report::build(&tree, options, now(), &mut cachedir_tagged)
}

fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap()
        .to_string_lossy()
        .into_owned()
}

/// A home with one of each candidate, a decoy tag, nested candidates and
/// ordinary files.
fn home(root: &Path) {
    write(root, "src/app/node_modules/left-pad/index.js", 30_000);
    write(root, "src/app/node_modules/.cache/x", 1_000);
    write(root, "src/app/main.js", 2_000);
    write(root, "src/py/__pycache__/m.pyc", 20_000);
    write(root, "src/rs/target/debug/app", 50_000);
    fs::write(root.join("src/rs/target/CACHEDIR.TAG"), TAG).unwrap();
    write(root, "src/fake/out.bin", 40_000);
    fs::write(root.join("src/fake/CACHEDIR.TAG"), b"Signature: wrong\n").unwrap();
    write(root, ".cache/thumbs/a.png", 10_000);
    write(root, ".local/share/Trash/files/old.iso", 60_000);
    write(root, "videos/big.mkv", 80_000);
    write(root, "docs/small.txt", 10);
}

#[test]
fn candidates_are_the_outermost_regenerable_directories_largest_first() {
    let scratch = Scratch::new("candidates");
    let root = &scratch.0;
    home(root);
    let report = build(root, apparent());
    let found: Vec<(String, Reason)> = report
        .candidates
        .iter()
        .map(|c| (relative(root, &c.entry.path), c.reason))
        .collect();
    assert_eq!(
        found,
        [
            (".local/share/Trash".into(), Reason::Trash),
            ("src/rs/target".into(), Reason::CacheTag),
            ("src/app/node_modules".into(), Reason::NodeModules),
            ("src/py/__pycache__".into(), Reason::PyCache),
            (".cache".into(), Reason::CacheDir),
        ]
    );
    // The .cache beneath node_modules is counted with it, not again.
    assert_eq!(report.candidate_count, 5);
    let node_modules = &report.candidates[2].entry;
    assert_eq!(node_modules.kind, Kind::Dir);
    assert_eq!(node_modules.files, 2);
    let sum: u64 = report.candidates.iter().map(|c| c.entry.bytes).sum();
    assert_eq!(report.candidate_bytes, sum);
    assert!(report.candidate_bytes >= 60_000 + 50_000 + 31_000 + 20_000 + 10_000);
}

#[test]
fn file_sections_skip_what_the_candidates_hold() {
    let scratch = Scratch::new("files");
    let root = &scratch.0;
    home(root);
    age(root, "videos/big.mkv", 400);
    age(root, "src/fake/out.bin", 10);
    age(root, "src/rs/target/debug/app", 900);
    age(root, ".local/share/Trash/files/old.iso", 900);
    let report = build(root, apparent());
    let largest: Vec<String> = report
        .largest_files
        .iter()
        .map(|e| relative(root, &e.path))
        .collect();
    // Under min_size: small.txt, CACHEDIR.TAG; inside candidates: the rest.
    assert_eq!(
        largest,
        ["videos/big.mkv", "src/fake/out.bin", "src/app/main.js"]
    );
    assert_eq!(report.largest_files[0].bytes, 80_000);
    assert_eq!(report.largest_files[0].files, 1);
    let stale: Vec<String> = report
        .stale_files
        .iter()
        .map(|e| relative(root, &e.path))
        .collect();
    assert_eq!(stale, ["videos/big.mkv"]);
    let fresher = build(
        root,
        Options {
            stale_days: 500,
            ..apparent()
        },
    );
    assert!(fresher.stale_files.is_empty());
}

#[test]
fn counts_cover_the_whole_tree_and_top_bounds_every_list() {
    let scratch = Scratch::new("counts");
    let root = &scratch.0;
    home(root);
    let report = build(root, apparent());
    let tree = scan(root, &Progress::default()).unwrap();
    assert_eq!(report.files, tree.root().unwrap().files);
    assert_eq!(report.bytes, tree.root().unwrap().total.apparent);
    // The root, src and its four project directories, node_modules and
    // left-pad and its .cache, __pycache__, target and debug, .cache and
    // thumbs, .local, share, Trash and files, videos, docs.
    assert_eq!(report.directories, 20);
    assert!(!report.partial);
    let top: Vec<String> = report
        .top_level
        .iter()
        .map(|e| relative(root, &e.path))
        .collect();
    assert_eq!(top[..2], ["src", "videos"]);
    assert_eq!(top.len(), 5);
    let one = build(
        root,
        Options {
            top: 1,
            ..apparent()
        },
    );
    assert_eq!(one.candidates.len(), 1);
    assert_eq!(one.candidate_count, 5);
    assert_eq!(one.largest_files.len(), 1);
    assert_eq!(one.top_level.len(), 1);
    let none = build(
        root,
        Options {
            top: 0,
            ..apparent()
        },
    );
    assert!(none.candidates.is_empty() && none.top_level.is_empty());
    assert_eq!(none.candidate_bytes, report.candidate_bytes);
}

#[test]
fn the_scanned_root_is_never_its_own_candidate() {
    let scratch = Scratch::new("root");
    let root = scratch.0.join(".cache");
    write(&root, "node_modules/x", 5_000);
    let report = build(&root, apparent());
    assert_eq!(report.candidates.len(), 1);
    assert_eq!(report.candidates[0].reason, Reason::NodeModules);
}

#[test]
fn json_is_one_document_with_every_section() {
    let scratch = Scratch::new("json");
    let root = &scratch.0;
    home(root);
    let report = build(root, apparent());
    let text = report.to_json().to_string();
    assert!(!text.contains('\n'));
    let json = td_json::parse(&text).unwrap();
    assert_eq!(json["root"].as_str(), root.to_str());
    assert_eq!(json["measure"].as_str(), Some("apparent"));
    assert_eq!(json["bytes"].as_u64(), Some(report.bytes));
    assert_eq!(json["candidate_count"].as_u64(), Some(5));
    let first = &json["candidates"][0];
    assert_eq!(first["reason"].as_str(), Some("trash"));
    assert_eq!(first["kind"].as_str(), Some("directory"));
    assert!(first["why"].as_str().is_some_and(|why| !why.is_empty()));
    assert!(first["modified"].as_str().is_some_and(|d| d.len() == 10));
    assert!(first.get("lossy").is_none());
    for section in [
        "largest_files",
        "stale_files",
        "top_level",
        "unreadable",
        "mounts",
    ] {
        assert!(json[section].as_array().is_some(), "{section}");
    }
    assert_eq!(json["unreadable_count"].as_u64(), Some(0));
}

#[test]
fn a_linked_file_says_so() {
    let scratch = Scratch::new("linked");
    let root = &scratch.0;
    write(root, "a", 5_000);
    fs::hard_link(root.join("a"), root.join("b")).unwrap();
    let report = build(root, apparent());
    // The inode's bytes belong to one name, which is the one listed.
    assert_eq!(report.largest_files.len(), 1);
    assert!(report.largest_files[0].linked);
    let json = report.to_json();
    assert!(json["largest_files"][0]["linked"].is_true());
    assert!(report.to_text().contains("(linked)"));
}

#[test]
fn text_keeps_one_entry_per_line() {
    let scratch = Scratch::new("text");
    let root = &scratch.0;
    write(root, "bad\nname", 5_000);
    write(root, "node_modules/x", 2_000);
    let text = build(root, apparent()).to_text();
    assert!(text.contains("bad?name"), "{text}");
    assert!(!text.contains("bad\nname"));
    assert!(text.contains("node-modules"));
    assert!(text.starts_with("td-dua report of "));
}

#[test]
fn a_tag_is_a_regular_file_with_the_signature() {
    let scratch = Scratch::new("tag");
    let root = &scratch.0;
    fs::write(root.join("good"), TAG).unwrap();
    fs::write(root.join("short"), &TAG[..20]).unwrap();
    fs::write(root.join("exact"), &TAG[..43]).unwrap();
    fs::write(
        root.join("wrong"),
        b"Signature: 8a477f597d28d172789f06886806bc56",
    )
    .unwrap();
    symlink(root.join("good"), root.join("link")).unwrap();
    fs::create_dir(root.join("dir")).unwrap();
    assert!(cachedir_tagged(&root.join("good")));
    assert!(cachedir_tagged(&root.join("exact")));
    for name in ["short", "wrong", "link", "dir", "missing"] {
        assert!(!cachedir_tagged(&root.join(name)), "{name}");
    }
}

#[test]
fn sizes_parse_in_binary_units() {
    assert_eq!(report::parse_size("0"), Some(0));
    assert_eq!(report::parse_size("1500"), Some(1500));
    assert_eq!(report::parse_size("4K"), Some(4096));
    assert_eq!(report::parse_size("2m"), Some(2 << 20));
    assert_eq!(report::parse_size("3G"), Some(3 << 30));
    assert_eq!(report::parse_size("1T"), Some(1 << 40));
    for bad in ["", "K", "1.5M", "-1", "1KB", " 1", "99999999999T"] {
        assert_eq!(report::parse_size(bad), None, "{bad:?}");
    }
}

#[test]
fn the_trash_is_found_from_any_scan_above_it() {
    let scratch = Scratch::new("trash");
    let root = &scratch.0;
    write(root, ".local/share/Trash/files/old", 5_000);
    for start in [root.clone(), root.join(".local"), root.join(".local/share")] {
        let report = build(&start, apparent());
        assert_eq!(report.candidates.len(), 1, "{}", start.display());
        assert_eq!(report.candidates[0].reason, Reason::Trash);
        assert!(report.largest_files.is_empty(), "{}", start.display());
    }
    // A directory named Trash elsewhere is not the trash.
    let elsewhere = Scratch::new("not-trash");
    write(&elsewhere.0, "share/Trash/x", 5_000);
    assert!(build(&elsewhere.0, apparent()).candidates.is_empty());
}

#[test]
fn equal_sizes_keep_the_first_paths_whatever_the_walk_order() {
    let scratch = Scratch::new("ties");
    let root = &scratch.0;
    for name in ["c", "a", "d", "b"] {
        write(root, name, 2_000);
    }
    let report = build(
        root,
        Options {
            top: 2,
            ..apparent()
        },
    );
    let kept: Vec<String> = report
        .largest_files
        .iter()
        .map(|e| relative(root, &e.path))
        .collect();
    assert_eq!(kept, ["a", "b"]);
}

#[test]
fn a_candidate_says_how_many_of_its_files_are_linked() {
    let scratch = Scratch::new("candidate-links");
    let root = &scratch.0;
    write(root, "app/node_modules/x/f", 4_000);
    write(root, "app/node_modules/x/g", 1_000);
    fs::hard_link(root.join("app/node_modules/x/f"), root.join("twin")).unwrap();
    let report = build(root, apparent());
    let candidate = &report.candidates[0];
    assert_eq!(candidate.linked_files, 1);
    assert_eq!(candidate.mounts, 0);
    let json = report.to_json();
    assert_eq!(json["candidates"][0]["linked_files"].as_u64(), Some(1));
    assert_eq!(json["candidates"][0]["mounts"].as_u64(), Some(0));
    assert!(report.to_text().contains("(1 file linked)"));
}

#[test]
fn json_carries_the_clock_and_the_stale_cutoff() {
    let scratch = Scratch::new("clock");
    write(&scratch.0, "a", 10);
    let before = now();
    let report = build(&scratch.0, apparent());
    let json = report.to_json();
    let at = json["now"].as_i64().unwrap();
    assert!(at >= before);
    let days = i64::try_from(Options::default().stale_days).unwrap();
    assert_eq!(json["stale_before"].as_i64(), Some(at - days * 86_400));
    assert!(!json["node_limit"].is_true());
    let text = report.to_text();
    assert!(text.contains("in 1 file and 1 directory."), "{text}");
    assert!(text.contains("0 B in 0 directories"), "{text}");
}

fn td_dua(args: &[&str]) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_td-dua"))
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn the_command_line_is_strict_and_writes_one_document() {
    let scratch = Scratch::new("cli");
    write(&scratch.0, "node_modules/x", 3_000);
    let dir = scratch.0.to_str().unwrap();
    let out = td_dua(&["report", "--json", "--apparent", "--top", "3", dir]);
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.ends_with('\n') && text.trim_end().lines().count() == 1);
    let json = td_json::parse(text.trim_end()).unwrap();
    assert_eq!(json["top"].as_u64(), Some(3));
    assert_eq!(
        json["candidates"][0]["reason"].as_str(),
        Some("node-modules")
    );
    let help = td_dua(&["report", "--help"]);
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains("td-dua report"));
    for bad in [
        &["report", "--top", "+2", dir][..],
        &["report", "--top", "10001", dir],
        &["report", "--stale-days", "-1", dir],
        &["report", "--min-size", "1.5M", dir],
        &["report", "--bogus", dir],
        &["report", dir, dir],
    ] {
        let out = td_dua(bad);
        assert!(!out.status.success(), "{bad:?}");
        assert!(out.stdout.is_empty(), "{bad:?}");
    }
}
