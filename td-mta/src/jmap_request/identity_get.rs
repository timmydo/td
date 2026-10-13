//! The default visible Identity/get projection consumed by td-mail.
use super::{get_ids::Requested, mailbox_get::storage, push::Children, Error, Worker};
use crate::{ids::IdentityId, ports::ReadView, sync::IdentityState};
use std::ops::Range;
use td_json::scan;

impl<P> Worker<'_, '_, '_, '_, '_, '_, P> {
    pub(super) fn identity_get(
        &mut self,
        arguments: Range<u64>,
        maximum: usize,
    ) -> Result<Range<u64>, Error> {
        let mut fields = Children::new(arguments, scan::Kind::Object)?;
        let (mut account, mut ids) = (None, None);
        while let Some(item) = fields.next(self)? {
            match item.key.bytes() {
                b"accountId" => account = Some(item),
                b"ids" => ids = Some(item),
                b"properties" if item.kind == scan::Kind::Null => {}
                _ => return Err(Error::InvalidArguments),
            }
        }
        let account = self.push_string(&account.ok_or(Error::InvalidArguments)?)?;
        let account_id = account.wire_id()?.local();
        let all = ids
            .as_ref()
            .is_none_or(|item| item.kind == scan::Kind::Null);
        let mut requested =
            self.get_ids::<IdentityId>(ids.filter(|item| item.kind != scan::Kind::Null), maximum)?;
        if account_id != Some(self.account) {
            return Err(Error::AccountNotFound);
        }
        let catalog = self.identities.ok_or(Error::State)?;
        if catalog.account() != self.account {
            return Err(Error::State);
        }
        self.check()?;
        let mut view = self
            .retention
            .method_view(self.account, self.execution)
            .map_err(storage)?;
        let state = catalog.state(view.identity()).map_err(|_| Error::State)?;
        if all {
            if catalog.ids().len() > maximum {
                return Err(Error::RequestTooLarge);
            }
            for id in catalog.ids() {
                self.check()?;
                requested.push(Requested {
                    name: None,
                    id: Some(id),
                    found: false,
                });
            }
        }
        let mut state_bytes = [0; IdentityState::WIRE_BYTES];
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
            let Some(id) = item.id else { continue };
            let Some(object) = catalog.object(id).map_err(|_| Error::State)? else {
                continue;
            };
            if !first {
                self.append(b",")?;
            }
            first = false;
            for chunk in object.chunks(4096) {
                self.append(chunk)?;
            }
            item.found = true;
        }
        self.append(br#"],"notFound":["#)?;
        self.get_not_found(&requested)?;
        self.append(b"]}")?;
        view.check_snapshot().map_err(storage)?;
        self.check()?;
        let end = self.retention.publish().map_err(Error::Storage)?;
        Ok(start..end)
    }
}

#[cfg(test)]
#[path = "identity_get_tests.rs"]
mod tests;
