#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
"""xh explorer: seed xh-off@local.test with three small messages (XH off A/B/C) and print the server state
(flagged copies of A, copies of B in INBOX / Trash). Usage: seed-off.py [state]   (state: only print)"""
import smtplib, sys, imaplib
from email.message import EmailMessage
addr = "xh-off@local.test"
if len(sys.argv) < 2:
    s = smtplib.SMTP("127.0.0.1", 3025)
    for k in "ABC":
        m = EmailMessage(); m["From"] = "xh-sender@local.test"; m["To"] = addr; m["Subject"] = f"XH off {k}"
        m.set_content(f"Offline write test message {k}."); s.send_message(m)
    s.quit()
i = imaplib.IMAP4("127.0.0.1", 3143); i.login(addr, addr)
print("folders", [x.decode() for x in i.list()[1]])
i.select("INBOX")
n = lambda *q: len(i.search(None, *q)[1][0].split())
print("INBOX: A", n('SUBJECT', '"XH off A"'), "A flagged", n('FLAGGED', 'SUBJECT', '"XH off A"'), "B", n('SUBJECT', '"XH off B"'), "C", n('SUBJECT', '"XH off C"'), "C seen", n('SEEN', 'SUBJECT', '"XH off C"'))
for mb in ("Trash", "Archive"):
    if i.select(mb)[0] == "OK": print(mb + ": B", n('SUBJECT', '"XH off B"'), "all", n('ALL'))
i.logout()
