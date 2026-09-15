//! Canonical installed hostnames shared by provisioning and installation.

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct Hostname(String);

impl Hostname {
    pub(crate) fn parse(name: &str) -> Result<Self, String> {
        if name.is_empty()
            || name.len() > 63
            || name.split('.').any(|label| {
                !label.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
                    || !label
                        .as_bytes()
                        .last()
                        .is_some_and(u8::is_ascii_alphanumeric)
                    || !label.bytes().all(|byte| {
                        byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'
                    })
            })
        {
            return Err("hostname must contain 1..=63 lowercase ASCII bytes; each dot-separated label starts with a letter, ends with a letter or digit, and contains only letters, digits or hyphens".into());
        }
        Ok(Self(name.into()))
    }

    pub(crate) fn name(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn installed_names_are_bounded_and_unambiguous() {
        for name in ["td", "my-laptop", "a1", "configured.host"] {
            assert_eq!(Hostname::parse(name).unwrap().name(), name);
        }
        assert!(Hostname::parse(&"a".repeat(63)).is_ok());
        for name in [
            "",
            "TD",
            "1host",
            "-host",
            "host-",
            ".host",
            "host.",
            "a..b",
            "a._b",
            "localhost
",
            "a b",
            "a/b",
            "a\0b",
            "höst",
            "127.0.0.1",
        ] {
            assert!(Hostname::parse(name).is_err(), "{name:?}");
        }
        assert!(Hostname::parse(&"a".repeat(64)).is_err());
    }
}
