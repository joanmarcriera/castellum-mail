#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
"""cm2/cm3 seeder (context menu: Move to, Junk). Usage: seed.py   (GreenMail 127.0.0.1 only). Re-run it before EACH of cm2-move-to and cm3-junk
(both start from "5 messages in the INBOX").
Resets cm-a@local.test to: 5 INBOX messages "CM a msg 1..5", empty folders "Projects" and "Junk"
(Castellum derives the junk role from the folder name). Touches ONLY cm-a@local.test."""
import imaplib, smtplib, time
from email.message import EmailMessage
from email.utils import formatdate
ACCT = "cm-a@local.test"
def send(subj, age):
    m = EmailMessage(); m["From"] = "CM Sender <cm-sender@local.test>"; m["To"] = ACCT
    m["Subject"] = subj; m["Date"] = formatdate(time.time() - age, localtime=True)
    m["Message-ID"] = f"<cm-{time.time_ns()}@local.test>"; m.set_content(f"Body of {subj}\n")
    with smtplib.SMTP("127.0.0.1", 3025) as s: s.send_message(m)
send("CM bootstrap", 0); time.sleep(1)
im = imaplib.IMAP4("127.0.0.1", 3143); im.login(ACCT, ACCT)
for b in im.list()[1]:
    name = b.decode().split(' "." ')[-1].strip('"')
    if im.select(f'"{name}"')[0] == "OK":
        im.store("1:*", "+FLAGS", "\\Deleted"); im.expunge(); im.close()
for box in ("Projects", "Junk"):
    im.create(box); im.subscribe(box)
im.logout()
for i in range(1, 6): send(f"CM a msg {i}", (6 - i) * 3600)
time.sleep(1)
print("seeded", ACCT)
