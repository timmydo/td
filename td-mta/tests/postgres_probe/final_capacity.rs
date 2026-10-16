use super::{deadline, sql, Fixture};
use td_crypto::TrustStore;
use td_mta::{
    ids::AttemptId, postgres_attempt_phases as phases, postgres_probe::Request,
    postgres_runtime_profile::Binding,
};

pub(super) fn before_possible(
    fixture: &Fixture,
    request: &Request<'_>,
    trust: &TrustStore,
    attempt: AttemptId,
    change: &phases::Change,
) {
    let old = sql(
        fixture,
        "SELECT encode(sequence,'hex') FROM td_mta.accounts",
    );
    sql(
        fixture,
        "UPDATE td_mta.accounts SET sequence=decode('fffffffffffffffe','hex')",
    );
    assert_eq!(
        phases::publish(request, trust, deadline(), change),
        Err(phases::Error::Capacity)
    );
    assert_eq!(
        sql(
            fixture,
            "SELECT count(*) FROM td_mta.attempt_final_reservations"
        ),
        "0"
    );
    sql(
        fixture,
        &format!("UPDATE td_mta.accounts SET sequence=decode('{old}','hex')"),
    );
    for (table, fields) in [
        (
            "attempt_acceptance_receipts",
            "a.account,k.id,4,k.digest,1,k.old,k.new".to_owned(),
        ),
        (
            "attempt_refusal_receipts",
            "a.account,k.id,5,k.digest,1,k.old,k.new".to_owned(),
        ),
        (
            "attempt_recovery_receipts",
            format!("a.account,k.id,k.digest,decode('{attempt}','hex'),1,k.old,k.new"),
        ),
        (
            "submission_expiry_receipts",
            "a.account,k.id,k.digest,a.submission_id,1,0,0,k.old,k.new".to_owned(),
        ),
    ] {
        sql(fixture,&format!("INSERT INTO td_mta.{table} SELECT {fields} FROM td_mta.submission_attempts a CROSS JOIN (SELECT decode(lpad(to_hex(n),32,'0'),'hex') id,decode(repeat('fe',32),'hex') digest,decode(repeat('00',8),'hex') old,decode('0000000000000001','hex') new FROM generate_series(1,1024) n) k WHERE a.attempt_id=decode('{attempt}','hex')"));
        assert_eq!(
            phases::publish(request, trust, deadline(), change),
            Err(phases::Error::Capacity),
            "{table}"
        );
        assert_eq!(
            sql(
                fixture,
                "SELECT count(*) FROM td_mta.attempt_final_reservations"
            ),
            "0"
        );
        sql(
            fixture,
            &format!(
                "DELETE FROM td_mta.{table} WHERE request_digest=decode(repeat('fe',32),'hex')"
            ),
        );
    }
}

pub(super) fn competing_writer(
    fixture: &Fixture,
    request: &Request<'_>,
    trust: &TrustStore,
    binding: &Binding,
) {
    use td_mta::{
        format::row::{ReceiptRecipients, ReceiptTls, SmtpReceipt},
        postgres_receiving as receiving,
    };
    assert_eq!(
        sql(
            fixture,
            "SELECT count(*) FROM td_mta.attempt_final_reservations"
        ),
        "1"
    );
    let mut scratch = vec![0; td_mta::format::row::MAX_RECEIPT_BYTES];
    let recipients = ReceiptRecipients::encode(&["alice@example.test"], &mut scratch).unwrap();
    let delivery=receiving::prepare(binding,&receiving::Message {
        bytes:b"From: sender@example.test\r\nTo: alice@example.test\r\nSubject: capacity\r\n\r\nbody\r\n",
        receipt:SmtpReceipt {peer:"192.0.2.1".parse().unwrap(),gateway:None,tls:ReceiptTls::Tls13,ehlo:"sender.example.test",reverse_path:"sender@example.test",recipients},
        hostname:"mail.example.test",extended:true,header_bytes:65536,received_at:1_700_000_000_000,
    },deadline()).unwrap();
    let old = sql(
        fixture,
        "SELECT encode(sequence,'hex') FROM td_mta.accounts",
    );
    sql(
        fixture,
        "UPDATE td_mta.accounts SET sequence=decode('fffffffffffffffe','hex')",
    );
    assert_eq!(
        receiving::publish(request, trust, deadline(), &delivery),
        Err(receiving::Error::Capacity)
    );
    assert_eq!(
        sql(
            fixture,
            "SELECT encode(sequence,'hex') FROM td_mta.accounts"
        ),
        "fffffffffffffffe"
    );
    assert_eq!(
        sql(
            fixture,
            "SELECT count(*) FROM td_mta.attempt_final_reservations"
        ),
        "1"
    );
    sql(
        fixture,
        &format!("UPDATE td_mta.accounts SET sequence=decode('{old}','hex')"),
    );
}
