//! td-mail draft input and publication on the authenticated HTTPS body worker.
use crate::{
    admission::work::Meter,
    draft_input,
    https_upload::Service,
    jmap_request::dispatch::{DraftCreator, DraftError, DraftInput},
    ports,
    store_fs::{DraftAuthorization, DraftCompletion, DraftRequest},
};

impl<P: DraftAuthorization + Sync> DraftCreator for Service<'_, '_, '_, P> {
    fn create_draft(
        &self,
        input: DraftInput<'_>,
        meter: &mut Meter,
    ) -> Result<DraftCompletion, DraftError> {
        if input.account != self.account {
            return Err(DraftError::Store(ports::Error::Forbidden.into()));
        }
        let draft = draft_input::parse(
            input.value,
            self.resources.limits().json_bytes,
            self.clock,
            input.deadline,
            meter,
        )
        .map_err(DraftError::Input)?;
        let mut entropy = td_crypto::SystemEntropy::try_new()
            .map_err(ports::Error::from)
            .map_err(|error| DraftError::Store(error.into()))?;
        self.coordinator
            .create_text_draft(
                &td_crypto::Provider,
                &mut entropy,
                self.spool,
                DraftRequest {
                    account: input.account,
                    device: input.device,
                    mailbox: draft.mailbox,
                    expected: input.expected,
                    maximum: self.maximum,
                    deadline: input.deadline,
                },
                &draft.message(),
                meter,
            )
            .map_err(DraftError::Store)
    }
}
