//! What a model request and its reply carried on the wire, kept beside a
//! conversation's log for the window's Debug view (DESIGN.md §6): the
//! request's line and headers, and the reply's status, headers and body
//! as they came, the key never among them.

use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

/// The directory of a conversation's records, beside its log.
pub const DIR: &str = "http";
/// The most bytes of a reply body's text a record keeps: the first.
pub const MAX_BODY: usize = 1024 * 1024;
/// The bytes past `MAX_BODY` held until the record is scrubbed, so a key
/// that begins before the cut is whole when it is looked for: more than
/// any form of one (`key::MAX_KEY` bytes, at most twice that escaped).
const LOOKAHEAD: usize = 4096;
/// The most bytes of the request's line or of either side's headers a
/// record keeps.
const MAX_HEAD: usize = 64 * 1024;
/// The most bytes a conversation's records take: past it the oldest go.
pub const MAX_TOTAL: u64 = 16 * 1024 * 1024;
/// The most bytes of how an exchange ended a record keeps.
const MAX_END: usize = 16 * 1024;
/// The most bytes read back of one record: past all a record can hold,
/// a scrub's replacements included.
const MAX_RECORD: u64 = 4 * (MAX_BODY + 3 * MAX_HEAD + MAX_END) as u64;
/// What stands for a credential: a header's value, the key, a URL's user.
pub const REDACTED: &str = "[redacted]";
/// Headers whose values are credentials, the request's or the reply's.
const SECRET_HEADERS: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "cookie",
    "set-cookie",
];
/// A record's separator line between the request and the reply.
pub const REPLY: &str = "--- reply ---";

/// One request's record, built as its reply arrives.
#[derive(Debug)]
pub struct Record {
    method: String,
    url: String,
    headers: Vec<(String, String)>,
    status: Option<u16>,
    reply_headers: Vec<(String, String)>,
    body: Vec<u8>,
    total: u64,
    end: Option<String>,
    /// A user and password the URL carried, and the password alone: looked
    /// for as the key is, should a failure's text give them back.
    user: Vec<String>,
}

/// `headers` with every credential's value replaced.
fn redacted<N: AsRef<str>>(headers: &[(N, String)]) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(name, value)| {
            let name = name.as_ref();
            let secret = SECRET_HEADERS
                .iter()
                .any(|one| name.eq_ignore_ascii_case(one));
            let value = if secret {
                REDACTED.to_string()
            } else {
                value.clone()
            };
            (name.to_string(), value)
        })
        .collect()
}

/// `url` with any user and password in its authority replaced, and
/// what was replaced: the user and password, and the password alone.
fn without_user(url: &str) -> (String, Vec<String>) {
    let Some((scheme, rest)) = url.split_once("://") else {
        return (url.to_string(), Vec::new());
    };
    let end = rest.find(['/', '\\', '?', '#']).unwrap_or(rest.len());
    let (authority, path) = rest.split_at(end);
    match authority.rsplit_once('@') {
        Some((user, host)) => {
            let mut secrets = vec![user.to_string()];
            if let Some((_, password)) = user.split_once(':') {
                secrets.push(password.to_string());
            }
            secrets.retain(|secret| !secret.is_empty());
            (format!("{scheme}://{REDACTED}@{host}{path}"), secrets)
        }
        None => (url.to_string(), Vec::new()),
    }
}

/// `text` less any ending that begins one of `forms` without finishing
/// it, the longest first, until none does: what a cut through a key
/// leaves of it.
fn without_partial(mut text: String, forms: &[String]) -> String {
    loop {
        let partial = forms
            .iter()
            .filter_map(|form| {
                (1..form.len())
                    .rev()
                    .find(|n| form.get(..*n).is_some_and(|start| text.ends_with(start)))
            })
            .max();
        match partial {
            Some(n) => text.truncate(text.len() - n),
            None => return text,
        }
    }
}

/// `text` cut to at most `bound` bytes at a character, and whether it was.
fn cut(text: &str, bound: usize) -> (&str, bool) {
    if text.len() <= bound {
        return (text, false);
    }
    let mut end = bound;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (text.get(..end).unwrap_or_default(), true)
}

/// Header lines within `MAX_HEAD`, saying when some are left out.
fn header_lines(headers: &[(String, String)]) -> String {
    let mut out = String::new();
    for (name, value) in headers {
        let line = format!("{name}: {value}\n");
        if out.len() + line.len() > MAX_HEAD {
            out.push_str("[further headers are left out]\n");
            break;
        }
        out.push_str(&line);
    }
    out
}

/// Every form `key` could take in a record: as written, as a JSON string
/// escapes it, and that with its solidi escaped too; the same forms the
/// diagnostics export looks for (DESIGN.md §4).
pub fn key_forms(key: &str) -> Vec<String> {
    let quoted = td_json::Json::Str(key.to_string()).to_string();
    let escaped = quoted
        .strip_prefix('"')
        .and_then(|q| q.strip_suffix('"'))
        .unwrap_or(key)
        .to_string();
    let solidi = escaped.replace('/', "\\/");
    let mut forms: Vec<String> = Vec::new();
    for form in [key.to_string(), escaped, solidi] {
        if !form.is_empty() && !forms.contains(&form) {
            forms.push(form);
        }
    }
    forms
}

impl Record {
    /// A record of a `method` request to `url` sent with `headers`, whose
    /// credentials it never holds.
    pub fn new<N: AsRef<str>>(method: &str, url: &str, headers: &[(N, String)]) -> Self {
        let (url, user) = without_user(url);
        Self {
            method: method.to_string(),
            url,
            user,
            headers: redacted(headers),
            status: None,
            reply_headers: Vec::new(),
            body: Vec::new(),
            total: 0,
            end: None,
        }
    }

    /// The reply's head.
    pub fn head(&mut self, status: u16, headers: &[(String, String)]) {
        self.status = Some(status);
        self.reply_headers = redacted(headers);
    }

    /// A frame of the reply's body: held while there is room, counted
    /// always.
    pub fn chunk(&mut self, bytes: &[u8]) {
        self.total = self.total.saturating_add(bytes.len() as u64);
        let room = (MAX_BODY + LOOKAHEAD).saturating_sub(self.body.len());
        self.body
            .extend_from_slice(bytes.get(..room.min(bytes.len())).unwrap_or_default());
    }

    /// How the exchange ended, the first said kept.
    pub fn end(&mut self, how: String) {
        if self.end.is_none() {
            self.end = Some(how);
        }
    }

    /// The record as text, every form of each of `keys` (`key_forms`)
    /// replaced wherever it is, should one have come back in a reply: the
    /// body is scrubbed whole, past its bound by `LOOKAHEAD`, before it
    /// is cut, so no part of a key the cut would split is kept.
    pub fn text(&self, keys: &[String]) -> String {
        let mut keys = keys.to_vec();
        keys.extend(self.user.iter().cloned());
        let keys = keys.as_slice();
        let line = format!("{} {}", self.method, self.url);
        let mut out = cut(&line, MAX_HEAD).0.to_string();
        out.push('\n');
        out.push_str(&header_lines(&self.headers));
        out.push_str(REPLY);
        out.push('\n');
        match self.status {
            Some(status) => out.push_str(&format!("status {status}\n")),
            None => out.push_str("no reply head came\n"),
        }
        out.push_str(&header_lines(&self.reply_headers));
        out.push('\n');
        let mut body = String::from_utf8_lossy(&self.body).into_owned();
        // Held short of the reply, the body may end inside a key, which
        // no scrub would match and a replacement before it could move
        // under the cut: that start goes first.
        if self.total > self.body.len() as u64 {
            body = without_partial(body, keys);
        }
        let body = scrubbed(body, keys);
        let (body, short) = cut(&body, MAX_BODY);
        out.push_str(body);
        if !body.is_empty() && !body.ends_with('\n') {
            out.push('\n');
        }
        if short || self.total > self.body.len() as u64 {
            out.push_str(&format!(
                "[{} bytes of the body's text are kept; it had {} bytes]\n",
                body.len(),
                self.total
            ));
        }
        let end = self.end.as_deref().unwrap_or("its end was not seen");
        out.push_str(&format!("[{}]\n", cut(end, MAX_END).0));
        scrubbed(out, keys)
    }
}

/// `text` with every non-empty key in `keys` replaced by `REDACTED`.
fn scrubbed(mut text: String, keys: &[String]) -> String {
    for key in keys {
        if !key.is_empty() && text.contains(key.as_str()) {
            text = text.replace(key.as_str(), REDACTED);
        }
    }
    text
}

/// Where request `request`'s record is in conversation directory `dir`.
pub fn path(dir: &Path, request: u64) -> PathBuf {
    dir.join(DIR).join(request.to_string())
}

/// Writes `text` as request `request`'s record in conversation directory
/// `dir`, private and whole or not at all, never through a link, then
/// removes the oldest records past `MAX_TOTAL`.
pub fn write(dir: &Path, request: u64, text: &str) -> Result<(), String> {
    let records = dir.join(DIR);
    let made = DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&records)
        .and_then(|()| fs::symlink_metadata(&records));
    match made {
        Ok(m) if m.is_dir() => {}
        Ok(_) => return Err(format!("{}: not a directory", records.display())),
        Err(e) => return Err(format!("{}: {e}", records.display())),
    }
    let at = path(dir, request);
    let part = records.join(format!("{request}.part"));
    let written = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(crate::store::O_NOFOLLOW)
        .open(&part)
        .and_then(|mut file| file.write_all(text.as_bytes()))
        .and_then(|()| fs::rename(&part, &at));
    if let Err(e) = written {
        let _ = fs::remove_file(&part);
        return Err(format!("{}: {e}", at.display()));
    }
    prune(&records, MAX_TOTAL);
    Ok(())
}

/// Removes the oldest records, by request, until those left take at most
/// `budget` bytes, and any temporary a write that did not finish left:
/// the conversation's one process writes them, one at a time.
fn prune(records: &Path, budget: u64) {
    let Ok(entries) = fs::read_dir(records) else {
        return;
    };
    let mut kept: Vec<(u64, u64, PathBuf)> = Vec::new();
    for entry in entries.filter_map(Result::ok) {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(metadata) = entry.metadata().ok().filter(|m| m.is_file()) else {
            continue;
        };
        if let Some(request) = name.strip_suffix(".part") {
            if request.parse::<u64>().is_ok() {
                let _ = fs::remove_file(entry.path());
            }
            continue;
        }
        if let Ok(request) = name.parse::<u64>() {
            kept.push((request, metadata.len(), entry.path()));
        }
    }
    kept.sort_unstable_by_key(|(request, _, _)| *request);
    let mut total: u64 = kept.iter().map(|(_, bytes, _)| *bytes).sum();
    for (_, bytes, path) in kept {
        if total <= budget {
            break;
        }
        if fs::remove_file(&path).is_ok() {
            total = total.saturating_sub(bytes);
        }
    }
}

/// Request `request`'s record in conversation directory `dir`, when one
/// is kept: read within what a record can hold, never refused for its
/// bytes.
pub fn read(dir: &Path, request: u64) -> Option<String> {
    let file = fs::File::open(path(dir, request)).ok()?;
    let mut bytes = Vec::new();
    file.take(MAX_RECORD).read_to_end(&mut bytes).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// The most bytes of a request's body or a reply the Debug view shows,
/// a list entry's bound less room for saying so.
const MAX_SHOWN: usize = td_ui::messages::MAX_TEXT_BYTES - 1024;

/// The most bytes of a record's ending lines kept past a cut.
const MAX_ENDING: usize = 2 * MAX_END + 1024;

/// `text` as the Debug view shows it: every form of `keys` replaced,
/// each line's control and bidirectional characters shown escaped as a
/// process's output's are, then held to `MAX_SHOWN`, cut at a character
/// and said so. Its last lines that begin `[`, up to `ending` of them,
/// say how a record ended, and are kept past the cut.
fn entry(text: &str, keys: &[String], ending: usize) -> String {
    let text = scrubbed(text.to_string(), keys);
    let text: Vec<String> = text.split('\n').map(crate::tools::visible).collect();
    let text = text.join("\n");
    if text.len() <= MAX_SHOWN {
        return text;
    }
    let mut tail = text.len();
    for _ in 0..ending {
        let before = text.get(..tail.saturating_sub(1)).unwrap_or_default();
        match before.rfind('\n') {
            Some(at)
                if text.get(at + 1..).is_some_and(|line| line.starts_with('['))
                    && text.len() - (at + 1) <= MAX_ENDING =>
            {
                tail = at + 1
            }
            _ => break,
        }
    }
    let (head, tail) = text.split_at_checked(tail).unwrap_or((&text, ""));
    let mut end = MAX_SHOWN.saturating_sub(tail.len()).min(head.len());
    while !head.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n[the first {end} of its {} bytes as shown here are shown; the conversation's diagnostics export holds the record whole]\n{tail}",
        head.get(..end).unwrap_or_default(),
        head.len()
    )
}

/// What the window's Debug view shows of request `request` of
/// conversation `id`: the request's line and headers, its body as the log
/// rebuilds it, and the reply as its record kept it, or why there is no
/// record.
pub fn view(
    state: &crate::store::StateDir,
    id: &crate::store::Id,
    request: u64,
    keys: &[String],
) -> Result<Vec<(String, String)>, String> {
    let events = crate::store::read_log(state, id)?;
    let index = events
        .iter()
        .position(|event| event.seq == request)
        .ok_or_else(|| format!("request {request} is not in the log"))?;
    let prefix = crate::store::read_prefix(state, id)?;
    let body = crate::client::body(&events, index, &prefix)?;
    let body = (
        format!("request body: {} bytes, rebuilt from the log", body.len()),
        entry(&body, keys, 0),
    );
    let split = format!("\n{REPLY}\n");
    let record = read(&state.conversation(id), request);
    Ok(match record.as_deref().and_then(|text| text.split_once(&split)) {
        Some((sent, reply)) => vec![
            ("request".into(), entry(sent, keys, 0)),
            body,
            ("reply".into(), entry(reply, keys, 2)),
        ],
        None => vec![
            body,
            (
                "reply".into(),
                format!(
                    "no record of it is kept: a reply is recorded once it ends, a conversation's records keep its most recent {} MiB, and a fork takes its source's log but none of its records",
                    MAX_TOTAL / (1024 * 1024)
                ),
            ),
        ],
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
    use super::*;

    const KEY: &str = "sk-or-v1-0123456789abcdef";

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("td-agent-wire-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn headers(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(n, v)| ((*n).to_string(), (*v).to_string()))
            .collect()
    }

    /// The key is never written: its header's value is replaced, a
    /// cookie's too, the key anywhere else in the record as well, and a
    /// URL's user and password.
    #[test]
    fn a_record_never_holds_the_key() {
        let mut record = Record::new(
            "POST",
            "https://me:secret@openrouter.ai/api/v1/chat/completions",
            &headers(&[
                ("authorization", &format!("Bearer {KEY}")),
                ("content-type", "application/json"),
            ]),
        );
        record.head(
            401,
            &headers(&[
                ("Set-Cookie", "session=1"),
                ("content-type", "application/json"),
            ]),
        );
        record.chunk(format!("{{\"error\":\"bad key {KEY}\"}}").as_bytes());
        record.end("the reply was whole".into());
        let text = record.text(&key_forms(KEY));
        assert!(!text.contains(KEY), "{text}");
        assert!(!text.contains("secret"), "{text}");
        assert!(
            text.starts_with("POST https://[redacted]@openrouter.ai/api/v1/chat/completions\n"),
            "{text}"
        );
        assert!(text.contains("authorization: [redacted]\n"), "{text}");
        assert!(text.contains("Set-Cookie: [redacted]\n"), "{text}");
        assert!(text.contains("content-type: application/json\n"), "{text}");
        assert!(text.contains("status 401\n"), "{text}");
        assert!(text.contains("bad key [redacted]"), "{text}");
        assert!(text.ends_with("[the reply was whole]\n"), "{text}");
    }

    /// A key a reply echoes across the body's bound leaves nothing of
    /// itself: the body is scrubbed past the bound before it is cut.
    #[test]
    fn a_key_across_the_cut_leaves_nothing() {
        for before in [1, 9, KEY.len() - 1] {
            let mut record = Record::new::<&str>("POST", "u", &[]);
            record.head(200, &[]);
            record.chunk(&vec![b'a'; MAX_BODY - before]);
            record.chunk(KEY.as_bytes());
            record.chunk(&[b'z'; 10_000]);
            let text = record.text(&key_forms(KEY));
            assert!(!text.contains("sk-or"), "{before}");
            // The body kept: the a's, then as much of the replacement and
            // what follows it as the bound leaves room for.
            let room = before.min(REDACTED.len());
            assert!(
                text.contains(&format!(
                    "aaaa{}{}\n[",
                    &REDACTED[..room],
                    "z".repeat(before - room)
                )),
                "{before}"
            );
            assert!(text.contains(&format!(
                "[{MAX_BODY} bytes of the body's text are kept; it had {} bytes]",
                MAX_BODY - before + KEY.len() + 10_000
            )));
        }
    }

    /// What the Debug view shows holds no key, escapes what a terminal
    /// or a bidirectional run would act on, and keeps how a record ended
    /// past its cut.
    #[test]
    fn an_entry_hides_the_key_escapes_and_keeps_its_ending() {
        let forms = key_forms(KEY);
        let shown = entry(&format!("a {KEY}\u{202e}b\u{1b}[2J"), &forms, 0);
        assert!(!shown.contains(KEY) && shown.contains(REDACTED), "{shown}");
        assert!(
            !shown.contains('\u{202e}') && !shown.contains('\u{1b}'),
            "{shown}"
        );
        let ending = "[1048576 bytes of the body's text are kept; it had 2000000 bytes]\n[td-agent read a whole reply]\n";
        let long = format!("status 200\n{}\n{ending}", "x".repeat(MAX_SHOWN));
        let shown = entry(&long, &forms, 2);
        assert!(shown.len() <= MAX_SHOWN + 512, "{}", shown.len());
        assert!(shown.ends_with(ending), "{}", &shown[shown.len() - 300..]);
        assert!(shown.contains("bytes as shown here are shown;"));
        // Without ending lines, a long last line is cut as the rest is.
        let shown = entry(&"y".repeat(2 * MAX_SHOWN), &forms, 2);
        assert!(shown.len() <= MAX_SHOWN + 512, "{}", shown.len());
    }

    /// A key cut short at the held bound leaves nothing either, however
    /// far the keys scrubbed before it move it back under the cut.
    #[test]
    fn a_key_cut_at_the_held_bound_leaves_nothing() {
        let mut record = Record::new::<&str>("POST", "u", &[]);
        // Each echo scrubbed saves 15 bytes: 400 of them move what follows
        // back past the lookahead, under the cut.
        let echoes = KEY.repeat(400);
        let filler = MAX_BODY + LOOKAHEAD - echoes.len() - 20;
        record.chunk(echoes.as_bytes());
        record.chunk(&vec![b'a'; filler]);
        record.chunk(KEY.as_bytes());
        let text = record.text(&key_forms(KEY));
        assert!(!text.contains("sk-or-v1-0123"), "a key's start is kept");
        assert!(
            text.contains(&format!("{}\n[", "a".repeat(20))),
            "the a's end it"
        );
    }

    /// A URL's password a failure's text gives back is replaced too.
    #[test]
    fn a_urls_password_is_never_kept() {
        let mut record = Record::new::<&str>("POST", "https://me:hunter22@h/v1", &[]);
        record.end("the exchange failed: https://me:hunter22@h/v1: refused".into());
        let text = record.text(&[]);
        assert!(!text.contains("hunter22"), "{text}");
        assert!(text.contains("POST https://[redacted]@h/v1\n"), "{text}");
    }

    /// A key a JSON body escapes, or a failure's debugging text, is found
    /// in each form it can take.
    #[test]
    fn an_escaped_key_is_found_too() {
        let key = "k\"e\\y/z";
        let forms = key_forms(key);
        assert_eq!(forms, [key, "k\\\"e\\\\y/z", "k\\\"e\\\\y\\/z"]);
        let mut record = Record::new::<&str>("POST", "u", &[]);
        record.chunk(br#"{"echo":"k\"e\\y/z","again":"k\"e\\y\/z"}"#);
        record.end(format!("td-agent read a failure: {:?}", key.to_string()));
        let text = record.text(&forms);
        assert!(!text.contains("e\\\\y"), "{text}");
        assert!(!text.contains("e\\y"), "{text}");
        assert_eq!(text.matches(REDACTED).count(), 3, "{text}");
    }

    /// A body that is not text is kept as text within its bound and read
    /// back whole; a record with no head says so.
    #[test]
    fn a_binary_body_is_kept_within_its_bound_and_read_back() {
        let dir = scratch("binary");
        let mut record = Record::new::<&str>("POST", "u", &[]);
        record.head(200, &[]);
        record.chunk(&vec![0xff; MAX_BODY]);
        record.end("ended".into());
        let text = record.text(&[]);
        write(&dir, 5, &text).unwrap();
        let back = read(&dir, 5).unwrap();
        assert_eq!(back, text);
        assert!(back.ends_with("[ended]\n"), "the trailer is kept");
        assert!(back.contains(&format!("it had {MAX_BODY} bytes]")));
        let none = Record::new::<&str>("POST", "u", &[]).text(&[]);
        assert!(none.contains("no reply head came\n"), "{none}");
        assert!(none.ends_with("[its end was not seen]\n"), "{none}");
        let _ = fs::remove_dir_all(&dir);
    }

    /// Records are written private and read back; past the budget the
    /// oldest requests' go first, and a temporary a write left goes too.
    #[test]
    fn records_are_kept_private_and_the_oldest_pruned() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("prune");
        for request in [3_u64, 10, 7] {
            write(&dir, request, &"x".repeat(100)).unwrap();
        }
        let mode = fs::metadata(path(&dir, 7)).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let mode = fs::metadata(dir.join(DIR)).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
        assert_eq!(read(&dir, 10).as_deref(), Some("x".repeat(100).as_str()));
        fs::write(dir.join(DIR).join("4.part"), "left").unwrap();
        fs::write(dir.join(DIR).join("notes"), "not a record").unwrap();
        prune(&dir.join(DIR), 200);
        assert!(read(&dir, 3).is_none());
        assert!(read(&dir, 7).is_some() && read(&dir, 10).is_some());
        assert!(read(&dir, 99).is_none());
        assert!(!dir.join(DIR).join("4.part").exists());
        assert!(dir.join(DIR).join("notes").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    /// A linked records directory, or a link where a temporary goes, is
    /// never written through.
    #[test]
    fn a_record_is_never_written_through_a_link() {
        let dir = scratch("link");
        let elsewhere = scratch("link-elsewhere");
        std::os::unix::fs::symlink(&elsewhere, dir.join(DIR)).unwrap();
        assert!(write(&dir, 1, "x").is_err());
        assert_eq!(fs::read_dir(&elsewhere).unwrap().count(), 0);
        fs::remove_file(dir.join(DIR)).unwrap();
        fs::create_dir(dir.join(DIR)).unwrap();
        let target = elsewhere.join("target");
        std::os::unix::fs::symlink(&target, dir.join(DIR).join("2.part")).unwrap();
        assert!(write(&dir, 2, "x").is_err());
        assert!(!target.exists());
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&elsewhere);
    }
}
