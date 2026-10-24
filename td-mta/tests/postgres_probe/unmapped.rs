use super::imports::{email, folder, write};
use super::{deadline, sql, Fixture};
use std::{fs, os::unix::fs::PermissionsExt, path::Path, process::Command};
use td_crypto::{Digest, Sha256, TrustStore};
use td_json::{json, Json};
use td_mta::{
    ids::{AccountId, StoreEpoch},
    postgres_imports as db,
    postgres_probe::Request,
};

fn state(fixture: &Fixture) -> String {
    sql(
        fixture,
        "SELECT encode(sequence,'hex'),body_bytes,blob_count,(SELECT count(*) FROM td_mta.imports),(SELECT count(*) FROM td_mta.changes) FROM td_mta.accounts JOIN td_mta.account_usage ON account=id",
    )
}
pub fn exercise(
    fixture: &Fixture,
    request: &Request<'_>,
    trust: &TrustStore,
    epoch: StoreEpoch,
    connection: &Path,
) {
    let account = AccountId::from_bytes([0xaa; 16]);
    let authority = db::prepare(request, trust, deadline(), epoch, account).unwrap();
    let folders = vec![
        folder("inbox", "Inbox", None, Some("inbox")),
        folder("child", "Nested", Some(b"inbox"), None),
    ];
    for value in &folders {
        assert!(
            db::publish_unmapped_mailbox(request, trust, deadline(), &authority, value, &folders)
                .unwrap()
                .recorded
        );
    }
    let body = b"Message-ID: <unmapped@example.test>\r\n\r\noriginal";
    let old = email("old", body, &["$seen"]);
    let gone = email("gone", body, &["$seen"]);
    for value in [&old, &gone] {
        db::publish_email(request, trust, deadline(), &authority, value, body).unwrap();
    }
    let changed = email("old", body, &["$flagged"]);
    assert_eq!(
        db::publish_email(request, trust, deadline(), &authority, &changed, body),
        Err(db::Error::SourceChanged)
    );
    let before = state(fixture);
    let skipped = db::publish_unmapped_email(
        request,
        trust,
        deadline(),
        &authority,
        &changed,
        b"",
        &folders,
    )
    .unwrap();
    assert!(!skipped.recorded && !skipped.created && skipped.present);
    assert_eq!(before, state(fixture));
    sql(
        fixture,
        "DELETE FROM td_mta.memberships WHERE email_id=(SELECT local_object FROM td_mta.imports WHERE source_object=convert_to('gone','UTF8')); DELETE FROM td_mta.keywords WHERE email_id=(SELECT local_object FROM td_mta.imports WHERE source_object=convert_to('gone','UTF8')); DELETE FROM td_mta.thread_anchors WHERE email_id=(SELECT local_object FROM td_mta.imports WHERE source_object=convert_to('gone','UTF8')); DELETE FROM td_mta.emails WHERE id=(SELECT local_object FROM td_mta.imports WHERE source_object=convert_to('gone','UTF8'))",
    );
    let changed_gone = email("gone", body, &["$flagged"]);
    let before = state(fixture);
    let skipped = db::publish_unmapped_email(
        request,
        trust,
        deadline(),
        &authority,
        &changed_gone,
        b"",
        &folders,
    )
    .unwrap();
    assert!(!skipped.recorded && !skipped.present);
    assert_eq!(before, state(fixture));
    let new = email("direct", body, &[]);
    let child = folder("new-child", "New child", Some(b"child"), None);
    sql(
        fixture,
        "UPDATE td_mta.mailboxes SET name='Local rename' WHERE role IS NULL",
    );
    let before = state(fixture);
    assert_eq!(
        db::publish_unmapped_email(request, trust, deadline(), &authority, &new, body, &folders),
        Err(db::Error::Conflict)
    );
    assert_eq!(
        db::publish_unmapped_mailbox(request, trust, deadline(), &authority, &child, &folders),
        Err(db::Error::Conflict)
    );
    assert_eq!(before, state(fixture));
    // Mapped folders are skips, but they cannot authorize dependent creation.
    assert!(
        !db::publish_unmapped_mailbox(
            request,
            trust,
            deadline(),
            &authority,
            &folders[1],
            &folders
        )
        .unwrap()
        .recorded
    );
    sql(
        fixture,
        "UPDATE td_mta.mailboxes SET name='Nested' WHERE role IS NULL",
    );
    assert!(
        !db::publish_unmapped_mailbox(
            request,
            trust,
            deadline(),
            &authority,
            &folder("child", "Source rename", Some(b"inbox"), None),
            &folders
        )
        .unwrap()
        .recorded
    );
    sql(
        fixture,
        "UPDATE td_mta.imports SET source_object=convert_to('missing-child','UTF8') WHERE source_object=convert_to('child','UTF8')",
    );
    assert_eq!(
        db::publish_unmapped_email(request, trust, deadline(), &authority, &new, body, &folders),
        Err(db::Error::MissingMailbox)
    );
    assert_eq!(
        db::publish_unmapped_mailbox(request, trust, deadline(), &authority, &child, &folders),
        Err(db::Error::MissingMailbox)
    );
    sql(
        fixture,
        "UPDATE td_mta.imports SET source_object=convert_to('child','UTF8') WHERE source_object=convert_to('missing-child','UTF8')",
    );
    sql(
        fixture,
        "INSERT INTO td_mta.imports SELECT account,source_instance,source_kind,source_account,convert_to('alias','UTF8'),local_object,historical_blob,source_digest,changed FROM td_mta.imports WHERE source_object=convert_to('child','UTF8')",
    );
    let aliased = state(fixture);
    assert_eq!(
        db::publish_unmapped_email(request, trust, deadline(), &authority, &new, body, &folders),
        Err(db::Error::Conflict)
    );
    assert_eq!(
        db::publish_unmapped_mailbox(request, trust, deadline(), &authority, &child, &folders),
        Err(db::Error::Conflict)
    );
    assert_eq!(aliased, state(fixture));
    sql(
        fixture,
        "DELETE FROM td_mta.imports WHERE source_object=convert_to('alias','UTF8')",
    );
    let drifted = vec![
        folder("inbox", "Source rename", None, Some("inbox")),
        folders[1].clone(),
    ];
    assert_eq!(
        db::publish_unmapped_email(request, trust, deadline(), &authority, &new, body, &drifted),
        Err(db::Error::Conflict)
    );
    assert_eq!(
        db::publish_unmapped_mailbox(request, trust, deadline(), &authority, &child, &drifted),
        Err(db::Error::Conflict)
    );
    assert_eq!(before, state(fixture));
    assert!(
        db::publish_unmapped_email(request, trust, deadline(), &authority, &new, body, &folders)
            .unwrap()
            .recorded
    );
    let after = state(fixture);
    assert!(
        !db::publish_unmapped_email(request, trust, deadline(), &authority, &new, body, &folders)
            .unwrap()
            .recorded
    );
    assert_eq!(after, state(fixture));
    let race = email("race", body, &[]);
    std::thread::scope(|scope| {
        let first = scope.spawn(|| {
            db::publish_unmapped_email(
                request,
                trust,
                deadline(),
                &authority,
                &race,
                body,
                &folders,
            )
            .unwrap()
        });
        let second = db::publish_unmapped_email(
            request,
            trust,
            deadline(),
            &authority,
            &race,
            body,
            &folders,
        )
        .unwrap();
        let first = first.join().unwrap();
        assert_eq!(first.local_object, second.local_object);
        assert_ne!(first.recorded, second.recorded);
    });
    use super::imports::key;
    use td_mta::format::key::SourceKind;
    let keys = [
        key("old", SourceKind::Email),
        key("gone", SourceKind::Email),
        key("not-mapped", SourceKind::Email),
        key("old", SourceKind::Email),
    ];
    let outcomes = db::inspect_mappings(request, trust, deadline(), &authority, &keys).unwrap();
    assert!(outcomes[0].unwrap().present);
    assert!(!outcomes[1].unwrap().present);
    assert!(outcomes[2].is_none());
    assert_eq!(outcomes[0], outcomes[3]);
    assert_eq!(
        db::inspect_mappings(request, trust, deadline(), &authority, &[]),
        Err(db::Error::Invalid)
    );
    assert_eq!(
        db::inspect_mappings(
            request,
            trust,
            deadline(),
            &authority,
            &vec![keys[0].clone(); db::MAX_MAPPING_BATCH + 1]
        ),
        Err(db::Error::Invalid)
    );
    assert_eq!(
        db::inspect_mappings(
            request,
            trust,
            deadline(),
            &authority,
            &[keys[0].clone(), key("inbox", SourceKind::Mailbox)]
        ),
        Err(db::Error::Invalid)
    );
    cli(fixture, epoch, account, connection, body);
    sql(fixture, "UPDATE td_mta.runtime_barrier SET blocked=true");
    assert_eq!(
        db::publish_unmapped_email(
            request,
            trust,
            deadline(),
            &authority,
            &email("blocked", body, &[]),
            body,
            &folders
        ),
        Err(db::Error::Policy)
    );
    sql(
        fixture,
        "UPDATE td_mta.runtime_barrier SET blocked=false; UPDATE td_mta.config_selection SET generation=decode('0000000000000002','hex')",
    );
    assert_eq!(
        db::publish_unmapped_email(
            request,
            trust,
            deadline(),
            &authority,
            &email("stale", body, &[]),
            body,
            &folders
        ),
        Err(db::Error::Policy)
    );
}
fn hash(bytes: &[u8]) -> String {
    let mut digest = Sha256::try_new().unwrap();
    digest.update(bytes).unwrap();
    digest
        .finish()
        .unwrap()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
fn cli(
    fixture: &Fixture,
    epoch: StoreEpoch,
    account: AccountId,
    connection: &Path,
    old_body: &[u8],
) {
    let archive = td_fs::TempDir::new("unmapped-cli").unwrap();
    fs::set_permissions(archive.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let new_body = b"Message-ID: <new-unmapped@example.test>\r\n\r\nnew";
    let old_hash = hash(old_body);
    let new_hash = hash(new_body);
    let source = json!({"version":1,"instance":"05050505050505050505050505050505","session":"https://source.example.test/jmap","account":"source","username":"source"});
    let record = |id: &str, body: &[u8], boxes: Json, keywords: Json| json!({"email":{"id":id,"blobId":id,"size":body.len(),"receivedAt":"2024-01-02T03:04:05.123Z","mailboxIds":boxes,"keywords":keywords},"body":hash(body)});
    let boxes = json!({"inbox":true,"child":true});
    let manifest = |invalid: bool| json!({"version":1,"provisional":true,"coverage":"per-mailbox-query","source":source.clone(),"mailboxState":"m","emailState":"e","mailboxes":[{"id":"inbox","name":"Inbox","parentId":null,"role":"inbox","sortOrder":0,"isSubscribed":true},{"id":"child","name":"Nested","parentId":"inbox","role":null,"sortOrder":0,"isSubscribed":true},{"id":"newbox","name":"New box","parentId":"child","role":null,"sortOrder":0,"isSubscribed":true}],"messages":[record("old",old_body,boxes.clone(),if invalid {json!({"A":true,"a":true})}else{json!({"$flagged":true})}),record("gone",old_body,boxes.clone(),json!({"$flagged":true})),record("new-cli",new_body,json!({"newbox":true}),json!({"$seen":true}))]});
    write(&archive.join("source.json"), source.to_string().as_bytes());
    write(
        &archive.join("manifest.json"),
        manifest(false).to_string().as_bytes(),
    );
    write(&archive.join(format!("{old_hash}.eml")), old_body);
    write(&archive.join(format!("{new_hash}.eml")), new_body);
    let invoke = |only: bool| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_td-mta"));
        command
            .args(["migration", "import", "--archive"])
            .arg(archive.path())
            .args(["--max-bytes", "33554432", "--connection-file"])
            .arg(connection)
            .args([
                "--epoch",
                &epoch.to_string(),
                "--account",
                &account.to_string(),
            ]);
        if only {
            command.arg("--only-unmapped");
        }
        command.output().unwrap()
    };
    let ordinary = invoke(false);
    assert!(!ordinary.status.success());
    assert_eq!(ordinary.stderr, b"migration-source-changed\n");
    // The ordinary importer already established the new folder before stopping.
    fs::remove_file(archive.join(format!("{old_hash}.eml"))).unwrap();
    write(
        &archive.join("manifest.json"),
        manifest(true).to_string().as_bytes(),
    );
    write(&archive.join(format!("{new_hash}.eml")), b"corrupt");
    let before = state(fixture);
    let corrupt = invoke(true);
    assert!(!corrupt.status.success());
    assert_eq!(corrupt.stderr, b"migration-body-digest\n");
    assert_eq!(before, state(fixture));
    write(&archive.join(format!("{new_hash}.eml")), new_body);
    let result = invoke(true);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let result = td_json::parse_slice(&result.stdout).unwrap();
    assert_eq!(result["createdMessages"].as_u64(), Some(1));
    assert_eq!(result["historicalObjectsAbsent"].as_u64(), Some(1));
    assert_eq!(result["skippedMappedMessages"].as_u64(), Some(2));
    let after = state(fixture);
    let result = invoke(true);
    assert!(result.status.success());
    let result = td_json::parse_slice(&result.stdout).unwrap();
    assert_eq!(result["createdMessages"].as_u64(), Some(0));
    assert_eq!(result["skippedMappedMessages"].as_u64(), Some(3));
    assert_eq!(after, state(fixture));
    assert_eq!(
        sql(
            fixture,
            "SELECT count(*) FROM td_mta.emails WHERE id=(SELECT local_object FROM td_mta.imports WHERE source_object=convert_to('gone','UTF8'))"
        ),
        "0"
    );
    assert_eq!(
        sql(
            fixture,
            "SELECT keyword FROM td_mta.keywords WHERE email_id=(SELECT local_object FROM td_mta.imports WHERE source_object=convert_to('old','UTF8'))"
        ),
        "$seen"
    );
    // The CLI also creates a new child while skipping all mapped Emails.
    let mut extended = manifest(true);
    if let Json::Obj(fields) = &mut extended {
        let boxes = fields
            .iter_mut()
            .find(|(name, _)| name == "mailboxes")
            .unwrap();
        if let Json::Arr(values) = &mut boxes.1 {
            values.push(json!({"id":"fresh-child","name":"Fresh child","parentId":"newbox","role":null,"sortOrder":0,"isSubscribed":true}));
        } else {
            panic!("mailbox array");
        }
    }
    write(
        &archive.join("manifest.json"),
        extended.to_string().as_bytes(),
    );
    let result = invoke(true);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let result = td_json::parse_slice(&result.stdout).unwrap();
    assert_eq!(result["createdMailboxes"].as_u64(), Some(1));
    assert_eq!(result["skippedMappedMessages"].as_u64(), Some(3));
    // A mostly mapped archive must not establish one connection per message.
    sql(
        fixture,
        "INSERT INTO td_mta.imports SELECT account,source_instance,source_kind,source_account,convert_to('scale-'||n,'UTF8'),decode(md5('scale-'||n),'hex'),historical_blob,source_digest,changed FROM td_mta.imports CROSS JOIN generate_series(1,32768) n WHERE source_object=convert_to('old','UTF8')",
    );
    let mut scaled = extended.clone();
    if let Json::Obj(fields) = &mut scaled {
        let messages = &mut fields
            .iter_mut()
            .find(|(name, _)| name == "messages")
            .unwrap()
            .1;
        if let Json::Arr(values) = messages {
            for index in 1..=32768 {
                values.push(record(
                    &format!("scale-{index}"),
                    old_body,
                    boxes.clone(),
                    json!({"$seen":true}),
                ));
            }
            for index in 1..=43 {
                values.push(record(
                    &format!("addition-{index}"),
                    new_body,
                    json!({"newbox":true}),
                    json!({"$seen":true}),
                ));
            }
        } else {
            panic!("message array");
        }
    }
    write(
        &archive.join("manifest.json"),
        scaled.to_string().as_bytes(),
    );
    let connections = || {
        sql(
            fixture,
            "SELECT sessions FROM pg_stat_database WHERE datname=current_database()",
        )
        .parse::<u64>()
        .unwrap()
    };
    let before = connections();
    let started = std::time::Instant::now();
    let result = invoke(true);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let used = connections() - before;
    eprintln!("mostly mapped import: 32768 historical mappings + 43 additions, {used} connections, {:.2}s", started.elapsed().as_secs_f64());
    assert!(
        used > 0 && used < 300,
        "mostly mapped import opened {used} connections"
    );
    let result = td_json::parse_slice(&result.stdout).unwrap();
    assert_eq!(result["createdMessages"].as_u64(), Some(43));
    assert_eq!(result["skippedMappedMessages"].as_u64(), Some(32771));
    assert_eq!(result["historicalObjectsAbsent"].as_u64(), Some(32769));
}
