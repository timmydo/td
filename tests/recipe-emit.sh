#!/bin/sh
# tests/recipe-emit.sh STEM — emit a recipe as JSON.
#
# td's package surface is declared in Rust (recipes/); this dispatches to
# `td-recipe-eval` (TD_RECIPE_EVAL), the dependency-free evaluator, which emits
# the JSON the build path consumes. Replaces the boa+tsgo `ts-emit.sh`
# (rust-recipe-surface): no transpile, no JS engine.
#
#   recipe-emit.sh hello          -> td-recipe-eval emit hello
#
# Input: TD_RECIPE_EVAL (set by cache-lib's load_recipe_eval / the build-recipes
# prelude). JSON to stdout.
set -eu

: "${TD_RECIPE_EVAL:?TD_RECIPE_EVAL (the td-recipe-eval binary) must be set}"
test -x "$TD_RECIPE_EVAL" || { echo "recipe-emit: $TD_RECIPE_EVAL is not executable" >&2; exit 1; }

exec "$TD_RECIPE_EVAL" emit "${1:?usage: recipe-emit.sh STEM}"
