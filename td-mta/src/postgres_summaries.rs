//! Rebuildable common header values from immutable, publication-owned bytes.
use crate::{
    admission::work::{Charge, Meter},
    clock::RuntimeClock,
    header_property,
    header_select::SourceEnd,
    header_value,
    nfc::{HeaderBudget, Scratch},
    ports::{Clock, Deadline},
};
use std::time::Instant;

// Bump when shared header interpretation or these fixed limits change.
pub(crate) const VERSION: i32 = 1;
pub(crate) const HEADER_LIMIT: usize = 256 * 1024;
pub(crate) const FIELD_LIMIT: usize = 16 * 1024;
pub(crate) const NAMES: &[&str] = &["from", "to", "cc", "subject"];

pub(crate) fn field_index(name: &[u8]) -> Option<usize> {
    NAMES
        .iter()
        .position(|candidate| candidate.as_bytes() == name)
}

pub(crate) fn valid(fields: &[Vec<u8>]) -> bool {
    fields.len() == NAMES.len()
        && fields.iter().enumerate().all(|(index, bytes)| {
            if bytes.is_empty() || bytes.len() > FIELD_LIMIT {
                return false;
            }
            let Ok(mut scanner) = td_json::scan::Scanner::new(FIELD_LIMIT as u64, 8) else {
                return false;
            };
            let mut offset = 0;
            let mut root = None;
            loop {
                let Some(tail) = bytes.get(offset..) else {
                    return false;
                };
                let Ok(progress) = scanner.poll(tail, true) else {
                    return false;
                };
                offset += progress.consumed;
                if root.is_none() {
                    if let Some(td_json::scan::Event::Start { kind, .. }) = progress.event {
                        root = Some(kind);
                    }
                }
                if progress.complete {
                    return offset == bytes.len()
                        && (root == Some(td_json::scan::Kind::Null)
                            || root
                                == Some(if index == 3 {
                                    td_json::scan::Kind::String
                                } else {
                                    td_json::scan::Kind::Array
                                }));
                }
            }
        })
}

/// Failure only omits derived data; callers still enforce publication deadlines.
/// The caller owns immutable bytes from a verified message; `complete` says
/// whether this slice reaches the original message EOF.
pub(crate) fn derive(bytes: &[u8], complete: bool, deadline: Instant) -> Option<Vec<Vec<u8>>> {
    let clock = RuntimeClock::new();
    let now = clock.monotonic().ok()?;
    let remaining = deadline.checked_duration_since(Instant::now())?;
    let cap = Deadline::after(now, u64::try_from(remaining.as_millis()).ok()?).ok()?;
    let mut work = Meter::new(
        cap,
        Charge {
            io_bytes: 32 * 1024 * 1024,
            records: 4 * 1024 * 1024,
            output_bytes: 4 * FIELD_LIMIT as u64,
            ..Charge::default()
        },
    );
    let prefix = bytes.get(..bytes.len().min(HEADER_LIMIT + 2))?;
    let source_end = if complete && prefix.len() == bytes.len() {
        SourceEnd::Eof
    } else {
        SourceEnd::Prefix
    };
    let mut budget = HeaderBudget::new();
    let mut scratch = Scratch::new();
    let mut fields = Vec::new();
    fields.try_reserve_exact(NAMES.len()).ok()?;
    for name in NAMES {
        let mut property = header_property::Cursor::new(name, header_property::Context::Email);
        let property = loop {
            if Instant::now() >= deadline {
                return None;
            }
            if let header_property::Status::Complete(property) =
                property.poll(clock.monotonic().ok()?, &mut work).ok()?
            {
                break property?;
            }
        };
        let mut cursor = header_value::Cursor::new(
            header_value::Input {
                bytes: prefix,
                base: 0,
                source_end,
                header_limit: HEADER_LIMIT as u64,
                property,
            },
            &mut scratch,
            &mut work,
            &mut budget,
        )
        .ok()?;
        let mut value = Vec::new();
        value.try_reserve_exact(FIELD_LIMIT).ok()?;
        let mut buffer = [0; 4096];
        loop {
            if Instant::now() >= deadline {
                return None;
            }
            let progress = cursor.poll(clock.monotonic().ok()?, &mut buffer).ok()?;
            if value.len().checked_add(progress.written)? > FIELD_LIMIT {
                return None;
            }
            value.extend_from_slice(buffer.get(..progress.written)?);
            if matches!(progress.status, header_value::Status::Complete(_)) {
                break;
            }
        }
        cursor.check_deadline(clock.monotonic().ok()?).ok()?;
        fields.push(value);
    }
    (Instant::now() < deadline).then_some(fields)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn common_fields_use_shared_decoding_and_ignore_body_bytes() {
        let head = b"From: =?UTF-8?B?Sm9zw6k=?= <sender@example.test>\r\nTo: r@example.test\r\nSubject: =?UTF-8?Q?caf=C3=A9?=\r\n\r\n";
        let deadline = Instant::now() + Duration::from_secs(30);
        let expected = derive(head, true, deadline).unwrap();
        assert_eq!(expected.get(2).unwrap(), b"null");
        assert_eq!(expected.get(3).unwrap(), "\"café\"".as_bytes());
        assert!(std::str::from_utf8(expected.first().unwrap())
            .unwrap()
            .contains("José"));
        let mut large = head.to_vec();
        large.resize(2 * 1024 * 1024, 0xff);
        assert_eq!(derive(&large, true, deadline).unwrap(), expected);
    }

    #[test]
    fn expired_deadline_and_oversize_headers_omit_optional_summary() {
        assert!(derive(b"Subject: x\r\n\r\n", true, Instant::now()).is_none());
        let mut message = b"Subject: ".to_vec();
        message.resize(HEADER_LIMIT + 3, b'x');
        assert!(derive(&message, true, Instant::now() + Duration::from_secs(30)).is_none());
    }

    #[test]
    fn stored_values_cannot_inject_members_or_replace_the_expected_root_kind() {
        let mut fields = derive(
            b"Subject: example\r\n\r\n",
            true,
            Instant::now() + Duration::from_secs(30),
        )
        .unwrap();
        assert!(valid(&fields));
        for bad in [
            b"\"x\",\"injected\":true".as_slice(),
            b"{}",
            b"[1]",
            b"\"unterminated",
        ] {
            *fields.get_mut(3).unwrap() = bad.to_vec();
            assert!(!valid(&fields));
        }
    }
}
