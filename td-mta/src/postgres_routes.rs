//! Passive named-route evidence; worker ownership requires fresh policy fences.
use crate::config::{outbound::Relay, values};
use td_crypto::{Crypto, Digest, Provider};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Name,
    Capacity,
    Crypto,
}

#[derive(Clone, Eq, PartialEq)]
pub struct Route {
    name: String,
    destination: [u8; 32],
    definition: [u8; 32],
}
impl std::fmt::Debug for Route {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Route(<redacted>)")
    }
}
impl Route {
    pub fn name(&self) -> &str {
        &self.name
    }
    /// Destination includes host, port, transport and authentication identity;
    /// credential and trust versions may rotate without silently rerouting mail.
    pub fn destination(&self) -> &[u8; 32] {
        &self.destination
    }
    pub(crate) fn compatible(&self, other: &Self) -> bool {
        self.name != other.name || self.definition == other.definition
    }
    pub(crate) fn from_relay(profile: &str, relay: Relay<'_>) -> Result<Self, Error> {
        let name = relay.route.unwrap_or(profile);
        values::profile_name(name).map_err(|_| Error::Name)?;
        let mut owned = String::new();
        owned
            .try_reserve_exact(name.len())
            .map_err(|_| Error::Capacity)?;
        owned.push_str(name);
        let destination = digest(&[
            b"td-mta route destination v1",
            relay.host.as_bytes(),
            &relay.port.to_be_bytes(),
            relay.transport.name().as_bytes(),
            relay.username.as_bytes(),
        ])?;
        let definition = digest(&[
            b"td-mta route definition v1",
            &destination,
            relay.password_file.as_bytes(),
            &[u8::from(relay.ca_file.is_some())],
            relay.ca_file.unwrap_or("").as_bytes(),
        ])?;
        Ok(Self {
            name: owned,
            destination,
            definition,
        })
    }
}
fn digest(fields: &[&[u8]]) -> Result<[u8; 32], Error> {
    let mut hash = Provider.sha256().map_err(|_| Error::Crypto)?;
    for field in fields {
        hash.update(&(field.len() as u64).to_be_bytes())
            .map_err(|_| Error::Crypto)?;
        hash.update(field).map_err(|_| Error::Crypto)?;
    }
    hash.finish().map_err(|_| Error::Crypto)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::config::{outbound::Transport, syntax::Location};
    #[test]
    fn names_share_routes_but_destination_changes_do_not_hide_in_rotation() {
        let relay = Relay {
            route: Some("personal"),
            location: Location {
                line: std::num::NonZeroU32::new(1).unwrap(),
                column: 1,
            },
            host: "relay.example.test",
            port: 465,
            transport: Transport::ImplicitTls,
            username: "user",
            password_file: "/postgres-material/one",
            ca_file: None,
        };
        let desktop = Route::from_relay("desktop", relay).unwrap();
        let cloud = Route::from_relay("cloud", relay).unwrap();
        assert_eq!(desktop, cloud);
        assert_eq!(desktop.name(), "personal");
        assert_eq!(format!("{desktop:?}"), "Route(<redacted>)");
        let default = Route::from_relay(
            "desktop",
            Relay {
                route: None,
                ..relay
            },
        )
        .unwrap();
        assert_eq!(default.name(), "desktop");
        assert!(default.compatible(&desktop));
        for changed in [
            Relay {
                host: "other.example.test",
                ..relay
            },
            Relay { port: 587, ..relay },
            Relay {
                transport: Transport::RequiredStartTls,
                ..relay
            },
            Relay {
                username: "other",
                ..relay
            },
        ] {
            let changed = Route::from_relay("desktop", changed).unwrap();
            assert_ne!(desktop.destination(), changed.destination());
            assert!(!desktop.compatible(&changed));
        }
        for changed in [
            Relay {
                password_file: "/postgres-material/two",
                ..relay
            },
            Relay {
                ca_file: Some("/postgres-material/trust"),
                ..relay
            },
        ] {
            let changed = Route::from_relay("desktop", changed).unwrap();
            assert_eq!(desktop.destination(), changed.destination());
            assert!(!desktop.compatible(&changed));
        }
        for name in ["", "Personal", "../route", "a b"] {
            assert_eq!(
                Route::from_relay(
                    "desktop",
                    Relay {
                        route: Some(name),
                        ..relay
                    }
                ),
                Err(Error::Name)
            );
        }
    }
}
