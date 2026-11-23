//! Schema 46: per-Email sender authentication rows.
use super::{deadline, sql, Fixture};
use td_mta::postgres_schema;

const ACCOUNT: &str = "decode(repeat('01',16),'hex')";

fn email(fixture: &Fixture, id: &str) {
    sql(fixture, &format!("BEGIN;
        INSERT INTO td_mta.blob_ids VALUES({ACCOUNT},decode(repeat('{id}',16),'hex'));
        INSERT INTO td_mta.blobs VALUES({ACCOUNT},decode(repeat('{id}',16),'hex'),0,sha256(''::bytea),0,decode(repeat('00',7)||'01','hex'),0);
        INSERT INTO td_mta.emails(account,id,blob_id,thread_id,received_at,origin,peer_family,peer_octets,tls,ehlo,reverse_path,receipt_count,changed)
            VALUES({ACCOUNT},decode(repeat('{id}',16),'hex'),decode(repeat('{id}',16),'hex'),{ACCOUNT},1,1,4,decode('7f000001','hex'),0,'peer.example.test','a@example.test',1,decode(repeat('00',7)||'01','hex'));
        COMMIT"));
}

/// The statement must fail an integrity constraint, deferred ones included.
fn refuse(fixture: &Fixture, statement: &str) {
    sql(fixture, &format!("DO $$ BEGIN BEGIN SET CONSTRAINTS ALL IMMEDIATE; {statement}; EXCEPTION WHEN integrity_constraint_violation THEN RETURN; END; RAISE EXCEPTION 'accepted'; END $$"));
}

#[test]
#[ignore = "real PostgreSQL 17 with pg_trgm 1.6 required"]
pub(super) fn sender_authentication_upgrade_adds_checked_cascading_rows() {
    let fixture = Fixture::new();
    let request = fixture.request();
    let fresh = postgres_schema::initialize(&request, deadline()).unwrap();
    sql(&fixture, &format!("INSERT INTO td_mta.accounts VALUES({ACCOUNT},decode(repeat('00',7)||'01','hex'),decode(repeat('00',8),'hex'));
        INSERT INTO td_mta.threads SELECT id,id,sequence FROM td_mta.accounts"));
    email(&fixture, "02");
    super::rewind_sender_authentication(&fixture);
    assert_eq!(
        sql(&fixture, "SELECT max(version), to_regclass('td_mta.email_authentication') IS NULL FROM td_mta.schema_migrations"),
        "45|t"
    );
    // An existing direct-MX Email keeps no row; the upgrade invents none.
    let upgraded = postgres_schema::upgrade(&request, deadline()).unwrap();
    assert_eq!(upgraded, fresh);
    assert_eq!(
        sql(&fixture, "SELECT count(*) FROM td_mta.email_authentication"),
        "0"
    );
    assert_eq!(
        sql(&fixture, "SELECT bool_and(NOT has_table_privilege('public', c.oid, 'SELECT')) FROM pg_class c WHERE c.oid IN ('td_mta.email_authentication'::regclass, 'td_mta.email_dkim_signatures'::regclass)"),
        "t"
    );
    email(&fixture, "03");
    let row = |spf: &str, dmarc: &str, policy: &str, aligned: &str| {
        format!("INSERT INTO td_mta.email_authentication VALUES({ACCOUNT},decode(repeat('03',16),'hex'),'{spf}','example.test',false,true,'{dmarc}','example.test','example.test',{policy},{aligned})")
    };
    for refused in [
        row("bogus", "pass", "'reject',false,false", "'dkim'"),
        row("pass", "bogus", "'reject',false,false", "'dkim'"),
        row("pass", "fail", "'reject',false,false", "'dkim'"),
        row("pass", "pass", "NULL,false,false", "NULL"),
        row("pass", "pass", "'block',false,false", "NULL"),
    ] {
        refuse(&fixture, &refused);
    }
    sql(
        &fixture,
        &row("pass", "pass", "'reject',true,false", "'dkim'"),
    );
    sql(&fixture, &format!("INSERT INTO td_mta.email_dkim_signatures VALUES({ACCOUNT},decode(repeat('03',16),'hex'),0,'pass','rsa-sha256','example.test','s1',2048)"));
    for refused in [
        format!("INSERT INTO td_mta.email_dkim_signatures VALUES({ACCOUNT},decode(repeat('03',16),'hex'),4,'pass',NULL,NULL,NULL,NULL)"),
        format!("INSERT INTO td_mta.email_dkim_signatures VALUES({ACCOUNT},decode(repeat('03',16),'hex'),1,'none',NULL,NULL,NULL,NULL)"),
        format!("INSERT INTO td_mta.email_dkim_signatures VALUES({ACCOUNT},decode(repeat('02',16),'hex'),0,'pass',NULL,NULL,NULL,NULL)"),
    ] {
        refuse(&fixture, &refused);
    }
    sql(&fixture, &format!("DELETE FROM td_mta.emails WHERE account={ACCOUNT} AND id=decode(repeat('03',16),'hex')"));
    assert_eq!(
        sql(&fixture, "SELECT (SELECT count(*) FROM td_mta.email_authentication), (SELECT count(*) FROM td_mta.email_dkim_signatures)"),
        "0|0"
    );
}
