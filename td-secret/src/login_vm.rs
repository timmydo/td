// The qemu-secret login guests (td-secret/DESIGN.md, "Login-key worker
// guests"), included into login_operation's tests for their root and keys.
// Each runs the worker's own `operate` over `Physical`: Device::discover,
// Session::open and each session's production HID worker with its
// operation lock, against fido_virtual keys that fido_uhid presents as
// hidraw devices. Root is the tests' own, over a socketpair Wire.

use super::*;
use crate::fido_device::Device;
use crate::fido_uhid::{guard, Plugged, Served, KEEPALIVE_PERIOD};
use crate::fido_virtual::Presence;
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
    Context {
        uid: UID,
        path: directory(),
        owner: Owner::ROOT,
        started: Instant::now(),
        lifetime: fido_device::MAX_LIFETIME,
        margin: COMMIT_MARGIN,
        current: READS,
        previous: READS,
        write: &write,
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
    let context = context();
    let (worker, parent) = UnixStream::pair().unwrap();
    let authority = std::thread::spawn(move || root(parent, plan));
    let deadline = context.started.checked_add(TWO_CEREMONIES).unwrap();
    let mut wire = Wire::new(worker, deadline).unwrap();
    let mut random = File::open("/dev/urandom").unwrap();
    let result = operate(
        &mut wire,
        &context,
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
