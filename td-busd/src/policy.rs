//! Who may see what, and who may talk to whom.
//!
//! §D's per-caller filter. Everything the broker answers about the bus is
//! answered to SOMEBODY, and until this module existed the answer did not
//! depend on who asked: `ListNames` reported every connection on the bus to
//! every caller, and a credentials lookup handed any peer's host pid to any
//! peer. That is correct for a session of mutually trusting programs and it
//! is not the confinement boundary §D describes.
//!
//! The three-valued identity in `lineage` is what this reads, and each arm is
//! a decision rather than a default:
//!
//! - `Unconfined` is a POSITIVE grant and is unrestricted. td's trust model
//!   says a same-uid process that is not a jailed application may do what
//!   same-uid processes may do; §E is explicit that this is a proved answer
//!   and not a fallback, which is the whole reason the oracle landed before
//!   this filter did.
//! - `Jailed` gets §D's default sandboxed policy.
//! - `Unknown` is denied everything the boundary protects, which is every
//!   other peer. It is the ambiguous case and §D fails it closed, because
//!   failing the other way is privilege up: a peer whose lineage the broker
//!   could not prove would otherwise collect the unconfined grant it was
//!   unable to demonstrate it was entitled to. It keeps the broker and its
//!   own name, and that is not a softening — see `may_see`.
//!
//! What this module does NOT decide is which methods a confined peer may
//! call. §D settles that in the other direction: the whole
//! `org.freedesktop.DBus` roster stays callable and it is the ANSWERS that
//! are filtered, because a `see` policy expressed as refused calls is a
//! filter on questions nobody needed to ask. `td.Jail1` is not part of that
//! roster and is refused here — it is the interface that CREATES confinement
//! records, and a confined peer reaching it could name its own instance.

use crate::lineage::Identity;

/// The broker's own name. Answerable to everyone: a peer that could not see
/// the bus could not ask the bus anything, and every arm below would be
/// unreachable.
const BUS_NAME: &str = "org.freedesktop.DBus";

/// The portal namespace, reserved by §D rather than merely conventional.
///
/// A confined application may see and talk to the portal, and that is the
/// only peer other than the broker it reaches by default: portals are how a
/// sandbox asks for anything outside itself. The reservation is compiled in
/// because the alternative is a name like any other, which an unsandboxed
/// same-uid process could claim after a restart and thereby receive other
/// applications' portal traffic.
const PORTAL_PREFIX: &str = "org.freedesktop.portal.";

/// The portal's own implementation side, reserved on the same argument.
const IMPL_PORTAL_PREFIX: &str = "org.freedesktop.impl.portal.";

/// Whether `name` is one this broker keeps out of general circulation.
///
/// §D: applications cannot own the broker or the reserved
/// `org.freedesktop.portal.*` and `org.freedesktop.impl.portal.*` names, and
/// the reservation survives the portal's death — a restarted portal
/// re-registers through the supervised path rather than racing for the name.
/// The argument is not about sandboxes: an UNSANDBOXED uid-1000 process,
/// which this design deliberately leaves unrestricted, could otherwise claim
/// `org.freedesktop.portal.Desktop` after a restart and start receiving
/// sandboxed applications' portal traffic. That is a same-uid process doing
/// what same-uid processes may do, which is exactly why the BROKER has to be
/// the one to refuse it.
pub fn is_reserved_name(name: &str) -> bool {
    name == BUS_NAME
        || is_portal_name(name)
        || name
            .strip_prefix(IMPL_PORTAL_PREFIX)
            .is_some_and(|rest| !rest.is_empty())
}

/// Whether `caller` may take `name` as a well-known name.
///
/// §D's default sandboxed policy MAY OWN NO NAME, and that is what this says.
/// The widening §D provides for is the `[Session Bus Policy]` `own` entries
/// in an application's permission file — exact names, never wildcards — and
/// those do not reach this broker yet: `td.Jail1`'s registration carries an
/// app id, an instance and a predeclared service list, and no own-set. Until
/// a landing carries them through, a confined peer asking for a name is
/// refused rather than quietly granted, which is the direction to be wrong
/// in.
///
/// A reserved name is refused to EVERYONE, including unconfined callers.
/// The connection that will hold the portal's names is the one a supervisor
/// registers as the portal at startup, and that path does not exist yet, so
/// today the reservation means the names are held by nobody. That is
/// fail-closed rather than a gap: a name nobody can take is a name nobody can
/// impersonate.
pub fn may_own(caller: &Identity, name: &str) -> bool {
    !is_reserved_name(name) && matches!(caller, Identity::Unconfined)
}

/// Whether `name` is in the reserved portal namespace.
///
/// The bare prefix with nothing after it is NOT a portal name: it is not a
/// legal well-known name either, and admitting it would put a name no portal
/// can own inside the reservation.
pub fn is_portal_name(name: &str) -> bool {
    name.strip_prefix(PORTAL_PREFIX)
        .is_some_and(|rest| !rest.is_empty())
}

/// Whether `caller` may learn that `target` exists.
///
/// `own` is the caller's own unique name, which it always sees: it was told
/// that name by `Hello`, so hiding it would be a fiction the broker had
/// already contradicted.
///
/// That exemption also carries the caller's own CREDENTIALS, including its
/// host pid — which a process inside a pid namespace has no other way to
/// learn. It is deliberate and it is narrower than it looks: the reason to
/// withhold a host pid is that another instance's is an identifier for
/// spelunking outside the jail and an input to the lineage walk, and neither
/// argument reaches a peer's own number, which buys it no ancestry it does
/// not already have. A rule that hid it would also have `GetNameOwner` and
/// `GetConnectionCredentials` disagree about the same name.
pub fn may_see(caller: &Identity, own: Option<&str>, target: &str) -> bool {
    // The broker and the caller's own name are not a grant. They are the two
    // facts every connection has already been told: it is talking to the
    // broker, and `Hello` answered with its name. A rule that hid them would
    // not withhold anything — it would contradict what the connection had
    // just been sent, and a first draft of `Unknown` did exactly that, so a
    // peer could hold a name the bus denied it had.
    let told_already = target == BUS_NAME || Some(target) == own;
    match caller {
        Identity::Unconfined => true,
        // Strictly less than `Jailed`: no portal either. An unprovable peer
        // is not a sandboxed application with a reduced grant, it is a peer
        // the broker could not place at all, and the portal is a grant.
        Identity::Unknown(_) => told_already,
        Identity::Jailed { .. } => told_already || is_portal_name(target),
    }
}

/// Whether `caller` may send a directed message to `target`.
///
/// Deliberately the same rule as `may_see` rather than a second table. §D
/// grants a sandboxed peer the portal and nothing else, so the two coincide
/// today, and one function behind both keeps them from drifting apart in
/// silence. They stay separate call sites because they REPORT differently: a
/// target the caller cannot see is absent, where one it can see but may not
/// reach would be `AccessDenied`.
pub fn may_talk(caller: &Identity, own: Option<&str>, target: &str) -> bool {
    may_see(caller, own, target)
}

/// Whether `caller` may originate a directed SIGNAL.
///
/// §D's default sandboxed policy grants a confined peer the right to CALL any
/// portal member and to RECEIVE the portal's replies and directed signals. It
/// does not grant the reverse: a signal aimed at the portal is the sandbox
/// telling the portal something happened, which is a channel nothing asked
/// for and which no toolkit uses. Refusing it keeps `may_talk` a statement
/// about who may be addressed and this a statement about what may be sent —
/// the pair is what makes the talk set safe to widen when `RequestName`
/// lands, since the widening then admits calls rather than arbitrary traffic.
///
/// The TARGET is not consulted, which makes the rule broader than that
/// rationale: a confined peer may not aim a signal at its own name either,
/// though `may_see` grants it that name as a fact it has already been told.
/// Deliberate, and the cheaper way to be wrong. Nothing needs to signal
/// itself through a broker — the sender already knows — so the permission
/// would buy nothing, while a rule that reads "a confined peer originates no
/// directed signals" has no edge for a later widening to be argued through.
///
/// A reply is neither of these: it is governed by whether it answers a real
/// call, which the broker's pending-reply table decides.
pub fn may_signal(caller: &Identity) -> bool {
    matches!(caller, Identity::Unconfined)
}

/// Whether `caller` may use `td.Jail1` at all.
///
/// Registration is authenticated by uid, which in v1 does not distinguish one
/// session peer from another — so this is the only thing between a confined
/// application and the interface that decides what confined applications ARE.
/// A jailed peer that could register would name its own instance and app id.
pub fn may_register(caller: &Identity) -> bool {
    matches!(caller, Identity::Unconfined)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jailed() -> Identity {
        Identity::Jailed {
            app_id: "firefox".into(),
            instance: "firefox-1".into(),
        }
    }

    #[test]
    fn an_unconfined_peer_is_unrestricted() {
        let anyone = Identity::Unconfined;
        for target in [
            BUS_NAME,
            ":1.7",
            "org.example.Thing",
            "org.freedesktop.portal.Desktop",
        ] {
            assert!(may_see(&anyone, Some(":1.1"), target), "{target}");
            assert!(may_talk(&anyone, Some(":1.1"), target), "{target}");
        }
        assert!(may_register(&anyone));
    }

    /// The direction that matters. An identity the broker could not establish
    /// must not collect the grant it failed to prove.
    #[test]
    fn an_unprovable_peer_is_denied_rather_than_trusted() {
        let nobody = Identity::Unknown("the walk hit a reused pid".into());
        // Every other peer, and the portal, which is a grant it has not
        // earned: an unprovable peer gets strictly LESS than a sandboxed one.
        for target in [":1.7", "org.freedesktop.portal.Desktop", "org.example.Thing"] {
            assert!(!may_see(&nobody, Some(":1.1"), target), "{target}");
            assert!(!may_talk(&nobody, Some(":1.1"), target), "{target}");
        }
        assert!(!may_register(&nobody));
    }

    /// What it keeps, and why that is not a softening: the broker it is
    /// already talking to, and the name `Hello` already handed it. Denying
    /// these would have the bus contradict what it had just sent.
    #[test]
    fn an_unprovable_peer_still_knows_the_broker_and_its_own_name() {
        let nobody = Identity::Unknown("the walk hit a reused pid".into());
        assert!(may_see(&nobody, Some(":1.1"), BUS_NAME));
        assert!(may_see(&nobody, Some(":1.1"), ":1.1"));
        assert!(may_talk(&nobody, Some(":1.1"), BUS_NAME));
    }

    /// An unprovable peer is never given more than a sandboxed one. Written
    /// as a comparison rather than two lists so that widening `Unknown` past
    /// `Jailed` reds even if both lists were edited to agree.
    #[test]
    fn an_unprovable_peer_never_outranks_a_sandboxed_one() {
        let nobody = Identity::Unknown("unplaceable".into());
        let app = jailed();
        for target in [
            BUS_NAME,
            ":1.1",
            ":1.7",
            "org.freedesktop.portal.Desktop",
            "org.example.Thing",
        ] {
            if may_see(&nobody, Some(":1.1"), target) {
                assert!(
                    may_see(&app, Some(":1.1"), target),
                    "an unplaceable peer outranks a sandboxed one at {target}"
                );
            }
        }
    }

    #[test]
    fn a_jailed_peer_sees_the_broker_the_portal_and_itself() {
        let app = jailed();
        for target in [
            BUS_NAME,
            ":1.4",
            "org.freedesktop.portal.Desktop",
            "org.freedesktop.portal.Documents",
        ] {
            assert!(may_see(&app, Some(":1.4"), target), "{target}");
        }
    }

    #[test]
    fn a_jailed_peer_sees_no_other_peer() {
        let app = jailed();
        for target in [
            ":1.5",
            "org.example.Thing",
            "org.mozilla.firefox",
            // Near misses on the reservation. It is a prefix test, so this is
            // exactly where a mistake would live.
            "org.freedesktop.portal",
            "org.freedesktop.portalX.Desktop",
            "org.freedesktop.Portal.Desktop",
            "com.example.org.freedesktop.portal.Desktop",
        ] {
            assert!(!may_see(&app, Some(":1.4"), target), "{target}");
            assert!(!may_talk(&app, Some(":1.4"), target), "{target}");
        }
    }

    /// A confined peer may not create confinement records.
    #[test]
    fn a_jailed_peer_may_not_register() {
        assert!(!may_register(&jailed()));
    }

    /// Only an unconfined peer may originate a directed signal. §D grants a
    /// sandbox the right to CALL the portal and to RECEIVE its signals, and
    /// says nothing about sending any.
    #[test]
    fn only_an_unconfined_peer_may_signal() {
        assert!(may_signal(&Identity::Unconfined));
        assert!(!may_signal(&jailed()));
        assert!(!may_signal(&Identity::Unknown("unplaceable".into())));
    }

    /// §D's default sandboxed policy owns no name, and nothing carries an
    /// own-set to this broker yet.
    #[test]
    fn only_an_unconfined_peer_may_own_a_name() {
        assert!(may_own(&Identity::Unconfined, "org.mozilla.firefox"));
        assert!(!may_own(&jailed(), "org.mozilla.firefox"));
        assert!(!may_own(
            &Identity::Unknown("unplaceable".into()),
            "org.mozilla.firefox"
        ));
    }

    /// The reservation holds against EVERYONE, which is the whole reason it
    /// is the broker's to enforce: an unsandboxed same-uid process is the
    /// caller it is there to refuse.
    #[test]
    fn nobody_may_own_a_reserved_name() {
        for name in [
            BUS_NAME,
            "org.freedesktop.portal.Desktop",
            "org.freedesktop.impl.portal.Access",
        ] {
            assert!(is_reserved_name(name), "{name} is not reserved");
            assert!(!may_own(&Identity::Unconfined, name), "{name} was granted");
            assert!(!may_own(&jailed(), name), "{name} was granted");
        }
    }

    /// A prefix on its own is not a reservation, for the same reason a prefix
    /// on its own is not a portal name: `org.freedesktop.portal` is a name
    /// somebody could legitimately want, and swallowing it would reserve a
    /// name §D does not.
    #[test]
    fn the_reserved_prefixes_alone_are_not_reserved() {
        assert!(!is_reserved_name("org.freedesktop.portal"));
        assert!(!is_reserved_name("org.freedesktop.impl.portal"));
        assert!(!is_reserved_name("org.freedesktop.portalish.Thing"));
        assert!(!is_reserved_name("org.mozilla.firefox"));
    }

    /// The bare prefix owns nothing and is not in the reservation.
    #[test]
    fn the_portal_prefix_alone_is_not_a_portal() {
        assert!(!is_portal_name("org.freedesktop.portal."));
        assert!(!is_portal_name("org.freedesktop.portal"));
        assert!(is_portal_name("org.freedesktop.portal.Desktop"));
    }

    /// A peer with no name yet still gets a decision, and it is not "see
    /// everything": `own` is an `Option` because `Hello` has not necessarily
    /// happened, not because its absence widens anything.
    #[test]
    fn a_nameless_jailed_peer_still_sees_only_the_broker_and_the_portal() {
        let app = jailed();
        assert!(may_see(&app, None, BUS_NAME));
        assert!(may_see(&app, None, "org.freedesktop.portal.Desktop"));
        assert!(!may_see(&app, None, ":1.4"));
    }
}
