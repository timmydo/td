#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
#[path = "dkim_vectors.rs"]
mod vectors;

struct Zone<F>(F);
impl<F> Dns for Zone<F>
where
    F: FnMut(&str) -> Result<Option<Vec<Record>>, DnsError>,
{
    fn lookup(&mut self, name: &str, query: Query) -> Result<Option<Vec<Record>>, DnsError> {
        assert_eq!(query, Query::Txt);
        (self.0)(name)
    }
}
fn dns(name: &str) -> Result<Option<Vec<Record>>, DnsError> {
    let key = match name {
        "test._domainkey.football.example.com" => vectors::RSA_TXT,
        "brisbane._domainkey.football.example.com" => vectors::ED_TXT,
        _ => panic!("unexpected domain {name}"),
    };
    Ok(Some(vec![Record::Text(key.as_bytes().to_vec())]))
}
fn check(raw: &[u8]) -> Report {
    evaluate(raw, 256 * 1024, &mut Zone(dns), 2_000_000_000, &|| true)
}
#[test]
fn rfc8463_appendix_both_signatures_verify_and_report_key_bits() {
    let result = check(vectors::MESSAGE);
    assert!(!result.incomplete);
    assert_eq!(result.signatures.len(), 2);
    for (record, (selector, bits)) in result
        .signatures
        .iter()
        .zip([("brisbane", 256), ("test", 1024)])
    {
        assert_eq!(record.outcome, Outcome::Pass, "{record:?}");
        assert_eq!(record.key_bits, Some(bits));
        assert_eq!(record.domain.as_deref(), Some("football.example.com"));
        assert_eq!(record.selector.as_deref(), Some(selector));
    }
}

fn digest(bytes: &[u8]) -> [u8; 32] {
    use td_crypto::Digest;
    let mut h = td_crypto::Sha256::try_new().unwrap();
    h.update(bytes).unwrap();
    h.finish().unwrap()
}

#[test]
fn canonicalizes_body_octets_before_length_truncation() {
    for (raw, simple, relaxed) in [
        (b"".as_slice(), b"\r\n".as_slice(), b"".as_slice()),
        (b"\r\n\r\n", b"\r\n", b""),
        (
            b"\r\n \ta  b \t\r\n \t\r\n\r\n",
            b"\r\n \ta  b \t\r\n \t\r\n",
            b"\r\n a b\r\n",
        ),
        (
            b"a\tb \r\n\r\nc",
            b"a\tb \r\n\r\nc\r\n",
            b"a b\r\n\r\nc\r\n",
        ),
    ] {
        for (relax, expected) in [(false, simple), (true, relaxed)] {
            assert_eq!(
                canon::body(raw, relax, None, &|| true).unwrap(),
                digest(expected)
            );
            for size in 0..=expected.len() {
                assert_eq!(
                    canon::body(raw, relax, Some(size as u64), &|| true).unwrap(),
                    digest(&expected[..size])
                );
            }
            assert_eq!(
                canon::body(raw, relax, Some(expected.len() as u64 + 1), &|| true),
                Err(Error::Invalid)
            );
        }
    }
}

#[test]
fn signed_headers_select_bottom_up_and_exclude_only_own_b_value() {
    let raw = b"From : A\r\nSubject: first\r\nSubject:\tlast \r\n \tpart  \r\nDKIM-Signature: prior\r\nDKIM-Signature:  a=x ; b =   line1\r\n \tline2  ; foo=a  b\r\n\r\nbody\r\n";
    let headers = canon::Headers::read(raw, 65536, &|| true).unwrap();
    let own = *headers.fields.last().unwrap();
    let value = headers.value(own).unwrap();
    let skip = value.windows(4).position(|s| s == b"b = ").unwrap() + 3
        ..value.windows(5).position(|s| s == b"; foo").unwrap();
    let signed = [
        "Subject",
        "subject",
        "NoSuch",
        "From",
        "DKIM-Signature",
        "Subject",
    ];
    let simple = b"Subject:\tlast \r\n \tpart  \r\nSubject: first\r\nFrom : A\r\nDKIM-Signature: prior\r\nDKIM-Signature:  a=x ; b =; foo=a  b";
    let relaxed = b"subject:last part\r\nsubject:first\r\nfrom:A\r\ndkim-signature:prior\r\ndkim-signature:a=x ; b =; foo=a b";
    for (is_relaxed, expected) in [(false, simple.as_slice()), (true, relaxed.as_slice())] {
        assert_eq!(
            canon::header_hash(&headers, own, &signed, is_relaxed, skip.clone(), &|| true).unwrap(),
            digest(expected)
        );
    }
}

#[test]
fn body_and_signed_header_changes_cannot_pass() {
    let text = std::str::from_utf8(vectors::MESSAGE).unwrap();
    for changed in [
        text.replace("We lost", "We won"),
        text.replace("Subject: Is dinner", "Subject: Is breakfast"),
        text.replace("From: Joe", "From: impostor\r\nFrom: Joe"),
    ] {
        let result = check(changed.as_bytes());
        assert_eq!(result.signatures.len(), 2);
        for signature in result.signatures {
            assert_eq!(signature.outcome, Outcome::Fail);
            assert!(signature.key_bits.is_some());
        }
    }
    let unrelated = text.replace("Message-ID:", "X-Unsigned: free to change\r\nMessage-ID:");
    assert!(check(unrelated.as_bytes())
        .signatures
        .iter()
        .all(|s| s.outcome == Outcome::Pass));
}

#[test]
fn rejects_malformed_and_restricted_signatures_independently() {
    let text = std::str::from_utf8(vectors::MESSAGE).unwrap();
    for (before, after) in [
        ("a=ed25519-sha256", "a=rsa-sha1"),
        ("v=1;", "v=9;"),
        ("s=brisbane;", "s=brisbane;s=test;"),
        ("i=@football.example.com", "i=@badfootball.example.com"),
        ("q=dns/txt", "q=other"),
        ("t=1528637909;", "t=1528637909; x=12;"),
        ("t=1528637909;", "x=100;"),
        ("t=1528637909;", "l=999999; t=1528637909;"),
        ("s=brisbane", "s=foo..bar"),
        ("h=from : to :", "h=a:b:"),
    ] {
        let mut changed = text.replacen(before, after, 1);
        if before.starts_with("h=") {
            changed = changed.replacen("message-id : from :", "message-id : other :", 1);
        }
        let r = check(changed.as_bytes());
        assert_ne!(r.signatures[0].outcome, Outcome::Pass, "{after}");
        assert_eq!(r.signatures[1].outcome, Outcome::Pass);
    }
    for key in [
        "v=DKIM1; p=",
        "v=DKIM2; k=ed25519;p=QQ==",
        "k=ed25519;v=DKIM1;p=QQ==",
        "k=ed25519; h=sha1; p=QQ==",
        "k=ed25519; s=other; p=QQ==",
        "k=rsa; p=QQ==",
        "k=ed25519;p=QQ==;X=a;X=b",
        "k=ed25519;p=A===",
    ] {
        let r = evaluate(
            vectors::MESSAGE,
            65536,
            &mut Zone(|_: &str| Ok(Some(vec![Record::Text(key.as_bytes().to_vec())]))),
            0,
            &|| true,
        );
        assert_eq!(r.signatures[0].outcome, Outcome::Permerror);
    }
    let raw = text.replacen("i=@football.example.com", "i=@mail.football.example.com", 1);
    let mut strict = Zone(|name: &str| {
        if name.starts_with("brisbane") {
            Ok(Some(vec![Record::Text(
                format!("{}; t=y:s", vectors::ED_TXT).into_bytes(),
            )]))
        } else {
            dns(name)
        }
    });
    assert_eq!(
        evaluate(raw.as_bytes(), 65536, &mut strict, 0, &|| true).signatures[0].outcome,
        Outcome::Permerror
    );
}

#[test]
fn distinguishes_unsigned_malformed_missing_dns_unavailable_and_work_limits() {
    let forged=b"From: alice@example.test\r\nAuthentication-Results: example.test; dkim=pass\r\n\r\nDKIM-Signature: v=1\r\n";
    assert_eq!(check(forged), Report::default());
    for (reply, expected) in [
        (Some(DnsError::Temporary), Outcome::Temperror),
        (Some(DnsError::Unavailable), Outcome::Temperror),
        (None, Outcome::Permerror),
    ] {
        let result = evaluate(
            vectors::MESSAGE,
            65536,
            &mut Zone(|_: &str| match reply {
                Some(err) => Err(err),
                None => Ok(None),
            }),
            0,
            &|| true,
        );
        assert_eq!(result.signatures.len(), 2);
        assert!(result
            .signatures
            .iter()
            .all(|r| r.outcome == expected && r.key_bits.is_none()));
    }
    let raw = format!(
        "{}From: x\r\n\r\n",
        "DKIM-Signature: invalid\r\n".repeat(MAX_SIGNATURES + 1)
    );
    let too_many = check(raw.as_bytes());
    assert_eq!(too_many.signatures.len(), MAX_SIGNATURES);
    assert!(too_many.incomplete);
    assert!(too_many
        .signatures
        .iter()
        .all(|s| s.outcome == Outcome::Permerror));
    assert!(evaluate(vectors::MESSAGE, 32, &mut Zone(dns), 0, &|| true).incomplete);
    let calls = std::cell::Cell::new(0usize);
    let current = || {
        calls.set(calls.get() + 1);
        calls.get() < 4
    };
    assert_eq!(
        canon::body(&vec![b'a'; 65536], true, None, &current),
        Err(Error::Unavailable)
    );
    assert_eq!(calls.get(), 4);
}

#[test]
fn local_incompletion_retains_partial_rows_and_distinguishes_remote_dns_errors() {
    let raw = std::str::from_utf8(vectors::MESSAGE).unwrap();
    let long_field = format!("s=brisbane; extra={};", "x".repeat(16_384));
    let many_tags = format!(
        "s=brisbane;{}",
        (0..65).map(|i| format!("x{i}=x;")).collect::<String>()
    );
    let long_listing = format!("h=from : {}to :", "z:".repeat(128));
    for (replace, new) in [
        ("s=brisbane;", long_field),
        ("s=brisbane;", many_tags),
        ("h=from : to :", long_listing),
    ] {
        let partial = check(raw.replacen(replace, &new, 1).as_bytes());
        assert!(partial.incomplete, "limit on {replace}");
        assert_eq!(partial.signatures[0].outcome, Outcome::Temperror);
        assert_eq!(partial.signatures[1].outcome, Outcome::Pass);
    }
    for (error, incomplete) in [(DnsError::Unavailable, true), (DnsError::Temporary, false)] {
        let mut zone = Zone(|name: &str| {
            if name.starts_with("test.") {
                Err(error)
            } else {
                dns(name)
            }
        });
        let report = evaluate(vectors::MESSAGE, 65536, &mut zone, 0, &|| true);
        assert_eq!(report.incomplete, incomplete);
        assert_eq!(report.signatures[0].outcome, Outcome::Pass);
        assert_eq!(report.signatures[1].outcome, Outcome::Temperror);
        assert_eq!(report.signatures[1].selector.as_deref(), Some("test"));
    }
    let current = std::cell::Cell::new(true);
    let mut zone = Zone(|name: &str| {
        if name.starts_with("test.") {
            current.set(false);
            Err(DnsError::Temporary)
        } else {
            dns(name)
        }
    });
    let late = evaluate(vectors::MESSAGE, 65536, &mut zone, 0, &|| current.get());
    assert!(late.incomplete);
    assert_eq!(late.signatures[0].outcome, Outcome::Pass);
    assert_eq!(late.signatures[1].outcome, Outcome::Temperror);
    for limit in 0..64 {
        let report = td_alloc::with_reservation_limit(limit, || check(vectors::MESSAGE));
        if report.signatures.len() != 2
            || report.signatures.iter().any(|s| s.outcome != Outcome::Pass)
        {
            assert!(report.incomplete, "reservation {limit}");
        }
    }
}

#[test]
fn dns_notes_and_query_extension_arguments_obey_their_qp_grammar() {
    for (notes, expected) in [
        ("two words\t=3B=FF", Outcome::Pass),
        ("", Outcome::Pass),
        ("bare=", Outcome::Permerror),
        ("bad=fg", Outcome::Permerror),
        ("split=4 1", Outcome::Permerror),
        ("split\r\n line", Outcome::Permerror),
    ] {
        let value = format!("{}; n={notes}", vectors::ED_TXT);
        let mut zone = Zone(|_: &str| Ok(Some(vec![Record::Text(value.as_bytes().to_vec())])));
        let report = evaluate(vectors::MESSAGE, 65536, &mut zone, 0, &|| true);
        assert_eq!(report.signatures[0].outcome, expected, "n={notes}");
    }
    let text = std::str::from_utf8(vectors::MESSAGE).unwrap();
    for (method, expected) in [
        ("other/foo=7Cbar:dns/txt", Outcome::Fail),
        ("other/foo|bar:dns/txt", Outcome::Permerror),
    ] {
        let changed = text.replacen("q=dns/txt", &format!("q={method}"), 1);
        let r = check(changed.as_bytes());
        assert_eq!(r.signatures[0].outcome, expected, "q={method}");
        assert!(!r.incomplete);
        assert_eq!(r.signatures[1].outcome, Outcome::Pass);
    }
}
