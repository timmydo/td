//! Bounded explicit ACL preservation across an owner schema transaction.
use crate::postgres_schema::Error;
use tokio_postgres::Client;

const MAX_GRANTS: usize = 4096;
const ACLS: &str = "SELECT * FROM (
 SELECT 1::smallint AS kind,n.oid::bigint AS object,0::smallint AS column_number,
 a.grantor::bigint,a.grantee::bigint,a.privilege_type,a.is_grantable
 FROM pg_catalog.pg_namespace n,
 LATERAL pg_catalog.aclexplode(COALESCE(n.nspacl,pg_catalog.acldefault('n',n.nspowner))) a
 WHERE n.nspname IN ('td_mta','td_mta_trgm') AND a.grantee<>n.nspowner
 UNION ALL
 SELECT 2::smallint,c.oid::bigint,0::smallint,
 a.grantor::bigint,a.grantee::bigint,a.privilege_type,a.is_grantable
 FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace,
 LATERAL pg_catalog.aclexplode(COALESCE(c.relacl,
 pg_catalog.acldefault(CASE WHEN c.relkind='S' THEN 's'::\"char\" ELSE 'r'::\"char\" END,c.relowner))) a
 WHERE n.nspname IN ('td_mta','td_mta_trgm') AND a.grantee<>c.relowner
 UNION ALL
 SELECT 3::smallint,c.oid::bigint,p.attnum,
 a.grantor::bigint,a.grantee::bigint,a.privilege_type,a.is_grantable
 FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace
 JOIN pg_catalog.pg_attribute p ON p.attrelid=c.oid,
 LATERAL pg_catalog.aclexplode(COALESCE(p.attacl,pg_catalog.acldefault('c',c.relowner))) a
 WHERE n.nspname IN ('td_mta','td_mta_trgm') AND p.attnum>0 AND NOT p.attisdropped AND a.grantee<>c.relowner
) grants LIMIT 4097";

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) struct Grant {
    kind: i16,
    object: i64,
    column: i16,
    grantor: i64,
    grantee: i64,
    privilege: u8,
    grantable: bool,
}

pub(super) async fn owner(client: &Client) -> Result<(), Error> {
    let owner: bool = client.query_one("SELECT current_user=session_user AND EXISTS (SELECT 1 FROM pg_catalog.pg_namespace n JOIN pg_catalog.pg_roles r ON r.rolname=current_user JOIN pg_catalog.pg_class c ON c.relnamespace=n.oid AND c.relname='schema_migrations' WHERE n.nspname='td_mta' AND n.nspowner=r.oid AND c.relowner=r.oid AND c.relkind='r' AND c.relpersistence='p' AND NOT c.relrowsecurity AND NOT c.relforcerowsecurity)", &[])
        .await.map_err(|_| Error::Database)?.try_get(0).map_err(|_| Error::Invalid)?;
    if owner {
        Ok(())
    } else {
        Err(Error::Owner)
    }
}

pub(super) async fn read(client: &Client) -> Result<Vec<Grant>, Error> {
    let rows = client.query(ACLS, &[]).await.map_err(|_| Error::Database)?;
    if rows.len() > MAX_GRANTS {
        return Err(Error::Invalid);
    }
    let mut grants = Vec::new();
    grants
        .try_reserve_exact(rows.len())
        .map_err(|_| Error::Invalid)?;
    for row in rows {
        let privilege: &str = row.try_get(5).map_err(|_| Error::Invalid)?;
        let privilege = match privilege {
            "USAGE" => 1,
            "CREATE" => 2,
            "SELECT" => 3,
            "INSERT" => 4,
            "UPDATE" => 5,
            "DELETE" => 6,
            "TRUNCATE" => 7,
            "REFERENCES" => 8,
            "TRIGGER" => 9,
            "MAINTAIN" => 10,
            _ => return Err(Error::Invalid),
        };
        let grant = Grant {
            kind: row.try_get(0).map_err(|_| Error::Invalid)?,
            object: row.try_get(1).map_err(|_| Error::Invalid)?,
            column: row.try_get(2).map_err(|_| Error::Invalid)?,
            grantor: row.try_get(3).map_err(|_| Error::Invalid)?,
            grantee: row.try_get(4).map_err(|_| Error::Invalid)?,
            privilege,
            grantable: row.try_get(6).map_err(|_| Error::Invalid)?,
        };
        if grant.grantee == 0 {
            return Err(Error::Invalid);
        }
        grants.push(grant);
    }
    grants.sort_unstable();
    grants.dedup();
    Ok(grants)
}

pub(super) fn preserves(before: &[Grant], after: &[Grant]) -> bool {
    after.iter().all(|grant| {
        before.binary_search(grant).is_ok()
            || (!grant.grantable && {
                let mut delegated = *grant;
                delegated.grantable = true;
                before.binary_search(&delegated).is_ok()
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preservation_binds_objects_columns_delegation_and_privileges() {
        let original = Grant {
            kind: 2,
            object: 20,
            column: 0,
            grantor: 10,
            grantee: 11,
            privilege: 3,
            grantable: false,
        };
        assert!(preserves(&[original], &[original]));
        assert!(preserves(&[original], &[]));
        for changed in [
            Grant {
                kind: 3,
                ..original
            },
            Grant {
                object: 21,
                ..original
            },
            Grant {
                column: 1,
                ..original
            },
            Grant {
                grantor: 12,
                ..original
            },
            Grant {
                grantee: 12,
                ..original
            },
            Grant {
                privilege: 4,
                ..original
            },
            Grant {
                grantable: true,
                ..original
            },
        ] {
            assert!(!preserves(&[original], &[changed]));
        }
        assert!(preserves(
            &[Grant {
                grantable: true,
                ..original
            }],
            &[original]
        ));
    }
}
