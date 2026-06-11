# M10 — bootc-style generations for td

## The idea
td already builds both a bootable image and an OCI image from one declaration. M10
makes that OCI image *bootable* — a bootc-style image. A "generation" is just a td
OCI image you can boot, list in GRUB, and roll back to by picking an older entry.

We copy Fedora's bootc / "image mode" *model* (it ships in production), but not its
guts: Guix isn't FHS and doesn't use ostree, so we are not interoperable with bootc
or ostree. We take the convention — a bootable OCI image you extract and boot — and
write our own small placer. We almost certainly don't ship the bootc binary itself.

## How a generation flows
1. **Build.** Guix builds the OS components — kernel, initrd, userspace — and
   assembles them into a bootc-style OCI image (today via the daemon; the rootless
   builder is the deferred `rootless-builder` side-track). Reproducible; passes
   `guix build --check`.
2. **Distribute.** It's an ordinary OCI image. Push/pull via any registry.
3. **Place.** On the target (which has no guix), a small tool unpacks the image into
   its own per-generation root and adds a GRUB menu entry. The entry points at that
   generation's kernel and initrd **and selects that generation's root** — via the
   kernel `root=` (a per-generation label/UUID, or a shared partition plus
   `rootflags`/an initrd that mounts the right subdir). This is the crux: td today
   hardcodes one shared root (the fixed `td-root` label, `system/td.scm:57`), so if
   every entry mounted `td-root` they'd all boot the *same* filesystem and rollback
   would do nothing. Each generation must therefore have a distinct, selectable root.
4. **Boot.** Ordinary kernel boot of *that generation's* root.
5. **Roll back.** Older generations keep their menu entries — boot one to roll back.
   Old entries get pruned once there are too many.

No boot counters, no health agent, no auto-commit. **Rollback is manual** — you pick
the entry. That's the whole simplification, and it's basically what Guix's own
generation menu already does.

## Build side: unprivileged Guix builder
Build the components with Guix, but rootless — user-namespace isolation instead of
the setuid daemon + build-user accounts. Our loop already runs everything inside
`guix shell -C --pure`, so we're partway there. The point is a pipeline that looks
like a normal rootless container build while keeping Guix's reproducibility.

## Reuse vs. skip
- **Reuse:** the bootc image *convention* (an OCI image carrying a bootable kernel),
  td's existing OCI lowering, plain GRUB menu entries.
- **Skip for v0:** ostree and composefs/fs-verity — i.e. cross-generation dedup and
  integrity verification. We keep a few full roots; dedup earns its place later.
  (Integrity verification has since been settled as M11 = dm-verity over the
  per-generation root image — see "Verified generations" below and DESIGN §7.1;
  dedup remains re-parked.)
- **Skip:** the bootc binary (Rust crate tree; FSDG vendoring cost, no payoff here).

## What "a generation bundle" is (checked)
td's current OCI lowering (`image-with-os docker-image`) emits **userspace only** —
no kernel, no initrd, no bootloader. It's a run-as-a-container image: its "boot" just
runs `guile $system/boot` (shepherd) under a container runtime, on the host kernel.
The disk-image lowering is the opposite — it builds the kernel, initrd, and GRUB.

So a generation bundle is **OCI userspace + a bootable kernel + initrd + its own root
identity**. The kernel/initrd the disk-image path already produces; the missing piece
is the per-generation **root**. Each generation needs a distinct root the bootloader can
select — its own labeled filesystem, or a shared partition with a per-generation
subvolume/directory the initrd mounts — because td today hardcodes one `td-root` label
shared by everything (`system/td.scm:57`). Pinning this is M10.1's core job; without it,
multiple generations boot the same root and rollback is a no-op.

## State model (added 2026-06-10 — normative version in DESIGN §2.6)

M10.3 is the first time anything persists on the target across a reboot, so the
state model lands here. The model (decided with the human; track-record rationale
in DESIGN §2.6): generation images are read-only OS content; one writable
filesystem, label `td-state`, is the only traditional read-write fs on the disk;
`/boot` is placer-owned. Persistence is **default-deny**: a typed
persistent-paths allowlist, each entry bind-mounted from `td-state` at boot,
tiered **precious** (`td-state/state/…` — identity, backup-worthy) vs
**disposable** (`td-state/cache/…` — logs, container images). `/home` =
`td-state/home`. `/etc` is never persistent and never merged.

Mechanics for M10.3:

- The test harness creates the `td-state` filesystem when it builds the
  two-generation disk; the placer never touches it (place and prune operate only
  on `/boot` and generation roots).
- Typed config grows the allowlist field; the compiler emits bind-mount
  `file-system` entries (device = path on `td-state`, `bind-mount` flag) plus an
  activation step that creates the backing dirs. Generation-mode only:
  `generation #f` converges to the frozen oracle unchanged.
- First entry: SSH host keys, relocated declaratively (openssh `HostKey` under
  `/var/lib/ssh`) so rollback cannot change machine identity.
- Acceptance strengthens both ways: a sentinel written under a declared path in
  gen N survives the rollback reboot into gen N−1; a sentinel under an undeclared
  path does not appear in N−1. Verified-red on both (drop the bind mount; write
  the undeclared sentinel into the shared fs instead).

Staging honesty: until M11, "lost on swap" is approximate — an undeclared write
lingers inside that generation's ext4 root (invisible after a swap, back if you
roll back into that gen) until pruned. The crisp form — tmpfs `/`, generation
image mounted read-only, activation assembling `/etc`/`/run`/`/tmp` — lands with
M11's sealing, which turns default-deny persistence into a kernel-enforced
fail-closed property (EROFS on undeclared writes). A root the boot path writes to
cannot be sealed, so tmpfs-root and verity are one move, not two.

## Still open — answer each when its rung needs it, with a test
- Unprivileged build (user-namespace guix-daemon vs. building inside the container
  we already use): deferred to the `rootless-builder` side-track —
  `plan/rootless-builder.md` owns the question now.

## Milestone ladder (all landed — records in `HISTORY.md`)
- [x] **M10.1** — build a reproducible bootc-style td image (the generation bundle), AND
  define the per-generation root: a distinct root artifact + how the GRUB entry/initrd
  selects it, replacing today's fixed `td-root` label. The boot path must mount *this*
  generation's root, not a shared one — that's what makes rollback real.
- [x] **M10.2** — guix-free place + GRUB menu update + prune, on the target.
- [x] **M10.3** — manual-rollback test: boot generation N, boot N-1 from the menu, assert.

Verified generations *(mechanism settled 2026-06-10 — normative in DESIGN §7.1 M11)*:
**dm-verity over the per-generation root image**. What M10.3 should know now:

- The per-gen ext4 image survives M11 unchanged — it becomes the verity data device,
  mounted read-only. Build it exactly as planned.
- Keep root *selection* thin: `root=td-root-gen-N` (the bare-label spec Guix's
  initrd parses — the M10.3 spike killed the dracut-style `LABEL=` spelling) is
  replaced at M11 by `root=/dev/dm-0 dm-mod.create=…` (root hash on the cmdline),
  so don't grow label plumbing beyond what the rollback test needs.
- Nothing may write into the root at first boot — the image must be sealable
  (the §2.6 "tmpfs-root and verity are one move" point).
- Cheap first rung when M11 starts: confirm the pinned linux-libre config has
  `CONFIG_DM_VERITY` (and `CONFIG_DM_INIT` for initrd-less assembly).

Tooling is already in the pin (`veritysetup` via cryptsetup; `erofs-utils` if the
image format is ever swapped). composefs is **not** in the pin and would replace,
not extend, the per-generation-image design — re-parked for if/when dedup earns
its place. Auto-rollback stays deferred.
