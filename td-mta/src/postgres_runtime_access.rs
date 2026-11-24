//! Cold runtime-role qualification; no grants, DDL or serving admission.
mod service;
pub use service::ServiceRole;

use crate::postgres_probe::Client;
use crate::{
    postgres_probe::{self, Request},
    postgres_schema::{self, Receipt},
};
use std::time::Instant;

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
        WHERE c.relnamespace=n.oid AND c.relowner=r.oid),current_user::text
    FROM pg_catalog.pg_roles r JOIN pg_catalog.pg_namespace n ON n.nspname='td_mta'
    JOIN pg_catalog.pg_database d ON d.datname=current_database()
    WHERE r.rolname=current_user";

const TABLES: &[(&str, Access)] = &[
    ("schema_migrations", Access::Read),
    ("runtime_barrier", Access::Read),
    ("replica_source", Access::Read),
    ("replica_accounts", Access::Read),
    ("replica_email_receipts", Access::Replica),
    ("replica_blob_identities", Access::Replica),
    ("replica_progress", Access::Replica),
    ("replica_pending", Access::Replica),
    ("replica_mailbox_receipts", Access::Replica),
    ("config_materials", Access::Read),
    ("config_material_chunks", Access::Read),
    ("config_versions", Access::Read),
    ("config_version_chunks", Access::Read),
    ("config_selection", Access::Selection),
    ("config_selection_receipts", Access::Read),
    ("account_bootstraps", Access::Read),
    ("accounts", Access::Mail),
    ("application_log_state", Access::Mail),
    ("application_logs", Access::Mail),
    ("acme_profiles", Access::Mail),
    ("blob_ids", Access::Mail),
    ("blobs", Access::Mail),
    ("blob_chunks", Access::Mail),
    ("blob_summaries", Access::Mail),
    ("blob_interpretations", Access::Mail),
    ("blob_search", Access::Mail),
    ("mailboxes", Access::Mail),
    ("threads", Access::Mail),
    ("emails", Access::Mail),
    ("smtp_receipt_recipients", Access::Mail),
    ("email_authentication", Access::Mail),
    ("email_dkim_signatures", Access::Mail),
    ("memberships", Access::Mail),
    ("keywords", Access::Mail),
    ("thread_anchors", Access::Mail),
    ("submissions", Access::Mail),
    ("recipients", Access::Mail),
    ("leases", Access::Lease),
    ("imports", Access::Read),
    ("changes", Access::Mail),
    ("devices", Access::Mail),
    ("flag_receipts", Access::Mail),
    ("account_usage", Access::Mail),
    ("upload_receipts", Access::Mail),
    ("draft_receipts", Access::Mail),
    ("mailbox_create_receipts", Access::Mail),
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
    Replica,
    Read,
    Lease,
    Selection,
    Mail,
}

/// The application role on a serving primary. Replica-specific roles have
/// separate contracts and are never selected by the production deployer.
pub fn primary_grants() -> impl Iterator<Item = (&'static str, &'static str)> {
    TABLES.iter().map(|(table, access)| {
        let privileges = match access {
            Access::Read | Access::Replica => "SELECT",
            Access::Lease => "SELECT, INSERT, DELETE",
            Access::Selection => "SELECT, UPDATE (singleton)",
            Access::Mail => "SELECT, INSERT, UPDATE, DELETE",
        };
        (*table, privileges)
    })
}

/// What a replica's shared logins hold: the publisher (`reader` false)
/// writes replica progress and mail, the reader reads, with the few writes
/// serving needs. Exactly what `qualify_replica` and
/// `qualify_replica_reader` accept, table by table.
pub fn replica_grants(reader: bool) -> impl Iterator<Item = (&'static str, &'static str)> {
    TABLES.iter().map(move |(table, access)| {
        let privileges = match (access, reader) {
            (Access::Read, _) | (Access::Replica, true) => "SELECT",
            (Access::Replica | Access::Mail, false) => "SELECT, INSERT, UPDATE, DELETE",
            (Access::Lease, false) => "SELECT, INSERT, DELETE",
            (Access::Lease, true) => "SELECT, INSERT",
            (Access::Selection, _) => "SELECT, UPDATE (singleton)",
            (Access::Mail, true) => match *table {
                "accounts" => "SELECT, UPDATE (id)",
                "application_log_state" | "application_logs" => "SELECT, INSERT, UPDATE, DELETE",
                _ => "SELECT",
            },
        };
        (*table, privileges)
    })
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
        AND NOT pg_catalog.has_table_privilege(c.oid,'DELETE'),
    CASE WHEN c.relname='accounts' THEN
        pg_catalog.has_column_privilege(c.oid,'id','UPDATE')
        AND NOT pg_catalog.has_any_column_privilege(c.oid,'INSERT')
        AND NOT pg_catalog.has_table_privilege(c.oid,'DELETE')
        AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_attribute a
            WHERE a.attrelid=c.oid AND a.attnum>0 AND NOT a.attisdropped
            AND a.attname<>'id'
            AND pg_catalog.has_column_privilege(c.oid,a.attnum,'UPDATE'))
        ELSE false END,
    pg_catalog.has_table_privilege(c.oid,'INSERT')
        AND pg_catalog.has_table_privilege(c.oid,'DELETE')
        AND NOT pg_catalog.has_any_column_privilege(c.oid,'UPDATE')
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
    Replica,
    Database,
}

/// Observe one dedicated login's privileges on a qualified primary. This
/// receipt closes with its connection and authorizes no later operation.
pub fn inspect(request: &Request<'_>, deadline: Instant) -> Result<Receipt, Error> {
    let result = postgres_probe::with_mailbox_client(request, deadline, |client| {
        let receipt = client.inspect_schema().map_err(Error::Schema)?;
        qualify(client)?;
        Ok(receipt)
    })
    .map_err(Error::Connection)?;
    if Instant::now() >= deadline {
        return Err(Error::Connection(postgres_probe::Error::Deadline));
    }
    result
}

pub(crate) fn qualify(client: &Client) -> Result<(), Error> {
    qualify_role(client, Authority::Primary)?;
    if crate::postgres_replica_source::read(client)
        .map_err(|_| Error::Database)?
        .is_some()
    {
        return Err(Error::Replica);
    }
    Ok(())
}

pub(crate) fn qualify_replica(
    client: &Client,
    source: &crate::postgres_replica_source::Source,
) -> Result<(), Error> {
    qualify_role(client, Authority::Publisher)?;
    let actual = crate::postgres_replica_source::read(client)
        .map_err(|_| Error::Database)?
        .ok_or(Error::Replica)?;
    if &actual != source {
        return Err(Error::Replica);
    }
    Ok(())
}

pub(crate) fn qualify_replica_reader(
    client: &Client,
    epoch: crate::ids::StoreEpoch,
) -> Result<(), Error> {
    qualify_role(client, Authority::Reader)?;
    let source = crate::postgres_replica_source::read(client)
        .map_err(|_| Error::Database)?
        .ok_or(Error::Replica)?;
    if source.epoch() == epoch {
        return Err(Error::Replica);
    }
    Ok(())
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Authority {
    Primary,
    Publisher,
    Reader,
}

fn qualify_role(client: &Client, authority: Authority) -> Result<(), Error> {
    let row = client.query_one(ROLE, &[]).map_err(|_| Error::Database)?;
    if !row.try_get::<_, bool>(0).map_err(|_| Error::Database)? {
        return Err(Error::Role);
    }
    let user: &str = row.try_get(1).map_err(|_| Error::Database)?;
    if let Some(role) = ServiceRole::for_login(user) {
        if authority != Authority::Primary {
            return Err(Error::Role);
        }
        service::qualify(client, role)?;
    } else {
        qualify_shared(client, authority)?;
    }
    let rows = client
        .query(
            "SELECT singleton,blocked FROM td_mta.runtime_barrier LIMIT 2",
            &[],
        )
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

fn qualify_shared(client: &Client, authority: Authority) -> Result<(), Error> {
    let limit = i64::try_from(TABLES.len() + 1).map_err(|_| Error::Privileges)?;
    let rows = client
        .query(PRIVILEGES, &[&limit])
        .map_err(|_| Error::Database)?;
    if rows.len() != TABLES.len() {
        return Err(Error::Privileges);
    }
    for row in &rows {
        let name: &str = row.try_get(0).map_err(|_| Error::Database)?;
        let access = TABLES
            .iter()
            .find(|(table, _)| *table == name)
            .map(|(_, access)| access)
            .ok_or(Error::Privileges)?;
        let value = |index| row.try_get::<_, bool>(index).map_err(|_| Error::Database);
        if !allowed(authority, *access, name, value)? {
            return Err(Error::Privileges);
        }
    }
    Ok(())
}

/// Whether one table's `PRIVILEGES` row, read column by column through
/// `value`, is what `authority` may hold.
fn allowed(
    authority: Authority,
    access: Access,
    name: &str,
    value: impl Fn(usize) -> Result<bool, Error>,
) -> Result<bool, Error> {
    if !value(1)? || !value(2)? || value(5)? {
        return Ok(false);
    }
    Ok(match access {
        Access::Read => !value(4)?,
        Access::Replica => {
            if authority == Authority::Publisher {
                value(3)?
            } else {
                !value(4)?
            }
        }
        // Writers reclaim expired upload leases; none may extend one.
        Access::Lease if authority == Authority::Reader => value(7)?,
        Access::Lease => value(9)?,
        Access::Selection => value(6)?,
        Access::Mail if authority == Authority::Reader => match name {
            "accounts" => value(8)?,
            "application_log_state" | "application_logs" => value(3)?,
            _ => !value(4)?,
        },
        Access::Mail => value(3)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `PRIVILEGES` columns of `name` for a login holding exactly
    /// `grants` there, as PostgreSQL computes them.
    fn columns(name: &str, grants: &str) -> [bool; 10] {
        let held: Vec<&str> = grants.split(", ").collect();
        let has = |p: &str| held.contains(&p);
        let column = held.iter().find_map(|p| p.strip_prefix("UPDATE ("));
        let (insert, update, delete) = (has("INSERT"), has("UPDATE"), has("DELETE"));
        let any_update = update || column.is_some();
        let only = |c: &str| {
            column.and_then(|c| c.strip_suffix(')')) == Some(c) && !update && !insert && !delete
        };
        [
            true,
            true,
            has("SELECT"),
            insert && update && delete,
            insert || any_update || delete,
            false,
            name == "config_selection" && only("singleton"),
            insert && !any_update && !delete,
            name == "accounts" && only("id"),
            insert && delete && !any_update,
        ]
    }

    fn qualifies(authority: Authority, reader: bool) -> bool {
        replica_grants(reader).all(|(name, grants)| {
            let row = columns(name, grants);
            TABLES
                .iter()
                .find(|(table, _)| *table == name)
                .is_some_and(|(_, access)| {
                    let value = |i| row.get(i).copied().ok_or(Error::Database);
                    allowed(authority, *access, name, value) == Ok(true)
                })
        })
    }

    #[test]
    fn replica_grants_are_what_each_replica_login_qualifies_with() {
        assert_eq!(replica_grants(false).count(), TABLES.len());
        assert!(qualifies(Authority::Publisher, false));
        assert!(qualifies(Authority::Reader, true));
        // Each holds what the other may not.
        assert!(!qualifies(Authority::Reader, false));
        assert!(!qualifies(Authority::Publisher, true));
        // A serving primary's grants leave replica progress read-only.
        assert!(primary_grants().ne(replica_grants(false)));
    }
}
