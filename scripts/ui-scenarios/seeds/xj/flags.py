# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
"""xj explorer: print server FLAGS + subject of every INBOX message of an xj account (read-only). Usage: flags.py <localpart>"""
import imaplib, sys, re
acct = sys.argv[1] + "@local.test"; assert acct.startswith("xj-")
im = imaplib.IMAP4("127.0.0.1", 3143); im.login(acct, acct); im.select("INBOX", readonly=True)
typ, d = im.fetch("1:*", "(FLAGS BODY.PEEK[HEADER.FIELDS (SUBJECT)])")
for part in d:
    if isinstance(part, tuple):
        print(re.search(rb"FLAGS \((.*?)\)", part[0]).group(1).decode(), "|", part[1].decode().strip()[:60])
print(im.list()[1]); im.logout()
