//! Direct-MX sender authentication results for one accepted transaction. The
//! stored Authentication-Results field and the per-Email rows both derive
//! from one validated value, so they state the same facts.
use crate::{dkim, dmarc, ports::Error, spf};
use std::fmt::Write as _;

const NAME_BYTES: usize = 253;
const HEAD: &str = "Authentication-Results: ";
const SPF_BYTES: usize = ";\r\n\tspf=permerror smtp.mailfrom=".len() + NAME_BYTES;
const DKIM_ROW_BYTES: usize = ";\r\n\tdkim=permerror (65535-bit key) header.d=".len()
    + NAME_BYTES
    + " header.s=".len()
    + NAME_BYTES
    + " header.a=ed25519-sha256".len();
const DKIM_INCOMPLETE: &str = ";\r\n\tdkim=temperror (verification incomplete)";
const DMARC_BYTES: usize =
    ";\r\n\tdmarc=permerror (policy=quarantine aspf=s adkim=s aligned=dkim policy-domain=)".len()
        + NAME_BYTES
        + " header.from=".len()
        + NAME_BYTES;
// The smallest padding comment, then the field's CRLF.
const FIXED_BYTES: usize = HEAD.len()
    + SPF_BYTES
    + dkim::MAX_SIGNATURES * DKIM_ROW_BYTES
    + DKIM_INCOMPLETE.len()
    + DMARC_BYTES
    + 5
    + 2;

/// One field length for every outcome, so a verdict never changes header
/// admission or the quota charge of the stored message.
pub(crate) fn field_bytes(authserv_id: &str) -> Result<usize, Error> {
    FIXED_BYTES
        .checked_add(authserv_id.len())
        .ok_or(Error::Capacity)
}

/// What the SMTP transaction observed, before validation for storage.
#[derive(Debug, Eq, PartialEq)]
pub struct Observed {
    pub spf: spf::Outcome,
    pub dkim: dkim::Report,
    pub dmarc: dmarc::Report,
}

/// Validated results; only this crate builds or reads one.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Results {
    pub(crate) spf: spf::Outcome,
    /// The MAIL FROM domain SPF evaluated; the HELO domain for a null path.
    pub(crate) spf_domain: Option<String>,
    /// SPF evaluated the HELO identity because the reverse path was null.
    pub(crate) spf_helo: bool,
    pub(crate) dkim: dkim::Report,
    pub(crate) dmarc: dmarc::Report,
}

fn trace_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= NAME_BYTES
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

fn copy(value: Option<&str>) -> Result<Option<String>, Error> {
    value
        .map(td_alloc::string)
        .transpose()
        .map_err(|_| Error::Capacity)
}

impl Results {
    /// A name the field cannot state withdraws that whole DKIM or DMARC
    /// observation, as an incomplete DKIM report or a DMARC temperror. A
    /// DMARC pass resting on withdrawn DKIM is withdrawn with it.
    pub(crate) fn new(observed: Observed, ehlo: &str, reverse_path: &str) -> Result<Self, Error> {
        let spf_domain = copy(
            spf::mailfrom_identity(ehlo, reverse_path)
                .map(|(_, domain)| domain)
                .filter(|domain| trace_name(domain)),
        )?;
        let Observed {
            spf,
            mut dkim,
            mut dmarc,
        } = observed;
        let readable = dkim.signatures.len() <= dkim::MAX_SIGNATURES
            && dkim.signatures.iter().all(|row| {
                [row.domain.as_deref(), row.selector.as_deref()]
                    .into_iter()
                    .flatten()
                    .all(trace_name)
            });
        if !readable {
            dkim = dkim::Report::unavailable();
            if dmarc.aligned == Some(dmarc::Identifier::Dkim) {
                dmarc = dmarc::Report::unavailable();
            }
        }
        let named = [dmarc.author.as_deref(), dmarc.policy_domain.as_deref()]
            .into_iter()
            .flatten()
            .all(|name| trace_name(name) && crate::config::values::certificate_name(name).is_ok());
        if !named {
            dmarc = dmarc::Report::unavailable();
        }
        Ok(Self {
            spf,
            spf_domain,
            spf_helo: reverse_path.is_empty(),
            dkim,
            dmarc,
        })
    }

    /// Every name is one the field could state, so it needs no JSON escape.
    pub(crate) fn readable(&self) -> bool {
        let dkim = self.dkim.signatures.len() <= dkim::MAX_SIGNATURES
            && self.dkim.signatures.iter().all(|row| {
                [row.domain.as_deref(), row.selector.as_deref()]
                    .into_iter()
                    .flatten()
                    .all(trace_name)
            });
        dkim && [
            self.spf_domain.as_deref(),
            self.dmarc.author.as_deref(),
            self.dmarc.policy_domain.as_deref(),
        ]
        .into_iter()
        .flatten()
        .all(trace_name)
    }

    /// An independent copy for another recipient account's Email.
    pub(crate) fn try_copy(&self) -> Result<Self, Error> {
        let mut signatures =
            td_alloc::with_capacity(self.dkim.signatures.len()).map_err(|_| Error::Capacity)?;
        for row in &self.dkim.signatures {
            td_alloc::push(
                &mut signatures,
                dkim::SignatureResult {
                    outcome: row.outcome,
                    domain: copy(row.domain.as_deref())?,
                    selector: copy(row.selector.as_deref())?,
                    algorithm: row.algorithm,
                    key_bits: row.key_bits,
                },
            )
            .map_err(|_| Error::Capacity)?;
        }
        Ok(Self {
            spf: self.spf,
            spf_domain: copy(self.spf_domain.as_deref())?,
            spf_helo: self.spf_helo,
            dkim: dkim::Report {
                signatures,
                incomplete: self.dkim.incomplete,
            },
            dmarc: dmarc::Report {
                outcome: self.dmarc.outcome,
                author: copy(self.dmarc.author.as_deref())?,
                policy_domain: copy(self.dmarc.policy_domain.as_deref())?,
                policy: self.dmarc.policy,
                aligned: self.dmarc.aligned,
            },
        })
    }

    /// RFC 8601 Authentication-Results naming this receiver, padded with
    /// folded comments to `field_bytes`.
    pub(crate) fn write_field(&self, output: &mut String, authserv_id: &str) -> Result<(), Error> {
        let start = output.len();
        let length = field_bytes(authserv_id)?;
        write_methods(self, output, authserv_id).map_err(|_| Error::Capacity)?;
        // Folded comments keep every physical line below 998 bytes.
        let used = output.len().checked_sub(start).ok_or(Error::Invalid)?;
        let mut remaining = length
            .checked_sub(2)
            .and_then(|n| n.checked_sub(used))
            .ok_or(Error::Capacity)?;
        while remaining > 0 {
            let fold = if remaining > 996 {
                remaining.checked_sub(5).ok_or(Error::Invalid)?.min(996)
            } else {
                remaining
            };
            let spaces = fold.checked_sub(5).ok_or(Error::Invalid)?;
            write!(output, "\r\n\t({empty:spaces$})", empty = "").map_err(|_| Error::Capacity)?;
            remaining = remaining.checked_sub(fold).ok_or(Error::Invalid)?;
        }
        output.write_str("\r\n").map_err(|_| Error::Capacity)
    }
}

fn write_methods(results: &Results, output: &mut String, id: &str) -> std::fmt::Result {
    write!(output, "{HEAD}{id};\r\n\tspf={}", results.spf.as_str())?;
    if let Some(domain) = &results.spf_domain {
        // RFC 8601 section 2.7.2 names the HELO identity smtp.helo.
        let property = if results.spf_helo { "helo" } else { "mailfrom" };
        write!(output, " smtp.{property}={domain}")?;
    }
    for row in &results.dkim.signatures {
        write!(output, ";\r\n\tdkim={}", row.outcome.as_str())?;
        if let Some(bits) = row.key_bits {
            write!(output, " ({bits}-bit key)")?;
        }
        if let Some(domain) = &row.domain {
            write!(output, " header.d={domain}")?;
        }
        if let Some(selector) = &row.selector {
            write!(output, " header.s={selector}")?;
        }
        if let Some(algorithm) = row.algorithm {
            write!(output, " header.a={}", algorithm.as_str())?;
        }
    }
    if results.dkim.incomplete {
        output.write_str(DKIM_INCOMPLETE)?;
    } else if results.dkim.signatures.is_empty() {
        output.write_str(";\r\n\tdkim=none")?;
    }
    let dmarc = &results.dmarc;
    write!(output, ";\r\n\tdmarc={}", dmarc.outcome.as_str())?;
    if let Some(policy) = dmarc.policy {
        let mode = |strict| if strict { "s" } else { "r" };
        write!(
            output,
            " (policy={} aspf={} adkim={}",
            policy.disposition.as_str(),
            mode(policy.strict_spf),
            mode(policy.strict_dkim)
        )?;
        if let Some(aligned) = dmarc.aligned {
            write!(output, " aligned={}", aligned.as_str())?;
        }
        if let Some(domain) = &dmarc.policy_domain {
            write!(output, " policy-domain={domain}")?;
        }
        output.write_char(')')?;
    }
    if let Some(author) = &dmarc.author {
        write!(output, " header.from={author}")?;
    }
    Ok(())
}

/// RFC 8601 section 5: a field naming this receiver did not come from it.
/// A field whose authserv-id cannot be read is treated the same way.
pub(crate) fn claims_receiver(value: &[u8], authserv_id: &str) -> bool {
    authserv_id_matches(value, authserv_id).unwrap_or(true)
}

struct Visits(usize);
impl td_header::Work for Visits {
    type Error = ();
    fn charge(&mut self, charge: td_header::Charge) -> Result<(), ()> {
        let used = usize::try_from(charge.visits.saturating_add(charge.records)).map_err(|_| ())?;
        self.0 = self.0.checked_sub(used).ok_or(())?;
        Ok(())
    }
}

fn authserv_id_matches(value: &[u8], authserv_id: &str) -> Option<bool> {
    let mut work = Visits(value.len().checked_mul(4)?.checked_add(64)?);
    let mut cfws = td_header::cfws::Cursor::<()>::new(value, 0);
    let start = loop {
        match cfws.poll(&mut work).ok()? {
            td_header::cfws::Status::Complete(end) => break end.position,
            td_header::cfws::Status::Comment(_) | td_header::cfws::Status::Yield => {}
        }
    };
    let rest = value.get(start..)?;
    if rest.first() == Some(&b'"') {
        let mut quoted = td_header::delimited::Cursor::<()>::new(
            value,
            start,
            td_header::delimited::Kind::QuotedString,
        );
        let extent = loop {
            match quoted.poll(&mut work).ok()? {
                td_header::delimited::Status::Complete(extent) => break extent,
                td_header::delimited::Status::Yield => {}
            }
        };
        let content = value.get(extent.start.checked_add(1)?..extent.end.checked_sub(1)?)?;
        let mut expected = authserv_id.bytes();
        let mut escaped = false;
        for &byte in content {
            if !escaped && byte == b'\\' {
                escaped = true;
                continue;
            }
            escaped = false;
            if !expected
                .next()
                .is_some_and(|wanted| wanted.eq_ignore_ascii_case(&byte))
            {
                return Some(false);
            }
        }
        return Some(expected.next().is_none());
    }
    let token = rest
        .iter()
        .position(|&byte| !td_header::mime_token_octet(byte))
        .map_or(rest, |end| rest.get(..end).unwrap_or_default());
    if token.is_empty() {
        return None;
    }
    Some(token.eq_ignore_ascii_case(authserv_id.as_bytes()))
}

/// Stored vocabulary, read back from PostgreSQL text columns.
pub(crate) mod stored {
    use crate::{dkim, dmarc, spf};
    use td_crypto::DkimAlgorithm;

    fn find<T: Copy>(values: &[T], name: fn(T) -> &'static str, text: &str) -> Option<T> {
        values.iter().copied().find(|value| name(*value) == text)
    }
    pub(crate) fn spf(text: &str) -> Option<spf::Outcome> {
        use spf::Outcome::*;
        find(
            &[None, Neutral, Pass, Fail, Softfail, Temperror, Permerror],
            spf::Outcome::as_str,
            text,
        )
    }
    pub(crate) fn dkim(text: &str) -> Option<dkim::Outcome> {
        use dkim::Outcome::*;
        find(
            &[Pass, Fail, Permerror, Temperror],
            dkim::Outcome::as_str,
            text,
        )
    }
    pub(crate) fn algorithm(text: &str) -> Option<DkimAlgorithm> {
        find(
            &[DkimAlgorithm::Rsa, DkimAlgorithm::Ed25519],
            DkimAlgorithm::as_str,
            text,
        )
    }
    pub(crate) fn dmarc(text: &str) -> Option<dmarc::Outcome> {
        use dmarc::Outcome::*;
        find(
            &[None, Pass, Fail, Permerror, Temperror],
            dmarc::Outcome::as_str,
            text,
        )
    }
    pub(crate) fn disposition(text: &str) -> Option<dmarc::Disposition> {
        use dmarc::Disposition::*;
        find(
            &[None, Quarantine, Reject],
            dmarc::Disposition::as_str,
            text,
        )
    }
    pub(crate) fn identifier(text: &str) -> Option<dmarc::Identifier> {
        use dmarc::Identifier::*;
        find(&[Spf, Dkim], dmarc::Identifier::as_str, text)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn copies_are_independent_and_equal() {
        let observed = Observed {
            spf: spf::Outcome::Softfail,
            dkim: dkim::Report {
                signatures: vec![dkim::SignatureResult {
                    outcome: dkim::Outcome::Fail,
                    domain: Some("example.test".into()),
                    selector: Some("s1".into()),
                    algorithm: Some(td_crypto::DkimAlgorithm::Rsa),
                    key_bits: Some(1024),
                }],
                incomplete: false,
            },
            dmarc: dmarc::Report {
                outcome: dmarc::Outcome::Fail,
                author: Some("example.test".into()),
                policy_domain: Some("example.test".into()),
                policy: Some(dmarc::Requested {
                    disposition: dmarc::Disposition::Reject,
                    strict_spf: false,
                    strict_dkim: false,
                }),
                aligned: None,
            },
        };
        let results = Results::new(observed, "helo.test", "a@Mail.Example.test").unwrap();
        assert_eq!(results.spf_domain.as_deref(), Some("Mail.Example.test"));
        assert_eq!(results.try_copy().unwrap(), results);
        let null = Results::new(
            Observed {
                spf: spf::Outcome::None,
                dkim: dkim::Report::default(),
                dmarc: dmarc::Report::unavailable(),
            },
            "helo.test",
            "",
        )
        .unwrap();
        assert_eq!(null.spf_domain.as_deref(), Some("helo.test"));
    }

    #[test]
    fn authserv_id_is_a_token_or_quoted_string_after_cfws() {
        for (value, claims) in [
            (&b" mx.example.test; spf=pass"[..], true),
            (b"MX.EXAMPLE.TEST", true),
            (b" (a (b) \\) c) \"mx.example.t\\est\" 1; none", true),
            (b" mx.example.test.; none", false),
            (b" \"mx.example.test \"; none", false),
            (b" other.example.test; none", false),
            (b" ; none", true),
            (b" (open", true),
            (b" \"open", true),
            (b"", true),
        ] {
            assert_eq!(
                claims_receiver(value, "mx.example.test"),
                claims,
                "{}",
                String::from_utf8_lossy(value)
            );
        }
    }

    #[test]
    fn stored_words_round_trip_every_variant() {
        use super::stored;
        use crate::{dkim, dmarc, spf};
        // Each exhaustive match stops compiling when a variant is added, so
        // the stored vocabulary is revisited with it.
        for value in [
            spf::Outcome::None,
            spf::Outcome::Neutral,
            spf::Outcome::Pass,
            spf::Outcome::Fail,
            spf::Outcome::Softfail,
            spf::Outcome::Temperror,
            spf::Outcome::Permerror,
        ] {
            match value {
                spf::Outcome::None
                | spf::Outcome::Neutral
                | spf::Outcome::Pass
                | spf::Outcome::Fail
                | spf::Outcome::Softfail
                | spf::Outcome::Temperror
                | spf::Outcome::Permerror => {}
            }
            assert_eq!(stored::spf(value.as_str()), Some(value));
        }
        for value in [
            dkim::Outcome::Pass,
            dkim::Outcome::Fail,
            dkim::Outcome::Permerror,
            dkim::Outcome::Temperror,
        ] {
            match value {
                dkim::Outcome::Pass
                | dkim::Outcome::Fail
                | dkim::Outcome::Permerror
                | dkim::Outcome::Temperror => {}
            }
            assert_eq!(stored::dkim(value.as_str()), Some(value));
        }
        for value in [
            td_crypto::DkimAlgorithm::Rsa,
            td_crypto::DkimAlgorithm::Ed25519,
        ] {
            match value {
                td_crypto::DkimAlgorithm::Rsa | td_crypto::DkimAlgorithm::Ed25519 => {}
            }
            assert_eq!(stored::algorithm(value.as_str()), Some(value));
        }
        for value in [
            dmarc::Outcome::None,
            dmarc::Outcome::Pass,
            dmarc::Outcome::Fail,
            dmarc::Outcome::Permerror,
            dmarc::Outcome::Temperror,
        ] {
            match value {
                dmarc::Outcome::None
                | dmarc::Outcome::Pass
                | dmarc::Outcome::Fail
                | dmarc::Outcome::Permerror
                | dmarc::Outcome::Temperror => {}
            }
            assert_eq!(stored::dmarc(value.as_str()), Some(value));
        }
        for value in [
            dmarc::Disposition::None,
            dmarc::Disposition::Quarantine,
            dmarc::Disposition::Reject,
        ] {
            match value {
                dmarc::Disposition::None
                | dmarc::Disposition::Quarantine
                | dmarc::Disposition::Reject => {}
            }
            assert_eq!(stored::disposition(value.as_str()), Some(value));
        }
        for value in [dmarc::Identifier::Spf, dmarc::Identifier::Dkim] {
            match value {
                dmarc::Identifier::Spf | dmarc::Identifier::Dkim => {}
            }
            assert_eq!(stored::identifier(value.as_str()), Some(value));
        }
        for word in ["", "PASS", "pass ", "softfail\0", "rsa-sha1"] {
            assert_eq!(stored::spf(word), None);
            assert_eq!(stored::algorithm(word), None);
        }
    }
}
