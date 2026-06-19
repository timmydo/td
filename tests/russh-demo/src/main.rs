// td-russh-demo — a self-contained russh client<->server round-trip over loopback:
// start an SSH server on 127.0.0.1, connect a client, authenticate by public key,
// exec a command, read the server's reply. Proves russh's SSH handshake + auth +
// channel + exec all work end to end (no external sshd). Prints the reply.
use std::sync::Arc;
use anyhow::Result;
use russh::keys::*;
use russh::server::{Msg, Server as _, Session};
use russh::*;
use tokio::net::TcpListener;

const TEST_KEY: &str = "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW\nQyNTUxOQAAACCIPJHhaH8qIsFU2QJi0O7p3lKaZnJq8tbL/8CtmQ0wrwAAAJCaC52Mmgud\njAAAAAtzc2gtZWQyNTUxOQAAACCIPJHhaH8qIsFU2QJi0O7p3lKaZnJq8tbL/8CtmQ0wrw\nAAAEAbUmkQe16m+pWjFZz5pn7XbR4ciX0nger8vt4v9H/LPIg8keFofyoiwVTZAmLQ7une\nUppmcmry1sv/wK2ZDTCvAAAADXRkLXJ1c3NoLXRlc3Q=\n-----END OPENSSH PRIVATE KEY-----\n";

#[derive(Clone)]
struct Srv;
impl server::Server for Srv {
    type Handler = Srv;
    fn new_client(&mut self, _: Option<std::net::SocketAddr>) -> Srv { Srv }
}
impl server::Handler for Srv {
    type Error = russh::Error;
    async fn auth_publickey(&mut self, _u: &str, _k: &ssh_key::PublicKey)
        -> Result<server::Auth, Self::Error> { Ok(server::Auth::Accept) }
    async fn channel_open_session(&mut self, _c: Channel<Msg>, _s: &mut Session)
        -> Result<bool, Self::Error> { Ok(true) }
    async fn exec_request(&mut self, channel: ChannelId, data: &[u8], session: &mut Session)
        -> Result<(), Self::Error> {
        let cmd = String::from_utf8_lossy(data);
        let out = format!("td-russh-ok: {cmd}\n");
        session.channel_success(channel)?;
        session.data(channel, out.into_bytes())?;
        session.exit_status_request(channel, 0)?;
        session.eof(channel)?;
        session.close(channel)?;
        Ok(())
    }
}

struct Cli;
impl client::Handler for Cli {
    type Error = russh::Error;
    async fn check_server_key(&mut self, _k: &ssh_key::PublicKey) -> Result<bool, Self::Error> { Ok(true) }
}

#[tokio::main]
async fn main() -> Result<()> {
    let key = russh::keys::PrivateKey::from_openssh(TEST_KEY)?;
    let server_key = key.clone();
    let scfg = Arc::new(server::Config {
        keys: vec![server_key],
        ..Default::default()
    });
    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let addr = listener.local_addr()?;
    let mut srv = Srv;
    tokio::spawn(async move { let _ = srv.run_on_socket(scfg, &listener).await; });

    let ckey = key.clone();
    let ccfg = Arc::new(client::Config::default());
    let mut session = client::connect(ccfg, addr, Cli).await?;
    let ok = session.authenticate_publickey(
        "td",
        PrivateKeyWithHashAlg::new(Arc::new(ckey), session.best_supported_rsa_hash().await?.flatten()),
    ).await?;
    if !ok.success() { anyhow::bail!("auth failed"); }

    let mut channel = session.channel_open_session().await?;
    channel.exec(true, "ping").await?;
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
    print!("{text}");
    eprintln!("exit={code:?}");
    if !text.contains("td-russh-ok: ping") { anyhow::bail!("unexpected reply: {text}"); }
    Ok(())
}
