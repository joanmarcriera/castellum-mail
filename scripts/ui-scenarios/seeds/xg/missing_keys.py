#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
"""Usage: missing_keys.py file.swift...  — list UI-looking string literals absent from Localizable.xcstrings."""
import json,re,sys
S=set(json.load(open('apple/Sources/Castellum/Resources/Localizable.xcstrings'))['strings'])
def norm(s):
    return re.sub(r'\\\((?:[^()]|\([^()]*\))*\)','%@',s)
keys={re.sub(r'%(\d+\$)?(lld|ld|d|@|lf|f)','%@',k) for k in S}
for f in sys.argv[1:]:
    for n,line in enumerate(open(f),1):
        if line.strip().startswith('//'): continue
        for m in re.finditer(r'"((?:[^"\\]|\\.)*)"',line):
            s=m.group(1)
            if len(s)<4 or not re.search(r'[A-Za-z]{3}',s): continue
            if re.fullmatch(r'[a-z0-9_.:\-/]+',s) or re.fullmatch(r'[a-z]+[A-Za-z0-9.]*',s): continue
            k=norm(s)
            if k in keys or s in S: continue
            pre=line[:m.start()][-40:]
            print(f'{f.split("/")[-1]}:{n}: {pre.strip()[-28:]!r} -> "{s}"')
