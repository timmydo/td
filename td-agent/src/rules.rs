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

/// The human's rules, in the state directory (DESIGN.md §11).
pub const HUMAN_FILE: &str = "rules";
/// The largest file of the human's rules, and the most rules in it.
pub const MAX_HUMAN_FILE: usize = 256 * 1024;
pub const MAX_HUMAN_RULES: usize = 4096;
/// The longest workspace key.
pub const MAX_KEY: usize = 4096;
/// The header of the rules for every workspace.
pub const EVERYWHERE: &str = "everywhere";

/// Where one of the human's rules applies.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Scope {
    /// The workspace with this key (`Workspace::key`), its forks
    /// included.
    Workspace(String),
    /// Every workspace, present and future.
    Everywhere,
}

/// One of the human's rules.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Human {
    pub scope: Scope,
    pub rule: Rule,
}

/// The header of the human's standing answers for crossings.
pub const CROSSINGS: &str = "crossings";

/// What a crossing does to the other conversation (DESIGN.md §3).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Crossed {
    Read,
    Message,
}

impl Crossed {
    pub fn name(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Message => "message",
        }
    }

    pub fn parse(word: &str) -> Option<Self> {
        match word {
            "read" => Some(Self::Read),
            "message" => Some(Self::Message),
            _ => None,
        }
    }
}

/// One of the human's standing answers for a crossing: whether
/// conversation `from` may `op` conversation `to`, that way only.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Crossing {
    pub allow: bool,
    pub op: Crossed,
    pub from: String,
    pub to: String,
}

impl Crossing {
    /// One line, `<allow|deny> <read|message> <from> <to>`, two
    /// conversation ids.
    pub fn parse(line: &str) -> Result<Self, String> {
        let words: Vec<&str> = line.split([' ', '\t']).filter(|w| !w.is_empty()).collect();
        let [effect, op, from, to] = words.as_slice() else {
            return Err(
                "a crossing is `allow` or `deny`, `read` or `message`, then two conversations"
                    .into(),
            );
        };
        let allow = match *effect {
            "allow" => true,
            "deny" => false,
            _ => return Err("a crossing that neither allows nor denies".into()),
        };
        let op = Crossed::parse(op).ok_or("a crossing that neither reads nor messages")?;
        for id in [from, to] {
            if crate::store::Id::parse(id).is_none() {
                return Err(format!("{id:?} is not a conversation's id"));
            }
        }
        if from == to {
            return Err("a crossing from a conversation to itself".into());
        }
        Ok(Self {
            allow,
            op,
            from: from.to_string(),
            to: to.to_string(),
        })
    }

    /// Its line, as `parse` reads it.
    pub fn text(&self) -> String {
        let effect = if self.allow { "allow" } else { "deny" };
        format!("{effect} {} {} {}", self.op.name(), self.from, self.to)
    }
}

/// The human's rules file, read: the rules for tool calls, the standing
/// answers for crossings, and each workspace's mode, by its key.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Policy {
    pub rules: Vec<Human>,
    pub crossings: Vec<Crossing>,
    pub modes: Vec<(String, crate::config::Mode)>,
}

impl Policy {
    /// Workspace `key`'s mode, as its last `mode` line sets it.
    pub fn mode(&self, key: &str) -> Option<crate::config::Mode> {
        self.modes
            .iter()
            .rev()
            .find(|(of, _)| of == key)
            .map(|(_, mode)| *mode)
    }
}

/// A `mode` line's mode, `mode ask` or `mode auto`; none for a line that
/// is not one.
fn mode_line(line: &str) -> Option<Result<crate::config::Mode, String>> {
    let mut words = line.split([' ', '\t']).filter(|w| !w.is_empty());
    if words.next() != Some("mode") {
        return None;
    }
    Some(match (words.next(), words.next()) {
        (Some("ask"), None) => Ok(crate::config::Mode::Ask),
        (Some("auto"), None) => Ok(crate::config::Mode::Auto),
        _ => Err("a mode is `mode ask` or `mode auto`".into()),
    })
}

/// The human's rules file's rules for tool calls (`parse_policy`).
pub fn parse_human(text: &str) -> Result<Vec<Human>, String> {
    parse_policy(text).map(|policy| policy.rules)
}

/// What a header of the human's file opens.
#[derive(Clone)]
enum Section {
    Scope(Scope),
    Crossings,
}

/// The human's rules file: a header, `[everywhere]`, `[<workspace>]` or
/// `[crossings]`, then that section's lines, one a line: a workspace's
/// or every workspace's rules as a repository's are written but allow
/// rules among them, a workspace's `mode ask` or `mode auto`, or
/// crossings as `Crossing::parse` reads them; blank lines and `#`
/// comments skipped.
pub fn parse_policy(text: &str) -> Result<Policy, String> {
    if text.len() > MAX_HUMAN_FILE {
        return Err(format!("past {MAX_HUMAN_FILE} bytes"));
    }
    let mut section = None;
    let mut rules = Vec::new();
    let mut crossings = Vec::new();
    let mut modes = Vec::new();
    for (at, line) in text.lines().enumerate() {
        // Comments too: a control escapes to six bytes on the wire.
        if line.chars().any(|c| c.is_control() && c != '\t') {
            return Err(format!("line {}: a control character", at + 1));
        }
        let line = line.trim_matches([' ', '\t']);
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(key) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = Some(match key {
                EVERYWHERE => Section::Scope(Scope::Everywhere),
                CROSSINGS => Section::Crossings,
                key => Section::Scope(Scope::Workspace(
                    workspace_key(key).map_err(|why| format!("line {}: {why}", at + 1))?,
                )),
            });
            continue;
        }
        if rules.len() + crossings.len() + modes.len() == MAX_HUMAN_RULES {
            return Err(format!("more than {MAX_HUMAN_RULES} rules"));
        }
        let scope = match section.clone() {
            None => {
                return Err(format!(
                    "line {}: a rule before any [workspace], [everywhere] or [crossings]",
                    at + 1
                ))
            }
            Some(Section::Crossings) => {
                crossings
                    .push(Crossing::parse(line).map_err(|why| format!("line {}: {why}", at + 1))?);
                continue;
            }
            Some(Section::Scope(scope)) => scope,
        };
        if let Some(mode) = mode_line(line) {
            let mode = mode.map_err(|why| format!("line {}: {why}", at + 1))?;
            let Scope::Workspace(key) = scope else {
                return Err(format!(
                    "line {}: a mode is for one workspace, not every one",
                    at + 1
                ));
            };
            modes.push((key, mode));
            continue;
        }
        let rule = Rule::parse(line).map_err(|why| format!("line {}: {why}", at + 1))?;
        if rule.effect == Effect::Allow && scope == Scope::Everywhere {
            return Err(format!(
                "line {}: an allow is for one workspace, not every one",
                at + 1
            ));
        }
        rules.push(Human { scope, rule });
    }
    Ok(Policy {
        rules,
        crossings,
        modes,
    })
}

/// `text`, the human's rules file, with workspace `key` in `mode`: each
/// `mode` line under its header taken out, then one added below the
/// file's last header when that is its, else under a new one, every
/// other line kept. Refused when `text` is, or the result would be.
pub fn set_mode(text: &str, key: &str, mode: crate::config::Mode) -> Result<String, String> {
    parse_policy(text)?;
    let header = format!("[{}]", workspace_key(key)?);
    let mut kept = String::new();
    let mut under = false;
    for line in text.lines() {
        let trimmed = line.trim_matches([' ', '\t']);
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            under = trimmed == header;
        } else if under && mode_line(trimmed).is_some() {
            continue;
        }
        kept.push_str(line);
        kept.push('\n');
    }
    append(&kept, &header, &[format!("mode {}", mode.word())])
}

/// What the human's standing answers say of conversation `from` doing
/// `op` to conversation `to`: a deny wins, then an allow.
pub fn cross(crossings: &[Crossing], op: Crossed, from: &str, to: &str) -> Verdict {
    let said = |allow: bool| {
        crossings
            .iter()
            .find(|one| one.allow == allow && one.op == op && one.from == from && one.to == to)
    };
    if let Some(one) = said(false) {
        return Verdict::Deny(format!("your rule `{}` denies it", one.text()));
    }
    if let Some(one) = said(true) {
        return Verdict::Allow(format!("your rule `{}` allows it", one.text()));
    }
    Verdict::Table
}

/// `key` as a header names a workspace, as `Workspace::key` writes one:
/// `workspace <name>`, `conversation <id>` or `directory <path>`, so a
/// mistyped header is refused, not a scope no workspace has.
pub fn workspace_key(key: &str) -> Result<String, String> {
    if key.len() > MAX_KEY || key.chars().any(char::is_control) {
        return Err("a workspace header past its bound".into());
    }
    let known = match key.split_once(' ') {
        Some(("workspace", name)) => crate::workspace::workspace_name(name),
        Some(("conversation", id)) => crate::store::Id::parse(id).is_some(),
        Some(("directory", path)) => crate::workspace::escaped_path(path),
        _ => false,
    };
    if !known {
        return Err(
            "a header that is neither [everywhere] nor a workspace's: [workspace <name>], [conversation <id>] or [directory <path>]"
                .into(),
        );
    }
    Ok(key.to_string())
}

/// `rules` as the human's file holds them: each scope's under its
/// header, in the order each first appears.
pub fn human_text(rules: &[Human]) -> String {
    let mut text = String::from(
        "# Your td-agent rules: a card's \"always\" answers add them, and you may\n# remove one by deleting its line. A [workspace] header is a workspace's\n# key; [everywhere] holds the denies for every workspace.\n",
    );
    let mut scopes: Vec<&Scope> = Vec::new();
    for one in rules {
        if !scopes.contains(&&one.scope) {
            scopes.push(&one.scope);
        }
    }
    for scope in scopes {
        text.push('\n');
        match scope {
            Scope::Everywhere => text.push_str(&format!("[{EVERYWHERE}]\n")),
            Scope::Workspace(key) => text.push_str(&format!("[{key}]\n")),
        }
        for one in rules.iter().filter(|one| &one.scope == scope) {
            text.push_str(&one.rule.text());
            text.push('\n');
        }
    }
    text
}

/// The most rules one card's "always" answer writes.
pub const MAX_BODIES: usize = 8;

/// Programs whose second word names what they do, so an "always" answer
/// takes it with the program: `cargo test`, not every `cargo`.
const SUBCOMMANDED: &[&str] = &[
    "apt",
    "bundle",
    "cargo",
    "dnf",
    "docker",
    "dotnet",
    "gh",
    "git",
    "go",
    "guix",
    "just",
    "kubectl",
    "make",
    "nix",
    "npm",
    "pip",
    "pip3",
    "pnpm",
    "podman",
    "poetry",
    "rustup",
    "systemctl",
    "uv",
    "yarn",
];

/// Interpreters and build tools: what they run is the workspace's code,
/// so an allow for one is broad (DESIGN.md §11).
const BROAD: &[&str] = &[
    "bash", "bun", "bundle", "cargo", "cc", "clang", "cmake", "dash", "deno", "dotnet", "gcc",
    "go", "gradle", "java", "just", "make", "meson", "mvn", "ninja", "node", "npm", "npx", "perl",
    "php", "pnpm", "poetry", "pytest", "python", "python3", "rake", "ruby", "rustc", "sh", "tox",
    "uv", "yarn", "zsh",
];

/// What a card's "always" answers would write: each rule's tool and
/// words, `shell cargo test`, as an allow or a deny, and whether an
/// allow is offered at all.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Always {
    pub allow: bool,
    pub bodies: Vec<String>,
}

impl Always {
    /// `bodies` as a card may offer them: one to `MAX_BODIES`, each once,
    /// each a rule's tool and words as `Rule::text` writes them.
    pub fn checked(allow: bool, bodies: Vec<String>) -> Result<Self, String> {
        if bodies.is_empty() || bodies.len() > MAX_BODIES {
            return Err(format!("from one to {MAX_BODIES} rules to remember"));
        }
        for (at, body) in bodies.iter().enumerate() {
            let line = format!("deny {body}");
            if Rule::parse(&line)?.text() != line {
                return Err(format!(
                    "a rule to remember not as a rule writes it: {body:?}"
                ));
            }
            if bodies
                .get(..at)
                .is_some_and(|earlier| earlier.contains(body))
            {
                return Err(format!("a rule to remember twice: {body:?}"));
            }
        }
        Ok(Self { allow, bodies })
    }
}

/// What a card's "always" answers would remember: rules for a tool call,
/// or, for a crossing, the standing answer for `op` to conversation `to`
/// from the conversation that asks.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Offer {
    Rules(Always),
    Crossing { op: Crossed, to: String },
}

/// Whether `word` reads as a subcommand: lower case, a letter first.
fn subcommand(word: &str) -> bool {
    word.starts_with(|c: char| c.is_ascii_lowercase())
        && word
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// What a card for a call to `tool`, running `command` when it is
/// `shell`, offers to remember (DESIGN.md §11): the tool alone, or for
/// each segment of a command its program and, for a program in
/// `SUBCOMMANDED`, its subcommand. No allow for a command the matcher
/// cannot see into, a segment of redirections alone, or a subcommanded
/// program with no subcommand to name; nothing for a command with no
/// program a rule can name.
pub fn proposals(tool: &str, command: Option<&str>) -> Option<Always> {
    if !TOOLS.contains(&tool) {
        return None;
    }
    if tool == "shell" && command.is_none() {
        return None;
    }
    let Some(command) = command.filter(|_| tool == "shell") else {
        return Some(Always {
            allow: true,
            bodies: vec![tool.to_string()],
        });
    };
    let parsed = split(command);
    let mut allow = !parsed.opaque && !parsed.segments.is_empty();
    let mut bodies: Vec<String> = Vec::new();
    for words in &parsed.segments {
        // A word with a space or tab, quoted, would read back as two.
        let one = |word: &str| !word.contains([' ', '\t']);
        let Some(Some(word)) = words
            .first()
            .filter(|word| word.as_deref().is_some_and(one))
        else {
            allow = false;
            continue;
        };
        let mut body = format!("shell {word}");
        if SUBCOMMANDED.contains(&program(word)) {
            match words.get(1) {
                Some(Some(sub)) if subcommand(sub) && one(sub) => {
                    body.push(' ');
                    body.push_str(sub);
                }
                // `git -C x push` or `make` alone: a rule for the
                // program alone would allow every subcommand.
                _ => allow = false,
            }
        }
        if Rule::parse(&format!("deny {body}")).is_err() {
            allow = false;
            continue;
        }
        if !bodies.contains(&body) {
            bodies.push(body);
        }
    }
    Always::checked(allow, bodies).ok()
}

/// The program of `body`, a rule's tool and words, when an allow for it
/// is broad: an interpreter or build tool.
pub fn broad(body: &str) -> Option<&str> {
    let word = body.strip_prefix("shell ")?.split(' ').next()?;
    BROAD.contains(&program(word)).then_some(word)
}

/// `text`, the human's rules file, without what deleted conversation
/// `id` leaves: each section under the header of workspace `key`, its
/// own, so a later workspace that came to share its key takes none of
/// its rules, and each crossing to or from it; `None` when it holds
/// neither. Refused when `text` is.
pub fn forget(text: &str, key: Option<&str>, id: &str) -> Result<Option<String>, String> {
    parse_policy(text)?;
    let header = key.map(|key| format!("[{key}]"));
    let crossings = format!("[{CROSSINGS}]");
    let mut out = String::new();
    let mut dropping = false;
    let mut crossing = false;
    let mut dropped = false;
    for line in text.lines() {
        let trimmed = line.trim_matches([' ', '\t']);
        let drop = if trimmed.starts_with('[') && trimmed.ends_with(']') {
            dropping = header.as_deref() == Some(trimmed);
            crossing = trimmed == crossings;
            // The blank line that parted it from the one before goes too.
            if dropping {
                while out.ends_with("\n\n") {
                    out.pop();
                }
            }
            dropping
        } else {
            dropping
                || (crossing
                    && Crossing::parse(trimmed).is_ok_and(|one| one.from == id || one.to == id))
        };
        dropped |= drop;
        if !drop {
            out.push_str(line);
            out.push('\n');
        }
    }
    if !dropped {
        return Ok(None);
    }
    parse_policy(&out)?;
    Ok(Some(out))
}

/// `text`, the human's rules file, with `effect` rules for `bodies`
/// added under `scope`: below the file's last header when that is
/// `scope`'s, else under a new one at its end, so every line already
/// there stays as it is. A rule the scope already holds is not added
/// again. Refused when `text` is, or the result would be.
pub fn add(text: &str, scope: &Scope, effect: Effect, bodies: &[String]) -> Result<String, String> {
    let rules = parse_human(text)?;
    let header = match scope {
        Scope::Everywhere => format!("[{EVERYWHERE}]"),
        Scope::Workspace(key) => format!("[{}]", workspace_key(key)?),
    };
    let mut lines = Vec::new();
    for body in bodies {
        let rule = Rule::parse(&format!("{} {body}", effect.name()))?;
        let held = |one: &Human| &one.scope == scope && one.rule == rule;
        if !rules.iter().any(held) && !lines.contains(&rule.text()) {
            lines.push(rule.text());
        }
    }
    append(text, &header, &lines)
}

/// `text`, the human's rules file, with `crossing` added under
/// `[crossings]`, as `add` adds a rule; unchanged when it holds it.
pub fn add_crossing(text: &str, crossing: &Crossing) -> Result<String, String> {
    if parse_policy(text)?.crossings.contains(crossing) {
        return Ok(text.to_string());
    }
    append(text, &format!("[{CROSSINGS}]"), &[crossing.text()])
}

/// `text` with `lines` below the file's last header when that is
/// `header`, else under a new `header` at its end; refused when the
/// result is.
fn append(text: &str, header: &str, lines: &[String]) -> Result<String, String> {
    let mut out = text.to_string();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    let last = text
        .lines()
        .rev()
        .map(|line| line.trim_matches([' ', '\t']))
        .find(|line| line.starts_with('[') && line.ends_with(']'));
    let mut headed = last == Some(header);
    for line in lines {
        if !headed {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(header);
            out.push('\n');
            headed = true;
        }
        out.push_str(line);
        out.push('\n');
    }
    parse_policy(&out)?;
    Ok(out)
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
    /// Whether this segment holds a redirection: with no words it still
    /// opens or truncates a file, so it is kept, and no allow matches it.
    redirected: bool,
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
        self.redirected = true;
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
        if words.is_empty() && !self.redirected {
            if self.joined || joined {
                return None;
            }
        } else {
            self.segments.push(words);
        }
        self.joined = joined;
        self.redirected = false;
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
    /// One of the human's allow rules lets it run without a card, and
    /// why.
    Allow(String),
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
/// is unread; then an allow, which only the human's rules hold.
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
            return Verdict::Ask(format!("{from} could not be read: {why}"));
        }
    }
    if let Some(ones) = allowed(rules, tool, parsed.as_ref()) {
        // At most `NAMED` named, so the reason stays short however
        // many segments a command has.
        let mut named: Vec<String> = ones
            .iter()
            .take(NAMED)
            .map(|one| format!("`{}` of {}", one.rule.text(), one.from))
            .collect();
        if ones.len() > NAMED {
            named.push(format!("{} more", ones.len() - NAMED));
        }
        return Verdict::Allow(match named.as_slice() {
            [one] => format!("the rule {one} allows it"),
            _ => format!("the rules {} allow it", named.join(", ")),
        });
    }
    Verdict::Table
}

/// The most allow rules an allow's reason names.
const NAMED: usize = 3;

/// The allow rules that let a call to `tool` run, `command` its shell
/// command as `split` read it: one for the whole tool, or for a command
/// the matcher can see into, one for each segment, every segment
/// covered, so `cargo test && cargo fmt` runs on `allow shell cargo
/// test` and `allow shell cargo fmt` together.
fn allowed<'a>(
    rules: &'a [Sourced],
    tool: &str,
    command: Option<&Parsed>,
) -> Option<Vec<&'a Sourced>> {
    let allows = rules
        .iter()
        .filter(|one| one.rule.effect == Effect::Allow && one.rule.tool == tool);
    if let Some(one) = allows
        .clone()
        .find(|one| one.rule.prefix.is_empty() && matches(&one.rule, tool, command))
    {
        return Some(vec![one]);
    }
    let parsed = command.filter(|parsed| !parsed.opaque && !parsed.segments.is_empty())?;
    let mut used: Vec<&Sourced> = Vec::new();
    for segment in &parsed.segments {
        let one = allows.clone().find(|one| {
            !one.rule.prefix.is_empty() && starts(&one.rule.prefix, segment, Effect::Allow)
        })?;
        if !used.iter().any(|u| std::ptr::eq(*u, one)) {
            used.push(one);
        }
    }
    Some(used)
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
        // Redirections alone after an operator: a segment with no words.
        assert_eq!(
            split("a | > f").segments,
            vec![vec![Some("a".into())], Vec::new()]
        );
        assert!(!split("a | > f").opaque);
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
        // A segment of redirections alone still writes a file, and no
        // allow with words matches it.
        assert_eq!(
            split("cargo test; > notes.txt").segments,
            vec![vec![Some("cargo".into()), Some("test".into())], Vec::new()]
        );
        assert!(!matches(
            &allow,
            "shell",
            Some(&parsed("cargo test; > notes.txt"))
        ));
        assert!(!matches(&allow, "shell", Some(&parsed("> notes.txt"))));
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
            "the rules of the repository at /w/x".to_string(),
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
        // Allows compose: each segment covered by one of them.
        let both = [
            Sourced {
                rule: rule("allow shell cargo test"),
                from: "A".into(),
            },
            Sourced {
                rule: rule("allow shell cargo fmt"),
                from: "B".into(),
            },
        ];
        assert_eq!(
            judge(
                &both,
                &[],
                "shell",
                Some("cargo test -p x && cargo fmt; cargo test"),
                true
            ),
            Verdict::Allow(
                "the rules `allow shell cargo test` of A, `allow shell cargo fmt` of B allow it"
                    .into()
            )
        );
        assert_eq!(
            judge(&both, &[], "shell", Some("cargo test && cargo build"), true),
            Verdict::Table
        );
        let many: Vec<Sourced> = (0..5)
            .map(|n| Sourced {
                rule: rule(&format!("allow shell p{n}")),
                from: "A".into(),
            })
            .collect();
        assert_eq!(
            judge(&many, &[], "shell", Some("p0; p1; p2; p3; p4"), true),
            Verdict::Allow(
                "the rules `allow shell p0` of A, `allow shell p1` of A, `allow shell p2` of A, 2 more allow it"
                    .into()
            )
        );
        // The human's allow, after every deny and ask, and never for a
        // command the matcher cannot see into.
        let yours = |line: &str| Sourced {
            rule: rule(line),
            from: "your rules for this workspace".into(),
        };
        assert_eq!(
            judge(
                &[yours("allow shell cargo test")],
                &[],
                "shell",
                Some("cargo test -p x"),
                true
            ),
            Verdict::Allow(
                "the rule `allow shell cargo test` of your rules for this workspace allows it"
                    .into()
            )
        );
        assert!(matches!(
            judge(
                &[yours("allow shell cargo"), from("ask shell cargo publish")],
                &[],
                "shell",
                Some("cargo publish"),
                true
            ),
            Verdict::Ask(_)
        ));
        assert!(matches!(
            judge(
                &[
                    yours("allow shell cargo"),
                    yours("deny shell cargo publish")
                ],
                &[],
                "shell",
                Some("cargo publish"),
                true
            ),
            Verdict::Deny(_)
        ));
        assert_eq!(
            judge(
                &[yours("allow shell cargo")],
                &[],
                "shell",
                Some("cargo $(rm x)"),
                true
            ),
            Verdict::Table
        );
        assert!(matches!(
            judge(
                &[yours("allow write_file")],
                &unread,
                "write_file",
                None,
                true
            ),
            Verdict::Ask(_)
        ));
        assert_eq!(
            judge(&[from("deny read_file")], &[], "read_file", None, false),
            Verdict::Deny("the rule `deny read_file` of the repository at /w/td denies it".into())
        );
    }
    /// What a card offers to remember: the tool alone, or each segment's
    /// program with a subcommanded one's subcommand; no allow for what
    /// the matcher cannot see into or a rule could not name narrowly.
    #[test]
    fn a_card_offers_to_remember_each_programs_rule() {
        let offer = |tool: &str, command: Option<&str>| proposals(tool, command);
        let always = |allow: bool, bodies: &[&str]| {
            Some(Always {
                allow,
                bodies: bodies.iter().map(|b| b.to_string()).collect(),
            })
        };
        assert_eq!(offer("write_file", None), always(true, &["write_file"]));
        assert_eq!(
            offer(
                "shell",
                Some("cargo test -p x && cargo fmt --check | tee log")
            ),
            always(true, &["shell cargo test", "shell cargo fmt", "shell tee"])
        );
        assert_eq!(
            offer("shell", Some("rm -f a; rm b")),
            always(true, &["shell rm"])
        );
        assert_eq!(
            offer("shell", Some("./build.sh")),
            always(true, &["shell ./build.sh"])
        );
        // A subcommanded program with none named, or an option first.
        assert_eq!(offer("shell", Some("make")), always(false, &["shell make"]));
        assert_eq!(
            offer("shell", Some("git -C sub push")),
            always(false, &["shell git"])
        );
        assert_eq!(
            offer("shell", Some("git Push")),
            always(false, &["shell git"])
        );
        // Opaque: only the denies of the programs it names, and nothing
        // for one the matcher cannot split.
        assert_eq!(
            offer("shell", Some("sh -c 'rm x'")),
            always(false, &["shell sh"])
        );
        assert_eq!(offer("shell", Some("echo $(rm x)")), None);
        assert_eq!(
            offer("shell", Some("ls; > notes.txt")),
            always(false, &["shell ls"])
        );
        assert_eq!(offer("shell", Some("\"$x\"")), None);
        // A quoted word with a space would read back as two.
        assert_eq!(
            offer("shell", Some("'git push' x; ls")),
            always(false, &["shell ls"])
        );
        assert_eq!(offer("shell", Some("> f")), None);
        assert_eq!(offer("push", None), None);
        assert_eq!(offer("shell", None), None);
        // A quoted program with its option: no allow past program and
        // subcommand.
        assert_eq!(offer("shell", Some("'git --no-pager' status")), None);
        let many: Vec<String> = (0..=MAX_BODIES).map(|n| format!("p{n}")).collect();
        assert_eq!(offer("shell", Some(&many.join("; "))), None);
        assert_eq!(broad("shell cargo test"), Some("cargo"));
        assert_eq!(broad("shell /usr/bin/python3"), Some("/usr/bin/python3"));
        assert_eq!(broad("shell rm"), None);
        assert_eq!(broad("write_file"), None);
    }

    #[test]
    fn what_a_card_offers_crosses_only_as_rules_write_it() {
        let bodies = |b: &[&str]| b.iter().map(|b| b.to_string()).collect::<Vec<_>>();
        assert!(Always::checked(true, bodies(&["shell cargo test", "glob"])).is_ok());
        for wrong in [
            bodies(&[]),
            bodies(&["shell  cargo"]),
            bodies(&["shell cargo test", "shell cargo test"]),
            bodies(&["allow shell"]),
            bodies(&["shell $x"]),
            bodies(&["write_file x"]),
            (0..=MAX_BODIES).map(|n| format!("shell p{n}")).collect(),
        ] {
            assert!(Always::checked(true, wrong.clone()).is_err(), "{wrong:?}");
        }
    }

    /// A deleted conversation's workspace's sections go, every other line
    /// staying; a file with none is left as it is.
    #[test]
    fn a_deleted_conversations_rules_go_with_it() {
        let text = "# mine\n[workspace td-1-ab]\nallow shell cargo test\n\n[everywhere]\ndeny shell rm\n[workspace td-1-ab]\n# again\ndeny glob\n[workspace td-2-cd]\nask glob\n";
        let id = "a".repeat(32);
        assert_eq!(
            forget(text, Some("workspace td-1-ab"), &id)
                .unwrap()
                .unwrap(),
            "# mine\n[everywhere]\ndeny shell rm\n[workspace td-2-cd]\nask glob\n"
        );
        assert_eq!(forget(text, Some("workspace td-3-ef"), &id).unwrap(), None);
        assert_eq!(forget(text, None, &id).unwrap(), None);
        assert!(forget("[x]\n", Some("workspace td-1-ab"), &id).is_err());
        // Its crossings, either way, and no other.
        let (a, b, c) = ("a".repeat(32), "b".repeat(32), "c".repeat(32));
        let text =
            format!("[crossings]\nallow read {a} {b}\ndeny message {b} {a}\nallow read {b} {c}\n");
        assert_eq!(
            forget(&text, None, &a).unwrap().unwrap(),
            format!("[crossings]\nallow read {b} {c}\n")
        );
    }

    /// A workspace's mode: its last `mode` line, under its own header
    /// only; setting it replaces each line there and keeps the rest.
    #[test]
    fn a_workspaces_mode_is_its_last_mode_line() {
        use crate::config::Mode;
        let text = "[workspace td-1-ab]\nmode auto\ndeny glob\n[workspace td-2-cd]\nmode ask\n[workspace td-1-ab]\nmode  ask\n";
        let policy = parse_policy(text).unwrap();
        assert_eq!(policy.mode("workspace td-1-ab"), Some(Mode::Ask));
        assert_eq!(policy.mode("workspace td-2-cd"), Some(Mode::Ask));
        assert_eq!(policy.mode("workspace td-3-ef"), None);
        assert_eq!(policy.rules.len(), 1);
        for (wrong, why) in [
            (
                "[everywhere]\nmode auto\n",
                "line 2: a mode is for one workspace, not every one",
            ),
            (
                "[workspace w]\nmode on\n",
                "line 2: a mode is `mode ask` or `mode auto`",
            ),
            (
                "[workspace w]\nmode auto ask\n",
                "line 2: a mode is `mode ask` or `mode auto`",
            ),
        ] {
            assert_eq!(parse_policy(wrong).unwrap_err(), why, "{wrong}");
        }
        assert!(parse_policy("[crossings]\nmode auto\n").is_err());
        let set = set_mode(text, "workspace td-1-ab", Mode::Auto).unwrap();
        assert_eq!(
            set,
            "[workspace td-1-ab]\ndeny glob\n[workspace td-2-cd]\nmode ask\n[workspace td-1-ab]\nmode auto\n"
        );
        assert_eq!(
            parse_policy(&set).unwrap().mode("workspace td-1-ab"),
            Some(Mode::Auto)
        );
        assert_eq!(
            set_mode("", "workspace td-3-ef", Mode::Ask).unwrap(),
            "[workspace td-3-ef]\nmode ask\n"
        );
        assert!(set_mode("", "everywhere", Mode::Auto).is_err());
        assert!(set_mode("[x]\n", "workspace td-3-ef", Mode::Auto).is_err());
        // A deleted workspace's mode goes with its section.
        let gone = forget(&set, Some("workspace td-1-ab"), &"a".repeat(32))
            .unwrap()
            .unwrap();
        assert_eq!(parse_policy(&gone).unwrap().mode("workspace td-1-ab"), None);
    }

    /// The human's crossings: read in their section, each a pair and a
    /// direction; a deny first, then an allow, each for its own way.
    #[test]
    fn a_crossing_is_answered_for_one_pair_one_way() {
        let (a, b) = ("a".repeat(32), "b".repeat(32));
        let text = format!("[crossings]\nallow read {a} {b}\n# no\ndeny message {a} {b}\n");
        let policy = parse_policy(&text).unwrap();
        assert!(policy.rules.is_empty());
        assert_eq!(policy.crossings.len(), 2);
        let crossings = &policy.crossings;
        assert_eq!(
            cross(crossings, Crossed::Read, &a, &b),
            Verdict::Allow(format!("your rule `allow read {a} {b}` allows it"))
        );
        assert_eq!(cross(crossings, Crossed::Read, &b, &a), Verdict::Table);
        let c = "c".repeat(32);
        assert_eq!(cross(crossings, Crossed::Read, &c, &b), Verdict::Table);
        assert_eq!(
            cross(crossings, Crossed::Message, &a, &b),
            Verdict::Deny(format!("your rule `deny message {a} {b}` denies it"))
        );
        let both = parse_policy(&format!("{text}allow message {a} {b}\n")).unwrap();
        assert!(matches!(
            cross(&both.crossings, Crossed::Message, &a, &b),
            Verdict::Deny(_)
        ));
        for wrong in [
            format!("[crossings]\nallow read {a}\n"),
            format!("[crossings]\nask read {a} {b}\n"),
            format!("[crossings]\nallow write {a} {b}\n"),
            format!("[crossings]\nallow read {a} {a}\n"),
            format!("[crossings]\nallow read {a} x\n"),
            "[crossings]\nallow shell ls\n".to_string(),
            format!("[everywhere]\nallow read {a} {b}\n"),
        ] {
            assert!(parse_policy(&wrong).is_err(), "{wrong}");
        }
        let one = Crossing::parse(&format!("allow read {a} {b}")).unwrap();
        let added = add_crossing("[everywhere]\ndeny glob\n", &one).unwrap();
        assert_eq!(
            added,
            format!("[everywhere]\ndeny glob\n\n[crossings]\nallow read {a} {b}\n")
        );
        assert_eq!(add_crossing(&added, &one).unwrap(), added);
    }

    /// An "always" answer adds its rules without disturbing the human's
    /// file: below the last header when it is the scope's, else under a
    /// new one; a rule already held is not added again.
    #[test]
    fn an_always_answer_adds_its_rules_and_keeps_every_line() {
        let here = Scope::Workspace("workspace td-1-ab".into());
        let cargo = vec!["shell cargo test".to_string()];
        let first = add("", &here, Effect::Allow, &cargo).unwrap();
        assert_eq!(first, "[workspace td-1-ab]\nallow shell cargo test\n");
        let fmt = vec![
            "shell cargo fmt".to_string(),
            "shell cargo test".to_string(),
        ];
        assert_eq!(
            add(&first, &here, Effect::Allow, &fmt).unwrap(),
            "[workspace td-1-ab]\nallow shell cargo test\nallow shell cargo fmt\n"
        );
        let mine = "# mine\n[workspace td-1-ab]\nallow shell cargo test # kept?\n";
        assert!(add(mine, &here, Effect::Allow, &cargo).is_err());
        let mine = "# mine\n[workspace td-1-ab]\n  allow shell cargo test";
        assert_eq!(
            add(mine, &here, Effect::Allow, &cargo).unwrap(),
            format!("{mine}\n")
        );
        let both = add(mine, &Scope::Everywhere, Effect::Deny, &["shell rm".into()]).unwrap();
        assert_eq!(both, format!("{mine}\n\n[everywhere]\ndeny shell rm\n"));
        assert_eq!(
            add(&both, &here, Effect::Deny, &["shell rm".into()]).unwrap(),
            format!("{both}\n[workspace td-1-ab]\ndeny shell rm\n")
        );
        // Never an allow for every workspace, nor into a file refused.
        assert!(add("", &Scope::Everywhere, Effect::Allow, &cargo).is_err());
        assert!(add("[x]\n", &here, Effect::Deny, &cargo).is_err());
    }

    #[test]
    fn the_humans_rules_file_reads_back_as_written() {
        let text = "# mine\n[workspace td-1-ab]\nallow shell cargo test\n deny write_file\n\n[everywhere]\ndeny shell rm\n[directory /home/u/my%20notes]\nask glob\n";
        let rules = parse_human(text).unwrap();
        let scopes: Vec<&Scope> = rules.iter().map(|one| &one.scope).collect();
        let td = Scope::Workspace("workspace td-1-ab".into());
        assert_eq!(
            scopes,
            [
                &td,
                &td,
                &Scope::Everywhere,
                &Scope::Workspace("directory /home/u/my%20notes".into())
            ]
        );
        assert_eq!(parse_human(&human_text(&rules)).unwrap(), rules);
        for (text, why) in [
            (
                "allow glob\n",
                "line 1: a rule before any [workspace], [everywhere] or [crossings]",
            ),
            (
                "[everywhere]\nallow glob\n",
                "line 2: an allow is for one workspace, not every one",
            ),
            ("[]\n", "line 1: a header that is neither [everywhere] nor a workspace's: [workspace <name>], [conversation <id>] or [directory <path>]"),
            ("[ everywhere]\ndeny glob\n", "line 1: a header that is neither [everywhere] nor a workspace's: [workspace <name>], [conversation <id>] or [directory <path>]"),
            ("[Everywhere]\ndeny glob\n", "line 1: a header that is neither [everywhere] nor a workspace's: [workspace <name>], [conversation <id>] or [directory <path>]"),
            ("[workspace td-1-ab ]\ndeny glob\n", "line 1: a header that is neither [everywhere] nor a workspace's: [workspace <name>], [conversation <id>] or [directory <path>]"),
            ("[conversation 12]\ndeny glob\n", "line 1: a header that is neither [everywhere] nor a workspace's: [workspace <name>], [conversation <id>] or [directory <path>]"),
            ("[directory /home/u/my notes]\ndeny glob\n", "line 1: a header that is neither [everywhere] nor a workspace's: [workspace <name>], [conversation <id>] or [directory <path>]"),
            ("[directory notes]\ndeny glob\n", "line 1: a header that is neither [everywhere] nor a workspace's: [workspace <name>], [conversation <id>] or [directory <path>]"),
            ("# a \u{1b}[1m comment\n", "line 1: a control character"),
            (
                "[workspace w]\nallow shell 'x'\n",
                "line 2: word 3 holds what the shell would read, not pass",
            ),
        ] {
            assert_eq!(parse_human(text).unwrap_err(), why, "{text:?}");
        }
        assert!(parse_human(&"#".repeat(MAX_HUMAN_FILE + 1)).is_err());
    }
}
