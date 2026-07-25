// td-sshd — a minimal, source-built russh SSH daemon for the td x86-64 image,
// plus a self-contained `selftest` that doubles as the boot oracle.
//
//   sshd serve    [--listen ADDR] [--host-key PATH] [--authorized-keys PATH]
//   sshd selftest
//
// `serve` runs the real daemon: it accepts SSH connections, authorizes ONLY the
// public keys listed in --authorized-keys (a missing/empty file => deny all),
// and runs each `exec` request through /bin/sh -c, returning its stdout, stderr,
// and exit status. `selftest` stands up an in-process server on an ephemeral
// 127.0.0.1 port and connects a client to it over real loopback TCP, proving the
// kernel's TCP/IP stack, the russh handshake+auth+channel+exec path, and the
// shipped runtime closure all work — it prints the boot marker on success and
// touches neither /bin/sh nor the shipped authorized-keys file.
use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use russh::keys::*;
use russh::server::{Msg, Server as _, Session};
use russh::*;
use tokio::net::TcpListener;

// The marker `selftest` prints on success. Kept in sync with
// recipes/src/ladder.rs SSHD_MARKER, which the qemu boot oracle greps for.
const OK_MARKER: &str = "TD-SSHD-OK";

// A fixed unencrypted ed25519 key. It is used as (a) the deny-all host key for
// `serve` ONLY when no --host-key is given AND no client can authenticate (empty
// authorized-keys, see `serve`), and (b) BOTH endpoints' identity in the
// in-process `selftest`. It is PUBLIC (committed here), so it is deliberately NOT
// an access credential: the real daemon authorizes only --authorized-keys, and
// this key is never written there. Because it is public it must not serve as a
// host identity a client could trust, so `serve` refuses to fall back to it once
// authorized keys exist. Per-machine persisted host keys are the follow-up.
const BUILTIN_KEY: &str = "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW\nQyNTUxOQAAACCIPJHhaH8qIsFU2QJi0O7p3lKaZnJq8tbL/8CtmQ0wrwAAAJCaC52Mmgud\njAAAAAtzc2gtZWQyNTUxOQAAACCIPJHhaH8qIsFU2QJi0O7p3lKaZnJq8tbL/8CtmQ0wrw\nAAAEAbUmkQe16m+pWjFZz5pn7XbR4ciX0nger8vt4v9H/LPIg8keFofyoiwVTZAmLQ7une\nUppmcmry1sv/wK2ZDTCvAAAADXRkLXJ1c3NoLXRlc3Q=\n-----END OPENSSH PRIVATE KEY-----\n";

// One connection's server state: the SHA-256 fingerprints of the authorized
// public keys and whether an exec runs a real shell (`serve`) or the builtin
// echo (`selftest`, which must not depend on /bin/sh existing). Fingerprints,
// not openssh lines: the key presented during auth carries no comment, so a
// line comparison against a commented authorized_keys entry would spuriously
// fail.
#[derive(Clone)]
struct Srv {
    authorized: Arc<HashSet<ssh_key::Fingerprint>>,
    use_shell: bool,
}

impl server::Server for Srv {
    type Handler = Srv;
    fn new_client(&mut self, _: Option<SocketAddr>) -> Srv {
        self.clone()
    }
}

impl server::Handler for Srv {
    type Error = russh::Error;

    // Key-only authorization for a single-user appliance admin service: possession
    // of an authorized private key IS the grant, so `_user` is intentionally
    // ignored (any username presenting an authorized key is accepted, like
    // `PermitRootLogin` + a single admin `AuthorizedKeysFile`). `serve` runs under
    // init as root and does not drop privileges, so an authorized client is by
    // design a root administrator; per-account resolution/privsep is a follow-up
    // if this ever grows multi-user. The shipped image authorizes NO key (deny-all
    // `authorized_keys`), so nothing is exposed until an operator explicitly
    // grants that admin access — at which point the host-key guard in `serve`
    // forces a real per-machine host key.
    async fn auth_publickey(
        &mut self,
        _user: &str,
        key: &ssh_key::PublicKey,
    ) -> Result<server::Auth, Self::Error> {
        let accepted = self
            .authorized
            .contains(&key.fingerprint(HashAlg::Sha256));
        if accepted {
            Ok(server::Auth::Accept)
        } else {
            Ok(server::Auth::Reject {
                proceed_with_methods: None,
                partial_success: false,
            })
        }
    }

    async fn channel_open_session(
        &mut self,
        _c: Channel<Msg>,
        _s: &mut Session,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }

    async fn exec_request(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let cmd = String::from_utf8_lossy(data).into_owned();
        session.channel_success(channel)?;
        if self.use_shell {
            // Real daemon: run the request through /bin/sh -c, then return its
            // stdout, stderr (SSH extended-data channel 1, not merged into
            // stdout), and exit code. Output is collected then sent — adequate for
            // short admin commands; incremental streaming is a follow-up if
            // long-running exec is ever needed.
            match tokio::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(&cmd)
                .output()
                .await
            {
                Ok(out) => {
                    if !out.stdout.is_empty() {
                        session.data(channel, out.stdout)?;
                    }
                    if !out.stderr.is_empty() {
                        session.extended_data(channel, 1, out.stderr)?;
                    }
                    let code = out.status.code().unwrap_or(1);
                    session.exit_status_request(channel, code as u32)?;
                }
                Err(e) => {
                    session.data(channel, format!("td-sshd: exec failed: {e}\n").into_bytes())?;
                    session.exit_status_request(channel, 127)?;
                }
            }
        } else {
            // selftest: builtin echo so the probe needs no shell in the closure.
            session.data(channel, format!("td-sshd-ok: {cmd}\n").into_bytes())?;
            session.exit_status_request(channel, 0)?;
        }
        session.eof(channel)?;
        session.close(channel)?;
        Ok(())
    }
}

struct Cli;
impl client::Handler for Cli {
    type Error = russh::Error;
    async fn check_server_key(&mut self, _k: &ssh_key::PublicKey) -> Result<bool, Self::Error> {
        Ok(true)
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("serve") | None => serve(args.get(2..).unwrap_or(&[])).await,
        Some("selftest") => selftest().await,
        Some("-h") | Some("--help") | Some("help") => {
            print_usage();
            Ok(())
        }
        Some(other) => {
            print_usage();
            bail!("td-sshd: unknown mode `{other}` (want serve|selftest)");
        }
    }
}

fn print_usage() {
    eprintln!(
        "usage:\n  sshd serve [--listen ADDR] [--host-key PATH] [--authorized-keys PATH]\n  sshd selftest"
    );
}

async fn serve(args: &[String]) -> Result<()> {
    let mut listen = "0.0.0.0:22".to_string();
    let mut host_key_path: Option<String> = None;
    let mut authorized_path: Option<String> = None;
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        let val = || -> Result<String> {
            it.clone()
                .next()
                .cloned()
                .with_context(|| format!("flag `{flag}` needs a value"))
        };
        match flag.as_str() {
            "--listen" => {
                listen = val()?;
                it.next();
            }
            "--host-key" => {
                host_key_path = Some(val()?);
                it.next();
            }
            "--authorized-keys" => {
                authorized_path = Some(val()?);
                it.next();
            }
            other => bail!("td-sshd serve: unknown flag `{other}`"),
        }
    }

    let authorized = Arc::new(load_authorized_keys(authorized_path.as_deref())?);
    let host_key = match &host_key_path {
        Some(p) => {
            let pem = std::fs::read_to_string(p).with_context(|| format!("read host key {p}"))?;
            PrivateKey::from_openssh(pem).with_context(|| format!("parse host key {p}"))?
        }
        // The builtin key is public; using it as a host identity lets anyone with
        // repo access impersonate this server. Permit it only in the deny-all
        // config (no authorized keys => no client can finish auth, nothing to
        // MITM). The moment real keys are configured, refuse to start rather than
        // serve a public host identity — supply a per-machine --host-key instead.
        None if authorized.is_empty() => {
            PrivateKey::from_openssh(BUILTIN_KEY).context("parse builtin host key")?
        }
        None => bail!(
            "td-sshd serve: refusing the public builtin host key while {} authorized \
             key(s) are configured; pass --host-key with a per-machine key",
            authorized.len()
        ),
    };

    let cfg = Arc::new(server::Config {
        keys: vec![host_key],
        ..Default::default()
    });
    let listener = TcpListener::bind(&listen)
        .await
        .with_context(|| format!("bind {listen}"))?;
    eprintln!(
        "td-sshd: listening on {listen} ({} authorized key(s))",
        authorized.len()
    );
    let mut srv = Srv {
        authorized,
        use_shell: true,
    };
    srv.run_on_socket(cfg, &listener).await?;
    Ok(())
}

// Parse an OpenSSH authorized_keys file into normalized openssh public-key
// lines. A missing path or missing file yields an empty set (deny all) rather
// than an error, so the daemon fails closed on a fresh machine.
fn load_authorized_keys(path: Option<&str>) -> Result<HashSet<ssh_key::Fingerprint>> {
    let mut set = HashSet::new();
    let Some(path) = path else {
        return Ok(set);
    };
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(set),
        Err(e) => return Err(e).with_context(|| format!("read authorized_keys {path}")),
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // Skip (don't hard-fail on) a malformed line: this daemon is init-
        // respawned, so aborting startup over one bad entry would spin a respawn
        // loop and lock out the good keys in the same file.
        match ssh_key::PublicKey::from_openssh(line) {
            Ok(key) => {
                set.insert(key.fingerprint(HashAlg::Sha256));
            }
            Err(e) => eprintln!("td-sshd: skipping malformed key in {path}: {e}"),
        }
    }
    Ok(set)
}

// Stand up an in-process server on an ephemeral loopback port, connect a client
// to it over real TCP, authenticate by public key, exec a probe, and check the
// reply. Exercises kernel TCP/IP + the full russh stack + the runtime closure
// without any external process, shell, or shipped credential.
async fn selftest() -> Result<()> {
    let key = PrivateKey::from_openssh(BUILTIN_KEY).context("parse builtin key")?;
    let mut authorized = HashSet::new();
    authorized.insert(key.public_key().fingerprint(HashAlg::Sha256));

    let cfg = Arc::new(server::Config {
        keys: vec![key.clone()],
        ..Default::default()
    });
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .context("bind loopback")?;
    let addr = listener.local_addr().context("local_addr")?;
    let mut srv = Srv {
        authorized: Arc::new(authorized),
        use_shell: false,
    };
    tokio::spawn(async move {
        let _ = srv.run_on_socket(cfg, &listener).await;
    });

    let ccfg = Arc::new(client::Config::default());
    let mut session = client::connect(ccfg, addr, Cli)
        .await
        .context("client connect")?;
    let auth = session
        .authenticate_publickey(
            "td",
            PrivateKeyWithHashAlg::new(
                Arc::new(key),
                session.best_supported_rsa_hash().await?.flatten(),
            ),
        )
        .await?;
    if !auth.success() {
        bail!("selftest: public-key auth failed");
    }

    let mut channel = session.channel_open_session().await.context("open session")?;
    channel.exec(true, "ping").await.context("exec")?;
    let mut out = Vec::new();
    let mut code = None;
    while let Some(msg) = channel.wait().await {
        match msg {
            ChannelMsg::Data { ref data } => out.extend_from_slice(data),
            ChannelMsg::ExitStatus { exit_status } => code = Some(exit_status),
            _ => {}
        }
    }
    let text = String::from_utf8_lossy(&out);
    // Require BOTH the expected reply AND a clean remote exit: a partial or failed
    // exec that happened to echo the text (or reported no status at all) must not
    // pass the boot oracle.
    if !text.contains("td-sshd-ok: ping") || code != Some(0) {
        bail!("selftest: unexpected reply {text:?} (exit={code:?})");
    }
    // The boot oracle greps stdout for this line.
    println!("{OK_MARKER}");
    Ok(())
}
