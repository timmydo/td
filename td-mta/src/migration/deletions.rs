//! Source absence is explicit, freshly checked, and separate from local custody.
use super::*;
use crate::{
    ids::{AccountId, InstanceId, StoreEpoch},
    postgres_imports::reconcile::deletion::{self, Applied, Baseline, Plan},
};

pub(super) fn run(
    current: &archive::Archive,
    previous: &archive::Archive,
    options: &Options,
) -> Result<Json> {
    let get = |key| {
        options
            .values
            .get(key)
            .map(String::as_str)
            .ok_or("migration-arguments")
    };
    let now = current.snapshot()?;
    let before = previous.snapshot()?;
    let source = field(&now, "source")?;
    if source.to_string() != field(&before, "source")?.to_string() {
        return Err("migration-source-mismatch");
    }
    let epoch = StoreEpoch::parse(get("--epoch")?).map_err(|_| "migration-invalid-epoch")?;
    let account = AccountId::parse(get("--account")?).map_err(|_| "migration-invalid-account")?;
    let instance =
        InstanceId::parse(string(source, "instance")?).map_err(|_| "migration-invalid-source")?;
    let messages = import::messages(&before, instance, string(source, "account")?.as_bytes())?;
    let live: std::collections::BTreeSet<_> = array(&now, "messages")?
        .iter()
        .map(|r| string(field(r, "email")?, "id"))
        .collect::<Result<_>>()?;
    let mut candidates = Vec::new();
    for message in &messages {
        let id = std::str::from_utf8(&message.0.key.object).map_err(|_| "migration-invalid-id")?;
        if !live.contains(id) {
            candidates.push(message);
        }
    }
    let ids: Vec<_> = candidates
        .iter()
        .map(|(v, _, _)| {
            std::str::from_utf8(&v.key.object)
                .map(str::to_owned)
                .map_err(|_| "migration-invalid-id")
        })
        .collect::<Result<_>>()?;
    let current_digest = digest(now.to_string().as_bytes())?;
    let previous_digest = digest(before.to_string().as_bytes())?;
    let retained = if options.action == "deletion-apply" {
        let plan = reconcile::read_plan(get("--plan-file")?)?;
        if integer(&plan, "format")? != 3
            || string(&plan, "epoch")? != epoch.to_string()
            || string(&plan, "account")? != account.to_string()
            || string(&plan, "manifestSha256")? != current_digest
            || string(&plan, "previousManifestSha256")? != previous_digest
            || field(&plan, "source")?.to_string() != source.to_string()
        {
            return Err("migration-plan-binding");
        }
        Some(operations(&plan, &ids, &candidates, epoch, account)?)
    } else {
        None
    };
    let remote = source_proof(current, previous, options, &now, &ids, "Email")?;
    let fence = || source_fence(&remote, &now);
    let (loaded, address, authority) = import::connect(get("--connection-file")?, epoch, account)?;
    let request = loaded.bootstrap.request(address);
    let mut output = Vec::new();
    let mut size = 0;
    // Reserve the largest apply report before any database write.
    for id in &ids {
        reconcile::reserve(
            &mut size,
            &json!({"id":id,"status":"historical-object-absent"}),
        )?;
    }
    size = 0;
    let mut deleted = 0;
    for (index, (value, _, _)) in candidates.iter().enumerate() {
        let id = ids.get(index).ok_or("migration-invalid-id")?;
        let item = if let Some(operations) = &retained {
            match operations.get(index).ok_or("migration-plan-objects")? {
                None => json!({"id":id,"status":"skipped-retained-observation"}),
                Some(baseline) => {
                    remote.absent(
                        "Email",
                        std::slice::from_ref(id),
                        string(&now, "emailState")?,
                    )?;
                    let status = match deletion::apply(
                        &request,
                        &loaded.trust,
                        import::deadline(),
                        &authority,
                        value,
                        baseline,
                    )
                    .map_err(reconcile::code)?
                    {
                        Applied::Deleted => {
                            deleted += 1;
                            "deleted"
                        }
                        Applied::Absent => "historical-object-absent",
                    };
                    json!({"id":id,"status":status})
                }
            }
        } else {
            match deletion::plan(
                &request,
                &loaded.trust,
                import::deadline(),
                &authority,
                value,
            )
            .map_err(import::code)?
            {
                Plan::Unmapped => json!({"id":id,"status":"unmapped"}),
                Plan::Absent => json!({"id":id,"status":"historical-object-absent"}),
                Plan::Conflict => json!({"id":id,"status":"local-conflict"}),
                Plan::Delete(b) => {
                    json!({"id":id,"status":"delete","local":hex(&b.local),"historicalBlob":hex(&b.historical_blob),"oldDigest":hex(&b.digest),"changed":format!("{:016x}",b.changed)})
                }
            }
        };
        reconcile::append(&mut output, &mut size, item)?;
    }
    fence()?;
    Ok(
        json!({"format":3,"status":if retained.is_some(){"applied-provisional-deletions"}else{"planned-provisional-deletions"},"epoch":epoch.to_string(),"account":account.to_string(),"source":source,"manifestSha256":current_digest,"previousManifestSha256":previous_digest,"deleted":deleted,"messages":output,"absenceEvidence":"authenticated-account-query-and-explicit-notFound","cutoverReady":false}),
    )
}

fn operations(
    plan: &Json,
    ids: &[String],
    candidates: &[&(crate::import_snapshot::Email, String, u64)],
    epoch: StoreEpoch,
    account: AccountId,
) -> Result<Vec<Option<Baseline>>> {
    let entries = array(plan, "messages")?;
    if entries.len() != ids.len() {
        return Err("migration-plan-objects");
    }
    let mut by_id = BTreeMap::new();
    for entry in entries {
        if by_id.insert(string(entry, "id")?, entry).is_some() {
            return Err("migration-plan-objects");
        }
    }
    let mut result = Vec::new();
    for (id, (value, _, _)) in ids.iter().zip(candidates) {
        let entry = by_id.remove(id.as_str()).ok_or("migration-plan-objects")?;
        result.push(match string(entry, "status")? {
            "unmapped" | "historical-object-absent" | "local-conflict" => None,
            "delete" => {
                let digest = reconcile::fixed(string(entry, "oldDigest")?)?;
                if digest != value.digest {
                    return Err("migration-plan-binding");
                }
                Some(Baseline {
                    epoch,
                    account,
                    key: value.key.clone(),
                    local: reconcile::fixed(string(entry, "local")?)?,
                    historical_blob: reconcile::fixed(string(entry, "historicalBlob")?)?,
                    digest,
                    changed: u64::from_be_bytes(reconcile::fixed(string(entry, "changed")?)?),
                })
            }
            _ => return Err("migration-plan-operation"),
        });
    }
    if !by_id.is_empty() {
        return Err("migration-plan-objects");
    }
    Ok(result)
}

pub(super) fn source_fence(remote: &source::Source, manifest: &Json) -> Result<()> {
    if remote.state("Email")? != string(manifest, "emailState")?
        || remote.state("Mailbox")? != string(manifest, "mailboxState")?
    {
        return Err("migration-source-changed");
    }
    Ok(())
}
pub(super) fn source_proof(
    current: &archive::Archive,
    previous: &archive::Archive,
    options: &Options,
    now: &Json,
    ids: &[String],
    kind: &str,
) -> Result<source::Source> {
    let get = |key| {
        options
            .values
            .get(key)
            .map(String::as_str)
            .ok_or("migration-arguments")
    };
    previous.verify()?;
    current.verify()?;
    let source_account = string(field(now, "source")?, "account")?;
    inventory::run(
        current,
        get("--session")?,
        source_account,
        get("--username")?,
        get("--password-file")?,
        options.values.get("--ca-file").map(String::as_str),
    )?;
    let remote = source::Source::open(
        get("--session")?,
        source_account,
        get("--username")?,
        get("--password-file")?,
        options.values.get("--ca-file").map(String::as_str),
    )?;
    source_fence(&remote, now)?;
    if kind == "Mailbox" {
        let found = remote.mailboxes(string(now, "mailboxState")?)?;
        let actual: std::collections::BTreeSet<_> = found
            .iter()
            .map(|v| string(v, "id"))
            .collect::<Result<_>>()?;
        let expected: std::collections::BTreeSet<_> = array(now, "mailboxes")?
            .iter()
            .map(|v| string(v, "id"))
            .collect::<Result<_>>()?;
        if actual != expected {
            return Err("migration-inventory-mismatch");
        }
    }
    let state = string(
        now,
        if kind == "Mailbox" {
            "mailboxState"
        } else {
            "emailState"
        },
    )?;
    for batch in ids.chunks(64) {
        remote.absent(kind, batch, state)?;
    }
    source_fence(&remote, now)?;
    Ok(remote)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::{
        format::key::SourceKind,
        import_snapshot::{Email, EmailInput, Key},
    };
    #[test]
    fn retained_deletions_bind_every_candidate_and_old_digest() {
        let value = (
            Email::new(
                Key::new(
                    InstanceId::from_bytes([1; 16]),
                    SourceKind::Email,
                    b"account",
                    b"one",
                )
                .unwrap(),
                EmailInput {
                    body_digest: [0; 32],
                    length: 0,
                    received_at: 0,
                    mailboxes: &[b"inbox"],
                    keywords: &[],
                },
            )
            .unwrap(),
            String::new(),
            0,
        );
        let ids = vec!["one".to_owned()];
        let candidates = vec![&value];
        let epoch = StoreEpoch::from_bytes([2; 16]);
        let account = AccountId::from_bytes([3; 16]);
        let entry = json!({"id":"one","status":"delete","local":"04".repeat(16),"historicalBlob":"05".repeat(16),"oldDigest":hex(&value.0.digest),"changed":"0000000000000007"});
        let parsed = operations(
            &json!({"messages":[entry.clone()]}),
            &ids,
            &candidates,
            epoch,
            account,
        )
        .unwrap();
        let baseline = parsed.first().unwrap().as_ref().unwrap();
        assert_eq!(baseline.local, [4; 16]);
        assert_eq!(baseline.changed, 7);
        for plan in [
            json!({"messages":[]}),
            json!({"messages":[entry.clone(),entry]}),
            json!({"messages":[{"id":"other","status":"local-conflict"}]}),
            json!({"messages":[{"id":"one","status":"update"}]}),
            json!({"messages":[{"id":"one","status":"delete","oldDigest":"00".repeat(32)}]}),
        ] {
            assert!(operations(&plan, &ids, &candidates, epoch, account).is_err());
        }
    }
}
