#![allow(clippy::unwrap_used, clippy::indexing_slicing)]
use super::*;

/// The first unit `retired_references` finds.
fn retired_reference<'a>(
    show: &'a str,
    converted: &[&Path],
    retiring: &[String],
) -> R<Option<&'a str>> {
    Ok(retired_references(show, converted, retiring)?
        .first()
        .copied())
}

const PASS: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn socket_block() -> String {
    socket_settings(SOCKET_DIR, isolation::DB_GROUP, PG_FENCE).unwrap()
}

#[test]
fn configuration_keeps_other_settings_and_ends_with_the_fence_include() {
    let original = "port = 35432\n  Listen_Addresses = '127.0.0.1'\nssl=on\n\
                    ssl_cert_file = '/var/lib/td-mta/mail/tls/server.pem'\n\
                    ssl_key_file '/var/lib/td-mta/mail/tls/server.key'\n\
                    unix_socket_directories = ''\n# ssl = on\nwal_level = replica";
    let block = socket_block();
    let converted = postgresql_conf(original, &block).unwrap();
    assert_eq!(
        converted,
        format!("port = 35432\n# ssl = on\nwal_level = replica\n{block}")
    );
    assert!(block.ends_with("include_if_exists = '/etc/td-mta/postgres-fence.conf'\n"));
    for line in [
        "listen_addresses = ''",
        "ssl = off",
        "unix_socket_directories = '/var/lib/td-mta-postgres'",
        "unix_socket_group = 'td-mta-db'",
        "unix_socket_permissions = 0660",
    ] {
        assert!(block.lines().any(|l| l == line), "{line}");
    }
    for refused in [
        "include 'other.conf'\n",
        "INCLUDE_DIR = 'conf.d'\n",
        "include_if_exists = '/etc/x'\n",
        "ssl_ciphers = 'HIGH'\n",
        "ssl_min_protocol_version = 'TLSv1.3'\n",
    ] {
        assert!(postgresql_conf(refused, &block).is_err(), "{refused}");
    }
    assert!(postgresql_conf(&"#".repeat(64 * 1024 + 1), &block).is_err());
}

#[test]
fn loopback_scram_rules_move_to_the_socket_in_place() {
    let original = "# TYPE DATABASE USER ADDRESS METHOD\n\
                    hostssl td_mta td_mta_owner 127.0.0.1/32 scram-sha-256\n\
                    \n\
                    hostssl all td_mta_runtime 127.0.0.1/32 scram-sha-256 # shared\n\
                    hostssl replication td_mta_cutover_backup 127.0.0.1/32 scram-sha-256\n\
                    hostssl postgres td_mta_cutover_marker 127.0.0.1/32 scram-sha-256\n\
                    local td_mta td_mta_frontend scram-sha-256\n\
                    host all all 127.0.0.1/32 reject\n\
                    host all all ::1 ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff reject\n";
    let converted = pg_hba(original, false).unwrap();
    assert!(converted.dropped.is_empty());
    assert_eq!(
        converted.text,
        "# TYPE DATABASE USER ADDRESS METHOD\n\
         local td_mta td_mta_owner scram-sha-256\n\
         \n\
         local all td_mta_runtime scram-sha-256 # shared\n\
         local replication td_mta_cutover_backup scram-sha-256\n\
         local postgres td_mta_cutover_marker scram-sha-256\n\
         local td_mta td_mta_frontend scram-sha-256\n\
         host all all 127.0.0.1/32 reject\n\
         host all all ::1 ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff reject\n"
    );
    for refused in [
        "hostssl td_mta td_mta_owner 0.0.0.0/0 scram-sha-256\n",
        "hostssl td_mta td_mta_owner 127.0.0.1/32 md5\n",
        "host td_mta td_mta_owner 127.0.0.1/32 scram-sha-256\n",
        "hostssl other td_mta_owner 127.0.0.1/32 scram-sha-256\n",
        "hostssl td_mta,postgres td_mta_owner 127.0.0.1/32 scram-sha-256\n",
        "hostssl td_mta +admins 127.0.0.1/32 scram-sha-256\n",
        "hostssl td_mta \"Owner\" 127.0.0.1/32 scram-sha-256\n",
        "hostssl td_mta td_mta_owner 127.0.0.1/32 scram-sha-256 clientcert=verify-full\n",
        "hostnossl all all 0.0.0.0/0 reject\n",
        "hostgssenc all all 127.0.0.1/32 reject\n",
        "local all all trust\n",
        "local all all peer\n",
        "include other_hba.conf\n",
        "include_dir hba.d\n",
    ] {
        assert!(pg_hba(refused, false).is_err(), "{refused}");
        // Without a socket only the inert local rules are dropped instead.
        let local = refused.starts_with("local ");
        assert_eq!(pg_hba(refused, true).is_err(), !local, "{refused}");
    }
}

/// Production's file: a trust rule left from before the socket existed.
const PRODUCTION_HBA: &str = "\
hostssl td_mta td_mta_frontend 127.0.0.1/32 scram-sha-256
hostssl td_mta td_mta_receive 127.0.0.1/32 scram-sha-256
hostssl td_mta td_mta_relay 127.0.0.1/32 scram-sha-256
hostssl td_mta td_mta_migadu 127.0.0.1/32 scram-sha-256
hostssl td_mta td_mta_acme 127.0.0.1/32 scram-sha-256
local all setup_admin trust
hostssl all td_mta_owner 127.0.0.1/32 scram-sha-256
hostssl all td_mta_runtime 127.0.0.1/32 scram-sha-256
hostssl replication td_mta_cutover_backup 127.0.0.1/32 scram-sha-256
hostssl postgres td_mta_cutover_marker 127.0.0.1/32 scram-sha-256
host all all 0.0.0.0/0 reject
";

#[test]
fn an_inert_local_rule_is_dropped_only_without_a_unix_socket() {
    let conf = "listen_addresses = '127.0.0.1'\nssl = on\nunix_socket_directories = ''\n";
    assert!(no_unix_socket(conf));
    let converted = pg_hba(PRODUCTION_HBA, no_unix_socket(conf)).unwrap();
    assert_eq!(converted.dropped, ["local all setup_admin trust"]);
    assert_eq!(
        converted.text,
        "local td_mta td_mta_frontend scram-sha-256\n\
         local td_mta td_mta_receive scram-sha-256\n\
         local td_mta td_mta_relay scram-sha-256\n\
         local td_mta td_mta_migadu scram-sha-256\n\
         local td_mta td_mta_acme scram-sha-256\n\
         local all td_mta_owner scram-sha-256\n\
         local all td_mta_runtime scram-sha-256\n\
         local replication td_mta_cutover_backup scram-sha-256\n\
         local postgres td_mta_cutover_marker scram-sha-256\n\
         host all all 0.0.0.0/0 reject\n"
    );
    // Every loopback SCRAM rule has its socket rule, and nothing else admits.
    let mut moved = 0;
    for line in PRODUCTION_HBA.lines() {
        let words: Vec<&str> = line.split_ascii_whitespace().collect();
        if let ["hostssl", database, user, "127.0.0.1/32", "scram-sha-256"] = words[..] {
            let local = format!("local {database} {user} scram-sha-256");
            assert!(converted.text.lines().any(|l| l == local), "{local}");
            moved += 1;
        }
    }
    assert_eq!(moved, 9);
    assert!(converted
        .text
        .lines()
        .any(|l| l == "host all all 0.0.0.0/0 reject"));
    assert!(!converted.text.contains("trust"));
    assert!(!converted.text.contains("setup_admin"));

    // With a socket the rule admits someone today: refused, not dropped.
    let refusal = "local HBA rule without SCRAM while PostgreSQL has a Unix socket";
    for socket in [
        "listen_addresses = '127.0.0.1'\n",
        "unix_socket_directories = '/run/postgresql'\n",
        "unix_socket_directories = ''\nunix_socket_directories = '/tmp'\n",
        "unix_socket_directories = '''/tmp'\n",
        "unix_socket_directories = ''\ninclude_if_exists = 'other.conf'\n",
    ] {
        assert!(!no_unix_socket(socket), "{socket}");
        let error = pg_hba(PRODUCTION_HBA, no_unix_socket(socket))
            .err()
            .unwrap();
        assert_eq!(error.to_string(), refusal, "{socket}");
    }

    // The file, the server's reading of it with postgresql.auto.conf, and
    // the running postmaster must all say there is no socket.
    let postmaster = "4242\n/var/lib/td-mta/mail/postgres\n1700000000\n35432\n\n\
                      127.0.0.1\n  1234 5678\nready   \n";
    assert!(no_socket_today(conf, "", postmaster));
    let socket_pid = postmaster.replace("35432\n\n", "35432\n/run/postgresql\n");
    for (file, setting, pid) in [
        ("unix_socket_directories = '/tmp'\n", "", postmaster),
        (conf, "/tmp", postmaster),
        (conf, "", socket_pid.as_str()),
        (
            conf,
            "",
            "4242\n/var/lib/td-mta/mail/postgres\n1700000000\n35432\n",
        ),
    ] {
        assert!(
            !no_socket_today(file, setting, pid),
            "{file} {setting} {pid}"
        );
    }
    for none in [
        "unix_socket_directories=''",
        "  UNIX_SOCKET_DIRECTORIES ''  # none\r\n",
        "unix_socket_directories = '/tmp'\nunix_socket_directories = ''\n",
        "unix_socket_directories = ''\n# unix_socket_directories = '/tmp'\n",
    ] {
        assert!(no_unix_socket(none), "{none}");
    }

    // A dropped rule goes with its options and comment; other shapes refuse.
    let peer = pg_hba("local all setup_admin peer map=trial # trial\n", true).unwrap();
    assert_eq!(peer.dropped, ["local all setup_admin peer map=trial"]);
    assert_eq!(peer.text, "");
    for refused in [
        "local all setup_admin custom\n",
        "local all setup_admin peer trial\n",
        "local all +admins trust\n",
        "local all setup_admin,other trust\n",
        "local td_mta td_mta_owner scram-sha-256 clientcert=verify-full\n",
        "local all setup_admin\n",
        "local all setup_admin peer map=\u{1b}[2J\n",
        "# continued \\\nlocal all setup_admin trust\n",
    ] {
        assert!(pg_hba(refused, true).is_err(), "{refused}");
    }
}

#[test]
fn connections_keep_their_credentials_and_change_only_the_transport() {
    let legacy = format!(
        "postgresql://td_mta_runtime:{PASS}@localhost:35432/td_mta?sslmode=verify-full&sslrootcert={MAIL_CA}\n"
    );
    let socket = format!(
        "postgresql://td_mta_runtime:{PASS}@:35432/td_mta?host=%2Fvar%2Flib%2Ftd-mta-postgres\n"
    );
    assert_eq!(
        connection(&legacy, "td_mta_runtime", MAIL_CA).unwrap(),
        socket
    );
    let numeric = legacy.replace("localhost", "127.0.0.1");
    let bare = numeric.trim_end();
    assert_eq!(
        connection(bare, "td_mta_runtime", MAIL_CA).unwrap(),
        socket.trim_end()
    );
    assert_eq!(
        isolation::socket_password(socket.trim_end(), "td_mta_runtime").unwrap(),
        PASS
    );
    assert_eq!(
        connection(
            &legacy.replace(PASS, "Not-Hex_9"),
            "td_mta_runtime",
            MAIL_CA
        )
        .unwrap(),
        socket.replace(PASS, "Not-Hex_9")
    );
    for refused in [
        legacy.replace("td_mta_runtime", "td_mta_owner"),
        legacy.replace(":35432", ":5432"),
        legacy.replace("localhost", "db.example"),
        legacy.replace("verify-full", "require"),
        legacy.replace(MAIL_CA, "/tmp/ca.pem"),
        legacy.replace(PASS, "se%2Fcret"),
        legacy.replace("/td_mta?", "/postgres?"),
        format!("{legacy}&application_name=x"),
        socket.clone(),
    ] {
        assert!(
            connection(&refused, "td_mta_runtime", MAIL_CA).is_err(),
            "{refused}"
        );
    }
}

#[test]
fn service_files_name_the_socket_directory_and_their_passfiles() {
    let original = "# operator profiles\n[cutover_repl]\nhost=localhost\nhostaddr=127.0.0.1\n\
                    port=35432\nuser=td_mta_cutover_backup\npassfile=/var/lib/td-mta-operator/physical-backup/repl.pass\n\
                    sslmode=verify-full\nsslrootcert=/var/lib/td-mta/mail/tls/ca.pem\nsslcertmode=disable\n\n\
                    [cutover_marker]\n  host=localhost\nport=35432\ndbname=postgres\n\
                    passfile=/var/lib/td-mta-operator/physical-backup/marker.pass\nconnect_timeout=3\n";
    let mut passfiles = Vec::new();
    assert_eq!(
        service_file(original, &mut passfiles).unwrap(),
        "# operator profiles\n[cutover_repl]\nhost=/var/lib/td-mta-postgres\nport=35432\n\
         user=td_mta_cutover_backup\npassfile=/var/lib/td-mta-operator/physical-backup/repl.pass\n\n\
         [cutover_marker]\nhost=/var/lib/td-mta-postgres\nport=35432\ndbname=postgres\n\
         passfile=/var/lib/td-mta-operator/physical-backup/marker.pass\nconnect_timeout=3\n"
    );
    assert_eq!(
        passfiles,
        [
            PathBuf::from("/var/lib/td-mta-operator/physical-backup/repl.pass"),
            PathBuf::from("/var/lib/td-mta-operator/physical-backup/marker.pass"),
        ]
    );
    for refused in [
        "[a]\nhost=localhost\nport=5432\n",
        "[a]\nhost=localhost\n",
        "[a]\nport=35432\n",
        "host=localhost\nport=35432\n",
        "[a]\nhost=localhost\nport=35432\noptions=-c x=y\n",
        "[a]\nhost = localhost\nport=35432\n",
        "[a]\nhost=localhost\nport=35432\npassfile=relative\n",
        "[a\nhost=localhost\nport=35432\n",
    ] {
        assert!(service_file(refused, &mut Vec::new()).is_err(), "{refused}");
    }
}

#[test]
fn passfile_entries_match_the_socket_directory() {
    assert_eq!(
        passfile("# backup\nlocalhost:35432:replication:td_mta_cutover_backup:a\\:b\n127.0.0.1:35432:*:u:p").unwrap(),
        "# backup\n/var/lib/td-mta-postgres:35432:replication:td_mta_cutover_backup:a\\:b\n\
         /var/lib/td-mta-postgres:35432:*:u:p"
    );
    for refused in [
        "*:35432:db:u:p\n",
        "localhost:5432:db:u:p\n",
        "localhost:35432:db:u\n",
        "localhost:35432:d\\:b:u:p\n",
        "db.example:35432:db:u:p\n",
    ] {
        assert!(passfile(refused).is_err(), "{refused}");
    }
}

#[test]
fn only_listening_rows_count_as_tcp_listeners() {
    let header = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n";
    assert!(!tcp_listening(header).unwrap());
    let established = format!("{header}   0: 0100007F:8A78 0100007F:D2E4 01 00000000:00000000 00:00000000 00000000   105        0 1 1\n");
    assert!(!tcp_listening(&established).unwrap());
    let listen = format!("{established}   1: 0100007F:8A78 00000000:0000 0A 00000000:00000000 00:00000000 00000000   105        0 2 1\n");
    assert!(tcp_listening(&listen).unwrap());
    assert!(tcp_listening("garbage\n").is_err());
    assert!(tcp_listening(&format!("{header}short row\n")).is_err());
}

#[test]
fn the_cutover_replaces_only_the_daemon_identity_settings() {
    let old = files::production_tls().unwrap();
    let new = files::production().unwrap();
    assert_eq!(old.len(), new.len());
    let mut changed = 0;
    for (old, new) in old.iter().zip(&new) {
        assert_eq!(old.path, new.path);
        if old.text != new.text {
            changed += 1;
            assert_eq!(
                old.text
                    .replace("SupplementaryGroups=\n", "SupplementaryGroups=td-mta-db\n"),
                new.text
            );
            assert!(!new.path.to_str().unwrap().contains("td-mta-maint-mail-"));
        }
    }
    assert_eq!(changed, service_database::ROLES.len());
    let (old, new) = (
        crate::readiness::aggregate(true).unwrap(),
        crate::readiness::aggregate(false).unwrap(),
    );
    assert_eq!(old.path, new.path);
    let removed: Vec<_> = old
        .text
        .lines()
        .filter(|line| !new.text.lines().any(|kept| kept == *line))
        .collect();
    assert_eq!(removed.len(), 2);
    assert!(removed
        .iter()
        .all(|line| line
            .starts_with("ExecStart=/usr/bin/openssl x509 -in /var/lib/td-mta/mail/tls/")));
}

#[test]
fn only_generated_renewal_units_are_td_mta_files() {
    let unit = "td-mta-maint-postgres-tls-renew.timer";
    for path in [
        "/opt/td-mta/share/td-mta/services/td-mta-maint-postgres-tls-renew.timer",
        "/var/lib/td-mta-operator/prefix/share/td-mta/services/td-mta-maint-postgres-tls-renew.timer",
    ] {
        assert!(generated_unit(Path::new(path), unit), "{path}");
    }
    for path in [
        "/etc/systemd/system/td-mta-maint-postgres-tls-renew.timer",
        "/opt/td-mta/share/td-mta/services/td-mta-maint-postgres-tls-renew.service",
        "/opt/td-mta/share/td-mta/other/td-mta-maint-postgres-tls-renew.timer",
        "/opt/td-mta/xshare/td-mta/services/td-mta-maint-postgres-tls-renew.timer",
        "share/td-mta/services/td-mta-maint-postgres-tls-renew.timer",
    ] {
        assert!(!generated_unit(Path::new(path), unit), "{path}");
    }
}

#[test]
fn units_naming_the_retired_transport_are_found_before_the_cutover() {
    let owner = Path::new("/var/lib/td-mta/mail/owner-connection");
    let runtime = Path::new("/var/lib/td-mta/mail/runtime-connection");
    let converted = [owner, runtime];
    let unit = |id: &str, argv: &str| {
        format!(
            "ExecStart={{ path=/opt/td-mta/bin/td-mta ; argv[]={argv} ; ignore_errors=no }}\nId={id}"
        )
    };
    let clean = [
        unit(
            "td-mta-frontend.service",
            "/opt/td-mta/bin/td-mta serve --connection-file /var/lib/td-mta/mail/runtime-connection",
        ),
        unit(
            "td-mta-maint-notifications-monitor.service",
            "/opt/td-mta/bin/td-mta-operator x --notify-connection-file=/var/lib/td-mta/mail/owner-connection",
        ),
        // The cutover replaces the aggregate and retires the renewal units.
        unit(
            "td-mta-health.service",
            "/usr/bin/openssl x509 -in /var/lib/td-mta/mail/tls/server.pem",
        ),
        unit(
            "td-mta-maint-postgres-tls-renew.service",
            "/opt/td-mta/bin/td-mta-operator --postgres-tls-renew --tls-dir /var/lib/td-mta/mail/tls",
        ),
        "Id=td-mta-maint-backup.timer".to_string(),
    ]
    .join("\n\n");
    assert_eq!(retired_reference(&clean, &converted, &[]).unwrap(), None);
    assert_eq!(retired_reference("", &converted, &[]).unwrap(), None);
    for (argv, why) in [
        (
            "/opt/td-mta/bin/td-mta-operator x --tls-certificate /var/lib/td-mta/mail/tls/ca.pem",
            "certificate check",
        ),
        (
            "/bin/cat /var/lib/td-mta-services/relay/tls",
            "service TLS directory",
        ),
        (
            "/opt/td-mta/bin/td-mta x --connection-file /home/alice/connection",
            "unconverted connection file",
        ),
        (
            "/opt/td-mta/bin/td-mta x --source-connection-file=/etc/other",
            "unconverted connection option",
        ),
    ] {
        let show = format!("{clean}\n\n{}", unit("td-mta-maint-x.service", argv));
        assert_eq!(
            retired_reference(&show, &converted, &[]).unwrap(),
            Some("td-mta-maint-x.service"),
            "{why}"
        );
    }
    let dangling = unit("td-mta-maint-x.service", "/bin/td-mta x --connection-file");
    assert!(retired_reference(&dangling, &converted, &[]).is_err());
    assert!(retired_reference("ExecStart=x", &converted, &[]).is_err());
}

// `systemctl show --all -p Id,ExecStartPre,ExecStart,ExecStartPost
// 'td-mta-*'` as production printed it on the cutover's second refusal,
// with the ACME contact and names replaced.
const PRODUCTION_SHOW: &str = include_str!("production-systemctl-show.txt");

fn connections() -> Vec<PathBuf> {
    let mut paths = vec![
        PathBuf::from(OWNER_CONNECTION),
        PathBuf::from(RUNTIME_CONNECTION),
    ];
    for &role in ROLES {
        paths.push(PathBuf::from(isolation::connection(role).unwrap()));
    }
    paths
}

fn check_unit(id: &str, lines: &[String]) -> String {
    let mut block = lines.join("\n");
    block.push_str(&format!("\nId={id}"));
    block
}

fn exec(argv: &str) -> String {
    let path = argv.split(' ').next().unwrap();
    format!("ExecStart={{ path={path} ; argv[]={argv} ; ignore_errors=no ; start_time=[n/a] ; stop_time=[n/a] ; pid=0 ; code=(null) ; status=0/0 }}")
}

fn openssl_check(file: &str) -> Vec<String> {
    [
        "/usr/bin/openssl",
        "x509",
        "-in",
        file,
        "-noout",
        "-checkend",
        "2592000",
    ]
    .map(String::from)
    .to_vec()
}

#[test]
fn production_units_pass_the_guard_once_their_tls_checks_retire() {
    let paths = connections();
    let converted: Vec<&Path> = paths.iter().map(PathBuf::as_path).collect();
    // What refused the cutover on production, and all that would.
    assert_eq!(
        retired_reference(PRODUCTION_SHOW, &converted, &[]).unwrap(),
        Some("td-mta-maint-tls-1.service")
    );
    assert_eq!(
        retired_references(PRODUCTION_SHOW, &converted, &[]).unwrap(),
        ["td-mta-maint-tls-1.service", "td-mta-maint-tls-0.service"]
    );
    for isolated in [true, false] {
        let found = tls_checks(PRODUCTION_SHOW, &checks::material_dirs(isolated).unwrap()).unwrap();
        assert_eq!(
            found,
            [
                TlsCheck {
                    name: "td-mta-maint-tls-1".into(),
                    argv: openssl_check("/var/lib/td-mta/mail/tls/ca.pem"),
                },
                TlsCheck {
                    name: "td-mta-maint-tls-0".into(),
                    argv: openssl_check("/var/lib/td-mta/mail/tls/server.pem"),
                },
            ]
        );
        let retiring: Vec<String> = found.iter().map(|c| c.unit("service").unwrap()).collect();
        assert_eq!(
            retired_reference(PRODUCTION_SHOW, &converted, &retiring).unwrap(),
            None
        );
    }
}

#[test]
fn a_tls_naming_unit_in_any_other_form_still_refuses() {
    let paths = connections();
    let converted: Vec<&Path> = paths.iter().map(PathBuf::as_path).collect();
    let dirs = checks::material_dirs(true).unwrap();
    let check =
        "/usr/bin/openssl x509 -in /var/lib/td-mta/mail/tls/ca.pem -noout -checkend 2592000";
    let pre = String::from("ExecStartPre={ path=/bin/true ; argv[]=/bin/true ; ignore_errors=no ; start_time=[n/a] ; stop_time=[n/a] ; pid=0 ; code=(null) ; status=0/0 }");
    let ignored = exec(check).replace("ignore_errors=no", "ignore_errors=yes");
    for (id, lines, why) in [
        (
            "td-mta-maint-tls-5.service",
            vec![pre, exec(check)],
            "a command before it",
        ),
        (
            "td-mta-maint-tls-5.service",
            vec![exec(check), exec(check)],
            "a second command",
        ),
        (
            "td-mta-maint-tls-5.service",
            vec![ignored],
            "its failure ignored",
        ),
        (
            "td-mta-maint-tls-00.service",
            vec![exec(check)],
            "an unnumbered name",
        ),
        (
            "td-mta-maint-tls-x.service",
            vec![exec(check)],
            "an unnumbered name",
        ),
        (
            "td-mta-maint-cert.service",
            vec![exec(check)],
            "another job's name",
        ),
        (
            "td-mta-maint-tls-5.service",
            vec![exec(&check.replace("2592000", "86400"))],
            "another threshold",
        ),
        (
            "td-mta-maint-tls-5.service",
            vec![exec(&check.replace("tls/ca.pem", "tls/old/ca.pem"))],
            "a file below the material",
        ),
        (
            "td-mta-maint-tls-5.service",
            vec![exec(
                &check.replace("/usr/bin/openssl", "/usr/bin/open%ssl"),
            )],
            "a program the installer would quote",
        ),
        (
            "td-mta-maint-tls-5.service",
            vec![exec(
                "/usr/bin/openssl x509 -in /var/lib/td-mta/mail/tls -noout -checkend 2592000",
            )],
            "the directory itself",
        ),
    ] {
        let show = format!("{PRODUCTION_SHOW}\n{}", check_unit(id, &lines));
        let found = tls_checks(&show, &dirs).unwrap();
        assert_eq!(found.len(), 2, "{why}");
        let retiring: Vec<String> = found.iter().map(|c| c.unit("service").unwrap()).collect();
        assert_eq!(
            retired_reference(&show, &converted, &retiring).unwrap(),
            Some(id),
            "{why}"
        );
    }
    // A shared layout has no service TLS directories to retire.
    let relay = "/usr/bin/openssl x509 -in /var/lib/td-mta-services/relay/tls/ca.pem -noout -checkend 2592000";
    let show = check_unit("td-mta-maint-tls-2.service", &[exec(relay)]);
    assert_eq!(tls_checks(&show, &dirs).unwrap().len(), 1);
    assert!(tls_checks(&show, &checks::material_dirs(false).unwrap())
        .unwrap()
        .is_empty());
    // A check of a certificate the cutover leaves is no check it retires,
    // and the guard leaves it too.
    let kept = "/usr/bin/openssl x509 -in /etc/ssl/td-mta/listener.pem -noout -checkend 2592000";
    let show = check_unit("td-mta-maint-tls-3.service", &[exec(kept)]);
    assert!(tls_checks(&show, &dirs).unwrap().is_empty());
    assert_eq!(retired_reference(&show, &converted, &[]).unwrap(), None);
    assert!(tls_checks("ExecStart=x", &dirs).is_err());
}

#[test]
fn retired_checks_match_the_files_the_operations_installer_wrote() {
    let check = TlsCheck {
        name: "td-mta-maint-tls-0".into(),
        argv: openssl_check("/var/lib/td-mta/mail/tls/server.pem"),
    };
    // builder/src/install_mta_operations.rs pins the same text.
    assert_eq!(
        service_text(&check).unwrap(),
        "[Unit]\nDescription=td-mta-maint-tls-0\n\n[Service]\nType=oneshot\nExecStart=:\"/usr/bin/openssl\" \"x509\" \"-in\" \"/var/lib/td-mta/mail/tls/server.pem\" \"-noout\" \"-checkend\" \"2592000\"\nUMask=0077\nTimeoutStartSec=120\nNoNewPrivileges=yes\nPrivateTmp=yes\n"
    );
    assert_eq!(
        timer_text(&check).unwrap(),
        "[Unit]\nDescription=Schedule td-mta-maint-tls-0\n\n[Timer]\nOnBootSec=2min\nOnUnitActiveSec=15min\nUnit=td-mta-maint-tls-0.service\n\n[Install]\nWantedBy=timers.target\n"
    );
    let (service, command) = checks::schedule_parts(&check).unwrap();
    assert_eq!(service, "(service '(td-mta-maint-tls-0)\n");
    assert_eq!(
        command,
        "(command '(\"/usr/bin/openssl\" \"x509\" \"-in\" \"/var/lib/td-mta/mail/tls/server.pem\" \"-noout\" \"-checkend\" \"2592000\")\n"
    );
}

#[test]
fn the_health_rewrite_keeps_every_other_line_exactly() {
    let dirs = checks::material_dirs(false).unwrap();
    // The TLS layout's readiness drop-in lists them as the health unit did;
    // the installer orders the options the other way and quotes each word.
    let tls_dropin = include_str!("../units/zz-readiness-jobs.tls.conf");
    let lines: Vec<&str> = tls_dropin
        .lines()
        .filter(|l| checks::expiry_line(l, &dirs).unwrap())
        .collect();
    assert_eq!(
        lines,
        [
            "ExecStart=/usr/bin/openssl x509 -in /var/lib/td-mta/mail/tls/server.pem -checkend 2592000 -noout",
            "ExecStart=/usr/bin/openssl x509 -in /var/lib/td-mta/mail/tls/ca.pem -checkend 2592000 -noout",
        ]
    );
    let socket_dropin = include_str!("../units/zz-readiness-jobs.conf");
    assert!(!socket_dropin
        .lines()
        .any(|l| checks::expiry_line(l, &dirs).unwrap()));
    let installer = "ExecStart=:\"/usr/bin/openssl\" \"x509\" \"-in\" \"/var/lib/td-mta/mail/tls/ca.pem\" \"-noout\" \"-checkend\" \"2592000\"";
    // Production's maintenance services carry the hardening block.
    let head = format!(
        "[Unit]\r\nDescription=td-mta aggregate health\n\n[Service]\n{}Type=oneshot\n\
        ExecStart=/usr/bin/systemctl --quiet is-active td-mta-postgres.service\n",
        checks::HARDENING
    );
    let middle = "# keep  this   comment\n";
    let tail = "ExecStart=/opt/td-mta/bin/td-mta acme-status --connection-file /var/lib/td-mta/mail/runtime-connection  \n\
        ExecStart=/usr/bin/systemctl --quiet is-active td-mta-migadu.service";
    let unit = format!(
        "{head}{}\n{middle}{}\n{installer}\n{tail}",
        lines[0], lines[1]
    );
    let kept = format!("{head}{middle}{tail}");
    assert_eq!(health_unit(&unit, &dirs).unwrap(), Some(kept.clone()));
    assert_eq!(health_unit(&kept, &dirs).unwrap(), None);
    // The last line, without its newline, goes as well.
    let last = format!("{kept}\n{}", lines[1]);
    assert_eq!(
        health_unit(&last, &dirs).unwrap(),
        Some(format!("{kept}\n"))
    );
    // A check continuing the line before it, or continued, is no check.
    for continued in [
        format!("Environment=A=b \\\n{}", lines[0]),
        format!("Environment=A=b \\\r\n{}", lines[0]),
        format!("Environment=A=b \\\n# note\n{}", lines[0]),
        format!("{} \\\n -x", lines[0]),
    ] {
        assert!(
            health_unit(&format!("{kept}\n{continued}\n"), &dirs).is_err(),
            "{continued}"
        );
    }
    for other in [
        "ExecStart=/usr/bin/openssl x509 -in /var/lib/td-mta/mail/tls/server.pem -checkend 2592000 -noout ",
        "ExecStart=/usr/bin/openssl  x509 -in /var/lib/td-mta/mail/tls/server.pem -checkend 2592000 -noout",
        "ExecStart=-/usr/bin/openssl x509 -in /var/lib/td-mta/mail/tls/ca.pem -checkend 2592000 -noout",
        "ExecStart=/usr/bin/openssl x509 -in /var/lib/td-mta/mail/tls/ca.pem -checkend 86400 -noout",
        "ExecStart=/usr/bin/openssl x509 -in /var/lib/td-mta/mail/tls/ca.pem -noout",
        "ExecStart=/usr/bin/openssl x509 -in /var/lib/td-mta/mail/tls/sub/ca.pem -checkend 2592000 -noout",
        "ExecStart=/usr/bin/openssl x509 -in \"/var/lib/td-mta/mail/tls/ca.pem -checkend 2592000 -noout",
        "ExecStart=/usr/bin/openssl x509 -in '/var/lib/td-mta/mail/tls/ca.pem' -checkend 2592000 -noout",
        "ExecStartPre=/usr/bin/test -e \"/var/lib/td-mta/mail/tls/ca.pem\"",
        "Environment=CA=/var/lib/td-mta/mail/tls/ca.pem",
        "# /var/lib/td-mta/mail/tls",
    ] {
        assert!(
            health_unit(&format!("{unit}\n{other}\n"), &dirs).is_err(),
            "{other}"
        );
    }
}

fn notification(checks: &[&str]) -> Json {
    td_json::parse(&format!(
        r#"{{"checks":[{}],"connection":"/var/lib/td-mta/mail/owner-connection","epoch":"0123456789abcdef0123456789abcdef","mta":"/opt/td-mta/bin/td-mta","recipient":"ops@example.org","smtp":"127.0.0.1:25","state":"/var/lib/td-mta/mail/.operations/notifications"}}"#,
        checks.join(",")
    ))
    .unwrap()
}

#[test]
fn notification_checks_of_the_material_go_and_the_rest_stay_as_written() {
    let mail = r#"{"argv":["/opt/td-mta/bin/td-mta","status","--json","--require-ready","--runtime","/var/lib/td-mta/mail/frontend/runtime"],"kind":"status","name":"mail-frontend"}"#;
    let tls = |n: u8, file: &str| {
        format!(
            r#"{{"argv":["/usr/bin/openssl","x509","-in","/var/lib/td-mta/mail/tls/{file}","-noout","-checkend","2592000"],"kind":"status","name":"tls-{n}"}}"#
        )
    };
    let (zero, one) = (tls(0, "server.pem"), tls(1, "ca.pem"));
    let dirs = checks::material_dirs(true).unwrap();
    let canonical = |json: &Json| -> R<String> { Ok(json.to_canonical()) };
    let printed = |json: &Json| -> R<String> { Ok(format!("{json}\n")) };
    let before = notification(&[mail, &zero, &one]);
    let after = notification(&[mail]);
    let text = before.to_canonical();
    let (new, removed) = notification_config(&text, &dirs, true, canonical)
        .unwrap()
        .unwrap();
    assert_eq!(new, after.to_canonical());
    assert_eq!(removed, ["tls-0", "tls-1"]);
    let text = format!("{before}\n");
    let (new, _) = notification_config(&text, &dirs, true, printed)
        .unwrap()
        .unwrap();
    assert_eq!(new, format!("{after}\n"));
    // Nothing to drop leaves the file alone, whatever its form.
    let plain = after.to_string_pretty();
    for exact in [true, false] {
        assert_eq!(
            notification_config(&plain, &dirs, exact, canonical).unwrap(),
            None
        );
    }
    // A derived file in a form its writer would not leave is not rewritten;
    // the operator's file, which may have been edited, takes the canonical
    // form with its content.
    let pretty = before.to_string_pretty();
    assert!(notification_config(&pretty, &dirs, true, canonical).is_err());
    let (new, _) = notification_config(&pretty, &dirs, false, canonical)
        .unwrap()
        .unwrap();
    assert_eq!(new, after.to_canonical());
    // A check naming the material in any other form refuses.
    for other in [
        zero.replace(r#""kind":"status""#, r#""kind":"performance""#),
        zero.replace(r#""name":"tls-0""#, r#""name":"tls-x""#),
        zero.replace(r#""name":"tls-0""#, r#""name":"cert-0""#),
        zero.replace("2592000", "86400"),
        zero.replace(r#""kind":"status""#, r#""kind":"status","retain":1"#),
        mail.replace(
            "/var/lib/td-mta/mail/frontend/runtime",
            "/var/lib/td-mta/mail/tls",
        ),
    ] {
        let text = notification(&[mail, &other]).to_canonical();
        for exact in [true, false] {
            assert!(
                notification_config(&text, &dirs, exact, canonical).is_err(),
                "{other}"
            );
        }
    }
}

#[test]
fn a_retired_checks_drop_in_directory_goes_once_only_the_fence_is_in_it() {
    let scratch = td_fs::TempDir::new("mta-retired-dropins").unwrap();
    let condition = fence_condition().unwrap();
    let directory = scratch.path().join("td-mta-maint-tls-0.service.d");
    assert_eq!(retire_dropins(&directory, &condition).unwrap(), None);
    assert!(fence_only(&directory, &condition).unwrap());
    // An empty directory, as production's fence left beside its checks,
    // is clean and goes.
    fs::create_dir(&directory).unwrap();
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(fence_only(&directory, &condition).unwrap());
    assert_eq!(retire_dropins(&directory, &condition).unwrap(), Some(true));
    assert!(!directory.exists());
    fs::create_dir(&directory).unwrap();
    fs::write(directory.join(FENCE_DROPIN), &condition).unwrap();
    assert!(fence_only(&directory, &condition).unwrap());
    assert_eq!(retire_dropins(&directory, &condition).unwrap(), Some(true));
    assert!(!directory.exists());
    // Anything else stays, and so does its directory.
    fs::create_dir(&directory).unwrap();
    fs::write(directory.join(FENCE_DROPIN), &condition).unwrap();
    fs::write(directory.join("10-local.conf"), "[Service]\n").unwrap();
    assert!(!fence_only(&directory, &condition).unwrap());
    assert_eq!(retire_dropins(&directory, &condition).unwrap(), Some(false));
    assert!(!directory.join(FENCE_DROPIN).exists());
    assert!(directory.join("10-local.conf").exists());
    fs::remove_file(directory.join("10-local.conf")).unwrap();
    fs::write(directory.join(FENCE_DROPIN), "[Unit]\n").unwrap();
    assert!(!fence_only(&directory, &condition).unwrap());
    assert_eq!(retire_dropins(&directory, &condition).unwrap(), Some(false));
    assert!(directory.join(FENCE_DROPIN).exists());
}

#[test]
fn a_check_unit_is_judged_by_what_systemd_says_of_it_and_its_file() {
    let unit = "td-mta-maint-tls-0.service";
    let fence =
        Path::new("/etc/systemd/system/td-mta-maint-tls-0.service.d/98-td-mta-deploy-fence.conf");
    let link = "/etc/systemd/system/td-mta-maint-tls-0.service";
    let props = |load: &str, fragment: &str, dropins: &str| {
        format!("LoadState={load}\nFragmentPath={fragment}\nDropInPaths={dropins}\n")
    };
    let state = |props: &str, timer: bool, clean: bool| {
        check_unit_state(unit, timer, props, fence, clean).map(|f| f.map(String::from))
    };
    assert_eq!(
        state(&props("loaded", link, ""), false, true).unwrap(),
        Some(link.to_string())
    );
    // The fence's own condition, left by a job that ended before its
    // cleanup, is the one drop-in a check may have.
    let fenced = props("loaded", link, fence.to_str().unwrap());
    assert!(state(&fenced, false, true).unwrap().is_some());
    assert!(state(&fenced, false, false).is_err());
    assert!(state(&props("loaded", link, ""), false, false).is_err());
    let other = format!(
        "{} /etc/systemd/system/td-mta-maint-.service.d/10-x.conf",
        fence.display()
    );
    assert!(state(&props("loaded", link, &other), false, true).is_err());
    // A timer systemd does not find leaves the service to retire alone.
    assert_eq!(
        state(&props("not-found", "", ""), true, true).unwrap(),
        None
    );
    for (load, fragment, timer) in [
        ("not-found", "", false),
        ("masked", "/dev/null", false),
        ("masked", "/dev/null", true),
        ("loaded", "", false),
        ("error", link, true),
    ] {
        assert!(
            state(&props(load, fragment, ""), timer, true).is_err(),
            "{load} {timer}"
        );
    }
    assert!(state("LoadState=loaded\nFragmentPath=/x\n", false, true).is_err());

    let check = TlsCheck {
        name: "td-mta-maint-tls-0".into(),
        argv: openssl_check("/var/lib/td-mta/mail/tls/server.pem"),
    };
    let text = service_text(&check).unwrap();
    let forms = [text.clone(), hardened_service_text(&check).unwrap()];
    let generated = Path::new("/opt/td-mta/share/td-mta/services/td-mta-maint-tls-0.service");
    generated_check(unit, generated, false, text.as_bytes(), &forms).unwrap();
    assert!(generated_check(unit, generated, false, b"[Unit]\n", &forms).is_err());
    // The hardened form is only production's copy, never a generated file.
    assert!(generated_check(unit, generated, false, forms[1].as_bytes(), &forms).is_err());
    for elsewhere in [
        "/etc/systemd/system/td-mta-maint-tls-0.service",
        "/opt/td-mta/share/td-mta/services/td-mta-maint-tls-1.service",
    ] {
        assert!(
            generated_check(unit, Path::new(elsewhere), false, text.as_bytes(), &forms).is_err()
        );
    }
    let scm = "(use-modules (shepherd service))\n(register-services\n (service '(td-mta-maint-tls-0)\n  #:start (make-timer-constructor\n    (command '(\"/usr/bin/openssl\" \"x509\" \"-in\" \"/var/lib/td-mta/mail/tls/server.pem\" \"-noout\" \"-checkend\" \"2592000\")\n      #:directory \"/state\"))))\n";
    schedule_check(&check, scm.as_bytes()).unwrap();
    assert!(schedule_check(&check, scm.replace("2592000", "1").as_bytes()).is_err());
    assert!(schedule_check(&check, scm.replace("tls-0)", "tls-1)").as_bytes()).is_err());
}

#[test]
fn installer_numbers_name_at_most_thirty_two_checks() {
    let dirs = checks::material_dirs(false).unwrap();
    let check =
        "/usr/bin/openssl x509 -in /var/lib/td-mta/mail/tls/ca.pem -noout -checkend 2592000";
    for (n, found) in [
        ("0", true),
        ("9", true),
        ("31", true),
        ("32", false),
        ("99", false),
        ("01", false),
        ("", false),
        ("+1", false),
    ] {
        let show = check_unit(&format!("td-mta-maint-tls-{n}.service"), &[exec(check)]);
        assert_eq!(tls_checks(&show, &dirs).unwrap().len() == 1, found, "{n}");
    }
}

struct Recorded(Vec<&'static str>, Option<&'static str>);

impl Recorded {
    fn step(&mut self, step: &'static str) -> R<()> {
        self.0.push(step);
        match self.1 {
            Some(failing) if failing == step => Err("refused".into()),
            _ => Ok(()),
        }
    }
}

impl CutoverSteps for Recorded {
    fn prepare(&mut self) -> R<()> {
        self.step("prepare")
    }
    fn inspect(&mut self) -> R<()> {
        self.step("inspect")
    }
    fn preflight(&mut self) -> R<()> {
        self.step("preflight")
    }
    fn stage(&mut self) -> R<()> {
        self.step("stage")
    }
    fn refused(&mut self, _: Failure) -> R<()> {
        self.step("refused")
    }
    fn checked(&mut self) -> R<()> {
        self.step("checked")
    }
    fn cut_over(&mut self) -> R<()> {
        self.step("cut-over")
    }
}

impl SocketSteps for Recorded {
    fn transport(&mut self) -> R<()> {
        self.step("transport")
    }
    fn prepare(&mut self) -> R<()> {
        self.step("prepare")
    }
    fn inspect(&mut self) -> R<()> {
        self.step("inspect")
    }
    fn preflight(&mut self) -> R<()> {
        self.step("preflight")
    }
    fn units(&mut self) -> R<()> {
        self.step("units")
    }
    fn retire(&mut self) -> R<()> {
        self.step("retire")
    }
    fn pending(&mut self) -> R<()> {
        self.step("pending")
    }
    fn verify(&mut self) -> R<()> {
        self.step("verify")
    }
    fn refused(&mut self, _: Failure) -> R<()> {
        self.step("refused")
    }
    fn checked(&mut self) -> R<()> {
        self.step("checked")
    }
    fn finished(&mut self) -> R<()> {
        self.step("finished")
    }
}

#[test]
fn a_check_only_cutover_reads_stages_and_checks_and_never_changes_the_host() {
    let run = |plan: bool, failing: Option<&'static str>| {
        let mut steps = Recorded(Vec::new(), failing);
        let ok = cutover(&mut steps, plan).is_ok();
        (steps.0, ok)
    };
    assert_eq!(
        run(true, None),
        (vec!["inspect", "preflight", "stage", "checked"], true)
    );
    assert_eq!(
        run(false, None),
        (vec!["prepare", "preflight", "cut-over"], true)
    );
    // A check stops where nothing after can be judged; a refusal in
    // staging is kept, and the checks before the fence still run and end
    // it. A cutover stops at any refusal.
    assert_eq!(run(true, Some("inspect")), (vec!["inspect"], false));
    assert_eq!(
        run(true, Some("preflight")),
        (vec!["inspect", "preflight"], false)
    );
    assert_eq!(
        run(true, Some("stage")),
        (
            vec!["inspect", "preflight", "stage", "refused", "checked"],
            true
        )
    );
    assert_eq!(run(false, Some("prepare")), (vec!["prepare"], false));
    // On the socket, a check reads and records; a run installs and retires.
    let on = |plan: bool, failing: Option<&'static str>| {
        let mut steps = Recorded(Vec::new(), failing);
        let ok = on_socket(&mut steps, plan).is_ok();
        (steps.0, ok)
    };
    assert_eq!(
        on(true, None),
        (
            vec![
                "transport",
                "inspect",
                "preflight",
                "units",
                "pending",
                "verify",
                "checked"
            ],
            true
        )
    );
    assert_eq!(
        on(true, Some("units")),
        (
            vec![
                "transport",
                "inspect",
                "preflight",
                "units",
                "refused",
                "pending",
                "verify",
                "checked"
            ],
            true
        )
    );
    assert_eq!(
        on(true, Some("verify")),
        (
            vec![
                "transport",
                "inspect",
                "preflight",
                "units",
                "pending",
                "verify",
                "refused",
                "checked"
            ],
            true
        )
    );
    assert_eq!(
        on(false, None),
        (
            vec![
                "transport",
                "prepare",
                "preflight",
                "units",
                "retire",
                "verify",
                "finished"
            ],
            true
        )
    );
    assert_eq!(
        on(false, Some("units")),
        (vec!["transport", "prepare", "preflight", "units"], false)
    );
    // Only a job that asks is a check.
    for (job, plan) in [
        (r#"{"mode":"postgres-socket"}"#, Some(false)),
        (r#"{"plan_only":false}"#, Some(false)),
        (r#"{"plan_only":true}"#, Some(true)),
        (r#"{"plan_only":"true"}"#, None),
        (r#"{"plan_only":null}"#, None),
    ] {
        assert_eq!(plan_only(&td_json::parse(job).unwrap()).ok(), plan, "{job}");
    }
}

#[test]
fn a_socket_directory_is_checked_only_when_it_exists() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let scratch = td_fs::TempDir::new("mta-socket-directory").unwrap();
    let path = scratch.path().join("socket");
    let (uid, gid) = {
        let meta = fs::metadata(scratch.path()).unwrap();
        (meta.uid(), meta.gid())
    };
    assert!(!socket_directory_owned(&path, uid, None).unwrap());
    assert!(!socket_directory_owned(&path, uid, Some(gid)).unwrap());
    fs::create_dir(&path).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o750)).unwrap();
    assert!(socket_directory_owned(&path, uid, Some(gid)).unwrap());
    // A group still to be created owns nothing yet.
    assert!(socket_directory_owned(&path, uid, None).is_err());
    assert!(socket_directory_owned(&path, uid.wrapping_add(1), Some(gid)).is_err());
    assert!(socket_directory_owned(&path, uid, Some(gid.wrapping_add(1))).is_err());
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(socket_directory_owned(&path, uid, Some(gid)).is_err());
}

#[test]
fn the_fence_needs_the_backup_and_health_timers() {
    let timers = |names: &[&str]| names.iter().map(|n| n.to_string()).collect::<Vec<_>>();
    fence::required_timers(&timers(&[
        "td-mta-health.timer",
        "td-mta-maint-backup.timer",
        "td-mta-maint-tls-0.timer",
    ]))
    .unwrap();
    assert!(fence::required_timers(&timers(&["td-mta-health.timer"])).is_err());
    assert!(fence::required_timers(&[]).is_err());
}

#[test]
fn production_copies_of_the_checks_are_recognised_and_nothing_else() {
    let units = [
        (
            "td-mta-maint-tls-0",
            "/var/lib/td-mta/mail/tls/server.pem",
            include_str!("production-units/td-mta-maint-tls-0.service"),
            include_str!("production-units/td-mta-maint-tls-0.timer"),
        ),
        (
            "td-mta-maint-tls-1",
            "/var/lib/td-mta/mail/tls/ca.pem",
            include_str!("production-units/td-mta-maint-tls-1.service"),
            include_str!("production-units/td-mta-maint-tls-1.timer"),
        ),
    ];
    // The readiness jobs' template carries the same block.
    let readiness = include_str!("../units/readiness.service.in");
    assert!(readiness.contains(&format!("[Service]\n{}Type=oneshot\n", checks::HARDENING)));
    for (name, file, service, timer) in units {
        let check = TlsCheck {
            name: name.into(),
            argv: openssl_check(file),
        };
        assert_eq!(hardened_service_text(&check).unwrap(), service);
        assert_eq!(timer_text(&check).unwrap(), timer);
        let forms = [
            service_text(&check).unwrap(),
            hardened_service_text(&check).unwrap(),
        ];
        let service_unit = check.unit("service").unwrap();
        let timer_unit = check.unit("timer").unwrap();
        let etc = |unit: &str| PathBuf::from(format!("/etc/systemd/system/{unit}"));
        let copy = etc(&service_unit);
        assert!(copied_unit(&service_unit, &copy, &copy, true, 0, 0o100644));
        generated_check(&service_unit, &copy, true, service.as_bytes(), &forms).unwrap();
        generated_check(&service_unit, &copy, true, forms[0].as_bytes(), &forms).unwrap();
        let timer_copy = etc(&timer_unit);
        assert!(copied_unit(
            &timer_unit,
            &timer_copy,
            &timer_copy,
            true,
            0,
            0o644
        ));
        generated_check(
            &timer_unit,
            &timer_copy,
            true,
            timer.as_bytes(),
            &[timer.to_string()],
        )
        .unwrap();
        // Not root's regular file, a link elsewhere, another name or
        // directory: not a copy.
        let share = PathBuf::from(format!("/opt/td-mta/share/td-mta/services/{service_unit}"));
        // Not a regular file, not root's, writable by others:
        assert!(!copied_unit(&service_unit, &copy, &copy, false, 0, 0o644));
        assert!(!copied_unit(&service_unit, &copy, &copy, true, 1000, 0o644));
        assert!(!copied_unit(&service_unit, &copy, &copy, true, 0, 0o664));
        assert!(!copied_unit(&service_unit, &copy, &copy, true, 0, 0o646));
        assert!(copied_unit(&service_unit, &copy, &copy, true, 0, 0o600));
        assert!(!copied_unit(&service_unit, &copy, &share, true, 0, 0o644));
        assert!(!copied_unit(
            &service_unit,
            &etc("other.service"),
            &etc("other.service"),
            true,
            0,
            0o644
        ));
        let nested = PathBuf::from(format!("/etc/systemd/system/x/{service_unit}"));
        assert!(!copied_unit(
            &service_unit,
            &nested,
            &nested,
            true,
            0,
            0o644
        ));
        assert!(generated_check(&service_unit, &copy, false, service.as_bytes(), &forms).is_err());
        // Any other change to the copy still refuses.
        for changed in [
            service.replace("MemoryMax=512M", "MemoryMax=1G"),
            service.replace("User=td-mta\n", ""),
            service.replace(
                "PrivateTmp=yes\n",
                "PrivateTmp=yes\nExecStartPost=/bin/true\n",
            ),
            service.replace("2592000", "86400"),
            format!("{service}\n"),
        ] {
            assert!(
                generated_check(&service_unit, &copy, true, changed.as_bytes(), &forms).is_err(),
                "{changed}"
            );
        }
        assert!(generated_check(
            &timer_unit,
            &timer_copy,
            true,
            timer.replace("15min", "1min").as_bytes(),
            &[timer.to_string()],
        )
        .is_err());
    }
}

#[test]
fn a_retired_unit_file_keeps_its_bytes_owner_and_mode() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let scratch = td_fs::TempDir::new("mta-retired-unit").unwrap();
    let kept = scratch.path().join("units");
    fs::create_dir(&kept).unwrap();
    let path = scratch.path().join("td-mta-maint-tls-1.service");
    let text = include_str!("production-units/td-mta-maint-tls-1.service");
    fs::write(&path, text).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    let before = fs::metadata(&path).unwrap();
    retire_file(&path, &kept).unwrap();
    assert!(!path.exists());
    let moved = kept.join("td-mta-maint-tls-1.service");
    assert_eq!(fs::read_to_string(&moved).unwrap(), text);
    let after = fs::metadata(&moved).unwrap();
    assert_eq!(after.mode() & 0o7777, 0o644);
    assert_eq!((after.uid(), after.gid()), (before.uid(), before.gid()));
    // Copying it back restores the file as it was.
    fs::copy(&moved, &path).unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), text);
    assert_eq!(fs::metadata(&path).unwrap().mode() & 0o7777, 0o644);
}

#[test]
fn a_check_collects_every_refusal_and_a_cutover_stops_at_the_first() {
    let refuse = |why: &'static str| -> R<u8> { Err(why.into()) };
    let mut check = Refusals::new(true);
    assert_eq!(check.take(R::<u8>::Ok(1)).unwrap(), Some(1));
    assert_eq!(check.take(refuse("first")).unwrap(), None);
    assert_eq!(check.take(refuse("second")).unwrap(), None);
    assert_eq!(check.found(), ["first", "second"]);
    let mut cutover = Refusals::new(false);
    assert_eq!(cutover.take(R::<u8>::Ok(1)).unwrap(), Some(1));
    assert_eq!(
        cutover.take(refuse("first")).unwrap_err().to_string(),
        "first"
    );
    assert!(cutover.found().is_empty());
    // A check names the file a refusal concerns; a cutover's message is
    // its own.
    let path = Path::new("/var/lib/td-mta/mail/owner-connection");
    assert_eq!(check.take_for(path, refuse("third")).unwrap(), None);
    assert_eq!(
        check.found().last().map(String::as_str),
        Some("/var/lib/td-mta/mail/owner-connection: third")
    );
    assert_eq!(check.take_for(path, R::<u8>::Ok(2)).unwrap(), Some(2));
    assert_eq!(
        cutover
            .take_for(path, refuse("third"))
            .unwrap_err()
            .to_string(),
        "third"
    );
}

#[test]
fn a_checks_service_and_timer_are_of_one_kind() {
    let share = || Some(PathBuf::from("/opt/td-mta/share/td-mta/services"));
    let other = Some(PathBuf::from("/opt/other/share/td-mta/services"));
    for kinds in [
        vec![None, None],
        vec![share(), share()],
        vec![None],
        vec![share()],
        vec![],
    ] {
        same_kind("td-mta-maint-tls-0", &kinds).unwrap();
    }
    for kinds in [
        vec![None, share()],
        vec![share(), None],
        vec![share(), other],
    ] {
        assert!(
            same_kind("td-mta-maint-tls-0", &kinds).is_err(),
            "{kinds:?}"
        );
    }
}
