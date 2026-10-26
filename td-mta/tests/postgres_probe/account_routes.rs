use super::{command_output, deadline, sql, Fixture};
use td_crypto::TrustStore;
use td_mta::{
    compose_text::{Address, Message},
    config::profile::Roles,
    generations::GenerationSet,
    ids::{AccountId, IdentityId, StoreEpoch},
    postgres_accounts as accounts, postgres_configuration as configuration,
    postgres_configuration_store as versions, postgres_devices as devices,
    postgres_drafts as drafts, postgres_runtime_profile as runtime,
    postgres_selection as selection, postgres_submission_input as input,
    postgres_submissions as submissions,
    tls_policy::TlsPolicies,
};

pub(super) fn exercise(
    fixture: &Fixture,
    request: &td_mta::postgres_probe::Request<'_>,
    trust: &TrustStore,
    epoch: StoreEpoch,
    shared: &str,
    frontend: &str,
    connection: &std::path::Path,
) {
    let frontend = frontend.replace(
        "[server]",
        "[server]\npostmaster_account = \"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"",
    );
    let a = AccountId::from_bytes([0xaa; 16]);
    let b = AccountId::from_bytes([0xcc; 16]);
    let shared = shared.replace(
        "username = \"user\"",
        "username = \"user\"\noutbound_profile = \"migadu\"",
    );
    let shared = format!("{shared}\n[account \"{b}\"]\nusername = \"other\"\n[alias \"second@example.test\"]\naccount = \"{b}\"\n[identity \"dddddddddddddddddddddddddddddddd\"]\naccount = \"{b}\"\nemail = \"second@example.test\"\n");
    let alternate = frontend
        .split_once("[certificate")
        .unwrap()
        .0
        .replace("personal-smarthost", "production-route")
        .replace("relay-user", "production-user");
    let profiles = [
        configuration::Profile {
            name: "front",
            roles: Roles::from_bits(6).unwrap(),
            source: frontend.as_bytes(),
        },
        configuration::Profile {
            name: "migadu",
            roles: Roles::from_bits(4).unwrap(),
            source: alternate.as_bytes(),
        },
    ];
    let prepare = |source: &str, profiles: &[configuration::Profile<'_>]| {
        configuration::prepare(
            request,
            trust,
            deadline(),
            epoch,
            source.as_bytes(),
            profiles,
        )
    };
    for target in ["missing", "read-only"] {
        let invalid = shared.replace(
            "outbound_profile = \"migadu\"",
            &format!("outbound_profile = \"{target}\""),
        );
        let mut bad_profiles = profiles.to_vec();
        bad_profiles.push(configuration::Profile {
            name: "read-only",
            roles: Roles::from_bits(2).unwrap(),
            source: frontend.as_bytes(),
        });
        assert_eq!(
            prepare(&invalid, &bad_profiles).err(),
            Some(configuration::Error::OutboundProfile)
        );
    }
    let candidate = prepare(&shared, &profiles).unwrap();
    assert_eq!(
        candidate.submission_route(a, "front"),
        candidate.route("migadu")
    );
    assert_eq!(
        candidate.submission_route(b, "front"),
        candidate.route("front")
    );
    accounts::publish(
        request,
        trust,
        deadline(),
        accounts::prepare(epoch, b).unwrap(),
    )
    .unwrap();
    let version = versions::Version::from_bytes([81; 16]);
    versions::publish(request, trust, deadline(), version, &candidate).unwrap();
    selection::publish(
        request,
        trust,
        deadline(),
        selection::prepare(epoch, 1, version, &candidate).unwrap(),
    )
    .unwrap();
    let domain = GenerationSet::<TlsPolicies>::at_startup();
    let front = runtime::load(
        request,
        trust,
        deadline(),
        epoch,
        "front",
        domain.reserve().unwrap(),
    )
    .unwrap();
    let relay_domain = GenerationSet::<TlsPolicies>::at_startup();
    let relay = runtime::load(
        request,
        trust,
        deadline(),
        epoch,
        "migadu",
        relay_domain.reserve().unwrap(),
    )
    .unwrap();
    for (account, identity, address, expected_route) in [
        (
            a,
            IdentityId::from_bytes([0xbb; 16]),
            "main@example.test",
            "production-route",
        ),
        (
            b,
            IdentityId::from_bytes([0xdd; 16]),
            "second@example.test",
            "personal-smarthost",
        ),
    ] {
        let binding = front.binding_for(account).unwrap();
        assert_eq!(binding.route().name(), "personal-smarthost");
        assert_eq!(binding.submission_route().name(), expected_route);
        assert_eq!(
            binding.serves_route(binding.submission_route()),
            account == b
        );
        assert_eq!(
            relay
                .binding_for(account)
                .unwrap()
                .serves_route(binding.submission_route()),
            account == a
        );
        let device = devices::prepare_creation(epoch, account, "routing", 1).unwrap();
        devices::publish_creation(request, trust, deadline(), &device).unwrap();
        let mailboxes = td_mta::postgres_mailboxes::list(
            request,
            trust,
            deadline(),
            epoch,
            account,
            device.password(),
        )
        .unwrap();
        let mailbox = mailboxes
            .mailboxes
            .iter()
            .find(|mailbox| mailbox.role.as_deref() == Some("inbox"))
            .unwrap()
            .id;
        let recipients = [Address {
            name: "",
            email: "recipient@example.test",
        }];
        let message = Message {
            from: Address {
                name: "",
                email: address,
            },
            to: &recipients,
            cc: &[],
            bcc: &[],
            reply_to: &[],
            subject: "Route selection",
            message_id: address,
            in_reply_to: &[],
            references: &[],
            sent_at_ms: 0,
            text: "route fixture",
        };
        let draft = drafts::prepare(
            drafts::DraftRequest {
                binding,
                device: device.device_id(),
                mailbox,
                expected: None,
            },
            &message,
            deadline(),
        )
        .unwrap();
        let draft = drafts::publish(request, trust, deadline(), &draft, device.password()).unwrap();
        let prepared = input::prepare(
            request,
            trust,
            deadline(),
            input::Input {
                binding,
                email: draft.email,
                identity,
                expected: None,
            },
            device.password(),
        )
        .unwrap();
        let submission = submissions::prepare(prepared, deadline()).unwrap();
        assert_eq!(submission.route(), binding.submission_route());
        submissions::publish(request, trust, deadline(), &submission, device.password()).unwrap();
        assert_eq!(sql(fixture, &format!("SELECT route_name FROM td_mta.submission_routes WHERE account=decode('{account}','hex')")), expected_route);
        assert_eq!(sql(fixture, &format!("SELECT encode(destination,'hex') FROM td_mta.submission_routes WHERE account=decode('{account}','hex')")), binding.submission_route().destination().iter().map(|b| format!("{b:02x}")).collect::<String>());
    }
    idle_hints(
        fixture,
        request,
        trust,
        front.binding_for(a).unwrap(),
        relay.binding_for(a).unwrap(),
    );
    let operation = fixture.root.join("routing.op");
    command_output(
        &[
            "postgres",
            "account-update",
            "--connection-file",
            connection.to_str().unwrap(),
            "--account",
            &a.to_string(),
            "--outbound-profile",
            "front",
            "--operation-file",
            operation.to_str().unwrap(),
        ],
        &fixture.root,
        0,
    );
    let fresh = runtime::load(
        request,
        trust,
        deadline(),
        epoch,
        "front",
        domain.reserve().unwrap(),
    )
    .unwrap();
    assert_eq!(
        fresh.binding_for(a).unwrap().submission_route().name(),
        "personal-smarthost"
    );
    assert_eq!(
        sql(
            fixture,
            &format!(
                "SELECT route_name FROM td_mta.submission_routes WHERE account=decode('{a}','hex')"
            )
        ),
        "production-route"
    );
    let listing = command_output(
        &[
            "postgres",
            "account-list",
            "--connection-file",
            connection.to_str().unwrap(),
        ],
        &fixture.root,
        0,
    );
    assert!(listing.contains("\"outbound_profile\":\"front\""));
    let clear = fixture.root.join("routing-clear.op");
    command_output(
        &[
            "postgres",
            "account-update",
            "--connection-file",
            connection.to_str().unwrap(),
            "--account",
            &a.to_string(),
            "--outbound-profile",
            "",
            "--operation-file",
            clear.to_str().unwrap(),
        ],
        &fixture.root,
        0,
    );
    let listing = command_output(
        &[
            "postgres",
            "account-list",
            "--connection-file",
            connection.to_str().unwrap(),
        ],
        &fixture.root,
        0,
    );
    assert!(!listing.contains("outbound_profile"));
}

fn idle_hints(
    fixture: &Fixture,
    request: &td_mta::postgres_probe::Request<'_>,
    trust: &TrustStore,
    front: &runtime::Binding,
    relay: &runtime::Binding,
) {
    use td_mta::{
        ids::BootId, postgres_queue_candidates as hints, postgres_route_owners as owners,
    };
    let front_claim = owners::prepare(front, BootId::from_bytes([91; 16])).unwrap();
    let relay_claim = owners::prepare(relay, BootId::from_bytes([92; 16])).unwrap();
    let front_grant = owners::acquire(request, trust, deadline(), &front_claim).unwrap();
    let relay_grant = owners::acquire(request, trust, deadline(), &relay_claim).unwrap();
    let idle = |claim: &owners::Claim, generation| {
        hints::idle_observation(request, trust, deadline(), claim, generation)
            .unwrap()
            .is_some()
    };
    let account = relay.account();
    // Another account has front-route work, while this account has relay work.
    assert!(idle(&front_claim, front_grant.generation));
    assert!(!idle(&relay_claim, relay_grant.generation));
    assert!(matches!(
        hints::idle_observation(
            request,
            trust,
            deadline(),
            &relay_claim,
            relay_grant.generation + 1
        ),
        Err(hints::Error::Owner(owners::Error::Stale))
    ));
    assert!(hints::idle_observation(
        request,
        trust,
        std::time::Instant::now(),
        &relay_claim,
        relay_grant.generation
    )
    .is_err());
    let original_send = sql(
        fixture,
        &format!("SELECT send_at FROM td_mta.submissions WHERE account=decode('{account}','hex')"),
    );
    sql(fixture, &format!("UPDATE td_mta.submissions SET send_at=expires_at WHERE account=decode('{account}','hex')"));
    assert!(!idle(&relay_claim, relay_grant.generation));
    sql(fixture, &format!("UPDATE td_mta.submissions SET send_at={original_send},completed_at=1 WHERE account=decode('{account}','hex')"));
    assert!(idle(&relay_claim, relay_grant.generation));
    assert!(idle(&front_claim, front_grant.generation));
    // Completed notices must survive an outbound-route change.
    sql(
        fixture,
        &format!(
            "UPDATE td_mta.submissions SET notification=1 WHERE account=decode('{account}','hex')"
        ),
    );
    assert!(!idle(&front_claim, front_grant.generation));
    assert!(!idle(&relay_claim, relay_grant.generation));
    sql(
        fixture,
        &format!(
            "UPDATE td_mta.submissions SET notification=0 WHERE account=decode('{account}','hex')"
        ),
    );
    // A retained active attempt remains recovery work independently of a
    // submission's completion marker. No publication is authorized by this hint.
    sql(fixture, &format!("INSERT INTO td_mta.submission_attempts SELECT account,decode(repeat('5b',16),'hex'),decode(repeat('00',32),'hex'),submission_id,route_name,destination,decode(repeat('5c',16),'hex'),decode(repeat('5d',16),'hex'),decode('0000000000000001','hex'),0,decode('00000000','hex'),decode('0000000000000001','hex'),decode('0000000000000002','hex'),true FROM td_mta.submission_routes WHERE account=decode('{account}','hex')"));
    assert!(!idle(&relay_claim, relay_grant.generation));
    assert!(idle(&front_claim, front_grant.generation));
    sql(
        fixture,
        "DELETE FROM td_mta.submission_attempts WHERE attempt_id=decode(repeat('5b',16),'hex')",
    );
    assert!(idle(&relay_claim, relay_grant.generation));
    // A new round observes work added immediately after an empty observation.
    sql(fixture, &format!("UPDATE td_mta.submissions SET completed_at=NULL WHERE account=decode('{account}','hex')"));
    assert!(!idle(&relay_claim, relay_grant.generation));
    owners::release(
        request,
        trust,
        deadline(),
        &front_claim,
        front_grant.generation,
    )
    .unwrap();
    owners::release(
        request,
        trust,
        deadline(),
        &relay_claim,
        relay_grant.generation,
    )
    .unwrap();
    assert!(hints::idle_observation(
        request,
        trust,
        deadline(),
        &relay_claim,
        relay_grant.generation
    )
    .is_err());
    scoped_qualification(fixture, request, trust, relay);
}

fn scoped_qualification(
    fixture: &Fixture,
    preparation: &td_mta::postgres_probe::Request<'_>,
    trust: &TrustStore,
    binding: &runtime::Binding,
) {
    use td_mta::{
        ids::BootId, postgres_probe as probe, postgres_queue_candidates as hints,
        postgres_route_owners as owners, postgres_schema as schema,
    };
    super::create_runtime_login(fixture);
    super::grant_runtime_mail(fixture);
    let mut runtime = fixture.request();
    runtime.user = "mail_runtime";
    runtime.mode = probe::Mode::Runtime;
    let claim = owners::prepare(binding, BootId::from_bytes([93; 16])).unwrap();
    let grant = owners::acquire(preparation, trust, deadline(), &claim).unwrap();
    let idle =
        |request| hints::idle_observation(request, trust, deadline(), &claim, grant.generation);
    let renew =
        |request| owners::renew_many(request, trust, deadline(), &[(&claim, grant.generation)]);

    // Count executed ledger reads on real sessions, rather than trusting a
    // source pattern or a mock that cannot observe recursive session locks.
    sql(fixture, "ALTER SYSTEM SET log_statement='all'");
    sql(fixture, "SELECT pg_reload_conf()");
    let until = deadline();
    while sql(fixture, "SHOW log_statement") != "all" {
        assert!(std::time::Instant::now() < until);
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    for request in [preparation, &runtime] {
        for renewal in [false, true] {
            let offset = std::fs::metadata(fixture.root.join("server.log"))
                .unwrap()
                .len() as usize;
            if renewal {
                renew(request).unwrap();
            } else {
                idle(request).unwrap();
            }
            super::body_checks::quiet(fixture);
            let log = std::fs::read_to_string(fixture.root.join("server.log")).unwrap();
            let reads = log[offset..].lines().filter(|line| {
                line.contains("execute ") && line.contains("SELECT version, checksum, store_epoch FROM td_mta.schema_migrations ORDER BY version")
            }).count();
            assert_eq!(reads, 1, "one schema qualification per operation");
            assert_eq!(
                sql(
                    fixture,
                    &format!(
                        "SELECT pg_catalog.pg_try_advisory_xact_lock({})",
                        schema::MIGRATION_KEY
                    )
                ),
                "t"
            );
        }
    }
    retained_idle(fixture, &runtime, trust, &claim, grant.generation);
    retained_renewal(fixture, &runtime, trust, &claim, grant.generation);
    sql(fixture, "ALTER SYSTEM RESET log_statement");
    sql(fixture, "SELECT pg_reload_conf()");

    let checksum = sql(
        fixture,
        "SELECT encode(checksum,'hex') FROM td_mta.schema_migrations WHERE version=1",
    );
    sql(fixture, "UPDATE td_mta.schema_migrations SET checksum=decode(repeat('00',32),'hex') WHERE version=1");
    assert_eq!(
        idle(&runtime),
        Err(hints::Error::Connection(probe::Error::RuntimeSchema))
    );
    assert_eq!(
        renew(&runtime),
        Err(owners::Error::Connection(probe::Error::RuntimeSchema))
    );
    assert_eq!(
        idle(preparation),
        Err(hints::Error::Owner(owners::Error::Schema(
            schema::Error::Version
        )))
    );
    assert_eq!(
        renew(preparation),
        Err(owners::Error::Schema(schema::Error::Version))
    );
    sql(fixture, &format!("UPDATE td_mta.schema_migrations SET checksum=decode('{checksum}','hex') WHERE version=1"));

    let epoch = sql(
        fixture,
        "SELECT encode(store_epoch,'hex') FROM td_mta.schema_migrations LIMIT 1",
    );
    sql(
        fixture,
        "UPDATE td_mta.schema_migrations SET store_epoch=decode(repeat('ee',16),'hex')",
    );
    for request in [preparation, &runtime] {
        assert_eq!(
            idle(request),
            Err(hints::Error::Owner(owners::Error::Epoch))
        );
        assert_eq!(renew(request), Err(owners::Error::Epoch));
    }
    sql(
        fixture,
        &format!("UPDATE td_mta.schema_migrations SET store_epoch=decode('{epoch}','hex')"),
    );
    for (change, restore, error) in [
        (
            "UPDATE td_mta.runtime_barrier SET blocked=true",
            "UPDATE td_mta.runtime_barrier SET blocked=false",
            probe::Error::RuntimeMaintenance,
        ),
        (
            "ALTER ROLE mail_runtime CREATEDB",
            "ALTER ROLE mail_runtime NOCREATEDB",
            probe::Error::RuntimeRole,
        ),
        (
            "REVOKE SELECT ON td_mta.emails FROM mail_runtime",
            "GRANT SELECT ON td_mta.emails TO mail_runtime",
            probe::Error::RuntimePrivileges,
        ),
    ] {
        renew(&runtime).unwrap();
        sql(fixture, change);
        assert_eq!(idle(&runtime), Err(hints::Error::Connection(error)));
        assert_eq!(renew(&runtime), Err(owners::Error::Connection(error)));
        sql(fixture, restore);
        idle(&runtime).unwrap();
    }
    super::body_checks::quiet(fixture);
    assert_eq!(
        sql(
            fixture,
            &format!(
                "SELECT pg_catalog.pg_try_advisory_xact_lock({})",
                schema::MIGRATION_KEY
            )
        ),
        "t"
    );
    owners::release_many(&runtime, trust, deadline(), &[(&claim, grant.generation)]).unwrap();
}

fn retained_idle(
    fixture: &Fixture,
    request: &td_mta::postgres_probe::Request<'_>,
    trust: &TrustStore,
    claim: &td_mta::postgres_route_owners::Claim,
    generation: u64,
) {
    use td_mta::{
        postgres_queue_candidates::{self as hints, IdleReader},
        postgres_read::ReadSlots,
        postgres_route_owners as owners, postgres_schema,
    };
    owners::renew(request, trust, deadline(), claim, generation).unwrap();
    let expected = hints::idle_observation(request, trust, deadline(), claim, generation)
        .unwrap()
        .is_some();
    super::body_checks::quiet(fixture);
    let offset = std::fs::metadata(fixture.root.join("server.log"))
        .unwrap()
        .len() as usize;
    let slots = ReadSlots::new(1).unwrap();
    let mut reader = IdleReader::new(&slots, request, trust);
    assert_eq!(
        reader
            .observe(deadline(), claim, generation)
            .unwrap()
            .is_some(),
        expected
    );
    let pid = super::body_checks::backend(fixture);
    for _ in 1..64 {
        assert_eq!(
            reader
                .observe(deadline(), claim, generation)
                .unwrap()
                .is_some(),
            expected
        );
    }
    assert_eq!(super::body_checks::backend(fixture), pid);
    assert_eq!(sql(fixture, &format!("SELECT state||'|'||(xact_start IS NULL)::text FROM pg_catalog.pg_stat_activity WHERE pid={pid}")), "idle|true");
    assert!(sql(fixture, &format!("BEGIN; SELECT pg_catalog.pg_try_advisory_xact_lock({}); SELECT id FROM td_mta.accounts FOR UPDATE NOWAIT; SELECT singleton FROM td_mta.config_selection FOR UPDATE NOWAIT; SELECT account FROM td_mta.route_owners FOR UPDATE NOWAIT; ROLLBACK", postgres_schema::MIGRATION_KEY)).lines().any(|line| line == "f"));
    let log = std::fs::read_to_string(fixture.root.join("server.log")).unwrap();
    let reads = log[offset..].lines().filter(|line| line.contains("execute ") && line.contains("SELECT version, checksum, store_epoch FROM td_mta.schema_migrations ORDER BY version")).count();
    assert_eq!(
        reads, 1,
        "one ledger qualification across 64 idle observations"
    );
    let mut other = IdleReader::new(&slots, request, trust);
    assert_eq!(
        other.observe(deadline(), claim, generation),
        Err(hints::Error::Busy)
    );
    assert_eq!(
        reader
            .observe(deadline(), claim, generation)
            .unwrap()
            .is_some(),
        expected
    );
    super::body_checks::closed(fixture, &pid);
    assert_ne!(super::body_checks::backend(fixture), pid);
    let pid = super::body_checks::backend(fixture);
    assert!(matches!(
        reader.observe(deadline(), claim, generation + 1),
        Err(hints::Error::Owner(owners::Error::Stale))
    ));
    super::body_checks::closed(fixture, &pid);
    assert!(other.observe(deadline(), claim, generation).is_ok());
    other.close();
    assert!(reader.observe(deadline(), claim, generation).is_ok());
    let pid = super::body_checks::backend(fixture);
    sql(fixture, "UPDATE td_mta.runtime_barrier SET blocked=true");
    assert_eq!(
        reader.observe(deadline(), claim, generation),
        Err(hints::Error::Connection(
            td_mta::postgres_probe::Error::RuntimeMaintenance
        ))
    );
    super::body_checks::closed(fixture, &pid);
    sql(fixture, "UPDATE td_mta.runtime_barrier SET blocked=false");
    assert!(reader.observe(deadline(), claim, generation).is_ok());
    let pid = super::body_checks::backend(fixture);
    drop(reader);
    super::body_checks::closed(fixture, &pid);
    assert_eq!(
        sql(
            fixture,
            &format!(
                "SELECT pg_catalog.pg_try_advisory_xact_lock({})",
                postgres_schema::MIGRATION_KEY
            )
        ),
        "t"
    );
    assert!(other.observe(deadline(), claim, generation).is_ok());
    other.close();
    idle_account_contention(fixture, request, trust, claim, generation);
}

fn retained_renewal(
    fixture: &Fixture,
    request: &td_mta::postgres_probe::Request<'_>,
    trust: &TrustStore,
    claim: &td_mta::postgres_route_owners::Claim,
    generation: u64,
) {
    use super::body_checks::{backend, closed, quiet};
    use td_mta::{postgres_probe as probe, postgres_route_owners as owners, postgres_schema};
    quiet(fixture);
    let offset = std::fs::metadata(fixture.root.join("server.log"))
        .unwrap()
        .len() as usize;
    let mut renewer = owners::Renewer::default();
    let leases = [(claim, generation)];
    renewer.renew(request, trust, deadline(), &leases).unwrap();
    let pid = backend(fixture);
    for _ in 1..15 {
        renewer.renew(request, trust, deadline(), &leases).unwrap();
    }
    assert_eq!(backend(fixture), pid);
    assert_eq!(sql(fixture, &format!("SELECT state||'|'||(xact_start IS NULL)::text FROM pg_catalog.pg_stat_activity WHERE pid={pid}")), "idle|true");
    assert!(sql(fixture, &format!("BEGIN; SELECT pg_catalog.pg_try_advisory_xact_lock({}); SELECT id FROM td_mta.accounts FOR UPDATE NOWAIT; SELECT singleton FROM td_mta.config_selection FOR UPDATE NOWAIT; SELECT account FROM td_mta.route_owners FOR UPDATE NOWAIT; ROLLBACK", postgres_schema::MIGRATION_KEY)).lines().any(|line| line == "f"));
    let log = std::fs::read_to_string(fixture.root.join("server.log")).unwrap();
    assert_eq!(log[offset..].lines().filter(|line| line.contains("execute ") && line.contains("SELECT version, checksum, store_epoch FROM td_mta.schema_migrations ORDER BY version")).count(), 1);
    renewer.renew(request, trust, deadline(), &leases).unwrap();
    closed(fixture, &pid);
    renewer.renew(request, trust, deadline(), &leases).unwrap();
    let pid = backend(fixture);
    sql(fixture, &format!("SELECT pg_terminate_backend({pid})"));
    closed(fixture, &pid);
    renewer.renew(request, trust, deadline(), &leases).unwrap();
    assert_ne!(backend(fixture), pid);
    let selection = sql(
        fixture,
        "SELECT encode(generation,'hex') FROM td_mta.config_selection",
    );
    let epoch = sql(
        fixture,
        "SELECT encode(store_epoch,'hex') FROM td_mta.schema_migrations LIMIT 1",
    );
    for (change, restore, error) in [
        (
            "UPDATE td_mta.runtime_barrier SET blocked=true".to_owned(),
            "UPDATE td_mta.runtime_barrier SET blocked=false".to_owned(),
            owners::Error::Connection(probe::Error::RuntimeMaintenance),
        ),
        (
            "UPDATE td_mta.config_selection SET generation=decode('0000000000000063','hex')"
                .to_owned(),
            format!("UPDATE td_mta.config_selection SET generation=decode('{selection}','hex')"),
            owners::Error::Policy,
        ),
        (
            "UPDATE td_mta.schema_migrations SET store_epoch=decode(repeat('ee',16),'hex')"
                .to_owned(),
            format!("UPDATE td_mta.schema_migrations SET store_epoch=decode('{epoch}','hex')"),
            owners::Error::Connection(probe::Error::RuntimeSchema),
        ),
    ] {
        renewer.renew(request, trust, deadline(), &leases).unwrap();
        let pid = backend(fixture);
        sql(fixture, &change);
        assert_eq!(
            renewer.renew(request, trust, deadline(), &leases),
            Err(error)
        );
        closed(fixture, &pid);
        sql(fixture, &restore);
    }
    renewer.renew(request, trust, deadline(), &leases).unwrap();
    let pid = backend(fixture);
    let baseline = sql(
        fixture,
        "SELECT string_agg(expires_at::text,',' ORDER BY account) FROM td_mta.route_owners",
    );
    assert_eq!(
        renewer.renew(request, trust, deadline(), &[(claim, generation + 1)]),
        Err(owners::Error::Stale)
    );
    closed(fixture, &pid);
    assert_eq!(
        sql(
            fixture,
            "SELECT string_agg(expires_at::text,',' ORDER BY account) FROM td_mta.route_owners"
        ),
        baseline
    );
    renewer.renew(request, trust, deadline(), &leases).unwrap();
    let pid = backend(fixture);
    sql(fixture, "CREATE FUNCTION public.delay_retained_renewal() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_sleep(2); RETURN NEW; END $$; CREATE CONSTRAINT TRIGGER delay_retained_renewal AFTER UPDATE ON td_mta.route_owners DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.delay_retained_renewal()");
    assert_eq!(
        renewer.renew(
            request,
            trust,
            std::time::Instant::now() + std::time::Duration::from_secs(1),
            &leases
        ),
        Err(owners::Error::Indeterminate)
    );
    std::thread::sleep(std::time::Duration::from_secs(3));
    closed(fixture, &pid);
    sql(
        fixture,
        "DROP TRIGGER delay_retained_renewal ON td_mta.route_owners",
    );
    drop(renewer);
    assert_eq!(
        sql(
            fixture,
            &format!(
                "SELECT pg_catalog.pg_try_advisory_xact_lock({})",
                postgres_schema::MIGRATION_KEY
            )
        ),
        "t"
    );
    owners::observe(request, trust, deadline(), claim, generation).unwrap();
}

fn idle_account_contention(
    fixture: &Fixture,
    request: &td_mta::postgres_probe::Request<'_>,
    trust: &TrustStore,
    claim: &td_mta::postgres_route_owners::Claim,
    generation: u64,
) {
    use std::{
        io::Write,
        process::Stdio,
        time::{Duration, Instant},
    };
    use td_mta::{postgres_queue_candidates as hints, postgres_read::ReadSlots};
    td_mta::postgres_route_owners::renew(request, trust, deadline(), claim, generation).unwrap();
    let slots = ReadSlots::new(1).unwrap();
    let mut reader = hints::IdleReader::new(&slots, request, trust);
    let expected = reader
        .observe(deadline(), claim, generation)
        .unwrap()
        .is_some();
    let pid = super::body_checks::backend(fixture);
    let mut blocker = super::ProbeChild(
        super::sql_command(fixture)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let mut input = blocker.0.stdin.take().unwrap();
    writeln!(
        input,
        "BEGIN; SELECT id FROM td_mta.accounts FOR UPDATE /* idle_poll_barrier */;"
    )
    .unwrap();
    let stop = deadline();
    while sql(fixture, "SELECT count(*) FROM pg_catalog.pg_stat_activity WHERE application_name='td-mta-schema-fixture' AND state='idle in transaction' AND query LIKE '%idle_poll_barrier%'") != "1" {
        assert!(Instant::now() < stop, "idle poll barrier was not acquired");
        std::thread::sleep(Duration::from_millis(10));
    }
    // The blocker remains held until the real operation returns. No timed
    // release or elapsed-time assertion decides whether contention occurred.
    let result = reader.observe(deadline(), claim, generation);
    writeln!(input, "ROLLBACK;\n\\q").unwrap();
    drop(input);
    super::wait_command(&mut blocker);
    assert_eq!(result, Err(hints::Error::Busy));
    super::body_checks::closed(fixture, &pid);
    assert_eq!(
        reader
            .observe(deadline(), claim, generation)
            .unwrap()
            .is_some(),
        expected
    );
    assert_ne!(super::body_checks::backend(fixture), pid);
    reader.close();
}
