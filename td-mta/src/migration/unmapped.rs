//! Explicit new-object import; mapped receipts are skipped, not reconciled.
use super::*;
use crate::{
    format::key::SourceKind,
    ids::{AccountId, InstanceId, StoreEpoch},
    import_snapshot::{Key, Mailbox},
    postgres_imports as database,
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
    let folders = Folders::new(array(&manifest, "mailboxes")?)?;
    let (loaded, address, authority) = import::connect(connection, epoch, account)?;
    let request = loaded.bootstrap.request(address);
    let mut created_mailboxes = 0usize;
    let mut created_messages = 0usize;
    let mut adopted_mailboxes = 0usize;
    let mut skipped_mailboxes = 0usize;
    let mut skipped_messages = 0usize;
    let mut absent = 0usize;
    for record in folders.ordered()? {
        let id = string(record, "id")?;
        let key = Key::new(instance, SourceKind::Mailbox, source_account, id.as_bytes())
            .map_err(|_| "migration-unrepresentable")?;
        if let Some(outcome) = database::inspect_mapping(
            &request,
            &loaded.trust,
            import::deadline(),
            &authority,
            &key,
        )
        .map_err(import::code)?
        {
            absent += usize::from(!outcome.present);
            skipped_mailboxes += 1;
            continue;
        }
        let definitions = folders.definitions(&[id], instance, source_account)?;
        let value = definitions
            .iter()
            .find(|v| v.key.object == id.as_bytes())
            .ok_or("migration-missing-parent")?;
        let outcome = database::publish_unmapped_mailbox(
            &request,
            &loaded.trust,
            import::deadline(),
            &authority,
            value,
            &definitions,
        )
        .map_err(import::code)?;
        absent += usize::from(!outcome.present);
        if !outcome.recorded {
            skipped_mailboxes += 1;
        } else if outcome.created {
            created_mailboxes += 1;
        } else {
            adopted_mailboxes += 1;
        }
    }
    let messages = array(&manifest, "messages")?;
    for records in messages.chunks(database::MAX_MAPPING_BATCH) {
        let keys = records
            .iter()
            .map(|record| {
                let email = field(record, "email")?;
                Key::new(
                    instance,
                    SourceKind::Email,
                    source_account,
                    string(email, "id")?.as_bytes(),
                )
                .map_err(|_| "migration-unrepresentable")
            })
            .collect::<Result<Vec<_>>>()?;
        let outcomes = database::inspect_mappings(
            &request,
            &loaded.trust,
            import::deadline(),
            &authority,
            &keys,
        )
        .map_err(import::code)?;
        if outcomes.len() != records.len() {
            return Err("migration-integrity");
        }
        for (record, outcome) in records.iter().zip(outcomes) {
            if let Some(outcome) = outcome {
                absent += usize::from(!outcome.present);
                skipped_messages += 1;
                continue;
            }
            let converted = import::messages(
                &json!({"messages":[record.clone()]}),
                instance,
                source_account,
            )?;
            let (value, body, size) = converted.first().ok_or("migration-invalid-field")?;
            let ids: Vec<_> = value
                .mailboxes
                .iter()
                .map(|id| std::str::from_utf8(id).map_err(|_| "migration-invalid-id"))
                .collect::<Result<_>>()?;
            let definitions = folders.definitions(&ids, instance, source_account)?;
            let bytes = archive.read_body(body, *size)?;
            let outcome = database::publish_unmapped_email(
                &request,
                &loaded.trust,
                import::deadline(),
                &authority,
                value,
                &bytes,
                &definitions,
            )
            .map_err(import::code)?;
            absent += usize::from(!outcome.present);
            if outcome.recorded {
                created_messages += 1;
            } else {
                skipped_messages += 1;
            }
        }
    }
    Ok(json!({
        "status":"imported-only-unmapped-provisional",
        "mailboxes":folders.by_id.len(),"messages":messages.len(),
        "createdMailboxes":created_mailboxes,"adoptedMailboxes":adopted_mailboxes,
        "createdMessages":created_messages,"historicalObjectsAbsent":absent,
        "skippedMappedMailboxes":skipped_mailboxes,"skippedMappedMessages":skipped_messages,
        "manifestSha256":digest(manifest.to_string().as_bytes())?,"cutoverReady":false
    }))
}
struct Folders<'a> {
    by_id: BTreeMap<&'a str, &'a Json>,
}
impl<'a> Folders<'a> {
    fn new(records: &'a [Json]) -> Result<Self> {
        let mut by_id = BTreeMap::new();
        for record in records {
            if by_id.insert(string(record, "id")?, record).is_some() {
                return Err("migration-invalid-field");
            }
        }
        Ok(Self { by_id })
    }
    fn ordered(&self) -> Result<Vec<&'a Json>> {
        let mut remaining: Vec<_> = self.by_id.values().copied().collect();
        let mut seen = std::collections::BTreeSet::new();
        let mut output = Vec::new();
        while !remaining.is_empty() {
            let index = remaining
                .iter()
                .position(|v| {
                    field(v, "parentId").is_ok_and(|p| match p {
                        Json::Null => true,
                        Json::Str(s) => seen.contains(s.as_str()),
                        _ => false,
                    })
                })
                .ok_or("migration-missing-parent")?;
            let record = remaining.remove(index);
            seen.insert(string(record, "id")?);
            output.push(record);
        }
        Ok(output)
    }
    fn definitions(
        &self,
        roots: &[&str],
        instance: InstanceId,
        account: &[u8],
    ) -> Result<Vec<Mailbox>> {
        let mut selected = BTreeMap::new();
        for root in roots {
            let mut at = Some(*root);
            let mut path = std::collections::BTreeSet::new();
            while let Some(id) = at {
                if !path.insert(id) {
                    return Err("migration-missing-parent");
                }
                if selected.contains_key(id) {
                    break;
                }
                let record = *self.by_id.get(id).ok_or("migration-missing-parent")?;
                selected.insert(id, record.clone());
                if selected.len() > crate::postgres_mailboxes::MAX_MAILBOXES {
                    return Err("migration-unrepresentable");
                }
                at = match field(record, "parentId")? {
                    Json::Null => None,
                    Json::Str(s) => Some(s.as_str()),
                    _ => return Err("migration-invalid-field"),
                };
            }
        }
        let records: Vec<_> = selected.into_values().collect();
        import::mailboxes(&json!({"mailboxes":records}), instance, account)
    }
}
