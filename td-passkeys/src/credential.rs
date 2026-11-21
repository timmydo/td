use super::{field, put, take, Error, Plain, Result, Secret};
use td_fido::{
    digest,
    fido_cbor::Encoder,
    web_request::{Create, Get},
};
use td_primitives::{ec::p256::SecretKey, random};

/// Only the trusted user interface supplies this result for the current request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Approval {
    Cancelled,
    Present,
    Verified,
}
impl Approval {
    pub(super) fn flags(self, require_uv: bool) -> Result<u8> {
        match self {
            Self::Verified => Ok(5),
            Self::Present if !require_uv => Ok(1),
            _ => Err(Error::Refused),
        }
    }
}

pub struct Account<'a> {
    pub id: &'a [u8; 32],
    pub rp: &'a str,
    pub user: &'a [u8],
    pub name: &'a str,
    pub discoverable: bool,
}
pub(super) struct Credential<'a> {
    pub account: Account<'a>,
    secret: &'a [u8; 32],
}

impl<'a> Credential<'a> {
    pub fn read(mut input: &'a [u8]) -> Result<Self> {
        let id = take(&mut input, 32)?
            .try_into()
            .map_err(|_| Error::Invalid)?;
        let byte = take(&mut input, 1)?;
        let discoverable = match byte {
            [0] => false,
            [1] => true,
            _ => return Err(Error::Invalid),
        };
        let rp = std::str::from_utf8(field(&mut input, 253)?).map_err(|_| Error::Invalid)?;
        let user = field(&mut input, 64)?;
        let name = std::str::from_utf8(field(&mut input, 256)?).map_err(|_| Error::Invalid)?;
        let secret = take(&mut input, 32)?
            .try_into()
            .map_err(|_| Error::Invalid)?;
        if !input.is_empty() || user.is_empty() || rp.is_empty() {
            return Err(Error::Invalid);
        }
        Ok(Self {
            account: Account {
                id,
                rp,
                user,
                name,
                discoverable,
            },
            secret,
        })
    }

    pub fn assertion(&self, r: &Get<'_>, approval: Approval) -> Result<Vec<u8>> {
        if r.rp() != self.account.rp
            || (!r.allow().ids().is_empty()
                && !r.allow().ids().contains(&self.account.id.as_slice()))
            || (r.allow().ids().is_empty() && !self.account.discoverable)
        {
            return Err(Error::Missing);
        }
        // A silent browser probe can never turn an unlocked vault into a signer.
        if !r.presence() {
            return Err(Error::Refused);
        }
        let flags = approval.flags(r.verify_user())?;
        self.signed(r, flags, true)
    }

    // A browser preflights the explicit handles before a real create/get.
    // Empty client data cannot encode a WebAuthn origin or challenge.
    pub fn preflight(&self, r: &Get<'_>) -> Result<Vec<u8>> {
        if r.presence()
            || r.verify_user()
            || r.client_data_hash() != &digest(b"")
            || r.rp() != self.account.rp
            || !r.allow().ids().contains(&self.account.id.as_slice())
        {
            return Err(Error::Refused);
        }
        self.signed(r, 0, false)
    }

    fn signed(&self, r: &Get<'_>, flags: u8, include_user: bool) -> Result<Vec<u8>> {
        let mut signed = [0u8; 69];
        let (auth, hash) = signed.split_at_mut(37);
        auth.get_mut(..32)
            .ok_or(Error::Invalid)?
            .copy_from_slice(&digest(r.rp().as_bytes()));
        *auth.get_mut(32).ok_or(Error::Invalid)? = flags;
        hash.copy_from_slice(r.client_data_hash());
        let signature = SecretKey::from_bytes(self.secret)?.sign_sha256(&signed)?;
        let mut e = Encoder::new();
        let result = (|| {
            e.head(5, if include_user { 4 } else { 3 })?;
            e.head(0, 1)?;
            e.head(5, 2)?;
            e.text("id")?;
            e.bytes(self.account.id)?;
            e.text("type")?;
            e.text("public-key")?;
            e.head(0, 2)?;
            e.bytes(signed.get(..37).ok_or("missing authenticator data")?)?;
            e.head(0, 3)?;
            e.bytes(&signature)?;
            if include_user {
                e.head(0, 4)?;
                e.head(5, 1)?;
                e.text("id")?;
                e.bytes(self.account.user)?;
            }
            e.finish()
        })();
        result.map_err(|_| Error::Invalid)
    }
}

pub(super) fn make(r: &Create<'_>, approval: Approval) -> Result<(Plain, Vec<u8>)> {
    let flags = approval.flags(r.verify_user() || r.discoverable())? | 0x40;
    let mut scalar = Secret([0; 32]);
    let key = (0..64)
        .find_map(|_| {
            if let Err(error) = random::fill(&mut scalar.0) {
                return Some(Err(error));
            }
            SecretKey::from_bytes(&scalar.0).ok().map(Ok)
        })
        .ok_or(Error::Refused)??;
    let public = key.public_key()?;
    let mut id = [0; 32];
    random::fill(&mut id)?;
    let mut data = Plain(td_alloc::with_capacity(
        32 + 1
            + 6
            + r.rp().len()
            + r.user_id().len()
            + r.user_name().unwrap_or_default().len()
            + 32,
    )?);
    td_alloc::extend_from_slice(&mut data.0, &id)?;
    td_alloc::push(&mut data.0, u8::from(r.discoverable()))?;
    put(&mut data.0, r.rp().as_bytes())?;
    put(&mut data.0, r.user_id())?;
    put(&mut data.0, r.user_name().unwrap_or_default().as_bytes())?;
    td_alloc::extend_from_slice(&mut data.0, &scalar.0)?;
    let mut auth = td_alloc::with_capacity(164)?;
    td_alloc::extend_from_slice(&mut auth, &digest(r.rp().as_bytes()))?;
    td_alloc::push(&mut auth, flags)?;
    td_alloc::extend_from_slice(&mut auth, &[0; 20])?;
    td_alloc::extend_from_slice(&mut auth, &32u16.to_be_bytes())?;
    td_alloc::extend_from_slice(&mut auth, &id)?;
    let mut cose = Encoder::new();
    let coordinates = (|| {
        cose.head(5, 5)?;
        cose.head(0, 1)?;
        cose.head(0, 2)?;
        cose.head(0, 3)?;
        cose.head(1, 6)?;
        cose.head(1, 0)?;
        cose.head(0, 1)?;
        cose.head(1, 1)?;
        cose.bytes(public.get(1..33).ok_or("invalid P-256 point")?)?;
        cose.head(1, 2)?;
        cose.bytes(public.get(33..65).ok_or("invalid P-256 point")?)?;
        cose.finish()
    })()
    .map_err(|_| Error::Invalid)?;
    td_alloc::extend_from_slice(&mut auth, &coordinates)?;
    let mut reply = Encoder::new();
    let reply = (|| {
        reply.head(5, 3)?;
        reply.head(0, 1)?;
        reply.text("none")?;
        reply.head(0, 2)?;
        reply.bytes(&auth)?;
        reply.head(0, 3)?;
        reply.head(5, 0)?;
        reply.finish()
    })()
    .map_err(|_| Error::Invalid)?;
    Ok((data, reply))
}
