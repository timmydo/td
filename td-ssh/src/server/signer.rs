//! Holds the host key after authentication and answers rekeys' signature
//! requests from the session, in the same jail as the worker. Rate limited,
//! because the oracle it exposes is only for rekeying (DESIGN.md section 4).

use super::credentials;
use super::ipc;
use super::jail;
use super::log;
use crate::error::{Error, Result};
use crate::kex;
use crate::keyfile::PrivateKey;
use crate::wire::{Reader, Writer};
use std::fs::File;
use std::io::Read;
use std::mem::ManuallyDrop;
use std::os::fd::AsFd;
use std::time::{Duration, Instant};

const MIN_INTERVAL: Duration = Duration::from_secs(1);

pub fn main(drop_identity: bool) -> i32 {
    match run(drop_identity) {
        Ok(()) => 0,
        Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::UnexpectedEof => 0,
        Err(error) => {
            log::line(format_args!("td-sshd: signer: {error}"));
            1
        }
    }
}

fn run(drop_identity: bool) -> Result<()> {
    let mut key_pipe = File::from(std::io::stdin().as_fd().try_clone_to_owned()?);
    // Closed by the process ending: see preauth.rs on std and the filter.
    let mut session =
        ManuallyDrop::new(File::from(std::io::stdout().as_fd().try_clone_to_owned()?));
    let status = credentials::open_status()?;
    let mut seed = [0u8; 32];
    key_pipe.read_exact(&mut seed)?;
    let key = PrivateKey::from_seed(&seed, "");
    td_primitives::ct::zero(&mut seed);
    let key = key?;
    drop(key_pipe);
    jail::enter(status, drop_identity).map_err(Error::refused)?;

    let mut last: Option<Instant> = None;
    loop {
        let frame = ipc::read_frame(&mut *session)?;
        let mut reader = Reader::new(&frame);
        let hash: [u8; 32] = reader.array()?;
        reader.finish()?;
        if let Some(wait) = last.and_then(|at| MIN_INTERVAL.checked_sub(at.elapsed())) {
            std::thread::sleep(wait);
        }
        last = Some(Instant::now());
        let signature = kex::ed25519_signature_blob(&key.sign(&hash)?);
        let mut reply = Writer::new();
        reply.string(&signature);
        ipc::write_frame(&mut *session, reply.as_slice())?;
    }
}
