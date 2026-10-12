#![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]
use super::*;
fn decode(
    tail: &str,
    names: Names,
    context: Context,
    last_scalar: bool,
    gaps: bool,
) -> (String, usize) {
    let chars: Vec<_> = tail.chars().collect();
    let mut pos = 0;
    let mut reference = Reference::new(names, context);
    let mut out = String::new();
    for _ in 0..chars.len() * 5 + 200 {
        if gaps {
            let p = reference.poll(None, false).unwrap();
            assert!(!p.consumed);
            if let Some(c) = p.scalar {
                out.push(c);
            }
            if p.status == Status::Complete {
                return (out, pos);
            }
        }
        let last = pos == chars.len() || (last_scalar && pos + 1 == chars.len());
        let p = reference.poll(chars.get(pos).copied(), last).unwrap();
        if p.consumed {
            pos += 1;
        }
        if let Some(c) = p.scalar {
            out.push(c);
        }
        if p.status == Status::Complete {
            assert_eq!(
                reference.poll(Some('!'), true).unwrap(),
                Progress {
                    consumed: false,
                    scalar: None,
                    status: Status::Complete
                }
            );
            return (out, pos);
        }
    }
    panic!("reference stalled");
}
#[test]
fn exact_legacy_unknown_and_numeric_references_are_fragment_independent() {
    for (tail, expected, consumed) in [
        ("", "&", 0),
        (";", "&", 0),
        ("amp;!", "&", 4),
        ("lt;", "<", 3),
        ("gt", ">", 2),
        ("quot ", "\"", 4),
        ("apos;", "'", 5),
        ("apos ", "&apos", 4),
        ("nbsp;", "\u{a0}", 5),
        ("ampx;", "&x", 4),
        ("unknown;", "&unknown", 7),
        ("#65;!", "A", 4),
        ("#x1f600;", "😀", 8),
        ("#X41!", "A", 4),
        ("#0;", "�", 3),
        ("#55296;", "�", 7),
        ("#1114112;", "�", 9),
        ("#128;", "€", 5),
        ("#;", "&#", 1),
        ("#x;", "&#x", 2),
        ("#-1;", "&#", 1),
        ("é", "&", 0),
        ("ampé", "&", 3),
    ] {
        for names in [Names::Html, Names::Basic] {
            for last in [false, true] {
                for gaps in [false, true] {
                    assert_eq!(
                        decode(tail, names, Context::Text, last, gaps),
                        (expected.to_owned(), consumed),
                        "{tail:?}"
                    );
                }
            }
        }
    }
}
#[test]
fn attribute_context_rejects_ambiguous_legacy_suffixes_and_names_are_case_sensitive() {
    for (tail, context, names, expected) in [
        ("amp=1", Context::Attribute, Names::Basic, "&amp"),
        ("ampx", Context::Attribute, Names::Basic, "&ampx"),
        ("amp;", Context::Attribute, Names::Basic, "&"),
        ("amp!", Context::Attribute, Names::Basic, "&"),
        ("AMP;", Context::Text, Names::Basic, "&AMP"),
        ("AMP;", Context::Text, Names::Html, "&"),
        ("copy;", Context::Text, Names::Basic, "&copy"),
        ("copy;", Context::Text, Names::Html, "©"),
    ] {
        assert_eq!(decode(tail, names, context, false, true).0, expected);
    }
}
#[test]
fn every_renderer_entity_uses_the_shared_table_and_legacy_flag() {
    for &(name, value, legacy) in crate::ENTITIES {
        assert!((2..=32).contains(&name.len()));
        assert!(name.bytes().all(|b| b.is_ascii_alphanumeric()));
        assert_eq!(
            decode(&format!("{name};"), Names::Html, Context::Text, true, true),
            (value.to_string(), name.len() + 1)
        );
        if legacy {
            assert_eq!(
                decode(name, Names::Html, Context::Text, false, false),
                (value.to_string(), name.len())
            );
        }
    }
}
#[test]
fn long_digit_runs_and_named_lookahead_keep_fixed_state_and_literal_suffixes() {
    assert!(std::mem::size_of::<Reference>() <= 128);
    for prefix in ["#", "#x"] {
        let tail = format!("{prefix}{};!", "9".repeat(4096));
        assert_eq!(
            decode(&tail, Names::Basic, Context::Text, false, true),
            ("�".to_owned(), tail.len() - 1)
        );
    }
    let tail = format!("amp{};!", "x".repeat(100));
    let (out, used) = decode(&tail, Names::Html, Context::Text, false, true);
    assert_eq!(used, 32);
    assert_eq!(out, format!("&{}", "x".repeat(29)));
    let full = format!("&{tail}");
    assert_eq!(
        crate::decode_entities(&full),
        format!("&{};!", "x".repeat(100))
    );
    assert_eq!(
        crate::decode_entities("A &bogus; B &#x; C &amp; D &ampx; E"),
        "A &bogus; B &#x; C & D &x; E"
    );
}

#[test]
fn input_requests_clones_and_terminal_misuse_are_explicit() {
    let mut reference = Reference::new(Names::Html, Context::Attribute);
    assert_eq!(
        reference.poll(None, false).unwrap(),
        Progress {
            consumed: false,
            scalar: None,
            status: Status::NeedInput,
        }
    );
    for c in "amp".chars() {
        assert!(reference.poll(Some(c), false).unwrap().consumed);
    }
    let mut cloned = reference.clone();
    let mut a = String::new();
    let mut b = String::new();
    for _ in 0..20 {
        let x = reference.poll(None, true).unwrap();
        let y = cloned.poll(None, true).unwrap();
        assert_eq!(x, y);
        if let Some(c) = x.scalar {
            a.push(c);
        }
        if let Some(c) = y.scalar {
            b.push(c);
        }
        if x.status == Status::Complete {
            break;
        }
    }
    assert_eq!(a, "&");
    assert_eq!(a, b);
    for last in [false, true] {
        for gaps in [false, true] {
            assert_eq!(
                decode("notin", Names::Html, Context::Attribute, last, gaps),
                ("&notin".to_owned(), 5)
            );
            assert_eq!(
                decode("amp", Names::Basic, Context::Attribute, last, gaps),
                ("&".to_owned(), 3)
            );
        }
    }
    let mut invalid = Reference::new(Names::Html, Context::Text);
    for c in "amp".chars() {
        assert!(invalid.poll(Some(c), false).unwrap().consumed);
    }
    assert!(!invalid.poll(Some(';'), false).unwrap().consumed);
    assert_eq!(invalid.poll(None, true), Err(Error::State));
    assert_eq!(invalid.poll(Some(';'), false), Err(Error::State));
}
