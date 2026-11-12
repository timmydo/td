//! The one-time import of the profiles td-sandbox kept before sandbox
//! accounts (td-sandbox/DESIGN.md, "Import"): each profile whose name the
//! ledger can hold becomes a sandbox created through root, its shares
//! below the human's home its grants and its network its network row; its
//! old home is left as it was and its record names it. A profile whose
//! name the ledger cannot hold, and a share outside the home or one root
//! refuses, is reported and dropped; one whose name a sandbox already has
//! is reported and never imported into. A marker says the import is
//! done; until it is, every run imports again, and what it already did
//! converges.

use crate::intake::{self, Answer, Intake, Status};
use crate::state;
use crate::{failure, owned, refused, text};
use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Below the old profiles' directory: the import's marker and its report.
pub const MARKER: &str = ".imported";
pub const REPORT: &str = "import-report.txt";
/// The old policy's bound, and its shares'.
const POLICY_BYTES: u64 = 65536;
const MAX_SHARES: usize = 30;
/// The most entries of the old profiles' directory looked at.
const SCAN_ENTRIES: usize = 4 * state::MAX_PROFILES;
/// The report's bound, read and written.
const REPORT_BYTES: u64 = 1024 * 1024;
const REPORT_HEAD: &str = "td-sandbox: what the import of profiles from before sandbox accounts did not carry over, and what changed (td-sandbox/DESIGN.md, \"Import\")\n";
const REPORT_FULL: &str = "the report is full; later lines were printed only";
const SHARES_MOVED: &str = "a sandbox now sees its shares at their own paths below your home, not inside its home: the ~/sandbox link the old default's home held is gone";

/// An old profile's policy: its network, its runtime and its shares, each
/// a canonical absolute path and whether it was writable.
#[derive(Debug, PartialEq, Eq)]
pub struct Policy {
    pub host: bool,
    pub runtime: Option<String>,
    pub shares: Vec<(PathBuf, bool)>,
}

/// An old profile's name: 1 to 32 lowercase letters, digits or hyphens.
fn old_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// The old policy: `format=1`, `network=off|host`, an optional
/// `runtime=`, then at most 30 `read=` or `write=` absolute paths.
pub fn parse_policy(policy: &str) -> io::Result<Policy> {
    let invalid = |what: &str| failure(format_args!("invalid old {what}"));
    let mut lines = policy.lines().peekable();
    if lines.next() != Some("format=1") {
        return Err(invalid("policy format"));
    }
    let host = match lines.next() {
        Some("network=host") => true,
        Some("network=off") => false,
        _ => return Err(invalid("network policy")),
    };
    let runtime = match lines.peek().and_then(|line| line.strip_prefix("runtime=")) {
        Some(name) if crate::flathub_app::valid_name(name) => {
            let name = owned(name)?;
            lines.next();
            Some(name)
        }
        Some(_) => return Err(invalid("runtime policy")),
        None => None,
    };
    let mut shares = Vec::new();
    for line in lines {
        let (key, path) = line.split_once('=').ok_or_else(|| invalid("share"))?;
        if !matches!(key, "read" | "write")
            || !Path::new(path).is_absolute()
            || shares.len() >= MAX_SHARES
        {
            return Err(invalid("share"));
        }
        state::text(Path::new(path))?;
        td_alloc::push(&mut shares, (PathBuf::from(owned(path)?), key == "write"))
            .map_err(refused)?;
    }
    Ok(Policy {
        host,
        runtime,
        shares,
    })
}

/// What the import reports, kept in `REPORT` below the old profiles:
/// each line once, however often a crash makes the import run again, and
/// the whole within `REPORT_BYTES`, a full report ending in `REPORT_FULL`.
struct Report {
    path: PathBuf,
    lines: Vec<String>,
    /// Every line reported or read, as a set: only membership is asked.
    seen: HashMap<String, ()>,
}

impl Report {
    fn open(base: &Path) -> io::Result<Self> {
        let path = base.join(REPORT);
        let text = match td_fs::read_bounded_real_file(&path, "import report", REPORT_BYTES) {
            Ok(bytes) => String::from_utf8(bytes).map_err(io::Error::other)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e),
        };
        let mut lines = Vec::new();
        let mut seen = HashMap::new();
        for line in text
            .lines()
            .filter(|line| REPORT_HEAD.strip_suffix('\n') != Some(*line))
        {
            td_alloc::push(&mut lines, owned(line)?).map_err(refused)?;
            td_alloc::insert(&mut seen, owned(line)?, ()).map_err(refused)?;
        }
        Ok(Self { path, lines, seen })
    }

    fn size(&self) -> usize {
        REPORT_HEAD.len() + self.lines.iter().map(|line| line.len() + 1).sum::<usize>()
    }

    /// Adds `line`, printing it and writing the report whole again, unless
    /// an earlier run reported it: what was not carried over is on disk
    /// before the import goes on. Past the bound the line is printed only,
    /// and the report says it is full.
    fn add(&mut self, line: String) -> io::Result<()> {
        if self.seen.contains_key(&line) {
            return Ok(());
        }
        td_alloc::insert(&mut self.seen, owned(&line)?, ()).map_err(refused)?;
        let _ = writeln!(io::stderr(), "td-sandbox: import: {line}");
        if self.lines.last().is_some_and(|last| last == REPORT_FULL) {
            return Ok(());
        }
        let room = (REPORT_BYTES as usize).saturating_sub(REPORT_FULL.len() + 1);
        if self.size() + line.len() + 1 > room {
            td_alloc::push(&mut self.lines, owned(REPORT_FULL)?).map_err(refused)?;
        } else {
            td_alloc::push(&mut self.lines, line).map_err(refused)?;
        }
        // The whole report, its head included, reserved once.
        let mut whole = String::new();
        td_alloc::reserve_string(&mut whole, self.size()).map_err(refused)?;
        td_alloc::push_str(&mut whole, REPORT_HEAD).map_err(refused)?;
        for line in &self.lines {
            td_alloc::push_str(&mut whole, line).map_err(refused)?;
            td_alloc::push_str(&mut whole, "\n").map_err(refused)?;
        }
        td_fs::replace(&self.path, whole.as_bytes(), 0o600)
    }
}

/// The line for an old profile whose name a sandbox already has: its
/// shares and network are not imported into a sandbox it did not make.
fn exists_line(name: &str, old_home: &Path) -> io::Result<String> {
    text(format_args!(
        "profile {name}: a sandbox named {name} already exists; its old profile's shares and \
         network were not imported. Its old home stays at {}",
        old_home.display()
    ))
}

/// A refusal that may pass stops the import before its marker, the
/// profile's record kept, so the next run asks again what this one did.
fn again(name: &str, what: &str, reason: &str) -> io::Error {
    failure(format_args!(
        "import: profile {name}: root refused {what} for now ({reason}); the import runs \
         again next time"
    ))
}

/// The names of the old profiles below `base`, sorted, from at most
/// `SCAN_ENTRIES` entries, and whether there were more.
fn old_profiles(base: &Path) -> io::Result<(Vec<String>, bool)> {
    let mut names = Vec::new();
    let mut entries = fs::read_dir(base)?;
    for entry in entries.by_ref().take(SCAN_ENTRIES) {
        let entry = entry?;
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if old_name(&name) && entry.file_type()?.is_dir() {
            td_alloc::push(&mut names, name).map_err(refused)?;
        }
    }
    // Directory entries' names are distinct, so no order among equals is
    // lost by an unstable sort, which allocates nothing.
    names.sort_unstable();
    Ok((names, entries.next().is_some()))
}

/// Imports the old profiles below `base` once, the human's canonical home
/// `home`, through `intake`; returns at once when the marker says it was
/// done. A refusal is dropped and reported only when root says it is
/// final; any other, like no answer, stops the import before its marker.
/// An imported profile's record is written, naming its old home, before
/// root is asked anything for it, so a run cut short leaves a record the
/// next run completes; a sandbox of the name that the import did not
/// make, held by root or recorded here, is never imported into.
pub fn once(base: &Path, home: &Path, intake: &impl Intake) -> io::Result<()> {
    once_within(base, home, intake, state::MAX_PROFILES)
}

/// `once`, importing at most `limit` profiles.
fn once_within(base: &Path, home: &Path, intake: &impl Intake, limit: usize) -> io::Result<()> {
    if fs::symlink_metadata(base.join(MARKER)).is_ok() {
        return Ok(());
    }
    let profiles = state::profiles(base)?;
    let mut report = Report::open(base)?;
    let (old, more) = old_profiles(base)?;
    if more {
        report.add(text(format_args!(
            "the old profiles' directory {} holds more than {SCAN_ENTRIES} entries; those past \
             them were not looked at",
            base.display()
        ))?)?;
    }
    for name in old.iter().skip(limit) {
        // As `base.join(name).join("home").display()` shows it.
        report.add(text(format_args!(
            "profile {name}: not imported, since the import takes at most {limit} profiles. \
             Its old home stays at {}/{name}/home",
            base.display()
        ))?)?;
    }
    let mut imported = false;
    for name in old.iter().take(limit) {
        let name = name.as_str();
        let root = base.join(name);
        let old_home = root.join("home");
        if state::name(name).is_err() {
            report.add(text(format_args!(
                "profile {name}: its name cannot name a sandbox account (it begins with a \
                 digit or a hyphen); dropped. Its old home stays at {}",
                old_home.display()
            ))?)?;
            continue;
        }
        let own = state::exists(&profiles, name)?;
        if own
            && state::Profile::read(&profiles, name).map_or(true, |record| {
                record.old_home.as_deref() != Some(old_home.as_path())
            })
        {
            report.add(exists_line(name, &old_home)?)?;
            continue;
        }
        let policy =
            td_fs::read_bounded_real_file(&root.join("policy"), "old policy", POLICY_BYTES)
                .and_then(|bytes| String::from_utf8(bytes).map_err(io::Error::other))
                .and_then(|text| parse_policy(&text));
        let mut policy = match policy {
            Ok(policy) => policy,
            Err(e) => {
                report.add(text(format_args!(
                    "profile {name}: its policy cannot be read ({e}); dropped. Its old home \
                     stays at {}",
                    old_home.display()
                ))?)?;
                continue;
            }
        };
        if !own {
            let status = intake
                .ask(&intake::simple("status", name)?)?
                .accepted()
                .and_then(|rest| intake::status(&rest))?;
            if let Status::Held(_) = status {
                // Adopted as it is: a record naming no old home, so a
                // later run reports it again rather than importing into it.
                drop(state::Profile::create(&profiles, name, None, None)?);
                report.add(exists_line(name, &old_home)?)?;
                continue;
            }
            drop(state::Profile::create(
                &profiles,
                name,
                policy.runtime.take(),
                Some(old_home.clone()),
            )?);
        }
        match intake.ask(&intake::simple("create", name)?)? {
            Answer::Ok(_) => {}
            // This import's own record: its earlier run made the sandbox.
            Answer::Refused(reason) if own && reason == intake::EXISTS => {}
            // Made since root's status said otherwise: adopted as it is.
            Answer::Refused(reason) if reason == intake::EXISTS => {
                if let Some(record) = state::removal(&profiles, name)? {
                    record.remove()?;
                }
                drop(state::Profile::create(&profiles, name, None, None)?);
                report.add(exists_line(name, &old_home)?)?;
                continue;
            }
            Answer::Refused(reason) => match intake::lasting(&reason) {
                Some(reason) => {
                    if let Some(record) = state::removal(&profiles, name)? {
                        record.remove()?;
                    }
                    report.add(text(format_args!(
                        "profile {name}: root refused its account ({reason}); dropped. Its old \
                         home stays at {}",
                        old_home.display()
                    ))?)?;
                    continue;
                }
                None => return Err(again(name, "its account", &reason)),
            },
        }
        imported = true;
        for (path, write) in &policy.shares {
            let Some(relative) = intake::relative(home, path) else {
                report.add(text(format_args!(
                    "profile {name}: its {} share {} lies outside your home; dropped",
                    if *write { "writable" } else { "read-only" },
                    path.display()
                ))?)?;
                continue;
            };
            if let Answer::Refused(reason) = intake.ask(&intake::grant(name, *write, relative)?)? {
                let Some(reason) = intake::lasting(&reason) else {
                    return Err(again(
                        name,
                        &text(format_args!("its share {}", path.display()))?,
                        &reason,
                    ));
                };
                report.add(text(format_args!(
                    "profile {name}: root refused its share {} ({reason}); dropped",
                    path.display()
                ))?)?;
            }
        }
        if policy.host {
            if let Answer::Refused(reason) = intake.ask(&intake::network(name, true)?)? {
                let Some(reason) = intake::lasting(&reason) else {
                    return Err(again(name, "its host network", &reason));
                };
                report.add(text(format_args!(
                    "profile {name}: root refused its host network ({reason}); dropped"
                ))?)?;
            }
        }
    }
    if imported {
        report.add(owned(SHARES_MOVED)?)?;
    }
    td_fs::replace(&base.join(MARKER), b"td-sandbox import done\n", 0o600)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// Root as the import sees it: each request recorded, an answer
    /// chosen by its line, a final refusal or one that may pass, and,
    /// once, no answer at all.
    #[derive(Default)]
    struct Fake {
        asked: RefCell<Vec<String>>,
        refuse: Vec<&'static str>,
        busy: Vec<&'static str>,
        held: Vec<&'static str>,
        lost: RefCell<Option<&'static str>>,
    }

    impl Intake for Fake {
        fn ask(&self, line: &str) -> io::Result<Answer> {
            if self.lost.borrow().is_some_and(|lost| line == lost) {
                self.lost.replace(None);
                return Err(io::Error::other("the intake went away"));
            }
            self.asked.borrow_mut().push(line.to_string());
            if self.refuse.contains(&line) {
                return Ok(Answer::Refused("final: refused here".into()));
            }
            if self.busy.contains(&line) {
                return Ok(Answer::Refused("busy; try again".into()));
            }
            let named = |verb: &str| {
                line.strip_prefix(verb)
                    .and_then(|rest| rest.strip_suffix('\n'))
                    .is_some_and(|name| self.held.contains(&name))
            };
            Ok(Answer::Ok(if named("status ") {
                "held 98304\n".into()
            } else if line.starts_with("status ") {
                "absent\n".into()
            } else if named("create ") {
                return Ok(Answer::Refused(intake::EXISTS.into()));
            } else if line.starts_with("create ") {
                "98304\n".into()
            } else {
                "added\n".into()
            }))
        }
    }

    /// An old profile below `base`, its policy `policy`, its home holding
    /// one file.
    fn old(base: &Path, name: &str, policy: &str) -> io::Result<()> {
        td_fs::private_dir(&base.join(name))?;
        td_fs::private_dir(&base.join(name).join("home"))?;
        fs::write(base.join(name).join("home/kept"), "kept")?;
        td_fs::replace(&base.join(name).join("policy"), policy.as_bytes(), 0o600)
    }

    #[test]
    fn the_old_policy_is_read_exactly() -> io::Result<()> {
        assert_eq!(
            parse_policy(
                "format=1\nnetwork=host\nruntime=freedesktop-platform-25-08\nread=/r\nwrite=/w\n"
            )?,
            Policy {
                host: true,
                runtime: Some("freedesktop-platform-25-08".into()),
                shares: vec![(PathBuf::from("/r"), false), (PathBuf::from("/w"), true)],
            }
        );
        for bad in [
            "format=1\nruntime=freedesktop-platform-25-08\nnetwork=off\n",
            "format=1\nnetwork=off\nruntime=Org.Freedesktop\n",
            "format=1\nnetwork=off\nwrite=relative\n",
            "format=1\nnetwork=on\n",
            "format=2\nnetwork=off\n",
            "format=1\nnetwork=host\nentry=/bin/sh\n",
        ] {
            assert!(parse_policy(bad).is_err(), "{bad:?}");
        }
        let many: String = (0..31).map(|n| format!("read=/r{n}\n")).collect();
        assert!(parse_policy(&format!("format=1\nnetwork=off\n{many}")).is_err());
        for (name, old) in [
            ("default", true),
            ("1box", true),
            ("-x", true),
            ("a.b", false),
        ] {
            assert_eq!(old_name(name), old, "{name}");
        }
        Ok(())
    }

    /// A profile whose name the ledger holds is created through root with
    /// its in-home shares as grants of the same access, home-relative, and
    /// its host network; its old home stays and its record names it. One
    /// whose name the ledger cannot hold, and a share outside the home or
    /// one root refuses, are reported once and dropped. A second run does
    /// nothing.
    #[test]
    fn the_import_creates_grants_reports_and_drops() -> io::Result<()> {
        let home = td_fs::TempDir::new("sandbox-import")?;
        let home = fs::canonicalize(home.path())?;
        let base = state::base(&home)?;
        let h = home.display();
        old(
            &base,
            "default",
            &format!("format=1\nnetwork=host\nwrite={h}/sandbox\nread={h}/src/my project\nread=/srv/data\nwrite={h}\nread={h}/.ssh\n"),
        )?;
        old(&base, "1box", "format=1\nnetwork=off\n")?;
        old(&base, "-x", "format=1\nnetwork=off\n")?;
        old(
            &base,
            "tools",
            "format=1\nnetwork=off\nruntime=freedesktop-sdk-25-08\n",
        )?;
        old(&base, "broken", "format=9\n")?;
        let fake = Fake {
            refuse: vec!["grant default ro .ssh\n"],
            ..Fake::default()
        };
        once(&base, &home, &fake)?;
        assert_eq!(
            *fake.asked.borrow(),
            [
                "status default\n",
                "create default\n",
                "grant default rw sandbox\n",
                "grant default ro src/my project\n",
                "grant default ro .ssh\n",
                "network default host\n",
                "status tools\n",
                "create tools\n",
            ]
        );
        let profiles = state::profiles(&base)?;
        assert_eq!(state::list(&profiles)?, ["default", "tools"]);
        let default = state::Profile::read(&profiles, "default")?;
        assert_eq!(default.old_home, Some(base.join("default/home")));
        assert_eq!(default.runtime, None);
        assert_eq!(
            state::Profile::read(&profiles, "tools")?.runtime.as_deref(),
            Some("freedesktop-sdk-25-08")
        );
        // Old homes are left as they were.
        for name in ["default", "1box", "-x", "tools"] {
            assert_eq!(
                fs::read_to_string(base.join(name).join("home/kept"))?,
                "kept"
            );
        }
        let report = fs::read_to_string(base.join(REPORT))?;
        for expected in [
            "profile -x: its name cannot name a sandbox account",
            "profile 1box: its name cannot name a sandbox account",
            "profile broken: its policy cannot be read",
            "share /srv/data lies outside your home; dropped",
            &format!("writable share {h} lies outside your home; dropped"),
            &format!("root refused its share {h}/.ssh (refused here); dropped"),
            SHARES_MOVED,
        ] {
            assert!(report.contains(expected), "{expected}: {report}");
        }
        assert!(report.starts_with(REPORT_HEAD));
        assert!(
            !report.contains("sandbox\n") && !report.contains("tools"),
            "{report}"
        );
        // Done: nothing is asked again.
        let again = Fake::default();
        once(&base, &home, &again)?;
        assert!(again.asked.borrow().is_empty());
        assert_eq!(fs::read_to_string(base.join(REPORT))?, report);
        Ok(())
    }

    /// An import cut short, here by root going away before a profile's
    /// grants were all made, leaves no marker and the profile's record,
    /// naming its old home; the next run asks again what it asked, which
    /// root answers as unchanged, takes the sandbox it made already
    /// existing as its own, and reports nothing twice.
    #[test]
    fn an_interrupted_import_converges() -> io::Result<()> {
        let home = td_fs::TempDir::new("sandbox-import-again")?;
        let home = fs::canonicalize(home.path())?;
        let base = state::base(&home)?;
        let h = home.display();
        old(
            &base,
            "default",
            &format!("format=1\nnetwork=off\nwrite={h}/a\nread=/outside\nread={h}/b\n"),
        )?;
        old(&base, "9lives", "format=1\nnetwork=off\n")?;
        let lost = Fake {
            lost: RefCell::new(Some("grant default ro b\n")),
            ..Fake::default()
        };
        assert!(once(&base, &home, &lost).is_err());
        assert!(!base.join(MARKER).exists());
        let profiles = state::profiles(&base)?;
        assert_eq!(
            state::Profile::read(&profiles, "default")?.old_home,
            Some(base.join("default/home"))
        );
        let first = fs::read_to_string(base.join(REPORT))?;
        // Root now holds the sandbox the first run made, so a creation is
        // refused as existing: the import's own record takes it.
        let resumed = Fake {
            held: vec!["default"],
            ..Fake::default()
        };
        once(&base, &home, &resumed)?;
        assert_eq!(
            *resumed.asked.borrow(),
            [
                "create default\n",
                "grant default rw a\n",
                "grant default ro b\n"
            ]
        );
        assert!(base.join(MARKER).exists());
        let report = fs::read_to_string(base.join(REPORT))?;
        assert!(report.starts_with(&first));
        assert_eq!(report.matches("profile 9lives").count(), 1);
        assert_eq!(report.matches("/outside").count(), 1);
        assert!(!report.contains("already exists"), "{report}");
        Ok(())
    }

    /// An old profile whose name root already holds a sandbox of, or one
    /// a record here already names that the import did not make, is never
    /// imported into: the held one is adopted as it is, the other left,
    /// each reported with its old home, and nothing of its shares or
    /// network is asked for.
    #[test]
    fn a_held_or_recorded_name_is_adopted_or_left_never_imported_into() -> io::Result<()> {
        let home = td_fs::TempDir::new("sandbox-import-held")?;
        let home = fs::canonicalize(home.path())?;
        let base = state::base(&home)?;
        let h = home.display();
        let policy = format!("format=1\nnetwork=host\nwrite={h}/src\n");
        old(&base, "default", &policy)?;
        old(&base, "tools", &policy)?;
        let profiles = state::profiles(&base)?;
        drop(state::Profile::create(&profiles, "tools", None, None)?);
        let fake = Fake {
            held: vec!["default"],
            ..Fake::default()
        };
        once(&base, &home, &fake)?;
        assert_eq!(*fake.asked.borrow(), ["status default\n"]);
        // One made between root's status and the creation is adopted too.
        let raced = td_fs::TempDir::new("sandbox-import-raced")?;
        let raced = fs::canonicalize(raced.path())?;
        let raced_base = state::base(&raced)?;
        old(&raced_base, "default", &policy)?;
        struct Raced(Fake);
        impl Intake for Raced {
            fn ask(&self, line: &str) -> io::Result<Answer> {
                if line == "create default\n" {
                    self.0.asked.borrow_mut().push(line.to_string());
                    return Ok(Answer::Refused(intake::EXISTS.into()));
                }
                self.0.ask(line)
            }
        }
        let creation = Raced(Fake::default());
        once(&raced_base, &raced, &creation)?;
        assert_eq!(
            *creation.0.asked.borrow(),
            ["status default\n", "create default\n"]
        );
        let adopted = state::Profile::read(&state::profiles(&raced_base)?, "default")?;
        assert_eq!(adopted.old_home, None);
        let adopted = state::Profile::read(&profiles, "default")?;
        assert_eq!((adopted.old_home, adopted.runtime), (None, None));
        let report = fs::read_to_string(base.join(REPORT))?;
        for name in ["default", "tools"] {
            let line = exists_line(name, &base.join(name).join("home"))?;
            assert!(report.contains(&line), "{line}: {report}");
        }
        assert!(!report.contains(SHARES_MOVED));
        Ok(())
    }

    /// No old profile is dropped silently: those past the import's bound
    /// are reported by name, and the report keeps to its own bound, saying
    /// when it is full.
    #[test]
    fn profiles_past_the_bound_are_reported_and_the_report_is_bounded() -> io::Result<()> {
        let home = td_fs::TempDir::new("sandbox-import-bound")?;
        let home = fs::canonicalize(home.path())?;
        let base = state::base(&home)?;
        for name in ["a", "b", "c"] {
            old(&base, name, "format=1\nnetwork=off\n")?;
        }
        let fake = Fake::default();
        once_within(&base, &home, &fake, 2)?;
        let report = fs::read_to_string(base.join(REPORT))?;
        assert!(report.contains("profile c: not imported"), "{report}");
        assert!(!report.contains("profile b:"), "{report}");
        assert_eq!(
            *fake.asked.borrow(),
            ["status a\n", "create a\n", "status b\n", "create b\n"]
        );
        assert!(base.join(MARKER).exists());
        let mut report = Report::open(&base)?;
        for index in 0..40 {
            report.add(format!("line {index:02} {}", "x".repeat(50_000)))?;
        }
        let text = fs::read_to_string(base.join(REPORT))?;
        assert!(text.len() as u64 <= REPORT_BYTES);
        assert!(text.ends_with(&format!("{REPORT_FULL}\n")));
        assert!(Report::open(&base).is_ok());
        Ok(())
    }

    /// A refusal root does not call final, a busy table's or a claim's,
    /// stops the import before its marker, drops nothing and reports
    /// nothing: the profile's record stays, naming its old home, and the
    /// next run, asking again, imports what the first could not.
    #[test]
    fn a_refusal_that_may_pass_stops_the_import_and_drops_nothing() -> io::Result<()> {
        let home = td_fs::TempDir::new("sandbox-import-busy")?;
        let home = fs::canonicalize(home.path())?;
        let base = state::base(&home)?;
        let h = home.display();
        old(
            &base,
            "default",
            &format!("format=1\nnetwork=host\nwrite={h}/a\n"),
        )?;
        old(&base, "tools", "format=1\nnetwork=off\n")?;
        let profiles = state::profiles(&base)?;
        for (busy, asked) in [
            (
                "create default\n",
                &["status default\n", "create default\n"][..],
            ),
            (
                "grant default rw a\n",
                &["create default\n", "grant default rw a\n"],
            ),
            (
                "network default host\n",
                &[
                    "create default\n",
                    "grant default rw a\n",
                    "network default host\n",
                ],
            ),
        ] {
            // The sandbox the import made is held from its creation on.
            let held = if busy.starts_with("create ") {
                vec![]
            } else {
                vec!["default"]
            };
            let fake = Fake {
                busy: vec![busy],
                held,
                ..Fake::default()
            };
            let stopped = once(&base, &home, &fake).err().map(|e| e.to_string());
            assert!(
                stopped.as_deref().is_some_and(|e| e.contains("runs again")),
                "{stopped:?}"
            );
            assert_eq!(*fake.asked.borrow(), asked);
            assert!(!base.join(MARKER).exists());
            assert_eq!(
                state::Profile::read(&profiles, "default")?.old_home,
                Some(base.join("default/home"))
            );
            assert!(!state::exists(&profiles, "tools")?);
            let report = fs::read_to_string(base.join(REPORT)).unwrap_or_default();
            assert!(!report.contains("dropped"), "{report}");
        }
        let resumed = Fake {
            held: vec!["default"],
            ..Fake::default()
        };
        once(&base, &home, &resumed)?;
        assert_eq!(
            *resumed.asked.borrow(),
            [
                "create default\n",
                "grant default rw a\n",
                "network default host\n",
                "status tools\n",
                "create tools\n",
            ]
        );
        assert!(base.join(MARKER).exists());
        Ok(())
    }

    /// With no old profile, the import only leaves its marker.
    #[test]
    fn nothing_to_import_leaves_only_the_marker() -> io::Result<()> {
        let home = td_fs::TempDir::new("sandbox-import-none")?;
        let home = fs::canonicalize(home.path())?;
        let base = state::base(&home)?;
        let fake = Fake::default();
        once(&base, &home, &fake)?;
        assert!(fake.asked.borrow().is_empty());
        assert!(base.join(MARKER).exists());
        assert!(!base.join(REPORT).exists());
        Ok(())
    }
}
