//! Mode admission: which backend this system may use. td mode belongs to
//! td's admitted vault service, which this build does not reach, so on td
//! the notebook refuses rather than falling back to standalone mode.

/// The backend this system admits.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Mode {
    /// A foreign desktop: the notebook runs td-secret's vault privately.
    Standalone,
}

/// Admits a mode from the os-release text, or says why none is admitted.
/// A system whose `ID` or `ID_LIKE` names td is td; one with no `ID` is
/// `linux`, as os-release defines; an unreadable or ambiguous file
/// admits nothing.
pub fn admit(os_release: Option<&str>) -> Result<Mode, &'static str> {
    let Some(text) = os_release else {
        return Err("td-pass cannot tell which system this is: os-release is unreadable");
    };
    let mut id = None;
    let mut like = None;
    for line in text.lines() {
        let (slot, value) = if let Some(value) = line.strip_prefix("ID=") {
            (&mut id, value)
        } else if let Some(value) = line.strip_prefix("ID_LIKE=") {
            (&mut like, value)
        } else {
            continue;
        };
        if slot.replace(unquote(value)).is_some() {
            return Err("td-pass cannot tell which system this is: os-release repeats an ID");
        }
    }
    let td = id == Some("td") || like.is_some_and(|like| like.split(' ').any(|word| word == "td"));
    if td {
        return Err("td-pass needs td's vault service on td, which this build does not reach");
    }
    Ok(Mode::Standalone)
}

fn unquote(value: &str) -> &str {
    for quote in ['"', '\''] {
        if let Some(inner) = value
            .strip_prefix(quote)
            .and_then(|rest| rest.strip_suffix(quote))
        {
            return inner;
        }
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn td_and_its_derivatives_are_refused_and_other_systems_are_standalone() {
        for text in [
            "ID=td\n",
            "NAME=td\nID=\"td\"\n",
            "ID='td'\n",
            "ID=tdlike\nID_LIKE=\"debian td\"\n",
        ] {
            assert!(admit(Some(text)).is_err(), "{text}");
        }
        for text in [
            "NAME=\"Guix System\"\nID=guix\n",
            "ID=td-extra\n",
            "ID=debian\nID_LIKE=\"tdx\"\n",
            "NAME=bare\n",
        ] {
            assert_eq!(admit(Some(text)), Ok(Mode::Standalone), "{text}");
        }
    }

    #[test]
    fn an_unreadable_or_ambiguous_identity_admits_nothing() {
        assert!(admit(None).is_err());
        assert!(admit(Some("ID=guix\nID=td\n")).is_err());
        assert!(admit(Some("ID_LIKE=a\nID_LIKE=b\n")).is_err());
    }
}
