// The qemu-secret login guests (td-secret/DESIGN.md, "Login-key worker
// guests"), included into login_operation's tests for their root and keys.
// Each runs the worker's own `operate` over `Physical`: Device::discover,
// Session::open and each session's production HID worker with its
// operation lock, against fido_virtual keys that fido_uhid presents as
// hidraw devices. Root is the tests' own, over a socketpair Wire. The
// power-cut guest ("Login power-cut guests") keeps its record and keys on a
// disposable disk across cold boots, and login-desktop pairs the
// production compositor and td-authd over a record the worker enrolled.

use super::*;
use crate::fido_device::vm_tests::desktop::{self, Diagnostics, Keyboard, Pair, Process};
use crate::fido_device::Device;
use crate::fido_uhid::{guard, Plugged, Served, KEEPALIVE_PERIOD};
use std::cell::Cell;
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
        retained: &(READS, READS),
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
    let deadline = context.started.checked_add(LOGIN_TWO_CEREMONIES).unwrap();
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

#[test]
#[ignore = "requires qemu-secret with a disposable guest and UHID"]
fn qemu_login_worker_ends_on_denied_presence_always_uv_and_a_list_too_small() {
    guard("login-refusals");
    prepare();
    let denied = Script {
        presence: Presence::Denied,
        ..Script::default()
    };
    let make = |step| enrolling(1, 1, step);
    let connected = vec![
        vec![0x18, 0],
        invitation(0x10, &login(make(LoginStep::Connect))),
    ];
    // alwaysUv true: KEY REFUSED 02 at the new key's getInfo, before any PIN.
    let strict = Virtual::new(
        Config {
            always_uv: Some(true),
            ..Config::default()
        },
        Some(PIN),
        "always-uv",
    );
    let plugged = Plugged::insert(&strict, NAME);
    let refused = Failure::Refused(LoginRefusal::AlwaysUv);
    assert_eq!(
        physical(begins(make(LoginStep::Connect))),
        (Err(refused), failed(connected.clone(), refused))
    );
    assert_eq!(refused.frame(), [0x15, 6, 2]);
    assert_eq!(pin_tokens(&strict), 0);
    assert!(sent(&strict, 1, 1).is_empty());
    assert_eq!(plugged.remove().channels, 1);
    assert!(names().is_empty());
    // Presence denied at the creation, after its PIN: DENIED, no credential,
    // and the key's count still 8.
    let new = blank("denied-create");
    new.script(denied);
    let plugged = Plugged::insert(&new, NAME);
    let mut frames = connected.clone();
    frames.push(invitation(
        0x10,
        &login(make(LoginStep::Create { retries: 8 })),
    ));
    assert_eq!(
        physical(begins(make(LoginStep::Connect))),
        (Err(Failure::Denied), failed(frames, Failure::Denied))
    );
    assert_eq!(Failure::Denied.frame(), [0x15, 7]);
    assert!(new.state().credentials.is_empty());
    assert_eq!(new.state().retries, 8);
    assert_eq!(plugged.remove().channels, 1);
    assert!(names().is_empty());
    // alwaysUv false is admitted: the key enrolls and its record unlocks.
    let relaxed = Virtual::new(
        Config {
            always_uv: Some(false),
            ..Config::default()
        },
        Some(PIN),
        "never-uv",
    );
    let plugged = Plugged::insert(&relaxed, NAME);
    let (result, seen) = physical(begins(make(LoginStep::Connect)));
    assert_eq!(result, Ok(()));
    let key = fingerprint(&newest(&relaxed));
    let mut frames = connected;
    frames.extend(ceremony(make, key));
    assert_eq!(seen, committed(frames, make(LoginStep::Probe { key })));
    assert_eq!(physical(pin(PIN)), (Ok(()), succeeded(&[key], key, 8)));
    // Presence denied at its unlock assertion, after the PIN: DENIED;
    // granted again, the key unlocks, its count reported 8 again.
    relaxed.script(denied);
    assert_eq!(
        physical(pin(PIN)),
        (
            Err(Failure::Denied),
            failed(through_unlock(&[key], key, 8), Failure::Denied)
        )
    );
    assert_eq!(relaxed.state().retries, 8);
    relaxed.script(Script::default());
    assert_eq!(physical(pin(PIN)), (Ok(()), succeeded(&[key], key, 8)));
    // The enrolled key reconfigured to require UV everywhere, as a real
    // key's tool can: KEY REFUSED 02 at the unlock's identify, no PIN step.
    relaxed.configure(|config| config.always_uv = Some(true));
    let tokens = pin_tokens(&relaxed);
    let refused = Failure::Refused(LoginRefusal::AlwaysUv);
    assert_eq!(
        physical(pin(PIN)),
        (
            Err(refused),
            failed(
                vec![baseline(&[key]), invitation(0x10, &identify(1))],
                refused
            )
        )
    );
    assert_eq!(pin_tokens(&relaxed), tokens);
    // Create and prove, repeat, probe; three unlocks of two sessions each;
    // the refused identify.
    assert_eq!(plugged.remove().channels, 3 + 3 * 2 + 1);
    // A new key whose list cannot hold the record's two exclusions: KEY
    // REFUSED 05 after the gated swap, before any PIN, the record untouched.
    let (one, two) = (Key::new(80), Key::new(81));
    let bytes = record(&[&one, &two], None, None);
    seed(&bytes);
    let order = fingerprints();
    let small = Virtual::new(
        Config {
            max_list: Some(1),
            ..Config::default()
        },
        Some(PIN),
        "small",
    );
    let port: Port = Arc::new(Mutex::new(Some(Plugged::insert(&one.device, NAME))));
    let (changed, polls) = (Swapped::default(), Polls::default());
    let connect = login(adding(2, LoginStep::Connect));
    let plan = Plan {
        operation: Some(adding(2, LoginStep::Identify)),
        pin: Some(pin_frame(PIN)),
        ..swap(&port, &changed, &polls, connect, &small)
    };
    let refused = Failure::Refused(LoginRefusal::ListTooSmall);
    assert_eq!(
        watched(plan, &polls),
        (
            Err(refused),
            failed(authorized_addition(&order, one.fingerprint()), refused)
        )
    );
    assert_eq!(refused.frame(), [0x15, 6, 5]);
    // Identify and authorize on the enrolled key; one session on the new.
    assert_eq!(swapped(&changed).channels, 2);
    let plugged = port.lock().unwrap().take().unwrap();
    assert_eq!(plugged.remove().channels, 1);
    assert_eq!(pin_tokens(&small), 0);
    assert!(small.state().credentials.is_empty());
    assert_eq!(fs::read(directory().join("1000")).unwrap(), bytes);
    finish();
}

#[test]
#[ignore = "requires qemu-secret with a disposable guest and UHID"]
fn qemu_login_worker_never_unlocks_a_tampered_record_or_a_stale_signature() {
    guard("login-verify");
    prepare();
    let (key, other) = (Key::new(82), Key::new(83));
    let order = [key.fingerprint()];
    let refused = failed(
        through_unlock(&order, key.fingerprint(), 8),
        Failure::Failed,
    );
    let mut flipped = key.output;
    flipped[0] ^= 1;
    let plugged = Plugged::insert(&key.device, NAME);
    // The production record rewritten between operations with a verifier,
    // then a public key, that are not the key's: its assertion verifies or
    // its output matches, but never both, and it is FAILED after the PIN.
    for bytes in [
        record(&[&key], Some(flipped), None),
        record(&[&key], None, Some(&other)),
    ] {
        seed(&bytes);
        assert_eq!(physical(pin(PIN)), (Err(Failure::Failed), refused.clone()));
        assert_eq!(fs::read(directory().join("1000")).unwrap(), bytes);
    }
    seed(&record(&[&key], None, None));
    assert_eq!(
        physical(pin(PIN)),
        (Ok(()), succeeded(&order, key.fingerprint(), 8))
    );
    // A signature over other data, here this operation's identify's.
    key.device.script(Script {
        signing: Signing::Stale,
        ..Script::default()
    });
    assert_eq!(physical(pin(PIN)), (Err(Failure::Failed), refused.clone()));
    // A replay: this assertion's data signed over the client-data hash the
    // key signed in an earlier unlock, which the worker's fresh challenge
    // no longer matches.
    key.device.script(Script {
        signing: Signing::Replayed,
        ..Script::default()
    });
    assert_eq!(physical(pin(PIN)), (Err(Failure::Failed), refused));
    // Every PIN was right; each refusal is the record's or the signature's.
    assert_eq!(key.device.state().retries, 8);
    // Five operations of an identify and an assertion session each.
    assert_eq!(plugged.remove().channels, 5 * 2);
    finish();
}

#[test]
#[ignore = "requires qemu-secret with a disposable guest and UHID"]
fn qemu_login_worker_refuses_a_changed_record_or_an_unshared_version_without_writing() {
    guard("login-changed");
    prepare();
    let (one, two, other) = (Key::new(84), Key::new(85), Key::new(86));
    let bytes = record(&[&one, &two], None, None);
    let changed = record(&[&one, &two, &other], None, None);
    let dir = directory().to_path_buf();
    let held = || fs::read(directory().join("1000")).ok();
    // The worker's store write, counted: no change below may attempt one.
    let writes = Cell::new(0);
    let counted = |store: &Store, baseline: Baseline, change: Change<'_>| {
        writes.set(writes.get() + 1);
        write(store, baseline, change)
    };
    let counting = |plan| operated(plan, &Polls::default(), &writing(&counted));
    seed(&bytes);
    let order = fingerprints();
    // Changed between the baseline frame and the description: RECORD
    // CHANGED at the first step, before any token I/O.
    let plugged = Plugged::insert(&one.device, NAME);
    let plan = Plan {
        change: Some((dir.clone(), Some(changed.clone()), At::Baseline)),
        ..pin(PIN)
    };
    assert_eq!(
        counting(plan),
        (
            Err(Failure::Changed),
            failed(
                vec![baseline(&order), invitation(0x10, &identify(2))],
                Failure::Changed
            )
        )
    );
    assert_eq!(Failure::Changed.frame(), [0x15, 0x0d]);
    assert_eq!(plugged.remove(), Served::default());
    assert_eq!(held(), Some(changed.clone()));
    // An addition whose record is replaced at its commit round: the whole
    // ceremony, then RECORD CHANGED with no write attempted.
    seed(&bytes);
    let new = blank("changed-late");
    let port: Port = Arc::new(Mutex::new(Some(Plugged::insert(&one.device, NAME))));
    let (swapping, polls) = (Swapped::default(), Polls::default());
    let connect = login(adding(2, LoginStep::Connect));
    let plan = Plan {
        operation: Some(adding(2, LoginStep::Identify)),
        pin: Some(pin_frame(PIN)),
        change: Some((dir.clone(), Some(changed.clone()), At::Commit)),
        ..swap(&port, &swapping, &polls, connect, &new)
    };
    let (result, seen) = operated(plan, &polls, &writing(&counted));
    assert_eq!(result, Err(Failure::Changed));
    let mut frames = addition(&order, one.fingerprint(), fingerprint(&newest(&new)));
    frames.pop();
    assert_eq!(seen, failed(frames, Failure::Changed));
    assert_eq!(swapped(&swapping).channels, 2);
    let plugged = port.lock().unwrap().take().unwrap();
    assert_eq!(plugged.remove().channels, 3);
    assert_eq!(held(), Some(changed.clone()));
    // A removal whose record is removed at its commit round.
    seed(&bytes);
    let at = order
        .iter()
        .position(|key| *key == two.fingerprint())
        .unwrap() as u8
        + 1;
    let removed = slots(&order, &[at]);
    let plugged = Plugged::insert(&one.device, NAME);
    let plan = Plan {
        change: Some((dir.clone(), None, At::Commit)),
        ..begins(removing(2, &removed, LoginStep::Identify))
    };
    let (result, seen) = counting(plan);
    assert_eq!(result, Err(Failure::Changed));
    let mut frames = removal(&order, &removed, one.fingerprint());
    frames.pop();
    assert_eq!(seen, failed(frames, Failure::Changed));
    assert_eq!(plugged.remove().channels, 2);
    assert!(names().is_empty());
    // A first enrollment over a record that appeared at its commit round.
    let key = blank("changed-enroll");
    let plugged = Plugged::insert(&key, NAME);
    let make = |step| enrolling(1, 1, step);
    let plan = Plan {
        change: Some((dir.clone(), Some(bytes.clone()), At::Commit)),
        ..begins(make(LoginStep::Connect))
    };
    let (result, seen) = counting(plan);
    assert_eq!(result, Err(Failure::Changed));
    let new = fingerprint(&newest(&key));
    let mut frames = vec![
        vec![0x18, 0],
        invitation(0x10, &login(make(LoginStep::Connect))),
    ];
    frames.extend(ceremony(make, new));
    let mut frames = committed(frames, make(LoginStep::Probe { key: new }));
    frames.pop();
    assert_eq!(seen, failed(frames, Failure::Changed));
    assert_eq!(plugged.remove().channels, 3);
    assert_eq!(held(), Some(bytes.clone()));
    assert_eq!(writes.get(), 0);
    // No record version this build and both retained deployments read:
    // VERSION for an addition, a removal that leaves a key and a first
    // enrollment, before any token I/O.
    let one_key = record(&[&one], None, None);
    let partial = removing(2, &removed, LoginStep::Identify);
    let plugged = Plugged::insert(&one.device, NAME);
    // Production's own empty sets first.
    let pairs: &[(&[u8], &[u8])] = &[
        (&[], &[]),
        (&[], &[VERSION]),
        (&[VERSION], &[]),
        (&[2], &[2]),
        (&[VERSION], &[2]),
    ];
    for &(current, previous) in pairs {
        for (seeded, plan, frame) in [
            (
                Some(&one_key),
                begins(adding(1, LoginStep::Identify)),
                baseline(&[one.fingerprint()]),
            ),
            (Some(&bytes), begins(partial.clone()), baseline(&order)),
            (None, begins(make(LoginStep::Connect)), vec![0x18, 0]),
        ] {
            replace(directory(), seeded.map(Vec::as_slice));
            let context = Context {
                retained: &(current, previous),
                ..writing(&counted)
            };
            assert_eq!(
                operated(plan, &Polls::default(), &context),
                (Err(Failure::Version), vec![frame, vec![0x15, 0x12]])
            );
            assert_eq!(writes.get(), 0);
            assert_eq!(fs::read(directory().join("1000")).ok().as_ref(), seeded);
        }
    }
    // Removing every key writes no version, so it unlinks the record even
    // with none read: the one write of this guest.
    replace(directory(), Some(&bytes));
    let context = Context {
        retained: &(&[][..], &[][..]),
        ..writing(&counted)
    };
    let every = slots(&order, &[1, 2]);
    assert_eq!(
        operated(
            begins(removing(2, &every, LoginStep::Identify)),
            &Polls::default(),
            &context
        ),
        (Ok(()), removal(&order, &every, one.fingerprint()))
    );
    assert_eq!(writes.get(), 1);
    assert!(matches!(state(), State::Unenrolled));
    assert!(names().is_empty());
    // Only the last removal opened sessions.
    assert_eq!(plugged.remove().channels, 2);
    finish();
}

#[test]
#[ignore = "requires qemu-secret with a disposable guest and UHID"]
fn qemu_login_worker_adds_keys_to_eight_and_refuses_a_ninth_before_any_token() {
    guard("login-eight");
    prepare();
    let first = Key::new(87);
    seed(&record(&[&first], None, None));
    // Each addition is authorized by the key added before it, still in,
    // which a gated swap then replaces with the next.
    let port: Port = Arc::new(Mutex::new(Some(Plugged::insert(&first.device, NAME))));
    let mut by = first.fingerprint();
    let mut keys = vec![(first.device.clone(), first.id.clone())];
    for count in 1..8u8 {
        let order = fingerprints();
        let new = blank(&format!("added-{count}"));
        let (changed, polls) = (Swapped::default(), Polls::default());
        let connect = login(adding(count, LoginStep::Connect));
        let plan = Plan {
            operation: Some(adding(count, LoginStep::Identify)),
            pin: Some(pin_frame(PIN)),
            ..swap(&port, &changed, &polls, connect, &new)
        };
        let (result, seen) = watched(plan, &polls);
        assert_eq!(result, Ok(()), "{count}");
        let id = newest(&new);
        assert_eq!(seen, addition(&order, by, fingerprint(&id)), "{count}");
        // The authorizer's identify and authorize, after its own create and
        // prove, repeat and probe when it was added.
        let authorized = if count == 1 { 2 } else { 3 + 2 };
        assert_eq!(swapped(&changed).channels, authorized, "{count}");
        by = fingerprint(&id);
        keys.push((new, id));
        let mut expected: Vec<Vec<u8>> = keys.iter().map(|(_, id)| id.clone()).collect();
        expected.sort();
        assert_eq!(stored(), Some(expected), "{count}");
    }
    let last = port.lock().unwrap().take().unwrap();
    assert_eq!(last.remove().channels, 3);
    // Every key unlocks the eight-key record.
    let order = fingerprints();
    assert_eq!(order.len(), 8);
    for (device, id) in &keys {
        let plugged = Plugged::insert(device, NAME);
        assert_eq!(
            physical(pin(PIN)),
            (Ok(()), succeeded(&order, fingerprint(id), 8))
        );
        assert_eq!(plugged.remove().channels, 2);
    }
    // At eight no addition can be described: root sends none, or one that
    // does not match the baseline, and the worker touches no key.
    assert!(Request::new(NONCE, UID, adding(8, LoginStep::Identify)).is_err());
    let ninth = blank("ninth");
    let plugged = Plugged::insert(&ninth, NAME);
    for plan in [
        Plan {
            description: Some(login(adding(7, LoginStep::Identify)).encode()),
            ..pin(PIN)
        },
        Plan {
            description: Some(Vec::new()),
            ..pin(PIN)
        },
    ] {
        assert_eq!(
            physical(plan),
            (
                Err(Failure::Internal),
                vec![baseline(&order), Failure::Internal.frame()]
            )
        );
    }
    assert_eq!(plugged.remove(), Served::default());
    assert!(ninth.transcript().is_empty());
    assert_eq!(fingerprints(), order);
    finish();
}

// login-desktop (td-secret/DESIGN.md, "Login desktop guest"): the
// production compositor and td-authd paired over a record this worker
// enrolled, root simulated, through a UHID key and a UHID keyboard. The
// host checks the display through QMP at each step the guest names
// (recipes/src/fixtures/secret_vm.rs, LOGIN_DESKTOP_SCREENS), and the
// guest acts only once the host has seen it.

/// The host's question on the console and its answer on ttyS0.
const SCREEN: &str = "TD-LOGIN-SCREEN";
const SHOWN: &str = "TD-LOGIN-SHOWN";
/// The name the lock surface shows above the account's.
const HOSTNAME: &str = "td-login-desktop";
const FRAMEBUFFER: &str = "/dev/fb0";
/// QEMU's fixed output: 1280x800 at 32 bits.
const STRIDE: usize = 1280 * 4;
/// Magenta, which no compositor paints: the screen a generation replaces.
const BLANK: [u8; 4] = [0xff, 0x00, 0xff, 0];
/// Long enough for the host to see the touch request.
const SLOW_TOUCH: Duration = Duration::from_secs(5);
/// A client behind the lock: the compositor's own demo.
const DEMO: &str = "/bin/td-ui-demo";
const DEMO_READY: &str = "/run/user/1000/demo-ready";
const ENTER: u8 = 0x28;
const ESCAPE: u8 = 0x29;
const CAPS_LOCK: u8 = 0x39;

/// The host, which checks the display when the guest asks.
struct Host {
    console: File,
    tty: File,
    read: Vec<u8>,
}

impl Host {
    fn open() -> Self {
        // Not a controlling terminal, and never blocking past the deadline.
        Self {
            console: OpenOptions::new().write(true).open("/dev/console").unwrap(),
            tty: OpenOptions::new()
                .read(true)
                .custom_flags(0o400 | 0o4000)
                .open("/dev/ttyS0")
                .unwrap(),
            read: Vec::new(),
        }
    }

    /// Asks the host to see screen `name` and waits for its answer.
    fn screen(&mut self, name: &str, arguments: &[&str]) {
        let mut request = format!("{SCREEN} {name}");
        for argument in arguments {
            request.push(' ');
            request.push_str(argument);
        }
        // One write: the host reads only finished lines.
        request.push('\n');
        self.console.write_all(request.as_bytes()).unwrap();
        self.console.flush().unwrap();
        let request = request.trim_end();
        let answer = format!("{SHOWN} {name}");
        let deadline = Instant::now() + Duration::from_secs(90);
        let mut chunk = [0; 256];
        loop {
            match self.tty.read(&mut chunk) {
                Ok(count) => self.read.extend_from_slice(&chunk[..count]),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("read ttyS0: {error}"),
            }
            while let Some(end) = self.read.iter().position(|byte| *byte == b'\n') {
                let line: Vec<u8> = self.read.drain(..=end).collect();
                let line = String::from_utf8_lossy(&line);
                let line = line.trim();
                if line == answer {
                    return;
                }
                assert!(!line.starts_with(SHOWN), "{line:?} answered {request:?}");
            }
            assert!(Instant::now() < deadline, "no answer to {request:?}");
        }
    }
}

/// The usages that type `pin`'s digits.
fn digits(pin: &[u8]) -> Vec<u8> {
    pin.iter()
        .map(|digit| match *digit {
            b'1'..=b'9' => 0x1e + digit - b'1',
            b'0' => 0x27,
            _ => panic!("not a digit"),
        })
        .collect()
}

fn type_pin(keyboard: &mut Keyboard, pin: &[u8]) {
    for usage in digits(pin) {
        keyboard.key(usage);
    }
}

/// Ctrl+Alt+Esc, after a fresh report that drains the quarantine a
/// closing attention screen leaves.
fn chord(keyboard: &mut Keyboard) {
    keyboard.key(CAPS_LOCK);
    keyboard.report(5, 0);
    keyboard.report(5, ESCAPE);
    keyboard.report(0, 0);
}

/// Nothing but the compositor draws on the framebuffer: the kernel's
/// console lets go of it.
fn release_console() {
    for entry in fs::read_dir("/sys/class/vtconsole").unwrap() {
        let path = entry.unwrap().path();
        if fs::read_to_string(path.join("name"))
            .unwrap()
            .contains("frame buffer")
        {
            fs::write(path.join("bind"), "0").unwrap();
        }
    }
    for (attribute, value) in [
        ("virtual_size", "1280,800"),
        ("bits_per_pixel", "32"),
        ("stride", "5120"),
    ] {
        let read = fs::read_to_string(format!("/sys/class/graphics/fb0/{attribute}")).unwrap();
        assert_eq!(read.trim(), value, "{attribute}");
    }
}

/// The screen a generation starts over: the last one's frame stays in the
/// framebuffer until the next paints.
fn blank_screen() {
    fs::write(FRAMEBUFFER, BLANK.repeat(STRIDE / 4 * 800)).unwrap();
}

/// The accounts the paired session's launch admission reads, its
/// enrolled ledger and its application policy, as the image and
/// firstboot leave them.
fn accounts() {
    let session = "td-principals-v1\nsession\t1000\t993\t992\t991\n";
    fs::create_dir_all("/etc").unwrap();
    for (name, text, mode) in [
        ("td-principals.tsv", session, 0o444),
        // The session's applications: none.
        ("td-bus-applications.tsv", "td-bus-applications-v1\t1000\n", 0o444),
        (
            "passwd",
            "tester:x:1000:1000::/home/tester:/bin/false\ntdc1000:x:993:993::/run/td-compositor/1000:/bin/false\ntdb1000:x:992:992::/var/empty:/bin/false\ntdp1000:x:991:991::/var/empty:/bin/false\n",
            0o644,
        ),
        (
            "group",
            "tester:x:1000:\ntdc1000:x:993:\ntdb1000:x:992:\ntdp1000:x:991:\n",
            0o644,
        ),
        (
            "shadow",
            "tester::0:0:99999:7:::\ntdc1000:!td-service:0:0:99999:7:::\ntdb1000:!td-service:0:0:99999:7:::\ntdp1000:!td-service:0:0:99999:7:::\n",
            0o600,
        ),
    ] {
        let path = Path::new("/etc").join(name);
        fs::write(&path, text).unwrap();
        fs::set_permissions(&path, Permissions::from_mode(mode)).unwrap();
    }
    fs::write("/var/lib/td/principals.tsv", session).unwrap();
    fs::set_permissions("/var/lib/td/principals.tsv", Permissions::from_mode(0o600)).unwrap();
}

/// The compositor's runtime directories and devices, given to its account
/// as trusted seat setup gives them; the FIDO node stays root's.
fn seat() {
    for (path, owner, mode) in [
        ("/run/td-compositor", 0, 0o755),
        ("/run/td-compositor/1000", 993, 0o755),
        ("/run/user", 0, 0o755),
        ("/run/user/1000", 1000, 0o700),
        ("/home/tester", 1000, 0o700),
    ] {
        fs::create_dir_all(path).unwrap();
        std::os::unix::fs::chown(path, Some(owner), Some(owner)).unwrap();
        fs::set_permissions(path, Permissions::from_mode(mode)).unwrap();
    }
    for entry in fs::read_dir("/dev/input").unwrap() {
        let path = entry.unwrap().path();
        if path
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("event")
        {
            std::os::unix::fs::chown(&path, Some(993), Some(993)).unwrap();
            fs::set_permissions(&path, Permissions::from_mode(0o600)).unwrap();
        }
    }
    std::os::unix::fs::chown(FRAMEBUFFER, Some(993), Some(993)).unwrap();
    fs::set_permissions(FRAMEBUFFER, Permissions::from_mode(0o600)).unwrap();
    std::os::unix::fs::symlink("/bin/td-compositor", DEMO).unwrap();
}

/// A window of the account's own behind whatever the output shows.
fn demo() -> Process {
    let mut command = Command::new("/bin/td-login");
    command.args([
        "exec-as",
        "tester",
        "--",
        DEMO,
        "run",
        "--socket",
        "/run/td-compositor/1000/wayland-0",
        "--ready-socket",
        DEMO_READY,
    ]);
    let mut process = Process::start(command, "/run/desktop-demo.log");
    desktop::wait("the demo client's window", || {
        assert!(
            process.exited().is_none(),
            "{}",
            fs::read_to_string("/run/desktop-demo.log").unwrap()
        );
        Path::new(DEMO_READY).exists()
    });
    process
}

/// What td-authd's reaped children have faulted (`cminflt`), which grows
/// with every child it reaps: the login worker, the state helper and
/// Prepare's relock.
fn reaped(authority: u32) -> u64 {
    let stat = fs::read_to_string(format!("/proc/{authority}/stat")).unwrap();
    let (_, fields) = stat.rsplit_once(')').unwrap();
    // After the name: state, ppid, pgrp, session, tty, tpgid, flags,
    // minflt, cminflt.
    fields.split_whitespace().nth(8).unwrap().parse().unwrap()
}

/// td-authd's live children's argument vectors.
fn live(authority: u32) -> Vec<Vec<String>> {
    fs::read_dir("/proc")
        .unwrap()
        .filter_map(|entry| entry.unwrap().file_name().into_string().ok())
        .filter(|name| name.bytes().all(|byte| byte.is_ascii_digit()))
        .filter(|pid| {
            fs::read_to_string(format!("/proc/{pid}/stat"))
                .ok()
                .and_then(|stat| {
                    let (_, fields) = stat.rsplit_once(')')?;
                    fields.split_whitespace().nth(1)?.parse::<u32>().ok()
                })
                == Some(authority)
        })
        .filter_map(|pid| fs::read(format!("/proc/{pid}/cmdline")).ok())
        .map(|line| {
            line.split(|byte| *byte == 0)
                .filter(|word| !word.is_empty())
                .map(|word| String::from_utf8_lossy(word).into_owned())
                .collect()
        })
        .collect()
}

/// td-authd at rest: no live child, and none reaped while its children
/// were counted. What it has reaped, for the next look to compare.
fn idle(authority: u32) -> u64 {
    let before = reaped(authority);
    let children = live(authority);
    let after = reaped(authority);
    assert!(children.is_empty(), "td-authd's children: {children:?}");
    assert_eq!(before, after, "td-authd reaped a child while counting");
    before
}

/// A paired generation, once td-authd has no live child at one look.
fn settled() -> Pair {
    let pair = Pair::start();
    desktop::wait("the authority with no live child", || {
        live(pair.authority()).is_empty()
    });
    pair
}

#[test]
#[ignore = "requires qemu-secret with a disposable guest, UHID and the paired desktop"]
fn qemu_login_desktop_starts_every_generation_locked_and_unlocks_with_the_key() {
    guard("login-desktop");
    let _diagnostics = Diagnostics;
    applet(&["hostname", HOSTNAME]);
    let mut host = Host::open();
    let mut keyboard = Keyboard::new();
    release_console();
    prepare();
    accounts();
    seat();
    // The record, as the fixture rule seeds one: a one-key enrollment
    // through this worker, root simulated, both deployments reading this
    // build's version.
    let key = blank("desktop");
    let plugged = Plugged::insert(&key, NAME);
    let (result, _) = physical(begins(enrolling(1, 1, LoginStep::Connect)));
    assert_eq!(result, Ok(()));
    let id = newest(&key);
    assert_eq!(stored(), Some(vec![id.clone()]));
    let print = hex(&fingerprint(&id));

    // Enrolled: the generation's first frame is the lock surface, and a
    // client's window behind it shows nothing.
    blank_screen();
    host.screen("blank", &[]);
    let pair = settled();
    host.screen("locked", &[]);
    let client = demo();
    host.screen("locked", &[]);

    // The chord runs the unlock: a wrong PIN ends it, still locked, and
    // starts no touch.
    chord(&mut keyboard);
    host.screen("pin", &[&print, "8", "0"]);
    type_pin(&mut keyboard, WRONG);
    host.screen("pin", &[&print, "8", "4"]);
    keyboard.key(ENTER);
    host.screen("wrong-pin", &[]);
    assert_eq!(key.state().retries, 7);
    keyboard.key(ESCAPE);
    host.screen("locked", &[]);

    // A key holding none of the record's credentials ends it at identify,
    // with no PIN step.
    let served = plugged.remove();
    // Enrollment's three sessions, then the wrong PIN's identify and
    // assertion sessions.
    assert_eq!(served.channels, 3 + 2);
    let stranger = blank("stranger");
    let plugged = Plugged::insert(&stranger, NAME);
    chord(&mut keyboard);
    host.screen("not-enrolled", &[]);
    keyboard.key(ESCAPE);
    host.screen("locked", &[]);
    assert_eq!(plugged.remove().channels, 1);
    assert_eq!(stranger.state().retries, 8);
    let plugged = Plugged::insert(&key, NAME);

    // The key's PIN and its touch unlock, to the client's window.
    key.script(Script {
        presence: Presence::Delayed(SLOW_TOUCH),
        ..Script::default()
    });
    let before = reaped(pair.authority());
    chord(&mut keyboard);
    host.screen("pin", &[&print, "7", "0"]);
    type_pin(&mut keyboard, PIN);
    host.screen("pin", &[&print, "7", "4"]);
    keyboard.key(ENTER);
    host.screen("touch", &[&print, "7"]);
    // Within the scripted touch, the one worker td-authd started for the
    // unlock's 1b is its only child: what the damaged directory below
    // must not see.
    let children = live(pair.authority());
    assert_eq!(children.len(), 1, "td-authd's children: {children:?}");
    assert!(
        children[0].iter().any(|word| word == "login-operation"),
        "{children:?}"
    );
    host.screen("unlocked", &[]);
    assert_eq!(key.state().retries, 8);
    // Its reaping, among others, shows in what td-authd's reaped children
    // faulted, which the check below reads.
    assert!(reaped(pair.authority()) > before);
    pair.disconnect();
    drop(client);
    // The unlock's identify and assertion sessions.
    assert_eq!(plugged.remove().channels, 2);

    // A restarted generation starts locked again.
    blank_screen();
    host.screen("blank", &[]);
    let pair = settled();
    host.screen("locked", &[]);
    pair.disconnect();

    // A damaged directory: locked with its cause, and the chord shows the
    // cause and sends no unlock, so td-authd starts no worker.
    fs::set_permissions(directory(), Permissions::from_mode(0o755)).unwrap();
    blank_screen();
    host.screen("blank", &[]);
    let pair = settled();
    host.screen("locked-damaged", &[]);
    let before = idle(pair.authority());
    chord(&mut keyboard);
    host.screen("damaged", &[]);
    std::thread::sleep(Duration::from_secs(1));
    assert_eq!(idle(pair.authority()), before);
    keyboard.key(ESCAPE);
    host.screen("locked-damaged", &[]);
    // Nor on Escape, nor on the lock surface again.
    std::thread::sleep(Duration::from_secs(1));
    assert_eq!(idle(pair.authority()), before);
    pair.disconnect();
    fs::set_permissions(directory(), Permissions::from_mode(0o700)).unwrap();

    // Unenrolled, the record removed through the worker: unlocked from the
    // generation's first frame.
    key.script(Script::default());
    let plugged = Plugged::insert(&key, NAME);
    let removed = slots(&[fingerprint(&id)], &[1]);
    let (result, _) = physical(begins(removing(1, &removed, LoginStep::Identify)));
    assert_eq!(result, Ok(()));
    assert!(matches!(state(), State::Unenrolled));
    plugged.remove();
    blank_screen();
    host.screen("blank-unlocked", &[]);
    let pair = Pair::start();
    host.screen("desktop", &[]);
    pair.disconnect();
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
