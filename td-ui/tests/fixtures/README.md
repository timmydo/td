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
these fixture-derived bindings to the isolated type resolver. The complete
keyboard compiler independently derives them from compatibility and modifier
maps. Separate tests deliberately move shortcut masks to ensure the compiler
does not guess their encodings.

## Independent key oracle

`us-keys.tsv` records libxkbcommon 1.13.1 results for 106 XKB keycodes:
9 through 91, 94 through 96, 104 through 119, 125, 127, 133 and 134.
Each line is the decimal XKB code, a tab, and a 64-bit FNV-1a digest.
Compile the unchanged `us.xkb` with `xkb_keymap_new_from_string`, text
format 1, no flags, and create an `xkb_state`. For each key, in ascending
mask order 0 through 31 (Shift, Lock, Control, Mod1 and Mod2), call
`xkb_state_update_mask(state, mask, 0, 0, 0, 0, 0)`. Hash these four values
as little-endian u32, using the offset/multiplier above:

1. `xkb_state_key_get_level(state, code, 0)`.
2. `xkb_state_key_get_one_sym(state, code)`.
3. `xkb_state_key_get_consumed_mods2(state, code, XKB_CONSUMED_MODE_XKB)`.
4. `xkb_keymap_key_repeats(keymap, code)` (0 or 1).

Normalize only XF86 values `0x10080000..=0x1008ffff` to `0xffffffff` in
step 2: the editor retains those names without a numeric vocabulary and
ignores their events. Tests separately require those named results to start
with XF86 and produce no chord. No other unknown symbol is normalized.
This covers ordinary text, modifiers, keypad, navigation and function keys,
not every extra key declared by evdev. No libxkbcommon library is linked or
invoked by the tests. Additional direct reference probes pin NoSymbol's
absence of implicit repeat and interpretation matching, post-type Lock
capitalization, interpretation predicate priority and first-declared ties.
Higher-level `useModMapMods=level1` probes use `xkb_state_update_key` after
setting Shift: AnyOfOrNone/NoneOf still supply the declared modifier action
while adding no virtual binding. An explicit Mod3 action sets Mod3; a
modMapMods operand uses the key's actual map. Additional probes cover
indirect modmap lookup after type truncation, trailing NoSymbol trimming,
swapped-case TWO_LEVEL inference and AnyOfOrNone on a disjoint real mask.
