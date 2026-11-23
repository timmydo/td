//! Bounded passive capacity observations; no message or credential contents.
use crate::{
    config::{composition, load, stanza, storage, stream},
    ids::StoreEpoch,
    postgres_configuration_store as versions, postgres_probe, postgres_selection,
};
use std::time::Instant;
use td_json::{json, Json};
const NAME: &str = "postgres-capacity-inspect";
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Error;
fn failed() -> Error {
    Error
}
fn metric(scope: &str, name: &str, used: Option<u64>, limit: u64, kind: &str) -> Json {
    json!({"scope":scope,"name":name,"used":used,"limit":limit,"kind":kind})
}
/// Read configured limits and a bounded consistent counter snapshot. The selected
/// policy must remain unchanged through the observation. This is not an audit.
pub fn inspect(
    request: &postgres_probe::Request<'_>,
    trust: &td_crypto::TrustStore,
    deadline: Instant,
    epoch: StoreEpoch,
) -> Result<Json, Error> {
    let selected = postgres_selection::observe(request, trust, deadline, epoch)
        .map_err(|_| Error)?
        .ok_or_else(failed)?;
    let sources = versions::read_sources(request, trust, deadline, epoch, selected.version)
        .map_err(|_| Error)?;
    let (mut rows, quotas) = configured(&sources)?;
    let db = postgres_probe::with_capacity_client(request,trust,deadline, |client| {
            let schema = client.inspect_schema().map_err(|_| ())?;
            if schema.store_epoch != epoch { return Err(()); }
            client.batch("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY; SET LOCAL statement_timeout=5000; SET LOCAL idle_in_transaction_session_timeout=10000").map_err(|_| ())?;
            let values = database(client, &quotas)?;
            client.rollback().map_err(|_| ())?;
            Ok(values)
        }).map_err(|_| failed())?.map_err(|_| failed())?;
    rows.extend(db);
    // A selection change cannot silently pair new counters with old policy.
    if postgres_selection::observe(request, trust, deadline, epoch).map_err(|_| Error)?
        != Some(selected)
    {
        return Err(failed());
    }
    Ok(
        json!({"schema":1,"command":NAME,"status":"ok","store_epoch":epoch.to_string(),
            "generation":selected.generation,"version":selected.version.to_string(),"metrics":rows,
            "usage_audited":false,"runtime_occupancy_measured":false}),
    )
}
fn configured(sources: &versions::Sources) -> Result<(Vec<Json>, [u64; 5]), Error> {
    let (shared, profiles) = sources.parts().map_err(|_| Error)?;
    let mut rows = vec![metric(
        "configuration",
        "profiles",
        Some(profiles.len() as u64),
        crate::postgres_configuration::MAX_PROFILES as u64,
        "capacity",
    )];
    let mut quotas = None;
    for profile in profiles {
        let mut scratch = vec![0; stream::SCRATCH_BYTES];
        let source =
            composition::Source::new(shared, profile.source, &mut scratch).map_err(|_| failed())?;
        let prepared = load::read_profile(
            storage::Storage::try_new().map_err(|_| failed())?,
            &mut stanza::Pending::new(),
            &mut scratch,
            &mut source.reader(),
            profile.roles,
        )
        .map_err(|_| failed())?;
        let candidate = prepared.candidate();
        let resources = candidate.resources();
        let plan = resources.admission();
        let quota = [
            plan.disk().body_bytes,
            plan.disk().blob_count,
            plan.upload_bytes(),
            plan.queue_bytes(),
            plan.queue_submissions(),
        ];
        if quotas.is_some_and(|old| old != quota) {
            return Err(failed());
        }
        quotas = Some(quota);
        rows.push(metric(
            profile.name,
            "configured_accounts",
            Some(
                candidate
                    .identities()
                    .map_err(|_| failed())?
                    .account_count() as u64,
            ),
            crate::config::identities::MAX_ACCOUNTS as u64,
            "capacity",
        ));
        let limits = resources.resources();
        rows.push(metric(
            profile.name,
            "planned_memory_bytes",
            Some(limits.total_bytes() as u64),
            limits.limits().memory_budget_bytes as u64,
            "reservation",
        ));
        for (name, value) in limits.limits().values() {
            let kind = if [
                "message_bytes",
                "header_bytes",
                "mime_depth",
                "mime_parts",
                "smtp_recipients",
                "json_bytes",
                "json_methods",
                "json_depth",
                "json_tokens",
                "jmap_method_bytes",
                "objects_per_method",
                "query_page",
                "transaction_bytes",
                "transaction_operations",
            ]
            .contains(&name)
            {
                "protocol"
            } else {
                "setting"
            };
            rows.push(metric(profile.name, name, None, value as u64, kind));
        }
        for (name, value) in plan.disk().values().chain(plan.work().values()) {
            rows.push(metric(profile.name, name, None, value, "setting"));
        }
        for (name, value) in [
            ("smtp_data_jobs", crate::limits::SMTP_DATA_JOBS as u64),
            (
                "smtp_data_per_peer",
                crate::limits::SMTP_DATA_PER_PEER as u64,
            ),
            (
                "outbound_recipient_batch",
                crate::limits::OUTBOUND_RECIPIENT_BATCH as u64,
            ),
            ("smtp_rate_peers", crate::limits::SMTP_RATE_PEERS as u64),
            (
                "smtp_rate_message_burst",
                crate::limits::SMTP_RATE_MESSAGE_BURST,
            ),
            ("smtp_rate_message_ms", crate::limits::SMTP_RATE_MESSAGE_MS),
            (
                "smtp_rate_block_bytes",
                crate::limits::SMTP_RATE_BLOCK as u64,
            ),
            (
                "smtp_rate_block_burst",
                crate::limits::SMTP_RATE_BLOCK_BURST,
            ),
            ("smtp_rate_block_ms", crate::limits::SMTP_RATE_BLOCK_MS),
        ] {
            rows.push(metric(profile.name, name, None, value, "setting"));
        }
    }
    Ok((rows, quotas.ok_or_else(failed)?))
}

fn count(
    client: &crate::postgres_probe::Client,
    table: &str,
    account: Option<&[u8]>,
    cap: u64,
) -> Result<u64, ()> {
    // table names come exclusively from the compiled roster below. The extra row
    // distinguishes exhausted from over-limit without scanning an unbounded table.
    let n = i64::try_from(cap.checked_add(1).ok_or(())?).map_err(|_| ())?;
    let row = if let Some(account) = account {
        client.query_one(&format!("SELECT count(*) FROM (SELECT 1 FROM td_mta.{table} WHERE account=$1 LIMIT $2) bounded"),&[&account,&n])
    } else {
        client.query_one(&format!("SELECT count(*) FROM (SELECT 1 FROM td_mta.{table} LIMIT $1) bounded"),&[&n])
    }.map_err(|_| ())?;
    let value: i64 = row.try_get(0).map_err(|_| ())?;
    u64::try_from(value).map_err(|_| ())
}

// Expired rows await the account's next publication of their kind and refuse
// nothing.
fn live(
    client: &crate::postgres_probe::Client,
    account: &[u8],
    query: &str,
    retention: i64,
    cap: u64,
) -> Result<u64, ()> {
    let n = i64::try_from(cap.checked_add(1).ok_or(())?).map_err(|_| ())?;
    let row = client
        .query_one(query, &[&account, &retention, &n])
        .map_err(|_| ())?;
    let value: i64 = row.try_get(0).map_err(|_| ())?;
    u64::try_from(value).map_err(|_| ())
}

fn database(client: &crate::postgres_probe::Client, quotas: &[u64; 5]) -> Result<Vec<Json>, ()> {
    use crate::{
        postgres_drafts, postgres_email_set, postgres_mailbox_set, postgres_selection,
        postgres_submissions, postgres_uploads,
    };
    let mut rows = Vec::new();
    let accounts = client.query("SELECT a.id,u.body_bytes,u.blob_count,u.upload_bytes,u.queue_bytes,u.queue_submissions FROM td_mta.accounts a LEFT JOIN td_mta.account_usage u ON a.id=u.account ORDER BY a.id LIMIT 17",&[]).map_err(|_| ())?;
    if accounts.len() > 16 {
        return Err(());
    }
    for row in &accounts {
        let id: &[u8] = row.try_get(0).map_err(|_| ())?;
        let (health_states, health_receipts) =
            crate::postgres_route_health::occupancy(client, id, None).map_err(|_| ())?;
        let scope = crate::ids::AccountId::from_bytes(id.try_into().map_err(|_| ())?).to_string();
        for (i, name) in [
            "body_bytes",
            "blob_count",
            "upload_bytes",
            "queue_bytes",
            "queue_submissions",
        ]
        .iter()
        .enumerate()
        {
            let used: i64 = row.try_get(i + 1).map_err(|_| ())?;
            rows.push(metric(
                &scope,
                name,
                Some(u64::try_from(used).map_err(|_| ())?),
                *quotas.get(i).ok_or(())?,
                "capacity",
            ));
        }
        let claims = count(client, "attempt_final_reservations", Some(id), 1024)?;
        for (table, cap, reserved) in [
            (
                "upload_receipts",
                postgres_uploads::MAX_RECEIPTS as u64,
                false,
            ),
            (
                "draft_receipts",
                postgres_drafts::MAX_RECEIPTS as u64,
                false,
            ),
            (
                "submission_receipts",
                postgres_submissions::MAX_RECEIPTS as u64,
                false,
            ),
            (
                "flag_receipts",
                postgres_email_set::MAX_RECEIPTS as u64,
                false,
            ),
            (
                "mailbox_create_receipts",
                postgres_mailbox_set::MAX_RECEIPTS as u64,
                false,
            ),
            (
                "mailboxes",
                crate::postgres_mailboxes::MAX_MAILBOXES as u64,
                false,
            ),
            (
                "devices",
                crate::device_credentials::DEVICE_RECORDS as u64,
                false,
            ),
            (
                "active_devices",
                crate::device_credentials::DEVICE_CAPACITY as u64,
                false,
            ),
            (
                "submission_cancel_receipts",
                crate::postgres_submission_cancel::MAX_RECEIPTS as u64,
                false,
            ),
            ("submission_attempts", 1024, false),
            ("route_health", 1024, false),
            ("route_health_receipts", 1024, false),
            ("route_owners", 1024, false),
            ("route_owner_receipts", 1024, false),
            ("attempt_final_reservations", 1024, false),
            // postgres_attempt_phases refuses phases 2-3, 6 and 7-8 at these.
            ("attempt_phase_receipts", 2048, false),
            ("attempt_rcpt_receipts", 1024, false),
            ("attempt_command_receipts", 1024, false),
            ("attempt_acceptance_receipts", 1024, true),
            ("attempt_refusal_receipts", 1024, true),
            ("attempt_recovery_receipts", 1024, true),
            ("submission_expiry_receipts", 1024, true),
        ] {
            let used = match table {
                "route_health" => health_states,
                "route_health_receipts" => health_receipts,
                "active_devices" => {
                    let n = i64::try_from(cap.checked_add(1).ok_or(())?).map_err(|_| ())?;
                    let row = client.query_one("SELECT count(*) FROM (SELECT 1 FROM td_mta.devices WHERE account=$1 AND revoked_at IS NULL LIMIT $2) bounded", &[&id, &n]).map_err(|_| ())?;
                    let value: i64 = row.try_get(0).map_err(|_| ())?;
                    u64::try_from(value).map_err(|_| ())?
                }
                "devices" => live(client, id, "SELECT count(*) FROM (SELECT 1 FROM td_mta.devices WHERE account=$1 AND (revoked_at IS NULL OR revoked_at>=(extract(epoch FROM clock_timestamp())*1000)::bigint-$2) LIMIT $3) bounded", crate::postgres_devices::REVOKED_RETENTION_MS, cap)?,
                "submission_expiry_receipts" => live(client, id, "SELECT count(*) FROM (SELECT 1 FROM td_mta.submission_expiry_receipts WHERE account=$1 AND published_at>=(extract(epoch FROM clock_timestamp())*1000)::bigint-$2 LIMIT $3) bounded", crate::postgres_queue_expiry::RECEIPT_RETENTION_MS, cap)?,
                "route_owner_receipts" => live(client, id, "SELECT count(*) FROM (SELECT 1 FROM td_mta.route_owner_receipts WHERE account=$1 AND expires_at>=(extract(epoch FROM clock_timestamp())*1000)::bigint-$2 LIMIT $3) bounded", crate::postgres_route_owners::RECEIPT_RETENTION_MS, cap)?,
                _ => match crate::postgres_receipt_expiry::live(table) {
                    Some(query) => live(client, id, query, crate::postgres_receipt_expiry::RETENTION_MS, cap)?,
                    None => count(client, table, Some(id), cap)?,
                },
            };
            rows.push(metric(
                &scope,
                table,
                Some(used + if reserved { claims } else { 0 }),
                cap,
                "capacity",
            ));
        }
    }
    for (table, cap) in [
        (
            "config_selection_receipts",
            postgres_selection::MAX_RECEIPTS as u64,
        ),
        ("application_logs", crate::postgres_logs::MAX_ROWS as u64),
    ] {
        rows.push(metric(
            "database",
            table,
            Some(count(client, table, None, cap)?),
            cap,
            if table == "application_logs" {
                "rotation"
            } else {
                "capacity"
            },
        ));
    }
    let row = client.query_one("SELECT (pg_total_relation_size('td_mta.application_logs')+pg_total_relation_size('td_mta.application_log_state')), (SELECT count(*) FROM pg_stat_activity WHERE backend_type='client backend'), current_setting('max_connections')::bigint, current_setting('superuser_reserved_connections')::bigint+current_setting('reserved_connections')::bigint",&[]).map_err(|_| ())?;
    let value = |i| -> Result<u64, ()> {
        let n: i64 = row.try_get(i).map_err(|_| ())?;
        u64::try_from(n).map_err(|_| ())
    };
    rows.push(metric(
        "database",
        "log_relation_bytes",
        Some(value(0)?),
        crate::postgres_logs::MAX_RELATION_BYTES as u64,
        "capacity",
    ));
    rows.push(metric(
        "database",
        "client_connections",
        Some(value(1)?),
        value(2)?.checked_sub(value(3)?).ok_or(())?,
        "capacity",
    ));
    rows.push(metric(
        "database",
        "log_retention_ms",
        None,
        crate::postgres_logs::RETENTION_MS as u64,
        "setting",
    ));
    Ok(rows)
}
