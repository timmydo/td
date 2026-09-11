//! Private session protocol and a physical attention lifetime.

use crate::authority::consent::{Enrollment, Operation, Platform, Recovery, Request, Role};
use crate::authority::Exchange;
use crate::input::EvdevOrigin;
use crate::runtime::Runtime;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const ACTIVE: u8 = 0;
const CANCELLED: u8 = 1;
const COMMITTED: u8 = 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Selection {
    Unlock(Role),
    Enroll(Recovery),
    Write,
    Install,
}
impl From<Role> for Selection {
    fn from(role: Role) -> Self {
        Self::Unlock(role)
    }
}
impl Selection {
    fn operation(self) -> Option<Operation> {
        Some(match self {
            Self::Write | Self::Install => return None,
            Self::Unlock(role) => Operation::Unlock { role },
            Self::Enroll(recovery) => Operation::Enroll {
                platform: Platform::TpmPcr7,
                recovery,
                step: Enrollment::CreatePrimary,
            },
        })
    }
    fn request(self) -> Vec<u8> {
        match self {
            Self::Write => vec![0x18],
            Self::Install => vec![0x19],
            Self::Unlock(Role::Primary) => vec![0x12, 1],
            Self::Unlock(Role::Recovery) => vec![0x12, 2],
            Self::Enroll(Recovery::Unrecoverable) => vec![0x16, 0],
            Self::Enroll(Recovery::SecondToken) => vec![0x16, 1],
        }
    }
}

pub(crate) struct Attempt {
    origin: EvdevOrigin,
    runtime: Arc<Mutex<Runtime>>,
    selection: Selection,
    state: AtomicU8,
    deadline: Option<Instant>,
    presentation: Mutex<Option<(Request, u128)>>,
    confirmed: AtomicBool,
}

impl Attempt {
    pub fn new(
        origin: EvdevOrigin,
        runtime: Arc<Mutex<Runtime>>,
        selection: impl Into<Selection>,
    ) -> Arc<Self> {
        Arc::new(Self {
            origin,
            runtime,
            selection: selection.into(),
            state: AtomicU8::new(ACTIVE),
            deadline: Instant::now().checked_add(Duration::from_secs(120)),
            presentation: Mutex::new(None),
            confirmed: AtomicBool::new(false),
        })
    }

    /// Input records cancellation before waiting for painting or channel I/O.
    pub fn cancel(&self) {
        self.state.fetch_or(CANCELLED, Ordering::SeqCst);
    }

    fn active(&self) -> bool {
        self.state.load(Ordering::SeqCst) == ACTIVE
            && self
                .deadline
                .is_some_and(|deadline| Instant::now() < deadline)
    }

    fn commit(&self) -> bool {
        if !self.active() || (self.selection == Selection::Install && !self.confirmed.load(Ordering::SeqCst)) {
            return false;
        }
        self.state
            .compare_exchange(ACTIVE, COMMITTED, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    fn present(&self, request: Request) -> Result<Request, String> {
        if !self.active() {
            return Err("physical attention was cancelled".into());
        }
        let mut runtime = self.runtime.lock().map_err(|_| "runtime lock poisoned")?;
        if !self.active() {
            return Err("physical attention was cancelled".into());
        }
        let remaining = self
            .deadline
            .and_then(|deadline| deadline.checked_duration_since(Instant::now()))
            .map(|duration| duration.as_secs())
            .filter(|seconds| *seconds > 0)
            .ok_or("physical secret operation expired")?;
        let receipt =
            runtime.present_attention_request_with_time(&self.origin, request, Some(remaining))?;
        if !self.active() {
            return Err("physical attention was cancelled during presentation".into());
        }
        let completed = receipt.completed();
        let request = receipt.into_request();
        if self.selection == Selection::Install {
            *self.presentation.lock().map_err(|_| "installation receipt lock poisoned")? = Some((request.clone(), completed));
        }
        Ok(request)
    }

    /// Only the physical evdev adapter can offer a confirmation key.
    pub fn confirm_install(&self, _origin: &EvdevOrigin, timestamp: u128) -> Result<(), String> {
        if self.selection != Selection::Install || !self.active() { return Ok(()); }
        let runtime = self.runtime.lock().map_err(|_| "runtime lock poisoned")?;
        let presentation = self.presentation.lock().map_err(|_| "installation receipt lock poisoned")?;
        if let Some((request, completed)) = &*presentation {
            if timestamp > *completed && self.active() && runtime.attention_request_visible(request) {
                self.confirmed.store(true, Ordering::SeqCst);
            }
        }
        Ok(())
    }

    pub fn notice(&self, notice: crate::attention::Notice) -> Result<(), String> {
        let visible = || matches!(self.state.load(Ordering::SeqCst), ACTIVE | COMMITTED);
        if !visible() {
            return Ok(());
        }
        let mut runtime = self.runtime.lock().map_err(|_| "runtime lock poisoned")?;
        if !visible() {
            return Ok(());
        }
        runtime.attention_notice(&self.origin, notice)
    }
}

struct Pending {
    attempt: Arc<Attempt>,
    request: Request,
    receipt: Option<Request>,
    committed: bool,
    cancelled: bool,
}

struct Inspection {
    attempt: Arc<Attempt>,
    begin: bool,
}

#[derive(Default)]
pub(crate) struct Client {
    pending: Option<Pending>,
    inspection: Option<Inspection>,
}

impl Client {
    pub fn start(&mut self, wire: &mut impl Exchange, attempt: Arc<Attempt>) -> Result<(), String> {
        if !attempt.active() {
            return Ok(());
        }
        if self.pending.is_some() || self.inspection.is_some() {
            return attempt.notice(crate::attention::Notice::Busy);
        }
        if matches!(attempt.selection, Selection::Enroll(_)) {
            return self.inspect(wire, attempt, true);
        }
        self.begin(wire, attempt)
    }

    fn inspect(
        &mut self,
        wire: &mut impl Exchange,
        attempt: Arc<Attempt>,
        begin: bool,
    ) -> Result<(), String> {
        if wire.exchange(&[0x17])? != [0x97] {
            return Err("invalid store inspection start".into());
        }
        self.inspection = Some(Inspection { attempt, begin });
        Ok(())
    }

    fn begin(&mut self, wire: &mut impl Exchange, attempt: Arc<Attempt>) -> Result<(), String> {
        if !attempt.active() {
            return Ok(());
        }
        let response = wire.exchange(&attempt.selection.request())?;
        if attempt.selection == Selection::Write && response == [0x98, 0] {
            return attempt.notice(crate::attention::Notice::NoWrite);
        }
        if attempt.selection == Selection::Install && response == [0x99, 0] {
            return attempt.notice(crate::attention::Notice::NoInstall);
        }
        let Some((&0x92, bytes)) = response.split_first() else {
            return Err("invalid secret operation start response".into());
        };
        let request = Request::decode(bytes)?;
        let selected = match attempt.selection.operation() {
            Some(operation) => request.operation() == &operation,
            None => match attempt.selection {
                Selection::Write => matches!(request.operation(), Operation::Set { requester: 1000, .. }),
                Selection::Install => matches!(request.operation(), Operation::Install { requester: 1000, .. }),
                _ => false,
            },
        };
        if request.owner() != 1000 || !selected {
            return Err("root secret request changed the selected operation".into());
        }
        self.pending = Some(Pending {
            attempt,
            request,
            receipt: None,
            committed: false,
            cancelled: false,
        });
        Ok(())
    }

    pub fn tick(&mut self, wire: &mut impl Exchange) -> Result<(), String> {
        if self.inspection.is_some() {
            return self.poll_inspection(wire);
        }
        let Some(pending) = &mut self.pending else {
            return Ok(());
        };
        if !pending.attempt.active() && !pending.committed && !pending.cancelled {
            pending.cancel(wire)?;
        }
        let response = wire.exchange(&[0x11])?;
        let [0x91, status, bytes @ ..] = response.as_slice() else {
            return Err("invalid secret operation status".into());
        };
        let description = Request::decode(bytes)?;
        if description != pending.request {
            let following = pending.request.following_enrollment_step()?;
            if following.as_ref() != Some(&description)
                || pending.receipt.as_ref() != Some(&pending.request)
                || !matches!(*status, 3 | 4 | 7)
            {
                return Err("secret operation status changed its request".into());
            }
            pending.receipt = None;
            pending.request = description;
        }
        match status {
            3 => (),
            4 if pending.receipt.is_none() && !pending.committed && !pending.cancelled => {
                match pending.attempt.present(pending.request.clone()) {
                    Ok(receipt) if receipt == pending.request && pending.attempt.active() => {
                        let mut bytes = vec![0x13];
                        bytes.extend_from_slice(&receipt.encode());
                        if wire.exchange(&bytes)? != [0x93] {
                            return Err("invalid presentation acknowledgement".into());
                        }
                        pending.receipt = Some(receipt);
                    }
                    _ => pending.cancel(wire)?,
                }
            }
            5 if pending.request.following_enrollment_step()?.is_none()
                && pending.receipt.as_ref() == Some(&pending.request)
                && !pending.committed
                && !pending.cancelled =>
            {
                if pending.attempt.selection == Selection::Install && pending.attempt.active()
                    && !pending.attempt.confirmed.load(Ordering::SeqCst) { return Ok(()); }
                // This CAS chooses between physical cancellation and consent.
                // No runtime lock spans the subsequent bounded exchange.
                if pending.attempt.commit() {
                    pending.committed = true;
                    let mut bytes = vec![0x14];
                    bytes.extend_from_slice(&pending.request.encode());
                    if wire.exchange(&bytes)? != [0x94] {
                        return Err("invalid commit acknowledgement".into());
                    }
                } else {
                    pending.cancel(wire)?;
                }
            }
            6 if pending.committed => {
                pending.attempt.notice(
                    if matches!(pending.attempt.selection, Selection::Enroll(_)) {
                        crate::attention::Notice::Enrolled
                    } else if pending.attempt.selection == Selection::Install {
                        crate::attention::Notice::Installed
                    } else if pending.attempt.selection == Selection::Write {
                        crate::attention::Notice::Stored
                    } else {
                        crate::attention::Notice::Unlocked
                    },
                )?;
                self.pending = None;
            }
            7 => {
                let attempt = Arc::clone(&pending.attempt);
                self.pending = None;
                if matches!(attempt.selection, Selection::Enroll(_)) {
                    self.inspect(wire, attempt, false)?;
                } else {
                    attempt.notice(crate::attention::Notice::Failed)?;
                }
            }
            _ => return Err("out-of-order secret operation status".into()),
        }
        Ok(())
    }

    fn poll_inspection(&mut self, wire: &mut impl Exchange) -> Result<(), String> {
        let response = wire.exchange(&[0x11])?;
        if response == [0x91, 8] {
            return Ok(());
        }
        let state = match response.as_slice() {
            [0x91, 9, state @ 0..=3] => Some(*state),
            [0x91, 10] => None,
            _ => return Err("invalid store inspection result".into()),
        };
        let inspection = self.inspection.take().ok_or("missing store inspection")?;
        if inspection.begin && matches!(state, Some(0 | 1)) && inspection.attempt.active() {
            return self.begin(wire, inspection.attempt);
        }
        inspection.attempt.notice(match state {
            Some(2 | 3) => crate::attention::Notice::Enrolled,
            Some(0 | 1) => crate::attention::Notice::Unenrolled,
            _ => crate::attention::Notice::Unavailable,
        })
    }
}

impl Pending {
    fn cancel(&mut self, wire: &mut impl Exchange) -> Result<(), String> {
        let mut bytes = vec![0x15];
        bytes.extend_from_slice(self.request.nonce());
        if !matches!(wire.exchange(&bytes)?.as_slice(), [0x95, 0 | 2]) {
            return Err("invalid cancellation acknowledgement".into());
        }
        self.cancelled = true;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::indexing_slicing, clippy::panic)]
    use super::*;
    use std::collections::VecDeque;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicU64;

    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    struct Screen {
        path: PathBuf,
        attempt: Arc<Attempt>,
    }
    impl Screen {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "td-secret-screen-{}-{}",
                std::process::id(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            let mut runtime = Runtime::new(
                crate::framebuffer::Framebuffer::test_file(&path, 800, 600, 3200).unwrap(),
            );
            runtime.enable_attention(true);
            runtime
                .attention(&crate::input::test_origin(), true)
                .unwrap();
            Self {
                path,
                attempt: Attempt::new(
                    crate::input::test_origin(),
                    Arc::new(Mutex::new(runtime)),
                    Role::Primary,
                ),
            }
        }
    }
    impl Drop for Screen {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    struct Wire {
        replies: VecDeque<Vec<u8>>,
        calls: Vec<Vec<u8>>,
    }
    impl Exchange for Wire {
        fn exchange(&mut self, bytes: &[u8]) -> Result<Vec<u8>, String> {
            self.calls.push(bytes.to_vec());
            self.replies.pop_front().ok_or("uncertain delivery".into())
        }
    }
    fn request() -> Request {
        Request::new(
            [42; 32],
            1000,
            Operation::Unlock {
                role: Role::Primary,
            },
        )
        .unwrap()
    }
    fn tagged(tag: &[u8]) -> Vec<u8> {
        [tag, request().encode().as_slice()].concat()
    }
    fn wire(replies: Vec<Vec<u8>>) -> Wire {
        Wire {
            replies: replies.into(),
            calls: Vec::new(),
        }
    }

    #[test]
    fn installation_waits_for_fresh_enter_after_complete_presentation() {
        let mut screen = Screen::new();
        Arc::get_mut(&mut screen.attempt).unwrap().selection = Selection::Install;
        let request = Request::new([42; 32], 1000, Operation::Install { deployment: "ab".repeat(32), requester: 1000 }).unwrap();
        let tagged = |tag: &[u8]| [tag, request.encode().as_slice()].concat();
        let mut wire = wire(vec![tagged(&[0x92]), tagged(&[0x91, 4]), vec![0x93],
            tagged(&[0x91, 5]), tagged(&[0x91, 5]), vec![0x94], tagged(&[0x91, 6])]);
        let mut client = Client::default();
        client.start(&mut wire, Arc::clone(&screen.attempt)).unwrap();
        screen.attempt.confirm_install(&crate::input::test_origin(), u128::MAX).unwrap();
        assert!(!screen.attempt.confirmed.load(Ordering::SeqCst));
        client.tick(&mut wire).unwrap();
        let completed = screen.attempt.presentation.lock().unwrap().as_ref().unwrap().1;
        screen.attempt.confirm_install(&crate::input::test_origin(), completed).unwrap();
        client.tick(&mut wire).unwrap();
        assert!(!wire.calls.iter().any(|call| call.first() == Some(&0x14)));
        screen.attempt.confirm_install(&crate::input::test_origin(), completed + 1).unwrap();
        client.tick(&mut wire).unwrap();
        screen.attempt.confirm_install(&crate::input::test_origin(), completed + 2).unwrap();
        client.tick(&mut wire).unwrap();
        assert!(client.pending.is_none());
        assert_eq!(wire.calls.iter().filter(|call| call.first() == Some(&0x14)).count(), 1);
    }

    #[test]
    fn installation_escape_or_hidden_prompt_cannot_be_confirmed() {
        for cancel in [false, true] {
            let mut screen = Screen::new();
            Arc::get_mut(&mut screen.attempt).unwrap().selection = Selection::Install;
            let request = Request::new([42; 32], 1000, Operation::Install { deployment: "ab".repeat(32), requester: 1000 }).unwrap();
            screen.attempt.present(request).unwrap();
            if cancel { screen.attempt.cancel(); }
            else { screen.attempt.runtime.lock().unwrap().attention_notice(&crate::input::test_origin(), crate::attention::Notice::Failed).unwrap(); }
            screen.attempt.confirm_install(&crate::input::test_origin(), u128::MAX).unwrap();
            assert!(!screen.attempt.commit());
        }
    }

    #[test]
    fn actual_presentation_and_one_commit_precede_success() {
        let screen = Screen::new();
        let mut client = Client::default();
        let mut wire = wire(vec![
            tagged(&[0x92]),
            tagged(&[0x91, 4]),
            vec![0x93],
            tagged(&[0x91, 5]),
            vec![0x94],
            tagged(&[0x91, 6]),
        ]);
        client
            .start(&mut wire, Arc::clone(&screen.attempt))
            .unwrap();
        let inert = std::fs::read(&screen.path).unwrap();
        client.tick(&mut wire).unwrap();
        assert_ne!(std::fs::read(&screen.path).unwrap(), inert);
        assert_eq!(client.pending.as_ref().unwrap().receipt, Some(request()));
        assert!(screen.attempt.active());
        client.tick(&mut wire).unwrap();
        assert!(!screen.attempt.active());
        client.tick(&mut wire).unwrap();
        assert!(client.pending.is_none());
        assert_eq!(
            wire.calls,
            [
                vec![0x12, 1],
                vec![0x11],
                tagged(&[0x13]),
                vec![0x11],
                tagged(&[0x14]),
                vec![0x11]
            ]
        );
    }

    #[test]
    fn cancellation_before_presentation_or_commit_sends_no_commit() {
        for presented in [false, true] {
            let screen = Screen::new();
            let mut client = Client::default();
            let mut replies = vec![tagged(&[0x92])];
            if presented {
                replies.extend([tagged(&[0x91, 4]), vec![0x93]]);
            }
            replies.extend([vec![0x95, 0], tagged(&[0x91, 7])]);
            let mut wire = wire(replies);
            client
                .start(&mut wire, Arc::clone(&screen.attempt))
                .unwrap();
            if presented {
                client.tick(&mut wire).unwrap();
            }
            screen.attempt.cancel();
            client.tick(&mut wire).unwrap();
            assert!(client.pending.is_none());
            assert!(!wire.calls.iter().any(|bytes| bytes.first() == Some(&0x14)));
            assert_eq!(
                wire.calls
                    .iter()
                    .filter(|bytes| bytes.first() == Some(&0x15))
                    .count(),
                1
            );
        }
    }

    #[test]
    fn cancellation_during_commit_poll_wins_before_the_execution_decision() {
        struct CancelOnCommit<'a> {
            wire: Wire,
            attempt: &'a Attempt,
        }
        impl Exchange for CancelOnCommit<'_> {
            fn exchange(&mut self, bytes: &[u8]) -> Result<Vec<u8>, String> {
                let response = self.wire.exchange(bytes)?;
                if response.starts_with(&[0x91, 5]) {
                    self.attempt.cancel();
                }
                Ok(response)
            }
        }
        let screen = Screen::new();
        let mut client = Client::default();
        let mut channel = CancelOnCommit {
            wire: wire(vec![
                tagged(&[0x92]),
                tagged(&[0x91, 4]),
                vec![0x93],
                tagged(&[0x91, 5]),
                vec![0x95, 0],
                tagged(&[0x91, 7]),
            ]),
            attempt: &screen.attempt,
        };
        client
            .start(&mut channel, Arc::clone(&screen.attempt))
            .unwrap();
        client.tick(&mut channel).unwrap();
        client.tick(&mut channel).unwrap();
        client.tick(&mut channel).unwrap();
        assert!(client.pending.is_none());
        assert!(!channel
            .wire
            .calls
            .iter()
            .any(|bytes| bytes.first() == Some(&0x14)));
        assert!(channel
            .wire
            .calls
            .iter()
            .any(|bytes| bytes.first() == Some(&0x15)));
    }

    #[test]
    fn cancellation_after_worker_failure_retires_the_relocked_result() {
        let screen = Screen::new();
        let mut client = Client::default();
        let mut wire = wire(vec![tagged(&[0x92]), vec![0x95, 2], tagged(&[0x91, 7])]);
        client
            .start(&mut wire, Arc::clone(&screen.attempt))
            .unwrap();
        screen.attempt.cancel();
        client.tick(&mut wire).unwrap();
        assert!(client.pending.is_none());
    }

    #[test]
    fn uncertain_commit_delivery_is_never_retried() {
        let screen = Screen::new();
        let mut client = Client::default();
        let mut wire = wire(vec![
            tagged(&[0x92]),
            tagged(&[0x91, 4]),
            vec![0x93],
            tagged(&[0x91, 5]),
        ]);
        client
            .start(&mut wire, Arc::clone(&screen.attempt))
            .unwrap();
        client.tick(&mut wire).unwrap();
        assert_eq!(client.tick(&mut wire).unwrap_err(), "uncertain delivery");
        assert!(!screen.attempt.active());
        assert!(!screen.attempt.commit());
        wire.replies.push_back(tagged(&[0x91, 5]));
        assert!(client.tick(&mut wire).is_err());
        assert_eq!(
            wire.calls
                .iter()
                .filter(|bytes| bytes.first() == Some(&0x14))
                .count(),
            1
        );
    }

    #[test]
    fn failed_presentation_cancels_without_starting_token_acquisition() {
        let screen = Screen::new();
        screen
            .attempt
            .runtime
            .lock()
            .unwrap()
            .begin_compound_commit()
            .unwrap();
        let mut client = Client::default();
        let mut wire = wire(vec![tagged(&[0x92]), tagged(&[0x91, 4]), vec![0x95, 0]]);
        client
            .start(&mut wire, Arc::clone(&screen.attempt))
            .unwrap();
        client.tick(&mut wire).unwrap();
        assert!(client.pending.as_ref().unwrap().cancelled);
        assert!(client.pending.as_ref().unwrap().receipt.is_none());
        assert!(!wire.calls.iter().any(|bytes| bytes.first() == Some(&0x13)));
    }

    #[test]
    fn cancelled_queued_attempt_is_never_started() {
        let screen = Screen::new();
        screen.attempt.cancel();
        let mut wire = wire(vec![]);
        Client::default()
            .start(&mut wire, Arc::clone(&screen.attempt))
            .unwrap();
        assert!(wire.calls.is_empty());
        assert!(!screen.attempt.commit());
    }

    #[test]
    fn cancellation_and_commit_have_one_atomic_winner() {
        for _ in 0..64 {
            let screen = Screen::new();
            let attempt = Arc::clone(&screen.attempt);
            let barrier = Arc::new(std::sync::Barrier::new(2));
            let other = Arc::clone(&barrier);
            let commit = std::thread::spawn(move || {
                other.wait();
                attempt.commit()
            });
            barrier.wait();
            screen.attempt.cancel();
            let committed = commit.join().unwrap();
            let state = screen.attempt.state.load(Ordering::SeqCst);
            assert_eq!(
                state,
                if committed {
                    COMMITTED | CANCELLED
                } else {
                    CANCELLED
                }
            );
            assert!(!screen.attempt.commit());
        }
    }

    #[test]
    fn stale_requests_and_success_without_commit_end_the_channel() {
        for response in [tagged(&[0x91, 6]), tagged(&[0x91, 5]), vec![0x91, 2], {
            let stale = Request::new([43; 32], 1000, request().operation().clone()).unwrap();
            [&[0x91, 4][..], stale.encode().as_slice()].concat()
        }] {
            let screen = Screen::new();
            let mut client = Client::default();
            let mut wire = wire(vec![tagged(&[0x92]), response]);
            client
                .start(&mut wire, Arc::clone(&screen.attempt))
                .unwrap();
            assert!(client.tick(&mut wire).is_err());
            assert!(!wire
                .calls
                .iter()
                .any(|bytes| bytes.first() == Some(&0x13) || bytes.first() == Some(&0x14)));
        }
    }

    #[test]
    fn late_result_cannot_paint_into_a_reopened_attention_screen() {
        let screen = Screen::new();
        assert!(screen.attempt.commit());
        screen.attempt.cancel();
        let mut runtime = screen.attempt.runtime.lock().unwrap();
        runtime
            .attention(&crate::input::test_origin(), false)
            .unwrap();
        runtime
            .attention(&crate::input::test_origin(), true)
            .unwrap();
        drop(runtime);
        let before = std::fs::read(&screen.path).unwrap();
        screen
            .attempt
            .notice(crate::attention::Notice::Unlocked)
            .unwrap();
        assert_eq!(std::fs::read(&screen.path).unwrap(), before);
    }

    fn enrollment(recovery: Recovery, step: Enrollment) -> Request {
        Request::new(
            [42; 32],
            1000,
            Operation::Enroll {
                platform: Platform::TpmPcr7,
                recovery,
                step,
            },
        )
        .unwrap()
    }
    fn description(tag: &[u8], request: &Request) -> Vec<u8> {
        [tag, request.encode().as_slice()].concat()
    }

    #[test]
    fn pending_write_displays_the_root_target_and_requires_one_exact_commit() {
        for role in [Role::Primary, Role::Recovery] {
            let mut screen = Screen::new();
            Arc::get_mut(&mut screen.attempt).unwrap().selection = Selection::Write;
            let request = Request::new([43; 32], 1000, Operation::Set {
                application: "mail".into(), name: "main".into(), application_uid: 65537,
                requester: 1000, role,
            }).unwrap();
            let mut wire = wire(vec![
                description(&[0x92], &request), description(&[0x91, 4], &request), vec![0x93],
                description(&[0x91, 5], &request), vec![0x94], description(&[0x91, 6], &request),
            ]);
            let mut client = Client::default();
            client.start(&mut wire, Arc::clone(&screen.attempt)).unwrap();
            let before = std::fs::read(&screen.path).unwrap();
            client.tick(&mut wire).unwrap();
            assert_ne!(std::fs::read(&screen.path).unwrap(), before);
            assert_eq!(client.pending.as_ref().unwrap().receipt, Some(request.clone()));
            client.tick(&mut wire).unwrap();
            client.tick(&mut wire).unwrap();
            assert!(client.pending.is_none());
            assert_eq!(wire.calls, [vec![0x18], vec![0x11], description(&[0x13], &request),
                vec![0x11], description(&[0x14], &request), vec![0x11]]);
        }
    }

    #[test]
    fn empty_write_queue_is_a_notice_and_a_different_operation_is_refused() {
        for response in [vec![0x98, 0], tagged(&[0x92])] {
            let mut screen = Screen::new();
            Arc::get_mut(&mut screen.attempt).unwrap().selection = Selection::Write;
            let mut client = Client::default();
            let no_write = response == [0x98, 0];
            let mut wire = wire(vec![response]);
            assert_eq!(client.start(&mut wire, Arc::clone(&screen.attempt)).is_ok(), no_write);
            assert!(client.pending.is_none());
            assert_eq!(wire.calls, [vec![0x18]]);
        }
    }
    fn enrollment_screen(recovery: Recovery) -> Screen {
        let mut screen = Screen::new();
        Arc::get_mut(&mut screen.attempt).unwrap().selection = Selection::Enroll(recovery);
        screen
    }

    #[test]
    fn failed_or_invalid_successor_consumes_the_presentation_slot() {
        for invalid in [true, false] {
            let screen = enrollment_screen(Recovery::SecondToken);
            let first = enrollment(Recovery::SecondToken, Enrollment::CreatePrimary);
            let next = first.following_enrollment_step().unwrap().unwrap();
            let mut runtime = screen.attempt.runtime.lock().unwrap();
            runtime
                .present_attention_request_with_time(&crate::input::test_origin(), first, Some(120))
                .unwrap();
            let request = if invalid {
                enrollment(Recovery::SecondToken, Enrollment::CreateRecovery)
            } else {
                next.clone()
            };
            // A zero budget fails raster preparation after admitting the successor.
            let remaining = if invalid { Some(119) } else { Some(0) };
            assert!(runtime
                .present_attention_request_with_time(
                    &crate::input::test_origin(),
                    request,
                    remaining
                )
                .is_err());
            assert!(runtime
                .present_attention_request_with_time(&crate::input::test_origin(), next, Some(119))
                .is_err());
        }
    }

    #[test]
    fn enrollment_inspects_then_presents_each_exact_step_before_one_commit() {
        for recovery in [Recovery::Unrecoverable, Recovery::SecondToken] {
            let screen = enrollment_screen(recovery);
            let initial = enrollment(recovery, Enrollment::CreatePrimary);
            let mut steps = vec![initial.clone()];
            while let Some(next) = steps.last().unwrap().following_enrollment_step().unwrap() {
                steps.push(next);
            }
            let last = steps.last().unwrap();
            let mut replies = vec![vec![0x97], vec![0x91, 9, 0], description(&[0x92], &initial)];
            for step in &steps {
                replies.extend([description(&[0x91, 4], step), vec![0x93]]);
            }
            replies.extend([
                description(&[0x91, 5], last),
                vec![0x94],
                description(&[0x91, 6], last),
            ]);
            let mut wire = wire(replies);
            let mut client = Client::default();
            client
                .start(&mut wire, Arc::clone(&screen.attempt))
                .unwrap();
            client.tick(&mut wire).unwrap();
            for step in &steps {
                let previous = std::fs::read(&screen.path).unwrap();
                client.tick(&mut wire).unwrap();
                assert_ne!(std::fs::read(&screen.path).unwrap(), previous);
                assert_eq!(
                    client.pending.as_ref().unwrap().receipt.as_ref(),
                    Some(step)
                );
            }
            client.tick(&mut wire).unwrap();
            client.tick(&mut wire).unwrap();
            assert!(client.pending.is_none());
            assert_eq!(
                wire.calls
                    .iter()
                    .filter(|call| call.first() == Some(&0x13))
                    .count(),
                steps.len()
            );
            assert_eq!(
                wire.calls
                    .iter()
                    .filter(|call| call.first() == Some(&0x14))
                    .count(),
                1
            );
        }
    }

    #[test]
    fn enrollment_refuses_skipped_steps_and_commit_before_final_proof() {
        for status in [4, 5] {
            let recovery = Recovery::SecondToken;
            let screen = enrollment_screen(recovery);
            let initial = enrollment(recovery, Enrollment::CreatePrimary);
            let forged = if status == 4 {
                enrollment(recovery, Enrollment::CreateRecovery)
            } else {
                initial.clone()
            };
            let mut wire = wire(vec![
                vec![0x97],
                vec![0x91, 9, 1],
                description(&[0x92], &initial),
                description(&[0x91, 4], &initial),
                vec![0x93],
                description(&[0x91, status], &forged),
            ]);
            let mut client = Client::default();
            client
                .start(&mut wire, Arc::clone(&screen.attempt))
                .unwrap();
            client.tick(&mut wire).unwrap();
            client.tick(&mut wire).unwrap();
            assert!(client.tick(&mut wire).is_err());
            assert!(!wire.calls.iter().any(|call| call.first() == Some(&0x14)));
        }
    }

    #[test]
    fn root_successor_timeout_waits_for_cleanup_then_inspects_without_acknowledging() {
        let recovery = Recovery::SecondToken;
        let screen = enrollment_screen(recovery);
        let first = enrollment(recovery, Enrollment::CreatePrimary);
        let next = first.following_enrollment_step().unwrap().unwrap();
        let mut wire = wire(vec![
            vec![0x97],
            vec![0x91, 9, 0],
            description(&[0x92], &first),
            description(&[0x91, 4], &first),
            vec![0x93],
            description(&[0x91, 3], &next),
            description(&[0x91, 7], &next),
            vec![0x97],
            vec![0x91, 9, 0],
        ]);
        let mut client = Client::default();
        client
            .start(&mut wire, Arc::clone(&screen.attempt))
            .unwrap();
        client.tick(&mut wire).unwrap();
        client.tick(&mut wire).unwrap();
        assert!(!client.pending.as_ref().unwrap().cancelled);
        client.tick(&mut wire).unwrap();
        assert_eq!(client.pending.as_ref().unwrap().request, next);
        assert!(client.pending.as_ref().unwrap().receipt.is_none());
        client.tick(&mut wire).unwrap();
        client.tick(&mut wire).unwrap();
        assert!(client.pending.is_none() && client.inspection.is_none());
        assert_eq!(
            wire.calls
                .iter()
                .filter(|call| call.first() == Some(&0x13))
                .count(),
            1
        );
        assert_eq!(
            wire.calls
                .iter()
                .filter(|call| call.first() == Some(&0x17))
                .count(),
            2
        );
        assert!(!wire.calls.iter().any(|call| call.first() == Some(&0x14)));
    }

    #[test]
    fn failed_enrollment_uses_a_new_inspection_and_never_retries_the_operation() {
        for state in [0, 2, 3] {
            let recovery = Recovery::SecondToken;
            let screen = enrollment_screen(recovery);
            let initial = enrollment(recovery, Enrollment::CreatePrimary);
            let mut wire = wire(vec![
                vec![0x97],
                vec![0x91, 9, 0],
                description(&[0x92], &initial),
                description(&[0x91, 7], &initial),
                vec![0x97],
                vec![0x91, 9, state],
            ]);
            let mut client = Client::default();
            client
                .start(&mut wire, Arc::clone(&screen.attempt))
                .unwrap();
            client.tick(&mut wire).unwrap();
            client.tick(&mut wire).unwrap();
            client.tick(&mut wire).unwrap();
            assert!(client.pending.is_none() && client.inspection.is_none());
            assert_eq!(
                wire.calls
                    .iter()
                    .filter(|call| call.first() == Some(&0x16))
                    .count(),
                1
            );
            assert_eq!(
                wire.calls
                    .iter()
                    .filter(|call| call.first() == Some(&0x17))
                    .count(),
                2
            );
            assert!(!wire.calls.iter().any(|call| call.first() == Some(&0x12)));
        }
    }

    #[test]
    fn cancellation_or_token_state_during_inspection_never_starts_enrollment() {
        for cancel in [false, true] {
            let screen = enrollment_screen(Recovery::Unrecoverable);
            let mut wire = wire(vec![vec![0x97], vec![0x91, 9, if cancel { 0 } else { 3 }]]);
            let mut client = Client::default();
            client
                .start(&mut wire, Arc::clone(&screen.attempt))
                .unwrap();
            if cancel {
                screen.attempt.cancel();
            }
            client.tick(&mut wire).unwrap();
            assert!(client.pending.is_none() && client.inspection.is_none());
            assert_eq!(wire.calls, [vec![0x17], vec![0x11]]);
        }
    }
}
