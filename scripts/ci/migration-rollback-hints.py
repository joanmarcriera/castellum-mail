#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
"""migration-rollback-hints.py <id> [id...] — print the DDL that undoes those
migrations, for pasting into a `pre_00NN_profile_upgrades_cleanly` test.

Why this exists: when a branch's migration is renumbered at merge time, or
when a migration lands on main while a branch is in flight, every pre_ upgrade
test must (a) DROP the objects the newer migrations created and (b) delete
those ids from `_migrations`. Part (b) check-migrations.py enforces. Part (a)
it cannot — dropping the wrong set leaves the reopened profile in a state the
migration cannot re-apply to, and the failure reads as an unrelated SQL error.

This reads the actual .sql files and emits the reverse in reverse order, so
the hint is derived from what the migration does rather than remembered.

    scripts/ci/migration-rollback-hints.py 24 25

Output is a hint, not a guarantee: a migration that rewrites rows or rebuilds a
table cannot be undone by DDL alone, and those are flagged with a WARNING
comment rather than silently mis-emitted. Read them before pasting.
"""
import re, sys, pathlib

MIG = pathlib.Path(__file__).resolve().parents[2] / 'native/crates/castellum-db/src/migrations'

def reverse(sql: str) -> list[str]:
    out, warn = [], []
    for m in re.finditer(r'CREATE\s+(?:VIRTUAL\s+)?TABLE\s+(?:IF NOT EXISTS\s+)?(\w+)', sql, re.I):
        out.append(f'DROP TABLE {m.group(1)};')
    for m in re.finditer(r'CREATE\s+TRIGGER\s+(?:IF NOT EXISTS\s+)?(\w+)', sql, re.I):
        out.append(f'DROP TRIGGER {m.group(1)};')
    for m in re.finditer(r'CREATE\s+(?:UNIQUE\s+)?INDEX\s+(?:IF NOT EXISTS\s+)?(\w+)', sql, re.I):
        out.append(f'DROP INDEX {m.group(1)};')
    for m in re.finditer(r'ALTER\s+TABLE\s+(\w+)\s+ADD\s+COLUMN\s+(\w+)', sql, re.I):
        out.append(f'ALTER TABLE {m.group(1)} DROP COLUMN {m.group(2)};')
    if re.search(r'\b(INSERT|UPDATE|DELETE)\b', sql, re.I):
        warn.append('-- WARNING: this migration also moves data; DDL alone does not undo it.')
    # Drop in the reverse of creation order so dependants go before dependencies.
    return warn + list(reversed(out))

def main(ids):
    for i in ids:
        hits = sorted(MIG.glob(f'{int(i):04d}_*.sql'))
        if not hits:
            print(f'-- {int(i):04d}: no migration file (inline SQL, or a gap)'); continue
        f = hits[0]
        print(f'-- {f.name}')
        lines = reverse(f.read_text())
        print('\n'.join(lines) if lines else '-- (nothing to drop)')
    print(f'\nDELETE FROM _migrations WHERE id IN ({", ".join(str(int(i)) for i in ids)});')

if __name__ == '__main__':
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    main(sys.argv[1:])
