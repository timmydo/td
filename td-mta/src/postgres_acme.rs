//! Bounded ACME ownership and certificate-pair publication in PostgreSQL.
use crate::{
    acme::{
        order::{Persistence, State},
        Error,
    },
    ids::StoreEpoch,
    postgres_probe::{self, Request},
    postgres_schema,
};
use std::time::Instant;
use td_crypto::TrustStore;

pub struct Active {
    pub generation: i64,
    pub settings: [u8; 32],
    pub chain: Vec<u8>,
    pub key: Vec<u8>,
}
impl Drop for Active {
    fn drop(&mut self) {
        self.key.fill(0);
        std::hint::black_box(&mut self.key);
    }
}
pub struct Lease {
    pub profile: String,
    pub owner: [u8; 16],
    pub revision: i64,
    pub state: State,
}
fn profile(name: &str) -> Result<(), Error> {
    if name.is_empty()
        || name.len() > 64
        || !name.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        || !name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-'))
    {
        return Err(Error::Invalid);
    }
    Ok(())
}
async fn check(client: &tokio_postgres::Client, epoch: StoreEpoch) -> Result<(), Error> {
    if postgres_schema::lock_and_inspect(client)
        .await
        .map_err(|_| Error::Database)?
        .store_epoch
        != epoch
    {
        return Err(Error::Conflict);
    }
    Ok(())
}
/// Claim one fixed slot. A durable lease spans a bounded network turn; no SQL
/// transaction or connection remains open during CA I/O. Same owner may renew.
pub struct Claim<'a> {
    pub profile: &'a str,
    pub settings: [u8; 32],
    pub owner: [u8; 16],
}

pub fn claim(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    epoch: StoreEpoch,
    claim: Claim<'_>,
    initial: &State,
) -> Result<Lease, Error> {
    let Claim {
        profile: name,
        settings,
        owner,
    } = claim;
    profile(name)?;
    let initial = initial.encode();
    if initial.len() > 32768 {
        return Err(Error::Capacity);
    }
    let name = name.to_owned();
    postgres_probe::with_material_client(request,trust,deadline,move|client| Box::pin(async move {
        check(client,epoch).await?;
        client.batch_execute("BEGIN; SET LOCAL synchronous_commit=on; SET LOCAL lock_timeout=1000; SET LOCAL statement_timeout=5000; SELECT pg_advisory_xact_lock(882015,30)").await.map_err(|_|Error::Database)?;
        client.execute("INSERT INTO td_mta.acme_profiles(slot,profile,settings_hash,revision,owner,lease_until,state) SELECT s,$1,$2,1,$3,clock_timestamp()+interval '180 seconds',$4 FROM generate_series(0,15) s WHERE NOT EXISTS(SELECT 1 FROM td_mta.acme_profiles WHERE slot=s) ORDER BY s LIMIT 1 ON CONFLICT(profile) DO NOTHING", &[&name,&settings.as_slice(),&owner.as_slice(),&initial]).await.map_err(|_|Error::Database)?;
        let row=client.query_opt("UPDATE td_mta.acme_profiles SET owner=$2,lease_until=clock_timestamp()+interval '180 seconds',revision=revision+1 WHERE profile=$1 AND settings_hash=$3 AND (owner=$2 OR lease_until<clock_timestamp()) RETURNING revision,state",&[&name,&owner.as_slice(),&settings.as_slice()]).await.map_err(|_|Error::Database)?.ok_or(Error::Conflict)?;
        let revision:i64=row.try_get(0).map_err(|_|Error::Database)?;
        let bytes:&[u8]=row.try_get(1).map_err(|_|Error::Database)?;
        let state=State::decode(bytes)?;
        client.batch_execute("COMMIT").await.map_err(|_|Error::Database)?;
        Ok(Lease {profile:name,owner,revision,state})
    })).map_err(|_|Error::Database)?
}
/// The active chain and key are returned from one row in one statement.
pub fn active(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    epoch: StoreEpoch,
    name: &str,
) -> Result<Active, Error> {
    profile(name)?;
    let name = name.to_owned();
    postgres_probe::with_material_client(request,trust,deadline,move|client|Box::pin(async move {
        check(client,epoch).await?;
        let row=client.query_opt("SELECT active_generation,chain,key,settings_hash FROM td_mta.acme_profiles WHERE profile=$1 AND active_generation>0",&[&name]).await.map_err(|_|Error::Database)?.ok_or(Error::Conflict)?;
        let active=Active{generation:row.try_get(0).map_err(|_|Error::Database)?,settings:row.try_get::<_,&[u8]>(3).map_err(|_|Error::Database)?.try_into().map_err(|_|Error::Invalid)?,chain:row.try_get(1).map_err(|_|Error::Database)?,key:row.try_get(2).map_err(|_|Error::Database)?};
        if active.generation<=0||active.chain.is_empty()||active.chain.len()>65536||active.key.is_empty()||active.key.len()>1024 {return Err(Error::Invalid);} Ok(active)
    })).map_err(|_|Error::Database)?
}
pub struct Writer<'a, 'r> {
    pub request: &'a Request<'r>,
    pub trust: &'a TrustStore,
    pub epoch: StoreEpoch,
    pub profile: &'a str,
    pub owner: [u8; 16],
    pub revision: i64,
    pub challenge: &'a std::sync::RwLock<crate::acme::challenge::Current>,
}
impl Persistence for Writer<'_, '_> {
    fn save(&mut self, state: &State, certificate: Option<(&[u8], &[u8])>) -> Result<(), Error> {
        let bytes = state.encode();
        if bytes.len() > 32768 {
            return Err(Error::Capacity);
        }
        let pair = certificate.map(|(chain, key)| (chain.to_vec(), key.to_vec()));
        if pair.as_ref().is_some_and(|(chain, key)| {
            chain.is_empty() || chain.len() > 65536 || key.is_empty() || key.len() > 1024
        }) {
            return Err(Error::Invalid);
        }
        let expires = if let Some((chain, key)) = pair.as_ref() {
            let current = self.challenge.read().map_err(|_| Error::Conflict)?;
            let names: Vec<_> = current.names.iter().map(String::as_str).collect();
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|_| Error::Deadline)?
                .as_secs();
            let identity = td_crypto::ServerIdentity::from_pem(chain, key, &names, Some(now))
                .map_err(|_| Error::Crypto)?;
            identity
                .check_exact_dns_names()
                .map_err(|_| Error::Crypto)?;
            Some(
                i64::try_from(identity.validity().1)
                    .map_err(|_| Error::Invalid)?
                    .checked_mul(1000)
                    .ok_or(Error::Invalid)?,
            )
        } else {
            None
        };
        let profile = self.profile.to_owned();
        let owner = self.owner;
        let revision = self.revision;
        let epoch = self.epoch;
        let result=postgres_probe::with_material_client(self.request,self.trust,Instant::now()+std::time::Duration::from_secs(10),move|client|Box::pin(async move {
            check(client,epoch).await?;
            client.batch_execute("SET synchronous_commit=on; SET lock_timeout=1000; SET statement_timeout=5000").await.map_err(|_|Error::Database)?;
            let (chain,mut key)=pair.unwrap_or_default();let publish=!chain.is_empty();
            let result=client.execute("UPDATE td_mta.acme_profiles SET state=$4,revision=revision+1,active_generation=active_generation+CASE WHEN $5 THEN 1 ELSE 0 END,chain=CASE WHEN $5 THEN $6 ELSE chain END,key=CASE WHEN $5 THEN $7 ELSE key END WHERE profile=$1 AND owner=$2 AND revision=$3 AND lease_until>clock_timestamp()",&[&profile,&owner.as_slice(),&revision,&bytes,&publish,&chain,&key]).await.map_err(|_|Error::Database);
            key.fill(0);std::hint::black_box(&mut key);
            if result?!=1{return Err(Error::Conflict);} Ok(())
        })).map_err(|_|Error::Database)?;
        result?;
        self.revision = self.revision.checked_add(1).ok_or(Error::Capacity)?;
        if let Some(expires_utc_ms) = expires {
            let event = crate::observability::Event {
                context: crate::observability::Context {
                    boot: crate::ids::BootId::from_bytes(self.owner),
                    utc_ms: None,
                    config_generation: 0,
                    connection: None,
                    request: None,
                    transaction: None,
                    submission: None,
                },
                kind: crate::observability::Kind::CertificateRenewed { expires_utc_ms },
            };
            let _ = crate::postgres_logs::append(
                self.request,
                self.trust,
                Instant::now() + std::time::Duration::from_secs(2),
                self.epoch,
                "acme",
                &[event],
            );
        }
        let mut challenge = self.challenge.write().map_err(|_| Error::Conflict)?;
        challenge.token = state.token.clone();
        challenge.authorization = state.key_authorization.clone();
        Ok(())
    }
}

/// Small revision inventory for service reload observation, never a key export.
pub fn generations(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    epoch: StoreEpoch,
) -> Result<Vec<(String, i64)>, Error> {
    postgres_probe::with_client(request,trust,deadline,move|client|Box::pin(async move {
        check(client,epoch).await?;
        let rows=client.query("SELECT profile,active_generation FROM td_mta.acme_profiles ORDER BY profile LIMIT 17",&[]).await.map_err(|_|Error::Database)?;
        if rows.len()>16{return Err(Error::Capacity);}
        let mut result=Vec::with_capacity(rows.len());
        for row in rows {result.push((row.try_get(0).map_err(|_|Error::Database)?,row.try_get(1).map_err(|_|Error::Database)?));}
        Ok(result)
    })).map_err(|_|Error::Database)?
}

pub struct Status {
    pub generation: i64,
    pub next_ms: i64,
    pub expires_ms: i64,
    pub failures: u32,
    pub leased: bool,
}
pub fn status(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    epoch: StoreEpoch,
    name: &str,
) -> Result<Status, Error> {
    profile(name)?;
    let name = name.to_owned();
    postgres_probe::with_material_client(request,trust,deadline,move|client|Box::pin(async move {
        check(client,epoch).await?;
        let row=client.query_opt("SELECT active_generation,state,lease_until>clock_timestamp() FROM td_mta.acme_profiles WHERE profile=$1",&[&name]).await.map_err(|_|Error::Database)?.ok_or(Error::Conflict)?;
        let state=State::decode(row.try_get(1).map_err(|_|Error::Database)?)?;
        Ok(Status {generation:row.try_get(0).map_err(|_|Error::Database)?,next_ms:state.next_ms,expires_ms:state.expires_ms,failures:state.failures,leased:row.try_get(2).map_err(|_|Error::Database)?})
    })).map_err(|_|Error::Database)?
}
