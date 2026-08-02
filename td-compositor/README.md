# td UI

`td-compositor` is td's dependency-free, software-rendered Wayland server.
`td-term` is its planned native terminal: a keyboard-first `wl_shm` client
written in the same Rust multicall.

td-term takes its taste from foot — fast, native, and quiet — but its scope
from td. A pure, bounded byte-stream state machine owns the terminal grid;
PTY, Wayland, and software rendering remain thin adapters. Behavior is defined
by td's attributed native corpus, malformed input is inert, and unsupported
sequences never desynchronize the parser.

There is no toolkit, GPU stack, dynamic font system, daemon, plugin language,
or external crate. [`DESIGN.md`](DESIGN.md) is normative.
