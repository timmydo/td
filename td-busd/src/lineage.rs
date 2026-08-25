//! Which jailed instance a connection belongs to, proved rather than asserted.
//!
//! `SO_PEERCRED` gives the broker a pid in its OWN namespace, which is the
//! useful half: a jailed application sees itself as pid 1 of a nested
//! namespace and cannot describe itself out of the number the kernel attached
//! to its socket. What that number does not say is which application it is.
//! §D's answer is descent: the broker holds each instance's stage-2 pid, and a
//! connection belongs to the instance whose stage-2 pid is one of its
//! ancestors.
//!
//! The obvious implementation of that is wrong, and §D says why. Checking the
//! connecting pid's start time before and after the walk closes pid reuse at
//! the ENDPOINTS and nowhere else: an intermediate ancestor can exit and have
//! its pid reused between two hops, after which the walk continues up a
//! lineage that is not the one it started in and can land on the registered
//! stage-2 pid by a path that never existed. Both endpoints are exactly what
//! they claim to be while the chain between them is fiction.
//!
//! So every EDGE is validated. Each hop records `(pid, starttime)`, and the
//! completed chain is checked twice over:
//!
//! * every recorded start time must be unchanged on a second read, and
//! * a parent's start time must be **less than or equal to** its child's.
//!
//! The second is the cheap one and it is the one that catches the race the
//! first can miss: a process that reuses a dead pid necessarily started later
//! than the child which already named that pid as its parent, so the
//! substitution shows up as a parent younger than its own child. A chain
//! failing either check is `Unknown` and is denied — never retried, because a
//! retry against an active attacker is a loop rather than a resolution.
//!
//! This is a sampled view of `/proc` and that is a real limitation rather than
//! an implementation detail. §D names the durable fix: a kernel-maintained
//! boundary, either pidfds held from creation or `cgroup.procs` membership of
//! the per-instance cgroup §P already delegates. Take the cgroup oracle when
//! that delegation lands; this walk is the fallback for the ordering where it
//! has not, which is the ordering td is in.
//!
//! # The peer's own pid, and why a pidfd is what proves it
//!
//! Everything above is about ANCESTORS. The pid the walk STARTS from is a
//! separate problem, and the walk being careful makes it easy to assume the
//! starting point was.
//!
//! `SO_PEERCRED` is not sound for it. The kernel samples peer credentials at
//! `connect(2)` and keeps a `struct pid` reference; a reference keeps the
//! STRUCT alive and does not reserve the NUMBER, which `free_pid` returns to
//! the allocator when the connecting process is reaped, after which `pid_vnr`
//! still reports it. A peer that connects, passes its socket to a sibling
//! through `SCM_RIGHTS` and exits can therefore have its pid recycled before
//! the broker reads it — with the delay under its own control, by filling the
//! listen backlog — and the walk would faithfully describe whichever process
//! holds that number now. The dangerous direction is a confined peer
//! resolving `Unconfined`, which is privilege UP.
//!
//! So the pid is taken from `SO_PEERPIDFD` instead, and the argument is a
//! liveness one rather than the "a pidfd cannot be recycled" one §D first
//! gave. That claim is true of the HANDLE and false of the NUMBER: holding a
//! pidfd across a reap does not stop the pid being handed out again, which is
//! measured rather than assumed. What a pidfd gives is the ability to ask.
//! `/proc/self/fdinfo/<pidfd>` reports the pid while the process is alive AND
//! while it is a zombie, and `-1` once it has been reaped — and a reap is
//! exactly what has to happen before a number can be reused.
//!
//! Hence the rule, and both halves are load-bearing:
//!
//! * read the pidfd BEFORE the walk, to learn which pid to start from and to
//!   refuse a peer that is already reaped, and
//! * read it AGAIN after every `/proc` read this lookup makes, and require
//!   the same pid.
//!
//! The second read is the one the soundness rests on. If the pidfd names the
//! peer at the end, the peer was never reaped between `connect(2)` and that
//! moment, so its number was never free, so every `/proc/<pid>` read taken in
//! between was a read of this peer. One read alone does not get there: the
//! peer can be reaped and its number reused in the window between the first
//! read and the walk, and the walk's own re-read of hop zero would then find
//! the impostor's start time unchanged and pass.

use std::collections::BTreeSet;
use std::os::fd::RawFd;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How far up a lineage the walk will climb before giving up.
///
/// A chain this long is pathological — a jailed peer's ancestry to its stage-2
/// pid is a handful of hops, and an unconfined one's to pid 1 is tens — but
/// the bound is what keeps a hostile `/proc` from turning one accept into
/// unbounded work. Exceeding it is `Unknown`, which denies: a walk that ran
/// out of patience has not proved anything, and this module's whole rule is
/// that an unproved lineage is refused.
const MAX_DEPTH: usize = 1024;

/// The three answers, and the middle one is the point.
///
/// `Unconfined` is a POSITIVE result rather than a default, which is what lets
/// §E rest full portal access on it. It means the walk terminated without
/// meeting any registered stage-2 pid AND every registered instance was
/// accounted for at query time — so "descends from none of them" is a
/// statement about a complete registry rather than an absence of evidence.
/// Anything that is merely unproved is `Unknown`, and `Unknown` is denied by
/// both the broker and the portal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Identity {
    Jailed {
        app_id: String,
        instance: String,
        /// The well-known names this instance's permission file granted it.
        ///
        /// It travels WITH the identity rather than being looked up when a
        /// name is asked for, for the reason that makes identity a
        /// once-at-accept decision in the first place: the record this came
        /// from is swept as soon as its process ends, so a later second
        /// lookup could find the instance gone — silently dropping a grant
        /// the connection still holds — or find a DIFFERENT instance that
        /// registered the same name in between. One walk answers about one
        /// instance, and the grant is part of that answer.
        ///
        /// Empty is the ordinary case and the default: §D's sandboxed policy
        /// owns no name unless a permission file says otherwise.
        owned: Vec<String>,
    },
    Unconfined,
    /// Carries why, because a denial nobody can explain is a bug report with
    /// no content: every arm below names the ambiguity it hit.
    Unknown(String),
}

impl Identity {
    /// The `td.AppId` value §D adds to `GetConnectionCredentials`, when there
    /// is one. Absent for everything else — an entry that is missing says "not
    /// known", which is the same rule the uid and pid entries already follow.
    pub fn app_id(&self) -> Option<&str> {
        match self {
            Self::Jailed { app_id, .. } => Some(app_id.as_str()),
            _ => None,
        }
    }
}

/// One process as `/proc/<pid>/stat` describes it, reduced to the three fields
/// this walk needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stat {
    pub ppid: i32,
    /// Field 22: start time in clock ticks since boot. Compared, never
    /// interpreted — the unit does not matter to any rule here, only that two
    /// reads of the same live process agree and that a parent's is not larger
    /// than its child's.
    pub starttime: u64,
}

/// What one `/proc/<pid>/stat` read established.
///
/// Three values rather than an `Option`, because the difference between them
/// is a privilege boundary. A review found the two-valued version fails open:
/// `Option` collapses "this pid does not exist" into "this read did not
/// work", the reap treats the second as the first, and one transient `EMFILE`
/// or `ENOMEM` therefore drops a LIVE instance from the registry. The
/// connection that observed it is refused, but the next connection from
/// inside that jail walks a registry with no record of it and resolves
/// `Unconfined` — full portal access for a process that is certainly
/// confined, with no attacker required.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reading {
    /// The process is there, and this is what it says.
    Of(Stat),
    /// The pid does not exist. This is the only reading that licenses
    /// dropping a record, because it is the only one that says something
    /// about the PROCESS rather than about the read.
    Gone,
    /// The read failed some other way, or the line did not parse. Nothing
    /// follows about the process, so nothing may be concluded about it.
    Unreadable,
}

/// What a pidfd names, read now.
///
/// A pidfd is not a pid. It names a `struct pid`, and the kernel reports a
/// NUMBER for it only while that struct is still allocated to a process. These
/// are the three things `/proc/self/fdinfo/<fd>` can say, and the difference
/// between the last two is a privilege boundary in the same way `Reading`'s is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Named {
    /// The pidfd names this pid and the process has not been reaped, so the
    /// number is still that process's and cannot have been recycled.
    Pid(i32),
    /// The process has been reaped: the kernel reports `-1`, and the number it
    /// held is back in the allocator. Nothing may be concluded from it.
    Reaped,
    /// The read failed, or the line did not parse. Nothing follows.
    Unreadable,
}

/// Where the walk reads process state from.
///
/// A trait rather than a direct `/proc` read because the interesting cases are
/// races: a pid that changes identity BETWEEN two reads is the whole reason
/// the edge validation exists, and there is no way to stage that against the
/// real kernel without losing to timing. The tests drive a table that can
/// answer differently on the second read; production reads `/proc`.
pub trait Procfs {
    fn stat(&self, pid: i32) -> Reading;

    /// What `pidfd` — a descriptor THIS process holds and keeps open across
    /// the call — names right now.
    ///
    /// On the same trait as `stat` because the two are read against each
    /// other: the whole soundness argument is that no reap happened between
    /// the first of these reads and the last, and a fake that could stage one
    /// side and not the other could not stage the race at all.
    fn named_by(&self, pidfd: RawFd) -> Named;
}

/// `/proc/<pid>/stat`, parsed the only way it can safely be parsed.
///
/// The second field is the executable name in parentheses and may contain both
/// spaces and parentheses — `comm` is attacker-controlled through
/// `prctl(PR_SET_NAME)` and the file name — so splitting the line on
/// whitespace and indexing is a bug that a process called `") 1 999999"` can
/// exploit to forge its own ppid. The only correct split is at the LAST `)`,
/// after which the remaining fields are positional and safe.
pub struct RealProcfs;

impl Procfs for RealProcfs {
    fn stat(&self, pid: i32) -> Reading {
        stat_of(std::path::Path::new(&format!("/proc/{pid}/stat")))
    }

    fn named_by(&self, pidfd: RawFd) -> Named {
        named_by(std::path::Path::new(&format!("/proc/self/fdinfo/{pidfd}")))
    }
}

/// One pidfd's `fdinfo`, read and parsed. Split from the impl for the reason
/// `stat_of` is: production reads a path no test can create.
///
/// The `Pid:` line is the kernel's own, reported in the READER's pid
/// namespace — the same namespace `SO_PEERCRED` answers in, which is what
/// makes the two comparable at all. `NSpid:` is deliberately not consulted: it
/// is the pid inside the process's OWN namespace, which for a jailed peer is 1.
///
/// A failed read is `Unreadable` and never `Reaped`. This descriptor is one
/// this process holds, so its `fdinfo` entry exists; anything that stops it
/// being read is the broker's problem and says nothing about the peer. Getting
/// that backwards would turn one `EMFILE` into a denial that reads like a
/// reaped peer.
fn named_by(path: &std::path::Path) -> Named {
    let Ok(raw) = std::fs::read(path) else {
        return Named::Unreadable;
    };
    let mut answer = None;
    for line in raw.split(|byte| *byte == b'\n') {
        // A PREFIX match. `NSpid:` would not match a substring search either
        // — the kernel spells it with a lower-case `p` — so the reason for
        // the prefix is not that one, and a review caught the comment giving
        // it. The reason is that a prefix is what "this line IS the pid" means,
        // and a substring search would accept a `Pid:` appearing anywhere in
        // any future line of this file.
        let Some(rest) = line.strip_prefix(b"Pid:") else {
            continue;
        };
        // A SECOND `Pid:` line is not something to pick a winner from. The
        // kernel emits exactly one; a file with two is a file this code does
        // not understand, and "take the first" would let `Pid: 400` followed
        // by `Pid: -1` read as live. Refused instead of resolved.
        if answer.is_some() {
            return Named::Unreadable;
        }
        let Ok(text) = std::str::from_utf8(rest) else {
            return Named::Unreadable;
        };
        answer = Some(match text.trim().parse::<i32>() {
            // `-1` is how a pidfd says its process has been reaped. Any other
            // non-positive number is not a pid, and is not read as one.
            Ok(-1) => Named::Reaped,
            Ok(pid) if pid > 0 => Named::Pid(pid),
            _ => Named::Unreadable,
        });
    }
    answer.unwrap_or(Named::Unreadable)
}

/// One `stat` file, read and parsed.
///
/// Split from `RealProcfs::stat` only so a test can point it at a file it
/// wrote: the path this reads in production is not one a test can create.
///
/// The read is BYTES, not a string. `comm` is not required to be UTF-8 — the
/// same `prctl(PR_SET_NAME)` that can put a `)` in it can put a stray 0x80 in
/// it — so reading this file as text lets any process make its own `/proc`
/// entry unreadable to the broker. That is not a parse failure the walk
/// shrugs off: `is_still_there` reads "unreadable" as "gone", and an instance
/// wrongly reaped is one whose descendants stop being recognised. Everything
/// after the last `)` is the kernel's own ASCII, so that part converts.
fn stat_of(path: &std::path::Path) -> Reading {
    match std::fs::read(path) {
        Ok(raw) => match parse_stat(&raw) {
            Some(stat) => Reading::Of(stat),
            None => Reading::Unreadable,
        },
        // `/proc/<pid>/stat` answers `ENOENT` for a pid that does not exist,
        // and a zombie still has an entry — so this really is "the process
        // has ended", and it is the ONLY error that says so. Anything else,
        // `EMFILE` and `ENOMEM` included, is the broker's problem rather than
        // a fact about the process, and it is answered as such. There is a
        // narrow third case: the process can exit between the open and the
        // read, which surfaces as `ESRCH` rather than `ENOENT`. That lands
        // here as `Unreadable`, which costs one refusal and one more pass
        // before the record is reaped.
        Err(why) if why.kind() == std::io::ErrorKind::NotFound => Reading::Gone,
        Err(_) => Reading::Unreadable,
    }
}

/// Fields 4 (`ppid`) and 22 (`starttime`) out of a `stat` line.
///
/// After the last `)` the first field is `state`, which is field 3 — so field
/// N is at offset N-3 in what remains.
fn parse_stat(raw: &[u8]) -> Option<Stat> {
    let close = raw.iter().rposition(|byte| *byte == b')')?;
    let rest = std::str::from_utf8(raw.get(close + 1..)?).ok()?;
    let mut fields = rest.split_ascii_whitespace();
    // state, then ppid.
    let _state = fields.next()?;
    let ppid = fields.next()?.parse::<i32>().ok()?;
    // starttime is field 22 and ppid was field 4, so seventeen fields stand
    // between the two.
    let starttime = fields.nth(17)?.parse::<u64>().ok()?;
    Some(Stat { ppid, starttime })
}

/// A registered jail instance, complete: phase two has bound a pid to it.
#[derive(Debug, Clone)]
pub struct Instance {
    pub instance: String,
    pub app_id: String,
    /// Stage 2's pid in the BROKER's namespace, read out of stage 1 rather
    /// than announced by stage 2 — a record the confined process supplies is a
    /// record the confined process chooses.
    pub pid: i32,
    pub starttime: u64,
    /// Names this instance may activate on its own private listener.
    ///
    /// Predeclared at phase one because §D puts them there, and stored unread
    /// until activation lands. Carrying them now rather than adding them later
    /// is what keeps the registration protocol from needing a breaking change
    /// the moment it acquires its first real consumer: `td.Jail1` is versioned
    /// for that eventuality, and spending the version on a field the design
    /// already specifies would be spending it badly.
    #[allow(dead_code, reason = "read by activation; predeclared here per §D")]
    pub services: Vec<String>,
    /// Well-known names this instance may take on the session bus.
    ///
    /// §D's `[Session Bus Policy]` `own` entries, which are the only widening
    /// of a default policy that owns no name. Unlike `services` this one is
    /// READ: `policy::may_own` consults the copy that reached the identity.
    ///
    /// Registrant-supplied, like the app id and for the same reason — v1
    /// authenticates registration by uid and nothing else — so the transport
    /// grades every entry before it arrives, and the broker's reservation is
    /// applied again at the point of use rather than trusted from here.
    pub owned: Vec<String>,
}

/// What phase one is told about an instance.
///
/// A struct rather than four more parameters on `open`, and the reason is the
/// pair of `Vec<String>` fields: as positional arguments they are adjacent and
/// interchangeable, so a caller that swapped them would compile — and the
/// mistake it makes, granting an instance ownership of the names it meant to
/// ACTIVATE, is exactly the one no signature can catch. A mutation that did
/// precisely that survived a whole suite before this became a struct.
///
/// Every field is registrant-supplied. §D authenticates registration by uid
/// and nothing else in v1, so what is recorded here is a claim, graded for
/// shape at the wire and worth what §D says a launcher's word is worth.
#[derive(Debug, Clone)]
pub struct Registration {
    pub instance: String,
    pub app_id: String,
    /// Names this instance may activate on its own private listener.
    pub services: Vec<String>,
    /// Names it may take on the session bus — §D's `own` entries.
    pub owned: Vec<String>,
}

/// A caller the transport has PROVED, rather than a number it was told.
///
/// The pid comes from `SO_PEERPIDFD` and the start time from `/proc`, read
/// between two agreeing reads of that pidfd -- so the pair names one PROCESS.
/// A pid on its own is a number the allocator recycles, which is the whole
/// subject of this module; a pid plus the start time of the process that held
/// it is the identity every other rule here is written in terms of.
///
/// The registry does not construct one. It is made where the socket is, in the
/// transport, because that is the only place a pidfd for the peer exists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Caller {
    pub pid: i32,
    pub starttime: u64,
}

/// How long a registration may stand between its two phases.
///
/// This is not a policy knob, it is the fix for an availability hole: a
/// pending registration makes `Unconfined` unsayable for EVERY connection, so
/// a stage 0 that registered and then died — killed, crashed, or refused by
/// its own mount plan — would deny the whole session until the broker
/// restarted. Nothing else prunes it, because the registrant is exactly the
/// party that is no longer there.
///
/// Generous rather than tight, because the cost of being wrong in the two
/// directions is not symmetric. Expiring early breaks a legitimate launch;
/// expiring late leaves the session degraded a little longer, and the whole
/// window is already bounded by `MAX_INSTANCES`. Everything stage 1 does
/// between the phases — unshare, build the root, spawn — is sub-second on any
/// machine that can run a compositor.
pub const PENDING_LIFETIME: Duration = Duration::from_secs(60);

/// Registration in flight: the instance exists, has no pid, and accepts
/// nothing.
#[derive(Debug, Clone)]
struct Pending {
    token: String,
    instance: String,
    app_id: String,
    services: Vec<String>,
    owned: Vec<String>,
    /// The uid that opened phase one. Phase two must come from the same uid.
    /// In v1 every session peer is uid 1000 so this refuses nothing today —
    /// §D is explicit that registration is authenticated by uid and that the
    /// app id is therefore a string the registrant supplies. It is written now
    /// because per-app uids (§L v2) make this line the whole check, and a
    /// check added later is a check that was missing in between.
    uid: u32,
    /// When phase one ran, for `PENDING_LIFETIME`.
    opened: Instant,
    /// The pid that opened phase one, which is what narrows the blast radius
    /// of an incomplete registration.
    ///
    /// A pending instance has no stage-2 pid on record, so a peer belonging to
    /// it would walk straight past and off the top — and `Unconfined` there
    /// would hand full portal access to the one process that is certainly
    /// confined. The first version of this refused `Unconfined` to EVERY peer
    /// while any registration was open, which is safe and far too broad: any
    /// uid-1000 process could open one, never complete it, and deny the whole
    /// session.
    ///
    /// It can be narrowed exactly, because stage 2 is a CHILD of the registrant
    /// — stage 0 registers before it unshares, and `unshare(CLONE_NEWPID)`
    /// moves the caller's children into the new namespace rather than the
    /// caller — so every peer that could belong to a pending instance is a
    /// strict descendant of the pid that opened it. Peers elsewhere in the
    /// process tree are unaffected, and a rogue registration now blocks only
    /// the rogue's own descendants.
    ///
    /// A `Caller` and not a pid, because phase two arrives on a DIFFERENT
    /// connection and comparing numbers across that gap is the mistake the
    /// walk stopped making one commit ago and the registry went on making. If
    /// the registrant ends between the phases and its number is reused, the
    /// process holding it is not the one that opened, and a number-only check
    /// cannot say so.
    registrant: Caller,
}

/// Every instance the broker knows, pending and complete.
///
/// Registration is two-phase because the pid does not exist when the instance
/// does: stage 0 unshares nothing yet and has no stage-2 pid to name, so it
/// opens with `{instance, app-id, services, owned names}` and receives a
/// one-shot token,
/// and stage 1 completes with the pid `Command::spawn` returned. A connection
/// arriving between the two phases resolves against a registry that does not
/// yet contain the instance — it fails closed, as §D requires, rather than
/// being queued.
pub struct Instances {
    inner: Mutex<State>,
}

#[derive(Default)]
struct State {
    pending: Vec<Pending>,
    live: Vec<Instance>,
}

/// The most registrations that may be open at once, and the most instances
/// that may be live.
///
/// Both are the same number and both are the accept path's problem rather than
/// an aesthetic one: `resolve` re-reads every live instance's pid before it
/// will answer `Unconfined`, so an unbounded registry turns one connection
/// into unbounded `/proc` reads. A registrant that fills this is refused.
pub const MAX_INSTANCES: usize = 64;

/// The most service names one instance may predeclare.
pub const MAX_SERVICES: usize = 32;

/// The most well-known names one instance's permission file may grant it.
///
/// The same number as `MAX_SERVICES` and for a plainer reason: this list is
/// copied into every `Identity` the walk answers with, and an identity is
/// resolved once per ACCEPT. An unbounded grant list would make one
/// registration's permission file the cost of every later connection. It also
/// bounds `may_own`, which scans the list on each `RequestName`.
///
/// It is a ceiling rather than a budget. Thirty-two exact well-known names is
/// already far more than any application in §B declares.
///
/// `td_engine::permissions::MAX_OWNED_BUS_NAMES` is the same number, stated
/// again because that crate is not linked here, and charged by `td-jail`'s
/// spec grader so an oversized permission file is refused with a reason
/// naming the ceiling rather than arriving as `that list of names cannot be
/// read`. Change one and change the other.
pub const MAX_OWNED_NAMES: usize = 32;

impl Default for Instances {
    fn default() -> Self {
        Self::new()
    }
}

impl Instances {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(State::default()),
        }
    }

    /// Phase one: the instance exists and has no pid. Returns its one-shot
    /// token.
    ///
    /// The token binds two calls together: it is what makes "stage 1 refuses
    /// to proceed without the token stage 0 obtained" checkable, and what
    /// makes a second attempt to bind a pid an error rather than a takeover.
    ///
    /// It has to be unguessable, which a draft got wrong by reasoning that
    /// anything able to guess it could simply call `Register` itself. That
    /// covers creating a new instance and misses the interesting move:
    /// CONSUMING somebody else's in-flight registration. The real stage 1's
    /// `Complete` then fails — and by then stage 2 has already been spawned,
    /// because its pid is what `Complete` was going to carry. A live jail with
    /// no registration on record is exactly the `Unconfined` answer §E exists
    /// to prevent. See `fresh_token`.
    pub fn open(
        &self,
        procfs: &dyn Procfs,
        registration: Registration,
        uid: u32,
        registrant: Caller,
    ) -> Result<String, String> {
        self.open_at(procfs, registration, uid, registrant, Instant::now())
    }

    /// `open` with the clock supplied, so the sweep it performs can be
    /// asserted rather than waited for.
    fn open_at(
        &self,
        procfs: &dyn Procfs,
        registration: Registration,
        uid: u32,
        registrant: Caller,
        now: Instant,
    ) -> Result<String, String> {
        let Registration {
            instance,
            app_id,
            services,
            owned,
        } = registration;
        if services.len() > MAX_SERVICES {
            return Err(format!(
                "an instance may predeclare at most {MAX_SERVICES} service names"
            ));
        }
        if owned.len() > MAX_OWNED_NAMES {
            return Err(format!(
                "an instance may be granted at most {MAX_OWNED_NAMES} well-known names"
            ));
        }
        // Read outside the lock: it touches a device file, and the registry is
        // on the accept path.
        let token = fresh_token()?;
        // BOTH sides of the ceiling are swept, not just the pending one. A
        // draft swept only pendings, and a review pointed out that leaves the
        // identical ratchet in the neighbouring collection: an instance whose
        // jail exited is reaped by `resolve` alone, so a launcher whose
        // applications never touch the bus fills all 64 slots and every later
        // `Register` is refused for good. This reads `/proc` outside the lock,
        // for the reason `resolve_against` gives.
        self.sweep_live(procfs);
        let mut state = self
            .inner
            .lock()
            .map_err(|_| "the instance registry is poisoned".to_string())?;
        // Registrations that were abandoned do not get to hold slots. Without
        // this the ceiling is a one-way ratchet: `resolve` is the only other
        // sweep, so 64 half-finished registrations refuse every launch until
        // some connection happens to arrive.
        sweep_pending(&mut state, now);
        if state.pending.len() + state.live.len() >= MAX_INSTANCES {
            return Err(format!("already tracking {MAX_INSTANCES} instances"));
        }
        if state.pending.iter().any(|p| p.instance == instance)
            || state.live.iter().any(|i| i.instance == instance)
        {
            return Err(format!("instance {instance:?} is already registered"));
        }
        state.pending.push(Pending {
            token: token.clone(),
            instance,
            app_id,
            services,
            owned,
            uid,
            opened: now,
            registrant,
        });
        Ok(token)
    }

    /// Phase two: bind a pid to the instance the token opened.
    ///
    /// The START TIME is read here rather than accepted from the caller. A
    /// registrant-supplied start time would be the one field of the record
    /// that the registrant chooses, and it is the field every later reuse
    /// check rests on — so the broker reads it for itself, and a pid that is
    /// already gone completes nothing.
    ///
    /// The PID is not taken on trust either, and a draft did take it. Under
    /// that version any session peer could open a registration and complete it
    /// with any pid it could read: completing with pid 1 makes every later
    /// connection in the session walk into the attacker's instance and be
    /// handed its app id. Two checks close it, and both are things the broker
    /// sees for itself rather than assertions about the caller:
    ///
    /// - the PROCESS that completes must be the process that opened, so a
    ///   guessed or stolen token is not enough on its own;
    /// - the pid must be a CHILD of that process, which is what stage 2 is —
    ///   stage 1 spawns it directly, so `/proc` records the registrant as its
    ///   parent.
    ///
    /// The process and not the CONNECTION, which a draft claimed and a review
    /// caught. One connection would be the stronger rule and it would break
    /// the only launcher there is: td-jail closes every descriptor above
    /// stderr between `unshare` and the spawn (§A step 0), so the connection
    /// stage 0 registered on is gone by the time stage 1 has a pid to report.
    /// Stage 1 reconnects, and it is the same PROCESS throughout because
    /// `unshare(CLONE_NEWPID)` does not move the caller.
    ///
    /// Together they say a registrant may label its own child and nothing
    /// else. That is not authenticity — §D's v1 exposure stands, and a rogue
    /// can still call its own child `org.mozilla.firefox` — but it is the
    /// difference between mislabelling a process you already own and
    /// relabelling somebody else's.
    ///
    /// This is a requirement on td-jail rather than an observation about it,
    /// and §D records it as one: a launcher that registered on behalf of a
    /// sibling rather than a descendant would be refused here.
    pub fn complete(
        &self,
        procfs: &dyn Procfs,
        token: &str,
        pid: i32,
        uid: u32,
        completer: Caller,
    ) -> Result<(), String> {
        self.complete_at(procfs, token, pid, uid, completer, Instant::now())
    }

    /// `complete` with the clock supplied, so the expiry rule can be asserted
    /// here rather than only where `resolve` happens to sweep.
    #[allow(clippy::too_many_arguments, reason = "one clock past rustc's six")]
    fn complete_at(
        &self,
        procfs: &dyn Procfs,
        token: &str,
        pid: i32,
        uid: u32,
        completer: Caller,
        now: Instant,
    ) -> Result<(), String> {
        // No `pid <= 0` guard. The wire converts from `u32`, so only zero can
        // arrive, and `/proc/0/stat` answers `ENOENT` like any other absent
        // pid — the read below refuses it with a reason rather than a special
        // case. A draft had one, and a review named it the same dead branch
        // shaped like a safety check that this module already removed once.
        //
        // Before the lock, for the reason `resolve_against` gives: a `/proc`
        // read can block, and one unreadable process must not stall every
        // other accept. Reading first also means a transient failure does not
        // consume the token.
        let stat = match procfs.stat(pid) {
            Reading::Of(stat) => stat,
            Reading::Gone => return Err(format!("pid {pid} does not exist")),
            Reading::Unreadable => {
                return Err(format!("pid {pid} has no readable /proc entry"))
            }
        };
        let mut state = self
            .inner
            .lock()
            .map_err(|_| "the instance registry is poisoned".to_string())?;
        // A token that stood too long is gone before it is looked up, rather
        // than surviving until some connection happens to sweep.
        sweep_pending(&mut state, now);
        let Some(at) = state.pending.iter().position(|p| p.token == token) else {
            return Err("no registration is open under that token".to_string());
        };
        let Some(pending) = state.pending.get(at) else {
            return Err("no registration is open under that token".to_string());
        };
        if pending.uid != uid {
            return Err(format!(
                "registration was opened by uid {} and completed by uid {uid}",
                pending.uid
            ));
        }
        // The process, not the connection — see this method's doc comment.
        //
        // A PID AND A START TIME, because the two phases arrive on different
        // connections and a bare number is a comparison across a gap the
        // registrant need not survive. The division of labour is exact, and a
        // review made this comment state it rather than overstate it: the
        // TRANSPORT establishes that each caller is a process that has not
        // been reaped, reading its start time inside the pidfd bracket, and
        // this comparison establishes that the two callers are the same one.
        // Neither half is the other's spare.
        //
        // TWO checks and not one conjunction. A reviewer's mutation joined
        // them with `&&`, which passes for a caller that differs in exactly
        // one field -- a different pid that started in the same clock tick
        // would then complete somebody else's token -- and every test at the
        // time differed in both. The cross-product case is pinned below.
        if pending.registrant.pid != completer.pid {
            return Err(format!(
                "registration was opened by pid {} and completed by pid {}",
                pending.registrant.pid, completer.pid
            ));
        }
        if pending.registrant.starttime != completer.starttime {
            return Err(format!(
                "the process that opened this registration at pid {} has ended, \
                 and that pid now belongs to something else",
                pending.registrant.pid
            ));
        }
        // A child of the COMPLETER, which the two checks above have just
        // established is the process that opened. The child's own pid stays a
        // number, soundly: the registrant may only name a process whose parent
        // it currently is, so if the child it meant had been reaped and its
        // number reused, the reusing process would have to be another of the
        // registrant's own children to pass. The worst available is
        // mislabelling a process it already owns, which is §D's v1 exposure,
        // rather than reaching one it does not.
        if stat.ppid != completer.pid {
            return Err(format!("pid {pid} is not a child of the registering process"));
        }
        if state.live.iter().any(|i| i.pid == pid) {
            return Err(format!("pid {pid} is already bound to an instance"));
        }
        // Consumed on completion, so a second attempt to bind a pid to this
        // instance finds no token rather than replacing the record.
        let pending = state.pending.swap_remove(at);
        state.live.push(Instance {
            instance: pending.instance,
            app_id: pending.app_id,
            pid,
            starttime: stat.starttime,
            services: pending.services,
            owned: pending.owned,
        });
        Ok(())
    }

    /// The grant on record for `instance`, pending or live.
    ///
    /// Both halves, because the grant is carried at phase one and the whole
    /// point of asking is to see what phase one recorded rather than what
    /// phase two later copied.
    #[cfg(test)]
    pub(crate) fn granted(&self, instance: &str) -> Option<Vec<String>> {
        let state = self.inner.lock().ok()?;
        state
            .pending
            .iter()
            .find(|p| p.instance == instance)
            .map(|p| p.owned.clone())
            .or_else(|| {
                state
                    .live
                    .iter()
                    .find(|i| i.instance == instance)
                    .map(|i| i.owned.clone())
            })
    }

    #[cfg(test)]
    fn live_count(&self) -> usize {
        self.inner.lock().map(|s| s.live.len()).unwrap_or(0)
    }

    #[cfg(test)]
    pub(crate) fn pending_count(&self) -> usize {
        self.inner.lock().map(|s| s.pending.len()).unwrap_or(0)
    }

    /// Sweep, then report the registrant pids of the registrations that
    /// remain — which is what the walk needs, and what asking twice would
    /// race with itself to get.
    fn expire_pending(&self, now: Instant) -> Result<Vec<Caller>, ()> {
        match self.inner.lock() {
            Ok(mut state) => {
                sweep_pending(&mut state, now);
                Ok(state.pending.iter().map(|p| p.registrant).collect())
            }
            Err(_) => Err(()),
        }
    }

    /// Which instance this peer belongs to, or a named reason it cannot be
    /// said.
    ///
    /// The argument is a pidfd the CALLER owns and keeps open across the call
    /// — `RawFd` because a fake has no descriptor to borrow, and there is no
    /// safe way to build a `BorrowedFd` from a number a test invented. A
    /// closed descriptor here is not so much a soundness hole as a reused
    /// number naming something else entirely, so the ownership rule is the
    /// caller's to keep; the accept path keeps it by holding the `OwnedFd` for
    /// the length of this call and no longer.
    ///
    /// Five things happen here and only the middle one is the walk.
    ///
    /// FIRST, the pidfd says which pid to start from — and refuses outright if
    /// the peer has already been reaped, which is the case `SO_PEERCRED`
    /// cannot tell apart from a live peer.
    ///
    /// SECOND, instances whose stage-2 process is gone are reaped. Without this
    /// a jail that simply exited would make `Unconfined` unsayable for every
    /// later connection for as long as the broker ran — the accounting pass
    /// below cannot tell "this instance ended" from "this instance's pid may
    /// have been reused underneath the walk I just did", so it refuses, and a
    /// registry nobody prunes turns every ordinary application exit into a
    /// permanent denial for everybody. Reaping is safe for the reason §E gives
    /// for the whole design: stage 2 is PID 1 of the instance's pid namespace,
    /// so killing it kills the namespace, and a dead instance has no live
    /// descendants left to misattribute.
    ///
    /// THIRD, the walk, outside the lock.
    ///
    /// FOURTH, a registration IN FLIGHT makes `Unconfined` unsayable, which is
    /// §E's own list of `Unknown` cases and not a conservative extra. Between
    /// phase one and phase two an instance has no pid on record, so a peer
    /// descending from it walks straight past and off the top — and answering
    /// `Unconfined` there would hand full portal access to the one process
    /// that is definitely confined. `Jailed` is still answerable while a
    /// registration is pending, because that answer rests on a positive match
    /// rather than on the registry being complete.
    ///
    /// FIFTH and last, the pidfd is read again. Every `/proc` read above is
    /// attributable to this peer only if the peer was never reaped while they
    /// were happening, and this is the read that establishes it.
    pub fn resolve(&self, procfs: &dyn Procfs, peer: RawFd) -> Identity {
        self.resolve_at(procfs, peer, Instant::now())
    }

    /// `resolve` with the clock supplied, so the expiry rule can be asserted
    /// rather than waited for.
    ///
    /// The sweep is INSIDE this function on purpose: a draft left the test
    /// calling `expire_pending` itself and then checking `resolve`, which
    /// passes just as happily if `resolve` stops sweeping at all.
    fn resolve_at(&self, procfs: &dyn Procfs, peer: RawFd, now: Instant) -> Identity {
        let pid = match procfs.named_by(peer) {
            Named::Pid(pid) => pid,
            // Not `Unconfined`, and the difference matters: a reaped peer is
            // precisely the process whose pid may already belong to somebody
            // else, so there is nothing left here to be unconfined ABOUT.
            Named::Reaped => {
                return Identity::Unknown(
                    "the peer was reaped before it could be identified, and the \
                     pid its socket reports is free for reuse"
                        .to_string(),
                )
            }
            Named::Unreadable => {
                return Identity::Unknown(
                    "the peer's pidfd could not be read, so which process \
                     connected is not established"
                        .to_string(),
                )
            }
        };
        let Ok(pending) = self.expire_pending(now) else {
            return Identity::Unknown("the instance registry is poisoned".to_string());
        };
        let live = match self.inner.lock() {
            Ok(state) => state.live.clone(),
            Err(_) => return Identity::Unknown("the instance registry is poisoned".to_string()),
        };
        let (gone, unsure) = self.sweep_live(procfs);
        if let Some(why) = unsure {
            return Identity::Unknown(why);
        }
        // A record that was just dropped travels WITH the walk rather than
        // refusing ahead of it. A draft refused every connection that observed
        // any reap — sound, and too broad: a review pointed out that one peer
        // can then deny an arbitrary connection at will, by registering its
        // own child, completing, killing it, and letting the next connection
        // trip over the stale record. Because identity is decided once at
        // accept, that connection is denied for its whole life. What actually
        // makes an answer unsound is narrower and checkable: the dropped pid
        // standing in THIS connection's lineage, which is the only way its
        // reuse could have bent the chain.
        let reaped: Vec<i32> = gone.iter().map(|(_, pid, _)| *pid).collect();
        let live: Vec<Instance> = live
            .into_iter()
            .filter(|instance| {
                !gone.iter().any(|(name, pid, starttime)| {
                    *name == instance.instance
                        && *pid == instance.pid
                        && *starttime == instance.starttime
                })
            })
            .collect();
        let identity = resolve_against(procfs, &live, &pending, &reaped, pid);
        // The read the whole thing rests on, and it is applied to EVERY answer
        // rather than only to `Unconfined`. A `Jailed` answer reached through
        // a recycled pid attributes one application's connection to another,
        // which is a smaller privilege move than reaching `Unconfined` and is
        // still a wrong one.
        match procfs.named_by(peer) {
            Named::Pid(again) if again == pid => identity,
            _ => Identity::Unknown(format!(
                "pid {pid} did not survive this lookup, so nothing read from \
                 /proc while it ran is attributable to the peer"
            )),
        }
    }

    /// Drop every live record whose process is certainly gone, and report
    /// what was dropped and the first instance that could not be read at all.
    ///
    /// Reaping is decided outside the lock and applied under it, keyed on the
    /// pid and start time that were seen to be gone — so an instance that
    /// registered again in between is not removed by a decision taken about
    /// its predecessor.
    fn sweep_live(&self, procfs: &dyn Procfs) -> (Vec<(String, i32, u64)>, Option<String>) {
        let live = match self.inner.lock() {
            Ok(state) => state.live.clone(),
            Err(_) => {
                return (
                    Vec::new(),
                    Some("the instance registry is poisoned".to_string()),
                )
            }
        };
        let mut gone: Vec<(String, i32, u64)> = Vec::new();
        let mut unsure: Option<String> = None;
        for instance in &live {
            match standing_of(procfs, instance) {
                Standing::There => {}
                Standing::Gone => {
                    gone.push((
                        instance.instance.clone(),
                        instance.pid,
                        instance.starttime,
                    ));
                }
                // Refused, and NOT reaped. The distinction is the whole point
                // of `Reading`: dropping a record the broker merely failed to
                // read would hand the next connection from inside that jail an
                // `Unconfined` answer.
                Standing::Unsure => {
                    unsure = Some(format!(
                        "instance {:?} is registered at pid {} and that process \
                         could not be read",
                        instance.instance, instance.pid
                    ));
                }
            }
        }
        if !gone.is_empty() {
            self.reap(&gone);
        }
        (gone, unsure)
    }

    /// Drop exactly the records that were seen to be gone.
    fn reap(&self, gone: &[(String, i32, u64)]) {
        if let Ok(mut state) = self.inner.lock() {
            state.live.retain(|instance| {
                !gone.iter().any(|(name, pid, starttime)| {
                    *name == instance.instance
                        && *pid == instance.pid
                        && *starttime == instance.starttime
                })
            });
        }
    }
}

/// Drop registrations that stood too long between their phases.
///
/// Both callers hold the lock already, which is why this takes the state
/// rather than the registry.
fn sweep_pending(state: &mut State, now: Instant) {
    let before = state.pending.len();
    state
        .pending
        .retain(|p| now.duration_since(p.opened) < PENDING_LIFETIME);
    let expired = before.saturating_sub(state.pending.len());
    if expired > 0 {
        // Loudly, because the consequence is a launch that fails much later
        // and somewhere else: `Complete` will find no token, and §D requires
        // stage 1 to refuse to proceed without one. A silent expiry would make
        // that refusal look like a bug in the jail.
        eprintln!(
            "td-busd: {expired} jail registration(s) expired unfinished after \
             {PENDING_LIFETIME:?}"
        );
    }
}

/// A fresh one-shot registration token: 16 bytes of `/dev/urandom`, hex.
///
/// A draft used a monotone counter and the instance name. That is derivable —
/// a peer's own registrations tell it the counter, and `open`'s two distinct
/// refusals are a name oracle — and `open` says why a derivable token is not
/// merely untidy.
///
/// An unreadable `/dev/urandom` refuses the registration rather than falling
/// back, because every fallback is a predictable token.
fn fresh_token() -> Result<String, String> {
    use std::io::Read;
    let mut file = std::fs::File::open("/dev/urandom")
        .map_err(|why| format!("no randomness for a registration token: {why}"))?;
    let mut bytes = [0u8; 16];
    file.read_exact(&mut bytes)
        .map_err(|why| format!("no randomness for a registration token: {why}"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// One hop of the walk, as it was seen on the way up.
#[derive(Debug, Clone, Copy)]
struct Hop {
    pid: i32,
    starttime: u64,
}

/// The walk itself, over a snapshot of the live instances.
///
/// Split out from `Instances::resolve` so it holds no lock: every `/proc` read
/// below can block, and holding the registry across them would let one
/// unreadable process stall every other accept.
fn resolve_against(
    procfs: &dyn Procfs,
    live: &[Instance],
    pending: &[Caller],
    reaped: &[i32],
    pid: i32,
) -> Identity {
    if pid <= 0 {
        return Identity::Unknown(format!("pid {pid} is not a process"));
    }
    let mut chain: Vec<Hop> = Vec::new();
    let mut seen = BTreeSet::new();
    let mut cursor = pid;
    let mut found: Option<&Instance> = None;

    loop {
        if chain.len() >= MAX_DEPTH {
            return Identity::Unknown(format!(
                "the lineage of pid {pid} is deeper than {MAX_DEPTH} processes"
            ));
        }
        // A `/proc` that repeats a pid on the way up is not a tree. It cannot
        // happen against a live kernel, and a walk that trusts it loops.
        if !seen.insert(cursor) {
            return Identity::Unknown(format!("the lineage of pid {pid} repeats pid {cursor}"));
        }
        let stat = match procfs.stat(cursor) {
            Reading::Of(stat) => stat,
            // Both deny. The walk has no use for the distinction — a lineage
            // it cannot read is a lineage it cannot vouch for either way — and
            // only the registry's own bookkeeping cares which it was.
            Reading::Gone => {
                return Identity::Unknown(format!("pid {cursor} does not exist"));
            }
            Reading::Unreadable => {
                return Identity::Unknown(format!("pid {cursor} has no readable /proc entry"));
            }
        };
        chain.push(Hop {
            pid: cursor,
            starttime: stat.starttime,
        });
        // A registered stage-2 pid whose start time matches is the answer. One
        // whose start time does NOT match is a reused pid rather than that
        // instance, and is not treated as a match — the accounting pass below
        // is what turns that into a refusal.
        if let Some(instance) = live
            .iter()
            .find(|i| i.pid == cursor && i.starttime == stat.starttime)
        {
            found = Some(instance);
            break;
        }
        if cursor == 1 || stat.ppid <= 0 {
            break;
        }
        cursor = stat.ppid;
    }

    if let Some(reason) = chain_is_broken(procfs, &chain) {
        return Identity::Unknown(reason);
    }

    // A pid this lookup just dropped from the registry, standing in this
    // connection's lineage, is the case the reap cannot answer around: that
    // instance ended, its pid may already belong to the hop the walk passed
    // through, and every hop above it was reached by trusting that hop. NOT
    // gated on `found`, because a positive match further up was reached
    // through the same suspect hop.
    if let Some(hop) = chain.iter().find(|hop| reaped.contains(&hop.pid)) {
        return Identity::Unknown(format!(
            "pid {} was dropped from the registry during this lookup and this \
             connection's lineage passes through it",
            hop.pid
        ));
    }

    // A registration in flight for one of this peer's ANCESTORS means the
    // stage 2 it would belong to may not be on record yet. Strict ancestors
    // only: the registrant itself is stage 0/1, an ordinary unconfined process
    // that has to stay able to make the very call that completes the
    // registration. Blocking it would be a self-lock — the broker would deny
    // the connection whose next message is `Complete`.
    if found.is_none() {
        // Matched on the PAIR, not on the number. A pending registrant that
        // ended and whose pid was handed on would otherwise deny `Unconfined`
        // to the new holder's descendants for the rest of
        // `PENDING_LIFETIME` — which is precisely the availability lever this
        // rule was narrowed to descendants to avoid, aimed at a process that
        // never registered anything. The pair has been on record since the
        // registry started taking a proved caller; this spends it.
        if let Some(hop) = chain.iter().skip(1).find(|hop| {
            pending
                .iter()
                .any(|opener| opener.pid == hop.pid && opener.starttime == hop.starttime)
        }) {
            return Identity::Unknown(format!(
                "a registration opened by pid {} is still in flight, and this \
                 connection descends from it",
                hop.pid
            ));
        }
    }

    match found {
        Some(instance) => Identity::Jailed {
            app_id: instance.app_id.clone(),
            instance: instance.instance.clone(),
            owned: instance.owned.clone(),
        },
        // `Unconfined` is the claim that this pid descends from NO registered
        // instance, which is only worth anything if every registered instance
        // is still where the registry says it is. An instance whose stage-2
        // pid has died may have had that pid reused — possibly by something in
        // the chain just walked — and then "descends from none of them" is a
        // statement about a registry that no longer describes the machine.
        None => match unaccounted_instance(procfs, live) {
            Some(reason) => Identity::Unknown(reason),
            None => Identity::Unconfined,
        },
    }
}

/// Re-read every hop and check the two invariants. `None` is a good chain.
fn chain_is_broken(procfs: &dyn Procfs, chain: &[Hop]) -> Option<String> {
    for hop in chain {
        match procfs.stat(hop.pid) {
            Reading::Of(now) if now.starttime == hop.starttime => {}
            Reading::Of(now) => {
                return Some(format!(
                    "pid {} was replaced during the walk: start time {} became {}",
                    hop.pid, hop.starttime, now.starttime
                ));
            }
            Reading::Gone => return Some(format!("pid {} left during the walk", hop.pid)),
            Reading::Unreadable => {
                return Some(format!("pid {} became unreadable during the walk", hop.pid));
            }
        }
    }
    // Adjacent pairs, child first. Only ONE thing is worth checking here, and
    // a draft checked two: it also required each child's recorded ppid to
    // equal the parent the walk moved to. That can never fail, because the
    // walk BUILDS the chain by following that ppid — the two values are made
    // equal one line apart, and a test written to break the link could only
    // break it by rewriting `/proc` in a way the walk never reads.
    //
    // What does the work is the ordering. A parent may not be younger than its
    // own child, and a reused pid cannot avoid being: whatever took the pid
    // had to start after the original exited, and the original was still alive
    // when the child named it as parent, so the impostor is necessarily
    // younger than the child that points at it.
    for pair in chain.windows(2) {
        let (Some(child), Some(parent)) = (pair.first(), pair.get(1)) else {
            continue;
        };
        if parent.starttime > child.starttime {
            return Some(format!(
                "pid {} started after its child {}, so its pid was reused",
                parent.pid, child.pid
            ));
        }
    }
    None
}

/// Where this instance's stage-2 process stands.
///
/// The start time is what makes this a question about the PROCESS rather than
/// about the number: a pid that has been reused answers "yes" to "does this
/// pid exist" and is a different process entirely.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Standing {
    /// Still the process the registry recorded.
    There,
    /// Certainly not: either the pid is unused or it belongs to a process that
    /// started later. Both are final, and for the same reason — the process
    /// the registry recorded has ended.
    Gone,
    /// The broker could not tell. Nothing may be concluded and nothing may be
    /// dropped; see `Reading`.
    Unsure,
}

fn standing_of(procfs: &dyn Procfs, instance: &Instance) -> Standing {
    match procfs.stat(instance.pid) {
        Reading::Of(now) if now.starttime == instance.starttime => Standing::There,
        Reading::Of(_) => Standing::Gone,
        Reading::Gone => Standing::Gone,
        Reading::Unreadable => Standing::Unsure,
    }
}

/// The first live instance that is no longer where the registry says, if any.
///
/// `Instances::resolve` reaps these before it calls the walk, so in production
/// this reports only an instance that died between that reap and this check —
/// which is exactly the race §E names, and which denies rather than reaps
/// because a process that vanished mid-answer has not been shown to be absent
/// from the lineage just walked.
fn unaccounted_instance(procfs: &dyn Procfs, live: &[Instance]) -> Option<String> {
    for instance in live {
        match standing_of(procfs, instance) {
            Standing::There => {}
            Standing::Gone => {
                return Some(format!(
                    "instance {:?} is registered at pid {} and that process is gone",
                    instance.instance, instance.pid
                ));
            }
            Standing::Unsure => {
                return Some(format!(
                    "instance {:?} is registered at pid {} and that process \
                     could not be read",
                    instance.instance, instance.pid
                ));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::collections::BTreeMap;
    use std::os::fd::AsRawFd;

    /// A `/proc` the test writes, and can change between reads.
    #[derive(Default)]
    struct Table {
        now: RefCell<BTreeMap<i32, Stat>>,
        /// Reads served so far, so a test can swap a process in after the walk
        /// has passed it and before the re-read reaches it.
        reads: RefCell<usize>,
        /// `(after_n_reads, pid, replacement)` — applied once.
        swap: RefCell<Option<(usize, i32, Option<Stat>)>>,
        /// Pids whose entry exists but cannot be read, which is a different
        /// answer from a pid that is not there.
        unreadable: RefCell<BTreeSet<i32>>,
        /// What every pidfd read answers, from the first. For the cases that
        /// are about the peer's state when the lookup STARTED.
        pidfd_always: RefCell<Option<Named>>,
        /// `(pid, answer)`: every pidfd read taken after this table has served
        /// a `stat` for `pid` answers `answer`.
        ///
        /// Keyed on a `/proc` read rather than on a count of pidfd reads, and
        /// that is the whole point of the knob. A count says "the second one"
        /// wherever the second one sits, so it cannot tell a read taken after
        /// the walk from one taken immediately after the first — which is the
        /// single-read version this design says is unsound. A review moved the
        /// second read up to just below the first, and up to just after the
        /// sweep, and moved the first read down past the sweep, and all three
        /// mutations passed the whole suite against a counted fake.
        pidfd_after_stat_of: RefCell<Option<(i32, Named)>>,
        /// Set once that `stat` has been served.
        pidfd_armed: Cell<bool>,
    }

    impl Table {
        fn with(rows: &[(i32, i32, u64)]) -> Self {
            let table = Table::default();
            for (pid, ppid, starttime) in rows {
                table.now.borrow_mut().insert(
                    *pid,
                    Stat {
                        ppid: *ppid,
                        starttime: *starttime,
                    },
                );
            }
            table
        }

        fn swap_after(&self, reads: usize, pid: i32, replacement: Option<Stat>) {
            *self.swap.borrow_mut() = Some((reads, pid, replacement));
        }

        /// Every pidfd read answers this, including the first.
        fn pidfd_always(&self, answer: Named) {
            *self.pidfd_always.borrow_mut() = Some(answer);
        }

        /// Every pidfd read taken AFTER a `stat` for `pid` answers `answer`.
        ///
        /// Pick a `pid` that only the stage being tested reads: the walk reads
        /// the chain, the sweep reads each live instance's stage-2 pid.
        fn pidfd_after_stat_of(&self, pid: i32, answer: Named) {
            *self.pidfd_after_stat_of.borrow_mut() = Some((pid, answer));
        }

        /// The pair the transport would have proved for `pid`: its number
        /// and the start time this table currently gives it.
        ///
        /// Deliberately NOT a `stat` call. Several tests arm `swap_after` on
        /// the read counter, and a helper that went through `Procfs` would
        /// shift every one of them by however many callers it has.
        fn caller(&self, pid: i32) -> Caller {
            let starttime = self
                .now
                .borrow()
                .get(&pid)
                .map(|stat| stat.starttime)
                .unwrap_or(0);
            Caller { pid, starttime }
        }

        /// The same NUMBER, a different process — what the allocator does
        /// when it wraps and hands a freed pid to the next caller of `fork`.
        fn replace(&self, pid: i32, stat: Stat) {
            self.now.borrow_mut().insert(pid, stat);
        }

        /// The process is there and the broker cannot read it — `EMFILE`, or
        /// a line that does not parse.
        fn hide(&self, pid: i32) {
            self.unreadable.borrow_mut().insert(pid);
        }
    }

    impl Procfs for Table {
        fn stat(&self, pid: i32) -> Reading {
            let mut reads = self.reads.borrow_mut();
            *reads += 1;
            let due = {
                let swap = self.swap.borrow();
                swap.filter(|(at, _, _)| *reads > *at)
            };
            if let Some((_, target, replacement)) = due {
                *self.swap.borrow_mut() = None;
                match replacement {
                    Some(stat) => self.now.borrow_mut().insert(target, stat),
                    None => self.now.borrow_mut().remove(&target),
                };
            }
            if let Some((target, _)) = *self.pidfd_after_stat_of.borrow() {
                if target == pid {
                    self.pidfd_armed.set(true);
                }
            }
            if self.unreadable.borrow().contains(&pid) {
                return Reading::Unreadable;
            }
            match self.now.borrow().get(&pid).copied() {
                Some(stat) => Reading::Of(stat),
                None => Reading::Gone,
            }
        }

        /// The fake's convention: a pidfd is NUMBERED with the pid it names,
        /// so a test that has nothing to say about pidfds passes the pid it
        /// always passed and gets the answer it always got.
        fn named_by(&self, pidfd: RawFd) -> Named {
            if let Some(answer) = *self.pidfd_always.borrow() {
                return answer;
            }
            if self.pidfd_armed.get() {
                if let Some((_, answer)) = *self.pidfd_after_stat_of.borrow() {
                    return answer;
                }
            }
            Named::Pid(pidfd)
        }
    }

    /// A registration that predeclares nothing and is granted nothing, which
    /// is what almost every test here wants: the fields it does not name are
    /// the ones it is not about.
    fn registration(instance: &str, app_id: &str) -> Registration {
        registration_with(instance, app_id, Vec::new(), Vec::new())
    }

    fn registration_with(
        instance: &str,
        app_id: &str,
        services: Vec<String>,
        owned: Vec<String>,
    ) -> Registration {
        Registration {
            instance: instance.to_string(),
            app_id: app_id.to_string(),
            services,
            owned,
        }
    }

    fn instance(name: &str, pid: i32, starttime: u64) -> Instance {
        Instance {
            instance: name.to_string(),
            app_id: format!("org.td.{name}"),
            pid,
            starttime,
            services: Vec::new(),
            owned: Vec::new(),
        }
    }

    /// A live instance whose permission file granted it `owned`.
    fn instance_owning(name: &str, pid: i32, starttime: u64, owned: &[&str]) -> Instance {
        Instance {
            owned: owned.iter().map(|name| (*name).to_string()).collect(),
            ..instance(name, pid, starttime)
        }
    }

    #[test]
    fn a_descendant_of_a_registered_stage_two_is_that_instance() {
        // 1 <- 100 (stage 2) <- 200 (the app) <- 300 (a child of the app)
        let table = Table::with(&[(1, 0, 1), (100, 1, 10), (200, 100, 20), (300, 200, 30)]);
        let live = [instance("one", 100, 10)];
        assert_eq!(
            resolve_against(&table, &live, &[], &[], 300),
            Identity::Jailed {
                app_id: "org.td.one".to_string(),
                instance: "one".to_string(),
                owned: Vec::new(),
            }
        );
        // And the stage-2 process itself resolves to its own instance.
        assert_eq!(
            resolve_against(&table, &live, &[], &[], 100),
            Identity::Jailed {
                app_id: "org.td.one".to_string(),
                instance: "one".to_string(),
                owned: Vec::new(),
            }
        );
    }

    #[test]
    fn a_process_descending_from_no_instance_is_unconfined() {
        let table = Table::with(&[(1, 0, 1), (100, 1, 10), (400, 1, 40)]);
        let live = [instance("one", 100, 10)];
        assert_eq!(resolve_against(&table, &live, &[], &[], 400), Identity::Unconfined);
        // An empty registry is the same claim about a smaller set, and it must
        // not accidentally be `Unknown` — the broker starts here, and every
        // peer on a bus with no jails is unconfined.
        assert_eq!(resolve_against(&table, &[], &[], &[], 400), Identity::Unconfined);
    }

    /// The finding §D was written against: the substitution the ENDPOINT check
    /// cannot see, caught while the walk is still in the middle of it.
    ///
    /// The walk starts at 300, whose parent is 200. After it has read 300 and
    /// before it reads 200, the real 200 exits and its pid is taken by a
    /// process that is a child of the registered stage-2 pid. The walk then
    /// climbs the impostor to 100 and lands on the instance by a path that
    /// never existed — and both endpoints are exactly what they claim to be,
    /// so re-reading 300 and 100 proves nothing.
    ///
    /// The impostor's start time is what gives it away, and it cannot be
    /// chosen: to take pid 200 it had to start after the original 200 exited,
    /// and the original was alive when 300 named it as parent, so the impostor
    /// is necessarily YOUNGER than 300. A draft of this test gave it a start
    /// time of 25 against a child of 30 and then reported that the check had
    /// failed to fire; the fixture was a history that cannot happen.
    #[test]
    fn an_intermediate_ancestor_replaced_mid_walk_is_not_a_lineage() {
        let live = [instance("one", 100, 10)];
        // Undisturbed, 300 is unconfined: its parent 200 is a child of pid 1.
        let table = Table::with(&[(1, 0, 1), (100, 1, 10), (200, 1, 20), (300, 200, 30)]);
        assert_eq!(resolve_against(&table, &live, &[], &[], 300), Identity::Unconfined);

        // Now let the walk read the impostor: swapped in after the first read
        // (300) and before the second (200).
        let table = Table::with(&[(1, 0, 1), (100, 1, 10), (200, 1, 20), (300, 200, 30)]);
        table.swap_after(1, 200, Some(Stat { ppid: 100, starttime: 35 }));
        match resolve_against(&table, &live, &[], &[], 300) {
            Identity::Unknown(why) => assert!(
                why.contains("was reused"),
                "the substitution must be named: {why}"
            ),
            other => panic!("a substituted ancestor resolved as {other:?}"),
        }
    }

    /// The other half of the same race: the ancestor exits and nothing takes
    /// its pid, so the re-read finds it simply gone.
    #[test]
    fn an_ancestor_that_leaves_during_the_walk_is_unknown() {
        let table = Table::with(&[(1, 0, 1), (200, 1, 20), (300, 200, 30)]);
        // Reads are 300, 200, 1 for the walk; drop 200 once the walk is past it.
        table.swap_after(3, 200, None);
        match resolve_against(&table, &[], &[], &[], 300) {
            Identity::Unknown(why) => assert!(why.contains("left during the walk"), "{why}"),
            other => panic!("a vanished ancestor resolved as {other:?}"),
        }
    }

    /// The invariant that catches what the re-read can miss: a parent cannot
    /// be younger than its own child.
    #[test]
    fn a_parent_younger_than_its_child_is_a_reused_pid() {
        // 300's parent is 200, but 200 started AFTER 300 — impossible for a
        // real parent, and exactly what a reused pid looks like once it has
        // settled and both reads agree about it.
        let table = Table::with(&[(1, 0, 1), (100, 1, 10), (200, 100, 90), (300, 200, 30)]);
        let live = [instance("one", 100, 10)];
        match resolve_against(&table, &live, &[], &[], 300) {
            Identity::Unknown(why) => assert!(
                why.contains("was reused"),
                "the reuse must be named: {why}"
            ),
            other => panic!("a parent younger than its child resolved as {other:?}"),
        }
    }

    /// `Unconfined` is a claim about a COMPLETE registry, so an instance the
    /// broker can no longer find makes it unsayable for everybody.
    #[test]
    fn an_instance_whose_stage_two_is_gone_makes_unconfined_unsayable() {
        let table = Table::with(&[(1, 0, 1), (400, 1, 40)]);
        let live = [instance("one", 100, 10)];
        match resolve_against(&table, &live, &[], &[], 400) {
            Identity::Unknown(why) => assert!(why.contains("that process is gone"), "{why}"),
            other => panic!("a stale registry still answered {other:?}"),
        }
        // A pid that came BACK as a different process is the same ambiguity:
        // the registry's start time is what distinguishes them.
        let table = Table::with(&[(1, 0, 1), (100, 1, 77), (400, 1, 40)]);
        match resolve_against(&table, &live, &[], &[], 400) {
            Identity::Unknown(why) => assert!(why.contains("that process is gone"), "{why}"),
            other => panic!("a reused instance pid still answered {other:?}"),
        }
    }

    /// A peer the walk cannot read denies, whichever way it cannot read it.
    ///
    /// The walk has no use for the distinction between "gone" and "could not
    /// be read" — a lineage it cannot read is one it cannot vouch for either
    /// way — but the reasons are named separately, because the registry's
    /// bookkeeping does care and a shared message would hide which happened.
    #[test]
    fn a_peer_whose_own_proc_entry_is_unreadable_is_unknown() {
        let table = Table::with(&[(1, 0, 1), (600, 1, 60)]);
        match resolve_against(&table, &[], &[], &[], 500) {
            Identity::Unknown(why) => assert!(why.contains("does not exist"), "{why}"),
            other => panic!("a peer that is not there resolved as {other:?}"),
        }
        table.hide(600);
        match resolve_against(&table, &[], &[], &[], 600) {
            Identity::Unknown(why) => assert!(why.contains("no readable /proc entry"), "{why}"),
            other => panic!("an unreadable peer resolved as {other:?}"),
        }
        assert!(matches!(
            resolve_against(&table, &[], &[], &[], 0),
            Identity::Unknown(_)
        ));
    }

    #[test]
    fn a_lineage_that_loops_is_refused_rather_than_walked_for_ever() {
        let table = Table::with(&[(10, 20, 1), (20, 10, 1)]);
        match resolve_against(&table, &[], &[], &[], 10) {
            Identity::Unknown(why) => assert!(why.contains("repeats pid"), "{why}"),
            other => panic!("a cyclic /proc resolved as {other:?}"),
        }
    }

    /// `comm` is attacker-controlled and may contain spaces AND parentheses,
    /// so the parse splits at the last `)` rather than tokenizing the line.
    #[test]
    fn stat_is_parsed_past_a_comm_that_looks_like_fields() {
        let hostile = "42 (evil) 1 999999) S 7 42 42 0 -1 4194304 0 0 0 0 0 0 0 0 20 0 1 0 \
                       4242 0 0 18446744073709551615 0 0 0 0 0 0 0 0 0 0 0 0 17 0 0 0 0";
        let parsed = parse_stat(hostile.as_bytes());
        assert_eq!(
            parsed,
            Some(Stat {
                ppid: 7,
                starttime: 4242
            }),
            "a comm containing ') 1 999999' forged its own ppid"
        );

        let ordinary = "1 (td-init) S 0 1 1 0 -1 4194560 100 0 0 0 1 2 0 0 20 0 1 0 \
                        7 0 0 18446744073709551615 0 0 0 0 0 0 0 0 0 0 0 0 17 0 0 0 0";
        assert_eq!(
            parse_stat(ordinary.as_bytes()),
            Some(Stat {
                ppid: 0,
                starttime: 7
            })
        );

        assert_eq!(parse_stat(b"nonsense with no paren"), None);
        assert_eq!(parse_stat(b"1 (short) S 0"), None);

    }

    /// A `comm` that is not UTF-8 must not make a process unreadable.
    ///
    /// Any process can set its own name to arbitrary bytes, and the broker
    /// reads "unreadable" as "gone" — which reaps a live instance and stops
    /// its descendants being recognised. The check has to go through the file
    /// read rather than through `parse_stat` alone, because the read is where
    /// the conversion would happen.
    #[test]
    fn a_stat_file_whose_comm_is_not_utf8_still_parses() {
        let mut raw = b"42 (ev".to_vec();
        raw.push(0x80);
        raw.extend_from_slice(
            b"l) S 7 42 42 0 -1 4194304 0 0 0 0 0 0 0 0 20 0 1 0 \
              4242 0 0 18446744073709551615 0 0 0 0 0 0 0 0 0 0 0 0 17 0 0 0 0",
        );
        let mut path = std::env::temp_dir();
        path.push(format!("td-busd-stat-{}", std::process::id()));
        if std::fs::write(&path, &raw).is_err() {
            return;
        }
        let parsed = stat_of(&path);
        let _ = std::fs::remove_file(&path);
        assert_eq!(
            parsed,
            Reading::Of(Stat {
                ppid: 7,
                starttime: 4242
            }),
            "a comm that is not UTF-8 made a live process unreadable"
        );
        // And a path that is not there is GONE rather than merely unreadable,
        // which is the distinction the reap rests on.
        assert_eq!(stat_of(&path), Reading::Gone);
    }

    /// An instance the broker could not READ is not an instance it has been
    /// shown is gone.
    ///
    /// `fs::read` fails for reasons that have nothing to do with the process —
    /// `EMFILE` and `ENOMEM` among them — and a draft treated every failure as
    /// death. The connection that observed it was refused, which looks safe,
    /// but the record was dropped: the NEXT connection from inside that jail
    /// walked a registry with no record of it and resolved `Unconfined`. No
    /// attacker is required, only a busy machine.
    #[test]
    fn an_instance_that_cannot_be_read_is_refused_rather_than_reaped() {
        let table = Table::with(&[(1, 0, 1), (900, 1, 90), (100, 900, 95), (400, 1, 40)]);
        let instances = Instances::new();
        let token = instances
            .open(&table, registration("one", "org.td.One"), 1000, table.caller(900))
            .expect("phase one opens");
        instances
            .complete(&table, &token, 100, 1000, table.caller(900))
            .expect("phase two completes");

        table.hide(100);
        match instances.resolve(&table, 400) {
            Identity::Unknown(why) => assert!(why.contains("could not be read"), "{why}"),
            other => panic!("an unreadable instance resolved as {other:?}"),
        }
        assert_eq!(
            instances.live_count(),
            1,
            "a read the broker fumbled dropped a live instance"
        );
    }

    /// The FIRST of the two edge invariants: a hop whose start time changes
    /// between the walk's read and the re-read is not the process the walk
    /// passed.
    ///
    /// A review found this untested, and untested here means something
    /// specific: deleting the whole re-read loop went red through the
    /// "left during the walk" case, and the mid-walk substitution test staged
    /// its impostor early enough that both reads agreed and the ORDERING check
    /// killed it. Nothing staged a hop that changed BETWEEN the two reads, so
    /// weakening the arm to `Some(_) => {}` passed everything.
    #[test]
    fn an_ancestor_replaced_between_the_two_reads_is_unknown() {
        let table = Table::with(&[(1, 0, 1), (100, 1, 10), (200, 100, 20), (300, 200, 30)]);
        let live = [instance("one", 100, 10)];
        // The walk reads 300, 200, 100 and stops at the match, so the swap
        // lands after read three: the walk saw 200 at start time 20 and the
        // re-read sees 25. And 25 is younger than 200's own recorded 20 while
        // still older than its child's 30, so the ordering check cannot fire
        // and only the re-read can.
        table.swap_after(
            3,
            200,
            Some(Stat {
                ppid: 100,
                starttime: 25,
            }),
        );
        match resolve_against(&table, &live, &[], &[], 300) {
            Identity::Unknown(why) => assert!(why.contains("was replaced"), "{why}"),
            other => panic!("a hop replaced between the two reads resolved as {other:?}"),
        }
    }

    /// A registered pid is a match only if its START TIME matches too.
    ///
    /// The pid alone is a number the kernel reissues. Without the start time
    /// the walk hands a recycled pid the previous tenant's app id, which is a
    /// false `Jailed` — the same failure class the ordering invariant exists
    /// to prevent, one step earlier.
    #[test]
    fn a_registered_pid_whose_start_time_differs_is_not_that_instance() {
        // 100 is registered at start time 10 and `/proc` says 77: the number
        // came back, the process did not.
        let table = Table::with(&[(1, 0, 1), (100, 1, 77)]);
        let live = [instance("one", 100, 10)];
        match resolve_against(&table, &live, &[], &[], 100) {
            Identity::Unknown(why) => assert!(why.contains("that process is gone"), "{why}"),
            other => panic!("a recycled instance pid resolved as {other:?}"),
        }
    }

    /// One pid may stand for one instance.
    ///
    /// Without this two instances can claim the same stage-2 pid and the
    /// walk's `find` silently picks whichever was registered first — an
    /// attribution decided by registration order rather than by the process
    /// tree.
    #[test]
    fn one_pid_binds_to_one_instance() {
        let table = Table::with(&[(1, 0, 1), (900, 1, 90), (100, 900, 95)]);
        let instances = Instances::new();
        let first = instances
            .open(&table, registration("one", "org.td.One"), 1000, table.caller(900))
            .expect("phase one opens");
        let second = instances
            .open(&table, registration("two", "org.td.Two"), 1000, table.caller(900))
            .expect("a second instance opens");
        instances
            .complete(&table, &first, 100, 1000, table.caller(900))
            .expect("the first binding takes");
        let clash = instances
            .complete(&table, &second, 100, 1000, table.caller(900))
            .expect_err("two instances bound the same pid");
        assert!(clash.contains("already bound"), "{clash}");
    }

    /// The walk is bounded. A `/proc` deep enough to exhaust it is refused
    /// rather than walked, and the cycle guard does not cover this: a very
    /// long chain repeats nothing.
    #[test]
    fn a_lineage_deeper_than_the_ceiling_is_refused() {
        let rows: Vec<(i32, i32, u64)> = (1..=(MAX_DEPTH as i32 + 8))
            .map(|pid| (pid, pid - 1, pid as u64))
            .collect();
        let table = Table::with(&rows);
        let deepest = MAX_DEPTH as i32 + 8;
        match resolve_against(&table, &[], &[], &[], deepest) {
            Identity::Unknown(why) => assert!(why.contains("deeper than"), "{why}"),
            other => panic!("an unbounded lineage resolved as {other:?}"),
        }
    }

    /// Reaping is keyed on the PROCESS — name, pid and start time together —
    /// not on the name.
    ///
    /// The decision to reap is taken outside the lock and applied under it, so
    /// an instance that registered again under the same name in between must
    /// survive a decision that was taken about its predecessor. Keying on the
    /// name alone would drop the live one, and its descendants would resolve
    /// `Unconfined`.
    #[test]
    fn reaping_is_keyed_on_the_process_rather_than_the_name() {
        let table = Table::with(&[(1, 0, 1), (900, 1, 90), (100, 900, 95)]);
        let instances = Instances::new();
        let token = instances
            .open(&table, registration("one", "org.td.One"), 1000, table.caller(900))
            .expect("phase one opens");
        instances
            .complete(&table, &token, 100, 1000, table.caller(900))
            .expect("phase two completes");
        // Decisions taken about something that is not this record. Each leg
        // differs from the live record in exactly ONE of the three fields, so
        // dropping any one of them from the key fails here rather than
        // needing all three to be wrong at once.
        for stale in [
            ("one".to_string(), 100, 10),  // same name and pid, older process
            ("one".to_string(), 700, 95),  // same name and start time, other pid
            ("two".to_string(), 100, 95),  // same pid and start time, other name
        ] {
            instances.reap(std::slice::from_ref(&stale));
            assert_eq!(
                instances.live_count(),
                1,
                "a reap decision about {stale:?} dropped a different instance"
            );
        }
        instances.reap(&[("one".to_string(), 100, 95)]);
        assert_eq!(instances.live_count(), 0, "the matching record survived");
    }

    /// A registrant may bind its own CHILD and nothing else.
    ///
    /// Without this any session peer could open a registration and complete it
    /// with any pid it can read. Completing with pid 1 is the sharp version:
    /// every later connection in the session walks through pid 1, so every one
    /// of them would land in the attacker's instance and be handed its app id.
    #[test]
    fn phase_two_may_bind_only_the_registrants_own_child() {
        // 700 is a perfectly readable process that 900 did not spawn.
        let table = Table::with(&[(1, 0, 1), (900, 1, 90), (100, 900, 95), (700, 1, 70)]);
        let instances = Instances::new();
        let token = instances
            .open(&table, registration("one", "org.td.One"), 1000, table.caller(900))
            .expect("phase one opens");

        for stranger in [1, 700] {
            let error = instances
                .complete(&table, &token, stranger, 1000, table.caller(900))
                .expect_err("a stranger's pid completed the registration");
            assert!(error.contains("not a child"), "{error}");
        }
        // The token survives every refusal, or a failed attempt would be a way
        // to deny somebody else's launch.
        instances
            .complete(&table, &token, 100, 1000, table.caller(900))
            .expect("the registrant's own child completes");
    }

    /// And phase two must come from the connection that opened phase one.
    ///
    /// The token is unguessable, but "unguessable" is a probability and this
    /// is a check. It is also what makes the child rule mean anything: the
    /// parent it compares against is the caller's own process.
    #[test]
    fn phase_two_must_come_from_the_connection_that_opened_it() {
        let table = Table::with(&[(1, 0, 1), (900, 1, 90), (100, 900, 95), (901, 1, 91)]);
        let instances = Instances::new();
        let token = instances
            .open(&table, registration("one", "org.td.One"), 1000, table.caller(900))
            .expect("phase one opens");
        let error = instances
            .complete(&table, &token, 100, 1000, table.caller(901))
            .expect_err("another connection completed the registration");
        assert!(error.contains("completed by pid 901"), "{error}");
    }

    /// The registrant's pid was handed on between the two phases, and the
    /// process wearing it now completes nothing.
    ///
    /// This is the case a bare number cannot see. `Register` and `Complete`
    /// arrive on DIFFERENT connections — td-jail closes every descriptor above
    /// stderr between them — so phase two's caller is compared against a
    /// number the registry wrote down, and a number is precisely what the
    /// allocator recycles. 900 opens, 900 ends, the allocator wraps, and 900
    /// is somebody else who calls `Complete` with a child of its own. Every
    /// other check passes: the uid matches, the token is live and unguessed,
    /// and the pid being bound really is the caller's own child. Only the
    /// start time says these are two processes.
    #[test]
    fn a_registrants_recycled_pid_completes_nothing() {
        let table = Table::with(&[(1, 0, 1), (900, 1, 90), (100, 900, 95)]);
        let instances = Instances::new();
        let token = instances
            .open(&table, registration("one", "org.td.One"), 1000, table.caller(900))
            .expect("phase one opens");

        table.replace(900, Stat { ppid: 1, starttime: 91 });
        let error = instances
            .complete(&table, &token, 100, 1000, table.caller(900))
            .expect_err("a stranger wearing the registrant's pid completed it");
        assert!(error.contains("now belongs to something else"), "{error}");

        // The registration is not consumed by the refusal. It belongs to a
        // process that has ended, so it expires on its own clock rather than
        // being burned by somebody else's attempt — the same rule every other
        // refusal here follows.
        assert_eq!(instances.pending_count(), 1);
        assert_eq!(instances.live_count(), 0);
    }

    /// A DIFFERENT process that started in the same clock tick completes
    /// nothing either.
    ///
    /// The cross-product case, and it exists because a reviewer's mutation
    /// joined the two refusals with `&&`: the pid check and the start-time
    /// check both passed over a caller that differs in exactly one field,
    /// because every test at the time differed in both. 901 here has 900's
    /// start time to the tick, which is ordinary — `starttime` is measured in
    /// clock ticks since boot and two processes forked together share one.
    #[test]
    fn a_different_process_of_the_same_age_completes_nothing() {
        let table = Table::with(&[(1, 0, 1), (900, 1, 90), (100, 900, 95), (901, 1, 90)]);
        let instances = Instances::new();
        let token = instances
            .open(&table, registration("one", "org.td.One"), 1000, table.caller(900))
            .expect("phase one opens");
        let error = instances
            .complete(&table, &token, 100, 1000, table.caller(901))
            .expect_err("a same-aged stranger completed the registration");
        assert!(error.contains("completed by pid 901"), "{error}");
        assert_eq!(instances.pending_count(), 1, "the token was consumed");
    }

    /// And the start-time comparison refuses in BOTH directions.
    ///
    /// A recycled pid necessarily starts later, so `<` passes every test `!=`
    /// passes and a reviewer's mutation to it survived. The rule is "the same
    /// process", not "no younger than", and a clock that is not monotonic
    /// across a `/proc` read — or a `starttime` this broker reads from a
    /// namespace it did not expect — should refuse rather than accept.
    #[test]
    fn a_start_time_that_differs_either_way_completes_nothing() {
        for theirs in [89, 91] {
            let table = Table::with(&[(1, 0, 1), (900, 1, 90), (100, 900, 95)]);
            let instances = Instances::new();
            let token = instances
                .open(&table, registration("one", "org.td.One"), 1000, table.caller(900))
                .expect("phase one opens");
            table.replace(900, Stat { ppid: 1, starttime: theirs });
            let error = instances
                .complete(&table, &token, 100, 1000, table.caller(900))
                .expect_err("a process of another age completed the registration");
            assert!(error.contains("now belongs to something else"), "{error}");
        }
    }

    /// An expired token completes nothing, and does not wait for a connection
    /// to happen along and sweep it.
    #[test]
    fn an_expired_token_completes_nothing() {
        let table = Table::with(&[(1, 0, 1), (900, 1, 90), (100, 900, 95)]);
        let instances = Instances::new();
        let token = instances
            .open(&table, registration("one", "org.td.One"), 1000, table.caller(900))
            .expect("phase one opens");
        let Some(later) = Instant::now()
            .checked_add(PENDING_LIFETIME)
            .and_then(|t| t.checked_add(Duration::from_secs(1)))
        else {
            return;
        };
        let error = instances
            .complete_at(&table, &token, 100, 1000, table.caller(900), later)
            .expect_err("an expired registration completed");
        assert!(error.contains("no registration is open"), "{error}");
        assert_eq!(instances.live_count(), 0);
    }

    /// The token is a secret, not a serial number.
    ///
    /// A draft used a counter and the instance name, which a peer can derive:
    /// its own registrations tell it the counter and `open`'s two distinct
    /// refusals are a name oracle. Guessing one lets an attacker CONSUME
    /// somebody else's in-flight registration, after which the real stage 1
    /// fails — with stage 2 already spawned and no record of it.
    #[test]
    fn tokens_are_unguessable_rather_than_serial() {
        // The registrant IS looked up now — phase one records when it
        // started, so that phase two can tell the same process from a later
        // one wearing its pid. Nothing else here is.
        let table = Table::with(&[(900, 1, 90)]);
        let instances = Instances::new();
        let first = instances
            .open(&table, registration("one", "org.td.One"), 1000, table.caller(900))
            .expect("phase one opens");
        let second = instances
            .open(&table, registration("two", "org.td.One"), 1000, table.caller(900))
            .expect("a second registration opens");
        assert_ne!(first, second);
        for token in [&first, &second] {
            assert_eq!(token.len(), 32, "{token} is not 128 bits of hex");
            assert!(
                token.bytes().all(|byte| byte.is_ascii_hexdigit()),
                "{token} is not hex"
            );
        }
        assert!(!first.contains("one") && !second.contains("two"));

        // The assertions above are all satisfied by a zero-padded counter,
        // which a review demonstrated by writing one. Two things separate a
        // secret from a serial and neither is shape. Consecutive counter
        // values share every character but the last few, so the FIRST HALVES
        // must differ...
        assert_ne!(
            first.get(..16),
            second.get(..16),
            "consecutive tokens share a prefix, which is what a counter does"
        );
        // ...and a counter restarts with the registry, so two fresh registries
        // would hand out the same first token for the same instance name.
        let elsewhere = Instances::new();
        let same_name = elsewhere
            .open(&table, registration("one", "org.td.One"), 1000, table.caller(900))
            .expect("a second registry opens");
        assert_ne!(
            first, same_name,
            "two registries issued the same token, which is what a counter does"
        );
    }

    /// The parse runs against the real kernel too, because a hand-written
    /// fixture only proves the parser agrees with the author's reading of
    /// `proc(5)`.
    #[test]
    fn the_real_proc_entry_for_this_process_parses() {
        let me = std::process::id();
        let Ok(pid) = i32::try_from(me) else {
            return;
        };
        let Reading::Of(stat) = RealProcfs.stat(pid) else {
            panic!("this process has no readable /proc entry");
        };
        assert!(stat.ppid > 0, "a test process has a parent");
        assert!(stat.starttime > 0, "a running process started at some point");
    }

    /// `fdinfo`, the four things it can say, against files rather than the
    /// kernel. The live half of this is in `transport`, which is the one
    /// module allowed to take a pidfd.
    #[test]
    fn a_pidfd_names_its_process_until_that_process_is_reaped() {
        let mut path = std::env::temp_dir();
        path.push(format!("td-busd-fdinfo-{}", std::process::id()));
        let write = |body: &str| std::fs::write(&path, body).is_ok();

        // A live pidfd, laid out the way the kernel lays one out. `NSpid` is
        // the line most likely to be mistaken for the answer: it is the pid
        // inside the process's OWN namespace, which for a jailed peer is 1,
        // and reading it would make every jail look like init.
        if !write("pos:\t0\nflags:\t02000002\nmnt_id:\t16\nino:\t9\nPid:\t4242\nNSpid:\t1\n") {
            return;
        }
        assert_eq!(named_by(&path), Named::Pid(4242));

        // Reaped, and `-1` is the only thing that licenses that answer.
        assert!(write("pos:\t0\nPid:\t-1\nNSpid:\t-1\n"));
        assert_eq!(named_by(&path), Named::Reaped);

        // Neither. An `fdinfo` with no `Pid:` line is what EVERY descriptor
        // that is not a pidfd looks like, and it must not read as a process.
        assert!(write("pos:\t0\nflags:\t02000002\nmnt_id:\t16\nino:\t9\n"));
        assert_eq!(named_by(&path), Named::Unreadable);

        // A `Pid:` that is not a number, and one that is a number no process
        // has. Zero is what `SO_PEERCRED` answers for a peer outside the
        // reader's namespace, so it is a value this code sees in practice.
        assert!(write("Pid:\tnonsense\n"));
        assert_eq!(named_by(&path), Named::Unreadable);
        assert!(write("Pid:\t0\n"));
        assert_eq!(named_by(&path), Named::Unreadable);

        // TWO `Pid:` lines is a file this parser does not understand, and
        // taking the first would read `400` and ignore the `-1` under it.
        assert!(write("Pid:\t400\nNSpid:\t400\nPid:\t-1\n"));
        assert_eq!(named_by(&path), Named::Unreadable);

        let _ = std::fs::remove_file(&path);
        // A descriptor whose `fdinfo` cannot be read says NOTHING about the
        // peer, and in particular does not say it was reaped — which would
        // turn one `EMFILE` into a denial that reads like an attack.
        assert_eq!(named_by(&path), Named::Unreadable);
    }

    /// And against the live kernel, for the half a fixture cannot prove: an
    /// ordinary descriptor's real `fdinfo` has no `Pid:` line in it.
    #[test]
    fn a_descriptor_that_is_not_a_pidfd_names_no_process() {
        let Ok(file) = std::fs::File::open("/proc/self/stat") else {
            return;
        };
        assert_eq!(
            RealProcfs.named_by(file.as_raw_fd()),
            Named::Unreadable,
            "a plain file was read as a process"
        );
        assert_eq!(RealProcfs.named_by(RawFd::MAX), Named::Unreadable);
    }

    /// A peer reaped before the broker ever looked. `SO_PEERCRED` cannot tell
    /// this from a live peer — the number it reports may belong to somebody
    /// else by now — and the pidfd can.
    #[test]
    fn a_peer_that_was_already_reaped_is_not_unconfined() {
        let table = Table::with(&[(1, 0, 1), (400, 1, 40)]);
        table.pidfd_always(Named::Reaped);
        let instances = Instances::new();
        match instances.resolve(&table, 400) {
            Identity::Unknown(why) => assert!(why.contains("reaped"), "{why}"),
            other => panic!("a reaped peer resolved {other:?}"),
        }
    }

    /// A pidfd the broker could not read is an identity it has not
    /// established, which denies. The `/proc` entries are all present and
    /// perfectly readable, so nothing else in the walk objects.
    #[test]
    fn a_peer_whose_pidfd_cannot_be_read_is_not_unconfined() {
        let table = Table::with(&[(1, 0, 1), (400, 1, 40)]);
        table.pidfd_always(Named::Unreadable);
        let instances = Instances::new();
        match instances.resolve(&table, 400) {
            Identity::Unknown(why) => assert!(why.contains("pidfd"), "{why}"),
            other => panic!("an unreadable pidfd resolved {other:?}"),
        }
    }

    /// The read the whole argument rests on: a peer reaped DURING the lookup.
    ///
    /// Everything the walk itself read stays self-consistent — this table
    /// never changes — so the start-time invariants see nothing wrong, which
    /// is exactly the case a single pidfd read at the start would miss. The
    /// peer is reaped, its number goes back to the allocator, and every
    /// `/proc` read this lookup took becomes a read of whatever holds that
    /// number next. Only the SECOND pidfd read catches it.
    #[test]
    fn a_peer_reaped_during_the_lookup_is_refused_though_the_walk_agreed() {
        // Same rows, same walk: without the reap this is `Unconfined`.
        let clean = Table::with(&[(1, 0, 1), (400, 1, 40)]);
        assert_eq!(Instances::new().resolve(&clean, 400), Identity::Unconfined);

        let table = Table::with(&[(1, 0, 1), (400, 1, 40)]);
        // Pid 1 is the top of the chain and nothing but the WALK reads it, so
        // the reap lands strictly inside the bracket: the first pidfd read
        // hands back 400, the walk runs, and only the read after it says the
        // process has gone. A second read taken any earlier still sees 400,
        // which is what makes this test fail for the single-read version
        // rather than pass for it.
        table.pidfd_after_stat_of(1, Named::Reaped);
        match Instances::new().resolve(&table, 400) {
            Identity::Unknown(why) => assert!(why.contains("did not survive this lookup"), "{why}"),
            other => panic!("a peer reaped mid-lookup resolved {other:?}"),
        }
    }

    /// And a `Jailed` answer is re-checked too. Reaching one through a
    /// recycled pid attributes one application's connection to another, which
    /// is a smaller move than reaching `Unconfined` and is still a wrong one —
    /// so the check is not gated on which answer the walk produced.
    #[test]
    fn a_jailed_answer_is_re_checked_against_the_pidfd_as_well() {
        let rows = &[(1, 0, 1), (900, 1, 90), (400, 900, 95), (401, 400, 96)];
        let jailed = Identity::Jailed {
            app_id: "org.td.One".to_string(),
            instance: "one".to_string(),
            owned: Vec::new(),
        };

        let clean = Table::with(rows);
        let instances = Instances::new();
        let token = instances
            .open(&clean, registration("one", "org.td.One"), 1000, clean.caller(900))
            .expect("phase one opens");
        instances
            .complete(&clean, &token, 400, 1000, clean.caller(900))
            .expect("phase two completes");
        assert_eq!(instances.resolve(&clean, 401), jailed);

        // The identical registry and the identical walk, with the peer reaped
        // underneath it.
        let table = Table::with(rows);
        let instances = Instances::new();
        let token = instances
            .open(&table, registration("one", "org.td.One"), 1000, table.caller(900))
            .expect("phase one opens");
        instances
            .complete(&table, &token, 400, 1000, table.caller(900))
            .expect("phase two completes");
        // 401's own chain tops out at the registered instance, so pid 1 is
        // never read here; 400 is, by the walk's first hop AND by the sweep.
        // The sweep half is `the_bracket_opens_before_the_registry_sweep`.
        table.pidfd_after_stat_of(401, Named::Reaped);
        match instances.resolve(&table, 401) {
            Identity::Unknown(why) => assert!(why.contains("did not survive this lookup"), "{why}"),
            other => panic!("a jailed answer survived a mid-lookup reap: {other:?}"),
        }
    }

    /// The bracket opens before the registry SWEEP, not just before the walk.
    ///
    /// `sweep_live` reads `/proc` for every registered instance and drops
    /// records on what it finds — and a dropped record changes the answer,
    /// through `reaped`. Those reads are attributable to this peer for exactly
    /// the reason the walk's are: the peer had not been reaped while they
    /// happened. So the first pidfd read has to come before them.
    ///
    /// The reap is armed on the live instance's own stage-2 pid, which the
    /// SWEEP reads before the walk reaches it. If the first read has been
    /// moved down past the sweep it is already armed when it happens, and the
    /// refusal changes from "did not survive this lookup" to "reaped before it
    /// could be identified" — which is why this asserts the exact reason
    /// rather than merely `Unknown`. A review moved that read and the whole
    /// suite stayed green.
    #[test]
    fn the_bracket_opens_before_the_registry_sweep() {
        let table = Table::with(&[(1, 0, 1), (900, 1, 90), (400, 900, 95), (401, 400, 96)]);
        let instances = Instances::new();
        let token = instances
            .open(&table, registration("one", "org.td.One"), 1000, table.caller(900))
            .expect("phase one opens");
        instances
            .complete(&table, &token, 400, 1000, table.caller(900))
            .expect("phase two completes");
        table.pidfd_after_stat_of(400, Named::Reaped);
        match instances.resolve(&table, 401) {
            Identity::Unknown(why) => assert!(
                why.contains("did not survive this lookup"),
                "the peer read as already reaped, so the bracket opened after \
                 the sweep rather than before it: {why}"
            ),
            other => panic!("a peer reaped during the sweep resolved {other:?}"),
        }
    }

    /// The second read compares the PID, and not merely that some pid came
    /// back.
    ///
    /// Today a pidfd can only go from naming its process to naming nothing, so
    /// the comparison is belt-and-braces — and it is pinned rather than left
    /// to a comment, because "the kernel cannot answer that" is exactly the
    /// kind of claim that stops being true one release later.
    #[test]
    fn the_second_pidfd_read_must_name_the_same_process() {
        let table = Table::with(&[(1, 0, 1), (400, 1, 40), (401, 1, 41)]);
        table.pidfd_after_stat_of(1, Named::Pid(401));
        match Instances::new().resolve(&table, 400) {
            Identity::Unknown(why) => {
                assert!(why.contains("did not survive this lookup"), "{why}")
            }
            other => panic!("a pidfd that changed process resolved {other:?}"),
        }
    }

    /// The pid the walk starts from comes from the PIDFD, not from anything
    /// the caller passed alongside it. Under the fake's numbering convention
    /// the two are the same, so this test breaks the convention: the fd is
    /// numbered 7 and names pid 401, and the answer must be 401's.
    #[test]
    fn the_walk_starts_from_the_pid_the_pidfd_names() {
        let table = Table::with(&[(1, 0, 1), (900, 1, 90), (400, 900, 95), (401, 400, 96)]);
        table.pidfd_always(Named::Pid(401));
        let instances = Instances::new();
        let token = instances
            .open(&table, registration("one", "org.td.One"), 1000, table.caller(900))
            .expect("phase one opens");
        instances
            .complete(&table, &token, 400, 1000, table.caller(900))
            .expect("phase two completes");
        // 7 is not in the table at all, so a walk that started from the
        // descriptor number would answer `Unknown`.
        assert_eq!(
            instances.resolve(&table, 7),
            Identity::Jailed {
                app_id: "org.td.One".to_string(),
                instance: "one".to_string(),
                owned: Vec::new(),
            }
        );
    }

    #[test]
    fn registration_is_two_phase_and_its_token_is_one_shot() {
        // 900 registers and 100 is its child, which is the shape a real
        // stage 0 and its stage 2 have. 110 is a second child of 900, for the
        // one-shot leg below.
        let table = Table::with(&[(1, 0, 1), (900, 1, 90), (100, 900, 95), (110, 900, 96)]);
        let instances = Instances::new();
        let token = instances
            .open(
                &table,
                registration_with(
                    "one",
                    "org.td.One",
                    vec!["ca.desrt.dconf".to_string()],
                    Vec::new(),
                ),
                1000,
                table.caller(900),
            )
            .expect("phase one opens");

        // Between the phases the instance is not resolvable, and the peer
        // fails CLOSED rather than being queued or called unconfined.
        assert!(matches!(
            instances.resolve(&table, 100),
            Identity::Unknown(_)
        ));
        assert_eq!(instances.live_count(), 0);

        instances
            .complete(&table, &token, 100, 1000, table.caller(900))
            .expect("phase two completes");
        assert_eq!(instances.live_count(), 1);
        assert_eq!(
            instances.resolve(&table, 100),
            Identity::Jailed {
                app_id: "org.td.One".to_string(),
                instance: "one".to_string(),
                owned: Vec::new(),
            }
        );

        // One shot: the TOKEN is consumed. A draft re-completed with the same
        // pid, which the "already bound to an instance" rule refuses on its
        // own — so deleting the consumption changed nothing the test could
        // see. A second CHILD is what separates them: under a non-consuming
        // token it binds a second live instance under one instance name, each
        // burning a `MAX_INSTANCES` slot, where a duplicate `Register` would
        // have been refused outright.
        let again = instances.complete(&table, &token, 110, 1000, table.caller(900));
        assert!(again.is_err(), "a consumed token bound a second pid");
        assert_eq!(instances.live_count(), 1, "one token bound two instances");
    }

    #[test]
    fn phase_two_must_come_from_the_uid_that_opened_phase_one() {
        let table = Table::with(&[(1, 0, 1), (900, 1, 90), (100, 900, 95)]);
        let instances = Instances::new();
        let token = instances
            .open(&table, registration("one", "org.td.One"), 1000, table.caller(900))
            .expect("phase one opens");
        let wrong = instances.complete(&table, &token, 100, 1001, table.caller(900));
        assert!(wrong.is_err(), "another uid completed the registration");
        // And the token survives a refused attempt rather than being burned by
        // it, or anyone could deny a launch by guessing at it once.
        assert!(instances.complete(&table, &token, 100, 1000, table.caller(900)).is_ok());
    }

    #[test]
    fn a_pid_that_is_already_gone_completes_nothing() {
        let table = Table::with(&[(1, 0, 1), (900, 1, 90), (100, 900, 95)]);
        let instances = Instances::new();
        let token = instances
            .open(&table, registration("one", "org.td.One"), 1000, table.caller(900))
            .expect("phase one opens");
        let error = instances
            .complete(&table, &token, 700, 1000, table.caller(900))
            .expect_err("a pid with no /proc entry must not complete");
        assert!(error.contains("does not exist"), "{error}");

        // And a pid the broker merely could not READ does not complete
        // either: the record's start time would have to be invented.
        table.hide(100);
        let error = instances
            .complete(&table, &token, 100, 1000, table.caller(900))
            .expect_err("an unreadable pid must not complete");
        assert!(error.contains("no readable /proc entry"), "{error}");
    }

    #[test]
    fn the_registry_is_bounded_and_refuses_a_repeated_instance() {
        // The registrant IS looked up now — phase one records when it
        // started, so that phase two can tell the same process from a later
        // one wearing its pid. Nothing else here is.
        let table = Table::with(&[(900, 1, 90)]);
        let instances = Instances::new();
        for which in 0..MAX_INSTANCES {
            instances
                .open(
                    &table,
                    registration(&format!("i{which}"), "org.td.One"),
                    1000,
                    table.caller(900),
                )
                .expect("under the ceiling");
        }
        assert!(instances
            .open(
                &table,
                registration("more", "org.td.One"),
                1000,
                table.caller(900)
            )
            .is_err());

        let fresh = Instances::new();
        fresh
            .open(&table, registration("one", "org.td.One"), 1000, table.caller(900))
            .expect("first");
        assert!(
            fresh
                .open(
                    &table,
                    registration("one", "org.td.Other"),
                    1000,
                    table.caller(900)
                )
                .is_err(),
            "a second registration took over a live instance name"
        );
        // A fresh name, or this leg never reaches the service ceiling: it
        // would be refused for the duplicate instance name instead, and the
        // ceiling could be deleted without failing anything.
        assert!(fresh
            .open(
                &table,
                registration_with(
                    "two",
                    "org.td.One",
                    vec![String::new(); MAX_SERVICES + 1],
                    Vec::new(),
                ),
                1000,
                table.caller(900),
            )
            .is_err());
        // The grant list has its own ceiling, and a fresh instance name again
        // for the same reason.
        assert!(fresh
            .open(
                &table,
                registration_with(
                    "three",
                    "org.td.One",
                    Vec::new(),
                    vec![String::new(); MAX_OWNED_NAMES + 1],
                ),
                1000,
                table.caller(900),
            )
            .is_err());
        // Exactly at the ceiling is admitted, or the bound could be off by one
        // in the safe direction and nothing would say so.
        assert!(fresh
            .open(
                &table,
                registration_with(
                    "four",
                    "org.td.One",
                    Vec::new(),
                    vec![String::new(); MAX_OWNED_NAMES],
                ),
                1000,
                table.caller(900),
            )
            .is_ok());
    }

    /// The permission file's grant reaches the identity the walk answers with.
    ///
    /// It is carried at phase ONE, held while the registration is pending, and
    /// copied into the live record at phase two — so this follows it the whole
    /// way rather than asserting the end state. A grant that survived neither
    /// hop would look identical to one that survived both if only the last
    /// step were checked.
    #[test]
    fn a_grant_travels_from_phase_one_to_the_resolved_identity() {
        let table = Table::with(&[(1, 0, 1), (900, 1, 90), (100, 900, 95)]);
        let instances = Instances::new();
        let token = instances
            .open(
                &table,
                registration_with(
                    "one",
                    "org.td.One",
                    Vec::new(),
                    vec![
                        "org.mozilla.firefox".to_string(),
                        "org.example.Two".to_string(),
                    ],
                ),
                1000,
                table.caller(900),
            )
            .expect("phase one opens");
        instances
            .complete(&table, &token, 100, 1000, table.caller(900))
            .expect("phase two completes");
        assert_eq!(
            instances.resolve(&table, 100),
            Identity::Jailed {
                app_id: "org.td.One".to_string(),
                instance: "one".to_string(),
                owned: vec![
                    "org.mozilla.firefox".to_string(),
                    "org.example.Two".to_string(),
                ],
            }
        );
    }

    /// One instance's grant is not another's, and is nobody's when the peer
    /// belongs to no instance at all.
    ///
    /// The walk picks ONE instance out of the live set, and the grant it
    /// answers with has to be that instance's. A copy taken from the wrong
    /// record — the first, the last, the union — would still produce a
    /// plausible `Jailed` answer, and the app id alone would not catch it.
    #[test]
    fn a_grant_belongs_to_the_instance_the_walk_matched() {
        // 100 and 200 are two stage 2s; 300 descends from 200; 400 from
        // neither.
        let table = Table::with(&[
            (1, 0, 1),
            (100, 1, 10),
            (200, 1, 20),
            (300, 200, 30),
            (400, 1, 40),
        ]);
        let live = [
            instance_owning("one", 100, 10, &["org.example.One"]),
            instance_owning("two", 200, 20, &["org.example.Two"]),
        ];
        assert_eq!(
            resolve_against(&table, &live, &[], &[], 100),
            Identity::Jailed {
                app_id: "org.td.one".to_string(),
                instance: "one".to_string(),
                owned: vec!["org.example.One".to_string()],
            }
        );
        assert_eq!(
            resolve_against(&table, &live, &[], &[], 300),
            Identity::Jailed {
                app_id: "org.td.two".to_string(),
                instance: "two".to_string(),
                owned: vec!["org.example.Two".to_string()],
            }
        );
        // A peer under no instance is unconfined, which carries no list at
        // all — the arm has nowhere to put one, and that is the point.
        assert_eq!(
            resolve_against(&table, &live, &[], &[], 400),
            Identity::Unconfined
        );
    }

    /// The instance ceiling is not a one-way ratchet.
    ///
    /// `resolve` is not the only sweep, and if it were the ceiling would be
    /// reachable and permanent: filling the registry with registrations that
    /// are never completed would refuse every later launch until some
    /// connection happened along, which on a session bus with nothing
    /// connecting is never.
    #[test]
    fn abandoned_registrations_do_not_hold_the_ceiling_for_ever() {
        // The registrant IS looked up now — phase one records when it
        // started, so that phase two can tell the same process from a later
        // one wearing its pid. Nothing else here is.
        let table = Table::with(&[(900, 1, 90)]);
        let instances = Instances::new();
        for which in 0..MAX_INSTANCES {
            instances
                .open(
                    &table,
                    registration(&format!("i{which}"), "org.td.One"),
                    1000,
                    table.caller(900),
                )
                .expect("under the ceiling");
        }
        assert!(instances
            .open(&table, registration("more", "org.td.One"), 1000, table.caller(900))
            .is_err());

        let Some(later) = Instant::now()
            .checked_add(PENDING_LIFETIME)
            .and_then(|t| t.checked_add(Duration::from_secs(1)))
        else {
            return;
        };
        instances
            .open_at(&table, registration("more", "org.td.One"), 1000, table.caller(900), later)
            .expect("the abandoned registrations no longer hold their slots");
        assert_eq!(instances.pending_count(), 1);
    }

    /// And the same for the LIVE side of the ceiling.
    ///
    /// `resolve` is the only other sweep and it needs an incoming connection,
    /// so a launcher whose applications exit without ever touching the bus
    /// fills all 64 slots with dead instances and every later `Register` is
    /// refused for good. A review found this after the pending half was fixed:
    /// the same ratchet, in the neighbouring collection.
    #[test]
    fn instances_whose_jails_exited_do_not_hold_the_ceiling_either() {
        let mut rows = vec![(1, 0, 1), (900, 1, 90)];
        for which in 0..MAX_INSTANCES as i32 {
            rows.push((1000 + which, 900, 95));
        }
        let table = Table::with(&rows);
        let instances = Instances::new();
        for which in 0..MAX_INSTANCES as i32 {
            let token = instances
                .open(
                    &table,
                    registration(&format!("i{which}"), "fixture"),
                    1000,
                    table.caller(900),
                )
                .expect("under the ceiling");
            instances
                .complete(&table, &token, 1000 + which, 1000, table.caller(900))
                .expect("phase two completes");
        }
        assert_eq!(instances.live_count(), MAX_INSTANCES);
        assert!(instances
            .open(&table, registration("more", "fixture"), 1000, table.caller(900))
            .is_err());

        // Every jail exits, and no connection arrives to notice.
        let after = Table::with(&[(1, 0, 1), (900, 1, 90)]);
        instances
            .open(&after, registration("more", "fixture"), 1000, after.caller(900))
            .expect("dead instances still held their slots");
    }

    /// An instance whose jail has exited is REAPED, not refused for ever.
    ///
    /// This is the lifecycle case, and getting it wrong is a denial of service
    /// with no attacker in it: every application exit would leave a record the
    /// accounting pass trips over, and one ordinary launch-and-quit would make
    /// `Unconfined` unsayable for the rest of the broker's life.
    #[test]
    fn an_instance_whose_jail_exited_is_reaped_rather_than_refused_for_ever() {
        let table = Table::with(&[(1, 0, 1), (900, 1, 90), (100, 900, 95), (400, 1, 40)]);
        let instances = Instances::new();
        let token = instances
            .open(&table, registration("one", "org.td.One"), 1000, table.caller(900))
            .expect("phase one opens");
        instances
            .complete(&table, &token, 100, 1000, table.caller(900))
            .expect("phase two completes");
        assert_eq!(instances.live_count(), 1);

        // The jail exits: pid 100 is no longer anywhere. A peer whose lineage
        // never touched it is answered normally — the record is stale, not
        // ambiguous, for THIS connection — and the record is dropped in the
        // same call.
        let after = Table::with(&[(1, 0, 1), (900, 1, 90), (400, 1, 40)]);
        assert_eq!(instances.resolve(&after, 400), Identity::Unconfined);
        assert_eq!(instances.live_count(), 0, "the dead instance was not reaped");
    }

    /// The reap refuses the connection it actually endangers, and only that
    /// one.
    ///
    /// A dropped record matters to a lookup exactly when the dropped pid
    /// stands in that lookup's lineage: the instance ended, the number may
    /// already belong to the hop the walk went through, and every hop above
    /// was reached by trusting it. A draft refused EVERY connection that
    /// observed any reap, which a review showed a rogue can schedule at will —
    /// register a child, complete, kill it, and the next connection is denied,
    /// for its whole life, because identity is decided once at accept.
    #[test]
    fn a_reaped_pid_in_this_lineage_refuses_and_elsewhere_does_not() {
        // The jail's stage 2 is 100. It exits and pid 100 comes back as a
        // different process — start time 95 became 77 — which is what makes
        // the walk through it unsound rather than merely stale.
        let table = Table::with(&[(1, 0, 1), (900, 1, 90), (100, 900, 95)]);
        let instances = Instances::new();
        let token = instances
            .open(&table, registration("one", "org.td.One"), 1000, table.caller(900))
            .expect("phase one opens");
        instances
            .complete(&table, &token, 100, 1000, table.caller(900))
            .expect("phase two completes");

        let after = Table::with(&[(1, 0, 1), (100, 1, 77), (400, 100, 80), (500, 1, 50)]);
        match instances.resolve(&after, 400) {
            Identity::Unknown(why) => assert!(why.contains("passes through it"), "{why}"),
            other => panic!("a peer descending from a reaped pid resolved as {other:?}"),
        }
        assert_eq!(instances.live_count(), 0, "the dead instance was not reaped");
        // A peer elsewhere in the tree is unaffected, which is the half a
        // blanket refusal gave away.
        assert_eq!(instances.resolve(&after, 500), Identity::Unconfined);
    }

    /// A registration between its two phases has no stage-2 pid on record, so
    /// a peer descending from it would walk straight past — and `Unconfined`
    /// is the one answer that must not be given to the process that is
    /// certainly confined.
    ///
    /// A pending registrant that ended does not deny the process that
    /// inherited its number.
    ///
    /// The pending rule is deliberately narrow — descendants of the registrant
    /// and nobody else — because the wide version let any uid-1000 process
    /// deny the whole session by opening a registration it never finished.
    /// Matching on the NUMBER alone gives that lever back to chance: 900 ends
    /// with its registration open, the allocator hands 900 to something else,
    /// and every descendant of that innocent process is denied `Unconfined`
    /// for the rest of `PENDING_LIFETIME`. The record has carried the
    /// registrant's start time since the registry started taking a proved
    /// caller, so the comparison is over a process.
    #[test]
    fn a_pending_registrant_that_ended_denies_nobody() {
        let table = Table::with(&[(1, 0, 1), (900, 1, 90), (950, 900, 95)]);
        let instances = Instances::new();
        instances
            .open(&table, registration("one", "org.td.One"), 1000, table.caller(900))
            .expect("phase one opens");
        // While the registrant that opened it is still there, its descendant
        // is denied -- which is the rule working.
        match instances.resolve(&table, 950) {
            Identity::Unknown(why) => assert!(why.contains("in flight"), "{why}"),
            other => panic!("a pending instance's descendant resolved {other:?}"),
        }

        // The registrant ends and its number is handed on. 950 is now a child
        // of a process that never registered anything.
        table.replace(900, Stat { ppid: 1, starttime: 91 });
        assert_eq!(
            instances.resolve(&table, 950),
            Identity::Unconfined,
            "a stranger wearing the registrant's pid denied its descendants"
        );
    }

    /// And the other half of the pair: sharing the registrant's START TIME is
    /// not enough either.
    ///
    /// A start time is a tick count since boot, so processes forked together
    /// share one freely — 700 here is nothing to do with the registration and
    /// began in the same tick. Matching on it alone would deny `Unconfined` to
    /// a swathe of the process tree chosen by nothing but boot timing, which
    /// is the wide rule this one was narrowed from.
    #[test]
    fn a_pending_registration_denies_nobody_who_merely_shares_its_age() {
        let table = Table::with(&[
            (1, 0, 1),
            (900, 1, 90),
            (950, 900, 95),
            (700, 1, 90),
            (750, 700, 96),
        ]);
        let instances = Instances::new();
        instances
            .open(&table, registration("one", "org.td.One"), 1000, table.caller(900))
            .expect("phase one opens");
        match instances.resolve(&table, 950) {
            Identity::Unknown(why) => assert!(why.contains("in flight"), "{why}"),
            other => panic!("the registrant's own descendant resolved {other:?}"),
        }
        assert_eq!(
            instances.resolve(&table, 750),
            Identity::Unconfined,
            "a process that merely shares the registrant's start time was denied"
        );
    }

    /// The rule is scoped to DESCENDANTS of the registrant, and the scoping is
    /// the security-relevant part twice over. Too wide and any uid-1000
    /// process denies the whole session by opening a registration it never
    /// finishes. Too narrow — excluding nothing — and the peer that is about
    /// to be confined is called unconfined.
    #[test]
    fn a_registration_in_flight_covers_the_registrants_descendants() {
        // 900 is the registrant (stage 0/1). 950 is a child of it, standing in
        // for the stage 2 that has not been registered yet. 400 is elsewhere.
        let table = Table::with(&[
            (1, 0, 1),
            (400, 1, 40),
            (900, 1, 90),
            (950, 900, 95),
            (960, 950, 96),
        ]);
        let instances = Instances::new();
        assert_eq!(instances.resolve(&table, 400), Identity::Unconfined);

        let token = instances
            .open(&table, registration("one", "org.td.One"), 1000, table.caller(900))
            .expect("phase one opens");

        // A peer that could belong to the pending instance is refused...
        for peer in [950, 960] {
            match instances.resolve(&table, peer) {
                Identity::Unknown(why) => assert!(why.contains("in flight"), "{why}"),
                other => panic!("pid {peer} resolved as {other:?} mid-registration"),
            }
        }
        // ...a peer elsewhere in the tree is not, which is what keeps an
        // abandoned registration from denying the session.
        assert_eq!(instances.resolve(&table, 400), Identity::Unconfined);
        // ...and the REGISTRANT itself is not, or the broker would deny the
        // connection whose next message completes the registration.
        assert_eq!(instances.resolve(&table, 900), Identity::Unconfined);

        instances
            .complete(&table, &token, 950, 1000, table.caller(900))
            .expect("phase two completes");
        assert_eq!(instances.resolve(&table, 400), Identity::Unconfined);
        assert_eq!(
            instances.resolve(&table, 960),
            Identity::Jailed {
                app_id: "org.td.One".to_string(),
                instance: "one".to_string(),
                owned: Vec::new(),
            }
        );
    }

    /// A registration whose second phase never comes must not deny the
    /// session for ever.
    ///
    /// The registrant is the one party that could clean this up and it is
    /// exactly the party that is gone, so nothing but a deadline can. Without
    /// it one stage 0 that died between the phases would leave every later
    /// connection `Unknown` until the broker restarted.
    #[test]
    fn a_registration_that_is_never_completed_expires() {
        let table = Table::with(&[(1, 0, 1), (900, 1, 90), (950, 900, 95)]);
        let instances = Instances::new();
        instances
            .open(&table, registration("one", "org.td.One"), 1000, table.caller(900))
            .expect("phase one opens");
        assert_eq!(instances.pending_count(), 1);
        assert!(matches!(
            instances.resolve(&table, 950),
            Identity::Unknown(_)
        ));

        // The clock is an argument to RESOLVE, not to a sweep the test runs
        // itself. A draft called `expire_pending` here and then checked
        // `resolve`, which passes just as happily if `resolve` stops sweeping
        // at all — the regression it existed to catch was invisible to it.
        let later = Instant::now()
            .checked_add(PENDING_LIFETIME)
            .and_then(|t| t.checked_add(Duration::from_secs(1)));
        let Some(later) = later else {
            return;
        };
        assert_eq!(instances.resolve_at(&table, 950, later), Identity::Unconfined);
        assert_eq!(instances.pending_count(), 0, "resolve did not sweep");
    }

    #[test]
    fn app_id_is_reported_only_for_a_jailed_connection() {
        assert_eq!(
            Identity::Jailed {
                app_id: "org.td.One".to_string(),
                instance: "one".to_string(),
                owned: Vec::new(),
            }
            .app_id(),
            Some("org.td.One")
        );
        assert_eq!(Identity::Unconfined.app_id(), None);
        assert_eq!(Identity::Unknown("why".to_string()).app_id(), None);
    }
}
