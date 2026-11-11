# Progressive coefficient oracles

Generated from a deterministic pixel formula in `generate.py`, using
libjpeg-turbo 2.1.4's `cjpeg -quality 87 -sample ...` and coefficient-preserving
`jpegtran -progressive`, with and without `-restart 2B`. No downloaded pictures
or new runtime dependencies. Regenerate with the tools' bin directory as the
script's sole argument. Tests use only the committed bytes.

Each `*-base.jpg` is the independently encoded baseline reference for its two
progressive variants. Comparing all four scales exactly tests reconstruction
without accepting IDCT or chroma-upsample rounding differences. The aligned
4:4:4 pair also compares every dequantised coefficient and callback order.
The nonaligned sizes exercise omitted padding blocks in single-component
scans. The default libjpeg scan script includes both DC and AC refinement.
