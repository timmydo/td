#!/bin/sh
# tests/verify-place-check.sh — M12 S4 verify-then-place rejection + differential legs.
#
# The Makefile `verify-place` rung builds the verified placed tree (the accept
# leg — validated separately by tests/place-check.scm with digest-form
# TD_IMAGES) and hands THIS script the pieces for the rest of the S4 contract:
#
#   (d) DIFFERENTIAL — the verified tree equals the direct-placement oracle
#       tree EXCEPT the per-generation td-identity files, whose ONLY
#       difference is the image-digest value: artifact sha256 (direct) vs the
#       verified manifest digest (registry mode). Same bytes placed, only the
#       §2.7 identity representation moved.
#
# Negative controls, run EVERY loop — the placer itself (run here against
# mutated scratch copies of the registry) must REFUSE each before placing
# anything (no generation dir appears), each for its own reason (§2.7: reject
# exactly unsigned, bad signature, digest mismatch):
#
#   (n1) UNSIGNED       — signatures stripped → "no signature".
#   (n2) BAD SIGNATURE  — statement re-stated for a different digest,
#        signature left → "signature verification failed". (The placer is
#        still asked for the ORIGINAL digest, so the statement file content
#        check would also catch it — signify must refuse FIRST.)
#   (n3) DIGEST MISMATCH — a referenced layer blob tampered (one byte) →
#        "does not re-hash".
#   (n4) FORGED EMBEDDED IDENTITY (the S1 §2.7 self-reference guard) — a
#        crafted LEGACY docker-archive whose embedded boot/td-identity already
#        carries an image-digest= line must be rejected by --image placement.
#
# Env: TD_REGISTRY (the registry store path), TD_PLACER (system/td-place.sh),
# TD_PUBKEY, SIGNIFY_BIN (dir holding signify, prepended to PATH),
# TD_DIGEST_1 (gen-1's manifest digest), TD_GEN1_IMG (gen-1 docker-archive,
# for n4), TD_GEN1_LABEL (gen-1's root label), TD_VPLACE / TD_DIRECT (the two
# placed trees, for the differential). Exits non-zero on any failure.
set -eu

reg=${TD_REGISTRY:?}; placer=${TD_PLACER:?}; pub=${TD_PUBKEY:?}
sigbin=${SIGNIFY_BIN:?}; d1=${TD_DIGEST_1:?}; gen1img=${TD_GEN1_IMG:?}
gen1label=${TD_GEN1_LABEL:?}; vplace=${TD_VPLACE:?}; direct=${TD_DIRECT:?}

PATH="$sigbin:$PATH"; export PATH

failures=0
fail() { echo "FAIL: $*"; failures=$((failures + 1)); }

scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT

echo
echo "== M12 S4 verify-then-place validation =="
echo "  registry=$reg"
echo "  verified=$vplace"
echo "  direct  =$direct"

# --- (d) differential: verified placement == direct placement, except the ---
# --- image-digest representation inside the per-generation td-identity.   ---
# No diffutils in the loop sandbox: compare the path sets (find) and per-file
# sha256 instead — file-level equality is exactly what the leg asserts anyway.
paths_a=$( (cd "$direct" && find . ! -type d | sort) )
paths_b=$( (cd "$vplace" && find . ! -type d | sort) )
if [ "$paths_a" != "$paths_b" ]; then
  fail "differential: the two trees do not even contain the same paths"
else
  moved=0
  for f in $paths_a; do                 # placed-tree paths carry no spaces
    a="$direct/$f"; b="$vplace/$f"
    case "$f" in
      */td-identity)
        [ "$(grep -v '^image-digest=' "$a")" = "$(grep -v '^image-digest=' "$b")" ] \
          || fail "differential: $f differs beyond the image-digest line"
        da=$(sed -n 's/^image-digest=//p' "$a")
        db=$(sed -n 's/^image-digest=//p' "$b")
        [ "$da" != "$db" ] \
          || fail "differential: $f image-digest did not change representation ($da)"
        moved=$((moved + 1)) ;;
      *)
        ha=$(sha256sum "$a"); hb=$(sha256sum "$b")
        [ "${ha%% *}" = "${hb%% *}" ] \
          || fail "differential: trees differ at $f beyond the td-identity files" ;;
    esac
  done
  [ "$moved" -eq 2 ] \
    || fail "differential: expected 2 per-generation td-identity files, saw $moved"
  if [ "$failures" -eq 0 ]; then
    echo "   differential holds: identical trees, only the image-digest representation moved"
  fi
fi

# run_placer REGISTRY DIGEST — verified-mode placement into a fresh scratch
# target (named after the registry copy, unique per control); echoes the
# placer's output. Returns the placer's status.
run_placer() {
  t="$scratch/t-$(basename "$1")"; rm -rf "$t"; mkdir -p "$t/boot/grub" "$t/roots"
  printf 'set timeout=5\n' > "$t/boot/grub/grub.cfg"
  out=$(sh "$placer" \
        --registry "$1" --digest "$2" --pubkey "$pub" \
        --generation 1 --root-label "$gen1label" \
        --boot-dir "$t/boot" --root-store "$t/roots" \
        --grub-cfg "$t/boot/grub/grub.cfg" --keep 10 2>&1) && st=0 || st=$?
  printf '%s\n' "$out"
  # Refusal must leave NOTHING placed (crash-safe staging).
  if [ "$st" -ne 0 ] && [ -e "$t/boot/td/gen-1" ]; then
    echo "   PLACED-DESPITE-REFUSAL"
  fi
  return "$st"
}

hex1=${d1#sha256:}

# (n1) UNSIGNED: signatures stripped — placer must refuse, for that reason.
cp -r "$reg" "$scratch/unsigned"; chmod -R u+w "$scratch/unsigned"
rm -f "$scratch/unsigned/signatures/"*.sig
if out=$(run_placer "$scratch/unsigned" "$d1"); then
  fail "negative control n1: the placer ACCEPTED an unsigned registry"
elif ! printf '%s\n' "$out" | grep -q "no signature"; then
  fail "negative control n1: unsigned refused, but not for the missing signature:"
  printf '%s\n' "$out"
elif printf '%s\n' "$out" | grep -q "PLACED-DESPITE-REFUSAL"; then
  fail "negative control n1: the placer refused but STILL placed gen-1"
fi

# (n2) BAD SIGNATURE: statement rewritten (signature left in place) — signify
# must refuse it.
cp -r "$reg" "$scratch/badsig"; chmod -R u+w "$scratch/badsig"
forged=$(printf '%s\n' "$hex1" | tr '0123456789abcdef' '123456789abcdef0')
printf 'sha256:%s\n' "$forged" > "$scratch/badsig/signatures/$hex1.digest"
if out=$(run_placer "$scratch/badsig" "$d1"); then
  fail "negative control n2: the placer ACCEPTED a forged statement"
elif ! printf '%s\n' "$out" | grep -q "signature verification failed"; then
  fail "negative control n2: forged statement refused, but not by the signature check:"
  printf '%s\n' "$out"
elif printf '%s\n' "$out" | grep -q "PLACED-DESPITE-REFUSAL"; then
  fail "negative control n2: the placer refused but STILL placed gen-1"
fi

# (n3) DIGEST MISMATCH: tamper one byte of the largest blob gen-1's manifest
# references — the placer's pull walk must catch the re-hash mismatch.
cp -r "$reg" "$scratch/tampered"; chmod -R u+w "$scratch/tampered"
walked_refs=$(tr -d ' \n\t' < "$scratch/tampered/oci/blobs/sha256/$hex1" \
  | grep -o '"digest":"sha256:[0-9a-f]\{64\}"' \
  | sed 's/^.*sha256://; s/"$//')
victim=$(cd "$scratch/tampered/oci/blobs/sha256" && ls -S $walked_refs | head -n 1)
vf="$scratch/tampered/oci/blobs/sha256/$victim"
off=$(( $(stat -c %s "$vf") / 2 ))
b=$(od -An -tu1 -j "$off" -N1 "$vf" | tr -d ' ')
printf "\\$(printf '%03o' $(( (b + 1) % 256 )))" \
  | dd of="$vf" bs=1 seek="$off" count=1 conv=notrunc status=none
if out=$(run_placer "$scratch/tampered" "$d1"); then
  fail "negative control n3: the placer ACCEPTED a tampered blob ($victim)"
elif ! printf '%s\n' "$out" | grep -q "does not re-hash"; then
  fail "negative control n3: tampered blob refused, but not as a re-hash mismatch:"
  printf '%s\n' "$out"
elif printf '%s\n' "$out" | grep -q "PLACED-DESPITE-REFUSAL"; then
  fail "negative control n3: the placer refused but STILL placed gen-1"
fi

# (n4) FORGED EMBEDDED IDENTITY: craft a legacy docker-archive whose embedded
# boot/td-identity already carries image-digest= — the §2.7 self-reference
# guard must refuse it (an image cannot state its own digest). The docker
# manifest does not bind layer hashes for the placer, so a plain repack works.
c="$scratch/crafted"; mkdir -p "$c/img"
tar xzf "$gen1img" -C "$c/img"
bl=
for lt in $(tr -d '\n' < "$c/img/manifest.json" \
              | sed -n 's/.*"Layers":\[\([^][]*\)\].*/\1/p' \
              | tr ',' '\n' | sed -n 's/^[[:space:]]*"\(.*\)"[[:space:]]*$/\1/p'); do
  if tar tf "$c/img/$lt" | grep -Eq '^(\./)?boot/td-identity$'; then
    bl=$lt; break
  fi
done
[ -n "$bl" ] || { fail "negative control n4: could not locate the boot layer in $gen1img"; bl=; }
if [ -n "$bl" ]; then
  mkdir -p "$c/layer"
  tar xf "$c/img/$bl" -C "$c/layer"
  chmod -R u+w "$c/layer"
  printf 'image-digest=sha256:0000000000000000000000000000000000000000000000000000000000000000\n' \
    >> "$c/layer/boot/td-identity"
  # Preserve the original member naming (boot/..., no ./ prefix) — the placer
  # extracts exactly `boot/td-identity` from the layer.
  (cd "$c/layer" && tar -cf "../img/$bl" boot)
  (cd "$c/img" && tar -czf "../crafted.tar.gz" .)
  t="$scratch/t-crafted"; mkdir -p "$t/boot/grub" "$t/roots"
  printf 'set timeout=5\n' > "$t/boot/grub/grub.cfg"
  if out=$(sh "$placer" --image "$c/crafted.tar.gz" \
             --generation 1 --root-label "$gen1label" \
             --boot-dir "$t/boot" --root-store "$t/roots" \
             --grub-cfg "$t/boot/grub/grub.cfg" --keep 10 2>&1); then
    fail "negative control n4: the placer ACCEPTED an image whose embedded identity states its own digest"
  elif ! printf '%s\n' "$out" | grep -q "cannot state its own digest"; then
    fail "negative control n4: crafted image refused, but not by the self-reference guard:"
    printf '%s\n' "$out"
  elif [ -e "$t/boot/td/gen-1" ]; then
    fail "negative control n4: the placer refused but STILL placed gen-1"
  fi
fi

if [ "$failures" -eq 0 ]; then
  echo "PASS: verified placement equals direct placement except the §2.7 image-digest representation; the placer refuses unsigned, forged-statement, and tampered registries (each for its own reason, placing nothing) and rejects an image that states its own digest."
  exit 0
else
  echo "$failures check(s) failed."
  exit 1
fi
