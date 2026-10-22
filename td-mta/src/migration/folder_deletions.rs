//! Retained source-folder deletions never remove messages or role folders.
use super::*;
use crate::{
    ids::{AccountId, InstanceId, StoreEpoch},
    postgres_imports::mailbox_reconcile::deletion::{self, Applied, Baseline, Plan},
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
    let mailboxes = import::mailboxes(&before, instance, string(source, "account")?.as_bytes())?;
    let live: std::collections::BTreeSet<_> = array(&now, "mailboxes")?
        .iter()
        .map(|r| string(r, "id"))
        .collect::<Result<_>>()?;
    let mut candidates = Vec::new();
    for mailbox in &mailboxes {
        let id = std::str::from_utf8(&mailbox.key.object).map_err(|_| "migration-invalid-id")?;
        if !live.contains(id) {
            candidates.push(mailbox);
        }
    }
    let ids: Vec<_> = candidates
        .iter()
        .map(|v| {
            std::str::from_utf8(&v.key.object)
                .map(str::to_owned)
                .map_err(|_| "migration-invalid-id")
        })
        .collect::<Result<_>>()?;
    let current_digest = digest(now.to_string().as_bytes())?;
    let previous_digest = digest(before.to_string().as_bytes())?;
    let retained = if options.action == "deletion-apply" {
        let plan = reconcile::read_plan(get("--plan-file")?)?;
        if integer(&plan, "format")? != 4
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
    let remote = deletions::source_proof(current, previous, options, &now, &ids, "Mailbox")?;
    let fence = || deletions::source_fence(&remote, &now);
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
    for (index, value) in candidates.iter().enumerate() {
        let id = ids.get(index).ok_or("migration-invalid-id")?;
        let item = if let Some(operations) = &retained {
            match operations.get(index).ok_or("migration-plan-objects")? {
                None => json!({"id":id,"status":"skipped-retained-observation"}),
                Some(baseline) => {
                    remote.absent(
                        "Mailbox",
                        std::slice::from_ref(id),
                        string(&now, "mailboxState")?,
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
                Plan::Role => json!({"id":id,"status":"role-protected"}),
                Plan::HasChild => json!({"id":id,"status":"has-child"}),
                Plan::HasEmail => json!({"id":id,"status":"has-email"}),
                Plan::Delete(b) => {
                    json!({"id":id,"status":"delete","local":hex(&b.local),"oldDigest":hex(&b.digest),"changed":format!("{:016x}",b.changed)})
                }
            }
        };
        reconcile::append(&mut output, &mut size, item)?;
    }
    fence()?;
    Ok(
        json!({"format":4,"status":if retained.is_some(){"applied-provisional-folder-deletions"}else{"planned-provisional-folder-deletions"},"epoch":epoch.to_string(),"account":account.to_string(),"source":source,"manifestSha256":current_digest,"previousManifestSha256":previous_digest,"deleted":deleted,"mailboxes":output,"absenceEvidence":"authenticated-account-query-and-explicit-notFound","cutoverReady":false}),
    )
}

fn operations(
    plan: &Json,
    ids: &[String],
    candidates: &[&crate::import_snapshot::Mailbox],
    epoch: StoreEpoch,
    account: AccountId,
) -> Result<Vec<Option<Baseline>>> {
    let entries = array(plan, "mailboxes")?;
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
    for (id, value) in ids.iter().zip(candidates) {
        let entry = by_id.remove(id.as_str()).ok_or("migration-plan-objects")?;
        result.push(match string(entry, "status")? {
            "unmapped"
            | "historical-object-absent"
            | "local-conflict"
            | "role-protected"
            | "has-child"
            | "has-email" => None,
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::{
        format::key::SourceKind,
        import_snapshot::{Key, Mailbox, MailboxInput},
    };
    #[test]
    fn retained_folder_plan_requires_exact_candidates_and_digest() {
        let value = Mailbox::new(
            Key::new(
                InstanceId::from_bytes([1; 16]),
                SourceKind::Mailbox,
                b"account",
                b"one",
            )
            .unwrap(),
            MailboxInput {
                name: "Folder",
                parent: None,
                role: None,
                sort_order: 0,
                subscribed: true,
            },
        )
        .unwrap();
        let ids = vec!["one".to_owned()];
        let candidates = vec![&value];
        let epoch = StoreEpoch::from_bytes([2; 16]);
        let account = AccountId::from_bytes([3; 16]);
        let entry = json!({"id":"one","status":"delete","local":"04".repeat(16),"oldDigest":hex(&value.digest),"changed":"0000000000000007"});
        let parsed = operations(
            &json!({"mailboxes":[entry.clone()]}),
            &ids,
            &candidates,
            epoch,
            account,
        )
        .unwrap();
        assert_eq!(parsed.first().unwrap().as_ref().unwrap().changed, 7);
        for plan in [
            json!({"mailboxes":[]}),
            json!({"mailboxes":[entry.clone(),entry]}),
            json!({"mailboxes":[{"id":"other","status":"has-child"}]}),
            json!({"mailboxes":[{"id":"one","status":"update"}]}),
            json!({"mailboxes":[{"id":"one","status":"delete","oldDigest":"00".repeat(32)}]}),
        ] {
            assert!(operations(&plan, &ids, &candidates, epoch, account).is_err());
        }
        for status in ["has-child", "has-email", "role-protected"] {
            assert!(operations(
                &json!({"mailboxes":[{"id":"one","status":status}]}),
                &ids,
                &candidates,
                epoch,
                account
            )
            .unwrap()
            .first()
            .unwrap()
            .is_none());
        }
    }
}
