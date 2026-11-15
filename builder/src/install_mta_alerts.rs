//! Optional external SMTP alerts. Independent of local submission and its DB.
use super::*;

fn issue(issues: &mut BTreeMap<String, String>, key: String, detail: String) {
    issues.insert(key, detail);
}
fn capacity(output: &str, issues: &mut BTreeMap<String, String>) -> Result<()> {
    // Reuse the strict report decoder before accepting labels into messages/state.
    capacity_summary(output)?;
    let value = json::parse(output).map_err(|e| e.to_string())?;
    for row in field(&value, "metrics")?.as_arr().ok_or("capacity rows")? {
        if string(row, "kind")? != "capacity" || string(row, "name")? == "application_logs" {
            continue;
        }
        let used = number(row, "used")?;
        let limit = number(row, "limit")?;
        if u128::from(used) * 100 >= u128::from(limit) * 80 {
            let severity = if u128::from(used) * 100 >= u128::from(limit) * 95 {
                "critical"
            } else {
                "warning"
            };
            let key = format!(
                "{severity}:{}:{}",
                string(row, "scope")?,
                string(row, "name")?
            );
            issue(
                issues,
                key,
                format!(
                    "{} {}: {used}/{limit}, remaining {}",
                    string(row, "scope")?,
                    string(row, "name")?,
                    limit.saturating_sub(used)
                ),
            );
        }
    }
    Ok(())
}
fn performance(
    label: &str,
    output: &str,
    previous: Option<&Json>,
    issues: &mut BTreeMap<String, String>,
) -> Result<Json> {
    let value = json::parse(output).map_err(|e| e.to_string())?;
    if string(&value, "status")? != "ok" {
        return Err("performance unavailable".into());
    }
    let same = previous.is_some_and(|p| p.get("boot_id") == value.get("boot_id"));
    for phase in ["smtp_commit", "smtp_publish"] {
        let failures = number(field(field(&value, "phases")?, phase)?, "failures")?;
        // Do not replay historical failures on the first sample or a restart.
        if same {
            let old = number(
                field(field(previous.ok_or("missing previous")?, "phases")?, phase)?,
                "failures",
            )?;
            if failures < old {
                return Err("counter decreased".into());
            }
            if failures > old {
                issue(
                    issues,
                    format!("{label}:{phase}"),
                    format!(
                        "{label} {phase}: {} new failures since last successful observation",
                        failures - old
                    ),
                );
            }
        }
    }
    Ok(value)
}

pub(super) fn validate(cfg: &Json) -> Result<()> {
    let Some(alert) = cfg.get("alerts") else {
        return Ok(());
    };
    for name in ["curl", "credentials"] {
        absolute(string(alert, name)?)?;
    }
    for name in ["from", "recipient"] {
        let value = string(alert, name)?;
        if value.len() > 254
            || !value.contains('@')
            || !value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"@._+-".contains(&b))
        {
            return Err("invalid alert address".into());
        }
    }
    let host = string(alert, "host")?;
    if host.is_empty()
        || host.len() > 253
        || !host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".-".contains(&b))
    {
        return Err("invalid alert SMTP host".into());
    }
    Ok(())
}

// A changed incident is sent promptly; unchanged incidents remind hourly.
// Failed/uncertain delivery retries after five minutes (duplicates are possible).
fn due(previous: &Json, keys: &Json, stamp: u64) -> Result<bool> {
    let attempted = previous
        .get("attempted")
        .map(integer)
        .transpose()?
        .unwrap_or(0);
    if stamp.saturating_sub(attempted) < 300 {
        return Ok(false);
    }
    let old = previous
        .get("delivered_keys")
        .cloned()
        .unwrap_or(Json::Arr(vec![]));
    let sent = previous.get("sent").map(integer).transpose()?.unwrap_or(0);
    Ok(old != *keys
        || (!keys.as_arr().ok_or("invalid issue keys")?.is_empty()
            && stamp.saturating_sub(sent) >= 3600))
}

pub(super) fn run(root: &Path, cfg: &Json, collected: bool, preview: bool) -> Result<()> {
    let Some(alert) = cfg.get("alerts") else {
        return Ok(());
    };
    let path = root.join("alerts.json");
    let mut previous = if path.exists() {
        read(&path)?
    } else {
        Json::Obj(vec![])
    };
    let stamp = now()?;
    let mut issues = BTreeMap::new();
    if !collected {
        issue(
            &mut issues,
            "collector".into(),
            "Native log collection unavailable; investigate the server and PostgreSQL.".into(),
        );
    }
    let mut snapshots = previous
        .get("snapshots")
        .cloned()
        .unwrap_or(Json::Obj(vec![]));
    let until = Instant::now() + Duration::from_secs(35);
    for check in field(cfg, "checks")?.as_arr().ok_or("invalid checks")? {
        let label = string(check, "name")?;
        let kind = string(check, "kind")?;
        if Instant::now() >= until {
            issue(
                &mut issues,
                "inspection-budget".into(),
                "Alert inspections incomplete: time budget exhausted.".into(),
            );
            break;
        }
        let argv = strings(field(check, "argv")?)?;
        let inspected = execute(root, label, &argv).and_then(|output| {
            match kind {
                "capacity" => capacity(&output, &mut issues)?,
                "performance" => {
                    let old = previous.get("snapshots").and_then(|s| s.get(label));
                    set(
                        &mut snapshots,
                        label,
                        performance(label, &output, old, &mut issues)?,
                    )?;
                }
                "disk" => {
                    let summary = disk_summary(&output)?;
                    if summary.contains("ATTENTION") {
                        issue(&mut issues, label.into(), summary);
                    }
                }
                _ => (), // readiness/freshness check exit status carries health.
            }
            Ok(())
        });
        if inspected.is_err() {
            issue(
                &mut issues,
                format!("unavailable:{label}"),
                format!("{label}: inspection failed; usage/health unknown."),
            );
        }
    }
    // Keep transient failure incidents open for ten quiet minutes. This avoids
    // alternating failure/recovery messages between sparse delivery attempts.
    let mut active = previous.get("active").cloned().unwrap_or(Json::Obj(vec![]));
    for (key, detail) in &issues {
        set(
            &mut active,
            key,
            object(&[("seen", num(stamp)), ("detail", Json::Str(detail.clone()))]),
        )?;
    }
    if let Json::Obj(entries) = &mut active {
        entries.retain(|(_, v)| {
            v.get("seen")
                .and_then(|x| integer(x).ok())
                .is_some_and(|seen| stamp.saturating_sub(seen) < 600)
        });
        for (key, v) in entries.iter() {
            issues
                .entry(key.clone())
                .or_insert(string(v, "detail")?.into());
        }
    } else {
        return Err("invalid active incidents".into());
    }
    let keys = Json::Arr(issues.keys().cloned().map(Json::Str).collect());
    let recovery = issues.is_empty();
    let mut body = format!(
        "td-mta {} at UTC epoch {stamp}\n\n",
        if recovery {
            "recovered"
        } else {
            "requires attention"
        }
    );
    for detail in issues.values() {
        body.push_str(detail);
        body.push('\n');
    }
    if recovery {
        body.push_str("No alert conditions remain active after inspection.\n");
    }
    body.push_str("\nDirect external SMTP; local mail submission and its database are bypassed.\nHost or network outages require an off-host heartbeat monitor.\n");
    if preview {
        print!("{body}");
        return Ok(());
    }
    let should_send = due(&previous, &keys, stamp)?;
    set(&mut previous, "snapshots", snapshots)?;
    set(&mut previous, "active", active)?;
    if should_send {
        set(&mut previous, "attempted", num(stamp))?;
    }
    save(&path, &previous)?;
    if should_send {
        deliver(root, alert, &body, recovery)?;
        set(&mut previous, "delivered_keys", keys)?;
        set(&mut previous, "sent", num(stamp))?;
        save(&path, &previous)?;
        println!("External alert accepted by SMTP provider.");
    }
    Ok(())
}

pub(super) fn test(root: &Path, cfg: &Json) -> Result<()> {
    let alert = field(cfg, "alerts")?;
    deliver(root,alert,"Configuration test: direct external SMTP alert delivery. No production incident is implied.",false)
}

fn deliver(root: &Path, cfg: &Json, body: &str, recovery: bool) -> Result<()> {
    let credentials = Path::new(string(cfg, "credentials")?);
    super::super::install_mta_setup::trusted_ancestors(
        credentials.parent().ok_or("missing credential parent")?,
    )?;
    private(credentials, false)?;
    if body.len() > 192 * 1024 || body.chars().any(|c| c.is_control() && c != '\n') {
        return Err("invalid alert body".into());
    }
    let message = root.join("alert-message.eml");
    if message.exists() {
        private(&message, false)?;
        fs::remove_file(&message).map_err(|e| e.to_string())?;
    }
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&message)
        .map_err(|e| e.to_string())?;
    write!(f,"From: {}\r\nTo: {}\r\nSubject: [td-mta {}] mx operations\r\nDate: {}\r\nAuto-Submitted: auto-generated\r\nMIME-Version: 1.0\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n{}\r\n",string(cfg,"from")?,string(cfg,"recipient")?,if recovery {"recovery"} else {"alert"},mail_date(now()?)?,body.replace('\n',"\r\n")).map_err(|e| e.to_string())?;
    f.sync_all().map_err(|e| e.to_string())?;
    let args = vec![
        string(cfg, "curl")?.into(),
        "--disable".into(),
        "--config".into(),
        string(cfg, "credentials")?.into(),
        "--silent".into(),
        "--show-error".into(),
        "--proto".into(),
        "=smtps".into(),
        "--url".into(),
        format!("smtps://{}:465", string(cfg, "host")?),
        "--ssl-reqd".into(),
        "--connect-timeout".into(),
        "5".into(),
        "--max-time".into(),
        "18".into(),
        "--mail-from".into(),
        string(cfg, "from")?.into(),
        "--mail-rcpt".into(),
        string(cfg, "recipient")?.into(),
        "--upload-file".into(),
        message.to_str().ok_or("invalid message path")?.into(),
    ];
    let result = execute(root, "external-smtp", &args);
    fs::remove_file(message).map_err(|e| e.to_string())?;
    result.map(|_| ())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    #[test]
    fn receipt_thresholds_and_rotating_logs() {
        let input = r#"{"schema":1,"status":"ok","metrics":[{"scope":"account","name":"receiving_receipts","used":819,"limit":1024,"kind":"capacity"},{"scope":"database","name":"application_logs","used":65536,"limit":65536,"kind":"capacity"}]}"#;
        let mut issues = BTreeMap::new();
        capacity(input, &mut issues).unwrap();
        assert!(issues.is_empty());
        capacity(&input.replace("819", "820"), &mut issues).unwrap();
        assert!(issues.contains_key("warning:account:receiving_receipts"));
        issues.clear();
        capacity(&input.replace("819", "1024"), &mut issues).unwrap();
        assert!(issues.contains_key("critical:account:receiving_receipts"));
    }
    #[test]
    fn failure_deltas_ignore_restart_and_first_observation() {
        let input = r#"{"status":"ok","boot_id":"one","phases":{"smtp_commit":{"failures":60},"smtp_publish":{"failures":60}}}"#;
        let mut issues = BTreeMap::new();
        let old = performance("receive", input, None, &mut issues).unwrap();
        assert!(issues.is_empty());
        performance("receive", input, Some(&old), &mut issues).unwrap();
        assert!(issues.is_empty());
        performance(
            "receive",
            &input.replace("60", "61"),
            Some(&old),
            &mut issues,
        )
        .unwrap();
        assert_eq!(issues.len(), 2);
        issues.clear();
        performance(
            "receive",
            &input.replace("one", "two"),
            Some(&old),
            &mut issues,
        )
        .unwrap();
        assert!(issues.is_empty());
    }
    #[test]
    fn incident_changes_retries_reminders_and_recovery() {
        let empty = Json::Arr(vec![]);
        let incident = Json::Arr(vec![Json::Str("capacity".into())]);
        let mut state = Json::Obj(vec![]);
        assert!(!due(&state, &empty, 10000).unwrap());
        assert!(due(&state, &incident, 10000).unwrap());
        set(&mut state, "attempted", num(10000)).unwrap();
        assert!(!due(&state, &incident, 10299).unwrap());
        assert!(due(&state, &incident, 10300).unwrap());
        set(&mut state, "delivered_keys", incident.clone()).unwrap();
        set(&mut state, "sent", num(10000)).unwrap();
        assert!(!due(&state, &incident, 10300).unwrap());
        assert!(due(&state, &incident, 13600).unwrap());
        assert!(due(&state, &empty, 10300).unwrap());
    }
}
