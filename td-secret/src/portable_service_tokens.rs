//! Token results arrive only from the worker's private root authority.

use super::lifecycle::{
    AssertRequest, Asserted, EnrollRequest, Enrolled, Presented, TokenError, Tokens,
};
use super::{Secret32, VerificationKey, MAX_CREDENTIAL, MAX_SLOTS};
use crate::fido_device::Interruption;
use crate::fido_transaction::{Error as Transaction, Status};
use crate::notebook_wire::{field, Bytes, Reader, Wire};
use std::time::Duration;

/// Root's answer to a token request that released nothing: `3f` and the
/// reason's code. The operation fails with that reason; the service, and
/// any session it holds, carry on.
pub(super) const DECLINED: u8 = 0x3f;

/// How long a token request waits for root: past root's 120-second step
/// deadline and the worker's 130-second lifetime, so root's own answer,
/// a refusal when the step expired, always arrives first.
const REPLY_TIME: Duration = Duration::from_secs(150);

/// Root's own refusal, which no token step gave: the secure attention
/// screen took the security key for another operation.
pub(super) const SUPERSEDED: u8 = 0x04;

/// Each refusal's code, which root relays without reading. A reason
/// whose text the code cannot carry comes back as its kind alone.
pub(super) fn refusal(error: &TokenError) -> u8 {
    match error {
        TokenError::Unavailable => 0x00,
        TokenError::Denied => 0x01,
        TokenError::Several => 0x02,
        TokenError::Host(_) => 0x03,
        TokenError::Failed(failed) => match failed {
            Transaction::Interrupted(Interruption::Cancelled) => 0x10,
            Transaction::Interrupted(Interruption::Expired) => 0x11,
            Transaction::Interrupted(Interruption::Closed) => 0x12,
            Transaction::Transport => 0x13,
            Transaction::Protocol(_) => 0x14,
            Transaction::PinInput => 0x15,
            Transaction::Entropy => 0x16,
            Transaction::Status(status) => match status {
                Status::CredentialExcluded => 0x20,
                Status::Denied => 0x21,
                Status::Cancelled => 0x22,
                Status::NoCredential => 0x23,
                Status::TouchTimeout => 0x24,
                Status::PinInvalid => 0x25,
                Status::PinBlocked => 0x26,
                Status::PinAuthInvalid => 0x27,
                Status::PinAuthBlocked => 0x28,
                Status::PinNotSet => 0x29,
                Status::PinRequired => 0x2a,
                Status::PinPolicy => 0x2b,
                Status::ActionTimeout => 0x2c,
                Status::Other(_) => 0x2d,
            },
        },
    }
}

/// The refusal a code names; a code that names none is the host's.
pub(super) fn refused(code: u8) -> TokenError {
    let status = |status| TokenError::Failed(Transaction::Status(status));
    match code {
        0x00 => TokenError::Unavailable,
        0x01 => TokenError::Denied,
        0x02 => TokenError::Several,
        SUPERSEDED => TokenError::Host(
            "cancelled: the secure attention screen took the security key for another operation",
        ),
        0x10 => TokenError::Failed(Transaction::Interrupted(Interruption::Cancelled)),
        0x11 => TokenError::Failed(Transaction::Interrupted(Interruption::Expired)),
        0x12 => TokenError::Failed(Transaction::Interrupted(Interruption::Closed)),
        0x13 => TokenError::Failed(Transaction::Transport),
        0x14 => TokenError::Failed(Transaction::Protocol(String::new())),
        0x15 => TokenError::Failed(Transaction::PinInput),
        0x16 => TokenError::Failed(Transaction::Entropy),
        0x20 => status(Status::CredentialExcluded),
        0x21 => status(Status::Denied),
        0x22 => status(Status::Cancelled),
        0x23 => status(Status::NoCredential),
        0x24 => status(Status::TouchTimeout),
        0x25 => status(Status::PinInvalid),
        0x26 => status(Status::PinBlocked),
        0x27 => status(Status::PinAuthInvalid),
        0x28 => status(Status::PinAuthBlocked),
        0x29 => status(Status::PinNotSet),
        0x2a => status(Status::PinRequired),
        0x2b => status(Status::PinPolicy),
        0x2c => status(Status::ActionTimeout),
        0x2d => status(Status::Other(0)),
        _ => TokenError::Host("the security key operation failed"),
    }
}

pub(super) struct Remote<'a> {
    wire: &'a mut Wire,
    failed: bool,
}
impl<'a> Remote<'a> {
    pub fn new(wire: &'a mut Wire) -> Self {
        Self {
            wire,
            failed: false,
        }
    }
    fn exchange(&mut self, request: &Bytes) -> Result<Bytes, TokenError> {
        let result = self
            .wire
            .send(&request.0)
            .and_then(|()| self.wire.receive(REPLY_TIME));
        match result {
            Ok(reply) => match reply.0.as_slice() {
                [DECLINED, code] => Err(refused(*code)),
                _ => Ok(reply),
            },
            Err(_) => {
                self.failed = true;
                Err(TokenError::Unavailable)
            }
        }
    }
}
fn invalid(_: String) -> TokenError {
    TokenError::Host("invalid private notebook token result")
}

impl Tokens for Remote<'_> {
    fn enroll(
        &mut self,
        presented: Presented<'_>,
        request: EnrollRequest<'_>,
    ) -> Result<Enrolled, TokenError> {
        let mut out = Bytes(vec![0x30, presented.purpose as u8, presented.role as u8]);
        out.extend(&request.challenge).map_err(invalid)?;
        out.extend(&request.proof_challenge).map_err(invalid)?;
        out.extend(&request.user).map_err(invalid)?;
        out.extend(&request.salt).map_err(invalid)?;
        if request.excluded.len() > MAX_SLOTS {
            return Err(TokenError::Unavailable);
        }
        out.push(u8::try_from(request.excluded.len()).map_err(|_| TokenError::Unavailable)?)
            .map_err(invalid)?;
        for credential in request.excluded {
            field(&mut out, credential).map_err(invalid)?;
        }
        let reply = self.exchange(&out)?;
        let decode = || -> Result<Enrolled, String> {
            let mut input = Reader::new(&reply.0);
            if input.byte()? != 0x30 {
                return Err("token enrollment refused".into());
            }
            let credential = input.field(MAX_CREDENTIAL)?.to_vec();
            let key = VerificationKey::from_cose(input.field(77)?)?;
            let salt = input.array()?;
            let secret = Secret32(input.array()?);
            let counter = u32::from_be_bytes(input.array()?);
            input.finish()?;
            Ok(Enrolled {
                credential,
                key,
                salt,
                secret,
                counter,
            })
        };
        let result = decode().map_err(invalid);
        self.failed |= result.is_err();
        result
    }
    fn assert(
        &mut self,
        presented: Presented<'_>,
        request: AssertRequest<'_>,
    ) -> Result<Asserted, TokenError> {
        let mut out = Bytes(vec![0x31, presented.purpose as u8, presented.role as u8]);
        field(&mut out, request.credential).map_err(invalid)?;
        field(&mut out, request.key.cose()).map_err(invalid)?;
        out.extend(request.salt).map_err(invalid)?;
        out.extend(&request.challenge).map_err(invalid)?;
        let reply = self.exchange(&out)?;
        let decode = || -> Result<Asserted, String> {
            let mut input = Reader::new(&reply.0);
            if input.byte()? != 0x31 {
                return Err("token assertion refused".into());
            }
            let secret = Secret32(input.array()?);
            let counter = u32::from_be_bytes(input.array()?);
            input.finish()?;
            Ok(Asserted { secret, counter })
        };
        let result = decode().map_err(invalid);
        self.failed |= result.is_err();
        result
    }
    fn revoked(&self) -> bool {
        self.failed
    }
}
