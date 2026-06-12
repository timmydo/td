#!/bin/sh
# tests/registry-check.sh — M12 S3 registry artifact validation.
#
# The Makefile `registry` rung builds + `--check`s the signed static registry
# (system/td-registry.scm) and hands it to THIS script. We assert the
# distribution contract (DESIGN §2.7) per pushed generation:
#
#   (1) STATEMENT  — the registry carries signatures/<hex>.digest whose single
#       line equals the manifest digest skopeo (the FOREIGN OCI
#       implementation, the oracle for "what the layout says") re-derives for
#       oci:…:gen-N — and <hex> is that digest, so statements are addressable
#       by the identity they state.
#   (2) SIGNATURE  — <hex>.digest.sig is a signify detached signature over the
#       statement that verifies with the committed td TEST pubkey.
#   (3) PULL       — pull-by-digest works from the bytes alone, no skopeo: the
#       manifest blob lives at oci/blobs/sha256/<hex> and re-hashes to <hex>,
#       and every blob it references ("digest":"sha256:…" — config + layers)
#       is present and re-hashes to its name. Content addressing makes that
#       byte-identity between pushed and pulled.
#   (4) STORE      — EVERY blob in oci/blobs/sha256 re-hashes to its name (no
#       unreferenced corrupt content either).
#
# Negative controls, run EVERY loop on scratch copies (the verifier is
# verify_pull below — the same contract S4's placer enforces before placing):
#
#   (n1) UNSIGNED  — signatures stripped: verification must FAIL (reason:
#        missing signature).
#   (n2) TAMPERED  — one byte of a layer blob flipped: the pull walk must
#        FAIL (reason: blob does not re-hash to its digest).
#   (n3) FORGED    — statement altered (different digest, signature left in
#        place): signify verification must FAIL.
#
# Env: TD_REGISTRY (the registry store path), SKOPEO, SIGNIFY (tool paths),
# TD_PUBKEY (the committed test pubkey), TD_GENS (space-sep generations).
# Exits non-zero on any failure.
set -eu

reg=${TD_REGISTRY:?}; skopeo=${SKOPEO:?}; signify=${SIGNIFY:?}
pub=${TD_PUBKEY:?}; gens=${TD_GENS:?}

failures=0
fail() { echo "FAIL: $*"; failures=$((failures + 1)); }

scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT

sha256_of() { # FILE -> hex on stdout
  set -- "$(sha256sum "$1")"; printf '%s\n' "${1%% *}"
}

# verify_pull REGISTRY DIGEST — the pull-side §2.7 contract, from the bytes
# alone (sha256sum + the pubkey; no skopeo): the statement for DIGEST exists,
# its signify signature verifies, its content IS the digest, the manifest blob
# re-hashes to it, and every referenced blob is present and re-hashes to its
# name. Exit 0 iff ALL hold; reasons go to stdout. S4's placer enforces this
# same contract before placing.
verify_pull() {
  r=$1; d=$2; hex=${d#sha256:}
  stmt="$r/signatures/$hex.digest"
  [ -f "$stmt" ] || { echo "   no statement for $d"; return 1; }
  [ -f "$stmt.sig" ] || { echo "   no signature for $d"; return 1; }
  "$signify" -V -q -p "$pub" -m "$stmt" -x "$stmt.sig" 2>&1 \
    || { echo "   signature verification failed for $d"; return 1; }
  [ "$(cat "$stmt")" = "$d" ] \
    || { echo "   statement content does not state $d"; return 1; }
  mf="$r/oci/blobs/sha256/$hex"
  [ -f "$mf" ] || { echo "   no manifest blob for $d"; return 1; }
  [ "$(sha256_of "$mf")" = "$hex" ] \
    || { echo "   manifest blob does not re-hash to $d"; return 1; }
  refs=$(tr -d ' \n\t' < "$mf" \
    | grep -o '"digest":"sha256:[0-9a-f]\{64\}"' \
    | sed 's/^.*sha256://; s/"$//')
  [ -n "$refs" ] || { echo "   manifest for $d references no blobs"; return 1; }
  for bh in $refs; do
    bf="$r/oci/blobs/sha256/$bh"
    [ -f "$bf" ] || { echo "   referenced blob $bh missing"; return 1; }
    [ "$(sha256_of "$bf")" = "$bh" ] \
      || { echo "   blob $bh does not re-hash to its digest"; return 1; }
  done
  return 0
}

echo
echo "== M12 S3 registry validation =="
echo "  registry=$reg  gens=$gens"

# --- (1)-(3) per pushed generation -----------------------------------------
first_digest=
for g in $gens; do
  # The FOREIGN implementation's reading of the layout is the expected value.
  d=$("$skopeo" --tmpdir "$scratch" inspect --format '{{.Digest}}' \
        "oci:$reg/oci:gen-$g") || d=
  case "$d" in
    sha256:*) : ;;
    *) fail "generation $g: skopeo derives no manifest digest from the layout (got '$d')"
       continue ;;
  esac
  [ -n "$first_digest" ] || first_digest=$d
  if out=$(verify_pull "$reg" "$d"); then
    echo "   gen-$g verifies: $d"
  else
    fail "generation $g: registry does not verify for $d:"
    printf '%s\n' "$out"
  fi
done

# --- (4) the whole blob store is content-addressed honestly -----------------
for bf in "$reg"/oci/blobs/sha256/*; do
  bh=$(basename "$bf")
  [ "$(sha256_of "$bf")" = "$bh" ] \
    || fail "blob store: $bh does not re-hash to its name"
done

# --- negative controls (on scratch copies; gen = first pushed) --------------
if [ -n "$first_digest" ]; then
  hex=${first_digest#sha256:}

  # (n1) UNSIGNED: strip the signatures — must be rejected, for that reason.
  cp -r "$reg" "$scratch/unsigned"; chmod -R u+w "$scratch/unsigned"
  rm -f "$scratch/unsigned/signatures/"*.sig
  if out=$(verify_pull "$scratch/unsigned" "$first_digest"); then
    fail "negative control n1: an UNSIGNED registry was accepted"
  elif ! printf '%s\n' "$out" | grep -q "no signature"; then
    fail "negative control n1: unsigned registry rejected, but not for the missing signature:"
    printf '%s\n' "$out"
  fi

  # (n2) TAMPERED: flip one byte mid-way through the LARGEST referenced blob
  # (a layer, not the manifest) — the pull walk must catch it, as a re-hash
  # mismatch.
  cp -r "$reg" "$scratch/tampered"; chmod -R u+w "$scratch/tampered"
  victim=$(ls -S "$scratch/tampered/oci/blobs/sha256" | head -n 1)
  vf="$scratch/tampered/oci/blobs/sha256/$victim"
  off=$(( $(stat -c %s "$vf") / 2 ))
  b=$(od -An -tu1 -j "$off" -N1 "$vf" | tr -d ' ')
  printf "\\$(printf '%03o' $(( (b + 1) % 256 )))" \
    | dd of="$vf" bs=1 seek="$off" count=1 conv=notrunc status=none
  if out=$(verify_pull "$scratch/tampered" "$first_digest"); then
    fail "negative control n2: a TAMPERED blob ($victim) was accepted"
  elif ! printf '%s\n' "$out" | grep -q "does not re-hash"; then
    fail "negative control n2: tampered registry rejected, but not as a blob re-hash mismatch:"
    printf '%s\n' "$out"
  fi

  # (n3) FORGED: rewrite the statement (signature left in place) — signify
  # must refuse it.
  cp -r "$reg" "$scratch/forged"; chmod -R u+w "$scratch/forged"
  forged=$(printf '%s\n' "$hex" | tr '0123456789abcdef' '123456789abcdef0')
  printf 'sha256:%s\n' "$forged" > "$scratch/forged/signatures/$hex.digest"
  if out=$(verify_pull "$scratch/forged" "$first_digest"); then
    fail "negative control n3: a FORGED statement was accepted"
  elif ! printf '%s\n' "$out" | grep -q "signature verification failed"; then
    fail "negative control n3: forged statement rejected, but not by the signature check:"
    printf '%s\n' "$out"
  fi
else
  fail "no pushed generation verified — negative controls could not run"
fi

if [ "$failures" -eq 0 ]; then
  echo "PASS: every pushed generation's manifest digest is stated, signify-signed, and pull-by-digest verifies from the bytes alone (statement, signature, manifest + referenced blobs re-hash); the whole blob store is honestly content-addressed; unsigned, tampered and forged variants are rejected, each for its own reason."
  exit 0
else
  echo "$failures check(s) failed."
  exit 1
fi
