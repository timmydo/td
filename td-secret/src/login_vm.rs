// The qemu-secret login guests (td-secret/DESIGN.md, "Login-key worker
// guests"), included into login_operation's tests for their root and keys.
// Each runs the worker's own `operate` over `Physical`: Device::discover,
// Session::open and each session's production HID worker with its
// operation lock, against fido_virtual keys that fido_uhid presents as
// hidraw devices. Root is the tests' own, over a socketpair Wire. The
// power-cut guest ("Login power-cut guests") keeps its record and keys on a
// disposable disk across cold boots.

use super::*;
use crate::fido_device::Device;
use crate::fido_uhid::{guard, Plugged, Served, KEEPALIVE_PERIOD};
use crate::fido_virtual::Presence;
use std::os::unix::fs::FileTypeExt;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

const NAME: &str = "td login key";
/// Each session's HID worker takes this with a nonblocking flock.
const LOCK: &str = "/run/td-fido/operation.lock";
/// How long a swap waits to see the worker poll in each of its states.
const GATE: Duration = Duration::from_secs(20);
/// The slow touch: thirty keepalive intervals.
const TOUCH: Duration = Duration::from_secs(3);
/// The longest silence the keepalive guest admits: well past the interval,
/// for a loaded guest's scheduling, and still a third of the touch.
const SILENCE: Duration = Duration::from_secs(1);

type Port = Arc<Mutex<Option<Plugged>>>;
type Swapped = Arc<Mutex<Option<JoinHandle<(Served, bool)>>>>;

fn directory() -> &'static Path {
    Path::new(login_store::DIRECTORY)
}

/// The production directory, root's with mode 0700 as firstboot makes it,
/// and no key yet.
fn prepare() {
    assert!(!directory().exists());
    fs::create_dir_all(directory().parent().unwrap()).unwrap();
    fs::DirBuilder::new()
        .mode(0o700)
        .create(directory())
        .unwrap();
    fs::set_permissions(directory(), Permissions::from_mode(0o700)).unwrap();
    assert!(matches!(state(), State::Unenrolled));
    assert!(Device::discover().unwrap().is_empty());
}

fn state() -> State {
    login_store::read(directory(), Owner::ROOT, UID)
}

fn seed(bytes: &[u8]) {
    replace(directory(), Some(bytes));
}

/// The stored slots' credentials in canonical order; none unenrolled.
fn stored() -> Option<Vec<Vec<u8>>> {
    match state() {
        State::Unenrolled => None,
        State::Enrolled(record) => Some(
            record
                .slots()
                .iter()
                .map(|slot| slot.credential().to_vec())
                .collect(),
        ),
        State::Unavailable(cause) => panic!("login state unavailable: {cause:?}"),
    }
}

fn fingerprints() -> Vec<Fingerprint> {
    stored()
        .unwrap_or_default()
        .iter()
        .map(|id| fingerprint(id))
        .collect()
}

fn names() -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(directory())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

/// `run`'s context but for the versions: both retained deployments read
/// this build's, as increment 4's tier marker will say. Until then
/// production passes none, and every write that leaves a record refuses.
fn context() -> Context<'static> {
    writing(&write)
}

/// `context`, its store write replaced.
fn writing(write: &dyn Fn(&Store, Baseline, Change<'_>) -> Outcome) -> Context<'_> {
    Context {
        uid: UID,
        path: directory(),
        owner: Owner::ROOT,
        started: Instant::now(),
        lifetime: fido_device::MAX_LIFETIME,
        margin: COMMIT_MARGIN,
        current: READS,
        previous: READS,
        write,
    }
}

/// How many devices each of the worker's discoveries found, in order.
#[derive(Clone, Default)]
struct Polls(Arc<Mutex<Vec<usize>>>);

impl Polls {
    fn push(&self, found: usize) {
        self.0.lock().unwrap().push(found);
    }

    fn count(&self) -> usize {
        self.0.lock().unwrap().len()
    }

    /// Waits up to `within` for a discovery after the first `from` that
    /// found `devices`, and returns how many there were up to it.
    fn seen(&self, from: usize, devices: usize, within: Duration) -> Option<usize> {
        let deadline = Instant::now() + within;
        loop {
            let found = self.0.lock().unwrap().get(from..).and_then(|after| {
                after
                    .iter()
                    .position(|found| *found == devices)
                    .map(|at| from + at + 1)
            });
            if found.is_some() || Instant::now() >= deadline {
                return found;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

/// `Physical`, every call passed through unchanged, with each discovery's
/// count recorded so a swap can follow what the worker saw.
struct Observed<'a>(&'a Polls);

impl Devices for Observed<'_> {
    type Node = <Physical as Devices>::Node;
    type Channel = <Physical as Devices>::Channel;
    fn discover(&mut self) -> Result<Vec<Self::Node>, String> {
        let found = Physical.discover();
        if let Ok(nodes) = &found {
            self.0.push(nodes.len());
        }
        found
    }
    fn open(&mut self, node: Self::Node, deadline: Instant) -> Result<Self::Channel, String> {
        Physical.open(node, deadline)
    }
    fn pause(&mut self, time: Duration) {
        Physical.pause(time);
    }
}

/// One operation, started as `run` starts it, over the guest's devices,
/// with kernel entropy and the protected-memory check; root follows `plan`.
fn physical(plan: Plan) -> (Result<(), Failure>, Vec<Vec<u8>>) {
    watched(plan, &Polls::default())
}

/// `physical`, recording the worker's discoveries in `polls`.
fn watched(plan: Plan, polls: &Polls) -> (Result<(), Failure>, Vec<Vec<u8>>) {
    operated(plan, polls, &context())
}

/// `watched` with this context.
fn operated(
    plan: Plan,
    polls: &Polls,
    context: &Context<'_>,
) -> (Result<(), Failure>, Vec<Vec<u8>>) {
    let (worker, parent) = UnixStream::pair().unwrap();
    let authority = std::thread::spawn(move || root(parent, plan));
    let deadline = context.started.checked_add(TWO_CEREMONIES).unwrap();
    let mut wire = Wire::new(worker, deadline).unwrap();
    let mut random = File::open("/dev/urandom").unwrap();
    let result = operate(
        &mut wire,
        context,
        &mut Observed(polls),
        &mut |bytes| {
            random
                .read_exact(bytes)
                .map_err(|_| "read kernel entropy".into())
        },
        store::require_protected_memory,
    );
    drop(wire);
    let seen = authority.join().unwrap();
    if Path::new(LOCK).exists() {
        released();
    }
    (result, seen)
}

/// The sessions' HID workers made the lock root's alone and left it free.
fn released() {
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .open(LOCK)
        .unwrap();
    let meta = lock.metadata().unwrap();
    assert_eq!(
        (meta.uid(), meta.gid(), meta.mode() & 0o7777),
        (0, 0, 0o600)
    );
    lock.try_lock().unwrap();
}

/// Every key is out, and the sessions took the lock and released it.
fn finish() {
    assert!(Device::discover().unwrap().is_empty());
    assert!(Path::new(LOCK).exists());
    released();
}

/// A reinsertion: the device goes, the key power-cycles, a new device comes.
fn reinsert(plugged: Plugged, key: &Virtual) -> Plugged {
    plugged.remove();
    key.power_cycle();
    Plugged::insert(key, NAME)
}

/// A person's swap, gated on what the worker saw: `remove` runs once a
/// discovery after the first `start` found the old key alone, and `insert`
/// once a later one found the port empty, so the worker is seen to wait
/// through both. False when either was not seen within `within`; the swap
/// still finishes then, so the worker is not left waiting for a key.
fn gate(
    polls: &Polls,
    start: usize,
    within: Duration,
    remove: impl FnOnce(),
    insert: impl FnOnce(),
) -> bool {
    let alone = polls.seen(start, 1, within);
    remove();
    let empty = polls.seen(alone.unwrap_or_else(|| polls.count()), 0, within);
    insert();
    alone.is_some() && empty.is_some()
}

/// At step `at`, a person's swap of the plugged key for `next` (`gate`).
/// The thread returns what the old key served and whether the worker was
/// seen to poll with the old key alone and then with none. A second call
/// at the same step starts nothing.
fn swap(port: &Port, swapped: &Swapped, polls: &Polls, at: Request, next: &Virtual) -> Plan {
    let (port, swapped, polls, next) = (
        Arc::clone(port),
        Arc::clone(swapped),
        polls.clone(),
        next.clone(),
    );
    Plan {
        admitted: Some(Arc::new(move |step: &Request| {
            let mut started = swapped.lock().unwrap();
            if *step != at || started.is_some() {
                return;
            }
            // Root has not yet acknowledged the step, so the worker has not
            // yet polled for the new key.
            let start = polls.count();
            let (port, polls, next) = (Arc::clone(&port), polls.clone(), next.clone());
            *started = Some(std::thread::spawn(move || {
                let mut served = Served::default();
                let gated = gate(
                    &polls,
                    start,
                    GATE,
                    || {
                        if let Some(old) = port.lock().unwrap().take() {
                            served = old.remove();
                        }
                    },
                    || *port.lock().unwrap() = Some(Plugged::insert(&next, NAME)),
                );
                (served, gated)
            }));
        })),
        ..Plan::default()
    }
}

/// What the swapped-out key served; the swap must have been gated.
fn swapped(swapped: &Swapped) -> Served {
    let (served, gated) = swapped.lock().unwrap().take().unwrap().join().unwrap();
    assert!(gated, "the worker was not seen polling through the swap");
    served
}

#[test]
fn a_swap_follows_the_worker_through_the_old_key_and_the_empty_port() {
    // A worker slow to start, as on a loaded host, polling a simulated
    // port: 1 the old key, 0 none, 2 the new key.
    let (polls, port) = (Polls::default(), Arc::new(Mutex::new(1u8)));
    let worker = {
        let (polls, port) = (polls.clone(), Arc::clone(&port));
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            let mut saw = Vec::new();
            loop {
                let now = *port.lock().unwrap();
                polls.push(usize::from(now != 0));
                if saw.last() != Some(&now) {
                    saw.push(now);
                }
                if now == 2 {
                    return saw;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        })
    };
    let gated = gate(
        &polls,
        0,
        Duration::from_secs(5),
        || *port.lock().unwrap() = 0,
        || *port.lock().unwrap() = 2,
    );
    assert!(gated);
    assert_eq!(worker.join().unwrap(), [1, 0, 2]);
    // A worker that never polls: the gate gives up, says so, and still swaps.
    let (polls, port) = (Polls::default(), Mutex::new(1u8));
    let gated = gate(
        &polls,
        0,
        Duration::from_millis(50),
        || *port.lock().unwrap() = 0,
        || *port.lock().unwrap() = 2,
    );
    assert!(!gated);
    assert_eq!(*port.lock().unwrap(), 2);
}

#[test]
#[ignore = "requires qemu-secret with a disposable guest and UHID"]
fn qemu_login_worker_unlocks_each_key_and_refuses_wrong_pins_strangers_and_counts() {
    guard("login-unlock");
    prepare();
    let keys = [Key::new(1), Key::new(2)];
    let all: Vec<&Key> = keys.iter().collect();
    seed(&record(&all, None, None));
    let order = canonical(&all);
    let identified = vec![baseline(&order), invitation(0x10, &identify(2))];
    // No key: ONE KEY, without waiting for one.
    assert_eq!(
        physical(pin(PIN)),
        (
            Err(Failure::OneKey),
            failed(identified.clone(), Failure::OneKey)
        )
    );
    for key in &keys {
        let before = key.device.transcript().len();
        let plugged = Plugged::insert(&key.device, NAME);
        assert_eq!(
            physical(pin(PIN)),
            (Ok(()), succeeded(&order, key.fingerprint(), 8))
        );
        let served = plugged.remove();
        // An identify session and an assertion session, every request
        // over the device.
        assert_eq!(served.channels, 2);
        assert_eq!(served.requests, key.device.transcript().len() - before);
    }
    // A wrong PIN reports the falling count; a later operation succeeds.
    let key = &keys[1];
    let plugged = Plugged::insert(&key.device, NAME);
    for retries in [8, 7] {
        assert_eq!(
            physical(pin(WRONG)),
            (
                Err(Failure::WrongPin(retries)),
                failed(
                    through_unlock(&order, key.fingerprint(), retries),
                    Failure::WrongPin(retries)
                )
            )
        );
    }
    assert_eq!(
        physical(pin(PIN)),
        (Ok(()), succeeded(&order, key.fingerprint(), 6))
    );
    assert_eq!(key.device.state().retries, 8);
    plugged.remove();
    // A key not in the record: NOT ENROLLED, and no PIN step.
    let stranger = Key::new(3);
    let tokens = stranger.pin_tokens();
    let plugged = Plugged::insert(&stranger.device, NAME);
    assert_eq!(
        physical(pin(PIN)),
        (
            Err(Failure::NotEnrolled),
            failed(identified.clone(), Failure::NotEnrolled)
        )
    );
    assert_eq!(stranger.pin_tokens(), tokens);
    plugged.remove();
    // Two keys: ONE KEY, and neither device sees a report.
    let before = [keys[0].device.transcript(), keys[1].device.transcript()];
    let one = Plugged::insert(&keys[0].device, "td login key 1");
    let two = Plugged::insert(&keys[1].device, "td login key 2");
    assert_eq!(
        physical(pin(PIN)),
        (Err(Failure::OneKey), failed(identified, Failure::OneKey))
    );
    assert_eq!([one.remove(), two.remove()], [Served::default(); 2]);
    assert_eq!(
        [keys[0].device.transcript(), keys[1].device.transcript()],
        before
    );
    assert_eq!(fingerprints(), order);
    finish();
}

#[test]
#[ignore = "requires qemu-secret with a disposable guest and UHID"]
fn qemu_login_worker_blocks_after_three_wrong_pins_until_the_key_is_reinserted() {
    guard("login-blocked");
    prepare();
    let key = Key::new(6);
    seed(&record(&[&key], None, None));
    let order = [key.fingerprint()];
    let wrong = |retries| {
        failed(
            through_unlock(&order, key.fingerprint(), retries),
            Failure::WrongPin(retries),
        )
    };
    let mut plugged = Plugged::insert(&key.device, NAME);
    assert_eq!(physical(pin(WRONG)), (Err(Failure::WrongPin(8)), wrong(8)));
    assert_eq!(physical(pin(WRONG)), (Err(Failure::WrongPin(7)), wrong(7)));
    assert_eq!(
        physical(pin(WRONG)),
        (
            Err(Failure::PinAuthBlocked),
            failed(
                through_unlock(&order, key.fingerprint(), 6),
                Failure::PinAuthBlocked
            )
        )
    );
    // Until reinserted the key gets no PIN step at all.
    let identified = vec![baseline(&order), invitation(0x10, &identify(1))];
    let tokens = key.pin_tokens();
    assert_eq!(
        physical(pin(PIN)),
        (
            Err(Failure::PinAuthBlocked),
            failed(identified.clone(), Failure::PinAuthBlocked)
        )
    );
    assert_eq!(key.pin_tokens(), tokens);
    plugged = reinsert(plugged, &key.device);
    assert_eq!(
        physical(pin(PIN)),
        (Ok(()), succeeded(&order, key.fingerprint(), 5))
    );
    assert_eq!(key.device.state().retries, 8);
    // One left: a wrong PIN blocks the key, reinserted or not.
    key.device.with_state(|state| state.retries = 1);
    assert_eq!(
        physical(pin(WRONG)),
        (
            Err(Failure::PinBlocked),
            failed(
                through_unlock(&order, key.fingerprint(), 1),
                Failure::PinBlocked
            )
        )
    );
    for _ in 0..2 {
        let tokens = key.pin_tokens();
        assert_eq!(
            physical(pin(PIN)),
            (
                Err(Failure::PinBlocked),
                failed(identified.clone(), Failure::PinBlocked)
            )
        );
        assert_eq!(key.pin_tokens(), tokens);
        plugged = reinsert(plugged, &key.device);
    }
    plugged.remove();
    finish();
}

#[test]
#[ignore = "requires qemu-secret with a disposable guest and UHID"]
fn qemu_login_worker_enrolls_one_key_whose_record_unlocks_and_is_then_removed() {
    guard("login-enroll-one");
    prepare();
    let key = blank("enroll-one");
    let plugged = Plugged::insert(&key, NAME);
    let make = |step| enrolling(1, 1, step);
    let (result, seen) = physical(begins(make(LoginStep::Connect)));
    assert_eq!(result, Ok(()));
    let id = newest(&key);
    let new = fingerprint(&id);
    let mut frames = vec![
        vec![0x18, 0],
        invitation(0x10, &login(make(LoginStep::Connect))),
    ];
    frames.extend(ceremony(make, new));
    assert_eq!(seen, committed(frames, make(LoginStep::Probe { key: new })));
    assert_eq!(stored(), Some(vec![id]));
    assert_eq!(names(), ["1000"]);
    // The next step: a fresh worker reads the published record, which the
    // key unlocks.
    assert_eq!(physical(pin(PIN)), (Ok(()), succeeded(&[new], new, 8)));
    // Removing the last key unlinks the record.
    let removed = slots(&[new], &[1]);
    assert_eq!(
        physical(begins(removing(1, &removed, LoginStep::Identify))),
        (Ok(()), removal(&[new], &removed, new))
    );
    assert!(matches!(state(), State::Unenrolled));
    assert!(names().is_empty());
    let served = plugged.remove();
    // Create and prove, repeat, probe; identify, unlock; identify, authorize.
    assert_eq!(served.channels, 7);
    assert_eq!(served.requests, key.transcript().len());
    finish();
}

#[test]
#[ignore = "requires qemu-secret with a disposable guest and UHID"]
fn qemu_login_worker_enrolls_two_keys_across_a_swap_of_devices() {
    guard("login-enroll-two");
    prepare();
    let (primary, backup) = (blank("enroll-primary"), blank("enroll-backup"));
    let port: Port = Arc::new(Mutex::new(Some(Plugged::insert(&primary, NAME))));
    let (changed, polls) = (Swapped::default(), Polls::default());
    let second = login(enrolling(2, 2, LoginStep::Connect));
    let plan = Plan {
        operation: Some(enrolling(2, 1, LoginStep::Connect)),
        pin: Some(pin_frame(PIN)),
        ..swap(&port, &changed, &polls, second, &backup)
    };
    let (result, seen) = watched(plan, &polls);
    assert_eq!(result, Ok(()));
    assert_eq!(swapped(&changed).channels, 3);
    let (first, second) = (newest(&primary), newest(&backup));
    let (one, two) = (fingerprint(&first), fingerprint(&second));
    let mut frames = vec![
        vec![0x18, 0],
        invitation(0x10, &login(enrolling(2, 1, LoginStep::Connect))),
    ];
    frames.extend(ceremony(|step| enrolling(2, 1, step), one));
    frames.push(invitation(
        0x10,
        &login(enrolling(2, 2, LoginStep::Connect)),
    ));
    frames.extend(ceremony(|step| enrolling(2, 2, step), two));
    assert_eq!(
        seen,
        committed(frames, enrolling(2, 2, LoginStep::Probe { key: two }))
    );
    let mut both = vec![first.clone(), second];
    both.sort();
    assert_eq!(stored(), Some(both));
    // The backup's creation excluded the primary's credential.
    let creation = backup
        .transcript()
        .into_iter()
        .find(|(request, _)| request[0] == 1)
        .unwrap()
        .0;
    assert!(creation.windows(first.len()).any(|window| window == first));
    // Each key unlocks the record.
    let order = fingerprints();
    assert_eq!(physical(pin(PIN)), (Ok(()), succeeded(&order, two, 8)));
    let plugged = port.lock().unwrap().take().unwrap();
    assert_eq!(plugged.remove().channels, 3 + 2);
    let plugged = Plugged::insert(&primary, NAME);
    assert_eq!(physical(pin(PIN)), (Ok(()), succeeded(&order, one, 8)));
    plugged.remove();
    finish();
}

#[test]
#[ignore = "requires qemu-secret with a disposable guest and UHID"]
fn qemu_login_worker_adds_a_key_across_a_swap_and_removes_the_authorizing_one() {
    guard("login-add-remove");
    prepare();
    let first = Key::new(30);
    seed(&record(&[&first], None, None));
    let order = fingerprints();
    let new = blank("added");
    let port: Port = Arc::new(Mutex::new(Some(Plugged::insert(&first.device, NAME))));
    let (changed, polls) = (Swapped::default(), Polls::default());
    let connect = login(adding(1, LoginStep::Connect));
    let plan = Plan {
        operation: Some(adding(1, LoginStep::Identify)),
        pin: Some(pin_frame(PIN)),
        ..swap(&port, &changed, &polls, connect, &new)
    };
    let (result, seen) = watched(plan, &polls);
    assert_eq!(result, Ok(()));
    let id = newest(&new);
    let added = fingerprint(&id);
    assert_eq!(seen, addition(&order, first.fingerprint(), added));
    // Identify and authorize on the first key.
    assert_eq!(swapped(&changed).channels, 2);
    let mut both = vec![first.id.clone(), id.clone()];
    both.sort();
    assert_eq!(stored(), Some(both));
    // The new key, still in, unlocks, then removes the first key.
    let order = fingerprints();
    assert_eq!(physical(pin(PIN)), (Ok(()), succeeded(&order, added, 8)));
    let position = order
        .iter()
        .position(|key| *key == first.fingerprint())
        .unwrap() as u8
        + 1;
    let removed = slots(&order, &[position]);
    assert_eq!(
        physical(begins(removing(2, &removed, LoginStep::Identify))),
        (Ok(()), removal(&order, &removed, added))
    );
    assert_eq!(stored(), Some(vec![id]));
    let plugged = port.lock().unwrap().take().unwrap();
    // Create and prove, repeat, probe; two unlock sessions; two removal ones.
    assert_eq!(plugged.remove().channels, 3 + 2 + 2);
    // The removed key no longer unlocks.
    let plugged = Plugged::insert(&first.device, NAME);
    let identified = vec![baseline(&[added]), invitation(0x10, &identify(1))];
    assert_eq!(
        physical(pin(PIN)),
        (
            Err(Failure::NotEnrolled),
            failed(identified, Failure::NotEnrolled)
        )
    );
    plugged.remove();
    finish();
}

#[test]
#[ignore = "requires qemu-secret with a disposable guest and UHID"]
fn qemu_login_worker_waits_through_keepalives_for_a_slow_touch() {
    guard("login-keepalive");
    prepare();
    let key = Key::new(50);
    seed(&record(&[&key], None, None));
    let order = [key.fingerprint()];
    key.device.script(Script {
        presence: Presence::Delayed(TOUCH),
        ..Script::default()
    });
    let plugged = Plugged::insert(&key.device, NAME);
    let started = Instant::now();
    assert_eq!(
        physical(pin(PIN)),
        (Ok(()), succeeded(&order, key.fingerprint(), 8))
    );
    assert!(started.elapsed() >= TOUCH);
    let served = plugged.remove();
    // Identify is silent, so only the unlock assertion waited for the
    // touch, reporting UPNEEDED while it lasted. Keepalives go at most once
    // a period, and none of the touch went longer than the observed silence
    // without one; a loaded guest stretches gaps, so the bounds come from
    // the touch as it ran and that silence, which must stay well under it.
    let touched = key.device.touched();
    assert!(touched >= TOUCH);
    assert!(served.silence < SILENCE, "{served:?}");
    let most = touched.as_micros() / KEEPALIVE_PERIOD.as_micros() + 2;
    let least =
        (touched.as_micros() / served.silence.max(KEEPALIVE_PERIOD).as_micros()).saturating_sub(2);
    assert!(least >= 1);
    let upneeded = served.upneeded as u128;
    assert!(
        (least..=most).contains(&upneeded),
        "{served:?} over {touched:?}"
    );
    finish();
}

#[test]
#[ignore = "requires qemu-secret with a disposable guest and UHID"]
fn qemu_login_worker_refuses_a_key_whose_credprotect_default_fails_the_probe() {
    guard("login-probe");
    prepare();
    let key = Virtual::new(
        Config {
            cred_protect: Some(3),
            ..Config::default()
        },
        Some(PIN),
        "protected",
    );
    let plugged = Plugged::insert(&key, NAME);
    let make = |step| enrolling(1, 1, step);
    let (result, seen) = physical(begins(make(LoginStep::Connect)));
    assert_eq!(result, Err(Failure::Unprobed));
    let new = fingerprint(&newest(&key));
    let mut frames = vec![
        vec![0x18, 0],
        invitation(0x10, &login(make(LoginStep::Connect))),
    ];
    frames.extend(ceremony(make, new));
    assert_eq!(seen, failed(frames, Failure::Unprobed));
    assert!(matches!(state(), State::Unenrolled));
    assert!(names().is_empty());
    // Create and prove, repeat, and the probe that did not select it.
    assert_eq!(plugged.remove().channels, 3);
    finish();
}

/// The power-cut guest's disposable disk, the fixture's keys and ledger on
/// it, and how `@var` is mounted: production's options, with the periodic
/// transaction commit put off past any boot, so that only a sync makes a
/// change durable before a cut.
const VOLUME: &str = "/dev/vda";
const FIXTURE: &str = "/var/lib/td-fixture";
const MOUNT: &str = "nosuid,nodev,subvol=@var,commit=300";
/// The persistent keys: the first enrolls and authorizes, the second is
/// added.
const KEYS: &[&str] = &["a", "b"];
/// The console line, with the phase, on which the host kills the guest.
const CUT: &str = "TD-LOGIN-CUT";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Act {
    /// A one-key enrollment of the first key: a publication over no record.
    Enroll,
    /// The second key added to the first: a publication over a record.
    Add,
    /// Every key removed: the record unlinked.
    Remove,
}

/// One cold boot between setup and the final check: it checks what the
/// boot before it left, then cuts its own write at `stage`, a store stage
/// (`publish_at`, `remove_at`), or after the worker's success when none.
struct Boot {
    phase: &'static str,
    act: Act,
    stage: Option<&'static str>,
}

const BOOTS: &[Boot] = &[
    Boot {
        phase: "enroll-created",
        act: Act::Enroll,
        stage: Some("Created"),
    },
    Boot {
        phase: "enroll-synced",
        act: Act::Enroll,
        stage: Some("FileSynced"),
    },
    Boot {
        phase: "enroll-renamed",
        act: Act::Enroll,
        stage: Some("Renamed"),
    },
    Boot {
        phase: "enroll-committed",
        act: Act::Enroll,
        stage: None,
    },
    Boot {
        phase: "add-written",
        act: Act::Add,
        stage: Some("Written"),
    },
    Boot {
        phase: "add-attempted",
        act: Act::Add,
        stage: Some("RenameAttempted"),
    },
    Boot {
        phase: "add-synced",
        act: Act::Add,
        stage: Some("DirectorySynced"),
    },
    Boot {
        phase: "remove-attempted",
        act: Act::Remove,
        stage: Some("UnlinkAttempted"),
    },
    Boot {
        phase: "remove-unlinked",
        act: Act::Remove,
        stage: Some("Unlinked"),
    },
    Boot {
        phase: "remove-synced",
        act: Act::Remove,
        stage: Some("DirectorySynced"),
    },
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Survivor {
    Old,
    New,
}

/// Whether the write's temporary must be gone, must have survived whole,
/// or may have survived.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Leftover {
    Gone,
    Kept,
    Either,
}

/// What a cut at `stage` leaves. Up to the temporary's sync nothing is
/// durable but the old record; the synced temporary survives, and so does
/// the old record until the directory sync, even after the rename or
/// unlink: that the `Renamed` and `Unlinked` cuts lose those is what shows
/// a missing directory sync would be caught. After it, the new state.
fn leaves(stage: Option<&str>) -> Result<(Survivor, Leftover), String> {
    Ok(match stage {
        Some("Created" | "Written") => (Survivor::Old, Leftover::Either),
        Some("FileSynced" | "RenameAttempted" | "Renamed") => (Survivor::Old, Leftover::Kept),
        Some("UnlinkAttempted" | "Unlinked") => (Survivor::Old, Leftover::Gone),
        Some("DirectorySynced") | None => (Survivor::New, Leftover::Gone),
        Some(other) => return Err(format!("no cut stage {other}")),
    })
}

/// What a boot's write was about to do, saved durably before it began:
/// the record's old and new digests, none for no record, and the
/// publication's temporary.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Ledger {
    phase: String,
    old: Option<[u8; 32]>,
    new: Option<[u8; 32]>,
    temporary: Option<String>,
}

impl Ledger {
    fn text(&self) -> String {
        let digest = |digest: &Option<[u8; 32]>| digest.map_or("none".into(), |bytes| hex(&bytes));
        format!(
            "phase {}\nold {}\nnew {}\ntemporary {}\n",
            self.phase,
            digest(&self.old),
            digest(&self.new),
            self.temporary.as_deref().unwrap_or("none")
        )
    }

    fn parse(text: &str) -> Result<Self, String> {
        let mut lines = text.lines();
        let mut field = |name: &str| {
            lines
                .next()
                .and_then(|line| line.strip_prefix(name)?.strip_prefix(' '))
                .filter(|value| !value.is_empty())
                .map(str::to_string)
                .ok_or_else(|| format!("ledger lacks {name}"))
        };
        let phase = field("phase")?;
        let old = digest(&field("old")?)?;
        let new = digest(&field("new")?)?;
        let temporary = field("temporary")?;
        if lines.next().is_some() {
            return Err("ledger has trailing lines".into());
        }
        Ok(Self {
            phase,
            old,
            new,
            temporary: (temporary != "none").then_some(temporary),
        })
    }

    fn path() -> PathBuf {
        Path::new(FIXTURE).join("ledger")
    }

    /// Whole and durable before the write it describes starts.
    fn save(&self) {
        let next = Path::new(FIXTURE).join("ledger.next");
        let mut file = File::create(&next).unwrap();
        file.write_all(self.text().as_bytes()).unwrap();
        file.sync_all().unwrap();
        fs::rename(&next, Self::path()).unwrap();
        File::open(FIXTURE).unwrap().sync_all().unwrap();
    }

    fn load() -> Result<Self, String> {
        Self::parse(
            &fs::read_to_string(Self::path())
                .map_err(|error| format!("read the ledger: {error}"))?,
        )
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn digest(text: &str) -> Result<Option<[u8; 32]>, String> {
    if text == "none" {
        return Ok(None);
    }
    let bytes = text
        .as_bytes()
        .chunks(2)
        .map(|pair| {
            std::str::from_utf8(pair)
                .ok()
                .filter(|pair| pair.len() == 2)
                .and_then(|pair| u8::from_str_radix(pair, 16).ok())
        })
        .collect::<Option<Vec<u8>>>()
        .ok_or_else(|| format!("ledger digest {text} is not hex"))?;
    bytes
        .try_into()
        .map(Some)
        .map_err(|_| format!("ledger digest {text} is not 32 bytes"))
}

/// The host kills the guest on this line; nothing after the write that
/// reached it syncs, unmounts or returns.
fn cut(phase: &str) -> ! {
    let mut console = OpenOptions::new().write(true).open("/dev/console").unwrap();
    writeln!(console, "{CUT} {phase}").unwrap();
    console.flush().unwrap();
    loop {
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// The worker's store write, with the ledger saved first and the guest cut
/// at the boot's stage.
fn cutting(
    boot: &'static Boot,
    keys: Vec<Virtual>,
    gated: Option<Swapped>,
) -> impl Fn(&Store, Baseline, Change<'_>) -> Outcome {
    move |store: &Store, baseline: Baseline, change: Change<'_>| {
        // An addition's swap was gated, and the authorizing key served its
        // identify and authorize sessions.
        if let Some(changed) = &gated {
            assert_eq!(swapped(changed).channels, 2, "{}", boot.phase);
        }
        let old = match baseline {
            Baseline::Absent => None,
            Baseline::Digest(digest) => Some(digest),
        };
        let (new, temporary) = match &change {
            Change::Publish(record, suffix) => (
                Some(record.digest()),
                Some(format!("{}{}", login_store::TEMPORARY, hex(suffix))),
            ),
            Change::Remove => (None, None),
        };
        Ledger {
            phase: boot.phase.into(),
            old,
            new,
            temporary,
        }
        .save();
        // The ledger was the fixture's last sync: from here to the cut no
        // key may write `@var` either.
        for key in &keys {
            key.freeze(frozen);
        }
        let mut hook = |at: &str| {
            if Some(at) == boot.stage {
                cut(boot.phase);
            }
            Ok(())
        };
        match change {
            Change::Publish(record, suffix) => {
                store.publish_at(baseline, record, &mut &suffix[..], &mut hook)
            }
            Change::Remove => store.remove_at(baseline, &mut hook),
        }
    }
}

/// A key save refused once the write began: never expected, so loud.
fn frozen(error: &str) {
    if let Ok(mut console) = OpenOptions::new().write(true).open("/dev/console") {
        let _ = writeln!(console, "TD-SECRET-VM-FAIL: {error}");
    }
}

fn applet(args: &[&str]) {
    assert!(
        Command::new("/bin/td-init")
            .args(args)
            .status()
            .unwrap()
            .success(),
        "{args:?}"
    );
}

/// `@var` on the disposable disk, made on a fresh one at setup.
fn mount(create: bool) {
    assert!(fs::metadata(VOLUME).unwrap().file_type().is_block_device());
    fs::create_dir_all("/var").unwrap();
    if create {
        let mut prefix = [0; 4096];
        File::open(VOLUME).unwrap().read_exact(&mut prefix).unwrap();
        assert_eq!(prefix, [0; 4096], "fixture disk is not fresh");
        assert!(Command::new("/bin/mkfs.btrfs")
            .args(["-q", VOLUME])
            .status()
            .unwrap()
            .success());
        fs::create_dir("/volume").unwrap();
        applet(&[
            "mount",
            "-t",
            "btrfs",
            "-o",
            "nosuid,nodev",
            VOLUME,
            "/volume",
        ]);
        assert!(Command::new("/bin/btrfs")
            .args(["subvolume", "create", "/volume/@var"])
            .status()
            .unwrap()
            .success());
        applet(&["umount", "/volume"]);
    }
    applet(&["mount", "-t", "btrfs", "-o", MOUNT, VOLUME, "/var"]);
    assert!(fs::read_to_string("/proc/self/mountinfo")
        .unwrap()
        .lines()
        .any(|line| {
            let fields: Vec<_> = line.split_whitespace().collect();
            fields.get(3) == Some(&"/@var")
                && fields.get(4) == Some(&"/var")
                && line.contains(" - btrfs ")
                && line.contains("commit=300")
        }));
}

fn key_file(name: &str) -> PathBuf {
    Path::new(FIXTURE).join("keys").join(name)
}

/// The keys as a cold boot finds them; a damaged state file fails the
/// guest by name.
fn restored() -> Result<Vec<Virtual>, String> {
    KEYS.iter()
        .map(|name| {
            Virtual::restore(Config::default(), &key_file(name))
                .map_err(|damage| format!("virtual key {name}: {damage}"))
        })
        .collect()
}

/// The fresh disk: `@var`, the login directory as firstboot makes it, and
/// blank persistent keys; unmounted, so all of it is on the disk.
fn setup() -> Result<(), String> {
    mount(true);
    fs::create_dir_all(directory().parent().unwrap()).unwrap();
    fs::DirBuilder::new()
        .mode(0o700)
        .create(directory())
        .unwrap();
    fs::set_permissions(directory(), Permissions::from_mode(0o700)).unwrap();
    fs::DirBuilder::new()
        .mode(0o700)
        .recursive(true)
        .create(Path::new(FIXTURE).join("keys"))
        .unwrap();
    for name in KEYS {
        Virtual::persistent(
            Config::default(),
            Some(PIN),
            &format!("powercut-{name}"),
            &key_file(name),
        )?;
    }
    Ledger {
        phase: "setup".into(),
        old: None,
        new: None,
        temporary: None,
    }
    .save();
    assert!(matches!(state(), State::Unenrolled));
    assert!(names().is_empty());
    applet(&["umount", "/var"]);
    Ok(())
}

/// The record a cut left is the one its stage leaves, whole, and its
/// temporary as that stage leaves it.
fn survived(ledger: &Ledger, stage: Option<&str>) -> Result<(), String> {
    let (survivor, leftover) = leaves(stage)?;
    let phase = &ledger.phase;
    let wanted = match survivor {
        Survivor::Old => ledger.old,
        Survivor::New => ledger.new,
    };
    match (state(), wanted) {
        (State::Unenrolled, None) => {}
        (State::Enrolled(record), Some(digest)) if record.digest() == digest => {}
        (State::Unavailable(cause), _) => {
            return Err(format!(
                "{phase}: the login state is unavailable: {cause:?}"
            ))
        }
        _ => return Err(format!("{phase}: the {survivor:?} record did not survive")),
    }
    let (temporaries, others): (Vec<String>, Vec<String>) = names()
        .into_iter()
        .partition(|name| name.starts_with(login_store::TEMPORARY));
    let record: &[&str] = if wanted.is_some() { &["1000"] } else { &[] };
    assert_eq!(others, record, "{phase}");
    let written: Vec<String> = ledger.temporary.iter().cloned().collect();
    match leftover {
        Leftover::Gone => assert!(temporaries.is_empty(), "{phase}: {temporaries:?}"),
        Leftover::Either => assert!(
            temporaries.iter().all(|name| written.contains(name)),
            "{phase}: {temporaries:?}"
        ),
        Leftover::Kept => {
            assert_eq!(temporaries, written, "{phase}");
            // Synced before the cut, it holds the whole new record.
            let bytes = fs::read(directory().join(&temporaries[0])).unwrap();
            assert_eq!(Some(crypto::digest(&bytes)), ledger.new, "{phase}");
        }
    }
    Ok(())
}

/// Each restored key whose credential the record holds unlocks it; one that
/// holds only other credentials is NOT ENROLLED.
fn unlocks(keys: &[Virtual]) {
    let State::Enrolled(record) = state() else {
        return;
    };
    let order = fingerprints();
    let count = order.len() as u8;
    for key in keys {
        let held = key.state().credentials;
        if held.is_empty() {
            continue;
        }
        let enrolled = held
            .iter()
            .find(|credential| record.slot(&credential.id).is_some());
        let expected = match enrolled {
            Some(credential) => (Ok(()), succeeded(&order, fingerprint(&credential.id), 8)),
            None => (
                Err(Failure::NotEnrolled),
                failed(
                    vec![baseline(&order), invitation(0x10, &identify(count))],
                    Failure::NotEnrolled,
                ),
            ),
        };
        let plugged = Plugged::insert(key, NAME);
        assert_eq!(physical(pin(PIN)), expected);
        plugged.remove();
        assert_eq!(key.saved(), Ok(()));
    }
}

/// The boot's own write, through the worker over the restored keys, cut at
/// its stage or after its success; it returns only if neither happens.
fn act(boot: &'static Boot, keys: &[Virtual]) -> Result<(), String> {
    let [first, second] = keys else {
        return Err("two keys".into());
    };
    let changed = Swapped::default();
    let gated = (boot.act == Act::Add).then(|| Arc::clone(&changed));
    let write = cutting(boot, keys.to_vec(), gated);
    let context = writing(&write);
    let count = fingerprints().len() as u8;
    let (result, _) = match boot.act {
        Act::Enroll => {
            let _plugged = Plugged::insert(first, NAME);
            let plan = begins(enrolling(1, 1, LoginStep::Connect));
            operated(plan, &Polls::default(), &context)
        }
        Act::Add => {
            let port: Port = Arc::new(Mutex::new(Some(Plugged::insert(first, NAME))));
            let polls = Polls::default();
            let connect = login(adding(count, LoginStep::Connect));
            let plan = Plan {
                operation: Some(adding(count, LoginStep::Identify)),
                pin: Some(pin_frame(PIN)),
                ..swap(&port, &changed, &polls, connect, second)
            };
            operated(plan, &polls, &context)
        }
        Act::Remove => {
            let _plugged = Plugged::insert(first, NAME);
            let order = fingerprints();
            let every: Vec<u8> = (1..=count).collect();
            let plan = begins(removing(count, &slots(&order, &every), LoginStep::Identify));
            operated(plan, &Polls::default(), &context)
        }
    };
    if result != Ok(()) {
        return Err(format!("{}: the write failed: {result:?}", boot.phase));
    }
    if let Some(stage) = boot.stage {
        return Err(format!("{}: the write never reached {stage}", boot.phase));
    }
    cut(boot.phase)
}

fn phase() -> Result<String, String> {
    let cmdline = fs::read_to_string("/proc/cmdline").unwrap();
    let mut phases = cmdline
        .split_ascii_whitespace()
        .filter_map(|token| token.strip_prefix("td.login-cut="));
    let phase = phases.next().ok_or("no td.login-cut phase")?;
    if phases.next().is_some() {
        return Err("several td.login-cut phases".into());
    }
    Ok(phase.into())
}

#[test]
fn every_cut_names_a_stage_its_write_reaches_and_each_boot_a_phase_of_its_own() {
    let fixture = Fixture::new();
    let store = Store::open(&fixture.dir, fixture.owner, UID).unwrap();
    let key = Key::new(70);
    let record = Record::decode(&record(&[&key], None, None), UID).unwrap();
    let mut published = Vec::new();
    let outcome = store.publish_at(Baseline::Absent, &record, &mut &[9; 16][..], &mut |at| {
        published.push(at.to_string());
        Ok(())
    });
    assert_eq!(outcome, Outcome::Committed);
    let mut removed = Vec::new();
    let outcome = store.remove_at(Baseline::Digest(record.digest()), &mut |at| {
        removed.push(at.to_string());
        Ok(())
    });
    assert_eq!(outcome, Outcome::Committed);
    let mut phases = vec!["setup", "final"];
    for boot in BOOTS {
        let reached = match boot.act {
            Act::Enroll | Act::Add => &published,
            Act::Remove => &removed,
        };
        if let Some(stage) = boot.stage {
            assert!(reached.iter().any(|at| at == stage), "{}", boot.phase);
        }
        assert!(leaves(boot.stage).is_ok());
        phases.push(boot.phase);
    }
    assert!(leaves(Some("Unknown")).is_err());
    let count = phases.len();
    phases.sort_unstable();
    phases.dedup();
    assert_eq!(phases.len(), count);
    // Every stage of both writes is cut once, and a publication after it
    // commits.
    let mut cut: Vec<&str> = BOOTS
        .iter()
        .filter(|boot| boot.act != Act::Remove)
        .filter_map(|boot| boot.stage)
        .collect();
    cut.sort_unstable();
    published.sort_unstable();
    assert_eq!(cut, published);
    let cut: Vec<&str> = BOOTS
        .iter()
        .filter(|boot| boot.act == Act::Remove)
        .filter_map(|boot| boot.stage)
        .collect();
    assert_eq!(cut, removed);
    assert!(BOOTS.iter().any(|boot| boot.stage.is_none()));
}

#[test]
fn the_ledger_round_trips_and_refuses_what_it_did_not_write() {
    let ledger = Ledger {
        phase: "add-written".into(),
        old: Some([0xab; 32]),
        new: Some([0x01; 32]),
        temporary: Some(format!("{}{}", login_store::TEMPORARY, "5a".repeat(16))),
    };
    assert_eq!(Ledger::parse(&ledger.text()), Ok(ledger.clone()));
    let empty = Ledger {
        phase: "setup".into(),
        old: None,
        new: None,
        temporary: None,
    };
    assert_eq!(
        empty.text(),
        "phase setup\nold none\nnew none\ntemporary none\n"
    );
    assert_eq!(Ledger::parse(&empty.text()), Ok(empty));
    let text = ledger.text();
    for broken in [
        String::new(),
        text.replace("old ", "olde "),
        text.replace(&"ab".repeat(32), &"ab".repeat(31)),
        text.replace(&"ab".repeat(32), &"zz".repeat(32)),
        text.replace("phase add-written", "phase "),
        format!("{text}extra\n"),
        text.lines().take(3).collect::<Vec<_>>().join("\n"),
    ] {
        assert!(Ledger::parse(&broken).is_err(), "{broken:?}");
    }
}

#[test]
#[ignore = "requires qemu-secret with a disposable guest, disk and UHID"]
fn qemu_login_record_survives_power_cuts_inside_its_writes() -> Result<(), String> {
    guard("login-powercut");
    let phase = phase()?;
    if phase == "setup" {
        return setup();
    }
    let at = BOOTS.iter().position(|boot| boot.phase == phase);
    let before = match at {
        Some(0) => None,
        Some(index) => BOOTS.get(index - 1),
        None if phase == "final" => BOOTS.last(),
        None => return Err(format!("no power-cut phase {phase}")),
    };
    assert!(Device::discover().unwrap().is_empty());
    mount(false);
    let keys = restored()?;
    let ledger = Ledger::load()?;
    let (previous, stage) = before.map_or(("setup", None), |boot| (boot.phase, boot.stage));
    if ledger.phase != previous {
        return Err(format!("{phase} follows {previous}, not {}", ledger.phase));
    }
    survived(&ledger, stage)?;
    unlocks(&keys);
    if let Some(index) = at {
        return act(&BOOTS[index], &keys);
    }
    // Every credential each key made, in boots cut before and after their
    // writes, stayed on it.
    let made = |act| BOOTS.iter().filter(|boot| boot.act == act).count();
    let [first, second] = &keys[..] else {
        return Err("two keys".into());
    };
    assert_eq!(first.state().credentials.len(), made(Act::Enroll));
    assert_eq!(second.state().credentials.len(), made(Act::Add));
    assert_eq!(first.state().retries, 8);
    applet(&["umount", "/var"]);
    Ok(())
}
