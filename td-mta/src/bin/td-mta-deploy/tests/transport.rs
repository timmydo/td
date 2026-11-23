#![allow(clippy::unwrap_used)]
use super::*;

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
    assert_eq!(
        pg_hba(original).unwrap(),
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
        assert!(pg_hba(refused).is_err(), "{refused}");
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
    assert_eq!(retired_reference(&clean, &converted).unwrap(), None);
    assert_eq!(retired_reference("", &converted).unwrap(), None);
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
            retired_reference(&show, &converted).unwrap(),
            Some("td-mta-maint-x.service"),
            "{why}"
        );
    }
    let dangling = unit("td-mta-maint-x.service", "/bin/td-mta x --connection-file");
    assert!(retired_reference(&dangling, &converted).is_err());
    assert!(retired_reference("ExecStart=x", &converted).is_err());
}
