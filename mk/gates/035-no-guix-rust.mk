# no-guix-rust — the shipped system carries NO guix-built Rust userland. The Rust
# userland (procs/fd/ripgrep/sd/eza/bat) ships as td's OWN bytes via td-native OCI
# images (gate rust-userland-image / #242); the guix `operating-system` no longer
# carries the guix `(gnu packages rust-apps)` objects. This is the DURABLE structural
# guard for that removal: it asserts neither system/td.scm nor system/td-typed.scm
# re-imports the guix Rust-apps module. The import is load-bearing — the six
# identifiers cannot resolve without it (the `eval` gate would fail to load a use
# without the import), so re-adding any of them forces the import back and trips this
# grep. GUIX-FREE by construction (no `$(GUIX)` in the recipe), so it stays green when
# guix is retired and adds no guix surface. Self-discriminating: re-add the module →
# red.
CHEAP_GATES += no-guix-rust
no-guix-rust:
	@echo ">> no-guix-rust: system/td.scm + system/td-typed.scm must NOT import the guix Rust-apps userland module (td ships its OWN Rust userland via td-native OCI images)"
	@set -eu; \
	for f in system/td.scm system/td-typed.scm; do \
	  test -f "$$f" || { echo "FAIL: $$f missing — cannot assert the shipped system is guix-rust-free" >&2; exit 1; }; \
	done; \
	if hits=`grep -nE 'gnu packages rust-apps' system/td.scm system/td-typed.scm`; then \
	  echo "FAIL: the guix Rust-apps module is referenced again — the shipped system would carry guix-built userland bytes instead of td's OWN build (rust-userland-image / #242):" >&2; \
	  printf '%s\n' "$$hits" >&2; \
	  exit 1; \
	fi; \
	echo "PASS: no-guix-rust — neither system/td.scm nor system/td-typed.scm imports the guix Rust-apps module; the Rust userland ships as td's OWN bytes via td-native OCI images (rust-userland-image)."
