# td-drv-add (DESIGN §7.1; the §5 move-off-Guile arc). Wire td's constructed `.drv`
# INTO the loop: td-builder REGISTERS it in the store itself via the guix-daemon
# worker-protocol `addTextToStore` (a Rust client, builder/src/daemon.rs) — no guile
# `(derivation …)`/`add-text-to-store`. The daemon (C++) stays the store/build
# backend. The gate: (1) `drv-emit` — td constructs the hello `.drv` byte-identical to
# guix's (#22); (2) `drv-add` — register it via the daemon, which returns td's OWN
# computed path (== guix's, by content addressing); (3) `store-add` of a
# uniquely-named object — the daemon WRITES td's bytes at a NOVEL path (proves it is
# not idempotent reuse: the path did not exist, and the read-back content matches);
# (4) `guix build` the td-registered `.drv` — output runs (Hello, world!), i.e. the
# loop builds td's registration. Scope: input RESOLUTION (the skeleton `.drv`) stays
# Guix's; the daemon is the backend. Heavy (a td-builder compile + a warm hello
# realise), so it slots in the heavy pool by the other td gates. Scratch on disk,
# removed on green.
HEAVY_GATES += td-drv-add
td-drv-add:
	@echo ">> td-drv-add: td-builder REGISTERS its constructed .drv via the daemon (addTextToStore) — no guile (derivation …); the loop builds td's registration"
	@set -euo pipefail; \
	tb=`$(GUIX) build $(LOAD) -e '(@ (system td-builder) td-builder)'`/bin/td-builder; \
	test -x "$$tb" || { echo "ERROR: could not build td-builder" >&2; exit 1; }; \
	scratch="$(CURDIR)/.td-drv-add-scratch"; rm -rf "$$scratch"; mkdir -p "$$scratch"; \
	drv=`$(GUIX) repl $(LOAD) tests/td-drv-add-drv.scm 2>/dev/null | sed -n 's/^DRV=//p'`; \
	test -n "$$drv" || { echo "ERROR: could not lower the td-build hello derivation" >&2; exit 1; }; \
	echo ">> hello .drv (skeleton, guile-resolved inputs): $$drv"; \
	echo ">> (1) td constructs the .drv byte-identical to guix's (#22):"; \
	"$$tb" drv-emit "$$drv" >/dev/null \
	  || { echo "FAIL: td's construction is not byte-identical to guix's .drv" >&2; exit 1; }; \
	echo ">> (2) td REGISTERS it via the daemon addTextToStore — daemon returns td's computed path:"; \
	added=`"$$tb" drv-add "$$drv"` \
	  || { echo "FAIL: td-builder drv-add (daemon registration) failed" >&2; exit 1; }; \
	test "$$added" = "$$drv" \
	  || { echo "FAIL: the daemon registered $$added but the .drv is $$drv" >&2; exit 1; }; \
	echo "   registered at td's own computed path: $$added"; \
	echo ">> (3) NOVEL-write proof: a uniquely-named object the store did NOT have:"; \
	uniq="td-drv-add-probe-$$$$.txt"; \
	printf 'td novel write %s\n' "$$uniq" > "$$scratch/novel.txt"; \
	novel=`"$$tb" store-add "$$uniq" "$$scratch/novel.txt"` \
	  || { echo "FAIL: td-builder store-add (daemon write) failed" >&2; exit 1; }; \
	test -f "$$novel" \
	  || { echo "FAIL: the daemon did not write the novel path $$novel" >&2; exit 1; }; \
	test "`cat "$$novel"`" = "`cat "$$scratch/novel.txt"`" \
	  || { echo "FAIL: the daemon-stored content does not match what td sent" >&2; exit 1; }; \
	echo "   daemon wrote $$novel (content matches td's bytes)"; \
	echo ">> (4) the loop builds td's REGISTERED .drv:"; \
	out=`$(GUIX) build "$$added"`; \
	test -n "$$out" -a -x "$$out/bin/hello" \
	  || { echo "FAIL: guix build of the td-registered .drv produced no bin/hello" >&2; exit 1; }; \
	say=`"$$out/bin/hello"`; \
	test "$$say" = "Hello, world!" \
	  || { echo "FAIL: the built hello printed '$$say', expected 'Hello, world!'" >&2; exit 1; }; \
	rm -rf "$$scratch"; \
	echo "PASS: td-builder constructed the hello .drv AND registered it in the store via the daemon's addTextToStore (no guile (derivation …)); the daemon returned td's own computed path, wrote a novel object byte-for-byte, and the loop built td's registered .drv to a working hello."
