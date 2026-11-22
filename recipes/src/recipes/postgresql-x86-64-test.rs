use crate::shared::{BUILD_ROOT_GLIBC, BUILD_ROOT_PATH, BUILD_ROOT_SH as SH};
use crate::types::{CheckRunner, Recipe, RecipeCheck, Step};

// PostgreSQL as td's mail server uses it, from the realized store output:
// initdb a SCRAM-only UTF-8 cluster in the build directory, start it on a
// Unix socket alone, check the settings the mail server's startup probe and
// physical backup verify, create pg_trgm 1.6 in the mail schema's own
// namespace and search through its index, take and restore a
// gzip-compressed custom dump, take and verify a base backup, and stop the
// server cleanly. It runs in the Debian build root,
// whose /bin/sh initdb and pg_ctl start the server through. Each check is
// SQL that raises, so psql's exit status grades it.

const PASSWORD: &str = "pg-test-password";

/// The mail server's startup probe (`postgres_probe::qualify`: version 17,
/// a writable primary with fsync and full-page writes), its schema check's
/// encoding, its physical backup's WAL settings, and the transport this
/// build serves: SCRAM over the socket, no TLS and no TCP listener.
const SETTINGS: &str = r#"DO $check$
DECLARE
  failed text;
BEGIN
  SELECT concat_ws(', ',
    CASE WHEN current_setting('server_version_num') <> '170012'
      THEN 'server_version_num ' || current_setting('server_version_num') END,
    CASE WHEN pg_is_in_recovery() THEN 'in recovery' END,
    CASE WHEN current_setting('transaction_read_only') <> 'off'
      THEN 'transaction_read_only on' END,
    CASE WHEN current_setting('fsync') <> 'on' THEN 'fsync off' END,
    CASE WHEN current_setting('full_page_writes') <> 'on' THEN 'full_page_writes off' END,
    CASE WHEN current_setting('synchronous_commit') <> 'on'
      THEN 'synchronous_commit ' || current_setting('synchronous_commit') END,
    CASE WHEN current_setting('server_encoding') <> 'UTF8'
      THEN 'server_encoding ' || current_setting('server_encoding') END,
    CASE WHEN current_setting('wal_level') NOT IN ('replica', 'logical')
      THEN 'wal_level ' || current_setting('wal_level') END,
    CASE WHEN current_setting('max_wal_senders')::int
        - (SELECT count(*) FROM pg_stat_replication) < 3
      THEN 'fewer than three free WAL senders' END,
    CASE WHEN current_setting('synchronous_standby_names') <> ''
      THEN 'synchronous_standby_names set' END,
    CASE WHEN current_setting('password_encryption') <> 'scram-sha-256'
      THEN 'password_encryption ' || current_setting('password_encryption') END,
    CASE WHEN system_user IS DISTINCT FROM 'scram-sha-256:td'
      THEN 'authenticated as ' || coalesce(system_user, 'nobody') END,
    CASE WHEN current_setting('ssl') <> 'off' THEN 'ssl on' END,
    CASE WHEN current_setting('listen_addresses') <> ''
      THEN 'listening on ' || current_setting('listen_addresses') END)
  INTO failed;
  IF failed <> '' THEN
    RAISE EXCEPTION 'settings the mail server verifies: %', failed;
  END IF;
END
$check$;
BEGIN;
SET LOCAL synchronous_commit = on;
CREATE TABLE durable (n int PRIMARY KEY);
INSERT INTO durable VALUES (1);
COMMIT;
"#;

/// The mail server's indexed search: pg_trgm 1.6 in a schema of its own, a
/// GIN trigram index, and a literal substring match the planner answers
/// from it.
const SEARCH: &str = r#"CREATE SCHEMA td_mta_trgm;
CREATE EXTENSION pg_trgm WITH SCHEMA td_mta_trgm VERSION '1.6';
CREATE TABLE bodies (id int PRIMARY KEY, body text NOT NULL);
INSERT INTO bodies
  SELECT n, 'message ' || n || CASE WHEN n = 4242 THEN ' quarterly invoice attached'
    ELSE ' nothing to see' END
  FROM generate_series(1, 20000) AS n;
CREATE INDEX bodies_trgm ON bodies USING gin (body td_mta_trgm.gin_trgm_ops);
ANALYZE bodies;
DO $search$
DECLARE
  plan text := '';
  line text;
BEGIN
  IF (SELECT extversion FROM pg_extension WHERE extname = 'pg_trgm') IS DISTINCT FROM '1.6' THEN
    RAISE EXCEPTION 'pg_trgm is not version 1.6';
  END IF;
  PERFORM set_config('enable_seqscan', 'off', true);
  FOR line IN EXECUTE 'EXPLAIN SELECT id FROM bodies WHERE body LIKE ''%quarterly invoice%''' LOOP
    plan := plan || line || E'\n';
  END LOOP;
  IF position('bodies_trgm' IN plan) = 0 THEN
    RAISE EXCEPTION 'the search did not use the trigram index: %', plan;
  END IF;
  IF (SELECT array_agg(id) FROM bodies WHERE body LIKE '%quarterly invoice%')
      IS DISTINCT FROM ARRAY[4242] THEN
    RAISE EXCEPTION 'the search found the wrong bodies';
  END IF;
  IF td_mta_trgm.similarity('invoice', 'invoices') < 0.5 THEN
    RAISE EXCEPTION 'pg_trgm similarity is wrong';
  END IF;
END
$search$;
"#;

/// The restored database holds what the dump took, the extension in its
/// schema and the index among it.
const RESTORED: &str = r#"DO $restored$
BEGIN
  IF (SELECT count(*) FROM bodies) <> 20000 THEN
    RAISE EXCEPTION 'the restore lost bodies';
  END IF;
  IF (SELECT e.extversion FROM pg_extension e JOIN pg_namespace n ON n.oid = e.extnamespace
      WHERE e.extname = 'pg_trgm' AND n.nspname = 'td_mta_trgm') IS DISTINCT FROM '1.6' THEN
    RAISE EXCEPTION 'pg_trgm 1.6 was not restored into td_mta_trgm';
  END IF;
  IF to_regclass('public.bodies_trgm') IS NULL THEN
    RAISE EXCEPTION 'the trigram index was not restored';
  END IF;
  IF (SELECT array_agg(id) FROM bodies WHERE body LIKE '%quarterly invoice%')
      IS DISTINCT FROM ARRAY[4242] THEN
    RAISE EXCEPTION 'the restored search found the wrong bodies';
  END IF;
  IF (SELECT n FROM durable) IS DISTINCT FROM 1 THEN
    RAISE EXCEPTION 'the committed row was not restored';
  END IF;
END
$restored$;
"#;

pub fn recipe() -> Recipe {
    let pg = "{in:postgresql-x86-64}";
    let bin = |program: &str| format!("{pg}/bin/{program}");
    let data = "{root}/data";
    let socket = "{root}/s";
    // A client step: the socket, the role, never a prompt, the password.
    let client = |program: &str, args: &[&str]| {
        let mut argv = vec![bin(program)];
        argv.extend(
            ["-h", socket, "-U", "td", "-w"]
                .into_iter()
                .chain(args.iter().copied())
                .map(String::from),
        );
        Step::Run {
            dir: "{root}".into(),
            argv,
            env: vec![
                ("PATH".into(), BUILD_ROOT_PATH.into()),
                ("PGPASSWORD".into(), PASSWORD.into()),
                ("TZ".into(), "UTC".into()),
            ],
        }
    };
    let sql = |database: &str, file: &str| {
        client(
            "psql",
            &[
                "-d",
                database,
                "-X",
                "-q",
                "-v",
                "ON_ERROR_STOP=1",
                "-f",
                file,
            ],
        )
    };

    let mut steps = vec![
        Step::Require {
            paths: super::postgresql_x86_64::PROGRAMS
                .iter()
                .map(|program| bin(program))
                .collect(),
            exec: true,
        },
        Step::Require {
            paths: [
                "lib/debug/bin/postgres.debug",
                "lib/debug/bin/psql.debug",
                "lib/debug/lib/pg_trgm.so.debug",
                "lib/debug/lib/plpgsql.so.debug",
            ]
            .iter()
            .map(|file| format!("{pg}/{file}"))
            .collect(),
            exec: false,
        },
        Step::run(
            "{root}",
            &[
                SH,
                "-c",
                &format!(
                    "set -e; g='{BUILD_ROOT_GLIBC}/lib'; pg='{pg}'; \
                     v=$(\"$pg/bin/postgres\" --version); [ \"$v\" = 'postgres (PostgreSQL) 17.12' ] || \
                     {{ echo \"postgres --version: $v\" >&2; exit 1; }}; \
                     for f in bin/postgres bin/initdb bin/pg_ctl bin/psql bin/pg_dump bin/pg_basebackup \
                     lib/libpq.so.5 lib/plpgsql.so lib/pg_trgm.so; do e=$pg/$f; \
                     readelf -h \"$e\" | grep -Fq 'Advanced Micro Devices X86-64' || {{ echo \"$f is not x86-64\" >&2; exit 1; }}; \
                     case $f in bin/*) readelf -l \"$e\" | grep -Fq \"[Requesting program interpreter: $g/ld-linux-x86-64.so.2]\" || \
                     {{ echo \"$f does not load through td glibc\" >&2; exit 1; }};; esac; \
                     d=$(readelf -d \"$e\"); \
                     for n in $(printf '%s\\n' \"$d\" | sed -n 's/.*Shared library: \\[\\(.*\\)\\]/\\1/p'); do \
                     case $n in libc.so.6|libm.so.6|libpq.so.5) ;; *) echo \"$f needs $n\" >&2; exit 1;; esac; done; \
                     if printf '%s\\n' \"$d\" | grep -Fq '(RPATH)'; then echo \"$f carries a legacy RPATH\" >&2; exit 1; fi; \
                     r=$(printf '%s\\n' \"$d\" | sed -n 's/.*Library runpath: \\[\\(.*\\)\\]/\\1/p'); \
                     for p in $(printf '%s\\n' \"$r\" | tr ':' ' '); do case $p in \"$g\"|\"$pg/lib\") ;; \
                     *) echo \"$f searches $p\" >&2; exit 1;; esac; done; \
                     case :$r: in *\":$g:\"*) ;; *) echo \"$f does not search td glibc\" >&2; exit 1;; esac; done; \
                     for p in include lib/pgxs lib/pkgconfig bin/pg_config bin/ecpg lib/libpq.a; do \
                     [ ! -e \"$pg/$p\" ] || {{ echo \"the development file $p ships\" >&2; exit 1; }}; done"
                ),
            ],
        )
        .env("PATH", BUILD_ROOT_PATH),
        Step::WriteFile {
            path: "{root}/password".into(),
            content: format!("{PASSWORD}\n"),
            exec: false,
        },
        Step::MkDir {
            path: socket.into(),
        },
        Step::WriteFile {
            path: "{root}/settings.sql".into(),
            content: SETTINGS.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{root}/search.sql".into(),
            content: SEARCH.into(),
            exec: false,
        },
        Step::WriteFile {
            path: "{root}/restored.sql".into(),
            content: RESTORED.into(),
            exec: false,
        },
        // As the mail server's fixtures make theirs: SCRAM for every
        // connection, C collation, UTF-8. The socket in the build directory
        // is the only listener.
        Step::run(
            "{root}",
            &[
                &bin("initdb"),
                "--no-sync",
                "-D",
                data,
                "-U",
                "td",
                "--pwfile={root}/password",
                "--auth=scram-sha-256",
                "--no-locale",
                "-E",
                "UTF8",
                "-c",
                "listen_addresses=",
                "-c",
                "unix_socket_directories={root}/s",
            ],
        )
        .env("PATH", BUILD_ROOT_PATH)
        .env("TZ", "UTC"),
        Step::run(
            "{root}",
            &[
                SH,
                "-c",
                &format!(
                    "'{}' -D '{data}' -l '{{root}}/server.log' -w -t 120 start || \
                     {{ cat '{{root}}/server.log' >&2; exit 1; }}",
                    bin("pg_ctl")
                ),
            ],
        )
        .env("PATH", BUILD_ROOT_PATH)
        .env("TZ", "UTC"),
        // The readiness probe a supervisor would run; it takes no password.
        Step::run(
            "{root}",
            &[
                &bin("pg_isready"),
                "-h",
                socket,
                "-d",
                "postgres",
                "-t",
                "30",
            ],
        )
        .env("PATH", BUILD_ROOT_PATH),
        sql("postgres", "{root}/settings.sql"),
        sql("postgres", "{root}/search.sql"),
        // SCRAM is enforced, not merely offered.
        Step::run(
            "{root}",
            &[
                SH,
                "-c",
                &format!(
                    "if PGPASSWORD=wrong '{}' -h '{socket}' -U td -w -d postgres -X -c 'SELECT 1' 2> wrong.log; then \
                     echo 'a wrong password was accepted' >&2; exit 1; fi; \
                     grep -Fq 'password authentication failed for user \"td\"' wrong.log || {{ cat wrong.log >&2; exit 1; }}",
                    bin("psql")
                ),
            ],
        )
        .env("PATH", BUILD_ROOT_PATH),
        // The build has no TLS to turn on: `ssl = on` fails its check hook.
        Step::run(
            "{root}",
            &[
                SH,
                "-c",
                &format!(
                    "if '{}' -h '{socket}' -U td -w -d postgres -X -c 'ALTER SYSTEM SET ssl = on' 2> ssl.log; then \
                     echo 'the server accepted ssl = on' >&2; exit 1; fi; \
                     grep -Fq 'SSL is not supported by this build' ssl.log || {{ cat ssl.log >&2; exit 1; }}",
                    bin("psql")
                ),
            ],
        )
        .env("PATH", BUILD_ROOT_PATH)
        .env("PGPASSWORD", PASSWORD),
        // gzip is what td zlib gives the custom format; a build without it
        // refuses the method.
        client(
            "pg_dump",
            &[
                "-d",
                "postgres",
                "--format=custom",
                "--compress=gzip:6",
                "--file={root}/mail.dump",
            ],
        ),
        client("psql", &["-d", "postgres", "-X", "-q", "-c", "CREATE DATABASE restored"]),
        client(
            "pg_restore",
            &[
                "-d",
                "restored",
                "--exit-on-error",
                "--single-transaction",
                "{root}/mail.dump",
            ],
        ),
        sql("restored", "{root}/restored.sql"),
        client(
            "pg_basebackup",
            &["-D", "{root}/base", "--wal-method=stream", "--checkpoint=fast", "--no-sync"],
        ),
        Step::run("{root}", &[&bin("pg_verifybackup"), "{root}/base"])
            .env("PATH", BUILD_ROOT_PATH),
        Step::run(
            "{root}",
            &[&bin("pg_ctl"), "-D", data, "-w", "-t", "120", "-m", "fast", "stop"],
        )
        .env("PATH", BUILD_ROOT_PATH),
        Step::run(
            "{root}",
            &[
                SH,
                "-c",
                &format!(
                    "set -e; state=$('{}' '{data}' | sed -n 's/^Database cluster state: *//p'); \
                     [ \"$state\" = 'shut down' ] || {{ echo \"cluster state after stop: $state\" >&2; exit 1; }}; \
                     grep -Fq 'database system is shut down' '{{root}}/server.log' || \
                     {{ cat '{{root}}/server.log' >&2; exit 1; }}; \
                     '{}' --list '{{root}}/mail.dump' > dump.list; \
                     grep -Eq '^; +Compression: gzip' dump.list || {{ echo 'the dump is not gzip-compressed' >&2; cat dump.list >&2; exit 1; }}",
                    bin("pg_controldata"),
                    bin("pg_restore")
                ),
            ],
        )
        .env("PATH", BUILD_ROOT_PATH)
        .env("LC_ALL", "C"),
    ];
    steps.push(Step::MkDir {
        path: "{out}".into(),
    });
    steps.push(Step::WriteFile {
        path: "{out}/result".into(),
        content: "PASS: PostgreSQL 17.12 initdb, start on a Unix socket under SCRAM, the settings the mail server verifies, pg_trgm 1.6 indexed search, a gzip custom dump restored, a verified base backup, and a clean stop\n".into(),
        exec: false,
    });
    steps.push(Step::Require {
        paths: vec!["{out}/result".into()],
        exec: false,
    });

    Recipe::mesboot("postgresql-x86-64-test", "1.0")
        .build_root()
        .native_inputs(&["postgresql-x86-64", "glibc-x86-64"])
        .steps(steps)
        .checks(vec![RecipeCheck::new(
            r#"
echo ">> recipe-check postgresql-x86-64-test: build PostgreSQL 17, run a cluster on a Unix socket, pg_trgm search, dump and restore, base backup, clean stop"
: "${TD_RECIPE_EVAL:=$PWD/target/release/td-recipe-eval}"
exec "$TD_RECIPE_EVAL" check-run postgresql-x86-64-test 1
"#,
        )
        .with_runner(CheckRunner::BuildOnly)])
}

#[cfg(test)]
mod tests {
    use super::{recipe, Step, PASSWORD};

    #[test]
    fn the_test_runs_the_realized_server_in_the_root() {
        let recipe = recipe();
        crate::shared::assert_build_root_test(&recipe, &["postgresql-x86-64", "glibc-x86-64"], &[]);
        let steps = recipe.steps.unwrap_or_default();
        let runs = |program: &str| {
            steps
                .iter()
                .filter(|step| {
                    matches!(step, Step::Run { argv, .. }
                    if argv.first().is_some_and(|a| a == &format!("{{in:postgresql-x86-64}}/bin/{program}")))
                })
                .count()
        };
        for (program, count) in [
            ("initdb", 1),
            ("psql", 4),
            ("pg_dump", 1),
            ("pg_restore", 1),
            ("pg_basebackup", 1),
            ("pg_verifybackup", 1),
            ("pg_ctl", 1),
            ("pg_isready", 1),
        ] {
            assert_eq!(runs(program), count, "{program}");
        }
        // The cluster is SCRAM-only and the server listens on no TCP port.
        let initdb = steps
            .iter()
            .find_map(|step| match step {
                Step::Run { argv, .. } if argv.first().is_some_and(|a| a.ends_with("/initdb")) => {
                    Some(argv)
                }
                _ => None,
            })
            .expect("initdb");
        for arg in [
            "--auth=scram-sha-256",
            "listen_addresses=",
            "--no-locale",
            "UTF8",
        ] {
            assert!(initdb.iter().any(|a| a == arg), "{arg}");
        }
        // Every client authenticates with the cluster's password;
        // pg_isready only probes and sends none.
        for step in &steps {
            if let Step::Run { argv, env, .. } = step {
                let probe = argv.first().is_some_and(|a| a.ends_with("/pg_isready"));
                if !probe && argv.iter().any(|a| a == "-h") {
                    assert!(env.iter().any(|(k, v)| k == "PGPASSWORD" && v == PASSWORD));
                }
            }
        }
    }
}
