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

/// The two namespace ROOTS, which are reserved without being portal names.
///
/// `org.freedesktop.portal` is a legal well-known name — two non-empty
/// elements, no leading digit — which a comment here once denied, and the
/// denial was the whole justification for leaving it outside the
/// reservation. `engine`'s permission parser refuses an application that asks
/// to own either root, so the broker leaving them takeable made the two
/// graders disagree about the same rule, with the broker the weaker of the
/// two. Nothing addresses a bare root, so nothing was reachable through it;
/// what was reachable was a namespace root held by an arbitrary peer, which
/// is the shape the reservation exists to refuse.
const PORTAL_ROOT: &str = "org.freedesktop.portal";
const IMPL_PORTAL_ROOT: &str = "org.freedesktop.impl.portal";

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
        || name == PORTAL_ROOT
        || name == IMPL_PORTAL_ROOT
        || is_portal_name(name)
        || name
            .strip_prefix(IMPL_PORTAL_PREFIX)
            .is_some_and(|rest| !rest.is_empty())
}

/// Whether `caller` may take `name` as a well-known name.
///
/// §D's default sandboxed policy MAY OWN NO NAME, and the widening it
/// provides for is the `[Session Bus Policy]` `own` entries in the
/// application's permission file. Those entries reach the broker as the
/// `owned` list on a `Jailed` identity, resolved once at accept with the rest
/// of the identity, so this is a comparison against a fixed set rather than a
/// lookup that could answer differently twice on one connection.
///
/// EXACT names, never wildcards, and the equality below is the whole rule.
/// §D says session-bus keys are exact well-known names, so a grant of
/// `org.example.Thing` is not a grant of `org.example.Thing.Sub`, and there
/// is no prefix arm here to argue about later. The cost is recorded rather
/// than hidden: an application whose names carry a runtime-generated suffix —
/// MPRIS players are the case §B.3.2 names — cannot express its grant in this
/// file at all, and admitting a suffix form is an amendment to §D rather than
/// a widening of this function.
///
/// `Unknown` owns nothing, which needs saying because the arms are no longer
/// symmetrical: an unprovable peer has no permission file to consult, and the
/// absence of a grant is the answer rather than a reason to look elsewhere
/// for one.
///
/// A reserved name is refused to EVERYONE, and it is checked FIRST so that no
/// grant can reach it. That ordering is the load-bearing part now that grants
/// exist at all: the registration path already refuses to RECORD a reserved
/// name, and this is the second of two independent refusals, placed where the
/// name is actually taken. The connection that will hold the portal's names
/// is the one a supervisor registers as the portal at startup, and that path
/// does not exist yet, so today the reservation means the names are held by
/// nobody. A name nobody can take is a name nobody can impersonate.
pub fn may_own(caller: &Identity, name: &str) -> bool {
    if is_reserved_name(name) {
        return false;
    }
    match caller {
        Identity::Unconfined => true,
        Identity::Jailed { owned, .. } => owned.iter().any(|granted| granted == name),
        Identity::Unknown(_) => false,
    }
}

/// Whether `name` is one the PORTAL serves.
///
/// Narrower than the reservation on purpose, and the difference is the
/// namespace root. `org.freedesktop.portal` is reserved — nobody may take it
/// — but it is not a portal service, so it is not in the set a sandbox may
/// see and talk to. A confined peer gets the portal's members, not the name
/// its members live under.
///
/// A comment here used to say the bare root "is not a legal well-known name
/// either", and that was simply false: two non-empty elements with no leading
/// digit is a legal name, `valid_well_known_name` accepts it, and the false
/// premise was the entire argument for leaving the root takeable. See
/// `PORTAL_ROOT`.
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
/// A name the caller's permission file GRANTED it is seen for both of those
/// reasons at once. The engine's `BusAccess` is an ORDERED capability —
/// `Own` allows `Talk` and `See`, which is `allows`' own table — so an `own`
/// entry is not only a licence to take the name, it is the `see` and `talk`
/// entries the same line implies. Reading it as ownership alone implemented a
/// third of the entry. It is also the `Hello` argument again: a peer that has
/// taken the name was sent `NameAcquired` for it, and a broker that then
/// reported the name absent would be denying something it had just said. It
/// is granted whether or not the caller currently HOLDS the name, because the
/// permission file is what confers it and a queued claimant needs to reach
/// the holder it is waiting behind.
///
/// THE GRANT DOES NOT CARRY CREDENTIALS, and this paragraph is about the
/// exemption ABOVE it rather than the one before that -- a reviewer read the
/// two as one, which is reason enough to separate them here. What a peer may
/// be told about the process behind a name is `may_ask_credentials`, and it
/// was deliberately left where it was when this function widened.
///
/// The OWN-NAME exemption does carry the caller's own credentials, including
/// its host pid — which a process inside a pid namespace has no other way to
/// learn. It is deliberate and it is narrower than it looks: the reason to
/// withhold a host pid is that another instance's is an identifier for
/// spelunking outside the jail and an input to the lineage walk, and neither
/// argument reaches a peer's own number, which buys it no ancestry it does
/// not already have. A rule that hid it would also have `GetNameOwner` and
/// `GetConnectionCredentials` disagree about the same name — which is not a
/// hypothetical: the narrower gate did exactly that to a peer asking about a
/// well-known name it was HOLDING, until `Peer::askable` sent that question
/// to the caller's own unique name.
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
        // Strictly less than `Jailed`: no portal either, and no granted name,
        // because there is no permission file to have granted one. An
        // unprovable peer is not a sandboxed application with a reduced
        // grant, it is a peer the broker could not place at all.
        Identity::Unknown(_) => told_already,
        Identity::Jailed { owned, .. } => {
            told_already
                || is_portal_name(target)
                || owned.iter().any(|granted| granted == target)
        }
    }
}

/// Whether `caller` may ask the broker WHO is behind `target`.
///
/// Narrower than `may_see`, and the gap is exactly the permission file's
/// grant. `own` implies `talk`, so a granted name is one the caller may look
/// up and address — but the uid and pid behind it belong to whoever HOLDS it,
/// which on a bus with two windows of one application is the other window.
/// §D singles the credential methods out for precisely this: another
/// instance's host pid is an identifier for `/proc` spelunking outside the
/// jail and the input to the lineage walk this broker's identity story rests
/// on, and nothing in "you may own this name" is a licence to learn where its
/// current holder lives. Two instances may call each other by name without
/// either learning the other's pid.
///
/// This is the set `may_see` had before the grant widened it, which is not a
/// coincidence: the widening was about NAMES, and this function is about
/// PEERS. Keeping them separate is what stopped the widening from quietly
/// carrying a disclosure with it — a reviewer found that it had.
///
/// A caller asking about a well-known name IT holds is refused too, and that
/// is a narrower route rather than a contradiction: its own credentials are
/// answerable through its unique name, which `told_already` covers, so
/// nothing it may learn is withheld — only one way of asking for it.
pub fn may_ask_credentials(caller: &Identity, own: Option<&str>, target: &str) -> bool {
    let told_already = target == BUS_NAME || Some(target) == own;
    match caller {
        Identity::Unconfined => true,
        Identity::Unknown(_) => told_already,
        Identity::Jailed { .. } => told_already || is_portal_name(target),
    }
}

/// Whether `caller` may send a directed message to `target`.
///
/// Deliberately the same rule as `may_see` rather than a second table. §D
/// grants a sandboxed peer the portal and the names its permission file
/// names, and `BusAccess::Own` allows `Talk` — so the two sets coincide, and
/// one function behind both keeps them from drifting apart in silence. They
/// stay separate call sites because they REPORT differently: a target the
/// caller cannot see is absent, where one it can see but may not reach would
/// be `AccessDenied`.
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
/// the pair is what made the talk set safe to widen when the permission
/// file's `own` entries landed, since that widening admits CALLS to a granted
/// name rather than arbitrary traffic toward whoever holds it.
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
            owned: Vec::new(),
        }
    }

    /// A confined peer whose permission file granted it `names`.
    fn granted(names: &[&str]) -> Identity {
        Identity::Jailed {
            app_id: "firefox".into(),
            instance: "firefox-1".into(),
            owned: names.iter().map(|name| (*name).to_string()).collect(),
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

    /// §D's default sandboxed policy owns no name. A confined peer whose
    /// permission file said nothing is that default.
    #[test]
    fn a_confined_peer_owns_no_name_by_default() {
        assert!(may_own(&Identity::Unconfined, "org.mozilla.firefox"));
        assert!(!may_own(&jailed(), "org.mozilla.firefox"));
        assert!(!may_own(
            &Identity::Unknown("unplaceable".into()),
            "org.mozilla.firefox"
        ));
    }

    /// The widening: a name the application's permission file granted it.
    ///
    /// This is what §B.3.2 needs for Firefox to be reachable as
    /// `org.mozilla.firefox` rather than merely routable by a name nothing
    /// may hold.
    #[test]
    fn a_confined_peer_owns_the_names_its_permission_file_granted() {
        let firefox = granted(&["org.mozilla.firefox"]);
        assert!(may_own(&firefox, "org.mozilla.firefox"));

        // And nothing else. A grant is one name, not a footing.
        assert!(!may_own(&firefox, "org.gnome.Nautilus"));
        assert!(!may_own(&firefox, "org.freedesktop.FileManager1"));
    }

    /// EXACT names. A grant is not a namespace.
    ///
    /// The suffix case is the one that matters in practice, because it is
    /// what an MPRIS player would need and what a prefix rule would quietly
    /// hand over: `org.mozilla.firefox` granting
    /// `org.mozilla.firefox.Anything` would let one grant cover names §D
    /// never named. The neighbour case is the cheaper mistake and is pinned
    /// beside it.
    #[test]
    fn a_grant_covers_the_name_and_not_the_names_under_it() {
        let firefox = granted(&["org.mozilla.firefox"]);
        for near in [
            "org.mozilla.firefox.Sub",
            "org.mozilla.firefox.",
            "org.mozilla.firefo",
            "org.mozilla.firefoxx",
            "Org.Mozilla.Firefox",
            "org.mpris.MediaPlayer2.firefox.instance1",
        ] {
            assert!(!may_own(&firefox, near), "{near} was covered by the grant");
        }
    }

    /// Several names, and each of them exactly.
    #[test]
    fn a_permission_file_may_grant_more_than_one_name() {
        let both = granted(&["org.example.One", "org.example.Two"]);
        assert!(may_own(&both, "org.example.One"));
        assert!(may_own(&both, "org.example.Two"));
        assert!(!may_own(&both, "org.example.Three"));
    }

    /// An `own` entry carries the `see` and `talk` it implies.
    ///
    /// `BusAccess` is an ordered capability and `Own` allows both of the
    /// others, so a permission file that grants a name has already granted
    /// the right to look it up and address it. Reading `own` as ownership
    /// alone left the broker reporting a name absent to the very peer it had
    /// just sent `NameAcquired` for.
    #[test]
    fn a_granted_name_is_seen_and_addressable_by_the_peer_granted_it() {
        let firefox = granted(&["org.mozilla.firefox"]);
        assert!(may_see(&firefox, Some(":1.4"), "org.mozilla.firefox"));
        assert!(may_talk(&firefox, Some(":1.4"), "org.mozilla.firefox"));

        // And no further. A grant is one name, for seeing as for owning.
        assert!(!may_see(&firefox, Some(":1.4"), "org.gnome.Nautilus"));
        assert!(!may_see(&firefox, Some(":1.4"), "org.mozilla.firefox.Sub"));
        assert!(!may_see(&firefox, Some(":1.4"), ":1.9"));

        // A confined peer with no grant is where it was.
        assert!(!may_see(&jailed(), Some(":1.4"), "org.mozilla.firefox"));

        // An unprovable peer has no permission file, so there is nothing for
        // this arm to widen.
        let unplaceable = Identity::Unknown("unplaceable".into());
        assert!(!may_see(&unplaceable, Some(":1.4"), "org.mozilla.firefox"));
    }

    /// A grant carries `see` and `talk`. It does NOT carry credentials.
    ///
    /// The name resolves to whoever holds it, so answering credentials about
    /// a granted name hands one instance the host pid of another — the one
    /// disclosure §D names explicitly, reached through a widening that was
    /// about names rather than peers.
    #[test]
    fn a_grant_does_not_carry_the_holders_credentials() {
        let firefox = granted(&["org.mozilla.firefox"]);
        assert!(may_see(&firefox, Some(":1.4"), "org.mozilla.firefox"));
        assert!(!may_ask_credentials(
            &firefox,
            Some(":1.4"),
            "org.mozilla.firefox"
        ));

        // Its own name and the broker stay answerable, so nothing it could
        // learn before is withheld now.
        assert!(may_ask_credentials(&firefox, Some(":1.4"), ":1.4"));
        assert!(may_ask_credentials(&firefox, Some(":1.4"), BUS_NAME));
        // And the portal, which is what the set was before the grant.
        assert!(may_ask_credentials(
            &firefox,
            Some(":1.4"),
            "org.freedesktop.portal.Desktop"
        ));
        // An unconfined caller is unrestricted here as everywhere.
        assert!(may_ask_credentials(
            &Identity::Unconfined,
            Some(":1.4"),
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
            // Including a peer whose registration claimed it. The registration
            // path refuses to record a reserved name, so this is the second of
            // two independent refusals rather than the only one — and it is
            // the one that holds if the first is ever bypassed, since this is
            // where the name would actually be taken.
            assert!(
                !may_own(&granted(&[name]), name),
                "{name} was granted by a permission file"
            );
        }
    }

    /// The namespace ROOTS are reserved, and neighbours that merely start
    /// with the same letters are not.
    ///
    /// This test asserted the opposite until a reviewer showed the two
    /// graders disagreed: `engine`'s permission parser refuses an application
    /// that asks to own either root, and the broker did not. The old
    /// assertion rested on a comment claiming the root was not a legal bus
    /// name, which it is.
    #[test]
    fn the_namespace_roots_are_reserved_and_their_neighbours_are_not() {
        assert!(is_reserved_name("org.freedesktop.portal"));
        assert!(is_reserved_name("org.freedesktop.impl.portal"));
        assert!(!is_reserved_name("org.freedesktop.portalish.Thing"));
        assert!(!is_reserved_name("org.freedesktop.impl.portalish.Thing"));
        assert!(!is_reserved_name("org.mozilla.firefox"));
        // Reserved is not the same set as "the portal serves it". A sandbox
        // may see and talk to portal MEMBERS; the root is not one, and
        // widening the see-set to cover it would hand out a name nothing
        // answers on.
        assert!(!is_portal_name("org.freedesktop.portal"));
        assert!(!is_portal_name("org.freedesktop.impl.portal"));
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
