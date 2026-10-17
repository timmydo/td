use super::{deadline, sql, Fixture};
use std::time::Instant;
use td_crypto::TrustStore;
use td_mta::{
    format::Table,
    ids::{AccountId, StoreEpoch},
    metadata_sweep::Limits,
    postgres_accounts, postgres_metadata_audit as audit, postgres_schema,
};

#[test]
#[ignore = "real local PostgreSQL tools and private trusted TMPDIR required"]
fn real_postgres_metadata_audit_checks_canonical_graph_without_repair() {
    let fixture = Fixture::new();
    let trust = TrustStore::from_pem(&fixture.ca).unwrap();
    let epoch = postgres_schema::initialize(&fixture.request(), &trust, deadline())
        .unwrap()
        .store_epoch;
    let account = AccountId::from_bytes([1; 16]);
    let limits = Limits {
        rows: 100,
        parent_reads: 100,
    };
    let inspect = || {
        audit::inspect(
            &fixture.request(),
            &trust,
            deadline(),
            epoch,
            account,
            0,
            limits,
        )
    };
    assert_eq!(inspect(), Err(audit::Error::Missing));
    postgres_accounts::publish(
        &fixture.request(),
        &trust,
        deadline(),
        postgres_accounts::prepare(epoch, account).unwrap(),
    )
    .unwrap();
    let initial = inspect().unwrap();
    assert_eq!(initial.references().identity().account, account);
    assert_eq!(initial.references().identity().epoch, epoch);
    assert_eq!(
        initial.references().identity().committed_sequence.number(),
        1
    );
    assert_eq!(initial.mailboxes().mailboxes(), 1);
    let id = |n: u8| format!("decode(repeat('{n:02x}',16),'hex')");
    let a = id(1);
    let b = id(3);
    let t = id(4);
    let e = id(5);
    let s = id(6);
    let m = id(7);
    let q = "decode('0000000000000001','hex')";
    sql(&fixture,&format!("BEGIN;
        INSERT INTO td_mta.blob_ids VALUES({a},{b});
        INSERT INTO td_mta.blobs VALUES({a},{b},0,decode(repeat('00',32),'hex'),0,{q},0);
        INSERT INTO td_mta.threads VALUES({a},{t},{q});
        INSERT INTO td_mta.emails(account,id,blob_id,thread_id,received_at,origin,changed) VALUES({a},{e},{b},{t},0,2,{q});
        INSERT INTO td_mta.mailboxes VALUES({a},{m},'Child',NULL,NULL,0,1,{q});
        INSERT INTO td_mta.memberships SELECT {a},{e},id,{q} FROM td_mta.mailboxes;
        INSERT INTO td_mta.keywords SELECT {a},{e},n,{q} FROM unnest(ARRAY['aa','z']) n;
        INSERT INTO td_mta.thread_anchors SELECT {a},n,{e},{q} FROM unnest(ARRAY['aa@example.test','z@example.test']) n;
        INSERT INTO td_mta.submissions(account,id,email_id,thread_id,identity_id,transmitted_blob_id,reverse_path,send_at,expires_at,recipient_count,notification,changed) VALUES({a},{s},{e},{t},{t},{b},'',0,1,2,0,{q});
        INSERT INTO td_mta.recipients(account,submission_id,ordinal,address,state,uncertain,attempt_count,phase,next_attempt_at,reason,diagnostic,changed) SELECT {a},{s},n,'a@example.test',1,0,0,0,0,0,'',{q} FROM generate_series(0,1) n;
        INSERT INTO td_mta.leases VALUES({a},{b},{t},1,1,{q});
        INSERT INTO td_mta.imports SELECT {a},{t},3,convert_to(n,'UTF8'),convert_to(o,'UTF8'),{e},{b},decode(repeat('00',32),'hex'),{q} FROM unnest(ARRAY['aa','z']) n CROSS JOIN unnest(ARRAY['aa','z']) o;
        COMMIT"));
    let report = inspect().unwrap();
    for (table, count) in [
        (Table::Blobs, 1),
        (Table::Mailboxes, 2),
        (Table::Emails, 1),
        (Table::Memberships, 2),
        (Table::Keywords, 2),
        (Table::Threads, 1),
        (Table::ThreadAnchors, 2),
        (Table::Submissions, 1),
        (Table::Recipients, 2),
        (Table::Leases, 1),
        (Table::Imports, 4),
    ] {
        assert_eq!(
            report.references().table_rows(table),
            Some(count),
            "{table:?}"
        );
    }
    // Canonical metadata does not certify body digests or usage counters.
    assert_eq!(
        sql(
            &fixture,
            "SELECT body_bytes,blob_count FROM td_mta.account_usage"
        ),
        "0|0"
    );
    assert_eq!(
        sql(&fixture, "SELECT encode(digest,'hex') FROM td_mta.blobs"),
        "00".repeat(32)
    );
    for (damage,restore) in [
        ("UPDATE td_mta.keywords SET keyword='BAD' WHERE keyword='aa'".to_owned(),"UPDATE td_mta.keywords SET keyword='aa' WHERE keyword='BAD'".to_owned()),
        ("UPDATE td_mta.threads SET changed=decode('0000000000000002','hex')".to_owned(),format!("UPDATE td_mta.threads SET changed={q}")),
        ("UPDATE td_mta.recipients SET ordinal=2 WHERE ordinal=1".to_owned(),"UPDATE td_mta.recipients SET ordinal=1 WHERE ordinal=2".to_owned()),
        ("UPDATE td_mta.submissions SET recipient_count=3".to_owned(),"UPDATE td_mta.submissions SET recipient_count=2".to_owned()),
        (format!("UPDATE td_mta.mailboxes SET parent_id=CASE WHEN id={m} THEN (SELECT id FROM td_mta.mailboxes WHERE role='inbox') ELSE {m} END"),"UPDATE td_mta.mailboxes SET parent_id=NULL".to_owned()),
    ] {
        sql(&fixture,&damage);
        assert!(matches!(inspect(),Err(audit::Error::Metadata(_))),"{damage}");
        sql(&fixture,&restore);
        assert_eq!(inspect().unwrap(),report);
    }
    sql(&fixture,&format!("UPDATE td_mta.emails SET origin=1,peer_family=4,peer_octets=decode('7f000001','hex'),tls=2,ehlo='peer.example.test',reverse_path='',receipt_count=1; INSERT INTO td_mta.smtp_receipt_recipients VALUES({a},{e},0,'a@example.test')"));
    assert_eq!(inspect().unwrap(), report);
    sql(
        &fixture,
        "UPDATE td_mta.smtp_receipt_recipients SET ordinal=1",
    );
    assert!(matches!(inspect(), Err(audit::Error::Metadata(_))));
    assert_eq!(
        sql(
            &fixture,
            "SELECT ordinal FROM td_mta.smtp_receipt_recipients"
        ),
        "1"
    );
    sql(&fixture,"UPDATE td_mta.smtp_receipt_recipients SET ordinal=0; UPDATE td_mta.emails SET receipt_count=2");
    assert!(matches!(inspect(), Err(audit::Error::Metadata(_))));
    sql(&fixture, "UPDATE td_mta.emails SET receipt_count=1");
    assert_eq!(inspect().unwrap(), report);
    for selected in [
        Limits {
            rows: 0,
            parent_reads: 100,
        },
        Limits {
            rows: 100,
            parent_reads: 0,
        },
    ] {
        assert!(matches!(
            audit::inspect(
                &fixture.request(),
                &trust,
                deadline(),
                epoch,
                account,
                0,
                selected
            ),
            Err(audit::Error::Metadata(_))
        ));
    }
    assert_eq!(
        audit::inspect(
            &fixture.request(),
            &trust,
            deadline(),
            StoreEpoch::from_bytes([0; 16]),
            account,
            0,
            limits
        ),
        Err(audit::Error::Epoch)
    );
    assert_eq!(
        audit::inspect(
            &fixture.request(),
            &trust,
            deadline(),
            epoch,
            AccountId::from_bytes([2; 16]),
            0,
            limits
        ),
        Err(audit::Error::Missing)
    );
    assert_eq!(
        audit::inspect(
            &fixture.request(),
            &trust,
            Instant::now(),
            epoch,
            account,
            0,
            limits
        ),
        Err(audit::Error::Connection(
            td_mta::postgres_probe::Error::Deadline
        ))
    );
    assert_eq!(
        sql(
            &fixture,
            "SELECT encode(sequence,'hex') FROM td_mta.accounts"
        ),
        "0000000000000001"
    );
    assert_eq!(sql(&fixture, "SELECT count(*) FROM td_mta.devices"), "0");
}
