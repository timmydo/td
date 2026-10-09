//! Metadata and declared-body checks within one captured account view.
use super::{BodyCheckLimits, IndexReadView, MAX_FILE_STEP_BYTES};
use crate::{
    account_checks,
    format::MAX_KEY_BYTES,
    metadata_sweep,
    ports::{self, Crypto, ReadView},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AccountCheckLimits {
    pub metadata: metadata_sweep::Limits,
    pub bodies: BodyCheckLimits,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AccountCheckError {
    Metadata(metadata_sweep::Error),
    Bodies(ports::Error),
    Reports(account_checks::Error),
}
impl std::fmt::Display for AccountCheckError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "account verification: {self:?}")
    }
}
impl std::error::Error for AccountCheckError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Metadata(e) => Some(e),
            Self::Bodies(e) => Some(e),
            Self::Reports(e) => Some(e),
        }
    }
}
impl IndexReadView<'_, '_> {
    /// Synchronous historical checks; the caller supplies trusted UTC and limits.
    pub fn verify_account<C: Crypto>(
        &mut self,
        crypto: &C,
        utc_ms: i64,
        limits: AccountCheckLimits,
        scratch: &mut [u8; MAX_FILE_STEP_BYTES],
    ) -> Result<account_checks::CompleteChecks, AccountCheckError> {
        let mut sweep = metadata_sweep::Sweep::new(self.identity(), utc_ms, limits.metadata);
        let mut key = [0; MAX_KEY_BYTES];
        while sweep
            .advance(self, &mut key, scratch)
            .map_err(AccountCheckError::Metadata)?
            != metadata_sweep::Step::Complete
        {}
        let metadata = sweep.finish().map_err(AccountCheckError::Metadata)?;
        let bodies = self
            .verify_bodies(crypto, limits.bodies, scratch)
            .map_err(AccountCheckError::Bodies)?;
        account_checks::combine(metadata, bodies).map_err(AccountCheckError::Reports)
    }
}
