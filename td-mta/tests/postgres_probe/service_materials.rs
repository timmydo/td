use super::*;
use td_mta::{
    config::profile::Roles,
    ids::StoreEpoch,
    postgres_configuration::{self as configuration, Profile},
    postgres_configuration_store as versions,
    postgres_materials::{self as materials, Kind, Version},
    postgres_runtime_access::{self as access, ServiceRole},
    postgres_selection as selection,
};

pub(super) fn exercise(
    fixture: &Fixture,
    trust: &TrustStore,
    epoch: StoreEpoch,
    shared: &str,
    desktop: &str,
) {
    let request = fixture.request();
    let owner = fixture.root.join("config-owner");
    let ca = fixture.root.join("config-ca");
    fs::write(&ca, &fixture.ca).unwrap();
    fs::write(
        &owner,
        format!(
            "postgresql://td_probe:{}@localhost:{}/postgres?sslrootcert={}\n",
            std::str::from_utf8(PASSWORD).unwrap(),
            fixture.port,
            ca.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&owner, fs::Permissions::from_mode(0o600)).unwrap();
    let id = |n| Version::from_bytes([n; 16]);
    for n in [7, 8] {
        materials::insert(
            &request,
            trust,
            deadline(),
            epoch,
            id(n),
            Kind::RelayPassword,
            b"other-password",
        )
        .unwrap();
    }
    acme::publish_fixture_pair(fixture);
    let front = desktop.replace(&format!("mode = \"files\"\nchain_file = \"/postgres-material/{}\"\nkey_file = \"/postgres-material/{}\"",id(2),id(3)),"mode = \"acme\"")
        + "\n[acme]\ndirectory = \"https://acme.example.test/directory\"\ncontact = \"operator@example.test\"\nterms_accepted = true\n[listener \"http01\"]\nkind = \"http01\"\nbind = \"127.0.0.1:80\"\n";
    let receiver = desktop.split_once("[listener ").unwrap().0.to_string() + "[listener \"smtp\"]\nkind = \"direct_smtp\"\nbind = \"127.0.0.1:25\"\nserver_name = \"localhost\"\ncertificate = \"public\"\nsession_limit = 1\nper_peer_limit = 1\n";
    let relay = desktop
        .split_once("[certificate ")
        .unwrap()
        .0
        .replace("personal-smarthost", "relay-route");
    let migadu = relay
        .replace("relay-route", "migadu-route")
        .replace(&id(1).to_string(), &id(7).to_string());
    let select = |generation: u64, password: u8, version: u8, migadu_bits: Option<u8>| {
        let relay = relay.replace(&id(1).to_string(), &id(password).to_string());
        let mut entries = vec![
            ("frontend", Roles::JMAP, &front),
            ("receive", Roles::RECEIVE, &receiver),
            ("relay", Roles::RELAY, &relay),
        ];
        if let Some(bits) = migadu_bits {
            let source = if bits == Roles::JMAP { &front } else { &migadu };
            entries.push(("migadu", bits, source));
        }
        let profiles: Vec<_> = entries
            .into_iter()
            .map(|(name, bits, source)| Profile {
                name,
                roles: Roles::from_bits(bits).unwrap(),
                source: source.as_bytes(),
            })
            .collect();
        let candidate = configuration::prepare(
            &request,
            trust,
            deadline(),
            epoch,
            shared.as_bytes(),
            &profiles,
        )
        .unwrap();
        let version = versions::Version::from_bytes([version; 16]);
        versions::publish(&request, trust, deadline(), version, &candidate).unwrap();
        let change = selection::prepare(epoch, generation, version, &candidate).unwrap();
        selection::publish(&request, trust, deadline(), change)
    };
    let bind = |expected| {
        command_output(
            &[
                "postgres",
                "service-material-bind",
                "--connection-file",
                owner.to_str().unwrap(),
                "--epoch",
                &epoch.to_string(),
            ],
            fixture.root.path(),
            expected,
        )
    };
    // Personal databases can select arbitrary profile sets before opting in.
    select(0, 1, 20, None).unwrap();
    assert!(bind(1).contains("required-service-profiles"));
    assert_eq!(
        sql(fixture, "SELECT to_regnamespace('td_mta_service') IS NULL"),
        "t"
    );
    let complete = Some(Roles::RELAY);
    let original = select(1, 1, 21, complete).unwrap();
    for _ in 0..2 {
        assert!(bind(0).contains("\"generation\":2"));
    }
    let connection_for = |role: ServiceRole| fixture.root.join(role.login());
    for role in ServiceRole::ALL {
        let login = role.login();
        let mut grants = format!(
            "CREATE ROLE {login} LOGIN PASSWORD '{}'; GRANT USAGE ON SCHEMA td_mta TO {login};",
            std::str::from_utf8(PASSWORD).unwrap()
        );
        for (table, privileges) in role.grants() {
            grants.push_str(&format!("GRANT {privileges} ON td_mta.{table} TO {login};"));
        }
        if *role != ServiceRole::Acme {
            grants.push_str(&format!("GRANT USAGE ON SCHEMA td_mta_service TO {login}; GRANT SELECT ON td_mta_service.materials,td_mta_service.material_chunks,td_mta_service.active_certificates TO {login};"));
        }
        sql(fixture, &grants);
        let connection = connection_for(*role);
        fs::write(
            &connection,
            fs::read_to_string(&owner)
                .unwrap()
                .replace("td_probe:", &format!("{login}:")),
        )
        .unwrap();
        fs::set_permissions(&connection, fs::Permissions::from_mode(0o600)).unwrap();
    }
    let profile_check = |role, expected| {
        command_output(
            &[
                "postgres",
                "service-profile-check",
                "--connection-file",
                connection_for(role).to_str().unwrap(),
            ],
            fixture.root.path(),
            expected,
        )
    };
    // Qualifying ACLs cannot substitute for actually loading the selected account.
    assert!(profile_check(ServiceRole::Receive, 1).contains("profile-preparation-failed"));
    let account = td_mta::ids::AccountId::from_bytes([0xaa; 16]);
    let creation = td_mta::postgres_accounts::prepare(epoch, account).unwrap();
    td_mta::postgres_accounts::publish(&request, trust, deadline(), creation).unwrap();
    for role in ServiceRole::ALL {
        let mut runtime = fixture.request();
        runtime.user = role.login();
        runtime.mode = td_mta::postgres_probe::Mode::Runtime;
        access::inspect(&runtime, trust, deadline()).unwrap();
        let expected = match role {
            ServiceRole::Frontend => "5",
            ServiceRole::Receive => "2,3",
            ServiceRole::Relay => "1,4",
            ServiceRole::Migadu => "4,7",
            ServiceRole::Acme => {
                assert!(profile_check(*role, 1).contains("serving-login-required"));
                continue;
            }
        };
        let result = profile_check(*role, 0);
        assert!(result.contains("\"profile_checked\":true"));
        assert!(result.contains(&format!("\"store_epoch\":\"{epoch}\"")));
        let ids = sql(fixture,&format!("SET SESSION AUTHORIZATION {}; SELECT string_agg(get_byte(id,0)::text,',' ORDER BY id) FROM td_mta_service.materials",role.login()));
        assert_eq!(ids.lines().last().unwrap(), expected);
        let active = td_mta::postgres_acme::active(&runtime, trust, deadline(), epoch, "public");
        assert_eq!(active.is_ok(), *role == ServiceRole::Frontend);
        // No account private state column is present even on the authorized public-pair view.
        assert_eq!(sql(fixture,&format!("SET SESSION AUTHORIZATION {}; SELECT count(*) FROM information_schema.columns WHERE table_schema='td_mta_service' AND table_name='active_certificates' AND column_name='state'",role.login())).lines().last(),Some("0"));
    }
    let mut main_relay = fixture.request();
    main_relay.user = ServiceRole::Relay.login();
    main_relay.mode = td_mta::postgres_probe::Mode::Runtime;
    let can_read = |n| {
        materials::read(
            &main_relay,
            trust,
            deadline(),
            epoch,
            id(n),
            Kind::RelayPassword,
        )
        .is_ok()
    };
    assert!(can_read(1));
    assert!(!can_read(7));
    for (version, migadu_bits) in [(23, None), (24, Some(Roles::JMAP))] {
        assert_eq!(
            select(2, 8, version, migadu_bits),
            Err(selection::Error::ServiceProfiles)
        );
        assert_eq!(
            selection::observe(&request, trust, deadline(), epoch).unwrap(),
            Some(original)
        );
        assert!(can_read(1));
        assert!(!can_read(8));
    }
    select(2, 8, 22, complete).unwrap();
    assert!(!can_read(1));
    assert!(can_read(8));
    select(3, 1, 21, complete).unwrap();
    assert!(can_read(1));
    assert!(!can_read(8));
    // Surprise column grants on the binding tables disqualify even a current, correctly named login.
    sql(
        fixture,
        "GRANT SELECT(material_id) ON td_mta_service.material_bindings TO td_mta_relay",
    );
    main_relay.mode = td_mta::postgres_probe::Mode::Preparation;
    assert_eq!(
        access::inspect(&main_relay, trust, deadline()),
        Err(access::Error::Privileges)
    );
    sql(
        fixture,
        "REVOKE SELECT(material_id) ON td_mta_service.material_bindings FROM td_mta_relay",
    );
    access::inspect(&main_relay, trust, deadline()).unwrap();
    sql(fixture,"CREATE OR REPLACE VIEW td_mta_service.materials WITH (security_barrier=true) AS SELECT id,kind,length,checksum,chunk_count FROM td_mta.config_materials WHERE false");
    assert_eq!(
        access::inspect(&main_relay, trust, deadline()),
        Err(access::Error::Privileges)
    );
}
