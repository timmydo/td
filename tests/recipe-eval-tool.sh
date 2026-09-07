#!/bin/sh
# Compatibility entry point; td-builder owns evaluator preparation and caching.
set -eu
base="${1:?usage: recipe-eval-tool.sh BASEDIR}"
case "$base" in
    /*) ;;
    *) base="$(pwd)/$base" ;;
esac
root=$(cd "$(dirname "$0")/.." && pwd)
td="${TD_BUILDER_SELF:?recipe-eval-tool requires TD_BUILDER_SELF}"
case "$td" in
    /*) ;;
    */*) td="$(pwd)/$td" ;;
esac
cd "$root"
exec "$td" recipe-eval-place "$base"
