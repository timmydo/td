//! Bounded operator edits of immutable configuration sources, before validation.
use crate::{
    config::{
        syntax::{self, Statement, Value},
        values,
    },
    ids::{AccountId, IdentityId},
};
use std::num::NonZeroU32;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Syntax,
    Capacity,
    Missing,
    Conflict,
    Invalid,
}

pub enum Edit<'a> {
    Create {
        account: AccountId,
        identity: IdentityId,
        address: &'a str,
        name: &'a str,
        username: &'a str,
    },
    Update {
        account: AccountId,
        name: Option<&'a str>,
        username: Option<&'a str>,
    },
    IdentityAdd {
        account: AccountId,
        identity: IdentityId,
        address: &'a str,
        name: &'a str,
    },
    IdentityUpdate {
        account: AccountId,
        identity: IdentityId,
        address: Option<&'a str>,
        name: Option<&'a str>,
    },
    IdentityRemove {
        account: AccountId,
        identity: IdentityId,
    },
    Alias {
        account: AccountId,
        address: &'a str,
        remove: bool,
    },
}
impl Edit<'_> {
    pub fn account(&self) -> AccountId {
        match self {
            Self::Create { account, .. }
            | Self::Update { account, .. }
            | Self::Alias { account, .. }
            | Self::IdentityAdd { account, .. }
            | Self::IdentityUpdate { account, .. }
            | Self::IdentityRemove { account, .. } => *account,
        }
    }
}

fn walk(
    source: &[u8],
    mut visit: impl FnMut(&[u8], Statement<'_>) -> Result<(), Error>,
) -> Result<(), Error> {
    if source.len() > syntax::MAX_INPUT_BYTES {
        return Err(Error::Capacity);
    }
    let mut scratch = [0; syntax::MAX_STRING_BYTES];
    for (index, line) in source.split_inclusive(|b| *b == b'\n').enumerate() {
        let number = u32::try_from(index)
            .ok()
            .and_then(|n| n.checked_add(1))
            .filter(|n| *n <= syntax::MAX_LINES)
            .and_then(NonZeroU32::new)
            .ok_or(Error::Capacity)?;
        let statement =
            syntax::parse_line(number, line, &mut scratch).map_err(|_| Error::Syntax)?;
        visit(line, statement)?;
    }
    Ok(())
}
fn append(out: &mut Vec<u8>, bytes: &[u8]) -> Result<(), Error> {
    if out
        .len()
        .checked_add(bytes.len())
        .filter(|n| *n <= syntax::MAX_INPUT_BYTES)
        .is_none()
    {
        return Err(Error::Capacity);
    }
    out.try_reserve(bytes.len()).map_err(|_| Error::Capacity)?;
    out.extend_from_slice(bytes);
    Ok(())
}
fn quoted(out: &mut Vec<u8>, value: &str) -> Result<(), Error> {
    if value.len() > syntax::MAX_STRING_BYTES || value.chars().any(char::is_control) {
        return Err(Error::Invalid);
    }
    append(out, b"\"")?;
    for byte in value.bytes() {
        if matches!(byte, b'"' | b'\\') {
            append(out, b"\\")?;
        }
        append(out, &[byte])?;
    }
    append(out, b"\"")
}
fn field(out: &mut Vec<u8>, key: &[u8], value: &str) -> Result<(), Error> {
    append(out, key)?;
    append(out, b" = ")?;
    quoted(out, value)?;
    append(out, b"\n")
}
fn section(out: &mut Vec<u8>, kind: &[u8], label: &str) -> Result<(), Error> {
    append(out, b"\n[")?;
    append(out, kind)?;
    append(out, b" ")?;
    quoted(out, label)?;
    append(out, b"]\n")
}
fn same_address(a: &str, b: &str) -> Result<bool, Error> {
    let mut left = [0; crate::format::row::MAX_ADDRESS];
    let mut right = [0; crate::format::row::MAX_ADDRESS];
    Ok(
        values::mailbox_key(a, &mut left).map_err(|_| Error::Invalid)?
            == values::mailbox_key(b, &mut right).map_err(|_| Error::Invalid)?,
    )
}

/// Only account headers are inspected here; complete validation follows editing.
pub fn accounts(source: &[u8]) -> Result<Vec<AccountId>, Error> {
    let mut ids = Vec::new();
    ids.try_reserve_exact(crate::config::identities::MAX_ACCOUNTS)
        .map_err(|_| Error::Capacity)?;
    walk(source, |_, statement| {
        if let Statement::Section {
            name: "account",
            label: Some(label),
            ..
        } = statement
        {
            if ids.len() == crate::config::identities::MAX_ACCOUNTS {
                return Err(Error::Capacity);
            }
            let id = AccountId::parse(label).map_err(|_| Error::Invalid)?;
            if ids.contains(&id) {
                return Err(Error::Conflict);
            }
            ids.push(id);
        }
        Ok(())
    })?;
    Ok(ids)
}

/// Preserve unrelated bytes, comments and stanza order. Result has no authority
/// until every profile passes the ordinary complete candidate validator.
pub fn edit(source: &[u8], edit: &Edit<'_>) -> Result<Vec<u8>, Error> {
    let ids = accounts(source)?;
    let account = edit.account();
    let exists = ids.contains(&account);
    if matches!(edit, Edit::Create { .. }) {
        if exists {
            return Err(Error::Conflict);
        }
        if ids.len() == crate::config::identities::MAX_ACCOUNTS {
            return Err(Error::Capacity);
        }
    } else if !exists {
        return Err(Error::Missing);
    }
    match edit {
        Edit::IdentityAdd { identity, .. }
        | Edit::IdentityUpdate { identity, .. }
        | Edit::IdentityRemove { identity, .. } => return edit_identity(source, edit, *identity),
        _ => {}
    }
    let account_text = account.to_string();
    let mut out = Vec::new();
    let mut target_account = false;
    let mut skip_alias = false;
    let mut alias_found = false;
    let mut alias_owner = false;
    walk(source, |line, statement| {
        if let Statement::Section { name, label, .. } = statement {
            target_account = name == "account"
                && label.and_then(|value| AccountId::parse(value).ok()) == Some(account);
            skip_alias = false;
            if let Edit::Alias {
                address, remove, ..
            } = edit
            {
                if name == "alias"
                    && label
                        .map(|label| same_address(label, address))
                        .transpose()?
                        .unwrap_or(false)
                {
                    if alias_found || !remove {
                        return Err(Error::Conflict);
                    }
                    alias_found = true;
                    skip_alias = true;
                }
            }
            if target_account {
                if let Edit::Update { name, username, .. } = edit {
                    append(&mut out, line)?;
                    if !line.ends_with(b"\n") {
                        append(&mut out, b"\n")?;
                    }
                    if let Some(name) = name {
                        field(&mut out, b"name", name)?;
                    }
                    if let Some(username) = username {
                        field(&mut out, b"username", username)?;
                    }
                    return Ok(());
                }
            }
        }
        if skip_alias {
            if let Statement::Assignment {
                key: "account",
                value: Value::Text(owner),
                ..
            } = statement
            {
                if AccountId::parse(owner).ok() != Some(account) {
                    return Err(Error::Conflict);
                }
                alias_owner = true;
            }
            return Ok(());
        }
        if target_account {
            if let (Edit::Update { name, username, .. }, Statement::Assignment { key, .. }) =
                (edit, statement)
            {
                if (key == "name" && name.is_some()) || (key == "username" && username.is_some()) {
                    return Ok(());
                }
            }
        }
        append(&mut out, line)
    })?;
    if !out.ends_with(b"\n") {
        append(&mut out, b"\n")?;
    }
    match edit {
        Edit::Create {
            identity,
            address,
            name,
            username,
            ..
        } => {
            same_address(address, address)?;
            section(&mut out, b"account", &account_text)?;
            field(&mut out, b"username", username)?;
            field(&mut out, b"name", name)?;
            section(&mut out, b"alias", address)?;
            field(&mut out, b"account", &account_text)?;
            section(&mut out, b"identity", &identity.to_string())?;
            field(&mut out, b"account", &account_text)?;
            field(&mut out, b"email", address)?;
            field(&mut out, b"name", name)?;
        }
        Edit::Alias {
            address,
            remove: false,
            ..
        } => {
            same_address(address, address)?;
            section(&mut out, b"alias", address)?;
            field(&mut out, b"account", &account_text)?;
        }
        Edit::Alias { remove: true, .. } if !alias_found || !alias_owner => {
            return Err(Error::Missing)
        }
        _ => {}
    }
    Ok(out)
}

fn edit_identity(source: &[u8], edit: &Edit<'_>, identity: IdentityId) -> Result<Vec<u8>, Error> {
    let adding = matches!(edit, Edit::IdentityAdd { .. });
    let removing = matches!(edit, Edit::IdentityRemove { .. });
    let mut target = false;
    let mut found = false;
    let mut owner = None;
    walk(source, |_, statement| {
        match statement {
            Statement::Section { name, label, .. } => {
                target = name == "identity"
                    && label.and_then(|v| IdentityId::parse(v).ok()) == Some(identity);
                if target {
                    if found || adding {
                        return Err(Error::Conflict);
                    }
                    found = true;
                }
            }
            Statement::Assignment {
                key: "account",
                value: Value::Text(value),
                ..
            } if target => {
                if owner.is_some() {
                    return Err(Error::Conflict);
                }
                owner = Some(AccountId::parse(value).map_err(|_| Error::Invalid)?);
            }
            _ => {}
        }
        Ok(())
    })?;
    if !adding {
        if !found {
            return Err(Error::Missing);
        }
        if owner != Some(edit.account()) {
            return Err(Error::Conflict);
        }
    }
    let mut out = Vec::new();
    let mut skip = false;
    target = false;
    walk(source, |line, statement| {
        if let Statement::Section { name, label, .. } = statement {
            let same = label.and_then(|v| IdentityId::parse(v).ok()) == Some(identity);
            target = same && name == "identity";
            skip = removing && same && matches!(name, "identity" | "identity_address");
            if target {
                if let Edit::IdentityUpdate { name, address, .. } = edit {
                    append(&mut out, line)?;
                    if !line.ends_with(b"\n") {
                        append(&mut out, b"\n")?;
                    }
                    if let Some(name) = name {
                        field(&mut out, b"name", name)?;
                    }
                    if let Some(address) = address {
                        field(&mut out, b"email", address)?;
                    }
                    return Ok(());
                }
            }
        }
        if skip {
            return Ok(());
        }
        if target {
            if let (Edit::IdentityUpdate { name, address, .. }, Statement::Assignment { key, .. }) =
                (edit, statement)
            {
                if (key == "name" && name.is_some()) || (key == "email" && address.is_some()) {
                    return Ok(());
                }
            }
        }
        append(&mut out, line)
    })?;
    if let Edit::IdentityAdd {
        account,
        address,
        name,
        ..
    } = edit
    {
        section(&mut out, b"identity", &identity.to_string())?;
        field(&mut out, b"account", &account.to_string())?;
        field(&mut out, b"email", address)?;
        field(&mut out, b"name", name)?;
    }
    Ok(out)
}

/// Preserve the singleton's implicit postmaster when adding a second account.
pub fn explicit_postmaster(source: &[u8], account: AccountId) -> Result<Vec<u8>, Error> {
    let mut server = false;
    let mut present = false;
    walk(source, |_, statement| {
        match statement {
            Statement::Section { name, .. } => server = name == "server",
            Statement::Assignment {
                key: "postmaster_account",
                ..
            } if server => present = true,
            _ => {}
        }
        Ok(())
    })?;
    let mut out = Vec::new();
    walk(source, |line, statement| {
        append(&mut out, line)?;
        if !present && matches!(statement, Statement::Section { name: "server", .. }) {
            if !line.ends_with(b"\n") {
                append(&mut out, b"\n")?;
            }
            field(&mut out, b"postmaster_account", &account.to_string())?;
        }
        Ok(())
    })?;
    Ok(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    const A: AccountId = AccountId::from_bytes([1; 16]);
    fn source() -> String {
        format!("version = 1\n# keep\n[account \"{A}\"]\nusername = \"old\"\nname = \"Old\"\n[domain \"example.test\"]\n[alias \"old@EXAMPLE.test\"]\naccount = \"{A}\"\n")
    }
    #[test]
    fn update_preserves_routes_and_escapes_text() {
        let output = edit(
            source().as_bytes(),
            &Edit::Update {
                account: A,
                name: Some("A \\\" B"),
                username: Some("new"),
            },
        )
        .unwrap();
        let text = std::str::from_utf8(&output).unwrap();
        assert!(text.contains("# keep"));
        assert!(!text.contains("username = \"old\""));
        assert!(text.contains("[alias \"old@EXAMPLE.test\"]"));
        walk(&output, |_, _| Ok(())).unwrap();
        assert_eq!(
            edit(
                &output,
                &Edit::Update {
                    account: A,
                    name: Some("A \\\" B"),
                    username: Some("new")
                }
            )
            .unwrap(),
            output
        );
    }
    #[test]
    fn alias_removal_matches_domain_case_but_checks_ownership() {
        let removed = edit(
            source().as_bytes(),
            &Edit::Alias {
                account: A,
                address: "old@example.test",
                remove: true,
            },
        )
        .unwrap();
        assert!(!std::str::from_utf8(&removed).unwrap().contains("[alias"));
        assert_eq!(
            edit(
                source().as_bytes(),
                &Edit::Alias {
                    account: A,
                    address: "Old@example.test",
                    remove: true
                }
            ),
            Err(Error::Missing)
        );
        assert_eq!(
            edit(
                source().as_bytes(),
                &Edit::Alias {
                    account: A,
                    address: "old@example.test",
                    remove: false
                }
            ),
            Err(Error::Conflict)
        );
    }
    #[test]
    fn postmaster_preserves_old_singleton_and_explicit_override() {
        let source = b"[server]\nhostname = \"mail.example.test\"\n";
        let output = explicit_postmaster(source, A).unwrap();
        assert!(std::str::from_utf8(&output)
            .unwrap()
            .contains(&A.to_string()));
        assert_eq!(
            explicit_postmaster(&output, AccountId::from_bytes([2; 16])).unwrap(),
            output
        );
    }
    #[test]
    fn identity_edits_preserve_owned_fields_and_remove_all_lists() {
        let id = IdentityId::from_bytes([3; 16]);
        let other = IdentityId::from_bytes([4; 16]);
        let lists = format!(
            "[identity_address \"{id}\"]\nkind = \"reply_to\"\nemail = \"reply@example.test\"\n"
        );
        let suffix = format!("[identity \"{id}\"]\naccount = \"{A}\"\nname = \"Old Sender\"\nemail = \"old@example.test\"\nreply_to = true\ntext_signature_file = \"/private/signature\"\n{lists}[identity \"{other}\"]\naccount = \"{A}\"\nname = \"Unrelated\"\nemail = \"other@example.test\"\n");
        let original = format!("{}{lists}{suffix}", source());
        let updated = edit(
            original.as_bytes(),
            &Edit::IdentityUpdate {
                account: A,
                identity: id,
                address: Some("new@example.test"),
                name: None,
            },
        )
        .unwrap();
        let text = std::str::from_utf8(&updated).unwrap();
        assert!(text.contains("name = \"Old Sender\""));
        assert!(text.contains("text_signature_file = \"/private/signature\""));
        assert!(text.contains(&lists));
        assert!(text.contains("[alias \"old@EXAMPLE.test\"]"));
        assert!(!text.contains("email = \"old@example.test\""));
        let removed = edit(
            &updated,
            &Edit::IdentityRemove {
                account: A,
                identity: id,
            },
        )
        .unwrap();
        let text = std::str::from_utf8(&removed).unwrap();
        assert!(!text.contains(&id.to_string()));
        assert!(text.contains(&other.to_string()));
        assert!(text.contains("[alias \"old@EXAMPLE.test\"]"));
        let foreign = original.replace(
            &format!("account = \"{A}\"\nname = \"Old Sender\""),
            &format!(
                "account = \"{}\"\nname = \"Old Sender\"",
                AccountId::from_bytes([8; 16])
            ),
        );
        for operation in [
            Edit::IdentityRemove {
                account: A,
                identity: id,
            },
            Edit::IdentityUpdate {
                account: A,
                identity: id,
                name: Some("wrong"),
                address: None,
            },
        ] {
            assert_eq!(edit(foreign.as_bytes(), &operation), Err(Error::Conflict));
        }
        assert_eq!(
            edit(
                original.as_bytes(),
                &Edit::IdentityAdd {
                    account: A,
                    identity: id,
                    address: "duplicate@example.test",
                    name: ""
                }
            ),
            Err(Error::Conflict)
        );
        let added = edit(
            source().as_bytes(),
            &Edit::IdentityAdd {
                account: A,
                identity: id,
                address: "sender@external.test",
                name: "",
            },
        )
        .unwrap();
        assert!(std::str::from_utf8(&added)
            .unwrap()
            .contains("sender@external.test"));
        assert_eq!(
            edit(
                &added,
                &Edit::IdentityRemove {
                    account: A,
                    identity: id
                }
            )
            .unwrap(),
            format!("{}\n", source()).as_bytes()
        );
    }

    #[test]
    fn invalid_edits_never_produce_output() {
        assert_eq!(
            edit(
                source().as_bytes(),
                &Edit::Update {
                    account: A,
                    name: Some("bad\nfield"),
                    username: None
                }
            ),
            Err(Error::Invalid)
        );
        assert_eq!(
            edit(
                source().as_bytes(),
                &Edit::Update {
                    account: AccountId::from_bytes([2; 16]),
                    name: Some("new"),
                    username: None
                }
            ),
            Err(Error::Missing)
        );
    }
}
