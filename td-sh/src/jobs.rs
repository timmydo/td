//! Background jobs: what `&` starts and what `wait` collects.
//!
//! A job is a THREAD, for the same reason a pipeline stage is one: this shell
//! clones itself in-process rather than forking, so the thing `&` starts is a
//! `Subshell` that owns its state and a clone of the AST it runs. That is what
//! makes `&` asynchronous at all -- before this, `&` ran the list to completion
//! and only then continued, so `sleep 1 & echo started` took a second to say
//! `started` and three background sleeps finished in the order they were
//! written rather than the order they were timed.
//!
//! Threads are also what the alternative costs. Real background PROCESSES need
//! `fork(2)`, which is not on this crate's syscall surface and would be a bad
//! one to add: forking a process that already runs pipeline stages as threads
//! gives a child in which only async-signal-safe work is defined, and the child
//! here would go on to allocate, expand and spawn. So the divergences below are
//! taken deliberately, and each is written down rather than left to be found.
//!
//! `$!` IS NOT A PID, and cannot be. POSIX says it is "the process ID of the
//! most recent background command"; there is no such process. The id allocated
//! here is therefore deliberately OUTSIDE the kernel's pid space -- Linux caps
//! `pid_max` at `PID_MAX_LIMIT`, 4194304, and these start at 2^30 -- so the one
//! use of `$!` that could do damage fails loudly instead. `kill -HUP $!`
//! (which the corpus does write) reports no such process, where an id that
//! collided with the pid space would have signalled a STRANGER: a shell that
//! kills an unrelated process is worse than one that admits it cannot. The ids
//! do not wrap and are not reused, so a stale `$!` never names a later job.
//!
//! A SHELL OUTLIVES THE JOBS IT STARTED, which a forking shell does not: bash
//! and dash orphan a running job and exit, and this one joins it. That is
//! `Drop` on the table below, so it holds for a subshell and a pipeline stage
//! as much as for the shell itself -- each joins its own.
//!
//! It is forced, and the alternative was measured rather than reasoned about. A
//! thread cannot be orphaned: when the process exits every thread stops where it
//! stands, so NOT joining does not give a job that keeps running, it gives a job
//! whose remaining output is discarded. `redirect.test.sh`'s noclobber case is
//! the corpus catching it -- dash reads `echo a &> /dev/null` as `echo a &` plus a
//! redirect, and the `a` went missing. A forking shell has no such window
//! because the job holds the stdout it inherited, so whoever reads that pipe
//! waits for the job whether the shell did or not.
//!
//! What it costs is a shell that does not come back while a job runs:
//! `td-sh -c 'sleep 100 &'` takes the full 100 seconds where bash returns at
//! once. That is the wrong end state and it is bounded by the same thing job
//! control is -- a job in a PROCESS of its own, which needs `fork(2)` or a
//! re-exec, and is where `jobs`/`fg`/`bg` have to land too. It is not a
//! regression against what this replaced: `&` used to run the list to
//! completion before the NEXT command, so the shell blocked for the same
//! duration and got no concurrency for it. And it is a visible failure rather
//! than a silent one, which is the trade this crate takes everywhere else.
//!
//! A JOB'S STDIN IS `/dev/null`, which POSIX 2.9.3 requires of an asynchronous
//! list wherever job control is disabled -- here, always. It reads as a nicety
//! and is the opposite: the script's own stdin is one descriptor with one
//! offset, and `sh < script` reads the script through the very descriptor its
//! commands share, deliberately, so that `cat` in a script sees the lines the
//! parser did not take. A job holding that descriptor takes bytes the parser is
//! owed AND moves the position under it. Measured on a 40-line script opening
//! with `{ read a; read b; } &`: three lines went missing and the parser
//! resumed MID-LINE, running `line8` as a command, where bash prints all 40 and
//! the job reads nothing.
//!
//! Worth saying because it was nearly recorded as unfixable: nothing about
//! WHICH read closes it. Reading the script a byte at a time while a job is
//! live does not, since the job's own `read` is bytewise too and the two
//! interleave byte by byte; seeking absolutely rather than relatively does not
//! either, the position query being a second syscall that races the same way.
//! Both were tried and measured still broken. The descriptor was the bug.

use crate::ast::AndOr;
use crate::exec::{self, Sig};
use crate::process::Subshell;
use std::thread::JoinHandle;

/// First job id. Above Linux's `PID_MAX_LIMIT` (4194304) so an id can never be
/// mistaken for a live process -- see the module note on `kill $!`.
const JOB_ID_BASE: u32 = 1 << 30;

/// Ids are allocated for the PROCESS, not per table.
///
/// Every clone gets a table of its own, so a per-table counter restarted at
/// `JOB_ID_BASE` for each -- and then `( { exit 3; } & wait $outer )` collected
/// the SUBSHELL's unrelated job, because the two had been handed the same
/// number. bash reports 127 there. Sharing one allocator is what makes "an id
/// that is not this shell's is 127" true, and with it the module's claim that a
/// stale `$!` never names another job.
static NEXT_ID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(JOB_ID_BASE);

/// How many COLLECTED jobs a table keeps the status of.
///
/// A job whose thread has ended still occupies an entry until something waits
/// for it, and a script that never waits would grow the table for as long as it
/// runs. Past this many, the oldest finished ones are forgotten and a `wait` for
/// them answers 127 -- which is what bash does when a job ages out of its own
/// list, and is the answer an id nobody kept would get anyway.
const REMEMBERED: usize = 4096;

struct Job {
    /// The `%N` number: small, 1-based, and REUSED once a job leaves the table,
    /// which is what makes `%1` the first job again after a `wait`. Distinct
    /// from `id` for that reason -- an id must never be reused, and a jobspec is
    /// no use if it grows without bound.
    number: u32,
    /// `None` once the ids are exhausted: the job still runs and is still
    /// joined, it just cannot be NAMED. Never matched by `wait_id`, since
    /// nothing can spell it.
    id: Option<u32>,
    /// The running thread, or -- once reaped -- `None` and a status.
    handle: Option<JoinHandle<i32>>,
    /// Set when the thread has been joined, which a `wait` for this id then
    /// answers with. A job is reaped as soon as it is seen to have FINISHED, so
    /// the shell holds a status rather than a thread.
    status: Option<i32>,
}

/// The shell's own background jobs. Not inherited by a clone: a subshell cannot
/// wait for its parent's jobs, and a `JoinHandle` belongs to one shell.
pub struct Jobs {
    jobs: Vec<Job>,
    /// Numbers given back by jobs that have left the table, smallest first.
    ///
    /// A HEAP rather than a scan of the table, because the scan was quadratic
    /// and the table is deliberately not small: `REMEMBERED` keeps up to 4096
    /// collected jobs, numbers are contiguous, so looking for the lowest free
    /// one cost `n²/2` comparisons per `&`. Measured on `while [ $i -lt N ]; do
    /// : & …; done`: 0.447s at N=8000 before jobs existed, 36.7s with the scan,
    /// and it is the very shape `reap` was written to keep survivable.
    free: std::collections::BinaryHeap<std::cmp::Reverse<u32>>,
    /// The next number never yet handed out, used when `free` is empty.
    next_number: u32,
}

impl Jobs {
    pub fn new() -> Self {
        Self { jobs: Vec::new(), free: std::collections::BinaryHeap::new(), next_number: 1 }
    }

    /// Give a number back, so the lowest free one is reused -- which is what
    /// makes `%1` the first job again once the previous one has been collected.
    /// Every path that takes a job out of the table goes through here.
    fn release(&mut self, number: u32) {
        self.free.push(std::cmp::Reverse(number));
    }

    /// Join the threads that have already ENDED, and forget the oldest
    /// collected jobs past `REMEMBERED`.
    ///
    /// This is what makes `while :; do cmd & done` survivable rather than the
    /// abort it was. An unjoined thread keeps its stack mappings charged to the
    /// process even after its work is done, so a loop that never waits ran the
    /// process out of `vm.max_map_count` in a couple of seconds -- and the
    /// failure is not the graceful one at the `spawn` call site, because
    /// `clone(2)` SUCCEEDS and the new thread then panics inside std's own
    /// bootstrap setting up its guard page. This crate aborts on a panic, so
    /// that is SIGABRT out of a loop the parent commit ran forever.
    ///
    /// `is_finished` is what makes it non-blocking: only threads already done
    /// are joined, so reaping never waits for anything.
    fn reap(&mut self) {
        for job in &mut self.jobs {
            if job.handle.as_ref().is_some_and(JoinHandle::is_finished) {
                if let Some(handle) = job.handle.take() {
                    job.status = Some(handle.join().unwrap_or(128));
                }
            }
        }
        let collected = self.jobs.iter().filter(|j| j.handle.is_none()).count();
        let mut excess = collected.saturating_sub(REMEMBERED);
        // Oldest first, and only the ones already collected: a job still
        // RUNNING is never forgotten, because the shell has to join it. The
        // third and last path that takes a job out of the table, so like the
        // other two it gives the number back.
        let mut freed = Vec::new();
        self.jobs.retain(|j| {
            if j.handle.is_none() && excess > 0 {
                excess -= 1;
                freed.push(j.number);
                return false;
            }
            true
        });
        for number in freed {
            self.release(number);
        }
    }

    /// Take a started job into the table, answering the id `$!` reports, or
    /// `None` once the ids are exhausted.
    ///
    /// Refusing is what keeps "ids are never reused" true rather than nearly
    /// true. A saturating counter would hand every job past the last id THE
    /// SAME one, and `wait_id` takes the first match -- so a stale `$!` would
    /// come to name a later job and every job after the first would be
    /// un-waitable, silently. The bound is 2^32 - 2^30 jobs in one shell, which
    /// nothing reaches by accident; a shell that does is out of a resource and
    /// says so.
    pub fn record(&mut self, handle: JoinHandle<i32>) -> Option<u32> {
        self.reap();
        // `try_update` rather than `fetch_add`, so exhaustion REFUSES instead of
        // wrapping onto ids that are still in use. The job is TAKEN either way
        // -- it is already running, and the table is what joins it -- so only
        // the name is refused.
        let id = NEXT_ID
            .try_update(
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
                |n| n.checked_add(1),
            )
            .ok();
        // The lowest number not currently in use, as both references do.
        let number = match self.free.pop() {
            Some(std::cmp::Reverse(n)) => n,
            None => {
                let n = self.next_number;
                self.next_number = self.next_number.saturating_add(1);
                n
            }
        };
        self.jobs.push(Job { number, id, handle: Some(handle), status: None });
        id
    }

    /// The id a `%…` jobspec names, or `None` if this shell has no such job.
    ///
    /// Only the four spellings POSIX gives without a command name: `%N`, and
    /// `%%`/`%+` (the current job) and `%-` (the previous one). `%name` and
    /// `%?string` search the COMMAND TEXT, which this shell does not keep --
    /// see `list` -- so they are refused rather than guessed at.
    pub fn spec_id(&self, spec: &str) -> Option<u32> {
        let rest = spec.strip_prefix('%')?;
        // "Current" and "previous" are by START order, which is the table's
        // order: nothing reorders it, and `retain`/`remove` keep it.
        let nth_from_end = |n: usize| self.jobs.len().checked_sub(n).and_then(|i| self.jobs.get(i));
        let job = match rest {
            "%" | "+" => nth_from_end(1),
            "-" => nth_from_end(2),
            digits if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) => {
                let number: u32 = digits.parse().ok()?;
                // `position` + `get` rather than the searching adaptor: this
                // file is embedded verbatim in the td-sh RECIPE, and the ladder
                // guard scans that text for the host findutils tools as bare
                // tokens -- comments included, so the name cannot be written
                // here either. The recipe documents the rule; this is the first
                // module to have tripped it.
                self.jobs.iter().position(|j| j.number == number).and_then(|at| self.jobs.get(at))
            }
            _ => None,
        };
        job?.id
    }

    /// One line per job, for the `jobs` builtin: the `%N` number, whether it is
    /// the current (`+`) or previous (`-`) one, its state, and its id.
    ///
    /// What is NOT here is the COMMAND, which both references print and this
    /// cannot: the AST carries no source span, so the text the operator typed is
    /// gone by the time a job exists, and reconstructing it from the tree would
    /// print something that is not what they wrote. The corpus never sees this
    /// -- its one `jobs` case runs inside a pipeline, whose stage has a table of
    /// its own and so has nothing to list -- which is why an approximation
    /// would be graded by nobody and believed by an operator.
    ///
    /// Sorted by NUMBER rather than by the table's own order, which reuse can
    /// leave out of step: collect `%2` of three, start a fourth, and the table
    /// reads 1, 3, 2. The `+`/`-` marks stay positional, since they are about
    /// which job is current and not about where it sits.
    pub fn list(&mut self) -> Vec<String> {
        // Reaped FIRST, or a job that has already finished is reported
        // `Running`: nothing else joins a thread, so without this the state
        // shown is whatever was true when the last `&` ran. Measured saying
        // `Running` for a job that had exited a fifth of a second earlier.
        self.reap();
        let last = self.jobs.len();
        let mut lines: Vec<(u32, String)> = self
            .jobs
            .iter()
            .enumerate()
            .map(|(at, job)| {
                let mark = match at + 1 {
                    n if n == last => '+',
                    n if n + 1 == last => '-',
                    _ => ' ',
                };
                let state = match (job.handle.is_some(), job.status) {
                    (true, _) => "Running".to_string(),
                    (false, Some(0)) | (false, None) => "Done".to_string(),
                    (false, Some(code)) => format!("Done({code})"),
                };
                let line = match job.id {
                    Some(id) => format!("[{}] {mark} {state}\t{id}", job.number),
                    None => format!("[{}] {mark} {state}", job.number),
                };
                (job.number, line)
            })
            .collect();
        lines.sort_by_key(|(number, _)| *number);
        lines.into_iter().map(|(_, line)| line).collect()
    }

    /// The ids of the jobs in the table, for `jobs -p`. Reaped first, for
    /// `list`'s reason.
    pub fn ids(&mut self) -> Vec<u32> {
        self.reap();
        self.jobs.iter().filter_map(|j| j.id).collect()
    }

    /// `wait` with no operands: collect every job, discarding statuses. POSIX
    /// gives this 0 whatever the jobs did, which is what "status is lost" in the
    /// corpus's own comment means.
    pub fn wait_all(&mut self) {
        // Drained rather than iterated: a job collected once is gone, so a
        // later `wait $id` for it reports no such job as bash does.
        for mut job in std::mem::take(&mut self.jobs) {
            if let Some(handle) = job.handle.take() {
                let _ = handle.join();
            }
        }
        // The table is empty, so the numbers start over rather than being handed
        // back one at a time: `%1` means the first job again after a bare
        // `wait`, which is what both references do.
        self.free.clear();
        self.next_number = 1;
    }

    /// Is any job of this shell's still running? Only the `exec` path asks,
    /// because replacing the process image is the one exit a join cannot follow.
    pub fn any_running(&self) -> bool {
        self.jobs.iter().any(|j| j.handle.is_some())
    }

    /// `wait <id>`: collect that job and answer its status, or `None` if this
    /// shell has no such job -- which includes one already collected.
    pub fn wait_id(&mut self, id: u32) -> Option<i32> {
        let at = self.jobs.iter().position(|j| j.id == Some(id))?;
        let mut job = self.jobs.remove(at);
        self.release(job.number);
        match job.handle.take() {
            // Already reaped: `reap` joined it when it was seen to have
            // finished, and the status it left is the answer.
            None => job.status,
            // A job whose thread died leaves no status to report. It cannot
            // happen through the closure below, which returns a status on every
            // path, so this is the shape of a panic elsewhere -- and this crate
            // aborts on one, so nothing observes the 128.
            Some(handle) => Some(handle.join().unwrap_or(128)),
        }
    }
}

/// A shell joins the jobs it started, wherever it ends -- see the module note.
///
/// On the table rather than at the exit paths so that it cannot be forgotten by
/// one of them, and so that a SUBSHELL gets it too: `( { sleep 1; echo x; } & )`
/// ends its clone long before the shell ends, and the clone is the only thing
/// holding that job.
impl Drop for Jobs {
    fn drop(&mut self) {
        self.wait_all();
    }
}

#[cfg(test)]
mod tests {
    use super::JOB_ID_BASE;

    fn run(src: &str) -> (i32, String, String) {
        crate::process::run_capturing(src)
    }

    /// Shell work with no external command and no clock, long enough that a
    /// shell which did NOT join its jobs would be seen not to. Timing is the
    /// corpus's job -- its `sleep` is a staged helper -- and these have to hold
    /// on a build host with nothing on `PATH`.
    const SLOW: &str = "i=0; while [ $i -lt 30000 ]; do i=$((i+1)); done";

    /// The safety-critical property, and the one nothing else can check: an id
    /// must not be able to NAME A PROCESS. `kill -HUP $!` is written in the
    /// corpus, and an id inside the pid space would signal a stranger.
    #[test]
    fn a_job_id_is_outside_the_kernels_pid_space() {
        // Linux's PID_MAX_LIMIT: `pid_max` cannot be raised past it, so no
        // process ever bears a number this large.
        const PID_MAX_LIMIT: u32 = 4 * 1024 * 1024;
        assert!(JOB_ID_BASE > PID_MAX_LIMIT);
        let (_, out, _) = run("true & echo $!");
        let id: u32 = out.trim().parse().expect("$! is a number");
        assert!(id > PID_MAX_LIMIT, "$! = {id} could be a live pid");
    }

    /// `$!` names the job just started, and `wait` answers for THAT one -- so
    /// the ids have to be distinct and the statuses have to be kept per job.
    /// Waited in the reverse of the order started, which is what stops a table
    /// that simply queues statuses from passing.
    #[test]
    fn each_job_keeps_its_own_id_and_status() {
        let (_, out, _) = run(
            "{ exit 7; } & a=$!\n{ exit 9; } & b=$!\n\
             [ \"$a\" != \"$b\" ] && echo distinct\n\
             wait $b; echo $?\nwait $a; echo $?",
        );
        assert_eq!(out, "distinct\n9\n7\n");
    }

    /// Ids are not REUSED once a job is collected, so a `$!` kept across a
    /// `wait` cannot come to name a later job -- the failure a pid-like table
    /// with recycling would have.
    #[test]
    fn a_collected_id_is_gone_rather_than_reused() {
        let (_, out, _) = run(
            "{ exit 1; } & a=$!\nwait $a; echo first=$?\n\
             wait $a; echo again=$?\n{ exit 2; } & b=$!\n\
             [ \"$a\" != \"$b\" ] && echo distinct\nwait $b; echo second=$?",
        );
        assert_eq!(out, "first=1\nagain=127\ndistinct\nsecond=2\n");
    }

    /// POSIX: `wait` with no operands collects everything and reports 0 whatever
    /// the jobs did. With operands it reports the LAST one's, not the last to
    /// finish and not the worst.
    #[test]
    fn wait_reports_zero_for_all_and_the_last_id_for_a_list() {
        assert_eq!(run("{ exit 3; } & wait; echo $?").1, "0\n");
        assert_eq!(run("{ exit 8; } & a=$!\n{ exit 9; } & b=$!\nwait $a $b; echo $?").1, "9\n");
        assert_eq!(run("wait; echo $?").1, "0\n");
    }

    /// POSIX makes the status the LAST operand's, so an id this shell does not
    /// have contributes its 127 and the loop goes on -- returning at the first
    /// one waited for NEITHER. bash agrees on both orders.
    #[test]
    fn an_unknown_id_does_not_abandon_the_operands_after_it() {
        assert_eq!(run("{ exit 7; } & p=$!\nwait 12345678 $p; echo $?").1, "7\n");
        assert_eq!(run("{ exit 7; } & p=$!\nwait $p 12345678; echo $?").1, "127\n");
        // ...and the job really was collected, rather than the status having
        // come from somewhere else: a second `wait` for it is now 127.
        assert_eq!(run("{ exit 7; } & p=$!\nwait 12345678 $p\nwait $p; echo $?").1, "127\n");
    }

    /// Ids come from ONE allocator for the process, not one per table. Every
    /// clone gets a table of its own, so a per-table counter handed the parent's
    /// first job and a subshell's first job the same number -- and `wait $outer`
    /// inside the subshell then collected the subshell's unrelated job and
    /// reported ITS status. bash answers 127, which is what an id that is not
    /// this shell's has to be.
    #[test]
    fn an_id_from_one_shell_never_names_another_shells_job() {
        let (_, out, _) = run(
            "{ exit 7; } & a=$!\n\
             ( { exit 3; } & b=$!\n\
               [ \"$a\" != \"$b\" ] && echo distinct\n\
               wait $a; echo inner=$? )\n\
             wait $a; echo outer=$?",
        );
        assert_eq!(out, "distinct\ninner=127\nouter=7\n");
    }

    /// A `%…` jobspec resolves to the same id `$!` gave, so `wait %1` and
    /// `wait $!` cannot disagree about a job's status.
    #[test]
    fn a_jobspec_names_the_same_job_an_id_does() {
        // `%N` by number, and `%%`/`%+`/`%-` by how recently it started.
        let (_, out, _) = run(
            "{ exit 8; } & { exit 9; } &\n\
             wait %-; echo prev=$?\nwait %+; echo cur=$?",
        );
        assert_eq!(out, "prev=8\ncur=9\n");
        assert_eq!(run("{ exit 5; } &\nwait %%; echo $?").1, "5\n");
        assert_eq!(run("{ exit 6; } & p=$!\nwait %1; echo $?\nwait $p; echo $?").1, "6\n127\n");
        // A spelling this shell cannot resolve is a usage error, not a silent
        // 127: `%foo` names a COMMAND, whose text is not kept.
        assert_eq!(run("wait %nope").0, 2);
        assert_eq!(run("wait %9").0, 2);
        assert_eq!(run("wait %").0, 2);
    }

    /// The `%N` NUMBER is reused once a job leaves the table, where the id never
    /// is. Both are needed for that reason: a jobspec nobody can predict is no
    /// use, and an id that comes round again would let a stale `$!` name a later
    /// job.
    #[test]
    fn a_job_number_is_reused_where_an_id_is_not() {
        let (_, out, _) = run(
            "{ exit 3; } & a=$!\nwait %1; echo a=$?\n\
             { exit 4; } & b=$!\nwait %1; echo b=$?\n\
             [ \"$a\" != \"$b\" ] && echo ids-differ",
        );
        assert_eq!(out, "a=3\nb=4\nids-differ\n");
    }

    /// The lowest FREE number, which needs a hole in the middle to show.
    ///
    /// Every other test here picks a number against an empty table, so they only
    /// prove the counter is not global -- an implementation handing out
    /// `len + 1` passes all of them, and then collecting `%2` of three and
    /// starting a fourth gives TWO jobs numbered 3, with `%3` silently naming
    /// the older one. That is what this covers, and it is the property the
    /// separate `free`/`next_number` bookkeeping exists for.
    #[test]
    fn a_freed_number_is_taken_before_a_fresh_one() {
        let (_, out, _) = run(
            "{ exit 1; } & { exit 2; } & { exit 3; } &\n\
             wait %2; echo second=$?\n\
             { exit 9; } &\n\
             jobs\n\
             wait %2; echo reused=$?\nwait %3; echo third=$?",
        );
        let lines: Vec<&str> = out.lines().collect();
        // The fourth job took 2 back, so the listing is 1, 2, 3 with no
        // duplicate -- and it is SORTED, where the table itself reads 1, 3, 2.
        let numbers: Vec<&str> = lines.iter().filter_map(|l| l.split(']').next()).collect();
        assert_eq!(numbers, ["second=2", "[1", "[2", "[3", "reused=9", "third=3"], "{out}");
    }

    /// `jobs` lists this shell's own, `-p` gives the ids alone, and a CLONE
    /// lists nothing -- which is the whole of what the corpus grades here. Its
    /// `jobs | wc -l` case expects 0 under dash because dash forks its pipeline
    /// stages and the stage running `jobs` inherits none; td-sh clones per stage
    /// for the same reason, so the 0 is not arranged.
    #[test]
    fn jobs_lists_this_shells_own_and_a_clone_has_none() {
        // One line per job, each a single WORD -- both halves of the corpus's
        // `jobs -p` case, counted here rather than through `wc`, which is not on
        // a unit test's `PATH`. The ids themselves cannot be spelled: they come
        // from one allocator for the process, so a concurrent test moves them.
        let (_, out, _) = run("{ exit 0; } & { exit 0; } &\njobs -p");
        assert_eq!(out.lines().count(), 2, "{out}");
        assert!(out.lines().all(|l| l.split_whitespace().count() == 1), "{out}");
        // And they are the IDS -- what `$!` gave and what `wait` takes -- not the
        // small `%N` numbers, which would also be one word on a line each. Both
        // are printed by the same run, since an id cannot be spelled ahead of
        // time: they come from one allocator for the process.
        let (_, out, _) = run("{ exit 0; } & echo bg=$!\njobs -p");
        let want = out.lines().next().and_then(|l| l.strip_prefix("bg=")).unwrap_or("");
        assert!(!want.is_empty(), "{out}");
        assert_eq!(out.lines().nth(1), Some(want), "{out}");
        // In a PIPELINE the stage has a table of its own, so it lists nothing --
        // `jobs` here runs as the SECOND stage, writing to the shell's stdout.
        assert_eq!(run("{ exit 0; } & { exit 0; } &\ntrue | jobs -p").1, "");
        // ...and so does a subshell.
        assert_eq!(run("{ exit 0; } &\n( jobs -p )").1, "");
        // The listing numbers the jobs and marks the current one `+` and the one
        // before it `-`, which is what a `%+`/`%-` spec is read against.
        let (_, out, _) = run("{ exit 0; } & { exit 0; } & { exit 0; } &\njobs");
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 3, "{out}");
        assert!(lines.first().is_some_and(|l| l.starts_with("[1]   ")), "{out}");
        assert!(lines.get(1).is_some_and(|l| l.starts_with("[2] - ")), "{out}");
        assert!(lines.get(2).is_some_and(|l| l.starts_with("[3] + ")), "{out}");
    }

    /// A job that has FINISHED is listed as such. Nothing else joins a thread,
    /// so without a reap on this path the state shown is whatever was true when
    /// the last `&` ran -- measured reporting `Running` for a job that had
    /// exited a fifth of a second earlier, and going on saying it.
    ///
    /// The entry is KEPT rather than dropped once reported, where bash lists a
    /// finished job once and forgets it. What bash does not forget is the
    /// STATUS: `{ exit 5; } & p=$!; sleep .2; jobs >/dev/null; wait $p` is 5
    /// there, so dropping the entry here would answer 127 for a status this
    /// shell still owes. Repeating a `Done` line costs nothing; losing a status
    /// `wait` is owed is a wrong answer.
    #[test]
    fn a_finished_job_is_listed_as_done_and_still_waitable() {
        let (_, out, _) = run(&format!("{{ exit 4; }} & p=$!\n{SLOW}\njobs\njobs\nwait $p; echo w=$?"));
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 3, "{out}");
        assert!(lines.first().is_some_and(|l| l.contains("Done(4)")), "{out}");
        // Listed AGAIN, deliberately -- see above.
        assert_eq!(lines.first(), lines.get(1), "{out}");
        assert_eq!(lines.get(2), Some(&"w=4"), "{out}");
    }

    /// Only `-p`. The other listing flags select or annotate by a state this
    /// shell does not have -- nothing here is ever STOPPED, a thread having no
    /// `SIGTSTP` -- so they are refused rather than ignored.
    #[test]
    fn an_unserved_jobs_option_is_refused() {
        for code in ["jobs -l", "jobs -r", "jobs -s", "jobs -n", "jobs %1"] {
            assert_eq!(run(code).0, 2, "{code}");
        }
        assert!(run("jobs -l").2.contains("jobs"), "no diagnostic");
        // An OPERAND is told apart from an option, so the diagnostic names the
        // mistake that was made: `jobs %1` asks to select, `jobs -l` asks for a
        // flag.
        assert!(run("jobs %1").2.contains("select"), "{:?}", run("jobs %1").2);
        // `--` ends the options here as it does for `wait`.
        assert_eq!(run("jobs --").0, 0);
        assert_eq!(run("{ exit 0; } &\njobs -p --").1.lines().count(), 1);
        // After `--` a `-p` is an OPERAND, which this refuses.
        assert_eq!(run("jobs -- -p").0, 2);
    }

    /// `--` ends the options, which the Utility Syntax Guidelines require of
    /// every utility and dash's `nextopt` gives every builtin.
    #[test]
    fn a_double_dash_ends_the_options() {
        assert_eq!(run("{ exit 5; } & p=$!\nwait -- $p; echo $?").1, "5\n");
        assert_eq!(run("wait --; echo $?").1, "0\n");
        // Only leading: after it, `--` is an operand, and not a number.
        assert_eq!(run("wait -- --").0, 2);
    }

    /// A job is the END of a shell environment, so a `return` inside one has
    /// nowhere to return to and its operand is the job's status -- bash reports
    /// 7 here, and reading `$?` instead reported 0.
    #[test]
    fn a_return_in_a_job_is_that_jobs_status() {
        assert_eq!(run("f() { return 7 & p=$!; wait $p; echo $?; }\nf").1, "7\n");
    }

    /// POSIX 2.9.3: with job control disabled, an asynchronous list's stdin is
    /// `/dev/null` before any explicit redirection. The reason is not tidiness
    /// -- `sh < script` reads the script through the descriptor its commands
    /// share, so a job that reads stdin eats the script and moves the parser's
    /// position under it.
    #[test]
    fn a_jobs_stdin_is_empty_rather_than_the_shells() {
        // The job has to REPORT what it read: a variable it sets is its own, so
        // asking the parent for `$x` is empty either way and would pass against
        // a job that had eaten the input.
        assert_eq!(run("echo hi | { { read x; echo \"job[$x]\"; } & wait; }").1, "job[]\n");
        // A redirect the list writes for ITSELF still wins, which is what
        // "before any explicit redirection" means. Through a path of its own:
        // `run` inherits the PROCESS's cwd, which is the crate directory, so a
        // relative redirect here writes into the repository -- and one did,
        // reaching a commit as `td-sh/f`.
        let dir = std::env::temp_dir().join(format!("td-sh-jobstdin-{}", std::process::id()));
        let created = std::fs::create_dir_all(&dir).is_ok();
        assert!(created, "could not make a scratch directory");
        let file = dir.join("f").to_string_lossy().into_owned();
        let out = run(&format!("echo hi > {file}\n{{ read x < {file}; echo \"[$x]\"; }} &\nwait"));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(out.1, "[hi]\n");
    }

    /// Every shape this shell cannot ANSWER is refused rather than approximated,
    /// which for `wait` is dash's status 2 -- an unknown id being the one
    /// exception, since 127 is the answer POSIX gives for "not a child of this
    /// shell" and is what the corpus grades.
    #[test]
    fn an_unanswerable_wait_is_refused_rather_than_guessed() {
        for (code, want) in [
            // bash's `-n`: collect whichever finishes next. Not per-id, so not
            // this table's question.
            ("wait -n", 2),
            ("wait -x", 2),
            // A jobspec needs the job table `jobs`/`fg`/`bg` would print.
            ("wait %1", 2),
            ("wait %nonexistent", 2),
            // Not a number at all.
            ("wait zzz", 2),
            ("wait 1a", 2),
            ("wait ''", 2),
            // `+5` is the spelling `str::parse` would have taken and a pid never
            // has; `-5` is read as an option before it can be a number.
            ("wait +5", 2),
            // A number, but no job of this shell's -- including one too large to
            // be an id at all.
            ("wait 12345678", 127),
            ("wait 99999999999999999999", 127),
        ] {
            assert_eq!(run(code).0, want, "{code}");
        }
        // The refusal is DIAGNOSED, not just counted: a status alone leaves a
        // script's author with nothing to read.
        assert!(run("wait zzz").2.contains("wait"), "no diagnostic");
        // An id that simply is not there is silent, which is where this parts
        // from bash (it says "pid N is not a child of this shell"). The corpus
        // asserts the STATUS alone for that case, and a shell modelled on
        // ash/dash has no message of its own to copy.
        assert_eq!(run("wait 12345678").2, "");
    }

    /// The rule the module note turns on: a shell does not end while a job it
    /// started is still running, so nothing a job writes is lost. Without it
    /// these three lose their output to whichever buffer is read first.
    #[test]
    fn a_shell_outlives_the_jobs_it_started() {
        // The shell itself, with no `wait` anywhere.
        assert_eq!(run(&format!("{{ {SLOW}; echo late; }} &")).1, "late\n");
        // A SUBSHELL, which ends long before the shell does and is the only
        // thing holding its own job.
        assert_eq!(run(&format!("( {{ {SLOW}; echo x; }} & ); echo after")).1, "x\nafter\n");
        // A command substitution, whose capture must not be read until the job
        // that writes into it is done -- `hi` is what bash captures here.
        assert_eq!(run(&format!("x=$( {{ {SLOW}; echo hi; }} & ); echo got=$x")).1, "got=hi\n");
    }

    /// A job is a subshell, so what it changes is its own. Concurrency does not
    /// relax that: the `wait` proves the job really ran and set the variable in
    /// the copy it was given.
    #[test]
    fn a_job_changes_nothing_in_the_shell_that_started_it() {
        let (_, out, _) = run("x=1\n{ x=2; echo job=$x; } &\nwait\necho shell=$x");
        assert_eq!(out, "job=2\nshell=1\n");
        // `cd` included, which a `&` that ran in the shell's own state would
        // move.
        assert_eq!(run("p=$PWD; cd / &\nwait\n[ \"$PWD\" = \"$p\" ] && echo same").1, "same\n");
    }

    /// `&` reports 0 for the START, never the job's own status -- the job has
    /// not finished, so there is nothing else it could report.
    #[test]
    fn backgrounding_reports_the_start_and_not_the_outcome() {
        assert_eq!(run("false & echo $?").1, "0\n");
        assert_eq!(run("{ exit 3; } & echo $?").1, "0\n");
        // `$!` is UNSET until one runs, rather than defaulting to a number that
        // would name process zero.
        assert_eq!(run("echo \"[$!]\"").1, "[]\n");
        assert_eq!(run("set -u; echo $!").0, 2);
    }
}

/// Start `and_or` as a background job in `child`, answering the thread `$!` will
/// name.
///
/// The `Subshell` moves INTO the thread, so its `Drop` runs when the job ends
/// rather than when `&` returns -- which is what puts back the umask the job
/// changed, and what joins any job the job itself started. Not the signal
/// dispositions: a job is `concurrent`, so it never installed one to put back.
pub fn spawn(mut child: Subshell, and_or: &AndOr) -> Result<JoinHandle<i32>, String> {
    // Marked HERE rather than by the caller, since this is the only way to make
    // a job and forgetting it is not a thing a call site should be able to do.
    // It is the flag a pipeline stage carries, for the same reason: the
    // interrupt guard and the kernel's signal dispositions are one cell shared
    // with whatever else is running, so neither is a job's to touch.
    child.concurrent = true;
    // POSIX 2.9.3: with job control disabled -- which is this shell, always --
    // an asynchronous list's stdin is `/dev/null` BEFORE any explicit
    // redirection. Set on the table here, so a redirect the list writes for
    // itself still wins, and so a background PIPELINE's first stage gets it too.
    //
    // It is not a nicety. The script's own stdin is one descriptor with one
    // offset, and a job that reads it takes bytes the parser is owed and moves
    // the position under it: measured on a 40-line `sh < script` opening with
    // `{ read a; read b; } &`, three lines went missing and the parser resumed
    // MID-LINE, running `line8` as a command. bash gives the job `/dev/null`
    // and prints all 40. Nothing about WHICH read closes that -- both a
    // bytewise script reader and an absolute rewind were tried and measured
    // still broken -- because the job should never have had the descriptor.
    child.fds.detach_stdin();
    // Cloned because the thread outlives the parse tree's borrow. The AST is
    // plain owned data, so this is a deep copy of one `&`-terminated list and
    // not of the script.
    let and_or = and_or.clone();
    // `Builder`, not `thread::spawn`, for `run_pipeline`'s reason: that one
    // PANICS when the OS cannot make a thread, and how many jobs a script
    // starts is whatever the operator wrote. This crate does not panic on an
    // error path.
    std::thread::Builder::new()
        .spawn(move || {
            let status = match exec::run_and_or(&mut child, &and_or) {
                Ok(()) => child.status,
                // `Return` is here beside them because a job IS the end of a
                // shell environment: `f() { return 7 & }` has nowhere left to
                // return TO, so 7 is the job's status, which is what bash
                // reports. Falling into the catch-all read `child.status`, which
                // `return` never sets, and lost the 7.
                Err(Sig::Exit(code) | Sig::Abort(code) | Sig::Return(code)) => code,
                // An interrupt does NOT come back out, where every other clone
                // boundary re-raises it: there is no caller left on this thread
                // to raise it to, and the parent is somewhere else entirely.
                // Nothing is swallowed that the operator would miss -- a job
                // takes no interrupt guard (`concurrent`), so the signal that
                // ended it reached the whole process and the shell dies of it
                // too, or was already ignored.
                Err(Sig::Interrupt(code)) => code,
                Err(_) => child.status,
            };
            // The EXIT trap RUNS even for that interrupt, where `run_pipeline`
            // returns before it for an interrupted stage. The two agree on the
            // rule and differ on the facts: a stage's interrupt is a signal that
            // reached this process, so the stage is a shell environment ending
            // in a signal DEATH and POSIX runs no trap for one. A job's is
            // inferred from a CHILD of the job dying, and the job itself is
            // ending normally with 130 -- nothing killed it, so its trap is
            // owed.
            exec::run_exit_trap(&mut child, status)
        })
        .map_err(|e| e.to_string())
}
