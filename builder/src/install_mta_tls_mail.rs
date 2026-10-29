//! Retryable activation of the setup replica's local certificate material.
use super::{
    install_mta_setup, peer_presented, private_file, publish, sync_dir, text, Options as Tls,
    Result, Work, LIMIT,
};
use crate::json::{self, Json};
use std::{fs, path::PathBuf, time::Duration};

pub(super) struct Options {
    pub root: PathBuf,
    pub binary: PathBuf,
    pub port: u16,
}
fn field<'a>(value: &'a Json, key: &str) -> Result<&'a str> {
    value
        .get(key)
        .and_then(Json::as_str)
        .ok_or_else(|| format!("missing renewal field {key}"))
}
fn number(value: &Json, key: &str) -> Result<u64> {
    value
        .get(key)
        .and_then(|v| match v {
            Json::Num(n) => n.parse::<u64>().ok(),
            _ => None,
        })
        .ok_or_else(|| format!("missing renewal number {key}"))
}
fn load(opts: &Options, name: &str) -> Result<String> {
    let bytes = install_mta_setup::protected_input(&opts.root.join(name), LIMIT)?;
    String::from_utf8(bytes).map_err(|_| "invalid setup text".into())
}
fn id(value: &str) -> Result<&str> {
    if value.len() != 32 || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("invalid retained identifier".into());
    }
    Ok(value)
}
fn call(opts: &Options, work: &mut Work, args: &[&str], owner: bool) -> Result<Json> {
    let mut cmd = install_mta_setup::command(&opts.binary);
    cmd.arg("postgres").args(args);
    if owner {
        cmd.args([
            "--connection-file",
            text(&opts.root.join("owner-connection"))?,
        ]);
    }
    let bytes = work.checked(cmd)?;
    let value =
        json::parse(std::str::from_utf8(&bytes).map_err(|_| "invalid mail command output")?)?;
    if field(&value, "status")? != "ok" {
        return Err("mail command did not report success".into());
    }
    Ok(value)
}
fn snapshot(opts: &Options, work: &mut Work, epoch: &str) -> Result<Json> {
    let value = call(opts, work, &["config-snapshot", "--epoch", epoch], true)?;
    if !value.get("selected").is_some_and(Json::is_true) {
        return Err("replica has no selected configuration".into());
    }
    Ok(value)
}
fn profile(snapshot: &Json, key: &str, chain: &str, port: u16) -> Result<String> {
    let profiles = snapshot
        .get("profiles")
        .and_then(Json::as_arr)
        .ok_or("missing profile inventory")?;
    let [profile] = profiles else {
        return Err("renewal requires the single-profile setup replica".into());
    };
    if field(profile, "name")? != "frontend" || number(profile, "roles")? != 2 {
        return Err("renewal requires the frontend JMAP profile".into());
    }
    let source = field(profile, "source")?;
    for expected in [
        format!("key_file = \"/postgres-material/{key}\""),
        format!("chain_file = \"/postgres-material/{chain}\""),
        format!("jmap_origin = \"https://localhost:{port}\""),
    ] {
        if source.lines().filter(|line| *line == expected).count() != 1 {
            return Err("replica configuration differs from the retained local TLS setup".into());
        }
    }
    Ok(source.to_owned())
}

fn replace_chain(source: &str, old: &str, new: &str) -> Result<String> {
    let expected = format!("chain_file = \"/postgres-material/{old}\"");
    let replacement = format!("chain_file = \"/postgres-material/{new}\"");
    let mut result = String::with_capacity(source.len());
    let mut changed = 0;
    for line in source.split_inclusive('\n') {
        let body = line.strip_suffix('\n').unwrap_or(line);
        let body = body.strip_suffix('\r').unwrap_or(body);
        if body == expected {
            changed += 1;
            result.push_str(&replacement);
            result.push_str(line.get(body.len()..).ok_or("invalid source line")?);
        } else {
            result.push_str(line);
        }
    }
    if changed != 1 {
        return Err("expected exactly one active chain reference".into());
    }
    Ok(result)
}

pub(super) fn renew(tls: &Tls, opts: &Options, work: &mut Work, certificate: &[u8]) -> Result<()> {
    let journal = tls.tls.join("mail-renew.json");
    let pending = journal.try_exists().map_err(|e| e.to_string())?;
    if !pending && peer_presented(tls, work, certificate, opts.port, false)? {
        return Ok(());
    }
    let epoch = load(opts, "store-epoch")?;
    id(&epoch)?;
    let certificate = std::str::from_utf8(certificate).map_err(|_| "invalid certificate PEM")?;
    let retained = if pending {
        private_file(&journal)?;
        json::parse(&fs::read_to_string(&journal).map_err(|e| e.to_string())?)?
    } else {
        let current = snapshot(opts, work, &epoch)?;
        let old_chain = load(opts, "material-chain-id")?;
        let key = load(opts, "material-key-id")?;
        let source = profile(&current, id(&key)?, id(&old_chain)?, opts.port)?;
        let chain = call(opts, work, &["material-version"], false)?;
        let chain = id(field(&chain, "version")?)?.to_owned();
        let version = call(opts, work, &["config-version"], false)?;
        let version = id(field(&version, "version")?)?.to_owned();
        let source = replace_chain(&source, &old_chain, &chain)?;
        let value = Json::Obj(vec![
            ("epoch".into(), Json::Str(epoch.clone())),
            (
                "generation".into(),
                Json::Num(number(&current, "generation")?.to_string()),
            ),
            (
                "previous_version".into(),
                Json::Str(id(field(&current, "version")?)?.into()),
            ),
            ("version".into(), Json::Str(version)),
            ("chain".into(), Json::Str(chain)),
            ("certificate".into(), Json::Str(certificate.into())),
            (
                "shared".into(),
                Json::Str(field(&current, "shared")?.into()),
            ),
            ("frontend".into(), Json::Str(source)),
        ]);
        let bytes = value.to_canonical();
        if bytes.len() as u64 > LIMIT {
            return Err("mail renewal journal exceeds 64 KiB".into());
        }
        publish(&tls.tls, "mail-renew.json", bytes.as_bytes())?;
        value
    };
    if field(&retained, "epoch")? != epoch || field(&retained, "certificate")? != certificate {
        return Err(
            "pending renewal belongs to a different epoch or certificate; inspect retained journal"
                .into(),
        );
    }
    let version = id(field(&retained, "version")?)?;
    let chain = id(field(&retained, "chain")?)?;
    let generation = number(&retained, "generation")?;
    let next_generation = generation
        .checked_add(1)
        .ok_or("selection generation exhausted")?;
    let current = snapshot(opts, work, &epoch)?;
    if field(&current, "version")? != version || number(&current, "generation")? != next_generation
    {
        if field(&current, "version")? != field(&retained, "previous_version")?
            || number(&current, "generation")? != generation
        {
            return Err(
                "configuration changed during renewal; retained request will not overwrite it"
                    .into(),
            );
        }
        call(
            opts,
            work,
            &[
                "material-import",
                "--epoch",
                &epoch,
                "--version",
                chain,
                "--kind",
                "certificate-chain",
                "--input-file",
                text(&tls.tls.join("server.pem"))?,
            ],
            true,
        )?;
        let shared = work.write("mail-shared", field(&retained, "shared")?.as_bytes())?;
        let frontend = work.write("mail-frontend", field(&retained, "frontend")?.as_bytes())?;
        call(
            opts,
            work,
            &[
                "config-import",
                "--epoch",
                &epoch,
                "--version",
                version,
                "--shared-file",
                text(&shared)?,
                "--instance",
                "frontend",
                "jmap",
                text(&frontend)?,
            ],
            true,
        )?;
        let request_path = tls.tls.join("mail-renew-request");
        let request = if request_path.try_exists().map_err(|e| e.to_string())? {
            private_file(&request_path)?;
            fs::read_to_string(&request_path).map_err(|e| e.to_string())?
        } else {
            let prepared = call(
                opts,
                work,
                &[
                    "config-selection-prepare",
                    "--epoch",
                    &epoch,
                    "--version",
                    version,
                    "--generation",
                    &generation.to_string(),
                ],
                true,
            )?;
            let request = field(&prepared, "request")?.to_owned();
            publish(&tls.tls, "mail-renew-request", request.as_bytes())?;
            request
        };
        // The exact request is durable before this potentially uncertain commit.
        call(opts, work, &["config-select", "--request", &request], true)?;
    }
    // The periodic watcher observes the generation, then drains and reloads.
    let mut verified = false;
    for _ in 0..12 {
        if peer_presented(tls, work, certificate.as_bytes(), opts.port, false)? {
            verified = true;
            break;
        }
        std::thread::sleep(Duration::from_secs(5));
    }
    if !verified {
        return Err(
            "mail has not presented the renewed certificate; journal retained for retry".into(),
        );
    }
    let current = snapshot(opts, work, &epoch)?;
    if field(&current, "version")? != version || number(&current, "generation")? != next_generation
    {
        return Err("configuration superseded renewal; journal retained for inspection".into());
    }
    publish(&opts.root, "material-chain-id", chain.as_bytes())?;
    publish(&opts.root, "config-version", version.as_bytes())?;
    match fs::remove_file(tls.tls.join("mail-renew-request")) {
        Ok(()) => (),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
        Err(e) => return Err(e.to_string()),
    }
    sync_dir(&tls.tls)?;
    fs::remove_file(journal).map_err(|e| e.to_string())?;
    sync_dir(&tls.tls)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    fn snapshot(source: &str, roles: u8, extra: bool) -> Json {
        let profile = Json::Obj(vec![
            ("name".into(), Json::Str("frontend".into())),
            ("roles".into(), Json::Num(roles.to_string())),
            ("source".into(), Json::Str(source.into())),
        ]);
        let mut profiles = vec![profile.clone()];
        if extra {
            profiles.push(profile);
        }
        Json::Obj(vec![("profiles".into(), Json::Arr(profiles))])
    }
    #[test]
    fn chain_replacement_ignores_comments_and_preserves_line_endings() {
        let old = "01".repeat(16);
        let new = "02".repeat(16);
        let comment = format!("# chain_file = \"/postgres-material/{old}\"\r\n");
        let active = format!("chain_file = \"/postgres-material/{old}\"\r\n");
        let source = format!("{comment}{active}# last line");
        assert_eq!(
            replace_chain(&source, &old, &new).unwrap(),
            format!("{comment}{}# last line", active.replace(&old, &new))
        );
        assert!(replace_chain(&comment, &old, &new).is_err());
        assert!(replace_chain(&format!("{active}{active}"), &old, &new).is_err());
    }
    #[test]
    fn renewal_refuses_configuration_drift_and_preserves_other_settings() {
        let key = "01".repeat(16);
        let chain = "02".repeat(16);
        let source = format!("[server]\njmap_origin = \"https://localhost:54445\"\n[certificate \"local\"]\nkey_file = \"/postgres-material/{key}\"\nchain_file = \"/postgres-material/{chain}\"\n# operator settings preserved\n");
        assert_eq!(
            profile(&snapshot(&source, 2, false), &key, &chain, 54445).unwrap(),
            source
        );
        for (candidate, roles, extra, port) in [
            (source.clone(), 3, false, 54445),
            (source.clone(), 2, true, 54445),
            (source.clone(), 2, false, 54444),
            (source.replace(&key, &chain), 2, false, 54445),
            (
                format!("{source}chain_file = \"/postgres-material/{chain}\"\n"),
                2,
                false,
                54445,
            ),
        ] {
            assert!(profile(&snapshot(&candidate, roles, extra), &key, &chain, port).is_err());
        }
    }
}
