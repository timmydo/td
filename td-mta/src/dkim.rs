//! Bounded observations of signatures on the original direct SMTP DATA.
use crate::spf::{Dns, DnsError, Query, Record};
use td_crypto::{verify_dkim_sha256, DkimAlgorithm};

mod canon;
mod tags;

pub const MAX_SIGNATURES: usize = 4;
const MAX_FIELDS: usize = 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Outcome {
    Pass,
    Fail,
    Permerror,
    Temperror,
}
impl Outcome {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail => "fail",
            Self::Permerror => "permerror",
            Self::Temperror => "temperror",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Error {
    Invalid,
    RemoteDns,
    Unavailable,
}
type Checked<T> = Result<T, Error>;
impl From<td_alloc::Error> for Error {
    fn from(_: td_alloc::Error) -> Self {
        Self::Unavailable
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SignatureResult {
    pub outcome: Outcome,
    pub domain: Option<String>,
    pub selector: Option<String>,
    pub algorithm: Option<DkimAlgorithm>,
    pub key_bits: Option<u16>,
}

#[derive(Clone, Default, Debug, Eq, PartialEq)]
pub struct Report {
    pub signatures: Vec<SignatureResult>,
    /// More signatures may exist, or limits prevented examining all of them.
    pub incomplete: bool,
}
impl Report {
    pub fn unavailable() -> Self {
        Self {
            signatures: Vec::new(),
            incomplete: true,
        }
    }
}

fn alive(current: &dyn Fn() -> bool) -> Checked<()> {
    if current() {
        Ok(())
    } else {
        Err(Error::Unavailable)
    }
}

struct BodyHash {
    relaxed: bool,
    length: Option<u64>,
    digest: [u8; 32],
}

struct Evaluation<'a, D> {
    headers: canon::Headers<'a>,
    bodies: Vec<BodyHash>,
    dns: &'a mut D,
    now: u64,
    current: &'a dyn Fn() -> bool,
}

impl<D: Dns> Evaluation<'_, D> {
    fn body(&mut self, signature: &tags::Signature<'_>) -> Checked<[u8; 32]> {
        let relaxed = signature.relaxed_body;
        let length = signature.length;
        if let Some(cache) = self
            .bodies
            .iter()
            .find(|old| old.relaxed == relaxed && old.length == length)
        {
            return Ok(cache.digest);
        }
        let digest = canon::body(self.headers.body, relaxed, length, self.current)?;
        td_alloc::push(
            &mut self.bodies,
            BodyHash {
                relaxed,
                length,
                digest,
            },
        )?;
        Ok(digest)
    }

    fn verify(
        &mut self,
        field: td_mime::headers::Field,
        observation: &mut SignatureResult,
    ) -> Checked<Outcome> {
        alive(self.current)?;
        let tags = tags::Tags::read(self.headers.value(field)?)?;
        observation.domain = tags.domain().map(td_alloc::string).transpose()?;
        observation.selector = tags.selector().map(td_alloc::string).transpose()?;
        observation.algorithm = tags.algorithm();
        let signature = tags::Signature::read(&tags, self.now)?;
        let name = td_alloc::format(format_args!(
            "{}._domainkey.{}",
            signature.selector, signature.domain
        ))
        .map_err(|_| Error::Unavailable)?;
        if name.len() > 253 {
            return Err(Error::Invalid);
        }
        alive(self.current)?;
        let records = self
            .dns
            .lookup(&name, Query::Txt)
            .map_err(|err| match err {
                DnsError::Temporary => Error::RemoteDns,
                DnsError::Unavailable => Error::Unavailable,
                DnsError::Permanent => Error::Invalid,
            })?
            .ok_or(Error::Invalid)?;
        let mut keys = records.into_iter().filter_map(|record| match record {
            Record::Text(value) => Some(value),
            _ => None,
        });
        let key = keys.next().ok_or(Error::Invalid)?;
        // Multiple TXT RRs at one selector do not give us an unambiguous key.
        if keys.next().is_some() {
            return Err(Error::Invalid);
        }
        let key = tags::key(&key, &signature)?;
        let digest = canon::header_hash(
            &self.headers,
            field,
            &signature.signed,
            signature.relaxed_header,
            signature.skip.start..signature.skip.end,
            self.current,
        )?;
        alive(self.current)?;
        let result = verify_dkim_sha256(signature.alg, &key, &digest, &signature.signature)
            .map_err(|err| match err {
                td_crypto::Error::Invalid => Error::Invalid,
                _ => Error::Unavailable,
            })?;
        observation.key_bits = Some(result.key_bits);
        alive(self.current)?;
        if !result.verified {
            return Ok(Outcome::Fail);
        }
        let actual = self.body(&signature)?;
        alive(self.current)?;
        Ok(if actual == signature.body_hash {
            Outcome::Pass
        } else {
            Outcome::Fail
        })
    }
}

pub(crate) fn evaluate(
    raw: &[u8],
    header_limit: usize,
    dns: &mut impl Dns,
    now_unix: u64,
    current: &dyn Fn() -> bool,
) -> Report {
    let Ok(headers) = canon::Headers::read(raw, header_limit, current) else {
        return Report::unavailable();
    };
    let mut evaluation = Evaluation {
        headers,
        bodies: Vec::new(),
        dns,
        now: now_unix,
        current,
    };
    let mut report = Report::default();
    for index in 0..evaluation.headers.fields.len() {
        let Some(&field) = evaluation.headers.fields.get(index) else {
            return Report::unavailable();
        };
        if !evaluation
            .headers
            .name(field)
            .is_ok_and(|s| s.eq_ignore_ascii_case(b"DKIM-Signature"))
        {
            continue;
        }
        if report.signatures.len() == MAX_SIGNATURES {
            report.incomplete = true;
            break;
        }
        let mut item = SignatureResult {
            outcome: Outcome::Temperror,
            domain: None,
            selector: None,
            algorithm: None,
            key_bits: None,
        };
        let result = evaluation.verify(field, &mut item);
        let timely = current();
        item.outcome = match (timely, result) {
            (true, Ok(outcome)) => outcome,
            (true, Err(Error::Invalid)) => Outcome::Permerror,
            (true, Err(Error::RemoteDns)) => Outcome::Temperror,
            (false, _) | (_, Err(Error::Unavailable)) => {
                report.incomplete = true;
                Outcome::Temperror
            }
        };
        if td_alloc::push(&mut report.signatures, item).is_err() {
            report.incomplete = true;
            break;
        }
        if !timely {
            break;
        }
    }
    report.incomplete |= !current();
    report
}

#[cfg(test)]
#[path = "../tests/support/dkim_engine.rs"]
mod tests;
