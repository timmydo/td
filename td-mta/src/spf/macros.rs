//! Domain-spec syntax and RFC 7208 section 7 substitution.
use super::{within, Checked, Dns, Evaluation, Outcome};
use std::{borrow::Cow, net::IpAddr};

enum Part<'a> {
    Literal(&'a str),
    Variable {
        letter: u8,
        count: usize,
        reverse: bool,
        delimiters: &'a str,
    },
}

fn next<'a>(source: &mut &'a str) -> Checked<Part<'a>> {
    let input = *source;
    if let Some(tail) = input.strip_prefix('%') {
        for (escape, value) in [('%', "%"), ('_', " "), ('-', "%20")] {
            if let Some(rest) = tail.strip_prefix(escape) {
                *source = rest;
                return Ok(Part::Literal(value));
            }
        }
        let (args, rest) = tail
            .strip_prefix('{')
            .and_then(|s| s.split_once('}'))
            .ok_or(Outcome::Permerror)?;
        let (letter, args) = args.as_bytes().split_first().ok_or(Outcome::Permerror)?;
        if !b"slodiphv".contains(&letter.to_ascii_lowercase()) {
            return Err(Outcome::Permerror);
        }
        let args = std::str::from_utf8(args).map_err(|_| Outcome::Permerror)?;
        let n = args.bytes().take_while(u8::is_ascii_digit).count();
        let (digits, tail) = args.split_at_checked(n).ok_or(Outcome::Permerror)?;
        let count = if digits.is_empty() {
            usize::MAX
        } else {
            let count = digits.bytes().fold(0usize, |v, b| {
                v.saturating_mul(10).saturating_add(usize::from(b - b'0'))
            });
            if count == 0 {
                return Err(Outcome::Permerror);
            }
            count
        };
        let reverse = tail.starts_with(['r', 'R']);
        let delimiters = tail.get(usize::from(reverse)..).ok_or(Outcome::Permerror)?;
        if !delimiters.bytes().all(|b| b".-+,/_=".contains(&b)) {
            return Err(Outcome::Permerror);
        }
        *source = rest;
        Ok(Part::Variable {
            letter: *letter,
            count,
            reverse,
            delimiters,
        })
    } else {
        let n = input.find('%').unwrap_or(input.len());
        let (literal, rest) = input.split_at_checked(n).ok_or(Outcome::Permerror)?;
        if !literal.bytes().all(|c| c.is_ascii_graphic()) {
            return Err(Outcome::Permerror);
        }
        *source = rest;
        Ok(Part::Literal(literal))
    }
}

pub(super) fn syntax(mut source: &str, require_domain_end: bool) -> Checked<()> {
    let whole = source;
    let mut ends_in_expansion = false;
    while !source.is_empty() {
        ends_in_expansion = source.starts_with('%');
        next(&mut source)?;
    }
    if require_domain_end && !ends_in_expansion {
        let (_, top) = whole
            .strip_suffix('.')
            .unwrap_or(whole)
            .rsplit_once('.')
            .ok_or(Outcome::Permerror)?;
        let bytes = top.as_bytes();
        if !bytes.first().is_some_and(u8::is_ascii_alphanumeric)
            || !bytes.last().is_some_and(u8::is_ascii_alphanumeric)
            || !bytes
                .iter()
                .all(|b| b.is_ascii_alphanumeric() || *b == b'-')
            || bytes.iter().all(u8::is_ascii_digit)
        {
            return Err(Outcome::Permerror);
        }
    }
    Ok(())
}

/// Only the rightmost complete labels of a long expansion can reach DNS.
/// Two extra bytes suffice for the preceding delimiter and an optional root.
struct DomainText {
    tail: [u8; 255],
    total: usize,
    limit: usize,
}

impl DomainText {
    fn write_byte(&mut self, byte: u8) -> Checked<()> {
        if self.total >= self.limit {
            return Err(Outcome::Temperror);
        }
        *self
            .tail
            .get_mut(self.total % 255)
            .ok_or(Outcome::Permerror)? = byte;
        self.total += 1;
        Ok(())
    }

    fn finish(self) -> Checked<String> {
        let mut buf = [0; 255];
        let start = self.total.saturating_sub(255);
        let end = self.total - start;
        for (index, dest) in buf.iter_mut().take(end).enumerate() {
            *dest = *self
                .tail
                .get((start + index) % 255)
                .ok_or(Outcome::Permerror)?;
        }
        let text = std::str::from_utf8(buf.get(..end).ok_or(Outcome::Permerror)?)
            .map_err(|_| Outcome::Permerror)?;
        let mut name = text.strip_suffix('.').unwrap_or(text);
        while name.len() > 253 {
            name = name.split_once('.').ok_or(Outcome::Permerror)?.1;
        }
        if name
            .split('.')
            .any(|label| label.is_empty() || label.len() > 63)
        {
            return Err(Outcome::Permerror);
        }
        Ok(td_alloc::string(name)?)
    }

    fn write_parts<'a>(
        &mut self,
        parts: impl Iterator<Item = &'a str>,
        escape: bool,
    ) -> Checked<()> {
        for (i, part) in parts.enumerate() {
            if i != 0 {
                self.write_byte(b'.')?;
            }
            for byte in part.bytes() {
                if escape && !(byte.is_ascii_alphanumeric() || b"-._~".contains(&byte)) {
                    self.write_byte(b'%')?;
                    self.write_byte(hex(byte >> 4))?;
                    self.write_byte(hex(byte & 15))?;
                } else {
                    self.write_byte(byte)?;
                }
            }
        }
        Ok(())
    }
}

fn hex(nibble: u8) -> u8 {
    if nibble < 10 {
        b'0' + nibble
    } else {
        b'A' + (nibble - 10)
    }
}

pub(super) fn ip_string(ip: IpAddr, reverse: bool) -> Checked<String> {
    match ip {
        IpAddr::V4(ip) => {
            let [a, b, c, d] = ip.octets();
            if reverse {
                td_alloc::format(format_args!("{d}.{c}.{b}.{a}"))
            } else {
                td_alloc::format(format_args!("{ip}"))
            }
            .map_err(|_| Outcome::Temperror)
        }
        IpAddr::V6(ip) => {
            let mut result = [b'.'; 63];
            for (i, n) in ip
                .octets()
                .into_iter()
                .flat_map(|b| [b >> 4, b & 15])
                .enumerate()
            {
                *result.get_mut(i * 2).ok_or(Outcome::Permerror)? = hex(n).to_ascii_lowercase();
            }
            if reverse {
                result.reverse();
            }
            Ok(td_alloc::string(
                std::str::from_utf8(&result).map_err(|_| Outcome::Permerror)?,
            )?)
        }
    }
}

impl<D: Dns> Evaluation<'_, D> {
    fn value<'a>(&'a mut self, letter: u8, domain: &'a str) -> Checked<Cow<'a, str>> {
        Ok(match letter.to_ascii_lowercase() {
            b's' => Cow::Owned(
                td_alloc::format(format_args!("{}@{}", self.local, self.origin))
                    .map_err(|_| Outcome::Temperror)?,
            ),
            b'l' => Cow::Borrowed(self.local),
            b'o' => Cow::Borrowed(self.origin),
            b'd' => Cow::Borrowed(domain),
            b'h' => Cow::Borrowed(self.helo),
            b'v' => Cow::Borrowed(if self.peer.is_ipv4() {
                "in-addr"
            } else {
                "ip6"
            }),
            b'i' => Cow::Owned(ip_string(self.peer, false)?),
            b'p' => {
                let names = self.validated(true)?;
                let best = names
                    .iter()
                    .find(|n| n.eq_ignore_ascii_case(domain))
                    .or_else(|| names.iter().find(|n| within(n, domain)))
                    .or_else(|| names.first());
                Cow::Borrowed(best.map_or("unknown", String::as_str))
            }
            _ => return Err(Outcome::Permerror),
        })
    }

    pub(super) fn expand(&mut self, mut spec: &str, domain: &str) -> Checked<String> {
        let mut text = DomainText {
            tail: [0; 255],
            total: 0,
            limit: self.expanded_bytes,
        };
        while !spec.is_empty() {
            match next(&mut spec)? {
                Part::Literal(part) => {
                    for byte in part.bytes() {
                        text.write_byte(byte)?;
                    }
                }
                Part::Variable {
                    letter,
                    count,
                    reverse,
                    delimiters,
                } => {
                    let value = self.value(letter, domain)?;
                    let delims = if delimiters.is_empty() {
                        "."
                    } else {
                        delimiters
                    };
                    let parts = || value.split(|c| delims.contains(c));
                    let skip = parts().count().saturating_sub(count);
                    if reverse {
                        text.write_parts(parts().rev().skip(skip), letter.is_ascii_uppercase())?;
                    } else {
                        text.write_parts(parts().skip(skip), letter.is_ascii_uppercase())?;
                    }
                }
            }
        }
        self.expanded_bytes -= text.total;
        text.finish()
    }
}
