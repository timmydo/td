//! The account database: `/etc/passwd` and `/etc/group`. There is no
//! `/etc/shadow`: td keeps no secret in its accounts, only the marker
//! `classify` reads, so nothing about them needs a root-only file.
//!
//! Parsing is fail-closed (THREAT-MODEL.md §7). A malformed line fails the whole
//! lookup instead of being skipped — skipping is how a crafted entry shadows a
//! real one — a duplicate user name is an error rather than a first-wins or
//! last-wins guess, and a uid two names claim resolves to neither: a `Secret`
//! is a property of a name, while the uid is what the kernel enforces. Signed
//! deployment tables may be projected read-only for the validated saved username
//! before consumers start. Readers still validate the live files; see
//! THREAT-MODEL.md and td-install/INSTALLER.md for admission and publication.
//!
//! An application account is not here at all: `tda<uid>` names the workload
//! account ledger's application row holding that uid, and no passwd or group
//! entry may take that form or an id in a workload account range, so neither
//! file can speak for an account of the other class (THREAT-MODEL.md §7).

pub use crate::workload_accounts::application_account_name as application_name;
use crate::workload_accounts::{self, Kind, Ledger, LEDGER_UIDS};
use std::fs;

pub const PASSWD: &str = "/etc/passwd";
pub const GROUP: &str = "/etc/group";

/// Refuse an entry that would make passwd or group speak for a workload
/// account: the reserved name form, or an id the ledger allocates.
fn refuse_workload_entry(
    path: &str,
    lineno: usize,
    name: &str,
    ids: [u32; 2],
) -> Result<(), String> {
    if application_name(name) || ids.iter().any(|id| LEDGER_UIDS.contains(id)) {
        return Err(format!(
            "{path}:{lineno}: {name:?} takes a workload account's name or id; \
             workload accounts come from the account ledger alone"
        ));
    }
    Ok(())
}

/// One `/etc/passwd` entry. `gecos` is carried because `login` has no use for it
/// and dropping it would make the seven-field check look like six.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    pub gecos: String,
    pub home: String,
    pub shell: String,
}

/// What `/etc/passwd`'s password field says about an account. The classification
/// IS the authentication policy; see `may_start_session`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Secret {
    /// Empty field: no password is required.
    NoPassword,
    /// `!`, `!!`, `*`, `x`, or any `!`-prefixed hash — administratively locked.
    Locked,
    /// Exact td image marker: locked for every human session and available
    /// only to the service-specific credential path.
    Service,
    /// A hash this build cannot verify. Denied, never treated as absent.
    Hashed,
}

/// Why a session was refused, so the caller can say so precisely. A single
/// "denied" would make an operator debug the wrong thing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Denied {
    /// The account is locked; even `login -f` will not revive it.
    Locked,
    /// The account has a password and this build verifies no hash scheme.
    NeedsPassword,
    /// The account is a service identity, not a human session.
    ServiceOnly,
    /// The service path was pointed at an interactive or ordinarily locked
    /// account rather than an explicitly marked service identity.
    NotService,
}

/// The whole authentication decision, in one place.
///
/// `forced` is `login -f` / `exec-as`: the CALLER has already
/// established the right to start this session, so no secret is consulted —
/// except that a locked account stays locked (THREAT-MODEL.md §3).
pub fn may_start_session(secret: Secret, forced: bool) -> Result<(), Denied> {
    match secret {
        Secret::Locked => Err(Denied::Locked),
        Secret::Service => Err(Denied::ServiceOnly),
        Secret::NoPassword => Ok(()),
        Secret::Hashed if forced => Ok(()),
        Secret::Hashed => Err(Denied::NeedsPassword),
    }
}

/// Admit only the exact service-account class.
///
/// This is intentionally not a `forced` branch above. `login -f` and ordinary
/// `exec-as` must keep refusing a service account, while a unit must
/// not use the service path to enter an interactive account.
pub fn may_start_service(secret: Secret) -> Result<(), Denied> {
    match secret {
        Secret::Service => Ok(()),
        Secret::Locked | Secret::NoPassword | Secret::Hashed => Err(Denied::NotService),
    }
}

pub fn classify(field: &str) -> Secret {
    if field.is_empty() {
        return Secret::NoPassword;
    }
    if field == "!td-service" {
        return Secret::Service;
    }
    // `*` and anything `!`-prefixed are the two spellings shadow-utils, busybox
    // and util-linux all agree mean "cannot authenticate". `!` in front of an
    // otherwise valid hash is how `passwd -l` locks an account without losing it.
    // `x` points at a shadow file td does not have, so it says nothing about the
    // account and is not read as a hash the forced paths would admit.
    if field == "*" || field == "x" || field.starts_with('!') {
        return Secret::Locked;
    }
    Secret::Hashed
}

/// Read a database file. A missing file is an error, not an empty database:
/// "no /etc/passwd" must not read as "no account has a password".
fn read(path: &str) -> Result<String, String> {
    fs::read_to_string(path).map_err(|e| format!("cannot read {path}: {e}"))
}

/// Split one colon-separated line into exactly `want` fields, or fail. Trailing
/// empty fields are real fields (`root:x:0:0::/root:/bin/sh` has two), so this
/// counts separators rather than trimming.
fn fields<'a>(
    line: &'a str,
    want: usize,
    path: &str,
    lineno: usize,
) -> Result<Vec<&'a str>, String> {
    let got: Vec<&str> = line.split(':').collect();
    if got.len() != want {
        return Err(format!(
            "{path}:{lineno}: expected {want} colon-separated fields, got {}",
            got.len()
        ));
    }
    Ok(got)
}

/// Every non-empty line of a database file, numbered from 1. Blank lines are the
/// only thing skipped — a trailing newline yields one, and it carries no entry.
fn entries(text: &str) -> impl Iterator<Item = (usize, &str)> {
    text.lines()
        .enumerate()
        .map(|(i, line)| (i + 1, line))
        .filter(|(_, line)| !line.trim().is_empty())
}

fn number(path: &str, lineno: usize, what: &str, token: &str) -> Result<u32, String> {
    token
        .parse::<u32>()
        .map_err(|_| format!("{path}:{lineno}: {what} {token:?} is not an unsigned 32-bit id"))
}

/// Look one account up. Every line is parsed even after a hit: a malformed or
/// duplicate entry LATER in the file is exactly the tampering this refuses to
/// resolve, and stopping early would never see it. "Duplicate" means either
/// column — a repeated name, or a uid a second name also claims.
pub fn account_in(text: &str, path: &str, want: &str) -> Result<Account, String> {
    let mut hit: Option<Account> = None;
    // Every OTHER entry's uid, so an alias can be refused below. `Secret` is a
    // property of a NAME; the uid is what the kernel enforces. A second name
    // for one uid is a second account policy for ONE kernel identity — its own
    // password field, its own group list — and whichever name a caller happened
    // to ask for would decide which policy applied.
    let mut aliases: Vec<(u32, usize, &str)> = Vec::new();
    for (lineno, line) in entries(text) {
        let f = fields(line, 7, path, lineno)?;
        let (Some(name), Some(uid), Some(gid), Some(gecos), Some(home), Some(shell)) =
            (f.first(), f.get(2), f.get(3), f.get(4), f.get(5), f.get(6))
        else {
            return Err(format!("{path}:{lineno}: malformed entry"));
        };
        // `?`, not a silent skip, on every line. A 7-field line whose ids do
        // not parse was the one malformation this function tolerated:
        // `fields` has always failed the whole lookup for a wrong field COUNT
        // on any line, so a passwd file that cannot be read is already refused
        // rather than partly believed. Tolerating it would also mean an
        // unreadable line could hide an alias or a workload id, which are the
        // cases being closed. Pinned by `a_malformed_line_fails_the_whole_lookup`.
        let uid = number(path, lineno, "uid", uid)?;
        let gid = number(path, lineno, "gid", gid)?;
        refuse_workload_entry(path, lineno, name, [uid, gid])?;
        if *name != want {
            aliases.push((uid, lineno, name));
            continue;
        }
        if hit.is_some() {
            return Err(format!(
                "{path}:{lineno}: user {want:?} appears more than once; refusing to \
                 choose between conflicting entries"
            ));
        }
        // `login` execs the shell by absolute path with no PATH search, and
        // chdir's to the home; a relative one would resolve against whatever
        // directory the caller happened to be in.
        for (what, value) in [("home", *home), ("shell", *shell)] {
            if !value.starts_with('/') {
                return Err(format!(
                    "{path}:{lineno}: {what} {value:?} is not an absolute path"
                ));
            }
        }
        hit = Some(Account {
            name: (*name).to_string(),
            uid,
            gid,
            gecos: (*gecos).to_string(),
            home: (*home).to_string(),
            shell: (*shell).to_string(),
        });
    }
    let account = hit.ok_or_else(|| format!("{path}: no such user {want:?}"))?;
    if let Some((_, lineno, other)) = aliases.iter().find(|(uid, _, _)| *uid == account.uid) {
        return Err(format!(
            "{path}:{lineno}: uid {} is claimed by {other:?} as well as {want:?}; \
             refusing to resolve an identity two names can speak for",
            account.uid
        ));
    }
    Ok(account)
}

pub fn account(name: &str) -> Result<Account, String> {
    account_in(&read(PASSWD)?, PASSWD, name)
}

/// The application account `name` (`tda<uid>`), resolved from the ledger
/// alone: its application row holds that uid. Its group is its uid, its home
/// the private one firstboot prepares, its shell `/bin/false`; the class is
/// the service class. passwd and group are read whole and must speak for no
/// workload account, so no entry of either aliases this one.
pub fn application_account_in(
    ledger: &Ledger,
    passwd: &str,
    group: &str,
    name: &str,
) -> Result<Account, String> {
    let uid = workload_accounts::application_account_uid(name)
        .ok_or_else(|| format!("{name:?} does not spell an application account's uid"))?;
    ledger
        .name(Kind::Application, uid)
        .ok_or_else(|| format!("the account ledger holds no application account {uid}"))?;
    for (lineno, line) in entries(passwd) {
        let f = fields(line, 7, PASSWD, lineno)?;
        let (Some(entry), Some(id), Some(gid)) = (f.first(), f.get(2), f.get(3)) else {
            return Err(format!("{PASSWD}:{lineno}: malformed entry"));
        };
        let ids = [
            number(PASSWD, lineno, "uid", id)?,
            number(PASSWD, lineno, "gid", gid)?,
        ];
        refuse_workload_entry(PASSWD, lineno, entry, ids)?;
    }
    // Refuses a workload group or member, so the account has no
    // supplementary group.
    supplementary_in(group, GROUP, name)?;
    Ok(Account {
        name: name.to_string(),
        uid,
        gid: uid,
        gecos: "td application".into(),
        home: format!("/var/lib/td/applications/{uid}"),
        shell: "/bin/false".into(),
    })
}

pub fn application_account(name: &str) -> Result<Account, String> {
    application_account_in(
        &workload_accounts::load()?,
        &read(PASSWD)?,
        &read(GROUP)?,
        name,
    )
}

/// The gids of every group that lists `user` as a member. The primary gid is NOT
/// added here — `Credentials::new` does that, so there is one place that decides
/// what the complete set is. A group named or numbered as a workload account,
/// or listing one as a member, fails the whole lookup: a workload account
/// holds only its primary group.
pub fn supplementary_in(text: &str, path: &str, user: &str) -> Result<Vec<u32>, String> {
    let mut gids = Vec::new();
    for (lineno, line) in entries(text) {
        let f = fields(line, 4, path, lineno)?;
        let (Some(name), Some(gid), Some(members)) = (f.first(), f.get(2), f.get(3)) else {
            return Err(format!("{path}:{lineno}: malformed entry"));
        };
        // Parsed for EVERY line, member or not: a group file with an unparseable
        // gid is a broken file, and answering from the lines that happened to
        // parse would hand out a set that is quietly too small.
        let gid = number(path, lineno, "gid", gid)?;
        refuse_workload_entry(path, lineno, name, [gid, gid])?;
        if let Some(member) = members.split(',').find(|member| application_name(member)) {
            return Err(format!(
                "{path}:{lineno}: group {name:?} lists the application account {member:?}"
            ));
        }
        // EXACT, not trimmed: trimming is the permissive direction — it would grant
        // a group on a whitespace-padded member field — and every other name match
        // in this file is exact. The empty check keeps an empty member slot in
        // `a,,b` from matching an empty user name.
        let member = members.split(',').any(|m| !m.is_empty() && m == user);
        if member {
            gids.push(gid);
        }
    }
    gids.sort_unstable();
    gids.dedup();
    Ok(gids)
}

pub fn supplementary(user: &str) -> Result<Vec<u32>, String> {
    supplementary_in(&read(GROUP)?, GROUP, user)
}

/// `passwd(5)`: name, password, uid, gid, gecos, home, shell.
const PASSWD_FIELDS: usize = 7;

/// The password field of one account. A missing entry is an error.
pub fn secret_in(text: &str, path: &str, user: &str) -> Result<Secret, String> {
    let mut hit: Option<Secret> = None;
    for (lineno, line) in entries(text) {
        // The whole record, not just the two fields that carry the decision.
        // Accepting a SHORT line is how corruption becomes authentication: a
        // truncated `root:` has an empty password field, and an empty password
        // field means NoPassword — so losing the tail of a LOCKED account's
        // entry would turn it into one anybody may log into.
        let f = fields(line, PASSWD_FIELDS, path, lineno)?;
        let (Some(name), Some(field)) = (f.first(), f.get(1)) else {
            return Err(format!("{path}:{lineno}: expected at least name:password"));
        };
        if *name != user {
            continue;
        }
        if hit.is_some() {
            return Err(format!(
                "{path}:{lineno}: user {user:?} appears more than once; refusing to \
                 choose between conflicting entries"
            ));
        }
        hit = Some(classify(field));
    }
    hit.ok_or_else(|| format!("{path}: no entry for user {user:?}"))
}

pub fn secret(user: &str) -> Result<Secret, String> {
    secret_in(&read(PASSWD)?, PASSWD, user)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing)]
    use super::*;

    /// What `system-x86-64` generates today, verbatim in shape.
    const PW: &str = "root:!:0:0:root:/root:/bin/sh\n\
                      tester::1000:1000:Test User:/home/tester:/bin/sh\n";
    const GR: &str = "root:x:0:\ntester:x:1000:\nwheel:x:10:tester\ntty:x:5:\n";

    #[test]
    fn the_shipped_database_resolves() {
        let tester = account_in(PW, PASSWD, "tester").unwrap();
        assert_eq!(tester.uid, 1000);
        assert_eq!(tester.gid, 1000);
        assert_eq!(tester.home, "/home/tester");
        assert_eq!(tester.shell, "/bin/sh");
        assert_eq!(account_in(PW, PASSWD, "root").unwrap().uid, 0);
        assert_eq!(supplementary_in(GR, GROUP, "tester").unwrap(), vec![10]);
        assert_eq!(
            supplementary_in(GR, GROUP, "root").unwrap(),
            Vec::<u32>::new()
        );
        assert_eq!(secret_in(PW, PASSWD, "tester").unwrap(), Secret::NoPassword);
        assert_eq!(secret_in(PW, PASSWD, "root").unwrap(), Secret::Locked);
    }

    /// A TRUNCATED record must not read as "no password". This is the
    /// direction corruption runs in: the password field is the SECOND of seven,
    /// so a line that loses its tail keeps a name and an empty secret — and an
    /// empty secret is the one class interactive `login` lets through. A locked
    /// account whose entry got shortened would become one anybody may log into.
    #[test]
    fn a_truncated_record_is_refused_rather_than_read_as_passwordless() {
        for truncated in [
            "tester:",
            "tester::",
            "tester::1000",
            "tester::1000:1000:Test User:/home/tester",
            "tester:!:1000:1000:Test User:/home/tester:/bin/sh:extra",
        ] {
            let text = format!("root:!:0:0:root:/root:/bin/sh\n{truncated}\n");
            let err = secret_in(&text, PASSWD, "tester").unwrap_err();
            assert!(
                err.contains("colon-separated fields"),
                "{truncated:?} must be refused as malformed, got: {err}"
            );
        }
        // The refusal is for the whole LOOKUP, not just the bad line: a
        // malformed entry anywhere means this is not a file to authenticate
        // against, whoever is being looked up.
        let text = format!("{PW}broken:\n");
        assert!(secret_in(&text, PASSWD, "root").is_err());
    }

    #[test]
    fn an_unknown_user_is_an_error_not_an_empty_account() {
        assert!(account_in(PW, PASSWD, "nobody").is_err());
        assert!(secret_in(PW, PASSWD, "nobody").is_err());
        // ...but an unknown user simply belongs to no groups.
        assert_eq!(
            supplementary_in(GR, GROUP, "nobody").unwrap(),
            Vec::<u32>::new()
        );
    }

    /// The duplicate-entry refusal, both lookups. "First wins" and "last wins" are
    /// each a defensible convention and each one an attacker's choice of which
    /// entry to append.
    #[test]
    fn a_duplicate_user_is_refused_rather_than_resolved() {
        let doubled = format!("{PW}tester:x:0:0::/root:/bin/sh\n");
        let err = account_in(&doubled, PASSWD, "tester").unwrap_err();
        assert!(err.contains("more than once"), "{err}");
        assert!(secret_in(&doubled, PASSWD, "tester").is_err());
        // A duplicate of a DIFFERENT user is still not this user's problem.
        let other = format!("{PW}root:x:0:0::/root:/bin/sh\n");
        assert!(account_in(&other, PASSWD, "tester").is_ok());
        assert!(account_in(&other, PASSWD, "root").is_err());
    }

    /// A uid two names can speak for resolves to neither.
    ///
    /// `Secret` is a property of a NAME — `classify` reads one password field —
    /// while the uid is what the kernel enforces. So an alias is not a second
    /// way to spell one account; it is a second account policy for one kernel
    /// identity, and the caller picks which one applies by picking a name.
    /// Both names must fail: refusing only the alias would leave "which name
    /// did you ask for" load-bearing, which is the thing being removed.
    #[test]
    fn a_uid_claimed_by_two_names_resolves_to_neither() {
        let aliased = format!("{PW}alias:x:1000:1000:Alias:/home/alias:/bin/sh\n");
        for name in ["tester", "alias"] {
            let err = account_in(&aliased, PASSWD, name).unwrap_err();
            assert!(err.contains("two names can speak for"), "{name}: {err}");
        }
        // A uid claimed ONCE still resolves: this refuses sharing, not the entry.
        assert!(account_in(PW, PASSWD, "tester").is_ok());
        // And a shared GID is not a shared identity — `login` and `exec-as`
        // switch to a uid, and two accounts in one group is ordinary.
        let shared_gid = format!("{PW}other:x:1001:1000:Other:/home/other:/bin/sh\n");
        assert!(account_in(&shared_gid, PASSWD, "tester").is_ok());
    }

    /// The service class alone does not decide who may be the daemon's uid.
    ///
    /// `!td-service` is refused by `login`, `login -f` and ordinary
    /// `exec-as`, which is what makes `exec-service-as` the only door. But the
    /// class hangs on the NAME `audio`. An alias sharing 994 carries its own
    /// password field, and an empty one is `NoPassword`, which every forced
    /// caller may start — so without the refusal above, ordinary `exec-as`
    /// walks straight past the class to the uid it protects.
    #[test]
    fn the_service_class_is_reached_by_a_uid_no_other_name_claims() {
        let text = "root:!:0:0:root:/root:/bin/sh\n\
                    audio:!td-service:994:994:System Audio:/run/td-audio:/bin/false\n";
        // The class does its own job: no forced path admits it.
        assert_eq!(
            may_start_session(classify("!td-service"), true),
            Err(Denied::ServiceOnly)
        );
        assert!(account_in(text, PASSWD, "audio").is_ok());

        // Now alias the uid with an account the class never sees.
        let aliased = format!("{text}evil::994:994:Evil:/run/td-audio:/bin/sh\n");
        assert_eq!(may_start_session(classify(""), true), Ok(()));
        for name in ["audio", "evil"] {
            assert!(
                account_in(&aliased, PASSWD, name).is_err(),
                "{name} resolved, so the service uid is reachable without the class"
            );
        }
    }

    /// A malformed line fails the lookup even when the wanted entry parsed
    /// cleanly, and even when it sits AFTER the hit. Skipping it is how a
    /// truncated line hides a second entry on the same physical line.
    #[test]
    fn a_malformed_line_fails_the_whole_lookup() {
        let short = format!("{PW}broken:x:2\n");
        let err = account_in(&short, PASSWD, "tester").unwrap_err();
        assert!(err.contains("expected 7"), "{err}");
        assert!(account_in("root:x:0:0:r:/root:/bin/sh:extra\n", PASSWD, "root").is_err());
        assert!(supplementary_in("wheel:x:10\n", GROUP, "tester").is_err());
        assert!(supplementary_in("wheel:x:ten:tester\n", GROUP, "tester").is_err());
        assert!(account_in("root:x:zero:0:r:/root:/bin/sh\n", PASSWD, "root").is_err());
        // ...including on a line the lookup does not want. A wrong field COUNT
        // on an unrelated line already failed here; a 7-field line with an
        // unparseable uid is the same corruption one column over, and a file
        // this function cannot read whole is one it refuses to answer from.
        let unrelated = format!("{PW}broken:x:zero:0:B:/home/b:/bin/sh\n");
        let err = account_in(&unrelated, PASSWD, "tester").unwrap_err();
        assert!(err.contains("not an unsigned 32-bit id"), "{err}");
        // Blank lines are the one thing skipped: a trailing newline makes one.
        assert!(account_in(&format!("\n{PW}\n"), PASSWD, "root").is_ok());
    }

    #[test]
    fn a_relative_home_or_shell_is_refused() {
        assert!(account_in("u:x:1:1:u:home:/bin/sh\n", PASSWD, "u").is_err());
        assert!(account_in("u:x:1:1:u:/home/u:sh\n", PASSWD, "u").is_err());
        assert!(account_in("u:x:1:1:u:/home/u:/bin/sh\n", PASSWD, "u").is_ok());
    }

    /// Group membership is exact, not substring. `test` must not inherit
    /// `tester`'s groups, and an empty member slot must not match an empty name.
    #[test]
    fn group_membership_matches_whole_names_only() {
        let g = "wheel:x:10:tester,root\nops:x:20:test\nempty:x:30:,\n";
        assert_eq!(supplementary_in(g, GROUP, "tester").unwrap(), vec![10]);
        assert_eq!(supplementary_in(g, GROUP, "test").unwrap(), vec![20]);
        assert_eq!(supplementary_in(g, GROUP, "root").unwrap(), vec![10]);
        assert_eq!(supplementary_in(g, GROUP, "").unwrap(), Vec::<u32>::new());
    }

    const LEDGER: &str = "td-accounts-v1\napplication\tfirefox\t65536\napplication\tweb\t65540\nworkspace\tagent\t98304\n";

    fn ledger() -> Ledger {
        Ledger::parse(LEDGER).unwrap()
    }

    /// `tda<uid>` is the ledger's application row holding that uid, with
    /// its fixed group, home and shell, and no supplementary group.
    #[test]
    fn an_application_account_comes_from_the_ledger_alone() {
        let account = application_account_in(&ledger(), PW, GR, "tda65540").unwrap();
        assert_eq!(
            account,
            Account {
                name: "tda65540".into(),
                uid: 65540,
                gid: 65540,
                gecos: "td application".into(),
                home: "/var/lib/td/applications/65540".into(),
                shell: "/bin/false".into(),
            }
        );
        assert_eq!(
            supplementary_in(GR, GROUP, "tda65540").unwrap(),
            Vec::<u32>::new()
        );
        // A uid the ledger does not hold, or holds for another kind, and a
        // spelling that is not the uid's canonical one.
        for name in [
            "tda65537",
            "tda98304",
            "tda065536",
            "tda4294967296",
            "tda",
            "tdafirefox",
            "firefox",
        ] {
            assert!(
                application_account_in(&ledger(), PW, GR, name).is_err(),
                "{name}"
            );
        }
        assert!(application_account_in(&Ledger::default(), PW, GR, "tda65536").is_err());
        // The reserved form is `tda` and digits; other `tda` names are names.
        assert!(application_name("tda1"));
        for name in ["tda", "tdadam", "tda1x", "xtda1", "TDA1"] {
            assert!(!application_name(name), "{name}");
        }
        let tdadam = "tdadam::1000:1000:Adam:/home/tdadam:/bin/sh\n";
        assert_eq!(account_in(tdadam, PASSWD, "tdadam").unwrap().uid, 1000);
    }

    /// Neither file may speak for a workload account, whatever is looked up:
    /// a passwd entry of the reserved form or with a workload id would be a
    /// second account policy for a uid the ledger allocates (an empty
    /// password field the forced paths admit, its own group list), and a
    /// group would hand a workload account authority beyond its own group.
    #[test]
    fn no_passwd_or_group_entry_speaks_for_a_workload_account() {
        for entry in [
            "tda65536:!td-service:65536:65536:td application:/var/lib/td/applications/65536:/bin/false",
            "tda1:!:1:1::/:/bin/false",
            "evil::65536:65536:Evil:/home/evil:/bin/sh",
            "evil::1001:65540:Evil:/home/evil:/bin/sh",
            "evil::98304:1001:Evil:/home/evil:/bin/sh",
            "evil::131071:1001:Evil:/home/evil:/bin/sh",
        ] {
            let passwd = format!("{PW}{entry}\n");
            for name in ["tester", "root", "evil"] {
                assert!(account_in(&passwd, PASSWD, name).is_err(), "{entry}: {name}");
            }
            assert!(
                application_account_in(&ledger(), &passwd, GR, "tda65536").is_err(),
                "{entry}"
            );
        }
        for group in [
            "tda65536:x:65536:",
            "apps:x:70000:",
            "audio:x:994:tda65536",
            "audio:x:994:tester,tda1",
        ] {
            let text = format!("{GR}{group}\n");
            assert!(supplementary_in(&text, GROUP, "tester").is_err(), "{group}");
            assert!(
                application_account_in(&ledger(), PW, &text, "tda65536").is_err(),
                "{group}"
            );
        }
        // The ranges are exact: the ids beside them are ordinary.
        for id in [65534, 65535, 131072] {
            let passwd = format!("{PW}other::{id}:{id}:Other:/home/other:/bin/sh\n");
            assert_eq!(account_in(&passwd, PASSWD, "other").unwrap().uid, id);
            let group = format!("{GR}other:x:{id}:tester\n");
            assert!(supplementary_in(&group, GROUP, "tester")
                .unwrap()
                .contains(&id));
        }
        // A malformed id anywhere still fails the lookup.
        let broken = format!("{PW}broken:x:1001:zero:B:/home/b:/bin/sh\n");
        assert!(account_in(&broken, PASSWD, "tester").is_err());
        assert!(application_account_in(&ledger(), &broken, GR, "tda65536").is_err());
    }

    #[test]
    fn the_password_field_classifies_the_way_the_policy_table_says() {
        assert_eq!(classify(""), Secret::NoPassword);
        assert_eq!(classify("*"), Secret::Locked);
        assert_eq!(classify("!"), Secret::Locked);
        assert_eq!(classify("!!"), Secret::Locked);
        assert_eq!(classify("!$6$salt$hash"), Secret::Locked);
        assert_eq!(classify("!td-service"), Secret::Service);
        assert_eq!(classify("$6$salt$hash"), Secret::Hashed);
        assert_eq!(classify("x"), Secret::Locked);
    }

    /// The policy table of THREAT-MODEL.md §3, asserted as a table. The row that
    /// matters most is `Locked` under `forced`: busybox's `login -f` skips the
    /// database entirely, so a locked account could be revived by editing
    /// /etc/autologin. Here it cannot.
    #[test]
    fn the_authentication_policy_denies_everything_it_cannot_verify() {
        assert_eq!(may_start_session(Secret::NoPassword, false), Ok(()));
        assert_eq!(may_start_session(Secret::NoPassword, true), Ok(()));
        assert_eq!(
            may_start_session(Secret::Hashed, false),
            Err(Denied::NeedsPassword)
        );
        assert_eq!(may_start_session(Secret::Hashed, true), Ok(()));
        assert_eq!(
            may_start_session(Secret::Locked, false),
            Err(Denied::Locked)
        );
        assert_eq!(may_start_session(Secret::Locked, true), Err(Denied::Locked));
        assert_eq!(
            may_start_session(Secret::Service, false),
            Err(Denied::ServiceOnly)
        );
        assert_eq!(
            may_start_session(Secret::Service, true),
            Err(Denied::ServiceOnly)
        );
        assert_eq!(may_start_service(Secret::Service), Ok(()));
        for secret in [Secret::NoPassword, Secret::Locked, Secret::Hashed] {
            assert_eq!(may_start_service(secret), Err(Denied::NotService));
        }
    }
}
