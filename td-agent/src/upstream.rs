//! What the window keeps of upstream (DESIGN.md §7, Keeping current):
//! which stores the live repository workspaces use, fetched in the
//! background every `fetch_interval`, and the commit each base they name
//! was at when its store was last fetched.

use std::collections::BTreeMap;

use crate::git::{Admission, Remote};
use crate::store::Meta;
use crate::workspace::Workspace;

/// Each admitted remote a repository workspace not archived and not
/// gone with its archive names, with every base those workspaces name
/// on it, once each, in the order first named, but those whose fetch is
/// still `pending`. One being deleted or archived counts until it is:
/// the store is shared, so a fetch for it costs nothing more.
pub fn in_use(
    metas: &[Meta],
    admitted: &[Admission],
    pending: &[String],
) -> Vec<(Remote, Vec<String>)> {
    let mut used: Vec<(String, Vec<String>)> = Vec::new();
    for meta in metas.iter().filter(|meta| !meta.archived && !meta.removed) {
        let Some(Workspace::Repositories(repositories)) = &meta.workspace else {
            continue;
        };
        for entry in &repositories.entries {
            match used.iter_mut().find(|(remote, _)| *remote == entry.remote) {
                Some((_, bases)) if bases.contains(&entry.base) => {}
                Some((_, bases)) => bases.push(entry.base.clone()),
                None => used.push((entry.remote.clone(), vec![entry.base.clone()])),
            }
        }
    }
    used.into_iter()
        .filter_map(|(remote, bases)| {
            let remote = Remote::parse(&remote).ok()?;
            if pending.contains(&remote.url()) {
                return None;
            }
            admitted
                .iter()
                .any(|admission| admission.admits(&remote))
                .then_some((remote, bases))
        })
        .collect()
}

/// The commit each base the window has seen was at when its store was
/// last fetched, in memory only: a cache of what the store said, which
/// says what moved while the window runs and nothing of before it; a
/// conversation compares a base with its own record of it.
#[derive(Debug, Default)]
pub struct Heads {
    at: BTreeMap<(String, String), String>,
}

impl Heads {
    /// Learns `remote`'s `bases` at `ids`, a base with none left as it
    /// was: each base that was known before and is now at another
    /// commit, with that commit.
    pub fn learn<'a>(
        &mut self,
        remote: &str,
        bases: &[String],
        ids: impl IntoIterator<Item = Option<&'a str>>,
    ) -> Vec<(String, String)> {
        let mut moved = Vec::new();
        for (base, id) in bases.iter().zip(ids) {
            let Some(id) = id else {
                continue;
            };
            let key = (remote.to_string(), base.clone());
            if let Some(was) = self.at.insert(key, id.to_string()) {
                if was != id {
                    moved.push((base.clone(), id.to_string()));
                }
            }
        }
        moved
    }

    /// The commit `remote`'s `base` was last known at.
    pub fn at(&self, remote: &str, base: &str) -> Option<&str> {
        self.at
            .get(&(remote.to_string(), base.to_string()))
            .map(String::as_str)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::store::{Id, Role};

    fn meta(remotes: &[(&str, &str)], archived: bool, removed: bool) -> Meta {
        let template = crate::config::Template {
            network: None,
            name: "td".into(),
            repos: remotes
                .iter()
                .enumerate()
                .map(|(at, (remote, base))| crate::config::Repo {
                    remote: (*remote).into(),
                    base: (*base).into(),
                    branch: format!("agent-{at}"),
                    sparse: None,
                })
                .collect(),
            shared: None,
        };
        let admitted = [
            Admission::parse("example.org").unwrap(),
            Admission::parse("elsewhere.org").unwrap(),
        ];
        let made = crate::workspace::repositories(
            &template,
            &Id::random().unwrap(),
            std::path::Path::new("/d"),
            std::path::Path::new("/t"),
            &admitted,
            0,
        )
        .unwrap();
        bare(Some(Workspace::Repositories(made)), archived, removed)
    }

    fn bare(workspace: Option<Workspace>, archived: bool, removed: bool) -> Meta {
        Meta {
            id: Id::random().unwrap(),
            role: Role::Conversation,
            title: String::new(),
            created: 0,
            paused: false,
            model: None,
            effort: None,
            workspace,
            archived,
            prepared: Vec::new(),
            removed,
            tracked: Vec::new(),
        }
    }

    /// Each remote once, its bases once each, from live workspaces only,
    /// and only while admitted.
    #[test]
    fn the_stores_in_use_are_the_live_workspaces_admitted_remotes() {
        let metas = [
            meta(
                &[
                    ("https://example.org/a/td", "main"),
                    ("https://example.org/a/td", "next"),
                ],
                false,
                false,
            ),
            meta(
                &[
                    ("https://example.org/a/td", "main"),
                    ("https://elsewhere.org/b/x", "trunk"),
                ],
                false,
                false,
            ),
            meta(&[("https://example.org/a/old", "main")], true, false),
            meta(&[("https://example.org/a/gone", "main")], false, true),
            bare(None, false, false),
        ];
        let admitted = [Admission::parse("example.org").unwrap()];
        let used: Vec<(String, Vec<String>)> = in_use(&metas, &admitted, &[])
            .into_iter()
            .map(|(remote, bases)| (remote.url(), bases))
            .collect();
        assert_eq!(
            used,
            [(
                "https://example.org/a/td".to_string(),
                vec!["main".to_string(), "next".to_string()]
            )]
        );
        // One whose fetch has not answered is not asked again.
        assert!(in_use(&metas, &admitted, &["https://example.org/a/td".into()]).is_empty());
    }

    /// A base first learnt is no advance; one at another commit is; one
    /// with no commit this time keeps what was known.
    #[test]
    fn a_base_advances_only_from_a_known_commit() {
        let mut heads = Heads::default();
        let bases = ["main".to_string(), "next".to_string()];
        let (a, b, c) = ("a".repeat(40), "b".repeat(40), "c".repeat(40));
        let remote = "https://example.org/a/td";
        assert!(heads
            .learn(remote, &bases, [Some(a.as_str()), Some(b.as_str())])
            .is_empty());
        assert!(heads
            .learn(remote, &bases, [Some(a.as_str()), None])
            .is_empty());
        assert_eq!(heads.at(remote, "next"), Some(b.as_str()));
        assert_eq!(
            heads.learn(remote, &bases, [Some(c.as_str()), Some(b.as_str())]),
            [("main".to_string(), c.clone())]
        );
        assert_eq!(heads.at(remote, "main"), Some(c.as_str()));
        assert_eq!(heads.at("https://example.org/a/other", "main"), None);
    }
}
