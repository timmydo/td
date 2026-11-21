//! CTAP2 getAssertion for a website. The caller binds the request to a
//! trusted presentation and constructs clientDataJSON for the origin.
//! Only the relying party holds the public key that verifies the signature.

use crate::fido_cbor::{self as cbor, Encoder, Value};
use crate::fido_ctap::{encode_descriptor, RP_ID};

const MAX_WEB_CREDENTIAL_ID: usize = 1023;

#[derive(Debug, Eq, PartialEq)]
pub enum Error {
    RelyingParty,
    Credential,
    MessageSize,
    Malformed,
    Status(u8),
    AccountSelection,
    Presence,
    Verification,
}

impl From<String> for Error {
    fn from(_: String) -> Self {
        Self::Malformed
    }
}

/// The exact authenticator input for user verification. A PIN parameter
/// is the MAC of this request's client-data hash from an authorized PIN
/// transaction. The caller keeps the PIN and token outside the browser.
#[derive(Clone, Copy)]
pub enum Verification<'a> {
    Presence,
    BuiltIn,
    PinProtocolOne(&'a [u8; 16]),
    PinProtocolTwo(&'a [u8; 32]),
}

impl Verification<'_> {
    fn pin(&self) -> Option<(u64, &[u8])> {
        match self {
            Self::PinProtocolOne(mac) => Some((1, mac.as_slice())),
            Self::PinProtocolTwo(mac) => Some((2, mac.as_slice())),
            _ => None,
        }
    }
}

/// Immutable inputs for one website assertion. Origin authorization belongs
/// to the caller. The authenticator's internal td RP is never a website RP.
pub struct Request<'a> {
    rp: &'a str,
    hash: &'a [u8; 32],
    allow: &'a [&'a [u8]],
    verification: Verification<'a>,
    max_message: usize,
}

pub struct Assertion<'a> {
    pub credential: &'a [u8],
    pub authenticator_data: &'a [u8],
    pub signature: &'a [u8],
    pub user_handle: Option<&'a [u8]>,
}

impl<'a> Request<'a> {
    pub fn new(
        rp: &'a str,
        hash: &'a [u8; 32],
        allow: &'a [&'a [u8]],
        verification: Verification<'a>,
        max_message: usize,
    ) -> Result<Self, Error> {
        if rp.is_empty()
            || rp.len() > 253
            || rp == RP_ID
            || !rp.split('.').all(|label| {
                (1..=63).contains(&label.len())
                    && !label.starts_with('-')
                    && !label.ends_with('-')
                    && label
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            })
        {
            return Err(Error::RelyingParty);
        }
        // Even the smallest encoded descriptor takes more than one byte.
        if allow.len() > cbor::MAX_BYTES {
            return Err(Error::MessageSize);
        }
        if allow
            .iter()
            .any(|id| id.is_empty() || id.len() > MAX_WEB_CREDENTIAL_ID)
        {
            return Err(Error::Credential);
        }
        Ok(Self {
            rp,
            hash,
            allow,
            verification,
            max_message: max_message.min(cbor::MAX_BYTES),
        })
    }

    /// Writes a complete authenticatorGetAssertion command into caller storage.
    pub fn encode<'b>(&self, buffer: &'b mut [u8]) -> Result<&'b [u8], Error> {
        let mut encoded = self.encode_payload().map_err(|_| Error::MessageSize)?;

        let size = encoded.len() + 1;
        let Some(command) = buffer.get_mut(..size).filter(|_| size <= self.max_message) else {
            encoded.fill(0);
            return Err(Error::MessageSize);
        };
        let (code, body) = command.split_first_mut().ok_or(Error::MessageSize)?;
        *code = 2;
        body.copy_from_slice(&encoded);
        encoded.fill(0);
        Ok(command)
    }

    fn encode_payload(&self) -> Result<Vec<u8>, String> {
        let pin = self.verification.pin();
        let mut out = Encoder::new();
        out.head(
            5,
            3 + u64::from(!self.allow.is_empty()) + 2 * u64::from(pin.is_some()),
        )?;
        out.head(0, 1)?;
        out.text(self.rp)?;
        out.head(0, 2)?;
        out.bytes(self.hash)?;
        if !self.allow.is_empty() {
            out.head(0, 3)?;
            out.head(4, self.allow.len() as u64)?;
            for credential in self.allow {
                encode_descriptor(&mut out, credential)?;
            }
        }
        out.head(0, 5)?;
        let uv = matches!(self.verification, Verification::BuiltIn);
        out.head(5, 1 + u64::from(uv))?;
        out.text("up")?;
        out.boolean(true)?;
        if uv {
            out.text("uv")?;
            out.boolean(true)?;
        }
        if let Some((protocol, mac)) = pin {
            out.head(0, 6)?;
            out.bytes(mac)?;
            out.head(0, 7)?;
            out.head(0, protocol)?;
        }
        out.finish()
    }

    /// The caller consumes one reply. Multiple accounts need a selection
    /// ceremony before any of their assertions can be released.
    pub fn finish<'b>(self, reply: &'b [u8]) -> Result<Assertion<'b>, Error>
    where
        'a: 'b,
    {
        if reply.len() > cbor::MAX_BYTES {
            return Err(Error::MessageSize);
        }
        let (&status, body) = reply.split_first().ok_or(Error::Malformed)?;
        if status != 0 {
            return Err(Error::Status(status));
        }
        let value = cbor::decode(body)?;
        if let Some(count) = value.get(&Value::Unsigned(5))? {
            match count.unsigned()? {
                0 => return Err(Error::Malformed),
                1 => (),
                _ => return Err(Error::AccountSelection),
            }
        }
        let credential = match value.get(&Value::Unsigned(1))? {
            None => match self.allow {
                [only] => *only,
                _ => return Err(Error::Credential),
            },
            Some(descriptor) => {
                let id = descriptor.required(&Value::Text("id"))?.bytes()?;
                if descriptor.required(&Value::Text("type"))?.text()? != "public-key"
                    || id.is_empty()
                    || id.len() > MAX_WEB_CREDENTIAL_ID
                    || (!self.allow.is_empty() && !self.allow.contains(&id))
                {
                    return Err(Error::Credential);
                }
                id
            }
        };
        let data = value.required(&Value::Unsigned(2))?.bytes()?;
        if data.get(..32) != Some(crate::digest(self.rp.as_bytes()).as_slice()) {
            return Err(Error::RelyingParty);
        }
        let flags = *data.get(32).ok_or(Error::Malformed)?;
        // This codec requests no extensions and assertions carry no AT data.
        if data.len() != 37 || flags & 0xc0 != 0 || flags & 0x18 == 0x10 {
            return Err(Error::Malformed);
        }
        if flags & 1 == 0 {
            return Err(Error::Presence);
        }
        if !matches!(self.verification, Verification::Presence) && flags & 4 == 0 {
            return Err(Error::Verification);
        }
        let signature = value.required(&Value::Unsigned(3))?.bytes()?;
        if signature.is_empty() {
            return Err(Error::Malformed);
        }
        let user_handle = value
            .get(&Value::Unsigned(4))?
            .map(|user| user.required(&Value::Text("id"))?.bytes())
            .transpose()?;
        if user_handle.is_some_and(|id| id.is_empty() || id.len() > 64)
            || (user_handle.is_none() && self.allow.is_empty())
        {
            return Err(Error::Credential);
        }
        Ok(Assertion {
            credential,
            authenticator_data: data,
            signature,
            user_handle,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RP: &str = "accounts.example";
    const HASH: &[u8; 32] = &[0xab; 32];
    const ALLOW: &[&[u8]] = &[b"key"];

    fn request<'a>(allow: &'a [&'a [u8]], verification: Verification<'a>) -> Request<'a> {
        Request::new(RP, HASH, allow, verification, 1024).unwrap()
    }

    #[test]
    fn encodes_command_and_user_verification() {
        for (method, options, protocol, mac) in [
            (Verification::Presence, 1, None, None),
            (Verification::BuiltIn, 2, None, None),
            (
                Verification::PinProtocolOne(&[1; 16]),
                1,
                Some(1),
                Some(&[1; 16][..]),
            ),
            (
                Verification::PinProtocolTwo(&[2; 32]),
                1,
                Some(2),
                Some(&[2; 32][..]),
            ),
        ] {
            let mut buffer = [0; 256];
            let encoded = request(ALLOW, method).encode(&mut buffer).unwrap();
            let (code, payload) = encoded.split_first().unwrap();
            assert_eq!(*code, 2);
            let map = cbor::decode(payload).unwrap();
            assert_eq!(
                map.required(&Value::Unsigned(1)).unwrap().text().unwrap(),
                RP
            );
            assert_eq!(
                map.required(&Value::Unsigned(2)).unwrap().bytes().unwrap(),
                HASH
            );
            let Value::Array(descriptors) = map.required(&Value::Unsigned(3)).unwrap() else {
                panic!()
            };
            assert_eq!(descriptors.len(), 1);
            assert_eq!(
                descriptors[0]
                    .required(&Value::Text("id"))
                    .unwrap()
                    .bytes()
                    .unwrap(),
                ALLOW[0]
            );
            assert_eq!(
                descriptors[0]
                    .required(&Value::Text("type"))
                    .unwrap()
                    .text()
                    .unwrap(),
                "public-key"
            );
            let selection = map.required(&Value::Unsigned(5)).unwrap();
            assert_eq!(selection.map().unwrap().len(), options);
            assert!(selection.required(&Value::Text("up")).unwrap() == &Value::Simple(21));
            if options == 2 {
                assert!(selection.required(&Value::Text("uv")).unwrap() == &Value::Simple(21));
            }
            assert_eq!(
                map.get(&Value::Unsigned(6))
                    .unwrap()
                    .map(|v| v.bytes().unwrap()),
                mac
            );
            assert_eq!(
                map.get(&Value::Unsigned(7))
                    .unwrap()
                    .map(|v| v.unsigned().unwrap()),
                protocol
            );
        }
        let mut buffer = [0; 128];
        let encoded = request(&[], Verification::Presence)
            .encode(&mut buffer)
            .unwrap();
        assert!(cbor::decode(&encoded[1..])
            .unwrap()
            .get(&Value::Unsigned(3))
            .unwrap()
            .is_none());
    }

    fn reply(
        rp: &str,
        flags: u8,
        descriptor: Option<&[u8]>,
        handle: Option<&[u8]>,
        count: u64,
    ) -> Vec<u8> {
        let mut e = Encoder::new();
        e.head(
            5,
            3 + u64::from(descriptor.is_some()) + u64::from(handle.is_some()),
        )
        .unwrap();
        if let Some(id) = descriptor {
            e.head(0, 1).unwrap();
            encode_descriptor(&mut e, id).unwrap();
        }
        let mut data = [0; 37];
        data[..32].copy_from_slice(&crate::digest(rp.as_bytes()));
        data[32] = flags;
        e.head(0, 2).unwrap();
        e.bytes(&data).unwrap();
        e.head(0, 3).unwrap();
        e.bytes(b"signature").unwrap();
        if let Some(id) = handle {
            e.head(0, 4).unwrap();
            e.head(5, 1).unwrap();
            e.text("id").unwrap();
            e.bytes(id).unwrap();
        }
        e.head(0, 5).unwrap();
        e.head(0, count).unwrap();
        let mut result = vec![0];
        result.extend(e.finish().unwrap());
        result
    }

    #[test]
    fn resolves_scoped_and_discoverable_credentials() {
        let response = reply(RP, 5, None, None, 1);
        let result = request(ALLOW, Verification::BuiltIn)
            .finish(&response)
            .unwrap();
        assert_eq!(result.credential, b"key");
        assert_eq!(result.authenticator_data[32], 5);
        assert_eq!(result.signature, b"signature");
        assert!(result.user_handle.is_none());
        let response = reply(RP, 1, Some(b"resident"), Some(b"account"), 1);
        let result = request(&[], Verification::Presence)
            .finish(&response)
            .unwrap();
        assert_eq!(result.credential, b"resident");
        assert_eq!(result.user_handle, Some(&b"account"[..]));
    }

    #[test]
    fn rejects_misbound_and_ambiguous_replies() {
        for (response, error) in [
            (reply(RP_ID, 5, None, None, 1), Error::RelyingParty),
            (reply(RP, 4, None, None, 1), Error::Presence),
            (reply(RP, 1, None, None, 1), Error::Verification),
            (reply(RP, 0x15, None, None, 1), Error::Malformed),
            (reply(RP, 0x45, None, None, 1), Error::Malformed),
            (reply(RP, 0x85, None, None, 1), Error::Malformed),
            (reply(RP, 5, Some(b"other"), None, 1), Error::Credential),
            (reply(RP, 5, None, None, 2), Error::AccountSelection),
            (reply(RP, 5, None, None, 0), Error::Malformed),
            (vec![0x2e], Error::Status(0x2e)),
        ] {
            assert_eq!(
                request(ALLOW, Verification::BuiltIn)
                    .finish(&response)
                    .err(),
                Some(error)
            );
        }
        let response = reply(RP, 1, Some(b"id"), None, 1);
        assert_eq!(
            request(&[], Verification::Presence).finish(&response).err(),
            Some(Error::Credential)
        );
        let response = reply(RP, 1, None, None, 1);
        assert_eq!(
            request(&[b"a", b"b"], Verification::Presence)
                .finish(&response)
                .err(),
            Some(Error::Credential)
        );
    }

    #[test]
    fn refuses_local_identity_and_oversized_commands() {
        for rp in [
            RP_ID,
            "TD.INVALID",
            "td.invalid.",
            "",
            "a..b",
            "-a",
            "https://example.org",
        ] {
            assert_eq!(
                Request::new(rp, HASH, ALLOW, Verification::Presence, 256).err(),
                Some(Error::RelyingParty)
            );
        }
        assert_eq!(
            Request::new(RP, HASH, &[&[]], Verification::Presence, 256).err(),
            Some(Error::Credential)
        );
        let mut buffer = [0xff; 256];
        let r = Request::new(RP, HASH, ALLOW, Verification::Presence, 8).unwrap();
        assert_eq!(r.encode(&mut buffer), Err(Error::MessageSize));
        assert_eq!(buffer, [0xff; 256]);
        assert_eq!(
            r.finish(&[0; cbor::MAX_BYTES + 1]).err(),
            Some(Error::MessageSize)
        );
        let long_list = vec![&b"x"[..]; cbor::MAX_BYTES + 1];
        assert_eq!(
            Request::new(RP, HASH, &long_list, Verification::Presence, 1024).err(),
            Some(Error::MessageSize)
        );
        let id = [1; 1000];
        let response = reply(RP, 1, Some(&id), Some(b"account"), 1);
        let r = Request::new(RP, HASH, &[], Verification::Presence, 1024).unwrap();
        assert_eq!(r.finish(&response).unwrap().credential, &id);
        assert_eq!(
            request(&[id.as_slice(); 8], Verification::Presence).encode(&mut buffer),
            Err(Error::MessageSize)
        );
        for length in [1023, 1024] {
            let id = &[1; 1024][..length];
            let allow = &[id][..];
            let r = Request::new(RP, HASH, allow, Verification::Presence, cbor::MAX_BYTES);
            assert_eq!(r.is_ok(), length == 1023);
            let response = reply(RP, 1, Some(id), Some(b"account"), 1);
            assert_eq!(
                Request::new(RP, HASH, &[], Verification::Presence, 1024)
                    .unwrap()
                    .finish(&response)
                    .is_ok(),
                length == 1023
            );
        }
    }
}
