# td-shell — `td-builder shell` is td's own `guix shell`: bring named packages
# into a command's env and run it. The package layer stays on the guix oracle for
# v1 (`guix build PKG`, retired last §5); the env composition + exec are td's own,
# so the merit is DURABLE — the command runs with the package on PATH. tests/td-shell.sh
# builds the td-builder under test as a guix-free STAGE0 (no packager site, so
# guix-surface stays put) and asserts: behavioral (hello greets), structural (a
# real store hello on the composed PATH), load-bearing (no package -> fail), and a
# REMOVABLE guix-shell differential. Heavy (a stage0 compile), in the heavy pool.
HEAVY_GATES += td-shell
td-shell:
	@echo ">> td-shell: td-builder shell brings a package into a command's env and runs it (td's own guix shell; durable behavioral + load-bearing, removable guix-shell oracle)"
	sh tests/td-shell.sh
