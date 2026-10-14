//! Structural instance roles; neither authorization nor serving capability.

/// Stable role bits for future PostgreSQL profile rows. A profile is never
/// an idle instance: at least receiving, JMAP or relay must be selected.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Roles(u8);

impl Roles {
    pub const RECEIVE: u8 = 1;
    pub const JMAP: u8 = 2;
    pub const RELAY: u8 = 4;
    pub(crate) const LEGACY: Self = Self(Self::RECEIVE | Self::JMAP);

    /// Refuse unknown bits rather than enabling a role a binary cannot name.
    /// Named roles still need separate runtime implementation admission.
    pub const fn from_bits(bits: u8) -> Option<Self> {
        if bits == 0 || bits & !(Self::RECEIVE | Self::JMAP | Self::RELAY) != 0 {
            None
        } else {
            Some(Self(bits))
        }
    }

    pub const fn bits(self) -> u8 {
        self.0
    }
    pub const fn receive(self) -> bool {
        self.0 & Self::RECEIVE != 0
    }
    pub const fn jmap(self) -> bool {
        self.0 & Self::JMAP != 0
    }
    pub const fn relay(self) -> bool {
        self.0 & Self::RELAY != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_refuse_empty_and_unknown_values() {
        for bits in u8::MIN..=u8::MAX {
            let roles = Roles::from_bits(bits);
            assert_eq!(roles.is_some(), (1..=7).contains(&bits));
            if let Some(roles) = roles {
                assert_eq!(roles.bits(), bits);
                assert_eq!(roles.receive(), bits & 1 != 0);
                assert_eq!(roles.jmap(), bits & 2 != 0);
                assert_eq!(roles.relay(), bits & 4 != 0);
            }
        }
    }
}
