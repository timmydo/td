#![allow(clippy::unwrap_used, clippy::panic)]
use super::*;

struct Resolver<F>(F);
impl<F: FnMut(&str, Query) -> Result<Vec<Record>, DnsError>> Dns for Resolver<F> {
    fn lookup(&mut self, name: &str, kind: Query) -> Result<Option<Vec<Record>>, DnsError> {
        let records = (self.0)(name, kind)?;
        Ok((!records.is_empty()).then_some(records))
    }
}

fn txt(value: &str) -> Vec<Record> {
    vec![Record::Text(value.as_bytes().to_vec())]
}

fn check(rule: &str) -> Outcome {
    let mut dns = Resolver(|host: &str, kind| {
        assert_eq!((host, kind), ("example.test", Query::Txt));
        Ok(txt(rule))
    });
    evaluate(
        &mut dns,
        "192.0.2.1".parse().unwrap(),
        "mx.test",
        "alice@example.test",
    )
}

#[test]
fn selects_one_record_and_validates_the_whole_record_before_matching() {
    for (policy, expected) in [
        ("V=sPf1 +all", Outcome::Pass),
        ("v=spf1 ~all", Outcome::Softfail),
        ("v=spf1 ?all", Outcome::Neutral),
        ("v=spf1 -all", Outcome::Fail),
        ("v=spf1", Outcome::Neutral),
        ("v=spf10 +all", Outcome::None),
        ("v=spf1\t+all", Outcome::None),
        ("v=spf1  ip4:192.0.2.0/24 -all  ", Outcome::Pass),
        ("v=spf1 ip4:192.0.2.0/32 ?all", Outcome::Neutral),
        ("v=spf1 ip6:2001:db8::/0 -all", Outcome::Fail),
        (
            "v=spf1 redirect=never.test ~all include:never.test",
            Outcome::Softfail,
        ),
        ("v=spf1 +all invalid", Outcome::Permerror),
        ("v=spf1 +all include:single", Outcome::Permerror),
        ("v=spf1 +all a://128", Outcome::Permerror),
        ("v=spf1 +all ip6:2001:db8::/012", Outcome::Permerror),
        ("v=spf1 ip4:192.0.2.001", Outcome::Permerror),
        ("v=spf1 +all a/33", Outcome::Permerror),
        ("v=spf1 +all a//129", Outcome::Permerror),
        (
            "v=spf1 +all redirect=a.test ReDiRect=b.test",
            Outcome::Permerror,
        ),
        ("v=spf1 exp=a.test -all exp=b.test", Outcome::Permerror),
        ("v=spf1 -all exp=a.test", Outcome::Fail),
        ("v=spf1 extension=%%unused extension= +all", Outcome::Pass),
        ("v=spf1 all ext=%(bad)", Outcome::Permerror),
        ("v=spf1 all exists:%{t}.test", Outcome::Permerror),
        ("v=spf1 all exists:%{d0}.test", Outcome::Permerror),
    ] {
        assert_eq!(check(policy), expected, "{policy}");
    }
    let peer = "192.0.2.1".parse().unwrap();
    let mut dns = Resolver(|_: &str, _| {
        Ok(vec![
            Record::Text(b"nothing".to_vec()),
            Record::Text(b"V=spf1 +all".to_vec()),
            Record::Text(b"v=SPF1".to_vec()),
        ])
    });
    assert_eq!(
        evaluate(&mut dns, peer, "helo.test", "a@paper.test"),
        Outcome::Permerror
    );
    let mut dns = Resolver(|_: &str, _| Err(DnsError::Temporary));
    assert_eq!(
        evaluate(&mut dns, peer, "helo.test", "a@paper.test"),
        Outcome::Temperror
    );
    for host in [
        "localhost",
        "[127.0.0.1]",
        "bad..test",
        "[IPv6:2001:db8::1]",
    ] {
        let mut dns = Resolver(|_: &str, _| panic!("unexpected DNS"));
        assert_eq!(evaluate(&mut dns, peer, host, ""), Outcome::None);
    }
}

#[test]
fn include_redirect_and_macros_keep_original_identity_including_null_sender() {
    let peer = "192.0.2.1".parse().unwrap();
    let mut dns = Resolver(|name: &str, query| {
        Ok(match (name, query) {
            ("sender.test", Query::Txt) => txt("v=spf1 include:foreign.test redirect=policy.test"),
            ("foreign.test", Query::Txt) => txt("v=spf1 -all"),
            ("policy.test", Query::Txt) => {
                txt("v=spf1 exists:%{lr-}.%{d1r}.%{o}.%{h}.%{ir}.%{S}.test -all")
            }
            (
                "bad.strong.policy.sender.test.mx.test.1.2.0.192.strong-bad%40sender.test.test",
                Query::A,
            ) => vec![Record::Address(peer)],
            ("mx.test", Query::Txt) => txt("v=spf1 exists:%{l}.%{o}.%{v}.test -all"),
            ("postmaster.mx.test.in-addr.test", Query::A) => vec![Record::Address(peer)],
            _ => panic!("unexpected query {query:?} {name}"),
        })
    });
    assert_eq!(
        evaluate(&mut dns, peer, "mx.test", "strong-bad@sender.test"),
        Outcome::Pass
    );
    assert_eq!(evaluate(&mut dns, peer, "mx.test", ""), Outcome::Pass);
    for action in ["include:", "redirect="] {
        let mut dns = Resolver(|name: &str, _| {
            Ok(if name == "one.test" {
                txt(&format!("v=spf1 {action}absent.test"))
            } else {
                Vec::new()
            })
        });
        assert_eq!(
            evaluate(&mut dns, peer, "mx.test", "a@one.test"),
            Outcome::Permerror
        );
    }
}

#[test]
fn family_specific_address_mx_and_ptr_use_validated_names() {
    let peer = "2001:db8::f".parse().unwrap();
    let rev = "f.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.8.b.d.0.1.0.0.2.ip6.arpa";
    let mut ptr_calls = 0;
    let mut dns = Resolver(|name: &str, kind| {
        Ok(match (name, kind) {
            ("example.test", Query::Txt) => txt(
                "v=spf1 a/1//128 mx:mail.test/0//64 ptr:other.test exists:%{p}.lookup.test -all",
            ),
            ("example.test", Query::Aaaa) => {
                vec![Record::Address("2001:db8:1::1".parse().unwrap())]
            }
            ("mail.test", Query::Mx) => {
                vec![Record::Name(".".into()), Record::Name("server.test".into())]
            }
            ("server.test", Query::Aaaa) => vec![Record::Address("2001:db8:2::1".parse().unwrap())],
            (name, Query::Ptr) if name == rev => {
                ptr_calls += 1;
                vec![
                    Record::Name("bad.other.test".into()),
                    Record::Name("server.example.test".into()),
                ]
            }
            ("bad.other.test", Query::Aaaa) => {
                vec![Record::Address("2001:db8::e".parse().unwrap())]
            }
            ("server.example.test", Query::Aaaa) => vec![Record::Address(peer)],
            ("server.example.test.lookup.test", Query::A) => {
                vec![Record::Address("127.0.0.1".parse().unwrap())]
            }
            _ => panic!("unexpected {kind:?} {name}"),
        })
    });
    assert_eq!(
        evaluate(&mut dns, peer, "hello.test", "a@example.test"),
        Outcome::Pass
    );
    assert_eq!(ptr_calls, 1);
    assert!(!within("mail.bad-example.test", "example.test"));
    let mut dns = Resolver(|name: &str, kind| match kind {
        Query::Txt => Ok(txt("v=spf1 ptr ~all")),
        Query::Ptr if name == rev => Err(DnsError::Temporary),
        _ => panic!(),
    });
    assert_eq!(
        evaluate(&mut dns, peer, "hello.test", "a@example.test"),
        Outcome::Softfail
    );
}

#[test]
fn lookup_budgets_span_nested_records_and_stop_before_an_eleventh_term() {
    let peer = "192.0.2.1".parse().unwrap();
    for (terms, expected) in [(10, Outcome::Pass), (11, Outcome::Permerror)] {
        let mut count = 0;
        let mut dns = Resolver(|_: &str, kind| {
            assert_eq!(kind, Query::Txt);
            count += 1;
            Ok(txt(if count <= terms {
                "v=spf1 include:same.test"
            } else {
                "v=spf1 +all"
            }))
        });
        assert_eq!(evaluate(&mut dns, peer, "a.test", "u@same.test"), expected);
        assert_eq!(count, 11);
    }
    for (terms, expected) in [(2, Outcome::Fail), (3, Outcome::Permerror)] {
        let mut dns = Resolver(|_: &str, kind| {
            Ok(match kind {
                Query::Txt => txt(&format!("v=spf1 {}-all", "exists:void.test ".repeat(terms))),
                Query::A => Vec::new(),
                _ => panic!(),
            })
        });
        assert_eq!(evaluate(&mut dns, peer, "a.test", "u@mail.test"), expected);
    }
    let mut dns = Resolver(|_: &str, kind| {
        Ok(match kind {
            Query::Txt => txt("v=spf1 mx -all"),
            Query::Mx => (0..11)
                .map(|n| Record::Name(format!("x{n}.test")))
                .collect(),
            _ => panic!(),
        })
    });
    assert_eq!(
        evaluate(&mut dns, peer, "a.test", "u@mail.test"),
        Outcome::Permerror
    );
}

#[test]
fn domain_expansion_drops_only_whole_left_labels_and_respects_root() {
    let right = format!(
        "{}.{}.{}.{}",
        "a".repeat(63),
        "b".repeat(63),
        "c".repeat(63),
        "x".repeat(61)
    );
    assert_eq!(right.len(), 253);
    let records = format!("v=spf1 exists:left.{right}. -all");
    let mut dns = Resolver(|name: &str, kind| {
        Ok(match kind {
            Query::Txt => txt(&records),
            Query::A => {
                assert_eq!(name, right);
                vec![Record::Address("192.0.2.1".parse().unwrap())]
            }
            _ => panic!(),
        })
    });
    assert_eq!(
        evaluate(
            &mut dns,
            "192.0.2.1".parse().unwrap(),
            "a.test",
            "u@mail.test"
        ),
        Outcome::Pass
    );
}

#[test]
fn resolver_exhaustion_stays_temporary_even_in_ptr_validation() {
    let peer = "192.0.2.1".parse().unwrap();
    for stop in [Query::Ptr, Query::A] {
        let mut dns = Resolver(|_: &str, kind| {
            if kind == stop {
                return Err(DnsError::Unavailable);
            }
            Ok(match kind {
                Query::Txt => txt("v=spf1 ptr -all"),
                Query::Ptr => vec![Record::Name("mail.example.test".into())],
                _ => panic!(),
            })
        });
        assert_eq!(
            evaluate(&mut dns, peer, "helo.test", "u@example.test"),
            Outcome::Temperror
        );
    }
}

#[test]
fn ptr_checks_only_first_ten_names_and_requires_forward_confirmation() {
    let peer = "192.0.2.1".parse().unwrap();
    let mut forwards = 0;
    let mut dns = Resolver(|_: &str, kind| {
        Ok(match kind {
            Query::Txt => txt("v=spf1 ptr -all"),
            Query::Ptr => (0..11)
                .map(|n| Record::Name(format!("ptr{n}.example.test")))
                .collect(),
            Query::A => {
                forwards += 1;
                vec![Record::Address("192.0.2.2".parse().unwrap())]
            }
            _ => panic!(),
        })
    });
    assert_eq!(
        evaluate(&mut dns, peer, "helo.test", "u@example.test"),
        Outcome::Fail
    );
    assert_eq!(forwards, 10);
}

#[test]
fn address_cidr_and_null_mx_check_only_the_peer_family() {
    let peer = "::ffff:192.0.2.1".parse().unwrap();
    let mut dns = Resolver(|name: &str, kind| {
        Ok(match (name, kind) {
            ("example.test", Query::Txt) => txt("v=spf1 mx:nomail.test a:other.test/24//32 -all"),
            ("nomail.test", Query::Mx) => vec![Record::Name(".".into())],
            ("other.test", Query::A) => vec![Record::Address("192.0.2.200".parse().unwrap())],
            _ => panic!(),
        })
    });
    assert_eq!(
        evaluate(&mut dns, peer, "helo.test", "u@example.test"),
        Outcome::Pass
    );
    let mut dns = Resolver(|_: &str, kind| {
        Ok(match kind {
            Query::Txt => txt("v=spf1 mx -all"),
            Query::Mx => Vec::new(),
            _ => panic!("no implicit MX"),
        })
    });
    assert_eq!(
        evaluate(&mut dns, peer, "helo.test", "u@example.test"),
        Outcome::Fail
    );
}

#[test]
fn bounds_local_policy_and_macro_work_without_assigning_a_sender_failure() {
    let peer = "2001:db8::f".parse().unwrap();
    let records = format!("v=spf1 exists:{}test -all", "%{i}.".repeat(1100));
    let mut dns = Resolver(|_: &str, kind| {
        assert_eq!(kind, Query::Txt);
        Ok(txt(&records))
    });
    assert_eq!(
        evaluate(&mut dns, peer, "helo.test", "u@example.test"),
        Outcome::Temperror
    );
    let records = format!("v=spf1 include:other.test x={} -all", "x".repeat(33_000));
    let mut dns = Resolver(|_: &str, kind| {
        assert_eq!(kind, Query::Txt);
        Ok(txt(&records))
    });
    assert_eq!(
        evaluate(&mut dns, peer, "helo.test", "u@example.test"),
        Outcome::Temperror
    );
}

#[test]
fn partial_forward_dns_failure_selects_unknown_for_p_and_keeps_confirmed_ptrs() {
    let peer = "192.0.2.7".parse().unwrap();
    for rule in [
        "v=spf1 ptr:unrelated.test exists:%{p}.check.test -all",
        "v=spf1 exists:%{p}.empty.test ptr -all",
    ] {
        let mut dns = Resolver(|name: &str, kind| match (name, kind) {
            ("example.test", Query::Txt) => Ok(txt(rule)),
            (_, Query::Ptr) => Ok(vec![
                Record::Name("broken.test".into()),
                Record::Name("mx.example.test".into()),
            ]),
            ("broken.test", Query::A) => Err(DnsError::Temporary),
            ("mx.example.test" | "unknown.check.test", Query::A) => Ok(vec![Record::Address(peer)]),
            ("unknown.empty.test", Query::A) => Ok(vec![]),
            _ => panic!("unexpected query {kind:?} {name}"),
        });
        assert_eq!(
            evaluate(&mut dns, peer, "h.test", "a@example.test"),
            Outcome::Pass
        );
    }
}
