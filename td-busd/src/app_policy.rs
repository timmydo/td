//! Immutable deployment grants joined with the workload account ledger: the
//! image declares each application, the account it runs as and the names it
//! may own, and an application with an account of its own takes that
//! account's uid from `/var/lib/td/accounts.tsv` (td-authd/DESIGN.md,
//! "Workload account ledger"). The wire never allocates an application
//! identity. Every includer also declares `crate::workload_accounts`.

use crate::workload_accounts::{self, Kind, Ledger, Stamp};
use crate::workload_accounts::{O_DIRECTORY, O_NOFOLLOW, O_NONBLOCK};
use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

pub const PORTAL_UID: u32 = 991;

pub const PATH: &str = "/etc/td-bus-applications.tsv";
pub const MAX_BYTES: usize = 64 * 1024;
const MAX_APPS: usize = 256;
const MAX_OWNED: usize = 32;
const HEADER: &str = "td-bus-applications-v2";

/// How a loaded policy treats a ledger it cannot read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Admission {
    /// The load fails: a launcher, a credential binding or a check.
    Strict,
    /// A long-running session service starts with no application rows,
    /// says so on stderr, and reads the ledger again once it changes, so
    /// the human's own connections survive a damaged ledger while every
    /// application stays refused.
    Session,
}

/// One declared application at the uid it runs as.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rule {
    pub uid: u32,
    pub application: String,
    pub owned: Vec<String>,
}

/// The account a declared application runs as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    /// The session's human (td's mail and news).
    Personal,
    /// Its own account: the ledger's application row of its name.
    Service,
}

impl Class {
    fn parse(text: &str) -> Option<Self> {
        match text {
            "personal" => Some(Self::Personal),
            "service" => Some(Self::Service),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Personal => "personal",
            Self::Service => "service",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Declaration {
    pub application: String,
    pub class: Class,
    pub owned: Vec<String>,
}

/// The image's `/etc/td-bus-applications.tsv`: a header naming the session
/// owner, then one `NAME<TAB>CLASS<TAB>OWNED` row per application, sorted by
/// name. It carries no uid.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Declarations {
    owner: u32,
    rows: Vec<Declaration>,
}

impl Declarations {
    pub fn new(owner: u32, mut rows: Vec<Declaration>) -> Result<Self, String> {
        if !(1000..=65533).contains(&owner) || rows.len() > MAX_APPS {
            return Err("invalid application policy owner or row count".into());
        }
        let mut names = BTreeSet::new();
        for row in &mut rows {
            if !application_name(&row.application)
                || !names.insert(row.application.clone())
                || row.owned.len() > MAX_OWNED
                || row.owned.iter().any(|name| !owned_grant(name))
            {
                return Err("invalid or duplicate application policy entry".into());
            }
            row.owned.sort();
            if row.owned.windows(2).any(|pair| pair.first() == pair.get(1)) {
                return Err("duplicate application bus grant".into());
            }
        }
        rows.sort_by(|left, right| left.application.cmp(&right.application));
        let declarations = Self { owner, rows };
        if declarations.to_tsv().len() > MAX_BYTES {
            return Err("application policy exceeds 64 KiB".into());
        }
        Ok(declarations)
    }

    pub fn parse(text: &str) -> Result<Self, String> {
        if text.len() > MAX_BYTES || !text.is_ascii() || !text.ends_with('\n') {
            return Err("application policy must be bounded canonical ASCII lines".into());
        }
        let mut lines = text.split_terminator('\n');
        let (header, owner) = lines
            .next()
            .and_then(|line| line.split_once('\t'))
            .ok_or("missing application policy header")?;
        if header != HEADER {
            return Err("unsupported application policy format".into());
        }
        let owner = decimal(owner)?;
        let mut rows = Vec::new();
        for line in lines {
            if rows.len() == MAX_APPS {
                return Err("too many application policy rows".into());
            }
            let fields: Vec<&str> = line.split('\t').take(4).collect();
            let [application, class, owned] = fields.as_slice() else {
                return Err("invalid application policy row".into());
            };
            let owned = if owned.is_empty() {
                Vec::new()
            } else {
                owned
                    .split(',')
                    .take(MAX_OWNED + 1)
                    .map(str::to_string)
                    .collect()
            };
            rows.push(Declaration {
                application: (*application).into(),
                class: Class::parse(class).ok_or("unknown application policy class")?,
                owned,
            });
        }
        let declarations = Self::new(owner, rows)?;
        if declarations.to_tsv() != text {
            return Err("application policy is not canonical".into());
        }
        Ok(declarations)
    }

    pub fn to_tsv(&self) -> String {
        let mut text = format!("{HEADER}\t{}\n", self.owner);
        for row in &self.rows {
            text.push_str(&format!(
                "{}\t{}\t{}\n",
                row.application,
                row.class.as_str(),
                row.owned.join(",")
            ));
        }
        text
    }

    pub fn owner(&self) -> u32 {
        self.owner
    }

    pub fn rows(&self) -> &[Declaration] {
        &self.rows
    }

    fn row(&self, name: &str) -> Option<&Declaration> {
        self.rows.iter().find(|row| row.application == name)
    }
}

/// The ledger copy a policy resolves against, and the stamp of the file
/// it last read or tried to read (`None`: there was none to inspect).
struct Accounts {
    ledger: Ledger,
    seen: Option<Stamp>,
    /// Ledger reads after the first, for the tests that hold a miss to one.
    #[cfg(test)]
    reads: usize,
}

/// Where a live policy re-reads the ledger.
struct Source {
    directory: PathBuf,
    owner: (u32, u32),
}

/// The declarations joined with the ledger's application rows. A uid comes
/// only from the ledger; a personal application runs as the session owner.
///
/// Ledger rows are appended and never reassigned, so a copy read earlier is
/// a correct subset of the published ledger. A loaded policy therefore keeps
/// its copy and, when a lookup misses (a uid in the application range, or a
/// declared application with no row), inspects the ledger and reads it again
/// only when it changed since the last read: publication renames a new file
/// into place. It adopts the new copy only when it extends the old. No
/// timer: a ledger published just after any miss is seen by the next one.
pub struct Policy {
    declarations: Declarations,
    accounts: Mutex<Accounts>,
    source: Option<Source>,
}

impl Policy {
    /// The declarations joined with `ledger`, which this policy never
    /// re-reads.
    pub fn join(declarations: Declarations, ledger: Ledger) -> Self {
        Self {
            declarations,
            accounts: Mutex::new(Accounts {
                ledger,
                seen: None,
                #[cfg(test)]
                reads: 0,
            }),
            source: None,
        }
    }

    fn live(
        declarations: Declarations,
        directory: &Path,
        owner: (u32, u32),
        admission: Admission,
    ) -> Result<Self, String> {
        let (ledger, seen) = match workload_accounts::load_stamped_from(directory, owner) {
            Ok((ledger, stamp)) => (ledger, Some(stamp)),
            Err(error) if admission == Admission::Session => {
                eprintln!(
                    "td application policy: {error}; admitting no application until the account ledger changes"
                );
                (
                    Ledger::default(),
                    workload_accounts::stamp_from(directory, owner).ok(),
                )
            }
            Err(error) => return Err(error),
        };
        Ok(Self {
            declarations,
            accounts: Mutex::new(Accounts {
                ledger,
                seen,
                #[cfg(test)]
                reads: 0,
            }),
            source: Some(Source {
                directory: directory.to_path_buf(),
                owner,
            }),
        })
    }

    #[cfg(test)]
    pub fn from_tables(declarations: &str, ledger: &str) -> Result<Self, String> {
        Ok(Self::join(
            Declarations::parse(declarations)?,
            Ledger::parse(ledger)?,
        ))
    }

    /// How many times a miss has read the ledger since the load.
    #[cfg(test)]
    pub fn ledger_reads(&self) -> usize {
        self.accounts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .reads
    }

    pub fn owner(&self) -> u32 {
        self.declarations.owner
    }

    pub fn declarations(&self) -> &Declarations {
        &self.declarations
    }

    /// The service application whose ledger account is `uid`. The session
    /// owner's uid names no one application.
    pub fn for_uid(&self, uid: u32) -> Option<Rule> {
        if uid == self.owner() {
            return None;
        }
        let name = self.resolve(
            |ledger| ledger.name(Kind::Application, uid).map(str::to_string),
            workload_accounts::APPLICATION_UIDS.contains(&uid),
        )?;
        let row = self
            .declarations
            .row(&name)
            .filter(|row| row.class == Class::Service)?;
        Some(Rule {
            uid,
            application: name,
            owned: row.owned.clone(),
        })
    }

    /// The declared application `name` at the uid it runs as.
    pub fn for_name(&self, name: &str) -> Option<Rule> {
        let row = self.declarations.row(name)?;
        let uid = match row.class {
            Class::Personal => self.owner(),
            Class::Service => self.account(name)?,
        };
        Some(Rule {
            uid,
            application: name.into(),
            owned: row.owned.clone(),
        })
    }

    /// The ledger account of the declared application `name`, whatever its
    /// class: the uid its private home, cgroup and credential records name.
    pub fn account(&self, name: &str) -> Option<u32> {
        self.declarations.row(name)?;
        self.resolve(|ledger| ledger.uid_of(Kind::Application, name), true)
    }

    /// `find` on the ledger copy; a miss may re-read the published ledger.
    fn resolve<T>(&self, find: impl Fn(&Ledger) -> Option<T>, reread: bool) -> Option<T> {
        let mut accounts = self.accounts.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(found) = find(&accounts.ledger) {
            return Some(found);
        }
        let source = self.source.as_ref().filter(|_| reread)?;
        // Read again only what changed since the last read or attempt, so a
        // stream of misses costs one inspection each and no read.
        let now = workload_accounts::stamp_from(&source.directory, source.owner).ok();
        if now == accounts.seen {
            return None;
        }
        accounts.seen = now;
        #[cfg(test)]
        {
            accounts.reads += 1;
        }
        // A ledger that fails to read, or that dropped or moved a row, is
        // not adopted; the lookup misses until the file changes again.
        let fresh = match workload_accounts::load_stamped_from(&source.directory, source.owner) {
            Ok((fresh, stamp)) => {
                accounts.seen = Some(stamp);
                fresh
            }
            Err(error) => {
                eprintln!(
                    "td application policy: keeping the account ledger read earlier: {error}"
                );
                return None;
            }
        };
        if !fresh.extends(&accounts.ledger) {
            eprintln!(
                "td application policy: the account ledger dropped or moved a row; keeping the copy read earlier"
            );
            return None;
        }
        accounts.ledger = fresh;
        find(&accounts.ledger)
    }

    pub fn admits(&self, uid: u32) -> bool {
        uid == 0 || uid == self.owner() || self.is_portal(uid) || self.for_uid(uid).is_some()
    }

    pub fn is_portal(&self, uid: u32) -> bool {
        self.owner() == 1000 && uid == PORTAL_UID
    }

    pub fn registration(
        &self,
        uid: u32,
        app: &str,
        services: &[String],
        owned: &[String],
    ) -> Result<(), String> {
        let rule = self
            .for_name(app)
            .ok_or("application is absent from deployment policy")?;
        // Only the assigned kernel identity may register the installed name.
        if uid != rule.uid {
            return Err("kernel UID does not own this application identity".into());
        }
        if !services.is_empty() || owned != rule.owned {
            return Err("registration differs from the immutable application grants".into());
        }
        Ok(())
    }
}

/// The image's declarations joined with the published ledger, failing on
/// either: a launcher's, a credential binding's or a check's. The image
/// owns the declarations and their parent; root owns the ledger.
pub fn load() -> Result<Policy, String> {
    load_live(Admission::Strict)
}

/// `load` for a long-running session service (the broker, the display, the
/// portal, audio): a ledger it cannot read admits no application until it
/// changes, rather than stopping the service the human's session needs.
/// The declarations stay strict.
pub fn load_session() -> Result<Policy, String> {
    load_live(Admission::Session)
}

fn load_live(admission: Admission) -> Result<Policy, String> {
    let directory = Path::new(PATH).parent().ok_or("bus policy has no parent")?;
    load_from(
        directory,
        Path::new(workload_accounts::DIRECTORY),
        (0, 0),
        admission,
    )
}

/// `load` from `etc` and the ledger's `state` directory, both `owner`'s.
pub fn load_from(
    etc: &Path,
    state: &Path,
    owner: (u32, u32),
    admission: Admission,
) -> Result<Policy, String> {
    let declarations = load_declarations_from(etc, owner)?;
    if declarations.owner() != 1000 {
        return Err("bus application policy names another session".into());
    }
    Policy::live(declarations, state, owner, admission)
}

/// The declarations in `directory`, which `owner` must control: the live
/// image's, or a staged tree's for the image build's check.
pub fn load_declarations_from(directory: &Path, owner: (u32, u32)) -> Result<Declarations, String> {
    use std::io::Read;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    let etc = fs::OpenOptions::new()
        .read(true)
        .custom_flags(O_NOFOLLOW | O_DIRECTORY)
        .open(directory)
        .map_err(|error| format!("open bus policy directory: {error}"))?;
    let metadata = etc.metadata().map_err(|error| error.to_string())?;
    if !metadata.is_dir()
        || metadata.uid() != owner.0
        || metadata.gid() != owner.1
        || metadata.mode() & 0o7022 != 0
    {
        return Err("bus policy directory is not root-controlled".into());
    }
    let name = Path::new(PATH)
        .file_name()
        .ok_or("bus policy has no filename")?;
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(O_NOFOLLOW | O_NONBLOCK)
        .open(Path::new(&format!("/proc/self/fd/{}", etc.as_raw_fd())).join(name))
        .map_err(|error| format!("open bus application policy: {error}"))?;
    let metadata = file.metadata().map_err(|error| error.to_string())?;
    if !metadata.is_file()
        || metadata.uid() != owner.0
        || metadata.gid() != owner.1
        || metadata.mode() & 0o7777 != 0o444
        || metadata.nlink() != 1
        || metadata.len() > MAX_BYTES as u64
    {
        return Err("bus application policy must be one bounded root-owned 0444 file".into());
    }
    let mut text = String::new();
    file.take(MAX_BYTES as u64 + 1)
        .read_to_string(&mut text)
        .map_err(|error| format!("read bus application policy: {error}"))?;
    Declarations::parse(&text)
}

fn decimal(text: &str) -> Result<u32, String> {
    let value = text.parse::<u32>().map_err(|_| "invalid policy UID")?;
    if value.to_string() != text {
        return Err("noncanonical policy UID".into());
    }
    Ok(value)
}
pub(crate) fn application_name(name: &str) -> bool {
    // Stock names must satisfy both the reserved principal and jail grammars.
    name.len() <= 32
        && !name.contains("..")
        && name.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        && name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"._-".contains(&byte)
        })
}

/// The reserved names and namespace roots no grant may reach.
const RESERVED_ROOTS: &[&str] = &[
    "org.freedesktop.DBus",
    "org.freedesktop.portal",
    "org.freedesktop.impl.portal",
];

/// The fewest components a suffix grant's base may have, so that no grant
/// spans a vendor's whole namespace.
const MIN_SUFFIX_BASE_COMPONENTS: usize = 3;

/// One `own` grant as the deployment records it: an exact well-known name,
/// or `BASE.*` for the names one or more components below `BASE`.
pub(crate) fn owned_grant(grant: &str) -> bool {
    match grant.strip_suffix(".*") {
        Some(base) => {
            owned_name(base)
                && base.split('.').count() >= MIN_SUFFIX_BASE_COMPONENTS
                && !RESERVED_ROOTS.iter().any(|root| overlaps(base, root))
        }
        None => owned_name(grant),
    }
}

/// Whether `base` equals `root` or lies inside or above it, by components.
fn overlaps(base: &str, root: &str) -> bool {
    let under = |name: &str, prefix: &str| {
        name.strip_prefix(prefix)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('.'))
    };
    under(base, root) || under(root, base)
}

/// Whether `grant` admits the well-known `name`: the name itself, or for
/// `BASE.*` a name under `BASE` but never `BASE`.
pub(crate) fn grant_covers(grant: &str, name: &str) -> bool {
    match grant.strip_suffix(".*") {
        Some(base) => name
            .strip_prefix(base)
            .and_then(|rest| rest.strip_prefix('.'))
            .is_some_and(|rest| !rest.is_empty()),
        None => grant == name,
    }
}

// This source is also compiled by recipes; broker tests cross-check its
// reserved-name decisions against policy::is_reserved_name.
pub(crate) fn owned_name(name: &str) -> bool {
    name.len() <= 255
        && name.contains('.')
        && !RESERVED_ROOTS.contains(&name)
        && !name.starts_with("org.freedesktop.portal.")
        && !name.starts_with("org.freedesktop.impl.portal.")
        && name.split('.').all(|part| {
            part.as_bytes()
                .first()
                .is_some_and(|byte| byte.is_ascii_alphabetic() || b"_-".contains(byte))
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
        })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    const DECLARED: &str =
        "td-bus-applications-v2\t1000\nfirefox\tservice\torg.mozilla.firefox\nmail\tservice\t\n";
    const LEDGER: &str = "td-accounts-v1\napplication\tfirefox\t65536\napplication\tmail\t65537\n";

    fn policy(declared: &str) -> Policy {
        Policy::from_tables(declared, LEDGER).unwrap()
    }

    #[test]
    fn registration_uses_canonical_lists_and_shared_grants_remain_explicit() {
        let policy = Policy::from_tables(
            "td-bus-applications-v2\t1000\none\tservice\torg.example.A,org.example.B\ntwo\tservice\torg.example.A\n",
            "td-accounts-v1\napplication\tone\t65536\napplication\ttwo\t65537\n",
        )
        .unwrap();
        let sorted = vec!["org.example.A".into(), "org.example.B".into()];
        policy.registration(65536, "one", &[], &sorted).unwrap();
        let reversed = vec!["org.example.B".into(), "org.example.A".into()];
        assert!(policy.registration(65536, "one", &[], &reversed).is_err());
        policy
            .registration(65537, "two", &[], &["org.example.A".into()])
            .unwrap();
    }

    #[test]
    fn personal_applications_share_the_human_uid_but_keep_distinct_registered_names() {
        let policy = policy(
            "td-bus-applications-v2\t1000\nfirefox\tservice\t\nmail\tpersonal\t\nnews\tpersonal\t\n",
        );
        assert!(policy.for_uid(1000).is_none());
        // A personal application's ledger row is its account, not the uid
        // it runs as.
        assert!(policy.for_uid(65537).is_none());
        assert_eq!(policy.account("mail"), Some(65537));
        assert_eq!(policy.account("news"), None);
        for app in ["mail", "news"] {
            policy.registration(1000, app, &[], &[]).unwrap();
            assert!(policy.registration(65537, app, &[], &[]).is_err());
        }
        assert!(policy.registration(1000, "firefox", &[], &[]).is_err());
        assert!(policy.registration(1000, "absent", &[], &[]).is_err());
    }

    #[test]
    fn canonical_policy_binds_kernel_identity_and_exact_grants() {
        let policy = policy(DECLARED);
        assert_eq!(policy.declarations().to_tsv(), DECLARED);
        let grants = vec!["org.mozilla.firefox".into()];
        policy.registration(65536, "firefox", &[], &grants).unwrap();
        for uid in [0, 991, 992, 1000, 1001, 65537, u32::MAX] {
            assert!(policy.registration(uid, "firefox", &[], &grants).is_err());
        }
        assert!(policy.registration(65536, "mail", &[], &[]).is_err());
        assert!(policy.registration(1000, "absent", &[], &[]).is_err());
        assert!(policy.registration(65536, "firefox", &[], &[]).is_err());
        assert!(policy
            .registration(65536, "firefox", &grants, &grants)
            .is_err());
        for uid in [0, 991, 1000, 65536, 65537] {
            assert!(policy.admits(uid));
        }
        for uid in [992, 993, 1001, 65534, 65538] {
            assert!(!policy.admits(uid));
        }
    }

    /// The uid comes from the ledger alone: a declaration carries none, an
    /// application without a row has no account, and a row the image does
    /// not declare (a removed application's) admits nothing.
    #[test]
    fn an_application_uid_comes_only_from_the_ledger() {
        let policy = Policy::from_tables(
            DECLARED,
            "td-accounts-v1\napplication\tclaude\t65539\napplication\tfirefox\t65536\n",
        )
        .unwrap();
        assert_eq!(policy.for_name("firefox").unwrap().uid, 65536);
        assert!(policy.for_name("mail").is_none());
        assert!(policy.for_uid(65537).is_none());
        assert_eq!(policy.account("mail"), None);
        assert!(policy.for_uid(65539).is_none());
        assert!(policy.for_name("claude").is_none());
        assert!(!policy.admits(65539));
        assert!(policy.registration(65537, "mail", &[], &[]).is_err());
        // The same declarations under another ledger take its uids.
        let moved = Policy::from_tables(
            DECLARED,
            "td-accounts-v1\napplication\tfirefox\t70000\napplication\tmail\t65537\n",
        )
        .unwrap();
        assert_eq!(moved.for_uid(70000).unwrap().application, "firefox");
        assert!(moved.for_uid(65536).is_none());
        // Only application rows are application accounts.
        let workspace = Policy::from_tables(
            DECLARED,
            "td-accounts-v1\napplication\tmail\t65537\nworkspace\tfirefox\t98304\n",
        )
        .unwrap();
        assert!(workspace.for_name("firefox").is_none());
        assert!(workspace.for_uid(98304).is_none());
    }

    #[test]
    fn a_policy_row_carries_a_narrow_suffix_grant_and_registers_it_exactly() {
        let text = "td-bus-applications-v2\t1000\nfirefox\tservice\torg.mozilla.firefox,org.mozilla.firefox.*\n";
        let policy = policy(text);
        assert_eq!(policy.declarations().to_tsv(), text);
        let grants = vec!["org.mozilla.firefox".into(), "org.mozilla.firefox.*".into()];
        policy.registration(65536, "firefox", &[], &grants).unwrap();
        assert!(policy
            .registration(65536, "firefox", &[], &["org.mozilla.firefox".into()])
            .is_err());
        for wide in ["org.mozilla.*", "org.freedesktop.impl.*"] {
            let text = text.replace("org.mozilla.firefox.*", wide);
            assert!(Declarations::parse(&text).is_err(), "{wide}");
        }
        assert!(grant_covers(
            "org.mozilla.firefox.*",
            "org.mozilla.firefox.ZGVmYXVsdC1yZWxlYXNl"
        ));
        assert!(!grant_covers(
            "org.mozilla.firefox.*",
            "org.mozilla.firefox"
        ));
        assert!(!grant_covers(
            "org.mozilla.firefox",
            "org.mozilla.firefox.A"
        ));
    }

    #[test]
    fn the_fixed_portal_belongs_only_to_the_stock_session() {
        let policy = Policy::join(
            Declarations::new(1001, Vec::new()).unwrap(),
            Ledger::default(),
        );
        assert!(!policy.admits(PORTAL_UID));
        assert!(!policy.is_portal(PORTAL_UID));
    }

    #[test]
    fn policy_refuses_aliases_reserved_names_and_noncanonical_encodings() {
        for text in [
            DECLARED.replace("mail", "firefox"),
            DECLARED.replace("firefox", "app..name"),
            DECLARED.replace("firefox", &"a".repeat(33)),
            DECLARED.replace("firefox", "App"),
            DECLARED.replace("firefox", "1app"),
            DECLARED.replace("firefox\t", "../firefox\t"),
            DECLARED.replace("1000", "01000"),
            DECLARED.replace("1000", "999"),
            DECLARED.replace("\tservice\t", "\tService\t"),
            DECLARED.replace("mail\tservice", "mail\t1000"),
            DECLARED.replace("mail\tservice\t", "mail\t"),
            DECLARED.replace("mail\tservice\t", "65537\tmail\tservice\t"),
            DECLARED.replacen("v2", "v1", 1),
            DECLARED.replace('\n', "\r\n"),
            DECLARED.replace("org.mozilla.firefox", "org.freedesktop.portal.Desktop"),
            DECLARED.replace("org.mozilla.firefox", "org.mozilla.*"),
            DECLARED.replace(
                "org.mozilla.firefox",
                "org.mozilla.firefox,org.mozilla.firefox",
            ),
            DECLARED.replace("org.mozilla.firefox", "org.zzz,org.aaa"),
            DECLARED.replace(
                "firefox\tservice\torg.mozilla.firefox\nmail",
                "mail\tservice\t\nfirefox\tservice\torg.mozilla.firefox\nzzz",
            ),
            DECLARED.trim_end().into(),
            format!("{DECLARED}\n"),
        ] {
            assert!(Declarations::parse(&text).is_err(), "{text:?}");
        }
        assert!(Declarations::parse(&"x".repeat(MAX_BYTES + 1)).is_err());
        // The image's old uid-bearing table is refused, not misread.
        assert!(Declarations::parse(
            "td-bus-applications-v1\t1000\n65536\tfirefox\torg.mozilla.firefox\n"
        )
        .is_err());
    }
}
