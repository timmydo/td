//! The running system's protector planners (ENCRYPTION.md "Protector
//! management", DESIGN.md "Protector management"): an addition, a
//! removal, the recovery key's removal and a request, each pure over a
//! protected header and confirmed by one `Plan::confirm`. The opener is
//! the operation's proof: the fido2 token whose keyslot the proof tested,
//! or the recovery key on the marker's keyslot. Library code: the storage
//! worker's protector operations call it.

use crate::luks2::{Header, MAX_FIDO2_TOKENS};
use crate::protected::{Fido2, FidoRole, Operation, Request, Shape, Token};
use crate::protected_plan::{self, Opened, Plan, Refusal, Step};
use crate::token::MAX_SLOT;

/// The most FIDO2 tokens of one role the FIDO2-primary shape holds.
const MAX_FIDO2_ROLE: usize = 3;

/// Whether a tpm-pin token names a keyslot: an orphan opens nothing, so
/// no minimum counts it.
fn pin_protector(header: &Header) -> bool {
    protected_plan::tpm_pins(header)
        .iter()
        .any(|(_, pin)| pin.keyslot.is_some())
}

/// The fido2 tokens naming a keyslot, by number: the FIDO2 protectors a
/// minimum counts. A pending or orphan token opens nothing.
pub fn protectors(header: &Header) -> Vec<(u8, &Fido2)> {
    header
        .protected
        .iter()
        .filter_map(|(number, token)| match token {
            Token::Fido2(fido) if fido.keyslot.is_some() => Some((*number, fido)),
            _ => None,
        })
        .collect()
}

/// Whether removing the fido2 token `token` leaves the shape its minimums
/// (td-authd/DESIGN.md amendment 10, `Removal::removable`): in the
/// TPM-primary shape any protector while a tpm-pin token names a
/// keyslot, and otherwise one that leaves another, so the recovery key is
/// never left alone; in the FIDO2-primary shape one that leaves a
/// fido2-primary protector and either two FIDO2 protectors or the marker.
/// A header `check` refuses is removable nowhere.
pub fn removable(header: &Header, token: u8) -> bool {
    let Ok(shape) = protected_plan::check(header) else {
        return false;
    };
    let protectors = protectors(header);
    if !protectors.iter().any(|(number, _)| *number == token) {
        return false;
    }
    let left: Vec<&Fido2> = protectors
        .iter()
        .filter(|(number, _)| *number != token)
        .map(|(_, fido)| *fido)
        .collect();
    match shape {
        Shape::Tpm => pin_protector(header) || !left.is_empty(),
        Shape::Fido2 => {
            left.iter().any(|fido| fido.role == FidoRole::Primary)
                && (left.len() >= 2 || protected_plan::marker(header).is_some())
        }
    }
}

/// The opener's keyslot, on a header `check` admits.
fn opened(header: &Header, opened: Opened) -> Result<(Shape, u8), Refusal> {
    let shape = protected_plan::check(header)?;
    // A proof is a security key's or the recovery key's.
    if let Opened::TpmPin { token } = opened {
        return Err(Refusal::UnknownToken { token });
    }
    Ok((shape, protected_plan::opener(header, opened)?))
}

/// One FIDO2 token in `new`'s role, its keyslot at the lowest free number
/// from 1, added under the opener with the new key's derived passphrase
/// (`Step::AddKeyslot`'s new key), imported naming it and tested.
pub fn add(header: &Header, opened: Opened, new: &Fido2) -> Result<Plan, Refusal> {
    let (shape, opener) = self::opened(header, opened)?;
    if shape == Shape::Tpm && new.role != FidoRole::Recovery {
        return Err(Refusal::Role);
    }
    // Every fido2 token counts against the reader's bound and the role's,
    // a pending or orphan one included.
    let tokens: Vec<FidoRole> = header
        .protected
        .iter()
        .filter_map(|(_, token)| match token {
            Token::Fido2(fido) => Some(fido.role),
            _ => None,
        })
        .collect();
    let same = tokens.iter().filter(|role| **role == new.role).count();
    if tokens.len() >= MAX_FIDO2_TOKENS || (shape == Shape::Fido2 && same >= MAX_FIDO2_ROLE) {
        return Err(Refusal::Full);
    }
    let slot = (1..=MAX_SLOT)
        .find(|slot| !header.keyslots.contains(slot))
        .ok_or(Refusal::NoFreeKeyslot)?;
    protected_plan::free_token(header)?;
    let token = Token::Fido2(Fido2 {
        keyslot: Some(slot),
        ..new.clone()
    });
    token.encode().map_err(Refusal::Token)?;
    Ok(Plan::new(
        header,
        vec![
            Step::AddKeyslot { slot, by: opener },
            Step::ImportToken {
                token,
                id: None,
                replace: false,
            },
            Step::Test { slot },
        ],
    ))
}

/// The fido2 token `token`'s keyslot, killed under the opener, then the
/// token, when it is removable and not the opener.
pub fn remove(header: &Header, opened: Opened, token: u8) -> Result<Plan, Refusal> {
    let (_, opener) = self::opened(header, opened)?;
    let slot = protectors(header)
        .into_iter()
        .find(|(number, _)| *number == token)
        .and_then(|(_, fido)| fido.keyslot)
        .ok_or(Refusal::UnknownToken { token })?;
    if slot == opener {
        return Err(Refusal::RemovesOpener);
    }
    if !removable(header, token) {
        return Err(Refusal::Minimum);
    }
    Ok(Plan::new(
        header,
        vec![
            Step::KillSlot { slot, by: opener },
            Step::RemoveToken { token },
        ],
    ))
}

/// The marker's keyslot, killed under a security key's, then the marker:
/// only in the FIDO2-primary shape while two FIDO2 protectors remain, a
/// fido2-primary one among them. The recovery key cannot remove itself.
pub fn remove_recovery(header: &Header, opened: Opened) -> Result<Plan, Refusal> {
    let (shape, opener) = self::opened(header, opened)?;
    let Some((marker, slot)) = protected_plan::marker(header) else {
        return Err(Refusal::Minimum);
    };
    if slot == opener {
        return Err(Refusal::RemovesOpener);
    }
    let protectors = protectors(header);
    if shape != Shape::Fido2
        || protectors.len() < 2
        || !protectors
            .iter()
            .any(|(_, fido)| fido.role == FidoRole::Primary)
    {
        return Err(Refusal::Minimum);
    }
    Ok(Plan::new(
        header,
        vec![
            Step::KillSlot { slot, by: opener },
            Step::RemoveToken { token: marker },
        ],
    ))
}

/// The pending `td-request` token's removal, then `request`'s import, when
/// the header holds the protector it acts on: a PIN change the TPM-primary
/// shape's tpm-pin token naming a keyslot and the lockout token, which
/// anchors the new seal; a TPM clear the lockout token; a re-enrollment,
/// in the TPM-primary shape, such a tpm-pin token or the lockout token to
/// find stale.
pub fn request(header: &Header, opened: Opened, request: &Request) -> Result<Plan, Refusal> {
    let (shape, _) = self::opened(header, opened)?;
    let pin = pin_protector(header);
    let lockout = protected_plan::lockout(header).is_some();
    let held = match request.operation {
        Operation::PinChange => shape == Shape::Tpm && pin && lockout,
        Operation::TpmClear => lockout,
        Operation::TpmReenroll => shape == Shape::Tpm && (pin || lockout),
    };
    if !held {
        return Err(Refusal::NoProtector);
    }
    let pending = crate::request::pending(header).map(|(number, _)| number);
    if pending.is_none() {
        protected_plan::free_token(header)?;
    }
    let token = Token::Request(request.clone());
    token.encode().map_err(Refusal::Token)?;
    let mut steps: Vec<Step> = pending
        .map(|token| Step::RemoveToken { token })
        .into_iter()
        .collect();
    steps.push(Step::ImportToken {
        token,
        id: None,
        replace: false,
    });
    Ok(Plan::new(header, steps))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protected::fixtures;
    use crate::protected_plan::tests::{fido, Disk, FIDO_KEY};

    fn new_key(role: FidoRole) -> Fido2 {
        let Token::Fido2(fido) = fixtures::fido2(None, role) else {
            panic!("the fixture changed kind")
        };
        fido
    }

    /// The plan confirmed against the disk, then committed step by step,
    /// the reader admitting each state.
    fn commit(disk: &mut Disk, plan: Plan) -> Vec<Step> {
        let steps = plan.confirm(&disk.dump()).unwrap();
        for step in &steps {
            disk.apply(step, 3000);
        }
        steps
    }

    const BY_FIDO: Opened = Opened::Fido2 { token: 2 };

    #[test]
    fn an_addition_takes_the_lowest_free_keyslot_and_tests_it() {
        let mut disk = Disk::tpm_shape();
        // Keyslot 3 is an orphan an interrupted addition left.
        disk.keyslots.insert(3, 103);
        for opened in [BY_FIDO, Opened::RecoveryKey] {
            let plan = add(&disk.header(), opened, &new_key(FidoRole::Recovery)).unwrap();
            let by = if opened == BY_FIDO { 2 } else { 0 };
            assert_eq!(
                plan.steps(),
                [
                    Step::AddKeyslot { slot: 4, by },
                    Step::ImportToken {
                        token: fixtures::fido2(Some(4), FidoRole::Recovery),
                        id: None,
                        replace: false,
                    },
                    Step::Test { slot: 4 },
                ]
            );
        }
        let plan = add(&disk.header(), BY_FIDO, &new_key(FidoRole::Recovery)).unwrap();
        commit(&mut disk, plan);
        assert_eq!(disk.keyslots.get(&4), Some(&3000));
        assert_eq!(
            disk.tokens.get(&4),
            Some(&fixtures::fido2(Some(4), FidoRole::Recovery))
        );
    }

    #[test]
    fn an_addition_refuses_a_role_or_room_the_shape_lacks() {
        let disk = Disk::tpm_shape();
        let refused = |disk: &Disk, role| add(&disk.header(), BY_FIDO, &new_key(role)).unwrap_err();
        assert_eq!(refused(&disk, FidoRole::Primary), Refusal::Role);
        // Four FIDO2 tokens, a pending one among them, fill either shape.
        let mut full = disk.clone();
        for (slot, number) in [(3, 5), (4, 6)] {
            full.keyslots.insert(slot, FIDO_KEY + u32::from(slot));
            full.tokens
                .insert(number, fido(Some(slot), FidoRole::Recovery));
        }
        full.tokens.insert(7, fido(None, FidoRole::Recovery));
        assert_eq!(refused(&full, FidoRole::Recovery), Refusal::Full);
        // The FIDO2 shape: three of a role, then four in all.
        let mut fido_shape = Disk::fido_shape();
        fido_shape.keyslots.remove(&3);
        fido_shape.tokens.remove(&3);
        assert!(add(&fido_shape.header(), BY_FIDO, &new_key(FidoRole::Primary)).is_ok());
        fido_shape.keyslots.insert(4, FIDO_KEY + 4);
        fido_shape
            .tokens
            .insert(4, fido(Some(4), FidoRole::Primary));
        assert_eq!(refused(&fido_shape, FidoRole::Primary), Refusal::Full);
        assert!(add(&fido_shape.header(), BY_FIDO, &new_key(FidoRole::Recovery)).is_ok());
        fido_shape.keyslots.insert(5, FIDO_KEY + 5);
        fido_shape
            .tokens
            .insert(5, fido(Some(5), FidoRole::Recovery));
        assert_eq!(refused(&fido_shape, FidoRole::Recovery), Refusal::Full);
    }

    #[test]
    fn a_removal_kills_under_another_protector_and_keeps_the_minimums() {
        let mut disk = Disk::tpm_shape();
        let plan = remove(&disk.header(), Opened::RecoveryKey, 2).unwrap();
        assert_eq!(
            commit(&mut disk, plan),
            [
                Step::KillSlot { slot: 2, by: 0 },
                Step::RemoveToken { token: 2 }
            ]
        );
        assert!(!disk.keyslots.contains_key(&2) && !disk.tokens.contains_key(&2));
        // The chosen token's own key, a token that is no fido2 protector,
        // and a pending one are refused.
        let disk = Disk::tpm_shape();
        assert_eq!(
            remove(&disk.header(), BY_FIDO, 2).unwrap_err(),
            Refusal::RemovesOpener
        );
        for token in [0, 1, 3, 9] {
            assert_eq!(
                remove(&disk.header(), Opened::RecoveryKey, token).unwrap_err(),
                Refusal::UnknownToken { token }
            );
        }
        let mut pending = disk.clone();
        pending.tokens.insert(4, fido(None, FidoRole::Recovery));
        assert!(!removable(&pending.header(), 4));
        // Without a tpm-pin token the recovery key is never left alone.
        let mut cleared = disk.clone();
        cleared.keyslots.remove(&1);
        cleared.tokens.remove(&1);
        assert!(!removable(&cleared.header(), 2));
        assert_eq!(
            remove(&cleared.header(), Opened::RecoveryKey, 2).unwrap_err(),
            Refusal::Minimum
        );
        cleared.keyslots.insert(3, FIDO_KEY + 3);
        cleared.tokens.insert(4, fido(Some(3), FidoRole::Recovery));
        assert!(removable(&cleared.header(), 2) && removable(&cleared.header(), 4));
        assert!(remove(&cleared.header(), Opened::Fido2 { token: 4 }, 2).is_ok());
    }

    #[test]
    fn a_fido2_shape_removal_leaves_a_primary_and_two_tokens_or_the_marker() {
        let disk = Disk::fido_shape();
        for token in [1, 2, 3] {
            assert!(removable(&disk.header(), token), "{token}");
        }
        // One primary and one recovery token beside the marker: the
        // primary is the last of its role; the recovery token may go.
        let mut two = disk.clone();
        two.keyslots.remove(&2);
        two.tokens.remove(&2);
        assert!(!removable(&two.header(), 1));
        assert!(removable(&two.header(), 3));
        // Without the marker, two FIDO2 tokens must remain.
        two.keyslots.remove(&0);
        two.tokens.remove(&0);
        assert!(!removable(&two.header(), 3));
        assert_eq!(
            remove(&two.header(), Opened::Fido2 { token: 1 }, 3).unwrap_err(),
            Refusal::Minimum
        );
        let mut three = disk.clone();
        three.keyslots.remove(&0);
        three.tokens.remove(&0);
        assert!(removable(&three.header(), 1) && removable(&three.header(), 3));
    }

    #[test]
    fn the_recovery_key_goes_only_in_the_fido2_shape_under_a_security_key() {
        let mut disk = Disk::fido_shape();
        let plan = remove_recovery(&disk.header(), Opened::Fido2 { token: 1 }).unwrap();
        assert_eq!(
            commit(&mut disk, plan),
            [
                Step::KillSlot { slot: 0, by: 1 },
                Step::RemoveToken { token: 0 }
            ]
        );
        let disk = Disk::fido_shape();
        assert_eq!(
            remove_recovery(&disk.header(), Opened::RecoveryKey).unwrap_err(),
            Refusal::RemovesOpener
        );
        assert_eq!(
            remove_recovery(&Disk::tpm_shape().header(), BY_FIDO).unwrap_err(),
            Refusal::Minimum
        );
        let mut one = disk.clone();
        for slot in [2, 3] {
            one.keyslots.remove(&slot);
            one.tokens.remove(&slot);
        }
        assert_eq!(
            remove_recovery(&one.header(), Opened::Fido2 { token: 1 }).unwrap_err(),
            Refusal::Minimum
        );
        let mut gone = disk.clone();
        gone.keyslots.remove(&0);
        gone.tokens.remove(&0);
        assert_eq!(
            remove_recovery(&gone.header(), Opened::Fido2 { token: 1 }).unwrap_err(),
            Refusal::Minimum
        );
    }

    fn request_of(operation: Operation) -> Request {
        Request {
            operation,
            nonce: [7; 32],
            tag: [8; 32],
        }
    }

    #[test]
    fn a_request_replaces_a_pending_one_where_its_protector_is() {
        let mut disk = Disk::tpm_shape();
        for operation in Operation::ALL {
            let plan = request(&disk.header(), BY_FIDO, &request_of(*operation)).unwrap();
            assert_eq!(
                plan.steps(),
                [Step::ImportToken {
                    token: Token::Request(request_of(*operation)),
                    id: None,
                    replace: false,
                }]
            );
        }
        let plan = request(
            &disk.header(),
            Opened::RecoveryKey,
            &request_of(Operation::TpmClear),
        );
        commit(&mut disk, plan.unwrap());
        let replacing = request(&disk.header(), BY_FIDO, &request_of(Operation::PinChange));
        assert_eq!(
            commit(&mut disk, replacing.unwrap()),
            [
                Step::RemoveToken { token: 4 },
                Step::ImportToken {
                    token: Token::Request(request_of(Operation::PinChange)),
                    id: None,
                    replace: false,
                }
            ]
        );
        assert_eq!(
            crate::request::pending(&disk.header()),
            Some((4, &request_of(Operation::PinChange)))
        );
        // Without the protector each acts on.
        let refused = |disk: &Disk, operation| {
            request(&disk.header(), BY_FIDO, &request_of(operation)).unwrap_err()
        };
        let mut no_lockout = Disk::tpm_shape();
        no_lockout.tokens.remove(&3);
        assert_eq!(
            refused(&no_lockout, Operation::PinChange),
            Refusal::NoProtector
        );
        assert_eq!(
            refused(&no_lockout, Operation::TpmClear),
            Refusal::NoProtector
        );
        assert!(request(
            &no_lockout.header(),
            BY_FIDO,
            &request_of(Operation::TpmReenroll)
        )
        .is_ok());
        let mut no_pin = Disk::tpm_shape();
        no_pin.keyslots.remove(&1);
        no_pin.tokens.remove(&1);
        assert_eq!(refused(&no_pin, Operation::PinChange), Refusal::NoProtector);
        assert!(request(
            &no_pin.header(),
            BY_FIDO,
            &request_of(Operation::TpmReenroll)
        )
        .is_ok());
        no_pin.tokens.remove(&3);
        assert_eq!(
            refused(&no_pin, Operation::TpmReenroll),
            Refusal::NoProtector
        );
        let fido_shape = Disk::fido_shape();
        for operation in Operation::ALL {
            assert_eq!(refused(&fido_shape, *operation), Refusal::NoProtector);
        }
    }

    #[test]
    fn every_planner_refuses_a_tpm_pin_opener_and_a_header_check_refuses() {
        let disk = Disk::tpm_shape();
        let header = disk.header();
        let pin = Opened::TpmPin { token: 1 };
        let unknown = Refusal::UnknownToken { token: 1 };
        assert_eq!(
            add(&header, pin, &new_key(FidoRole::Recovery)).unwrap_err(),
            unknown
        );
        assert_eq!(remove(&header, pin, 2).unwrap_err(), unknown);
        assert_eq!(remove_recovery(&header, pin).unwrap_err(), unknown);
        assert_eq!(
            request(&header, pin, &request_of(Operation::TpmClear)).unwrap_err(),
            unknown
        );
        let mut mixed = disk.clone();
        mixed.tokens.insert(5, fido(Some(2), FidoRole::Primary));
        mixed.tokens.remove(&2);
        let header = mixed.header();
        assert_eq!(
            add(&header, Opened::RecoveryKey, &new_key(FidoRole::Recovery)).unwrap_err(),
            Refusal::MixedShape
        );
        assert!(!removable(&header, 5));
    }

    #[test]
    fn an_orphan_tpm_pin_token_is_no_protector() {
        // A cut re-enrollment killed the tpm-pin keyslot and left its
        // token: the recovery key would be left alone.
        let mut disk = Disk::tpm_shape();
        disk.keyslots.remove(&1);
        if let Some(Token::TpmPin(pin)) = disk.tokens.get_mut(&1) {
            pin.keyslot = None;
        }
        assert!(!removable(&disk.header(), 2));
        let refused = |disk: &Disk, operation| {
            request(&disk.header(), BY_FIDO, &request_of(operation)).unwrap_err()
        };
        assert_eq!(refused(&disk, Operation::PinChange), Refusal::NoProtector);
        // A re-enrollment still finds the lockout token stale, and
        // without it nothing.
        assert!(request(&disk.header(), BY_FIDO, &request_of(Operation::TpmReenroll)).is_ok());
        disk.tokens.remove(&3);
        assert_eq!(refused(&disk, Operation::TpmReenroll), Refusal::NoProtector);
    }

    #[test]
    fn the_recovery_key_goes_beside_exactly_two_protectors_and_a_primary() {
        let mut disk = Disk::fido_shape();
        disk.keyslots.remove(&2);
        disk.tokens.remove(&2);
        assert!(remove_recovery(&disk.header(), Opened::Fido2 { token: 1 }).is_ok());
        // A pending token is no protector.
        let mut pending = disk.clone();
        pending.keyslots.remove(&3);
        pending.tokens.insert(3, fido(None, FidoRole::Recovery));
        assert_eq!(
            remove_recovery(&pending.header(), Opened::Fido2 { token: 1 }).unwrap_err(),
            Refusal::Minimum
        );
        // Two recovery protectors beside an orphan primary.
        let mut orphan = Disk::fido_shape();
        orphan.keyslots.remove(&1);
        orphan.keyslots.remove(&2);
        orphan.tokens.insert(1, fido(None, FidoRole::Primary));
        orphan.tokens.insert(2, fido(Some(4), FidoRole::Recovery));
        orphan.keyslots.insert(4, FIDO_KEY + 4);
        assert_eq!(
            remove_recovery(&orphan.header(), Opened::Fido2 { token: 3 }).unwrap_err(),
            Refusal::Minimum
        );
    }

    #[test]
    fn an_addition_never_takes_keyslot_0() {
        let mut disk = Disk::fido_shape();
        disk.keyslots.remove(&0);
        disk.tokens.remove(&0);
        let plan = add(
            &disk.header(),
            Opened::Fido2 { token: 1 },
            &new_key(FidoRole::Recovery),
        )
        .unwrap();
        assert_eq!(
            plan.steps().first(),
            Some(&Step::AddKeyslot { slot: 4, by: 1 })
        );
    }
}
