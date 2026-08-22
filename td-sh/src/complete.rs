//! Tab completion for the line editor.
//!
//! The policy is a pure function of the line, the cursor and a `Source`, so it
//! is tested without a terminal, a filesystem or a `PATH` — the editor's own
//! `Keys` already takes its bytes the same way, and for the same reason.
//!
//! busybox's `lineedit.c` is the model rather than readline: this shell is
//! ash's, and ash completes command names in command position and pathnames
//! everywhere else (`complete_cmd_dir_file`), inserts the longest common
//! prefix, and lists on a second Tab. Deliberately NOT here, and each its own
//! increment: `~user` (it needs the passwd database, which `std` does not
//! expose — the same reason `tilde_split` leaves `~user` alone), variable and
//! hostname completion, and any programmable form.
//!
//! QUOTING is not understood either, and because a blank inside `'…'` is not
//! a word break, not understanding it is not the same as ignoring it: a Tab
//! inside an unclosed quote DECLINES, rather than completing the fragment
//! after the last blank and rewriting it inside the quote. Backslashes ARE
//! understood, because completion produces them.

/// What a directory listing is: each name, and whether it is a directory.
pub type Entries = Vec<(String, bool)>;

/// What completion is allowed to see. Both arms are injected because a test
/// must be able to state the whole world: a completion policy checked against
/// the build host's `PATH` asserts whatever that host happens to hold.
///
/// Both take the PREFIX the word starts with. That is a narrowing and not a
/// contract: the policy filters again, so a source that ignores it is still
/// correct. What it buys is that a source need not ask the kernel about a
/// candidate it is about to discard — a Tab in command position would otherwise
/// stat every file on `PATH` before the first character was compared.
pub struct Source<'a> {
    /// Everything beginning with `prefix` that can begin a command: the
    /// builtins, plus the shell's functions and aliases, plus the executables
    /// on `PATH`.
    pub commands: &'a dyn Fn(&str) -> Vec<String>,
    /// The entries of `dir` beginning with `prefix`, as (name, is_directory).
    /// `dir` is "" for the working directory, so the caller decides what that
    /// means rather than this module resolving a path itself.
    pub entries: &'a dyn Fn(&str, &str) -> Entries,
}

/// What a Tab produced.
#[derive(Debug, PartialEq, Eq)]
pub struct Completion {
    /// Byte range of the word being replaced, always on character boundaries.
    pub start: usize,
    pub end: usize,
    /// The text to put there: the longest common prefix of the matches, with a
    /// trailing space (or `/`) when the match is unique and complete.
    pub insert: String,
    /// Every match, for the listing a second Tab prints. Sorted, so the display
    /// does not depend on directory order.
    pub matches: Vec<String>,
}

/// Characters that end a word without being part of it. The unquoted blanks,
/// plus the operator characters a command word cannot contain — so `echo a|gr`
/// completes `gr` as a COMMAND, which is what it is.
fn breaks(c: char) -> bool {
    matches!(c, ' ' | '\t' | ';' | '|' | '&' | '<' | '>' | '(' | ')' | '\n')
}

/// The word under the cursor: back to the nearest unescaped break. A `\ ` is
/// part of the word, since that is how a completed filename with a space in it
/// comes back — otherwise the second Tab on `foo\ ba` would complete `ba`.
fn word_start(buf: &str, pos: usize) -> usize {
    let mut start = pos;
    for (i, c) in buf.char_indices().rev() {
        if i >= pos {
            continue;
        }
        if breaks(c) && !escaped(buf, i) {
            break;
        }
        start = i;
    }
    start
}

/// Whether the byte at `i` is preceded by an odd number of backslashes.
///
/// Over `buf[..i]` and not over the whole of `buf` with a skip, which is the
/// difference between costing the backslash RUN and costing the distance from
/// the end of the line. `word_start` calls this once per escaped break, so the
/// skipping form made a word of escaped blanks quadratic: `cat ` + `"x\ "`
/// repeated took 16 ms at n=4000 and 718 ms at n=32000 in the release build,
/// against 97 µs and 691 µs here.
fn escaped(buf: &str, i: usize) -> bool {
    let mut n = 0usize;
    for c in buf.get(..i).unwrap_or("").chars().rev() {
        if c != '\\' {
            break;
        }
        n += 1;
    }
    n % 2 == 1
}

/// Whether the word starting at `start` is where a COMMAND goes: nothing before
/// it but blanks, and before those either the start of the line, one of the
/// separators that end a command, or a run of words a command can still follow.
///
/// This is where td-sh is deliberately MORE capable than the model rather than
/// faithful to it. busybox decides with one scan for the first blank (or `<`
/// or `>`) in the line, so everything past the first word is a pathname to it
/// — `echo hi; ec` and `A=1 ec` included. The grammar this shell already
/// parses says otherwise, and following busybox here would mean completing a
/// filename where only a command can go.
///
/// A LOOP rather than the obvious recursion: that run is as long as the line,
/// and a pasted one is as long as the operator likes.
fn command_position(buf: &str, start: usize) -> bool {
    let mut end = start;
    loop {
        let before = buf.get(..end).unwrap_or("");
        let trimmed = before.trim_end_matches([' ', '\t']);
        let Some(last) = trimmed.chars().next_back() else {
            return true;
        };
        if matches!(last, ';' | '|' | '&' | '(' | '\n') {
            return true;
        }
        if trimmed.len() >= before.len() {
            return false;
        }
        let last_word = trimmed.rsplit([' ', '\t']).next().unwrap_or("");
        if !transparent(last_word) {
            return false;
        }
        // Strictly leftwards, since every `transparent` word is non-empty --
        // which is what ends this loop.
        end = trimmed.len().saturating_sub(last_word.len());
    }
}

/// A word a command can still follow. Each is only one BY BEING in command
/// position itself, which is why they step the loop leftwards rather than
/// answering it: `echo then ec` completes a pathname, because `then` there is
/// an argument.
fn transparent(word: &str) -> bool {
    is_assignment(word) || is_redirection(word) || introduces_command(word)
}

/// `name=value` with a portable name, which is what keeps a command word in
/// command position across it.
fn is_assignment(word: &str) -> bool {
    let Some((name, _)) = word.split_once('=') else {
        return false;
    };
    crate::ast::is_name(name)
}

/// A COMPLETE redirection — the operator and its target in ONE word, which
/// POSIX allows before the command word (`2>/dev/null cmd`). A bare `>` is not
/// one: the word after that operator is the target, so `> ec` is a filename.
fn is_redirection(word: &str) -> bool {
    let rest = word.trim_start_matches(|c: char| c.is_ascii_digit());
    // Longest first, or `>>f` matches `>` with the target `>f`.
    for op in [">>", "<>", ">&", "<&", ">|", ">", "<"] {
        if let Some(target) = rest.strip_prefix(op) {
            return !target.is_empty();
        }
    }
    false
}

/// The reserved words a COMMAND follows. Not all of them: `for`, `in` and
/// `case` take a name or a word list, and `fi`/`done`/`esac` end a command
/// rather than introduce one.
fn introduces_command(word: &str) -> bool {
    matches!(word, "!" | "{" | "do" | "elif" | "else" | "if" | "then" | "until" | "while")
}

/// Whether `pos` sits inside an unclosed quote. Completion DECLINES there:
/// this module does not understand quoting, and a blank inside `'…'` is not a
/// word break, so `cat 'foo ba` would otherwise complete `ba` on its own and
/// rewrite it inside the quote.
fn in_quote(buf: &str, pos: usize) -> bool {
    let mut quote: Option<char> = None;
    let mut esc = false;
    for (i, c) in buf.char_indices() {
        if i >= pos {
            break;
        }
        if esc {
            esc = false;
            continue;
        }
        match quote {
            None => match c {
                '\\' => esc = true,
                '\'' | '"' => quote = Some(c),
                _ => {}
            },
            // A backslash inside `'…'` is an ordinary character; only the
            // closing quote is special.
            Some('\'') => {
                if c == '\'' {
                    quote = None;
                }
            }
            Some(_) => match c {
                '\\' => esc = true,
                '"' => quote = None,
                _ => {}
            },
        }
    }
    quote.is_some()
}

/// A name with a control character in it is not offered at all. The editor
/// drops control bytes on INPUT precisely so one cannot reach the buffer, and
/// draws what is there byte for byte — a completion carrying an ESC would hand
/// the terminal an escape sequence out of a filename, and a CR or a newline
/// would break the redraw's one-row invariant. The listing is the same
/// argument, and it is the same list.
fn printable(name: &str) -> bool {
    !name.chars().any(char::is_control)
}

/// The longest prefix every candidate shares, by CHARACTER: a byte-wise answer
/// could cut a multi-byte character in half and put half of it in the line.
fn common_prefix(matches: &[String]) -> String {
    let Some(first) = matches.first() else {
        return String::new();
    };
    let mut end = first.len();
    for m in matches.iter().skip(1) {
        let mut i = 0usize;
        for ((ai, a), (_, b)) in first.char_indices().zip(m.char_indices()) {
            if a != b {
                break;
            }
            i = ai + a.len_utf8();
        }
        end = end.min(i);
    }
    first.get(..end).unwrap_or("").to_string()
}

/// A space in a completed filename has to come back ESCAPED, or the word the
/// shell re-reads is two words. The set is busybox's `is_special_char` plus the
/// two blanks it does not list, which between them are every character this
/// shell would read as something other than one more byte of the word.
fn escape(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        if breaks(c)
            || matches!(c, '\\' | '\'' | '"' | '#' | '$' | '~' | '`' | '?' | '*' | '[' | '{')
        {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Undo `escape`, so the text in the line can be matched against real names.
fn unescape(word: &str) -> String {
    let mut out = String::with_capacity(word.len());
    let mut esc = false;
    for c in word.chars() {
        if esc {
            out.push(c);
            esc = false;
        } else if c == '\\' {
            esc = true;
        } else {
            out.push(c);
        }
    }
    out
}

/// Complete the word under the cursor, or `None` when there is nothing to
/// complete — which is what leaves Tab meaning a literal tab.
pub fn complete(buf: &str, pos: usize, src: &Source<'_>) -> Option<Completion> {
    if in_quote(buf, pos) {
        return None;
    }
    let start = word_start(buf, pos);
    let word = buf.get(start..pos)?;
    let plain = unescape(word);
    // A word with a `/` in it is a PATHNAME even in command position: `./scr`
    // and `bin/tool` name a file to run, not a name on `PATH`. busybox splits
    // on exactly that test (`strrchr(command, '/')`, lineedit.c:851).
    let cmd = command_position(buf, start) && !plain.contains('/');
    // An empty word in command position would list every executable on PATH;
    // ash does that and it is not worth the screenful here. Everywhere else an
    // empty word lists the directory, which IS useful.
    if word.is_empty() && cmd {
        return None;
    }
    let (matches, keep) = if cmd {
        let mut hit: Vec<(String, bool)> = Vec::new();
        // Filtered again here, and not only by the source: the prefix passed
        // there is a chance to do less work, not a promise this relies on.
        for n in (src.commands)(&plain) {
            if n.starts_with(&plain) && printable(&n) {
                hit.push((n, false));
            }
        }
        // ash offers the working directory's SUBDIRECTORIES here too, and only
        // those (lineedit.c:904) -- a directory can begin a command word, so
        // `sr<Tab>` giving `src/` is how you walk to `src/tool`; a plain file
        // cannot, so it is not offered.
        for (name, is_dir) in (src.entries)("", &plain) {
            if is_dir && name.starts_with(&plain) && printable(&name) {
                hit.push((name, true));
            }
        }
        (hit, String::new())
    } else {
        // Split at the last `/`: everything up to it names the directory and is
        // KEPT, so `src/li` completes to `src/line.rs` rather than replacing
        // the path with a bare name. Kept VERBATIM, off the typed word rather
        // than re-escaped off `plain` -- the operator's own text is already a
        // word this shell reads, and escaping it again would turn a `~/` the
        // shell expands into a `\~/` it does not.
        let (dir_raw, leaf_raw) = match word.rfind('/') {
            Some(i) => (word.get(..i + 1).unwrap_or(""), word.get(i + 1..).unwrap_or("")),
            None => ("", word),
        };
        let dir = unescape(dir_raw);
        let leaf = unescape(leaf_raw);
        let mut hit: Vec<(String, bool)> = Vec::new();
        for (name, is_dir) in (src.entries)(&dir, &leaf) {
            if name.starts_with(&leaf) && printable(&name) {
                hit.push((name, is_dir));
            }
        }
        (hit, dir_raw.to_string())
    };
    if matches.is_empty() {
        return None;
    }
    let mut names: Vec<String> = matches.iter().map(|(n, _)| n.clone()).collect();
    names.sort();
    names.dedup();
    let shared = common_prefix(&names);
    // A unique match is finished: a directory gets `/` so the next Tab walks
    // into it, and anything else a space, so the next word starts. `all` rather
    // than `any`, so a name that is BOTH a command and a directory ends the
    // word -- the command is what that word would run.
    let unique = names.len() == 1;
    let is_dir = unique && matches.iter().all(|(_, d)| *d);
    let mut insert = format!("{keep}{}", escape(&shared));
    if unique {
        insert.push(if is_dir { '/' } else { ' ' });
    }
    Some(Completion { start, end: pos, insert, matches: names })
}

/// Lay the matches out for what a second Tab prints, as ash's `showfiles`
/// does: the widest name plus two spaces is the column, and the names run DOWN
/// each column rather than across, so a sorted list reads top-to-bottom. Every
/// row ends with a newline, including the last.
///
/// Measured in COLUMNS by the editor's own `display_cols`, so a filename with a
/// double-width or a control character in it aligns like the line it would be
/// typed into rather than like its character count.
pub fn listing(names: &[String], width: u16) -> String {
    let n = names.len();
    if n == 0 {
        return String::new();
    }
    let mut colw = 0usize;
    for s in names {
        colw = colw.max(crate::line::display_cols(s));
    }
    colw = colw.saturating_add(2);
    let ncols = (usize::from(width) / colw).max(1);
    let nrows = n.div_ceil(ncols);
    let mut out = String::new();
    for row in 0..nrows {
        let mut i = row;
        while let Some(s) = names.get(i) {
            out.push_str(s);
            let next = i.saturating_add(nrows);
            // The last name on a row is not padded, so a listing does not end
            // in a run of spaces the terminal would wrap on.
            if next < n {
                for _ in crate::line::display_cols(s)..colw {
                    out.push(' ');
                }
            }
            i = next;
        }
        out.push('\n');
    }
    out
}

// The two candidate sources the shell itself supplies. They are the impure half
// of this module -- `PATH` and the filesystem -- kept behind `Source` so the
// policy above stays a function of its arguments.

/// Everything beginning with `prefix` that can begin a command: the builtins,
/// the shell's functions and aliases, and the executable files on `PATH`. Order
/// and duplicates do not matter; the policy sorts and dedups.
pub fn commands(sh: &crate::exec::Shell, prefix: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for n in crate::builtin::NAMES {
        if n.starts_with(prefix) {
            out.push((*n).to_string());
        }
    }
    for n in sh.funcs.defined_names().chain(sh.aliases.keys()) {
        if n.starts_with(prefix) {
            out.push(n.clone());
        }
    }
    for dir in sh.get_var("PATH").unwrap_or_default().split(':') {
        // An empty PATH element is the working directory, as it is everywhere
        // else this shell reads PATH.
        let Ok(rd) = std::fs::read_dir(shell_path(sh, dir)) else {
            continue;
        };
        for e in rd.flatten() {
            let name = e.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if !name.starts_with(prefix) {
                continue;
            }
            // The stat comes AFTER the prefix test. Asking the kernel about
            // every file on `PATH` before comparing a character is a pause the
            // operator feels on a system with a large `/usr/bin`.
            //
            // Through the link and testing the mode, so a directory on `PATH`
            // is not offered as a command and neither is a file nothing can
            // run.
            if std::fs::metadata(e.path()).is_ok_and(|m| m.is_file() && executable(&m)) {
                out.push(name.to_string());
            }
        }
    }
    out
}

/// The entries of `dir` beginning with `prefix`, as (name, is_directory);
/// `dir` is "" for the working directory. `.` and `..` are absent because
/// `read_dir` does not yield them, which is the same exclusion busybox makes
/// by hand.
pub fn entries(sh: &crate::exec::Shell, dir: &str, prefix: &str) -> Entries {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(shell_path(sh, dir)) else {
        return out;
    };
    for e in rd.flatten() {
        let name = e.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.starts_with(prefix) {
            continue;
        }
        // `d_type`, off the directory read itself, so listing a whole directory
        // costs no syscall per entry -- but a symlink has to be FOLLOWED to
        // know what it points at, which is what makes a link to a directory
        // complete with a `/` and walk.
        let is_dir = match e.file_type() {
            Ok(ft) if !ft.is_symlink() => ft.is_dir(),
            _ => std::fs::metadata(e.path()).is_ok_and(|m| m.is_dir()),
        };
        out.push((name.to_string(), is_dir));
    }
    out
}

/// A relative path against the SHELL's directory, not the process's. `cd`
/// moves `sh.cwd` and never `chdir`s, so a bare `read_dir(".")` here lists the
/// directory td-sh was STARTED in for the rest of the session -- and it does
/// not fail, it silently completes the wrong names.
///
/// A leading `~` is expanded on `tilde_split`'s exact rule -- the whole word
/// or up to the first `/`, and only when `HOME` is set -- so `~/<Tab>` reaches
/// the directory the shell would run the command in. Without it the `read_dir`
/// fails, and a failed completion is a literal tab drawn as a space: nothing
/// the operator can see went wrong.
fn shell_path(sh: &crate::exec::Shell, dir: &str) -> std::path::PathBuf {
    let dir = match dir.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => match sh.get_var("HOME") {
            Some(home) => format!("{home}{rest}"),
            None => dir.to_string(),
        },
        _ => dir.to_string(),
    };
    sh.resolve(if dir.is_empty() { "." } else { &dir })
}

fn executable(m: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    m.permissions().mode() & 0o111 != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn src<'a>(
        cmds: &'a [&'a str],
        files: &'a [(&'a str, &'a str, bool)],
    ) -> (impl Fn(&str) -> Vec<String> + 'a, impl Fn(&str, &str) -> Entries + 'a) {
        // Both FILTER by the prefix rather than ignoring it, even though the
        // policy filters again: a policy that asked for the wrong prefix --
        // the whole word where the leaf was due -- gets nothing back here
        // instead of an answer that quietly agrees.
        let c = move |p: &str| {
            cmds.iter().filter(|s| s.starts_with(p)).map(|s| (*s).to_string()).collect()
        };
        let e = move |dir: &str, p: &str| {
            let mut out = Vec::new();
            for (d, n, is_dir) in files {
                if *d == dir && n.starts_with(p) {
                    out.push(((*n).to_string(), *is_dir));
                }
            }
            out
        };
        (c, e)
    }

    fn run(buf: &str, cmds: &[&str], files: &[(&str, &str, bool)]) -> Option<Completion> {
        let (c, e) = src(cmds, files);
        complete(buf, buf.len(), &Source { commands: &c, entries: &e })
    }

    /// The first word of a command completes against the command names and the
    /// rest against pathnames, which is ash's split (`complete_cmd_dir_file`).
    #[test]
    fn the_first_word_completes_commands_and_the_rest_pathnames() {
        let cmds = ["echo", "echoes", "export", "printf"];
        let files = [("", "echoes.txt", false), ("", "elm", true)];
        // Two commands share `echo`, so the shared prefix goes in and no
        // pathname is offered even though one matches.
        let got = run("ec", &cmds, &files).unwrap();
        assert_eq!(got.insert, "echo");
        assert_eq!(got.matches, ["echo", "echoes"]);
        // Unique, so it finishes with a space.
        assert_eq!(run("pri", &cmds, &files).unwrap().insert, "printf ");
        // Past the command word it is pathnames, and the command names are not
        // offered: `echoes.txt` matches, `echo` must not.
        let got = run("printf ec", &cmds, &files).unwrap();
        assert_eq!(got.matches, ["echoes.txt"]);
        assert_eq!(got.insert, "echoes.txt ");
    }

    /// A unique directory finishes with `/` rather than a space, so the next
    /// Tab walks into it; and the part of the word before the last `/` is kept
    /// rather than replaced.
    #[test]
    fn a_directory_completes_into_itself() {
        let files =
            [("", "src", true), ("src/", "line.rs", false), ("src/", "lineedit.rs", false)];
        assert_eq!(run("cat sr", &[], &files).unwrap().insert, "src/");
        let got = run("cat src/l", &[], &files).unwrap();
        assert_eq!(got.matches, ["line.rs", "lineedit.rs"]);
        assert_eq!(got.insert, "src/line", "the kept directory is not replaced");
    }

    /// Nothing to complete is what leaves Tab meaning a literal tab, which
    /// `<<-EOF` depends on. An empty word in command position is also nothing:
    /// listing every executable on PATH is not worth a screenful.
    #[test]
    fn nothing_to_complete_is_none() {
        assert!(run("", &["echo"], &[]).is_none());
        assert!(run("echo ", &["echo"], &[]).is_none(), "no entries: still nothing");
        assert!(run("zz", &["echo"], &[]).is_none(), "no match is not a completion");
        // ...but an empty word PAST the command completes the directory, which
        // is the useful half of the same case.
        let got = run("cat ", &[], &[("", "a", false), ("", "b", false)]).unwrap();
        assert_eq!(got.matches, ["a", "b"]);
    }

    /// Command position survives assignment words and the separators, and does
    /// NOT survive a redirection: after `>` the next word is a filename.
    #[test]
    fn command_position_is_where_a_command_goes() {
        let cmds = ["echo"];
        let files = [("", "ec.txt", false)];
        assert_eq!(run("ec", &cmds, &files).unwrap().matches, ["echo"]);
        assert_eq!(run("A=1 ec", &cmds, &files).unwrap().matches, ["echo"]);
        assert_eq!(run("A=1 B=2 ec", &cmds, &files).unwrap().matches, ["echo"]);
        assert_eq!(run("x; ec", &cmds, &files).unwrap().matches, ["echo"]);
        assert_eq!(run("x | ec", &cmds, &files).unwrap().matches, ["echo"]);
        assert_eq!(run("x && ec", &cmds, &files).unwrap().matches, ["echo"]);
        // A redirection target is a pathname, not a command.
        assert_eq!(run("x > ec", &cmds, &files).unwrap().matches, ["ec.txt"]);
        // ...and so is an ordinary argument.
        assert_eq!(run("x ec", &cmds, &files).unwrap().matches, ["ec.txt"]);
        // An assignment word does not RESTORE command position: the word after
        // `ls A=1` is `ls`'s second argument, not a command.
        assert_eq!(run("ls A=1 ec", &cmds, &files).unwrap().matches, ["ec.txt"]);
    }

    /// The run of assignments is walked leftwards rather than recursed into: a
    /// pasted line is as long as the operator likes. The recursive form aborts
    /// here in the gate's own build (`cargo test`, no `--release`) and NOT in
    /// an optimised one, where the tail call becomes a jump -- so this pins the
    /// loop where the stack is 2 MiB and takes the optimiser's word for the
    /// rest, which is the wrong way round to leave it as recursion.
    #[test]
    fn a_long_run_of_assignments_does_not_walk_the_stack() {
        let mut line = "A=1 ".repeat(50_000);
        line.push_str("ec");
        assert_eq!(run(&line, &["echo"], &[]).unwrap().matches, ["echo"]);
    }

    /// A dotfile IS offered on an empty word. That is ash's answer and not
    /// modern bash's: `complete_cmd_dir_file` filters only `.` and `..`,
    /// which `std::fs::read_dir` never yields in the first place. And a name
    /// with a space comes back ESCAPED -- otherwise the word the shell
    /// re-reads is two.
    #[test]
    fn dotfiles_are_offered_and_spaces_come_back_escaped() {
        let files = [("", ".bashrc", false), ("", "bin", true), ("", "my file", false)];
        assert_eq!(run("cat ", &[], &files).unwrap().matches, [".bashrc", "bin", "my file"]);
        assert_eq!(run("cat .", &[], &files).unwrap().insert, ".bashrc ");
        assert_eq!(run("cat my", &[], &files).unwrap().insert, "my\\ file ");
        // ...and the escaped form is still the same word on the way back in,
        // so a second Tab does not complete `file` on its own.
        let got = run("cat my\\ fil", &[], &files).unwrap();
        assert_eq!(got.matches, ["my file"]);
        assert_eq!(got.start, 4, "the word starts at `my`, not after the escape");
        // The directory prefix is the operator's own text, put back verbatim
        // rather than re-escaped: a `~/` the shell expands must not come back
        // as a `\~/` it does not.
        let nested = [("~/", "notes", false), ("my dir/", "f", false)];
        assert_eq!(run("cat ~/no", &[], &nested).unwrap().insert, "~/notes ");
        assert_eq!(run("cat my\\ dir/", &[], &nested).unwrap().insert, "my\\ dir/f ");
    }

    /// A word with a `/` is a pathname even in command position, and command
    /// position also offers the working directory's SUBDIRECTORIES and not its
    /// files -- between them, how `./tool` and `src/tool` get typed.
    #[test]
    fn a_command_word_with_a_slash_is_a_pathname() {
        let cmds = ["echo"];
        let files = [
            ("", "bin", true),
            ("", "echo.txt", false),
            ("bin/", "tool", false),
            ("./", "echo.txt", false),
        ];
        // No slash: the command names, plus the cwd's directories and not its
        // files -- `echo.txt` matches `ec` and is still not offered.
        assert_eq!(run("ec", &cmds, &files).unwrap().matches, ["echo"]);
        assert_eq!(run("b", &cmds, &files).unwrap().insert, "bin/");
        // With one, it is a pathname: `bin/` is listed and `PATH` is not.
        let got = run("bin/t", &cmds, &files).unwrap();
        assert_eq!(got.matches, ["tool"]);
        assert_eq!(got.insert, "bin/tool ");
        // The directory prefix reaches the source VERBATIM, so `./` is a
        // request for `./` and not one the policy resolves behind its back.
        assert_eq!(run("./ec", &cmds, &files).unwrap().insert, "./echo.txt ");
    }

    /// The shared prefix is cut on a CHARACTER boundary: a byte-wise answer
    /// could put half a character in the line.
    #[test]
    fn the_shared_prefix_is_whole_characters() {
        // The PAIR matters. `日本`/`日中` share exactly three bytes, which is
        // already a boundary, so a byte-wise answer passes; `日本`/`日暮`
        // share FOUR -- all of `日` plus the first byte of the next character
        // -- and that is what a byte-wise answer cuts in half.
        assert_eq!(common_prefix(&["日本".to_string(), "日暮".to_string()]), "日");
        assert_eq!(common_prefix(&["ab".to_string(), "abc".to_string()]), "ab");
        assert_eq!(common_prefix(&["ab".to_string(), "cd".to_string()]), "");
        assert_eq!(common_prefix(&[]), "");
        let files = [("", "日本", false), ("", "日暮", false)];
        assert_eq!(run("cat 日", &[], &files).unwrap().insert, "日");
    }

    /// The listing runs DOWN each column, as ash's `showfiles` does, so a
    /// sorted list reads top-to-bottom; and a narrow terminal gets one column
    /// rather than a division by zero.
    #[test]
    fn the_listing_fills_columns_downwards() {
        let names: Vec<String> =
            ["a", "bb", "ccc", "d", "e"].iter().map(|s| (*s).to_string()).collect();
        // Column width is 3+2 = 5, so 20 columns hold four of them; five names
        // over four columns is two rows, filled down.
        assert_eq!(listing(&names, 20), "a    ccc  e\nbb   d\n");
        // Too narrow for even one column: one name per row, never zero.
        assert_eq!(listing(&names, 1), "a\nbb\nccc\nd\ne\n");
        assert_eq!(listing(&[], 80), "");
        // The last name on a row is not padded.
        assert_eq!(listing(&["ab".to_string()], 80), "ab\n");
        // A double-width name is two COLUMNS wide, not one character -- and
        // the OTHER name has to be the wider one, or the padding is `colw - w`
        // with the same `w` on both sides and the measurement cancels out.
        assert_eq!(listing(&["日".to_string(), "xxxx".to_string()], 80), "日    xxxx\n");
    }

    /// The impure half: what the shell actually offers. Checked against a real
    /// directory, since its whole job is to ask the filesystem.
    #[test]
    fn the_candidates_come_from_path_and_the_filesystem() {
        let base = std::env::temp_dir().join(format!("td-sh-comp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("sub")).unwrap();
        std::fs::write(base.join("runme"), "#!/bin/sh\n").unwrap();
        std::fs::write(base.join("plain"), "").unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            let f = std::fs::File::open(base.join("runme")).unwrap();
            f.set_permissions(std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let dir = base.to_string_lossy().into_owned();
        let mut sh = crate::exec::Shell::new();
        let mut names = entries(&sh, &dir, "");
        names.sort();
        assert_eq!(
            names,
            [
                ("plain".to_string(), false),
                ("runme".to_string(), false),
                ("sub".to_string(), true),
            ]
        );
        // The prefix is a narrowing the source may act on.
        assert_eq!(entries(&sh, &dir, "ru"), [("runme".to_string(), false)]);
        // A directory that cannot be read is no candidates, not an error.
        assert!(entries(&sh, &format!("{dir}/nope"), "").is_empty());

        sh.set_var("PATH", &dir).unwrap();
        sh.aliases.insert("myalias".to_string(), "echo".to_string());
        let cmds = commands(&sh, "");
        assert!(cmds.contains(&"runme".to_string()), "an executable on PATH is a command");
        assert!(!cmds.contains(&"plain".to_string()), "a file nothing can run is not");
        assert!(!cmds.contains(&"sub".to_string()), "a directory on PATH is not");
        assert!(cmds.contains(&"myalias".to_string()), "an alias is");
        assert!(cmds.contains(&"umask".to_string()), "and every builtin is");
        assert_eq!(commands(&sh, "myal"), ["myalias"], "and the prefix narrows");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A relative directory is resolved against the SHELL's cwd, not the
    /// process's. `cd` moves only the former, so reading `.` completes the
    /// directory td-sh was STARTED in -- for the whole session, silently.
    #[test]
    fn a_relative_directory_follows_the_shell_and_not_the_process() {
        let base = std::env::temp_dir().join(format!("td-sh-comp-cwd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("here")).unwrap();
        std::fs::write(base.join("here").join("moved-marker"), "").unwrap();
        let mut sh = crate::exec::Shell::new();
        // What `cd` does: `sh.cwd` moves and the process never `chdir`s.
        sh.cwd = base.join("here");
        assert_eq!(entries(&sh, "", ""), [("moved-marker".to_string(), false)]);
        assert_eq!(entries(&sh, ".", ""), [("moved-marker".to_string(), false)]);
        // `~/` reaches HOME on `tilde_split`'s rule, so `~/<Tab>` is not a
        // failed `read_dir` -- which would be a literal tab drawn as a space,
        // nothing the operator can see went wrong.
        sh.set_var("HOME", &base.join("here").to_string_lossy()).unwrap();
        assert_eq!(entries(&sh, "~/", ""), [("moved-marker".to_string(), false)]);
        assert_eq!(entries(&sh, "~", ""), [("moved-marker".to_string(), false)]);
        // `~user` is NOT expanded, as `tilde_split` does not expand it either.
        assert!(entries(&sh, "~nobody/", "").is_empty());
        // And an empty `PATH` element, which means the same directory.
        sh.set_var("PATH", "").unwrap();
        assert!(!commands(&sh, "moved").contains(&"moved-marker".to_string()), "not executable");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A name with a control character in it is offered by nothing. The editor
    /// drops control bytes on input so one cannot reach the buffer; a
    /// completion is the one way round that, and an ESC out of a filename is
    /// an escape sequence the terminal obeys.
    #[test]
    fn a_control_character_in_a_name_is_never_offered() {
        let files = [("", "safe.txt", false), ("", "ev\u{1b}[2Jil", false), ("", "nl\nname", false)];
        let got = run("cat ", &[], &files).unwrap();
        assert_eq!(got.matches, ["safe.txt"]);
        // ...and it is not offered as a command either.
        let cmds = ["ok", "b\u{7}d"];
        assert_eq!(run("b", &cmds, &[]), None);
        assert_eq!(run("o", &cmds, &[]).unwrap().matches, ["ok"]);
    }

    /// Inside an unclosed quote completion DECLINES. Quoting is not understood
    /// here, and a blank inside `'…'` is not a word break -- so completing the
    /// fragment after it would rewrite an unrelated suffix inside the quote.
    #[test]
    fn an_unclosed_quote_declines() {
        let files = [("", "bar", false), ("", "foo bar", false)];
        assert_eq!(run("cat 'foo ba", &[], &files), None);
        assert_eq!(run("cat \"foo ba", &[], &files), None);
        // A CLOSED quote is not inside one, so the word after it completes.
        assert_eq!(run("cat 'x' ba", &[], &files).unwrap().insert, "bar ");
        // An escaped quote opens nothing.
        assert_eq!(run("cat \\'ba", &[], &files), None, "the word is `'ba`, which matches none");
        assert_eq!(run("echo \\' ba", &[], &files).unwrap().insert, "bar ");
        // A backslash inside `'…'` is an ordinary character, so the quote that
        // follows it still CLOSES.
        assert_eq!(run("cat 'a\\' ba", &[], &files).unwrap().insert, "bar ");
    }

    /// A command can follow more than a separator: an assignment, a COMPLETE
    /// redirection, and the reserved words that introduce one. Each is only
    /// one by being in command position itself.
    #[test]
    fn a_command_follows_assignments_redirections_and_reserved_words() {
        let cmds = ["echo"];
        let files = [("", "ec.txt", false)];
        for line in [
            "A=1 ec",
            ">out ec",
            "2>/dev/null ec",
            "<in ec",
            "if true; then ec",
            "while ec",
            "until ec",
            "if true; then :; else ec",
            "for x in y; do ec",
            "{ ec",
            "! ec",
            "A=1 >out ec",
        ] {
            assert_eq!(run(line, &cmds, &files).unwrap().matches, ["echo"], "{line}");
        }
        // ...and does NOT follow a bare operator, an ordinary word, or a
        // reserved word that is itself an argument.
        for line in ["x > ec", "x >> ec", "x ec", "echo then ec", "for ec"] {
            assert_eq!(run(line, &cmds, &files).unwrap().matches, ["ec.txt"], "{line}");
        }
    }
}
