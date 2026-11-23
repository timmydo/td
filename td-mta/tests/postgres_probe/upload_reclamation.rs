//! Expired upload leases are reclaimed by the account's next upload
//! (POSTGRES.md section 35): one bounded batch per publication releases
//! their upload bytes and retires each body nothing else reaches, so a full
//! upload quota recovers instead of being a lifetime budget.
use super::{deadline, sql, Fixture};
use td_mta::{
    admission::work::{Charge, Meter},
    clock::RuntimeClock,
    ids::BlobId,
    ports::{Clock, Deadline},
    postgres_body as bodies, postgres_devices as devices,
    postgres_probe::Request,
    postgres_runtime_profile::Binding,
    postgres_uploads as uploads, postgres_usage_audit as audit,
};

const NOW: &str = "(extract(epoch FROM clock_timestamp())*1000)::bigint";

fn list(blobs: &[BlobId]) -> String {
    blobs
        .iter()
        .map(|blob| format!("decode('{blob}','hex')"))
        .collect::<Vec<_>>()
        .join(",")
}

/// Runs on the upload fixture's account after its own checks, whose
/// earlier uploads are the `old` leases here.
pub(super) fn exercise(fixture: &Fixture, request: &Request<'_>, binding: &Binding) {
    let account = binding.account();
    let acc = format!("decode('{account}','hex')");
    let creation =
        devices::prepare_creation(binding.epoch(), account, "reclamation fixture", 1).unwrap();
    devices::publish_creation(request, deadline(), &creation).unwrap();
    let (device, password) = (creation.device_id(), creation.password());
    let clock = RuntimeClock::new();
    let publish = |bytes: &[u8]| {
        let mut meter = Meter::new(
            Deadline::after(clock.sample().unwrap().monotonic, 10_000).unwrap(),
            Charge {
                io_bytes: 3 * uploads::MAX_BYTES as u64,
                output_bytes: uploads::MAX_BYTES as u64,
                records: 4096,
                ..Charge::default()
            },
        );
        let upload =
            uploads::prepare_with_meter(binding, device, bytes, deadline(), &mut meter, &clock)
                .unwrap();
        (
            uploads::publish(request, deadline(), &upload, password),
            upload.blob(),
        )
    };
    let usage = || {
        let row = sql(
            fixture,
            &format!("SELECT body_bytes,blob_count,upload_bytes FROM td_mta.account_usage WHERE account={acc}"),
        );
        let values: Vec<i64> = row.split('|').map(|value| value.parse().unwrap()).collect();
        <[i64; 3]>::try_from(values).unwrap()
    };
    let audited = || {
        let report = audit::inspect(request, deadline(), binding.epoch(), account).unwrap();
        let [body, blobs, upload] = usage();
        assert_eq!(
            (
                report.usage.body_bytes,
                report.usage.blob_count,
                report.usage.upload_bytes
            ),
            (body as u64, blobs as u64, upload as u64)
        );
    };
    let leased = |blobs: &[BlobId]| {
        sql(
            fixture,
            &format!(
                "SELECT (SELECT count(*) FROM td_mta.leases WHERE account={acc} AND blob_id IN ({0})),(SELECT count(*) FROM td_mta.blobs WHERE account={acc} AND id IN ({0}))",
                list(blobs)
            ),
        )
    };
    let read = |blob| {
        bodies::with_blob(
            request,
            deadline(),
            bodies::BlobRequest {
                binding,
                blob,
                maximum: bodies::MAX_BODY_BYTES,
            },
            password,
            |_, _| Ok(()),
        )
    };
    audited();
    let max = uploads::MAX_BYTES as i64;
    let old: Vec<BlobId> = sql(
        fixture,
        &format!("SELECT string_agg(encode(blob_id,'hex'),',' ORDER BY blob_id) FROM td_mta.leases WHERE account={acc}"),
    )
    .split(',')
    .map(|blob| BlobId::parse(blob).unwrap())
    .collect();
    let old_bytes: i64 = sql(
        fixture,
        &format!(
            "SELECT sum(length) FROM td_mta.blobs WHERE account={acc} AND id IN ({})",
            list(&old)
        ),
    )
    .parse()
    .unwrap();
    let old_count = old.len() as i64;
    // The second batch must stop at the byte bound before the large leases.
    assert!(6 + old_bytes + max > uploads::RECLAIM_BYTES && old_count + 6 < 64);

    // Fill the upload quota with attachments that cannot expire yet.
    let mut big = Vec::new();
    let refused = loop {
        let before = usage();
        match publish(&vec![0x61; uploads::MAX_BYTES]) {
            (Ok(_), blob) => big.push(blob),
            (Err(uploads::Error::Quota), _) => break before,
            (other, _) => panic!("{other:?}"),
        }
    };
    assert_eq!(big.len(), 2, "the default 128 MiB quota");
    assert_eq!(
        usage(),
        refused,
        "a refusal reclaiming nothing commits nothing"
    );
    let mut tiny = Vec::new();
    for _ in 0..70 {
        let (result, blob) = publish(b"t");
        result.unwrap();
        tiny.push(blob);
    }
    let (fresh, fresh_blob) = publish(b"fresh");
    let fresh = fresh.unwrap();
    // Another owner keeps each large body after its lease goes.
    sql(fixture, &format!("INSERT INTO td_mta.threads SELECT id,decode(repeat('5a',16),'hex'),sequence FROM td_mta.accounts WHERE id={acc}; INSERT INTO td_mta.emails(account,id,blob_id,thread_id,received_at,origin,changed) SELECT id,decode(repeat('5b',16),'hex'),decode('{}','hex'),decode(repeat('5a',16),'hex'),0,2,sequence FROM td_mta.accounts WHERE id={acc}; INSERT INTO td_mta.submissions(account,id,email_id,thread_id,identity_id,transmitted_blob_id,reverse_path,send_at,expires_at,recipient_count,completed_at,notification,notification_email_id,changed) SELECT id,decode(repeat('5c',16),'hex'),decode(repeat('5b',16),'hex'),decode(repeat('5a',16),'hex'),decode(repeat('5d',16),'hex'),decode('{}','hex'),'',0,1,1,1,0,NULL,sequence FROM td_mta.accounts WHERE id={acc}; UPDATE td_mta.account_usage SET queue_bytes=queue_bytes+{max},queue_submissions=queue_submissions+1 WHERE account={acc}", big[0], big[1]));
    audited();
    // Expiry order: tiny oldest first, then the old uploads, then the large.
    sql(fixture, &format!("UPDATE td_mta.leases l SET expires_at={NOW}-3000000+q.n FROM unnest(ARRAY[{}]) WITH ORDINALITY q(id,n) WHERE l.account={acc} AND l.blob_id=q.id; UPDATE td_mta.leases SET expires_at={NOW}-2000000 WHERE account={acc} AND blob_id IN ({}); UPDATE td_mta.leases SET expires_at={NOW}-1000000 WHERE account={acc} AND blob_id IN ({})", list(&tiny), list(&old), list(&big)));
    let fresh_expiry = sql(
        fixture,
        &format!("SELECT expires_at FROM td_mta.leases WHERE account={acc} AND blob_id=decode('{fresh_blob}','hex')"),
    );
    assert_eq!(fresh_expiry, fresh.expires_at.to_string());
    let last = *tiny.last().unwrap();
    assert_eq!(read(last), Err(bodies::Error::Missing));

    // 64 leases are one batch; too few bytes for this upload, yet the
    // refusal keeps the batch.
    let before = usage();
    assert_eq!(
        publish(&vec![0x62; uploads::MAX_BYTES]).0,
        Err(uploads::Error::Quota)
    );
    let [body, blobs, upload] = before;
    assert_eq!(usage(), [body - 64, blobs - 64, upload - 64]);
    assert_eq!(leased(&tiny[..64]), "0|0");
    assert_eq!(leased(&tiny[64..]), "6|6");
    assert_eq!(read(tiny[0]), Err(bodies::Error::Missing));
    assert_eq!(read(last), Err(bodies::Error::Missing));
    read(fresh_blob).unwrap();
    audited();

    // The rest of the tiny and old leases fit 64 MiB; the large do not.
    let [body, blobs, upload] = usage();
    let (result, kept) = publish(&vec![0x63; uploads::MAX_BYTES]);
    result.unwrap();
    let released = 6 + old_bytes;
    assert_eq!(
        usage(),
        [
            body - released + max,
            blobs - 6 - old_count + 1,
            upload - released + max
        ]
    );
    assert_eq!(leased(&tiny[64..]), "0|0");
    assert_eq!(leased(&old), "0|0");
    assert_eq!(leased(&big), "2|2");
    audited();

    // Referenced bodies stay: only their upload bytes leave.
    let [body, blobs, upload] = usage();
    let (result, _) = publish(b"s");
    result.unwrap();
    assert_eq!(usage(), [body + 1, blobs + 1, upload - 2 * max + 1]);
    assert_eq!(leased(&big), "0|2");
    assert_eq!(
        sql(
            fixture,
            &format!("SELECT expires_at FROM td_mta.leases WHERE account={acc} AND blob_id=decode('{fresh_blob}','hex')"),
        ),
        fresh_expiry
    );
    assert_eq!(leased(&[kept]), "1|1");
    read(fresh_blob).unwrap();
    audited();
}
