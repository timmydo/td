# system-image-native — td constructs the OCI/disk image, not `(gnu system image)`

## Why

The *package* build path is already Guile-free: `td-builder build-recipe`
(`assemble_recipe_drv`) resolves every input from a lock, assembles the `.drv` in
Rust, and realizes it with guix/Guile scrubbed from PATH (gate `220-corpus-no-guix`).
The genuinely-remaining Guile **build** lowering is the **system-image** side —
`guix system image -t docker system/td.scm` and `(gnu system image)`, used by the
`oci` (120), `generation-image` (105), `registry` (140), `place` (160), `oci-load`
(135) gates. This track replaces that construction with a td-native builder.

This is north-star **priority 3** ("oracle/lowering retired last") — the hardest and
last. The toolchain/kernel **bytes** stay guix (retired even later, the `/td/store`
source-bootstrap tracks). What moves here is the IMAGE CONSTRUCTION (Guile → Rust).

## Target format (cracked from a real guix `-t docker` image)

Docker-archive v1.x (`docker save` layout), skopeo reads it via `docker-archive:PATH`,
crun runs it; guix gzips it but skopeo also accepts an **uncompressed** tar — and
td-builder is **zero-dep** (no gzip crate), so the td packer emits an uncompressed
`.tar`:

```
manifest.json   [{"Config":"config.json","RepoTags":["td:latest"],"Layers":["<id>/layer.tar"]}]
config.json     {"architecture":"amd64","os":"linux","created":"1970-01-01T00:00:01Z",
                 "config":{"env":[...],"entrypoint":[...]},"container_config":null,
                 "history":[...],"rootfs":{"type":"layers","diff_ids":["sha256:<H>"]}}
<id>/VERSION    1.0
<id>/json       {"id":"<id>","created":"1970-01-01T00:00:01Z","container_config":null}
<id>/layer.tar  the rootfs tar (gnu/store/…, etc/, var/guix/gcroots/…)
repositories    {"td":{"latest":"<id>"}}
```

`diff_ids[0] = sha256(layer.tar)`. `<id>` is a v1 chain-id label; skopeo recomputes
digests from the bytes, so a deterministic `<id>` (e.g. derived from the layer hash)
is fine. Everything normalized (mtime, uid/gid, sorted entries) for reproducibility.

## Brick ladder

1. **oci.rs packer (this PR).** `builder/src/oci.rs`: a deterministic ustar writer
   (sorted, mtime=1, uid/gid=0, GNU longname for >100-char store paths) + a
   `write_docker_archive(layer_root, image_config) -> bytes` that lays the given
   rootfs dir into `layer.tar`, computes `diff_id`, and emits the archive above. A
   `td-builder oci-image <rootfs-dir> <config-json> <out.tar>` subcommand. Unit tests
   (cargo-test / check-engine smoke): the archive has the right members; the
   ustar headers round-trip; `config.rootfs.diff_ids[0] == sha256(layer.tar)`;
   manifest references config + layer; byte-identical determinism. NO guix involved.
2. **Closure → rootfs.** Lay a store closure (the `guix gc -R` set) into the layer
   rootfs (store paths + `/var/guix/gcroots`), so td packs a real image.
3. **Gate (durable + oracle).** A gate that packs a real image with td, then:
   DURABLE — skopeo `copy docker-archive:` loads it, crun runs its userspace,
   `td-builder check` double-build is byte-identical (intrinsic repro); OCI structure
   self-consistent; a perturbed input diverges. REMOVABLE ORACLE — diff vs
   `guix system image -t docker` (label the byte-identity leg as the migration
   oracle, deletable when guix retires).
4. Generation-image (/boot layer), then registry (OCI layout + signing), then the
   full OS image — each swapping its `(gnu system image)` lowering onto the td packer.

## Status / evidence

- **Brick 1 — DONE (PR #233).** `builder/src/oci.rs` + `td-builder oci-image`. Verified:
  4 unit tests (members, `diff_id==sha256(layer.tar)`, byte-determinism, `@LongLink`
  roundtrip), full engine suite 93/0, and END-TO-END skopeo `copy docker-archive:` +
  `inspect` loads a td-built image (foreign OCI impl accepts td's zero-guix bytes).
  `td-builder affected-checks --committed-only --run` → exit 0, full `./check.sh` waived
  (builder/src/* → check-engine smoke). NOTE: this is the primitive; it does not yet
  replace a guix call in a gate (brick 3 does).
- **Brick 2 — DONE.** `build_layer_tar_from_store_paths` + `td-builder oci-image-closure`
  pack a real store CLOSURE (`Db::closure`, no guix process) into the image. +1 unit
  test; END-TO-END on real /var/guix/db data (skopeo loaded a 22-path hello closure).
- **Brick 3 — DONE (the "simple example").** `mk/gates/118-oci-native.mk` +
  `tests/oci-native-check.sh`: td builds a docker-archive from hello's closure (no guix
  system image), and DURABLE assertions prove it works — skopeo `copy docker-archive:`
  loads it, **crun runs hello → "Hello, world!"**, INTRINSIC byte-reproducibility (same
  closure packs to the same sha256, no guix oracle), self-discriminating negative exec.
  `./check.sh oci-native` GREEN. (Gotcha: the sandbox has no diffutils → compare with
  `sha256sum`, not `cmp`.) The package/toolchain bytes stay guix (retired last).
- NEXT (separate, large PR): RIP OUT the guix-system-image gates/tests/system files
  (oci/oci-diff/oci-load/generation-*/manifest-*/registry/verify-place/place/rollback/
  container/run/rootless/boot/reset + their tests/*.scm + system/td*.scm image lowering)
  — destructive + spine-touching (check.sh/Makefile/DIGESTS/CI), surfaced for sign-off.
- Mid-track env change: `tools/affected-checks.sh` was replaced on main by the Rust
  `td-builder affected-checks` subcommand — validation uses that now.
