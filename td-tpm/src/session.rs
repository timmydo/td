//! Salted, parameter-encrypted HMAC and policy sessions (DESIGN.md
//! "Salted and PIN-authorized sessions"): the salt by ECDH and KDFe to the
//! storage primary's P-256 key, the session key by KDFa, the session HMAC
//! over cpHash and rpHash, and AES-128-CFB parameter encryption, as TPM 2.0
//! Part 1 specifies them. P-256, AES and HMAC-SHA256 are td-fido's.

use super::{
    digest, equal, put16, put32, put_blob, random, session_class, zero, Client, Reader, Refusal,
    Transport, MAX_PACKET, NO_SESSIONS, NULL, SESSIONS, SHA256, START_AUTH_SESSION,
};
use std::fs::File;
use std::io::Read;
use td_fido::fido_aes;
use td_fido::fido_p256::{PublicKey, SecretScalar};

/// TPMA_SESSION bits.
pub(crate) const CONTINUE_SESSION: u8 = 0x01;
pub(crate) const DECRYPT: u8 = 0x20;
pub(crate) const ENCRYPT: u8 = 0x40;
pub(crate) const HMAC_SESSION: u8 = 0x00;
const AES: u16 = 0x0006;
const CFB: u16 = 0x0043;
/// The largest authValue a SHA-256 object takes: its nameAlg's digest.
pub const MAX_AUTH_VALUE: usize = 32;
/// One session in the authorization area: its handle, a 32-byte nonce,
/// the attributes and a 32-byte HMAC.
const AREA: usize = 4 + 2 + 32 + 1 + 2 + 32;

/// An authValue without its trailing zero octets, as the TPM removes them
/// before it stores or uses one.
pub fn trim_auth(auth: &[u8]) -> &[u8] {
    let end = auth
        .iter()
        .rposition(|byte| *byte != 0)
        .map_or(0, |at| at + 1);
    auth.get(..end).unwrap_or_default()
}

/// KDFa with HMAC-SHA256 for 256 bits: one block, the
/// label with its terminating zero octet.
pub(crate) fn kdfa(key: &[u8], label: &[u8], context_u: &[u8], context_v: &[u8]) -> [u8; 32] {
    td_fido::hmac_sha256(
        key,
        &[
            &1u32.to_be_bytes(),
            label,
            &[0],
            context_u,
            context_v,
            &256u32.to_be_bytes(),
        ],
    )
}

/// KDFe with SHA-256 for 256 bits.
pub(crate) fn kdfe(z: &[u8], label: &[u8], party_u: &[u8], party_v: &[u8]) -> [u8; 32] {
    let mut input =
        Vec::with_capacity(4 + z.len() + label.len() + 1 + party_u.len() + party_v.len());
    input.extend_from_slice(&1u32.to_be_bytes());
    input.extend_from_slice(z);
    input.extend_from_slice(label);
    input.push(0);
    input.extend_from_slice(party_u);
    input.extend_from_slice(party_v);
    let out = digest(&input);
    zero(&mut input);
    out
}

/// A 32-byte secret in one heap allocation, zeroed on drop.
pub(crate) struct Key(Box<[u8; 32]>);
impl Key {
    pub(crate) fn new(bytes: [u8; 32]) -> Self {
        let mut key = Self(Box::new([0; 32]));
        let mut bytes = bytes;
        *key.0 = bytes;
        zero(&mut bytes);
        key
    }

    pub(crate) fn bytes(&self) -> &[u8; 32] {
        &self.0
    }
}
impl Drop for Key {
    fn drop(&mut self) {
        zero(self.0.as_mut());
    }
}

/// An ephemeral P-256 scalar from `/dev/random`, which blocks until the
/// kernel's generator is initialized, as early boot needs: it hides the
/// session salt, so unlike a nonce it is a secret.
fn ephemeral_scalar() -> Result<SecretScalar, String> {
    for _ in 0..8 {
        let mut bytes = Box::new([0; 32]);
        let read = File::open("/dev/random").and_then(|mut file| file.read_exact(bytes.as_mut()));
        if let Err(error) = read {
            zero(bytes.as_mut());
            return Err(format!("read /dev/random: {error}"));
        }
        if let Ok(scalar) = SecretScalar::from_bytes(bytes) {
            return Ok(scalar);
        }
    }
    Err("/dev/random gave no P-256 scalar in eight attempts".into())
}

/// The salt `ephemeral` shares with the storage primary whose public point
/// is (`x`, `y`) as the TPM returned it, and the ephemeral public point
/// that StartAuthSession's encryptedSalt carries: Z is the x-coordinate of
/// the ECDH point, PartyUInfo the ephemeral x, PartyVInfo the primary's.
pub(crate) fn salt(
    ephemeral: &SecretScalar,
    x: &[u8],
    y: &[u8],
) -> Result<(Key, [u8; 32], [u8; 32]), String> {
    let primary = PublicKey::from_coordinates(&coordinate(x)?, &coordinate(y)?)?;
    let (ephemeral_x, ephemeral_y) = ephemeral.public_key()?.coordinates();
    let shared = ephemeral.agree(&primary)?;
    let salt = Key::new(kdfe(shared.bytes(), b"SECRET", &ephemeral_x, x));
    Ok((salt, ephemeral_x, ephemeral_y))
}

/// A TPM coordinate of one to 32 bytes, left-padded to 32.
fn coordinate(value: &[u8]) -> Result<[u8; 32], String> {
    let start = 32usize
        .checked_sub(value.len())
        .filter(|_| !value.is_empty())
        .ok_or("invalid TPM parent public point")?;
    let mut out = [0; 32];
    for (to, from) in out.iter_mut().skip(start).zip(value) {
        *to = *from;
    }
    Ok(out)
}

/// A salted session: its handle, key and the TPM's last nonce.
pub(crate) struct Session {
    pub(crate) handle: u32,
    key: Key,
    nonce_tpm: [u8; 32],
}

/// cpHash: the command code, the Names of its handles, then the parameter
/// area as sent, encrypted where it is.
fn cp_hash(code: u32, names: &[&[u8]], parameters: &[u8]) -> [u8; 32] {
    let mut input = code.to_be_bytes().to_vec();
    for name in names {
        input.extend_from_slice(name);
    }
    input.extend_from_slice(parameters);
    digest(&input)
}

/// rpHash for a successful reply: TPM_RC_SUCCESS, the command code, then
/// the parameter area as received.
fn rp_hash(code: u32, parameters: &[u8]) -> [u8; 32] {
    let mut input = vec![0; 4];
    put32(&mut input, code);
    input.extend_from_slice(parameters);
    digest(&input)
}

/// The session value, the session key followed by the trimmed authValue,
/// which keys both the HMAC and the CFB key derivation.
fn session_value(key: &Key, auth: &[u8]) -> Vec<u8> {
    let auth = trim_auth(auth);
    let mut value = Vec::with_capacity(32 + auth.len());
    value.extend_from_slice(key.bytes());
    value.extend_from_slice(auth);
    value
}

/// The session HMAC over a parameter hash, the newer then the older nonce,
/// and the session attributes.
fn session_hmac(
    key: &Key,
    auth: &[u8],
    parameter_hash: &[u8; 32],
    newer: &[u8],
    older: &[u8],
    attributes: u8,
) -> [u8; 32] {
    let mut value = session_value(key, auth);
    let mac = td_fido::hmac_sha256(&value, &[parameter_hash, newer, older, &[attributes]]);
    zero(&mut value);
    mac
}

/// Encrypt or decrypt, in place, the data of the TPM2B that leads
/// `parameters`, under the AES-128 key and IV KDFa derives from the session
/// value and the nonces, newer first.
fn crypt_first(
    parameters: &mut [u8],
    key: &Key,
    auth: &[u8],
    newer: &[u8],
    older: &[u8],
    encrypt: bool,
) -> Result<(), String> {
    let mut size = Reader(parameters);
    let size = usize::from(size.u16().map_err(|_| "no TPM2B parameter to encrypt")?);
    let data = parameters
        .get_mut(2..2 + size)
        .ok_or("truncated TPM2B parameter")?;
    let mut value = session_value(key, auth);
    let mut derived = kdfa(&value, b"CFB", newer, older);
    zero(&mut value);
    // The key, then the IV.
    let crypted = match derived
        .split_first_chunk::<16>()
        .and_then(|(aes, iv)| Some((aes, <&[u8; 16]>::try_from(iv).ok()?)))
    {
        Some((aes, iv)) if encrypt => fido_aes::cfb_encrypt(aes, iv, data),
        Some((aes, iv)) => fido_aes::cfb_decrypt(aes, iv, data),
        None => Err("invalid CFB key"),
    };
    zero(&mut derived);
    crypted.map_err(String::from)
}

/// StartAuthSession's parameters for a session of `kind` salted by
/// `ephemeral`'s point, with AES-128-CFB and SHA-256.
fn start_parameters(
    nonce_caller: &[u8; 32],
    ephemeral_x: &[u8; 32],
    ephemeral_y: &[u8; 32],
    kind: u8,
) -> Result<Vec<u8>, String> {
    let mut point = Vec::with_capacity(68);
    put_blob(&mut point, ephemeral_x)?;
    put_blob(&mut point, ephemeral_y)?;
    let mut parameters = Vec::with_capacity(115);
    put_blob(&mut parameters, nonce_caller)?;
    put_blob(&mut parameters, &point)?;
    parameters.push(kind);
    put16(&mut parameters, AES);
    put16(&mut parameters, 128);
    put16(&mut parameters, CFB);
    put16(&mut parameters, SHA256);
    Ok(parameters)
}

/// One command in `session`, authorizing `handles` (whose Names are
/// `names`) with `auth`, its first parameter encrypted when `attributes`
/// sets DECRYPT. `nonce_caller` is the command's fresh nonce.
#[allow(
    clippy::too_many_arguments,
    reason = "the command's fields, each one Part 1 names"
)]
fn session_command(
    code: u32,
    handles: &[u32],
    names: &[&[u8]],
    session: &Session,
    auth: &[u8],
    attributes: u8,
    parameters: &[u8],
    nonce_caller: &[u8; 32],
) -> Result<Vec<u8>, String> {
    if names.len() != handles.len() {
        return Err("a TPM session command needs each handle's Name".into());
    }
    let length = 10 + 4 * handles.len() + 4 + AREA + parameters.len();
    if length > MAX_PACKET {
        return Err("oversized TPM command".into());
    }
    let size = u32::try_from(length).map_err(|_| "oversized TPM command")?;
    let mut wire = Vec::with_capacity(parameters.len());
    wire.extend_from_slice(parameters);
    if attributes & DECRYPT != 0 {
        let crypted = crypt_first(
            &mut wire,
            &session.key,
            auth,
            nonce_caller,
            &session.nonce_tpm,
            true,
        );
        if let Err(error) = crypted {
            zero(&mut wire);
            return Err(error);
        }
    }
    let mac = session_hmac(
        &session.key,
        auth,
        &cp_hash(code, names, &wire),
        nonce_caller,
        &session.nonce_tpm,
        attributes,
    );
    let mut command = Vec::with_capacity(length);
    put16(&mut command, SESSIONS);
    put32(&mut command, size);
    put32(&mut command, code);
    for handle in handles {
        put32(&mut command, *handle);
    }
    put32(&mut command, AREA as u32);
    put32(&mut command, session.handle);
    put16(&mut command, 32);
    command.extend_from_slice(nonce_caller);
    command.push(attributes);
    put16(&mut command, 32);
    command.extend_from_slice(&mac);
    command.extend_from_slice(&wire);
    zero(&mut wire);
    Ok(command)
}

/// A successful reply to `session_command`: its HMAC verified before
/// anything in it is trusted, the session's nonce rolled, and its first
/// parameter decrypted when `attributes` sets ENCRYPT. No session command
/// returns a handle.
fn session_response(
    code: u32,
    session: &mut Session,
    auth: &[u8],
    attributes: u8,
    nonce_caller: &[u8; 32],
    response: &[u8],
) -> Result<Vec<u8>, String> {
    let mut reader = Reader(response);
    if reader.u16()? != SESSIONS || reader.u32()? as usize != response.len() {
        return Err("invalid TPM session response".into());
    }
    if reader.u32()? != 0 {
        return Err("TPM refused the session command".into());
    }
    let size = reader.u32()? as usize;
    let parameters = reader.take(size)?;
    let nonce: [u8; 32] = reader
        .blob()?
        .try_into()
        .map_err(|_| "invalid TPM response nonce")?;
    if reader.u8()? != attributes {
        return Err("unexpected TPM session attributes".into());
    }
    let mac: [u8; 32] = reader
        .blob()?
        .try_into()
        .map_err(|_| "invalid TPM response HMAC")?;
    reader.end()?;
    let expected = session_hmac(
        &session.key,
        auth,
        &rp_hash(code, parameters),
        &nonce,
        nonce_caller,
        attributes,
    );
    if !equal(&expected, &mac) {
        return Err("TPM response HMAC does not verify".into());
    }
    session.nonce_tpm = nonce;
    let mut out = Vec::with_capacity(parameters.len());
    out.extend_from_slice(parameters);
    if attributes & ENCRYPT != 0 {
        if let Err(error) = crypt_first(&mut out, &session.key, auth, &nonce, nonce_caller, false) {
            zero(&mut out);
            return Err(error);
        }
    }
    Ok(out)
}

impl<T: Transport> Client<T> {
    /// A session of `kind` (HMAC or policy) salted to the loaded storage
    /// primary `parent`, whose public point is (`x`, `y`). The session is
    /// owned by this client.
    pub(crate) fn salted_session(
        &mut self,
        parent: u32,
        x: &[u8],
        y: &[u8],
        kind: u8,
    ) -> Result<Session, String> {
        let ephemeral = ephemeral_scalar()?;
        self.salted_session_with(parent, x, y, kind, &ephemeral, &random()?)
    }

    fn salted_session_with(
        &mut self,
        parent: u32,
        x: &[u8],
        y: &[u8],
        kind: u8,
        ephemeral: &SecretScalar,
        nonce_caller: &[u8; 32],
    ) -> Result<Session, String> {
        let (salt, ephemeral_x, ephemeral_y) = salt(ephemeral, x, y)?;
        let parameters = start_parameters(nonce_caller, &ephemeral_x, &ephemeral_y, kind)?;
        let (handle, out) =
            self.call(START_AUTH_SESSION, &[parent, NULL], None, &parameters, true)?;
        let handle = session_class(handle.ok_or("missing TPM session handle")?, kind)?;
        let mut reader = Reader(&out);
        let nonce_tpm: [u8; 32] = reader
            .blob()?
            .try_into()
            .map_err(|_| "invalid TPM session nonce")?;
        reader.end()?;
        let key = Key::new(kdfa(salt.bytes(), b"ATH", &nonce_tpm, nonce_caller));
        Ok(Session {
            handle,
            key,
            nonce_tpm,
        })
    }

    /// One command in a salted `session` (see `session_command`). The
    /// reply's HMAC is verified before it is trusted or decrypted; a
    /// session without CONTINUE_SESSION is no longer owned once the TPM
    /// answers the command.
    #[allow(
        clippy::too_many_arguments,
        reason = "the command's fields, each one Part 1 names"
    )]
    pub(crate) fn call_session(
        &mut self,
        code: u32,
        handles: &[u32],
        names: &[&[u8]],
        session: &mut Session,
        auth: &[u8],
        attributes: u8,
        parameters: &[u8],
    ) -> Result<Vec<u8>, String> {
        self.refused = None;
        let nonce_caller = random()?;
        let mut command = session_command(
            code,
            handles,
            names,
            session,
            auth,
            attributes,
            parameters,
            &nonce_caller,
        )?;
        let response = self.transport.exchange(&command);
        zero(&mut command);
        let mut response = response?;
        let result = (|| {
            let mut reader = Reader(&response);
            let tag = reader.u16()?;
            if reader.u32()? as usize != response.len() || response.len() > MAX_PACKET {
                return Err("invalid TPM response size".into());
            }
            let rc = reader.u32()?;
            if rc != 0 {
                if tag != NO_SESSIONS || !reader.0.is_empty() {
                    return Err("malformed TPM error response".into());
                }
                self.refused = Some(Refusal { command: code, rc });
                return Err(format!("TPM command {code:#x} refused: {rc:#x}"));
            }
            let out = session_response(code, session, auth, attributes, &nonce_caller, &response)?;
            // Only a verified success shows the TPM ended the session;
            // otherwise it stays owned, for drop to flush.
            if attributes & CONTINUE_SESSION == 0 {
                self.handles.retain(|handle| *handle != session.handle);
            }
            Ok(out)
        })();
        zero(&mut response);
        result
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::{POLICY_AUTH_VALUE, POLICY_SESSION};
    use std::cell::RefCell;
    use std::rc::Rc;

    pub(crate) fn response(tag: u16, rc: u32, body: &[u8]) -> Vec<u8> {
        let mut out = tag.to_be_bytes().to_vec();
        put32(&mut out, 10 + body.len() as u32);
        put32(&mut out, rc);
        out.extend_from_slice(body);
        out
    }

    pub(crate) type Sent = Rc<RefCell<Vec<Vec<u8>>>>;

    /// Replies with one fixed packet and records every command.
    pub(crate) struct Fixed {
        reply: Vec<u8>,
        sent: Sent,
    }
    impl Fixed {
        pub(crate) fn client(reply: Vec<u8>) -> (Client<Self>, Sent) {
            let sent = Rc::new(RefCell::new(Vec::new()));
            let client = Client::new(Self {
                reply,
                sent: sent.clone(),
            });
            (client, sent)
        }
    }
    impl Transport for Fixed {
        fn exchange(&mut self, command: &[u8]) -> Result<Vec<u8>, String> {
            self.sent.borrow_mut().push(command.to_vec());
            Ok(self.reply.clone())
        }
    }

    pub(crate) fn vector(name: &str) -> Vec<u8> {
        let line = include_str!("../tests/session_vectors.txt")
            .lines()
            .find_map(|line| line.strip_prefix(name)?.strip_prefix(' '))
            .unwrap_or_else(|| panic!("no session vector {name}"));
        (0..line.len())
            .step_by(2)
            .map(|at| u8::from_str_radix(&line[at..at + 2], 16).unwrap())
            .collect()
    }

    fn array(name: &str) -> [u8; 32] {
        vector(name).try_into().unwrap()
    }

    pub(crate) fn ephemeral() -> SecretScalar {
        SecretScalar::from_bytes(Box::new(array("ephemeral_scalar"))).unwrap()
    }

    fn session(key: &str, nonce_tpm: &str) -> Session {
        Session {
            handle: 0x0300_0000,
            key: Key::new(array(key)),
            nonce_tpm: array(nonce_tpm),
        }
    }

    #[test]
    fn the_salt_is_kdfe_over_the_ecdh_point_and_both_x_coordinates() {
        let (x, y) = (vector("primary_x"), vector("primary_y"));
        let (derived, ephemeral_x, ephemeral_y) = salt(&ephemeral(), &x, &y).unwrap();
        assert_eq!(ephemeral_x.to_vec(), vector("ephemeral_x"));
        assert_eq!(ephemeral_y.to_vec(), vector("ephemeral_y"));
        assert_eq!(derived.bytes().to_vec(), vector("salt"));
        let shared = ephemeral()
            .agree(&PublicKey::from_coordinates(&array("primary_x"), &array("primary_y")).unwrap())
            .unwrap();
        assert_eq!(shared.bytes().to_vec(), vector("z"));
        // A point off the curve, and coordinates of no or too many bytes,
        // are refused before any ECDH.
        let mut off = y.clone();
        off[31] ^= 1;
        assert!(salt(&ephemeral(), &x, &off).is_err());
        assert!(salt(&ephemeral(), &[], &y).is_err());
        assert!(salt(&ephemeral(), &[&[0][..], &x].concat(), &y).is_err());
        // A coordinate the TPM returned short is the same point, left-padded,
        // and PartyVInfo is the bytes as returned.
        assert_eq!(coordinate(&[1, 2]).unwrap()[30..], [1, 2]);
        assert!(coordinate(&[0; 32]).unwrap().iter().all(|byte| *byte == 0));
    }

    /// StartAuthSession's bytes, salted to the primary at 0x80000000, and
    /// the session key KDFa derives from the salt and both nonces.
    #[test]
    fn salted_sessions_send_the_ephemeral_point_and_derive_the_session_key() {
        for (label, kind, handle, other) in [
            ("policy", POLICY_SESSION, 0x0300_0000, 0x0200_0000),
            ("hmac", HMAC_SESSION, 0x0200_0000, 0x0300_0000),
        ] {
            let start = vector(&format!("{label}_start"));
            let caller: [u8; 32] = start[20..52].try_into().unwrap();
            let reply = |handle: u32| {
                let mut body = handle.to_be_bytes().to_vec();
                put_blob(&mut body, &vector(&format!("{label}_nonce_tpm"))).unwrap();
                response(NO_SESSIONS, 0, &body)
            };
            let start_with = |client: &mut Client<Fixed>| {
                client.salted_session_with(
                    0x8000_0000,
                    &vector("primary_x"),
                    &vector("primary_y"),
                    kind,
                    &ephemeral(),
                    &caller,
                )
            };
            let (mut client, sent) = Fixed::client(reply(handle));
            let session = start_with(&mut client).unwrap();
            assert_eq!(sent.borrow().as_slice(), [start]);
            assert_eq!(
                session.key.bytes().to_vec(),
                vector(&format!("{label}_key"))
            );
            assert_eq!(session.handle, handle);
            assert_eq!(client.owned_handles(), 1);
            // The other kind's class is refused, and stays owned to flush.
            let (mut client, _) = Fixed::client(reply(other));
            assert!(
                start_with(&mut client).is_err_and(|error| error.contains("session handle class"))
            );
            assert_eq!(client.owned_handles(), 1);
        }
        // Unsalted policy and trial sessions are class 3 too.
        for kind in [POLICY_SESSION, crate::TRIAL_SESSION] {
            for (handle, accepted) in [(0x0300_0000u32, true), (0x0200_0000, false)] {
                let mut body = handle.to_be_bytes().to_vec();
                put_blob(&mut body, &[0x0e; 32]).unwrap();
                let (mut client, _) = Fixed::client(response(NO_SESSIONS, 0, &body));
                assert_eq!(client.start_session(kind).is_ok(), accepted);
                assert_eq!(client.owned_handles(), 1);
            }
        }
    }

    #[test]
    fn trailing_zeros_are_trimmed_from_an_auth_value() {
        assert_eq!(trim_auth(&vector("auth")), vector("auth_trimmed"));
        assert_eq!(trim_auth(&[1, 0, 2, 0, 0]), [1, 0, 2]);
        assert_eq!(trim_auth(&[0; 32]), [0u8; 0]);
        assert_eq!(trim_auth(&[]), [0u8; 0]);
        assert_eq!(trim_auth(&[0, 7]), [0, 7]);
        // The HMAC under an authValue with trailing zeros is the HMAC under
        // the trimmed one, and differs from one under any other value. (With
        // a 32-byte session key the two key the same HMAC anyway: HMAC pads
        // a key shorter than its 64-byte block with zeros. Trimming matters
        // to the stored userAuth's size, and is what the TPM does.)
        let key = Key::new([3; 32]);
        let mac = |auth: &[u8]| session_hmac(&key, auth, &[1; 32], &[2; 32], &[4; 32], 0);
        assert_eq!(mac(&[9, 0, 0]), mac(&[9]));
        assert_eq!(mac(&[0, 0]), mac(&[]));
        assert_ne!(mac(&[9, 0, 1]), mac(&[9]));
    }

    /// Both directions of AES-128-CFB, each under its own nonce order.
    #[test]
    fn parameter_encryption_matches_the_vectors_both_ways() {
        let key = Key::new(array("policy_key"));
        let auth = vector("auth");
        let (caller, tpm) = (vector("cfb_caller"), vector("cfb_tpm"));
        let plain = vector("cfb_plain");
        let mut value = session_value(&key, &auth);
        assert_eq!(
            kdfa(&value, b"CFB", &caller, &tpm).to_vec(),
            vector("cfb_command_key")
        );
        assert_eq!(
            kdfa(&value, b"CFB", &tpm, &caller).to_vec(),
            vector("cfb_response_key")
        );
        zero(&mut value);
        for (newer, older, expected) in [
            (&caller, &tpm, vector("cfb_command")),
            (&tpm, &caller, vector("cfb_response")),
        ] {
            let mut parameters = Vec::new();
            put_blob(&mut parameters, &plain).unwrap();
            parameters.extend_from_slice(b"rest");
            crypt_first(&mut parameters, &key, &auth, newer, older, true).unwrap();
            assert_eq!(parameters[2..2 + plain.len()], expected);
            assert_eq!(&parameters[2 + plain.len()..], b"rest");
            crypt_first(&mut parameters, &key, &auth, newer, older, false).unwrap();
            assert_eq!(parameters[2..2 + plain.len()], plain);
        }
        for malformed in [&[][..], &[0][..], &[0, 3, 1, 2][..]] {
            let mut parameters = malformed.to_vec();
            assert!(crypt_first(&mut parameters, &key, &auth, &caller, &tpm, true).is_err());
            assert_eq!(parameters, malformed);
        }
    }

    /// Create under the primary in the salted HMAC session, its sensitive
    /// area encrypted, and Unseal in the salted policy session with the
    /// authValue, its reply decrypted: the whole command and reply bytes.
    #[test]
    fn hmac_session_commands_and_replies_match_the_vectors() {
        let template = vector("object_template");
        let public = vector("object_public");
        let payload = vector("payload");
        let auth = vector("auth");
        let mut sensitive = Vec::new();
        put_blob(&mut sensitive, &vector("auth_trimmed")).unwrap();
        put_blob(&mut sensitive, &payload).unwrap();
        let mut parameters = Vec::new();
        put_blob(&mut parameters, &sensitive).unwrap();
        put_blob(&mut parameters, &template).unwrap();
        put_blob(&mut parameters, &[]).unwrap();
        put32(&mut parameters, 0);

        let mut hmac = session("hmac_key", "hmac_nonce_tpm");
        let caller = array("create_caller");
        let name = vector("primary_name");
        let create = session_command(
            crate::CREATE,
            &[0x8000_0000],
            &[&name],
            &hmac,
            &[],
            DECRYPT,
            &parameters,
            &caller,
        )
        .unwrap();
        assert_eq!(create, vector("create"));
        // The sensitive area does not cross in the clear.
        assert!(!create.windows(32).any(|window| window == payload));
        let reply = vector("create_reply");
        assert_eq!(
            session_response(crate::CREATE, &mut hmac, &[], DECRYPT, &caller, &reply).unwrap(),
            vector("create_reply_parameters")
        );
        assert_eq!(hmac.nonce_tpm, array("create_reply_tpm"));

        let mut policy = session("policy_key", "policy_nonce_tpm");
        let caller = array("unseal_caller");
        let name = crate::auth::object_name(&public);
        let unseal = session_command(
            crate::UNSEAL,
            &[0x8000_0001],
            &[&name],
            &policy,
            &auth,
            ENCRYPT,
            &[],
            &caller,
        )
        .unwrap();
        assert_eq!(unseal, vector("unseal"));
        let reply = vector("unseal_reply");
        assert!(!reply.windows(32).any(|window| window == payload));
        let mut expected = Vec::new();
        put_blob(&mut expected, &payload).unwrap();
        assert_eq!(
            session_response(crate::UNSEAL, &mut policy, &auth, ENCRYPT, &caller, &reply).unwrap(),
            expected
        );
        assert_eq!(policy.nonce_tpm, array("unseal_reply_tpm"));

        // The trimmed authValue authorizes the same bytes; any other value,
        // a changed byte, or other attributes do not verify.
        let mut policy = session("policy_key", "policy_nonce_tpm");
        assert_eq!(
            session_command(
                crate::UNSEAL,
                &[0x8000_0001],
                &[&name],
                &policy,
                &vector("auth_trimmed"),
                ENCRYPT,
                &[],
                &caller,
            )
            .unwrap(),
            vector("unseal")
        );
        let mut wrong = auth.clone();
        wrong[0] ^= 1;
        assert!(
            session_response(crate::UNSEAL, &mut policy, &wrong, ENCRYPT, &caller, &reply).is_err()
        );
        for at in 10..reply.len() {
            let mut changed = reply.clone();
            changed[at] ^= 1;
            assert!(
                session_response(
                    crate::UNSEAL,
                    &mut policy,
                    &auth,
                    ENCRYPT,
                    &caller,
                    &changed
                )
                .is_err(),
                "byte {at}"
            );
        }
        assert!(session_response(crate::UNSEAL, &mut policy, &auth, 0, &caller, &reply).is_err());
        assert_eq!(policy.nonce_tpm, array("policy_nonce_tpm"));
    }

    #[test]
    fn session_commands_are_bounded_and_refusals_recorded() {
        let session = session("policy_key", "policy_nonce_tpm");
        assert!(session_command(1, &[1], &[], &session, &[], 0, &[], &[0; 32]).is_err());
        let large = vec![0; MAX_PACKET];
        assert!(session_command(1, &[], &[], &session, &[], 0, &large, &[0; 32]).is_err());
        assert!(session_command(1, &[], &[], &session, &[], DECRYPT, &[], &[0; 32]).is_err());

        let mut session = session;
        let (mut client, _) = Fixed::client(response(NO_SESSIONS, 0x98e, &[]));
        client.handles.push(session.handle);
        let refused = client
            .call_session(crate::UNSEAL, &[1], &[&[]], &mut session, &[], ENCRYPT, &[])
            .unwrap_err();
        assert_eq!(refused, "TPM command 0x15e refused: 0x98e");
        assert_eq!(
            client.refused,
            Some(Refusal {
                command: crate::UNSEAL,
                rc: 0x98e
            })
        );
        // A refused command leaves its session owned, so drop flushes it.
        assert_eq!(client.owned_handles(), 1);
        let (mut client, _) = Fixed::client(response(NO_SESSIONS, 0x98e, &[0]));
        assert!(client
            .call_session(crate::UNSEAL, &[1], &[&[]], &mut session, &[], 0, &[])
            .is_err());
        assert_eq!(client.refused, None);
    }

    /// A TPM answering one Unseal in `key`'s session: verified, with its
    /// HMAC forged, or truncated.
    struct Answering {
        key: Key,
        reply: Reply,
    }
    #[derive(Clone, Copy, PartialEq)]
    enum Reply {
        Verified,
        Forged,
        Truncated,
    }
    impl Transport for Answering {
        fn exchange(&mut self, command: &[u8]) -> Result<Vec<u8>, String> {
            // Drop's FlushContext.
            if command.len() < 56 {
                return Ok(response(NO_SESSIONS, 0, &[]));
            }
            // One handle, the area size and the session handle precede the
            // caller's nonce.
            let caller = &command[24..56];
            let nonce = [0x6e; 32];
            let mut mac = session_hmac(
                &self.key,
                &[],
                &rp_hash(crate::UNSEAL, &[]),
                &nonce,
                caller,
                0,
            );
            if self.reply == Reply::Forged {
                mac[0] ^= 1;
            }
            let mut body = vec![0; 4];
            put_blob(&mut body, &nonce).unwrap();
            body.push(0);
            put_blob(&mut body, &mac).unwrap();
            let mut reply = response(SESSIONS, 0, &body);
            if self.reply == Reply::Truncated {
                reply.truncate(reply.len() - 1);
                let size = (reply.len() as u32).to_be_bytes();
                reply[2..6].copy_from_slice(&size);
            }
            Ok(reply)
        }
    }

    #[test]
    fn only_a_verified_success_ends_the_session_s_ownership() {
        for (reply, verified) in [
            (Reply::Verified, true),
            (Reply::Forged, false),
            (Reply::Truncated, false),
        ] {
            let mut session = session("policy_key", "policy_nonce_tpm");
            let mut client = Client::new(Answering {
                key: Key::new(array("policy_key")),
                reply,
            });
            client.handles.push(session.handle);
            let answered =
                client.call_session(crate::UNSEAL, &[1], &[&[]], &mut session, &[], 0, &[]);
            assert_eq!(answered.is_ok(), verified);
            assert_eq!(client.owned_handles(), usize::from(!verified));
            assert_eq!(client.refused, None);
        }
    }

    #[test]
    fn policy_auth_value_sends_its_command_bytes() {
        let (mut client, sent) = Fixed::client(response(NO_SESSIONS, 0, &[]));
        client
            .call(POLICY_AUTH_VALUE, &[0x0300_0000], None, &[], false)
            .unwrap();
        assert_eq!(sent.borrow().as_slice(), [vector("policy_auth_value")]);
    }

    #[test]
    fn kdf_labels_end_with_their_zero_octet() {
        let mut with = Vec::new();
        with.extend_from_slice(&1u32.to_be_bytes());
        with.extend_from_slice(b"z");
        with.extend_from_slice(b"SECRET\0uv");
        assert_eq!(kdfe(b"z", b"SECRET", b"u", b"v"), digest(&with));
        assert_eq!(
            kdfa(b"k", b"ATH", b"u", b"v"),
            td_fido::hmac_sha256(b"k", &[&[0, 0, 0, 1], b"ATH\0uv", &[0, 0, 1, 0]])
        );
    }
}
