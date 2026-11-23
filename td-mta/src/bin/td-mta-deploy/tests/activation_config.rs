#![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
use super::*;

const OLD: &str = "00000000000000000000000000000001";
const NEW: &str = "00000000000000000000000000000002";
const DIGEST: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

/// Scripted host: each fault names the step that fails and how.
#[derive(Default)]
struct Fake {
    log: Vec<String>,
    receipt: Vec<(String, String)>,
    changed: bool,
    publish: Vec<&'static str>,
    select: Vec<&'static str>,
    inspect: &'static str,
    verify: Vec<bool>,
    check: Option<&'static str>,
}

impl Fake {
    fn changed() -> Self {
        Self {
            changed: true,
            publish: vec!["ok"],
            select: vec!["recorded", "recorded"],
            inspect: "uncertain",
            verify: vec![true, true],
            ..Self::default()
        }
    }
    fn get(&self, key: &str) -> Option<&str> {
        self.receipt
            .iter()
            .rev()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
    fn report(&self, version: Option<&str>) -> Report {
        Report {
            changed: self.changed,
            published: version.is_some() && self.changed,
            version: version.map(str::to_owned),
            digest: DIGEST.to_owned(),
            generation: 7,
            selected: OLD.to_owned(),
            changes: Json::Arr(Vec::new()),
            profiles: Json::Arr(Vec::new()),
        }
    }
    fn selection(kind: &str, generation: u64) -> Selection {
        match kind {
            "recorded" => Selection::Recorded(generation),
            "refused" => Selection::Refused("refused".into()),
            _ => Selection::Uncertain("uncertain".into()),
        }
    }
}

impl Steps for Fake {
    fn converge(&mut self, version: Option<&str>) -> R<Result<Report, Refusal>> {
        self.log
            .push(format!("converge {}", version.unwrap_or("plan")));
        if version.is_none() {
            return Ok(Ok(self.report(None)));
        }
        match self.publish.remove(0) {
            "ok" => Ok(Ok(self.report(version))),
            "indeterminate" => Ok(Err(Refusal {
                code: INDETERMINATE.to_owned(),
                text: "unknown".to_owned(),
            })),
            _ => Ok(Err(Refusal {
                code: "source-or-material-validation".to_owned(),
                text: "refused".to_owned(),
            })),
        }
    }
    fn check(&mut self, version: &str) -> R<String> {
        self.log.push(format!("check {version}"));
        Ok(self.check.unwrap_or(DIGEST).to_owned())
    }
    fn prepare(&mut self, version: &str, generation: u64) -> R<String> {
        self.log.push(format!("prepare {version} {generation}"));
        Ok(format!("request-{version}"))
    }
    fn select(&mut self, request: &str, _: &str) -> Selection {
        self.log.push(format!("select {request}"));
        let next = 8 + u64::from(self.log.iter().filter(|l| l.starts_with("select")).count() > 1);
        Fake::selection(self.select.remove(0), next)
    }
    fn inspect(&mut self, request: &str, _: &str) -> Selection {
        self.log.push(format!("inspect {request}"));
        Fake::selection(self.inspect, 8)
    }
    fn serving(&mut self, generation: u64) -> R<()> {
        self.log.push(format!("serving {generation}"));
        Ok(())
    }
    fn verify(&mut self, version: &str, generation: u64) -> R<()> {
        self.log.push(format!("verify {version} {generation}"));
        if self.verify.remove(0) {
            Ok(())
        } else {
            Err("probe".into())
        }
    }
    fn new_version(&mut self) -> R<String> {
        Ok(NEW.to_owned())
    }
    fn restart_failed(&mut self) -> R<()> {
        self.log.push("restart failed".to_owned());
        self.retain("config_restarted_units", Json::Arr(Vec::new()))
    }
    fn retain(&mut self, key: &str, value: Json) -> R<()> {
        self.log.push(format!("retain {key}"));
        self.receipt.push((key.to_owned(), format!("{value}")));
        Ok(())
    }
    fn event(&mut self, _: &str) -> R<()> {
        Ok(())
    }
}

fn position(fake: &Fake, entry: &str) -> usize {
    fake.log
        .iter()
        .position(|l| l == entry)
        .unwrap_or_else(|| panic!("{entry} missing from {:?}", fake.log))
}

#[test]
fn identifiers_are_retained_before_their_publications_and_success_converges() {
    let mut fake = Fake::changed();
    converge(&mut fake, false, Some(DIGEST)).unwrap();
    assert_eq!(fake.get("converge_result"), Some("\"converged\""));
    assert!(position(&fake, "retain config_version") < position(&fake, &format!("converge {NEW}")));
    assert!(
        position(&fake, "retain selection_request")
            < position(&fake, &format!("select request-{NEW}"))
    );
    assert!(
        position(&fake, &format!("check {NEW}")) < position(&fake, &format!("prepare {NEW} 7"))
    );
    assert_eq!(
        fake.get("config_selected"),
        Some(format!("\"{NEW}\"").as_str())
    );
    assert_eq!(
        fake.get("config_previous_version"),
        Some(format!("\"{OLD}\"").as_str())
    );
    assert!(fake.log.contains(&format!("verify {NEW} 8")));
}

#[test]
fn nothing_to_change_or_a_plan_publishes_nothing() {
    let mut unchanged = Fake::default();
    converge(&mut unchanged, false, None).unwrap();
    assert_eq!(unchanged.get("converge_result"), Some("\"unchanged\""));
    assert_eq!(unchanged.get("config_version"), None);
    assert!(unchanged.log.contains(&"serving 7".to_owned()));
    let mut plan = Fake::changed();
    converge(&mut plan, true, None).unwrap();
    assert_eq!(plan.get("converge_result"), Some("\"planned\""));
    assert_eq!(plan.get("config_version"), None);
    assert_eq!(
        plan.log
            .iter()
            .filter(|l| l.starts_with("converge"))
            .count(),
        1
    );
    // An apply bound to a different reviewed plan stops before publishing.
    let mut stale = Fake::changed();
    assert!(converge(&mut stale, false, Some(&"b".repeat(64))).is_err());
    assert_eq!(stale.get("converge_result"), Some("\"failed\""));
    assert_eq!(stale.get("config_version"), None);
}

#[test]
fn an_uncertain_publication_is_settled_under_the_same_id() {
    let mut fake = Fake::changed();
    fake.publish = vec!["indeterminate", "ok"];
    converge(&mut fake, false, None).unwrap();
    let publishes = fake
        .log
        .iter()
        .filter(|l| *l == &format!("converge {NEW}"))
        .count();
    assert_eq!(publishes, 2);
    let mut refused = Fake::changed();
    refused.publish = vec!["refused"];
    assert!(converge(&mut refused, false, None).is_err());
    assert_eq!(refused.get("converge_result"), Some("\"failed\""));
    assert_eq!(
        refused.get("config_selected"),
        Some(format!("\"{OLD}\"").as_str())
    );
    let mut differs = Fake::changed();
    differs.check = Some("bb");
    assert!(converge(&mut differs, false, None).is_err());
    assert!(!differs.log.iter().any(|l| l.starts_with("prepare")));
}

#[test]
fn a_refused_selection_changes_nothing_and_an_uncertain_one_is_inspected_once() {
    let mut refused = Fake::changed();
    refused.select = vec!["refused"];
    assert!(converge(&mut refused, false, None).is_err());
    assert_eq!(refused.get("converge_result"), Some("\"failed\""));
    assert_eq!(
        refused.get("config_selected"),
        Some(format!("\"{OLD}\"").as_str())
    );
    assert!(!refused.log.iter().any(|l| l.starts_with("inspect")));
    // While the request is in flight the receipt names no selection.
    let retained = position(&refused, "retain selection_request");
    assert_eq!(refused.log[retained + 1], "retain config_selected");
    assert!(retained + 1 < position(&refused, &format!("select request-{NEW}")));

    let mut settled = Fake::changed();
    settled.select = vec!["uncertain", "recorded"];
    settled.inspect = "recorded";
    converge(&mut settled, false, None).unwrap();
    assert_eq!(settled.get("converge_result"), Some("\"converged\""));

    let mut unknown = Fake::changed();
    unknown.select = vec!["uncertain"];
    assert!(converge(&mut unknown, false, None).is_err());
    assert_eq!(unknown.get("converge_result"), Some("\"uncertain\""));
    assert_eq!(unknown.get("config_selected"), Some("null"));
    assert_eq!(
        unknown
            .log
            .iter()
            .filter(|l| l.starts_with("inspect"))
            .count(),
        1
    );
    assert!(!unknown.log.iter().any(|l| l.starts_with("verify")));
}

#[test]
fn a_failed_verification_restores_the_previous_version_at_the_new_generation() {
    let mut fake = Fake::changed();
    fake.verify = vec![false, true];
    let error = converge(&mut fake, false, None).unwrap_err();
    assert_eq!(error.to_string(), "probe");
    assert_eq!(fake.get("converge_result"), Some("\"restored\""));
    assert_eq!(fake.get("config_verify_error"), Some("\"probe\""));
    assert!(fake.log.contains(&format!("prepare {OLD} 8")));
    assert!(fake.log.contains(&format!("verify {OLD} 9")));
    assert!(
        position(&fake, "retain rollback_request")
            < position(&fake, &format!("select request-{OLD}"))
    );
    assert_eq!(
        fake.get("config_selected"),
        Some(format!("\"{OLD}\"").as_str())
    );

    // The restore selected but could not verify: the previous version is
    // selected and both failures are reported.
    let mut unverified = Fake::changed();
    unverified.verify = vec![false, false];
    let error = converge(&mut unverified, false, None).unwrap_err();
    assert!(error.to_string().contains("restored selection unverified"));
    assert_eq!(unverified.get("converge_result"), Some("\"failed\""));
    assert_eq!(
        unverified.get("config_selected"),
        Some(format!("\"{OLD}\"").as_str())
    );

    // The restore could not select: the converged version stays selected.
    let mut stuck = Fake::changed();
    stuck.verify = vec![false];
    stuck.select = vec!["recorded", "refused"];
    assert!(converge(&mut stuck, false, None).is_err());
    assert_eq!(stuck.get("converge_result"), Some("\"failed\""));
    assert_eq!(
        stuck.get("config_selected"),
        Some(format!("\"{NEW}\"").as_str())
    );

    let mut lost = Fake::changed();
    lost.verify = vec![false];
    lost.select = vec!["recorded", "uncertain"];
    assert!(converge(&mut lost, false, None).is_err());
    assert_eq!(lost.get("converge_result"), Some("\"uncertain\""));
    assert_eq!(lost.get("config_selected"), Some("null"));
}

#[test]
fn only_a_definite_select_refusal_means_nothing_was_applied() {
    let json = |text: &str| td_json::parse(text).unwrap();
    let recorded = json(&format!(
        r#"{{"status":"ok","recorded":true,"version":"{NEW}","generation":8}}"#
    ));
    assert_eq!(
        classify("config-select", true, Some(&recorded), NEW),
        Classified::Recorded(8)
    );
    assert_eq!(
        classify("config-selection-inspect", true, Some(&recorded), NEW),
        Classified::Recorded(8)
    );
    // Recorded for another version, or without a generation, settles nothing.
    assert!(matches!(
        classify("config-select", true, Some(&recorded), OLD),
        Classified::Uncertain(_)
    ));
    let conflict =
        json(r#"{"status":"error","stage":"selection","error":"request-or-generation-conflict"}"#);
    assert_eq!(
        classify("config-select", false, Some(&conflict), NEW),
        Classified::Refused("request-or-generation-conflict")
    );
    assert!(matches!(
        classify("config-selection-inspect", false, Some(&conflict), NEW),
        Classified::Uncertain(_)
    ));
    for code in [
        "indeterminate-retain-request",
        "request-expired-outcome-unknown",
    ] {
        let unknown = json(&format!(r#"{{"status":"error","error":"{code}"}}"#));
        assert_eq!(
            classify("config-select", false, Some(&unknown), NEW),
            Classified::Uncertain(code)
        );
    }
    let absent = json(r#"{"status":"ok","recorded":false}"#);
    assert!(matches!(
        classify("config-selection-inspect", true, Some(&absent), NEW),
        Classified::Uncertain(_)
    ));
    assert_eq!(
        classify("config-select", false, None, NEW),
        Classified::Uncertain("unreadable-output")
    );
}

#[test]
fn failed_units_are_restarted_before_a_restore_is_verified_and_after_a_failure() {
    let mut restored = Fake::changed();
    restored.verify = vec![false, true];
    assert!(converge(&mut restored, false, None).is_err());
    assert!(
        position(&restored, "restart failed") < position(&restored, &format!("verify {OLD} 9"))
    );
    assert_eq!(restored.get("config_reload_seconds"), Some("150"));
    let mut stuck = Fake::changed();
    stuck.verify = vec![false];
    stuck.select = vec!["recorded", "refused"];
    assert!(converge(&mut stuck, false, None).is_err());
    assert!(position(&stuck, "restart failed") < position(&stuck, "retain converge_result"));
    // Nothing selected changed nothing to restart.
    let mut plan = Fake::changed();
    converge(&mut plan, true, None).unwrap();
    assert!(!plan.log.contains(&"restart failed".to_owned()));
    let mut stale = Fake::changed();
    assert!(converge(&mut stale, false, Some(&"b".repeat(64))).is_err());
    assert!(!stale.log.contains(&"restart failed".to_owned()));
    let mut refused = Fake::changed();
    refused.select = vec!["refused"];
    assert!(converge(&mut refused, false, None).is_err());
    assert!(!refused.log.contains(&"restart failed".to_owned()));
}

#[test]
fn only_failed_serving_units_are_restarted() {
    let listed = "td-mta-receive.service loaded failed failed td-mta receive\n\
td-mta-health.service loaded failed failed td-mta health\n\
td-mta-frontend.service loaded failed failed td-mta frontend\n\n";
    let units: Vec<_> = failed_serving_units(listed).collect();
    assert_eq!(units, ["td-mta-receive.service", "td-mta-frontend.service"]);
    assert_eq!(failed_serving_units("").count(), 0);
}
