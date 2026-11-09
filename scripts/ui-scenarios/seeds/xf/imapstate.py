#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
"""Print every mailbox of xf-<tag>@local.test with its subjects+flags (read-only). Usage: imapstate.py tag ..."""
import imaplib, sys, re
for t in sys.argv[1:]:
    a = f"xf-{t}@local.test"; im = imaplib.IMAP4("127.0.0.1", 3143); im.login(a, a)
    for b in im.list()[1]:
        name = b.decode().split(' "." ')[-1].strip('"')
        n = int(im.select(f'"{name}"', readonly=True)[1][0])
        out = []
        if n:
            for part in im.fetch("1:*", "(FLAGS BODY.PEEK[HEADER.FIELDS (SUBJECT)])")[1]:
                if isinstance(part, tuple):
                    fl = re.search(rb"FLAGS \(([^)]*)\)", part[0]); out.append(part[1].decode().strip().replace("Subject: ", "")[:22] + "[" + (fl.group(1).decode() if fl else "") + "]")
        print(a, name, n, out)
    im.logout()
