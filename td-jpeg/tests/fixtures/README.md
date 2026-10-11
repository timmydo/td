# td-jpeg test fixtures

## `z8-thumb.jpg`

The smallest embedded preview of a Nikon Z 8 NEF (`DSC_4628.NEF`,
firmware `Ver.03.10`, taken 2026-09-12; IFD0's `JPEGIFOffset`, 13063
bytes at file offset 258768), the frame td-photo's `z8-rows.bin` is cut
from: a 160x120 baseline JPEG, three components with 4:2:2 sampling
(luma 2x1), one quantisation and one Huffman segment, no restart
interval, the layout the camera's 1620x1080 and 8256x5504 previews
share.

`jpeg_ref.py` is an independent baseline JPEG decoder written from ITU-T
T.81 and not from td-jpeg's Rust. Its arithmetic is the contract the Rust
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

`tests/decode.rs` holds the Rust decoder to all five. Over the 1620x1080
preview of the same file (1025548 bytes at offset 372224, not committed)
the script prints 55080 blocks, coefficients 0x3f936101a759076a and rgb
0x09eb4176d426cad7, and `td-photo probe DSC_4628.NEF --decode` prints the
same pixel hash as `preview-decode: 2 1620x1080 fnv1a64
0x09eb4176d426cad7`. To check a new body, cut a preview at the offsets
`probe` prints and compare the two the same way.
