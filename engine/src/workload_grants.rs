//! The workload grant table (td-authd/DESIGN.md, "Workload grant table";
//! WORKLOADS.md, "Grants") and the manifest of one account's published
//! views. A grant row names an account by kind and name, as the workload
//! account ledger keys it, and what that account may reach of the
//! human's. The file is std-only, uses only `workload_accounts` beside it
//! and performs no I/O, so target programs include it by path: td-firstboot
//! seeds the table, td-authd publishes a launching account's views from
//! it and lists them in that account's manifest, and td-jail admits the
//! views the manifest lists. Every directory a row or a manifest names is
//! checked by `validate_path`, the one byte validator all three share.

use crate::workload_accounts::{valid_name, Kind};

const MAGIC: &str = "td-grants-v1\n";
/// The table's name in td's persistent state directory, `/var/lib/td`.
pub const FILE_NAME: &str = "grants.tsv";
/// The published table's exact mode: root alone reads it.
pub const MODE: u32 = 0o600;
/// Rows stay until the human revokes them, and a rolled-back deployment
/// must still read what a later one wrote, so the v1 bounds are generous.
pub const MAX_BYTES: usize = 1024 * 1024;
const MAX_ROWS: usize = 4096;
/// Directory rows one account may hold, and so views one launch lists.
pub const MAX_DIRECTORIES: usize = 32;
/// View indices: a launch publishes a replacement beside the view it
/// replaces before releasing that one, so twice the rows.
pub const MAX_VIEWS: usize = 2 * MAX_DIRECTORIES;
const MAX_SOCKETS: usize = 16;
/// The root-owned tree the views are published in, one directory per
/// account uid below it: `VIEW_ROOT/UID/N`, outside every home a workload
/// writes.
pub const VIEW_ROOT: &str = "/var/lib/td/grant-views";
/// `VIEW_ROOT`'s last component, below `/var/lib/td`.
pub const VIEW_ROOT_NAME: &str = "grant-views";
/// An account's view directory, root-owned and of the account's group: the
/// account reaches the names it knows (its manifest and the views it lists)
/// without listing the directory, and no other account enters it.
pub const VIEWS_MODE: u32 = 0o710;
/// The manifest's name in an account's view directory, root-owned, of the
/// account's group and readable by it alone.
pub const MANIFEST_NAME: &str = "manifest";
pub const MANIFEST_MODE: u32 = 0o640;
const MANIFEST_MAGIC: &str = "td-grant-views-v1\n";
pub const MAX_MANIFEST_BYTES: usize = 64 * 1024;
const MAX_PATH_BYTES: usize = 1024;
const MAX_PATH_DEPTH: usize = 16;
const MAX_NAME_BYTES: usize = 255;

/// A directory grant's access. Every view is noexec, nosuid and nodev.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Access {
    ReadOnly,
    ReadWrite,
}

impl Access {
    fn parse(text: &str) -> Option<Self> {
        match text {
            "ro" => Some(Self::ReadOnly),
            "rw" => Some(Self::ReadWrite),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "ro",
            Self::ReadWrite => "rw",
        }
    }
}

/// What a row grants. Only directories are published; v1 parses the socket
/// and network forms, which no v1 writer adds, so the deployment that
/// starts writing them (WORKLOADS.md step 7) can roll back to this one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Grant {
    Directory { access: Access, path: String },
    Socket(String),
    Network(Network),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Network {
    Off,
    Host,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub kind: Kind,
    pub name: String,
    pub grant: Grant,
}

impl Row {
    fn line(&self) -> String {
        let account = format!("{}\t{}", self.kind.as_str(), self.name);
        match &self.grant {
            Grant::Directory { access, path } => {
                format!("{account}\tdirectory\t{}\t{path}\n", access.as_str())
            }
            Grant::Socket(socket) => format!("{account}\tsocket\t{socket}\n"),
            Grant::Network(Network::Off) => format!("{account}\tnetwork\toff\n"),
            Grant::Network(Network::Host) => format!("{account}\tnetwork\thost\n"),
        }
    }

    fn parse(line: &str) -> Result<Self, String> {
        let fields: Vec<&str> = line.split('\t').take(6).collect();
        let (kind, name, grant) = match fields.as_slice() {
            [kind, name, "directory", access, path] => {
                let access = Access::parse(access).ok_or("unknown grant access")?;
                validate_path(path.as_bytes())?;
                (
                    kind,
                    name,
                    Grant::Directory {
                        access,
                        path: (*path).to_string(),
                    },
                )
            }
            [kind, name, "socket", socket] => {
                if !socket_name(socket) {
                    return Err("invalid socket grant name".into());
                }
                (kind, name, Grant::Socket((*socket).to_string()))
            }
            [kind, name, "network", "off"] => (kind, name, Grant::Network(Network::Off)),
            [kind, name, "network", "host"] => (kind, name, Grant::Network(Network::Host)),
            _ => return Err("invalid grant row shape".into()),
        };
        let kind = Kind::parse(kind).ok_or("unknown grant account kind")?;
        if !valid_name(name) {
            return Err("invalid grant account name".into());
        }
        Ok(Self {
            kind,
            name: (*name).to_string(),
            grant,
        })
    }
}

/// A socket a later deployment may grant: a lowercase name of at most 32
/// bytes.
fn socket_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 32
        && value.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

/// One directory grant of an account.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Directory {
    pub access: Access,
    pub path: String,
}

/// The grant table, canonical: a `td-grants-v1` line, then rows strictly
/// ascending by their bytes, which orders them by kind, name and grant.
/// Grant data, not authority: only the root-owned file td-authd reads
/// grants anything.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Table {
    rows: Vec<Row>,
}

impl Table {
    /// The table a first boot publishes: Firefox's read-write `Downloads`.
    /// A boot that finds a table publishes nothing, so a revoked row stays
    /// revoked.
    pub fn seeded() -> Self {
        Self {
            rows: vec![Row {
                kind: Kind::Application,
                name: "firefox".into(),
                grant: Grant::Directory {
                    access: Access::ReadWrite,
                    path: "Downloads".into(),
                },
            }],
        }
    }

    pub fn parse(text: &str) -> Result<Self, String> {
        if text.len() > MAX_BYTES || !text.ends_with('\n') {
            return Err("grant table is oversized or not newline-terminated".into());
        }
        let lines = text
            .strip_prefix(MAGIC)
            .ok_or("unsupported grant table format")?;
        let mut rows: Vec<Row> = Vec::new();
        let mut previous: Option<&str> = None;
        for (index, line) in lines.split_inclusive('\n').enumerate() {
            if index >= MAX_ROWS {
                return Err(format!("grant table exceeds {MAX_ROWS} rows"));
            }
            if previous.is_some_and(|previous| previous >= line) {
                return Err("grant table rows are duplicate or unsorted".into());
            }
            let row = Row::parse(line.strip_suffix('\n').unwrap_or(line))?;
            require_account_rows(&rows, &row)?;
            rows.push(row);
            previous = Some(line);
        }
        Ok(Self { rows })
    }

    pub fn encode(&self) -> String {
        let mut text = MAGIC.to_string();
        for row in &self.rows {
            text.push_str(&row.line());
        }
        text
    }

    /// The directory grants of the `kind` account `name`.
    pub fn directories(&self, kind: Kind, name: &str) -> Vec<Directory> {
        self.rows
            .iter()
            .filter(|row| row.kind == kind && row.name == name)
            .filter_map(|row| match &row.grant {
                Grant::Directory { access, path } => Some(Directory {
                    access: *access,
                    path: path.clone(),
                }),
                _ => None,
            })
            .collect()
    }
}

/// `row` beside the rows of its account already admitted: within the
/// account's bounds, at most one network row, and no directory that is,
/// contains or lies in another of its directories, whatever their access.
fn require_account_rows(admitted: &[Row], row: &Row) -> Result<(), String> {
    let same = || {
        admitted
            .iter()
            .filter(|other| other.kind == row.kind && other.name == row.name)
    };
    match &row.grant {
        Grant::Directory { path, .. } => {
            let mut count = 1;
            for other in same() {
                if let Grant::Directory { path: other, .. } = &other.grant {
                    count += 1;
                    if nested(path, other) {
                        return Err(format!(
                            "grant directories {other:?} and {path:?} of one account overlap"
                        ));
                    }
                }
            }
            if count > MAX_DIRECTORIES {
                return Err(format!(
                    "an account holds more than {MAX_DIRECTORIES} directory grants"
                ));
            }
        }
        Grant::Socket(_) => {
            if same()
                .filter(|other| matches!(other.grant, Grant::Socket(_)))
                .count()
                >= MAX_SOCKETS
            {
                return Err(format!(
                    "an account holds more than {MAX_SOCKETS} socket grants"
                ));
            }
        }
        Grant::Network(_) => {
            if same().any(|other| matches!(other.grant, Grant::Network(_))) {
                return Err("an account holds more than one network grant".into());
            }
        }
    }
    Ok(())
}

/// Whether two valid paths below the home are the same directory or one
/// lies in the other. They are compared as bytes, which `validate_path`
/// makes exact: no empty, `.` or `..` name and no trailing `/`.
fn nested(left: &str, right: &str) -> bool {
    let below = |path: &str, root: &str| {
        path == root
            || path
                .strip_prefix(root)
                .is_some_and(|rest| rest.starts_with('/'))
    };
    below(left, right) || below(right, left)
}

/// td's own trees of the human's home, which no grant may name, nor any
/// directory containing one: td's state below `.td`, and the trees the
/// jail's private home mounts over or keeps (td-jail refuses a grant target
/// there): the caches, `.config` whole (td's own configuration and what the
/// session or git runs on the human's behalf among it), `.local/share`
/// (td-sandbox's homes, keyrings and desktop entries among it) and
/// `.local/state`. Every top-level `.td-*` name is td's too (`.td-update`'s
/// publisher keys, `.td-agent`, `.td-rust`, `.td-build-cache`).
const TD_RESERVED: &[&str] = &[".td", ".cache", ".config", ".local/share", ".local/state"];
const TD_PREFIX: &str = ".td-";

/// Below the home: credentials, keyrings, browser and mail profiles and
/// sandboxed applications' data. No grant names one, and td-agent admits no
/// workspace or share reaching one; this is the one list both read.
pub const HOME_SENSITIVE: &[&str] = &[
    ".aws",
    ".azure",
    ".cargo/credentials",
    ".cargo/credentials.toml",
    ".docker",
    ".git-credentials",
    ".gnupg",
    ".kube",
    ".librewolf",
    ".mozilla",
    ".netrc",
    ".npmrc",
    ".password-store",
    ".pypirc",
    ".ssh",
    ".thunderbird",
    ".var/app",
    "snap",
];
/// Below each configuration home: credentials, browser profiles, and what
/// the session starts or git runs on the human's behalf. A grant reaches
/// none, since `.config` is td's whole.
pub const CONFIG_SENSITIVE: &[&str] = &[
    "BraveSoftware",
    "autostart",
    "chromium",
    "environment.d",
    "gcloud",
    "gh",
    "git",
    "google-chrome",
    "microsoft-edge",
    "opera",
    "systemd",
    "td-agent",
    "vivaldi",
];
/// Below each data home: keyrings, td-pass's vault, desktop entries, and
/// td-agent's own stores and workspace repositories. A grant reaches none,
/// since `.local/share` is td's whole.
pub const DATA_SENSITIVE: &[&str] = &["applications", "keyrings", "td-agent", "td-pass"];
/// Below the home: where the human's own programs are put. A writable
/// grant there would run the workload's code as the human, and a read-only
/// one serves nothing a workload needs, so no grant names one.
pub const EXECUTED: &[&str] = &[".cargo/bin", ".local/bin", "bin"];

/// Unicode format controls that reorder or hide what a path shows, refused
/// so the path a consent shows is the one granted: the bidirectional
/// embeddings, overrides, isolates and marks, the byte-order mark and the
/// line and paragraph separators. The replacement character too: td-authd
/// reads mountinfo lossily, so a source renamed to a name that is not
/// UTF-8 must never show the root of a row that names U+FFFD.
fn deceptive(character: char) -> bool {
    character.is_control()
        || matches!(
            character,
            '\u{061C}'
                | '\u{200B}'..='\u{200F}'
                | '\u{2028}'..='\u{202E}'
                | '\u{2060}'..='\u{2069}'
                | '\u{FEFF}'
                | '\u{FFFD}'
        )
}

/// A directory below the human's home, as a grant spells it: one to 16
/// names of at most 255 bytes joined by single `/`, none empty, `.` or
/// `..`, without NUL or any other control or deceptive format character,
/// 1024 bytes at most, and neither at, below nor above a tree of td's
/// (`TD_RESERVED`, a top-level `.td-*`) or a sensitive one
/// (`HOME_SENSITIVE`, `EXECUTED`). It reads bytes, not `Path` components,
/// which normalise `a//b`, `a/./b` and a trailing `/` away.
pub fn validate_path(relative: &[u8]) -> Result<(), String> {
    let refuse = |why: &str| {
        Err(format!(
            "human directory {:?} {why}",
            String::from_utf8_lossy(relative)
        ))
    };
    if relative.is_empty() || relative.len() > MAX_PATH_BYTES {
        return refuse("is empty or too long");
    }
    let mut depth = 0_usize;
    for name in relative.split(|byte| *byte == b'/') {
        depth = depth.saturating_add(1);
        if name.is_empty() || name == b"." || name == b".." {
            return refuse("is not a plain path below the home");
        }
        if name.len() > MAX_NAME_BYTES
            || name
                .iter()
                .any(|byte| byte.is_ascii_control() || *byte == 0x7f)
        {
            return refuse("holds an invalid name");
        }
    }
    if depth > MAX_PATH_DEPTH {
        return refuse("is too deep");
    }
    let Ok(text) = std::str::from_utf8(relative) else {
        return refuse("is not UTF-8");
    };
    if text.chars().any(deceptive) {
        return refuse("holds a control or format character");
    }
    if TD_RESERVED
        .iter()
        .chain(HOME_SENSITIVE)
        .chain(EXECUTED)
        .any(|reserved| nested(text, reserved))
        || text.starts_with(TD_PREFIX)
    {
        return refuse("is or contains a reserved tree of the home");
    }
    Ok(())
}

/// One published view: its index below the account's view directory, its
/// access and the directory below the human's home it shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct View {
    pub index: usize,
    pub access: Access,
    pub path: String,
}

/// The views td-authd published for one account's launch, in a
/// `td-grant-views-v1` file: at most `MAX_DIRECTORIES`
/// `INDEX<TAB>ACCESS<TAB>PATH` rows strictly ascending by index, each below
/// `MAX_VIEWS`, their directories valid and apart.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Manifest {
    views: Vec<View>,
}

impl Manifest {
    pub fn new(mut views: Vec<View>) -> Result<Self, String> {
        views.sort_by_key(|view| view.index);
        let mut manifest = Self::default();
        for view in views {
            manifest.admit(view)?;
        }
        Ok(manifest)
    }

    fn admit(&mut self, view: View) -> Result<(), String> {
        validate_path(view.path.as_bytes())?;
        if view.index >= MAX_VIEWS
            || self.views.len() >= MAX_DIRECTORIES
            || self
                .views
                .last()
                .is_some_and(|last| last.index >= view.index)
        {
            return Err("grant view indices are out of range, duplicate or unsorted".into());
        }
        if self
            .views
            .iter()
            .any(|other| nested(&other.path, &view.path))
        {
            return Err(format!(
                "grant views of {:?} overlap another view",
                view.path
            ));
        }
        self.views.push(view);
        Ok(())
    }

    pub fn parse(text: &str) -> Result<Self, String> {
        if text.len() > MAX_MANIFEST_BYTES || !text.ends_with('\n') {
            return Err("grant view manifest is oversized or not newline-terminated".into());
        }
        let lines = text
            .strip_prefix(MANIFEST_MAGIC)
            .ok_or("unsupported grant view manifest format")?;
        let mut manifest = Self::default();
        for line in lines.split_terminator('\n') {
            let fields: Vec<&str> = line.split('\t').take(4).collect();
            let [index, access, path] = fields.as_slice() else {
                return Err("invalid grant view row shape".into());
            };
            let number = index
                .parse::<usize>()
                .ok()
                .filter(|number| number.to_string() == *index)
                .ok_or("noncanonical grant view index")?;
            manifest.admit(View {
                index: number,
                access: Access::parse(access).ok_or("unknown grant view access")?,
                path: (*path).to_string(),
            })?;
        }
        Ok(manifest)
    }

    pub fn encode(&self) -> String {
        let mut text = MANIFEST_MAGIC.to_string();
        for view in &self.views {
            text.push_str(&format!(
                "{}\t{}\t{}\n",
                view.index,
                view.access.as_str(),
                view.path
            ));
        }
        text
    }

    pub fn views(&self) -> &[View] {
        &self.views
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    const SEEDED: &str = "td-grants-v1\napplication\tfirefox\tdirectory\trw\tDownloads\n";

    fn directory(access: Access, path: &str) -> Directory {
        Directory {
            access,
            path: path.into(),
        }
    }

    #[test]
    fn the_seed_is_firefox_downloads_read_write_and_round_trips() {
        let table = Table::seeded();
        assert_eq!(table.encode(), SEEDED);
        assert_eq!(Table::parse(SEEDED).unwrap(), table);
        assert_eq!(
            table.directories(Kind::Application, "firefox"),
            [directory(Access::ReadWrite, "Downloads")]
        );
        assert!(table.directories(Kind::Application, "mail").is_empty());
        assert!(table.directories(Kind::Sandbox, "firefox").is_empty());
        assert_eq!(Table::parse(MAGIC).unwrap(), Table::default());
    }

    #[test]
    fn later_kinds_and_the_reserved_forms_parse_and_round_trip() {
        let text = "td-grants-v1\n\
            application\tfirefox\tdirectory\tro\tDocuments/taxes\n\
            application\tfirefox\tdirectory\trw\tDownloads\n\
            application\tfirefox\tnetwork\thost\n\
            application\tfirefox\tsocket\twayland\n\
            sandbox\tdefault\tdirectory\trw\tsrc/td\n\
            sandbox\tdefault\tnetwork\toff\n\
            sandbox\tdefault\tsocket\tfetch\n\
            sandbox\tdefault\tsocket\tssh-agent\n\
            workspace\tagent-1\tdirectory\tro\tT\u{e9}l\u{e9}chargements\n";
        let table = Table::parse(text).unwrap();
        assert_eq!(table.encode(), text);
        assert_eq!(
            table.directories(Kind::Application, "firefox"),
            [
                directory(Access::ReadOnly, "Documents/taxes"),
                directory(Access::ReadWrite, "Downloads")
            ]
        );
        assert_eq!(
            table.directories(Kind::Sandbox, "default"),
            [directory(Access::ReadWrite, "src/td")]
        );
    }

    #[test]
    fn malformed_unsorted_and_unknown_rows_are_refused() {
        for text in [
            "",
            "td-grants-v2\n",
            "td-grants-v1",
            "td-grants-v1\n\n",
            "td-grants-v1\napplication\tfirefox\tdirectory\trw\tDownloads",
            "td-grants-v1\napplication\tfirefox\tdirectory\trw\tDownloads\napplication\tfirefox\tdirectory\trw\tDownloads\n",
            "td-grants-v1\napplication\tmail\tdirectory\trw\tDownloads\napplication\tfirefox\tdirectory\trw\tDownloads\n",
            "td-grants-v1\nservice\tfirefox\tdirectory\trw\tDownloads\n",
            "td-grants-v1\napplication\tFirefox\tdirectory\trw\tDownloads\n",
            "td-grants-v1\napplication\tfirefox\tdirectory\trwx\tDownloads\n",
            "td-grants-v1\napplication\tfirefox\tdirectory\texec\tDownloads\n",
            "td-grants-v1\napplication\tfirefox\tdirectory\trw\n",
            "td-grants-v1\napplication\tfirefox\tdirectory\trw\tDownloads\textra\n",
            "td-grants-v1\napplication\tfirefox\tfile\trw\tDownloads\n",
            "td-grants-v1\napplication\tfirefox\tnetwork\tsome\n",
            "td-grants-v1\napplication\tfirefox\tsocket\tWayland\n",
            "td-grants-v1\napplication\tfirefox\tsocket\t\n",
            "td-grants-v1\napplication\tfirefox\tnetwork\thost\napplication\tfirefox\tnetwork\toff\n",
            "td-grants-v1\napplication\tfirefox\tdirectory\trw\t.ssh\n",
            "td-grants-v1\napplication\tfirefox\tdirectory\trw\t/Downloads\n",
            "td-grants-v1\napplication\tfirefox\tdirectory\trw\tDownloads/\n",
        ] {
            assert!(Table::parse(text).is_err(), "{text:?}");
        }
    }

    /// One account's directories may not overlap, whatever their access;
    /// two accounts may hold the same one.
    #[test]
    fn overlapping_directories_of_one_account_are_refused() {
        for (first, second) in [
            ("ro\tDownloads", "rw\tDownloads"),
            ("rw\tDownloads", "rw\tDownloads/a"),
            ("ro\tDownloads/a", "rw\tDownloads"),
            ("ro\ta", "ro\ta/b/c"),
        ] {
            let mut rows = [
                format!("application\tfirefox\tdirectory\t{first}\n"),
                format!("application\tfirefox\tdirectory\t{second}\n"),
            ];
            rows.sort();
            let text = format!("{MAGIC}{}", rows.concat());
            assert!(Table::parse(&text).is_err(), "{text:?}");
        }
        for (first, second) in [("Down", "Downloads"), ("a/b", "a/bc"), ("ab", "a")] {
            let mut rows = [
                format!("application\tfirefox\tdirectory\trw\t{first}\n"),
                format!("application\tfirefox\tdirectory\trw\t{second}\n"),
            ];
            rows.sort();
            Table::parse(&format!("{MAGIC}{}", rows.concat())).unwrap();
        }
        Table::parse(&format!(
            "{MAGIC}application\tfirefox\tdirectory\trw\tDownloads\napplication\tmail\tdirectory\tro\tDownloads\nsandbox\tfirefox\tdirectory\trw\tDownloads\n"
        ))
        .unwrap();
    }

    #[test]
    fn rows_and_bytes_are_bounded() {
        let names = |count: usize| {
            (0..count)
                .map(|index| format!("application\tfirefox\tdirectory\trw\td{index:03}\n"))
                .collect::<Vec<_>>()
        };
        Table::parse(&format!("{MAGIC}{}", names(MAX_DIRECTORIES).concat())).unwrap();
        assert!(Table::parse(&format!("{MAGIC}{}", names(MAX_DIRECTORIES + 1).concat())).is_err());
        let sockets = |count: usize| {
            (0..count)
                .map(|index| format!("application\tfirefox\tsocket\ts{index:03}\n"))
                .collect::<String>()
        };
        Table::parse(&format!("{MAGIC}{}", sockets(MAX_SOCKETS))).unwrap();
        assert!(Table::parse(&format!("{MAGIC}{}", sockets(MAX_SOCKETS + 1))).is_err());
        // Many accounts: the row bound, then the byte bound.
        let accounts = |count: usize, path: &str| {
            (0..count)
                .map(|index| format!("workspace\tw{index:05}\tdirectory\trw\t{path}\n"))
                .collect::<String>()
        };
        Table::parse(&format!("{MAGIC}{}", accounts(MAX_ROWS, "d"))).unwrap();
        assert!(Table::parse(&format!("{MAGIC}{}", accounts(MAX_ROWS + 1, "d"))).is_err());
        let long = vec!["n".repeat(200); 5].join("/");
        let wide = format!("{MAGIC}{}", accounts(1100, &long));
        assert!(wide.len() > MAX_BYTES);
        assert!(Table::parse(&wide).is_err());
    }

    #[test]
    fn the_path_validator_reads_bytes_and_refuses_every_irregular_path() {
        for relative in [
            "",
            "/Downloads",
            "../Downloads",
            "Downloads/../.td",
            "./Downloads",
            "Downloads/",
            "Downloads//x",
            "Downloads/.",
            "a/./b",
            "a//b",
            "a\0b",
            "a\tb",
            "a\nb",
            "a\u{7f}b",
        ] {
            assert!(validate_path(relative.as_bytes()).is_err(), "{relative:?}");
        }
        assert!(validate_path(b"a\xffb").is_err());
        // Bounds: a name of 255 bytes, 16 names and 1024 bytes are the most.
        let name = |length: usize| "n".repeat(length);
        let longest = name(MAX_NAME_BYTES);
        let deepest = vec!["d"; MAX_PATH_DEPTH].join("/");
        let fullest =
            |last: usize| format!("{longest}/{longest}/{longest}/{}/{}", name(200), name(last));
        assert_eq!(fullest(55).len(), MAX_PATH_BYTES);
        for admitted in [
            longest.clone(),
            deepest.clone(),
            fullest(55),
            "Downloads".into(),
            "Music/new".into(),
            "a b".into(),
            "T\u{e9}l\u{e9}chargements".into(),
        ] {
            validate_path(admitted.as_bytes()).unwrap();
        }
        for refused in [
            name(MAX_NAME_BYTES + 1),
            format!("{deepest}/d"),
            fullest(56),
        ] {
            validate_path(refused.as_bytes()).unwrap_err();
        }
    }

    /// td's state, SSH keys, the trees the jail's private home mounts, td's
    /// own configuration and every directory containing one of them.
    #[test]
    fn reserved_trees_and_their_ancestors_are_refused() {
        for refused in [
            ".td",
            ".td/app/mail",
            ".ssh",
            ".ssh/id_ed25519.d",
            ".cache",
            ".cache/x",
            ".local",
            ".local/share",
            ".local/share/td-sandbox",
            ".local/share/td-sandbox/default",
            ".local/share/fonts",
            ".local/state",
            ".config",
            ".config/td-mail",
            ".config/td-",
            ".config/td-news/x",
            ".config/git",
            ".config/foo",
            ".config/autostart",
            ".td-update",
            ".td-update/keys",
            ".td-agent",
            ".td-rust",
            ".td-build-cache",
            ".td-",
            ".local/bin",
            ".cargo",
            ".cargo/bin",
            ".cargo/credentials.toml",
            "bin",
            "bin/x",
            ".gnupg",
            ".password-store/web",
            ".mozilla",
            ".var",
            ".var/app/org.example.App",
            "snap",
        ] {
            assert!(validate_path(refused.as_bytes()).is_err(), "{refused}");
        }
        for admitted in [
            ".tdx",
            "x/.td",
            "x/.td-update",
            ".sshd",
            ".cargo/registry",
            ".var/lib",
            "binaries",
            "src/bin",
            "src/.ssh",
            "Downloads",
            "Music/new",
        ] {
            validate_path(admitted.as_bytes()).unwrap();
        }
        // Every list td-agent shares is reached by the validator.
        for sensitive in HOME_SENSITIVE.iter().chain(EXECUTED) {
            assert!(validate_path(sensitive.as_bytes()).is_err(), "{sensitive}");
        }
        for config in CONFIG_SENSITIVE {
            assert!(validate_path(format!(".config/{config}").as_bytes()).is_err());
        }
        for data in DATA_SENSITIVE {
            assert!(validate_path(format!(".local/share/{data}").as_bytes()).is_err());
        }
    }

    /// A path a consent shows is the path granted: no character reorders
    /// or hides what is shown.
    #[test]
    fn deceptive_format_characters_are_refused() {
        for character in [
            '\u{061C}', '\u{200B}', '\u{200E}', '\u{200F}', '\u{202A}', '\u{202E}', '\u{2066}',
            '\u{2069}', '\u{FEFF}', '\u{2028}', '\u{0085}', '\u{FFFD}',
        ] {
            let path = format!("Down{character}loads");
            assert!(validate_path(path.as_bytes()).is_err(), "{character:?}");
        }
        validate_path("Téléchargements".as_bytes()).unwrap();
        validate_path("下载".as_bytes()).unwrap();
    }

    #[test]
    fn the_manifest_lists_valid_apart_views_by_index() {
        let views = vec![
            View {
                index: 3,
                access: Access::ReadOnly,
                path: "Documents".into(),
            },
            View {
                index: 0,
                access: Access::ReadWrite,
                path: "Downloads".into(),
            },
        ];
        let manifest = Manifest::new(views).unwrap();
        let text = "td-grant-views-v1\n0\trw\tDownloads\n3\tro\tDocuments\n";
        assert_eq!(manifest.encode(), text);
        assert_eq!(Manifest::parse(text).unwrap(), manifest);
        assert_eq!(manifest.views().len(), 2);
        assert_eq!(
            Manifest::parse(MANIFEST_MAGIC).unwrap(),
            Manifest::default()
        );
        for refused in [
            "td-grant-views-v1\n0\trw\tDownloads\n0\tro\tDocuments\n",
            "td-grant-views-v1\n1\trw\tDownloads\n0\tro\tDocuments\n",
            "td-grant-views-v1\n00\trw\tDownloads\n",
            "td-grant-views-v1\n64\trw\tDownloads\n",
            "td-grant-views-v1\n0\trw\tDownloads\n1\tro\tDownloads/a\n",
            "td-grant-views-v1\n0\trwx\tDownloads\n",
            "td-grant-views-v1\n0\trw\t.ssh\n",
            "td-grant-views-v1\n0\trw\tDownloads\textra\n",
            "td-grant-views-v1\n0\trw\tDownloads",
            "td-grant-views-v2\n",
        ] {
            assert!(Manifest::parse(refused).is_err(), "{refused:?}");
        }
        let oversized = format!(
            "{MANIFEST_MAGIC}0\trw\t{}\n",
            "a".repeat(MAX_MANIFEST_BYTES)
        );
        assert!(Manifest::parse(&oversized).is_err());
        // Up to MAX_VIEWS indices, a replacement beside the view it
        // replaces, but never more views than an account holds rows.
        Manifest::parse("td-grant-views-v1\n63\trw\tDownloads\n").unwrap();
        let views = |count: usize| {
            (0..count)
                .map(|index| View {
                    index: index * 2,
                    access: Access::ReadOnly,
                    path: format!("d{index}"),
                })
                .collect::<Vec<_>>()
        };
        Manifest::new(views(MAX_DIRECTORIES)).unwrap();
        assert!(Manifest::new(views(MAX_DIRECTORIES + 1)).is_err());
    }
}
