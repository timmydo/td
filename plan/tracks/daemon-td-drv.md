section: side
status: claimed
title: daemon-td-drv
handle: claude-opus-3267ea
date: 2026-06-20
notes: plan/daemon-td-drv.md
summary: the td-artifact bridge (step 1) — the DAEMON realizes a td-assembled .drv, so a td-built artifact becomes daemon-valid and referenceable by the daemon-built system image. td assembles the recipe .drv with the daemon-valid GUIX-built td-builder (build-recipe, no stage0 override); a reusable Guile helper instantiates it into the daemon store (add-text-to-store) and the daemon builds it. Proven with hello (daemon ran td-builder → working hello at td's own daemon-free path). Gate: the daemon realizes td's coreutils .drv → daemon-valid 79-util multicall, distinct from guix's coreutils. Follow-up: SystemSpec.tdPackages → profile (reuses the helper).
