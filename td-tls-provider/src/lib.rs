//! td-primitives behind rustls's `CryptoProvider`: everything rustls asks of a
//! crypto library, with nothing chosen here that rustls could not already
//! refuse. The engine stays rustls's, HKDF and the TLS 1.2 PRF included, which it
//! derives from the HMAC below (td-primitives/DESIGN.md). td-tls-provider/DESIGN.md
//! is the contract: what is here, and that each consumer builds its own policy.
//!
//! Record layouts follow rustls's own providers. Each wrong-shape case refuses:
//! a key of the wrong length makes a cipher that seals and opens nothing.

use std::marker::PhantomData;
use std::sync::Arc;

use rustls::crypto::cipher::{
    make_tls12_aad, make_tls13_aad, AeadKey, InboundOpaqueMessage, InboundPlainMessage, Iv,
    KeyBlockShape, MessageDecrypter, MessageEncrypter, Nonce, OutboundOpaqueMessage,
    OutboundPlainMessage, PrefixedPayload, Tls12AeadAlgorithm, Tls13AeadAlgorithm,
    UnsupportedOperationError,
};
use rustls::crypto::hash::{self, HashAlgorithm};
use rustls::crypto::tls12::PrfUsingHmac;
use rustls::crypto::tls13::HkdfUsingHmac;
use rustls::crypto::{
    hmac, ActiveKeyExchange, CryptoProvider, GetRandomFailed, KeyExchangeAlgorithm, KeyProvider,
    SecureRandom, SharedSecret, SupportedKxGroup, WebPkiSupportedAlgorithms,
};
use rustls::pki_types::SubjectPublicKeyInfoDer;
use rustls::pki_types::{
    alg_id, AlgorithmIdentifier, InvalidSignature, PrivateKeyDer, SignatureVerificationAlgorithm,
};
use rustls::sign::{public_key_to_spki, Signer, SigningKey};
use rustls::{
    CipherSuite, CipherSuiteCommon, ConnectionTrafficSecrets, ContentType, Error, NamedGroup,
    PeerMisbehaved, ProtocolVersion, SignatureAlgorithm, SignatureScheme, SupportedCipherSuite,
    Tls12CipherSuite, Tls13CipherSuite,
};
use td_primitives::aes_gcm::AesGcm;
use td_primitives::curve25519::x25519;
use td_primitives::ec::{p256, p384};
use td_primitives::hmac::Hmac;
use td_primitives::sha::{Digest, Sha256, Sha384, Sha512};
use td_primitives::{chacha20_poly1305, ct, random, rsa, Refused};

/// ChaCha20-Poly1305 leads: without AES instructions it is the faster and has
/// no bitsliced key schedule to hold.
pub fn provider() -> CryptoProvider {
    CryptoProvider {
        cipher_suites: vec![
            TLS13_CHACHA20_POLY1305_SHA256,
            TLS13_AES_128_GCM_SHA256,
            TLS13_AES_256_GCM_SHA384,
            TLS12_ECDSA_CHACHA20,
            TLS12_RSA_CHACHA20,
            TLS12_ECDSA_AES128,
            TLS12_RSA_AES128,
            TLS12_ECDSA_AES256,
            TLS12_RSA_AES256,
        ],
        kx_groups: vec![&X25519, &SECP256R1, &SECP384R1],
        signature_verification_algorithms: SIGNATURE_ALGORITHMS,
        secure_random: &SystemEntropy,
        key_provider: &P256Keys,
    }
}

/// The provider, shared: a process builds configurations from one.
pub fn shared() -> Arc<CryptoProvider> {
    Arc::new(provider())
}

// ---- hashes and HMAC ---------------------------------------------------------

trait Named: Digest + Send + Sync + 'static {
    const ALGORITHM: HashAlgorithm;
}
impl Named for Sha256 {
    const ALGORITHM: HashAlgorithm = HashAlgorithm::SHA256;
}
impl Named for Sha384 {
    const ALGORITHM: HashAlgorithm = HashAlgorithm::SHA384;
}

struct HashOf<D>(PhantomData<fn() -> D>);

impl<D: Named> hash::Hash for HashOf<D> {
    fn start(&self) -> Box<dyn hash::Context> {
        Box::new(Running(D::new()))
    }
    fn hash(&self, data: &[u8]) -> hash::Output {
        hash::Output::new(D::digest(data).as_ref())
    }
    fn output_len(&self) -> usize {
        D::OUTPUT_LEN
    }
    fn algorithm(&self) -> HashAlgorithm {
        D::ALGORITHM
    }
}

struct Running<D>(D);

impl<D: Named> hash::Context for Running<D> {
    fn fork_finish(&self) -> hash::Output {
        hash::Output::new(self.0.clone().finalize().as_ref())
    }
    fn fork(&self) -> Box<dyn hash::Context> {
        Box::new(Running(self.0.clone()))
    }
    fn finish(self: Box<Self>) -> hash::Output {
        hash::Output::new(self.0.finalize().as_ref())
    }
    fn update(&mut self, data: &[u8]) {
        self.0.update(data);
    }
}

struct HmacOf<D>(PhantomData<fn() -> D>);

impl<D: Named> hmac::Hmac for HmacOf<D> {
    fn with_key(&self, key: &[u8]) -> Box<dyn hmac::Key> {
        Box::new(HmacKey(Hmac::<D>::new(key)))
    }
    fn hash_output_len(&self) -> usize {
        D::OUTPUT_LEN
    }
}

struct HmacKey<D: Digest>(Hmac<D>);

impl<D: Named> hmac::Key for HmacKey<D> {
    fn sign_concat(&self, first: &[u8], middle: &[&[u8]], last: &[u8]) -> hmac::Tag {
        let mut mac = self.0.clone();
        mac.update(first);
        for part in middle {
            mac.update(part);
        }
        mac.update(last);
        hmac::Tag::new(mac.finalize().as_ref())
    }
    fn tag_len(&self) -> usize {
        D::OUTPUT_LEN
    }
}

static HASH_SHA256: HashOf<Sha256> = HashOf(PhantomData);
static HASH_SHA384: HashOf<Sha384> = HashOf(PhantomData);
static HMAC_SHA256: HmacOf<Sha256> = HmacOf(PhantomData);
static HMAC_SHA384: HmacOf<Sha384> = HmacOf(PhantomData);

// ---- record protection -------------------------------------------------------

const TAG_LEN: usize = 16;
/// RFC 8446 5.1 and RFC 5246 6.2.3: at most this much plaintext in one record.
const MAX_FRAGMENT_LEN: usize = 16384;
const GCM_EXPLICIT_NONCE_LEN: usize = 8;
const GCM_SALT_LEN: usize = 4;
const NONCE_LEN: usize = 12;

#[derive(Clone, Copy)]
enum Algorithm {
    Aes128Gcm,
    Aes256Gcm,
    ChaCha20Poly1305,
}

impl Algorithm {
    fn key_len(self) -> usize {
        match self {
            Self::Aes128Gcm => 16,
            Self::Aes256Gcm | Self::ChaCha20Poly1305 => 32,
        }
    }

    fn key(self, key: &AeadKey) -> RecordKey {
        match self {
            Self::Aes128Gcm => key
                .as_ref()
                .try_into()
                .map_or(RecordKey::None, |key| RecordKey::Aes(AesGcm::new_128(key))),
            Self::Aes256Gcm => key
                .as_ref()
                .try_into()
                .map_or(RecordKey::None, |key| RecordKey::Aes(AesGcm::new_256(key))),
            Self::ChaCha20Poly1305 => key
                .as_ref()
                .try_into()
                .map_or(RecordKey::None, |key: &[u8; 32]| RecordKey::ChaCha(*key)),
        }
    }
}

enum RecordKey {
    Aes(AesGcm),
    ChaCha([u8; 32]),
    /// rustls's key schedule fixes the length, so this is never built today.
    /// Were it reached, it protects nothing rather than protecting badly.
    None,
}

impl RecordKey {
    fn seal(
        &self,
        nonce: &[u8; NONCE_LEN],
        aad: &[u8],
        data: &mut [u8],
    ) -> Result<[u8; TAG_LEN], Error> {
        match self {
            Self::Aes(key) => key.seal(nonce, aad, data),
            Self::ChaCha(key) => chacha20_poly1305::seal(key, nonce, aad, data),
            Self::None => Err(Refused),
        }
        .map_err(|_| Error::EncryptError)
    }

    /// Open `sealed`, the ciphertext then its tag; leaves the plaintext at its
    /// front and returns its length.
    fn open(&self, nonce: &[u8; NONCE_LEN], aad: &[u8], sealed: &mut [u8]) -> Result<usize, Error> {
        let plain_len = sealed
            .len()
            .checked_sub(TAG_LEN)
            .ok_or(Error::DecryptError)?;
        let (data, tag) = sealed.split_at_mut(plain_len);
        let tag: &[u8; TAG_LEN] = (&*tag).try_into().map_err(|_| Error::DecryptError)?;
        match self {
            Self::Aes(key) => key.open(nonce, aad, data, tag),
            Self::ChaCha(key) => chacha20_poly1305::open(key, nonce, aad, data, tag),
            Self::None => Err(Refused),
        }
        .map_err(|_| Error::DecryptError)?;
        Ok(plain_len)
    }
}

impl Drop for RecordKey {
    fn drop(&mut self) {
        if let Self::ChaCha(key) = self {
            ct::zero(key);
        }
    }
}

struct Tls13Aead(Algorithm);

impl Tls13AeadAlgorithm for Tls13Aead {
    fn encrypter(&self, key: AeadKey, iv: Iv) -> Box<dyn MessageEncrypter> {
        Box::new(Tls13Records {
            key: self.0.key(&key),
            iv,
        })
    }
    fn decrypter(&self, key: AeadKey, iv: Iv) -> Box<dyn MessageDecrypter> {
        Box::new(Tls13Records {
            key: self.0.key(&key),
            iv,
        })
    }
    fn key_len(&self) -> usize {
        self.0.key_len()
    }
    /// td has no kernel TLS or other reader of traffic secrets.
    fn extract_keys(
        &self,
        _: AeadKey,
        _: Iv,
    ) -> Result<ConnectionTrafficSecrets, UnsupportedOperationError> {
        Err(UnsupportedOperationError)
    }
}

struct Tls13Records {
    key: RecordKey,
    iv: Iv,
}

impl MessageEncrypter for Tls13Records {
    fn encrypt(
        &mut self,
        msg: OutboundPlainMessage<'_>,
        seq: u64,
    ) -> Result<OutboundOpaqueMessage, Error> {
        let total_len = self.encrypted_payload_len(msg.payload.len());
        let mut payload = PrefixedPayload::with_capacity(total_len);
        payload.extend_from_chunks(&msg.payload);
        payload.extend_from_slice(&msg.typ.to_array());
        let tag = self.key.seal(
            &Nonce::new(&self.iv, seq).0,
            &make_tls13_aad(total_len),
            payload.as_mut(),
        )?;
        payload.extend_from_slice(&tag);
        Ok(OutboundOpaqueMessage::new(
            ContentType::ApplicationData,
            ProtocolVersion::TLSv1_2,
            payload,
        ))
    }
    fn encrypted_payload_len(&self, payload_len: usize) -> usize {
        payload_len + 1 + TAG_LEN
    }
}

impl MessageDecrypter for Tls13Records {
    fn decrypt<'a>(
        &mut self,
        mut msg: InboundOpaqueMessage<'a>,
        seq: u64,
    ) -> Result<InboundPlainMessage<'a>, Error> {
        let aad = make_tls13_aad(msg.payload.len());
        let plain_len = self
            .key
            .open(&Nonce::new(&self.iv, seq).0, &aad, &mut msg.payload)?;
        msg.payload.truncate(plain_len);
        msg.into_tls13_unpadded_message()
    }
}

struct Tls12Gcm(Algorithm);

impl Tls12AeadAlgorithm for Tls12Gcm {
    fn encrypter(&self, key: AeadKey, iv: &[u8], explicit: &[u8]) -> Box<dyn MessageEncrypter> {
        let (key, iv) = match joined_iv(iv, explicit) {
            Some(iv) => (self.0.key(&key), iv),
            None => (RecordKey::None, [0; NONCE_LEN]),
        };
        Box::new(Tls12GcmSeal { key, iv })
    }
    fn decrypter(&self, key: AeadKey, salt: &[u8]) -> Box<dyn MessageDecrypter> {
        let salt = <[u8; GCM_SALT_LEN]>::try_from(salt);
        Box::new(Tls12GcmOpen {
            key: if salt.is_ok() {
                self.0.key(&key)
            } else {
                RecordKey::None
            },
            salt: salt.unwrap_or_default(),
        })
    }
    fn key_block_shape(&self) -> KeyBlockShape {
        KeyBlockShape {
            enc_key_len: self.0.key_len(),
            fixed_iv_len: GCM_SALT_LEN,
            explicit_nonce_len: GCM_EXPLICIT_NONCE_LEN,
        }
    }
    fn extract_keys(
        &self,
        _: AeadKey,
        _: &[u8],
        _: &[u8],
    ) -> Result<ConnectionTrafficSecrets, UnsupportedOperationError> {
        Err(UnsupportedOperationError)
    }
}

/// The GCM salt, then rustls's explicit starting point: XORing the sequence
/// number into it gives each record's nonce, as TLS 1.3 does.
fn joined_iv(salt: &[u8], explicit: &[u8]) -> Option<[u8; NONCE_LEN]> {
    let salt = <[u8; GCM_SALT_LEN]>::try_from(salt).ok()?;
    let explicit = <[u8; GCM_EXPLICIT_NONCE_LEN]>::try_from(explicit).ok()?;
    let [a, b, c, d] = salt;
    let [e, f, g, h, i, j, k, l] = explicit;
    Some([a, b, c, d, e, f, g, h, i, j, k, l])
}

struct Tls12GcmSeal {
    key: RecordKey,
    iv: [u8; NONCE_LEN],
}

impl MessageEncrypter for Tls12GcmSeal {
    fn encrypt(
        &mut self,
        msg: OutboundPlainMessage<'_>,
        seq: u64,
    ) -> Result<OutboundOpaqueMessage, Error> {
        let nonce = Nonce::new(&Iv::new(self.iv), seq).0;
        let total_len = self.encrypted_payload_len(msg.payload.len());
        let mut payload = PrefixedPayload::with_capacity(total_len);
        payload.extend_from_slice(nonce.split_at(GCM_SALT_LEN).1);
        payload.extend_from_chunks(&msg.payload);
        let aad = make_tls12_aad(seq, msg.typ, msg.version, msg.payload.len());
        let tag = self.key.seal(
            &nonce,
            &aad,
            payload.as_mut().split_at_mut(GCM_EXPLICIT_NONCE_LEN).1,
        )?;
        payload.extend_from_slice(&tag);
        Ok(OutboundOpaqueMessage::new(msg.typ, msg.version, payload))
    }
    fn encrypted_payload_len(&self, payload_len: usize) -> usize {
        payload_len + GCM_EXPLICIT_NONCE_LEN + TAG_LEN
    }
}

struct Tls12GcmOpen {
    key: RecordKey,
    salt: [u8; GCM_SALT_LEN],
}

impl MessageDecrypter for Tls12GcmOpen {
    fn decrypt<'a>(
        &mut self,
        mut msg: InboundOpaqueMessage<'a>,
        seq: u64,
    ) -> Result<InboundPlainMessage<'a>, Error> {
        let plain_len = msg
            .payload
            .len()
            .checked_sub(GCM_EXPLICIT_NONCE_LEN + TAG_LEN)
            .ok_or(Error::DecryptError)?;
        let aad = make_tls12_aad(seq, msg.typ, msg.version, plain_len);
        let (explicit, sealed) = msg.payload.split_at_mut(GCM_EXPLICIT_NONCE_LEN);
        let nonce = joined_iv(&self.salt, explicit).ok_or(Error::DecryptError)?;
        let opened = self.key.open(&nonce, &aad, sealed)?;
        if opened > MAX_FRAGMENT_LEN {
            return Err(Error::PeerSentOversizedRecord);
        }
        msg.payload
            .copy_within(GCM_EXPLICIT_NONCE_LEN..GCM_EXPLICIT_NONCE_LEN + opened, 0);
        msg.payload.truncate(opened);
        Ok(msg.into_plain_message())
    }
}

struct Tls12ChaCha;

impl Tls12AeadAlgorithm for Tls12ChaCha {
    fn encrypter(&self, key: AeadKey, iv: &[u8], _: &[u8]) -> Box<dyn MessageEncrypter> {
        Box::new(Tls12ChaChaRecords::new(key, iv))
    }
    fn decrypter(&self, key: AeadKey, iv: &[u8]) -> Box<dyn MessageDecrypter> {
        Box::new(Tls12ChaChaRecords::new(key, iv))
    }
    fn key_block_shape(&self) -> KeyBlockShape {
        KeyBlockShape {
            enc_key_len: 32,
            fixed_iv_len: NONCE_LEN,
            explicit_nonce_len: 0,
        }
    }
    fn extract_keys(
        &self,
        _: AeadKey,
        _: &[u8],
        _: &[u8],
    ) -> Result<ConnectionTrafficSecrets, UnsupportedOperationError> {
        Err(UnsupportedOperationError)
    }
}

struct Tls12ChaChaRecords {
    key: RecordKey,
    iv: [u8; NONCE_LEN],
}

impl Tls12ChaChaRecords {
    fn new(key: AeadKey, iv: &[u8]) -> Self {
        match <[u8; NONCE_LEN]>::try_from(iv) {
            Ok(iv) => Self {
                key: Algorithm::ChaCha20Poly1305.key(&key),
                iv,
            },
            Err(_) => Self {
                key: RecordKey::None,
                iv: [0; NONCE_LEN],
            },
        }
    }
}

impl MessageEncrypter for Tls12ChaChaRecords {
    fn encrypt(
        &mut self,
        msg: OutboundPlainMessage<'_>,
        seq: u64,
    ) -> Result<OutboundOpaqueMessage, Error> {
        let total_len = self.encrypted_payload_len(msg.payload.len());
        let mut payload = PrefixedPayload::with_capacity(total_len);
        payload.extend_from_chunks(&msg.payload);
        let aad = make_tls12_aad(seq, msg.typ, msg.version, msg.payload.len());
        let nonce = Nonce::new(&Iv::new(self.iv), seq).0;
        let tag = self.key.seal(&nonce, &aad, payload.as_mut())?;
        payload.extend_from_slice(&tag);
        Ok(OutboundOpaqueMessage::new(msg.typ, msg.version, payload))
    }
    fn encrypted_payload_len(&self, payload_len: usize) -> usize {
        payload_len + TAG_LEN
    }
}

impl MessageDecrypter for Tls12ChaChaRecords {
    fn decrypt<'a>(
        &mut self,
        mut msg: InboundOpaqueMessage<'a>,
        seq: u64,
    ) -> Result<InboundPlainMessage<'a>, Error> {
        let plain_len = msg
            .payload
            .len()
            .checked_sub(TAG_LEN)
            .ok_or(Error::DecryptError)?;
        let aad = make_tls12_aad(seq, msg.typ, msg.version, plain_len);
        let nonce = Nonce::new(&Iv::new(self.iv), seq).0;
        let opened = self.key.open(&nonce, &aad, &mut msg.payload)?;
        if opened > MAX_FRAGMENT_LEN {
            return Err(Error::PeerSentOversizedRecord);
        }
        msg.payload.truncate(opened);
        Ok(msg.into_plain_message())
    }
}

// ---- cipher suites -----------------------------------------------------------

static TLS13_CHACHA20_POLY1305_SHA256: SupportedCipherSuite =
    SupportedCipherSuite::Tls13(&Tls13CipherSuite {
        common: CipherSuiteCommon {
            suite: CipherSuite::TLS13_CHACHA20_POLY1305_SHA256,
            hash_provider: &HASH_SHA256,
            confidentiality_limit: u64::MAX,
        },
        hkdf_provider: &HkdfUsingHmac(&HMAC_SHA256),
        aead_alg: &Tls13Aead(Algorithm::ChaCha20Poly1305),
        quic: None,
    });

static TLS13_AES_128_GCM_SHA256: SupportedCipherSuite =
    SupportedCipherSuite::Tls13(&Tls13CipherSuite {
        common: CipherSuiteCommon {
            suite: CipherSuite::TLS13_AES_128_GCM_SHA256,
            hash_provider: &HASH_SHA256,
            confidentiality_limit: 1 << 24,
        },
        hkdf_provider: &HkdfUsingHmac(&HMAC_SHA256),
        aead_alg: &Tls13Aead(Algorithm::Aes128Gcm),
        quic: None,
    });

static TLS13_AES_256_GCM_SHA384: SupportedCipherSuite =
    SupportedCipherSuite::Tls13(&Tls13CipherSuite {
        common: CipherSuiteCommon {
            suite: CipherSuite::TLS13_AES_256_GCM_SHA384,
            hash_provider: &HASH_SHA384,
            confidentiality_limit: 1 << 24,
        },
        hkdf_provider: &HkdfUsingHmac(&HMAC_SHA384),
        aead_alg: &Tls13Aead(Algorithm::Aes256Gcm),
        quic: None,
    });

const TLS12_ECDSA_SCHEMES: &[SignatureScheme] = &[
    SignatureScheme::ED25519,
    SignatureScheme::ECDSA_NISTP384_SHA384,
    SignatureScheme::ECDSA_NISTP256_SHA256,
];

const TLS12_RSA_SCHEMES: &[SignatureScheme] = &[
    SignatureScheme::RSA_PSS_SHA512,
    SignatureScheme::RSA_PSS_SHA384,
    SignatureScheme::RSA_PSS_SHA256,
    SignatureScheme::RSA_PKCS1_SHA512,
    SignatureScheme::RSA_PKCS1_SHA384,
    SignatureScheme::RSA_PKCS1_SHA256,
];

macro_rules! tls12_suite {
    ($vis:vis $name:ident, $suite:ident, $hash:ident, $hmac:ident, $limit:expr, $schemes:ident, $aead:expr) => {
        $vis static $name: SupportedCipherSuite = SupportedCipherSuite::Tls12(&Tls12CipherSuite {
            common: CipherSuiteCommon {
                suite: CipherSuite::$suite,
                hash_provider: &$hash,
                confidentiality_limit: $limit,
            },
            kx: KeyExchangeAlgorithm::ECDHE,
            sign: $schemes,
            aead_alg: &$aead,
            prf_provider: &PrfUsingHmac(&$hmac),
        });
    };
}

tls12_suite!(
    TLS12_ECDSA_CHACHA20,
    TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256,
    HASH_SHA256,
    HMAC_SHA256,
    u64::MAX,
    TLS12_ECDSA_SCHEMES,
    Tls12ChaCha
);
tls12_suite!(
    TLS12_RSA_CHACHA20,
    TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256,
    HASH_SHA256,
    HMAC_SHA256,
    u64::MAX,
    TLS12_RSA_SCHEMES,
    Tls12ChaCha
);
tls12_suite!(
    TLS12_ECDSA_AES128,
    TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256,
    HASH_SHA256,
    HMAC_SHA256,
    1 << 24,
    TLS12_ECDSA_SCHEMES,
    Tls12Gcm(Algorithm::Aes128Gcm)
);
tls12_suite!(
    TLS12_RSA_AES128,
    TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256,
    HASH_SHA256,
    HMAC_SHA256,
    1 << 24,
    TLS12_RSA_SCHEMES,
    Tls12Gcm(Algorithm::Aes128Gcm)
);
tls12_suite!(
    TLS12_ECDSA_AES256,
    TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384,
    HASH_SHA384,
    HMAC_SHA384,
    1 << 24,
    TLS12_ECDSA_SCHEMES,
    Tls12Gcm(Algorithm::Aes256Gcm)
);
tls12_suite!(
    TLS12_RSA_AES256,
    TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384,
    HASH_SHA384,
    HMAC_SHA384,
    1 << 24,
    TLS12_RSA_SCHEMES,
    Tls12Gcm(Algorithm::Aes256Gcm)
);

// ---- key exchange ------------------------------------------------------------

#[derive(Debug)]
struct X25519Group;
static X25519: X25519Group = X25519Group;

impl SupportedKxGroup for X25519Group {
    fn start(&self) -> Result<Box<dyn ActiveKeyExchange>, Error> {
        let secret = x25519::generate_secret().map_err(|_| GetRandomFailed)?;
        Ok(Box::new(X25519Exchange {
            public: x25519::public_key(&secret),
            secret,
        }))
    }
    fn name(&self) -> NamedGroup {
        NamedGroup::X25519
    }
}

struct X25519Exchange {
    secret: [u8; x25519::KEY_LEN],
    public: [u8; x25519::KEY_LEN],
}

impl ActiveKeyExchange for X25519Exchange {
    fn complete(self: Box<Self>, peer: &[u8]) -> Result<SharedSecret, Error> {
        let peer: &[u8; x25519::KEY_LEN] = peer
            .try_into()
            .map_err(|_| PeerMisbehaved::InvalidKeyShare)?;
        let mut shared =
            x25519::agree(&self.secret, peer).map_err(|_| PeerMisbehaved::InvalidKeyShare)?;
        let secret = SharedSecret::from(shared.as_slice());
        ct::zero(&mut shared);
        Ok(secret)
    }
    fn pub_key(&self) -> &[u8] {
        &self.public
    }
    fn group(&self) -> NamedGroup {
        NamedGroup::X25519
    }
}

impl Drop for X25519Exchange {
    fn drop(&mut self) {
        ct::zero(&mut self.secret);
    }
}

macro_rules! nist_group {
    ($static:ident, $group:ident, $exchange:ident, $curve:ident, $name:ident) => {
        #[derive(Debug)]
        struct $group;
        static $static: $group = $group;

        impl SupportedKxGroup for $group {
            fn start(&self) -> Result<Box<dyn ActiveKeyExchange>, Error> {
                let secret = $curve::SecretKey::generate().map_err(|_| GetRandomFailed)?;
                let public = secret.public_key().map_err(|_| GetRandomFailed)?;
                Ok(Box::new($exchange { secret, public }))
            }
            fn name(&self) -> NamedGroup {
                NamedGroup::$name
            }
        }

        struct $exchange {
            secret: $curve::SecretKey,
            public: [u8; $curve::PUBLIC_LEN],
        }

        impl ActiveKeyExchange for $exchange {
            fn complete(self: Box<Self>, peer: &[u8]) -> Result<SharedSecret, Error> {
                let mut shared = self
                    .secret
                    .agree(peer)
                    .map_err(|_| PeerMisbehaved::InvalidKeyShare)?;
                let secret = SharedSecret::from(shared.as_slice());
                ct::zero(&mut shared);
                Ok(secret)
            }
            fn pub_key(&self) -> &[u8] {
                &self.public
            }
            fn group(&self) -> NamedGroup {
                NamedGroup::$name
            }
        }
    };
}

nist_group!(
    SECP256R1,
    Secp256r1Group,
    Secp256r1Exchange,
    p256,
    secp256r1
);
// Offered last, and needed even when never chosen: a TLS 1.2 server holding a
// P-384 certificate refuses a client that does not list this group.
nist_group!(
    SECP384R1,
    Secp384r1Group,
    Secp384r1Exchange,
    p384,
    secp384r1
);

// ---- signature verification --------------------------------------------------

/// webpki hands each algorithm its key as the SPKI's BIT STRING content: an EC
/// point, a raw Ed25519 key, or a DER RSAPublicKey.
type Check = fn(&[u8], &[u8], &[u8]) -> Result<(), Refused>;

struct Verifier {
    name: &'static str,
    public_key: AlgorithmIdentifier,
    signature: AlgorithmIdentifier,
    check: Check,
}

impl std::fmt::Debug for Verifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name)
    }
}

impl SignatureVerificationAlgorithm for Verifier {
    fn verify_signature(
        &self,
        public_key: &[u8],
        message: &[u8],
        signature: &[u8],
    ) -> Result<(), InvalidSignature> {
        (self.check)(public_key, message, signature).map_err(|_| InvalidSignature)
    }
    fn public_key_alg_id(&self) -> AlgorithmIdentifier {
        self.public_key
    }
    fn signature_alg_id(&self) -> AlgorithmIdentifier {
        self.signature
    }
}

fn pkcs1<D: rsa::RsaHash>(public: &[u8], message: &[u8], signature: &[u8]) -> Result<(), Refused> {
    rsa::PublicKey::from_der(public)?.verify_pkcs1::<D>(message, signature)
}

fn pss<D: Digest>(public: &[u8], message: &[u8], signature: &[u8]) -> Result<(), Refused> {
    rsa::PublicKey::from_der(public)?.verify_pss::<D>(message, signature)
}

fn ed25519(public: &[u8], message: &[u8], signature: &[u8]) -> Result<(), Refused> {
    use td_primitives::curve25519::ed25519::{verify, PUBLIC_LEN, SIGNATURE_LEN};
    let public: &[u8; PUBLIC_LEN] = public.try_into().map_err(|_| Refused)?;
    let signature: &[u8; SIGNATURE_LEN] = signature.try_into().map_err(|_| Refused)?;
    verify(public, message, signature)
}

macro_rules! ecdsa {
    ($name:ident, $curve:ident, $hash:ident, $public:ident, $signature:ident) => {
        static $name: Verifier = Verifier {
            name: stringify!($name),
            public_key: alg_id::$public,
            signature: alg_id::$signature,
            check: |public, message, signature| {
                $curve::verify(public, $hash::digest(message).as_ref(), signature)
            },
        };
    };
}

ecdsa!(ECDSA_P256_SHA256, p256, Sha256, ECDSA_P256, ECDSA_SHA256);
ecdsa!(ECDSA_P256_SHA384, p256, Sha384, ECDSA_P256, ECDSA_SHA384);
ecdsa!(ECDSA_P384_SHA256, p384, Sha256, ECDSA_P384, ECDSA_SHA256);
ecdsa!(ECDSA_P384_SHA384, p384, Sha384, ECDSA_P384, ECDSA_SHA384);

macro_rules! rsa_verifier {
    ($name:ident, $signature:expr, $check:expr) => {
        static $name: Verifier = Verifier {
            name: stringify!($name),
            public_key: alg_id::RSA_ENCRYPTION,
            signature: $signature,
            check: $check,
        };
    };
}

// sha256WithRSAEncryption and its siblings without the NULL RFC 5754 calls for,
// which webpki admits for certificates in the wild. The check is unchanged:
// these name the same algorithm.
const PKCS1_SHA256_ABSENT_PARAMS: AlgorithmIdentifier = AlgorithmIdentifier::from_slice(&[
    0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0b,
]);
const PKCS1_SHA384_ABSENT_PARAMS: AlgorithmIdentifier = AlgorithmIdentifier::from_slice(&[
    0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0c,
]);
const PKCS1_SHA512_ABSENT_PARAMS: AlgorithmIdentifier = AlgorithmIdentifier::from_slice(&[
    0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0d,
]);

rsa_verifier!(RSA_PKCS1_SHA256, alg_id::RSA_PKCS1_SHA256, pkcs1::<Sha256>);
rsa_verifier!(RSA_PKCS1_SHA384, alg_id::RSA_PKCS1_SHA384, pkcs1::<Sha384>);
rsa_verifier!(RSA_PKCS1_SHA512, alg_id::RSA_PKCS1_SHA512, pkcs1::<Sha512>);
rsa_verifier!(
    RSA_PKCS1_SHA256_BARE,
    PKCS1_SHA256_ABSENT_PARAMS,
    pkcs1::<Sha256>
);
rsa_verifier!(
    RSA_PKCS1_SHA384_BARE,
    PKCS1_SHA384_ABSENT_PARAMS,
    pkcs1::<Sha384>
);
rsa_verifier!(
    RSA_PKCS1_SHA512_BARE,
    PKCS1_SHA512_ABSENT_PARAMS,
    pkcs1::<Sha512>
);
rsa_verifier!(RSA_PSS_SHA256, alg_id::RSA_PSS_SHA256, pss::<Sha256>);
rsa_verifier!(RSA_PSS_SHA384, alg_id::RSA_PSS_SHA384, pss::<Sha384>);
rsa_verifier!(RSA_PSS_SHA512, alg_id::RSA_PSS_SHA512, pss::<Sha512>);

static ED25519: Verifier = Verifier {
    name: "ED25519",
    public_key: alg_id::ED25519,
    signature: alg_id::ED25519,
    check: ed25519,
};

static SIGNATURE_ALGORITHMS: WebPkiSupportedAlgorithms = WebPkiSupportedAlgorithms {
    all: &[
        &ECDSA_P256_SHA256,
        &ECDSA_P256_SHA384,
        &ECDSA_P384_SHA256,
        &ECDSA_P384_SHA384,
        &ED25519,
        &RSA_PSS_SHA256,
        &RSA_PSS_SHA384,
        &RSA_PSS_SHA512,
        &RSA_PKCS1_SHA256,
        &RSA_PKCS1_SHA384,
        &RSA_PKCS1_SHA512,
        &RSA_PKCS1_SHA256_BARE,
        &RSA_PKCS1_SHA384_BARE,
        &RSA_PKCS1_SHA512_BARE,
    ],
    // TLS 1.2 does not tie a scheme to a curve; TLS 1.3 does, and rustls holds
    // that itself by taking only the first algorithm listed.
    mapping: &[
        (
            SignatureScheme::ECDSA_NISTP384_SHA384,
            &[&ECDSA_P384_SHA384, &ECDSA_P256_SHA384],
        ),
        (
            SignatureScheme::ECDSA_NISTP256_SHA256,
            &[&ECDSA_P256_SHA256, &ECDSA_P384_SHA256],
        ),
        (SignatureScheme::ED25519, &[&ED25519]),
        (SignatureScheme::RSA_PSS_SHA512, &[&RSA_PSS_SHA512]),
        (SignatureScheme::RSA_PSS_SHA384, &[&RSA_PSS_SHA384]),
        (SignatureScheme::RSA_PSS_SHA256, &[&RSA_PSS_SHA256]),
        (SignatureScheme::RSA_PKCS1_SHA512, &[&RSA_PKCS1_SHA512]),
        (SignatureScheme::RSA_PKCS1_SHA384, &[&RSA_PKCS1_SHA384]),
        (SignatureScheme::RSA_PKCS1_SHA256, &[&RSA_PKCS1_SHA256]),
    ],
};

// ---- entropy and the one signing key td serves with ---------------------------

#[derive(Debug)]
struct SystemEntropy;

impl SecureRandom for SystemEntropy {
    fn fill(&self, buf: &mut [u8]) -> Result<(), GetRandomFailed> {
        random::fill(buf).map_err(|_| GetRandomFailed)
    }
}

/// Only a P-256 PKCS#8 key, in the shape `p256_from_pkcs8` reads, loads, and
/// it signs only ECDSA-P256-SHA256.
#[derive(Debug)]
struct P256Keys;

impl KeyProvider for P256Keys {
    fn load_private_key(&self, key: PrivateKeyDer<'static>) -> Result<Arc<dyn SigningKey>, Error> {
        let PrivateKeyDer::Pkcs8(pkcs8) = &key else {
            return Err(Error::General("only a PKCS#8 key loads".into()));
        };
        let secret = p256_from_pkcs8(pkcs8.secret_pkcs8_der())
            .ok_or_else(|| Error::General("the key is not a P-256 PKCS#8".into()))?;
        let public = secret
            .public_key()
            .map_err(|_| Error::General("the key has no public half".into()))?;
        Ok(Arc::new(P256Identity {
            key: Arc::new(secret),
            public,
        }))
    }
}

const P256_PKCS8_HEAD: &[u8] = &[
    0x30, 0x81, 0x87, 0x02, 0x01, 0x00, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02,
    0x01, 0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x04, 0x6d, 0x30, 0x6b, 0x02,
    0x01, 0x01, 0x04, 0x20,
];
const P256_PKCS8_MIDDLE: &[u8] = &[0xa1, 0x44, 0x03, 0x42, 0x00];

/// RFC 5958 around RFC 5915, the one shape ring and OpenSSL both write for this
/// curve. The public half must be the secret's, so a damaged file cannot serve.
pub(crate) fn p256_from_pkcs8(pkcs8: &[u8]) -> Option<p256::SecretKey> {
    let rest = pkcs8.strip_prefix(P256_PKCS8_HEAD)?;
    let (secret, rest) = rest.split_at_checked(p256::SECRET_LEN)?;
    let public = rest.strip_prefix(P256_PKCS8_MIDDLE)?;
    let key = p256::SecretKey::from_bytes(secret.try_into().ok()?).ok()?;
    (key.public_key().ok()?.as_slice() == public).then_some(key)
}

/// The PKCS#8 `p256_from_pkcs8` reads.
pub fn p256_to_pkcs8(secret: &[u8; p256::SECRET_LEN], public: &[u8]) -> Vec<u8> {
    let mut der = Vec::with_capacity(P256_PKCS8_HEAD.len() + 32 + 5 + public.len());
    der.extend_from_slice(P256_PKCS8_HEAD);
    der.extend_from_slice(secret);
    der.extend_from_slice(P256_PKCS8_MIDDLE);
    der.extend_from_slice(public);
    der
}

struct P256Identity {
    key: Arc<p256::SecretKey>,
    public: [u8; p256::PUBLIC_LEN],
}

// By hand: rustls wants these to be `Debug`, and the secret must never print.
impl std::fmt::Debug for P256Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("P256Identity")
    }
}

impl SigningKey for P256Identity {
    fn choose_scheme(&self, offered: &[SignatureScheme]) -> Option<Box<dyn Signer>> {
        offered
            .contains(&SignatureScheme::ECDSA_NISTP256_SHA256)
            .then(|| Box::new(P256Signer(Arc::clone(&self.key))) as Box<dyn Signer>)
    }
    /// So rustls refuses a certificate this key does not match when it loads
    /// them, not each client when it connects.
    fn public_key(&self) -> Option<SubjectPublicKeyInfoDer<'_>> {
        Some(public_key_to_spki(&alg_id::ECDSA_P256, self.public))
    }
    fn algorithm(&self) -> SignatureAlgorithm {
        SignatureAlgorithm::ECDSA
    }
}

struct P256Signer(Arc<p256::SecretKey>);

impl std::fmt::Debug for P256Signer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("P256Signer")
    }
}

impl Signer for P256Signer {
    fn sign(&self, message: &[u8]) -> Result<Vec<u8>, Error> {
        self.0
            .sign_sha256(message)
            .map_err(|_| Error::General("ECDSA signing failed".into()))
    }
    fn scheme(&self) -> SignatureScheme {
        SignatureScheme::ECDSA_NISTP256_SHA256
    }
}

/// Verifiers a consumer assembles its own `WebPkiSupportedAlgorithms` from, in
/// its own order and mapping, as rustls's `webpki_algs` are. td-crypto's policy
/// takes these; `provider()` keeps its own, and is unchanged by them.
pub mod verifiers {
    use super::{alg_id, Digest, Sha256, Sha384, Sha512, SignatureVerificationAlgorithm, Verifier};
    use td_primitives::ec::{p256, p384, p521};

    /// ECDSA whose key may also be in SEC 1's compressed form, as a certificate
    /// may hold it (td-crypto admits compressed anchor keys: its DESIGN.md,
    /// "Trust-store admission"); `uncompressed` validates either and refuses
    /// the hybrid form and infinity. These verify signatures only: a TLS key
    /// share is never compressed. `provider()`'s ECDSA takes only uncompressed.
    macro_rules! ecdsa_any_point {
        ($name:ident, $curve:ident, $hash:ident, $public:ident, $signature:ident) => {
            pub static $name: &dyn SignatureVerificationAlgorithm = &Verifier {
                name: stringify!($name),
                public_key: alg_id::$public,
                signature: alg_id::$signature,
                check: |public, message, signature| {
                    let point = $curve::uncompressed(public)?;
                    $curve::verify(&point, $hash::digest(message).as_ref(), signature)
                },
            };
        };
    }

    ecdsa_any_point!(ECDSA_P256_SHA256, p256, Sha256, ECDSA_P256, ECDSA_SHA256);
    ecdsa_any_point!(ECDSA_P256_SHA384, p256, Sha384, ECDSA_P256, ECDSA_SHA384);
    ecdsa_any_point!(ECDSA_P256_SHA512, p256, Sha512, ECDSA_P256, ECDSA_SHA512);
    ecdsa_any_point!(ECDSA_P384_SHA256, p384, Sha256, ECDSA_P384, ECDSA_SHA256);
    ecdsa_any_point!(ECDSA_P384_SHA384, p384, Sha384, ECDSA_P384, ECDSA_SHA384);
    ecdsa_any_point!(ECDSA_P384_SHA512, p384, Sha512, ECDSA_P384, ECDSA_SHA512);
    ecdsa_any_point!(ECDSA_P521_SHA256, p521, Sha256, ECDSA_P521, ECDSA_SHA256);
    ecdsa_any_point!(ECDSA_P521_SHA384, p521, Sha384, ECDSA_P521, ECDSA_SHA384);
    ecdsa_any_point!(ECDSA_P521_SHA512, p521, Sha512, ECDSA_P521, ECDSA_SHA512);

    /// `provider()`'s own Ed25519 and RSA verifiers, the same statics.
    pub static ED25519: &dyn SignatureVerificationAlgorithm = &super::ED25519;
    pub static RSA_PSS_SHA256: &dyn SignatureVerificationAlgorithm = &super::RSA_PSS_SHA256;
    pub static RSA_PSS_SHA384: &dyn SignatureVerificationAlgorithm = &super::RSA_PSS_SHA384;
    pub static RSA_PSS_SHA512: &dyn SignatureVerificationAlgorithm = &super::RSA_PSS_SHA512;
    pub static RSA_PKCS1_SHA256: &dyn SignatureVerificationAlgorithm = &super::RSA_PKCS1_SHA256;
    pub static RSA_PKCS1_SHA384: &dyn SignatureVerificationAlgorithm = &super::RSA_PKCS1_SHA384;
    pub static RSA_PKCS1_SHA512: &dyn SignatureVerificationAlgorithm = &super::RSA_PKCS1_SHA512;
    /// PKCS#1 named by its signature identifier without the NULL parameters.
    pub static RSA_PKCS1_SHA256_BARE: &dyn SignatureVerificationAlgorithm =
        &super::RSA_PKCS1_SHA256_BARE;
    pub static RSA_PKCS1_SHA384_BARE: &dyn SignatureVerificationAlgorithm =
        &super::RSA_PKCS1_SHA384_BARE;
    pub static RSA_PKCS1_SHA512_BARE: &dyn SignatureVerificationAlgorithm =
        &super::RSA_PKCS1_SHA512_BARE;
}

/// `provider()`'s TLS 1.2 ECDHE-ECDSA suites with rustls's own providers' full
/// server scheme list, which also names ECDSA_NISTP521_SHA512. The key provider
/// here holds only P-256, so the list changes no handshake it can serve.
pub mod tls12_ecdsa {
    use super::{
        Algorithm, CipherSuite, CipherSuiteCommon, KeyExchangeAlgorithm, PrfUsingHmac,
        SignatureScheme, SupportedCipherSuite, Tls12ChaCha, Tls12CipherSuite, Tls12Gcm,
        HASH_SHA256, HASH_SHA384, HMAC_SHA256, HMAC_SHA384,
    };

    const SCHEMES: &[SignatureScheme] = &[
        SignatureScheme::ED25519,
        SignatureScheme::ECDSA_NISTP521_SHA512,
        SignatureScheme::ECDSA_NISTP384_SHA384,
        SignatureScheme::ECDSA_NISTP256_SHA256,
    ];

    tls12_suite!(
        pub TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256,
        TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256,
        HASH_SHA256,
        HMAC_SHA256,
        u64::MAX,
        SCHEMES,
        Tls12ChaCha
    );
    tls12_suite!(
        pub TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256,
        TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256,
        HASH_SHA256,
        HMAC_SHA256,
        1 << 24,
        SCHEMES,
        Tls12Gcm(Algorithm::Aes128Gcm)
    );
    tls12_suite!(
        pub TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384,
        TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384,
        HASH_SHA384,
        HMAC_SHA384,
        1 << 24,
        SCHEMES,
        Tls12Gcm(Algorithm::Aes256Gcm)
    );
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic
    )]

    use super::*;
    use rustls::pki_types::{pem::PemObject, CertificateDer, ServerName};
    use rustls::{ClientConfig, ClientConnection, RootCertStore, ServerConfig, ServerConnection};
    use std::io::{Read, Write};

    const CA: &[u8] = include_bytes!("../tests/fixtures/account-ca/ca.pem");
    const CERT: &[u8] = include_bytes!("../tests/fixtures/account-ca/server.pem");
    const KEY: &[u8] = include_bytes!("../tests/fixtures/account-ca/server.key");

    fn to_hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn from_hex(text: &str) -> Result<Vec<u8>, std::num::ParseIntError> {
        (0..text.len())
            .step_by(2)
            .map(|at| u8::from_str_radix(&text[at..at + 2], 16))
            .collect()
    }

    fn narrowed(
        suite: SupportedCipherSuite,
        group: &'static dyn SupportedKxGroup,
    ) -> Arc<CryptoProvider> {
        Arc::new(CryptoProvider {
            cipher_suites: vec![suite],
            kx_groups: vec![group],
            ..provider()
        })
    }

    /// One whole in-memory connection, both ends on this provider, moving more
    /// than one record each way.
    fn connect(provider: Arc<CryptoProvider>) -> (ClientConnection, ServerConnection) {
        let chain: Vec<_> = CertificateDer::pem_slice_iter(CERT)
            .map(Result::unwrap)
            .collect();
        let server = ServerConfig::builder_with_provider(Arc::clone(&provider))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(chain, PrivateKeyDer::from_pem_slice(KEY).unwrap())
            .unwrap();
        let mut roots = RootCertStore::empty();
        for cert in CertificateDer::pem_slice_iter(CA) {
            roots.add(cert.unwrap()).unwrap();
        }
        let client = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        (
            ClientConnection::new(Arc::new(client), ServerName::try_from("localhost").unwrap())
                .unwrap(),
            ServerConnection::new(Arc::new(server)).unwrap(),
        )
    }

    fn shuttle(client: &mut ClientConnection, server: &mut ServerConnection) -> Result<(), Error> {
        let mut wire = Vec::new();
        while client.wants_write() {
            client.write_tls(&mut wire).unwrap();
        }
        let mut reader = wire.as_slice();
        while !reader.is_empty() {
            server.read_tls(&mut reader).unwrap();
            server.process_new_packets()?;
        }
        let mut wire = Vec::new();
        while server.wants_write() {
            server.write_tls(&mut wire).unwrap();
        }
        let mut reader = wire.as_slice();
        while !reader.is_empty() {
            client.read_tls(&mut reader).unwrap();
            client.process_new_packets()?;
        }
        Ok(())
    }

    fn handshake(client: &mut ClientConnection, server: &mut ServerConnection) {
        while client.is_handshaking() || server.is_handshaking() {
            shuttle(client, server).unwrap();
        }
    }

    #[test]
    fn every_suite_and_group_completes_and_carries_data_both_ways() {
        let groups: [&'static dyn SupportedKxGroup; 3] = [&X25519, &SECP256R1, &SECP384R1];
        // The fixture's identity is ECDSA, so these are the suites it can serve.
        let suites = [
            TLS13_CHACHA20_POLY1305_SHA256,
            TLS13_AES_128_GCM_SHA256,
            TLS13_AES_256_GCM_SHA384,
            TLS12_ECDSA_CHACHA20,
            TLS12_ECDSA_AES128,
            TLS12_ECDSA_AES256,
        ];
        let tested_long = vec![0x5a; 16_000];
        for suite in suites {
            for group in groups {
                let name = suite.suite();
                let (mut client, mut server) = connect(narrowed(suite, group));
                handshake(&mut client, &mut server);
                assert_eq!(
                    client.negotiated_cipher_suite().map(|s| s.suite()),
                    Some(name)
                );
                assert_eq!(
                    client.negotiated_key_exchange_group().map(|g| g.name()),
                    Some(group.name())
                );

                // Three records with reads between: rustls holds at most 16 KiB
                // of plaintext unread.
                for _ in 0..3 {
                    client.writer().write_all(&tested_long).unwrap();
                    shuttle(&mut client, &mut server).unwrap();
                    let mut got = vec![0; tested_long.len()];
                    server.reader().read_exact(&mut got).unwrap();
                    assert_eq!(got, tested_long, "{name:?}");
                }
                server.writer().write_all(b"short").unwrap();
                shuttle(&mut client, &mut server).unwrap();
                let mut got = [0; 5];
                client.reader().read_exact(&mut got).unwrap();
                assert_eq!(&got, b"short", "{name:?}");
            }
        }
    }

    #[test]
    fn a_changed_record_is_refused_under_every_suite() {
        for suite in [
            TLS13_CHACHA20_POLY1305_SHA256,
            TLS13_AES_128_GCM_SHA256,
            TLS13_AES_256_GCM_SHA384,
            TLS12_ECDSA_CHACHA20,
            TLS12_ECDSA_AES128,
            TLS12_ECDSA_AES256,
        ] {
            let (mut client, mut server) = connect(narrowed(suite, &X25519));
            handshake(&mut client, &mut server);
            client.writer().write_all(b"to be changed").unwrap();
            let mut wire = Vec::new();
            while client.wants_write() {
                client.write_tls(&mut wire).unwrap();
            }
            let last = wire.len() - 1;
            wire[last] ^= 1;
            let mut reader = wire.as_slice();
            server.read_tls(&mut reader).unwrap();
            assert!(
                matches!(server.process_new_packets(), Err(Error::DecryptError)),
                "{:?}",
                suite.suite()
            );
        }
    }

    #[test]
    fn a_key_of_the_wrong_length_seals_and_opens_nothing() {
        let short = AeadKey::from([0u8; 32]);
        let mut key = Tls13Aead(Algorithm::Aes128Gcm).encrypter(short, Iv::new([0; 12]));
        let payload = rustls::crypto::cipher::OutboundChunks::Single(b"x");
        let message = OutboundPlainMessage {
            typ: ContentType::ApplicationData,
            version: ProtocolVersion::TLSv1_3,
            payload,
        };
        assert!(matches!(key.encrypt(message, 0), Err(Error::EncryptError)));
        let mut open =
            Tls13Aead(Algorithm::Aes128Gcm).decrypter(AeadKey::from([0u8; 32]), Iv::new([0; 12]));
        let mut body = [0u8; 32];
        assert!(matches!(
            open.decrypt(inbound(&mut body), 0),
            Err(Error::DecryptError)
        ));
    }

    // ---- answers td did not compute -----------------------------------------
    //
    // Both ends of every connection above run this file, so an error made the same
    // way on both, a reused nonce or a wrong AAD, connects happily and only a real
    // server would notice. These do not depend on the other end.

    /// "hello" as application data under key bytes 0, 1, 2... and IV bytes a0, a1,
    /// a2..., each frozen from ring 0.17.14: through rustls's ring provider where its
    /// key type allows, and for AES-128, whose key length rustls keeps private, from
    /// ring's AEAD with rustls's nonce and AAD functions.
    const RECORDS: &[(&str, u64, &str)] = &[
        (
            "tls13-chacha",
            0,
            "170303001664ce143322f19ce0a7af3f1dd372502eed4340caf7d7",
        ),
        (
            "tls13-chacha",
            1,
            "17030300161a10a09f806a268485fcd588bdc7963eae82591b6f58",
        ),
        (
            "tls13-chacha",
            4294967301,
            "1703030016d91a52b47ff0edb9c5f57ad2ed30de5ee1107f5891a4",
        ),
        (
            "tls13-aes256",
            0,
            "17030300168e7d10412adc6b67281b21164071a6344d91b6680631",
        ),
        (
            "tls13-aes256",
            1,
            "17030300163dfbaf0bbff8b7ce3fc975167a386dd8cc1a1dbbd609",
        ),
        (
            "tls13-aes256",
            4294967301,
            "1703030016af946c48171a93effdc659e5dda036a7e6273463b3c9",
        ),
        (
            "tls12-chacha",
            0,
            "170303001564ce143322103e11b7f9ff8aef0470aa62a8753da6",
        ),
        (
            "tls12-chacha",
            1,
            "17030300151a10a09f800297f9ae0e48b378d8eb05f698bb45e6",
        ),
        (
            "tls12-chacha",
            4294967301,
            "1703030015d91a52b47f3b153253636d628de2b6b72a3b30129e",
        ),
        (
            "tls12-aes256",
            0,
            "170303001da4a5a6a7a8a9aaab8e7d10412a3a5331e8312fa0cb733abc23bac4d2cf",
        ),
        (
            "tls12-aes256",
            1,
            "170303001da4a5a6a7a8a9aaaa3dfbaf0bbf14d4b02de2ec36a7047372a783474a6f",
        ),
        (
            "tls12-aes256",
            4294967301,
            "170303001da4a5a6a6a8a9aaaeaf946c48172872572a95e06c7cb42a26d7f7999fee",
        ),
        (
            "tls13-aes128",
            0,
            "1703030016c2e354d7119eb4e3c26f9a2769c0df1e4a089219fbd8",
        ),
        (
            "tls13-aes128",
            1,
            "1703030016579f0af6c5fd6a48001237536fa3614d18bb75413627",
        ),
        (
            "tls13-aes128",
            4294967301,
            "1703030016f6d2a849a46cf4e47570b8fa13ca891cc92a9c1c25f5",
        ),
        (
            "tls12-aes128",
            0,
            "170303001da4a5a6a7a8a9aaabc2e354d7115185d5535a4b36b45b562b03310bcf05",
        ),
        (
            "tls12-aes128",
            1,
            "170303001da4a5a6a7a8a9aaaa579f0af6c5b9c8f9e22f54a6bc0d1b347411ef6881",
        ),
        (
            "tls12-aes128",
            4294967301,
            "170303001da4a5a6a6a8a9aaaef6d2a849a43fd971b42a4403512ed0710215f60995",
        ),
    ];

    const RECORD_FAMILIES: &[&str] = &[
        "tls13-chacha",
        "tls13-aes128",
        "tls13-aes256",
        "tls12-chacha",
        "tls12-aes128",
        "tls12-aes256",
    ];

    fn key_32() -> [u8; 32] {
        std::array::from_fn(|i| i as u8)
    }

    fn fixed_iv() -> [u8; 12] {
        std::array::from_fn(|i| 0xa0 + i as u8)
    }

    fn aes_128() -> RecordKey {
        let key: [u8; 16] = std::array::from_fn(|i| i as u8);
        RecordKey::Aes(AesGcm::new_128(&key))
    }

    type Ends = (
        Box<dyn MessageEncrypter>,
        Box<dyn MessageDecrypter>,
        ProtocolVersion,
    );

    fn tls13_ends(suite: SupportedCipherSuite) -> Ends {
        let SupportedCipherSuite::Tls13(suite) = suite else {
            panic!("a TLS 1.3 suite");
        };
        (
            suite
                .aead_alg
                .encrypter(AeadKey::from(key_32()), Iv::new(fixed_iv())),
            suite
                .aead_alg
                .decrypter(AeadKey::from(key_32()), Iv::new(fixed_iv())),
            ProtocolVersion::TLSv1_3,
        )
    }

    fn tls12_ends(suite: SupportedCipherSuite, fixed: &[u8], explicit: &[u8]) -> Ends {
        let SupportedCipherSuite::Tls12(suite) = suite else {
            panic!("a TLS 1.2 suite");
        };
        (
            suite
                .aead_alg
                .encrypter(AeadKey::from(key_32()), fixed, explicit),
            suite.aead_alg.decrypter(AeadKey::from(key_32()), fixed),
            ProtocolVersion::TLSv1_2,
        )
    }

    fn ends(family: &str) -> Ends {
        let iv = fixed_iv();
        match family {
            "tls13-chacha" => tls13_ends(TLS13_CHACHA20_POLY1305_SHA256),
            "tls13-aes256" => tls13_ends(TLS13_AES_256_GCM_SHA384),
            "tls13-aes128" => (
                Box::new(Tls13Records {
                    key: aes_128(),
                    iv: Iv::new(iv),
                }),
                Box::new(Tls13Records {
                    key: aes_128(),
                    iv: Iv::new(iv),
                }),
                ProtocolVersion::TLSv1_3,
            ),
            "tls12-chacha" => tls12_ends(TLS12_ECDSA_CHACHA20, &iv, &[]),
            "tls12-aes256" => tls12_ends(TLS12_ECDSA_AES256, &iv[..4], &iv[4..]),
            "tls12-aes128" => (
                Box::new(Tls12GcmSeal { key: aes_128(), iv }),
                Box::new(Tls12GcmOpen {
                    key: aes_128(),
                    salt: iv[..4].try_into().unwrap(),
                }),
                ProtocolVersion::TLSv1_2,
            ),
            other => panic!("no record family {other}"),
        }
    }

    fn outbound(payload: &[u8], version: ProtocolVersion) -> OutboundPlainMessage<'_> {
        OutboundPlainMessage {
            typ: ContentType::ApplicationData,
            version,
            payload: rustls::crypto::cipher::OutboundChunks::Single(payload),
        }
    }

    /// A TLS 1.3 record's outer version is 1.2's too.
    fn inbound(body: &mut [u8]) -> InboundOpaqueMessage<'_> {
        InboundOpaqueMessage::new(ContentType::ApplicationData, ProtocolVersion::TLSv1_2, body)
    }

    #[test]
    fn records_are_the_bytes_ring_wrote_and_open_only_at_their_own_sequence_number() {
        assert_eq!(RECORDS.len(), 3 * RECORD_FAMILIES.len());
        for (family, seq, wire) in RECORDS {
            let (mut seal, mut open, version) = ends(family);
            let written = seal
                .encrypt(outbound(b"hello", version), *seq)
                .unwrap()
                .encode();
            assert_eq!(
                to_hex(&written),
                *wire,
                "{family} at {seq}: the nonce, the AAD or the layout moved"
            );
            let wire = from_hex(wire).unwrap();
            let mut body = wire[5..].to_vec();
            let mut again = body.clone();
            let opened = open.decrypt(inbound(&mut body), *seq).unwrap();
            assert_eq!(opened.typ, ContentType::ApplicationData);
            assert_eq!(opened.payload, b"hello", "{family} at {seq}");
            assert!(
                matches!(
                    open.decrypt(inbound(&mut again), *seq + 1),
                    Err(Error::DecryptError)
                ),
                "{family} opened at another sequence number"
            );
        }
        // The RSA suites are the ECDSA suites' records under other signatures.
        let iv = fixed_iv();
        for (family, ends) in [
            ("tls12-chacha", tls12_ends(TLS12_RSA_CHACHA20, &iv, &[])),
            (
                "tls12-aes256",
                tls12_ends(TLS12_RSA_AES256, &iv[..4], &iv[4..]),
            ),
        ] {
            let (mut seal, _, version) = ends;
            for (_, seq, wire) in RECORDS.iter().filter(|row| row.0 == family) {
                let written = seal
                    .encrypt(outbound(b"hello", version), *seq)
                    .unwrap()
                    .encode();
                assert_eq!(to_hex(&written), *wire, "RSA {family}");
            }
        }
    }

    #[test]
    fn a_tls12_iv_of_the_wrong_length_seals_and_opens_nothing() {
        let sealers: [Box<dyn MessageEncrypter>; 2] = [
            Tls12Gcm(Algorithm::Aes256Gcm).encrypter(AeadKey::from(key_32()), &[0; 3], &[0; 8]),
            Tls12ChaCha.encrypter(AeadKey::from(key_32()), &[0; 11], &[]),
        ];
        for mut seal in sealers {
            assert!(matches!(
                seal.encrypt(outbound(b"x", ProtocolVersion::TLSv1_2), 0),
                Err(Error::EncryptError)
            ));
        }
        let openers: [Box<dyn MessageDecrypter>; 2] = [
            Tls12Gcm(Algorithm::Aes256Gcm).decrypter(AeadKey::from(key_32()), &[0; 3]),
            Tls12ChaCha.decrypter(AeadKey::from(key_32()), &[0; 11]),
        ];
        for mut open in openers {
            let mut body = [0u8; 32];
            assert!(matches!(
                open.decrypt(inbound(&mut body), 0),
                Err(Error::DecryptError)
            ));
        }
    }

    // Both ends of a handshake run this file, so a secret that came out wrong, even
    // all zeros, would still agree with itself. RFC 7748 6.1 and RFC 5903 8.1 and 8.2.
    #[test]
    fn each_group_agrees_on_the_secret_its_rfc_gives() {
        fn bytes(text: &str) -> Vec<u8> {
            from_hex(text).unwrap()
        }

        let secret: [u8; 32] =
            bytes("77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a")
                .try_into()
                .unwrap();
        let exchange = Box::new(X25519Exchange {
            public: x25519::public_key(&secret),
            secret,
        });
        assert_eq!(
            to_hex(exchange.pub_key()),
            "8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a"
        );
        let shared = exchange
            .complete(&bytes(
                "de9edb7d7b7dc1b4d35b61c2ece435373f8343c85b78674dadfc7e146f882b4f",
            ))
            .unwrap();
        assert_eq!(
            to_hex(shared.secret_bytes()),
            "4a5d9d5ba4ce2de1728e3bf480350f25e07e21c947d19e3376f09b3c1e161742"
        );

        let secret: [u8; 32] =
            bytes("c88f01f510d9ac3f70a292daa2316de544e9aab8afe84049c62a9c57862d1433")
                .try_into()
                .unwrap();
        let secret = p256::SecretKey::from_bytes(&secret).unwrap();
        let exchange = Box::new(Secp256r1Exchange {
            public: secret.public_key().unwrap(),
            secret,
        });
        assert_eq!(
            to_hex(exchange.pub_key()),
            "04dad0b65394221cf9b051e1feca5787d098dfe637fc90b9ef945d0c3772581180\
             5271a0461cdb8252d61f1c456fa3e59ab1f45b33accf5f58389e0577b8990bb3"
        );
        let shared = exchange
            .complete(&bytes(
                "04d12dfb5289c8d4f81208b70270398c342296970a0bccb74c736fc7554494bf63\
                 56fbf3ca366cc23e8157854c13c58d6aac23f046ada30f8353e74f33039872ab",
            ))
            .unwrap();
        assert_eq!(
            to_hex(shared.secret_bytes()),
            "d6840f6b42f6edafd13116e0e12565202fef8e9ece7dce03812464d04b9442de"
        );

        let secret: [u8; 48] = bytes(
            "099f3c7034d4a2c699884d73a375a67f7624ef7c6b3c0f160647b67414dce655\
             e35b538041e649ee3faef896783ab194",
        )
        .try_into()
        .unwrap();
        let secret = p384::SecretKey::from_bytes(&secret).unwrap();
        let exchange = Box::new(Secp384r1Exchange {
            public: secret.public_key().unwrap(),
            secret,
        });
        assert_eq!(
            to_hex(exchange.pub_key()),
            "04667842d7d180ac2cde6f74f37551f55755c7645c20ef73e31634fe72b4c55ee6\
             de3ac808acb4bdb4c88732aee95f41aa\
             9482ed1fc0eeb9cafc4984625ccfc23f65032149e0e144ada024181535a0f38e\
             eb9fcff3c2c947dae69b4c634573a81c"
        );
        let shared = exchange
            .complete(&bytes(
                "04e558dbef53eecde3d3fccfc1aea08a89a987475d12fd950d83cfa41732bc509d\
                 0d1ac43a0336def96fda41d0774a3571\
                 dcfbec7aacf3196472169e838430367f66eebe3c6e70c416dd5f0c68759dd1ff\
                 f83fa40142209dff5eaad96db9e6386c",
            ))
            .unwrap();
        assert_eq!(
            to_hex(shared.secret_bytes()),
            "11187331c279962d93d604243fd592cb9d0a926f422e47187521287e7156c5c4\
             d603135569b9e9d09cf5d4a270f59746"
        );

        // start() hands out each group's own key, and refuses another's.
        let lengths = [
            (&X25519 as &dyn SupportedKxGroup, 32),
            (&SECP256R1, 65),
            (&SECP384R1, 97),
        ];
        for (group, length) in lengths {
            let started = group.start().unwrap();
            assert_eq!(started.pub_key().len(), length, "{:?}", group.name());
            assert_eq!(started.group(), group.name());
            assert!(started.complete(&[4; 33]).is_err(), "{:?}", group.name());
        }
    }

    #[test]
    fn a_record_past_the_fragment_limit_is_refused_though_it_authenticates() {
        for family in RECORD_FAMILIES {
            let (mut seal, mut open, version) = ends(family);
            for (len, fits) in [(16384, true), (16385, false)] {
                let plain = vec![0x5a; len];
                let wire = seal.encrypt(outbound(&plain, version), 1).unwrap().encode();
                let mut body = wire[5..].to_vec();
                let opened = open.decrypt(inbound(&mut body), 1);
                if fits {
                    assert_eq!(opened.unwrap().payload, plain, "{family}");
                } else {
                    assert!(
                        matches!(opened, Err(Error::PeerSentOversizedRecord)),
                        "{family} took {len} bytes"
                    );
                }
            }
        }
    }

    /// Python's hmac module: HKDF's first block under salt "td salt", secret
    /// "td secret" and info "td info", and the TLS 1.2 PRF's first 48 bytes under
    /// secret "td secret", label "td label" and seed "td seed".
    const HKDF_SHA256: &str = "97e28e6be6b2ae8eaa217bbf3e7fa25bc7063a32ceefb364fa0958b2e2435a40";
    const HKDF_SHA384: &str = "9f9f6d6d087018b1382eee4113f7011c6c584c3b5c2d995d8848038523998ce5\
                               07271831e90f79a28758866c4863e752";
    const PRF_SHA256: &str = "2e84d30f08bb82f27eb1f799c4fb619240ed70c9fd619b6b589e2d0462391e3f\
                              4a30a3257cbbf469580c32ff9766eead";
    const PRF_SHA384: &str = "96ed9ab56cec82b9c402d6e878722ec9427392878c55334babf9643c8ca5e35c\
                              49a7ba38c9ac62468baf2fc25d1228b5";

    #[test]
    fn each_suite_pairs_the_cipher_hash_schedule_and_limit_its_name_says() {
        const AES_LIMIT: u64 = 1 << 24;
        let provider = provider();
        let names: Vec<CipherSuite> = provider.cipher_suites.iter().map(|s| s.suite()).collect();
        assert_eq!(
            names,
            [
                CipherSuite::TLS13_CHACHA20_POLY1305_SHA256,
                CipherSuite::TLS13_AES_128_GCM_SHA256,
                CipherSuite::TLS13_AES_256_GCM_SHA384,
                CipherSuite::TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256,
                CipherSuite::TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256,
                CipherSuite::TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256,
                CipherSuite::TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256,
                CipherSuite::TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384,
                CipherSuite::TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384,
            ]
        );
        // Written out, not named: this pins the constants the suites use.
        const ECDSA: &[SignatureScheme] = &[
            SignatureScheme::ED25519,
            SignatureScheme::ECDSA_NISTP384_SHA384,
            SignatureScheme::ECDSA_NISTP256_SHA256,
        ];
        const RSA: &[SignatureScheme] = &[
            SignatureScheme::RSA_PSS_SHA512,
            SignatureScheme::RSA_PSS_SHA384,
            SignatureScheme::RSA_PSS_SHA256,
            SignatureScheme::RSA_PKCS1_SHA512,
            SignatureScheme::RSA_PKCS1_SHA384,
            SignatureScheme::RSA_PKCS1_SHA256,
        ];
        // RFC 5288 and 7905: a GCM key block fixes 4 bytes of IV, ChaCha's 12.
        const GCM: (usize, usize) = (4, 8);
        const CHACHA: (usize, usize) = (12, 0);
        // The hash, key length, limit, and a TLS 1.2 suite's IV shape and signatures.
        type Row = (
            HashAlgorithm,
            usize,
            u64,
            (usize, usize),
            &'static [SignatureScheme],
        );
        let expected: &[Row] = &[
            (HashAlgorithm::SHA256, 32, u64::MAX, CHACHA, &[]),
            (HashAlgorithm::SHA256, 16, AES_LIMIT, GCM, &[]),
            (HashAlgorithm::SHA384, 32, AES_LIMIT, GCM, &[]),
            (HashAlgorithm::SHA256, 32, u64::MAX, CHACHA, ECDSA),
            (HashAlgorithm::SHA256, 32, u64::MAX, CHACHA, RSA),
            (HashAlgorithm::SHA256, 16, AES_LIMIT, GCM, ECDSA),
            (HashAlgorithm::SHA256, 16, AES_LIMIT, GCM, RSA),
            (HashAlgorithm::SHA384, 32, AES_LIMIT, GCM, ECDSA),
            (HashAlgorithm::SHA384, 32, AES_LIMIT, GCM, RSA),
        ];
        assert_eq!(provider.cipher_suites.len(), expected.len());
        let sha384 = |hash| hash == HashAlgorithm::SHA384;
        for (suite, (hash, key_len, limit, iv_shape, schemes)) in
            provider.cipher_suites.iter().zip(expected)
        {
            let name = suite.suite();
            match suite {
                SupportedCipherSuite::Tls13(suite) => {
                    assert_eq!(suite.common.hash_provider.algorithm(), *hash, "{name:?}");
                    assert_eq!(suite.common.confidentiality_limit, *limit, "{name:?}");
                    assert_eq!(suite.aead_alg.key_len(), *key_len, "{name:?}");
                    let block = suite
                        .hkdf_provider
                        .extract_from_secret(Some(b"td salt"), b"td secret")
                        .expand_block(&[b"td info"]);
                    let answer = if sha384(*hash) {
                        HKDF_SHA384
                    } else {
                        HKDF_SHA256
                    };
                    assert_eq!(to_hex(block.as_ref()), answer, "{name:?}");
                }
                SupportedCipherSuite::Tls12(suite) => {
                    assert_eq!(suite.common.hash_provider.algorithm(), *hash, "{name:?}");
                    assert_eq!(suite.common.confidentiality_limit, *limit, "{name:?}");
                    let shape = suite.aead_alg.key_block_shape();
                    assert_eq!(shape.enc_key_len, *key_len, "{name:?}");
                    assert_eq!(
                        (shape.fixed_iv_len, shape.explicit_nonce_len),
                        *iv_shape,
                        "{name:?}"
                    );
                    assert_eq!(suite.sign, *schemes, "{name:?}");
                    assert!(suite.kx == KeyExchangeAlgorithm::ECDHE, "{name:?}");
                    let mut output = [0u8; 48];
                    suite.prf_provider.for_secret(
                        &mut output,
                        b"td secret",
                        b"td label",
                        b"td seed",
                    );
                    let answer = if sha384(*hash) {
                        PRF_SHA384
                    } else {
                        PRF_SHA256
                    };
                    assert_eq!(to_hex(&output), answer, "{name:?}");
                }
            }
        }
        let groups: Vec<NamedGroup> = provider.kx_groups.iter().map(|g| g.name()).collect();
        assert_eq!(
            groups,
            [
                NamedGroup::X25519,
                NamedGroup::secp256r1,
                NamedGroup::secp384r1
            ]
        );
    }

    // tests/signature_answers.py made these with big integers. ring verified each
    // signed row it has the algorithm for; td-crypto's differential tests hold
    // every row to AWS-LC, the P-521, SHA-512 ECDSA and refused ones included.
    const SIGNATURES: &str = include_str!("../tests/signature_answers.txt");

    /// The algorithm a row signs under: a SMALL_ORDER_ row is a signature under
    /// a small-order key, which verification takes, for any message. Every
    /// REFUSED_ row is no algorithm's.
    fn row_algorithm(name: &str) -> &str {
        name.strip_prefix("SMALL_ORDER_").unwrap_or(name)
    }

    /// SEC 1's compressed form of an uncompressed point, by y's parity.
    fn compressed(point: &[u8]) -> Vec<u8> {
        let size = (point.len() - 1) / 2;
        let mut out = vec![2 | (point[point.len() - 1] & 1)];
        out.extend_from_slice(&point[1..=size]);
        out
    }

    /// Every `verifiers` export.
    fn exported() -> Vec<&'static dyn SignatureVerificationAlgorithm> {
        use verifiers::*;
        vec![
            ECDSA_P256_SHA256,
            ECDSA_P256_SHA384,
            ECDSA_P256_SHA512,
            ECDSA_P384_SHA256,
            ECDSA_P384_SHA384,
            ECDSA_P384_SHA512,
            ECDSA_P521_SHA256,
            ECDSA_P521_SHA384,
            ECDSA_P521_SHA512,
            ED25519,
            RSA_PSS_SHA256,
            RSA_PSS_SHA384,
            RSA_PSS_SHA512,
            RSA_PKCS1_SHA256,
            RSA_PKCS1_SHA384,
            RSA_PKCS1_SHA512,
            RSA_PKCS1_SHA256_BARE,
            RSA_PKCS1_SHA384_BARE,
            RSA_PKCS1_SHA512_BARE,
        ]
    }

    #[test]
    fn the_provider_verifiers_take_only_uncompressed_keys() {
        let all = SIGNATURE_ALGORITHMS.all;
        let mut checked = 0;
        for line in SIGNATURES.lines().filter(|l| !l.starts_with("PGP_")) {
            let fields: Vec<&str> = line.split(' ').collect();
            let [name, public, message, signature] = fields[..] else {
                panic!("malformed row: {line}");
            };
            // Only the curve and hash pairs `provider()` has.
            let Some(own) = all.iter().find(|v| format!("{v:?}") == name) else {
                continue;
            };
            if !name.starts_with("ECDSA_") {
                continue;
            }
            let (public, message, signature) = (
                from_hex(public).unwrap(),
                from_hex(message).unwrap(),
                from_hex(signature).unwrap(),
            );
            assert!(own.verify_signature(&public, &message, &signature).is_ok());
            for verifier in all {
                assert!(verifier
                    .verify_signature(&compressed(&public), &message, &signature)
                    .is_err());
            }
            checked += 1;
        }
        assert_eq!(checked, 4);
    }

    #[test]
    fn exported_verifiers_take_their_own_rows_with_the_key_in_either_form() {
        let all = exported();
        let mut exercised = std::collections::BTreeSet::new();
        for line in SIGNATURES.lines().filter(|l| !l.starts_with("PGP_")) {
            let fields: Vec<&str> = line.split(' ').collect();
            let [name, public, message, signature] = fields[..] else {
                panic!("malformed row: {line}");
            };
            let unhex = |text| from_hex(text).unwrap();
            let (public, message, signature) = (unhex(public), unhex(message), unhex(signature));
            // Each key form, and whether it is the row's key.
            let mut keys = vec![(public.clone(), true)];
            if name.contains("ECDSA_") {
                let short = compressed(&public);
                let mut negated = short.clone();
                negated[0] ^= 1;
                let mut hybrid = public.clone();
                hybrid[0] = 6 | (public[public.len() - 1] & 1);
                keys.extend([(short, true), (negated, false), (hybrid, false)]);
            }
            let algorithm = row_algorithm(name);
            for verifier in &all {
                let verifier_name = format!("{verifier:?}");
                let own =
                    verifier_name == algorithm || verifier_name == format!("{algorithm}_BARE");
                for (form, (key, is_own_key)) in keys.iter().enumerate() {
                    assert_eq!(
                        verifier.verify_signature(key, &message, &signature).is_ok(),
                        own && *is_own_key,
                        "{name} under {verifier_name}, key form {form}"
                    );
                }
                if own {
                    exercised.insert(verifier_name.clone());
                    for (key, _) in keys.iter().filter(|(_, own_key)| *own_key) {
                        let mut changed = message.clone();
                        changed[0] ^= 1;
                        assert_eq!(
                            verifier.verify_signature(key, &changed, &signature).is_ok(),
                            algorithm != name,
                            "{name} over another message"
                        );
                        let mut changed = signature.clone();
                        let last = changed.len() - 1;
                        changed[last] ^= 1;
                        assert!(verifier.verify_signature(key, &message, &changed).is_err());
                    }
                }
            }
        }
        assert_eq!(exercised.len(), all.len(), "{exercised:?}");
    }

    #[test]
    fn the_exported_verifiers_name_their_algorithms() {
        use alg_id::*;
        let rsa_bare = |last: u8| {
            AlgorithmIdentifier::from_slice(
                [
                    0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, last,
                ]
                .to_vec()
                .leak(),
            )
        };
        let expected = [
            ("ECDSA_P256_SHA256", ECDSA_P256, ECDSA_SHA256),
            ("ECDSA_P256_SHA384", ECDSA_P256, ECDSA_SHA384),
            ("ECDSA_P256_SHA512", ECDSA_P256, ECDSA_SHA512),
            ("ECDSA_P384_SHA256", ECDSA_P384, ECDSA_SHA256),
            ("ECDSA_P384_SHA384", ECDSA_P384, ECDSA_SHA384),
            ("ECDSA_P384_SHA512", ECDSA_P384, ECDSA_SHA512),
            ("ECDSA_P521_SHA256", ECDSA_P521, ECDSA_SHA256),
            ("ECDSA_P521_SHA384", ECDSA_P521, ECDSA_SHA384),
            ("ECDSA_P521_SHA512", ECDSA_P521, ECDSA_SHA512),
            ("ED25519", alg_id::ED25519, alg_id::ED25519),
            ("RSA_PSS_SHA256", RSA_ENCRYPTION, alg_id::RSA_PSS_SHA256),
            ("RSA_PSS_SHA384", RSA_ENCRYPTION, alg_id::RSA_PSS_SHA384),
            ("RSA_PSS_SHA512", RSA_ENCRYPTION, alg_id::RSA_PSS_SHA512),
            ("RSA_PKCS1_SHA256", RSA_ENCRYPTION, alg_id::RSA_PKCS1_SHA256),
            ("RSA_PKCS1_SHA384", RSA_ENCRYPTION, alg_id::RSA_PKCS1_SHA384),
            ("RSA_PKCS1_SHA512", RSA_ENCRYPTION, alg_id::RSA_PKCS1_SHA512),
            ("RSA_PKCS1_SHA256_BARE", RSA_ENCRYPTION, rsa_bare(0x0b)),
            ("RSA_PKCS1_SHA384_BARE", RSA_ENCRYPTION, rsa_bare(0x0c)),
            ("RSA_PKCS1_SHA512_BARE", RSA_ENCRYPTION, rsa_bare(0x0d)),
        ];
        let all = exported();
        assert_eq!(all.len(), expected.len());
        for (verifier, (name, public, signature)) in all.iter().zip(expected) {
            assert_eq!(format!("{verifier:?}"), name);
            assert_eq!(verifier.public_key_alg_id(), public, "{name}");
            assert_eq!(verifier.signature_alg_id(), signature, "{name}");
            // Ed25519 and RSA are `provider()`'s own statics; ECDSA is not.
            let shared = SIGNATURE_ALGORITHMS
                .all
                .iter()
                .any(|v| std::ptr::addr_eq(*v, *verifier));
            assert_eq!(shared, !name.starts_with("ECDSA_"), "{name}");
        }
    }

    #[test]
    fn the_tls12_ecdsa_suites_are_the_provider_suites_with_rustls_scheme_list() {
        let provider = provider();
        // Written out: rustls's own providers' TLS 1.2 ECDSA list.
        const ECDSA: &[SignatureScheme] = &[
            SignatureScheme::ED25519,
            SignatureScheme::ECDSA_NISTP521_SHA512,
            SignatureScheme::ECDSA_NISTP384_SHA384,
            SignatureScheme::ECDSA_NISTP256_SHA256,
        ];
        let suites = [
            tls12_ecdsa::TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256,
            tls12_ecdsa::TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256,
            tls12_ecdsa::TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384,
        ];
        for suite in suites {
            let SupportedCipherSuite::Tls12(exported) = suite else {
                panic!("a TLS 1.2 suite");
            };
            let name = exported.common.suite;
            let ours = provider
                .cipher_suites
                .iter()
                .find_map(|s| match s {
                    SupportedCipherSuite::Tls12(s) if s.common.suite == name => Some(*s),
                    _ => None,
                })
                .unwrap();
            assert_eq!(exported.sign, ECDSA, "{name:?}");
            assert!(exported.kx == KeyExchangeAlgorithm::ECDHE, "{name:?}");
            assert_eq!(
                exported.common.hash_provider.algorithm(),
                ours.common.hash_provider.algorithm()
            );
            assert_eq!(
                exported.common.confidentiality_limit,
                ours.common.confidentiality_limit
            );
            let (shape, our_shape) = (
                exported.aead_alg.key_block_shape(),
                ours.aead_alg.key_block_shape(),
            );
            assert_eq!(
                (
                    shape.enc_key_len,
                    shape.fixed_iv_len,
                    shape.explicit_nonce_len
                ),
                (
                    our_shape.enc_key_len,
                    our_shape.fixed_iv_len,
                    our_shape.explicit_nonce_len
                ),
                "{name:?}"
            );
            let (mut output, mut our_output) = ([0u8; 48], [0u8; 48]);
            exported
                .prf_provider
                .for_secret(&mut output, b"td secret", b"td label", b"td seed");
            ours.prf_provider
                .for_secret(&mut our_output, b"td secret", b"td label", b"td seed");
            assert_eq!(output, our_output, "{name:?}");
        }
        // And they serve over every group.
        for suite in suites {
            for group in provider.kx_groups.iter().copied() {
                let narrowed = Arc::new(CryptoProvider {
                    cipher_suites: vec![suite],
                    kx_groups: vec![group],
                    ..super::provider()
                });
                let (mut client, mut server) = connect(narrowed);
                handshake(&mut client, &mut server);
                assert_eq!(
                    client.negotiated_cipher_suite().map(|s| s.suite()),
                    Some(suite.suite())
                );
                client.writer().write_all(b"exported").unwrap();
                shuttle(&mut client, &mut server).unwrap();
                let mut got = [0; 8];
                server.reader().read_exact(&mut got).unwrap();
                assert_eq!(&got, b"exported");
            }
        }
    }

    #[test]
    fn every_verifier_takes_its_own_signature_and_nobody_elses() {
        let all = SIGNATURE_ALGORITHMS.all;
        assert_eq!(all.len(), 14);
        let mut exercised = std::collections::BTreeSet::new();
        for line in SIGNATURES.lines().filter(|l| !l.starts_with("PGP_")) {
            let fields: Vec<&str> = line.split(' ').collect();
            let [name, public, message, signature] = fields[..] else {
                panic!("malformed row: {line}");
            };
            let unhex = |text| from_hex(text).unwrap();
            let (public, message, signature) = (unhex(public), unhex(message), unhex(signature));
            let algorithm = row_algorithm(name);
            for verifier in all {
                let verifier_name = format!("{verifier:?}");
                // The _BARE names are the same algorithm's other spelling.
                let own =
                    verifier_name == algorithm || verifier_name == format!("{algorithm}_BARE");
                assert_eq!(
                    verifier
                        .verify_signature(&public, &message, &signature)
                        .is_ok(),
                    own,
                    "{name} under {verifier_name}"
                );
                if own {
                    exercised.insert(verifier_name.clone());
                    let mut changed = message.clone();
                    changed[0] ^= 1;
                    // A small-order key's signature verifies any message.
                    assert_eq!(
                        verifier
                            .verify_signature(&public, &changed, &signature)
                            .is_ok(),
                        algorithm != name,
                        "{name} over another message"
                    );
                    let mut changed = signature.clone();
                    let last = changed.len() - 1;
                    changed[last] ^= 1;
                    assert!(
                        verifier
                            .verify_signature(&public, &message, &changed)
                            .is_err(),
                        "{verifier_name} with another signature"
                    );
                }
            }
        }
        assert_eq!(exercised.len(), all.len(), "{exercised:?}");
    }

    #[test]
    fn each_verifier_names_its_algorithms_and_each_scheme_its_verifiers() {
        let expected: &[(&str, AlgorithmIdentifier, AlgorithmIdentifier)] = &[
            (
                "ECDSA_P256_SHA256",
                alg_id::ECDSA_P256,
                alg_id::ECDSA_SHA256,
            ),
            (
                "ECDSA_P256_SHA384",
                alg_id::ECDSA_P256,
                alg_id::ECDSA_SHA384,
            ),
            (
                "ECDSA_P384_SHA256",
                alg_id::ECDSA_P384,
                alg_id::ECDSA_SHA256,
            ),
            (
                "ECDSA_P384_SHA384",
                alg_id::ECDSA_P384,
                alg_id::ECDSA_SHA384,
            ),
            ("ED25519", alg_id::ED25519, alg_id::ED25519),
            (
                "RSA_PSS_SHA256",
                alg_id::RSA_ENCRYPTION,
                alg_id::RSA_PSS_SHA256,
            ),
            (
                "RSA_PSS_SHA384",
                alg_id::RSA_ENCRYPTION,
                alg_id::RSA_PSS_SHA384,
            ),
            (
                "RSA_PSS_SHA512",
                alg_id::RSA_ENCRYPTION,
                alg_id::RSA_PSS_SHA512,
            ),
            (
                "RSA_PKCS1_SHA256",
                alg_id::RSA_ENCRYPTION,
                alg_id::RSA_PKCS1_SHA256,
            ),
            (
                "RSA_PKCS1_SHA384",
                alg_id::RSA_ENCRYPTION,
                alg_id::RSA_PKCS1_SHA384,
            ),
            (
                "RSA_PKCS1_SHA512",
                alg_id::RSA_ENCRYPTION,
                alg_id::RSA_PKCS1_SHA512,
            ),
        ];
        for (name, public, signature) in expected {
            let verifier = SIGNATURE_ALGORITHMS
                .all
                .iter()
                .find(|v| format!("{v:?}") == *name)
                .unwrap();
            assert_eq!(
                verifier.public_key_alg_id().as_ref(),
                public.as_ref(),
                "{name}"
            );
            assert_eq!(
                verifier.signature_alg_id().as_ref(),
                signature.as_ref(),
                "{name}"
            );
        }
        // The same algorithm without its NULL: the OID of sha256WithRSAEncryption and
        // its siblings, from RFC 4055.
        for (name, last) in [
            ("RSA_PKCS1_SHA256_BARE", 0x0b),
            ("RSA_PKCS1_SHA384_BARE", 0x0c),
            ("RSA_PKCS1_SHA512_BARE", 0x0d),
        ] {
            let verifier = SIGNATURE_ALGORITHMS
                .all
                .iter()
                .find(|v| format!("{v:?}") == name)
                .unwrap();
            assert_eq!(
                verifier.signature_alg_id().as_ref(),
                [0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, last].as_slice(),
                "{name}"
            );
            assert_eq!(
                verifier.public_key_alg_id().as_ref(),
                alg_id::RSA_ENCRYPTION.as_ref()
            );
        }

        let mapping: Vec<(SignatureScheme, Vec<String>)> = SIGNATURE_ALGORITHMS
            .mapping
            .iter()
            .map(|(scheme, verifiers)| {
                (
                    *scheme,
                    verifiers.iter().map(|v| format!("{v:?}")).collect(),
                )
            })
            .collect();
        let named = |names: &[&str]| names.iter().map(|n| n.to_string()).collect::<Vec<_>>();
        assert_eq!(
            mapping,
            [
                (
                    SignatureScheme::ECDSA_NISTP384_SHA384,
                    named(&["ECDSA_P384_SHA384", "ECDSA_P256_SHA384"])
                ),
                (
                    SignatureScheme::ECDSA_NISTP256_SHA256,
                    named(&["ECDSA_P256_SHA256", "ECDSA_P384_SHA256"])
                ),
                (SignatureScheme::ED25519, named(&["ED25519"])),
                (SignatureScheme::RSA_PSS_SHA512, named(&["RSA_PSS_SHA512"])),
                (SignatureScheme::RSA_PSS_SHA384, named(&["RSA_PSS_SHA384"])),
                (SignatureScheme::RSA_PSS_SHA256, named(&["RSA_PSS_SHA256"])),
                (
                    SignatureScheme::RSA_PKCS1_SHA512,
                    named(&["RSA_PKCS1_SHA512"])
                ),
                (
                    SignatureScheme::RSA_PKCS1_SHA384,
                    named(&["RSA_PKCS1_SHA384"])
                ),
                (
                    SignatureScheme::RSA_PKCS1_SHA256,
                    named(&["RSA_PKCS1_SHA256"])
                ),
            ]
        );
    }

    #[test]
    fn the_providers_entropy_fills_what_it_is_given() {
        let random = provider().secure_random;
        let (mut one, mut other) = ([0u8; 32], [0u8; 32]);
        random.fill(&mut one).unwrap();
        random.fill(&mut other).unwrap();
        assert_ne!(one, [0; 32]);
        assert_ne!(one, other);
        // Every part of it, not just the start.
        let (head, tail) = one.split_at(16);
        assert_ne!(head, [0; 16]);
        assert_ne!(tail, [0; 16]);
        random.fill(&mut []).unwrap();
    }

    #[test]
    fn the_served_key_signs_only_its_own_scheme_and_refuses_another_certificate() {
        let provider = provider();
        let key = provider
            .key_provider
            .load_private_key(PrivateKeyDer::from_pem_slice(KEY).unwrap())
            .unwrap();
        assert_eq!(key.algorithm(), SignatureAlgorithm::ECDSA);
        assert!(key
            .choose_scheme(&[SignatureScheme::ECDSA_NISTP384_SHA384])
            .is_none());
        assert!(key.choose_scheme(&[]).is_none());
        let signer = key
            .choose_scheme(&[
                SignatureScheme::ED25519,
                SignatureScheme::ECDSA_NISTP256_SHA256,
            ])
            .unwrap();
        assert_eq!(signer.scheme(), SignatureScheme::ECDSA_NISTP256_SHA256);

        let chain = || -> Vec<CertificateDer<'static>> {
            CertificateDer::pem_slice_iter(CERT)
                .map(Result::unwrap)
                .collect()
        };
        let own = PrivateKeyDer::from_pem_slice(KEY).unwrap();
        assert!(rustls::sign::CertifiedKey::from_der(chain(), own, &provider).is_ok());
        let secret = [7u8; 32];
        let other = p256::SecretKey::from_bytes(&secret).unwrap();
        let der = p256_to_pkcs8(&secret, &other.public_key().unwrap());
        let other = PrivateKeyDer::try_from(der).unwrap();
        assert!(matches!(
            rustls::sign::CertifiedKey::from_der(chain(), other, &provider),
            Err(Error::InconsistentKeys(_))
        ));
    }

    #[test]
    fn the_fixture_key_and_only_that_shape_loads() {
        let PrivateKeyDer::Pkcs8(pkcs8) = PrivateKeyDer::from_pem_slice(KEY).unwrap() else {
            panic!("the fixture is PKCS#8");
        };
        let der = pkcs8.secret_pkcs8_der();
        let key = p256_from_pkcs8(der).expect("the fixture loads");
        let public = key.public_key().unwrap();
        let (secret, _) = der.split_at(P256_PKCS8_HEAD.len() + 32);
        let secret: &[u8; 32] = secret[P256_PKCS8_HEAD.len()..].try_into().unwrap();
        assert_eq!(p256_to_pkcs8(secret, &public), der, "round trip");

        let mut other_public = der.to_vec();
        let last = other_public.len() - 1;
        other_public[last] ^= 1;
        assert!(
            p256_from_pkcs8(&other_public).is_none(),
            "a mismatched public half"
        );
        assert!(
            p256_from_pkcs8(&der[..der.len() - 1]).is_none(),
            "a short file"
        );
        let mut trailing = der.to_vec();
        trailing.push(0);
        assert!(p256_from_pkcs8(&trailing).is_none(), "trailing bytes");
    }

    #[test]
    fn hmac_and_hash_bridges_give_the_primitive_answers() {
        use hash::Hash;
        use hmac::Hmac as _;
        let key = HMAC_SHA256.with_key(b"key");
        let joined = key.sign(&[b"The quick brown fox ", b"jumps over the lazy dog"]);
        // RFC-independent: Python's hmac module.
        assert_eq!(
            to_hex(joined.as_ref()),
            "f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8"
        );
        let mut running = HASH_SHA256.start();
        running.update(b"a");
        let forked = running.fork_finish();
        running.update(b"bc");
        assert_eq!(
            to_hex(running.finish().as_ref()),
            to_hex(HASH_SHA256.hash(b"abc").as_ref())
        );
        assert_eq!(
            to_hex(forked.as_ref()),
            to_hex(HASH_SHA256.hash(b"a").as_ref())
        );
    }
}
