use super::{deadline, sql, wait_command, Fixture, ProbeChild, PASSWORD};
use std::{fs, process::Stdio};
use td_crypto::TrustStore;
use td_mta::{
    ids::{AccountId, StoreEpoch},
    postgres_accounts,
    postgres_maintenance::{self, ExternalFence},
    postgres_recovery::{self, Error, ExternalChecks, Plan},
    postgres_schema,
};

#[test]
#[ignore = "real local PostgreSQL 17 tools and private trusted TMPDIR required"]
pub(super) fn real_postgres_restore_quarantines_pending_delivery_and_changes_epoch() {
    let fixture = Fixture::new();
    let trust = TrustStore::from_pem(&fixture.ca).unwrap();
    let owner = fixture.request();
    let old = postgres_schema::initialize(&owner, &trust, deadline())
        .unwrap()
        .store_epoch;
    postgres_accounts::publish(
        &owner,
        &trust,
        deadline(),
        postgres_accounts::prepare(old, AccountId::from_bytes([1; 16])).unwrap(),
    )
    .unwrap();
    sql(
        &fixture,
        r#"
INSERT INTO td_mta.blob_ids VALUES(decode(repeat('01',16),'hex'),decode(repeat('02',16),'hex'));
INSERT INTO td_mta.blobs VALUES(decode(repeat('01',16),'hex'),decode(repeat('02',16),'hex'),0,decode('e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855','hex'),0,decode('0000000000000001','hex'),0);
INSERT INTO td_mta.submissions SELECT decode(repeat('01',16),'hex'),decode(lpad(to_hex(n),32,'0'),'hex'),decode(repeat('03',16),'hex'),decode(repeat('04',16),'hex'),decode(repeat('05',16),'hex'),decode(repeat('02',16),'hex'),'sender@example.test',0,432000000,1,CASE WHEN n IN(4,5,6,8) THEN 1 ELSE NULL END,CASE WHEN n IN(5,8) THEN 1 ELSE 0 END,NULL,decode('0000000000000001','hex') FROM generate_series(1,8) n;
INSERT INTO td_mta.recipients SELECT decode(repeat('01',16),'hex'),decode(lpad(to_hex(n),32,'0'),'hex'),0,'recipient@example.test',least(n,7),CASE WHEN n>=7 THEN 1 ELSE 0 END,CASE WHEN n IN(1,6) THEN NULL ELSE decode(repeat('09',16),'hex') END,CASE WHEN n IN(1,6) THEN 0 ELSE 1 END,CASE WHEN n IN(1,6) THEN NULL ELSE 0 END,CASE WHEN n IN(1,6) THEN 0 WHEN n IN(2,7) THEN 3 ELSE 4 END,CASE WHEN n IN(1,3,7) THEN 10 ELSE NULL END,CASE WHEN n IN(1,6) THEN NULL WHEN n=5 THEN '550 retained refusal' ELSE '250 retained RCPT' END,CASE WHEN n=4 THEN '250 retained DATA' ELSE NULL END,CASE n WHEN 3 THEN 3 WHEN 5 THEN 2 WHEN 6 THEN 7 WHEN 7 THEN 9 WHEN 8 THEN 6 ELSE 0 END,'',decode('0000000000000001','hex') FROM generate_series(1,8) n;
UPDATE td_mta.account_usage SET blob_count=1,queue_submissions=8;
INSERT INTO td_mta.devices VALUES(decode(repeat('01',16),'hex'),decode(repeat('08',16),'hex'),decode(repeat('11',32),'hex'),decode(repeat('22',32),'hex'),'restored device',0,decode(repeat('33',32),'hex'),NULL,NULL,NULL);
INSERT INTO td_mta.route_owners VALUES(decode(repeat('01',16),'hex'),'outbound',decode(repeat('aa',32),'hex'),decode('0000000000000001','hex'),decode(repeat('bb',16),'hex'),decode(repeat('cc',16),'hex'),0,1000);
INSERT INTO td_mta.submission_attempts VALUES(decode(repeat('01',16),'hex'),decode(repeat('09',16),'hex'),decode(repeat('dd',32),'hex'),decode(lpad(to_hex(2),32,'0'),'hex'),'outbound',decode(repeat('aa',32),'hex'),decode(repeat('bb',16),'hex'),decode(repeat('cc',16),'hex'),decode('0000000000000001','hex'),0,decode('00000000','hex'),decode('0000000000000001','hex'),decode('0000000000000002','hex'),true);
INSERT INTO td_mta.attempt_final_reservations SELECT account,attempt_id,ordinals FROM td_mta.submission_attempts;
"#,
    );
    // A real archive round trip restores both pending delivery and old access.
    let dump = fixture.root.join("mail.dump");
    let tool = |name: &str, extra: &[&str]| {
        let log = fs::File::create(fixture.root.join("restore-tool.log")).unwrap();
        let mut command = fixture.command(name);
        command
            .env("PGPASSWORD", std::str::from_utf8(PASSWORD).unwrap())
            .env("PGSSLMODE", "require")
            .args([
                "-h",
                "localhost",
                "-p",
                &fixture.port.to_string(),
                "-U",
                "td_probe",
            ]);
        let mut child = ProbeChild(
            command
                .args(extra)
                .stdout(log.try_clone().unwrap())
                .stderr(log)
                .stdin(Stdio::null())
                .spawn()
                .unwrap(),
        );
        wait_command(&mut child);
    };
    tool(
        "pg_dump",
        &["-Fc", "-f", dump.to_str().unwrap(), "postgres"],
    );
    sql(&fixture, "DROP SCHEMA td_mta CASCADE");
    tool(
        "pg_restore",
        &[
            "--exit-on-error",
            "--single-transaction",
            "-d",
            "postgres",
            dump.to_str().unwrap(),
        ],
    );
    assert_eq!(
        postgres_schema::inspect(&owner, &trust, deadline())
            .unwrap()
            .store_epoch,
        old
    );
    let account = AccountId::from_bytes([1; 16]);
    let before_usage =
        td_mta::postgres_usage_audit::inspect(&owner, &trust, deadline(), old, account)
            .unwrap()
            .usage;
    let plan = Plan {
        old_epoch: old,
        new_epoch: StoreEpoch::from_bytes([0x44; 16]),
        utc_ms: 100,
        maximum_rows: 100,
    };
    let release = |plan| {
        postgres_recovery::release(
            &owner,
            &trust,
            deadline(),
            plan,
            ExternalChecks::InstancesAndSmtpFencedAndConfigurationReviewed,
        )
    };
    assert_eq!(release(plan), Err(Error::BarrierRequired));
    postgres_maintenance::enter(
        &owner,
        &trust,
        deadline(),
        old,
        ExternalFence::InstancesAndSmtpEgressStopped,
    )
    .unwrap();
    assert_eq!(release(plan), Err(Error::ActiveCredentials));
    sql(&fixture,"UPDATE td_mta.devices SET revoked_at=1,revocation_id=decode(repeat('99',16),'hex'),revocation_digest=decode(repeat('88',32),'hex')");
    assert_eq!(
        release(Plan {
            maximum_rows: 1,
            ..plan
        }),
        Err(Error::Capacity)
    );
    assert_eq!(
        sql(
            &fixture,
            "SELECT count(*) FROM td_mta.recipients WHERE state IN(1,2,3)"
        ),
        "3"
    );
    sql(&fixture,"CREATE FUNCTION public.refuse_release() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NOT NEW.blocked THEN RAISE EXCEPTION 'test rollback'; END IF; RETURN NEW; END $$; CREATE TRIGGER refuse_release BEFORE UPDATE ON td_mta.runtime_barrier FOR EACH ROW EXECUTE FUNCTION public.refuse_release()");
    assert_eq!(release(plan), Err(Error::Database));
    assert_eq!(
        sql(
            &fixture,
            "SELECT count(*) FROM td_mta.recipients WHERE state IN(1,2,3)"
        ),
        "3"
    );
    assert_eq!(
        postgres_schema::inspect(&owner, &trust, deadline())
            .unwrap()
            .store_epoch,
        old
    );
    sql(
        &fixture,
        "DROP TRIGGER refuse_release ON td_mta.runtime_barrier",
    );
    let report = release(plan).unwrap();
    assert_eq!(report.quarantined_recipients, 4);
    assert_eq!(report.completed_submissions, 4);
    assert_eq!(report.epoch, plan.new_epoch);
    assert_eq!(
        td_mta::postgres_usage_audit::inspect(&owner, &trust, deadline(), plan.new_epoch, account)
            .unwrap()
            .usage,
        before_usage
    );
    assert!(td_mta::postgres_blob_audit::inspect(
        &owner,
        &trust,
        deadline(),
        plan.new_epoch,
        account,
        td_mta::ids::BlobId::from_bytes([2; 16])
    )
    .is_ok());
    assert_eq!(
        sql(
            &fixture,
            "SELECT count(*) FROM td_mta.submission_attempts WHERE active"
        ),
        "0"
    );
    assert_eq!(
        sql(
            &fixture,
            "SELECT count(*) FROM td_mta.route_owners WHERE owner_boot IS NOT NULL"
        ),
        "0"
    );
    assert_eq!(
        sql(
            &fixture,
            "SELECT count(*) FROM td_mta.attempt_final_reservations"
        ),
        "0"
    );
    assert_eq!(
        sql(
            &fixture,
            "SELECT count(*) FROM td_mta.recipients WHERE next_attempt_at IS NOT NULL"
        ),
        "0"
    );
    assert_eq!(sql(&fixture,"SELECT attempt_count,phase,reason FROM td_mta.recipients WHERE submission_id=decode(lpad(to_hex(1),32,'0'),'hex')"),"0|0|10");
    assert_eq!(
        sql(
            &fixture,
            "SELECT count(*) FROM td_mta.recipients WHERE state IN(1,2,3)"
        ),
        "0"
    );
    assert_eq!(
        sql(
            &fixture,
            "SELECT count(*) FROM td_mta.recipients WHERE state=7 AND uncertain=1"
        ),
        "5"
    );
    assert_eq!(sql(&fixture,"SELECT string_agg(state::text,',' ORDER BY ordinal,submission_id) FROM td_mta.recipients"),"7,7,7,4,5,6,7,7");
    assert_eq!(
        sql(
            &fixture,
            "SELECT count(*) FROM td_mta.recipients WHERE rcpt_reply='250 retained RCPT'"
        ),
        "5"
    );
    assert_eq!(sql(&fixture,"SELECT attempt_count,phase,next_attempt_at IS NULL FROM td_mta.recipients WHERE submission_id=decode(lpad(to_hex(2),32,'0'),'hex')"),"1|4|t");
    assert_eq!(
        sql(
            &fixture,
            "SELECT count(*) FROM td_mta.submissions WHERE completed_at=100 AND notification=1"
        ),
        "4"
    );
    assert_eq!(
        sql(
            &fixture,
            "SELECT bool_and(floor=sequence) FROM td_mta.accounts"
        ),
        "t"
    );
    assert_eq!(
        postgres_maintenance::inspect(&owner, &trust, deadline(), plan.new_epoch),
        Ok(false)
    );
    assert_eq!(release(plan), Err(Error::Epoch));
}

#[test]
#[ignore = "real local PostgreSQL tools required"]
pub(super) fn real_postgres_recovery_uncertain_commit_keeps_retained_new_epoch() {
    use std::{
        thread,
        time::{Duration, Instant},
    };
    let fixture = Fixture::new();
    let trust = TrustStore::from_pem(&fixture.ca).unwrap();
    let owner = fixture.request();
    let old = postgres_schema::initialize(&owner, &trust, deadline())
        .unwrap()
        .store_epoch;
    postgres_maintenance::enter(
        &owner,
        &trust,
        deadline(),
        old,
        ExternalFence::InstancesAndSmtpEgressStopped,
    )
    .unwrap();
    sql(&fixture,"CREATE FUNCTION public.delay_recovery_commit() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_catalog.pg_sleep(6); RETURN NEW; END $$; CREATE CONSTRAINT TRIGGER delay_recovery_commit AFTER UPDATE ON td_mta.runtime_barrier DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.delay_recovery_commit()");
    let plan = Plan {
        old_epoch: old,
        new_epoch: StoreEpoch::from_bytes([0x55; 16]),
        utc_ms: 100,
        maximum_rows: 100,
    };
    assert_eq!(
        postgres_recovery::release(
            &owner,
            &trust,
            Instant::now() + Duration::from_secs(2),
            plan,
            ExternalChecks::InstancesAndSmtpFencedAndConfigurationReviewed
        ),
        Err(Error::Indeterminate)
    );
    let stop = Instant::now() + Duration::from_secs(8);
    while sql(
        &fixture,
        "SELECT count(*) FROM pg_catalog.pg_stat_activity WHERE application_name='td-mta-postgres'",
    ) != "0"
    {
        assert!(Instant::now() < stop);
        thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(
        postgres_schema::inspect(&owner, &trust, deadline())
            .unwrap()
            .store_epoch,
        plan.new_epoch
    );
    assert_eq!(
        postgres_maintenance::inspect(&owner, &trust, deadline(), plan.new_epoch),
        Ok(false)
    );
    assert_eq!(
        postgres_recovery::release(
            &owner,
            &trust,
            deadline(),
            plan,
            ExternalChecks::InstancesAndSmtpFencedAndConfigurationReviewed
        ),
        Err(Error::Epoch)
    );
}
