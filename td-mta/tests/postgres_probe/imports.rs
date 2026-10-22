use super::{command_output, deadline, sql, Fixture};
use std::{fs, os::unix::fs::PermissionsExt, path::Path};
use td_crypto::{Digest, Sha256, TrustStore};
use td_json::json;
use td_mta::{
    format::key::SourceKind,
    ids::{AccountId, InstanceId, StoreEpoch},
    import_snapshot::{self as snapshot, Key},
    postgres_imports as db,
    postgres_probe::Request,
};
pub(super) fn write(path: &Path, bytes: &[u8]) {
    fs::write(path, bytes).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}
pub(super) fn key(id: &str, kind: SourceKind) -> Key {
    Key::new(
        InstanceId::from_bytes([5; 16]),
        kind,
        b"source",
        id.as_bytes(),
    )
    .unwrap()
}
pub(super) fn folder(
    id: &str,
    name: &str,
    parent: Option<&[u8]>,
    role: Option<&str>,
) -> snapshot::Mailbox {
    snapshot::Mailbox::new(
        key(id, SourceKind::Mailbox),
        snapshot::MailboxInput {
            name,
            parent,
            role,
            sort_order: 0,
            subscribed: true,
        },
    )
    .unwrap()
}
pub(super) fn email(id: &str, body: &[u8], keywords: &[&str]) -> snapshot::Email {
    let mut hash = Sha256::try_new().unwrap();
    hash.update(body).unwrap();
    snapshot::Email::new(
        key(id, SourceKind::Email),
        snapshot::EmailInput {
            body_digest: hash.finish().unwrap(),
            length: body.len() as u64,
            received_at: 1704164645123,
            mailboxes: &[b"inbox", b"child"],
            keywords,
        },
    )
    .unwrap()
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
    assert!(matches!(
        db::prepare(
            request,
            trust,
            deadline(),
            StoreEpoch::from_bytes([0; 16]),
            account
        ),
        Err(db::Error::Epoch)
    ));
    let mut runtime = fixture.request();
    runtime.mode = td_mta::postgres_probe::Mode::Runtime;
    assert!(matches!(
        db::prepare(&runtime, trust, deadline(), epoch, account),
        Err(db::Error::Owner)
    ));
    let inbox = folder("inbox", "Inbox", None, Some("inbox"));
    let adopted = db::publish_mailbox(request, trust, deadline(), &authority, &inbox).unwrap();
    assert!(!adopted.created);
    assert!(adopted.present);
    let child = folder("child", "Nested", Some(b"inbox"), None);
    let nested = db::publish_mailbox(request, trust, deadline(), &authority, &child).unwrap();
    assert!(nested.created);
    assert_eq!(
        sql(
            fixture,
            "SELECT count(*) FROM td_mta.mailboxes WHERE parent_id IS NOT NULL"
        ),
        "1"
    );
    assert_eq!(
        db::publish_mailbox(
            request,
            trust,
            deadline(),
            &authority,
            &folder("orphan", "Orphan", Some(b"missing"), None)
        ),
        Err(db::Error::MissingMailbox)
    );
    assert_eq!(
        db::publish_mailbox(
            request,
            trust,
            deadline(),
            &authority,
            &folder("inbox", "Changed", None, Some("inbox"))
        ),
        Err(db::Error::SourceChanged)
    );
    assert_eq!(
        db::publish_mailbox(
            request,
            trust,
            deadline(),
            &authority,
            &folder("duplicate", "Inbox", None, Some("inbox"))
        ),
        Err(db::Error::Conflict)
    );
    let body=b"Return-Path: <original@example.test>\r\nMessage-ID: <original@example.test>\r\nSubject: untouched\r\n\r\nraw\0binary\xff\n";
    let one = email("one", body, &["$Seen", "Custom"]);
    let first = db::publish_email(request, trust, deadline(), &authority, &one, body).unwrap();
    assert!(first.created);
    let two = email("two", body, &["$seen", "custom"]);
    let second = db::publish_email(request, trust, deadline(), &authority, &two, body).unwrap();
    assert_ne!(first.local_object, second.local_object);
    assert_eq!(sql(fixture,"SELECT count(*),count(DISTINCT blob_id),count(DISTINCT thread_id),min(received_at),min(origin) FROM td_mta.emails"),"2|2|1|1704164645123|3");
    assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.memberships"), "4");
    assert_eq!(
        sql(
            fixture,
            "SELECT string_agg(keyword,',' ORDER BY keyword) FROM td_mta.keywords"
        ),
        "$seen,$seen,custom,custom"
    );
    let expected: String = body.iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(
        sql(
            fixture,
            "SELECT encode(bytes,'hex') FROM td_mta.blob_chunks ORDER BY blob_id LIMIT 1"
        ),
        expected
    );
    let state=sql(fixture,"SELECT encode(sequence,'hex'),(SELECT count(*) FROM td_mta.changes),body_bytes,blob_count FROM td_mta.accounts JOIN td_mta.account_usage ON account=id");
    assert!(
        !db::publish_email(request, trust, deadline(), &authority, &one, body)
            .unwrap()
            .created
    );
    assert!(
        !db::publish_mailbox(request, trust, deadline(), &authority, &child)
            .unwrap()
            .created
    );
    assert_eq!(sql(fixture,"SELECT encode(sequence,'hex'),(SELECT count(*) FROM td_mta.changes),body_bytes,blob_count FROM td_mta.accounts JOIN td_mta.account_usage ON account=id"),state);
    assert_eq!(
        db::publish_email(
            request,
            trust,
            deadline(),
            &authority,
            &email("one", body, &["$flagged"]),
            body
        ),
        Err(db::Error::SourceChanged)
    );
    let wrong = vec![0; body.len()];
    assert_eq!(
        db::publish_email(
            request,
            trust,
            deadline(),
            &authority,
            &email("bad", body, &[]),
            &wrong
        ),
        Err(db::Error::Invalid)
    );
    // Quota refusal leaves no receipt, body or sequence; restore fixture usage.
    sql(
        fixture,
        "UPDATE td_mta.account_usage SET body_bytes=9223372036854775807",
    );
    assert_eq!(
        db::publish_email(
            request,
            trust,
            deadline(),
            &authority,
            &email("quota", body, &[]),
            body
        ),
        Err(db::Error::Quota)
    );
    assert_eq!(sql(fixture, "SELECT count(*) FROM td_mta.imports"), "4");
    sql(
        fixture,
        &format!(
            "UPDATE td_mta.account_usage SET body_bytes={}",
            body.len() * 2
        ),
    );
    // Concurrent identical requests settle to one publication.
    let concurrent = email("concurrent", body, &[]);
    let outcomes = std::thread::scope(|scope| {
        let left = scope
            .spawn(|| db::publish_email(request, trust, deadline(), &authority, &concurrent, body));
        let right = scope
            .spawn(|| db::publish_email(request, trust, deadline(), &authority, &concurrent, body));
        (
            left.join().unwrap().unwrap(),
            right.join().unwrap().unwrap(),
        )
    });
    assert_eq!(outcomes.0.local_object, outcomes.1.local_object);
    assert_ne!(outcomes.0.created, outcomes.1.created);
    // Historical receipts survive a local deletion and never resurrect it.
    let local: String = first
        .local_object
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    sql(fixture,&format!("DELETE FROM td_mta.memberships WHERE email_id=decode('{local}','hex'); DELETE FROM td_mta.keywords WHERE email_id=decode('{local}','hex'); DELETE FROM td_mta.thread_anchors WHERE email_id=decode('{local}','hex'); DELETE FROM td_mta.emails WHERE id=decode('{local}','hex')"));
    let historical = db::publish_email(request, trust, deadline(), &authority, &one, body).unwrap();
    assert!(!historical.present);
    assert!(!historical.created);
    assert_eq!(historical.local_object, first.local_object);
    // The executable consumes a real private archive, then repeats it unchanged.
    let archive = td_fs::TempDir::new("import-cli").unwrap();
    fs::set_permissions(archive.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let mut hash = Sha256::try_new().unwrap();
    hash.update(body).unwrap();
    let digest: String = hash
        .finish()
        .unwrap()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let source = json!({"version":1,"instance":"05050505050505050505050505050505","session":"https://source.example.test/jmap","account":"source","username":"source"});
    let manifest = json!({"version":1,"provisional":true,"coverage":"per-mailbox-query","source":source.clone(),"mailboxState":"m","emailState":"e","mailboxes":[{"id":"inbox","name":"Inbox","parentId":null,"role":"inbox","sortOrder":0,"isSubscribed":true},{"id":"child","name":"Nested","parentId":"inbox","role":null,"sortOrder":0,"isSubscribed":true}],"messages":[{"email":{"id":"cli","blobId":"raw","size":body.len(),"receivedAt":"2024-01-02T03:04:05.123000Z","mailboxIds":{"inbox":true,"child":true},"keywords":{"Custom":true}},"body":digest.clone()}]});
    write(&archive.join("source.json"), source.to_string().as_bytes());
    write(
        &archive.join("manifest.json"),
        manifest.to_string().as_bytes(),
    );
    write(&archive.join(format!("{digest}.eml")), body);
    let invoke = || {
        command_output(
            &[
                "migration",
                "import",
                "--archive",
                archive.path().to_str().unwrap(),
                "--max-bytes",
                "1048576",
                "--connection-file",
                connection.to_str().unwrap(),
                "--epoch",
                &epoch.to_string(),
                "--account",
                &account.to_string(),
            ],
            fixture.root.path(),
            0,
        )
    };
    assert!(invoke().contains("\"createdMessages\":1"));
    assert!(invoke().contains("\"createdMessages\":0"));
    sql(fixture,"CREATE SEQUENCE public.import_attempts; CREATE FUNCTION public.delay_import() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM nextval('public.import_attempts'); PERFORM pg_sleep(11); RETURN NEW; END $$; CREATE CONSTRAINT TRIGGER delay_import AFTER INSERT ON td_mta.imports DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.delay_import()");
    let delayed = email("delayed", body, &[]);
    assert_eq!(
        db::publish_email(request, trust, deadline(), &authority, &delayed, body),
        Err(db::Error::Indeterminate)
    );
    sql(fixture, "DROP TRIGGER delay_import ON td_mta.imports");
    let resolved =
        db::publish_email(request, trust, deadline(), &authority, &delayed, body).unwrap();
    assert_eq!(
        db::inspect(
            request,
            trust,
            deadline(),
            &authority,
            &key("delayed", SourceKind::Email),
            delayed.digest()
        )
        .unwrap()
        .unwrap()
        .local_object,
        resolved.local_object
    );
    // Full supported relationship and body bounds fit one publication budget.
    sql(fixture,"INSERT INTO td_mta.mailboxes(account,id,name,parent_id,role,sort_order,subscribed,changed) SELECT decode(repeat('aa',16),'hex'),decode(md5('box-'||n),'hex'),'Box '||n,NULL,NULL,0,1,(SELECT sequence FROM td_mta.accounts) FROM generate_series(1,254) n; INSERT INTO td_mta.imports(account,source_instance,source_kind,source_account,source_object,local_object,historical_blob,source_digest,changed) SELECT decode(repeat('aa',16),'hex'),decode(repeat('05',16),'hex'),1,convert_to('source','UTF8'),convert_to('box-'||n,'UTF8'),decode(md5('box-'||n),'hex'),NULL,decode(repeat('00',32),'hex'),(SELECT sequence FROM td_mta.accounts) FROM generate_series(1,254) n");
    let mut folders = vec!["inbox".to_owned(), "child".to_owned()];
    folders.extend((1..=254).map(|n| format!("box-{n}")));
    let mailbox_refs: Vec<&[u8]> = folders.iter().map(|s| s.as_bytes()).collect();
    let keywords: Vec<String> = (0..256).map(|n| format!("flag{n}")).collect();
    let keyword_refs: Vec<&str> = keywords.iter().map(String::as_str).collect();
    let mut large = b"Message-ID: <large@example.test>\r\n\r\n".to_vec();
    large.resize(32 * 1024 * 1024, b'x');
    let mut hash = Sha256::try_new().unwrap();
    hash.update(&large).unwrap();
    let largest = snapshot::Email::new(
        key("largest", SourceKind::Email),
        snapshot::EmailInput {
            body_digest: hash.finish().unwrap(),
            length: large.len() as u64,
            received_at: -1,
            mailboxes: &mailbox_refs,
            keywords: &keyword_refs,
        },
    )
    .unwrap();
    assert!(
        db::publish_email(request, trust, deadline(), &authority, &largest, &large)
            .unwrap()
            .created
    );
    assert_eq!(sql(fixture,"SELECT count(*) FROM td_mta.memberships WHERE email_id=(SELECT local_object FROM td_mta.imports WHERE source_object=convert_to('largest','UTF8'))"),"256");
    assert_eq!(sql(fixture,"SELECT count(*) FROM td_mta.keywords WHERE email_id=(SELECT local_object FROM td_mta.imports WHERE source_object=convert_to('largest','UTF8'))"),"256");
    assert_eq!(
        db::publish_mailbox(
            request,
            trust,
            deadline(),
            &authority,
            &folder("too-many", "Overflow", None, None)
        ),
        Err(db::Error::Capacity)
    );
    sql(fixture, "UPDATE td_mta.runtime_barrier SET blocked=true");
    assert_eq!(
        db::publish_email(
            request,
            trust,
            deadline(),
            &authority,
            &email("maintenance", body, &[]),
            body
        ),
        Err(db::Error::Policy)
    );
    sql(fixture, "UPDATE td_mta.runtime_barrier SET blocked=false");
    super::create_runtime_login(fixture);
    super::grant_runtime_mail(fixture);
    let mut nonowner = fixture.request();
    nonowner.user = "mail_runtime";
    assert!(matches!(
        db::prepare(&nonowner, trust, deadline(), epoch, account),
        Err(db::Error::Owner)
    ));
    // New writes are fenced by the selected generation; old receipts remain known.
    sql(
        fixture,
        "UPDATE td_mta.config_selection SET generation=decode('0000000000000002','hex')",
    );
    assert_eq!(
        db::publish_email(
            request,
            trust,
            deadline(),
            &authority,
            &email("stale", body, &[]),
            body
        ),
        Err(db::Error::Policy)
    );
    assert!(
        !db::publish_email(request, trust, deadline(), &authority, &two, body)
            .unwrap()
            .created
    );
}
