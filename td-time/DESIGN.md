# Time management

`td-time` owns clock status, automatic network synchronization and the
root-only application of consented time settings. The human-facing CLI
queues manual UTC, timezone and automatic-sync changes with td-authd;
secure attention presents the exact typed operation before Enter commits.
An SSH key grants no clock-changing authority.

## Clocks and policy

The system clock and the hardware RTC use UTC. The timezone is display
policy only, stored in the existing `/var/lib/td/timezone` and the new
`/var/lib/td/localtime` TZif file, linked from `/etc/localtime`. Existing
application sandboxes retain their timezone snapshot until relaunched. Changing it never shifts either clock. Manual UTC input is an exact
`YYYY-MM-DDTHH:MM:SSZ` in 2020 through 2099, validated with td-civil. The
requested instant is applied at commitment, not interpreted as local time.
The review says that automatic synchronization, if enabled, can adjust it.

Automatic synchronization is enabled by default. Its root-owned persisted
switch can be changed only by the consent operation. The service retries
while offline without blocking boot. Successful polls are thirty minutes
apart; failed polls retry after one minute. Kernel uptime advancing more than
five seconds beyond elapsed monotonic time schedules a fresh poll after
resume; slow queries do not reset the retry interval. All timeouts and poll
scheduling use monotonic time; wall-clock corrections never extend an authorization.

The first network implementation is a bounded SNTP client, not a full NTP
clock-discipline daemon or an authenticated time service. It queries fixed
upstream public time services, accepts only matching nonce/peer responses
with valid server mode, version, stratum and bounded timestamps, and requires
agreement between two distinct addresses. Neither DNS nor unauthenticated
NTP proves a server's identity: a network adversary controlling both paths
can falsify the time. Source agreement is an error check, not authentication.
This boundary must be visible in status and must not be described as secure
or authenticated synchronization. No TLS certificate check is disabled to
bootstrap time. NTS is a future separate protocol increment.

Each accepted sample is projected using elapsed monotonic time. Samples
more than two seconds apart, stale or oversized packets, unsynchronized
servers, kiss-of-death replies, mismatched origin timestamps, excessive
round-trip delay, and times outside the supported range are refused.
The service limits automatic correction to one day in either direction;
a clock farther away requires manual correction. A failed attempt retains
the existing clock and reports failure. Network and DNS work belongs to a
bounded child, so a resolver cannot hang the privileged service indefinitely.

A successful correction sets CLOCK_REALTIME and writes the RTC in UTC
when the RTC device is available. Status reports system-clock and RTC
outcomes separately; an RTC failure does not claim persistent correction.
The service does not configure alarms, interrupts or RTC wakeups.

## Authorization and state

The authority receives one bounded typed request from the session owner,
with kernel-pinned sender credentials and liveness. It shares the existing
hostname settings intake and H menu row; time changes require the separate
`manage-time` grant. The shared queue and backoff permit only one pending
hostname or time request. Consent tag 39 binds the exact canonical setting. The root-owned elevation
table must grant time management before admission and again before selection.
A fresh nonce binds the requester and exact operation. Cancellation, expiry,
requester loss or any mismatched description before commitment changes
nothing. Commitment is consumed once, including on helper failure. A lost
completion receipt is uncertain and never retried automatically.

Only the fixed installed helper can apply a request, with no caller-selected
program or path. Root and all account/system directories remain trusted;
other writers, symlinks, malformed state and oversized records refuse.
A root-owned lock serializes manual changes and automatic synchronization.
Manual application and a network result recheck the enabled state under that
lock; disabling automatic synchronization cannot race a later auto write.
Settings and status publish by atomic replacement. For a timezone change,
the canonical timezone name publishes first and localtime second; service
startup repairs localtime from that committed name after interruption.
The compositor reloads the selected zone when its setting bytes change. Persisted status is public
read-only data with boot identity and monotonic age and never a source of authorization.

The CLI status reports current UTC, configured timezone, RTC UTC when
readable, automatic mode, last successful synchronization and its age,
and the last error. It does not call an old success synchronized forever:
age and availability are explicit. Ordinary users may read status, but
cannot select network sources, change persistent policy or set clocks.

## Implementation and evidence

The library and protocol parsing forbid unsafe code. One binary-only,
function-scoped syscall boundary owns CLOCK_REALTIME writes and the one
RTC_SET_TIME request over a borrowed validated character-device descriptor.
UNSAFE.md records its exact numbers, values and layouts before it ships.
No process-global timezone environment variable supplies privileged policy.

Host tests cover leap dates and UTC bounds, hostile packet parsing, nonce
mismatch, era rollover, source disagreement, monotonic projection, bounded
correction, protected reads and atomic publication. Authority tests prove
exact request binding, distinct principal grants and no helper launch before
matching presentation and commitment; cancellation starts no helper. The
compositor tests prove display changes at a fixed UTC sample. No host test
sets the machine's clock.

`fixtures/clock_vm.rs` boots a disposable guest with its RTC seven hours
behind. It runs the production target-built helper and service. Only the
network-result child is a fixture: offline and agreeing-time replies exercise
the same parent deadline, correction policy, status and RTC writes without
depending on public servers. Packet checks are the library tests' evidence,
not this VM's. The guest checks disabled and offline non-mutation, correction
and RTC readback, manual UTC, timezone-only changes, persisted settings and
repair of an interrupted timezone publication on service restart. It does
not claim a physical-attention UI test or a power-cycle persistence test.

Build the host-only driver with Cargo's installed musl target:

```text
cargo build --release --manifest-path td-time/Cargo.toml --example clock-vm --target x86_64-unknown-linux-musl
```

Run `clock-vm KERNEL TD-INIT TD-TIME ZONEINFO NEW-LOG` with a target-built
helper and `TMPDIR` set beneath the owning worktree's
`.td-build-cache/agent-scratch/`. The driver owns and reaps only its QEMU child.
The raw clock boundary is absent from the driver; root inside the guest
invokes the fixed production helper. Ordinary service filesystem operations
and kernel teardown retain the kernel's uninterruptible-I/O limitations.

## Commands

```text
td-time status
td-time set 2026-10-30T06:11:00Z
td-time timezone America/Los_Angeles
td-time automatic on
td-time automatic off
```

Every setting command queues a request. Press Ctrl+Alt+Escape, then H,
review the exact setting, and press Enter. Status is read-only and works
without approval. A manual UTC value means that exact instant at commitment;
account for the time spent reviewing it. Timezone names use the installed
IANA catalog, including `Etc/UTC` for UTC.

References: [NTPv4](https://www.rfc-editor.org/rfc/rfc5905),
[NTP best practices](https://www.rfc-editor.org/rfc/rfc8633), and
[Linux RTC interface](https://docs.kernel.org/admin-guide/rtc.html).
