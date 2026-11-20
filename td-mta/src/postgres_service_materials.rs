//! Owner-derived, selected-profile grants for immutable inputs and active TLS pairs.
use crate::{
    config::{certificate, gateway, inputs, profile::Roles, storage},
    ids::StoreEpoch,
    postgres_configuration::{Error as ConfigError, Profile},
    postgres_configuration_store::Version,
    postgres_materials,
    postgres_probe::{self, Request},
    postgres_runtime_access::{Error as AccessError, ServiceRole},
    postgres_schema,
    postgres_selection::{self as selection, Error, Selection},
};
use std::time::Instant;
use td_crypto::TrustStore;
use tokio_postgres::Client;

// PostgreSQL 17 deparses the reviewed definitions with search_path=pg_catalog.
// The owner installs the same SQL checked at startup, including all row filters.
const VIEWS: &[(&str, &str)] = &[
    (
        "active_certificates",
        "SELECT a.profile,
    a.active_generation,
    a.chain,
    a.key,
    a.settings_hash
   FROM td_mta.acme_profiles a
     JOIN td_mta_service.certificate_bindings b ON b.profile = a.profile
     JOIN td_mta.config_selection s ON s.singleton AND s.version_id = b.version_id
  WHERE b.login = SESSION_USER AND a.active_generation > 0;",
    ),
    (
        "material_chunks",
        "SELECT m.material_id,
    m.ordinal,
    m.bytes
   FROM td_mta.config_material_chunks m
     JOIN td_mta_service.material_bindings b ON b.material_id = m.material_id
     JOIN td_mta.config_selection s ON s.singleton AND s.version_id = b.version_id
  WHERE b.login = SESSION_USER;",
    ),
    (
        "materials",
        "SELECT m.id,
    m.kind,
    m.length,
    m.checksum,
    m.chunk_count
   FROM td_mta.config_materials m
     JOIN td_mta_service.material_bindings b ON b.material_id = m.id
     JOIN td_mta.config_selection s ON s.singleton AND s.version_id = b.version_id
  WHERE b.login = SESSION_USER;",
    ),
];

pub(crate) fn scoped(request: &Request<'_>) -> bool {
    ServiceRole::for_login(request.user).is_some_and(|role| role != ServiceRole::Acme)
}

pub(crate) fn serves(role: ServiceRole, profile: Profile<'_>) -> bool {
    let bits = match role {
        ServiceRole::Frontend => Roles::JMAP,
        ServiceRole::Receive => Roles::RECEIVE,
        ServiceRole::Relay | ServiceRole::Migadu => Roles::RELAY,
        ServiceRole::Acme => return false,
    };
    role.name() == profile.name && profile.roles.bits() == bits
}

#[derive(Default)]
pub(crate) struct Bindings(Vec<ProfileBindings>);

struct ProfileBindings {
    role: ServiceRole,
    materials: Vec<postgres_materials::Version>,
    certificates: Vec<String>,
}

impl Bindings {
    pub(crate) fn include_profile(
        &mut self,
        profile: Profile<'_>,
        parsed: &storage::Candidate,
    ) -> Result<(), ConfigError> {
        let Some(role) = ServiceRole::ALL
            .iter()
            .copied()
            .find(|role| serves(*role, profile))
        else {
            return Ok(());
        };
        let mut bindings = ProfileBindings {
            role,
            materials: Vec::new(),
            certificates: Vec::new(),
        };
        let mut server = [false; certificate::MAX_PROFILES];
        let mut gateway = [false; gateway::MAX_GATEWAYS];
        parsed
            .with_graph(|records, text| {
                let graph = records.view(text).map_err(|_| ConfigError::Profile)?;
                let certificates = graph.certificates().map_err(|_| ConfigError::Profile)?;
                for (i, used) in server.iter_mut().take(certificates.len()).enumerate() {
                    let certificate = certificates
                        .profile(i)
                        .map_err(|_| ConfigError::Profile)?
                        .ok_or(ConfigError::Profile)?;
                    *used = graph
                        .serves_certificate(certificate.name)
                        .map_err(|_| ConfigError::Profile)?;
                    if *used && certificate.mode == certificate::Mode::Acme {
                        td_alloc::push(
                            &mut bindings.certificates,
                            td_alloc::string(certificate.name)
                                .map_err(|_| ConfigError::Capacity)?,
                        )
                        .map_err(|_| ConfigError::Capacity)?;
                    }
                }
                let gateways = graph.gateways().map_err(|_| ConfigError::Profile)?;
                for (i, used) in gateway.iter_mut().take(gateways.len()).enumerate() {
                    let entry = gateways
                        .gateway(i)
                        .map_err(|_| ConfigError::Profile)?
                        .ok_or(ConfigError::Profile)?;
                    *used = graph
                        .serves_gateway(entry.name)
                        .map_err(|_| ConfigError::Profile)?;
                }
                Ok(())
            })
            .map_err(|_| ConfigError::Profile)??;
        let mut cursor = inputs::Cursor::new(parsed).map_err(|_| ConfigError::Profile)?;
        while cursor
            .visit_next(parsed, |input| {
                use inputs::Target::*;
                let served = match input.target() {
                    CertificateKey(i) | CertificateChain(i) => server
                        .get(usize::from(i))
                        .copied()
                        .ok_or(ConfigError::Profile)?,
                    GatewayCa(i) => gateway
                        .get(usize::from(i))
                        .copied()
                        .ok_or(ConfigError::Profile)?,
                    _ => true,
                };
                if served && input.target().needed_by(profile.roles) {
                    let id = crate::postgres_profile_materials::version(input.path())
                        .map_err(ConfigError::Material)?;
                    if !bindings.materials.contains(&id) {
                        td_alloc::push(&mut bindings.materials, id)
                            .map_err(|_| ConfigError::Capacity)?;
                    }
                }
                Ok(())
            })
            .map_err(|error| match error {
                inputs::Error::Callback(error) => error,
                _ => ConfigError::Profile,
            })?
            .is_some()
        {}
        td_alloc::push(&mut self.0, bindings).map_err(|_| ConfigError::Capacity)
    }

    pub(crate) async fn replace_if_installed(
        &self,
        client: &Client,
        version: Version,
    ) -> Result<(), Error> {
        if installed(client).await? {
            self.publish_bindings(client, version).await?;
        }
        Ok(())
    }

    async fn publish_bindings(&self, client: &Client, version: Version) -> Result<(), Error> {
        if ServiceRole::ALL.iter().any(|role| {
            *role != ServiceRole::Acme && !self.0.iter().any(|binding| binding.role == *role)
        }) {
            return Err(Error::ServiceProfiles);
        }
        qualify_catalog(client, None).await.map_err(|err| {
            if err == AccessError::Privileges {
                Error::Integrity
            } else {
                Error::Database
            }
        })?;
        client.batch_execute("DELETE FROM td_mta_service.material_bindings; DELETE FROM td_mta_service.certificate_bindings").await.map_err(|_| Error::Database)?;
        for binding in &self.0 {
            let mut ids = Vec::new();
            for id in &binding.materials {
                td_alloc::push(&mut ids, id.as_bytes().as_slice()).map_err(|_| Error::Capacity)?;
            }
            let login = binding.role.login();
            client.execute("INSERT INTO td_mta_service.material_bindings (login,version_id,material_id) SELECT $1,$2,id FROM unnest($3::bytea[]) AS ids(id)", &[&login,&version.as_bytes().as_slice(),&ids]).await.map_err(|_| Error::Database)?;
            client.execute("INSERT INTO td_mta_service.certificate_bindings (login,version_id,profile) SELECT $1,$2,profile FROM unnest($3::text[]) AS profiles(profile)", &[&login,&version.as_bytes().as_slice(),&binding.certificates]).await.map_err(|_| Error::Database)?;
        }
        Ok(())
    }
}

async fn installed(client: &Client) -> Result<bool, Error> {
    client
        .query_one(
            "SELECT pg_catalog.to_regnamespace('td_mta_service') IS NOT NULL",
            &[],
        )
        .await
        .map_err(|_| Error::Database)?
        .try_get(0)
        .map_err(|_| Error::Database)
}

/// Rebuild from a validated selected version. A concurrent selection or an owner
/// mismatch rolls back the entire installation. Re-running after an uncertain commit is safe.
pub fn bind(
    request: &Request<'_>,
    trust: &TrustStore,
    deadline: Instant,
    epoch: StoreEpoch,
) -> Result<Selection, Error> {
    let (selected, candidate) =
        selection::read(request, trust, deadline, epoch)?.ok_or(Error::Conflict)?;
    let bindings = candidate.bindings;
    postgres_probe::with_mutation_client(request, trust, deadline, move |client| {
        Box::pin(async move {
            let receipt = postgres_schema::lock_and_inspect(client).await.map_err(Error::Schema)?;
            if receipt.store_epoch != epoch { return Err(Error::Epoch); }
            client.batch_execute("BEGIN ISOLATION LEVEL READ COMMITTED;
                SET LOCAL synchronous_commit=on; SET LOCAL idle_in_transaction_session_timeout=10000")
                .await.map_err(|_| Error::Database)?;
            let row = client.query_one(
                "SELECT generation,version_id FROM td_mta.config_selection WHERE singleton FOR UPDATE", &[]
            ).await.map_err(|_| Error::Database)?;
            let generation: &[u8] = row.try_get(0).map_err(|_| Error::Database)?;
            let version: Option<&[u8]> = row.try_get(1).map_err(|_| Error::Database)?;
            if generation != selected.generation.to_be_bytes() || version != Some(selected.version.as_bytes()) {
                return Err(Error::Conflict);
            }
            if !installed(client).await? {
                client.batch_execute(include_str!("postgres_service_materials.sql")).await.map_err(|_| Error::Database)?;
                for (name, definition) in VIEWS {
                    let sql=td_alloc::format(format_args!(
                        "CREATE VIEW td_mta_service.{name} WITH (security_barrier=true) AS {definition}
                        REVOKE ALL ON td_mta_service.{name} FROM PUBLIC"
                    )).map_err(|_| Error::Capacity)?;
                    client.batch_execute(&sql).await.map_err(|_| Error::Database)?;
                }
            }
            bindings.publish_bindings(client, selected.version).await?;
            client.batch_execute("COMMIT").await.map_err(|_| Error::Indeterminate)?;
            Ok(selected)
        })
    }).map_err(Error::Connection)?
}

// Both owner writes and service admission require the same reviewed objects.
// Startup additionally verifies the effective rights of the authenticated login.
pub(crate) async fn qualify_views(client: &Client, role: ServiceRole) -> Result<(), AccessError> {
    qualify_catalog(client, Some(role)).await
}

const NAMESPACE: &str = "SELECT n.nspowner=(SELECT nspowner FROM pg_catalog.pg_namespace WHERE nspname='td_mta')
    AND current_user=session_user
    AND CASE WHEN $1 THEN n.nspowner=current_user::regrole ELSE
        pg_catalog.has_schema_privilege(n.oid,'USAGE')=$2 AND NOT pg_catalog.has_schema_privilege(n.oid,'CREATE,USAGE WITH GRANT OPTION') END
    AND NOT EXISTS (SELECT FROM pg_catalog.aclexplode(n.nspacl) x WHERE x.grantee<>n.nspowner
        AND (x.grantee=0 OR x.privilege_type<>'USAGE' OR x.is_grantable))
    AND NOT EXISTS (SELECT FROM pg_catalog.pg_proc p WHERE p.pronamespace=n.oid)
    FROM pg_catalog.pg_namespace n WHERE n.nspname='td_mta_service'";

const OBJECTS: &str = "SELECT c.relname, c.relkind='v',
    c.relowner=n.nspowner AND c.relkind IN ('r','v') AND c.relpersistence='p'
    AND NOT c.relrowsecurity AND NOT c.relforcerowsecurity
    AND NOT EXISTS (SELECT FROM pg_catalog.pg_trigger t WHERE t.tgrelid=c.oid)
    AND NOT EXISTS (SELECT FROM pg_catalog.pg_rewrite r WHERE r.ev_class=c.oid AND r.rulename<>'_RETURN')
    AND NOT EXISTS (SELECT FROM pg_catalog.pg_attribute a WHERE a.attrelid=c.oid AND a.attacl IS NOT NULL)
    AND NOT EXISTS (SELECT FROM pg_catalog.aclexplode(c.relacl) x WHERE x.grantee<>c.relowner
        AND (c.relkind<>'v' OR x.grantee=0 OR x.privilege_type<>'SELECT' OR x.is_grantable))
    AND (c.relkind<>'v' OR c.reloptions=ARRAY['security_barrier=true']),
    $1 OR (pg_catalog.has_table_privilege(c.oid,'SELECT')=($2 AND c.relkind='v')
        AND pg_catalog.has_any_column_privilege(c.oid,'SELECT')=($2 AND c.relkind='v')
        AND NOT pg_catalog.has_table_privilege(c.oid,'SELECT WITH GRANT OPTION,INSERT,UPDATE,DELETE,TRUNCATE,REFERENCES,TRIGGER,MAINTAIN')
        AND NOT pg_catalog.has_any_column_privilege(c.oid,'SELECT WITH GRANT OPTION,INSERT,UPDATE,REFERENCES')),
    CASE WHEN c.relkind='v' THEN left(pg_catalog.pg_get_viewdef(c.oid,true),1024) END
    FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON c.relnamespace=n.oid
    WHERE n.nspname='td_mta_service' AND c.relkind<>'i' ORDER BY c.relname LIMIT 6";

async fn qualify_catalog(client: &Client, role: Option<ServiceRole>) -> Result<(), AccessError> {
    let owner = role.is_none();
    let reads = role.is_some_and(|role| role != ServiceRole::Acme);
    let valid: bool = client
        .query_one(NAMESPACE, &[&owner, &reads])
        .await
        .map_err(|_| AccessError::Privileges)?
        .try_get(0)
        .map_err(|_| AccessError::Database)?;
    if !valid {
        return Err(AccessError::Privileges);
    }
    let rows = client
        .query(OBJECTS, &[&owner, &reads])
        .await
        .map_err(|_| AccessError::Database)?;
    if rows.len() != VIEWS.len() + 2 {
        return Err(AccessError::Privileges);
    }
    for row in rows {
        let name: &str = row.try_get(0).map_err(|_| AccessError::Database)?;
        let view: bool = row.try_get(1).map_err(|_| AccessError::Database)?;
        let valid: Option<bool> = row.try_get(2).map_err(|_| AccessError::Database)?;
        let grants: bool = row.try_get(3).map_err(|_| AccessError::Database)?;
        if valid != Some(true) || !grants {
            return Err(AccessError::Privileges);
        }
        if view {
            let actual: &str = row.try_get(4).map_err(|_| AccessError::Database)?;
            if actual.len() >= 1024
                || !VIEWS
                    .iter()
                    .any(|&(view, sql)| view == name && sql == actual.trim())
            {
                return Err(AccessError::Privileges);
            }
        } else if !matches!(name, "material_bindings" | "certificate_bindings") {
            return Err(AccessError::Privileges);
        }
    }
    Ok(())
}
