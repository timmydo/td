//! Cold runtime-role qualification; no grants, DDL or serving admission.
use crate::{
    postgres_probe::{self, Request},
    postgres_schema::{self, Receipt},
};
use std::time::Instant;
use td_crypto::TrustStore;
use tokio_postgres::Client;

// Dedicated runtime logins have no role memberships. Owner-side configuration
// and schema commands use a separate connection specification.
const ROLE: &str = "SELECT current_user=session_user AND r.rolcanlogin
    AND NOT (r.rolsuper OR r.rolcreatedb OR r.rolcreaterole
        OR r.rolreplication OR r.rolbypassrls)
    AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_auth_members m WHERE m.member=r.oid)
    AND d.datdba<>r.oid
    AND NOT pg_catalog.has_database_privilege(current_database(),'CREATE')
    AND pg_catalog.has_schema_privilege(n.oid,'USAGE')
    AND NOT pg_catalog.has_schema_privilege(n.oid,'CREATE,USAGE WITH GRANT OPTION')
    AND n.nspowner<>r.oid
    AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_class c
        WHERE c.relnamespace=n.oid AND c.relowner=r.oid)
    FROM pg_catalog.pg_roles r JOIN pg_catalog.pg_namespace n ON n.nspname='td_mta'
    JOIN pg_catalog.pg_database d ON d.datname=current_database()
    WHERE r.rolname=current_user";

const TABLES: &[(&str, Access)] = &[
    ("schema_migrations", Access::Read),
    ("runtime_barrier", Access::Read),
    ("config_materials", Access::Read),
    ("config_material_chunks", Access::Read),
    ("config_versions", Access::Read),
    ("config_version_chunks", Access::Read),
    ("config_selection", Access::Selection),
    ("config_selection_receipts", Access::Read),
    ("account_bootstraps", Access::Read),
    ("accounts", Access::Mail),
    ("blob_ids", Access::Mail),
    ("blobs", Access::Mail),
    ("blob_chunks", Access::Mail),
    ("mailboxes", Access::Mail),
    ("threads", Access::Mail),
    ("emails", Access::Mail),
    ("smtp_receipt_recipients", Access::Mail),
    ("memberships", Access::Mail),
    ("keywords", Access::Mail),
    ("thread_anchors", Access::Mail),
    ("submissions", Access::Mail),
    ("recipients", Access::Mail),
    ("leases", Access::Insert),
    ("imports", Access::Read),
    ("changes", Access::Mail),
    ("devices", Access::Mail),
    ("flag_receipts", Access::Mail),
    ("account_usage", Access::Mail),
    ("upload_receipts", Access::Mail),
    ("draft_receipts", Access::Mail),
    ("mailbox_create_receipts", Access::Mail),
    ("receiving_receipts", Access::Mail),
    ("submission_receipts", Access::Mail),
    ("submission_routes", Access::Mail),
    ("route_owners", Access::Mail),
    ("route_owner_receipts", Access::Mail),
    ("submission_attempts", Access::Mail),
    ("attempt_recovery_receipts", Access::Mail),
    ("attempt_phase_receipts", Access::Mail),
    ("attempt_acceptance_receipts", Access::Mail),
    ("attempt_refusal_receipts", Access::Mail),
    ("attempt_rcpt_receipts", Access::Mail),
    ("attempt_command_receipts", Access::Mail),
    ("submission_expiry_receipts", Access::Mail),
    ("queue_expiry_progress", Access::Mail),
    ("route_health", Access::Mail),
    ("route_health_receipts", Access::Mail),
    ("submission_cancel_receipts", Access::Mail),
    ("attempt_final_reservations", Access::Mail),
];

#[derive(Clone, Copy)]
enum Access {
    Read,
    Insert,
    Selection,
    Mail,
}

const PRIVILEGES: &str = "SELECT c.relname::text,
    c.relkind='r' AND c.relpersistence='p' AND NOT c.relrowsecurity
        AND NOT c.relforcerowsecurity,
    pg_catalog.has_table_privilege(c.oid,'SELECT'),
    pg_catalog.has_table_privilege(c.oid,'INSERT')
        AND pg_catalog.has_table_privilege(c.oid,'UPDATE')
        AND pg_catalog.has_table_privilege(c.oid,'DELETE'),
    pg_catalog.has_any_column_privilege(c.oid,'INSERT,UPDATE')
        OR pg_catalog.has_table_privilege(c.oid,'DELETE'),
    pg_catalog.has_table_privilege(c.oid,'TRUNCATE,REFERENCES,TRIGGER,MAINTAIN,
        SELECT WITH GRANT OPTION,INSERT WITH GRANT OPTION,
        UPDATE WITH GRANT OPTION,DELETE WITH GRANT OPTION')
        OR pg_catalog.has_any_column_privilege(c.oid,'REFERENCES,
        SELECT WITH GRANT OPTION,INSERT WITH GRANT OPTION,UPDATE WITH GRANT OPTION'),
    CASE WHEN c.relname='config_selection' THEN
        pg_catalog.has_column_privilege(c.oid,'singleton','UPDATE')
        AND NOT pg_catalog.has_any_column_privilege(c.oid,'INSERT')
        AND NOT pg_catalog.has_table_privilege(c.oid,'DELETE')
        AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_attribute a
            WHERE a.attrelid=c.oid AND a.attnum>0 AND NOT a.attisdropped
            AND a.attname<>'singleton'
            AND pg_catalog.has_column_privilege(c.oid,a.attnum,'UPDATE'))
        ELSE false END,
    pg_catalog.has_table_privilege(c.oid,'INSERT')
        AND NOT pg_catalog.has_any_column_privilege(c.oid,'UPDATE')
        AND NOT pg_catalog.has_table_privilege(c.oid,'DELETE')
    FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace
    WHERE n.nspname='td_mta' AND c.relkind IN ('r','p','v','m','f','S')
    ORDER BY c.relname LIMIT $1";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Connection(postgres_probe::Error),
    Schema(postgres_schema::Error),
    Role,
    Privileges,
    Maintenance,
    Database,
}

/// Observe one dedicated login's privileges on a qualified primary. This
/// receipt closes with its connection and authorizes no later operation.
pub fn inspect(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
) -> Result<Receipt, Error> {
    let result = postgres_probe::with_mailbox_client(request, trust, deadline, |client| {
        Box::pin(async move {
            let receipt = postgres_schema::lock_and_inspect(client)
                .await
                .map_err(Error::Schema)?;
            qualify(client).await?;
            Ok(receipt)
        })
    })
    .map_err(Error::Connection)?;
    if Instant::now() >= deadline {
        return Err(Error::Connection(postgres_probe::Error::Deadline));
    }
    result
}

pub(crate) async fn qualify(client: &Client) -> Result<(), Error> {
    let row = client
        .query_one(ROLE, &[])
        .await
        .map_err(|_| Error::Database)?;
    if !row.try_get::<_, bool>(0).map_err(|_| Error::Database)? {
        return Err(Error::Role);
    }
    let limit = i64::try_from(TABLES.len() + 1).map_err(|_| Error::Privileges)?;
    let rows = client
        .query(PRIVILEGES, &[&limit])
        .await
        .map_err(|_| Error::Database)?;
    if rows.len() != TABLES.len() {
        return Err(Error::Privileges);
    }
    for row in rows {
        let name: &str = row.try_get(0).map_err(|_| Error::Database)?;
        let access = TABLES
            .iter()
            .find(|(table, _)| *table == name)
            .map(|(_, access)| access)
            .ok_or(Error::Privileges)?;
        let value = |index| row.try_get::<_, bool>(index).map_err(|_| Error::Database);
        if !value(1)? || !value(2)? || value(5)? {
            return Err(Error::Privileges);
        }
        let allowed = match access {
            Access::Read => !value(4)?,
            Access::Insert => value(7)?,
            Access::Selection => value(6)?,
            Access::Mail => value(3)?,
        };
        if !allowed {
            return Err(Error::Privileges);
        }
    }
    let rows = client
        .query(
            "SELECT singleton,blocked FROM td_mta.runtime_barrier LIMIT 2",
            &[],
        )
        .await
        .map_err(|_| Error::Database)?;
    let row = rows.first().ok_or(Error::Maintenance)?;
    if rows.len() != 1
        || !row.try_get::<_, bool>(0).map_err(|_| Error::Database)?
        || row.try_get::<_, bool>(1).map_err(|_| Error::Database)?
    {
        return Err(Error::Maintenance);
    }
    Ok(())
}
