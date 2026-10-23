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
