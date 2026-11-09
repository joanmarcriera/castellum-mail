#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
"""conflicts.py — parse git conflict blocks line by line.

A regex with `.*?` and re.S silently swallows the markers of an ADJACENT
conflict and fuses two blocks into one, which is how a resolution ends up
splicing one stage's code into the middle of another's unfinished function.
This walks lines instead, so blocks can never merge.

parse(text) -> list of (start_index, end_index, ours_lines, theirs_lines)
resolve(text, fn) -> text, with fn(i, ours, theirs) returning the replacement
string for block i (0-based).
"""
def parse(text):
    lines = text.splitlines(keepends=True)
    blocks, i = [], 0
    while i < len(lines):
        if lines[i].startswith('<<<<<<< '):
            start, ours, theirs, side = i, [], [], 'ours'
            i += 1
            while i < len(lines) and not lines[i].startswith('>>>>>>> '):
                if lines[i].startswith('======='):
                    side = 'theirs'
                elif side == 'ours':
                    ours.append(lines[i])
                else:
                    theirs.append(lines[i])
                i += 1
            assert i < len(lines), f'unterminated conflict at line {start + 1}'
            blocks.append((start, i, ours, theirs))
        i += 1
    return lines, blocks

def resolve(text, fn):
    lines, blocks = parse(text)
    out, prev = [], 0
    for n, (start, end, ours, theirs) in enumerate(blocks):
        out.append(''.join(lines[prev:start]))
        out.append(fn(n, ''.join(ours), ''.join(theirs)))
        prev = end + 1
    out.append(''.join(lines[prev:]))
    return ''.join(out), len(blocks)
