# td-picture

The bounded picture path td's page readers share: td-web's navigated
and inline pictures (td-web/DESIGN.md §10), and the pictures a message
carries for its `cid:` URLs in td-mail (td-mail/README.md, "HTML
mail"), at td-mail's limits: a 2 MiB body, 24 MiB of decoding. It holds
no state, starts no thread and fetches nothing: a caller brings a body
and its own `Limits`, and gets a td-ui picture or a refusal.

## The path

1. **Format.** From the body's type (`Format::from_mime`) or its bytes
   (`Format::sniff`): PNG, JPEG, GIF, WebP or AVIF, each through td's
   own decoder crate.
2. **Plan.** The header read before any pixel (`plan`): refused for a
   body past `Limits::body`, a header that does not read, or a size
   td-ui cannot hold at any scale (`td_ui::picture`'s axis and pixel
   bounds); a JPEG too large at its own size is planned at the largest
   of td-jpeg's scales that fits.
3. **Work.** The plan's reckoned working peak (`work`): each decoder's
   largest phase, from its stated layout, the pixels it keeps and the
   tables it keeps whatever the size. td-web/DESIGN.md §10, "Memory",
   records the reckoning's measurement against every decoder (0.08 to
   0.99 of it).
4. **Pixels.** Decoded, kept at its capacity under the charge taken for
   it (`decode_with_budget`, `decode_inline`).

## Limits are the caller's, and bind

`Limits::body` is the largest body read as a picture; `Limits::work` the
most one decoding may hold. A picture whose reckoned peak passes `work`
is refused at its plan (`Error::Work`), before any pixel, so a caller
that charges what `work` returns, as `decode_inline` does, has admitted
everything the decoder will hold. A caller's `work` need not cover every
picture its `body` admits: the pictures past it are refused rather than
decoded beyond their charge. td-web's `PICTURE_WORK` does cover them all
(td-web's `picture::tests::td_webs_costliest_picture_is_admitted` plans
the costliest at its limits), so td-web refuses none this way.

## Boundaries

`std`, td-alloc's budgets, td-ui's picture, td-page's `Resident` (the
charge a kept picture is held under) and the five decoders. No `unsafe`.
New production growth goes through td-alloc (td-alloc/DESIGN.md); the
JPEG RGB-to-RGBA expansion is the one recorded debt row.
