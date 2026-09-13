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
