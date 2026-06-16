section: mainline
status: claimed
title: own-builder-daemon
handle: claude-fable-2715d4
date: 2026-06-16
notes: plan/own-builder-daemon.md
summary: Stand up td's OWN builder daemon so the loop realizes derivations without guix-daemon. First increment landed: `td-builder realize` computes the build's input closure ITSELF (td's SQLite reader over the store db's Refs graph, replacing `guix gc -R`), builds in its userns sandbox, and registers the output — guix-daemon out of the realize path, only the differential oracle (td-realize gate). Next: drive richer recipes, register into td-store-db, then resume the parked offline-isolation / daemon-network work.
