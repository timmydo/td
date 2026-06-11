# Track: offline-isolation (side-track) — CLOSED 2026-06-11 (rescoped)

**Claim status:** see `PLAN.md` (the single source of truth for claims).
**Origin:** standing follow-up first surfaced in M6 (see HISTORY.md "Offline posture").
**Scope authority:** DESIGN §7.1 (entry updated with the 2026-06-11 rescope).

## Goal (original)

Close the gap between the loop's guarantee (*no substitutes + no offload*) and full
network isolation: drop nonguix from the host daemon's substitute URLs and isolate
the daemon's network so a cold path can't even query.

## Outcome — half delivered, half rescoped (human decision, 2026-06-11)

- **Delivered (landed on main, 0d7c3de):** "a deliberate undeclared fetch
  (non-fixed-output network access) demonstrably fails (verified-red)" — the
  `offline` rung asserts it EVERY loop, with `--check` re-executing the probe.
- **Rescoped:** isolating the daemon's network and dropping nonguix from its
  substitute set. The human declined host changes on `t5700g`: it is the
  owner's main machine and its OWN system config needs nonguix substitutes
  (kernel/firmware), so the shared host daemon is not td's to isolate. That
  work moves to the era when td runs its **own builder daemon** (the
  rootless-builder line of work and its successors; prime directive 4 — any
  such daemon must first prove behavioral equivalence against guix-daemon as
  oracle). When the daemon is project state instead of host state, its network
  posture can be declared and asserted without touching anyone's machine. The
  human's instruction is the DESIGN §4.3 sign-off for this roadmap change;
  DESIGN §7.1's entry records it.

Everything needed to resume is archived below: the verified-red evidence, the
assertions that were built and proven red (the rung extension full-text, the
check.sh guards in condensed form), and the netns-wrapper package design.
They were committed as f9aa3d5 on the since-discarded track branch — that SHA
is unreachable from any ref and may be GC-pruned; THIS file is the durable
record.

## Host survey (2026-06-11, for the record)

- Host is a Guix System (shepherd). `guix-daemon` runs as root in the host
  netns, cmdline: `--build-users-group guixbuild --max-silent-time 3600
  --timeout 86400 --log-compression gzip --discover=no --substitute-urls
  https://substitutes.nonguix.org https://bordeaux.guix.gnu.org
  https://ci.guix.gnu.org` → nonguix in the daemon's substitute set (the M6
  finding), nonguix signing key authorized in `/etc/guix/acl` (1 of 3 keys).
- Host config source: `~/.config/guix-system-config/t5700g/config.scm`
  (provenance comment in /run/current-system/configuration.scm); it appends
  the nonguix URL and key via `modify-services` on guix-service-type.

## What landed (S1) — the `offline` rung

`tests/offline-drv.scm` lowers two probe derivations sharing one builder body
(read `/proc/net/dev`, expect ONLY `lo`; then a TCP connect to 192.0.2.1:9
must raise — interface check first so a network-visible red fails fast):

- **DRV_SANDBOX** (non-fixed-output): runs in the build sandbox's private
  netns. The rung builds it and `guix build --check`s it (assertions
  re-execute every loop + reproducibility). This is the acceptance's
  "deliberate undeclared fetch fails".
- **DRV_DAEMON** (fixed-output twin): lowered but NOT built by the rung — FO
  builders run in the daemon's own netns, so on this host it is red by
  construction. It stays in the script as the verified-red vehicle (and the
  ready-made assertion for the own-builder era).

`check.sh` gained the host-side control: the host netns must show a non-lo
interface via the same /proc/net/dev mechanism, so "only lo" probes provably
discriminate.

### S1 verified-red evidence (2026-06-11)

Green run first: `./check.sh offline` →
`sandbox probe: netns interfaces: ("lo")`,
`sandbox probe: egress attempt failed as required: Network is unreachable`,
then `--check` re-ran the builder with the same output (PASS, exit 0).

(a) **Network-visible red** — the fixed-output twin (DRV_DAEMON,
`a64qwx62y989z8zdmh28z40qsjyp50wl-td-offline-daemon-probe.drv`), the IDENTICAL
builder body run where network IS present (the daemon's netns), built
manually with `guix build --no-substitutes --no-offload`:

    daemon probe: netns interfaces: ("lo" "tap0")
    FAIL: the daemon netns sees non-loopback interfaces ("lo" "tap0") — an
    undeclared fetch could reach the network from a path that must be isolated.
    builder for `…-td-offline-daemon-probe.drv' failed with exit code 1
    (guix build exit status 1)

So the shared assertions detect a network-visible netns and fail the build —
the sandbox probe's green is not vacuous.

(b) **Plumbing red** — temporarily inverted the expected interface list
(`'("lo")` → `'("lo0")`) and ran `./check.sh offline`:

    FAIL: the sandbox netns sees non-loopback interfaces ("lo") — …
    builder for `…v5viivszpzrddqfrql1ag87whdv5ia65-td-offline-sandbox-probe.drv'
    failed with exit code 1
    make: *** [Makefile:587: offline] Error 1   (check.sh exit 2)

So a builder-level failure propagates guix build → make → check.sh red.
Reverted; rung green again (exit 0).

## Archived for the own-builder era (S2 — built, proven red, NOT landed)

Commit f9aa3d5 (discarded branch) carried these; all three were observed red
against the live host with no temp edits — today's host state IS the broken
state — which is exactly why they could not land:

1. **cmdline drift guard:** `./check.sh offline` → `check.sh: FATAL:
   guix-daemon (pid 856) is configured with a nonguix substitute URL …`
   (exit 1, before the container starts).
2. **ACL drift guard:** the nonguix key's q-value matches `/etc/guix/acl`
   (`grep -ci … /etc/guix/acl` → 1 of 3 keys), so the pattern provably
   detects the state it forbids.
3. **daemon probe wired into the rung:** see S1 evidence (a).

### The rung extension (Makefile `offline`, after the sandbox-probe `--check`)

```make
	echo ">> daemon probe (fixed-output twin): the DAEMON'S OWN netns must be empty too"; \
	echo ">> daemon probe derivation: $$daemon_drv"; \
	$(GUIX) build "$$daemon_drv"; \
	echo ">> re-run + reproducibility: --check forces the daemon probe assertions to re-execute"; \
	$(GUIX) build --check "$$daemon_drv"; \
```

(plus extracting `daemon_drv` from the `DRV_DAEMON=` line and requiring it
non-empty alongside `sandbox_drv`.)

### The check.sh drift guards (fail-closed; ran on the host before the container)

```sh
# (1) No guix-daemon process may name nonguix as a substitute server. Every
#     branch fails CLOSED: no pidof, no visible daemon, or an unreadable
#     cmdline of a live pid all refuse to run; only a pid that exited
#     mid-scan is skipped.
command -v pidof >/dev/null || fatal "pidof not found"
daemon_pids=$(pidof guix-daemon || true)
[ -n "$daemon_pids" ] || fatal "no running guix-daemon"
for pid in $daemon_pids; do
  if ! cmdline=$(tr '\0' ' ' < "/proc/$pid/cmdline" 2>/dev/null); then
    [ -d "/proc/$pid" ] && fatal "cannot read cmdline of live daemon $pid"
    continue
  fi
  printf '%s' "$cmdline" | grep -q nonguix && fatal "daemon $pid has nonguix"
done
# (2) The nonguix signing key must not be authorized: substitute-urls is only
#     a default (a client can pass its own), but an archive signed by a key
#     absent from /etc/guix/acl can NEVER be imported — the ACL is the
#     enforcement layer. Unreadable ACL fails CLOSED.
[ -r /etc/guix/acl ] || fatal "cannot read /etc/guix/acl"
grep -qi "C1FD53E5D4CE971933EC50C9F307AE2171A2D3B52C804642A7A35F84F3A4EA98" \
  /etc/guix/acl && fatal "nonguix key authorized in /etc/guix/acl"
```

(f9aa3d5 carried these expanded, with per-case FATAL messages, plus a rewrite
of check.sh's "THE CONTRACT" header paragraph to match the stronger posture.)

### The netns-wrapper design (for a daemon td owns)

For a stock guix-daemon, the only netns lever is wrapping the binary: the
regular guix package with ONLY `bin/guix-daemon` swapped for
`exec unshare --net -- real/bin/guix-daemon "$@"` (everything else symlinked
through so a system profile is unaffected). Test-built fine as
`kz03ks…-guix-daemon-netns-isolated-1.5.0-1.deedd48` — a seconds-long
trivial-build-system symlink farm, no guix rebuild:

```scheme
(use-modules (guix packages) (guix gexp) (guix build-system trivial)
             (gnu packages package-management)   ;guix
             (gnu packages linux)                ;util-linux
             (gnu packages bash))                ;bash-minimal

(define guix-daemon-netns-isolated
  ;; guix with bin/guix-daemon confined to its own EMPTY network namespace:
  ;; substitute queries, discovery, offloading AND fixed-output builders can
  ;; never reach the network. Client<->daemon unix sockets are unaffected;
  ;; non-fixed-output builds already get their own private netns from the
  ;; daemon; the store stays warm.
  (package
    (inherit guix)
    (name "guix-daemon-netns-isolated")
    (source #f)
    (build-system trivial-build-system)
    (native-inputs '())
    (propagated-inputs '())
    (inputs (list guix util-linux bash-minimal))
    (arguments
     (list
      #:modules '((guix build utils) (ice-9 ftw))
      #:builder
      #~(begin
          (use-modules (guix build utils) (ice-9 ftw))
          (let ((real    #$(this-package-input "guix"))
                (sh      (string-append #$(this-package-input "bash-minimal")
                                        "/bin/sh"))
                (unshare (string-append #$(this-package-input "util-linux")
                                        "/bin/unshare")))
            (mkdir-p (string-append #$output "/bin"))
            (for-each (lambda (entry)
                        (unless (member entry '("." ".." "bin"))
                          (symlink (string-append real "/" entry)
                                   (string-append #$output "/" entry))))
                      (scandir real))
            (for-each (lambda (entry)
                        (unless (member entry '("." ".." "guix-daemon"))
                          (symlink (string-append real "/bin/" entry)
                                   (string-append #$output "/bin/" entry))))
                      (scandir (string-append real "/bin")))
            (call-with-output-file (string-append #$output "/bin/guix-daemon")
              (lambda (port)
                (format port "#!~a
# guix-daemon pinned in its own empty network namespace (td offline-isolation).
# unshare(1) needs CAP_SYS_ADMIN; the daemon starts as root under shepherd.
exec ~a --net -- ~a/bin/guix-daemon \"$@\"
"
                        sh unshare real)))
            (chmod (string-append #$output "/bin/guix-daemon") #o555)))))
    (synopsis
     "Guix with the build daemon confined to an empty network namespace")))
```

Wiring (any Guix System hosting such a daemon):
`(guix-configuration (inherit config) (guix guix-daemon-netns-isolated)
(substitute-urls %default-substitute-urls)
(authorized-keys %default-authorized-guix-keys))`, then reconfigure and
`herd restart guix-daemon`.

**Why this was declined for t5700g:** an empty daemon netns also blocks
fixed-output source fetches THROUGH that daemon, so the HOST's own
`guix pull`/reconfigure would break at the first cold fetch — and this host's
config needs nonguix kernel/firmware fetches. Acceptable for a dedicated td
builder daemon; not for the owner's main machine. Hence the rescope.

### Resumption checklist (when td has its own builder daemon)

1. The daemon's network posture is project state: declare it isolated (empty
   netns or equivalent) in the daemon's own configuration.
2. Wire `DRV_DAEMON` into the `offline` rung (extension above) — it must go
   green against the isolated daemon and stays the per-loop behavioral assert.
3. Re-add drift guards appropriate to wherever that daemon's config lives
   (the check.sh forms above assume the daemon is host state; an own daemon
   may make them structural instead).
4. Re-run the S1/S2 verified-red playbook before trusting any green.

## Sub-task ladder (final)

- [x] S1 `offline` rung: sandbox probe (build + --check) + check.sh host
      control. Landed on main (0d7c3de) after a full green check.
- [x] S2 daemon probe + drift guards: built and proven red against the live
      host (f9aa3d5, discarded branch; full text archived above).
- [x] S3 host change — DECLINED by the human 2026-06-11; daemon-side isolation
      rescoped to the own-builder-daemon era (DESIGN §7.1 updated).
- [x] S4 close-out: rescope recorded, claim released in PLAN.md.
