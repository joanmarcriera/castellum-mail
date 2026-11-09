#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
"""migcheck.py [repo-root] — assert the migration invariants that the
renumber trap breaks silently.

Checks, against native/crates/castellum-db/src/:
 1. every migrations/*.sql file has a MIGRATIONS entry, and vice versa
 2. the numeric prefix of each file == the `id:` of its entry
 3. MIGRATIONS ids are strictly increasing and contiguous from 1
 4. every `pre_NNNN_*` test deletes EVERY id >= NNNN from _migrations
    from _migrations. The runner re-applies by MAX(id) watermark, so an id
    left behind means the older migrations never re-run and the test passes
    while proving nothing.
Exit 1 and print each violation; silence + "migcheck OK" means clean.
"""
import re, sys, pathlib
root = pathlib.Path(sys.argv[1] if len(sys.argv) > 1 else '.')
src = root / 'native/crates/castellum-db/src'
lib = (src / 'lib.rs').read_text()
files = sorted(p.name for p in (src / 'migrations').glob('*.sql'))
file_ids = {int(n[:4]): n for n in files}
# Parse only the real `MIGRATIONS` array: test modules build their own
# `Migration { .. }` fixtures (e.g. an injected failing migration), and scanning
# all of lib.rs counted those as duplicate production ids.
m = re.search(r'const MIGRATIONS: &\[Migration\] = &\[(.*?)\n\];', lib, re.S)
if not m:
    print('migcheck: cannot find `const MIGRATIONS: &[Migration] = &[ ... ];` in lib.rs'); sys.exit(1)
table = m.group(1)
# id 1 carries its SQL inline (no file); such entries are legitimate.
entries = [(int(i), n, f) for i, n, f in
           re.findall(r'Migration\s*\{\s*id:\s*(\d+),\s*name:\s*"([^"]+)",\s*sql:\s*include_str!\("migrations/([^"]+)"\)', table)]
inline = [int(i) for i in re.findall(r'id:\s*(\d+),\s*name:\s*"[^"]+",\s*sql:\s*"', table)]
bad, warn = [], []
ent_ids = sorted([i for i, _, _ in entries] + inline)
for i, name, fn in entries:
    if fn not in files: bad.append(f'MIGRATIONS id {i} includes missing file {fn}')
    elif int(fn[:4]) != i: bad.append(f'file {fn} does not match its entry id {i}')
for i, n in file_ids.items():
    if i not in ent_ids: bad.append(f'migration file {n} has no MIGRATIONS entry')
dupes = sorted({i for i in ent_ids if ent_ids.count(i) > 1})
if dupes: bad.append(f'duplicate MIGRATIONS ids: {dupes}')
# Gaps are tolerated (8 has been absent since early development) but reported,
# so a renumber that leaves a NEW hole is visible rather than silent.
gaps = sorted(set(range(1, max(ent_ids) + 1)) - set(ent_ids)) if ent_ids else []
if gaps: warn.append(f'note: id gaps {gaps} (pre-existing on main: [8])')
maxid = max(ent_ids) if ent_ids else 0
# Any pre_NNNN_* test simulates an old profile, whatever it is called.
# Matching only *_profile_upgrades_cleanly* covered 2 of the 4 that
# exist — pre_0017_profile_backfills_project_links and
# pre_0019_profile_defaults_existing_accounts_to_read_only were never
# checked, so half the invariant went unenforced.
for m in re.finditer(r'fn (pre_(\d{4})_\w+)\(\)\s*\{(.*?)\n    \}', lib, re.S):
    fn_name, base, body = m.group(1), int(m.group(2)), m.group(3)
    dels = set()
    for d in re.finditer(r'DELETE FROM _migrations WHERE id IN \(([^)]*)\)', body):
        dels |= {int(x) for x in re.findall(r'\d+', d.group(1))}
    # only ids that actually exist in MIGRATIONS; a historical gap is not a row
    want = {i for i in ent_ids if i >= base}
    missing = sorted(want - dels)
    if missing:
        bad.append(f'{fn_name}: DELETE list is missing id(s) {missing} — the MAX(id) '
                   f'watermark will skip {base}..{maxid} and the test proves nothing')
if warn: print('\n'.join(warn))
print('\n'.join(bad) if bad else f'migcheck OK — {len(entries)} migrations, latest {maxid}')
sys.exit(1 if bad else 0)
