//! ACME's bounded buffered facade over the shared streaming HTTPS client.
use super::Error;
use crate::outbound_http;
use std::{
    path::Path,
    time::{Duration, Instant},
};
use td_crypto::TrustStore;
pub struct Response {
    pub status: u16,
    pub nonce: Option<String>,
    pub location: Option<String>,
    pub retry_after: Option<String>,
    pub content_type: Option<String>,
    pub body: Vec<u8>,
}
pub trait Exchange {
    fn exchange(&mut self, method: &str, url: &str, body: &[u8]) -> Result<Response, Error>;
}
pub struct Https<'a> {
    pub executable: &'a Path,
    pub directory: &'a str,
    pub trust: &'a TrustStore,
    pub deadline: Instant,
}
impl Exchange for Https<'_> {
    fn exchange(&mut self, method: &str, url: &str, body: &[u8]) -> Result<Response, Error> {
        if body.len() > 32768 {
            return Err(Error::Invalid);
        }
        let mut output = Vec::new();
        let response = outbound_http::Client {
            executable: self.executable,
            origin: self.directory,
            trust: self.trust,
            deadline: self.deadline.min(Instant::now() + Duration::from_secs(15)),
        }
        .exchange(
            outbound_http::Request {
                method,
                url,
                body,
                content_type: "application/jose+json",
                accept: "application/json, application/pem-certificate-chain",
                authorization: None,
                body_limit: 65536,
            },
            |part| {
                output
                    .try_reserve(part.len())
                    .map_err(|_| outbound_http::Error::Capacity)?;
                output.extend_from_slice(part);
                Ok(())
            },
        )
        .map_err(|e| match e {
            outbound_http::Error::Invalid => Error::Invalid,
            outbound_http::Error::Capacity => Error::Capacity,
            outbound_http::Error::Crypto => Error::Crypto,
            outbound_http::Error::Protocol | outbound_http::Error::Status(_) => Error::Protocol,
            outbound_http::Error::Deadline => Error::Deadline,
            outbound_http::Error::Transport | outbound_http::Error::Sink => Error::Transport,
        })?;
        Ok(Response {
            status: response.status,
            nonce: response.nonce,
            location: response.location,
            retry_after: response.retry_after,
            content_type: response.content_type,
            body: output,
        })
    }
}
