//! Primary service logins. The grant plan and the cold catalog check use the
//! same inventory, including individual columns needed only for row locks.
use super::{Error, TABLES};
use crate::postgres_probe::Client;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServiceRole {
    Frontend,
    Receive,
    Relay,
    Migadu,
    Acme,
}

impl ServiceRole {
    pub const ALL: &'static [Self] = &[
        Self::Frontend,
        Self::Receive,
        Self::Relay,
        Self::Migadu,
        Self::Acme,
    ];

    pub fn login(self) -> &'static str {
        match self {
            Self::Frontend => "td_mta_frontend",
            Self::Receive => "td_mta_receive",
            Self::Relay => "td_mta_relay",
            Self::Migadu => "td_mta_migadu",
            Self::Acme => "td_mta_acme",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Frontend => "frontend",
            Self::Receive => "receive",
            Self::Relay => "relay",
            Self::Migadu => "migadu",
            Self::Acme => "acme",
        }
    }

    pub fn for_login(login: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|role| role.login() == login)
    }

    pub fn grants(self) -> impl Iterator<Item = (&'static str, &'static str)> {
        TABLES
            .iter()
            .filter_map(move |(table, _)| Some((*table, self.grant(table)?)))
    }

    fn grant(self, table: &str) -> Option<&'static str> {
        if matches!(
            table,
            "schema_migrations" | "runtime_barrier" | "replica_source"
        ) {
            return Some("SELECT");
        }
        if self == Self::Acme {
            return (table == "acme_profiles").then_some("SELECT, INSERT, UPDATE (owner,lease_until,revision,state,active_generation,chain,key)");
        }
        let grant = match table {
            "config_versions" | "config_version_chunks" | "account_bootstraps" => "SELECT",
            "config_selection" => "SELECT, UPDATE (singleton)",
            "accounts" => "SELECT, UPDATE (sequence)",
            "account_usage" => {
                "SELECT, UPDATE (body_bytes,blob_count,upload_bytes,queue_bytes,queue_submissions)"
            }
            "application_log_state" => "SELECT, UPDATE (next_id,window_ms,used,suppressed,evicted)",
            "application_logs" => "SELECT, INSERT, UPDATE (id,recorded_ms,profile,event), DELETE",
            "blob_ids" => "INSERT",
            "changes" if self == Self::Receive => "SELECT (account,kind,object), INSERT",
            "changes" => "SELECT, INSERT",
            _ => match self {
                Self::Frontend => frontend(table)?,
                Self::Receive => receive(table)?,
                Self::Relay | Self::Migadu => relay(table)?,
                Self::Acme => return None,
            },
        };
        Some(grant)
    }
}

fn receive(table: &str) -> Option<&'static str> {
    Some(match table {
        "blob_chunks"
        | "blobs"
        | "blob_summaries"
        | "blob_interpretations"
        | "smtp_receipt_recipients"
        | "email_authentication"
        | "email_dkim_signatures"
        | "memberships" => "INSERT",
        "emails" => "SELECT (account,id,thread_id,changed), INSERT",
        "threads" => "SELECT, INSERT, UPDATE (changed)",
        "thread_anchors" => "SELECT, INSERT",
        "mailboxes" => "SELECT (account,id,role,changed), UPDATE (changed)",
        "attempt_final_reservations" => "SELECT (account)",
        _ => return None,
    })
}

fn frontend(table: &str) -> Option<&'static str> {
    Some(match table {
        "devices" => "SELECT (account,id,salt,verifier,revoked_at)",
        "blob_search"
        | "smtp_receipt_recipients"
        | "email_authentication"
        | "email_dkim_signatures" => "SELECT",
        "blob_chunks" | "blob_summaries" | "blob_interpretations" | "submission_routes" => "SELECT, INSERT",
        "mailboxes" | "emails" | "threads" => "SELECT, INSERT, UPDATE (changed), DELETE",
        "blobs" | "memberships" | "thread_anchors" | "keywords" | "leases"
        | "flag_receipts" | "upload_receipts" | "draft_receipts" | "mailbox_create_receipts"
        | "submission_receipts" | "submission_cancel_receipts" => "SELECT, INSERT, DELETE",
        "submissions" => "SELECT, INSERT, UPDATE (changed,completed_at)",
        "recipients" => "SELECT, INSERT, UPDATE (state,uncertain,phase,next_attempt_at,reason,changed)",
        "submission_attempts" => "SELECT (account,submission_id,active,route_name,destination,generation,owner_boot,owner_operation,attempt_id), UPDATE (active)",
        "route_owners" => "SELECT (account,route_name,destination,generation,owner_boot,operation)",
        "attempt_final_reservations" => "SELECT (account)",
        _ => return None,
    })
}

fn relay(table: &str) -> Option<&'static str> {
    Some(match table {
        "blobs" => "SELECT, INSERT, DELETE",
        "blob_chunks" | "threads" => "SELECT, INSERT",
        "blob_summaries" | "blob_interpretations" | "memberships" | "thread_anchors" => "INSERT",
        "emails" => "SELECT (account,id,blob_id), INSERT",
        "mailboxes" => "SELECT (account,id,role,changed), UPDATE (changed)",
        "leases" => "SELECT, DELETE",
        "submissions" => "SELECT, UPDATE (changed,completed_at,notification,notification_email_id), DELETE",
        "recipients" => "SELECT, UPDATE (state,uncertain,attempt_id,attempt_count,last_attempt_at,phase,next_attempt_at,rcpt_reply,data_reply,reason,diagnostic,changed), DELETE",
        "submission_routes" => "SELECT",
        "submission_receipts" | "submission_cancel_receipts" => {
            "SELECT (account,submission_id), DELETE"
        }
        "route_owners" => "SELECT, INSERT, UPDATE (destination,generation,owner_boot,operation,last_clock,expires_at), DELETE",
        "route_health" => "SELECT, INSERT, UPDATE (revision,failure_count,reason,failed_at,next_at,repair_generation), DELETE",
        "submission_attempts" => "SELECT, INSERT, UPDATE (active), DELETE",
        "route_owner_receipts"
        | "route_health_receipts"
        | "attempt_acceptance_receipts"
        | "attempt_refusal_receipts"
        | "attempt_command_receipts"
        | "attempt_rcpt_receipts"
        | "attempt_recovery_receipts"
        | "attempt_phase_receipts"
        | "submission_expiry_receipts" => "SELECT, INSERT, DELETE",
        // FOR UPDATE while consuming reservations requires an updatable column.
        "attempt_final_reservations" => "SELECT, INSERT, UPDATE (ordinals), DELETE",
        "queue_expiry_progress" => "SELECT, UPDATE (published_at)",
        _ => return None,
    })
}

/// The role check already ruled out memberships, ownership and superuser.
/// Inspect the remaining sources of effective grants (self and PUBLIC).
const PRIVILEGES: &str = "SELECT c.relname::text,
        c.relkind='r' AND c.relpersistence='p' AND NOT c.relrowsecurity
        AND NOT c.relforcerowsecurity AND c.relowner=n.nspowner,
        acl.privilege_type,acl.col,
        acl.grantee=r.oid AND NOT acl.is_grantable AND acl.grantor=n.nspowner
    FROM pg_catalog.pg_namespace n JOIN pg_catalog.pg_class c ON c.relnamespace=n.oid
    JOIN pg_catalog.pg_roles r ON r.rolname=current_user
    LEFT JOIN LATERAL (
        SELECT a.*,NULL::text AS col FROM pg_catalog.aclexplode(c.relacl) a
        WHERE a.grantee IN (0,r.oid)
        UNION ALL
        SELECT a.*,att.attname::text FROM pg_catalog.pg_attribute att
        CROSS JOIN LATERAL pg_catalog.aclexplode(att.attacl) a
        WHERE att.attrelid=c.oid AND NOT att.attisdropped
        AND a.grantee IN (0,r.oid)
    ) acl ON true
    WHERE n.nspname='td_mta' AND c.relkind IN ('r','p','v','m','f','S')
    LIMIT 1025";

/// Each returned item is one PostgreSQL table privilege or one column privilege.
fn entries(grants: &str) -> impl Iterator<Item = (&str, Option<&str>)> {
    grants.split(", ").flat_map(|clause| {
        let (verb, columns) = clause.split_once(" (").unwrap_or((clause, ""));
        columns
            .trim_end_matches(')')
            .split(',')
            .map(move |column| (verb, (!column.is_empty()).then_some(column)))
    })
}

pub(super) fn qualify(client: &Client, role: ServiceRole) -> Result<(), Error> {
    let rows = client.query(PRIVILEGES, &[]).map_err(|_| Error::Database)?;
    if rows.len() > 1024 {
        return Err(Error::Privileges);
    }
    let mut counts = [0usize; TABLES.len()];
    for row in &rows {
        let name: &str = row.try_get(0).map_err(|_| Error::Database)?;
        let i = TABLES
            .iter()
            .position(|(table, _)| *table == name)
            .ok_or(Error::Privileges)?;
        if !row.try_get::<_, bool>(1).map_err(|_| Error::Database)? {
            return Err(Error::Privileges);
        }
        let privilege: Option<&str> = row.try_get(2).map_err(|_| Error::Database)?;
        let column: Option<&str> = row.try_get(3).map_err(|_| Error::Database)?;
        match (role.grant(name), privilege) {
            (Some(grants), Some(verb))
                if entries(grants).any(|entry| entry == (verb, column))
                    && row.try_get::<_, bool>(4).map_err(|_| Error::Database)? => {}
            (None, None) => {}
            _ => return Err(Error::Privileges),
        }
        *counts.get_mut(i).ok_or(Error::Privileges)? += 1;
    }
    for ((table, _), actual) in TABLES.iter().zip(counts) {
        let expected = role
            .grant(table)
            .map_or(1, |grants| entries(grants).count());
        if actual != expected {
            return Err(Error::Privileges);
        }
    }
    crate::postgres_service_materials::qualify_views(client, role)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_primary_logins_have_distinct_authority() {
        for &role in ServiceRole::ALL {
            assert_eq!(ServiceRole::for_login(role.login()), Some(role));
            for (table, grants) in role.grants() {
                assert!(!matches!(
                    table,
                    "config_materials" | "config_material_chunks"
                ));
                assert!(!grants.is_empty());
            }
            assert_eq!(
                role.grant("acme_profiles").is_some(),
                role == ServiceRole::Acme
            );
        }
        assert_eq!(ServiceRole::for_login("td_mta_relay_other"), None);
        let receiver = ServiceRole::Receive;
        assert_eq!(receiver.grant("devices"), None);
        assert_eq!(receiver.grant("submissions"), None);
        assert_eq!(receiver.grant("blob_chunks"), Some("INSERT"));
        assert_eq!(ServiceRole::Acme.grants().count(), 4);
        assert_eq!(ServiceRole::Relay.grant("devices"), None);
        assert!(ServiceRole::Relay.grants().eq(ServiceRole::Migadu.grants()));
    }

    #[test]
    fn catalog_columns_do_not_qualify_as_a_table_grant() {
        let grants = "SELECT (account,id), INSERT, UPDATE (changed)";
        assert!(entries(grants).eq([
            ("SELECT", Some("account")),
            ("SELECT", Some("id")),
            ("INSERT", None),
            ("UPDATE", Some("changed")),
        ]));
        assert!(!entries(grants).any(|entry| entry == ("SELECT", None)));
    }
}
