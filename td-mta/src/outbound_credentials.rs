//! Protected Basic credentials and HTTPS trust for explicit operator clients.
use crate::{
    operator_files::{Inputs, Role},
    store_fs::PrivateRoot,
};
use std::{io::Read, path::Path};
use td_crypto::TrustStore;

/// Deliberately has no Debug implementation. It grants no remote authority.
pub struct Loaded {
    pub authorization: String,
    pub trust: TrustStore,
}
impl Loaded {
    pub fn open(
        username: &str,
        password_file: &str,
        ca: Option<&str>,
    ) -> Result<Self, &'static str> {
        if username.is_empty()
            || username.len() > 998
            || username.contains(':')
            || username.chars().any(char::is_control)
        {
            return Err("invalid-username");
        }
        let parent = Path::new(password_file)
            .parent()
            .and_then(Path::to_str)
            .ok_or("secret-parent")?;
        let root = PrivateRoot::open(parent).map_err(|_| "secret-parent")?;
        let mut inputs = Inputs::new();
        inputs.bind(&root).map_err(|_| "secret-parent")?;
        let bytes = read(
            inputs
                .open(password_file, Role::Secret)
                .map_err(|_| "secret-file")?,
            1024,
        )?;
        let password = std::str::from_utf8(&bytes).map_err(|_| "invalid-password")?;
        let password = password
            .strip_suffix("\r\n")
            .or_else(|| password.strip_suffix('\n'))
            .unwrap_or(password);
        if password.is_empty() || password.chars().any(char::is_control) {
            return Err("invalid-password");
        }
        let authorization = format!(
            "Basic {}",
            crate::acme::wire::base64(format!("{username}:{password}").as_bytes(), false)
                .map_err(|_| "credential-capacity")?
        );
        let trust = match ca {
            Some(path) => TrustStore::from_pem(&read(
                inputs.open(path, Role::Public).map_err(|_| "trust-file")?,
                128 * 1024,
            )?),
            None => TrustStore::public_roots(),
        }
        .map_err(|_| "trust")?;
        Ok(Self {
            authorization,
            trust,
        })
    }
}
fn read(input: impl Read, limit: usize) -> Result<Vec<u8>, &'static str> {
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(limit + 1).map_err(|_| "capacity")?;
    input
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "read")?;
    if bytes.len() > limit {
        return Err("capacity");
    }
    Ok(bytes)
}
