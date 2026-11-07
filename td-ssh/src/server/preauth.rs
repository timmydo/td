//! The pre-auth worker: every byte an unauthenticated peer can send is parsed
//! here, in a jailed process that holds no host key (DESIGN.md section 4).
//!
//! The socket and the monitor's stream are `File`s because `File` issues the
//! `read` and `write` the seccomp allowlist admits, where `TcpStream` would use
//! `recv` and `send`.

use super::credentials;
use super::ipc::{self, TransportState};
use super::jail;
use super::log;
use crate::conn;
use crate::error::{Error, Result};
use crate::kex::{self, Kex, KexEnvironment, RandomSource, Role, DEFAULT_KEX, HOST_KEY_ALGORITHM};
use crate::msg;
use crate::packet::{exchange_versions, PacketReader, PacketWriter, IDENTIFICATION};
use crate::wire::{Reader, Writer};
use std::fs::File;
use std::mem::ManuallyDrop;
use std::os::fd::AsFd;

struct WorkerEnv<'a> {
    random: RandomSource<&'a File>,
    monitor: &'a File,
    host_key: Vec<u8>,
}

impl KexEnvironment for WorkerEnv<'_> {
    fn random(&mut self, out: &mut [u8]) -> Result<()> {
        self.random.fill(out)
    }

    fn host_key_blob(&mut self) -> Result<Vec<u8>> {
        Ok(self.host_key.clone())
    }

    fn sign(&mut self, hash: &[u8; 32]) -> Result<Vec<u8>> {
        let mut request = Writer::message(ipc::WORKER_SIGN);
        request.string(hash);
        ipc::write_frame(&mut self.monitor, request.as_slice())?;
        let reply = ipc::read_frame(&mut self.monitor)?;
        let mut reader = Reader::new(&reply);
        if reader.u8()? != ipc::MONITOR_SIGNATURE {
            return Err(Error::protocol("the monitor refused to sign"));
        }
        Ok(reader.string()?.to_vec())
    }

    fn check_host_key(&mut self, _blob: &[u8]) -> Result<()> {
        Err(Error::protocol("the server checks no host key"))
    }
}

fn duplicate(source: impl AsFd) -> std::io::Result<File> {
    source.as_fd().try_clone_to_owned().map(File::from)
}

/// Runs the worker. Everything fallible before the jail exits before a byte of
/// the peer's is read.
pub fn main(drop_identity: bool) -> i32 {
    match setup_and_run(drop_identity) {
        Ok(()) => 0,
        Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::UnexpectedEof => 0,
        // A peer's orderly goodbye is no error of anyone's.
        Err(Error::Disconnected(_)) => 0,
        Err(error) => {
            log::line(format_args!("td-sshd: pre-auth: {error}"));
            1
        }
    }
}

fn setup_and_run(drop_identity: bool) -> Result<()> {
    // Closed by the process ending and never before: a debug build of std checks a
    // descriptor with `fcntl` as it drops, which the filter would answer with
    // SIGSYS before a failure could be logged.
    let socket = ManuallyDrop::new(duplicate(std::io::stdin())?);
    let socket_writer = ManuallyDrop::new(socket.try_clone()?);
    let monitor = ManuallyDrop::new(duplicate(std::io::stdout())?);
    let random = ManuallyDrop::new(File::open("/dev/urandom")?);
    let status = credentials::open_status()?;
    jail::enter(status, drop_identity).map_err(Error::refused)?;

    let mut monitor_reader: &File = &monitor;
    let init = ipc::read_frame(&mut monitor_reader)?;
    let mut reader = Reader::new(&init);
    if reader.u8()? != ipc::MONITOR_INIT {
        return Err(Error::protocol("unexpected first frame from the monitor"));
    }
    let host_key = reader.string()?.to_vec();

    let client_version = exchange_versions(&mut &*socket, false)?;
    let mut env = WorkerEnv {
        random: RandomSource(&*random),
        monitor: &monitor,
        host_key,
    };
    let mut kex_engine = Kex::new(
        Role::Server,
        &client_version,
        IDENTIFICATION,
        DEFAULT_KEX.to_vec(),
    );
    let mut packet_reader = PacketReader::new(&*socket);
    let mut packet_writer = PacketWriter::new(&*socket_writer);
    let session_id = conn::initial_kex(
        &mut packet_reader,
        &mut packet_writer,
        &mut kex_engine,
        &mut env,
    )?;
    userauth(&mut packet_reader, &mut packet_writer, env.monitor)?;

    let state = TransportState {
        client_version,
        session_id,
        tx_key: packet_writer
            .key()
            .ok_or_else(|| Error::protocol("no transmit key"))?,
        rx_key: packet_reader
            .key()
            .ok_or_else(|| Error::protocol("no receive key"))?,
        tx: packet_writer.counters(),
        rx: packet_reader.counters(),
    };
    let mut frame = Writer::message(ipc::WORKER_AUTHENTICATED);
    state.encode(&mut frame);
    ipc::write_frame(&mut monitor_reader, frame.as_slice())?;
    Ok(())
}

fn failure(writer: &mut PacketWriter<&File>) -> Result<()> {
    let mut reply = Writer::message(msg::USERAUTH_FAILURE);
    reply.str("publickey").bool(false);
    writer.write_packet(reply.as_slice())
}

/// Asks the monitor one question. True means yes; a too-many answer ends the
/// connection with the same word OpenSSH's clients show.
fn ask_monitor(
    mut monitor: &File,
    writer: &mut PacketWriter<&File>,
    request: &[u8],
) -> Result<bool> {
    ipc::write_frame(&mut monitor, request)?;
    let reply = ipc::read_frame(&mut monitor)?;
    let mut reader = Reader::new(&reply);
    match reader.u8()? {
        ipc::MONITOR_ANSWER => reader.bool(),
        ipc::MONITOR_TOO_MANY => {
            let mut bye = Writer::message(msg::DISCONNECT);
            bye.u32(msg::DISCONNECT_NO_MORE_AUTH_METHODS)
                .str("Too many authentication failures")
                .str("");
            writer.write_packet(bye.as_slice())?;
            Err(Error::refused("too many authentication failures"))
        }
        _ => Err(Error::protocol("unexpected monitor answer")),
    }
}

/// The `ssh-userauth` service: only `none`, as the method probe, and `publickey`.
/// Returns once the monitor has accepted and the client has been told.
fn userauth(
    reader: &mut PacketReader<&File>,
    writer: &mut PacketWriter<&File>,
    monitor: &File,
) -> Result<()> {
    let mut service_requested = false;
    loop {
        let payload = reader.read_packet()?;
        let number = payload.first().copied().unwrap_or(0);
        let mut message = Reader::new(payload.get(1..).unwrap_or_default());
        match number {
            msg::IGNORE | msg::DEBUG | msg::EXT_INFO | msg::UNIMPLEMENTED => {}
            msg::DISCONNECT => return Err(Error::Disconnected(String::new())),
            msg::SERVICE_REQUEST => {
                if message.str()? != "ssh-userauth" || service_requested {
                    return Err(Error::protocol("unexpected service request"));
                }
                service_requested = true;
                let mut accept = Writer::message(msg::SERVICE_ACCEPT);
                accept.str("ssh-userauth");
                writer.write_packet(accept.as_slice())?;
            }
            msg::USERAUTH_REQUEST if service_requested => {
                let user = message.str()?;
                let service = message.str()?;
                let method = message.str()?;
                if service != "ssh-connection" || method != "publickey" {
                    failure(writer)?;
                    continue;
                }
                let has_signature = message.bool()?;
                let algorithm = message.str()?;
                let key = message.string()?;
                if algorithm != HOST_KEY_ALGORITHM || kex::parse_ed25519_public(key).is_err() {
                    failure(writer)?;
                    continue;
                }
                if has_signature {
                    let signature = message.string()?;
                    let mut request = Writer::message(ipc::WORKER_AUTHENTICATE);
                    request.str(user).string(key).string(signature);
                    if ask_monitor(monitor, writer, request.as_slice())? {
                        writer.write_packet(&[msg::USERAUTH_SUCCESS])?;
                        return Ok(());
                    }
                    failure(writer)?;
                } else {
                    let mut request = Writer::message(ipc::WORKER_KEY_ACCEPTABLE);
                    request.str(user).string(key);
                    if ask_monitor(monitor, writer, request.as_slice())? {
                        let mut ok = Writer::message(msg::USERAUTH_PK_OK);
                        ok.str(algorithm).string(key);
                        writer.write_packet(ok.as_slice())?;
                    } else {
                        failure(writer)?;
                    }
                }
            }
            msg::KEXINIT => return Err(Error::protocol("rekey before authentication")),
            // RFC 4253 section 11.4, as for any message this side has no use for.
            _ => {
                let mut reply = Writer::message(msg::UNIMPLEMENTED);
                reply.u32(reader.counters().seq.wrapping_sub(1));
                writer.write_packet(reply.as_slice())?;
            }
        }
    }
}
