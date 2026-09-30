//! The root installation service core: one held review and its disk claim,
//! answered under INSTALLER.md "Installation service protocol" over one
//! stream. It writes nothing. Consent is not connected yet, so execute is
//! refused as consent unavailable once the held disk rechecks.

use std::io::{self, Read, Write};

use crate::installation_plan::{Candidates, Destination, Plan, Settings};
use crate::installation_protocol::{
    check_greeting, frame, payload_len, Abandon, Refusal, Reply, Request, ReviewNonce, State,
    GREETING, MAX_REPLY_BYTES, MAX_REQUEST_BYTES,
};

/// What the service observes and holds. Production reads the machine;
/// tests supply fakes. A refusal leaves nothing written.
pub(crate) trait Host {
    /// Held until the review ends; dropping it releases the disk.
    type Claim;
    fn candidates(&mut self) -> Result<Candidates, Refusal>;
    fn check_settings(&mut self, settings: &Settings) -> Result<(), Refusal>;
    fn claim(&mut self, destination: &Destination) -> Result<Self::Claim, Refusal>;
    /// The authenticated deployment manifest digest.
    fn authenticate_source(&mut self) -> Result<[u8; 32], Refusal>;
    fn recheck(&mut self, destination: &Destination, claim: &mut Self::Claim) -> bool;
    /// A proposal nonce and volume UUID. Failure ends the service.
    fn entropy(&mut self) -> io::Result<([u8; 32], [u8; 16])>;
}

enum Held<C> {
    Idle,
    Reviewed {
        plan: Box<Plan>,
        claim: C,
    },
    /// A review that ended before any write; its claim is released.
    /// Nothing can complete or fail until execution exists. Each match on
    /// `Held` is exhaustive, so adding those states must decide their
    /// admission, including busy after completion.
    Abandoned(ReviewNonce, Abandon),
}

pub(crate) struct Service<H: Host> {
    host: H,
    held: Held<H::Claim>,
}

impl<H: Host> Service<H> {
    pub(crate) fn new(host: H) -> Self {
        Self {
            host,
            held: Held::Idle,
        }
    }

    pub(crate) fn state(&self) -> State {
        match &self.held {
            Held::Idle => State::Idle,
            Held::Reviewed { plan, .. } => State::Reviewed(ReviewNonce::from(&**plan)),
            Held::Abandoned(nonce, cause) => State::Abandoned(*nonce, *cause),
        }
    }

    /// One reply per request. An error ends the service and its claim.
    pub(crate) fn answer(&mut self, request: Request) -> io::Result<Reply> {
        Ok(match request {
            Request::Status => Reply::Status(self.state()),
            Request::Destinations => match self.host.candidates() {
                Ok(candidates) => Reply::Destinations(candidates),
                Err(refusal) => Reply::Refused(refusal),
            },
            Request::Propose {
                destination,
                settings,
            } => match self.propose(destination, settings)? {
                Ok(plan) => Reply::Reviewed(Box::new(plan)),
                Err(refusal) => Reply::Refused(refusal),
            },
            Request::Execute(plan) => self.execute(&plan),
            Request::Withdraw(nonce) => self.withdraw(nonce),
        })
    }

    fn propose(
        &mut self,
        destination: Destination,
        settings: Settings,
    ) -> io::Result<Result<Plan, Refusal>> {
        match &self.held {
            Held::Reviewed { .. } => return Ok(Err(Refusal::Busy)),
            Held::Idle | Held::Abandoned(..) => {}
        }
        if let Err(refusal) = self.host.check_settings(&settings) {
            return Ok(Err(refusal));
        }
        // The installer's destination only selects: it must equal the
        // service's own observation, labels included, and that observation
        // is what the review carries.
        let destination = match self.host.candidates() {
            Ok(candidates) => match candidates.as_slice().iter().find(|d| **d == destination) {
                Some(observed) => observed.clone(),
                None => return Ok(Err(Refusal::DestinationChanged)),
            },
            Err(refusal) => return Ok(Err(refusal)),
        };
        let mut claim = match self.host.claim(&destination) {
            Ok(claim) => claim,
            Err(refusal) => return Ok(Err(refusal)),
        };
        let deployment = match self.host.authenticate_source() {
            Ok(deployment) => deployment,
            Err(refusal) => return Ok(Err(refusal)),
        };
        let (nonce, uuid) = self.host.entropy()?;
        let plan = Plan::new(nonce, destination, deployment, uuid, settings)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        // Source authentication can take a while; the disk must not have
        // changed under the claim meanwhile.
        if !self.host.recheck(plan.destination(), &mut claim) {
            return Ok(Err(Refusal::DestinationChanged));
        }
        self.held = Held::Reviewed {
            plan: Box::new(plan.clone()),
            claim,
        };
        Ok(Ok(plan))
    }

    fn execute(&mut self, echoed: &Plan) -> Reply {
        let (plan, claim) = match &mut self.held {
            Held::Reviewed { plan, claim } => (plan, claim),
            Held::Idle | Held::Abandoned(..) => return Reply::Refused(Refusal::NoReview),
        };
        if **plan != *echoed {
            return Reply::Refused(Refusal::StaleReview);
        }
        if !self.host.recheck(plan.destination(), claim) {
            let nonce = ReviewNonce::from(&**plan);
            self.held = Held::Abandoned(nonce, Abandon::DestinationChanged);
            return Reply::Status(State::Abandoned(nonce, Abandon::DestinationChanged));
        }
        Reply::Refused(Refusal::ConsentUnavailable)
    }

    fn withdraw(&mut self, nonce: ReviewNonce) -> Reply {
        let plan = match &self.held {
            Held::Reviewed { plan, .. } => plan,
            Held::Idle | Held::Abandoned(..) => return Reply::Refused(Refusal::NoReview),
        };
        if ReviewNonce::from(&**plan) != nonce {
            return Reply::Refused(Refusal::StaleReview);
        }
        self.held = Held::Abandoned(nonce, Abandon::Withdrawn);
        Reply::Status(State::Abandoned(nonce, Abandon::Withdrawn))
    }
}

/// Serve one installer over `stream` until it closes. A malformed frame or
/// message ends the channel without a reply; so does any service error.
/// The service ends with the channel, releasing any held claim: no review
/// outlives its installer.
pub(crate) fn serve<H: Host>(
    stream: &mut (impl Read + Write),
    mut service: Service<H>,
) -> io::Result<()> {
    stream.write_all(GREETING)?;
    stream.flush()?;
    let mut greeting = [0; 8];
    stream.read_exact(&mut greeting)?;
    check_greeting(&greeting).map_err(invalid)?;
    loop {
        let Some(header) = read_header(stream)? else {
            return Ok(());
        };
        let length = payload_len(header, MAX_REQUEST_BYTES).map_err(invalid)?;
        let mut payload = vec![0; length];
        stream.read_exact(&mut payload)?;
        let request = Request::decode(&payload).map_err(invalid)?;
        let reply = service.answer(request)?;
        stream.write_all(&frame(&reply.encode(), MAX_REPLY_BYTES).map_err(invalid)?)?;
        stream.flush()?;
    }
}

/// A frame header, or `None` when the peer closed between frames.
fn read_header(stream: &mut impl Read) -> io::Result<Option<[u8; 4]>> {
    let mut header = [0; 4];
    let mut filled = 0;
    while let Some(rest) = header.get_mut(filled..).filter(|rest| !rest.is_empty()) {
        match stream.read(rest) {
            Ok(0) if filled == 0 => return Ok(None),
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(count) => filled += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(Some(header))
}

fn invalid(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::installation_plan::DestinationObservation;
    use std::cell::RefCell;
    use std::os::unix::net::UnixStream;
    use std::rc::Rc;

    fn disk(name: &str, sequence: u64, model: Option<&str>) -> Destination {
        Destination::new(DestinationObservation {
            name,
            major: 253,
            minor: sequence as u32,
            sequence,
            capacity: 8 << 30,
            sector: 512,
            removable: false,
            model,
            serial: None,
            wwid: None,
        })
        .unwrap()
    }
    fn settings() -> Settings {
        Settings::new("alice", "td-laptop", "us", "Etc/UTC").unwrap()
    }

    /// Records every host call; `live` counts claims not yet dropped.
    #[derive(Default)]
    struct Log {
        calls: Vec<&'static str>,
        live: usize,
    }
    struct Claim(Rc<RefCell<Log>>);
    impl Drop for Claim {
        fn drop(&mut self) {
            self.0.borrow_mut().live -= 1;
        }
    }
    struct Fake {
        log: Rc<RefCell<Log>>,
        disks: Vec<Destination>,
        settings: Result<(), Refusal>,
        claim: Result<(), Refusal>,
        source: Result<[u8; 32], Refusal>,
        rechecks: Vec<bool>,
        entropy: bool,
    }
    impl Fake {
        fn new() -> Self {
            Self {
                log: Rc::default(),
                disks: vec![disk("vda", 1, Some("Disk")), disk("vdb", 2, None)],
                settings: Ok(()),
                claim: Ok(()),
                source: Ok([0xab; 32]),
                rechecks: Vec::new(),
                entropy: true,
            }
        }
        fn call(&self, name: &'static str) {
            self.log.borrow_mut().calls.push(name);
        }
    }
    impl Host for Fake {
        type Claim = Claim;
        fn candidates(&mut self) -> Result<Candidates, Refusal> {
            self.call("candidates");
            Candidates::new(self.disks.clone()).map_err(|_| Refusal::DiscoveryFailed)
        }
        fn check_settings(&mut self, _: &Settings) -> Result<(), Refusal> {
            self.call("settings");
            self.settings
        }
        fn claim(&mut self, _: &Destination) -> Result<Claim, Refusal> {
            self.call("claim");
            self.claim?;
            self.log.borrow_mut().live += 1;
            Ok(Claim(Rc::clone(&self.log)))
        }
        fn authenticate_source(&mut self) -> Result<[u8; 32], Refusal> {
            self.call("source");
            // The source is authenticated while the disk is held.
            assert_eq!(self.log.borrow().live, 1);
            self.source
        }
        fn recheck(&mut self, _: &Destination, _: &mut Claim) -> bool {
            self.call("recheck");
            if self.rechecks.is_empty() {
                true
            } else {
                self.rechecks.remove(0)
            }
        }
        fn entropy(&mut self) -> io::Result<([u8; 32], [u8; 16])> {
            self.call("entropy");
            if !self.entropy {
                return Err(io::ErrorKind::Other.into());
            }
            let mut uuid = [0x11; 16];
            uuid[6] = 0x41;
            uuid[8] = 0x81;
            Ok(([0x5a; 32], uuid))
        }
    }

    fn propose(disk: Destination) -> Request {
        Request::Propose {
            destination: disk,
            settings: settings(),
        }
    }

    fn reviewed(service: &mut Service<Fake>) -> Plan {
        match service
            .answer(propose(disk("vda", 1, Some("Disk"))))
            .unwrap()
        {
            Reply::Reviewed(plan) => *plan,
            other => panic!("{other:?}"),
        }
    }

    fn live(service: &Service<Fake>) -> usize {
        service.host.log.borrow().live
    }

    #[test]
    fn propose_reviews_the_service_observation_under_its_claim() {
        let mut service = Service::new(Fake::new());
        let plan = reviewed(&mut service);
        assert_eq!(plan.destination(), &disk("vda", 1, Some("Disk")));
        assert_eq!(plan.deployment(), &[0xab; 32]);
        assert_eq!(plan.nonce(), &[0x5a; 32]);
        assert_eq!(plan.settings(), &settings());
        assert_eq!(service.state(), State::Reviewed(ReviewNonce::from(&plan)));
        assert_eq!(live(&service), 1);
        assert_eq!(
            service.host.log.borrow().calls,
            [
                "settings",
                "candidates",
                "claim",
                "source",
                "entropy",
                "recheck"
            ]
        );
    }

    #[test]
    fn a_destination_differing_in_any_field_is_refused_before_a_claim() {
        let observed = DestinationObservation {
            name: "vda",
            major: 253,
            minor: 1,
            sequence: 1,
            capacity: 8 << 30,
            sector: 512,
            removable: false,
            model: Some("Disk"),
            serial: None,
            wwid: None,
        };
        let changes: [fn(&mut DestinationObservation<'_>); 12] = [
            |d| d.name = "vdc",
            |d| d.major = 254,
            |d| d.minor = 2,
            |d| d.sequence = 9,
            |d| d.capacity = 16 << 30,
            |d| d.sector = 4096,
            |d| d.removable = true,
            |d| d.model = None,
            |d| d.model = Some(""),
            |d| d.model = Some("Disk "),
            |d| d.serial = Some(""),
            |d| d.wwid = Some("naa.1"),
        ];
        assert_eq!(
            Destination::new(DestinationObservation { ..observed }).unwrap(),
            disk("vda", 1, Some("Disk"))
        );
        for change in changes {
            let mut differing = DestinationObservation { ..observed };
            change(&mut differing);
            let chosen = Destination::new(differing).unwrap();
            let mut service = Service::new(Fake::new());
            assert_eq!(
                service.answer(propose(chosen)).unwrap(),
                Reply::Refused(Refusal::DestinationChanged)
            );
            assert!(!service.host.log.borrow().calls.contains(&"claim"));
            assert_eq!(service.state(), State::Idle);
        }
    }

    #[test]
    fn each_refusal_stops_before_the_next_step_and_releases_the_claim() {
        let cases: [(fn(&mut Fake), Refusal, &[&str]); 5] = [
            (
                |f| f.settings = Err(Refusal::InvalidUsername),
                Refusal::InvalidUsername,
                &["settings"],
            ),
            (
                |f| f.disks = vec![disk("vda", 1, None), disk("vda", 2, None)],
                Refusal::DiscoveryFailed,
                &["settings", "candidates"],
            ),
            (
                |f| f.claim = Err(Refusal::DestinationBusy),
                Refusal::DestinationBusy,
                &["settings", "candidates", "claim"],
            ),
            (
                |f| f.source = Err(Refusal::SourceUnavailable),
                Refusal::SourceUnavailable,
                &["settings", "candidates", "claim", "source"],
            ),
            (
                |f| f.rechecks = vec![false],
                Refusal::DestinationChanged,
                &[
                    "settings",
                    "candidates",
                    "claim",
                    "source",
                    "entropy",
                    "recheck",
                ],
            ),
        ];
        for (setup, refusal, calls) in cases {
            let mut fake = Fake::new();
            setup(&mut fake);
            let mut service = Service::new(fake);
            assert_eq!(
                service
                    .answer(propose(disk("vda", 1, Some("Disk"))))
                    .unwrap(),
                Reply::Refused(refusal)
            );
            assert_eq!(service.host.log.borrow().calls, calls);
            assert_eq!(live(&service), 0, "{refusal:?}");
            assert_eq!(service.state(), State::Idle);
        }
    }

    #[test]
    fn failed_entropy_ends_the_service_and_releases_the_claim() {
        let mut fake = Fake::new();
        fake.entropy = false;
        let mut service = Service::new(fake);
        assert!(service
            .answer(propose(disk("vda", 1, Some("Disk"))))
            .is_err());
        assert_eq!(live(&service), 0);
    }

    #[test]
    fn execute_rechecks_then_refuses_until_consent_exists() {
        let mut service = Service::new(Fake::new());
        assert_eq!(
            service.answer(Request::Execute(sample_plan())).unwrap(),
            Reply::Refused(Refusal::NoReview)
        );
        let plan = reviewed(&mut service);
        assert_eq!(
            service.answer(Request::Execute(sample_plan())).unwrap(),
            Reply::Refused(Refusal::StaleReview)
        );
        for _ in 0..2 {
            assert_eq!(
                service.answer(Request::Execute(plan.clone())).unwrap(),
                Reply::Refused(Refusal::ConsentUnavailable)
            );
            assert_eq!(service.state(), State::Reviewed(ReviewNonce::from(&plan)));
            assert_eq!(live(&service), 1);
        }
        service.host.rechecks = vec![false];
        let abandoned = State::Abandoned(ReviewNonce::from(&plan), Abandon::DestinationChanged);
        assert_eq!(
            service.answer(Request::Execute(plan.clone())).unwrap(),
            Reply::Status(abandoned)
        );
        assert_eq!(live(&service), 0);
        assert_eq!(service.state(), abandoned);
        assert_eq!(
            service.answer(Request::Execute(plan)).unwrap(),
            Reply::Refused(Refusal::NoReview)
        );
        // Abandoned admits a new review.
        reviewed(&mut service);
        assert_eq!(live(&service), 1);
    }

    #[test]
    fn withdraw_releases_only_the_held_review() {
        let mut service = Service::new(Fake::new());
        let other = ReviewNonce::new([9; 32]).unwrap();
        assert_eq!(
            service.answer(Request::Withdraw(other)).unwrap(),
            Reply::Refused(Refusal::NoReview)
        );
        let plan = reviewed(&mut service);
        assert_eq!(
            service.answer(Request::Withdraw(other)).unwrap(),
            Reply::Refused(Refusal::StaleReview)
        );
        assert_eq!(live(&service), 1);
        let nonce = ReviewNonce::from(&plan);
        let withdrawn = State::Abandoned(nonce, Abandon::Withdrawn);
        assert_eq!(
            service.answer(Request::Withdraw(nonce)).unwrap(),
            Reply::Status(withdrawn)
        );
        assert_eq!(live(&service), 0);
        assert_eq!(
            service.answer(Request::Withdraw(nonce)).unwrap(),
            Reply::Refused(Refusal::NoReview)
        );
    }

    #[test]
    fn propose_is_busy_while_reviewed_and_admitted_after_abandonment() {
        let mut service = Service::new(Fake::new());
        reviewed(&mut service);
        service.host.log.borrow_mut().calls.clear();
        assert_eq!(
            service.answer(propose(disk("vdb", 2, None))).unwrap(),
            Reply::Refused(Refusal::Busy)
        );
        // Busy wins: nothing else was consulted.
        assert!(service.host.log.borrow().calls.is_empty());
        assert_eq!(live(&service), 1);

        for cause in Abandon::ALL {
            let mut service = Service::new(Fake::new());
            service.held = Held::Abandoned(nonce(), *cause);
            let reply = service.answer(propose(disk("vdb", 2, None))).unwrap();
            assert!(matches!(reply, Reply::Reviewed(_)), "{cause:?}");
        }
    }

    fn nonce() -> ReviewNonce {
        ReviewNonce::new([3; 32]).unwrap()
    }

    #[test]
    fn status_and_destinations_answer_in_every_state() {
        let mut held = Service::new(Fake::new());
        let plan = reviewed(&mut held);
        let mut services = vec![
            (State::Idle, Service::new(Fake::new())),
            (State::Reviewed(ReviewNonce::from(&plan)), held),
        ];
        for cause in Abandon::ALL {
            let mut service = Service::new(Fake::new());
            service.held = Held::Abandoned(nonce(), *cause);
            services.push((State::Abandoned(nonce(), *cause), service));
        }
        for (state, mut service) in services {
            assert_eq!(
                service.answer(Request::Status).unwrap(),
                Reply::Status(state)
            );
            let reply = service.answer(Request::Destinations).unwrap();
            assert!(matches!(reply, Reply::Destinations(_)), "{state:?}");
            assert_eq!(service.state(), state);
        }
    }

    fn sample_plan() -> Plan {
        let mut uuid = [0x22; 16];
        uuid[6] = 0x42;
        uuid[8] = 0x82;
        Plan::new(
            [0x33; 32],
            disk("vda", 1, Some("Disk")),
            [0xab; 32],
            uuid,
            settings(),
        )
        .unwrap()
    }

    fn exchange(stream: &mut UnixStream, request: &Request) -> Reply {
        stream
            .write_all(&frame(&request.encode(), MAX_REQUEST_BYTES).unwrap())
            .unwrap();
        let mut header = [0; 4];
        stream.read_exact(&mut header).unwrap();
        let mut payload = vec![0; payload_len(header, MAX_REPLY_BYTES).unwrap()];
        stream.read_exact(&mut payload).unwrap();
        Reply::decode(&payload).unwrap()
    }

    /// The serve result, and the claims still live after it returned.
    fn served(client: impl FnOnce(&mut UnixStream) + Send + 'static) -> (io::Result<()>, usize) {
        let (mut ours, theirs) = UnixStream::pair().unwrap();
        let peer = std::thread::spawn(move || {
            let mut theirs = theirs;
            client(&mut theirs);
        });
        let fake = Fake::new();
        let log = Rc::clone(&fake.log);
        let result = serve(&mut ours, Service::new(fake));
        drop(ours);
        peer.join().unwrap();
        let live = log.borrow().live;
        (result, live)
    }

    fn greet(stream: &mut UnixStream) {
        let mut greeting = [0; 8];
        stream.read_exact(&mut greeting).unwrap();
        assert_eq!(&greeting, GREETING);
        stream.write_all(GREETING).unwrap();
    }

    #[test]
    fn the_stream_greets_answers_and_releases_the_claim_at_close() {
        let (result, live) = served(|stream| {
            greet(stream);
            assert_eq!(
                exchange(stream, &Request::Status),
                Reply::Status(State::Idle)
            );
            let reply = exchange(stream, &propose(disk("vda", 1, Some("Disk"))));
            assert!(matches!(reply, Reply::Reviewed(_)), "{reply:?}");
        });
        result.unwrap();
        assert_eq!(live, 0);
    }

    #[test]
    fn a_bad_greeting_or_frame_ends_the_channel_without_a_reply() {
        let clients: [fn(&mut UnixStream); 5] = [
            |s| s.write_all(b"TDINS02\n").unwrap(),
            |s| {
                s.write_all(&[GREETING.as_slice(), &[0, 0, 0, 0]].concat())
                    .unwrap()
            },
            |s| {
                s.write_all(&[GREETING.as_slice(), &[0, 0, 8, 2]].concat())
                    .unwrap()
            },
            |s| {
                s.write_all(&[GREETING.as_slice(), &[0, 0, 0, 1, 0x7f]].concat())
                    .unwrap()
            },
            |s| {
                s.write_all(&[GREETING.as_slice(), &[0, 0, 0, 2, 0x04, 0x00]].concat())
                    .unwrap()
            },
        ];
        for client in clients {
            let (result, _) = served(move |stream| {
                client(stream);
                stream.shutdown(std::net::Shutdown::Write).unwrap();
                let mut rest = Vec::new();
                stream.read_to_end(&mut rest).unwrap();
                // Only the service's own greeting was ever sent.
                assert_eq!(rest, GREETING);
            });
            assert!(result.is_err());
        }
    }

    #[test]
    fn a_frame_cut_short_is_an_error_but_a_close_between_frames_is_not() {
        let (result, _) = served(|stream| {
            greet(stream);
            stream.write_all(&[0, 0]).unwrap();
        });
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
        let (result, _) = served(greet);
        result.unwrap();
    }
}
