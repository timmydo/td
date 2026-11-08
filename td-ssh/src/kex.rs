//! Algorithm negotiation and key exchange as a state machine both roles and
//! both connection stages share (DESIGN.md section 2). It sends nothing itself:
//! each step returns what to send and which keys to install, so the blocking
//! pre-auth worker and the threaded post-auth loop drive the same code.

use crate::error::{peer_text, Error, Result};
use crate::msg;
use crate::packet::TransportKey;
use crate::wire::{Reader, Writer};
use std::io::Read;
use td_primitives::curve25519::{ed25519, x25519};
use td_primitives::mlkem768;
use td_primitives::sha::{Digest, Sha256};

pub const KEX_MLKEM: &str = "mlkem768x25519-sha256";
pub const KEX_CURVE: &str = "curve25519-sha256";
pub const HOST_KEY_ALGORITHM: &str = "ssh-ed25519";
pub const CIPHER: &str = "chacha20-poly1305@openssh.com";
/// Named so a strict peer sees an intersection; the AEAD means it is never
/// computed (DESIGN.md section 2).
pub const MAC_PLACEHOLDER: &str = "hmac-sha2-256-etm@openssh.com";
pub const COMPRESSION: &str = "none";

const STRICT_CLIENT: &str = "kex-strict-c-v00@openssh.com";
const STRICT_SERVER: &str = "kex-strict-s-v00@openssh.com";
const HYBRID_CLIENT_LEN: usize = mlkem768::EK_LEN + 32;
const HYBRID_SERVER_LEN: usize = mlkem768::CT_LEN + 32;

pub const DEFAULT_KEX: &[&str] = &[KEX_MLKEM, KEX_CURVE];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Client,
    Server,
}

/// What a key exchange needs from its role: entropy, then either the host key
/// signature or the decision about the host key the peer presented.
pub trait KexEnvironment {
    fn random(&mut self, out: &mut [u8]) -> Result<()>;
    /// Server: the public host key blob.
    fn host_key_blob(&mut self) -> Result<Vec<u8>>;
    /// Server: an `ssh-ed25519` signature blob over the exchange hash.
    fn sign(&mut self, hash: &[u8; 32]) -> Result<Vec<u8>>;
    /// Client: decides whether this host key may speak for the destination,
    /// before its signature is checked or any secret is derived from it.
    fn check_host_key(&mut self, blob: &[u8]) -> Result<()>;
    /// Client: that key's signature over the exchange verified, so it is safe
    /// to remember. Nothing a peer merely sent may reach a file.
    fn host_key_verified(&mut self) {}
}

/// Read entropy from an open descriptor: the worker's chroot has no devices.
pub struct RandomSource<R>(pub R);

impl<R: Read> RandomSource<R> {
    pub fn fill(&mut self, out: &mut [u8]) -> Result<()> {
        self.0
            .read_exact(out)
            .map_err(|error| Error::refused(format!("reading entropy: {error}")))
    }
}

#[derive(Default)]
pub struct KexOutput {
    /// In order. A `NEWKEYS` is always last, so install `tx_key` after sending.
    pub send: Vec<Vec<u8>>,
    pub tx_key: Option<TransportKey>,
    pub rx_key: Option<TransportKey>,
    /// The exchange finished: traffic that was held back may flow.
    pub finished: bool,
}

/// A 32-byte secret, wiped however its scope ends.
pub(crate) struct Secret32(pub(crate) [u8; 32]);

impl Drop for Secret32 {
    fn drop(&mut self) {
        td_primitives::ct::zero(&mut self.0);
    }
}

/// A client's ephemeral secrets, which live only until the reply arrives.
enum Ephemeral {
    Curve {
        secret: [u8; 32],
        public: [u8; 32],
    },
    Hybrid {
        decapsulation: Vec<u8>,
        secret: [u8; 32],
        public: Vec<u8>,
    },
}

impl Ephemeral {
    fn wipe(&mut self) {
        match self {
            Self::Curve { secret, .. } => td_primitives::ct::zero(secret),
            Self::Hybrid {
                decapsulation,
                secret,
                ..
            } => {
                td_primitives::ct::zero(decapsulation);
                td_primitives::ct::zero(secret);
            }
        }
    }
}

impl Drop for Ephemeral {
    fn drop(&mut self) {
        self.wipe();
    }
}

enum State {
    Idle,
    /// Our KEXINIT is out; the peer's is awaited.
    SentInit,
    ClientAwaitReply {
        algorithm: &'static str,
        ephemeral: Ephemeral,
    },
    ServerAwaitInit {
        algorithm: &'static str,
    },
    AwaitNewKeys {
        rx_key: TransportKey,
    },
}

pub struct Kex {
    role: Role,
    client_version: Vec<u8>,
    server_version: Vec<u8>,
    algorithms: Vec<&'static str>,
    state: State,
    negotiated: Option<&'static str>,
    local_init: Vec<u8>,
    peer_init: Vec<u8>,
    session_id: Option<[u8; 32]>,
    skip_next: bool,
    peer_init_seen: bool,
}

impl Kex {
    pub fn new(
        role: Role,
        client_version: &str,
        server_version: &str,
        algorithms: Vec<&'static str>,
    ) -> Self {
        Self {
            role,
            client_version: client_version.as_bytes().to_vec(),
            server_version: server_version.as_bytes().to_vec(),
            algorithms,
            state: State::Idle,
            negotiated: None,
            local_init: Vec::new(),
            peer_init: Vec::new(),
            session_id: None,
            skip_next: false,
            peer_init_seen: false,
        }
    }

    /// Resumes for rekeying, in a process other than the one that negotiated.
    pub fn resume(
        role: Role,
        client_version: &str,
        server_version: &str,
        session_id: [u8; 32],
    ) -> Self {
        let mut kex = Self::new(role, client_version, server_version, DEFAULT_KEX.to_vec());
        kex.session_id = Some(session_id);
        kex
    }

    pub fn session_id(&self) -> Option<[u8; 32]> {
        self.session_id
    }

    pub fn in_progress(&self) -> bool {
        !matches!(self.state, State::Idle)
    }

    /// True from the peer's KEXINIT to the end: only KEX traffic may arrive.
    pub fn peer_committed(&self) -> bool {
        self.peer_init_seen
    }

    /// What the latest exchange settled on, once both lists are in.
    pub fn negotiated(&self) -> Option<&'static str> {
        self.negotiated
    }

    /// Whether strict rules cover this exchange: the first, before any key.
    pub fn is_initial(&self) -> bool {
        self.session_id.is_none()
    }

    /// Starts an exchange: our KEXINIT.
    pub fn start(&mut self, env: &mut impl KexEnvironment) -> Result<Vec<u8>> {
        if self.in_progress() {
            return Err(Error::protocol("key exchange already in progress"));
        }
        let mut cookie = [0u8; 16];
        env.random(&mut cookie)?;
        let mut kex_names: Vec<&str> = self.algorithms.clone();
        // Advertised only in the first KEXINIT of a connection, as OpenSSH does.
        if self.session_id.is_none() {
            kex_names.push(match self.role {
                Role::Client => STRICT_CLIENT,
                Role::Server => STRICT_SERVER,
            });
        }
        let mut writer = Writer::message(msg::KEXINIT);
        writer.raw(&cookie);
        writer.name_list(&kex_names);
        writer.str(HOST_KEY_ALGORITHM);
        writer.str(CIPHER).str(CIPHER);
        writer.str(MAC_PLACEHOLDER).str(MAC_PLACEHOLDER);
        writer.str(COMPRESSION).str(COMPRESSION);
        writer.str("").str("");
        writer.bool(false);
        writer.u32(0);
        self.local_init = writer.into_vec();
        self.state = State::SentInit;
        Ok(self.local_init.clone())
    }

    /// Consumes one KEX message: KEXINIT, NEWKEYS or an ECDH message.
    pub fn on_message(
        &mut self,
        payload: &[u8],
        env: &mut impl KexEnvironment,
    ) -> Result<KexOutput> {
        let number = payload
            .first()
            .copied()
            .ok_or_else(|| Error::protocol("empty message"))?;
        if self.skip_next && number != msg::KEXINIT && number != msg::NEWKEYS {
            // RFC 4253 section 7: the peer's guessed packet, when the guess lost.
            self.skip_next = false;
            return Ok(KexOutput::default());
        }
        match number {
            msg::KEXINIT => self.on_kexinit(payload, env),
            msg::KEX_ECDH_INIT if self.role == Role::Server => self.on_ecdh_init(payload, env),
            msg::KEX_ECDH_REPLY if self.role == Role::Client => self.on_ecdh_reply(payload, env),
            msg::NEWKEYS => self.on_newkeys(),
            _ => Err(Error::protocol("unexpected key exchange message")),
        }
    }

    fn on_kexinit(&mut self, payload: &[u8], env: &mut impl KexEnvironment) -> Result<KexOutput> {
        if self.peer_init_seen {
            return Err(Error::protocol("second KEXINIT during key exchange"));
        }
        let mut output = KexOutput::default();
        if matches!(self.state, State::Idle) {
            output.send.push(self.start(env)?);
        }
        if !matches!(self.state, State::SentInit) {
            return Err(Error::protocol("KEXINIT out of order"));
        }
        self.peer_init = payload.to_vec();
        self.peer_init_seen = true;
        let peer = ParsedInit::parse(payload)?;
        if self.session_id.is_none() {
            let wanted = match self.role {
                Role::Client => STRICT_SERVER,
                Role::Server => STRICT_CLIENT,
            };
            if !peer.kex.contains(&wanted) {
                return Err(Error::refused(
                    "peer does not support strict key exchange (OpenSSH 9.6 or newer is required)",
                ));
            }
        }
        let algorithm = self.negotiate(&peer)?;
        self.negotiated = Some(algorithm);
        if !peer.host_key.contains(&HOST_KEY_ALGORITHM) {
            return Err(Error::refused(format!(
                "no matching host key type: peer offers {}",
                offered(&peer.host_key)
            )));
        }
        if !peer.cipher_c2s.contains(&CIPHER) || !peer.cipher_s2c.contains(&CIPHER) {
            return Err(Error::refused(format!(
                "no matching cipher: peer offers {}",
                offered(&peer.cipher_c2s)
            )));
        }
        if !peer.compression_c2s.contains(&COMPRESSION)
            || !peer.compression_s2c.contains(&COMPRESSION)
        {
            return Err(Error::refused("peer requires compression"));
        }
        // RFC 4253 section 7: a guess is right only if both sides' first entries
        // agree in both lists, whatever negotiation then settled on.
        self.skip_next = peer.first_follows
            && (peer.kex.first().copied() != self.algorithms.first().copied()
                || peer.host_key.first() != Some(&HOST_KEY_ALGORITHM));
        match self.role {
            Role::Client => {
                let ephemeral = make_ephemeral(algorithm, env)?;
                let mut writer = Writer::message(msg::KEX_ECDH_INIT);
                match &ephemeral {
                    Ephemeral::Curve { public, .. } => writer.string(public),
                    Ephemeral::Hybrid { public, .. } => writer.string(public),
                };
                output.send.push(writer.into_vec());
                self.state = State::ClientAwaitReply {
                    algorithm,
                    ephemeral,
                };
            }
            Role::Server => {
                self.state = State::ServerAwaitInit { algorithm };
            }
        }
        Ok(output)
    }

    /// RFC 4253 section 7.1: the client's preference, among what both offer.
    fn negotiate(&self, peer: &ParsedInit<'_>) -> Result<&'static str> {
        let found = match self.role {
            Role::Client => self
                .algorithms
                .iter()
                .find(|name| peer.kex.contains(name))
                .copied(),
            Role::Server => peer
                .kex
                .iter()
                .find_map(|name| self.algorithms.iter().find(|local| *local == name).copied()),
        };
        found.ok_or_else(|| {
            Error::refused(format!(
                "no matching key exchange method: peer offers {}",
                offered(&peer.kex)
            ))
        })
    }

    fn on_ecdh_init(&mut self, payload: &[u8], env: &mut impl KexEnvironment) -> Result<KexOutput> {
        let State::ServerAwaitInit { algorithm } = self.state else {
            return Err(Error::protocol("KEX_ECDH_INIT out of order"));
        };
        let mut reader = Reader::new(payload.get(1..).unwrap_or_default());
        let client_public = reader.string()?;
        reader.finish()?;
        let (server_public, secret) = server_agree(algorithm, client_public, env)?;
        let host_key = env.host_key_blob()?;
        let hash = self.exchange_hash(&host_key, client_public, &server_public, &secret);
        let signature = env.sign(&hash)?;
        let session_id = *self.session_id.get_or_insert(hash);
        let keys = derive_keys(&secret, &hash, &session_id);

        let mut reply = Writer::message(msg::KEX_ECDH_REPLY);
        reply
            .string(&host_key)
            .string(&server_public)
            .string(&signature);
        let output = KexOutput {
            send: vec![reply.into_vec(), Writer::message(msg::NEWKEYS).into_vec()],
            tx_key: Some(keys.server_to_client),
            rx_key: None,
            finished: false,
        };
        self.state = State::AwaitNewKeys {
            rx_key: keys.client_to_server,
        };
        Ok(output)
    }

    fn on_ecdh_reply(
        &mut self,
        payload: &[u8],
        env: &mut impl KexEnvironment,
    ) -> Result<KexOutput> {
        let State::ClientAwaitReply {
            algorithm,
            ephemeral,
        } = std::mem::replace(&mut self.state, State::Idle)
        else {
            return Err(Error::protocol("KEX_ECDH_REPLY out of order"));
        };
        let mut reader = Reader::new(payload.get(1..).unwrap_or_default());
        let host_key = reader.string()?;
        let server_public = reader.string()?;
        let signature = reader.string()?;
        reader.finish()?;
        env.check_host_key(host_key)?;
        let public_key = parse_ed25519_public(host_key)?;
        let signature = parse_ed25519_signature(signature)?;
        let client_public: &[u8] = match &ephemeral {
            Ephemeral::Curve { public, .. } => public,
            Ephemeral::Hybrid { public, .. } => public,
        };
        let secret = client_agree(algorithm, &ephemeral, server_public)?;
        let hash = self.exchange_hash(host_key, client_public, server_public, &secret);
        if ed25519::verify(&public_key, &hash, &signature).is_err() {
            return Err(Error::protocol("host key signature is invalid"));
        }
        env.host_key_verified();
        let session_id = *self.session_id.get_or_insert(hash);
        let keys = derive_keys(&secret, &hash, &session_id);
        let output = KexOutput {
            send: vec![Writer::message(msg::NEWKEYS).into_vec()],
            tx_key: Some(keys.client_to_server),
            rx_key: None,
            finished: false,
        };
        self.state = State::AwaitNewKeys {
            rx_key: keys.server_to_client,
        };
        Ok(output)
    }

    fn on_newkeys(&mut self) -> Result<KexOutput> {
        if !matches!(self.state, State::AwaitNewKeys { .. }) {
            return Err(Error::protocol("NEWKEYS out of order"));
        }
        let State::AwaitNewKeys { rx_key } = std::mem::replace(&mut self.state, State::Idle) else {
            return Err(Error::protocol("NEWKEYS out of order"));
        };
        self.peer_init_seen = false;
        self.skip_next = false;
        self.local_init.clear();
        self.peer_init.clear();
        Ok(KexOutput {
            send: Vec::new(),
            tx_key: None,
            rx_key: Some(rx_key),
            finished: true,
        })
    }

    fn exchange_hash(
        &self,
        host_key: &[u8],
        client_public: &[u8],
        server_public: &[u8],
        secret: &SharedSecret,
    ) -> [u8; 32] {
        let (client_init, server_init) = match self.role {
            Role::Client => (&self.local_init, &self.peer_init),
            Role::Server => (&self.peer_init, &self.local_init),
        };
        let mut hash = Sha256::new();
        for field in [
            self.client_version.as_slice(),
            &self.server_version,
            client_init,
            server_init,
            host_key,
            client_public,
            server_public,
        ] {
            hash.update(&u32::try_from(field.len()).unwrap_or(u32::MAX).to_be_bytes());
            hash.update(field);
        }
        // `K` straight into the hash, where no buffer keeps a copy of it.
        hash.update(&secret.encoded);
        hash.finalize()
    }
}

/// A peer's name list as an error quotes it: it arrives before any host check.
fn offered(names: &[&str]) -> String {
    peer_text(names.join(",").as_bytes())
}

struct ParsedInit<'a> {
    kex: Vec<&'a str>,
    host_key: Vec<&'a str>,
    cipher_c2s: Vec<&'a str>,
    cipher_s2c: Vec<&'a str>,
    compression_c2s: Vec<&'a str>,
    compression_s2c: Vec<&'a str>,
    first_follows: bool,
}

impl<'a> ParsedInit<'a> {
    fn parse(payload: &'a [u8]) -> Result<Self> {
        let mut reader = Reader::new(payload.get(1..).unwrap_or_default());
        reader.bytes(16)?;
        let kex = reader.name_list()?;
        let host_key = reader.name_list()?;
        let cipher_c2s = reader.name_list()?;
        let cipher_s2c = reader.name_list()?;
        reader.name_list()?;
        reader.name_list()?;
        let compression_c2s = reader.name_list()?;
        let compression_s2c = reader.name_list()?;
        reader.name_list()?;
        reader.name_list()?;
        let first_follows = reader.bool()?;
        reader.u32()?;
        Ok(Self {
            kex,
            host_key,
            cipher_c2s,
            cipher_s2c,
            compression_c2s,
            compression_s2c,
            first_follows,
        })
    }
}

/// `K` as both hash and KDF consume it: an `mpint` for Curve25519 (RFC 8731)
/// and a `string` for the hybrid. Wiped on drop.
struct SharedSecret {
    encoded: Vec<u8>,
}

impl Drop for SharedSecret {
    fn drop(&mut self) {
        td_primitives::ct::zero(&mut self.encoded);
    }
}

/// Each written into room for its longest form, so that growing leaves no copy.
fn curve_secret(raw: &Secret32) -> SharedSecret {
    let mut writer = Writer::with_capacity(4 + 1 + 32);
    writer.mpint(&raw.0);
    SharedSecret {
        encoded: writer.into_vec(),
    }
}

fn hybrid_secret(kem: &Secret32, curve: &Secret32) -> SharedSecret {
    let mut hasher = Sha256::new();
    hasher.update(&kem.0);
    hasher.update(&curve.0);
    let digest = Secret32(hasher.finalize());
    let mut writer = Writer::with_capacity(4 + 32);
    writer.string(&digest.0);
    SharedSecret {
        encoded: writer.into_vec(),
    }
}

fn make_ephemeral(algorithm: &str, env: &mut impl KexEnvironment) -> Result<Ephemeral> {
    let mut secret = Secret32([0u8; 32]);
    env.random(&mut secret.0)?;
    let curve_public = x25519::public_key(&secret.0);
    if algorithm == KEX_MLKEM {
        let mut d = Secret32([0u8; 32]);
        let mut z = Secret32([0u8; 32]);
        env.random(&mut d.0)?;
        env.random(&mut z.0)?;
        let (encapsulation, decapsulation) = mlkem768::keygen(&d.0, &z.0);
        let mut public = encapsulation;
        public.extend_from_slice(&curve_public);
        Ok(Ephemeral::Hybrid {
            decapsulation,
            secret: secret.0,
            public,
        })
    } else {
        Ok(Ephemeral::Curve {
            secret: secret.0,
            public: curve_public,
        })
    }
}

/// `Q_S` and `K` for the server's half of the agreement.
fn server_agree(
    algorithm: &str,
    client_public: &[u8],
    env: &mut impl KexEnvironment,
) -> Result<(Vec<u8>, SharedSecret)> {
    let mut secret = Secret32([0u8; 32]);
    env.random(&mut secret.0)?;
    let curve_public = x25519::public_key(&secret.0);
    if algorithm == KEX_MLKEM {
        if client_public.len() != HYBRID_CLIENT_LEN {
            return Err(Error::protocol("bad hybrid client public key length"));
        }
        let (encapsulation, curve) = client_public.split_at(mlkem768::EK_LEN);
        let mut message = Secret32([0u8; 32]);
        env.random(&mut message.0)?;
        let (ciphertext, kem_secret) = mlkem768::encaps(encapsulation, &message.0)
            .ok_or_else(|| Error::protocol("bad ML-KEM encapsulation key"))?;
        let kem_secret = Secret32(kem_secret);
        let shared = curve_shared(&secret.0, curve)?;
        let mut reply = ciphertext;
        reply.extend_from_slice(&curve_public);
        Ok((reply, hybrid_secret(&kem_secret, &shared)))
    } else {
        let shared = curve_shared(&secret.0, client_public)?;
        Ok((curve_public.to_vec(), curve_secret(&shared)))
    }
}

fn client_agree(
    algorithm: &str,
    ephemeral: &Ephemeral,
    server_public: &[u8],
) -> Result<SharedSecret> {
    match ephemeral {
        Ephemeral::Curve { secret, .. } if algorithm == KEX_CURVE => {
            Ok(curve_secret(&curve_shared(secret, server_public)?))
        }
        Ephemeral::Hybrid {
            decapsulation,
            secret,
            ..
        } if algorithm == KEX_MLKEM => {
            if server_public.len() != HYBRID_SERVER_LEN {
                return Err(Error::protocol("bad hybrid server public key length"));
            }
            let (ciphertext, curve) = server_public.split_at(mlkem768::CT_LEN);
            let kem_secret = mlkem768::decaps(decapsulation, ciphertext)
                .ok_or_else(|| Error::protocol("bad ML-KEM ciphertext"))?;
            let kem_secret = Secret32(kem_secret);
            Ok(hybrid_secret(&kem_secret, &curve_shared(secret, curve)?))
        }
        _ => Err(Error::protocol("key exchange state mismatch")),
    }
}

fn curve_shared(secret: &[u8; 32], public: &[u8]) -> Result<Secret32> {
    let public: &[u8; 32] = public
        .try_into()
        .map_err(|_| Error::protocol("bad X25519 public key length"))?;
    x25519::agree(secret, public)
        .map(Secret32)
        .map_err(|_| Error::protocol("X25519 result is all zero"))
}

struct DerivedKeys {
    client_to_server: TransportKey,
    server_to_client: TransportKey,
}

fn derive_keys(secret: &SharedSecret, hash: &[u8; 32], session_id: &[u8; 32]) -> DerivedKeys {
    DerivedKeys {
        client_to_server: derive(secret, hash, b'C', session_id),
        server_to_client: derive(secret, hash, b'D', session_id),
    }
}

/// RFC 4253 section 7.2, extended by one block for the AEAD's 64 bytes.
fn derive(
    secret: &SharedSecret,
    hash: &[u8; 32],
    letter: u8,
    session_id: &[u8; 32],
) -> TransportKey {
    let mut first = Sha256::new();
    first.update(&secret.encoded);
    first.update(hash);
    first.update(&[letter]);
    first.update(session_id);
    let first = Secret32(first.finalize());
    let mut second = Sha256::new();
    second.update(&secret.encoded);
    second.update(hash);
    second.update(&first.0);
    let second = Secret32(second.finalize());
    let mut key = TransportKey::zeroed();
    let (head, tail) = key.bytes_mut().split_at_mut(32);
    head.copy_from_slice(&first.0);
    tail.copy_from_slice(&second.0);
    key
}

/// The 32 key bytes in an `ssh-ed25519` public key blob. Every public key td-ssh
/// trusts passes here, and one of small order is refused: `(R = identity, S = 0)`
/// verifies on every message under it, so it would be a universal key.
pub fn parse_ed25519_public(blob: &[u8]) -> Result<[u8; 32]> {
    let key = ed25519_blob_key(blob)?;
    if ed25519::is_small_order(&key) {
        return Err(Error::protocol("ssh-ed25519 key of small order"));
    }
    Ok(key)
}

/// A well-formed blob's key bytes, before any judgement of the key.
pub(crate) fn ed25519_blob_key(blob: &[u8]) -> Result<[u8; 32]> {
    let mut reader = Reader::new(blob);
    if reader.string()? != HOST_KEY_ALGORITHM.as_bytes() {
        return Err(Error::protocol("not an ssh-ed25519 key"));
    }
    let key = reader.array::<32>()?;
    reader.finish()?;
    Ok(key)
}

/// The 64 bytes in an `ssh-ed25519` signature blob.
pub fn parse_ed25519_signature(blob: &[u8]) -> Result<[u8; 64]> {
    let mut reader = Reader::new(blob);
    if reader.string()? != HOST_KEY_ALGORITHM.as_bytes() {
        return Err(Error::protocol("not an ssh-ed25519 signature"));
    }
    let signature = reader.array::<64>()?;
    reader.finish()?;
    Ok(signature)
}

pub fn ed25519_public_blob(key: &[u8; 32]) -> Vec<u8> {
    let mut writer = Writer::new();
    writer.str(HOST_KEY_ALGORITHM).string(key);
    writer.into_vec()
}

pub fn ed25519_signature_blob(signature: &[u8; 64]) -> Vec<u8> {
    let mut writer = Writer::new();
    writer.str(HOST_KEY_ALGORITHM).string(signature);
    writer.into_vec()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::unreachable)]
    use super::*;

    struct TestEnv {
        counter: u8,
        signing: ed25519::SigningKey,
        host_key_refused: bool,
        verified: u32,
    }

    impl TestEnv {
        fn new() -> Self {
            Self {
                counter: 1,
                signing: ed25519::SigningKey::from_seed(&[42; 32]).unwrap(),
                host_key_refused: false,
                verified: 0,
            }
        }
    }

    impl KexEnvironment for TestEnv {
        fn random(&mut self, out: &mut [u8]) -> Result<()> {
            for byte in out.iter_mut() {
                *byte = self.counter;
                self.counter = self.counter.wrapping_add(13);
            }
            Ok(())
        }
        fn host_key_blob(&mut self) -> Result<Vec<u8>> {
            Ok(ed25519_public_blob(&self.signing.public_key()))
        }
        fn sign(&mut self, hash: &[u8; 32]) -> Result<Vec<u8>> {
            Ok(ed25519_signature_blob(&self.signing.sign(hash).unwrap()))
        }
        fn check_host_key(&mut self, _blob: &[u8]) -> Result<()> {
            if self.host_key_refused {
                Err(Error::refused("Host key verification failed."))
            } else {
                Ok(())
            }
        }
        fn host_key_verified(&mut self) {
            self.verified += 1;
        }
    }

    fn kexinit(kex: &[&str], host_key: &[&str], follows: bool) -> Vec<u8> {
        let mut writer = Writer::message(msg::KEXINIT);
        writer.raw(&[0; 16]);
        writer.name_list(kex);
        writer.name_list(host_key);
        writer.str(CIPHER).str(CIPHER);
        writer.str(MAC_PLACEHOLDER).str(MAC_PLACEHOLDER);
        writer.str("none").str("none");
        writer.str("").str("");
        writer.bool(follows).u32(0);
        writer.into_vec()
    }

    /// Whether this side would discard the peer's guessed packet.
    fn skips_guess(role: Role, local: &[&'static str], peer: &[u8]) -> bool {
        let mut env = TestEnv::new();
        let mut kex = Kex::new(role, V_C, V_S, local.to_vec());
        kex.start(&mut env).unwrap();
        kex.on_message(peer, &mut env).unwrap();
        kex.skip_next
    }

    #[test]
    fn a_guess_is_right_only_if_both_first_entries_agree() {
        const STRICT: &[(Role, &str)] =
            &[(Role::Server, STRICT_CLIENT), (Role::Client, STRICT_SERVER)];
        for (role, strict) in STRICT {
            let local = &[KEX_MLKEM, KEX_CURVE];
            let right = kexinit(&[KEX_MLKEM, KEX_CURVE, strict], &[HOST_KEY_ALGORITHM], true);
            assert!(!skips_guess(*role, local, &right), "{role:?} right");
            // Negotiation settles on the peer's first entry, which the guess
            // still lost, since the first entries differ.
            let wrong_kex = kexinit(&[KEX_CURVE, KEX_MLKEM, strict], &[HOST_KEY_ALGORITHM], true);
            assert!(skips_guess(*role, local, &wrong_kex), "{role:?} kex");
            let only_curve = kexinit(&[KEX_CURVE, strict], &[HOST_KEY_ALGORITHM], true);
            assert!(skips_guess(*role, local, &only_curve), "{role:?} sole");
            let wrong_key = kexinit(
                &[KEX_MLKEM, strict],
                &["ecdsa-sha2-nistp256", HOST_KEY_ALGORITHM],
                true,
            );
            assert!(skips_guess(*role, local, &wrong_key), "{role:?} key");
            let no_guess = kexinit(&[KEX_CURVE, strict], &[HOST_KEY_ALGORITHM], false);
            assert!(!skips_guess(*role, local, &no_guess), "{role:?} none");
        }
    }

    #[test]
    fn a_hostile_name_list_reaches_an_error_without_controls() {
        const HOSTILE: &str = "\x1b]0;PWNED\x07";
        let strict = STRICT_SERVER;
        let peers = [
            kexinit(&[HOSTILE, strict], &[HOST_KEY_ALGORITHM], false),
            kexinit(&[KEX_CURVE, strict], &[HOSTILE], false),
            {
                let mut writer = Writer::message(msg::KEXINIT);
                writer.raw(&[0; 16]);
                writer.name_list(&[KEX_CURVE, strict]);
                writer.str(HOST_KEY_ALGORITHM);
                writer.str(HOSTILE).str(HOSTILE);
                writer.str(MAC_PLACEHOLDER).str(MAC_PLACEHOLDER);
                writer.str("none").str("none");
                writer.str("").str("");
                writer.bool(false).u32(0);
                writer.into_vec()
            },
        ];
        for peer in peers {
            let mut env = TestEnv::new();
            let mut client = Kex::new(Role::Client, V_C, V_S, DEFAULT_KEX.to_vec());
            client.start(&mut env).unwrap();
            let text = client
                .on_message(&peer, &mut env)
                .err()
                .unwrap()
                .to_string();
            assert!(text.contains("PWNED"), "{text}");
            assert!(!text.chars().any(char::is_control), "{text:?}");
        }
    }

    #[test]
    fn the_ephemeral_secrets_wipe() {
        let mut curve = Ephemeral::Curve {
            secret: [9; 32],
            public: [9; 32],
        };
        curve.wipe();
        let Ephemeral::Curve { secret, public } = &curve else {
            unreachable!()
        };
        assert_eq!((*secret, *public), ([0; 32], [9; 32]));
        let mut hybrid = Ephemeral::Hybrid {
            decapsulation: vec![9; 8],
            secret: [9; 32],
            public: vec![9; 8],
        };
        hybrid.wipe();
        let Ephemeral::Hybrid {
            decapsulation,
            secret,
            public,
        } = &hybrid
        else {
            unreachable!()
        };
        assert_eq!(decapsulation, &[0; 8]);
        assert_eq!(*secret, [0; 32]);
        assert_eq!(public, &[9; 8]);
    }

    #[test]
    fn a_host_key_is_confirmed_only_after_its_signature_verifies() {
        let exchange = |tamper: bool| {
            let mut client_env = TestEnv::new();
            let mut server_env = TestEnv::new();
            let mut client = Kex::new(Role::Client, V_C, V_S, vec![KEX_CURVE]);
            let mut server = Kex::new(Role::Server, V_C, V_S, DEFAULT_KEX.to_vec());
            let client_init = client.start(&mut client_env).unwrap();
            let server_init = server.start(&mut server_env).unwrap();
            let out = client.on_message(&server_init, &mut client_env).unwrap();
            server.on_message(&client_init, &mut server_env).unwrap();
            let reply = server.on_message(&out.send[0], &mut server_env).unwrap();
            let mut message = reply.send[0].clone();
            if tamper {
                let last = message.len() - 1;
                message[last] ^= 1;
            }
            let _ = client.on_message(&message, &mut client_env);
            client_env.verified
        };
        assert_eq!(exchange(false), 1);
        assert_eq!(exchange(true), 0);
    }

    /// The identity's encoding, and the signature that verifies under it on any
    /// message: `(R = identity, S = 0)`.
    const IDENTITY: [u8; 32] = {
        let mut key = [0u8; 32];
        key[0] = 1;
        key
    };
    const UNIVERSAL_SIGNATURE: [u8; 64] = {
        let mut signature = [0u8; 64];
        signature[0] = 1;
        signature
    };

    /// A server that holds the identity as its host key and signs with the
    /// universal signature, which is all such a key needs.
    struct SmallOrderHost(TestEnv);

    impl KexEnvironment for SmallOrderHost {
        fn random(&mut self, out: &mut [u8]) -> Result<()> {
            self.0.random(out)
        }
        fn host_key_blob(&mut self) -> Result<Vec<u8>> {
            Ok(ed25519_public_blob(&IDENTITY))
        }
        fn sign(&mut self, _hash: &[u8; 32]) -> Result<Vec<u8>> {
            Ok(ed25519_signature_blob(&UNIVERSAL_SIGNATURE))
        }
        fn check_host_key(&mut self, _blob: &[u8]) -> Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_host_key_of_small_order_is_a_protocol_error() {
        assert!(ed25519::verify(&IDENTITY, b"any message", &UNIVERSAL_SIGNATURE).is_ok());
        let mut client_env = TestEnv::new();
        let mut server_env = SmallOrderHost(TestEnv::new());
        let mut client = Kex::new(Role::Client, V_C, V_S, vec![KEX_CURVE]);
        let mut server = Kex::new(Role::Server, V_C, V_S, DEFAULT_KEX.to_vec());
        let client_init = client.start(&mut client_env).unwrap();
        let server_init = server.start(&mut server_env).unwrap();
        let out = client.on_message(&server_init, &mut client_env).unwrap();
        server.on_message(&client_init, &mut server_env).unwrap();
        let reply = server.on_message(&out.send[0], &mut server_env).unwrap();
        let error = client
            .on_message(&reply.send[0], &mut client_env)
            .err()
            .unwrap();
        assert!(matches!(error, Error::Protocol(_)), "{error}");
        assert!(error.to_string().contains("small order"), "{error}");
        assert_eq!(client_env.verified, 0);
    }

    #[test]
    fn every_spelling_of_a_small_order_key_is_refused_and_an_ordinary_one_is_not() {
        let mut spellings = vec![IDENTITY, [0; 32]];
        let mut p_minus_one = [0xff; 32];
        p_minus_one[0] = 0xec;
        p_minus_one[31] = 0x7f;
        spellings.push(p_minus_one);
        for key in spellings {
            let mut signed = key;
            signed[31] |= 0x80;
            for key in [key, signed] {
                let error = parse_ed25519_public(&ed25519_public_blob(&key)).err();
                assert!(error.is_some(), "{key:?}");
            }
        }
        let ordinary = TestEnv::new().signing.public_key();
        assert_eq!(
            parse_ed25519_public(&ed25519_public_blob(&ordinary)).unwrap(),
            ordinary
        );
    }

    /// Both directions' keys as each side installed them, and the session ID.
    type ExchangedKeys = ([u8; 64], [u8; 64], [u8; 64], [u8; 64], [u8; 32]);

    const V_C: &str = "SSH-2.0-client";
    const V_S: &str = "SSH-2.0-server";

    /// Runs a whole exchange in memory and returns both sides' installed keys.
    fn run(algorithms: Vec<&'static str>) -> ExchangedKeys {
        let mut client = Kex::new(Role::Client, V_C, V_S, algorithms);
        let mut server = Kex::new(Role::Server, V_C, V_S, DEFAULT_KEX.to_vec());
        let mut client_env = TestEnv::new();
        let mut server_env = TestEnv::new();
        let client_init = client.start(&mut client_env).unwrap();
        let server_init = server.start(&mut server_env).unwrap();
        let ecdh_init = client.on_message(&server_init, &mut client_env).unwrap();
        assert_eq!(ecdh_init.send.len(), 1);
        assert!(server
            .on_message(&client_init, &mut server_env)
            .unwrap()
            .send
            .is_empty());
        let reply = server
            .on_message(&ecdh_init.send[0], &mut server_env)
            .unwrap();
        assert_eq!(reply.send.len(), 2);
        let client_tx = client.on_message(&reply.send[0], &mut client_env).unwrap();
        let client_rx = client.on_message(&reply.send[1], &mut client_env).unwrap();
        let server_rx = server
            .on_message(&client_tx.send[0], &mut server_env)
            .unwrap();
        assert!(client_rx.finished && server_rx.finished);
        assert_eq!(client.session_id(), server.session_id());
        (
            *client_tx.tx_key.unwrap().bytes(),
            *client_rx.rx_key.unwrap().bytes(),
            *reply.tx_key.unwrap().bytes(),
            *server_rx.rx_key.unwrap().bytes(),
            client.session_id().unwrap(),
        )
    }

    #[test]
    fn both_algorithms_agree_on_keys_in_each_direction() {
        for algorithm in DEFAULT_KEX {
            let (client_tx, client_rx, server_tx, server_rx, _) = run(vec![*algorithm]);
            assert_eq!(client_tx, server_rx, "{algorithm}");
            assert_eq!(client_rx, server_tx, "{algorithm}");
            assert_ne!(client_tx, client_rx, "{algorithm}");
        }
    }

    #[test]
    fn a_peer_without_strict_kex_is_refused_at_the_first_exchange_only() {
        let init_without_strict = {
            let mut writer = Writer::message(msg::KEXINIT);
            writer.raw(&[0; 16]);
            writer.name_list(&[KEX_CURVE]);
            writer.str(HOST_KEY_ALGORITHM);
            writer.str(CIPHER).str(CIPHER);
            writer.str(MAC_PLACEHOLDER).str(MAC_PLACEHOLDER);
            writer.str("none").str("none");
            writer.str("").str("");
            writer.bool(false).u32(0);
            writer.into_vec()
        };
        let mut env = TestEnv::new();
        let mut server = Kex::new(Role::Server, V_C, V_S, DEFAULT_KEX.to_vec());
        server.start(&mut env).unwrap();
        let error = server
            .on_message(&init_without_strict, &mut env)
            .err()
            .unwrap();
        assert!(error.to_string().contains("strict key exchange"));

        // A rekey carries no marker, and that is not a refusal.
        let mut server = Kex::resume(Role::Server, V_C, V_S, [1; 32]);
        server.start(&mut env).unwrap();
        assert!(server.on_message(&init_without_strict, &mut env).is_ok());
    }

    #[test]
    fn negotiation_failures_name_what_the_peer_offered() {
        let mut env = TestEnv::new();
        let mut client = Kex::new(Role::Client, V_C, V_S, vec![KEX_CURVE]);
        client.start(&mut env).unwrap();
        let mut writer = Writer::message(msg::KEXINIT);
        writer.raw(&[0; 16]);
        writer.name_list(&["diffie-hellman-group14-sha1", STRICT_SERVER]);
        writer.str(HOST_KEY_ALGORITHM);
        writer.str(CIPHER).str(CIPHER);
        writer.str(MAC_PLACEHOLDER).str(MAC_PLACEHOLDER);
        writer.str("none").str("none");
        writer.str("").str("");
        writer.bool(false).u32(0);
        let error = client
            .on_message(&writer.into_vec(), &mut env)
            .err()
            .unwrap();
        assert!(error.to_string().contains("diffie-hellman-group14-sha1"));
    }

    #[test]
    fn messages_out_of_order_are_refused() {
        let mut env = TestEnv::new();
        let mut client = Kex::new(Role::Client, V_C, V_S, DEFAULT_KEX.to_vec());
        client.start(&mut env).unwrap();
        for number in [msg::NEWKEYS, msg::KEX_ECDH_REPLY, msg::KEX_ECDH_INIT, 99] {
            assert!(client.on_message(&[number], &mut env).is_err());
        }
        let mut server = Kex::new(Role::Server, V_C, V_S, DEFAULT_KEX.to_vec());
        server.start(&mut env).unwrap();
        assert!(server.on_message(&[msg::KEX_ECDH_REPLY], &mut env).is_err());
    }

    #[test]
    fn a_second_kexinit_during_an_exchange_is_refused() {
        let mut client_env = TestEnv::new();
        let mut server_env = TestEnv::new();
        let mut client = Kex::new(Role::Client, V_C, V_S, DEFAULT_KEX.to_vec());
        let mut server = Kex::new(Role::Server, V_C, V_S, DEFAULT_KEX.to_vec());
        client.start(&mut client_env).unwrap();
        let init = server.start(&mut server_env).unwrap();
        client.on_message(&init, &mut client_env).unwrap();
        assert!(client.on_message(&init, &mut client_env).is_err());
    }

    #[test]
    fn a_host_key_refusal_or_a_bad_signature_stops_the_client() {
        let mut client_env = TestEnv::new();
        let mut server_env = TestEnv::new();
        let mut client = Kex::new(Role::Client, V_C, V_S, vec![KEX_CURVE]);
        let mut server = Kex::new(Role::Server, V_C, V_S, DEFAULT_KEX.to_vec());
        let client_init = client.start(&mut client_env).unwrap();
        let server_init = server.start(&mut server_env).unwrap();
        let out = client.on_message(&server_init, &mut client_env).unwrap();
        server.on_message(&client_init, &mut server_env).unwrap();
        let reply = server.on_message(&out.send[0], &mut server_env).unwrap();

        // A tampered final signature byte fails verification.
        let mut bad = reply.send[0].clone();
        let last = bad.len() - 1;
        bad[last] ^= 1;
        let mut same_client = Kex::new(Role::Client, V_C, V_S, vec![KEX_CURVE]);
        let mut same_env = TestEnv::new();
        same_client.start(&mut same_env).unwrap();
        same_client.on_message(&server_init, &mut same_env).unwrap();
        let error = same_client
            .on_message(&bad, &mut same_env)
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("signature is invalid"), "{error}");

        // A client whose cookie differs has another transcript, so the good
        // signature fails for it: the signature binds the whole negotiation.
        let mut other_client = Kex::new(Role::Client, V_C, V_S, vec![KEX_CURVE]);
        let mut other_env = TestEnv::new();
        other_env.counter = 200;
        other_client.start(&mut other_env).unwrap();
        other_client
            .on_message(&server_init, &mut other_env)
            .unwrap();
        assert!(other_client
            .on_message(&reply.send[0], &mut other_env)
            .is_err());

        client_env.host_key_refused = true;
        let error = client
            .on_message(&reply.send[0], &mut client_env)
            .err()
            .unwrap();
        assert!(error.to_string().contains("Host key verification failed"));
    }

    #[test]
    fn a_rekey_keeps_the_session_id_and_changes_the_keys() {
        let (client_tx, _, _, _, session_id) = run(vec![KEX_CURVE]);
        let mut client = Kex::resume(Role::Client, V_C, V_S, session_id);
        let mut server = Kex::resume(Role::Server, V_C, V_S, session_id);
        let mut client_env = TestEnv::new();
        client_env.counter = 77;
        let mut server_env = TestEnv::new();
        let client_init = client.start(&mut client_env).unwrap();
        // The server learns of the rekey from the client's KEXINIT.
        let server_out = server.on_message(&client_init, &mut server_env).unwrap();
        assert_eq!(server_out.send.len(), 1);
        let ecdh = client
            .on_message(&server_out.send[0], &mut client_env)
            .unwrap();
        let reply = server.on_message(&ecdh.send[0], &mut server_env).unwrap();
        let tx = client.on_message(&reply.send[0], &mut client_env).unwrap();
        let rx = client.on_message(&reply.send[1], &mut client_env).unwrap();
        server.on_message(&tx.send[0], &mut server_env).unwrap();
        assert_eq!(client.session_id(), Some(session_id));
        assert_eq!(server.session_id(), Some(session_id));
        assert_ne!(*tx.tx_key.unwrap().bytes(), client_tx);
        assert_eq!(reply.tx_key.unwrap(), rx.rx_key.unwrap());
        assert!(!client.in_progress() && !server.in_progress());
    }

    #[test]
    fn malformed_kex_messages_do_not_panic() {
        let mut env = TestEnv::new();
        for len in 0..64 {
            let garbage = vec![msg::KEXINIT; len];
            let mut kex = Kex::new(Role::Server, V_C, V_S, DEFAULT_KEX.to_vec());
            kex.start(&mut env).unwrap();
            let _ = kex.on_message(&garbage, &mut env);
        }
        let mut kex = Kex::new(Role::Server, V_C, V_S, DEFAULT_KEX.to_vec());
        kex.start(&mut env).unwrap();
        kex.state = State::ServerAwaitInit {
            algorithm: KEX_MLKEM,
        };
        let mut message = Writer::message(msg::KEX_ECDH_INIT);
        message.string(&[0u8; HYBRID_CLIENT_LEN]);
        // Zero is a valid ML-KEM polynomial; an all-zero X25519 point is refused.
        assert!(kex.on_message(&message.into_vec(), &mut env).is_err());
    }
}
