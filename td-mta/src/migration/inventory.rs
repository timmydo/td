//! Independent account-view ID coverage at the retained source states.
use super::*;
use std::collections::BTreeSet;

pub(super) fn run(
    archive: &archive::Archive,
    session: &str,
    account: &str,
    username: &str,
    password: &str,
    ca: Option<&str>,
) -> Result<Json> {
    let manifest = archive.snapshot()?;
    let binding = field(&manifest, "source")?;
    for (key, expected) in [
        ("session", session),
        ("account", account),
        ("username", username),
    ] {
        if string(binding, key)? != expected {
            return Err("migration-source-mismatch");
        }
    }
    let source = source::Source::open(session, account, username, password, ca)?;
    states(
        &manifest,
        &source.state("Mailbox")?,
        &source.state("Email")?,
    )?;
    let ids = source.query("Email", None, OBJECT_LIMIT)?;
    matches(&manifest, &ids)?;
    states(
        &manifest,
        &source.state("Mailbox")?,
        &source.state("Email")?,
    )?;
    Ok(
        json!({"format":1,"status":"inventory-matches","source":binding,
        "manifestSha256":digest(manifest.to_string().as_bytes())?,
        "emailState":field(&manifest,"emailState")?,
        "mailboxState":field(&manifest,"mailboxState")?,
        "messages":ids.len(),"coverage":"authenticated-account-query",
        "filter":{"minSize":0},"exactEmailIds":true,
        "bodyDigestsVerified":false,"cutoverReady":false}),
    )
}
fn states(manifest: &Json, mailboxes: &str, emails: &str) -> Result<()> {
    if string(manifest, "mailboxState")? != mailboxes || string(manifest, "emailState")? != emails {
        return Err("migration-source-changed");
    }
    Ok(())
}
fn matches(manifest: &Json, ids: &[String]) -> Result<()> {
    let expected: BTreeSet<&str> = array(manifest, "messages")?
        .iter()
        .map(|record| string(field(record, "email")?, "id"))
        .collect::<Result<_>>()?;
    let observed: BTreeSet<&str> = ids.iter().map(String::as_str).collect();
    if expected.len() != ids.len() || observed != expected {
        return Err("migration-inventory-mismatch");
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    #[test]
    fn inventory_requires_the_exact_set_and_both_retained_states() {
        let manifest = json!({"mailboxState":"m", "emailState":"e",
            "messages":[{"email":{"id":"one"}},{"email":{"id":"two"}}]});
        assert!(matches(&manifest, &["two".into(), "one".into()]).is_ok());
        for ids in [
            vec![],
            vec!["one"],
            vec!["one", "other"],
            vec!["one", "two", "three"],
            vec!["one", "one"],
            vec!["one", "two", "two"],
        ] {
            assert_eq!(
                matches(
                    &manifest,
                    &ids.into_iter().map(String::from).collect::<Vec<_>>()
                ),
                Err("migration-inventory-mismatch")
            );
        }
        assert!(states(&manifest, "m", "e").is_ok());
        assert_eq!(
            states(&manifest, "changed", "e"),
            Err("migration-source-changed")
        );
        assert_eq!(
            states(&manifest, "m", "changed"),
            Err("migration-source-changed")
        );
        assert!(matches(&json!({"messages":[]}), &[]).is_ok());
    }
}
