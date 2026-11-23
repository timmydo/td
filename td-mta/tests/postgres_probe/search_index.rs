use super::{deadline, sql, sql_commands_bounded, Fixture};

#[test]
#[ignore = "real PostgreSQL 17 required"]
pub(super) fn received_order_upgrade_preserves_pages_and_indexes_the_first_page() {
    use td_mta::postgres_schema;

    let fixture = populated();
    let request = fixture.request();
    let fresh = postgres_schema::inspect(&request, deadline()).unwrap();
    super::rewind_email_order(&fixture);
    assert_eq!(
        sql(
            &fixture,
            "SELECT max(version) FROM td_mta.schema_migrations"
        ),
        "44"
    );
    sql(&fixture, "UPDATE td_mta.emails SET received_at=received_at / 10;
        INSERT INTO td_mta.memberships SELECT account,id,account,changed FROM td_mta.emails ON CONFLICT DO NOTHING;
        ANALYZE td_mta.emails; ANALYZE td_mta.memberships");
    let asc = include_str!("../../src/postgres_email_page_asc.sql");
    let desc = include_str!("../../src/postgres_email_page_desc.sql");
    let account = "decode(repeat('01',16),'hex')";
    let pages = || {
        [asc, desc].map(|query| {
            sql_commands_bounded(
                &fixture,
                &[
                    &format!("PREPARE page(bytea,bytea,bigint,bigint,bigint,bigint) AS {query}"),
                    &format!("EXECUTE page({account},{account},51,0,NULL,NULL)"),
                    &format!("EXECUTE page({account},{account},51,50,NULL,NULL)"),
                ],
                32 * 1024,
            )
        })
    };
    let before = pages();
    let upgraded = postgres_schema::upgrade(&request, deadline()).unwrap();
    assert_eq!(upgraded, fresh);
    assert_eq!(pages(), before, "tied dates retain both ordered pages");
    assert_eq!(
        sql(&fixture, "SELECT pg_get_indexdef(to_regclass('td_mta.emails_received_order'))"),
        "CREATE INDEX emails_received_order ON td_mta.emails USING btree (account, received_at DESC, id)"
    );
    let report = sql_commands_bounded(&fixture, &[
        "SET random_page_cost=2",
        &format!("PREPARE page(bytea,bytea,bigint,bigint,bigint,bigint) AS {desc}"),
        &format!("EXPLAIN (ANALYZE, BUFFERS, TIMING OFF) EXECUTE page({account},{account},51,0,NULL,NULL)"),
    ], 32 * 1024);
    assert!(report.contains("using emails_received_order"), "{report}");
    assert!(!report.contains("Sort Key:"), "{report}");
}

fn populated() -> Fixture {
    let fixture = Fixture::new();
    td_mta::postgres_schema::initialize(&fixture.request(), deadline()).unwrap();
    // The production schema and indexes; thousands of unrelated messages make
    // account-wide candidate materialization visible without a timing bound.
    sql(&fixture, "INSERT INTO td_mta.accounts VALUES(decode(repeat('01',16),'hex'),decode(repeat('00',7)||'01','hex'),decode(repeat('00',8),'hex'));
        INSERT INTO td_mta.threads SELECT id,id,sequence FROM td_mta.accounts;
        INSERT INTO td_mta.mailboxes SELECT id,id,'small',NULL,NULL,0,1,sequence FROM td_mta.accounts;
        INSERT INTO td_mta.blob_ids SELECT a.id,decode(md5(n::text),'hex') FROM td_mta.accounts a CROSS JOIN generate_series(1,5000) n;
        INSERT INTO td_mta.blobs SELECT account,id,0,sha256(''::bytea),0,decode(repeat('00',7)||'01','hex'),0 FROM td_mta.blob_ids;
        INSERT INTO td_mta.emails(account,id,blob_id,thread_id,received_at,origin,changed) SELECT a.id,decode(md5(n::text),'hex'),decode(md5(n::text),'hex'),a.id,n,2,a.sequence FROM td_mta.accounts a CROSS JOIN generate_series(1,5000) n;
        INSERT INTO td_mta.memberships SELECT account,id,account,changed FROM td_mta.emails WHERE received_at>=4999;
        ANALYZE td_mta.emails; ANALYZE td_mta.blobs; ANALYZE td_mta.memberships;");
    fixture
}

#[test]
#[ignore = "real PostgreSQL 17 with pg_trgm 1.6 required"]
pub(super) fn scoped_candidates_do_not_materialize_the_account() {
    let fixture = populated();
    for indexed in [false, true] {
        if indexed {
            sql(&fixture, "INSERT INTO td_mta.blob_search(account,blob_id,version,body_length,body_digest,fields,fields_digest,candidate_text) SELECT account,id,1,length,digest,decode('000000066e6565646c65','hex'),sha256('fixture'::bytea),'needle' FROM td_mta.blobs; ANALYZE td_mta.blob_search");
        }
        for query in [
            include_str!("../../src/postgres_search_candidates_asc.sql"),
            include_str!("../../src/postgres_search_candidates_desc.sql"),
        ] {
            for (mailbox, after, before) in [
                ("NULL", "4999", "5001"),
                ("decode(repeat('01',16),'hex')", "NULL", "NULL"),
            ] {
                for strings in ["on", "off"] {
                    let setting = format!("SET standard_conforming_strings={strings}");
                    let report = sql_commands_bounded(&fixture, &[&setting, &format!(
                    "PREPARE scoped(bytea,bytea,bigint,bigint,bigint,bigint,text,integer) AS {query}; EXPLAIN (ANALYZE, FORMAT JSON, TIMING OFF) EXECUTE scoped(decode(repeat('01',16),'hex'),{mailbox},33,0,{after},{before},'%needle%',1)")], 256 * 1024);
                    let json = report.get(report.find('[').unwrap()..).unwrap();
                    let plan = td_json::parse(json).unwrap();
                    assert_eq!(plan[0]["Plan"]["Actual Rows"].as_u64(), Some(2), "{report}");
                    let mut pending = vec![&plan[0]["Plan"]];
                    while let Some(node) = pending.pop() {
                        if matches!(
                            node["Node Type"].as_str(),
                            Some("Append" | "Merge Append" | "Materialize")
                        ) || node["Subplan Name"]
                            .as_str()
                            .is_some_and(|name| name.starts_with("CTE "))
                        {
                            assert!(
                                node["Actual Rows"].as_u64().unwrap() <= 2,
                                "unrelated candidate rows materialized: {report}"
                            );
                        }
                        if let Some(children) = node["Plans"].as_array() {
                            pending.extend(children);
                        }
                    }
                }
            }
        }
    }
}
