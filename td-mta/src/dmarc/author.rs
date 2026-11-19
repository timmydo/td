//! Extract the single Author Domain from the original direct-MX headers.
use crate::config::values::{certificate_name, MAX_CERTIFICATE_NAME_BYTES};
use td_mime::{
    header_addr_spec as addr, header_address_items as items, header_cfws as cfws,
    header_mailbox as mailbox, header_phrase as phrase, headers,
    time::{Deadline, Tick},
    work::{Charge, Meter},
};

const MAX_HEADERS: usize = 256 * 1024;
const MAX_FIELDS: usize = 1_024;
const MAX_FROM: usize = 16 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Error {
    Invalid,
    Unavailable,
}

struct Scan<'a> {
    current: &'a dyn Fn() -> bool,
    meter: Meter,
}
impl Scan<'_> {
    fn running(&self) -> Result<(), Error> {
        if (self.current)() {
            Ok(())
        } else {
            Err(Error::Unavailable)
        }
    }

    fn find_from<'a>(&mut self, raw: &'a [u8], limit: usize) -> Result<&'a [u8], Error> {
        let prefix = raw.get(..limit + 2).unwrap_or(raw);
        let mut scanner = headers::Scanner::new(0, limit as u64);
        let mut from = None;
        let mut fields = 0;
        let mut pos = 0;
        loop {
            self.running()?;
            let input = prefix.get(pos..).ok_or(Error::Invalid)?;
            let step = scanner
                .poll(input, prefix.len() == raw.len(), Tick(0), &mut self.meter)
                .map_err(|_| Error::Unavailable)?;
            pos += step.consumed;
            match step.status {
                headers::Status::Field(f) => {
                    fields += 1;
                    if fields > MAX_FIELDS {
                        return Err(Error::Unavailable);
                    }
                    let name = prefix
                        .get(f.name_start as usize..f.name_end as usize)
                        .ok_or(Error::Invalid)?;
                    if name.eq_ignore_ascii_case(b"From") {
                        if from.is_some() {
                            return Err(Error::Invalid);
                        }
                        from = prefix.get(f.value_start as usize..f.value_end as usize);
                    }
                }
                headers::Status::Complete(end) => {
                    // The shared MIME scanner can recover a body at a malformed header.
                    // Only a real header terminator or exhaustion can settle uniqueness.
                    let start = end.header_bytes as usize;
                    let end = end.body_start as usize;
                    if prefix.get(start..end) != Some(b"\r\n")
                        && !(start == raw.len() && start == end)
                    {
                        return Err(Error::Invalid);
                    }
                    return from.ok_or(Error::Invalid);
                }
                headers::Status::NeedInput => return Err(Error::Unavailable),
                headers::Status::Yield => {}
            }
        }
    }

    fn group_name(&mut self, source: &[u8]) -> Result<(), Error> {
        let mut parser = phrase::Cursor::new(source);
        let mut seen = false;
        loop {
            self.running()?;
            match parser.poll(Tick(0), &mut self.meter) {
                Ok(phrase::Status::Token(_)) => seen = true,
                Ok(phrase::Status::Complete(_)) if seen => return Ok(()),
                Ok(phrase::Status::Complete(_)) | Err(phrase::Error::Malformed) => {
                    return Err(Error::Invalid)
                }
                Ok(phrase::Status::Yield) => {}
                Err(_) => return Err(Error::Unavailable),
            }
        }
    }

    fn cfws_only(&mut self, source: &[u8]) -> Result<bool, Error> {
        let mut parser = cfws::Cursor::new(source, 0);
        loop {
            self.running()?;
            match parser.poll(Tick(0), &mut self.meter) {
                Ok(cfws::Status::Complete(end)) => return Ok(end.position == source.len()),
                Ok(cfws::Status::Yield | cfws::Status::Comment(_)) => {}
                Err(cfws::Error::Malformed) => return Err(Error::Invalid),
                Err(_) => return Err(Error::Unavailable),
            }
        }
    }

    fn address<'a>(&mut self, candidate: &'a [u8]) -> Result<&'a [u8], Error> {
        let mut parser = mailbox::Cursor::new(candidate);
        loop {
            self.running()?;
            match parser.poll(Tick(0), &mut self.meter) {
                Ok(mailbox::Status::Complete(m)) => {
                    return candidate
                        .get(m.address.start..m.address.end)
                        .ok_or(Error::Invalid);
                }
                Ok(mailbox::Status::Yield) => {}
                Err(mailbox::Error::Malformed) => return Err(Error::Invalid),
                Err(_) => return Err(Error::Unavailable),
            }
        }
    }

    fn domain<'a>(&mut self, address: &[u8], buf: &'a mut [u8]) -> Result<&'a str, Error> {
        let mut length = 0;
        let mut right = false;
        let mut parser = addr::Cursor::new(address);
        loop {
            self.running()?;
            match parser.poll(Tick(0), &mut self.meter) {
                Ok(addr::Status::Part(p)) => {
                    let part = address.get(p.start..p.end).ok_or(Error::Invalid)?;
                    if right {
                        if !part.is_ascii() {
                            // The receiving profile has no SMTPUTF8/IDNA conversion.
                            return Err(Error::Unavailable);
                        }
                        let end = length + part.len();
                        buf.get_mut(length..end)
                            .ok_or(Error::Invalid)?
                            .copy_from_slice(part);
                        length = end;
                    } else if part == b"@" {
                        // A quoted local-part, including any '@', is one whole token.
                        right = true;
                    }
                }
                Ok(addr::Status::Complete) => {
                    let bytes = buf.get_mut(..length).ok_or(Error::Invalid)?;
                    bytes.make_ascii_lowercase();
                    let name = std::str::from_utf8(bytes).map_err(|_| Error::Invalid)?;
                    certificate_name(name).map_err(|_| Error::Invalid)?;
                    return Ok(name);
                }
                Ok(addr::Status::Yield) => {}
                Err(addr::Error::Malformed) => return Err(Error::Invalid),
                Err(_) => return Err(Error::Unavailable),
            }
        }
    }
}

pub(crate) fn domain(
    raw: &[u8],
    header_limit: usize,
    current: &dyn Fn() -> bool,
) -> Result<String, Error> {
    let limit = header_limit.min(MAX_HEADERS);
    let budget = (2 * (limit + 3) + 64 * MAX_FROM) as u64;
    let mut scan = Scan {
        current,
        meter: Meter::new(
            Deadline::at(Tick(1)),
            Charge {
                io_bytes: budget,
                records: budget,
                ..Charge::default()
            },
        ),
    };
    let from = scan.find_from(raw, limit)?;
    if from.len() > MAX_FROM {
        return Err(Error::Unavailable);
    }
    let mut chosen: Option<String> = None;
    let mut buf = [0; MAX_CERTIFICATE_NAME_BYTES];
    let mut parser = items::Cursor::new(from);
    let mut in_group = false;
    let mut after_group = false;
    loop {
        scan.running()?;
        match parser
            .poll(Tick(0), &mut scan.meter)
            .map_err(|_| Error::Unavailable)?
        {
            items::Status::Item(item) => {
                if item.unclosed {
                    return Err(Error::Invalid);
                }
                let mut start = item.start;
                if let Some(colon) = item.colon {
                    if in_group || after_group {
                        return Err(Error::Invalid);
                    }
                    scan.group_name(from.get(start..colon).ok_or(Error::Invalid)?)?;
                    in_group = true;
                    start = colon + 1;
                }
                let candidate = from.get(start..item.end).ok_or(Error::Invalid)?;
                let empty = scan.cfws_only(candidate)?;
                if (after_group && !empty)
                    || (item.separator == items::Separator::Semicolon && !in_group)
                    || (item.separator == items::Separator::End && in_group)
                {
                    return Err(Error::Invalid);
                }
                // A group ends at ';'. Only CFWS may precede the next comma or EOF.
                after_group = item.separator == items::Separator::Semicolon;
                if after_group {
                    in_group = false;
                }
                if empty {
                    continue;
                }
                let address = scan.address(candidate)?;
                let name = scan.domain(address, &mut buf)?;
                match &chosen {
                    Some(old) if old != name => return Err(Error::Invalid),
                    Some(_) => {}
                    None => chosen = Some(td_alloc::string(name).map_err(|_| Error::Unavailable)?),
                }
            }
            items::Status::Complete => return chosen.ok_or(Error::Invalid),
            items::Status::Yield => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extract(raw: &[u8]) -> Result<String, Error> {
        domain(raw, MAX_HEADERS, &|| true)
    }

    #[test]
    fn validated_mailboxes_yield_only_the_normalized_author_domain() {
        for raw in [
            b"FrOm : a@EXAMPLE.org\r\n\r\nFrom: body@elsewhere.example\r\n".as_slice(),
            b"From: \"author@example.net, Inc.\"\r\n\t<\"joe@y\"(x)@(x)Example.(else)ORG>\r\n\r\n",
            b"From: <@route.example:a@EXAMPLE.org>, b@Example.Org (fake@test.example)\r\n",
            b"Sender: sender@example.net\r\nResent-From: resender@example.net\r\nFrom: a@example.org\r\n\r\n",
        ] {
            assert_eq!(extract(raw).as_deref(), Ok("example.org"));
        }
        assert_eq!(
            extract(b"From: x@XN--P1AI\r\n\r\n").as_deref(),
            Ok("xn--p1ai")
        );
    }

    #[test]
    fn missing_competing_and_malformed_fields_cannot_supply_an_author() {
        for raw in [
            b"Sender: a@example.org\r\n\r\nFrom: body@example.org".as_slice(),
            b"From: a@example.org\r\nFROM: b@example.org\r\n\r\n",
            b"From: a@example.org,b@example.net\r\n\r\n",
            b"From: a@example.org\r\nmalformed\r\nFrom: b@example.net\r\n\r\n",
            b"From: a@example.org garbage\r\n\r\n",
            b"From: a@example.org; b@example.org\r\n\r\n",
            b"From: a@example.org, malformed\r\n\r\n",
            b"From: a@[127.0.0.1]\r\n\r\n",
            b"From: a@example._mail\r\n\r\n",
            b"From: a@example.org (unclosed\r\n\r\n",
        ] {
            assert_eq!(extract(raw), Err(Error::Invalid), "{raw:?}");
        }
    }

    #[test]
    fn groups_obey_address_list_boundaries_and_need_one_distinct_domain() {
        for value in [
            "Team: joe@example.org, alex@EXAMPLE.org;",
            "Undisclosed:; (tail), Team: x@example.org; , , y@example.org,",
            "\"A:B@fake.test\": x@example.org; (group)",
        ] {
            let raw = format!("From: {value}\r\n\r\n");
            assert_eq!(
                extract(raw.as_bytes()).as_deref(),
                Ok("example.org"),
                "{value}"
            );
        }
        for value in [
            "No authors:;",
            "Team: x@example.org; y@example.org",
            "Team: x@example.org",
            "Team: x@example.org, Other: y@example.org;",
            "Team: x@example.org, y@different.org;",
            ": x@example.org;",
            "a@wrong.org: x@example.org;",
            "Team: x@example.org;;",
        ] {
            let raw = format!("From: {value}\r\n\r\n");
            assert_eq!(extract(raw.as_bytes()), Err(Error::Invalid), "{value}");
        }
    }

    #[test]
    fn unsupported_input_and_resource_limits_are_inconclusive() {
        let raw = b"From: a@example.org\r\nOther: too much\r\n\r\n";
        assert_eq!(domain(raw, 22, &|| true), Err(Error::Unavailable));
        assert_eq!(domain(raw, MAX_HEADERS, &|| false), Err(Error::Unavailable));
        let mut raw = b"From: a@example.org\r\n".to_vec();
        for _ in 0..MAX_FIELDS {
            raw.extend_from_slice(b"X: 1\r\n");
        }
        raw.extend_from_slice(b"\r\n");
        assert_eq!(extract(&raw), Err(Error::Unavailable));
        let oversized = format!("From: {} <a@example.org>\r\n\r\n", "x".repeat(MAX_FROM));
        assert_eq!(extract(oversized.as_bytes()), Err(Error::Unavailable));
        assert_eq!(
            extract("From: x@é.example\r\n\r\n".as_bytes()),
            Err(Error::Unavailable)
        );
    }
}
