use super::*;
use crate::{
    ids::{AccountId, InstanceId, StoreEpoch},
    postgres_imports::mailbox_reconcile::{
        self as reconcile, Applied, Baseline, FolderFence, Plan,
    },
};
pub(super) fn run(
    archive: &archive::Archive,
    connection: &str,
    epoch: &str,
    account: &str,
    retained: Option<Json>,
) -> Result<Json> {
    let epoch = StoreEpoch::parse(epoch).map_err(|_| "migration-invalid-epoch")?;
    let account = AccountId::parse(account).map_err(|_| "migration-invalid-account")?;
    let manifest = archive.snapshot()?;
    let source = field(&manifest, "source")?;
    let instance =
        InstanceId::parse(string(source, "instance")?).map_err(|_| "migration-invalid-source")?;
    let source_account = string(source, "account")?.as_bytes();
    let boxes = import::mailboxes(&manifest, instance, source_account)?;
    let manifest_digest = digest(manifest.to_string().as_bytes())?;
    if let Some(plan) = &retained {
        if integer(plan, "format")? != 2
            || string(plan, "kind")? != "mailboxes"
            || string(plan, "epoch")? != hex(epoch.as_bytes())
            || string(plan, "account")? != hex(account.as_bytes())
            || string(plan, "manifestSha256")? != manifest_digest
            || field(plan, "source")?.to_string() != source.to_string()
        {
            return Err("migration-plan-binding");
        }
    }
    // Parse every retained operation before any write. Never refresh its fences.
    let operations = retained
        .as_ref()
        .map(|p| operations(p, &boxes, epoch, account))
        .transpose()?;
    if operations.is_some() {
        let mut capacity = 0usize;
        for value in &boxes {
            let id = std::str::from_utf8(&value.key.object).map_err(|_| "migration-invalid-id")?;
            reserve(
                &mut capacity,
                &json!({"id":id,"status":"skipped-retained-observation"}),
            )?;
        }
    }
    let (loaded, address, authority) = import::connect(connection, epoch, account)?;
    let request = loaded.bootstrap.request(address);
    let mut output = Vec::new();
    let mut size = 0usize;
    if let Some(operations) = operations {
        let mut updated = 0usize;
        let mut current = 0usize;
        let mut absent = 0usize;
        let mut skipped = 0usize;
        for (index, baseline) in operations {
            let value = boxes.get(index).ok_or("migration-plan-object")?;
            let id = std::str::from_utf8(&value.key.object).map_err(|_| "migration-invalid-id")?;
            let status = if let Some(baseline) = baseline {
                match reconcile::apply(
                    &request,
                    &loaded.trust,
                    import::deadline(),
                    &authority,
                    value,
                    &boxes,
                    &baseline,
                )
                .map_err(code)?
                {
                    Applied::Updated => {
                        updated += 1;
                        "updated"
                    }
                    Applied::CurrentReceipt => {
                        current += 1;
                        "current-receipt-matches"
                    }
                    Applied::Absent => {
                        absent += 1;
                        "historical-object-absent"
                    }
                }
            } else {
                skipped += 1;
                "skipped-retained-observation"
            };
            append(&mut output, &mut size, json!({"id":id,"status":status}))?;
        }
        return Ok(
            json!({"status":"applied-provisional-folder-metadata","updated":updated,"currentReceipts":current,"historicalObjectsAbsent":absent,"skippedPlanEntries":skipped,"mailboxes":output,"manifestSha256":manifest_digest,"cutoverReady":false}),
        );
    }
    for value in &boxes {
        let id = std::str::from_utf8(&value.key.object).map_err(|_| "migration-invalid-id")?;
        let item = match reconcile::plan(
            &request,
            &loaded.trust,
            import::deadline(),
            &authority,
            value,
            &boxes,
        )
        .map_err(import::code)?
        {
            Plan::New => json!({"id":id,"status":"unmapped"}),
            Plan::Current => json!({"id":id,"status":"matches-at-observation"}),
            Plan::Absent => json!({"id":id,"status":"historical-object-absent"}),
            Plan::LocalChanged => json!({"id":id,"status":"local-conflict"}),
            Plan::RoleChanged => json!({"id":id,"status":"role-source-change"}),
            Plan::FolderChanged => json!({"id":id,"status":"folder-conflict"}),
            Plan::Update(baseline) => {
                let mut fences = Vec::new();
                for fence in baseline.folders {
                    let source =
                        std::str::from_utf8(&fence.source).map_err(|_| "migration-invalid-id")?;
                    fences.push(json!({"source":source,"local":hex(&fence.local)}));
                }
                json!({"id":id,"status":"update","local":hex(&baseline.local),"oldDigest":hex(&baseline.digest),"changed":format!("{:016x}",baseline.changed),"desiredDigest":hex(&baseline.desired),"folders":fences})
            }
        };
        append(&mut output, &mut size, item)?;
    }
    Ok(
        json!({"format":2,"kind":"mailboxes","status":"planned-provisional-folder-metadata","epoch":hex(epoch.as_bytes()),"account":hex(account.as_bytes()),"source":source.clone(),"manifestSha256":manifest_digest,"mailboxes":output,"cutoverReady":false}),
    )
}
use super::reconcile::{append, code, fixed, reserve};

fn operations(
    plan: &Json,
    boxes: &[crate::import_snapshot::Mailbox],
    epoch: StoreEpoch,
    account: AccountId,
) -> Result<Vec<(usize, Option<Baseline>)>> {
    let entries = array(plan, "mailboxes")?;
    if entries.len() != boxes.len() {
        return Err("migration-plan-objects");
    }
    let wanted: BTreeMap<_, _> = boxes
        .iter()
        .enumerate()
        .map(|(n, v)| (v.key.object.as_slice(), n))
        .collect();
    let mut seen = std::collections::BTreeSet::new();
    let mut output = Vec::new();
    for entry in entries {
        let id = string(entry, "id")?.as_bytes();
        if !seen.insert(id) {
            return Err("migration-plan-objects");
        }
        let index = *wanted.get(id).ok_or("migration-plan-objects")?;
        let value = boxes.get(index).ok_or("migration-plan-objects")?;
        let baseline = match string(entry, "status")? {
            "unmapped"
            | "matches-at-observation"
            | "historical-object-absent"
            | "local-conflict"
            | "role-source-change"
            | "folder-conflict" => None,
            "update" => {
                let entries = array(entry, "folders")?;
                if entries.len() > 256 {
                    return Err("migration-plan-folders");
                }
                let mut folders = Vec::new();
                let mut unique = std::collections::BTreeSet::new();
                for fence in entries {
                    let source = string(fence, "source")?;
                    identifier(source)?;
                    if !unique.insert(source) {
                        return Err("migration-plan-folders");
                    }
                    folders.push(FolderFence {
                        source: source.as_bytes().to_vec(),
                        local: fixed(string(fence, "local")?)?,
                    });
                }
                let desired = fixed(string(entry, "desiredDigest")?)?;
                if desired != value.digest {
                    return Err("migration-plan-binding");
                }
                Some(Baseline {
                    epoch,
                    account,
                    key: value.key.clone(),
                    local: fixed(string(entry, "local")?)?,
                    digest: fixed(string(entry, "oldDigest")?)?,
                    changed: u64::from_be_bytes(fixed(string(entry, "changed")?)?),
                    desired,
                    folders,
                })
            }
            _ => return Err("migration-plan-operation"),
        };
        output.push((index, baseline));
    }
    Ok(output)
}
