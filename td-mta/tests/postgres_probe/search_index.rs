use super::{deadline, sql, sql_commands_bounded, Fixture};
use td_crypto::TrustStore;

#[test]
#[ignore = "real PostgreSQL 17 with pg_trgm 1.6 required"]
pub(super) fn scoped_candidates_do_not_materialize_the_account() {
    let fixture = Fixture::new();
    let trust = TrustStore::from_pem(&fixture.ca).unwrap();
    td_mta::postgres_schema::initialize(&fixture.request(), &trust, deadline()).unwrap();
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
