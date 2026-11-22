//! RFC 9989 version selection, policy syntax and bounded DNS tree walks.
use super::{Checked, Disposition, Outcome, Requested};
use crate::spf::{Dns, DnsError, Query, Record};

const POLICY_BYTES: usize = 16 * 1024;
const MAX_QUESTIONS: usize = (2 + crate::dkim::MAX_SIGNATURES) * 8;

#[derive(Clone, Copy)]
pub(super) struct Policy {
    pub strict_spf: bool,
    pub strict_dkim: bool,
    psd: Option<bool>,
    p: Option<Disposition>,
    sp: Option<Disposition>,
}

impl Policy {
    /// `sp` governs a subdomain author and `p` the policy domain itself. A
    /// record admitted for its reporting URI without a usable `p` monitors.
    /// `np` would need an author-existence query, which td does not make.
    pub fn requested(self, subdomain: bool) -> Requested {
        let chosen = if subdomain {
            self.sp.or(self.p)
        } else {
            self.p
        };
        Requested {
            disposition: chosen.unwrap_or(Disposition::None),
            strict_spf: self.strict_spf,
            strict_dkim: self.strict_dkim,
        }
    }
}

fn disposition(value: &[u8]) -> Option<Disposition> {
    if value.eq_ignore_ascii_case(b"none") {
        Some(Disposition::None)
    } else if value.eq_ignore_ascii_case(b"quarantine") {
        Some(Disposition::Quarantine)
    } else if value.eq_ignore_ascii_case(b"reject") {
        Some(Disposition::Reject)
    } else {
        None
    }
}

fn wsp(mut input: &[u8]) -> &[u8] {
    while let [b' ' | b'\t', rest @ ..] = input {
        input = rest;
    }
    while let [rest @ .., b' ' | b'\t'] = input {
        input = rest;
    }
    input
}

fn pair(tag: &[u8]) -> Option<(&[u8], &[u8])> {
    let at = tag.iter().position(|&b| b == b'=')?;
    Some((wsp(tag.get(..at)?), wsp(tag.get(at + 1..)?)))
}

fn version(record: &[u8]) -> bool {
    let first = record.split(|&b| b == b';').next().unwrap_or_default();
    matches!(first.first(), Some(b'v' | b'V'))
        && pair(first)
            .is_some_and(|(tag, value)| tag.eq_ignore_ascii_case(b"v") && value == b"DMARC1")
}

struct UriWork<'a>(&'a dyn Fn() -> bool);

impl td_header::Work for UriWork<'_> {
    type Error = ();
    fn charge(&mut self, _: td_header::Charge) -> Result<(), ()> {
        (self.0)().then_some(()).ok_or(())
    }
}

fn reporting_uri(input: &[u8], current: &dyn Fn() -> bool) -> Checked<bool> {
    let input = wsp(input);
    let uri = if let Some(i) = input.iter().position(|b| *b == b'!') {
        let size = input.get(i + 1..).unwrap_or_default();
        let digits = match size {
            [digits @ .., b'k' | b'K' | b'm' | b'M' | b'g' | b'G' | b't' | b'T'] => digits,
            other => other,
        };
        if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
            return Ok(false);
        }
        input.get(..i).unwrap_or_default()
    } else {
        input
    };
    let mut validator = td_header::uri::Validator::new();
    let mut work = UriWork(current);
    for chunk in uri.chunks(256) {
        if !current() {
            return Err(Outcome::Temperror);
        }
        for byte in chunk {
            match td_header::uri::Validator::push(&mut validator, *byte, &mut work) {
                Ok(()) => (),
                Err(td_header::uri::Error::Work(())) => return Err(Outcome::Temperror),
                Err(_) => return Ok(false),
            }
        }
    }
    Ok(validator.finish().is_ok())
}

fn parse(record: &[u8], current: &dyn Fn() -> bool) -> Checked<Policy> {
    if record.len() > POLICY_BYTES || !current() {
        return Err(Outcome::Temperror);
    }
    // The version tag is checked separately. Other names follow the inherited
    // tag-list uniqueness rule even when the value has no meaning here.
    let mut names = Vec::new();
    let (mut p, mut sp, mut np) = (None, None, None);
    let (mut aspf, mut adkim, mut psd, mut rua) = (None, None, None, None);
    for field in record.split(|&b| b == b';').skip(1) {
        let Some((tag, value)) = pair(field) else {
            continue;
        };
        if tag.is_empty() || !tag.iter().all(u8::is_ascii_alphabetic) {
            continue;
        }
        td_alloc::push(&mut names, tag).map_err(|_| Outcome::Temperror)?;
        let slot = if tag.eq_ignore_ascii_case(b"p") {
            &mut p
        } else if tag.eq_ignore_ascii_case(b"sp") {
            &mut sp
        } else if tag.eq_ignore_ascii_case(b"np") {
            &mut np
        } else if tag.eq_ignore_ascii_case(b"aspf") {
            &mut aspf
        } else if tag.eq_ignore_ascii_case(b"adkim") {
            &mut adkim
        } else if tag.eq_ignore_ascii_case(b"psd") {
            &mut psd
        } else if tag.eq_ignore_ascii_case(b"rua") {
            &mut rua
        } else if tag.eq_ignore_ascii_case(b"v") {
            return Err(Outcome::Permerror);
        } else {
            continue;
        };
        *slot = Some(value);
    }
    // Each borrowed name consumes bytes from the same 16 KiB TXT record.
    names.sort_unstable_by(|a, b| {
        a.iter()
            .map(u8::to_ascii_lowercase)
            .cmp(b.iter().map(u8::to_ascii_lowercase))
    });
    if names
        .windows(2)
        .any(|pair| matches!(pair, [a,b] if a.eq_ignore_ascii_case(b)))
    {
        return Err(Outcome::Permerror);
    }
    let strict = |s: &[u8]| s.eq_ignore_ascii_case(b"s");
    let policy = Policy {
        strict_spf: aspf.is_some_and(strict),
        strict_dkim: adkim.is_some_and(strict),
        psd: match psd {
            Some(s) if s.eq_ignore_ascii_case(b"n") => Some(false),
            Some(s) if s.eq_ignore_ascii_case(b"y") => Some(true),
            _ => None,
        },
        p: p.and_then(disposition),
        sp: sp.and_then(disposition),
    };
    let valid = |tag: Option<&[u8]>| tag.is_none_or(|value| disposition(value).is_some());
    if policy.p.is_some() && valid(sp) && valid(np) {
        return Ok(policy);
    }
    // Section 4.10.1 supplies the processing rule for absent/broken policies.
    for uri in rua.into_iter().flat_map(|v| v.split(|&b| b == b',')) {
        if reporting_uri(uri, current)? {
            // Section 4.10.1: evaluate under the monitoring policy.
            return Ok(Policy {
                p: None,
                sp: None,
                ..policy
            });
        }
    }
    Err(Outcome::Permerror)
}

#[derive(Clone, Copy)]
enum Node {
    Absent,
    Unusable,
    Valid(Policy),
}

fn read(records: Option<Vec<Record>>, current: &dyn Fn() -> bool) -> Checked<Node> {
    let mut selected = None;
    for rr in records.into_iter().flatten() {
        let Record::Text(text) = rr else { continue };
        if !current() {
            return Err(Outcome::Temperror);
        }
        if version(&text) {
            if selected.is_some() {
                // Multiple version-matching TXT records invalidate this node only.
                return Ok(Node::Absent);
            }
            selected = Some(text);
        }
    }
    let Some(text) = selected else {
        return Ok(Node::Absent);
    };
    match parse(&text, current) {
        Ok(policy) => Ok(Node::Valid(policy)),
        Err(Outcome::Permerror) => Ok(Node::Unusable),
        Err(other) => Err(other),
    }
}

pub(super) struct Discovery<'a, 'd, D> {
    dns: &'d mut D,
    current: &'d dyn Fn() -> bool,
    answers: Vec<(&'a str, Checked<Node>)>,
}

#[derive(Clone, Copy)]
pub(super) struct Tree<'a> {
    pub org: &'a str,
    pub inherited: Option<(&'a str, Policy)>,
}

impl<'a, 'd, D: Dns> Discovery<'a, 'd, D> {
    pub fn new(dns: &'d mut D, current: &'d dyn Fn() -> bool) -> Self {
        Self {
            dns,
            current,
            answers: Vec::new(),
        }
    }

    pub fn direct(&mut self, domain: &'a str) -> Checked<Option<Policy>> {
        match self.node(domain)? {
            Node::Absent => Ok(None),
            Node::Valid(policy) => Ok(Some(policy)),
            Node::Unusable => Err(Outcome::Permerror),
        }
    }

    fn node(&mut self, domain: &'a str) -> Checked<Node> {
        if let Some((_, result)) = self
            .answers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(domain))
        {
            return *result;
        }
        if !(self.current)() || self.answers.len() >= MAX_QUESTIONS {
            return Err(Outcome::Temperror);
        }
        if domain.len() > 253 - "_dmarc.".len() {
            return Ok(Node::Absent);
        }
        let name =
            td_alloc::format(format_args!("_dmarc.{domain}")).map_err(|_| Outcome::Temperror)?;
        let result = self
            .dns
            .lookup(&name, Query::Txt)
            .map_err(|e| match e {
                DnsError::Permanent => Outcome::Permerror,
                DnsError::Temporary | DnsError::Unavailable => Outcome::Temperror,
            })
            .and_then(|records| read(records, self.current));
        let result = if (self.current)() {
            result
        } else {
            Err(Outcome::Temperror)
        };
        td_alloc::push(&mut self.answers, (domain, result)).map_err(|_| Outcome::Temperror)?;
        result
    }

    pub fn walk(&mut self, start: &'a str) -> Checked<Tree<'a>> {
        let mut target = start;
        let mut highest = None;
        loop {
            // Only valid records can identify an organizational domain or PSD.
            if let Node::Valid(policy) = self.node(target)? {
                if policy.psd == Some(true) && target != start {
                    let org = child_of(start, target).ok_or(Outcome::Permerror)?;
                    // Prefer the organizational record when both it and its PSD publish.
                    let inherited = highest
                        .filter(|(name, _)| *name == org)
                        .or(Some((target, policy)));
                    return Ok(Tree { org, inherited });
                }
                highest = Some((target, policy));
                if policy.psd.is_some() {
                    break;
                }
            }
            let labels = target.split('.').count();
            if labels <= 1 {
                break;
            }
            let remove = if labels >= 8 { labels - 7 } else { 1 };
            for _ in 0..remove {
                target = target.split_once('.').ok_or(Outcome::Permerror)?.1;
            }
        }
        Ok(Tree {
            org: highest.map(|(name, _)| name).unwrap_or(start),
            inherited: highest,
        })
    }
}

fn child_of<'a>(start: &'a str, parent: &str) -> Option<&'a str> {
    let prefix = start.strip_suffix(parent)?.strip_suffix('.')?;
    let label = prefix.rsplit('.').next()?;
    start.get(prefix.len().checked_sub(label.len())?..)
}
