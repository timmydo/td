//! Whether this system admits the prompt. td-ui's masked field is no
//! trusted-input path (td-ui/DESIGN.md, "Invariants"), and on td a secret
//! is asked for only on td's secure-attention path, so td-pinentry runs
//! on foreign desktops alone, by the rule td-pass's mode admission reads.

/// Admits a foreign system from its os-release text, or says why not. A
/// system whose `ID` or `ID_LIKE` names td is td; an unreadable or
/// ambiguous file admits nothing.
pub fn admit(os_release: Option<&str>) -> Result<(), &'static str> {
    let Some(text) = os_release else {
        return Err("td-pinentry cannot tell which system this is: os-release is unreadable");
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
            return Err("td-pinentry cannot tell which system this is: os-release repeats an ID");
        }
    }
    let td = id == Some("td") || like.is_some_and(|like| like.split(' ').any(|word| word == "td"));
    if td {
        return Err("td-pinentry asks for secrets on foreign desktops; td asks on its secure-attention path");
    }
    Ok(())
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

/// The system's identity: `/etc/os-release`, or `/usr/lib/os-release`
/// only when the first does not exist, as os-release specifies; any other
/// failure reads as unknown.
pub fn os_release() -> Option<String> {
    let mut paths = ["/etc/os-release", "/usr/lib/os-release"].into_iter();
    loop {
        let path = paths.next()?;
        match std::fs::read_to_string(path) {
            Ok(text) => return Some(text),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn td_and_its_derivatives_are_refused_and_other_systems_admitted() {
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
            assert_eq!(admit(Some(text)), Ok(()), "{text}");
        }
    }

    #[test]
    fn an_unreadable_or_ambiguous_identity_admits_nothing() {
        assert!(admit(None).is_err());
        assert!(admit(Some("ID=guix\nID=td\n")).is_err());
        assert!(admit(Some("ID_LIKE=a\nID_LIKE=b\n")).is_err());
    }
}
