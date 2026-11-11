# GIF oracles

`generate.py /path/to/convert` regenerates the fixtures with ImageMagick
6.9.13-5. The source is a deterministic RGBA pixel formula, not a downloaded
image. Tests use committed bytes and compare every decoded pixel to the
formula, without calling ImageMagick or Python.

`palette.gif` and `interlaced.gif` hold 128x128 pseudorandom grayscale pixels
(all 256 palette entries); `transparent.gif` is a 13x11 interlaced four-level
image with an independent transparency pattern. Only opaque pixel values
are significant beneath transparent input; the decoder returns zero RGBA
there. The files include trailers; the decoder deliberately returns before
them. Synthetic tests cover frame offsets, local palettes and animation tails.
