//! Token results arrive only from the worker's private root authority.

use super::lifecycle::{
    AssertRequest, Asserted, EnrollRequest, Enrolled, Presented, TokenError, Tokens,
};
use super::{Secret32, VerificationKey, MAX_CREDENTIAL, MAX_SLOTS};
use crate::notebook_wire::{field, Bytes, Reader, Wire};
use std::time::Duration;

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
            .and_then(|()| self.wire.receive(Duration::from_secs(120)));
        if result.is_err() {
            self.failed = true;
        }
        result.map_err(|_| TokenError::Unavailable)
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
