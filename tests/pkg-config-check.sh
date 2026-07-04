#!/bin/sh
# tests/pkg-config-check.sh — behavioral leg for the td-built pkg-config
# (issue #297): prove the tool RESOLVES a .pc file, the thing pkg-config is for
# (not just "it built"). Writes a throwaway foo.pc, points PKG_CONFIG_PATH at it,
# and asserts pkg-config parses it: --modversion yields the Version, --cflags/--libs
# expand the ${prefix} variable and emit the declared flags, and a nonexistent
# module FAILS (self-discrimination — the green tells a working resolver from a
# broken one). No guix, no network.
#
# Usage: pkg-config-check.sh PKG-CONFIG-BIN
set -eu

PC=${1:?usage: pkg-config-check.sh PKG-CONFIG-BIN}
test -x "$PC" || { echo "FAIL: no pkg-config binary at $PC" >&2; exit 1; }

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT INT TERM
cat > "$work/foo.pc" <<'EOF'
prefix=/opt/foo
Name: foo
Description: pkg-config behavioral leg (issue #297)
Version: 1.2.3
Cflags: -I${prefix}/include -DFOO_ENABLED
Libs: -L${prefix}/lib -lfoo
EOF

export PKG_CONFIG_PATH="$work"

mv=$("$PC" --modversion foo) || { echo "FAIL: pkg-config --modversion foo errored" >&2; exit 1; }
test "$mv" = "1.2.3" || { echo "FAIL: pkg-config --modversion foo = '$mv' (want 1.2.3)" >&2; exit 1; }

cf=$("$PC" --cflags foo) || { echo "FAIL: pkg-config --cflags foo errored" >&2; exit 1; }
echo "$cf" | grep -q -- "-I/opt/foo/include" \
  || { echo "FAIL: --cflags did not expand \${prefix}/include (got: '$cf')" >&2; exit 1; }
echo "$cf" | grep -q -- "-DFOO_ENABLED" \
  || { echo "FAIL: --cflags dropped the declared -DFOO_ENABLED (got: '$cf')" >&2; exit 1; }

lb=$("$PC" --libs foo) || { echo "FAIL: pkg-config --libs foo errored" >&2; exit 1; }
echo "$lb" | grep -q -- "-lfoo" \
  || { echo "FAIL: --libs did not emit -lfoo (got: '$lb')" >&2; exit 1; }

# Self-discrimination: a module with no .pc MUST fail (a resolver that says yes to
# everything would pass the positive legs vacuously).
if "$PC" --exists no-such-module 2>/dev/null; then
  echo "FAIL: pkg-config --exists no-such-module SUCCEEDED — the resolver cannot discriminate" >&2
  exit 1
fi

echo "  [DURABLE behavioral] the td-built pkg-config resolved foo.pc: --modversion=1.2.3, --cflags expanded \${prefix} + kept -DFOO_ENABLED, --libs emitted -lfoo; a missing module fails (self-discriminating)"
