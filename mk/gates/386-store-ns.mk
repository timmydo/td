# store-ns — user-pm Phase 0: td OWNS ITS OWN ROOT with its own store at /td/store, breaking
# from guix (human 2026-06-21). `td-builder store-ns STORE-DIR -- CMD` enters a user namespace
# pivoted into a minimal td-owned root that binds STORE-DIR at /td/store and binds NOTHING from
# /gnu/store or /var/guix — so inside, /td/store IS the store and the host /gnu/store + guix
# install are ABSENT. Rootless (no daemon, no root). tests/store-ns.sh places a static binary
# (bash-static, from hello's seed closure) into a td-owned store and runs it inside the store-ns,
# asserting it runs from /td/store with /gnu/store absent (unmixed from the local guix). The
# unmixed base the /td/store package manager runs in; the dynamic toolchain is relocated to
# /td/store in Phase 2 (static sidesteps relocation here). td-builder is the guix-free stage0.
# Heavy (stage0 + a nested userns) → HEAVY_GATES.
HEAVY_GATES += store-ns
store-ns:
	@echo ">> store-ns: td owns its own root with its store at /td/store — a binary runs from /td/store, /gnu/store ABSENT (rootless, unmixed from guix; user-pm Phase 0)"
	sh tests/store-ns.sh
