# XKB test data

`us.xkb` is a complete compiled text-v1 map, not an abbreviated hand-written
US key-position table. It was generated with libxkbcommon 1.13.1 and
xkeyboard-config 2.44, using these explicit inputs:

```text
xkbcli compile-keymap --include /path/to/xkeyboard-config-2.44/share/X11/xkb --rules evdev --model pc105 --layout us --options '' --output-format 1
```

The output's trailing blank line is removed, leaving exactly one final LF.
Its SHA-256 is
`4c059a0715a6e211b2bd21acba102b6ae91b667bd087a6bd9e508ecfbcc98218`.
The complete upstream xkeyboard-config `COPYING` is retained in
`XKB-COPYING`; generation expands the included definitions without changing
their key assignments. These are test data, never shipped or loaded by the
editor executable. No external library, data package or generator is needed
to build or run the tests.

This is the evdev/pc105/US rules configuration for testing the ordinary-US
compatibility target, not a map captured from a live Weston input event.
Neither this fixture nor type-table tests claim live keyboard compatibility.
The separate Weston input/pixel acceptance test remains required.

## Independent type oracle

`us-types.tsv` records results obtained from libxkbcommon 1.13.1, not from
td-editor. Each line is a type name, a tab, and a 64-bit FNV-1a digest. To
reproduce it with libxkbcommon's public API:

1. For each of the 26 type declarations in `us.xkb`, make a fresh copy of the
   map. Replace only the `<AC01>` symbol definition with
   `key <AC01> { type="TYPE_NAME", [a,b,c,d,e,f,g,h,i,j,k,l,m,n,o,p] };`.
   The sixteen distinct symbols avoid implicit type inference. Explicit type
   selection retains the declared type's actual level count.
2. Compile with `xkb_keymap_new_from_string`, text format 1, no flags, then
   create an `xkb_state`. Use a new keymap/state for each type.
3. In ascending order for masks 0 through 255, call
   `xkb_state_update_mask(state, mask, 0, 0, 0, 0, 0)`. Obtain the zero-based
   level from `xkb_state_key_get_level(state, 38, 0)` and consumed mask from
   `xkb_state_key_get_consumed_mods2(state, 38, XKB_CONSUMED_MODE_XKB)`.
4. Hash each level followed by its consumed mask as four-byte little-endian
   unsigned integers, with offset `0xcbf29ce484222325` and multiplier
   `0x100000001b3`, wrapping modulo 2^64. Write sixteen lowercase hex digits.

The real encodings reported by `xkbcli compile-keymap --from-xkb us.xkb
--modmaps` are NumLock=16, Alt=8, LevelThree=128, Super=64, LevelFive=32,
Meta=8 and Hyper=32; ScrollLock has explicit encoding 32768. Tests supply
these fixture-derived bindings to the type resolver. Deriving them from
compatibility interpretations and modifier assignments is not implemented
by this increment. Separate tests deliberately move Alt and NumLock to
different masks to ensure the resolver does not guess their encodings.
