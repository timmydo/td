# td-editor

A planned Wayland-native, dependency-free Rust text editor with a simple
tabbed interface, Windows-like and Emacs key profiles, paragraph filling,
and on-demand whole-document spell checking. It is intended to run both on
td and other Linux Wayland desktops and to support deterministic tests and
explicit local remote control.

Start with [DESIGN.md](DESIGN.md). It records the architecture, reuse map,
file-safety requirements, compatibility boundaries, tmc integration findings,
acceptance tests, and independently landable increments. Version 1 uses
Unicode-scalar editing and single-cell Unifont rendering, preserves UTF-8
BOM and uniform LF/CRLF files, defaults to Windows-like bindings, and uses
an explicitly selected local English word list. Spelling runs only on
request; an edit clears marks without starting another scan. The current
change contains the design only; there is no executable or build command yet.

Two constraints shape the first implementation: td's current bitmap renderer
is software-based, and td-term's exact-keymap check does not support arbitrary
host Wayland keyboards. Version 1 targets td and Weston's US English map.
`sockets=wayland` alone supplies neither GPU access nor an editor executable
inside td-jail. A render-node grant can be added for Firefox; the design
lists its driver, runtime-policy and DMA-BUF prerequisites. Direct GPU
rendering for the dependency-free editor also needs a source-built graphics
implementation; the software reference backend does not complete that goal.

tmc currently deletes its temporary draft and attachment files when its
editor child exits. Saving a draft in place does not retain it, and tmc has
no mail submission path. The design describes this integration gap explicitly.
