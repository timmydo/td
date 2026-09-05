# td-editor

A planned Wayland-native, dependency-free Rust text editor with a simple
tabbed interface, Windows-like and Emacs key profiles, paragraph filling,
and local spell checking. It is intended to run both on td and other Linux
Wayland desktops and to support deterministic tests and explicit local
remote control.

Start with [DESIGN.md](DESIGN.md). It records the architecture, reuse map,
file-safety requirements, compatibility boundaries, tmc integration findings,
acceptance tests, and independently landable increments. The current change
contains the design only; there is no executable or build command yet.

Two constraints shape the first implementation: td's current bitmap renderer
is software-based, and td-term's exact-keymap check does not support arbitrary
host Wayland keyboards. `sockets=wayland` alone supplies neither GPU access
nor an editor executable inside td-jail.

tmc currently deletes its temporary draft and attachment files when its
editor child exits. Saving a draft in place does not retain it, and tmc has
no mail submission path. The design describes this integration gap explicitly.
