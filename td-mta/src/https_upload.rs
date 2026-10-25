//! Bounded upload transfer on a dedicated worker; publication stays in the store.
use crate::{
    bounded::TextBuffer,
    http_request::Framing,
    ids::{AccountId, DeviceId},
    ports::{Deadline, Error, Transport},
    smtp_receiving::Control,
    wire,
};

pub struct Request<'a> {
    pub account: AccountId,
    pub device: DeviceId,
    pub credentials: &'a crate::http_auth::BasicCredentials,
    pub framing: Framing,
    pub content_type: &'a str,
    pub prefix: &'a [u8],
    pub deadline: Deadline,
    pub body_deadline: Deadline,
    pub expect_continue: bool,
}
pub struct Response {
    bytes: [u8; 4096],
    length: usize,
    pub stop: bool,
    uncertain: bool,
}
impl Response {
    /// Publication may have committed. The HTTP owner must close without a
    /// final response; the backend retains the operation for settlement.
    pub(crate) fn uncertain() -> Self {
        Self {
            bytes: [0; 4096],
            length: 0,
            stop: false,
            uncertain: true,
        }
    }
    pub(crate) fn is_uncertain(&self) -> bool {
        self.uncertain
    }
    pub(crate) fn created(
        account: AccountId,
        blob: crate::ids::BlobId,
        length: u64,
        content_type: &str,
        stop: bool,
    ) -> Result<Self, Error> {
        let mut response = Self {
            bytes: [0; 4096],
            length: 0,
            stop,
            uncertain: false,
        };
        let mut account_bytes = [0; wire::IdKind::WIRE_BYTES];
        let mut blob_bytes = [0; wire::IdKind::WIRE_BYTES];
        let account =
            wire::encode_id(account, &mut account_bytes).map_err(|_| Error::WriterStopped)?;
        let blob = wire::encode_id(blob, &mut blob_bytes).map_err(|_| Error::WriterStopped)?;
        let mut out = TextBuffer::new(&mut response.bytes);
        out.format(format_args!(
            "{{\"accountId\":\"{account}\",\"blobId\":\"{blob}\",\"size\":{length},\"type\":"
        ))
        .map_err(|_| Error::WriterStopped)?;
        crate::jmap_session::quoted(&mut out, content_type).map_err(|_| Error::WriterStopped)?;
        out.append("}").map_err(|_| Error::WriterStopped)?;
        response.length = out.len();
        Ok(response)
    }
    #[cfg(test)]
    pub(crate) fn stopped_for_test() -> Self {
        let mut bytes = [0; 4096];
        if let Some(prefix) = bytes.get_mut(..2) {
            prefix.copy_from_slice(b"{}");
        }
        Self {
            bytes,
            length: 2,
            stop: true,
            uncertain: false,
        }
    }
    pub fn bytes(&self) -> Result<&[u8], Error> {
        if self.uncertain {
            return Err(Error::Conflict);
        }
        self.bytes.get(..self.length).ok_or(Error::Invalid)
    }
}
/// Authenticated HTTPS operations. Transfer methods run on the body worker.
/// Request credentials borrow the original bounded owner through the call;
/// parsing and the initial device observation confer no later store authority.
pub trait Backend: Sync {
    fn account(&self) -> AccountId;
    /// Construct state only on its owning body worker; never on the reactor.
    fn api_worker(&self) -> Option<Box<dyn crate::https_api::Worker + '_>> {
        None
    }
    fn session(&self) -> Option<&crate::jmap_session::Session> {
        None
    }
    /// Called by the network reactor: must not block or acquire a store view.
    fn event_snapshot(
        &self,
        _device: DeviceId,
        _deadline: Deadline,
    ) -> Result<crate::jmap_events::Snapshot, Error> {
        Err(Error::NotFound)
    }
    fn receive(
        &self,
        request: Request<'_>,
        transport: &mut dyn Transport,
        control: &Control,
    ) -> Result<Response, Error>;
    fn api(
        &self,
        _request: crate::https_api::Request<'_>,
        _transport: &mut dyn Transport,
        _control: &Control,
    ) -> Result<(), crate::https_download::Failure> {
        Err(crate::https_download::Failure::Before(Error::NotFound))
    }

    fn download(
        &self,
        _request: crate::https_download::Request<'_>,
        _transport: &mut dyn Transport,
        _control: &Control,
    ) -> Result<(), crate::https_download::Failure> {
        Err(crate::https_download::Failure::Before(Error::NotFound))
    }
}
