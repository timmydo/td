#!/usr/bin/env python3
"""Regenerate with libjpeg-turbo 2.1.4's bin directory as argv[1].
No runtime/test dependency: committed pairs contain identical coefficients.
"""
import pathlib
import subprocess
import sys

root = pathlib.Path(__file__).parent
tools = pathlib.Path(sys.argv[1])
for name, width, height, sampling, grey in [
    ('grey', 19, 17, '1x1', True),
    ('444', 32, 32, '1x1,1x1,1x1', False),
    ('420', 35, 27, '2x2,1x1,1x1', False),
    ('422', 35, 27, '2x1,1x1,1x1', False),
    ('440', 35, 27, '1x2,1x1,1x1', False),
    ('fine-chroma', 35, 27, '1x1,2x2,1x1', False),
]:
    channels = 1 if grey else 3
    raw = bytes((x * 19 + y * 37 + c * 59 + x * y * 3) % 256
                for y in range(height) for x in range(width) for c in range(channels))
    pnm = f'P{5 if grey else 6}\n{width} {height}\n255\n'.encode() + raw
    base = subprocess.check_output([str(tools / 'cjpeg'), '-quality', '87', '-sample', sampling], input=pnm)
    (root / (name + '-base.jpg')).write_bytes(base)
    for restart in [False, True]:
        args = [str(tools / 'jpegtran'), '-progressive']
        if restart:
            args += ['-restart', '2B']
        output = subprocess.check_output(args, input=base)
        (root / (name + ('-restart' if restart else '') + '-prog.jpg')).write_bytes(output)
