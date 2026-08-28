//! Bounded Firefox autotest introspection over Marionette's loopback protocol.

use crate::cgroup::ProcessSandbox;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

const MARIONETTE_PORT: u16 = 2828;
const PROBE_DEADLINE: Duration = Duration::from_secs(60);
const MAX_FRAME_BYTES: usize = 1024 * 1024;
const MAX_COMMAND_BYTES: usize = 64 * 1024;
const MAX_HEADER_DIGITS: usize = 7;
const HELLO: &str = r#"{"applicationType":"gecko","marionetteProtocol":3}"#;
const NEW_SESSION: &str = r#"[0,1,"WebDriver:NewSession",{}]"#;
const NEW_SESSION_PREFIX: &str = r#"[1,1,null,{"sessionId":"#;
const SET_CONTEXT: &str = r#"[0,2,"Marionette:SetContext",{"value":"chrome"}]"#;
const SET_CONTEXT_RESPONSE: &str = r#"[1,2,null,{"value":null}]"#;
const DELETE_SESSION: &str = r#"[0,4,"WebDriver:DeleteSession",{}]"#;
const DELETE_SESSION_RESPONSE: &str = r#"[1,4,null,{"value":null}]"#;
const EXECUTE_RESPONSE_PREFIX: &str = "[1,3,null,{\"value\":\"";
const EXECUTE_RESPONSE_SUFFIX: &str = r#""}]"#;
const REPORT_PREFIX: &str = "TD-FIREFOX-SUPPORT-V1";

// This runs only after the exact QEMU autotest launch enabled Marionette and
// opted into privileged script execution. It reads Firefox's own about:support
// providers and reports Firefox's own child-role mapping. The Rust side binds
// those namespace PIDs to the live application cgroup and reads their kernel
// status independently. The returned alphabet deliberately needs no JSON
// escapes, so the Rust side can validate one exact response shape.
const REPORT_SCRIPT: &str = r#"
const done = arguments[arguments.length - 1];
(async () => {
  const { Troubleshoot } = ChromeUtils.importESModule(
    "resource://gre/modules/Troubleshoot.sys.mjs"
  );
  const snapshot = await Troubleshoot.snapshot();
  const graphics = snapshot.graphics || {};
  const sandbox = snapshot.sandbox || {};
  const processes = snapshot.processes || {};
  const remoteTypes = processes.remoteTypes || {};
  const processInfo = await ChromeUtils.requestProcInfo();
  const window = Services.wm.getMostRecentWindow("");
  const roles = new Map([
    ["content", new Set()],
    ["gpu", new Set()],
    ["socket", new Set()],
    ["rdd", new Set()],
    ["utility", new Set()],
  ]);
  const mediaUtilityPids = new Set();
  const contentTypes = new Set([
    "web",
    "webIsolated",
    "file",
    "extension",
    "privilegedabout",
    "privilegedmozilla",
    "withCoopCoep",
    "webServiceWorker",
    "preallocated",
    "inference",
  ]);
  const mediaUtilityActors = new Set([
    "audioDecoder_Generic",
    "audioDecoder_AppleMedia",
    "audioDecoder_WMF",
    "mfMediaEngineCDM",
  ]);
  const nonMediaUtilityActors = new Set([
    "jSOracle",
    "windowsUtils",
    "windowsFileDialog",
    "pkcs11Module",
  ]);
  const add = (role, pid) => {
    const set = roles.get(role);
    if (set && Number.isInteger(pid) && pid > 0) {
      set.add(pid);
    }
  };
  for (const child of processInfo.children) {
    const type = String(child.type || "");
    if (contentTypes.has(type)) {
      add("content", child.pid);
    } else if (type === "gpu" || type === "vr") {
      add("gpu", child.pid);
    } else if (type === "socket") {
      add("socket", child.pid);
    } else if (type === "rdd") {
      add("rdd", child.pid);
    } else if (type === "utility") {
      add("utility", child.pid);
      const actors = Array.from(child.utilityActors || []);
      for (const actor of actors) {
        const actorName = String(actor.actorName || "");
        if (mediaUtilityActors.has(actorName)) {
          mediaUtilityPids.add(child.pid);
        } else if (!nonMediaUtilityActors.has(actorName)) {
          throw new Error(`unknown utility actor ${actorName}`);
        }
      }
    } else if (type === "gmpPlugin") {
      add("utility", child.pid);
      mediaUtilityPids.add(child.pid);
    } else if (type !== "forkServer") {
      throw new Error(`unknown process type ${type}`);
    }
  }
  try {
    add("gpu", window.windowUtils.gpuProcessPid);
  } catch (_) {}
  try {
    if (Services.io.socketProcessLaunched) {
      add("socket", Services.io.socketProcessId);
    }
  } catch (_) {}
  const roleReports = [];
  for (const [role, pids] of roles) {
    roleReports.push(`${role}:${Array.from(pids).sort((a, b) => a - b).join(".")}`);
  }
  const clean = value => {
    const sanitized = Array.from(
      String(value === undefined ? "missing" : value),
      character => {
        const code = character.codePointAt(0);
        return code >= 0x20 && code <= 0x7e &&
          !"|,\"\\".includes(character) ? character : "_";
      }
    ).join("");
    return sanitized || "missing";
  };
  const remote = Object.keys(remoteTypes)
    .sort()
    .map(name => `${clean(name)}:${remoteTypes[name]}`)
    .join(",");
  done([
    "TD-FIREFOX-SUPPORT-V1",
    `protocol=${clean(graphics.windowProtocol)}`,
    `compositor=${clean(graphics.windowLayerManagerType)}`,
    `adapter=${clean(graphics.adapterDescription)}`,
    `seccomp_bpf=${clean(sandbox.hasSeccompBPF)}`,
    `seccomp_tsync=${clean(sandbox.hasSeccompTSync)}`,
    `privileged_userns=${clean(sandbox.hasPrivilegedUserNamespaces)}`,
    `userns=${clean(sandbox.hasUserNamespaces)}`,
    `content_sandbox=${clean(sandbox.canSandboxContent)}`,
    `media_sandbox=${clean(sandbox.canSandboxMedia)}`,
    `configured=${clean(sandbox.contentSandboxLevel)}`,
    `effective=${clean(sandbox.effectiveContentSandboxLevel)}`,
    `roles=${roleReports.join(",")}`,
    `media=${Array.from(mediaUtilityPids).sort((a, b) => a - b).join(".") || "none"}`,
    `remote=${remote || "missing"}`,
  ].join("|"));
})().catch(error => done(
  "TD-FIREFOX-SUPPORT-ERROR:" +
  String(error).replace(/[|,"\\\r\n]/g, "_").slice(0, 256)
));
"#;

#[derive(Clone, Debug, Eq, PartialEq)]
struct RoleReport {
    name: String,
    namespace_pids: Vec<u32>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SupportReport {
    protocol: String,
    compositor: String,
    adapter: String,
    seccomp_bpf: bool,
    seccomp_tsync: bool,
    privileged_userns: bool,
    userns: bool,
    content_sandbox: bool,
    media_sandbox: bool,
    configured: u32,
    effective: u32,
    roles: Vec<RoleReport>,
    media_utility_pids: Vec<u32>,
    remote: String,
}

pub(crate) fn probe_support() -> io::Result<SupportReport> {
    let address = SocketAddr::from(([127, 0, 0, 1], MARIONETTE_PORT));
    let deadline = Instant::now()
        .checked_add(PROBE_DEADLINE)
        .ok_or_else(|| io::Error::other("Firefox probe deadline overflow"))?;
    let stream = TcpStream::connect_timeout(&address, remaining(deadline)?)
        .map_err(|error| contextual("connect to Firefox Marionette on loopback", error))?;
    let mut stream = DeadlineStream { stream, deadline };

    probe_stream(&mut stream)
}

struct DeadlineStream {
    stream: TcpStream,
    deadline: Instant,
}

impl Read for DeadlineStream {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.stream
            .set_read_timeout(Some(remaining(self.deadline)?))
            .map_err(|error| contextual("set Firefox probe read deadline", error))?;
        self.stream.read(buffer)
    }
}

impl Write for DeadlineStream {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.stream
            .set_write_timeout(Some(remaining(self.deadline)?))
            .map_err(|error| contextual("set Firefox probe write deadline", error))?;
        self.stream.write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stream
            .set_write_timeout(Some(remaining(self.deadline)?))
            .map_err(|error| contextual("set Firefox probe flush deadline", error))?;
        self.stream.flush()
    }
}

fn remaining(deadline: Instant) -> io::Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|duration| !duration.is_zero())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                "Firefox support probe exceeded its 60-second deadline",
            )
        })
}

fn probe_stream<S: Read + Write>(stream: &mut S) -> io::Result<SupportReport> {
    let hello = read_frame(stream)
        .map_err(|error| contextual("read Firefox Marionette greeting", error))?;
    require_exact("Marionette greeting", &hello, HELLO)?;
    write_frame(stream, NEW_SESSION)
        .map_err(|error| contextual("write Firefox new-session command", error))?;
    let new_session = read_frame(stream)
        .map_err(|error| contextual("read Firefox new-session response", error))?;
    if !new_session.starts_with(NEW_SESSION_PREFIX) || !new_session.ends_with("}}]") {
        return Err(unexpected("new-session response", &new_session));
    }

    let result = run_session_probe(stream);
    let cleanup = delete_session(stream);
    match (result, cleanup) {
        (Ok(report), Ok(())) => Ok(report),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Err(probe), Err(cleanup)) => Err(io::Error::other(format!(
            "Firefox support probe failed: {probe}; session cleanup also failed: {cleanup}"
        ))),
    }
}

fn run_session_probe<S: Read + Write>(stream: &mut S) -> io::Result<SupportReport> {
    write_frame(stream, SET_CONTEXT)
        .map_err(|error| contextual("write Firefox set-context command", error))?;
    let context = read_frame(stream)
        .map_err(|error| contextual("read Firefox set-context response", error))?;
    require_exact("set-context response", &context, SET_CONTEXT_RESPONSE)?;

    let command = execute_command(REPORT_SCRIPT)?;
    write_frame(stream, &command)
        .map_err(|error| contextual("write Firefox support script", error))?;
    let response = read_frame(stream)
        .map_err(|error| contextual("read Firefox support script response", error))?;
    let value = response
        .strip_prefix(EXECUTE_RESPONSE_PREFIX)
        .and_then(|rest| rest.strip_suffix(EXECUTE_RESPONSE_SUFFIX))
        .ok_or_else(|| unexpected("execute-script response", &response))?;
    if value.contains(['"', '\\'])
        || !value
            .bytes()
            .all(|byte| byte == b' ' || byte.is_ascii_graphic())
    {
        return Err(io::Error::other(format!(
            "Firefox support report used an escaped or non-ASCII value: {}",
            printable(value)
        )));
    }
    parse_report(value)
}

fn delete_session<S: Read + Write>(stream: &mut S) -> io::Result<()> {
    write_frame(stream, DELETE_SESSION)
        .map_err(|error| contextual("write Firefox delete-session command", error))?;
    let response = read_frame(stream)
        .map_err(|error| contextual("read Firefox delete-session response", error))?;
    require_exact(
        "delete-session response",
        &response,
        DELETE_SESSION_RESPONSE,
    )
}

fn execute_command(script: &str) -> io::Result<String> {
    let escaped = json_string(script);
    let command =
        format!("[0,3,\"WebDriver:ExecuteAsyncScript\",{{\"script\":{escaped},\"args\":[]}}]");
    if command.len() > MAX_COMMAND_BYTES {
        return Err(io::Error::other(
            "Firefox support command exceeded its 64 KiB bound",
        ));
    }
    Ok(command)
}

fn json_string(value: &str) -> String {
    let mut output = String::with_capacity(value.len().saturating_add(2));
    output.push('"');
    for character in value.chars() {
        match character {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            '\u{08}' => output.push_str("\\b"),
            '\u{0c}' => output.push_str("\\f"),
            value if (value as u32) < 0x20 => {
                output.push_str(&format!("\\u{:04x}", value as u32));
            }
            value => output.push(value),
        }
    }
    output.push('"');
    output
}

fn write_frame<W: Write>(writer: &mut W, body: &str) -> io::Result<()> {
    if body.is_empty() || body.len() > MAX_COMMAND_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Marionette command length is outside its bound",
        ));
    }
    write!(writer, "{}:", body.len())?;
    writer.write_all(body.as_bytes())
}

fn read_frame<R: Read>(reader: &mut R) -> io::Result<String> {
    let mut length = 0_usize;
    let mut digits = 0_usize;
    let mut first = None;
    loop {
        let mut byte = [0_u8; 1];
        reader.read_exact(&mut byte)?;
        match byte.first().copied() {
            Some(b':') if digits > 0 => break,
            Some(value @ b'0'..=b'9') if digits < MAX_HEADER_DIGITS => {
                if first.is_none() {
                    first = Some(value);
                }
                length = length
                    .checked_mul(10)
                    .and_then(|current| current.checked_add(usize::from(value - b'0')))
                    .ok_or_else(|| io::Error::other("Marionette frame length overflowed"))?;
                digits = digits.saturating_add(1);
            }
            _ => {
                return Err(io::Error::other(
                    "Marionette frame header is not canonical decimal",
                ));
            }
        }
    }
    if length == 0 || length > MAX_FRAME_BYTES || (digits > 1 && first == Some(b'0')) {
        return Err(io::Error::other(
            "Marionette frame length is empty, over limit, or noncanonical",
        ));
    }
    let mut body = vec![0_u8; length];
    reader.read_exact(&mut body)?;
    String::from_utf8(body)
        .map_err(|error| io::Error::other(format!("Marionette frame is not UTF-8: {error}")))
}

fn parse_report(value: &str) -> io::Result<SupportReport> {
    let mut fields = value.split('|');
    if fields.next() != Some(REPORT_PREFIX) {
        return Err(io::Error::other(format!(
            "Firefox returned an unexpected support report: {}",
            printable(value)
        )));
    }
    let protocol = take_field(&mut fields, "protocol")?;
    let compositor = take_field(&mut fields, "compositor")?;
    let adapter = take_field(&mut fields, "adapter")?;
    let seccomp_bpf = parse_bool("seccomp_bpf", &take_field(&mut fields, "seccomp_bpf")?)?;
    let seccomp_tsync = parse_bool("seccomp_tsync", &take_field(&mut fields, "seccomp_tsync")?)?;
    let privileged_userns = parse_bool(
        "privileged_userns",
        &take_field(&mut fields, "privileged_userns")?,
    )?;
    let userns = parse_bool("userns", &take_field(&mut fields, "userns")?)?;
    let content_sandbox = parse_bool(
        "content_sandbox",
        &take_field(&mut fields, "content_sandbox")?,
    )?;
    let media_sandbox = parse_bool("media_sandbox", &take_field(&mut fields, "media_sandbox")?)?;
    let configured = parse_u32("configured", &take_field(&mut fields, "configured")?)?;
    let effective = parse_u32("effective", &take_field(&mut fields, "effective")?)?;
    let roles = parse_roles(&take_field(&mut fields, "roles")?)?;
    let media_utility_pids = parse_pid_list(&take_field(&mut fields, "media")?)?;
    let remote = take_field(&mut fields, "remote")?;
    if fields.next().is_some() {
        return Err(io::Error::other(
            "Firefox support report carried trailing fields",
        ));
    }
    Ok(SupportReport {
        protocol,
        compositor,
        adapter,
        seccomp_bpf,
        seccomp_tsync,
        privileged_userns,
        userns,
        content_sandbox,
        media_sandbox,
        configured,
        effective,
        roles,
        media_utility_pids,
        remote,
    })
}

fn take_field<'a, I>(fields: &mut I, name: &str) -> io::Result<String>
where
    I: Iterator<Item = &'a str>,
{
    let field = fields
        .next()
        .ok_or_else(|| io::Error::other(format!("Firefox support report omitted {name}")))?;
    let expected = format!("{name}=");
    let value = field.strip_prefix(&expected).ok_or_else(|| {
        io::Error::other(format!(
            "Firefox support report expected {name}, got {}",
            printable(field)
        ))
    })?;
    if value.is_empty() || value.len() > 4096 {
        return Err(io::Error::other(format!(
            "Firefox support field {name} is empty or over limit"
        )));
    }
    Ok(value.to_string())
}

fn parse_bool(name: &str, value: &str) -> io::Result<bool> {
    match value {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(io::Error::other(format!(
            "Firefox support field {name} is not boolean: {}",
            printable(value)
        ))),
    }
}

fn parse_u32(name: &str, value: &str) -> io::Result<u32> {
    value.parse::<u32>().map_err(|error| {
        io::Error::other(format!(
            "Firefox support field {name} is not a bounded integer: {error}"
        ))
    })
}

fn parse_roles(value: &str) -> io::Result<Vec<RoleReport>> {
    let mut reports = Vec::new();
    let mut total_pids = 0_usize;
    for row in value.split(',') {
        let mut fields = row.splitn(2, ':');
        let name = fields
            .next()
            .filter(|name| matches!(*name, "content" | "gpu" | "socket" | "rdd" | "utility"))
            .ok_or_else(|| io::Error::other("Firefox support report has an unknown role"))?;
        if reports
            .iter()
            .any(|report: &RoleReport| report.name == name)
        {
            return Err(io::Error::other(
                "Firefox support report repeats a process role",
            ));
        }
        let encoded = fields
            .next()
            .ok_or_else(|| io::Error::other(format!("Firefox role {name} omitted its PIDs")))?;
        let mut namespace_pids = Vec::new();
        if !encoded.is_empty() {
            for field in encoded.split('.') {
                let pid = parse_u32(&format!("role {name} PID"), field)?;
                if pid == 0
                    || namespace_pids.contains(&pid)
                    || reports
                        .iter()
                        .any(|report: &RoleReport| report.namespace_pids.contains(&pid))
                {
                    return Err(io::Error::other(format!(
                        "Firefox role {name} has a zero or repeated PID"
                    )));
                }
                total_pids = total_pids.saturating_add(1);
                if total_pids > 256 {
                    return Err(io::Error::other(
                        "Firefox support report exceeded 256 child PIDs",
                    ));
                }
                namespace_pids.push(pid);
            }
        }
        reports.push(RoleReport {
            name: name.to_string(),
            namespace_pids,
        });
    }
    if reports.len() != 5 {
        return Err(io::Error::other(
            "Firefox support report did not name all five process roles",
        ));
    }
    Ok(reports)
}

fn parse_pid_list(value: &str) -> io::Result<Vec<u32>> {
    if value == "none" {
        return Ok(Vec::new());
    }
    let mut pids = Vec::new();
    for field in value.split('.') {
        let pid = parse_u32("media utility PID", field)?;
        if pid == 0 || pids.contains(&pid) || pids.len() >= 256 {
            return Err(io::Error::other(
                "Firefox media utility PID set is zero, repeated, or over limit",
            ));
        }
        pids.push(pid);
    }
    Ok(pids)
}

pub(crate) fn namespace_pids(report: &SupportReport) -> Vec<u32> {
    report
        .roles
        .iter()
        .flat_map(|role| role.namespace_pids.iter().copied())
        .collect()
}

pub(crate) fn validate_and_render(
    report: &SupportReport,
    sandboxes: &[ProcessSandbox],
) -> io::Result<String> {
    validate_report(report, sandboxes)?;
    Ok(format!(
        "TD-FIREFOX-SUPPORT-OK protocol={} compositor={} adapter={} roles={} media={} remote={}",
        report.protocol,
        report.compositor,
        report.adapter,
        render_roles(&report.roles, sandboxes),
        render_pids(&report.media_utility_pids),
        report.remote
    ))
}

fn validate_report(report: &SupportReport, sandboxes: &[ProcessSandbox]) -> io::Result<()> {
    if report.protocol != "wayland" {
        return Err(report_error(
            report,
            sandboxes,
            "window protocol is not Wayland",
        ));
    }
    if report.compositor != "WebRender (Software)" {
        return Err(report_error(
            report,
            sandboxes,
            "compositing is not Software WebRender",
        ));
    }
    if !report.seccomp_bpf
        || !report.seccomp_tsync
        || report.privileged_userns
        || report.userns
        || !report.content_sandbox
        || !report.media_sandbox
        || report.configured != 6
        || report.effective != 6
    {
        return Err(report_error(
            report,
            sandboxes,
            "about:support sandbox facts differ from the pinned fallback policy",
        ));
    }
    for required in ["content", "socket"] {
        require_role(report, sandboxes, required)?;
    }
    let rdd = role(report, "rdd");
    let utility = role(report, "utility");
    let media_utility = !report.media_utility_pids.is_empty();
    if media_utility
        && !utility.is_some_and(|role| {
            report
                .media_utility_pids
                .iter()
                .all(|pid| role.namespace_pids.contains(pid))
        })
    {
        return Err(report_error(
            report,
            sandboxes,
            "media utility PID is not a reported utility process",
        ));
    }
    if !rdd.is_some_and(|role| role_is_sandboxed(role, sandboxes)) && !media_utility {
        return Err(report_error(
            report,
            sandboxes,
            "neither Firefox media role carried a nested seccomp filter",
        ));
    }
    for observed in &report.roles {
        if !observed.namespace_pids.is_empty() && !role_is_sandboxed(observed, sandboxes) {
            return Err(report_error(
                report,
                sandboxes,
                &format!(
                    "Firefox role {} contains a process without its nested seccomp filter",
                    observed.name
                ),
            ));
        }
    }
    Ok(())
}

fn require_role(
    report: &SupportReport,
    sandboxes: &[ProcessSandbox],
    name: &str,
) -> io::Result<()> {
    if role(report, name).is_some_and(|role| role_is_sandboxed(role, sandboxes)) {
        return Ok(());
    }
    Err(report_error(
        report,
        sandboxes,
        &format!("Firefox role {name} is absent or lacks its nested seccomp filter"),
    ))
}

fn role<'a>(report: &'a SupportReport, name: &str) -> Option<&'a RoleReport> {
    report.roles.iter().find(|role| role.name == name)
}

fn role_is_sandboxed(role: &RoleReport, sandboxes: &[ProcessSandbox]) -> bool {
    !role.namespace_pids.is_empty()
        && role.namespace_pids.iter().all(|pid| {
            sandboxes
                .iter()
                .any(|sandbox| sandbox.namespace_pid == *pid && sandbox_is_nested(sandbox))
        })
}

fn sandbox_is_nested(sandbox: &ProcessSandbox) -> bool {
    sandbox.no_new_privileges == 1 && sandbox.seccomp == 2 && sandbox.filters >= 2
}

fn report_error(report: &SupportReport, sandboxes: &[ProcessSandbox], message: &str) -> io::Error {
    io::Error::other(format!(
        "{message}: protocol={} compositor={} adapter={} sandbox={}/{}/{}/{}/{}/{}/{}:{} roles={} media={} remote={}",
        report.protocol,
        report.compositor,
        report.adapter,
        report.seccomp_bpf,
        report.seccomp_tsync,
        report.privileged_userns,
        report.userns,
        report.content_sandbox,
        report.media_sandbox,
        report.configured,
        report.effective,
        render_roles(&report.roles, sandboxes),
        render_pids(&report.media_utility_pids),
        report.remote
    ))
}

fn render_pids(pids: &[u32]) -> String {
    if pids.is_empty() {
        return "none".to_string();
    }
    pids.iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(".")
}

fn render_roles(roles: &[RoleReport], sandboxes: &[ProcessSandbox]) -> String {
    roles
        .iter()
        .map(|role| {
            let statuses = role
                .namespace_pids
                .iter()
                .filter_map(|pid| sandboxes.iter().find(|status| status.namespace_pid == *pid))
                .collect::<Vec<_>>();
            let no_new_privileges = statuses
                .iter()
                .map(|status| status.no_new_privileges)
                .min()
                .unwrap_or(0);
            let seccomp = statuses
                .iter()
                .map(|status| status.seccomp)
                .min()
                .unwrap_or(0);
            let filters = statuses
                .iter()
                .map(|status| status.filters)
                .min()
                .unwrap_or(0);
            format!(
                "{}:{}/{}/{}/{}",
                role.name,
                role.namespace_pids.len(),
                no_new_privileges,
                seccomp,
                filters
            )
        })
        .collect::<Vec<_>>()
        .join(",")
}

fn require_exact(name: &str, actual: &str, expected: &str) -> io::Result<()> {
    if actual == expected {
        return Ok(());
    }
    Err(unexpected(name, actual))
}

fn unexpected(name: &str, value: &str) -> io::Error {
    io::Error::other(format!(
        "Firefox returned an unexpected {name}: {}",
        printable(value)
    ))
}

fn printable(value: &str) -> String {
    value
        .chars()
        .take(512)
        .map(|character| {
            if character == ' ' || character.is_ascii_graphic() {
                character
            } else {
                '?'
            }
        })
        .collect()
}

fn contextual(context: &str, error: io::Error) -> io::Error {
    io::Error::new(error.kind(), format!("{context}: {error}"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use std::io::Cursor;

    struct ScriptedIo {
        input: Cursor<Vec<u8>>,
        output: Vec<u8>,
    }

    impl Read for ScriptedIo {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.input.read(buf)
        }
    }

    impl Write for ScriptedIo {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.output.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    const GOOD_REPORT: &str = "TD-FIREFOX-SUPPORT-V1|protocol=wayland|compositor=WebRender (Software)|adapter=llvmpipe|seccomp_bpf=true|seccomp_tsync=true|privileged_userns=false|userns=false|content_sandbox=true|media_sandbox=true|configured=6|effective=6|roles=content:11.12,gpu:,socket:13,rdd:14,utility:15|media=none|remote=rdd:1,socket:1,utility_jSOracle:1,web:2";

    fn good_sandboxes() -> Vec<ProcessSandbox> {
        [11, 12, 13, 14, 15]
            .into_iter()
            .map(|namespace_pid| ProcessSandbox {
                namespace_pid,
                no_new_privileges: 1,
                seccomp: 2,
                filters: 2,
            })
            .collect()
    }

    #[test]
    fn frame_codec_is_canonical_and_bounded() {
        let mut encoded = Vec::new();
        write_frame(&mut encoded, "[1,2,null,{}]").unwrap();
        assert_eq!(encoded, b"13:[1,2,null,{}]");
        assert_eq!(
            read_frame(&mut Cursor::new(encoded)).unwrap(),
            "[1,2,null,{}]"
        );

        for malformed in [
            b"0:".as_slice(),
            b"02:{}".as_slice(),
            b"x:{}".as_slice(),
            b"12345678:".as_slice(),
            b"1048577:".as_slice(),
            b"4:{}".as_slice(),
        ] {
            assert!(read_frame(&mut Cursor::new(malformed)).is_err());
        }
    }

    #[test]
    fn production_probe_has_one_end_to_end_deadline() {
        assert_eq!(PROBE_DEADLINE, Duration::from_secs(60));
        assert_eq!(
            remaining(Instant::now()).unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
    }

    #[test]
    fn report_script_is_one_bounded_privileged_snapshot() {
        let command = execute_command(REPORT_SCRIPT).unwrap();
        assert!(command.starts_with(r#"[0,3,"WebDriver:ExecuteAsyncScript",{"script":"#));
        assert!(command.contains("Troubleshoot.snapshot()"));
        assert!(command.contains("ChromeUtils.requestProcInfo()"));
        assert!(command.contains("Array.from(pids)"));
        assert!(command.contains("child.utilityActors || []"));
        assert!(command.contains("mediaUtilityActors.has"));
        assert!(command.contains("nonMediaUtilityActors.has"));
        assert!(command.contains("audioDecoder_Generic"));
        assert!(command.contains("contentTypes.has"));
        assert!(command.contains("unknown process type"));
        assert!(!REPORT_SCRIPT.contains("type.startsWith"));
        for process_type in [
            "web",
            "webIsolated",
            "file",
            "extension",
            "privilegedabout",
            "privilegedmozilla",
            "withCoopCoep",
            "webServiceWorker",
            "preallocated",
            "inference",
        ] {
            assert!(REPORT_SCRIPT.contains(&format!("\"{process_type}\"")));
        }
        for actor in [
            "jSOracle",
            "windowsUtils",
            "windowsFileDialog",
            "pkcs11Module",
        ] {
            assert!(REPORT_SCRIPT.contains(&format!("\"{actor}\"")));
        }
        assert_eq!(REPORT_SCRIPT.matches("type === \"utility\"").count(), 1);
        assert!(!REPORT_SCRIPT.contains("roles.has(type)"));
        assert!(command.contains("windowProtocol"));
        assert!(command.contains("windowLayerManagerType"));
        assert!(command.contains("code <= 0x7e"));
        assert!(REPORT_SCRIPT.contains("sanitized || \"missing\""));
        assert!(REPORT_SCRIPT.contains("remote || \"missing\""));
        assert!(command.len() < MAX_COMMAND_BYTES);
    }

    #[test]
    fn complete_protocol_transcript_is_exact_and_cleans_up() {
        let responses = [
            HELLO.to_string(),
            r#"[1,1,null,{"sessionId":"td","capabilities":{}}]"#.to_string(),
            SET_CONTEXT_RESPONSE.to_string(),
            format!("{EXECUTE_RESPONSE_PREFIX}{GOOD_REPORT}{EXECUTE_RESPONSE_SUFFIX}"),
            DELETE_SESSION_RESPONSE.to_string(),
        ];
        let mut input = Vec::new();
        for response in responses {
            write_frame(&mut input, &response).unwrap();
        }
        let mut io = ScriptedIo {
            input: Cursor::new(input),
            output: Vec::new(),
        };

        let report = probe_stream(&mut io).unwrap();
        assert!(validate_and_render(&report, &good_sandboxes())
            .unwrap()
            .starts_with("TD-FIREFOX-SUPPORT-OK protocol=wayland"));

        let mut commands = Cursor::new(io.output);
        assert_eq!(read_frame(&mut commands).unwrap(), NEW_SESSION);
        assert_eq!(read_frame(&mut commands).unwrap(), SET_CONTEXT);
        assert_eq!(
            read_frame(&mut commands).unwrap(),
            execute_command(REPORT_SCRIPT).unwrap()
        );
        assert_eq!(read_frame(&mut commands).unwrap(), DELETE_SESSION);
        assert!(read_frame(&mut commands).is_err());
    }

    #[test]
    fn exact_support_report_requires_nested_filters_for_live_roles() {
        let report = parse_report(GOOD_REPORT).unwrap();
        validate_report(&report, &good_sandboxes()).unwrap();

        for replacement in [
            ("protocol=wayland", "protocol=x11"),
            ("effective=6", "effective=0"),
            ("userns=false", "userns=true"),
            ("content:11.12", "content:"),
            ("rdd:14,utility:15", "rdd:,utility:"),
        ] {
            let altered = GOOD_REPORT.replace(replacement.0, replacement.1);
            let parsed = parse_report(&altered).unwrap();
            assert!(
                validate_report(&parsed, &good_sandboxes()).is_err(),
                "accepted {altered}"
            );
        }

        let mut weak = good_sandboxes();
        weak.get_mut(2).unwrap().filters = 1;
        assert!(validate_report(&report, &weak).is_err());

        let unrelated_utility = GOOD_REPORT.replace("rdd:14", "rdd:");
        assert!(validate_report(
            &parse_report(&unrelated_utility).unwrap(),
            &good_sandboxes(),
        )
        .is_err());

        let media_utility = unrelated_utility.replace("media=none", "media=15");
        validate_report(&parse_report(&media_utility).unwrap(), &good_sandboxes()).unwrap();
        assert!(parse_report(&media_utility.replace("media=15", "media=16")).is_ok());
        assert!(validate_report(
            &parse_report(&media_utility.replace("media=15", "media=16")).unwrap(),
            &good_sandboxes(),
        )
        .is_err());
        for malformed in ["media=0", "media=15.15", "media="] {
            assert!(parse_report(&GOOD_REPORT.replace("media=none", malformed)).is_err());
        }
    }

    #[test]
    fn report_parser_refuses_reordered_duplicate_and_escaped_fields() {
        assert!(parse_report(&GOOD_REPORT.replace(
            "|protocol=wayland|compositor=WebRender (Software)",
            "|compositor=WebRender (Software)|protocol=wayland"
        ))
        .is_err());
        assert!(parse_report(&GOOD_REPORT.replace("utility:15", "rdd:15")).is_err());
        let response =
            format!("{EXECUTE_RESPONSE_PREFIX}{GOOD_REPORT}\\n{EXECUTE_RESPONSE_SUFFIX}");
        let value = response
            .strip_prefix(EXECUTE_RESPONSE_PREFIX)
            .and_then(|rest| rest.strip_suffix(EXECUTE_RESPONSE_SUFFIX))
            .unwrap();
        assert!(value.contains('\\'));
    }
}
