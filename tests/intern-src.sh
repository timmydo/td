#!/bin/sh
# intern-src.sh — intern a source TREE into a td-OWNED store with td's OWN recursive
# addToStore (`td-builder store-add-recursive`), NO `guix repl` / NO guix-daemon. This
# is the source PREP that the rust-build / rust-vendor / rust-russh gates do; it
# replaces the retired Guile helpers tests/td-builder-source.scm,
# td-vendor-demo-source.scm and td-russh-demo-source.scm, whose `guix repl …
# lower-object` made the daemon intern the live tree (move-off-Guile §5). td computes
# the content-addressed `source` path from the tree's recursive NAR sha256 ITSELF and
# restores it into its own store dir + db.
#
# Like the .scm's local-file `#:select?`, the named build dirs (e.g. target, .cargo)
# are dropped so a stray local build cannot perturb the source hash; every other entry
# — dotfiles such as .gitignore included — is kept. `cp -a` preserves the NAR-relevant
# properties (contents, the executable bit, symlinks), so td's path matches the
# daemon's for the same tree (the gate-285 differential).
#
# Prints three shell-eval-able assignments for the gate to feed `build-recipe`:
#     src=<canonical /gnu/store path>   srcstore=<td store dir>   srcdb=<td db>
# Usage: intern-src.sh TB NAME SRC-TREE WORKDIR [EXCLUDE-BASENAME...]
set -eu

tb=$1; name=$2; tree=$3; work=$4
shift 4

clean="$work/srctree"
store="$work/srcstore"
db="$work/src.db"
rm -rf "$clean" "$store" "$db"
mkdir -p "$clean" "$store"

# Clean-copy the tree, dropping the excluded top-level basenames.
for e in "$tree"/* "$tree"/.*; do
  [ -e "$e" ] || continue
  b=$(basename "$e")
  case "$b" in
    .|..) continue ;;
  esac
  skip=
  for x in "$@"; do
    if [ "$b" = "$x" ]; then skip=1; fi
  done
  if [ -n "$skip" ]; then continue; fi
  cp -a "$e" "$clean/"
done

src=$("$tb" store-add-recursive "$name" "$clean" "$store" "$db") || exit 1
if [ -z "$src" ]; then
  echo "intern-src: store-add-recursive produced no path" >&2
  exit 1
fi

echo "src='$src'"
echo "srcstore='$store'"
echo "srcdb='$db'"
