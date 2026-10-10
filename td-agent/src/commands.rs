//! The person's commands (DESIGN.md §15): `NAME.md` files in the
//! `commands` directory beside td-agent's configuration, which no jail can
//! reach (§8). `/NAME ARGS` in the composer sends the file's text, with
//! the arguments in it, as the person's message.

use std::io::Read;
use std::path::Path;

/// The directory beside the configuration file that holds them.
pub const DIR: &str = "commands";

/// Where a command's arguments go in its text.
const ARGUMENTS: &str = "$ARGUMENTS";

/// The composer's own commands, never looked up as files.
const BUILT_IN: &[&str] = &[
    "commands",
    "compact",
    "review",
    "schedule",
    "schedules",
    "skills",
    "unschedule",
];

/// The most commands `/commands` lists, of the most directory entries it
/// reads; the most characters of a first line it shows, and bytes of
/// each file it reads for it.
const MAX_LISTED: usize = 128;
const MAX_ENTRIES: usize = 1024;
const MAX_FIRST: usize = 120;
const MAX_HEAD: u64 = 4096;

/// What the composer's text asks of the commands.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Typed {
    /// `/commands`: list them.
    List,
    /// `/skills`: list the person's skills (DESIGN.md §12).
    Skills,
    /// `/NAME ARGS`: command NAME's text, when there is such a command.
    Run { name: String, args: String },
}

/// A command's name: 1 to 32 lowercase ASCII letters, digits and `-`,
/// starting with a letter.
pub fn name_ok(name: &str) -> bool {
    (1..=32).contains(&name.len())
        && name.starts_with(|c: char| c.is_ascii_lowercase())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// The most bytes of a revision `/review` names.
pub const MAX_REVISION: usize = 256;

/// The revision `/review [REVISION]` names, HEAD when none, when `text`
/// is that command (DESIGN.md §15, `/review`): one word of what git
/// revisions are written with, never starting with `-`, so it is never
/// taken for an option.
pub fn review(text: &str) -> Option<Result<String, String>> {
    let rest = text.trim().strip_prefix("/review")?;
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let revision = rest.trim();
    if revision.is_empty() {
        return Some(Ok("HEAD".into()));
    }
    Some(
        self::revision(revision).map(str::to_string).map_err(|_| {
            format!(
                "/review takes one revision of at most {MAX_REVISION} bytes, such as HEAD or a commit's hash, not starting with -"
            )
        }),
    )
}

/// `word` when it is a revision `/review` takes: at most `MAX_REVISION`
/// bytes of what git revisions are written with, never starting with
/// `-`, so it is never taken for an option.
pub fn revision(word: &str) -> Result<&str, String> {
    let allowed = |b: u8| b.is_ascii_alphanumeric() || b"._/~^@{}:-".contains(&b);
    if !word.is_empty()
        && word.len() <= MAX_REVISION
        && !word.starts_with('-')
        && word.bytes().all(allowed)
    {
        Ok(word)
    } else {
        Err(format!("{word:?} is not a revision /review takes"))
    }
}

/// What `text` asks, when it starts with `/` and a name a command could
/// have, not one of the composer's own.
pub fn typed(text: &str) -> Option<Result<Typed, String>> {
    let rest = text.trim().strip_prefix('/')?;
    let (name, args) = rest
        .split_once(char::is_whitespace)
        .map_or((rest, ""), |(name, args)| (name, args.trim()));
    for (word, typed) in [("commands", Typed::List), ("skills", Typed::Skills)] {
        if name == word {
            return Some(match args.is_empty() {
                true => Ok(typed),
                false => Err(format!("/{word} takes nothing")),
            });
        }
    }
    if !name_ok(name) || BUILT_IN.contains(&name) {
        return None;
    }
    Some(Ok(Typed::Run {
        name: name.into(),
        args: args.into(),
    }))
}

/// Whether `dir` is there as a directory of its own: a link is refused,
/// as its target could be a place a workspace writes (§8).
pub fn real_dir(dir: &Path) -> Result<bool, String> {
    match std::fs::symlink_metadata(dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(format!("{}: {e}", dir.display())),
        Ok(meta) if meta.file_type().is_dir() => Ok(true),
        Ok(_) => Err(format!(
            "{} is not a directory of its own: a link is refused, as its target could be a place a workspace writes",
            dir.display()
        )),
    }
}

/// The file `path`'s bytes, none when there is nothing there: a regular
/// file of its own, opened without following a link, whose target could be
/// a place a workspace writes, or blocking, as a FIFO would. With `whole`,
/// all of it, refused past `most` bytes; else its first `most`.
pub fn real(path: &Path, label: &str, most: u64, whole: bool) -> Result<Option<Vec<u8>>, String> {
    let (file, meta) = match td_fs::open_real_file(path, label) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.to_string()),
        Ok(opened) => opened,
    };
    let past = || format!("{label} {} exceeds {most} bytes", path.display());
    if whole && meta.len() > most {
        return Err(past());
    }
    let mut bytes = Vec::new();
    file.take(most.saturating_add(u64::from(whole)))
        .read_to_end(&mut bytes)
        .map_err(|e| format!("{label} {}: {e}", path.display()))?;
    if whole && u64::try_from(bytes.len()).map_or(true, |read| read > most) {
        return Err(past());
    }
    Ok(Some(bytes))
}

/// Command `name`'s file in `dir`, read whole: at most `most` bytes of
/// UTF-8. None when there is none.
fn read(dir: &Path, name: &str, most: usize) -> Result<Option<String>, String> {
    if !real_dir(dir)? {
        return Ok(None);
    }
    let path = dir.join(format!("{name}.md"));
    let label = format!("command /{name}");
    let Some(bytes) = real(&path, &label, u64::try_from(most).unwrap_or(u64::MAX), true)? else {
        return Ok(None);
    };
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| format!("{label}: {} is not UTF-8", path.display()))
}

/// Command `name`'s text from `dir` with `args` in it: each `$ARGUMENTS`
/// replaced by them, or, with none in it, the arguments after a blank
/// line; at most `most` bytes, and not blank. None when there is no such
/// command, so the text goes as typed.
pub fn expand(dir: &Path, name: &str, args: &str, most: usize) -> Result<Option<String>, String> {
    let Some(body) = read(dir, name, most)? else {
        return Ok(None);
    };
    let body = body.trim_end();
    // Its length first, so a file of many `$ARGUMENTS` is refused before
    // it is built.
    let places = body.matches(ARGUMENTS).count();
    let length = match places {
        0 if args.is_empty() => Some(body.len()),
        0 => body.len().checked_add(2 + args.len()),
        places => places
            .checked_mul(args.len())
            .and_then(|added| (body.len() - places * ARGUMENTS.len()).checked_add(added)),
    };
    if length.is_none_or(|length| length > most) {
        return Err(format!(
            "command /{name} with its arguments is past the {} KiB a message may be",
            most / 1024
        ));
    }
    let text = match (places, args.is_empty()) {
        (0, true) => body.to_string(),
        (0, false) => format!("{body}\n\n{args}"),
        _ => body.replace(ARGUMENTS, args),
    };
    if text.trim().is_empty() {
        return Err(format!(
            "command /{name} gives an empty message without arguments"
        ));
    }
    Ok(Some(text))
}

/// The commands in `dir`, by name, each with its first line that is not
/// blank, cut, and how many more there are than are listed; none when
/// there is no directory.
pub fn list(dir: &Path) -> Result<(Vec<(String, String)>, usize), String> {
    if !real_dir(dir)? {
        return Ok((Vec::new(), 0));
    }
    let entries = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let mut names = Vec::new();
    for entry in entries.take(MAX_ENTRIES) {
        let entry = entry.map_err(|e| format!("{}: {e}", dir.display()))?;
        if let Some(name) = entry
            .file_name()
            .to_str()
            .and_then(|f| f.strip_suffix(".md"))
            .filter(|name| name_ok(name))
        {
            names.push(name.to_string());
        }
    }
    names.sort();
    let more = names.len().saturating_sub(MAX_LISTED);
    names.truncate(MAX_LISTED);
    let listed = names
        .into_iter()
        .map(|name| {
            let first = match BUILT_IN.contains(&name.as_str()) {
                true => {
                    "td-agent's own command has this name, so the file is never used".to_string()
                }
                false => match real(
                    &dir.join(format!("{name}.md")),
                    &format!("command /{name}"),
                    MAX_HEAD,
                    false,
                ) {
                    Ok(bytes) => first_line(&String::from_utf8_lossy(
                        bytes.as_deref().unwrap_or_default(),
                    )),
                    Err(why) => why,
                },
            };
            (name, first)
        })
        .collect();
    Ok((listed, more))
}

/// `text`'s first line that is not blank, cut to `MAX_FIRST` characters
/// and made visible.
fn first_line(text: &str) -> String {
    let line = text
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default()
        .trim();
    let cut: String = line.chars().take(MAX_FIRST).collect();
    let more = if cut.len() < line.len() { "…" } else { "" };
    format!("{}{more}", crate::tools::visible(&cut))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

    /// `/review` names one revision, HEAD by default; what could be
    /// taken for an option, or is not a revision's word, is refused, and
    /// a longer name is no `/review`.
    #[test]
    fn review_names_one_revision_head_by_default() {
        assert_eq!(review("/review"), Some(Ok("HEAD".into())));
        assert_eq!(review("  /review  "), Some(Ok("HEAD".into())));
        assert_eq!(review("/review HEAD~2"), Some(Ok("HEAD~2".into())));
        assert_eq!(
            review("/review origin/main@{1}"),
            Some(Ok("origin/main@{1}".into()))
        );
        for refused in [
            "/review --repo=/etc",
            "/review -n",
            "/review HEAD main",
            "/review a;b",
            "/review $(x)",
        ] {
            assert!(matches!(review(refused), Some(Err(_))), "{refused}");
        }
        let long = format!("/review {}", "a".repeat(MAX_REVISION + 1));
        assert!(matches!(review(&long), Some(Err(_))));
        assert_eq!(review("/reviews"), None);
        assert_eq!(review("review"), None);
        // The composer's own: never looked up as the person's command.
        assert_eq!(typed("/review HEAD"), None);
    }

    #[test]
    fn a_command_is_a_slash_and_a_name_not_the_composers_own() {
        assert_eq!(
            typed("  /review-pr  12\n  and more "),
            Some(Ok(Typed::Run {
                name: "review-pr".into(),
                args: "12\n  and more".into()
            }))
        );
        assert_eq!(
            typed("/fix"),
            Some(Ok(Typed::Run {
                name: "fix".into(),
                args: String::new()
            }))
        );
        assert_eq!(typed("/commands"), Some(Ok(Typed::List)));
        assert!(matches!(typed("/commands x"), Some(Err(_))));
        assert_eq!(typed("/skills"), Some(Ok(Typed::Skills)));
        assert!(matches!(typed("/skills all"), Some(Err(_))));
        for text in [
            "fix it",
            "/usr/bin is missing",
            "/Fix",
            "/9lives",
            "/compact",
            "/schedules",
            "/",
            &format!("/{}", "a".repeat(33)),
        ] {
            assert_eq!(typed(text), None, "{text}");
        }
    }

    #[test]
    fn a_commands_text_takes_its_arguments() {
        let scratch = crate::store::tests::Scratch::new("commands");
        let dir = scratch.0.join(DIR);
        assert_eq!(expand(&dir, "none", "x", 1024), Ok(None));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(expand(&dir, "none", "x", 1024), Ok(None));
        std::fs::write(
            dir.join("review.md"),
            "Review $ARGUMENTS, then $ARGUMENTS again.\n\n",
        )
        .unwrap();
        assert_eq!(
            expand(&dir, "review", "PR 12", 1024),
            Ok(Some("Review PR 12, then PR 12 again.".into()))
        );
        std::fs::write(dir.join("plain.md"), "Run the tests.\n").unwrap();
        assert_eq!(
            expand(&dir, "plain", "", 1024),
            Ok(Some("Run the tests.".into()))
        );
        assert_eq!(
            expand(&dir, "plain", "only td-agent", 1024),
            Ok(Some("Run the tests.\n\nonly td-agent".into()))
        );
        assert!(expand(&dir, "plain", &"x".repeat(1024), 1024)
            .unwrap_err()
            .contains("past the 1 KiB"));
        // Refused by its length, before it is built.
        std::fs::write(dir.join("many.md"), ARGUMENTS.repeat(100)).unwrap();
        assert!(expand(&dir, "many", &"x".repeat(1000), 1024)
            .unwrap_err()
            .contains("past the 1 KiB"));
        std::fs::write(dir.join("only.md"), "$ARGUMENTS\n").unwrap();
        assert_eq!(expand(&dir, "only", "go", 1024), Ok(Some("go".into())));
        assert!(expand(&dir, "only", "", 1024)
            .unwrap_err()
            .contains("empty message"));
        std::fs::write(dir.join("empty.md"), " \n").unwrap();
        assert!(expand(&dir, "empty", "", 1024)
            .unwrap_err()
            .contains("empty"));
        std::fs::write(dir.join("big.md"), "x".repeat(1025)).unwrap();
        assert!(expand(&dir, "big", "", 1024)
            .unwrap_err()
            .contains("exceeds"));
        std::fs::write(dir.join("bytes.md"), [0xff, 0xfe]).unwrap();
        assert!(expand(&dir, "bytes", "", 1024)
            .unwrap_err()
            .contains("not UTF-8"));
        std::fs::create_dir(dir.join("folder.md")).unwrap();
        assert!(expand(&dir, "folder", "", 1024)
            .unwrap_err()
            .contains("real regular file"));
        // A link's target could be a place a workspace writes.
        std::os::unix::fs::symlink(dir.join("plain.md"), dir.join("linked.md")).unwrap();
        assert!(expand(&dir, "linked", "", 1024)
            .unwrap_err()
            .contains("real regular file"));
        // A FIFO is refused unopened, which would block until a writer came.
        let fifo = dir.join("fifo.md");
        assert!(std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success());
        assert!(expand(&dir, "fifo", "", 1024)
            .unwrap_err()
            .contains("real regular file"));
        let (listed, _) = list(&dir).unwrap();
        assert!(listed
            .iter()
            .any(|(n, why)| n == "fifo" && why.contains("real regular file")));
        // And the directory, as a link, too.
        let elsewhere = scratch.0.join("elsewhere");
        std::os::unix::fs::symlink(&dir, &elsewhere).unwrap();
        assert!(expand(&elsewhere, "plain", "", 1024)
            .unwrap_err()
            .contains("a link is refused"));
        assert!(list(&elsewhere).is_err());
    }

    #[test]
    fn the_commands_are_listed_by_name_with_their_first_line() {
        let scratch = crate::store::tests::Scratch::new("listed");
        let dir = scratch.0.join(DIR);
        assert_eq!(list(&dir), Ok((Vec::new(), 0)));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("b.md"), "\n\n  Second\u{7}  \nmore").unwrap();
        std::fs::write(dir.join("a.md"), "x".repeat(200)).unwrap();
        std::fs::write(dir.join("compact.md"), "mine").unwrap();
        std::fs::write(dir.join("Not-A-Name.md"), "x").unwrap();
        std::fs::write(dir.join("notes.txt"), "x").unwrap();
        let (listed, more) = list(&dir).unwrap();
        let names: Vec<&str> = listed.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["a", "b", "compact"]);
        assert_eq!(more, 0);
        assert_eq!(listed[0].1, format!("{}…", "x".repeat(MAX_FIRST)));
        assert!(listed[1].1.starts_with("Second") && !listed[1].1.contains('\u{7}'));
        assert!(listed[2].1.contains("never used"));
        // Only the head of each file is read for its first line.
        std::fs::write(dir.join("big.md"), "x".repeat(1 << 20)).unwrap();
        assert!(list(&dir)
            .unwrap()
            .0
            .iter()
            .any(|(n, l)| n == "big" && l.ends_with('…')));
        // The first by name are listed, the rest counted.
        for n in 0..MAX_LISTED {
            std::fs::write(dir.join(format!("z{n:03}.md")), "z").unwrap();
        }
        let (listed, more) = list(&dir).unwrap();
        assert_eq!(listed.len(), MAX_LISTED);
        assert_eq!(listed[0].0, "a");
        assert_eq!(more, 4);
    }
}
