//! Retained Thread/get over one original snapshot.
use super::{push::Children, Error, Worker};
use crate::{
    ids::ThreadId,
    ports::MAX_THREAD_EMAILS,
    sync::{DataState, DataType},
    wire,
};
use std::ops::Range;
use td_json::scan;
enum View<'v> {
    Postgres(&'v mut dyn super::MailRead),
}
impl View<'_> {
    fn identity(&self) -> crate::ports::ViewIdentity {
        match self {
            Self::Postgres(view) => view.identity(),
        }
    }
    fn check(&mut self) -> Result<(), Error> {
        match self {
            Self::Postgres(view) => view.check(),
        }
    }
    fn thread_emails(
        &mut self,
        thread: ThreadId,
        after: Option<crate::ports::ThreadEmail>,
        maximum: usize,
    ) -> Result<Option<crate::ports::ThreadEmails>, Error> {
        match self {
            Self::Postgres(view) => view.thread_emails(thread, after, maximum),
        }
    }
}
impl Worker<'_, '_, '_, '_, '_> {
    pub(super) fn thread_get(
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
        let mut members = true;
        if let Some(properties) = properties.filter(|item| item.kind != scan::Kind::Null) {
            members = false;
            let mut children = Children::new(properties.range, scan::Kind::Array)?;
            while let Some(item) = children.next(self)? {
                match self.push_string(&item)?.bytes() {
                    b"id" => {}
                    b"emailIds" => members = true,
                    _ => return Err(Error::InvalidArguments),
                }
            }
        }
        let all = ids
            .as_ref()
            .is_none_or(|item| item.kind == scan::Kind::Null);
        let mut requested =
            self.get_ids::<ThreadId>(ids.filter(|item| item.kind != scan::Kind::Null), maximum)?;
        if account_id != Some(self.account) {
            return Err(Error::AccountNotFound);
        }
        self.check()?;
        if all && self.read.is_some() {
            return Err(Error::InvalidArguments);
        }
        let mut read = self.read.take();
        let result = (|| {
            let mut view = View::Postgres(super::reborrow_read(
                &mut **read.as_mut().ok_or(Error::State)?,
            ));

            let identity = view.identity();
            let mut state_bytes = [0; DataState::WIRE_BYTES];
            let state = DataState {
                account: identity.account,
                epoch: identity.epoch,
                kind: DataType::Thread,
                sequence: identity.committed_sequence,
            }
            .encode(&mut state_bytes)
            .map_err(|_| Error::State)?;
            let start = self.retention.len();
            self.append(br#"{"accountId":"#)?;
            self.push_id(&account)?;
            self.append(br#","state":""#)?;
            self.append(state.as_bytes())?;
            self.append(br#"","list":["#)?;
            let mut first = true;
            let mut encoded = [0; wire::IdKind::WIRE_BYTES];
            let window = if members { MAX_THREAD_EMAILS } else { 1 };
            // Stored Thread values are empty; no row-value scratch is needed.
            for item in &mut requested {
                self.check()?;
                let Some(id) = item.id else {
                    continue;
                };
                let Some(mut page) = view.thread_emails(id, None, window)? else {
                    continue;
                };
                // Stored empty rows are not visible JMAP threads.
                if page.emails.is_empty() {
                    if page.has_more {
                        return Err(Error::State);
                    }
                    continue;
                }
                if !first {
                    self.append(b",")?;
                }
                first = false;
                self.append(br#"{"id":""#)?;
                self.append(
                    wire::encode_id(id, &mut encoded)
                        .map_err(|_| Error::State)?
                        .as_bytes(),
                )?;
                self.append(b"\"")?;
                if members {
                    self.append(br#","emailIds":["#)?;
                    let mut first_email = true;
                    loop {
                        for email in &page.emails {
                            self.check()?;
                            if !first_email {
                                self.append(b",")?;
                            }
                            first_email = false;
                            self.append(b"\"")?;
                            self.append(
                                wire::encode_id(email.email, &mut encoded)
                                    .map_err(|_| Error::State)?
                                    .as_bytes(),
                            )?;
                            self.append(b"\"")?;
                        }
                        if !page.has_more {
                            break;
                        }
                        let after = page.emails.last().copied().ok_or(Error::State)?;
                        self.check()?;
                        page = view
                            .thread_emails(id, Some(after), window)?
                            .ok_or(Error::State)?;
                        if page.emails.is_empty() {
                            return Err(Error::State);
                        }
                    }
                    self.append(b"]")?;
                }
                self.append(b"}")?;
                item.found = true;
            }
            self.append(br#"],"notFound":["#)?;
            self.get_not_found(&requested)?;
            self.append(b"]}")?;
            view.check()?;
            self.check()?;
            let end = self.retention.publish().map_err(Error::Storage)?;
            Ok(start..end)
        })();
        self.read = read;
        result
    }
}
