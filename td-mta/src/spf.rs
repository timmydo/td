//! RFC 7208 envelope-sender authorization. The result is observation data.
use std::net::IpAddr;

mod macros;
mod policy;
#[cfg(test)]
#[path = "../tests/support/spf_evaluator.rs"]
mod tests;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Outcome {
    None,
    Neutral,
    Pass,
    Fail,
    Softfail,
    Temperror,
    Permerror,
}

impl Outcome {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Neutral => "neutral",
            Self::Pass => "pass",
            Self::Fail => "fail",
            Self::Softfail => "softfail",
            Self::Temperror => "temperror",
            Self::Permerror => "permerror",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Query {
    A,
    Aaaa,
    Mx,
    Ptr,
    Txt,
}

#[derive(Debug)]
pub(crate) enum Record {
    Address(IpAddr),
    Name(String),
    Text(Vec<u8>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DnsError {
    /// A DNS failure for this question, such as SERVFAIL.
    Temporary,
    /// The evaluator can no longer use its reserved resources or deadline.
    Unavailable,
    Permanent,
}

/// Complete, bounded RRset for the requested type after alias traversal.
/// None means NXDOMAIN or NOERROR with ANCOUNT zero (RFC 7208 §4.6.4).
/// A positive answer may yield an empty RRset after filtering by owner and type.
pub(crate) trait Dns {
    fn lookup(&mut self, name: &str, kind: Query) -> Result<Option<Vec<Record>>, DnsError>;
}

pub(crate) const MAX_PTR_NAMES: usize = 10;

type Checked<T> = Result<T, Outcome>;

// Bound aggregate policy storage across recursive includes and redirects,
// and the substitution work an observation can consume after its last query.
const POLICY_BYTES: usize = 65_535;
const EXPANDED_BYTES: usize = 65_535;

impl From<td_alloc::Error> for Outcome {
    fn from(_: td_alloc::Error) -> Self {
        Self::Temperror
    }
}

impl From<DnsError> for Outcome {
    fn from(value: DnsError) -> Self {
        match value {
            DnsError::Temporary | DnsError::Unavailable => Self::Temperror,
            DnsError::Permanent => Self::Permerror,
        }
    }
}

/// Called only for a direct SMTP peer. An empty reverse path uses
/// postmaster@HELO; the original envelope remains fixed through recursion.
pub(crate) fn evaluate(
    dns: &mut impl Dns,
    peer: IpAddr,
    helo: &str,
    reverse_path: &str,
) -> Outcome {
    let (local, domain) = if reverse_path.is_empty() {
        ("postmaster", helo)
    } else if let Some((local, domain)) = reverse_path.rsplit_once('@') {
        (
            if local.is_empty() {
                "postmaster"
            } else {
                local
            },
            domain,
        )
    } else {
        return Outcome::None;
    };
    let domain = domain.strip_suffix('.').unwrap_or(domain);
    if !domain.contains('.') || crate::config::values::certificate_name(domain).is_err() {
        return Outcome::None;
    }
    let mut check = Evaluation {
        dns,
        peer: peer.to_canonical(),
        helo,
        local,
        origin: domain,
        terms: 0,
        voids: 0,
        validated: None,
        policy_bytes: POLICY_BYTES,
        expanded_bytes: EXPANDED_BYTES,
    };
    check.domain(domain).unwrap_or_else(|outcome| outcome)
}

struct ValidatedNames {
    names: Vec<String>,
    dns_error: bool,
}

struct Evaluation<'a, D> {
    dns: &'a mut D,
    peer: IpAddr,
    helo: &'a str,
    local: &'a str,
    origin: &'a str,
    terms: usize,
    voids: usize,
    validated: Option<ValidatedNames>,
    policy_bytes: usize,
    expanded_bytes: usize,
}

impl<D: Dns> Evaluation<'_, D> {
    fn term(&mut self) -> Checked<()> {
        self.terms += 1;
        if self.terms > 10 {
            return Err(Outcome::Permerror);
        }
        Ok(())
    }

    fn lookup(&mut self, name: &str, kind: Query) -> Result<Vec<Record>, DnsError> {
        if let Some(records) = self.dns.lookup(name, kind)? {
            return Ok(records);
        }
        self.voids += 1;
        if self.voids > 2 {
            return Err(DnsError::Permanent);
        }
        Ok(Vec::new())
    }

    fn domain(&mut self, domain: &str) -> Checked<Outcome> {
        if !domain.contains('.') {
            return Ok(Outcome::None);
        }
        let mut selected = None;
        for record in self.lookup(domain, Query::Txt)? {
            if let Record::Text(text) = record {
                if text
                    .split(|&b| b == b' ')
                    .next()
                    .is_some_and(|first| first.eq_ignore_ascii_case(b"v=spf1"))
                {
                    if selected.is_some() {
                        return Err(Outcome::Permerror);
                    }
                    selected = Some(text);
                }
            }
        }
        let Some(selected) = selected else {
            return Ok(Outcome::None);
        };
        self.policy_bytes = self
            .policy_bytes
            .checked_sub(selected.len())
            .ok_or(Outcome::Temperror)?;
        let record = std::str::from_utf8(&selected).map_err(|_| Outcome::Permerror)?;
        let terms = || record.split(' ').skip(1).filter(|term| !term.is_empty());
        let mut redirect = None;
        let mut explanation = false;
        // Syntax throughout the record must be valid before the first match.
        for token in terms() {
            match policy::parse(token)? {
                policy::Term::Redirect(target) => {
                    if redirect.is_some() {
                        return Err(Outcome::Permerror);
                    }
                    redirect = Some(target);
                }
                policy::Term::Explanation if std::mem::replace(&mut explanation, true) => {
                    return Err(Outcome::Permerror)
                }
                _ => {}
            }
        }
        for token in terms() {
            if let policy::Term::Directive(qualifier, rule) = policy::parse(token)? {
                if self.matches(rule, domain)? {
                    return Ok(qualifier);
                }
            }
        }
        if let Some(target) = redirect {
            self.term()?;
            let target = self.expand(target, domain)?;
            match self.domain(&target)? {
                Outcome::None => Err(Outcome::Permerror),
                answer => Ok(answer),
            }
        } else {
            Ok(Outcome::Neutral)
        }
    }

    fn addresses(&mut self, host: &str) -> Result<Vec<Record>, DnsError> {
        self.lookup(
            host,
            if self.peer.is_ipv4() {
                Query::A
            } else {
                Query::Aaaa
            },
        )
    }

    fn matches(&mut self, rule: policy::Mechanism<'_>, domain: &str) -> Checked<bool> {
        use policy::Mechanism::*;
        match rule {
            All => return Ok(true),
            Network(address, prefix) => return Ok(same_network(self.peer, address, prefix)),
            _ => self.term()?,
        }
        let target = match rule.target() {
            Some(spec) => self.expand(spec, domain)?,
            None => td_alloc::string(domain)?,
        };
        match rule {
            Include(_) => match self.domain(&target)? {
                Outcome::None => Err(Outcome::Permerror),
                result => Ok(result == Outcome::Pass),
            },
            Address(_, v4, v6) => {
                let prefix = if self.peer.is_ipv4() { v4 } else { v6 };
                let records = self.addresses(&target)?;
                Ok(has_address(&records, self.peer, prefix))
            }
            Mx(_, v4, v6) => {
                let prefix = if self.peer.is_ipv4() { v4 } else { v6 };
                let names = self.lookup(&target, Query::Mx)?;
                if names.len() > 10 {
                    return Err(Outcome::Permerror);
                }
                for name in names {
                    if let Record::Name(host) = name {
                        if host != "." && has_address(&self.addresses(&host)?, self.peer, prefix) {
                            return Ok(true);
                        }
                    }
                }
                Ok(false)
            }
            Ptr(_) => {
                let names = self.validated(false)?;
                Ok(names.iter().any(|name| within(name, &target)))
            }
            Exists(_) => Ok(self
                .lookup(&target, Query::A)?
                .iter()
                .any(|record| matches!(record, Record::Address(IpAddr::V4(_))))),
            All | Network(_, _) => Err(Outcome::Permerror),
        }
    }

    fn validated(&mut self, macro_lookup: bool) -> Checked<&[String]> {
        if self.validated.is_none() {
            if macro_lookup {
                self.term()?;
            }
            let mut reverse = macros::ip_string(self.peer, true)?;
            td_alloc::push_str(
                &mut reverse,
                if self.peer.is_ipv4() {
                    ".in-addr.arpa"
                } else {
                    ".ip6.arpa"
                },
            )?;
            let ptr = match self.lookup(&reverse, Query::Ptr) {
                Ok(records) => records,
                Err(DnsError::Temporary) => Vec::new(),
                Err(error) => return Err(error.into()),
            };
            let mut valid = ValidatedNames {
                names: Vec::new(),
                dns_error: false,
            };
            for record in ptr.into_iter().take(MAX_PTR_NAMES) {
                if let Record::Name(name) = record {
                    let addresses = match self.addresses(&name) {
                        Ok(records) => records,
                        Err(DnsError::Temporary) => {
                            valid.dns_error = true;
                            continue;
                        }
                        Err(error) => return Err(error.into()),
                    };
                    if addresses
                        .iter()
                        .any(|r| matches!(r, Record::Address(ip) if *ip == self.peer))
                    {
                        td_alloc::push(&mut valid.names, name)?;
                    }
                }
            }
            self.validated = Some(valid);
        }
        // A ptr mechanism may still match another confirmed host. The p macro
        // substitutes unknown if any of the validation lookups failed.
        Ok(self
            .validated
            .as_ref()
            .filter(|valid| !(macro_lookup && valid.dns_error))
            .map_or(&[], |valid| valid.names.as_slice()))
    }
}

fn within(name: &str, parent: &str) -> bool {
    name.eq_ignore_ascii_case(parent)
        || name.len().checked_sub(parent.len()).is_some_and(|at| {
            name.get(..at).is_some_and(|prefix| prefix.ends_with('.'))
                && name
                    .get(at..)
                    .is_some_and(|suffix| suffix.eq_ignore_ascii_case(parent))
        })
}

fn has_address(records: &[Record], peer: IpAddr, prefix: u8) -> bool {
    records.iter().any(
        |record| matches!(record, Record::Address(address) if same_network(peer,*address,prefix)),
    )
}

fn same_network(peer: IpAddr, net: IpAddr, prefix: u8) -> bool {
    match (peer, net) {
        (IpAddr::V4(a), IpAddr::V4(b)) => {
            (a.to_bits() ^ b.to_bits()).leading_zeros() >= u32::from(prefix)
        }
        (IpAddr::V6(a), IpAddr::V6(b)) => {
            (a.to_bits() ^ b.to_bits()).leading_zeros() >= u32::from(prefix)
        }
        _ => false,
    }
}
