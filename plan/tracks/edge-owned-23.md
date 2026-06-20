section: side
status: done
title: edge-owned-23
handle: claude-opus-feb041
date: 2026-06-19
pr: 107
notes: plan/edge-owned-23.md
summary: drive the guix-dependence edge-owned metric to 23/23 — chain the last 3 guix-wired edges (readline->ncurses, gettext-minimal->libunistring+ncurses, bash->readline+ncurses) and fold the per-subject build-plan gates (365 grep, 366 nano) into ONE manifest-driven gate. Every owned recipe then builds FROM td inputs.
