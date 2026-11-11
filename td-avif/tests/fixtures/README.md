# AVIF container fixture

`libavif.avif` is a synthetic 6×4, eight-bit, full-range 4:2:0 image with
constant planes Y=160, U=100 and V=180. Generated with host libavif 1.0.4,
libaom 3.8.0, speed 10, colour quality 75 and CICP 1/13/6. It contains
32 bytes of coded AV1 primary-item data and no alpha or metadata.

The encoder was used only to produce this test data. It is not a build or
runtime dependency.

`libaom-reduced.avif` encodes the same constant planes with the same tools
and parameters, adding `reduced-tx-type-set=1`. Its pixels are admitted by
the native AV1 decoder; dav1d 1.5.1 independently confirms its YUV planes.
