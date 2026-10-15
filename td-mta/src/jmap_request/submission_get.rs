//! Explicit id/emailId projection used by td-mail's send reconciliation.
use super::{push::Children, Error, Worker};
use crate::{
    ids::SubmissionId,
    sync::{DataState, DataType},
    wire,
};
use std::ops::Range;
use td_json::scan;

impl<P> Worker<'_, '_, '_, '_, '_, '_, P> {
    pub(super) fn submission_get(
        &mut self,
        arguments: Range<u64>,
        maximum: usize,
    ) -> Result<Range<u64>, Error> {
        let mut fields = Children::new(arguments, scan::Kind::Object)?;
        let (mut account, mut ids, mut properties) = (None, None, None);
        while let Some(item) = fields.next(self)? {
            match item.key.bytes() {
                b"accountId" => account = Some(item),
                b"ids" => ids = Some(item),
                b"properties" => properties = Some(item),
                _ => return Err(Error::InvalidArguments),
            }
        }
        let account = self.push_string(&account.ok_or(Error::InvalidArguments)?)?;
        let account_id = account.wire_id()?.local();
        let mut email = false;
        let properties = properties.ok_or(Error::InvalidArguments)?;
        let mut properties = Children::new(properties.range, scan::Kind::Array)?;
        while let Some(item) = properties.next(self)? {
            match self.push_string(&item)?.bytes() {
                b"id" => (),
                b"emailId" => email = true,
                _ => return Err(Error::InvalidArguments),
            }
        }
        // Unbounded enumeration and the default full status projection are
        // outside this prepared client seam. They are never silently partial.
        let ids = ids
            .filter(|item| item.kind == scan::Kind::Array)
            .ok_or(Error::InvalidArguments)?;
        let mut requested = self.get_ids::<SubmissionId>(Some(ids), maximum)?;
        if account_id != Some(self.account) {
            return Err(Error::AccountNotFound);
        }
        self.check()?;
        let identity = self.read.as_ref().ok_or(Error::State)?.identity();
        if identity.account != self.account {
            return Err(Error::State);
        }
        let state = DataState {
            account: identity.account,
            epoch: identity.epoch,
            kind: DataType::Submission,
            sequence: identity.committed_sequence,
        };
        let mut state_bytes = [0; DataState::WIRE_BYTES];
        let state = state.encode(&mut state_bytes).map_err(|_| Error::State)?;
        let start = self.retention.len();
        self.append(br#"{"accountId":"#)?;
        self.push_id(&account)?;
        self.append(br#","state":""#)?;
        self.append(state.as_bytes())?;
        self.append(br#"","list":["#)?;
        let mut first = true;
        for item in &mut requested {
            self.check()?;
            let Some(id) = item.id else {
                continue;
            };
            let Some(value) = self.read.as_mut().ok_or(Error::State)?.submission(id)? else {
                continue;
            };
            if value.view != identity || value.submission != id {
                return Err(Error::State);
            }
            if !first {
                self.append(b",")?;
            }
            first = false;
            let mut bytes = [0; wire::IdKind::WIRE_BYTES];
            let id = wire::encode_id(id, &mut bytes).map_err(|_| Error::State)?;
            self.append(br#"{"id":""#)?;
            self.append(id.as_bytes())?;
            self.append(b"\"")?;
            if email {
                let id = wire::encode_id(value.email, &mut bytes).map_err(|_| Error::State)?;
                self.append(br#","emailId":""#)?;
                self.append(id.as_bytes())?;
                self.append(b"\"")?;
            }
            self.append(b"}")?;
            item.found = true;
        }
        self.append(br#"],"notFound":["#)?;
        self.get_not_found(&requested)?;
        self.append(b"]}")?;
        self.read.as_mut().ok_or(Error::State)?.check()?;
        self.check()?;
        let end = self.retention.publish().map_err(Error::Storage)?;
        Ok(start..end)
    }
}
