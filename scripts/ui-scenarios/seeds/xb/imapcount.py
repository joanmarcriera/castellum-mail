#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
"""Read-only GreenMail check. Usage: imapcount.py <persona> <subject-substring> [--dump]
Lists every mailbox of <persona>@local.test (EXAMINE, never SELECT) and prints the count of
messages whose Subject contains the substring; --dump prints To/Cc/From/Subject headers."""
import imaplib, sys, re
p, needle = sys.argv[1], sys.argv[2]
dump = "--dump" in sys.argv
M = imaplib.IMAP4("127.0.0.1", 3143)
M.login(f"{p}@local.test", f"{p}-pass")
typ, boxes = M.list()
for b in boxes:
    name = re.findall(r'"([^"]*)"$|(\S+)$', b.decode())[-1]
    name = name[0] or name[1]
    typ, _ = M.select(f'"{name}"', readonly=True)
    if typ != "OK": continue
    typ, d = M.search(None, "SUBJECT", f'"{needle}"')
    ids = d[0].split()
    print(f"{name}: {len(ids)}")
    if dump:
        for i in ids:
            typ, h = M.fetch(i, "(BODY.PEEK[HEADER.FIELDS (FROM TO CC BCC SUBJECT REPLY-TO)])")
            print("   ", h[0][1].decode().strip().replace("\r\n", " | "))
M.logout()
