//! Provisional, resumable JMAP export. Archives grant no cutover authority.
mod archive;
mod compare;
mod deletions;
mod folder_deletions;
mod folder_reconcile;
mod import;
mod inventory;
mod reconcile;
mod source;
mod unmapped;
use std::{
    collections::BTreeMap,
    ffi::OsString,
    io::{self, Read},
    path::Path,
    time::{Duration, Instant},
};
use td_crypto::{Digest, Sha256};
use td_json::{json, Json};
type Result<T> = std::result::Result<T, &'static str>;
const INDEX_LIMIT: usize = 32 * 1024 * 1024;
const OBJECT_LIMIT: usize = 100_000;
const MAILBOX_LIMIT: usize = 4096;
fn field<'a>(value: &'a Json, key: &str) -> Result<&'a Json> {
    value.get(key).ok_or("migration-missing-field")
}
fn string<'a>(value: &'a Json, key: &str) -> Result<&'a str> {
    field(value, key)?
        .as_str()
        .ok_or("migration-invalid-string")
}
fn array<'a>(value: &'a Json, key: &str) -> Result<&'a [Json]> {
    field(value, key)?
        .as_array()
        .ok_or("migration-invalid-array")
}
fn integer(value: &Json, key: &str) -> Result<u64> {
    field(value, key)?
        .as_u64()
        .ok_or("migration-invalid-integer")
}
fn identifier(id: &str) -> Result<()> {
    if id.is_empty() || id.len() > 998 || id.bytes().any(|b| b < 32 || b == 127) {
        Err("migration-invalid-id")
    } else {
        Ok(())
    }
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn digest(bytes: &[u8]) -> Result<String> {
    let mut h = Sha256::try_new().map_err(|_| "migration-digest")?;
    h.update(bytes).map_err(|_| "migration-digest")?;
    Ok(hex(&h.finish().map_err(|_| "migration-digest")?))
}
fn hash_name(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn read_bounded(input: impl io::Read, limit: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    input
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "migration-read")?;
    if bytes.len() > limit {
        return Err("migration-capacity");
    }
    Ok(bytes)
}
fn parse(bytes: &[u8]) -> Result<Json> {
    td_json::parse_slice(bytes).map_err(|_| "migration-invalid-json")
}

/// Fixed diagnostics deliberately exclude source content, paths and credentials.
pub fn command(args: impl Iterator<Item = OsString>) -> io::Result<std::process::ExitCode> {
    use io::Write;
    let result = arguments(args).and_then(run);
    match result {
        Ok(report) => {
            writeln!(io::stdout().lock(), "{report}")?;
            Ok(std::process::ExitCode::SUCCESS)
        }
        Err(code) => {
            writeln!(io::stderr().lock(), "{code}")?;
            Ok(std::process::ExitCode::FAILURE)
        }
    }
}
struct Options {
    action: String,
    only_unmapped: bool,
    mailboxes: bool,
    values: BTreeMap<String, String>,
}
fn arguments(mut args: impl Iterator<Item = OsString>) -> Result<Options> {
    let action = match args.next().and_then(|s| s.into_string().ok()).as_deref() {
        Some("export") => "export",
        Some("inventory") => "inventory",
        Some("verify") => "verify",
        Some("compare") => "compare",
        Some("import") => "import",
        Some("plan") => "plan",
        Some("apply") => "apply",
        Some("deletion-plan") => "deletion-plan",
        Some("deletion-apply") => "deletion-apply",
        _ => return Err("migration-arguments"),
    };
    let mut values = BTreeMap::new();
    let mut only_unmapped = false;
    let mut mailboxes = false;
    while let Some(key) = args.next() {
        let key = key.into_string().map_err(|_| "migration-arguments")?;
        if key == "--mailboxes" {
            if !matches!(action, "plan" | "deletion-plan") || mailboxes {
                return Err("migration-arguments");
            }
            mailboxes = true;
            continue;
        }
        if key == "--only-unmapped" {
            if action != "import" || only_unmapped {
                return Err("migration-arguments");
            }
            only_unmapped = true;
            continue;
        }
        if ![
            "--archive",
            "--against-archive",
            "--previous-archive",
            "--max-bytes",
            "--session",
            "--account",
            "--username",
            "--password-file",
            "--ca-file",
            "--connection-file",
            "--epoch",
            "--plan-file",
        ]
        .contains(&key.as_str())
            || (action == "verify" && !["--archive", "--max-bytes"].contains(&key.as_str()))
            || (matches!(action, "import" | "plan" | "apply")
                && ![
                    "--archive",
                    "--max-bytes",
                    "--connection-file",
                    "--epoch",
                    "--account",
                    "--plan-file",
                ]
                .contains(&key.as_str()))
            || (key == "--against-archive" && action != "compare")
            || (action == "compare"
                && !["--archive", "--against-archive", "--max-bytes"].contains(&key.as_str()))
            || (key == "--plan-file" && !matches!(action, "apply" | "deletion-apply"))
            || (key == "--previous-archive"
                && !matches!(action, "deletion-plan" | "deletion-apply"))
            || (matches!(action, "export" | "inventory")
                && ["--connection-file", "--epoch"].contains(&key.as_str()))
        {
            return Err("migration-arguments");
        }
        let value = args
            .next()
            .and_then(|s| s.into_string().ok())
            .ok_or("migration-arguments")?;
        if values.insert(key, value).is_some() {
            return Err("migration-arguments");
        }
    }
    Ok(Options {
        action: action.to_owned(),
        only_unmapped,
        mailboxes,
        values,
    })
}
fn run(options: Options) -> Result<Json> {
    let get = |key| {
        options
            .values
            .get(key)
            .map(String::as_str)
            .ok_or("migration-arguments")
    };
    let maximum: u64 = get("--max-bytes")?
        .parse()
        .map_err(|_| "migration-arguments")?;
    if maximum == 0 || maximum > 1024 * 1024 * 1024 * 1024 {
        return Err("migration-arguments");
    }
    let mut archive = archive::Archive::open(get("--archive")?, maximum)?;
    if matches!(options.action.as_str(), "deletion-plan" | "deletion-apply") {
        let previous = archive::Archive::open(get("--previous-archive")?, maximum)?;
        let folders = options.mailboxes
            || (options.action == "deletion-apply"
                && reconcile::read_plan(get("--plan-file")?)?
                    .get("format")
                    .and_then(Json::as_u64)
                    == Some(4));
        return if folders {
            folder_deletions::run(&archive, &previous, &options)
        } else {
            deletions::run(&archive, &previous, &options)
        };
    }
    if options.action == "compare" {
        let against = archive::Archive::open(get("--against-archive")?, maximum)?;
        return compare::run(&archive, &against);
    }
    if options.action == "verify" {
        return archive.verify();
    }
    if matches!(options.action.as_str(), "plan" | "apply") {
        let retained = if options.action == "apply" {
            Some(reconcile::read_plan(get("--plan-file")?)?)
        } else {
            None
        };
        let run = if options.mailboxes
            || retained
                .as_ref()
                .is_some_and(|p| p.get("format").and_then(Json::as_u64) == Some(2))
        {
            folder_reconcile::run
        } else {
            reconcile::run
        };
        return run(
            &archive,
            get("--connection-file")?,
            get("--epoch")?,
            get("--account")?,
            retained,
        );
    }
    if options.action == "import" {
        let run = if options.only_unmapped {
            unmapped::run
        } else {
            import::run
        };
        return run(
            &archive,
            get("--connection-file")?,
            get("--epoch")?,
            get("--account")?,
        );
    }
    let session = get("--session")?;
    let account = get("--account")?;
    let username = get("--username")?;
    identifier(account)?;
    if options.action == "inventory" {
        return inventory::run(
            &archive,
            session,
            account,
            username,
            get("--password-file")?,
            options.values.get("--ca-file").map(String::as_str),
        );
    }
    let source = source::Source::open(
        session,
        account,
        username,
        get("--password-file")?,
        options.values.get("--ca-file").map(String::as_str),
    )?;
    let binding = archive.bind(session, account, username)?;
    export(&source, &mut archive, binding)
}
fn export(source: &source::Source, archive: &mut archive::Archive, binding: Json) -> Result<Json> {
    let mailbox_state = source.state("Mailbox")?;
    let email_state = source.state("Email")?;
    let mailboxes = source.mailboxes(&mailbox_state)?;
    let mut index_bytes = 0usize;
    for mailbox in &mailboxes {
        index_bytes = index_bytes
            .checked_add(mailbox.to_string().len())
            .filter(|n| *n <= INDEX_LIMIT / 2)
            .ok_or("migration-capacity")?;
    }
    let mut emails = BTreeMap::new();
    for mailbox in &mailboxes {
        let mailbox_id = string(mailbox, "id")?;
        let ids = source.query("Email", Some(mailbox_id), OBJECT_LIMIT)?;
        for batch in ids.chunks(64) {
            for email in source.get("Email", batch, &email_state)? {
                validate_email(&email)?;
                let id = string(&email, "id")?.to_owned();
                if let Some(previous) = emails.get(&id) {
                    if !same_metadata(previous, &email) {
                        return Err("migration-source-changed");
                    }
                } else {
                    index_bytes = index_bytes
                        .checked_add(email.to_string().len())
                        .ok_or("migration-capacity")?;
                    if index_bytes > INDEX_LIMIT / 2 || emails.len() >= OBJECT_LIMIT {
                        return Err("migration-capacity");
                    }
                    emails.insert(id, email);
                }
            }
        }
    }
    let mut records = Vec::new();
    for email in emails.into_values() {
        let body = archive.body(source, &email)?;
        records.push(json!({"email":email,"body":body}));
    }
    if source.state("Mailbox")? != mailbox_state || source.state("Email")? != email_state {
        return Err("migration-source-changed");
    }
    let manifest = json!({"version":1,"provisional":true,"coverage":"per-mailbox-query","source":binding,"mailboxState":mailbox_state,"emailState":email_state,"mailboxes":mailboxes,"messages":records});
    validate_manifest(&manifest)?;
    archive.publish(&manifest)?;
    archive.verify()
}
fn same_metadata(left: &Json, right: &Json) -> bool {
    left.to_canonical() == right.to_canonical()
}
fn validate_email(email: &Json) -> Result<()> {
    identifier(string(email, "id")?)?;
    identifier(string(email, "blobId")?)?;
    if integer(email, "size")? > crate::outbound_http::MAX_BODY as u64 {
        return Err("migration-message-too-large");
    }
    let received = string(email, "receivedAt")?;
    if received.len() > 64 || td_civil::parse_rfc3339(received).is_none() {
        return Err("migration-invalid-date");
    }
    for key in ["mailboxIds", "keywords"] {
        let values = field(email, key)?
            .as_obj()
            .ok_or("migration-invalid-membership")?;
        if values.len() > 4096 || (key == "mailboxIds" && values.is_empty()) {
            return Err("migration-invalid-membership");
        }
        for (name, value) in values {
            identifier(name)?;
            if key == "keywords" && name.len() > 255 {
                return Err("migration-invalid-keyword");
            }
            if value.as_bool() != Some(true) {
                return Err("migration-invalid-membership");
            }
        }
    }
    Ok(())
}
fn validate_manifest(manifest: &Json) -> Result<()> {
    if integer(manifest, "version")? != 1
        || field(manifest, "provisional")?.as_bool() != Some(true)
        || string(manifest, "coverage")? != "per-mailbox-query"
    {
        return Err("migration-archive-version");
    }
    identifier(string(manifest, "mailboxState")?)?;
    identifier(string(manifest, "emailState")?)?;
    let mailboxes = array(manifest, "mailboxes")?;
    let records = array(manifest, "messages")?;
    if mailboxes.len() > MAILBOX_LIMIT || records.len() > OBJECT_LIMIT {
        return Err("migration-capacity");
    }
    let mut parents = BTreeMap::new();
    for mailbox in mailboxes {
        let id = string(mailbox, "id")?;
        identifier(id)?;
        let name = string(mailbox, "name")?;
        if name.is_empty() || name.len() > 1024 {
            return Err("migration-invalid-mailbox");
        }
        let parent = match field(mailbox, "parentId")? {
            Json::Null => None,
            Json::Str(s) => {
                identifier(s)?;
                Some(s.as_str())
            }
            _ => return Err("migration-invalid-mailbox"),
        };
        if parents.insert(id, parent).is_some() {
            return Err("migration-duplicate-id");
        }
        match field(mailbox, "role")? {
            Json::Null => {}
            Json::Str(s) if s.len() <= 64 => {}
            _ => return Err("migration-invalid-mailbox"),
        }
        if integer(mailbox, "sortOrder")? > i32::MAX as u64
            || field(mailbox, "isSubscribed")?.as_bool().is_none()
        {
            return Err("migration-invalid-mailbox");
        }
    }
    for id in parents.keys() {
        let mut next = Some(*id);
        for step in 0..=parents.len() {
            let Some(id) = next else { break };
            if step == parents.len() {
                return Err("migration-mailbox-cycle");
            }
            next = *parents.get(id).ok_or("migration-missing-parent")?;
        }
    }
    let mut ids = std::collections::BTreeSet::new();
    for record in records {
        let email = field(record, "email")?;
        validate_email(email)?;
        if !ids.insert(string(email, "id")?) {
            return Err("migration-duplicate-id");
        }
        for (id, _) in field(email, "mailboxIds")?
            .as_obj()
            .ok_or("migration-invalid-membership")?
        {
            if !parents.contains_key(id.as_str()) {
                return Err("migration-missing-mailbox");
            }
        }
        if !hash_name(string(record, "body")?) {
            return Err("migration-invalid-body");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    fn mailbox(id: &str, parent: Json) -> Json {
        json!({"id":id,"name":"Inbox","parentId":parent,"role":"inbox","sortOrder":0,"isSubscribed":true})
    }
    fn email(id: &str) -> Json {
        json!({"id":id,"blobId":"blob","size":3,"receivedAt":"2024-01-02T03:04:05.123Z","mailboxIds":{"inbox":true,"child":true},"keywords":{"$seen":true,"custom":true}})
    }
    pub(super) fn manifest(source: Json, body: &str) -> Json {
        json!({"version":1,"provisional":true,"coverage":"per-mailbox-query","source":source,"mailboxState":"s1","emailState":"s2","mailboxes":[mailbox("inbox",Json::Null),mailbox("child",Json::Str("inbox".into()))],"messages":[{"email":email("one"),"body":body},{"email":email("two"),"body":body}]})
    }
    #[test]
    fn archive_accepts_legacy_large_messages_but_keeps_a_hard_ceiling() {
        for (size, expected) in [
            (43_041_897u64, Ok(())),
            (64 * 1024 * 1024, Ok(())),
            (64 * 1024 * 1024 + 1, Err("migration-message-too-large")),
        ] {
            let mut value = email("large");
            if let Json::Obj(ref mut pairs) = value {
                for (key, v) in pairs {
                    if key == "size" {
                        *v = json!(size);
                    }
                }
            }
            assert_eq!(validate_email(&value), expected);
        }
    }
    #[test]
    fn preserves_distinct_ids_identical_bodies_and_full_membership() {
        let manifest = manifest(Json::Null, &"a".repeat(64));
        validate_manifest(&manifest).unwrap();
        let records = array(&manifest, "messages").unwrap();
        assert_eq!(records.len(), 2);
        assert_ne!(
            string(field(records.first().unwrap(), "email").unwrap(), "id").unwrap(),
            string(field(records.get(1).unwrap(), "email").unwrap(), "id").unwrap()
        );
    }
    #[test]
    fn dangling_memberships_duplicate_ids_and_cycles_fail_closed() {
        let base = manifest(Json::Null, &"a".repeat(64));
        for replacement in [
            json!([]),
            json!([mailbox("inbox", Json::Null)]),
            json!([
                mailbox("inbox", Json::Str("child".into())),
                mailbox("child", Json::Str("inbox".into()))
            ]),
            json!([mailbox("inbox", Json::Null), mailbox("inbox", Json::Null)]),
        ] {
            let mut value = base.clone();
            if let Json::Obj(ref mut pairs) = value {
                for (key, v) in pairs {
                    if key == "mailboxes" {
                        *v = replacement.clone();
                    }
                }
            }
            assert!(validate_manifest(&value).is_err());
        }
        let mut value = base.clone();
        if let Json::Obj(ref mut pairs) = value {
            for (key, v) in pairs {
                if key == "messages" {
                    let one = v.as_array().unwrap().first().unwrap().clone();
                    *v = json!([one.clone(), one]);
                }
            }
        }
        assert_eq!(validate_manifest(&value), Err("migration-duplicate-id"));
    }
    #[test]
    fn repeated_membership_ignores_json_property_order_but_not_changes() {
        let first = email("one");
        let mut reordered = first.clone();
        if let Json::Obj(ref mut pairs) = reordered {
            pairs.reverse();
            for (_, value) in pairs {
                if let Json::Obj(nested) = value {
                    nested.reverse();
                }
            }
        }
        assert!(same_metadata(&first, &reordered));
        if let Json::Obj(ref mut pairs) = reordered {
            for (key, value) in pairs {
                if key == "size" {
                    *value = json!(4);
                }
            }
        }
        assert!(!same_metadata(&first, &reordered));
    }
    #[test]
    fn cli_rejects_ambiguous_or_credential_arguments() {
        for args in [
            vec!["verify", "--password-file", "secret"],
            vec!["export", "--archive", "one", "--archive", "two"],
            vec!["export", "--password", "secret"],
            vec!["export", "--archive"],
            vec!["inventory", "--connection-file", "owner"],
            vec!["inventory", "--epoch", "epoch"],
            vec!["inventory", "--plan-file", "plan"],
            vec!["inventory", "--password", "secret"],
            vec!["compare", "--password-file", "secret"],
            vec!["compare", "--connection-file", "owner"],
            vec!["compare", "--session", "https://example.test"],
            vec!["export", "--against-archive", "other"],
            vec!["export", "--previous-archive", "other"],
            vec!["deletion-plan", "--plan-file", "plan"],
            vec!["deletion-apply", "--only-unmapped"],
            vec!["deletion-apply", "--against-archive", "other"],
        ] {
            assert!(arguments(args.into_iter().map(OsString::from)).is_err());
        }
    }
}

#[cfg(test)]
mod unmapped_arguments {
    use super::*;
    #[test]
    fn only_unmapped_is_an_import_only_single_flag() {
        let parse = |args: &[&str]| arguments(args.iter().map(OsString::from));
        assert!(parse(&["import", "--only-unmapped"]).is_ok_and(|v| v.only_unmapped));
        assert!(parse(&["import"]).is_ok_and(|v| !v.only_unmapped));
        assert!(parse(&["import", "--only-unmapped", "--only-unmapped"]).is_err());
        for action in ["export", "inventory", "verify", "compare", "plan", "apply"] {
            assert!(parse(&[action, "--only-unmapped"]).is_err());
        }
    }
}

#[cfg(test)]
mod mailbox_plan_arguments {
    use super::*;
    #[test]
    fn mailbox_selector_is_a_plan_only_single_flag() {
        let parse = |args: &[&str]| arguments(args.iter().map(OsString::from));
        assert!(parse(&["plan", "--mailboxes"]).is_ok_and(|v| v.mailboxes));
        assert!(parse(&["deletion-plan", "--mailboxes"]).is_ok_and(|v| v.mailboxes));
        assert!(parse(&["plan"]).is_ok_and(|v| !v.mailboxes));
        assert!(parse(&["plan", "--mailboxes", "--mailboxes"]).is_err());
        for action in [
            "export",
            "inventory",
            "verify",
            "compare",
            "import",
            "apply",
        ] {
            assert!(parse(&[action, "--mailboxes"]).is_err());
        }
    }
}
