section: side
status: done
title: td-tsgo-pin
handle: claude-fable-5caf33
date: 2026-06-20
notes: plan/td-tsgo-pin.md
summary: move-off-Guile §5 consumer-swap (follow-on to td-fetch #116) — retire the 15 in-loop `guix build -e '(@ (system td-ts) td-tsgo-tarball)'` invocations. td-fetch warms the tsgo tarball (host network PREP), the daemon only STORES the verified bytes (add-to-store lands at the SAME FOD path the origin produces), and the loop reads a committed pin (tests/td-tsgo.lock) — no guix-as-fetcher in the gates. check.sh ensures warmth host-side; ci/build-ci-image.sh keeps the FOD an image root so check-fast stays offline. The guix origin stays as the pin/oracle (own, then diverge).
