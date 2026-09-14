# td-photo test fixtures

## `z8-rows.bin`

The first 18192 bytes of the raw strip of a Nikon Z 8 NEF (`DSC_4628.NEF`,
firmware `Ver.03.10`, taken 2026-09-12, 8280x5520 samples, 14-bit,
`Compression` 34713, one strip of 54954015 bytes at file offset 5683200).
The first two sensor rows consume 145297 bits, 18163 bytes, of that
stream; the slice carries 29 more so the decoder's lookahead reads real
bytes rather than padding.

The header values that decode it come from the file's Nikon maker note
linearization table (tag 0x0096, 46 bytes): version bytes `0x46 0x30`,
which with 14-bit samples select dcraw's 14-bit lossless tree (index 5),
four vertical predictors of 2048, a curve length of 34 that a `0x46` table
does not use (identity curve, range 16384) and no split.

`nikon_ref.py` is an independent transcription of dcraw 9.28's
`nikon_load_raw` in Python, written from the C source and not from
td-photo's Rust; run over the whole file it decodes every row with zero
range errors and consumes exactly the strip's 54954015 bytes. It prints
the FNV-1a-64 hash of the decoded samples as little-endian `u16` bytes:

```text
python3 nikon_ref.py DSC_4628.NEF 2      # rows 0..2: 0x44df96cc9a8684a0
python3 nikon_ref.py DSC_4628.NEF 5520   # whole frame: 0xe09ae870943b71be
```

`tests/nef.rs` holds the Rust decoder to the two-row hash over this slice;
the whole-frame hash is recorded for anyone with the file. The script is a
test oracle generator and no part of any build. It transcribes the
no-curve (`0x46`) path only: the sampled-curve and split forms are held to
dcraw's arithmetic by the synthetic tests, not by this oracle.

## `z8-thumb.jpg`

The smallest embedded preview of the same NEF (IFD0's `JPEGIFOffset`,
13063 bytes at file offset 258768): a 160x120 baseline JPEG, three
components with 4:2:2 sampling (luma 2x1), one quantisation and one
Huffman segment, no restart interval, the layout the camera's 1620x1080
and 8256x5504 previews share.

`jpeg_ref.py` is an independent baseline JPEG decoder written from ITU-T
T.81 and not from td-photo's Rust. Its arithmetic is the contract the Rust
decoder shares (a separable float64 inverse DCT over the same constants,
rows then columns; samples `floor(v + 128 + 0.5)` clamped; replicated
chroma; the JFIF colour constants with `floor(x + 0.5)`), so the pixel
hashes are exact and not a tolerance, and it refuses the same malformed
streams the Rust decoder names (an over-full table or a code of all ones,
a predictor, run or ZRL past its bounds, a reserved AC symbol, a scan out
of the frame's order, a restart marker out of sequence or preceded by an
unread byte, anything but EOI after the scan, a second frame, and the
arithmetic and hierarchical markers), so a stream one accepts the other
accepts. It prints the FNV-1a-64 hash of every dequantised coefficient
(little-endian i32, decode order) and of the RGB bytes:

```text
python3 jpeg_ref.py z8-thumb.jpg 8   # 600 blocks, coefficients 0x4012c2335efb6bfa
                                     # 160x120 rgb 0x15cf1df123ee575d
python3 jpeg_ref.py z8-thumb.jpg 4   # 80x60   rgb 0x164a0dbefb89395d
python3 jpeg_ref.py z8-thumb.jpg 2   # 40x30   rgb 0x7c9a6f6b601504ab
python3 jpeg_ref.py z8-thumb.jpg 1   # 20x15   rgb 0x8e6c3e050993ae30
```

`tests/jpeg.rs` holds the Rust decoder to all five. Over the 1620x1080
preview of the same file (1025548 bytes at offset 372224, not committed)
the script prints 55080 blocks, coefficients 0x3f936101a759076a and rgb
0x09eb4176d426cad7, and `td-photo probe DSC_4628.NEF --decode` prints the
same pixel hash as `preview-decode: 2 1620x1080 fnv1a64
0x09eb4176d426cad7`. To check a new body, cut a preview at the offsets
`probe` prints and compare the two the same way.
