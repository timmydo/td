//! Canonical per-user service and application identities and retained reservations.
//! Parsing and enrollment perform no I/O; callers establish file authority.

use std::collections::{BTreeMap, BTreeSet};

const MAGIC: &str = "td-principals-v1\n";
pub const MAX_BYTES: usize = 64 * 1024;
const MAX_ROWS: usize = 256;

/// Assignment data, not an authority token. Use a validated Registry lookup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Session {
    pub owner: u32,
    pub compositor: u32,
    pub broker: u32,
    pub portal: u32,
}

/// Assignment data, not an authority token. Use a validated Registry lookup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Application {
    pub owner: u32,
    pub name: String,
    pub uid: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Registry {
    sessions: BTreeMap<u32, Session>,
    applications: BTreeMap<(u32, String), Application>,
}

pub(crate) fn decimal(value: &str, range: std::ops::RangeInclusive<u32>) -> Result<u32, String> {
    let number = value.parse::<u32>().map_err(|_| "invalid principal uid")?;
    if !range.contains(&number) || number.to_string() != value {
        return Err("noncanonical or out-of-range principal uid".into());
    }
    Ok(number)
}

fn name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"._-".contains(&byte)
        })
}

impl Registry {
    pub fn sessions(&self) -> impl Iterator<Item = &Session> {
        self.sessions.values()
    }

    pub fn applications(&self) -> impl Iterator<Item = &Application> {
        self.applications.values()
    }

    pub fn parse(text: &str) -> Result<Self, String> {
        if text.len() > MAX_BYTES || !text.ends_with('\n') || !text.is_ascii() {
            return Err("principal table is oversized or not canonical ASCII lines".into());
        }
        let rows = text
            .strip_prefix(MAGIC)
            .ok_or("unsupported principal table format")?;
        let mut registry = Self {
            sessions: BTreeMap::new(),
            applications: BTreeMap::new(),
        };
        let mut assigned = BTreeSet::new();
        let mut previous_session = None;
        let mut previous_application = None;
        for (index, row) in rows.split_terminator('\n').enumerate() {
            if index >= MAX_ROWS {
                return Err("principal table exceeds 256 rows".into());
            }
            let fields: Vec<&str> = row.split('\t').take(6).collect();
            match fields.as_slice() {
                ["session", owner, compositor, broker, portal] => {
                    let owner = decimal(owner, 1000..=65533)?;
                    let compositor = decimal(compositor, 1..=999)?;
                    let broker = decimal(broker, 1..=999)?;
                    let portal = decimal(portal, 1..=999)?;
                    if previous_application.is_some()
                        || previous_session.is_some_and(|previous| previous >= owner)
                    {
                        return Err("principal sessions are duplicate or unsorted".into());
                    }
                    for uid in [compositor, broker, portal] {
                        if !assigned.insert(uid) {
                            return Err("principal uid is assigned more than once".into());
                        }
                    }
                    registry.sessions.insert(
                        owner,
                        Session {
                            owner,
                            compositor,
                            broker,
                            portal,
                        },
                    );
                    previous_session = Some(owner);
                }
                ["application", owner, application, uid] => {
                    let owner = decimal(owner, 1000..=65533)?;
                    let uid = decimal(uid, 65536..=2147483647)?;
                    if !registry.sessions.contains_key(&owner) {
                        return Err("application has no declared human session".into());
                    }
                    if !name(application) {
                        return Err("invalid principal application name".into());
                    }
                    let key = (owner, (*application).to_string());
                    if previous_application
                        .as_ref()
                        .is_some_and(|previous| previous >= &key)
                    {
                        return Err("principal applications are duplicate or unsorted".into());
                    }
                    if !assigned.insert(uid) {
                        return Err("principal uid is assigned more than once".into());
                    }
                    registry.applications.insert(
                        key.clone(),
                        Application {
                            owner,
                            name: (*application).to_string(),
                            uid,
                        },
                    );
                    previous_application = Some(key);
                }
                _ => return Err("invalid principal row shape".into()),
            }
        }
        if registry.sessions.is_empty() {
            return Err("principal table needs at least one session".into());
        }
        Ok(registry)
    }

    pub fn session(&self, owner: u32) -> Option<&Session> {
        self.sessions.get(&owner)
    }

    #[cfg(test)]
    pub fn application(&self, owner: u32, name: &str) -> Option<&Application> {
        self.applications.get(&(owner, name.to_string()))
    }

    #[cfg(test)]
    pub fn application_for_uid(&self, uid: u32) -> Option<&Application> {
        self.applications
            .values()
            .find(|application| application.uid == uid)
    }

    /// Preserve retired assignments so a later deployment cannot recycle them.
    pub fn enroll(&self, desired: &Self) -> Result<Self, String> {
        let mut retained = self.clone();
        for (owner, session) in &desired.sessions {
            if self
                .sessions
                .get(owner)
                .is_some_and(|prior| prior != session)
            {
                return Err("deployment changes an enrolled session identity".into());
            }
            retained.sessions.insert(*owner, session.clone());
        }
        for (key, application) in &desired.applications {
            if self
                .applications
                .get(key)
                .is_some_and(|prior| prior != application)
            {
                return Err("deployment changes an enrolled application identity".into());
            }
            retained
                .applications
                .insert(key.clone(), application.clone());
        }
        // Validate aggregate bounds and cross-principal collisions in the union.
        Self::parse(&retained.encode())
    }

    pub fn encode(&self) -> String {
        let mut text = MAGIC.to_string();
        for session in self.sessions.values() {
            text.push_str(&format!(
                "session\t{}\t{}\t{}\t{}\n",
                session.owner, session.compositor, session.broker, session.portal
            ));
        }
        for application in self.applications.values() {
            text.push_str(&format!(
                "application\t{}\t{}\t{}\n",
                application.owner, application.name, application.uid
            ));
        }
        text
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    const TABLE: &str = "td-principals-v1\nsession\t1000\t993\t991\t989\nsession\t1001\t992\t990\t988\napplication\t1000\tfirefox\t65536\napplication\t1000\tmail\t65538\napplication\t1001\tmail\t65537\n";

    #[test]
    fn application_identity_is_scoped_to_its_human_and_exact_name() {
        let registry = Registry::parse(TABLE).unwrap();
        assert_eq!(registry.session(1000).unwrap().compositor, 993);
        assert_eq!(registry.session(1000).unwrap().broker, 991);
        assert_eq!(registry.session(1000).unwrap().portal, 989);
        assert_eq!(registry.application(1000, "mail").unwrap().uid, 65538);
        assert_eq!(registry.application(1001, "mail").unwrap().uid, 65537);
        assert!(registry.application(1001, "firefox").is_none());
        assert!(registry.application(1000, "Mail").is_none());
        assert!(registry.session(1002).is_none());
        assert_eq!(registry.application_for_uid(65537).unwrap().owner, 1001);
        assert!(registry.application_for_uid(1000).is_none());
        assert!(registry.application_for_uid(993).is_none());
    }

    #[test]
    fn adding_an_earlier_name_does_not_renumber_existing_assignments() {
        let before = Registry::parse(TABLE).unwrap();
        let after = Registry::parse(&TABLE.replace(
            "application\t1000\tfirefox",
            "application\t1000\talpha\t65539\napplication\t1000\tfirefox",
        ))
        .unwrap();
        for (owner, name) in [(1000, "firefox"), (1000, "mail"), (1001, "mail")] {
            assert_eq!(
                before.application(owner, name),
                after.application(owner, name)
            );
        }
    }

    #[test]
    fn duplicate_or_ambiguous_bindings_are_refused() {
        for altered in [
            TABLE.replace(
                "session\t1001\t992\t990\t988",
                "session\t1001\t993\t990\t988",
            ),
            TABLE.replace(
                "session\t1001\t992\t990\t988",
                "session\t1000\t992\t990\t988",
            ),
            TABLE.replace("mail\t65538", "mail\t65536"),
            TABLE.replace("\t991\t", "\t993\t"),
            TABLE.replace("\t989\n", "\t990\n"),
            TABLE.replace("application\t1000\tmail", "application\t1000\tfirefox"),
            TABLE.replace("application\t1001\tmail", "application\t1002\tmail"),
        ] {
            assert!(Registry::parse(&altered).is_err(), "{altered}");
        }
    }

    #[test]
    fn ordering_is_numeric_then_name_and_sessions_cannot_follow_applications() {
        for altered in [
            TABLE.replace(
                "session\t1000\t993\t991\t989\nsession\t1001\t992\t990\t988",
                "session\t1001\t992\t990\t988\nsession\t1000\t993\t991\t989",
            ),
            TABLE.replace("application\t1000\tfirefox", "application\t1000\tzebra"),
            format!("{TABLE}session\t1002\t987\t986\t985\n"),
            TABLE.replace("application\t1001\tmail", "application\t1000\taardvark"),
        ] {
            assert!(Registry::parse(&altered).is_err(), "{altered}");
        }
    }

    #[test]
    fn canonical_ids_exclude_root_overflow_and_cross_role_aliases() {
        for value in [
            "0",
            "999",
            "65534",
            "65535",
            "65536",
            "+1000",
            "01000",
            " 1000",
            "-1",
            "4294967296",
        ] {
            assert!(
                Registry::parse(&TABLE.replace("1000", value)).is_err(),
                "owner {value}"
            );
        }
        for value in ["0", "1000", "65534", "65536", "+993", "0993"] {
            assert!(
                Registry::parse(&TABLE.replace("993", value)).is_err(),
                "compositor {value}"
            );
        }
        for value in [
            "0",
            "993",
            "1000",
            "65534",
            "65535",
            "2147483648",
            "+65536",
            "065536",
        ] {
            assert!(
                Registry::parse(&TABLE.replace("65536", value)).is_err(),
                "app {value}"
            );
        }
    }

    #[test]
    fn malformed_names_framing_and_versions_are_refused() {
        for value in [
            "",
            ".mail",
            "-mail",
            "Mail",
            "mail/name",
            "mail name",
            "máil",
            "mail\tother",
            "mail\0",
        ] {
            assert!(
                Registry::parse(&TABLE.replace("firefox", value)).is_err(),
                "name {value:?}"
            );
        }
        for altered in [
            String::new(),
            MAGIC.to_string(),
            TABLE.trim_end().to_string(),
            TABLE.replace('\n', "\r\n"),
            TABLE.replacen("v1", "v2", 1),
            format!("{TABLE}\n"),
            TABLE.replace("993", "993\textra"),
            TABLE.replace("65536", "65536\textra"),
            TABLE.replace("firefox", &"a".repeat(65)),
        ] {
            assert!(Registry::parse(&altered).is_err(), "{altered:?}");
        }
    }

    #[test]
    fn aggregate_row_and_byte_bounds_are_enforced() {
        let mut text = format!("{MAGIC}session\t1000\t993\t991\t989\n");
        for index in 0..255 {
            text.push_str(&format!(
                "application\t1000\tapp{index:03}\t{}\n",
                65536 + index
            ));
        }
        assert!(Registry::parse(&text).is_ok());
        text.push_str("application\t1000\tapp255\t65791\n");
        assert!(Registry::parse(&text).is_err());
        assert!(Registry::parse(&format!("{TABLE}{}\n", "a".repeat(MAX_BYTES))).is_err());
    }

    #[test]
    fn enrollment_preserves_retired_assignments_and_refuses_reuse() {
        let prior = Registry::parse(TABLE).unwrap();
        let retired =
            Registry::parse(&TABLE.replace("application\t1000\tmail\t65538\n", "")).unwrap();
        assert_eq!(prior.enroll(&retired).unwrap(), prior);
        let recycled = Registry::parse(&TABLE.replace(
            "application\t1000\tmail\t65538",
            "application\t1000\tnews\t65538",
        ))
        .unwrap();
        assert!(prior.enroll(&recycled).is_err());
        let renumbered = Registry::parse(&TABLE.replace("mail\t65538", "mail\t65539")).unwrap();
        assert!(prior.enroll(&renumbered).is_err());
        let new_compositor = Registry::parse(&TABLE.replace("993", "987")).unwrap();
        assert!(prior.enroll(&new_compositor).is_err());
    }

    #[test]
    fn enrollment_is_idempotent_and_serializes_in_canonical_order() {
        let prior = Registry::parse(TABLE).unwrap();
        assert_eq!(prior.encode(), TABLE);
        assert_eq!(prior.enroll(&prior).unwrap(), prior);
        let expanded = Registry::parse(&TABLE.replace(
            "application\t1000\tfirefox",
            "application\t1000\talpha\t65539\napplication\t1000\tfirefox",
        ))
        .unwrap();
        let combined = prior.enroll(&expanded).unwrap();
        assert_eq!(combined, expanded);
        assert_eq!(combined.enroll(&prior).unwrap(), combined);
        assert_eq!(Registry::parse(&combined.encode()).unwrap(), combined);
    }
}
