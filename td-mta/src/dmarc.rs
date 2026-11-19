//! Local DMARC observations for messages accepted by direct MX.
use crate::{dkim, spf};
use policy::{Discovery, Tree};

pub(crate) mod author;
mod policy;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Outcome {
    None,
    Pass,
    Fail,
    Permerror,
    Temperror,
}

impl Outcome {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Pass => "pass",
            Self::Fail => "fail",
            Self::Permerror => "permerror",
            Self::Temperror => "temperror",
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
pub struct Report {
    pub outcome: Outcome,
    pub author: Option<String>,
    pub policy_domain: Option<String>,
}

impl Report {
    pub fn unavailable() -> Self {
        Self {
            outcome: Outcome::Temperror,
            author: None,
            policy_domain: None,
        }
    }

    pub fn invalid_author() -> Self {
        Self {
            outcome: Outcome::Permerror,
            ..Self::unavailable()
        }
    }
}

type Checked<T> = Result<T, Outcome>;

// SPF can finish on its independent worker after this DNS tree walk. Save
// its potential alignment alongside the DKIM evidence, then combine at custody.
pub(crate) struct PendingReport {
    report: Report,
    spf_alignment: Checked<bool>,
}
impl PendingReport {
    pub(crate) fn complete(report: Report) -> Self {
        Self {
            report,
            spf_alignment: Ok(false),
        }
    }

    pub(crate) fn finish(mut self, spf: spf::Outcome) -> Report {
        self.report.outcome = match (spf, self.spf_alignment) {
            (spf::Outcome::Pass, Ok(true)) => Outcome::Pass,
            (spf::Outcome::Pass, Err(error)) => combine(self.report.outcome, error),
            (spf::Outcome::Temperror, Ok(true) | Err(_)) => {
                combine(self.report.outcome, Outcome::Temperror)
            }
            _ => self.report.outcome,
        };
        self.report
    }
}

fn combine(a: Outcome, b: Outcome) -> Outcome {
    if a == Outcome::Pass || b == Outcome::Pass {
        Outcome::Pass
    } else if a == Outcome::Temperror || b == Outcome::Temperror {
        Outcome::Temperror
    } else if a == Outcome::Permerror || b == Outcome::Permerror {
        Outcome::Permerror
    } else {
        a
    }
}

struct Alignment<'a> {
    author: &'a str,
    tree: Option<Checked<Tree<'a>>>,
}

impl<'a> Alignment<'a> {
    fn aligned<D: spf::Dns>(
        &mut self,
        identifier: &'a str,
        strict: bool,
        discovery: &mut Discovery<'a, '_, D>,
    ) -> Checked<bool> {
        if self.author.eq_ignore_ascii_case(identifier) {
            return Ok(true);
        }
        if strict
            || !self
                .author
                .rsplit('.')
                .next()
                .zip(identifier.rsplit('.').next())
                .is_some_and(|(a, b)| a.eq_ignore_ascii_case(b))
        {
            return Ok(false);
        }
        let author_tree = *self.tree.get_or_insert_with(|| discovery.walk(self.author));
        let org = author_tree?.org;
        // The identifier's organizational domain must be one of its own suffixes.
        if !is_at_or_below(identifier, org) {
            return Ok(false);
        }
        Ok(discovery.walk(identifier)?.org.eq_ignore_ascii_case(org))
    }
}

fn is_at_or_below(domain: &str, suffix: &str) -> bool {
    if domain.eq_ignore_ascii_case(suffix) {
        return true;
    }
    let Some(start) = domain.len().checked_sub(suffix.len()) else {
        return false;
    };
    domain
        .get(..start)
        .is_some_and(|prefix| prefix.ends_with('.'))
        && domain
            .get(start..)
            .is_some_and(|tail| tail.eq_ignore_ascii_case(suffix))
}

pub(crate) fn evaluate(
    author: &str,
    spf_domain: Option<&str>,
    spf: Option<spf::Outcome>,
    dkim: &dkim::Report,
    dns: &mut impl spf::Dns,
    current: &dyn Fn() -> bool,
) -> PendingReport {
    let Ok(owned_author) = td_alloc::string(author) else {
        return PendingReport::complete(Report::unavailable());
    };
    let mut report = Report {
        outcome: Outcome::None,
        author: Some(owned_author),
        policy_domain: None,
    };
    let mut discovery = Discovery::new(dns, current);
    let mut align = Alignment { author, tree: None };
    let computation = (|| {
        let (name, policy) = match discovery.direct(author)? {
            Some(policy) => (author, policy),
            None => {
                let walked = discovery.walk(author)?;
                align.tree = Some(Ok(walked));
                let Some(found) = walked.inherited else {
                    return Ok((Outcome::None, Ok(false)));
                };
                found
            }
        };
        report.policy_domain = Some(td_alloc::string(name).map_err(|_| Outcome::Temperror)?);

        let exact_dkim = dkim.signatures.iter().any(|sig| {
            sig.outcome == dkim::Outcome::Pass
                && sig
                    .domain
                    .as_deref()
                    .is_some_and(|d| d.eq_ignore_ascii_case(author))
        });
        if exact_dkim
            || (spf == Some(spf::Outcome::Pass)
                && spf_domain.is_some_and(|d| d.eq_ignore_ascii_case(author)))
        {
            return Ok((Outcome::Pass, Ok(false)));
        }
        let mut dkim_result = if dkim.incomplete {
            Outcome::Temperror
        } else {
            Outcome::Fail
        };
        for sig in &dkim.signatures {
            if !matches!(sig.outcome, dkim::Outcome::Pass | dkim::Outcome::Temperror) {
                continue;
            }
            let Some(domain) = sig.domain.as_deref() else {
                dkim_result = combine(dkim_result, Outcome::Temperror);
                continue;
            };
            match align.aligned(domain, policy.strict_dkim, &mut discovery) {
                Ok(true) if sig.outcome == dkim::Outcome::Pass => {
                    return Ok((Outcome::Pass, Ok(false)))
                }
                Ok(true) => dkim_result = combine(dkim_result, Outcome::Temperror),
                Err(error) => dkim_result = combine(dkim_result, error),
                Ok(false) => (),
            }
        }
        let spf_alignment = match (spf_domain, spf) {
            (Some(domain), None | Some(spf::Outcome::Pass | spf::Outcome::Temperror)) => {
                align.aligned(domain, policy.strict_spf, &mut discovery)
            }
            _ => Ok(false),
        };
        Ok((dkim_result, spf_alignment))
    })();
    let (outcome, spf_alignment) = computation.unwrap_or_else(|error| (error, Ok(false)));
    report.outcome = outcome;
    PendingReport {
        report,
        spf_alignment,
    }
}

#[cfg(test)]
#[path = "../tests/support/dmarc_policy.rs"]
mod tests;
