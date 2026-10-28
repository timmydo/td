use super::{certificate, create_runtime_login, deadline, grant_runtime_mail, sql, Fixture};
use std::{
    sync::RwLock,
    time::{Duration, Instant},
};
use td_crypto::{Crypto, Provider, TrustStore};
use td_mta::{
    acme::{
        challenge::Current,
        order::{Persistence, State},
        Error,
    },
    postgres_acme::{self as acme, Claim, Writer},
    postgres_schema,
};

#[test]
#[ignore = "real PostgreSQL 17 tools and private trusted TMPDIR required"]
pub(super) fn real_postgres_acme_fences_ownership_and_pins_complete_pairs() {
    let fixture = Fixture::new();
    let trust = TrustStore::from_pem(&fixture.ca).unwrap();
    let epoch = postgres_schema::initialize(&fixture.request(), &trust, deadline())
        .unwrap()
        .store_epoch;
    sql(
        &fixture,
        "DROP TABLE td_mta.replica_email_receipts,td_mta.replica_blob_identities,td_mta.replica_accounts,td_mta.replica_progress,td_mta.replica_pending,td_mta.replica_mailbox_receipts,td_mta.replica_source,td_mta.blob_search,td_mta.blob_interpretations,td_mta.blob_summaries,td_mta.acme_profiles; DELETE FROM td_mta.schema_migrations WHERE version>=30",
    );
    let upgraded = postgres_schema::upgrade(&fixture.request(), &trust, deadline()).unwrap();
    assert_eq!(upgraded.version, 39);
    assert_eq!(upgraded.store_epoch, epoch);
    create_runtime_login(&fixture);
    grant_runtime_mail(&fixture);
    sql(&fixture, "ALTER ROLE mail_runtime SET synchronous_commit=off; CREATE FUNCTION td_mta.require_acme_durability() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF current_setting('synchronous_commit') <> 'on' THEN RAISE EXCEPTION 'asynchronous checkpoint'; END IF; RETURN NEW; END $$; CREATE TRIGGER require_acme_durability BEFORE INSERT OR UPDATE ON td_mta.acme_profiles FOR EACH ROW EXECUTE FUNCTION td_mta.require_acme_durability()");
    let mut request = fixture.request();
    request.user = "mail_runtime";
    request.mode = td_mta::postgres_probe::Mode::Runtime;
    let initial = State::initial().unwrap();
    let claim = |owner, settings| {
        acme::claim(
            &request,
            &trust,
            deadline(),
            epoch,
            Claim {
                profile: "public",
                settings,
                owner,
            },
            &initial,
        )
    };
    let lease = claim([1; 16], [2; 32]).unwrap();
    assert!(matches!(claim([3; 16], [2; 32]), Err(Error::Conflict)));
    assert!(matches!(claim([1; 16], [4; 32]), Err(Error::Conflict)));
    let current = RwLock::new(Current {
        names: vec!["localhost".into()],
        token: String::new(),
        authorization: String::new(),
        expires: Instant::now() + Duration::from_secs(150),
    });
    let mut writer = Writer {
        request: &request,
        trust: &trust,
        epoch,
        profile: "public",
        owner: [1; 16],
        revision: lease.revision,
        challenge: &current,
    };
    let mut state = lease.state;
    state.account = "https://localhost/account".into();
    writer.save(&state, None).unwrap();
    let key = Provider
        .load_p256_pem(state.certificate_key.as_bytes())
        .unwrap();
    let mut raw = [0; td_crypto::P256_PKCS8_CAPACITY];
    let n = Provider.generate_p256(&mut raw).unwrap();
    let root = Provider.load_p256(&raw[..n]).unwrap();
    let mut chain = certificate::pem(
        "CERTIFICATE",
        &certificate::certificate_names(&key, &root, false, 2, &["localhost"], false),
    );
    chain.extend(certificate::pem(
        "CERTIFICATE",
        &certificate::certificate_names(&root, &root, true, 1, &[], false),
    ));
    writer
        .save(&state, Some((&chain, state.certificate_key.as_bytes())))
        .unwrap();
    let pair = acme::active(&request, &trust, deadline(), epoch, "public").unwrap();
    assert_eq!(pair.generation, 1);
    assert_eq!(pair.chain, chain);
    assert_eq!(pair.key, state.certificate_key.as_bytes());
    assert!(writer
        .save(
            &state,
            Some((b"not a certificate", state.certificate_key.as_bytes()))
        )
        .is_err());
    assert_eq!(
        acme::active(&request, &trust, deadline(), epoch, "public")
            .unwrap()
            .generation,
        1
    );
    let newer = claim([1; 16], [2; 32]).unwrap();
    assert_eq!(newer.state.account, state.account);
    assert_eq!(writer.save(&state, None), Err(Error::Conflict));
    sql(
        &fixture,
        "UPDATE td_mta.acme_profiles SET lease_until=clock_timestamp()-interval '1 second'",
    );
    let next = claim([3; 16], [2; 32]).unwrap();
    assert!(next.revision > newer.revision);
    let old = acme::active(&request, &trust, deadline(), epoch, "public").unwrap();
    assert_eq!(old.key, pair.key);
    for index in 1..16 {
        acme::claim(
            &request,
            &trust,
            deadline(),
            epoch,
            Claim {
                profile: &format!("cert{index}"),
                settings: [2; 32],
                owner: [3; 16],
            },
            &initial,
        )
        .unwrap();
    }
    assert!(acme::claim(
        &request,
        &trust,
        deadline(),
        epoch,
        Claim {
            profile: "overflow",
            settings: [2; 32],
            owner: [3; 16]
        },
        &initial
    )
    .is_err());
    assert_eq!(
        sql(&fixture, "SELECT count(*) FROM td_mta.acme_profiles"),
        "16"
    );
}

pub(super) fn publish_fixture_pair(fixture: &Fixture) {
    let trust = TrustStore::from_pem(&fixture.ca).unwrap();
    let request = fixture.request();
    let epoch = postgres_schema::inspect(&request, &trust, deadline())
        .unwrap()
        .store_epoch;
    let settings = td_mta::acme::order::Settings {
        directory: "https://acme.example.test/directory".into(),
        contact: "operator@example.test".into(),
        names: vec!["localhost".into()],
    };
    let lease = acme::claim(
        &request,
        &trust,
        deadline(),
        epoch,
        Claim {
            profile: "public",
            settings: settings.identity().unwrap(),
            owner: [90; 16],
        },
        &State::initial().unwrap(),
    )
    .unwrap();
    let current = RwLock::new(Current {
        names: settings.names,
        token: String::new(),
        authorization: String::new(),
        expires: Instant::now() + Duration::from_secs(150),
    });
    let mut writer = Writer {
        request: &request,
        trust: &trust,
        epoch,
        profile: "public",
        owner: [90; 16],
        revision: lease.revision,
        challenge: &current,
    };
    let chain = std::fs::read(fixture.root.join("server.crt")).unwrap();
    let key = std::fs::read(fixture.root.join("server.key")).unwrap();
    writer.save(&lease.state, Some((&chain, &key))).unwrap();
}
