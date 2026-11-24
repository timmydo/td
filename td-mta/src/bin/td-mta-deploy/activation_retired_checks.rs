//! The loopback TLS material the socket cutover moves aside, and the jobs
//! that check it: the operations installer's `td-mta-maint-tls-N` expiry
//! checks, the aggregate health unit's lines and the notification checks.
//! Each is recognised only in the exact form its generator wrote; anything
//! else naming the material refuses the cutover before it fences.
use super::*;

/// One `td-mta-maint-tls-N` check of a file of retired material.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct TlsCheck {
    /// `td-mta-maint-tls-N`: the stem of its service, timer and Shepherd file.
    pub(super) name: String,
    pub(super) argv: Vec<String>,
}

impl TlsCheck {
    pub(super) fn unit(&self, suffix: &str) -> R<String> {
        fmt(format_args!("{}.{suffix}", self.name))
    }
}

const CHECK_PREFIX: &str = "td-mta-maint-tls-";

/// Each directory of loopback TLS material, with its name under `retired/`.
pub(super) fn material(isolated: bool) -> R<Vec<(PathBuf, String)>> {
    let mut dirs = Vec::new();
    td_alloc::push(
        &mut dirs,
        (child(Path::new(MAIL), "tls")?, owned("mail-tls")?),
    )?;
    if isolated {
        for &role in ROLES {
            let connection = isolation::connection(role)?;
            let home = parent(Path::new(&connection))?;
            td_alloc::push(
                &mut dirs,
                (child(home, "tls")?, fmt(format_args!("{role}-tls"))?),
            )?;
        }
    }
    Ok(dirs)
}

/// The directories of `material`.
pub(super) fn material_dirs(isolated: bool) -> R<Vec<PathBuf>> {
    let mut dirs = Vec::new();
    for (dir, _) in material(isolated)? {
        td_alloc::push(&mut dirs, dir)?;
    }
    Ok(dirs)
}

/// Whether `word` names a td-mta TLS directory or something inside one.
pub(super) fn tls_word(word: &str) -> bool {
    word.starts_with("/var/lib/td-mta") && (word.contains("/tls/") || word.ends_with("/tls"))
}

/// Whether `text` names TLS material in any word, quoted or assigned.
pub(super) fn names_material(text: &str) -> bool {
    text.split(|c: char| c.is_ascii_whitespace() || matches!(c, '"' | '\'' | '='))
        .any(tls_word)
}

// Words systemd and Shepherd quote as they are, so the generated text is
// the words in quotes.
fn plain(word: &str) -> bool {
    !word.is_empty()
        && word
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

fn program(word: &str) -> bool {
    word.starts_with('/') && word.split('/').skip(1).all(plain)
}

/// Whether `path` is a file directly inside one of `dirs`.
fn retired_file(path: &str, dirs: &[PathBuf]) -> bool {
    let path = Path::new(path);
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(plain)
        && path.parent().is_some_and(|p| dirs.iter().any(|d| d == p))
}

/// Whether `argv` is the installer's expiry check of a file in `dirs`.
pub(super) fn check_argv(argv: &[&str], dirs: &[PathBuf]) -> bool {
    matches!(
        argv,
        [openssl, "x509", "-in", path, "-noout", "-checkend", "2592000"]
            if program(openssl) && retired_file(path, dirs)
    )
}

// The installer numbers at most 32 certificates from 0.
fn check_number(n: &str) -> bool {
    n.bytes().all(|b| b.is_ascii_digit())
        && (n == "0" || !n.starts_with('0'))
        && n.parse::<u8>().is_ok_and(|n| n < 32)
}

/// `td-mta-maint-tls-N` for the installer's check service `unit`.
fn check_name(unit: &str) -> Option<&str> {
    let name = unit.strip_suffix(".service")?;
    check_number(name.strip_prefix(CHECK_PREFIX)?).then_some(name)
}

/// The argv of a `systemctl show` `ExecStart=` line systemd runs as it is,
/// failing the unit on a nonzero exit.
fn command(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("ExecStart={ path=")?;
    let (path, rest) = rest.split_once(" ; argv[]=")?;
    let (argv, rest) = rest.split_once(" ; ignore_errors=")?;
    (rest.starts_with("no ;") && argv.split(' ').next() == Some(path)).then_some(argv)
}

/// The installer's checks of retired material in `systemctl show` output
/// (blank-line separated units, `Id=` anywhere in each): a check service
/// running exactly that one command, with nothing before or after it.
pub(super) fn tls_checks(show: &str, dirs: &[PathBuf]) -> R<Vec<TlsCheck>> {
    let mut found = Vec::new();
    for block in show.split("\n\n").filter(|b| !b.trim().is_empty()) {
        let unit = block
            .lines()
            .find_map(|line| line.strip_prefix("Id="))
            .ok_or("unit without an Id")?;
        let Some(name) = check_name(unit) else {
            continue;
        };
        let mut commands = block.lines().filter(|line| line.starts_with("ExecStart"));
        let (Some(line), None) = (commands.next(), commands.next()) else {
            continue;
        };
        let Some(argv) = command(line) else {
            continue;
        };
        let mut words = Vec::new();
        for word in argv.split(' ') {
            td_alloc::push(&mut words, word)?;
        }
        if !check_argv(&words, dirs) {
            continue;
        }
        let mut argv = Vec::new();
        for word in words {
            td_alloc::push(&mut argv, owned(word)?)?;
        }
        td_alloc::push(
            &mut found,
            TlsCheck {
                name: owned(name)?,
                argv,
            },
        )?;
    }
    Ok(found)
}

fn quoted(argv: &[String]) -> R<String> {
    let mut out = String::new();
    for word in argv {
        if !out.is_empty() {
            td_alloc::push_str(&mut out, " ")?;
        }
        td_alloc::push_str(&mut out, &fmt(format_args!("\"{word}\""))?)?;
    }
    Ok(out)
}

/// The service file the installer generated for `check`.
pub(super) fn service_text(check: &TlsCheck) -> R<String> {
    fmt(format_args!(
        "[Unit]\nDescription={}\n\n[Service]\nType=oneshot\nExecStart=:{}\nUMask=0077\nTimeoutStartSec=120\nNoNewPrivileges=yes\nPrivateTmp=yes\n",
        check.name,
        quoted(&check.argv)?
    ))
}

/// The timer file the installer generated for `check`.
pub(super) fn timer_text(check: &TlsCheck) -> R<String> {
    fmt(format_args!(
        "[Unit]\nDescription=Schedule {0}\n\n[Timer]\nOnBootSec=2min\nOnUnitActiveSec=15min\nUnit={0}.service\n\n[Install]\nWantedBy=timers.target\n",
        check.name
    ))
}

/// What the Shepherd definition generated beside them holds: the service
/// it registers and the command it runs.
pub(super) fn schedule_parts(check: &TlsCheck) -> R<(String, String)> {
    Ok((
        fmt(format_args!("(service '({})\n", check.name))?,
        fmt(format_args!("(command '({})\n", quoted(&check.argv)?))?,
    ))
}

fn refuse<T>(subject: &str, why: &str) -> R<T> {
    Err(fmt(format_args!("{subject} {why}"))?.into())
}

/// Judges a check unit's `systemctl show -p LoadState -p FragmentPath -p
/// DropInPaths` output: the fragment whose file to compare when it is
/// loaded, None for a `timer` systemd does not find (the service alone is
/// still a check to retire). Its only drop-in may be `fence`, the fence's
/// condition, and `clean` says whether its drop-in directory holds nothing
/// else.
pub(super) fn check_unit_state<'a>(
    unit: &str,
    timer: bool,
    props: &'a str,
    fence: &Path,
    clean: bool,
) -> R<Option<&'a str>> {
    let prop = |key: &str| {
        props.lines().find_map(|line| {
            line.strip_prefix(key)
                .and_then(|rest| rest.strip_prefix('='))
        })
    };
    let dropins = prop("DropInPaths").ok_or("missing DropInPaths")?;
    if !clean
        || !dropins
            .split_ascii_whitespace()
            .all(|p| Path::new(p) == fence)
    {
        return refuse(unit, "has drop-ins the cutover does not retire");
    }
    match prop("LoadState") {
        Some("loaded") => match prop("FragmentPath") {
            Some(fragment) if fragment.starts_with('/') => Ok(Some(fragment)),
            _ => refuse(unit, "has no unit file"),
        },
        Some("not-found") if timer => Ok(None),
        _ => refuse(unit, "is not a loaded generated check unit"),
    }
}

/// `bytes`, the canonical file `real` of `unit`, is the installer's `text`
/// in its prefix's `share/td-mta/services`.
pub(super) fn generated_check(unit: &str, real: &Path, bytes: &[u8], text: &str) -> R<()> {
    if !generated_unit(real, unit) {
        return refuse(unit, "is not the operations installer's generated file");
    }
    if bytes != text.as_bytes() {
        return refuse(
            unit,
            "differs from the check the operations installer generates",
        );
    }
    Ok(())
}

/// `bytes`, the Shepherd definition beside `check`'s service, registers
/// that check and runs its command.
pub(super) fn schedule_check(check: &TlsCheck, bytes: &[u8]) -> R<()> {
    let text = std::str::from_utf8(bytes)?;
    let (service, command) = schedule_parts(check)?;
    if !(text.contains(&service) && text.contains(&command)) {
        return refuse(
            &check.unit("scm")?,
            "is not the check's Shepherd definition",
        );
    }
    Ok(())
}

/// The aggregate health unit `text` without the `lines` that check the
/// retired material, every other line byte for byte; None when it holds
/// none. Any other line naming the material refuses.
pub(super) fn health_unit(text: &str, lines: &[&str]) -> R<Option<String>> {
    let mut out = String::new();
    td_alloc::reserve_string(&mut out, text.len())?;
    let mut removed = false;
    for line in text.split_inclusive('\n') {
        let body = line.strip_suffix('\n').unwrap_or(line);
        if lines.contains(&body) {
            removed = true;
            continue;
        }
        require(
            !names_material(body),
            "td-mta-health.service names loopback TLS material in a line the cutover does not recognise",
        )?;
        td_alloc::push_str(&mut out, line)?;
    }
    Ok(removed.then_some(out))
}

/// `text`, a notification configuration, without its status checks of
/// retired material, written by `encode`, with their names; None when it
/// has none. A check naming the material in any other form refuses. When
/// `exact`, so does a text `encode` would not write back as it is; else
/// the operator's form is replaced by `encode`'s, the content kept.
pub(super) fn notification_config(
    text: &str,
    dirs: &[PathBuf],
    exact: bool,
    encode: impl Fn(&Json) -> R<String>,
) -> R<Option<(String, Vec<String>)>> {
    let mut config = td_json::parse(text).map_err(|_| "invalid notification configuration")?;
    let faithful = !exact || encode(&config)? == text;
    let Some(Json::Arr(checks)) = config.get_mut("checks") else {
        return Err("missing notification checks".into());
    };
    let mut kept = Vec::new();
    let mut removed = Vec::new();
    for check in checks.drain(..) {
        let mut words = Vec::new();
        for word in check
            .get("argv")
            .and_then(Json::as_arr)
            .ok_or("invalid notification check")?
        {
            td_alloc::push(
                &mut words,
                word.as_str().ok_or("invalid notification check")?,
            )?;
        }
        if !words.iter().any(|word| names_material(word)) {
            td_alloc::push(&mut kept, check)?;
            continue;
        }
        let name = check.get("name").and_then(Json::as_str);
        require(
            matches!(&check, Json::Obj(fields) if fields.len() == 3)
                && check.get("kind").and_then(Json::as_str) == Some("status")
                && name
                    .and_then(|n| n.strip_prefix("tls-"))
                    .is_some_and(check_number)
                && check_argv(&words, dirs),
            "a notification check names loopback TLS material in a form the cutover does not retire",
        )?;
        td_alloc::push(
            &mut removed,
            owned(name.ok_or("invalid notification check")?)?,
        )?;
    }
    *checks = kept;
    if removed.is_empty() {
        return Ok(None);
    }
    require(
        faithful,
        "a notification configuration naming loopback TLS material is not in the form its writer leaves",
    )?;
    Ok(Some((encode(&config)?, removed)))
}
