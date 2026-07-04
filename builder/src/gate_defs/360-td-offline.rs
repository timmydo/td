//! td-offline — td's OWN builder enforces the no-undeclared-fetch isolation (DESIGN
//! §7.1 move-off-Guile §5; the parked offline-isolation work resumed in the
//! own-builder era). The `offline` gate (185) proves a non-fixed-output build cannot
//! reach the network under GUIX-DAEMON's sandbox; this proves td's OWN builder does
//! the same. td realizes the existing DRV_SANDBOX probe (tests/offline-drv.scm — a
//! regular derivation whose builder asserts /proc/net/dev lists ONLY `lo` and that a
//! TCP egress attempt raises): `td-builder realize` runs it in td's userns+NEWNET
//! sandbox, so realize SUCCEEDING means the build saw only loopback and egress failed
//! — DURABLE (the probe asserts it; no guix oracle). The discrimination control
//! (a userns+netns given a dummy non-lo interface, where the same /proc/net/dev check
//! DOES see it) proves the probe is load-bearing, not vacuous. All-durable; no guix
//! differential leg.

use crate::gates::{GateDef, Pool, StoreMode};

pub fn gate() -> GateDef {
    GateDef {
        name: "td-offline",
        pools: &[Pool::Heavy],
        needs: &[],
        build_gate: false,
        specs: &[],
        store: StoreMode::Private, // cold by design (#317 audit): offline probe — warm state would mask a network reach
        non_blocking: true,
        script: r##"
echo ">> td-offline: td's OWN builder network-isolates a non-fixed-output build (realize the DRV_SANDBOX probe: only lo + egress fails); the dummy-interface control proves the check is load-bearing"
set -euo pipefail; \
. tests/cache-lib.sh; export TD_STAGE0_BASE="$PWD/.td-build-cache/stage0"; load_stage0; tb="$TB"; \
case "$tb" in *.td-build-cache/stage0/*) : ;; *) echo "FAIL: td-builder is not the bootstrapped stage0 ($tb)" >&2; exit 1 ;; esac; \
test -x "$tb" || { echo "ERROR: no td-builder" >&2; exit 1; }; \
sdrv=`$TD_GUIX repl -L . tests/offline-drv.scm 2>/dev/null | sed -n 's/^DRV_SANDBOX=//p'`; \
test -n "$sdrv" || { echo "ERROR: could not lower the DRV_SANDBOX probe" >&2; exit 1; }; \
$TD_GUIX build "$sdrv" >/dev/null 2>&1 || { echo "ERROR: could not realize the probe's inputs" >&2; exit 1; }; \
scratch="$PWD/.td-offline-scratch"; chmod -R u+w "$scratch" 2>/dev/null || true; rm -rf "$scratch"; mkdir -p "$scratch"; \
echo ">> [DURABLE: td isolation] td-builder realize runs the probe in its userns+NEWNET sandbox (only lo + egress must fail, else the build reds)"; \
"$tb" realize "$sdrv" /gnu/store "$scratch/b" > "$scratch/log.txt" 2>&1 || { echo "FAIL: td realize of the network probe failed — td's builder did not isolate the build (or the probe saw the network):" >&2; cat "$scratch/log.txt" >&2; exit 1; }; \
grep -q 'netns interfaces: ("lo")' "$scratch/log.txt" || { echo "FAIL: the probe under td's builder did not report a loopback-only netns" >&2; cat "$scratch/log.txt" >&2; exit 1; }; \
grep -q 'egress attempt failed as required' "$scratch/log.txt" || { echo "FAIL: the probe under td's builder did not confirm egress failed" >&2; cat "$scratch/log.txt" >&2; exit 1; }; \
echo "   td's builder gave the build a loopback-only netns and egress failed"; \
echo ">> [DISCRIMINATION] the same /proc/net/dev check DOES see a non-lo interface when one is present (probe is load-bearing)"; \
ip=`$TD_GUIX build iproute2 2>/dev/null | head -1`/sbin/ip; \
unshare=`$TD_GUIX build util-linux 2>/dev/null | sed -n 's,$,/bin/unshare,p' | while read c; do test -x "$c" && { echo "$c"; break; }; done`; \
test -x "$ip" -a -n "$unshare" || { echo "ERROR: no ip/unshare binary for the control" >&2; exit 1; }; \
seen=`"$unshare" --user --map-root-user --net sh -c "$ip link add dummy0 type dummy >/dev/null 2>&1; grep -oE '^[ ]*[a-z0-9]+:' /proc/net/dev | tr -d ' :' | paste -sd, -"`; \
echo "   control netns interfaces: $seen"; \
case "$seen" in *dummy0*) : ;; *) echo "FAIL: control could not create/observe a non-lo interface — the discrimination is not exercised" >&2; exit 1;; esac; \
chmod -R u+w "$scratch" 2>/dev/null || true; rm -rf "$scratch"; \
echo "PASS: td's OWN builder network-isolated a non-fixed-output build — td-builder realize ran the probe in its userns+NEWNET sandbox, which saw a loopback-only netns and a failed TCP egress (DURABLE: the probe asserts it, no guix oracle); the dummy-interface control proves the /proc/net/dev check genuinely detects a non-lo interface, so the isolation assertion is load-bearing. The parked offline-isolation property now holds for td's builder."
"##,
    }
}
