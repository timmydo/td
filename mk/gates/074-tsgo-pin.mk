# tsgo-pin — keep the td-fetch tsgo pin (tests/td-tsgo.lock) honest against the guix
# origin (move-off-Guile §5 consumer-swap, follow-on to td-fetch #116). The loop reads the
# pin instead of `guix build -e '(@ (system td-ts) td-tsgo-tarball)'`; this gate guards the
# pin so it cannot silently drift from the origin in system/td-ts.scm.
#
#   [DURABLE] the warm tarball at the pinned path hashes to the pin's sha256 (the bytes
#     the loop will extract really are the pinned content — coreutils sha256sum, no guix).
#   [DURABLE] the pin url equals the origin uri in system/td-ts.scm (a bumped origin url
#     without a pin bump reds).
#   [MIGRATION ORACLE, removable] the pinned store path equals the guix origin's FOD path
#     (`guix build -e '(@ (system td-ts) td-tsgo-tarball)'`) — own, then diverge: the
#     daemon-stored td-fetched bytes land at the same content-addressed path guix pins.
CHEAP_GATES += tsgo-pin
tsgo-pin:
	@echo ">> tsgo-pin: the td-fetch tsgo pin (tests/td-tsgo.lock) matches the guix origin (content + url + FOD path)"
	@set -euo pipefail; \
	lock="$(CURDIR)/tests/td-tsgo.lock"; \
	url=`sed -n 's/^url //p' "$$lock" | head -1`; \
	sha=`sed -n 's/^sha256 //p' "$$lock" | head -1`; \
	path=`sed -n 's/^path //p' "$$lock" | head -1`; \
	test -n "$$url" -a -n "$$sha" -a -n "$$path" || { echo "FAIL: malformed pin (need url/sha256/path)" >&2; exit 1; }; \
	test -s "$$path" || { echo "FAIL: pinned tarball not warm at $$path — run tools/warm-tsgo.sh" >&2; exit 1; }; \
	got=`sha256sum "$$path" | cut -d' ' -f1`; \
	test "$$got" = "$$sha" || { echo "FAIL: warm tarball sha256 $$got != pin $$sha" >&2; exit 1; }; \
	echo "  [DURABLE] the warm tarball hashes to the pin sha256 ($$sha)"; \
	fn=`basename "$$url"`; \
	grep -qF "$$fn" "$(CURDIR)/system/td-ts.scm" || { echo "FAIL: pin url filename $$fn not found in system/td-ts.scm — origin/pin version drift" >&2; exit 1; }; \
	echo "  [DURABLE] the pin url's tarball filename ($$fn) matches the td-tsgo-tarball origin"; \
	op=`$(GUIX) build $(LOAD) -e '(@ (system td-ts) td-tsgo-tarball)'`; \
	test "$$op" = "$$path" || { echo "FAIL: pin path $$path != guix origin FOD $$op — bump tests/td-tsgo.lock with the origin" >&2; exit 1; }; \
	echo "  [MIGRATION ORACLE, removable] the pin path equals the guix origin FOD ($$op) — td-fetched bytes, daemon-stored, same content-addressed path"; \
	echo "PASS: tsgo-pin — tests/td-tsgo.lock matches the guix td-tsgo-tarball origin (warm content sha256 + url + FOD path); the loop reads this pin instead of resolving the origin via guix, td-fetch is the fetcher (warm-tsgo.sh), the daemon only stores the verified bytes (own, then diverge)."
