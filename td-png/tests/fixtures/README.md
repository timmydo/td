# td-png test fixtures

## `suite/`

Thirty-nine synthetic PNGs that `png_ref.py make suite` writes: each
colour type at each depth the specification allows (grey at 1, 2, 4, 8
and 16 bits, RGB at 8 and 16, palette at 1, 2, 4 and 8, grey with alpha
and RGBA at 8 and 16), plain and Adam7-interlaced (`-i`), at 13x11 so
every pass has pixels and a sub-byte row ends mid-byte; `tRNS` variants
(`-t`) for grey at 2, 8 and 16 bits, RGB at 8 and 16 and a 4-bit
palette, of which `g16-t` and `rgb16-t` are interlaced and carry beside
the key a near miss with the same eight-bit value, and `g8-tm` an 8-bit
key with bits above the depth set, which a decoder masks off; and a 1x1
and a 3x1 interlaced frame, whose later passes are empty. Rows take the
five filter types in turn, the image data is split over three `IDAT`
chunks, and a `gAMA` and a `tEXt` chunk precede it to be skipped. The
pixels come from a fixed LCG and zlib 1.3.1 compressed them at level 9.

## `shapes.png`

`shapes.svg`, a drawing written for this suite, rendered by librsvg
2.58.5's `rsvg-convert -f png` (cairo's PNG writer over libpng 1.6), so
the decoder meets a file another encoder wrote: 96x64 eight-bit RGBA,
not interlaced, one `IDAT` whose rows libpng filtered with None, Sub, Up
and Paeth, and a `bKGD` chunk. 4846 bytes, SHA-256
`96bb94e1991e4c3ba17fdb475d8eb471873885f2cf2ecd7eac0bc7fae6fede50`.

## The oracle

`png_ref.py` is an independent PNG decoder written from the PNG
specification and not from td-png's Rust, inflating with Python's zlib
rather than the engine's inflater. Its expansion is the contract the
Rust decoder shares (a depth below 8 scaled by `v * 255 / (2**d - 1)`, a
16-bit sample by `(v * 255 + 32767) // 65535`, grey replicated, a `tRNS`
key masked to the image's depth and compared at it, a palette entry's
alpha its `tRNS` byte or 255), so the hashes are exact. It prints the
FNV-1a-64 hash of the RGBA bytes:

```text
python3 png_ref.py shapes.png
shapes 96x64 rgba 0xaf1c86b7bf39b47b
python3 png_ref.py suite/*.png
g1-3x1-i 3x1 rgba 0x0f63d359e34d1ec0
g16-i 13x11 rgba 0xbaa7fd6ca336771a
g16 13x11 rgba 0xf4db75743e072490
g16-t 13x11 rgba 0xde55094d917863b0
g1-i 13x11 rgba 0xacfb746e1e3f2f79
g1 13x11 rgba 0x667e6ed4564e11d8
g2-i 13x11 rgba 0xa6fb485718f799bb
g2 13x11 rgba 0xd0a0ad74cc17ccbb
g2-t 13x11 rgba 0xe9483b4cb09ef57c
g4-i 13x11 rgba 0x6a476c8adcdac168
g4 13x11 rgba 0x8173bdf7a63063a0
g8-i 13x11 rgba 0x630a193d9f4376e6
g8 13x11 rgba 0xb0ca025a21f51a1b
g8-tm 13x11 rgba 0x4447438a8346ec19
g8-t 13x11 rgba 0x6e20a2c1a266ca3c
ga16-i 13x11 rgba 0x2d913132dae1811e
ga16 13x11 rgba 0xc5cc40834584b65b
ga8-i 13x11 rgba 0xc290906cebfb1206
ga8 13x11 rgba 0x14dcd8533ed4db4f
p1-i 13x11 rgba 0x45fa5bae96bff8bc
p1 13x11 rgba 0x413909af0c7231c4
p2-i 13x11 rgba 0xce85fac0b8ee5796
p2 13x11 rgba 0x4f21043b95cc4193
p4-i 13x11 rgba 0x571680601b041342
p4 13x11 rgba 0x7669b6680d9d68a1
p4-t 13x11 rgba 0x121112ee79dbfb5f
p8-i 13x11 rgba 0xd0c91cc7016419c0
p8 13x11 rgba 0x5bfdffb77d22e1d7
rgb16-i 13x11 rgba 0x0931181b5b758a40
rgb16 13x11 rgba 0x67c9e2a5e22d75fa
rgb16-t 13x11 rgba 0x12d284de7419c52f
rgb8-i 13x11 rgba 0x74a97c14abedfd28
rgb8 13x11 rgba 0x8165ec73a575cc12
rgb8-t 13x11 rgba 0xb202f15e1c2765c5
rgba16-i 13x11 rgba 0xfe1925732d7e4c33
rgba16 13x11 rgba 0x7d724b3cc444eeb2
rgba8-1x1-i 1x1 rgba 0x6e80c2eb779cab86
rgba8-i 13x11 rgba 0xd680ae4db097ed46
rgba8 13x11 rgba 0x2f38a191ca7f7a0f
```

`tests/decode.rs` holds the Rust decoder to every one. The script is a
test oracle and fixture generator and no part of any build.
