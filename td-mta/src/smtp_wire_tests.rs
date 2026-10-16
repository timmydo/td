#![allow(clippy::indexing_slicing, clippy::unwrap_used)]
use super::*;

fn offer(bytes: &[u8]) -> Result<StartTlsOffer, Error> {
    let mut scratch = [0; LINE_BYTES];
    let mut reader = EhloReader::new(&mut scratch)?;
    let mut consumed = 0;
    while consumed < bytes.len() {
        let progress = reader.feed(&bytes[consumed..])?;
        consumed += progress.consumed;
        if reader.complete() {
            break;
        }
        if progress.complete {
            reader.advance()?;
        }
        assert!(progress.consumed > 0);
    }
    reader.into_starttls_offer(&bytes[consumed..])
}

#[test]
fn ehlo_offer_requires_a_complete_advertisement_and_no_tail() {
    assert!(offer(b"250-localhost\r\n250 sTaRtTlS\r\n").is_ok());
    for bytes in [
        b"250 STARTTLS\r\n".as_slice(),
        b"250-localhost\r\n250 STARTTLS ARG\r\n",
        b"250-localhost\r\n250 AUTH PLAIN\r\n",
        b"550 not available\r\n",
    ] {
        assert_eq!(offer(bytes).err(), Some(Error::Tls));
    }
    assert_eq!(
        offer(b"250-localhost\r\n250-STARTTLS\r\n").err(),
        Some(Error::Conflict)
    );
    assert_eq!(
        offer(b"250-localhost\r\n250-STARTTLS\r\n550 fail\r\n").err(),
        Some(Error::Invalid)
    );
    for tail in [b"220 injected\r\n".as_slice(), b"\x16\x03\x03"] {
        let bytes = [b"250-localhost\r\n250 STARTTLS\r\n".as_slice(), tail].concat();
        assert_eq!(offer(&bytes).err(), Some(Error::Invalid));
    }
}

#[test]
fn ehlo_offer_handles_fragmentation_and_skips_malformed_extensions() {
    let mut failed_scratch = [0; LINE_BYTES];
    let mut failed = EhloReader::new(&mut failed_scratch).unwrap();
    assert_eq!(failed.feed(b"250 bad\n"), Err(Error::Invalid));
    assert_eq!(failed.into_starttls_offer(&[]).err(), Some(Error::Invalid));

    let bytes = b"250-localhost\r\n250-AUTH=PLAIN LOGIN\r\n250-STARTTLS \r\n250-STARTTLS\r\n250 SIZE 1000\r\n";
    let mut scratch = [0; LINE_BYTES];
    let mut reader = EhloReader::new(&mut scratch).unwrap();
    for &byte in bytes {
        let progress = reader.feed(&[byte]).unwrap();
        assert_eq!(progress.consumed, 1);
        if progress.complete && !reader.complete() {
            reader.advance().unwrap();
        }
    }
    assert!(reader.into_starttls_offer(&[]).is_ok());
    assert_eq!(
        offer(b"250-localhost\r\n250 STARTTLS \r\n").err(),
        Some(Error::Tls)
    );
}

#[test]
fn line_split_at_every_boundary_retains_exact_tail() {
    let command = b"STARTTLS\r\n";
    for split in 0..=command.len() {
        let mut storage = [0; LINE_BYTES];
        let mut reader = LineReader::new(&mut storage).unwrap();
        let first = reader.feed(&command[..split]).unwrap();
        assert_eq!(first.consumed, split);
        let rest = [&command[split..], b"MAIL FROM:<injected>\r\n"].concat();
        let second = reader.feed(&rest).unwrap();
        assert!(second.complete);
        assert_eq!(second.consumed, command.len() - split);
        assert_eq!(&rest[second.consumed..], b"MAIL FROM:<injected>\r\n");
        assert_eq!(reader.line(), Some(&b"STARTTLS"[..]));
        assert_eq!(reader.feed(b"ignored").unwrap().consumed, 0);
        reader.advance().unwrap();
        assert_eq!(reader.line(), None);
        assert!(reader.feed(b"NOOP\r\n").unwrap().complete);
        assert_eq!(reader.line(), Some(&b"NOOP"[..]));
    }
}

#[test]
fn strict_line_framing_is_terminal_after_failure() {
    for input in [
        b"NOOP\n".as_slice(),
        b"NOOP\rX",
        b"NO\0OP\r\n",
        b"\x80\r\n",
        b"NOOP\x7f",
    ] {
        for split in 0..=input.len() {
            let mut storage = [0; LINE_BYTES];
            let mut reader = LineReader::new(&mut storage).unwrap();
            let result = reader
                .feed(&input[..split])
                .and_then(|_| reader.feed(&input[split..]));
            assert_eq!(result, Err(Error::Invalid));
            assert_eq!(reader.line(), None);
            assert_eq!(reader.advance(), Err(Error::Invalid));
            assert_eq!(reader.feed(b"NOOP\r\n"), Err(Error::Invalid));
        }
    }
}

#[test]
fn control_line_counts_crlf_and_clears_only_its_reservation() {
    let mut storage = [0xa5; LINE_BYTES + 16];
    {
        let mut reader = LineReader::new(&mut storage).unwrap();
        assert_eq!(reader.advance(), Err(Error::Conflict));
        assert!(!reader.feed(&[b'x'; LINE_BYTES - 2]).unwrap().complete);
        assert!(!reader.feed(b"\r").unwrap().complete);
        assert!(reader.feed(b"\nTAIL").unwrap().complete);
        assert_eq!(reader.line().unwrap().len(), LINE_BYTES - 2);
        reader.advance().unwrap();
    }
    assert!(storage[..LINE_BYTES].iter().all(|&b| b == 0));
    assert_eq!(storage[LINE_BYTES..], [0xa5; 16]);
    let mut reader = LineReader::new(&mut storage).unwrap();
    assert_eq!(reader.feed(&[b'x'; LINE_BYTES - 1]), Err(Error::Capacity));
    assert_eq!(reader.feed(b"\r\n"), Err(Error::Capacity));
    assert!(matches!(
        LineReader::new(&mut [0; LINE_BYTES - 1]),
        Err(Error::Capacity)
    ));
}

#[test]
fn reply_syntax_accepts_bare_codes_and_refuses_ambiguous_separators() {
    for (input, last, text) in [
        (b"220".as_slice(), true, b"".as_slice()),
        (b"250 ", true, b""),
        (b"250-", false, b""),
        (b"550 denied\ttext", true, b"denied\ttext"),
    ] {
        let reply = ReplyLine::parse(input).unwrap();
        assert_eq!(reply.last, last);
        assert_eq!(reply.text, text);
    }
    for input in [
        b"22".as_slice(),
        b"099 bad",
        b"199 bad",
        b"600 bad",
        b"260 bad",
        b"25x bad",
        b"250x",
        b"250\ttext",
        b"250 a\n",
        b"250 \x80",
    ] {
        assert_eq!(ReplyLine::parse(input), Err(Error::Invalid));
    }
}

#[test]
fn multiline_replies_require_one_code_and_leave_the_next_reply_unread() {
    let bytes = b"250-localhost\r\n250-STARTTLS\r\n250 SIZE 42\r\n220 ready\r\n";
    let mut storage = [0; LINE_BYTES];
    let mut reader = ReplyReader::new(&mut storage).unwrap();
    let mut offset = 0;
    for (index, text) in [b"localhost".as_slice(), b"STARTTLS", b"SIZE 42"]
        .into_iter()
        .enumerate()
    {
        let progress = reader.feed(&bytes[offset..]).unwrap();
        assert!(progress.complete);
        offset += progress.consumed;
        assert_eq!(reader.line().unwrap().text, text);
        assert_eq!(reader.first_line(), index == 0);
        assert_eq!(reader.complete(), index == 2);
        if index != 2 {
            reader.advance().unwrap();
        }
    }
    assert_eq!(&bytes[offset..], b"220 ready\r\n");
    assert_eq!(reader.wire_bytes(), offset);
    assert_eq!(reader.feed(&bytes[offset..]).unwrap().consumed, 0);
    assert_eq!(reader.advance(), Err(Error::Conflict));
    let mut other = [0; LINE_BYTES];
    let mut reader = ReplyReader::new(&mut other).unwrap();
    reader.feed(b"250-hello\r\n").unwrap();
    reader.advance().unwrap();
    assert_eq!(reader.feed(b"220 start\r\n"), Err(Error::Invalid));
    assert!(!reader.complete());
    assert!(reader.line().is_none());
    assert_eq!(reader.feed(b"250 end\r\n"), Err(Error::Invalid));
    let mut storage = [0; LINE_BYTES];
    let mut reader = ReplyReader::new(&mut storage).unwrap();
    assert_eq!(
        reader.feed("550 5.1.1 café unknown\r\n".as_bytes()),
        Err(Error::Invalid)
    );
    assert_eq!(reader.line(), None);
    assert!(!reader.complete());
}

#[test]
fn fragmented_220_reply_stops_before_plaintext_or_record_tail() {
    for reply in [b"220 Ready\r\n".as_slice(), b"220-first\r\n220\r\n"] {
        let mut storage = [0; LINE_BYTES];
        let mut reader = ReplyReader::new(&mut storage).unwrap();
        for &byte in reply {
            let progress = reader.feed(&[byte]).unwrap();
            assert_eq!(progress.consumed, 1);
            if progress.complete && !reader.complete() {
                reader.advance().unwrap();
            }
        }
        assert!(reader.complete());
        assert_eq!(reader.line().unwrap().code, 220);
        assert_eq!(reader.feed(b"\x16\x03\x03").unwrap().consumed, 0);
        assert_eq!(reader.wire_bytes(), reply.len());
    }
}

#[test]
fn reply_aggregate_cap_includes_all_lines_and_the_final_crlf() {
    for last in [true, false] {
        let mut storage = [0; LINE_BYTES];
        let mut reader = ReplyReader::new(&mut storage).unwrap();
        let mut line = [b'x'; LINE_BYTES];
        line[..4].copy_from_slice(b"250-");
        line[LINE_BYTES - 2..].copy_from_slice(b"\r\n");
        for _ in 0..31 {
            assert!(reader.feed(&line).unwrap().complete);
            reader.advance().unwrap();
        }
        if last {
            line[3] = b' ';
        }
        let result = reader.feed(&line);
        if last {
            assert!(result.unwrap().complete);
            assert!(reader.complete());
            assert_eq!(reader.wire_bytes(), REPLY_BYTES);
        } else {
            assert_eq!(result, Err(Error::Capacity));
            assert_eq!(reader.advance(), Err(Error::Capacity));
        }
    }
    let mut storage = [0; LINE_BYTES];
    let mut reader = ReplyReader::new(&mut storage).unwrap();
    // 2730 minimal continuation lines leave four bytes, too few for a final code.
    for _ in 0..2730 {
        assert!(reader.feed(b"250-\r\n").unwrap().complete);
        reader.advance().unwrap();
    }
    assert_eq!(reader.feed(b"250\r\n"), Err(Error::Capacity));
    assert!(!reader.complete());
}

#[test]
fn ehlo_greeting_is_never_a_starttls_advertisement() {
    let greeting = ReplyLine::parse(b"250-STARTTLS").unwrap();
    assert_eq!(ehlo_extension(greeting, true), Ok(None));
    let extension = ehlo_extension(greeting, false).unwrap().unwrap();
    assert!(extension.keyword.eq_ignore_ascii_case(b"starttls"));
    assert!(extension.parameters.is_empty());
    let auth = ReplyLine::parse(b"250 AUTH PLAIN LOGIN").unwrap();
    assert_eq!(
        ehlo_extension(auth, false),
        Ok(Some(EhloExtension {
            keyword: b"AUTH",
            parameters: b"PLAIN LOGIN"
        }))
    );
    assert_eq!(
        ehlo_extension(ReplyLine::parse(b"550 unavailable").unwrap(), false),
        Ok(None)
    );
    for text in [
        b"250 ".as_slice(),
        b"250 -X",
        b"250 AUTH ",
        b"250 AUTH  PLAIN",
        b"250 AUTH\tPLAIN",
        b"250 AUTH=PLAIN LOGIN",
        b"250 SIZE 1000 ",
    ] {
        assert_eq!(
            ehlo_extension(ReplyLine::parse(text).unwrap(), false),
            Err(Error::Invalid)
        );
    }

    // A malformed legacy extension does not poison a correctly framed reply.
    let mut storage = [0; LINE_BYTES];
    let mut reader = ReplyReader::new(&mut storage).unwrap();
    reader.feed(b"250-localhost\r\n").unwrap();
    reader.advance().unwrap();
    reader.feed(b"250-AUTH=PLAIN LOGIN\r\n").unwrap();
    assert_eq!(
        ehlo_extension(reader.line().unwrap(), reader.first_line()),
        Err(Error::Invalid)
    );
    reader.advance().unwrap();
    reader.feed(b"250 AUTH PLAIN\r\n").unwrap();
    assert!(reader.complete());
    assert_eq!(
        ehlo_extension(reader.line().unwrap(), reader.first_line()),
        Ok(Some(EhloExtension {
            keyword: b"AUTH",
            parameters: b"PLAIN"
        }))
    );
}

fn auth_offer(chunks: &[&[u8]]) -> Result<AuthMechanisms, Error> {
    relay_capabilities(chunks).map(RelayCapabilities::auth)
}
fn relay_capabilities(chunks: &[&[u8]]) -> Result<RelayCapabilities, Error> {
    let mut scratch = [0; LINE_BYTES];
    let mut reader = EhloReader::new(&mut scratch)?;
    for (index, chunk) in chunks.iter().enumerate() {
        let mut consumed = 0;
        while consumed < chunk.len() {
            let progress = reader.feed(&chunk[consumed..])?;
            consumed += progress.consumed;
            if reader.complete() {
                if chunks[index + 1..].iter().any(|chunk| !chunk.is_empty()) {
                    return Err(Error::Invalid);
                }
                return reader.into_relay_capabilities(&chunk[consumed..]);
            }
            if progress.complete {
                reader.advance()?;
            }
            assert!(progress.consumed > 0);
        }
    }
    reader.into_relay_capabilities(&[])
}

#[test]
fn auth_capabilities_require_complete_ehlo_and_exact_mechanism_names() {
    for (wire, plain, login) in [
        (
            b"250-host\r\n250 AUTH PLAIN LOGIN\r\n".as_slice(),
            true,
            true,
        ),
        (b"250-host\r\n250 aUtH LOGIN\r\n", false, true),
        (b"250-host\r\n250 AUTH PLAIN SCRAM-SHA-256\r\n", true, false),
        (
            b"250-host\r\n250 AUTH EXTERNAL X_PRIVATE-2\r\n",
            false,
            false,
        ),
        (b"250 AUTH PLAIN LOGIN\r\n", false, false),
        (b"250-host\r\n250 SIZE 1000\r\n", false, false),
        (
            b"250-host\r\n250-AUTH PLAIN\r\n250 AUTH LOGIN\r\n",
            true,
            true,
        ),
    ] {
        for split in 0..=wire.len() {
            let mechanisms = auth_offer(&[&wire[..split], &wire[split..]]).unwrap();
            assert_eq!((mechanisms.plain(), mechanisms.login()), (plain, login));
        }
        let chunks: Vec<_> = wire.chunks(1).collect();
        let mechanisms = auth_offer(&chunks).unwrap();
        assert_eq!((mechanisms.plain(), mechanisms.login()), (plain, login));
    }
}

#[test]
fn malformed_auth_list_cannot_partially_advertise_mechanisms() {
    for parameters in [
        "",
        "PLAIN ",
        "PLAIN  LOGIN",
        "PLAIN\tLOGIN",
        "plain LOGIN",
        "PLAIN login",
        "PLAIN X+BAD",
        "PLAIN ABCDEFGHIJKLMNOPQRSTU",
    ] {
        let wire = format!("250-host\r\n250 AUTH {parameters}\r\n");
        let mechanisms = auth_offer(&[wire.as_bytes()]).unwrap();
        assert!(!mechanisms.plain());
        assert!(!mechanisms.login());
    }
    for line in ["AUTH", "AUTH=PLAIN LOGIN", "AUTH\tPLAIN"] {
        let wire = format!("250-host\r\n250 {line}\r\n");
        let mechanisms = auth_offer(&[wire.as_bytes()]).unwrap();
        assert!(!mechanisms.plain());
        assert!(!mechanisms.login());
    }
    let mechanisms =
        auth_offer(&[b"250-host\r\n250-AUTH PLAIN bad\r\n250 AUTH LOGIN\r\n"]).unwrap();
    assert!(!mechanisms.plain());
    assert!(mechanisms.login());
    let mechanisms = auth_offer(&[b"250-host\r\n250 AUTH PLAIN ABCDEFGHIJKLMNOPQRST\r\n"]).unwrap();
    assert!(mechanisms.plain());
}

#[test]
fn auth_capabilities_refuse_partial_failed_and_tailed_replies() {
    for (wire, error) in [
        (b"".as_slice(), Error::Conflict),
        (b"250-host\r\n250-AUTH PLAIN\r\n", Error::Conflict),
        (b"250-host\r\n250 AUTH PLAIN\r", Error::Conflict),
        (b"550-host\r\n550 AUTH PLAIN\r\n", Error::Invalid),
        (
            b"250-host\r\n250-AUTH PLAIN\r\n550 denied\r\n",
            Error::Invalid,
        ),
        (b"250-host\r\n250 AUTH PLAIN\n", Error::Invalid),
        (
            b"250-host\r\n250 AUTH PLAIN\r\n235 injected\r\n",
            Error::Invalid,
        ),
        (
            b"250-host\r\n250 AUTH PLAIN\r\n\x16\x03\x03",
            Error::Invalid,
        ),
    ] {
        assert_eq!(auth_offer(&[wire]).err(), Some(error));
    }
    // A correctly advertised mechanism does not survive a later framing failure.
    let mut scratch = [0; LINE_BYTES];
    let mut reader = EhloReader::new(&mut scratch).unwrap();
    reader.feed(b"250-host\r\n").unwrap();
    reader.advance().unwrap();
    reader.feed(b"250-AUTH PLAIN\r\n").unwrap();
    reader.advance().unwrap();
    assert_eq!(reader.feed(b"250 bad\n"), Err(Error::Invalid));
    assert_eq!(
        reader.into_relay_capabilities(&[]).err(),
        Some(Error::Invalid)
    );
}

#[test]
fn eight_bit_mime_requires_bare_extension_in_complete_successful_ehlo() {
    for (wire, offered) in [
        (b"250-host\r\n250 8BITMIME\r\n".as_slice(), true),
        (b"250-host\r\n250 8bitMime\r\n", true),
        (b"250-host\r\n250-8BITMIME\r\n250 8BITMIME\r\n", true),
        (b"250-host\r\n250-8BITMIME bogus\r\n250 8BITMIME\r\n", true),
        (b"250 8BITMIME\r\n", false),
        (b"250-host\r\n250 8BITMIME bogus\r\n", false),
        (b"250-host\r\n250 8BITMIME \r\n", false),
        (b"250-host\r\n250 8BITMIME\t\r\n", false),
        (b"250-host\r\n250 X8BITMIME\r\n", false),
        (b"250-host\r\n250 AUTH PLAIN\r\n", false),
    ] {
        for split in 0..=wire.len() {
            let caps = relay_capabilities(&[&wire[..split], &wire[split..]]).unwrap();
            assert_eq!(caps.eight_bit_mime(), offered);
        }
        let chunks: Vec<_> = wire.chunks(1).collect();
        assert_eq!(
            relay_capabilities(&chunks).unwrap().eight_bit_mime(),
            offered
        );
    }
    for wire in [
        b"250-host\r\n250-8BITMIME\r\n".as_slice(),
        b"550-host\r\n550 8BITMIME\r\n",
        b"250-host\r\n250-8BITMIME\r\n550 failed\r\n",
        b"250-host\r\n250 8BITMIME\r\n250 injected\r\n",
    ] {
        for split in 0..=wire.len() {
            assert!(relay_capabilities(&[&wire[..split], &wire[split..]]).is_err());
        }
    }
}
