#![allow(clippy::unwrap_used, clippy::panic)]
use super::*;
use crate::spf::{Dns, DnsError, Query, Record};
use std::collections::BTreeMap;

#[derive(Default)]
struct Zone {
    names: Vec<String>,
    rr: BTreeMap<String, Result<Vec<Vec<u8>>, DnsError>>,
}
impl Zone {
    fn publish(&mut self, name: &str, records: &[&str]) {
        self.rr.insert(
            format!("_dmarc.{name}"),
            Ok(records.iter().map(|s| s.as_bytes().to_vec()).collect()),
        );
    }
}
impl Dns for Zone {
    fn lookup(&mut self, name: &str, kind: Query) -> Result<Option<Vec<Record>>, DnsError> {
        assert_eq!(kind, Query::Txt);
        assert!(
            self.names.iter().all(|n| !n.eq_ignore_ascii_case(name)),
            "duplicate lookup of {name}"
        );
        self.names.push(name.to_string());
        self.rr
            .get(&name.to_ascii_lowercase())
            .cloned()
            .transpose()
            .map(|rows| rows.map(|values| values.into_iter().map(Record::Text).collect()))
    }
}

fn signature(domain: &str, outcome: dkim::Outcome) -> dkim::SignatureResult {
    dkim::SignatureResult {
        domain: Some(domain.into()),
        outcome,
        selector: None,
        algorithm: None,
        key_bits: None,
    }
}

fn check(
    author: &str,
    envelope: Option<&str>,
    spf: spf::Outcome,
    dkim: &dkim::Report,
    zone: &mut Zone,
) -> Report {
    evaluate(author, envelope, Some(spf), dkim, zone, &|| true).finish(spf)
}
fn unsigned(zone: &mut Zone, author: &str) -> Report {
    check(
        author,
        None,
        spf::Outcome::None,
        &dkim::Report::default(),
        zone,
    )
}

#[test]
fn policy_is_required_and_only_version_matching_records_count() {
    let mut zone = Zone::default();
    zone.publish(
        "example.test",
        &[
            " unrelated",
            "v=dmarc1; p=reject",
            "foo; v=DMARC1; p=reject",
        ],
    );
    let report = check(
        "example.test",
        Some("example.test"),
        spf::Outcome::Pass,
        &dkim::Report::default(),
        &mut zone,
    );
    assert_eq!(report.outcome, Outcome::None);
    assert_eq!(report.author.as_deref(), Some("example.test"));
    assert_eq!(report.policy_domain, None);
    let mut zone = Zone::default();
    zone.publish(
        "a.example.test",
        &["v=DMARC1; p=reject", "v=DMARC1; p=none"],
    );
    zone.publish(
        "example.test",
        &["unrelated", "V = DMARC1; P=Quarantine; psd=N"],
    );
    let report = unsigned(&mut zone, "a.example.test");
    assert_eq!(report.outcome, Outcome::Fail);
    assert_eq!(report.policy_domain.as_deref(), Some("example.test"));
    assert_eq!(zone.names, ["_dmarc.a.example.test", "_dmarc.example.test"]);
}

#[test]
fn malformed_assessment_policy_can_only_be_used_with_a_valid_reporting_uri() {
    for text in [
        "v=DMARC1",
        "v=DMARC1; p=bad; rua=bogus",
        "v=DMARC1; p=none; np=no; rua=https://a/%xx, mailto:x!oops",
        "v=DMARC1; p=reject; ASPF=s; aspf=r",
    ] {
        let mut zone = Zone::default();
        zone.publish("example.test", &[text]);
        assert_eq!(
            unsigned(&mut zone, "example.test").outcome,
            Outcome::Permerror,
            "{text}"
        );
    }
    for text in [
        "v=DMARC1; rua=mailto:admin@example.test",
        "v=DMARC1; p=bogus; rua=bad, https://a.test/path%21!10m",
        "v=DMARC1; p=none; sp=n/a; rua=mailto:admin@example.test",
    ] {
        let mut zone = Zone::default();
        zone.publish("example.test", &[text]);
        assert_eq!(
            unsigned(&mut zone, "example.test").outcome,
            Outcome::Fail,
            "{text}"
        );
    }
}

#[test]
fn policy_discovery_uses_highest_record_and_skips_deep_intermediate_labels() {
    let mut zone = Zone::default();
    let author = "a.b.c.d.e.f.g.h.i.j.mail.example.com";
    zone.publish("mail.example.com", &["v=DMARC1; p=quarantine; adkim=s"]);
    zone.publish("example.com", &["v=DMARC1; p=reject"]);
    let report = check(
        author,
        Some("example.com"),
        spf::Outcome::Pass,
        &dkim::Report::default(),
        &mut zone,
    );
    assert_eq!(report.outcome, Outcome::Pass);
    assert_eq!(report.policy_domain.as_deref(), Some("example.com"));
    assert_eq!(
        zone.names,
        [
            "_dmarc.a.b.c.d.e.f.g.h.i.j.mail.example.com",
            "_dmarc.g.h.i.j.mail.example.com",
            "_dmarc.h.i.j.mail.example.com",
            "_dmarc.i.j.mail.example.com",
            "_dmarc.j.mail.example.com",
            "_dmarc.mail.example.com",
            "_dmarc.example.com",
            "_dmarc.com"
        ]
    );
}

#[test]
fn psd_boundary_uses_one_label_child_and_prefers_its_own_policy() {
    for (signer, pass) in [
        ("mail.giant.bank.example", true),
        ("mail.other.bank.example", false),
    ] {
        let mut zone = Zone::default();
        zone.publish("bank.example", &["v=DMARC1; p=reject; psd=y"]);
        zone.publish("giant.bank.example", &["v=DMARC1; p=none"]);
        let dkim = dkim::Report {
            signatures: vec![signature(signer, dkim::Outcome::Pass)],
            incomplete: false,
        };
        let report = check(
            "mx.giant.bank.example",
            None,
            spf::Outcome::None,
            &dkim,
            &mut zone,
        );
        assert_eq!(
            report.outcome,
            if pass { Outcome::Pass } else { Outcome::Fail }
        );
        assert_eq!(report.policy_domain.as_deref(), Some("giant.bank.example"));
        assert!(!zone.names.iter().any(|n| n == "_dmarc.example"));
    }
    let mut zone = Zone::default();
    zone.publish("bank.example", &["v=DMARC1; p=reject; psd=y"]);
    let report = unsigned(&mut zone, "mail.giant.bank.example");
    assert_eq!(report.policy_domain.as_deref(), Some("bank.example"));
}

#[test]
fn direct_strict_policy_overrides_relaxed_ancestor_and_unaligned_pass() {
    let mut zone = Zone::default();
    zone.publish(
        "mail.example.test",
        &["v=DMARC1; p=reject; adkim=s; aspf=s"],
    );
    zone.publish("example.test", &["v=DMARC1; p=reject; psd=n"]);
    let dkim = dkim::Report {
        signatures: vec![signature("s.mail.example.test", dkim::Outcome::Pass)],
        incomplete: false,
    };
    let report = check(
        "mail.example.test",
        Some("example.test"),
        spf::Outcome::Pass,
        &dkim,
        &mut zone,
    );
    assert_eq!(report.outcome, Outcome::Fail);
    assert_eq!(zone.names, ["_dmarc.mail.example.test"]);
}

#[test]
fn dns_errors_and_missing_authentication_evidence_never_become_definite_failures() {
    let mut zone = Zone::default();
    zone.rr
        .insert("_dmarc.example.test".into(), Err(DnsError::Temporary));
    assert_eq!(
        unsigned(&mut zone, "example.test").outcome,
        Outcome::Temperror
    );
    let mut zone = Zone::default();
    zone.publish("example.test", &["v=DMARC1; p=reject"]);
    assert_eq!(
        check(
            "example.test",
            None,
            spf::Outcome::None,
            &dkim::Report::unavailable(),
            &mut zone
        )
        .outcome,
        Outcome::Temperror
    );
    for (domain, expected) in [
        ("EXAMPLE.test", Outcome::Temperror),
        ("other.test", Outcome::Fail),
    ] {
        let mut zone = Zone::default();
        zone.publish("example.test", &["v=DMARC1; p=reject; adkim=s"]);
        let dkim = dkim::Report {
            signatures: vec![signature(domain, dkim::Outcome::Temperror)],
            incomplete: false,
        };
        assert_eq!(
            check("example.test", None, spf::Outcome::None, &dkim, &mut zone).outcome,
            expected
        );
    }
    let mut zone = Zone::default();
    assert_eq!(
        evaluate(
            "example.test",
            None,
            None,
            &dkim::Report::default(),
            &mut zone,
            &|| false
        )
        .finish(spf::Outcome::None)
        .outcome,
        Outcome::Temperror
    );
    assert!(zone.names.is_empty());
}

#[test]
fn deferred_spf_uses_its_final_verdict_and_any_aligned_dkim_pass_is_decisive() {
    for (spf, expected) in [
        (spf::Outcome::Pass, Outcome::Pass),
        (spf::Outcome::Temperror, Outcome::Temperror),
        (spf::Outcome::Fail, Outcome::Fail),
    ] {
        let mut zone = Zone::default();
        zone.publish("example.test", &["v=DMARC1; p=none; psd=n"]);
        let report = evaluate(
            "example.test",
            Some("sub.example.test"),
            None,
            &dkim::Report::default(),
            &mut zone,
            &|| true,
        )
        .finish(spf);
        assert_eq!(report.outcome, expected);
    }
    let mut zone = Zone::default();
    zone.publish("example.test", &["v=DMARC1; p=reject"]);
    let dkim = dkim::Report {
        signatures: vec![
            signature("unrelated.invalid", dkim::Outcome::Temperror),
            signature("EXAMPLE.TEST", dkim::Outcome::Pass),
        ],
        incomplete: true,
    };
    assert_eq!(
        check(
            "example.test",
            Some("unrelated.invalid"),
            spf::Outcome::Temperror,
            &dkim,
            &mut zone
        )
        .outcome,
        Outcome::Pass
    );
    assert_eq!(zone.names, ["_dmarc.example.test"]);
}

#[test]
fn reporting_fallback_preserves_strict_alignment_and_the_psd_boundary() {
    let mut zone = Zone::default();
    zone.publish(
        "example.test",
        &["v=DMARC1; p=broken; rua=mailto:r@example.test; aspf=s; psd=n"],
    );
    let report = check(
        "example.test",
        Some("mail.example.test"),
        spf::Outcome::Pass,
        &dkim::Report::default(),
        &mut zone,
    );
    assert_eq!(report.outcome, Outcome::Fail);
    assert_eq!(zone.names, ["_dmarc.example.test"]);
}

#[test]
fn organization_walk_uses_valid_records_and_preserves_dns_errors() {
    for (reply, expected) in [
        (
            Ok(vec![b"v=DMARC1; p=broken; psd=n".to_vec()]),
            Outcome::Pass,
        ),
        (Err(DnsError::Temporary), Outcome::Temperror),
        (Err(DnsError::Permanent), Outcome::Permerror),
    ] {
        let mut zone = Zone::default();
        zone.publish("example.test", &["v=DMARC1; p=reject; psd=n"]);
        zone.rr.insert("_dmarc.mail.example.test".into(), reply);
        let report = check(
            "example.test",
            Some("mail.example.test"),
            spf::Outcome::Pass,
            &dkim::Report::default(),
            &mut zone,
        );
        assert_eq!(report.outcome, expected);
        assert_eq!(report.policy_domain.as_deref(), Some("example.test"));
    }
    let mut zone = Zone::default();
    zone.publish("mail.example.test", &["v=DMARC1; p=broken"]);
    zone.publish("example.test", &["v=DMARC1; p=reject; psd=n"]);
    assert_eq!(
        unsigned(&mut zone, "mail.example.test").outcome,
        Outcome::Permerror
    );
}

#[test]
fn duplicate_tag_names_are_invalid_and_unknown_tag_values_are_ignored() {
    for pair in [
        "fo=1; FO=0",
        "ruf=mailto:a@test; Ruf=mailto:b@test",
        "t=n; T=y",
        "future=a; FUTURE=b",
    ] {
        let mut zone = Zone::default();
        zone.publish("example.test", &[&format!("v=DMARC1; p=reject; {pair}")]);
        assert_eq!(
            unsigned(&mut zone, "example.test").outcome,
            Outcome::Permerror,
            "{pair}"
        );
    }
    let mut zone = Zone::default();
    zone.publish(
        "example.test",
        &["v=DMARC1; p=reject; extra=x; :::; malformed:name=y; t=n; aspf=bogus; other=opaque"],
    );
    let report = check(
        "example.test",
        Some("mail.example.test"),
        spf::Outcome::Pass,
        &dkim::Report::default(),
        &mut zone,
    );
    assert_eq!(report.outcome, Outcome::Pass);
}

#[test]
fn inherited_policy_discovery_skips_an_unusable_intermediary() {
    let mut zone = Zone::default();
    zone.publish("dept.example.test", &["v=DMARC1; p=broken; psd=n"]);
    zone.publish("example.test", &["v=DMARC1; p=reject; psd=n"]);
    let report = check(
        "mx.dept.example.test",
        Some("example.test"),
        spf::Outcome::Pass,
        &dkim::Report::default(),
        &mut zone,
    );
    assert_eq!(report.outcome, Outcome::Pass);
    assert_eq!(report.policy_domain.as_deref(), Some("example.test"));
    assert_eq!(
        zone.names,
        [
            "_dmarc.mx.dept.example.test",
            "_dmarc.dept.example.test",
            "_dmarc.example.test"
        ]
    );
    let mut zone = Zone::default();
    zone.publish("dept.example.test", &["v=DMARC1; p=broken; psd=n"]);
    assert_eq!(
        unsigned(&mut zone, "mx.dept.example.test").outcome,
        Outcome::None
    );
}

#[test]
fn report_records_the_governing_policy_and_the_aligned_identifier() {
    let requested = |disposition, strict_spf, strict_dkim| {
        Some(Requested {
            disposition,
            strict_spf,
            strict_dkim,
        })
    };
    let mut zone = Zone::default();
    zone.publish(
        "example.test",
        &["v=DMARC1; p=reject; sp=quarantine; aspf=s"],
    );
    let signed = dkim::Report {
        signatures: vec![signature("example.test", dkim::Outcome::Pass)],
        incomplete: false,
    };
    let report = check("example.test", None, spf::Outcome::None, &signed, &mut zone);
    assert_eq!(report.outcome, Outcome::Pass);
    assert_eq!(report.policy, requested(Disposition::Reject, true, false));
    assert_eq!(report.aligned, Some(Identifier::Dkim));

    // A subdomain author inherits sp; an exact SPF domain aligns at once.
    let mut zone = Zone::default();
    zone.publish(
        "example.test",
        &["v=DMARC1; p=reject; sp=quarantine; aspf=s"],
    );
    let report = check(
        "mail.example.test",
        Some("mail.example.test"),
        spf::Outcome::Pass,
        &dkim::Report::default(),
        &mut zone,
    );
    assert_eq!(report.outcome, Outcome::Pass);
    assert_eq!(
        report.policy,
        requested(Disposition::Quarantine, true, false)
    );
    assert_eq!(report.aligned, Some(Identifier::Spf));

    // Relaxed SPF alignment is decided when the SPF result arrives.
    let mut zone = Zone::default();
    zone.publish("example.test", &["v=DMARC1; p=none; adkim=s"]);
    let pending = evaluate(
        "news.example.test",
        Some("example.test"),
        None,
        &dkim::Report::default(),
        &mut zone,
        &|| true,
    );
    let report = pending.finish(spf::Outcome::Pass);
    assert_eq!(report.outcome, Outcome::Pass);
    assert_eq!(report.policy, requested(Disposition::None, false, true));
    assert_eq!(report.aligned, Some(Identifier::Spf));

    // Failures carry the policy but no aligned identifier.
    let mut zone = Zone::default();
    zone.publish("example.test", &["v=DMARC1; p=quarantine"]);
    let report = unsigned(&mut zone, "example.test");
    assert_eq!(report.outcome, Outcome::Fail);
    assert_eq!(
        report.policy,
        requested(Disposition::Quarantine, false, false)
    );
    assert_eq!(report.aligned, None);

    // A record admitted only for its reporting URI monitors.
    let mut zone = Zone::default();
    zone.publish(
        "example.test",
        &["v=DMARC1; p=bogus; sp=reject; rua=mailto:admin@example.test"],
    );
    let report = unsigned(&mut zone, "a.example.test");
    assert_eq!(report.outcome, Outcome::Fail);
    assert_eq!(report.policy, requested(Disposition::None, false, false));

    let report = unsigned(&mut Zone::default(), "example.test");
    assert_eq!(report.outcome, Outcome::None);
    assert_eq!((report.policy, report.aligned), (None, None));
}
