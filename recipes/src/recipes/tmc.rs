use crate::types::{CargoSourcePatch, Recipe, TextEdit};

/// Timmy's Mail Console — `tmc`, the JMAP terminal mail client that the `mail`
/// application packages. The source is one upstream commit archive pinned by
/// SHA-256, and its registry closure is the committed
/// `recipes/locks/tmc/Cargo.lock`, which must equal the lock the archive
/// ships. The manifest sits at the archive root; `cargo_subdir(".")` names
/// that root explicitly, as every fixed-output archive must. The binary links
/// fully static because the `mail` application package validates it as a
/// static entry on the empty runtime: the jail shows an application no
/// `/td/store` loader.
pub fn recipe() -> Recipe {
    Recipe::rust("tmc", "0.1.0-g8f04e38")
        .source_input("tmc-source")
        .native_inputs(&[
            "rust-toolchain",
            "gcc-x86-64-self",
            "binutils-x86-64-self",
            "glibc-x86-64",
            "busybox-x86-64",
        ])
        .cargo_subdir(".")
        .cargo_lock("recipes/locks/tmc/Cargo.lock")
        .cargo_source_patches(portal_patches())
        .static_link()
        .bins(&["tmc"])
}

fn portal_patches() -> Vec<CargoSourcePatch> {
    vec![
        CargoSourcePatch::new(
            "src/config.rs",
            vec![
                TextEdit::new(
                    r###"fn password_source(
    password_command: Option<String>,
    password_file: Option<String>,
    section: &str,
) -> Result<PasswordSource, ConfigError> {
    match (password_command, password_file) {
        (Some(command), None) => Ok(PasswordSource::Command(command)),
        (None, Some(file)) => Ok(PasswordSource::File(file)),
        (Some(_), Some(_)) => Err(ConfigError::Parse(format!(
            "both password_command and password_file set in {}; choose one",
            section
        ))),
        (None, None) => Err(ConfigError::Parse(format!(
            "missing password_command or password_file in {}",
            section
        ))),
    }
}

"###,
                    r###"fn password_source(
    password_command: Option<String>,
    secret: Option<String>,
    section: &str,
) -> Result<PasswordSource, ConfigError> {
    match (password_command, secret.as_deref()) {
        (Some(command), None) => Ok(PasswordSource::Command(command)),
        (None, Some("portal")) => {
            let name = section.strip_prefix("[account.").and_then(|s| s.strip_suffix(']')).unwrap_or("default");
            if name.is_empty() || name.len() > 64 || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
                return Err(ConfigError::Parse("invalid portal credential name".into()));
            }
            Ok(PasswordSource::Portal(name.to_string()))
        }
        _ => Err(ConfigError::Parse(format!("{} missing password_command or secret, or both set; use secret = \"portal\"", section))),
    }
}

"###,
                    1,
                ),
                TextEdit::new(r###"    File(String),"###, r###"    Portal(String),"###, 1),
                TextEdit::new(r###"password_file"###, r###"secret"###, 8),
                TextEdit::new(
                    r###"secret = "/home/td/.config/tmc/password""###,
                    r###"secret = "portal""###,
                    1,
                ),
                TextEdit::new(
                    r###"PasswordSource::File("/home/td/.config/tmc/password".to_string())"###,
                    r###"PasswordSource::Portal("td".to_string())"###,
                    1,
                ),
                TextEdit::new(
                    "both password_command and secret",
                    "missing password_command or secret",
                    1,
                ),
            ],
        ),
        CargoSourcePatch::new(
            "src/main.rs",
            vec![
                TextEdit::new(
                    r###"/// Read a password file as `password_file` names it: the whole file with
/// trailing newlines removed, so a file written by `echo` or an editor works.
pub fn read_password_file(path: &str) -> Result<String, String> {
    let bytes = std::fs::read(path)
        .map_err(|e| format!("failed to read password file {}: {}", path, e))?;
    let password = String::from_utf8(bytes)
        .map_err(|e| format!("password file {} is not valid UTF-8: {}", path, e))?;
    Ok(password.trim_end_matches('\n').to_string())
}

"###,
                    r###"/// The td-owned helper receives the credential over D-Bus and an fd.
pub fn read_portal_credential(name: &str) -> Result<String, String> {
    let output = Command::new("/app/bin/td-secret").args(["get", name]).output()
        .map_err(|e| format!("credential portal: {e}"))?;
    if !output.status.success() { return Err(format!("credential portal: {}", String::from_utf8_lossy(&output.stderr).trim())); }
    let password = String::from_utf8(output.stdout).map_err(|_| "credential is not UTF-8")?;
    Ok(password.trim_end_matches('\n').to_string())
}

"###,
                    1,
                ),
                TextEdit::new(
                    r###"PasswordSource::File(path) => read_password_file(path),"###,
                    r###"PasswordSource::Portal(name) => read_portal_credential(name),"###,
                    1,
                ),
                TextEdit::new(
                    r###"`password_command` or `password_file`"###,
                    r###"`password_command` or `secret`"###,
                    1,
                ),
                TextEdit::new(
                    r###"- `password_file` is a path whose contents (minus trailing newlines) are the password; it needs no shell."###,
                    r###"- `secret = "portal"` retrieves mail/NAME from the credential portal for [account.NAME]."###,
                    1,
                ),
                TextEdit::new(
                    r###"password_command = "pass show email/example.com""###,
                    r###"secret = "portal""###,
                    1,
                ),
                TextEdit::new(
                    r###"password_command = "pass show email/work.com""###,
                    r###"secret = "portal""###,
                    1,
                ),
                TextEdit::new(
                    r###"password_command = \"pass show email/example\"     # Shell command returning password (required)"###,
                    r###"secret = \"portal\"              # Credential stored as mail/NAME"###,
                    1,
                ),
                TextEdit::new(
                    r###"password_command = \"pass show email/example.com\""###,
                    r###"secret = \"portal\""###,
                    1,
                ),
            ],
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::recipe;

    /// The pinned upstream commit; the version suffix and the pin's URL and
    /// file name must all name it.
    const TMC_COMMIT: &str = "8f04e380bd378f735152fae18e1bd1c189b6eddf";

    #[test]
    fn tmc_is_a_root_workspace_rust_recipe_pinned_to_one_commit() {
        let recipe = recipe();
        assert_eq!(recipe.source_input.as_deref(), Some("tmc-source"));
        assert_eq!(recipe.cargo_subdir.as_deref(), Some("."));
        assert_eq!(
            recipe.cargo_lock.as_deref(),
            Some("recipes/locks/tmc/Cargo.lock")
        );
        assert_eq!(recipe.bins, Some(vec!["tmc".to_string()]));
        assert_eq!(recipe.static_link, Some(true));
        assert!(recipe.version.ends_with(&TMC_COMMIT[..7]));
        let pin = crate::source_pins::by_key("tmc-source").expect("tmc-source pin");
        assert!(pin.url.contains(TMC_COMMIT), "{}", pin.url);
        assert!(pin.file.contains(TMC_COMMIT), "{}", pin.file);
    }
}
