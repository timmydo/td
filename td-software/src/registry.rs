//! The applications the deployment ships: the registry's names
//! (`/etc/td-applications.tsv`) and the launcher table's display names
//! (`/etc/td-launcher.tsv`), both immutable tables the image builds
//! (APPLICATIONS.md §A.0). Pure: the window reads the files and hands
//! their text here. Root, not this list, decides what a request may name.

/// The registry the image installs.
pub const REGISTRY: &str = "/etc/td-applications.tsv";
/// The launcher table beside it.
pub const LAUNCHER_TABLE: &str = "/etc/td-launcher.tsv";
/// The most of either table this reads: the bound the image builder
/// writes them to (`engine/src/launcher.rs`), whose validators those
/// tables already passed.
pub const MAX_TABLE_BYTES: usize = 1024 * 1024;
/// The tables' own bound on applications.
pub const MAX_APPLICATIONS: usize = 256;
/// The longest display name, the builder's own bound.
const MAX_DISPLAY_BYTES: usize = 128;

/// One application the image declares.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Application {
    /// The registry name, which every request names.
    pub name: String,
    /// The launcher table's display name, or the name.
    pub display: String,
    /// The Flathub app ref when the declaration is a Flathub one, which
    /// td-software can install; `None` for one the image ships.
    pub flathub: Option<&'static str>,
}

/// The registry's name grammar, which root and td-app hold too: one to 64
/// lowercase letters, digits, `.`, `_` and `-`, first a letter.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.bytes().next().is_some_and(|b| b.is_ascii_lowercase())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
}

/// The applications `registry` names, with `launcher`'s display names
/// where it has them: the Flathub declarations first, which are what
/// td-software installs, then the image's own, each by display name.
pub fn applications(registry: &str, launcher: Option<&str>) -> Result<Vec<Application>, String> {
    if registry.len() > MAX_TABLE_BYTES {
        return Err("the application registry is too large".into());
    }
    let mut applications: Vec<Application> = Vec::new();
    for line in registry.lines() {
        let name = line
            .split('\t')
            .next()
            .filter(|name| valid_name(name) && line.contains('\t'))
            .ok_or_else(|| "the application registry has a malformed line".to_string())?;
        if applications.iter().any(|known| known.name == name) {
            return Err(format!("the application registry names {name} twice"));
        }
        if applications.len() >= MAX_APPLICATIONS {
            return Err("the application registry names too many applications".into());
        }
        applications.push(Application {
            name: name.to_string(),
            display: name.to_string(),
            flathub: crate::flathub_app::flathub_source(name).map(|source| source.application),
        });
    }
    // Display names are presentation only: a malformed launcher line
    // leaves the name standing rather than hiding the application.
    for line in launcher.unwrap_or_default().lines().take(MAX_APPLICATIONS) {
        let mut fields = line.split('\t');
        let (Some(name), Some(display)) = (fields.next(), fields.next()) else {
            continue;
        };
        let shown = !display.is_empty()
            && display.len() <= MAX_DISPLAY_BYTES
            && display.chars().all(|c| !c.is_control());
        if let Some(application) = applications.iter_mut().find(|a| a.name == name) {
            if shown {
                application.display = display.to_string();
            }
        }
    }
    applications.sort_by(|a, b| {
        (a.flathub.is_none(), a.display.to_lowercase(), &a.name).cmp(&(
            b.flathub.is_none(),
            b.display.to_lowercase(),
            &b.name,
        ))
    });
    Ok(applications)
}
