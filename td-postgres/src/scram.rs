//! SCRAM-SHA-256 (RFC 5802, RFC 7677) as a client, without channel
//! binding: the GS2 header is `n,,`, which tells a server offering
//! SCRAM-SHA-256-PLUS that this client does not support binding.
//!
//! The server is not trusted. Its first message must extend this
//! client's nonce, give a canonical base64 salt and an iteration count
//! within the caller's bound, and carry nothing else. Its final message
//! must prove it holds the password's verifier, compared in constant
//! time; the client accepts AuthenticationOk only after that proof.
use crate::Error;
use td_primitives::hmac::{mac, pbkdf2_sha256};
use td_primitives::sha::{Digest, Sha256};
use td_primitives::{ct, random};

pub(crate) const MECHANISM: &[u8] = b"SCRAM-SHA-256";
/// `base64("n,,")`, the channel-binding attribute for no binding.
const CHANNEL_BINDING: &[u8] = b"c=biws";
/// The client nonce: 18 random bytes as 24 base64 characters.
const NONCE_BYTES: usize = 18;
const NONCE_LEN: usize = 24;
/// The longest salt accepted, decoded. PostgreSQL's is 16 bytes.
const MAX_SALT: usize = 128;

#[derive(Debug, PartialEq, Eq)]
enum Stage {
    First,
    Final,
    Done,
}

/// One exchange: its nonce, and once the server's first message is
/// answered, the server signature it must return.
pub(crate) struct Exchange<'a> {
    user: &'a [u8],
    password: &'a [u8],
    nonce: [u8; 64],
    nonce_len: usize,
    server_signature: [u8; 32],
    stage: Stage,
}

impl Drop for Exchange<'_> {
    fn drop(&mut self) {
        ct::zero(&mut self.server_signature);
    }
}

impl<'a> Exchange<'a> {
    /// An exchange with a fresh random nonce. PostgreSQL takes the user
    /// from the startup message and ignores SCRAM's, so `user` is empty
    /// there.
    pub(crate) fn new(user: &'a [u8], password: &'a [u8]) -> Result<Self, Error> {
        let mut random = [0u8; NONCE_BYTES];
        random::fill(&mut random).map_err(|_| Error::Io)?;
        let mut nonce = [0u8; NONCE_LEN];
        let written = td_encoding::base64_into(&random, &mut nonce).ok_or(Error::Invalid)?;
        ct::zero(&mut random);
        Self::with_nonce(user, password, nonce.get(..written).ok_or(Error::Invalid)?)
    }

    /// An exchange with a given nonce, which must be printable, without a
    /// comma, and at most 64 bytes. `user` may hold neither `,` nor `=`,
    /// which SCRAM would have to escape.
    pub(crate) fn with_nonce(
        user: &'a [u8],
        password: &'a [u8],
        nonce: &[u8],
    ) -> Result<Self, Error> {
        if nonce.is_empty()
            || !nonce.iter().all(|&byte| printable(byte))
            || user
                .iter()
                .any(|&byte| byte == b',' || byte == b'=' || byte == 0)
        {
            return Err(Error::Invalid);
        }
        let mut stored = [0u8; 64];
        stored
            .get_mut(..nonce.len())
            .ok_or(Error::Invalid)?
            .copy_from_slice(nonce);
        Ok(Self {
            user,
            password,
            nonce: stored,
            nonce_len: nonce.len(),
            server_signature: [0; 32],
            stage: Stage::First,
        })
    }

    fn nonce(&self) -> &[u8] {
        self.nonce.get(..self.nonce_len).unwrap_or(&[])
    }

    /// The client-first message: the GS2 header, then the bare message.
    pub(crate) fn client_first(&self, out: &mut Vec<u8>) -> Result<(), Error> {
        td_alloc::extend_from_slice(out, b"n,,")?;
        self.client_first_bare(out)
    }

    fn client_first_bare(&self, out: &mut Vec<u8>) -> Result<(), Error> {
        td_alloc::extend_from_slice(out, b"n=")?;
        td_alloc::extend_from_slice(out, self.user)?;
        td_alloc::extend_from_slice(out, b",r=")?;
        Ok(td_alloc::extend_from_slice(out, self.nonce())?)
    }

    /// Answer the server-first message with the client-final message,
    /// keeping the server signature the server-final must carry. More than
    /// `max_iterations` is `Limit`; any malformed or hostile message is
    /// `Authentication`.
    pub(crate) fn client_final(
        &mut self,
        server_first: &[u8],
        max_iterations: u32,
        out: &mut Vec<u8>,
    ) -> Result<(), Error> {
        if self.stage != Stage::First {
            return Err(Error::Authentication);
        }
        self.stage = Stage::Done;
        let mut attributes = server_first.split(|&byte| byte == b',');
        let (Some(combined), Some(salt), Some(iterations), None) = (
            attributes.next().and_then(|a| a.strip_prefix(b"r=")),
            attributes.next().and_then(|a| a.strip_prefix(b"s=")),
            attributes.next().and_then(|a| a.strip_prefix(b"i=")),
            attributes.next(),
        ) else {
            return Err(Error::Authentication);
        };
        if combined.len() <= self.nonce_len
            || !combined.starts_with(self.nonce())
            || !combined.iter().all(|&byte| printable(byte))
        {
            return Err(Error::Authentication);
        }
        let mut salt_bytes = [0u8; MAX_SALT];
        let salt_len = td_encoding::from_base64_into(salt, &mut salt_bytes)
            .filter(|&length| length > 0)
            .ok_or(Error::Authentication)?;
        let salt = salt_bytes.get(..salt_len).ok_or(Error::Authentication)?;
        let iterations = iteration_count(iterations)?;
        if iterations > max_iterations {
            return Err(Error::Limit);
        }

        let mut salted = [0u8; 32];
        pbkdf2_sha256(self.password, salt, iterations, &mut salted)
            .map_err(|_| Error::Authentication)?;
        let mut client_key = mac::<Sha256>(&salted, &[b"Client Key"]);
        let mut stored_key = Sha256::digest(&client_key);
        let mut server_key = mac::<Sha256>(&salted, &[b"Server Key"]);
        ct::zero(&mut salted);

        // AuthMessage = client-first-bare "," server-first ","
        //               client-final-without-proof
        let parts: [&[u8]; 10] = [
            b"n=",
            self.user,
            b",r=",
            self.nonce(),
            b",",
            server_first,
            b",",
            CHANNEL_BINDING,
            b",r=",
            combined,
        ];
        let mut client_signature = mac::<Sha256>(&stored_key, &parts);
        self.server_signature = mac::<Sha256>(&server_key, &parts);
        let mut proof = [0u8; 32];
        for ((out, key), signature) in proof.iter_mut().zip(&client_key).zip(&client_signature) {
            *out = key ^ signature;
        }
        ct::zero(&mut client_key);
        ct::zero(&mut stored_key);
        ct::zero(&mut server_key);
        ct::zero(&mut client_signature);

        let mut encoded = [0u8; 44];
        let encoded_len = td_encoding::base64_into(&proof, &mut encoded).ok_or(Error::Invalid)?;
        ct::zero(&mut proof);
        td_alloc::extend_from_slice(out, CHANNEL_BINDING)?;
        td_alloc::extend_from_slice(out, b",r=")?;
        td_alloc::extend_from_slice(out, combined)?;
        td_alloc::extend_from_slice(out, b",p=")?;
        td_alloc::extend_from_slice(out, encoded.get(..encoded_len).ok_or(Error::Invalid)?)?;
        self.stage = Stage::Final;
        Ok(())
    }

    /// Check the server-final message's signature. Anything but exactly
    /// `v=` and the expected signature, a server error `e=` included, is
    /// `Authentication`.
    pub(crate) fn verify(&mut self, server_final: &[u8]) -> Result<(), Error> {
        if self.stage != Stage::Final {
            return Err(Error::Authentication);
        }
        self.stage = Stage::Done;
        let encoded = server_final
            .strip_prefix(b"v=")
            .ok_or(Error::Authentication)?;
        let mut signature = [0u8; 32];
        let length =
            td_encoding::from_base64_into(encoded, &mut signature).ok_or(Error::Authentication)?;
        let matched = length == 32 && ct::eq(&signature, &self.server_signature);
        ct::zero(&mut self.server_signature);
        if matched {
            Ok(())
        } else {
            Err(Error::Authentication)
        }
    }
}

/// RFC 5802's `printable`: 0x21 to 0x7e but the comma.
fn printable(byte: u8) -> bool {
    (0x21..=0x7e).contains(&byte) && byte != b','
}

/// A positive decimal count without a leading zero, as RFC 5802's
/// `posit-number` spells it, that fits a u32.
fn iteration_count(text: &[u8]) -> Result<u32, Error> {
    match text.first() {
        Some(b'1'..=b'9') if text.len() <= 10 && text.iter().all(u8::is_ascii_digit) => {}
        _ => return Err(Error::Authentication),
    }
    let mut value = 0u64;
    for &digit in text {
        value = value * 10 + u64::from(digit - b'0');
    }
    u32::try_from(value).map_err(|_| Error::Authentication)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const NONCE: &[u8] = b"rOprNGfwEbeRWgbNEkqO";
    const SERVER_FIRST: &[u8] =
        b"r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096";

    // RFC 7677 3, the SCRAM-SHA-256 exchange for user "user", password
    // "pencil".
    #[test]
    fn rfc_7677() {
        let mut exchange = Exchange::with_nonce(b"user", b"pencil", NONCE).unwrap();
        let mut first = Vec::new();
        exchange.client_first(&mut first).unwrap();
        assert_eq!(first, b"n,,n=user,r=rOprNGfwEbeRWgbNEkqO");
        let mut last = Vec::new();
        exchange
            .client_final(SERVER_FIRST, 4096, &mut last)
            .unwrap();
        assert_eq!(
            last,
            &b"c=biws,r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,\
               p=dHzbZapWIk4jUhN+Ute9ytag9zjfMHgsqmmiz7AndVQ="[..]
        );
        assert_eq!(
            exchange.verify(b"v=6rriTRBi23WpRR/wtup+mMhUZUn/dB5nLTJRsjl95G4="),
            Ok(())
        );
    }

    // PostgreSQL's form, the user left empty, from Python's hashlib and
    // hmac over the same password, salt and nonces.
    #[test]
    fn postgresql_empty_user() {
        let mut exchange = Exchange::with_nonce(b"", b"pencil", NONCE).unwrap();
        let mut first = Vec::new();
        exchange.client_first(&mut first).unwrap();
        assert_eq!(first, b"n,,n=,r=rOprNGfwEbeRWgbNEkqO");
        let mut last = Vec::new();
        exchange
            .client_final(SERVER_FIRST, 4096, &mut last)
            .unwrap();
        assert!(last.ends_with(b",p=qvT2SWdEH5Q06albL+hjSYuUhCG7VndFyzIb7CK4n9k="));
        assert_eq!(
            exchange.verify(b"v=3HO6Qt1M4MKJrmlKaoOqLAI0/0TV0HZe7J9H3MBtSOg="),
            Ok(())
        );
    }

    fn answer(server_first: &[u8], max: u32) -> Result<(), Error> {
        let mut exchange = Exchange::with_nonce(b"", b"pencil", NONCE).unwrap();
        exchange.client_final(server_first, max, &mut Vec::new())
    }

    #[test]
    fn a_hostile_server_first_message_is_refused() {
        let salt = ",s=W22ZaJ0SNY7soEsUEjb6gQ==";
        for bad in [
            // The nonce not extended, not ours, or extended unprintably.
            format!("r=rOprNGfwEbeRWgbNEkqO{salt},i=4096"),
            format!("r=XOprNGfwEbeRWgbNEkqOabc{salt},i=4096"),
            format!("r=rOprNGfwEbeRWgbNEkqO\x7f{salt},i=4096"),
            format!("r=rOprNGfwEbeRWgbNEkqO {salt},i=4096"),
            // A mandatory extension, attributes reordered, missing or added.
            format!("m=x,r=rOprNGfwEbeRWgbNEkqOabc{salt},i=4096"),
            format!("{},r=rOprNGfwEbeRWgbNEkqOabc,i=4096", &salt[1..]),
            "r=rOprNGfwEbeRWgbNEkqOabc,i=4096".to_string(),
            format!("r=rOprNGfwEbeRWgbNEkqOabc{salt}"),
            format!("r=rOprNGfwEbeRWgbNEkqOabc{salt},i=4096,x=1"),
            format!("r=rOprNGfwEbeRWgbNEkqOabc{salt},i=4096,"),
            // A salt empty, not canonical base64, or longer than 128 bytes.
            "r=rOprNGfwEbeRWgbNEkqOabc,s=,i=4096".to_string(),
            "r=rOprNGfwEbeRWgbNEkqOabc,s=W22ZaJ0SNY7soEsUEjb6gQ=,i=4096".to_string(),
            "r=rOprNGfwEbeRWgbNEkqOabc,s=W22ZaJ0SNY7soEsUEjb6gR==,i=4096".to_string(),
            format!("r=rOprNGfwEbeRWgbNEkqOabc,s={},i=4096", "A".repeat(176)),
            // A count zero, signed, padded, spaced, empty or past u32.
            format!("r=rOprNGfwEbeRWgbNEkqOabc{salt},i=0"),
            format!("r=rOprNGfwEbeRWgbNEkqOabc{salt},i=+4096"),
            format!("r=rOprNGfwEbeRWgbNEkqOabc{salt},i=04096"),
            format!("r=rOprNGfwEbeRWgbNEkqOabc{salt},i= 4096"),
            format!("r=rOprNGfwEbeRWgbNEkqOabc{salt},i="),
            format!("r=rOprNGfwEbeRWgbNEkqOabc{salt},i=4294967296"),
            format!("r=rOprNGfwEbeRWgbNEkqOabc{salt},i=99999999999"),
        ] {
            assert_eq!(
                answer(bad.as_bytes(), u32::MAX),
                Err(Error::Authentication),
                "{bad}"
            );
        }
        // A 128-byte salt is the longest taken.
        let longest = format!("r=rOprNGfwEbeRWgbNEkqOabc,s={}AAA=,i=1", "A".repeat(168));
        assert_eq!(answer(longest.as_bytes(), 1), Ok(()));
        // More iterations than the caller allows is its own error.
        assert_eq!(answer(SERVER_FIRST, 4095), Err(Error::Limit));
        assert_eq!(answer(SERVER_FIRST, 4096), Ok(()));
    }

    #[test]
    fn the_server_must_prove_the_password_once_and_in_order() {
        let mut exchange = Exchange::with_nonce(b"", b"pencil", NONCE).unwrap();
        // No proof is checked before the client has answered.
        assert_eq!(
            exchange.verify(b"v=3HO6Qt1M4MKJrmlKaoOqLAI0/0TV0HZe7J9H3MBtSOg="),
            Err(Error::Authentication)
        );
        let mut exchange = Exchange::with_nonce(b"", b"pencil", NONCE).unwrap();
        exchange
            .client_final(SERVER_FIRST, 4096, &mut Vec::new())
            .unwrap();
        assert_eq!(
            exchange.client_final(SERVER_FIRST, 4096, &mut Vec::new()),
            Err(Error::Authentication)
        );
        for bad in [
            &b"v=3HO6Qt1M4MKJrmlKaoOqLAI0/0TV0HZe7J9H3MBtSOh="[..],
            b"v=6rriTRBi23WpRR/wtup+mMhUZUn/dB5nLTJRsjl95G4=",
            b"v=3HO6Qt1M4MKJrmlKaoOqLAI0/0TV0HZe7J9H3MBtSOg=,x=1",
            b"v=3HO6Qt1M4MKJrmlKaoOqLAI0/0TV0HZe7J9H3MBt",
            b"e=invalid-proof",
            b"",
        ] {
            let mut exchange = Exchange::with_nonce(b"", b"pencil", NONCE).unwrap();
            exchange
                .client_final(SERVER_FIRST, 4096, &mut Vec::new())
                .unwrap();
            assert_eq!(exchange.verify(bad), Err(Error::Authentication), "{bad:?}");
        }
        let mut exchange = Exchange::with_nonce(b"", b"pencil", NONCE).unwrap();
        exchange
            .client_final(SERVER_FIRST, 4096, &mut Vec::new())
            .unwrap();
        let good = b"v=3HO6Qt1M4MKJrmlKaoOqLAI0/0TV0HZe7J9H3MBtSOg=";
        assert_eq!(exchange.verify(good), Ok(()));
        assert_eq!(exchange.verify(good), Err(Error::Authentication));
    }

    #[test]
    fn fresh_nonces_are_printable_and_differ() {
        let a = Exchange::new(b"", b"pencil").unwrap();
        let b = Exchange::new(b"", b"pencil").unwrap();
        assert_eq!(a.nonce().len(), 24);
        assert!(a.nonce().iter().all(|&byte| printable(byte)));
        assert_ne!(a.nonce(), b.nonce());
        assert!(Exchange::with_nonce(b"a,b", b"x", NONCE).is_err());
        assert!(Exchange::with_nonce(b"a=b", b"x", NONCE).is_err());
        assert!(Exchange::with_nonce(b"", b"x", b"a,b").is_err());
        assert!(Exchange::with_nonce(b"", b"x", &[b'a'; 65]).is_err());
    }
}
