//! Instance metadata a server applies at boot (td-svc/DESIGN.md §10,
//! "Instance metadata"): a NoCloud seed's hostname and Ed25519 keys for the
//! one baked account. Nothing else in the seed is read or acted on.

use std::io::{Read, Seek, SeekFrom};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt};
use std::path::Path;

const O_NOFOLLOW: i32 = 0x20000;
const O_NONBLOCK: i32 = 0x800;
const SYS_BLOCK: &str = "/sys/class/block";
const MAX_DEVICES: usize = 4096;
const MOUNT_PROGRAM: &str = "/bin/mount";
const UMOUNT_PROGRAM: &str = "/bin/umount";
/// Where the seed is mounted read-only while its two files are read.
const MOUNT_POINT: &str = "/run/td-metadata-seed";
const META_DATA: &str = "meta-data";
const USER_DATA: &str = "user-data";
/// Each seed file, and the keys file written from them.
pub(crate) const MAX_FILE_BYTES: u64 = 64 * 1024;
pub(crate) const MAX_KEYS: usize = 64;
const MAX_COMMENT_BYTES: usize = 256;
const MAX_DEPTH: usize = 16;
const ISO_SECTOR: u64 = 2048;
const ISO_VOLUME_ID: std::ops::Range<usize> = 40..72;

/// What a seed asks for, after every check.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Request {
    pub(crate) hostname: Option<crate::hostname::Hostname>,
    /// Canonical `ssh-ed25519 BASE64[ COMMENT]` lines, deduplicated.
    pub(crate) keys: Vec<String>,
    /// Why anything offered was left out, for the console.
    pub(crate) skipped: Vec<String>,
}

/// The keys td reads from each file; any other top-level key is skipped
/// unparsed, so its value cannot refuse the seed.
const META_KEYS: &[&str] = &["local-hostname", "public-keys"];
const USER_KEYS: &[&str] = &[
    "hostname",
    "ssh_authorized_keys",
    "ssh-authorized-keys",
    "users",
];

/// One seed file as read.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SeedFile {
    Absent,
    Text(String),
    /// Not UTF-8 text of at most `MAX_FILE_BYTES`, as compressed
    /// user-data is: skipped, not refused.
    Unreadable,
}

impl SeedFile {
    fn text(&self) -> Option<&str> {
        match self {
            SeedFile::Text(text) => Some(text),
            SeedFile::Absent | SeedFile::Unreadable => None,
        }
    }
}

/// One attached NoCloud seed: its device and its two files.
pub(crate) struct Seed {
    pub(crate) device: String,
    pub(crate) meta: SeedFile,
    pub(crate) user: SeedFile,
}

/// The one NoCloud seed attached, or none. Two seeds are refused, not
/// chosen from.
pub(crate) fn read_nocloud() -> Result<Option<Seed>, String> {
    let mut found: Vec<(String, std::fs::File)> = Vec::new();
    let entries =
        std::fs::read_dir(SYS_BLOCK).map_err(|error| format!("read {SYS_BLOCK}: {error}"))?;
    for (index, entry) in entries.enumerate() {
        if index >= MAX_DEVICES {
            return Err("too many block devices to look for a seed".into());
        }
        let Ok(entry) = entry else { continue };
        let name = entry.file_name();
        let Some(name) = name.to_str().filter(|name| seed_device_name(name)) else {
            continue;
        };
        let path = Path::new("/dev").join(name);
        // A drive with no medium, or a device that vanished, is no seed.
        let Ok(mut file) = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(O_NOFOLLOW | O_NONBLOCK)
            .open(&path)
        else {
            continue;
        };
        let is_block = file
            .metadata()
            .is_ok_and(|metadata| metadata.file_type().is_block_device());
        let mut sector = vec![0; ISO_SECTOR as usize];
        let read = file
            .seek(SeekFrom::Start(16 * ISO_SECTOR))
            .and_then(|_| file.read_exact(&mut sector));
        if is_block && read.is_ok() && seed_label(&sector) {
            found.push((format!("/dev/{name}"), file));
        }
    }
    let mut found = found.into_iter();
    let Some((device, file)) = found.next() else {
        return Ok(None);
    };
    if let Some((other, _)) = found.next() {
        return Err(format!(
            "two NoCloud seeds are attached ({device} and {other})"
        ));
    }
    let (meta, user) = mounted(&file, |root| {
        Ok((
            read_seed_file(&root.join(META_DATA))?,
            read_seed_file(&root.join(USER_DATA))?,
        ))
    })?;
    Ok(Some(Seed { device, meta, user }))
}

/// Whole disks and optical drives a seed may be: no partition, loop, RAM
/// or mapped device.
fn seed_device_name(name: &str) -> bool {
    let digits = |value: &str| !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit());
    let letters = |value: &str| !value.is_empty() && value.bytes().all(|b| b.is_ascii_lowercase());
    if let Some(number) = name.strip_prefix("sr") {
        return digits(number);
    }
    if let Some(rest) = name
        .strip_prefix("vd")
        .or_else(|| name.strip_prefix("sd"))
        .or_else(|| name.strip_prefix("xvd"))
    {
        return letters(rest);
    }
    name.strip_prefix("nvme")
        .and_then(|rest| rest.split_once('n'))
        .is_some_and(|(controller, namespace)| digits(controller) && digits(namespace))
}

/// An ISO-9660 primary volume descriptor whose volume identifier is
/// NoCloud's `cidata`, in either case.
fn seed_label(sector: &[u8]) -> bool {
    sector.get(..7) == Some(b"\x01CD001\x01")
        && sector
            .get(ISO_VOLUME_ID)
            .and_then(|id| std::str::from_utf8(id).ok())
            .is_some_and(|id| id.trim_end_matches(' ').eq_ignore_ascii_case("cidata"))
}

/// Mounts the descriptor's device read-only at `MOUNT_POINT` through its
/// `/proc` link, so the device checked is the device mounted, runs `read`
/// and unmounts. Rock Ridge, which Linux's isofs reads by default, gives
/// the files their NoCloud names.
fn mounted<T>(
    device: &std::fs::File,
    read: impl FnOnce(&Path) -> Result<T, String>,
) -> Result<T, String> {
    let point = Path::new(MOUNT_POINT);
    match std::fs::DirBuilder::new().mode(0o700).create(point) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let metadata = std::fs::symlink_metadata(point)
                .map_err(|error| format!("inspect {MOUNT_POINT}: {error}"))?;
            if !metadata.is_dir() || metadata.uid() != 0 || metadata.mode() & 0o077 != 0 {
                return Err(format!("{MOUNT_POINT} is not a private directory"));
            }
        }
        Err(error) => return Err(format!("create {MOUNT_POINT}: {error}")),
    }
    let source = format!("/proc/{}/fd/{}", std::process::id(), device.as_raw_fd());
    let status = std::process::Command::new(MOUNT_PROGRAM)
        .args(["-t", "iso9660", "-o", "ro,nodev,nosuid,noexec"])
        .arg(&source)
        .arg(point)
        .status()
        .map_err(|error| format!("mount the seed: {error}"))?;
    if !status.success() {
        return Err(format!("mount the seed: {status}"));
    }
    let result = read(point);
    let unmounted = std::process::Command::new(UMOUNT_PROGRAM)
        .arg(point)
        .status()
        .map_err(|error| format!("unmount the seed: {error}"))
        .and_then(|status| {
            if status.success() {
                Ok(())
            } else {
                Err(format!("unmount the seed: {status}"))
            }
        });
    let value = result?;
    unmounted?;
    Ok(value)
}

/// One seed file: absent, a bounded UTF-8 regular file, or a regular file
/// that is neither.
fn read_seed_file(path: &Path) -> Result<SeedFile, String> {
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(O_NOFOLLOW | O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(SeedFile::Absent),
        Err(error) => return Err(format!("open {}: {error}", path.display())),
    };
    let metadata = file
        .metadata()
        .map_err(|error| format!("inspect {}: {error}", path.display()))?;
    if !metadata.is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    if metadata.len() > MAX_FILE_BYTES {
        return Ok(SeedFile::Unreadable);
    }
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("read {}: {error}", path.display()))?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Ok(SeedFile::Unreadable);
    }
    Ok(String::from_utf8(bytes).map_or(SeedFile::Unreadable, SeedFile::Text))
}

/// `request` over a seed's two files, saying why one was not read.
pub(crate) fn seed_request(meta: &SeedFile, user: &SeedFile) -> Result<Request, String> {
    let mut skipped = Vec::new();
    for (name, file) in [(META_DATA, meta), (USER_DATA, user)] {
        if *file == SeedFile::Unreadable {
            skipped.push(format!(
                "{name} is not UTF-8 text of at most {MAX_FILE_BYTES} bytes"
            ));
        }
    }
    if *meta == SeedFile::Absent && *user == SeedFile::Absent {
        skipped.push(format!(
            "the seed holds neither {META_DATA} nor {USER_DATA}"
        ));
    }
    let mut request = request(meta.text(), user.text())?;
    skipped.append(&mut request.skipped);
    request.skipped = skipped;
    Ok(request)
}

/// What `meta-data` and `user-data` ask for. A key td reads that does not
/// parse refuses the seed; a value that fails its own check is skipped and
/// said. `user-data`'s `hostname` is tried before `meta-data`'s
/// `local-hostname`.
pub(crate) fn request(meta: Option<&str>, user: Option<&str>) -> Result<Request, String> {
    let mut request = Request::default();
    let mut names = Vec::new();
    let mut offered = Vec::new();
    if let Some(text) = meta {
        if flow_document(text) {
            request
                .skipped
                .push(format!("{META_DATA} in JSON form is not read"));
        } else {
            let node =
                parse(text, Some(META_KEYS)).map_err(|error| format!("{META_DATA}: {error}"))?;
            if let Some(name) = node.get("local-hostname").and_then(Node::scalar) {
                names.push(("meta-data local-hostname", name.to_string()));
            }
            collect_keys(node.get("public-keys"), &mut offered, &mut request.skipped);
        }
    }
    if let Some(text) = user {
        // NoCloud's other user-data kinds (scripts, MIME archives) are not
        // configuration, so they ask for nothing here.
        if text.lines().next().map(str::trim_end) != Some("#cloud-config") {
            request
                .skipped
                .push(format!("{USER_DATA} is not #cloud-config"));
        } else if flow_document(text) {
            request
                .skipped
                .push(format!("{USER_DATA} in JSON form is not read"));
        } else {
            let node =
                parse(text, Some(USER_KEYS)).map_err(|error| format!("{USER_DATA}: {error}"))?;
            if let Some(name) = node.get("hostname").and_then(Node::scalar) {
                names.insert(0, ("user-data hostname", name.to_string()));
            }
            for key in ["ssh_authorized_keys", "ssh-authorized-keys"] {
                collect_keys(node.get(key), &mut offered, &mut request.skipped);
            }
            // Every user's keys admit the baked account, whose name wins.
            if let Some(Node::Seq(users)) = node.get("users") {
                for user in users {
                    for key in ["ssh_authorized_keys", "ssh-authorized-keys"] {
                        collect_keys(user.get(key), &mut offered, &mut request.skipped);
                    }
                }
            }
        }
    }
    // The first label, as cloud-init sets by default, of the first name
    // offered that has a valid one; each one passed over is said.
    for (source, name) in names.iter().filter(|(_, name)| !name.is_empty()) {
        let name = name.to_ascii_lowercase();
        match crate::hostname::Hostname::parse(name.split('.').next().unwrap_or_default()) {
            Ok(name) => {
                request.hostname = Some(name);
                break;
            }
            Err(_) => request.skipped.push(format!("an invalid {source}")),
        }
    }
    let (mut refused, mut surplus) = (0, 0);
    // One line per key blob, the first comment offered kept.
    let blob = |key: &str| key.split(' ').nth(1).map(str::to_owned);
    for line in offered {
        match canonical_key(&line) {
            Some(key) if request.keys.iter().any(|kept| blob(kept) == blob(&key)) => {}
            Some(_) if request.keys.len() >= MAX_KEYS => surplus += 1,
            Some(key) => request.keys.push(key),
            None => refused += 1,
        }
    }
    if refused > 0 {
        request
            .skipped
            .push(format!("{refused} key(s) other than plain ssh-ed25519"));
    }
    if surplus > 0 {
        request
            .skipped
            .push(format!("{surplus} key(s) past the first {MAX_KEYS}"));
    }
    Ok(request)
}

/// A document whose first content is a flow mapping, as JSON is.
fn flow_document(text: &str) -> bool {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#') && *line != "---")
        .is_some_and(|line| line.starts_with('{'))
}

/// Each line of a string, as a block scalar lists keys, or of each string
/// in a sequence.
fn collect_keys(node: Option<&Node>, out: &mut Vec<String>, skipped: &mut Vec<String>) {
    let lines = |text: &str, out: &mut Vec<String>| {
        out.extend(
            text.lines()
                .filter(|line| !line.trim().is_empty())
                .map(str::to_owned),
        );
    };
    match node {
        None => {}
        Some(Node::Scalar(key)) => lines(key, out),
        Some(Node::Seq(items)) => {
            for item in items {
                match item {
                    Node::Scalar(key) => lines(key, out),
                    _ => skipped.push("a key that is not a string".into()),
                }
            }
        }
        Some(Node::Map(_)) => skipped.push("keys given as a mapping".into()),
    }
}

/// `ssh-ed25519 BASE64[ COMMENT]` whose blob is one Ed25519 public key,
/// rewritten canonically. Option prefixes and other key types are refused:
/// the server's sshd admits Ed25519 alone. An unprintable or long comment
/// is dropped, not the key.
pub(crate) fn canonical_key(line: &str) -> Option<String> {
    let rest = line.trim().strip_prefix("ssh-ed25519 ")?;
    let (blob, comment) = rest
        .trim_start()
        .split_once(char::is_whitespace)
        .map_or((rest.trim_start(), ""), |(blob, comment)| {
            (blob, comment.trim())
        });
    let decoded = crate::encoding::from_base64(blob)?;
    let mut expected = Vec::with_capacity(51);
    expected.extend_from_slice(&11u32.to_be_bytes());
    expected.extend_from_slice(b"ssh-ed25519");
    expected.extend_from_slice(&32u32.to_be_bytes());
    if decoded.len() != 51 || decoded.get(..19) != Some(expected.as_slice()) {
        return None;
    }
    let comment = Some(comment).filter(|comment| {
        !comment.is_empty()
            && comment.len() <= MAX_COMMENT_BYTES
            && comment.bytes().all(|byte| (0x20..0x7f).contains(&byte))
    });
    Some(match comment {
        Some(comment) => format!("ssh-ed25519 {blob} {comment}"),
        None => format!("ssh-ed25519 {blob}"),
    })
}

/// The keys file's bytes: one line per key.
pub(crate) fn keys_file(keys: &[String]) -> Vec<u8> {
    let mut bytes = Vec::new();
    for key in keys {
        bytes.extend_from_slice(key.as_bytes());
        bytes.push(b'\n');
    }
    bytes
}

/// A YAML value of the block subset cloud-config is written in.
#[derive(Debug, PartialEq, Eq)]
enum Node {
    Scalar(String),
    Seq(Vec<Node>),
    Map(Vec<(String, Node)>),
}

impl Node {
    fn get(&self, key: &str) -> Option<&Node> {
        match self {
            Node::Map(entries) => entries
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, node)| node),
            _ => None,
        }
    }

    fn scalar(&self) -> Option<&str> {
        match self {
            Node::Scalar(text) => Some(text),
            _ => None,
        }
    }
}

/// One content line: its indent in spaces, its text after them, whether
/// that text starts with a tab, and its 1-based number for errors.
struct Line<'a> {
    indent: usize,
    text: &'a str,
    tab: bool,
    number: usize,
}

fn is_item(text: &str) -> bool {
    text == "-" || text.starts_with("- ")
}

/// Block mappings and sequences of plain, single- and double-quoted
/// scalars, flow sequences of scalars, one-line flow mappings (kept
/// opaque) and literal or folded block scalars, under one optional `---`.
/// With `wanted`, a top-level key outside it is skipped unparsed. In what
/// is parsed, anchors, aliases, tags, multi-line plain scalars, duplicate
/// keys and tab indentation are refused rather than guessed at.
fn parse(text: &str, wanted: Option<&[&str]>) -> Result<Node, String> {
    let mut lines = Vec::new();
    let (mut started, mut ended) = (false, false);
    for (index, raw) in text.split('\n').enumerate() {
        let number = index + 1;
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        let body = raw.trim_start_matches(' ');
        // A marker stands alone at column 0; indented, it is a block
        // scalar's text.
        // A directive comes before the document starts.
        if !started && raw.starts_with('%') {
            continue;
        }
        match strip_comment(raw) {
            "---" if !started && !ended => {
                started = true;
                continue;
            }
            "..." if !ended => {
                ended = true;
                continue;
            }
            "---" | "..." => return Err(format!("line {number}: a second document")),
            _ => {}
        }
        if body.trim().is_empty() || body.starts_with('#') {
            // Kept for block scalars, which own their blank lines.
            lines.push(Line {
                indent: usize::MAX,
                text: raw,
                tab: false,
                number,
            });
            continue;
        }
        if ended {
            return Err(format!("line {number}: a second document"));
        }
        started = true;
        lines.push(Line {
            indent: raw.len() - body.len(),
            text: body.trim_end(),
            tab: body.starts_with('\t'),
            number,
        });
    }
    if let Some(wanted) = wanted {
        lines = keep_top_level(lines, wanted);
    }
    let mut parser = Parser { lines, at: 0 };
    parser.skip_blank();
    let Some(first) = parser.peek() else {
        return Ok(Node::Map(Vec::new()));
    };
    let indent = first.indent;
    let node = parser.block(indent, 0)?;
    parser.skip_blank();
    if let Some(line) = parser.peek() {
        return Err(format!("line {}: unexpected indentation", line.number));
    }
    Ok(node)
}

/// The lines of a margin-aligned top-level mapping's `wanted` keys: each
/// key's line and every line up to the next key at the margin. A line at
/// the margin that is no key, as a multi-line flow collection's or a
/// directive's, goes with the key above it.
fn keep_top_level<'a>(lines: Vec<Line<'a>>, wanted: &[&str]) -> Vec<Line<'a>> {
    let at_margin = |line: &Line| line.indent == 0 && !line.tab && !is_item(line.text);
    let first = lines.iter().find(|line| line.indent != usize::MAX);
    if !first.is_some_and(at_margin) {
        return lines;
    }
    let mut keep = false;
    lines
        .into_iter()
        .filter(|line| {
            if at_margin(line) {
                keep = split_key(line.text).map_or(keep, |(key, _)| wanted.contains(&key));
            }
            keep
        })
        .collect()
}

struct Parser<'a> {
    lines: Vec<Line<'a>>,
    at: usize,
}

impl<'a> Parser<'a> {
    fn skip_blank(&mut self) {
        while self
            .lines
            .get(self.at)
            .is_some_and(|line| line.indent == usize::MAX)
        {
            self.at += 1;
        }
    }

    fn peek(&mut self) -> Option<&Line<'a>> {
        self.skip_blank();
        self.lines.get(self.at)
    }

    /// The mapping or sequence whose lines start at exactly `indent`.
    fn block(&mut self, indent: usize, depth: usize) -> Result<Node, String> {
        if depth > MAX_DEPTH {
            return Err("nested too deeply".into());
        }
        let Some(first) = self.peek() else {
            return Err("a value is missing".into());
        };
        if is_item(first.text) {
            self.sequence(indent, depth)
        } else {
            self.mapping(indent, depth, None)
        }
    }

    fn sequence(&mut self, indent: usize, depth: usize) -> Result<Node, String> {
        let mut items = Vec::new();
        while let Some(line) = self.peek() {
            if line.tab && line.indent >= indent {
                return Err(format!("line {}: tab indentation", line.number));
            }
            if line.indent != indent || !is_item(line.text) {
                break;
            }
            let number = line.number;
            let rest = line.text.get(1..).unwrap_or_default();
            let content = rest.trim_start_matches(' ');
            self.at += 1;
            if content.is_empty() || content.starts_with('#') {
                items.push(self.nested(indent, depth)?);
            } else if let Some((key, value)) = split_key(content) {
                // `- key: value` opens a mapping whose further keys align
                // with `key`.
                let inner = indent + 1 + (rest.len() - content.len());
                let first = (
                    key.to_string(),
                    self.value(value, inner, depth + 1, number)?,
                );
                items.push(self.mapping(inner, depth + 1, Some(first))?);
            } else {
                items.push(self.inline(content, indent, number)?);
            }
        }
        Ok(Node::Seq(items))
    }

    fn mapping(
        &mut self,
        indent: usize,
        depth: usize,
        first: Option<(String, Node)>,
    ) -> Result<Node, String> {
        // A sequence at its key's own indent reaches here without `block`.
        if depth > MAX_DEPTH {
            return Err("nested too deeply".into());
        }
        let mut entries: Vec<(String, Node)> = first.into_iter().collect();
        while let Some(line) = self.peek() {
            if line.indent < indent {
                break;
            }
            let number = line.number;
            if line.tab {
                return Err(format!("line {number}: tab indentation"));
            }
            if line.indent > indent {
                return Err(format!("line {number}: unexpected indentation"));
            }
            let Some((key, value)) = split_key(line.text) else {
                return Err(format!("line {number}: expected `key: value`"));
            };
            let key = key.to_string();
            self.at += 1;
            let node = self.value(value, indent, depth, number)?;
            if entries.iter().any(|(name, _)| *name == key) {
                return Err(format!("line {number}: duplicate key {key:?}"));
            }
            entries.push((key, node));
        }
        Ok(Node::Map(entries))
    }

    /// A key's value: inline, a block scalar, or the block below it. A
    /// sequence may sit at the key's own indent.
    fn value(
        &mut self,
        text: &str,
        indent: usize,
        depth: usize,
        number: usize,
    ) -> Result<Node, String> {
        if !text.is_empty() {
            return self.inline(text, indent, number);
        }
        match self.peek() {
            Some(next) if next.indent == indent && is_item(next.text) => {
                self.sequence(indent, depth + 1)
            }
            _ => self.nested(indent, depth),
        }
    }

    /// The block more indented than `indent`, or an empty scalar.
    fn nested(&mut self, indent: usize, depth: usize) -> Result<Node, String> {
        match self.peek() {
            Some(next) if next.indent > indent => {
                let inner = next.indent;
                self.block(inner, depth + 1)
            }
            _ => Ok(Node::Scalar(String::new())),
        }
    }

    fn inline(&mut self, text: &str, indent: usize, number: usize) -> Result<Node, String> {
        let text = strip_comment(text);
        let first = text.as_bytes().first().copied();
        let node = match first {
            Some(b'|') | Some(b'>') => {
                let folded = first == Some(b'>');
                // Chomping and indentation indicators.
                let header = text.get(1..).unwrap_or_default();
                if header.len() > 2
                    || !header
                        .bytes()
                        .all(|byte| matches!(byte, b'1'..=b'9' | b'-' | b'+'))
                {
                    return Err(format!("line {number}: unsupported block scalar header"));
                }
                return Ok(Node::Scalar(self.block_scalar(indent, folded)));
            }
            Some(b'[') => Node::Seq(flow_sequence(text, number)?),
            // Nothing td reads is a mapping, so one on a line is kept whole.
            Some(b'{') if text.ends_with('}') => Node::Scalar(text.to_string()),
            Some(b'{') => return Err(format!("line {number}: an unclosed flow mapping")),
            Some(b'&') | Some(b'*') | Some(b'!') | Some(b'%') | Some(b'@') | Some(b'`') => {
                return Err(format!("line {number}: unsupported YAML construct"));
            }
            _ => Node::Scalar(scalar(text, number)?),
        };
        // A scalar continued on the next, deeper line is not supported.
        if let Some(next) = self.peek() {
            if next.indent > indent && !matches!(node, Node::Seq(_)) {
                return Err(format!(
                    "line {}: a value continued on another line",
                    next.number
                ));
            }
        }
        Ok(node)
    }

    /// The lines more indented than `indent`, joined by newlines (literal)
    /// or spaces (folded); only skipped values use them.
    fn block_scalar(&mut self, indent: usize, folded: bool) -> String {
        let mut text = String::new();
        // The first content line's indent; a shallower line ends the
        // scalar, and the parser then refuses it if it is no key.
        let mut content = None;
        while let Some(line) = self.lines.get(self.at) {
            let empty = line.text.trim().is_empty();
            // A comment line keeps its raw text, so its own indent.
            let depth = if line.indent == usize::MAX {
                line.text.len() - line.text.trim_start_matches(' ').len()
            } else {
                line.indent
            };
            if !empty && (depth <= indent || content.is_some_and(|content| depth < content)) {
                break;
            }
            if !empty && content.is_none() {
                content = Some(depth);
            }
            if empty {
                text.push('\n');
            } else {
                if !text.is_empty() && !text.ends_with('\n') {
                    text.push(if folded { ' ' } else { '\n' });
                }
                text.push_str(line.text.trim());
            }
            self.at += 1;
        }
        text.trim_end_matches('\n').to_string()
    }
}

/// `key: value` or `key:`, the key a plain or quoted scalar, any comment
/// gone.
fn split_key(text: &str) -> Option<(&str, &str)> {
    let text = strip_comment(text);
    let (key, value) = if text.starts_with(['"', '\'']) {
        // A quoted key ends at its closing quote; a quoted scalar with no
        // `:` after it is no key.
        let end = quoted_end(text)?;
        let rest = text.get(end..)?.trim_start_matches(' ').strip_prefix(':')?;
        if !(rest.is_empty() || rest.starts_with([' ', '\t'])) {
            return None;
        }
        (text.get(..end)?, rest.trim_start())
    } else {
        let separator = [": ", ":\t"]
            .iter()
            .filter_map(|separator| text.find(separator))
            .min();
        match separator {
            Some(at) => (text.get(..at)?, text.get(at + 1..)?.trim_start()),
            None => (text.strip_suffix(':')?, ""),
        }
    };
    let key = key.trim_end();
    if key.is_empty() || key.starts_with(['-', '[', '{', '#']) {
        return None;
    }
    let key = key
        .strip_prefix('"')
        .and_then(|key| key.strip_suffix('"'))
        .or_else(|| {
            key.strip_prefix('\'')
                .and_then(|key| key.strip_suffix('\''))
        })
        .unwrap_or(key);
    Some((key, value))
}

/// `text` before a comment: a `#` at its start or after a space or tab,
/// outside quotes. The rest is trimmed at its end.
fn strip_comment(text: &str) -> &str {
    let bytes = text.as_bytes();
    let mut index = 0;
    while let Some(&byte) = bytes.get(index) {
        let after_space = index == 0 || matches!(bytes.get(index - 1), Some(b' ' | b'\t'));
        match byte {
            b'#' if after_space => return text.get(..index).unwrap_or_default().trim_end(),
            // A quote opens a string only where a scalar can start.
            b'"' | b'\''
                if index == 0
                    || matches!(
                        bytes.get(index - 1),
                        Some(b' ' | b'\t' | b'[' | b',' | b'{' | b':')
                    ) =>
            {
                match text.get(index..).and_then(quoted_end) {
                    Some(length) => index += length,
                    None => return text.trim_end(),
                }
            }
            _ => index += 1,
        }
    }
    text.trim_end()
}

fn flow_sequence(text: &str, number: usize) -> Result<Vec<Node>, String> {
    let inner = text
        .strip_prefix('[')
        .and_then(|text| text.strip_suffix(']'))
        .ok_or_else(|| format!("line {number}: an unclosed flow sequence"))?;
    if inner.trim().is_empty() {
        return Ok(Vec::new());
    }
    let mut items = Vec::new();
    let mut rest = inner.trim_start();
    loop {
        let (item, after) = match rest.as_bytes().first() {
            Some(b'"') | Some(b'\'') => {
                let end = quoted_end(rest)
                    .ok_or_else(|| format!("line {number}: an unterminated string"))?;
                let after = rest.get(end..).unwrap_or_default();
                if !(after.trim_start().is_empty() || after.trim_start().starts_with(',')) {
                    return Err(format!("line {number}: text after a string"));
                }
                (rest.get(..end).unwrap_or_default(), after)
            }
            _ => rest
                .split_once(',')
                .map_or((rest, ""), |(item, after)| (item, after)),
        };
        if !item.starts_with(['"', '\'']) && item.contains(['[', ']', '{', '}']) {
            return Err(format!("line {number}: a nested flow collection"));
        }
        items.push(Node::Scalar(scalar(item.trim(), number)?));
        let after = after.trim_start();
        let after = after.strip_prefix(',').unwrap_or(after).trim_start();
        if after.is_empty() {
            break;
        }
        rest = after;
    }
    Ok(items)
}

/// The byte just past a quoted scalar starting `text`.
fn quoted_end(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let quote = *bytes.first()?;
    let mut index = 1;
    while let Some(byte) = bytes.get(index) {
        match (quote, *byte) {
            (b'"', b'\\') => index += 2,
            (b'\'', b'\'') if bytes.get(index + 1) == Some(&b'\'') => index += 2,
            (q, b) if q == b => return Some(index + 1),
            _ => index += 1,
        }
    }
    None
}

/// A plain or quoted scalar, its trailing comment removed.
fn scalar(text: &str, number: usize) -> Result<String, String> {
    match text.as_bytes().first() {
        Some(b'"') | Some(b'\'') => {
            let end =
                quoted_end(text).ok_or_else(|| format!("line {number}: an unterminated string"))?;
            let tail = text.get(end..).unwrap_or_default().trim();
            if !(tail.is_empty() || tail.starts_with('#')) {
                return Err(format!("line {number}: text after a string"));
            }
            let body = text.get(1..end - 1).unwrap_or_default();
            if text.starts_with('\'') {
                Ok(body.replace("''", "'"))
            } else {
                unescape(body, number)
            }
        }
        _ => {
            let value = match text.find(" #") {
                Some(at) => text.get(..at).unwrap_or_default(),
                None => text,
            };
            Ok(value.trim_end().to_string())
        }
    }
}

fn unescape(body: &str, number: usize) -> Result<String, String> {
    let mut out = String::with_capacity(body.len());
    let mut chars = body.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some('/') => out.push('/'),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            _ => return Err(format!("line {number}: an unsupported escape")),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

    fn parse_all(text: &str) -> Result<Node, String> {
        parse(text, None)
    }

    /// A distinct valid key for each `seed`.
    fn key(seed: u8) -> String {
        let mut blob = Vec::new();
        blob.extend_from_slice(&11u32.to_be_bytes());
        blob.extend_from_slice(b"ssh-ed25519");
        blob.extend_from_slice(&32u32.to_be_bytes());
        blob.extend_from_slice(&[seed; 32]);
        format!("ssh-ed25519 {}", crate::encoding::base64(&blob))
    }

    const KEY_A: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIGmTjqOMdHoPzoqbE+wEE5Q4aNmEGC0tgJbQOkxqdgUi";
    const KEY_B: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIKfrDzq8C4wZk7DS0KzwkwRUzvsRUsfgXiEL7c/R1oQy";

    #[test]
    fn a_cloud_localds_seed_asks_for_its_hostname_and_every_key() {
        let meta = "instance-id: iid-local01\nlocal-hostname: from-meta\n";
        let user = format!(
            "#cloud-config\n\
             hostname: Seeded-Host\n\
             ssh_authorized_keys:\n  - {KEY_A} ada@laptop\n\
             users:\n  - default\n  - name: someone-else\n    shell: /bin/bash\n    \
             ssh_authorized_keys:\n      - {KEY_B}\n      - {KEY_A} again\n\
             write_files:\n  - path: /etc/x\n    content: |\n      line one\n\n      line two\n\
             runcmd: [ls, 'echo hi']\n"
        );
        let asked = request(Some(meta), Some(&user)).unwrap();
        assert_eq!(asked.hostname.unwrap().name(), "seeded-host");
        assert_eq!(
            asked.keys,
            vec![format!("{KEY_A} ada@laptop"), KEY_B.to_string()]
        );
        assert!(asked.skipped.is_empty(), "{:?}", asked.skipped);
    }

    #[test]
    fn meta_data_alone_names_the_host_and_its_public_keys() {
        let meta = format!("local-hostname: node7\npublic-keys:\n- {KEY_B}\n");
        let asked = request(Some(&meta), None).unwrap();
        assert_eq!(asked.hostname.unwrap().name(), "node7");
        assert_eq!(asked.keys, vec![KEY_B.to_string()]);
        // A compact sequence and quoted keys parse alike.
        let user = format!("#cloud-config\nssh_authorized_keys: [\"{KEY_A}\", '{KEY_B}']\n");
        assert_eq!(request(None, Some(&user)).unwrap().keys.len(), 2);
    }

    #[test]
    fn what_fails_its_own_check_is_skipped_and_said() {
        let user = format!(
            "#cloud-config\nhostname: not_a_host\nssh_authorized_keys:\n  - ssh-rsa AAAAB3NzaC1yc2E= x\n  - from=\"10.0.0.1\" {KEY_A}\n  - ssh-ed25519 AAAA\n  - {KEY_A} bad\u{7}comment\n"
        );
        let asked = request(None, Some(&user)).unwrap();
        assert_eq!(asked.hostname, None);
        assert_eq!(asked.keys, vec![KEY_A.to_string()]);
        assert_eq!(
            asked.skipped,
            vec![
                "an invalid user-data hostname".to_string(),
                "3 key(s) other than plain ssh-ed25519".to_string()
            ]
        );
        let script = request(None, Some("#!/bin/sh\necho hi\n")).unwrap();
        assert_eq!(script.skipped, vec!["user-data is not #cloud-config"]);
        assert!(script.keys.is_empty());
    }

    #[test]
    fn what_the_subset_cannot_read_is_refused_whole() {
        for bad in [
            "a: 1\na: 2\n",
            "a:\n\t- x\n",
            "a: &anchor x\n",
            "a: *alias\n",
            "a: !!str x\n",
            "a: {b: c\n",
            "a: [x] y\n",
            "a: ['x' y, z]\n",
            "a: |9x\n",
            "a:\n  b: 1\n\t c: 2\n",
            "a: \"open\n",
            "a: \"bad \\q escape\"\n",
            "a: plain\n  continued\n",
            "a: 1\n---\nb: 2\n",
            "  a: 1\nb: 2\n",
            "just text\n",
            "a: [x, [y]]\n",
        ] {
            assert!(parse_all(bad).is_err(), "{bad:?}");
        }
        let nested = (0..20).fold(String::new(), |text, level| {
            format!("{text}{}k{level}:\n", " ".repeat(level * 2))
        }) + &" ".repeat(40)
            + "leaf: x\n";
        assert!(parse_all(&nested).is_err());
        // Sequences at their keys' own indent nest without a block.
        let chain = (0..20).fold(String::new(), |text, level| {
            format!("{text}{}- a:\n", " ".repeat(level * 2))
        });
        assert!(parse_all(&chain).is_err());
        assert!(parse_all(&chain.lines().take(4).collect::<Vec<_>>().join("\n")).is_ok());
        // A shallower line inside a block scalar ends it, and is no key.
        assert!(parse_all("k: |\n    a\n  b\n").is_err());
        assert!(request(None, Some("#cloud-config\nhostname: a\nhostname: b\n")).is_err());
        assert!(request(None, Some("#cloud-config\nusers:\n\t- name: x\n")).is_err());
        assert!(request(None, Some("#cloud-config\nusers: &all\n  - x\n")).is_err());
    }

    /// cloud-localds's own example and what tutorials and generators write
    /// beside the keys td reads: none of it refuses the seed.
    #[test]
    fn what_td_does_not_read_cannot_refuse_the_seed() {
        let user = format!(
            "#cloud-config\n\
             password: passw0rd\n\
             chpasswd: {{ expire: False }}\n\
             ssh_pwauth: True\n\
             ssh_authorized_keys: # admins\n  - {KEY_A}\n\
             apt: {{\"sources\": {{}}}}\n\
             base: &base\n  x: !!str 1\n\
             other: *base\n\
             write_files:\n  - path: /etc/Makefile\n    content: |\n      all:\n      \techo hi\n      ---\n      ...\n  - path: /etc/k8s.yaml\n    content: |2\n      a: 1\n\
             runcmd: [ls, 'echo [hi]'] # list\n\
             users:\n  - default\n  - name: x\n    groups: [adm, sudo]\n    sudo: ALL=(ALL) NOPASSWD:ALL\n    \
             lock_passwd: false\n    chpasswd: {{ expire: False }}\n    ssh_authorized_keys: |\n      {KEY_B}\n      {}\n\
             final_message: >\n  done\n  ---\n",
            key(7)
        );
        let asked = request(None, Some(&user)).unwrap();
        assert_eq!(
            asked.keys,
            vec![KEY_A.to_string(), KEY_B.to_string(), key(7)]
        );
        assert!(asked.skipped.is_empty(), "{:?}", asked.skipped);
        // Comments, even ending in a colon, tabs after a colon, folded
        // key lists and what follows a skipped key at the margin.
        let user = format!(
            "#cloud-config\n\
             %YAML 1.2\n\
             --- # the one document\n\
             runcmd: [\nls,\n]\n\
             ssh_authorized_keys: # admin keys:\n  - {KEY_A} # laptop:\n\
             hostname:\tweb # was db:\n\
             users:\n  - name: x\n    ssh_authorized_keys: >\n      {KEY_B}\n\n      {}\n\
             # a comment at the margin\n  - name: y\n    \
             ssh_authorized_keys: [\"{}\"] # see [1]\n",
            key(8),
            key(9)
        );
        let asked = request(None, Some(&user)).unwrap();
        assert_eq!(asked.hostname.unwrap().name(), "web");
        assert_eq!(
            asked.keys,
            vec![KEY_A.to_string(), KEY_B.to_string(), key(8), key(9)]
        );
        assert!(asked.skipped.is_empty(), "{:?}", asked.skipped);
        // Margin markers still bound the document.
        assert!(request(None, Some("#cloud-config\nhostname: a\n---\nb: 1\n")).is_err());
        assert!(request(None, Some("#cloud-config\nhostname: a\n...\nb: 1\n")).is_err());
    }

    #[test]
    fn the_first_valid_name_gives_its_first_label() {
        let meta = "local-hostname: node-1.example.com\n";
        for (user, skipped) in [
            ("#cloud-config\nhostname:\n", &[][..]),
            (
                "#cloud-config\nhostname: My_Host\n",
                &["an invalid user-data hostname"][..],
            ),
            ("#cloud-config\n", &[][..]),
        ] {
            let asked = request(Some(meta), Some(user)).unwrap();
            assert_eq!(asked.hostname.unwrap().name(), "node-1", "{user:?}");
            assert_eq!(asked.skipped, skipped, "{user:?}");
        }
        let asked = request(Some("local-hostname: Web.Example\n"), None).unwrap();
        assert_eq!(asked.hostname.unwrap().name(), "web");
        let asked = request(Some("local-hostname: 1bad\n"), None).unwrap();
        assert_eq!(asked.hostname, None);
        assert_eq!(asked.skipped, vec!["an invalid meta-data local-hostname"]);
        assert!(request(Some("local-hostname:\n"), None)
            .unwrap()
            .skipped
            .is_empty());
    }

    #[test]
    fn keys_past_the_cap_are_counted_apart() {
        let keys: String = (0..70).map(|seed| format!("  - {}\n", key(seed))).collect();
        let user = format!("#cloud-config\nssh_authorized_keys:\n{keys}  - ssh-rsa AAAA\n");
        let asked = request(None, Some(&user)).unwrap();
        assert_eq!(asked.keys.len(), MAX_KEYS);
        assert_eq!(
            asked.skipped,
            vec![
                "1 key(s) other than plain ssh-ed25519".to_string(),
                format!("6 key(s) past the first {MAX_KEYS}")
            ]
        );
    }

    #[test]
    fn an_unreadable_or_empty_seed_is_said_not_refused() {
        let meta = SeedFile::Text("local-hostname: node\n".into());
        let asked = seed_request(&meta, &SeedFile::Unreadable).unwrap();
        assert_eq!(asked.hostname.unwrap().name(), "node");
        assert_eq!(
            asked.skipped,
            vec![format!(
                "user-data is not UTF-8 text of at most {MAX_FILE_BYTES} bytes"
            )]
        );
        let asked = seed_request(&SeedFile::Absent, &SeedFile::Absent).unwrap();
        assert_eq!(
            asked.skipped,
            vec!["the seed holds neither meta-data nor user-data"]
        );
        let json = SeedFile::Text("{\"local-hostname\": \"node\"}\n".into());
        let asked = seed_request(&json, &SeedFile::Absent).unwrap();
        assert_eq!(asked.hostname, None);
        assert_eq!(asked.skipped, vec!["meta-data in JSON form is not read"]);
    }

    #[test]
    fn the_subset_reads_what_cloud_config_writes() {
        let node = parse_all(
            "---\n# comment\nk: v # trailing\nq: 'it''s'\nd: \"a\\\"b\"\nempty:\nlist:\n- one\n- 'two'\nmap:\n  inner: x\n",
        )
        .unwrap();
        assert_eq!(node.get("k").and_then(Node::scalar), Some("v"));
        assert_eq!(node.get("q").and_then(Node::scalar), Some("it's"));
        assert_eq!(node.get("d").and_then(Node::scalar), Some("a\"b"));
        assert_eq!(node.get("empty").and_then(Node::scalar), Some(""));
        assert_eq!(
            node.get("list"),
            Some(&Node::Seq(vec![
                Node::Scalar("one".into()),
                Node::Scalar("two".into())
            ]))
        );
        assert_eq!(
            node.get("map")
                .and_then(|map| map.get("inner"))
                .and_then(Node::scalar),
            Some("x")
        );
        assert_eq!(parse_all("").unwrap(), Node::Map(Vec::new()));
        // A quoted scalar holding `: ` is no key; a quoted key may hold one.
        assert_eq!(
            parse_all("- \"ssh-ed25519 AAAA note: work\"\n").unwrap(),
            Node::Seq(vec![Node::Scalar("ssh-ed25519 AAAA note: work".into())])
        );
        assert_eq!(
            parse_all("\"a: b\": c\n")
                .unwrap()
                .get("a: b")
                .and_then(Node::scalar),
            Some("c")
        );
        // A block scalar's text may hold tabs and indented markers.
        assert_eq!(
            parse_all("a: |\n  \tx\n  ---\nb: 1\n")
                .unwrap()
                .get("a")
                .and_then(Node::scalar),
            Some("x\n---")
        );
        assert_eq!(
            parse_all("a: [x, 'y]'] # c\n").unwrap().get("a"),
            Some(&Node::Seq(vec![
                Node::Scalar("x".into()),
                Node::Scalar("y]".into())
            ]))
        );
        assert_eq!(
            parse_all("a: {b: c} # x\n")
                .unwrap()
                .get("a")
                .and_then(Node::scalar),
            Some("{b: c}")
        );
    }

    #[test]
    fn only_one_ed25519_blob_is_a_key() {
        assert_eq!(canonical_key(KEY_A).as_deref(), Some(KEY_A));
        assert_eq!(
            canonical_key(&format!("  {KEY_A}   ada@host  ")).as_deref(),
            Some(format!("{KEY_A} ada@host").as_str())
        );
        // A long comment is dropped, the key kept.
        let long = format!("{KEY_A} {}", "c".repeat(MAX_COMMENT_BYTES + 1));
        assert_eq!(canonical_key(&long).as_deref(), Some(KEY_A));
        for bad in [
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIGmTjqOMdHoPzoqbE+wEE5Q4aNmEGC0tgJbQOkxqdgU",
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIGmTjqOMdHoPzoqbE+wEE5Q4aNmEGC0tgJbQOkxqdgUi=",
            // The same length, but a blob naming another key type.
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE4AAAAIGmTjqOMdHoPzoqbE+wEE5Q4aNmEGC0tgJbQOkxqdgUi",
            "ssh-ed25519",
            "ssh-dss AAAA",
        ] {
            assert_eq!(canonical_key(bad), None, "{bad}");
        }
    }

    #[test]
    fn a_seed_is_a_whole_disk_labelled_cidata() {
        for name in ["sr0", "vda", "vdb", "sdc", "xvda", "nvme0n1", "nvme12n3"] {
            assert!(seed_device_name(name), "{name}");
        }
        for name in [
            "vda1",
            "sda2",
            "nvme0n1p1",
            "loop0",
            "ram0",
            "dm-0",
            "sr",
            "vd",
            "nvme0",
            "md0",
        ] {
            assert!(!seed_device_name(name), "{name}");
        }
        let mut sector = vec![0u8; 2048];
        sector[..7].copy_from_slice(b"\x01CD001\x01");
        let mut id = *b"cidata                          ";
        sector[ISO_VOLUME_ID].copy_from_slice(&id);
        assert!(seed_label(&sector));
        id[..6].copy_from_slice(b"CIDATA");
        sector[ISO_VOLUME_ID].copy_from_slice(&id);
        assert!(seed_label(&sector));
        id[..6].copy_from_slice(b"TD_INS");
        sector[ISO_VOLUME_ID].copy_from_slice(&id);
        assert!(!seed_label(&sector));
        assert!(!seed_label(&[0; 2048]));
    }

    #[test]
    fn the_keys_file_is_one_line_per_key() {
        assert_eq!(
            keys_file(&[KEY_A.to_string(), KEY_B.to_string()]),
            format!("{KEY_A}\n{KEY_B}\n").into_bytes()
        );
        assert!(keys_file(&[]).is_empty());
    }
}
