# td UI

`td-compositor` is td's dependency-free, software-rendered Wayland server.
td's native terminal, `td-term`, is a separate program on the shared td-ui
client toolkit; [`td-term/DESIGN.md`](../td-term/DESIGN.md) specifies it.

There is no GPU stack, dynamic font system, daemon, plugin language, or
external crate. [`DESIGN.md`](DESIGN.md) is normative.
