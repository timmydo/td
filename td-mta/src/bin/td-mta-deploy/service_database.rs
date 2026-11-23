//! Provision and qualify the five PostgreSQL logins on the private trial cluster.
use crate::{
    common::{child, fmt, owned, random_hex, require, set, Host},
    R,
};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::Path,
    time::Duration,
};
use td_json::Json;
pub const ROLES: &[&str] = &["frontend", "receive", "relay", "migadu", "acme"];
pub const VAULT: &str = "/var/lib/td-mta-operator/service-passwords.json";
pub struct Passwords(Json);
impl Passwords {
    pub fn get(&self, role: &str) -> Option<&str> {
        self.0.get(role)?.as_str()
    }
    fn new() -> R<Self> {
        let mut obj = Json::Obj(Vec::new());
        for &role in ROLES {
            set(&mut obj, role, Json::Str(random_hex(32)?))?;
        }
        Ok(Self(obj))
    }
    fn validate(&self) -> R<()> {
        require(
            self.0.as_obj().is_some_and(|r| r.len() == ROLES.len()),
            "invalid credential vault",
        )?;
        for (i, &name) in ROLES.iter().enumerate() {
            let pass = self.get(name).ok_or("missing service password")?;
            require(
                pass.len() == 64
                    && pass
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                    && !ROLES
                        .iter()
                        .take(i)
                        .any(|other| self.get(other) == Some(pass)),
                "invalid service password",
            )?;
        }
        Ok(())
    }
}
pub fn passwords(create: bool) -> R<Passwords> {
    let path = Path::new(VAULT);
    let parent = path.parent().ok_or("vault parent")?;
    let missing = match fs::symlink_metadata(path) {
        Ok(_) => false,
        Err(error) if error.kind() == io::ErrorKind::NotFound => true,
        Err(error) => return Err(error.into()),
    };
    if create && missing {
        let values = Passwords::new()?;
        values.validate()?;
        let pending = child(
            parent,
            &fmt(format_args!(".passwords-{}", random_hex(12)?))?,
        )?;
        let mut out = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&pending)?;
        let result = (|| -> R<()> {
            write!(out, "{}", values.0)?;
            out.sync_all()?;
            // The production activation lock serializes vault creation. A
            // rename never leaves the final vault sharing a temporary inode.
            fs::rename(&pending, path)?;
            File::open(parent)?.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(pending);
        }
        result?;
    }
    let (mut f, meta) = td_fs::open_real_file(path, "service credential vault")?;
    require(
        meta.uid() == 0 && meta.mode() & 0o7777 == 0o600 && meta.nlink() == 1 && meta.len() <= 4096,
        "invalid service credential vault",
    )?;
    let mut data = [0u8; 4096];
    let bytes = data
        .get_mut(..usize::try_from(meta.len())?)
        .ok_or("vault size")?;
    f.read_exact(bytes)?;
    require(f.read(&mut [0u8; 1])? == 0, "credential vault changed")?;
    let json = td_json::parse_slice(bytes).map_err(|_| "invalid service credential vault")?;
    let values = Passwords(json);
    values.validate()?;
    Ok(values)
}
pub fn create_logins(host: &Host, psql: &Path, args: &[&str], values: &Passwords) -> R<()> {
    // libpq generates the salted SCRAM verifier on the client. With no
    // controlling terminal, psql reads both password prompts from stdin.
    values.validate()?;
    match File::open("/dev/tty") {
        Ok(_) => return Err("service password provisioning requires a detached supervisor".into()),
        // Linux returns ENXIO for /dev/tty in a detached systemd service.
        Err(error)
            if error.raw_os_error() == Some(6) || error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let mut cmd = host.cmd(psql);
    cmd.args(args).args([
        "-c",
        "BEGIN",
        "-c",
        "SET LOCAL password_encryption='scram-sha-256'",
    ]);
    let mut input = String::new();
    for &role in ROLES {
        let password = values.get(role).ok_or("missing role password")?;
        let create = fmt(format_args!(
            "CREATE ROLE td_mta_{role} LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE
             NOINHERIT NOREPLICATION NOBYPASSRLS"
        ))?;
        let password_command = fmt(format_args!("\\password td_mta_{role}"))?;
        let connect = fmt(format_args!(
            "GRANT CONNECT ON DATABASE td_mta TO td_mta_{role}"
        ))?;
        cmd.args(["-c", &create, "-c", &password_command, "-c", &connect]);
        for _ in 0..2 {
            td_alloc::push_str(&mut input, password)?;
            td_alloc::push_str(&mut input, "\n")?;
        }
    }
    cmd.args([
        "-c",
        "ALTER ROLE td_mta_frontend IN DATABASE td_mta SET random_page_cost=2",
        "-c",
        "COMMIT",
    ]);
    host.exec(&mut cmd, input.as_bytes(), Duration::from_secs(40), true)?;
    Ok(())
}
pub fn hba_rules() -> R<String> {
    let mut s = String::new();
    for &role in ROLES {
        td_alloc::push_str(
            &mut s,
            &fmt(format_args!("local td_mta td_mta_{role} scram-sha-256\n"))?,
        )?;
    }
    Ok(s)
}
pub fn private_hba(persistent: &Path) -> R<String> {
    let p = persistent.to_str().ok_or("invalid HBA pathname")?;
    require(
        persistent.is_absolute() && !p.contains(['"', '\r', '\n', '\\']),
        "invalid HBA include",
    )?;
    fmt(format_args!(
        "local td_mta setup_admin peer map=trial\ninclude \"{p}\"\n"
    ))
}
fn ident(s: &str) -> bool {
    s.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        && s.len() <= 63
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}
fn privileges(mut text: &str) -> bool {
    loop {
        let Some(after) = ["SELECT", "INSERT", "UPDATE", "DELETE"]
            .iter()
            .find_map(|v| text.strip_prefix(v))
        else {
            return false;
        };
        text = after;
        if let Some(columns) = text.strip_prefix(" (") {
            let Some((list, end)) = columns.split_once(')') else {
                return false;
            };
            let mut names = list.split(',');
            if !names.next().is_some_and(ident)
                || !names.all(|c| ident(c.strip_prefix(' ').unwrap_or(c)))
            {
                return false;
            }
            text = end;
        }
        if text.is_empty() {
            return true;
        }
        let Some(next) = text.strip_prefix(", ") else {
            return false;
        };
        text = next;
    }
}
pub fn grant_sql(plan: &Json) -> R<String> {
    require(
        plan.as_obj().is_some_and(|o| o.len() == ROLES.len()),
        "service grant roles",
    )?;
    let mut sql = owned("BEGIN;\n")?;
    for &role in ROLES {
        let grants = plan
            .get(role)
            .and_then(Json::as_obj)
            .filter(|t| !t.is_empty())
            .ok_or("missing service grants")?;
        let reset = fmt(format_args!(
            "GRANT USAGE ON SCHEMA td_mta TO td_mta_{role};\n\
             REVOKE ALL PRIVILEGES ON ALL TABLES IN SCHEMA td_mta FROM td_mta_{role};\n"
        ))?;
        td_alloc::push_str(&mut sql, &reset)?;
        for (table, grant) in grants {
            let p = grant.as_str().ok_or("nil service grant")?;
            require(ident(table) && privileges(p), "invalid service grant")?;
            td_alloc::push_str(
                &mut sql,
                &fmt(format_args!(
                    "GRANT {p} ON TABLE td_mta.{table} TO td_mta_{role};\n"
                ))?,
            )?;
        }
        if role != "acme" {
            let materials = fmt(format_args!(
                "GRANT USAGE ON SCHEMA td_mta_service TO td_mta_{role};\n\
                 GRANT SELECT ON td_mta_service.materials, td_mta_service.material_chunks, \
                 td_mta_service.active_certificates TO td_mta_{role};\n"
            ))?;
            td_alloc::push_str(&mut sql, &materials)?;
        }
    }
    td_alloc::push_str(&mut sql, "COMMIT;\n")?;
    Ok(sql)
}
pub fn add_hba(path: &Path) -> R<()> {
    let (mut f, m) = td_fs::open_real_file(path, "private candidate HBA")?;
    require(m.len() <= 1024 * 1024, "unexpected HBA size")?;
    let mut bytes = td_alloc::filled(usize::try_from(m.len())?, 0)?;
    f.read_exact(&mut bytes)?;
    require(f.read(&mut [0u8; 1])? == 0, "candidate HBA changed")?;
    let rules = hba_rules()?;
    if bytes.starts_with(rules.as_bytes()) {
        return Ok(());
    }
    let parent = path.parent().ok_or("HBA parent")?;
    let tmp = child(parent, &fmt(format_args!(".hba-{}", random_hex(12)?))?)?;
    let mut out = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&tmp)?;
    let result = (|| -> R<()> {
        std::os::unix::fs::fchown(&out, Some(m.uid()), Some(m.gid()))?;
        out.set_permissions(fs::Permissions::from_mode(m.mode() & 0o7777))?;
        out.write_all(rules.as_bytes())?;
        out.write_all(&bytes)?;
        out.sync_all()?;
        fs::rename(&tmp, path)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(tmp);
    }
    result
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn accepts_only_the_native_table_and_column_grant_language() {
        for v in [
            "SELECT",
            "SELECT, INSERT, UPDATE (account,id), DELETE",
            "SELECT (x, y)",
        ] {
            assert!(privileges(v));
        }
        for v in [
            "SELECT ()",
            "UPDATE (a);DELETE",
            "SELECT;",
            "SELECT, ",
            "UPDATE (a.b)",
            "SELECT (a,  b)",
            "SELECT ( a)",
            "EXECUTE",
        ] {
            assert!(!privileges(v));
        }
    }
}

#[cfg(test)]
#[path = "tests/service_database_pg.rs"]
mod postgres_test;
