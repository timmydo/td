//! Bounded public origin identity shared by the host Git tools.
#![forbid(unsafe_code)]

#[derive(Debug, PartialEq, Eq)]
pub struct Origin {
    pub repository: String,
    pub head: String,
}

impl Origin {
    pub fn new(repository: String, head: String) -> Result<Self, String> {
        if !repository.starts_with('/')
            || repository.len() > 200
            || repository
                .split('/')
                .skip(1)
                .any(|part| part.is_empty() || part == "." || part == "..")
            || !repository
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"/-_.".contains(&b))
            || !matches!(head.len(), 40 | 64)
            || !head
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            || head.bytes().all(|b| b == b'0')
        {
            return Err("invalid VM origin path or commit".into());
        }
        Ok(Self { repository, head })
    }

    pub fn encode(&self) -> String {
        let mut result = String::with_capacity(self.repository.len() * 2 + self.head.len() + 2);
        use std::fmt::Write;
        for byte in self.repository.bytes() {
            // Writing into String cannot fail.
            let _ = write!(result, "{byte:02x}");
        }
        result.push(' ');
        result.push_str(&self.head);
        result.push('\n');
        result
    }

    pub fn parse(text: &str) -> Result<Self, String> {
        let (hex, head) = text
            .strip_suffix('\n')
            .and_then(|s| s.split_once(' '))
            .ok_or("invalid origin reply")?;
        if hex.len() > 400 || hex.len() % 2 != 0 {
            return Err("invalid origin path encoding".into());
        }
        let mut bytes = Vec::with_capacity(hex.len() / 2);
        for [a, b] in hex.as_bytes().as_chunks::<2>().0 {
            let nibble = |b| match b {
                b'0'..=b'9' => Ok(b - b'0'),
                b'a'..=b'f' => Ok(b - b'a' + 10),
                _ => Err("invalid origin path encoding"),
            };
            bytes.push(nibble(*a)? * 16 + nibble(*b)?);
        }
        Self::new(
            String::from_utf8(bytes).map_err(|_| "invalid origin path")?,
            head.into(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn origin_roundtrip_is_bounded_and_refuses_ambiguous_fields() {
        for length in [40, 64] {
            let origin = Origin::new("/srv/git/td.git".into(), "a".repeat(length)).unwrap();
            assert_eq!(Origin::parse(&origin.encode()).unwrap(), origin);
        }
        for path in [
            "/", "relative", "/a/../b", "/a//b", "/a b", "/a%20b", "/a/", "/a\nb",
        ] {
            assert!(Origin::new(path.into(), "a".repeat(40)).is_err());
        }
        for bad in ["2f61 a\n", "2f61 a\nextra", "2F61 a\n", "f a\n"] {
            assert!(Origin::parse(bad).is_err());
        }
        assert!(Origin::new("/a".into(), "0".repeat(40)).is_err());
    }
}
