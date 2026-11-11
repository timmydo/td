#!/usr/bin/env python3
"""Regenerate using ImageMagick 6.9.13-5 convert executable as argv[1]."""
import pathlib
import subprocess
import sys
root = pathlib.Path(__file__).parent
for name, width, height, interlace, alpha in [
    ('palette', 128, 128, False, False),
    ('interlaced', 128, 128, True, False),
    ('transparent', 13, 11, True, True),
]:
    raw = bytearray()
    state = 0x12345678
    for y in range(height):
        for x in range(width):
            state = (state * 1664525 + 1013904223) & 0xffffffff
            value = (state >> 24) if not alpha else ((x + y) % 4) * 85
            raw += bytes([value, value, value, 0 if alpha and (x + 2*y) % 3 == 0 else 255])
    args = [sys.argv[1], '-size', f'{width}x{height}', '-depth', '8', 'rgba:-',
            '-dither', 'None', '-interlace', 'GIF' if interlace else 'None', 'gif:-']
    out = subprocess.check_output(args, input=raw)
    (root / (name + '.gif')).write_bytes(out)
