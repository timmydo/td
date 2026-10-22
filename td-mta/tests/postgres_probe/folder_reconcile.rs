use super::imports::{email, folder, key, write};
use super::{deadline, sql, Fixture};
use td_crypto::TrustStore;
use td_mta::{
    format::key::SourceKind,
    ids::{AccountId, StoreEpoch},
    import_snapshot::{Mailbox, MailboxInput},
    postgres_imports::{
        self as db,
        mailbox_reconcile::{self as reconcile, Applied, Baseline, Plan},
    },
    postgres_probe::Request,
};
fn update(plan: Plan) -> Baseline {
    match plan {
        Plan::Update(value) => *value,
        _ => panic!("expected update"),
    }
}
fn state(f: &Fixture) -> String {
    sql(
        f,
        "SELECT encode(sequence,'hex'),(SELECT count(*) FROM td_mta.changes) FROM td_mta.accounts",
    )
}
fn untouched(f: &Fixture) -> String {
    sql(f,"SELECT (SELECT string_agg(row(e.*)::text,',' ORDER BY id) FROM td_mta.emails e),(SELECT string_agg(row(m.*)::text,',' ORDER BY email_id,mailbox_id) FROM td_mta.memberships m),(SELECT string_agg(row(k.*)::text,',' ORDER BY email_id,keyword) FROM td_mta.keywords k),(SELECT string_agg(row(b.*)::text,',' ORDER BY id) FROM td_mta.blobs b),(SELECT string_agg(row(u.*)::text,',') FROM td_mta.account_usage u)")
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
    let initial = vec![
        folder("inbox", "Inbox", None, Some("inbox")),
        folder("child", "Nested", Some(b"inbox"), None),
        folder("grand", "Grandchild", Some(b"child"), None),
        folder("other", "Other", None, None),
        folder("occupied", "Occupied", Some(b"other"), None),
    ];
    for value in &initial {
        db::publish_mailbox(request, trust, deadline(), &authority, value).unwrap();
    }
    let desired = Mailbox::new(
        key("child", SourceKind::Mailbox),
        MailboxInput {
            name: "Renamed",
            parent: Some(b"other"),
            role: None,
            sort_order: 99,
            subscribed: false,
        },
    )
    .unwrap();
    let mut definitions = initial.clone();
    definitions[1] = desired.clone();
    let plan = |value: &Mailbox, definitions: &[Mailbox]| {
        reconcile::plan(request, trust, deadline(), &authority, value, definitions).unwrap()
    };
    let apply = |value: &Mailbox, definitions: &[Mailbox], baseline: &Baseline| {
        reconcile::apply(
            request,
            trust,
            deadline(),
            &authority,
            value,
            definitions,
            baseline,
        )
    };
    assert!(matches!(plan(&initial[1], &initial), Plan::Current));
    assert!(matches!(
        plan(&folder("new", "New", None, None), &initial),
        Plan::New
    ));
    let baseline = update(plan(&desired, &definitions));
    // Mail arriving after planning changes folder counters/state, not its metadata.
    let raw = b"Message-ID: <folder-reconcile@example.test>\r\n\r\nraw\0bytes\xff";
    db::publish_email(
        request,
        trust,
        deadline(),
        &authority,
        &email("arrival", raw, &["$seen"]),
        raw,
    )
    .unwrap();
    let invariant = untouched(fixture);
    let descendant = sql(
        fixture,
        "SELECT row(m.*)::text FROM td_mta.mailboxes m WHERE name='Grandchild'",
    );
    let before = state(fixture);
    assert_eq!(
        apply(&desired, &definitions, &baseline),
        Ok(Applied::Updated)
    );
    assert_ne!(before, state(fixture));
    assert_eq!(invariant, untouched(fixture));
    assert_eq!(
        descendant,
        sql(
            fixture,
            "SELECT row(m.*)::text FROM td_mta.mailboxes m WHERE name='Grandchild'"
        )
    );
    assert_eq!(sql(fixture,"SELECT m.name,p.name,m.sort_order,m.subscribed FROM td_mta.mailboxes m JOIN td_mta.mailboxes p ON p.id=m.parent_id WHERE m.name='Renamed'"),"Renamed|Other|99|0");
    assert_eq!(sql(fixture,"SELECT count(*) FROM td_mta.changes WHERE sequence=(SELECT sequence FROM td_mta.accounts) AND kind=1 AND action=2"),"1");
    let after = state(fixture);
    assert_eq!(
        apply(&desired, &definitions, &baseline),
        Ok(Applied::CurrentReceipt)
    );
    assert_eq!(after, state(fixture));
    sql(
        fixture,
        "UPDATE td_mta.mailboxes SET name='Local' WHERE name='Renamed'",
    );
    assert_eq!(
        apply(&desired, &definitions, &baseline),
        Ok(Applied::CurrentReceipt)
    );
    assert!(matches!(plan(&desired, &definitions), Plan::LocalChanged));
    sql(
        fixture,
        "UPDATE td_mta.mailboxes SET name='Renamed' WHERE name='Local'",
    );
    let back = update(plan(&initial[1], &initial));
    assert_eq!(apply(&initial[1], &initial, &back), Ok(Applied::Updated));
    assert_eq!(
        apply(&desired, &definitions, &baseline),
        Err(db::Error::Conflict)
    );
    let fresh = update(plan(&desired, &definitions));
    sql(
        fixture,
        "UPDATE td_mta.mailboxes SET name='Local' WHERE name='Nested'",
    );
    assert_eq!(
        apply(&desired, &definitions, &fresh),
        Err(db::Error::Conflict)
    );
    sql(
        fixture,
        "UPDATE td_mta.mailboxes SET name='Nested' WHERE name='Local'",
    );
    // Another source namespace must not share a folder being overwritten.
    sql(fixture,"INSERT INTO td_mta.imports SELECT account,decode(repeat('07',16),'hex'),source_kind,source_account,source_object,local_object,historical_blob,source_digest,changed FROM td_mta.imports WHERE source_object=convert_to('child','UTF8')");
    assert!(matches!(plan(&desired, &definitions), Plan::LocalChanged));
    assert_eq!(
        apply(&desired, &definitions, &fresh),
        Err(db::Error::Conflict)
    );
    sql(
        fixture,
        "DELETE FROM td_mta.imports WHERE source_instance=decode(repeat('07',16),'hex')",
    );
    sql(
        fixture,
        "UPDATE td_mta.mailboxes SET name='Parent drift' WHERE name='Other'",
    );
    assert!(matches!(plan(&desired, &definitions), Plan::FolderChanged));
    assert_eq!(
        apply(&desired, &definitions, &fresh),
        Err(db::Error::Conflict)
    );
    sql(
        fixture,
        "UPDATE td_mta.mailboxes SET name='Other' WHERE name='Parent drift'",
    );
    let collision = folder("child", "Occupied", Some(b"other"), None);
    assert!(matches!(plan(&collision, &initial), Plan::FolderChanged));
    let cyclic = folder("child", "Nested", Some(b"grand"), None);
    assert!(matches!(plan(&cyclic, &initial), Plan::FolderChanged));
    let role = folder("inbox", "Inbox", None, Some("sent"));
    assert!(matches!(plan(&role, &initial), Plan::RoleChanged));
    let root = folder("child", "Root", None, None);
    let root_plan = update(plan(&root, &initial));
    let mut wrong = root_plan.clone();
    wrong.folders = fresh.folders.clone();
    assert_eq!(apply(&root, &initial, &wrong), Err(db::Error::Conflict));
    let mut wrong = fresh.clone();
    wrong.account = AccountId::from_bytes([0; 16]);
    assert_eq!(
        apply(&desired, &definitions, &wrong),
        Err(db::Error::Invalid)
    );
    let mut wrong = fresh.clone();
    wrong.local = [0; 16];
    assert_eq!(
        apply(&desired, &definitions, &wrong),
        Err(db::Error::Conflict)
    );
    sql(fixture, "UPDATE td_mta.runtime_barrier SET blocked=true");
    assert_eq!(
        apply(&desired, &definitions, &fresh),
        Err(db::Error::Policy)
    );
    sql(fixture, "UPDATE td_mta.runtime_barrier SET blocked=false");
    let uncertain_sequence = u64::from_str_radix(
        &sql(
            fixture,
            "SELECT encode(sequence,'hex') FROM td_mta.accounts",
        ),
        16,
    )
    .unwrap();
    sql(fixture,"CREATE FUNCTION public.delay_folder() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_sleep(11); RETURN NEW; END $$; CREATE CONSTRAINT TRIGGER delay_folder AFTER UPDATE ON td_mta.imports DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.delay_folder()");
    assert_eq!(
        apply(&desired, &definitions, &fresh),
        Err(db::Error::Indeterminate)
    );
    sql(fixture, "DROP TRIGGER delay_folder ON td_mta.imports");
    assert!(matches!(
        apply(&desired, &definitions, &fresh),
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
        uncertain_sequence + 1
    );
    assert!(matches!(plan(&desired, &definitions), Plan::Current));
    assert_eq!(invariant, untouched(fixture));
    let absent = folder("occupied", "Absent", Some(b"other"), None);
    let absent_plan = update(plan(&absent, &definitions));
    sql(
        fixture,
        "DELETE FROM td_mta.mailboxes WHERE name='Occupied'",
    );
    assert_eq!(
        apply(&absent, &definitions, &absent_plan),
        Ok(Applied::Absent)
    );
    assert!(matches!(plan(&absent, &definitions), Plan::Absent));
    cli(fixture, connection, epoch, account);
    deletion_checks(fixture, request, trust, &authority);
    // Fill the account with long-name local descendants, without source receipts.
    sql(fixture,"INSERT INTO td_mta.mailboxes SELECT a.id,decode(lpad(to_hex(n),32,'0'),'hex'),repeat('x',1020)||n::text,CASE WHEN n=1000 THEN (SELECT local_object FROM td_mta.imports WHERE source_object=convert_to('other','UTF8')) ELSE decode(lpad(to_hex(n-1),32,'0'),'hex') END,NULL,0,1,a.sequence FROM td_mta.accounts a CROSS JOIN generate_series(1000,1251) n");
    let largest = folder("other", "Large subtree", None, None);
    let largest_plan = update(plan(&largest, &definitions));
    assert_eq!(
        apply(&largest, &definitions, &largest_plan),
        Ok(Applied::Updated)
    );
    assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.mailboxes"), "256");
    assert_eq!(invariant, untouched(fixture));
    sql(fixture,"INSERT INTO td_mta.mailboxes SELECT id,decode(repeat('ff',16),'hex'),'Overflow',NULL,NULL,0,1,sequence FROM td_mta.accounts");
    assert!(matches!(
        reconcile::plan(
            request,
            trust,
            deadline(),
            &authority,
            &folder("other", "Other again", None, None),
            &definitions
        ),
        Err(db::Error::Capacity)
    ));
    sql(
        fixture,
        "DELETE FROM td_mta.mailboxes WHERE name='Overflow'",
    );
}
fn cli(fixture: &Fixture, connection: &std::path::Path, epoch: StoreEpoch, account: AccountId) {
    use std::{fs, os::unix::fs::PermissionsExt, process::Command};
    use td_json::{json, Json};
    let archive = td_fs::TempDir::new("folder-plan-archive").unwrap();
    let control = td_fs::TempDir::new("folder-plan-control").unwrap();
    for path in [archive.path(), control.path()] {
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let source = json!({"version":1,"instance":"05050505050505050505050505050505","session":"https://source.example.test/jmap","account":"source","username":"source"});
    let manifest = json!({"version":1,"provisional":true,"coverage":"per-mailbox-query","source":source.clone(),"mailboxState":"m","emailState":"e","mailboxes":[{"id":"inbox","name":"Incoming","parentId":null,"role":"inbox","sortOrder":3,"isSubscribed":false}],"messages":[]});
    write(&archive.join("source.json"), source.to_string().as_bytes());
    write(
        &archive.join("manifest.json"),
        manifest.to_string().as_bytes(),
    );
    let invoke = |action: &str| {
        let mut c = Command::new(env!("CARGO_BIN_EXE_td-mta"));
        c.args(["migration", action, "--archive"])
            .arg(archive.path())
            .args(["--max-bytes", "1048576", "--connection-file"])
            .arg(connection)
            .args([
                "--epoch",
                &epoch.to_string(),
                "--account",
                &account.to_string(),
            ]);
        if action == "plan" {
            c.arg("--mailboxes");
        } else {
            c.arg("--plan-file").arg(control.join("plan.json"));
        }
        c.output().unwrap()
    };
    let result = invoke("plan");
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let plan = td_json::parse_slice(&result.stdout).unwrap();
    assert_eq!(plan["format"].as_u64(), Some(2));
    assert_eq!(plan["mailboxes"][0]["status"].as_str(), Some("update"));
    let invariant = untouched(fixture);
    let before = state(fixture);
    let mut wrong = plan.clone();
    if let Json::Obj(fields) = &mut wrong {
        fields
            .iter_mut()
            .find(|(k, _)| k == "manifestSha256")
            .unwrap()
            .1 = json!("00".repeat(32));
    }
    write(&control.join("plan.json"), wrong.to_string().as_bytes());
    assert!(!invoke("apply").status.success());
    assert_eq!(before, state(fixture));
    write(&control.join("plan.json"), plan.to_string().as_bytes());
    let result = invoke("apply");
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        td_json::parse_slice(&result.stdout).unwrap()["updated"].as_u64(),
        Some(1)
    );
    let after = state(fixture);
    assert!(invoke("apply").status.success());
    assert_eq!(after, state(fixture));
    assert_eq!(invariant, untouched(fixture));
    assert_eq!(
        sql(
            fixture,
            "SELECT name,sort_order,subscribed FROM td_mta.mailboxes WHERE role='inbox'"
        ),
        "Incoming|3|0"
    );
}

fn deletion_checks(
    fixture: &Fixture,
    request: &Request<'_>,
    trust: &TrustStore,
    authority: &db::Authority,
) {
    use td_mta::postgres_imports::mailbox_reconcile::deletion::{self, Applied, Plan};
    let parent = folder("delete-parent", "Delete parent", None, None);
    let child = folder("delete-child", "Delete child", Some(b"delete-parent"), None);
    let protected = folder("delete-protected", "Protected", None, Some("archive"));
    let plan =
        |value: &Mailbox| deletion::plan(request, trust, deadline(), authority, value).unwrap();
    assert!(matches!(plan(&parent), Plan::Unmapped));
    for value in [&parent, &child, &protected] {
        db::publish_mailbox(request, trust, deadline(), authority, value).unwrap();
    }
    assert!(matches!(plan(&parent), Plan::HasChild));
    assert!(matches!(plan(&protected), Plan::Role));
    let baseline = match plan(&child) {
        Plan::Delete(b) => b,
        _ => panic!("expected leaf"),
    };
    let apply = |value: &Mailbox, baseline: &deletion::Baseline| {
        deletion::apply(request, trust, deadline(), authority, value, baseline)
    };
    let id = baseline
        .local
        .iter()
        .map(|v| format!("{v:02x}"))
        .collect::<String>();
    let invariant = untouched(fixture);
    sql(fixture,&format!("INSERT INTO td_mta.memberships SELECT account,id,decode('{id}','hex'),changed FROM td_mta.emails LIMIT 1"));
    assert!(matches!(plan(&child), Plan::HasEmail));
    assert_eq!(apply(&child, &baseline), Err(db::Error::Conflict));
    sql(
        fixture,
        &format!("DELETE FROM td_mta.memberships WHERE mailbox_id=decode('{id}','hex')"),
    );
    sql(
        fixture,
        &format!("UPDATE td_mta.mailboxes SET name='Local name' WHERE id=decode('{id}','hex')"),
    );
    assert!(matches!(plan(&child), Plan::Conflict));
    assert_eq!(apply(&child, &baseline), Err(db::Error::Conflict));
    sql(
        fixture,
        &format!("UPDATE td_mta.mailboxes SET name='Delete child' WHERE id=decode('{id}','hex')"),
    );
    let mut wrong = baseline.clone();
    wrong.account = AccountId::from_bytes([0; 16]);
    assert_eq!(apply(&child, &wrong), Err(db::Error::Invalid));
    let mut wrong = baseline.clone();
    wrong.changed += 1;
    assert_eq!(apply(&child, &wrong), Err(db::Error::Conflict));
    let old = state(fixture);
    assert_eq!(apply(&child, &baseline), Ok(Applied::Deleted));
    assert_ne!(state(fixture), old);
    let settled = state(fixture);
    assert_eq!(apply(&child, &baseline), Ok(Applied::Absent));
    assert_eq!(state(fixture), settled);
    assert!(
        !db::publish_mailbox(request, trust, deadline(), authority, &child)
            .unwrap()
            .present
    );
    assert_eq!(state(fixture), settled);
    assert_eq!(untouched(fixture), invariant);
    let parent_baseline = match plan(&parent) {
        Plan::Delete(b) => b,
        _ => panic!("expected parent leaf"),
    };
    let old = u64::from_str_radix(
        &sql(
            fixture,
            "SELECT encode(sequence,'hex') FROM td_mta.accounts",
        ),
        16,
    )
    .unwrap();
    sql(fixture,"CREATE FUNCTION public.delay_folder_delete() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_sleep(11); RETURN OLD; END $$; CREATE CONSTRAINT TRIGGER delay_folder_delete AFTER DELETE ON td_mta.mailboxes DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.delay_folder_delete()");
    assert_eq!(
        apply(&parent, &parent_baseline),
        Err(db::Error::Indeterminate)
    );
    sql(
        fixture,
        "DROP TRIGGER delay_folder_delete ON td_mta.mailboxes",
    );
    assert!(matches!(
        apply(&parent, &parent_baseline),
        Ok(Applied::Deleted | Applied::Absent)
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
        old + 1
    );
    assert_eq!(untouched(fixture), invariant);
    assert_eq!(sql(fixture,"SELECT count(*) FROM td_mta.imports WHERE source_object IN (convert_to('delete-parent','UTF8'),convert_to('delete-child','UTF8'))"),"2");
    // Restore this fixture's mailbox count before its separate 256-folder probe.
    sql(fixture,"DELETE FROM td_mta.mailboxes WHERE id=(SELECT local_object FROM td_mta.imports WHERE source_object=convert_to('delete-protected','UTF8')); DELETE FROM td_mta.imports WHERE source_object=convert_to('delete-protected','UTF8')");
}
