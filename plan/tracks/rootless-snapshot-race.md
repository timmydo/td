section: side
status: claimed
title: rootless-snapshot-race
handle: claude-opus-117569
date: 2026-06-15
notes: plan/rootless-snapshot-race.md
summary: make the rootless store-DB snapshot race-free across concurrent checks by CONSTRUCTING it from the static closure (td store-register) instead of copying the live DB; the two daemon-lock/online-backup fixes are blocked by non-root perms.
