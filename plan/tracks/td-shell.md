section: side
status: done
title: td-shell
handle: claude-fable-db65ca
date: 2026-06-20
pr: 119
notes: plan/td-shell.md
summary: `td-builder shell` — td's own `guix shell`. `td shell PKG... -- CMD...` brings the named packages into CMD's environment and runs it. The "own, then diverge" split: the package layer (name -> derivation -> output) stays on the guix ORACLE for v1 (`guix build PKG`, the move-off-Guile §5 layer retired LAST), but the ENVIRONMENT COMPOSITION + exec are td's own (td prepends each resolved output's bin/sbin to PATH itself, no guix process in the exec path). v1 supports `shell PKG... [-- CMD...]` (no `--` -> interactive $SHELL); fuller flag surface (`-C`, `-m`, search-paths beyond PATH) is later. New td-shell gate (tests/td-shell.sh, stage0 td-builder so no packager site): DURABLE behavioral (hello greets), structural (a real store hello on the composed PATH), load-bearing (no package -> fail); REMOVABLE guix-shell differential.
