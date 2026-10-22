use super::*;
use crate::{
    format::key::SourceKind,
    ids::{AccountId, InstanceId, StoreEpoch},
    import_snapshot::{self as snapshot, Key},
    postgres_imports as database,
    postgres_input::Loaded,
};

pub(super) fn run(
    archive: &archive::Archive,
    connection: &str,
    epoch: &str,
    account: &str,
) -> Result<Json> {
    let epoch = StoreEpoch::parse(epoch).map_err(|_| "migration-invalid-epoch")?;
    let account = AccountId::parse(account).map_err(|_| "migration-invalid-account")?;
    let manifest = archive.snapshot()?;
    let source = field(&manifest, "source")?;
    let instance =
        InstanceId::parse(string(source, "instance")?).map_err(|_| "migration-invalid-source")?;
    let source_account = string(source, "account")?.as_bytes();
    // Validate all metadata before the first publication; bodies are rehashed
    // immediately before use while the cooperative archive lock remains held.
    let mailboxes = mailboxes(&manifest, instance, source_account)?;
    let messages = messages(&manifest, instance, source_account)?;
    let (loaded, address, authority) = connect(connection, epoch, account)?;
    let request = loaded.bootstrap.request(address);
    let mut created_mailboxes = 0usize;
    let mut created_messages = 0usize;
    let mut absent = 0usize;
    for value in &mailboxes {
        let outcome =
            database::publish_mailbox(&request, &loaded.trust, deadline(), &authority, value)
                .map_err(code)?;
        created_mailboxes += usize::from(outcome.created);
        absent += usize::from(!outcome.present);
    }
    for (value, body, size) in &messages {
        let bytes = archive.read_body(body, *size)?;
        let outcome = database::publish_email(
            &request,
            &loaded.trust,
            deadline(),
            &authority,
            value,
            &bytes,
        )
        .map_err(code)?;
        created_messages += usize::from(outcome.created);
        absent += usize::from(!outcome.present);
    }
    Ok(
        json!({"status":"imported-provisional","mailboxes":mailboxes.len(),"messages":messages.len(),"createdMailboxes":created_mailboxes,"createdMessages":created_messages,"historicalObjectsAbsent":absent,"manifestSha256":digest(manifest.to_string().as_bytes())?,"cutoverReady":false}),
    )
}
pub(super) fn connect(
    connection: &str,
    epoch: StoreEpoch,
    account: AccountId,
) -> Result<(Loaded, std::net::IpAddr, database::Authority)> {
    let loaded = Loaded::open(connection, "migration").map_err(|_| "migration-connection-file")?;
    let executable = std::env::current_exe().map_err(|_| "migration-resolver")?;
    let addresses = crate::postgres_resolver::resolve(
        &executable,
        &loaded.bootstrap,
        Instant::now() + Duration::from_secs(5),
    )
    .map_err(|_| "migration-resolver")?;
    let mut selected = None;
    for address in addresses {
        let request = loaded.bootstrap.request(address);
        match database::prepare(&request, &loaded.trust, deadline(), epoch, account) {
            Ok(authority) => {
                selected = Some((address, authority));
                break;
            }
            Err(database::Error::Connection(_)) => {}
            Err(error) => return Err(code(error)),
        }
    }
    let (address, authority) = selected.ok_or("migration-database-connection")?;
    Ok((loaded, address, authority))
}
pub(super) fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(10)
}
pub(super) fn code(error: database::Error) -> &'static str {
    use database::Error as E;
    match error {
        E::Owner => "migration-owner-required",
        E::Epoch => "migration-epoch",
        E::Policy => "migration-policy",
        E::Capacity => "migration-capacity",
        E::Quota => "migration-quota",
        E::Integrity => "migration-integrity",
        E::Conflict => "migration-conflict",
        E::SourceChanged => "migration-source-changed",
        E::MissingMailbox => "migration-missing-local-mailbox",
        E::Invalid => "migration-unrepresentable",
        E::Crypto => "migration-crypto",
        E::Database => "migration-database",
        E::Indeterminate => "migration-indeterminate-retry-same-archive",
        E::Connection(_) => "migration-database-connection",
    }
}
fn optional<'a>(value: &'a Json, key: &str) -> Result<Option<&'a str>> {
    match field(value, key)? {
        Json::Null => Ok(None),
        Json::Str(s) => Ok(Some(s)),
        _ => Err("migration-invalid-field"),
    }
}
pub(super) fn mailboxes(
    manifest: &Json,
    instance: InstanceId,
    account: &[u8],
) -> Result<Vec<snapshot::Mailbox>> {
    let source = array(manifest, "mailboxes")?;
    if source.len() > crate::postgres_mailboxes::MAX_MAILBOXES {
        return Err("migration-unrepresentable");
    }
    let mut remaining: Vec<_> = source.iter().collect();
    let mut seen = std::collections::BTreeSet::new();
    let mut output = Vec::new();
    while !remaining.is_empty() {
        let index = remaining
            .iter()
            .position(|value| {
                optional(value, "parentId")
                    .is_ok_and(|parent| parent.is_none_or(|id| seen.contains(id)))
            })
            .ok_or("migration-missing-parent")?;
        let value = remaining.remove(index);
        let id = string(value, "id")?;
        let key = Key::new(instance, SourceKind::Mailbox, account, id.as_bytes())
            .map_err(|_| "migration-unrepresentable")?;
        output.push(
            snapshot::Mailbox::new(
                key,
                snapshot::MailboxInput {
                    name: string(value, "name")?,
                    parent: optional(value, "parentId")?.map(str::as_bytes),
                    role: optional(value, "role")?,
                    sort_order: u32::try_from(integer(value, "sortOrder")?)
                        .map_err(|_| "migration-unrepresentable")?,
                    subscribed: field(value, "isSubscribed")?
                        .as_bool()
                        .ok_or("migration-invalid-field")?,
                },
            )
            .map_err(|_| "migration-unrepresentable")?,
        );
        seen.insert(id);
    }
    Ok(output)
}
pub(super) fn messages(
    manifest: &Json,
    instance: InstanceId,
    account: &[u8],
) -> Result<Vec<(snapshot::Email, String, u64)>> {
    let mut output = Vec::new();
    for record in array(manifest, "messages")? {
        let email = field(record, "email")?;
        let body = string(record, "body")?;
        let length = integer(email, "size")?;
        let mut hash = [0; 32];
        for (part, slot) in body
            .as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .zip(hash.iter_mut())
        {
            let text = std::str::from_utf8(part).map_err(|_| "migration-invalid-body")?;
            *slot = u8::from_str_radix(text, 16).map_err(|_| "migration-invalid-body")?;
        }
        let mailboxes: Vec<_> = field(email, "mailboxIds")?
            .as_obj()
            .ok_or("migration-invalid-field")?
            .iter()
            .map(|(id, _)| id.as_bytes())
            .collect();
        let keywords: Vec<_> = field(email, "keywords")?
            .as_obj()
            .ok_or("migration-invalid-field")?
            .iter()
            .map(|(id, _)| id.as_str())
            .collect();
        let key = Key::new(
            instance,
            SourceKind::Email,
            account,
            string(email, "id")?.as_bytes(),
        )
        .map_err(|_| "migration-unrepresentable")?;
        let value = snapshot::Email::new(
            key,
            snapshot::EmailInput {
                body_digest: hash,
                length,
                received_at: utc_ms(string(email, "receivedAt")?)?,
                mailboxes: &mailboxes,
                keywords: &keywords,
            },
        )
        .map_err(|_| "migration-unrepresentable")?;
        output.push((value, body.to_owned(), length));
    }
    Ok(output)
}
pub(super) fn utc_ms(value: &str) -> Result<i64> {
    let bytes = value.as_bytes();
    if bytes.len() < 20
        || bytes.get(4) != Some(&b'-')
        || bytes.get(7) != Some(&b'-')
        || bytes.get(10) != Some(&b'T')
        || bytes.get(13) != Some(&b':')
        || bytes.get(16) != Some(&b':')
        || !value.ends_with('Z')
    {
        return Err("migration-unrepresentable-date");
    }
    for range in [0..4, 5..7, 8..10, 11..13, 14..16, 17..19] {
        if !bytes
            .get(range)
            .is_some_and(|b| b.iter().all(u8::is_ascii_digit))
        {
            return Err("migration-unrepresentable-date");
        }
    }
    if bytes.get(17..19).is_none_or(|b| b > b"59".as_slice()) {
        return Err("migration-unrepresentable-date");
    }
    let tail = value.get(19..).ok_or("migration-unrepresentable-date")?;
    let fraction = if tail == "Z" {
        ""
    } else {
        tail.strip_prefix('.')
            .and_then(|s| s.strip_suffix('Z'))
            .filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
            .ok_or("migration-unrepresentable-date")?
    };
    if fraction.len() > 9 || fraction.bytes().skip(3).any(|b| b != b'0') {
        return Err("migration-unrepresentable-date");
    }
    let mut millis = 0i64;
    for (index, byte) in fraction.bytes().take(3).enumerate() {
        millis += i64::from(byte - b'0')
            * match index {
                0 => 100,
                1 => 10,
                _ => 1,
            };
    }
    td_civil::parse_rfc3339(value)
        .and_then(|s| s.checked_mul(1000))
        .and_then(|s| s.checked_add(millis))
        .ok_or("migration-unrepresentable-date")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_milliseconds_and_strict_utc_dates() {
        for (text, want) in [
            ("2024-01-02T03:04:05.123Z", 1704164645123),
            ("2024-01-02T03:04:05.1Z", 1704164645100),
            ("2024-01-02T03:04:05.123000000Z", 1704164645123),
            ("1969-12-31T23:59:59.999Z", -1),
        ] {
            assert_eq!(utc_ms(text), Ok(want));
        }
        for text in [
            "2024-01-02T03:04:05.1231Z",
            "2024-01-02T03:04:60Z",
            "2024-01-02T03:04Z",
            "2024-01-02T03:04:05+00:00",
            "2024-01-02t03:04:05Z",
            " 2024-01-02T03:04:05Z",
            "2024-02-31T03:04:05Z",
            "2024-01-02T03:04:05.Z",
        ] {
            assert!(utc_ms(text).is_err(), "{text}");
        }
    }
}
