//! Bounded host operational records and local-only notification delivery.
use super::{absolute, Result};
use crate::json::{self, Json};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;
use std::process::{Command, ExitCode};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const LIMIT: u64 = 3 * 1024 * 1024;
const EVENTS: usize = 512;
const PAGE_LIMIT: usize = 16;
fn num(v: u64) -> Json {
    Json::Num(v.to_string())
}
fn field<'a>(v: &'a Json, name: &str) -> Result<&'a Json> {
    v.get(name).ok_or_else(|| format!("missing {name}"))
}
fn integer(v: &Json) -> Result<u64> {
    match v {
        Json::Num(s) => s.parse().map_err(|_| "invalid unsigned number".into()),
        _ => Err("expected number".into()),
    }
}
fn number(v: &Json, name: &str) -> Result<u64> {
    integer(field(v, name)?)
}
fn string<'a>(v: &'a Json, name: &str) -> Result<&'a str> {
    field(v, name)?
        .as_str()
        .ok_or_else(|| format!("invalid {name}"))
}
fn set(v: &mut Json, name: &str, value: Json) -> Result<()> {
    let Json::Obj(members) = v else {
        return Err("expected object".into());
    };
    if let Some((_, old)) = members.iter_mut().find(|(k, _)| k == name) {
        *old = value;
    } else {
        members.push((name.into(), value));
    }
    Ok(())
}
fn object(values: &[(&str, Json)]) -> Json {
    Json::Obj(
        values
            .iter()
            .map(|(k, v)| ((*k).into(), v.clone()))
            .collect(),
    )
}
fn now() -> Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .map_err(|e| e.to_string())
}
fn private(path: &Path, directory: bool) -> Result<()> {
    let m = fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if m.uid() != crate::sys::getuid()
        || m.mode() & 0o077 != 0
        || (if directory {
            !m.is_dir()
        } else {
            !m.is_file() || m.len() > LIMIT
        })
    {
        return Err(format!("invalid private path {}", path.display()));
    }
    Ok(())
}
fn read(path: &Path) -> Result<Json> {
    private(path, false)?;
    let mut data = String::new();
    File::open(path)
        .map_err(|e| e.to_string())?
        .take(LIMIT + 1)
        .read_to_string(&mut data)
        .map_err(|e| e.to_string())?;
    if data.len() as u64 > LIMIT {
        return Err("state exceeds bound".into());
    }
    json::parse(&data).map_err(|e| format!("invalid JSON: {e}"))
}
fn save(path: &Path, v: &Json) -> Result<()> {
    let data = v.to_canonical();
    if data.len() as u64 > LIMIT {
        return Err("state exceeds bound".into());
    }
    let stage = path.with_extension("new");
    if stage.exists() {
        private(&stage, false)?;
        fs::remove_file(&stage).map_err(|e| e.to_string())?;
    }
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&stage)
        .map_err(|e| e.to_string())?;
    f.write_all(data.as_bytes())
        .and_then(|_| f.sync_all())
        .map_err(|e| e.to_string())?;
    fs::rename(stage, path).map_err(|e| e.to_string())?;
    sync(path.parent().ok_or("missing state parent")?)
}
fn sync(path: &Path) -> Result<()> {
    File::open(path)
        .and_then(|f| f.sync_all())
        .map_err(|e| e.to_string())
}
fn remove(path: &Path) -> Result<()> {
    fs::remove_file(path).map_err(|e| e.to_string())?;
    sync(path.parent().ok_or("missing parent")?)
}
fn lock(root: &Path, wait: Duration) -> Result<File> {
    super::install_mta_setup::trusted_ancestors(root)?;
    private(root, true)?;
    let p = root.join("lock");
    if p.exists() {
        private(&p, false)?;
    }
    let f = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(p)
        .map_err(|e| e.to_string())?;
    let deadline = Instant::now() + wait;
    while !crate::sys::flock_try_exclusive(f.as_raw_fd()).map_err(|e| e.to_string())? {
        if Instant::now() >= deadline {
            return Err("notification operation already running".into());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(f)
}
fn execute(root: &Path, label: &str, argv: &[String]) -> Result<String> {
    let (program, args) = argv.split_first().ok_or("empty check")?;
    absolute(program)?;
    let p = root.join("tool-output");
    let diagnostic = root.join(format!("tool-error-{label}"));
    for old in [&p, &diagnostic] {
        if old.exists() {
            private(old, false)?;
            fs::remove_file(old).map_err(|e| e.to_string())?;
        }
    }
    let mut out = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&p)
        .map_err(|e| e.to_string())?;
    let mut cmd = Command::new(program);
    cmd.args(args).env("LC_ALL", "C");
    let result = super::install_mta_backup::bounded_tool_output(
        cmd,
        &mut out,
        262144,
        &diagnostic,
        Duration::from_secs(20),
    );
    let answer = match result {
        Ok((_, status)) if status.success() => fs::read_to_string(&p).map_err(|e| e.to_string()),
        _ => Err("bounded inspection failed; inspect tool-error".into()),
    };
    fs::remove_file(p).map_err(|e| e.to_string())?;
    answer
}
fn strings(v: &Json) -> Result<Vec<String>> {
    v.as_arr()
        .ok_or("expected array")?
        .iter()
        .map(|v| {
            v.as_str()
                .map(str::to_owned)
                .ok_or("expected string".into())
        })
        .collect()
}
fn logs(root: &Path, cfg: &Json, before: u64) -> Result<Json> {
    let mut args = vec![
        string(cfg, "mta")?.into(),
        "postgres".into(),
        "logs".into(),
        "--connection-file".into(),
        string(cfg, "connection")?.into(),
        "--epoch".into(),
        string(cfg, "epoch")?.into(),
        "--limit".into(),
        "100".into(),
    ];
    if before != 0 {
        args.extend(["--before".into(), before.to_string()]);
    }
    json::parse(&execute(root, "logs", &args)?).map_err(|e| e.to_string())
}
fn new_monitor(epoch: &str) -> Json {
    object(&[
        ("epoch", Json::Str(epoch.into())),
        ("cursor", num(0)),
        ("initialized", Json::Bool(false)),
        ("before", num(0)),
        ("high", num(0)),
        ("checked", num(0)),
        ("evicted", num(0)),
        ("events", Json::Arr(vec![])),
    ])
}
fn wanted(row: &Json) -> Result<bool> {
    let facts = field(field(row, "event")?, "facts")?;
    Ok(matches!(
        string(facts, "code")?,
        "smtp_rejected"
            | "smtp_data_interrupted"
            | "log_suppressed"
            | "certificate_renewal_failed"
            | "relay_connection_failed"
            | "relay_step_failed"
            | "delivery_notice_deferred"
            | "service_stopped"
    ))
}
fn apply_page(state: &mut Json, page: &Json, stamp: u64) -> Result<bool> {
    if string(page, "store_epoch")? != string(state, "epoch")? {
        return Err("log epoch mismatch".into());
    }
    let before = number(state, "before")?;
    let rows = field(page, "records")?
        .as_arr()
        .ok_or("invalid log records")?;
    if rows.len() > 100 {
        return Err("oversized log page".into());
    }
    let cursor = number(state, "cursor")?;
    let mut prior = if before == 0 { u64::MAX } else { before };
    for row in rows {
        let id = number(row, "id")?;
        if id == 0 || id >= prior {
            return Err("unordered log page".into());
        }
        prior = id;
    }
    if !field(state, "initialized")?.is_true() {
        set(
            state,
            "cursor",
            num(rows
                .first()
                .map(|r| number(r, "id"))
                .transpose()?
                .unwrap_or(0)),
        )?;
        for name in ["database_suppressed", "capacity_evicted"] {
            set(state, name, num(number(page, name)?))?;
        }
        set(state, "initialized", Json::Bool(true))?;
        set(state, "checked", num(stamp))?;
        return Ok(true);
    }
    if before == 0 {
        set(
            state,
            "high",
            num(rows
                .first()
                .map(|r| number(r, "id"))
                .transpose()?
                .unwrap_or(cursor)
                .max(cursor)),
        )?;
    }
    let mut events = field(state, "events")?
        .as_arr()
        .ok_or("invalid events")?
        .to_vec();
    if events.len() > EVENTS {
        return Err("invalid event ring".into());
    }
    let mut evicted = number(state, "evicted")?;
    for row in rows {
        if number(row, "id")? > cursor && wanted(row)? {
            if row.to_canonical().len() > 4096 {
                return Err("event bound exceeded".into());
            }
            if !events.iter().any(|old| old.get("id") == row.get("id")) {
                events.push(row.clone());
            }
        }
    }
    events.sort_by_key(|row| row.get("id").and_then(|v| integer(v).ok()).unwrap_or(0));
    while events.len() > EVENTS {
        events.remove(0);
        evicted = evicted.saturating_add(1);
    }
    set(state, "events", Json::Arr(events))?;
    set(state, "evicted", num(evicted))?;
    let done = rows.len() < 100 || prior <= cursor;
    if done {
        let high = number(state, "high")?;
        set(state, "cursor", num(high))?;
        set(state, "before", num(0))?;
        set(state, "high", num(0))?;
    } else {
        set(state, "before", num(prior))?;
    }
    for name in ["database_suppressed", "capacity_evicted"] {
        set(state, name, num(number(page, name)?))?;
    }
    set(state, "checked", num(stamp))?;
    Ok(done)
}
fn collect(root: &Path, cfg: &Json) -> Result<()> {
    let path = root.join("monitor.json");
    let epoch = string(cfg, "epoch")?;
    let mut state = if path.exists() {
        read(&path)?
    } else {
        new_monitor(epoch)
    };
    if string(&state, "epoch")? != epoch {
        return Err("monitor epoch changed; retain old evidence".into());
    }
    for _ in 0..PAGE_LIMIT {
        let before = number(&state, "before")?;
        let page = logs(root, cfg, before)?;
        if string(&page, "store_epoch")? != epoch {
            return Err("log epoch mismatch".into());
        }
        let done = apply_page(&mut state, &page, now()?)?;
        save(&path, &state)?;
        if done {
            return Ok(());
        }
    }
    println!("Monitor backlog retained for next bounded page run.");
    Ok(())
}
fn render(root: &Path, cfg: &Json, previous: &Json, monitor: &Json) -> Result<(String, Json)> {
    let current = now()?;
    let cursor = number(monitor, "cursor")?;
    let old = previous
        .get("cursor")
        .map(integer)
        .transpose()?
        .unwrap_or(0);
    if cursor < old {
        return Err("monitor cursor went backwards".into());
    }
    let mut report=format!("td-mta operational digest\nUTC epoch window: {} to {current}\nCollector log IDs: {old} through {cursor}\n\n",previous.get("through").map(integer).transpose()?.unwrap_or(current));
    if current.saturating_sub(number(monitor, "checked")?) > 180 || number(monitor, "before")? != 0
    {
        report.push_str("ATTENTION: collector stale or backlog incomplete.\n");
    }
    let mut counts = BTreeMap::<String, u64>::new();
    for row in field(monitor, "events")?
        .as_arr()
        .ok_or("invalid event ring")?
    {
        let id = number(row, "id")?;
        if id <= old || id > cursor {
            continue;
        }
        let facts = field(field(row, "event")?, "facts")?;
        let code = string(facts, "code")?;
        let profile = string(row, "profile")?;
        let reason = facts.get("reason").and_then(Json::as_str).unwrap_or("");
        if [code, profile, reason].iter().any(|s| {
            s.len() > 64
                || !s
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        }) {
            return Err("invalid diagnostic label".into());
        }
        let status = facts
            .get("smtp_status")
            .map(integer)
            .transpose()?
            .map(|v| v.to_string())
            .unwrap_or_default();
        let key = format!("{profile} {code} {status} {reason}");
        *counts.entry(key).or_default() += 1;
    }
    report.push_str("Sampled diagnostics (not complete bounce or delivery counts):\n");
    if counts.is_empty() {
        report.push_str("No samples retained for this interval.\n");
    }
    for (kind, count) in counts {
        report.push_str(&format!("{kind}: {count}\n"));
    }
    for name in ["evicted", "database_suppressed", "capacity_evicted"] {
        report.push_str(&format!(
            "{name} (cumulative): {}\n",
            monitor.get(name).map(integer).transpose()?.unwrap_or(0)
        ));
    }
    let evicted = number(monitor, "evicted")?;
    let previous_evicted = previous
        .get("evicted")
        .map(integer)
        .transpose()?
        .unwrap_or(0);
    if evicted > previous_evicted {
        report.push_str(
            "ATTENTION: collector ring evicted samples; this interval may be incomplete.\n",
        );
    }
    let mut snapshots = Vec::new();
    for check in field(cfg, "checks")?.as_arr().ok_or("invalid checks")? {
        let label = string(check, "name")?;
        let argv = strings(field(check, "argv")?)?;
        let result = execute(root, label, &argv);
        report.push_str(&format!(
            "\n{label}: {}\n",
            if result.is_ok() {
                "inspection passed"
            } else {
                "ATTENTION: inspection failed"
            }
        ));
        if string(check, "kind")? == "performance" {
            if let Ok(output) = result {
                let prior = previous.get("snapshots").and_then(|s| s.get(label));
                match performance_summary(&output, prior) {
                    Ok((summary, value)) => {
                        report.push_str(&summary);
                        snapshots.push((label.into(), value));
                    }
                    Err(_) => report.push_str(
                        "ATTENTION: performance snapshot unavailable; no counter delta.\n",
                    ),
                }
            }
        } else if string(check, "kind")? == "disk" {
            if let Ok(output) = result {
                match disk_summary(&output) {
                    Ok(summary) => report.push_str(&summary),
                    Err(_) => report.push_str("ATTENTION: disk snapshot unavailable.\n"),
                }
            }
        }
    }
    report.push_str("\nPublication counters do not prove peers received SMTP replies.\nSampling, retention and process restarts limit completeness.\nLocal delivery cannot alert when this mail server is down; inspect the supervisor.\n");
    if report.len() > 32768 {
        return Err("digest report exceeds bound".into());
    }
    Ok((
        report,
        object(&[
            ("through", num(current)),
            ("cursor", num(cursor)),
            ("snapshots", Json::Obj(snapshots)),
            ("evicted", num(evicted)),
        ]),
    ))
}
fn performance_summary(output: &str, previous: Option<&Json>) -> Result<(String, Json)> {
    let value = json::parse(output).map_err(|e| e.to_string())?;
    if string(&value, "status")? != "ok" {
        return Err("performance unavailable".into());
    }
    let boot = string(&value, "boot_id")?;
    let same = previous.is_some_and(|p| p.get("boot_id").and_then(Json::as_str) == Some(boot));
    let mut summary = if same {
        "Counters since previous digest:\n"
    } else {
        "Current process lifetime; incomplete window after first run/restart:\n"
    }
    .to_string();
    for phase in [
        "smtp_admission",
        "smtp_data",
        "smtp_commit",
        "http_api",
        "http_download",
    ] {
        let values = field(field(&value, "phases")?, phase)?;
        summary.push_str(phase);
        for key in ["calls", "failures", "lost"] {
            let current = number(values, key)?;
            let prior = if same {
                previous
                    .and_then(|p| p.get("phases"))
                    .and_then(|p| p.get(phase))
                    .and_then(|p| p.get(key))
                    .map(integer)
                    .transpose()?
                    .ok_or("previous counter unavailable")?
            } else {
                0
            };
            let delta = current
                .checked_sub(prior)
                .ok_or("performance counter decreased")?;
            summary.push_str(&format!(" {key}={delta}"));
        }
        summary.push('\n');
    }
    Ok((summary, value))
}
fn disk_summary(output: &str) -> Result<String> {
    let row = output.lines().last().ok_or("missing disk row")?;
    let parts: Vec<_> = row.split_whitespace().collect();
    if parts.len() < 6 {
        return Err("invalid df -Pk output".into());
    }
    let available = parts
        .get(3)
        .ok_or("disk available missing")?
        .parse::<u64>()
        .map_err(|_| "invalid available space")?;
    let percent = parts
        .get(4)
        .and_then(|s| s.strip_suffix('%'))
        .ok_or("invalid disk use")?
        .parse::<u64>()
        .map_err(|_| "invalid disk percent")?;
    Ok(format!(
        "Disk: {percent}% used, {available} KiB available{}\n",
        if percent >= 90 || available < 3 * 1024 * 1024 {
            "; ATTENTION: low disk space"
        } else {
            ""
        }
    ))
}
fn reply(stream: &mut TcpStream, expected: u16, until: Instant) -> Result<()> {
    let mut total = 0;
    loop {
        let mut line = Vec::new();
        loop {
            stream
                .set_read_timeout(Some(
                    until
                        .checked_duration_since(Instant::now())
                        .ok_or("SMTP deadline")?,
                ))
                .map_err(|e| e.to_string())?;
            let mut b = [0];
            stream
                .read_exact(&mut b)
                .map_err(|e| format!("SMTP read: {e}"))?;
            let byte = *b.first().ok_or("missing byte")?;
            line.push(byte);
            total += 1;
            if line.len() > 512 || total > 8192 {
                return Err("SMTP reply bound".into());
            }
            if byte == b'\n' {
                break;
            }
        }
        if !line.ends_with(b"\r\n") || line.get(..3) != Some(expected.to_string().as_bytes()) {
            return Err(format!("SMTP reply was not {expected}"));
        }
        match line.get(3) {
            Some(b' ') => return Ok(()),
            Some(b'-') => (),
            _ => return Err("invalid SMTP reply".into()),
        }
    }
}
fn write(stream: &mut TcpStream, mut data: &[u8], until: Instant) -> Result<()> {
    while !data.is_empty() {
        stream
            .set_write_timeout(Some(
                until
                    .checked_duration_since(Instant::now())
                    .ok_or("SMTP deadline")?,
            ))
            .map_err(|e| e.to_string())?;
        let n = stream.write(data).map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("SMTP write made no progress".into());
        }
        data = data.get(n..).ok_or("invalid SMTP write count")?;
    }
    Ok(())
}

fn send(root: &Path, cfg: &Json, report: &str, mut receipt: Json) -> Result<()> {
    let day = now()? / 86400;
    let mut entropy = [0u8; 16];
    File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut entropy))
        .map_err(|e| e.to_string())?;
    let random = entropy
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let id = format!("td-mta-digest-{day}-{random}@localhost");
    let stamp = now()?;
    if stamp > 253402300799 {
        return Err("date outside supported range".into());
    }
    let date = crate::gate_timing::utc_iso(stamp);
    let month = date
        .get(5..7)
        .ok_or("invalid month")?
        .parse::<usize>()
        .map_err(|_| "invalid month")?;
    let months: &[&str] = &[
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let month = months
        .get(month.checked_sub(1).ok_or("invalid month")?)
        .ok_or("invalid month")?;
    let mail_date = format!(
        "{} {month} {} {} +0000",
        date.get(8..10).ok_or("invalid day")?,
        date.get(..4).ok_or("invalid year")?,
        date.get(11..19).ok_or("invalid time")?
    );
    let label = date.get(..10).ok_or("invalid date")?;
    let recipient = string(cfg, "recipient")?;
    let address: SocketAddr = string(cfg, "smtp")?
        .parse()
        .map_err(|_| "invalid SMTP endpoint")?;
    if !address.ip().is_loopback() {
        return Err("notification SMTP must be loopback".into());
    }
    let mut message=format!("From: td-mta operations <postmaster@localhost>\r\nTo: {recipient}\r\nSubject: [td-mta daily] {label}\r\nDate: {mail_date}\r\nMessage-ID: <{id}>\r\nAuto-Submitted: auto-generated\r\nX-TD-MTA-Report: daily-operations\r\nMIME-Version: 1.0\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: 8bit\r\n\r\n");
    for line in report.lines() {
        if line.len() > 990 {
            return Err("report line too long".into());
        }
        if line.starts_with('.') {
            message.push('.');
        }
        message.push_str(line);
        message.push_str("\r\n");
    }
    if message.len() > 65536 {
        return Err("digest exceeds message bound".into());
    }
    set(&mut receipt, "day", num(day))?;
    set(&mut receipt, "message_id", Json::Str(id.clone()))?;
    save(
        &root.join("last-report.json"),
        &object(&[
            ("message_id", Json::Str(id.clone())),
            ("body", Json::Str(report.into())),
        ]),
    )?;
    let until = Instant::now() + Duration::from_secs(30);
    let mut smtp =
        TcpStream::connect_timeout(&address, Duration::from_secs(5)).map_err(|e| e.to_string())?;
    reply(&mut smtp, 220, until)?;
    for (command, code) in [
        ("EHLO localhost\r\n".to_string(), 250),
        ("MAIL FROM:<> BODY=8BITMIME\r\n".into(), 250),
        (format!("RCPT TO:<{recipient}>\r\n"), 250),
        ("DATA\r\n".into(), 354),
    ] {
        write(&mut smtp, command.as_bytes(), until)?;
        reply(&mut smtp, code, until)?;
    }
    save(&root.join("pending.json"), &receipt)?;
    write(&mut smtp, message.as_bytes(), until)?;
    write(&mut smtp, b".\r\n", until)?;
    reply(&mut smtp, 250, until)?;
    save(&root.join("sent.json"), &receipt)?;
    remove(&root.join("pending.json"))?;
    let _ = write(&mut smtp, b"QUIT\r\n", until);
    println!("Digest accepted by local SMTP: <{id}>");
    Ok(())
}
fn digest(root: &Path, cfg: &Json, preview: bool) -> Result<()> {
    let sent = root.join("sent.json");
    let pending = root.join("pending.json");
    let previous = if sent.exists() {
        read(&sent)?
    } else {
        Json::Obj(vec![])
    };
    if pending.exists() {
        let p = read(&pending)?;
        if p.get("message_id").is_some() && p.get("message_id") == previous.get("message_id") {
            remove(&pending)?;
        } else {
            return Err(
                "unresolved SMTP delivery; inspect pending.json and recipient mailbox before retry"
                    .into(),
            );
        }
    }
    if !preview && previous.get("day").map(integer).transpose()? == Some(now()? / 86400) {
        println!("Digest already acknowledged for this UTC day.");
        return Ok(());
    }
    let monitor = read(&root.join("monitor.json"))?;
    if string(&monitor, "epoch")? != string(cfg, "epoch")? {
        return Err("notification epoch changed".into());
    }
    let (report, receipt) = render(root, cfg, &previous, &monitor)?;
    if preview {
        print!("{report}");
        Ok(())
    } else {
        send(root, cfg, &report, receipt)
    }
}
fn resolve(root: &Path, id: &str, outcome: &str) -> Result<()> {
    let pending = root.join("pending.json");
    let receipt = read(&pending)?;
    if string(&receipt, "message_id")? != id {
        return Err("pending Message-ID does not match".into());
    }
    if !["accepted", "not-delivered"].contains(&outcome) {
        return Err("outcome must be accepted or not-delivered".into());
    }
    let sent = root.join("sent.json");
    if outcome == "not-delivered"
        && sent.exists()
        && read(&sent)?.get("message_id") == receipt.get("message_id")
    {
        return Err("delivery already recorded as accepted".into());
    }
    save(
        &root.join("resolution.json"),
        &object(&[
            ("receipt", receipt.clone()),
            ("outcome", Json::Str(outcome.into())),
            ("resolved_at", num(now()?)),
        ]),
    )?;
    if outcome == "accepted" {
        save(&sent, &receipt)?;
    }
    remove(&pending)?;
    println!("Pending digest resolved as {outcome}: <{id}>");
    Ok(())
}
pub(super) fn run(args: &[String], mode: &str) -> ExitCode {
    let result = (|| {
        if args.len() != if mode == "resolve" { 6 } else { 2 }
            || args.first().map(String::as_str) != Some("--config-file")
        {
            return Err("use --config-file PRIVATE_PATH".into());
        }
        let path = absolute(args.get(1).ok_or("missing config")?)?;
        super::install_mta_setup::trusted_ancestors(path.parent().ok_or("missing parent")?)?;
        let cfg = read(&path)?;
        validate(&cfg)?;
        let root = absolute(string(&cfg, "state")?)?;
        let wait = if mode == "digest" {
            Duration::from_secs(130)
        } else {
            Duration::ZERO
        };
        let _lock = lock(&root, wait)?;
        match mode {
            "monitor" => collect(&root, &cfg),
            "preview" => digest(&root, &cfg, true),
            "digest" => digest(&root, &cfg, false),
            "resolve" => {
                if args.get(2).map(String::as_str) != Some("--message-id")
                    || args.get(4).map(String::as_str) != Some("--outcome")
                {
                    return Err("use --message-id ID --outcome accepted|not-delivered".into());
                }
                resolve(
                    &root,
                    args.get(3).ok_or("missing Message-ID")?,
                    args.get(5).ok_or("missing outcome")?,
                )
            }
            _ => Err("unknown notification operation".into()),
        }
    })();
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("td-mta notifications: {e}");
            ExitCode::FAILURE
        }
    }
}
pub(super) fn validate(cfg: &Json) -> Result<()> {
    for k in ["mta", "state", "connection"] {
        absolute(string(cfg, k)?)?;
    }
    let epoch = string(cfg, "epoch")?;
    if epoch.len() != 32
        || !epoch
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err("invalid notification epoch".into());
    }
    let recipient = string(cfg, "recipient")?;
    let (local, domain) = recipient.split_once('@').ok_or("recipient needs domain")?;
    if recipient.len() > 254
        || local.is_empty()
        || domain.is_empty()
        || !local
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._+-".contains(&b))
        || !domain
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".-".contains(&b))
    {
        return Err("invalid ASCII notification recipient".into());
    }
    let smtp: SocketAddr = string(cfg, "smtp")?
        .parse()
        .map_err(|_| "invalid SMTP endpoint")?;
    if !smtp.ip().is_loopback() || smtp.port() == 0 {
        return Err("SMTP must be a nonzero loopback port".into());
    }
    let checks = field(cfg, "checks")?.as_arr().ok_or("invalid checks")?;
    if checks.len() > 32 {
        return Err("too many notification checks".into());
    }
    for check in checks {
        let name = string(check, "name")?;
        if !super::profile_name(name) {
            return Err("invalid check name".into());
        }
        if !["status", "performance", "disk"].contains(&string(check, "kind")?) {
            return Err("invalid check kind".into());
        }
        let args = strings(field(check, "argv")?)?;
        if args.is_empty()
            || args.len() > 32
            || args
                .iter()
                .any(|s| s.len() > 4096 || s.chars().any(char::is_control))
        {
            return Err("invalid check argv".into());
        }
        absolute(args.first().ok_or("missing command")?)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::net::TcpListener;
    fn config(root: &Path, address: SocketAddr) -> Json {
        object(&[
            ("mta", Json::Str("/mta".into())),
            ("state", Json::Str(root.display().to_string())),
            ("connection", Json::Str("/private/owner".into())),
            ("epoch", Json::Str("a".repeat(32))),
            ("recipient", Json::Str("timmy@example.test".into())),
            ("smtp", Json::Str(address.to_string())),
            ("checks", Json::Arr(vec![])),
        ])
    }
    fn smtp_server(accepted: bool) -> (SocketAddr, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let task = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(30)))
                .unwrap();
            stream.write_all(b"220 fixture\r\n").unwrap();
            let mut input = BufReader::new(stream.try_clone().unwrap());
            for (expected, response) in [
                ("EHLO localhost", "250-fixture\r\n250 8BITMIME\r\n"),
                ("MAIL FROM:<>", "250 sender\r\n"),
                ("RCPT TO:<timmy@example.test>", "250 recipient\r\n"),
                ("DATA", "354 continue\r\n"),
            ] {
                let mut line = String::new();
                input.read_line(&mut line).unwrap();
                assert!(line.starts_with(expected));
                stream.write_all(response.as_bytes()).unwrap();
            }
            let mut body = String::new();
            loop {
                let mut line = String::new();
                assert!(input.read_line(&mut line).unwrap() > 0);
                if line == ".\r\n" {
                    break;
                }
                body.push_str(&line);
                assert!(body.len() < 65536);
            }
            stream
                .write_all(if accepted {
                    b"250 stored\r\n"
                } else {
                    b"451 refused\r\n"
                })
                .unwrap();
            body
        });
        (address, task)
    }
    #[test]
    fn local_delivery_records_acceptance_before_cleanup_and_deduplicates() {
        let root = crate::test_temp::TempDir::new("notify-send").unwrap();
        let (address, server) = smtp_server(true);
        let cfg = config(root.path(), address);
        validate(&cfg).unwrap();
        send(
            root.path(),
            &cfg,
            ".one\nTwo\n",
            object(&[
                ("through", num(1)),
                ("cursor", num(3)),
                ("snapshots", Json::Obj(vec![])),
            ]),
        )
        .unwrap();
        let wire = server.join().unwrap();
        assert!(wire.contains("\r\n..one\r\n"));
        assert!(wire.contains("\r\nDate: "));
        assert!(wire.contains("Auto-Submitted: auto-generated"));
        let sent = read(&root.path().join("sent.json")).unwrap();
        save(&root.path().join("pending.json"), &sent).unwrap();
        digest(root.path(), &cfg, false).unwrap();
        assert!(!root.path().join("pending.json").exists());
        assert_eq!(read(&root.path().join("sent.json")).unwrap(), sent);
    }
    #[test]
    fn unacknowledged_data_retains_pending_and_never_blindly_retries() {
        let root = crate::test_temp::TempDir::new("notify-uncertain").unwrap();
        let (address, server) = smtp_server(false);
        let cfg = config(root.path(), address);
        assert!(send(root.path(), &cfg, "hello\n", Json::Obj(vec![])).is_err());
        server.join().unwrap();
        assert!(root.path().join("pending.json").exists());
        assert!(!root.path().join("sent.json").exists());
        assert!(digest(root.path(), &cfg, false)
            .unwrap_err()
            .contains("unresolved SMTP"));
    }
    #[test]
    fn suppression_does_not_assume_receiver_profile_name() {
        let row = object(&[
            ("profile", Json::Str("inbound".into())),
            (
                "event",
                object(&[(
                    "facts",
                    object(&[("code", Json::Str("log_suppressed".into()))]),
                )]),
            ),
        ]);
        assert!(wanted(&row).unwrap());
    }
    #[test]
    fn performance_missing_snapshots_are_unavailable_not_zero() {
        assert!(performance_summary(r#"{"status":"unavailable"}"#, None).is_err());
        let mut phases = vec![];
        for phase in [
            "smtp_admission",
            "smtp_data",
            "smtp_commit",
            "http_api",
            "http_download",
        ] {
            phases.push((
                phase.to_string(),
                object(&[("calls", num(4)), ("failures", num(1)), ("lost", num(0))]),
            ));
        }
        let mut value = object(&[
            ("status", Json::Str("ok".into())),
            ("boot_id", Json::Str("boot".into())),
            ("phases", Json::Obj(phases)),
        ]);
        let (summary, _) = performance_summary(&value.to_canonical(), Some(&value)).unwrap();
        assert!(summary.contains("calls=0 failures=0 lost=0"));
        let mut phases = field(&value, "phases").unwrap().clone();
        set(&mut phases, "smtp_data", Json::Null).unwrap();
        set(&mut value, "phases", phases).unwrap();
        assert!(performance_summary(&value.to_canonical(), None).is_err());
    }
    #[test]
    fn explicit_resolution_binds_pending_id_and_keeps_evidence() {
        let root = crate::test_temp::TempDir::new("digest-resolution").unwrap();
        let pending = object(&[
            ("message_id", Json::Str("exact@localhost".into())),
            ("cursor", num(9)),
        ]);
        save(&root.path().join("pending.json"), &pending).unwrap();
        assert!(resolve(root.path(), "wrong", "accepted").is_err());
        assert!(resolve(root.path(), "exact@localhost", "unknown").is_err());
        resolve(root.path(), "exact@localhost", "accepted").unwrap();
        assert_eq!(
            read(&root.path().join("sent.json")).unwrap().to_canonical(),
            pending.to_canonical()
        );
        save(&root.path().join("pending.json"), &pending).unwrap();
        assert!(resolve(root.path(), "exact@localhost", "not-delivered").is_err());
        remove(&root.path().join("sent.json")).unwrap();
        resolve(root.path(), "exact@localhost", "not-delivered").unwrap();
        assert!(!root.path().join("pending.json").exists());
        assert!(!root.path().join("sent.json").exists());
        assert_eq!(
            string(
                &read(&root.path().join("resolution.json")).unwrap(),
                "outcome"
            )
            .unwrap(),
            "not-delivered"
        );
    }
    #[test]
    fn configuration_refuses_remote_smtp_and_header_injection() {
        let mut cfg = config(Path::new("/private"), "127.0.0.1:25".parse().unwrap());
        validate(&cfg).unwrap();
        set(&mut cfg, "smtp", Json::Str("192.0.2.1:25".into())).unwrap();
        assert!(validate(&cfg).is_err());
        set(&mut cfg, "smtp", Json::Str("127.0.0.1:25".into())).unwrap();
        set(
            &mut cfg,
            "recipient",
            Json::Str("a@example.test\r\nBcc: x@y.test".into()),
        )
        .unwrap();
        assert!(validate(&cfg).is_err());
    }
    #[test]
    fn render_uses_log_cursor_even_when_event_time_precedes_previous_digest() {
        let root = crate::test_temp::TempDir::new("notify-cursor").unwrap();
        let cfg = config(root.path(), "127.0.0.1:25".parse().unwrap());
        let mut monitor = new_monitor(&"a".repeat(32));
        set(&mut monitor, "checked", num(now().unwrap())).unwrap();
        set(&mut monitor, "cursor", num(20)).unwrap();
        let event = object(&[
            ("id", num(19)),
            ("recorded_ms", num(1)),
            ("profile", Json::Str("receive".into())),
            (
                "event",
                object(&[(
                    "facts",
                    object(&[
                        ("code", Json::Str("smtp_rejected".into())),
                        ("reason", Json::Str("invalid_data_framing".into())),
                    ]),
                )]),
            ),
        ]);
        set(&mut monitor, "events", Json::Arr(vec![event])).unwrap();
        let previous = object(&[("cursor", num(18)), ("through", num(100))]);
        let (report, next) = render(root.path(), &cfg, &previous, &monitor).unwrap();
        assert!(report.contains("invalid_data_framing: 1"));
        assert_eq!(number(&next, "cursor").unwrap(), 20);
        let (report, _) = render(root.path(), &cfg, &next, &monitor).unwrap();
        assert!(!report.contains("invalid_data_framing: 1"));
        assert!(disk_summary(
            "Filesystem 1024-blocks Used Available Capacity Mounted\n/dev/root 100 90 10 90% /\n"
        )
        .unwrap()
        .contains("ATTENTION"));
        assert!(disk_summary("truncated").is_err());
    }
    fn page(ids: impl Iterator<Item = u64>) -> Json {
        let rows = ids
            .map(|id| {
                object(&[
                    ("id", num(id)),
                    ("profile", Json::Str("receive".into())),
                    (
                        "event",
                        object(&[(
                            "facts",
                            object(&[
                                ("code", Json::Str("smtp_rejected".into())),
                                ("reason", Json::Str("command_refused".into())),
                            ]),
                        )]),
                    ),
                ])
            })
            .collect();
        object(&[
            ("records", Json::Arr(rows)),
            ("store_epoch", Json::Str("a".repeat(32))),
            ("database_suppressed", num(0)),
            ("capacity_evicted", num(0)),
        ])
    }
    #[test]
    fn collector_empty_baseline_first_arrival_and_pagination_survive_restart() {
        let mut state = new_monitor(&"a".repeat(32));
        assert!(apply_page(&mut state, &page(std::iter::empty()), 1).unwrap());
        assert!(apply_page(&mut state, &page(std::iter::once(1)), 2).unwrap());
        assert_eq!(number(&state, "cursor").unwrap(), 1);
        assert_eq!(field(&state, "events").unwrap().as_arr().unwrap().len(), 1);
        assert!(!apply_page(&mut state, &page((102..202).rev()), 3).unwrap());
        assert_eq!(number(&state, "cursor").unwrap(), 1);
        assert_eq!(number(&state, "before").unwrap(), 102);
        let mut restarted = json::parse(&state.to_canonical()).unwrap();
        assert!(!apply_page(&mut restarted, &page((2..102).rev()), 4).unwrap());
        assert!(apply_page(&mut restarted, &page(std::iter::once(1)), 5).unwrap());
        assert_eq!(number(&restarted, "cursor").unwrap(), 201);
        assert_eq!(
            field(&restarted, "events").unwrap().as_arr().unwrap().len(),
            201
        );
        assert!(apply_page(&mut restarted, &page((190..202).rev()), 6).unwrap());
        assert_eq!(
            field(&restarted, "events").unwrap().as_arr().unwrap().len(),
            201
        );
        let mut wrong = page(std::iter::once(202));
        set(&mut wrong, "store_epoch", Json::Str("b".repeat(32))).unwrap();
        assert!(apply_page(&mut restarted, &wrong, 7).is_err());
    }
    #[test]
    fn collector_bounds_ring_and_exposes_evictions() {
        let mut state = new_monitor(&"a".repeat(32));
        apply_page(&mut state, &page(std::iter::empty()), 1).unwrap();
        for end in [100, 200, 300, 400, 500, 600] {
            apply_page(&mut state, &page((end - 99..=end).rev()), end).unwrap();
            apply_page(&mut state, &page(std::iter::empty()), end).unwrap();
        }
        assert_eq!(
            field(&state, "events").unwrap().as_arr().unwrap().len(),
            EVENTS
        );
        assert_eq!(number(&state, "evicted").unwrap(), 600 - EVENTS as u64);
    }
}
