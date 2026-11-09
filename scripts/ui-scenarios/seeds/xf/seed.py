#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
"""xf explorer seeder: reset the xf-* GreenMail accounts to a known state.
Usage: seed.py [tag ...]   (GreenMail 127.0.0.1 only; touches ONLY xf-<tag>@local.test, default a b)
Each account ends with exactly n INBOX messages "XF <a|b> msg 1..n" (+ one long-subject mail for a)."""
import imaplib, smtplib, sys, time
from email.message import EmailMessage
from email.utils import formatdate
N = 5
ACCTS = [f"xf-{t}@local.test" for t in (sys.argv[1:] or ["a", "b"])]
def send(to, subj, body, age):
    m = EmailMessage(); m["From"] = "XF Sender <xf-sender@local.test>"; m["To"] = to
    m["Subject"] = subj; m["Date"] = formatdate(time.time() - age, localtime=True)
    m["Message-ID"] = f"<xf-{time.time_ns()}@local.test>"; m.set_content(body)
    with smtplib.SMTP("127.0.0.1", 3025) as s: s.send_message(m)
for a in ACCTS:
    send(a, "XF bootstrap", "creates the account", 0)
time.sleep(1)
for a in ACCTS:
    im = imaplib.IMAP4("127.0.0.1", 3143); im.login(a, a)
    typ, boxes = im.list()
    for b in boxes:
        name = b.decode().split(' "." ')[-1].strip('"') if ' "." ' in b.decode() else b.decode().split()[-1].strip('"')
        if im.select(f'"{name}"')[0] != "OK": continue
        im.store("1:*", "+FLAGS", "\\Deleted"); im.expunge(); im.close()
    im.logout()
    tag = a[3]
    for i in range(1, N + 1):
        send(a, f"XF {tag} msg {i}", f"Body of message {i} for {a}.\n", (N - i + 1) * 3600)
    if tag in "ah":
        send(a, f"XF {tag} long " + "Lorem ipsum dolor sit amet consectetur " * 8, "long subject\n", (N + 2) * 3600)
time.sleep(1)
for a in ACCTS:
    im = imaplib.IMAP4("127.0.0.1", 3143); im.login(a, a)
    for b in im.list()[1]:
        name = b.decode().split(' "." ')[-1].strip('"')
        r = im.select(f'"{name}"', readonly=True); print(a, name, r[1][0].decode())
    im.logout()
