#![forbid(unsafe_code)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

//! The library: the sidecar grammar held to its refusals and its in-place
//! rewriting, the roll and dating rules, and the four headless verbs run
//! as the built binary over a temporary library, with the never-overwrite
//! and never-unlink oracles.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use td_photo::library::{self, Crop, Error, Filter, Flag, Key, Sidecar, SIDECAR_HEADER};

const SAMPLE: &str = "td-photo edit 1\nflag pick\nfuture 1 2 3\nexposure -0.33\ncrop 0.1000 0.0500 0.8000 0.9000\nlook classic-chrome\n";

#[test]
fn a_sidecar_round_trips_and_keeps_unknown_lines_in_place() {
    let mut sidecar = Sidecar::parse(SAMPLE.as_bytes()).unwrap();
    assert_eq!(sidecar.flag(), Some(Flag::Pick));
    assert_eq!(sidecar.exposure(), Some(-33));
    assert_eq!(
        sidecar.crop(),
        Some(Crop::new(1000, 500, 8000, 9000).unwrap())
    );
    assert_eq!(sidecar.look(), Some("classic-chrome"));
    // A file from before the history seeds one step per develop key, in
    // the file's order, written after the lines; the lines themselves
    // are kept as they were.
    let seeded = "step-1 on exposure -0.33\nstep-2 on crop 0.1000 0.0500 0.8000 0.9000\nstep-3 on look classic-chrome\n";
    assert_eq!(sidecar.text(), format!("{SAMPLE}{seeded}"));
    assert_eq!(sidecar.entries().count(), 5);
    assert_eq!(sidecar.steps().len(), 3);
    assert_eq!(
        sidecar.bytes() + SIDECAR_HEADER.len() + 1,
        sidecar.text().len()
    );
    // A set rewrites the key in place and adds a step; the unknown line
    // stays where it was.
    sidecar.set(Key::Exposure, Some("1.00")).unwrap();
    let stepped = format!("{seeded}step-4 on exposure 1.00\n");
    assert_eq!(
        sidecar.text(),
        format!(
            "{}{stepped}",
            SAMPLE.replace("exposure -0.33", "exposure 1.00")
        )
    );
    sidecar.set(Key::Flag, None).unwrap();
    assert_eq!(
        sidecar.text(),
        format!(
            "{}{stepped}",
            SAMPLE
                .replace("flag pick\n", "")
                .replace("exposure -0.33", "exposure 1.00")
        )
    );
    // An absent key is appended, before the history.
    sidecar.set(Key::Flag, Some("reject")).unwrap();
    assert!(sidecar
        .text()
        .ends_with(&format!("look classic-chrome\nflag reject\n{stepped}")));
    // Reset keeps the cull decision and what it does not know.
    sidecar.reset();
    assert_eq!(
        sidecar.text(),
        "td-photo edit 1\nfuture 1 2 3\nflag reject\n"
    );
    // The grammar that guards a parse guards a set.
    assert_eq!(
        sidecar.set(Key::Exposure, Some("5.01")),
        Err(Error::Value(Key::Exposure))
    );
    assert_eq!(
        sidecar.set(Key::Look, Some("../x")),
        Err(Error::Value(Key::Look))
    );
    assert_eq!(sidecar.set(Key::Crop, None), Ok(()));
    // Without a trailing newline, and empty, both parse.
    assert_eq!(
        Sidecar::parse(b"td-photo edit 1\nflag pick")
            .unwrap()
            .flag(),
        Some(Flag::Pick)
    );
    assert_eq!(
        Sidecar::parse(b"td-photo edit 1\n").unwrap(),
        Sidecar::default()
    );
    assert_eq!(Sidecar::default().text(), "td-photo edit 1\n");
}

#[test]
fn a_sidecar_is_refused_as_a_whole_at_the_first_fault() {
    let head = "td-photo edit 1\n";
    let mut too_many = head.to_string();
    for i in 0..library::MAX_SIDECAR_LINES {
        too_many.push_str(&format!("k{i} v\n"));
    }
    let cases: Vec<(Vec<u8>, Error)> = vec![
        (vec![b'x'; library::MAX_SIDECAR_BYTES + 1], Error::TooLong),
        (too_many.into_bytes(), Error::TooManyLines),
        (b"td-photo edit 1\nflag \xff\n".to_vec(), Error::Utf8),
        (b"td-photo edit 2\n".to_vec(), Error::Header),
        (Vec::new(), Error::Header),
        (format!("{head}\nflag pick\n").into_bytes(), Error::Line(2)),
        (format!("{head}flag\n").into_bytes(), Error::Line(2)),
        (format!("{head}flag  pick\n").into_bytes(), Error::Line(2)),
        (format!("{head}flag pick \n").into_bytes(), Error::Line(2)),
        (format!("{head}Flag pick\n").into_bytes(), Error::Line(2)),
        (
            format!("{head}flag pick\nlook a\tb\n").into_bytes(),
            Error::Line(3),
        ),
        (
            format!("{head}flag maybe\n").into_bytes(),
            Error::Value(Key::Flag),
        ),
        (
            format!("{head}exposure 5.01\n").into_bytes(),
            Error::Value(Key::Exposure),
        ),
        (
            format!("{head}exposure -0.00\n").into_bytes(),
            Error::Value(Key::Exposure),
        ),
        (
            format!("{head}exposure 1.5\n").into_bytes(),
            Error::Value(Key::Exposure),
        ),
        (
            format!("{head}exposure +1.00\n").into_bytes(),
            Error::Value(Key::Exposure),
        ),
        (
            format!("{head}exposure 10.00\n").into_bytes(),
            Error::Value(Key::Exposure),
        ),
        (
            format!("{head}crop 0.1000 0.1000 0.9500 0.5000\n").into_bytes(),
            Error::Value(Key::Crop),
        ),
        (
            format!("{head}crop 0 0 1 1\n").into_bytes(),
            Error::Value(Key::Crop),
        ),
        (
            format!("{head}crop 0.0000 0.0000 0.0400 0.5000\n").into_bytes(),
            Error::Value(Key::Crop),
        ),
        (
            format!("{head}crop 0.0000 0.0000 0.5000 0.5000 0.5000\n").into_bytes(),
            Error::Value(Key::Crop),
        ),
        (
            format!("{head}look ../x\n").into_bytes(),
            Error::Value(Key::Look),
        ),
        (
            format!("{head}look .hidden\n").into_bytes(),
            Error::Value(Key::Look),
        ),
        (
            format!("{head}look {}\n", "a".repeat(65)).into_bytes(),
            Error::Value(Key::Look),
        ),
        (
            format!("{head}flag pick\nflag reject\n").into_bytes(),
            Error::Repeated(Key::Flag),
        ),
    ];
    for (bytes, expected) in cases {
        let shown = String::from_utf8_lossy(bytes.get(..40).unwrap_or(&bytes)).into_owned();
        assert_eq!(Sidecar::parse(&bytes), Err(expected), "{shown:?}");
    }
    // The line ceiling admits exactly the ceiling, the header counted.
    let mut full = head.to_string();
    for i in 0..library::MAX_SIDECAR_LINES - 1 {
        full.push_str(&format!("k{i} v\n"));
    }
    assert_eq!(
        Sidecar::parse(full.as_bytes()).unwrap().entries().count(),
        library::MAX_SIDECAR_LINES - 1
    );
    assert_eq!(Error::Value(Key::Crop).to_string(), "malformed crop value");
    assert_eq!(Error::Line(7).to_string(), "line 7 is not `key value`");
    assert_eq!(Error::Repeated(Key::Look).to_string(), "look given twice");
}

#[test]
fn values_print_canonically() {
    assert_eq!(library::exposure_text(-33), "-0.33");
    assert_eq!(library::exposure_text(500), "5.00");
    assert_eq!(library::exposure_text(0), "0.00");
    assert_eq!(library::exposure_text(7), "0.07");
    assert_eq!(library::exposure("-5.00"), Ok(-500));
    assert_eq!(library::exposure("0.00"), Ok(0));
    assert_eq!(
        Crop::new(1000, 500, 8000, 9000).unwrap().text(),
        "0.1000 0.0500 0.8000 0.9000"
    );
    assert_eq!(
        Crop::parse("0.0000 0.0000 1.0000 1.0000"),
        Crop::new(0, 0, 10_000, 10_000)
    );
    assert_eq!(
        Crop::new(9500, 0, 500, 500).map(Crop::text),
        Ok("0.9500 0.0000 0.0500 0.0500".to_string())
    );
    assert_eq!(Crop::new(9501, 0, 500, 500), Err(Error::Value(Key::Crop)));
    assert!(library::valid_look("classic-chrome_2.v1"));
    assert!(!library::valid_look(""));
    assert!(!library::valid_look("a/b"));
    assert_eq!(Key::parse("look"), Some(Key::Look));
    assert_eq!(Key::parse("Look"), None);
}

#[test]
fn rolls_list_originals_and_filter_by_flag() {
    for (name, original) in [
        ("DSC_0001.NEF", true),
        ("dsc_0001.nef", true),
        (".nef", false),
        ("x.NEF.part", false),
        (".x.nef", false),
        ("x.jpg", false),
        ("x.Nef", false),
        ("x.NEF.edit", false),
        ("a b.nef", true),
        ("a\tb.NEF", false),
        ("a\nb.nef", false),
    ] {
        assert_eq!(library::is_original(name), original, "{name}");
    }
    assert_eq!(library::sidecar_name("DSC_0001.NEF"), "DSC_0001.NEF.edit");
    for (filter, none, pick, reject) in [
        (Filter::All, true, true, true),
        (Filter::Picks, false, true, false),
        (Filter::Rejects, false, false, true),
        (Filter::Unflagged, true, false, false),
    ] {
        assert_eq!(filter.admits(None), none, "{filter:?}");
        assert_eq!(filter.admits(Some(Flag::Pick)), pick, "{filter:?}");
        assert_eq!(filter.admits(Some(Flag::Reject)), reject, "{filter:?}");
    }
}

#[test]
fn an_import_is_dated_by_the_capture_time_or_not_at_all() {
    assert_eq!(library::roll_folder(None), "undated");
    assert_eq!(
        library::roll_folder(Some("2026:09:13 22:10:05")),
        "2026/2026-09-13"
    );
    assert_eq!(
        library::roll_folder(Some("2024:02:29 00:00:00")),
        "2024/2024-02-29"
    );
    for bad in [
        "    :  :     :  :  ",
        "2026:13:01 00:00:00",
        "2026:09:00 00:00:00",
        "1899:12:31 00:00:00",
        "2026-09-13 22:10:05",
        "",
        "2026:9:13 22:10:05",
        "2026:09:13",
        "2026:09:13 22:10:05x",
        "2026:09:13GARBAGE!!",
        "2026:09:13T22:10:05",
        "2026:09:31 00:00:00",
        "2023:02:29 00:00:00",
        "1900:02:29 00:00:00",
        "2026:09:13 24:00:00",
        "2026:09:13 22:60:00",
        "2026:09:13 22:10:60",
    ] {
        assert_eq!(library::roll_folder(Some(bad)), "undated", "{bad:?}");
    }
    let dated = tiff_with_date("2026:09:13 22:10:05");
    assert_eq!(
        library::taken(&dated).as_deref(),
        Some("2026:09:13 22:10:05")
    );
    assert_eq!(library::taken(b"not a tiff"), None);
    assert_eq!(library::taken(&tiff_without_exif()), None);
}

/// A little-endian TIFF whose IFD0 points at an Exif IFD holding
/// `DateTimeOriginal` and nothing else, so it is not a NEF.
fn tiff_with_date(date: &str) -> Vec<u8> {
    let mut out = b"II\x2a\x00\x08\x00\x00\x00".to_vec();
    // IFD0 at 8: one LONG entry, the Exif IFD's offset; 8 + 2 + 12 + 4 = 26.
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&34665u16.to_le_bytes());
    out.extend_from_slice(&4u16.to_le_bytes());
    out.extend_from_slice(&1u32.to_le_bytes());
    out.extend_from_slice(&26u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    // Exif IFD at 26: one ASCII entry, the string at 44; 26 + 2 + 12 + 4.
    let text = format!("{date}\0");
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&36867u16.to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&(text.len() as u32).to_le_bytes());
    out.extend_from_slice(&44u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(text.as_bytes());
    out
}

fn tiff_without_exif() -> Vec<u8> {
    let mut out = b"II\x2a\x00\x08\x00\x00\x00".to_vec();
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&256u16.to_le_bytes());
    out.extend_from_slice(&3u16.to_le_bytes());
    out.extend_from_slice(&1u32.to_le_bytes());
    out.extend_from_slice(&1u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out
}

struct Temp(PathBuf);

impl Temp {
    fn new(tag: &str) -> Temp {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "td-photo-library-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        Temp(path)
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn td_photo(args: &[&str]) -> (bool, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_td-photo"))
        .args(args)
        .output()
        .unwrap();
    (
        output.status.success(),
        String::from_utf8(output.stdout).unwrap(),
        String::from_utf8(output.stderr).unwrap(),
    )
}

fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn import_files_by_date_skips_identical_copies_and_never_overwrites() {
    let temp = Temp::new("import");
    let card = temp.0.join("card/DCIM/100NIKON");
    fs::create_dir_all(&card).unwrap();
    let dated = tiff_with_date("2026:09:13 22:10:05");
    let undated: Vec<u8> = (0..5000u32).map(|i| (i * 7 % 251) as u8).collect();
    fs::write(card.join("DSC_0001.NEF"), &dated).unwrap();
    fs::write(card.join("DSC_0002.nef"), &undated).unwrap();
    fs::write(card.join("NOTES.TXT"), b"not a photo").unwrap();
    // Eight folders deep is read, nine is not, and no link under the
    // source is followed, to a folder or to a file.
    let deep = temp.0.join("card/1/2/3/4/5/6/7/8");
    fs::create_dir_all(deep.join("9")).unwrap();
    fs::write(deep.join("DSC_0008.NEF"), b"eight deep").unwrap();
    fs::write(deep.join("9/DSC_0009.NEF"), b"nine deep").unwrap();
    let elsewhere = temp.0.join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();
    fs::write(elsewhere.join("DSC_0010.NEF"), b"elsewhere").unwrap();
    std::os::unix::fs::symlink(&elsewhere, card.join("linked")).unwrap();
    std::os::unix::fs::symlink(elsewhere.join("DSC_0010.NEF"), card.join("DSC_0011.NEF")).unwrap();
    let src = temp.0.join("card");
    let library = temp.0.join("library");
    let (src_s, library_s) = (src.to_str().unwrap(), library.to_str().unwrap());
    let (ok, out, err) = td_photo(&["import", src_s, library_s]);
    assert!(ok, "{err}");
    let first = library.join("2026/2026-09-13/DSC_0001.NEF");
    let second = library.join("undated/DSC_0002.nef");
    assert_eq!(fs::read(&first).unwrap(), dated);
    assert_eq!(fs::read(&second).unwrap(), undated);
    assert_eq!(
        fs::read(library.join("undated/DSC_0008.NEF")).unwrap(),
        b"eight deep"
    );
    // No temporary is left, and the card is read, never written.
    assert_eq!(names(&library.join("2026/2026-09-13")), ["DSC_0001.NEF"]);
    assert_eq!(
        names(&library.join("undated")),
        ["DSC_0002.nef", "DSC_0008.NEF"]
    );
    assert_eq!(
        names(&card),
        [
            "DSC_0001.NEF",
            "DSC_0002.nef",
            "DSC_0011.NEF",
            "NOTES.TXT",
            "linked"
        ]
    );
    assert_eq!(fs::read(card.join("DSC_0001.NEF")).unwrap(), dated);
    assert!(
        out.contains(&format!("imported {}\n", first.display())),
        "{out}"
    );
    assert!(
        out.ends_with("imported 3, skipped 0, conflicts 0, unread 0\n"),
        "{out}"
    );
    // Again: identical copies are skipped and counted.
    let (ok, out, _) = td_photo(&["import", src_s, library_s]);
    assert!(ok);
    assert!(
        out.ends_with("imported 0, skipped 3, conflicts 0, unread 0\n"),
        "{out}"
    );
    // A copy that differs is reported, left alone, and fails the run
    // after everything else was done.
    let mut changed = undated.clone();
    changed.push(1);
    fs::write(&second, &changed).unwrap();
    let (ok, out, err) = td_photo(&["import", src_s, library_s]);
    assert!(!ok);
    assert!(
        out.contains(&format!(
            "conflict {}: differs; source {}\n",
            second.display(),
            card.join("DSC_0002.nef").display()
        )),
        "{out}"
    );
    assert!(
        out.ends_with("imported 0, skipped 2, conflicts 1, unread 0\n"),
        "{out}"
    );
    assert!(err.contains("1 conflict"), "{err}");
    assert_eq!(fs::read(&second).unwrap(), changed);
    assert_eq!(
        names(&library.join("undated")),
        ["DSC_0002.nef", "DSC_0008.NEF"]
    );
    // A folder in a copy's place and a NAME.part left by an earlier run
    // are conflicts too, reported with why, and neither is touched.
    fs::write(card.join("DSC_0000.NEF"), &dated).unwrap();
    fs::write(card.join("DSC_0003.nef"), b"three").unwrap();
    let stale = library.join("2026/2026-09-13/DSC_0000.NEF.part");
    fs::write(&stale, b"stale").unwrap();
    fs::create_dir(library.join("undated/DSC_0003.nef")).unwrap();
    let (ok, out, _) = td_photo(&["import", src_s, library_s]);
    assert!(!ok);
    assert!(
        out.contains(&format!(
            "conflict {}: {} is in the way; source {}\n",
            library.join("2026/2026-09-13/DSC_0000.NEF").display(),
            stale.display(),
            card.join("DSC_0000.NEF").display()
        )),
        "{out}"
    );
    assert!(
        out.contains(&format!(
            "conflict {}: not a regular file; source {}\n",
            library.join("undated/DSC_0003.nef").display(),
            card.join("DSC_0003.nef").display()
        )),
        "{out}"
    );
    assert!(
        out.ends_with("imported 0, skipped 2, conflicts 3, unread 0\n"),
        "{out}"
    );
    assert_eq!(fs::read(&stale).unwrap(), b"stale");
    assert!(library.join("undated/DSC_0003.nef").is_dir());
    assert_eq!(
        names(&library.join("2026/2026-09-13")),
        ["DSC_0000.NEF.part", "DSC_0001.NEF"]
    );
    // The source itself may be a link.
    let link = temp.0.join("cardlink");
    std::os::unix::fs::symlink(&src, &link).unwrap();
    let fresh = temp.0.join("fresh");
    let (ok, out, err) = td_photo(&["import", link.to_str().unwrap(), fresh.to_str().unwrap()]);
    assert!(ok, "{err}");
    assert!(
        out.ends_with("imported 5, skipped 0, conflicts 0, unread 0\n"),
        "{out}"
    );
    // A source that is missing, or not a folder, and a missing DEST.
    assert!(
        !td_photo(&[
            "import",
            temp.0.join("nothing").to_str().unwrap(),
            library_s
        ])
        .0
    );
    assert!(
        !td_photo(&[
            "import",
            card.join("NOTES.TXT").to_str().unwrap(),
            library_s
        ])
        .0
    );
    assert!(!td_photo(&["import", src_s]).0);
}

/// Every regular file under `dir`, by path, for the count that shows
/// nothing was unlinked by a move.
fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let kind = entry.file_type().unwrap();
        if kind.is_dir() {
            out.extend(files_under(&entry.path()));
        } else if kind.is_file() {
            out.push(entry.path());
        }
    }
    out.sort();
    out
}

#[test]
fn delete_rejected_moves_rejects_with_their_sidecars_and_unlinks_nothing() {
    let temp = Temp::new("delete");
    let roll = temp.0.join("roll");
    fs::create_dir_all(roll.join("rejected")).unwrap();
    // A reject with an unknown line to carry, a pick, a reject whose
    // sidecar's name is taken in `rejected/`, a refused sidecar that says
    // reject (not a reject the reader can vouch for), and no sidecar.
    for i in 1..=5 {
        fs::write(roll.join(format!("DSC_000{i}.NEF")), format!("photo {i}")).unwrap();
    }
    let first = "td-photo edit 1\nflag reject\nfuture 1 2 3\nexposure -0.33\n";
    fs::write(roll.join("DSC_0001.NEF.edit"), first).unwrap();
    fs::write(
        roll.join("DSC_0002.NEF.edit"),
        "td-photo edit 1\nflag pick\n",
    )
    .unwrap();
    fs::write(
        roll.join("DSC_0003.NEF.edit"),
        "td-photo edit 1\nflag reject\n",
    )
    .unwrap();
    fs::write(roll.join("rejected/DSC_0003.NEF.edit"), "taken").unwrap();
    fs::write(
        roll.join("DSC_0004.NEF.edit"),
        "td-photo edit 1\nflag reject\nexposure bad\n",
    )
    .unwrap();
    let roll_s = roll.to_str().unwrap();
    let before = files_under(&roll);
    let (ok, out, err) = td_photo(&["delete-rejected", roll_s]);
    assert!(!ok);
    assert_eq!(
        out,
        format!(
            "moved DSC_0001.NEF\nkept DSC_0003.NEF: {}: already exists\n",
            roll.join("rejected/DSC_0003.NEF.edit").display()
        )
    );
    assert!(err.contains("1 reject(s) not moved"), "{err}");
    // The reject and its sidecar are in `rejected/`, byte for byte; the
    // rest, the kept reject's sidecar included, are where they were; and
    // as many files as before are under the roll.
    assert_eq!(
        fs::read(roll.join("rejected/DSC_0001.NEF")).unwrap(),
        b"photo 1"
    );
    assert_eq!(
        fs::read_to_string(roll.join("rejected/DSC_0001.NEF.edit")).unwrap(),
        first
    );
    assert_eq!(
        names(&roll),
        [
            "DSC_0002.NEF",
            "DSC_0002.NEF.edit",
            "DSC_0003.NEF",
            "DSC_0003.NEF.edit",
            "DSC_0004.NEF",
            "DSC_0004.NEF.edit",
            "DSC_0005.NEF",
            "rejected"
        ]
    );
    assert_eq!(files_under(&roll).len(), before.len());
    assert_eq!(
        td_photo(&["list", roll_s, "--rejects"]).1,
        "DSC_0003.NEF\treject\t-\t-\t-\tok\n"
    );
    // Asked again, the kept one is kept again and nothing else moves.
    let (ok, out, _) = td_photo(&["delete-rejected", roll_s]);
    assert!(!ok);
    assert!(out.starts_with("kept DSC_0003.NEF: "), "{out}");
    assert_eq!(files_under(&roll).len(), before.len());
    // A roll without a reject the reader can vouch for: nothing said,
    // nothing made, a refused sidecar that says reject and a link at an
    // original's name flagged reject both left where they are, unmentioned.
    let quiet = temp.0.join("quiet");
    fs::create_dir_all(&quiet).unwrap();
    fs::write(quiet.join("DSC_0001.NEF"), b"x").unwrap();
    fs::write(quiet.join("DSC_0002.NEF"), b"y").unwrap();
    fs::write(
        quiet.join("DSC_0002.NEF.edit"),
        "td-photo edit 1\nflag reject\nexposure bad\n",
    )
    .unwrap();
    std::os::unix::fs::symlink(quiet.join("DSC_0001.NEF"), quiet.join("DSC_0003.NEF")).unwrap();
    fs::write(
        quiet.join("DSC_0003.NEF.edit"),
        "td-photo edit 1\nflag reject\n",
    )
    .unwrap();
    let (ok, out, err) = td_photo(&["delete-rejected", quiet.to_str().unwrap()]);
    assert!(ok, "{err}");
    assert_eq!(out, "");
    assert_eq!(
        names(&quiet),
        [
            "DSC_0001.NEF",
            "DSC_0002.NEF",
            "DSC_0002.NEF.edit",
            "DSC_0003.NEF",
            "DSC_0003.NEF.edit"
        ]
    );
    // The original's name taken in `rejected/` keeps the photo as the
    // sidecar's does; a stale sidecar temporary keeps it too; and a move
    // interrupted after the link, the original under both names, is
    // finished, its sidecar following.
    let held = temp.0.join("held");
    fs::create_dir_all(held.join("rejected")).unwrap();
    for i in 1..=3 {
        fs::write(held.join(format!("DSC_000{i}.NEF")), format!("held {i}")).unwrap();
        fs::write(
            held.join(format!("DSC_000{i}.NEF.edit")),
            "td-photo edit 1\nflag reject\n",
        )
        .unwrap();
    }
    fs::write(held.join("rejected/DSC_0001.NEF"), b"taken").unwrap();
    fs::write(held.join("DSC_0002.NEF.edit.tmp"), b"stale").unwrap();
    fs::hard_link(
        held.join("DSC_0003.NEF"),
        held.join("rejected/DSC_0003.NEF"),
    )
    .unwrap();
    let held_s = held.to_str().unwrap();
    let (ok, out, _) = td_photo(&["delete-rejected", held_s]);
    assert!(!ok);
    assert_eq!(
        out,
        format!(
            "kept DSC_0001.NEF: {}: already exists\nkept DSC_0002.NEF: {}: stale temporary in the way\nmoved DSC_0003.NEF\n",
            held.join("rejected/DSC_0001.NEF").display(),
            held.join("DSC_0002.NEF.edit.tmp").display()
        )
    );
    assert_eq!(
        fs::read(held.join("rejected/DSC_0001.NEF")).unwrap(),
        b"taken"
    );
    assert_eq!(fs::read(held.join("DSC_0001.NEF")).unwrap(), b"held 1");
    assert!(held.join("DSC_0001.NEF.edit").is_file());
    assert!(held.join("DSC_0002.NEF").is_file() && held.join("DSC_0002.NEF.edit").is_file());
    assert_eq!(
        fs::read(held.join("DSC_0002.NEF.edit.tmp")).unwrap(),
        b"stale"
    );
    assert!(!held.join("DSC_0003.NEF").exists() && !held.join("DSC_0003.NEF.edit").exists());
    assert_eq!(
        fs::read(held.join("rejected/DSC_0003.NEF")).unwrap(),
        b"held 3"
    );
    assert!(held.join("rejected/DSC_0003.NEF.edit").is_file());
    // A link at `rejected` is refused by name before anything moves.
    let linked = temp.0.join("linked");
    fs::create_dir_all(&linked).unwrap();
    fs::write(linked.join("DSC_0001.NEF"), b"x").unwrap();
    fs::write(
        linked.join("DSC_0001.NEF.edit"),
        "td-photo edit 1\nflag reject\n",
    )
    .unwrap();
    std::os::unix::fs::symlink(&quiet, linked.join("rejected")).unwrap();
    let (ok, _, err) = td_photo(&["delete-rejected", linked.to_str().unwrap()]);
    assert!(!ok);
    assert!(err.contains("not a directory"), "{err}");
    assert_eq!(
        names(&linked),
        ["DSC_0001.NEF", "DSC_0001.NEF.edit", "rejected"]
    );
    assert_eq!(names(&quiet).len(), 5);
    // A roll that is not there, and the argument's shape.
    assert!(!td_photo(&["delete-rejected", temp.0.join("none").to_str().unwrap()]).0);
    let (ok, _, err) = td_photo(&["delete-rejected"]);
    assert!(!ok);
    assert!(err.contains("needs ROLL"), "{err}");
}

#[test]
fn list_flag_and_edit_go_through_the_sidecar_and_unlink_nothing() {
    let temp = Temp::new("edit");
    let roll = temp.0.join("2026/2026-09-13");
    fs::create_dir_all(roll.join("rejected")).unwrap();
    let photo = roll.join("DSC_0001.NEF");
    fs::write(&photo, tiff_with_date("2026:09:13 22:10:05")).unwrap();
    fs::write(roll.join("DSC_0002.NEF"), b"x").unwrap();
    fs::write(roll.join("notes.txt"), b"x").unwrap();
    fs::write(roll.join("DSC_0003.NEF.part"), b"x").unwrap();
    // A name with a control character is not an original: it would
    // break the listing's rows.
    fs::write(roll.join("DSC\t0004.NEF"), b"x").unwrap();
    let (roll_s, photo_s) = (roll.to_str().unwrap(), photo.to_str().unwrap());
    let (ok, out, err) = td_photo(&["list", roll_s]);
    assert!(ok, "{err}");
    assert_eq!(
        out,
        "DSC_0001.NEF\t-\t-\t-\t-\tnone\nDSC_0002.NEF\t-\t-\t-\t-\tnone\n"
    );
    let sidecar = roll.join("DSC_0001.NEF.edit");
    assert!(td_photo(&["flag", photo_s, "pick"]).0);
    assert_eq!(
        fs::read_to_string(&sidecar).unwrap(),
        "td-photo edit 1\nflag pick\n"
    );
    // A stale temporary is reported, never reused or removed.
    let stale = roll.join("DSC_0001.NEF.edit.tmp");
    fs::write(&stale, b"stale").unwrap();
    let (ok, _, err) = td_photo(&["flag", photo_s, "reject"]);
    assert!(!ok);
    assert!(err.contains("DSC_0001.NEF.edit.tmp"), "{err}");
    assert_eq!(fs::read(&stale).unwrap(), b"stale");
    assert_eq!(
        fs::read_to_string(&sidecar).unwrap(),
        "td-photo edit 1\nflag pick\n"
    );
    fs::remove_file(&stale).unwrap();
    assert!(
        td_photo(&[
            "edit",
            photo_s,
            "exposure",
            "-0.33",
            "look",
            "classic-chrome",
            "crop",
            "0.1000",
            "0.0500",
            "0.8000",
            "0.9000",
        ])
        .0
    );
    let steps = "step-1 on exposure -0.33\nstep-2 on look classic-chrome\nstep-3 on crop 0.1000 0.0500 0.8000 0.9000\n";
    let text = format!("td-photo edit 1\nflag pick\nexposure -0.33\nlook classic-chrome\ncrop 0.1000 0.0500 0.8000 0.9000\n{steps}");
    assert_eq!(fs::read_to_string(&sidecar).unwrap(), text);
    assert_eq!(td_photo(&["edit", photo_s]).1, text);
    assert_eq!(
        td_photo(&["list", roll_s, "--picks"]).1,
        "DSC_0001.NEF\tpick\t-0.33\t0.1000 0.0500 0.8000 0.9000\tclassic-chrome\tok\n"
    );
    assert_eq!(td_photo(&["list", roll_s, "--rejects"]).1, "");
    assert_eq!(
        td_photo(&["list", roll_s, "--unflagged"]).1,
        "DSC_0002.NEF\t-\t-\t-\t-\tnone\n"
    );
    assert!(!td_photo(&["list", roll_s, "--picks", "--rejects"]).0);
    // An unknown line survives, in place, a flag change and a reset.
    fs::write(
        &sidecar,
        text.replace("exposure -0.33\nlook", "exposure -0.33\nfuture 1 2 3\nlook"),
    )
    .unwrap();
    assert!(td_photo(&["flag", photo_s, "reject"]).0);
    assert_eq!(
        fs::read_to_string(&sidecar).unwrap(),
        format!("td-photo edit 1\nflag reject\nexposure -0.33\nfuture 1 2 3\nlook classic-chrome\ncrop 0.1000 0.0500 0.8000 0.9000\n{steps}")
    );
    // Undo takes the last step back and rewrites the keys from the rest.
    assert!(td_photo(&["edit", photo_s, "undo"]).0);
    assert_eq!(
        fs::read_to_string(&sidecar).unwrap(),
        "td-photo edit 1\nflag reject\nexposure -0.33\nfuture 1 2 3\nlook classic-chrome\nstep-1 on exposure -0.33\nstep-2 on look classic-chrome\n"
    );
    assert!(td_photo(&["edit", photo_s, "reset"]).0);
    assert_eq!(
        fs::read_to_string(&sidecar).unwrap(),
        "td-photo edit 1\nflag reject\nfuture 1 2 3\n"
    );
    assert!(td_photo(&["edit", photo_s, "exposure", "-"]).0);
    assert!(td_photo(&["flag", photo_s, "clear"]).0);
    assert_eq!(
        fs::read_to_string(&sidecar).unwrap(),
        "td-photo edit 1\nfuture 1 2 3\n"
    );
    // A bad value, key or shape is refused before anything is written.
    let (ok, _, err) = td_photo(&["edit", photo_s, "exposure", "9.00"]);
    assert!(!ok);
    assert!(err.contains("malformed exposure value"), "{err}");
    assert!(!td_photo(&["edit", photo_s, "colour", "red"]).0);
    assert!(!td_photo(&["edit", photo_s, "crop", "0.1000"]).0);
    assert!(!td_photo(&["edit", photo_s, "exposure"]).0);
    assert!(!td_photo(&["flag", photo_s, "maybe"]).0);
    assert_eq!(
        fs::read_to_string(&sidecar).unwrap(),
        "td-photo edit 1\nfuture 1 2 3\n"
    );
    // A malformed sidecar is shown as an error and never rewritten.
    fs::write(&sidecar, "td-photo edit 1\nexposure 7.00\n").unwrap();
    assert_eq!(
        td_photo(&["list", roll_s]).1,
        "DSC_0001.NEF\t-\t-\t-\t-\terror malformed exposure value\nDSC_0002.NEF\t-\t-\t-\t-\tnone\n"
    );
    let (ok, _, err) = td_photo(&["flag", photo_s, "pick"]);
    assert!(!ok);
    assert!(err.contains("malformed exposure value"), "{err}");
    assert_eq!(
        fs::read_to_string(&sidecar).unwrap(),
        "td-photo edit 1\nexposure 7.00\n"
    );
    // The ceilings hold through the file: a sidecar at 1024 lines or
    // 64 KiB is read, one past either is refused, and an edit that would
    // take it past is refused before anything is written.
    let full: String = std::iter::once("td-photo edit 1\n".to_string())
        .chain((0..1023).map(|i| format!("k{i} v\n")))
        .collect();
    fs::write(&sidecar, &full).unwrap();
    assert!(td_photo(&["list", roll_s])
        .1
        .starts_with("DSC_0001.NEF\t-\t-\t-\t-\tok\n"));
    let (ok, _, err) = td_photo(&["flag", photo_s, "pick"]);
    assert!(!ok);
    assert!(err.contains("over 1024 lines"), "{err}");
    assert_eq!(fs::read_to_string(&sidecar).unwrap(), full);
    let long = |value: usize| format!("td-photo edit 1\nk {}\n", "v".repeat(value));
    fs::write(&sidecar, long(65536 - 19)).unwrap();
    assert!(td_photo(&["list", roll_s])
        .1
        .starts_with("DSC_0001.NEF\t-\t-\t-\t-\tok\n"));
    fs::write(&sidecar, long(65537 - 19)).unwrap();
    assert!(td_photo(&["list", roll_s])
        .1
        .starts_with("DSC_0001.NEF\t-\t-\t-\t-\terror sidecar over 65536 bytes\n"));
    // A sidecar's name that is not a regular file is refused unopened,
    // shown as an error, and never replaced.
    let linked = roll.join("DSC_0002.NEF.edit");
    std::os::unix::fs::symlink(roll.join("notes.txt"), &linked).unwrap();
    assert!(td_photo(&["list", roll_s])
        .1
        .ends_with("DSC_0002.NEF\t-\t-\t-\t-\terror not a regular file\n"));
    let two = roll.join("DSC_0002.NEF");
    let (ok, _, err) = td_photo(&["flag", two.to_str().unwrap(), "pick"]);
    assert!(!ok);
    assert!(err.contains("not a regular file"), "{err}");
    assert!(fs::symlink_metadata(&linked)
        .unwrap()
        .file_type()
        .is_symlink());
    // Nothing but the sidecar was written and nothing was unlinked.
    assert_eq!(
        names(&roll),
        [
            "DSC\t0004.NEF",
            "DSC_0001.NEF",
            "DSC_0001.NEF.edit",
            "DSC_0002.NEF",
            "DSC_0002.NEF.edit",
            "DSC_0003.NEF.part",
            "notes.txt",
            "rejected",
        ]
    );
    // Only an original that is there takes a sidecar.
    assert!(!td_photo(&["flag", roll.join("DSC_0009.NEF").to_str().unwrap(), "pick"]).0);
    assert!(!td_photo(&["flag", roll.join("notes.txt").to_str().unwrap(), "pick"]).0);
    assert!(!td_photo(&["flag", roll.join("DSC\t0004.NEF").to_str().unwrap(), "pick"]).0);
    assert_eq!(
        names(&roll),
        [
            "DSC\t0004.NEF",
            "DSC_0001.NEF",
            "DSC_0001.NEF.edit",
            "DSC_0002.NEF",
            "DSC_0002.NEF.edit",
            "DSC_0003.NEF.part",
            "notes.txt",
            "rejected",
        ]
    );
}

/// The history: a develop key set is a step, a run of the same key one
/// step, the keys the fold of the steps that are on; undo, toggle and
/// delete rewrite the keys from what remains; the file's history is read
/// over its summary; and every malformed step line refuses the file.
#[test]
fn a_history_folds_its_steps_and_the_keys_are_its_summary() {
    let mut sidecar = Sidecar::default();
    assert!(sidecar.steps().is_empty());
    assert!(!sidecar.undo() && !sidecar.toggle_step(0) && !sidecar.delete_step(0));
    sidecar.set(Key::Exposure, Some("0.33")).unwrap();
    sidecar.set(Key::Exposure, Some("0.66")).unwrap();
    // Every set is its own step, so undo is one nudge at a time.
    assert_eq!(sidecar.steps().len(), 2);
    assert_eq!(sidecar.exposure(), Some(66));
    assert!(sidecar.undo());
    assert_eq!(sidecar.exposure(), Some(33));
    sidecar.set(Key::Exposure, Some("0.66")).unwrap();
    assert!(sidecar.delete_step(0));
    sidecar.set(Key::Look, Some("mono")).unwrap();
    sidecar.set(Key::Exposure, Some("1.00")).unwrap();
    let steps = |sidecar: &Sidecar| {
        sidecar
            .steps()
            .iter()
            .map(|step| step.text())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        steps(&sidecar),
        ["on exposure 0.66", "on look mono", "on exposure 1.00"]
    );
    assert_eq!(
        sidecar.text(),
        "td-photo edit 1\nexposure 1.00\nlook mono\nstep-1 on exposure 0.66\nstep-2 on look mono\nstep-3 on exposure 1.00\n"
    );
    // The value a key already holds is no step, nor is clearing a key that
    // is clear; the flag is never a step.
    sidecar.set(Key::Look, Some("mono")).unwrap();
    sidecar.set(Key::Crop, None).unwrap();
    sidecar.set(Key::Flag, Some("pick")).unwrap();
    assert_eq!(sidecar.steps().len(), 3);
    // A step off is folded over: the earlier value of its key shows.
    assert!(sidecar.toggle_step(2));
    assert_eq!(sidecar.exposure(), Some(66));
    assert!(sidecar.text().contains("step-3 off exposure 1.00\n"));
    assert!(sidecar.toggle_step(2));
    assert_eq!(sidecar.exposure(), Some(100));
    // A clear is a step too, and the summary drops the key.
    sidecar.set(Key::Look, None).unwrap();
    assert_eq!(
        steps(&sidecar).last().map(String::as_str),
        Some("on look -")
    );
    assert_eq!(sidecar.look(), None);
    assert!(!sidecar.text().contains("\nlook "));
    // Deleting a step closes the rest up; undo takes the last back.
    assert!(sidecar.delete_step(1));
    assert_eq!(
        steps(&sidecar),
        ["on exposure 0.66", "on exposure 1.00", "on look -"]
    );
    assert!(sidecar.undo());
    assert!(sidecar.undo());
    assert_eq!(sidecar.exposure(), Some(66));
    assert_eq!(
        sidecar.text(),
        "td-photo edit 1\nexposure 0.66\nflag pick\nstep-1 on exposure 0.66\n"
    );
    assert!(sidecar.undo() && !sidecar.undo());
    assert_eq!(sidecar.text(), "td-photo edit 1\nflag pick\n");
    // A round trip keeps steps that are off, and the bytes are the text's
    // after the header.
    let mut off = Sidecar::default();
    off.set(Key::Crop, Some("0.1000 0.1000 0.5000 0.5000"))
        .unwrap();
    off.set(Key::Look, Some("mono")).unwrap();
    off.toggle_step(0);
    let text = off.text();
    assert_eq!(
        text,
        "td-photo edit 1\nlook mono\nstep-1 off crop 0.1000 0.1000 0.5000 0.5000\nstep-2 on look mono\n"
    );
    let back = Sidecar::parse(text.as_bytes()).unwrap();
    assert_eq!(back, off);
    assert_eq!(back.bytes() + SIDECAR_HEADER.len() + 1, text.len());
    // A summary that disagrees with its history is read by the history and
    // rewritten from it.
    let stale = Sidecar::parse(
        b"td-photo edit 1\nexposure 2.00\nfuture 1\nstep-1 on exposure 0.50\nstep-2 on crop -\n",
    )
    .unwrap();
    assert_eq!(stale.exposure(), Some(50));
    assert_eq!(
        stale.text(),
        "td-photo edit 1\nexposure 0.50\nfuture 1\nstep-1 on exposure 0.50\nstep-2 on crop -\n"
    );
    // Reset clears the history with the keys.
    let mut reset = stale.clone();
    reset.reset();
    assert_eq!(reset.text(), "td-photo edit 1\nfuture 1\n");
    // A full history refuses a further step until one goes.
    let mut full = Sidecar::default();
    for i in 0..library::MAX_STEPS {
        // Each value differs from the one in force, so each set is a step.
        let value = format!("{}.{:02}", i / 100, i % 100);
        full.set(Key::Exposure, Some(&value)).unwrap();
    }
    assert_eq!(full.steps().len(), library::MAX_STEPS);
    assert_eq!(
        full.set(Key::Crop, Some("0.1000 0.1000 0.5000 0.5000")),
        Err(Error::HistoryFull)
    );
    assert_eq!(full.set(Key::Flag, Some("pick")), Ok(()));
    assert!(full.undo());
    assert_eq!(
        full.set(Key::Crop, Some("0.1000 0.1000 0.5000 0.5000")),
        Ok(())
    );
    let text = full.text();
    assert_eq!(Sidecar::parse(text.as_bytes()).unwrap(), full);
    // Every malformed step line refuses the file: out of sequence, a
    // leading zero, past the ceiling, not on|off, a key that does not
    // develop, a value outside the key's grammar.
    let head = "td-photo edit 1\n";
    for (body, number) in [
        ("step-2 on exposure 1.00\n", 2),
        ("step-01 on exposure 1.00\n", 2),
        ("step-1 on exposure 1.00\nstep-1 on look mono\n", 3),
        ("step-1 on exposure 1.00\nstep-3 on look mono\n", 3),
        ("step-1 maybe exposure 1.00\n", 2),
        ("step-1 on flag pick\n", 2),
        ("step-1 on exposure 9.00\n", 2),
        ("step-1 on exposure\n", 2),
        ("step-1 on crop 0.1000 0.1000\n", 2),
    ] {
        assert_eq!(
            Sidecar::parse(format!("{head}{body}").as_bytes()),
            Err(Error::Step(number)),
            "{body}"
        );
    }
    let mut past = head.to_string();
    for i in 1..=library::MAX_STEPS + 1 {
        past.push_str(&format!("step-{i} on look l{i}\n"));
    }
    assert_eq!(
        Sidecar::parse(past.as_bytes()),
        Err(Error::Step(library::MAX_STEPS + 2))
    );
    assert_eq!(Error::Step(4).to_string(), "line 4 is not the next step");
    // The bare `-` is the clear everywhere a look is set, so it is not a
    // look: a file naming one is refused rather than read as a clear.
    assert!(!library::valid_look("-") && library::valid_look("-x"));
    assert_eq!(
        Sidecar::parse(b"td-photo edit 1\nlook -\n"),
        Err(Error::Value(Key::Look))
    );
    assert_eq!(
        Error::HistoryFull.to_string(),
        format!("history holds {} steps", library::MAX_STEPS)
    );
}
