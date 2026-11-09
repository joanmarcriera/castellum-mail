#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
"""Usage: missing_ui_keys.py [dir] — string literals passed to SwiftUI localising initialisers
(Text, Button, Toggle, Picker, Section, Label, TextField, .help, String(localized:) ...) that have
no key in the COMPILED es.lproj table (build/explore/xg/built-es.json). Prints file:line and a per-file count."""
import json,re,sys,glob,collections
S=set(json.load(open('build/explore/xg/built-es.json')))
N=lambda k: re.sub(r'%(\d+\$)?(lld|ld|d|@|lf|f)','%@',k)
keys={N(k) for k in S}
norm=lambda s: re.sub(r'\\\((?:[^()]|\([^()]*\))*\)','%@',s)
CALL=r'(?:\b(?:Text|Button|Toggle|Picker|Section|SettingsSection|SettingsRow|Label|LabeledContent|TextField|SecureField|Menu|Tab|DisclosureGroup|GroupBox|Link|Stepper)\(|\.(?:help|navigationTitle|alert|confirmationDialog|accessibilityLabel|accessibilityHint)\(|String\(localized:\s*|LocalizedStringKey\()\s*"((?:[^"\\]|\\.)*)"'
cnt=collections.Counter(); out=[]
for f in sorted(glob.glob((sys.argv[1] if len(sys.argv)>1 else 'apple/Sources/Castellum/App')+'/**/*.swift',recursive=True)):
    if 'UIHarness' in f: continue
    for n,line in enumerate(open(f),1):
        if line.strip().startswith('//'): continue
        for m in re.finditer(CALL,line):
            s=m.group(1)
            if not re.search(r'[A-Za-z]{2}',s) or s.startswith('\\(') and s.endswith(')') and s.count('\\(')==1: continue
            if norm(s) in keys or s in S: continue
            cnt[f.split('/')[-1]]+=1; out.append(f'{f.split("/")[-1]}:{n}: "{s[:90]}"')
print('\n'.join(out)); print('---'); print(sum(cnt.values()),'literals missing;',cnt.most_common())
