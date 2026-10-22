use super::imports::{email, folder, key};
use super::{deadline, sql, Fixture};
use td_crypto::TrustStore;
use td_mta::{
    format::key::SourceKind,
    ids::{AccountId, StoreEpoch},
    import_snapshot::{Email, EmailInput},
    postgres_imports::{
        self as db,
        reconcile::{self, Applied, Baseline, Plan},
    },
    postgres_probe::Request,
};
fn update(plan: Plan) -> Baseline {
    match plan {
        Plan::Update(value) => *value,
        _ => panic!("expected update plan"),
    }
}
pub fn exercise(
    fixture: &Fixture,
    request: &Request<'_>,
    trust: &TrustStore,
    epoch: StoreEpoch,
    connection: &std::path::Path,
) {
    let account = AccountId::from_bytes([0xaa; 16]);
    let authority = db::prepare(request, trust, deadline(), epoch, account).unwrap();
    let folders = vec![
        folder("inbox", "Inbox", None, Some("inbox")),
        folder("child", "Nested", Some(b"inbox"), None),
    ];
    for value in &folders {
        db::publish_mailbox(request, trust, deadline(), &authority, value).unwrap();
    }
    let raw = b"Message-ID: <reconcile@example.test>\r\nSubject: untouched\r\n\r\nraw\0bytes\xff";
    let initial = email("one", raw, &["$seen", "custom"]);
    let desired = email("one", raw, &["$flagged", "custom"]);
    let plan = |value: &Email| {
        reconcile::plan(request, trust, deadline(), &authority, value, &folders).unwrap()
    };
    let apply = |value: &Email, baseline: &Baseline| {
        reconcile::apply(
            request,
            trust,
            deadline(),
            &authority,
            value,
            &folders,
            baseline,
        )
    };
    assert!(matches!(plan(&initial), Plan::New));
    db::publish_email(request, trust, deadline(), &authority, &initial, raw).unwrap();
    let invariant=sql(fixture,"SELECT encode(e.blob_id,'hex'),encode(e.thread_id,'hex'),e.received_at,encode(b.digest,'hex'),b.length,u.body_bytes,u.blob_count FROM td_mta.emails e JOIN td_mta.blobs b ON b.account=e.account AND b.id=e.blob_id JOIN td_mta.account_usage u ON u.account=e.account");
    let before = sql(
        fixture,
        "SELECT encode(sequence,'hex'),(SELECT count(*) FROM td_mta.changes) FROM td_mta.accounts",
    );
    assert!(matches!(plan(&initial), Plan::Current));
    let retained = update(plan(&desired));
    assert_eq!(before,sql(fixture,"SELECT encode(sequence,'hex'),(SELECT count(*) FROM td_mta.changes) FROM td_mta.accounts"));
    assert_eq!(apply(&desired, &retained), Ok(Applied::Updated));
    assert_eq!(invariant,sql(fixture,"SELECT encode(e.blob_id,'hex'),encode(e.thread_id,'hex'),e.received_at,encode(b.digest,'hex'),b.length,u.body_bytes,u.blob_count FROM td_mta.emails e JOIN td_mta.blobs b ON b.account=e.account AND b.id=e.blob_id JOIN td_mta.account_usage u ON u.account=e.account"));
    assert_eq!(
        sql(
            fixture,
            "SELECT string_agg(keyword,',' ORDER BY keyword) FROM td_mta.keywords"
        ),
        "$flagged,custom"
    );
    let after = sql(
        fixture,
        "SELECT encode(sequence,'hex'),(SELECT count(*) FROM td_mta.changes) FROM td_mta.accounts",
    );
    assert_eq!(apply(&desired, &retained), Ok(Applied::CurrentReceipt));
    assert_eq!(after,sql(fixture,"SELECT encode(sequence,'hex'),(SELECT count(*) FROM td_mta.changes) FROM td_mta.accounts"));
    // A historical receipt never resets later local edits.
    sql(
        fixture,
        "UPDATE td_mta.keywords SET keyword='local' WHERE keyword='custom'",
    );
    assert_eq!(apply(&desired, &retained), Ok(Applied::CurrentReceipt));
    assert!(matches!(plan(&initial), Plan::LocalChanged));
    assert_eq!(
        sql(
            fixture,
            "SELECT count(*) FROM td_mta.keywords WHERE keyword='local'"
        ),
        "1"
    );
    sql(
        fixture,
        "UPDATE td_mta.keywords SET keyword='custom' WHERE keyword='local'",
    );
    let back = update(plan(&initial));
    assert_eq!(apply(&initial, &back), Ok(Applied::Updated));
    // Same old source digest, later receipt sequence: reject stale ABA transition.
    assert_eq!(apply(&desired, &retained), Err(db::Error::Conflict));
    let fresh = update(plan(&desired));
    sql(
        fixture,
        "UPDATE td_mta.keywords SET keyword='local' WHERE keyword='custom'",
    );
    assert_eq!(apply(&desired, &fresh), Err(db::Error::Conflict));
    sql(
        fixture,
        "UPDATE td_mta.keywords SET keyword='custom' WHERE keyword='local'",
    );
    sql(fixture,"INSERT INTO td_mta.imports SELECT account,source_instance,source_kind,source_account,convert_to('alias','UTF8'),local_object,historical_blob,source_digest,changed FROM td_mta.imports WHERE source_object=convert_to('child','UTF8')");
    assert!(matches!(plan(&desired), Plan::LocalChanged));
    assert_eq!(apply(&desired, &fresh), Err(db::Error::Conflict));
    sql(
        fixture,
        "DELETE FROM td_mta.imports WHERE source_object=convert_to('alias','UTF8')",
    );
    sql(fixture,"UPDATE td_mta.imports SET source_instance=decode(repeat('07',16),'hex') WHERE source_object=convert_to('child','UTF8')");
    assert!(matches!(plan(&desired), Plan::LocalChanged));
    sql(fixture,"UPDATE td_mta.imports SET source_instance=decode(repeat('05',16),'hex') WHERE source_object=convert_to('child','UTF8')");
    sql(fixture,"UPDATE td_mta.imports SET historical_blob=decode(repeat('00',16),'hex') WHERE source_kind=3");
    assert!(matches!(plan(&desired), Plan::LocalChanged));
    assert_eq!(apply(&desired, &fresh), Err(db::Error::Conflict));
    sql(fixture,"UPDATE td_mta.imports SET historical_blob=(SELECT blob_id FROM td_mta.emails) WHERE source_kind=3");

    let mut wrong = fresh.clone();
    wrong.historical_blob = [0; 16];
    assert_eq!(apply(&desired, &wrong), Err(db::Error::Conflict));
    let mut wrong = fresh.clone();
    wrong.epoch = StoreEpoch::from_bytes([0; 16]);
    assert_eq!(apply(&desired, &wrong), Err(db::Error::Invalid));
    let mut wrong = fresh.clone();
    wrong.account = AccountId::from_bytes([0; 16]);
    assert_eq!(apply(&desired, &wrong), Err(db::Error::Invalid));
    let mut wrong = fresh.clone();
    wrong.key = key("other", SourceKind::Email);
    assert_eq!(apply(&desired, &wrong), Err(db::Error::Invalid));
    sql(
        fixture,
        "UPDATE td_mta.mailboxes SET name='Local rename' WHERE role IS NULL",
    );
    assert!(matches!(plan(&desired), Plan::FolderChanged));
    assert_eq!(apply(&desired, &fresh), Err(db::Error::Conflict));
    sql(
        fixture,
        "UPDATE td_mta.mailboxes SET name='Nested' WHERE role IS NULL",
    );
    sql(
        fixture,
        "UPDATE td_mta.mailboxes SET name='Ancestor rename' WHERE role='inbox'",
    );
    assert!(matches!(plan(&desired), Plan::FolderChanged));
    sql(
        fixture,
        "UPDATE td_mta.mailboxes SET name='Inbox' WHERE role='inbox'",
    );
    assert!(matches!(
        plan(&email("one", b"other body", &[])),
        Plan::ImmutableChanged
    ));
    sql(fixture, "UPDATE td_mta.emails SET origin=4");
    assert!(matches!(plan(&desired), Plan::LocalChanged));
    assert_eq!(apply(&desired, &fresh), Err(db::Error::Conflict));
    sql(fixture, "UPDATE td_mta.emails SET origin=3");
    sql(fixture, "UPDATE td_mta.runtime_barrier SET blocked=true");
    assert_eq!(apply(&desired, &fresh), Err(db::Error::Policy));
    sql(fixture, "UPDATE td_mta.runtime_barrier SET blocked=false");
    assert_eq!(apply(&desired, &fresh), Ok(Applied::Updated));
    // Removing one membership is allowed, while blob/thread ownership stays fixed.
    let mut hash = td_crypto::Sha256::try_new().unwrap();
    use td_crypto::Digest;
    hash.update(raw).unwrap();
    let moved = Email::new(
        key("one", SourceKind::Email),
        EmailInput {
            body_digest: hash.finish().unwrap(),
            length: raw.len() as u64,
            received_at: 1704164645123,
            mailboxes: &[b"child"],
            keywords: &["$seen"],
        },
    )
    .unwrap();
    let movement = update(plan(&moved));
    assert_eq!(apply(&moved, &movement), Ok(Applied::Updated));
    assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.memberships"), "1");
    assert_eq!(sql(fixture,"SELECT count(*) FROM td_mta.changes WHERE sequence=(SELECT sequence FROM td_mta.accounts) AND kind=1"),"2");
    let retained = update(plan(&initial));
    let before_sequence = u64::from_str_radix(
        &sql(
            fixture,
            "SELECT encode(sequence,'hex') FROM td_mta.accounts",
        ),
        16,
    )
    .unwrap();
    sql(fixture,"CREATE FUNCTION public.delay_reconcile() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_sleep(11); RETURN NEW; END $$; CREATE CONSTRAINT TRIGGER delay_reconcile AFTER UPDATE ON td_mta.imports DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.delay_reconcile()");
    assert_eq!(apply(&initial, &retained), Err(db::Error::Indeterminate));
    sql(fixture, "DROP TRIGGER delay_reconcile ON td_mta.imports");
    assert!(matches!(
        apply(&initial, &retained),
        Ok(Applied::Updated | Applied::CurrentReceipt)
    ));
    assert_eq!(
        u64::from_str_radix(
            &sql(
                fixture,
                "SELECT encode(sequence,'hex') FROM td_mta.accounts"
            ),
            16
        )
        .unwrap(),
        before_sequence + 1
    );
    let absent_plan = update(plan(&moved));
    sql(fixture,"BEGIN; DELETE FROM td_mta.keywords; DELETE FROM td_mta.memberships; DELETE FROM td_mta.thread_anchors; DELETE FROM td_mta.emails; COMMIT");
    assert_eq!(apply(&moved, &absent_plan), Ok(Applied::Absent));
    assert!(matches!(plan(&initial), Plan::Absent));
    assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.emails"), "0");
    cli(fixture, connection, epoch, account);
    bounds(fixture, request, trust, &authority, folders);
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn cli(fixture: &Fixture, connection: &std::path::Path, epoch: StoreEpoch, account: AccountId) {
    use super::imports::write;
    use std::{fs, os::unix::fs::PermissionsExt, process::Command};
    use td_crypto::Digest;
    use td_json::{json, Json};
    let archive = td_fs::TempDir::new("reconcile-cli-archive").unwrap();
    let control = td_fs::TempDir::new("reconcile-cli-plan").unwrap();
    for root in [archive.path(), control.path()] {
        fs::set_permissions(root, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let raw = b"Message-ID: <cli-reconcile@example.test>\r\n\r\nraw";
    let mut hash = td_crypto::Sha256::try_new().unwrap();
    hash.update(raw).unwrap();
    let body = hex(&hash.finish().unwrap());
    let source = json!({"version":1,"instance":"05050505050505050505050505050505","session":"https://source.example.test/jmap","account":"source","username":"source"});
    let manifest = |keywords: Json, memberships: Json| json!({"version":1,"provisional":true,"coverage":"per-mailbox-query","source":source.clone(),"mailboxState":"m","emailState":"e","mailboxes":[{"id":"inbox","name":"Inbox","parentId":null,"role":"inbox","sortOrder":0,"isSubscribed":true},{"id":"child","name":"Nested","parentId":"inbox","role":null,"sortOrder":0,"isSubscribed":true}],"messages":[{"email":{"id":"cli-reconcile","blobId":"raw","size":raw.len(),"receivedAt":"2024-01-02T03:04:05.123Z","mailboxIds":memberships,"keywords":keywords},"body":body.clone()}]});
    write(&archive.join("source.json"), source.to_string().as_bytes());
    write(&archive.join(format!("{body}.eml")), raw);
    let invoke = |action: &str, plan: bool| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_td-mta"));
        command
            .env_clear()
            .args(["migration", action, "--archive"])
            .arg(archive.path())
            .args(["--max-bytes", "1048576", "--connection-file"])
            .arg(connection)
            .args([
                "--epoch",
                &epoch.to_string(),
                "--account",
                &account.to_string(),
            ]);
        if plan {
            command.arg("--plan-file").arg(control.join("plan.json"));
        }
        command.output().unwrap()
    };
    let original = manifest(json!({"$seen":true}), json!({"inbox":true,"child":true}));
    write(
        &archive.join("manifest.json"),
        original.to_string().as_bytes(),
    );
    let imported = invoke("import", false);
    assert!(
        imported.status.success(),
        "{}",
        String::from_utf8_lossy(&imported.stderr)
    );
    let desired = manifest(
        json!({"$flagged":true,"custom":true}),
        json!({"child":true}),
    );
    write(
        &archive.join("manifest.json"),
        desired.to_string().as_bytes(),
    );
    let planned = invoke("plan", false);
    assert!(
        planned.status.success(),
        "{}",
        String::from_utf8_lossy(&planned.stderr)
    );
    let plan = td_json::parse_slice(&planned.stdout).unwrap();
    assert_eq!(plan["messages"][0]["status"].as_str(), Some("update"));
    write(&control.join("plan.json"), &planned.stdout);
    // A changed desired archive cannot silently retarget a retained plan.
    let before = sql(
        fixture,
        "SELECT encode(sequence,'hex') FROM td_mta.accounts",
    );
    write(
        &archive.join("manifest.json"),
        original.to_string().as_bytes(),
    );
    let refused = invoke("apply", true);
    assert!(!refused.status.success());
    assert_eq!(refused.stderr, b"migration-plan-binding\n");
    assert_eq!(
        before,
        sql(
            fixture,
            "SELECT encode(sequence,'hex') FROM td_mta.accounts"
        )
    );
    write(
        &archive.join("manifest.json"),
        desired.to_string().as_bytes(),
    );
    let applied = invoke("apply", true);
    assert!(
        applied.status.success(),
        "{}",
        String::from_utf8_lossy(&applied.stderr)
    );
    assert_eq!(
        td_json::parse_slice(&applied.stdout).unwrap()["updated"].as_u64(),
        Some(1)
    );
    let repeated = invoke("apply", true);
    assert!(repeated.status.success());
    assert_eq!(
        td_json::parse_slice(&repeated.stdout).unwrap()["currentReceipts"].as_u64(),
        Some(1)
    );
    let observed = invoke("plan", false);
    assert!(observed.status.success());
    assert_eq!(
        td_json::parse_slice(&observed.stdout).unwrap()["messages"][0]["status"].as_str(),
        Some("matches-at-observation")
    );
    fs::set_permissions(control.join("plan.json"), fs::Permissions::from_mode(0o644)).unwrap();
    let public = invoke("apply", true);
    assert!(!public.status.success());
    assert_eq!(public.stderr, b"migration-plan-file\n");
}
fn bounds(
    fixture: &Fixture,
    request: &Request<'_>,
    trust: &TrustStore,
    authority: &db::Authority,
    mut folders: Vec<td_mta::import_snapshot::Mailbox>,
) {
    use td_crypto::Digest;
    let mut sources = vec!["inbox".to_owned(), "child".to_owned()];
    let mut parent = "child".to_owned();
    let mut batch = String::new();
    for n in 0..254u128 {
        let source = format!("s{n:03}{}", "x".repeat(896));
        let name = format!("box{n:03}{}", "x".repeat(1018));
        let value = folder(&source, &name, Some(parent.as_bytes()), None);
        let local = hex(&(n + 1000).to_be_bytes());
        let object = hex(source.as_bytes());
        let parent_hex = hex(parent.as_bytes());
        let digest = hex(value.digest());
        batch.push_str(&format!("INSERT INTO td_mta.mailboxes SELECT id,decode('{local}','hex'),'{name}',(SELECT local_object FROM td_mta.imports WHERE source_object=decode('{parent_hex}','hex')),NULL,0,1,sequence FROM td_mta.accounts; INSERT INTO td_mta.imports SELECT id,decode(repeat('05',16),'hex'),1,convert_to('source','UTF8'),decode('{object}','hex'),decode('{local}','hex'),NULL,decode('{digest}','hex'),sequence FROM td_mta.accounts;"));
        if batch.len() > 60 * 1024 {
            sql(fixture, &batch);
            batch.clear();
        }
        parent = source.clone();
        sources.push(source);
        folders.push(value);
    }
    if !batch.is_empty() {
        sql(fixture, &batch);
    }
    let keywords: Vec<String> = (0..256)
        .map(|n| format!("k{n:03}{}", "x".repeat(251)))
        .collect();
    let mailbox_refs: Vec<_> = sources.iter().map(|s| s.as_bytes()).collect();
    let keyword_refs: Vec<_> = keywords.iter().map(String::as_str).collect();
    let mut large = b"Message-ID: <capacity@example.test>\r\n\r\npayload".to_vec();
    large.resize(32 * 1024 * 1024, b'x');
    let body = large.as_slice();
    let mut hash = td_crypto::Sha256::try_new().unwrap();
    hash.update(body).unwrap();
    let digest = hash.finish().unwrap();
    let value = |flags: &[&str]| {
        Email::new(
            key("capacity", SourceKind::Email),
            EmailInput {
                body_digest: digest,
                length: body.len() as u64,
                received_at: 0,
                mailboxes: &mailbox_refs,
                keywords: flags,
            },
        )
        .unwrap()
    };
    let initial = value(&keyword_refs);
    assert!(
        db::publish_unmapped_email(
            request,
            trust,
            deadline(),
            authority,
            &initial,
            body,
            &folders
        )
        .unwrap()
        .recorded
    );
    let extra = "z".repeat(255);
    let mut flags = keyword_refs;
    flags[0] = &extra;
    let desired = value(&flags);
    let plan =
        update(reconcile::plan(request, trust, deadline(), authority, &desired, &folders).unwrap());
    assert_eq!(plan.folders.len(), 256);
    assert_eq!(
        reconcile::apply(
            request,
            trust,
            deadline(),
            authority,
            &desired,
            &folders,
            &plan
        ),
        Ok(Applied::Updated)
    );
    assert_eq!(
        sql(fixture, "SELECT count(*) FROM td_mta.memberships WHERE email_id=(SELECT local_object FROM td_mta.imports WHERE source_object=convert_to('capacity','UTF8'))"),
        "256"
    );
    assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.keywords WHERE email_id=(SELECT local_object FROM td_mta.imports WHERE source_object=convert_to('capacity','UTF8'))"), "256");
}
