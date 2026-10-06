//! Rules (DESIGN.md §11): what a repository's `.td-agent/rules` says of a
//! tool call, and the shell matcher that reads a command for them.
//!
//! The matcher is syntactic, so a rule is not a boundary; the jail is. It
//! splits a command into its pipeline elements and `&&`, `||`, `;` and
//! `&` segments without running anything, and calls opaque any command
//! that holds one of the constructs §11 names, or one it cannot split.

use td_json::Json;

/// A repository's rules, at the top of its base commit.
pub const FILE: &str = ".td-agent/rules";
/// The largest rules file read.
pub const MAX_FILE: usize = 16 * 1024;
/// The most rules one file holds.
pub const MAX_RULES: usize = 256;
/// The longest rule line.
pub const MAX_LINE: usize = 512;
/// The most bytes of rules one store's answer carries, every base's
/// counted, and the most a conversation records.
pub const MAX_CARRIED: usize = 32 * 1024;
/// The longest reason a file was not read: only td-agent's own words,
/// and git's cut to it, so none carries the file's.
pub const MAX_WHY: usize = 256;

/// The tools a rule may name: those the tool host runs.
pub const TOOLS: &[&str] = &[
    "read_file",
    "write_file",
    "edit_file",
    "glob",
    "grep",
    "sed",
    "shell",
];

/// What a rule does with the calls it matches.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Effect {
    Deny,
    Ask,
    Allow,
}

impl Effect {
    pub fn name(self) -> &'static str {
        match self {
            Self::Deny => "deny",
            Self::Ask => "ask",
            Self::Allow => "allow",
        }
    }
}

/// One rule: an effect, a tool and, for `shell`, the argv prefix every
/// segment of a command is matched against.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Rule {
    pub effect: Effect,
    pub tool: String,
    pub prefix: Vec<String>,
}

impl Rule {
    /// One line, `<deny|ask|allow> <tool> [word ...]`, its words parted by
    /// spaces or tabs.
    pub fn parse(line: &str) -> Result<Self, String> {
        if line.len() > MAX_LINE {
            return Err(format!("past {MAX_LINE} bytes"));
        }
        let mut words = line.split([' ', '\t']).filter(|w| !w.is_empty());
        let effect = match words.next() {
            Some("deny") => Effect::Deny,
            Some("ask") => Effect::Ask,
            Some("allow") => Effect::Allow,
            _ => return Err("does not start with deny, ask or allow".into()),
        };
        let tool = words.next().ok_or("names no tool")?;
        let Some(tool) = TOOLS.iter().copied().find(|one| *one == tool) else {
            return Err("names a tool no rule applies to".into());
        };
        let prefix: Vec<String> = words.map(str::to_string).collect();
        if !prefix.is_empty() && tool != "shell" {
            return Err(format!("gives {tool} words, which only shell takes"));
        }
        if let Some(at) = prefix.iter().position(|w| !plain(w)) {
            return Err(format!(
                "word {} holds what the shell would read, not pass",
                at + 3
            ));
        }
        Ok(Self {
            effect,
            tool: tool.to_string(),
            prefix,
        })
    }

    /// Its line, as `parse` reads it.
    pub fn text(&self) -> String {
        let mut text = format!("{} {}", self.effect.name(), self.tool);
        for word in &self.prefix {
            text.push(' ');
            text.push_str(word);
        }
        text
    }
}

/// Whether `word` reaches a program as written, so the matcher can
/// match it: no quoting, escape, expansion, glob, comment, operator or
/// control.
fn plain(word: &str) -> bool {
    !word.starts_with('~')
        && !word.chars().any(|c| {
            c.is_control()
                || matches!(
                    c,
                    '\'' | '"'
                        | '\\'
                        | '$'
                        | '`'
                        | ';'
                        | '&'
                        | '|'
                        | '<'
                        | '>'
                        | '('
                        | ')'
                        | '#'
                        | '*'
                        | '?'
                        | '['
                        | ']'
                        | '{'
                        | '}'
                )
        })
}

/// A rules file's text: its rules, none for blank lines and `#`
/// comments, or why it is refused whole. A repository's may only deny or
/// ask (DESIGN.md §11).
pub fn parse(text: &str, repository: bool) -> Result<Vec<Rule>, String> {
    let mut rules = Vec::new();
    for (at, line) in text.lines().enumerate() {
        let line = line.trim_matches([' ', '\t']);
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let rule = Rule::parse(line).map_err(|why| format!("line {}: {why}", at + 1))?;
        if repository && rule.effect == Effect::Allow {
            return Err(format!(
                "line {}: a repository's rules may only deny or ask",
                at + 1
            ));
        }
        if rules.len() == MAX_RULES {
            return Err(format!("more than {MAX_RULES} rules"));
        }
        rules.push(rule);
    }
    Ok(rules)
}

/// A repository's rules as the git worker read them at its base.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum Read {
    /// There is no `.td-agent/rules` at the base's top.
    #[default]
    Absent,
    Found(Vec<Rule>),
    /// Refused or not read, and why; every call that changes the
    /// workspace or runs a command then goes to the human.
    Unread {
        why: String,
    },
}

impl Read {
    /// Not read, and why, cut to `MAX_WHY` bytes on a character.
    pub fn unread(why: &str) -> Self {
        let mut end = why.len().min(MAX_WHY);
        while !why.is_char_boundary(end) {
            end -= 1;
        }
        Self::Unread {
            why: why.get(..end).unwrap_or_default().to_string(),
        }
    }

    /// The file's bytes, as found at the base.
    pub fn of(bytes: Option<Vec<u8>>) -> Self {
        let Some(bytes) = bytes else {
            return Self::Absent;
        };
        if bytes.len() > MAX_FILE {
            return Self::unread(&format!("{FILE} is past {MAX_FILE} bytes"));
        }
        let Ok(text) = String::from_utf8(bytes) else {
            return Self::unread(&format!("{FILE} is not UTF-8"));
        };
        match parse(&text, true) {
            Ok(rules) => Self::Found(rules),
            Err(why) => Self::unread(&format!("{FILE}: {why}")),
        }
    }

    /// The bytes of rule text it carries.
    pub fn carried(&self) -> usize {
        match self {
            Self::Found(rules) => rules.iter().map(|r| r.text().len() + 1).sum(),
            Self::Absent | Self::Unread { .. } => 0,
        }
    }

    pub fn to_json(&self) -> Json {
        let kind = |kind: &str| ("kind".to_string(), Json::Str(kind.into()));
        match self {
            Self::Absent => Json::Obj(vec![kind("absent")]),
            Self::Found(rules) => Json::Obj(vec![
                kind("found"),
                (
                    "rules".into(),
                    Json::Arr(rules.iter().map(|r| Json::Str(r.text())).collect()),
                ),
            ]),
            Self::Unread { why } => {
                Json::Obj(vec![kind("unread"), ("why".into(), Json::Str(why.clone()))])
            }
        }
    }

    /// As `to_json` wrote it, each rule read again as a repository's.
    pub fn from_json(value: &Json) -> Result<Self, String> {
        match value.get("kind").and_then(Json::as_str) {
            Some("absent") => Ok(Self::Absent),
            Some("found") => {
                let lines = value
                    .get("rules")
                    .and_then(Json::as_arr)
                    .ok_or("rules without `rules`")?;
                if lines.len() > MAX_RULES {
                    return Err(format!("more than {MAX_RULES} rules"));
                }
                let mut rules = Vec::new();
                for line in lines {
                    let rule = Rule::parse(line.as_str().ok_or("a rule that is not text")?)?;
                    if rule.effect == Effect::Allow {
                        return Err("a repository's rule that allows".into());
                    }
                    rules.push(rule);
                }
                Ok(Self::Found(rules))
            }
            Some("unread") => {
                let why = value
                    .get("why")
                    .and_then(Json::as_str)
                    .ok_or("rules unread without `why`")?;
                if why.len() > MAX_WHY {
                    return Err("rules unread for a reason past its bound".into());
                }
                Ok(Self::unread(why))
            }
            _ => Err("rules of no known kind".into()),
        }
    }
}

/// A word of a command as the shell would pass it, or none when an
/// expansion or a glob decides it.
pub type Word = Option<String>;

/// A command as the matcher reads it: its segments' argv, and whether it
/// holds an opaque construct (DESIGN.md §11), when no rule but a deny or
/// ask with no words can see what it runs.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Parsed {
    pub segments: Vec<Vec<Word>>,
    pub opaque: bool,
}

/// Command words that run another command the matcher cannot see.
const WRAPPERS: &[&str] = &[
    "eval",
    "exec",
    "command",
    "builtin",
    ".",
    "source",
    "env",
    "xargs",
    "nohup",
    "nice",
    "ionice",
    "taskset",
    "chrt",
    "setsid",
    "stdbuf",
    "flock",
    "time",
    "timeout",
    "unshare",
    "chroot",
    "nsenter",
    "sudo",
    "doas",
    "su",
    "script",
    "watch",
    "strace",
    "setpriv",
    "prlimit",
    "systemd-run",
    "bwrap",
    "parallel",
    "busybox",
    "alias",
    "trap",
    "hash",
];

/// Reserved words, which make a command compound: the splitter reads
/// simple commands only.
const KEYWORDS: &[&str] = &[
    "if", "then", "elif", "else", "fi", "for", "while", "until", "do", "done", "case", "esac",
    "function", "select", "coproc", "{", "}", "!", "[[", "]]",
];

/// Shells whose `-c` runs a string as a command.
const SHELLS: &[&str] = &["sh", "bash", "dash", "zsh", "ksh", "fish", "td-sh"];

/// `find`'s options that run a command.
const FIND_RUNS: &[&str] = &["-exec", "-execdir", "-ok", "-okdir"];

/// `git`'s options before its subcommand that take no argument of their
/// own: another, written without `=`, may take the next word, which the
/// matcher would read as the subcommand.
const GIT_FLAGS: &[&str] = &[
    "-p",
    "-P",
    "--paginate",
    "--no-pager",
    "--bare",
    "--no-replace-objects",
    "--no-lazy-fetch",
    "--no-optional-locks",
    "--no-advice",
    "--literal-pathspecs",
    "--glob-pathspecs",
    "--noglob-pathspecs",
    "--icase-pathspecs",
];

/// `git`'s options before its subcommand that change what runs or where.
const GIT_OPTIONS: &[&str] = &[
    "-c",
    "-C",
    "--config-env",
    "--exec-path",
    "--git-dir",
    "--work-tree",
    "--namespace",
];

/// `command` read for rules: split, and opaque when it holds a construct
/// §11 names or cannot be split.
pub fn split(command: &str) -> Parsed {
    match Splitter::default().run(command) {
        Some(segments) => {
            let opaque = segments.iter().any(|segment| opaque(segment));
            Parsed { segments, opaque }
        }
        None => Parsed {
            segments: Vec::new(),
            opaque: true,
        },
    }
}

/// The name a command word runs as: the part after its last `/`.
fn program(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or(word)
}

/// Whether one simple command's argv holds an opaque construct.
fn opaque(segment: &[Word]) -> bool {
    let Some(first) = segment.first() else {
        return false;
    };
    let Some(first) = first.as_deref() else {
        // What runs is an expansion's.
        return true;
    };
    if assignment(first) || KEYWORDS.contains(&first) {
        return true;
    }
    let name = program(first);
    if WRAPPERS.contains(&name) {
        return true;
    }
    let rest = segment.get(1..).unwrap_or_default();
    if SHELLS.contains(&name) && !runs_a_script(rest) {
        return true;
    }
    if name == "find"
        && rest
            .iter()
            .any(|word| word.as_deref().is_none_or(|w| FIND_RUNS.contains(&w)))
    {
        return true;
    }
    if name == "git" {
        for word in rest {
            let Some(word) = word.as_deref() else {
                // An option an expansion decides, before the subcommand.
                return true;
            };
            if !word.starts_with('-') {
                break;
            }
            let option = word.split('=').next().unwrap_or(word);
            if GIT_OPTIONS.contains(&option) || (!word.contains('=') && !GIT_FLAGS.contains(&word))
            {
                return true;
            }
        }
    }
    false
}

/// Whether a shell given `rest` runs a script file: not a string given
/// `-c` or `--command`, nor commands read from its input (`-s`, or no
/// script named), nor an option an expansion decides.
fn runs_a_script(rest: &[Word]) -> bool {
    for word in rest {
        let Some(word) = word.as_deref() else {
            return false;
        };
        if word == "--command" || word.starts_with("--command=") {
            return false;
        }
        if word.starts_with("--") {
            continue;
        }
        if let Some(flags) = word.strip_prefix('-') {
            if flags.contains(['c', 's']) {
                return false;
            }
            continue;
        }
        return true;
    }
    false
}

/// Whether `word` is a `NAME=value` or `NAME+=value` assignment.
fn assignment(word: &str) -> bool {
    let Some((name, _)) = word.split_once('=') else {
        return false;
    };
    let name = name.strip_suffix('+').unwrap_or(name);
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c == '_' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

/// `command` with each `\` and newline the shell joins taken out, as
/// the shell takes them out before reading operators and words: not
/// inside single quotes, nor in a comment, which a newline ends.
fn folded(command: &str) -> String {
    let mut out = String::with_capacity(command.len());
    let mut chars = command.chars().peekable();
    let (mut single, mut double) = (false, false);
    while let Some(c) = chars.next() {
        if single {
            single = c != '\'';
            out.push(c);
            continue;
        }
        match c {
            '\\' if chars.peek() == Some(&'\n') => {
                chars.next();
            }
            '\\' => {
                out.push(c);
                if let Some(next) = chars.next() {
                    out.push(next);
                }
            }
            '\'' if !double => {
                single = true;
                out.push(c);
            }
            '"' => {
                double = !double;
                out.push(c);
            }
            '#' if !double
                && out
                    .chars()
                    .last()
                    .is_none_or(|before| " \t\n;&|()<>".contains(before)) =>
            {
                out.push(c);
                while let Some(c) = chars.next_if(|c| *c != '\n') {
                    out.push(c);
                }
            }
            c => out.push(c),
        }
    }
    out
}

/// The splitter's state over one command.
#[derive(Default)]
struct Splitter {
    segments: Vec<Vec<Word>>,
    words: Vec<Word>,
    word: String,
    /// Whether a word has started, though it may be empty (`''`).
    started: bool,
    /// Whether an expansion or glob decides the word.
    unknown: bool,
    /// Whether any of the word was quoted or escaped.
    quoted: bool,
    /// Whether the next word is a redirection's target, not argv.
    target: bool,
    /// Whether the last segment ended with an operator that needs another.
    joined: bool,
}

impl Splitter {
    /// The segments, or none when the command cannot be split.
    fn run(mut self, command: &str) -> Option<Vec<Vec<Word>>> {
        let command = folded(command);
        let mut chars = command.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                ' ' | '\t' => self.end_word(),
                // A line may end after an operator that needs another.
                '\n' if self.joined && !self.started && self.words.is_empty() => {}
                '\n' => self.end_segment(false)?,
                '#' if !self.started => while chars.next_if(|c| *c != '\n').is_some() {},
                '\'' => {
                    self.started = true;
                    self.quoted = true;
                    loop {
                        match chars.next()? {
                            '\'' => break,
                            c => self.word.push(c),
                        }
                    }
                }
                '"' => {
                    self.started = true;
                    self.quoted = true;
                    loop {
                        match chars.next()? {
                            '"' => break,
                            '\\' => match chars.next()? {
                                '\n' => {}
                                c @ ('$' | '`' | '"' | '\\') => self.word.push(c),
                                c => {
                                    self.word.push('\\');
                                    self.word.push(c);
                                }
                            },
                            '`' => return None,
                            '$' => self.dollar(&mut chars, true)?,
                            c => self.word.push(c),
                        }
                    }
                }
                '\\' => match chars.next()? {
                    '\n' => {}
                    c => {
                        self.started = true;
                        self.quoted = true;
                        self.word.push(c);
                    }
                },
                '`' | '(' | ')' => return None,
                '$' => {
                    self.started = true;
                    self.dollar(&mut chars, false)?;
                }
                '*' | '?' | '[' | '{' | '}' => {
                    self.started = true;
                    self.unknown = true;
                    self.word.push(c);
                }
                '~' if !self.started => {
                    self.started = true;
                    self.unknown = true;
                    self.word.push(c);
                }
                ';' => {
                    if chars.next_if_eq(&';').is_some() {
                        return None;
                    }
                    self.end_segment(false)?;
                }
                '|' => {
                    let both = chars.next_if_eq(&'|').is_some();
                    if !both {
                        chars.next_if_eq(&'&');
                    }
                    self.end_segment(true)?;
                }
                '&' => {
                    if chars.next_if_eq(&'&').is_some() {
                        self.end_segment(true)?;
                    } else if chars.next_if_eq(&'>').is_some() {
                        // `&>` redirects both streams in bash, but `sh`
                        // may be dash, where it ends a command in the
                        // background and the next word starts one.
                        return None;
                    } else {
                        self.end_segment(false)?;
                    }
                }
                '<' | '>' => {
                    // A file descriptor's number names where, not argv.
                    if self.started
                        && !self.quoted
                        && !self.unknown
                        && self.word.bytes().all(|b| b.is_ascii_digit())
                    {
                        self.word.clear();
                        self.started = false;
                    }
                    if chars.peek() == Some(&'(') {
                        return None;
                    }
                    if c == '<' && chars.next_if_eq(&'<').is_some() {
                        // A here-document's body is lines the splitter
                        // does not read; a here-string's is one word.
                        chars.next_if_eq(&'<')?;
                    } else if c == '<' {
                        chars.next_if(|n| matches!(n, '>' | '&'));
                    } else {
                        chars.next_if(|n| matches!(n, '>' | '|' | '&'));
                    }
                    self.redirect()?;
                }
                c => {
                    self.started = true;
                    self.word.push(c);
                }
            }
        }
        self.end_segment(false)?;
        Some(self.segments)
    }

    /// After a `$`: a command substitution cannot be split, and a
    /// parameter decides its word; a `$` before nothing it expands is
    /// itself.
    fn dollar(
        &mut self,
        chars: &mut std::iter::Peekable<std::str::Chars>,
        quoted: bool,
    ) -> Option<()> {
        match chars.peek() {
            Some('(') => None,
            // ANSI-C and locale quoting: read as opaque.
            Some('\'') | Some('"') if !quoted => None,
            // `${NAME}`, `${#NAME}` or `${!NAME}` is one word; anything
            // more holds syntax the splitter does not read.
            Some('{') => {
                chars.next();
                let mut name = String::new();
                loop {
                    match chars.next()? {
                        '}' => break,
                        c @ ('#' | '!') if name.is_empty() => name.push(c),
                        c if c == '_' || c.is_ascii_alphanumeric() => name.push(c),
                        _ => return None,
                    }
                }
                self.unknown = true;
                self.word.push_str("${");
                self.word.push_str(&name);
                self.word.push('}');
                Some(())
            }
            Some(c) if *c == '_' || c.is_ascii_alphanumeric() => {
                self.unknown = true;
                self.word.push('$');
                Some(())
            }
            Some('@' | '*' | '#' | '?' | '-' | '$' | '!') => {
                self.unknown = true;
                self.word.push('$');
                Some(())
            }
            _ => {
                self.word.push('$');
                Some(())
            }
        }
    }

    /// A redirection's operator: what came before is a word, and the next
    /// word is its target.
    fn redirect(&mut self) -> Option<()> {
        self.end_word();
        if self.target {
            return None;
        }
        self.target = true;
        Some(())
    }

    fn end_word(&mut self) {
        if !self.started {
            return;
        }
        let word = std::mem::take(&mut self.word);
        if self.target {
            self.target = false;
        } else {
            self.words.push((!self.unknown).then_some(word));
        }
        self.started = false;
        self.unknown = false;
        self.quoted = false;
    }

    /// Ends a segment at an operator; `joined` when one must follow. An
    /// empty segment between operators, or a redirection without its
    /// target, cannot be split.
    fn end_segment(&mut self, joined: bool) -> Option<()> {
        self.end_word();
        if self.target {
            return None;
        }
        let words = std::mem::take(&mut self.words);
        if words.is_empty() {
            if self.joined || joined {
                return None;
            }
        } else {
            self.segments.push(words);
        }
        self.joined = joined;
        Some(())
    }
}

/// A rule and where it came from, as the human is told.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Sourced {
    pub rule: Rule,
    pub from: String,
}

/// What the rules say of one call, before the table (DESIGN.md §11).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Verdict {
    /// Refused, and why.
    Deny(String),
    /// The human decides, and why the rules ask, a clause the card
    /// puts after "Asked because".
    Ask(String),
    /// The rules say nothing; the table decides.
    Table,
}

/// Whether `prefix` is the start of `segment`: its command word by
/// name, wherever it is, for a deny or an ask, and as written for an
/// allow, so `./git` is not `git`; the rest word for word.
fn starts(prefix: &[String], segment: &[Word], effect: Effect) -> bool {
    if prefix.len() > segment.len() {
        return false;
    }
    prefix
        .iter()
        .zip(segment)
        .enumerate()
        .all(|(at, (want, have))| {
            let Some(have) = have.as_deref() else {
                return false;
            };
            have == want || (at == 0 && effect != Effect::Allow && program(have) == want)
        })
}

/// Whether `rule` matches a call to `tool`, given a shell call's command
/// as `split` read it.
pub fn matches(rule: &Rule, tool: &str, command: Option<&Parsed>) -> bool {
    if rule.tool != tool {
        return false;
    }
    if rule.prefix.is_empty() {
        return rule.effect != Effect::Allow || command.is_none_or(|parsed| !parsed.opaque);
    }
    let Some(parsed) = command else {
        return false;
    };
    match rule.effect {
        Effect::Allow => {
            !parsed.opaque
                && !parsed.segments.is_empty()
                && parsed
                    .segments
                    .iter()
                    .all(|segment| starts(&rule.prefix, segment, rule.effect))
        }
        Effect::Deny | Effect::Ask => parsed.segments.iter().any(|segment| {
            starts(&rule.prefix, segment, rule.effect)
                || (rule.prefix.first().is_some_and(|word| word == "git")
                    && starts(&rule.prefix, &past_options(segment), rule.effect))
        }),
    }
}

/// `segment` without the options before its subcommand, which a deny or
/// ask on `git` reads past: `git --no-pager push` is `git push`.
fn past_options(segment: &[Word]) -> Vec<Word> {
    let mut words = segment.iter();
    let mut out: Vec<Word> = words.next().cloned().into_iter().collect();
    out.extend(
        words
            .skip_while(|word| word.as_deref().is_some_and(|w| w.starts_with('-')))
            .cloned(),
    );
    out
}

/// What `rules` say of a call to `tool`, `command` its shell command,
/// `acts` whether it changes the workspace or runs one, and `unread` the
/// rules files that could not be read, each where from and why: a deny
/// wins, then an ask; a command the matcher cannot see into asks while
/// any shell deny or ask exists, and an acting call asks while a file
/// is unread.
pub fn judge(
    rules: &[Sourced],
    unread: &[(String, String)],
    tool: &str,
    command: Option<&str>,
    acts: bool,
) -> Verdict {
    let parsed = command.map(split);
    let said = |effect: Effect| {
        rules
            .iter()
            .find(|one| one.rule.effect == effect && matches(&one.rule, tool, parsed.as_ref()))
    };
    if let Some(one) = said(Effect::Deny) {
        return Verdict::Deny(format!(
            "the rule `{}` of {} denies it",
            one.rule.text(),
            one.from
        ));
    }
    if let Some(one) = said(Effect::Ask) {
        return Verdict::Ask(format!(
            "the rule `{}` of {} asks",
            one.rule.text(),
            one.from
        ));
    }
    if parsed.as_ref().is_some_and(|p| p.opaque)
        && rules
            .iter()
            .any(|one| one.rule.effect != Effect::Allow && one.rule.tool == "shell")
    {
        return Verdict::Ask(
            "the command holds a construct the rules cannot see into, and a shell deny or ask rule exists"
                .into(),
        );
    }
    if acts {
        if let Some((from, why)) = unread.first() {
            return Verdict::Ask(format!("the rules of {from} could not be read: {why}"));
        }
    }
    Verdict::Table
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;

    fn words(command: &str) -> Vec<Vec<Word>> {
        let parsed = split(command);
        assert!(!parsed.opaque, "{command:?} read as opaque");
        parsed.segments
    }

    fn known(segments: &[&[&str]]) -> Vec<Vec<Word>> {
        segments
            .iter()
            .map(|s| s.iter().map(|w| Some(w.to_string())).collect())
            .collect()
    }

    #[test]
    fn a_command_splits_at_every_operator_and_drops_redirections() {
        assert_eq!(
            words("cargo build && cargo test || echo failed; ls | wc -l & true"),
            known(&[
                &["cargo", "build"],
                &["cargo", "test"],
                &["echo", "failed"],
                &["ls"],
                &["wc", "-l"],
                &["true"]
            ])
        );
        assert_eq!(
            words("make 2>&1 >out.log </dev/null |& tee x\nls >> y 2> z"),
            known(&[&["make"], &["tee", "x"], &["ls"]])
        );
        assert_eq!(
            words(r#"git commit -m 'a b' -m "c \"d\" \$e" x\ y # note"#),
            known(&[&["git", "commit", "-m", "a b", "-m", "c \"d\" $e", "x y"]])
        );
        // An expansion or glob leaves its word unknown, not opaque.
        assert_eq!(
            words("rm -f $X *.o ~/a \"$HOME\" {a,b}"),
            vec![vec![
                Some("rm".into()),
                Some("-f".into()),
                None,
                None,
                None,
                None,
                None
            ]]
        );
        assert_eq!(words("echo $ 5"), known(&[&["echo", "$", "5"]]));
        assert_eq!(words("cat <<< word"), known(&[&["cat"]]));
        // A `\` and newline the shell joins, inside an operator, a word
        // or double quotes, but not single quotes or a comment.
        assert_eq!(
            words("echo safe >\\\n>/dev/null rm -f x"),
            known(&[&["echo", "safe", "rm", "-f", "x"]])
        );
        assert_eq!(
            words("ca\\\nrgo te\"s\\\nt\""),
            known(&[&["cargo", "test"]])
        );
        assert_eq!(words("echo 'a\\\nb'"), known(&[&["echo", "a\\\nb"]]));
        assert_eq!(
            words("echo a # b \\\nls"),
            known(&[&["echo", "a"], &["ls"]])
        );
        assert_eq!(words("echo a\\\\\nls"), known(&[&["echo", "a\\"], &["ls"]]));
        // A braced parameter is one unknown word.
        assert_eq!(
            words("echo ${HOME} ${#X}"),
            vec![vec![Some("echo".into()), None, None]]
        );
        assert_eq!(words("ls;"), known(&[&["ls"]]));
        assert_eq!(
            words("cargo build &&\n  cargo test |\n wc"),
            known(&[&["cargo", "build"], &["cargo", "test"], &["wc"]])
        );
    }

    #[test]
    fn every_construct_section_eleven_names_is_opaque() {
        for command in [
            // Cannot be split.
            "echo 'unterminated",
            "echo \"unterminated",
            "ls &&",
            "&& ls",
            "ls | | wc",
            "cat >",
            "a > > b",
            "(cd x; make)",
            "cat <<EOF\nx\nEOF",
            "case x in y) ;; esac",
            "echo \\",
            // A function definition.
            "f() { rm -rf x; }; f",
            "function f { ls; }",
            // Alias and trap.
            "alias ls='rm -rf'",
            "trap 'rm x' EXIT",
            // A leading assignment.
            "PATH=. git status",
            "X=1",
            // Shells given -c, alone or clustered, by any path.
            "sh -c 'rm -rf x'",
            "bash -lc ls",
            "/bin/dash -ec ls",
            "zsh -c ls",
            "ksh -c ls",
            "fish --command ls",
            "fish --command=ls",
            "td-sh -c ls",
            // Busybox with any applet, and every wrapper named.
            "busybox rm x",
            "eval ls",
            "exec ls",
            "command ls",
            "builtin cd",
            ". ./x",
            "source x",
            "env rm x",
            "/usr/bin/env rm x",
            "xargs rm",
            "nohup ls",
            "nice ls",
            "ionice ls",
            "taskset 1 ls",
            "chrt 1 ls",
            "setsid ls",
            "stdbuf -o0 ls",
            "flock x ls",
            "time ls",
            "timeout 5 ls",
            "unshare ls",
            "chroot / ls",
            "nsenter ls",
            "sudo ls",
            "doas ls",
            "su -",
            "script -c ls",
            "watch ls",
            "strace ls",
            "setpriv ls",
            "prlimit ls",
            "systemd-run ls",
            "bwrap ls",
            "parallel ls",
            // find running a command.
            "find . -exec rm {} +",
            "find . -execdir rm {} ;",
            "find . -ok rm {} \\;",
            "find . -okdir rm {} \\;",
            // git told to run elsewhere or with other configuration.
            "git -c core.pager=x log",
            "git -C .. status",
            "git --config-env=a.b=C log",
            "git --exec-path=. status",
            "git --git-dir=x status",
            "git --git-dir x status",
            "git --work-tree=x status",
            "git --namespace=x status",
            "git $OPT status",
            "git --attr-source HEAD push",
            "git --unknown push",
            // Substitutions and backticks.
            "echo $(rm x)",
            "echo \"$(rm x)\"",
            "echo `rm x`",
            "echo \"`rm x`\"",
            "diff <(ls) >(ls)",
            "echo $'a'",
            // A braced expansion holding more than a name.
            "echo ${UNSET:-safe; rm -f victim}",
            "echo \"${X:-a}\"",
            "echo ${X",
            // `&>`, which dash reads as `&` and `>`.
            "2&>/dev/null rm -f victim",
            "ls &>x git push",
            // An append assignment.
            "X+=1 git push",
            // A shell or find option an expansion decides.
            "sh -$X 'git push'",
            "find . $X rm {} +",
            // A shell reading commands from its input, and hash.
            "echo git push | sh",
            "sh <<< 'git push'",
            "bash -s",
            "bash -xs arg",
            "sh -",
            "hash -p /usr/bin/git ls",
            // A command word an expansion decides.
            "$CMD x",
            "\"$CMD\" x",
            // Compound commands.
            "if true; then ls; fi",
            "for x in a; do ls; done",
            "! ls",
            "{ ls; }",
            "[[ -f x ]]",
            // Any one segment's construct makes the whole command opaque.
            "ls && sh -c x",
        ] {
            assert!(split(command).opaque, "{command:?} was not opaque");
        }
        // Options after git's subcommand are that subcommand's.
        assert!(!split("git log -C -c").opaque);
        assert!(!split("git --no-pager log").opaque);
        // A shell running a script, or given another option, is not.
        assert!(!split("bash build.sh").opaque);
        assert!(!split("sh --norc x").opaque);
        assert!(!split("find . -name x").opaque);
    }

    fn rule(line: &str) -> Rule {
        Rule::parse(line).unwrap()
    }

    #[test]
    fn a_shell_rule_matches_any_segment_and_an_allow_every_one() {
        let deny = rule("deny shell rm -rf");
        let parsed = |c: &str| split(c);
        assert!(matches(&deny, "shell", Some(&parsed("ls && rm -rf x"))));
        assert!(matches(
            &deny,
            "shell",
            Some(&parsed("cat x | /bin/rm -rf y"))
        ));
        assert!(!matches(&deny, "shell", Some(&parsed("rm -r x"))));
        assert!(!matches(&deny, "shell", Some(&parsed("echo rm -rf"))));
        // A deny or ask on git reads past git's options; an allow does
        // not.
        let push = rule("deny shell git push");
        assert!(matches(
            &push,
            "shell",
            Some(&parsed("git --no-pager push"))
        ));
        assert!(matches(&push, "shell", Some(&parsed("git -p push origin"))));
        assert!(!matches(
            &push,
            "shell",
            Some(&parsed("git --no-pager log"))
        ));
        assert!(!matches(
            &rule("allow shell git status"),
            "shell",
            Some(&parsed("git --no-pager status"))
        ));
        // Never the raw string.
        assert!(!matches(&deny, "shell", Some(&parsed("echo 'rm -rf x'"))));
        assert!(!matches(&deny, "write_file", None));
        let allow = rule("allow shell cargo test");
        assert!(matches(
            &allow,
            "shell",
            Some(&parsed("cargo test && cargo test -p x"))
        ));
        assert!(!matches(
            &allow,
            "shell",
            Some(&parsed("cargo test && rm x"))
        ));
        assert!(!matches(&allow, "shell", Some(&parsed("./cargo test"))));
        assert!(!matches(&allow, "shell", Some(&parsed("cargo $T"))));
        assert!(!matches(
            &allow,
            "shell",
            Some(&parsed("cargo test $(rm x)"))
        ));
        assert!(!matches(
            &allow,
            "shell",
            Some(&parsed("cargo test; sh -c x"))
        ));
        // A rule with no words takes every call to its tool; an allow
        // still none that is opaque.
        let any = rule("deny shell");
        assert!(matches(&any, "shell", Some(&parsed("sh -c x"))));
        assert!(!matches(
            &rule("allow shell"),
            "shell",
            Some(&parsed("sh -c x"))
        ));
        assert!(matches(&rule("ask write_file"), "write_file", None));
    }

    #[test]
    fn a_rules_file_is_read_whole_or_refused() {
        let text = "# keep pushes for people\n\n  ask shell git push\t\ndeny write_file\n";
        assert_eq!(
            parse(text, true).unwrap(),
            [rule("ask shell git push"), rule("deny write_file")]
        );
        for (text, why) in [
            (
                "allow shell ls",
                "line 1: a repository's rules may only deny or ask",
            ),
            ("deny\n", "line 1: names no tool"),
            (
                "ok\nmaybe shell",
                "line 1: does not start with deny, ask or allow",
            ),
            (
                "deny process_kill",
                "line 1: names a tool no rule applies to",
            ),
            (
                "deny read_file x",
                "line 1: gives read_file words, which only shell takes",
            ),
            (
                "deny shell rm 'x'",
                "line 1: word 4 holds what the shell would read, not pass",
            ),
            (
                "deny shell rm\u{1}",
                "line 1: word 3 holds what the shell would read, not pass",
            ),
            (
                "deny shell git push # no",
                "line 1: word 5 holds what the shell would read, not pass",
            ),
            (
                "deny shell rm *.o",
                "line 1: word 4 holds what the shell would read, not pass",
            ),
            (
                "deny shell cat ~/x",
                "line 1: word 4 holds what the shell would read, not pass",
            ),
        ] {
            assert_eq!(parse(text, true).unwrap_err(), why, "{text:?}");
        }
        assert_eq!(
            parse("allow shell ls", false).unwrap(),
            [rule("allow shell ls")]
        );
        let many = "deny glob\n".repeat(MAX_RULES + 1);
        assert!(parse(&many, true).is_err());
        assert!(Rule::parse(&format!("deny shell {}", "x".repeat(MAX_LINE))).is_err());
        assert_eq!(Read::of(None), Read::Absent);
        assert!(matches!(Read::of(Some(vec![0xff])), Read::Unread { .. }));
        assert!(matches!(
            Read::of(Some(vec![b'#'; MAX_FILE + 1])),
            Read::Unread { .. }
        ));
        let read = Read::of(Some(text.as_bytes().to_vec()));
        assert_eq!(Read::from_json(&read.to_json()).unwrap(), read);
        let unread = Read::of(Some(b"allow shell ls".to_vec()));
        assert_eq!(Read::from_json(&unread.to_json()).unwrap(), unread);
        // A reason is cut on a character, and a longer one does not cross.
        let long = Read::unread(&"é".repeat(MAX_WHY));
        assert!(matches!(&long, Read::Unread { why } if why.len() == MAX_WHY));
        let past = td_json::parse(&format!(
            r#"{{"kind":"unread","why":"{}"}}"#,
            "x".repeat(MAX_WHY + 1)
        ))
        .unwrap();
        assert!(Read::from_json(&past).is_err());
        // A forged allow does not cross.
        let forged = td_json::parse(r#"{"kind":"found","rules":["allow shell rm"]}"#).unwrap();
        assert!(Read::from_json(&forged).is_err());
    }

    #[test]
    fn the_rules_judge_a_deny_first_then_an_ask() {
        let from = |line: &str| Sourced {
            rule: rule(line),
            from: "the repository at /w/td".into(),
        };
        let rules = [from("ask shell git"), from("deny shell git push")];
        assert_eq!(
            judge(&rules, &[], "shell", Some("git status && git push"), true),
            Verdict::Deny(
                "the rule `deny shell git push` of the repository at /w/td denies it".into()
            )
        );
        assert_eq!(
            judge(&rules, &[], "shell", Some("git status"), true),
            Verdict::Ask("the rule `ask shell git` of the repository at /w/td asks".into())
        );
        assert_eq!(
            judge(&rules, &[], "shell", Some("ls"), true),
            Verdict::Table
        );
        // A command the rules cannot see into asks while a shell deny
        // or ask exists, and only then.
        assert!(matches!(
            judge(&rules, &[], "shell", Some("sh -c 'git push'"), true),
            Verdict::Ask(_)
        ));
        assert!(matches!(
            judge(
                &[from("ask shell git")],
                &[],
                "shell",
                Some("sh -c 'git push'"),
                true
            ),
            Verdict::Ask(_)
        ));
        assert_eq!(
            judge(
                &[from("deny write_file")],
                &[],
                "shell",
                Some("sh -c 'git push'"),
                true
            ),
            Verdict::Table
        );
        // An unread file asks for every call that acts, not for reads.
        let unread = [(
            "the repository at /w/x".to_string(),
            "line 1: names no tool".to_string(),
        )];
        assert_eq!(
            judge(&[], &unread, "write_file", None, true),
            Verdict::Ask(
                "the rules of the repository at /w/x could not be read: line 1: names no tool"
                    .into()
            )
        );
        assert_eq!(
            judge(&[], &unread, "read_file", None, false),
            Verdict::Table
        );
        assert_eq!(
            judge(&[from("deny read_file")], &[], "read_file", None, false),
            Verdict::Deny("the rule `deny read_file` of the repository at /w/td denies it".into())
        );
    }
}
