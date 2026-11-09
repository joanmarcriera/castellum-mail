#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Joan Marc Riera Duocastella
"""Seed xf-f@local.test for the plain-text folding scenarios. Usage: python3 scripts/ui-scenarios/seeds/xf/seed_fold.py
Two plain-text mails: "XF fold" (a 12-line quote and a 6-line signature) and "XF fold other" (nothing to fold).
Touches only xf-f@local.test on the local GreenMail (127.0.0.1)."""
import imaplib, smtplib, time
from email.message import EmailMessage
from email.utils import formatdate
A = "xf-f@local.test"
QUOTE = "\n".join(f"> quoted line {i}" for i in range(1, 13))
FOLD = ("Thanks for the update, I will be there.\n\nOn Mon, 5 Oct 2026, Ada Lovelace wrote:\n" + QUOTE +
        "\n\nSee you soon.\n-- \nMarc Riera\nCambridge\nUK\nhttps://example.org\nhello@example.org\n")
def send(subj, body, age):
    m = EmailMessage(); m["From"] = "XF Sender <xf-sender@local.test>"; m["To"] = A
    m["Subject"] = subj; m["Date"] = formatdate(time.time() - age * 3600, localtime=True)
    m["Message-ID"] = f"<xf-fold-{time.time_ns()}@local.test>"; m.set_content(body)
    with smtplib.SMTP("127.0.0.1", 3025) as s: s.send_message(m)
send("XF fold bootstrap", "creates the account", 0); time.sleep(1)
im = imaplib.IMAP4("127.0.0.1", 3143); im.login(A, A)
for b in im.list()[1]:
    name = b.decode().split(' "." ')[-1].strip('"')
    if im.select(f'"{name}"')[0] != "OK": continue
    im.store("1:*", "+FLAGS", "\\Deleted"); im.expunge(); im.close()
im.logout()
send("XF fold other", "Nothing to fold here.\n", 2)
send("XF fold", FOLD, 1)
time.sleep(1)
im = imaplib.IMAP4("127.0.0.1", 3143); im.login(A, A); print(A, "INBOX", im.select("INBOX", readonly=True)[1][0].decode()); im.logout()
