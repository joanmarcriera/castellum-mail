#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
"""Usage: tree.py <tree.json> [substring] — prints role and label of every labelled node (optionally filtered)."""
import json, sys
t = json.load(open(sys.argv[1])); needle = sys.argv[2] if len(sys.argv) > 2 else ""
def walk(n):
    if isinstance(n, dict):
        lab = n.get("label")
        if isinstance(lab, str) and lab and "role" in n and needle in lab: print(f"{str(n.get('role','')):12} {lab[:150]!r}")
        for v in n.values(): walk(v)
    elif isinstance(n, list):
        for v in n: walk(v)
walk(t)
