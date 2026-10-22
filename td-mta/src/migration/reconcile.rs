use super::*;
use crate::{
    ids::{AccountId, InstanceId, StoreEpoch},
    postgres_imports::{
        self as database,
        reconcile::{self, Applied, Baseline, FolderFence, Plan},
    },
};
pub(super) fn run(
    archive: &archive::Archive,
    connection: &str,
    epoch: &str,
    account: &str,
    plan_file: Option<&str>,
) -> Result<Json> {
    let epoch = StoreEpoch::parse(epoch).map_err(|_| "migration-invalid-epoch")?;
    let account = AccountId::parse(account).map_err(|_| "migration-invalid-account")?;
    let manifest = archive.snapshot()?;
    let source = field(&manifest, "source")?;
    let instance =
        InstanceId::parse(string(source, "instance")?).map_err(|_| "migration-invalid-source")?;
    let source_account = string(source, "account")?.as_bytes();
    let boxes = import::mailboxes(&manifest, instance, source_account)?;
    let messages = import::messages(&manifest, instance, source_account)?;
    let manifest_digest = digest(manifest.to_string().as_bytes())?;
    let retained = plan_file.map(read_plan).transpose()?;
    if let Some(plan) = &retained {
        if integer(plan, "format")? != 1
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
        .map(|p| operations(p, &messages, epoch, account))
        .transpose()?;
    if operations.is_some() {
        let mut capacity = 0usize;
        for (value, _, _) in &messages {
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
            let (value, _, _) = messages.get(index).ok_or("migration-plan-object")?;
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
            json!({"status":"applied-provisional-metadata","updated":updated,"currentReceipts":current,"historicalObjectsAbsent":absent,"skippedPlanEntries":skipped,"messages":output,"manifestSha256":manifest_digest,"cutoverReady":false}),
        );
    }
    for (value, _, _) in &messages {
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
            Plan::ImmutableChanged => json!({"id":id,"status":"immutable-source-change"}),
            Plan::FolderChanged => json!({"id":id,"status":"folder-conflict"}),
            Plan::Update(baseline) => {
                let mut fences = Vec::new();
                for fence in baseline.folders {
                    let source =
                        std::str::from_utf8(&fence.source).map_err(|_| "migration-invalid-id")?;
                    fences.push(json!({"source":source,"local":hex(&fence.local)}));
                }
                json!({"id":id,"status":"update","local":hex(&baseline.local),"historicalBlob":hex(&baseline.historical_blob),"oldDigest":hex(&baseline.digest),"changed":format!("{:016x}",baseline.changed),"desiredDigest":hex(&baseline.desired),"folders":fences})
            }
        };
        append(&mut output, &mut size, item)?;
    }
    Ok(
        json!({"format":1,"status":"planned-provisional-metadata","epoch":hex(epoch.as_bytes()),"account":hex(account.as_bytes()),"source":source.clone(),"manifestSha256":manifest_digest,"messages":output,"cutoverReady":false}),
    )
}
fn append(output: &mut Vec<Json>, size: &mut usize, item: Json) -> Result<()> {
    reserve(size, &item)?;
    output.push(item);
    Ok(())
}
fn reserve(size: &mut usize, item: &Json) -> Result<()> {
    let id = string(item, "id")?;
    let report = json!({"id":id,"status":"skipped-retained-observation"});
    *size = size
        .checked_add(item.to_string().len().max(report.to_string().len()) + 1)
        .filter(|n| *n <= INDEX_LIMIT / 2)
        .ok_or("migration-plan-capacity")?;
    Ok(())
}
fn read_plan(path: &str) -> Result<Json> {
    use crate::{
        operator_files::{Inputs, Role},
        store_fs::PrivateRoot,
    };
    let parent = Path::new(path)
        .parent()
        .and_then(Path::to_str)
        .ok_or("migration-plan-file")?;
    let root = PrivateRoot::open(parent).map_err(|_| "migration-plan-file")?;
    let mut inputs = Inputs::new();
    inputs.bind(&root).map_err(|_| "migration-plan-file")?;
    parse(&read_bounded(
        inputs
            .open(path, Role::Secret)
            .map_err(|_| "migration-plan-file")?,
        INDEX_LIMIT,
    )?)
}
fn fixed<const N: usize>(text: &str) -> Result<[u8; N]> {
    if text.len() != N * 2
        || !text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err("migration-plan-field");
    }
    let mut result = [0; N];
    for (part, slot) in text
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .zip(result.iter_mut())
    {
        let text = std::str::from_utf8(part).map_err(|_| "migration-plan-field")?;
        *slot = u8::from_str_radix(text, 16).map_err(|_| "migration-plan-field")?;
    }
    Ok(result)
}
fn operations(
    plan: &Json,
    messages: &[(crate::import_snapshot::Email, String, u64)],
    epoch: StoreEpoch,
    account: AccountId,
) -> Result<Vec<(usize, Option<Baseline>)>> {
    let entries = array(plan, "messages")?;
    if entries.len() != messages.len() {
        return Err("migration-plan-objects");
    }
    let wanted: BTreeMap<_, _> = messages
        .iter()
        .enumerate()
        .map(|(n, (v, _, _))| (v.key.object.as_slice(), n))
        .collect();
    let mut seen = std::collections::BTreeSet::new();
    let mut output = Vec::new();
    for entry in entries {
        let id = string(entry, "id")?.as_bytes();
        if !seen.insert(id) {
            return Err("migration-plan-objects");
        }
        let index = *wanted.get(id).ok_or("migration-plan-objects")?;
        let (value, _, _) = messages.get(index).ok_or("migration-plan-objects")?;
        let baseline = match string(entry, "status")? {
            "unmapped"
            | "matches-at-observation"
            | "historical-object-absent"
            | "local-conflict"
            | "immutable-source-change"
            | "folder-conflict" => None,
            "update" => {
                let entries = array(entry, "folders")?;
                if entries.is_empty() || entries.len() > 256 {
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
                    historical_blob: fixed(string(entry, "historicalBlob")?)?,
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

fn code(error: database::Error) -> &'static str {
    match error {
        database::Error::Indeterminate => "migration-indeterminate-retry-same-plan",
        error => import::code(error),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::{
        format::key::SourceKind,
        import_snapshot::{Email, EmailInput, Key},
    };
    fn value(id: &str) -> (Email, String, u64) {
        (
            Email::new(
                Key::new(
                    InstanceId::from_bytes([1; 16]),
                    SourceKind::Email,
                    b"a",
                    id.as_bytes(),
                )
                .unwrap(),
                EmailInput {
                    body_digest: [0; 32],
                    length: 0,
                    received_at: 0,
                    mailboxes: &[b"box"],
                    keywords: &[],
                },
            )
            .unwrap(),
            "00".repeat(32),
            0,
        )
    }
    #[test]
    fn retained_operations_require_exact_unique_object_set() {
        let messages = vec![value("one"), value("two")];
        let epoch = StoreEpoch::from_bytes([2; 16]);
        let account = AccountId::from_bytes([3; 16]);
        let good = json!({"messages":[{"id":"two","status":"unmapped"},{"id":"one","status":"local-conflict"}]});
        assert_eq!(
            operations(&good, &messages, epoch, account).unwrap().len(),
            2
        );
        for bad in [
            json!({"messages":[{"id":"one","status":"unmapped"}]}),
            json!({"messages":[{"id":"one","status":"unmapped"},{"id":"one","status":"unmapped"}]}),
            json!({"messages":[{"id":"one","status":"delete"},{"id":"two","status":"unmapped"}]}),
            json!({"messages":[{"id":"one","status":"unmapped"},{"id":"foreign","status":"unmapped"}]}),
        ] {
            assert!(operations(&bad, &messages, epoch, account).is_err());
        }
    }
    #[test]
    fn update_plan_keeps_receipt_fields_and_rejects_retargeted_digest() {
        let messages = vec![value("one")];
        let epoch = StoreEpoch::from_bytes([2; 16]);
        let account = AccountId::from_bytes([3; 16]);
        let desired = hex(messages.first().unwrap().0.digest());
        let plan = |digest: &str, changed: &str, folders: Json| json!({"messages":[{"id":"one","status":"update","local":"04".repeat(16),"historicalBlob":"05".repeat(16),"oldDigest":"06".repeat(32),"desiredDigest":digest,"changed":changed,"folders":folders}]});
        let folders = json!([{"source":"box","local":"07".repeat(16)}]);
        let parsed = operations(
            &plan(&desired, "0000000000000123", folders.clone()),
            &messages,
            epoch,
            account,
        )
        .unwrap();
        let baseline = parsed.first().unwrap().1.as_ref().unwrap();
        assert_eq!(baseline.changed, 0x123);
        assert_eq!(baseline.local, [4; 16]);
        assert_eq!(baseline.historical_blob, [5; 16]);
        assert_eq!(baseline.digest, [6; 32]);
        for bad in [
            plan(&"ff".repeat(32), "0000000000000123", folders.clone()),
            plan(&desired, "123", folders.clone()),
            plan(&desired, "0000000000000123", json!([])),
            plan(
                &desired,
                "0000000000000123",
                json!([{"source":"box","local":"07".repeat(16)},{"source":"box","local":"08".repeat(16)}]),
            ),
        ] {
            assert!(operations(&bad, &messages, epoch, account).is_err());
        }
    }
}
