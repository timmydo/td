//! Owner-bound replica mode, separate from source authentication and copy progress.
use crate::postgres_probe::Client;
use crate::{
    bounded::TextBuffer,
    config::endpoint::{Origin, MAX_URI_BYTES},
    ids::StoreEpoch,
    postgres_maintenance,
    postgres_probe::{self, Mode, Request},
    postgres_schema,
};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Instant,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Connection(postgres_probe::Error),
    Schema(postgres_schema::Error),
    Owner,
    Epoch,
    Invalid,
    Capacity,
    Occupied,
    Conflict,
    Integrity,
    Database,
    Indeterminate,
    /// The database is not bound to a replica source.
    Unbound,
    /// The publisher and reader logins are not two distinct plain names
    /// other than the owner's.
    Logins,
    /// After its grants were replaced, a login still refuses to qualify,
    /// and why.
    Publisher(crate::postgres_runtime_access::Error),
    Reader(crate::postgres_runtime_access::Error),
}

/// Passive validated source identity, never a device or publication credential.
#[derive(Eq, PartialEq)]
pub struct Source {
    origin: String,
    epoch: StoreEpoch,
}
impl std::fmt::Debug for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ReplicaSource(<redacted>)")
    }
}
impl Source {
    pub fn new(origin: &str, epoch: StoreEpoch) -> Result<Self, Error> {
        let parsed = Origin::parse(origin).map_err(|_| Error::Invalid)?;
        let mut bytes = [0; MAX_URI_BYTES];
        let mut out = TextBuffer::new(&mut bytes);
        parsed
            .write_canonical(&mut out)
            .map_err(|_| Error::Capacity)?;
        let canonical = out.as_str().map_err(|_| Error::Invalid)?;
        let mut origin = String::new();
        origin
            .try_reserve_exact(canonical.len())
            .map_err(|_| Error::Capacity)?;
        origin.push_str(canonical);
        Ok(Self { origin, epoch })
    }
    pub fn origin(&self) -> &str {
        &self.origin
    }
    pub fn epoch(&self) -> StoreEpoch {
        self.epoch
    }
}

pub(crate) fn read(client: &Client) -> Result<Option<Source>, Error> {
    let rows = client
        .query(
            "SELECT singleton,origin,source_epoch,protocol FROM td_mta.replica_source LIMIT 2",
            &[],
        )
        .map_err(|_| Error::Database)?;
    if rows.len() > 1 {
        return Err(Error::Integrity);
    }
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    if !row.try_get::<_, bool>(0).map_err(|_| Error::Integrity)?
        || row.try_get::<_, i32>(3).map_err(|_| Error::Integrity)? != 1
    {
        return Err(Error::Integrity);
    }
    let origin: &str = row.try_get(1).map_err(|_| Error::Integrity)?;
    let epoch: &[u8] = row.try_get(2).map_err(|_| Error::Integrity)?;
    let source = Source::new(
        origin,
        StoreEpoch::from_bytes(epoch.try_into().map_err(|_| Error::Integrity)?),
    )
    .map_err(|_| Error::Integrity)?;
    if source.origin() != origin {
        return Err(Error::Integrity);
    }
    Ok(Some(source))
}

/// Historical mode observation. Serving must check this under its startup lock
/// before listeners; this result alone never authorizes a runtime profile.
pub fn inspect(
    request: &Request<'_>,
    deadline: Instant,
    epoch: StoreEpoch,
) -> Result<Option<Source>, Error> {
    if request.mode != Mode::Preparation {
        return Err(Error::Owner);
    }
    let result=postgres_probe::with_mutation_client(request,deadline,move |client| {
            let receipt=postgres_schema::lock_and_inspect(client).map_err(Error::Schema)?;
            if receipt.store_epoch!=epoch {return Err(Error::Epoch);}
            client.batch("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY; SET LOCAL idle_in_transaction_session_timeout=10000").map_err(|_|Error::Database)?;
            postgres_maintenance::owner(client).map_err(|_|Error::Owner)?;
            let source=read(client)?;
            client.rollback().map_err(|_| Error::Database)?;
            Ok(source)
        }).map_err(Error::Connection)?;
    if Instant::now() >= deadline {
        return Err(Error::Connection(postgres_probe::Error::Deadline));
    }
    result
}

/// Bind an empty, unselected database. The exclusive migration lock excludes
/// every admitted runtime/owner operation; matching retries never change mode.
pub fn initialize(
    request: &Request<'_>,
    deadline: Instant,
    local_epoch: StoreEpoch,
    source: &Source,
) -> Result<(), Error> {
    if request.mode != Mode::Preparation {
        return Err(Error::Owner);
    }
    if source.epoch() == local_epoch {
        return Err(Error::Epoch);
    }
    let source = Source::new(source.origin(), source.epoch())?;
    let attempted = Arc::new(AtomicBool::new(false));
    let marker = attempted.clone();
    let result = postgres_probe::with_mutation_client(request, deadline, move |client| {
        client.batch("BEGIN ISOLATION LEVEL READ COMMITTED; SET LOCAL synchronous_commit=on; SET LOCAL idle_in_transaction_session_timeout=10000").map_err(|_|Error::Database)?;
        let receipt = postgres_schema::lock_exclusive_transaction_and_inspect(client)
            .map_err(Error::Schema)?;
        if receipt.store_epoch != local_epoch {
            return Err(Error::Epoch);
        }
        postgres_maintenance::owner(client).map_err(|_| Error::Owner)?;
        if let Some(existing) = read(client)? {
            if existing != source {
                return Err(Error::Conflict);
            }
            client.rollback().map_err(|_| Error::Database)?;
            return Ok(());
        }
        let occupied:bool=client.query_one("SELECT EXISTS(SELECT 1 FROM td_mta.accounts) OR EXISTS(SELECT 1 FROM td_mta.config_selection WHERE version_id IS NOT NULL) OR EXISTS(SELECT 1 FROM td_mta.acme_profiles)",&[]).map_err(|_|Error::Database)?.try_get(0).map_err(|_|Error::Integrity)?;
        if occupied {
            return Err(Error::Occupied);
        }
        if client.execute("INSERT INTO td_mta.replica_source(singleton,origin,source_epoch,protocol) VALUES(true,$1,$2,1)",&[&source.origin(), &source.epoch().as_bytes().as_slice()]).map_err(|_|Error::Database)?!=1 {return Err(Error::Integrity);}
        marker.store(true, Ordering::Relaxed);
        client.commit().map_err(|_| Error::Indeterminate)?;
        Ok(())
    });
    match result {
        Ok(Ok(())) if !attempted.load(Ordering::Relaxed) && Instant::now() >= deadline => {
            Err(Error::Connection(postgres_probe::Error::Deadline))
        }
        Ok(result) => result,
        Err(_) if attempted.load(Ordering::Relaxed) => Err(Error::Indeterminate),
        Err(error) => Err(Error::Connection(error)),
    }
}

/// A plain lowercase login name, which `grant` quotes into its statements.
fn login(name: &str) -> bool {
    let bytes = name.as_bytes();
    (1..=63).contains(&bytes.len())
        && bytes
            .first()
            .is_some_and(|b| b.is_ascii_lowercase() || *b == b'_')
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_')
}

/// The statements that leave `publisher` and `reader` exactly their
/// replica access to the mail schema, whatever they held before.
fn grant_sql(publisher: &str, reader: &str) -> Result<String, Error> {
    let mut sql = td_alloc::format(format_args!(
        "REVOKE ALL ON ALL TABLES IN SCHEMA td_mta FROM \"{publisher}\", \"{reader}\";\n\
         REVOKE ALL ON SCHEMA td_mta FROM \"{publisher}\", \"{reader}\";\n\
         GRANT USAGE ON SCHEMA td_mta TO \"{publisher}\", \"{reader}\";\n"
    ))
    .map_err(|_| Error::Capacity)?;
    for (role, reading) in [(publisher, false), (reader, true)] {
        for (table, privileges) in crate::postgres_runtime_access::replica_grants(reading) {
            let line = td_alloc::format(format_args!(
                "GRANT {privileges} ON td_mta.{table} TO \"{role}\";\n"
            ))
            .map_err(|_| Error::Capacity)?;
            td_alloc::push_str(&mut sql, &line).map_err(|_| Error::Capacity)?;
        }
    }
    Ok(sql)
}

/// Give a bound replica's publisher and reader logins exactly the access
/// their qualifiers require, in one owner transaction, then qualify each
/// over its own login as the copier and the reader will. The grants are
/// replaced, not added to, so a rerun converges on the same set. Every
/// connection is a schema client: the batch is a statement per table and
/// login, and each qualification reads every table's privileges.
pub fn grant(
    owner: &Request<'_>,
    publisher: &Request<'_>,
    reader: &Request<'_>,
    deadline: Instant,
    epoch: StoreEpoch,
) -> Result<(), Error> {
    if !login(publisher.user)
        || !login(reader.user)
        || publisher.user == reader.user
        || owner.user == publisher.user
        || owner.user == reader.user
    {
        return Err(Error::Logins);
    }
    let sql = grant_sql(publisher.user, reader.user)?;
    let source = postgres_probe::with_schema_client(owner, deadline, |client| {
        client.batch("BEGIN ISOLATION LEVEL READ COMMITTED; SET LOCAL synchronous_commit=on; SET LOCAL idle_in_transaction_session_timeout=10000").map_err(|_| Error::Database)?;
        let receipt =
            postgres_schema::lock_transaction_and_inspect(client).map_err(Error::Schema)?;
        if receipt.store_epoch != epoch {
            return Err(Error::Epoch);
        }
        postgres_maintenance::owner(client).map_err(|_| Error::Owner)?;
        let source = read(client)?.ok_or(Error::Unbound)?;
        client.batch(&sql).map_err(|_| Error::Database)?;
        // Replacing grants twice leaves them as once: a rerun settles it.
        client.commit().map_err(|_| Error::Indeterminate)?;
        Ok(source)
    })
    .map_err(Error::Connection)??;
    postgres_probe::with_schema_client(publisher, deadline, |client| {
        let receipt = postgres_schema::lock_and_inspect(client).map_err(Error::Schema)?;
        if receipt.store_epoch != epoch {
            return Err(Error::Epoch);
        }
        crate::postgres_runtime_access::qualify_replica(client, &source).map_err(Error::Publisher)
    })
    .map_err(Error::Connection)??;
    postgres_probe::with_schema_client(reader, deadline, |client| {
        let receipt = postgres_schema::lock_and_inspect(client).map_err(Error::Schema)?;
        if receipt.store_epoch != epoch {
            return Err(Error::Epoch);
        }
        crate::postgres_runtime_access::qualify_replica_reader(client, epoch).map_err(Error::Reader)
    })
    .map_err(Error::Connection)??;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn grants_replace_each_login_and_quote_only_plain_names() {
        for name in ["td_mta_runtime", "_r", "mail_reader2"] {
            assert!(login(name));
        }
        for name in ["", "Td", "1r", "a-b", "a\"b", "a b", &"a".repeat(64)] {
            assert!(!login(name));
        }
        let sql = grant_sql("td_mta_runtime", "td_mta_reader").unwrap();
        assert!(sql.starts_with(
            "REVOKE ALL ON ALL TABLES IN SCHEMA td_mta FROM \"td_mta_runtime\", \"td_mta_reader\";\n"
        ));
        assert!(sql.contains(
            "GRANT SELECT, INSERT, UPDATE, DELETE ON td_mta.replica_progress TO \"td_mta_runtime\";\n"
        ));
        assert!(sql.contains("GRANT SELECT ON td_mta.replica_progress TO \"td_mta_reader\";\n"));
        assert!(
            sql.contains("GRANT SELECT, UPDATE (id) ON td_mta.accounts TO \"td_mta_reader\";\n")
        );
        assert_eq!(
            sql.lines()
                .filter(|l| l.starts_with("GRANT ") && l.contains(" ON td_mta."))
                .count(),
            2 * crate::postgres_runtime_access::replica_grants(true).count()
        );
        // One reply per statement, beside a few hundred for the handshake,
        // ledger, lock and owner reads: a schema client admits them all as
        // tables are added, where an ordinary client's 128 did not.
        assert!(sql.lines().count() * 4 < postgres_probe::SCHEMA_FRAMES);
    }
    #[test]
    fn source_identity_is_canonical_and_contains_no_credentials_or_path() {
        let epoch = StoreEpoch::from_bytes([7; 16]);
        let source = Source::new("https://MAIL.example.test:443", epoch).unwrap();
        assert_eq!(source.origin(), "https://mail.example.test");
        assert_eq!(source.epoch(), epoch);
        assert_eq!(
            source,
            Source::new("https://mail.example.test", epoch).unwrap()
        );
        assert_ne!(
            source,
            Source::new("https://mail.example.test:8443", epoch).unwrap()
        );
        assert_ne!(
            source,
            Source::new("https://mail.example.test", StoreEpoch::from_bytes([8; 16])).unwrap()
        );
        for invalid in [
            "http://mail.example.test",
            "https://user:password@mail.example.test",
            "https://mail.example.test/path",
            "https://mail.example.test?token=secret",
            "https://mail.example.test#fragment",
        ] {
            assert_eq!(Source::new(invalid, epoch).unwrap_err(), Error::Invalid);
        }
        assert_eq!(format!("{source:?}"), "ReplicaSource(<redacted>)");
    }
}
